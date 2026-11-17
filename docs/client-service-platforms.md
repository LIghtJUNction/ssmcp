# Client service installation

The operating system keeps one ssmcp session manager running. That manager owns the
named sessions in one account's state directory. Installing the manager starts it
immediately and registers it for the platform's normal startup event.

## Choose the startup scope

| Platform | Default `ssmcp service install` | `ssmcp service install --system --user ACCOUNT` |
| --- | --- | --- |
| Linux | `ssmcp.service` in the current user's systemd manager | `ssmcp.service` in the system manager, with an explicit non-root `User=` |
| macOS | `best.lmm.ssmcp` LaunchAgent in the current GUI login | `best.lmm.ssmcp` LaunchDaemon at boot, with an explicit `UserName` |
| Windows | Current-user scheduled task triggered at login | Automatic native Windows SCM service named `ssmcp`, under the explicit account |

The two scopes use different operating-system managers. Select one manager for a
given session directory. A system installation changes who starts the manager;
the configured account still owns its sessions. Linux and macOS reject root
workers. Windows rejects the LocalSystem SID, including account aliases.

Preview the exact platform definition without installing it:

```sh
ssmcp service install --dry-run
ssmcp service install --system --user alice --dry-run
```

Dry-run output is JSON and includes the owner, definition, destination and normal
startup commands. On Windows, a system preview resolves the account but does not read a
password. Use an existing account name.

## Linux

A user installation requires no elevation:

```sh
ssmcp service install
ssmcp service status
systemctl --user status ssmcp
journalctl --user -u ssmcp
```

A system installation requires an elevated invocation and a non-root owner:

```sh
sudo "$(command -v ssmcp)" service install --system --user "$USER"
sudo systemctl enable --now ssmcp
sudo systemctl status ssmcp
sudo journalctl -u ssmcp
```

The second command is the normal systemd enable-and-start interface; installation
has already performed it. `sudo ssmcp service install --system` can also derive the
owner from a non-root `SUDO_USER`. A direct root login must provide `--user`.
ssmcp never runs sudo internally.

User definitions are stored in
`${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ssmcp.service`; system definitions
are stored in `/etc/systemd/system/ssmcp.service`. The unit uses `Type=exec`,
`Restart=on-failure` and a non-root owner in system scope. An explicit stop stays
stopped; an unexpected manager failure is eligible for restart. Reinstalling an
owned running unit restarts it to apply the changed definition. A foreign unit
with the same name is rejected, including units in other systemd search paths.

A user manager normally follows the user's login lifecycle. To start that user
manager at boot and retain it after logout, enable systemd lingering explicitly:

```sh
sudo loginctl enable-linger "$USER"
```

Systemd documents the distinction between the system manager and `--user`, the
meaning of `enable --now`, restart behavior, and lingering in its
[systemctl](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html),
[service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)
and [loginctl](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html)
manuals.

## macOS

A LaunchAgent runs while its owner has a GUI login:

```sh
ssmcp service install
ssmcp service status
launchctl print "gui/$(id -u)/best.lmm.ssmcp"
```

For boot-time operation, install a LaunchDaemon with an explicit user:

```sh
sudo "$(command -v ssmcp)" service install --system --user "$USER"
sudo launchctl print system/best.lmm.ssmcp
```

The agent definition is
`~/Library/LaunchAgents/best.lmm.ssmcp.plist`; the daemon definition is
`/Library/LaunchDaemons/best.lmm.ssmcp.plist`. Installation enables and bootstraps
the job. The persistent plist specifies the executable and arguments separately,
sets `RunAtLoad`, and requests relaunch after an unsuccessful exit.

