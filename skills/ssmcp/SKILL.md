---
name: ssmcp
description: Operate authenticated ssmcp workspaces through list and code_mode. Use for parallel and background commands, bounded waits, client reload, explicitly loaded coding harnesses, scoped file access and audit, standalone Zuse CLI lifecycle management, concise tool search, live project context and skill views, RTK output compression, Linux rsync transfers, and loopback TCP forwarding.
---

# ssmcp unified workspace bridge

## Discover the machines and their capabilities

Call `list` first. Use each client's optional `title` to understand its task, then check its `name` and `cwd`. Select clients by their returned `id`; titles, names and hostnames are labels and may repeat. Use `tools.search(clientId,{query,limit:20})` inside code_mode to locate concise tool summaries; request `include_schemas:true` for the complete native descriptor before calling. On older brokers, use `list({client_id})` or `tools(clientId,cursor)` pagination. Respect each native backend's permission and approval policy.

Keep each tool-catalog traversal on its returned cursor. Its pages share a snapshot even if a native tool directory changes; start a fresh cursor-free request to discover updates. A client retains at most four snapshots and 64 MiB of serialized JSON total. Each successful page read renews the 120-second idle timeout, but a snapshot expires at most 900 seconds after creation. Invalid cursors do not renew it, and reusing an unchanged catalog does not reset its creation time; LRU and capacity limits may evict it earlier. Restart discovery if the server reports that a snapshot expired or was evicted.

Current clients connect as `type:"shell"` and provide a main interactive terminal, additional independent terminals, independent command jobs, files and context tools without requiring a Harness. Load an optional provider explicitly with `ssmcp.harnesses.load` or the local load option. Only that provider is checked, started and registered after its native handshake and catalog are usable. `ssmcp.harnesses.list` reads cached status and diagnostics; an unavailable provider does not remove Shell or other tools. Remove obsolete `-t`, `--type`, `--backend` and trailing backend arguments from startup commands.

Discovery must not install runtimes, dependencies or Harnesses, submit model requests, or relax native permissions. Do not treat offline package caches as proof that nothing will be installed. Providers without a verified safe startup remain unavailable. Pi discovery connects only to an explicitly configured, already initialized native JSONL RPC session through `SSMCP_PI_RPC_SOCKET`. The absolute Unix socket and its direct parent must belong to the current UID, have no group/other permissions and not be symlinks; the connected peer UID, installed binary and version are checked before a five-second `get_state` handshake. OpenCode discovery attaches only to an existing service at `http://127.0.0.1:PORT/` (`SSMCP_OPENCODE_URL`, `OPENCODE_PASSWORD`). The explicit port and root path are required; proxies, redirects, other hosts, userinfo, query and fragment are rejected. Native discovery reads only info and OpenAPI; event subscription is lazy. Missing endpoints mean unavailable. Neither adapter starts an external service or disables user extensions to manufacture a complete catalog.

`list` and `tools` use one registry, preserving native Schema and descriptors with routing namespaces. Namespaces include `codex.*`, `claude.*`, `pi.*` and `opencode.*` after explicit loading; exact names come from the current catalog. Codex MCP tools are enumerated through native `mcpServerStatus/list` and forwarded through `mcpServer/tool/call`; select discovered `codex.mcp.*` names without hardcoding a server. Pi exposes only the `pi.rpc`, `ssmcp.pi.events`, `ssmcp.pi.requests`, and `ssmcp.pi.respond` bridge operations when available. A CLI installation or known control-command list is not an enumeration of its model, plugin or internal MCP tool Schema.

Read `ssmcp://guide`, `ssmcp://limits`, and `ssmcp://clients` through MCP resources or `resources.read(uri)` inside `code_mode`. Use `resources.templates()` for client-specific resources. Check current catalogs when a connected client has an older version.

