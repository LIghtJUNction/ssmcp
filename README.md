# ssmcp

**连接你的设备，让终端和 MCP 客户端调用本机工具。**

[![ssmcp 原创短片《日蚀》封面，日冕照亮立体城市天际线](docs/assets/ssmcp-eclipse.jpg)](docs/assets/ssmcp-eclipse.mp4)

[播放《日蚀》 · 24 秒](docs/assets/ssmcp-eclipse.mp4)

[![CI](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ssmcp.svg)](https://crates.io/crates/ssmcp)
[![License: MIT](https://img.shields.io/badge/License-MIT-64748b)](LICENSE)

ssmcp 将本机 Shell、命令任务、文件和已安装工具接入经过授权的 MCP 会话。设备通过 HTTPS/WSS 连接服务器，操作在设备上执行。`list` 发现设备，`code_mode` 编排原生调用，`collab` 管理同一会话中的 agent 成员与消息。终端界面展示执行记录、源码审计和工作区状态，也支持通过原子接口构建自己的 UI。

[快速开始](#快速开始) · [服务器与信任](#中转服务器与信任) · [定制客户端](#定制客户端) · [原子接口](#原子接口) · [文档](#文档)

## 一个命令连接

运行 `ssmcp alice@relay.example.com`，首次连接会检查你选择的用户名，引导 GitHub 授权，然后建立 HTTPS/WSS 连接。后续复用这台服务器的本机凭据；`ssmcp login` 保留为兼容入口。

本项目由 Astra Pro 在网页版 Chat 模式下使用 ssmcp 插件开发。

## 快速开始

### 1. 安装客户端

Linux / macOS：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh
```

Windows PowerShell：

```powershell
irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex
```

已安装 Rust 时也可运行 `cargo install ssmcp --locked --bin ssmcp`。Release 提供 Linux、macOS、Windows 的 x86_64 与 ARM64 客户端；安装脚本核验下载文件和版本后替换程序，重复运行即可升级。[平台要求与安装选项](docs/releases.md)

### 2. 登录并连接设备

使用已启用 GitHub OAuth、账号门户和客户端 API 的 ssmcp 服务器，将示例地址换成实际服务地址：

```sh
# 在要操作的项目目录运行，首次连接自动引导授权。
ssmcp alice@ssmcp.example.com --name dev --title "开发与测试"

# 无图形浏览器时，打印链接和确认码供另一台设备打开。
ssmcp alice@ssmcp.example.com --no-browser
```

`alice` 是你在这台服务器选择的用户名，可以与 GitHub 用户名不同。用户名统一为小写，长度为 3–32 个 ASCII 字符；首字符为字母，末字符为字母或数字，中间可用字母、数字、`_`、`-`。每个 GitHub 账号在一台服务器绑定一个用户名，各服务器的凭据分别保存。[用户名与管理员配置](docs/named-accounts.md)

保持终端打开即可提供当前目录的本机工具。之后可直接运行 `ssmcp --name dev` 复用默认服务器，`ssmcp whoami` 查看身份。连接采用出站 HTTPS/WSS，无需设备入站端口；断线后默认尝试重连，`--no-watchdog` 可以关闭自动重连。

在另一个终端查看会话，或进入设备的交互终端：

```sh
ssmcp list
ssmcp -i
```

Ctrl-] 离开远端控制界面，会话继续运行；其中的 Ctrl-C 发送给远端 Shell。[账号、临时接入与会话管理](docs/client-accounts.md)

### 3. 接入 MCP 客户端

将终端显示的 `https://你的服务器/mcp` 加入 ChatGPT 或其他支持 OAuth 的 MCP 客户端，用同一 GitHub 账号确认授权。调用 `list` 选择设备，再通过 `code_mode` 调用该设备的能力；需要多个 agent 交换任务与结果时，使用 `collab`。CLI 登录和 MCP 应用授权分别管理。

已有 SSH 连接通过 `ssmcp --ssh my-server` 显式使用；`-F`、`--identity` 或 `-p` 也会选择 SSH 路径，见[SSH 接入与身份授权](docs/configuration.md)。自建服务器的部署步骤见[部署指南](docs/deployment.md)。

## 中转服务器与信任

### 保存默认服务器

```sh
ssmcp config set server https://relay.example.com
ssmcp config get server
ssmcp config path
```

地址按 `--server` → 配置文件的 `[client].server` → 最后登录的服务器 → 内置地址 `https://ssmcp.lmm.best` 选择。`config set` 保留已有 UI 设置与注释；`config unset server` 移除文件中的覆盖。首次切换服务器可运行 `ssmcp alice@relay.example.com` 自动注册或登录，各服务器的账号凭据分别保存。

也可编辑 `ssmcp config path` 指定的文件，默认是 `~/.config/ssmcp/config.toml`：

```toml
[client]
server = "https://relay.example.com"
```

`SSMCP_CLIENT_CONFIG` 可指定另一份绝对路径配置文件；`--ui-config` 和 `SSMCP_UI_CONFIG` 只选择显示配置。[路径、优先级与命令示例](docs/client-server.md#设置默认服务器)

### 固定可信公钥

取得并核验管理员提供的 HTTPS 入口公钥后，可登记到本机：

```sh
ssmcp trust add --server https://relay.example.com --public-key ./server-public.pem
ssmcp trust path
ssmcp trust list
```

记录保存在客户端配置同目录的 `trusted-servers.toml`，默认是 `~/.config/ssmcp/trusted-servers.toml`。这里登记 TLS 证书的 SPKI 公钥；使用 Nginx、Caddy 或 CDN 时，应登记实际 HTTPS 入口的公钥。HTTPS API 与 WSS 连接会同时检查正常证书条件和已登记公钥，不匹配时停止连接。证书沿用原公钥续签时无需更新，换钥时先核验并登记新公钥。[文件格式、指纹与轮换](docs/client-server.md#信任公钥写到哪里)

## 定制客户端

客户端默认使用 `dark` 主题和 `chat` 布局：连续事件流展示调用、输出和结果，底部输入框用于追加用户要求。运行期间可以继续输入，待交付要求保留在队列中，支持取回编辑；下一次符合目标的 `code_mode` 响应携带这些要求时，界面标记为已交付。已交付表示附加到了响应，不代表模型已经阅读或执行。

`/help` 查看操作，`/audit` 浏览原始编排源码，`/agents` 选择真实登记成员，`/queue` 管理待交付要求。Tab 切换输入和浏览焦点，F2 切换事件流、审计、成员和队列，F3 收起侧栏。`/reload` 重新启动客户端，F5 只重载显示配置。终端输入继续通过 `terminal.*` 或 `ssmcp -i` 使用。

![ssmcp 客户端：执行记录、输入框和状态面板](docs/assets/client-ui.png)

*图片由客户端实际渲染器回放固定示例生成；其中的成员、命令、输出与耗时用于展示布局。*

先在本机预览，不需要连接服务器：

```sh
ssmcp ui preview
ssmcp ui init
ssmcp ui show
ssmcp ui check
```

预览中按 Ctrl-] 退出，输入的要求不会执行。`ui init` 创建默认配置，已有文件会保留。编辑 `~/.config/ssmcp/config.toml` 后，按 F5 应用主题与布局；也可用 `--ui-config FILE` 为不同项目选择配置。

```toml
[ui]
theme = "minimal"
sidebar = true
sidebar_width = 30
panels = ["agents", "queue", "tasks", "git", "history"]
hide_empty = true

[ui.colors]
background = "#101214"
foreground = "#e8eaed"
muted = "#9aa0a6"
accent = "#8ab4f8"
success = "#81c995"
warning = "#fdd663"
error = "#f28b82"
heading = "#e8eaed"
```

内置 `dark`、`codex-dark`、`nord`、`solarized`、`light`、`minimal` 六种主题。默认 `dark` 绘制完整深色背景；`minimal` 跟随终端颜色。颜色可逐项覆盖，源码审计与日志可以单独浏览，状态面板可以排序或隐藏。[完整配置、快捷键与队列接口](docs/client-ui.md)

要求队列和成员目标需要客户端与服务器支持协议 v7，源码审计需要 v6 或更新版本。兼容旧连接时界面会说明可用范围。源码来自调用方提交的 `code_mode.code`，脱敏后只发送给本次调用实际操作的客户端。重载使用私有、单次读取的草稿与队列交接；交付不确定的记录保留待复核，不会自动重发。[队列与恢复](docs/client-ui.md#输入要求与队列) · [源码范围](docs/client-ui.md#源码审计与运行状态)

## 原子接口

ssmcp 的显示和执行能力各自提供小接口。自定义 TUI、桌面面板或其他 MCP 客户端可以读取状态、选择终端、控制任务和调整主题，不必解析屏幕文字。`ssmcp.ui.get` 支持按 JSON Pointer 读取字段和分页，界面轮询可以只取需要的状态。

| 能力 | 接口 |
| --- | --- |
| 界面配置、状态与筛选 | `ssmcp.ui.get`、`ssmcp.ui.set`、`ssmcp.ui.action`、`ssmcp.ui.filter` |
| 用户要求队列 | `ssmcp.ui.queue`：读取、提交、取回、编辑、取消与重新选择目标 |
| 源码与调用审计 | `ssmcp.audit.events`，按游标读取 |
| 多个交互终端 | `terminal.open/list/select/close/read/write/status/resize` |
| 独立命令任务 | `exec.start/run/list/status/read/write/wait/cancel/forget` |
| RTK 输出压缩 | `rtk.status/start/run`，调用设备上已安装的 RTK |
| 执行事件 | `exec.events`，按游标增量读取 |
| 工作区文件 | `fs.list/stat/read/write/mkdir`、`fs.readText/writeText/editText` |
| 技能与项目指令 | `skills.list/read/view`、`agents.list/read/resolve` |
| 当前项目上下文 | MCP `resources/read`，读取 AGENTS 内容、技能摘要与全文 |
| 浏览器与桌面 | `browser.*`、`computer.*`，按实际能力状态调用 |
| 输出归档 | `outputs.read/stat/cleanup` |
| 会话内 agent 协作 | `collab.join/list/update/send/receive/ack/leave` |

对外 MCP 提供 `list`、`code_mode` 和 `collab` 三个工具，分别用于设备发现、JavaScript 编排和成员通信。调用设备能力前先查询原生工具目录，再使用返回的参数 schema：

```javascript
const matches = clients.filter(c => c.name === "dev");
if (matches.length !== 1) throw new Error("Select a unique client ID from list");
const client = matches[0];

return await tools.search(client.id, {
  query: "ssmcp.ui",
  include_schemas: true
});
```

在下一次 `code_mode` 中，通过选中的 ID 调整界面：

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");

return await call(client.id, "ssmcp.ui.set", {
  patch: { layout: "audit", sidebar: false },
  persist: true
});
```

使用同一套连接运行独立命令：

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");

return await exec.run(client.id, {
  command: ["git", "status", "--short"],
  wait_ms: 5000
});
```

命令使用 argv 数组，结果带任务 ID、退出码、stdout/stderr 和输出游标。日志使用 `python3 -c · 83 lines` 这类脚本标签，并分别显示实际输出和结束状态；源码可在审计区展开。运行中的任务用同一 ID 继续 `wait` 或 `read`。`exec.parallel` 按实际运行的命令限制并发，单次等待到期不会释放占用名额。[命令接口](docs/commands.md) · [多终端与后台输入](docs/terminal-control.md) · [自定义 UI](docs/client-ui.md#构建自己的界面)

## 多个 agent 协作

多个 agent 可以加入同一在线客户端连接，分别取得 `agent_id` 与私密 `token`。`collab.list` 返回当前有效成员，`send` 支持定向 JSON 消息和广播；`receive` 可重读未确认消息，处理后用 `ack` 释放邮箱容量。

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await collab.join(clientId, {
  name: "implementation",
  role: "developer",
  capabilities: ["rust", "tests"]
});
```

保存返回的成员凭证，后续请求继续使用；JavaScript 变量不会跨 `code_mode` 请求保留。成员租约为 15 分钟，成功的成员操作续租，单纯查询名单不续租。状态由成员自行报告，发送成功表示入队；协作接口不启动模型，也不会唤醒停止的 agent。

接收定向的本地用户要求时，agent 在每次 `code_mode` 开始调用 `collab.bind(clientId, {agent_id, token})`，绑定已经登记的身份。服务器按经过校验的成员 ID 交付；显示名称和 `controller.model` 不决定接收者。

持续等消息时，直接调用公开 `collab` 工具的 `receive`，最长等待 30 秒；`code_mode` 中的同名 helper 最长等待 1 秒。客户端断开或服务器重启会结束对应的内存协作状态。[完整接口、私信与恢复语义](docs/agent-collaboration.md)

## 设备与工具

**临时接入另一台设备。** 在已登录的设备执行 `ssmcp invite`，把生成的命令交给另一台设备运行；Windows 使用 `ssmcp invite --shell powershell`。邀请默认 10 分钟内兑换一次，会话默认持续 1 小时。未固定公钥时，从所选服务器下载临时客户端；已固定公钥时，使用预安装客户端流程，在接入前登记可信指纹。[临时接入](docs/client-accounts.md) · [公钥与邀请](docs/client-server.md#公钥固定后的临时邀请)

**后台会话。** `ssmcp --daemon --name dev` 创建本地后台会话；使用 `ssmcp sessions list/status/logs/stop` 管理，`ssmcp attach dev` 打开观察界面。需要开机或登录后常驻时，安装平台对应的服务。[常驻服务](docs/session-service.md) · [平台说明](docs/client-service-platforms.md)

**本机工具发现。** Shell、文件、命令任务和上下文接口内置于客户端。Codex、Claude Code、Pi、OpenCode 及 Playwright MCP 按本机安装与配置发现，工具保留原生参数和审批行为。`ssmcp --list-tools` 查看实际目录；`ssmcp.harnesses.list` 查看提供者状态。[后端指南](docs/backends.md)

**项目上下文。** MCP 资源直接提供当前 AGENTS 内容、技能摘要和技能全文；修改文件后重新读取即可获取最新内容。项目技能覆盖同名全局技能，额外只读根通过 `--context-root` 显式加入。[上下文资源与原语](docs/context.md)

**命令输出压缩。** 设备已安装 RTK 时，可使用 `rtk.run(clientId, {command:["git","status"]})`。参数按数组原样传入，返回退出码和 RTK 的原文恢复提示，后台任务继续通过 `exec.read/wait/cancel` 管理。[RTK 用法与限制](docs/rtk.md)

**浏览器与桌面。** 浏览器接口提供标签页、页面快照、点击、填表与截图；桌面接口提供窗口和输入操作。实际可用能力由 `browser.status`、`computer.status` 返回。已有 Chrome 扩展与 CDP 会话的接入方式见[现有浏览器指南](docs/browser-existing-session.md)。

## 执行与授权

每个调用按服务器分配的客户端 ID 路由，并检查当前授权。命令和原生工具使用设备上的系统用户权限；工作区文件接口以客户端启动目录为根。`--context-root` 只扩展技能与指令文档的只读范围。中转服务器负责处理连接与工具调用，应选择自己信任的服务器。

终端主题和布局保存在本机。JSONL、后台会话或重定向日志适合无界面的消费者；事件展示与审计使用脱敏副本，原生工具结果保持原有内容。完整输出通过任务游标或输出归档读取。[日志与输出](docs/logging.md)

任务归属于创建它的客户端连接。断线清理或重载会结束这条连接拥有的后台任务；重连后重新 `list` 获取客户端 ID。长时间执行的操作应保存任务 ID，并检查状态后再决定是否重试。[生命周期与恢复](docs/session-control.md)

## 开发

仓库固定 Rust 工具链见 [rust-toolchain.toml](rust-toolchain.toml)。在项目目录运行：

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

协议与 Harness 测试使用确定性 fixture。[客户端交互验证](docs/reports/client-experience-20261118/README.md)提供真实 PTY、队列、重载和截图的复现步骤；[历史权限验收](docs/validation.md)保留早期 SSH 与 OAuth 记录；发布与平台构建见[发布流程](docs/releases.md)。提交改动时，请附上对应验证结果；界面修改同时附上渲染截图。

## 文档

| 主题 | 文档 |
| --- | --- |
| 输入与队列、主题、源码审计和自定义界面 | [客户端 UI](docs/client-ui.md) |
| 登录、设备列表和临时邀请 | [客户端账号](docs/client-accounts.md) |
| 用户名绑定与服务器管理 | [具名账号](docs/named-accounts.md) |
| 默认中转服务器与可信公钥 | [服务器与信任](docs/client-server.md) |
| 成员发现、私信、广播与消息确认 | [Agent 协作](docs/agent-collaboration.md) |
| 执行任务、事件与长输出 | [命令接口](docs/commands.md) · [日志](docs/logging.md) |
| RTK 命令与输出压缩 | [RTK](docs/rtk.md) |
| 多终端、后台输入和控制者显示 | [终端控制](docs/terminal-control.md) |
| 自动发现、本机工具和 JavaScript 编排 | [后端与代码模式](docs/backends.md) |
| 浏览器、桌面和 Omarchy | [电脑控制](docs/computer-use.md) · [现有 Chrome](docs/browser-existing-session.md) |
| 后台会话、常驻服务和重载 | [会话管理](docs/session-control.md) · [常驻服务](docs/session-service.md) |
| 技能、项目指令与文件操作 | [上下文接口](docs/context.md) · [工作区](docs/workspaces.md) |
| 服务器、账号门户和 SSH 配置 | [部署](docs/deployment.md) · [门户](docs/portal-console.md) · [配置](docs/configuration.md) |
| 安装升级、平台要求和发布 | [Release 指南](docs/releases.md) |
| 给 AI 客户端的使用说明 | [ssmcp skill](skills/ssmcp/SKILL.md) |

[MIT License](LICENSE) · [MCP](https://modelcontextprotocol.io/)
