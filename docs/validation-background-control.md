# 后台浏览器与独立桌面验收记录

> Historical record: the sections below preserve PR #13's original branch
> validation, retained from commit `b4425f7488588798a257b89145b5891376b93647`.
> The integration on main retains current browser setup and desktop APIs:
> `desktop_start`, `desktop_stop`, `app_launch`, and explicit `desktop:"host"`.
> Current batch/wait usage is in [Ordered browser actions and condition waits](background-control.md);
> setup and desktop usage is in [the current control guide](browser-desktop.md).
> Results below belong to the original branch runs.

日期：2026-11-17。范围：开发分支 `feat/background-cua-setup`，未发布新的版本，也未在开发机替换正在运行的客户端。

## 结论

后台控制实现、可执行文件构建、浏览器环境安装与协议测试已完成。本轮相关模块的 51 项单元测试通过；真实 Playwright MCP 安装、重复安装、服务握手与工具枚举通过。完整 Rust 测试得到 **569 通过、3 失败、8 忽略**，不能将这次运行表述为全套通过。

3 项失败都停在现有管理接口创建 Unix socket 的步骤。最小环境诊断确认 `socket(AF_UNIX, ...)` 返回 `EPERM`，loopback TCP 可以创建、绑定和监听。真实 Xvfb 测试和通过 SSH/OAuth/HTTP 的浏览器端到端测试也分别在 Xvfb、守护进程启动阶段被这一限制挡住，尚未验证真实网页动作和真实独立桌面输入。

开发开始时已连接开发机并创建独立工作树，之后 ssmcp 客户端离线。源码已推送到 GitHub；开发机的安装、reload 和实机联调仍待连接恢复。没有把本轮称为完整达到 ChatGPT CUA 的功能和性能水平。

## 实现对应关系

| 需求 | 实现 | 已有证据与边界 |
| --- | --- | --- |
| 避免默认抢占鼠标 | 桌面输入默认要求独立会话；共享输入必须明确给出 `desktop:"shared"` | 路由、释放与清理回归测试通过；真实 Xvfb 输入仍待验证 |
| 网页后台操作 | 原生浏览器默认 headless，新标签默认 background；CDP 输入直接送到目标标签 | 原生协议与参数测试通过；实际 Chromium e2e 被环境阻断 |
| 提高调用效率 | `browser.act` 合并最多 16 个动作，默认返回新快照；`browser.wait` 提供条件等待 | 预验证、部分失败、排队截止与导航恢复测试通过；未给出未经实测的速度倍数 |
| setup 配好环境 | `ssmcp setup browser --install/--check/--json`，托管固定版本 Playwright MCP | 三次真实 CLI 运行通过，重复安装复用已有包 |
| 复用现有插件和登录 | Playwright 采用公开的 `--extension --background --no-tab-group`；检查已有扩展清单 | 服务握手通过；当前本地未发现可配对扩展，未连接用户标签页 |
| ChatGPT 插件复用 | 检查安装元数据；已建立的官方连接继续由 ChatGPT Browser 工具使用 | 没有实现或声称存在通用的私有扩展桥接接口 |

完整用法、平台差异与首次配对条件见[后台操作指南](background-control.md)。Linux 独立显示不构成文件访问沙箱；Windows/macOS 本轮不提供同样的 Xvfb 会话。

## 执行结果

环境为 Linux x86_64，Rust 1.100.0，开发构建。代码仍使用仓库的 `0.7.0` 包版本，没有触发发布或部署流程。

| 检查 | 实际结果 |
| --- | --- |
| `cargo build --locked --bins` | 通过，生成 `ssmcp` 与 `ssmcpd` |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过，零警告 |
| `cargo test --locked --lib` | 569 通过、3 失败、8 忽略，运行约 26.49 秒 |
| 浏览器模块 | 29 项通过 |
| 桌面模块 | 14 项通过；最终清理代码调整后再次运行仍为 14 通过、3 忽略 |
| Playwright 适配器 | 7 项通过 |
| browser setup 模块 | 1 项通过，内含参数与诊断状态断言 |
| `node --test scripts/test_cua_facade.mjs` | 6/6 通过 |
| `python3 -m py_compile scripts/e2e.py scripts/e2e_computer_use.py` | 通过 |
| `git diff --check` | 通过 |

