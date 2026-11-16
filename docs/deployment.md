# Linux 部署

服务器部署 OpenSSH、`ssmcpd` 和 HTTPS 反向代理；本机部署 `ssmcp` 及需要控制的 Agent。服务器不运行 Codex、Claude Code、Pi 或 OpenCode，模型身份和项目配置都留在本机。以下以 systemd Linux、`/usr/local` 前缀和服务端账户 `alice` 为例。

服务器已经部署好时，可直接阅读 [配置、SSH 公钥与登录](configuration.md)：复用 `~/.ssh/config`、添加和轮换公钥、完成 OAuth 登录，以及处理重新打开后的授权状态。

ssmcp 0.4.0 继续使用内部 wire v4，对外只提供 `list`、`code_mode` 两个 MCP 工具。旧客户端可继续使用登录码；新的 `--allow 用户名@github` 需要两端更新，并在服务端配置 GitHub OAuth。服务器重启会清空当前连接、客户端登记和 OAuth 授权。

## 1. 安装程序与服务账户

客户端需要 Linux 5.3 或更新内核以使用 pidfd；源码构建使用仓库固定的 Rust 1.100.0。服务端通过系统安装的 Node.js 18+ 执行代码，通过 bubblewrap 0.8+ 创建隔离环境，并在该环境内用 util-linux 提供的 prlimit 限制进程数；依赖由发行版包管理器安装和更新：

```sh
# Debian / Ubuntu
sudo apt install openssh-server nodejs bubblewrap util-linux
# Arch Linux
sudo pacman -S openssh nodejs bubblewrap util-linux
# Fedora
sudo dnf install openssh-server nodejs bubblewrap util-linux
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

<a id="github-oauth"></a>

### 配置 GitHub OAuth 与 TOML

在 GitHub 的 Settings → Developer settings → OAuth Apps 中创建一个 OAuth App。Homepage URL 填服务器的公网 HTTPS origin，Authorization callback URL 填该 origin 后接 `/oauth/callback/github`。例如下列配置对应的 callback 是 `https://ssmcp.example.com/oauth/callback/github`。取得该应用的 Client ID 和 Client secret，只在服务器配置中保存。[GitHub 创建 OAuth App](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/creating-an-oauth-app)

在 `/etc/ssmcpd/ssmcpd.toml` 写入配置，将示例 origin 和两项凭据占位文本替换为实际值：

```toml
[server]
listen = "127.0.0.1:8080"
public_url = "https://ssmcp.example.com"
socket = "/run/ssmcpd/control.sock"

[auth.github]
client_id = "YOUR_GITHUB_OAUTH_CLIENT_ID"
client_secret = "YOUR_GITHUB_OAUTH_CLIENT_SECRET"

[oauth]
access_token_ttl = 28800
refresh_token_idle_ttl = 2592000
refresh_token_max_ttl = 7776000
client_registration_ttl = 31536000
```

`public_url` 只包含 `https://`、主机名和可选端口，不带用户路径、查询参数或 fragment。GitHub 模式使用该 origin 下的 `/mcp`；登录码模式使用 `/SSH用户名`。GitHub callback 从同一 origin 自动生成，无需另配 callback 参数。

含密钥的 TOML 文件使用 `0600`，所有者设为运行服务的 `ssmcpd` 用户。以下命令只调整已有配置文件的所有权和权限：

```sh
sudo chown ssmcpd:ssmcp /etc/ssmcpd/ssmcpd.toml
sudo chmod 0600 /etc/ssmcpd/ssmcpd.toml
```

服务器以存在的 `/etc/ssmcpd/ssmcpd.toml` 为默认文件，也可通过 `ssmcpd serve --config /absolute/path/ssmcpd.toml` 或 `SSMCP_CONFIG` 显式指定。配置值按 **命令行参数 → 环境变量 → TOML → 内置默认值** 的顺序生效。新安装的环境示例全部使用注释，不会覆盖 TOML。原来的 `/etc/ssmcpd/ssmcpd.env` 仍可由 systemd 读取；迁移到 TOML 时同步清理同名旧环境项，避免它们覆盖新配置。显式指定的文件缺失或解析失败会阻止启动。[配置读取](../src/bin/ssmcpd.rs)

