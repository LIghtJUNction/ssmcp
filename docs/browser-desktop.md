# Background browser control and private desktops

Browser automation defaults to page-scoped input and background tabs. Native
desktop input uses an explicitly selected surface: an owned private desktop, or
the operator's desktop when the caller passes `desktop: "host"`.

## Configure an existing browser

```sh
ssmcp setup browser --check --json
ssmcp setup browser
ssmcp setup browser --install
ssmcp setup browser --playwright-mcp /absolute/path/to/cli.js
```

The first command is read-only. The default setup discovers installed browsers,
extension manifests and MCP entry points, validates a usable Playwright CLI, and
saves its absolute path. Installation places a fixed, verified
`@playwright/mcp@0.0.85` under the ssmcp user configuration directory. It does
not install another browser, restart an existing browser, or copy profile
credentials. Repeating setup preserves unrelated TOML settings.

```toml
[browser]
playwright_mcp = "/absolute/path/to/cli.js"
background = true
```

`SSMCP_PLAYWRIGHT_MCP` takes precedence over the saved executable. An invalid
explicit override produces a diagnostic. The native provider checks the actual
CLI's `--help` output for background support before starting it; unsupported
versions do not silently switch to foreground control. `--foreground` is an
explicit setup choice for operators who want extension-controlled windows to
activate.

The Playwright MCP Bridge extension must be enabled and its native connection
request approved. A successful CLI probe does not mean a browser is already
paired. Existing ChatGPT and Claude extension manifests are reported as
inventory. ssmcp does not read extension token storage or claim a private
ChatGPT extension API.

Playwright background control requires extension 0.5.0 or newer. Its first
connection approval page can still appear in the foreground. The upstream
background pairing path additionally requires an operator-provided token,
an already-running browser, and Chrome 158.0.8127.2 or newer. Native approval
remains the authority for tabs that the extension exposes.

## Browser operations

For an existing browser, start with `ssmcp.browser.discover` and the actual
`playwright.*` tool catalog. This reuses authorized tabs and the provider's
locator, iframe and waiting support. The direct CDP adapter remains available
for observed loopback debugging endpoints and isolated test browsers.
Before the CDP example below, connect to an observed loopback endpoint with
`browser.connect`, or create an isolated test browser with `browser.launch`.
Browser setup configures the extension provider; it does not create a CDP session.

```javascript
const api = native(clientId);
const opened = await api.ssmcp.browser.open({url: "http://127.0.0.1:8000"});
// open defaults to background:true. Pass false only for intentional activation.
const target_id = opened.structuredContent.target_id;
const snapshot = await api.ssmcp.browser.snapshot({target_id});
const ready = await api.ssmcp.browser.wait_for({
  target_id, text: "Ready", state: "present", timeout_ms: 5000
});
```

Direct CDP input targets the browser page rather than moving the system cursor.
For explicit click, fill, key and scroll operations, the adapter enables CDP
focus emulation on that target so background pages receive trusted input and
can render. This does not activate the browser tab or window. Emulation lasts
for the target connection and is reset on disconnect, close or shutdown;
read-only connections do not enable it. Since emulation can change the page's
reported visibility, tests also check the actual browser window title and an
independent window's keyboard focus and pointer position.
`wait_for` waits for text to appear or disappear on the client, avoiding
repeated remote snapshot calls. It has a bounded deadline and does not invent
an element condition. Snapshots include implicit control roles and accessible
names from `aria-labelledby`, `aria-label`, labels and image alternatives.
The direct snapshot's main-document limitation remains; use the native
Playwright provider for workflows that need its broader locator support.

The `[browser].background` setting controls Playwright extension behavior.
Direct CDP `browser.open` has its own per-call `background` argument,
defaulting to true.

`browser.launch` now defaults to a headless owned browser. Explicit
`headless: false` is available for an intentional headed session, including an
owned private desktop. Owned Chromium launches enable `CDPScreenshotNewSurface`,
the same capture path used by Playwright, so background capture does not wait
for the target tab to be presented in the operator's window.

Already-running external browsers retain their original launch configuration.
In the Linux/Xvfb acceptance environment, default-launched Chromium
157.0.8103.38 and Chrome 157.0.8103.78 accepted trusted background input but their
background surface screenshots timed out. Connecting Playwright to the same
external process did not change that launch-time setting. ssmcp reports the
capture failure and does not activate a tab to conceal it. Use an owned browser
or private desktop when that external configuration cannot provide an image.

## Native desktop input without using the host pointer

Linux clients can allocate one private Xvfb desktop on demand. Its X server
uses a private authorization cookie, a private runtime directory, and no TCP
listener. Input helpers and their cancellation cleanup inherit that display
explicitly; process-global display variables are never changed.

```javascript
const api = native(clientId);
await api.ssmcp.computer.desktop_start({width: 1280, height: 800});
await api.ssmcp.computer.app_launch({command: ["xmessage", "Private desktop"]});
const windows = await api.ssmcp.computer.windows({});
image(await api.ssmcp.computer.screenshot({}));
// Use observed window IDs and screenshot coordinates for subsequent input.
await api.ssmcp.computer.desktop_stop({});
```

Once started, ordinary `computer.*` operations use the private display by
default. `desktop: "isolated"` makes this choice explicit. If the display
exits or is stopped, input fails with a clear diagnostic instead of falling
back to the host. A private session is never allocated by capability listing,
setup checks or idle client startup.

Host input is still supported when intended:

```javascript
await api.ssmcp.computer.click({desktop: "host", x: 200, y: 100});
```

A read such as `computer.status` or `computer.screenshot` can inspect the
host when no private display exists. Results identify their surface and
coordinate space. Existing desktop workflows should explicitly select host
input when that is their intention.

`app_launch` uses argv, a fresh D-Bus session, private XDG directories and
removes known host application IPC environment variables. Chromium, Firefox and
VS Code family launchers receive private profiles. Other single-instance apps
must support the supplied display/session environment or provide their own
instance option. Applications already open on the host are not moved into the
private desktop.

The private desktop requires `Xvfb`, `xdotool`, `magick`, `xprop` and
`dbus-run-session` plus `dbus-daemon`. Setup reports each dependency separately. macOS and
Windows keep their native host adapters and browser background controls; the
Xvfb private desktop is Linux-specific.

Stopping or disconnecting terminates only the owned application trees and
private X server. The cancellation reaper retains authorization and application
state until those processes exit, then removes the temporary directory.
Owned X11 socket cleanup checks the original filesystem identity before
removal, so it does not remove a replacement server's socket.

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
route. Authorization is checked again after remote I/O. Existing wire v4/v5
and native `call()` remain supported. A new client can expose these
capabilities through `call()` on an older relay; the new dynamic JavaScript
conveniences and generic relay resource URI entry points require the updated
relay.

## Sources and validation

- [Playwright extension background behavior](https://github.com/microsoft/playwright/blob/main/packages/extension/README.md#working-in-the-background)
- [Playwright MCP releases](https://github.com/microsoft/playwright-mcp/releases)
- [Playwright Chromium launch configuration](https://github.com/microsoft/playwright/blob/main/packages/playwright-core/src/server/chromium/chromiumSwitches.ts)
- [Chromium background screenshot feature](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/content/common/features.cc)
- [ChatGPT browser extension documentation](https://learn.chatgpt.com/docs/chrome-extension.md)
- [Validation and environment scope](validation-cua-client-capabilities.md)

This change does not assert complete ChatGPT CUA feature parity or an unmeasured
memory reduction. The acceptance tests cover the implemented input isolation,
focus preservation, setup semantics, resource freshness and compatibility paths.
