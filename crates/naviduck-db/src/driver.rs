//! Driver abstraction: a `Driver` creates a `Session`, and the session exposes `Relational`,
//! `Document` or `KeyValue` depending on the database family.
//! Phase one implements only `Relational`; the other two are interfaces only, implemented when
//! MongoDB and Redis land in M14.

use std::sync::Arc;

use futures::future::BoxFuture;
use naviduck_model::object::{ObjectKind, Scope};

use crate::params::ConnectParams;
use naviduck_sql::dialect::Dialect;

/// Driver error. `code` keeps the database's original error code, displayed in Navicat's
/// "1045 - Access denied …" format.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct DbError {
    pub code: Option<String>,
    pub message: String,
    /// The physical session is dead. Ordinary SQL must never be replayed automatically;
    /// only an explicitly stateless read can opt in to recovery on a fresh session.
    pub disconnected: bool,
    pub outcome_unknown: bool,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{code} - {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl DbError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
            disconnected: false,
            outcome_unknown: false,
        }
    }

    pub fn coded(code: impl ToString, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.to_string()),
            message: message.into(),
            disconnected: false,
            outcome_unknown: false,
        }
    }

    /// The physical session cannot accept more work. UI callers decide how to explain recovery.
    pub fn is_session_lost(&self) -> bool {
        self.disconnected
    }

    /// Ordinary SQL was submitted, but a disconnect prevented confirming its outcome.
    pub fn is_outcome_unknown(&self) -> bool {
        self.outcome_unknown
    }

    pub fn with_session_loss(mut self, outcome_unknown: bool) -> Self {
        self.disconnected = true;
        self.outcome_unknown = outcome_unknown;
        self
    }
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Relational,
    Document,
    KeyValue,
}

#[derive(Clone, Debug)]
pub struct DriverInfo {
    /// Registry key, also the `driver` field in connections.json
    pub id: String,
    pub name: String,
    pub family: Family,
    pub default_port: Option<u16>,
    pub icon: &'static str,
    /// Relational SQL dialect; `None` for non-SQL families (Redis, MongoDB).
    pub dialect: Option<Arc<dyn Dialect>>,
    /// File database (SQLite): the connection is a path, not a host.
    pub file_based: bool,
    /// The general page can list several hosts (PostgreSQL).
    pub multi_host: bool,
    pub supports_network: bool,
    pub supports_ssl: bool,
    /// The SSL page offers a mode list (PostgreSQL libpq). MySQL only has the on/off switch.
    pub chooses_ssl_mode: bool,
    /// Connect, read and write timeouts, compression and a socket path (MySQL protocol).
    pub protocol_timeouts: bool,
    /// A free-form connection-parameter string (libpq).
    pub connection_params: bool,
    pub supports_ssh: bool,
    /// A session can access only one database (PostgreSQL); browsing another database needs
    /// a separate session
    pub per_database_sessions: bool,
    /// The general page takes a numeric database index (Redis) instead of a name.
    pub database_number: bool,
    /// The general page shows the optional initial/auth database name (MongoDB).
    pub initial_database: bool,
}

impl DriverInfo {
    /// Baseline for a relational driver. Every capability starts off, so a driver opts in to
    /// the network, SSL, SSH and the rest explicitly, and a new field does not break drivers
    /// that use struct update syntax.
    pub fn relational(
        id: impl Into<String>,
        name: impl Into<String>,
        dialect: Arc<dyn Dialect>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            family: Family::Relational,
            default_port: None,
            icon: "",
            dialect: Some(dialect),
            file_based: false,
            multi_host: false,
            supports_network: false,
            supports_ssl: false,
            chooses_ssl_mode: false,
            protocol_timeouts: false,
            connection_params: false,
            supports_ssh: false,
            per_database_sessions: false,
            database_number: false,
            initial_database: false,
        }
    }

    /// Baseline for a non-SQL key-value driver: no dialect at all.
    pub fn key_value(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            family: Family::KeyValue,
            default_port: None,
            icon: "",
            dialect: None,
            file_based: false,
            multi_host: false,
            supports_network: false,
            supports_ssl: false,
            chooses_ssl_mode: false,
            protocol_timeouts: false,
            connection_params: false,
            supports_ssh: false,
            per_database_sessions: false,
            database_number: false,
            initial_database: false,
        }
    }

    /// Baseline for a non-SQL document driver: no dialect at all.
    pub fn document(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            family: Family::Document,
            default_port: None,
            icon: "",
            dialect: None,
            file_based: false,
            multi_host: false,
            supports_network: false,
            supports_ssl: false,
            chooses_ssl_mode: false,
            protocol_timeouts: false,
            connection_params: false,
            supports_ssh: false,
            per_database_sessions: false,
            database_number: false,
            initial_database: false,
        }
    }
}

