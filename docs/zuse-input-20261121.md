# Historical Zuse input and local-watch integration

> This is a historical implementation record from before the standalone Zuse CLI migration. The SSMCP desktop/browser actions, default Super+Alt+C shortcut and internal worker described below have been removed. All SSMCP computer-control switches and their compatibility entry points have also been removed. SSMCP now provides only four Zuse CLI lifecycle tools. Their examples and validation results do not describe the current implementation or establish results for the current candidate. Use [Zuse CLI lifecycle](computer-use.md) and [desktop/browser workflows](browser-desktop.md) for current instructions. The original record is preserved below.

This iteration keeps desktop input behind SSMCP's local Computer Use control
switch. It adds complete timed pointer paths, explicit keyboard-state operations,
window-bound click checks and a bounded existing-tab browser watch. The candidate
has not been installed into the operator's running host session.

## Existing-tab appearance clicks

`ssmcp.computer.watch_until_click` requires all of these explicit arguments:

```json
{
  "desktop": "host",
  "endpoint": "http://127.0.0.1:9222",
  "tab_id": "the-observed-existing-page-target-id",
  "scope": "#board",
  "selector": ".ready",
  "expected_origin": "http://127.0.0.1:8000",
  "max_clicks": 1,
  "timeout_ms": 5000
}
```

Use the actual selected page ID and its observed origin. The operation does not
create, activate or navigate a tab and does not move the operating system pointer.
It binds one document, tab, origin, container and unique target. A target visible
across many mutations is one episode; disappearance followed by reappearance may
produce another click up to `max_clicks` (1–64) and `timeout_ms` (1–60000).

The endpoint must be an explicit HTTP or WebSocket loopback browser-debugging
endpoint with a nonzero port. `expected_origin` must be the exact observed HTTP(S)
origin without a path, query, credentials or fragment. Only simple compound target
selectors are accepted. Ambiguity, scope/container removal, navigation, focus loss,
occlusion, cancellation or revoked input authorization ends the watch.

The worker holds the original host-input Ticket. Each press/release send batch
acquires a shared file gate; `/computer off` takes the exclusive gate and changes
the authorization generation. Once off returns, that original Ticket cannot send
another batch. No file gate is held while waiting for an appearance or a browser
reply. Worker stdin EOF wakes the same cancellation token used during connection
and observation.

Connection discovery, handshake and attachment are cancellable. Sends and release
acknowledgement reads are bounded; cancellation cleanup only reads the original
release response. It never retries input. An input attempt without that response
is explicitly unconfirmed, including a partially written first frame whose count
of completely written mouse events is still zero. Structured error receipts are
preserved so completed clicks and cleanup status remain reviewable.

DOM checking and input are separate CDP commands. This protocol does not make a
renderer change and the next input atomic. A successful write is distinct from a
trusted application click receipt; inspect both dispatch and received timings.

## Desktop paths, keys and retained coordinates

`computer.move_path` accepts one bounded trajectory and schedules all frames
locally, including the exact endpoint. The existing emergency toggle and
`/computer off` revoke future frames. A worker opened before a binary upgrade
must finish its versioned startup handshake before it can send input; Linux can
start the still-running executable through `/proc/self/exe` when its old pathname
has been removed.

`computer.keyboard_state` separates the current Hyprland keyboard from other
devices. Ordinary text input cannot toggle NumLock. Explicit
`computer.num_lock` takes `state` equal to `on`, `off` or `toggle` and checks native
readback; unavailable device mapping or permissions remain visible errors.

On Linux, a click can bind `window_id`, `coordinate_space` (`desktop` or `window`),
and the observed `expected_window` geometry/scale. The window must retain identity,
focus, geometry and scale immediately before button input. A change rejects the
old coordinates rather than silently selecting another window.

## Unified control and isolated workers

A mutation captures its Permit at the public request entry. An isolated worker
inherits that original Permit's Ticket, including its generation and state-file
gate. A private helper Computer does not create a new authorization for an
already admitted task. Turning control off and then on therefore leaves the old
task and worker revoked. Read-only operations do not request an input Ticket.

The shared state-file lock covers only each input dispatch or publication check.
Off changes the state under the corresponding exclusive lock. Waiting for an
event, a trajectory deadline or a browser acknowledgement leaves that state-file
lock available. The separate host-input lease still serializes an owned input
operation and its release cleanup. Isolated cancellation also waits for the
original worker's registered cleanup before returning.

