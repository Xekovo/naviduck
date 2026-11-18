# 达梦 DM8 接入验收记录

分支 `feat/dameng`，对应 M14。规格：[140-dameng.md](../design/140-dameng.md)、[db/dameng/features.md](../design/db/dameng/features.md)。驱动选型：`dm-pure`（MIT，纯 Rust 同步 API）为主，ODBC 兜底；阶段 0 判据 1–4 全部通过，**继续 dm-pure 路线，不切 ODBC**。

## 技术验证（阶段 0）

### 环境与许可

- 驱动：`dm-pure` 1.2.4（crates.io 最新版，MIT）。同步 API，照 `navidog-sqlite` 的阻塞线程模式跑在 `navidog-db` 运行时上。
- 服务器：DM8 官方 Docker 镜像 `dm8_single:dm8_20241022_rev244896_x86_rh6_64`，从官网 `download.dameng.com` 下载 tar（约 769 MB）后 `docker load`。x86_64 镜像，本机 Apple Silicon 经 OrbStack Rosetta 模拟运行（`--platform linux/amd64 --privileged`）。
- **许可核实**：开发版许可为 1 年试用，条款限定学习/测试/开发用途。结论：**DM 镜像不进 CI**，实库测试标记为本地可选（手动起实例 + 环境变量开关，见规格第 8 节）。
- 三个实例（凭据均 `SYSDBA/SYSDBA001`；`UNICODE_FLAG`、`CASE_SENSITIVE` 是建库参数，初始化后不可改；`COMPATIBLE_MODE` 是静态参数，`SP_SET_PARA_VALUE(2,'COMPATIBLE_MODE',n)` + 重启生效）：

| 容器 | 端口 | UNICODE_FLAG | CASE_SENSITIVE | COMPATIBLE_MODE | 用途 |
| --- | --- | --- | --- | --- | --- |
| `dm8-utf8` | 15236 | 1（UTF-8） | 1 | 0 | 主验证 |
| `dm8-gbk` | 15237 | 0（GB18030） | 1 | 0（临时切 2/4 实测后已恢复） | 编码与兼容模式 |
| `dm8-ci` | 15238 | 1（UTF-8） | 0 | 0 | 大小写不敏感 |

- 验证程序：`/tmp/dm8-verify/verify`（临时 cargo 工程，不进仓库），依赖打补丁后的 dm-pure 副本。输出存档：`result-utf8.txt`、`result-gbk.txt`（打补丁前）、`result-utf8-patched.txt`、`result-gbk-patched.txt`（四补丁后全量回归，各 51 项 PASS、0 FAIL）、`mode-utf8-cs1.txt`、`mode-utf8-cs0.txt`、`mode-gbk-mode2.txt`、`mode-gbk-mode4.txt`、`ddl-probe.txt`、`ddl-probe2.txt`。

### 复现命令

```sh
docker load -i dm8.tar
docker run -d --name dm8-utf8 --platform linux/amd64 --privileged -p 15236:5236 \
  -e PAGE_SIZE=16 -e LD_LIBRARY_PATH=/opt/dmdbms/bin -e EXTENT_SIZE=32 \
  -e BLANK_PAD_MODE=1 -e LOG_SIZE=1024 -e UNICODE_FLAG=1 -e LENGTH_IN_CHAR=1 \
  -e INSTANCE_NAME=dm8-utf8 -e SYSDBA_PWD=SYSDBA001 -e BUFFER=1000 \
  dm8_single:dm8_20241022_rev244896_x86_rh6_64
# dm8-gbk：UNICODE_FLAG=0、端口 15237；dm8-ci：CASE_SENSITIVE=0、端口 15238
cd /tmp/dm8-verify/verify
DM_PORT=15236 cargo run --bin dm8-verify   # 全量九项
DM_PORT=15236 cargo run --bin modeprobe    # 大小写/兼容模式/lazy fetch
DM_PORT=15236 cargo run --bin ddlprobe2    # 设计器 DDL/事务/SP_TABLEDEF
```

### 九项结论

