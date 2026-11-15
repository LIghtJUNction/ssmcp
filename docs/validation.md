# ssmcp 0.2.0 验收报告

本次实现包含五种本机后端、SSH 公钥身份验证、一次性动态登录码与 OAuth 登录，内部协议为 wire v3。本机最终完整检查中，格式检查、Clippy 和构建通过，Rust 测试为 **82 个通过、4 个原生 CLI 探针默认忽略**，包含新增的进程清理和启动断线回归。Claude Code、Pi 和 OpenCode 的真实 CLI 无模型探针已单独通过；Codex 的真实命令执行受到当前运行环境限制。

完整 SSH 与 OAuth 端到端脚本已写入 CI，**本版本的最终 CI 结果仍待确认**。当前工作环境禁止绑定 Unix socket，无法在这里启动实际控制 socket；真实浏览器端到端操作也尚未完成。下文分别记录已执行的证据和需要后续环境完成的验证，不沿用旧版本的 CI 结果。

## 1. 本机 Rust 检查

最终完整执行的 `cargo fmt --all -- --check`、`cargo clippy --offline --locked --all-targets -- -D warnings`、`cargo test --offline --locked` 和 `cargo build --offline --locked` 均通过，下表记录包含所有回归修复的运行结果。`--offline` 使用当前环境已有的依赖缓存；首次在新机器构建时应按仓库锁文件下载依赖。

| 测试集合 | 通过数 | 验证范围与源码 |
| --- | ---: | --- |
| 库测试 | 55 | PTY、wire、MCP HTTP、OAuth 状态与协议、事件缓存、后端适配和进程管理；[源码](../src/) |
| 服务端参数测试 | 1 | 公网 origin 解析与约束；[ssmcpd](../src/bin/ssmcpd.rs) |
| Claude Code 集成测试 | 5 | 原生目录和结果保留、请求回复、后台输出、进程退出；[测试](../tests/cc_backend.rs) |
| Codex 集成测试 | 4 | 版本 schema、原生方法与事件、双向请求、并发取消；[测试](../tests/codex_backend.rs) |
| OAuth HTTP 安全测试 | 9 | 发现、表单、PKCE、绑定、重放、刷新与重连隔离；[测试](../tests/oauth_security.rs) |
| Pi 集成测试 | 5 | 原生响应与事件、扩展 UI、未来 RPC 参数、退出清理；[测试](../tests/pi_backend.rs) |
| 客户端启动集成测试 | 3 | SSH 提前退出、部分帧、启动取消清理与登录码剩余时间；[测试](../tests/client_startup.rs) |
| **合计** | **82** | 默认测试不调用真实模型 |

四个真实 CLI 探针采用显式 `ignored` 测试，需要单独选择已安装的可执行文件与隔离配置。因此，默认测试中的 `4 ignored` 不能解读为真实 Agent 已被该次 `cargo test` 全部验证；单独执行情况见下一节。

## 2. 真实原生 CLI 探针

原生接口验证使用临时配置和测试项目，没有发起模型推理任务。工具目录数量记录的是当前安装版本公开的接口；数量本身不能证明所有模型、账号、插件和原生 API 的每一种行为都已运行。

| 本机 CLI | 目录快照 | 实际验证结果 |
| --- | --- | --- |
| Codex `0.167.0-alpha.4` | 生成 182 个客户端方法；桥处理 `initialize`，公开 181 个方法，加 5 个辅助工具，共 186 项 | 原生 schema 生成、Rust 适配器目录发现、`configRequirements/read` 的 null 参数及 `server/diagnostics` 通过；真实 `command/exec` 因宿主环境的 `NETLINK_ROUTE` 限制失败，原生错误完整返回 |
| Claude Code `2.1.325` | 22 个原生 MCP 工具，包括 `Agent`；加 3 个辅助工具，共 25 项 | Rust 适配器调用真实 `mcp serve`，验证目录、文件读取、Bash、后台输出增量跟随，以及仍在运行的 Bash / sleep 进程在断开后退出；没有调用 `Agent` 执行模型任务 |
| Pi `1.5.2` | 33 个原生控制命令，加通用 RPC、events、requests、respond，共 37 项 | Rust 适配器验证状态、扩展命令发现、本机 Bash 与增量事件、显式拒绝扩展确认；独立原生 RPC 探针另通过 16 项检查，包括队列、会话、错误与 EOF 退出 |
| OpenCode `2.3.0` | 170 个 OpenAPI 操作，包含 HTTP、SSE 和 3 个 WebSocket 操作；加 5 个辅助工具，共 175 项 | Rust 适配器验证私有服务启动、目录、session 创建和删除、权限 ask/list/reject、SSE、Shell printf、二进制读写与 HTTP Range 206；真实 PTY WebSocket 连接、发送、接收、关闭及删除均通过 |

