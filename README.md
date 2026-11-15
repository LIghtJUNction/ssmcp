# ssmcp

通过一条 SSH 公钥连接，把本机 Shell、Codex、Claude Code、Pi 或 OpenCode 提供给远程 MCP 客户端。客户端 `ssmcp` 和服务端 `ssmcpd` 均使用 Rust；命令与 Agent 在你的 Linux 电脑执行，服务器负责认证和转发。

```sh
ssmcp alice@ssmcp.xxxx.yyy                 # 默认 shell
ssmcp -t codex alice@ssmcp.xxxx.yyy
ssmcp -t cc alice@ssmcp.xxxx.yyy
ssmcp -t pi alice@ssmcp.xxxx.yyy
ssmcp -t opencode alice@ssmcp.xxxx.yyy
```

服务器预先配置你的 SSH 公钥。连接及本机后端初始化成功后，stdout 输出一行 JSON：

```json
{"url":"https://ssmcp.xxxx.yyy/alice","login_code":"本次连接的一次性动态令牌","login_expires_in":600,"type":"shell"}
```

在支持 Streamable HTTP 与 OAuth 的 MCP 客户端中填写 `url`，启动登录。浏览器页面显示包含用户名的目标 MCP URL、请求连接的客户端及回调来源；确认后，把 `login_code` 粘贴到唯一的输入框并提交。页面成功后自动返回 MCP 客户端，客户端取得访问令牌并连接。

`login_code` 是签发后有效 10 分钟、成功使用一次即失效的登录码，只能用于这个 OAuth 页面。`login_expires_in` 会扣除本机后端初始化耗时，表示剩余有效秒数。访问令牌由 OAuth 流程单独签发，MCP 客户端负责保存和续期。本机终端的 stderr 持续显示远程输入、执行结果和原生 Agent 事件；保持 `ssmcp` 运行。本机按 `Ctrl-C` 会断开连接、停止本次后端并撤销本次授权。

`ssmcp.xxxx.yyy` 是占位域名，部署时替换为你控制的实际域名。[部署说明](docs/deployment.md)包含服务器配置；[后端说明](docs/backends.md)包含各模式的完整调用方法。

## 五种模式

“后端”指本次连接在本机启动的 Shell 或 Agent 进程。一个服务端用户名同时对应一个后端；使用相同用户名重复连接会被拒绝，已有连接保持可用。

| `-t` | 本机接口 | 对外提供的能力 | 本轮目录快照 |
| --- | --- | --- | --- |
| `shell`，默认 | 独立 PTY | `terminal.read/write/resize/status` | 4 个工具 |
| `codex` | Codex App Server | 按本机版本生成全部客户端 API schema，转发事件与服务端请求 | `0.167.0-alpha.4`：181 个 API 方法 + 5 个辅助工具 |
| `cc` | `claude mcp serve` | 动态发现全部原生 MCP 工具，包括 `Agent` | `2.1.325`：22 个原生工具 + 3 个辅助工具 |
| `pi` | `pi --mode rpc` | 33 个原生控制命令、通用 RPC、事件与扩展对话 | `1.5.2`：37 个工具 |
| `opencode` | OpenCode v2 本机服务 | 动态映射 OpenAPI 操作，支持 HTTP、SSE、WebSocket | `2.3.0`：170 个操作 + 5 个辅助工具 |

工具数是已验证版本的快照，实际目录以 `--list-tools` 或远程 `tools/list` 为准。Codex 的 181 项是线程、回合、命令、文件、配置等 **API 方法**；Agent 在任务中使用的内部工具仍由原程序管理。各接口的范围与限制见[后端说明](docs/backends.md)。

## 安装

运行需要 Linux 5.3 或更新内核和 OpenSSH；从源码构建使用仓库固定的 Rust 1.100.0。客户端使用 pidfd 跟踪本次进程，服务器使用系统 `sshd` 验证公钥。

```sh
cargo build --locked --release
sudo make install
```

`make install` 默认安装到 `/usr/local`，支持 `PREFIX` 和 `DESTDIR`。服务器只需要 `ssmcpd`、OpenSSH 和 HTTPS 反向代理；各 Agent 安装、登录、模型和工具配置都放在运行 `ssmcp` 的本机。

### Arch Linux / AUR 构建文件

仓库包含 [ssmcp-git 的 PKGBUILD](packaging/aur/PKGBUILD)。在 Arch Linux 上可从仓库内构建安装：

```sh
cd packaging/aur
makepkg -si
```

该构建文件从 GitHub 拉取源码，安装到 `/usr`，并提供 systemd 服务和 sysusers 配置。这里的入口是仓库内构建文件；包的发布状态不作为运行前提。

## 客户端用法

先切换到需要交给远程工具操作的本机项目目录，再启动相应模式。Agent 使用本机已有的安装与配置；模型调用需要该 Agent 的有效登录或 provider 配置。

```sh
# 默认 Shell，也可明确指定类型。
ssmcp -t shell --shell /bin/bash alice@ssmcp.xxxx.yyy

# SSH 密钥、端口、独立配置文件。
ssmcp -i ~/.ssh/id_ed25519 -p 2222 alice@ssmcp.xxxx.yyy
ssmcp -F ~/.ssh/ssmcp_config -t codex alice@my-server

# 选择本机 Agent 可执行文件。
ssmcp -t pi --backend /opt/pi/bin/pi alice@ssmcp.xxxx.yyy

# -- 后的参数原样传给所选 Agent；此例创建不落盘的 Pi 会话。
ssmcp -t pi alice@ssmcp.xxxx.yyy -- --no-session

# 本地发现工具，输出 {type,tools} 后退出；不连接 SSH，不带 user@host。
ssmcp -t codex --list-tools
ssmcp -t cc --list-tools
ssmcp -t pi --list-tools -- --no-session
ssmcp -t opencode --list-tools
```

