# Native client language

Use `/language en` or `/language zh` in the chat interface to apply and save its display language. English is the default. Command names, protocol fields, user messages, paths, code, tool output, and server diagnostics retain their original values.

## Configuration

```toml
[ui]
language = "en" # or "zh"
```

The command writes to the same selected configuration file used by the native UI. It preserves other sections, writes a private temporary file, and atomically replaces the configuration. If saving fails, the previous language stays active and the original document is kept.

A language change preserves the draft, cursor, selected recipient, focus, selection mode, queue state, and scroll anchors. It invalidates presentation caches, including caches used with `NO_COLOR`. F7 selection mode keeps its deliberate frozen display until selection mode is closed.

In the other native layouts, edit `[ui].language` and press F5 to reload. The chat interface also discovers `/language` through its slash menu and F1 help.

## Display coverage

| Area | Localized content |
| --- | --- |
| Conversation and composer | Navigation, input and selection modes, command descriptions, action hints, local validation and failure context |
| Queue and receipts | Waiting, confirmation, cancellation, recall, review, and delivery labels; edit and inspection hints |
| Tasks | List and detail labels, refresh status, job-state labels, cancellation instructions |
| Help and saved history | F1 help, navigation, history controls, resume and handoff explanations |
| Account and connection | Login and logout outcomes, local access labels, quota and reset captions, connection and application-data rate status |
| Audit and side panels | Account and agent labels, call status, source-view controls, Git fetch status, progress captions |
| User waits | Wait descriptions, cancellation controls, truthful notification status, locally generated notification title and fallback text |

A receipt marked “Read” / “已读” means that the request was included in a tool response. It does not claim that a model understood it or executed it. The language layer uses the same typed receipt state; it does not manufacture delivery confirmation.

User-defined agent names, commit subjects, questions, answers, and rich content from tools are preserved. External error details stay verbatim within translated local context.

## Tool results and input waits

Tool container labels and their scrolling hints use the chosen language. On the left half, scrolling proceeds through the output and its container before reaching the conversation; the right half scrolls the conversation directly. The native help describes the matching keyboard controls.

The input-wait overlay translates both its large and compact wake hints. It centers them by terminal cell width, including Chinese characters. A display-language change preserves the active wait and draft; waking the display does not answer the wait.

## Preview and verification

```sh
ssmcp ui preview --language zh
ssmcp ui preview --static --language en --rows 30 --cols 45
ssmcp ui show
```

The `--language` option is a temporary override for `ui show` and `ui preview`. An explicit `/language` command inside interactive preview saves the choice. Preview uses fictional data and does not start a backend or authorize a real account.

The regression suite exercises real rendered pages at widths 1, 2, 10, 24, 45, 80, and 120 in both languages. It verifies raw content and protocol snapshots, draft and cursor preservation, cache invalidation, private configuration writes, invalid language values, and failed saves. The private PTY fixture verifies immediate switching and persistence across process restarts using a temporary configuration directory.

No installer, client reload, live authorization, or network update is part of these checks.
