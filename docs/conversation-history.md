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

## Retention and validation

| Scope | Limit |
| --- | --- |
| Message input | 16 KiB of UTF-8 text |
| One conversation | 500 messages and 512 KiB of encoded JSONL |
| Managed histories | 64 files and 16 MiB in total |
| Native response page | 64 KiB; at most 64 requested records |

New messages remove the oldest retained messages if needed and increment `truncated_messages`. New or growing histories prune the oldest inactive conversations when the global budget is reached. Active conversations are protected by their leases. If all reclaimable space belongs to active clients, recording returns a clear capacity error.

Readers reject unsupported schema versions and incomplete files. Stored content passes through audit redaction again before display. Malformed, linked, or nonprivate files are not exposed or reclaimed as valid conversation histories. Raw exports remain ordinary JSONL files in the private state directory.
