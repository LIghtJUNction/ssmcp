# Worker sessions

`ssmcp alice@relay.example --worker 3` keeps the current window as the main chat and opens a second window with three worker slots. Use the main window to talk to the coordinating agent; use the worker window to follow reported agent progress and inspect task states. Logs and client resource details are available with F2. Each window has its own client connection and operating-system process.

The default worker is an external agent. It actively connects to the worker window, registers with `role: "worker"`, and calls `ssmcp.worker.wait` to wait for a task. After executing an assigned task, it reports the result with `ssmcp.worker.complete` and waits again. Starting the pool creates task capacity without starting any model.

## Member lifecycle and bounded maintenance

Collaboration members have five lifecycle states: `working`, `idle`, `offline`,
`dreaming` and `retiring`. These are separate from native pool station counts
and task outcomes. A state change uses `collab.update` with the exact member's
current `lifecycle.revision` as `expected_revision`. Read the public member
snapshot first; a stale revision is rejected. Reconcile an unknown update
outcome from the original state instead of replaying it.
[Lifecycle state machine](../src/collab/lifecycle.rs)

New members begin `idle` at lifecycle revision 1. The default idle interval is
300,000 ms (configurable from 1,000 to 86,400,000); the default maintenance
budget is 30,000 ms (configurable from 1,000 to 55,000). Configure these as
top-level `idle_after_ms` and `maintenance_budget_ms` fields with the current
`expected_revision`.

At an eligible idle deadline the relay enters Dreaming. Its existing deadline
driver checks the member's inbox once per round. Acknowledging the last message
removes the message but can leave its queue storage allocated. If the inbox is
empty, the driver releases that one allocation. A nonempty inbox keeps its
messages and storage. The round's `maintenance.local_housekeeping` receipt
reports the actual `released_slots` and `released_slot_bytes` (slot storage
only). Zero means the check ran and released nothing. A null receipt means
the local action has not run. The driver then sleeps until an existing
notification or deadline. Human input and a new worker claim preempt Dreaming.

The same round offers the participant a separate, bounded maintenance action.
The local receipt leaves the participant's round status and outcome intact.
For an already authorized action, retain the returned `round_id` and report
`maintenance: {round_id, action: "start"}` with the current revision. Report the
actual `complete`, `fail`, `cancel` or `unknown` outcome separately. A lifecycle
status change and a maintenance outcome must use separate updates. New human
input preempts the round; uncertain running work keeps that outcome until its
original effects are reconciled. Generated system, timer and worker-event
inputs remain passive. Budget expiry cancels an unstarted request; a running
round becomes `unknown`. That outcome neither proves the work stopped nor
authorizes another execution.
[Maintenance and preemption](../src/collab/lifecycle.rs) ·
[Visible generated inputs](../src/http/input_projection.rs)

### Voluntary offline requests

Main and worker participants enter Dreaming after the configured uninterrupted idle interval (five minutes by default). An active claim remains working while its dependencies run. A new human instruction or claim restores working immediately, invalidates any prior exit permission and starts the idle/Dreaming sequence over. Wait for the relay deadline; preserve the original outcome of any interrupted work.

To request voluntary offline, read the exact member's live `status`, `lifecycle.dreaming_since_ms` and `lifecycle.dreaming_elapsed_ms`. The status must still be `dreaming` and the uninterrupted elapsed time must be at least 1,800,000 ms. Keep the same member, connection and episode start with the request. The default 30,000 ms allowance bounds one maintenance action. Between actions, keep blocking on notifications. If continuous time cannot be verified, remain available.

The coordinator uses the control thread's real `ui.wait(intent:"finish")` decision only for its own exit. Preserve one pending request ID across observation windows and wait quietly. Only a matching final outer `may_finish:true` with a consumed Yes from the local Human authorizes voluntary exit. On No, bookmark the current episode and elapsed value and wait another uninterrupted 1,800,000 ms **from the refusal** before requesting again. Recheck the member, episode and outstanding work before acting.

A worker forwards one request with its member, pool and episode start to the bound coordinator and continues blocking on `worker.wait`. The coordinator asks the control-window user one explicit **worker-only** question through `ssmcp.ui.ask` using one retained idempotency key, naming the worker and episode. Read the exact question's answered record and verify the unambiguous Yes and `answer_actor.origin == "human"`. Relay the question ID, control session, answer revision and episode to the worker for verification through the authorized control context. A coordinator's own decision, a pending or skipped question, a missing receipt, and silence leave the worker available. A new claim invalidates the request. New work always takes priority, including work arriving while the human is deciding.

After approval, finish or reconcile outstanding claims; use the real retirement/release protocol for an attached station. If there is no exact retirement receipt or authorized station-removal path, keep waiting.

An agent must not bypass this process using `collab.update`, `leave`, `retiring`, pool resize, `worker.release`, or by letting a lease expire. An actual operator-ordered stop, revocation or station retirement follows its existing draining and acknowledgement protocol. It can remove capacity independently of voluntary offline. Never start or restart a model to obtain a decision.

