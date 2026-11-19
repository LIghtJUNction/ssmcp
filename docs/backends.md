# 统一客户端、Harness 与代码模式

ssmcp 对外提供 `list`、`code_mode` 和 `collab` 三个 MCP 工具。`list` 找到当前授权可访问的在线客户端并读取能力目录；`code_mode` 执行 JavaScript，通过客户端连接编排原生工具；`collab` 在服务端处理同一会话的 agent 成员与消息。每条新连接始终显示为 `shell`，同时提供交互式终端和独立命令任务，并可注册多个已安装 harness 的原生能力。[HTTP 入口](../src/http.rs) · [后端分派](../src/backend.rs)

<a id="启动参数"></a>

## 1. 启动客户端

先在 Linux、macOS 或 Windows 进入需要工作的本机目录，再启动一个客户端。它始终提供交互式 shell，并自动发现 `PATH` 中可用的 Codex、Claude Code、Pi、OpenCode 原生接口。把 `octocat@github` 换成允许访问这条连接的 GitHub 用户名；安装方式和平台范围见[版本与安装](releases.md)。

```sh
ssmcp --server https://relay.example --allow alice@relay.example --name dev --title "检查当前项目并修复构建"
```

| 参数 | 作用 |
| --- | --- |
| `--name NAME` | 客户端显示名称；默认包含主机名和进程 ID |
| `--title TITLE` | 供 agent 识别任务的会话标题；最多 512 个 UTF-8 字节 |
| `--allow USER@SERVER` | 指定当前中继上已注册的控制账号；可重复或以逗号分隔，最多 32 个不同账号 |
| `--shell /absolute/path` | 选择交互式 shell；Unix 默认使用绝对路径的 `SHELL` 或 `/bin/sh`，Windows 使用绝对路径的 `COMSPEC` 或系统 `cmd.exe` |
| `--context-root PATH` | 增加只读 skills／AGENTS 文档根目录；最多 8 个，不扩展普通文件写入范围 |
| `--list-tools` | 发现并打印本机工具目录后退出；不连接 SSH、不接受 SSH 目标或 `--allow` |
| `-F FILE`、`-i FILE`、`-p PORT` | OpenSSH 配置、私钥文件和端口 |

`--name` 和 `--title` 是显示信息，允许重名。服务端为每条连接分配独立 `id`，所有调用用这个 ID 分派；重连后需重新读取 `list`。`--name` 最多 128 个 UTF-8 字节，不能全为空白或含控制字符；同名连接需按 ID 区分。`cwd` 记录启动目录，不随交互式 shell 的 `cd` 改变。[客户端参数](../src/client.rs) · [元数据约束](../src/wire.rs)

### 从旧的后端选择参数迁移

删除旧命令中的 `-t TYPE`、`--type TYPE`、`--backend PROGRAM` 和尾随的后端参数，保留 SSH、身份、名称和日志选项。客户端已移除这些选择入口；例如旧的 `ssmcp -t codex --name dev my-server` 改为 `ssmcp --ssh --name dev my-server`。把需要的原生 CLI 安装到 `PATH`，原生账户、插件、MCP 与模型设置继续在各 CLI 中维护。

```sh
# 不连接 SSH，先检查这台电脑实际发现了什么。
ssmcp --list-tools

# 同一连接提供 shell 和成功初始化的原生适配器。
ssmcp --server https://relay.example --name dev --title "继续项目开发" --allow alice@relay.example
```

| 来源 | 本机原生入口或实现 | 可调用范围 |
| --- | --- | --- |
| ssmcp | 交互式终端、独立后台进程、文件、上下文原语 | 不依赖安装 coding harness；文件读写与命令执行直接在本机完成 |
| Codex | `codex app-server --listen stdio://`，并须支持 `app-server generate-json-schema --experimental` | App Server 方法；成功发现时注册 `codex.mcp.*`，通过原生 MCP 调用接口转发 |
| Claude Code | `claude mcp serve` | 该入口实际返回的 MCP 工具目录，统一加 `claude.` 前缀 |
| Playwright MCP | 已安装的 `playwright-mcp` 或显式配置的本机入口，固定 `--extension` | 原生 MCP 握手和分页枚举后注册 `playwright.*`；扩展连接和批准仍由原生接口处理 |
| Pi | 显式连接已初始化的本机私有 Unix RPC socket | `pi.rpc` 与事件、请求、回复四项桥接操作；内部模型、插件和 MCP 工具 Schema 不可枚举 |
| OpenCode | 操作者已启动的本机 HTTP 服务；显式配置 loopback 地址 | 从 `/api/info` 握手和 `/openapi.json` 发现操作；发现阶段不代为启动服务 |

