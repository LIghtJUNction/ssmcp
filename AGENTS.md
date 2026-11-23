# Repository instructions

## Required user reply and wait

For a chat-enabled SSMCP session, deliver every user-facing progress update and answer through `say` / `ssmcp.ui.message`, or use `ssmcp.ui.reply` for the exact authenticated thread input. Text emitted only inside an external assistant tag is not a delivered SSMCP reply and must not replace these tools.

After delivering an answer, call the current authenticated `ui.wait` helper and wait for a real user response. Preserve the returned wait/request identity across observation windows. A timeout means no reply was received; it is not confirmation. Only the final outer receipt with the matching user messages establishes ordinary input delivery. If ending the task, follow the currently advertised finish-decision contract and its exact consumed human decision. Do not invent a response or start a model to supply one.

Worker-to-coordinator reports use collaboration messages and the exact worker claim; after reporting, return to `worker.wait` rather than opening a human-input wait.

## Generated artifacts

Commit source code, text reports, manifests, and test inputs. Store generated screenshots, recordings, archives, compiled binaries, and acceptance bundles in Google Drive or GitHub Release assets; do not add them to Git, including with `git add -f`.

Text evidence records should include the source commit, exact checks performed, artifact hashes, and external artifact location. Existing required runtime assets are source inputs and must not be deleted as generated evidence.

After a coordinated history cleanup, create new work from the cleaned branch. Transfer unpublished changes as reviewed text patches or new commits on that branch; do not merge or push the old ancestry back into the repository.

Generated documentation media under `docs/assets`, `docs/qa`, and `docs/reports` follow the same external-storage rule. Preserve `src/portal/brand-mark.png`, which is an actual runtime input; a file's binary format alone is not grounds for deletion. Keep historical build commits and artifact hashes truthful when Git history changes; use a reviewed source-equivalence mapping instead of rewriting old release manifests.

## Main branch workflow

Use `main` as the only ongoing development branch. Commit reviewed source changes and push them directly to `main` in small steps, then continue local validation and corrective commits on `main`. Routine iterations do not require a feature branch or pull request.

Preserve unrelated uncommitted work. When an existing checkout is busy or dirty, use a detached worktree based on the current `origin/main`. Push with a normal fast-forward, and reconcile any new upstream commits without overwriting another contributor's work. Remove this task's temporary local branches only after confirming that `main` contains their commits.

Local tests remain the acceptance gate. A source push does not mean that a release was published or a running client or server was updated.
