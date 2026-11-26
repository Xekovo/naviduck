//! Maps fred `Value` replies to naviduck's text grid, key data and result sets
//! (141-redis.md §4.1). Pure functions; unit tests pin the RESP2 shapes that all
//! server versions produce.

use fred::prelude::Value;
use naviduck_db::driver::{Column, KeyValueData, ResultSet};

/// Grid text of one value. Bytes that are not UTF-8 show as `0x…` hex so the text is
/// unambiguous and round-trips through the editor (141-redis.md D7).
pub(crate) fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Boolean(b) => Some(if *b { "1".into() } else { "0".into() }),
        Value::Integer(i) => Some(i.to_string()),
        Value::Double(f) => Some(double_text(*f)),
        Value::String(s) => Some(bytes_to_text(s.as_bytes())),
        Value::Bytes(b) => Some(bytes_to_text(b)),
        Value::Queued => Some("QUEUED".into()),
        // Composite values are rendered by `reply_to_result_sets`; a bare one has no
        // honest flat text, so it shows the kind instead.
        Value::Map(_) => Some("{map}".into()),
        Value::Array(_) => Some("[array]".into()),
    }
}

pub(crate) fn bytes_to_text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => format!("0x{}", hex::encode(b)),
    }
}

/// Redis prints scores without a trailing `.0` (`3` not `3.0`).
fn double_text(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f.trunc() as i64)
    } else {
        format!("{f}")
    }
}

/// The inverse of `bytes_to_text`: hex text goes back to bytes, anything else is UTF-8.
/// The key editor uses it to write hex cells back (141-redis.md D7); the driver needs it
/// once member writes gain a binary form.
#[allow(dead_code)]
pub(crate) fn text_to_bytes(s: &str) -> Vec<u8> {
    let trimmed = s.trim();
    if let Some(hex_text) = trimmed.strip_prefix("0x")
        && let Ok(bytes) = hex::decode(hex_text)
    {
        return bytes;
    }
    s.as_bytes().to_vec()
}

