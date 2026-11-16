---
name: ssmcp
description: Operate SSH-connected ssmcp workspaces through list and code_mode. Use for remote shell or coding tools, scoped file access, system-rsync transfers, loopback TCP forwarding, and discovery of live MCP resources.
---

# ssmcp workspaces

## Discover the machines and their capabilities

Call `list` first. Select clients by their returned `id`; names and hostnames are labels. Call `list({client_id})` and follow `nextCursor` before using a native tool. Respect each native backend's permission and approval policy.

Read `ssmcp://guide`, `ssmcp://limits`, and `ssmcp://clients` through MCP resources or `resources.read(uri)` inside `code_mode`. Use `resources.templates()` for client-specific resources. Check current catalogs when a connected client has an older version.

`code_mode` runs on the MCP server. Its filesystem and loopback network are not the caller's workspace. To include a sandbox or another computer in a transfer, run a separate ssmcp client in that environment and select its returned ID. Start it from the directory to share:

```sh
cd /path/to/workspace
ssmcp --name workspace --allow YOUR_GITHUB_LOGIN@github user@ssmcp.example
```

Use the operator's configured SSH destination and authorized identity. Do not copy private SSH keys, disable host-key checks, or work around a network policy. If the caller environment cannot connect, report that exact limitation and continue work on reachable peers.

## Execute native tools

```js
const client = clients.find(c => c.name === "development");
if (!client) throw new Error("Select an online client from list");
return await tools(client.id);
```

Call a discovered operation with `await call(clientId, toolName, arguments)`. This returns the complete MCP result; check `isError` and read `structuredContent` when present. Await every call. Return only the output needed for the task. Never start an agent or model through a native backend unless the user authorized that delegation.

## Read and write workspace files

Use the `fs` helpers in `code_mode`: `fs.list(id, options)`, `fs.stat(id, options)`, `fs.read(id, options)`, `fs.write(id, options)`, and `fs.mkdir(id, options)`. Paths are relative to the client's startup directory. Native schemas specify bounds and optional fields.

```js
return await fs.read(clients[0].id, {
  path: "README.md", offset: 0, max_bytes: 32768
});
```

File contents are base64, so binary bytes survive transport. Follow returned offsets for chunked reads. Write helpers replace a bounded file atomically; set `overwrite:true` only for an intended replacement. Use rsync for directory trees and large files.

## Synchronize with system rsync

Both clients need system `rsync`; install dependencies with the system package manager. Both selected directories must exist; create the destination with `fs.mkdir` when authorized. The operation copies directory contents, preserving ordinary files, directories and modification times. It skips symlinks, devices, ownership, permission bits and extended attributes. Start with a dry run:

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

Read the returned listener address on the listening client. A `cleanup_failed` state retains pending handles; retry close or cancel and inspect the result. Swap endpoint IDs for the reverse direction. List mappings with `ports.list()` and close one with `ports.close(id)` when done. Listeners and targets are limited to loopback; the mapping remains managed after the JavaScript invocation finishes and ends on closure, expiry, authorization loss, or disconnection.

## Verify and report

For transfers, verify the intended files and report source, destination, job state, and dry-run status. For forwarding, make a real request through the returned local port, then close temporary mappings. Distinguish code changes, local tests, committed code, and deployed software. Never report a live sandbox connection from a test that used two development-host peers.
