//! An in-memory key-value driver for UI tests (141-redis.md §7.2): connects instantly,
//! serves deterministic data, no GPUI. The snapshot and headless tests register it as
//! `redis-fake` and drive the real navigator/editor/console paths against it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::FutureExt as _;
use futures::future::BoxFuture;

use naviduck_db::driver::{
    DbResult, Driver, DriverInfo, KeyValue, KeyValueData, ResultSet, Session,
};

/// One key's decoded value plus metadata.
#[derive(Clone, Debug)]
pub struct FakeKey {
    pub key_type: String,
    pub ttl: i64,
    pub data: KeyValueData,
}

impl FakeKey {
    pub fn string(value: &str) -> Self {
        Self {
            key_type: "string".into(),
            ttl: -1,
            data: KeyValueData::Str(value.into()),
        }
    }

    pub fn typed(key_type: &str, data: KeyValueData) -> Self {
        Self {
            key_type: key_type.into(),
            ttl: -1,
            data,
        }
    }
}

/// `db → key → value`, shared between the test and the driver's sessions.
pub type FakeKvData = Arc<Mutex<HashMap<u32, BTreeMap<String, FakeKey>>>>;

pub fn new_data() -> FakeKvData {
    Arc::new(Mutex::new(HashMap::new()))
}

pub struct FakeKvDriver {
    info: DriverInfo,
    data: FakeKvData,
}

impl FakeKvDriver {
    pub fn new(data: FakeKvData) -> Self {
        Self {
            info: DriverInfo {
                default_port: Some(6390),
                icon: "icons/db/redis.png",
                supports_network: true,
                database_number: true,
                ..DriverInfo::key_value("redis-fake", "Redis (test)")
            },
            data,
        }
    }
}

impl Driver for FakeKvDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn connect(
        &self,
        _params: naviduck_db::params::ConnectParams,
    ) -> BoxFuture<'static, DbResult<Box<dyn Session>>> {
        let data = self.data.clone();
        async move {
            Ok(Box::new(FakeKvSession {
                data,
                version: "Redis 7.9-fake".into(),
            }) as Box<dyn Session>)
        }
        .boxed()
    }
}

pub struct FakeKvSession {
    data: FakeKvData,
    version: String,
}