pub trait Driver: Send + Sync + 'static {
    fn info(&self) -> &DriverInfo;
    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, DbResult<Box<dyn Session>>>;
}

/// Cancels the statement currently executing.
pub type CancelHandle = Arc<dyn Fn() -> BoxFuture<'static, DbResult<()>> + Send + Sync>;

pub trait Session: Send + 'static {
    /// Server version obtained when connecting
    fn server_version(&self) -> &str;
    fn ping(&mut self) -> BoxFuture<'_, DbResult<()>>;
    fn cancel_handle(&self) -> Option<CancelHandle>;
    /// The server's ID for this session (MySQL `CONNECTION_ID()`, PostgreSQL `pg_backend_pid()`),
    /// shown in the history log; SQLite has none.
    fn session_id(&self) -> Option<u64>;
    fn close(self: Box<Self>) -> BoxFuture<'static, DbResult<()>>;

    fn relational(&mut self) -> Option<&mut dyn Relational> {
        None
    }
    fn document(&mut self) -> Option<&mut dyn Document> {
        None
    }
    fn key_value(&mut self) -> Option<&mut dyn KeyValue> {
        None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// SQL type name; empty when the protocol does not provide the type
    pub type_name: String,
    /// The table column this result column reads, when the server reports it
    pub origin: Option<ColumnOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnOrigin {
    /// MySQL database, PostgreSQL schema, or SQLite `main` / attached database
    pub schema: String,
    pub table: String,
    pub column: String,
}

/// Result set in text form; `None` means NULL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
    pub affected_rows: Option<u64>,
    /// PostgreSQL NOTICE and WARNING messages received while the statement ran
    pub notices: Vec<String>,
}

/// One entry in an object list (the List and Detail views of the Objects tab).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    pub name: String,
    pub kind: ObjectKind,
    pub comment: Option<String>,
    /// Detail view columns; keys are stable English names (rows, data_length, engine, created, …)
    pub properties: Vec<(String, String)>,
}

/// Elements shown under a table in the navigator, besides its fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableChildren {
    pub indexes: Vec<String>,
    pub foreign_keys: Vec<String>,
    pub triggers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

/// One column of a foreign key and the column it references (F-DATA-012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub column: String,
    /// Referenced schema, or the SQLite schema / MySQL database. Empty if the server omitted it.
    pub schema: String,
    pub table: String,
    pub referenced: String,
}

/// Generates `execute`/`execute_read` for a session with `attempt`, `reconnect`,
/// `remember`, `lost`, `recovery` and `cancellation`. Both network drivers differ
/// only in how they decide a connection is lost, so keeping the recovery policy
/// here prevents the two copies from drifting.
/// Generates `ping` for a network session whose `$probe` future checks liveness.
#[macro_export]
macro_rules! recovering_ping {
    ($self:ident, $probe:expr) => {
        fn ping(&mut $self) -> $crate::BoxFuture<'_, $crate::DbResult<()>> {
            async move {
                let first = $probe.await;
                let result = match first {
                    Err(e) if $self.lost(&e) && $self.recovery.can_reconnect() => {
                        tracing::warn!("metadata session lost ({e}); reconnecting");
                        $self.reconnect().await?;
                        $probe.await
                    }
                    other => other,
                };
                result.map_err(|e| {
                    if $self.lost(&e) {
                        $self.cancellation.clear();
                        $crate::execution::interrupted(e, false)
                    } else {
                        e
                    }
                })
            }
            .boxed()
        }
    };
}

