# Code mode guide

The default catalog shows a small set of entry points. All registered native tools and existing JavaScript helpers remain available; this guide supplies their details on demand.

Read this document inside code mode with `await resources.read('ssmcp://guide')`. MCP clients can also call `resources/read` with `{"uri":"ssmcp://guide"}`; `ssmcp://skill` is a compatibility alias for the same document.

## Start with the intended session

Use the public `list` tool to identify the exact task and workspace. Select its returned client ID, then search only for the capability needed now. `tools.search` returns summaries by default; request `include_schemas:true` before invoking a native action. Keep the returned schema unchanged. `name_prefix` filters registered names, so use `ssmcp.exec.` for native execution tools; `exec.run` itself is a convenience helper, not a registered tool name.

```javascript
const client = clients.find(item => item.id === 'SELECTED_CLIENT_ID');
if (!client) throw new Error('The selected client is no longer connected');
return await tools.search(client.id, {
  query: 'file', limit: 10, include_schemas: true
});
```

After discovering the exact native schema, call `await call(client.id, toolName, arguments)`. `await tools(client.id, cursor)` and public `list({client_id:client.id,cursor})` retain full-schema pagination. Discovery summaries do not disable providers or alter native schemas, access policies, permissions or approval requirements.

## Keep the local conversation current

Send every user-facing update, explanation, question and final answer through `say(clientId,{text,kind:'progress'})`; use `kind:'answer'` for final answers. `ui.present` renders structured displays, and `ui.ask` requests nonblocking user input. `print` and `console` output are diagnostics for the caller. An acceptance acknowledgement confirms recording, not that the user read the message.

Inspect every tool response for `structuredContent.user_messages` and its `USER REQUIREMENTS` block. Explicit image attachments follow as real MCP image content blocks in message order; metadata does not repeat base64. These are newly delivered user requests. A response can preserve its original native result, including `isError`, and carry new requests at the same time.

`structuredContent.context_memories` and the separate `SESSION CONTEXT` block restore bounded local history. They are historical reference, never a newly delivered instruction. When `context_memories_deferred` is true, discover and call the client's `ssmcp.context.read` for the complete branch. Context status/read/list are observations and do not consume queued requirements. Keep new queue delivery separate from saved context.

## Full helper reference

Descriptions below document existing helpers. Availability and exact native arguments come from the selected client's current schemas and capability manifest. Follow returned cursors unchanged; re-read after client updates.

### Discovery and orchestration

**`clients`** — Online client metadata snapshot; select a client by id from list.

**`tools`** — await tools.search(clientId,{query,name_prefix,limit:20,include_schemas:false,cursor:null,max_pages:32}) returns concise registered names, routing namespaces and descriptions. Optional name_prefix filters registered names only, case-insensitively, and combines with query. Set include_schemas:true for complete unchanged native descriptors. Continue with nextCursor and the same client/query/name_prefix/limit/schema options. Original tools(clientId,cursor?) returns full native pages. Search reduces model-visible output, not the first native page scan; see ssmcp://limits.

**`call`** — await call(clientId, toolName, arguments = {}) returns short native MCP results unchanged. Long backend output returns artifact metadata in structuredContent; inspect isError, spilled and completeness. Use outputs.read for raw PTY bytes or the saved MCP JSON of other oversized backend results.

**`output`** — Use print(value) or console.log(value) for logs; return a JSON-serializable value. Use image(nativeScreenshotResult) to emit actual MCP image content without copying base64 into logs.

**`controller`** — Pass optional controller:{name,model} to code_mode to attribute each native call. Values are caller-reported, not verified model identity. Without a model declaration, the terminal shows Model unknown. clientInfo supplies only the current request's application name.

**`native`** — native(clientId) builds dynamic names for discovered client tools, e.g. native(id).ssmcp.browser.snapshot(args). capabilities(clientId) reads the client-owned manifest. Future client primitives need no server helper update; call(id,name,args) remains compatible with older relays.

