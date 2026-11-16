---
name: ssmcp
description: Operate SSH-connected ssmcp workspaces through list and code_mode. Use for parallel and background commands, bounded waits, client reload, automatically discovered coding harnesses, scoped file access and audit, skills and instruction discovery, Linux rsync transfers, and loopback TCP forwarding.
---

# ssmcp unified workspace bridge

## Discover the machines and their capabilities

Call `list` first. Use each client's optional `title` to understand its task, then check its `name` and `cwd`. Select clients by their returned `id`; titles, names and hostnames are labels and may repeat. Call `list({client_id})` and follow `nextCursor` before using a native tool. Respect each native backend's permission and approval policy.

Keep each tool-catalog traversal on its returned cursor. Its pages share a snapshot even if a native tool directory changes; start a fresh cursor-free request to discover updates. A client retains at most four snapshots, 64 MiB of serialized JSON total, for 120 seconds. Restart discovery if the server reports that a snapshot expired or was evicted.

Current clients connect as `type:"shell"` and provide a shared interactive terminal, independent command jobs, files and context tools without requiring a Harness. Background discovery checks installed providers and registers only interfaces whose version, startup policy, handshake and actual catalog are usable. `ssmcp.harnesses.list` reports status and diagnostics; an unavailable provider does not remove Shell or other tools. Remove obsolete `-t`, `--type`, `--backend` and trailing backend arguments from startup commands.

Discovery must not install runtimes, dependencies or Harnesses, submit model requests, or relax native permissions. Do not treat offline package caches as proof that nothing will be installed. Providers without a verified safe startup remain unavailable. OpenCode discovery attaches only to an explicitly configured existing loopback service (`SSMCP_OPENCODE_URL`, `OPENCODE_PASSWORD`); it does not start a new service or disable user plugins to manufacture a complete catalog.

`list` and `tools` use one registry, preserving native Schema and descriptors with routing namespaces. Namespaces include `codex.*`, `claude.*`, `pi.*`, and `opencode.*`; exact names come from the current catalog. Codex MCP tools are enumerated through native `mcpServerStatus/list` and forwarded through `mcpServer/tool/call`; select discovered `codex.mcp.*` names without hardcoding a server. Pi exposes only the `pi.rpc`, `ssmcp.pi.events`, `ssmcp.pi.requests`, and `ssmcp.pi.respond` bridge operations when available. A CLI installation or known control-command list is not an enumeration of its model, plugin or internal MCP tool Schema.

Read `ssmcp://guide`, `ssmcp://limits`, and `ssmcp://clients` through MCP resources or `resources.read(uri)` inside `code_mode`. Use `resources.templates()` for client-specific resources. Check current catalogs when a connected client has an older version.

`code_mode` runs on the MCP server. Its filesystem and loopback network are not the caller's workspace. To include a sandbox or another computer in a transfer, run a separate ssmcp client in that environment and select its returned ID. Start it from the directory to share:

```sh
cd /path/to/workspace
ssmcp --name workspace --title "Describe this session's task" --allow YOUR_GITHUB_LOGIN@github user@ssmcp.example
```

Use the operator's configured SSH destination and authorized identity. Do not copy private SSH keys, disable host-key checks, or work around a network policy. If the caller environment cannot connect, report that exact limitation and continue work on reachable peers.

## Inspect available skills and instruction files

Use `context.roots(id)` to see the workspace and any explicit read-only context roots. Start with `skills.view(id)` for a compact Markdown index or `skills.list(id,options)` for metadata. Follow `next_cursor` using the same scan options. Names can repeat; choose by root and path, then call `skills.read(id,{path})` for the full SKILL.md.

`agents.resolve(id,{path:"src/lib.rs"})` returns instruction origins and exact content in root-to-target order. `agents.list` returns origins only, and `agents.read` reads a selected file. At each directory ssmcp selects the first nonempty AGENTS.override.md, AGENTS.md, or agents.md. Review provenance and diagnostics before applying relevant instructions.

The default root is the client's startup workspace. An operator may add up to eight read-only roots with repeated `--context-root PATH`; select their returned IDs explicitly with the `root` option. A root that directly contains skill folders can be scanned with `paths:["."]`. These context roots do not extend ordinary file-write access. This is a current disk view, not an inventory of a backend model's loaded prompt. Metadata parsing uses the documented YAML subset; read diagnostics and do not infer missing descriptions from body text.

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

Use `exec.parallel(clientId,[{command:[...]},...],{concurrency:4})` for up to 32 commands. Concurrency accepts `1..8`. Results keep input order and contain `fulfilled/value` or `rejected/reason`; retain every result and check each exit code. Output defaults to 2048 bytes per stream per command. Concurrency limits simultaneous start/wait/read sequences. When a wait expires, that command continues in the background and a new sequence may start, so running processes can exceed the concurrency setting. For a strict running-process limit, wait for a job to finish before filling its slot.

