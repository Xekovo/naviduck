# 143 Microsoft SQL Server 接入

M14 的第四个数据库（Relational 家族）。新建不依赖 GPUI 的 `naviduck-mssql` crate，实现 `Driver`/`Session`/`Relational`/`Dialect`，在 `naviduck-session::init` 注册。界面零分支：连接表单只读 `DriverInfo` 标志，grid/query/designer/schema 只问能力方法，禁止比较 `Dialect::id()`。专属功能清单见 [db/mssql/features.md](db/mssql/features.md)。

驱动选用纯 Rust TDS 异步驱动 `tiberius`（0.12.3，MIT/Apache-2.0，关闭 `default-features` 后启用 `tds73`、`rustls`、`rust_decimal`、`time`，避免新引入 `chrono` 传递依赖，不引入 GPL）。

## 1. 范围与首版边界

首版实现：

- **连接**：常规（主机、端口 1433、SQL Server 验证用户名/密码、可选初始数据库）、SSL（`tiberius` + `rustls`）、SSH 隧道（`russh`，复用 `naviduck_db::connect` 编排）、高级连接参数（ODBC/ADO.NET 风格键值串，如 `ApplicationIntent=ReadOnly`、`Application Name=…`、`TrustServerCertificate=true`，以及 `initial_queries`、`keepalive`）、URI 新建连接（`mssql://` / `sqlserver://`）。
- **导航树到对象级**：连接 → 数据库 → 模式 → 对象（表、视图、函数/存储过程、序列、触发器）及表下子对象（字段、索引、外键、触发器）；系统数据库（`master`/`tempdb`/`model`/`msdb`）与系统模式（`sys`/`INFORMATION_SCHEMA`/`guest`/`db_*`）随「显示隐藏项目」开关过滤。
- **表查看器与网格编辑**：分批读取（`ORDER BY … OFFSET … FETCH NEXT … ROWS ONLY` / `SELECT TOP …`）、行数统计、单元格编辑、外键选择器（`SELECT DISTINCT TOP …`）、提交流程（`naviduck_db::commit`，使用 T-SQL `BEGIN TRANSACTION` / `SAVE TRANSACTION` / `ROLLBACK TRANSACTION`）。
- **查询编辑器**：多批/多语句执行（`GO` 批分隔符与 `;` 分句）、停止（另开短期连接执行 `KILL <spid>`）、解释（`SET SHOWPLAN_TEXT ON` / `OFF` 文本执行计划）、历史日志（复用 `naviduck-sql-log`）、命令列界面（`sqlcmd` 风格提示符与 `USE [db]` 切库）。
- **表设计与对象设计基础能力**：表设计器建表、加列（`ALTER TABLE … ADD col def`）、删列、改列名（`sp_rename`）与改类型/可空性（`ALTER COLUMN`）、索引创建与删除（`DROP INDEX idx ON schema.table`）、外键/检查约束、`IDENTITY` 参数只读；`DefinitionFamily::Mssql` 覆盖视图/函数/过程/序列定义编辑、在库中查找、依赖关系与只读账号列表（`sys.database_principals` / `sys.server_principals`）。

首版不做（列入偏差与后续遗留）：

- **F-MSSQL-001 扩展验证方式**：Windows 集成验证（`tiberius` `winauth` / `integrated-auth-gssapi`，受平台与 Kerberos/SSPI 环境约束）与 Azure AD Token 验证（`AADToken`）。
- **F-MSSQL-003**：Azure SQL 防火墙规则弹窗与 REST 管理。
- **F-MSSQL-004**：数据库文件组/文件与模式设计器。
- **F-MSSQL-008**：索引重建/重组（`ALTER INDEX … REBUILD/REORGANIZE`）、更新统计信息（`UPDATE STATISTICS`）、`DBCC CHECKDB` 维护面板。
- **F-MSSQL-009**：登录名、服务器角色、数据库用户、数据库角色、应用程序角色完整设计器与权限管理。
- **F-MSSQL-010**：SQL Server 原生 `BACKUP` / `RESTORE` 界面。
- **F-MSSQL-005 / 006 / 007 进阶项**：表存储（文件组/分区方案）、计算列属性表单与已有列默认值约束在线修改、列扩展属性注释（`sp_addextendedproperty`）、同义词/程序集/用户定义类型独立分类节点、可视化/XML 执行计划（`SHOWPLAN_XML`）。

## 2. 连接参数与 DriverInfo

```text
id: "mssql"
name: "SQL Server"
family: Family::Relational
default_port: Some(1433)
icon: "icons/db/mssql.svg"
dialect: Some(Arc::new(MssqlDialect::default()))
file_based: false
multi_host: false
supports_network: true
supports_ssl: true            // tiberius + rustls
chooses_ssl_mode: false       // 复用通用 SSL 开关 + 验证服务器证书 + CA 证书
protocol_timeouts: false
connection_params: true       // F-MSSQL-002：高级页连接参数输入
supports_ssh: true            // 复用 naviduck_db::connect 编排
per_database_sessions: true   // 多数据库 + 多模式，每个数据库按需保持会话（缺省库 master）
database_number: false
initial_database: true        // F-MSSQL-001：常规页显示“初始数据库”输入
```

