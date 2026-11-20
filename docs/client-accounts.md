# 客户端账号、授权与被控设备

被控设备用 `ssmcp --allow 用户名@中继服务器` 指定控制者，设备本身不需要登录。控制者必须在该服务器完成真实账号登录；中继服务器只把请求交给明确允许该账号的设备。CLI 的 `invite` / `join` 和 MCP 的 `invite` 入口已移除；`collab.join` 仍用于会话内 agent 成员登记。[客户端入口](../src/client.rs) · [服务端检查](../src/device.rs) · [公开工具目录](../src/http.rs)

## 被控端：明确指定控制者

先在目标设备安装客户端，再在要开放的项目目录运行：

~~~sh
ssmcp --allow alice@ssmcp.lmm.best --name device-b

# 使用其他中继时，同时选择服务器与该服务器上的账号。
ssmcp --server https://relay.example \
  --allow alice@relay.example,bob@relay.example --name shared-device
~~~

`alice` 是该中继门户中已经注册的账号用户名。用户名使用服务端登记的小写形式，并绑定稳定账号 ID；GitHub 昵称、caller 显示名和 agent 名称都不会授予权限。每条连接最多允许 32 个不同账号，可重复 `--allow` 或使用逗号分隔。显式名单仅授权列出的人；本机保存了其他登录也不会自动把该账号加入名单或发出其登录凭据。[目标解析](../src/server_endpoint.rs) · [账号查找](../src/broker/allow.rs)

目标的服务器 origin 必须与当前选择的服务器完全匹配。HTTPS 的默认 443 端口会规范化，非默认端口保留；`alice@relay.example:8443` 需要选择 `https://relay.example:8443`。裸用户名、`@github`、未知账号、被封禁账号和混入其他服务器的名单都会被拒绝。开发机 loopback HTTP 使用明确的 `alice@http://127.0.0.1:PORT`；普通主机名省略协议时表示 HTTPS。[端点规则](../src/server_endpoint.rs)

## 旧服务端：显式使用已验证的自身账号

如果旧服务端尚不支持匿名 `--allow`，而受控设备已经保存该服务器的有效登录，可显式选择认证兼容模式：

~~~sh
ssmcp --account-compat --allow alice@ssmcp.lmm.best

# 其他服务器仍须显式选择同一个 origin。
ssmcp --server https://relay.example \
  --account-compat --allow alice@relay.example
~~~

此模式仅接受一个 `--allow` 账号。客户端使用同一 origin 的现有未过期凭据调用 `whoami`，核对稳定账号 ID、已保存的登录身份和服务端真实注册用户名；GitHub 显示名不能替代注册用户名。无凭据、凭据撤销或过期、账号不匹配、多个账号、跨服务器或验证失败都会停止连接。它不会打开 OAuth、注册用户名或更新保存的凭据。[验证入口](../src/client/account.rs)

**这是已认证连接。该账号拥有连接，包括账号管理页查看和断开连接、审计可见性；连接沿用该账号的额度。** 客户端会在连接时明确提示，并在连接 JSON 中标记 `account_compat: true`、`ownership: "verified_account"`、`quota: "account"`。访问列表编辑不可用，握手不能返回登录码或委派能力。保存的会话仍保留原始 allow 及此显式开关，每次重连重新验证相同绑定。[握手和会话保存](../src/client.rs)

未指定 `--account-compat` 时，匿名 `--allow` 的行为不变，不会自动携带本机凭据或切换为账号所有。服务端升级完成后可移除此开关，使用标准匿名模式。兼容模式不负责升级服务端，也不要求重启其他在线用户。

零参数 `ssmcp` 和命名账号连接原本已使用登录账号的所有权及额度。它们在注册完成前遇到旧 relay 的名单格式拒绝时，会再次验证同一账号，并明确提示后只重试一次。显式匿名 `--allow` 不进入这条路径；验证失败或第二次拒绝会停止连接。

