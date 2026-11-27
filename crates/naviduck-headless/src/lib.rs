//! Headless entry point: `naviduck-model` and `naviduck-db` on their own, with no GPUI dependency.
//!
//! The integration tests in this crate compile and run against those two crates only, so the
//! driver stack cannot quietly grow a window/GPU dependency (A-02). The shell, the navigator and
//! every other pane live in crates that depend on `naviduck-app-state` instead.

pub use naviduck_dameng as dameng;
pub use naviduck_db as db;
pub use naviduck_model as model;
pub use naviduck_mongodb as mongodb;
pub use naviduck_mssql as mssql;
pub use naviduck_redis as redis;

#[cfg(test)]
mod tests {
    /// The SQL Server driver stack compiles and links without GPUI, and its DriverInfo and
    /// dialect flags match the spec (143-mssql.md §2, §4).
    #[test]
    fn mssql_driver_flags_stay_capability_only() {
        use naviduck_db::Driver as _;
        let info = crate::mssql::MssqlDriver::default().info().clone();
        assert_eq!(info.id, "mssql");
        assert_eq!(info.default_port, Some(1433));
        assert!(info.supports_network && info.supports_ssl && info.supports_ssh);
        assert!(info.connection_params && info.initial_database && info.per_database_sessions);
        assert!(!info.file_based && !info.multi_host);
        let dialect = info.dialect.expect("mssql has a SQL dialect");
        assert!(dialect.features().has_schemas);
        assert!(dialect.features().top_limit);
        assert!(dialect.features().supports_identity);
        assert_eq!(dialect.default_schema(), Some("dbo"));
    }
    /// The MongoDB driver stack compiles and links without GPUI, and its DriverInfo
    /// flags match the spec (142-mongodb.md §2): network, SSL, SSH and initial_database,
    /// no dialect at all.
    #[test]
    fn mongodb_driver_flags_stay_capability_only() {
        use naviduck_db::Driver as _;
        use naviduck_db::Family;
        let info = crate::mongodb::MongoDriver::default().info().clone();
        assert_eq!(info.id, "mongodb");
        assert_eq!(info.default_port, Some(27017));
        assert!(info.supports_network && info.supports_ssl && info.supports_ssh);
        assert!(info.initial_database);
        assert!(!info.database_number);
        assert_eq!(info.family, Family::Document);
        assert!(info.dialect.is_none());
        assert!(!info.file_based && !info.multi_host);
    }
    /// The Redis driver stack compiles and links without GPUI, and the DriverInfo
    /// flags match the spec (141-redis.md §2): network, SSL, SSH and the numeric
    /// database index, no dialect at all.
    #[test]
    fn redis_driver_flags_stay_capability_only() {
        use naviduck_db::Driver as _;
        use naviduck_db::Family;
        let info = crate::redis::RedisDriver::default().info().clone();
        assert_eq!(info.id, "redis");
        assert_eq!(info.default_port, Some(6379));
        assert!(info.supports_network && info.supports_ssl && info.supports_ssh);
        assert!(info.database_number);
        assert_eq!(info.family, Family::KeyValue);
        assert!(info.dialect.is_none());
        assert!(!info.file_based && !info.multi_host);
    }

    /// The Dameng driver stack compiles and links without GPUI, and the DriverInfo
    /// flags match the spec (140-dameng.md §2): network and SSH only, no SSL.
    #[test]
    fn dameng_driver_flags_stay_capability_only() {
        use naviduck_db::Driver as _;
        let info = crate::dameng::DamengDriver::default().info().clone();
        assert_eq!(info.id, "dameng");
        assert_eq!(info.default_port, Some(5236));
        assert!(info.supports_network && info.supports_ssh);
        assert!(!info.supports_ssl);
        assert!(!info.file_based && !info.multi_host);
        // The dialect capability set: DM folds unquoted names to upper case and has
        // no transactional DDL.
        let features = info.dialect.as_deref().unwrap().features();
        assert!(features.folds_unquoted_upper);
        assert!(!features.transactional_ddl);
        assert!(!features.identity_editable);
    }
}
