# 等待用户输入

`ui.wait` 在当前已认证会话中有两种用途：`need_user` 等待真实用户输入；`finish` 在准备结束对话时请求本机用户明确选择 No 或 Yes。每次调用都有截止时间，终端显示真实剩余时间，用户可取消活动等待。超时、普通消息和唤醒画面都不表示同意结束。该能力不调用模型，也不注册 `/send` 的监听程序。

## 发起一次等待

在 `code_mode` 中选择已授权的客户端，然后返回等待结果。一次调用等待 1–25 秒，默认 20 秒；外层 `code_mode` 的超时需要覆盖这段等待及交付开销。

```javascript
const client = clients.find(c => c.id === selectedClientId);
if (!client) throw new Error('Selected client is no longer available');
return await ui.wait(client.id, {
  timeout_ms: 20000,
  intent: 'need_user',
  reason: '请确认下一步要处理哪个文件。',
  notify: true,
});
```

`intent: 'need_user'` 表示工作需要用户输入。准备结束时使用 `intent: 'finish'`，并明确询问用户是否希望结束；这个参数本身不会赋予结束许可。`reason` 可省略；提供时应包含 1–512 个 UTF-8 字节，不能包含控制字符。`notify` 默认为 `true`。

同一轮 `code_mode` 对同一客户端只能调用一次 `ui.wait`；重复调用会返回运行错误，不会返回结构化的 `busy`。下一轮等待应放在新的外层调用中。

程序应在每次外层响应后检查真实等待结果。`timed_out` 携带 `continue_wait: true`，只表示这次有界等待到期，没有批准结束。需要输入时可以在新的外层调用中继续等待；结束确认的重试应使用返回的精确 `finish_request_id`。[回执实现](../src/code_mode/user_wait.rs)

## 结束前取得真实用户决定

准备结束对话前，先使用已存在参与者的真实 ID 和私有 token 绑定提问线程，再调用 `ui.wait`。下面的 `participant` 必须来自实际协作登记；绑定不会启动模型，显示名称或手写身份字段不能代替它。

```javascript
await collab.bind(client.id, {
  agent_id: participant.agent_id,
  token: participant.token,
});
return await ui.wait(client.id, {
  intent: 'finish',
  reason: '本轮工作已完成，请决定现在结束还是继续。',
  timeout_ms: 20000,
  notify: false,
});
```

没有已验证线程绑定时，结束等待返回错误。如果已有可交付的普通用户输入，服务先按原来的预约、权限复核和最终 `received` 流程交付这些输入，不创建新的结束申请，也不把文本中的“yes”视为批准。没有已排队输入时，同一次原生等待会同步创建或续接真实的 No/Yes 问题，并登记取消清理；不会先创建一份脱离等待生命周期的申请。[服务端分流](../src/http/user_wait.rs) · [本机等待](../src/client/interface/user_wait.rs)

外层响应的 `structuredContent.user_wait` 包含 `finish_request_id`、`finish_decision` 和 `may_finish`。应找到本次客户端及 `wait_id` 对应的条目，检查以下完整条件：

| 字段 | 本轮允许结束时的值 |
| --- | --- |
| `intent` / `status` | `finish` / `decided` |
| `may_finish` | `true` |
| `finish_request_id` | 与 `finish_decision.request_id` 完全相同的实际申请 ID |
| `finish_decision.status` / `choice` | `completed` / `yes` |
| `finish_decision.actor` | `{"origin":"human"}` |
| `finish_decision.revision` | 大于零的真实决定修订号 |
| `finish_decision.consumed` / `invalidation_reason` | `true` / `null` |

脚本内部取得的对象可能只有 `status: "decision_ready"`、`may_finish: false`。本机已经记录 Yes 也不等于最终外层响应批准：服务器还要复核账户权限、原连接、原提问线程和相关输入 episode，再提交 `decided`。必须读取最终外层回执，不能只检查 JavaScript 返回值或问题列表中的 `approved`。[最终回执](../src/code_mode/user_wait.rs) · [Human 判定与精确消费](../src/client/interface/questions/requests.rs)