## 已登录客户端：省略名单仅允许自己

在需要登录的客户端执行：

~~~sh
ssmcp alice@relay.example
ssmcp whoami --server https://relay.example
ssmcp whoami --server https://relay.example --json
~~~

首次指定账号时，客户端引导浏览器授权并绑定用户名；已有有效凭据时复用。后续运行 `ssmcp --server https://relay.example` 可以省略 `--allow`，此时仅授权已登录账号自己。没有有效保存登录又没有显式名单时，程序会给出错误，不会建立一个无身份限制的设备连接。[登录与默认名单](../src/client.rs) · [凭据存储](../src/client/account.rs)

用户名使用 3–32 个 ASCII 字符，首字符是字母，末字符是字母或数字，中间还可使用 `_` 和 `-`；保留名不能注册。每个账号在同一服务器绑定一个名字，其他账号不能通过重名或更换 GitHub 昵称接管它。[用户名规则](../src/username.rs) · [永久绑定](../src/portal/store.rs)

默认服务器依次由 `--server`、配置文件、最近登录的服务器和内置默认值选择。可用 `ssmcp config set server URL` 保存默认值；不同服务器的凭据分别保存。[服务器配置与可信公钥](client-server.md)

## 控制端：登录后查看与调用

控制者登录后，可以按真实的进程或线程身份连接对话。这里的进程指一条在线客户端连接；线程指该连接中经 `collab.join` 登记的成员。主接收入口（main）接收未绑定成员的调用，使用 `client:CLIENT_ID` 标识；它不是另一个模型实例。[目录来源](../src/broker/session_io.rs) · [目标解析](../src/client/account/directory.rs)

~~~sh
ssmcp list --server https://relay.example
ssmcp list --server https://relay.example --json
ssmcp attach process PROCESS_ID
ssmcp attach thread THREAD_ID
ssmcp attach thread THREAD_NUMBER --read-only
ssmcp attach thread client:CLIENT_ID
ssmcp attach --local workstation
~~~

ChatGPT 等 MCP 应用使用该服务器的 `/mcp` OAuth 入口。CLI 登录和 MCP 应用授权分别管理，二者都需要真实账号身份。`list` 只返回当前账号有权控制的在线连接及其真实登记成员；知道 ID、修改显示名称或填写 `--allow` 字符串都不能替代控制端认证。[HTTP 控制接口](../src/device.rs) · [MCP 授权](../src/http.rs)

### 选择进程与线程

`ssmcp list` 的树形目录把进程、主接收入口和已登记线程分别编号。进程可用完整客户端 ID、确切且唯一的进程名称或列表序号选择；线程使用列表给出的完整成员 ID、唯一的 `agent_…` ID 或序号。名称只是显示信息，同名线程不能互相替代。[目录与选择实现](../src/client/account/directory.rs)

数字绑定最近一次显示的列表快照，保留 5 分钟，并绑定服务器 origin 和登录身份。列表重新排序不会改变数字原来指向的对象；对象或成员消失、类型不符、快照过期时会要求重新选择。用数字选择进程时，接收集合固定为那次列表中的主接收入口和成员；后来加入的成员不会被悄悄加入本次发送。[快照校验](../src/client/account/directory.rs)

远程 `attach` 默认连接聊天输入与输出。按 Enter 提交输入；进程输入发给所选目录中的主接收入口及已登记线程，线程输入只发给确切的目标。Ctrl-] 或 Ctrl-C 退出观察，已经提交的工作继续运行。`--read-only` 仅查看，不发送聊天输入、终端输入或终端尺寸变更。[聊天连接](../src/client/account/chat.rs) · [attach 路由](../src/client/account/attach.rs)

原有 Shell PTY 需要显式选择。使用 `ssmcp -i` 打开终端选择器，或指定进程与已存在的终端 ID；普通 PTY 模式的 Ctrl-C 发给远端 Shell，Ctrl-] 分离。这里的终端编号属于即时终端选择器，不会改写 `ssmcp list` 保存的聊天目录编号。[PTY 连接](../src/client/account.rs)