| 项 | 结论 | 证据与要点 |
| --- | --- | --- |
| 1 连接与认证 | 通过 | 正常登录 OK；错误密码报 `code=-2501`「用户名或密码错误」（干净消息，见补丁 3）；版本取 `SELECT BANNER FROM V$VERSION`（`DM Database Server 64 V8`）+ `SELECT ID_CODE()`（`--03134284294-20241009-244896-20119 Pack3`，即 8.1.3.140 rev244896）。 |
| 2 目录 SQL | 通过 | `ALL_TABLES`、`ALL_TAB_COLUMNS`、`ALL_CONSTRAINTS`、`ALL_CONS_COLUMNS`、`ALL_VIEWS`、`ALL_PROCEDURES`、`ALL_INDEXES`、`ALL_IND_COLUMNS`、`ALL_OBJECTS`、`ALL_USERS`、`ALL_SEQUENCES`、`ALL_SOURCE`、`ALL_TAB_COMMENTS`、`ALL_COL_COMMENTS` 均可用。`OWNER` = 用户名（用户≈模式）；`V$DATABASE.NAME` = `DAMENG`（单数据库）；`SYS_CONTEXT('USERENV','CURRENT_SCHEMA')`、`USER`、`SF_GET_SCHEMA_NAME_BY_ID(CURRENT_SCHID())` 均返回当前模式。 |
| 3 中文与编码 | 通过 | UTF-8 与 GB18030 两实例：中文表名/列名（引用与未引用）、参数绑定与内联字面量写入读回全部精确匹配。握手 `server_encoding_name()` 返回 `UTF-8`/`gb18030`，dm-pure 用 encoding_rs 自动检测。**DM 建库的「GBK」选项实际是 GB18030**。 |
| 4 LOB | 通过（补丁 1 后） | CLOB 136000 字符、BLOB 200000 字节读回精确匹配；`set_lob_chunk_size(4096)` 同样匹配；行内小 LOB 正常。VARCHAR(10) 超长报 `-6169`「列[S]长度超出定义」，**不静默截断**。原版 1.2.4 行外 LOB 读回为空（服务器侧数据经 disql 证实已写入），根因见补丁 1。 |
| 5 事务 | 通过 | `START TRANSACTION`/`COMMIT`/`ROLLBACK` SQL 与 dm-pure 协议级 `begin()`/`commit()`/`rollback()`（cmd 52/8/9）均正常；`SAVEPOINT`/`ROLLBACK TO SAVEPOINT`/`RELEASE SAVEPOINT` 可用（`navidog_db::commit` 流程兼容）。**DDL 隐式提交确认**：事务内未提交的 INSERT 被 DDL 提交，随后 ROLLBACK 仍在（SQL 路径与协议路径各验一次）。`BEGIN TRANSACTION` 不支持（`-2004` 不支持的语句类型）。 |
| 6 取消执行 | 通过（有偏差） | dm-pure 无取消 API。第二连接 `CALL SP_CLOSE_SESSION(sessid)` 可中断长查询（40000×40000 聚合，约 8s；2s 时 kill，victim 收 `-6852` 连接关闭）。**杀的是整个会话而非仅语句**，取消后原会话必须重连。两实例均验证。 |
| 7 SSL/TLS | 不支持 | dm-pure 1.2.4 源码无 TLS 路径（源码审查）；服务器 `ENABLE_ENCRYPT=0` 已记录。`DriverInfo.supports_ssl=false`，SSL 页不显示。 |
| 8 会话 ID | 通过 | `SELECT SESSID()` 返回 BigInt，可用；`SF_GET_SESSION_ID()` 不存在（`-2207`）。 |
| 9 大小写与兼容模式 | 通过 | `CASE_SENSITIVE()` 函数可连接后探测（`V$DM_INI` 无此行）；`COMPATIBLE_MODE`/`ENABLE_ENCRYPT`/`BLANK_PAD_MODE` 经 `V$DM_INI` 探测。CS=1：未引用标识符折大写、引用标识符大小写敏感；CS=0：按原样存储、引用标识符不敏感匹配。mode 2（Oracle）：空串 `IS NULL`；mode 0/4：空串非 NULL。mode 4（MySQL）：支持反引号；mode 0/2 不支持（`-2007`）。 |

### 附加验证

