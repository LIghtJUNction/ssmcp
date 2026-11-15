# 后端与 MCP 调用

ssmcp 0.2.0 在本机启动一个后端，把它的原生控制接口映射为远程 MCP 工具。后端负责执行命令、调用模型、保存会话和执行自己的权限政策；`ssmcpd` 验证通过 OAuth 签发的 access token，再经 SSH 转发请求。连接时的动态登录码只用于网页授权，调用工具前需完成 [OAuth 登录](deployment.md#5-建立连接并登录)。[实现入口](../src/backend/mod.rs)

本文示例中的 JSON 均为 `tools/call` 的 `params`，即 `{"name":"工具名","arguments":{...}}`。示例中的项目路径、thread ID、session ID 和请求 ID 必须换成当前本机及前一步响应中的实际值。

## 启动参数

```sh
ssmcp [-t shell|codex|cc|pi|opencode] [options] user@host [-- backend_args...]
ssmcp -t TYPE [--backend PROGRAM] --list-tools [-- backend_args...]
```

`-t` 默认 `shell`；`user@host` 始终是 SSH 目标，与 Agent 的账户或模型名称无关。`--backend PROGRAM` 替换本机原生可执行文件。`--` 后的参数原样传递，不经过额外 shell 解析；Shell 模式使用 `--shell /absolute/path`，不接受 Agent 参数。

| 模式 | ssmcp 启动的原生入口 | 参数边界 |
| --- | --- | --- |
| Shell | 当前 `SHELL` 的绝对路径，缺省 `/bin/sh` | `--shell` 可替换；独立 PTY 保留目录和进程状态 |
| Codex | `codex app-server --listen stdio://` | 先调用本机 `generate-json-schema --experimental`；`--listen` 由 ssmcp 管理 |
| Claude Code | `claude mcp serve` | 使用该子命令支持的配置参数；禁止切换 print、SDK URL 或输入输出协议 |
| Pi | `pi --mode rpc` | 可传模型、工具、扩展、session 等选项；保持 RPC 模式 |
| OpenCode | `opencode serve --stdio --hostname 127.0.0.1 --port 0` | 要求 v2 ready 握手；stdio、监听地址、端口和服务模式由 ssmcp 管理 |

原生入口使用启动 ssmcp 时的本机工作目录与环境。先在本机安装并配置相应 Agent；模型任务使用其已有登录或 provider 配置。服务器无须安装这些程序。

本地 `--list-tools` 会启动后端、完成原生握手和 discovery、打印 `{"type":"...","tools":[...]}`，然后关闭后端。这个选项不连接 SSH，也不接受 SSH 目标；它适合确认实际安装版本提供了什么。[CLI 实现](../src/client.rs)

## 目录发现与版本快照

远程 `tools/list` 每页最多返回 64 项，也可能因 schema 大小提前分页。收到 `nextCursor` 后，将它作为下一次 `tools/list` 的 `params.cursor`；游标包含目录指纹，目录发生变化时旧游标会报错，应从无游标请求重新开始。`--list-tools` 输出本地完整目录，不使用这层远程分页。

| 本轮探测版本 | 原生目录 | ssmcp 辅助能力 | 对外总数 |
| --- | --- | --- | ---: |
| Shell | 4 个 PTY 工具 | 无 | 4 |
| Codex `0.167.0-alpha.4` | 生成 182 个客户端方法，连接内的 `initialize` 由桥执行，剩余 181 项公开 | RPC、events、requests、respond、schema | 186 |
| Claude Code `2.1.325` | 22 个原生 MCP 工具 | events、requests、respond | 25 |
| Pi `1.5.2` | 33 个原生控制命令 | RPC、events、requests、respond | 37 |
| OpenCode `2.3.0` | 170 个 OpenAPI 操作，包含 HTTP、SSE 和 3 个 WebSocket 操作 | API、schema、events、stream_send、stream_close | 175 |

这些数字描述目录覆盖范围，不表示每个模型任务都已运行。Codex / Pi 的控制方法、Claude Code 的 MCP 工具和 OpenCode 的 API 操作是不同层次的接口；模型在任务中可用的工具由各 Agent 的安装版本、配置和权限决定。验证证据与范围见[验收报告](validation.md)。

## Shell

Shell 模式创建当前本机用户权限下的独立 PTY。`cd`、环境变量与前台程序都保留在这个会话中；远程输入和 PTY 输出同时显示在启动 ssmcp 的本机终端。

| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `terminal.write` | `data` | 写入 UTF-8 字符串；命令通常以 `\n` 结束 |
| `terminal.read` | `cursor`、`max_bytes`、`wait_ms` | 按字节游标读取输出，可短暂等待新数据 |
| `terminal.resize` | `rows`、`cols` | 设置 PTY 窗口，行列范围均为 `1..=1000` |
| `terminal.status` | 无 | 读取终端运行状态 |

```json
{"name":"terminal.write","arguments":{"data":"pwd\n"}}
```

```json
{"name":"terminal.read","arguments":{"cursor":0,"max_bytes":16384,"wait_ms":1000}}
```

`terminal.read` 返回 `text`、保留原字节的 `data_base64`、`next_cursor`、`truncated` 与 `running`。下一次使用 `next_cursor`。最近 1 MiB 输出保留在内存中；游标早于缓存时 `truncated:true`，读取从当前最早可用位置开始。

单次读取默认 16 KiB、最大 32 KiB；单次写入最大 16 KiB；等待上限为 5000 ms。远程 `terminal.write` 写入 `\u0003` 中断 PTY 前台程序；本机 `Ctrl-C` 断开 ssmcp 并清理本次后端。[PTY 实现](../src/pty.rs)

## Codex

Codex 模式使用本机 App Server 的生成式 schema，公开其全部客户端方法。原生 `thread/start` 映射为 `codex.thread_start`，方法名中的 `/` 替换为 `_`；参数 schema 来自当前可执行文件，嵌套定义随工具一起公开。桥完成一次 `initialize` / `initialized`，声明实验 API 能力；功能是否可执行仍由 Codex 的账户、功能开关和权限政策决定。[官方 App Server 文档](https://developers.openai.com/codex/app-server/)；[适配实现](../src/backend/codex.rs)

### 创建会话并运行任务

先创建本机项目的 thread：

```json
{"name":"codex.thread_start","arguments":{"cwd":"/absolute/project"}}
```

读取结果 `structuredContent.thread.id`，用它启动 turn：

```json
{"name":"codex.turn_start","arguments":{"threadId":"返回的thread.id","input":[{"type":"text","text":"阅读当前项目并说明主要模块"}]}}
```

`turn_start` 返回后继续读取 `ssmcp.codex.events`。事件保留原生 method 和 params，包括消息 delta、命令输出、工具调用和 `turn/completed`；以完成事件中的状态判断任务结局。`codex.turn_interrupt` 使用当前 `threadId` 与 `turnId` 中断任务。

```json
{"name":"ssmcp.codex.events","arguments":{"cursor":0,"max_events":50,"wait_ms":1000}}
```

线程、文件、命令、配置、MCP、技能等所有生成的方法都在目录中，未单独举例的方法也可按其 schema 调用。通用入口保留原始 method 名：

```json
{"name":"codex.rpc","arguments":{"method":"model/list","params":{}}}
```

### 服务端请求与回复

原生审批、用户输入、elicitation 和 dynamic tool 请求会进入 `ssmcp.codex.requests`。ssmcp 不替调用方选择答案；调用方先检查请求 method 和内容，再按该版本的响应 schema 回复原 ID。

```json
{"name":"ssmcp.codex.requests","arguments":{}}
```

```json
{"name":"ssmcp.codex.schema","arguments":{"name":"CommandExecutionRequestApprovalResponse.json"}}
```

例如，只有当前请求属于命令执行审批时，才可用下列结构拒绝该命令；`id` 必须保留原始类型和值：

```json
{"name":"ssmcp.codex.respond","arguments":{"id":"待处理请求的原id","result":{"decision":"decline"}}}
```

`ssmcp.codex.schema` 不带 `name` 时列出所有生成 schema 文件名。不同请求的结果结构不同，不能把审批的 `decision` 结构套给用户输入或 dynamic tool。`respond` 要求 `result` 与 `error` 二选一；未知和重复 ID 被拒绝。

## Claude Code

Claude Code 模式连接原生 `claude mcp serve`，每次发现目录时获取完整分页结果，仅为工具名增加 `cc.` 前缀。工具的 schema、描述、annotations、图片、结构化结果和错误字段按原生返回保留；本轮版本包含 `Agent`、`Bash`、`Read`、`Edit`、`Write` 等全部 22 项。[官方 MCP 服务说明](https://code.claude.com/docs/en/mcp#use-claude-code-as-an-mcp-server)；[适配实现](../src/backend/cc.rs)

调用 `cc.Agent` 可把任务交给本机 Claude Code agent。以下是当前 schema 的最小示例；运行这类任务会使用本机 Agent 的模型身份，协议探针没有代替真实模型任务验证。

```json
{"name":"cc.Agent","arguments":{"description":"Review project structure","prompt":"阅读当前项目，说明主要模块及关系，不修改文件。"}}
```

原生工具也可直接调用，例如读取本机文件：

```json
{"name":"cc.Read","arguments":{"file_path":"/absolute/project/README.md"}}
```

### 命令输出的实时范围

前台 `cc.Bash` 的输出在原生调用返回时可见。原生 progress 通知保留在事件流里，但 progress 心跳不能代表命令输出的字节流。

要观察运行中持续追加的 Bash 输出，可使用原生后台模式：

```json
{"name":"cc.Bash","arguments":{"command":"for n in 1 2 3; do printf 'step %s\n' \"$n\"; sleep 1; done","run_in_background":true}}
```

当原生结果包含 `backgroundOutputPath` 时，桥在本机增量读取该输出文件，向终端及 `ssmcp.cc.events` 写入 `background_output` 事件。事件包含 `path`、字节 `offset`、`text` 和 `data_base64`；每块最多 8 KiB，最多同时跟随 32 个输出文件。没有该字段时不能推断文件位置，也不会虚构增量输出。[官方后台输出说明](https://code.claude.com/docs/en/mcp#read-a-background-commands-output-as-an-mcp-client)

Claude Code 没有为这类输出文件提供完成通知；文件暂时不增长并不表示进程已结束。需要确认完成时，可由被运行的脚本输出约定标记，或查询该任务的业务状态。原生任务工具是否接受某个后台任务 ID 以安装版本的实际响应为准。跟随失败会产生 `background_output_error`，可继续用 `cc.Read` 读取原生给出的路径。

### 权限与原生请求

`claude mcp serve` 的逐次工具调用确认由远程 MCP 客户端负责。它的行为不能等同于 Claude 交互式终端的确认弹窗；直接调用 `cc.Bash` 可以直接执行命令。ssmcp 不添加跳过权限的启动参数，也不会自动答复原生请求。[官方权限说明](https://code.claude.com/docs/en/mcp#use-claude-code-as-an-mcp-server)

原生真正发来的服务端请求可通过 `ssmcp.cc.requests` 查询，并用 `ssmcp.cc.respond` 的 `{id,result}` 或 `{id,error}` 回复。`ssmcp.cc.events` 提供事件、调用和结果记录；这些辅助工具不改变原生目录。

## Pi

Pi 模式使用官方 JSONL RPC，完整映射 Pi 1.5.2 的 33 个控制命令，并提供 `pi.rpc` 兼容新增命令。当前官方 npm 包名为 `@earendil-works/pi-coding-agent`，可执行文件仍为 `pi`。Pi 的 MCP 客户端能力由 Pi 自己管理；本桥控制入口是其 RPC 接口。[官方 RPC 文档](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md)

| 命令类别 | `pi.` 前缀后的命令 |
| --- | --- |
| 输入与运行 | `prompt`、`steer`、`follow_up`、`abort`、`clear_queue`、`new_session` |
| 状态与消息 | `get_state`、`get_messages`、`get_commands` |
| 模型与推理 | `set_model`、`cycle_model`、`get_available_models`、`set_thinking_level`、`cycle_thinking_level`、`get_available_thinking_levels` |
| 队列策略 | `set_steering_mode`、`set_follow_up_mode` |
| 压缩与重试 | `compact`、`set_auto_compaction`、`set_auto_retry`、`abort_retry` |
| 命令执行 | `bash`、`abort_bash` |
| 会话管理 | `get_session_stats`、`export_html`、`switch_session`、`fork`、`clone`、`get_fork_messages`、`get_entries`、`get_tree`、`get_last_assistant_text`、`set_session_name` |

### 运行与完成

```json
{"name":"pi.prompt","arguments":{"message":"阅读当前项目并说明主要模块"}}
```

`prompt` 成功仅表示接受、排队或由扩展处理；`data.disposition` 为 `started`、`queued` 或 `handled`。通过 `ssmcp.pi.events` 持续读取，`agent_settled` 表示该次运行没有待自动继续的工作。`agent_end` 后仍可能发生重试、压缩或队列续跑；`handled` 的扩展命令可以完全不启动 agent。[命令生命周期](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-commands.md)

运行中追加输入时，`prompt` 需要设置 `streamingBehavior:"steer"` 或 `"followUp"`；也可直接调用 `pi.steer` / `pi.follow_up`。若要停止连同排队任务在内的工作，先 `pi.clear_queue`，再 `pi.abort`。`new_session` 等操作可能被扩展取消，需检查 `data.cancelled`。

`pi.bash` 的 `bash_execution_update` 事件带有原生请求 ID 和增量 `delta`，结果在命令结束后返回。原生命令响应完整保留在 `structuredContent`；`success:false` 同时标记 MCP `isError:true`。

### 扩展与对话

`pi.get_commands` 动态发现扩展 slash commands、prompt templates 和 skills。用 `pi.prompt` 的 `message:"/命令名 参数"` 调用它们；这个目录不包含模型工具 schema，也不包含只由 TUI 处理的内置 slash commands。Pi 在运行中使用的内置、自定义及 MCP 工具仍由其配置决定。[原生命令定义](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/modes/rpc/rpc-types.ts)

`select`、`confirm`、`input` 和 `editor` 对话进入 `ssmcp.pi.requests`。使用原 ID 显式回复，以下为拒绝确认的示例：

```json
{"name":"ssmcp.pi.respond","arguments":{"id":"待处理对话的原id","confirmed":false}}
```

`select` / `input` / `editor` 使用 `value` 字符串；任意对话可使用 `cancelled:true`。三种回答字段只能出现一个。通知、状态栏和 widget 记录只进入 events，不等待回复。Pi 可自行超时结束对话，因此 `respond` 的成功仅确认已发送，不能代表原任务成功；TUI 专用自定义组件不在 RPC 支持范围内。[官方扩展 UI 协议](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-extension-ui.md)

后续版本新增原生命令时可使用通用入口：

```json
{"name":"pi.rpc","arguments":{"type":"get_state","args":{}}}
```

## OpenCode v2

OpenCode 模式启动专属 v2 服务，绑定随机 loopback 端口，设置独立随机 Basic 密码，并从该进程的 `/openapi.json` 发现全部命名操作。工具名为 `opencode.<operationId>`，按 schema 保留 path、query、header 和 body 参数；HTTP、SSE 与 OpenAPI 标记的 WebSocket 操作均映射到同一 MCP 入口。[官方 v2 API](https://opencode.ai/docs/api)；[服务认证](https://opencode.ai/docs/cli/web#authentication)；[适配实现](../src/backend/opencode.rs)

需要 **OpenCode v2** 的 `serve --stdio` ready 握手。v1 的 `/doc`、服务密码变量及旧 session API 不能套用到本模式。ssmcp 管理私有监听地址与传输参数；Agent 的模型、插件及权限仍由本机 OpenCode 配置决定。

### 会话、任务与权限

创建本机项目会话：

```json
{"name":"opencode.session.create","arguments":{"body":{"title":"Remote review","location":{"directory":"/absolute/project"}}}}
```

读取 `structuredContent.body.data.id`，提交任务：

```json
{"name":"opencode.session.prompt","arguments":{"path":{"sessionID":"ses_REPLACE_WITH_RETURNED_ID"},"body":{"text":"阅读当前项目并说明主要模块"}}}
```

将 `ses_REPLACE_WITH_RETURNED_ID` 换成创建结果里的真实 ID。提交响应表示输入已进入原生处理流程；通过 `opencode.events` 和 `opencode.session.message.list` 查看实际进度与结果。桥在发布 endpoint 前订阅 `/api/event`，把模型、工具与权限事件同时写入本机终端和事件缓存。

`permission.asked` 表示原生策略需要回复。查询 `opencode.session.permission.list` 后，用原 session ID、request ID 调用回复接口；下面示例拒绝一次请求：

```json
{"name":"opencode.session.permission.reply","arguments":{"path":{"sessionID":"ses_REPLACE_WITH_RETURNED_ID","requestID":"per_REPLACE_WITH_PENDING_ID"},"body":{"decision":"reject"}}}
```

将两个 ID 占位值换成当前会话和待处理权限请求的真实 ID。原生支持的决定为 `once`、`always`、`reject`，对应本次允许、持续允许和拒绝；ssmcp 不自动选择。`permission.replied` 和实际任务结果继续通过事件与原生查询接口查看。

### Schema、HTTP 与流

`opencode.schema` 不带参数时列出操作，传 `operation_id` 返回原操作定义及引用 schema。可用它核对本机版本的参数；`opencode.api` 可按 `operation_id`，或本机绝对 `url_path` 加 `method` 调用原生接口。

```json
{"name":"opencode.schema","arguments":{"operation_id":"session.create"}}
```

```json
{"name":"opencode.api","arguments":{"method":"GET","url_path":"/api/info"}}
```

HTTP 返回保留 `status`、`content_type`、`headers` 和 `truncated`。JSON 结果使用 `body`，文本或截断预览使用 `body_text`，二进制结果使用 `body_base64`。JSON 请求使用 `body`，二进制请求使用 `body_base64`。`response_max_bytes` 默认 256 KiB、最大 1 MiB；`timeout_ms` 默认 30000、最大 120000，较长输出应使用原生分页、范围读取或流。

SSE 操作返回 `stream_id`，后续消息由 `opencode.events` 读取。本轮 OpenAPI 的三个 WebSocket 操作为 `pty.connect`、`experimental.persistentPty.connect`、`experimental.mcp.client`；连接结果同样返回 `stream_id`，并给出发送与关闭工具名。连接参数以 `opencode.schema` 中的原生定义为准。

```json
{"name":"opencode.stream_send","arguments":{"stream_id":1,"text":"pwd\n"}}
```

将上例 `stream_id` 换成实际连接返回值。`stream_send` 支持 text / binary / ping / pong，使用 `text` 或 `data_base64`，二者选一。WebSocket 接收事件的 type 为 `opencode.websocket`，包含 `stream_id`、`path` 和 `frame`；text 帧使用 `frame.text`，其余数据帧使用 `frame.data_base64`。

用 `opencode.stream_close` 的 `{stream_id}` 关闭自行打开的 SSE / WebSocket；`stream_id:0` 是桥始终维护的原生事件订阅，不接受手动关闭。原生事件连接重连后会记录 `ssmcp.stream_gap`，缺口期间的上游事件无法重放；事件缓存中的 `truncated:true` 则表示本机缓存已淘汰旧记录。

## 传输与资源边界

公网 MCP 使用 JSON HTTP 响应，Agent 的增量事件通过工具轮询读取。支持 MCP `2026-07-28` 的 `server/discover` 和逐请求元数据，以及 `2025-03-26`、`2025-06-18`、`2025-11-25` 的 `initialize`；无 MCP HTTP session、无公网 SSE，通知返回 `202`，已授权 MCP 资源路径的 `GET` / `DELETE` 返回 `405`。[HTTP 实现](../src/http.rs)

Codex / Claude Code / Pi 的事件入口分别为 `ssmcp.codex.events`、`ssmcp.cc.events`、`ssmcp.pi.events`，OpenCode 使用 `opencode.events`。读取参数统一为：

```json
{"cursor":0,"max_events":50,"wait_ms":1000}
```

响应包含 `events:[{cursor,event}]`、`next_cursor`、`start_cursor`、`truncated` 和 `closed`。事件游标计数记录，Shell 游标计数字节；两者不要混用。每次读取后保存 `next_cursor`，避免重复处理。

| 边界 | 0.2.0 行为 |
| --- | --- |
| 内部 SSH 协议 | wire v3；与 0.1 和早期 wire v2 开发快照不兼容，客户端与服务端需一起升级 |
| SSH 单帧与 MCP 请求体 | 最多 4 MiB，包含 JSON 结构开销 |
| 工具目录 | 每页最多 64 项，同时受 schema 字节预算限制；游标绑定目录指纹 |
| Agent 事件缓存 | 最近最多 8 MiB；旧记录被淘汰后 `truncated:true` |
| 单条缓存事件 | 超过 256 KiB 时替换为 `ssmcp.event_truncated`，保留原大小及预览 |
| 单页事件 | 最多 512 KiB；`max_events` 默认 50，范围 `1..=256` |
| 事件等待 | `wait_ms` 默认 0，最大 5000 ms |
| 并发 | 每条 SSH 连接最多 16 个在途请求；原生 JSONL peer 最多 8 个在途调用，为事件和回复留出空间 |
| Codex / CC / Pi 原生调用 | 最多等待 15 分钟；事件读取与对话回复可并行进行 |
| OpenCode HTTP 调用 | 默认 30 秒，调用参数可设到 120 秒；原生长任务通过事件或状态接口观察 |

超过桥帧上限的完整结果会返回明确错误，应缩小原生分页或输出限制。事件预览截断与完整工具响应是独立的：缓存中的摘要不会替换仍能在桥帧内传输的原始响应。[wire 定义](../src/wire.rs)；[事件缓存](../src/backend/events.rs)；[原生 peer](../src/backend/rpc.rs)

调用超时不能证明原操作已取消。先使用对应 Agent 的状态、任务或会话接口确认结果，再决定是否重试；修改文件和执行命令的请求尤其需要避免重复提交。

## 已知兼容边界

当前测试覆盖协议握手、目录发现、本机文件和命令操作、事件及显式拒绝流程；真实模型任务需要使用你自己的 Agent 配置验证。版本与证据以[验收报告](validation.md)为准，目录出现某个 API 不代表所有账户都获得其对应功能权限。

Pi 1.5.2 在挂接 RPC 输入前等待 `session_start` 扩展初始化；若扩展在这个阶段等待 UI 对话，上游进程可能无法接收回复。ssmcp 保持不自动确认，启动超时后结束该连接。运行后的原生扩展对话已通过独立 RPC 流程验证。[上游启动顺序](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/modes/rpc/rpc-mode.ts)

本次连接拥有其新建后端的生命周期。断开时清理本次进程与普通子任务；主动脱离会话并重新托管的外部服务，以及 Agent 已写入的会话、文件和远程状态，不会因断开而回滚。