/// A flat reply as field/value pairs: RESP3 maps and the RESP2 flat arrays that
/// HGETALL / CONFIG GET / ZRANGE WITHSCORES / SCAN produce.
pub(crate) fn value_pairs(v: &Value) -> Option<Vec<(String, String)>> {
    match v {
        Value::Map(m) => Some(
            m.iter()
                .filter_map(|(k, v)| Some((bytes_to_text(k.as_bytes()), value_text(v)?)))
                .collect(),
        ),
        Value::Array(items) => {
            if items.iter().all(scalar) && items.len().is_multiple_of(2) {
                Some(
                    items
                        .chunks(2)
                        .map_while(|pair| Some((value_text(&pair[0])?, value_text(&pair[1])?)))
                        .collect(),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

fn scalar(v: &Value) -> bool {
    matches!(
        v,
        Value::Null
            | Value::Boolean(_)
            | Value::Integer(_)
            | Value::Double(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Queued
    )
}

/// A `read` reply as the editor's value, dispatched by the type the server reported.
pub(crate) fn read_value_to_data(key_type: &str, v: Value) -> KeyValueData {
    match key_type {
        "string" => KeyValueData::Str(value_text(&v).unwrap_or_default()),
        "list" => KeyValueData::List(value_items(&v).unwrap_or_default()),
        "set" => KeyValueData::Set(value_items(&v).unwrap_or_default()),
        "hash" => KeyValueData::Hash(value_pairs(&v).unwrap_or_default()),
        "zset" => KeyValueData::ZSet(value_pairs(&v).unwrap_or_default()),
        "stream" => KeyValueData::Stream(stream_entries(&v)),
        other => KeyValueData::Other(other.to_string()),
    }
}

fn value_items(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(items) => Some(items.iter().filter_map(value_text).collect()),
        Value::Null => Some(Vec::new()),
        Value::String(_) | Value::Bytes(_) => Some(vec![value_text(v)?]),
        _ => None,
    }
}

/// XRANGE entries: `[id, [field, value, …]]` pairs (RESP2) or a map of id → fields (RESP3).
fn stream_entries(v: &Value) -> Vec<(String, Vec<(String, String)>)> {
    match v {
        Value::Array(entries) => entries
            .iter()
            .filter_map(|entry| match entry {
                Value::Array(pair) if pair.len() == 2 => Some((
                    value_text(&pair[0]).unwrap_or_default(),
                    value_pairs(&pair[1]).unwrap_or_default(),
                )),
                _ => None,
            })
            .collect(),
        Value::Map(m) => m
            .iter()
            .map(|(id, fields)| {
                (
                    bytes_to_text(id.as_bytes()),
                    value_pairs(fields).unwrap_or_default(),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// One command reply as result sets for the query console (141-redis.md §4.1).
pub(crate) fn reply_to_result_sets(v: Value) -> Vec<ResultSet> {
    match v {
        Value::Integer(i) => vec![ResultSet {
            affected_rows: Some(i.max(0) as u64),
            ..ResultSet::default()
        }],
        Value::Null => vec![null_set()],
        Value::Map(_) => pairs_set(value_pairs(&v).unwrap_or_default()),
        Value::Array(items) => reply_array(items),
        ref scalar => vec![scalar_set(scalar)],
    }
}
fn reply_array(items: Vec<Value>) -> Vec<ResultSet> {
    let all_pairs = items.iter().all(is_pair);
    let all_scalars = items.iter().all(scalar);
    if all_scalars {
        // HGETALL / CONFIG GET / ZRANGE WITHSCORES ship as one flat array.
        if items.len().is_multiple_of(2) {
            return pairs_set(
                items
                    .chunks(2)
                    .map_while(|pair| Some((value_text(&pair[0])?, value_text(&pair[1])?)))
                    .collect(),
            );
        }
        return vec![rows_set(
            &["value"],
            items.iter().map(|v| vec![value_text(v)]),
        )];
    }
    if all_pairs {
        // XRANGE: `[id, [field, value, …]]` rows. The widest entry fixes the columns.
        let width = items
            .iter()
            .filter_map(|v| match v {
                Value::Array(entry) => Some(
                    1 + entry
                        .iter()
                        .filter(|f| matches!(f, Value::Array(inner) if inner.len() == 2))
                        .count()
                        * 2,
                ),
                _ => None,
            })
            .max()
            .unwrap_or(1);
        let names: Vec<String> = std::iter::once("id".to_string())
            .chain((2..=width).map(|i| format!("col{i}")))
            .collect();
        let rows = items.iter().map(|v| {
            let mut row: Vec<Option<String>> = match v {
                Value::Array(entry) => entry
                    .iter()
                    .flat_map(|f| match f {
                        Value::Array(inner) if inner.len() == 2 => {
                            vec![value_text(&inner[0]), value_text(&inner[1])]
                        }
                        _ => vec![value_text(f)],
                    })
                    .collect(),
                _ => vec![value_text(v)],
            };
            row.resize(width, None);
            row
        });
        return vec![rows_set(
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            rows,
        )];
    }
    // Mixed shapes (SCAN's `[cursor, keys…]`, nested arrays): one column, arrays inlined.
    vec![rows_set(
        &["value"],
        items.iter().map(|v| vec![value_text(v)]),
    )]
}

fn is_pair(v: &Value) -> bool {
    matches!(
        v,
        Value::Array(inner) if inner.len() == 2 && inner.iter().any(|f| matches!(f, Value::Array(_) | Value::Map(_)))
    )
}

fn null_set() -> ResultSet {
    ResultSet {
        columns: vec![Column {
            name: "result".into(),
            ..Column::default()
        }],
        rows: vec![vec![None]],
        ..ResultSet::default()
    }
}

fn scalar_set(v: &Value) -> ResultSet {
    rows_set(&["result"], std::iter::once(vec![value_text(v)]))
}

fn pairs_set(pairs: Vec<(String, String)>) -> Vec<ResultSet> {
    vec![rows_set(
        &["field", "value"],
        pairs.into_iter().map(|(f, v)| vec![Some(f), Some(v)]),
    )]
}

fn rows_set<I>(names: &[&str], rows: I) -> ResultSet
where
    I: IntoIterator<Item = Vec<Option<String>>>,
{
    ResultSet {
        columns: names
            .iter()
            .map(|n| Column {
                name: (*n).into(),
                ..Column::default()
            })
            .collect(),
        rows: rows.into_iter().collect(),
        ..ResultSet::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fred::types::{Key, Map};
    use naviduck_db::driver::KeyValueData;
    use std::collections::HashMap;

    fn s(text: &str) -> Value {
        Value::from(text)
    }

    fn map_from(pairs: [(&str, &str); 1]) -> Map {
        Map::try_from(HashMap::from([(
            Key::from(pairs[0].0),
            Value::from(pairs[0].1),
        )]))
        .unwrap()
    }

    #[test]
    fn non_utf8_bytes_show_as_hex() {
        let bytes = Value::Bytes(vec![0x89, 0x50, 0x4e].into());
        assert_eq!(value_text(&bytes).unwrap(), "0x89504e");
        assert_eq!(text_to_bytes("0x89504e"), vec![0x89, 0x50, 0x4e]);
        assert_eq!(text_to_bytes("plain"), b"plain".to_vec());
    }

    #[test]
    fn scalars_render_as_grid_text() {
        assert_eq!(value_text(&Value::Integer(42)).unwrap(), "42");
        assert_eq!(value_text(&Value::Double(1.5)).unwrap(), "1.5");
        assert_eq!(value_text(&Value::Boolean(true)).unwrap(), "1");
        assert_eq!(value_text(&Value::Null), None);
        assert_eq!(value_text(&s("hello")).unwrap(), "hello");
    }

    #[test]
    fn hash_and_zset_replies_become_pairs() {
        let flat = Value::Array(vec![s("a"), s("1"), s("b"), s("2")]);
        assert_eq!(
            value_pairs(&flat),
            Some(vec![("a".into(), "1".into()), ("b".into(), "2".into())])
        );
        let map = Value::Map(map_from([("a", "1")]));
        assert_eq!(value_pairs(&map), Some(vec![("a".into(), "1".into())]));
        // An odd-length array is not pairs.
        assert_eq!(value_pairs(&Value::Array(vec![s("only")])), None);
    }

    #[test]
    fn read_replies_dispatch_by_type() {
        assert_eq!(
            read_value_to_data("string", s("v")),
            KeyValueData::Str("v".into())
        );
        assert_eq!(
            read_value_to_data("list", Value::Array(vec![s("a"), s("b")])),
            KeyValueData::List(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            read_value_to_data("zset", Value::Array(vec![s("m"), s("0.5")])),
            KeyValueData::ZSet(vec![("m".into(), "0.5".into())])
        );
        assert_eq!(
            read_value_to_data("module", s("x")),
            KeyValueData::Other("module".into())
        );
        let entries = Value::Array(vec![Value::Array(vec![
            s("1-1"),
            Value::Array(vec![s("k"), s("v")]),
        ])]);
        assert_eq!(
            read_value_to_data("stream", entries),
            KeyValueData::Stream(vec![("1-1".into(), vec![("k".into(), "v".into())])])
        );
    }

    #[test]
    fn replies_map_to_result_sets() {
        assert_eq!(
            reply_to_result_sets(Value::Integer(3))[0].affected_rows,
            Some(3)
        );
        let ok = reply_to_result_sets(s("OK"));
        assert_eq!(ok[0].columns[0].name, "result");
        assert_eq!(ok[0].rows[0][0].as_deref(), Some("OK"));

        let pairs = reply_to_result_sets(Value::Array(vec![s("a"), s("1"), s("b"), s("2")]));
        assert_eq!(pairs[0].columns.len(), 2);
        assert_eq!(pairs[0].rows.len(), 2);

        let scan = reply_to_result_sets(Value::Array(vec![s("17"), Value::Array(vec![s("k1")])]));
        assert_eq!(scan[0].columns[0].name, "value");
        assert_eq!(scan[0].rows[0][0].as_deref(), Some("17"));

        let entries = reply_to_result_sets(Value::Array(vec![Value::Array(vec![
            s("1-1"),
            Value::Array(vec![s("k"), s("v")]),
        ])]));
        assert_eq!(entries[0].columns[0].name, "id");
        assert_eq!(entries[0].rows[0][0].as_deref(), Some("1-1"));
        assert_eq!(entries[0].rows[0][1].as_deref(), Some("k"));
        assert_eq!(entries[0].rows[0][2].as_deref(), Some("v"));
    }
}