#[macro_export]
macro_rules! recovering_execute {
    () => {
        fn execute<'a>(
            &'a mut self,
            sql: &'a str,
        ) -> $crate::BoxFuture<'a, $crate::DbResult<Vec<$crate::ResultSet>>> {
            async move {
                self.recovery.observe(sql);
                let result = self.attempt(sql).await.map_err(|e| {
                    if self.lost(&e) {
                        self.cancellation.clear();
                        $crate::execution::interrupted(e, true)
                    } else {
                        e
                    }
                });
                if result.is_ok() {
                    self.remember(sql);
                }
                result
            }
            .boxed()
        }

        fn execute_read<'a>(
            &'a mut self,
            sql: &'a str,
        ) -> $crate::BoxFuture<'a, $crate::DbResult<Vec<$crate::ResultSet>>> {
            async move {
                let result = match self.attempt(sql).await {
                    Err(e) if self.lost(&e) && self.recovery.can_reconnect() => {
                        self.reconnect().await?;
                        self.attempt(sql).await
                    }
                    other => other,
                };
                result.map_err(|e| {
                    if self.lost(&e) {
                        self.cancellation.clear();
                        $crate::execution::interrupted(e, false)
                    } else {
                        e
                    }
                })
            }
            .boxed()
        }
    };
}

pub trait Relational: Send {
    fn dialect(&self) -> Arc<dyn Dialect>;
    /// Execute once on the current physical session. Transport failures never replay the SQL.
    fn execute<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, DbResult<Vec<ResultSet>>>;
    /// Application-owned, side-effect-free metadata/probe SQL that does not require session
    /// state. Network drivers may reconnect and retry once, only before arbitrary SQL has
    /// pinned the physical session. Never use this to classify user SQL by its first keyword.
    fn execute_read<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, DbResult<Vec<ResultSet>>> {
        self.execute(sql)
    }
    /// Execute one statement typed by the user (query editor), filling column types and origins
    /// where the plain `execute` cannot (PostgreSQL's simple protocol reports neither).
    fn execute_statement<'a>(
        &'a mut self,
        sql: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>> {
        self.execute(sql)
    }

    // Catalog queries. The SQL lives in each database crate: a real driver writes
    // `naviduck_db::relational_catalog!(metadata);`, a test double
    // `naviduck_db::unsupported_catalog!();`. Both are required, so a new driver cannot compile
    // with the catalog quietly missing.

    fn list_databases(&mut self) -> BoxFuture<'_, DbResult<Vec<String>>>;
    /// Schemas in the database this session is connected to. Dialects without a schema level
    /// return an empty list.
    fn list_schemas(&mut self) -> BoxFuture<'_, DbResult<Vec<String>>>;
    fn object_kinds(&self) -> &'static [ObjectKind] {
        self.dialect().object_kinds()
    }
    fn list_objects<'a>(
        &'a mut self,
        scope: &'a Scope,
        kind: ObjectKind,
    ) -> BoxFuture<'a, DbResult<Vec<ObjectInfo>>>;
    fn object_ddl<'a>(
        &'a mut self,
        scope: &'a Scope,
        kind: ObjectKind,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<String>>;
    fn table_columns<'a>(
        &'a mut self,
        scope: &'a Scope,
        table: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<ColumnInfo>>>;
    fn table_children<'a>(
        &'a mut self,
        scope: &'a Scope,
        table: &'a str,
    ) -> BoxFuture<'a, DbResult<TableChildren>>;
    fn foreign_keys<'a>(
        &'a mut self,
        scope: &'a Scope,
        table: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<ForeignKey>>>;
    /// Tables, views, functions and columns used to complete SQL. `system` includes hidden catalogs.
    fn outline<'a>(
        &'a mut self,
        scope: &'a Scope,
        system: bool,
    ) -> BoxFuture<'a, DbResult<crate::outline::Outline>>;
}

/// Metadata of one collection or view inside a document database (142-mongodb.md §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionInfo {
    pub name: String,
    /// `"collection"`, `"view"`, `"timeseries"`, …
    pub kind: String,
    pub document_count: Option<u64>,
}

/// Filter, sort, projection and pagination for [`Document::find`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindQuery {
    pub filter_json: String,
    pub sort_json: String,
    pub projection_json: String,
    pub skip: u64,
    pub limit: u64,
}

/// One page of documents serialized as Relaxed Extended JSON strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentPage {
    pub documents: Vec<String>,
    pub total: Option<u64>,
}

