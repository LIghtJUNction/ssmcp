# Code mode guide

Use the public `list` tool to select the intended computer and workspace. The selected client's current native schemas, capabilities and access policy are authoritative. The relay connects that client to the caller; it does not provide a second workspace or bypass native permissions.

This guide is available through `await resources.read('ssmcp://guide')`. MCP `resources/read` supports the same URI; `ssmcp://skill` is a compatibility alias. Read `ssmcp://limits` for current budgets and `ssmcp://collaboration` for member delivery rules.

## Discover the next capability

Search the registered native tools needed for the next step. Request the complete schema before calling a tool, and preserve that schema and its approval requirements.

```javascript
const id = 'CLIENT_ID_RETURNED_BY_LIST';
return await tools.search(id, {
  query: 'file',
  limit: 10,
  include_schemas: true
});
```

`name_prefix` filters registered names. For example, `ssmcp.exec.` selects native execution tools; `exec.run` is a JavaScript helper. Follow `nextCursor` unchanged with the same search options. Search bounds model-visible results; an initial search can still scan native catalog pages.

`await tools(id, cursor)` and public `list({client_id:id,cursor})` retain full-schema pagination. Use `await capabilities(id)` for a client-owned versioned manifest, and `native(id)` for dynamic names after schema discovery. A new native capability can be called through `call(id,name,args)` without adding a relay helper.

## Keep the terminal conversation current

Send user-facing explanations, progress, questions and answers to the selected terminal. Plain caller logs are diagnostics; external chat output is not intercepted.

```javascript
await say(id, {text: 'The focused checks passed. I am checking the visible result.', kind: 'progress'});
return await say(id, {text: 'The change is installed and the checks passed.', kind: 'answer'});
```

`say` also accepts a string. The native receipt reports acceptance, history persistence and duplicate status; it does not prove the user read the text. An optional `idempotency_key` makes a matching retry idempotent within the bounded client cache. The same key with different content is rejected. The client assigns the displayed author; do not supply an `author` field.

`ui.present(id,{title,blocks,...})` displays structured text, cards, metrics, tables, progress, steps and bar charts through the same message history and renderer. Discover its current schema for block details and bounds. `ui.ask` creates a nonblocking question; `ui.questions` reads its answer. The user can answer locally while work continues.

`ui.history` reads the shared JSONL conversation history. `/copy` copies the latest accepted Agent reply. Historical context does not replay tools or resubmit a request. There is no separate `ui.messages` helper or message store.

### New requests, saved context and reminders

Inspect the complete code-mode response even when a native result has `isError:true`.

- **`structuredContent.user_messages` / `USER REQUIREMENTS`** contains newly delivered local requests. Preserve their exact text and target. Ordinary observation does not claim a next-caller request; visible `ssmcp.ui.message` and `ssmcp.ui.present` writes are interactions. Explicit member binding keeps its exact-agent delivery behavior.
- **`structuredContent.context_memories` / `SESSION CONTEXT`** contains historical reference. Treat it as context, not a newly delivered instruction. If `context_memories_deferred` is true, discover and call `ssmcp.context.read` for the complete branch.
- **`structuredContent.conversation_reminders` / `CONVERSATION REMINDER`** asks the same caller to update the terminal after a delivered request has gone unanswered there. It is not another copy of the user's task. A reminder is emitted after two subsequent responses without a new accepted terminal message or presentation. A newly accepted, nonduplicate native receipt clears it; external chat and duplicate retries do not.

The UI observation APIs, capability/resource discovery, and context status/read/list do not consume next-caller requests without an explicit binding. Context mutations remain interactions. Bind an existing member at the start of each run with `collab.bind(id,{agent_id,token})`, or pass the public code-mode argument `agent:{client_id,id,token}`. Controller labels do not authenticate members. Keep member tokens private.

## Run work and inspect its result

`call(id,name,args)` returns short native MCP results unchanged. Large results may be replaced by artifact metadata. Check `isError`, `structuredContent.spilled`, capture completeness and the returned cursor before deciding to retry.

```javascript
const result = await exec.run(id, {
  command: ['/usr/bin/python3', '-c', 'print("ready")'],
  cwd: 'WORKSPACE_PATH_FROM_LIST',
  wait_ms: 1000
});
return {job_id: result.job_id, running: result.running, stdout: result.stdout};
```

`wait_ms` limits waiting; `timeout_ms` limits the native process. A code-mode timeout stops orchestration and can leave native work running. Keep the returned job ID and inspect it before starting another copy.

