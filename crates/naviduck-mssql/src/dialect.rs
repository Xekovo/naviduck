//! Microsoft SQL Server dialect (`MssqlDialect`).

use naviduck_model::object::{ObjectKind, Scope};
use naviduck_sql::definition::{DefinitionDraft, DefinitionFamily};
use naviduck_sql::{
    ColumnType, ContextSelection, Dialect, DialectFeatures, ExplainStyle, Lex, TableOptionStyle,
    TypeArgs,
};

#[derive(Clone, Debug)]
pub struct MssqlDialect {
    /// SQL Server major version probed from `SERVERPROPERTY('ProductMajorVersion')` (16 = 2022).
    pub major_version: u32,
    /// Whether the server's default collation is case-sensitive (`_CS_`).
    pub collation_case_sensitive: bool,
}

impl Default for MssqlDialect {
    fn default() -> Self {
        Self {
            major_version: 16,
            collation_case_sensitive: false,
        }
    }
}

impl MssqlDialect {
    /// Parses `SERVERPROPERTY('ProductMajorVersion')` and `SERVERPROPERTY('Collation')`.
    /// Falls back to `(16, false)` when either value is empty or unparseable.
    pub fn from_server_properties(major_version: &str, collation: &str) -> Self {
        let major = major_version
            .trim()
            .split('.')
            .next()
            .and_then(|part| part.parse::<u32>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(16);
        let cs = collation.to_ascii_uppercase().contains("_CS_")
            || collation.to_ascii_uppercase().contains("_BIN");
        Self {
            major_version: major,
            collation_case_sensitive: cs,
        }
    }

    fn is_identity_definition(def: &str) -> bool {
        def.to_ascii_uppercase().contains("IDENTITY")
    }
}

impl Dialect for MssqlDialect {
    fn id(&self) -> &'static str {
        "mssql"
    }

    fn lex(&self) -> Lex {
        Lex::MSSQL
    }

    fn quote_ident(&self, name: &str) -> String {
        format!("[{}]", name.replace(']', "]]"))
    }

