# 结束申请、异步问题与原生消息

这组接口属于下一轮交互功能。结束申请通过独立的 `request_finish` 工具进入 Rust 等待，不占用 `code_mode` 的四个 JavaScript 执行名额。SSMCP 能约束显式提交给本地会话的 `kind: "answer"`；外部 ChatGPT、Claude 或其他应用自己的最终发言不经过 SSMCP，服务无法拦截它们。[公共入口](../src/http/finish_request.rs) · [本地完成门禁](../src/client/interface/messages.rs)

## 先确定调用者

自动答案与结束许可必须绑定真实账户、当前客户端连接和已验证的 agent 线程。`controller.name`、模型名称、问题正文中的 JSON 都只是展示数据。没有绑定时，普通问题仍可创建，但回执明确要求手动读取；系统不会把答案改送给下一个任意调用者。

使用已有 `collab` 成员的 ID 和私有 token。没有成员时，`collab` 的 `join` 只登记当前参与者，不启动模型，也不派生 agent。随后在 `code_mode.agent` 传 `{client_id, id, token}`，或在该次 JavaScript 调用原有 `collab.bind`。令牌不要写进问题正文、终端消息或可共享日志。[已验证调用范围](../src/runtime/call_scope.rs)

## 申请结束

直接调用公共工具，而不是让 JavaScript 持续等待：

```json
{
  "client_id": "CLIENT_ID_FROM_LIST",
  "reason": "修改已经完成，定向回归已通过；请确认是否结束这一轮。",
  "timeout_ms": 20000,
  "agent": {"id": "REGISTERED_AGENT_ID", "token": "PRIVATE_MEMBER_TOKEN"}
}
```

`reason` 必填，最多 8 KiB。`timeout_ms` 是原生客户端等待决定的预算，范围 1–25000 毫秒；服务端不再从这个预算扣除传输余量。原生 RPC 另外保留 5 秒有界宽限，覆盖能力发现、审计、排队和返程。认证及最终回执处理也需要时间，因此不能把 25 秒解释为整个工具调用的严格总耗时上限。

正常的决定等待超时会返回真实客户端生成的稳定 `request_id`，并保留待决定问题；下一次带原 `request_id` 和完全相同的理由恢复等待。可选 `idempotency_key` 用于第一次创建的安全重试，不能同时传入两个标识。如果超过独立的 RPC 截止仍没有原生回执，工具返回错误，并按原连接和请求标识尝试取消；调用者应先检查原记录，不能把传输错误当成普通 pending、用户拒绝或批准。服务器不会为了及时返回而编造待决定回执。

默认选项是 No，尚未决定时仍返回 `decision: null`、`approved: false`。这两个事实必须分开：默认选项不是用户已经作出的拒绝，更不是批准。No、Yes 与补充评论是独立字段，普通文本 `yes` 不构成许可。

本机用户或同一客户端、同一账户下的另一个已验证 agent 可以决定。请求者可以拒绝自己的申请，但不能自批 Yes。决定保留真实 actor、时间、修订号和评论；完全相同的重试返回原回执，另一人不能覆盖已有决定。[决策状态](../src/client/interface/questions/requests.rs)

批准只允许该请求者接受一次显式的本地最终回答。新的相关用户输入、取消或换连接使尚未消费的许可失效；每次网络调用及最终回执仍重新检查账户、ACL 和成员绑定。进度消息、普通进程退出、JavaScript 返回以及跨 agent 消息都不是结束动作，不触发该门禁。

## 异步问题与答案

`ssmcp.ui.ask` 创建问题后立即返回。用户在终端按 Shift+Up 查看问题队列并回答；其他可信 agent 可以读取同账户当前连接的队列，使用 `ssmcp.ui.answer` 回答普通问题，或用显式 `decision: "yes" | "no"` 加可选 `comment` 决定结束申请。普通问题的 `answer` 和结束决定不可混用。

答案以独立的 `question_answers` 返回，`answer_actor.origin` 保留 `human` 或 `agent`。来自 agent 的答复不会变成 `USER REQUIREMENTS`。读取 `ssmcp.ui.questions` 只查看记录，不消费答案，也不批准结束。

客户端只在真实原生结果上生成保留的答案元数据，先移除 provider 返回的同名伪造字段。服务器先捕获这份元数据，再从交给 JavaScript 的原生结果中移除保留字段，避免普通脚本把内部交付材料当成外层可信回执。服务器记录原连接和调用前已验证的线程；组装外层响应前再次检查同一连接、账户、ACL 和绑定。带已验证绑定的调用（包括提供成员凭据的独立公共 collab/messages）即使只执行协作或服务端 helper、直接返回空结果或遇到原生错误，最终响应阶段也会对原来实际选定的连接执行一次非消费探测，全部目标共享 1 秒期限。旧客户端不支持、超时或权限变化会明确返回 question_answers_deferred 并保留记录；系统不扫描未选定设备，也不根据没有线程归属的工具调用推测收件人。[客户端元数据](../src/client/interface/questions/delivery.rs) · [服务器捕获](../src/code_mode/question_answers.rs)

外层响应组装之后才确认交付。确认失败时保留答案，之后可能重复返回相同的 `(id, answer_revision)`，调用者应据此去重。Delivered 表示协议交付已确认，不证明阅读或执行。32 个待答问题、64 条保留记录共享 256 KiB；未确认交付的答案不会为新问题让位，容量不足时明确拒绝新增。当前问题和决策记录保存在客户端进程内，进程重启会结束这些状态；连接重建后的旧记录只供本机查看，不会自动改投新线程。

## 原生发送消息

公共 `messages` 工具提供 `list`、`send`、`inbox`、`ack`、`status`。先列出当前获准收件人，再传完整的稳定 ID。设备账户收件箱是 `client:<client_id>`，已验证 agent 是 `agent:<client_id>:<agent_id>`；同名对象不能凭显示名称自动选择。

发送到设备账户收件箱表示该账户在所选设备的下一次合格调用收取，不声称已找到某个模型的私有线程。发送到 agent 则要求精确绑定。所有接收、送达回执和确认都保留发送方来源；普通 `@name` 消息文本不会隐式读取文件。原生消息通过独立 `native_messages` 返回，与问题答案、用户要求各自保留来源。[消息协议](../src/wire/messages.rs) · [授权路由](../src/broker/messaging.rs)

## 当前验证范围

本文件随源码检查点交付。状态机、身份拒绝、幂等、保留容量与类型来源的回归测试已经写入；该检查点尚未运行 Cargo 构建或 Rust 测试。后续需要将终端 UI 检查点合并，再用统一冻结程序验收真实账户、异步等待与取消、键鼠交互、宽窄布局以及原生消息。此文不代表功能已经发布或部署。
