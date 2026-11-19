# 客户端账号、会话与临时接入

在 A 的项目目录运行 `ssmcp username@server`，客户端会自动完成所需的 GitHub 登录、用户名绑定和 HTTPS / WSS 连接。随后可以列出可控会话、进入远程终端，或生成一条命令，让 B 临时成为自己的被控端。

客户端和服务端均需支持具名账号入口。下文的 MCP `invite` 与校验后持久安装需要新版网关、同源发布清单和对应平台二进制；只升级本机客户端不会改变正在运行的服务器。以网关实际 `tools/list` 和 `/client/release.json` 为准。示例域名 `ssmcp.example.com` 应替换为你的服务器。

## 1. 选择用户名并连接

在 A 上安装客户端后运行：

~~~sh

# 在需要开放的项目目录中运行，保持这个终端打开。
ssmcp alice@ssmcp.example.com --name device-a

# 没有本机浏览器时，使用打印的链接和确认码。
ssmcp alice@ssmcp.example.com --no-browser
~~~

首次连接自动打开浏览器。使用 GitHub 登录、核对终端配对信息，并确认注册或登录请求后，客户端继续连接。已有有效凭据时直接复用；旧账号尚未绑定用户名时，将本次选择的名字绑定到已认证账号。`whoami --json` 返回 GitHub 身份、服务器用户名和有效期，不输出令牌。

用户名大小写不敏感并保存为小写，长度为 3–32，首字符为 ASCII 字母，末字符为字母或数字，中间可用字母、数字、`_` 和 `-`。保留名不能注册；已占名字只允许其绑定的 GitHub 账号登录，每个 GitHub 账号在同一服务器绑定一个名字。GitHub 的数字 ID 始终用来识别账户。详见[用户名与服务器管理](named-accounts.md)。

默认地址按 `--server`、配置文件的 `[client].server`、最后登录的服务器、内置地址依次选择。`ssmcp config set server URL` 保存默认地址，`ssmcp config get server` 显示生效值，`ssmcp config path` 定位可编辑文件。不同服务器各自保存凭据，不会把一个服务器的令牌交给另一个服务器。[配置与优先级](client-server.md#设置默认服务器)

账户连接通过 HTTPS / WSS 建立；A、B 都不需要 SSH 密钥或入站监听端口。服务端在注册时绑定已经验证的 GitHub 数字 ID。[客户端账号实现](../src/client/account.rs) · [服务端授权](../src/device.rs)

启动本机被控会话和查看账号下的远端会话是两个动作。前台 `ssmcp` 负责开放当前设备；在另一个终端执行下一节的列表和交互命令。

## 2. 列出会话，进入终端

~~~sh
ssmcp list
ssmcp list --json

# 列出在线会话并选择一个；也支持 ssmcp list -i。
ssmcp -i

# 已知目标时直接进入。
ssmcp --server https://ssmcp.example.com -i --client SESSION_ID

# 选择该会话中的另一个已存在终端。
ssmcp -i --client SESSION_ID --terminal pty-0001
~~~

列表只显示当前登录账号有权操作的在线会话。可按序号、完整 ID 或唯一名称选择；同名设备使用 ID 区分。交互模式默认进入选中会话的 `main` 终端，传送键盘输入、输出和窗口尺寸；Ctrl-C 交给远端 Shell，Ctrl-] 退出当前控制界面，远端会话继续运行。

| 命令 | 范围 |
| --- | --- |
| `ssmcp list` / `ssmcp ls` | 当前账号在服务器上可控的在线会话 |
| `ssmcp sessions list --remote` | 同一账号列表的显式写法 |
| `ssmcp sessions list` | 当前设备保存的本地后台会话 |
| `ssmcp attach NAME` | 连接当前设备上的本地后台会话 |
| `ssmcp -i FILE HOST` | 旧 SSH 路径的私钥参数；可用 `--identity FILE` 写清用途 |

## 3. 从 A 生成 B 的临时接入命令

在已登录的 A 上执行：

~~~sh
ssmcp invite
~~~

