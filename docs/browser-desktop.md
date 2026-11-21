# Standalone Zuse for desktop and browser tasks

Desktop and browser operations run directly in the standalone Zuse CLI through SSMCP's generic exec tools. SSMCP supplies connection authorization, literal argv, process ownership and output collection. Its computer tools manage the CLI installation; they do not implement a browser or desktop action protocol.

Start with the [four lifecycle tools](computer-use.md). Query the actual executable, read that version's `--help` and `schema`, then prepare a Zuse program for the authorized target. Do not infer supported operations from an old SSMCP tool catalog.

## Run an explicit program

Use the executable path returned by `computer.version` or `computer.install`:

```javascript
return await exec.start(clientId, {
  command: [zusePath, "run", "automation.zuse"],
  cwd: projectDirectory,
  timeout_ms: 60000
});
```

The placeholders above refer to the actual executable, authorized workspace and prepared program. Keep the returned `job_id`; inspect stdout, stderr, exit status and completeness through `exec.read/wait`. Use `exec.cancel` for that owned job when cancellation is needed. A successful process launch does not prove that a page or desktop action completed.

Keep dependent actions within the same Zuse process. A one-shot invocation does not promise to retain its browser or application state for a later process; use the CLI's documented persistent mode when the workflow requires it. After an interrupted operation, observe the target before deciding what to repeat. [Zuse CLI and program examples](https://github.com/LIghtJUNction/zuse#readme) · [SSMCP command lifecycle](code-mode.md#exec)

## Select the intended desktop or browser

For an existing browser, identify the actual browser, endpoint and target supported by the installed Zuse version. Reuse the user's selected session only when that interface is available and authorized. Do not replace it with a fresh profile or another browser family. Zuse's current documentation distinguishes launching an isolated browser, connecting to an explicit debugging endpoint and selecting an existing page. [Existing-session workflow](browser-existing-session.md)

Desktop permissions, display selection and application isolation belong to the selected CLI and its operating-system environment. SSMCP no longer starts an internal private desktop, routes input to a built-in host backend, or provides a Computer Use toggle. Generic exec retains the client's OS permissions; it is not an OS-wide desktop permission switch.

## Retired SSMCP entry points

The old `ssmcp.browser.*`, `ssmcp.omarchy.*`, desktop action methods under `ssmcp.computer.*`, and `browser` / `cua` / `omarchy` JavaScript facades are removed. The four retained computer methods are `version`, `download`, `install` and `uninstall`. There is no `ssmcp setup browser`, Playwright-specific bridge or internal worker fallback.

Opening an OAuth authorization page or a link explicitly requested by the local user remains supported. Generic external MCP forwarding also preserves discovered schemas and approvals; it does not recreate SSMCP's retired action APIs.

## Client-owned capabilities and resources

The relay authenticates connections, enforces connection access and transport
budgets, and forwards operations. The client owns native tool schemas,
capability summaries, usage guides, native limits and project resources.

```javascript
const manifest = await capabilities(clientId);
const resourcesForClient = resources.forClient(clientId);
const firstPage = await resourcesForClient.list({limit: 2});
const guide = await resourcesForClient.read("ssmcp-client://guide");
const api = native(clientId);
// Any future registered tool can use the same dynamic call path.
```

The manifest identifies client version, wire version, registry generation,
tool counts and provider counts. The registry produces one consistent summary
under a read lock. Full schemas remain in the client's existing paginated tool
catalog. Resources are read on demand; the relay does not retain a second
document or tool catalog cache.

New client resource endpoints are:

| Relay resource | Purpose |
| --- | --- |
| `ssmcp://clients/{id}/capabilities` | Client capability manifest |
| `ssmcp://clients/{id}/resources` | Paginated client resource catalog |
| `ssmcp://clients/{id}/resource-templates` | Client URI templates |
| `ssmcp://clients/{id}/resource?uri=...` | Generic client resource read |

Unknown future resource types and extension fields pass through the generic
route. Authorization is checked again after remote I/O. Use each client's advertised wire version and actual schemas;
native `call()` remains the common forwarding path. A new client can expose these
capabilities through `call()` on an older relay; the new dynamic JavaScript
conveniences and generic relay resource URI entry points require the updated
relay.

## Source references

- [Lifecycle manager and tool schema](../src/computer.rs)
- [Local CLI parser](../src/client/computer_control.rs)
- [Generic exec API](code-mode.md#exec)
- [Zuse CLI documentation](https://github.com/LIghtJUNction/zuse#readme)

Older [CUA capability validation](validation-cua-client-capabilities.md) describes the former embedded implementation and its dated environment.
