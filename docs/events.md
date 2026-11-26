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
| `worker.offline` | A station actually went offline or acknowledged its retirement release. |
| `worker.retiring` | An idle registered station entered cooperative retirement. |
| `worker.unknown` | A task's outcome could not be established. No automatic replay follows. |
| `agent.joined`, `agent.left` | A collaboration membership began or ended. |
| `agent.status_changed` | A real lifecycle transition occurred through an authenticated update, work, input, worker event or deadline. |
| `agent.maintenance_requested` | A bounded maintenance request was privately addressed to its exact eligible member. |
| `wait.started`, `wait.suspended`, `wait.resumed`, `wait.completed` | An observer entered, left, or completed a bounded waiting episode. |
| `message.sent` | The message service accepted the message for its actual recipients. |
| `message.received` | A message was included in a receive result. This does not prove reading or execution. |
| `message.acknowledged` | The recipient explicitly confirmed the retained delivery cursor or message. |
| `user.input_available` | The real user-input queue contains an eligible message. Retrieve its text through the existing user-input delivery path. |
| `repo.upstream_changed` | The checkout observed a change in local and upstream commit counts. |
| `repo.pull_request_opened` | The tracked remote advertised a new numbered pull-request head. |
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
| `subscribe` | Create a manual subscription with an immutable filter and return its server-issued ID. |
| `read` | Read the next retained matching page without acknowledging it. |
| `wait` | Suspend until a matching page is available or the observation budget expires. |
| `ack` | Explicitly acknowledge a cursor that this subscription has already delivered. |
| `unsubscribe` | End this subscription without cancelling any underlying operation. |
| `subscriptions` | Inspect retained subscriptions, their `automatic`, `configured` or `manual` management, and managed status. |
| `publish` | Publish a `custom.*` event with an idempotent request ID. |
| `policy` | Inspect or configure the main participant's actual managed `enabled`/`kinds` selection and its separate advisory prompt strategy. |

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

## Watch a remote repository

Subscribe to `repo.*` on the connected repository client. For example, this
manual subscription receives observations made after registration:

```javascript
const sub = await events.subscribe(clientId, {
  request_id: "watch-workspace-repo",
  filter: { kinds: ["repo.*"], client_ids: [clientId] },
});
```

Use the returned subscription ID with `events.wait` or `events.read`, and
acknowledge the page after processing it. Consecutive progress updates for the
same task and worker appear as a counted summary. The structured page keeps
the individual events and its original acknowledgement cursor.

The relay delivers repository
observations to authorized subscriptions on connections that negotiated
`repo_events_v1`. A coordinator's default managed subscription also includes
both kinds.

An upstream event identifies the local `branch` and its configured `upstream`.
`ahead` counts commits unique to the local branch. `behind` counts commits
unique to the upstream. For example, `ahead: 2, behind: 5` means both branches
have new work. The next observation after synchronizing can report zero for
both counts. `previous` holds the prior counts for that tracking relationship
or null for an initial divergence. A subscription opened after that initial
observation receives subsequent changes. A failed status read does not change the
counts. Git compares the local remote-tracking ref, refreshed by the normal
background fetch or a manual fetch.

The PR observer reads numbered `refs/pull/<number>/head` from the current
branch's configured remote. Its first successful listing establishes a
baseline. Later, newly observed higher numbers produce one event each, with
the remote name and PR number. Updates to an existing PR's head produce no
additional opening event. This works on hosts that advertise these refs and
does not require a hosting-service API token. The ref listing provides no PR
title, review decision, or open/closed status.

Periodic remote checks use `[git].fetch_interval_seconds` (300 seconds by
default). Setting the interval to zero disables background network checks.
Local comparisons still reflect changes to the tracking ref after a manual
fetch. A reconnect establishes a new PR baseline.

### Observe the upstream commit ID

For automation that tracks individual commits, subscribe to
`git.tracking_changed` on a connection that negotiated `git_tracking_v1`.
For example, a force-push can replace one upstream commit with another while
the local branch stays one commit behind. The event identifies both upstream
commit IDs, so a subscriber can detect that change even when the counts match.

The first sample for a branch and upstream establishes a baseline. Later
samples report a change in the upstream commit ID or in the ahead/behind
counts. Each event contains the short local `branch`, full `upstream` ref,
local `head`, `previous_upstream_oid`, `upstream_oid`, `ahead`, and `behind`.
The existing Git status loop samples the local remote-tracking ref. A manual
or automatic fetch makes remote changes visible to that loop. A branch or
upstream switch establishes a fresh baseline.