表单与 `ConnectParams` 映射复用现有标志逻辑，**界面不按产品分支**：

- **常规页**：`supports_network=true`、`multi_host=false`、`initial_database=true` → 显示主机、端口（默认 1433）、初始数据库（留空连 `master`）、用户名（`driver_defaults("mssql").user = "sa"`）、密码与保存密码。
- **高级页**：`connection_params=true` → 显示连接参数输入框，格式为 ADO.NET/ODBC 分号分隔键值对（如 `ApplicationIntent=ReadOnly;Application Name=naviduck;TrustServerCertificate=true`）。
  - 已知参数显式映射到 `tiberius::Config`（`ApplicationIntent=ReadOnly` → `readonly(true)`、`Application Name`/`ApplicationName` → `application_name`、`TrustServerCertificate` → `trust_cert`、`TrustServerCertificateCA` → `trust_cert_ca`、`Encrypt` → `encryption`、`Database`/`Initial Catalog` 在未填常规页数据库时回退使用）；非法键值返回 `naviduck.bad_params`。
  - `ConnectParams.timeout()` 包裹 TCP 连接、TLS 握手与 TDS 登录全过程。
- **SSL 页**：`supports_ssl=true`、`chooses_ssl_mode=false`：
  - `ssl == None` 时设置 `EncryptionLevel::Off`（仅握手登录包加密，登录完成后按明文传输；注意若连接参数显式指定 `Encrypt=DANGER_PLAINTEXT` 且未开 SSL 页，则尊重 `NotSupported`）。
  - `ssl == Some(ssl)` 时设置 `EncryptionLevel::Required`：若 `ssl.verify_server_cert == true` 且提供 `ca_cert` 则调用 `config.trust_cert_ca(ca)`，否则按系统信任根校验证书；若 `ssl.verify_server_cert == false` 则调用 `config.trust_cert()`（跳过证书校验，同时兼容 IP 地址主机名与自签证书）。
  - **SSH 隧道下的 TLS 主机名**：`naviduck_db::connect` 将 `params.host` 改为 `127.0.0.1` 并把原主机写入 `params.tls_host`。驱动在建立 TCP 连接时连 `(params.host, params.port)`，而将 `config.host(params.tls_host.as_deref().unwrap_or(&params.host))` 传给 `tiberius` 用于 SNI 与证书主机名校验。
  - **rustls 限制（偏差 D2）**：`tiberius` 0.12.3 的 `TrustConfig` 仅支持系统根、单 CA 文件（`.pem`/`.crt`/`.der`）和跳过校验，不支持客户端证书双向 TLS（`client_cert`/`client_key`）与 CRL（`crl`）；界面若填写则记 warn 日志。

`Sessions::open` 对 `per_database_sessions=true` 的默认库回退由硬编码 `"postgres"` 改为读取 `driver_defaults(&conn.driver).database`（PostgreSQL 仍为 `"postgres"`，SQL Server 为 `"master"`），零产品名分支。

图标：自绘原创 SVG（`crates/naviduck-ui/assets/icons/db/mssql.svg`，32×32 viewBox 128×128，风格与现有数据库图标一致），不提取 Navicat 或微软商标图标。

## 3. 会话与执行

### 3.1 线程模型

照 `naviduck-postgres`：`tiberius::Client<Compat<TcpStream>>` 为异步客户端，运行在 `naviduck_db::runtime` 的全局 tokio 多线程运行时上。TCP 连接使用 `tokio::net::TcpStream::connect`，设置 `set_nodelay(true)` 并通过 `tokio_util::compat::TokioAsyncWriteCompatExt::compat_write()` 传入 `tiberius::Client::connect`。`MssqlSession` 直接持有 `Client`，`Session`/`Relational` 方法返回 `BoxFuture`（无 `Mutex` 阻塞线程池；仅连接测试或取消使用短生命周期连接）。

### 3.2 Session 方法

| 方法 | 实现 |
| --- | --- |
| `server_version` | 连接时查询 `SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(128)), CAST(SERVERPROPERTY('ProductLevel') AS NVARCHAR(128)), CAST(SERVERPROPERTY('Edition') AS NVARCHAR(128))`，空格拼接缓存（如 `16.0.4155.4 RTM Developer Edition (64-bit)`）。 |
| `ping` | `SELECT 1`，走 `recovering_ping!`。 |
| `session_id` | 连接时查询 `SELECT CAST(@@SPID AS BIGINT)`，缓存为 `u64` 并写入日志 `Tag.session`。 |
| `cancel_handle` | 见 3.4。 |
| `close` | 调用 `tiberius::Client::close().await` 关闭连接。 |
| `relational` | 返回 `Some(self)`。 |

连接成功后立即探测服务器属性构造会话级 `MssqlDialect`（探测失败不阻塞连接，记 warn 日志并回退默认值 `major_version = 16, case_sensitive = false`）：

- `SELECT CAST(SERVERPROPERTY('ProductMajorVersion') AS NVARCHAR(32))` → `major_version: u32`（默认 `16`）。
- `SELECT CAST(SERVERPROPERTY('Collation') AS NVARCHAR(128))` → 若排序规则名包含 `_CS_`、`_BIN` 或 `_BIN2`（忽略大小写），则 `case_sensitive = true`，否则 `false`。

### 3.3 执行语义、错误映射与恢复

