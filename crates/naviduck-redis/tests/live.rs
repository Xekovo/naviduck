//! Driver tests against the docker compose `redis` service (port 16379, password
//! `naviduck`, ACL user `alice`/`alice123`):
//!
//! ```sh
//! docker compose up -d --wait redis
//! NAVIDUCK_REDIS_LIVE=1 cargo test -p naviduck-redis --test live
//! ```
//!
//! Redis has no license restriction, so these run in CI like the other `NAVIDUCK_LIVE`
//! tests (141-redis.md §7.1).

use std::sync::Arc;

use naviduck_db::driver::KeyValueData;
use naviduck_db::params::ConnectParams;
use naviduck_db::{Driver, KeyValue as _, Session, connect, runtime, test_connection};
use naviduck_redis::RedisDriver;

fn live() -> bool {
    std::env::var_os("NAVIDUCK_REDIS_LIVE").is_some()
}

fn port() -> u16 {
    std::env::var("REDIS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(16379)
}

fn params() -> ConnectParams {
    ConnectParams {
        host: "127.0.0.1".into(),
        port: port(),
        password: Some("naviduck".into()),
        database: Some("0".into()),
        ..Default::default()
    }
}

#[test]
fn connects_reports_version_and_rejects_bad_credentials() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let driver: Arc<dyn Driver> = Arc::new(RedisDriver::default());
        let version = test_connection(driver.clone(), params()).await.unwrap();
        assert!(version.starts_with("Redis "), "{version}");

        let mut bad = params();
        bad.password = Some("wrong".into());
        let e = test_connection(driver.clone(), bad).await.unwrap_err();
        assert!(
            e.message.to_lowercase().contains("password") || e.code.is_some(),
            "{e}"
        );

        // The compose service defines an ACL user besides the default one.
        let acl = ConnectParams {
            user: "alice".into(),
            password: Some("alice123".into()),
            ..params()
        };
        test_connection(driver, acl).await.unwrap();
    });
}

#[test]
fn databases_and_key_counts_come_from_the_server() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        assert_eq!(kv.databases().await.unwrap(), (0..16).collect::<Vec<u32>>());
        kv.key_count(0).await.unwrap();
    });
}

#[test]
fn scan_pages_and_filters_by_pattern() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        kv.delete_keys(
            3,
            &[
                "live:scan:a".to_string(),
                "live:scan:b".to_string(),
                "live:other".to_string(),
            ],
        )
        .await
        .unwrap();
        kv.create_key(3, "live:scan:a").await.unwrap();
        kv.create_key(3, "live:scan:b").await.unwrap();
        kv.create_key(3, "live:other").await.unwrap();
        let (mut cursor, mut keys, mut rounds) = (0u64, Vec::new(), 0);
        loop {
            let (next, batch) = kv.scan(3, "live:scan:*", cursor, 1).await.unwrap();
            keys.extend(batch);
            cursor = next;
            rounds += 1;
            if cursor == 0 || rounds > 100 {
                break;
            }
        }
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(keys.contains(&"live:scan:a".to_string()));
    });
}

#[test]
fn five_key_types_round_trip() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        let keys: Vec<String> = [
            "live:str",
            "live:list",
            "live:hash",
            "live:set",
            "live:zset",
            "live:stream",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        kv.delete_keys(3, &keys).await.unwrap();

        kv.set_string(3, "live:str", "hello").await.unwrap();
        assert_eq!(kv.key_type(3, "live:str").await.unwrap(), "string");
        assert_eq!(
            kv.read(3, "live:str", 0, 0).await.unwrap(),
            KeyValueData::Str("hello".into())
        );

        kv.list_push(3, "live:list", true, "a").await.unwrap();
        kv.list_push(3, "live:list", true, "b").await.unwrap();
        kv.list_push(3, "live:list", false, "head").await.unwrap();
        assert_eq!(
            kv.read(3, "live:list", 0, 0).await.unwrap(),
            KeyValueData::List(vec!["head".into(), "a".into(), "b".into()])
        );
        kv.list_delete(3, "live:list", "a").await.unwrap();
        assert_eq!(
            kv.read(3, "live:list", 0, 0).await.unwrap(),
            KeyValueData::List(vec!["head".into(), "b".into()])
        );

        kv.hash_set_field(3, "live:hash", "f1", "v1").await.unwrap();
        kv.hash_set_field(3, "live:hash", "f2", "v2").await.unwrap();
        assert_eq!(
            kv.read(3, "live:hash", 0, 0).await.unwrap(),
            KeyValueData::Hash(vec![("f1".into(), "v1".into()), ("f2".into(), "v2".into())])
        );
        kv.hash_delete_field(3, "live:hash", "f1").await.unwrap();
        assert_eq!(
            kv.read(3, "live:hash", 0, 0).await.unwrap(),
            KeyValueData::Hash(vec![("f2".into(), "v2".into())])
        );

        kv.set_add(3, "live:set", "m1").await.unwrap();
        kv.set_add(3, "live:set", "m2").await.unwrap();
        let read = kv.read(3, "live:set", 0, 0).await.unwrap();
        let KeyValueData::Set(members) = read.clone() else {
            panic!("{read:?}");
        };
        assert_eq!(members.len(), 2);
        kv.set_remove(3, "live:set", "m1").await.unwrap();
        assert_eq!(
            kv.read(3, "live:set", 0, 0).await.unwrap(),
            KeyValueData::Set(vec!["m2".into()])
        );

        kv.zset_add(3, "live:zset", "one", "1.5").await.unwrap();
        kv.zset_add(3, "live:zset", "two", "2").await.unwrap();
        assert_eq!(
            kv.read(3, "live:zset", 0, 0).await.unwrap(),
            KeyValueData::ZSet(vec![
                ("one".into(), "1.5".into()),
                ("two".into(), "2".into())
            ])
        );
        kv.zset_remove(3, "live:zset", "two").await.unwrap();
        assert_eq!(
            kv.read(3, "live:zset", 0, 0).await.unwrap(),
            KeyValueData::ZSet(vec![("one".into(), "1.5".into())])
        );

        kv.execute_command(
            3,
            &[
                "XADD".to_string(),
                "live:stream".to_string(),
                "*".to_string(),
                "k".to_string(),
                "v".to_string(),
            ],
        )
        .await
        .unwrap();
        let read = kv.read(3, "live:stream", 0, 10).await.unwrap();
        let KeyValueData::Stream(entries) = read.clone() else {
            panic!("{read:?}");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, vec![("k".into(), "v".into())]);
    });
}

