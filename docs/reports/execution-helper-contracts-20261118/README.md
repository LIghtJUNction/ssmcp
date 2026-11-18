# ssmcp 执行接口迭代记录

日期：2026-11-18（Asia/Taipei）  
交付：[PR #29](https://github.com/LIghtJUNction/ssmcp/pull/29)  
已验证源码：`c49f0498554325937d9b47a8dae294c3b640ad57`

## 完成的改动

### 本地参数错误不再占住并行名额

`exec.parallel` 通过工作名额限制同时运行的命令数量。此前，一项命令在本地选项校验阶段失败，也会退出一个 worker；并发数为 1 时，后续合法命令会全部被跳过。

本轮将本地校验与原生启动分开处理。命令选项类型、`wait_ms` 或 `max_bytes` 校验失败时，该项返回 `rejected` 和 `not_started:true`，同一 worker 继续处理后续合法项。进入原生启动后，若启动回包丢失或观察失败，命令可能仍在运行，继续保留并发名额，并返回已经取得的任务 ID，供后续查询。

实现与回归测试：[503edad](https://github.com/LIghtJUNction/ssmcp/commit/503edad86c57d576c006983d0decdd1e670ec59d)。

### 六个任务管理接口支持对象参数

`status`、`wait`、`read`、`write`、`cancel`、`forget` 可以使用与原生工具相同的对象参数。例如：

```javascript
return await exec.read(clientId, {
  job_id: jobId,
  stdout_cursor: savedStdoutCursor,
  stderr_cursor: savedStderrCursor,
  max_bytes: 16384
});
```

原有位置参数形式继续可用：

```javascript
return await exec.read(clientId, jobId, { max_bytes: 16384 });
```

对象形式需要非空字符串 `job_id`，全部选项放在同一对象中；同时提供第三参数会提前报错。位置形式仍由第二参数确定任务 ID，第三参数的 `job_id` 无法改换目标。参数对象保持不变，其余字段继续交由客户端原生 schema 验证；原生错误原样传播，不自动重试写入、取消等操作。

对象形式由新版服务端 JavaScript 助手转换为既有原生参数，接入时需要更新服务端助手。详细用法见[命令接口](https://github.com/LIghtJUNction/ssmcp/blob/c49f0498554325937d9b47a8dae294c3b640ad57/docs/commands.md)。

实现、发现提示与验证入口：[c49f049](https://github.com/LIghtJUNction/ssmcp/commit/c49f0498554325937d9b47a8dae294c3b640ad57)。

### 统一自动检查入口

新增 `make test-code-mode`，统一执行执行任务、RTK、工具搜索、CUA 和客户端能力五个 JavaScript 测试文件。`make test` 和既有 Linux CI 复用同一目标。CI 保持仓库现有的手动触发及复用工作流方式。

## 验证结果

| 检查 | 结果 | 覆盖范围 |
| --- | --- | --- |
| JavaScript 助手回归 | 50 通过，0 失败 | 包括本地错误后继续调度、丢失启动回包后保留名额、新旧参数形式等价 |
| 无效参数矩阵 | 102 种参数组合，0 次原生请求 | 包含于上述 50 项 JavaScript 测试；覆盖无效或歧义参数 |
| Rust code mode 回归 | 24 通过，0 失败，0 忽略 | 包含两个新增 Node → Rust Host 边界测试和真实系统隔离验证 |
| Rust 格式检查 | 通过 | `cargo fmt --all -- --check` |
| 验证入口检查 | 通过 | `make -n test-code-mode` 指向同一组五个测试文件 |
| 独立代码审查 | 无阻塞问题 | 并发名额、任务目标、参数兼容、错误传播和文档一致性 |
| 严格 Clippy | 通过，退出码 0 | `cargo clippy --locked --all-targets -- -D warnings` |

完整已完成测试输出：[JavaScript](https://github.com/LIghtJUNction/ssmcp/blob/8bce82a536ca7de774044d3675d83b45a49d8c6d/docs/reports/execution-helper-contracts-20261118/javascript-tests.log) · [Rust](https://github.com/LIghtJUNction/ssmcp/blob/8bce82a536ca7de774044d3675d83b45a49d8c6d/docs/reports/execution-helper-contracts-20261118/rust-tests.log)。日志中的工作区绝对路径已替换为占位名称。

复现命令：

```bash
make test-code-mode
cargo test --locked --lib code_mode::tests -- --include-ignored
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

严格 Clippy 的完整输出（工作区路径已替换为占位名称）：

```text
Checking ssmcp v0.8.0 (<workspace>)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 6m 30s
exit_code: 0
```

## 交付状态

实现、回归测试、使用说明与 CI 入口均包含在 PR #29，合入状态和记录见该 PR 页面。本次验收已完成。本轮没有切换生产服务版本；线上启用对象参数形式需要部署包含本轮改动的服务端 JavaScript 助手。

