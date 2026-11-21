# 用 `--load-harness` 加载原生协议

`--load-harness` 在客户端启动时加载一个明确指定的协议适配器。原生命令、文件和 Zuse CLI 管理工具继续使用同一份 runtime 与工具目录；适配器完成握手和工具枚举后，连接才报告就绪。启动过程不提交 prompt 或模型回合。需要执行任务时，调用已枚举的原生工具，并遵循该工具的参数和审批流程。[入口](../src/client/agent.rs) · [加载与关闭](../src/runtime/native_plugins/process.rs)

`--agent` 单独选择聊天消息的处理方，默认 `next-caller`。两项可同时使用，例如 `ssmcp local --agent next-caller --load-harness codex`。聊天输入和收件人见[客户端输入](client-input.md)。

## 选择一个 provider

`--list-harnesses` 显示本版本支持的 provider 和别名，无需登录，也不探测本机程序。普通启动保持按需加载；添加 `--load-harness` 后，只初始化选中的一项。

```sh
ssmcp --list-harnesses
ssmcp --list-harnesses --json

ssmcp alice@relay.example --load-harness codex
ssmcp local --load-harness claude-code
ssmcp --context work --load-harness codex

# 只检查原生协议和实际工具目录，完成后退出。
ssmcp --list-tools --load-harness claude-code
```

| 名称 | 别名 | 启动时连接的接口 | 参数来源 |
| --- | --- | --- | --- |
| `codex` | — | 已安装 `codex` 的 App Server stdio 接口 | `--` 后的参数追加到协议进程 |
| `claude` | `claude-code`、`cc` | 已安装 `claude` 的 `mcp serve` 接口 | `--` 后的参数追加到协议进程 |
| `pi` | — | 已初始化的本机私有 RPC socket | `SSMCP_PI_RPC_SOCKET` |
| `opencode` | — | 已运行的原生 loopback HTTP 服务 | `SSMCP_OPENCODE_URL`、`OPENCODE_PASSWORD` |
| `ssmcp` | — | 明确指定的 MCP peer 网关 | `--` 后的 `--endpoint`、`--token-env`，可选 `--socket` |

表中的前四项复用现有适配器，包括程序解析、协议校验、原生审批和退出清理。Pi 与 OpenCode 连接操作者已准备好的服务；这两项收到额外进程参数会直接报错，避免参数被忽略。实际支持的工具以握手后返回的目录为准。[适配器](../src/backend.rs) · [按需加载](optional-harnesses.md)

## 用 `--` 分开两层参数

`--` 前属于 ssmcp；`--` 后完整交给选中的协议适配器。一次连接只接受一个 `--load-harness`。Codex、Claude Code 的参数必须适用于它们各自的协议服务入口，具体参数由已安装版本校验。

```sh
# --name 命名 ssmcp 连接；--verbose 传给 Claude Code 的 MCP 服务。
ssmcp alice@relay.example --name review --load-harness claude-code -- --verbose

# 原有连接上下文继续选择中继；额外参数仍需要 --。
ssmcp --context work --load-harness codex -- --config approval_policy=on-request
```

参数中的中文、空字符串以及看起来像 ssmcp 选项的值都保持原样。保存后台会话时，会记录 provider 和参数分界；重连继续使用该选择。旧 `--type`、`--backend` 和 `-t` 入口仍会被拒绝。旧版协议保存记录如果只有 `--agent`，恢复会提示重新创建。预加载请用 `--load-harness`，要指定聊天处理方则显式配置受支持的输入模式与 `--agent`。[参数与会话记录](../src/client.rs)

## 连接另一个 ssmcp 的 MCP 网关

`--load-harness ssmcp` 接受 peer 公布的完整 MCP URL，例如 `https://peer.example/alice`。URL 指向该账号的网关；具体设备和线程需要从 peer 的目录中选择，再使用实际 ID 路由。接入完成后，本机目录以 `peer.` 前缀发布远端实际返回的工具，并保留其 schema、返回内容和错误。[peer 实现](../src/backend/peer.rs)

认证使用 peer 的 MCP OAuth 流程签发的 bearer。`--token-env` 指定保存 bearer 的环境变量名；命令行、会话记录和诊断中不保存 token 值。以下 Bash 示例从终端读入 token，避免将值写进命令历史：

```bash
read -r -s -p 'Peer OAuth bearer: ' SSMCP_PEER_TOKEN
export SSMCP_PEER_TOKEN
printf '\n'

ssmcp local --load-harness ssmcp -- \
  --endpoint https://peer.example/alice \
  --token-env SSMCP_PEER_TOKEN

unset SSMCP_PEER_TOKEN
```

HTTPS 使用项目既有的证书信任配置。明文 HTTP 仅接受字面 loopback IP；URL 中拒绝用户名密码、query 和 fragment。请求保持目标 URL 的 Host 和 Origin，经过 peer 原有的 OAuth 与授权检查；重定向被拒绝，bearer 留在明确指定的目标上。

Unix socket 传输使用本地 listener 实际显示的 HTTP URL。若 peer 以 `ssmcp local --socket` 开启 MCP socket，可在同一用户下连接它：

```sh
ssmcp local --load-harness ssmcp -- \
  --endpoint http://127.0.0.1:43123/alice \
  --token-env SSMCP_PEER_TOKEN \
  --socket /absolute/private-directory/mcp.sock
```

socket 和父目录必须由当前用户拥有且保持私有；路径禁止符号链接。`--socket` 与 HTTPS URL 会报错，以保留 TLS 的真实含义。socket 只改变连接传输，Host、Origin 和 bearer 仍对应给定 URL。

peer 连接不启动另一个 ssmcp 进程，也不拥有远端进程。客户端会拒绝本次连接自己公布的完整 MCP URL；同一主机的不同账号路径仍可明确选择。此检查针对直接自连接，不替代远端账号权限，也不为任意跨网关调用链做自动选路。关闭本机连接会停止其拥有的请求，peer 网关继续运行。

## 错误与验证范围

provider 缺失、原生握手失败、认证拒绝或目录无效时，启动返回具体错误；前台连接继续遵循已有 watchdog 策略。可使用 `--list-tools` 单独检查所选协议，也可添加 `--no-watchdog` 让连接失败后退出。

peer 使用 JSON-response MCP，协商 2025 系列协议。单个请求最多 1 MiB、单个响应最多 2 MiB；目录最多 64 页、4,096 个工具、合计 8 MiB。初始化和通知各最多等待 10 秒，单页目录 15 秒、整份目录 75 秒；显式工具调用最多等待 900 秒。超时返回错误，不将 ACK、空响应或伪造的本地返回值当作远端工具结果。

回归检查覆盖 CLI 参数、保存会话、原生假进程，以及私有 HTTP/Unix 协议服务。实际 CLI 脚本使用独立 HOME 和配置目录，核对真实 argv、握手、目录、连接就绪与子进程退出，并断言启动没有提交模型回合：

```sh
cargo test --lib client::agent::tests
cargo test --lib peer_
python3 scripts/e2e_agent_entry.py \
  --binary /absolute/path/to/built/ssmcp \
  --output /absolute/path/to/new-evidence-directory
```

这些用例使用协议 fixture；不同真实 CLI 版本的工具、账户、模型权限和原生审批由各自实现决定。peer 用例核对完整返回内容、工具错误和 JSON-RPC 错误，并覆盖错误认证、重复游标、重定向、响应上限、超时及关闭时的取消。[CLI 回归](../scripts/e2e_agent_entry.py) · [协议回归](../src/backend/peer.rs)
