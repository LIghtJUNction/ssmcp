# UI release-gate verification — 2026-11-20

The complete, unchanged Makefile target test-client-ui-ready passed in one invocation: **9 suites, 691 checks, exit 0, 130.521 seconds**. The test processes used private PTYs and a loopback instance of the production account router with a synthetic identity provider. They did not use the installed account, a live relay, an external model, or a user daemon.

## What changed

The fixture corrections follow the existing renderer and its focused Rust tests. Recipient captions now expect To followed directly by the member name. Queue assertions inspect the actual Queue column and its Waiting, Read and Cancelled receipts, retaining message text, selected-row checks, exact full identities, CAS revisions, authenticated recipients and response-boundary delivery checks. Successful sends are first observed in Conversation, then inspected in Queue. Held edits require the actual Edit queue caption.

The shared terminal shutdown helper follows the real on-screen Ctrl-C sequence within its original eight-second limit. It accepts the split-layout first-key exit; chat mode clears the active editor, shows the exit reminder, and then accepts confirmation, using at most three keys. Existing terminal-restoration and successful-process-exit assertions remain.

The private relay fixture now verifies the exact identity already defined by its production-router harness, including the registered handle. No account or authorization rule changed.

The preview login-rejection check compares the caption immediately before Enter with the rejected state. Typing a command legitimately changes the empty-draft hint; the draft, cursor, body, recipient and canvas must still remain unchanged by rejection.

The generated image sample changed from 512 × 128 to 512 × 127 pixels. The existing 32-column thumbnail renders it at 320 × 79.375 pixels, leaving a partial fourth terminal row. The full original-pixel hash, width-only scale axis, bounds, source-pixel crop, no-reupload and rejection-fallback checks remain. The real cropped placement used source y = 64, height = 63 and the same scale.

## Complete local result

| Suite | Checks | Result |
| --- | ---: | --- |
| client-ui | 56 | passed |
| canvas-logs | 68 | passed |
| composer | 87 | passed |
| questions | 42 | passed |
| input-history | 119 | passed |
| presentations | 57 | passed |
| context-memory | 71 | passed |
| file-paste | 145 | passed |
| relay-composer | 46 | passed |

The JSON record contains the exact commands, script hashes, frozen binary hashes and observed crop geometry. The client came from source 07928a32ae35795bc256d7ea1d873a55a7a40b22; the production-router library fixture came from the previously validated Linux build at 8a54a3f8500ce37724d6062bee7b4bc17f597d88. Subsequent changes to the relevant Linux implementation were release evidence and the removal of a Unix-only attribute from an already portable test helper. These local checks do not replace the full native platform release checks.

## Server deployment remains deferred

All **84 release-policy tests** passed after adding 0.10.17 and 0.10.18 to the existing 0.10.15 and 0.10.16 deployment deferrals. The tests execute the actual workflow policy for tag pushes and dispatches, including explicit boolean and string true requests. All four known releases produce deploy_server=false. Future selection examples use 0.10.19; this does not authorize a production switch.

## Evidence and limits

The [prior full-platform preflight](https://github.com/LIghtJUNction/ssmcp/actions/runs/44703969970) at 0d3c4dc723ca7d554bcbe6dbd41e8b7b6a9b15ff passed six optimized builds, both native Windows checks, both native macOS checks and both installed-server checks. Linux stopped at the old UI fixtures. Its subsequent real-user-systemd and watchdog step had not run; the merged candidate still needs one complete release preflight.

The additional file-only @ completion regression passed. The extra agent-panel script, which is outside this Makefile release gate, stopped at its caller-group navigation assertion before reaching the recipient-caption change. Its exact error, keyboard navigation did not reach ChatGPT, was retained and reported for the separate feature work. It is not counted among the 691 passing checks.

The [recipient and preview corrections](https://github.com/LIghtJUNction/ssmcp/commit/51e772340c214a1ae6f24a6a94113dbbcfe951ee) and [shared queue and exit corrections](https://github.com/LIghtJUNction/ssmcp/commit/831e0969465929890ed130537223cedc18d6c80d) were committed separately. Original failed frames and intermediate logs remain in the private task evidence directory. Failed v0.10.15 and v0.10.16 tags remain unchanged; this report does not claim a new published release or server deployment.