~~~sh
ssmcp -i --server https://relay.example --client PROCESS_ID
ssmcp attach process PROCESS_ID --terminal main
ssmcp attach process PROCESS_ID --terminal main --read-only
~~~

本地保存会话仍可离线连接。`ssmcp sessions list` 与裸 `ssmcp attach` 查看本机保存的会话；已有的普通本地名称优先走本地路径，`--remote` 明确选择在线对象，`--local` 明确选择本地保存会话。数字或 `process`、`thread` 等有语法含义的本地名称可用 `ssmcp attach --local -- NAME`。本地定义损坏、权限错误或会话停止均报告本地错误。[本地与远程选择](../src/client/account/attach.rs)

### 发送输入与等待实际回复

`send` 提交一条输入，并把请求回执 JSON 写到 stdout。回执包含请求 ID 和本次固定的接收集合；进程发送最多覆盖 32 个主入口及成员，线程发送只有一个接收者。输入可以是一个带引号的参数，也可以从 UTF-8 stdin 读取；进度提示写到 stderr，便于管道传递回执。[发送实现](../src/client/account/send.rs)

~~~sh
ssmcp send thread THREAD_ID '检查失败用例'
ssmcp send process PROCESS_ID --stdin < request.txt
ssmcp send thread THREAD_ID '检查失败用例' --request-id io_review_01
~~~

回执确认输入已入队，实际交付要等接收者下一次符合身份条件的原生调用响应。主接收入口只交给匹配账号的未绑定调用，成员输入只交给匹配的已认证成员；`send`、`wait` 和 `attach` 不启动模型或新 agent。重试同一个请求 ID 必须保持文字和完整接收集合一致；只得到入队或交付确认，还不能当作任务完成。[交付边界](../src/http/session_io.rs) · [请求与身份检查](../src/broker/session_io.rs)

`wait` 只把 `result/done` 或 `result/error` 当成最终回复。默认等待全部输入；`--any` / `--any-of` / `--anyof` 等待任意一个，`--all` / `--gather` 等待全部，`--count N` / `--n-of N` 等待 N 个不同输入的结果。同一个线程回答两条不同请求可以计为两个结果，每个 `input_id` 只计一次；交付确认和 `progress/running` 不计入数量。[等待与计数](../src/client/account/wait.rs)

~~~sh
# 输出实际回复文字；请求回执通过管道进入 wait。
ssmcp send process PROCESS_ID '检查当前改动' | ssmcp wait --any

# 所选进程至少有两个接收者时，等待其中两个结果。
ssmcp send process PROCESS_ID '检查当前改动' |
  ssmcp wait --count 2 --timeout 120

# 把实际回复交给另一个已登记线程。
ssmcp send thread THREAD_ID '整理当前结论' |
  ssmcp wait --gather |
  ssmcp send thread NEXT_THREAD_ID --stdin

# 已有请求可再次读取；--json 输出每行一个最终回复对象。
ssmcp wait io_review_01 --all --json
ssmcp send thread THREAD_ID '检查失败用例' --wait --json
~~~

等待结束只结束本地等待，未完成的工作继续运行。`--any` 或 `--count N` 最多输出所需数量的结果；其他结果保留在服务端，可在保留期内再次 `wait`。重复等待会重新读取已保留的结果，不是从共享队列取走结果。输出的最终回复包含错误，或等待超时，都会使命令以非零状态退出；已经写到 stdout 的真实回复仍可被下一条管道命令读取。[等待循环](../src/client/account/wait.rs)