`code_mode` runs on the MCP server. Its filesystem and loopback network are not the caller's workspace. To include a sandbox or another computer in a transfer, run a separate ssmcp client in that environment and select its returned ID. Start it from the directory to share:

```sh
cd /path/to/workspace
ssmcp --name workspace --title "Describe this session's task" --allow YOUR_GITHUB_LOGIN@github user@ssmcp.example
```

Use the operator's configured SSH destination and authorized identity. Do not copy private SSH keys, disable host-key checks, or work around a network policy. If the caller environment cannot connect, report that exact limitation and continue work on reachable peers.

## Inspect available skills and instruction files

Use `context.roots(id)` to discover the workspace and explicitly granted read-only context roots. Start with `skills.view(id)` for a live Markdown index that overlays project skills on global roots. Use `skills.list(id,{scope:"effective"})` for the same resolved metadata, or `skills.list(id)` for all project entries. Same-name valid project skills win; inspect root, path and shadowed origins. Read a selected document with `skills.read(id,{root:entry.root.id,path:entry.path})`. Follow next_cursor with the same scan options; restart without a cursor when the scan changes.

Use standard MCP resources/read as a second channel: read `ssmcp://clients/{id}/agents` for the current instruction chain, `/skills` for project metadata, `/skills?scope=effective` for resolved metadata, and `/skills/view` for the effective Markdown view. Follow each metadata entry's readUri for exact skill or instruction text. Every read fetches current files; reread after edits, additions or deletions. The transport has no subscriptions, so previously loaded host context updates only on a new read.

`agents.resolve(id,{path:"src/lib.rs"})` returns instruction origins and exact content in root-to-target order. `agents.list` returns origins only, and `agents.read` reads a selected file. At each directory ssmcp selects the first nonempty AGENTS.override.md, AGENTS.md, or agents.md. Review provenance and diagnostics before applying relevant instructions.

The default root is the client's startup workspace. An operator may add up to eight read-only roots with repeated `--context-root PATH`; use the `root` option for a single root or the effective skill view to merge them in configuration order. A root that directly contains skill folders can be scanned with `paths:["."]`. These context roots do not extend ordinary file-write access. This is a current disk view, not an inventory of a backend model's loaded prompt. Metadata parsing uses the documented YAML subset; read diagnostics and do not infer missing descriptions from body text.

## Run and manage commands

Use `exec` for independent commands with exit status and separate stdout/stderr. Pass argv arrays, an optional existing `cwd` inside the authorized workspace, and optional `env` overrides. Each start creates an independent process and pipes; it does not queue input through the interactive PTY. For pipelines or shell expansion, invoke the platform shell explicitly. A command does not inherit later `cd` or environment changes inside the interactive terminal.

```js
const client = clients.find(c => c.id === "CLIENT_ID_FROM_LIST");
if (!client) throw new Error("Select an online client from list");
return await exec.run(client.id, {
  command: ["git", "status", "--short"],
  cwd: client.cwd,
  wait_ms: 5000,
  max_bytes: 16384
});
```

Read `job_id`, `running`, `state`, `exit_code`, `stdout`, `stderr`, the two output cursors, and `snapshot`. A successful start confirms launch only. If process metadata includes `pid_role`, retain it when reporting the PID; a supervisor PID is not the command PID. Only assess the final exit when `running:false`; nonzero exit codes are command failures even when the helper resolved successfully. `snapshot` contains the latest atomic state and output metadata. `wait_timed_out` records the wait step; the subsequent read may already show completion.

Keep waiting separate from command lifetime. `wait_ms` defaults to 5000 and accepts `0..60000`; expiry leaves the command running. A command's `timeout_ms` defaults to 0, meaning no execution deadline; a positive value up to 900000 terminates its process tree on expiry. Set the outer `code_mode.timeout_ms` long enough for the entire sequence. A JavaScript deadline does not undo commands that were already started.

For long work, start with `exec.start(clientId,{command,cwd})` and return its `job_id` promptly. Continue in later requests:

