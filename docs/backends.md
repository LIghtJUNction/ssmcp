# 后端与代码模式

ssmcp 对外提供 `list` 和 `code_mode` 两个 MCP 工具。`list` 找到当前授权可访问的在线客户端并读取能力目录；`code_mode` 执行 JavaScript，通过 SSH 编排该客户端的原生工具。每条新连接始终显示为 `shell`，同时提供交互式终端和独立命令任务，并可注册多个已安装 harness 的原生能力。[HTTP 入口](../src/http.rs) · [后端分派](../src/backend/mod.rs)

<a id="启动参数"></a>

## 1. 启动客户端

先在 Linux、macOS 或 Windows 进入需要工作的本机目录，再启动一个客户端。它始终提供交互式 shell，并自动发现 `PATH` 中可用的 Codex、Claude Code、Pi、OpenCode 原生接口。把 `octocat@github` 换成允许访问这条连接的 GitHub 用户名；安装方式和平台范围见[版本与安装](releases.md)。

```sh
ssmcp --allow octocat@github --name dev --title "检查当前项目并修复构建" user@host
```

| 参数 | 作用 |
| --- | --- |
| `--name NAME` | 客户端显示名称；默认包含主机名和进程 ID |
| `--title TITLE` | 供 agent 识别任务的会话标题；最多 512 个 UTF-8 字节 |
| `--allow USER@PROVIDER` | 指定可访问身份；目前支持 `github`，可重复或以逗号分隔，最多 32 个不同身份 |
| `--shell /absolute/path` | 选择交互式 shell；Unix 默认使用绝对路径的 `SHELL` 或 `/bin/sh`，Windows 使用绝对路径的 `COMSPEC` 或系统 `cmd.exe` |
| `--context-root PATH` | 增加只读 skills／AGENTS 文档根目录；最多 8 个，不扩展普通文件写入范围 |
| `--list-tools` | 发现并打印本机工具目录后退出；不连接 SSH、不接受 SSH 目标或 `--allow` |
| `-F FILE`、`-i FILE`、`-p PORT` | OpenSSH 配置、私钥文件和端口 |

`--name` 和 `--title` 是显示信息，允许重名。服务端为每条连接分配独立 `id`，所有调用用这个 ID 分派；重连后需重新读取 `list`。`--name` 最多 128 个 UTF-8 字节，不能全为空白或含控制字符；同名连接需按 ID 区分。`cwd` 记录启动目录，不随交互式 shell 的 `cd` 改变。[客户端参数](../src/client.rs) · [元数据约束](../src/wire.rs)

### 从旧的后端选择参数迁移

删除旧命令中的 `-t TYPE`、`--type TYPE`、`--backend PROGRAM` 和尾随的后端参数，保留 SSH、身份、名称和日志选项。客户端已移除这些选择入口；例如旧的 `ssmcp -t codex --name dev my-server` 改为 `ssmcp --name dev my-server`。把需要的原生 CLI 安装到 `PATH`，原生账户、插件、MCP 与模型设置继续在各 CLI 中维护。

```sh
# 不连接 SSH，先检查这台电脑实际发现了什么。
ssmcp --list-tools

# 同一连接提供 shell 和成功初始化的原生适配器。
ssmcp --name dev --title "继续项目开发" --allow octocat@github my-server
```

| 来源 | 本机原生入口或实现 | 可调用范围 |
| --- | --- | --- |
| ssmcp | 交互式终端、独立后台进程、文件、上下文原语 | 不依赖安装 coding harness；文件读写与命令执行直接在本机完成 |
| Codex | `codex app-server --listen stdio://`，并须支持 `app-server generate-json-schema --experimental` | App Server 方法；成功发现时注册 `codex.mcp.*`，通过原生 MCP 调用接口转发 |
| Claude Code | `claude mcp serve` | 该入口实际返回的 MCP 工具目录，统一加 `cc.` 前缀 |
| Pi | `pi --mode rpc` | 原生 JSONL RPC 控制命令；内部模型工具不自动变成独立 RPC 接口 |
| OpenCode | `opencode serve --stdio --hostname 127.0.0.1 --port 0` | v2 服务的 `/openapi.json` 操作；要求对应 ready 握手，不兼容 v1 旧入口 |