## Automatic worker subscriptions

A chat-enabled participant with a verified `main`, `coordinator`, `controller`
or `lead` role gets one managed subscription when the server can verify its
native `/worker_pool` link and an independently authenticated participant in that
pool. Both bindings use private credentials from the existing memberships:

```javascript
await collab.bind(poolId, poolCredentials);
await collab.bind(controlId, controlCredentials);
return await events.subscriptions(controlId);
```

A display name does not select a role or grant pool access. If the pool binding
is missing, `worker_subscription.error` explains that it needs binding; the
server does not join the pool, attach a manager or create credentials. Repeating
a bind reuses the managed subscription and its cursor. Link discovery has one
750 ms total budget across at most four already-bound main participants.

Defaults select `task.submitted`, `worker.started`, `worker.progress`,
`worker.completed`, `worker.idle`, `worker.offline`, `worker.retiring`,
`worker.cancelling` and `worker.unknown`, plus `user.input_available` and the
main's privately addressed `agent.maintenance_requested`. Main-session input
and maintenance metadata already originate in the server journal. The separate
pool observer reads native `ssmcp.worker.events`; it never claims work.

Configure the actual managed selection through the existing helper:

```javascript
return await events.policy(controlId, {
  patch: {
    enabled: true,
    kinds: ["worker.completed", "worker.offline", "worker.unknown"]
  }
});
```

These choices replace the default kinds exactly. A partial patch preserves
unmentioned managed fields. Responses expose the effective `enabled`, `kinds`,
configuration revision, source pool and subscription ID. Live managed choices
last for the original main Session, independently of the advisory policy cache.
Changing kinds preserves the acknowledged cursor and invalidates the earlier
attempted-delivery ceiling; read the new selection before acknowledging it.
Manual subscriptions keep their own filters.

A matching, authorized native worker event enters the retained journal before
it can change the existing main member to `working`. A retiring member is not
reactivated. An existing `await` read window can wake with
`wake_reason: "subscription_event"`, the subscription ID, and a bounded
`worker_events` notice. If its named targets remain unfinished, the response is
still `pending` with `continue_wait: true`; resume the same `wait_id`. For
durable waits, the original detached observer keeps its lifetime and target
readiness. An event wake does not complete an unrelated task or user-input wait.
A maintenance notice does not invoke the worker-event state transition.

A retained notice is also published to the original native conversation as a
passive Input: `worker_event` for worker transitions and `maintenance` for a
maintenance request. Its publication receipt retains the original source and
message identity. Publication can complete before an HTTP response delivers the
notice body. Rendering, body delivery and explicit event acknowledgement are
separate observations; none grants permission to finish or represents human
input. Re-reading or acknowledging a notice does not publish another copy.

Eligible bound responses carry at most four notices within a 16 KiB total
growth budget. The collector shortens a page to a fitting event prefix and
sets `next_cursor` to that prefix's final event. If one event cannot fit, a
`read_required` receipt identifies the subscription and `read_limit: 1`.
Use `events.read(controlId, { subscription_id, limit: 1 })` to retrieve it.
Handle each delivered page before explicitly calling `events.ack`. A notice,
a lost outer response or an await result does not acknowledge any event,
consume input, or prove that an external model received it.

Set `enabled: false` to pause automatic delivery and wake behavior while
retaining the current cursor. `events.unsubscribe` explicitly ends the managed
subscription and leaves it disabled; another bind does not recreate it. The
original authenticated main can revoke its own managed observer even after
the pool binding becomes unavailable, without reading or acknowledging the
old source. To recover after that revocation, bind the live pool with separate
valid credentials and explicitly set `enabled: true`. A fresh subscription
gets a fresh ID; it does not inherit or reconcile the old source's cursor.
Expired subscriptions likewise require explicit enablement to start again.

All reads still require the same account, original main and source Sessions,
and private participant bindings. Another member or a replacement connection
cannot inherit these proofs. Gaps and unknown task outcomes remain explicit.
This feature wakes an existing caller or supplies retained events on reconnect;
it does not start a model or schedule an external AI turn.

## Observe in windows of at most 55 seconds

Long waits belong in the public `await` or `events` tools. They suspend on the
server without occupying a JavaScript runner. Code-mode helpers remain useful
for publication, subscription setup and short observations; a helper call does
not make its enclosing JavaScript runner disappear.

For a separate manual subscription, select the control session and its linked pool:

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