| Helper | Purpose |
| --- | --- |
| `exec.list(clientId)` | Find running and retained completed jobs |
| `exec.status(clientId,jobId)` | Inspect current state, duration, exit code and artifact metadata |
| `exec.wait(clientId,jobId,{wait_ms:5000})` | Wait for completion within a bounded interval |
| `exec.read(clientId,jobId,{stdout_cursor,stderr_cursor,max_bytes,wait_ms})` | Read incremental output and an atomic `snapshot` |
| `exec.cancel(clientId,jobId)` | Terminate the owned process tree and inspect the returned state |
| `exec.forget(clientId,jobId)` | Release a completed job's retained record |
| `exec.events(clientId,{cursor,max_events,wait_ms})` | Read bounded command events |

Carry `stdout_cursor` and `stderr_cursor` separately between reads. Each stream returns at most 32768 bytes per read; `*_base64` preserves exact bytes and `*_eof` identifies its end. If `*_truncated` is true, read older bytes from the corresponding `snapshot.stdout.artifact` or `snapshot.stderr.artifact`. If observation fails after start, inspect the error's `job_id` with status/read before retrying; do not launch duplicate work.

Use `exec.parallel(clientId,[{command:[...]},...],{concurrency:4})` for up to 32 commands with 1..8 workers. Each worker waits until its command actually ends before starting the next; an initial `wait_ms:0` never releases a running slot. Results keep input order and contain fulfilled/value or rejected/reason. Observation failures retain job_id when available and stop that worker; queued entries never launched are marked not_started:true. Inspect each exit code and recover uncertain jobs before retrying. Internal bounded waits count toward code_mode's 128-call budget and outer timeout. Use exec.start for persistent servers. Parallel output defaults to 2048 bytes per stream.

Expect at most 32 running jobs and 128 retained records per client. On reaching the record limit, new starts evict the oldest completed job; running jobs stay registered. Jobs survive `code_mode` completion but stop on client disconnect or reload. `forget` does not immediately delete output artifacts. Use `exec.*` for commands; `jobs.*` manages rsync transfer jobs. Use `terminal.*` when a shared interactive shell is necessary, and do not infer command completion from one terminal read.

## Compress command output with RTK

Check `rtk.status(id)` for the client's installed RTK version. Use `rtk.run(id,{command:["git","status"],cwd:".",wait_ms:5000})` for bounded output or `rtk.start(id,{command:["cargo","test"]})` for a background job. Pass RTK arguments without the rtk prefix; the helper prepends it and preserves literal argv. Manage the returned job through exec.read/wait/write/cancel.

Inspect exit_code, running, capture completeness and RTK's recall or tee hints. RTK's supported subcommands determine compression; bypass/proxy pass output through. To recover a saved original, use `rtk.run(id,{command:["recall",returnedHash,"--full"]})` when RTK provides that identifier. The helper does not install RTK, evaluate rewritten shell text, or rerun an unfiltered command after a failure. Connection and permission errors remain errors.

## Write background input and select terminals

Start a command with stdin:"pipe" before using exec.write(clientId,jobId,{data,close_stdin:true}). Use data_base64 for binary input; never provide both forms. Each write is bounded to 32 KiB. Inspect written, remaining_bytes, timed_out and closed; retry only the unwritten suffix. Closing input sends EOF after all bytes have been accepted. Input bodies are not copied into audit logs.

Use terminal.open(clientId,{name,cwd,select}) for an independent interactive shell, then terminal.write/read/status/resize with its terminal_id. Keep output cursors separate. terminal.list reports the selected local view; terminal.select changes the view and human keyboard target while other sessions keep running. F7 cycles sessions, F6 selects audit logs. Omitting terminal_id always targets main, independently of the selected view. terminal.close ends an extra session; main ends with the connection. At most 16 terminals may be open.

