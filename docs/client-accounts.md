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

控制者可以在 CLI 登录后操作：

~~~sh
ssmcp list --server https://relay.example
ssmcp list --server https://relay.example --json
ssmcp -i --server https://relay.example --client SESSION_ID
~~~

ChatGPT 等 MCP 应用使用该服务器的 `/mcp` OAuth 入口。CLI 登录和 MCP 应用授权分别管理，二者都需要真实账号身份。`list` 只返回当前账号有权控制的在线设备；知道设备 ID、伪造显示名称或填写 `--allow` 字符串都不能替代控制端认证。[HTTP 控制接口](../src/device.rs) · [MCP 授权](../src/http.rs)

`ssmcp sessions list` 查看本机保存的后台会话，`ssmcp attach NAME` 连接本机后台会话；它们与账号的远程设备列表用途不同。远程交互中 Ctrl-] 离开当前控制界面，设备进程继续运行。

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
