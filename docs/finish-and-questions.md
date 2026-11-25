# 结束决定、异步问题与原生消息

成员连续空闲达到配置期限（默认 5 分钟）进入 Dreaming，连续 Dreaming 满 30 分钟后才可申请主动离线。明确拒绝后从该次拒绝起再等完整 30 分钟，待决沿用原申请 ID 安静等待。新需求或 claim 立即恢复 working 并重置计时。协调器为自己申请时，通过 `ui.wait(intent: "finish")` 请求本机真实用户决定。先绑定已验证的提问线程，读取最终外层 `structuredContent.user_wait`：只有匹配的 Human Yes 被原线程按精确申请和修订号消费，并经最终权限与输入复核返回 `status: "decided"`、`may_finish: true`，才允许本轮结束。超时、普通消息、取消以及问题列表中的批准状态都不能代替这份回执。[完整参数与回执](user-wait.md#结束前取得真实用户决定) · [服务端协调](../src/http/user_wait.rs)

## 申请结束

`ui.wait` 使用 `intent: "finish"`，可以带 `reason`、1–25 秒的 `timeout_ms` 和 `notify`。没有已验证线程绑定时返回错误。若已有可交付的普通输入，服务按原协议交付输入，不创建新的结束申请，也不批准结束；否则，同一次原生等待创建或续接真实的 No/Yes 问题。[本机等待入口](../src/client/interface/user_wait.rs)

用户在 Questions 中选择 No 或 Yes，再按 Enter 提交；默认选中 No 仍是“未决定”。评论中的 `yes`、唤醒画面和 agent 参数都不能代替用户的选择。No 表示继续工作；Yes 只用于原提问线程的这一次精确申请，不授予其他线程或后续请求通行权。其他 agent 不能代用户批准 Yes。[Human 决策规则](../src/client/interface/questions/requests.rs)

一次有界等待超时后，保留实际返回的 `finish_request_id`，在新的外层调用中绑定同一线程并带该 ID 续等。超时不批准也不拒绝；取消、新的相关输入、断线或重连、权限与绑定变化都不能产生许可。已消费的 Yes 再被读取或续等时 `consumed: false`，不能重放。[续接与取消](../src/client/interface/questions/workflow.rs)

脚本内部的 `decision_ready` 仍不是最终结果；最终外层回执必须同时包含匹配的申请 ID、`finish_decision.status: "completed"`、`choice: "yes"`、`actor: {"origin":"human"}`、正的修订号、`consumed: true`、空的失效原因，以及 `may_finish: true`。[回执提交](../src/code_mode/user_wait.rs)

普通进度和 `ssmcp.ui.message` / `ssmcp.ui.present` 的 `kind: "answer"`、`ui.reply` 继续可用，并保留真实会话归属、历史记录和幂等处理。这些消息入口不是结束许可的消费入口；外部模型自行停止或发出最终发言也不会被 MCP 强制拦截。调用者应在结束前完成上述真实用户决策流程。[消息提交](../src/client/interface/messages.rs) · [会话归属](../src/runtime/call_scope.rs)

## 继续退役的旧入口

当前结束确认使用 `ui.wait(intent: "finish")`，不恢复旧公共工具。直接调用下列旧名称仍返回 `tool_retired`；普通任务、消息和用户输入等待保留。[公共分派](../src/http.rs) · [原生分派前校验](../src/wire.rs) · [等待目标](../src/http/await_tool.rs)

| 原入口 | 当前行为 |
|---|---|
| `request_finish` | 退役；结束决定从已验证线程的 `ui.wait` 发起。 |
| `ssmcp.ui.finish` | 退役，不通过旧工具创建、恢复或批准结束请求。 |
| `ssmcp.ui.answer` | 退役，agent 不能代替本机用户回答问题。 |
| 公共 `await` 的 `type: "finish"` | 退役；它与当前 `ui.wait` 的 `intent: "finish"` 是不同接口。 |

## 异步问题与答案

`--input none` 禁用所选连接的普通问题、人工等待和回复。仍受支持的聊天入口返回 `chat_disabled`，全局退役的名称继续返回 `tool_retired`。独立的原生消息仍可按已认证收件人投递。[连接能力](client-input.md)

`ssmcp.ui.ask` 创建问题后立即返回，真实用户在终端按 Shift+Up 查看并回答。回答通过独立的 `question_answers` 交付；`ssmcp.ui.questions` 可以手动查看，读取不会消费答案。历史记录里的 `human` 与 `agent` 来源继续保留，不把旧 agent 答案改写成用户要求。[问题队列](../src/client/interface/questions.rs) · [答案交付](../src/client/interface/questions/delivery.rs)

自动交付仍绑定认证账户、原客户端连接和已验证的提问线程。使用现有 `collab` 成员的 ID 和私有 token，在 `code_mode.agent` 或 `collab.bind` 中绑定；`collab.join` 只登记当前参与者，不启动模型。未绑定的普通问题明确要求手动读取，系统不会把答案改投下一个任意调用者。[调用范围](../src/runtime/call_scope.rs) · [协作登记](../src/collab.rs)

服务器只接收真实原生回调中的答案元数据，并在组装外层响应前重新检查连接、账户、ACL 和线程绑定。非消费探测仍共享 1 秒期限；旧客户端、超时或权限变化会返回 `question_answers_deferred` 并保留记录，普通脚本不能用同名字段伪造交付。[可信捕获](../src/code_mode/question_answers.rs) · [响应复核](../src/http.rs)

答案在外层响应组装后才确认交付，确认失败时可能按同一 `(id, answer_revision)` 重复返回。32 个待答问题和 64 条保留记录共享 256 KiB，未确认交付的答案不会为新问题让位；进程重启结束当前队列状态，重连后的旧记录不会自动改投新线程。[队列与容量](../src/client/interface/questions.rs) · [确认协议](../src/client/interface/questions/delivery.rs)

## 线程结果与原生消息

`ssmcp.ui.reply` 继续回传已交付线程输入的进度和结果。它只完成被指定的输入，服务器按真实调用者身份重新检查原连接和接收线程，并在记录结果后返回 `accepted: true`；结果保留 agent 来源。[原生结果](../src/client/interface/reply.rs) · [服务器提交](../src/broker/session_io.rs)

公共 `messages` 工具按完整收件人 ID 路由消息。设备账户收件箱为 `client:<client_id>`，已验证线程为 `agent:<client_id>:<agent_id>`；设备消息在该账户下一次合格调用中领取，不自动启动模型。原生消息通过独立的 `native_messages` 返回，并保留发送者来源。[消息协议](../src/wire/messages.rs) · [授权路由](../src/broker/messaging.rs)

## 历史验证记录

以下记录对应旧版本的结束审批行为，不是当前 `ui.wait` 结束决策集成的验收结论。

2026-11-20 在不可变源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92` 上完成了 106 项定向 Rust 回归，全部通过，格式和差异检查也通过。其中 35 项覆盖问题与结束状态机，3 项验证公共结束接口的原生回执边界，6 项覆盖原生最终消息，26 项验证通用等待及真实内存账户、成员和用户输入队列之间的集成。[验证命令、分组及范围](validation/await-finish-20261120.md)

这些验证涵盖权限撤销、等待恢复、取消竞争、交付保留和一次性人工许可。它们没有登录真实外部模型，也没有更换运行中的 relay 或客户端；终端键鼠、宽窄布局和正式发布仍由整合后的冻结验收负责。本文件不代表功能已经发布或部署。
