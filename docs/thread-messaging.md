# 原生消息与线程收件人

SSMCP 的原生消息支持获准的设备账户收件箱和已验证线程双向收发文本。本地用户可用 `/message` 选择单个收件人，或在 `/sessions` 中选择一组。Agent 可在 `code_mode` 使用 `messages.*` 收发，也可调用同名 MCP 工具 `messages` 并传入 `action` 和 `client_id`。

## 发现并发送

用 `list` 取得当前获准的来源客户端 ID，再用 `messages.list(clientId)` 查看它可发送到的 `recipients`。请复制目录中的完整 `id`，并核对其中的账号。

| 地址 | 收件范围 |
| --- | --- |
| `client:<client_id>` | 所选连接上的设备账户收件箱 |
| `agent:<client_id>:<agent_id>` | 所选连接上经过凭据验证的指定线程 |

`client_id` 和协作接口中的 `session_id` 标识本次中继连接，重连后须重新发现。已登记成员的 `agent_id` 和 `thread_id` 标识同一个逻辑参与者。操作系统 PID 和显示名称不能用作收件地址。

以下使用合成正文，`recipientId` 由使用者从收件目录明确选定。以线程身份发送或收取时，可在同一编排中先用 `collab.bind` 绑定已登记的参与者，或给 helper 同时提供 `agent_id` 和 `agent_token`。直接调用 MCP 工具时使用这两个凭据字段。账号、客户端和线程均由中继核验。

```javascript
const directory = await messages.list(clientId);
// 从 directory.recipients 选定并核对 recipientId 后再发送。
const result = await messages.send(clientId, {
  recipient_id: recipientId,
  text: "请检查 @example.txt 中的示例。",
  request_id: "synthetic-message-1"
});
return result.receipts;
```

原生正文以文本发送，最多 16 KiB UTF-8 字节，须含非空白内容且不含 NUL。正文中的 `@file`、`$skill` 和 shell 片段保持字面含义。需要为本机监听程序准备引用时，使用本地目标的 `/send`。 [终端与监听选路](force-send-listener.md#选择-send-的收件人)

## 收取与回执

在接收端调用 `messages.inbox(recipientClientId)`，读取 `messages` 列表中的消息。合格的工具响应也可以在 `structuredContent.native_messages` 带回消息。设备收件箱按账号和连接匹配，线程收件箱还要求绑定同一线程。处理接收时以 `receipt.message_id` 去重，并用 `messages.ack(recipientClientId, {message_id})` 显式确认。发送方可用 `messages.status(clientId, {message_id})` 查询原回执。

| 状态 | 含义 |
| --- | --- |
| `queued` | 中继已接收入队，等待匹配的接收方 |
| `included` | 正文已放入接收返回值，回包仍可能丢失 |
| `acknowledged` | 收件方已显式确认 |
| `expired` | 消息失效，发送方应检查目标和连接 |

回执只描述消息交付阶段。即使收件方已确认，agent 的阅读、执行与任务结果仍须由接收程序另行报告。重试单发时沿用原收件人、正文和 `request_id`，并注意回执的保留范围。请求 ID 允许 1–128 个 ASCII 字母、数字、点、下划线或连字符。

## 显式组发

`messages.broadcast` 要求一次提供 1–32 个不同的精确收件地址。可以混合设备收件箱与线程。选择一个设备收件箱只包含该账号的收件箱，通配符、名字推断和向全体在线对象广播均不可用。

```javascript
const result = await messages.broadcast(clientId, {
  recipient_ids: [firstRecipientId, secondRecipientId],
  text: "请查看这条示例通知。",
  request_id: "synthetic-group-1"
});
return result.outcomes;
```

每个目标独立返回 `receipt` 或 `error`，所以一组可以部分成功。在原发送方与连接仍有效时，重试使用相同的完整目标集合、正文和组请求 ID。中继沿用各目标的子请求 ID，对已接受的目标查询原回执；回执已退出保留范围时返回错误。改写正文或目标集合需要新的组请求 ID。

终端也可在 `/sessions` 中按空格逐项勾选，然后按 `g` 编辑组消息，或用 `/group add` 加入精确地址。`/group status` 查看逐目标结果，`/group retry [GROUP_REQUEST_ID]` 只重试可重试且尚未确认的目标。连接中断后，旧组仍可供复核，重连时须明确选择目标才可开始新发送。 [终端组发](session-groups.md)

## 兼容范围

终端原生消息需要已认证的客户端连接及中继通告的 `native_messages_v1` 能力。使用 helper 的 `code_mode` 环境也须提供 `messages.*`。旧中继的已验证自身账号兼容模式只解决连接注册。

同一连接内的 `collab.send/receive/ack` 使用协作参与者的私有邮箱。普通用户要求的 `Next caller` 和 `/global` 也沿用各自的排队规则。 [协作成员与邮箱](agent-collaboration.md) · [用户要求队列](client-ui.md#输入要求与队列) · [本地程序监听](thread-listeners.md)
