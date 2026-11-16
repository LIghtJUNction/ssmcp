---
name: ssmcp
description: Operate SSH-connected ssmcp shell clients through list and code_mode. Each client auto-registers installed coding harnesses, native MCP tools, workspace file/skill audit and independent background commands.
---

# ssmcp unified client

## Discover clients and the combined tool catalog

Call `list` first. Use each client's optional `title` to understand its task, then check its `name` and `cwd`. Select clients by their returned `id`; labels may repeat. The unified client uses a Shell/PTY entry point and probes installed Codex, Claude Code, Pi and OpenCode providers. Available tools depend on the installed version and successful native discovery; do not pass a harness selection mode.

Call list({client_id}) and follow nextCursor to inspect the merged native tool catalog. The catalog includes terminal.*, ssmcp.exec.*, ssmcp.fs.*, ssmcp.skills.*, and each installed harness namespace:
- Codex: codex.* and ssmcp.codex.*
- Claude Code: cc.* and ssmcp.cc.*
- Pi: pi.* and ssmcp.pi.*
- OpenCode: opencode.*

Discover every capability through `list({client_id})` or `tools(clientId,cursor)` before calling it, and preserve the returned native arguments, errors and approvals. `ssmcp.harnesses.list`, when available, reports discovery information. A CLI installation alone is not a complete tool inventory; use the advertised scope and diagnostics. Do not manufacture a missing Schema or infer tools from a provider name.

```sh
cd /path/to/workspace
ssmcp --name workspace --title "Describe this session's task" --allow YOUR_GITHUB_LOGIN@github user@ssmcp.example
```

## Execute tools in parallel

Use Promise.all for independent native calls. Each call is still authorized and checked separately.

## Inspect available skills and instruction files

Use `context.roots(id)` to see the workspace and any explicit read-only context roots. Start with `skills.view(id)` for a compact Markdown index or `skills.list(id,options)` for metadata. Follow `next_cursor` using the same scan options. Names can repeat; choose by root and path, then call `skills.read(id,{path})` for the full SKILL.md.

`agents.resolve(id,{path:"src/lib.rs"})` returns instruction origins and exact content in root-to-target order. `agents.list` returns origins only, and `agents.read` reads a selected file. At each directory ssmcp selects the first nonempty AGENTS.override.md, AGENTS.md, or agents.md. Review provenance and diagnostics before applying relevant instructions.

The default root is the client's startup workspace. An operator may add up to eight read-only roots with repeated `--context-root PATH`; select their returned IDs explicitly with the `root` option. A root that directly contains skill folders can be scanned with `paths:["."]`. These context roots do not extend ordinary file-write access. This is a current disk view, not an inventory of a backend model's loaded prompt. Metadata parsing uses the documented YAML subset; read diagnostics and do not infer missing descriptions from body text.

## Execute native tools

Call a discovered operation with `await call(clientId, toolName, arguments)`. Check `isError` and read `structuredContent` when present. Short results remain inline; oversized backend results return artifact metadata instead of the full body. Await every call. Return only the output needed for the task. Never start an agent or model through a native backend unless the user authorized that delegation.

## Reload an updated client

After installing an update at the client's original executable entry, call `await reload(clientId)`. Check that `ssmcp.client.reload` is available first. Both client and server must support reload; an older client needs an ordinary restart for its first upgrade.

Treat `accepted: true` as acceptance, then call the public `list` again to verify reconnection and obtain the new ID. Match title, name and cwd without assuming labels are unique. Startup arguments, cwd, environment and `--allow` are preserved. Background jobs, harnesses and forwards stop; in-flight calls cause a busy rejection. Login-code authorization must be renewed after reconnection, while valid GitHub identity authorization can discover the new connection under the same allow list. Do not blindly repeat an uncertain reload.

## Read long-output artifacts

Check `spilled` and `artifact` in long-output results. PTY captures contain the full terminal session's merged raw stream; other oversized backend results contain MCP JSON. A spilled terminal read advances `next_cursor` to its returned range end, so read historical bytes with `outputs.read(clientId,{id:artifact.id,offset,max_bytes:16384})` and follow `next_offset`. Use `data_base64` for exact bytes, and do not return every page into the agent context at once.

