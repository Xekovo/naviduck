//! SQL Server catalog SQL (`sys.*` and `OBJECT_DEFINITION`).

use naviduck_db::driver::{ColumnInfo, DbResult, ObjectInfo, Relational, ResultSet};
use naviduck_db::metadata::{self, cell};
use naviduck_db::outline::{self, Outline, OutlineTable};
use naviduck_model::object::{ObjectKind, Scope};

pub async fn list_databases<R: Relational + ?Sized>(r: &mut R) -> DbResult<Vec<String>> {
    metadata::first_column(
        r,
        "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name",
    )
    .await
}

pub async fn list_schemas<R: Relational + ?Sized>(r: &mut R) -> DbResult<Vec<String>> {
    metadata::first_column(r, "SELECT name FROM sys.schemas ORDER BY name").await
}

pub async fn list_objects<R: Relational + ?Sized>(
    r: &mut R,
    scope: &Scope,
    kind: ObjectKind,
) -> DbResult<Vec<ObjectInfo>> {
    let (sql, props) = object_sql(r.dialect().as_ref(), scope, kind)?;
    let set = metadata::query(r, &sql).await?;
    Ok(metadata::objects(&set.rows, kind, props))
}

fn object_sql(
    d: &dyn naviduck_sql::Dialect,
    scope: &Scope,
    kind: ObjectKind,
) -> DbResult<(String, &'static [&'static str])> {
    let schema = d.string_literal(scope.schema.as_deref().unwrap_or("dbo"));
    match kind {
        ObjectKind::Table => Ok((
            format!(
                "SELECT t.name, SUM(p.rows), SUM(a.total_pages) * 8192, \
                 USER_NAME(t.principal_id), MAX(CAST(ep.value AS nvarchar(4000))) \
                 FROM sys.tables t \
                 JOIN sys.schemas s ON s.schema_id = t.schema_id \
                 LEFT JOIN sys.indexes i ON i.object_id = t.object_id AND i.index_id IN (0, 1) \
                 LEFT JOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id = i.index_id \
                 LEFT JOIN sys.allocation_units a ON a.container_id = p.partition_id \
                 LEFT JOIN sys.extended_properties ep \
                   ON ep.major_id = t.object_id AND ep.minor_id = 0 AND ep.class = 1 AND ep.name = 'MS_Description' \
                 WHERE s.name = {schema} AND t.is_ms_shipped = 0 \
                 GROUP BY t.object_id, t.name, t.principal_id \
                 ORDER BY t.name"
            ),
            &["rows", "data_length", "owner", "comment"],
        )),
        ObjectKind::View => Ok((
            format!(
                "SELECT v.name, USER_NAME(v.principal_id), CAST(ep.value AS nvarchar(4000)) \
                 FROM sys.views v \
                 JOIN sys.schemas s ON s.schema_id = v.schema_id \
                 LEFT JOIN sys.extended_properties ep \
                   ON ep.major_id = v.object_id AND ep.minor_id = 0 AND ep.class = 1 AND ep.name = 'MS_Description' \
                 WHERE s.name = {schema} AND v.is_ms_shipped = 0 \
                 ORDER BY v.name"
            ),
            &["owner", "comment"],
        )),
        ObjectKind::Function => Ok((
            format!(
                "SELECT o.name, \
                 CASE o.type WHEN 'P' THEN 'PROCEDURE' ELSE 'FUNCTION' END, \
                 USER_NAME(o.principal_id), \
                 CAST(ep.value AS nvarchar(4000)) \
                 FROM sys.objects o \
                 JOIN sys.schemas s ON s.schema_id = o.schema_id \
                 LEFT JOIN sys.extended_properties ep \
                   ON ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.class = 1 AND ep.name = 'MS_Description' \
                 WHERE s.name = {schema} AND o.type IN ('FN', 'IF', 'TF', 'P') AND o.is_ms_shipped = 0 \
                 ORDER BY o.name"
            ),
            &["type", "owner", "comment"],
        )),
        ObjectKind::Sequence => Ok((
            format!(
                "SELECT sq.name, TYPE_NAME(sq.user_type_id), \
                 CAST(sq.start_value AS nvarchar(100)), CAST(sq.increment AS nvarchar(100)), \
                 CAST(sq.current_value AS nvarchar(100)) \
                 FROM sys.sequences sq JOIN sys.schemas s ON s.schema_id = sq.schema_id \
                 WHERE s.name = {schema} ORDER BY sq.name"
            ),
            &["type", "start", "increment", "current"],
        )),
        ObjectKind::Trigger => Ok((
            format!(
                "SELECT tr.name, OBJECT_NAME(tr.parent_id), \
                 CASE WHEN tr.is_disabled = 1 THEN 'DISABLED' ELSE 'ENABLED' END \
                 FROM sys.triggers tr \
                 JOIN sys.objects p ON p.object_id = tr.parent_id \
                 JOIN sys.schemas s ON s.schema_id = p.schema_id \
                 WHERE s.name = {schema} AND tr.is_ms_shipped = 0 ORDER BY tr.name"
            ),
            &["table", "status"],
        )),
        other => Err(metadata::unsupported(other)),
    }
}

