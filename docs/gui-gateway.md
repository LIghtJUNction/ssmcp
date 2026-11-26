# CLI 图形客户端网关

`ssmcp gui serve` 把已有 CLI 会话连接到 Web、桌面或移动界面。它提供真实会话列表、zbridge 请求转发和有界事件观察；会话身份、终端操作、worker 任务和权限仍由 CLI 核心处理。网关不会启动模型、创建会话或伪造活动数据。[CLI 入口与原生连接](../src/session/gui.rs) · [HTTP 实现](../src/session/gui/gateway.rs)

## 启动与配对

```sh
ssmcp gui open
```

在安装了桌面菜单项的 Linux 系统上，点击 **SSMCP Web** 即可用默认浏览器打开工作台。再次打开时会复用使用默认配对文件的网关。命令行入口是 `ssmcp gui open`。在网页上选择配对文件、选择已有会话，再按需启用“控制会话”发送消息。

`ssmcp gui serve` 在自动分配的回环端口提供同一工作台和 API。`--listen 127.0.0.1:8787` 可指定固定端口，`--assets DIRECTORY` 可替换内置静态页面。

启动输出包含服务地址、实际监听地址和配对文件路径，不包含令牌。默认配对文件位于当前会话状态目录的 `gui/pairing.json`；`--pairing-file /absolute/private/gui-pairing.json` 可以指定已有私有目录中的另一位置。网关复用 CLI 的私有目录和文件检查，Unix 目录要求当前用户所有且权限为 0700，凭证文件以 0600 创建；Windows 使用现有当前用户 DACL 检查。

配对文件的格式是 `ssmcp-gui-pairing-v1`，包含 `url`、`token`、`pid` 和进程生命周期说明。前端经用户配对取得令牌后，在每个 API 请求中发送 `Authorization: Bearer <token>`。令牌仅对应本次网关进程；重启生成新值，正常退出移除仍属于本进程的配对文件。不要把它放在 URL、前端静态文件或原生日志中。

这是独立的配对凭证。原生 attach descriptor、descriptor token 和 lease 始终留在 CLI 进程内，不会传给浏览器或复制到手机。每个会话状态目录只允许一个网关实例；该约束使用已有的本地锁实现。

## 手机与明确的 HTTPS 代理

网关只允许监听 loopback。需要从另一台设备访问时，由部署者明确配置 HTTPS 反向代理，并启动：

```sh
ssmcp gui serve --listen 127.0.0.1:8787 \
  --public-origin https://gui.example.com \
  --assets /absolute/apps/ssmcp-web
```

代理应保留该公开地址的 `Host` 和浏览器 `Origin`，并允许至少覆盖 55 秒事件窗口的响应时间。服务检查这两个值是否与配置地址一致。原生手机客户端可以省略 `Origin`，但仍必须提供配对 bearer。浏览器与静态资源使用同一个公开地址；没有通配 CORS，也不会根据请求参数切换任意后端 URL。此命令本身不申请证书或部署代理。

## HTTP 契约

所有 API 使用 `/api/gui/v1` 前缀。POST 请求采用 `application/json`，未知字段会被拒绝。网关错误返回 `{"error":{"code":...,"message":...,"effect":...}}`；`/bridge` 已取得的业务错误保留 zbridge 原生响应信封。

| 方法与路径 | 输入 | 返回 |
| --- | --- | --- |
| `GET /sessions` | 可选 `offset`，默认 0；`limit` 默认 16，范围 1–32 | `sessions`、`next_offset`、`has_more` |
| `GET /connections` | 无 | 当前网关连接的 `connection_id`、`request_id`、`session`、`read_only` 和 `status` |
| `POST /connections` | `request_id`、`session`，可选 `read_only`，默认 true | `status`、`connection_id`、`session`、`read_only`、`duplicate`；连接成功时附实际 `capabilities` 响应 |
| `POST /connections/{id}/bridge` | 完整 zbridge `Request` | 完整且已校验关联的 zbridge `Response` |
| `POST /connections/{id}/events` | `request_id`、`generation`，可选十进制字符串 `cursor`、`limit`、`wait_ms` | `ssmcp-gui-events-v1` 信封及原生 `EventPage` |
| `DELETE /connections/{id}` | 无 | 真实释放确认后返回 `closed:true`、`session_stopped:false` 和 `duplicate` |

会话列表直接来自 `session::list()`，仅投影 `name`、`cwd`、`manager_state`、`manager_active`、`connection_state`、`phase`、`generation` 和 `stop_requested`。响应不会包含状态目录、启动参数或认证文件。管理器在运行与原生后端在线是不同状态，界面应分别使用实际字段。[会话状态源](../src/session.rs)

连接创建请求示例：

```json
{"request_id":"view-open-1","session":"work","read_only":true}
```

同一个 `request_id` 和相同字段重试会复用原连接；改变字段返回冲突。正在创建时，重复请求返回 HTTP 202 与 `status:"opening"`。关闭或正在关闭的旧回执分别返回 `closed` 或 `closing`，不会重新申请写入租约。回执有界保留 15 分钟，活跃连接保留其创建回执；重复请求与确认关闭会刷新保留期限。

