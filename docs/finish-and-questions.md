# 最终回复、异步问题与原生消息

最终回复现在直接使用 `ssmcp.ui.message` 或 `ssmcp.ui.present` 的 `kind: "answer"`，无需申请结束许可。它们仍要求真实会话归属，并按原来的身份、历史记录和幂等键处理提交。[消息提交](../src/client/interface/messages.rs) · [会话归属](../src/runtime/call_scope.rs)

## 已退役的入口

结束审批和 agent 代答入口已从公共目录与原生目录移除。直接调用旧名称会返回 `tool_retired`，不会创建问题卡、等待审批或写入代答；旧 `await` 的 `finish` 目标也已停用。[公共分派](../src/http.rs) · [原生分派前校验](../src/wire.rs) · [等待目标](../src/http/await_tool.rs)

| 原入口 | 当前行为 |
|---|---|
| `request_finish` | 退役，最终回复不再依赖它。 |
| `ssmcp.ui.finish` | 退役，不创建、恢复或批准结束请求。 |
| `ssmcp.ui.answer` | 退役，agent 不能代替本机用户回答问题。 |
| `await` 的 `type: "finish"` | 退役，普通任务、消息和用户输入等待保留。 |

调用响应不再注入强制续做、结束同意或会话催答提示。等待仍返回真实状态、截止时间结果和消息回执；超时表示本次等待没有收到可交付输入，调用者可根据任务需要再次等待。[等待回执](../src/code_mode/user_wait.rs) · [响应交付](../src/http.rs)

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

以下记录对应旧版本的结束审批行为，不是本次退役改动的验证结论。

2026-11-20 在不可变源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92` 上完成了 106 项定向 Rust 回归，全部通过，格式和差异检查也通过。其中 35 项覆盖问题与结束状态机，3 项验证公共结束接口的原生回执边界，6 项覆盖原生最终消息，26 项验证通用等待及真实内存账户、成员和用户输入队列之间的集成。[验证命令、分组及范围](validation/await-finish-20261120.md)

这些验证涵盖权限撤销、等待恢复、取消竞争、交付保留和一次性人工许可。它们没有登录真实外部模型，也没有更换运行中的 relay 或客户端；终端键鼠、宽窄布局和正式发布仍由整合后的冻结验收负责。本文件不代表功能已经发布或部署。