把输出的完整单行命令复制到 B 的终端。未固定服务器公钥时，脚本选择本机系统和 CPU，从同一网关获取当前发布的兼容版本，核对 wire 版本、字节数、SHA-256 和程序的 `--version`，再原子安装到用户目录，并用安装后的绝对路径前台运行 `join`。校验失败时保留原安装，且不兑换邀请。B 无须预装 SSH、注册 GitHub 账号或取得 A 的长期令牌。[引导与校验实现](../src/client_downloads.rs)

Linux / macOS 默认安装到 `~/.local/bin/ssmcp`；Windows 默认到 `%LOCALAPPDATA%\ssmcp\bin\ssmcp.exe`。`SSMCP_INSTALL_DIR` 可指定绝对安装目录，PowerShell 另接受 `-InstallDir`。临时下载结束后清理，已安装程序保留；脚本不修改 PATH，也不注册后台服务。系统范围是 Linux GNU x86_64 / ARM64（glibc 2.35+）、macOS Intel / Apple Silicon、Windows x64 / ARM64。Linux 的 musl 与其他 CPU 不在此入口支持范围内。[Shell](../src/client_downloads/install.sh) · [PowerShell](../src/client_downloads/install.ps1)

Windows 默认同时准备原生 Node.js、固定 Playwright npm 包和 Chromium；不安装或注册外部 harness。成功前会实际验证浏览器启动，并通过 `SSMCP_NODE` / `SSMCP_BROWSER_EXECUTABLE` 保存当前和后续终端使用的路径。依赖失败不会继续邀请接入；只装基础客户端可明确使用 `-SkipBrowserDependencies`。[安装位置、版本、原生检查和失败处理](windows-browser-setup.md)

默认邀请可在 **10 分钟**内使用一次。B 兑换成功后，临时会话可用 **1 小时**，同时受 A 本次登录的剩余有效期约束。A 在另一终端运行 `ssmcp list` 或 `ssmcp -i` 即可控制 B。

~~~sh
# 指定另一台已登录的服务器；生成命令中的下载和接入地址一起切换。
ssmcp invite --server https://other.example.com

# 15 分钟内兑换；B 兑换后最多使用 2 小时。
ssmcp invite --ttl 900 --session-ttl 7200

# B 使用 Windows PowerShell。
ssmcp invite --shell powershell

# B 已安装兼容版本时，生成简短的 ssmcp join 命令。
ssmcp invite --no-bootstrap

# 供脚本读取，包含 POSIX / PowerShell 两种命令与有效期。
ssmcp invite --json
~~~

`--ttl` 接受 30–3600 秒，`--session-ttl` 接受 60–86400 秒。POSIX 引导需要系统 Shell、curl 或 wget，以及 sha256sum 或 shasum；Windows 使用 PowerShell / .NET。`--json` 包含两种命令、实际期限，以及网关提供的 `release`、`wire_version`、`supported_targets`；未发布时给出 `release_error`。管理员须准备相应平台的兼容发布，配置见第 6 节。[命令实现](../src/client/account.rs)

