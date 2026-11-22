# Worker sessions

`ssmcp --worker 3` keeps the current window as the main chat and opens a second window with three worker slots. Use the main window to talk to the coordinating agent; use the worker window to inspect task states and per-slot logs. Each window has its own client connection and operating-system process.

The default worker is an external agent. It actively connects to the worker window, registers with `role: "worker"`, and calls `ssmcp.worker.wait` to wait for a task. After executing an assigned task, it reports the result with `ssmcp.worker.complete` and waits again. Starting the pool creates task capacity without starting any model.

## Start a pool

Start the main chat and its worker window with one command:

```sh
ssmcp --worker 3
ssmcp alice@relay.example --worker 10
```

The worker window shows the real state of each station, the total station count, and queued work. All stations begin **offline**. An external worker registers and actively waits before its station becomes **idle**; an assigned task makes it **working**. The monitor has no chat composer. Keyboard input and paste cannot become input to a hidden shell. Ordinary `ssmcp` opens only the main chat.

The same external-worker workflow applies on Linux, macOS and Windows. Each worker uses its own existing agent application and tools.

The foreground command needs an available terminal adapter for its second window. To select a terminal explicitly, set `SSMCP_WORKER_TERMINAL` to one executable path. The value is a literal path, including any spaces; it is not a shell command or a list of options.

```sh
SSMCP_WORKER_TERMINAL=/absolute/path/to/xterm ssmcp --worker 3
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

Invitation text appears in a local, scrollable panel, and the client also tries to copy it to the clipboard. A missing clipboard does not prevent reading the prompt. When the pool is fully staffed, `/invite worker` displays `Worker stations are fully staffed.` and does not generate another invitation. Sending a prompt to an external agent does not itself connect that agent or start a model.

Shrinking a pool preserves in-flight claims and cancellation acknowledgements. A station being removed while its task is still running remains **working**. Once it has no running task but still awaits retirement confirmation, it is **retiring**. The physical station count is `idle + working + offline + retiring`; `desired_total` describes the requested capacity while retirement is pending.

Capacity can range from zero to 128 stations. While a resize is awaiting retirement acknowledgements, only a repeat of the same target is accepted. The parent projection's `last_resize` identifies the actual request, target, acceptance and any error; sending a window-control message alone does not prove that the pool accepted it. A pool with zero stations remains connected and can grow again.

## Connect the external workers

A worker is an external agent that actively joins the worker connection and waits for a task. Keep each agent's existing conversation open while it waits. SSMCP supplies the task queue and tools; the external application supplies the model.

1. Start `ssmcp --worker 3`, use `/invite`, and connect the coordinating agent to the main chat.
2. Use `/invite worker` for each external agent. Each joins the linked worker connection once with `role: "worker"` and keeps its returned credentials private.
3. Each worker binds its own credentials and calls `ssmcp.worker.wait`. A wait timeout leaves it available; it calls the wait tool again.
4. The main agent submits tasks. A waiting worker receives one authenticated claim, performs the task, and reports it with `ssmcp.worker.complete`.
5. After completion, the worker requests another task with a new wait request ID.

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

Reuse the same request ID while waiting or recovering a lost response. The result contains `waiting`, `assigned`, `already_completed`, `retiring`, `released`, or `not_accepting`. A wait timeout leaves the agent available. Use the complete claim unchanged when reporting a result, then use a new request ID to wait again. An assigned task can carry both `cancel_requested` and `retire_requested`. Finish that claim before requesting its retirement receipt; the old wait ID continues to return its completed-task receipt. For `retiring`, acknowledge the exact `retirement` object with `ssmcp.worker.release`. Stop waiting after release succeeds or the status is `released`.

Registered worker participants and idle stations are separate counts. Three newly created stations are offline until workers actually connect and wait. Read the actual station binding from the pool; an agent's display name, such as `worker#3`, does not select panel `#03`.

## Use the monitor

Select a station to inspect its retained activity. The left column scrolls through its real work log; the right column refreshes its current summary and measurements in fixed positions. Scrolling logs does not scroll the status column.