Use `outputs.stat(clientId,{id})` to refresh file size, SHA-256, line count and completion metadata. A live capture can grow after a snapshot; `complete:false` or `error` must not be reported as a complete log. Underlying backend truncation cannot be recovered.

Expect automatic spilling above 32 KiB or 500 lines, 24-hour retention, a 512 MiB completed-file quota and a 256 MiB individual cap. Artifacts live on the client in `/tmp/ssmcp/<uid>`. `outputs.cleanup(clientId)` applies normal retention without deleting active or unrelated files. After reconnection, use the new client ID and verify that `outputs.stat` authorizes the requested artifact. Do not assume that artifacts belong to another client merely because both clients run as the same local user.

## Run independent shell commands

Use `ssmcp.exec.start` for independent commands and reserve `terminal.*` for interactive shell state. Pass an argv array; a successful start returns a job ID. It does not report command completion. Check `isError`, then use the task's final status and exit code.

~~~js
const matches = clients.filter(c => c.name === "workstation");
if (matches.length !== 1) throw new Error("Select a unique client ID from list");
const client = matches[0];
const jobs = await Promise.all([
  call(client.id, "ssmcp.exec.start", {
    command: ["cargo", "test", "--locked"],
    cwd: client.cwd,
    timeout_ms: 600000
  }),
  call(client.id, "ssmcp.exec.start", {
    command: ["git", "status", "--short", "--branch"],
    cwd: client.cwd
  })
]);
return jobs;
~~~

Manage jobs through `call(clientId, toolName, arguments)`, using the live Schema:

- ssmcp.exec.list()
- ssmcp.exec.status({job_id})
- ssmcp.exec.wait({job_id,wait_ms})
- ssmcp.exec.read({job_id,stdout_cursor,stderr_cursor,max_bytes,wait_ms})
- ssmcp.exec.cancel({job_id})
- ssmcp.exec.events({cursor})

stdout and stderr have independent cursors. Follow the returned cursors and artifact metadata to obtain all retained output. Read and wait limits are defined by the live Schema; a wait may return while a task is still running. Tasks belong to their client connection. After cancellation, timeout or disconnect, verify the terminal state before reporting completion or retrying work.

## Files, diffs and audit

Use fs.readText/writeText/editText for UTF-8 text without base64. For an existing-file change, read the complete file, confirm eof and non-null sha256, then pass expected_sha256 to the write or edit. editText requires exactly one nonempty literal match and rejects ambiguous or stale changes. Text pagination follows next_offset without splitting UTF-8 characters.

Binary file contents are base64, so binary bytes survive transport. Follow returned offsets for chunked reads. Write helpers replace a bounded file atomically; set `overwrite:true` only for an intended replacement. Use rsync for directory trees and large files.

Workspace paths are relative to the selected client's startup directory. Symlinks and parent traversal are rejected. Use ssmcp.fs.list/stat/read/write/mkdir. File writes are atomic; overwrite must be explicit.

Inspect successful file-operation results and available events for confirmed paths, changes and Diff metadata. Use `ssmcp.fs.events` only when it appears in the current catalog. A path in a Harness argument is a request, not evidence that the file was read or written. Treat unavailable snapshots and unreported external operations as unobserved; do not present complete audit coverage.

## Interpret skill activity

Use the context-root and path interfaces above. `skills.list/view` discovers metadata; `skills.read(clientId,{path,root})` reads a full document. Neither operation invokes a model or executes a skill. Report an actual skill invocation only when the native interface provides evidence of that invocation. Do not infer it from a tool-name substring.

## Observe local activity

Observe the connected terminal and event interfaces advertised by the current catalog. Keep each task's stdout and stderr identities and cursors separate; do not replay historical output as a new live event. Audit claims must match the successfully observed operation. The caller model is not assumed: if MCP does not expose it, treat it as Unknown. A harness model/provider is only shown when the harness reports authoritative metadata.

## Safety and lifecycle

- Keep the client process running while using its tools.
- Never copy private SSH keys, disable host-key checks, or work around network policy.
- Native permissions and approvals remain in force.
- Use the returned client id for every call. Reconnecting creates a new id.
- Use `ssmcp.exec.cancel` for independent command tasks; `jobs.cancel` manages the separate transfer jobs created by `rsync/sync`.