    fn string_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn qualify_prefix<'a>(&self, scope: &'a Scope) -> Option<&'a str> {
        scope.schema.as_deref()
    }

    fn object_kinds(&self) -> &'static [ObjectKind] {
        &[
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Function,
            ObjectKind::Sequence,
            ObjectKind::Trigger,
        ]
    }

    fn definition_family(&self) -> Option<DefinitionFamily> {
        Some(DefinitionFamily::Mssql)
    }

    fn definition_statements(
        &self,
        scope: &Scope,
        draft: &DefinitionDraft,
    ) -> Result<Vec<String>, String> {
        naviduck_sql::definition::plan(self, DefinitionFamily::Mssql, scope, draft)
    }

    fn begin_transaction(&self) -> &'static str {
        "BEGIN TRANSACTION"
    }

    fn savepoint_sql(&self, name: &str) -> String {
        format!("SAVE TRANSACTION {}", self.quote_ident(name))
    }

    fn rollback_savepoint_sql(&self, name: &str) -> String {
        format!("ROLLBACK TRANSACTION {}", self.quote_ident(name))
    }

    fn release_savepoint_sql(&self, _name: &str) -> Option<String> {
        None
    }

    fn is_system_database(&self, name: &str) -> bool {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "master" | "tempdb" | "model" | "msdb"
        )
    }

    fn is_system_schema(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        matches!(lower.as_str(), "sys" | "information_schema" | "guest") || lower.starts_with("db_")
    }

    fn extra_keywords(&self) -> &'static [&'static str] {
        crate::words::KEYWORDS
    }

    fn extra_functions(&self) -> &'static [&'static str] {
        crate::words::FUNCTIONS
    }

    fn context_selection(&self) -> ContextSelection {
        ContextSelection::Database
    }

    fn default_schema(&self) -> Option<&'static str> {
        Some("dbo")
    }

    fn session_context_sql(&self) -> Option<&'static str> {
        Some("SELECT DB_NAME(), SCHEMA_NAME(), SCHEMA_NAME()")
    }

    fn console_prompt(&self, database: &str, _superuser: bool, continuation: bool) -> String {
        let sep = if continuation { '-' } else { '>' };
        format!("{database}{sep} ")
    }

    fn rename_object(
        &self,
        scope: &Scope,
        kind: ObjectKind,
        old: &str,
        new: &str,
    ) -> Option<String> {
        match kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence => Some(format!(
                "EXEC sp_rename {}, {}",
                self.string_literal(&self.qualified(scope, old)),
                self.string_literal(new)
            )),
            _ => None,
        }
    }

    fn duplicate_table(
        &self,
        scope: &Scope,
        old: &str,
        new: &str,
        with_data: bool,
        _type: Option<&str>,
    ) -> Option<Vec<String>> {
        let (from, to) = (self.qualified(scope, old), self.qualified(scope, new));
        let filter = if with_data { "" } else { " WHERE 1 = 0" };
        Some(vec![format!("SELECT * INTO {to} FROM {from}{filter}")])
    }

    fn features(&self) -> DialectFeatures {
        DialectFeatures {
            has_schemas: true,
            folds_unquoted: false,
            folds_unquoted_upper: false,
            transactional_ddl: true,
            drop_index_by_name: false,
            drop_trigger_by_name: false,
            console_boxed_tables: true,
            console_errors_have_code: true,
            separate_comments: true,
            enriches_from_catalog: true,
            supports_identity: true,
            identity_editable: false,
            supports_virtual_generated: false,
            supports_index_method: false,
            supports_index_expression: false,
            fk_reference_schema: true,
            supports_partitions: false,
            case_insensitive_like: !self.collation_case_sensitive,
            top_limit: true,
            ..DialectFeatures::default()
        }
    }

    fn table_option_style(&self) -> TableOptionStyle {
        TableOptionStyle::StorageParameters
    }

    fn explain_style(&self) -> ExplainStyle {
        ExplainStyle::ShowplanText
    }

    fn column_types(&self) -> &'static [ColumnType] {
        COLUMN_TYPES
    }

    fn default_column_type(&self) -> &'static str {
        "int"
    }

    fn added_column_type(&self) -> &'static str {
        "nvarchar(200)"
    }

    fn autoincrement_keyword(&self) -> &'static str {
        "IDENTITY(1,1)"
    }

    fn drop_index_sql(&self, scope: &Scope, table: &str, index: &str) -> String {
        format!(
            "DROP INDEX {} ON {}",
            self.quote_ident(index),
            self.qualified(scope, table)
        )
    }

    fn rename_column(&self, scope: &Scope, table: &str, old: &str, new: &str) -> String {
        format!(
            "EXEC sp_rename {}, {}, 'COLUMN'",
            self.string_literal(&format!("{}.{old}", self.qualified(scope, table))),
            self.string_literal(new),
        )
    }

    fn select_page(
        &self,
        scope: &Scope,
        table: &str,
        filter: &str,
        order: &str,
        limit: Option<u64>,
        offset: u64,
        _rowid: bool,
    ) -> String {
        let target = self.qualified(scope, table);
        let where_clause = if filter.trim().is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", filter.trim())
        };
        let order_trim = order.trim();
        let order_clause = if !order_trim.is_empty() {
            format!(" ORDER BY {order_trim}")
        } else {
            String::new()
        };
        let Some(limit) = limit else {
            return format!("SELECT * FROM {target}{where_clause}{order_clause}");
        };
        if self.major_version >= 11 {
            let order_by = if order_clause.is_empty() {
                " ORDER BY (SELECT NULL)".to_string()
            } else {
                order_clause
            };
            format!(
                "SELECT * FROM {target}{where_clause}{order_by} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
            )
        } else {
            let top = offset.saturating_add(limit);
            format!("SELECT TOP {top} * FROM {target}{where_clause}{order_clause}")
        }
    }

    fn table_options_sql(&self, scope: &Scope, name: &str) -> Option<String> {
        let schema = self.string_literal(scope.schema.as_deref().unwrap_or("dbo"));
        let table = self.string_literal(name);
        Some(format!(
            "SELECT USER_NAME(t.principal_id), 'p', '', '', '' \
             FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
             WHERE s.name = {schema} AND t.name = {table}"
        ))
    }

    fn column_metadata_sql(&self, scope: &Scope, name: &str) -> Option<String> {
        let schema = self.string_literal(scope.schema.as_deref().unwrap_or("dbo"));
        let table = self.string_literal(name);
        Some(format!(
            "SELECT c.name, \
             CASE \
               WHEN tp.name IN ('nvarchar', 'nchar') THEN tp.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS nvarchar(10)) END + ')' \
               WHEN tp.name IN ('varchar', 'char', 'varbinary', 'binary') THEN tp.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS nvarchar(10)) END + ')' \
               WHEN tp.name IN ('decimal', 'numeric') THEN tp.name + '(' + CAST(c.precision AS nvarchar(10)) + ', ' + CAST(c.scale AS nvarchar(10)) + ')' \
               WHEN tp.name IN ('datetime2', 'datetimeoffset', 'time') THEN tp.name + '(' + CAST(c.scale AS nvarchar(10)) + ')' \
               ELSE tp.name \
             END AS type_text, \
             CASE WHEN c.is_nullable = 0 THEN 't' ELSE 'f' END, \
             dc.definition, \
             CASE WHEN ic.column_id IS NOT NULL \
                  THEN 'IDENTITY(' + CAST(CAST(ic.seed_value AS bigint) AS nvarchar(20)) + ',' + CAST(CAST(ic.increment_value AS bigint) AS nvarchar(20)) + ')' \
             END, \
             cc.definition, \
             NULL, \
             CASE WHEN c.collation_name IS NOT NULL \
                       AND c.collation_name <> CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS nvarchar(128)) \
                  THEN c.collation_name END, \
             CAST(ep.value AS nvarchar(4000)) \
             FROM sys.columns c \
             JOIN sys.tables t ON t.object_id = c.object_id \
             JOIN sys.schemas s ON s.schema_id = t.schema_id \
             JOIN sys.types tp ON tp.user_type_id = c.user_type_id \
             LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
             LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
             LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
             LEFT JOIN sys.extended_properties ep \
               ON ep.major_id = t.object_id AND ep.minor_id = c.column_id AND ep.class = 1 AND ep.name = 'MS_Description' \
             WHERE s.name = {schema} AND t.name = {table} \
             ORDER BY c.column_id"
        ))
    }

    fn identity_definition(&self, catalog: &str) -> String {
        if catalog.is_empty() {
            String::new()
        } else {
            format!(" {catalog}")
        }
    }

    /// T-SQL uses `ALTER TABLE t ADD col def` without the `COLUMN` keyword.
    fn add_column(&self, table: &str, column: &str, definition: &str, _position: &str) -> String {
        format!("ALTER TABLE {table} ADD {column} {definition}")
    }

    fn modify_column(
        &self,
        table: &str,
        old_name: &str,
        new_name: &str,
        old_definition: &str,
        new_definition: &str,
    ) -> Option<Result<Vec<String>, String>> {
        if Self::is_identity_definition(old_definition)
            || Self::is_identity_definition(new_definition)
        {
            return Some(Err(
                "This column is an IDENTITY column. SQL Server cannot alter IDENTITY parameters in place; \
                 drop and re-add the column manually."
                    .into(),
            ));
        }
        let mut out = Vec::new();
        if old_name != new_name {
            let old_unquoted = unquote_bracket(old_name);
            let new_unquoted = unquote_bracket(new_name);
            let table_unquoted = table.replace(['[', ']'], "");
            out.push(format!(
                "EXEC sp_rename {}, {}, 'COLUMN'",
                self.string_literal(&format!("{table_unquoted}.{old_unquoted}")),
                self.string_literal(new_unquoted),
            ));
        }
        let (ty, null, default) = parse_definition(new_definition);
        let (old_ty, old_null, _) = parse_definition(old_definition);
        if ty != old_ty || null != old_null || old_name == new_name {
            let null_clause = if null.is_empty() { " NULL" } else { null };
            out.push(format!(
                "ALTER TABLE {table} ALTER COLUMN {new_name} {ty}{null_clause}"
            ));
        }
        let _ = default;
        Some(Ok(out))
    }
}