Expect at most 32 running jobs and 128 retained records per client. On reaching the record limit, new starts evict the oldest completed job; running jobs stay registered. Jobs survive `code_mode` completion but stop on client disconnect or reload. `forget` does not immediately delete output artifacts. Use `exec.*` for commands; `jobs.*` manages rsync transfer jobs. Use `terminal.*` when a shared interactive shell is necessary, and do not infer command completion from one terminal read.

## Execute other native tools

```js
const client = clients.find(c => c.id === "CLIENT_ID_FROM_LIST");
if (!client) throw new Error("Select an online client from list");
return await tools(client.id);
```

Call a discovered operation with `await call(clientId, toolName, arguments)`. Check `isError` and read `structuredContent` when present. Short results remain inline; oversized backend results return artifact metadata instead of the full body. Await every call. Return only the output needed for the task. Never start an agent or model through a native backend unless the user authorized that delegation.

The native command primitives are `ssmcp.exec.start/list/status/wait/read/cancel/events`, plus `ssmcp.exec.forget` for completed records. When an older server lacks `exec` helpers, call these discovered tools with `call(clientId, toolName, arguments)` and inspect the full MCP result. The schemas remain authoritative. Use `Promise.all` for independent native calls; every call is separately authorized. Concurrent Cargo commands can still contend for their shared build lock.

## Reload an updated client

After installing an update at the client's original executable entry, call `await reload(clientId)`. Check that `ssmcp.client.reload` is available first. Both client and server must support reload; an older client needs an ordinary restart for its first upgrade.

Treat `accepted: true` as acceptance, then call the public `list` again to verify reconnection and obtain the new ID. Match title, name and cwd without assuming labels are unique. Startup arguments, cwd, environment and `--allow` are preserved. Background jobs, harnesses and forwards stop; in-flight calls cause a busy rejection. Login-code authorization must be renewed after reconnection, while valid GitHub identity authorization can discover the new connection under the same allow list. Do not blindly repeat an uncertain reload.

## Read long-output artifacts

Check `spilled`, `artifact`, and source-specific metadata in long-output results. PTY captures contain the full terminal session's merged raw stream; independent command artifacts contain one job's stdout or stderr; oversized MCP results retain the native result as JSON. OpenCode HTTP bodies may use `body_artifact` with `body_format` rather than a generic MCP-result artifact; short JSON bodies remain objects. Preserve the result type when reconstructing a response. Command reads expose their artifacts inside `snapshot.stdout` and `snapshot.stderr`. A spilled terminal read advances `next_cursor` to its returned range end. Read historical bytes with `outputs.read(clientId,{id:artifact.id,offset,max_bytes:16384})` and follow `next_offset`. Use `data_base64` for exact bytes, and do not return every page into the agent context at once.

Use `outputs.stat(clientId,{id})` to refresh file size, SHA-256, line count and completion metadata. A live capture can grow after a snapshot; `complete:false` or `error` must not be reported as a complete log. Underlying backend truncation cannot be recovered.

Expect automatic spilling above 32 KiB or 500 lines, up to 24-hour retention, a 512 MiB completed-file quota and a 256 MiB individual cap. Artifacts live in the client's private storage, normally `/tmp/ssmcp/<uid>` on Unix and `%LOCALAPPDATA%\ssmcp\output` on Windows. Use returned metadata rather than assuming a path on other platforms. `outputs.cleanup(clientId)` applies normal retention without deleting active or unrelated files. Artifacts, jobs and audit records are scoped to their owning connection. Retention on disk does not grant another client or a replacement connection access; use only IDs authorized by the current interface.

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

## Observe activity and respect lifecycle

Background command stdout/stderr is displayed while it runs. Keep job IDs, stream sources and cursors separate; a historical read or wait result must not be reported as new live output. Pretty, verbose and JSONL audit logs use a redacted copy of events. Do not print environment mappings, credentials or sensitive file bodies. Native return values preserve their types and permission states; sanitizing the audit copy does not change a provider's approval decision.

Keep the client process running while using its tools. Disconnect and reload invalidate the old registry and stop connection-owned work. Re-select the current ID from `list`; labels are not identities. Never copy private SSH keys, disable host-key checks, or bypass native permission and approval requests. Use `exec.cancel` for commands and `jobs.cancel` for the separate rsync transfer jobs.

## Verify and report

For commands, report final state and exit code, or a still-running `job_id`; inspect log completeness before describing output as complete. For Codex MCP integration, preserve separate evidence for actual dynamic enumeration and a real native call, including original errors and approval state. Protocol fixtures alone do not establish compatibility with an installed provider or its configured servers. For reload, verify the replacement connection with `list`. For transfers, verify the intended files and report source, destination, job state, and dry-run status. For forwarding, make a real request through the returned local port, then close temporary mappings. Distinguish code changes, local tests, committed code, and deployed software. Never report a live sandbox connection from a test that used two development-host peers.
