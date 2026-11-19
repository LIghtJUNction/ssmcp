# 客户端连接、账号与授权

当前客户端通过 HTTPS / WSS 连接中继。被控端可以不登录，使用已在目标服务器注册的账号用户名明确指定控制者：

~~~sh
ssmcp --server https://relay.example --allow alice@relay.example --name device-b
~~~

控制者必须登录该服务器，才能查看和操作获准的设备。被控端的名字、工作目录和 caller 显示信息用于识别设备，不是登录凭据。[完整账号与授权说明](client-accounts.md)

<a id="github-allow"></a>

## 授权名单

`--allow USER@SERVER` 支持重复或逗号分隔，最多 32 个不同账号。服务器必须与本次连接的 origin 一致；用户名使用门户登记的小写形式，再解析到不可转让的稳定账号 ID。显式名单不会自动附加本机登录账号。裸用户名、`@github`、未知或被封禁账号、其他服务器的目标均被拒绝。[解析与注册](../src/server_endpoint.rs) · [服务端 ACL](../src/broker/allow.rs)

~~~sh
ssmcp --server https://relay.example \
  --allow alice@relay.example,bob@relay.example
~~~

不写 `--allow` 时，客户端必须已经保存有效登录，默认只允许自己。首次选择并登录账号可执行 `ssmcp alice@relay.example`；`ssmcp whoami` 查看当前账号信息，`ssmcp logout` 注销当前服务器上的这份登录。被控端没有登录时必须显式给出名单。

<a id="oauth-login"></a>

## 控制端登录与会话列表

CLI 控制者使用已保存登录，MCP 应用使用 `https://relay.example/mcp` 的 OAuth 授权。两种登录分别管理，所有控制调用仍需有效身份与目标连接的 ACL。

~~~sh
ssmcp list --server https://relay.example
ssmcp list --server https://relay.example --json
ssmcp -i --server https://relay.example --client SESSION_ID
~~~

`ssmcp sessions list` 与 `ssmcp attach NAME` 管理本机后台会话。账号下的远程列表使用 `ssmcp list`，也可显式使用 `ssmcp sessions list --remote`。远程交互中 Ctrl-] 离开当前控制界面。[账号与设备](client-accounts.md)

<a id="silent-refresh"></a>
<a id="session-lifetime"></a>

## 凭据与连接生命周期

账号凭据按服务器隔离保存，控制请求始终复核当前凭据和授权名单。设备连接本身与控制者登录分别管理：一个控制者注销、封禁或额度不足不会关闭其他获准账号正在使用的共享设备。被撤销账号的旧凭据不会因解封自动恢复。[凭据验证](../src/device.rs) · [请求路由](../src/broker.rs)

看门狗默认开启，网络连接中断后按配置重试；`--no-watchdog` 关闭，`--reconnect-delay SECONDS` 修改间隔。鉴权失败不会自动降级到未受限制的连接。CLI 的 `invite` / `join` 与 MCP 的 `invite` 已移除；设备直接使用 `--allow 用户名@服务器`，会话内 agent 的 `collab.join` 保留。

当前版本已退出旧 SSH 设备接入与登录码用法。旧的 `--ssh`、私钥参数和服务器 SSH 用户名不能代替 HTTPS 账号。已有脚本应改成上面的明确服务器和授权名单形式。

## 服务端月度赠送额度

服务器可以分别设置免费用户和付费用户的每月赠送流量，单位为字节。例如，下面给免费用户每月 1 GB、付费用户每月 10 GB：

~~~toml
[portal]
state_dir = "/var/lib/ssmcpd/portal"
free_user_monthly_traffic_bytes = 1000000000
paid_user_monthly_traffic_bytes = 10000000000
~~~

这里的“付费用户”沿用已有账户标记：该账户至少有一次经过服务端核验、成功结算的生产环境充值。测试订单、客户端标签、用户名和当前剩余额度都不能获得这一身份；目前没有订阅到期或按月续费的档位。[可信标记的写入](../src/portal/commerce.rs) · [账户记录与额度选择](../src/portal/store.rs)

两个字段都省略时，两类账户仍各赠送每月 1,000,000,000 字节，兼容旧配置。0 表示没有赠送流量，不表示无限。自定义额度需要 `portal.state_dir`。修改的是服务器策略，需由服务器加载配置后生效；本次代码改动不会自动部署或重启服务器。

额度按以下优先级计算：

1. 服务端明确配置的管理员不设月度上限。
2. 管理员对单个账户、当月设置的剩余额度优先于档位默认值。即使设置与当时默认值相同，也会保存为显式调整；之后购买的流量包仍增加该上限。
3. 其他账户使用所属档位的月度赠送额，加上当月购买的流量包。

调整档位额度或发生首次充值时，本月已累计的上传、下载和迁移前用量全部保留；上限低于已用量时剩余为 0。赠送额、购买额和管理员当月调整在下一个自然月重新计算，旧月份的使用记录保留。计费继续是 `历史迁移用量 + max(累计上传, 累计下载)`，按服务端 `commerce.utc_offset_minutes` 的固定偏移划分月份。[额度策略](../src/user_policy.rs) · [持久账本与覆盖优先级](../src/portal/traffic.rs)