/// A glob match over one pattern: `*` spans any run of characters, `?` one.
fn glob(pattern: &str, key: &str) -> bool {
    fn go(p: &[u8], k: &[u8]) -> bool {
        match (p.first(), k.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], k) || (!k.is_empty() && go(p, &k[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &k[1..]),
            (Some(&a), Some(&b)) if a == b => go(&p[1..], &k[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), key.as_bytes())
}

impl FakeKvSession {
    fn with<F: FnOnce(&mut HashMap<u32, BTreeMap<String, FakeKey>>) -> R, R>(
        &self,
        f: F,
    ) -> DbResult<R> {
        let mut guard = self.data.lock().unwrap_or_else(|p| p.into_inner());
        Ok(f(&mut guard))
    }
}

impl Session for FakeKvSession {
    fn server_version(&self) -> &str {
        &self.version
    }

    fn ping(&mut self) -> BoxFuture<'_, DbResult<()>> {
        async { Ok(()) }.boxed()
    }

    fn cancel_handle(&self) -> Option<naviduck_db::driver::CancelHandle> {
        None
    }

    fn session_id(&self) -> Option<u64> {
        None
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, DbResult<()>> {
        async { Ok(()) }.boxed()
    }

    fn key_value(&mut self) -> Option<&mut dyn KeyValue> {
        Some(self)
    }
}

impl KeyValue for FakeKvSession {
    fn databases(&mut self) -> BoxFuture<'_, DbResult<Vec<u32>>> {
        async move { Ok((0..16).collect()) }.boxed()
    }

    fn key_count(&mut self, db: u32) -> BoxFuture<'_, DbResult<u64>> {
        async move { self.with(|data| Ok(data.get(&db).map(|m| m.len() as u64).unwrap_or(0)))? }
            .boxed()
    }

    fn scan<'a>(
        &'a mut self,
        db: u32,
        pattern: &'a str,
        cursor: u64,
        count: u32,
    ) -> BoxFuture<'a, DbResult<(u64, Vec<String>)>> {
        async move {
            self.with(|data| {
                let keys: Vec<String> = data
                    .get(&db)
                    .map(|m| m.keys().filter(|k| glob(pattern, k)).cloned().collect())
                    .unwrap_or_default();
                let start = cursor as usize;
                let end = (start + count as usize).min(keys.len());
                let next = if end == keys.len() { 0 } else { end as u64 };
                (next, keys[start..end].to_vec())
            })
        }
        .boxed()
    }

    fn key_type<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<String>> {
        async move {
            self.with(|data| {
                data.get(&db)
                    .and_then(|m| m.get(key))
                    .map(|k| k.key_type.clone())
                    .unwrap_or_else(|| "none".into())
            })
        }
        .boxed()
    }

    fn ttl<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<i64>> {
        async move {
            self.with(|data| {
                data.get(&db)
                    .and_then(|m| m.get(key))
                    .map(|k| k.ttl)
                    .unwrap_or(-2)
            })
        }
        .boxed()
    }

    fn read<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        _offset: u64,
        _count: u32,
    ) -> BoxFuture<'a, DbResult<KeyValueData>> {
        async move {
            self.with(|data| {
                data.get(&db)
                    .and_then(|m| m.get(key))
                    .map(|k| k.data.clone())
                    .unwrap_or(KeyValueData::Other("none".into()))
            })
        }
        .boxed()
    }

    fn set_string<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                data.entry(db)
                    .or_default()
                    .insert(key.into(), FakeKey::string(value));
            })
        }
        .boxed()
    }

    fn hash_set_field<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        field: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                let m = data.entry(db).or_default();
                let entry = m
                    .entry(key.into())
                    .or_insert_with(|| FakeKey::typed("hash", KeyValueData::Hash(Vec::new())));
                if let KeyValueData::Hash(pairs) = &mut entry.data {
                    if let Some(slot) = pairs.iter_mut().find(|(f, _)| f == field) {
                        slot.1 = value.into();
                    } else {
                        pairs.push((field.into(), value.into()));
                    }
                }
            })
        }
        .boxed()
    }

    fn hash_delete_field<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        field: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                if let Some(entry) = data.get_mut(&db).and_then(|m| m.get_mut(key))
                    && let KeyValueData::Hash(pairs) = &mut entry.data
                {
                    pairs.retain(|(f, _)| f != field);
                }
            })
        }
        .boxed()
    }

    fn list_push<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        tail: bool,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                let m = data.entry(db).or_default();
                let entry = m
                    .entry(key.into())
                    .or_insert_with(|| FakeKey::typed("list", KeyValueData::List(Vec::new())));
                if let KeyValueData::List(items) = &mut entry.data {
                    if tail {
                        items.push(value.into());
                    } else {
                        items.insert(0, value.into());
                    }
                }
            })
        }
        .boxed()
    }

    fn list_delete<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                if let Some(entry) = data.get_mut(&db).and_then(|m| m.get_mut(key))
                    && let KeyValueData::List(items) = &mut entry.data
                {
                    items.retain(|v| v != value);
                }
            })
        }
        .boxed()
    }

    fn set_add<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                let m = data.entry(db).or_default();
                let entry = m
                    .entry(key.into())
                    .or_insert_with(|| FakeKey::typed("set", KeyValueData::Set(Vec::new())));
                if let KeyValueData::Set(items) = &mut entry.data
                    && !items.contains(&member.to_string())
                {
                    items.push(member.into());
                }
            })
        }
        .boxed()
    }

    fn set_remove<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                if let Some(entry) = data.get_mut(&db).and_then(|m| m.get_mut(key))
                    && let KeyValueData::Set(items) = &mut entry.data
                {
                    items.retain(|v| v != member);
                }
            })
        }
        .boxed()
    }

    fn zset_add<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
        score: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                let m = data.entry(db).or_default();
                let entry = m
                    .entry(key.into())
                    .or_insert_with(|| FakeKey::typed("zset", KeyValueData::ZSet(Vec::new())));
                if let KeyValueData::ZSet(pairs) = &mut entry.data {
                    if let Some(slot) = pairs.iter_mut().find(|(m_, _)| m_ == member) {
                        slot.1 = score.into();
                    } else {
                        pairs.push((member.into(), score.into()));
                    }
                    pairs.sort_by(|a, b| {
                        a.1.parse::<f64>()
                            .unwrap_or(0.)
                            .total_cmp(&b.1.parse::<f64>().unwrap_or(0.))
                    });
                }
            })
        }
        .boxed()
    }

    fn zset_remove<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                if let Some(entry) = data.get_mut(&db).and_then(|m| m.get_mut(key))
                    && let KeyValueData::ZSet(pairs) = &mut entry.data
                {
                    pairs.retain(|(m_, _)| m_ != member);
                }
            })
        }
        .boxed()
    }

    fn create_key<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                data.entry(db)
                    .or_default()
                    .insert(key.into(), FakeKey::string(""));
            })
        }
        .boxed()
    }

    fn delete_keys<'a>(&'a mut self, db: u32, keys: &'a [String]) -> BoxFuture<'a, DbResult<u64>> {
        async move {
            self.with(|data| {
                let mut removed = 0;
                if let Some(m) = data.get_mut(&db) {
                    for key in keys {
                        removed += m.remove(key).is_some() as u64;
                    }
                }
                removed
            })
        }
        .boxed()
    }

    fn rename_key<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        new_name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            let mut guard = self.data.lock().unwrap_or_else(|p| p.into_inner());
            let m = guard.entry(db).or_default();
            if let Some(value) = m.remove(key) {
                m.insert(new_name.into(), value);
                Ok(())
            } else {
                Err(naviduck_db::DbError::coded("ERR", "no such key"))
            }
        }
        .boxed()
    }

    fn set_ttl<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        seconds: Option<i64>,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.with(|data| {
                if let Some(entry) = data.get_mut(&db).and_then(|m| m.get_mut(key)) {
                    entry.ttl = seconds.unwrap_or(-1);
                }
            })
        }
        .boxed()
    }

    fn execute_command<'a>(
        &'a mut self,
        db: u32,
        args: &'a [String],
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>> {
        async move {
            let (name, rest) = args
                .split_first()
                .ok_or_else(|| naviduck_db::DbError::coded("ERR", "wrong number of arguments"))?;
            match name.to_ascii_uppercase().as_str() {
                "GET" => {
                    let value = self.with(|data| {
                        data.get(&db)
                            .and_then(|m| m.get(&rest[0]))
                            .and_then(|k| match &k.data {
                                KeyValueData::Str(s) => Some(s.clone()),
                                _ => None,
                            })
                    })?;
                    Ok(vec![ResultSet {
                        columns: vec![naviduck_db::driver::Column {
                            name: "result".into(),
                            ..Default::default()
                        }],
                        rows: vec![vec![value]],
                        ..Default::default()
                    }])
                }
                "PING" => Ok(vec![ResultSet {
                    columns: vec![naviduck_db::driver::Column {
                        name: "result".into(),
                        ..Default::default()
                    }],
                    rows: vec![vec![Some("PONG".into())]],
                    ..Default::default()
                }]),
                "DBSIZE" => {
                    let n = self.key_count(db).await?;
                    Ok(vec![ResultSet {
                        affected_rows: Some(n),
                        ..Default::default()
                    }])
                }
                _ => Err(naviduck_db::DbError::coded(
                    "ERR",
                    format!("unknown command '{name}'"),
                )),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_and_question_mark() {
        assert!(glob("*", "anything"));
        assert!(glob("live:*", "live:scan:a"));
        assert!(!glob("live:*", "other"));
        assert!(glob("k?y", "key"));
        assert!(!glob("k?y", "ky"));
    }

    #[tokio::test]
    async fn scan_pages_through_sorted_keys() {
        let data = new_data();
        let mut m = BTreeMap::new();
        for k in ["a", "b", "c", "d"] {
            m.insert(k.to_string(), FakeKey::string("v"));
        }
        data.lock().unwrap().insert(0, m);
        let mut session = FakeKvSession {
            data,
            version: "v".into(),
        };
        let (c1, page1) = session.scan(0, "*", 0, 2).await.unwrap();
        assert_eq!(page1, vec!["a", "b"]);
        let (c2, page2) = session.scan(0, "*", c1, 2).await.unwrap();
        assert_eq!(page2, vec!["c", "d"]);
        assert_eq!(c2, 0);
    }
}
