# 客户端命令与独立任务

## 连接、账号与设备列表

控制者先在目标中继登录；被控设备明确指定允许的账号，设备本身可以不登录：

~~~sh
# 控制者注册或复用该服务器的登录。
ssmcp alice@relay.example

# 在被控设备的项目目录运行。
ssmcp --server https://relay.example \
  --allow alice@relay.example --name device-b

# 控制者另开终端，查看并操作获准设备。
ssmcp list --server https://relay.example
ssmcp -i --server https://relay.example --client SESSION_ID
~~~

| 命令 | 用途 |
| --- | --- |
| `ssmcp username@server` | 登录或注册账号，并连接自己的设备 |
| `ssmcp --server URL --allow USER@SERVER` | 明确授权控制者并连接被控设备 |
| `ssmcp whoami [--server URL]` | 确认所选服务器上的登录身份 |
| `ssmcp logout [--server URL] [--json]` | 清理该服务器的本机凭据，并报告远端撤销结果；离线也能退出 |
| `ssmcp list` / `ls` / `sessions list --remote` | 列出账号在所选服务器可控的在线设备 |
| `ssmcp -i` / `list -i` | 选择设备并进入远端终端，Ctrl-] 离开 |
| `ssmcp sessions list` / `attach NAME` | 管理当前设备保存的本机后台会话 |

用户名采用 3–32 个 ASCII 字符，首字符为字母、末字符为字母或数字，中间还可使用 `_` 或 `-`。名单中的用户名须已在目标服务器注册，并与本次连接的服务器匹配。凭据按服务器隔离；已登录设备省略名单时只允许自己，显式名单只授权其中的账号。[用户名](named-accounts.md) · [完整账号与设备流程](client-accounts.md)

看门狗默认开启，`--no-watchdog` 可禁用；显式停止后不会继续恢复。CLI 的 `invite` / `join` 与 MCP 的 `invite` 已移除，设备授权使用 `--allow 用户名@服务器`；会话内 agent 登记继续使用 `collab.join`。[首次连接](onboarding.md)

## 查询本地路径

`ssmcp paths` 枚举实际配置、凭据目录、工具仓库和本地状态的位置。它在连接与配置加载之前返回，不读取文件内容、不创建目录。可组合选择名称，按给定顺序输出；不指定名称时列出全部十项。

~~~sh
ssmcp paths --list
ssmcp paths config history --plain
ssmcp paths history config --json
~~~

默认每行输出 `名称<TAB>路径`，`--plain` 只输出路径，`--json` 输出名称到路径的对象。`--list` 只枚举名称与说明，不解析环境；`--help` 显示用法。[名称、环境优先级与组合示例](paths.md)

## 更新客户端

`ssmcp update` 将手动安装的客户端更新到官方 GitHub Release 中的最新稳定版。它是独立本地命令，不建立服务器会话，也不加载 harness。

~~~sh
ssmcp update --check
ssmcp update
~~~

`--check` 只读取发布元数据并显示是否有新版本，不下载安装包或修改安装文件。版本按主版本、次版本和修订号比较；已是最新版时直接结束，当前源码版本比发布版新时保留当前程序。

更新复用客户端随包携带的安装器：从官方 GitHub 下载 `SHA256SUMS`，按平台获取发布包，校验 SHA-256，并运行候选程序的 `--version` 确认版本。只有全部成功才替换当前可执行文件。发布包可按既有安装器规则回退至下载镜像，校验清单始终来自官方源。下载失败、校验失败、候选版本不符或目录不可写时返回错误，保留原程序。

Linux、macOS 使用系统 Shell、curl、tar 和校验工具；Windows 使用 Windows PowerShell。更新目标是正在执行的 `ssmcp` 或 `ssmcp.exe` 的安装目录；重命名的程序需用原安装方式更新。Cargo、Homebrew、Scoop、Windows 包管理器、Nix、Snap 和系统目录中的包管理器安装不会被原位覆盖，命令会提示使用对应更新方式；`--check` 仍可使用。不能识别所有第三方安装布局，使用自定义包管理器时应继续由该管理器更新。