Pass controller:{name,model} with the outer code_mode request when the current caller/model is known. The client shows this declaration as caller-reported, not a verified provider identity. Never guess a model from an application name. MCP clientInfo can identify the application; unknown models stay unknown. Check list's controller_metadata before relying on old clients to display declarations.

Run desktop and browser tasks directly through the standalone Zuse CLI with generic exec jobs. Use the four computer lifecycle helpers only to query, download, install or uninstall that CLI. Preserve literal argv, native output and job ownership; do not invoke another model to proxy the operation.

## Execute other native tools

```js
const client = clients.find(c => c.id === "CLIENT_ID_FROM_LIST");
if (!client) throw new Error("Select an online client from list");
return await tools.search(client.id, {query:"ssmcp.computer.", limit:10, include_schemas:true});
```

Search matches case-insensitive name/description substrings. By default it returns names, observed routing namespaces and descriptions shortened to 240 Unicode characters; include_schemas preserves the complete native descriptor. Keep the same client/query/limit/schema options when continuing with nextCursor. Search reduces model-visible catalog output and manual pagination, not the first underlying page scan. Each page is still authorized and counts toward the call budget; use max_pages for bounded work. A changed or expired page requires restarting discovery. Oversized schemas fail explicitly rather than being truncated. See ssmcp://limits for budgets; the helper requires an updated broker.

Call a discovered operation with `await call(clientId, toolName, arguments)`. Check `isError` and read `structuredContent` when present. Short results remain inline; oversized backend results return artifact metadata instead of the full body. Await every call. Return only the output needed for the task. Never start an agent or model through a native backend unless the user authorized that delegation.

The native command primitives are `ssmcp.exec.start/list/status/wait/read/write/cancel/events`, plus `ssmcp.exec.forget` for completed records. When an older server lacks `exec` helpers, call these discovered tools with `call(clientId, toolName, arguments)` and inspect the full MCP result. The schemas remain authoritative. Use `Promise.all` for independent native calls; every call is separately authorized. Concurrent Cargo commands can still contend for their shared build lock.

## Manage Zuse and execute its programs

Read the actual four-tool schema with tools.search. Call computer.version(id) first and inspect isError and structuredContent. The query runs the selected CLI's --version with a deadline; an unavailable CLI is not a reason to invent its version or install it implicitly. PATH entries take precedence over managed installations, and a failed selected executable does not silently fall back.

When download or installation is part of the authorized task, use computer.download(id,{version?}) for an official stable GitHub release and keep its download_id. Download verifies the published asset identity, byte size and SHA-256; it neither executes nor installs. Pass that exact ID to computer.install(id,{download_id}); save the returned installation_id and absolute path. Installation stays in private manager storage and does not modify PATH or replace external executables. Use computer.uninstall(id,{installation_id}) only for an installation owned by this manager; its ownership receipt and bytes must still match.

Execute a prepared Zuse program with literal argv, for example exec.start(id,{command:[zusePath,"run","automation.zuse"],cwd:projectDirectory,timeout_ms:60000}). Obtain zusePath from a real version or installation result, inspect the selected CLI's --help/schema, and prepare the program for the authorized target. Use exec.read/wait/cancel with the returned job_id. Keep dependent page/input actions ordered and reobserve after uncertain effects instead of replaying them blindly.

SSMCP has no built-in browser, desktop action backend, cua/omarchy helper, Playwright-specific bridge or internal worker. It does not create a hidden private desktop or fall back to host input. General exec retains the client's operating-system permissions.

Explicit local `ssmcp computer on|off|toggle|status` and `/computer` commands preserve compatibility for providers that read the old shared state. An explicit Off also revokes local scoped observations. For a window or DOM observation use the local operator’s separate observation grant and the exact target via `ssmcp computer locate-window` or `observe-tab`. Never use general exec to bypass a required grant. They do not control this version's lifecycle tools, general exec or the standalone Zuse CLI, and are never a prerequisite for ordinary Zuse work. The default Super+Alt+C shortcut remains removed. Status is observational: `unset` or `unavailable` is not an Off or protection claim. Publishing Off does not prove all older work has exited. Do not describe this compatibility state as an OS-wide boundary or invent a takeover step for Zuse.

