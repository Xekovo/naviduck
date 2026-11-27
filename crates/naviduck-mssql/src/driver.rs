//! Microsoft SQL Server driver (`tiberius` over tokio + rustls).

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use connection_string::AdoNetString;
use futures::future::BoxFuture;
use futures::{FutureExt as _, TryStreamExt as _};
use jiff::Span;
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::{Offset, TimeZone};
use tiberius::error::Error as TdsError;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use naviduck_db::driver::{
    CancelHandle, Column, DbError, DbResult, Driver, DriverInfo, Relational, ResultSet, Session,
};
use naviduck_db::execution::{Cancellation, RecoveryState};
use naviduck_db::log::{Tag, logged};
use naviduck_db::params::ConnectParams;
use naviduck_sql::span::{self, Lex, SpanKind};

use crate::dialect::MssqlDialect;
use crate::metadata;

type TdsClient = Client<Compat<TcpStream>>;

pub struct MssqlDriver {
    info: DriverInfo,
}

impl Default for MssqlDriver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                default_port: naviduck_model::connection::driver_defaults("mssql").port,
                icon: "icons/db/mssql.svg",
                supports_network: true,
                supports_ssl: true,
                supports_ssh: true,
                connection_params: true,
                initial_database: true,
                per_database_sessions: true,
                ..DriverInfo::relational("mssql", "SQL Server", Arc::new(MssqlDialect::default()))
            },
        }
    }
}

fn server_error_disconnected(code: u32, class: u8) -> bool {
    class >= 20
        || matches!(
            code,
            0 | 121 | 233 | 596 | 6005 | 10053 | 10054 | 40197 | 40613
        )
}

fn map_err(e: TdsError) -> DbError {
    match &e {
        TdsError::Server(tok) => {
            let mut out = DbError::coded(tok.code().to_string(), tok.message());
            // Severity >= 20 or fatal transport codes close the server session.
            out.disconnected = server_error_disconnected(tok.code(), tok.class());
            out
        }
        TdsError::Io { .. } => {
            let mut out = DbError::msg(e.to_string());
            out.disconnected = true;
            out
        }
        TdsError::Tls(msg) => DbError::msg(format!("SSL: {msg}")),
        _ => DbError::msg(e.to_string()),
    }
}

fn is_truthy(val: &str) -> bool {
    matches!(
        val.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "sspi" | "1"
    )
}

/// Parses optional ADO.NET connection parameters (`ApplicationIntent=ReadOnly`,
/// `Application Name=...`, `Encrypt=...`, `TrustServerCertificate=...`) while rejecting
/// unsupported Windows Integrated Auth / Azure AD modes cleanly.
fn parse_ado_params(raw: &str) -> DbResult<HashMap<String, String>> {
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let parsed = raw.parse::<AdoNetString>().map_err(|e| {
        DbError::coded(
            "naviduck.bad_params",
            rust_i18n::t!("database.mssql_bad_params", detail = e.to_string()),
        )
    })?;
    let mut map = HashMap::new();
    for (k, v) in parsed.iter() {
        map.insert(k.to_ascii_lowercase(), v.to_string());
    }
    if map.get("integrated security").is_some_and(|v| is_truthy(v))
        || map.get("trusted_connection").is_some_and(|v| is_truthy(v))
        || map
            .get("authentication")
            .is_some_and(|v| !v.eq_ignore_ascii_case("sqlpassword") && !v.trim().is_empty())
    {
        return Err(DbError::msg(rust_i18n::t!(
            "database.mssql_auth_unsupported"
        )));
    }
    Ok(map)
}