1. **查询与写入通道分离**：
   - `tiberius::Client::simple_query(sql)` 发送 TDS `SQLBatch`，保留 `USE [db]` 的跨语句持久化、多结果集以及 `CREATE VIEW/PROCEDURE/FUNCTION/TRIGGER` 批语义，但 `QueryStream` 会消费 `Done` token 而不暴露 `affected_rows`。
   - `tiberius::Client::execute(sql, &[])` 通过 `sp_executesql` 返回 `ExecuteResult::rows_affected()`，供 `naviduck_db::commit::matched_a_row` 校验 `UPDATE`/`DELETE` 命中行数与查询摘要统计。
   - 因此单条顶层关键字为 `INSERT`/`UPDATE`/`DELETE`/`MERGE` 且不含 `OUTPUT` 子句（通过 `naviduck_sql::span` 判断，不另写扫描器）的语句走 `client.execute(sql, &[])` 返回 `affected_rows`；其余语句（`SELECT`、`WITH`、`EXEC`、`USE`、`BEGIN/COMMIT/ROLLBACK`、`SAVE TRANSACTION`、DDL、`SET SHOWPLAN_TEXT`、多语句批）走 `client.simple_query(sql)`，遍历 `QueryItem::Metadata` 与 `QueryItem::Row` 构造 `Vec<ResultSet>`。
   - 列类型与来源：`Metadata` 提供 `Column::name()` 与 `Column::column_type()`（映射为可读 SQL 类型名，供网格右对齐与类型图标）；TDS `COLMETADATA` 不稳定携带表来源元数据（`tiberius` 不暴露 `TABNAME`），因此查询结果的 `ColumnOrigin` 保持 `None`（表查看器自身持有 `Scope` 与表名，可直接编辑）。
2. **错误映射 `DbError { code, message, disconnected, outcome_unknown }`**：
   - `tiberius::Error::Server(e)`：`code = Some(e.code().to_string())`，`message = e.message().to_string()`。`disconnected = true` 仅在连接级/被杀会话错误码触发（`596` 会话处于 KILL 状态、`6005` 关闭中、`10054`/`10053`/`233`/`121` 传输断开、`40197`/`40613` 等严重服务端断连；普通语法/权限/约束错误 `disconnected = false`）。
   - `tiberius::Error::Io { .. }` / `Error::Tls(_)` / `Error::Protocol(_)` / `Error::Routing { .. }`：`disconnected = true`。
   - `outcome_unknown`：由 `naviduck_db::recovering_execute!` 统一设置——普通 SQL 发出后断连永不自动重放。
3. **重连与状态恢复**：
   - 走 `naviduck_db::recovering_execute!` 与 `recovering_ping!`。
   - `remember(sql)` 经 `naviduck_db::sql::session_setup` 识别单条 `USE …` 语句并记住；重连成功后依次执行 `initial_queries` 与记住的 `USE …`。

### 3.4 取消（偏差 D1）

`tiberius` 0.12.3 未暴露协议级 TDS Attention Signal 发送接口（`Client` 拥有独占连接且未拆出取消句柄）。因此照 `140-dameng.md` §3.4：`cancel_handle` 持有连接参数副本与目标 `spid`，取消时另开一个短期连接执行 `KILL <spid>`：

- 目标会话在服务端被终止，正在执行的查询立即报错中断（`disconnected = true`）。
- 恢复宏识别会话已断开并提示事务丢失守卫；短期取消连接用完立即关闭。

## 4. 目录 SQL

八个目录方法经 `naviduck_db::relational_catalog!(metadata)` 转发到 `crates/naviduck-mssql/src/metadata.rs`。因 `per_database_sessions=true`，每个数据库拥有独立会话（或由 `new_session` / `run_rel` 传入 `database` 连入目标库），`sys.*` 目录视图直接读取当前数据库；对象名与模式名用 `dialect.string_literal` 绑定。

