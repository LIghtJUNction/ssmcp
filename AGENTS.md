# Repository instructions

## Required user reply and wait

For a chat-enabled SSMCP session, deliver every user-facing progress update and answer through `say` / `ssmcp.ui.message`, or use `ssmcp.ui.reply` for the exact authenticated thread input. Text emitted only inside an external assistant tag is not a delivered SSMCP reply and must not replace these tools.

After delivering an answer, call the current authenticated `ui.wait` helper and wait for a real user response. Preserve the returned wait/request identity across observation windows. A timeout means no reply was received; it is not confirmation. Only the final outer receipt with the matching user messages establishes ordinary input delivery. If ending the task, follow the currently advertised finish-decision contract and its exact consumed human decision. Do not invent a response or start a model to supply one.

During an assignment, use `ssmcp.worker.report` with the exact claim to tell the user your first check, then report milestones and blockers as they occur. During longer work, give a verified observation within 60 seconds when able. Use collaboration messages for questions the coordinator needs to answer. After completing the claim, return to `worker.wait`.

## Returning to work and requesting voluntary offline

A main or worker with new human work or a claim returns to `working` immediately. Wait for the real idle deadline (default five minutes) before Dreaming. Verify 30 continuous minutes of the same Dreaming episode before asking to go offline. The default 30-second maintenance action has its own budget. Keep a single pending request and block quietly; after a Human No wait a full new 30 minutes before asking again. Only the explicit local Human Yes for this member and episode permits voluntary offline. Main uses its verified finish receipt. Workers use the coordinator's specifically addressed human question and original answer reference, remaining available if that evidence is missing. Do not bypass this policy with retirement, release, leave or lease expiration. See [worker lifecycle](docs/workers.md#voluntary-offline-requests).

## Project workspaces

Use the current project's `.ssmcp/workspace/` for SSMCP-created temporary jobs and review worktrees. Create a managed Git checkout with `ssmcp worktree create` from the project root and retain its ID. Release it with `ssmcp worktree release ID` after all work and observers have finished. Project cleanup runs at startup, periodically and on reload. Use `ssmcp cleanup --dry-run` to see what is eligible. Keep source edits in the repository and keep recovery copies in `.ssmcp/recovery/`. [Workspace management](docs/workspaces.md)

## Generated artifacts

Commit source code, text reports, manifests, and test inputs. Store generated screenshots, recordings, archives, compiled binaries, and acceptance bundles in Google Drive or GitHub Release assets; do not add them to Git, including with `git add -f`.

Text evidence records should include the source commit, exact checks performed, artifact hashes, and external artifact location. Existing required runtime assets are source inputs and must not be deleted as generated evidence.

After a coordinated history cleanup, create new work from the cleaned branch. Transfer unpublished changes as reviewed text patches or new commits on that branch; do not merge or push the old ancestry back into the repository.

Generated documentation media under `docs/assets`, `docs/qa`, and `docs/reports` follow the same external-storage rule. Preserve `src/portal/brand-mark.png`, which is an actual runtime input; a file's binary format alone is not grounds for deletion. Keep historical build commits and artifact hashes truthful when Git history changes; use a reviewed source-equivalence mapping instead of rewriting old release manifests.

## Main branch workflow

Use `main` as the only ongoing development branch. Commit reviewed source changes and push them directly to `main` in small steps, then continue local validation and corrective commits on `main`. Routine iterations do not require a feature branch or pull request.

Preserve unrelated uncommitted work. When an existing checkout is busy or dirty, use a detached worktree based on the current `origin/main`. Push with a normal fast-forward, and reconcile any new upstream commits without overwriting another contributor's work. Remove this task's temporary local branches only after confirming that `main` contains their commits.

Local tests remain the acceptance gate. A source push does not mean that a release was published or a running client or server was updated.
