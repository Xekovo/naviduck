//! Microsoft SQL Server (T-SQL) driver and SQL dialect for naviduck.
//!
//! Built on `tiberius` (pure Rust TDS over tokio + native-tls). Shares `naviduck-sql`
//! for lexical scanning (`GO` batch separator, `[bracket]` identifiers, nested block comments),
//! keyword/function metadata, and query definition planning.

rust_i18n::i18n!("locales", fallback = "en");

pub mod dialect;
pub mod driver;
pub mod metadata;
pub mod words;

pub use dialect::MssqlDialect;
pub use driver::MssqlDriver;