| 方法 | SQL 来源与规则 |
| --- | --- |
| `list_databases` | `SELECT name FROM sys.databases ORDER BY name`。 |
| `list_schemas` | `SELECT name FROM sys.schemas ORDER BY name`（导航树根据 `is_system_schema` 过滤系统模式）。 |
| `list_objects(Table)` | `sys.tables t JOIN sys.schemas s` + `sys.dm_db_partition_stats`（或 `sys.partitions` 聚合 `SUM(p.rows)`）估算行数 + `OBJECT_SCHEMA_NAME` / `USER_NAME(t.principal_id)` + `sys.extended_properties`（`class=1 AND minor_id=0 AND name='MS_Description'`）注释；排除 `t.is_ms_shipped = 1`。属性：`rows`、`owner`、`comment`。 |
| `list_objects(View)` | `sys.views v JOIN sys.schemas s` + `sys.extended_properties` 注释，排除 `is_ms_shipped = 1`。 |
| `list_objects(Function)` | `sys.objects o JOIN sys.schemas s` WHERE `o.type IN ('FN', 'IF', 'TF', 'P')` AND `o.is_ms_shipped = 0`，将标量/表值函数映射为 `'FUNCTION'`、存储过程映射为 `'PROCEDURE'`（照 PG/DM 惯例挂载在 `ObjectKind::Function`，`properties` 带 `type`）。 |
| `list_objects(Sequence)` | `sys.sequences seq JOIN sys.schemas s`（`current_value`、`increment`）。 |
| `list_objects(Trigger)` | `sys.triggers tr JOIN sys.tables tb ON tb.object_id = tr.parent_id JOIN sys.schemas s ON s.schema_id = tb.schema_id` WHERE `tr.is_ms_shipped = 0`。 |
| `object_ddl(Table)` | 由 `sys.columns` + `sys.types` + `sys.identity_columns` + `sys.computed_columns` + `sys.default_constraints` + `sys.key_constraints` + `sys.foreign_keys` + `sys.check_constraints` + `sys.indexes` + `sys.index_columns` 合成规范 `CREATE TABLE [schema].[table] (…)` 及非约束 `CREATE [UNIQUE] INDEX … ON [schema].[table] (…)`（结构同 PG `object_ddl(Table)`）。 |
| `object_ddl(View/Function/Trigger)` | `SELECT OBJECT_DEFINITION(OBJECT_ID(N'[schema].[name]'))`（等价 `sys.sql_modules.definition`）；若加密或无源码返回 `naviduck.no_source`。 |
| `object_ddl(Sequence)` | 由 `sys.sequences` 合成 `CREATE SEQUENCE [schema].[name] AS <type> START WITH … INCREMENT BY … MINVALUE … MAXVALUE … [CYCLE|NO CYCLE] [CACHE …]`。 |
| `object_ddl(Index)` | 由 `sys.indexes` + `sys.index_columns` + `sys.columns` + `sys.tables` + `sys.schemas` 合成 `CREATE [UNIQUE] [CLUSTERED\|NONCLUSTERED] INDEX [idx] ON [schema].[table] (…)`。 |
| `table_columns` | `sys.columns c JOIN sys.types ty ON ty.user_type_id = c.user_type_id` + `sys.default_constraints` + 主键判断（`sys.indexes i JOIN sys.index_columns ic WHERE i.is_primary_key = 1`）+ `sys.extended_properties` 列注释。 |
| `table_children` | 索引（`sys.indexes WHERE object_id = … AND type > 0 AND name IS NOT NULL`，排除堆 `type=0`）、外键（`sys.foreign_keys WHERE parent_object_id = …`）、触发器（`sys.triggers WHERE parent_id = …`）。 |
| `foreign_keys` | `sys.foreign_keys fk JOIN sys.foreign_key_columns fkc` 联 `sys.columns` / `sys.tables` / `sys.schemas`，按 `fkc.constraint_column_id` 排序。 |
| `outline` | 当前库 `sys.tables` / `sys.views` + `sys.columns` + `sys.objects`（`FN`/`IF`/`TF`/`P`）+ `list_schemas`（`system=false` 时过滤系统模式）。 |

系统库（`Dialect::is_system_database`）：`master`、`tempdb`、`model`、`msdb`。
系统模式（`Dialect::is_system_schema`）：`sys`、`INFORMATION_SCHEMA`、`guest`、`db_owner`、`db_accessadmin`、`db_securityadmin`、`db_ddladmin`、`db_backupoperator`、`db_datareader`、`db_datawriter`、`db_denydatareader`、`db_denydatawriter`。

## 5. 方言差异

`MssqlDialect { major_version: u32, case_sensitive: bool }`。`Default` = `major_version: 16, case_sensitive: false`。**禁止比较 `Dialect::id()`**，所有差异经 `Lex`、`DialectFeatures` 与能力方法表达。

### 5.1 词法（Lex 扩展）

`naviduck_sql::span::Lex` 新增字段 `pub go_batches: bool`（现有常量 `MYSQL`/`POSTGRES`/`GENERIC`/`SQLITE` 均为 `false`），新增 `Lex::MSSQL`：

| 标志 | 值 | 说明 |
| --- | --- | --- |
| `backtick_idents` | false | T-SQL 不支持反引号。 |
| `hash_comments` | false | 无 `#` 注释。 |
| `dollar_quotes` | false | 无 `$tag$`。 |
| `bracket_idents` | true | `[ident]`，内部 `]]` 转义；`[$name]` 参数标记优先于方括号标识符。 |
| `backslash_strings` | false | 字符串内反斜杠按字面保留（`''` 转义单引号）。 |
| `escape_string_prefix` | false | 无 PG `E''`。 |
| `double_quote_strings` | false | 默认 `QUOTED_IDENTIFIER ON`，`"` 为标识符引号。 |
| `nested_block_comments` | true | T-SQL 支持嵌套 `/* /* */ */`。 |
| `delimiter_command` | false | 无 `DELIMITER`。 |
| `go_batches` | true | **新增**：行首独立 `GO`（可选重复次数如 `GO 5` 与行尾 `--` 注释，不匹配引号/注释内及 `GOTO` 等词）作为 `SpanKind::StatementEnd` 切分批并重置 `block` 嵌套，不被发送到服务端。 |

`N'…'` Unicode 字面量在扫描器中已自然拆为 `Word("N") + String("'…'")`，引号内分号与注释符受保护，格式化不会破坏 `N'…'` 序列。

### 5.2 标识符、字面量与上下文

