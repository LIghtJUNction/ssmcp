# 客户端会话、重连与输出管理

需要持久系统服务、原子新建/重启/关闭，以及 `ssmcp attach [NAME]`，请先阅读[常驻服务与 attach](session-service.md)。本页保留旧 native 后台模式、重连及输出转储的完整说明。

本页说明连接标题、客户端重载、本机后台会话及长输出读取。公开 MCP 工具仍是 `list` 和 `code_mode`；`reload`、`outputs` 是 `code_mode` 内的 JavaScript 原语。`ssmcp sessions list` 枚举本地保存会话；登录后使用 `ssmcp list` 查看账号可控的在线会话，使用 `ssmcp -i` 进入其终端，详见[客户端账号与临时接入](client-accounts.md)。

## 用标题说明本次任务

`--title` 设置本次会话的任务标题。对话界面顶部显示 `ssmcp / activity / 项目名 / 会话标题`：项目名取自客户端真实启动目录的末级目录名，未设置标题时省略最后一段。切换视图时，`activity` 随之更新；长标题按终端宽度裁剪，不挤占正文。

标题也进入客户端元数据，agent 调用 `list` 就能看到它。标题支持中文和空格，最多 512 个 UTF-8 字节；空值、纯空白和控制字符会被拒绝。

```sh
ssmcp --ssh --name dev --title "完善 ssmcp 客户端重载和输出转储" \
  --allow octocat@github my-ssmcp
```

也可以写成 `--title="完善客户端重载"`。将 `octocat` 替换为获准访问的 GitHub 用户名，并使用已配置的 SSH 目标。

| 字段 | 含义 | 选择客户端时的作用 |
| --- | --- | --- |
| `id` | 服务端为这条连接分配的唯一 ID | 调用工具时必须使用它；重连后改变 |
| `name` | `--name` 设置的显示名称 | 帮助辨认机器或工作区，允许重名 |
| `title` | `--title` 设置的本次任务说明 | 帮助理解任务，允许重名 |
| `cwd` | 客户端启动目录 | 帮助核对所选项目 |

标题保持原文，并出现在连接信息、`list`、指定客户端详情和 `code_mode` 的 `clients` 数组里。它是操作者提供的描述，不会自动执行命令，也不改变 `--allow` 的访问规则。未设置标题时，元数据省略 `title` 字段。[参数与元数据实现](../src/client.rs) · [字段验证](../src/wire.rs)

在 `code_mode` 中先检查匹配数量，再用唯一 ID 操作目标：

```javascript
const matches = clients.filter(c =>
  c.name === "dev" && c.title === "完善 ssmcp 客户端重载和输出转储"
);
if (matches.length !== 1) {
  throw new Error("请通过 list 核对会话的 ID、标题和 cwd");
}
return matches[0];
```

带标题的握手需要支持 `title` 字段的服务端。旧服务端会拒绝未知的握手字段，因此首次使用前要升级服务端；未设置标题时保持原有元数据形状，新服务端也能接受旧客户端。

## 用 reload 重新加载客户端

`await reload(clientId)` 请求目标客户端从原启动入口重新执行，适用于已经安装新版二进制后的重载。它保留启动参数、启动目录和环境，因此 `--name`、`--title`、`--allow`、SSH 配置和其他参数会继续生效；它本身不会下载、编译或安装更新。

将新版安装到该客户端启动时使用的入口路径后，在 `code_mode` 执行：

```javascript
return await reload(clientId);
```

原生工具调用形式为 `await call(clientId, "ssmcp.client.reload", {})`。两种形式都经过现有客户端授权检查，不接受额外参数。[JavaScript 原语](../src/code_mode.js) · [重载实现](../src/client/reload.rs)

重载先确认请求，再关闭旧客户端的资源并重新执行程序。客户端检查启动入口仍可执行；有其他进行中的工具请求时，重载返回 busy。接受请求后，它返回 `accepted: true` 和 `status: "restarting"`，等待服务端回执后再开始重启。连接拥有的后台命令、同步和端口转发会随旧连接停止，原注册表及请求状态失效，不能依靠 `reload` 保留这些运行状态。客户端创建的原生子进程也会关闭；通过私有 socket 或 loopback 地址连接的既有 Pi/OpenCode 服务仍由操作者管理，ssmcp 仅断开桥接，不终止外部服务。

