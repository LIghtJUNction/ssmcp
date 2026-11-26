# 统一客户端、Harness 与代码模式

本开发分支对外提供 `link`、`list`、`code_mode`、`collab`、`await`、`events` 和 `messages` 七个 MCP 工具。`list` 枚举当前获准客户端；`code_mode` 编排原生操作；`collab` 管理现有参与者；`await` 观察任务、协作对象或用户输入；`messages` 按明确收件人处理原生消息。每条新连接显示为 `shell`，提供交互式终端和独立命令任务。[HTTP 入口](../src/http.rs) · [后端分派](../src/backend.rs)

<a id="启动参数"></a>

## 1. 启动客户端

先在 Linux、macOS 或 Windows 进入需要工作的本机目录，再启动一个客户端。它始终提供交互式 shell，以及原生命令和工作区能力。把 `octocat@github` 换成允许访问这条连接的 GitHub 用户名；安装方式和平台范围见[版本与安装](releases.md)。

```sh
ssmcp alice@relay.example --name dev --title "检查当前项目并修复构建"
```

| 参数 | 作用 |
| --- | --- |
| `--name NAME` | 客户端显示名称；默认包含主机名和进程 ID |
| `--title TITLE` | 供 agent 识别任务的会话标题；最多 512 个 UTF-8 字节 |
| `--shell NAME\|PATH` | 选择新终端和 `!命令` 的 shell。Unix 默认优先用 zrun，其次是 `SHELL`，最后用 `/bin/sh`。Windows 默认用 `COMSPEC`，最后用系统 `cmd.exe` |
| `--context-root PATH` | 增加只读 skills／AGENTS 文档根目录；最多 8 个，不扩展普通文件写入范围 |
| `--list-tools` | 打印本机工具目录后退出 |
| `-F FILE`、`-i FILE`、`-p PORT` | OpenSSH 配置、私钥文件和端口 |

在对话里输入 `/chsh` 查看当前 shell，输入 `/chsh zsh` 或 `/chsh /绝对路径` 保存首选项。使用 `/chsh system` 选择系统默认，或用 `/chsh auto` 恢复自动选择。切换完成后，新终端和 `!命令` 使用所选 shell，已打开的终端继续运行原进程。

首选项保存在 `[client].shell`。启动时 `--shell` 优先于此配置。可直接用 `--shell zrun` 选择自动回退，或用其他名称、绝对路径指定必须能启动的程序。回退会写入警告日志，全部启动失败会记录错误。`ssmcp.exec.start` 继续直接执行调用方提供的 argv。 [客户端配置示例](../packaging/ssmcp-client.toml.example)

`--name` 和 `--title` 是显示信息，允许重名。服务端为每条连接分配独立 `id`，所有调用用这个 ID 分派；重连后需重新读取 `list`。`--name` 最多 128 个 UTF-8 字节，不能全为空白或含控制字符；同名连接需按 ID 区分。`cwd` 记录启动目录，不随交互式 shell 的 `cd` 改变。[客户端参数](../src/client.rs) · [元数据约束](../src/wire.rs)

### 从旧的后端选择参数迁移

删除旧命令中的 `-t TYPE`、`--type TYPE`、`--backend PROGRAM` 和尾随的后端参数，并把旧 SSH 接入改为明确的 HTTPS 服务器与账号名单。例如使用 `ssmcp alice@relay.example --name dev`。把需要的原生 CLI 安装到 `PATH`，原生账户、插件、MCP 与模型设置继续在各 CLI 中维护。[账号接入](client-accounts.md)

```sh
# 不连接 SSH，先检查这台电脑实际发现了什么。
ssmcp --list-tools

# 同一连接提供原生 shell、命令任务和工作区能力。
ssmcp alice@relay.example --name dev --title "继续项目开发"
```

外部客户端通过标准 MCP 接入。旧导入选项见[迁移说明](optional-harnesses.md)，显式 SSMCP peer 见[连接说明](agent-entry.md)。

桌面与浏览器程序直接调用独立 Zuse CLI，经通用 exec 管理任务；`computer` 只提供版本、下载、安装和卸载。SSMCP 不再加载 Playwright 专用桥或内置浏览器后端。外部 MCP 的通用目录与调用转发继续遵循真实 schema 和授权。[Zuse 管理与执行](computer-use.md)