The relay retains at most 1,024 events and 1 MiB per account for 15 minutes.
An event can occupy up to 4 KiB. Each read returns at most 64 events or 64 KiB.
Capacity pressure can evict events sooner. A system event too large to retain
can also leave a missing sequence between two retained events. If a manual
subscription reports a gap, reconcile affected tasks and messages, then
resubscribe from a retained cursor.

Managed subscriptions return surviving events in order, including events
before a missing sequence. The page's `retained_after` is one less than the
earliest physically retained sequence, or the journal's latest sequence if
the journal is empty. A `retention_gap` describes an interval for which the
subscription's history is incomplete. Its fields have distinct meanings.

| Field | Meaning |
| --- | --- |
| `after` | The subscription's currently acknowledged cursor. |
| `episode_after` | The acknowledged cursor when this outstanding loss was first detected. It stays fixed across partial acknowledgements. |
| `through` | The highest sequence covered by the loss warning. Earlier matching events can still be available. |
| `resume_after` | The position from which this page scans retained events, taking the actual acknowledgement and physical retention boundary into account. |

For example, if sequences 1, 2 and 4 survive and sequence 3 is missing, a
reader starting at 0 can still receive 1 and 2. Acknowledging through 1
leaves 2 and 4 available. The native conversation shows one warning for that
loss episode. The structured page continues to report the current boundaries.
Process the retained events, reconcile missing work by its original ID, and
acknowledge the page's `next_cursor`. Broadening a filter can also produce a
warning when the newly selected history is no longer available.

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

### A bounded lifecycle maintenance round

A live lifecycle round is the source of executable maintenance guidance. After
the configured idle interval, the existing member can enter `dreaming` with
`lifecycle.maintenance.status: "requested"`. Defaults are a five-minute idle
interval and a 30-second maintenance budget. The round retains its native
`round_id`, requested time and deadline. An advisory policy value does not
create that round.

When the authenticated member is still dreaming, the request is still live,
policy is enabled, no relevant user input is pending and the response is not
an error, the response hook can return
`coordination_hints[].dreaming_round`. This guidance comes from the live
lifecycle snapshot. It does not require a standby pool worker: the coordinator
can do an already-authorized bounded chore itself. Delegating a separate chore
still requires real available capacity and the ordinary worker submission and
claim contracts.

Preserve the returned `round_id`. Read the member's current lifecycle revision
before each start or outcome update and use that revision as
`expected_revision`; a stale revision requires reconciliation. New user input
preempts further maintenance, and the native budget deadline ends the round.
Do not infer an outcome from a timer, elapsed budget or a repeated hint.
[Lifecycle updates and maintenance receipts](workers.md#member-lifecycle-and-bounded-maintenance)

A dependency wait inside an active claim remains working. A suitable authorized chore might inspect an existing failure,
review a pending patch or improve the current task's documentation. When no
useful authorized work remains, wait for input or worker events using the
existing bounded observation contract.

For compatibility, `events.policy` can store an advisory `dreaming` object:

~~~json
{
  "dreaming": {
    "round_id": "tools-round-1",
    "repository": "project",
    "budget_ms": 20000
  }
}
~~~

All three fields are required: `round_id` is 1–128 ASCII letters, digits,
underscores or hyphens; `repository` is `project` or `global`; and
`budget_ms` is 1,000–55,000. Invalid patches preserve the old policy, omission
preserves the field and `{"dreaming": null}` clears it. These scope names
neither register a source nor grant permission to edit it. Setting this object
starts no lifecycle round, timer, model, worker assignment or Git write.

The existing bounded hint cache retains the last offered live round. Policy
entries expire after 15 inactive minutes, and both the advisory registry and
hint cache hold at most 256 entries. Eviction or reconnect can expose a familiar
round again. Reuse its original task and job identities; a cancelled wait or
an `unknown` outcome never authorizes another execution. Event ACK still does
not deliver user text; handle `user.input_available` through the original
input-delivery path.
See [the tool-maintenance workflow](tool-repositories.md#maintaining-workflows-with-dreaming).

`window_ms`, `strategy` and `dreaming` remain advisory. Managed `enabled` and
`kinds` change the actual subscription; neither set of fields changes task
ownership, access control or completion receipts. The hook shares its existing 750 ms probe
deadline and 4 KiB response-growth budget; a suppressed or oversized hint does
not spend the round's delivery opportunity.
[Policy validation](../src/http/coordination_policy.rs) ·
[Hint gating and cache](../src/http/worker_reminders.rs)
