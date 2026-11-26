//! Redis driver: `Driver`/`Session`/`KeyValue` over fred (141-redis.md §3).
//! fred is async, so commands run directly on `naviduck_db::runtime`. One fred client
//! serves the whole session; every command carries the target database, so the lazy
//! `SELECT` switch is safe under the shared session's serialization.

use fred::prelude::{Client, ClientLike as _, Error as FredError, ErrorKind, Value};
use fred::types::{ClusterHash, CustomCommand};
use futures::FutureExt as _;
use futures::future::BoxFuture;

use naviduck_db::driver::{
    CancelHandle, DbError, DbResult, Driver, DriverInfo, KeyValue, KeyValueData, ResultSet, Session,
};
use naviduck_db::log::{Tag, logged};
use naviduck_db::params::ConnectParams;

use crate::config::build_client;
use crate::mapping::{read_value_to_data, reply_to_result_sets, value_pairs, value_text};

pub struct RedisDriver {
    info: DriverInfo,
}

impl Default for RedisDriver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                default_port: naviduck_model::connection::driver_defaults("redis").port,
                icon: "icons/db/redis.png",
                supports_network: true,
                supports_ssl: true,
                supports_ssh: true,
                database_number: true,
                ..DriverInfo::key_value("redis", "Redis")
            },
        }
    }
}

/// Maps a fred error. Server replies carry their code as the first word of the details
/// (`WRONGTYPE …`, `NOAUTH …`); IO and timeout errors mean the session is gone.
fn err(e: FredError) -> DbError {
    let disconnected = matches!(
        e.kind(),
        ErrorKind::IO | ErrorKind::Timeout | ErrorKind::Canceled
    );
    let details = e.details().trim();
    let (code, message) = match details.split_once(' ') {
        Some((word, rest)) if is_error_code(word) => (Some(word.to_string()), rest.to_string()),
        _ if !details.is_empty() => (None, details.to_string()),
        _ => (None, e.to_string()),
    };
    let mut out = match code {
        Some(code) => DbError::coded(code, message),
        None => DbError::msg(message),
    };
    out.disconnected = disconnected;
    out
}

/// RESP error prefixes are bare uppercase words (`ERR`, `WRONGTYPE`, `NOAUTH`,
/// `LOADING`, `CLUSTERDOWN`).
fn is_error_code(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 32
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
}

fn cmd(name: &str) -> CustomCommand {
    // Standalone servers ignore the hash policy; cluster support comes later (141-redis.md D1).
    CustomCommand::new(name, ClusterHash::FirstKey, false)
}

/// Sends one command on the given client. Every command goes through fred's generic
/// `custom` call, so replies always arrive as one uniform `Value`.
async fn try_cmd(tag: &Tag, client: &Client, args: Vec<String>) -> DbResult<Value> {
    let (name, rest) = args
        .split_first()
        .ok_or_else(|| DbError::coded("naviduck.empty_command", "The command is empty."))?;
    let text = args.join(" ");
    let (name, rest) = ((*name).clone(), rest.to_vec());
    logged(tag, &text, async move {
        client
            .custom::<Value, _>(cmd(&name), rest)
            .await
            .map_err(err)
    })
    .await
}

impl Driver for RedisDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, DbResult<Box<dyn Session>>> {
        let tag = Tag::new(&params.name, &self.info.name);
        async move {
            let client = tokio::task::spawn_blocking({
                let params = params.clone();
                move || build_client(&params)
            })
            .await
            .map_err(|e| DbError::msg(e.to_string()))??;
            // `init` connects and authenticates; errors here are the real connect error.
            client.init().await.map_err(err)?;
            let version = probe_version(&client, &tag).await;
            let initial_db = crate::config::database_index(&params)? as u32;
            tracing::debug!(db = initial_db, "Redis session established");
            Ok(Box::new(RedisSession {
                client,
                params,
                version,
                db: initial_db,
                initial_db,
                tag,
            }) as Box<dyn Session>)
        }
        .boxed()
    }
}

/// `INFO server` once at connect time; a probe failure never blocks the connection.
async fn probe_version(client: &Client, tag: &Tag) -> String {
    match try_cmd(tag, client, vec!["INFO".into(), "server".into()]).await {
        Ok(v) => value_text(&v)
            .unwrap_or_default()
            .lines()
            .find_map(|line| line.strip_prefix("redis_version:"))
            .map(|v| format!("Redis {v}"))
            .unwrap_or_else(|| "Redis".into()),
        Err(e) => {
            tracing::warn!("INFO probe failed: {e}");
            "Redis".into()
        }
    }
}