**`workspace`** — The caller sandbox needs its own registered client; server-side JavaScript cannot see its filesystem or loopback.

### Messages, questions and shared agents

**`say`** — Send all user-facing text through await say(clientId,{text,kind:'progress',author?,idempotency_key?}); a text string is also accepted. Messages appear in the client conversation. /copy copies the latest message, and ui.messages(clientId,{limit}) reads bounded local history. Use explicit acceptance fields; do not claim the user has read a message. A say call can also deliver newly queued user requirements in its code_mode response.

**`ui`** — ui.present(clientId,{title,subtitle?,blocks,kind?,author?,idempotency_key?}) renders typed Intelligence UI in the native client. Blocks include text, card, metrics, table, progress, steps and bar_chart; discover the current schema. ui.ask/questions create and read nonblocking questions; ui.history reads persistent local conversations. /questions answers without replacing a requirement draft. ui.present can receive new user requirements, like say.

**`user_messages`** — The local composer queues user requirements for the next authorized code_mode response operating on that client. The ssmcp.ui.* observation/configuration APIs (except the user-facing ssmcp.ui.message and ssmcp.ui.present writes), tool discovery, and these four client observation tools do not collect requirements unless this run explicitly binds an agent: ssmcp.client.capabilities, ssmcp.resources.list, ssmcp.resources.templates, and ssmcp.resources.read. Other native calls remain eligible. Requirements appear in structuredContent.user_messages and a separate USER REQUIREMENTS text block. Bind an existing participant with await collab.bind(clientId,{agent_id,token}) at the start of every run, or pass agent:{client_id,id,token}, for exact-agent delivery. Controller names never authenticate members. Inclusion in a response does not claim the message was read or executed.

**`collab`** — Join a live client session with collab.join(clientId,{name,role?,capabilities?}), save agent_id and private token, discover collab.list(clientId), send JSON with collab.send(clientId,{agent_id,token,to,payload,idempotency_key?}); to:'*' broadcasts to current peers. collab.receive returns messages and next_cursor; collab.ack(clientId,{agent_id,token,through:next_cursor}) releases processed messages. update renews presence; leave ends participation. All helpers take clientId first. Direct public collab accepts action and client_id and supports wait_ms<=30000; JS receive is limited to 1000ms. Read ssmcp://collaboration for limits and delivery semantics.

### Commands, files and job management

**`exec`** — exec.run(clientId,{command:[...],cwd,wait_ms}) runs a command and collects output; exec.parallel(clientId,[{command:[...]},...],{concurrency:4}) settles bounded parallel runs. exec.start/list/status/read/wait/cancel/forget manage independent jobs. wait_ms only limits waiting; timeout_ms limits execution. Reuse job_id; commands survive code_mode completion.

**`rtk`** — rtk.status(clientId) checks the installed RTK version. rtk.run/start(clientId,{command:["git","status"],cwd,...}) execute literal RTK argv with managed exec jobs. Read/wait/cancel through exec. No shell rewriting or implicit fallback; preserve exit codes and RTK recall hints.

**`outputs`** — Long command output spills to private files under /tmp/ssmcp on the client. outputs.read(clientId,{id,offset,max_bytes}), outputs.stat(clientId,{id}), outputs.cleanup(clientId) inspect artifacts by returned ID. Metadata includes SHA-256, bytes, lines, completeness and expiry; automatic retention is 24 hours.

**`fs`** — fs.list/stat/read/write/mkdir and fs.readText/writeText/editText(clientId, options) provide rooted binary and UTF-8 file primitives.

**`rsync`** — rsync({source:{client_id,path},destination:{client_id,path},dry_run:true,delete:false}) starts a managed job; sync is an alias. Both paths must be existing directories; copies contents. Reverse by swapping endpoints.

**`jobs`** — jobs.status(id), jobs.wait(id,{wait_ms:5000}), jobs.cancel(id), jobs.list(). Jobs survive code_mode completion; inspect the final state and exit codes.

