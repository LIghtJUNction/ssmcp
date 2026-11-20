# Repository instructions

## Generated artifacts

Commit source code, text reports, manifests, and test inputs. Store generated screenshots, recordings, archives, compiled binaries, and acceptance bundles in Google Drive or GitHub Release assets; do not add them to Git, including with `git add -f`.

Text evidence records should include the source commit, exact checks performed, artifact hashes, and external artifact location. Existing required runtime assets are source inputs and must not be deleted as generated evidence.

After a coordinated history cleanup, create new work from the cleaned branch. Transfer unpublished changes as reviewed text patches or new commits on that branch; do not merge or push the old ancestry back into the repository.

Generated documentation media under `docs/assets`, `docs/qa`, and `docs/reports` follow the same external-storage rule. Preserve `src/portal/brand-mark.png`, which is an actual runtime input; a file's binary format alone is not grounds for deletion. Keep historical build commits and artifact hashes truthful when Git history changes; use a reviewed source-equivalence mapping instead of rewriting old release manifests.