- `quote_ident`：`format!("[{}]", name.replace(']', "]]"))`；`ident_quote` 返回 `'['`（仅供兜底提示，实际引用统一走 `quote_ident`）。
- `string_literal`：若包含非 ASCII 字符，输出 `N'…'`（单引号翻倍），否则 `'…'`，保证非 UTF-8 排序规则下的中文与多语言数据不丢失。
- `context_selection()`：返回 `ContextSelection::Database`！理由：SQL Server 支持多数据库 + 多模式，但会话运行时切换的是当前数据库（`USE [db]`），**没有** `SET search_path` 或 `SET SCHEMA` 语法。`toolbar` 在 `has_schemas = true` 时仍显示模式选择器供大纲补全与对象归属；`session_context_sql()` 返回 `Some("SELECT DB_NAME()")`；`prepare_database_sql(db)` 返回 `Some(format!("USE {}", self.quote_ident(db)))`；`default_schema()` 返回 `Some("dbo")`。
  - 对应通用修正（按能力驱动，无产品名分支）：
    1. `naviduck-query/src/editor/session.rs` 的默认模式优先取 `dialect.default_schema().unwrap_or("public")`（PG 仍选 `public`，MSSQL 选 `dbo`）。
    2. `naviduck-designer/src/panel/references.rs` 的外键引用目标范围按 `dialect.features().has_schemas` 决定取 `list_schemas()` 还是 `list_databases()`（与 `reference_is_self` 一致）。

### 5.3 DialectFeatures 与能力方法扩展

| 字段 | 值 | 理由 |
| --- | --- | --- |
| `has_schemas` | true | 数据库下有 `dbo` 等模式层。 |
| `folds_unquoted` / `folds_unquoted_upper` | false / false | SQL Server 保留标识符声明大小写（CI/CS 决定比较而非折叠）。 |
| `transactional_ddl` | true | T-SQL DDL 可在显式事务内回滚。 |
| `standalone_indexes` | true | 独立 `CREATE INDEX`。 |
| `drop_index_by_name` | false | SQL Server 删索引必须带 `ON [schema].[table]`。 |
| `drop_trigger_by_name` | true | `DROP TRIGGER [schema].[trg]`。 |
| `separate_comments` | false | 首版不做扩展属性注释读写。 |
| `supports_column_comments` | false | 同上。 |
| `supports_identity` | true | `IDENTITY(seed, increment)`。 |
| `identity_editable` | false | 已有表不能在线修改 IDENTITY 属性；`Definition` 框可创建。 |
| `enriches_from_catalog` | false | `object_ddl(Table)` 直接从目录合成完整建表语句，无需二次富集。 |
| `counts_changed_rows` | false | `UPDATE` 返回匹配行数，兼容 `matched_a_row`。 |
| `like_text_cast` | false | T-SQL `LIKE` 支持数值/日期隐式转换或由 `CAST` 处理（首版 `false`）。 |
| `supports_virtual_generated` | false | 计算列为 `AS (expr) [PERSISTED]`，不走 `VIRTUAL`。 |
| `supports_index_method` | false | 无 `USING BTREE/HASH`。 |
| `supports_index_expression` | false | 索引键为列名（表达式索引靠计算列实现）。 |
| `supports_partitions` | false | 分区方案设计器后续。 |
| `console_boxed_tables` | false | `sqlcmd` 文本表格输出。 |
| `console_errors_have_code` | true | 错误带 SQL Server 数字错误码。 |

**新增能力方法（零产品分支，现有方言保持默认实现）：**

1. **保存点能力方法**（`naviduck_sql::Dialect` + `naviduck_db::commit::apply`）：
   - `savepoint_sql(&self, name: &str) -> String`：默认 `format!("SAVEPOINT {name}")`；MSSQL 覆盖为 `format!("SAVE TRANSACTION {name}")`。
   - `rollback_savepoint_sql(&self, name: &str) -> String`：默认 `format!("ROLLBACK TO SAVEPOINT {name}")`；MSSQL 覆盖为 `format!("ROLLBACK TRANSACTION {name}")`。
   - `release_savepoint_sql(&self, name: &str) -> Option<String>`：默认 `Some(format!("RELEASE SAVEPOINT {name}"))`；MSSQL 覆盖为 `None`（T-SQL 无 `RELEASE SAVEPOINT`）。
2. **`TOP` 限制能力**：
   - `DialectFeatures::top_limit: bool`（默认 `false`，MSSQL `true`）。消费者 `naviduck_db::metadata::referenced_values_sql` 与 `naviduck_sql::security::data_probe` 据此生成 `SELECT [DISTINCT] TOP N …` 而非末尾 `LIMIT N`。
3. **`ExplainStyle::ShowplanText`**：
   - `ExplainStyle` 新增 `ShowplanText`（返回编译计划行，仅展示文本视图，不解析结构化节点）。
   - `naviduck-query/src/explain.rs` 在 `ShowplanText` 下依次执行 `SET SHOWPLAN_TEXT ON` → `<sql>` →（无论成功与否必执行）`SET SHOWPLAN_TEXT OFF`，将返回的 `StmtText` 行拼成 `Plan.text`。