pub trait Document: Send {
    fn list_databases(&mut self) -> BoxFuture<'_, DbResult<Vec<String>>>;
    fn list_collections<'a>(
        &'a mut self,
        database: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<CollectionInfo>>>;
    /// Collection properties for the Objects detail view and Info pane.
    fn collection_stats<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<(String, String)>>>;
    fn find<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        query: &'a FindQuery,
    ) -> BoxFuture<'a, DbResult<DocumentPage>>;
    /// Inserts one Extended JSON document; returns the assigned `_id` as Extended JSON.
    fn insert_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<String>>;
    /// Replaces the document matching `_id == id_json` (Extended JSON); returns matched count.
    fn replace_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        id_json: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<u64>>;
    /// Deletes every document whose `_id` matches one entry in `ids_json`.
    fn delete_documents<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        ids_json: &'a [String],
    ) -> BoxFuture<'a, DbResult<u64>>;
    fn create_collection<'a>(
        &'a mut self,
        database: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn drop_collection<'a>(
        &'a mut self,
        database: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn rename_collection<'a>(
        &'a mut self,
        database: &'a str,
        old_name: &'a str,
        new_name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn list_indexes<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<String>>>;
    /// Executes one user command (JSON `runCommand` or shell helper) and maps replies to result sets.
    fn execute_command<'a>(
        &'a mut self,
        database: &'a str,
        command: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>>;
}

/// A key's decoded value for the key-value data editor (141-redis.md §4).
/// Text values keep the grid convention: bytes that are not UTF-8 show as `0x…` hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyValueData {
    Str(String),
    List(Vec<String>),
    Hash(Vec<(String, String)>),
    Set(Vec<String>),
    /// Member and score text as the server prints them.
    ZSet(Vec<(String, String)>),
    /// Entry ID plus field pairs.
    Stream(Vec<(String, Vec<(String, String)>)>),
    /// Unknown or module type; carries the type name the server reported.
    Other(String),
}

impl Default for KeyValueData {
    fn default() -> Self {
        Self::Str(String::new())
    }
}

pub trait KeyValue: Send {
    /// Database indexes this server exposes (Redis `SELECT` targets).
    fn databases(&mut self) -> BoxFuture<'_, DbResult<Vec<u32>>>;
    /// Number of keys in one database (`DBSIZE`).
    fn key_count(&mut self, db: u32) -> BoxFuture<'_, DbResult<u64>>;
    /// SCAN one batch of keys; returns the next cursor and the keys.
    fn scan<'a>(
        &'a mut self,
        db: u32,
        pattern: &'a str,
        cursor: u64,
        count: u32,
    ) -> BoxFuture<'a, DbResult<(u64, Vec<String>)>>;
    /// The type of a key: string, list, hash, set, zset, stream, … or "none".
    fn key_type<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<String>>;
    /// Seconds until the key expires; -1 without expiry, -2 when the key is missing.
    fn ttl<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<i64>>;
    /// Reads one page of the key's value, dispatched by type. Strings ignore offset/count.
    fn read<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        offset: u64,
        count: u32,
    ) -> BoxFuture<'a, DbResult<KeyValueData>>;
    fn set_string<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn hash_set_field<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        field: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn hash_delete_field<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        field: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    /// Appends a member to the head (`tail=false`) or tail of a list.
    fn list_push<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        tail: bool,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    /// Removes every occurrence of a member from a list (`LREM`).
    fn list_delete<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn set_add<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn set_remove<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    /// Adds or updates a sorted-set member with the given score text.
    fn zset_add<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
        score: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn zset_remove<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        member: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    /// Creates an empty string key (`SET key ""`).
    fn create_key<'a>(&'a mut self, db: u32, key: &'a str) -> BoxFuture<'a, DbResult<()>>;
    fn delete_keys<'a>(&'a mut self, db: u32, keys: &'a [String]) -> BoxFuture<'a, DbResult<u64>>;
    fn rename_key<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        new_name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    /// Sets the expiry in seconds; `None` removes it (`PERSIST`).
    fn set_ttl<'a>(
        &'a mut self,
        db: u32,
        key: &'a str,
        seconds: Option<i64>,
    ) -> BoxFuture<'a, DbResult<()>>;
    /// Runs one user command (query editor) and maps the reply to result sets.
    fn execute_command<'a>(
        &'a mut self,
        db: u32,
        args: &'a [String],
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>>;
}
