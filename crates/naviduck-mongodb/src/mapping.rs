//! BSON / Extended JSON conversion, shell command parsing and ResultSet mapping (142-mongodb.md §4).

use std::collections::BTreeSet;

use mongodb::bson::{Bson, Document};
use naviduck_db::driver::{Column, ResultSet};
use naviduck_db::{DbError, DbResult};

/// Parses a JSON / Extended JSON object string into a BSON document. Empty input yields `{}`.
pub(crate) fn parse_doc_json(input: &str) -> DbResult<Document> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Document::new());
    }
    serde_json::from_str::<Document>(trimmed).map_err(|e| {
        DbError::coded(
            "naviduck.invalid_json",
            format!("Invalid JSON document: {e}"),
        )
    })
}

/// Parses an optional JSON object string (filter/sort/projection). Empty input yields `None`.
pub(crate) fn parse_opt_doc_json(input: &str) -> DbResult<Option<Document>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    parse_doc_json(trimmed).map(Some)
}

/// Parses a single Extended JSON `_id` value back into BSON.
pub(crate) fn parse_id_json(input: &str) -> DbResult<Bson> {
    serde_json::from_str::<Bson>(input.trim())
        .map_err(|e| DbError::coded("naviduck.invalid_id", format!("Invalid _id JSON: {e}")))
}

/// Serializes one BSON document as pretty Relaxed Extended JSON.
pub(crate) fn doc_to_pretty_json(doc: &Document) -> String {
    let val = Bson::Document(doc.clone()).into_relaxed_extjson();
    serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string())
}

/// Renders one BSON value as compact text for result tables.
pub(crate) fn bson_cell_text(val: &Bson) -> Option<String> {
    match val {
        Bson::Null | Bson::Undefined => None,
        Bson::String(s) => Some(s.clone()),
        Bson::Boolean(b) => Some(b.to_string()),
        Bson::Int32(n) => Some(n.to_string()),
        Bson::Int64(n) => Some(n.to_string()),
        Bson::Double(n) => Some(n.to_string()),
        Bson::ObjectId(oid) => Some(oid.to_hex()),
        other => Some(other.clone().into_relaxed_extjson().to_string()),
    }
}

/// Maps a list of BSON documents into a tabular `ResultSet` (`_id` first when present).
pub(crate) fn docs_to_result_sets(docs: &[Document]) -> Vec<ResultSet> {
    if docs.is_empty() {
        return vec![ResultSet::default()];
    }
    let mut keys = BTreeSet::new();
    let mut has_id = false;
    for doc in docs {
        for key in doc.keys() {
            if key == "_id" {
                has_id = true;
            } else {
                keys.insert(key.clone());
            }
        }
    }
    let mut ordered = Vec::new();
    if has_id {
        ordered.push("_id".to_string());
    }
    ordered.extend(keys);
    let columns = ordered
        .iter()
        .map(|name| Column {
            name: name.clone(),
            type_name: String::new(),
            origin: None,
        })
        .collect();
    let rows = docs
        .iter()
        .map(|doc| {
            ordered
                .iter()
                .map(|k| doc.get(k).and_then(bson_cell_text))
                .collect()
        })
        .collect();
    vec![ResultSet {
        columns,
        rows,
        affected_rows: None,
        notices: Vec::new(),
    }]
}