4. **表设计器索引删除钩子**：
   - `Dialect::drop_index_sql(&self, scope: &Scope, table: &str, index: &str) -> String`：默认保持现有 `DROP INDEX [schema.]idx`；MSSQL 覆盖为 `format!("DROP INDEX {} ON {}", self.quote_ident(index), self.qualified(scope, table))`。
   - `Dialect::drop_object_sql(&self, scope: &Scope, kind: ObjectKind, name: &str) -> Option<String>` 默认沿用 `drop_keyword`，供 `naviduck-navigator/src/ops.rs` 删除操作统一调用。

**其余覆盖方法：**

- `begin_transaction()`：`"BEGIN TRANSACTION"`
- `select_page`：`ORDER BY` 存在或 `offset > 0` 时生成 `SELECT … FROM [schema].[t] [WHERE …] ORDER BY <order 或 (SELECT NULL)> OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY`；当 `offset == 0` 且无 `order` 且有 `limit` 时生成 `SELECT TOP {limit} … FROM …`。
- `add_column`：`ALTER TABLE {table} ADD {column} {definition}`（T-SQL 不写 `COLUMN`）。
- `modify_column`：改名用 `EXEC sp_rename 'schema.table.old', 'new', 'COLUMN'`；改类型或可空用 `ALTER TABLE {table} ALTER COLUMN {new_name} {ty}{null}`；涉及 `IDENTITY(` 或修改已有列 `DEFAULT` 约束时返回明确提示错误（引导删改约束或列重建，不静默丢数据）。
- `rename_object`：对 Table/View/Sequence/Function 生成 `EXEC sp_rename {old_lit}, {new_lit}`。
- `duplicate_table`：`SELECT * INTO {to} FROM {from}[ WHERE 1 = 0]`。
- `console_prompt`：`1> ` / `2> `（`sqlcmd` 风格，简化为 `1> ` 与续行 `2> `）。

### 5.4 类型映射

`tiberius::ColumnData` → 网格 `Option<String>`：

| `ColumnData` | 显示文本 | 说明 |
| --- | --- | --- |
| `* (None)` | `None` (NULL) | 任意类型的空值均映射为 `None` |
| `Bit(Some(b))` | `"1"` / `"0"` | 兼容网格布尔与写回 |
| `U8` / `I16` / `I32` / `I64` / `F32` / `F64` | 数值文本 | |
| `Numeric(Some(n))` | `n.to_string()` | 保留小数位（`decimal`/`numeric`/`money`） |
| `Guid(Some(u))` | UUID 连字符小写串 | `uniqueidentifier` |
| `String(Some(s))` | 原文 | `char`/`varchar`/`nchar`/`nvarchar`/`text`/`ntext` |
| `Xml(Some(x))` | `x.as_ref().to_string()` | `xml` |
| `Binary(Some(b))` | `format!("0x{}", hex::encode(b))` | `binary`/`varbinary`/`image`/`timestamp`/`rowversion` |
| `Date(Some(d))` | `YYYY-MM-DD` | 经 `tiberius::time::time::Date` 计算 |
| `Time(Some(t))` | `HH:MM:SS[.fff…]` | 经 `Time` 去尾部无效 0 |
| `SmallDateTime` / `DateTime` / `DateTime2` | `YYYY-MM-DD HH:MM:SS[.fff…]` | 去尾部无效 0 |
| `DateTimeOffset` | `YYYY-MM-DD HH:MM:SS[.fff…] +HH:MM` | 带时区偏移 |

`Column.type_name` 由 `tiberius::ColumnType` 映射为小写的 SQL Server 类型名（`int`、`bigint`、`nvarchar`、`datetime2`、`decimal`、`varbinary`、`uniqueidentifier` 等），对齐 `naviduck_util::sql_type`（同步确认 `uniqueidentifier`、`datetimeoffset`、`sysname` 等归类）。

### 5.5 关键字与内置函数（`words.rs`）

按 SQL Server 2022 T-SQL 保留字与常用内置函数整理（来源：Microsoft SQL Server 2022 Transact-SQL 保留关键字文档与系统函数列表，去掉 `naviduck_sql::keywords::COMMON` 已有项）：

- 关键字：`TOP`、`GO`、`IDENTITY`、`ROWGUIDCOL`、`EXEC`、`EXECUTE`、`PROC`、`OUTPUT`、`MERGE`、`HOLDLOCK`、`NOLOCK`、`UPDLOCK`、`ROWLOCK`、`XLOCK`、`TRAN`、`TRY`、`CATCH`、`THROW`、`RAISERROR`、`PRINT`、`DECLARE`、`NVARCHAR`、`NCHAR`、`DATETIME2`、`DATETIMEOFFSET`、`SMALLDATETIME`、`UNIQUEIDENTIFIER`、`VARBINARY`、`TINYINT`、`BIT`、`MONEY`、`SMALLMONEY`、`SYSNAME`、`CLUSTERED`、`NONCLUSTERED`、`PERSISTED`、`APPLY`、`CROSS`、`PIVOT`、`UNPIVOT`、`OFFSET`、`FETCH`、`NEXT`、`ONLY`、`OVER`、`PARTITION`、`WITHIN` 等。
- 函数：`GETDATE`、`GETUTCDATE`、`SYSDATETIME`、`SYSUTCDATETIME`、`SYSDATETIMEOFFSET`、`DATEADD`、`DATEDIFF`、`DATENAME`、`DATEPART`、`EOMONTH`、`FORMAT`、`ISNULL`、`COALESCE`、`NULLIF`、`IIF`、`CHOOSE`、`CAST`、`CONVERT`、`TRY_CAST`、`TRY_CONVERT`、`TRY_PARSE`、`PARSE`、`NEWID`、`NEWSEQUENTIALID`、`SCOPE_IDENTITY`、`IDENT_CURRENT`、`@@IDENTITY`、`@@ROWCOUNT`、`@@SPID`、`@@VERSION`、`DB_NAME`、`OBJECT_ID`、`OBJECT_NAME`、`OBJECT_SCHEMA_NAME`、`OBJECT_DEFINITION`、`SERVERPROPERTY`、`STRING_AGG`、`STRING_SPLIT`、`JSON_VALUE`、`JSON_QUERY`、`OPENJSON`、`ROW_NUMBER`、`RANK`、`DENSE_RANK`、`LAG`、`LEAD` 等。

