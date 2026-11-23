# Local tool repositories

SSMCP loads JavaScript helper factories from explicitly selected repositories. A
helper composes existing operations; the client and relay still authenticate and
validate each operation. Keeping these factories in small Git repositories lets
a project retain its own workflows and makes each loaded revision inspectable.

## Locations and precedence

| Layer | Location | Activation |
| --- | --- | --- |
| Project | `./.ssmcp/tools/` under the client's exact workspace root | That canonical workspace root is registered in the trusted RC |
| Global | `~/.ssmcp/tool/` (singular `tool`) | `tools.global = true` in the trusted RC |
| Official | `$XDG_DATA_HOME/ssmcp/tools/official`, default `~/.local/share/ssmcp/tools/official` | Selected client's official source |

Factories load in this order: **official → explicit RC overrides in their listed
order → enabled global → registered project**. Later same-name exports replace
earlier exports. Thus project helpers take precedence over global helpers, and
both take precedence over official helpers. A namespace is one export: use
`core.previous` to retain earlier namespace members when replacing it.

The project root is the client's workspace, or the command's current directory
for local CLI operations. It is canonicalized and matched exactly; the loader
does not search parent directories or automatically trust nested checkouts.
Creating a directory, cloning a repository, or adding a manifest alone does not
activate code. Local layers require real directories and reject symlink
redirection. [Selection implementation](../src/tool_repository.rs)

## Initialize, enable and disable

Run project commands from the workspace you intend to register:

~~~sh
ssmcp tools paths
ssmcp tools init project --git
ssmcp tools status
~~~

`paths` prints the RC, official, global and project locations as JSON without
reading the RC, creating directories or loading code. `init` creates a
`manifest.json`, an empty `tools.js` factory and a short README, then explicitly
registers the layer in the trusted RC. It is offline. The optional `--git`
creates an independent local Git repository on an unborn `main` branch; it
does not make a commit, add a remote or fetch anything. Omitting the scope selects
`project`. Use `ssmcp tools init global --git` for the global layer.

An existing destination is preserved and causes `init` to fail. After reviewing
an existing source, enable it explicitly; disable removes its registration and
preserves the source files and Git history:

~~~sh
ssmcp tools enable project
ssmcp tools disable project
ssmcp tools enable global
ssmcp tools disable global
~~~

Enabling reads the manifest, source bytes and Git identity; it does not evaluate
the JavaScript factory. It rejects invalid paths, empty or oversized source
files and duplicate selections. If the same local directory or entry file was
already in the legacy `overrides` list, enabling migrates that registration to
the named layer. Repeated enable/disable operations preserve an unchanged RC.
Disable can remove an enabled layer whose files have become missing or invalid.

The management commands take the existing tool-repository maintenance lock.
If the RC changes before a write, the command reports an error instead of
replacing the changed configuration. An initialization interrupted after the
destination was created preserves that directory for inspection; inspect both
the RC and directory before retrying. [Local management](../src/tool_repository/local.rs)

`status` reads the selected source bundle and reports its identity; it may
bootstrap the official checkout from the bundled offline seed. `check` fetches
the official upstream state, and `update` can fast-forward a clean official
checkout. Neither updates project or global repositories. Local workflow edits
do not require a fetch, client restart or service deployment.

## Trusted configuration

The RC is `$XDG_CONFIG_HOME/ssmcp/ssmcprc.toml`, default
`~/.config/ssmcp/ssmcprc.toml`. It is separate from the client's connection and
display configuration. The version remains 1:

~~~toml
version = 1

[tools]
overrides = []
global = true
projects = ["/absolute/canonical/project"]
~~~

Both new fields are optional: `global` defaults to `false`, and `projects`
defaults to an empty list. The existing `overrides` field keeps its ordering and
relative-path behavior: a relative override resolves against the selected
client workspace. It accepts at most 16 entries. `projects` accepts at most
128 distinct absolute canonical workspace roots; only the exact current root
selects a project layer. Other registered projects are not loaded for this call.

Use the CLI to register reviewed sources, or edit this user-owned configuration
deliberately. Workspace contents cannot register themselves. A reported source
path is not permission to write that path: native workspace, account and
filesystem boundaries still apply.

Each directory source has a manifest:

~~~json
{"schema":"ssmcp-tools-v1","entry":"tools.js"}
~~~

The entry must be a relative file path inside the registered directory, without
traversal or symlinks. A factory receives the host API and a snapshot of earlier
exports. For example, a project can add a local label and retain existing
execution helpers without performing an operation during loading:

~~~js
core => ({
  workflowLabel: () => "project checks",
  exec: Object.freeze({
    ...core.previous.exec,
    label: () => "project checks",
  }),
})
~~~

The protected globals `call`, `clients`, `image`, `print` and `console`
remain host-owned. Factories cannot start native operations while initializing.
Call a returned helper explicitly after loading to perform an authorized
operation. A malformed or missing selected source stops the call; the loader
does not fall back to a lower layer. Repair the source or explicitly disable it.

## Verify the actual loaded code

Select the intended authorized client for `code_mode`, make a new call after
editing, and inspect the outer `structuredContent.tool_sources` receipt. It
contains the client and selection mode, RC path/hash, and each source's name,
entry path, SHA-256, byte count and Git HEAD/dirty state. Global and project
sources have those layer names. The v1 source-bundle fields are unchanged.

The receipt identifies loaded layers, not the source of each exported function.
It is outside the editable program's return value. The
`ssmcp-client://tool-sources` resource includes source code and may bootstrap
the official checkout; it is not a metadata-only query.
[Client selection and source receipts](code-mode.md#find-and-iterate-tool-implementations)

| Bound | Limit |
| --- | --- |
| Loaded sources | 19: official + 16 explicit overrides + global + project |
| One source | 128 KiB |
| All source code | 192 KiB |
| Encoded source bundle | 512 KiB |
| Source identity metadata | 16 KiB |

The count is checked by both Rust and the JavaScript host. Older builds may
reject a bundle with more layers; update the matching client and relay before
depending on the new limit. These byte limits are unchanged. Invalid digests,
duplicate entry paths, invalid manifests and configuration changes during
selection cause explicit errors.
[Boundary tests](../src/tool_repository/tests.rs) ·
[Actual JavaScript host tests](../scripts/test_client_capabilities.mjs)

## Maintaining workflows with Dreaming

Dreaming can use the existing authenticated execution, filesystem and Git
operations to maintain a selected local tool repository. This is a workflow
over those operations; initializing a repository creates no scheduler, model
process or automatic task loop.

1. Inspect `ssmcp tools paths` and the current source receipt. Choose the
   project or global layer that the user authorized you to maintain, then
   initialize or enable that exact repository if needed.
2. Read its instructions, manifest, helper source and Git status. Add reusable
   helpers and ordinary workflow files. Keep initialization free of operations;
   helpers should preserve real job IDs, incomplete output and errors.
3. Run focused checks in an isolated fixture. Inspect any existing `unknown`
   execution through its recorded identity instead of replaying it. Review the
   source diff and commit reviewed text when the workflow calls for a commit.
   Keep private credentials, large outputs and generated binaries out of Git.
4. Make a new `code_mode` call for the same authorized client and compare the
   outer source hash and Git state with the intended revision. Invoke the helper
   explicitly and report the native outcome.

Additional workflow files are ordinary repository content. Only the manifest's
selected factory is loaded, and only returned helpers invoked by the caller
perform operations. The existing native schemas, authorization and approval
rules continue to govern those operations.
