# RTK 命令输出压缩

code_mode 的 rtk 原语调用客户端 PATH 中的 RTK。RTK 按命令类型压缩输出；ssmcp 负责参数传递、进程管理、超时和输出读取。服务端无需安装 RTK，实际执行命令的设备需要安装它。

## 接口

| 方法 | 行为 |
| --- | --- |
| rtk.status(clientId, {cwd, env}?) | 有界执行 rtk --version，返回 available、version 与执行状态 |
| rtk.run(clientId, options) | 启动 RTK，等待指定时长并返回输出与 job_id |
| rtk.start(clientId, options) | 立即启动 RTK，返回可继续管理的后台任务 |

command 是不带 rtk 前缀的 **RTK 参数数组**。例如 ["git", "status"] 执行 rtk git status，["cargo", "test"] 执行 rtk cargo test。支持的子命令、过滤格式与压缩效果由设备上的 RTK 版本决定。

~~~js
return await rtk.status(clientId);
~~~

available 为 true 表示进程成功返回 RTK 版本；false 表示无法启动或版本探测未成功；null 表示探测仍在运行。探测最长执行 5 秒，结果保留 job_id、退出码和输出。授权拒绝、连接故障和无效工作目录仍按原生错误返回。

## 执行并读取结果

~~~js
const result = await rtk.run(clientId, {
  command: ["git", "log", "-8"],
  cwd: ".",
  timeout_ms: 30000,
  wait_ms: 5000,
  max_bytes: 16384
});

return {
  job_id: result.job_id,
  running: result.running,
  exit_code: result.exit_code,
  stdout: result.stdout,
  stderr: result.stderr,
  snapshot: result.snapshot
};
~~~

run 支持 exec.run 的 command、cwd、env、stdin、timeout_ms、wait_ms 和 max_bytes。start 支持 command、cwd、env、stdin 与 timeout_ms。wait_ms 只控制这次等待；执行超时由 timeout_ms 控制。超时、取消、信号、非零退出码与输出制品信息都沿用 exec 的结果。

参数逐项传给进程，不拼接成 Shell 字符串。含空格、中文或 Shell 特殊字符的参数保持字面值。ssmcp 不自动安装 RTK，也不在 RTK 失败后再次执行原命令。

## 后台任务与输入

~~~js
const job = await rtk.start(clientId, {
  command: ["cargo", "test"],
  cwd: ".",
  timeout_ms: 120000
});
return {job_id: job.job_id};
~~~

后续调用使用同一个客户端 ID 与 job_id：

~~~js
return await exec.read(clientId, jobId, {
  stdout_cursor: 0,
  stderr_cursor: 0,
  max_bytes: 16384
});
~~~

exec.wait 等待完成，exec.status 读取状态，exec.cancel 停止进程树，exec.forget 释放已完成任务的记录。任务继承客户端的连接生命周期，code_mode 结束后继续运行，客户端断开或重载时停止。

支持 pipe 过滤的 RTK 版本可以压缩已经收集的文本：

~~~js
const job = await rtk.start(clientId, {
  command: ["pipe", "--filter", "cargo-test"],
  stdin: "pipe",
  timeout_ms: 30000
});
const written = await exec.write(clientId, job.job_id, {
  data: capturedOutput,
  close_stdin: true
});
return {job_id: job.job_id, written};
~~~

写入遵守 exec.write 的字节上限；检查 written、remaining_bytes 和 timed_out，分块提交大文本，并在最后一块关闭 stdin。

## 获取完整输出

ssmcp 保留 RTK 输出中的 recall 或 tee 提示。RTK 已保存原文并返回对应标识时，可以通过同一原语获取：

~~~js
return await rtk.run(clientId, {
  command: ["recall", returnedHash, "--full"],
  max_bytes: 32768
});
~~~

是否保存原文以及保存位置由 RTK 版本与配置决定。ssmcp 的输出制品保存 **RTK 进程实际输出的内容**；需要未压缩原文时，按 RTK 返回的恢复信息读取。

RTK v1.2.0 的 bypass（以及兼容别名 proxy）用于原样执行，调用这些子命令时不进行压缩。普通压缩命令使用 git、cargo 等受支持的 RTK 子命令。本接口不依赖 rtk rewrite 或额外的模型代理。

## 版本与验证

开发机验证使用 RTK 1.2.0。验收结果与命令见 [项目上下文与 RTK 验收记录](validation-live-context-rtk.md)。

实现只新增 code_mode helper，沿用已有原生执行接口。新服务端负责暴露 rtk 命名空间；客户端须支持 exec.start/wait/read，管道输入另需 exec.write。

官方参考：

- [RTK v1.2.0](https://github.com/rtk-ai/rtk/releases/tag/v1.2.0)
- [版本化 CLI 定义](https://github.com/rtk-ai/rtk/blob/v1.2.0/src/lib.rs)
- [命令执行与退出码处理](https://github.com/rtk-ai/rtk/blob/v1.2.0/src/core/runner.rs)
