# Zuse input and local-watch integration

This iteration keeps desktop input behind SSMCP's existing local host-input
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

## Local validation

Use the existing shared Cargo-owner outer lock when running these commands on the
developer machine. The lockfile must reference published rolling Git source;
temporary local path overrides used during development are not publication locks.

```sh
cargo test --lib computer:: -- --test-threads=1
cargo test --lib computer::zuse_backend::browser_tests::owned_chromium_watch_uses_host_ticket_and_stops_after_off \
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

Source changes, published locks and runtime installation are recorded separately.
No binary, browser profile, screenshot or private message belongs in this commit.
