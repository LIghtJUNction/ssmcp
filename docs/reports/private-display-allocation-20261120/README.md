# Private display allocation and OpenCode teardown — v0.10.14

The final default library gate passed: **1512 passed, 0 failed, 21 ignored**, with two test threads and exit status 0. The allocator also passed real private Xvfb conflict, concurrency, input, capture and cleanup checks. OpenCode cancellation releases its HTTP stream while a private archive lock remains held, then retains the received partial bytes when that lock is released. [Full receipt](validation.json)

## Source and artifact binding

| Role | Immutable source |
| --- | --- |
| Real desktop and account execution | 3298bfb98497e5d47661d94bcfec2ea39f006dbb |
| Final product build and strict Clippy | 6d9a522195a402ab05d1575150db18315b4e37c1 |
| Final test build and complete default gate | 0a6615d0c325dfb364c9005c01a4a63fefa5fad7 |
| OpenCode six-case targeted execution | 6f26b8e50197468d88da701e7b16c467dd23453e |
| Account evidence commit | 6e0931796da3ad7092a41722115d0f094dd15b4e |

The final two source changes affect test assertions only. The build receipt compares every changed file before its test module and verifies the unchanged cleanup implementation. The CLI, daemon, frontend, worker integration executable and native plugin fixture are the frozen 6d9a522 artifacts; the final library test executable was rebuilt from 0a6615d. Their provenance is explicit in validation.json.

The desktop, account, authorization and Python fixture sources are identical between 3298bfb and the final candidate. Their blob IDs are recorded in validation.json, so the prior real desktop and account results retain their precise execution origin.

## Private Xvfb ownership

The allocator requests an explicit display in 10000–29999 and makes at most 64 candidate attempts. A stable per-UID private lock serializes cooperating allocators. Candidate checks reserve both XN and XN_ filesystem and abstract endpoints and existing server lock names; the host display is excluded.

Readiness must report the exact requested number. The new X server must be a live child of the managed guardian, with the same UID, a live pidfd and matching filesystem/abstract socket peer credentials. Cleanup holds an inode reference and removes only the socket inode proved to belong to that managed instance. Cancellation retains the allocation reservation until its process tree has been reaped. The child environment removes inherited WAYLAND_SOCKET as well as WAYLAND_DISPLAY.

The Python desktop fixtures use the same UID reservation protocol, explicit displays in 30000–49999, a private cookie, strict readiness and socket ownership checks. They pass -nopn and preserve unrelated occupied or aliased endpoints. The tested fixtures use owned X servers and owned applications.

