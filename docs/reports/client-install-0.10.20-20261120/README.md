# SSMCP v0.10.20 client installation checkpoint

The official 0.10.20 client replaced the 0.10.15 executable at `$HOME/.cargo/bin/ssmcp`. Login interactive Zsh now resolves and runs 0.10.20 from that path. Native execution and the systemd user manager PATH still select the independently managed oh-my-ztty client at 0.10.10. The zero-argument probe is live after the legacy self-only fallback. Native `printf` acceptance and probe cleanup await the coordinator’s receipt. [Verification data](verification.json)

## Publication and provenance

GitHub release `452506742` was published at `2026-11-20T09:43:48Z` with 16 assets. The annotated tag object `ae483355fd85e40ecb3e0f04ff64338b17a9ffc9` peels to `4f4dd88275c205e0e61a7601b4e0d11f56038769`, which matches the release manifest and successful workflow run `44710769033`. The workflow reported `deploy_server: skipped`. The first anonymous latest-release request reported v0.10.20 and the recheck returned HTTP 403. The coordinator independently confirmed v0.10.20 through the authorized GitHub connector before the replacement. [Release API](https://api.github.com/repos/LIghtJUNction/ssmcp/releases/tags/v0.10.20) · [Workflow](https://github.com/LIghtJUNction/ssmcp/actions/runs/44710769033)

The 10,141,793-byte Linux x86-64 archive matched GitHub’s asset digest, the release manifest, and `SHA256SUMS`. Its 13,434,720-byte client ran `--version` and `--help` locally. The installed binary matched the extracted executable’s SHA-256. [Official archive](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.10.20/ssmcp-x86_64-unknown-linux-gnu.tar.gz)

The 8,877,099-byte crates.io payload matched the supplied SHA-256 and its Cargo VCS stamp identified the release source commit. The version metadata endpoint returned HTTP 403, so its checksum field remains independently unread. [Official crate payload](https://static.crates.io/crates/ssmcp/ssmcp-0.10.20.crate) · [Version metadata endpoint](https://crates.io/api/v1/crates/ssmcp/0.10.20)

## Local installation

| Measured environment | Resolved executable | Before | After |
| --- | --- | --- | --- |
| Login interactive Zsh | `$HOME/.cargo/bin/ssmcp` | 0.10.15 | 0.10.20 |
| Native execution | `$HOME/.local/bin/ssmcp` | 0.10.10 | 0.10.10 |
| systemd user manager PATH | `$HOME/.local/bin/ssmcp` | 0.10.10 | 0.10.10 |

The replacement used a verified private rollback copy, a synced temporary sibling, an atomic rename, and a directory sync. Both Cargo install records stayed byte-identical, including the 0.10.15 local-path install and 37 other package records per file. The 0.10.20 executable came from the official release archive. The unchanged metadata records the prior Cargo build. The sampled configuration and auth hashes, startup files, and measured PATH values matched. The configured history file’s modification time predates both shell observations. [Unix installer](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/scripts/install.sh)

Across the Cargo replacement, five loaded service main processes and 26 other persistent candidates kept their PID, start ticks, and executable device and inode. Nineteen continued on the retired Cargo inode. The earlier native-path survey had two unattributed PID disappearances across its own observations. The task sent zero signals to existing processes.

## Production and probe

The read-only production inspection observed the same 0.10.13 daemon throughout the query. PID, service start timestamp, executable hash, and zero restarts matched the supplied prior receipt. Start ticks matched within the fresh query. The inspector exited 3 with `blocked_active_sessions` and `switch_permitted=false`. It found active sessions and no verified atomic admission fence or drain lease. [Official inspector](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/scripts/inspect_server_upgrade.py)

One client started with only the installed executable in argv. Its private terminal emitted the legacy self-only fallback notice. The authorized SSMCP live-client listing independently matched the probe’s exact working directory. The PTY runner stays live in a task-owned transient process scope. The listing confirms registration, and the coordinator’s native call is still pending. [Legacy fallback source](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/src/client.rs#L1237)
