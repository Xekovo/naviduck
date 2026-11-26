# 142 MongoDB 接入

M14 的第三个数据库，也是文档模型（`Family::Document`）首例。新建不依赖 GPUI 的 `naviduck-mongodb` crate，实现 `Driver`/`Session`/`Document`，在 `naviduck-session::init` 注册；同时新建 GPUI feature crate `naviduck-doc`，提供集合文档查看/编辑器与命令列界面。专属功能清单见 [db/mongodb/features.md](db/mongodb/features.md)（F-MONGO-001~015），首版覆盖连接、数据库与集合导航、数据编辑器（网格/树/JSON 三视图及文档增删改）、命令执行与查询控制台；其余见第 6 节偏差。

驱动：**mongodb 3.9**（Apache-2.0，官方异步驱动，搭配 `bson 2.15`，启用 `compat-3-0-0` + `rustls-tls` + `dns-resolver`）。TLS 选用 `rustls-tls`（内部使用 `ring` crypto provider），与工作区 `mysql_async`、`fred` 对齐，避免同时拉入 aws-lc-rs 引发运行时 CryptoProvider 冲突。异步 API 直接跑在 `naviduck_db::runtime` 的 tokio 运行时上。

## 1. 范围与首版边界

首版实现：

- 连接：独立连接（主机/端口/用户名/密码/认证或初始数据库）、`mongodb://` URI 新建、SSL（rustls，支持 CA 证书、客户端证书密钥 PEM、跳过证书校验）、SSH 隧道（复用 `naviduck_db::connect` 编排，隧道下自动启用 `directConnection=true`）。副本集多节点/SRV/SCRAM 选项页后续（偏差 D1）。
- 导航树到集合级：连接 → 数据库（按“显示隐藏项目”过滤 `admin`/`local`/`config`）→ 集合（支持新建集合、重命名集合、删除集合、⌘R 刷新、对象选项卡列表/详情展示与统计属性）。
- 集合数据编辑器（`naviduck-doc`）：网格视图、树视图、JSON 视图三种视图切换（F-MONGO-007）；Filter / Sort JSON 筛选与排序；Skip/Limit 分页浏览；新增文档、编辑整条文档 JSON 并替换保存、删除文档。
- 命令列界面（`naviduck-doc`）：支持数据库切换、JSON `runCommand` 及常用 Shell 风格语句（`show dbs`、`show collections`、`use <db>`、`db.runCommand({...})`、`db.<coll>.find(...)`、`findOne(...)`、`countDocuments(...)`、`insertOne(...)`、`deleteOne(...)`、`deleteMany(...)`、`stats()`），结果格式化为 `ResultSet` 文本表输出，并记入历史日志。

首版不做（后续任务）：副本集/分片/SRV 连接模式与高级重试页（F-MONGO-001/002）、集合校验规则/时间序列/索引设计器（F-MONGO-004/005）、JS 函数/MapReduce/GridFS（F-MONGO-006）、查询创建工具与聚合管道 `.nagg`（F-MONGO-009/010）、维护/用户角色/MongoDump-Import 工具/命令监控/备份（F-MONGO-011~015）。

## 2. 连接参数与 DriverInfo

```text
id: "mongodb"
name: "MongoDB"
family: Document
default_port: 27017
file_based: false
multi_host: false
supports_network: true
supports_ssl: true
supports_ssh: true
per_database_sessions: false
initial_database: true
```

`DriverInfo` 新增能力方法 `DriverInfo::document(id, name)`（与 `DriverInfo::key_value` 对称，`dialect: None`），以及字段 **`initial_database: bool`**（基线 false）：连接表单常规页对 `multi_host || database_number || initial_database` 显示「初始数据库」输入（MongoDB 语义为默认库及认证源 `authSource`）。

`driver_defaults("mongodb")`：host `localhost`、port `27017`、user 空、database `None`。

URI 支持：`DatabaseUri::parse` 新增 `mongodb://` scheme 解析（提取 user/password/host/port/database），`conn.uri_hint` 更新提示。

配置映射：
- host/port → 单节点 `ServerAddress::Tcp`；若经 SSH 隧道连接（`params.tls_host.is_some()`）则设置 `direct_connection = Some(true)`，防止副本集发现改回内网地址。
- 凭据：`user` 非空时构造 `Credential`（`username` + `password`、`source` 取 `params.database` 或回退 `"admin"`）。
- SSL：`params.ssl` 存在时构造 `TlsOptions`：`ca_file_path` 映射 `ca_cert`；`cert_key_file_path` 映射 `client_key` 或 `client_cert`；`verify_server_cert == false` 时设置 `allow_invalid_certificates = Some(true)`。
- 连接与命令超时：映射 `connect_timeout`、`server_selection_timeout`（使用 `params.timeout()`）。