严格 lint 首次指出独立桌面退出清理可用 `mem::take` 直接接管已有进程列表，已修正。该调整后重新执行桌面模块测试，结果为 14 通过、0 失败、3 忽略。

完整测试中未通过的 3 项为：

- `management::tests::oversized_frames_and_disabled_management_fail_closed`
- `management::tests::private_socket_enforces_scope_record_types_and_file_permissions`
- `portal::tests::dashboard_http_uses_sessions_enforces_admin_and_bounds_failure_audits`

这些测试的实现不属于本轮修改范围。失败位置均为 `ManagementServer::bind`，错误为 `Operation not permitted`。它们保留原样，没有通过跳过或降低权限检查改成绿色结果。

### 浏览器环境配置

使用独立的 `XDG_DATA_HOME` 执行实际构建产物，未修改普通浏览器 profile：

| 调用 | 退出码 | 安装状态 | 耗时 |
| --- | --- | --- | --- |
| 第一次 `setup browser --install --json` | 0 | `installed` | 26.159 秒 |
| 第二次相同调用 | 0 | `already_installed` | 1.883 秒 |
| `setup browser --check --json` | 0 | 未安装新包 | 1.275 秒 |

三次都报告 Playwright MCP `0.0.85`、`configured`、`protocol_handshake:"passed"`、28 个工具条目、`background:true`、`automatic_tab_group:false`、`browser_connection:"not_attempted"`。总体状态为 `needs_extension`，与本地未发现 Playwright 扩展一致。三份 stderr 为空，安装临时目录已清理；8 项汇总断言全部通过。这些耗时描述安装检查过程，不是浏览器操作性能基准。

### 真实运行的阻塞

真实桌面测试调用 `computer.session_start`，Xvfb 未能宣布有效显示并退出。直接运行 Xvfb 的诊断显示无法打开 local/unix 监听 socket，独立最小 socket 测试确认 `AF_UNIX` 创建被禁止。

默认 headless e2e 已通过临时 sshd 的 publickey-only 配置检查。之后 `ssmcpd` 在 HTTP listener 打开前退出，日志为 `ssmcpd: Operation not permitted (os error 1)`。其启动路径先绑定 Unix control socket，因此尚未进入 Chromium 启动或页面验证。未使用 `--desktop`，没有操作用户共享桌面。

## 本轮修正的边界行为

- 等待可见元素要求正面积布局矩形；零面积元素不再误判可见。
- `act` 的 25 秒期限和 `wait` 的毫秒期限包含等待目标标签操作锁的时间；过期后不会在释放锁时补发输入。
- 导航暂时销毁默认执行上下文时，条件等待继续轮询。
- 同标签批次持有同一把锁，其他标签仍可独立执行；部分失败返回已完成步骤和 `replay_safe:false`。
- 共享桌面的历史输入释放错误不会阻断独立桌面停止操作。
- 独立会话失败或退出不会自动选择共享桌面。

对应回归测试见[浏览器测试](../src/browser.rs)、[独立桌面测试](../src/computer/isolated.rs)和[端到端测试脚本](../scripts/e2e_computer_use.py)。端到端脚本包含零面积元素、后台标签、引用更新和真实点击计数的检查，但这些真实页面断言尚未执行到。

## 开发机恢复后的剩余验收

以下步骤属于尚未完成的验收，不应从本地协议测试推断为已通过：

1. 同步开发分支到开发机的独立工作树，构建并运行真实 Xvfb 测试，确认窗口、按键、点击、截图及退出清理。
2. 运行默认 headless `scripts/e2e_computer_use.py`，确认真实页面的后台状态、批次效果、条件等待和旧引用失效。
3. 在开发机执行 `ssmcp setup browser --install`，复用已有兼容 Playwright 扩展并完成必要的首次配对，再验证已有登录会话中的后台操作。
4. 完成以上验证后再替换已安装客户端并 reload，重新枚举工具，验证用户同时操作鼠标时没有输入冲突。

上游后台扩展语义见 [Playwright Extension 说明](https://github.com/microsoft/playwright/blob/main/packages/extension/README.md#working-in-the-background)。首次配对是否出现前台页取决于扩展、浏览器版本与操作者配置；本轮没有把它报告为完全无前台交互。

机器可读的测试汇总见[validation.json](reports/background-control-20261117/validation.json)。
