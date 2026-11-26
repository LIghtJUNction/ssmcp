# Unsupervised native control process

Opt in with `ssmcp alice@relay.example --daemon --control-only`.
The account and relay are ordinary connection arguments. Existing local controller
admission and per-request approval still apply; this option grants no additional
identity, tool or filesystem permissions. Review accounts with `ssmcp controllers`.

This combination keeps one process in the caller's process tree, with no terminal
required. Stdin is unused (EOF is harmless); stdout emits one JSON connection
record after registration. Diagnostics go to stderr. SIGINT or SIGTERM stops
admission, cancels outstanding calls, shuts down owned native processes and reaps
the transport child. Relay EOF, rejection, protocol error or failed authorization
at registration exits nonzero. A denied individual request returns an error;
other authorized requests may continue. There is no automatic reconnect or replay.
The startup registration deadline is 120 seconds per existing handshake phase.

To explicitly put it in the background on a Unix shell:

```sh
ssmcp alice@relay.example --daemon --control-only </dev/null >control.log 2>control.err &
control_pid=$!
# Later, from the same shell, with the retained PID:
kill -TERM "$control_pid"
wait "$control_pid"
```

The binary does not fork, install a service, write a PID file, enable startup or
survive logout by itself. An existing service manager can invoke the same command
as a simple process with null stdin and captured stdout/stderr, and owns restart
and logout policy. No service manager is installed by this mode. On Linux, the first authorized
command start uses the existing session resource-admission code, which can ask
an already running user manager for transient resource scopes/slices. This is
resource accounting, not persistent session or service installation.
Use connection registration output to distinguish readiness from a live PID.
SIGKILL or machine failure cannot promise graceful cleanup.

## Capability boundary

The initial minimal catalog contains ordinary `ssmcp.exec.*`
(excluding `ssmcp.exec.events`) and `ssmcp.client.capabilities`. These are the same
native implementations, bounds and owned process cleanup used by the normal
client. Command/job status, output cursors, authenticated attribution and request
IDs are necessary native control state. Optional harnesses/plugins cannot be
loaded through this mode. Interactive terminal selection, filesystem, desktop, network forwarding, saved-session
management, reload and resource tools are outside this initial catalog; use
ordinary authorized process control within its existing permissions as needed.

There is no frontend Runtime, TUI, chat ingress, agent launch, worker pool,
workflow lifecycle, repository watcher, event subscription or event forwarding.
Only explicit native requests run. Native process implementations may retain
bounded diagnostic/output records internally; this is not an event dispatcher.
Code-mode completion sidecars are validated and discarded without dispatch.
Unexpected workflow/event frames fail the connection. Transport authentication,
local controller approval and bounded request correlation remain required.
The sole relay-maintenance exception is an empty capability-manifest read;
it grants no control and cannot execute a tool.

Do not combine this mode with chat/agent flags, worker pools, harness preload,
shell selection, TTY observation, context roots, session-control, background resource-limit flags
or reconnect configuration. Additional constraints belong to the caller's
existing service manager. No startup work or command is configured implicitly.

## Compatibility

`--daemon` alone retains its existing native managed-session behavior, including
its platform service manager integration. Existing saved sessions, 007 recovery
and the foreground default are unchanged. `--control-only` requires `--daemon`;
it is not silently enabled by lack of a terminal or by `--input none`.

## Isolated acceptance

On Linux, execution retains the existing delegated cgroup memory requirement.
An existing user manager must support the normal SSMCP resource admission.
Admission occurs only on an authorized command-start request. This mode does
not install/start a manager or disable command memory ceilings. Without the
required user bus and delegation, registration and read-only process controls
work, but native command starts fail closed.

Build the CLI and library test executable with the locked toolchain, then run:

```sh
cargo test --locked --lib client::control_only::tests
cargo build --locked --bin ssmcp
cargo test --locked --lib --no-run
# Use the executable path printed by the preceding command.
SSMCP_E2E_FIXTURE_BINARY=/absolute/path/to/the/library-test-executable \
  python3 scripts/e2e_control_only.py --bin-dir target/debug
```

The Python fixture uses loopback and disposable account/controller state. It
checks the real CLI without a TTY, native control, local revocation, catalog
exclusions, signals and disconnection. Execution and owned-command cleanup
require the existing resource delegation; missing prerequisites are reported
as blockers, never as successful command execution. Library tests separately
exercise native execution using the existing library-test resource policy.
Default parsing and offline discovery are checked without requiring a manager.
It never connects to a live relay, installs a service or invokes a model.
On a capable Linux host, native command admission uses the existing user manager.
Use `--require-execution` to make missing execution prerequisites exit with code 2.