`/api/client/whoami` 的 `traffic` 返回该真实登录账号的 `used_bytes`、`limit_bytes`、`remaining_bytes`，以及下一自然月开始时刻 `reset_at`（Unix 秒）。`free_bytes` 是该档位已选中的赠送额，`paid_bytes` 只表示当月另购的流量包。只有服务器明确授予的无限额度返回 `limit_bytes: null` 与 `remaining_bytes: null`；旧服务器没有 `reset_at` 时，客户端应显示时间未知。[实际 HTTP 回归](../src/device.rs)

## 配置入口

| 需求 | 文档 |
| --- | --- |
| 默认服务器、配置文件优先级 | [服务器配置](client-server.md) |
| HTTPS 入口公钥固定与指纹轮换 | [可信服务器](client-server.md) |
| 用户名注册与管理员设置 | [具名账号](named-accounts.md) |
| Linux、macOS、Windows 安装 | [版本与安装](releases.md) |
| 服务端 OAuth、账号存储与代理路由 | [部署](deployment.md) |
| 命令执行与并行任务 | [命令指南](commands.md) |
| 本机后台会话 | [会话](session-service.md) |

<a id="background-git-fetch"></a>

## 定时 Git fetch

每条已建立的工作区连接都会启动 Git 观察与 fetch 调度，独立于界面。交互式 TUI、`--log-format jsonl`、重定向输出以及后台会话使用相同设置；结束连接时停止调度并回收本连接启动的 fetch 进程。

```sh
ssmcp config get git-fetch-interval
ssmcp config set git-fetch-interval 300
ssmcp config set git-fetch-interval 0
```

`git-fetch-interval` 的默认值为 **300 秒**，允许 **60–86400 秒**，**0** 关闭后台 fetch。也可在 `ssmcp config path` 指向的 TOML 文件中设置：

```toml
[git]
fetch_interval_seconds = 300
```

客户端在**建立新连接时**读取此设置。已有连接继续使用启动时的值；重连或重新启动后采用新值，F5 的界面配置重载不会修改 fetch 间隔。`SSMCP_CLIENT_CONFIG` 可指定另一份配置文件。

调度启动后先检查一次，再按所设间隔检查。每次读取当前分支的已配置 upstream，只获取该远端分支并更新其远端跟踪引用。分离 HEAD、未配置 upstream 或 upstream 指向本地分支时跳过。fetch 保留本地 HEAD、索引、工作区、标签及手动操作的 `FETCH_HEAD`。

同一仓库及其 linked worktree 通过 Git 公共目录内的锁与时间记录协调，跳过同一目标尚未到期的重复请求。单次 fetch 最多 30 秒，失败后按次数增加等待时间；达到 `max(所设间隔, 1800 秒)` 后保持该等待上限。它使用已有的非交互式凭据，凭据失效或网络失败会结束本次尝试；交互界面的 Git 面板显示等待、获取中、已更新或失败状态。

实现与本地回归：[连接生命周期](../src/client.rs) · [fetch 调度与锁](../src/client/git_status/fetch.rs) · [无终端账户连接测试](../scripts/e2e_periodic_git_fetch.py)。

<a id="troubleshooting"></a>

## 常见问题

| 现象 | 检查 |
| --- | --- |
| 没有保存登录且未指定授权名单 | 被控端加 `--allow 用户名@服务器`；需要账号默认行为时先登录 |
| 提示授权目标属于其他服务器 | 同时核对 `--server` 与目标的主机、协议、端口 |
| 提示未知用户名 | 控制者先在该服务器注册门户用户名，不能用 GitHub 昵称代替 |
| 列表为空 | 核对当前账号、服务器与设备名单，并确认设备进程在线 |
| 旧服务器没有确认账号访问能力 | 更新实际运行的服务端；客户端不会自动降级 |
| 额度不足 | 补充流量后发起新请求；不要自动重放可能已经执行的调用 |
| 想查看本机后台服务 | 使用 `ssmcp sessions list`；远程账号设备用 `ssmcp list` |

## 本地显示与重载

真实交互终端默认启用 `[ui] dashboard = true` 和 `layout = "chat"`。主区域连续展示调用、输出与结果，底部输入框用于追加用户要求；要求进入可编辑队列，在后续工具响应中交付。`/audit` 打开源码审计，`/agents` 查看登记成员，`/queue` 查看要求队列，`/filter 文本` 筛选日志。F3 或 `/sidebar` 收起侧栏，窄窗口会自动调整可用区域。[输入、队列与界面配置](client-ui.md)

