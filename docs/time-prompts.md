# 定时提示词

`ssmcp.time.prompt` 为当前 Agent 的原生连接保存一个有界的时间安排。到点时，客户端把渲染后的提示词加入队列；同一 Agent 的后续工具响应会带回 `prompt_hooks`。创建提示词不会启动模型，也不会把提示词变成用户消息或审批。[调度器](../src/runtime/time/prompts.rs) · [响应交付](../src/http/prompt_hooks.rs)

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

没有 HTTP 调用时，原生调度器仍会推进时间并保留队列。交付发生在下一次符合条件的工具响应；它不会主动创建一个新的 ChatGPT 回合。也可以显式读原队列：

```javascript
return await call(clientId, "ssmcp.time.prompt", {
  action: "read", timer_id: originalTimerId, limit: 4
});
```

读取不确认。处理完成后，按刚才读取的精确事件 ID 与版本确认：

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

等待面板展示安排的状态、下一次触发时间、逻辑次数和待确认数量。完整正文只交付给对应的已授权 Agent。[后台等待](await.md)