更新不重启现有客户端或后台服务。它们继续执行已加载的程序，按需重启后使用新版本；Windows 若正在运行的旧文件暂时无法删除，安装器会保留带唯一名称的备份并说明位置。更新命令不会提升权限，也不会修改 Shell 或系统 PATH。

## 管理独立 Zuse CLI

| 本机命令 | 对应原生工具 | 作用 |
| --- | --- | --- |
| `ssmcp computer [version] [--json]` | `ssmcp.computer.version` | 实际执行所选 CLI 的 `--version`；默认子命令 |
| `ssmcp computer download [VERSION] [--json]` | `ssmcp.computer.download` | 下载当前平台官方稳定版；返回 `download_id`，不执行、不安装 |
| `ssmcp computer install DOWNLOAD_ID [--json]` | `ssmcp.computer.install` | 核验受管下载并安装到私有目录；返回 `installation_id` 和路径 |
| `ssmcp computer uninstall INSTALLATION_ID [--json]` | `ssmcp.computer.uninstall` | 仅删除身份、所有权和字节仍匹配的受管安装 |

不指定下载版本时选择最新稳定版；显式版本接受 `MAJOR.MINOR.PATCH` 或带 `v` 的同一格式。使用前一次结果中的真实 ID。`--json` 输出完整原生 MCP 结果；默认输出其结构化正文。安装不修改 PATH，也不覆盖或卸载外部 Zuse。[真实参数解析](../src/client/computer_control.rs)

实际桌面与浏览器程序直接交给所选 Zuse CLI，经下方 exec 接口运行。[完整管理边界与示例](computer-use.md)

## 远程任务池

只接收远程 agent 分配的任务时，运行 `ssmcp --worker N`。默认远程模式不启动本地模型或 harness，也不要求设置 `--agent`、`--input`。终端显示每个 worker 的空闲／工作／离线状态与可滚动日志，没有聊天输入框。

~~~sh
ssmcp --worker 2
ssmcp --worker 2 --daemon
~~~

主 Agent 通过 `ssmcp.worker.attach` 绑定自己的 worker 池，使用 `submit`、`tasks`、`result`、`cancel` 管理任务，并通过 `stop` 停止接单。管理权来自经过认证的会话线程；其他已授权执行者通过 `wait`／`claim` 领取任务并提交实际完成回执。忙满时进入有界队列；停止中的在途任务仍占槽，直到原执行者确认。后台模式自动禁用聊天输入。显式本地执行器和其 Linux 资源限额属于高级选项，见[Worker 模式、领取回执和资源边界](workers.md)。普通 `ssmcp` 继续提供聊天界面。

## 前台观察与终端输入

前台默认使用 `dark` 主题和 `chat` 布局，连续显示执行记录，底部输入框用于追加用户要求。最左侧是统计页，展示会话进程 ID、已绑定主 Agent 的会话线程 ID、OS PID 和实际计数。Enter 提交到队列；`/queue` 管理待交付项，`/sessions` 或 `/message` 显式选择收件人，`/audit` 打开源码审计。`/agents` 与旧的默认 Agent 页已移除。Tab 切换输入与内容焦点，滚轮或 PgUp/PgDn 直接浏览当前内容；`/filter 文本` 搜索日志，F9 切换类别。[客户端 UI 与队列原子接口](client-ui.md)

输入框要求通过支持协议 v7 的连接附加到下一次符合目标的 `code_mode` 响应，已交付不代表模型已阅读或执行。需要直接终端输入时，在另一个终端运行 `ssmcp -i`；自定义前端使用 `terminal.read/write/resize`。`ssmcp attach NAME` 用于观察本地后台会话，目前不转发 attach 输入框的要求队列；后台连接可通过 `ssmcp.ui.queue` 管理要求。独立命令继续使用下方的 `exec.*`。

## 查看 OpenAI 调用标识和打开 ChatGPT