各探针入口保留在 [Codex 测试](../tests/codex_backend.rs)、[Claude Code 测试](../tests/cc_backend.rs)、[Pi 测试](../tests/pi_backend.rs) 和 [OpenCode 适配器测试](../src/backend/opencode.rs)。Pi 的独立无模型 RPC 探针见 [smoke_pi.py](../scripts/smoke_pi.py)。原生协议和调用范围见[后端说明](backends.md)。

### Codex 环境限制

真实 Codex 命令返回 `exitCode:1`，stderr 为 `bwrap: loopback: Failed to create NETLINK_ROUTE socket: Operation not permitted`。这说明请求已到达原生 Codex，并正确带回失败结果；当前环境没有完成该命令的成功执行及后续真实输出流断言。对应的原生 smoke 测试在这里未通过，不能用其他后端或模拟进程的成功结果替代。[原生 smoke 的断言](../tests/codex_backend.rs)

Codex 的四项协议 fixture 测试已通过，覆盖方法发现、参数和事件转发、显式审批回复、长调用并发终止等桥接逻辑。真实 Codex 的命令执行与模型任务仍需在允许其原生沙箱正常工作的客户端环境验证。

### 原生行为的实际边界

Claude Code 前台 Bash 的输出在原生调用完成后返回；实时增量跟随依赖后台模式返回的 `backgroundOutputPath`。本轮真实探针还观察到 `TaskStop` 对新返回的后台任务 ID 报 `No task found with ID`，因此没有把原生任务停止能力记为通过。ssmcp 自身的断开清理已经针对仍在运行的 Bash 和 sleep 验证。[测试与清理断言](../tests/cc_backend.rs)