| Control | Action |
| --- | --- |
| Left / Right | Select the previous or next worker. |
| Up / Down | Scroll the selected worker's log. |
| Page Up / Page Down | Scroll that log by a page. |
| Home | Show its earliest retained entry. |
| End | Return to the latest entries and follow new activity. |
| Ctrl+Home / Ctrl+End | Select the first or last worker. |
| Mouse click | Select a station from the station navigation. |
| Mouse wheel | Scroll the selected station's log when the pointer is in the log area. |

Each station retains up to 64 log entries within a 16 KiB budget; an entry's text is limited to 1 KiB. Older entries drop out as new activity arrives, so Home only reaches the earliest retained entry. The monitor shows task summaries and work attributed to that station. A missing command or progress report remains explicitly unavailable. Full task results remain accessible through the authenticated `worker.result` workflow.

The status column shows current and accumulated work time and the last three completed task durations when those observations are available. Resource readings identify the process they measure. An external model has no implied local PID, so its CPU and memory use cannot be inferred from the worker window. Local client measurements, their bounded line chart, and the CPU heat value refer to the worker client process and are labelled accordingly; unavailable samples remain unavailable.

The monitor shows the worker process's OS PID and its bound manager identity when space permits. Full client and thread IDs wrap across rows on larger screens. Smaller screens use a compact binding status or omit these rows to preserve the task, log and measurement areas. The main window's Statistics page provides a scrollable view of both windows' IDs and the linked pool's counts. These displays observe identity without choosing a message recipient or acquiring management rights.

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
| `ssmcp.worker.tasks` | `{cursor?, limit?: 1..128}` | Main agent only: a bounded page of task IDs and lifecycle metadata, with `next_cursor`; default limit 32. It excludes task text, output, and error bodies. |
| `ssmcp.worker.wait_available` | `{after?: {generation, revision}, wait_ms?: 0..5000}` | Attached main agent only: observe capacity or wait for its revision to change; omit `after` for an immediate snapshot. |
| `ssmcp.worker.submit` | `{request_id, text}` | One task receipt, reference warnings, and the current worker snapshot. |
| `ssmcp.worker.result` | `{task_id, wait_ms?: 0..5000}` | That caller's task state and any confirmed output. |
| `ssmcp.worker.cancel` | `{task_id}` | A cancellation request; a running task remains occupied until completion or confirmed cleanup. |
| `ssmcp.worker.stop` | `{}` | Main agent only: stop accepting new tasks, cancel queued work, and request cancellation of running work while retaining its occupied slots. |
| `ssmcp.worker.resize` | `{total: 0..128}` | Attached main agent only, remote mode: change desired capacity. Added stations are offline; occupied stations await completion and retirement acknowledgement. |
| `ssmcp.worker.wait` | `{request_id, wait_ms?: 0..5000}` | Remote mode: wait for a claim, default 5000 ms; return a waiting, assignment, completion, retirement, release or nonaccepting status. |
| `ssmcp.worker.claim` | `{request_id}` | Remote mode: the same claim operation without waiting. |
| `ssmcp.worker.complete` | `{claim, status, output?, error?}` | Remote mode: report the result for the exact claim and its authenticated claimant. |
| `ssmcp.worker.report` | `{claim, request_id, event, command?, progress?, detail?}` | Record actual command or progress observations for the claimant's active task; return `recorded`, duplicate status, and cancellation or retirement flags. |
| `ssmcp.worker.release` | `{retirement}` | Acknowledge the exact retirement receipt with its authenticated worker; an active task must finish first. |

To wait once for a result, use `call(id, "ssmcp.worker.result", {task_id, wait_ms: 5000})` under the same verified binding. A timeout returns the current task state; it does not complete or re-submit the task.

Tasks keep the authenticated caller and original live-thread owner captured at submission. Completion does not follow a later UI selection. File and skill references use local expansion; a missing file warns while retaining the literal message.

The snapshot exposes `manager` and `stopping`. An unbound pool has `manager: null`. A bound manager includes the actual `client_id`, verified `thread_id` and participant identity, connection, and binding time. The snapshot's `process_id` is the local operating-system PID; it is separate from the session's client ID and the main agent's logical thread ID.

