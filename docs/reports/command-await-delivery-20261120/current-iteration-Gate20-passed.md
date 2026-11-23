# SSMCP 0.10.25：Gate20 的 35 个阶段全部通过，后续运行验收已交接

**快照：2026-11-21 14:26:05 UTC。** 固定候选 **`02aa4dcd18786062ef44be6c76422dff054fbfed`**、tree **`167e9976d2ac334fb3dfbd58f82288baf0eb9b17`** 的 Gate20 已结束：**35 通过、0 失败，内外两份结束回执均为 0**。外层实际运行时间为 **13:49:25.180585—14:22:23.093866 UTC**，约 **1977.913 秒（32 分 58 秒）**，error 为 null。源码协调者随后正式将 source／target 窗口交给原验收执行者；后续执行不以本报告发布为前置。

这次通过包括库测试、集成测试、指定 ignored 用例、严格 Clippy 与本地构建。**本快照尚未取得报告组对 N20、当前 runtime／UI 及其他消费者结果的原件通过核验，也没有正式发布结论。** 不将缺少本报告回执表述成这些后续工作一定尚未启动。[本轮事实与原件身份](gate-S02aa-r20-final-public-facts.json)

## 本轮通过了什么

Gate 的“阶段”是一次检查、编译、测试分组或构建命令；测试的“用例”是该阶段内部的独立检查。两者不能相加。本轮 35 个阶段包含 **26 个 Rust 结果阶段、20 个 integration targets**；26 不是测试用例总数。

| 检查范围 | 本轮实际结果 | 计数边界 |
|---|---|---|
| 完整 Gate20 | 35 个阶段全部退出 0 | 包含检查、编译、测试与构建阶段 |
| release-policy | 106 tests，OK | Python 策略检查，不是发布动作 |
| deployment-policy | 35 tests，OK | 不代表部署已执行 |
| private 库父汇总 | **2104 passed、0 failed、26 ignored、6 filtered** | 父汇总耗时 78.72 秒 |
| host 库父汇总 | **6 passed、0 failed、0 ignored、2130 filtered** | 独立分区，父汇总耗时 0.03 秒 |
| 19 个普通 integration targets | **101 passed、0 failed** | 各 target 原 ignored 状态保留 |
| zuse worker entry target | **1 passed、0 failed** | 与上述 19 个 target 分列，合为 20 个 targets |
| 4 组必跑 ignored 用例 | **2＋7＋3＋2＝14 passed** | 2 个库用例、12 个 integration 用例 |

private 日志另有三个子进程各自的 `1 passed`，**不追加到 2104**。同样，单独摘读的七条回归 `ok` 已包含在该父汇总中，不再加七。普通阶段中的 ignored 数也不倒改成零；指定的 14 个 ignored 用例通过，另由其实际执行分组证明。本报告不把不同统计口径拼成一个“总测试数”。

报告组已读取这七项在本轮的确切 `ok` 行：E16 的取消状态锁回归、E17 的空 Questions 列表与保留选择直至 Enter 两项键盘回归，以及协议摘要的超时／恢复／取消、code-mode wait、撤权复验、public await 第十一次调用四项。它们证明相应库用例在 S02aa 下通过，**不替代真实 UI、worker whole-text EOF 或生产 relay 的端到端验收**。完整用例名保留在事实文件的 `selected_seven_test_lines`。

| 其他已完成阶段 | 退出码 | 阶段耗时 | 能说明的范围 |
|---|---:|---:|---|
| fmt | 0 | 4.66 秒 | 格式检查 |
| library-compile | 0 | 278.12 秒 | 本轮库测试产物编译 |
| clippy | 0 | 622.62 秒 | 命令保留 `-D warnings` |
| windows-gnu-clippy | 0 | 91.67 秒 | `x86_64-pc-windows-gnu` 目标的严格 Clippy |
| integration-compile | 0 | 129.68 秒 | 本轮集成测试产物编译 |
| release | 0 | 414.05 秒 | 本地构建，不是 GitHub Release |
| decoder | 0 | 211.61 秒 | 本地 decoder 构建，不是发布上传 |