`--list-tools` 会启动、探测并关闭本机后端，可先用它检查可执行文件和接口兼容性。`--shell` 只用于 Shell；`--backend` 与 `--` 后的参数用于 Agent。stdio、监听地址等传输选项由 ssmcp 管理，原生配置参数的范围见[后端说明](docs/backends.md#启动参数)。

SSH 只允许公钥认证，并保留 OpenSSH 的 known_hosts 主机密钥检查。加密私钥的口令由 OpenSSH 或 `ssh-agent` 处理；服务器账户密码和键盘交互认证均被禁用。

## 从 MCP 执行

远程先调用 `tools/list` 获得当前模式的工具 schema，再调用 `tools/call`。Shell 模式写入 `{"data":"pwd\n"}` 到 `terminal.write`，通过 `terminal.read` 读取结果；本机终端同时实时显示输入与输出。

```json
{"name":"terminal.write","arguments":{"data":"pwd\n"}}
```

```json
{"name":"terminal.read","arguments":{"cursor":0,"wait_ms":1000}}
```

以上 JSON 是 `tools/call` 的 `params`。后续读取携带返回的 `next_cursor`。远程写入 `{"data":"\u0003"}` 可中断 Shell 前台命令；本机按 `Ctrl-C` 则结束整个 ssmcp 连接。

Agent 模式使用各自原生接口：Codex 创建 thread 后启动 turn；Claude Code 可调用 `cc.Agent` 或原生工具；Pi 使用 `pi.prompt` 并读取事件；OpenCode 使用 session API。具体示例、完成信号和对话回复格式见[后端说明](docs/backends.md)。

## 连接与权限

服务器为每次 SSH 连接生成一次性动态登录码，并把 endpoint 放在已认证用户名对应的 `/user` 路径下。用户名由 `ssmcpd attach` 的 Unix socket 内核凭据确定。登录码通过已认证 SSH 通道回到本机；服务器不需要安装 Agent，本机无需开放公网监听端口。

OAuth 使用 Authorization Code 与 S256 PKCE，提供自动发现和动态客户端注册（DCR）。MCP 客户端需支持这套流程；当前不支持 Client ID Metadata Documents。access token 最长有效 1 小时；客户端注册时声明 `refresh_token` grant 才会取得刷新令牌，每次刷新轮换，同一授权最长持续 24 小时。登录码不能用作 `Authorization: Bearer` 凭据。[OAuth 部署与兼容性](docs/deployment.md#oauth-兼容性)

授权后的 MCP 客户端可以调用所选后端提供的能力。ssmcp 保留原生接口的权限行为，不自动批准 Agent 发来的请求。Claude Code 的 `mcp serve` 由调用方 MCP 客户端负责逐次工具确认，不能把它当成交互式 TUI 的确认流程。[官方说明](https://code.claude.com/docs/en/mcp#use-claude-code-as-an-mcp-server)

HTTPS 保护 MCP 客户端到服务器的通信，SSH 保护服务器到本机的通信。服务器处理授权和请求内容，应部署在你信任的主机上。退出、SSH 断开或守护进程重启后，本次登录码、access token 和 refresh token 全部失效；本机清理本次后端及普通子任务。Agent 已保存的会话和文件按原程序规则保留。

## 协议与升级

**0.2.0 使用内部 wire v3，客户端与服务端必须一起升级。** 0.1 的连接凭据和早期开发快照的 wire v2 都不兼容；升级后重新连接并通过 OAuth 登录。SSH 桥单帧上限为 4 MiB，工具目录支持分页，实时事件使用有界缓存。完整限制与超时行为见[后端说明](docs/backends.md#传输与资源边界)。

公网入口支持 MCP `2026-07-28` 的 `server/discover`，以及 `2025-03-26`、`2025-06-18`、`2025-11-25` 的 `initialize`。MCP 响应使用 JSON，通知返回 `202`，已授权的 MCP 资源路径不提供 `GET` / `DELETE`，返回 `405`；Agent 的流式进度通过事件工具读取。

## 开发与验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/e2e.py --bin-dir target/debug
python3 scripts/e2e_agents.py --bin-dir target/debug
```

端到端脚本使用临时 SSH 密钥和本机测试服务；`e2e_agents.py` 在五种模式中走 OAuth 和完整转发链路，Agent 使用本地协议 fixture，不调用模型。真实原生 Agent 的独立验证使用单独配置，不依赖生产服务器。具体执行环境、版本、已通过项目和未验证范围见[验收报告](docs/validation.md)。

## 参考

- [MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
- [MCP HTTP 授权](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)
- [OpenSSH ssh_config](https://man.openbsd.org/ssh_config) / [sshd_config](https://man.openbsd.org/sshd_config)
- [Codex App Server](https://developers.openai.com/codex/app-server/)
- [Claude Code MCP 服务](https://code.claude.com/docs/en/mcp#use-claude-code-as-an-mcp-server)
- [Pi RPC](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md)
- [OpenCode v2 API](https://opencode.ai/docs/api)

MIT License.
