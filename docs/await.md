# 等待任务、协作对象与用户

`await` 把等待作为一个独立的服务端操作：指定要观察的对象，服务端订阅已有通知，有结果、超时或取消时返回。调用者无需频繁查询状态。公共 `await` 直接运行在 Rust 中；`code_mode` 内的 `waitFor` 使用相同协调器，但仍占用它所在的 JavaScript 执行名额。[协调器](../src/http/await_tool.rs) · [JavaScript helper](../src/code_mode.js)

## 先绑定真实参与者

每次等待都绑定认证账户、选定客户端的当前连接，以及当前参与者的 `collab` ID 和私有 token。已经绑定的 `code_mode` 调用可沿用该绑定；公共工具使用 `agent: {id, token}`。没有成员时，`collab.join` 仅登记正在调用的参与者，不启动模型。用户消息的 `target: "next"`、显示名称和手写线程字段都不能代替这个身份。[绑定实现](../src/collab.rs) · [等待范围](../src/http/await_tool.rs)

公共工具示例：

```json
{
  "client_id": "CLIENT_ID_FROM_LIST",
  "agent": {"id": "REGISTERED_AGENT_ID", "token": "PRIVATE_MEMBER_TOKEN"},
  "targets": [
    {"type": "exec", "job_id": "EXEC_JOB_ID"},
    {"type": "agent_status", "agent_id": "REGISTERED_PEER_ID", "statuses": ["done", "failed"]}
  ],
  "mode": "any",
  "timeout_ms": 20000
}
```

`waitFor(clientId, options)` 使用相同的 `targets`、`mode`、`timeout_ms` 和 `wait_id`。外层 `code_mode` 超时应留出返回余量；不要将 helper 的存在解释为旧 relay 已获得新接口。公共工具和 helper 都来自已安装的 relay；`exec` 目标还依赖客户端对应的原生工具，缺失时明确返回 `unavailable`。[公共目录](../src/http.rs) · [能力说明](../src/runtime/capabilities.rs)

## 可等待的对象

| `type` | 目标字段 | `ready` 的含义 |
|---|---|---|
| `exec` | `job_id` | 原生执行已结束；结果保留真实 `exit_code`、`exit_signal`、取消标记和输出游标，非零退出码不会被改成成功。 |
| `job` | `id` | 现有 relay 同步任务已离开启动、运行或收尾状态；原始 `state`、错误和清理信息仍返回。 |
| `agent_status` | `agent_id`、`statuses` | 该已登记成员当前声明的状态匹配指定字符串；状态由该 agent 自己报告，不证明用户验收。 |
| `agent_messages` | 可选 `after`、`limit` | 当前绑定成员的收件箱已有消息；返回原始来源与游标，不自动确认消息。 |
| `user_input` | 可选 `reason`、`notify` | 已收到可供最终响应交付的用户输入；真正交付以外层 `user_wait.status: "received"` 和匹配的 `user_messages` 为准。 |

执行任务复用原生通知，relay 任务复用任务通知，协作状态与消息复用成员 watch。等待期间每秒仅复核没有对应通知的 OAuth 与成员权限；对象状态不靠循环短查询推进。[执行等待](../src/backend/shell.rs) · [relay 任务](../src/bridge.rs) · [协作通知](../src/collab.rs)

最多同时选择 8 个不同目标。`any` 在一个目标就绪后返回，尚未完成观察的条目标为 `pending`；`all` 要求全部目标就绪。已观察到的权限、未知目标或其他类型错误会立即结束本次等待，并保留每个目标的位置与错误。`user_input` 必须单独等待，避免把用户交付混进多个对象的竞争中。[参数与聚合规则](../src/http/await_tool.rs)

## 超时、恢复与取消

`timeout_ms` 为 1000–25000，默认 20000。首次等待创建随机的服务端 `wait_id`；它标识观察记录，不是执行任务 ID。超时或资源忙时，带返回的 `wait_id` 恢复，不再传 `targets` 或 `mode`：

```javascript
return await waitFor(clientId, {wait_id: previous.wait_id, timeout_ms: 20000});
```