只使用原有登录码时，省略整个 `[auth.github]` 节即可。启用 GitHub 后，服务端只请求身份验证所需的公开用户信息，不请求仓库或邮箱 scope。GitHub 返回的 token 用于读取一次已登录用户身份，随后释放；MCP access token 和 refresh token 由 ssmcp 自己签发。[身份提供方实现](../src/identity.rs) · [GitHub OAuth 流程](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps)

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now ssmcpd
sudo systemctl status ssmcpd
```

`journalctl -u ssmcpd` 查看守护进程状态。守护进程日志不记录登录码、OAuth token、GitHub client secret 或后端执行数据；本机 `ssmcp` 的 stderr 是实时活动日志，stdout 输出连接信息。登录码模式的 stdout 还含一次性登录码，应按凭据保管。

## 2. 配置 SSH 公钥账户

为允许连接的用户创建服务端普通账户，并加入 `ssmcp` 组：

```sh
sudo useradd --create-home --user-group --shell /bin/sh --groups ssmcp alice
```

已有账户使用 `sudo usermod -aG ssmcp alice`，保留其 home、主组与已有公钥。账户需要满足发行版的账户和 PAM 登录策略，并有可执行的登录 shell，供 OpenSSH 执行固定桥接命令。

由管理账户或服务器控制台按 [公钥安装步骤](configuration.md#public-keys) 添加客户端的 `.pub` 文件。该步骤查找目标账户的实际 home 和主组，向 `authorized_keys` 追加尚未登记的公钥，保留已有条目及授权选项；`.ssh` 使用 `0700`，`authorized_keys` 使用 `0600`，所有者为目标账户。私钥保留在客户端。已有自定义 `AuthorizedKeysFile` 或 `AuthorizedKeysCommand` 时，应更新 OpenSSH 实际使用的授权来源。

尚未启用强制桥接、且目标账户已有普通 SSH 登录方式时，也可先用 `ssh-copy-id -i ~/.ssh/id_ed25519.pub alice@server` 添加公钥。启用下述 `ForceCommand` 后，普通 SSH 命令及 SCP/SFTP 都进入桥接程序；后续密钥管理应通过组外管理账户或控制台进行。管理员账户应保留独立的登录通道。

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

按 Caddy 部署要求完成证书签发并开放 HTTPS 端口。8080 只监听 loopback，不需要向公网开放。反向代理保留路径，以及 `Authorization`、`Origin`、`MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name` 请求头。将 `/mcp`、SSH 用户路径、`/.well-known/` 和 `/oauth/` 整体转发给 `ssmcpd`，其中包含 `/oauth/callback/github`。[Caddy reverse_proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)

MCP 请求存在 `Origin` 时，其值必须等于配置生效后的 `public_url` origin；没有 `Origin` 的服务器端 MCP 客户端可正常连接。MCP 客户端需支持 OAuth Authorization Code、S256 PKCE 和动态客户端注册。动态登录码、access token 与 refresh token 都不能放入 URL 或代理日志；登录表单用 POST 提交，访问令牌用 `Authorization` 请求头传递。

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

首次使用时可按 [客户端配置与逐步登录教程](configuration.md#oauth-login) 操作。已有 SSH Host 条目的复用方法见 [SSH 配置](configuration.md#ssh-config)，重新打开后的行为见 [连接与授权生命周期](configuration.md#session-lifetime)。

GitHub 模式在所选后端命令中指定允许的身份；将示例用户名换成实际获准访问的用户：

```sh
ssmcp --allow octocat@github alice@ssmcp.example.com
ssmcp -t codex --allow octocat@github alice@ssmcp.example.com
ssmcp -t cc --allow octocat@github alice@ssmcp.example.com
ssmcp -t pi --allow octocat@github alice@ssmcp.example.com -- --no-session
ssmcp -t opencode --allow octocat@github alice@ssmcp.example.com
```

同一服务端用户名最多同时连接 128 个客户端，可以在不同电脑或项目目录中选择不同模式；使用 `--name laptop` 等参数指定显示名称。每条连接有独立的 `client_id`，同名客户端也用 ID 区分。首次连接按 OpenSSH 提示核验主机密钥，或预先配置可信 known_hosts。客户端强制公钥认证，保留私钥口令与 ssh-agent 的原生处理。

本机后端握手和目录发现成功后，stdout 输出：

```json
{"url":"https://ssmcp.example.com/mcp","auth":"github","allow":["octocat@github"],"client_id":"服务端分配的连接ID","name":"laptop","type":"pi","cwd":"/home/alice/project","hostname":"alice-laptop"}
```

在 MCP 客户端中添加完整 `/mcp` URL，启用 OAuth 登录。ssmcp 页面显示 MCP 客户端名称、已登记回调来源和资源 URL；点击 GitHub 登录，在 GitHub 完成身份验证后返回 ssmcp。页面显示已验证身份和当前可访问会话数，再由用户点击“允许连接”或“取消”。只完成 GitHub 登录不会自动授权这个 MCP 客户端。[登录步骤](configuration.md#github-allow)

同意后由 MCP 客户端取得并保存 ssmcp 令牌。调用 `tools/list` 确认工具表只有 `list`、`code_mode`，再用 `list({})` 查看允许访问的在线客户端。每个目标客户端都依据接入时解析的 GitHub 稳定用户 ID 检查权限；未获准的客户端不会显示，也不能通过猜测 `client_id` 调用。当前没有获准访问的在线连接时可先完成登录，`list` 返回空列表，之后会显示新接入且允许该身份的连接。

省略 `--allow` 时保留原有 `/SSH用户名` 与登录码方式。登录码从签发起有效 1800 秒，成功使用一次即消耗；`login_expires_in` 扣除后端初始化时间。该授权只覆盖同一 SSH 账户下未设置 `--allow` 的连接，签发连接退出后撤销。设置了 `--allow` 的连接无法通过登录码授权访问。[两种授权方式](configuration.md#authorization-modes)

HTTPS、localhost 和 `127.0.0.1` 回调通常直接跳转。IPv6 等 IP 回调可能先显示无脚本的自动返回页面；浏览器未自动跳转时，可点击“返回客户端”链接。两种路径都返回同一个短时 authorization code，页面不会把动态登录码放进回调 URL。

stderr 始终显示本机活动。stdout 可以保存到文件；登录码模式的输出含可授权本机操作的一次性凭据，应限制文件访问权限。

ssmcp 每次连接创建新的后端进程。Shell 是新的 PTY，Agent 是否恢复会话由其原生参数与配置决定。退出会让这条连接及后端消失；GitHub 授权本身可以保持有效，重连后按新连接的允许列表重新判断访问。登录码授权仍绑定其签发连接。GitHub 用户名在每次接入时重新解析，当前没有跨连接持久保存的用户名与稳定 ID 绑定；改名后应更新 `--allow` 参数。

### OAuth 兼容性

ssmcpd 同时承担 MCP 资源服务器和 OAuth 授权服务器角色。GitHub 模式通过上游 OAuth 验证身份，随后由 ssmcp 获取用户对 MCP 客户端的同意并签发自己的令牌；原有登录码方式也使用这套 MCP OAuth 接口。服务通过资源元数据与授权服务器元数据提供自动发现，使用 DCR 注册 MCP 客户端，并要求 Authorization Code + S256 PKCE。[MCP 授权规范](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)；[PKCE](https://www.rfc-editor.org/rfc/rfc7636)

| 入口 | 用途 |
| --- | --- |
| `GET /.well-known/oauth-protected-resource/mcp` | 声明全局 GitHub 身份资源 `/mcp` |
| `GET /.well-known/oauth-protected-resource/alice` | 声明 SSH 用户资源 `/alice` |
| `GET /.well-known/oauth-authorization-server` | 提供授权、注册、令牌端点与能力元数据 |
| `POST /oauth/register` | 注册 OAuth 客户端及准确的回调地址 |
| `GET /oauth/authorize` | 校验请求，显示 MCP 客户端信息与可用登录方式 |
| `POST /oauth/authorize` | 开始提供方登录、确认或取消授权，或提交原有登录码 |
| `GET /oauth/callback/github` | 接收 GitHub 回调，验证身份后显示 MCP 客户端同意页 |
| `POST /oauth/token` | 用 authorization code 与 PKCE verifier 换取令牌，或轮换 refresh token |

客户端注册支持 `none`、`client_secret_basic` 和 `client_secret_post` 三种令牌端点认证方式，省略时采用 `client_secret_basic`。无客户端 secret 的公开客户端应明确声明 `none`，并仍使用 PKCE。回调地址仅接受 HTTPS，或 native 应用的本机 HTTP loopback，授权请求必须与登记值精确匹配。[动态注册协议](https://www.rfc-editor.org/rfc/rfc7591)

当前不支持 Client ID Metadata Documents（CIMD）；仅支持 CIMD 的 MCP 客户端无法连接。MCP 2026-07-28 推荐 CIMD，并保留 DCR 作为兼容路径，本实现采用后者。[MCP 注册选择](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)

MCP 的 DCR 接口在省略 `grant_types` 时默认登记 `["authorization_code","refresh_token"]`，并在响应中返回实际登记的类型。显式只请求 `["authorization_code"]` 时，令牌响应不包含 refresh token。MCP 客户端登记及其密钥默认有效 365 天，状态只保存在内存；守护进程重启或登记过期后需要重新注册。若应用持续缓存旧 `client_id`，可删除该 MCP 连接后重新添加。[授权实现](../src/oauth.rs)

客户端实现自动续期时，应保存首次登录和每次刷新返回的最新令牌。在 access token 到期前，或收到 MCP `401` 响应中 `WWW-Authenticate: Bearer` 的 `error="invalid_token"` 后，向 token endpoint 发送 `grant_type=refresh_token`、当前 refresh token、原 `resource`，并按登记的认证方式携带客户端身份。成功后，用返回的新 refresh token 替换旧值，再用新 access token 重试。刷新请求应按授权串行执行；已使用的 refresh token 会失效，重放它会触发授权撤销。服务端提供这条刷新路径，MCP 客户端负责主动调用。[客户端续期步骤](configuration.md#silent-refresh)

授权及令牌请求必须携带精确的 `resource`，例如 GitHub 模式的 `https://ssmcp.example.com/mcp`，或登录码模式的 `https://ssmcp.example.com/alice`。支持的 scope 为 `mcp`，省略时采用该值。令牌绑定 resource 与 MCP 客户端；GitHub 授权另绑定已验证的稳定身份，登录码授权另绑定签发连接。不同 resource 的令牌不能互用。token 请求可省略 `redirect_uri`；携带时必须与授权时一致。[资源元数据协议](https://www.rfc-editor.org/rfc/rfc9728) · [授权实现](../src/oauth.rs)