#[test]
fn ttl_reads_writes_and_persists() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        kv.delete_keys(3, &["live:ttl".to_string()]).await.unwrap();
        kv.set_string(3, "live:ttl", "x").await.unwrap();
        assert_eq!(kv.ttl(3, "live:ttl").await.unwrap(), -1);
        kv.set_ttl(3, "live:ttl", Some(120)).await.unwrap();
        let ttl = kv.ttl(3, "live:ttl").await.unwrap();
        assert!((100..=120).contains(&ttl), "ttl {ttl}");
        kv.set_ttl(3, "live:ttl", None).await.unwrap();
        assert_eq!(kv.ttl(3, "live:ttl").await.unwrap(), -1);
    });
}

#[test]
fn keys_rename_and_delete() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        kv.delete_keys(3, &["live:ren:a".to_string(), "live:ren:b".to_string()])
            .await
            .unwrap();
        kv.create_key(3, "live:ren:a").await.unwrap();
        kv.rename_key(3, "live:ren:a", "live:ren:b").await.unwrap();
        assert_eq!(kv.key_type(3, "live:ren:b").await.unwrap(), "string");
        assert_eq!(kv.key_type(3, "live:ren:a").await.unwrap(), "none");
        let deleted = kv
            .delete_keys(3, &["live:ren:b".to_string()])
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(kv.key_type(3, "live:ren:b").await.unwrap(), "none");
    });
}

#[test]
fn commands_execute_and_errors_carry_their_code() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        kv.set_string(3, "live:cmd", "v").await.unwrap();
        // A list-typed key triggers WRONGTYPE on GET, the editor's most common mistake.
        kv.list_push(3, "live:listcmd", true, "x").await.unwrap();
        let sets = kv
            .execute_command(3, &["GET".to_string(), "live:cmd".to_string()])
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0].as_deref(), Some("v"));
        let e = kv
            .execute_command(3, &["GET".to_string(), "live:listcmd".to_string()])
            .await
            .unwrap_err();
        assert_eq!(e.code.as_deref(), Some("WRONGTYPE"), "{e}");
        let e = kv
            .execute_command(3, &["NOSUCHCMD".to_string()])
            .await
            .unwrap_err();
        assert!(e.message.contains("unknown"), "{e}");
    });
}

#[test]
fn select_switches_databases() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let mut session = connect(Arc::new(RedisDriver::default()), params())
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        kv.delete_keys(15, &["live:db15".to_string()])
            .await
            .unwrap();
        kv.create_key(15, "live:db15").await.unwrap();
        assert_eq!(kv.key_type(15, "live:db15").await.unwrap(), "string");
        assert_eq!(kv.key_type(0, "live:db15").await.unwrap(), "none");
        kv.delete_keys(15, &["live:db15".to_string()])
            .await
            .unwrap();
    });
}

#[test]
fn the_initial_database_comes_from_the_connection() {
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let params = ConnectParams {
            database: Some("4".into()),
            ..params()
        };
        let mut session = connect(Arc::new(RedisDriver::default()), params)
            .await
            .unwrap();
        let kv = session.key_value().unwrap();
        kv.create_key(4, "live:init4").await.unwrap();
        assert_eq!(kv.key_type(4, "live:init4").await.unwrap(), "string");
        kv.delete_keys(4, &["live:init4".to_string()])
            .await
            .unwrap();
    });
}

#[test]
fn commands_run_on_the_db_runtime() {
    // The driver must run on naviduck_db::runtime, not on an ambient tokio context.
    if !live() {
        return;
    }
    runtime::runtime().block_on(async {
        let job = runtime::spawn(async move {
            let mut session = connect(Arc::new(RedisDriver::default()), params())
                .await
                .unwrap();
            let kv = session.key_value().unwrap();
            kv.key_count(0).await
        });
        job.await.unwrap().unwrap();
    });
}