图标：为 naviduck 原创绘制抽象 SVG 图标 `icons/db/mongodb.svg`（叶片/文档胶囊几何图形，不提取或照抄 Navicat 或第三方商标位图），在 `naviduck-ui/src/assets.rs` 的 `OWN` 中登记。

## 3. 会话与执行

### 3.1 线程模型

`mongodb::Client` 为异步客户端，`Driver::connect` 与各命令直接在 `naviduck_db::runtime` 上 await，不引入 `spawn_blocking`。一个 `MongoSession` 持有一个 `mongodb::Client`。

### 3.2 Session 方法

| 方法 | 实现 |
| --- | --- |
| `server_version` | `admin` 库执行 `{ "buildInfo": 1 }` 取 `version` 字段，格式 `MongoDB 8.0.x`。连接时探测一次缓存，探测失败回退 `"MongoDB"` 且不阻塞连接。 |
| `ping` | `admin` 库执行 `{ "ping": 1 }`，断连时重连并重试一次。 |
| `session_id` | `None`（MongoDB 驱动连接池无单一数值会话 ID）。 |
| `cancel_handle` | `None`（偏差 D6）。 |
| `close` | `Client::shutdown` 优雅关闭。 |
| `document` | 返回自身。 |

### 3.3 执行语义与恢复

- 读操作（`list_databases`、`list_collections`、`collection_stats`、`find`、`list_indexes`）：断连时重建 `mongodb::Client` 并重试一次（`retrying_read`）。
- 写操作与任意命令（`insert_document`、`replace_document`、`delete_documents`、`create_collection`、`drop_collection`、`rename_collection`、`execute_command`）：**不重放**，断连报 `outcome_unknown = true`。
- 日志：所有服务端命令均经 `naviduck_db::log::logged` 记录，自动进入历史日志视图与 `QueryExec.log`。
- 错误映射：提取 MongoDB `CommandError` 的 `code`（数值转字符串）与 `message`；连接超时/IO/PoolCleared/Shutdown 等标记 `disconnected = true`。

## 4. Document 能力层