pub struct RedisSession {
    client: Client,
    params: ConnectParams,
    version: String,
    /// The database the connection is currently on; `SELECT` before commands when it differs.
    db: u32,
    initial_db: u32,
    tag: Tag,
}

impl RedisSession {
    async fn attempt(&self, args: Vec<String>) -> DbResult<Value> {
        try_cmd(&self.tag, &self.client, args).await
    }

    async fn ensure_db(&mut self, db: u32) -> DbResult<()> {
        if self.db != db {
            self.attempt(vec!["SELECT".into(), db.to_string()]).await?;
            self.db = db;
        }
        Ok(())
    }

    /// Read commands may retry once on a fresh connection: key-value reads carry no session
    /// state, and `ensure_db` replays the database switch after reconnecting. Writes never
    /// retry; their outcome is preserved via `outcome_unknown` instead. The closure receives
    /// a client clone so it can be invoked again after the reconnect.
    async fn retrying_read<T, F, Fut>(&mut self, db: u32, f: F) -> DbResult<T>
    where
        F: Fn(Client) -> Fut,
        Fut: std::future::Future<Output = DbResult<T>>,
    {
        self.ensure_db(db).await?;
        let first = f(self.client.clone()).await;
        match first {
            Err(e) if e.disconnected => {
                self.reconnect().await?;
                self.ensure_db(db).await?;
                f(self.client.clone()).await.map_err(|e| {
                    if e.disconnected {
                        naviduck_db::execution::interrupted(e, false)
                    } else {
                        e
                    }
                })
            }
            other => other,
        }
    }

    async fn reconnect(&mut self) -> DbResult<()> {
        let params = self.params.clone();
        let client = tokio::task::spawn_blocking(move || build_client(&params))
            .await
            .map_err(|e| DbError::msg(e.to_string()))??;
        client.init().await.map_err(err)?;
        self.version = probe_version(&client, &self.tag).await;
        self.client = client;
        self.db = self.initial_db;
        Ok(())
    }

    async fn write(&mut self, args: Vec<String>) -> DbResult<()> {
        self.attempt(args).await.map(|_| ()).map_err(|e| {
            // A write whose reply never arrived must not be replayed.
            if e.disconnected {
                naviduck_db::execution::interrupted(e, true)
            } else {
                e
            }
        })
    }
}

/// The command that reads one page of `key_type`; the widest `offset`/`count` reads everything.
fn read_command(key_type: &str, key: &str, offset: u64, count: u32) -> Vec<String> {
    let key = key.to_string();
    match key_type {
        "string" => vec!["GET".into(), key],
        "list" => vec![
            "LRANGE".into(),
            key,
            offset.to_string(),
            offset.saturating_add(count.max(1) as u64 - 1).to_string(),
        ],
        // Hashes and sets have no positional read; the editor pages the reply locally.
        "hash" => vec!["HGETALL".into(), key],
        "set" => vec!["SMEMBERS".into(), key],
        "zset" => vec![
            "ZRANGE".into(),
            key,
            offset.to_string(),
            offset.saturating_add(count.max(1) as u64 - 1).to_string(),
            "WITHSCORES".into(),
        ],
        "stream" => vec![
            "XRANGE".into(),
            key,
            "-".into(),
            "+".into(),
            "COUNT".into(),
            count.max(1).to_string(),
        ],
        other => vec!["GET".into(), key, other.into()],
    }
}

impl Session for RedisSession {
    fn server_version(&self) -> &str {
        &self.version
    }

    fn ping(&mut self) -> BoxFuture<'_, DbResult<()>> {
        async move {
            let tag = self.tag.clone();
            self.retrying_read(0, move |client| {
                let tag = tag.clone();
                async move {
                    try_cmd(&tag, &client, vec!["PING".into()])
                        .await
                        .map(|_| ())
                }
            })
            .await
        }
        .boxed()
    }

    fn cancel_handle(&self) -> Option<CancelHandle> {
        None
    }

    fn session_id(&self) -> Option<u64> {
        None
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, DbResult<()>> {
        async move {
            if let Err(e) = self.client.quit().await {
                tracing::warn!("Redis quit failed: {e}");
            }
            Ok(())
        }
        .boxed()
    }

    fn key_value(&mut self) -> Option<&mut dyn KeyValue> {
        Some(self)
    }
}

