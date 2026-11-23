# Local machine overview

`ssmcp overview` reports this operating-system user's registered local sessions.
It connects to their existing private attach endpoints. It does not scan ports,
guess process names, connect to another account or start an agent.

```sh
ssmcp overview
ssmcp overview --watch
ssmcp overview --json
ssmcp overview --watch --json
```

The ordinary view is a compact table. JSON includes the current connection ID,
backend generation, agent page, worker counts, native job summary and missing
resource names. Watch mode refreshes after each bounded observation, with a
two-second interval. An unavailable endpoint does not block every other session.

## Select one actual session

Foreground processes publish a private registration when they start. A normal
restart is needed for an older running binary to appear. Saved sessions are
discovered from their validated manifests, including definitions whose attach
endpoint is currently unavailable. The inventory contains up to 256 entries;
the JSON reports truncation and invalid entries rather than silently treating
an incomplete inventory as complete.

The exact target is `live:INSTANCE` for a foreground process or `saved:NAME` for
a saved session. Two sessions may share a display name. Their targets remain
distinct. A process reconnect keeps its foreground target; a new process gets
a new instance. The target alone is not authority: connecting also requires the
current user's protected endpoint descriptor, secret and actual lease.

```sh
ssmcp overview show live:0123456789abcdef0123456789abcdef
ssmcp overview jobs saved:workstation
ssmcp overview state saved:workstation workers '{"offset":32,"limit":32}'
ssmcp overview state saved:workstation threads '{"offset":32,"limit":32}'
```

Agent and worker detail pages contain at most 32 entries. Their `has_more` or
`next_offset` describes the next page. Worker totals come from the actual pool
snapshot; an agent count with `+` in the table describes only the displayed
page. `state` reads another advertised page without obtaining a write lease.
Native execution summaries retain at most 32 rows and expose their retained
count. They omit argv, working directories, output bodies and artifact paths.
The connection's main PTY is marked as infrastructure, not a submitted command.

## Read measurements in their actual scope

Linux process measurements come from `/proc/PID/stat`. RSS is the client
process's resident memory in bytes. CPU is a difference between two samples:
100 percent means one fully used CPU core. The first CPU sample is null; a PID
or process start-time change starts a new sample. These values exclude child
processes and do not estimate an agent's token usage or model compute.

Unsupported or unavailable measurements remain null. Native completed job CPU
and peak RSS, when available, retain their own units and measurement scope.
PTY jobs do not acquire invented resource usage. Offline, connecting,
unavailable and unknown states are distinct from a verified live backend.

## Submit a command to one session

```sh
ssmcp overview exec saved:workstation -- git status --short
ssmcp overview jobs saved:workstation
ssmcp overview cancel saved:workstation JOB_ID
```

`exec` forwards the explicit argument vector to `ssmcp.exec.start` in the
selected runtime. It uses the existing registry, hooks, workspace policy,
process ownership and job receipts. Detaching the overview does not cancel the
job. Cancellation requires its exact returned job ID. The implicit main PTY
cannot be cancelled through this command.

An existing frontend may hold the exclusive input lease. In that case another
controller must wait for its release. An overview observation never takes that
lease. `command TARGET COMMAND JSON_OBJECT` is also available for commands
advertised by the selected bridge, including explicit terminal input and pool
resize. It has the same lease and generation checks.

A received start receipt confirms native admission, not command completion.
Inspect `jobs` for the actual state and exit code. A timeout or lost response
after dispatch is reported as unknown, with the original request ID and target.
The CLI never retries the command. Inspect the original registry or attach to
that session before deciding what to do next. The same request ID is correlation,
not a promise that execution is idempotent.

## Implementation and validation

Discovery is in [overview/discovery.rs](../src/session/overview/discovery.rs).
The command path uses the existing
[private attach bridge](../src/session/attach/zbridge_adapter/execution.rs).
The collector neither reads endpoint credentials into its JSON output nor
reconstructs authenticated agent identities from display labels.

[e2e_overview.py](../scripts/e2e_overview.py) starts two owned Linux local
clients with the same label and an isolated home. It checks distinct targets,
actual PIDs and memory, exact native dispatch, read-only rejection and stale
generation rejection. A separately labelled protocol fixture drops one command
receipt to verify unknown outcomes and absence of automatic replay. This fixture
does not claim to execute a native command. The test never changes installed
services, real account credentials or existing user sessions.
