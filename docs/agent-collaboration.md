# 会话内 Agent 协作

同一条在线客户端连接可以容纳多个独立 agent。每个 agent 显式加入后，获得自己的身份和收件箱，可以发现其他成员、发送私信、广播进展、读取并确认消息。协作状态由服务器维护；agent 可以来自不同应用，也可以共用同一 GitHub 账号。[会话实现](../src/broker.rs) · [协作实现](../src/collab.rs)

这里的“会话”指 `list` 返回的一个在线 `client_id`。每条连接有独立的成员表和邮箱；即使两条连接的名字、目录相同，成员和消息也各自独立。

## 接口与职责

公开 MCP 工具 `collab` 接收 `action`、`client_id` 和该操作的参数。所有操作都检查当前 OAuth 授权与目标连接的访问权限；已加入成员还需提供自己的 `agent_id` 和 `token`。

| action | 额外参数 | 作用 |
| --- | --- | --- |
| `join` | `name`；可选 `role`、`capabilities` | 加入会话，返回私密凭证、成员快照和限额 |
| `list` | 无 | 查看当前有效成员的公开资料 |
| `update` | 成员凭证；可选 `name`、`role`、`status`、`capabilities` | 更新自己的资料并续租；不传资料字段时仅续租 |
| `send` | 成员凭证、`to`、`payload`；可选 `topic`、`reply_to`、`idempotency_key` | 发送 JSON 私信或向当前成员广播 |
| `receive` | 成员凭证；可选 `after`、`limit`、`wait_ms` | 非破坏读取自己的邮箱，可等待新消息 |
| `ack` | 成员凭证、`through` | 确认已处理的消息，释放邮箱容量 |
| `leave` | 成员凭证 | 退出会话，清理自己的邮箱并结束正在等待的收件请求 |

`code_mode` 也提供 `collab.join/list/update/send/receive/ack/leave(clientId, options)`。这些 helper 返回解包后的业务结果，错误会抛出异常。`agents.*` 继续用于读取 AGENTS 指令文件。

