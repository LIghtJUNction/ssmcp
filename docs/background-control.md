# Ordered browser actions and condition waits

Use `ssmcp.browser.act` to send an ordered sequence to one observed browser tab.
The client holds that tab's operation lock through the sequence and optional
final snapshot, so another operation on the same tab cannot interleave its
input. Other tabs retain their independent operation locks. The implementation
is in [browser.rs](../src/browser.rs) and [actions.rs](../src/browser/actions.rs).

Existing browser setup, private desktops and explicit host input are described
in [Browser control and private desktops](browser-desktop.md).

## Send a batch

Obtain `target_id`, `inputRef` and `buttonRef` from the target's current snapshot.
The selector and expected text below refer to elements observed on that page.

```javascript
const reply = await browser.act(clientId, {
  target_id,
  actions: [
    {type: "fill", ref: inputRef, text: "Ready to save"},
    {type: "click", ref: buttonRef},
    {type: "wait", selector: "#result", text: "Saved", timeout_ms: 5000}
  ]
});
return reply;
```

A batch contains 1–16 fill, click, key, scroll or wait actions. Its 25-second
deadline includes acquiring the tab lock, executing actions and taking the final
snapshot. Fields, types, sizes and numeric bounds are checked for every action
before input starts. Element references and selectors are resolved when their
action executes. Successful batches report acknowledged steps and, by default,
a fresh snapshot that replaces the previous element references. Set
`snapshot: false` when the final snapshot is unnecessary.

Input actions use the same target focus emulation and press/release cleanup as
standalone browser input. Focus emulation applies to the CDP target and is
restored on Computer Use revocation or disconnect; it does not activate a browser tab or move the system
pointer. Wait actions send no input.

## Continue after a partial failure

Read both `isError` and `structuredContent` before continuing. A failed action
can have an effect before its response fails, so observe the page again and
choose the remaining work from the observed state.

| Report field | Meaning |
| --- | --- |
| `completed_count` and `completed` | Actions whose successful replies were received, in order |
| `phase: "queue"` | Failure before the batch began its input |
| `phase: "actions"` and `failed_index` | Zero-based index of the action that failed or timed out |
| `phase: "snapshot"` | Input actions completed; the final snapshot failed |
| `replay_safe: false` | Reobserve the target before deciding which operation to issue |

The client stops on the first failure and returns the partial report without
replaying the batch or falling back to desktop input. Off uses the same partial
report. Actions whose replies were interrupted may already have affected the page.

## Wait for a page condition

```javascript
return await browser.wait(clientId, {
  target_id,
  selector: "#result",
  text: "Saved",
  state: "visible",
  timeout_ms: 5000
});
```

Provide a CSS selector, literal text, or both. When both are supplied, the first
matching element must contain the requested text. Conditions support
`visible`, `hidden`, `attached` and `detached`. Visible content requires
a positive-area client rectangle and visible CSS display. Text-only conditions
inspect the document body.

The 1–10000 ms timeout includes time queued for the target lock. The host polls
the page condition and retries transient missing execution contexts during
navigation. It does not depend on a timer scheduled inside the page.

Existing `browser.wait_for` remains available with its text
`present`/`absent` contract and existing timeout bounds.

## Validation entry points

- `cargo test --lib browser::` covers validation, target ordering, queue
  deadlines, navigation recovery, focus preparation and cancellation cleanup.
- `node --test scripts/test_cua_facade.mjs` checks native argument/result
  routing and partial-error preservation.
- `python3 scripts/e2e_computer_use.py` exercises real Chromium batches,
  stale references, conditional waits and the existing browser checks.

[Historical PR #13 validation](validation-background-control.md) records the
original branch environment and results.
