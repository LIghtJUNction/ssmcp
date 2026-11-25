# 定时提示词

`ssmcp.time.prompt` 把安排绑定到创建时的原生连接和已验证 Agent 会话。到期后，原生调度器推进事件，把当前版本记录为原会话中的输入，来源标记为 `agent_timer`。只有该精确版本已有记录，正文才会从 `read` 返回，或由同一 Agent 的后续响应通过 `prompt_hooks` 带回。创建安排不会启动模型，也不授予用户消息或审批的权限。[调度器](../src/runtime/time/prompts.rs) · [原生输入记录](../src/client/interface/messages.rs) · [响应交付](../src/http/prompt_hooks.rs)

终端会显示安排、暂停、恢复和取消提醒等操作。中继也会在部分工具响应返回前读取本会话的到期提醒。成功的后台读取无需在对话中单列，真正到期的正文会以“定时提示”进入原会话。读取失败时，终端显示“检查到期提醒”及失败状态。

## 创建一次或重复提醒

先用 `collab.bind` 绑定自己的私有身份，并从当前客户端的工具目录确认它实际提供 `ssmcp.time.prompt`。以下调用在一秒后产生一次提示词：

```javascript
return await call(clientId, "ssmcp.time.prompt", {
  action: "schedule",
  request_id: "review-build-001",
  after_ms: 1000,
  lifetime_ms: 60000,
  template: "检查 {{project}} 的构建结果；计划时间 {{scheduled_at}}，实际时间 {{fired_at}}。",
  variables: {project: "ssmcp"}
});
```

指定绝对时间时，用 Unix 毫秒时间戳 `at_unix_ms` 替代 `after_ms`。设置 `interval_ms` 可以重复触发，最短间隔为 1000 毫秒；`max_fires` 限制逻辑触发次数，`lifetime_ms` 限制总期限，最长为 24 小时。重复提交相同 `request_id` 和相同定义会找回原安排；相同 ID 搭配不同定义会报错。

模板只替换 `{{变量名}}`。变量可以是有界的标量值；不会执行脚本或再次展开替换后的内容。内置变量包括 `timer_id`、`scheduled_at`、`fired_at`、`tick` 和 `missed`；时间值为 Unix 毫秒。私有模板与变量不进入等待面板的摘要。

## 接收、重读与确认

系统把到期事件作为 `prompt_hooks` 附加到同一已验证 Agent 的后续响应。事件的来源固定为 `agent_timer`，包含 `timer_id`、`event_id`、`revision`、计划与实际触发时间，以及渲染后的 `body`。它不能代替 `user_messages`、用户回答或允许结束的回执。

没有 HTTP 调用时，原生调度和输入记录仍可继续；原生记录与工具响应中的转交可以发生在不同时间。工具响应会带回已确认记录的正文，但不会主动创建新的 ChatGPT 回合。这些输入不会进入人类请求队列，也不会取消已请求的维护轮次。可以显式读取原安排：[被动输入处理](../src/broker/session_io.rs)

```javascript
return await call(clientId, "ssmcp.time.prompt", {
  action: "read", timer_id: originalTimerId, limit: 4
});
```

`read` 返回空的 `events` 不一定表示没有待处理事件。`visibility_deferred` 大于零时，至少一个待处理版本尚未确认原生记录。`status` 的 `schedule.visibility` 为 `none` 时没有待处理事件；`queued`、`in_progress` 表示发布尚未完成；`recorded` 表示该版本已有记录；`rejected`、`unknown` 保留实际发布结果。继续观察原 `timer_id`，不要重新安排或重发结果不明的效果。历史缺失或被淘汰不能证明发布从未发生。[发布状态](../src/runtime/time/prompts.rs) · [原记录核对](../src/client/interface/history/ownership.rs)

原生记录、向工具调用返回正文、处理后的 ACK 是三个阶段。记录成功不证明模型已处理或用户已阅读；旧版本已记录也不代表新版本可读。读取不确认。处理完成后，按刚才读取的精确事件 ID 与版本确认：

```javascript
return await call(clientId, "ssmcp.time.prompt", {
  action: "ack", timer_id: originalTimerId,
  event_id: event.event_id, revision: event.revision
});
```

一次安排最多保留一个未确认事件。若多个间隔在读取前经过，系统合并为最新状态，更新 `tick`、`missed` 和 `revision`，避免补发大量过时提示词。旧版本 ACK 不会清除更新后的事件；同一个 ACK 的重试也不会确认下一次触发。

## 暂停、取消与连接范围

`list` 查看当前身份的摘要；`status` 查看一个 `timer_id`。`pause` 冻结距下一次触发的剩余时间，`resume` 恢复这个间隔；总生命周期继续推进。`cancel` 停止未来触发，已经排队的事件仍需读取并精确确认。

```javascript
return await call(clientId, "ssmcp.time.prompt", {
  action: "cancel", timer_id: originalTimerId
});
```

安排绑定原来的原生连接和 Agent 身份。HTTP 调用中断不销毁安排；原生客户端断开、重载或关闭会结束该命名空间，新的客户端连接不能接管旧安排。活动安排和保留记录都有数量上限，未确认事件不会被静默淘汰。终态已确认记录保留五分钟，方便识别重复请求。

等待面板展示安排状态、下一次触发时间、逻辑次数与待确认数量。完整正文记录在原会话的输入区，并通过对应 Agent 的已授权响应返回；它与人类发言、系统提示和订阅通知使用同一套右侧输入展示。[后台等待](await.md) · [输入与引用回复](client-input.md#统一的输入与引用回复)
