# Live conversation events

ssmcp keeps conversation messages and events in bounded process memory. A network disconnect and reconnect within the same process preserves retained content. Exiting, restarting, or `/reload` clears it. The client does not create conversation JSONL, archive indexes, or context-memory checkpoints automatically; old files are neither read as runtime state nor deleted by this migration.

Use `ssmcp.ui.events` to read the current event window. Local `/resume`, `/archive`, `/archived`, and `/unarchive`, along with `ssmcp.ui.history`, `ssmcp.ui.history_archive`, and `ssmcp.ui.history_update`, are retired. Explicit `/handoff-export` export remains available below.

## Message receipts and ownership

`ssmcp.ui.message` and `ssmcp.ui.present` return `recorded: true` and `storage: "memory"` when their message has been recorded in memory. `message_id` identifies that message; its event carries the same identifier in `value.message_id`. The event's separate `id` and `sequence` identify its place in the process journal. `shown` reports whether the message was handed to the local display, not whether the user read it.

Ownership comes from the authenticated account, client connection, and verified participant at the actual call. Display names, model names, and identity fields embedded in event JSON do not grant access or change that source. A thread query includes only events attributed to that exact participant. The process view may also include retained events without a verified thread attribution; those events are not assigned to a participant by guessing its name.

The existing message idempotency rules still apply within the retained receipt window. A matching retry returns the original message ID and receipt with `duplicate: true`; conflicting content is rejected. In-process reconnect preserves this memory, while process restart clears it. Recording a message does not replay a tool, submit a user request, or recreate an external model conversation.

## Reading the current event window

The native entry point is `ssmcp.ui.events` with `action: "thread"`. Select an authorized client from the live client list, then use its exact ID in both the tool target and the arguments.

| Argument | Meaning |
| --- | --- |
| `action` | Required; use `"thread"`. |
| `client_id` | Required actual client ID: 64 lowercase hexadecimal characters. |
| `agent_id` | Optional exact participant ID, `agent_` followed by 32 lowercase hexadecimal characters. |
| `main` | Set `true` for authenticated calls through the main entry without a bound participant. It cannot be combined with `agent_id`. |
| `cursor` | Optional `u64` event sequence cursor. Omit it for the first page. |
| `generation` | Process-journal generation returned by the previous page; required with a nonzero cursor. |
| `limit` | 1–64 events; default 16. The response also has a 64 KiB page budget. |

Omitting both `agent_id` and `main` selects the process aggregate. A participant named or assigned the role “main” still has a participant ID; use that `agent_id` to read its events. The `main: true` filter identifies the authenticated unbound entry, not a display name or collaboration role.

