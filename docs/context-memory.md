# 上下文、压缩与分支记忆

ssmcp 在工作区内保存一份有容量上限的会话记忆：一个检查点摘要，加上最近的消息摘录。客户端再次启动时恢复上次使用的分支；agent 下一次调用该客户端的原生工具时，可以在工具响应中读到这份记忆。

这解决的是“重新连接后，怎样把已有进展带回 agent”的问题。外部模型自己的上下文窗口由它所在的平台管理；ssmcp 的本地压缩不会清空那个窗口，也不会额外启动模型。

## 在输入框使用

| 命令 | 作用 |
| --- | --- |
| `/context` | 查看当前工作区的分支、当前分支与 revision |
| `/memory` | 阅读当前摘要、最近摘录及原始消息标识 |
| `/compact` | 从已有记忆生成标明来源的本地摘录检查点，保留最近四条记录 |
| `/compact 摘要正文` | 将自己输入的摘要保存为检查点 |
| `/fork 方案 B` | 复制当前记忆并切换到新分支 |
| `/switch context_…` | 切回 `/context` 列出的某个分支 |

这些命令在本地执行。创建分支时复制已有摘要与摘录；之后各分支独立接收新增记录。分支 ID 是本地记忆的标识。它不代表另一个模型、进程或 Git 分支。

输入框原有的图片、收件人和暂存草稿沿用当前会话的保护规则；记忆操作不领取或重放要求队列。要继续先前的需求，请先查看队列状态。

## 哪些内容进入记忆

客户端确认保存的 assistant 消息可以成为最近记忆。普通消息、原生 Intelligence UI 的可复制文字均沿用同一会话记录通道。

用户要求只有在服务器确认 `Delivered`、并且目标是共享的 `next` 时才进入共享记忆。仍在发送、排队、取回或编辑的要求不进入记忆；发给指定 agent 的要求也不进入共享记忆。这样，读取历史不会抢走队列里的新要求，也不会把定向要求传给其他调用者。

摘录保留原始 conversation ID、message ID、正文 SHA-256、时间与截断标记。已交付的共享用户要求还保留 delivery receipt。原始 JSONL 会话文件继续按原来的规则保留；压缩与 fork 不改写原文件。

`/compact` 的默认方式叫 `local_extractive`：选择已有文字形成摘录，而不是声称模型理解并总结过它。调用者自己提供摘要时，方式标为 `provided_summary`。摘要与来源标识一起保存；调用者应核对内容是否准确。

## Agent 使用

```javascript
const before = await context.status(clientId);
const snapshot = await context.read(clientId);

return await context.compact(clientId, {
  summary: "已完成输入框和队列；下一步验证文件粘贴及重载恢复。",
  expected_revision: before.revision
});
```

`expected_revision` 比较的是工作区顶层 revision。保存前已有其他调用者修改记忆时，整个修改会被拒绝；重新读取状态，再决定如何合并。它避免多个 agent 把彼此的新进展覆盖掉。

```javascript
const current = await context.status(clientId);
const branch = await context.fork(clientId, {
  name: "另一种实现",
  activate: true,
  expected_revision: current.revision
});
return branch;
```

完整原生接口：

| 原生工具 | JavaScript helper | 主要参数 |
| --- | --- | --- |
| `ssmcp.context.status` | `context.status(id)` | 无 |
| `ssmcp.context.read` | `context.read(id, args)` | 可选分支 `id` |
| `ssmcp.context.list` | `context.list(id)` | 无 |
| `ssmcp.context.compact` | `context.compact(id, args)` | 可选 `summary`、`expected_revision` |
| `ssmcp.context.fork` | `context.fork(id, args)` | 可选 `name`、`activate`、`expected_revision` |
| `ssmcp.context.switch` | `context.switch(id, args)` | 必填分支 `id`，可选 `expected_revision` |

原有的 `context.roots(id)` 继续返回工作区根目录。

## 自动恢复如何传给模型

成功授权并执行的原生工具结果会附带当前客户端的有界记忆快照。中转端收集本次编排触达的客户端，每个客户端保留最新 revision，再放进外层：

```text
structuredContent.context_memories
  [{ client_id, context: { workspace, branch, revision, mode, summary, recent, provenance } }]
```

响应另有 `SESSION CONTEXT` 文字块，便于模型直接阅读。即使 JavaScript 只返回一个简短结果，这份记忆仍能到达外层响应。普通 `result`、日志、图片和错误标记保持各自的含义。

记忆是历史参考。`structuredContent.user_messages` 才是本次新交付的用户要求；两者需要分别处理。不要因为看见历史摘要就重新执行已完成任务。工具目录查询、拒绝授权的动作或单纯 JavaScript 计算不会被当成一次新的记忆采集。

容量不足时，响应显式标记摘录或 deferred 状态；可再用 `context.read` 读取当前分支。读取 status、list、read 属于观测操作，默认不领取新要求。通过中转调用 compact、fork、switch 属于实际操作，按正常规则接收符合目标的新要求。

## 保存范围与容量

| 项目 | 上限或规则 |
| --- | --- |
| 工作区身份 | 启动目录的规范路径散列 |
| 分支 | 每个工作区最多 8 个 |
| 手写摘要 | 1–8192 UTF-8 bytes |
| 每个分支 | 24 KiB |
| 持久状态总量 | 256 KiB |
| 单客户端自动快照 | 小于 12 KiB，包含序列化后的字符转义 |
| 一次编排的自动记忆 | 最多 64 KiB；外层仍服从原有响应总量限制 |

容量用字节衡量，不冒充模型的 token 统计。超出快照预算时采用显式摘录，完整的有界分支仍可读取。

状态保存在 `$XDG_STATE_HOME/ssmcp/context-memory/<workspace-id>/state.json`；没有设置 XDG_STATE_HOME 时使用用户主目录的 `.local/state`。目录权限为 0700，状态文件为 0600；更新采用同目录临时文件、同步与原子替换，并检查符号链接、硬链接与工作区身份。不同工作区的记忆独立保存。

实现见 [context_memory.rs](../src/client/context_memory.rs)、[会话历史](../src/client/interface/history.rs) 和 [工具编排](../src/code_mode.rs)。真实会话恢复、分支隔离、pending 队列与图片草稿的验收入口是 [e2e_context_memory.py](../scripts/e2e_context_memory.py)。
