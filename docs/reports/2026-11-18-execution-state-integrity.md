# ssmcp 执行状态与输出归档一致性迭代报告

日期：2026-11-18（Asia/Taipei）

## 交付结果

本轮修复三类会让调用方误判任务结果的问题：RTK 参数错误绕过 Promise、任务已报告完成但执行名额尚未归还、输出归档损坏或被替换后仍可能返回不可靠结果。修改通过 PR [#31](https://github.com/LIghtJUNction/ssmcp/pull/31) 交付，合入状态与合并提交以该 PR 的记录为准。

最终源代码提交为 [a380b36849d0f2a97658024291c45cfa11870a18](https://github.com/LIghtJUNction/ssmcp/commit/a380b36849d0f2a97658024291c45cfa11870a18)，基线为 [68d87539e97efce78b0e2b90fdac3c6324f46e01](https://github.com/LIghtJUNction/ssmcp/commit/68d87539e97efce78b0e2b90fdac3c6324f46e01)。本报告与验收归档的后续提交仅补充交付材料。

共通过 **125 项测试**，包括 JavaScript 56 项和 Rust 69 项；格式检查、差异检查及严格 Clippy 检查也通过。本轮未部署、重启或切换正在运行的客户端和服务端，未运行远端 GitHub CI。测试结果来自 Linux 开发机上的指定测试集。

## 1. RTK 错误统一通过 Promise 返回

`rtk.start` 和 `rtk.run` 原先会在构造命令、校验选项时同步抛错。因此，调用方在返回值上接 `.catch()` 来不及捕获错误；构造 `Promise.allSettled([...])` 的参数数组也可能提前中止。这时数组中较早的合法命令可能已经启动，但调用方拿不到各任务独立的结果。

现在两个入口都以异步函数返回 Promise。参数错误表现为拒绝，合法任务仍能独立完成并返回任务 ID；原生启动错误和传输错误继续向调用方传播，不增加自动重试。新增回归覆盖 `.catch()`、合法与非法任务混合提交、任务 ID 保留以及启动失败只调用一次。实现和回归见 [RTK 入口](https://github.com/LIghtJUNction/ssmcp/blob/a380b36849d0f2a97658024291c45cfa11870a18/src/code_mode.js) 与 [RTK 测试](https://github.com/LIghtJUNction/ssmcp/blob/a380b36849d0f2a97658024291c45cfa11870a18/scripts/test_rtk_helpers.mjs)。

## 2. 归还执行名额后再发布完成状态

任务结束包含多个步骤：停止进程、收完输出、归还并发名额、发布状态、清理观察器。原先状态可能先变为 `running: false`，而并发名额仍被占用。在并发上限附近，调用方看到任务结束后立即启动替代任务，仍可能遭遇容量不足。

修改将名额归还放到更新任务状态的同一临界区内，并排在发布 `running: false` 之前。进程关闭和输出收集仍先完成；观察器清理可以随后继续。因此，“执行完成”保证执行容量已归还，不代表观察器等全部后续资源都已清理。若其他并发请求先取得名额，新的启动仍需正常遵守并发限制。

回归使用真实进程，占用其余 31 个执行名额，并刻意阻止观察器清理，以检验旧竞态窗口。最终夹具由独立操作系统线程在限定时间内观测状态与可用名额，再释放观察器锁，避免测试自身依赖同一个执行器解除阻塞。实现和测试见 [shell 后端](https://github.com/LIghtJUNction/ssmcp/blob/a380b36849d0f2a97658024291c45cfa11870a18/src/backend/shell.rs)。

## 3. 输出归档先检查，再返回

新增检查保持运行中输出与完整归档的语义不同：

| 场景 | 现在的行为 |
| --- | --- |
| 文件短于清单已记录的字节数 | 读取和统计报错 |
| 标记为完整的归档长度与清单不一致 | 读取和统计报错 |
| 读取过程中提前到达文件末尾 | 报错，不把不足长度的结果当作成功页 |
| 完整归档长度未变，但内容已变 | `outputs.stat` 计算完整 SHA-256 并与清单比对，拒绝不一致 |
| 采集尚未完成，文件仍在增长 | 允许按快照边界读取和统计 |
| 达到输出硬上限，留下部分采集 | 保留 `complete: false`、已保存字节数、观察到的字节数和原错误 |
| Unix 上归档被替换成 FIFO | 使用 `O_NONBLOCK` 打开后，现有常规文件检查拒绝该对象，避免等待管道另一端 |

`outputs.read` 检查长度和读取边界，并返回本页内容的哈希；它不为每一页重新计算整个归档的原始哈希。需要核验完整归档的内容时，应使用 `outputs.stat`。这里也不声称可以抵御同时改写清单和归档内容的同 UID 对手。

FIFO 修复针对打开命名管道时等待另一端的问题，不意味着普通磁盘文件的所有 I/O 都变为非阻塞。实现及测试见 [输出模块](https://github.com/LIghtJUNction/ssmcp/blob/a380b36849d0f2a97658024291c45cfa11870a18/src/output.rs)，使用说明见 [命令文档](https://github.com/LIghtJUNction/ssmcp/blob/a380b36849d0f2a97658024291c45cfa11870a18/docs/commands.md)。

## 验收与证据

| 验证命令 | 最终结果 |
| --- | --- |
| `make test-code-mode` | 56 项通过，0 失败 |
| `cargo test --locked --lib output::` | 24 项通过，0 失败 |
| `cargo test --locked --lib backend::shell::` | 21 项通过，0 失败；默认并发执行 |
| `cargo test --locked --lib code_mode::tests -- --include-ignored` | 24 项通过，0 失败，0 忽略 |
| `cargo fmt --all -- --check` | 退出码 0 |
| `git diff --check` | 退出码 0 |
| `cargo clippy --locked --all-targets -- -D warnings` | 退出码 0 |

RTK 14 项测试包含在 JavaScript 的 56 项中，不重复计数。Rust 三组分别为 24、21、24 项，共 69 项；125 项是这四组测试的合计，不表示仓库所有可能的测试和所有平台均已覆盖。

回归先在旧行为上验证，再在修复后验证：

- RTK 新测试用于旧实现时，10 项通过、4 项失败；修复后该组 14 项全部通过。
- 输出模块新增回归用于旧实现时，21 项通过、3 项失败；修复后 24 项全部通过。
- 最终版执行容量回归在恢复旧发布顺序后明确失败：观察到完成状态时，可用名额为 0，期望为 1。恢复修复后，完整 shell 测试组 21 项通过。

前期 shell 测试夹具曾发生阻塞和失败，随后修正了夹具自身的线程协调方式。上述验收只计入最终完成的验证，不将中断或失败的中间运行计为通过；最终夹具在恢复旧发布顺序后明确失败，捕获了完成状态先于执行名额归还的问题。

验证平台为 Linux x86_64，工具版本：rustc 1.100.0、cargo 1.100.0、Node.js v26.13.1。使用独立工作树与专用构建目录，未修改其他进行中的功能分支或部署。

### 可复查归档

[下载压缩的 JSON 验收证据](https://github.com/LIghtJUNction/ssmcp/blob/fix/execution-state-integrity-20261118/docs/reports/2026-11-18-execution-state-integrity.validation.json.gz)

归档包含最终检查记录、旧实现回归失败记录、最终源文件 Git blob 与 SHA-256、平台版本及验证范围说明。开发机用户主目录及专用构建目录已替换为占位符，测试临时目录路径按日志保留；大量重复的测试输出使用 gzip 压缩保存。

证据文件 SHA-256：

```text
65b52ffac4b5c0eacfb22d09272eec5ac19533faef5c4e33c8c43925b833e6bc
```

解压后可按 `checks`、`regressions`、`files` 和 `summary` 查看，不需执行仓库代码：

```bash
gzip -dc 2026-11-18-execution-state-integrity.validation.json.gz
```

## 提交记录与生效边界

| 提交 | 内容 |
| --- | --- |
| [bf77e473](https://github.com/LIghtJUNction/ssmcp/commit/bf77e4737bb35e52f8ad988ebe2518cbdb2f1b58) | RTK Promise 错误语义与六项回归 |
| [2a452fdb](https://github.com/LIghtJUNction/ssmcp/commit/2a452fdb9a4cefdd5a2c3a7eee18ded7a284df96) | 输出归档完整性检查与 FIFO 拒绝 |
| [a380b368](https://github.com/LIghtJUNction/ssmcp/commit/a380b36849d0f2a97658024291c45cfa11870a18) | 执行名额与完成状态的顺序、最终回归和使用文档 |

服务端的 code mode 助手需要更新到相应版本，RTK 的新错误语义才会生效；执行状态与输出归档修复需要运行相应版本的客户端。合入代码不等于运行中的进程已经升级，本轮交付不包含该升级步骤。