Replace the placeholder with the actual ID returned by `list`:

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
const response = await call(clientId, "ssmcp.ui.events", {
  action: "thread",
  client_id: clientId,
  limit: 16
});
if (response.isError) return response;
return response.structuredContent;
```

For a specific participant, add its actual `agent_id`; for the unbound main entry, add `main: true`. Keep that selection unchanged while continuing a page sequence:

```javascript
const selection = {
  action: "thread",
  client_id: clientId,
  agent_id: agentId,
  limit: 16
};
const firstResponse = await call(clientId, "ssmcp.ui.events", selection);
if (firstResponse.isError) return firstResponse;
const firstPage = firstResponse.structuredContent;
return await call(clientId, "ssmcp.ui.events", {
  ...selection,
  cursor: firstPage.next_cursor,
  generation: firstPage.generation
});
```

`clientId` and `agentId` are the actual client and verified participant selected for this sequence. The next read uses the first native result's `structuredContent` and the same selection. A native MCP error remains an error; it is not an empty event page.

## Pagination and retention

The event window retains at most 4096 events within a 2 MiB process budget. Other live views and tool-output buffers have their own limits. Reaching a retention limit can remove earlier events while the process continues running.

| Response field | Meaning |
| --- | --- |
| `schema_version` | Current projection schema, `1`. |
| `scope` | The selected process, main entry, or exact thread, with authenticated-source provenance. |
| `generation` | Identity of the current process journal. |
| `events` | Retained event projections for this page. |
| `next_cursor` | Cursor to use for the next read with the same generation and selection. |
| `start_cursor` | Earliest sequence still retained by the process journal. |
| `has_more` | More process events remain after this page's cursor. |
| `truncated_since_cursor` / `retention_gap_possible` | The requested cursor predates the retained process window. |
| `truncation_scope` | `"process_journal_window"`; a gap does not prove which selected thread lost events. |
| `retention` | `"live_process_memory"`. |

Cursors advance over unmatched events as well as returned events. Use `next_cursor` even when `events` is empty; deriving a cursor from the number of matching records would reread or skip the wrong positions. `has_more: false` means the read has reached the current end, and later activity may add events.

A cursor beyond the current process sequence is rejected. A nonzero cursor without its generation is rejected, and a cursor from an earlier process generation cannot continue in the new process. After a restart, start a new read without the old cursor or generation and treat the previous window as lost.

Each event contains its sequence, bounded text, authenticated source, and retained event fields in `value`. Some events also expose status, phase, exit code, job ID, or command provenance. Check `truncated` and `text_lossy`; a bounded event projection is not a complete tool-output capture. Use the task or output interfaces for bytes still retained by those buffers.

[Source attribution and pagination](../src/session/attach/thread_history.rs) · [Process event window](../src/session/attach.rs) · [Client UI](client-ui.md)

## /handoff：生成可直接粘贴的交接提示词

在运行客户端的前台终端选择当前 Agent（或 Next caller），输入 `/handoff [附加说明]` 并按 Enter。Agent 根据当前任务生成一份独立的交接提示词，调用 `ssmcp.ui.handoff` 将它复制到运行此客户端的系统剪贴板。终端确认复制成功后，就可以粘贴给下一位 Agent。剪贴板不可用时，Agent 会收到错误，并可在会话中提供原文以供手动复制。每次请求的复制权限有效 30 分钟，成功写入后即失效。

## /handoff-export：准备可编辑的交接上下文

`/handoff-export` 将当前进程仍保留的会话、尚未送达、仍在编辑或等待确认的本地要求队列，以及未回答的问题显式导出为一个 Markdown 文件。`/handoff-export 附加说明` 会同时保留用户输入的说明，例如：

```text
/handoff-export 请先检查输入恢复的边界，再继续处理尚未回答的问题。
```

完成后，输入框出现 `@".ssmcp/inputs/handoff-随机ID.md"`。目标 Agent 保持不变；用户可继续编辑和追加要求，再决定是否按 Enter 发送。导出不发送请求、不改变队列送达状态、不回答问题、不调用模型，也不执行或重放旧工具。它按当前记录生成上下文，不推断项目目标或生成模型摘要。

文件读取当前进程已有的内容，不扫描旧会话或记忆文件。待发要求和未回答问题单独列出；已送达、已取消的队列记录不再作为待办列入。尚未提交的普通草稿和 Agent 凭据不导出。用户文本经过现有 audit 脱敏；生成的标签使用产品当前的 English 默认界面，中文正文和说明保持 UTF-8。

| 导出范围 | 上限 |
| --- | --- |
| 整份 Markdown | 2 MiB |
| 附加说明 | 16 KiB UTF-8 |
| 单条文本预览 | 16 KiB，按 UTF-8 边界裁剪 |
| 当前会话 | 最多 500 条，768 KiB 展示预算；优先保留最近记录 |
| 待发要求 | 最多 128 条，768 KiB 展示预算 |
| 未回答问题 | 最多 32 条，320 KiB 展示预算 |

导出标明单条截断、因导出预算省略的记录，以及源内容已有的裁剪。导出预算独立于会话、队列和内存输出的保留规则；文件引用仍指向原工作区，附件内容不会被复制到交接文档。已经从内存淘汰的内容无法由导出补回。

各数据源分别读取和释放锁，因此文件是生成时的近似快照，不是跨数据源的原子事务；后续新消息不会自动更新已经生成的文件。保存复用 `.ssmcp/inputs` 私有写入路径：Unix 目录 0700、文件 0600，拒绝符号链接和不安全父目录，文件保留到用户手动管理。私有输入文件导出当前需要 Unix；未支持的平台返回错误并保留草稿。生成失败不会自动发送残缺上下文。

实现与回归入口：[handoff 模块](../src/client/handoff.rs)、[共享私有输入写入](../src/client/references/paste.rs)、[真实 PTY 输入场景](../scripts/e2e_input_history.py)。这些入口的存在不代表某一构建已通过验收，实际结果以对应二进制和运行报告为准。
