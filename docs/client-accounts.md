# 客户端账号、会话与临时接入

在 A 设备登录一次 GitHub，随后运行 `ssmcp` 就能以自己的身份接入服务器，无须重复填写自己的 `--allow`。已登录的 A 可以列出可控会话、进入远程终端，也可以生成一条命令，让 B 临时成为自己的被控端。

使用这些命令前，安装包含账号与服务器配置命令的新版客户端，并确认服务器已提供客户端账号 API。下载引导流程还要求服务器准备目标平台的客户端二进制。示例域名 `https://ssmcp.example.com` 应替换为你的服务器。

## 1. 登录并接入自己的设备

在 A 上安装客户端后运行：

~~~sh
ssmcp config set server https://ssmcp.example.com
ssmcp login
ssmcp whoami

# 在需要开放的项目目录中运行，保持这个终端打开。
ssmcp --name device-a
~~~

`login` 打开浏览器，使用 GitHub 登录并确认终端显示的配对信息。没有图形浏览器时用 `--no-browser`，在另一台设备打开打印的链接即可。`whoami --json` 返回已验证身份、服务器和有效期，不输出登录令牌。

默认地址按 `--server`、配置文件的 `[client].server`、最后登录的服务器、内置地址依次选择。`ssmcp config set server URL` 保存默认地址，`ssmcp config get server` 显示生效值，`ssmcp config path` 定位可编辑文件。不同服务器各自保存凭据，不会把一个服务器的令牌交给另一个服务器。[配置与优先级](client-server.md#设置默认服务器)账户连接通过 HTTPS / WSS 建立；A、B 都不需要 SSH 密钥或入站监听端口。服务端在注册时绑定已经验证的 GitHub 数字 ID。[客户端账号实现](../src/client/account.rs) · [服务端授权](../src/device.rs)

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

把输出的完整单行命令复制到 B 的终端。未固定服务器公钥时，默认命令从所选服务器下载对应平台的临时客户端，然后前台运行 `join`。B 只需要能访问这台服务器；无需安装 SSH、注册 GitHub 账号或取得 A 的长期令牌。下载文件放在私有临时目录，结束后清理，不修改 B 的 PATH 或注册后台服务。

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

`--ttl` 接受 30–3600 秒，`--session-ttl` 接受 60–86400 秒。POSIX 引导命令需要系统 Shell 和 curl 或 wget；Windows 使用 PowerShell / .NET。服务器管理员须准备相应平台的客户端，配置方法见下文。

**对已登记可信公钥的服务器，`invite` 自动生成预安装客户端命令。** B 应先从可信来源安装新版 ssmcp；输出命令把 A 的公钥指纹保存到 B 的 `trusted-servers.toml`，成功后再执行 `join`。该流程适用于 POSIX Shell 和 PowerShell，避免在公钥校验生效前下载执行安装脚本。公开信任记录会留在 B，临时设备令牌仍只驻留进程。[信任文件与邀请](client-server.md#公钥固定后的临时邀请)

手工使用已安装客户端时，命令形式为：

~~~sh
ssmcp join --server https://ssmcp.example.com --token '<一次性邀请令牌>'
~~~

B 收到的设备凭据只允许注册自己的被控会话，不能列出 A 的其他设备、调用其他会话或签发新邀请。设备凭据仅保存在运行进程内；网络暂时中断时，在有效期内复用它重连，不再次兑换邀请。退出 B 进程后需要在 A 生成新命令。B 的进程使用运行命令的本机用户权限。[邀请与设备凭据](../src/device.rs) · [客户端传输](../src/client/relay.rs)

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

客户端登录默认有效 30 天，到期后重新运行 `ssmcp login`。Linux / macOS 默认在 `$XDG_CONFIG_HOME/ssmcp/auth` 或 `~/.config/ssmcp/auth` 保存凭据；Windows 使用 `%LOCALAPPDATA%\ssmcp\auth`。`SSMCP_AUTH_HOME` 可指定独立的绝对目录。Unix 目录与文件权限分别为 0700 和 0600；Windows 使用当前用户的私有访问权限。

服务器保存令牌摘要，并按已验证的 GitHub 数字 ID 判断身份。CLI 长期登录令牌不写入邀请命令。邀请本身允许一次受限接入，应只交给需要控制的设备操作者。

已有 SSH 别名、公钥和本地后台会话继续可用：

~~~sh
ssmcp --name workstation my-ssmcp
ssmcp --identity ~/.ssh/id_ed25519 -p 2222 alice@ssmcp.example.com

# 显式共享给指定 GitHub 用户时继续使用 SSH --allow。
ssmcp --allow alice@github,bob@github my-ssmcp

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

引导脚本通过 `/client/install.sh`、`/client/install.ps1` 获取，二进制通过 `/client/bin/{target}` 获取；下载源固定为配置的服务器地址。未配置下载目录或缺少目标平台文件时返回 503，并提示管理员配置 `client_download_dir`。已安装兼容客户端的 B 可使用 `invite --no-bootstrap`。[下载服务实现](../src/client_downloads.rs)

反向代理须把 `/setup`、`/api/client/*`、`/mcp` 与 GitHub 回调交给同一个 Broker 实例；同一浏览器授权还会使用账号门户路由。`/api/client/connect` 需要转发 WebSocket Upgrade，`/client/*` 需要转发到提供下载的新版服务。把新登录页与旧 API 分到两个独立进程，会使配对状态或会话列表不一致。以完成登录、B 接入、A 列表与交互的完整链路验收部署。

## 常见问题

| 现象 | 检查 |
| --- | --- |
| 登录接口 404 | 服务端尚未提供客户端账号 API，更新实际运行的 ssmcpd 和代理路由 |
| B 引导下载返回 503 | 配置 `client_download_dir`，放入 B 对应平台的二进制 |
| 已设置默认服务器但仍提示未登录 | 运行 `ssmcp config get server`，然后在该服务器执行 `ssmcp login` |
| 公钥不匹配，HTTPS / WSS 连接失败 | 通过可信管理员渠道核对 HTTPS 入口是否换钥，再更新 `trusted-servers.toml` |
| 固定公钥后邀请不再下载安装程序 | B 先安装新版 ssmcp，随后执行生成的信任登记与接入命令 |
| A 列表为空 | 核对 `whoami` 的账号与服务器，确认 B 的前台进程仍在线 |
| 邀请已使用或过期 | 在 A 重新执行 `invite`，复制新命令 |
| 临时会话结束后没有恢复 | 检查到期、注销或手动停止；这些操作会结束恢复权限 |
| 想查看本机后台服务 | 使用 `ssmcp sessions list`，账号列表使用 `ssmcp list` |
| 非交互脚本需要列表 | 使用 `ssmcp list --json`；`-i` 需要真实终端 |

验收入口：`cargo test --lib client::account::tests`、`cargo test --lib device::tests`、`python3 scripts/e2e_account_interactive.py --bin-dir target/debug`。测试运行范围和本次结果见[客户端账号验收记录](validation-client-accounts.md)。
