# 141 Redis 接入

M14 的第二个数据库，也是第一个非关系型（`Family::KeyValue`）。新建不依赖 GPUI 的 `naviduck-redis` crate，实现 `Driver`/`Session`/`KeyValue`，在 `naviduck-session::init` 注册。专属功能清单见 [db/redis/features.md](db/redis/features.md)（F-REDIS-001~012），首版只覆盖其中连接、数据库、数据编辑器、搜索排序、导航栏、查询六项；其余见第 6 节偏差。

驱动：**fred 10.1.0**（MIT，原生异步）。阶段 0 选型对比过 redis-rs（2.0 仍在 RC，稳定线 0.27 的集群与哨兵支持较弱）；fred 的集群、哨兵、RESP3、TLS（rustls）与流式命令是一等公民，异步 API 直接跑在 `naviduck_db::runtime` 的 tokio 运行时上，不需要 spawn_blocking。

## 1. 范围与首版边界

首版实现：

- 连接：独立连接（主机/端口/ACL 用户名/密码/初始数据库编号）、SSL（rustls）、SSH 隧道（复用 `naviduck_db::connect` 编排）。哨兵与集群连接方式后续（偏差 D1）。
- 导航树到键级：连接 → 数据库（db0..dbN，显示键数量）→ 键（SCAN 分批加载、模式搜索）。键前缀分组后续（偏差 D3）。
- 键数据编辑器：String/List/Hash/Set/Sorted Set 读写，Stream 只读（偏差 D2）；TTL 显示/修改/持久化；新增 String 键、删除键、重命名键。
- 查询：Redis 命令面板（逐行命令、表格化结果，复用结果网格）。

首版不做（后续任务）：Pub/Sub（F-REDIS-009）、ACL 用户管理（F-REDIS-010）、`.nbr` 备份还原（F-REDIS-011）、命令监控（F-REDIS-012）、哨兵/兼容性设置页（F-REDIS-002/003）、Stream/JSON 编辑、键前缀分组、命令补全。

## 2. 连接参数与 DriverInfo

```text
id: "redis"
name: "Redis"
family: KeyValue
default_port: 6379
file_based: false
multi_host: false
supports_network: true
supports_ssl: true
supports_ssh: true
per_database_sessions: false
```

`DriverInfo` 新增字段 **`database_number: bool`**（基线 false）：常规页显示「数据库」编号输入（非负整数，默认 0）。现有 `initial_database` 输入只在 `multi_host=true` 显示，不适用于 Redis 的编号语义。连接表单其余部分零改动：SSL 页（rustls 验证模式）、SSH 页、高级页照常。

`driver_defaults("redis")`：host `localhost`、port 6379、user 空、database `Some("0")`。

fred 配置映射：host/port → 地址；`general.user` 非空时作为 ACL 用户名（RESP3 AUTH username password）；`general.password` → 密码；初始数据库 → `general.database` 解析为编号，fred `database` 字段；SSL → `enable-rustls` 特性，验证模式沿用现有 `SslMode` 映射（Required/VerifyFull 同 fred 的证书校验开关）。连接/读写超时沿用 `ConnectParams::timeout()`。`auto_reconnect` 固定关闭（恢复由 naviduck-db 编排，驱动不自作主张）。

图标：自绘抽象 SVG `icons/db/redis.svg`，不提取 Navicat 图标。

## 3. 会话与执行

### 3.1 线程模型

fred 是异步客户端，`Driver::connect` 与各命令直接在 `naviduck_db::runtime` 上 await，不引入 spawn_blocking。一个 `RedisSession` 持有一个 fred `Client`（内部连接由 fred 管理）。

### 3.2 Session 方法

| 方法 | 实现 |
| --- | --- |
| `server_version` | `INFO server` 的 `redis_version`，格式 `Redis 7.2.x`。连接时取一次缓存，探测失败不阻塞连接。 |
| `ping` | `PING`，走 `recovering_ping!`。 |
| `session_id` | `CLIENT ID`（取不到返回 None）。 |
| `cancel_handle` | `None`（偏差 D6）：首版命令均为快速命令，无长任务。 |
| `close` | fred `Client::quit`（优雅关闭），失败忽略。 |
| `key_value` | 返回自身。 |

### 3.3 数据库切换