Unix 使用 `exec` 替换当前进程映像。Windows 启动替代客户端，并让已关闭连接资源的旧进程等待它结束，以保留前台控制台和管道的使用关系；多次重载可能保留多个等待进程，但只有替代客户端维持新的连接。

`accepted: true` 表示重载已接受，仍需确认新连接出现。旧 `clientId` 随连接关闭而失效；随后再次调用公开 `list`，通过标题、名字和 `cwd` 核对新连接，使用新的 ID。每次 `code_mode` 执行的 `clients` 是当次快照，旧快照不会自动更新。

| 授权方式 | 重载后的行为 |
| --- | --- |
| `--allow ...@github` | 新连接继续声明相同允许列表。已有 GitHub OAuth 授权仍有效且身份匹配时，可继续发现和调用新连接 |
| 一次性登录码 | 旧连接的授权失效。重连生成新登录码，需要重新完成 OAuth 登录 |

重载保留安装入口，包括入口末端的符号链接，因此更新该链接指向的新版本也能生效。它不会继续执行已被替换的旧程序映像。使用 `reload` 前需同时升级客户端和服务端，使两端都支持重载回执；旧客户端首次升级仍需手动重新启动。

后台会话使用创建时保存的可执行文件副本。对这种会话，`reload` 和 `sessions restart` 仍运行已保存版本；更新普通安装入口不会自动更换它。后台版本升级步骤见下节。

## 后台运行与延时重连

新建的前台和后台会话默认启用 watchdog，在连接失败或断开后重试。使用 `--no-watchdog` 禁用，原有 `--watchdog` 保留为显式启用的兼容参数；二者不能同时使用。`--list-tools` 始终只发现一次工具后退出，不会进入重连循环。[参数与连接循环](../src/client.rs)

`--reconnect-delay SECONDS` 设置重试间隔，默认 **5 秒**，允许 **1–3600 秒**的整数，不需要另外指定 `--watchdog`，但不能与 `--no-watchdog` 同用。默认重连不会把前台进程变成后台服务；`--daemon` 才会把客户端交给当前用户的会话管理器，命令返回后继续运行。[本机会话管理](../src/session.rs)

旧保存会话继续使用 manifest 中记录的 watchdog 策略，包括原来未启用的情况；升级不会把已保存的禁用策略改成启用。新保存的禁用配置会明确记录 `--no-watchdog`。

在项目目录中创建一个具名后台会话：

```sh
ssmcp --ssh --daemon --reconnect-delay 5 \
  --name workstation --title "修复项目并执行测试" \
  --allow octocat@github my-ssmcp
ssmcp sessions status workstation --json
```

将 GitHub 用户与 SSH 目标替换为已有配置。后台模式沿用 SSH 公钥、`--allow` 和原生工具权限；可用工具仍需真实发现。启动成功只表示原生管理器接受了启动请求，尚不能证明 SSH 或 Broker 已连通。先检查 `connection_state`，再通过 MCP `list` 核对在线客户端。未登录客户端账号且没有 `--allow` 的 SSH 连接仍使用登录码 OAuth；只有确实需要登录码时，在本机明确执行 `ssmcp sessions auth workstation`。登录码不会进入普通状态或生命周期日志；auth 命令只返回当前已连接会话的可用码，正常断线处理会清除本地 auth 记录。

### 查看和管理保存的会话

这些命令都支持 `--json`，只管理当前用户所选状态目录下的会话。保存的 `NAME` 是本地管理标识；每次 SSH 连接的 `client_id` 仍由 Broker 新建。后台名称须为 1–128 个 UTF-8 字节，不得以 `-` 开头，不得含控制字符、路径分隔符或首尾空白；名称内部可以有空格，使用时加引号。同名创建会明确拒绝覆盖。

