# Cross-session candidate messages

`messages` is a native relay tool. The same operations are available as the frozen `messages.*` namespace in `code_mode`. They exchange bounded text candidates between currently authorized, live clients. They do not start a model, run a command, or put a peer message into the user's steering queue.

## Identify the destination

Start with `await messages.list()`. Follow `next_cursor` with `messages.list({cursor})` when present. A page contains at most 32 sessions. `messages.list({client_id, include_threads:true})` also lists that connection's currently registered collaboration participants.

| Field | Meaning |
|---|---|
| `target.client_id` | Exact live relay connection; reconnecting creates another ID. |
| `target.session_id` | Native Runtime inbox generation; old generations cannot receive new messages. |
| `process_id` | Random native process startup generation. Multiple Runtime sessions in the same process share it. PID reuse cannot select a restarted process. |
| `pid` | Native process information for inspection; not a routing credential. |
| `target.thread_id` | Optional registered collaboration participant ID. This is an explicit participant, not an OS thread or a hidden model thread. |

A PTY tab, saved conversation, display name, working directory or historical task does not create a live messaging session. Display names can repeat. The relay checks authorization using the existing account and session ACL, never names or controller labels.

The list is a current snapshot. Every actual dispatch and completed read checks access again. Old clients without the `peer_messages` capability are reported as unsupported; the relay sends them no private messaging operation. The existing wire version and login protocol are unchanged.

## Choose one session or a group

`send.to` accepts exactly one of these selectors:

| Selector | Recipients |
|---|---|
| `{scope:'session', ...target}` | One exact Runtime session, optionally one registered participant. |
| `{scope:'targets', targets}` | An explicit set of exact targets; duplicates are removed. |
| `{scope:'process', process_id}` | Shared candidate inboxes of the currently authorized Runtime sessions in that native process generation. |
| `{scope:'global'}` | Shared candidate inboxes in one snapshot of all currently authorized live clients, including the sender if it is in that snapshot. |

A batch contains at most 128 recipients. Process and global selectors do not subscribe to future sessions and do not fan out to every participant inbox. Use explicit participant targets when that is intended. Failed capability discovery produces an unavailable result and never guesses a recipient generation.

## Send and inspect the delivery receipt

Obtain a key immediately before an intentional new send. Keep that key when checking an uncertain outcome.

```javascript
async function sendTo(target, text) {
  const key = await messages.key();
  return await messages.send({
    to: {scope: 'session', ...target},
    text,
    idempotency_key: key.idempotency_key,
    topic: 'handoff',
  });
}

async function sendGroup(targets, text) {
  const key = await messages.key();
  return await messages.send({
    to: {scope: 'targets', targets},
    text,
    idempotency_key: key.idempotency_key,
  });
}
```

The relay creates the message ID, per-target delivery IDs and source envelope. Source contains the authenticated account or connection, the immutable `code_mode` run ID when present, and a verified participant binding when available. Caller-reported controller names are separately labeled display information. No request can supply `source`, `role:'user'`, `role:'system'`, or a private `Deliver` envelope.

When a run has one collaboration binding, it is captured automatically. When there are several, select the source with `from_client_id`; that client must already have the run's verified binding. A direct tool caller can supply `agent:{client_id,id,token}`. Credentials are checked against the current participant registry and are not included in candidates or delivery receipts.

The result includes one immutable recipient snapshot and individual results. `accepted` means a native candidate-storage acknowledgement was received. It does not mean a human read the message or that an agent executed it. `rejected` means the native inbox explicitly refused storage, for example because its byte budget is full. `unknown` means delivery may have happened without a usable acknowledgement. Neither cancellation nor an unknown result causes a retry.

`messages.status({message_id})` reads the sender's retained batch receipt. Add `refresh:true` to query those same recipients' current candidate states. Refresh never sends the text again. A recipient may report `candidate`, `acknowledged`, `expired`, or `unknown` after its receipt is no longer retained.

Keys are valid for five minutes and one relay startup generation. A duplicate key with matching content and selector returns the original batch, including its original source run. Changing the content or selector is rejected. Valid keys are never evicted to make room for another batch. Expired keys and keys from a restarted relay are rejected, so a later retry cannot silently expand an old global broadcast. A new key always means a deliberate new send.

## Read, wait, then explicitly acknowledge

`read` and `receive` are equivalent, non-consuming reads. They accept `target`, `after` (default 0), and `limit` (1–32). Use the returned `next_cursor` for the next page. Keep the previous cursor when rereading unhandled candidates. `has_more` indicates another bounded page; `truncated` warns that earlier candidates have been retired.

```javascript
async function checkReplies(target, cursor, messageId) {
  const page = await messages.wait({
    target, after: cursor, wait_ms: 1000,
  });
  return {
    ...page,
    matching: page.messages.filter(
      candidate => candidate.message.reply_to === messageId
    ),
  };
}
```

`wait` waits for a candidate after the cursor, up to 30 seconds, and returns `timed_out` when the deadline expires. There are at most four native waiters per Runtime session. Set the enclosing `code_mode.timeout_ms` above the requested wait plus dispatch time. Canceling orchestration does not consume candidates; an already dispatched native wait can remain until its bounded deadline or the connection closes. Reconnection closes the old inbox instead of moving its messages to another Runtime.

Only call `messages.ack({target,ids})` after deciding to handle those exact delivery IDs. Alternatively, `through` acknowledges that mailbox's candidates through an observed cursor; `ids` and `through` are mutually exclusive. Reading a participant mailbox or acknowledging it requires the matching current `collab.bind` in the run, or its private `agent` credential. Knowing an agent ID is insufficient. Shared-session inboxes use the session's existing control ACL. The typed local frontend facade can inspect the physical connection's candidates without publishing them as user instructions. Its inbox presentation is separate from the user's steering queue.

## Bounds and lifetime

| Resource | Bound |
|---|---|
| Text | 16 KiB UTF-8 per message; terminal controls are rejected except newline and tab. |
| Native candidate inbox | 128 candidates and 256 KiB of accounted serialized data and metadata. |
| Read page | 32 candidates and 64 KiB. |
| Candidate TTL | Default 10 minutes; caller may select 1 ms to one hour. |
| Native receipt retention | Up to 128 receipts; active candidates retain their deduplication proof. |
| Relay batch receipts | 256 batches globally, 64 per authenticated principal, eight active sends. |
| Relay receipt reservation | 8 MiB; includes source and bounded per-recipient metadata. No text body is copied into this cache. |
| Receipt/key window | Five minutes; expired or restarted-relay keys cannot replay. |
| Batch fan-out | At most 128 targets and four concurrent native requests per batch. |

Candidate payloads are removed on explicit acknowledgement, expiry or Runtime shutdown. Expiry uses one task while an inbox contains candidates; an unused inbox has no periodic timer. Relay receipts contain bounded source, destination and acknowledgement metadata, not copies of conversation history or output files. This API provides live coordination, not durable conversation history.

The direct MCP tool uses the same fields with an additional `action` key: `key`, `list`, `send`, `read`, `receive`, `wait`, `ack`, or `status`. There is no model invocation or automatic command replay in either layer.