`KeyValue` 的所有方法显式携带 db 编号。会话内部维护 `current_db`：执行前 `db != current_db` 时发 `SELECT <db>`，应用层串行（`SharedSession` 的 tokio Mutex）保证 SELECT 与命令之间不会被插入。查询面板/键编辑器经 `Sessions::new_session(id, Some("2"))` 获得独立会话时，`ConnectParams.database` 直接映射到 fred 的初始 database，会话从目标库开始。

`databases()` 用 `CONFIG GET databases` 探测（Redis 默认 16，可配置到更多），返回 0..N；探测失败回退 16（偏差 D5）。`ConnState::Open { databases }` 沿用现有 `Vec<String>`，编号以十进制字符串存放（"0"…"15"）。

### 3.4 执行语义与恢复

`recovering_execute!` 是 SQL 专用的；Redis 命令不经它。恢复策略：`ping` 与各元数据读走 `recovering_ping!` 模式（attempt → disconnected → reconnect → 重试一次）；写命令（写值、删键、TTL、execute_command）**不重放**，断连报 `outcome_unknown`。重连 = 重建 fred client + 重新探测版本，恢复 `current_db`。

错误映射 `DbError { code, message, disconnected, outcome_unknown }`：

- `code`：RESP 错误前缀（`ERR`、`WRONGTYPE`、`NOAUTH`、`MOVED`…），显示为 `WRONGTYPE - …` 形式。
- `disconnected=true`：IO/连接关闭/超时错误。
- `outcome_unknown=true`：写命令发出后连接断开且未收到响应。

## 4. KeyValue 能力层

`naviduck_db::driver::KeyValue` 按下列签名扩展（现有 `databases`/`scan`/`get` 保留，`get` 并入 `read` 后删除）。全部默认不实现，编译器强制驱动补齐：

```rust
/// 键的解码值，供数据编辑器渲染。
pub enum KeyValueData {
    /// 文本值；非 UTF-8 字节显示为 `0x…` 十六进制文本（与达梦 Bytes 网格约定一致）
    Str(String),
    List(Vec<String>),
    Hash(Vec<(String, String)>),
    Set(Vec<String>),
    /// 成员 + score 文本
    ZSet(Vec<(String, String)>),
    /// ID + 字段对
    Stream(Vec<(String, Vec<(String, String)>)>),
    /// 未知/模块类型，携带类型名原样说明
    Other(String),
}

pub trait KeyValue: Send {
    fn databases(&mut self) -> DbResult<Vec<u32>>;
    fn key_count(&mut self, db: u32) -> DbResult<u64>;                     // DBSIZE
    fn scan(&mut self, db: u32, pattern: &str, cursor: u64, count: u32)
        -> DbResult<(u64, Vec<String>)>;
    fn key_type(&mut self, db: u32, key: &str) -> DbResult<String>;       // TYPE
    fn ttl(&mut self, db: u32, key: &str) -> DbResult<i64>;               // PTTL→秒，-1 无过期，-2 不存在
    fn read(&mut self, db: u32, key: &str, offset: u64, count: u32)
        -> DbResult<KeyValueData>;                                        // 按类型分派，分批
    fn set_string(&mut self, db: u32, key: &str, value: &str) -> DbResult<()>;
    fn hash_set_field(&mut self, db: u32, key: &str, field: &str, value: &str) -> DbResult<()>;
    fn hash_delete_field(&mut self, db: u32, key: &str, field: &str) -> DbResult<()>;
    fn list_push(&mut self, db: u32, key: &str, tail: bool, value: &str) -> DbResult<()>;
    fn list_delete(&mut self, db: u32, key: &str, value: &str) -> DbResult<()>;
    fn set_add(&mut self, db: u32, key: &str, member: &str) -> DbResult<()>;
    fn set_remove(&mut self, db: u32, key: &str, member: &str) -> DbResult<()>;
    fn zset_add(&mut self, db: u32, key: &str, member: &str, score: &str) -> DbResult<()>;
    fn zset_remove(&mut self, db: u32, key: &str, member: &str) -> DbResult<()>;
    fn create_key(&mut self, db: u32, key: &str) -> DbResult<()>;         // SET key ""
    fn delete_keys(&mut self, db: u32, keys: &[String]) -> DbResult<u64>; // DEL
    fn rename_key(&mut self, db: u32, key: &str, new_name: &str) -> DbResult<()>;
    fn set_ttl(&mut self, db: u32, key: &str, seconds: Option<i64>) -> DbResult<()>; // None=EXPIRE 持久化 PERSIST
    /// 查询面板的通用命令执行；结果映射见 4.1
    fn execute_command(&mut self, db: u32, args: &[String]) -> DbResult<Vec<ResultSet>>;
}
```

