# Worker sessions

A worker session accepts tasks from authenticated agents and assigns each task to one idle execution slot. Start it with `--worker N`; its terminal shows **空闲中**, **正在工作**, **离线**, the total slot count, and the queued task count. It has no chat composer. Keyboard input, paste, layout changes, and a failed display cannot become input to a hidden shell.

## Start a pool

Worker execution currently requires Linux, cgroup v2 with CPU and memory controllers, and a usable per-user systemd manager. Every execution slot runs in its own owned scope. If the hard limits or the executor are unavailable, slots remain offline with a reason; there is no unrestricted execution fallback.

Choose an installed, configured CLI explicitly:

```sh
ssmcp alice@relay.example --worker 10 --agent codex
ssmcp alice@relay.example --worker 4 --worker-k 0.5 --agent claude
```

The supported local executors are `codex`, `claude`, `opencode`, and `exec`. The usual default, `next-caller`, does not execute a model; a pool started without a local executor remains offline. A forwarded `ssmcp` peer is also not a locally limited executor.

For an executable that already processes a message, use an absolute path:

```sh
ssmcp alice@relay.example --worker 2 --input none \
  --agent exec --agent-exec /absolute/path/to/task-executor
```

Optional `--agent-argv-file /absolute/path/to/argv.json` supplies a private JSON array of fixed arguments. SSMCP appends `--` and the task prompt as one argument; it does not evaluate a shell command.

Use `--input none` for a headless pool or a saved background worker session. Interactive worker mode requires a working monitor and input attachment; closing that monitor stops the connection and cleans up its owned scopes. Worker mode does not combine with the conversation resume picker, other chat input modes, `--list-tools`, or `--load-harness`.

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
| `ssmcp.worker.status` | `{}` | The typed monitor snapshot, per-user resource registration, and any cleanup error. |
| `ssmcp.worker.submit` | `{request_id, text}` | One task receipt, reference warnings, and the current worker snapshot. |
| `ssmcp.worker.result` | `{task_id, wait_ms?: 0..5000}` | That caller's task state and any confirmed output. |
| `ssmcp.worker.cancel` | `{task_id}` | A cancellation receipt; a running task can still be cleaning up. |

To wait once for a result, use `call(id, "ssmcp.worker.result", {task_id, wait_ms: 5000})` under the same verified binding. A timeout returns the current task state; it does not complete or re-submit the task.

Tasks keep the authenticated caller and original history owner captured at submission. Completion does not follow a later UI selection. File and skill references use local expansion; a missing file warns while retaining the literal message.

## Queue, context, and cancellation

An atomic FIFO claim assigns a task to exactly one idle slot. When all slots are occupied, up to 128 tasks can wait. Input text is limited to 16 KiB and expanded input to 48 KiB.

Each slot has an independent SSMCP supervisor and context. It invokes the selected CLI for each task, rather than claiming a persistent provider-native thread. Context holds at most eight turns and 32 KiB and clears when the caller changes. Tasks can use different idle slots; callers must not depend on slot affinity.

Idle supervisors block on local notifications and pipes, without occupying human `ui.wait`, polling a model, or requiring a public `sleepp` tool. After a reply and confirmed cleanup, they wait again automatically. The CLI adapter retains its 120-second task limit and 64 KiB output limit; idle waiting is indefinite.

Queued cancellation prevents execution. Running cancellation keeps the slot busy until owned cleanup is confirmed. SSMCP stops the owned systemd scope and checks that its cgroup is empty; wrapper termination alone is insufficient. Cancellation cannot undo earlier effects.

An unconfirmed result becomes `unknown` and must not be replayed blindly. Identical `request_id` retries deduplicate within the same caller and connection. The queue keeps 128 terminal results, guards against replaying evicted unknown tasks, and refuses new work if that separate guard fills. Reconnection does not make uncertain effects safe to repeat.

## Resource limits

The default multiplier is `k = 1`; `--worker-k K` accepts only a finite positive value. The intended per-slot ceilings are:

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
