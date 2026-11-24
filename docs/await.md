# 等待任务、协作对象与用户

`await` 把等待作为一个独立的服务端操作：指定要观察的对象，服务端订阅已有通知。单次调用可以短暂观察；显式设置 `lifetime_ms` 后，后台观察会独立于这次 HTTP 调用继续运行，并保留终态结果。调用者无需频繁查询状态。公共 `await` 直接运行在 Rust 中；`code_mode` 内的 `waitFor` 使用相同协调器，但仍占用它所在的 JavaScript 执行名额。[协调器](../src/http/await_tool.rs) · [JavaScript helper](../src/code_mode.js)

## 先绑定真实参与者

每次等待都绑定认证账户、选定客户端的当前连接，以及当前参与者的 `collab` ID 和私有 token。已经绑定的 `code_mode` 调用可沿用该绑定；公共工具使用 `agent: {id, token}`。没有成员时，`collab.join` 仅登记正在调用的参与者，不启动模型。用户消息的 `target: "next"`、显示名称和手写线程字段都不能代替这个身份。[绑定实现](../src/collab.rs) · [等待范围](../src/http/await_tool.rs)

公共工具示例：

```json
{
  "client_id": "CLIENT_ID_FROM_LIST",
  "agent": {"id": "REGISTERED_AGENT_ID", "token": "PRIVATE_MEMBER_TOKEN"},
  "targets": [
    {"type": "exec", "job_id": "EXEC_JOB_ID"},
    {"type": "agent_status", "agent_id": "REGISTERED_PEER_ID", "statuses": ["idle", "offline"]}
  ],
  "mode": "any",
  "timeout_ms": 20000
}
```

`waitFor(clientId, options)` 使用相同的目标和身份字段，但只做一次 1000 ms 的短观察；默认 `timeout_ms:1000`，显式 `window_ms` 也只能是 1000。长等待使用公共 `await`，避免占用 JavaScript 名额。外层 `code_mode` 超时仍须留出返回余量；不要将 helper 的存在解释为旧 relay 已获得新接口。公共工具和 helper 都来自已安装的 relay；`exec` 目标还依赖客户端对应的原生工具，缺失时明确返回 `unavailable`。[公共目录](../src/http.rs) · [能力说明](../src/runtime/capabilities.rs)

## 可等待的对象

| `type` | 目标字段 | `ready` 的含义 |
|---|---|---|
| `exec` | `job_id` | 原生执行已结束；结果保留真实 `exit_code`、`exit_signal`、取消标记和输出游标，非零退出码不会被改成成功。 |
| `job` | `id` | 现有 relay 同步任务已离开启动、运行或收尾状态；原始 `state`、错误和清理信息仍返回。 |
| `worker_result` | `task_id` | 原任务处于 `completed`、`failed` 或 `cancelled`，保留真实输出与错误；`unknown` 返回 `unknown_target`，应先核对原任务效果。 |
| `worker_assignment` | `request_id` | 原生等待返回 `assigned`、`already_completed`、`retiring`、`released` 或 `not_accepting`；按实际状态处理，原样保留分派的完整 `claim`。 |
| `agent_status` | `agent_id`、`statuses` | 已登记成员的生命周期状态匹配指定集合。可用值为 `working`、`idle`、`offline`、`dreaming`、`retiring`。状态匹配不证明任务成功或用户验收；任务结果仍读取原 `exec` 或 `worker_result` 回执。 |
| `agent_messages` | 可选 `after`、`limit` | 当前绑定成员的收件箱已有消息；返回原始来源与游标，不自动确认消息。 |
| `event` | `subscription_id`，可选 `limit` | 订阅已有匹配事件，返回原始来源与游标；处理后显式调用 `events.ack`。保留缺口和权限错误按原状态处理。 |
| `user_input` | 可选 `reason`、`notify` | 已收到可供最终响应交付的用户输入；真正交付以外层 `user_wait.status: "received"` 和匹配的 `user_messages` 为准。 |