管道输入不能改变命令选用的服务器或账号。`wait` 接受回执 JSON、JSONL 或请求 ID，并核对服务器 origin、账号和固定接收身份；需要其他服务器时显式传 `--server`。`--timeout` 限制 stdin 到达 EOF 之后的回复等待，不限制上游写 stdin 的时长。下游 `send` 的单条输入仍限 16 KiB，多个回复拼接后超过限制时需要显式拆分或缩减。[输入与回执校验](../src/client/account/wait.rs) · [输入大小限制](../src/client/account/send.rs)

### 查看线程历史

成员列表中的右键操作按实际点击行的身份打开历史。进程视图显示当前进程保留的事件，成员视图只显示能够核对该成员 ID 的事件；没有先左键选中也不会误读另一行。Esc 或再次右键返回成员页，其他对话页仍可用右键切换日志。[界面选择](../src/client/terminal/conversation/history.rs) · [鼠标路由](../src/client/terminal/conversation.rs)

线程归属来自真正调用时冻结的账号、连接和成员信息。更新后的事件可以按这个来源筛选；旧记录、未带身份的记录以及已经离开保留窗口的内容不会被补造为某个成员的聊天。无法归属的保留记录留在进程视图，本机界面也不会把自己的日志标成另一个客户端的历史。[来源记录与分页](../src/session/attach/thread_history.rs) · [调用身份](../src/runtime/call_scope.rs)

这份线程历史是当前进程的有界事件窗口。它最多保留 4096 个事件和 2 MiB 预算，重启进程会失去该窗口；原有 JSONL 历史和 `/resume` 保持各自的会话恢复用途。远程 `attach --read-only` 读取所选客户端的身份筛选历史及本账号的已确认回复，不会恢复模型内部对话状态。[窗口限制](../src/session/attach.rs) · [远程观察](../src/client/account/chat.rs)

### 聊天接口的兼容与保留边界

聊天收发需要命令所在客户端、接收端客户端和中继都支持相应接口。中继提供 `/api/client/io`、线程目录、定向输入交付和回复确认；接收端提供真实的 `ssmcp.ui.reply` 与线程历史。仅更新本机客户端不会给旧中继增加这些能力；旧接收端不能在未支持回复接口时被加入聊天队列，原有显式 PTY 入口仍可使用。[HTTP 接口](../src/device/session_io.rs) · [原生回复确认](../src/http/session_io.rs)

请求和回复最多保留 1 小时，保留期从请求创建时计算。状态位于服务端内存，服务端重启会丢失未完成请求及已保留回复；容量满时拒绝新的请求或进度，最终回复预留所需空间。连接或成员失效后，待处理输入标为过期，不会改投同名对象或新连接。这套接口不提供跨服务端重启的持久任务队列。[保留与容量实现](../src/broker/session_io.rs)

| 范围 | 限制 |
| --- | --- |
| 单次发送 | 1–32 个确切接收者；输入最多 16 KiB UTF-8，控制字符仅允许换行和 Tab |
| 单次等待 | 最多 32 个请求 ID；`--count N` 支持 1–1024，不能超过这些请求中的输入总数 |
| 等待超时 | 0.001–86400 秒；不取消接收者工作 |
| 服务端请求保留 | 全局最多 1024 条、每账号最多 128 条，最长 1 小时 |
| 服务端输出预算 | 总预算 16 MiB；每请求最多 2 MiB、256 条进度与最终输出 |
| 客户端线程历史 | 进程内最多 4096 个事件、2 MiB 预算；不补造旧记录身份 |

这些限制分别来自[输入解析](../src/client/account/send.rs)、[等待解析](../src/client/account/wait.rs)、[服务端账本](../src/broker/session_io.rs)和[事件窗口](../src/session/attach.rs)。版本号或显示名称不能替代真实工具目录和接口检查。

## 修改名单与连接生命周期

本地被控端的 Access 界面只修改承载该命令的本条连接。新增账号仍要通过当前服务器的既有账号检查；撤销按稳定账号 ID 生效，已经发出的请求也会在返回结果前再次检查。允许移除注册者，只要还保留其他获准账号；不能移除最后一个账号后退回旧的无名单授权。停止设备进程即可关闭该连接。[连接内修改](../src/broker/access.rs) · [调用与返回检查](../src/broker.rs)