pub(crate) fn build_config(params: &ConnectParams) -> DbResult<Config> {
    let ado = parse_ado_params(&params.params)?;
    let mut cfg = Config::new();
    let tls_host = params.tls_host.as_deref().unwrap_or(&params.host);
    cfg.host(tls_host);
    cfg.port(params.port);
    cfg.database(params.database.as_deref().unwrap_or("master"));
    cfg.authentication(AuthMethod::sql_server(
        params.user.clone(),
        params.password.clone().unwrap_or_default(),
    ));

    if let Some(app_name) = ado.get("application name").or_else(|| ado.get("app")) {
        cfg.application_name(app_name);
    }
    if let Some(intent) = ado.get("applicationintent")
        && intent.eq_ignore_ascii_case("readonly")
    {
        cfg.readonly(true);
    }

    if let Some(ssl) = &params.ssl {
        if ssl.client_cert.is_some() || ssl.client_key.is_some() || ssl.crl.is_some() {
            tracing::warn!(
                "SQL Server driver (tiberius/rustls) ignores client certificate and CRL settings"
            );
        }
        cfg.encryption(EncryptionLevel::Required);
        // `tiberius` panics if both `trust_cert` and `trust_cert_ca` are called on one Config.
        if !ssl.verify_server_cert {
            cfg.trust_cert();
        } else if let Some(ca) = &ssl.ca_cert {
            cfg.trust_cert_ca(ca.to_string_lossy().into_owned());
        } else if ado
            .get("trustservercertificate")
            .is_some_and(|v| is_truthy(v))
        {
            cfg.trust_cert();
        }
    } else if let Some(enc) = ado.get("encrypt") {
        let lower = enc.trim().to_ascii_lowercase();
        match lower.as_str() {
            "true" | "yes" | "strict" => {
                cfg.encryption(EncryptionLevel::Required);
                if ado
                    .get("trustservercertificate")
                    .is_some_and(|v| is_truthy(v))
                {
                    cfg.trust_cert();
                } else if let Some(ca) = ado.get("trustservercertificateca") {
                    cfg.trust_cert_ca(ca);
                }
            }
            "danger_plaintext" => cfg.encryption(EncryptionLevel::NotSupported),
            _ => cfg.encryption(EncryptionLevel::Off),
        }
    } else {
        // Explicit plaintext when SSL is disabled in naviduck's SSL tab.
        cfg.encryption(EncryptionLevel::Off);
    }

    Ok(cfg)
}

async fn with_timeout<F, T>(timeout: Duration, action: &str, fut: F) -> DbResult<T>
where
    F: Future<Output = Result<T, TdsError>>,
{
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| DbError::msg(format!("{action} timed out after {}s", timeout.as_secs())))?
        .map_err(map_err)
}

async fn connect_client(params: &ConnectParams) -> DbResult<TdsClient> {
    let cfg = build_config(params)?;
    let addr = (params.host.as_str(), params.port);
    let tcp = tokio::time::timeout(params.timeout(), TcpStream::connect(addr))
        .await
        .map_err(|_| {
            DbError::msg(format!(
                "TCP connect timed out after {}s",
                params.timeout().as_secs()
            ))
        })?
        .map_err(|e| {
            let mut out = DbError::msg(e.to_string());
            out.disconnected = true;
            out
        })?;
    let _ = tcp.set_nodelay(true);
    with_timeout(
        params.timeout(),
        "SQL Server handshake",
        Client::connect(cfg, tcp.compat_write()),
    )
    .await
}

async fn run_simple(client: &mut TdsClient, sql: &str) -> DbResult<Vec<ResultSet>> {
    let mut stream = client.simple_query(sql).await.map_err(map_err)?;
    let mut sets = Vec::new();
    let mut current: Option<ResultSet> = None;

    while let Some(item) = stream.try_next().await.map_err(map_err)? {
        match item {
            QueryItem::Metadata(meta) => {
                if let Some(prev) = current.take() {
                    sets.push(prev);
                }
                let columns = meta
                    .columns()
                    .iter()
                    .map(|c| Column {
                        name: c.name().to_string(),
                        type_name: col_type_name(c.column_type()).to_string(),
                        origin: None,
                    })
                    .collect();
                current = Some(ResultSet {
                    columns,
                    rows: Vec::new(),
                    affected_rows: None,
                    notices: Vec::new(),
                });
            }
            QueryItem::Row(row) => {
                let cells: Vec<Option<String>> = row.into_iter().map(format_column_data).collect();
                current
                    .get_or_insert_with(ResultSet::default)
                    .rows
                    .push(cells);
            }
        }
    }
    if let Some(last) = current {
        sets.push(last);
    }
    Ok(sets)
}

