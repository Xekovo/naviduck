//! An in-memory document driver for UI tests (142-mongodb.md §7.1): connects instantly,
//! serves deterministic data, no GPUI. Snapshot and headless tests register it as
//! `mongo-fake` and drive the real navigator/viewer/console paths against it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use futures::FutureExt as _;
use futures::future::BoxFuture;
use naviduck_db::driver::{
    CollectionInfo, Column, DbError, DbResult, Document, DocumentPage, Driver, DriverInfo,
    FindQuery, ResultSet, Session,
};
use serde_json::{Map, Value};

/// `database -> collection -> documents` (each stored as a JSON Object Value).
pub type FakeDocData = Arc<Mutex<BTreeMap<String, BTreeMap<String, Vec<Value>>>>>;

pub fn new_data() -> FakeDocData {
    Arc::new(Mutex::new(BTreeMap::new()))
}

pub struct FakeDocDriver {
    info: DriverInfo,
    data: FakeDocData,
}

impl FakeDocDriver {
    pub fn new(data: FakeDocData) -> Self {
        Self {
            info: DriverInfo {
                default_port: Some(27018),
                icon: "icons/db/mongodb.svg",
                supports_network: true,
                initial_database: true,
                ..DriverInfo::document("mongo-fake", "MongoDB (test)")
            },
            data,
        }
    }
}

impl Driver for FakeDocDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn connect(
        &self,
        _params: naviduck_db::params::ConnectParams,
    ) -> BoxFuture<'static, DbResult<Box<dyn Session>>> {
        let data = self.data.clone();
        async move {
            Ok(Box::new(FakeDocSession {
                data,
                version: "MongoDB 8.0-fake".into(),
                next_id: 1,
            }) as Box<dyn Session>)
        }
        .boxed()
    }
}

pub struct FakeDocSession {
    data: FakeDocData,
    version: String,
    next_id: u64,
}

impl FakeDocSession {
    fn with<F: FnOnce(&mut BTreeMap<String, BTreeMap<String, Vec<Value>>>) -> R, R>(
        &self,
        f: F,
    ) -> DbResult<R> {
        let mut guard = self.data.lock().unwrap_or_else(|p| p.into_inner());
        Ok(f(&mut guard))
    }
}

fn matches_filter(doc: &Value, filter: &Map<String, Value>) -> bool {
    filter
        .iter()
        .all(|(k, expected)| doc.get(k) == Some(expected))
}

impl Session for FakeDocSession {
    fn server_version(&self) -> &str {
        &self.version
    }

    fn ping(&mut self) -> BoxFuture<'_, DbResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn cancel_handle(&self) -> Option<naviduck_db::CancelHandle> {
        None
    }

    fn session_id(&self) -> Option<u64> {
        None
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, DbResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn document(&mut self) -> Option<&mut dyn Document> {
        Some(self)
    }
}

