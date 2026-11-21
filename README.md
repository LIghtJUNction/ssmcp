# ssmcp

### 把你的电脑接到 Agent。用一个 `code_mode` 调用原生工具。

ssmcp 是一个可自建的远程 harness 通道。客户端把命令、文件、会话和已安装的开发工具接入授权会话；Agent 通过 MCP 选择设备并调用这些工具。模型可以在云端，工作区、执行环境和工具状态留在你的机器上。

[演示视频](#演示视频) · [快速开始](#快速开始) · [本地与远程目标](docs/connection-contexts.md) · [会话操作](docs/client-input.md) · [工具与 Zuse CLI](docs/computer-use.md) · [编写自己的客户端](docs/client-extension.md) · [部署服务器](docs/deployment.md)

[![Rust](https://img.shields.io/badge/Rust-1.100%2B-64748b?style=flat-square)](Cargo.toml) [![License: MIT](https://img.shields.io/badge/License-MIT-64748b?style=flat-square)](LICENSE) [![Releases](https://img.shields.io/badge/Download-Releases-4285f4?style=flat-square)](https://github.com/LIghtJUNction/ssmcp/releases)

---

## 演示视频

- **中文**：[抖音 · ssmcp 项目介绍](https://v.douyin.com/_3yRZNA-Qls/) · [哔哩哔哩 · ssmcp 项目介绍](https://b23.tv/wA4gzmV)
- **English**：[YouTube · ssmcp Demo](https://youtu.be/9LqpS5BXRTQ?si=kn2Oy9kPiUUPoFxm)

## 一条连接，三种用法

| 你想做的事 | ssmcp 提供什么 |
| --- | --- |
| 让云端 Agent 操作开发机 | 原生 Shell、独立命令任务、文件、Zuse CLI 管理，以及按需加载的 Codex / Claude Code / Pi / OpenCode 工具 |
| 在终端里持续协作 | Markdown 回复、代码高亮、文件和技能引用、问答队列、历史恢复；运行期间仍可继续输入 |
| 做自己的远程 harness | 客户端拥有工具目录、schema、上下文与 UI；复用协议和客户端扩展 hook，替换自己需要的部分 |

它的基本过程很直接：Agent 选中一个在线客户端，提交带参数的工具调用，客户端在本机执行，再返回结果、图片或完整输出的归档位置。工具的名称和参数来自实时目录；安装了一个产品，不代表它的全部内部功能都可以直接调用。

## 快速开始

### 1. 安装

Linux / macOS：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh
```

Windows PowerShell：

```powershell
irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex
```

也可以使用 `cargo install ssmcp --locked --bin ssmcp`。安装脚本获取已发布版本；源码构建使用下文的完整子模块检出步骤。[平台与安装选项](docs/releases.md)

手动安装的客户端可以直接检查和更新：

~~~sh
ssmcp update --check   # 只检查最新稳定版
ssmcp update           # 校验下载后替换当前安装
~~~

更新使用官方发布的 `SHA256SUMS`，并检查候选程序的版本；下载或校验失败会保留原程序，源码版本较新时不会降级。Cargo、系统包管理器等管理的安装继续使用原来的更新命令。已有会话保持当前程序，重启后使用新版本。[自更新说明](docs/commands.md#更新客户端)

### 2. 在项目目录连接

```sh
ssmcp alice@relay.example.com --name dev --title "开发与测试"
```

把服务器地址换成自己的服务地址。首次连接引导 GitHub 授权，并在该服务器绑定你选择的用户名。后续直接运行 `ssmcp`，即可复用最近的授权账号；没有图形浏览器时使用 `--no-browser`。

客户端主动建立 HTTPS/WSS 连接，无需在设备上开放入站端口。保持终端或后台客户端运行，就能使用它的工作区和工具。

```sh
ssmcp list       # 查看已授权的在线进程与已登记线程
ssmcp -i         # 进入一台设备的交互终端
ssmcp whoami    # 查看当前账号
```

完整步骤见[账号与设备](docs/client-accounts.md)和[默认服务器与可信公钥](docs/client-server.md)。

### 3. 接入 MCP

将终端显示的 `https://你的服务器/mcp` 加入 ChatGPT 或其他支持 OAuth 的 MCP 客户端，再完成该应用的授权。CLI 登录与 MCP 应用授权分别管理。

对外入口有三个：

| 入口 | 作用 |
| --- | --- |
| `list` | 发现当前身份可访问的设备与客户端工具目录 |
| `code_mode` | 用 JavaScript 组合原生调用，安排顺序、并发和结果读取 |
| `collab` | 同一会话中的成员身份、定向消息、广播与确认 |

被控设备可以不登录，直接运行 `ssmcp --allow 用户名@中继服务器`；控制者必须登录该服务器的既有账号。显式名单仅授权列出的人，已登录客户端省略名单时仅允许自己。CLI `invite` / `join` 和 MCP `invite` 已移除，会话内 agent 的 `collab.join` 保留。[账号与授权](docs/client-accounts.md)

先从 `list` 选择确切的客户端 ID，再查询工具的原始 schema：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await tools.search(clientId, {
  query: "ssmcp.exec",
  include_schemas: true
});
```

运行一个独立命令：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await exec.run(clientId, {
  command: ["git", "status", "--short"],
  wait_ms: 5000
});
```

返回值包含任务 ID、运行状态、退出码与输出游标。等待时间到期后，任务可以继续运行；使用同一个 ID 读取或取消。大结果返回可追溯的归档元数据。[命令接口](docs/commands.md) · [编排与输出](docs/session-control.md)

## 终端就是工作会话

### 只在本机运行

Linux 上可以使用 `ssmcp local` 在当前项目启动本地 MCP 会话，无需远程服务器或 GitHub 登录。默认监听回环地址的可用端口；在终端执行 `/logs`，将本次 MCP 地址加入同机的 OAuth 客户端，并用其中的一次性登录码授权。终端会保持运行，退出后本次连接授权结束。

本地 `code_mode` 还需已安装的 Node.js、Bubblewrap（`bwrap`）和 `prlimit`，以及系统允许创建相应的 Linux namespace。缺失时会明确失败，不会绕过沙箱。同版 `code-mode.js` 已编入 Linux 客户端，每次本地会话使用自己的私有副本，无需额外安装脚本。详见[运行前提](docs/local-mode.md#运行前提)。

已有 `.local` 名称解析和受信任的本机 HTTPS 反向代理时，可运行 `ssmcp local --listen 127.0.0.1:8765 --public-url https://workstation.local`。代理须指向该固定端口；命令不自动配置 mDNS、证书或防火墙。详见[本地运行与 `.local` 入口](docs/local-mode.md)。本段描述源码能力，安装版本是否包含该命令，以 `ssmcp local --help` 为准。

### 使用同一入口选择连接目标

把本机或 HTTPS 中继保存为命名 context，随后用同一个参数选择：

~~~sh
ssmcp context create here --local
ssmcp context create work --server https://relay.example.com
ssmcp --context work --name dev
ssmcp context use here
~~~

`--context` 只选择本次目标，`context use` 保存新连接的默认选择；已有连接和保存的 session 继续使用原目标。用 `ssmcp context show` 查看目标与选择来源。命名目标不保存账号凭据；完整优先级和本地运行前提见[本地与远程目标](docs/connection-contexts.md)。安装版本是否支持，以 `ssmcp context --help` 为准。

### 对话与工具

默认对话页收起侧栏，把主要空间留给正文和输入。你可以在工具运行期间补充要求，也可以指定某个已登记的 Agent 接收。账号与本月剩余流量显示在输入区上方，底部保留连接来源与操作提示。

| 操作 | 用法 |
| --- | --- |
| 发给指定 Agent | 光标在草稿开头时按 **←**，选择成员，Enter / → 返回 |
| 引用文件、图片 | 输入 `@` 枚举工作区；支持中文、带空格路径和目录续补 |
| 粘贴图片 | **Ctrl + V** 或 `/paste`；`/attach PATH` 导入本地图片 |
| 引用技能 | 输入 `$` 枚举有效技能；项目技能覆盖同名全局技能 |
| 接受候选 | ↑ / ↓ 选择，Tab / Enter 补全；补全后再 Enter 发送 |
| 运行本地命令 | `!pwd`、`!cargo test`；输出与退出码回到会话 |
| 查看任务 | `/tasks`；`/tasks cancel JOB_ID` 停止指定命令任务 |
| 回答问题 | **Shift + ↑** 打开待答问题，保留普通草稿 |
| 继续旧会话 | `/resume` 选择本工作区的 JSONL 历史 |
| 交接给另一个 Agent | `/handoff [备注]` 生成上下文文件与可编辑引用草稿，选好收件人后发送 |
| 查看上下文 | `/context`、`/memory` 查看当前分支与有界记忆 |
| 压缩与分支 | `/compact [摘要]`、`/fork [名称]`、`/switch CONTEXT_ID` |
| 查看原生工具 | `/pi`、`/cc`、`/codex` 列出真实目录；`TOOL {...}` 显式调用 |
| 浏览工具输出 | 默认显示状态与最多两行已脱敏结果；点击 `Click expand` 或选中后按 Enter 展开 |
| 查看成员历史 | 在成员列表右键点击进程或线程，查看该真实身份的记录；Esc 返回 |
| 查看详细日志 | `/logs` 打开 Logs；其他对话页可用右键切换日志 |
| 展开状态侧栏 | **F3** 或 `/sidebar`；再按一次收起，F8 保存当前设置 |
| 复制与帮助 | `/copy` 复制最近回复；**F7** 原生选择；`/help` 打开完整帮助 |

工具调用成功结束时显示 `done`，运行中、失败、取消或超时显示实际状态。对话页只预览简短结果，详细记录可在 Logs 中查看；长输出仍可按任务游标或归档读取。

回复支持 Markdown 和常见代码块语法高亮。读取图片后，客户端会探测终端的 Kitty 图像能力；支持时显示清晰图片，其他终端显示有界半块预览或文字说明。超过 8 KiB 的整段粘贴会保存为私有工作区文件并插入引用，便于继续补充文字。

Agent 使用 `ssmcp.ui.message` 把进度与回答显示在这个会话里；重试可带幂等键，回执提供消息 ID 与历史保存状态。使用 `ssmcp.ui.ask` 非阻塞提问，再通过 `ssmcp.ui.questions` 读取答案。这些工具提供本地沟通通道，外部聊天应用仍决定自己的消息显示方式。

`ssmcp.ui.present` 支持文字、卡片、指标、表格、进度、步骤与条形图，复用同一条消息历史和复制通道。[结构化显示](docs/intelligence-ui.md)

分支上下文保留消息来源 ID 与内容哈希，并限制保存和返回的大小。`/compact` 可使用本地抽取或你提供的摘要；`/fork` 和 `/switch` 管理分支。它们不启动模型，也不重放历史任务。[上下文与分支](docs/context-memory.md)

未选择接收人时，用户要求由下一次符合条件的单个调用领取；选择成员后，仅绑定该成员身份的调用能领取。**已交付**表示要求进入了工具响应，执行进度仍需由接收者反馈。[输入与交付语义](docs/client-input.md) · [成员协作](docs/agent-collaboration.md)

### 从命令行连接对话与传递结果

`ssmcp list` 按进程和已登记线程显示树形目录，远程 `attach` 默认连接所选对象的聊天输入与输出。进程输入发给选定目录中的主接收入口及已登记线程；线程输入只发给确切的成员 ID。数字引用最近一次列表，保留 5 分钟；显示名称不会被当成线程身份。[选择规则](docs/client-accounts.md#选择进程与线程)

~~~sh
ssmcp list
ssmcp attach thread THREAD_ID
ssmcp send process PROCESS_ID '检查当前改动' | ssmcp wait --any
ssmcp send thread THREAD_ID '整理当前结论' |
  ssmcp wait --gather |
  ssmcp send thread NEXT_THREAD_ID --stdin
~~~

`send` 输出请求回执，`wait` 输出接收者的实际最终回复。可以等待任意一条、全部，或用 `--count N` 等待 N 个输入的结果；确认收到输入和进度更新都不算完成。输入在接收者下一次符合条件的原生调用响应中交付，这些命令不启动模型；停止等待或退出 attach 也不取消尚未完成的工作。[收发与等待](docs/client-accounts.md#发送输入与等待实际回复)

聊天收发需要中继与接收端客户端同时支持相应接口，仅更新本机程序不会更新远端服务。请求和回复保存在服务端的有界内存中，最长保留 1 小时，服务端重启会丢失这批状态。原有 Shell 终端仍可用 `ssmcp -i` 或 `ssmcp attach process PROCESS_ID --terminal main`；本地保存会话使用 `ssmcp attach --local NAME`。[兼容与保留边界](docs/client-accounts.md#聊天接口的兼容与保留边界)

成员历史只展示当前进程保留、能够核对真实身份的事件。旧记录中无法确定成员归属的内容留在进程视图，不会按显示名称分配给某个线程。[线程历史](docs/client-accounts.md#查看线程历史)

### 定时更新 Git 上游

连接存续期间，ssmcp 默认每 300 秒检查当前分支的远端上游并更新对应的远端跟踪引用；JSONL、无终端和后台连接同样运行。可设为 60–86400 秒，或用 `0` 关闭，修改对新连接生效。

```sh
ssmcp config get git-fetch-interval
ssmcp config set git-fetch-interval 300
ssmcp config set git-fetch-interval 0
```

首次检查、共享调度、超时与更新范围见[定时 Git fetch 配置](docs/configuration.md#background-git-fetch)。

### 先预览，再调整

```sh
ssmcp ui preview
ssmcp ui init
ssmcp ui check
```

内置 `dark`、`codex-dark`、`nord`、`solarized`、`light`、`minimal` 六种主题。通过 `[ui]` 配置调整侧栏、面板顺序和颜色，F5 应用显示配置；`/reload` 则重新启动客户端。终端支持原生图片且侧栏空间足够时显示方形像素二维码，窄窗口显示完整链接；网关提供教程时进入中英文指南，旧网关暂未提供教程时进入门户首页。[界面配置](docs/client-ui.md)

## 复用现有工具

ssmcp 默认启用命令、文件、会话和 Zuse CLI 管理工具，不要求安装其他 harness。Codex、Claude Code、Pi 和 OpenCode 默认不探测、不启动；需要时用 `ssmcp.harnesses.load` 显式加载一项，`ssmcp.harnesses.list` 只读取缓存状态。加载后保留该工具的完整 schema、会话和审批方式，实际模型调用仍须显式发起。[按需加载说明](docs/optional-harnesses.md)

`--input` 默认从终端接收聊天消息，`--agent` 默认由下一位调用方领取。要在启动时加载原生协议，请用 `--load-harness codex` 或 `--load-harness claude-code`。用 `ssmcp --list-harnesses` 查看支持项。`--load-harness ssmcp` 可以连接明确指定并已认证的 MCP 网关。[客户端输入](docs/client-input.md) · [协议加载](docs/agent-entry.md)

`ssmcp local --socket` 提供默认路径的本机 HTTP/MCP Unix socket，`--socket PATH` 可指定私有路径。时间工具支持当前 UTC、跨时区时间差和倒计时，终端直接显示运行时长与剩余时间，`/wait` 可查看详情。[本机连接](docs/local-mode.md) · [时间原语](docs/time-primitives.md)

- **开发工具**：Codex、Claude Code、Pi、OpenCode；能力以当前成功注册的目录为准。
- **桌面与浏览器**：直接运行独立 Zuse CLI，通过通用 `exec` 传递 argv、管理任务和读取结果。`computer` 原生工具只提供版本查询、下载、安装、卸载；没有内置动作代理或隐藏 fallback。[Zuse 工作流](docs/computer-use.md) · [Windows 安装范围](docs/windows-browser-setup.md)
- **上下文**：工作区文件、项目指令、有效技能视图和显式只读 context roots。

指定已有浏览器时，先确认所选 Zuse 版本支持的连接方式和实际会话，再操作用户选定的目标。OAuth 登录、用户打开链接和已授权外部 MCP 的通用转发仍保留；它们不恢复旧内置浏览器工具。[现有浏览器连接](docs/browser-existing-session.md) · [code_mode 示例](docs/code-mode.md)

### 管理 Zuse CLI

```sh
ssmcp computer version --json
ssmcp computer download --json
ssmcp computer install DOWNLOAD_ID_FROM_REPLY --json
```

版本来自实际 CLI 的 `--version`。下载选择官方稳定版并核验资产身份、大小与 SHA-256；安装仅写入当前管理器的私有目录，不覆盖外部 Zuse、不修改 PATH。使用返回的可执行路径运行任务；卸载必须提供真实 `installation_id`，且所有权和字节仍匹配。[四个管理工具与示例](docs/computer-use.md)

旧控制开关、兼容入口和默认 `Super + Alt + C` 快捷键已移除。独立 Zuse CLI 可直接使用，无需先切换控制权或进入 takeover 流程。[Zuse 管理与执行](docs/computer-use.md)

通用 Shell 与独立 Zuse 仍遵循操作系统权限；需要隔离时配置实际的账户、会话或沙箱边界。

## 独立核心库

[ztuio](https://github.com/LIghtJUNction/ztuio)、[zharness](https://github.com/LIghtJUNction/zharness) 和 [zplugin](https://github.com/LIghtJUNction/zplugin) 作为固定源码的 Rust 依赖参与构建，分别承担终端帧、扩展接口和本机动态库加载。三个库均使用固定 Git 子模块，ztuio 的包名与 Rust 导入名均为 `ztuio`，精确版本为 `0.1.0`。它们独立维护版本，不启动模型或外部 harness。源码构建请使用 `git clone --recurse-submodules`；已有 checkout 使用 `git submodule update --init --recursive`。[依赖与协同开发](docs/dependencies.md)

## 客户端掌握能力，服务端负责连接

服务端负责身份、会话路由、成员通信与受限 JavaScript 编排。客户端负责执行工具、组织上下文、保存输出和绘制界面。增加客户端原语后，调用方可以通过实时目录发现它，并继续使用通用 `call(clientId, name, arguments)`。

因此，你可以重写客户端，接入自己的远程 harness，或只替换 UI。现有界面也通过小接口读取状态和控制任务：

| 边界 | 可复用接口 |
| --- | --- |
| 能力发现 | 客户端版本、原生工具 schema、客户端资源 |
| 执行 | `terminal.*`、`ssmcp.exec.*`、原生 harness 工具 |
| 上下文 | `ssmcp.fs.*`、`ssmcp.skills.*`、`ssmcp.agents.*` |
| 会话界面 | `ssmcp.ui.*`、问答、历史、队列与源码事件 |
| 扩展策略 | 可选的客户端 `ToolCallHook`，调用前允许或拒绝，调用后观察结果 |

hook 默认关闭，不内置额外审核流程，也不改变工具原生权限。自定义策略由客户端开发者接入。当前连接统一使用 **wire v7**，旧协议会明确拒绝。[扩展接口与最小客户端](docs/client-extension.md) · [上下文边界](docs/context.md)

### Project Z：ztuio

终端帧差分、几何边界和定宽行已接入固定源码子模块 [ztuio](crates/ztuio/README.md)。这个小库使用 `no_std`，禁止 `unsafe`，没有第三方依赖；缓冲区由调用方提供，Unicode 宽度规则由宿主决定。它只计算该写哪些终端字节，图片协议、终端输入、主题和会话状态仍由客户端管理。

## 运行边界

工具使用客户端进程的操作系统权限；MCP 账号授权控制谁能调用设备。文件 API 受工作区和显式 context roots 限制，Shell 仍按操作系统权限执行。

本地历史采用版本化 JSONL，`/resume` 恢复文本上下文，不重放工具，也不恢复外部模型线程。问答队列当前保存在进程内；reload 后可从历史查看已记录正文。完整边界见[会话历史](docs/conversation-history.md)。

图片导入会在客户端生成有界预览文件，仍以私有文件引用发送；不改变 wire v7。原生图片读取限制为 400 KiB，支持 PNG / JPEG / GIF / WebP。GIF / WebP 经 `code_mode` 返回时还需要支持这些格式的 relay；旧 relay 的图像通道仍只有 PNG / JPEG。长工具输出保留有界预览与归档，展开界面事件只读取当前保留的显示正文。

## 开发

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/ssmcp.git
cd ssmcp
cargo build --locked --bins --example vt_snapshot
cargo run --locked --bin ssmcp -- ui preview
```

`make check` 检查格式和 Clippy；`make test-client-ui` 在隔离的真实 PTY 中验证输入、问答、图片、历史和重载。验证脚本使用测试自己的进程与工作区。

项目持续使用 ssmcp 自己的远程工具开发。[发布与构建](docs/releases.md) · [下一版本计划](docs/next-version.md)

## 文档

| 从这里开始 | 深入实现 |
| --- | --- |
| [账号、设备与临时接入](docs/client-accounts.md) | [工具注册与 harness](docs/backends.md) |
| [输入框与本地工作流](docs/client-input.md) | [客户端扩展与上下文工程](docs/client-extension.md) |
| [主题、布局与快捷键](docs/client-ui.md) | [Zuse、浏览器与能力资源](docs/browser-desktop.md) |
| [部署服务器](docs/deployment.md) | [完整输出与会话控制](docs/session-control.md) |
| [信任与默认服务器](docs/client-server.md) | [会话历史格式](docs/conversation-history.md) |

<details>
<summary>项目创作记录</summary>

[原创短片《日蚀》 · 24 秒](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/ssmcp-eclipse.mp4`）

</details>

---

[MIT License](LICENSE)
