# Events and bounded waiting

SSMCP keeps event metadata in a bounded journal so a coordinator can stop one
observation call and start the next without losing a worker update in the gap.
The CLI continues to own tasks, executions and user messages. Events describe
those operations; receiving an event does not execute or acknowledge them.

## Event contract

Every relay event includes `id`, `seq`, `client_id`, `kind`, `origin`,
`actor_id`, `time_ms` and `data`. Sequence numbers order one authenticated
account's relay journal. They do not establish a global clock across machines.
Native worker metadata also retains its source generation, sequence and time.

| Kind | Committed state represented |
| --- | --- |
| `task.submitted` | A worker task was accepted into its pool queue. |
| `worker.started` | A station acquired its verified task claim. |
| `worker.cancelling` | Cancellation was requested while a claimed task may still be running. |
| `worker.progress` | The pool accepted a progress or activity update. |
| `worker.completed` | The pool retained a terminal task outcome. Inspect the task status and result. |
| `worker.idle` | A registered station became available for another task. |
| `worker.unknown` | A task's outcome could not be established. No automatic replay follows. |
| `agent.joined`, `agent.left` | A collaboration membership began or ended. |
| `agent.status_changed` | An authenticated participant changed its declared status. |
| `wait.started`, `wait.suspended`, `wait.resumed`, `wait.completed` | An observer entered, left, or completed a bounded waiting episode. |
| `message.sent` | The message service accepted the message for its actual recipients. |
| `message.received` | A message was included in a receive result. This does not prove reading or execution. |
| `message.acknowledged` | The recipient explicitly confirmed the retained delivery cursor or message. |
| `user.input_available` | The real user-input queue contains an eligible message. Retrieve its text through the existing user-input delivery path. |
| `custom.*` | An authenticated agent published its own application event. Its origin remains `agent`. |

System producers run at the corresponding state change. A repeated successful
send, claim or completion request reuses its receipt and does not invent another
state transition. A query of current state is not evidence of an earlier start
or completion. Declared agent status is not proof that the process is working.

Message events contain routing and receipt metadata, never message bodies or
private credentials. Private audiences identify the full `client_id:agent_id`.
Having access to two clients does not make their participants interchangeable.
Pure account inboxes keep their existing native-message delivery semantics;
an agent subscription does not acquire all messages belonging to its account.

## Publish, subscribe, read and acknowledge

The public `events` tool and the `events` code-mode helper share these actions:

| Action | Purpose |
| --- | --- |
| `subscribe` | Create an immutable filter and return a server-issued subscription ID. |
| `read` | Read the next retained matching page without acknowledging it. |
| `wait` | Suspend until a matching page is available or the observation budget expires. |
| `ack` | Explicitly acknowledge a cursor that this subscription has already delivered. |
| `unsubscribe` | End this subscription without cancelling any underlying operation. |
| `subscriptions` | Inspect this participant's retained subscriptions. |
| `publish` | Publish a `custom.*` event with an idempotent request ID. |
| `policy` | Inspect or modify this participant's coordinator prompt strategy. |

Subscribe with a stable `request_id`. Repeating that ID with the same filter
recovers the same subscription; changing its meaning is an error. Omit `after`
to begin at the journal's current head, or provide a retained sequence to replay
from that point. Filters support exact kinds or a trailing `.*`, selected
client IDs, an exact actor ID and an exact task ID. Filter fields narrow a query;
they do not grant access.

The selected client and each source client must remain authorized. A source
participant can be supplied through that client's existing `collab.bind` or
through explicit `source_agents` credentials. A worker-pool source uses its own
verified participant, even when the same coordinator also belongs to the
control session. Do not put private tokens in examples, logs or event data.

Read and wait return two different cursor positions:

- `cursor` is the subscription's acknowledged position.
- `next_cursor` is the last sequence scanned for this delivered page.

After handling the page, pass `next_cursor` to `ack` as `through`. A failed or
lost response does not advance the acknowledged position. Reading again returns
the retained unacknowledged events. The server rejects a cursor beyond what
that subscription has delivered. Page limits and `has_more` allow bounded
draining without returning the entire journal.

## Observe in windows of at most 55 seconds

Long waits belong in the public `await` or `events` tools. They suspend on the
server without occupying a JavaScript runner. Code-mode helpers remain useful
for publication, subscription setup and short observations; a helper call does
not make its enclosing JavaScript runner disappear.

For example, create a subscription for the control session and its linked pool:

```javascript
await collab.bind(controlId, controlCredentials);
await collab.bind(poolId, poolCredentials);
const subscription = await events.subscribe(controlId, {
  request_id: "coordinator-work-events-1",
  filter: {
    client_ids: [controlId, poolId],
    kinds: [
      "user.input_available",
      "worker.progress",
      "worker.completed",
      "worker.idle",
      "worker.unknown"
    ]
  }
});
return subscription;
```

Then use the public `await` tool with a bounded window:

```json
{
  "client_id": "CONTROL_CLIENT_ID",
  "agent": {"id": "YOUR_AGENT_ID", "token": "YOUR_PRIVATE_TOKEN"},
  "targets": [{"type": "event", "subscription_id": "RETURNED_SUBSCRIPTION_ID", "limit": 32}],
  "mode": "any",
  "window_ms": 55000
}
```