`stop` returns `stop_requested: true` and whether this call changed the state. Repeating it is safe. Its `external_execution_stopped: false` explicitly means that requesting a stop does not prove an external model or process has stopped. Running remote tasks remain occupied with cancellation requested until their original claimant acknowledges completion; their slots then go offline. A real connection loss continues to use the existing `unknown` outcome and must not trigger automatic replay.

## Observe capacity changes

`ssmcp.worker.wait_available` observes the pool under its existing manager binding. It does not attach a new manager, register a worker, start a model, reserve a station or consume human input. Successful worker-tool replies to that manager can also include the same `availability` projection alongside their original result.

The projection separates usable idle capacity from offline and retiring stations. It includes `total`, `desired_total`, `registered_workers`, `idle`, `working`, `offline`, `retiring` and `queued`. In remote mode, idle stations require actual worker registration; an empty station is offline. A busy station selected for removal remains working until completion, then remains retiring until release. Released stations are excluded from `total`, and surviving `slot` IDs and `epoch` values are not renumbered. Counts describe the pool's recorded state, not a heartbeat from an external model.

`accepting` reports whether a new valid task can enter the queue. A ready remote pool may accept queued work while all stations are offline; this does not make any worker available. Zero capacity, all-retiring stations, a full queue, stopping, closure or a guard that blocks new submissions makes it false. `idle_slots` contains observed eligible `{slot, epoch}` pairs only while the pool is accepting. These pairs are observations, so another task can take an idle station before the manager submits its next task.

A cursor waits for a capacity revision, not for a log entry or progress update. Omit `after` to read immediately; otherwise pass the returned `generation` and `revision` with `wait_ms` from 0 to 5000, default 5000. The result status is `current`, `changed`, `unchanged` or `not_accepting`; `not_accepting` takes precedence when the pool cannot admit work. The separate `changed` flag compares the supplied revision with the current one. A cursor from another generation or ahead of the current revision is rejected. Discover the new pool and read a fresh cursor after reconnecting.

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

When a station is selected for removal, its original worker receives a retirement receipt through `worker.wait`. Keep that object unchanged. `worker.release` refuses an active claim and only removes the station after the owning worker acknowledges retirement. Repeating an already accepted release is safe while its receipt is retained. The worker can then leave its collaboration membership and stop waiting; no external model or operating-system process is forcibly terminated by this acknowledgement.

A closed connection between the worker client and its relay makes unconfirmed assigned work `unknown`; do not replay it automatically. An agent that goes silent has not confirmed completion or cancellation. Its in-flight task continues to occupy its station until the task's lifecycle resolves; an idle-status display must not be used as evidence that external effects have stopped.

The default remote pool does not impose CPU or RAM limits on external agents. Its resource status reports external execution and no local OS quota. Active push transports and model APIs are not selected implicitly by a host or port.

## Task queue and cancellation

An atomic FIFO claim assigns a task to exactly one idle slot. When all slots are occupied, up to 128 tasks can wait. Input text is limited to 16 KiB and expanded input to 48 KiB. Workers follow their assigned task and authenticated claim.

Queued cancellation prevents assignment. Cancellation of an assigned remote task keeps its slot occupied until the original worker acknowledges completion. The worker checks `cancel_requested` by repeating its wait request with the same request ID, stops its own work, and reports `status: "cancelled"` with the original claim.

A worker client disconnect makes unconfirmed assigned work `unknown`. Preserve that outcome and inspect any effects before retrying. Identical submission requests deduplicate within the same caller and connection; terminal results have bounded retention.

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

Lifecycle cases cover parent exit, parent `/reload`, worker-only closure, missing-terminal failure, and explicit headless mode. State and temporary directories are isolated inside the fixture. The test checks that coordination and cleanup leave no conversation or context-memory files. Its explicit report and terminal receipts are test artifacts. Each result records the tested executable hashes.

This Linux test uses a terminal fixture and does not exercise an installed desktop terminal. Its daemon case checks dispatch to the background-service path and the absence of an extra window; full user-systemd lifecycle coverage remains in [e2e_daemon.py](../scripts/e2e_daemon.py). A failed prerequisite exits with a blocked result; it is not counted as a passing test.