一个控制者注销、封禁或额度不足，不会关闭共享给其他有效控制者的设备连接。一个账号等待限速时，其他账号仍可收发；尚未发送的请求取消后，不会在限速解除时补发。被撤销的凭据仍然失效；解除封禁不会让旧凭据复活。设备连接结束后，控制者的独立账号登录不会因此被当作设备凭据撤销。[登录复核](../src/device.rs) · [管理员封禁](../src/broker/management.rs)

## 流量按实际控制者计费

每个请求绑定服务端验证的控制账号，返回结果沿用原请求的账号。设备注册者只是来源记录，匿名设备没有注册者；名单中的第一个账号、owner 标签和 caller 提交的元数据都不是账单依据。Hello、Ping 和本地名单操作不会被随意计入某个控制者。[请求归属](../src/broker.rs)

同一计费周期先分别累计该账号上传和下载字节，再取较大值；旧账本中无法恢复方向的历史用量单独保留：

~~~text
本月用量 = 旧格式历史用量 + max(累计上传, 累计下载)
~~~

额度拒绝发生在请求发送前时，提示本次未执行；结果发送受限时，提示工具可能已经执行但结果未送达。补充流量后可以发起新请求，本次请求不会自动重放，不要求其他控制者重连。[方向账本](../src/portal/traffic.rs) · [错误状态](../src/user_policy.rs)

免费、付费账户的每月赠送额度可由服务器分别配置；付费身份来自已有的成功生产充值记录。改变赠送上限保留本月已用量，单账户的管理员调整优先。额度详情和下月恢复时间来自真实登录账号的服务端快照，见[月度额度配置](configuration.md#服务端月度赠送额度)。

## 安装、服务器与验收

客户端平台和安装位置见[版本与安装](releases.md)，HTTPS 公钥固定见[服务器信任](client-server.md)。服务端必须启用 GitHub OAuth 与持久账号存储，并正确设置公开 HTTPS origin；反向代理需要转发 `/mcp`、账号 API、OAuth 回调和 `/api/client/connect` 的 WebSocket Upgrade。[部署说明](deployment.md)

客户端与实际运行的服务器都要支持本授权模型。客户端要求服务器明确确认账号访问能力；遇到旧服务器不会回落到邀请或登录码流程。仅替换本机客户端不会更新远端服务。

本地回归入口：

~~~sh
cargo test --locked --offline --lib anonymous_allow_
cargo test --locked --offline --lib device::tests::
cargo test --locked --offline --lib oauth::
cargo test --locked --offline --lib broker::traffic::tests::
cargo test --locked --offline --test allow_account
~~~

这些测试使用隔离账号存储、loopback HTTP / WebSocket 和假登录凭据，覆盖名单解析、实际控制端认证、请求与结果计费、共享账号隔离及 CLI 输出；不向真实账号授权，也不部署服务器。

聊天收发、身份筛选与 CLI 管道有独立的本地回归入口：

~~~sh
cargo test --locked --offline --lib broker::session_io::tests
cargo test --locked --offline --lib broker::allow::tests::session_io
cargo test --locked --offline --lib http::session_io::tests
cargo test --locked --offline --lib client::account::
cargo test --locked --offline --lib session::attach::thread_history::tests
python3 scripts/e2e_account_interactive.py --bin-dir /path/to/candidate-binaries
~~~

CLI 脚本只需要候选 `ssmcp`，使用自己的 loopback HTTP 接口和真实 PTY Shell；Rust 回归另行检查真实服务端路由、WebSocket 连接和回复归属。[CLI 夹具](../scripts/e2e_account_interactive.py) · [服务端夹具](../src/broker/allow/tests/session_io.rs)
