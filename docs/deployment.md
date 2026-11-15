# Linux 部署

服务器部署 OpenSSH、`ssmcpd` 和 HTTPS 反向代理；本机部署 `ssmcp` 及需要控制的 Agent。服务器不运行 Codex、Claude Code、Pi 或 OpenCode，模型身份和项目配置都留在本机。以下以 systemd Linux、`/usr/local` 前缀和服务端账户 `alice` 为例。

ssmcp 0.3.0 使用内部 wire v4，对外只提供 `list`、`code_mode` 两个 MCP 工具。**升级时同时更新客户端和服务端**，重启守护进程后重新连接，并在 MCP 客户端中完成 OAuth 登录、刷新工具表。旧版私有桥接协议不兼容；服务器重启会撤销当前在线入口及其全部授权。

## 1. 安装程序与服务账户

客户端需要 Linux 5.3 或更新内核以使用 pidfd；源码构建使用仓库固定的 Rust 1.100.0。服务端通过系统安装的 Node.js 18+ 执行代码，通过 bubblewrap 0.8+ 创建隔离环境；依赖由发行版包管理器安装和更新：

```sh
# Debian / Ubuntu
sudo apt install openssh-server nodejs bubblewrap
# Arch Linux
sudo pacman -S openssh nodejs bubblewrap
# Fedora
sudo dnf install openssh-server nodejs bubblewrap
```

只运行本机 `ssmcp` 客户端时无需 Node.js 或 bubblewrap。构建并安装服务端文件：

```sh
cargo build --locked --release
sudo make install-server
sudo groupadd --system ssmcp
sudo useradd --system --gid ssmcp --no-create-home \
  --home-dir /nonexistent --shell /usr/sbin/nologin ssmcpd
```

已有同名组或服务账户时复用即可。`make install-server` 安装二进制、`$PREFIX/libexec/ssmcp/code-mode.js`、man page、配置示例和 systemd unit，不自动启用服务。Arch Linux 使用仓库内 `packaging/aur/PKGBUILD` 时安装前缀为 `/usr`，其 sysusers 配置提供服务账户定义；相应二进制路径使用 `/usr/bin`。

守护进程以独立的 `ssmcpd` 用户运行。systemd 创建 `/run/ssmcpd`，权限为 `0750`；控制 socket 使用 `0660` 和 `ssmcp` 组。该组中的账户可以通过 SSH 注册自己的 MCP endpoint。[服务配置](../packaging/ssmcpd.service)

编辑 `/etc/ssmcpd/ssmcpd.env`：

```ini
SSMCP_LISTEN=127.0.0.1:8080
SSMCP_PUBLIC_URL=https://ssmcp.xxxx.yyy
```