pub async fn object_ddl<R: Relational + ?Sized>(
    r: &mut R,
    scope: &Scope,
    kind: ObjectKind,
    name: &str,
) -> DbResult<String> {
    let d = r.dialect();
    let target = d.qualified(scope, name);
    let obj_lit = d.string_literal(&target);
    let first = |set: ResultSet| set.rows.first().map(|row| cell(row, 0)).unwrap_or_default();
    match kind {
        ObjectKind::View | ObjectKind::Function | ObjectKind::Trigger => Ok(first(
            metadata::query(
                r,
                &format!("SELECT OBJECT_DEFINITION(OBJECT_ID({obj_lit}))"),
            )
            .await?,
        )),
        ObjectKind::Sequence => {
            let set = metadata::query(
                r,
                &format!(
                    "SELECT TYPE_NAME(user_type_id), CAST(start_value AS nvarchar(100)), \
                     CAST(increment AS nvarchar(100)), CAST(minimum_value AS nvarchar(100)), \
                     CAST(maximum_value AS nvarchar(100)), cache_size, is_cycling \
                     FROM sys.sequences WHERE object_id = OBJECT_ID({obj_lit})"
                ),
            )
            .await?;
            let row = set.rows.first().cloned().unwrap_or_default();
            Ok(format!(
                "CREATE SEQUENCE {target} AS {}\n  START WITH {}\n  INCREMENT BY {}\n  MINVALUE {}\n  MAXVALUE {}{};",
                cell(&row, 0),
                cell(&row, 1),
                cell(&row, 2),
                cell(&row, 3),
                cell(&row, 4),
                if cell(&row, 6) == "1" || cell(&row, 6).eq_ignore_ascii_case("true") {
                    "\n  CYCLE"
                } else {
                    ""
                }
            ))
        }
        ObjectKind::Table => {
            let cols = metadata::query(
                r,
                &format!(
                    "SELECT c.name, \
                     CASE \
                       WHEN tp.name IN ('nvarchar', 'nchar') THEN tp.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS nvarchar(10)) END + ')' \
                       WHEN tp.name IN ('varchar', 'char', 'varbinary', 'binary') THEN tp.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS nvarchar(10)) END + ')' \
                       WHEN tp.name IN ('decimal', 'numeric') THEN tp.name + '(' + CAST(c.precision AS nvarchar(10)) + ', ' + CAST(c.scale AS nvarchar(10)) + ')' \
                       WHEN tp.name IN ('datetime2', 'datetimeoffset', 'time') THEN tp.name + '(' + CAST(c.scale AS nvarchar(10)) + ')' \
                       ELSE tp.name \
                     END, \
                     c.is_nullable, \
                     dc.definition, \
                     CASE WHEN ic.column_id IS NOT NULL THEN 'IDENTITY(' + CAST(CAST(ic.seed_value AS bigint) AS nvarchar(20)) + ',' + CAST(CAST(ic.increment_value AS bigint) AS nvarchar(20)) + ')' END, \
                     cc.definition \
                     FROM sys.columns c \
                     JOIN sys.types tp ON tp.user_type_id = c.user_type_id \
                     LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
                     LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
                     LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                     WHERE c.object_id = OBJECT_ID({obj_lit}) ORDER BY c.column_id"
                ),
            )
            .await?;
            let pk = metadata::query(
                r,
                &format!(
                    "SELECT kc.name, COL_NAME(ic.object_id, ic.column_id) \
                     FROM sys.key_constraints kc \
                     JOIN sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
                     WHERE kc.parent_object_id = OBJECT_ID({obj_lit}) AND kc.type = 'PK' \
                     ORDER BY ic.key_ordinal"
                ),
            )
            .await?;
            let mut lines: Vec<String> = cols
                .rows
                .iter()
                .map(|row| {
                    let computed = cell(row, 5);
                    if !computed.is_empty() {
                        return format!("  {} AS {computed}", d.quote_ident(&cell(row, 0)));
                    }
                    let mut line = format!("  {} {}", d.quote_ident(&cell(row, 0)), cell(row, 1));
                    let identity = cell(row, 4);
                    if !identity.is_empty() {
                        line.push_str(&format!(" {identity}"));
                    }
                    if cell(row, 2) == "0" || cell(row, 2).eq_ignore_ascii_case("false") {
                        line.push_str(" NOT NULL");
                    }
                    let default = cell(row, 3);
                    if !default.is_empty() {
                        line.push_str(&format!(" DEFAULT {default}"));
                    }
                    line
                })
                .collect();
            if let Some(first_row) = pk.rows.first() {
                let pk_name = d.quote_ident(&cell(first_row, 0));
                let pk_cols: Vec<String> = pk
                    .rows
                    .iter()
                    .map(|row| d.quote_ident(&cell(row, 1)))
                    .collect();
                lines.push(format!(
                    "  CONSTRAINT {pk_name} PRIMARY KEY ({})",
                    pk_cols.join(", ")
                ));
            }
            Ok(format!(
                "CREATE TABLE {target} (\n{}\n);",
                lines.join(",\n")
            ))
        }
        other => Err(metadata::unsupported(other)),
    }
}