发现成功才会加入对应工具；缺少 CLI、版本不兼容或初始化失败会记录本机诊断，shell 连接仍可使用。调用 `ssmcp.harnesses.list` 可查看当前注册数量，完整名称、参数和权限以 `list({client_id})` 为准。安装一个 harness 不等于它的每个内部功能都提供可直接调用的接口。[自动发现与分派](../src/backend.rs)

各提供者在后台独立探测和刷新；Shell 的连接与目录不等待可选 Harness。版本、握手或目录读取超时只使该提供者不可用，断线会取消仍挂起的发现。时间、页数和工具数量有界，具体限制由对应适配器定义。[启动回归测试](../tests/client_startup.rs)

发现只使用已有程序，不安装运行时、依赖或 Harness，也不提交模型任务。mise、asdf、Volta 等 shim 需要解析到已安装入口；无法确认安全启动的版本保留为 unavailable，并给出原因。关闭网络下载不能证明不会从缓存安装依赖。参见 [OpenCode 的启动限制](#8-opencode从-openapi-发现操作) 和 [原生程序解析](../src/program.rs)。

Playwright 的显式入口通过 `SSMCP_PLAYWRIGHT_MCP=/absolute/path/to/already-installed/cli.js` 配置；JavaScript 入口由已验证的真实 Node.js 执行。发现先检查版本，再启动 `--extension` 模式并执行 MCP 握手和工具枚举。不会使用 `npx`、`npm`、`corepack` 或 shim 去下载安装，缺失时只保留 unavailable 诊断。已有环境传给原生程序，但 ssmcp 不读取扩展 token 存储、不生成 token，也不代替 Chrome 中的确认。`ssmcp.playwright.events/requests/respond` 只用于已发现的原生事件和待处理请求，参数以实时 Schema 为准。[现有浏览器流程](browser-existing-session.md) · [适配器](../src/backend/playwright.rs)

后续显式调用是否使用模型由原生方法决定。例如 Codex 的 turn、Pi 的 prompt 控制请求，以及 Claude 目录中可能存在的 Agent 工具会进入其任务流程；直接 MCP 工具也可能在内部使用模型。开发与验收默认使用协议 fixture、状态查询和已确认无模型的工具。普通读写文件与 Shell 命令可直接使用 `fs` 和 `exec`。

`ssmcpd` 和代码模式的隔离执行环境运行在 Linux。服务端通过系统 Node.js、bubblewrap 与 util-linux 的 `prlimit` 执行编排代码；客户端只需本机 OpenSSH 与所选 shell。服务端的安装条件见[部署说明](deployment.md#系统代码运行环境)。原生 rsync 端点目前也仅在 Linux 客户端提供；macOS／Windows 可使用文件和 loopback 网络原语。[rsync 平台分支](../src/rsync.rs)

被控端使用 `--allow 用户名@中继服务器` 明确列出控制账号，本机不需要登录；省略名单时要求已保存有效登录并仅授权自己。控制端始终使用真实账号认证与连接 ACL。交互终端显示连接摘要，重定向 stdout 或 `--raw` 使用机器可读输出。[账号与授权](client-accounts.md)

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

MCP 协议本身的 `tools/list` 返回 `list`、`code_mode`、`collab`，没有后续页。`collab` 的 `action` 选择加入、成员发现、发送、接收或确认等会话内 agent 操作；它不代替控制端登录。`terminal.write`、`claude.Read` 等原生名称供代码模式内部调用，完整参数以当前原生目录为准。

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
| `await reload(clientId)` | 重启已安装的客户端入口，随后用 `list` 获取新连接 ID |
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

`concurrency` 限制本批实际运行中的命令任务数。初次等待到期后，worker 继续观察同一任务，收到 `running:false` 后才启动下一条；内部等待最长 60 秒。观察失败会保留可取得的 `job_id`，并停止该 worker 继续启动任务；最终未启动的项目带有 `not_started:true`。内部调用仍计入 128 次原生调用和外层时限，持续服务使用 `exec.start`；外层超时后通过 `exec.list/status/read` 接管已启动任务。[helper 回归测试](../scripts/test_execution_helpers.mjs) · [多终端与后台输入](terminal-control.md)

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

长输出超过 32 KiB 或 500 行时按来源转储，返回 ID、路径、摘要、字节数和行数。Unix 使用 `/tmp/ssmcp/<uid>`，Windows 默认使用 `%LOCALAPPDATA%\ssmcp\output`，未设置 LOCALAPPDATA 时使用用户临时目录下的 `ssmcp/output`；返回路径属于目标客户端机器。默认保留 24 小时，已结束文件总量上限 512 MiB，单文件上限 256 MiB，活动文件不参加自动淘汰。具体的 `complete`、范围及截断语义见[长输出护卫](session-control.md#长输出自动转储)与[实现](../src/output.rs)。

## 5. Codex：App Server 与动态 MCP 工具

Codex 适配器使用安装版本生成的 JSON Schema 注册 App Server 方法，例如原生 `thread/start` 对应路由 `codex.thread_start`。通用 `codex.rpc` 保留原生 method 和 params；它不能重复初始化连接。工具的可用范围取决于本机版本、配置和实际握手。[Codex 适配器](../src/backend/codex.rs)

内部 MCP 工具通过原生 `mcpServerStatus/list` 完整分页发现，再由 `mcpServer/tool/call` 调用。注册名称采用 `codex.mcp.<server>.<tool>`，组件会转义，过长组件使用摘要。原生 Tool 对象只替换路由名称；`inputSchema`、可选 `outputSchema`、描述、annotations、结果和错误保留。缺少工具 Schema 的接口标为不可枚举，不能从“服务器存在”推断其全部工具可用。

~~~javascript
const c = clients.find(c => c.id === "CLIENT_ID_FROM_LIST");
if (!c) throw new Error("Select an online client from list");
return await tools(c.id);
~~~

跟随返回的 `nextCursor` 读取全部目录。选择其中实际出现的完整名称，按原生 Schema 调用 `call(c.id, toolName, arguments)`；不要硬编码某个 MCP Server、工具或参数。验收需同时保存实际枚举与实际工具调用证据，仅列出服务器名称不足以证明转发成功。

只有完整分页成功才发布 MCP 快照。后台定期刷新，动态 MCP 调用前再次检查；发现失败、取消或原生连接失效时清除旧 MCP 路由。通用 App Server 控制方法与动态工具目录分别维护可用范围。

`ssmcp.codex.events` 读取事件，`ssmcp.codex.requests` 读取待处理审批、用户输入和动态工具请求，`ssmcp.codex.schema` 查看原生响应 Schema。需要回复时，用原始请求 ID 和对应的 `result` 或 `error` 调用 `ssmcp.codex.respond`。ssmcp 不自动选择审批答案，不改变 Codex 的原生沙箱和账户设置。模型 turn 只在获得任务授权后显式提交，发现和本项目回归测试不提交 turn。

## 6. Claude Code：发现原生 MCP 工具

Claude Code 适配器连接 `claude mcp serve`，重新读取它的完整分页目录，并给每个原生名称加上 `claude.` 前缀。原生 schema、描述、annotations 和结果字段保留；工具是否存在由本机版本与配置决定。[Claude Code 适配实现](../src/backend/cc.rs)

当发现目录提供 `claude.Read` 且参数包含 `file_path` 时，可以读取启动目录中的 README：

```js
const c = clients.find(c => c.name === "dev");
if (!c) throw new Error("Claude Code client is offline");
return await call(c.id, "claude.Read", { file_path: c.cwd + "/README.md" });
```

如果目录提供 `claude.Agent`，可按其实际 `inputSchema` 把任务交给本机 Agent。原生服务真正发来的请求可通过 `ssmcp.claude.requests` 读取，再用 `ssmcp.claude.respond` 的 `{id, result}` 或 `{id, error}` 回复。直接调用原生 MCP 工具时，不能假定它会出现交互式 Claude 终端的确认弹窗。

Claude 的前台 Bash 输出通常随调用结果返回。桥记录原生进度事件；当成功的后台 Bash 结果包含 `backgroundOutputPath` 时，桥才增量跟随这个已知路径，将输出写入本机终端和 `ssmcp.claude.events`。没有该字段时不会猜测输出文件；文件暂时不增长也不能证明命令已结束。最多同时跟随 32 个输出文件。

## 7. Pi：有限的原生 RPC 桥接

Pi 适配器的支持范围是控制协议，成功握手只注册四项：`pi.rpc`、`ssmcp.pi.events`、`ssmcp.pi.requests`、`ssmcp.pi.respond`。ssmcp 不把已知控制命令表伪装为发现到的模型工具目录。模型、扩展和内部 MCP 工具没有可枚举的原生 Schema 时保持不可枚举状态。[Pi 适配器](../src/backend/pi.rs)

在当前连接实际注册 `pi.rpc` 后，可以按它的 Schema 查询状态：

~~~javascript
const c = clients.find(c => c.id === "CLIENT_ID_FROM_LIST");
if (!c) throw new Error("Select an online client from list");
return await call(c.id, "pi.rpc", {type: "get_state", arguments: {}});
~~~

`get_state` 用于协议握手，发现不会发送 prompt。扩展对话通过 `ssmcp.pi.requests` 读取，`ssmcp.pi.respond` 保留原始 ID；实际字段和互斥条件以目录 Schema 为准。收到回复确认只表示交给了 Pi，不证明任务完成。

自动发现不启动新的 Pi 进程。操作者需事先初始化原生 JSONL RPC 会话，并通过 `SSMCP_PI_RPC_SOCKET` 显式提供本机私有 Unix socket 入口。ssmcp 核对已安装程序与版本；socket 路径必须为绝对路径，socket 和直接父目录不能是符号链接，必须由当前 UID 拥有且没有 group/other 权限。连接后再次核对 peer UID，并在 5 秒内完成原生 `get_state` 握手。只有 `success: true`、`command: "get_state"` 且 `data` 为对象的响应才被接受。未配置安全的既有入口时保持 unavailable；保留已安装与不可连接的诊断，不禁用用户扩展，也不替代其原生审批。ssmcp 不模拟终端交互来创建这一接口。[连接与发现](../src/backend.rs)

Pi 1.5.0 的原生包解析器在 `PI_OFFLINE=1` 时跳过缺失包安装，但扩展代码仍可执行。RPC 模式在处理第一条 `get_state` 前先绑定扩展，发出 `session_start` 并运行资源发现钩子；仅配置离线模式不能证明启动无安装或模型副作用。[包解析](https://github.com/earendil-works/pi/blob/v1.5.0/packages/coding-agent/src/core/package-manager.ts) · [RPC 启动顺序](https://github.com/earendil-works/pi/blob/v1.5.0/packages/coding-agent/src/modes/rpc/rpc-mode.ts) · [扩展绑定](https://github.com/earendil-works/pi/blob/v1.5.0/packages/coding-agent/src/core/agent-session.ts)

既有会话由操作者管理。断线或 reload 关闭 ssmcp 的 RPC 连接与本地请求状态，不杀死外部 Pi 服务；每条连接使用新的 128 位随机请求前缀，避免旧响应与重连后的请求重号。上述四项是适配器的协议范围，实际是否已注册仍以当前目录和握手结果为准。

## 8. OpenCode：从 OpenAPI 发现操作

OpenCode 适配器从原生 OpenAPI 文档发现 `opencode.<operationId>`。参数按 `path`、`query`、`headers`、`body` 分组，关联 Schema 与完整文档可通过 `opencode.schema` 查询；`opencode.api` 保留原生操作调用入口。[OpenCode 适配器](../src/backend/opencode.rs)

自动发现不会启动新的 OpenCode 服务。操作者需先按原生方式启动服务，再用 `SSMCP_OPENCODE_URL` 指定 `http://127.0.0.1:端口/`，并通过 `OPENCODE_PASSWORD` 提供非空、最多 4096 字节的原生密码；Basic 用户名固定为 `opencode`。端口必须显式指定，不接受其他主机、userinfo、非根路径、query 或 fragment，也不使用环境代理或跟随重定向。没有已配置端点时，提供者报告 unavailable。发现阶段只查询 `/api/info` 与 `/openapi.json`，要求原生信息为 v2 主版本并包含正数 PID；不创建会话、不提交 prompt，也不订阅 SSE。首次明确调用原生操作或 `opencode.events` 时才连接 `/api/event`。后续调用仍遵循原生权限审批；断线和 reload 只关闭本桥接连接。真实连接与版本兼容性必须在部署环境分别验证。

这一限制来自 OpenCode v2.3.0 的启动路径：服务会启动调度器；调度步骤会初始化会话 location，插件解析可进一步安装缺失包，到期任务也可能唤起模型。关闭更新、模型列表抓取或 npm 网络访问不能证明这些行为已经停止。固定版本的源码依据见 [服务启动](https://github.com/anomalyco/opencode/blob/v2.3.0/packages/server/src/process.ts)、[调度器](https://github.com/anomalyco/opencode/blob/v2.3.0/packages/core/src/schedule.ts)、[插件管理](https://github.com/anomalyco/opencode/blob/v2.3.0/packages/core/src/plugin/supervisor.ts)、[插件安装](https://github.com/anomalyco/opencode/blob/v2.3.0/packages/core/src/plugin/module.ts)。

注册成功后可读取原生文档，具体操作仍以实时目录为准：

~~~javascript
const c = clients.find(c => c.id === "CLIENT_ID_FROM_LIST");
if (!c) throw new Error("Select an online client from list");
return await call(c.id, "opencode.schema", {document: true});
~~~

HTTP 响应保留状态、内容类型和原生数据。短 JSON 仍为原生对象；长正文返回 `body_format`、`body_artifact`、预览及 `complete`。SSE 与 WebSocket 使用 `stream_id`，通过 `opencode.events` 读取、`opencode.stream_send` 写入 WebSocket、`opencode.stream_close` 关闭。SSE 解析达到边界时记录 decode_error，原始流仍交给同一 OutputStore 捕获。适配器隐藏自身的 transport 凭据，其他二进制字节保持保真。

## 9. 更新后的客户端重载

更新磁盘上的 ssmcp 后，在当前工具目录确认 `ssmcp.client.reload` 可用，再通过 `code_mode` 调用 `await reload(clientId)`。helper 返回原生重载应答；`accepted:true` 表示请求已经被接收，仍须再次调用公开 `list` 验证新连接。支持此协议的客户端会等待服务器确认应答已收取，随后停止本连接拥有的后台任务与转发、关闭原生适配器连接，并从原安装入口启动更新后的程序，保留参数、cwd、环境与允许列表。[重载握手](../src/client.rs) · [重启实现](../src/client/reload.rs)

重载时存在未结束的内部调用会返回 busy，应先等调用完成。已返回 `job_id` 的后台任务不属于在途请求，但会在重载时被终止；先完成或取消需要保留结果的任务。`reload` 不安装新版本，也不迁移进程和会话状态；它使用当前安装路径上的程序。初次从不支持该能力的旧版升级，需要在本机普通重启一次。

重连会分配新的 `client_id`，可结合 `title`、`name`、`cwd` 辨认目标，避免假定标签唯一。有效的 GitHub 身份授权可发现允许同一身份的新连接；登录码授权绑定旧连接，断线后须使用新码重新授权。结果不确定时先查 `list` 与本机日志，避免直接重复重载。[访问控制](../src/broker.rs) · [重载测试](../tests/client_reload.rs)

<a id="传输与资源边界"></a>

## 10. 事件、权限与运行边界

| 后端 | 事件或输出 | 待处理请求与回复 |
| --- | --- | --- |
| Shell | `terminal.read` | 控制字符由 `terminal.write` 显式发送 |
| 独立命令 | `exec.read`、`exec.events` | `exec.wait`、`exec.cancel`、`exec.forget` |
| Codex | `ssmcp.codex.events` | `ssmcp.codex.requests`、`ssmcp.codex.respond`、`ssmcp.codex.schema` |
| Claude Code | `ssmcp.claude.events` | `ssmcp.claude.requests`、`ssmcp.claude.respond` |
| Pi | `ssmcp.pi.events` | `ssmcp.pi.requests`、`ssmcp.pi.respond` |
| OpenCode | `opencode.events` | 发现到的原生 permission 操作 |

事件页接受 `cursor`、`max_events` 和 `wait_ms`，返回 `next_cursor`。内存缓存有界；`truncated:true` 表示游标早于缓存起点，应检查返回的 JSONL 归档引用，通过 `outputs.read` 读取已保存记录。每页最多 256 条事件，等待最多 5000 ms；归档完整性由 artifact 和 `archive_error` 判断。一次长轮询结束不证明原生任务完成。[事件日志](../src/backend/events.rs)

后台命令以 `exec.started/stdout/stderr/exited` 实时显示，stdout/stderr 有任务 ID 与独立顺序。`read/wait` 返回历史数据时，日志只显示响应元数据，避免把同一正文再打印一次。Harness 调用的开始、完成、耗时和原生审批状态由 `ssmcp.harness.tool.started/completed` 记录；不能据工具名称推断文件已写入或技能已调用。

### 文件与技能审计

成功的原生文件读取产生 `ssmcp.file.read`。写入与编辑在保护提交的锁内采集前像，原子提交成功后产生 `ssmcp.file.write` 和 `ssmcp.file.diff`。快照区分 `missing`、`known`、`unknown`；不可读、二进制、敏感或过大的前像不会被当作空文件。可比较文本有实际增删行数，未知计数用 `null`；比较超过资源边界也明确返回不可用原因。[文件审计](../src/workspace/audit.rs)

小 Diff 完整放在事件的 `diff`，大 Diff 通过 `diff_artifact` 复用输出存储；`preview` 仅为终端展示。敏感路径与可识别秘密不输出 Diff 正文；进一步脱敏时设置 `diff_redacted` 和 `diff_complete`。`ssmcp.fs.events` 可查询事件。外部文件副作用标为 `file_audit:"unknown"`，审计不宣称覆盖 Harness 内部或其他进程的全部文件操作。

Context 的 `ssmcp.skill.discovered` 表示返回具体技能元数据，`ssmcp.skill.read` 表示读取正文，`ssmcp.skill.discovery.completed` 是摘要扫描完成状态。它们都不等于实际调用技能；只有原生接口明确报告调用才可展示 invoked。技能按 root/path 读取，见[上下文接口](context.md)。

终端摘要、详细日志、JSONL 与持久化审计使用脱敏副本，隐藏环境映射、凭据字段、敏感路径正文和可识别的秘密格式。原生调用返回及结构化结果保持各自类型；这不构成对任意未知秘密的完整识别保证。[脱敏](../src/audit.rs) · [渲染](../src/client/render.rs)

输出、任务、工具代号和审计游标属于各自连接；每次调用重新检查授权与提供者。文件存储在共享用户目录不意味着另一连接可读取它。断线与 reload 会清理连接拥有的进程和路由，旧连接 ID 不再可用。Pi/OpenCode 的既有外部服务不属于连接拥有的进程；ssmcp 关闭与它们的桥接，不终止这些服务。

注册连接时，服务器把 `--allow` 中当前中继的门户用户名解析为稳定账号 ID；控制请求按已验证身份和目标 ACL 检查。注册者仅是来源记录，匿名连接没有注册者；调用与结果流量计入实际控制账号。[解析](../src/broker/allow.rs) · [访问与计费](../src/broker.rs)

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