A LaunchDaemon does not provide the user's GUI session. Use the LaunchAgent when
a worker needs that desktop. Apple's
[launchd documentation](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
describes the agent/daemon lifecycle and the plist fields.

## Windows

The default installation registers a task for the current SID and starts it:

```powershell
ssmcp service install
ssmcp service status --json
```

The task is named `ssmcp-client-SID`, uses the current user's interactive token,
and has a login trigger. It runs a hidden PowerShell worker that sets the session
directory and invokes `ssmcp service run`. A logged-out user has no interactive
token for that task.

For boot-time service operation, open an elevated PowerShell and choose a
dedicated or named account. A named account requires its password through stdin,
and Windows must allow that account to log on as a service:

```powershell
$account = "$env:COMPUTERNAME\$env:USERNAME"
$credential = Get-Credential -UserName $account
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$credential.GetNetworkCredential().Password |
    ssmcp service install --system --user $credential.UserName --password-stdin
Remove-Variable credential
Get-Service ssmcp
```

ssmcp passes the password directly to SCM; it does not put the password in command
arguments, logs or its own configuration files. Windows manages the service
account credentials. Explicit built-in low-privilege accounts, virtual accounts
and managed service accounts can use their appropriate passwordless Windows
logon configuration.

This is a native SCM service. Its process connects through
`StartServiceCtrlDispatcherW`, registers a control handler and reports startup,
running and shutdown states. It reports running only after the local supervisor
answers a health request. STOP and SHUTDOWN notify the session manager to stop
its children gracefully. The service is automatic and configures restart after
failure. The native controls work after installation:

```powershell
Start-Service ssmcp
Stop-Service ssmcp
Restart-Service ssmcp
sc.exe query ssmcp
```

The system service uses the configured account's service profile. It runs in
Session 0 and does not have an interactive desktop. Use the default user task for
desktop-dependent work. The system service rejects `--state-dir`; that option is
available on the user task.

Microsoft documents
[SCM dispatch](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-startservicectrldispatcherw),
[service control handlers](https://learn.microsoft.com/en-us/windows/win32/services/service-control-handler-function),
[service creation](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-createservicew),
[login triggers](https://learn.microsoft.com/en-us/windows/win32/taskschd/logontrigger)
and [Session 0 behavior](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services).

## State, identity and control

The installer embeds an absolute executable path. Move or replace that binary
deliberately; a renamed checkout does not update a registered service. Installing
an existing Windows system service is rejected so that an account or executable
change cannot silently replace it; uninstall it explicitly before changing that
registration.

User installations honor `SSMCP_SESSION_HOME`, or the platform's normal per-user
state location. Linux also honors `XDG_STATE_HOME`. For a custom session location,
use `--state-dir /absolute/path` during installation and point interactive ssmcp
commands at the same location through `SSMCP_SESSION_HOME`.

System installations on Linux and macOS resolve the owner through the operating
system account database, set that owner's HOME, and use that owner's default
session directory unless `--state-dir` is given. They construct PATH from the
owner's mise shims, Cargo and local bin directories plus standard system paths.
They do not copy root's HOME or PATH into the worker. macOS also includes the
standard Apple Silicon Homebrew paths.

Use the same scope flag for subsequent management:

```sh
ssmcp service start
ssmcp service stop
ssmcp service restart
ssmcp service status --json
ssmcp service uninstall
```

Append `--system` for a system registration and use the platform's elevated shell
for changes. Status is read-only. Uninstall removes the owned registration after
stopping it; it does not delete saved session data. Existing foreign service
definitions and tasks are not overwritten or removed.

## Acceptance checks

These checks exercise observable behavior and require the relevant operating
system. Cross-compilation establishes type correctness; it does not establish
that a service manager has accepted a definition.

| Check | Expected result |
| --- | --- |
| Install with `--dry-run` | Exact definition is shown; no registration or process is created |
| Install as an ordinary user | User registration runs under that user's identity |
| Install Linux/macOS system scope without a non-root owner | Clear error before writing a definition |
| Install Windows SCM using any LocalSystem alias | SID policy rejects it before creating a service |
| Install with spaces, quotes, Unicode, percent or dollar characters in paths | Native definition preserves executable and environment values |
| Try to replace a foreign definition | Existing content remains intact |
| Run the normal platform start/status/stop commands | Manager reports the resulting native state |
| Stop SCM service with running sessions | STOP reaches the supervisor and its owned children are reaped |
| Kill a running manager unexpectedly | The configured native failure policy restarts it |
| Reboot or log out/in as appropriate for the scope | The registered startup trigger starts the same account's manager |
| Uninstall | Owned registration is gone; saved session data remains |

The Rust test suite covers option validation, account-derived paths, literal
systemd/plist argument handling, user task generation, safe definition replacement
and Windows account/argument policy. Runtime acceptance is recorded separately
when performed on each operating system.
