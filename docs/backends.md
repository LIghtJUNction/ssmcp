# 后端与代码模式

ssmcp 0.3 系列对外只提供 `list` 和 `code_mode` 两个 MCP 工具。`list` 找到在线客户端并读取它们的原生能力说明；`code_mode` 执行 JavaScript，通过 SSH 调用选定客户端的后端。`-t` 决定本机启动 Shell、Codex、Claude Code、Pi 或 OpenCode，不会增加对外 MCP 工具。[HTTP 入口](../src/http.rs) · [后端分派](../src/backend/mod.rs)

<a id="启动参数"></a>

## 1. 启动客户端

先进入需要工作的本机目录，再运行 ssmcp。下面五条命令可以在不同终端运行，也可以分布在不同机器上；使用同一个 SSH 账户时，它们出现在同一份 `list` 中。

```sh
ssmcp --name build-shell user@host
ssmcp -t codex --name review-codex user@host
ssmcp -t cc --name review-claude user@host
ssmcp -t pi --name review-pi user@host
ssmcp -t opencode --name review-opencode user@host
```

| 参数 | 作用 |
| --- | --- |
| `-t TYPE`、`--type TYPE` | 选择 `shell`、`codex`、`cc`、`pi` 或 `opencode`；默认 `shell` |
| `--name NAME` | 客户端显示名称；默认包含主机名、后端类型和进程 ID |
| `--shell /absolute/path` | Shell 模式的可执行文件；默认使用绝对路径的 `SHELL`，否则 `/bin/sh` |
| `--backend PROGRAM` | 替换 Agent 模式的原生可执行文件 |
| `-- backend_args...` | 原样传给所选 Agent，不经过额外 shell 解析 |
| `--list-tools` | 本机启动后端、打印完整原生能力目录后退出；不连接 SSH、不接受 SSH 目标 |
| `-F FILE`、`-i FILE`、`-p PORT` | OpenSSH 配置、私钥文件和端口 |

服务器为每条连接生成独立 `id`，`--name` 仅用作显示标签。即使名称相同，也不会替换已有连接；分派时始终使用 ID。名称最多 128 个 UTF-8 字节，不能全为空白或包含控制字符。示例通过唯一名称找到客户端；同名连接存在时，请改为按 `list` 返回的 `id` 选择。[客户端实现](../src/client.rs) · [元数据约束](../src/wire.rs)

| `-t` | 本机原生入口 | 前提与边界 |
| --- | --- | --- |
| `shell` | 交互式 Shell + Linux PTY | 状态保留在此 PTY 中 |
| `codex` | `codex app-server --listen stdio://` | 可执行文件必须支持 `app-server generate-json-schema --experimental` |
| `cc` | `claude mcp serve` | 使用该入口实际提供的 MCP 目录 |
| `pi` | `pi --mode rpc` | 使用原生 JSONL RPC；扩展和模型配置来自本机 Pi |
| `opencode` | `opencode serve --stdio --hostname 127.0.0.1 --port 0` | 要求 v2 ready 握手和 `/openapi.json`；不兼容 v1 的旧入口 |

先在客户端安装所选 Agent，并按其要求配置账户或模型。ssmcpd 不需要安装这些 Agent；各 Agent 的安装及运行依赖由该 Agent 决定。

代码模式调用服务器上由系统包管理器安装的 Node.js 18 或更新版本，并使用系统 bubblewrap 0.8 或更新版本隔离每次执行。Debian/Ubuntu 可用 `sudo apt install nodejs bubblewrap`，Arch Linux 可用 `sudo pacman -S nodejs bubblewrap`；安装后确认版本满足要求。这两项依赖安装在服务器上；仅运行 `ssmcp` 客户端不需要它们，也不需要为代码模式安装 npm 包。