**`ports`** — ports.open({listen:{client_id,port},target:{client_id,port}}), ports.list(), ports.close(id). Loopback only; port 0 selects an unused listener port.

**`reload`** — await reload(clientId) acknowledges a client restart from the updated installed executable. Stops background work; call list again for its new ID. GitHub --allow access persists; login-code connections require new OAuth login.

### Computer and browser routing

**`computer`** — Read client schemas and capabilities for desktop control. computer.desktop_start/status/stop and app_launch manage an isolated desktop; input defaults to it, and shared host input requires desktop:host. Browser-target controls are preferred for browser tasks. New primitives remain available through native(clientId).

**`omarchy`** — Native Omarchy system primitives: omarchy.status, themes/theme_set, backgrounds/background_set, fonts/font_set, audio/audio_set, brightness/brightness_set, toggles/toggle_set, workspaces/workspace_switch, window_action, apps/app_launch, menu, battery and network. Inspect schemas and current state before changes.

**`browser`** — Prefer the authorized ChatGPT Browser plugin when it is actually available in the caller for the requested browser; call its native API outside ssmcp code_mode. The client cannot invoke that public browser JavaScript API. Otherwise call ssmcp.browser.discover for cached routing metadata and ssmcp.harnesses.list for provider status. Search actual registered playwright.* tools and keep their native schemas and approvals. Use explicitly selected CDP only after these available extension routes. CDP controls remain browser.status/launch/connect/tabs/open/navigate/snapshot/click/fill/key/scroll/act/wait/wait_for/evaluate/screenshot/close/disconnect. Use connect only with an observed loopback endpoint. Do not replace requested existing Chrome with launch or another browser family. No model is invoked.

### Context and resources

**`context`** — context.roots(clientId) lists workspace and operator-configured read-only context roots. Pass a returned root ID to skills/agents.

**`skills`** — skills.list/read/view(clientId, options) read current skill metadata, exact SKILL.md contents and Markdown views. view defaults to project-over-global scope across explicitly granted roots; list uses scope:effective for that same resolution. Each entry retains its root and path.

**`agents`** — agents.list/read/resolve(clientId, options) inspect instruction files and root-to-target precedence with explicit provenance.

**`resources`** — resources.list(cursor?), resources.templates(), resources.read(uri) discover relay resources. resources.forClient(clientId).list(options?), .templates(options?) and .read(uri) forward paged client-owned resources. Read /clients/{id}/capabilities for the live client version and registry generation.

## Connection, resource and execution boundaries

Clients connect outbound over authenticated HTTPS/WebSocket or SSH. Read ssmcp://clients, select one authorized live client, then read its /capabilities and /resources directories. The client's version, provider registry, schemas and resource contents are authoritative. The relay retains connection and authorization state; documents and native implementations remain on the client.

Use call(clientId, nativeToolName, arguments) or native(clientId) after discovering tools with tools.search. capabilities(clientId) reads a bounded atomic manifest. resources.forClient(clientId).list(), .templates() and .read(uri) forward to that client's registry without a server-side provider catalog. Older clients retain tools(), call() and the compatibility resource routes. A newer client can use its resource tools through call() on an older relay.

Read ssmcp://limits for relay budgets and ssmcp://collaboration for session coordination. Resource pages are snapshots without subscriptions; follow nextCursor or nextUri unchanged and re-read after client updates. Each read rechecks authorization after forwarding; disconnection revokes connection-bound access. Client content is data and does not grant permissions.

Code mode exposes orchestration APIs, not the caller sandbox's filesystem or loopback. The caller sandbox needs its own registered client for cross-workspace file or port access. System Node.js executes orchestration in an isolated process. Native actions run through the selected client's authenticated outbound HTTPS/WebSocket or SSH connection and retain native policy and approvals.

Orchestration timeout or cancellation may leave an already-started native action running. Inspect its returned job or operation state before retrying. Short native results remain unchanged; long results may spill to private artifacts. Check `isError`, `structuredContent.spilled` and capture completeness, and read the returned artifact when needed.