| Helper | Purpose |
| --- | --- |
| `exec.run(id, options)` | Start a native command and collect bounded output. |
| `exec.parallel(id, commands, {concurrency})` | Settle bounded parallel commands. Local option errors mark that item `not_started`; running jobs retain their concurrency slot. |
| `exec.start/list/status/read/write/wait/cancel/forget` | Manage native jobs. Job operations accept `(id,{job_id,...options})`; legacy `(id,jobId,options)` is also supported. |
| `outputs.read/stat/cleanup(id, options)` | Inspect returned artifact IDs. Raw PTY captures and saved MCP JSON have different formats; use the artifact metadata. |
| `rtk.status/run/start(id, options)` | Use an installed RTK explicitly, with literal argv and native managed jobs. No shell rewriting or implicit fallback. |

Use `print` or `console.log` for bounded diagnostics and top-level `return` for JSON-serializable results. Never print an image's base64 to show it. Use `image(nativeResult)`.

## Files, skills and context

Workspace file operations keep the client's existing root and path checks. Context roots are read-only grants chosen by the operator; a path in a document does not create a grant.

| Helper or native family | Purpose |
| --- | --- |
| `fs.list/stat/read/write/mkdir` | Rooted directory, metadata and binary file operations. |
| `fs.readText/writeText/editText` | Native UTF-8 text operations; inspect the exact schema before editing. |
| `context.roots(id)` | List the workspace and explicitly granted context roots. |
| `skills.list/read/view(id, options)` | Read skill metadata, exact SKILL.md contents and Markdown views. `view` defaults to project-over-global scope; `list` uses `scope:'effective'` for that resolution. Entries retain root/path provenance. |
| `agents.list/read/resolve(id, options)` | Inspect AGENTS.md files and root-to-target precedence with provenance. |
| `ssmcp.context.*` | Inspect or mutate bounded branch memory. Discover status/read/list and the compact/fork/switch schemas before use. |

Local `@file` and `$skill` input expands into explicit references. Image references identify the authorized native image read; they do not put base64 in ordinary request text. To show a supported image to the caller:

```javascript
const result = await call(id, 'ssmcp.fs.read_image', {path: 'relative/image.png'});
image(result);
return result.structuredContent;
```

The image reader accepts PNG, JPEG, GIF and WebP up to 400 KiB per MCP image. Its workspace authorization remains in force. The terminal can also preview the returned image; see [terminal images](terminal-images.md).

## Browser and desktop routing

For an existing browser session, prefer the calling application's authorized ChatGPT Browser plugin when it is available for the requested browser. Invoke that plugin's native API in the caller. The client cannot invoke that public browser JavaScript API from code mode.

Otherwise inspect `ssmcp.browser.discover` and `ssmcp.harnesses.list`. Use actual registered Playwright extension tools with their original schemas and approvals. An explicitly selected, observed CDP endpoint is the next route. Do not guess extension endpoints or substitute a fresh browser for the requested session.

The `browser` helper exposes available native CDP status, launch/connect, tabs, navigation, snapshot, input, act/wait/wait_for, evaluation and screenshot operations. Discover the current schema and scope first; screenshots use `image(result)`.

The `computer` helper exposes native desktop/session capabilities. `computer.desktop_start/status/stop` and `app_launch` can use an isolated desktop; shared-host input requires the appropriate native capability and permission. Browser-target controls are preferred for browser work. The `omarchy` helper exposes available native configuration and window actions; inspect current state before changing it.

## Other entry points

| Entry point | Purpose |
| --- | --- |
| `ui.get/set/action/filter/queue` | Inspect or configure the live UI and local request queue under their native schemas. |
| `ui.message/present/ask/questions/history/harness` | User-visible messages, presentations, questions, shared history and installed harness state. |
| `resources.list/templates/read` | Relay resources. Reads are current snapshots without subscriptions. |
| `resources.forClient(id).list/templates/read` | Forward paged client-owned resources; follow returned cursors unchanged. |
| `collab.join/list/update/send/receive/ack/leave` | Coordinate members already sharing a live client. These operations do not start a model. Direct public `collab` waits up to 30 seconds; JavaScript receive waits at most one second. |
| `rsync({source,destination,dry_run,delete})` / `sync` | Managed directory-content transfer between registered clients. Paths must be existing directories; default to a dry run when reviewing changes. |
| `jobs.status/wait/cancel/list` | Inspect managed transfer jobs across code-mode runs. |
| `ports.open/list/close` | Explicit loopback forwarding between registered clients; port zero requests an unused listener. |
| `reload(id)` | Restart from the updated installed client. Background work stops and the connection ID changes; list again. |
| Public `invite` tool | Create a short-lived, one-use installation/enrollment command under the current verified OAuth authorization and published gateway release. Inspect its returned platform, expiry and command. |

Optional `controller:{name,model}` attributes calls; labels are caller-reported, not verified identity. Without a model declaration, the terminal shows an unknown model. The caller's own sandbox must be registered as a separate client before code mode can reach its workspace or loopback. Connection-bound grants end when their issuing connection ends; every lookup and forwarded call rechecks authorization.
