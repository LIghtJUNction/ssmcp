# SSMCP v0.10.20 client installation checkpoint

The 10,141,793-byte Linux x86-64 release archive matched GitHub’s asset digest, the release manifest, and `SHA256SUMS`. Its 13,434,720-byte client ran `--version` and `--help` on the local x86-64 GNU/Linux host. The PATH-selected executable stayed at **0.10.10** because its symlink resolves into the oh-my-ztty bundle. The bundle owner needs to handle promotion of the PATH-selected client. [GitHub release](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20) · [Archive](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.10.20/ssmcp-x86_64-unknown-linux-gnu.tar.gz)

## Publication and provenance

GitHub release `452506742` was published at `2026-11-20T09:43:48Z` with 16 assets. The annotated tag object `ae483355fd85e40ecb3e0f04ff64338b17a9ffc9` peels to `4f4dd88275c205e0e61a7601b4e0d11f56038769`, which matches the checked-out source, release manifest, and successful workflow run `44710769033`. The workflow reported `deploy_server: skipped`. The initial GitHub latest release API response reported v0.10.20. The subsequent request returned HTTP 403. [Release API](https://api.github.com/repos/LIghtJUNction/ssmcp/releases/tags/v0.10.20) · [Latest release API](https://api.github.com/repos/LIghtJUNction/ssmcp/releases/latest) · [Workflow](https://github.com/LIghtJUNction/ssmcp/actions/runs/44710769033)

The official crates.io download is a separate artifact from the Linux executable. The 8,877,099-byte payload matched the supplied SHA-256 and its Cargo VCS stamp identified the same source commit. The crates.io version metadata request returned HTTP 403, which prevented independent readback of the registry’s checksum field on that endpoint. [Official crate payload](https://static.crates.io/crates/ssmcp/ssmcp-0.10.20.crate) · [Version metadata endpoint](https://crates.io/api/v1/crates/ssmcp/0.10.20)

## Local observations

| Executable | Before | Checkpoint | Ownership or provenance |
| --- | --- | --- | --- |
| `$HOME/.local/bin/ssmcp` | 0.10.10 | 0.10.10 | PATH-selected link into oh-my-ztty 0.2.5 |
| `$HOME/.cargo/bin/ssmcp` | 0.10.15 | 0.10.15 | Cargo’s local-path install record identifies 0.10.15 |
| `/usr/bin/ssmcp` | 0.9.1 | 0.9.1 | System package `ssmcp-git` |

The installer stages a verified client in the destination directory and renames it into place. The update command directs managed installations to their package owner. This checkpoint made zero replacements. The two Cargo install records and the sampled user-state hashes were byte-identical at the second observation. [Unix installer](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/scripts/install.sh) · [Update command](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/src/client/update.rs)

The system client manager, 007 service and three running session units retained their reported MainPID, start timestamps, and fragments. Of 33 process identities sampled, 31 matched the second snapshot and two PIDs were absent. This task sent zero process signals. [Verification data](verification.json)

## Connection acceptance

A zero-argument v0.10.20 probe depends on the owner-managed promotion. The release source emits `Older relay: reconnecting once with the same verified account and self-only access.` only after the specific pre-Ready legacy rejection and re-verification of the same account. A subsequent `Connected` status identifies the accepted client. Both observations and a coordinator-issued native `printf` receipt remain pending. The production server’s live version and switch eligibility were outside this checkpoint. [Legacy fallback](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/src/client.rs#L1237) · [Connected status](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/src/client.rs#L2242)

[Verification data](verification.json)