/// True when `sql` is a single DML statement without an `OUTPUT` clause, so running it through
/// `client.execute` yields `affected_rows` (which TDS `SQLBatch` / `simple_query` omits in
/// `tiberius`).
fn should_count_affected_rows(sql: &str) -> bool {
    let spans = span::spans(sql, Lex::MSSQL);
    let mut words = spans
        .iter()
        .filter(|s| s.kind.is_code() && s.kind != SpanKind::StatementEnd);
    let Some(first) = words.next() else {
        return false;
    };
    if first.kind != SpanKind::Word {
        return false;
    }
    let kw = first.text(sql);
    let is_dml = kw.eq_ignore_ascii_case("INSERT")
        || kw.eq_ignore_ascii_case("UPDATE")
        || kw.eq_ignore_ascii_case("DELETE")
        || kw.eq_ignore_ascii_case("MERGE");
    if !is_dml {
        return false;
    }
    // Any StatementEnd before trailing whitespace or a top-level OUTPUT clause routes to
    // simple_query so result sets or multi-statement batches stay intact.
    let mut ends = spans.iter().filter(|s| s.kind == SpanKind::StatementEnd);
    if let Some(end) = ends.next()
        && (!sql[end.range.end..].trim().is_empty() || ends.next().is_some())
    {
        return false;
    }
    !spans.iter().any(|s| {
        s.depth == 0 && s.kind == SpanKind::Word && s.text(sql).eq_ignore_ascii_case("OUTPUT")
    })
}

async fn establish(params: &ConnectParams) -> DbResult<(TdsClient, String, u64, MssqlDialect)> {
    let mut client = connect_client(params).await?;
    if !params.initial_queries.trim().is_empty() {
        run_simple(&mut client, &params.initial_queries).await?;
    }
    let sets = run_simple(
        &mut client,
        "SELECT @@VERSION, CAST(@@SPID AS bigint), \
         CAST(SERVERPROPERTY('ProductMajorVersion') AS nvarchar(20)), \
         CAST(SERVERPROPERTY('Collation') AS nvarchar(128))",
    )
    .await?;
    let row = sets.first().and_then(|s| s.rows.first());
    let raw_version = row
        .and_then(|r| r.first())
        .and_then(|c| c.as_deref())
        .unwrap_or("SQL Server");
    let version = raw_version
        .lines()
        .next()
        .unwrap_or("SQL Server")
        .trim()
        .to_string();
    let spid = row
        .and_then(|r| r.get(1))
        .and_then(|c| c.as_deref())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let major = row
        .and_then(|r| r.get(2))
        .and_then(|c| c.as_deref())
        .unwrap_or("");
    let collation = row
        .and_then(|r| r.get(3))
        .and_then(|c| c.as_deref())
        .unwrap_or("");
    let dialect = MssqlDialect::from_server_properties(major, collation);
    Ok((client, version, spid, dialect))
}

fn cancel_target(params: ConnectParams, spid: u64) -> CancelHandle {
    Arc::new(move || {
        let params = params.clone();
        async move {
            if spid == 0 {
                return Ok(());
            }
            let mut client = connect_client(&params).await?;
            let sql = format!("KILL {spid}");
            run_simple(&mut client, &sql).await?;
            Ok(())
        }
        .boxed()
    })
}

impl Driver for MssqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, DbResult<Box<dyn Session>>> {
        let mut tag = Tag::new(&params.name, &self.info.name);
        async move {
            let (client, version, spid, dialect) = establish(&params).await?;
            tag.session = spid;
            let dialect = Arc::new(dialect);
            let cancellation = Cancellation::default();
            cancellation.replace(cancel_target(params.clone(), spid));
            let recovery = RecoveryState::new(&params.initial_queries, dialect.as_ref());
            Ok(Box::new(MssqlSession {
                client,
                params,
                version,
                tag,
                dialect,
                setup: None,
                recovery,
                cancellation,
            }) as Box<dyn Session>)
        }
        .boxed()
    }
}

