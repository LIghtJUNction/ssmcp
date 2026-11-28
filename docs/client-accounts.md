# 客户端账号、授权与被控设备

在要开放的设备和项目目录中启动：

```sh
ssmcp alice@relay.example
```

首次启动时，在被控端打开所选中继的设备授权页面，核对短码并登录与目标用户名对应的 GitHub 账号。请分别确认账号归属和控制权限。目标账号获准后可以控制会话、运行命令并访问会话可读的文件。如果尚未注册中继用户名，可以在命令中填一个可用名称，在确认页面登记到当前 GitHub 账号。授权完成后，设备保存中继签发的设备凭据。凭据过期或撤销后需重新登录。控制者在 MCP 应用中另行完成 OAuth。[客户端入口](../src/client.rs) · [服务端检查](../src/portal/setup.rs)

要连接中继，请写出 `ssmcp username@server`。`ssmcp local` 启动仅在本机运行的会话。远程设备登录使用 HTTPS。没有桌面浏览器时，在被控设备执行 `ssmcp login alice@relay.example --no-browser`，在浏览器打开命令显示的地址，核对短码并确认。命令成功后再运行会话或启动后台服务。

## 设置控制者账号

控制者在中继的账户网页用 GitHub 登录。账户页显示服务器分配的用户名，可在“修改用户名”中选择自己的名称。把这个名称和中继地址交给被控设备的使用者即可。用户名是 3–32 个 ASCII 字符，首字符为字母，末字符为字母或数字，中间可用下划线和连字符。[用户名规则](../src/username.rs) · [账户网页](../src/portal/view.rs)

设备端可通过 `ssmcp whoami alice@relay.example --json` 查询该用户名的公开信息。此查询按命令中给出的中继和用户名执行。

## 公告

在中继网页登录后，账户页可以保存个人公告。留空恢复默认的 `{username} · {time} · 你好`。在自定义文字中也可使用这两个变量，时间显示为 UTC。管理员在同一页面维护全站公告，留空可清除。公告会显示在连接到该账号的客户端顶部，每 30 秒刷新，并可通过账号只读接口查询；请使用可公开显示的文字。

## 设备上的控制账号记录

设备在本机记录由中继验证的控制账号。身份由中继地址、身份提供方和稳定账号 ID 组成；登录名只用于显示。首次连接时，为本机明确选择、已由中继解析的目标预留位置。首次经中继验证的控制操作确认其默认身份。设备保留后续不同账号的记录，等待本机操作者批准。

在设备终端运行 `ssmcp controllers list --json` 查看身份、状态和时间。仅完成中继用户名解析时，`first_verified` 为空；收到归属已验证的控制操作后，该字段记录观察时间。摘要时间按分钟合并。运行 `ssmcp controllers history --json` 可按序号查看最近 256 条事件，区分目标解析、每条真实连接上控制者首次获准或被拒、以及本机批准和撤销。同一账号在同一连接连续执行工具只记录一次访问。新连接或准入结果变化会产生新的访问记录。升级已有的汇总记录时会保留原批准状态和时间摘要，逐条访问记录从新版本观察到的事件开始。

尝试连接第二个中继账号后，SSMCP 会在被控设备上给出对应的批准命令。可以用已记录的 `用户名@中继` 批准或撤销。如果该名称对应多个已观察身份，或记录里只有 GitHub 登录名，请使用列表中的中继地址、身份提供方和稳定账号 ID。

~~~sh
ssmcp controllers list --json
ssmcp controllers history --json
ssmcp controllers approve bob@relay.example
ssmcp controllers revoke bob@relay.example
ssmcp controllers approve https://relay.example github 12345
ssmcp controllers revoke https://relay.example github 12345
~~~

`12345` 是示例稳定账号 ID。命令在本机终端显示选中的身份并等待操作者输入 `yes`。批准只影响这台设备的本地策略，中继仍会检查控制者的身份和设备名单。

## 从控制端发现设备

在 ChatGPT 等 MCP 应用添加该中继的 `/mcp` 地址，并通过 OAuth 授权控制者账号。调用 MCP 的 `list`，根据工作目录和设备信息选择连接，再用实际返回的客户端 ID 查询工具与调用。中继逐次核对账号、连接和授权名单。[MCP 入口](../src/http.rs) · [设备路由](../src/device.rs)

## 在设备上查看本机会话

`ssmcp list --json` 列出当前用户的本机会话。每行的 `target` 可以原样传给 `ssmcp attach`。保存的后台定义使用 `saved:名称`；登记中的前台使用 `live:实例ID`。两个前台的显示名可以相同，请用列表返回的完整目标选定实例。

~~~sh
ssmcp list --json
ssmcp attach saved:workstation
ssmcp attach --read-only saved:workstation
ssmcp attach -- list
~~~

最后一条连接名为 `list` 的本机会话。Ctrl-] 分离后原会话继续运行。出现本机名称缺失、定义有误或会话停止时，可运行 `ssmcp sessions status NAME` 检查。[本机会话服务](session-service.md)

### 查看线程历史

成员列表中的右键操作按实际点击行的身份打开当前事件。进程视图显示本进程仍保留的事件，成员视图只显示能够核对该成员 ID 的事件；没有先左键选中也不会误读另一行。Esc 或再次右键返回成员页，其他对话页仍可用右键切换日志。[界面选择](../src/client/terminal/conversation/history.rs) · [鼠标路由](../src/client/terminal/conversation.rs)

