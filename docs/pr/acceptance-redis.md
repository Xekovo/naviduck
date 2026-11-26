# Redis 接入验收记录（141-redis.md）

分支 `feat/redis`，M14 第二个数据库（`Family::KeyValue` 首例）。驱动 fred 10.1.0（MIT，原生异步；redis-rs 2.0 仍在 RC 落选，理由见规格）。分四批提交：规格、批次 1（能力层+驱动）、批次 2a/2b（导航树+编辑器）、批次 3（命令面板+截图+测试）。

## 阶段 0：选型与技术验证

- 客户端对比 redis-rs（2.0.0-rc.4）与 fred（10.1.0），fred 的集群/哨兵/RESP3/TLS 一等支持且 API 稳定，全部命令经 fred 的 `custom` 通用调用，回复统一为 `Value`，RESP2 默认保证各服务器版本回复形状一致。
- TLS 选 `enable-rustls-ring`：工作区同时拉进 ring 与 aws-lc-rs 两个 crypto provider，rustls 运行时会拒绝默认选择；ring 与 mysql_async 的 `default-rustls-ring` 对齐。该问题在实库测试中以「CryptoProvider 未确定」panic 暴露。
- SSH 隧道复用 `naviduck_db::connect` 编排（隧道建立后重写 host/port，驱动无感）。

## 批次 1：能力层与驱动

- `KeyValue` trait 按 141-redis.md §4 扩展到 19 个方法（`KeyValueData` 五种键类型 + `Other`）；`DriverInfo.dialect` 改为 `Option`（非 SQL 家族无方言），新增 `database_number` 标志驱动连接表单。
- `naviduck-redis`：独立连接、lazy SELECT 切库（会话串行保证安全）、读命令断连重试一次、写命令不重放报 `outcome_unknown`；所有命令经 `naviduck_db::log` 记录（历史日志自动填充）。
- 实库测试（docker compose `redis` 服务，`NAVIDUCK_REDIS_LIVE=1`）10 项：连接/错误密码/ACL 用户、CONFIG databases 探测、SCAN 分批与 pattern、五种键类型往返、TTL/持久化、改名删除、命令执行与 WRONGTYPE 错误码、SELECT 切库、初始数据库。
- 实测坑：redis-server 的 `--user` 命令行参数解析不可靠（ACL 用户被拆坏），改用 `docker/redis/users.acl` 文件挂载；SCAN 的 RESP2 回复是嵌套 `[cursor, [keys]]`，初版解析按平铺假设写错，由实库测试抓出。

## 批次 2：导航树与键编辑器

- 键空间分页（`keyspace.rs`）：SCAN 250 键/批、⌘R 从 cursor 0 重扫、连接关闭/重开整页失效、键数量 `db0 (4)` 显示、加载更多行。
- 截图流程抓出**状态机 bug**：`require` 对新建页（状态已是 Loading）early-return，扫描永远不启动，键列表永久卡在骨架屏——首版 `require` 先置 Loading 再判断的顺序错误，重构为 `require`（只判断是否需要）+ `scan`（真正启动）两层后修复。该 bug 会同样影响真实 Redis。
- 键数据编辑器（`naviduck-keyval`）：五种键类型值区、成员增删、TTL 设置与持久化、删除键；自持会话经 `new_session`、`on_removed` 归还；选项卡接入 tabs。
- headless 测试：fake 驱动下编辑器加载、写入往返、`set_active` 发布 `Selection::Key`。

## 批次 3：命令面板与截图

- 命令列界面（`console.rs`）：逐行命令、引号分词（`//` 注释）、数据库编号输入、结果按文本表渲染、错误标红；历史日志走驱动侧 `logged`。
- 入口：键值连接右键「命令列界面」（`KeyConsole` 动作），与 SQL 命令列互不干扰。
- 截图（`target/snapshots/`，浅深色各一套）：`redis-navigator`（键树）、`redis-editor`（String 编辑）、`redis-console`。
- 截图基建：`naviduck-test-support` 新增 `kv` 模块——内存 fake 驱动（SCAN glob、五种类型读写），截图与 headless 测试离线跑真实导航/编辑器/命令面板路径。

## 验证

- fmt、clippy（`-D warnings` 等价清零，含批次 1 潜伏的 8 处警告）、check-deps、check-i18n、check-dialect-branches、check-raw-px 全过。
- 全工作区 74 个测试套件 0 失败；`NAVIDUCK_REDIS_LIVE=1` 实库 10 项通过。
- 截图逐张目测：键树层级/键数量/钥匙图标、String 编辑器（类型徽章、TTL 行、保存）、命令面板（提示语、编号输入、执行按钮）浅深色均正常。

## 已知偏差（详见 141-redis.md 第 6 节）

哨兵/集群、Pub/Sub、ACL 用户管理、.nbr 备份、命令监控、Stream/JSON 编辑、键前缀分组、键编辑器 DataTable 化与大键分页读值——各自独立任务，fred 的 API 已预留。