发现成功才会加入对应工具；缺少 CLI、版本不兼容或初始化失败会记录本机诊断，shell 连接仍可使用。调用 `ssmcp.harnesses.list` 可查看当前注册数量，完整名称、参数和权限以 `list({client_id})` 为准。安装一个 harness 不等于它的每个内部功能都提供可直接调用的接口。[自动发现与分派](../src/backend/mod.rs)

工具发现不为这些 harness 提交生成任务。后续操作是否调用模型由原生方法决定：例如 `codex.turn_start`、`pi.prompt` 和可能存在的 `cc.Agent` 会进入其任务或模型流程；直接 MCP 工具也可能在工具内部使用模型。普通读写文件与 shell 命令优先使用 ssmcp 自带的 `fs` 和 `ssmcp.exec.*`，不要把“注册到 code_mode”理解为所有方法都无需模型。

`ssmcpd` 和代码模式的隔离执行环境运行在 Linux。服务端通过系统 Node.js、bubblewrap 与 util-linux 的 `prlimit` 执行编排代码；客户端只需本机 OpenSSH 与所选 shell。服务端的安装条件见[部署说明](deployment.md#系统代码运行环境)。原生 rsync 端点目前也仅在 Linux 客户端提供；macOS／Windows 可使用文件和 loopback 网络原语。[rsync 平台分支](../src/rsync.rs)

带 `--allow` 时使用 `/mcp` 的 GitHub OAuth；省略它时使用 `/<SSH 用户名>` 的一次性登录码授权。交互式终端显示连接摘要；重定向 stdout 或使用 `--raw` 时得到机器可读连接 JSON。两种方式均经原有 SSH 公钥认证，具体流程见[配置与登录](configuration.md)。

## 2. 用 list 找到客户端和能力

本文的 JSON 请求均是 MCP `tools/call` 的 `params`。首先调用：

```json
{"name":"list","arguments":{}}
```

结果的 `structuredContent.clients` 包含 `id`、`name`、可选 `title`、`type`、`hostname`、`cwd` 和 `cwd_kind`。`cwd_kind` 为 `startup`：它记录启动 ssmcp 时的目录，不随 Shell 的 `cd` 改变，也不代表某个 Agent 内部 thread/session 的目录。`structuredContent.code_mode` 提供代码模式的调用说明。

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
{"name":"code_mode","arguments":{"code":"const c = clients.find(c => c.name === 'dev'); if (!c) throw new Error('Shell client is offline'); return await call(c.id, 'terminal.status', {});"}}
```

| JavaScript 接口 | 返回或作用 |
| --- | --- |
| `clients` | 本次调用开始时的只读客户端元数据快照 |
| `await call(clientId, toolName, arguments = {})` | 选定后端的完整 MCP 工具结果 |
| `await tools(clientId, cursor?)` | 一页原生能力目录；用 `nextCursor` 继续读取 |
| `exec.run(clientId, options)` | 启动独立命令，并在指定等待时间内返回状态和分离的输出流 |
| `exec.parallel(clientId, commands, {concurrency:4})` | 有界并行启动和观察命令，按输入顺序保留每项结果 |
| `exec.start/list/status/read/wait/cancel/forget` | 管理客户端后台命令；与服务端传输用的 `jobs.*` 分开 |
| `fs.readText/writeText/editText(clientId, options)` | 工作区内的原生 UTF-8 文件操作；成功写入产生本机 diff 记录 |
| `skills.list/view/read(clientId, options)` | 索引、摘要视图和原文读取；不会提交模型任务 |
| `agents.list/resolve/read(clientId, options)` | 查看指令来源、根到目标的优先级和原文 |
| `await exec.run(clientId, options)` | 启动独立命令、有界等待并读取一次输出；需检查 `running` 与退出码 |
| `await exec.parallel(clientId, commands, options?)` | 限制同时进行的命令编排，按输入顺序保留每项结果 |
| `context.roots(clientId)` | 查看工作区与额外只读上下文根 |
| `await reload(clientId)` | 重启已安装的客户端入口，随后用 `list` 获取新连接 ID |
| `outputs.read/stat(clientId, options)` | 按转储 ID 读取长输出或查看元数据 |
| `outputs.cleanup(clientId)` | 立即执行与自动清理相同的保留策略 |
| `print(value)`、`console.log(value)` | 记录本次代码模式的有界日志；也支持 `console.warn`、`console.error` |
| `return value` | 返回可序列化为 JSON 的值；未返回时为 `null` |

代码模式结果的 `structuredContent` 为 `{ "result": ..., "logs": [...] }`。其中 `result` 是显式返回值，`logs` 为字符串数组。`call` 保留原生结果中的 `content`、`structuredContent`、`isError` 等字段；原生工具可以正常返回 `isError: true`，调用方需检查这个字段。连接或转发错误会使对应调用失败，可用 JavaScript `try/catch` 处理。

`fs`、`skills`、`agents`、`exec`、`reload` 和 `outputs` 的便捷原语返回解包后的结构化结果，原生工具返回错误时会抛出异常；`exec.parallel` 把各项失败保留为 `rejected/reason`。直接 `call` 则返回完整 MCP 结果。更多文件、显式上下文根、rsync、端口与托管同步任务示例见[工作区](workspaces.md)、[上下文](context.md)与[会话控制](session-control.md)。

以下代码在一次请求中查看多个 Shell 客户端。此后各小节的 JavaScript 块都应作为 `code_mode` 的 `code` 字符串发送。

```js
const states = [];
for (const c of clients.filter(c => c.type === "shell")) {
  const state = await call(c.id, "terminal.status", {});
  states.push({ id: c.id, name: c.name, state });
}
return states;
```

每次 `call` 或 `tools` 都会重新检查当前授权及目标客户端的访问条件。客户端离线时请重新调用 `list`；断线后重连会得到新 ID。JavaScript 变量不会跨请求保留，但 Shell 和 Agent 的本机会话仍由客户端进程维护。需要继续读取输出或会话时，把游标、thread ID 或 session ID 保存到 MCP 调用方，再显式传入下一段代码。

## 4. 命令：并行执行、后台管理与等待

分步操作、完整输出读取和故障恢复见[独立命令指南](commands.md)。

普通命令通过 `exec` 执行，每项任务有独立 `job_id`、退出码和输出流。`command` 是 argv 数组，第一个元素是程序，其余元素是参数；不会隐式解析管道、重定向或变量展开。需要 Shell 语法时，显式运行该平台的 Shell，例如 Unix 的 `["sh", "-c", "printf hello"]` 或 Windows 的 `["cmd.exe", "/d", "/s", "/c", "echo hello"]`。默认目录是客户端启动目录，`cwd` 可指定另一个已存在的绝对目录；`env` 是对继承环境的覆盖。[命令实现](../src/backend/shell.rs) · [JavaScript helper](../src/code_mode.js)

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("Client is offline");
return await exec.run(c.id, {
  command: ["git", "status", "--short"],
  cwd: c.cwd,
  wait_ms: 5000,
  max_bytes: 16384
});
```

`exec.run` 将启动、等待和一次输出读取组合起来，返回 `job_id`、`running`、`state`、`exit_code`、`stdout`、`stderr`、两条流的游标及 `snapshot`。`snapshot.stdout` 与 `snapshot.stderr` 保存流和日志文件的元数据。只在 `running:false` 时判断最终状态；`exit_code:0` 表示正常退出，非零码是程序报告的失败，进程被信号或取消终止时可能没有退出码。`wait_timed_out` 记录等待步骤是否到期；紧随其后的读取可能已经观察到完成，因此最终状态以最新 `snapshot` 为准。

等待时间与命令寿命分别设置。`wait_ms` 默认为 5000，范围为 `0..=60000`，它到期后命令继续运行；保留 `job_id`，后续调用 `exec.wait` 或 `exec.read`。命令自身的 `timeout_ms` 默认为 0，表示不设执行期限；指定 `1..=900000` 时，到期会终止此任务的进程树。外层 `code_mode.timeout_ms` 只限制本次 JavaScript 请求，应留出比等待步骤更长的时间；它到期不会自动取消已经启动的命令。

### 后台任务与增量输出

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("Client is offline");
const job = await exec.start(c.id, {
  command: ["cargo", "test"], cwd: c.cwd
});
return {job_id: job.job_id, state: await exec.wait(c.id, job.job_id, {wait_ms: 5000})};
```

后续请求使用实际返回的 `job_id`。`exec.read` 默认立即返回；指定 `wait_ms` 时等待新输出或任务结束。stdout 与 stderr 各有独立字节游标，下一页分别传回 `stdout_cursor`、`stderr_cursor`，并检查各自的 `*_eof`。每条流每次读取最多 32 KiB；`*_base64` 保留原始字节，可读字符串中的非法 UTF-8 会被替换。

| Helper | 作用 |
| --- | --- |
| `exec.start(id, {command, cwd?, env?, timeout_ms?})` | 立即启动并返回任务状态 |
| `exec.list(id)` | 列出正在运行及保留的已完成任务 |
| `exec.status(id, jobId)` | 查询状态、耗时、退出码与日志元数据 |
| `exec.wait(id, jobId, {wait_ms:5000})` | 有界等待完成；到期保留后台任务 |
| `exec.read(id, jobId, {stdout_cursor, stderr_cursor, max_bytes, wait_ms})` | 增量读取两条输出流和同一时刻的状态快照 |
| `exec.cancel(id, jobId)` | 请求终止所属进程树并返回最新状态；仍需检查 `running`，对已完成任务可重复调用 |
| `exec.forget(id, jobId)` | 移除已完成任务的记录；正在运行的任务需先取消 |
| `exec.events(id, {cursor, max_events, wait_ms})` | 分页读取命令事件 |

每个客户端最多同时运行 32 个任务，最多保留 128 条记录；新任务达到记录上限时淘汰最早启动的已完成任务，正在运行的任务不会被淘汰。每条输出流在内存中保留最近 64 KiB；`stdout_truncated` 或 `stderr_truncated` 表示请求的游标已经落在保留范围之前。使用 `snapshot.stdout.artifact` 或 `snapshot.stderr.artifact` 的 ID，通过 `outputs.read` 分页读历史输出，并检查文件的 `complete`、`error` 和过期时间。日志文件遵循输出存储的容量与保留规则，`forget` 不立即删除它们。[输出存储](../src/output.rs)

任务由客户端进程管理，可以跨多次 `code_mode` 请求继续运行。客户端断线或重载会终止其后台任务；需要系统服务长期运行时，应通过操作系统的服务管理器管理。`jobs.*` 仍用于 rsync 传输任务，命令任务使用 `exec.*`。

### 并行命令

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("Client is offline");
return await exec.parallel(c.id, [
  {command: ["git", "status", "--short"], cwd: c.cwd},
  {command: ["git", "diff", "--stat"], cwd: c.cwd},
  {command: ["cargo", "test"], cwd: c.cwd, wait_ms: 5000}
], {concurrency: 3});
```

一次最多传入 32 项命令，`concurrency` 默认为 4，范围为 `1..=8`。结果顺序与输入一致：成功启动并完成观察的项为 `{status:"fulfilled", value:...}`，启动或观察失败的项为 `{status:"rejected", reason:...}`。其中程序非零退出仍会返回 `fulfilled`，需要检查 `value.exit_code`。某一项失败不会丢弃其他项的结果；默认每项每条流返回 2048 字节，后续按游标继续读取。

`concurrency` 限制同时执行的“启动 → 等待 → 读取”编排数量。某项等待到期后，其后台命令继续运行，编排会转去处理下一项，因此实际运行中的命令数可能超过 `concurrency`，仍受客户端 32 项上限约束。需要严格限制实际进程数量时，显式管理一组 `job_id`，等其中一项 `running:false` 后再启动下一项。多批等待需要把外层 `code_mode.timeout_ms` 设置得足够长，或分成多次请求。[helper 回归测试](../src/code_mode.rs) · [协议端到端测试](../scripts/e2e_agents.py)

### 交互式终端

`terminal.write`、`terminal.read`、`terminal.resize`、`terminal.status` 操作同一个交互式终端。命令输入和合并后的 PTY 输出显示在本机；`cd`、环境变量和前台程序保留在该终端中。独立 `exec` 任务不继承终端内后续发生的目录或环境变化。[Linux PTY](../src/pty.rs) · [macOS 与 Windows 终端](../src/pty_portable.rs)

下面的 `pwd` 用于 Unix shell；Windows 默认 `cmd.exe` 可发送 `"cd\r\n"` 查看当前目录。命令文本必须符合所选 shell 的语法。

```js
const c = clients.find(c => c.name === "dev");
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

单次 `terminal.read` 可能只拿到命令回显或部分输出，并不表示命令完成。继续使用返回的 `next_cursor` 读取；需要独立退出码与可靠等待时使用 `exec`。`data_base64` 保留原始字节，`text` 提供可读文本，`truncated: true` 表示请求游标早于保留缓冲区。

PTY 最近 1 MiB 输出保存在内存中。单次读取最多 32 KiB，写入最多 16 KiB，`wait_ms` 最多 5000；`terminal.resize` 接受 `rows`、`cols`，范围均为 `1..=1000`。通过 `terminal.write` 写入 `"\u0003"` 可中断 PTY 前台程序；本机按 Ctrl-C 会结束 ssmcp 连接及其后端。

### 标题、重载与长输出

`--title` 随连接进入 `list`，供 agent 确认本轮任务。安装更新后可按[重载协议](#9-更新后的客户端重载)重启原入口；重载应答与新版本已经在线是两个状态，需要重新读取 `list` 确认。

长输出超过 32 KiB 或 500 行时按来源转储，返回 ID、路径、摘要、字节数和行数。Unix 使用 `/tmp/ssmcp/<uid>`，Windows 默认使用 `%LOCALAPPDATA%\ssmcp\output`，未设置 LOCALAPPDATA 时使用用户临时目录下的 `ssmcp/output`；返回路径属于目标客户端机器。默认保留 24 小时，已结束文件总量上限 512 MiB，单文件上限 256 MiB，活动文件不参加自动淘汰。具体的 `complete`、范围及截断语义见[长输出护卫](session-control.md#长输出自动转储)与[实现](../src/output.rs)。

## 5. Codex：生成式 App Server 目录

Codex 适配器从当前可执行文件生成 schema，并把客户端方法转换成内部能力：例如 `thread/start` 对应 `codex.thread_start`。ssmcp 完成连接初始化；其他生成方法、通用 `codex.rpc`、事件和待回复请求均通过代码模式调用。可用方法及参数取决于本机 Codex 版本，应先检查 `list({client_id})`。[Codex 适配实现](../src/backend/codex.rs)

适配器也尝试从原生 MCP server 状态中发现工具，成功项注册为 `codex.mcp.<server>.<tool>`，通过 `mcpServer/tool/call` 直接转发。名称与参数以目录为准；查询或原生接口失败时会记录诊断，不会伪造缺失工具。Codex 的 MCP 目录、App Server 方法和模型 turn 是三类不同入口。

在提供 `thread/start` 和 `turn/start` 的版本中，下面创建工作线程并提交模型任务。这段示例用于明确需要 Codex 处理任务的场景；查询 schema、文件或执行 shell 无须提交 turn：

```js
const c = clients.find(c => c.name === "dev");
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
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("Codex client is offline");
return await call(c.id, "ssmcp.codex.requests", {});
```

`codex.rpc` 保留原生 method 名及参数，例如 `{ method: "model/list", params: {} }`。该入口不能重复初始化连接，也不会改变 Codex 自身的账户、沙箱、审批和功能开关。

## 6. Claude Code：发现原生 MCP 工具

Claude Code 适配器连接 `claude mcp serve`，重新读取它的完整分页目录，并给每个原生名称加上 `cc.` 前缀。原生 schema、描述、annotations 和结果字段保留；工具是否存在由本机版本与配置决定。[Claude Code 适配实现](../src/backend/cc.rs)

当发现目录提供 `cc.Read` 且参数包含 `file_path` 时，可以读取启动目录中的 README：

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("Claude Code client is offline");
return await call(c.id, "cc.Read", { file_path: c.cwd + "/README.md" });
```

如果目录提供 `cc.Agent`，可按其实际 `inputSchema` 把任务交给本机 Agent。原生服务真正发来的请求可通过 `ssmcp.cc.requests` 读取，再用 `ssmcp.cc.respond` 的 `{id, result}` 或 `{id, error}` 回复。直接调用原生 MCP 工具时，不能假定它会出现交互式 Claude 终端的确认弹窗。

Claude 的前台 Bash 输出通常随调用结果返回。桥记录原生进度事件；当成功的后台 Bash 结果包含 `backgroundOutputPath` 时，桥才增量跟随这个已知路径，将输出写入本机终端和 `ssmcp.cc.events`。没有该字段时不会猜测输出文件；文件暂时不增长也不能证明命令已结束。最多同时跟随 32 个输出文件。

## 7. Pi：原生 JSONL RPC

Pi 适配器把已有控制命令映射为 `pi.*`，并通过 `pi.rpc` 转发本机支持的新命令。目录描述的是控制协议，模型工具、扩展和 MCP 配置仍归 Pi 管理。[Pi 适配实现](../src/backend/pi.rs)

```js
const c = clients.find(c => c.name === "dev");
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

OpenCode 适配器启动专属 v2 服务，使用随机 loopback 端口和独立密码，再从 `/openapi.json` 生成 `opencode.<operationId>` 能力。`opencode.api` 保留按 `operation_id` 或 `method`、`url_path` 调用的入口；`opencode.schema` 返回操作与关联 schema。参数按 `path`、`query`、`headers`、`body` 分组。[OpenCode 适配实现](../src/backend/opencode.rs)

先在代码模式中查看当前安装版本提供哪些操作：

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("OpenCode client is offline");
return await call(c.id, "opencode.schema", {});
```

对于提供 `session.create` 的 v2 目录，可以按该操作的 schema 创建会话：

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("OpenCode client is offline");
return await call(c.id, "opencode.api", {
  operation_id: "session.create",
  body: { title: "Remote review", location: { directory: c.cwd } }
});
```

保留原生返回的 session ID，再按发现到的 prompt 和权限回复操作继续工作。桥在发布连接信息前订阅事件，`opencode.events` 可读取任务进展、权限请求与适配器日志；权限决定需要调用对应的原生操作，不会由 ssmcp 自动通过。

SSE 和 WebSocket 操作返回 `stream_id`。后续通过 `opencode.events` 读取数据，`opencode.stream_send` 写入 WebSocket，`opencode.stream_close` 关闭已打开的流。流 ID 是后端资源句柄；这些操作仍通过代码模式调用。

## 9. 更新后的客户端重载

更新磁盘上的 ssmcp 后，在当前工具目录确认 `ssmcp.client.reload` 可用，再通过 `code_mode` 调用 `await reload(clientId)`。helper 返回原生重载应答；`accepted:true` 表示请求已经被接收，仍须再次调用公开 `list` 验证新连接。支持此协议的客户端会等待服务器确认应答已收取，随后停止本连接的后台任务、原生适配器和转发，并从原安装入口启动更新后的程序，保留参数、cwd、环境与允许列表。[重载握手](../src/client.rs) · [重启实现](../src/client/reload.rs)

重载时存在未结束的内部调用会返回 busy，应先等调用完成。已返回 `job_id` 的后台任务不属于在途请求，但会在重载时被终止；先完成或取消需要保留结果的任务。`reload` 不安装新版本，也不迁移进程和会话状态；它使用当前安装路径上的程序。初次从不支持该能力的旧版升级，需要在本机普通重启一次。

重连会分配新的 `client_id`，可结合 `title`、`name`、`cwd` 辨认目标，避免假定标签唯一。有效的 GitHub 身份授权可发现允许同一身份的新连接；登录码授权绑定旧连接，断线后须使用新码重新授权。结果不确定时先查 `list` 与本机日志，避免直接重复重载。[访问控制](../src/broker.rs) · [重载测试](../tests/client_reload.rs)

<a id="传输与资源边界"></a>

## 10. 事件、权限与运行边界

| 后端 | 事件或输出 | 待处理请求与回复 |
| --- | --- | --- |
| Shell | `terminal.read` | 控制字符由 `terminal.write` 显式发送 |
| 独立命令 | `exec.read`、`exec.events` | `exec.wait`、`exec.cancel`、`exec.forget` |
| Codex | `ssmcp.codex.events` | `ssmcp.codex.requests`、`ssmcp.codex.respond`、`ssmcp.codex.schema` |
| Claude Code | `ssmcp.cc.events` | `ssmcp.cc.requests`、`ssmcp.cc.respond` |
| Pi | `ssmcp.pi.events` | `ssmcp.pi.requests`、`ssmcp.pi.respond` |
| OpenCode | `opencode.events` | 发现到的原生 permission 操作 |

Agent 事件页接受 `cursor`、`max_events` 和 `wait_ms`，返回 `next_cursor`。把游标保存到下一次代码请求；事件缓存有界，`truncated: true` 表示旧记录已经丢失。每页最多 256 条事件，等待最多 5000 ms。一次长轮询结束不代表 Agent 任务完成。[事件日志实现](../src/backend/events.rs)

GitHub 模式在客户端接入时，把 `--allow` 中的用户名解析为稳定的 GitHub 用户 ID；浏览器登录取得的身份只能访问明确允许此 ID 的在线客户端。同一授权可发现随后接入且允许该身份的客户端，也可以在尚无可访问客户端时完成授权，此时 `list` 为空。身份绑定只保留到本条 SSH 连接结束，重连会重新解析用户名。GitHub 授权不绑定某一条 SSH 来源连接；客户端离线会移除它自身，但不会因此撤销 GitHub OAuth 令牌。[身份解析](../src/identity.rs) · [访问控制](../src/broker.rs)

传统登录码模式允许访问同一 SSH 账户下当前及后来连接、且未设置 `--allow` 的客户端，始终排除所有带有 `--allow` 的客户端。每次内部调用会检查签发登录码的来源连接；该连接断开时，其关联的 access token 和 refresh token 立即失效，即使其他客户端仍在线。[OAuth 实现](../src/oauth.rs)

默认 access token 有效期为 8 小时；refresh token 闲置期为 30 天，每次成功刷新后重新计时并轮换令牌，同一授权从首次登录起最长 90 天。客户端登记默认有效 365 天。新注册省略 `grant_types` 时默认提供刷新授权；显式只申请 `authorization_code` 时不签发 refresh token。MCP 客户端负责保存轮换后的新令牌并主动刷新，access token 到期本身不要求重新交互登录。传统模式的登录码有效 30 分钟、只能使用一次；需要重新授权时，应从新连接取得新码。访问令牌、刷新令牌与未完成授权仍保存在内存中，ssmcpd 重启后需要重新授权；启用客户端登记持久化时，未过期的登记可跨重启保留。见[静默续期流程](configuration.md#silent-refresh)与[有效期及撤销规则](deployment.md#oauth-lifetimes)。

JavaScript 提供 `clients`、`call`、`tools`、日志函数，以及 `exec`、`fs`、`outputs`、`context`、`skills`、`agents`、`reload` 等 helper；完整说明可读取 `ssmcp://guide` 与 `ssmcp://limits`。每次请求使用独立的系统 Node.js 进程，进程环境由 bubblewrap 隔离，不继承服务器凭据；宿主机的工作目录、用户目录和控制 socket 不挂载进该环境。helper 和 `call` 将操作交给选定的客户端，按该本机用户的权限执行。

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

后端 fixture 验证协议映射与响应处理；跨平台发布测试另外执行真实文件与原生进程操作。它们不证明每个已安装 harness、插件、MCP 服务器或模型都可用。实际任务依赖本机原生 CLI、账户和服务，新增能力仍需由 `list` 与原生 schema 确认。[跨平台原语测试](../tests/release_client.rs)