Work dispatched through the native worker queue still requires its real claim.
Preserve all claim fields and report the task through `ssmcp.worker.complete`;
a maintenance round cannot substitute for that claim. Within an authorized
task, make independent progress while dependencies run. When no useful work
remains, block on the existing wait or event notification and retain pending
wait IDs. This policy uses existing agents and does not require every other
worker to be idle.
[Lifecycle maintenance and advisory policy](events.md#a-bounded-lifecycle-maintenance-round)

## Start a pool

Start the main chat and its worker window with one command:

```sh
ssmcp alice@relay.example --worker 3
ssmcp alice@relay.example --worker 10
```

The worker window shows the real state of each station, the total station count, and queued work. All stations begin **offline**. An external worker registers and actively waits before its station becomes **idle**; an assigned task makes it **working**. The monitor has no chat composer. Keyboard input and paste cannot become input to a hidden shell. Ordinary `ssmcp` opens only the main chat.

The same external-worker workflow applies on Linux, macOS and Windows. Each worker uses its own existing agent application and tools.

The foreground command needs an available terminal adapter for its second window. To select a terminal explicitly, set `SSMCP_WORKER_TERMINAL` to one executable path. The value is a literal path, including any spaces; it is not a shell command or a list of options.

```sh
SSMCP_WORKER_TERMINAL=/absolute/path/to/xterm ssmcp alice@relay.example --worker 3
```

The child receives the original working directory and arguments as separate values. Startup waits for the actual worker process and its connection to become ready. A terminal launcher's successful exit alone does not establish readiness. An unavailable terminal or failed child startup produces an explicit error.

Closing the main chat stops its owned worker window. A main-window `/reload` closes the old worker connection and starts its replacement. Closing only the worker window leaves the main chat available. A relay reconnect keeps the existing window rather than opening another pool.

## Invite agents and adjust capacity

Use these commands in the main chat:

| Command | Result |
| --- | --- |
| `/invite` | Show a connection prompt for the main coordinating agent. |
| `/invite worker` | Show a prompt for an external worker to join the linked pool and actively wait. |
| `/worker ls` | Show the linked pool's actual stations and registered workers. |
| `/worker +N` | Add `N` offline stations. |
| `/worker -N` | Request removal of `N` stations, starting with offline and idle stations. |
| `/worker retire #03` | Select station #03 for retirement. Repeat the command locally after its confirmation prompt to force that station offline. |

Invitation text appears in a local, scrollable panel, and the client also tries to copy it to the clipboard. A missing clipboard does not prevent reading the prompt. When the pool is fully staffed, `/invite worker` displays `Worker stations are fully staffed.` and does not generate another invitation. Sending a prompt to an external agent does not itself connect that agent or start a model.

Shrinking a pool preserves in-flight claims and cancellation acknowledgements. A station being removed while its task is still running remains **working**. Once it has no running task but still awaits retirement confirmation, it is **retiring**. The physical station count is `idle + working + offline + retiring`; `desired_total` describes the requested capacity while retirement is pending.

To remove a worker that can no longer acknowledge retirement, enter `/worker retire #03` in the main terminal, substituting the number shown in the worker window. Read the station and task in the prompt. If that worker remains stuck, enter the same command again. The pool records any unconfirmed task as `unknown` with `replay_safe: false`. Other stations keep their claims. A normal release before confirmation preserves the recorded result. The pool cannot stop work that the external agent may still be doing.

Capacity can range from zero to 128 stations. While a count-based resize is awaiting retirement acknowledgements, only a repeat of the same target is accepted for that resize. The parent projection's `last_resize` identifies the actual request, target, acceptance and any error; sending a window-control message alone does not prove that the pool accepted it. A pool with zero stations remains connected and can grow again.

## Connect the external workers

A worker is an external agent that actively joins the worker connection and waits for a task. Keep each agent's existing conversation open while it waits. SSMCP supplies the task queue and tools; the external application supplies the model.

1. Connect the main window. Enter `/invite` and press Enter, then paste the prompt into the authorized coordinator conversation. The coordinator joins the control session as `main` and binds with its own credentials.
2. Start the linked pool with `ssmcp alice@relay.example --worker 3` or open stations with `/worker +N` in the main window. Check for a connected pool and a free station with `/worker ls`. In the main window, enter `/invite worker` and press Enter. Paste the prompt into each separate authorized worker conversation.
3. Each worker joins the linked pool as `worker`, binds with its own private credentials, and calls `ssmcp.worker.wait`. A wait timeout leaves it available. The worker calls wait again.
4. The main agent submits tasks. A waiting worker receives one authenticated claim, performs the task, and reports it with `ssmcp.worker.complete`. After completion, it requests another task with a new wait request ID.

Both commands open a scrollable panel and try to copy the text. Copy it manually if needed and press Esc to close the panel. Wait for a connected, stable pool with a free station before creating a worker invitation.

Join the selected worker client once:

```js
const participant = await collab.join(id, {
  name: "worker#3",
  role: "worker",
  capabilities: ["implementation", "tests"]
});
```

Save the returned `agent_id` and private `token`. Bind that same participant in each later call before waiting:

```js
await collab.bind(id, {
  agent_id: participant.agent_id,
  token: participant.token
});
const reply = await call(id, "ssmcp.worker.wait", {
  request_id: "worker3-next-task-1",
  wait_ms: 5000
});
if (reply.isError) throw new Error(JSON.stringify(reply));
return reply.structuredContent;
```

Reuse the same request ID while waiting or recovering a lost response. The result contains `waiting`, `assigned`, `already_completed`, `retiring`, `released`, or `not_accepting`. A wait timeout leaves the agent available. An idle station in a capacity snapshot can be between wait calls. A submitted task with `status: "queued"`, `attempt: 0` and `slot: null` is waiting for its first claim. Use the complete claim unchanged when reporting a result, then use a new request ID to wait again. An assigned task can carry both `cancel_requested` and `retire_requested`. Finish that claim before requesting its retirement receipt; the old wait ID continues to return its completed-task receipt. For `retiring`, acknowledge the exact `retirement` object with `ssmcp.worker.release`. Stop waiting after release succeeds or the status is `released`.

Registered worker participants and idle stations are separate counts. Three newly created stations are offline until workers actually connect and wait. Read the actual station binding from the pool; an agent's display name, such as `worker#3`, does not select panel `#03`.

## Reconnect after an unexpected disconnect

Keep the project task across a disconnect, reload or manual restart. Record its authorized workspace and verified device/session identity at bind. The control and pool client IDs route their current connections. Preserve the agent conversation, original claims and task evidence.

1. Automatically refresh `list`. Use the saved device/session identity where supported or obtain a fresh invitation or operator selection for an ambiguous or unverified replacement. Discover current schemas. Verify the authorized workspace and the control/pool link using a fresh invitation or the main client's `/worker_pool` projection. Workers can obtain that link from the verified coordinator. A matching display name or working directory alone does not identify a replacement connection.
2. If the original connection and membership remain valid, bind the existing private credentials and retain its wait request ID. If the client was replaced or membership expired, join and bind your own fresh credentials with the same authorized role. Keep credentials private; workers must not take over the manager role or consume control-session next-caller input.
3. Preserve the complete original claim and its generation. Use it only while the original pool and authenticated claimant still accept it. Never edit an old claim to fit a new generation, or send it as a claim on a replacement pool. A new connection or membership starts a new wait request ID after reconciliation.
4. Inspect retained receipts, job state/output and worktree/commit evidence where available. Report to the verified coordinator what completed, what is still running and what remains `unknown`. A lost response does not prove failure or cancellation. Keep unresolved outcomes `unknown`; joining again does not authorize repeating a command, submission, build or deployment.
5. Resume remaining work only under a valid current claim. After a pool replacement, reconcile the old task first, then wait for a fresh assignment. Rejoining restores participation; it must not start or restart a model.

If the authorized link cannot yet be verified, keep the task paused, report the blocker and retry bounded discovery while remaining available. An explicit exit or retirement request still follows the completion and release procedure above.

## Use the monitor

Select a station to follow its **Agent progress** timeline at full width, with a compact task summary and actual state above it. The timeline shows progress reported for the authenticated task claim. Command starts and finishes do not become progress updates. Until a report arrives, it displays `Waiting for agent progress updates`. Warnings, errors, unknown outcomes and cancellation notices remain visible; command failures show their observed exit, signal or cancellation outcome with a pointer to F2 Logs.

Press F2 to open the selected station's logs and client resource details. Wide terminals put a fixed activity column beside the log; narrow terminals put a compact summary above it. The default progress view keeps that space for reported work. F1 opens the monitor's own help page.

| Control | Action |
| --- | --- |
| Left / Right or Tab / Shift+Tab | Select the previous or next worker. |
| F2 | Switch the selected worker between progress and logs / client resource details. |
| F1 | Open or close the monitor help page. |
| T | Open or close the full task, including complete task and manager IDs. |
| Enter | From progress, open logs with command text expanded; in log details, expand or compact command arguments. |
| Esc | Close help or the current task / log detail view. |
| Up / Down | Scroll the current progress, log or task view. |
| Page Up / Page Down | Scroll the current view by a page. |
| Home | Show the earliest content still retained in that view. |
| End | Return to the latest content and follow new activity. |
| Ctrl+Home / Ctrl+End | Select the first or last worker. |
| Ctrl+R | Request a reload of this worker session. |
| Ctrl+C | Close this worker window. |
| Mouse click | Select a station from the station navigation. |
| Mouse wheel | Scroll the selected station's current content when the pointer is in that area. |

In an attached worker window, Ctrl+R replaces the worker child while keeping the main chat, its draft, and its connection open. The replacement keeps the requested pool size, working directory, and launch arguments, but starts with a new client identity and no inherited manager or worker registrations. A standalone or headless worker follows the existing process-restart path.

Reload is refused while tasks are running or queued; the worker panel shows the reason. Finish the tasks, or cancel them and wait for their acknowledgements and cleanup, before retrying. An accepted reload follows the existing connection cleanup and starts a fresh in-memory pool. It does not resume old tasks or prove that an external executor has stopped.

Each worker keeps separate progress and log scroll positions. Changing worker or toggling F2 preserves those positions. A new task or station epoch resets the progress position; the station's retained diagnostic log stays available. With no active task, the timeline is labelled **Recent agent progress**. If the original log entry has expired but the latest reported text remains in the snapshot, it appears as **Latest report**, without an invented timestamp.

Log details use a muted time column, a short event label and aligned body text. Continuation lines stay under the body. RUN marks a reported command start; DONE requires an explicit exit 0; END means the finish record did not include an outcome. Warnings, failures and nonzero exit codes remain visible. Command start and finish records stay separate and preserve their observed order. Enter reveals retained command arguments.

Each station retains up to 64 log entries within a 16 KiB budget; an entry's text is limited to 1 KiB. Older entries drop out as new activity arrives, so Home only reaches the earliest retained entry. The monitor shows task summaries and work attributed to that station. A missing command or progress report remains explicitly unavailable. Full task results remain accessible through the authenticated `worker.result` workflow.

F2 details show current and accumulated work time and the last three completed task durations when those observations are available. Resource readings identify the process they measure. An external model has no implied local PID, so its CPU and memory use cannot be inferred from the worker window. Local client measurements, their bounded line chart, and the CPU heat value refer to the worker client process and are labelled accordingly; unavailable samples remain unavailable.

The F2 detail header shows the worker client's OS PID and a compact manager binding status when space permits. Press T from either view to read the full task and complete manager client and thread IDs. The main window's Statistics page starts with session labels, member roles, worker states and current task summaries. Press Enter to expand Diagnostics with complete session, thread, pool-generation and task IDs; Enter collapses it, and Esc returns one level. These views preserve the message recipient and management binding.

Task submission and assignment records appear in the main conversation, in operation order. Each card identifies the observed state and assigned worker, when known; expand it to read the retained summary, exact task ID, station epoch and pool generation. Repainting does not duplicate cards. These records use the conversation's existing bounded history and scroll behavior. Wait operations also keep their full conditions in expandable conversation cards. A live wait updates its existing card without adding repeated entries or changing its position. Statistics keeps concise pool counts, with complete identities in Diagnostics.

[Monitor rendering and input](../src/client/terminal/worker.rs)

## Bind the main agent and manage tasks

Read the main chat's `ssmcp.ui.get` projection at `/worker_pool` to identify its linked pool. It returns the actual worker `client_id` and `process_id`, connection state, station counts, queue length and registered-worker count. Its `manager` projection contains only the bound manager's `client_id` and `thread_id`, or `null` when unbound. Confirm that client through `list` before binding a participant. The main chat and worker pool have distinct client IDs; the worker advertises `chat_enabled: false` and exposes `ssmcp.worker.status`. A display name alone does not select a connection.

```js
const projection = await call(mainClientId, "ssmcp.ui.get", {path: "/worker_pool"});
if (projection.isError) throw new Error(JSON.stringify(projection));
return projection.structuredContent.value;
```

Worker tools use the existing authenticated native call route. Bind a real collaboration participant to that worker client, then call `ssmcp.worker.attach` to become the pool's main agent. The pool derives this identity from the verified call scope; arguments cannot supply an account, owner, slot, or connection identity. Reading status or claiming a task does not acquire management rights.

```js
const id = "the-worker-client-id";
const participant = await collab.join(id, {
  name: "task-dispatcher",
  role: "dispatcher",
  capabilities: []
});
await collab.bind(id, {
  agent_id: participant.agent_id,
  token: participant.token
});

const attached = await call(id, "ssmcp.worker.attach", {});
if (attached.isError) throw new Error(JSON.stringify(attached));

const reply = await call(id, "ssmcp.worker.submit", {
  request_id: "review-issue-123-attempt-1",
  text: "Review @src/example.rs and report the concrete findings."
});
if (reply.isError) throw new Error(JSON.stringify(reply));
return reply.structuredContent;
```

Keep that participant's credentials private and reuse the same binding for subsequent calls. A pool has one main agent; a different participant cannot take it over or manage its tasks. The first successful `submit` can also establish the binding for existing callers; a rejected submission does not reserve the pool. Native calls return the ordinary MCP envelope: task data is under `structuredContent`, not directly on the outer reply.

| Tool | Arguments | Result |
| --- | --- | --- |
| `ssmcp.worker.attach` | `{}` | Bind the authenticated main agent; return its verified identity, `caller_is_manager`, and the worker snapshot. |
| `ssmcp.worker.status` | `{}` | The typed monitor snapshot, main agent identity, `caller_is_manager`, execution mode, resource status, and any cleanup error. Reading it does not change the main agent. |
| `ssmcp.worker.events` | `{after?: {generation, seq}, limit?: 1..128, wait_ms?: 0..55000}` | Read or wait for committed lifecycle metadata; non-consuming cursor pages, explicit retention gaps, default limit 32 and no wait. |
| `ssmcp.worker.tasks` | `{cursor?, limit?: 1..128}` | Main agent only: a bounded page of task IDs and lifecycle metadata, with `next_cursor`; default limit 32. It excludes task text, output, and error bodies. |
| `ssmcp.worker.wait_available` | `{after?: {generation, revision}, wait_ms?: 0..55000}` | Attached main agent only: observe capacity or wait for its revision to change; omit `after` for an immediate snapshot. |
| `ssmcp.worker.wait_for` | `{generation, mode, conditions, wait_ms?: 0..55000}` | Attached main agent only: wait for `any` or `all` specified task-finished or worker-idle conditions in that exact pool generation. |
| `ssmcp.worker.submit` | `{request_id, text}` | One task receipt, reference warnings, and the current worker snapshot. |
| `ssmcp.worker.result` | `{task_id, wait_ms?: 0..55000}` | That caller's task state and any confirmed output. |
| `ssmcp.worker.cancel` | `{task_id}` | A cancellation request; a running task remains occupied until completion or confirmed cleanup. |
| `ssmcp.worker.stop` | `{}` | Main agent only: stop accepting new tasks, cancel queued work, and request cancellation of running work while retaining its occupied slots. |
| `ssmcp.worker.resize` | `{total: 0..128}` | Attached main agent only, remote mode: change desired capacity. Added stations are offline; occupied stations await completion and retirement acknowledgement. |
| `ssmcp.worker.wait` | `{request_id, wait_ms?: 0..55000}` | Remote mode: wait for a claim, default 5000 ms; return a waiting, assignment, completion, retirement, release or nonaccepting status. |
| `ssmcp.worker.claim` | `{request_id}` | Remote mode: the same claim operation without waiting. |
| `ssmcp.worker.complete` | `{claim, status, output?, error?}` | Remote mode: report the result for the exact claim and its authenticated claimant. |
| `ssmcp.worker.report` | `{claim, request_id, event, command?, progress?, detail?}` | Record actual command or progress observations for the claimant's active task; return `recorded`, duplicate status, and cancellation or retirement flags. |
| `ssmcp.worker.release` | `{retirement}` | Acknowledge the exact retirement receipt with its authenticated worker; an active task must finish first. |

To wait once for a result, use `call(id, "ssmcp.worker.result", {task_id, wait_ms: 5000})` under the same verified binding. A timeout returns the current task state; it does not complete or re-submit the task.

Tasks keep the authenticated caller and original live-thread owner captured at submission. Completion does not follow a later UI selection. File and skill references use local expansion; a missing file warns while retaining the literal message.

The snapshot exposes `manager` and `stopping`. An unbound pool has `manager: null`. A bound manager includes the actual `client_id`, verified `thread_id` and participant identity, connection, and binding time. The snapshot's `process_id` is the local operating-system PID; it is separate from the session's client ID and the main agent's logical thread ID.

`stop` returns `stop_requested: true` and whether this call changed the state. Repeating it is safe. Its `external_execution_stopped: false` explicitly means that requesting a stop does not prove an external model or process has stopped. Running remote tasks remain occupied with cancellation requested until their original claimant acknowledges completion; their slots then go offline. A real connection loss continues to use the existing `unknown` outcome and must not trigger automatic replay.

## Dispatch independent tasks as results arrive

Submit ready work with stable request IDs and retain the returned task IDs and pool generation. Workers can claim queued work as soon as a station frees up. When several results can unblock the next step, use one `worker.wait_for` with `mode: "any"` and a `task_finished` condition per outstanding ID (up to 128). Read `worker.result` once for each satisfied ID, remove it from the outstanding set, and submit newly eligible work immediately. For a single ID, a blocking `worker.result` returns the body in the same call.

When dispatch depends on capacity, take `{generation, revision}` from the last manager receipt's `availability` and pass it as `after` to `worker.wait_available`. On `not_accepting`, inspect the reason. A full queue can drain through an outstanding completion; closure or a stopped pool requires reconciliation. For recovery, read committed `worker.events` from the last processed cursor and verify terminal results by task ID. Reconcile a gap against retained state. Preserve unresolved outcomes as `unknown`.

Set an explicit observation window that leaves room within the transport deadline (up to 55,000 ms). A native wait returns as soon as its condition holds. After an empty window, renew the same outstanding wait or cursor. An idle worker reuses its pending claim request ID until it receives a claim or a terminal receipt.

### Reproduce the scheduling comparison

The pool tests run real claim, completion, condition and event handlers with synthetic task durations. Run the fixed-clock comparison and the progress-wakeup regression with `cargo test --locked --lib worker_scheduler -- --nocapture --test-threads=1`. Run the two wall-clock benchmarks with `cargo test --release --locked --lib worker_scheduler -- --ignored --nocapture --test-threads=1`.

The comparison starts with 12 seconds of idle time, then queues two jobs per worker. Service times are 7, 8 or 9 seconds per job. The reference policy scans pending results every 5 seconds and uses 5-second worker waits. The blocking policy claims with a 55-second window and groups outstanding IDs in a single condition wait. Both retry the first submission, claim, completion and event page. Counts include these retries. The execution interval starts at submission and ends at the final completion.

| Workers | Completed jobs | Handler calls, reference / blocking | Empty responses, reference / blocking | Execution time, both |
| ---: | ---: | ---: | ---: | ---: |
| 3 | 6 | 52 / 35 | 23 / 0 | 18 s |
| 7 | 14 | 114 / 67 | 53 / 0 | 18 s |
| 17 | 34 | 272 / 148 | 130 / 0 | 18 s |

The event journal verifies unique completions and peak concurrency. The wall-clock saturation test preloads 120 tasks, uses four Tokio threads, and times the drain with all workers already holding a claim. Sample zero warms up each pool size. Model inference and subscription accounting take place outside this fixture.

To repeat the cross-revision comparison from this checkout, install the same test modules into a detached copy of the measured parent. Both binaries use the repository's Release profile. Runs print every sample, task count, peak occupancy and elapsed time.

```sh
run=$(mktemp -d)
base="$run/base"
git worktree add --detach "$base" 28ea7289f26e2b517f364e4b38abb9287a527f6d
for module in noise_tests saturated_tests scheduling_tests; do
    cp "src/worker/pool/$module.rs" "$base/src/worker/pool/"
    printf '\n#[cfg(test)]\nmod %s;\n' "$module" >> "$base/src/worker/pool.rs"
done
for version in base candidate candidate base; do
    tree=.
    if test "$version" = base; then tree="$base"; fi
    CARGO_TARGET_DIR="$run/target-$version" cargo test --manifest-path "$tree/Cargo.toml" \
        --locked --release --lib worker_scheduler_saturated -- --ignored --nocapture --test-threads=1
done
```

## Member tool activity

The Sessions recipient row and the selected worker display the number of attributable tool requests admitted for that member during the past ten minutes. The relay uses a monotonic clock and groups requests by millisecond. A newly joined member may show zero while the observation window fills. After ten full minutes without a call, the Sessions row shows Offline activity and the worker monitor lists the station number as quiet. A subsequent call clears that activity marker. A client that has no measured value leaves the count blank.

One user-issued bound native tool or catalog call, client-scoped resource read, explicit collaboration action, authenticated event or messaging request, or typed await counts once on its connection. Explicit UI waits count once as well. An await covering several targets counts once, including when the relay repeats a native observation in the background. An explicit new read of a retained wait counts as a new call. Automatic probes produce no count. Server-wide discoveries, account-owned jobs, and multi-connection transfers do not select one verified member on one connection. A member's signed-in account and display name cannot establish attribution.

This display concerns tool use. A registered member can work outside the relay for ten minutes and keep a valid task claim. The worker monitor counts Offline from scheduler station state and shows registrations against current capacity. During a shrink, an occupied station stays Working until its claim finishes; the separate target shows the requested capacity while retirement is pending.

## Share work between occupied and idle stations

A registered worker making a blocking `worker.wait` first tries to claim ordinary
work. If it is still idle, the pool sends one request in that worker's name to a
peer holding a live claim. The request identifies both participants and their station
epochs. The claimant reads `share_requests` on its claim wait or progress report.
The helper reads the latest eight `share_replies` on its own wait or report.
Either participant can page its retained history with `worker.share({action:"list"})`.

A claimant can decline when it has no independent work. After a decline, an
unanswered request's 55-second deadline, or a change of claim or station, the
helper can ask another busy peer. Repeated idle polls never ask the same helper
station epoch and parent claim twice. Once a claimant accepts, the manager
submits the exact scope. The helper gives human instructions and ordinary queued
work priority and starts the child only after it receives its own claim.

| Participant | Native call | Result |
| --- | --- | --- |
| Idle worker | `worker.share({action:"request",request_id,task_id})` | Returns a `share_id` for this helper, claimant and parent task. Use the same request ID on retry. |
| Current claimant | `worker.share({action:"accept",share_id,text})` or `{action:"decline",share_id}` | Accept with the scope of an independent child task or decline the request. Pending requests appear in the claimant's wait/report responses. |
| Pool manager | `worker.share({action:"list"})`, then `worker.submit({request_id,share_id,text})` | Submit the accepted text verbatim. The child receives its own task ID and is reserved for the helper's station and epoch. |
| Helper | `worker.wait`, then `worker.complete` | Perform the child under its own claim and return its result. The original worker continues to own and complete the parent. |

The manager can wait for either task through the usual `worker.wait_for` and
read each result by ID. A changed station or parent can invalidate a pending
split. If a reserved station is confirmed offline or removed before it claims
the child, the queue records the child as cancelled with zero attempts. A
manager can inspect the cancellation reason through the original child task
ID. Inspect the returned share status before submitting a child.

### Inspect or reset a station's work history

Read `ssmcp.worker.station_state({slot})` on the authenticated pool to see the
occupant, generation, epoch, current task and progress, recent task outcomes,
and workspace-relative file paths. The queue fills in current work when the
worker claims a task and records its outcome when the claim is completed.
Workers can describe their current step with the existing
`worker.report({claim,request_id,event:"progress",progress})`.

A successful native file read on the pool connection records its path under
the active claim. For a file read using a shell, editor or another connection,
the worker can report `{claim,request_id,event:"file_read",detail:"src/lib.rs"}`
through `worker.report`. The history distinguishes observed and reported
reads. It retains up to 24 path entries and 12 recent completed-task records.

A new occupant starts with fresh history. To clear its own station again,
read `station_state`, then call `ssmcp.worker.station_reset` with the returned
`generation`, `slot`, `epoch` and `expected_reset_revision` set to the observed
`reset_revision`. Reset clears the display history. The current claim and
the manager's retained task results keep their original identities.

## Wait across call windows

Each worker wait observes one request for at most 55,000 milliseconds. A caller can choose a shorter window to leave time for transport and response handling within its own outer deadline. The default remains 5,000 milliseconds for `wait`, `wait_available` and `wait_for`; `result` reads immediately when `wait_ms` is omitted.

The pool keeps queued tasks, active claims and completed results between calls. A task submitted after an idle wait returns is available to the next wait. Retry `worker.wait` with the same `request_id` while waiting or recovering an assignment response: it returns the original claim, including its task, slot, epoch and attempt. After completing that claim, use a new wait ID for the next task. A manager reads the same `task_id` with `worker.result` after a wait window ends; an identical completion report reuses its existing receipt.

This buffering belongs to the current pool connection and its bounded retention. The queue retains up to 128 terminal tasks, and remote completion receipts retain the last 128 reports. Drain results and keep identifiers while the connection is live. A closed connection leaves unconfirmed effects `unknown`; a new connection does not authorize replay.

Routine replies include a compact `worker` snapshot: pool identity, manager, counts, shutdown state and each station's slot, epoch, state, current task and registration or retirement flags. Claims, individual task results and cancellation flags remain in their original response fields. Use `worker.status` for full station logs, command text, progress, timing and permitted manager-operation details. The monitor continues to use the full snapshot.

## Read committed worker events

`ssmcp.worker.events` reads a pool-local journal of the last 1,024 lifecycle events. The queue records each event under the same lock that commits its state change, before waking observers. Delivery of a claim, report or completion response is not required for the event to exist. Retried requests that reuse an existing receipt do not add another event.

Call with `{limit: 32, wait_ms: 0}` to read retained metadata, then pass the returned `next_cursor` as `after`. A cursor contains the exact pool `generation` and last observed `seq`. Each call accepts one to 128 events and a wait budget from zero to 55,000 milliseconds; the defaults are 32 events and an immediate read. The response includes `events`, `oldest_seq`, `latest_seq`, `next_cursor`, `has_more`, `gap`, `closed` and `stopping`. Its status is `events`, `empty`, `timed_out` or `closed`. Reuse the same cursor after a lost response. Reading and waiting do not consume or acknowledge records.

A supplied cursor older than retained history returns `gap: true` with the oldest still-retained events. Continue from `next_cursor` while `has_more` is true; a page cursor advances only through the entries actually returned. Omitting `after` starts at retained history and does not assert that earlier history is complete. A cursor from another generation or beyond the current sequence is rejected. Reconcile a reported gap with current task results and pool status; a gap does not authorize replay. The journal belongs to the live pool and does not survive a new connection.

Events contain `seq`, `generation`, `type`, `at_ms`, `task_id`, `slot`, `epoch`, `attempt`, `participant_id`, `task_status` and `activity`. Fields that do not apply are null. Event types include `task.submitted`, `worker.started`, `worker.progress`, `worker.completed`, `worker.idle`, `worker.cancelling` and `worker.unknown`. A progress event identifies `command_start`, `command_end` or `progress` without copying free-form command or progress text. The journal never includes task input, result bodies, claim nonces or credentials.

`worker.completed` records a confirmed terminal task state, including cancellation of a queued task that never occupied a station. For remote execution, confirmation is the authenticated worker's report; it is not independent proof of OS process cleanup. Requesting cancellation produces `worker.cancelling`, and a lost connection can produce `worker.unknown`; neither is a completion event. `worker.idle` is recorded only when a station actually becomes idle.

The caller must retain its verified collaboration binding and belong to the pool's authenticated account and connection. Metadata reads neither attach a manager nor register or reserve a station. Existing exact-actor restrictions continue to protect task results. Subscribing to queue changes before reading the journal closes the race between checking a cursor and beginning a wait.

[Committed event journal](../src/worker/queue/events.rs) · [Native event contract](../src/worker/pool.rs) · [Event regression tests](../src/worker/pool/events_tests.rs)

## Observe capacity changes

`ssmcp.worker.wait_available` observes the pool under its existing manager binding. It does not attach a new manager, register a worker, start a model, reserve a station or consume human input. Successful worker-tool replies to that manager can also include the same `availability` projection alongside their original result.

The projection separates usable idle capacity from offline and retiring stations. It includes `total`, `desired_total`, `registered_workers`, `idle`, `working`, `offline`, `retiring` and `queued`. In remote mode, idle stations require actual worker registration; an empty station is offline. A busy station selected for removal remains working until completion, then remains retiring until release. Released stations are excluded from `total`, and surviving `slot` IDs and `epoch` values are not renumbered. Counts describe the pool's recorded state, not a heartbeat from an external model.

`accepting` reports whether a new valid task can enter the queue. A ready remote pool may accept queued work while all stations are offline; this does not make any worker available. Zero capacity, all-retiring stations, a full queue, stopping, closure or a guard that blocks new submissions makes it false. `idle_slots` contains observed eligible `{slot, epoch}` pairs only while the pool is accepting. These pairs are observations, so another task can take an idle station before the manager submits its next task.

A cursor waits for a capacity revision, not for a log entry or progress update. Omit `after` to read immediately; otherwise pass the returned `generation` and `revision` with `wait_ms` from 0 to 55000, default 5000. The result status is `current`, `changed`, `unchanged` or `not_accepting`; `not_accepting` takes precedence when the pool cannot admit work. The separate `changed` flag compares the supplied revision with the current one. A cursor from another generation or ahead of the current revision is rejected. Discover the new pool and read a fresh cursor after reconnecting.

Use the same verified manager binding for both calls:

```js
const first = await call(id, "ssmcp.worker.wait_available", {wait_ms: 0});
if (first.isError) throw new Error(JSON.stringify(first));
const {generation, revision} = first.structuredContent.availability;
const next = await call(id, "ssmcp.worker.wait_available", {
  after: {generation, revision},
  wait_ms: 5000
});
if (next.isError) throw new Error(JSON.stringify(next));
return next.structuredContent;
```

The returned `scope: "coordination_slots"`, `execution` and `claim_required` identify what the observation covers. They do not establish an external agent's readiness to generate text or replace the worker's `wait` / `complete` loop. The existing passive reminders remain separate from this explicit manager wait. [Manager tool contract](../src/worker/pool.rs) · [Capacity and revision calculation](../src/worker/queue.rs)

## Wait for explicit conditions

`ssmcp.worker.wait_for` lets the bound main agent state exactly what it is waiting for. Read `worker.generation` from the current pool's `status` reply and provide `mode: "any"` or `mode: "all"`, one to 128 distinct conditions, and an optional `wait_ms` from 0 to 55000, default 5000. The manager identity still comes from the authenticated call; the request cannot choose another owner. A stale generation, unretained or foreign task ID, or stale station epoch is rejected.

| Condition | Meaning |
| --- | --- |
| `{type: "task_finished", task_id}` | The manager's retained task has a confirmed `completed`, `failed`, or `cancelled` state. This does not mean the task succeeded. An `unknown` outcome remains indeterminate. |
| `{type: "worker_idle", slot, epoch}` | That exact station is currently idle and eligible to receive work while the pool is accepting. Remote stations also require actual worker registration. `slot` is zero-based; the monitor's `#01` means slot `0`. Use the actual epoch from the pool snapshot. Offline, retiring or replaced stations do not satisfy this condition. |

All conditions are evaluated together under one queue-state lock. `any` needs at least one satisfied condition; `all` needs every condition satisfied in the same observation. Two workers becoming idle at different times do not satisfy `all` unless they are idle together when checked. These are current-state conditions, not subscriptions to arbitrary log events. Observing an idle station does not reserve it.

For example, under the same manager binding, use a `taskId` from a prior submit receipt and the `slot` and `epoch` from the current pool's slot record:

```js
const observed = await call(id, "ssmcp.worker.status", {});
if (observed.isError) throw new Error(JSON.stringify(observed));
const generation = observed.structuredContent.worker.generation;
const reply = await call(id, "ssmcp.worker.wait_for", {
  generation,
  mode: "all",
  conditions: [
    {type: "task_finished", task_id: taskId},
    {type: "worker_idle", slot, epoch}
  ],
  wait_ms: 5000
});
if (reply.isError) throw new Error(JSON.stringify(reply));
return reply.structuredContent;
```

The reply identifies the operation, pool generation, mode, each condition's state and the measured elapsed time. Its status can be `satisfied`, `timed_out`, `indeterminate` or `unavailable`; inspect that status before proceeding. An uncertain task or a station that disappears cannot be silently treated as completion. This tool does not replace `worker.wait` for claiming work, `worker.result` for retrieving one task's result, or `worker.wait_available` for observing capacity revisions.

## See dispatch and waiting state

The default Activity view shows a compact summary of real submissions, assignments and current waits. A **submitted** receipt means the task entered the queue; it does not identify a worker. An **assigned** observation names the task and its actual slot and epoch after a successful claim. The worker monitor shows the waiting mode, condition type, targets and elapsed time while preserving the selected station's progress or log position. The conversation's expandable wait card contains the complete target list and exact identities. Statistics keeps concise pool counts; Diagnostics contains full pool and session identities.

Wait cards show the requested target or an authorized, redacted task summary, the actual `ANY`, `ALL`, `SINGLE` or `CHANGE` mode, elapsed time and this call's observation budget. Long task, observer and generation IDs stay in expanded details. `Triggered` means the requested condition caused a return; `Window expired` means this observation returned without establishing completion. `Observation stopped` does not cancel execution. Missing or uncertain receipts remain `Unknown`.

General `await`, standalone `events.wait` and native waits use the same conversation viewport. A local display deadline without a terminal receipt becomes `Unknown`. Public awaits carry declared target references, authenticated public member labels, mode, timing and typed outcomes through negotiated `wait_display_v4`, including durable-observer metadata; peers that negotiate only `wait_display_v3` retain the original short-window format. These displays do not copy private inbox bodies, task results or unauthorized object metadata. Peers that negotiate neither capability receive no new display frames. Native `exec.wait` uses its real default 55-second window, while `worker.result` defaults to an immediate query and worker `wait`, `wait_for` and capacity-change waits default to five seconds. `wait_available` without a cursor remains an immediate capacity snapshot. Rendering these states does not acquire a task, acknowledge an inbox or change a queue.

The pool records these observations in `manager_operations`. Each real call has an `operation_id` within its pool generation. `wait_for` is labelled `any` or `all`; a one-task `result` wait is `single`; a capacity-revision wait is `change`. Only `state: "waiting"` is active. Completed observations move to bounded history with their actual outcome, such as `satisfied`, `timed_out`, `indeterminate`, `unavailable`, `cancelled` or `error`. Existing availability outcomes retain their own names. At most 16 active observations and 64 terminal observations are retained; the active target total is bounded at 128. Targets are not silently dropped to make an accepted wait appear complete.

`elapsed_ms` comes from a backend monotonic `Instant` for that call. Repeated polling creates distinct operations; it does not count the gaps between calls as time spent waiting. `wait_ms` is a native waiting budget of at most 55 seconds, rather than a promise about transport or scheduling latency. An outer `code_mode` timeout alone does not prove that the native wait future was dropped. Cancelling or dropping an observation ends that observation, not its tasks; task cancellation still requires the existing `worker.cancel` and acknowledgement flow.

The main window reads the owned child's fresh projection. A disconnected or closed child connection, or a snapshot older than three seconds, is not shown as a live observation. The frequent `/worker_pool` projection stays limited to counts and identity; the internal monitor projection carries complete retained operation targets. The separate task-activity overview keeps its existing bounded summaries and explicit omitted count. Operation metadata and the linked activity overview omit full task input, result bodies and participant credentials; the local worker full-task view remains separate. [Operation lifecycle and conditions](../src/worker/queue/operations.rs) · [Native tool contract](../src/worker/pool.rs) · [Owned-window projection](../src/client/worker_window.rs)

## Remote claim and completion

A remote executor must use its own authenticated collaboration binding on the same account and connection. An assignment includes prepared `text`, `cancel_requested`, `wait_hint`, and the complete `claim` object: `id`, `generation`, `slot`, `epoch`, `task_id`, and `attempt`. Keep that object unchanged. The submitter owns the original task; the claimant's identity comes from the authenticated call, not from a user-supplied field.

Repeat the same claim `request_id` while waiting or recovering its response. This returns the existing assignment rather than claiming a second task. After finishing it, use a new `request_id` to request another task. Only the original claimant can complete the assignment; a stale generation, altered claim, or another participant is rejected.

Use one completion form after performing the work:

- `status: "completed"` with the actual `output`, at most 64 KiB.
- `status: "failed"` with a nonempty `error`, at most 4 KiB.
- `status: "cancelled"` with neither output nor error.

A matching repeat of a retained completion is idempotent; changing its payload is rejected. Completion is identified as `authenticated_remote_agent`: it records what the remote agent reported, not an independent OS observation that an external process succeeded or stopped. Cancellation requests do not forcibly terminate an external agent or release an occupied slot before its report.

Native command starts and finishes are attached to their real task and authenticated caller. For progress observed by the external worker, call `ssmcp.worker.report` with `event: "progress"` after the work happened:

```js
return await call(id, "ssmcp.worker.report", {
  claim: assignment.claim,
  request_id: "task-step-1",
  event: "progress",
  progress: "The requested check completed; its actual result is available."
});
```

The tool also accepts `command_start` and `command_end` events for commands an external worker actually observes. Command text is limited to 4096 bytes; progress and detail are limited to 1024 bytes. Use the event's fields from the discovered schema. Repeating a retained request ID with identical content is idempotent; changed content, another caller or a stale claim is rejected. A progress report does not complete the task.

For cooperative removal, the original worker receives a retirement receipt through `worker.wait`. Keep that object unchanged. `worker.release` refuses an active claim and only removes the station after the owning worker acknowledges retirement. Repeating an already accepted release is safe while its receipt is retained. The worker can then leave its collaboration membership and stop waiting; no external model or operating-system process is forcibly terminated by this acknowledgement.

A closed connection between the worker client and its relay makes unconfirmed assigned work `unknown`; do not replay it automatically. An agent that goes silent has not confirmed completion or cancellation. Its in-flight task continues to occupy its station until the task's lifecycle resolves; an idle-status display must not be used as evidence that external effects have stopped.

The default remote pool does not impose CPU or RAM limits on external agents. Its resource status reports external execution and no local OS quota. Active push transports and model APIs are not selected implicitly by a host or port.

## Task queue and cancellation

An atomic FIFO claim assigns a task to exactly one idle slot. When all slots are occupied, up to 128 tasks can wait. Input text is limited to 16 KiB and expanded input to 48 KiB. Workers follow their assigned task and authenticated claim.

Queued cancellation prevents assignment. Cancellation of an assigned remote task keeps its slot occupied until the original worker acknowledges completion. The worker checks `cancel_requested` by repeating its wait request with the same request ID, stops its own work, and reports `status: "cancelled"` with the original claim.

A worker client disconnect makes unconfirmed assigned work `unknown`. Preserve that outcome and inspect any effects before retrying. Identical submission requests deduplicate within the same caller and connection; terminal results have bounded retention.

An unexpected disconnect is a recovery attempt, not a completed task. Follow [`ssmcp://recovery`](recovery.md): preserve the exact claim and original request IDs; make bounded read-only rediscovery attempts; prefer a previously verified, authorized 007 on the target device; never clear explicit stop/retirement or bypass revoked authorization. Match device and saved-definition identities before a single guarded `ssmcp.007.recover`. A recovered connection cannot make an old pool generation or unknown effect replay-safe. If the target or 007 is unavailable, report the blocker and retained checkpoint, and remain resumable.

A waiting external agent uses bounded `worker.wait` calls and continues waiting after idle timeouts. It does not occupy a human question dialog. Default worker pools impose no local model process or CPU/RAM quota on that external agent.

## Live content and process lifetime

Conversation content, worker logs, context, task receipts and output buffers have bounded in-memory retention. Exiting or restarting the client discards that process's retained content. The client does not create conversation JSONL files, history archives, context-memory checkpoints or reload handoff files for this workflow. Old user files from earlier versions are neither read for recovery nor deleted.

A same-process network reconnect can retain the current draft, queue and frozen reference content. An unconfirmed delivery remains uncertain and is not automatically replayed. `outputs.read` only returns bytes still retained by the live output store; its cursors and truncation flags describe the actual retained range.

## Implementation references

The CLI entry is in [client.rs](../src/client.rs); [worker_window.rs](../src/client/worker_window.rs) manages the owned second window. The [queue](../src/worker/queue.rs) tracks task assignment and completion. The [worker tools](../src/worker/pool.rs) derive ownership from the authenticated call scope, and the [monitor](../src/client/terminal/worker.rs) consumes the status snapshot. Internal background, saved-session, and explicit local-executor paths retain their existing lifecycle.

This page describes the source contract. A build or test result applies only to the exact candidate on which it was run.

## Isolated window acceptance

The window acceptance script uses the built client, a private instance of the production account/router fixture, and private PTYs. Its terminal fixture returns immediately while a separate supervisor runs the real worker process. This exercises ownership and cleanup when the terminal launcher's lifetime is shorter than the worker's.

```sh
python3 scripts/e2e_worker_windows.py \
  --bin-dir /path/to/built/bin \
  --fixture-binary /path/to/ssmcp-tests \
  --decoder /path/to/vt_snapshot \
  --source-revision TESTED_COMMIT \
  --report /tmp/worker-windows.json
```

The script requires already-built executables and never invokes Cargo. It checks the two connections and the `/worker_pool` link, initial offline capacity, external workers becoming available through real waits, an editable parent chat, literal arguments and working directory, and a single child launch. A private clipboard recorder exercises invitation prompts and the full-pool refusal; resizing checks the actual request acknowledgement and preserves waiting station identities. The authenticated protocol covers manager attachment, task submission, claim retry, an actual private command, truthful progress, per-task timing, completion, cancellation acknowledgement, and result retrieval. Shrinking to zero exercises both idle and busy retirement, authenticated release and retry receipts; subsequent growth must create offline capacity without counting old collaboration memberships as available workers.

Lifecycle cases cover parent exit, parent `/reload`, worker-only closure, missing-terminal failure, and explicit headless mode. The same worker scenario also checks a visible Ctrl+R refusal while a claim is active, then a worker-only replacement after work is settled. It checks the unchanged parent connection and draft, preserved capacity and arguments, fresh child identity, and the existing normal-exit and terminal-restoration receipts. State and temporary directories are isolated inside the fixture. The test checks that coordination and cleanup leave no conversation or context-memory files. Its explicit report and terminal receipts are test artifacts. Each result records the tested executable hashes.

This Linux test uses a terminal fixture and does not exercise an installed desktop terminal. Its daemon case checks dispatch to the background-service path and the absence of an extra window; full user-systemd lifecycle coverage remains in [e2e_daemon.py](../scripts/e2e_daemon.py). A failed prerequisite exits with a blocked result; it is not counted as a passing test.