写命令（`set_string` 到 `rename_key`）不重放、断连报 outcome_unknown；读命令可恢复重试。命令映射：`read` 按 `TYPE` 分派 GET/LRANGE/HGETALL/SMEMBERS/ZRANGE(REV, WITHSCORES)/XRANGE；`set_ttl` EXPIRE/PERSIST；分批读的 offset/count 映射各类型命令的游标参数，Stream 用 `XRANGE - + COUNT` 顺序分页。

### 4.1 execute_command 的结果映射

命令数组经 fred 的通用 `custom` 调用执行，`Value` 按 RESP 类型映射为 `ResultSet`，供结果网格与消息区复用现有渲染：

| RESP 值 | ResultSet |
| --- | --- |
| SimpleString（OK、PONG） | 1 列 1 行状态文本 |
| Int | 单行整数，`affected_rows` 同值 |
| BulkString | 1 列 1 行 |
| VerbatimString | 去掉类型前缀后的文本 |
| Double/Bool/BigInt | 文本 |
| Nil | NULL 单元格 |
| Array（元素为标量） | 1 列 N 行 |
| Array（元素为双元素数组，如 HGETALL、CONFIG GET） | 2 列 N 行 |
| Array（元素为更深数组，如 XRANGE） | 每个子数组展开为固定列数的行，列名 `col1..colN` |
| Push（RESP3） | 后续 Pub/Sub 使用，首版按消息文本呈现 |

## 5. 界面

### 5.1 导航树（naviduck-navigator）

导航树现有层级按 `dialect`（关系型专用）构造。新增按 Family 的分支：`DriverRegistry` 取驱动 `info().family == KeyValue` 时，`database()` 的子项不查 catalog 而查 **KeyCatalog**（navigator 内新结构）：

- 展开数据库节点 → `scan(db, "*", 0, 250)` 首批键，平铺显示；底部「加载更多」项携带 cursor 续扫。
- 搜索框输入 → 以 `*<text>*` 为 pattern 重扫（F-REDIS-006 的首版形态）。
- 键行右键：打开、删除、重命名（复用 F-NAV 菜单机制）；双击打开键数据编辑器。
- 数据库节点标签显示 `db3 (12)` 键数量（`key_count`），⌘R 刷新 = cursor 重置重扫。
- 每个数据库的已加载键与 cursor 按连接代际失效（照 catalog 的生命周期）。

键节点 id 与 `Node` 枚举新增变体 `Key(conn, db, name)`；`scripts/check-dialect-branches.sh` 不涉及。

### 5.2 键数据编辑器（新 crate `naviduck-keyval`）

GPUI feature crate，依赖 `naviduck-grid` 复用 DataTable 渲染成员表（与 `naviduck-query` 依赖 `naviduck-grid` 同构，是依赖方向的第二个例外，登记进 00-overview）；不依赖其他 feature crate。

- 工具栏：键名（truncated）、类型图标、TTL 显示与编辑（弹出输入秒数，`-1` 显示「永久」）、刷新、删除。
- 成员区按类型：String 单值文本编辑；Hash 字段/值两列；List 序号/值；Set 成员；ZSet 成员/分数。单元格编辑离开即提交（照网格编辑语义）；行右键删除成员（Hash 字段、List/Set/ZSet 成员）。
- 底部「添加」行：按类型调用 `list_push`/`set_add`/`zset_add`/`hash_set_field`。
- Stream 只读显示 ID 与字段。
- 会话经 `Sessions::new_session(conn, Some(db))` 获得，关闭时归还（照表查看器）；选项卡接入 `naviduck_ui::tabs`（on_added_to/set_active/on_removed、`tabs::title`）。

入口：`crates/naviduck/src/editors.rs` 的 `open()` 增加键分支（`Node::Key` 对应的打开路径不走 `ObjectRef::kind`，外壳按连接 driver 的 Family 判定）；导航树连接节点的查询入口打开命令面板。

### 5.3 查询命令面板（`naviduck-keyval`）

简版命令编辑器 + 结果区（复用查询面板的结果网格与消息区组件，不接 SQL 分句器/补全/解释）：