Shutdown rejects a closed Computer at the public entry, preserving the existing
`computer connection is closed` error before checking authorization. Later closed
checks remain in place for shutdown races. Closing one Computer preserves the
shared control switch's previous On/Off value.

The concurrent main-line integration also exposed a large browser dispatch
future: the unchanged main-line commit reproduced a stack overflow in the queued
action deadline test. The inner transport future now lives on the heap before
Permit wrapping. A 64 KiB bound on the public future and the original 25-second
queued-action test both pass with the default thread stack. Authorization,
session binding, deadlines and cancellation still use their original paths.

## Local validation

Use the existing shared Cargo-owner outer lock when running these commands on the
developer machine. The lockfile must reference published rolling Git source;
temporary local path overrides used during development are not publication locks.

```sh
cargo test --lib computer:: -- --test-threads=1
cargo test --lib computer::zuse_backend::browser_tests::owned_chromium_watch_uses_host_ticket_and_stops_after_off \
  -- --ignored --exact --nocapture --test-threads=1
cargo test --lib computer::isolated::tests::private_pointer_trajectory_reaches_its_endpoint \
  -- --ignored --exact --nocapture --test-threads=1
```

The native browser regression owns its sandboxed headless Chromium profile and
loopback fixture. It uses a temporary unit-test HostControl store, not the user's
input switch. It checks the complete `Computer::perform` → Ticket → worker →
trusted-click path, then revokes the same Ticket before another appearance. The
ordinary tests cover worker upgrade/unlink, trajectory cancellation, cross-process
dispatch locking, stale window geometry and keyboard preflight classification.

The completed local candidate run passed 88 computer-module tests, with 13 explicit
native/subprocess ignores. The separately invoked owned-Chromium full-chain test
passed, and `cargo clippy --lib -- -D warnings` passed with the candidate Zuse
source override. The same 88 tests, owned-Chromium test and Clippy check also
passed against the published Git dependency
`ac5713c2bfc3cc4848e89107cc1a6929ec48e2dc`. The temporary path override is not
retained in Cargo.lock. Git dependency manifests have no version/rev/tag selector;
the lock records the exact validated source. The bundled Zuse gitlink matches it.

### Final main-line integration

The final source checkpoint is `769ab956a2d838c2c039e5abf75a953ad58bf609`.
It includes main-line `4b4cbff311e930a8c8280ff1989a7f036ce7a892` and the later
published source/fixture updates through
`64738751cb59af9ef0d467ce34c50f091d2f6fa4`. The exact-source full library run
passed **2,105 tests**, with **26 explicit ignores**, using
`cargo test --lib --locked --no-fail-fast -- --test-threads=1` and the default
thread stack. The two separately invoked native tests above also passed: the
owned Chromium watch in 1.38 seconds and the private Xvfb trajectory in 3.95
seconds. These are test durations, not input-latency measurements.
`cargo clippy --all-targets --locked -- -D warnings` and
`cargo fmt --all -- --check` also passed at that exact source checkpoint.

The earlier full parallel run aborted at the browser stack overflow and had
reported two RPC fixture failures before aborting; its buffered assertions were
not emitted. Both RPC fixtures passed individually and in the final complete
run. No specific cause is attributed to that first pair of failures. A clean
checkout of main-line `4b4cbff` reproduced the browser overflow with its original
lockfile; the heap-boundary repair passed the original default-stack test.

A subsequent full run exposed the shutdown error-priority regression and the
existing Playwright EOF fixture timeout. The closed-entry check repairs the former;
the latter uses the deterministic invalid-JSON fixture already published in
`6473875`. The final complete run includes both corrections.

The full-library test executable observed during that run had SHA256
`6c0bbd59f1fd780bfca37cd7686da3a18a1246b8fd7377bfbf77162ff2d2f2ca`.
It is a regenerable executable in the shared Cargo target, not a published
release asset. Command receipts and complete logs are retained in the isolated
worktree's ignored `dist/final-source-gate.json` and `dist/final-source-gate-*.log`.

Source changes, published locks and runtime installation are recorded separately.
No binary, browser profile, screenshot or private message belongs in this commit.