| 本地命令 | 行为 |
| --- | --- |
| `ssmcp sessions list --json` | 枚举保存的会话及当前观测状态，包括已经停止的会话 |
| `ssmcp sessions status NAME --json` | 分别查看原生管理器状态与 Broker 连接状态 |
| `ssmcp sessions start NAME` | 使用保存的配置和可执行文件启动已停止的会话 |
| `ssmcp sessions stop NAME` | 请求停止，抑制 watchdog，并确认原生管理器已经停止；不会因延时到期自行恢复 |
| `ssmcp sessions restart NAME` | 先停止再启动同一保存配置，不改变版本或工作目录 |
| `ssmcp sessions logs NAME --lines 100 --json` | 查询有界日志；`--lines` 默认 50，允许 1–1000，返回值明确来源与覆盖范围 |
| `ssmcp sessions auth NAME --json` | 仅在当前连接存在可用登录码时显式读取私有认证信息；不要把结果贴入普通日志 |
| `ssmcp sessions remove NAME` | 先停止会话，再删除它的保存配置、二进制副本及本地生命周期记录 |

`manager_state`、`manager_active`、`manager_pid` 描述系统管理器观测；`connection_state` 描述 `connected`、`disconnected` 或 `unknown`。管理器 active、进程存在或 CLI 返回 0 都不等于已连上 Broker。`phase` 记录最近的 `starting`、`connected`、`reconnecting`、`disconnected` 或 `stopped` 阶段，配合 `attempt`、`retry_after_secs` 和观测时间阅读。只有当前进程的有效 connected 观测才会附带 `connection`；首次尚无观测时可能显示 unknown。

连接断开会清理它拥有的命令、PTY、转发与工具注册；watchdog 随后建立新连接并重新发现能力。它不会保存旧任务，也不会把旧连接输出的读取权限迁移给新连接。GitHub OAuth 能否继续使用仍取决于有效身份授权及新连接的 `--allow`；登录码模式需要使用新连接签发的码重新登录。人工停止优先使用 `sessions stop`；前台 watchdog 可用 Ctrl-C 停止。直接强制杀死后台进程可能触发原生崩溃恢复。

### 平台、状态与版本

本节表格描述旧 native 后台模式：没有运行常驻管理服务时，`--daemon` 仍使用已存在的用户管理器，不自动启用系统启动、登录触发或 Linux linger。已安装常驻 `ssmcp` 服务时，新会话由它托管并按保存的运行意图恢复；两种管理类型在状态中明确区分。持久定义与平台 scope 见[平台服务说明](client-service-platforms.md)。

| 平台 | 原生后台管理 | watchdog 启用时的进程崩溃恢复（新会话默认） |
| --- | --- | --- |
| Linux | transient `systemd --user` service | `Restart=on-failure`，按所设延时配置 `RestartSec` |
| macOS | 当前用户的 launchd agent | 失败退出时恢复，按所设延时配置 `ThrottleInterval` |
| Windows | 当前已登录用户的计划任务，无管理员或 SYSTEM 身份 | 原生间隔为设定值与 60 秒中的较大值，最多 255 次失败重启 |

