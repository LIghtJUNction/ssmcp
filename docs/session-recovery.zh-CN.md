# 主动读取当前会话的历史与工作位置

新对话连接到仍在运行的客户端后，Agent 可以主动调用 `history.recover(clientId)`。它返回一份有界索引：当前本地对话的最近记录、已有检查点、当前或最近的原生命令状态，以及继续读取审计和输出的引用。索引不启动模型，不恢复旧命令，不切换对话，也不把历史要求重新放进用户队列。

## 从哪里继续

先从已授权的在线客户端列表选定准确的 `clientId`，再调用：

```javascript
const state = await history.recover(clientId, {
  recent_limit: 8,
  job_limit: 8,
});
print(state);
```

对应原生工具是 `ssmcp.client.recover`。旧 relay 没有该 JavaScript helper 时，可以先从客户端工具目录确认原生工具存在，再使用 `call(clientId, 'ssmcp.client.recover', {})` 或动态原生 API。旧客户端没有此工具时会明确报不支持，不猜测历史文件位置。

| 返回部分 | 来源与含义 |
| --- | --- |
| `identity` | 当前原生进程的启动代次、PID、Runtime 会话代次与关闭状态。 |
| `history` | 当前进程正在记录的 JSONL 对话，包含真实对话 ID、消息 ID、原文哈希、记录下标和短预览。 |
| `checkpoint` | 当前工作区已保存的 context 分支、revision、摘要和来源 ID。检查点可能引用同工作区的其他对话；来源不会被改成当前对话。 |
| `jobs` | 此原生命令管理器保留的任务状态；正在运行的任务排在前面，其后按开始时间由近到远排列。只返回元数据和输出引用，不复制命令正文或输出内容。 |
| `audit` | 当前 Runtime 的真实审计游标边界及已有归档引用，不复制事件正文。 |

`history` 和 `checkpoint` 各自带 `available`。存储损坏、未初始化或工作区不一致时，只把该部分标为不可用并给出有界错误，其他来源仍可读取。没有当前对话时，`history.value.active` 为 `false`；此时可以调用 `history.list(clientId)`，不会自动把最近的另一段对话当作当前对话。

这些是分别读取的快照，不是整个进程的全局事务。任务可能刚结束，历史可能继续追加。返回状态用于定位原始记录，不推断项目已经完成、用户已经阅读或模型正在思考什么。

## 按来源继续读取

最近记录的预览最多 512 字节，保留 UTF-8 边界。需要全文时，按返回的对话 ID 和消息下标读取：

```javascript
const state = await history.recover(clientId);
if (state.history.available && state.history.value.active) {
  const index = state.history.value;
  const recent = index.recent.at(-1);
  if (recent) {
    const page = await history.read(clientId, {
      id: index.conversation_id,
      offset: recent.message_index,
      limit: 1,
    });
    // 保留策略可能移动下标；确认 page.messages[0].id 与 recent.message_id 一致。
    print(page);
  }
}
```

完整历史页沿用 `next_offset` 分页，每页最多 64 KiB；可以用 `history.search` 按关键词定位原文。`truncated_messages` 表示此前已按保留限制移除的旧记录，`preview_truncated` 表示本次短预览被截短，两者含义不同。哈希对应已保存、已脱敏的原始消息文本，便于与检查点来源核对，不证明摘要判断正确。

完整检查点继续用 `context.read(clientId)`。命令详情和正文继续用 `exec.status`、`exec.read`；归档按真实 artifact ID 调用 `outputs.read`。审计读取使用索引给出的起始游标，并把 `snapshot_end_cursor` 作为此次追溯的终点，避免把持续新增事件或读取操作自身无限追下去。历史和输出仍遵守各自的保留、授权及完整性检查。

## 不同 ID 的边界

`client_id` 是当前 relay 连接；`session_id` 是原生 Runtime 代次；`process_id` 是进程启动代次；`conversation_id` 是持久 JSONL 文件的身份。它们不是同一个 ID。重新连接或重启后，应重新发现在线客户端和 Runtime 身份，不能按相同目录、相同显示名或复用的 PID 推定原会话还活着。

新建外部 AI 对话不会重建仍在线客户端的本地历史。原生进程重启后，当前对话可能为空，而工作区检查点和仍在保留期内的历史文件仍可主动读取。任务列表只描述当前命令管理器保留的真实任务，不把历史聊天、PTY 标签页或外部模型线程伪装成活任务。

独立、未绑定 Agent 的 recovery 调用按观察处理，不会仅因读取历史而领取 Next caller 要求。已有明确 Agent 绑定所允许的新用户要求投递规则保持原样；历史文本本身始终作为来源数据返回。

## 存储与读取预算

| 存储或返回 | 当前边界 |
| --- | --- |
| Recovery 结构化索引 | 最多 32 KiB；普通 MCP 文本镜像和已有 context 侧元数据另有封装开销。 |
| 最近历史索引 | 最多 16 条，每条预览 512 字节，整体最多 12 KiB。默认 8 条，设置 0 只取身份和位置。 |
| 任务索引 | 最多 16 条，默认 8 条；返回 `total`、`shown` 和 `truncated`，可进一步调用原生任务列表。 |
| 对话 JSONL | 每条最多 16 KiB，每段最多 500 条或 512 KiB；有效历史合计最多 64 段或 16 MiB。活会话租约受保护，满额时明确拒写。 |
| Context 检查点 | 每工作区最多 8 个分支、256 KiB；摘要最多 8 KiB。新建工作区目录总数限 256，准入在跨进程文件锁内检查。 |
| 命令状态 | 当前命令管理器最多 32 个运行任务、128 个保留任务；输出继续使用现有独立保留策略。 |
| 输出归档 | 既有默认 24 小时、512 MiB 总预算、单归档 256 MiB；实际配置仍以客户端能力和返回元数据为准。 |

对话默认位于用户状态目录的 `ssmcp/conversations`；检查点位于同级 `ssmcp/context-memory/<workspace-hash>/state.json`。本接口不新增历史副本，不复制附件，也不额外持久化 task 或 audit 正文。

检查点目录达到 256 个时，现有工作区仍可读取和在自身预算内更新，新工作区会得到明确的容量错误。此前已经超出目录数量的内容也保留；该限制不会自动删除旧记录。需要腾出目录名额时，应由用户明确归档不再使用的工作区，再移走对应目录。对新准入的工作区，256 × 256 KiB 对应最多 64 MiB 的检查点正文预算，文件系统目录等开销另计。

已有保存会话服务另外保留会话配置、执行映像和生命周期日志。生命周期日志按 256 KiB 阈值轮转并保留当前及上一份，单条状态文档另受 128 KiB 限制。保存会话目录的创建总数在本阶段没有新增准入限制；本接口不更改服务配置、删除映像或终止会话。该层的容量治理需与会话服务自身的持久化合同协调。

相关文档：[JSONL 历史](conversation-history.md)、[本地检查点](context-memory.md)、[会话候选消息](session-messages.md)。
