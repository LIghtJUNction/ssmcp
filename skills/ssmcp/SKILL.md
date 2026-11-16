---
name: ssmcp
description: Operate SSH-connected ssmcp workspaces through list and code_mode. Use for remote shell or coding tools, scoped file access, system-rsync transfers, loopback TCP forwarding, and discovery of live MCP resources.
---

# ssmcp workspaces

## Discover the machines and their capabilities

Call `list` first. Use each client's optional `title` to understand its task, then check its `name` and `cwd`. Select clients by their returned `id`; titles, names and hostnames are labels and may repeat. Call `list({client_id})` and follow `nextCursor` before using a native tool. Respect each native backend's permission and approval policy.

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

## Execute native tools

```js
const client = clients.find(c => c.name === "development");
if (!client) throw new Error("Select an online client from list");
return await tools(client.id);
```

Call a discovered operation with `await call(clientId, toolName, arguments)`. Check `isError` and read `structuredContent` when present. Short results remain inline; oversized backend results return artifact metadata instead of the full body. Await every call. Return only the output needed for the task. Never start an agent or model through a native backend unless the user authorized that delegation.

## Reload an updated client

After installing an update at the client's original executable entry, call `await reload(clientId)`. Check that `ssmcp.client.reload` is available first. Both client and server must support reload; an older client needs an ordinary restart for its first upgrade.

Treat `accepted: true` as acceptance, then call the public `list` again to verify reconnection and obtain the new ID. Match title, name and cwd without assuming labels are unique. Startup arguments, cwd, environment and `--allow` are preserved. Background jobs, harnesses and forwards stop; in-flight calls cause a busy rejection. Login-code authorization must be renewed after reconnection, while valid GitHub identity authorization can discover the new connection under the same allow list. Do not blindly repeat an uncertain reload.

## Read long-output artifacts

Check `spilled` and `artifact` in long-output results. PTY captures contain the full terminal session's merged raw stream; other oversized backend results contain MCP JSON. A spilled terminal read advances `next_cursor` to its returned range end, so read historical bytes with `outputs.read(clientId,{id:artifact.id,offset,max_bytes:16384})` and follow `next_offset`. Use `data_base64` for exact bytes, and do not return every page into the agent context at once.

Use `outputs.stat(clientId,{id})` to refresh file size, SHA-256, line count and completion metadata. A live capture can grow after a snapshot; `complete:false` or `error` must not be reported as a complete log. Underlying backend truncation cannot be recovered.

Expect automatic spilling above 32 KiB or 500 lines, 24-hour retention, a 512 MiB completed-file quota and a 256 MiB individual cap. Artifacts live on the client in `/tmp/ssmcp/<uid>`. `outputs.cleanup(clientId)` applies normal retention without deleting active or unrelated files. An unexpired artifact may remain readable through the reconnected client ID.

## Read and write workspace files

Use the `fs` helpers in `code_mode`: `fs.list(id, options)`, `fs.stat(id, options)`, `fs.read(id, options)`, `fs.write(id, options)`, and `fs.mkdir(id, options)`. Paths are relative to the client's startup directory. Native schemas specify bounds and optional fields.

```js
return await fs.read(clients[0].id, {
  path: "README.md", offset: 0, max_bytes: 32768
});
```

Use fs.readText/writeText/editText for UTF-8 text without base64. For an existing-file change, read the complete file, confirm eof and non-null sha256, then pass expected_sha256 to the write or edit. editText requires exactly one nonempty literal match and rejects ambiguous or stale changes. Text pagination follows next_offset without splitting UTF-8 characters.

Binary file contents are base64, so binary bytes survive transport. Follow returned offsets for chunked reads. Write helpers replace a bounded file atomically; set `overwrite:true` only for an intended replacement. Use rsync for directory trees and large files.

## Synchronize with system rsync

Both clients need system `rsync`; install dependencies with the system package manager. Both selected directories must exist; create the destination with `fs.mkdir` when authorized. The operation copies directory contents, preserving ordinary files, directories and modification times. It skips symlinks and devices and does not enable preservation of ownership, permission bits, ACLs or extended attributes. Start with a dry run:

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

## Verify and report

For transfers, verify the intended files and report source, destination, job state, and dry-run status. For forwarding, make a real request through the returned local port, then close temporary mappings. Distinguish code changes, local tests, committed code, and deployed software. Never report a live sandbox connection from a test that used two development-host peers.