`make install` 把桥接脚本安装到 `$PREFIX/libexec/ssmcp/code-mode.js`，Node.js 和 bubblewrap 本身由系统包管理器维护。自定义脚本位置时，用服务环境变量 `SSMCP_CODE_MODE_RUNNER` 指定绝对路径。依赖缺失或系统不允许建立隔离环境时，`code_mode` 返回错误；`list` 仍可用于发现客户端。具体部署条件见[部署说明](deployment.md#系统代码运行环境)。

原生传输由 ssmcp 管理。不要用后端参数改写 Codex 的 `--listen`、Claude 的 print/输入输出/SDK 传输参数，或 OpenCode 的监听、stdio 和服务模式参数。Shell 模式只接受 `--shell`，不接受 `--backend` 或 Agent 参数。

连接成功后，stdout 输出 MCP `url`、一次性 `login_code`、剩余有效秒数 `login_expires_in`，以及 `client_id` 和客户端元数据。把 URL 添加到支持 OAuth 的 MCP 客户端，在授权页面输入本机显示的登录码。OAuth Authorization Code + S256 PKCE 完成后，MCP 客户端使用取得的 access token 调用工具；登录码只用于网页授权。具体步骤见[OAuth 登录说明](deployment.md#5-建立连接并登录)。

## 2. 用 list 找到客户端和能力

本文的 JSON 请求均是 MCP `tools/call` 的 `params`。首先调用：

```json
{"name":"list","arguments":{}}
```

结果的 `structuredContent.clients` 包含 `id`、`name`、`type`、`hostname`、`cwd` 和 `cwd_kind`。`cwd_kind` 为 `startup`：它记录启动 ssmcp 时的目录，不随 Shell 的 `cd` 改变，也不代表某个 Agent 内部 thread/session 的目录。`structuredContent.code_mode` 提供代码模式的调用说明。

为选定客户端读取能力目录时，将下面的 ID 换成 `list` 的实际返回值：

```json
{"name":"list","arguments":{"client_id":"CLIENT_ID_FROM_LIST"}}
```

这个结果包含 `client` 和 `tools`；每个能力提供名称、描述及 `inputSchema`。目录按最多 64 项分页，也可能因 schema 大小提前分页。存在 `nextCursor` 时，将它传入下一次请求；目录变更导致旧游标失效时，从无游标请求重新开始。[目录分页](../src/backend/mod.rs)

```json
{"name":"list","arguments":{"client_id":"CLIENT_ID_FROM_LIST","cursor":"NEXT_CURSOR_FROM_RESULT"}}
```

MCP 协议本身的 `tools/list` 始终只返回 `list` 和 `code_mode`，没有后续页。上述 `list` 工具返回的 `terminal.write`、`cc.Read` 等名称供代码模式内部调用，不能直接作为外部 `tools/call` 的工具名。

## 3. 用 code_mode 编排调用

把 JavaScript 源码放入 `code`，即可使用变量、分支、循环、`await` 和 `Promise.all`。下面查询刚才命名的 Shell 状态：

```json
{"name":"code_mode","arguments":{"code":"const c = clients.find(c => c.name === 'build-shell'); if (!c) throw new Error('Shell client is offline'); return await call(c.id, 'terminal.status', {});"}}
```

| JavaScript 接口 | 返回或作用 |
| --- | --- |
| `clients` | 本次调用开始时的只读客户端元数据快照 |
| `await call(clientId, toolName, arguments = {})` | 选定后端的完整 MCP 工具结果 |
| `await tools(clientId, cursor?)` | 一页原生能力目录；用 `nextCursor` 继续读取 |
| `print(value)`、`console.log(value)` | 记录本次代码模式的有界日志；也支持 `console.warn`、`console.error` |
| `return value` | 返回可序列化为 JSON 的值；未返回时为 `null` |

代码模式结果的 `structuredContent` 为 `{ "result": ..., "logs": [...] }`。其中 `result` 是显式返回值，`logs` 为字符串数组。`call` 保留原生结果中的 `content`、`structuredContent`、`isError` 等字段；原生工具可以正常返回 `isError: true`，调用方需检查这个字段。连接或转发错误会使对应调用失败，可用 JavaScript `try/catch` 处理。

以下代码在一次请求中查看多个 Shell 客户端。此后各小节的 JavaScript 块都应作为 `code_mode` 的 `code` 字符串发送。

```js
const states = [];
for (const c of clients.filter(c => c.type === "shell")) {
  const state = await call(c.id, "terminal.status", {});
  states.push({ id: c.id, name: c.name, state });
}
return states;
```

每次 `call` 或 `tools` 都会重新检查授权连接和目标连接。客户端离线时请重新调用 `list`；断线后重连会得到新 ID。JavaScript 变量不会跨请求保留，但 Shell 和 Agent 的本机会话仍由客户端进程维护。需要继续读取输出或会话时，把游标、thread ID 或 session ID 保存到 MCP 调用方，再显式传入下一段代码。

## 4. Shell：保留目录和进程状态

Shell 模式提供 `terminal.write`、`terminal.read`、`terminal.resize`、`terminal.status` 四项内部能力。命令输入和 PTY 输出显示在运行 ssmcp 的本机终端；`cd`、环境变量和前台程序保留在该 PTY 中。[PTY 实现](../src/pty.rs)

```js
const c = clients.find(c => c.name === "build-shell");
if (!c) throw new Error("Shell client is offline");
const before = await call(c.id, "terminal.status", {});
if (before.isError) return before;
await call(c.id, "terminal.write", { data: "pwd\n" });
return await call(c.id, "terminal.read", {
  cursor: before.structuredContent.next_cursor,
  max_bytes: 16384,
  wait_ms: 1000
});
```

单次 `read` 可能只拿到命令回显或部分输出，并不表示命令完成。继续使用返回的 `next_cursor` 读取；需要可靠的命令完成判断时，应让命令输出约定标记。`data_base64` 保留原始字节，`text` 提供可读文本，`truncated: true` 表示请求游标早于保留缓冲区。

PTY 最近 1 MiB 输出保存在内存中。单次读取最多 32 KiB，写入最多 16 KiB，`wait_ms` 最多 5000；`terminal.resize` 接受 `rows`、`cols`，范围均为 `1..=1000`。通过 `terminal.write` 写入 `"\u0003"` 可中断 PTY 前台程序；本机按 Ctrl-C 会结束 ssmcp 连接及其后端。

## 5. Codex：生成式 App Server 目录

Codex 模式从当前可执行文件生成 schema，并把客户端方法转换成内部能力：例如 `thread/start` 对应 `codex.thread_start`。ssmcp 完成连接初始化；其他生成方法、通用 `codex.rpc`、事件和待回复请求均通过代码模式调用。可用方法及参数取决于本机 Codex 版本，应先检查 `list({client_id})`。[Codex 适配实现](../src/backend/codex.rs)

在提供 `thread/start` 和 `turn/start` 的版本中，下面创建工作线程并提交一项任务：

```js
const c = clients.find(c => c.name === "review-codex");
if (!c) throw new Error("Codex client is offline");
const created = await call(c.id, "codex.thread_start", { cwd: c.cwd });
if (created.isError) return created;
const threadId = created.structuredContent.thread.id;
return await call(c.id, "codex.turn_start", {
  threadId,
  input: [{ type: "text", text: "阅读当前项目，说明主要模块，不修改文件。" }]
});
```

提交 turn 的响应不代表模型任务完成。继续调用 `ssmcp.codex.events`，读取原生事件及 `turn/completed` 的状态。`ssmcp.codex.requests` 返回待处理的审批、用户输入或动态工具请求；`ssmcp.codex.schema` 可查看响应 schema，`ssmcp.codex.respond` 使用原始 ID 和对应的 `result` 或 `error` 回复。ssmcp 不会自动选择审批答案。

```js
const c = clients.find(c => c.name === "review-codex");
if (!c) throw new Error("Codex client is offline");
return await call(c.id, "ssmcp.codex.requests", {});
```

`codex.rpc` 保留原生 method 名及参数，例如 `{ method: "model/list", params: {} }`。该入口不能重复初始化连接，也不会改变 Codex 自身的账户、沙箱、审批和功能开关。

## 6. Claude Code：发现原生 MCP 工具

Claude Code 模式连接 `claude mcp serve`，重新读取它的完整分页目录，并给每个原生名称加上 `cc.` 前缀。原生 schema、描述、annotations 和结果字段保留；工具是否存在由本机版本与配置决定。[Claude Code 适配实现](../src/backend/cc.rs)

当发现目录提供 `cc.Read` 且参数包含 `file_path` 时，可以读取启动目录中的 README：

```js
const c = clients.find(c => c.name === "review-claude");
if (!c) throw new Error("Claude Code client is offline");
return await call(c.id, "cc.Read", { file_path: c.cwd + "/README.md" });
```

如果目录提供 `cc.Agent`，可按其实际 `inputSchema` 把任务交给本机 Agent。原生服务真正发来的请求可通过 `ssmcp.cc.requests` 读取，再用 `ssmcp.cc.respond` 的 `{id, result}` 或 `{id, error}` 回复。直接调用原生 MCP 工具时，不能假定它会出现交互式 Claude 终端的确认弹窗。

Claude 的前台 Bash 输出通常随调用结果返回。桥记录原生进度事件；当成功的后台 Bash 结果包含 `backgroundOutputPath` 时，桥才增量跟随这个已知路径，将输出写入本机终端和 `ssmcp.cc.events`。没有该字段时不会猜测输出文件；文件暂时不增长也不能证明命令已结束。最多同时跟随 32 个输出文件。

## 7. Pi：原生 JSONL RPC

Pi 模式把已有控制命令映射为 `pi.*`，并通过 `pi.rpc` 转发本机支持的新命令。目录描述的是控制协议，模型工具、扩展和 MCP 配置仍归 Pi 管理。[Pi 适配实现](../src/backend/pi.rs)

```js
const c = clients.find(c => c.name === "review-pi");
if (!c) throw new Error("Pi client is offline");
const state = await call(c.id, "pi.get_state", {});
print(state);
return await call(c.id, "pi.prompt", {
  message: "阅读当前项目，说明主要模块，不修改文件。"
});
```

`pi.prompt` 成功表示接受、排队或交给扩展处理，不能作为完成信号。读取 `ssmcp.pi.events` 中的 `agent_settled` 判断没有自动续跑任务；`agent_end` 后仍可能发生重试、压缩或队列续跑。`pi.bash` 的增量输出同样进入事件日志。若要停止当前任务及排队消息，先调用 `pi.clear_queue`，再调用 `pi.abort`。

Pi 的扩展对话出现在 `ssmcp.pi.requests`。`ssmcp.pi.respond` 保留原始 `id`：确认对话用 `confirmed`，输入或选项用 `value`，取消用 `cancelled: true`，三者只能选一个。发送回复成功只表示已交给 Pi；扩展可能已独立超时。

`pi.get_commands` 可发现扩展 slash commands、prompt templates 和 skills。用 `pi.prompt` 的 `message` 传入 `/命令名 参数`；TUI 专用行为不因此获得远程支持。通用转发示例为 `call(c.id, "pi.rpc", {type: "get_state", args: {}})`。

## 8. OpenCode：从 OpenAPI 发现操作

OpenCode 模式启动专属 v2 服务，使用随机 loopback 端口和独立密码，再从 `/openapi.json` 生成 `opencode.<operationId>` 能力。`opencode.api` 保留按 `operation_id` 或 `method`、`url_path` 调用的入口；`opencode.schema` 返回操作与关联 schema。参数按 `path`、`query`、`headers`、`body` 分组。[OpenCode 适配实现](../src/backend/opencode.rs)

先在代码模式中查看当前安装版本提供哪些操作：

```js
const c = clients.find(c => c.name === "review-opencode");
if (!c) throw new Error("OpenCode client is offline");
return await call(c.id, "opencode.schema", {});
```

对于提供 `session.create` 的 v2 目录，可以按该操作的 schema 创建会话：

```js
const c = clients.find(c => c.name === "review-opencode");
if (!c) throw new Error("OpenCode client is offline");
return await call(c.id, "opencode.api", {
  operation_id: "session.create",
  body: { title: "Remote review", location: { directory: c.cwd } }
});
```

保留原生返回的 session ID，再按发现到的 prompt 和权限回复操作继续工作。桥在发布连接信息前订阅事件，`opencode.events` 可读取任务进展、权限请求与适配器日志；权限决定需要调用对应的原生操作，不会由 ssmcp 自动通过。

SSE 和 WebSocket 操作返回 `stream_id`。后续通过 `opencode.events` 读取数据，`opencode.stream_send` 写入 WebSocket，`opencode.stream_close` 关闭已打开的流。流 ID 是后端资源句柄；这些操作仍通过代码模式调用。

<a id="传输与资源边界"></a>

## 9. 事件、权限与运行边界

| 后端 | 事件或输出 | 待处理请求与回复 |
| --- | --- | --- |
| Shell | `terminal.read` | 控制字符由 `terminal.write` 显式发送 |
| Codex | `ssmcp.codex.events` | `ssmcp.codex.requests`、`ssmcp.codex.respond`、`ssmcp.codex.schema` |
| Claude Code | `ssmcp.cc.events` | `ssmcp.cc.requests`、`ssmcp.cc.respond` |
| Pi | `ssmcp.pi.events` | `ssmcp.pi.requests`、`ssmcp.pi.respond` |
| OpenCode | `opencode.events` | 发现到的原生 permission 操作 |

Agent 事件页接受 `cursor`、`max_events` 和 `wait_ms`，返回 `next_cursor`。把游标保存到下一次代码请求；事件缓存有界，`truncated: true` 表示旧记录已经丢失。每页最多 256 条事件，等待最多 5000 ms。一次长轮询结束不代表 Agent 任务完成。[事件日志实现](../src/backend/events.rs)

通过一条 SSH 连接的登录码完成 OAuth 后，该授权允许访问同一 SSH 账户下当前及后来连接的客户端。每次内部调用都会检查这条授权来源连接；它断开时，关联的 access token 和 refresh token 立即失效，即使其他客户端仍在线。其他 SSH 账户的客户端不会被列出或作为调用目标。[账户隔离实现](../src/broker.rs) · [OAuth 实现](../src/oauth.rs)

0.3.2 的登录码从签发起有效 30 分钟，成功授权一次即消耗；显示的剩余秒数已扣除后端启动耗时。默认 access token 最长有效 8 小时，refresh token 闲置期为 30 天，每次成功刷新后重新计时并轮换令牌；同一授权从首次登录起最长 90 天，客户端登记默认有效 365 天。新注册省略 `grant_types` 时默认提供刷新授权；显式只申请 `authorization_code` 时不签发 refresh token。MCP 客户端负责保存轮换后的新令牌并主动刷新，访问令牌到期后可静默续期，无须再次输入登录码。需要重新授权时，从新连接取得新码。见[静默续期流程](configuration.md#silent-refresh)与[有效期及撤销规则](deployment.md#oauth-lifetimes)。

JavaScript 的公开编排接口是 `clients`、`call`、`tools` 和日志函数。每次请求使用独立的系统 Node.js 进程，进程环境由 bubblewrap 隔离，不继承服务器凭据；宿主机的工作目录、用户目录和控制 socket 不挂载进该环境。文件及命令操作通过 `call` 交给选定的本机后端，按该本机用户的权限执行。

代码隔离依靠 Linux 进程、文件系统和网络命名空间。Node.js 的 `vm` 上下文及权限开关不能独立承担不可信代码的安全隔离；无法启动 bubblewrap 时不会退回直接执行。原生审批仍需显式回复，已提交的原生操作不会因脚本退出而回滚。[Node.js VM 说明](https://nodejs.org/api/vm.html) · [Node.js 权限模型](https://nodejs.org/api/permissions.html) · [bubblewrap](https://github.com/containers/bubblewrap)

| 代码模式限制 | 当前值 |
| --- | --- |
| 代码长度 | 64 KiB UTF-8 |
| V8 老年代堆 | 32 MiB；不是整个进程的内存上限 |
| Linux 数据段限制 `RLIMIT_DATA` | 256 MiB |
| Linux 虚拟地址空间限制 `RLIMIT_AS` | 2 GiB |
| 子进程 CPU 时间 | 2 秒；不包括等待后端的时间 |
| `timeout_ms` | 默认 30000；范围 `1..=900000` |
| 原生能力调用次数 | 每次代码模式最多 128 次 |
| 同时等待的原生调用 | 每次代码模式最多 16 次 |
| 返回 JSON / 日志 | 1 MiB / 64 KiB |
| 同时运行的代码任务 | 最多 4 个 |

超时、断线或结果过大时，已发送的原生命令可能仍在运行或已经完成；应先查询其状态，避免盲目重发写入、启动任务等操作。上表约束来自[运行时实现](../src/code_mode.rs)。

后端 fixture 验证协议映射与响应处理。实际模型任务使用本机配置的 CLI、账户及模型服务；本次验证范围见验收报告。新增版本的能力应由 `list` 和原生 schema 确认。
