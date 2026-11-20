# SSMCP v0.10.20 client acceptance

The official 0.10.20 client replaced 0.10.15 at `$HOME/.cargo/bin/ssmcp`, and login interactive Zsh runs the new executable. A real native `printf` call completed on the single zero-argument probe after legacy self-only fallback. The probe and its PTY process scope are gone. The native execution path and the systemd mise shim both still reported 0.10.10 at final readback. [Verification data](verification.json)

## Publication and provenance

GitHub release `452506742` was published at `2026-11-20T09:43:48Z` with 16 assets. The annotated tag object `ae483355fd85e40ecb3e0f04ff64338b17a9ffc9` peels to `4f4dd88275c205e0e61a7601b4e0d11f56038769`, which matches the release manifest and successful workflow run `44710769033`. The workflow reported `deploy_server: skipped`. The first anonymous latest-release request reported v0.10.20 and the recheck returned HTTP 403. The coordinator independently confirmed v0.10.20 through the authorized GitHub connector before the replacement. [Release API](https://api.github.com/repos/LIghtJUNction/ssmcp/releases/tags/v0.10.20) · [Workflow](https://github.com/LIghtJUNction/ssmcp/actions/runs/44710769033)

The 10,141,793-byte Linux x86-64 archive matched GitHub’s asset digest, the release manifest, and `SHA256SUMS`. Its 13,434,720-byte client ran `--version` and `--help` locally. The final installed executable matched the extracted client’s SHA-256 and the inode recorded at installation. [Official archive](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.10.20/ssmcp-x86_64-unknown-linux-gnu.tar.gz)

The 8,877,099-byte crates.io payload matched the supplied SHA-256 and its Cargo VCS stamp identified the release source commit. The version metadata endpoint returned HTTP 403, which blocked independent readback of the registry checksum field. [Official crate payload](https://static.crates.io/crates/ssmcp/ssmcp-0.10.20.crate) · [Version metadata endpoint](https://crates.io/api/v1/crates/ssmcp/0.10.20)

## Local installation

| Measured environment | First lookup | Before | Final |
| --- | --- | --- | --- |
| Login interactive Zsh | `$HOME/.cargo/bin/ssmcp` | 0.10.15 | 0.10.20 |
| Native execution | `$HOME/.local/bin/ssmcp` | 0.10.10 | 0.10.10 |
| systemd user manager PATH | `$HOME/.local/share/mise/shims/ssmcp` | 0.10.10 | 0.10.10 |

The replacement used a verified private rollback copy, a synced temporary sibling, an atomic rename, and a directory sync. Both Cargo install records stayed byte-identical, including the 0.10.15 local-path install and 37 other package records per file. The 0.10.20 executable came from the official release archive. The Cargo metadata records the prior local build. The sampled bundle binary and symlink matched, so the suite update remains separate. [Unix installer](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/scripts/install.sh)

Sampled user configuration, auth, startup, and history files matched. The systemd manager PATH matched byte for byte. The native worker and Zsh PATH samples differed only in the per-worker Codex launcher directory. Their SSMCP lookup paths matched the earlier observations.

Across the replacement interval, five loaded service main processes and 26 other persistent candidates kept their PID, start ticks, and executable device and inode. Nineteen continued on the retired Cargo inode. At final readback, all five service main processes and 24 of the 26 other candidates still matched. Two candidate PIDs were absent with unknown attribution. They were distinct from the two unattributed disappearances in the initial native-path survey. This task sent zero signals to existing processes.

## Production and native probe

The saved read-only production receipt observed the same 0.10.13 daemon throughout the query. PID, service start timestamp, executable hash, and zero restarts matched the supplied prior receipt. The inspector exited 3 with `blocked_active_sessions` and `switch_permitted=false`. It found active sessions and no verified atomic admission fence or drain lease. Final acceptance reused that receipt. [Official inspector](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/scripts/inspect_server_upgrade.py)

The probe started with only the installed executable in argv. Its private terminal emitted the legacy self-only fallback notice. The coordinator matched the exact task workspace in the live client list and used that client’s native schema to run `/usr/bin/printf`. The job completed with exit 0, 30 stdout bytes matching `ssmcp-0.10.20-native-probe-ok\n`, and zero stderr bytes. Cancellation and timeout were both false. [Legacy fallback source](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/src/client.rs#L1268)

After receipt verification, a single SIGINT to the verified probe invoked the same shutdown handler as Ctrl-C. The probe exited 0. The PTY runner exited on its own, all four task-owned process identities were gone, and the transient scope was unloaded. The private receipts and verified backup were retained. [Client interrupt handler](https://github.com/LIghtJUNction/ssmcp/blob/4f4dd88275c205e0e61a7601b4e0d11f56038769/src/client/terminal.rs#L855)