`read_only:false` 是明确的可写连接请求。是否取得租约、能执行哪些命令，都由实际 `RemoteFrontend` 和核心能力响应决定。`connection_id` 只是关联标识，不提供权限。`status:"connected"` 表示网关已取得桥接对象；原生 Runtime 是否活跃要读取能力与 `runtime` 状态，不能据此把离线后端画成正在工作。

成功创建响应中，嵌套 `capabilities.id` 与外层创建请求的 `request_id` 一致，网关返回前会校验关联。前端也可以使用自己的新 ID，通过 `/bridge` 发起独立能力发现。

## 能力、状态与事件

连接成功时的 `capabilities` 是原生 zbridge `Response`。保留其中完整、不透明的 `generation`，并只调用已公布的资源和命令。网关不维护另一份硬编码能力目录；新增的核心对话、线程或输入投影可以通过同一个 `/bridge` 路由传递。

读取状态时发送原生信封，例如：

```json
{
  "version":1,
  "id":"runtime-1",
  "operation":{
    "op":"state",
    "generation":"generation-from-capabilities",
    "resource":"runtime",
    "query":{}
  }
}
```

先读取 `runtime.live_cursor`，再读取其他当前状态，可以保留读取状态期间到达的事件。随后把这个游标传给 HTTP 观察端点：

```json
{
  "request_id":"observe-1",
  "generation":"generation-from-capabilities",
  "cursor":"9007199254740993",
  "limit":16,
  "wait_ms":55000
}
```

`wait_ms` 默认 55,000，范围 0–55,000；`limit` 默认 16，范围 1–128。HTTP 层复用 `EventObserver`，将现有每次最多一秒的 zbridge v1 读取组合成一次有界观察。收到事件、保留缺口或还有更多数据时立即返回。每个实际子请求都有自己的关联 ID，保持同一后端代次；空回复会按等待窗口节奏处理。[共享观察器](../src/session/attach/zbridge_adapter/observation.rs)

观察、状态读取和命令共用原生租约，但不会把整段观察窗口放进互斥临界区。关闭操作会唤醒正在等待的原生请求，并让观察返回关闭错误；已发送命令仍按不确定结果处理。真实 detach 回执另外确认，界面不需要等满 55 秒才能结束观察。

响应的 `schema` 为 `ssmcp-gui-events-v1`，其余字段是 `request_id`、`generation` 和 `page`。界面处理完 `page` 后保存 `page.next_cursor`，下一次观察使用新 `request_id` 和这个游标。`retention_gap:true` 要求重新读取当前状态并处理历史缺口。所有计数器保持十进制字符串，包括超过 2^53 的值；不要先转成 JavaScript `Number` 再保存。

断线或界面进入后台时，保留最后已处理的游标。重新连接后先检查能力代次；只有完整代次相同才继续使用原游标。新代次需要重新取得状态和事件边界。没有显式游标的请求从当次原生读取的实时边界开始，不能用连续的 `cursor:null` 请求代替断线恢复。

## 命令和关闭的真实回执

`/bridge` 每次只转发一次请求。原生成功回执保持原样，异步 worker 任务是否完成仍由任务状态决定。命令发出后遇到传输错误、失配响应或截止时间，错误效果会保留为 `may_have_occurred`。HTTP 客户端在未取得响应时也必须保留这个不确定状态；相同请求 ID 不保证命令幂等，不能自动重发。

关闭连接会先拒绝该连接上的新工作，再等待真实 detach 回执。若确认失败，HTTP 返回不确定错误，连接保持 `closing`。后续显式 `DELETE` 可以重试原生幂等释放；确认前不会把未知结果写成已释放。重复关闭已确认的连接会返回保留的成功状态；不存在保留回执的 ID 返回 `Closed` 错误。关闭网关连接不会停止后台会话或完成 worker claim。

## 边界与验证

服务最多接受 32 个并发请求、16 个活动或正在创建的连接，以及 128 个近期创建回执。闲置连接在后续访问时按 15 分钟期限进入关闭流程；未确认释放的连接保留为 `closing`，等待显式重试。API 输入最多 256 KiB，事件页遵守 zbridge 的 128 条和 128 KiB 上限。静态文件每个最多 2 MiB；路径需位于配置目录内，不能通过 symlink 越界，也不会提供 JSON 凭证文件。

默认内容安全策略使用同源外部 JS、CSS 和字体。界面资源应把脚本和样式放在独立文件中；代理与前端无需复制后端实现。平台包和窗口的验收仍由对应应用执行。

[HTTP 共享测试数据](../src/session/gui/gateway/contract-v1.json)包含大数游标、观察信封、创建请求和未知命令结果。测试通过真实 loopback HTTP 验证认证、只读能力、创建去重、关闭确认、事件代次、帧大小和静态资源边界；另外的隔离测试把网关连接到实际私有 attach IPC，检查真实租约、状态和释放回执。[HTTP 测试](../src/session/gui/gateway/tests.rs) · [私有 IPC 测试](../src/session/attach/zbridge_adapter/gui_gateway_tests.rs)

```sh
flock /var/tmp/project-z-cargo-target/.project-z-cargo-owner.lock \
  cargo test --locked --offline --lib gui
```

运行结果应与具体源码提交一同记录。协议、HTTP 和私有 IPC 测试不替代 Windows 窗口、Android 生命周期、iOS 签名或真实设备验收。
