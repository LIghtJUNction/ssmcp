# RTK 命令输出压缩

通过 `exec.start` 或 `exec.run` 显式运行客户端 PATH 中的 RTK。RTK 处理命令输出，SSMCP 负责传递 argv、管理进程和读取结果。当前 SDK 已移除独立的 `rtk` 命名空间；原有任务、输出游标和错误处理都使用通用 exec 接口。[exec 契约](code-mode.md#exec)

## 查询实际版本

~~~js
return await exec.run(clientId, {
  command: ["rtk", "--version"],
  timeout_ms: 5000,
  wait_ms: 1000,
  max_bytes: 1024
});
~~~

检查 `running`、`exit_code` 和输出。`running:true` 表示探测尚未结束，保存 `job_id` 后继续观察。无法启动、授权拒绝、无效工作目录或连接故障仍返回实际错误；助手不把这些情况转换成猜测的安装状态。

## 执行并读取结果

`command` 包含可执行文件名和全部参数。例如 `["rtk","git","status"]` 执行 RTK 的 git status。支持哪些子命令和过滤格式，以该设备实际安装的 RTK 版本为准。

~~~js
const result = await exec.run(clientId, {
  command: ["rtk", "git", "log", "-8"],
  cwd: ".",
  timeout_ms: 30000,
  wait_ms: 1000,
  max_bytes: 16384
});
return {
  job_id: result.job_id,
  running: result.running,
  exit_code: result.exit_code,
  stdout: result.stdout,
  stderr: result.stderr,
  stdout_cursor: result.stdout_cursor,
  stderr_cursor: result.stderr_cursor,
  snapshot: result.snapshot
};
~~~

`wait_ms` 限制本次观察，默认 1000 ms；`timeout_ms` 决定进程执行期限。一次 run 最多等待一次，再立即读取一页输出。观察到期仍返回原 job ID 和真实状态，不取消进程。更长的工作可用 `exec.start` 启动，再通过外层 `await` 或后续读取继续观察。

参数逐项传给进程。空格、中文和 Shell 特殊字符保持字面值。SSMCP 不自动安装 RTK，也不会在 RTK 失败后再次执行未压缩的原命令。启动成功而观察失败时，保留错误上的 `job_id`，先查该任务。[错误和任务保留测试](../scripts/test_rtk_helpers.mjs)

## 后台任务与输入

~~~js
const job = await exec.start(clientId, {
  command: ["rtk", "cargo", "test"],
  cwd: ".",
  timeout_ms: 120000
});
return {job_id: job.job_id};
~~~

后续调用保留同一客户端与任务 ID，并使用上次返回的两个游标：

~~~js
return await exec.read(clientId, {
  job_id: jobId,
  stdout_cursor: savedStdoutCursor,
  stderr_cursor: savedStderrCursor,
  max_bytes: 16384
});
~~~

`exec.status` 读取状态，`exec.wait` 做有界观察，`exec.cancel` 请求停止进程树，`exec.forget` 释放已完成记录。code_mode 返回后任务仍可运行；任务的连接生命周期由客户端原生执行器管理。[任务管理](commands.md)

支持 pipe 过滤的 RTK 版本也可处理已收集的文本：

~~~js
const job = await exec.start(clientId, {
  command: ["rtk", "pipe", "--filter", "cargo-test"],
  stdin: "pipe",
  timeout_ms: 30000
});
const written = await exec.write(clientId, job.job_id, {
  data: capturedOutput,
  close_stdin: true
});
return {job_id: job.job_id, written};
~~~

检查写入结果的 `written`、`remaining_bytes` 和 `timed_out`。分块提交大文本，仅补写尚未写入的字节，并在最后一块关闭 stdin。

## 获取完整输出

保留 RTK 实际返回的 recall 或 tee 提示。如果该版本已经保存原文并给出恢复标识，可显式执行：

~~~js
return await exec.run(clientId, {
  command: ["rtk", "recall", returnedHash, "--full"],
  wait_ms: 1000,
  max_bytes: 32768
});
~~~

保存位置和原文可用性由 RTK 版本与配置决定。SSMCP 输出制品保留的是 RTK 进程实际输出的字节；读取后仍须核对完整性和截断状态。

[此前 RTK 1.2.0 验收记录](validation-live-context-rtk.md) 对应当时的源码与环境，不证明当前客户端已加载新的 helper。当前迁移使用已有 `ssmcp.exec.start/wait/read`；输入管道还需要 `ssmcp.exec.write`。先发现所选客户端的实际 schema，再执行。

官方参考：

- [RTK v1.2.0](https://github.com/rtk-ai/rtk/releases/tag/v1.2.0)
- [版本化 CLI 定义](https://github.com/rtk-ai/rtk/blob/v1.2.0/src/lib.rs)
- [命令执行与退出码处理](https://github.com/rtk-ai/rtk/blob/v1.2.0/src/core/runner.rs)