pub async fn table_columns<R: Relational + ?Sized>(
    r: &mut R,
    scope: &Scope,
    table: &str,
) -> DbResult<Vec<ColumnInfo>> {
    let d = r.dialect();
    let obj_lit = d.string_literal(&d.qualified(scope, table));
    let sql = format!(
        "SELECT c.name, \
         CASE \
           WHEN tp.name IN ('nvarchar', 'nchar') THEN tp.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS nvarchar(10)) END + ')' \
           WHEN tp.name IN ('varchar', 'char', 'varbinary', 'binary') THEN tp.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS nvarchar(10)) END + ')' \
           WHEN tp.name IN ('decimal', 'numeric') THEN tp.name + '(' + CAST(c.precision AS nvarchar(10)) + ', ' + CAST(c.scale AS nvarchar(10)) + ')' \
           WHEN tp.name IN ('datetime2', 'datetimeoffset', 'time') THEN tp.name + '(' + CAST(c.scale AS nvarchar(10)) + ')' \
           ELSE tp.name \
         END, \
         c.is_nullable, \
         CASE WHEN EXISTS ( \
           SELECT 1 FROM sys.indexes i \
           JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
           WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id \
         ) THEN 1 ELSE 0 END, \
         dc.definition, \
         CAST(ep.value AS nvarchar(4000)) \
         FROM sys.columns c \
         JOIN sys.types tp ON tp.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
         LEFT JOIN sys.extended_properties ep \
           ON ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.class = 1 AND ep.name = 'MS_Description' \
         WHERE c.object_id = OBJECT_ID({obj_lit}) \
         ORDER BY c.column_id"
    );
    Ok(metadata::query(r, &sql)
        .await?
        .rows
        .iter()
        .map(|row| {
            let nullable = cell(row, 2);
            let pk = cell(row, 3);
            ColumnInfo {
                name: cell(row, 0),
                type_name: cell(row, 1),
                nullable: nullable == "1" || nullable.eq_ignore_ascii_case("true"),
                primary_key: pk == "1" || pk.eq_ignore_ascii_case("true"),
                default: row.get(4).cloned().flatten(),
                comment: row.get(5).cloned().flatten(),
            }
        })
        .collect())
}