执行任务复用原生通知，relay 任务复用任务通知，协作状态与消息复用成员 watch。等待期间每秒仅复核没有对应通知的 OAuth 与成员权限；对象状态不靠循环短查询推进。[执行等待](../src/backend/shell.rs) · [relay 任务](../src/bridge.rs) · [协作通知](../src/collab.rs)

最多同时选择 8 个不同目标。`any` 在一个目标就绪后返回，尚未完成观察的条目标为 `pending`；`all` 要求全部目标就绪。已观察到的权限、未知目标或其他类型错误会立即结束本次等待，并保留每个目标的位置与错误。`user_input` 和 `worker_assignment` 必须各自单独等待；`worker_result` 和 `event` 可以参与多目标观察。[参数与聚合规则](../src/http/await_tool.rs)

## 调用中断后继续等待

长任务需要分别设置三个时间：

| 时间 | 字段或规则 | 控制的对象 |
|---|---|---|
| 本次读窗口 | `window_ms`，1000–55000 毫秒 | 这次 HTTP 请求最多等待多久。窗口到期可先返回 `pending`。 |
| 后台观察期限 | `lifetime_ms`，1000–86400000 毫秒 | 服务端观察原目标的最长时间，调用者断开不会重新计时。 |
| 终态保留时间 | 终态记录至少保留 1800000 毫秒 | 结果产生后可供原参与者恢复、重读和确认的时间；确认后释放正文。 |

例如，一个命令需要十分钟，而调用方每分钟都会中断工具请求，可以只等待一秒取得 `wait_id`，让服务端继续观察，再于之后恢复读取。同一次观察不需要保持同一条 HTTP 连接。[后台观察与缓存](../src/http/await_tool.rs)

首次创建必须给出调用者生成的 `request_id`。同一身份使用相同 ID、目标、聚合模式与后台期限重试，会找回原观察；读窗口可以改变。若原请求的回执丢失，仍使用原 `request_id`。改变目标、模式或后台期限会被拒绝。

```json
{
  "client_id": "CLIENT_ID_FROM_LIST",
  "agent": {
    "id": "REGISTERED_AGENT_ID",
    "token": "PRIVATE_MEMBER_TOKEN"
  },
  "request_id": "my-build-observation-001",
  "lifetime_ms": 900000,
  "window_ms": 1000,
  "targets": [
    {
      "type": "exec",
      "job_id": "ORIGINAL_EXEC_JOB_ID"
    }
  ]
}
```

收到 ID 后，恢复读取只需带原身份、`wait_id` 和读窗口：

```json
{
  "client_id": "CLIENT_ID_FROM_LIST",
  "agent": {
    "id": "REGISTERED_AGENT_ID",
    "token": "PRIVATE_MEMBER_TOKEN"
  },
  "wait_id": "ORIGINAL_WAIT_ID",
  "window_ms": 1000,
  "after": 0
}
```

`cursor: 0` 表示还没有终态结果；`cursor: 1` 表示终态已固定。`ready`、`timed_out`、`cancelled`、权限错误和其他终态都进入同一缓存协议。处理成功前用 `after: 0` 重读；处理完成后用 `after: 1` 确认。确认可以重复，已确认的结果正文会被释放，记录仍保留原终态和时间信息。`op: "list"` 与 `op: "status"` 提供原参与者的元数据，不返回结果正文。协作收件箱或事件订阅的业务确认仍使用各自的 `ack`。[缓存状态机](../src/http/await_tool.rs)

后台观察支持 `exec`、`job`、`worker_result`、`agent_status`、`agent_messages` 和 `event`。涉及领取任务的 `worker_assignment` 及用户输入 `user_input` 继续使用各自的交付协议，不能作为后台观察目标。观察任务不会调用执行、提交任务或自动重放操作的接口。