pub struct MssqlSession {
    client: TdsClient,
    params: ConnectParams,
    version: String,
    tag: Tag,
    dialect: Arc<MssqlDialect>,
    /// Last successful `USE <db>`; replayed after a transparent reconnect.
    setup: Option<String>,
    recovery: RecoveryState,
    cancellation: Cancellation,
}

impl MssqlSession {
    async fn reconnect(&mut self) -> DbResult<()> {
        self.cancellation.clear();
        let (client, version, spid, dialect) = establish(&self.params).await?;
        self.client = client;
        self.version = version;
        self.tag.session = spid;
        self.dialect = Arc::new(dialect);
        self.replay().await?;
        self.cancellation
            .replace(cancel_target(self.params.clone(), spid));
        Ok(())
    }

    fn remember(&mut self, sql: &str) {
        if let Some(setup) = naviduck_db::sql::session_setup(sql) {
            self.setup = Some(setup);
        }
    }

    async fn replay(&mut self) -> DbResult<()> {
        if let Some(sql) = self.setup.clone() {
            self.attempt(&sql).await?;
        }
        Ok(())
    }

    fn lost(&self, e: &DbError) -> bool {
        e.disconnected
    }

    async fn attempt(&mut self, sql: &str) -> DbResult<Vec<ResultSet>> {
        let client = &mut self.client;
        let tag = &self.tag;
        if should_count_affected_rows(sql) {
            let res = logged(tag, sql, async {
                client.execute(sql, &[]).await.map_err(map_err)
            })
            .await?;
            let count: u64 = res.rows_affected().iter().sum();
            return Ok(vec![ResultSet {
                affected_rows: Some(count),
                ..ResultSet::default()
            }]);
        }
        logged(tag, sql, run_simple(client, sql)).await
    }
}

impl Session for MssqlSession {
    fn server_version(&self) -> &str {
        &self.version
    }

    naviduck_db::recovering_ping!(self, async {
        run_simple(&mut self.client, "SELECT 1").await.map(|_| ())
    });

    fn cancel_handle(&self) -> Option<CancelHandle> {
        Some(self.cancellation.handle())
    }

    fn session_id(&self) -> Option<u64> {
        Some(self.tag.session)
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, DbResult<()>> {
        self.cancellation.clear();
        async move {
            let _ = self.client.close().await;
            Ok(())
        }
        .boxed()
    }

    fn relational(&mut self) -> Option<&mut dyn Relational> {
        Some(self)
    }
}

impl Relational for MssqlSession {
    fn dialect(&self) -> Arc<dyn naviduck_sql::Dialect> {
        self.dialect.clone()
    }

