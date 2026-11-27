# Microsoft SQL Server 接入验收记录

分支：`feat/mssql`
规格：[143-mssql.md](../design/143-mssql.md)

## 1. 交付范围总结

- **规格先提交**：`docs/design/143-mssql.md` 单独提交（`733afec`），同步更新 `docs/design/00-overview.md` 与 `docs/design/db/README.md`。
- **驱动 crate `crates/naviduck-mssql`**：
  - 基于纯 Rust 异步 TDS 驱动 `tiberius` 0.13.0（`tds80` + `native-tls`），复用工作区 `native-tls 0.2`，不依赖 GPUI、不引入 GPL 依赖，避免旧版 `rustls 0.21` 漏洞与双 `CryptoProvider` 冲突。
  - `MssqlDriver`（`DriverInfo` 含 `default_port: 1433`、`supports_ssl`、`supports_ssh`、`connection_params`、`initial_database`、`per_database_sessions`）。
  - `ConnectParams` 到 `tiberius::Config` 映射：支持 SSL（`rustls` 系统根 / CA 证书 / 跳过校验，兼容 SSH 隧道 `tls_host` SNI）与 ADO.NET 连接参数（`ApplicationIntent=ReadOnly`、`Application Name`、`Encrypt`、`TrustServerCertificate`），明确拒绝尚未接入的 Windows 集成验证与 Azure AD 令牌配置。
  - `MssqlSession`：连接后探测 `@@VERSION`、`@@SPID`、`SERVERPROPERTY('ProductMajorVersion')` 与 `SERVERPROPERTY('Collation')` 构造会话级 `MssqlDialect`；单条无 `OUTPUT` 的 `INSERT`/`UPDATE`/`DELETE`/`MERGE` 经 `client.execute` 返回 `affected_rows`，其余批次经 `client.simple_query` 迭代 `QueryItem::Metadata`/`QueryItem::Row` 读取结果集与 TDS 类型格式化（含 `numeric`、`uniqueidentifier`、`varbinary`、`date`/`time`/`datetime`/`smalldatetime`/`datetime2`/`datetimeoffset` 等）；`cancel_handle` 经短期连接执行 `KILL <spid>`；`remember`/`replay` 复用 `USE ...` 会话重放与 `recovering_execute!` 恢复。
  - `metadata.rs`：`relational_catalog!` 八方法全部使用 `sys.*` 与 `OBJECT_DEFINITION` 目录视图实现。
- **能力抽象（零 `Dialect::id()` 分支）**：
  - `naviduck-sql`：`Lex::go_batches` 与 `Lex::MSSQL` 在唯一共享扫描器中扩展 `GO` 批分隔行与 `[bracket]` 标识符；`DialectFeatures::top_limit`；`ExplainStyle::ShowplanText`；`savepoint_sql` / `rollback_savepoint_sql` / `release_savepoint_sql` / `drop_index_sql`；`DefinitionFamily::Mssql` 接入视图/函数/过程/序列定义、在库中查找、对象依赖（`sys.sql_expression_dependencies`）及只读主体列表（`sys.database_principals`）。
  - `naviduck-db`：`commit::apply` 改用方言保存点方法（SQL Server 为 `SAVE TRANSACTION` / `ROLLBACK TRANSACTION`，无 `RELEASE`）；`referenced_values_sql` 支持 `SELECT DISTINCT TOP`。
  - `naviduck-session`、`naviduck-headless`、`naviduck-query`、`naviduck-schema`、`naviduck-designer`、`naviduck-util`、`naviduck-model`、`naviduck-app-state`、`naviduck-ui` 完成接线，并绘制原创 SVG 图标 `crates/naviduck-ui/assets/icons/db/mssql.svg`。

## 2. 验证记录

| 验证项 | 命令 / 方式 | 结果 |
| --- | --- | --- |
| 依赖方向检查 | `bash scripts/check-deps.sh` | 通过（`naviduck-mssql` 无 GPUI 依赖） |
| 方言分支检查 | `bash scripts/check-dialect-branches.sh` | 通过（生产代码无 `Dialect::id()` 比较） |
| i18n 检查 | `bash scripts/check-i18n.sh` | 通过（`locales/mssql.yml` en / zh-CN 完整） |
| 裸值检查 | `bash scripts/check-raw-px.sh` | 通过 |
| 格式与 Clippy | `cargo fmt --all --check && cargo clippy --offline --workspace --all-targets -- -D warnings` | 通过（零错误零警告） |
| 工作区全量测试 | `cargo test --offline --workspace` | 通过 |
| 截图渲染与目测 | `cargo test --offline -p naviduck --test snapshot` | 通过；逐张检查 `picker.png`（8 个驱动图标含 SQL Server 原创图标）、`form-mssql.png`（常规 1433 / sa / 初始数据库 / 高级 / SSL / SSH 标签页）及 `main-light.png` / `main-dark.png` |
| 实库测试 | `docker-compose.yml` 新增 `mssql`（`mcr.microsoft.com/mssql/server:2022-latest`，端口 `11433`），用例位于 `crates/naviduck-mssql/tests/live.rs` | 已编译通过；本机 `docker info` 返回未启动守护进程，待有 Docker 环境时运行 `docker compose up -d --wait mssql && NAVIDUCK_MSSQL_LIVE=1 cargo test -p naviduck-mssql --test live && docker compose down` |
