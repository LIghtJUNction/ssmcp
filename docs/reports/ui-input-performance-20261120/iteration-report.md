# SSMCP 迭代记录｜2026-11-20

> 阶段记录，2026-11-20 21:43（Asia/Taipei）。19 项性能相关功能回归通过；真实终端延迟验收仍在进行。

提醒乱序修复和批处理使用指导已合并至工具组摘要功能分支；并行维护工作的 Project Z 依赖更新已合并至 main。界面性能修复已提交草稿 [PR #129](https://github.com/LIghtJUNction/ssmcp/pull/129)，正在验收。用户随后补充了发送消息、撤回编辑和内存占用的优化要求，本轮已一并纳入。本轮尚未发布新版本。

## 已合并改动

### 1. 保留被旧列表覆盖的摘要提醒

同一修订版本下，第 11 次调用已触发提醒，较早生成、仅含 10 次调用的列表随后返回时，旧实现会删除这条提醒，并跳过最终投递探测。修复保留调用计数更新的提醒，同时允许有效的已覆盖列表正常结束提醒。[PR #124](https://github.com/LIghtJUNction/ssmcp/pull/124)

验证直接编译生产 `Capture` 模块，使用其实际 `MAX_CALLS` 常量和开发机已有 serde 依赖：旧实现的目标用例失败 1 项，补丁后的 2 项回归通过；格式及差异检查通过。该结果属于独立模块回归，不能替代完整应用或真实 PTY 验收。开发机回执：`/var/tmp/ssmcp-reminder-order-regression-20261120/report.json`。[验证说明](https://github.com/LIghtJUNction/ssmcp/pull/124)

实现提交：[3162f0f](https://github.com/LIghtJUNction/ssmcp/commit/3162f0f765c8ba9c1d3dc8d60d05b8b7b5ea77d9)；合并提交：[5998949](https://github.com/LIghtJUNction/ssmcp/commit/5998949d0fd6103bf72794c2293ebe8e42eebab9)。

### 2. 将有界批处理写入智能体使用指导

MCP 初始化说明、工具使用提示和 code-mode 文档现在引导智能体合并独立检查。示例将 6 项只读 Git 检查交给 4 个 worker，并保留逐项错误、未启动项目、job ID、输出游标及完整性信息；依赖操作仍按顺序执行。执行逻辑、限制和 schema 未改变。[PR #127](https://github.com/LIghtJUNction/ssmcp/pull/127)

已验证示例的 Node.js 语法和现有 `exec.parallel` 返回契约；未完成实时批次验收，未测得提速幅度。实现提交：[b78c3ec](https://github.com/LIghtJUNction/ssmcp/commit/b78c3ec28276dc8629ba0a0a119dbabbcbe24194)、[184408d](https://github.com/LIghtJUNction/ssmcp/commit/184408d9fd91224f1673b61ddaa85b4ccc1a7b7e)；合并提交：[eb9b206](https://github.com/LIghtJUNction/ssmcp/commit/eb9b206900fdc1700ec9d4d0f8defcced2b729b8)。

### 3. 更新已发布的 Project Z 依赖

依赖更新已通过 [PR #120](https://github.com/LIghtJUNction/ssmcp/pull/120) 合并，提交为 [ec91a0a](https://github.com/LIghtJUNction/ssmcp/commit/ec91a0a9b3109438eba795e7cc107c5865da3828)。该 PR 将 zharness 固定为已发布的 0.6.0，并记录依赖来源、校验值及针对性验证。后续发布仍须在整合后的源码版本执行验收，不能用该 PR 的验证代替。

## 界面延迟：草稿 PR 已提交，效果待测量

用户报告点击鼠标、发送消息和撤回编辑时延迟约 1 秒；端到端计时基线尚未建立。源码确认：工具日志先取得 terminal `State` 锁，再通过 `current_owner()` 等待 history 存储锁；后者可能覆盖文件扫描、JSONL 重写和文件、目录的 `fsync`。因此，历史 I/O 可以经由日志路径阻塞鼠标、输入回填和绘制。[terminal 调用链](https://github.com/LIghtJUNction/ssmcp/blob/ec91a0a9b3109438eba795e7cc107c5865da3828/src/client/terminal.rs#L1597-L1635) · [原身份读取](https://github.com/LIghtJUNction/ssmcp/blob/ec91a0a9b3109438eba795e7cc107c5865da3828/src/client/interface/history/ownership.rs#L179-L197) · [同步写入](https://github.com/LIghtJUNction/ssmcp/blob/ec91a0a9b3109438eba795e7cc107c5865da3828/src/client/interface/history.rs#L1290-L1326)

草稿 [PR #129](https://github.com/LIghtJUNction/ssmcp/pull/129) 包含三项改动：

- **当前 history owner 分锁。** 用共享的短锁身份对象承载唯一当前选择，使显示读取不再等待历史磁盘锁；保留 token、线程绑定和撤销语义。持住存储锁仍可读取身份，以及切换、撤销、释放的确定性回归已通过。
- **复用事件快照的工具分组结果。** 去除逐事件反复构建同一分组的计算；折叠、展开时可见卡片数量和逻辑调用计数的回归已通过。
- **直接读取正在编辑的消息状态。** 原编辑轮询约每 20 ms 构造包含完整 outbox、草稿和上下文的 JSON，仅为了判断一条消息是否取消或已送达。新实现按完整消息 ID，在同一 Manager 锁内读取原有状态值，省去全量 JSON 分配和请求 ID 复制。取消、撤回确认、重新发布、跨会话重试及交付赢得撤回竞争的回归已通过；未修改消息保留规则。[实现提交](https://github.com/LIghtJUNction/ssmcp/commit/67d546e7f9c4cfccdefbd15ba0aeb4a7fbbb3567)

实际测试源码为 [67d546e](https://github.com/LIghtJUNction/ssmcp/commit/67d546e7f9c4cfccdefbd15ba0aeb4a7fbbb3567)，包含先前 [ad9bd66](https://github.com/LIghtJUNction/ssmcp/commit/ad9bd667f7bcd2ba0ea457f33a4c2a30a3014677) 的历史分锁和快照优化。四个子模块已按 gitlink 固定版本初始化，验证前后工作树保持干净。后续 [e7b7884](https://github.com/LIghtJUNction/ssmcp/commit/e7b78845e2778cf884cd344ee482d2ca95334f9b) 仅增加真实终端计时脚本，未改变被测生产源码。PR 仍为 draft，等待物理输入验收。[提交与状态](https://github.com/LIghtJUNction/ssmcp/pull/129)

| 已运行范围 | 通过数 | 结果 |
| --- | ---: | --- |
| History ownership 模块 | 5 | 通过 |
| 工具组快照、折叠与展开 | 1 | 通过 |
| 撤回、编辑、投递竞争与修订号 | 4 | 通过 |
| 归档失败、迟到输出、重试与会话隔离 | 9 | 通过 |
| **合计** | **19** | **零失败** |

后九项直接使用已经冻结的 libtest，未重复编译；其运行前后 SHA-256 均为 `005686d64d5aff0b52deb3b9bcc64d75a2f460c5756d12a399097e45d80db6b6`。两次原生任务退出码均为 0，输出完整、无截断。结构化证据记录了源码、子模块、每项测试结果和输出摘要哈希：[native-validation.json](https://github.com/LIghtJUNction/ssmcp/blob/perf/ui-input-history-owner-20261120/docs/reports/ui-input-performance-20261120/native-validation.json)。

真实终端脚本复用现有 PTY、账号路由夹具与独立 VT 解码器，记录 Enter、Up、鼠标点击和普通输入到首个对应画面的耗时，并核对输入与实际历史写入调用重叠。计时期间不发送额外 UI 查询 RPC，超出诊断预算会以非零退出码报告；本报告尚无这部分测量结果。[计时脚本](https://github.com/LIghtJUNction/ssmcp/blob/e7b78845e2778cf884cd344ee482d2ca95334f9b/docs/reports/ui-input-performance-20261120/pty_latency_focused.py)

在用主客户端的单次内存基线为 RSS 13,736 KiB（约 13.4 MiB）、PSS 6,859 KiB、进程历史峰值 RSS 25,784 KiB。该进程尚未运行本轮性能补丁，以上数据不构成前后对比。当前证据支持去掉高频临时分配，尚不能给出常驻内存下降幅度。基线回执位于开发机 `/var/tmp/ssmcp-pr129-latency-review-ad9bd667-20261120/memory-baseline.json`。

## 仓库维护与交付状态

- 分支审计时（main 为 `c451ef0`），未发现已合并且未被工作树占用、可安全删除的分支；本轮未删除分支。
- 未执行版本发布。
- 下一步在最终整合源码的冻结二进制上运行真实 PTY 计时，并完成整合后的发布验收。当前 19 项结果证明相关功能与会话归属约束通过，尚不能据此给出约 1 秒卡顿的改善幅度。
