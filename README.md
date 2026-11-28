# SSMCP

**Connect AI agents to your development machines through an authenticated MCP endpoint.**

[![CI](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/ssmcp.svg)](https://crates.io/crates/ssmcp) [![MIT License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

English · [简体中文](README.zh-CN.md)

[Quick start](#quick-start) · [Demo](#demo) · [Documentation](#documentation) · [Contributing](#contributing)

SSMCP is a Rust client and relay for the Model Context Protocol (MCP). Start a session in a project directory, authorize it, and connect an MCP client to the relay. The agent can discover the devices it can access and use the tools exposed by the selected session. Each device connects out to the relay, so it needs no public IP address.

- **Use your own environment.** Run commands and interact with a terminal where your source, tools, and dependencies already live.
- **Keep working during long jobs.** Start commands independently, read results by job ID, and use event-driven waits for tool output and human input.
- **Coordinate agents.** Assign work through a worker pool, exchange directed messages, and follow progress from the same session.
- **Choose your interface.** Use the terminal UI, the browser workbench, or an available desktop or mobile client. You can also run the relay yourself.

## Demo

[![Watch the SSMCP demo showing three project sessions](https://i.ytimg.com/vi/9LqpS5BXRTQ/hqdefault.jpg)](https://youtu.be/9LqpS5BXRTQ)

After installing, explore the terminal UI with sample data, without signing in or connecting a device.

```sh
ssmcp ui preview
```

## Quick start

### Install the client

Download the archive for your platform from [GitHub Releases](https://github.com/LIghtJUNction/ssmcp/releases/latest), where the installers and `SHA256SUMS` are also available. With a compatible Rust toolchain, you can install the CLI from crates.io.

```sh
cargo install ssmcp --locked --bin ssmcp
```

Linux and macOS are the primary targets. Check the [platform and installation notes](docs/releases.md) for Windows, Android, and desktop builds.

On Omarchy, the [SSMCP status bar plugin](packaging/omarchy/README.md) embeds a collapsible session terminal and displays its tool calls over the last 10 minutes.

### Connect a project

Run this from the project directory. Replace `alice` with your relay username. Use `--webui` to open the browser workbench instead of the terminal UI.

```sh
ssmcp alice@ssmcp.lmm.best --name dev
```

On first use, follow the device sign-in instructions and confirm which account may control the session. For a self-hosted relay, replace `ssmcp.lmm.best` with your server address.

### Connect your MCP client

Add the HTTPS MCP URL printed by SSMCP to an OAuth-capable MCP client and complete its authorization. Call `list` to find the live session. From `code_mode`, use that session's current client ID to run a command such as:

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await exec.run(clientId, {
  command: ["git", "status", "--short"],
  wait_ms: 5000
});
```

Commands run with the SSMCP client's OS permissions. Grant control only to accounts and applications you trust. See the [English setup guide](https://ssmcp.lmm.best/guide/en) for device authorization and MCP setup.

## Documentation

| Topic | Guide |
| --- | --- |
| Install and connect | [Setup in English](https://ssmcp.lmm.best/guide/en) · [Platforms and releases](docs/releases.md) |
| Work locally or self-host | [Local mode](docs/local-mode.md) · [Deploy a relay](docs/deployment.md) |
| Call tools and manage jobs | [Code Mode](docs/code-mode.md) · [Command execution](docs/commands.md) |
| Coordinate agents | [Collaboration](docs/agent-collaboration.md) · [Worker pools](docs/workers.md) |
| Use or extend a client | [Client overview](docs/clients.md) · [Client extensions](docs/client-extension.md) |
| Read the full Chinese guide | [中文使用指南](docs/guide.zh-CN.md) |

## Contributing

Build from a recursive checkout using the Rust toolchain pinned by the repository.

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/ssmcp.git
cd ssmcp
cargo build --locked --bins
make check
```

When reporting a problem, include the SSMCP version, platform, and steps to reproduce it. [GitHub Issues](https://github.com/LIghtJUNction/ssmcp/issues)

## License

[MIT](LICENSE)