    naviduck_db::recovering_execute!();
    naviduck_db::relational_catalog!(metadata);
}

fn col_type_name(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

pub(crate) fn format_column_data(data: ColumnData<'_>) -> Option<String> {
    match data {
        ColumnData::U8(v) => v.map(|x| x.to_string()),
        ColumnData::I16(v) => v.map(|x| x.to_string()),
        ColumnData::I32(v) => v.map(|x| x.to_string()),
        ColumnData::I64(v) => v.map(|x| x.to_string()),
        ColumnData::F32(v) => v.map(|x| x.to_string()),
        ColumnData::F64(v) => v.map(|x| x.to_string()),
        ColumnData::Bit(v) => v.map(|x| if x { "1" } else { "0" }.to_string()),
        ColumnData::String(v) => v.map(Cow::into_owned),
        ColumnData::Guid(v) => v.map(|g| g.to_string()),
        ColumnData::Binary(v) => v.map(|b| format!("0x{}", hex::encode(b))),
        ColumnData::Numeric(v) => v.map(|n| n.to_string()),
        ColumnData::Xml(v) => v.map(|x| x.into_owned().into_string()),
        ColumnData::Date(v) => v.map(|d| format_date_from_year1(d.days())),
        ColumnData::Time(v) => v.map(|t| format_time(t.increments(), t.scale())),
        ColumnData::DateTime(v) => v.map(|dt| format_datetime(dt.days(), dt.seconds_fragments())),
        ColumnData::SmallDateTime(v) => {
            v.map(|dt| format_smalldatetime(dt.days(), dt.seconds_fragments()))
        }
        ColumnData::DateTime2(v) => v.map(|dt| {
            format_datetime2(dt.date().days(), dt.time().increments(), dt.time().scale())
        }),
        ColumnData::DateTimeOffset(v) => v.map(|dto| {
            format_datetimeoffset(
                dto.datetime2().date().days(),
                dto.datetime2().time().increments(),
                dto.datetime2().time().scale(),
                dto.offset(),
            )
        }),
    }
}

fn date_from_days(start: Date, days: i64) -> Option<Date> {
    start.checked_add(Span::new().days(days)).ok()
}

fn format_date_from_year1(days: u32) -> String {
    date_from_days(Date::constant(1, 1, 1), i64::from(days))
        .map(|d| d.to_string())
        .unwrap_or_default()
}

fn time_from_increments(increments: u64, scale: u8) -> (u64, u64, u64, u64) {
    let scale = scale.min(7);
    let factor = 10u64.pow(u32::from(scale));
    let total_secs = increments / factor;
    let frac = increments % factor;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    (h, m, s, frac)
}

fn format_time(increments: u64, scale: u8) -> String {
    let scale = scale.min(7);
    let (h, m, s, frac) = time_from_increments(increments, scale);
    if scale == 0 || frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!(
            "{h:02}:{m:02}:{s:02}.{frac:0width$}",
            width = usize::from(scale)
        )
    }
}

fn format_datetime(days: i32, seconds_fragments: u32) -> String {
    let date = date_from_days(Date::constant(1900, 1, 1), i64::from(days))
        .map(|d| d.to_string())
        .unwrap_or_default();
    let total_secs = seconds_fragments / 300;
    let subsec_frags = seconds_fragments % 300;
    let ms = (u64::from(subsec_frags) * 1000 + 150) / 300;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if ms == 0 {
        format!("{date} {h:02}:{m:02}:{s:02}")
    } else {
        format!("{date} {h:02}:{m:02}:{s:02}.{ms:03}")
    }
}

fn format_smalldatetime(days: u16, minutes_since_midnight: u16) -> String {
    let date = date_from_days(Date::constant(1900, 1, 1), i64::from(days))
        .map(|d| d.to_string())
        .unwrap_or_default();
    let h = minutes_since_midnight / 60;
    let m = minutes_since_midnight % 60;
    format!("{date} {h:02}:{m:02}:00")
}

fn format_datetime2(days: u32, increments: u64, scale: u8) -> String {
    let date = format_date_from_year1(days);
    let time = format_time(increments, scale);
    format!("{date} {time}")
}

fn format_datetimeoffset(days: u32, increments: u64, scale: u8, offset_minutes: i16) -> String {
    let scale = scale.min(7);
    let (h, m, s, frac) = time_from_increments(increments, scale);
    let Some(date) = date_from_days(Date::constant(1, 1, 1), i64::from(days)) else {
        return String::new();
    };
    let nanos = frac * 10u64.pow(9 - u32::from(scale));
    let Ok(time) = Time::new(h as i8, m as i8, s as i8, nanos as i32) else {
        return String::new();
    };
    let utc = DateTime::from_parts(date, time);
    let Ok(offset) = Offset::from_seconds(i32::from(offset_minutes) * 60) else {
        return String::new();
    };
    let local = utc
        .to_zoned(TimeZone::UTC)
        .map(|z| z.with_time_zone(TimeZone::fixed(offset)));
    let sign = if offset_minutes >= 0 { '+' } else { '-' };
    let abs_mins = offset_minutes.unsigned_abs();
    let oh = abs_mins / 60;
    let om = abs_mins % 60;
    match local {
        Ok(zdt) => {
            let frac_str = if scale == 0 || zdt.subsec_nanosecond() == 0 {
                String::new()
            } else {
                let f = zdt.subsec_nanosecond() as u64 / 10u64.pow(9 - u32::from(scale));
                format!(".{f:0width$}", width = usize::from(scale))
            };
            format!(
                "{} {:02}:{:02}:{:02}{frac_str} {sign}{oh:02}:{om:02}",
                zdt.date(),
                zdt.hour(),
                zdt.minute(),
                zdt.second()
            )
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use naviduck_db::params::SslParams;
    use tiberius::numeric::Numeric;

    #[test]
    fn config_rejects_windows_and_aad_auth() {
        let mut params = ConnectParams {
            host: "db.local".into(),
            port: 1433,
            user: "sa".into(),
            ..ConnectParams::default()
        };
        params.params = "Integrated Security=true".into();
        assert!(build_config(&params).is_err());

        params.params = "Authentication=ActiveDirectoryPassword".into();
        assert!(build_config(&params).is_err());
    }

    #[test]
    fn config_builds_ssl_and_tunnel_without_panicking() {
        let params = ConnectParams {
            host: "127.0.0.1".into(),
            port: 49999,
            user: "sa".into(),
            password: Some("Secret".into()),
            database: Some("AppDb".into()),
            tls_host: Some("sql.example.com".into()),
            params: "ApplicationIntent=ReadOnly;Application Name=naviduck".into(),
            ssl: Some(SslParams {
                verify_server_cert: false,
                ..SslParams::default()
            }),
            ..ConnectParams::default()
        };
        let cfg = build_config(&params).expect("valid config");
        let _ = cfg;
    }

    #[test]
    fn affected_row_routing_detects_plain_dml() {
        assert!(should_count_affected_rows(
            "INSERT INTO [dbo].[t] VALUES (1)"
        ));
        assert!(should_count_affected_rows(
            "UPDATE t SET x = 1 WHERE id = 2;"
        ));
        assert!(should_count_affected_rows("DELETE FROM t WHERE id = 2"));
        assert!(!should_count_affected_rows(
            "INSERT INTO t OUTPUT INSERTED.id VALUES (1)"
        ));
        assert!(!should_count_affected_rows("SELECT * FROM t"));
        assert!(!should_count_affected_rows(
            "UPDATE t SET x = 1; SELECT @@ROWCOUNT"
        ));
    }

    #[test]
    fn column_data_formats_tds_types() {
        assert_eq!(
            format_column_data(ColumnData::I32(Some(42))),
            Some("42".into())
        );
        assert_eq!(format_column_data(ColumnData::I32(None)), None);
        assert_eq!(
            format_column_data(ColumnData::Bit(Some(true))),
            Some("1".into())
        );
        assert_eq!(
            format_column_data(ColumnData::Binary(Some(Cow::Borrowed(&[
                0xde, 0xad, 0xbe, 0xef
            ])))),
            Some("0xdeadbeef".into())
        );
        assert_eq!(
            format_column_data(ColumnData::Numeric(Some(Numeric::new_with_scale(12345, 2)))),
            Some("123.45".into())
        );
        assert_eq!(format_date_from_year1(739_000), "2024-04-25");
        assert_eq!(format_time(36_610_000_000, 7), "01:01:01");
        assert_eq!(format_smalldatetime(0, 90), "1900-01-01 01:30:00");
        assert_eq!(format_datetime(0, 300), "1900-01-01 00:00:01");
        assert_eq!(
            format_datetimeoffset(739_000, 0, 0, 480),
            "2024-04-25 08:00:00 +08:00"
        );
    }

    #[test]
    fn server_error_severity_controls_disconnect_flag() {
        assert!(!server_error_disconnected(2627, 14));
        assert!(!server_error_disconnected(1205, 13));
        assert!(server_error_disconnected(40197, 20));
        assert!(server_error_disconnected(596, 14));
        assert!(server_error_disconnected(233, 11));
        assert!(
            map_err(TdsError::Io {
                kind: std::io::ErrorKind::ConnectionReset,
                message: "reset".into(),
            })
            .disconnected
        );
    }
}
