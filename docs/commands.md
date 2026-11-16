# 独立命令、并行执行与后台任务

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

`run` 先启动，再等待，最后读取一次输出。返回 `job_id`、`running`、`exit_code`、`stdout`、`stderr`、两个字节游标和最新 `snapshot`。`running: false` 才说明任务收尾结束；`exit_code: 0` 表示进程正常成功退出。启动或参数错误会拒绝 JavaScript Promise，进程自身非零退出仍是一个有效结果，应由调用方检查退出码。

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
| `exec.cancel(clientId, jobId)` | 请求停止任务及其受管理子进程；重复取消已结束任务可安全返回 |
| `exec.forget(clientId, jobId)` | 删除已完成的任务记录；运行中任务必须先取消 |
| `exec.events(clientId, options)` | 读取任务事件；参数以发现的 schema 为准 |

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

一次最多提交 32 项，`concurrency` 为 1–8，默认 4。它限制同时进行的“启动 → 等待 → 读取”流程。长命令等待到期后仍会在后台继续，而该流程可以处理下一项，因此后台运行总数可能高于此并行度。客户端另有 32 个同时运行任务的硬上限；容量不足时在创建进程前报错。需要严格限制长期任务数时，按实际完成状态分批调度。

`parallel` 默认每个流只读取 2048 字节，控制汇总大小；单项可用 `max_bytes` 覆盖。长输出按需分页，避免把多份完整构建日志放进一次响应。

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

每个流在内存保留最近 64 KiB。游标落后于保留范围时，`stdout_truncated` 或 `stderr_truncated` 为 true，并从可读尾部继续返回。输出超过 32 KiB 或 500 行后同时保存转储文件；从 `snapshot.stdout.artifact` 或 `snapshot.stderr.artifact` 取得 ID，用 `outputs.read` 读取已捕获的完整前缀：

```javascript
return await outputs.read(clientId, {
  id: artifactId,
  offset: savedOffset,
  max_bytes: 16384
});
```

继续沿用 `next_offset`。活动文件的 EOF 只表示当前已经读到文件末尾，仍可能产生后续内容。结束后结合任务状态与转储的 `complete`、`error` 检查捕获完整性。写入失败、单文件超过 256 MiB、收尾读取异常或超时都会明确记录，不应把不完整输出当作完整日志。[输出保留和清理规则](session-control.md)

最多保留 128 条任务记录，达到上限后只淘汰最早完成的记录；每次启动都会返回实际被淘汰的 ID（若有）。`forget` 只移除任务记录，转储文件继续遵守输出目录的 24 小时与总量清理策略。对于需要长期保留的日志，应在清理前导出。

## 重载与故障恢复

安装新版二进制后，在 `code_mode` 中调用 `return await reload(clientId)`。客户端确认重载请求、等待服务端确认，再停止后台任务、原生工具和转发，重新启动原入口。调用 `list` 找到新的客户端 ID，再用一个无副作用的状态请求确认连接可用。[重载协议](session-control.md)

命令启动成功后若等待或读取失败，助手错误信息会携带已启动的任务 ID。先用 `exec.status`、`exec.read` 或 `exec.list` 检查已有工作，再决定是否重试。`exec.*` 管理命令进程，`jobs.*` 管理 rsync 同步；两类任务使用不同接口。
