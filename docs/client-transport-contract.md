# Shared client transport

SSMCP frontends reuse the running CLI through its existing zbridge adapter. The CLI retains session identity, authorization, input leases, worker claims and command execution. A frontend stores a small current projection and asks the core for explicit state or operations. The shared observer supplies bounded event observation, and `ssmcp gui serve` provides a paired HTTP gateway to the same native core. Platform windows and application packages have separate hosts. [Adapter](../src/session/attach/zbridge_adapter.rs) · [Remote connection](../src/session/attach/zbridge_adapter/remote.rs) · [Gateway contract](gui-gateway.md)

## Verified source baseline

The source inventory on 2026-11-23 found the following checkouts. Commits identify the inspected source, not a deployed build. Existing changes in the primary SSMCP checkout were preserved; this work uses a separate detached worktree.

| Component | Observed source | Existing implementation |
| --- | --- | --- |
| SSMCP | `3f2cacc985670b950f29fd840c3684ca224e05e7`, primary branch `main` | CLI, portal/public renderer, local attach service and zbridge adapter; unrelated server upgrade changes were already uncommitted |
| zbridge | `1cfeb5b1f11c26c35cf7456c9b8b60d1f2d5d8de`, clean `main` and matching SSMCP submodule | Rust v1 protocol, bounded codecs, authorization-aware dispatch and decimal-string counters |
| zgpui | observed `origin/main` `d34c41f79ddab343e7468c5192885a82ca487687`; clean detached checkout `zgpui-zbridge-20261122` | Native Linux/Windows/macOS hosts; optional `BridgeModel` consuming a typed transport callback |

An exact-name search of non-hidden directories under the developer home, through depth six, found no `ssmcp-web`, `ssmcp-desktop`, `ssmcp-android`, `ssmcp-ios` or `ssmcp-mobile` checkout. The inspected SSMCP source manifests did not define Android or iOS apps. The existing portal renderer is useful web infrastructure, but it is not evidence that an interactive CLI web frontend has been implemented. [Public renderer](../src/portal/frontend.rs) · [GUI bridge scope](gui-bridge.md)

The machine has Rust/Cargo 1.100.0, Node 26.13.1, Java 21, Android SDK platforms 35/36/37.0, NDK 30.0.16248370, cached Gradle distributions, `adb`, MinGW and Xvfb. Installed Rust targets include Linux, Windows GNU, WebAssembly and `x86_64-linux-android`. Swift, Xcode, Flutter, Dart and Wine were absent from PATH; no iOS or Android ARM64 Rust target was installed. These are toolchain observations only. No platform package, window or device behavior was validated by this transport change.

## One authorized connection

