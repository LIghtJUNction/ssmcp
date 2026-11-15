# ssmcp 0.3.0 验收报告

本次更新将公开 MCP 工具收敛为 `list`、`code_mode`，支持同一 SSH 账号下的多客户端发现、原生能力查询和跨客户端调用。代码由服务器系统安装的 Node.js 执行，bubblewrap 提供进程运行环境隔离；仓库不嵌入 JavaScript 引擎，也不需要为代码模式安装 npm 依赖。已有的一次性登录码、OAuth PKCE、刷新轮换和断线撤销流程已合并保留。客户端与服务端的私有桥接协议升级为 v4。[实现入口](../src/http.rs) · [系统运行时](../src/code_mode.rs)

## 当前验证结果

截至 2026-11-16（UTC+8），包含最终系统运行时修复的提交 [`b70b0ac`](https://github.com/LIghtJUNction/ssmcp/commit/b70b0ac2f52980ccfb24233052745a065c46e678) 已在 **Linux x86_64 和 arm64 两个架构通过完整 CI**。每个架构完成 96 项默认 Rust 测试、完整系统隔离与 MCP 集成测试、133 项 SSH/OAuth 检查、189 项五模式链路检查，以及 Release 构建和安装检查。[通过的运行记录](https://github.com/LIghtJUNction/ssmcp/actions/runs/43746211798)

本机格式检查、Clippy、构建和默认 Rust 测试也已通过；本机默认测试结果为 **96 个通过、8 个显式忽略**。默认 Rust 集合的范围如下，CI 会另外执行需要系统隔离环境的测试。

| 测试集合 | 本机通过数 | 内容 |
| --- | ---: | --- |
| 库测试 | 69 | 多客户端注册与授权、MCP 工具表、OAuth、PTY、后端协议、系统 Node.js 管道、日志、错误、超时、进程回收 |
| 服务端参数 | 1 | 公网 origin 约束 |
| Claude Code fixture | 5 | 原生目录、完整结果、审批、后台输出和退出清理 |
| Codex fixture | 4 | schema、方法、事件、审批和取消 |
| Pi fixture | 5 | RPC、事件、扩展交互和进程退出 |
| OAuth HTTP | 9 | 发现、PKCE、CSRF、绑定、刷新轮换、重放和重连 |
| 客户端启动 | 3 | SSH 提前断开、部分帧、启动清理和登录码剩余时间 |
| **合计** | **96** | 不包含模型调用 |

默认忽略项共 8 个：4 个需要独立安装及配置原生 CLI 的探针，1 个需要真实 bubblewrap 命名空间的系统运行时测试，3 个需要完整系统代码运行环境的 MCP HTTP 集成测试。后四项已在两个架构的 CI 中使用专用命令执行并通过；没有通过关闭隔离来适应当前工作环境。[测试定义](../tests/two_tools.rs) · [运行时测试](../src/code_mode.rs)

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked

# 要求系统已安装 Node.js 和 bubblewrap，并允许相应命名空间。
cargo test --locked --lib code_mode::tests -- --include-ignored
cargo test --locked --test two_tools -- --include-ignored
python3 scripts/e2e.py --bin-dir target/debug
python3 scripts/e2e_agents.py --bin-dir target/debug
```

当前工作环境拒绝 Unix listener 和 bubblewrap 配置隔离网络所需的 NETLINK 操作，自动审批也拒绝了提权请求。因此完整 SSH 与 OS 隔离验证使用仓库已有的 Ubuntu x86_64 / arm64 CI，两架构均已通过。本机成功结果仅对应上表中的实际执行范围。[CI 定义](../.github/workflows/ci.yml)

## 双架构 CI 结果

运行 [43746211798](https://github.com/LIghtJUNction/ssmcp/actions/runs/43746211798) 对应提交 `b70b0ac2f52980ccfb24233052745a065c46e678`，最终状态为 `completed / success`。后续验收报告提交仅更新文档，没有更改已验证代码、依赖、服务配置或工作流。

| 检查 | [x86_64 作业](https://github.com/LIghtJUNction/ssmcp/actions/runs/43746211798/job/132909965714) | [arm64 作业](https://github.com/LIghtJUNction/ssmcp/actions/runs/43746211798/job/132909966015) |
| --- | --- | --- |
| fmt / Clippy 零警告 | 通过 | 通过 |
| 默认 Rust 测试 | 96 通过，8 ignored | 96 通过，8 ignored |
| Node.js 与 bubblewrap 运行时集合 | 8 通过，无忽略项 | 8 通过，无忽略项 |
| 两工具 MCP HTTP 集成集合 | 3 通过，无忽略项 | 3 通过，无忽略项 |
| 安装路径与 systemd 服务限制下的运行时探针 | 1 通过 | 1 通过 |
| SSH、OAuth、HTTP、PTY 与多客户端 | 133 项通过 | 133 项通过 |
| 五种后端完整 SSH/OAuth 链路 | 189 项通过 | 189 项通过 |
| Release 构建与暂存安装 | 通过 | 通过 |

上表中的运行时集合会重复执行默认集合中的 7 项测试，systemd 探针则重复验证同一个 OS 隔离测试；各行不应简单相加。除 4 个单独配置原生 CLI 的探针外，本轮所需 Rust 测试均已执行。

服务验证使用系统包安装的 Node.js 与 bubblewrap，把测试程序和运行脚本放在实际安装目录，并按仓库 systemd unit 的保护设置启动。它同时验证默认脚本定位、只读工作目录、宿主文件不可见、隔离 PID 和网络、清空环境。worker 不挂载 `/proc`，避免与 systemd 的受保护 proc 挂载冲突；服务保护设置保持有效。[运行时探针](../src/code_mode.rs) · [服务单元](../packaging/ssmcpd.service)

## 行为验收范围

| 行为 | 断言位置 |
| --- | --- |
| 对外只有两个工具，所有 `-t` 模式一致 | [HTTP 工具表与参数测试](../src/http.rs)、[五模式集成测试](../tests/two_tools.rs) |
| `list` 返回名字、ID、类型、主机和启动目录；原生 schema 支持分页 | [Broker 测试](../src/broker.rs)、[端到端脚本](../scripts/e2e.py) |
| 同名客户端不互相替换；调用按 ID 路由；账号彼此隔离 | [注册与授权测试](../src/broker.rs)、[HTTP 集成测试](../tests/two_tools.rs) |
| JavaScript 支持循环、条件、await、并发调用和日志；原生结果保留 | [运行时测试](../src/code_mode.rs)、[五模式端到端脚本](../scripts/e2e_agents.py) |
| 每次原生调用检查签发连接是否仍在线，包括 await 之后 | [运行中的授权测试](../tests/two_tools.rs) |
| 签发连接断开撤销关联授权，其他客户端的独立授权仍有效 | [OAuth 测试](../tests/oauth_security.rs)、[多客户端端到端脚本](../scripts/e2e.py) |
| Node.js 进程受系统限制，不能访问宿主账号文件、工作目录或网络 | [独立 OS 隔离探针](../src/code_mode.rs)；两架构普通环境与 systemd 服务环境均通过 |
| 关闭后端前先发现并停止子进程，避免 EOF 导致子进程被重新挂接后漏清理 | [进程管理](../src/backend/process.rs)、[顺序回归测试](../src/backend/rpc.rs) |

## 安装与使用

Debian/Ubuntu 服务端使用 `sudo apt install nodejs bubblewrap` 安装代码运行依赖，Arch Linux 使用 `sudo pacman -S nodejs bubblewrap`，Fedora 使用 `sudo dnf install nodejs bubblewrap`。系统运行时由发行版包管理器更新。`make install` 安装桥接脚本到 `$PREFIX/libexec/ssmcp/code-mode.js`；仅运行 ssmcp 客户端不需要这两项服务端依赖。[安装步骤](../README.md#安装与升级)

同一账号可以启动多个命名客户端；先通过 `list` 找到 ID 和能力 schema，再通过 `code_mode` 的 `call` 访问选定客户端。`cwd` 是启动时快照。升级时同时更新 ssmcp 和 ssmcpd，重新连接并完成 OAuth 登录，刷新 MCP 工具表。[调用示例](backends.md) · [部署说明](deployment.md)

本轮端到端 Agent 验证使用本地协议 fixture，不发起付费模型推理，也不表示所有模型、账号或插件组合已经验证。生产 DNS、HTTPS、第三方 MCP 客户端的浏览器交互和实际服务器部署不属于本次执行范围。上游 0.2.0 的真实原生 CLI 探针记录保留在[原版本报告](https://github.com/LIghtJUNction/ssmcp/blob/991cabed2535be440f9523d779a85bbbef5eafad/docs/validation.md)，其结果不替代本版本系统运行时验收。