**对已登记可信公钥的服务器，`invite` 自动生成预安装客户端命令。** B 应先从可信来源安装新版 ssmcp；输出命令把 A 的公钥指纹保存到 B 的 `trusted-servers.toml`，成功后再执行 `join`。该流程适用于 POSIX Shell 和 PowerShell，避免在公钥校验生效前下载执行安装脚本。公开信任记录会留在 B，临时设备令牌仍只驻留进程。[信任文件与邀请](client-server.md#公钥固定后的临时邀请)

手工使用已安装客户端时，命令形式为：

~~~sh
ssmcp join --server https://ssmcp.example.com --token '<一次性邀请令牌>'
~~~

B 收到的设备凭据只允许注册自己的被控会话，不能列出 A 的其他设备、调用其他会话或签发新邀请。设备凭据仅保存在运行进程内；网络暂时中断时，在有效期内复用它重连，不再次兑换邀请。退出 B 进程后需要在 A 生成新命令。B 的进程使用运行命令的本机用户权限。[邀请与设备凭据](../src/device.rs) · [客户端传输](../src/client/relay.rs)


### 已在 MCP 应用授权的控制者

ChatGPT 等 MCP 应用可以直接调用公开工具 `invite`，使用本次已验证的 GitHub OAuth 身份生成安装命令，不需要在控制者本机额外创建一份 CLI 登录。参数只有期限与 Shell 类型：

~~~json
{
  "name": "invite",
  "arguments": {
    "ttl_secs": 600,
    "session_ttl_secs": 3600,
    "shell": "sh"
  }
}
~~~

B 使用 Windows 时把 `shell` 改为 `powershell`。返回值同时含 `command_sh`、`command_powershell`、选中的 `command`、当前发布的精确版本及平台、实际可用期限。该工具是 MCP `tools/call` 入口，不是本机原生工具；不需要先找到在线客户端。[公开工具与参数校验](../src/http.rs)

授权只来自当前 OAuth：接受身份入口 `/mcp` 或该身份自己的稳定账号资源；拒绝借用其他资源、登录码授权，以及调用方提供的 `owner`、`subject`、`token`。命令只包含短期、一次使用的 `ci_` 邀请，不包含 GitHub 令牌、OAuth access token 或 CLI 长期凭据。服务端把设备 ACL 绑定到已验证的 GitHub 数字 ID，B 不再进行 GitHub 登录。[授权与账本](../src/device.rs)

MCP 邀请默认 600 秒内兑换、会话最多 3600 秒；API 接受的上限分别为 3600 和 86400 秒。二者还受**签发时原始 OAuth access grant 的剩余期限**约束。刷新 OAuth 不会延长已有邀请或设备；原 family 被撤销、原 access grant 过期、账号被封禁，或网关重启丢失该授权时，后续兑换和连接被拒绝，已连接设备每秒复核后结束。CLI `logout` 撤销的是其对应 CLI 登录；独立 MCP 授权不受这次 CLI 注销替代管理。[原授权引用与固定期限](../src/oauth.rs) · [存活检查](../src/device.rs)

网关未配置账户存储或兼容发布时，MCP `invite` 在签发邀请前返回明确错误。安装后的设备仍以 B 运行命令的本机用户身份工作，原生权限和审批继续生效。

## 4. 停止、退出和看门狗

看门狗默认开启，连接断开后默认每 5 秒重连。可调整间隔或关闭：

~~~sh
ssmcp --reconnect-delay 10
ssmcp --no-watchdog
~~~

| 操作 | 结果 |
| --- | --- |
| A 的交互界面按 Ctrl-] | A 离开控制界面，B 继续运行 |
| A 的交互界面按 Ctrl-C | 中断 B 当前前台任务，保持终端连接 |
| 在 B 启动 ssmcp 的终端按 Ctrl-C | 结束 B 被控会话及其连接拥有的工作 |
| 临时会话到期 | 服务器停止接受该设备授权，客户端停止恢复它 |
| A 执行 `ssmcp logout` | 撤销这次客户端登录，以及由它签发的邀请和设备授权 |
| 本地 `ssmcp sessions stop NAME` | 保存停止意图，看门狗不会重新启动它 |

注销作用于当前服务器上的这次客户端登录。其他独立登录各自管理；ChatGPT 等 MCP 应用的 OAuth 授权另有生命周期。`--watchdog` 仍作为显式启用参数保留。`--no-watchdog` 不能与 `--reconnect-delay` 一起指定。

## 5. 凭据与原有 SSH 用法

客户端凭据默认有效 30 天。使用具名账号的前台连接时，过期或被撤销的凭据会触发新的 GitHub 授权；同一运行进程最多自动重新授权一次，以免失败时循环打开浏览器。403 封禁、网络故障及服务端错误不会触发重新登录，也不会删除凭据。后台会话不会打开浏览器；在终端重新运行 `ssmcp alice@server` 授权后，再用 `ssmcp sessions restart NAME` 恢复。Linux / macOS 默认在 `$XDG_CONFIG_HOME/ssmcp/auth` 或 `~/.config/ssmcp/auth` 保存凭据；Windows 使用 `%LOCALAPPDATA%\ssmcp\auth`。`SSMCP_AUTH_HOME` 可指定独立的绝对目录。Unix 目录与文件权限分别为 0700 和 0600；Windows 使用当前用户的私有访问权限。