fn unquote_bracket(ident: &str) -> &str {
    ident
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(ident)
}

fn parse_definition(def: &str) -> (&str, &str, Option<&str>) {
    let trimmed = def.trim();
    let upper = trimmed.to_ascii_uppercase();
    let (before_default, default) = match upper.find(" DEFAULT ") {
        Some(pos) => (trimmed[..pos].trim_end(), Some(trimmed[pos + 9..].trim())),
        None => (trimmed, None),
    };
    if let Some(ty) = before_default
        .strip_suffix(" NOT NULL")
        .or_else(|| before_default.strip_suffix(" not null"))
    {
        (ty.trim_end(), " NOT NULL", default)
    } else {
        (before_default, "", default)
    }
}

const COLUMN_TYPES: &[ColumnType] = &[
    ColumnType {
        name: "int",
        synonyms: &["INTEGER", "INT4"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "bigint",
        synonyms: &["INT8"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "smallint",
        synonyms: &["INT2"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "tinyint",
        synonyms: &[],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "bit",
        synonyms: &["BOOLEAN", "BOOL"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "decimal",
        synonyms: &["DEC", "NUMERIC"],
        args: TypeArgs::LengthAndScale,
    },
    ColumnType {
        name: "numeric",
        synonyms: &[],
        args: TypeArgs::LengthAndScale,
    },
    ColumnType {
        name: "money",
        synonyms: &[],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "smallmoney",
        synonyms: &[],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "float",
        synonyms: &["DOUBLE", "FLOAT8"],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "real",
        synonyms: &["FLOAT4"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "nvarchar",
        synonyms: &[],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "varchar",
        synonyms: &["TEXT", "CHARACTER VARYING"],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "nchar",
        synonyms: &[],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "char",
        synonyms: &["CHARACTER"],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "date",
        synonyms: &[],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "time",
        synonyms: &[],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "datetime",
        synonyms: &["TIMESTAMP"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "datetime2",
        synonyms: &[],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "smalldatetime",
        synonyms: &[],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "datetimeoffset",
        synonyms: &["TIMESTAMPTZ"],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "uniqueidentifier",
        synonyms: &["UUID"],
        args: TypeArgs::None,
    },
    ColumnType {
        name: "varbinary",
        synonyms: &["BLOB", "BYTEA"],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "binary",
        synonyms: &[],
        args: TypeArgs::Length,
    },
    ColumnType {
        name: "xml",
        synonyms: &[],
        args: TypeArgs::None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_brackets_and_strings() {
        let d = MssqlDialect::default();
        assert_eq!(d.quote_ident("a]b"), "[a]]b]");
        assert_eq!(d.string_literal("it's\\"), "'it''s\\'");
        assert_eq!(
            d.qualified(&Scope::new("c", "db", Some("dbo".into())), "orders"),
            "[dbo].[orders]"
        );
    }

    #[test]
    fn server_property_parsing_detects_version_and_case_sensitivity() {
        let d = MssqlDialect::from_server_properties("16.0.4155.4", "SQL_Latin1_General_CP1_CS_AS");
        assert_eq!(d.major_version, 16);
        assert!(d.collation_case_sensitive);
        assert!(!d.features().case_insensitive_like);

        let fallback = MssqlDialect::from_server_properties("", "SQL_Latin1_General_CP1_CI_AS");
        assert_eq!(fallback.major_version, 16);
        assert!(!fallback.collation_case_sensitive);
        assert!(fallback.features().case_insensitive_like);
    }

    #[test]
    fn dialect_capabilities_match_tsql() {
        let d = MssqlDialect::default();
        assert_eq!(d.begin_transaction(), "BEGIN TRANSACTION");
        assert_eq!(d.savepoint_sql("sp1"), "SAVE TRANSACTION [sp1]");
        assert_eq!(
            d.rollback_savepoint_sql("sp1"),
            "ROLLBACK TRANSACTION [sp1]"
        );
        assert_eq!(d.release_savepoint_sql("sp1"), None);
        assert_eq!(
            d.drop_index_sql(&Scope::new("c", "db", Some("dbo".into())), "t", "ix_a"),
            "DROP INDEX [ix_a] ON [dbo].[t]"
        );
        assert_eq!(
            d.add_column("[dbo].[t]", "[col]", "nvarchar(200)", ""),
            "ALTER TABLE [dbo].[t] ADD [col] nvarchar(200)"
        );
        let scope = Scope::new("c", "db", Some("dbo".into()));
        assert_eq!(
            d.select_page(&scope, "t", "", "", Some(50), 100, false),
            "SELECT * FROM [dbo].[t] ORDER BY (SELECT NULL) OFFSET 100 ROWS FETCH NEXT 50 ROWS ONLY"
        );
        assert_eq!(
            d.select_page(&scope, "t", "[id] > 1", "[id] ASC", Some(25), 0, false),
            "SELECT * FROM [dbo].[t] WHERE [id] > 1 ORDER BY [id] ASC OFFSET 0 ROWS FETCH NEXT 25 ROWS ONLY"
        );
        assert_eq!(
            d.rename_column(&scope, "t", "old_col", "new_col"),
            "EXEC sp_rename '[dbo].[t].old_col', 'new_col', 'COLUMN'"
        );
        assert!(d.features().top_limit);
        assert_eq!(d.explain_style(), ExplainStyle::ShowplanText);
        assert_eq!(d.context_selection(), ContextSelection::Database);
        assert_eq!(d.default_schema(), Some("dbo"));
        assert!(d.is_system_database("master"));
        assert!(!d.is_system_database("AppDb"));
        assert!(d.is_system_schema("INFORMATION_SCHEMA"));
        assert!(d.is_system_schema("db_owner"));
        assert!(!d.is_system_schema("dbo"));
    }
}