首次传入 `window_ms: 20000` 可启用短窗口协议。窗口到期返回 `pending`，同一个 `wait_id` 可继续观察原目标。作业、任务分派和待交付输入继续由各自的有界队列持有，窗口结束不取消它们。普通结果就绪时返回 `cursor: 1`。处理前用 `after: 0` 重读同一结果，应用成功后用 `after: 1` 确认。确认可重复，逾期游标与超出 128 KiB 的结果会显式报错。协作收件箱仍应单独执行 `collab.ack`。用户输入仍由外层交付事务提交。短窗口的外层回执返回 `await_delivery_cursor`，公共 `await` 的响应也返回 `cursor: 1`。响应丢失时用 `after: 0` 重读已交付的同一消息及版本，按 `message_id` 和 `revision` 去重，处理后用 `after: 1` 确认。重读不会领取下一条输入。Code Mode 的 `waitFor` 重试将已交付消息放在目标数据的 `replayed_user_messages`；首次交付仍以外层 `user_messages` 为准。

`worker_assignment` 接受该 worker 的 `request_id`，同一个观察持续使用该 ID。 `worker_result` 接受原任务的 `task_id`，并保留原生结果的终态。两个目标都要求独立等待，并继续由原生 worker 权限检查约束。

只有原账户、原客户端连接和原成员能恢复或取消该记录。调用者不能指定一个新 `wait_id` 来创建记录，也不能取消同账户下其他成员的等待。每条记录最多同时有一个活动等待；普通观察每成员最多四个、每连接最多三十二个，不占用户输入的等待名额。只有 `user_input` 沿用客户端已有的四个用户等待名额，记录数量与保留时间也有明确上限。空闲记录最多保留五分钟，已结束记录在容量压力下可提前回收。已预留用户输入但尚待最终交付的记录会受保护，直到真实交付结算或外层调用取消；最终结果仍是超时或资源忙时，原 ID 继续可恢复。`ssmcp://limits` 返回当前数值。[记录和容量](../src/http/await_tool.rs)

公共取消使用 `{"client_id":"…","agent":{"id":"…","token":"…"},"op":"cancel","wait_id":"…"}`；helper 使用 `waitFor.cancel(clientId, waitId)`。取消只停止观察，释放等待者；不会调用 `exec.cancel`、结束被观察的 agent、确认收件。首次请求尚未返回 ID 时，宿主取消该工具请求也会释放当前观察。完成后的观察不能重新用于消费下一份用户输入；返回 `await_already_resolved` 时应为新事件创建新观察。[取消与响应](../src/http/await_tool.rs)

`timed_out`、`cancelled`、`unauthorized`、`unknown_target`、`unavailable` 和 `busy` 是不同结果。权限在返回前重新检查；权限被撤销时，不返回此前取得的数据。协作消息依然遵循先接收、处理后显式 `collab.ack` 的协议；重试可返回同一未确认消息，应按消息 ID 或游标去重。`waitFor` 所在的普通 `code_mode` 响应仍遵循既有用户输入交付协议，helper 中的 `ready` 不能替代外层交付回执。[协作接收](../src/collab.rs) · [用户交付](../src/http/user_wait.rs)

## 已退役的结束目标

公共 `await` 的 `finish` 目标已退役，旧参数返回 `tool_retired`。结束前的真实用户决定使用另一个接口：先绑定已验证的提问线程，再调用 `ui.wait(clientId, {intent: "finish", ...})`；只有最终外层匹配的已消费 Human Yes 和 `may_finish: true` 才允许本轮结束。普通任务和用户输入的等待行为保留。[参数校验](../src/http/await_tool.rs) · [结束决定](finish-and-questions.md#申请结束)

公共 `await` 只报告被观察对象的状态。它的兼容字段 `completion_permission` 保持为 `null`，`continue_wait` 只标记本次是否超时，不能将这些字段用作结束许可。`ui.wait(intent: "finish")` 的独立外层回执和重试规则见[等待用户输入](user-wait.md#结束前取得真实用户决定)。[返回结构](../src/http/await_tool.rs)

## 历史验证范围

2026-11-20，源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92` 的 26 项等待回归及 80 项关联回归全部通过。等待测试包括真实内存身份的范围隔离、两个成员并发、租约到期、取消与最终交付竞争、容量和保留期限保护、同一 ID 超时恢复；它们没有启动外部模型或切换生产进程。[完整验证记录](validation/await-finish-20261120.md)
