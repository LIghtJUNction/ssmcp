# RFC: SSMCP guard and worker isolation

Status: proposed. The #199 prototype currently enforces the documented Linux
native file mutation policy. The identities and sandbox below are design
requirements for later stages.

## Threat model and trust boundary

Safety limits mistakes by an authorized agent. Examples include file and
resource limits, explicit persistent outputs, task-private temporary storage
and lease-checked reclamation. Security controls which identity may inspect,
modify, approve or dispatch, including when an authenticated agent sends an
unexpected request or a worker is compromised.

Trust the host kernel, administrator, policy loader, guard binary and protected
identity store. Treat the project tree, agent messages, worker processes, tool
parameters and tool output as untrusted inputs. A compromised guard, kernel or
administrator is outside this model. Management operations and runtime effects
are separate capabilities. Remote login or a collaboration role string alone
does not grant workspace or policy access.

The guard runs as a dedicated OS identity. Its service starts independently
from workers, with an administrator-owned executable, configuration and service
definition. A worker starts as a separate identity. It has no guard group
membership, writable guard ancestor, guard credential, inherited privileged
descriptor, or authority to start or replace the guard. Validate UID, GID,
supplementary groups, descriptor inheritance, filesystem mounts and IPC peer
identity before admitting work. User and device IDs are verified at the
authenticated relay and carried to the guard in host-owned metadata. The guard
checks the exact session and operation before side effects.

Ordinary agents see an empty read-only mount for `.ssmcp/safety`. The real
directory is reachable only by the guard, administrator and a separately
authenticated policy-edit interface with that scope. The editing interface can
submit project policy changes to the guard for validation and atomic publication.
It cannot change the user baseline or grant its own privileges. The audit reader
continues to use a separately authorized, redacted stream. Revoking audit access
also ends live and resumed reads.

Every agent's execution view, including the policy editor's, has read-only
access to Git metadata. Protect the pointer or directory, resolved gitdir and
common-dir for normal repos, linked worktrees and submodules. Resolve and pin
identities before granting writable mounts, and account for overlapping roots
and alternate paths to the same object. If the topology cannot be verified,
deny writes for that mount. A future Git write broker would have a separate
explicit operation allowlist and approval scope. Existing host-side fetch is
outside agent execution and requires its own review before reuse.

## Approval and failure behavior

On first connection the relay presents an authenticated agent and proved device
identity to the operator. Approval binds account, agent, device key, session,
operation scope, expiry and policy revision. Reuse is short-lived and limited to
that tuple. Revocation advances an authorization generation and invalidates old
leases, pending dispatch and resumed reads. Recheck immediately before side
effects. Already completed effects and bytes already delivered cannot be
recalled. A specific high-risk action additionally binds single-use approval
to the normalized original request and immutable policy revision. No self
approval, authority by text, or fallback to a broader principal.

A YubiKey touch at guard startup is a separate proposal for local operator
presence. Review enrollment, recovery, timeouts and the privileged startup
sequence. Absence or failure blocks startup when this mode is selected. Any
alternative needs explicit operator configuration and a recorded choice.
Startup presence does not replace remote connection or per-operation approval.

Missing identity, stale approval, failed guard startup, unresolved mount
identity or unsupported isolation denies controlled dispatch. A rejected policy
reload retains the last valid complete snapshot. An unavailable configured
policy on cold start denies controlled operations. Audit-sink loss must be
observable. A future controlled execution profile that requires durable audit
must refuse operations if that sink cannot commit the admission record.

## Implementation and experiments

1. **Native preflight and evidence.** Retain the current zguard rule matrix and
   atomic snapshots. Verify malformed, unavailable and unsupported rule
   sources against synthetic files. Record verdict, operation, rule revision and
   outcome without raw parameters. Current prototype covers four native
   mutations and a read-only local evidence queue. Its rules-only changes do
   not create filesystem confidentiality or cross-process sandboxing.
2. **Guard identity and execution isolation.** Prototype under a disposable
   supported Linux runner with dedicated guard and worker users. Verify the
   startup chain, empty policy mount, Git metadata protection and ordinary
   project writes for both main and independent worker sessions. Include
   normal, linked and submodule Git fixtures. Verify all supported native
   file, transfer, shell, PTY and worker entry points converge on the boundary
   or explicitly refuse the controlled profile. Current cloud Bubblewrap UID
   setup cannot qualify this stage.
3. **Approval lifecycle and controlled writes.** Use an owned local relay to
   record first connect, in-scope reuse, expiry, scope mismatch, revocation
   during a pending request and refusal after reconnect. Validate identity
   bindings before separately evaluating hardware startup presence and any Git
   write broker. Rule edits should test denial, atomic publication and revision
   binding as independent cases.
4. **Temporary resource lifecycle.** Reuse the managed-file and job artifact
   mechanisms. Verify process-private temporary/cache mounts, capacity, TTL and
   teardown after completion, reload and restart. Active leases, explicit
   retention and unknown files survive. Compare logical byte accounting with
   the actual volume limit. Report only reclaim attributable to registered
   temporary objects.

Use an independent expected-verdict table, balanced allow/deny/approval cases,
and fresh generated workspaces. Publish exact commits, OS/kernel, toolchain,
sandbox profile, fixture seeds, skipped cases and raw aggregate measurements.
For correctness count allowed operations with expected effects, denied
operations with zero effects, and leaked or missing audit records. Report false
allow and false refusal with their denominators, separately per endpoint and
role. Run profile-off, guard+memory audit, and guard+isolated execution on
matched fixtures. Measure admission separately from full tool completion,
with cold/reload and warmed runs, at least 1,000 warmups and 10,000 samples per
configuration where practical. Report P50/P95, sample counts and repeat-run
variation. Record peak RSS separately for guard, worker and harness, plus
retained audit bytes at a fixed record count. Compare zguard NoAudit on the
same executor as a separate microbenchmark. Record unsupported stages as
unverified. Use harmless synthetic writes, role checks and private loopback
relays. Keep test evidence free of real credentials and user paths.