后台记录绑定原账户、原成员和原客户端连接，每次读结果都会重新鉴权。ChatGPT 的 HTTP 调用重连可以沿用这些身份；换一个原生客户端连接不能接管旧记录。记录保存在当前 relay 进程内。relay 重启或原连接消失后，应核对原任务的实际状态；`unknown` 不构成重新执行的依据。

终端 `/wait`、Web 与桌面的等待页面展示后台期限、读窗口、总耗时、结果状态、首次读取延迟和确认状态。耗时统计基于页面当前保留的有界样本；取消、未知状态和仍在等待的记录单独列示。首次读取时间表示服务端尝试向已授权调用返回结果的时间，不能证明模型已经处理结果或用户已经确认。新版面板通过 `wait_display_v4` 协商；旧 `v3` 连接继续接收原有的短窗口显示格式。[显示协议](../src/wire/wait_display.rs) · [运行时投影](../src/runtime/time/relay.rs)

时间字段在对应事件尚未发生时可以省略。例如，未尝试返回终态正文时没有 `first_read_at_ms`，尚未确认时没有 `acknowledged_at_ms`。读取元数据不能产生这两个事件；处理方应按字段是否存在解释状态。[元数据格式](../src/wire/wait_display.rs) · [读出与确认](../src/http/await_tool/durable.rs)

## 单次观察的超时、恢复与取消

以下规则适用于没有设置 `lifetime_ms` 的单次观察和兼容窗口协议。
公共 `await` 的 `timeout_ms` 为 1000–55000 毫秒，默认 20000；显式 `window_ms` 也接受 1000–55000 毫秒。JavaScript `waitFor` 的单次观察保持 1000 毫秒，长等待使用公共入口。[实际参数范围](../src/http/await_tool.rs) · [共享观察上限](../src/wire.rs)

首次等待创建服务端 `wait_id`，用于恢复同一次观察。超时或资源忙时沿用该 ID，不再传 `targets` 或 `mode`。以下示例恢复一次 JavaScript 短观察：

```javascript
return await waitFor(clientId, {wait_id: previous.wait_id, timeout_ms: 1000});
```

公共 `await` 首次传入 `window_ms: 20000` 可启用窗口协议；在 JS 的 `waitFor` 中使用 `window_ms: 1000`。窗口到期返回 `pending`，同一个 `wait_id` 可继续观察原目标。作业、任务分派和待交付输入继续由各自的有界队列持有，窗口结束不取消它们。普通结果就绪时返回 `cursor: 1`。处理前用 `after: 0` 重读同一结果，应用成功后用 `after: 1` 确认。确认可重复，逾期游标与超出 128 KiB 的结果会显式报错。协作收件箱仍应单独执行 `collab.ack`。用户输入仍由外层交付事务提交。短窗口的外层回执返回 `await_delivery_cursor`，公共 `await` 的响应也返回 `cursor: 1`。响应丢失时用 `after: 0` 重读已交付的同一消息及版本，按 `message_id` 和 `revision` 去重，处理后用 `after: 1` 确认。重读不会领取下一条输入。Code Mode 的 `waitFor` 重试将已交付消息放在目标数据的 `replayed_user_messages`；首次交付仍以外层 `user_messages` 为准。