Pi 的普通 `prompt` 只报告接受或排队状态，任务完成应观察 `agent_settled`。探针使用本机扩展完成确认流程，不需要运行模型。Pi 1.5.2 的启动扩展如果在挂接 RPC 输入前等待 UI，仍可能导致原生启动阻塞；文档保留该兼容边界。[Pi 说明](backends.md#pi)

## 3. OAuth 与 HTTP 验证

九项独立 HTTP 测试通过真实 Axum 路由与请求/响应对象验证授权流程，底层 SSH 注册使用测试连接，不依赖受限制的 Unix listener。它们验证 HTTP 与 OAuth 语义，没有模拟浏览器已经完成交互。[OAuth HTTP 测试](../tests/oauth_security.rs)

| 已验证内容 | 具体断言 |
| --- | --- |
| 自动发现与表单 | 未授权 MCP 请求的发现信息、精确资源 URL、单一可见登录码输入框、隐藏事务标识、Cookie 安全属性、禁止缓存与 referrer 泄漏 |
| 登录码使用范围 | SSH 返回的动态登录码不能作为 MCP Bearer token；成功授权后一次性消耗 |
| 回调与 PKCE | 未登记回调和 PKCE 降级被拒绝，错误请求不形成任意重定向；授权码绑定客户端、resource、回调和 verifier |
| CSRF 与并发 | 缺失浏览器绑定、跨来源提交、重复表单及同时兑换不能产生独立有效授权 |
| 刷新与重放 | refresh token 轮换，旧令牌重放撤销授权族；错误的其他客户端或资源请求不能借此撤销目标授权 |
| 断线与重连 | 原 SSH Session 失效后 OAuth 授权失效；相同用户名的新连接不能继承旧授权 |
| 客户端认证 | confidential client 的认证不能被降级或混用；不合法用户名不会插入认证挑战头 |

OAuth 库测试同时检查过期、容量、CSP 与回调输出等边界。生命周期与部署参数以[部署文档](deployment.md#oauth-兼容性)和[实现](../src/oauth.rs)为准：登录码最多 600 秒，authorization code 最多 60 秒，access token 最多 1 小时；登记刷新能力的客户端可轮换 refresh token，同一授权最多 24 小时。

实际浏览器尝试无法访问当前环境的本机 HTTP 探针，因此没有完成浏览器提交、回调和 MCP 客户端恢复连接的整段交互，也没有成功交互的截图证据。当前页面证据来自 HTTP、HTML 和响应头测试；正式浏览器兼容性需在可访问的 HTTPS 部署上继续验证。

## 4. 完整 SSH 端到端验证状态

`scripts/e2e.py` 和 `scripts/e2e_agents.py` 使用真实 OpenSSH、临时 Ed25519 密钥、临时 known_hosts、独立 sshd 配置和随机 loopback 端口。脚本不读取用户已有 SSH 身份，不更改系统 SSH 配置；Agent 模式采用本地协议 fixture，不调用模型。

| 脚本 | 计划执行的完整链路 | 当前本机结果 |
| --- | --- | --- |
| [e2e.py](../scripts/e2e.py) | 公钥认证、OAuth、MCP 2025 / 2026、真实 PTY、并发、断线、刷新与重放 | 控制 socket 的 Unix bind 返回 `EPERM`，守护进程健康检查无法通过；完整链路未完成 |
| [e2e_agents.py](../scripts/e2e_agents.py) | 依次运行 shell / codex / cc / pi / opencode，逐次 OAuth 登录、目录分页、原生调用、实时日志、元数据、请求回复、撤销及子进程退出 | Unix bind 预检查返回 `PermissionError: Operation not permitted`，按此结果停止，未启动完整链路；报告状态为 `blocked` |

Unix socket pair 在本机可用，因此相关 Rust 协议和路由测试能够执行；创建具名 Unix listener 被环境禁止。这个限制解释了本机单元及集成测试通过与完整 SSH 脚本尚未运行之间的差别。

OpenCode 端到端 fixture 已先完成本机独立验证：9 项 HTTP、SSE、Basic 认证和 EOF 清理检查通过；真实 Rust 适配器连接该 fixture，成功发现 78 个工具，并确认后端及其子进程退出。其 73 个原生操作加 5 个辅助工具会触发 MCP 的两页目录。[fixture](../tests/fixtures/opencode_http.py)；[五模式断言](../scripts/e2e_agents.py)

## 5. CI 状态

本版最终 CI **待确认**，不引用此前版本的运行结果。工作流已配置 Linux x86_64（`ubuntu-24.04`）与 arm64（`ubuntu-24.04-arm`），并在每个架构顺序执行格式检查、Clippy、默认 Rust 测试、构建、两份真实 SSH/OAuth 端到端脚本、Release 构建及暂存安装检查。[工作流](../.github/workflows/ci.yml)

最终通过状态应对应包含本次 OAuth、五模式和 wire v3 代码的具体提交及运行记录。可从 [GitHub Actions](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml) 查看当前运行；待该提交完成后，再把运行链接和各架构结果补入本节。

## 6. 生命周期与部署边界

断开时，ssmcp 跟踪并终止本次创建且仍归属的后端及其子进程。测试包含通过 `setsid` 新建 session、仍在本次父子关系内的子任务，也覆盖显式 shutdown 与 `Peer` 自动析构。已主动转交外部服务管理器的进程、Agent 已保存的文件和会话、远端服务上的副作用不会因断开而回滚。[进程管理](../src/backend/process.rs)；[RPC 生命周期](../src/backend/rpc.rs)；[PTY](../src/pty.rs)

获得 OAuth 授权的 MCP 客户端可以调用当前后端公开的能力，权限来自本机运行用户和原生 Agent。Claude Code 的 MCP 服务由调用方负责逐次工具确认；ssmcp 不自动批准原生请求。真实模型、账号限制、项目权限以及所有插件的组合行为没有在本轮无模型探针中穷举。

`ssmcp.xxxx.yyy` 是占位域名，本次没有在该域名部署公网服务。实际 DNS、TLS 证书、反向代理、发行版 SSH 账户策略和真实 MCP 客户端 OAuth 兼容性仍需在部署环境检查。CI 的 HTTPS origin 是协议配置，测试传输走隔离的 loopback HTTP，不能据此宣称公网 TLS 已验收。

## 7. 复现

在支持 Unix listener、PTY 和 pidfd 的 Linux 环境安装 OpenSSH server 后，可以运行默认验证；脚本自行生成临时密钥和配置：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/e2e.py --bin-dir target/debug --report artifacts/oauth-shell-e2e.json
python3 scripts/e2e_agents.py --bin-dir target/debug --report artifacts/oauth-agents-e2e.json
cargo build --locked --release
make install-server DESTDIR=/tmp/ssmcp-install
```

JSON 报告只在脚本成功完成后由脚本输出，不包含登录码、access token 或 refresh token。本次环境阻断报告另行明确记录 `blocked`，没有把缺失运行结果写成成功。原生 CLI 的四项 ignored 探针需要按各测试源码准备明确可执行路径和隔离配置，再选择对应测试运行；它们不是默认 CI 的模型任务测试。

客户端、后端和公网部署的操作步骤见 [README](../README.md)、[后端说明](backends.md)与[部署说明](deployment.md)。