线程归属来自真正调用时冻结的账号、连接和成员信息。事件按这个来源筛选；未带身份的记录与已经离开保留窗口的内容不会被补造为某个成员的聊天。无法归属的保留记录留在进程视图，本机界面也不会把自己的日志标成另一个客户端的事件。[来源记录与分页](../src/session/attach/thread_history.rs) · [调用身份](../src/runtime/call_scope.rs)

线程事件使用当前进程的有界内存窗口，最多保留 4096 个事件、2 MiB。当前读取接口为 `ssmcp.ui.events`，使用 `action: "thread"` 与实际 `client_id`；可传 `agent_id` 或 `main: true`，二者互斥。不传这两个过滤参数时返回所选进程范围的聚合事件，包括无法归属到具体线程的内容。分页沿用 `generation` 和 `next_cursor`，并检查 `retention_gap_possible`。[事件参数与分页](conversation-history.md)

同一进程断网重连保留内存窗口；退出、重启或 `/reload` 会清空。旧会话和记忆文件不参与恢复，也不会被迁移删除。控制端可通过获准连接的原生事件接口读取保留记录。[窗口限制](../src/session/attach.rs)

<a id="聊天接口的兼容与保留边界"></a>

### 中继保存的请求与回复

中继的会话 I/O 接口按账号、连接和接收者记录请求与回复。接收端通过 `ssmcp.ui.reply` 提交回复。单个请求的保留期自创建起最长一小时，服务端重启会清空内存队列。连接或成员失效后，待处理输入标为过期。[HTTP 接口](../src/device/session_io.rs) · [回复确认](../src/http/session_io.rs) · [服务端记录](../src/broker/session_io.rs)

## 修改名单与连接生命周期

在被控端会话里输入以下命令。用户名由当前中继解析，名单以服务器回执为准。

| 命令 | 作用 |
| --- | --- |
| `/allow add bob` | 允许已注册用户 `bob` 控制这条会话 |
| `/allow list` | 显示当前会话已经确认的控制名单 |
| `/allow remove bob` | 撤销 `bob` 对这条会话的控制权 |

撤销按稳定账号 ID 生效，已发请求在返回结果前还会再次检查。至少要保留一个获准账号。旧启动命令和已保存定义如带有 `--allow`，请改用 `ssmcp 用户名@中继`，启动后在需要共享的会话里添加控制者。[连接内修改](../src/broker/access.rs) · [调用与返回检查](../src/broker.rs)

设备凭据被撤销后，需重新完成设备登录。单个控制者的流量额度用尽时，其他仍有授权和额度的控制者可以继续使用同一设备。设备断开后，控制者可以沿用自己的 MCP 登录查看其他获准设备。[登录复核](../src/device.rs) · [管理员封禁](../src/broker/management.rs)

## 同时在线的会话数

启用计费的中继按设备启动者的稳定账号 ID 统计在线会话。免费账号已有一条在线会话时，再启动一条会收到 `online_session_limit`。原会话断开后可以再连。控制会话与工作池若分别向中继注册，会分别占用名额。通过同一会话增加一位控制者不会多占名额。同一账号更名后沿用原有名额。完成正式充值的账号没有这一项账号级上限。自建中继关闭计费后仍需验证设备身份，沿用原有连接容量。

## 流量按实际控制者计费

每个请求和对应回复都绑定服务端验证的实际控制账号。设备启动者用来统计同时在线的会话数。共享设备时，流量仍按发起调用的控制者结算。Hello、Ping 和本地名单操作沿用协议已有的计量规则。[请求归属](../src/broker.rs)

同一计费周期先分别累计该账号上传和下载字节，再取较大值；旧账本中无法恢复方向的历史用量单独保留：

~~~text
本月用量 = 旧格式历史用量 + max(累计上传, 累计下载)
~~~

额度拒绝发生在请求发送前时，提示本次未执行；结果发送受限时，提示工具可能已经执行但结果未送达。补充流量后可以发起新请求，本次请求不会自动重放，不要求其他控制者重连。[方向账本](../src/portal/traffic.rs) · [错误状态](../src/user_policy.rs)

免费、付费账户的每月赠送额度可由服务器分别配置；付费身份来自已有的成功生产充值记录。改变赠送上限保留本月已用量，单账户的管理员调整优先。额度详情和下月恢复时间来自真实登录账号的服务端快照，见[月度额度配置](configuration.md#服务端月度赠送额度)。

## 安装、服务器与验收

客户端平台和安装位置见[版本与安装](releases.md)，HTTPS 公钥固定见[服务器信任](client-server.md)。服务端必须启用 GitHub OAuth 与持久账号存储，并正确设置公开 HTTPS origin；反向代理需要转发 `/mcp`、账号 API、OAuth 回调和 `/api/client/connect` 的 WebSocket Upgrade。[部署说明](deployment.md)

设备客户端和中继都需支持这次设备 OAuth 握手。服务端在升级连接时明确确认已验证设备；旧服务缺少确认时，新客户端会提示升级。

本地回归入口：

~~~sh
cargo test --locked --offline --lib anonymous_allow_
cargo test --locked --offline --lib device::tests::
cargo test --locked --offline --lib oauth::
cargo test --locked --offline --lib broker::traffic::tests::
cargo test --locked --offline --test allow_account
~~~

这些测试使用隔离账号存储、loopback HTTP / WebSocket 和假登录凭据，覆盖名单解析、实际控制端认证、请求与结果计费、共享账号隔离及 CLI 输出；不向真实账号授权，也不部署服务器。

服务端请求归属和本机事件过滤另有定向测试。

~~~sh
cargo test --locked --offline --lib broker::session_io::tests
cargo test --locked --offline --lib http::session_io::tests
cargo test --locked --offline --lib session::attach::thread_history::tests
~~~
