# Terminal observer verification

## Scope

The Linux client was exercised with the real viewer process in a recording launcher,
Kitty and ztty. Each run passed seven acceptance groups, including concurrent commands,
cancellation and reopening a manually closed viewer. These results establish the command
observer behavior on the tested Linux machine. They do not measure aggregate desktop
memory or establish runtime behavior on macOS or Windows.

The GUI evidence was captured from implementation checkpoint
`d3fa813f7fcdd1c38d27b834de74a9be21775b9f`, using the pinned debug executable whose
SHA-256 is `ea0d128515b069668479eebdd7be3349aa1ede3b4ddde3879f6f97356d752537`.
The final follow-up adds Pi/OpenCode wrapper recognition, removes platform-specific
unused mutability and changes the package version to 0.7.4. The affected Rust tests
are checked separately; the earlier GUI runs are not presented as repeated runs on
the later release executable.

## Results and evidence

| Check | Observed result | Evidence |
| --- | --- | --- |
| Observer unit and managed-process tests | 10 passed | `src/observer/tests.rs`; local targeted test invocation |
| Existing managed command regressions | 17 passed | `backend::shell::tests`; local regression invocation |
| Existing client regressions | 22 passed | `client::tests`; local regression invocation |
| Saved `--tty` option behavior | 1 passed | `client::tty_option_tests`; local targeted invocation |
| Real viewer with recording launcher | 7 acceptance groups passed | [Recording report](reports/tty-observer-recording-20261118.json) |
| Real Kitty windows | 7 acceptance groups passed | [Kitty report](reports/tty-observer-kitty-20261118.json) |
| Real ztty windows | 7 acceptance groups passed | [ztty report](reports/tty-observer-ztty-20261118.json) |
| Fixed 0.7.4 release with recording launcher | 7 acceptance groups passed | [Release report](reports/tty-observer-release-e2e-20261118.json) |

The 50 Rust tests above are unique tests from four local invocations, not a claim that
the entire repository suite was run. The observer tests cover bounded replay storage,
slot reuse and expansion, literal launcher arguments, admission failure, PTY output,
exact exit status, piped input, cancellation, idle retirement and native stream routing.
A failed viewer launch is checked before a command is allowed to start.

Each end-to-end run verifies the same seven groups. A command checks that both output
descriptors are terminals, emits ANSI and Unicode text, writes stderr, and exits with
code 7. Subsequent commands verify reuse of slot 1, overlapping work in slots 1 and 2,
piped input and EOF, cancellation of a command and its child, viewer reopening while a
long command continues, and observation of the existing interactive shell.

The two GUI screenshots were visually inspected after capture. They show working
directories, command arguments, command output, completion or cancellation markers,
and the interactive shell marker. The Kitty screenshot also shows the Unicode argument
and exit code 7; the ztty screenshot shows the retained tail after window reopening.
Only disposable fixture content appears in these images.

- [Kitty screenshot](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/tty-observer-kitty-20261118.png`）
- [ztty screenshot](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/tty-observer-ztty-20261118.png`）

## Reproduction

The runner uses the real client and a disposable local wire peer. Its isolated PATH
contains the fixture transport, so it does not start installed model harnesses or
connect to an external relay. GUI runs used an isolated Xvfb display with software
rendering, and the ztty run used a unique abstract socket. These choices isolate the
test windows from the developer's normal desktop and ztty sessions.

```sh
cargo test --locked --lib observer::tests -j1
cargo test --locked --lib backend::shell::tests -j1
cargo test --locked --lib client::tests -j1
cargo test --locked --lib client::tty_option_tests -j1
cargo build --locked --bin ssmcp -j1
python3 scripts/e2e_tty_observer.py --binary target/debug/ssmcp
python3 scripts/e2e_tty_observer.py --binary target/debug/ssmcp --terminal kitty
python3 scripts/e2e_tty_observer.py --binary target/debug/ssmcp --terminal /path/to/ztty
```

## Final focused validation

The Pi RPC and OpenCode shell wrappers passed the native-command detection test after
recognition was extended to their nested arguments. The managed PTY test also passed
after platform-specific initialization was simplified. Formatting and strict Clippy
for the library and client binary passed. The first Clippy run identified two
collapsible conditions; both were rewritten without changing their predicates.
The [command history](reports/tty-observer-validation-20261118.json) records that failed
lint attempt and the successful follow-up, alongside the unchanged unique-test count.

## Fixed release artifact

The optimized 0.7.4 executable was built from the clean source commit
`05f72fc2bbed776502b3a19c3a6b118f7c7646db` using
`cargo build --locked --release --bin ssmcp -j1`. Its SHA-256 is
`8271d106f45af1c23cef9873f675e210741ecd4ecda83d7ea5c2f67d29e5a0c5`, and its size is
11,012,544 bytes. The [provenance record](reports/tty-observer-release-20261118.json)
identifies the fixed local copy, source commit and build command.

The fixed copy passed its version and `--help` checks, followed by all seven groups
of the [recording-launcher E2E](reports/tty-observer-release-e2e-20261118.json).
This additional run validates the release executable itself. The Kitty and ztty
screenshots remain evidence from the earlier GUI runs described above.

The final evidence commit adds only these records and this documentation. It does
not rebuild or change the tested implementation, so the binary's source remains
`05f72fc2bbed776502b3a19c3a6b118f7c7646db` even when the branch includes that later
documentation commit.

## Behavioral boundaries

Managed Unix commands use a real output PTY and report `output_mode: "pty"`; their
combined stdout and stderr is returned in `stdout`. Input keeps the existing null
or pipe contract. The observer window is a view of the owned process, while the
existing process supervisor remains responsible for exit status and cancellation.
Windows managed commands retain pipes; the interactive terminal keeps its existing
native PTY or ConPTY implementation.

Native-provider commands retain the provider's execution and approval behavior. The
observer displays their command and returned result and forwards supported incremental
events using call and process identifiers. The tests use protocol fixtures; this
verification does not launch external models. Claude background observation ends when
the explicit output source closes, without interpreting silence as a process exit.

Launcher mappings and the executable-wrapper interface are documented in
[Command observer terminals](terminal-observers.md). Kitty and ztty were run as actual
terminals; other launcher mappings were checked as literal argument arrays. Runtime
compatibility with every terminal application has not been claimed.

## Review questions

| Dimension | Question | Resolution |
| --- | --- | --- |
| Contribution | Does the user get reusable, expandable command windows? | Sequential and parallel E2E assertions passed with real viewers. |
| Clarity | Is execution ownership distinguishable from display? | The process supervisor, PTY and native-provider boundaries are stated above. |
| Experimental strength | Are exit, input, cancellation and long tasks exercised? | Each GUI run includes these assertions; screenshots supplement them. |
| Evaluation completeness | Are untested platforms and providers identified? | Linux runtime evidence, launcher-only checks and protocol fixtures are separated. |
| Design soundness | Can closing a window lose or repeat a command? | The owned task survives viewer closure; reopening replays its bounded tail. |
