# Android module console touch interaction

Source base: `59593716a235746b90fdd0862aab887e7c7af60a`.

The module WebUI uses a plain-text `vt100` screen snapshot. Before this fix,
`console.js` focused its hidden input on click, but did not forward mouse cell
coordinates. Single-finger motion only attempted DOM scrolling, although the
snapshot is a bounded screen, not the native TUI's history. The native client
already decodes SGR mouse sequences and uses button 0 to toggle activity cards,
and buttons 64/65 to scroll the hit-tested panel (`src/client/terminal.rs` and
`src/client/terminal/conversation.rs`).

The fix maps viewport coordinates through the measured terminal glyph grid,
padding and scroll offsets; sends SGR press/release for taps and wheel reports
for single-finger swipes and wheel input; reserves touch handling for the TUI;
and suppresses synthetic clicks following a swipe, pinch or cancelled touch.
The existing queued, tab-specific input transport is reused without retries.

Checks performed in the cloud workspace:

- `node --check packaging/magisk/src/ssmcp/webroot/console.js`: passed.
- `node --check scripts/test_magisk_console_browser.mjs`: passed.
- `PLAYWRIGHT_MODULE=/opt/codex/cua_node/lib/node_modules/playwright/index.mjs node scripts/test_magisk_console_browser.mjs`: passed all four combinations of manager bridge / loopback HTTP and 320 / 390 px mobile Chromium viewport. Starts from the real `webroot/index.html`; checks exact click coordinates and release, keyboard input, both swipe directions, wheel coordinates, swipe suppression of keyboard/click, font pinch, and session switching. Transport replies and expanded/scrolled screen text are explicitly isolated fixtures, not a native TUI execution.
- The same browser check against the source base's WebUI: failed as expected at the first exact click-report assertion (8-second timeout).
- `python3 scripts/test_magisk_installer_state.py`: all 5 passed.
- `git diff --check`: passed.

SHA-256 of reviewed inputs:

| File | SHA-256 |
| --- | --- |
| `packaging/magisk/src/ssmcp/webroot/console.js` | `31b2c92eadf1c941735887dee4f3ea645be08c78b21d09fde45ac39d784fb2a9` |
| `packaging/magisk/src/ssmcp/webroot/console.css` | `51f7fc2d4dba56c369e5c7e51499ba80a75f24bbfbac7b474ff8dbe62a0b903d` |
| `scripts/test_magisk_console_browser.mjs` | `3f19348fda286ecf73a7d559cdfd6782ebd0bf134d00107b529ac527f3fb9e84` |

Limitations: no user's computer or Android device was accessed. Actual KernelSU /
Magisk WebView, Android soft keyboard, device PTY and native TUI end-to-end behavior
were not exercised. No Rust build or Android module packaging was performed
(Rust toolchain is absent; only browser assets changed). No release or deployment
was requested or performed. No generated media or binary evidence was produced;
external artifact location: not applicable.
