# 在 Zuse 程序中组织动作与等待

SSMCP 已撤去内置浏览器批次、条件等待和后台页面控制 API。需要操作同一页面时，把观察、动作与条件判断写入独立 Zuse 程序，再通过通用 exec 运行。具体语言与操作参数以所选 CLI 的 `schema` 和 [Zuse 指南](https://github.com/LIghtJUNction/zuse#readme) 为准。

## 一次任务，两种时间限制

```javascript
return await exec.run(clientId, {
  command: [zusePath, "run", "automation.zuse"],
  cwd: projectDirectory,
  wait_ms: 1000,
  timeout_ms: 60000,
  max_bytes: 16384
});
```

`wait_ms` 限制这次调用等待结果的时间；任务仍在运行时，用返回的 `job_id` 继续读取。`timeout_ms` 限制命令生命周期。程序自身的页面等待与输入顺序由 Zuse 负责，SSMCP 不再提供原来 25 秒的 browser.act 批次合同或 tab 操作锁。[exec 参数](code-mode.md#exec)

## 顺序、取消与重试

同一页面的依赖操作放在同一进程中按顺序组织。只有相互独立的目标才适合并行执行；不要把一次 CLI 退出前的浏览器状态当作另一次进程的状态。

读取真实退出码、stdout、stderr 和产物，再判断哪一步完成。对已开始的操作，超时、取消或连接错误都不证明没有副作用。停止对应 owned exec job 后，重新观察目标，按实际状态继续，而不是整段重放。

SSMCP 不会因 CLI 失败而改走内置 CDP、桌面点击或 Playwright。目标选择和原生权限见 [Zuse 与浏览器](browser-desktop.md)，历史内置浏览器结果见 [原 PR #13 验证](validation-background-control.md)。
