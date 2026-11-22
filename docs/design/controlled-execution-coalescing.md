# Controlled execution savings: staged contract

Status: design plus unavailable-state UI only; no execution merging or numeric
counter is implemented. Reviewed main: `628960c74ca97fd29766c7271ac7f0bc959ed095`.

## Existing boundaries to reuse

- `Runtime::perform_inner` (`src/runtime.rs`) validates the registry and invokes
  hooks before `perform_registered`. Followers must retain individual checks.
- `src/backend/shell.rs` owns native jobs, output and semaphore admission. It
  accepts arbitrary commands, not a trusted read-only operation. Queuing and job
  retention do not prove a saved spawn.
- `src/client/git_status.rs` has a fixed local Git observer, with optional index
  locking, fsmonitor and maintenance disabled. It is not a remotely authorized
  native status operation. Its refresh/cache behavior is not execution savings.
- `Session::call_inner` and pending-call checks (`src/broker.rs`) hold actual
  authorization and cancellation state and recheck authority at delivery.
  Native `CallScope` has verified account/thread attribution, but not the full
  live authorization lease or final delivery status. Account equality is not
  sufficient visibility isolation. Runtime success alone cannot credit savings.
- Stats (`src/client/terminal/conversation/statistics.rs`) counts bounded audit
  records in this window. These are call outcomes, not native spawn receipts.

This increment adds only **Avoided command executions / unavailable · host metric
not implemented**. Old and disconnected snapshots also show unavailable. It
accepts no editable helper/provider counter and infers nothing from audit totals.
No registry, shell, hook, approval, identity or permission behavior changes.

## Controlled operation prerequisite

Proposed `ssmcp.repo.status` is a design name, **not registered in this revision**.
Version 1 accepts exactly `{}` and uses the existing permitted workspace root.
There is no caller shell, argv, executable, cwd, environment, stdin, format or
limit override. Unknown arguments fail; missing read permission remains denied.

Before enabling merging, implement one bounded native status process:

- Pin a trusted Git executable and canonical permitted worktree/Git metadata.
  Reject symlink escapes, external metadata/alternates and ungranted linked
  worktrees; do not widen filesystem access to support them.
- Build fixed porcelain-v2/NUL status options, fixed untracked mode and
  `--ignore-submodules=all`. Disable index refresh/optional locking, fsmonitor,
  hooks, filters, pager, automatic maintenance, lazy fetch and all transports.
  Audit config includes and filter execution before reusing the observer builder;
  recognizing `git status` in caller argv is never eligibility evidence.
- Freeze an allowlisted environment, removing Git routing/config/tracing and
  executable injection variables. Fixed locale, stdin null, bounded captured
  stdout/stderr, no PTY. Do not expose environment values in metrics.
- Bound runtime, output and group/member counts. Changed metadata/config,
  nonzero exit, timeout, incomplete output or parse failure fail explicitly.
  Never fall back to generic exec. Document the snapshot's observation interval.

## One host owner, one in-flight state

Keep the join map, member lifecycle and counter in the existing broker Session
execution lifecycle, where complete authorization and cancellation are available.
Invoke the controlled native operation once and require its host-produced spawn
receipt. Do not introduce a JS scheduler, a second native-host join map, a UI
counter, a generic framework, short result caching or automatic GC in this change.
The receipt is observation only and confers no authority. Unsupported peers run
independently and expose no numeric savings metric.

Admission requires the existing per-request authorization, registry and hook
checks. The canonical key must include all of:

| Dimension | Exact identity |
| --- | --- |
| Authority | Authenticated subject, owner/delegation and actual authorization/visibility lease generation; never display names, caller JSON or logged tokens |
| Destination | Selected client and live connection generation |
| Visibility | Verified session/thread/agent and effective read-grant generation; initially no cross-thread sharing |
| Operation | Fixed packaged route, registry generation, canonical validated arguments and contract version |
| Workspace | Pinned permitted cwd/worktree/Git metadata and config identity |
| Execution | Executable identity, fixed options, full environment policy and bounded noninteractive I/O contract |

Missing identity disables merging. Changed keys do not join. Recheck authority
and route before dispatch and each member completion. `run_id` and attribution
alone are not authorization leases. Generic exec and dynamic providers cannot
enter this map. Per-request IDs identify members, not groups.

Join only an actually running execution, not queued or completed work. Remove
its group from the map before completion publication; old cleanup cannot remove
a newer same-key group. Each member has an independent cancellation lease.
Cancelling the initiator must not cancel followers. All-cancel kills/reaps the
owned process without savings. Disconnect/revoke closes only the affected scope.
Existing idempotency replay runs before admission and earns no new member credit.

## Counter invariant and projection

For a group with exactly one confirmed successful native spawn, let S contain
only distinct, non-replayed members whose complete successful shared result is
accepted at the host's authorized completion boundary while uncancelled:

`avoided_spawns(group) = max(0, |S| - 1)`

Serialize cancellation against acceptance; increment only once per extra accepted
member. The first accepted member consumes the one actual-spawn baseline even
when the initiator cancelled. No join-time increments or rollback counters.
Native Runtime completion or writing a reply to a pipe alone is insufficient;
reuse broker delivery validation. Ambiguous delivery undercounts, never guesses.

| Outcome | Saved spawns |
| --- | ---: |
| 10 same-key accepted successes, one native execution | 9 |
| One cancelled member, 9 accepted successes | 8 |
| Initiator cancelled, 9 accepted followers | 8 |
| Failure, all-cancel, or only one accepted success | 0 |
| Queuing, retry, replay or completed-cache reuse | 0 |
| Different authority, arguments or execution contract | No cross-key savings |

Project only the host total through the existing connected-session Stats snapshot:
versioned schema, unsigned `avoided_spawns`, `unit: avoided_native_process_spawns`,
`scope: client_connection`, opaque connection epoch. Reset on reconnect/restart;
never sum different epochs. No command, path, arguments, secrets or account labels.
Keep this total independent of bounded audit retention. UI only renders this
summary; it never repeats admission or counting rules. Unsupported/missing,
malformed, unknown-schema, disconnected or stale-epoch data is **unavailable**.
Only a supported authoritative zero may display 0. Label the limited controlled
operation coverage and connection scope. The producer/projection is future work.

## Implementation acceptance gate (not yet run)

Use deterministic admission/spawn/exit/completion barriers, not sleep races:

1. Ten requests: prove one actual native spawn and nine credits through Runtime
   and authenticated broker fixtures; a call after completion starts a new group.
2. Vary every key dimension, especially two grants for the same account, two
   subjects, client reconnect, thread, cwd/config and environment/I/O. No join.
3. Cancel initiator/follower at every boundary; others succeed, cancelled members
   receive no credit. All-cancel reaps the process; no orphaned state remains.
4. Cover spawn/exit/parse/output/timeout failure, revoke-before-delivery,
   ambiguous/duplicate completion, retry and replay. No inflated counter.
5. Assert no index writes, hook/filter/fsmonitor execution, lazy fetch, external
   metadata reads or network; no arbitrary shell/argv eligibility.
6. Render supported 0/9, old backend, malformed/unknown snapshot, disconnect and
   new epoch. Preserve narrow-screen layout, drafts, audit totals and approvals.
   Fixtures do not substitute for live-client or model-understanding E2E.