<a id="oauth-lifetimes"></a>

### 有效期与服务端参数

| 凭据或状态 | 0.4.0 默认生命周期 |
| --- | --- |
| 动态登录码 `login_code` | 30 分钟；在网页成功授权一次即消耗 |
| 授权表单事务 | 30 分钟；独立于登录码有效期 |
| OAuth authorization code | 60 秒；需原客户端携带匹配的 S256 PKCE verifier 兑换 |
| access token | 最长 8 小时；通过 Bearer 请求头调用 MCP |
| refresh token 闲置期 | 30 天；每次成功刷新后从当前时刻重新计时，并轮换令牌 |
| 同一授权总期限 | 自首次授权起最长 90 天；成功刷新也不会延长这个期限 |
| OAuth 客户端登记及密钥 | 最长 365 天 |

令牌实际寿命还受当前授权和客户端登记到期时间约束，MCP 客户端应读取令牌响应中的 `expires_in`。access token 到期后可通过 refresh token 继续访问，无须重新输入登录码。刷新闲置期或授权总期限到期后，需要新登录。[生命周期实现](../src/oauth.rs)

以下四项可写入 TOML 的 `[oauth]` 节，键名与环境变量去掉 `SSMCP_` 后的小写形式相同。也支持 `ssmcpd serve` 参数和环境变量，优先级为命令行 → 环境变量 → TOML → 默认值：