impl KeyValue for RedisSession {
    fn databases(&mut self) -> BoxFuture<'_, DbResult<Vec<u32>>> {
        async move {
            let tag = self.tag.clone();
            let count = self
                .retrying_read(0, move |client| {
                    let tag = tag.clone();
                    async move {
                        try_cmd(
                            &tag,
                            &client,
                            vec!["CONFIG".into(), "GET".into(), "databases".into()],
                        )
                        .await
                    }
                })
                .await?;
            let value = value_pairs(&count)
                .and_then(|pairs| pairs.into_iter().next().map(|(_, v)| v))
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(16);
            Ok((0..value).collect())
        }
        .boxed()
    }

    fn key_count(&mut self, db: u32) -> BoxFuture<'_, DbResult<u64>> {
        async move {
            let tag = self.tag.clone();
            self.retrying_read(db, move |client| {
                let tag = tag.clone();
                async move {
                    let reply = try_cmd(&tag, &client, vec!["DBSIZE".into()]).await?;
                    reply
                        .as_u64()
                        .ok_or_else(|| DbError::msg("DBSIZE returned a non-integer."))
                }
            })
            .await
        }
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
            let tag = self.tag.clone();
            let pattern = pattern.to_string();
            self.retrying_read(db, move |client| {
                let tag = tag.clone();
                let pattern = pattern.clone();
                async move {
                    let reply = try_cmd(
                        &tag,
                        &client,
                        vec![
                            "SCAN".into(),
                            cursor.to_string(),
                            "MATCH".into(),
                            pattern,
                            "COUNT".into(),
                            count.to_string(),
                        ],
                    )
                    .await?;
                    match reply_pairs(&reply) {
                        Some((next, keys)) => Ok((next, keys)),
                        None => Err(DbError::msg("SCAN returned an unexpected reply shape.")),
                    }
                }
            })
            .await
        }
        .boxed()
    }

    fn key_type<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<String>> {
        async move {
            let tag = self.tag.clone();
            let key = key.to_string();
            self.retrying_read(db, move |client| {
                let tag = tag.clone();
                let key = key.clone();
                async move {
                    let ty = try_cmd(&tag, &client, vec!["TYPE".into(), key]).await?;
                    Ok(value_text(&ty).unwrap_or_default())
                }
            })
            .await
        }
        .boxed()
    }

    fn ttl<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<i64>> {
        async move {
            let tag = self.tag.clone();
            let key = key.to_string();
            self.retrying_read(db, move |client| {
                let tag = tag.clone();
                let key = key.clone();
                async move {
                    let reply = try_cmd(&tag, &client, vec!["TTL".into(), key]).await?;
                    reply
                        .as_i64()
                        .ok_or_else(|| DbError::msg("TTL returned a non-integer."))
                }
            })
            .await
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
            let tag = self.tag.clone();
            let key = key.to_string();
            let (ty, value) = self
                .retrying_read(db, move |client| {
                    let tag = tag.clone();
                    let key = key.clone();
                    async move {
                        let ty = try_cmd(&tag, &client, vec!["TYPE".into(), key.clone()]).await?;
                        let ty = value_text(&ty).unwrap_or_default();
                        let value = try_cmd(
                            &tag,
                            &client,
                            // The widest bounds read the whole value; the editor pages locally.
                            read_command(&ty, &key, 0, u32::MAX),
                        )
                        .await?;
                        Ok((ty, value))
                    }
                })
                .await?;
            if ty == "none" {
                return Ok(KeyValueData::Other("none".into()));
            }
            Ok(read_value_to_data(&ty, value))
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
            self.ensure_db(db).await?;
            self.write(vec!["SET".into(), key.into(), value.into()])
                .await
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
            self.ensure_db(db).await?;
            self.write(vec!["HSET".into(), key.into(), field.into(), value.into()])
                .await
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
            self.ensure_db(db).await?;
            self.write(vec!["HDEL".into(), key.into(), field.into()])
                .await
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
            self.ensure_db(db).await?;
            let head = if tail { "RPUSH" } else { "LPUSH" };
            self.write(vec![head.into(), key.into(), value.into()])
                .await
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
            self.ensure_db(db).await?;
            // 0 removes every occurrence (F-REDIS-005 editor semantics).
            self.write(vec!["LREM".into(), key.into(), "0".into(), value.into()])
                .await
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
            self.ensure_db(db).await?;
            self.write(vec!["SADD".into(), key.into(), member.into()])
                .await
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
            self.ensure_db(db).await?;
            self.write(vec!["SREM".into(), key.into(), member.into()])
                .await
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
            self.ensure_db(db).await?;
            self.write(vec!["ZADD".into(), key.into(), score.into(), member.into()])
                .await
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
            self.ensure_db(db).await?;
            self.write(vec!["ZREM".into(), key.into(), member.into()])
                .await
        }
        .boxed()
    }

    fn create_key<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<()>> {
        async move {
            self.ensure_db(db).await?;
            self.write(vec!["SET".into(), key.into(), String::new()])
                .await
        }
        .boxed()
    }

    fn delete_keys<'a>(&'a mut self, db: u32, keys: &'a [String]) -> BoxFuture<'a, DbResult<u64>> {
        async move {
            self.ensure_db(db).await?;
            let mut args = vec!["DEL".to_string()];
            args.extend(keys.iter().cloned());
            self.write(args).await?;
            Ok(keys.len() as u64)
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
            self.ensure_db(db).await?;
            self.write(vec!["RENAME".into(), key.into(), new_name.into()])
                .await
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
            self.ensure_db(db).await?;
            match seconds {
                Some(seconds) => {
                    self.write(vec!["EXPIRE".into(), key.into(), seconds.to_string()])
                        .await
                }
                None => self.write(vec!["PERSIST".into(), key.into()]).await,
            }
        }
        .boxed()
    }

    fn execute_command<'a>(
        &'a mut self,
        db: u32,
        args: &'a [String],
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>> {
        async move {
            self.ensure_db(db).await?;
            let reply = self.attempt(args.to_vec()).await?;
            Ok(reply_to_result_sets(reply))
        }
        .boxed()
    }
}