将域名替换为实际域名。公网 URL 只包含 `https://`、主机名和可选端口，不带用户路径、查询参数或 fragment；服务端根据已认证用户名追加 `/alice`。

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now ssmcpd
sudo systemctl status ssmcpd
```

`journalctl -u ssmcpd` 查看守护进程状态。守护进程日志不记录动态登录码、access token、refresh token 或后端执行数据；本机 `ssmcp` 的 stderr 是实时活动日志，stdout 包含连接 URL 和动态登录码。

## 2. 配置 SSH 公钥账户

为允许连接的用户创建服务端普通账户，加入 `ssmcp` 组，并安装公钥：

```sh
sudo useradd --create-home --user-group --shell /bin/sh --groups ssmcp alice
sudo install -d -m 0700 -o alice -g alice /home/alice/.ssh
sudo install -m 0600 -o alice -g alice alice.pub /home/alice/.ssh/authorized_keys
```

已有账户使用 `sudo usermod -aG ssmcp alice`。`alice.pub` 是事先取得的公钥文件，私钥留在客户端。账户需要满足发行版的账户和 PAM 登录策略，并有可执行的登录 shell，供 OpenSSH 执行固定桥接命令。

安装仓库中的 SSH 配置片段：

```sh
sudo install -m 0644 packaging/sshd-ssmcp.conf /etc/ssh/sshd_config.d/60-ssmcp.conf
sudo sshd -t
sudo sshd -T -C user=alice,host=localhost,addr=127.0.0.1
```

确认主配置包含 `Include /etc/ssh/sshd_config.d/*.conf`。本例使用包含 `Match Group` 的配置片段，建议使用 OpenSSH 8.4 或更新版本，或已包含相应修复的发行版版本；8.4 修复了 `Include` 与后续 `Match` 的解析问题。[OpenSSH 8.4 发布说明](https://www.openssh.org/txt/release-8.4)

上述有效配置输出应至少包含：

```text
authenticationmethods publickey
pubkeyauthentication yes
passwordauthentication no
kbdinteractiveauthentication no
forcecommand /usr/local/bin/ssmcpd attach
disableforwarding yes
permittty no
permituserrc no
```

`Match Group ssmcp` 把该组的 SSH 登录固定到 `ssmcpd attach`，包括普通 SSH 命令和 SFTP 请求；组外账户保留原策略。`DisableForwarding` 关闭 TCP、Unix socket、agent 和 X11 转发。原生 Agent 通过本机 ssmcp 的既有 SSH 数据通道工作，不需要开放这些 SSH 转发功能。[OpenSSH 有效配置说明](https://man.openbsd.org/sshd_config)

确认检查通过后按发行版重载 SSH 服务。Debian/Ubuntu 通常使用 `sudo systemctl reload ssh`，其他发行版可能使用 `sshd` 服务名。保留现有管理连接，直到新连接验证成功。

`ssmcpd attach` 默认连接 `/run/ssmcpd/control.sock`。更改 socket 时在守护进程与 `ForceCommand` 两边设置相同的 `--socket`；更改安装前缀时同步更新 `ForceCommand`。Arch 的包内示例已经使用 `/usr/bin/ssmcpd attach`，可从 `/usr/share/doc/ssmcp/examples/sshd-ssmcp.conf` 安装。

## 3. 提供 HTTPS

将实际域名解析到服务器，在 Caddy 中配置同一域名：

```caddy
ssmcp.xxxx.yyy {
    reverse_proxy 127.0.0.1:8080
}
```

按 Caddy 部署要求完成证书签发并开放 HTTPS 端口。8080 只监听 loopback，不需要向公网开放。反向代理保留路径，以及 `Authorization`、`Origin`、`MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name` 请求头。域名根路径的 `/.well-known/` 与 `/oauth/` 同样转发给 `ssmcpd`，不能只代理 `/alice`。[Caddy reverse_proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)

MCP 请求存在 `Origin` 时，其值必须等于 `SSMCP_PUBLIC_URL` 的公网 origin；没有 `Origin` 的服务器端 MCP 客户端可正常连接。MCP 客户端需支持 OAuth Authorization Code、S256 PKCE 和动态客户端注册。动态登录码、access token 与 refresh token 都不能放入 URL 或代理日志；登录表单用 POST 提交，访问令牌用 `Authorization` 请求头传递。

原生 JSONL 后端的长调用最多等待 15 分钟。若反向代理或 MCP 客户端自定义了请求超时，应与任务时长相容；实时进度使用短等待的事件工具读取。公网入口使用 JSON 响应，不要求代理提供后台 SSE。

## 4. 准备本机后端

本机安装 `ssmcp`，并安装希望使用的 Agent。运行 Agent 原生 CLI 完成登录、provider、模型和项目权限配置。Shell 模式只依赖系统 shell；Agent 模式的可执行文件及最低接口要求见[后端说明](backends.md#启动参数)。

在本机项目目录先检查工具目录：

```sh
ssmcp -t shell --list-tools
ssmcp -t codex --list-tools
ssmcp -t cc --list-tools
ssmcp -t pi --list-tools -- --no-session
ssmcp -t opencode --list-tools
```

每次只检查需要使用的模式。`--list-tools` 在本机启动并关闭后端，不需要 SSH 目标；OpenCode 必须使用 v2。若可执行文件不在 PATH，使用 `--backend /absolute/path/to/program`。

## 5. 建立连接并登录

选择一个模式连接同一 SSH 目标：

```sh
ssmcp alice@ssmcp.xxxx.yyy
ssmcp -t codex alice@ssmcp.xxxx.yyy
ssmcp -t cc alice@ssmcp.xxxx.yyy
ssmcp -t pi alice@ssmcp.xxxx.yyy -- --no-session
ssmcp -t opencode alice@ssmcp.xxxx.yyy
```

同一服务端用户名最多同时连接 128 个客户端，可以在不同电脑或项目目录中选择不同模式；使用 `--name laptop` 等参数指定显示名称。每条连接有独立的 `client_id`，同名客户端也用 ID 区分。首次连接按 OpenSSH 提示核验主机密钥，或预先配置可信 known_hosts。客户端强制公钥认证，保留私钥口令与 ssh-agent 的原生处理。

本机后端握手和目录发现成功后，stdout 输出：

```json
{"url":"https://ssmcp.xxxx.yyy/alice","login_code":"本次连接的一次性动态令牌","login_expires_in":600,"client_id":"服务端分配的连接ID","name":"laptop","type":"pi","cwd":"/home/alice/project","hostname":"alice-laptop"}
```

在 MCP 客户端中添加 `url`，启用 OAuth 登录。客户端自动发现授权端点并打开浏览器；页面只有一个动态令牌输入框和提交按钮，同时显示包含用户名的 MCP 资源 URL、客户端名称与回调来源。核对这些信息，把 `login_code` 粘贴到输入框并提交；成功后浏览器返回 MCP 客户端，由客户端取得并保存 OAuth 令牌。随后调用 MCP `tools/list` 确认只有 `list`、`code_mode`，再调用 `list({})` 查看在线客户端，使用 `list({client_id})` 读取目标客户端的原生能力 schema。

动态登录码从签发起有效 600 秒，成功授权一次即消耗，只能在登录页面使用。stdout 的 `login_expires_in` 已扣除本机后端初始化耗时，可能小于 600。它不是 MCP Bearer token。若登录码过期、已经消耗或需要重新授权，先结束本次 ssmcp 连接，再重连取得新码；这也会终止旧后端并撤销旧授权。已取得 refresh token 的客户端正常续期无须再次输入登录码。

HTTPS、localhost 和 `127.0.0.1` 回调通常直接跳转。IPv6 等 IP 回调可能先显示无脚本的自动返回页面；浏览器未自动跳转时，可点击“返回客户端”链接。两种路径都返回同一个短时 authorization code，页面不会把动态登录码放进回调 URL。

stderr 始终显示本机活动。若只想保存首次输出，可使用 `ssmcp -t pi alice@ssmcp.xxxx.yyy > connection.json`；该文件含有效期内可授权本机操作的登录码，应限制文件访问权限。

ssmcp 每次连接创建新的后端进程。Shell 是新的 PTY，Agent 是否恢复或持久化会话由其原生参数与配置决定。一个 ssmcp 进程的模式在启动时确定；要并用其他模式，启动另一个 ssmcp 进程即可。授权范围覆盖同一 SSH 账号的所有在线客户端，包括后来连接的客户端；签发登录码的连接断开后，只撤销关联的授权，其他连接及其独立授权仍可使用。

### OAuth 兼容性

ssmcpd 同时承担 MCP 资源服务器和 OAuth 授权服务器角色，无须额外部署账号系统或身份提供商。它通过 MCP Protected Resource Metadata 和 OAuth Authorization Server Metadata 提供自动发现，使用 DCR 注册客户端，并要求 Authorization Code + S256 PKCE。[MCP 授权规范](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)；[PKCE](https://www.rfc-editor.org/rfc/rfc7636)

| 入口 | 用途 |
| --- | --- |
| `GET /.well-known/oauth-protected-resource/alice` | 声明 `/alice` 资源对应的授权服务器 |
| `GET /.well-known/oauth-authorization-server` | 提供授权、注册、令牌端点与能力元数据 |
| `POST /oauth/register` | 注册 OAuth 客户端及准确的回调地址 |
| `GET /oauth/authorize` | 校验授权请求并显示动态登录码输入页 |
| `POST /oauth/authorize` | 提交动态登录码，成功后返回一次性 OAuth authorization code |
| `POST /oauth/token` | 用 authorization code 与 PKCE verifier 换取令牌，或轮换 refresh token |

客户端注册支持 `none`、`client_secret_basic` 和 `client_secret_post` 三种令牌端点认证方式，省略时采用 `client_secret_basic`。无客户端 secret 的公开客户端应明确声明 `none`，并仍使用 PKCE。回调地址仅接受 HTTPS，或 native 应用的本机 HTTP loopback，授权请求必须与登记值精确匹配。[动态注册协议](https://www.rfc-editor.org/rfc/rfc7591)

当前不支持 Client ID Metadata Documents（CIMD）；仅支持 CIMD 的 MCP 客户端无法连接。MCP 2026-07-28 推荐 CIMD，并保留 DCR 作为兼容路径，本实现采用后者。[MCP 注册选择](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)

需要自动续期的客户端应在 DCR 中登记 `grant_types:["authorization_code","refresh_token"]`。省略 `grant_types` 时只登记 `authorization_code`，令牌响应不会包含 refresh token。客户端登记也仅保存在内存，有效期最多 7 天；守护进程重启或登记过期后需要重新注册。若 MCP 客户端仍缓存旧 `client_id` 且不能自动恢复，可删除该 MCP 连接后重新添加。[授权实现](../src/oauth.rs)

授权及令牌请求必须携带 `resource`，其值精确等于完整 MCP URL，例如 `https://ssmcp.xxxx.yyy/alice`。支持的 scope 为 `mcp`，省略时采用该值。令牌绑定用户名路径、OAuth 客户端和签发登录码的 SSH 连接。它可以操作同一账号的其他在线客户端，但不能用于另一个用户；签发连接退出后，原授权不能借重连恢复。token 请求可省略 `redirect_uri`；若携带，必须与授权时的地址一致。[资源元数据协议](https://www.rfc-editor.org/rfc/rfc9728)；[授权实现](../src/oauth.rs)

| 凭据 | 生命周期 |
| --- | --- |
| 动态登录码 `login_code` | 600 秒；在网页成功授权一次即消耗 |
| OAuth authorization code | 60 秒；需原客户端携带匹配的 S256 PKCE verifier 兑换 |
| access token | 最长 1 小时；通过 Bearer 请求头调用 MCP |
| refresh token | 仅向登记此 grant 的客户端签发；每次刷新轮换，同一授权最长 24 小时，不因刷新延长 |

在客户端身份、resource 和 PKCE 等绑定仍正确的前提下，重放已成功兑换的授权码，或重放已使用的 refresh token，会撤销该授权族。SSH 断开、客户端退出或服务重启会立即使其关联的登录码及全部令牌失效。OAuth 状态仅保存在内存；重新授权时应使用新连接生成的动态登录码。

手工测试 MCP HTTP 时，`Authorization: Bearer <access_token>` 中的令牌必须来自上述 OAuth 流程。把 stdout 中的 `login_code` 填进该请求头会被拒绝。浏览器回调 URL 中出现的是短时、PKCE 绑定的 OAuth authorization code，和用户粘贴的动态登录码是两个不同凭据。

OAuth 状态使用有界内存：最多保留 1024 个客户端登记，每项最长 7 天；最多保留 2048 个授权事务，每项最长 10 分钟。其他令牌状态也有容量上限，容量不足时返回 `503`。项目本身没有按请求来源限流；公网部署可根据实际负载在反向代理配置速率和并发限制。[容量定义](../src/oauth.rs)

## 6. 检查与升级

`http://127.0.0.1:8080/healthz` 只检查守护进程健康，不列出用户名或凭据。未授权访问 `/alice` 会得到 OAuth 发现信息；完成登录后，每次 MCP 请求都需携带 access token。再完成 MCP discovery/initialize，确认 `tools/list` 只有 `list` 和 `code_mode`，通过 `list` 枚举目标客户端的 ID、名字、启动目录和后端类型。`cwd_kind: "startup"` 表示目录是注册时快照。

在 `code_mode` 中通过 `await call(clientId, "terminal.write", {data:"pwd\n"})` 和 `terminal.read` 验证 Shell 执行。Agent 可先用无模型的状态接口检查链路，例如 `pi.get_state` 或 `opencode.server.info`；先通过 `list({client_id})` 查看 schema，再在 `code_mode` 调用。模型任务和审批流程按[后端说明](backends.md)验证。Claude Code 前台 Bash 的输出在调用结束时返回，持续增量观察应使用其原生后台输出路径。

本机退出 ssmcp 后，旧 access token 应得到 `401`，旧 refresh token 也不能继续刷新。同一账号的其他客户端仍应在线，其独立授权仍有效。需要隔离不同操作者的权限时，使用不同服务端账户；这些账户的路径与授权独立，不能借请求体指定其他用户名。

升级 0.3.0 时先安排现有连接退出，更新两端二进制与安装脚本，重启 `ssmcpd`，再重新连接并完成 OAuth 登录，刷新 MCP 客户端缓存的工具表。原有直接调用原生工具的代码改为经 `code_mode` 的 `call` 调用。内部 wire 版本不匹配会被拒绝；旧凭据和旧事件游标不可用于新连接。

本仓库的 `scripts/e2e.py` 使用临时本机 sshd 和临时密钥检验认证、MCP、后端转发与撤销行为。实际 DNS、TLS 证书、反向代理和发行版 SSH 策略仍需在你的服务器上验证；示例域名没有预先部署服务。

## 系统代码运行环境

服务端从 PATH 查找系统 `node` 和 `bwrap`。每次 `code_mode` 请求启动独立子进程，使用受限资源和有界 JSON 管道；Rust 侧验证请求并转发到指定客户端。脚本由 ssmcp 安装，运行时由系统包管理器维护。自定义脚本路径时，在服务环境中用 `SSMCP_CODE_MODE_RUNNER` 指定绝对路径。

bubblewrap 需要系统允许创建用户和网络命名空间。Ubuntu 24.04 启用 AppArmor 用户命名空间限制时，安装 `apparmor-profiles`，并加载发行版提供的 `bwrap-userns-restrict` profile；不需要关闭全局 AppArmor。服务单元允许 bubblewrap 配置隔离网络所需的 `AF_NETLINK`。[Ubuntu 用户命名空间说明](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces)

依赖缺失或隔离环境不可用时，`code_mode` 返回明确错误；`list` 仍可用于发现客户端。代码源码、运行时间、内存、原生调用次数和输出均有上限，详见[后端说明](backends.md#传输与资源边界)。

## 参考

- [OpenSSH ssh_config](https://man.openbsd.org/ssh_config) / [sshd_config](https://man.openbsd.org/sshd_config)
- [systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)
- [Caddy reverse_proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)
- [后端接口与资源边界](backends.md)
- [验收报告](validation.md)