| 设置 | 命令行参数 | 环境变量 | 默认秒数 |
| --- | --- | --- | ---: |
| 访问令牌寿命 | `--access-token-ttl` | `SSMCP_ACCESS_TOKEN_TTL` | 28800 |
| 刷新令牌闲置期 | `--refresh-token-idle-ttl` | `SSMCP_REFRESH_TOKEN_IDLE_TTL` | 2592000 |
| 授权总期限 | `--refresh-token-max-ttl` | `SSMCP_REFRESH_TOKEN_MAX_TTL` | 7776000 |
| 客户端登记寿命 | `--client-registration-ttl` | `SSMCP_CLIENT_REGISTRATION_TTL` | 31536000 |

```sh
ssmcpd serve --public-url https://ssmcp.example.com \
  --access-token-ttl 28800 \
  --refresh-token-idle-ttl 2592000 \
  --refresh-token-max-ttl 7776000 \
  --client-registration-ttl 31536000
```

数值必须为正整数秒，并满足 `access ≤ idle ≤ max ≤ registration ≤ 315360000`；上限为 3650 天。登录码、授权表单与短时 authorization code 使用上表固定期限。配置在守护进程启动时生效；重启服务会清空当前连接、登记和授权，因此应安排好任务，再重新连接并登录。

在客户端身份、resource 和 PKCE 等绑定仍正确的前提下，重放已成功兑换的授权码或已使用的 refresh token，会撤销该授权族。登录码授权会在其签发 SSH 连接退出时失效。GitHub 授权不锚定某条 SSH 连接，但每次调用只允许操作当前在线且允许该身份的目标。所有 OAuth 状态仍只在内存中，服务重启后需要重新注册和登录。

