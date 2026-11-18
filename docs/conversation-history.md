# Conversation history format

ssmcp saves the local conversation as versioned JSON Lines. Each line is one complete JSON object, and the file ends with a newline. The format identifier is `ssmcp.conversation.v1`.

A resumed conversation restores readable messages and changes the destination for future records. It does not start a model, recreate an external harness session, resend earlier user requests, or restore collaboration credentials.

## Location and ownership

On Linux and macOS, files live under `$XDG_STATE_HOME/ssmcp/conversations`, or `~/.local/state/ssmcp/conversations` when that variable is absent. On Windows, they live under `%LOCALAPPDATA%/ssmcp/conversations`.

The directory is private to the current user. Unix directories use mode 0700 and files use mode 0600; Windows uses the existing private-directory ACL implementation. Symbolic links, hard-linked files, FIFOs, nonprivate files, and invalid identifiers are rejected. Writes use a private temporary file, flush it, and atomically replace the destination.

A conversation starts when the process first records a message. Its workspace is the canonical startup working directory. Listing, reading, and resuming are scoped to that workspace. Different client processes start separate conversations. A process holds a lease on its active conversation: another process can read it but cannot resume or prune it until the first process exits or selects another conversation.

## Records

The first line is session metadata:

```json
{"type":"session","schema":"ssmcp.conversation.v1","schema_version":1,"id":"conversation_0123456789abcdef0123456789abcdef","workspace":"/home/user/project","created_at_ms":1794988800000,"truncated_messages":0}
```

All later lines are messages with string content:

```json
{"type":"message","id":"message_0123456789abcdef0123456789abcdef","role":"user","content":"Inspect @src/main.rs","kind":"requirement","created_at_ms":1794988800100,"truncated":false}
{"type":"message","id":"message_abcdef0123456789abcdef0123456789","role":"assistant","content":"## Progress\n\nThe first check passed.","kind":"agent_message","created_at_ms":1794988800200,"truncated":false}
```

Timestamps are Unix milliseconds. IDs consist of a fixed prefix and 32 lowercase hexadecimal characters. The file name is the session ID followed by `.jsonl`.

| Field | Meaning |
| --- | --- |
| `role` | One of `system`, `user`, `assistant`, or `tool`. |
| `content` | The original message text after the existing audit redaction pass. Markdown is preserved as text. |
| `kind` | Distinguishes `requirement`, `answer`, `agent_message`, `system_message`, and `tool_message`. A locally entered answer uses role `user`. |
| `created_at_ms` | Time when this message was recorded. |
| `truncated` | True if redaction expanded the content beyond the per-message limit and the saved text was shortened at a UTF-8 boundary. |
| `truncated_messages` | Session-level count of older messages removed to meet retention limits. |

Acknowledged user requests are recorded at the transport confirmation boundary, including clients without a terminal frontend. Their existing message IDs are derived from the request identity and original text, so a repeated acknowledgement does not append the same request again in the current or resumed history; an edited text remains a distinct record.

An identified `ssmcp.ui.message` reply returns the same `message_id` used by its saved JSONL entry when `history_saved` is true. Its display category (`progress` or `answer`) does not change the JSONL role `assistant` or kind `agent_message`.

The internal `record` entry point accepts `agent` as an alias for `assistant` and `answer` as a user answer. Those aliases are never written as role values. No agent tokens, pending-delivery state, or recipient credentials are part of this schema.

This is an openly documented ssmcp format. The use of conventional roles does not imply compatibility with another harness's private session format.

## Reading and resuming

The local `/resume` view chooses a past conversation. Its UI displays the stored text without putting those records back into the request queue.

The read-only native tool exposes metadata and bounded pages:

```javascript
await call(clientId, "ssmcp.ui.history", {action: "list", limit: 16});
await call(clientId, "ssmcp.ui.history", {
  action: "read",
  id: "conversation_0123456789abcdef0123456789abcdef",
  offset: 0,
  limit: 16
});
```

Follow `next_offset` until it is null. List responses contain session summaries; read responses contain session metadata and `messages`. Offsets count records rather than bytes. Native reads do not select a conversation or write a user answer.

After connecting, agents should use `ssmcp.ui.message` for local progress and result messages, `ssmcp.ui.ask` to queue questions, and `ssmcp.ui.questions` to read answers later. This is the local communication convention; ssmcp does not prevent an external harness from writing somewhere else.

## /handoff：准备可编辑的交接上下文

`/handoff` 将**当前** JSONL 会话、尚未送达、仍在编辑或等待确认的本地要求队列和未回答的问题导出为一个 Markdown 文件。`/handoff 附加说明` 会同时保留用户输入的说明，例如：

```text
/handoff 请先检查输入恢复的边界，再继续处理尚未回答的问题。
```

完成后，输入框出现 `@".ssmcp/inputs/handoff-随机ID.md"`。目标 Agent 保持不变；用户可继续编辑和追加要求，再决定是否按 Enter 发送。导出不发送请求、不改变队列送达状态、不回答问题、不调用模型、不执行或重放旧工具。它按现有记录生成上下文，不推断项目目标，也不生成模型摘要。

文件读取当前会话的最新已保存内容，不扫描或自动恢复别的历史。待发要求和未回答问题单独列出；已送达、已取消的队列记录不再作为待办列入。尚未提交的普通草稿和 Agent 凭据不导出。用户文本经过现有 audit 脱敏；生成的标签使用产品当前的 English 默认界面，中文正文和说明保持 UTF-8。

| 导出范围 | 上限 |
| --- | --- |
| 整份 Markdown | 2 MiB |
| 附加说明 | 16 KiB UTF-8 |
| 单条文本预览 | 16 KiB，按 UTF-8 边界裁剪 |
| 当前历史 | 最多 500 条，768 KiB 展示预算；优先保留最近记录 |
| 待发要求 | 最多 128 条，768 KiB 展示预算 |
| 未回答问题 | 最多 32 条，320 KiB 展示预算 |

导出明确标注单条截断、因导出预算省略的记录，以及 JSONL 自身的 `truncated_messages`。这些限制独立于原历史、队列和输出归档的保留规则；文件引用仍指向原工作区，附件内容不会被复制到交接文档。

各数据源分别读取和释放锁，因此是生成时的近似快照，不是跨存储的原子事务；后续新消息不会自动更新已经生成的文件。保存复用 `.ssmcp/inputs` 私有写入路径：Unix 目录 0700、文件 0600，拒绝符号链接和不安全父目录，文件保留到用户手动管理。自动私有输入保存当前需要 Unix；未支持的平台返回错误并保留草稿。生成失败不会自动发送残缺上下文。

实现与回归：[handoff 模块](../src/client/handoff.rs)、[共享私有输入写入](../src/client/references/paste.rs)、[真实 PTY 输入场景](../scripts/e2e_input_history.py)。

## Retention and validation

| Scope | Limit |
| --- | --- |
| Message input | 16 KiB of UTF-8 text |
| One conversation | 500 messages and 512 KiB of encoded JSONL |
| Managed histories | 64 files and 16 MiB in total |
| Native response page | 64 KiB; at most 64 requested records |

New messages remove the oldest retained messages if needed and increment `truncated_messages`. New or growing histories prune the oldest inactive conversations when the global budget is reached. Active conversations are protected by their leases. If all reclaimable space belongs to active clients, recording returns a clear capacity error.

Readers reject unsupported schema versions and incomplete files. Stored content passes through audit redaction again before display. Malformed, linked, or nonprivate files are not exposed or reclaimed as valid conversation histories. Raw exports remain ordinary JSONL files in the private state directory.