pub async fn table_children<R: Relational + ?Sized>(
    r: &mut R,
    scope: &Scope,
    table: &str,
) -> DbResult<naviduck_db::TableChildren> {
    let d = r.dialect();
    let obj_lit = d.string_literal(&d.qualified(scope, table));
    metadata::children_from(
        r,
        format!(
            "SELECT name FROM sys.indexes WHERE object_id = OBJECT_ID({obj_lit}) AND name IS NOT NULL ORDER BY index_id"
        ),
        format!(
            "SELECT name FROM sys.foreign_keys WHERE parent_object_id = OBJECT_ID({obj_lit}) ORDER BY name"
        ),
        format!(
            "SELECT name FROM sys.triggers WHERE parent_id = OBJECT_ID({obj_lit}) ORDER BY name"
        ),
    )
    .await
}

pub fn foreign_key_sql(dialect: &dyn naviduck_sql::Dialect, scope: &Scope, table: &str) -> String {
    let obj_lit = dialect.string_literal(&dialect.qualified(scope, table));
    format!(
        "SELECT COL_NAME(fkc.parent_object_id, fkc.parent_column_id), \
         SCHEMA_NAME(ro.schema_id), ro.name, \
         COL_NAME(fkc.referenced_object_id, fkc.referenced_column_id) \
         FROM sys.foreign_keys fk \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
         WHERE fk.parent_object_id = OBJECT_ID({obj_lit}) \
         ORDER BY fk.object_id, fkc.constraint_column_id"
    )
}

pub async fn foreign_keys<R: Relational + ?Sized>(
    r: &mut R,
    scope: &Scope,
    table: &str,
) -> DbResult<Vec<naviduck_db::ForeignKey>> {
    let sql = foreign_key_sql(r.dialect().as_ref(), scope, table);
    Ok(metadata::foreign_keys_from_rows(
        &metadata::query(r, &sql).await?.rows,
    ))
}

pub async fn outline<R: Relational + ?Sized>(
    rel: &mut R,
    scope: &Scope,
    system: bool,
) -> DbResult<Outline> {
    let d = rel.dialect();
    let schema = scope.schema.clone().unwrap_or_else(|| "dbo".into());
    let mut wanted = vec![d.string_literal(&schema)];
    if system {
        wanted.extend([
            d.string_literal("sys"),
            d.string_literal("INFORMATION_SCHEMA"),
        ]);
    }
    let list = wanted.join(", ");
    let mut tables: Vec<OutlineTable> = outline::rows(
        rel.execute_read(&format!(
            "SELECT o.name, o.type FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name IN ({list}) AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
             ORDER BY o.name"
        ))
        .await?,
    )
    .into_iter()
    .map(|r| OutlineTable {
        name: r[0].clone(),
        view: r.get(1).is_some_and(|t| t.trim() == "V"),
        columns: Vec::new(),
    })
    .collect();
    let columns = outline::rows(
        rel.execute_read(&format!(
            "SELECT o.name, c.name, TYPE_NAME(c.user_type_id) \
             FROM sys.columns c \
             JOIN sys.objects o ON o.object_id = c.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name IN ({list}) AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
             ORDER BY o.name, c.column_id"
        ))
        .await?,
    );
    outline::group_columns(&mut tables, columns);
    let functions = outline::rows(
        rel.execute_read(&format!(
            "SELECT DISTINCT o.name FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name IN ({list}) AND o.type IN ('FN', 'IF', 'TF', 'P') AND o.is_ms_shipped = 0 \
             ORDER BY o.name"
        ))
        .await?,
    )
    .into_iter()
    .map(|r| r[0].clone())
    .collect();
    let mut schemas = rel.list_schemas().await?;
    if !system {
        schemas.retain(|s| !d.is_system_schema(s));
    }
    Ok(Outline {
        tables,
        functions,
        schemas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_key_sql_uses_sys_foreign_keys() {
        let scope = Scope::new("c", "master", Some("dbo".into()));
        let sql = foreign_key_sql(&crate::MssqlDialect::default(), &scope, "Orders");
        assert!(sql.contains("sys.foreign_keys"));
        assert!(sql.contains("OBJECT_ID('[dbo].[Orders]')"));
    }
}