When the response is pending, resume the returned `wait_id` in another outer
call. Keep its targets and mode. A ready window has a retained result cursor;
acknowledge that result with `after: 1` on its wait ID after handling it. This
acknowledgement is separate from the event subscription's `ack`. Neither one
acknowledges a collaboration inbox or consumes a user-input message.

`any` resolves when a requested target is ready. `all` retains completed target
results across windows until all requested targets resolve. Worker-result
targets can be combined in these modes. A worker-assignment target remains a
single-target operation because obtaining an assignment has a distinct claim
receipt. User-input delivery retains its separate transaction boundary.

Observation timeouts end only the current observation window. They do not
terminate an execution, reclaim a worker, resubmit a task or infer success.
Inspect the original task ID, claim or request receipt when an outcome is
unknown. A new business operation requires a new deliberate decision.

Waiting events carry `data.origin_wait_id`. Subscriptions exclude the same
participant's waiting lifecycle by default, and an await event target always
excludes its own wait ID. This prevents a coordinator from waking itself merely
because it announced that it had suspended.

## Retention and native sources

The relay journal has hard limits: 1,024 events and 1 MiB per account, at most
4 KiB per event, and reads of at most 64 events or 64 KiB. The retention window
is 15 minutes; hard capacity can evict entries sooner. An old or missing cursor
returns an explicit gap. The server does not pretend that the missing history
was delivered. Use current authoritative task and message state to recover.

Native worker events originate in the pool's own retained journal. The relay
reads `ssmcp.worker.events` with a verified source binding and resumes its native
generation/sequence cursor. That asynchronous bridge continues between outer
tool calls. Replies expose native-source health; unsupported clients, revoked
bindings, gaps and unavailable sources remain distinguishable. No native task
is replayed by this bridge.

Authorization is checked when subscribing, while observing and before returning
cached data. Disconnecting or replacing an original source connection invalidates
its subscriptions. Retained results are not a grant to read a revoked source.

## Coordinator prompt strategy

An authenticated main coordinator receives a bounded prompt hint when worker
capacity is available and when it should consider waiting. The instruction is
conditional: continue any independent useful work; when none remains, await
user-input and worker events. The hook does not invent an idle state or launch
another model.

Use `events.policy` to change `enabled`, `window_ms`, `kinds`, a concise
`strategy`, or an optional `dreaming` round for the current participant. The
default waiting window is 55 seconds. The generated subscription filter always
includes `user.input_available` unless `user.*` already covers it; the stored
`kinds` preference and explicit subscriptions are unchanged. Wait guidance is
suppressed when input for this participant or the next caller is queued or being
edited, or when the response already contains user messages. Observing this
state does not reserve, consume or acknowledge any input.

### One optional Dreaming round

Dreaming is disabled by default. An authorized coordinator can set a single
round through the public `events` tool with `action: "policy"` and this
`patch` on its bound control client:

~~~json
{
  "dreaming": {
    "round_id": "tools-round-1",
    "repository": "project",
    "budget_ms": 20000
  }
}
~~~

All three round fields are required. `round_id` contains 1–128 ASCII letters,
digits, underscores or hyphens; `repository` is `project` or `global`;
`budget_ms` is 1,000–55,000. An invalid patch preserves the previous policy.
Omitting `dreaming` preserves it, while `{"dreaming": null}` clears it.
The scope names identify a requested local layer; they do not register a source
or grant permission to edit it.

The existing response hook can return `coordination_hints[].dreaming_round`
when the selected round has not been offered in its current cache entry, no
relevant input is pending, the response is not an error, and the linked pool is
both authorized and observed fully idle. Busy, queued, offline, retiring,
stopping, disconnected or unavailable pool state suppresses the round. This
observation is not a reservation of capacity or proof that earlier uncertain
effects have stopped. The caller must still have no independent authorized
work left and must reconcile known task and job receipts before starting.

The last offered round ID is retained in the existing bounded hint cache.
Ordinary wait reminders, changing the same round's budget, and temporarily
disabling policy do not offer that round again. Policy entries expire after
15 inactive minutes; both the policy registry and hint cache hold at most
256 entries. Cache eviction or reconnect can expose a familiar round ID again.
Reuse the original task and job identities; a repeated hint, elapsed budget,
cancelled wait or `unknown` outcome never authorizes another execution.

The round and its time budget are advisory instructions for the external
coordinator. Setting the policy starts no timer, model, task submission or Git
write. Use short operations within the remaining budget, let new user input
preempt further steps, and retain the actual outcome and reviewable local
changes. Explicit event acknowledgement still does not deliver user text;
handle `user.input_available` through the existing input-delivery path.
See [the tool-maintenance workflow](tool-repositories.md#maintaining-workflows-with-dreaming).

All policy fields change guidance without changing task ownership, access
control or completion receipts. The hook shares its existing 750 ms probe
deadline and 4 KiB response-growth budget; a suppressed or oversized hint does
not spend the round's delivery opportunity.
[Policy validation](../src/http/coordination_policy.rs) ·
[Hint gating and cache](../src/http/worker_reminders.rs)