When the user names an existing browser, verify that exact browser/session through the selected Zuse version's supported interface. Do not substitute a new profile or another browser family. External MCP tools remain usable only under their actually discovered names, schemas and native approvals. OAuth login and explicit human navigation remain separate from computer automation. See [Zuse lifecycle](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/computer-use.md) and [browser workflow](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/browser-desktop.md).

## Reload an updated client

After installing an update at the client's original executable entry, call `await reload(clientId)`. Check that `ssmcp.client.reload` is available first. Both client and server must support reload; an older client needs an ordinary restart for its first upgrade.

Treat `accepted: true` as acceptance, then call the public `list` again to verify reconnection and obtain the new ID. Match title, name and cwd without assuming labels are unique. Startup arguments, cwd, environment and `--allow` are preserved. Connection-owned background jobs and forwards stop, and Harness bridges close; operator-managed external Pi/OpenCode services continue. In-flight calls cause a busy rejection. Login-code authorization must be renewed after reconnection, while valid GitHub identity authorization can discover the new connection under the same allow list. Do not blindly repeat an uncertain reload.

## Manage a saved background client

Use the local CLI `ssmcp sessions list|status NAME|start NAME|stop NAME|restart NAME|logs NAME|remove NAME|auth NAME` for saved background clients; these are not additional top-level MCP tools. Management commands accept `--json`; logs accepts `--lines N` (1–1000). A saved session name differs from its changing Broker client ID. Read `manager_state` and `connection_state` separately, then use MCP `list` to confirm the current authorized connection. Read `sessions auth` only when authentication is needed; never copy its code into ordinary status or logs.

`--daemon` saves the current executable; `start`, `restart` and MCP `reload` keep that image. To upgrade, preserve the session parameters and required results, remove the saved session, then recreate it with the installed replacement. `--watchdog` retries connections; `--reconnect-delay SECONDS` defaults to 5, accepts 1–3600 and requires watchdog. Explicit stop suppresses recovery; default creation does not enable boot/login startup. Windows native crash recovery has a separate minimum 60-second delay and 255-restart limit. Disconnect still clears connection-owned tasks, routes and output access. For logs, inspect `source`, `coverage`, `fallback` and `warning`: Linux journal events and lifecycle-only records have different evidence scope; macOS/Windows currently expose lifecycle records only.

## Read long-output artifacts

Check `spilled`, `artifact`, and source-specific metadata in long-output results. PTY captures contain the full terminal session's merged raw stream; independent command artifacts contain one job's stdout or stderr; oversized MCP results retain the native result as JSON. OpenCode HTTP bodies may use `body_artifact` with `body_format` rather than a generic MCP-result artifact; short JSON bodies remain objects. Preserve the result type when reconstructing a response. Command reads expose their artifacts inside `snapshot.stdout` and `snapshot.stderr`. A spilled terminal read advances `next_cursor` to its returned range end. Read historical bytes with `outputs.read(clientId,{id:artifact.id,offset,max_bytes:16384})` and follow `next_offset`. Use `data_base64` for exact bytes, and do not return every page into the agent context at once.

Use `outputs.stat(clientId,{id})` to refresh file size, SHA-256, line count and completion metadata. A live capture can grow after a snapshot; `complete:false` or `error` must not be reported as a complete log. Underlying backend truncation cannot be recovered.