在当前已保存的对话中输入 `/openai`，可以查看最近观察到的 `openai/session` 和 `openai/subject`。只看其中一项时用 `/openai-session` 或 `/openai-subject`。面板会标明来源、字段缺失状态和最后观察时间。字段来自当前 MCP 工具请求的 `_meta`，并且只由支持此能力的中继和客户端在已验证的调用线程上记录。客户端、请求或线程没有提供这些条件时，出现“未观察到”属正常情况。[OpenAI Plugins 参考](https://developers.openai.com/plugins/reference)

这两个值是匿名关联标识，无法从中解析出 ChatGPT 地址。要返回网页继续对话，请从浏览器复制会话地址，再在对应的 SSMCP 对话里执行以下命令。地址须使用 HTTPS，主机须为 `chatgpt.com` 或 `chat.openai.com`，路径为 `/c/ID` 或 `/g/ID/c/ID`。

| 命令 | 用途 |
| --- | --- |
| `/chatgpt set <URL>` | 把浏览器复制的链接保存到当前已保存的本地对话 |
| `/chatgpt open` 或 `/chatgpt` | 请求本机浏览器打开已保存的链接，然后可在网页手动发送「继续」 |
| `/chatgpt copy` | 把已保存的链接复制到本机剪贴板 |
| `/chatgpt clear` | 只删除该对话的链接，保留已观察到的标识 |
| `/plugin-docs` | 在本机浏览器打开官方 Plugins 参考 |

切换或恢复历史时只切换对应的本地数据。浏览器和剪贴板操作只在显式命令发出时执行，启动浏览器程序后仍由浏览器处理加载和登录。

## 独立命令接口

`exec` 在选定客户端启动独立进程，并用任务 ID 保存状态。每个任务有自己的工作目录、退出码和 stdout/stderr。它适合构建、测试、开发服务器等可由 argv 表达的命令。需要持续交互、终端控制或修改同一 Shell 状态时，使用 `terminal.*`。

这些助手在服务端 `code_mode` 中提供，最终调用客户端的 `ssmcp.exec.*` 工具。先通过 `list` 选择客户端，并用 `list({client_id})` 确认新版能力已上线。客户端和服务端都需要升级；仅更新其中一端不会增加另一端缺失的接口。[实现](../src/backend/shell.rs) · [JavaScript 助手](../src/code_mode.js)

## 运行一次命令

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");
return await exec.run(client.id, {
  command: ["git", "status", "--short"],
  wait_ms: 5000,
  max_bytes: 16384
});
```

`run` 先启动，再等待，最后读取一次输出。返回 `job_id`、`running`、`exit_code`、`stdout`、`stderr`、两个字节游标和最新 `snapshot`。`status/read/wait` 返回 `running: false` 时，该任务占用的执行槽已释放；观察器收尾可能继续，这一状态不表示所有 UI 资源都已释放。`exit_code: 0` 表示进程正常成功退出。启动或参数错误会拒绝 JavaScript Promise，进程自身非零退出仍是一个有效结果，应由调用方检查退出码。

`command` 是 argv 数组，不经过隐式 Shell 展开。`["echo", "$HOME"]` 不会把 `$HOME` 替换为目录。需要管道、重定向或 Shell 内建命令时，显式选择本机 Shell，例如 Unix 的 `["sh", "-lc", "printf '%s\\n' hello | wc -l"]`，或 Windows 的 `["cmd.exe", "/d", "/s", "/c", "echo hello"]`。

`cwd` 可指定现有绝对目录，省略时使用客户端启动目录；`env` 只覆盖所列环境变量，其他值继承客户端环境。每个任务独立，某次命令中的 `cd` 不会改变其他命令的工作目录。

## 启动后台任务，再继续等待

第一次调用启动任务并保存返回的 `job_id`：

```javascript
return await exec.start(clientId, {
  command: ["cargo", "test", "--locked"],
  timeout_ms: 600000
});
```

随后在新的 `code_mode` 调用中使用同一客户端 ID 和任务 ID：

```javascript
return await exec.wait(clientId, jobId, { wait_ms: 5000 });
```

`wait_ms` 控制这次请求等待多久，取值 0–60000 毫秒，默认 5000。到期仍在运行时，返回 `running: true` 和 `wait_timed_out: true`，任务继续执行。不要为了继续等待重新启动命令；再次调用 `wait` 或 `read`。

`timeout_ms` 是进程的执行期限，取值 0–900000 毫秒。默认 0 表示没有执行期限；非零期限到达后停止受管理的进程树，并记录 `timed_out`。服务端 `code_mode` 的 `timeout_ms` 是另一层编排预算：编排结束不代表此前启动的本机进程结束。为单次长等待预留足够的编排预算，或者使用多次短等待。

| 管理接口 | 含义 |
| --- | --- |
| `exec.list(clientId)` | 列出该连接保留的任务及容量限制 |
| `exec.status(clientId, jobId)` | 读取一个任务的最新状态 |
| `exec.wait(clientId, jobId, {wait_ms})` | 等待结束或等待期限到达 |
| `exec.read(clientId, jobId, options)` | 按字节游标读取两个输出流，可同时短等待 |
| `exec.write(clientId, jobId, options)` | 向以 `stdin:"pipe"` 启动的任务写入数据或发送 EOF |
| `exec.cancel(clientId, jobId)` | 请求停止任务及其受管理子进程；重复取消已结束任务可安全返回 |
| `exec.forget(clientId, jobId)` | 删除已完成的任务记录；运行中任务必须先取消 |
| `exec.events(clientId, options)` | 读取任务事件；参数以发现的 schema 为准 |

以上六个按任务 ID 操作的接口（`status/wait/read/write/cancel/forget`）也接受与原生 schema 相同的对象参数：

```javascript
return await exec.read(clientId, {
  job_id: jobId,
  stdout_cursor: savedStdoutCursor,
  stderr_cursor: savedStderrCursor,
  max_bytes: 16384
});
```

对象形式需要含非空字符串 `job_id`，所有选项写在同一个对象中；同时提供第三参数会在调用前报错。原有 `exec.read(clientId, jobId, options)` 继续可用，第三参数中的 `job_id` 仍由第二参数确定的任务 ID 覆盖。参数对象不会被修改，其余字段继续由客户端原生 schema 验证；原生失败保留原错误且不自动重试。对象形式需要更新服务端的 JavaScript 助手，已有原生命令接口的客户端可以直接使用。

任务在当前 `code_mode` 完成后继续存在，但属于这条客户端连接。退出、断开清理或 `reload` 会停止其后台工作；任务 ID 不能迁移到重连后的新客户端。

## 并行执行

```javascript
return await exec.parallel(clientId, [
  { command: ["git", "status", "--short"] },
  { command: ["git", "log", "-5", "--oneline"] },
  { command: ["git", "diff", "--stat"] }
], { concurrency: 3 });
```

结果按输入顺序排列，每项为 `{status:"fulfilled", value:...}` 或 `{status:"rejected", reason:...}`。一个启动失败不会抹掉其他项的结果。fulfilled 表示助手正常返回，是否构建成功仍需检查 `value.running` 和 `value.exit_code`。

一次最多提交 32 项，`concurrency` 为 1–8，默认 4，限制这次调用实际运行中的命令数。单次等待到期时，任务继续占用名额，直到明确结束才启动下一项。客户端另有 32 个同时运行任务的硬上限，容量不足时在创建进程前报错。

命令选项类型、`wait_ms` 或 `max_bytes` 在本地校验失败时，该项返回 `rejected` 和 `not_started:true`，同一 worker 继续执行后续合法项；这类错误不会发送原生命令。

进入原生启动阶段后，如果启动回包丢失或后续状态读取失败，该 worker 会保留名额，并返回已经取得的 `job_id`（若有），避免在完成状态未知时继续启动任务。未被其他 worker 执行的项目返回 `not_started:true`；先用 `exec.status/read/list` 检查任务状态，再决定是否重试。

`parallel` 默认每个流只读取 2048 字节，控制汇总大小；单项可用 `max_bytes` 覆盖。长输出按需分页，避免把多份完整构建日志放进一次响应。

## RTK 的异步错误处理

`rtk.start` 和 `rtk.run` 始终返回 Promise，本地参数错误也可由 `.catch(...)` 或混合的 `Promise.allSettled(...)` 处理。例如，下面的第二项用空命令演示本地参数错误：

```javascript
const results = await Promise.allSettled([
  rtk.run(clientId, { command: ["git", "status", "--short"] }),
  rtk.start(clientId, { command: [] }),
  exec.run(clientId, { command: ["git", "diff", "--stat"] })
]);
return results.map(result => result.status === "fulfilled"
  ? result
  : { status: result.status, error: result.reason.message });
```

每项分别返回 fulfilled 或 rejected；已经启动的合法项保留独立结果，不因其他项的参数错误而丢失。助手不自动重试命令。

## 读取完整输出

`read` 默认从偏移 0 开始，每个流最多返回 16384 字节，可设为 1–32768。两个流的游标分别推进：

```javascript
return await exec.read(clientId, jobId, {
  stdout_cursor: savedStdoutCursor,
  stderr_cursor: savedStderrCursor,
  max_bytes: 16384,
  wait_ms: 1000
});
```

下次沿用返回的 `stdout_cursor`、`stderr_cursor`。`stdout_base64` / `stderr_base64` 保留精确字节，文本字段便于阅读；UTF-8 字符可能横跨字节页，要求精确还原时拼接 base64 解码后的字节再解码文本。未来游标会报错，避免悄悄跳过尚未产生的内容。

运行中的每个流在内存保留最近 64 KiB。此时游标落后于保留范围，`stdout_truncated` 或 `stderr_truncated` 为 true，并从可读尾部继续返回。流成功结束且捕获完整后会释放重复尾部；后续 `exec.read` 按游标读取完整 capture，小输出可保留一份内存捕获，较大输出从归档读取。归档缺失或缩短会返回错误，捕获失败仍保留尾部和错误信息。输出超过 32 KiB 或 500 行后保存转储文件；从 `snapshot.stdout.artifact` 或 `snapshot.stderr.artifact` 取得 ID，也可用 `outputs.read` 读取已捕获的完整前缀：

```javascript
return await outputs.read(clientId, {
  id: artifactId,
  offset: savedOffset,
  max_bytes: 16384
});
```

`outputs.read` 和 `outputs.stat` 均拒绝实际长度小于 manifest 记录值的归档；`complete: true` 时，实际长度必须与记录值相等。`outputs.stat` 还会核对完整归档的原始 SHA-256；`outputs.read` 仅校验长度与分页，并返回本页内容的哈希。活动归档可以继续增长，`complete: false` 的部分捕获在满足长度要求时仍可读取。Unix 上，归档路径若被替换成 FIFO，会作为非普通文件及时报错，无需等待管道另一端。

继续沿用 `next_offset`。活动文件的 EOF 只表示当前已经读到文件末尾，仍可能产生后续内容。结束后结合任务状态与转储的 `complete`、`error` 检查捕获完整性。写入失败、单文件超过 256 MiB、收尾读取异常或超时都会明确记录，不应把不完整输出当作完整日志。[输出保留和清理规则](session-control.md)

最多保留 128 条任务记录，达到上限后只淘汰最早完成的记录；每次启动都会返回实际被淘汰的 ID（若有）。`forget` 只移除任务记录，转储文件继续遵守输出目录的 24 小时与总量清理策略。对于需要长期保留的日志，应在清理前导出。

## 重载与故障恢复

安装新版二进制后，在 `code_mode` 中调用 `return await reload(clientId)`。客户端确认重载请求、等待服务端确认，再停止后台任务、原生工具和转发，重新启动原入口。调用 `list` 找到新的客户端 ID，再用一个无副作用的状态请求确认连接可用。[重载协议](session-control.md)

命令启动成功后若等待或读取失败，助手错误信息会携带已启动的任务 ID。先用 `exec.status`、`exec.read` 或 `exec.list` 检查已有工作，再决定是否重试。`exec.*` 管理命令进程，`jobs.*` 管理 rsync 同步；两类任务使用不同接口。