默认使用 `dark` 深色主题，也可选择 `codex-dark`、`nord`、`solarized`、`light` 或跟随终端颜色的 `minimal`，并逐项覆盖语义颜色。输入框的文字不会发给后台 PTY；直接交互使用另一个终端中的 `ssmcp -i` 或 `terminal.*`。`NO_COLOR` 只关闭颜色，`dashboard = false`、JSONL 或重定向 stderr 使用行式输出。任务进度来自原生数据，Git 状态来自授权启动目录的实际观察。[面板布局与配置](logging.md#固定状态区与窗口尺寸)

`--log-format pretty-verbose` 展开脱敏后的详细元数据，`--log-format jsonl` 输出脱敏事件 JSON；`--verbose` 是详细事件的别名，`--raw` 与 `--raw-events` 是 JSONL 的别名。重定向 stdout 时，连接信息始终是 JSON。`--no-color`、`NO_COLOR=1`、`TERM=dumb` 或重定向 stderr 会禁用 ANSI 颜色。stdout/stderr 按任务和流分别显示，结束记录包含实际退出状态与耗时；读取历史 `wait/read` 结果不重复打印正文。内联脚本默认以解释器和行数识别，正文可在详细日志中展开。`[ui] activity_interval_secs = 5` 控制行式日志的活动提示，设为 0 关闭该提示。[日志与输出保留](logging.md)

F5 或 `ssmcp.ui.action` 的 `reload` 只重载本地显示配置；输入框中的 `/reload` 请求重新启动客户端，两者的作用不同。主题、配色和布局可直接重载，`dashboard` 与 `log_format` 需要重新启动。

用户要求队列与本地成员目标需要协议 v7，源码审计需要 v6 或更新版本。队列中的已交付表示附加到符合目标的 `code_mode` 响应，不表示模型已经阅读或执行。受控重启通过私有、单次读取的交接文件保留草稿与队列；交付不确定或接收目标已变化的记录需要用户复核，不会自动重发。[队列状态与恢复](client-ui.md#输入要求与队列)

```sh
ssmcp --server https://ssmcp.lmm.best --title "检查构建与测试" --allow alice@ssmcp.lmm.best
ssmcp --server https://ssmcp.lmm.best --theme nord --timestamps --allow alice@ssmcp.lmm.best
ssmcp --server https://ssmcp.lmm.best --log-format jsonl --allow alice@ssmcp.lmm.best
NO_COLOR=1 ssmcp --server https://ssmcp.lmm.best --allow alice@ssmcp.lmm.best
```

更新已安装的程序后，可以从 `code_mode` 调用 `await reload(clientId)`，让客户端从原安装入口重新启动，并保留启动参数、目录、环境和 `--allow`。先确认目录包含 `ssmcp.client.reload`；不支持该工具的旧客户端，初次升级须在本机普通重启。`accepted:true` 只是接收应答，应再调用 `list` 确认重连及新的 ID。重载会停止本连接拥有的命令任务和转发、关闭原生桥接；不会终止操作者已运行的外部 Pi/OpenCode 服务；在途调用会导致 busy 拒绝。[重载协议与回归测试](../src/client/reload.rs)

有效的账号授权在新连接继续允许同一账号时可复用；控制请求会重新核对凭据和名单。调用方应重新发现在线设备及实际版本，不能把“程序已更新”或“重载已接收”报告为“新版本已经在线”。[重载完整说明](backends.md#9-更新后的客户端重载)


成功文件读写和可查询 Diff 来自原生操作完成后的证据；敏感、二进制或无法取得的快照明确标为未知。技能发现、正文读取和实际调用分别记录，Harness 外部副作用不作完整审计承诺。完整事件范围见[文件与技能审计](backends.md#文件与技能审计)。

## 会话标题与输出保留

`--title` 为这一条连接附加任务说明，`--name` 用于辨认机器或工作区；两者都不代替唯一客户端 ID。标题支持中文和空格，最多 512 个 UTF-8 字节。升级时保留原有服务器、`--allow` 与名称等启动选项；标题只是显示元数据，不改变账号和授权名单。首次使用标题前应确认服务端支持该字段。[参数实现](../src/client.rs) · [会话说明](session-control.md)

```sh
ssmcp --server https://ssmcp.lmm.best --name dev --title "修复构建并准备发布" --allow alice@ssmcp.lmm.best
```

重载只会重新启动原入口，不会下载更新或选择另一安装目录。Windows 手动按版本目录安装时，切换版本需停止旧进程并从新目录启动；使用安装器更新同一入口后，才可按上述重载流程接管。

长输出超过 32 KiB 或 500 行时会按来源转储，调用方获得转储 ID、客户端路径、SHA-256、字节数和行数，再用 `outputs.read` 分块读取。Unix 的默认目录是 `/tmp/ssmcp-<uid>`，其中 `<uid>` 来自当前进程的有效系统用户 ID；目录权限为 0700，输出文件为 0600。每个用户直接拥有自己的目录，不依赖另一个用户创建的共享父目录。Windows 默认是 `%LOCALAPPDATA%\ssmcp\output`，无该变量时使用用户临时目录下的 `ssmcp/output`；该路径属于目标客户端；文件仍按连接归属校验，其他客户端不能读取。默认保留 24 小时，已结束文件总量上限 512 MiB，单文件上限 256 MiB，运行期间每 60 秒清理一次。活动文件被跳过；达到单文件上限或发生写入错误时，应检查 `complete` 与 `error`，不能假定已保存完整输出。[输出实现](../src/output.rs) · [读取与清理](session-control.md#按需读取正文)
