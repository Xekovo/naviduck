//! Live integration tests against SQL Server 2022 (`docker-compose.yml` service `mssql`).
//!
//! Run with:
//! `docker compose up -d --wait mssql && NAVIDUCK_MSSQL_LIVE=1 cargo test -p naviduck-mssql --test live`

use std::sync::Arc;

use naviduck_db::commit::Commit;
use naviduck_db::{ConnectParams, connect};
use naviduck_model::object::{ObjectKind, Scope};
use naviduck_mssql::MssqlDriver;
use naviduck_sql::dialect::{ChangeSet, RowKey, RowUpdate, Written};
use naviduck_sql::span::Lex;

fn live() -> bool {
    std::env::var_os("NAVIDUCK_MSSQL_LIVE").is_some()
}

fn mssql_params() -> ConnectParams {
    ConnectParams {
        name: "live-mssql".into(),
        host: "127.0.0.1".into(),
        port: 11433,
        user: "sa".into(),
        password: Some("naviduck!123A".into()),
        database: Some("master".into()),
        ..ConnectParams::default()
    }
}

#[test]
fn mssql_end_to_end() {
    if !live() {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let driver = Arc::new(MssqlDriver::default());
        let mut sess = connect(driver, mssql_params())
            .await
            .expect("connect mssql");
        assert!(sess.server_version().contains("Microsoft SQL Server"));
        assert!(sess.session_id().is_some_and(|id| id > 0));
        sess.ping().await.expect("ping");

        let rel = sess.relational().expect("relational");
        let dbs = rel.list_databases().await.expect("list_databases");
        assert!(dbs.iter().any(|d| d.eq_ignore_ascii_case("master")));

        // Recreate a dedicated test database.
        let _ = rel
            .execute(
                "IF DB_ID('naviduck_test') IS NOT NULL \
                 BEGIN ALTER DATABASE [naviduck_test] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [naviduck_test]; END",
            )
            .await;
        rel.execute("CREATE DATABASE [naviduck_test]")
            .await
            .expect("create db");
        rel.execute("USE [naviduck_test]").await.expect("use db");

        let schemas = rel.list_schemas().await.expect("list_schemas");
        assert!(schemas.iter().any(|s| s == "dbo"));

        let batches = naviduck_db::sql::split_statements(
            "CREATE TABLE [dbo].[items] (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(100) NOT NULL)\nGO\n\
             CREATE TABLE [dbo].[orders] (id INT PRIMARY KEY, item_id INT NOT NULL REFERENCES [dbo].[items](id))\nGO\n\
             INSERT INTO [dbo].[items] (name) VALUES (N'alpha'), (N'beta')",
            Lex::MSSQL,
        );
        for batch in &batches {
            rel.execute(batch).await.expect(batch);
        }

        let scope = Scope::new("live", "naviduck_test", Some("dbo".into()));
        let tables = rel
            .list_objects(&scope, ObjectKind::Table)
            .await
            .expect("list tables");
        assert!(tables.iter().any(|t| t.name == "items"));
        assert!(tables.iter().any(|t| t.name == "orders"));

        let cols = rel
            .table_columns(&scope, "items")
            .await
            .expect("table_columns");
        assert_eq!(cols.len(), 2);
        assert!(cols[0].primary_key);

        let fks = rel
            .foreign_keys(&scope, "orders")
            .await
            .expect("foreign_keys");
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].table, "items");

        let ddl = rel
            .object_ddl(&scope, ObjectKind::Table, "items")
            .await
            .expect("object_ddl");
        assert!(ddl.contains("CREATE TABLE [dbo].[items]"));
        assert!(ddl.contains("IDENTITY(1,1)"));

        // Savepoint + DML affected-row commit path.
        let changes = ChangeSet {
            updates: vec![RowUpdate {
                key: RowKey(vec![("id".into(), Some("1".into()))]),
                values: vec![("name".into(), Written::text(Some("alpha-2".into())))],
            }],
            inserts: Vec::new(),
            deletes: Vec::new(),
        };
        let dialect = rel.dialect();
        let stmts = dialect.render_changes(&scope, "items", &changes, true);
        let commit = Commit {
            begin: dialect.begin_transaction(),
            statements: stmts,
            dialect,
        };
        naviduck_db::commit::apply(rel, commit, false, true)
            .await
            .expect("commit apply");

        let res = rel
            .execute(&rel.dialect().select_page(&scope, "items", "", "[id] ASC", Some(10), 0, false))
            .await
            .expect("select_page");
        assert_eq!(res[0].rows[0][1].as_deref(), Some("alpha-2"));

        // Showplan EXPLAIN path.
        rel.execute("SET SHOWPLAN_TEXT ON").await.expect("showplan on");
        let plan = rel
            .execute("SELECT * FROM [dbo].[items]")
            .await
            .expect("showplan query");
        rel.execute("SET SHOWPLAN_TEXT OFF")
            .await
            .expect("showplan off");
        assert!(!plan.is_empty());

        // Cleanup.
        rel.execute("USE [master]").await.expect("use master");
        let _ = rel
            .execute(
                "ALTER DATABASE [naviduck_test] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [naviduck_test]",
            )
            .await;
        sess.close().await.expect("close");
    });
}
