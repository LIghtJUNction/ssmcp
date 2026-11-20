# SSMCP 迭代记录｜2026-11-20

> 更新于 2026-11-20 22:11（Asia/Taipei）。补丁候选已通过 19 项 Rust 回归、69 项真实终端验收检查和 8 次响应计时；最终整合版本的发布验收仍在进行。

## 本轮结果

本轮完成提醒乱序修复、批量执行指导，以及针对发送消息、撤回编辑和鼠标卡顿的三项性能改动。提醒修复与批量指导已合并到工具组摘要功能分支；性能补丁 [PR #129](https://github.com/LIghtJUNction/ssmcp/pull/129) 已通过补丁验收并转为可审阅状态。主线整合工作由 [PR #133](https://github.com/LIghtJUNction/ssmcp/pull/133) 汇合。这些结果尚不等于最终版本已经发布或已替换在用客户端。

## 改动及原因

**保留正确的工具组摘要提醒。** 同一修订版本下，第 11 次调用已经产生提醒，但先前生成、只含 10 次调用的列表较晚返回时，旧实现会清掉这条提醒。修复会保留更新的计数，同时允许有效的已覆盖列表正常结束提醒。直接编译生产 Capture 模块后，旧实现的目标用例失败；修复后的两项回归全部通过。该结果是模块级验证。[PR #124](https://github.com/LIghtJUNction/ssmcp/pull/124)

**让智能体明确采用有界批处理。** MCP 入口说明、工具提示和文档增加了六项独立 Git 检查、四个 worker 的示例，并保留每项错误、job ID、输出游标及完整性信息。依赖操作仍顺序执行。执行器及 schema 没有变更；示例通过 Node.js 语法和既有返回契约核对，未据此声称实际吞吐提升。[PR #127](https://github.com/LIghtJUNction/ssmcp/pull/127)

**把界面读取与历史磁盘操作分开。** 原来界面日志在持有 UI State 锁时读取当前 history owner，该读取又要等待可能正在扫描、重写 JSONL 或 fsync 的历史存储锁。新实现用同一共享身份对象上的短锁提供当前归属读取，保留历史事务、token、线程绑定、归档和撤销语义。确定性回归验证了历史存储锁被持有时，归属读取仍能返回。[实现提交 ad9bd66](https://github.com/LIghtJUNction/ssmcp/commit/ad9bd667f7bcd2ba0ea457f33a4c2a30a3014677)

**复用界面快照的工具分组。** 单次快照不再为每个事件重复构建相同分组；折叠、展开和逻辑调用计数回归通过。该改动减少快照生成期间的重复计算。[PR #129](https://github.com/LIghtJUNction/ssmcp/pull/129)

**减少编辑轮询中的临时分配。** 原编辑循环约每 20 ms 构建完整 outbox、草稿和上下文的 JSON，只为了读取一条消息的状态。新实现按完整请求 ID，在同一个 Manager 锁内调用原状态计算，省掉整份 JSON 和请求 ID 的复制。取消、撤回确认、再次发布、跨会话重试和已送达判断保持原有约束；没有引入缓存或改变消息保留规则。[实现提交 67d546e](https://github.com/LIghtJUNction/ssmcp/commit/67d546e7f9c4cfccdefbd15ba0aeb4a7fbbb3567)

## 运行验证

| Rust 回归范围 | 通过数 | 失败数 |
| --- | ---: | ---: |
| History ownership 模块 | 5 | 0 |
| 工具组快照、折叠与展开 | 1 | 0 |
| 撤回、编辑、重试、投递竞争及修订号 | 4 | 0 |
| 归档失败、迟到输出、重试归属与跨会话隔离 | 9 | 0 |
| **合计** | **19** | **0** |

实际测试源码为 `67d546e7f9c4cfccdefbd15ba0aeb4a7fbbb3567`。四个子模块均按 gitlink 固定版本初始化，验证前后工作树保持干净。后九项直接运行冻结的 libtest，未重复编译；其前后 SHA-256 一致。后续 `2c26fff3` 只在两份 Rust 文件中调整四处格式器要求的换行，格式及差异检查通过。[Rust 验证记录](https://github.com/LIghtJUNction/ssmcp/blob/2a94204e08469e5ff0993ca2635d0cd69daff061/docs/reports/ui-input-performance-20261120/native-validation.json)

### 真实终端响应

独立的 PTY 测试使用真实账号路由夹具、WebSocket、客户端及 VT 解码器。计时从物理输入写入开始，到解码器首次看到对应画面结束；期间不添加按键或 UI 查询 RPC。测试包含 69 项验收检查，以下 8 个测量全部通过诊断预算。

| 操作 | 实测延迟 | 本轮诊断预算 |
| --- | ---: | ---: |
| 第一次发送显示已接受 | 24.043 ms | 500 ms |
| 第二次发送显示已接受 | 42.273 ms | 500 ms |
| Up 确认撤回并回填 | 31.153 ms | 500 ms |
| 编辑后重新发送 | 30.860 ms | 500 ms |
| Up 回填已投递的输入历史 | 35.859 ms | 500 ms |
| 鼠标切换到正文焦点 | 29.034 ms | 250 ms |
| 鼠标切换到编辑框焦点 | 30.194 ms | 250 ms |
| 输入文字显示到草稿 | 36.199 ms | 250 ms |

后三项全部与实际历史写入调用的时间区间重叠；本次完成三次写入。测量包含 5 ms pump 和解码器开销。预算是本轮诊断标准，不是产品 SLO。这是一个隔离 debug-build 场景的八次样本，不是生产长会话的前后对照；不能把用户报告的约 1 秒直接当作同条件基线计算提速倍数。[完整 PTY 记录](https://github.com/LIghtJUNction/ssmcp/blob/2a94204e08469e5ff0993ca2635d0cd69daff061/docs/reports/ui-input-performance-20261120/pty-report.json)

### 制品来源和失败记录

客户端、服务端、VT 解码器与 libtest 全部由干净的 `2c26fff31bb64eae5e8f5cc390e84032d548e2f7` 工作树离线、锁定依赖、单编译任务构建，然后立即冻结。成功测试采用仅修正 Python 夹具的 `0d32db070eb349710339cb94d0e8914a71136c94`；该提交与二进制来源的产品代码完全一致。程序、计时 runner 和测试依赖的前后哈希均未改变。

前两次失败也保留了记录：第一次在开始物理交互前遇到 Node 兼容性探针三秒超时，有界重试后通过；第二次发现既有测试直接比较队列默认目标 `next` 与界面的 `null`。修复只规范这项表示差异，完整请求 ID、完整文本、UTF-8 光标及明确指定的接收目标仍严格比较，没有修改产品或放宽延迟预算。修正后这些约束全部通过。[构建、脚本与失败来源记录](https://github.com/LIghtJUNction/ssmcp/blob/2a94204e08469e5ff0993ca2635d0cd69daff061/docs/reports/ui-input-performance-20261120/validation-provenance.json)

## 内存结论

在用主客户端的一次安装前采样为 RSS 13,736 KiB（约 13.4 MiB）、PSS 6,859 KiB、进程历史峰值 RSS 25,784 KiB。它不是所有会话的总量，也不是补丁前后对照。当前有依据的优化是去掉编辑轮询中的反复 JSON 构建和正文复制；没有测得常驻内存下降比例，因此不报告具体 RSS 降幅。

派生行缓存另有进一步优化空间，但尚未证明其在用户进程内存中占主导，本轮未修改其保留策略。[验证与内存基线](https://github.com/LIghtJUNction/ssmcp/blob/2a94204e08469e5ff0993ca2635d0cd69daff061/docs/reports/ui-input-performance-20261120/native-validation.json)

## 整合与交付状态

Project Z 依赖更新由并行维护工作经 [PR #120](https://github.com/LIghtJUNction/ssmcp/pull/120) 合并至 main，其中 zharness 固定为已发布的 0.6.0。性能补丁的依赖来源另按各自精确提交记录；最终整合版本还须使用同一套冻结程序重新验收。

分支审计时（main 为 `c451ef0`），没有找到已合并、未被工作树占用且可安全删除的分支，本轮没有删除分支。

本轮性能补丁已完成源码、回归和隔离终端验收；最终整合及正式发布仍由共同发布流程完成。当前没有发布新版本，也没有替换在用客户端。完整证据和计时脚本已提交到 [GitHub 报告目录](https://github.com/LIghtJUNction/ssmcp/tree/2a94204e08469e5ff0993ca2635d0cd69daff061/docs/reports/ui-input-performance-20261120)。
