# MongoDB 接入验收记录（142-mongodb.md）

分支 `feat/mongodb`，M14 第三个数据库（`Family::Document` 首例）。驱动选用官方 `mongodb` 3.9（Apache-2.0，rustls-tls + ring + dns-resolver）。规格文档单独先行提交（`d7fdd75`），其后完成驱动、会话、导航与对象窗格、集合数据编辑器、命令列界面、`FakeDocDriver`、headless UI 测试与 Metal 截图。

## 能力层与驱动（`naviduck-db` / `naviduck-mongodb`）

- `DriverInfo` 新增 `initial_database: bool` 能力标志与 `DriverInfo::document(id, name)` 构造器；连接表单按 `multi_host || database_number || initial_database` 显示初始数据库输入框，无产品名分支（经 `naviduck-headless` 与 `check-dialect-branches.sh` 验证）。
- `Document` trait 按 [142-mongodb.md](../design/142-mongodb.md) §4 扩展为 12 个方法：`list_databases`、`list_collections`、`collection_stats`、`find`、`insert_document`、`replace_document`、`delete_documents`、`create_collection`、`drop_collection`、`rename_collection`、`list_indexes`、`execute_command`。
- `naviduck-mongodb`：构建标准 `mongodb://` / `mongodb+srv://` 连接串，接入 TLS 与 `naviduck_db::connect` SSH 隧道；`_id` 转义为规范 Extended JSON（`$oid` / `$uuid` / 字符串等）支持精确替换与批量删除；`list_databases` 自动过滤或保留系统库，所有命令写入 `naviduck_db::log`；纯 Rust 单元测试 8 项通过。
- `naviduck-model` 与 `naviduck-app-state`：补齐 `driver_defaults("mongodb")`（默认端口 `27017`）、`DatabaseUri` 的 `mongodb://` 方案解析与 `Selection::Collection` 构建。
- `naviduck-session`：注册 `MongoDriver::default()`（第 7 个内置驱动），为 `Family::Document` 补齐 `Sessions::open` / `refresh` / `run_doc` / `run_on_doc`。

## 导航树、图标与对象选项卡（`naviduck-navigator` / `naviduck-ui`）

- 新增原创 MongoDB 抽象 SVG 图标（`crates/naviduck-ui/assets/icons/db/mongodb.svg`），注册为 `db/mongodb`；集合与文档类型着色接入 `tokens.rs`。
- `naviduck-navigator` 新增 `docspace.rs` 与 `navigator/docops.rs`：支持文档库（区分 `admin`/`local`/`config` 系统库与隐藏系统库开关）和集合浏览、`Selection::Collection` 发布、对象选项卡集合条目与常规信息窗格、新建/重命名/删除集合、⌘R 刷新与「MongoDB 命令列界面」菜单入口。

## 集合数据编辑器与命令面板（`naviduck-doc`）

- `CollectionViewer`：提供网格（Table）、树（Tree，可展开嵌套文档/数组与类型着色）和 JSON（Card/Extended JSON）三种视图切换；支持 Filter / Sort JSON 条件、分页（每页 50 条）、新增/编辑/删除文档（JSON 编辑对话框）与刷新。自持会话经 `Sessions::new_session` 获取并在 `on_removed` 归还。
- `DocConsole`：支持 `show dbs`、`show collections`、`use <db>`、`db.runCommand(...)`、`db.<coll>.find/countDocuments/insertOne/deleteOne(...)` 以及原始命令 JSON，支持库名切换、文本表/JSON 结果渲染与错误标红，日志写入连接历史记录。
- `naviduck-test-support::doc`：`FakeDocDriver` 内存文档库替身，支撑 `naviduck-doc` headless 编辑器往返测试及主窗口快照测试。

## 验证

- 静态检查：`scripts/check-raw-px.sh`、`scripts/check-i18n.sh`、`scripts/check-dialect-branches.sh`、`scripts/check-deps.sh`、`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --offline -- -D warnings` 全过。
- 离线测试：`cargo test --workspace --offline` 全工作区通过（含 `naviduck-mongodb` 8 项单测、`naviduck-doc` headless UI 测试、`naviduck-headless`、`naviduck-navigator`、`naviduck-ui`、`naviduck` 全套用例）。
- 截图目测（`target/snapshots/`）：`picker.png`（NoSQL 分类与 MongoDB 图标）、`mongo-navigator.png`、`mongo-viewer-tree.png`、`mongo-viewer-table.png`、`mongo-viewer-json.png`、`mongo-viewer-dark.png`、`mongo-console.png`、`mongo-console-dark.png` 共 8 张截图全部逐张查看通过，浅深色对比度、截断与图标显示正常。
- 实库测试（`docker-compose.yml` `mongo:8` 服务与 `crates/naviduck-mongodb/tests/live.rs`）：测试套件已编译通过检查，但当前环境未运行 Docker / OrbStack 守护进程（`~/.orbstack/run/docker.sock` 不存在），`NAVIDUCK_MONGO_LIVE=1` 实库用例待启动 Docker 后补跑。

## 已知偏差（详见 142-mongodb.md 第 6 节）

聚合管道构建器、Index 设计与写入编辑、Schema 分析抽样、GridFS 视图、Change Streams 监控、`mongodump`/`mongorestore` 备份还原与事务会话控制各自独立后续，不在首版范围内。