- 每行一条命令：空白分词，支持 `"`/`'` 引号参数，`//` 开头的行是注释；空行跳过。
- 目标选择：数据库编号下拉（连接打开后的 `ConnState::databases`）。
- 逐条执行，出错的命令标出，可继续；结果按 4.1 的映射进结果网格；`SERVER_VERSION` 等无网格的结果进消息区。
- 历史日志走 `naviduck-sql-log`（args 以空格拼接，含引号转义）。

### 5.4 i18n 与文案

`naviduck-redis/locales/redis.yml` 与 `naviduck-keyval/locales/keyval.yml`，en 与 zh-CN 同步，中文用词照 `reference/help-text/` 的 Redis 帮助页；navigator 新增的键相关文案进 navigator 的 locale。动态 key 登记 `scripts/i18n-dynamic-keys.txt`。

## 6. 偏差

| 编号 | 偏差 | 理由 | 计划处理 |
| --- | --- | --- | --- |
| D1 | 哨兵与集群连接方式后续 | 控制首版范围；fred API 已具备，规格预留 Builder 分支 | M14 后续任务，实库测试覆盖后开启 |
| D2 | Stream 只读，RedisJSON 不支持 | 控制首版范围 | Stream XADD 编辑与 JSON 模块后续 |
| D3 | 键平铺 + 搜索，前缀分组后续 | Navicat 的前缀分组是可选行为 | 后续按 `hide_object_groups` 风格做成开关 |
| D4 | Pub/Sub、ACL 用户、.nbr 备份、命令监控不做 | F-REDIS-009~012 整体后续 | 各自独立任务 |
| D5 | 数据库数量探测失败回退 16 | `CONFIG GET databases` 可能被禁用 | 显示实际数量，失败时 16 个 |
| D6 | 取消不做（cancel_handle=None） | 首版无长任务 | 命令监控/MONITOR 类任务时再评估 CLIENT KILL |
| D7 | 非 UTF-8 值以 `0x…` 十六进制显示，编辑按十六进制还原 | 网格是文本通道 | 与达梦 Bytes 约定一致 |
| D8 | 键编辑器不提供 Navicat 的批量过期/格式化视图 | 控制首版范围 | 后续 |

## 7. 测试与验收清单

### 7.1 测试

- 单元测试（进 CI）：`naviduck-redis` 的连接参数组装（用户/密码/初始库/TLS/SSH 端口）、错误映射、`execute_command` 的 RESP→ResultSet 映射、db 切换状态机、十六进制值往返；`naviduck-db` 的 KeyValue 默认方法语义；driver_defaults 与连接构造器一致性。
- 实库测试：`docker compose` 增加 `redis` 服务（端口 16379），环境变量 `NAVIDUCK_REDIS_LIVE=1`。覆盖：连接/错误密码/ACL 用户、CONFIG databases 探测、SCAN 分批与 pattern 搜索、五种键类型读写往返、TTL 设置与持久化、删/改名、execute_command（GET/HGETALL/XRANGE/错误命令）、SELECT 切库、重连恢复 current_db。进 CI 的 integration 工作流（Redis 无许可问题）。
- `naviduck-headless` 增加 redis 编译回归（证明驱动栈不链接 GPUI）；`scripts/check-deps.sh` 名单加 `naviduck-redis`（keyval 是 GPUI crate 不加）。
- 截图测试：键数据编辑器各类型浅深色；导航树 db/键层级。

### 7.2 验收清单

- [ ] 连接表单：常规页显示数据库编号输入；SSL/SSH 页正常；错误密码报干净消息（实库）。
- [ ] 连接打开：导航树 db0..dbN 带键数量；键分批加载与搜索；⌘R 刷新。
- [ ] 键数据编辑器：五种类型读写往返（实库）、TTL 修改/持久化、新增/删除/重命名键、成员增删。
- [ ] 命令面板：逐行执行、表格化结果、错误命令标出；历史日志记录。
- [ ] 能力字段 `database_number` 驱动表单；无产品名分支；`check-dialect-branches.sh` 过。
- [ ] 第 5 节全部检查通过（fmt/clippy/check-deps/check-i18n/check-raw-px/全量测试）。
- [ ] progress.md 更新（M14 行、任务表、偏差表）；验收记录 `pr/acceptance-redis.md`。
