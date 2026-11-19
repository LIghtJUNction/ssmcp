# 客户端连接、账号与授权

当前客户端通过 HTTPS / WSS 连接中继。被控端可以不登录，使用已在目标服务器注册的账号用户名明确指定控制者：

~~~sh
ssmcp --server https://relay.example --allow alice@relay.example --name device-b
~~~

控制者必须登录该服务器，才能查看和操作获准的设备。被控端的名字、工作目录和 caller 显示信息用于识别设备，不是登录凭据。[完整账号与授权说明](client-accounts.md)

<a id="github-allow"></a>

## 授权名单

`--allow USER@SERVER` 支持重复或逗号分隔，最多 32 个不同账号。服务器必须与本次连接的 origin 一致；用户名按门户规则转成小写，再解析到不可转让的稳定账号 ID。显式名单不会自动附加本机登录账号。裸用户名、`@github`、未知或被封禁账号、其他服务器的目标均被拒绝。[解析与注册](../src/server_endpoint.rs) · [服务端 ACL](../src/broker/allow.rs)

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
ssmcp --ssh --title "检查构建与测试" --allow octocat@github my-ssmcp
ssmcp --ssh --theme nord --timestamps --allow octocat@github my-ssmcp
ssmcp --ssh --log-format jsonl --allow octocat@github my-ssmcp
NO_COLOR=1 ssmcp --ssh --allow octocat@github my-ssmcp
```

更新已安装的程序后，可以从 `code_mode` 调用 `await reload(clientId)`，让客户端从原安装入口重新启动，并保留启动参数、目录、环境和 `--allow`。先确认目录包含 `ssmcp.client.reload`；不支持该工具的旧客户端，初次升级须在本机普通重启。`accepted:true` 只是接收应答，应再调用 `list` 确认重连及新的 ID。重载会停止本连接拥有的命令任务和转发、关闭原生桥接；不会终止操作者已运行的外部 Pi/OpenCode 服务；在途调用会导致 busy 拒绝。[重载协议与回归测试](../src/client/reload.rs)

GitHub 授权在仍有效、且新连接允许同一身份时继续可用。登录码授权绑定旧连接，重载断线后需要新登录码和新 OAuth 授权。调用方应按实际授权方式处理重连，避免把“程序已更新”或“重载已接收”报告为“新版本已经在线”。[重载完整说明](backends.md#9-更新后的客户端重载)


成功文件读写和可查询 Diff 来自原生操作完成后的证据；敏感、二进制或无法取得的快照明确标为未知。技能发现、正文读取和实际调用分别记录，Harness 外部副作用不作完整审计承诺。完整事件范围见[文件与技能审计](backends.md#文件与技能审计)。

## 会话标题与输出保留

`--title` 为这一条连接附加任务说明，`--name` 用于辨认机器或工作区；两者都不代替唯一客户端 ID。标题支持中文和空格，最多 512 个 UTF-8 字节。升级时保留原有 `--allow` 与 SSH 选项即可，无须为了增加标题重新登记同一把 SSH 公钥；首次使用标题前应确认服务端支持该元数据字段。[参数实现](../src/client.rs) · [会话说明](session-control.md)

```sh
ssmcp --ssh --name dev --title "修复构建并准备发布" --allow octocat@github my-ssmcp
```

重载只会重新启动原入口，不会下载更新或选择另一安装目录。Windows 手动按版本目录安装时，切换版本需停止旧进程并从新目录启动；使用安装器更新同一入口后，才可按上述重载流程接管。

长输出超过 32 KiB 或 500 行时会按来源转储，调用方获得转储 ID、客户端路径、SHA-256、字节数和行数，再用 `outputs.read` 分块读取。Unix 的默认目录是 `/tmp/ssmcp/<uid>`，Windows 默认是 `%LOCALAPPDATA%\ssmcp\output`，无该变量时使用用户临时目录下的 `ssmcp/output`；该路径属于目标客户端；文件仍按连接归属校验，其他客户端不能读取。默认保留 24 小时，已结束文件总量上限 512 MiB，单文件上限 256 MiB，运行期间每 60 秒清理一次。活动文件被跳过；达到单文件上限或发生写入错误时，应检查 `complete` 与 `error`，不能假定已保存完整输出。[输出实现](../src/output.rs) · [读取与清理](session-control.md#按需读取正文)