**Windows GNU Clippy 通过不等于 Windows 原生程序已经运行。** 两条 Clippy 命令均为 offline、locked、workspace、all-targets、jobs 1，并保留 `-D warnings`。以上耗时来自本次命令记录，用于说明执行范围，不作为跨机器性能基准，也不据此宣称所有输出都没有任何告警。35 项逐项退出码、耗时和日志摘要见事实文件的 `gate.checks` 与 `log_records`。

## 来源与输入确实绑定到 S02aa

[PR #173](https://github.com/LIghtJUNction/ssmcp/pull/173) 于 **13:44:16 UTC** 正常合并。报告组核对了 main、实际提交与 PR；作者和合并提交的树及完整补丁一致，没有使用预览 merge 身份。[固定合并提交](https://github.com/LIghtJUNction/ssmcp/commit/02aa4dcd18786062ef44be6c76422dff054fbfed)

本轮 Gate run 为 `2f9197c074824f45545f28952342f25d`。最终记录显示九项受跟踪输入 **before／after 哈希完全一致**，工作树干净，最终 HEAD 仍是 S02aa。报告组还在 **13:58:09.264221 UTC** 独立重哈希这九项输入，均与 launch.before 和 binding.expected 相同；这次成功读取是此前 13:52 快照之后的新证据，不能与更早被 busy 拒绝的读取请求混为一次。

报告组已读取 bound stage contract；绑定清单另记录逆向字节与规范化 AST 相同，报告组没有重新执行该比较，也没有复制旧分数。源码锁和独占 target 锁记录保留，编译 jobs 为 1；**锁记录与结束回执不独立证明所有后代进程均已回收**。

Gate 内还保存了本轮库测试二进制及四个运行产物的身份记录。报告组此次读取的是最终 Gate 中的 manifest 字段，**没有独立重哈希这些二进制**；这些字段也不能提前证明后续验收者的私有副本、运行绑定或 N20 已通过。产物记录与消费者的实际使用证明应分别核对。

| 最终原件 | 字节数 | SHA-256 |
|---|---:|---|
| Gate20 完整结果 | 1311388 | `a6adc20c1e712c546017c5a7f6d47fde806771d5ef3db4eada49c31a83f20c8c` |
| 内层结束回执 | 4805 | `e12792c7ddb46646169756934cfb3d8f2fcbff3ee412c023abe055c3584d7c59` |
| 外层结束回执 | 1172 | `c1c2ab0cfed19f890b6d189afa4bcbe4b0296a38b8e853ba2b799dba72cc62b0` |

内外结束回执已完整读回；最终 Gate 文件取得完整哈希并读取来源、状态与结果等选定字段。输入一致性依据启动／结束回执及独立运行中哈希。报告组取得全部 35 份日志的哈希，读取实际 policy／Rust 汇总及七条指定回归原行；**这不等于逐行审计了 1.3 MB Gate JSON 或全部日志文本**。正文依据这一级证据陈述结果，不将文件哈希扩写成内容全量审计。

## E18 新预算下通过，旧 1 秒失败仍然保留

E18 仅调整 `src/http/question_delivery_tests.rs`，117 行增加、36 行删除，没有新增测试定义。三个成功路径夹具的目标输入从 **1 秒改为具名 5 秒**，另设 **9 秒观察 guard**。这次 Gate20 是新来源、新夹具下的实际通过；不能改写为旧 1 秒条件复验通过。[E18 源码与运行中报告](current-iteration-E18-summary-fixture.md) · [固定测试源码](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/http/question_delivery_tests.rs)

生产 deadline、身份复验和 quota 未改，code-mode 原 10 秒输入、public await 默认 5 秒，以及超时／恢复／取消测试原 1 秒输入均保留。11 次调用、单次 wait、单次 probe、ACK、unauthorized 和 no-consume 等关键断言没有删除。

**9 秒不覆盖整项测试的全部墙钟时间。** 建立夹具与前十次调用在观察阶段之前；撤权 dispatch 与 abort／join 没有额外总界限，OAuth 与 agent 迭代分别建立观察 deadline。成功所需的协议行为与允许等待多久是两个条件：本轮满足了调整后的条件，不据此宣称已定位或修复某个生产性能根因。

| 历史来源 | 原实际结果 | 本次如何保留 |
|---|---|---|
| S_cf358 的 Gate19 | **4 阶段通过、1 失败、30 未运行**；private 为 2102 passed、2 failed、26 ignored、6 filtered | 保留原失败；不把 S02aa 结果回填 |
| S_b8d 的 Gate18／N18 | 原 35 阶段及原只读来源证明通过 | 仅属于 S_b8d，不用作本轮输入证明 |
| S_b8d 的 runtime18 | 6 transport＋12 private 通过 | 不证明 S02aa runtime 或生产 relay ACK |
| S_b8d 的 UI24 首轮 | 4 启动、3 通过、第 4 项 zuse_worker 失败、20 未运行；264 父检查 | 五个 whole-text child 成功项不追加到 264，也不改写父失败 |

Gate19 两处失败分别是 code-mode 结果实际 `timed_out` 而期望 `ready`，以及撤权复验夹具等待最终 probe 到达时 `Elapsed`。后者在到达屏障处结束，该迭代尚未走到撤销 OAuth／agent 的后续动作；旧失败不能被解释成已发生撤权或 probe 永远不会到达。详情继续保留在 [Gate19 失败报告](current-iteration-Gate19-failure.md)。

E16／E17 源码修改、此前 Gate16／17 与 W UI 的失败也不被本报告覆盖。[E16 取消修复](current-iteration-E16-merged.md) · [E17 Questions 导航](current-iteration-E17-questions.md) · [Gate18 完整结果](current-iteration-Gate18-passed.md) · [旧 runtime18／UI24 首次结果](current-iteration-runtime18-UI24-failure.md)

## `/send` 与回执：当前源码口径

固定 S02aa 中，**原生收件人与群组都支持 `/send <正文>`**。这两条路径发送字面正文，不隐式读取或附带本机 `@` 文件、`$` 技能；本地 listener 目标才进入 `prepare_references`。发送仍受收件人、账户、连接、预览与待回执等条件检查。原生收件人模式关闭引用补全；群组只在输入被识别为 `/send` 时关闭，不能扩大为选择群组后所有普通输入均关闭。历史材料中“native 目标拒绝 `/send`”不能继续作为当前行为说明。

[目标分流与本地准备](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/listener.rs#L33-L155) · [原生正文传递](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/messaging.rs#L544-L654) · [群组正文传递](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/groups.rs#L226-L287) · [补全条件](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation.rs#L959-L975)

`queued` 表示服务已排队、等待收件人调用；`included` 表示消息已纳入收件人的工具响应；`acknowledged` 表示收件人确认。**三者均不能单独证明对方已阅读、理解或执行完毕**；源码也区分了已纳入响应但连接关闭、无法取得确认的情况。[回执文案](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/messaging.rs#L121-L139)

本节为固定源码核对，不增加后台投递或端到端测试结果。

## 后续验收与发布边界

源码协调者已通报正式交接，原验收者继续按原定范围推进。本快照尚无报告组独立原件核验的 **N20、runtime、UI24、controls、多 UID、version_all、P1 或两项 native 补充**通过结论；不从 Gate20 库测试或构建结果推导它们已经通过。两个 native 补充的既有未绑定静态准备，也不能预先算作运行成绩。

H2 三模块仍未 import／execute，保持既有静态合同；source owner 通报未进行 tag、release dispatch、安装或重启，报告组也未执行这些动作。**本地 release／decoder 构建完成与 GitHub 正式发布是不同事件。** crate publication 延期与生产 relay 部署延期继续分开记录，不能以一项延期替代另一项授权或结果。[H2 静态交付](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)

Computer Use 仍是应用内许可，`os_permission_boundary=false`；本轮通过不证明同 UID shell、独立程序或外部插件已被操作系统权限边界禁止。报告协作继续保持 0 Cargo／metadata／构建／产品／测试／target 写入／H2／dispatch，不另开执行链。后续结果按实际消费者使用的源码、产物和回执单独更新，保留此前运行中快照与失败报告。
