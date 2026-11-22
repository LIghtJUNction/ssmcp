# Worker sessions

A worker session accepts tasks from authenticated agents and assigns each task to one idle execution slot. Start it with `--worker N`; its terminal shows **空闲中**, **正在工作**, **离线**, the total slot count, and the queued task count. It has no chat composer. Keyboard input, paste, layout changes, and a failed display cannot become input to a hidden shell.

## Start a pool

Start a remote pool with one command:

```sh
ssmcp --worker 2
ssmcp alice@relay.example --worker 10
```

Remote is the default on Linux, macOS and Windows. The pool holds a bounded task queue; authorized remote agents claim tasks, do the work through their own tools, and report completion. Starting the pool does not start a model, a local harness, or a task executable. No `--agent` or `--input` option is required. `--agent remote` is an explicit equivalent; the older worker spelling `--agent next-caller` is accepted as the same remote mode.

The foreground terminal is a monitor with per-slot state and bounded, scrollable logs. It has no chat input or human question dialog. For a background pool, use `ssmcp --worker 2 --daemon`; saved and daemon workers disable chat input internally. The advanced `--input none` spelling remains available for an explicitly headless foreground pool.

Interactive mode requires a working monitor. Closing it stops the connection. Worker mode does not combine with the conversation resume picker, external chat input modes, `--list-tools`, or `--load-harness`. Ordinary `ssmcp` remains the normal chat interface and does not start a worker pool.

## Use the monitor

Each worker has its own status panel and log view. Select a panel to inspect its retained activity:

| Control | Action |
| --- | --- |
| Left / Right | Select the previous or next worker. |
| Up / Down | Scroll the selected worker's log. |
| Page Up / Page Down | Scroll that log by a page. |
| Home | Show its earliest retained entry. |
| End | Return to the latest entries and follow new activity. |
| Ctrl+Home / Ctrl+End | Select the first or last worker. |
| Mouse click | Select the worker panel under the pointer. |
| Mouse wheel | Scroll the log in the panel under the pointer. |

Each worker retains up to 64 lifecycle entries within a 16 KiB log budget; an entry's text is limited to 1 KiB. Older entries drop out as new activity arrives, so Home does not recover discarded history. These logs show events such as assignment, cancellation and completion. Task text and results are excluded from the monitor and shared `worker.status` output; the submitting participant reads its result through `worker.result`.

## Optional local executors

Use a local executor only when you intend to run an installed, configured CLI on this machine:

```sh
ssmcp alice@relay.example --worker 10 --agent codex
ssmcp alice@relay.example --worker 4 --worker-k 0.5 --agent claude
```

Local execution supports `codex`, `claude`, `opencode`, and `exec`. It requires Linux, cgroup v2 with CPU and memory controllers, and a usable per-user systemd manager. Every local execution slot runs in its own owned scope. If the hard limits or executable are unavailable, slots remain offline with a reason; there is no unrestricted execution fallback. A forwarded `ssmcp` chat peer is not a worker executor.

For an executable that already processes a message, use an absolute path:

```sh
ssmcp alice@relay.example --worker 2 --daemon \
  --agent exec --agent-exec /absolute/path/to/task-executor
```

Optional `--agent-argv-file /absolute/path/to/argv.json` supplies a private JSON array of fixed arguments. SSMCP appends `--` and the task prompt as one argument; it does not evaluate a shell command. Normal remote workers never enter this executable path.

## Submit and observe tasks

Worker tools use the existing authenticated native call route. Bind a real collaboration participant to the target client before calling them; task arguments cannot supply an account, owner, slot, or connection identity.

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