手工测试 MCP HTTP 时，`Authorization: Bearer <access_token>` 中的令牌必须来自上述 OAuth 流程。把 stdout 中的 `login_code` 填进该请求头会被拒绝。浏览器回调 URL 中出现的是短时、PKCE 绑定的 OAuth authorization code，和用户粘贴的动态登录码是两个不同凭据。

OAuth 状态使用有界内存：最多保留 1024 个客户端登记，每项默认最长 365 天；最多保留 2048 个授权事务，每项最长 30 分钟。其他令牌状态也有容量上限，容量不足时返回 `503`。项目本身没有按请求来源限流；公网部署可根据实际负载在反向代理配置速率和并发限制。[容量定义](../src/oauth.rs)

## 6. 检查与升级

`http://127.0.0.1:8080/healthz` 只检查守护进程健康，不列出用户名或凭据。未授权访问 `/mcp` 或 `/alice` 会得到 OAuth 发现信息；完成登录后，每次 MCP 请求都需携带 access token。完成 discovery/initialize，确认 `tools/list` 只有 `list` 和 `code_mode`，再用 `list` 枚举有权操作的在线客户端。`cwd_kind: "startup"` 表示目录是注册时快照。

在 `code_mode` 中通过 `await call(clientId, "terminal.write", {data:"pwd\n"})` 和 `terminal.read` 验证 Shell 执行。Agent 可先用无模型的状态接口检查链路，例如 `pi.get_state` 或 `opencode.server.info`；先通过 `list({client_id})` 查看 schema，再在 `code_mode` 调用。模型任务和审批流程按[后端说明](backends.md)验证。Claude Code 前台 Bash 的输出在调用结束时返回，持续增量观察应使用其原生后台输出路径。

验证 GitHub 模式时，为两个客户端设置不同 `--allow`，确认当前身份只看到并能调用匹配的客户端；未匹配的 `client_id` 也必须被拒绝。关闭一个客户端后，它应从列表消失；已有 GitHub 授权可以继续访问其他匹配客户端。重连时按新的允许列表重新判断。

验证登录码模式时，签发连接退出后，其旧 access token 应得到 `401`，refresh token 也不能继续刷新。确认该授权无法列出或调用任何设置了 `--allow` 的连接；同一账号的其他独立登录码授权保持各自生命周期。

从 0.2 系列或更早版本升级时，更新两端二进制与安装脚本，重启 `ssmcpd`，再重新连接、登录并刷新 MCP 工具表。原有直接调用原生工具的代码改为经 `code_mode` 的 `call` 调用。内部 wire 版本不匹配会被拒绝；旧事件游标不可用于新连接。

0.3.1 的 SSH Host 别名支持保持有效。0.4.0 继续使用 wire v4：省略 `--allow` 的客户端保持旧消息格式；新增允许列表会被不支持它的旧服务器拒绝。新客户端也会检查服务端是否确认 GitHub 模式，避免把受限连接按旧授权方式开放。

<a id="upgrade-032"></a>
<a id="upgrade-040"></a>

### 升级到 0.4.0：启用 GitHub 身份与每客户端允许列表

更新源码后，需要把新二进制和服务文件安装到实际服务器，并重启守护进程。只更新 GitHub 仓库不会改变运行中的服务。已有 SSH 账户、公钥和客户端 Host 配置可以保留。

1. 安排现有任务退出，构建安装 0.4.0 两端程序；需要 GitHub 模式时，创建 OAuth App，按上面的 callback 和 TOML 示例配置服务器。
2. 确认 TOML 所有权、`0600` 权限、配置优先级与 HTTPS 路径，重启 `ssmcpd`。用实际服务程序的 `--version` 检查版本。
3. 本机带 `--allow 用户名@github` 重新连接；在 MCP 应用添加 `/mcp`，完成 GitHub 登录并明确同意当前 MCP 客户端。保留登录码方式时继续添加终端返回的 `/SSH用户名` URL。
4. 确认应用保存并轮换 ssmcp refresh token；调用 `list` 重新选择当前可访问的客户端。如果应用保留已失效的 OAuth 客户端登记，移除旧 MCP 连接后重新添加。

授权默认沿用 8 小时访问令牌、30 天刷新闲置期和 90 天总期限。GitHub 授权按身份检查每条在线连接，登录码授权保留连接绑定；两者都在守护进程重启后重新登录。具体 MCP 应用是否自动发起刷新，需要在该应用中确认。

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