### 5.6 DefinitionFamily::Mssql

`naviduck-sql` 的 `DefinitionFamily` 新增 `Mssql` 变体，编译器强制所有 `match` 补齐：

- `definition.rs`：支持 View / Function / Procedure（使用 `CREATE` / `ALTER`，或 SQL Server 2016+ 的 `CREATE OR ALTER`）、Sequence（`CREATE SEQUENCE` / `ALTER SEQUENCE`）。
- `security.rs`：
  - `account_form` → `AccountForm::Unavailable(MSSQL_ACCOUNT_REASON)`（首版不做 login/user 设计器，列出只读主体列表 `SELECT name, type_desc AS kind FROM sys.database_principals WHERE type IN ('S', 'U', 'G', 'R') ORDER BY name`）。
  - `manages_privileges` / `monitors_server` → `false`；`maintenance` → 空。
  - `structure_queries` → 查询 `sys.objects`（`U`/`V`/`FN`/`IF`/`TF`/`P`/`SO`/`TR`）与 `sys.indexes`。
  - `text_columns_sql` → `sys.columns` 联 `sys.types` 过滤文本类型（`char`/`varchar`/`nchar`/`nvarchar`/`text`/`ntext`/`xml`/`sysname`）。
  - `like_cmp` → `LIKE … ESCAPE '\'`（无 `ILIKE`；`regex_sql` 返回不支持正则查找提示）。
  - `relation_plan` → 使用 `sys.sql_expression_dependencies`（`Uses` 与 `UsedBy`）。

## 6. 各 feature 的接入点

| Crate | 改动 |
| --- | --- |
| `naviduck-mssql` | 新建纯 Rust crate（`driver.rs` / `dialect.rs` / `metadata.rs` / `words.rs` / `locales/mssql.yml`），不依赖 GPUI。 |
| `naviduck-sql` | `Lex::go_batches` + `Lex::MSSQL`；`DialectFeatures::top_limit`；`ExplainStyle::ShowplanText`；`savepoint_sql` / `rollback_savepoint_sql` / `release_savepoint_sql` / `drop_index_sql`；`DefinitionFamily::Mssql`。 |
| `naviduck-db` | `commit::apply` 调方言保存点方法；`metadata::referenced_values_sql` 支持 `top_limit`。 |
| `naviduck-util` | `sql_type` 补充 `uniqueidentifier`、`datetimeoffset`、`sysname`、`smallmoney`、`money` 等归类与 `temporal_kind`。 |
| `naviduck-model` | `connection::driver_defaults("mssql")` 返回 `localhost:1433`、user `"sa"`、`database: None`。 |
| `naviduck-app-state` | `uri::DatabaseUri::parse` 支持 `mssql://` / `sqlserver://`。 |
| `naviduck-session` | `Cargo.toml` 引入 `naviduck-mssql`；`init` 注册 `MssqlDriver`（第 8 个内置驱动）；`Sessions::open` 默认库回退改为读 `driver_defaults`（不再硬编码 `"postgres"`）。 |
| `naviduck-headless` | 引入 `naviduck-mssql`，增加无 GPUI 链接与 `DriverInfo`/`DialectFeatures` 单测。 |
| `naviduck-query` | `explain.rs` 实现 `ExplainStyle::ShowplanText`；`editor/session.rs` 默认模式按 `dialect.default_schema()` 选取。 |
| `naviduck-schema` | `table_design/plan.rs` 使用 `d.drop_index_sql`；补齐 MSSQL 表设计单元测试。 |
| `naviduck-designer` | `panel/references.rs` 按 `has_schemas` 读取 schema 列表。 |
| `naviduck-ui` | 新增原创自绘 `assets/icons/db/mssql.svg`，登记 `assets.rs` 的 `OWN`。 |
| `scripts/check-deps.sh` | 将 `naviduck-mssql` 加入无 GPUI 依赖检查名单。 |
| `docker-compose.yml` | 新增 `mssql` 服务（`mcr.microsoft.com/mssql/server:2022-latest`）。 |

## 7. 测试方案

