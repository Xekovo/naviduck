//! Turns `ConnectParams` into fred client configuration (141-redis.md §2).
//! Kept pure so tests can inspect the config without connecting.

use std::time::Duration;

use fred::prelude::{Builder, Config, ServerConfig, TlsConfig, TlsConnector};
use naviduck_db::params::ConnectParams;
use naviduck_db::{DbError, DbResult};

pub(crate) struct RedisConfig {
    pub config: Config,
    /// Connect and command timeouts applied through the builder.
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
}

/// The database index: `general.database` holds the numeric index, "0".."255" (fred stores
/// `Option<u8>`). An unparseable or empty field means 0; numbers beyond 255 are an error
/// instead of silently clamping to another database.
pub(crate) fn database_index(params: &ConnectParams) -> DbResult<u8> {
    match params.database.as_deref().unwrap_or("").trim() {
        "" => Ok(0),
        s => s.parse::<u8>().map_err(|_| {
            DbError::msg(format!(
                "The Redis database index must be a number from 0 to 255, got \"{s}\"."
            ))
        }),
    }
}

pub(crate) fn redis_config(params: &ConnectParams) -> DbResult<RedisConfig> {
    let database = database_index(params)?;
    let mut config = Config::default();
    config.server = ServerConfig::new_centralized(params.host.clone(), params.port);
    if !params.user.is_empty() {
        config.username = Some(params.user.clone());
    }
    config.password = params.password.clone();
    config.database = Some(database);
    // Reconnecting is orchestrated by the driver (it also replays `SELECT`), never by fred.
    config.fail_fast = true;
    if params.ssl.is_some() {
        let connector = TlsConnector::default_rustls()
            .map_err(|e| DbError::msg(format!("TLS setup failed: {e}")))?;
        config.tls = Some(TlsConfig::from(connector));
    }
    Ok(RedisConfig {
        config,
        connect_timeout: params.timeout(),
        command_timeout: params.read_timeout.unwrap_or_else(|| params.timeout()),
    })
}

pub(crate) fn build_client(params: &ConnectParams) -> DbResult<fred::prelude::Client> {
    let cfg = redis_config(params)?;
    let client = Builder::from_config(cfg.config)
        .with_performance_config(|performance| {
            performance.default_command_timeout = cfg.command_timeout;
        })
        .with_connection_config(|connection| {
            connection.connection_timeout = cfg.connect_timeout;
            connection.internal_command_timeout = cfg.connect_timeout;
            connection.max_command_attempts = 1;
        })
        .build()
        .map_err(|e| DbError::msg(e.to_string()))?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fred::prelude::Server;
    use naviduck_model::connection::{Connection, SslMode};

    fn params(conn: &Connection) -> ConnectParams {
        ConnectParams::from_connection(conn, Default::default(), &redis_info())
    }

    fn redis_info() -> naviduck_db::DriverInfo {
        naviduck_db::DriverInfo {
            default_port: Some(6379),
            supports_network: true,
            supports_ssh: true,
            supports_ssl: true,
            ..naviduck_db::DriverInfo::key_value("redis", "Redis")
        }
    }

    #[test]
    fn maps_host_user_and_database_index() {
        let mut conn = Connection::new("redis");
        conn.general.user = "alice".into();
        conn.general.database = Some("3".into());
        let cfg = redis_config(&params(&conn)).unwrap().config;
        let ServerConfig::Centralized { server } = cfg.server else {
            panic!("centralized expected");
        };
        let Server { host, port, .. } = server;
        assert_eq!((host.to_string(), port), ("localhost".to_string(), 6379));
        assert_eq!(cfg.username.as_deref(), Some("alice"));
        assert_eq!(cfg.database, Some(3));
        assert!(cfg.tls.is_none());
    }

    #[test]
    fn an_empty_or_missing_database_means_zero() {
        let mut conn = Connection::new("redis");
        conn.general.database = None;
        assert_eq!(database_index(&params(&conn)).unwrap(), 0);
        conn.general.database = Some("".into());
        assert_eq!(database_index(&params(&conn)).unwrap(), 0);
        conn.general.database = Some(" 9 ".into());
        assert_eq!(database_index(&params(&conn)).unwrap(), 9);
    }

    #[test]
    fn a_database_beyond_255_is_rejected() {
        let mut conn = Connection::new("redis");
        conn.general.database = Some("700".into());
        assert!(database_index(&params(&conn)).is_err());
        conn.general.database = Some("abc".into());
        assert!(database_index(&params(&conn)).is_err());
    }

    #[test]
    fn ssl_switches_the_rustls_connector() {
        let mut conn = Connection::new("redis");
        conn.ssl.enabled = true;
        conn.ssl.mode = SslMode::Require;
        let cfg = redis_config(&params(&conn)).unwrap().config;
        assert!(cfg.tls.is_some());
    }

    #[test]
    fn an_empty_username_is_not_sent() {
        let conn = Connection::new("redis");
        let cfg = redis_config(&params(&conn)).unwrap().config;
        assert_eq!(cfg.username, None);
    }
}