/// SCAN's `[cursor, key…]` reply.
fn reply_pairs(reply: &Value) -> Option<(u64, Vec<String>)> {
    match reply {
        Value::Array(items) if !items.is_empty() => {
            let cursor = value_text(&items[0])?.parse::<u64>().ok()?;
            // RESP2 nests the key list inside the second element.
            let keys = match items.get(1) {
                Some(Value::Array(inner)) => {
                    inner.iter().filter_map(value_text).collect::<Vec<_>>()
                }
                _ => items[1..].iter().filter_map(value_text).collect(),
            };
            Some((cursor, keys))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fred::error::Error as FredError;

    #[test]
    fn server_errors_keep_their_resp_code() {
        let e = FredError::new(ErrorKind::InvalidArgument, "WRONGTYPE wrong kind of value");
        let e = err(e);
        assert_eq!(e.code.as_deref(), Some("WRONGTYPE"));
        assert_eq!(e.message, "wrong kind of value");
        assert!(!e.disconnected);

        let e = FredError::new(ErrorKind::Auth, "WRONGPASS invalid username-password pair");
        assert_eq!(err(e).code.as_deref(), Some("WRONGPASS"));

        let e = FredError::new(ErrorKind::Unknown, "ERR unknown command 'FOO'");
        assert_eq!(err(e).code.as_deref(), Some("ERR"));
    }

    #[test]
    fn io_errors_mark_the_session_lost() {
        let e = err(FredError::new(ErrorKind::IO, "connection closed"));
        assert!(e.disconnected);
        assert_eq!(e.code, None);
        let e = err(FredError::new(ErrorKind::Timeout, "command timed out"));
        assert!(e.disconnected);
    }

    #[test]
    fn plain_details_do_not_gain_a_code() {
        let e = err(FredError::new(ErrorKind::IO, "connection refused by peer"));
        assert_eq!(e.code, None);
        assert_eq!(e.message, "connection refused by peer");
    }

    #[test]
    fn scan_replies_split_cursor_and_keys() {
        // RESP2 nests the key list inside the second element.
        let nested = Value::Array(vec![
            Value::from("17"),
            Value::Array(vec![Value::from("k1"), Value::from("k2")]),
        ]);
        assert_eq!(
            reply_pairs(&nested),
            Some((17, vec!["k1".into(), "k2".into()]))
        );
        let flat = Value::Array(vec![Value::from("0"), Value::from("k1")]);
        assert_eq!(reply_pairs(&flat), Some((0, vec!["k1".into()])));
        assert_eq!(reply_pairs(&Value::Null), None);
    }

    #[test]
    fn read_commands_match_the_key_type() {
        let expected: Vec<String> = vec!["GET".into(), "k".into()];
        assert_eq!(read_command("string", "k", 0, 100), expected);
        let expected: Vec<String> = vec!["LRANGE".into(), "k".into(), "10".into(), "29".into()];
        assert_eq!(read_command("list", "k", 10, 20), expected);
        let expected: Vec<String> = vec![
            "ZRANGE".into(),
            "k".into(),
            "0".into(),
            "4".into(),
            "WITHSCORES".into(),
        ];
        assert_eq!(read_command("zset", "k", 0, 5), expected);
        let expected: Vec<String> = vec![
            "XRANGE".into(),
            "k".into(),
            "-".into(),
            "+".into(),
            "COUNT".into(),
            "3".into(),
        ];
        assert_eq!(read_command("stream", "k", 0, 3), expected);
        let expected: Vec<String> = vec!["HGETALL".into(), "k".into()];
        assert_eq!(read_command("hash", "k", 0, 0), expected);
    }
}