Native desktop clients can use `RemoteFrontend::connect(directory, read_only)` for an existing private attach endpoint. An embedded host can obtain `Frontend` from `Handle::frontend`. Both issue a real attach lease, and neither accepts caller-supplied account or agent identity. Closing the frontend releases its lease without stopping the session. A writable frontend must be an explicit user choice; observer capabilities do not advertise commands. [Connection and lease rules](gui-bridge.md#连接与授权)

A browser connects through `ssmcp gui serve`, which owns an authorized `RemoteFrontend` and forwards bounded zbridge frames. The private descriptor and native lease stay in the CLI process. The gateway listens on loopback, serves optional same-origin assets and requires a separate process-scoped pairing bearer for every API request. Android and iOS may use an explicitly configured HTTPS reverse proxy and the same pairing contract. Each platform still owns its view and lifecycle behavior. [CLI binding](../src/session/gui.rs) · [HTTP implementation and limits](gui-gateway.md)

Start every connection with `capabilities`, retain the complete opaque `generation`, and use only advertised resources and commands. A generation change requires capability discovery and fresh state. Preserve any uncertain command record with the old generation; reconnecting never turns it into a command for the new backend.

## Available state and remaining product work

| Client requirement | Current bridge contract | Frontend behavior |
| --- | --- | --- |
| Connection and core presence | `runtime`, empty query; includes `backend_active`, native capabilities and `live_cursor` | Display observed presence and preserve the returned decimal cursor |
| Session selection | The paired HTTP gateway projects actual `session::list()` results through `/api/gui/v1/sessions`; native clients can select an existing private attach directory | Display actual manager and connection state separately; explicitly select a saved session before opening a lease |
| Terminal state | `terminals`, empty query, only when a Runtime is bound | Render the actual returned list |
| Task and worker state | `workers`, `offset` and `limit` 1–32, only when a pool exists | Use real offline/idle/working/retiring counts, stable slot/epoch, task IDs and native result semantics |
| Conversation and messages | The inspected baseline had no conversation/message state resource or send command; any added core projection is discovered from live capabilities | Enable the feature only when the connected core advertises and authorizes it; gateway routing does not invent a separate conversation API |
| Event observation | zbridge `events`; one protocol wait is 0–1,000 ms | Use `EventObserver`, directly or through `/connections/{id}/events`, to compose a frontend window up to 55,000 ms |
| Explicit control | `terminal.write`, `terminal.resize`, and configured `workers.resize` under a write lease | Submit once following a user action; preserve the native receipt |

These resources and commands come from the real capability and dispatch implementation. Observation does not claim next-caller input, acknowledge messages, complete tasks or acquire manager authority. [Capability catalog and dispatch](../src/session/attach/zbridge_adapter.rs)

Several existing interfaces serve different purposes and should retain those boundaries. `session::list()` enumerates the current OS user's saved sessions; native session-control tools are enabled explicitly with `--session-control`. The portal's `/account/sessions` is authenticated server-rendered HTML, and `/account/history` displays management audit events. Neither is a conversation JSON API. [Session core](../src/session.rs) · [Session control](../src/session/control.rs) · [Portal routes](../src/portal/dashboard.rs)

Native `ssmcp.ui.events` reads a bounded live process log tied to the real call's client and verified thread selector. The old `ssmcp.ui.history` family is retired in this source. `ssmcp.ui.message` displays authenticated agent progress or answers to the human; it is not a general human-message submission endpoint. A product conversation view and composer need an explicit core adapter for those roles, with the existing ownership checks intact. [Live-log implementation](../src/client/interface/history.rs) · [UI dispatch](../src/client/interface.rs) · [Agent message handler](../src/client/interface/messages.rs)

For a state-and-events view, capture `runtime.live_cursor` before loading additional state, then start observation from that cursor. Events arriving during state reads remain recoverable. Keep separate cursors for separate backends and journals. In particular, attach zbridge pages project the attach log using `ssmcp.<kind>` topics; the relay's account event journal has a different authorization and cursor contract. They must not be combined by comparing their numeric positions.

## Bounded observation and consumption

`EventObserver` accepts the existing typed transport callback. The generation is fixed for its lifetime; each exchange checks the real zbridge response association, event order and byte limits. It retains at most one event page. It has no disk log, command queue, connection factory or model launcher. [Observer implementation](../src/session/attach/zbridge_adapter/observation.rs)

```rust
use ssmcp::session::attach::{EventObserver, RemoteFrontend};
use zbridge::{BridgeError, EventPage, Id};

async fn observe_once(
    frontend: &RemoteFrontend,
    observer: &mut EventObserver,
) -> Result<EventPage, BridgeError> {
    let page = observer.wait_with(
        Id::new("view-42")?, 32, 55_000,
        |request| frontend.request(request),
    ).await?;
    Ok(page.clone())
}
```

The caller creates the observer after capability discovery, using `EventObserver::new(generation, cursor)`. Each invocation receives a new request ID, at most 124 bytes; the helper appends a round number to correlate its sequential reads. A positive observation window includes transport time and is capped at 55 seconds. A zero window makes one nonwaiting protocol read with a two-second transport deadline. Each underlying event request still respects the current v1 maximum of one second. The zbridge protocol and dependency are unchanged.

Apply a returned event page to the frontend, then call `advance()` to commit its next cursor locally. Until that explicit step, another read cannot replace a page containing events, a retention gap or more retained data. `advance()` sends no server acknowledgment and has no effect on worker claims. Empty reads can retain their confirmed head while waiting again. The retained floor, `start_cursor`, can precede a live read; it is not used as the live resume position when the first delivered event occurs later.

Dropping an in-flight observation preserves its last confirmed cursor. A transport error leaves the generation unchanged and does not issue a reconnect or retry. After reconnect, rediscover capabilities: resume with the saved cursor only if the complete generation still matches. If a response reports `retention_gap`, refresh current state and reconcile missing history before advancing. Counter values remain canonical decimal strings on JSON boundaries, including values beyond JavaScript's exact integer range.

An observation that reaches its deadline after accepted empty reads can return the last confirmed empty page. Its cursor is safe for the next read; this does not claim the transport is currently healthy. If no page was accepted, the helper returns `Unavailable`. Early empty replies are paced so that they cannot create a busy request loop.

## Command uncertainty

The observer emits only `events` requests. Explicit commands continue to use `RemoteFrontend::request` or the existing zgpui `BridgeModel`; those paths preserve the original request, command and generation. A successful receipt does not imply an asynchronous worker task has finished. An interrupted command marked `effect: "may_have_occurred"` remains unresolved until its native state or task record establishes the outcome. The same request ID alone does not make resending safe. [Remote error handling](../src/session/attach/zbridge_adapter/remote.rs) · [Command contract](gui-bridge.md#明确写入命令)

## Shared fixtures and verification

[contract-v1.json](../src/session/attach/zbridge_adapter/observation/contract-v1.json) contains language-neutral examples for observer capabilities, a cursor above 2^53, a retained-history gap and an uncertain command outcome. These are explicitly synthetic protocol inputs, with no credentials or live-session identifiers. Native, browser and mobile implementations can consume the same fixtures while retaining their own authorized transport.

The focused Rust tests decode those fixtures with the actual zbridge codecs. They also exercise segmented live observation, a retained floor older than the live cursor, explicit page consumption, cancellation, deadlines, pacing, rejected authority, generation mismatch and request bounds. The regular repository entry point is:

```sh
flock /var/tmp/project-z-cargo-target/.project-z-cargo-owner.lock \
  cargo test --locked --lib session::attach::zbridge_adapter::observation::tests
```

The gateway has a separate [language-neutral HTTP fixture](../src/session/gui/gateway/contract-v1.json), real loopback HTTP tests and a private attach IPC round-trip test. Its [verification entry point](gui-gateway.md#边界与验证) covers the CLI transport integration. Protocol and gateway tests do not establish Linux/Windows window behavior, Android lifecycle, iOS builds, signing or device acceptance.