- **单元测试**（全部进 CI，无需容器）：
  - `naviduck-sql`：`Lex::MSSQL` 的 `GO` 批切分（含 `GO 2`、行尾注释、字符串/注释/`GOTO` 内不切分）、`[bracket]]]` 标识符与嵌套 `/* /* */ */` 注释。
  - `naviduck-mssql`：方言引用/字面量/`N'…'`/分页（`TOP` 与 `OFFSET FETCH`）/保存点 SQL/能力字段随 `SERVERPROPERTY` 探测结果变化、ADO.NET 连接参数解析（含 `ApplicationIntent=ReadOnly`、TLS 参数、非法键拒绝）、`ColumnData` → 网格文本映射、TDS 错误码断连判定、`words.rs` 非空与核心词检查。
  - `naviduck-schema` / `naviduck-db`：MSSQL 建表、加改列、删索引、保存点提交、外键引用值 `TOP` SQL 单测。
  - `naviduck-headless`：`MssqlDriver` 无 GPUI 编译与标志断言。
- **实库测试**（`crates/naviduck-mssql/tests/live.rs`，环境变量 `NAVIDUCK_MSSQL_LIVE=1` 开关，不破坏默认离线 `cargo test --workspace`）：
  - 针对 `docker-compose.yml` 中的 `mssql` 服务（端口 `11433`、`sa` / `naviduck!123A`），覆盖：连接/错误密码（18456）/SSL/连接参数（`ApplicationIntent`）、`SERVERPROPERTY` 探测、目录八方法（库/模式/表/视图/函数/过程/序列/触发器/索引/外键/DDL/大纲）、Unicode 中文读写、事务与 `SAVE TRANSACTION` 回滚、`KILL <spid>` 取消查询与断连恢复、分页与网格增删改提交、`GO` 多批执行、`SET SHOWPLAN_TEXT` 解释、设计器建表改列删索引。
- **静态与界面检查**：
  - `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`
  - `scripts/check-deps.sh`、`scripts/check-dialect-branches.sh`、`scripts/check-i18n.sh`、`scripts/check-raw-px.sh`
  - `cargo test --workspace`
  - `cargo test -p naviduck --test snapshot` 并逐张检查 `target/snapshots/` 下包含 SQL Server 图标与连接表单的浅深色截图。

## 8. 偏差与遗留

| 编号 | 偏差 | 理由 | 计划处理 |
| --- | --- | --- | --- |
| D1 | 取消查询使用独立短期连接执行 `KILL <spid>` | `tiberius` 0.12.3 未暴露 TDS Attention 取消包 | 若后续升级或补充 Attention 能力可改为只中断语句；当前由 `recovering_execute` 提示会话中断与事务丢失 |
| D2 | SSL 使用 `rustls`（`tiberius`），不支持客户端证书与 CRL | `tiberius::TrustConfig` 仅支持系统根、单 CA 文件与 `TrustAll` | 若填写客户端证书/CRL 记 warn 日志；后续评估自定义 TLS stream |
| D3 | 首版仅支持 SQL Server 用户名密码验证 | Windows 集成验证（`winauth`/`gssapi`）依赖平台与域环境，Azure AD 依赖 OAuth2 令牌流 | 后续按平台条件编译与令牌获取器补齐（F-MSSQL-001） |
| D4 | 表设计器 `IDENTITY` 参数只读、暂不支持修改已有列默认约束与列级扩展属性注释 | SQL Server 默认约束为独立命名对象，扩展属性需调用 `sp_addextendedproperty` | 后续在表设计器补齐默认约束查询/替换与扩展属性生成 |
| D5 | 查询结果 `ColumnOrigin` 为 `None` | `tiberius` 0.12.3 未从 TDS `COLMETADATA`/`TABNAME` 暴露来源表名 | 表查看器不受影响；查询结果直接编辑依赖 `TABNAME` 解析或元数据回查（后续） |
| D6 | F-MSSQL-003/004/008/009/010 专属设计器与备份工具首版未做 | 控制首版接入范围，对齐 140-dameng 边界 | 各自按功能编号在后续迭代补齐 |
| D7 | 本地无 `reference/help-text/` 目录 | 未在本机导出 Navicat 帮助原文 | 后续导出时再并排核对文案与细节 |

## 9. 验收清单

- [x] 规格 `docs/design/143-mssql.md` 独立提交，结构完整。
- [x] `naviduck-mssql` 无 GPUI 依赖，通过 `scripts/check-deps.sh`。
- [x] `DriverInfo` 与 `driver_defaults("mssql")` 符合第 2 节；连接表单正常显示常规（含初始数据库）/高级（含连接参数）/SSL/SSH 页。
- [x] `Lex::MSSQL` 在 `naviduck-sql` 扫描器中扩展 `GO` 批分隔符与 `[bracket]` 引号，无单独扫描函数。
- [x] `DialectFeatures` 与能力方法实现无任何 `Dialect::id()` 分支，通过 `scripts/check-dialect-branches.sh`。
- [x] 单元测试（方言/词法/参数解析/类型映射/错误码/设计器计划/headless）全部通过。
- [ ] Docker `mssql` 实库测试（`NAVIDUCK_MSSQL_LIVE=1`）通过，测完执行 `docker compose down`。（用例与 compose 服务已就绪，当前机器未启动 Docker 守护进程待补跑）
- [x] 第 5 节全套静态检查、全工作区测试及浅深色截图检查通过。
- [x] `docs/progress.md` 与 `docs/pr/acceptance-mssql.md` 更新完成。