pub(crate) fn scalar_result_set(column: &str, values: Vec<String>) -> Vec<ResultSet> {
    vec![ResultSet {
        columns: vec![Column {
            name: column.into(),
            type_name: String::new(),
            origin: None,
        }],
        rows: values.into_iter().map(|v| vec![Some(v)]).collect(),
        affected_rows: None,
        notices: Vec::new(),
    }]
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MongoCommand {
    ShowDatabases,
    ShowCollections,
    RunCommand(Document),
    CollectionFind {
        collection: String,
        filter: Document,
        limit: Option<i64>,
    },
    CollectionCount {
        collection: String,
        filter: Document,
    },
    CollectionInsertOne {
        collection: String,
        doc: Document,
    },
    CollectionDelete {
        collection: String,
        filter: Document,
        many: bool,
    },
    CollectionStats {
        collection: String,
    },
}

/// Parses one MongoDB console line: JSON `runCommand`, `show dbs`, `show collections`,
/// `db.runCommand({...})`, or `db.<collection>.<method>(...)`.
pub(crate) fn parse_command(input: &str) -> DbResult<MongoCommand> {
    let trimmed = input.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(DbError::coded(
            "naviduck.empty_command",
            "The command is empty.",
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "show dbs" || lower == "show databases" {
        return Ok(MongoCommand::ShowDatabases);
    }
    if lower == "show collections" || lower == "show tables" {
        return Ok(MongoCommand::ShowCollections);
    }
    if trimmed.starts_with('{') {
        return Ok(MongoCommand::RunCommand(parse_doc_json(trimmed)?));
    }
    if let Some(rest) = trimmed.strip_prefix("db.") {
        if let Some(arg) = strip_call(rest, "runCommand") {
            return Ok(MongoCommand::RunCommand(parse_doc_json(arg)?));
        }
        if let Some((collection, method_call)) = split_collection_call(rest) {
            if let Some(arg) = strip_call(method_call, "find") {
                let filter = parse_doc_json(arg)?;
                return Ok(MongoCommand::CollectionFind {
                    collection: collection.to_string(),
                    filter,
                    limit: Some(50),
                });
            }
            if let Some(arg) = strip_call(method_call, "findOne") {
                let filter = parse_doc_json(arg)?;
                return Ok(MongoCommand::CollectionFind {
                    collection: collection.to_string(),
                    filter,
                    limit: Some(1),
                });
            }
            if let Some(arg) = strip_call(method_call, "countDocuments") {
                let filter = parse_doc_json(arg)?;
                return Ok(MongoCommand::CollectionCount {
                    collection: collection.to_string(),
                    filter,
                });
            }
            if let Some(arg) = strip_call(method_call, "insertOne") {
                let doc = parse_doc_json(arg)?;
                return Ok(MongoCommand::CollectionInsertOne {
                    collection: collection.to_string(),
                    doc,
                });
            }
            if let Some(arg) = strip_call(method_call, "deleteOne") {
                let filter = parse_doc_json(arg)?;
                return Ok(MongoCommand::CollectionDelete {
                    collection: collection.to_string(),
                    filter,
                    many: false,
                });
            }
            if let Some(arg) = strip_call(method_call, "deleteMany") {
                let filter = parse_doc_json(arg)?;
                return Ok(MongoCommand::CollectionDelete {
                    collection: collection.to_string(),
                    filter,
                    many: true,
                });
            }
            if strip_call(method_call, "stats").is_some() {
                return Ok(MongoCommand::CollectionStats {
                    collection: collection.to_string(),
                });
            }
        }
    }
    Err(DbError::coded(
        "naviduck.unsupported_command",
        "Unsupported command. Use JSON {\"ping\": 1}, show dbs, show collections, db.runCommand({...}), or db.<collection>.find({...}).",
    ))
}

fn strip_call<'a>(s: &'a str, method: &str) -> Option<&'a str> {
    let after = s.strip_prefix(method)?.trim_start();
    let inside = after.strip_prefix('(')?.strip_suffix(')')?;
    Some(inside.trim())
}

/// Splits `<collection>.<method>(...)` where `<collection>` may use `getCollection("name")`
/// or a plain identifier.
fn split_collection_call(rest: &str) -> Option<(&str, &str)> {
    if let Some(after) = rest.strip_prefix("getCollection(") {
        let close = after.find(')')?;
        let raw_name = after[..close].trim();
        let name = raw_name
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                raw_name
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })?;
        let tail = after[close + 1..].trim_start().strip_prefix('.')?;
        return Some((name, tail));
    }
    let dot = rest.find('.')?;
    let coll = rest[..dot].trim();
    if coll.is_empty() || coll.contains('(') {
        return None;
    }
    Some((coll, rest[dot + 1..].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn extjson_round_trips_object_id_and_nested_fields() {
        let oid = mongodb::bson::oid::ObjectId::parse_str("65f1a2b3c4d5e6f708192a3b").unwrap();
        let original = doc! {
            "_id": oid,
            "name": "alice",
            "tags": ["admin", "ops"],
            "meta": { "count": 42_i32 }
        };
        let json = doc_to_pretty_json(&original);
        let parsed = parse_doc_json(&json).unwrap();
        assert_eq!(parsed.get_object_id("_id").unwrap(), oid);
        assert_eq!(parsed.get_str("name").unwrap(), "alice");
        assert_eq!(
            parsed
                .get_document("meta")
                .unwrap()
                .get_i32("count")
                .unwrap(),
            42
        );
    }

    #[test]
    fn parses_shell_commands_and_json_run_command() {
        assert_eq!(
            parse_command("show dbs;").unwrap(),
            MongoCommand::ShowDatabases
        );
        assert_eq!(
            parse_command("show collections").unwrap(),
            MongoCommand::ShowCollections
        );
        assert_eq!(
            parse_command(r#"{"ping": 1}"#).unwrap(),
            MongoCommand::RunCommand(doc! { "ping": 1 })
        );
        assert_eq!(
            parse_command(r#"db.runCommand({"buildInfo": 1})"#).unwrap(),
            MongoCommand::RunCommand(doc! { "buildInfo": 1 })
        );
        assert_eq!(
            parse_command(r#"db.users.find({"active": true})"#).unwrap(),
            MongoCommand::CollectionFind {
                collection: "users".into(),
                filter: doc! { "active": true },
                limit: Some(50),
            }
        );
        assert_eq!(
            parse_command(r#"db.getCollection("my.users").findOne()"#).unwrap(),
            MongoCommand::CollectionFind {
                collection: "my.users".into(),
                filter: doc! {},
                limit: Some(1),
            }
        );
        assert!(parse_command("   ").is_err());
    }

    #[test]
    fn maps_documents_into_result_sets_with_id_first() {
        let docs = vec![
            doc! { "z": 1, "_id": "a", "name": "first" },
            doc! { "name": "second", "_id": "b" },
        ];
        let sets = docs_to_result_sets(&docs);
        assert_eq!(sets.len(), 1);
        let names: Vec<_> = sets[0].columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "name", "z"]);
        assert_eq!(
            sets[0].rows[0],
            vec![Some("a".into()), Some("first".into()), Some("1".into())]
        );
        assert_eq!(
            sets[0].rows[1],
            vec![Some("b".into()), Some("second".into()), None]
        );
    }
}
