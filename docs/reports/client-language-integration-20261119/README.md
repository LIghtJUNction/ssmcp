# Native language integration: sleep and tool containers

This follow-up completes the English/Chinese language path after integrating the native terminal consumers, input-wait overlay, and bounded tool containers.

## Source and behavior

The combined parent is `972f8e23bcbebc23d16c590e364eb0723ebeed66`, retaining full UI localization [0801588](https://github.com/LIghtJUNction/ssmcp/commit/080158809e5de1c8388f1e85227868e639457e1f), approved integration [9710a57](https://github.com/LIghtJUNction/ssmcp/commit/9710a57f6a16ae9c0ef4ec4d9afdf4e15635c702), and container labels [11722b1](https://github.com/LIghtJUNction/ssmcp/commit/11722b152c804f3ce63f2a30871fca095afab419).

The merge preserves both sides of the local-authorization and collapsed-thumbnail tests. Container titles call `chat_title_in(ui.language)`. Incoming evidence logs remain unchanged.

All four large and compact input-wait hints now follow the selected language. Centering and highlight spans use terminal cell widths, including Chinese. Language switching preserves the draft, active wait, dismissal IDs, and existing wake behavior. Waking the display does not answer the wait.

Help explains both languages' tool-container controls: the left half scrolls output, then the container, then the conversation at each edge; the right half always scrolls the conversation. Enter, PgUp/PgDn, and Home/End descriptions match existing routing.

User text, paths, tool output, protocol fields, and authorization retain their existing meaning. Clickable links are a separate pending change.

## Validation

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo test --locked --lib client::terminal` | 296 passed, 0 failed, 2 ignored |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo build --locked --bin ssmcp` | Passed |
| `scripts/verify-ui-language.py` on the frozen executable | 10 checks passed |

The new real-frame regression cycles English → Chinese → English at six terminal sizes from 1×1 to 28×120. It checks cell placement and width limits while preserving a Unicode draft and wait state.

The two ignored tests are the release-only PNG preparation microbenchmark and the foreign-owned PTY/drop-UID output fixture. This is a complete terminal-group run, not a full repository test run.

Owned PTYs use private preview configuration. They verify default English, immediate switching, private atomic save, restart persistence in both languages, invalid input, save failure, narrow Unicode frames, and clean machine-readable TOML. They do not contact a backend or use a real account.

## Evidence and limits

Before and after the gate, the full tracked/non-ignored tree digest was `ca22d7963ea44f7a0714f21a790b5fa85a00bd5f6548db23528d415d80545b74`. These report files were added afterward; production source did not change. Commands, exit codes, hashes, gitlinks, and PTY checks are in [validation.json](validation.json).

Frozen development executable:

- SHA-256: `b25ddc07542582f93c1d8e9487dc4ec43ac1c956fffceefa7cbe3bda259e1767`
- Size: 111,529,016 bytes
- Private path: `/tmp/ssmcp-language-integration-20261119/ssmcp`
- Reported package version: 0.10.14

The build used a worktree-specific target, one Cargo job, disabled development/test debug information and incremental compilation, and the existing host compiler lock. No executable was installed, no client was restarted, and no CI run was requested. The earlier full-UI results are separate; their count is not added to this overlapping terminal run.
