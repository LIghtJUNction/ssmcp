# ssmcp

### 把你的电脑接到 Agent。用一个 `code_mode` 调用原生工具。

ssmcp 是一个可自建的远程 harness 通道。客户端把命令、文件、浏览器和已安装的开发工具接入授权会话；Agent 通过 MCP 选择设备并调用这些工具。模型可以在云端，工作区、执行环境和工具状态留在你的机器上。

[快速开始](#快速开始) · [会话操作](docs/client-input.md) · [工具与浏览器](docs/backends.md) · [编写自己的客户端](docs/client-extension.md) · [部署服务器](docs/deployment.md)

[![Rust](https://img.shields.io/badge/Rust-1.89%2B-64748b?style=flat-square)](Cargo.toml) [![License: MIT](https://img.shields.io/badge/License-MIT-64748b?style=flat-square)](LICENSE) [![Releases](https://img.shields.io/badge/Download-Releases-4285f4?style=flat-square)](https://github.com/LIghtJUNction/ssmcp/releases)

---

## 一条连接，三种用法

| 你想做的事 | ssmcp 提供什么 |
| --- | --- |
| 让云端 Agent 操作开发机 | 原生 Shell、独立命令任务、文件、浏览器、桌面，以及实际发现到的 Codex / Claude Code / Pi / OpenCode / Playwright 工具 |
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

也可以使用 `cargo install ssmcp --locked --bin ssmcp`。安装脚本获取已发布版本；本文的新会话能力对应仓库中的 **0.9.0 源码**。[平台与安装选项](docs/releases.md)

### 2. 在项目目录连接

```sh
ssmcp alice@relay.example.com --name dev --title "开发与测试"
```

把服务器地址换成自己的服务地址。首次连接引导 GitHub 授权，并在该服务器绑定你选择的用户名。后续直接运行 `ssmcp`，即可复用最近的授权账号；没有图形浏览器时使用 `--no-browser`。

客户端主动建立 HTTPS/WSS 连接，无需在设备上开放入站端口。保持终端或后台客户端运行，就能使用它的工作区和工具。已有 SSH 部署可以显式运行 `ssmcp --ssh my-server`。

```sh
ssmcp list       # 查看已授权的在线设备
ssmcp -i         # 进入一台设备的交互终端
ssmcp whoami    # 查看当前账号
```

完整步骤见[账号与设备](docs/client-accounts.md)、[默认服务器与可信公钥](docs/client-server.md)和[SSH 配置](docs/configuration.md)。

### 3. 接入 MCP

将终端显示的 `https://你的服务器/mcp` 加入 ChatGPT 或其他支持 OAuth 的 MCP 客户端，再完成该应用的授权。CLI 登录与 MCP 应用授权分别管理。

对外入口保持三个：

| 入口 | 作用 |
| --- | --- |
| `list` | 发现当前身份可访问的设备与客户端工具目录 |
| `code_mode` | 用 JavaScript 组合原生调用，安排顺序、并发和结果读取 |
| `collab` | 同一会话中的成员身份、定向消息、广播与确认 |

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

## 原生 Intelligence UI 与图片队列

Agent 通过工具把进度和答复写入 ssmcp 本身的对话框。除了 Markdown 和代码高亮，`ssmcp.ui.present` 还可展示卡片、指标组、响应式表格、进度条、任务步骤和条形图；终端按主题与可用宽度布局，完整正文保留给 `/copy` 和历史。交互问题使用独立的 `/questions` 面板，回答时保留普通输入草稿。

复制截图后使用 Ctrl+V 或 `/paste`，看到附件标识后写要求并提交；`/attach PATH` 可直接加图片文件。图片和文字共同排队、重新编辑及重载交接。Shift+↑ 取回最新待交付要求，并暂存当前文字、图片和接收目标；`/reload` 重启客户端。`/sidebar` 或 F3 收起右侧状态面板，顶部 Active/Idle 与右下项目上下文继续保留。

Agent 发送普通文字使用 `say(clientId,{text,kind:"progress"})`，最终答复使用 `kind:"answer"`；结构化内容使用 `ui.present(clientId,{title,blocks})`。两者的响应都可能带回用户追加的需求，调用方应继续检查。

[完整界面与图片操作](docs/client-ui.md) · [Intelligence UI 数据格式](docs/intelligence-ui.md)

## 终端就是工作会话

客户端采用持续事件流和独立输入框。你可以在工具运行期间补充要求，也可以指定某个已登记的 Agent 接收。

| 操作 | 用法 |
| --- | --- |
| 发给指定 Agent | 光标在草稿开头时按 **←**，选择成员，Enter / → 返回 |
| 引用文件、图片 | 输入 `@` 枚举工作区；支持中文、带空格路径和目录续补 |
| 引用技能 | 输入 `$` 枚举有效技能；项目技能覆盖同名全局技能 |
| 接受候选 | ↑ / ↓ 选择，Tab / Enter 补全；补全后再 Enter 发送 |
| 运行本地命令 | `!pwd`、`!cargo test`；输出与退出码回到会话 |
| 查看任务 | `/tasks`；`/tasks cancel JOB_ID` 停止指定命令任务 |
| 回答问题 | **Shift + ↑** 打开待答问题，保留普通草稿 |
| 继续旧会话 | `/resume` 选择本工作区的 JSONL 历史 |
| 查看原生工具 | `/pi`、`/cc`、`/codex` 列出真实目录；`TOOL {...}` 显式调用 |
| 浏览长输出 | 选择事件后 Enter 折叠或展开；PgUp / PgDn、滚轮滚动 |
| 复制与帮助 | **F7** 冻结显示并使用终端原生选择；`/help` 打开完整帮助 |

回复支持 Markdown 和常见代码块语法高亮。读取图片后，客户端会生成终端缩略图。超过 8 KiB 的整段粘贴会保存为私有工作区文件并插入引用，便于继续补充文字。

Agent 使用 `ssmcp.ui.message` 把回复显示在这个会话里；使用 `ssmcp.ui.ask` 非阻塞提问，再通过 `ssmcp.ui.questions` 读取答案。这些工具提供本地沟通通道，外部聊天应用仍决定自己的消息显示方式。

未选择接收人时，用户要求由下一次符合条件的单个调用领取；选择成员后，仅绑定该成员身份的调用能领取。**已交付**表示要求进入了工具响应，执行进度仍需由接收者反馈。[输入与交付语义](docs/client-input.md) · [成员协作](docs/agent-collaboration.md)

### 先预览，再调整

```sh
ssmcp ui preview
ssmcp ui init
ssmcp ui check
```

内置 `dark`、`codex-dark`、`nord`、`solarized`、`light`、`minimal` 六种主题。通过 `[ui]` 配置调整侧栏、面板顺序和颜色，F5 应用显示配置；`/reload` 则重新启动客户端。[界面配置](docs/client-ui.md)

## 复用现有工具

ssmcp 发现并转发已安装工具的原生能力，保留它们的 schema、会话状态和审批方式。目录查询和原语调用不会自动启动模型回合；显式调用某个 harness 的模型接口仍以该接口的原生行为为准。

- **开发工具**：Codex、Claude Code、Pi、OpenCode；能力以当前成功注册的目录为准。
- **浏览器**：优先使用已安装 Playwright MCP 的扩展连接复用现有 Chrome；也提供独立 Chromium CDP 控制、批量动作和条件等待。
- **桌面**：按客户端实际平台提供窗口、截图和输入原语，支持独立桌面。
- **上下文**：工作区文件、项目指令、有效技能视图和显式只读 context roots。

浏览器扩展的安装记录、MCP 握手和浏览器配对是三个状态。先使用 `ssmcp.browser.discover`、`ssmcp.harnesses.list` 和实际工具目录确认连接，再调用枚举到的原语。ChatGPT 或其他浏览器扩展也遵循这个条件。[现有浏览器连接](docs/browser-existing-session.md) · [code_mode 示例](docs/client-input.md)

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

## 运行边界

工具使用客户端进程的操作系统权限；MCP 账号授权控制谁能调用设备。文件 API 受工作区和显式 context roots 限制，Shell 仍按操作系统权限执行。

本地历史采用版本化 JSONL，`/resume` 恢复文本上下文，不重放工具，也不恢复外部模型线程。问答队列当前保存在进程内；reload 后可从历史查看已记录正文。完整边界见[会话历史](docs/conversation-history.md)。

图片原生读取限制为 400 KiB，支持 PNG / JPEG / GIF / WebP。GIF / WebP 经 `code_mode` 返回时还需要支持这些格式的 relay；旧 relay 的图像通道仍只有 PNG / JPEG。长工具输出保留有界预览与归档，展开界面事件只读取当前保留的显示正文。

## 开发

```sh
git clone https://github.com/LIghtJUNction/ssmcp.git
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
| [主题、布局与快捷键](docs/client-ui.md) | [协议、资源与能力](docs/browser-desktop.md) |
| [部署服务器](docs/deployment.md) | [完整输出与会话控制](docs/session-control.md) |
| [信任与默认服务器](docs/client-server.md) | [会话历史格式](docs/conversation-history.md) |

<details>
<summary>项目创作记录</summary>

[原创短片《日蚀》 · 24 秒](docs/assets/ssmcp-eclipse.mp4)

</details>

---

[MIT License](LICENSE)