Read bounded context documents directly: the context provider preserves complete structured results up to its 1 MiB response limit and 256 KiB document limit, including through MCP resources. For other ordinary output, expect automatic spilling above 32 KiB or 500 lines, up to 24-hour retention, a 512 MiB completed-file quota and a 256 MiB individual cap. Artifacts live in the client's private storage, normally `/tmp/ssmcp/<uid>` on Unix and `%LOCALAPPDATA%\ssmcp\output` on Windows. Use returned metadata rather than assuming a path on other platforms. `outputs.cleanup(clientId)` applies normal retention without deleting active or unrelated files. Artifacts, jobs and audit records are scoped to their owning connection. Retention on disk does not grant another client or a replacement connection access; use only IDs authorized by the current interface.

## Read and write workspace files

Use the `fs` helpers in `code_mode`: `fs.list(id, options)`, `fs.stat(id, options)`, `fs.read(id, options)`, `fs.write(id, options)`, and `fs.mkdir(id, options)`. Paths are relative to the client's startup directory. Native schemas specify bounds and optional fields.

```js
return await fs.read(clients[0].id, {
  path: "README.md", offset: 0, max_bytes: 32768
});
```

Use fs.readText/writeText/editText for UTF-8 text without base64. For an existing-file change, read the complete file, confirm eof and non-null sha256, then pass expected_sha256 to the write or edit. editText requires exactly one nonempty literal match and rejects ambiguous or stale changes. Text pagination follows next_offset without splitting UTF-8 characters.

Binary file contents are base64, so binary bytes survive transport. Follow returned offsets for chunked reads. Write helpers replace a bounded file atomically; set `overwrite:true` only for an intended replacement. Use rsync for directory trees and large files.

Read `ssmcp.fs.events` for successful native file access and write evidence. Reads emit `ssmcp.file.read`; atomic writes and edits emit `ssmcp.file.write` and `ssmcp.file.diff` only after successful commit. Snapshots distinguish missing, known text and unknown content. Unknown, sensitive, binary, oversized or inaccessible snapshots have unknown line counts rather than a fabricated empty preimage. Comparable text has actual added/removed counts. Inspect `diff_complete`, `diff_unavailable_reason`, and `diff_redacted`; small complete diffs are inline, while `diff_artifact` points to the shared output store for larger diffs. A preview is not the complete artifact.

Harness lifecycle events record provider, tool, call ID, status and duration. External file effects remain `file_audit:"unknown"` unless the native interface supplies evidence; never infer successful writes from arguments or tool-name substrings. On macOS and Windows, hash-precondition writes serialize cooperating calls within one client process; they do not lock out unrelated processes.

Skill discovery and reading are distinct from invocation. `ssmcp.skill.discovered` reports actual metadata, `ssmcp.skill.read` reports a body read, and `ssmcp.skill.discovery.completed` summarizes a scan. These operations do not execute a skill or prove a model used it. Report invoked only with explicit native evidence. Skill selection uses root and path, not an assumed unique name.

## Synchronize with system rsync

Check that both clients expose the rsync transport; it is currently available on Linux clients and requires system `rsync`. Install dependencies with the system package manager. Both selected directories must exist; create the destination with `fs.mkdir` when authorized. The operation copies directory contents, preserving ordinary files, directories and modification times. It skips symlinks and devices and does not enable preservation of ownership, permission bits, ACLs or extended attributes. Start with a dry run:

```js
return await rsync({
  source: { client_id: sourceId, path: "project/" },
  destination: { client_id: destinationId, path: "project/" },
  dry_run: true,
  delete: false
});
```

The response identifies a managed background job. Use `jobs.wait(id,{wait_ms:5000})` or `jobs.status(id)` until it finishes. Check the terminal state and both rsync exit codes. Review the dry-run changes, then run with `dry_run:false` for an already authorized copy. Enable `delete:true` only when destination deletions are part of the requested synchronization. Use `jobs.cancel(id)` to stop a running transfer.

Swap the two endpoints to transfer in the reverse direction. These are directional synchronizations; do not run opposing copies concurrently or describe them as conflict-aware merging. Inspect conflicting edits before choosing an authoritative source.