const reply = await call(id, "ssmcp.worker.submit", {
  request_id: "review-issue-123-attempt-1",
  text: "Review @src/example.rs and report the concrete findings."
});
if (reply.isError) throw new Error(JSON.stringify(reply));
return reply.structuredContent;
```

Keep that participant's credentials and reuse the same binding for subsequent calls. Native calls return the ordinary MCP envelope: task data is under `structuredContent`, not directly on the outer reply.

| Tool | Arguments | Result |
| --- | --- | --- |
| `ssmcp.worker.status` | `{}` | The typed monitor snapshot, execution mode, resource status, and any cleanup error. |
| `ssmcp.worker.submit` | `{request_id, text}` | One task receipt, reference warnings, and the current worker snapshot. |
| `ssmcp.worker.result` | `{task_id, wait_ms?: 0..5000}` | That caller's task state and any confirmed output. |
| `ssmcp.worker.cancel` | `{task_id}` | A cancellation request; a running task remains occupied until completion or confirmed cleanup. |
| `ssmcp.worker.wait` | `{request_id, wait_ms?: 0..5000}` | Remote mode: wait for a claim, default 5000 ms; return `waiting`, `assigned`, or `already_completed`. |
| `ssmcp.worker.claim` | `{request_id}` | Remote mode: the same claim operation without waiting. |
| `ssmcp.worker.complete` | `{claim, status, output?, error?}` | Remote mode: report the result for the exact claim and its authenticated claimant. |

To wait once for a result, use `call(id, "ssmcp.worker.result", {task_id, wait_ms: 5000})` under the same verified binding. A timeout returns the current task state; it does not complete or re-submit the task.

Tasks keep the authenticated caller and original history owner captured at submission. Completion does not follow a later UI selection. File and skill references use local expansion; a missing file warns while retaining the literal message.

## Remote claim and completion

A remote executor must use its own authenticated collaboration binding on the same account and connection. An assignment includes prepared `text`, `cancel_requested`, `wait_hint`, and the complete `claim` object: `id`, `generation`, `slot`, `epoch`, `task_id`, and `attempt`. Keep that object unchanged. The submitter owns the original task; the claimant's identity comes from the authenticated call, not from a user-supplied field.

Repeat the same claim `request_id` while waiting or recovering its response. This returns the existing assignment rather than claiming a second task. After finishing it, use a new `request_id` to request another task. Only the original claimant can complete the assignment; a stale generation, altered claim, or another participant is rejected.

Use one completion form after performing the work:

- `status: "completed"` with the actual `output`, at most 64 KiB.
- `status: "failed"` with a nonempty `error`, at most 4 KiB.
- `status: "cancelled"` with neither output nor error.

A matching repeat of a retained completion is idempotent; changing its payload is rejected. Completion is identified as `authenticated_remote_agent`: it records what the remote agent reported, not an independent OS observation that an external process succeeded or stopped. Cancellation requests do not forcibly terminate an external agent or release an occupied slot before its report.

A closed connection between the worker client and its relay makes unconfirmed assigned work `unknown`; do not replay it automatically. SSMCP does not separately check whether each external agent is still connected. If an agent goes silent while the worker remains connected, its unconfirmed task continues to occupy the slot.

The default remote pool does not impose CPU or RAM limits on external agents. Its resource status reports external execution and no local OS quota. Active push transports and model APIs are not selected implicitly by a host or port.

## Queue, context, and local cancellation

An atomic FIFO claim assigns a task to exactly one idle slot. When all slots are occupied, up to 128 tasks can wait. Input text is limited to 16 KiB and expanded input to 48 KiB.

Each explicitly local slot has an independent SSMCP supervisor and context. It invokes the selected CLI for each task, rather than claiming a persistent provider-native thread. Context holds at most eight turns and 32 KiB and clears when the caller changes. Tasks can use different idle slots; callers must not depend on slot affinity.

Idle local supervisors block on notifications and pipes, without occupying human `ui.wait`, polling a model, or requiring a public `sleepp` tool. After a reply and confirmed cleanup, they wait again automatically. The local CLI adapter retains its 120-second task limit and 64 KiB output limit; idle waiting is indefinite. Remote agents use bounded `worker.wait` calls to receive assignments instead.

Queued cancellation prevents assignment. For a running local executor, cancellation keeps the slot busy until owned cleanup is confirmed. SSMCP stops the owned systemd scope and checks that its cgroup is empty; wrapper termination alone is insufficient. Cancellation cannot undo earlier effects.

An unconfirmed result becomes `unknown` and must not be replayed blindly. Identical `request_id` retries deduplicate within the same caller and connection. The queue keeps 128 terminal results, guards against replaying evicted unknown tasks, and refuses new work if that separate guard fills. Reconnection does not make uncertain effects safe to repeat.

## Local executor resource limits

These limits apply only to owned local executor processes, not to the default remote queue or its external agents. The default multiplier is `k = 1`; `--worker-k K` accepts only a finite positive value. The intended per-slot ceilings are:

```text
CPU ceiling = effective CPU capacity / registered slot count × k
RAM ceiling = effective memory capacity / registered slot count × k
```

The count includes live idle slots across SSMCP pools of the **current OS user**, not just this `--worker N`. Other users are outside that count. Effective capacity respects CPU affinity and inherited cgroup limits; transient free-memory readings do not replace it.

These values are ceilings, not reserved CPU time or guaranteed free memory. SSMCP rounds enforceable values down, disables swap for the scope, and limits its process count. A share below 1% of one CPU or 16 MiB of RAM is rejected. A multiplier above one requests oversubscription.

Before admitting more slots, SSMCP lowers and reads back existing idle scopes' limits. Busy tasks or an idle scope that cannot fit the lower memory ceiling defer admission, leaving the new slots offline. It does not force that reduction on a running model. Stricter ceilings may remain after a pool leaves or a partial admission fails; status exposes the registered limits.

These limits govern owned worker process trees; they are not an OS security sandbox. Unrelated developer shells, external model processes, and independently launched Zuse commands remain outside them.

## Implementation references

The CLI and lifecycle are in [client.rs](../src/client.rs). The [queue](../src/worker/queue.rs), [executor](../src/worker/executor.rs), [child protocol](../src/worker/child.rs), and [resource registry](../src/worker/registry.rs) keep assignment, execution, and cleanup separate. The [worker tools](../src/worker/pool.rs) derive task ownership from the existing authenticated call scope. The [monitor](../src/client/terminal/worker.rs) consumes the same typed snapshot used by the status tool.

This page describes the source contract. A build or test result applies only to the exact candidate on which it was run.