- **分页**：`LIMIT n OFFSET m`、`TOP n`、`ROWNUM <= n`、`FETCH FIRST n ROWS ONLY` 四种语法均可用（mode 0 实测）。方言分页统一用 `LIMIT … OFFSET …`。
- **EXPLAIN**：返回缩进文本计划（简单查询 4 行）。原版 1.2.4 挂起至超时，根因见补丁 2。
- **lazy fetch**：`execute_lazy` 首批 100 行，`fetch_more` 续取 4900 行合计 5000，两实例一致；网格分批读取可用。
- **模式切换**：`SET SCHEMA <name>` 与 `ALTER SESSION SET CURRENT_SCHEMA = <name>` 均可用（补丁 4 后），`SYS_CONTEXT` 证实切换生效；无效模式名报干净服务端错误 `-2103`。
- **表 DDL 来源**：`CALL SP_TABLEDEF('SYSDBA','T')` 返回完整 `CREATE TABLE`（含引号、`STORAGE(ON "MAIN", CLUSTERBTR)`、`\r\n` 换行）。`DBMS_METADATA.GET_DDL` 对 TABLE 报 `-20008`、对 VIEW/INDEX/SEQUENCE 返回空串，**不可用**；视图 DDL 用 `ALL_VIEWS.TEXT` 拼接，索引/序列 DDL 由目录合成。
- **设计器 DDL 语法**（`ddl-probe.txt`/`ddl-probe2.txt`）：`ALTER TABLE … ADD (col …)`、`ADD col …`、`MODIFY col …`、`MODIFY (col …)`、`RENAME COLUMN a TO b`、`DROP COLUMN c`、`ALTER COLUMN c SET DEFAULT …`/`DROP DEFAULT`、`ALTER TABLE … RENAME TO new`、`COMMENT ON TABLE/COLUMN`、`ADD CONSTRAINT … UNIQUE/CHECK`、`DROP CONSTRAINT`、`CREATE/DROP INDEX`、`IDENTITY(1,1)`、`CREATE SEQUENCE … START WITH … INCREMENT BY …`、`seq.NEXTVAL` 均通过。独立 `RENAME old TO new` 不支持（`-2007`），改名统一走 `ALTER TABLE … RENAME TO`。

### dm-pure 缺陷与补丁（4 处）

对照官方 Go 驱动（`gitee.com/chunanyong/dm`，GitHub 仓库 404）协议实现定位。干净补丁 `/tmp/dm8-verify/dm-pure-pr.patch`（122 行，仅 `src/client.rs`，无调试代码）；托管仓库 `/tmp/dm8-verify/dm-pure-host`：`main` = `0a299ff`（crates.io 1.2.4 原版镜像，溯源上游 commit `58579165765aa764566c191d3a458b03c28db2f0`），`fix/protocol-lob-explain-error-status` = `59abd96`（仅四补丁，`src/client.rs` +70/-6），PR 描述 `/tmp/dm8-verify/PR_DESCRIPTION.md`。四补丁后 UTF-8 与 GB18030 全量回归各 51 项 PASS、0 FAIL（`result-utf8-pr.txt`/`result-gbk-pr.txt`）。

1. **行外 LOB 读回为空**（核心项）：`read_offrow_lob` 的 cmd=32 请求把 offset/length 写成 i64，协议要求 int32（Go `dm_build_676.dm_build_421`），服务器读到 length=0。改为 i32；CLOB 偏移按服务器返回的字符数推进（原按请求字节数）。
2. **EXPLAIN 挂起**：响应是 cmd=149 帧（Go `retSqlType` Dm_build_386=127+22=149），布局 int32 长度 + 计划文本，服务器随后不再发帧。dm-pure 只认 187/163，苦等至 EAGAIN。加 149 分支：解析文本按行返回单列 `PLAN` 结果集。
3. **错误消息带前缀**：错误体布局 12 字节头 + int32 长度 + 文本；原实现整体解码，长度低字节恰为可打印字符时出现 `E   ` 之类前缀。按布局解析后消息干净（如「用户名或密码错误」）。
4. **SET SCHEMA / ALTER SESSION 挂起**：这类会话状态语句的响应只有 cmd=0 短状态帧（length≤30）、没有 cmd=187 结果帧；exec 循环苦等 187 至 EAGAIN。循环遇到无 187 伴随的 cmd=0 短帧即停止，按 `Affected(0)` 返回；有 187 时不受影响（参数化路径回归通过）。

### 判据与路线

判据 1–4 全部通过（LOB 经补丁 1）→ **继续 dm-pure 路线**。5–9 的缺口（取消杀会话、无 SSL、DDL 隐式提交、GB18030 命名）不阻塞，逐条写进 [140-dameng.md 第 7 节「偏差」](../design/140-dameng.md#7-偏差)。

**依赖策略（已确认，上游消失后更新）**：上游仓库 `zhangtao103239/dm8_pure_rs` 在 GitHub 返回 404，全站搜索（含 fork）未找到镜像，无法 fork/提 PR。改为自建托管：用户创建公开仓库 `github.com/ekovovo/dm8_pure_rs`（非 fork，不初始化 README/license），`main` 为 crates.io 1.2.4 原版镜像，`fix/protocol-lob-explain-error-status` 分支仅含四补丁。上游发布修复版前，`navidog-dameng` 用 `[patch.crates-io]` git 依赖指向该修复分支（`dm-pure = "=1.2.4"` + patch，提交 Cargo.lock）；上游恢复后向其提 PR，上游发修复版后切回 crates.io 精确版本并移除 patch。

## 验收记录（阶段 3，待补）