The installed X server was xorg-server-xvfb 21.1.26-1. The matching [official 21.1.26 source archive](https://www.x.org/releases/individual/xserver/xorg-server-21.1.26.tar.xz) was inspected: an explicit display uses the single-number branch in os/connection.c; the low-number scan belongs to the non-explicit branch. The source also documents the displayfd no-lock behavior and -nopn handling. [Archive and source hashes](xserver-source.json)

The repository's Rust minimum is 1.100; the existing serial rustc wrapper and one-job policy were retained.

## Verification

| Check | Result | Executed source |
| --- | --- | --- |
| Package formatting and strict all-target Clippy | Passed | 6d9a522 |
| CLI, daemon, frontend, library tests, worker fixture and native cdylib build | Passed | 6d9a522 |
| Computer unit group | 41 passed; 8 ignored | 3298bfb |
| Real private input/capture and application lifecycle tests | 2 passed | 3298bfb |
| Real concurrent allocation, conflict, alias and cleanup tests | 2 passed | 3298bfb |
| Embedded worker-entry integration test | Passed | 3298bfb |
| Authenticated MCP → Computer → native library path | 34 checks passed | 3298bfb |
| Native CLI worker | 27 checks passed | 3298bfb |
| Native daemon worker | 27 checks passed | 3298bfb |
| Official v13 and observed v12 ordinary-client compatibility | 41 checks passed | 3298bfb |
| Modern account lifecycle | 47 checks passed | 3298bfb |
| OpenCode cancellation and lifecycle targets | 6 passed | 6f26b8e |
| Task-panel real refresh and unchanged-snapshot targets | Passed | 0a6615d |
| Complete default library suite, two threads | 1512 passed; 0 failed; 21 ignored | 0a6615d |

The default suite keeps its ignored cases visible. The four real display tests and worker-entry test above were executed separately; the ignored real OpenCode external-service case was not run. No model request was needed for these checks.

The MCP checks exercise native typing, click, bounded image output and the actual Computer dispatch after an unsupported key. The worker checks cover cancellation during drag and Unicode typing, button release, keyboard-map restoration, fixed display environments and child exit. [Exact check lists](desktop-checks.json)

## OpenCode cancellation

The previous complete gate exposed an attached SSE stream that remained open during archive cleanup. A payload-free system-call trace reached the first shutdown after ten fixture requests: nine short connections had closed and the SSE connection remained open. Its destructor then waited 9.097509 seconds for the output cleanup lock. The trace also amplified directory scanning; it was stopped at its 45-second diagnostic limit and is recorded as an interrupted diagnostic.

Cancellation now transfers only archival state to blocking cleanup. The Response and stream task remain in the async teardown path. Explicit shutdown aborts and joins stream tasks without holding their map lock, waits for their partial-archive records and then closes the event archive. Drop retains that same cleanup ownership. If the runtime discards a queued blocking closure, the deferred work moves to an independent thread.

Four private-lock cases verify explicit shutdown, Drop, cancellation of shutdown itself and Drop outside the runtime context. Each requires TCP EOF and an async heartbeat before unlocking its own output directory. After release, the owner-checked artifact stat remains incomplete with the expected error, and the raw received SSE bytes match exactly. A fifth regression checks that queued archival work survives runtime shutdown exactly once. The original attachment lifecycle test also passes. [Targeted receipts](opencode-checks.json)

This change targets cancellation cleanup. Active-stream capture and normal EOF processing retain their existing storage path.

## Earlier results retained

[Prior receipts](prior-results.json) preserve the allocator's initial E0509 compile error and its fix, the missing MCP fixture environment variable and corrected continuation, the 3298bfb full-suite OpenCode failure, and the interrupted system-call trace.

The pre-allocator 1501/1/18 full-suite failure and its same-binary exact replay remain in the [legacy compatibility report](../legacy-account-self-20261120/validation-status.json). The final complete gate above closes the earlier OpenCode failure without changing its timeout or forcing its fixture connection closed.

The first new OpenCode assertion incorrectly expected arbitrary error text in an audited event. Audit projection deliberately omits that text; the corrected test also reads the owner-checked artifact metadata and verifies its full partial-stream error. Product behavior stayed unchanged for this correction.

The next complete suite passed all OpenCode tests and exposed a task-panel fixture that required every successful refresh to request a redraw. The second native snapshot can be identical within a clock tick. The corrected fixture retains the initial and completed-state redraw assertions, requires refresh consumption and valid native detail, and keeps the existing identical-snapshot/mouse-target regression. Its previous failing full run and passing exact replay remain recorded.

## Frozen artifacts

| Artifact | SHA-256 | Built from |
| --- | --- | --- |
| ssmcp | 230b57f9e8d36b31c775eb45010e7bb2f7f1635b9025bd0c82fcadd5afd7525f | 6d9a522 |
| ssmcpd | 56294dfaf69631cd923197cdb820ee59a5428d44802c072fe14c6532ba9987e4 | 6d9a522 |
| native_plugin_fixture | f432324f95b5a3de32bf5c82e81a6fadd017fd97071ceab6c9e81f7202b6858a | 6d9a522 |
| ssmcp-frontend | f6e937a5c2584a97cf357c6040934d102ff3a21b85a9b59db208ff6310ca2e13 | 6d9a522 |
| zuse_worker_entry | a388f9d689446771ef6f61a7e6bbf648f59f4a9b8ae98b9e68691efa3a88fe01 | 6d9a522 |
| libtest | 5cb460a9afef5f0b9ce2d04a6fcdf0a8ce0e4d76deb520705854351cc1512352 | 0a6615d |

## Reproduction

Use a clean checkout of the listed source with recursive submodules and the repository's configured serial Rust wrapper. The original full product checks were:

    cargo fmt --package ssmcp -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --locked --no-run --lib --test zuse_worker_entry
    cargo build --locked --bins --example native_plugin_fixture

The final library executable was then run with --test-threads=2. Run each real allocator test explicitly with --exact and --ignored. The precise test names, commands, source commits, exit codes and log hashes are in validation.json. The private desktop scripts use scripts/private_xvfb.py.

## Review of the evidence

| Review dimension | Evidence and scope |
| --- | --- |
| Change significance | Actual display ownership and a reproduced subscription teardown failure are addressed. |
| Clarity | Product builds, test-only updates and earlier execution origins have separate immutable identities. |
| Experimental strength | Owned native X servers and real loopback TCP streams exercise the relevant behavior. |
| Completeness | The full default gate, explicit native tests and ignored-case boundaries are recorded. |
| Design soundness | Network ownership is separate from disk finalization; endpoint cleanup is tied to proved process and inode ownership. |

No live client or service was restarted by this validation. This report records local acceptance; release tagging and deployment remain separately coordinated.