`naviduck_db::driver::Document` 扩展如下（全部默认不实现，编译器强制驱动与测试替身补齐）：

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionInfo {
    pub name: String,
    /// "collection", "view", "timeseries", …
    pub kind: String,
    pub document_count: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindQuery {
    pub filter_json: String,
    pub sort_json: String,
    pub projection_json: String,
    pub skip: u64,
    pub limit: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentPage {
    /// Relaxed Extended JSON strings, one per document.
    pub documents: Vec<String>,
    pub total: Option<u64>,
}

pub trait Document: Send {
    fn list_databases(&mut self) -> BoxFuture<'_, DbResult<Vec<String>>>;
    fn list_collections<'a>(&'a mut self, database: &'a str)
        -> BoxFuture<'a, DbResult<Vec<CollectionInfo>>>;
    fn collection_stats<'a>(&'a mut self, database: &'a str, collection: &'a str)
        -> BoxFuture<'a, DbResult<Vec<(String, String)>>>;
    fn find<'a>(&'a mut self, database: &'a str, collection: &'a str, query: &'a FindQuery)
        -> BoxFuture<'a, DbResult<DocumentPage>>;
    fn insert_document<'a>(&'a mut self, database: &'a str, collection: &'a str, doc_json: &'a str)
        -> BoxFuture<'a, DbResult<String>>;
    fn replace_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        id_json: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<u64>>;
    fn delete_documents<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        ids_json: &'a [String],
    ) -> BoxFuture<'a, DbResult<u64>>;
    fn create_collection<'a>(&'a mut self, database: &'a str, name: &'a str)
        -> BoxFuture<'a, DbResult<()>>;
    fn drop_collection<'a>(&'a mut self, database: &'a str, name: &'a str)
        -> BoxFuture<'a, DbResult<()>>;
    fn rename_collection<'a>(
        &'a mut self,
        database: &'a str,
        old_name: &'a str,
        new_name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>>;
    fn list_indexes<'a>(&'a mut self, database: &'a str, collection: &'a str)
        -> BoxFuture<'a, DbResult<Vec<String>>>;
    fn execute_command<'a>(&'a mut self, database: &'a str, command: &'a str)
        -> BoxFuture<'a, DbResult<Vec<ResultSet>>>;
}
```

### 4.1 文档序列化约定

- `find` 返回的每篇文档使用 BSON Relaxed Extended JSON（`bson::Bson::into_relaxed_extjson()`）序列化为格式化 JSON 字符串，保留 `_id` 的 Extended JSON 表示（如 `{"$oid":"..."}` 或标量），以便 `replace_document` 和 `delete_documents` 用同一 `id_json` 解析回 `Bson` 并精确匹配 `{ "_id": <bson> }`。
- 插入和替换接受任意合法 JSON / Extended JSON 对象；替换时自动保持原 `_id` 不可变。

### 4.2 execute_command 的命令解析与结果映射

`execute_command(database, command)` 支持以下形式：
1. `show dbs` / `show databases` → 调用 `listDatabases`，输出数据库名与大小表格。
2. `show collections` / `show tables` → 调用 `listCollections`，输出集合名与类型表格。
3. `db.runCommand(<json>)` 或直接传入 JSON 对象 `{ ... }` → 作为 BSON Document 发送至 `db.run_command`。
4. `db.<collection>.find(<filter?>)` / `findOne(<filter?>)` / `countDocuments(<filter?>)` / `insertOne(<doc>)` / `deleteOne(<filter>)` / `deleteMany(<filter>)` / `stats()` → 解析参数并执行对应集合操作，转换为 `ResultSet`。

## 5. 界面

### 5.1 导航树与对象选项卡（`naviduck-navigator`）

- 新增 `is_document(connection, cx)` 判定（`info().family == Family::Document`）。
- `Selection::Collection(Scope, String)` 与 `Node::Collection(Scope, String)`（id 前缀 `n:`）。
- `DocCatalog`（`docspace.rs`）：按 `(connection, database)` 缓存 `Vec<CollectionInfo>`，随连接关闭/重开失效；`Refresh`（⌘R）重拉集合列表。
- 系统库过滤：`show_system_objects == false` 时隐藏 `admin`、`local`、`config`。
- 导航树交互：双击或回车打开集合（派发 `OpenCollection`）；右键集合提供打开集合、新建集合、重命名集合（原地输入）、删除集合（确认框）、复制名称；右键数据库提供打开/关闭数据库、新建集合、命令列界面（`DocConsole`）、刷新；右键连接提供命令列界面（`DocConsole`）。
- 对象选项卡：选中 Document 连接列出数据库；选中 Document 数据库列出集合及类型/文档数/大小信息，双击直接打开集合编辑器。
- 信息窗格：常规页展示连接、数据库、集合名称与类型，支持 URI 共享。

### 5.2 集合数据查看与编辑器（`naviduck-doc`）

GPUI feature crate，遵守依赖方向规则（仅依赖 `naviduck-session`、`naviduck-ui`、`naviduck-model`、`naviduck-app-state`、`naviduck-db`，不依赖其他 feature crate）。

- 选项卡接入 `naviduck_ui::tabs`（`on_added_to`/`set_active`/`on_removed`、`tabs::title` 显示连接颜色与标题，弹出新窗口不关闭会话）。
- 工具栏：
  - 视图切换按钮：**网格视图**（Table）、**树视图**（Tree）、**JSON 视图**（JSON）（F-MONGO-007）。
  - Filter JSON 输入框、Sort JSON 输入框、执行筛选按钮、刷新按钮。
  - 分页控制：上一页 / 下一页 / 每页条数（默认 50）与范围/总数提示。
  - 操作按钮：新建文档（`+`）、编辑选中文档、删除选中文档（确认提示）。
- 三种视图：
  - **网格视图**：提取本页文档顶层字段（`_id` 优先置于首列），渲染表头与值单元格；嵌套对象/数组以摘要呈现，单行截断带悬停提示；点击选中文档，双击打开文档 JSON 编辑器。
  - **树视图**：每篇文档可展开/折叠，逐层显示键名、类型标记与值（字符串绿、数字蓝、布尔橙、ObjectId/日期紫、null 灰等语义区分）。
  - **JSON 视图**：逐条展示格式化后的文档 JSON，选中高亮，支持双击进入编辑。
- 文档 JSON 编辑器弹窗/面板：使用多行文本编辑器，校验 JSON 合法性后调用 `insert_document` 或 `replace_document`，提交成功刷新当前页。

### 5.3 命令列界面（`naviduck-doc`）

- 入口：MongoDB 连接或数据库右键「命令列界面」（`DocConsole { connection, database }`）。
- 顶部或底栏可切换目标数据库（支持 `use <db>` 同步更新当前数据库），输入命令按回车执行，输出区按命令行与结果表格/JSON 逐行显示，错误以 `p.danger` 标红。

### 5.4 i18n 与文案

`naviduck-mongodb/locales/mongodb.yml` 与 `naviduck-doc/locales/doc.yml`，`en` 与 `zh-CN` 同步，动态 key 登记在 `scripts/i18n-dynamic-keys.txt`。

## 6. 偏差

| 编号 | 偏差 | 理由 | 计划处理 |
| --- | --- | --- | --- |
| D1 | 首版仅支持单节点连接（含 SSH 隧道与 `mongodb://` URI）；副本集多节点、SRV、X.509/GSSAPI/AWS 认证页未做 | 控制首版范围；驱动已启用 `dns-resolver`，可后续扩展 | M14 后续任务 |
| D2 | 不含集合设计器（校验规则/时间序列）、索引设计器、JS 函数、MapReduce、GridFS | F-MONGO-004~006 完整设计器工作量独立 | 各自独立任务 |
| D3 | 不含查询创建工具（Find/Agg Builder）与聚合管道 `.nagg` 设计器 | F-MONGO-009/010 属于专用可视化构建器 | M14 后续任务 |
| D4 | 不含维护、MongoDB 用户/角色、Dump/Restore/Import/Export 工具、命令监控、备份 | F-MONGO-011~015 各自独立功能 | 各自独立任务 |
| D5 | 系统库固定过滤 `admin`、`local`、`config` | MongoDB 通用约定 | 无 |
| D6 | 取消句柄 `cancel_handle = None` | 首版为交互分页与短命令 | 后续接长查询/命令监控时补充 |
| D7 | 命令列支持 JSON `runCommand` 与常用 `db.<coll>.*` / `show` 语句，不内嵌完整 JavaScript 引擎 | 避免引入 JS 运行时复杂性与安全边界；覆盖日常管理查询 | 按需求逐步扩充 shell 解析器 |