服务器保存令牌摘要，并按已验证的 GitHub 数字 ID 判断身份。CLI 长期登录令牌不写入邀请命令。邀请本身允许一次受限接入，应只交给需要控制的设备操作者。

已有 SSH 别名、公钥和本地后台会话继续可用：

~~~sh
ssmcp --ssh --name workstation my-ssmcp
ssmcp --identity ~/.ssh/id_ed25519 -p 2222 alice@ssmcp.example.com

# 显式共享给指定 GitHub 用户时继续使用 SSH --allow。
ssmcp --ssh --allow alice@github,bob@github my-ssmcp

ssmcp sessions list
ssmcp sessions status workstation
ssmcp attach workstation
~~~

已登录时，SSH 路径省略 `--allow` 会先核验账号的 GitHub 稳定身份，再自动填写自己的访问规则。GitHub 改名、用户名归属不符或无法核验时会停止这次自动授权并提示重新登录或重试。显式 `--allow` 保留原有语义；HTTPS 账户连接固定归属当前登录者。SSH 首次配置仍可用 `ssmcp setup`。[完整 SSH 配置](configuration.md#ssh-config)

## 6. 服务器配置

客户端登录依赖现有 GitHub OAuth 和账号门户。`public_url` 应为浏览器与 CLI 实际访问的 HTTPS origin；生产入口应提供有效 TLS 证书。GitHub callback 保持 `/oauth/callback/github`。[OAuth 与门户部署](deployment.md#github-oauth)

要让没有安装 ssmcp 的 B 直接执行临时命令，在服务端配置：

~~~toml
[server]
client_download_dir = "/var/lib/ssmcpd/clients"
~~~

也可使用 `SSMCP_CLIENT_DOWNLOAD_DIR` 或 `ssmcpd serve --client-download-dir PATH`，优先级为命令行、环境变量、TOML。目录必须是绝对路径，路径各层不含符号链接，末级目录由 root 或服务用户拥有且不可被组或其他用户写入。

从对应平台的发布归档提取原始客户端二进制，按下表命名放入目录。可以只准备实际需要的平台。

| B 平台 | 服务端目录中的文件名 |
| --- | --- |
| Linux x86_64 | `ssmcp-x86_64-unknown-linux-gnu` |
| Linux ARM64 | `ssmcp-aarch64-unknown-linux-gnu` |
| macOS x86_64 | `ssmcp-x86_64-apple-darwin` |
| macOS ARM64 | `ssmcp-aarch64-apple-darwin` |
| Windows x86_64 | `ssmcp-x86_64-pc-windows-msvc.exe` |
| Windows ARM64 | `ssmcp-aarch64-pc-windows-msvc.exe` |


同一目录还需要 `release.json`。它是此网关发布的客户端版本声明；`version` 必须与可执行文件一致，`wire_version` 必须为当前 wire 7，`targets` 只列出已准备的六种固定平台中的子集。每个条目提供原始可执行文件的 `size` 和 `sha256`。例如：

~~~json
{
  "schema": 1,
  "version": "0.9.0",
  "wire_version": 7,
  "targets": {
    "x86_64-unknown-linux-gnu": {
      "size": 12345678,
      "sha256": "<替换为该原始可执行文件的64位小写SHA-256>"
    }
  }
}
~~~

示例大小和摘要必须替换为真实值。复用[现有 Release 流程](../.github/workflows/release.yml)生成的目标归档，先按同次发布的 `SHA256SUMS` 核验归档，再提取客户端并计算原始文件摘要；不另建编译或发布流水线。选择已经验证与当前 wire 7 兼容的发布，公共 GitHub `latest` 的版本不会被这个引导入口自动采用。先原子替换准备好的平台文件，最后原子替换发布清单；发布切换中的不一致会在客户端下载校验时停止，B 可在更新完成后重新使用尚未兑换、未过期的命令。[发布说明](releases.md) · [下载校验](../src/client_downloads/install.sh)

清单上限 16 KiB，每个可执行文件上限 256 MiB。清单和可执行文件均须是 root 或服务用户拥有、不可被组或其他用户写入的普通文件，不接受符号链接；所有已列出的平台文件必须存在且大小一致。网关异步工作通过有界磁盘读取取得发布信息，MCP 与下载路由共享同一个已打开目录，不根据请求 Host 或另一个配置源选择下载。

引导脚本为 `/client/install.sh`、`/client/install.ps1`；`/client/release.json` 提供发布元数据，`/client/release/{target}` 提供脚本读取的固定五行投影，`/client/bin/{target}` 提供原始二进制。下载固定使用配置的 HTTPS origin，拒绝跳转。清单、版本、平台或文件不完整时返回 503；已安装兼容客户端的 B 仍可使用 `invite --no-bootstrap`，普通账号连接不依赖下载目录。[下载服务实现](../src/client_downloads.rs)

反向代理须把 `/setup`、`/api/client/*`、`/mcp` 与 GitHub 回调交给同一个 Broker 实例；同一浏览器授权还会使用账号门户路由。`/api/client/connect` 需要转发 WebSocket Upgrade，`/client/*` 需要转发到提供下载的新版服务。把新登录页与旧 API 分到两个独立进程，会使配对状态或会话列表不一致。以完成登录、B 接入、A 列表与交互的完整链路验收部署。

## 常见问题

| 现象 | 检查 |
| --- | --- |
| 登录接口 404 | 服务端尚未提供客户端账号 API，更新实际运行的 ssmcpd 和代理路由 |
| B 引导下载返回 503 | 核对 `client_download_dir`、`release.json` 的 wire 7 和版本、列出的文件与大小；已安装兼容客户端可使用 `--no-bootstrap` |
| 已设置默认服务器但仍提示未登录 | 运行 `ssmcp config get server`，然后在该服务器执行 `ssmcp login` |
| 公钥不匹配，HTTPS / WSS 连接失败 | 通过可信管理员渠道核对 HTTPS 入口是否换钥，再更新 `trusted-servers.toml` |
| 固定公钥后邀请不再下载安装程序 | B 先安装新版 ssmcp，随后执行生成的信任登记与接入命令 |
| A 列表为空 | 核对 `whoami` 的账号与服务器，确认 B 的前台进程仍在线 |
| 邀请已使用或过期 | 在 A 重新执行 `invite`，复制新命令 |
| 临时会话结束后没有恢复 | 检查到期、注销或手动停止；这些操作会结束恢复权限 |
| 想查看本机后台服务 | 使用 `ssmcp sessions list`，账号列表使用 `ssmcp list` |
| 非交互脚本需要列表 | 使用 `ssmcp list --json`；`-i` 需要真实终端 |
| 名字无效或命中保留词 | 按上述 3–32 字符规则换一个名字 |
| 名字已被占用 | 使用原绑定 GitHub 账号登录，或选择未注册名字 |
| 当前账号已绑定其他名字 | 使用原名字连接；需要切换 GitHub 账号时先 `ssmcp logout` |
| 账号已封禁 | 联系该服务器的管理员；反复登录不会解除封禁 |

引导离线验收：`python3 scripts/test_client_bootstrap.py -v` 使用临时 HOME / PATH 与假下载器，覆盖平台选择、同源校验、旧安装保留和绝对路径接入。模拟平台名不等于在 macOS / Windows 实机完成验收；PowerShell 和跨平台原生结果应按实际运行环境另行记录。

授权与目录验收入口：`cargo test --lib oauth::tests::delegation`、`cargo test --lib device::tests::oauth_`、`cargo test --lib client_downloads::tests`。它们覆盖原授权期限、resource、refresh replay、一次兑换、HTTP / WebSocket 撤销和清单边界；部署仍需检查实际网关工具目录与发布状态。

其他验收入口：`cargo test --lib client::account::named_tests`、`cargo test --lib client::named_entry_tests`、`cargo test --lib client::account::tests`、`cargo test --lib device::tests`、`python3 scripts/e2e_account_interactive.py --bin-dir target/debug`。测试运行范围和本次结果见[客户端账号验收记录](validation-client-accounts.md)。
