# Linux / Windows 偏好设置入口修复验收记录

日期：2026-11-26。分支：`m4-cross-platform-settings`。对应 F-APP-003、F-PREF-001，规格见 [10-shell.md §9.1](../design/10-shell.md#91-linux-与-windows-的菜单入口)。规格已先单独提交：`fdc5408`。

## 原因与改动

此前应用只构建菜单并交给 macOS 原生菜单栏，非 macOS 主窗口没有渲染 `AppMenuBar`，导致 Linux 用户看不到偏好设置入口。设置快捷键也统一写成 `cmd-,`，Linux 上对应 Super 而非 Ctrl。

- Linux/Windows 在标题栏下、主工具栏上方显示现有 `AppMenuBar`，隐藏工具栏不影响菜单；使用 `menubar.height` token 和现有背景、分隔线颜色。macOS 继续走原生系统菜单。
- 各平台共用菜单构建、Action 和偏好设置窗口；非 macOS 直接发布组件的菜单模型，避免依赖平台原生菜单读回。服务、隐藏、隐藏其他、全部显示仅保留在 macOS。
- 设置快捷键按平台选择：macOS `cmd-,`，Linux/Windows `ctrl-,`；命令目录复用同一常量。其余历史快捷键不在本轮迁移范围内。
- 菜单重建通过专用 `MenusChanged` 通知窗口刷新，不观察每次指针事件都会变化的组件 `GlobalState`，避免菜单刚点开就被重建关闭。
- 修复已打开的设置窗口处理设置 Action 时重复新建窗口的问题：先确认窗口句柄仍存在，当前窗口正在更新时延后激活。

## 回归用例

`crates/naviduck/tests/preferences_entry.rs` 复用生产初始化、主窗口和设置 UI，验证：

1. 命令目录的设置快捷键符合平台约定；修复前先看到 `Some("cmd-,") != Some("ctrl-,")` 的失败，修复后通过。
2. 非 macOS 在浅色和深色下第一次点击应用菜单即可看到 `Settings…`，没有 macOS 专属的 Services；Escape 关闭菜单并归还焦点。
3. 隐藏主工具栏后，通过真实菜单点击仍能打开设置。
4. 设置快捷键复用同一窗口；从设置窗口内再次派发设置 Action 不会重复创建。
5. 通过设置中的语言选项切到中文，主菜单和「偏好设置…」随之刷新。
6. 关闭设置后再次按快捷键可以重新打开。

## 验证结果

| 检查 | 命令 / 证据 | 结果 |
| --- | --- | --- |
| 设置与既有菜单回归 | `cargo test -p naviduck --test preferences_entry --test app_menu --test command_catalog --locked` | 3 项通过 |
| 全工作区测试 | `env -u CARGO_TARGET_DIR cargo test --workspace --locked --no-fail-fast -j 2` | 通过，退出码 0；含单元测试、headless UI 与文档测试 |
| Clippy | `env -u CARGO_TARGET_DIR cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings` | 通过，退出码 0 |
| 格式与静态约束 | `cargo fmt --all --check`；`scripts/check-raw-px.sh`、`scripts/check-i18n.sh`、`scripts/check-dialect-branches.sh`、`scripts/check-deps.sh` | 通过；裸值检查按项目要求在沙箱外运行 |
| Linux WGPU 截图 | `env -u CARGO_TARGET_DIR cargo test -p naviduck --test snapshot --locked` | 6 张通过并逐张目测 |
| 依赖审计 | `cargo deny --all-features check`、`cargo audit` | 未运行成功：环境未安装 cargo-deny、cargo-audit |
| Docker 实库与查询解释 | `docker compose up -d --wait` | Docker Hub 拉取镜像连接超时，未能启动实库验证；已执行 `docker compose down` 清理 |
| 网格、导航树、编辑器性能 | 第 5 节的 `spike` / `tree_spike` / `editor_spike` | 现有示例仅 macOS / Metal 有实际测量实现，Linux 无法验证 |

工作区测试未设置 `NAVIDUCK_LIVE`、`NAVIDUCK_DM_LIVE`、`NAVIDUCK_REDIS_LIVE`、`NAVIDUCK_MONGO_LIVE`；相应实库用例的环境门禁返回不算本轮服务端验证。未修改依赖或锁文件。

## 截图目测

以下截图均由 Linux 的实际 WGPU headless 渲染器生成，保存在本地 `target/snapshots/`，不提交：

- `linux-main-light.png`、`linux-main-dark.png`：菜单独立位于标题栏和主工具栏之间，文字、分隔线清晰。
- `linux-menu-light.png`、`linux-menu-dark.png`：应用菜单中的「偏好设置…」和 Ctrl+, 可见，没有 macOS 专属条目。
- `linux-preferences-light.png`、`linux-preferences-dark.png`：由菜单点击打开的实际设置窗口，分类、表单及外观、语言控件清晰，无裁切。

## 平台边界与待验

Linux 本轮已完成上述交互与截图验证；Windows 使用相同的非 macOS 菜单与快捷键路径，但没有 Windows 原生运行环境，不能视为已验。macOS 保留原生菜单分支，设置单例激活逻辑有共用改动，本轮也未进行 macOS 原生运行态回归。

窄窗口的菜单横向滚动和窗口控制按钮、查询/表选项卡切换、收藏夹及所有勾选菜单的组合走查仍待补验；语言刷新已由实际设置 UI 验证。28pt 菜单高度是已登记的暂定平台适配值。
