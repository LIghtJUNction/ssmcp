# Native client language validation

This stage extends the persistent language core in `a3e4b679528e9e35ae9940ac4c296f5205e7cd0b` across the native client interface. English remains the default. `/language en` and `/language zh` apply and save the choice without changing drafts, recipients, navigation, or delivery state.

## Implemented coverage

The visible translations cover composer placeholders and command descriptions, navigation, input and selection modes, help, queue editing and receipts, tasks, questions, history controls, login and logout outcomes, local access, waits and notification status, quota and reset captions, connection data rates, audit views, and side panels.

User messages, answers, agent names, paths, commands, tool output, protocol keys, and external diagnostic details remain original. The generated skipped-question history record remains the canonical `Skipped by user`; only its live presentation changes language. The typed receipt still requires a real delivery record. “Read” means included in a tool response, not understood or executed by a model.

See the [usage guide](../../client-language.md), [machine-readable validation](validation.json), and [reproducible PTY fixture](../../../scripts/verify-ui-language.py).

## Validation

| Check | Observed result |
| --- | --- |
| Rust regression groups | 352 distinct passing test names across the full stage and its narrow follow-up |
| Native terminal group | Complete group passed without skips |
| Language rendering | Eight views in both languages at widths 1, 2, 10, 24, 45, 80, and 120 |
| Strict Clippy | All targets passed; after the canonical history correction, the affected library passed again |
| Final binary | Build passed, source unchanged throughout its gate, binary copied and hash verified before PTY execution |
| Private PTY and CLI fixture | All 10 named checks passed |
| Formatting and patch checks | Passed |

The PTY fixture starts three owned interactive preview processes with private configuration, state, and cache directories. It checks immediate Chinese/English changes, persistence through process restarts, invalid values, preservation of unrelated configuration, mode 0600, and failed writes leaving the original document and active language intact. It also renders twelve narrow static previews and checks that `ui show` remains clean, parseable TOML. These previews do not connect to a server or authorize an account.

The earlier core had its own 15-test gate. That count is not added to the 352 because some coverage overlaps.

## Source and binary binding

- Full gate source digest: `cf87404b7ff5b1063642a2168f6578abc77a3245188d4737978fb82d5e46ef8e`.
- Final build source digest: `93d335366c5407266cca2c7f5a58c38acf4bd99a9423ef048335908e7e8075d2`.
- Frozen development binary: 104,640,512 bytes.
- Binary SHA-256: `81f9ce37b125cd63be7d57724912c50e1342527a8e3f11710fc3608532567a4a`.
- Executed PTY fixture SHA-256: `a935eba85ac92e51e45e2e8a598e0cec20bff2a167072404905b55b16a492859`.

The final fixture gained an owned-child SIGTERM cleanup handler after compilation and was then executed against the frozen binary. That fixture change and this report do not change the compiled Rust sources. Builds used a worktree-specific target, one Cargo job, disabled debug information/incremental compilation, and the existing host compiler lock.

## Observations and limits

Two existing Unix datagram socket-close assertions failed in the parallel `client::link_metrics` group. Both passed with their original assertions when the unchanged group ran with `--test-threads=1`. This stage changes only the metrics display text, not socket ownership or task cancellation. The report preserves both results; it does not claim to have fixed a transport failure or proven the precise source of the parallel interference.

Four baseline test expectations were aligned with already implemented thumbnail and task-refresh behavior using the peer's `6e74d811da72a0d4db88dc64efeab9f0a746160c` patch. The image tests explicitly open the initially collapsed thumbnail before checking pixels, placement, rejection, and selection freeze. The task test checks completion after polling even when no visible refresh was needed. No production behavior was changed by those adjustments.

Cargo reported the existing unused `qrcodegen` manifest dependency warning. The strict Rust lint checks passed. Compiler-lock and package-cache waits were recorded as scheduling time, not product failures.

The newer sleep overlay and tool-container integration are separate follow-up changes after this isolated stage. No installer, running-client reload, live authorization, CI run, or release publication was used.