真实用户选择 No 时，决定保留 `status: "denied"`、`choice: "no"`、Human 来源和修订号，`consumed` 与 `may_finish` 都为 `false`。Yes 只由原提问线程按精确申请和修订号消费一次；以后读取或续等已完成的申请会得到 `consumed: false`，不能再次取得结束许可。其他 agent 不能代替用户批准 Yes。

成员须在同一 Dreaming 窗口内连续达到 1,800,000 毫秒才可申请主动离线。Human No 后转入普通输入或事件等待，自拒绝起再满 30 分钟才可新建申请。新需求或 claim 立即恢复 working，旧窗口和批准失效。[完整申请流程](workers.md#voluntary-offline-requests)

超时不是 No，也不是 Yes。保留响应中实际返回的 `finish_request_id`，在新的 `code_mode` 调用中重新绑定同一线程，并将该 ID 传给 `ui.wait(..., {intent: 'finish', finish_request_id, ...})` 续等；不要猜测申请 ID。此参数仅适用于 `finish`，必须是 1–128 字节的可见 ASCII 精确 ID。取消、新的相关用户输入、断线或重连、撤权、绑定失效都不能批准结束；应处理实际回执或新的用户要求，而不是重放旧许可。[续等与清理](../src/client/interface/questions/workflow.rs)

普通进度、`ui.message` / `ui.present` 的 `kind: "answer"`、`ui.reply` 以及代码执行完成通知仍可正常提交。这是结束前取得真实用户决定的协议；MCP 不能拦截任意外部模型自行停止或发送最终发言，也不会从模型参数推断用户同意。

## 判断消息是否已经交付

最终依据是外层响应的 `structuredContent.user_wait` 和对应的 `structuredContent.user_messages`。同一客户端、等待编号和精确消息记录相匹配，且等待状态为 `received`，才表示服务器已提交这次交付。消息被程序接收不表示它已经阅读、执行或完成用户的任务。

| 状态 | 含义 | 调用者应如何处理 |
| --- | --- | --- |
| `waiting` | 等待仍在进行。 | 不宣称收到消息。 |
| `ready` | 消息已预约，等待外层响应提交。 | 等待最终响应；不能仅凭这个状态处理消息。 |
| `received` | 服务器重新检查权限并提交了响应中对应的用户消息。 | 按精确消息记录处理新要求；不据此结束。 |
| `decision_ready` | 已取得本机决定回执，尚未通过最终外层复核。 | 不当作结束许可。 |
| `decided` | 外层已提交结束决定的回执，可能是 Yes、No 或不可用的旧决定。 | 仍须检查上表全部许可字段；只有 `may_finish: true` 的匹配 Human Yes 允许本轮结束。 |
| `timed_out` | 本轮等待结束，没有完成交付。 | 需要用户输入时再次等待。 |
| `cancelled` | 用户或调用生命周期取消了等待。 | 停止这一轮等待；不把取消当成会话结束许可。 |
| `disconnected` | 原连接已结束。 | 重新选择当前授权连接，不能把旧等待搬到新连接。 |
| `authorization_revoked` / `binding_invalid` | 权限或指定接收者的绑定失效。 | 停止使用失效的权限或绑定。 |
| `superseded` | 预约或完成条件已改变，原结果没有提交。 | 以最终消息队列状态为准，不处理未提交的正文。 |
| `busy` | 当前等待容量已满，或原生等待编号重复。 | 保留已有等待，稍后再尝试。 |
| `unavailable` | 该客户端不能提供需要的原生等待能力。 | 明确报告不可用，不伪造等待或成功结果。 |

`ui.wait` 在脚本内返回的对象可能仍为 `ready`；最终 `received` 是外层响应提交阶段生成的结果。原生 `ssmcp.ui.wait` 管理本机展示、取消及真实结束问题，不读取服务端用户队列正文，也不能替代最终外层交付或结束许可回执。原生 `op: "finish"` 只结束精确等待的生命周期；调用者不能用它提交 `received` 或 `decided`。

## 用户在终端中能做什么

真实人工等待仍在进行时，输入框下方显示红色提示，例如 `! 等待你的输入 · 剩余 00:13 · /wait`。多个独立 episode 显示 `! N 项等待你的输入 · 剩余 mm:ss · /wait`；倒计时取活动等待中最短的实际剩余时间，并向上取整到秒。它表示本轮有界调用还可等待多久，不是用户已经等待的累计时长。

同一 episode 的重入不会不断增加对话消息。普通后台任务和计时器不显示红色人工等待提醒；具体错误、补全和浏览提示仍按界面优先级显示。关闭终端颜色时保留文字提示，不依赖颜色表达状态。

- `/wait` 查看当前目标、原因、完整等待编号、本轮剩余时间和通知状态。
- `/wait cancel <完整编号>` 取消这一条活动等待。
- `/wait cancel <唯一前缀>` 只在前缀唯一时取消；前缀不明确会拒绝操作。

每个客户端最多同时展示四个独立等待，满额时返回 `busy`，不会替换先到的等待。 `code_mode` 提供四个共享执行槽，另有一个控制槽。主协调线程在外层 `code_mode.agent` 中传入已绑定聊天连接的凭据，就能在四个共享槽都被 worker 占用时继续操作。调用在可用的执行槽上按到达顺序排队。准备、排队和执行共用调用方设置的超时。排队超时会明确告知用户代码尚未启动。本地 `/wait` 取消不占用外层执行名额。取消会唤醒真实的等待调用；它不退出会话、不提交新消息，也不修改草稿、普通 Queue 或 `/send` 的投递。

多个等待即使属于同一个 episode，仍有独立的 `wait_id` 和取消入口。结束申请另外使用 `finish_request_id` 续接同一问题；它不是 `/wait cancel` 所需的等待编号。收到本机结束结果时，`/wait` 的近期记录显示中性的“本轮结束决策已返回”，不把 No 或未通过服务端复核的结果显示为批准。

## 系统通知的范围

本机只复用一个通知器。服务器提供稳定的 `episode_id`，同一轮需求因超时再次等待时保持该编号；通知器保存有界的最近编号记录，并有冷却时间，避免每次重试都弹出通知。等待结束或调用被取消时，正在进行的通知尝试也可能被取消；这不代表通知一定没有显示。

通知回执包含 `status`、固定原因 `reason`、`deduplicated` 和 `user_seen`。`sent` 仅表示本机通知提交成功；`user_seen` 始终为 `null`，不会宣称用户已经看到。桌面环境、系统助手缺失、超时或失败都以明确的通知状态返回，不能替代用户消息。设置 `notify: false` 可跳过系统通知。

## 生命周期与竞争结果

等待严格属于当前连接。连接结束会完成活动等待并清理本连接的完成记录；后台旧调用的清理操作通过注册代次匹配，不能取消新连接中同名的等待。

本地取消、截止时间与服务端完成共用同一条终态记录。先完成的结果保留，后到的 `finish` 会读到这个结果。终端最多保存最近 32 条本连接内的完成元数据，用于回传竞争结果和通知回执；这不是持久会话历史。通知正文和等待状态都不包含用户队列正文。

服务器还需要持有有效的消息预约，并在最终响应中重新检查认证、接收者绑定和消息版本。原生 `ready` 不能越过这些检查。HTTP 调用被取消时，服务器先释放预约，再尝试结束对应原生等待；原生调用自己的截止时间提供最后的清理上界。

## 原生接口与责任划分

| 原生操作 | 输入 | 返回 |
| --- | --- | --- |
| `wait` | `wait_id`、`episode_id`、`recipient`、`intent`、`reason`、`timeout_ms`、`notify`；结束续等可带 `finish_request_id`。 | 阻塞至完成；返回相同 `wait_id`、本机状态、可选通知与结束决定回执，不返回用户正文。 |
| `finish` | 精确 `wait_id`、`outcome`，以及已知时的 `finish_request_id`。 | `wait_id`、`finished`、已知最终 `status`、通知及结束决定回执。未知等待的 `status` 为 `null`；非超时关闭会取消所属未完成申请。 |
| `status` | 无额外字段。 | 当前活动等待和有界的最近完成记录；只观察。 |

`wait_id` 是最多 128 字节的可见 ASCII 编号。`episode_id` 是 64 位十六进制摘要，用于聚合与通知去重，不是访问令牌。普通输入等待的 `recipient` 可以是服务器已验证的精确 agent 编号或 `next`；结束等待必须属于已验证的提问线程，不能使用 `next`。本机结束申请按真实调用范围核对账户、连接和线程，不按显示名称授权。原生 `reason` 同时接受省略和 `null`。

旧 `request_finish`、`ssmcp.ui.finish`、`ssmcp.ui.answer` 及公共 `await` 的 `finish` 目标继续退役。当前结束确认只从已验证线程的 `ui.wait(intent: 'finish')` 发起，不恢复 agent 代用户作答的工具。[结束与问题协议](finish-and-questions.md)

## 源码与验收入口

认证、消息预约及最终回执位于 [服务端等待协调器](../src/code_mode/user_wait.rs)；本机截止时间、取消和完成缓存位于 [原生等待模块](../src/client/interface/user_wait.rs)；终端命令和提示位于 [等待交互模块](../src/client/terminal/conversation/user_wait.rs)；系统通知位于 [通知器](../src/client/notification.rs)。

定点测试覆盖四条等待的容量、精确取消与模糊前缀拒绝、取消与完成竞争、旧注册清理、真实 deadline、严格参数解析，以及同 episode 的混合意图展示。实际交付还应通过统一客户端与服务器的隔离 PTY 验收；源码测试与真实端到端验收分别记录，不相互替代。

## 等待时的休眠画面

已连接的终端在收到有效的用户等待状态后，会压暗背景并显示居中的 ASCII 休眠图案。真实活动等待的倒计时在最后一行以红色粗体保留，不随背景压暗；关闭颜色时仍保留粗体。背景内容仍可辨认；终端字符没有 alpha 通道，低亮度文字与背景色形成半透明的视觉效果。窄窗口会缩小图案和提示，原生图片会暂时隐藏，唤醒后恢复。

鼠标按下会解除遮罩；这一整次按下、拖动和抬起只用于唤醒，不会选中消息、滚动命令窗口或打开其他视图。滚轮和鼠标移动不会唤醒。键盘输入也会解除遮罩，并保留首次输入、Unicode 字节和粘贴内容。草稿、待发队列、浏览位置及 Copy 模式继续保留。

唤醒只改变画面，不会提交消息、取消等待或替用户同意结束。只要真实等待仍在进行，正常输入界面仍保留红色倒计时；遮罩消失不等于等待完成。真实发送的消息、现有取消操作、连接/授权变化和超时仍按原协议处理。客户端按连接和逻辑 episode 记录最近 64 个已唤醒 episode；同一 episode 在超时后换 wait_id 重试，不会再次压暗。新 episode 或新连接可再次进入休眠。

这里的休眠画面与有界等待循环分工明确：服务端已有 1–25 秒（默认 20 秒）的异步等待，在消息到达时唤醒，并检查授权心跳；超时返回 continue_wait 后，调用方可查看结果并继续下一轮等待。画面不新增轮询协议，也不改变预约、提交和最终回执。

本地回归位于 src/client/terminal/sleep.rs。设置 SSMCP_SLEEP_PREVIEW_DIR 可从生产渲染路径导出宽、窄窗口及唤醒后的 ANSI 帧，供 VT 解码和 PNG 检查。