两个 worker 目标首次等待时都必须提供 `window_ms`，并沿用原生 worker 的权限检查。`worker_assignment` 接受该 worker 的 `request_id`，必须单独等待；等待与重试保留同一 ID，完成原 `claim` 后再用新 ID 领取下一项任务。`worker_result` 接受提交者拥有的原任务 `task_id`，可以与其他观察目标组合。结果为 `unknown` 时先核对已发生的效果，禁止自动重新执行。[worker 生命周期](workers.md#remote-claim-and-completion)

只有原账户、原客户端连接和原成员能恢复或取消该记录。调用者不能指定一个新 `wait_id` 来创建记录，也不能取消同账户下其他成员的等待。每条记录最多同时有一个活动等待；普通观察每成员最多四个、每连接最多三十二个，不占用户输入的等待名额。只有 `user_input` 沿用客户端已有的四个用户等待名额，记录数量与保留时间也有明确上限。空闲记录最多保留五分钟，已结束记录在容量压力下可提前回收。已预留用户输入但尚待最终交付的记录会受保护，直到真实交付结算或外层调用取消；最终结果仍是超时或资源忙时，原 ID 继续可恢复。`ssmcp://limits` 返回当前数值。[记录和容量](../src/http/await_tool.rs)

公共取消使用 `{"client_id":"…","agent":{"id":"…","token":"…"},"op":"cancel","wait_id":"…"}`；helper 使用 `waitFor.cancel(clientId, waitId)`。取消只停止观察，释放等待者；不会调用 `exec.cancel`、结束被观察的 agent、确认收件。首次请求尚未返回 ID 时，宿主取消该工具请求也会释放当前观察。完成后的观察不能重新用于消费下一份用户输入；返回 `await_already_resolved` 时应为新事件创建新观察。[取消与响应](../src/http/await_tool.rs)

`timed_out`、`cancelled`、`unauthorized`、`unknown_target`、`unavailable` 和 `busy` 是不同结果。权限在返回前重新检查；权限被撤销时，不返回此前取得的数据。协作消息依然遵循先接收、处理后显式 `collab.ack` 的协议；重试可返回同一未确认消息，应按消息 ID 或游标去重。`waitFor` 所在的普通 `code_mode` 响应仍遵循既有用户输入交付协议，helper 中的 `ready` 不能替代外层交付回执。[协作接收](../src/collab.rs) · [用户交付](../src/http/user_wait.rs)

## 已退役的结束目标

公共 `await` 的 `finish` 目标已退役，旧参数返回 `tool_retired`。结束前的真实用户决定使用另一个接口：先绑定已验证的提问线程，再调用 `ui.wait(clientId, {intent: "finish", ...})`；只有最终外层匹配的已消费 Human Yes 和 `may_finish: true` 才允许本轮结束。普通任务和用户输入的等待行为保留。[参数校验](../src/http/await_tool.rs) · [结束决定](finish-and-questions.md#申请结束)

公共 `await` 只报告被观察对象的状态。它的兼容字段 `completion_permission` 保持为 `null`，`continue_wait` 表示原等待仍待继续或本次读取窗口超时。订阅事件可以提前唤醒一次读取；若原目标尚未完成，响应仍为 `pending` 且 `continue_wait: true`，应沿用原 `wait_id`。这些字段不能用作结束许可。`ui.wait(intent: "finish")` 的独立外层回执和重试规则见[等待用户输入](user-wait.md#结束前取得真实用户决定)。[返回结构](../src/http/await_tool.rs)

## 自动回归与十分钟验收

Linux 的 `make test-client-ui` 使用同次构建的原生客户端和账号路由 fixture，运行真实终端引用回复及 15 秒后台等待回归。短回归检查断开观察后的原任务完成、结果重读、精确 ACK 与定时提示，报告明确标记为 `short_diagnostic`。独立十分钟验收使用同一脚本的 `--duration-seconds 600`，报告标记为 `600_second`；短回归不能替代这份证据。每次运行使用新报告路径，并核对实际二进制 SHA256 和构建提交。[回归入口](../Makefile) · [后台等待验收](../scripts/e2e_durable_wait.py)

## 历史验证范围

2026-11-20，源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92` 的 26 项等待回归及 80 项关联回归全部通过。等待测试包括真实内存身份的范围隔离、两个成员并发、租约到期、取消与最终交付竞争、容量和保留期限保护、同一 ID 超时恢复；它们没有启动外部模型或切换生产进程。[完整验证记录](validation/await-finish-20261120.md)
