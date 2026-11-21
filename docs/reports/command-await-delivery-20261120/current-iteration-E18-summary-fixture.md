# SSMCP 0.10.25：E18 协议成功路径夹具调整已合并，Gate20 运行中

**记录截止：2026-11-21 13:52:57 UTC。E18 合并与 Gate20 启动原件均已独立读回；本次 Gate20 快照仍为 running，尚无结束回执。** [PR #173](https://github.com/LIghtJUNction/ssmcp/pull/173) 于 **13:44:16 UTC** 正常合并，当前候选为 **`02aa4dcd18786062ef44be6c76422dff054fbfed`**，tree 为 **`167e9976d2ac334fb3dfbd58f82288baf0eb9b17`**。报告组分别读取 GitHub main、合并提交与 PR，三者身份一致；真实父提交为 `cf358f00f84b73e9892e62dd9b50b0dcc218201c` 与 `8521fc1a9a81788cc93e727b66b4dfcbcc5660c5`；作者与合并提交的树及完整补丁相同。[固定合并提交](https://github.com/LIghtJUNction/ssmcp/commit/02aa4dcd18786062ef44be6c76422dff054fbfed)

E18 只改动 **`src/http/question_delivery_tests.rs`**，117 行增加、36 行删除。三个协议成功路径夹具的等待预算由 **1 秒改为具名的 5 秒**，另加显式 **9 秒观察 guard**。生产 deadline、身份复验、quota 逻辑与原关键断言未改；没有新增测试定义。**E18 作者阶段**未运行 Cargo、编译、测试或产品验收，随后 Gate20 的实际编译进展在下节另列。[E18 合并事实](source-E18-summary-fixture-merged-public-facts.json)

这是一项明确的**测试成功路径等待窗口调整**，不能描述成“原 1 秒场景已复验通过”。旧 Gate19 的 4 通过／1 失败／30 未运行，以及 private 的 2102 passed／2 failed，继续原样保存。未来若新夹具通过，结论也只归属于新的源码与预算。

## 为什么要区分“成功标准”和“等多久”

异步协议检查包含两件事：一是必须观察到哪些请求、回复、身份与确认动作；二是给这些动作留下多长的等待窗口。扩大观察窗口，不必然改变成功标准，但确实改变了夹具的时间条件。因此，本报告同时记录预算变化和保留的协议断言，不将两者混为一项“测试通过”。

| 项目 | E18 的变化或保留 | 证据范围 |
|---|---|---|
| 三个协议成功路径夹具 | 原 1 秒改为具名 5 秒预算 | 测试源码调整，尚未执行 |
| 观察 guard | 增加显式 9 秒边界 | 限定所包围的观察阶段，不是整项测试墙钟上限 |
| code-mode 参数 | 原 10 秒保持 | 不因新观察 guard 变成 9 秒 |
| public await 默认预算 | 原 5 秒保持 | 与新成功夹具的 5 秒是不同对象 |
| 生产 deadline、认证与 quota | 未修改 | 本次只改测试文件 |
| 原超时、恢复、取消测试 | 原 1 秒输入保持 | 未用成功路径预算替换它们的合同 |

**9 秒不是整项测试从开始到退出的最大耗时。** 它由 5 秒成功路径预算、未改的 1 秒 collector probe 预算和 3 秒余量组成；probe 与放行后观察共用该 deadline。夹具建立和前十次调用在此前，撤权 dispatch、abort／join 没有额外总界限；OAuth 与 agent 迭代分别建立 deadline。原 code-mode 10 秒输入也不是包括 finalization／ACK 的整测试上限。

撤权夹具要求 probe 已实际到达且观察任务仍待完成；如果观察任务提前结束，该路径判失败。清理先放行 peer 的暂停点，再处理结果，仅对尚未结束或未消费的 handle 执行 abort／join，不再次等待已由 select 消费的结果。这些是静态行为核对，清理路径在作者阶段没有动态执行。

本次仍保留 **11 次调用、单次 wait、单次 probe、ACK、unauthorized 与 no-consume** 等关键断言。它们约束请求数量、交付确认和撤权后的行为；预算变长没有被用于删掉这些条件。静态审阅结论为 READY，表示源码合同已按此范围核对，**不等于新等待窗口下已经运行成功**。[固定夹具源码](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/http/question_delivery_tests.rs)

旧失败不能仅凭此次时间调整就被归为机器慢、调度偶发或已确认的性能问题。E18 的实际效果仍须由新来源绑定后的原定检查给出。

## 作者阶段：源码完成，未执行验收

原作者沿原会话于 **13:32:04.273349 UTC** 启动，**13:38:45.338024 UTC** 正常退出，exit code 为 **0**、error 为 null。作者从 E17 作者提交 183d7ad 开始，先快进到实际主分支 S_cf358；两者 tree 均为 a9b283…，再形成 [E18 作者提交 8521fc1](https://github.com/LIghtJUNction/ssmcp/commit/8521fc1a9a81788cc93e727b66b4dfcbcc5660c5)、tree 167e997…。

| 原记录 | 实际结果 | 说明 |
|---|---|---|
| 首次 rustfmt check | 退出 1 | 格式检查尚未满足，不隐去这次失败 |
| 同轮 git diff check | 退出 0 | diff 格式检查完成 |
| format adjustment | 退出 0 | 修正排版 |
| diagnostic wording 调整 | 退出 0 | 调整诊断文案，不是测试运行 |
| 最终 rustfmt 与 git diff check | 均退出 0 | 最终格式通过 |
| commit | 退出 0 | 形成作者提交 |
| 一次正常 push | 完成退出 0 | 没有强推 |
| 117 条 post_commit Git 记录 | 全部退出 0 | **Git 静态记录，不是 117 项测试** |
| Cargo／编译／测试／Gate／产品 | 均未执行 | 没有本阶段运行验收成绩 |
| 新增测试定义 | 0 | 调整的是既有成功路径夹具 |

推送时出现凭据存储 D-Bus 告警，但 push 完成退出 0；随后远端引用记录确认作者分支为 8521…，当时 main 为 S_cf358。之后 PR173 的实际合并身份单独核对，不能把推送时的旧 main 写成当前主线。

源码回执记录 index、worktree 与四个子模块干净。18 项关键文件与 11 项内部 Git 来源均标记 unchanged，包括 Cargo.lock、六份发布配方及 E16／E17 的修复源码。报告组读取了这些保留项和检查记录，没有将“回执标记未变”扩写成再次全文审阅全部文件。

当前 `Cargo.lock` 仍为 **66476 字节**，SHA-256 `b8caf2e4f747094a0bcd12842b1fe7894d7d956efa1f8b756f17a16518b9d393`；zuse 仍锁定 `8d37a7df6fd94df1d0939eb6800c238e71f5835d`。E18 没有改锁或追逐后来观察到的上游 head。

## Gate20：原件确认运行中，前三阶段通过

原先 P20 prepared／unbound 状态已由后来实际启动原件更新。Gate20 外层于 **13:49:25.180585 UTC** 启动，绑定源码为 S02aa／tree 167e997…。报告组完整读取内外 launch，并于 **13:52:57 UTC** 读取 Gate 与 binding 的选定字段：

| 已完成阶段 | 退出码 | 原阶段耗时 |
|---|---:|---:|
| release-policy | 0 | 10.62 秒 |
| deployment-policy | 0 | 0.62 秒 |
| fmt | 0 | 4.66 秒 |

另据原执行者 13:50:26 的通报，library-compile 当时 active；报告组本次快照没有该阶段完成结果。内外两份 exit 均不存在，**运行中不等于整批通过，库测试尚无新增通过结果**。其余阶段没有完成结果，不等于它们都尚未启动。

原执行者报告计划沿用以下数量；**报告组本次尚未完整读取 bound stage contract**，不把该计划表写成已经独立逐项复核的执行合同：

| 原计划范围 | 数量 |
|---|---:|
| 完整阶段 | 35 |
| Rust 结果阶段 | 26，不是 26 个用例 |
| integration targets | 20 |
| 必跑 ignored 测试 | 14 |
| host 库测试 | 6 |

launch 中记录的九项 before 哈希与 binding 的 expected 字段相同。这是原件字段比较；后续独立运行中重哈希请求被 `code_mode busy` 立即拒绝，未创建原生任务，**不能宣称已经重哈希九个文件，也没有完成后置哈希比较，不能写成输入前后不变**。准备清单保存于 **13:47:37.382872 UTC**，其中 `execution_started=false` 是启动前历史状态，不覆盖后来的 launch。

| 运行原件 | 字节数 | SHA-256 | 读取范围 |
|---|---:|---|---|
| 内层 launch | 3636 | `7c7596d9f1b33bc3ea0cff99bf3bf45e686432960b75b806bf11010af3919fe2` | 全文 |
| 外层 launch | 1008 | `5d7b288c0e38878bc288c2bd85acef747912153e0f19395d33a0b5f4e71d9ad3` | 全文 |
| running Gate 快照 | 32651 | `28a3b64d69ed691e4cb02bef23e3ebc0a54e0a115fcd67801bb0f73d958532b2` | 完整哈希与状态／前三阶段等选定字段 |
| binding | 16561 | `8918b7e3b1b9b23a1c52ba00bcdb6ff3c65ee9b013ea5f356e8d121c281e96be` | 完整哈希与来源／输入等选定字段，未全读 stage contract |

Gate 文件仍可变化，所列摘要只对应 13:52:57 的运行快照，不是最终结果。E18 没有增加检查阶段，现有回归继续使用原库阶段；不预填测试通过数，也不将旧 ELF 的结果转入新候选。

source／target 窗口仍由原执行者独占。报告组及协作者保持 0 Cargo／metadata／构建／产品／测试／target 写入／H2／dispatch 和排队重任务，没有待回执重操作；允许的原生只读回执核对继续进行，不另启模型、控制器或替代验证器。

## Gate19 的失败保持原样

旧 Gate19 实际来源为 **`cf358f00f84b73e9892e62dd9b50b0dcc218201c`**，于 **13:10:20 UTC** 失败结束。内外结束回执均为 1，九项输入前后相同，结束时工作树干净且仍为 S_cf358；这些一致性字段没有把失败变成通过，也不独立证明所有后代都已回收。

| Gate19 项目 | 原结果 |
|---|---|
| 完整 35 阶段 | 4 通过、1 失败、30 未运行 |
| private 库 | 退出 101；2102 passed、2 failed、26 ignored、6 filtered |
| 耗时 | 外层阶段 72.85 秒；父测试汇总 72.83 秒 |
| 第一失败 | `tool_summary_code_mode_wait_for_keeps_the_native_delivery_boundary`，959:5 实际 timed_out、期望 ready |
| 第二失败 | `tool_summary_passive_await_rechecks_oauth_and_binding_after_probe`，1007:14 等待最终 probe 到达时 Elapsed |
| E16／E17 三项新回归 | 实际 ok，已经包含在 2102 中 |
| 三个子进程各自的 1 passed | 不追加到父汇总 |

第二项旧失败在 3 秒 probe 到达屏障处结束，该迭代未进入后续撤销 OAuth／agent 或继续动作。Elapsed 不能推出撤销已经发生，也不能证明 probe 永远不会到达。E18 对成功路径夹具作了明确调整，但**不会将未来新预算下的结果回填成 Gate19 原 1 秒场景通过**。

三个新增库回归通过，也仍不等于旧 whole-text 取消 EOF 或 Questions 截图问题已获得实机 UI／E2E 通过结果。详细诊断、原件摘要和读取范围保留在 [Gate19 失败报告](current-iteration-Gate19-failure.md)与 [事实 JSON](gate-Scf358-r19-failure-public-facts.json)，不覆盖旧报告。

## 更早成绩、消费者与发布范围

| 原来源 | 保留成绩 | 不能用于证明 |
|---|---|---|
| S_b8d Gate18 | 原 35 阶段全部通过 | 新 S02aa 的编译、测试或构建通过 |
| S_b8d N18 | 只读来源证明通过 | 新候选已完成来源证明 |
| S_b8d runtime18 | 6 transport＋12 private 全通过 | 生产 relay ACK 或当前新源码运行 |
| S_b8d UI24 首轮 | 4 启动、3 通过、第 4 项失败、20 未运行；264 父检查 | 新源码 UI 已完成；五个 child 用例不追加 |
| E16／E17 源码阶段 | 各自正常合并；三个新回归后来在 Gate19 通过 | 整套 Gate19 或真实 UI 已通过 |

历史详情继续见 [Gate18 完整报告](current-iteration-Gate18-passed.md)、[runtime18／UI24 首次结果](current-iteration-runtime18-UI24-failure.md)、[E16 报告](current-iteration-E16-merged.md)和 [E17 报告](current-iteration-E17-questions.md)。Gate16、Gate17 及 W UI 的失败亦按各自原来源保留。

当前尚无 N20 的通过结果，也没有新源码 runtime、UI24、controls、多 UID、version_all、P1 或两项 native 补充通过结果。两项 native 补充仍只是未绑定静态准备，不能提前算作执行；后续保持原定消费者范围与串行交接流程。

用户希望发布的请求已由源码协调者记录，原发布目标保留。**源码任务退出 0、静态 READY、正常合并均不是正式发布回执。** H2 三模块仍未 import／execute，没有 release dispatch、安装、生产 reload 或部署；crate publication 与生产 relay 部署延期仍为两项独立事项。[既有 H2 静态交付](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)

Computer Use 仍为应用内许可，`os_permission_boundary=false`。此次测试夹具调整没有改变生产控制权限、操作系统隔离或同 UID 程序的能力边界。

## 原件身份与读取边界

| 原件 | 字节数 | SHA-256 | 本稿使用范围 |
|---|---:|---|---|
| launch-receipt.json | 1934 | `62290a6a58cda4a45179e4681a4e8e7f267e67e43e4ddfd31b1e2fae68696f0a` | 完整原件读取，含阶段开始与来源身份 |
| author-exit.json | 2094 | `2ed51becdfce366f66cf5a7872c417f24875127cf1163bff956817d07039bc4c` | 完整原件读取，含结束时间、退出 0、error null |
| source-changes-receipt.json | 56579 | `d0a33c15890a081f2f4a970611e2f9eb9f2c57e759dcb2325e047ae42c88351d` | 完整文件哈希；来源、scope、保留项、检查／提交／推送记录与 117 条 Git 返回码 |
| 两份作者 diff（各一份） | 9272 | `54f784754337cf60ce156bac313e56937e4ecdbfa70ad75655e5d2540cee9e55` | 回执提供文件身份；GitHub 作者与合并的完整补丁另经对照相同 |
| 修改后 question_delivery_tests.rs | 46021 | `5f29c9a8304417a577025d58befb31d6888e32295a6e32e51d70976596f2bb2f` | 本轮测试文件身份 |

报告组读取源码回执的选定字段与检查命令记录，并独立核对 GitHub main／commit／PR。完整文件哈希不等于逐条审阅全部 Git 输出；长编辑命令与首次格式输出只摘读。本文没有将源码回执当作编译、测试或发布证明。公开内容不复制私密路径、native 身份或夹具载荷；进一步字段保留在 [E18 合并事实](source-E18-summary-fixture-merged-public-facts.json)。

当前可报告的结论是：**E18 已合并，三个成功路径夹具的时间条件已明确调整；Gate20 原件确认运行中，尚未取得终态。** 后继结果必须按新源码、新夹具与实际回执分别记录。