## 7. 测试与验收清单

### 7.1 测试

- 单元测试（进 CI）：`naviduck-mongodb` 的配置组装（host/port/authSource/TLS/SSH directConnection）、BSON ↔ Extended JSON 往返与类型映射、错误码提取、Shell 命令解析与 `ResultSet` 映射；`naviduck-app-state` 的 `mongodb://` URI 解析；`naviduck-model` 的 `driver_defaults("mongodb")`。
- 实库测试：`docker-compose.yml` 增加 `mongo` 服务（镜像 `mongo:8`，端口 `17017:27017`，开启密码认证），环境变量 `NAVIDUCK_MONGO_LIVE=1`。覆盖：连接/错误密码/版本探测、建库建集、文档增删改查往返（ObjectId/嵌套文档/数组/日期）、Filter/Sort/Skip/Limit、重命名与删集合、索引列举、`execute_command`（`ping`、`show dbs`、`show collections`、`db.<coll>.find`）。
- `naviduck-headless` 增加 mongodb 编译与能力标志回归（证明驱动栈不链接 GPUI）；`scripts/check-deps.sh` 名单加入 `naviduck-mongodb`。
- 离线 UI 测试与截图：`naviduck-test-support::doc` 提供内存 `FakeDocDriver`（`mongo-fake`）；`naviduck-doc` headless 测试覆盖集合加载、三视图切换、文档增删改与命令面板执行；`crates/naviduck/tests/snapshot.rs` 生成导航树、集合编辑器（网格/树/JSON）、命令面板浅深色截图并逐张目测。

### 7.2 验收清单

- [x] 连接表单：常规页显示初始数据库输入；SSL/SSH 页正常；`mongodb://` URI 解析正常；错误密码处理（驱动错误映射与离线校验；`NAVIDUCK_MONGO_LIVE=1` 实库复测待启动 Docker 守护进程）。
- [x] 连接打开：导航树列出数据库（隐藏系统库开关生效）与集合；支持新建/重命名/删除集合与 ⌘R 刷新；对象选项卡展示集合与属性（`FakeDocDriver` 截图与 `naviduck-navigator` 单测通过；实库待测）。
- [x] 集合数据编辑器：网格/树/JSON 三视图切换正常；Filter/Sort/分页工作；文档新增/编辑保存/删除往返正常（headless UI 测试与浅深色截图通过；实库待测）。
- [x] 命令面板：支持 `show`、`db.runCommand` 与 `db.<coll>.*` 语句、切换数据库、结果渲染与错误标红；历史日志记录（`FakeDocDriver` 截图与命令解析/执行单测通过）。
- [x] 能力字段 `initial_database` 驱动表单；无产品名分支；`check-dialect-branches.sh`、`check-deps.sh`、`check-i18n.sh`、`check-raw-px.sh` 全过。
- [x] 第 5 节离线检查全过（fmt/clippy/全工作区测试）；`progress.md` 更新；验收记录 `../pr/acceptance-mongodb.md`。
- [ ] `NAVIDUCK_MONGO_LIVE=1` 实库测试（10 项：连接/认证失败、系统库过滤、集合增删改名、find 过滤排序分页、文档增改删、索引只读、命令执行、URI 连入）待本机 Docker/OrbStack 启动后执行。