impl Document for FakeDocSession {
    fn list_databases(&mut self) -> BoxFuture<'_, DbResult<Vec<String>>> {
        Box::pin(async {
            self.with(|d| {
                let mut names: Vec<String> = d.keys().cloned().collect();
                if !names.iter().any(|n| n == "admin") {
                    names.insert(0, "admin".into());
                }
                names
            })
        })
    }

    fn list_collections<'a>(
        &'a mut self,
        database: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<CollectionInfo>>> {
        Box::pin(async move {
            self.with(|d| {
                d.get(database)
                    .map(|colls| {
                        colls
                            .iter()
                            .map(|(name, docs)| CollectionInfo {
                                name: name.clone(),
                                kind: "collection".into(),
                                document_count: Some(docs.len() as u64),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
        })
    }

    fn collection_stats<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<(String, String)>>> {
        Box::pin(async move {
            self.with(|d| {
                let count = d
                    .get(database)
                    .and_then(|c| c.get(collection))
                    .map(|docs| docs.len())
                    .unwrap_or(0);
                vec![
                    ("rows".into(), count.to_string()),
                    ("indexes".into(), "1".into()),
                ]
            })
        })
    }

    fn find<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        query: &'a FindQuery,
    ) -> BoxFuture<'a, DbResult<DocumentPage>> {
        Box::pin(async move {
            let filter: Map<String, Value> = if query.filter_json.trim().is_empty() {
                Map::new()
            } else {
                serde_json::from_str::<Value>(&query.filter_json)
                    .map_err(|e| DbError::msg(format!("Invalid JSON: {e}")))?
                    .as_object()
                    .cloned()
                    .ok_or_else(|| DbError::msg("Filter must be a JSON object"))?
            };
            self.with(|d| {
                let all: Vec<Value> = d
                    .get(database)
                    .and_then(|c| c.get(collection))
                    .map(|docs| {
                        docs.iter()
                            .filter(|doc| matches_filter(doc, &filter))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let total = all.len() as u64;
                let limit = if query.limit == 0 { 50 } else { query.limit };
                let documents = all
                    .into_iter()
                    .skip(query.skip as usize)
                    .take(limit as usize)
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
                    .collect();
                DocumentPage {
                    documents,
                    total: Some(total),
                }
            })
        })
    }

    fn insert_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<String>> {
        Box::pin(async move {
            let mut val: Value = serde_json::from_str(doc_json)
                .map_err(|e| DbError::msg(format!("Invalid JSON: {e}")))?;
            let obj = val
                .as_object_mut()
                .ok_or_else(|| DbError::msg("Document must be a JSON object"))?;
            if !obj.contains_key("_id") {
                let id = format!("fake-{}", self.next_id);
                self.next_id += 1;
                obj.insert("_id".into(), Value::String(id));
            }
            let id_json = obj.get("_id").map(Value::to_string).unwrap_or_default();
            self.with(|d| {
                d.entry(database.to_string())
                    .or_default()
                    .entry(collection.to_string())
                    .or_default()
                    .push(val);
            })?;
            Ok(id_json)
        })
    }

    fn replace_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        id_json: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<u64>> {
        Box::pin(async move {
            let id: Value = serde_json::from_str(id_json)
                .map_err(|e| DbError::msg(format!("Invalid _id JSON: {e}")))?;
            let mut val: Value = serde_json::from_str(doc_json)
                .map_err(|e| DbError::msg(format!("Invalid JSON: {e}")))?;
            let obj = val
                .as_object_mut()
                .ok_or_else(|| DbError::msg("Document must be a JSON object"))?;
            obj.insert("_id".into(), id.clone());
            self.with(|d| {
                let Some(docs) = d.get_mut(database).and_then(|c| c.get_mut(collection)) else {
                    return 0;
                };
                if let Some(slot) = docs.iter_mut().find(|doc| doc.get("_id") == Some(&id)) {
                    *slot = val;
                    1
                } else {
                    0
                }
            })
        })
    }

    fn delete_documents<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        ids_json: &'a [String],
    ) -> BoxFuture<'a, DbResult<u64>> {
        Box::pin(async move {
            let ids: Vec<Value> = ids_json
                .iter()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .collect();
            self.with(|d| {
                let Some(docs) = d.get_mut(database).and_then(|c| c.get_mut(collection)) else {
                    return 0;
                };
                let before = docs.len();
                docs.retain(|doc| !doc.get("_id").is_some_and(|id| ids.contains(id)));
                (before - docs.len()) as u64
            })
        })
    }

    fn create_collection<'a>(
        &'a mut self,
        database: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        Box::pin(async move {
            self.with(|d| {
                d.entry(database.to_string())
                    .or_default()
                    .entry(name.to_string())
                    .or_default();
            })
        })
    }

    fn drop_collection<'a>(
        &'a mut self,
        database: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        Box::pin(async move {
            self.with(|d| {
                if let Some(colls) = d.get_mut(database) {
                    colls.remove(name);
                }
            })
        })
    }

    fn rename_collection<'a>(
        &'a mut self,
        database: &'a str,
        old_name: &'a str,
        new_name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        Box::pin(async move {
            self.with(|d| {
                if let Some(colls) = d.get_mut(database)
                    && let Some(docs) = colls.remove(old_name)
                {
                    colls.insert(new_name.to_string(), docs);
                }
            })
        })
    }

    fn list_indexes<'a>(
        &'a mut self,
        _database: &'a str,
        _collection: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<String>>> {
        Box::pin(async { Ok(vec!["_id_".into()]) })
    }

    fn execute_command<'a>(
        &'a mut self,
        database: &'a str,
        command: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>> {
        Box::pin(async move {
            let trimmed = command.trim().trim_end_matches(';').trim();
            let lower = trimmed.to_ascii_lowercase();
            if lower == "show dbs" || lower == "show databases" {
                let dbs = self.list_databases().await?;
                return Ok(vec![ResultSet {
                    columns: vec![Column {
                        name: "database".into(),
                        ..Default::default()
                    }],
                    rows: dbs.into_iter().map(|d| vec![Some(d)]).collect(),
                    ..Default::default()
                }]);
            }
            if lower == "show collections" || lower == "show tables" {
                let colls = self.list_collections(database).await?;
                return Ok(vec![ResultSet {
                    columns: vec![Column {
                        name: "collection".into(),
                        ..Default::default()
                    }],
                    rows: colls.into_iter().map(|c| vec![Some(c.name)]).collect(),
                    ..Default::default()
                }]);
            }
            if let Some(rest) = trimmed.strip_prefix("db.")
                && let Some((coll, tail)) = rest.split_once('.')
                && tail.starts_with("find(")
                && tail.ends_with(')')
            {
                let filter_json = tail
                    .trim_start_matches("find(")
                    .trim_end_matches(')')
                    .to_string();
                let page = self
                    .find(
                        database,
                        coll,
                        &FindQuery {
                            filter_json,
                            ..Default::default()
                        },
                    )
                    .await?;
                let parsed: Vec<Value> = page
                    .documents
                    .iter()
                    .filter_map(|s| serde_json::from_str(s).ok())
                    .collect();
                let mut cols = BTreeSet::new();
                for v in &parsed {
                    if let Some(obj) = v.as_object() {
                        cols.extend(obj.keys().cloned());
                    }
                }
                let cols: Vec<String> = cols.into_iter().collect();
                let rows = parsed
                    .iter()
                    .map(|v| {
                        cols.iter()
                            .map(|k| v.get(k).map(|x| x.to_string()))
                            .collect()
                    })
                    .collect();
                return Ok(vec![ResultSet {
                    columns: cols
                        .into_iter()
                        .map(|name| Column {
                            name,
                            ..Default::default()
                        })
                        .collect(),
                    rows,
                    ..Default::default()
                }]);
            }
            if trimmed.starts_with('{') {
                let _: Value = serde_json::from_str(trimmed)
                    .map_err(|e| DbError::msg(format!("Invalid JSON: {e}")))?;
                let mut row_map = HashMap::new();
                row_map.insert("ok", "1");
                return Ok(vec![ResultSet {
                    columns: vec![Column {
                        name: "ok".into(),
                        ..Default::default()
                    }],
                    rows: vec![vec![Some(row_map["ok"].into())]],
                    ..Default::default()
                }]);
            }
            Err(DbError::msg(format!("Unsupported command: {trimmed}")))
        })
    }
}