表中描述代码配置，平台现场验收状态另见[验收材料](validation-0.6.0.md)。原生进程崩溃恢复与 SSH 重连分开：Windows 的 60 秒下限和 255 次配置不限制存活客户端内部按 `--reconnect-delay` 进行的连接重试。Linux `sessions logs` 优先查询对应 unit 的实际 journal；查询不可用或无记录时明确退回生命周期日志。macOS、Windows 目前仅提供生命周期记录，不能据此宣称已观察完整原生工具输出。[日志来源](logging.md#后台会话日志)

默认状态目录为 Linux 的 `$XDG_STATE_HOME/ssmcp/sessions`（未设置时为 `~/.local/state/ssmcp/sessions`）、macOS 的 `~/Library/Application Support/ssmcp/sessions`，或 Windows 的 `%LOCALAPPDATA%\ssmcp\sessions`。`SSMCP_SESSION_HOME` 可指定私有绝对路径，创建和管理时须使用同一值。Unix 目录和记录文件默认分别为 0700、0600，程序拒绝不符合私有权限或符号链接约束的状态路径。该目录与工作区、连接输出目录分别管理。

会话保存启动目录、参数、当前可执行文件副本及 SHA-256，并只记录所需的 `PATH`、`SSH_AUTH_SOCK` 环境引用；HOME 由用户管理器提供。不要假定创建命令所在 Shell 的全部环境变量会被复制。保留 SSH 配置、密钥、工作区及实际工具路径，不能因为二进制已经复制就删除这些依赖。

升级后台会话时，先记录原来的工作目录、SSH 参数、标题和允许列表，处理需要保留的结果，然后 `sessions remove NAME`；安装新版本后，从原工作目录用同一组参数重新执行 `--daemon`。`start`、`restart`、MCP `reload` 都不会替换保存的二进制。删除会话不会无条件清空连接输出区，已结束输出仍按保留与配额策略清理。

## 长输出自动转储

长输出护卫在输出超过 **32 KiB** 或 **500 行** 时，将输出保存到客户端的私有输出目录，返回元数据供 agent 按需读取。Unix 使用 `/tmp/ssmcp-<uid>/output-<id>.log`，`<uid>` 是运行客户端的本机用户 ID；Windows 默认使用 `%LOCALAPPDATA%\ssmcp\output`，未设置 LOCALAPPDATA 时使用用户临时目录下的 `ssmcp/output`，并限制目录访问权限。路径属于该客户端机器，不属于 MCP 服务器或调用方沙箱；以返回的 artifact 元数据为准。

升级不会修改或删除旧的 `/tmp/ssmcp/<uid>` 目录。已确认的热重载可凭原有的私有、一次性凭据继续读取旧位置的输出；新连接使用新的用户私有目录。账户、会话账本以及显式配置的持久存储路径不受此临时目录变更影响。

流式捕获会保留触发阈值之前的前缀，并继续追加后续字节。短输出保持原来的内联形式；长输出返回文件 ID、路径、SHA-256、字节数、行数、完成状态和过期时间，避免把全部日志塞进一次工具返回。[输出存储实现](../src/output.rs)

不同输出来源有不同的文件范围：

| 来源 | 转储内容与范围 |
| --- | --- |
| 独立 Shell 命令 | 每个任务的 stdout、stderr 分别捕获；状态中的 artifact 标识各自输出 |
| 交互式 PTY | 整个终端会话的合并输出，包含提示符和终端控制字节 |
| 超大的原生 MCP / Harness 结果 | ssmcp 实际收到的完整 MCP 结果 JSON，`format` 为 `mcp_tool_result_json` |
| OpenCode HTTP / 事件流 | HTTP 正文通过 `body_artifact` 与 `body_format` 保留类型；SSE 原始流单独捕获 |
| 审计事件与文件 Diff | 脱敏后的 JSONL 事件归档，以及可查询的完整文本 Diff |

PTY 的 `terminal.read` 遇到长输出时返回 `spilled: true`、`artifact` 和 `range`。`range.start`、`range.end` 标记本次请求覆盖的字节区间，`next_cursor` 用于下一次终端读取；需要正文时根据 `artifact.id` 读取转储文件。原生工具自己已经截断或未提供的输出不能通过护卫恢复。[PTY 捕获实现](../src/pty.rs) · [原生结果护卫](../src/runtime.rs)

`exec.read` 返回两条流的独立游标，完整输出引用位于 `snapshot.stdout.artifact` 和 `snapshot.stderr.artifact`。滚动内存尾部被淘汰不代表已保存文件也被丢弃；使用 artifact 分页恢复早期字节。后台输出实时进入终端，查询历史 `read/wait` 不会再作为新输出打印。`start` 成功只代表已启动，最终命令结果仍看状态与退出码。[独立命令](commands.md)

小的结构化返回保留原生类型。OpenCode 大 HTTP 响应保留状态与内容类型，并用 `body_format` 标识 JSON、文本或二进制；不要把所有 artifact 都当作纯文本。SSE 解析错误和原始捕获完整性分别记录。审计 JSONL 使用脱敏副本，MCP 原生返回不会为日志展示而被改写。[适配器](../src/backend/opencode.rs) · [事件归档](../src/backend/events.rs)

### 如何理解元数据

`artifact` 描述一个时间点上已经保存的原始字节。文件仍在增长时，后续读取到的大小和摘要可能变化；使用 `outputs.stat` 可以重新取得快照。

| 字段 | 含义 |
| --- | --- |
| `id`、`path` | 转储文件的标识与客户端本机绝对路径 |
| `scope` | 捕获来源，例如 `terminal_session` 或 `native_tool_result_json` |
| `bytes` | 当前快照已保存的字节数 |
| `sha256`、`sha256_scope` | 原始文件前 `bytes` 字节的 SHA-256；`first_bytes_snapshot` 表明摘要对应这个快照 |
| `lines` | 按换行符计数；末尾没有换行的非空记录也算一行 |
| `observed_bytes` | 捕获器已经收到的总字节数；发生容量限制时可能大于 `bytes` |
| `complete` | 捕获已经结束且没有丢失错误时为 `true`；运行中和不完整文件为 `false` |
| `error` | 存储失败、超限或提前终止的原因；出现时不能把文件当作完整输出 |
| `expires_at_unix` | 根据最近写入或完成时间计算的过期时间，单位为 Unix 秒 |
| `retention_secs`、`max_bytes` | 保留时间和单文件容量上限 |

`complete` 表示捕获完整性，命令成功与否仍由退出码、超时或取消状态决定。保存完整的失败日志可以是 `complete: true`；正在运行的命令则还没有最终捕获结果。

### 按需读取正文

`outputs.read` 按字节偏移读取一小段文件，默认读取 16 KiB，每次最多 32 KiB。后续调用继续使用返回的 `next_offset`，不需要把全部文件重新返回给 agent。

```javascript
return await outputs.read(clientId, {
  id: artifactId,
  offset: 0,
  max_bytes: 16384
});
```

返回值包括 `text`、`data_base64`、`bytes`、`next_offset`、`eof` 和该段的 SHA-256。`text` 用于阅读；原始二进制或被字节边界切开的 UTF-8 字符以 `data_base64` 为准。此处 `sha256_scope` 为 `returned_chunk`，摘要只覆盖返回的片段。

`eof: true` 只表示已经到达本次读取所见的文件末尾；活动文件还可能增长。要检查整个文件的当前摘要、行数和完成状态，使用：

```javascript
return await outputs.stat(clientId, { id: artifactId });
```

这些操作也能通过 `ssmcp.output.read` 和 `ssmcp.output.stat` 原生工具调用。它们按转储 ID 访问，不接受任意文件路径；工作区文件仍使用 `fs` 原语。每条连接共享一个 OutputStore，Shell、PTY、Harness、MCP 与审计都使用同一配额和捕获机制。读取时校验 artifact 的连接归属；同一 OS 用户的另一客户端也不能读取该连接的输出、任务或审计记录。

## 自动清理与容量

转储区自动删除过期文件，并按时间淘汰超出容量的已结束文件。客户端初始化输出存储、创建转储文件时会清理一次；客户端运行期间每 60 秒再清理一次。

| 策略 | 默认值与行为 |
| --- | --- |
| 保留时间 | 24 小时；按最近写入或完成时间计算 |
| 已结束文件总量 | 512 MiB；超出后先删除最旧文件 |
| 单个文件上限 | 256 MiB；超出后保留已捕获前缀，返回不完整状态及错误 |
| 活动文件 | 写入期间持有文件锁，自动清理跳过它们 |
| 其他文件 | 只清理符合 ssmcp 命名及元数据格式的文件，不处理无关文件 |

已经创建转储文件后，单文件超限或磁盘写入失败会保留已写入的前缀，并标记 `complete: false` 和错误。若文件创建本身失败，结果会明确报错，可能返回 `artifact: null`、`output_error`，或原生工具错误，没有可读的转储文件。总量限制针对已结束文件；仍在写入的文件不会为了满足总量限制被删除，所以活动输出可能使实际占用暂时高于 512 MiB。

需要立即执行同一清理策略时，在 `code_mode` 调用：

```javascript
return await outputs.cleanup(clientId);
```

原生调用形式为 `await call(clientId, "ssmcp.output.cleanup", {})`。返回值包含删除文件数、删除字节数、保留字节数和活动文件数；该调用不会无条件清空目录。客户端全部退出期间没有清理任务运行，下次启动会继续清理。重载不会立即删除尚在保留期内的已结束文件，但磁盘保留不授予新连接读取旧连接输出的权限。旧 client ID 失效，替代连接具有新的输出归属；在结束或重载前读取需要保留的结果。受控的跨连接持久任务与输出迁移尚未提供。

`--max-inline-output` 控制的是本机日志展示行数，与此处返回给 agent 的 32 KiB / 500 行护卫阈值独立。完整终端展示设置见[日志与主题配置](logging.md)。