另有 `collab.bind(clientId, {agent_id, token})` 用于把本次 `code_mode` 编排绑定到已登记成员，以接收客户端输入框中的定向用户要求。它是代码模式 helper，不是直接 `collab` 工具新增的 action。[绑定与用户要求](#绑定本次编排并接收用户要求)

**持续等待消息时，直接调用公开 `collab` 工具。** 直接 `receive` 最长等待 30 秒，由 Rust 服务端处理，不占用 `code_mode` 的四个 JavaScript 执行槽。JavaScript 中的 `collab.receive` 最长等待 1 秒，适合在一次编排中收取消息。

## 加入并发现成员

先调用 `list({})`，选择要共同操作的客户端 ID，再让每个 agent 分别加入。下面的 JSON 是 MCP `tools/call` 的 `params`；将大写占位符换成实际返回值。

~~~json
{
  "name": "collab",
  "arguments": {
    "action": "join",
    "client_id": "CLIENT_ID_FROM_LIST",
    "name": "implementation",
    "role": "developer",
    "capabilities": ["rust", "tests"]
  }
}
~~~

成功响应的 `structuredContent` 包含以下字段。使用 JavaScript helper 时直接得到这份对象。

| 字段 | 含义 |
| --- | --- |
| `agent_id` | 服务端生成的公开成员 ID，用于定向发送 |
| `token` | 只属于当前成员的私密凭证 |
| `lease_ms`、`expires_at_ms` | 租约时长与当前到期时间 |
| `cursor` | 加入时的会话序号，作为该成员的初始收件游标 |
| `agents` | 当前有效成员的公开资料，包含自己 |
| `limits` | 当前服务器的成员、消息、邮箱和等待限额 |

**调用方需要保存 `client_id`、`agent_id` 和 `token`。** 每次 `code_mode` 都是独立执行，JavaScript 变量不会跨请求保留。保存凭证后在后续请求中显式传入，避免每次工具调用都重新 `join`。`token` 不应发给其他成员，也不应进入广播、共享文档或公开日志。

同样的加入操作可写成：

~~~javascript
const clientId = "CLIENT_ID_FROM_LIST";
const joined = await collab.join(clientId, {
  name: "implementation",
  role: "developer",
  capabilities: ["rust", "tests"]
});
return { client_id: clientId, ...joined };
~~~

其他 agent 使用同一个 `client_id` 加入后，可通过 `list` 互相发现：

~~~javascript
return await collab.list("CLIENT_ID_FROM_LIST");
~~~

公开资料包含 `agent_id`、`name`、`role`、`status`、`capabilities`、加入时间、最近活动时间和租约到期时间。`status` 初始为 `available`；角色、能力与状态都是成员自行填写的工作信息。名称允许重复，消息目标始终使用 `agent_id`。

## 绑定本次编排并接收用户要求

客户端输入框可以把用户要求定向给一个已登记成员。该成员在每次 `code_mode` 开始时，使用自己保存的凭证声明本次身份：

~~~javascript
const clientId = "CLIENT_ID_FROM_LIST";
await collab.bind(clientId, {
  agent_id: "AGENT_ID_FROM_JOIN",
  token: "PRIVATE_TOKEN_FROM_JOIN"
});
return await exec.list(clientId);
~~~

绑定只覆盖本次编排，同一客户端不能在一次运行中切换到另一成员。服务端校验当前授权、目标连接和成员凭证，在响应交付前再次核对成员仍然有效。名称、角色和 `controller` 展示字段不参与接收身份判断；不要把同名成员视为同一身份。

客户端与服务器支持协议 v7 时，本地要求可附加到下一次符合目标的 `code_mode` 响应，位于 `structuredContent.user_messages` 和独立的用户要求文本块。模型或前端应同时检查这部分内容。`Next caller` 面向下一位实际访问本客户端的合格调用方；指定成员 ID 的要求只交付给绑定该身份的编排。

这条通道与成员邮箱分别管理：`collab.send/receive/ack` 用于 agent 之间的 JSON 消息；`ssmcp.ui.queue` 用于设备操作者提交、取回和修改用户要求。输入框要求不通过 `collab.ack` 确认，`delivered` 只表示附加到了响应，不代表模型已经阅读或执行。编辑竞争、队列容量和重载恢复见[客户端 UI](client-ui.md#输入要求与队列)。

## 发送私信与广播

定向消息只进入目标成员的邮箱。下一次 `code_mode` 请求可以用已保存凭证寻找一个负责评审的成员，并发出请求：

~~~javascript
const clientId = "CLIENT_ID_FROM_LIST";
const self = {
  agent_id: "AGENT_ID_FROM_JOIN",
  token: "PRIVATE_TOKEN_FROM_JOIN"
};
const { agents } = await collab.list(clientId);
const reviewers = agents.filter(agent =>
  agent.agent_id !== self.agent_id && agent.role === "reviewer"
);
if (reviewers.length !== 1) {
  throw new Error("Select one reviewer agent_id from collab.list");
}
return await collab.send(clientId, {
  ...self,
  to: reviewers[0].agent_id,
  topic: "review",
  payload: {
    commit: "COMMIT_TO_REVIEW",
    paths: ["src/http.rs"],
    question: "检查授权与断线处理"
  },
  idempotency_key: "review-request-1"
});
~~~

`to: "*"` 广播给发送时已经加入的其他成员，不包含发送者。之后加入的成员从自己的加入位置开始接收消息，不补发此前的广播。

~~~javascript
return await collab.send("CLIENT_ID_FROM_LIST", {
  agent_id: "AGENT_ID_FROM_JOIN",
  token: "PRIVATE_TOKEN_FROM_JOIN",
  to: "*",
  topic: "progress",
  payload: {
    task: "collaboration",
    state: "ready_for_review",
    commit: "COMMIT_TO_REVIEW"
  },
  idempotency_key: "progress-ready-1"
});
~~~

`payload` 可以是任意 JSON 值，包括显式 `null`，但请求必须提供这个字段。发送成功返回 `message_id`、`seq`、实际 `recipients` 和 `duplicate`。这表示消息已经入队；收件者完成工作后可以再发一条回复，用 `reply_to` 携带原 `message_id`。`reply_to` 是关联信息，消息路由仍由 `to` 决定。

`idempotency_key` 用来处理发送响应丢失后的重试。每个发送者在最近 128 次成功的新发送中保留去重记录，窗口也随未提供 key 的新发送推进；同一个 key 与相同内容返回原回执，`duplicate` 为 `true`，不重复入队。同一个 key 改用不同目标、正文、主题或回复关联会被拒绝。为每条新的逻辑消息分配新的 key，重试时保持原 key 和内容。

## 收取、处理、确认

`receive` 不会删除消息。调用方可以重试读取，并在确认一页消息全部处理完毕后，用该页的 `next_cursor` 调用 `ack`。收件响应丢失时，邮箱中的未确认消息仍可再次读取。

长等待通过直接 MCP 工具发送：

~~~json
{
  "name": "collab",
  "arguments": {
    "action": "receive",
    "client_id": "CLIENT_ID_FROM_LIST",
    "agent_id": "AGENT_ID_FROM_JOIN",
    "token": "PRIVATE_TOKEN_FROM_JOIN",
    "limit": 32,
    "wait_ms": 30000
  }
}
~~~

省略 `after` 时，从最后已确认的位置开始读取。返回的 `messages` 按会话序号递增，每条包含：

| 字段 | 含义 |
| --- | --- |
| `seq` | 会话内递增序号，用于收件游标 |
| `message_id` | 不透明消息 ID，用于回复关联与业务去重 |
| `from`、`to` | 发送者与目标成员 ID；广播的 `to` 为 `"*"` |
| `payload` | 发送的 JSON 值 |
| `topic`、`reply_to` | 可选主题和关联消息 ID，未提供时为 `null` |
| `sent_at_ms` | 服务端发送时间，Unix 毫秒 |

结果还包含 `next_cursor`、`has_more` 和 `acked_cursor`。`has_more: true` 时可用 `after: next_cursor` 读下一页；游标必须位于当前已确认位置与该成员已经收到的最高游标之间。跳到未知的未来位置或读取已经确认清理的位置会明确失败。

短收件可以放进 `code_mode`：

~~~javascript
return await collab.receive("CLIENT_ID_FROM_LIST", {
  agent_id: "AGENT_ID_FROM_JOIN",
  token: "PRIVATE_TOKEN_FROM_JOIN",
  limit: 32,
  wait_ms: 1000
});
~~~

查看结果、完成对应工作后，再确认已经处理的那一页。下面假设该页的 `next_cursor` 为 `42`；实际调用必须使用收到的游标。

~~~json
{
  "name": "collab",
  "arguments": {
    "action": "ack",
    "client_id": "CLIENT_ID_FROM_LIST",
    "agent_id": "AGENT_ID_FROM_JOIN",
    "token": "PRIVATE_TOKEN_FROM_JOIN",
    "through": 42
  }
}
~~~

`ack` 清理 `seq <= through` 的消息，允许重复确认旧位置，拒绝确认高于已经交付给该成员的游标。处理后、确认前发生故障仍会导致重复读取；对修改文件、提交任务等外部副作用，调用方应以 `message_id` 或自己的业务 ID 保证重复处理安全。协作层提供可重读邮箱与有限窗口的发送去重，业务处理的 exactly-once 需要由调用方完成。

## 在线状态与生命周期

每个成员的租约为 15 分钟，成功的成员操作会续租；开始收件等待也会续租。单纯查询 `list` 不续租。需要长时间思考或等待其他系统时，调用方应在租约到期前 `update`，同时可以向同伴说明自己的状态：

~~~javascript
return await collab.update("CLIENT_ID_FROM_LIST", {
  agent_id: "AGENT_ID_FROM_JOIN",
  token: "PRIVATE_TOKEN_FROM_JOIN",
  status: "reviewing",
  role: "reviewer"
});
~~~

`name`、`status` 最多 128 个 UTF-8 字节；`role` 最多 128 字节，`update` 传 `role: ""` 可清空角色。`capabilities` 最多 16 项，每项为 1–64 字节。主题、回复 ID 和发送去重 key 若提供，须为 1–128 字节；这些文字字段均拒绝控制字符。

租约到期后，成员不再出现在有效名单中，其邮箱和凭证被清理。主动 `leave` 也会删除成员与邮箱，并结束该成员的收件等待；重复使用已经离开的凭证返回成员失效。退出前先处理需要保留的消息。

| 事件 | 后续处理 |
| --- | --- |
| 单次 HTTP 请求结束、`code_mode` 返回 | 保留成员与未确认消息，在租约内继续使用原凭证 |
| 收件响应丢失、调用方短暂断网 | 使用原凭证重读；发送重试沿用原 `idempotency_key` |
| 成员租约过期或 `leave` | 重新 `join`，取得新的身份与邮箱 |
| 开发机客户端断开、重载后重连 | 旧协作空间关闭；重新 `list`，选择新 `client_id` 并加入 |
| `ssmcpd` 重启 | 内存中的成员和消息结束；恢复连接与授权后重新加入 |

成员凭证同时绑定当前授权主体和在线客户端连接。同一 GitHub 用户的多个 agent 各自持有不同凭证，知道同伴的公开 ID 仍无法读取它的私信。成员名和 `controller.model` 都只是展示信息，不参与身份认证。OAuth 刷新后，只要授权主体、目标连接和成员租约仍有效，就可继续使用原成员凭证。

## 容量与背压

邮箱达到容量时，`send` 会明确失败，调用方应等收件者处理并确认后再重试。广播先检查所有目标邮箱，再一次性入队；某个目标邮箱已满时整次广播失败，避免一部分成员收到、一部分成员漏收。

| 范围 | 上限 |
| --- | --- |
| 同一客户端连接的有效成员 | 64 |
| 单条 `payload` 序列化后的 UTF-8 字节 | 16 KiB |
| 每个成员未确认邮箱 | 128 条，且消息总字节最多 256 KiB |
| 整个会话的逻辑排队字节 | 4 MiB，广播按每个目标邮箱分别计入 |
| 单次收件 | 64 条，且 JSON 业务结果最多 128 KiB；默认 32 条 |
| 同一成员并发等待 | 1 个 |
| 直接 `collab.receive` 等待 | 0–30,000 ms |
| JavaScript `collab.receive` 等待 | 0–1,000 ms |
| 发送去重窗口 | 每个发送者最近 128 次成功的新发送 |

邮箱字节数包括消息封装字段；`payload` 的 16 KiB 上限单独检查。广播复用内存中的消息对象，逻辑配额仍按投递份数计算，防止大量广播占满所有邮箱。未确认消息不会因新消息入队而被静默覆盖；它们在确认、成员结束或会话结束时释放。

较大的日志、补丁和工件应保存在已有工作区，再通过 `payload` 交换路径、提交 ID、产物 ID 或摘要。对方读取这些对象时仍走现有文件或工具权限检查。

## 调度边界与实现入口

协作消息作为 JSON 数据交给收件者，收件者决定如何处理。`send` 不会执行消息中的命令、启动模型或唤起一个已经停止运行的 agent；调用方需要安排收件循环，或在任务阶段之间主动检查邮箱。

这组原语负责成员、消息和确认。任务分配、领导者选择、代码所有权以及并行修改策略由调用方约定，可以直接放进 `role`、`status` 和 `payload`，无需为每一种协作方式添加新的服务端工作流。

实现与验证入口：

- [有界成员表、邮箱、租约和重试去重](../src/collab.rs)
- [公共 MCP 路由与工具 Schema](../src/http.rs)
- [当前授权与会话访问检查](../src/bridge.rs)
- [JavaScript helper](../src/code_mode.js)
- [HTTP 协作测试](../tests/collaboration.rs)

已连接的 MCP 应用可以读取 `ssmcp://collaboration` 获取本文，从 `ssmcp://limits` 获取当前构建的限额。

# 结束决定与原生消息

交互接口复用当前参与者绑定，不启动额外模型。真实用户通过 `ssmcp.ui.ask` 的本机问题队列回答；agent 代答与结束审批入口已退役，正常进度、最终消息和 `ui.reply` 线程结果仍保留来源。问题答案独立放在 `question_answers`，不会伪装成用户要求。[当前交互协议](finish-and-questions.md)

跨客户端原生消息使用 `messages` 的 list/send/inbox/ack/status。必须选择列出的完整稳定收件人 ID；同名显示标签不能代替身份。设备账户收件箱表示该账户下一次合格调用收取，agent 收件箱则要求精确绑定。独立 `native_messages` 保留实际来源，送达与确认不等于已读。完整边界与当前尚未完成的运行验收见[结束申请、异步问题与原生消息](finish-and-questions.md)。