`ssmcpd` 和代码模式的隔离执行环境运行在 Linux。服务端通过系统 Node.js、bubblewrap 与 util-linux 的 `prlimit` 执行编排代码；客户端只需本机 OpenSSH 与所选 shell。服务端的安装条件见[部署说明](deployment.md#系统代码运行环境)。原生 rsync 端点目前也仅在 Linux 客户端提供；macOS／Windows 可使用文件和 loopback 网络原语。[rsync 平台分支](../src/rsync.rs)

在被控端运行 `ssmcp 用户名@中继服务器` 并完成设备授权。会话内的 `/allow add 用户名`、`/allow list` 和 `/allow remove 用户名` 管理可访问当前会话的控制者。交互终端显示连接摘要，重定向 stdout 或使用 `--raw` 可取得机器可读输出。[账号与授权](client-accounts.md)

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

这个结果包含 `client` 和 `tools`；每个能力提供名称、描述及 `inputSchema`。目录按最多 64 项分页，也可能因 schema 大小提前分页。无游标的请求读取当前注册表，并为这次扫描保存合并后的目录快照；存在 `nextCursor` 时，将它传入下一次请求。后续页面读取同一快照，因此扫描途中原生目录变化不会改变已开始的分页。后台刷新后，重新发送无游标请求可读取最新注册表。[分页实现](../src/backend.rs) · [合并目录快照](../src/runtime.rs)

`list` 与 `tools` 共享同一注册表。注册项保留提供者、原生名称、完整 Schema、可用状态与代号；不同来源使用独立命名空间。目录中的工具不因名称相同而互相覆盖；调用时仍验证当前提供者和代号，陈旧快照不会授予已失效工具的调用权。

每条客户端连接最多保留四份目录快照，按序列化 JSON 计合计不超过 64 MiB。成功读取分页会续期 120 秒的空闲时限，但自创建起最长保留 900 秒。无效游标不会续期，复用未变化的目录也不会重置创建时间；LRU 和容量限制可能提前淘汰快照。过期或被淘汰的游标会明确要求从无游标请求重新开始。游标只属于对应的客户端连接，重连后需用新的 ID 重新扫描。

```json
{"name":"list","arguments":{"client_id":"CLIENT_ID_FROM_LIST","cursor":"NEXT_CURSOR_FROM_RESULT"}}
```

MCP 协议本身的 `tools/list` 返回上述七个工具，没有后续页。`collab` 的 `action` 选择加入、成员发现、发送、接收或确认等会话内 agent 操作；它不代替控制端登录。`terminal.write`、`ssmcp.fs.read` 等原生名称供代码模式内部调用，完整参数以当前原生目录为准。

## 3. 用 code_mode 编排调用

把 JavaScript 源码放入 `code`，即可使用变量、分支、循环、`await` 和 `Promise.all`。下面查询刚才命名的 Shell 状态：

```json
{"name":"code_mode","arguments":{"code":"const c = clients.find(c => c.name === 'dev'); if (!c) throw new Error('Shell client is offline'); return await call(c.id, 'terminal.status', {});"}}
```

| JavaScript 接口 | 返回或作用 |
| --- | --- |
| `clients` | 本次调用开始时的只读客户端元数据快照 |
| `await call(clientId, toolName, arguments = {})` | 选定提供者的完整 MCP 工具结果 |
| `await tools(clientId, cursor?)` | 一页原生能力目录；用 `nextCursor` 继续读取 |
| `await tools.search(clientId, options)` | 有界目录检索；默认摘要，`include_schemas:true` 返回完整原生 descriptor |
| `exec.run(clientId, options)` | 启动独立命令，并在指定等待时间内返回状态和分离的输出流 |
| `exec.parallel(clientId, commands, {concurrency:4})` | 有界并行启动和观察命令，按输入顺序保留每项结果 |
| `exec.start/list/status/read/wait/cancel/forget` | 管理客户端后台命令；与服务端传输用的 `jobs.*` 分开 |
| `fs.readText/writeText/editText(clientId, options)` | 工作区内的原生 UTF-8 文件操作；成功写入产生本机 diff 记录 |
| `skills.list/view/read(clientId, options)` | 索引、摘要视图和原文读取；不会提交模型任务 |
| `agents.list/resolve/read(clientId, options)` | 查看指令来源、根到目标的优先级和原文 |
| `collab.join/list/update/send/receive/ack/leave(clientId, options)` | 会话内成员发现与消息交换；收件等待最多 1 秒，长等待使用公开 `collab` 工具 |
| `context.roots(clientId)` | 查看工作区与额外只读上下文根 |
| `await call(clientId, "ssmcp.client.reload", {})` | 重启已安装的客户端入口，随后用 `list` 获取新连接 ID |
| `outputs.read/stat(clientId, options)` | 按转储 ID 读取长输出或查看元数据 |
| `outputs.cleanup(clientId)` | 立即执行与自动清理相同的保留策略 |
| `print(value)`、`console.log(value)` | 记录本次代码模式的有界日志；也支持 `console.warn`、`console.error` |
| `return value` | 返回可序列化为 JSON 的值；未返回时为 `null` |

代码模式结果的 `structuredContent` 为 `{ "result": ..., "logs": [...] }`。其中 `result` 是显式返回值，`logs` 为字符串数组。`call` 保留原生结果中的 `content`、`structuredContent`、`isError` 等字段；原生工具可以正常返回 `isError: true`，调用方需检查这个字段。连接或转发错误会使对应调用失败，可用 JavaScript `try/catch` 处理。

`fs`、`skills`、`agents`、`collab`、`exec`、`reload` 和 `outputs` 的便捷原语返回解包后的结构化结果，原生工具返回错误时会抛出异常；`exec.parallel` 把各项失败保留为 `rejected/reason`。直接 `call` 则返回完整 MCP 结果。更多文件、显式上下文根、rsync、端口与托管同步任务示例见[工作区](workspaces.md)、[上下文](context.md)与[会话控制](session-control.md)。

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

### 按需查询目录

`tools.search` 在一次代码调用中读取所需的目录页，只把匹配结果返回给模型。`query` 是名称或描述的不区分大小写子串；默认返回 20 项名称、路由命名空间与短说明。可选的 `name_prefix` 只匹配实际注册名称的前缀，不区分大小写；同时提供 `query` 时，两项条件都必须满足。这样可查询指定工具族，避免其他工具的长说明因提及同一个词而进入结果。`namespace` 来自实际注册名称前缀，不是额外的权威 provider 字段。指定 `include_schemas:true` 时返回整个原生 descriptor，保留 input/output Schema、注解及扩展字段。[worker 实现](../src/code_mode.js)

```javascript
const id = "CLIENT_ID_FROM_LIST";
return await tools.search(id, {
  name_prefix: "ssmcp.exec.",
  limit: 10,
  include_schemas: true
});
```

`name_prefix` 最长 128 字节，只接受注册名称字符 `A–Z`、`a–z`、`0–9`、`_`、`.`、`-`。省略或传空字符串时，搜索结果及 v1 游标保持兼容；非空前缀会规范为小写并返回在结果的 `name_prefix` 中，使用绑定该前缀的 v2 游标。若要只匹配 `ssmcp.exec` 工具族，请保留末尾的 `.`；前缀不支持通配符。

`exec.run` 和 `exec.parallel` 是 JavaScript 便捷函数，不是目录中的原生工具。`exec.run` 组合调用 `ssmcp.exec.start`、`ssmcp.exec.wait` 和 `ssmcp.exec.read`；`wait_ms:0` 时跳过等待。查询参数时可使用上面的工具族前缀，不能将 `ssmcp.exec.run` 当作原生工具名称。

返回 `nextCursor` 表示还可继续。下一次传入该游标，并保持同一客户端、query、name_prefix、limit 与 schema 模式；这些搜索游标不能直接传给原生 `tools(id,cursor)`。页内续查会重新读取该页并核对 SHA-256，变更时明确要求重新搜索。底层仍保留原有目录快照语义，每页重新经过在线状态与授权校验，没有新增 helper 缓存。

一次搜索最多读取 32 页，`max_pages` 可设为 1–32；单条续查链最多经过 128 个不同页和 8,192 个工具。`limit` 为 1–50。结果中的 `complete`、`stopped_reason` 和 `stats` 区分已结束、条数上限、分页预算与输出预算；即使本页没有匹配项，仍应检查 `nextCursor`。

默认说明最多 240 个 Unicode 字符，`description_truncated` 会说明缩略。工具 JSON 合计预算为 96 KiB，完整搜索结果最多 128 KiB。单个原生 descriptor 超限时明确报错并提示使用原始 `tools` 分页，不会裁掉 Schema 或悄悄跳过工具。

检索减少的是模型上下文中的无关目录内容和手工分页往返，不减少首次扫描的底层请求数量。每页仍计入 `code_mode` 的 128 次原生调用预算；外层时间不足时应降低 `max_pages` 并续查。`tools.search` 位于 broker worker，需要服务端更新；客户端更新负责新增原生能力，二者分别验收。

## 4. 命令：并行执行、后台管理与等待

分步操作、完整输出读取和故障恢复见[独立命令指南](commands.md)。

普通命令通过 `exec` 执行，每项任务有独立 `job_id`、退出码和输出流。`command` 是 argv 数组，第一个元素是程序，其余元素是参数；不会隐式解析管道、重定向或变量展开。需要 Shell 语法时，显式运行该平台的 Shell，例如 Unix 的 `["sh", "-c", "printf hello"]` 或 Windows 的 `["cmd.exe", "/d", "/s", "/c", "echo hello"]`。默认目录是客户端启动目录；显式 `cwd` 必须是授权工作区内已有目录，不能用父目录穿越或符号链接把工作目录设置到根目录之外。`env` 是对继承环境的覆盖，审计日志不会展示环境变量映射。[命令实现](../src/backend/shell.rs) · [JavaScript helper](../src/code_mode.js)

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

`ssmcp.exec.start/list/status/wait/read/cancel/events` 是上述 helper 对应的原生原语，另提供 `ssmcp.exec.forget`。`start` 成功只确认启动；并行任务仍可能因共享 Cargo 构建目录等应用资源等待锁。

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

`concurrency` 限制本批实际运行中的命令数。批次共享一个 `wait_ms` 观察窗口，默认 1000 ms、上限 55000 ms；每个任务只等待一次并立即读取一次。已完成任务释放名额，仍在运行或状态未知的任务继续占用名额，其余项目明确返回 `not_started:true`。保留实际 `running`、原 `job_id` 和流游标。观察到期不会取消或重放命令；长任务使用 `exec.start`，再用公共 `await` 或 `exec.status/read` 观察已有任务。[helper 回归测试](../scripts/test_execution_helpers.mjs) · [多终端与后台输入](terminal-control.md)

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

PTY 最近 1 MiB 输出保存在内存中。单次读取最多 32 KiB，写入最多 16 KiB，`wait_ms` 最多 5000；`terminal.resize` 接受 `rows`、`cols`，范围均为 `1..=1000`。交互输入和前台控制字符遵循所选终端的行为。连接退出会清理其独立任务和提供者。真实终端上的 PTY 字节透传与日志配色无关，`NO_COLOR` 不删除光标控制或退格；JSONL 模式保留事件格式。

### 标题、重载与长输出

`--title` 随连接进入 `list`，供 agent 确认本轮任务。安装更新后可按[重载协议](#9-更新后的客户端重载)重启原入口；重载应答与新版本已经在线是两个状态，需要重新读取 `list` 确认。

长输出超过 32 KiB 或 500 行时按来源转储，返回 ID、路径、摘要、字节数和行数。Unix 使用 `/tmp/ssmcp-<uid>`，Windows 默认使用 `%LOCALAPPDATA%\ssmcp\output`，未设置 LOCALAPPDATA 时使用用户临时目录下的 `ssmcp/output`；返回路径属于目标客户端机器。默认保留 24 小时，已结束文件总量上限 512 MiB，单文件上限 256 MiB，活动文件不参加自动淘汰。具体的 `complete`、范围及截断语义见[长输出护卫](session-control.md#长输出自动转储)与[实现](../src/output.rs)。

## 5. 更新后的客户端重载

更新磁盘上的 ssmcp 后，在当前工具目录确认 `ssmcp.client.reload` 可用，再通过 `code_mode` 调用 `await call(clientId, "ssmcp.client.reload", {})`。检查 MCP `isError` 后从 `structuredContent` 读取原生重载应答；`accepted:true` 表示请求已经被接收，仍须再次调用公开 `list` 验证新连接。支持此协议的客户端会等待服务器确认应答已收取，随后停止本连接拥有的后台任务与转发、关闭原生适配器连接，并从原安装入口启动更新后的程序，保留参数、cwd、环境与允许列表。[重载握手](../src/client.rs) · [重启实现](../src/client/reload.rs)

重载时存在未结束的内部调用会返回 busy，应先等调用完成。已返回 `job_id` 的后台任务不属于在途请求，但会在重载时被终止；先完成或取消需要保留结果的任务。`reload` 不安装新版本，也不迁移进程和会话状态；它使用当前安装路径上的程序。初次从不支持该能力的旧版升级，需要在本机普通重启一次。

重连会分配新的 `client_id`，可结合 `title`、`name`、`cwd` 辨认目标，避免假定标签唯一。有效的 GitHub 身份授权可发现允许同一身份的新连接；登录码授权绑定旧连接，断线后须使用新码重新授权。结果不确定时先查 `list` 与本机日志，避免直接重复重载。[访问控制](../src/broker.rs) · [重载测试](../tests/two_tools.rs)

<a id="传输与资源边界"></a>

## 6. 事件、权限与运行边界

| 后端 | 事件或输出 | 待处理请求与回复 |
| --- | --- | --- |
| Shell | `terminal.read` | 控制字符由 `terminal.write` 显式发送 |
| 独立命令 | `exec.read`、`exec.events` | `exec.wait`、`exec.cancel`、`exec.forget` |

事件页接受 `cursor`、`max_events` 和 `wait_ms`，返回 `next_cursor`。内存缓存有界；`truncated:true` 表示游标早于缓存起点，应检查返回的 JSONL 归档引用，通过 `outputs.read` 读取已保存记录。每页最多 256 条事件，等待最多 5000 ms；归档完整性由 artifact 和 `archive_error` 判断。一次长轮询结束不证明原生任务完成。[事件日志](../src/backend/events.rs)

后台命令以 `exec.started/stdout/stderr/exited` 实时显示，stdout/stderr 有任务 ID 与独立顺序。`read/wait` 返回历史数据时，日志只显示响应元数据，避免把同一正文再打印一次。文件和技能是否实际发生操作，以相应的审计事件为准。

### 文件与技能审计

成功的原生文件读取产生 `ssmcp.file.read`。写入与编辑在保护提交的锁内采集前像，原子提交成功后产生 `ssmcp.file.write` 和 `ssmcp.file.diff`。快照区分 `missing`、`known`、`unknown`；不可读、二进制、敏感或过大的前像不会被当作空文件。可比较文本有实际增删行数，未知计数用 `null`；比较超过资源边界也明确返回不可用原因。[文件审计](../src/workspace/audit.rs)

小 Diff 完整放在事件的 `diff`，大 Diff 通过 `diff_artifact` 复用输出存储；`preview` 仅为终端展示。敏感路径与可识别秘密不输出 Diff 正文；进一步脱敏时设置 `diff_redacted` 和 `diff_complete`。`ssmcp.fs.events` 可查询事件。外部文件副作用标为 `file_audit:"unknown"`，审计不宣称覆盖 Harness 内部或其他进程的全部文件操作。

Context 的 `ssmcp.skill.discovered` 表示返回具体技能元数据，`ssmcp.skill.read` 表示读取正文，`ssmcp.skill.discovery.completed` 是摘要扫描完成状态。它们都不等于实际调用技能；只有原生接口明确报告调用才可展示 invoked。技能按 root/path 读取，见[上下文接口](context.md)。

终端摘要、详细日志、JSONL 与持久化审计使用脱敏副本，隐藏环境映射、凭据字段、敏感路径正文和可识别的秘密格式。原生调用返回及结构化结果保持各自类型；这不构成对任意未知秘密的完整识别保证。[脱敏](../src/audit.rs) · [渲染](../src/client/render.rs)

输出、任务、工具代号和审计游标属于各自连接；每次调用重新检查授权与提供者。文件存储在共享用户目录不意味着另一连接可读取它。断线与 reload 会清理连接拥有的进程和路由，旧连接 ID 不再可用。SSMCP peer 网关继续管理自身的进程。

服务端先验证设备账号，再按当前会话的控制名单及控制者的稳定账号 ID 检查每次调用。设备注册账号用于计算在线会话数，调用与回复流量计入实际控制账号。[解析](../src/broker/allow.rs) · [访问与计费](../src/broker.rs)

CLI 的邀请、join 和旧 SSH 登录码入口已退出当前设备接入流程。被控设备直接指定服务器账号；会话内 agent 的 `collab.join` 保留。[迁移用法](client-accounts.md)

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
