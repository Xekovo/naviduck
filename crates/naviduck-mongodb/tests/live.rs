//! Driver tests against the docker compose `mongo` service (port 17017, root user
//! `naviduck` / password `naviduck` in `admin`).
//!
//! ```sh
//! docker compose up -d --wait mongo
//! NAVIDUCK_MONGO_LIVE=1 cargo test -p naviduck-mongodb --test live
//! ```

use std::sync::Arc;

use naviduck_db::driver::FindQuery;
use naviduck_db::params::ConnectParams;
use naviduck_db::{Driver, connect, runtime, test_connection};
use naviduck_mongodb::MongoDriver;

fn live() -> bool {
    std::env::var_os("NAVIDUCK_MONGO_LIVE").is_some()
}

fn port() -> u16 {
    std::env::var("MONGO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(17017)
}

fn params() -> ConnectParams {
    ConnectParams {
        host: "127.0.0.1".into(),
        port: port(),
        user: "naviduck".into(),
        password: Some("naviduck".into()),
        database: Some("admin".into()),
        ..Default::default()
    }
}

#[test]
fn connects_reports_version_and_rejects_bad_credentials() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let driver: Arc<dyn Driver> = Arc::new(MongoDriver::default());
        let version = test_connection(driver.clone(), params()).await.unwrap();
        assert!(version.starts_with("MongoDB "), "{version}");

        let mut bad = params();
        bad.password = Some("wrong".into());
        let err = test_connection(driver, bad).await.unwrap_err();
        assert!(err.code.is_some() || !err.message.is_empty(), "{err}");
    });
}

#[test]
fn collection_lifecycle_crud_query_and_commands_round_trip() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(MongoDriver::default()), params())
            .await
            .unwrap();
        let doc = session.document().unwrap();
        let db = "naviduck_live_test";
        let _ = doc.drop_collection(db, "items").await;
        let _ = doc.drop_collection(db, "items_renamed").await;

        doc.create_collection(db, "items").await.unwrap();
        let dbs = doc.list_databases().await.unwrap();
        assert!(dbs.iter().any(|d| d == db), "{dbs:?}");
        let colls = doc.list_collections(db).await.unwrap();
        assert!(colls.iter().any(|c| c.name == "items"), "{colls:?}");

        let id1 = doc
            .insert_document(
                db,
                "items",
                r#"{"name": "alpha", "count": 10, "tags": ["a"]}"#,
            )
            .await
            .unwrap();
        let id2 = doc
            .insert_document(
                db,
                "items",
                r#"{"name": "beta", "count": 20, "tags": ["b"]}"#,
            )
            .await
            .unwrap();

        let page = doc
            .find(
                db,
                "items",
                &FindQuery {
                    filter_json: r#"{"count": {"$gte": 10}}"#.into(),
                    sort_json: r#"{"count": -1}"#.into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, Some(2));
        assert_eq!(page.documents.len(), 2);
        assert!(page.documents[0].contains("beta"));

        let matched = doc
            .replace_document(
                db,
                "items",
                &id1,
                r#"{"name": "alpha-updated", "count": 15}"#,
            )
            .await
            .unwrap();
        assert_eq!(matched, 1);

        let one = doc
            .find(
                db,
                "items",
                &FindQuery {
                    filter_json: r#"{"name": "alpha-updated"}"#.into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(one.documents.len(), 1);

        let indexes = doc.list_indexes(db, "items").await.unwrap();
        assert!(indexes.iter().any(|n| n == "_id_"), "{indexes:?}");

        let sets = doc.execute_command(db, r#"{"ping": 1}"#).await.unwrap();
        assert!(!sets.is_empty());
        let shell_sets = doc
            .execute_command(db, r#"db.items.find({"name": "beta"})"#)
            .await
            .unwrap();
        assert_eq!(shell_sets[0].rows.len(), 1);

        let deleted = doc
            .delete_documents(db, "items", &[id1, id2])
            .await
            .unwrap();
        assert_eq!(deleted, 2);

        doc.rename_collection(db, "items", "items_renamed")
            .await
            .unwrap();
        doc.drop_collection(db, "items_renamed").await.unwrap();
    });
}
