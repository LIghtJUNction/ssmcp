# Native capability boundary

ssmcp uses two ownership layers. The connection kernel decides whether a call
may run and how its result is observed. Capability plugins own the state that
performs the operation and cleans up its resources.

The boundary is implemented in [Runtime](../src/runtime.rs), the
[native host](../src/runtime/native_host.rs), and its
[versioned ABI](../src/runtime/native_host/abi.rs).

| Layer | Owns |
| --- | --- |
| Kernel | Authenticated connection, one live registry, route generations, call hooks, cancellation, audit correlation, output retention and result delivery |
| Plugins | Tool declarations, workspace and context state, desktop state, terminals and command jobs, optional harness drivers, operation execution and cleanup |

The packaged plugins are `files`, `desktop`, and `ssmcp.process`. They retain the
existing tool names and schemas. Moving a tool does not grant new filesystem
roots, desktop consent, account access or permission to start an optional
harness. The registry remains the authority for availability: a plugin may own
an optional provider without registering any of its tools until an explicit
load succeeds.

## Packaged plugin ABI

Each packaged plugin supplies a static zplugin descriptor. The host loads it
with `Plugin::from_static` and binds `dev.ssmcp.capability`, version 1. Execution
crosses that interface; it does not call back into a Runtime provider match.

| Method | Contract |
| --- | --- |
| `catalog() -> OwnedBytes` | Return JSON schema version 1 with the plugin ID and complete tool declarations grouped by owned provider. This starts no asynchronous work. |
| `start(request_id, native_name, input) -> OwnedTask<()>` | Copy the JSON request into plugin ownership and create a task. Input carries the exact registered ID, provider label, generation and original arguments. It contains no controller credentials. |
| `finish(request_id) -> OwnedBytes` | Consume one completed result. Its versioned JSON envelope distinguishes a native `Result` error from an unchanged MCP result, including `isError`, images and opaque metadata. A second read fails. |
| `shutdown(request_id) -> OwnedTask<()>` | Run the plugin's actual cleanup after its in-flight calls have been cancelled and dropped. Retrieve the cleanup outcome through `finish`. |

The input byte slice ends when `start` returns. A task retains its own input,
plugin state and result slot. It may outlive its creating ABI proxy. Cancelling
or releasing an unfinished task drops the native future and removes the pending
slot. The host consumes completed result bytes even when the caller disappears
at completion, so a disconnected receiver cannot retain abandoned results.

These builtin calls keep their existing native input and output budgets. The
64 KiB input and 256 KiB output limits of the older synchronous dynamic Extension
interface are not applied to them. The kernel still performs its original
output capture and spill handling after the plugin returns.

## Ownership and scheduling

zplugin descriptors, instances and tasks are deliberately not `Send`. One lazy
executor per NativeHost keeps all of them on their creating thread. Its LocalSet
polls independent tasks concurrently; waiting for one command, browser response
or file operation does not serialize unrelated calls. There is no permanent
thread per packaged plugin. Catalog enumeration uses a temporary ABI instance
on the calling thread and does not start the executor.

Concrete plugin state is constructed from the authorized root, context roots,
shared registry and shared output store. A `Definition` owns an `Arc` to that
state. During a linked static factory call, a short thread-local construction
scope gives the factory that concrete allocation. This is an internal Rust
ownership handoff, not a C ABI argument or a callback into the kernel. Local
attachment, completion and desktop-consent methods share the same plugin state.

The host queue retains a small route stamp instead of another copy of the tool
schemas. It validates the stamp before admission, after queueing and immediately
before ABI dispatch. Request IDs and harness audit observations cross the local
queue as separate Rust metadata and are restored for task polling, cancellation
and destruction. They are not serialized into plugin arguments.

Admission retains at most 128 queued or active native calls per host. This is
separate from the existing connection and capability-specific concurrency limits.

Shutdown stops admission independently of queue capacity, cancels and drains
all active ABI tasks, then runs the plugins' cleanup concurrently. The executor
releases ABI objects and stops its reactor before reporting completion. Cleanup
errors remain errors; creating a plugin boundary does not make a failed native
cleanup successful or roll back an operation that already took effect.

## Existing dynamic extensions

Explicitly loaded libraries retain the existing
`dev.project-z.zharness.extension` interface and trusted-code loading policy.
Their synchronous calls cannot be forcibly interrupted. They keep their bounded
workers and existing size limits; they do not run synchronously on the shared
async builtin executor. No external zplugin source or submodule pin is changed
by the packaged capability boundary.

## Verification

The focused `runtime::native_host::tests` suite checks lazy enumeration, one
executor shared by multiple plugins, concurrent tasks in one plugin, a `Send`
public host future, audit context across the thread boundary, cancellation,
shutdown ordering, route-generation changes, exact large MCP results, and
single-consumption ABI result slots. It supplements the existing native
workspace, context, desktop, command-process and real account-transport gates;
it does not replace those capability tests with mocks.

The [contract tests](../src/runtime/native_host/tests.rs) are kept with the host.
