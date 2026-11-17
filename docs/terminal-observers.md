# Command observer terminals

Run the client with a terminal executable to keep commands visible in separate windows:

```sh
ssmcp --tty kitty username@ssmcp.lmm.best
ssmcp --tty ztty username@ssmcp.lmm.best
ssmcp --tty /path/to/terminal-wrapper username@ssmcp.lmm.best
```

The client opens one window when it connects. Sequential commands reuse that window.
Overlapping commands receive separate windows, so a long build continues to show progress
while another command runs. Extra windows close after three minutes without an assigned
command. The first window remains open until the client disconnects.

Each window shows the working directory, literal command arguments, output and completion
status. Spaces, Unicode, empty arguments and quotes retain their original meaning.
The displayed command is never parsed and executed a second time.

## Execution and output

On Unix, `exec.run`, `exec.parallel`, `exec.start` and RTK commands use a real PTY
when `--tty` is enabled. The existing command supervisor owns the process tree and the
chosen terminal displays that PTY's output. Closing the window leaves its command running;
the client reopens a view within the heartbeat interval and replays retained output.

Job results include `tty_slot` and `output_mode`. A Unix PTY combines stdout and stderr,
so `output_mode: "pty"` places the combined bytes in `stdout`; `stderr` is empty.
ANSI colors, carriage-return progress updates and terminal dimensions are preserved.
Windows managed exec jobs retain separate pipes and report `output_mode: "pipes"`.
Interactive sessions retain the existing native PTY/ConPTY implementation.

Standard input keeps its existing contract: `stdin: "null"` supplies EOF, and
`stdin: "pipe"` accepts `exec.write` plus explicit EOF. Use `terminal.write` for a
persistent interactive shell. That shell receives its own observer slot on its first
command and keeps it until the shell closes. Observer windows display execution;
keyboard input to them does not inject commands.

Execution deadlines and cancellation continue to stop the owned process tree. A wait
deadline only ends the wait. The bounded agent output buffers and full-output artifacts
remain available after a window closes or is reused. Each view has a lazy 128 KiB replay
tail; falling behind it produces an explicit notice referring to retained agent output.

## Native provider commands

Commands owned by a native provider retain that provider's execution, approval and
cancellation behavior. The observer shows the command and returned result; streaming
providers also send incremental output to the assigned window. Concurrent native command
streams are correlated by their existing call and process IDs.

Claude Code background commands with an explicit `backgroundOutputPath` keep their
observer while that native output source remains open. The observer reports the output
source closing when the provider removes it. It does not infer a process exit code from
a quiet output file.

## Terminal launch conventions

The executable is resolved as a path or through `PATH`. Arguments are passed directly.

| Terminal | Execute-command arguments |
| --- | --- |
| Kitty | `--title TITLE -- COMMAND ARGS…` |
| ztty, Alacritty, Konsole, xterm, st and other `-e` launchers | `-e COMMAND ARGS…` |
| GNOME Terminal, Console, foot, footclient | `-- COMMAND ARGS…` |
| WezTerm | `start -- COMMAND ARGS…` |
| Windows Terminal | `new-tab -- COMMAND ARGS…` |

For another convention, use an executable wrapper that accepts `-e COMMAND ARGS…`
and forwards the argument array to the terminal. `--tty` accepts one executable,
including paths containing spaces.

The terminal must be able to open a window in the client's desktop environment. A launch
failure or missing viewer handshake stops startup or command admission before the command
executes. Saved sessions preserve `--tty`; their service environment must provide the
same desktop access.

The viewer uses the existing authenticated local control transport. Its endpoint
descriptor is private to the local user and its listener binds only to loopback.
Client shutdown closes its windows and removes temporary observer descriptors.

## Verification

```sh
cargo test --locked --lib observer::tests -j1
cargo test --locked --lib client::tty_option_tests -j1
python3 scripts/e2e_tty_observer.py --binary target/debug/ssmcp
python3 scripts/e2e_tty_observer.py --binary target/debug/ssmcp --terminal kitty
python3 scripts/e2e_tty_observer.py --binary target/debug/ssmcp --terminal /path/to/ztty
```

The end-to-end runner starts the real client against a disposable local wire peer.
It verifies PTY detection, literal arguments, combined output, exit codes, sequential
reuse, parallel windows, piped stdin, cancellation, window reopening and interactive shell
observation. It does not connect to a relay or invoke a model. The default launcher runs
the real viewer and records output; `--terminal` opens the requested desktop terminal.

Recorded Linux results and inspected screenshots are available in the
[verification record](validation-terminal-observers.md).