## Forward a port

```js
return await ports.open({
  listen: { client_id: workspaceId, host: "127.0.0.1", port: 8080 },
  target: { client_id: developmentId, host: "127.0.0.1", port: 3000 }
});
```

Confirm the returned state is `running` before reading the listener address on the listening client. The open call waits at most eight seconds; if it returns `starting`, use `ports.list()` to observe the same ID until its listener is ready. A `cleanup_failed` state retains pending handles; retry close or cancel and inspect the result. Swap endpoint IDs for the reverse direction. List mappings with `ports.list()` and close one with `ports.close(id)` when done. Listeners and targets are limited to loopback; the mapping remains managed after the JavaScript invocation finishes and ends on closure, expiry, authorization loss, or disconnection.

## Wait for the real user before ending

Use `return await ui.wait(clientId,{intent:"need_user",timeout_ms:20000})` when user input is needed. Only the final outer `structuredContent.user_wait` entry with `status:"received"` and matching `user_messages` confirms message delivery. Read those messages before continuing.

Before ending, bind the existing asking participant and request a real local Human No/Yes decision:

```js
await collab.bind(clientId, {agent_id: participant.agent_id, token: participant.token});
return await ui.wait(clientId, {
  intent: "finish",
  reason: "The work is ready. May this conversation end now?",
  timeout_ms: 20000,
  notify: false,
});
```

Use the actual participant ID and private token; display labels grant no authority. A finish wait requires a verified thread. Already queued user input is delivered normally and never counts as approval. Each run allows one wait per client. After timeout, bind the same thread in a new call and pass the actual returned `finish_request_id` to resume; do not invent IDs.

Inspect the final outer receipt for the exact client and wait. End only with `status:"decided"`, `may_finish:true`, matching request IDs, and `finish_decision` containing `status:"completed"`, `choice:"yes"`, `actor:{origin:"human"}`, a positive revision, `consumed:true`, and `invalidation_reason:null`. An inner `decision_ready`, a question-list read, Human No, text saying yes, timeout, cancel, new input, lost connection or revoked authority is not approval. A consumed decision cannot be replayed; another agent cannot approve for the user.

Progress and ordinary answer/reply messages remain available. This protocol does not intercept arbitrary external model final replies or force a model to stay running. Do not revive the retired `request_finish`, `ui.finish` or `ui.answer` tools. The live red countdown is the current wait's remaining budget; waking the screen does not answer or cancel it. See [user waiting](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/user-wait.md) for the full contract.

## Observe activity and respect lifecycle

Background command stdout/stderr is displayed while it runs. Keep job IDs, stream sources and cursors separate; a historical read or wait result must not be reported as new live output. Pretty, verbose and JSONL audit logs use a redacted copy of events. Do not print environment mappings, credentials or sensitive file bodies. Native return values preserve their types and permission states; sanitizing the audit copy does not change a provider's approval decision.

Keep the client process running while using its tools. Disconnect and reload invalidate the old registry and stop connection-owned work. Closing an attached external native transport does not terminate its operator-managed service. Re-select the current ID from `list`; labels are not identities. Never copy private SSH keys, disable host-key checks, or bypass native permission and approval requests. Use `exec.cancel` for commands and `jobs.cancel` for the separate rsync transfer jobs.

## Verify and report

For commands, report final state and exit code, or a still-running `job_id`; inspect log completeness before describing output as complete. For Codex MCP integration, preserve separate evidence for actual dynamic enumeration and a real native call, including original errors and approval state. Protocol fixtures alone do not establish compatibility with an installed provider or its configured servers. For reload, verify the replacement connection with `list`. For transfers, verify the intended files and report source, destination, job state, and dry-run status. For forwarding, make a real request through the returned local port, then close temporary mappings. Distinguish code changes, local tests, committed code, and deployed software. Never report a live sandbox connection from a test that used two development-host peers.
