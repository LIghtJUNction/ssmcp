# SSMCP 0.10.25：Gate19 在 private 库阶段失败，两个等待相关用例待定位

**记录截止：2026-11-21 13:26:26 UTC。** Gate19 已在提交 **`cf358f00f84b73e9892e62dd9b50b0dcc218201c`**、tree **`a9b283173ee46cff86ca4a119566f0ebc488292a`** 上实际执行，并于 **13:10:20 UTC** 失败结束。原 35 阶段中，**4 个通过、1 个失败、30 个未运行**；内层启动器与外层控制器的退出回执均为 **1**。[本轮失败事实](gate-Scf358-r19-failure-public-facts.json)

失败的是第 5 阶段 `library-private`，退出码 **101**。完整父汇总为 **2102 passed、2 failed、26 ignored、6 filtered**；阶段耗时 72.85 秒，父测试汇总耗时 72.83 秒。两项失败分别是 HTTP tool summary 的 `wait_for` 交付边界检查和 passive await 的最终 probe 检查，具体原诊断见下文。

E16 的取消锁回归与 E17 的两个完整键盘入口回归，本轮均已实际编译、执行并记录 `ok`。**它们包含在 2102 中，不能再加 3；日志里三个子进程各自的 1 passed 也不相加。** 这三个库回归通过不等于 UI／E2E 已复验，整套 Gate19 仍为失败。

源码协调者正在只读诊断，尚未观察到下一作者阶段启动。当前没有盲目重跑、放宽预算、删除断言或跳过失败项；N19 及后续消费者均未执行。旧“准备中”与恢复资料里的 running 字段是历史快照，不再代表当前 Gate19 状态。

## 实际来源、启动与结束

本轮使用 [PR #172](https://github.com/LIghtJUNction/ssmcp/pull/172) 于 **12:56:33 UTC** 正常合并后的实际主分支。真实父提交为 `9362d2232e1ca57d976308b10df6ed6b4b167332` 与 `183d7ada89d88e53b30c4d4bc8c4aef62c1e8d91`；包版本仍为 **0.10.25**，zuse 精确来源为 **`8d37a7df6fd94df1d0939eb6800c238e71f5835d`**。[固定合并提交](https://github.com/LIghtJUNction/ssmcp/commit/cf358f00f84b73e9892e62dd9b50b0dcc218201c)

| 执行层 | 实际开始（UTC） | 实际结束（UTC） | 终态 |
|---|---|---|---|
| 外层控制器 | 13:04:16.681655 | 13:10:20.501836 | 退出 1，owned error 为 null |
| 内层启动器 | 13:04:17.882 | 13:10:20.463 | 退出 1，记录 `AssertionError: gate did not pass` |
| Gate driver | 13:04:19.413 | 13:10:20.359 | `failed`，4／1／30 |

此前报告中的“Gate19 待运行”现已由这些实际启动与结束原件更新。本轮原合同仍为 **35 阶段、26 个 Rust 结果阶段、20 个 integration targets、14 个必跑 ignored 测试**；没有加减阶段。26 是阶段数，不是只有 26 个测试。

## 原 35 阶段：已执行 5 个，随后停止

| 顺序 | 阶段 | 退出码 | 原阶段耗时 | 实际结果 |
|---|---|---:|---:|---|
| 1 | release-policy | 0 | 14.97 秒 | 106 项，父汇总 14.614 秒、OK |
| 2 | deployment-policy | 0 | 12.89 秒 | 35 项，父汇总 12.607 秒、OK |
| 3 | fmt | 0 | 9.87 秒 | 格式检查通过 |
| 4 | library-compile | 0 | 201.65 秒 | 库测试产物编译通过 |
| 5 | library-private | 101 | 72.85 秒 | 父汇总 2102 通过、2 失败、26 ignored、6 filtered |

剩余 **30 阶段未运行**。下表按原 Gate18 的同一 35 阶段计划归类；Gate19 绑定记录声明合同和逆向字节／AST 相同。名称来自既有计划，不是本轮执行记录，报告组没有重跑一致性比较：

| 未运行范围 | 阶段数 |
|---|---:|
| library-host | 1 |
| 本机 Clippy、Windows GNU Clippy | 2 |
| integration-compile | 1 |
| 20 个集成 target 的执行阶段，包括独立 worker 入口 | 20 |
| 原定四个 mandatory ignored 阶段 | 4 |
| release 与 decoder 构建 | 2 |
| **合计** | **30** |

因此，本轮没有新的 host 6 项、Windows GNU lint、集成分区或 14 项 mandatory ignored 通过成绩，也没有完成最终 release／decoder 产物构建。旧 Gate18 的对应成绩不能补到这张表中。

本轮预期的 private 父汇总是 2104 passed、0 failed；实际是 2102 passed、2 failed。ignored 和 filtered 继续保留原义，不能将它们计入 passed，也不以期望数量替换实际汇总。

## 两个失败：已观察到什么，还不知道什么

### 1. tool summary 的 wait_for 返回 timed_out，而断言期待 ready

用例：

`http::question_delivery_tests::tool_summary_code_mode_wait_for_keeps_the_native_delivery_boundary`

原位置为 **`src/http/question_delivery_tests.rs:959:5`**，实际诊断为：

`assertion left == right failed; left String("timed_out"), right "ready"`

959:5 是 ready 断言：实际状态为 timed_out，期望状态为 ready。这证明断言未满足，尚不能定位延迟环节或确定根因。

### 2. passive await 的最终 authenticated-peer probe 等待超时

用例：

`http::question_delivery_tests::tool_summary_passive_await_rechecks_oauth_and_binding_after_probe`

原位置为 **`src/http/question_delivery_tests.rs:1007:14`**，实际诊断为：

`the final native summary probe must reach the authenticated peer: Elapsed(())`

1007:14 是 **3 秒等待最终 probe 到达的屏障**。失败迭代尚未走到后续撤销 OAuth／agent 或继续动作；Elapsed 不能推出撤销已经发生，也不能证明 probe 永远不会到达。

两项都属于 `question_delivery_tests` 中的库检查，**不是此前 UI worker 的“取消 EOF”失败重现**。报告组已读取失败名称、位置和原断言，原源码组继续定位。后继若确认原因或修改源码，再按新的来源、检查和执行回执另行记录。[固定测试源码](https://github.com/LIghtJUNction/ssmcp/blob/cf358f00f84b73e9892e62dd9b50b0dcc218201c/src/http/question_delivery_tests.rs)

部分原件读取请求超时后，通过既有授权维护会话取得。**取证超时与测试 timed_out／Elapsed 分开记录**，未用作测试失败根因。

## E16／E17 的三个新回归已实际通过，但范围没有扩大

本轮 private 原日志分别记录：

| 来源 | 新回归 | 实际结果 |
|---|---|---|
| E16 | `computer::zuse_backend::tests::cancellation_returns_while_native_submit_is_blocked_and_preserves_its_error` | ok |
| E17 | `client::terminal::conversation::questions::history_tests::full_keyboard_empty_questions_list_arrows_move_between_panels` | ok |
| E17 | `client::terminal::conversation::questions::history_tests::full_keyboard_questions_list_arrows_preserve_selection_until_enter` | ok |

本轮新增了上一份源码报告之后的执行记录：当时它们只是新增定义，未编译、未执行；现在三项都已进入本次库测试并通过。三个名字是 **2102 个父汇总通过项的子集**，不另外增加用例总数。原日志另有三个子进程各自的 1 passed 汇总，同样不再次累计。

E16 通过的是受控提交与取消回归，仍不证明 cancel 返回时 native／清理已结束，也不使最后检查与调用原子化。E17 完整键盘入口回归**没有替代实机截图、PTY 或 UI24 运行验证**。[E16 源码报告](current-iteration-E16-merged.md) · [E17 合并报告](current-iteration-E17-questions.md)

三项通过不等于 whole-text 的取消 EOF 问题已通过 UI 复验。S_cf358 尚未执行 UI24，原 0.7 秒夹具条件未放宽。

## 输入与工作树未变，不等于整批通过或所有后代已回收

原结束记录确认：

- 九项固定输入 before／after 的哈希一致；报告组还在运行中独立读取过这九项输入的实际哈希。
- 结束时工作树干净，HEAD 仍为 `cf358f00f84b73e9892e62dd9b50b0dcc218201c`。
- 外层取得源码锁并保持到子进程返回；原记录确认 target 所有者锁覆盖整批。
- 内外层退出码均为 1，没有把 `owned_error=null` 误写成产品检查成功。

九项输入分别是 Cargo 配置、Cargo.lock、依赖审计器、driver、Git 来源记录、owner agreement、审阅基线、workspace wrapper 和 wrapper。它们不变可以说明本轮按固定输入执行，**不能让失败断言变成通过，也不独立证明所有后代都已回收**。

准备清单中的 `execution_started=false` 等字段来自启动前状态；后来的 launch 才证明实际启动。清单记录的反向字节与规范化 AST 一致性也只按已有记录保留，本次报告没有重跑比较。冻结文件的精确摘要用于识别调用者给出的那份文件，实际 Git 提交与树另行核对，不靠单个 JSON 哈希替代源码身份。

## 后续消费者暂停，两项 native 补充仅为静态准备

| 范围 | 截至本快照状态 |
|---|---|
| 下一作者阶段 | 尚未观察到启动；源码协调者只读诊断中 |
| N19 来源证明 | 未执行 |
| 新源码 runtime／UI24 | 未执行 |
| controls／多 UID／version_all／P1 | 未执行 |
| 两项 native 补充 | 未执行，仅保留未绑定的静态材料 |
| H2 发布核验 | 未执行 |
| 正式 release dispatch、安装、生产 reload／部署 | 未执行 |

两项 native 补充材料记录 **`execution_allowed=false`、`final_binding=null`、`runtime_execution=false`**。9362d22 只是参考源码，不是运行绑定，没有新增通过数。报告组完整读取 manifest 与 pending 字段，对相关源文件仅计算哈希，未 import 或执行；具体文件身份保留在事实 JSON。

源码协调者已经记录用户希望发布的请求，报告组读取了检查点中的请求记录，未独立读回原交付消息。原发布目标保留，但本次失败后仍按原流程暂停消费者。没有盲重跑、放宽断言或预算，也没有跳过失败项来形成“通过”结果。

H2 模块与配方的既有静态准备不等于正式发布验证；crate publication 和生产 relay 部署的延期仍分别保留。报告组继续 **0 Cargo／metadata／构建／产品／测试／target 写入和排队重任务**，没有另启模型、Gate 或修复作者。

## 13:08 的恢复快照不能覆盖 13:10 的失败终态

最后实读的恢复用 `continuation.json` 为 **22519 字节**，SHA-256 **`de8f0ad9e51bd2297574387c4ca02faff5d74602093a6079599fca0dcee55589`**。它取自 **13:08:05.759058 UTC** 保存的 running 协调检查点；该来源检查点为 17408 字节，SHA-256 `d5bb049e8040ea6fff7ef817d930aa0e19ba22b9a662bbaf69abc9e25d85ad67`。

这些资料保存时 Gate19 尚未结束，running 与下一步计划仅表示当时状态。**当前以 13:10:20 的 gate 与两份退出原件为准：failed，内外退出 1。**

这次读取观察到 boot 未变，报告组没有重启或 reload。恢复资料的保存不代表脚本自然触发，更不证明图形窗口、连接或旧会话已经恢复；没有新增恢复通过结论。

## 历史结果保留，不能跨源码填补本轮未运行项

| 原来源 | 原结果 | 保留范围 |
|---|---|---|
| S_b8d Gate18 | 35 阶段全部通过，两个退出回执 0 | 归属 `b8d94f5…`，不移到 S_cf358 |
| S_b8d N18／runtime18 | N18 只读证明通过；runtime 6 transport＋12 private 全通过 | 不证明新依赖与新补丁的下游运行 |
| S_b8d UI24 首轮 | 4 启动、3 通过、第 4 项失败、20 未运行；264 父检查 | 五个 child 用例另列，不把此次库回归当 UI 补验 |
| S_cf358 Gate19 | 4 通过、private 失败、30 未运行 | 本文真实终态，2102 passed／2 failed 单独记录 |

旧 Gate18 的完整阶段表见 [通过报告](current-iteration-Gate18-passed.md)，旧 runtime／UI 的失败细节见 [首次结果报告](current-iteration-runtime18-UI24-failure.md)。更早 Gate16 同样是 4／1／30，但它因栈溢出没有完整库汇总；本次 Gate19 已有明确的父 `FAILED` 汇总和两个失败名，二者不能混写。Gate17 的 Windows 失败与 W 的 UI 账户兼容失败也继续保留在历史报告中。

旧本地 relay／socket 结果与生产环境的边界仍按历史记录保留。Computer Use 仍是应用内许可，`os_permission_boundary=false`，此次库结果没有建立 OS 权限隔离。

## 原件身份与读取范围

| 原件 | 字节数 | SHA-256 | 报告组实际读取 |
|---|---:|---|---|
| binding-manifest.json | 16562 | `a4ebb9f9023af2ba65276eeee0c236aa94915f176306f69b85292a050095fbf6` | 完整文件哈希；来源、原控制器合同与固定输入等选定字段，未重跑字节／AST 比较 |
| 外层 owned/launch.json | 1007 | `f61c629908503d13f23ccfc62d2ef4aaf363077918ba71eb6191fd9b3d8e38ac` | 完整原件读取 |
| 内层 launch.json | 3636 | `ea743796fc6c2336ef4155a367d11807048d24135858d7adc4c272fb7eedc78f` | 完整原件读取 |
| candidate-gates.json | 934590 | `da18fa551856991da17b4cfd9f93b91c7f63b287b843d95836da77f79a07126f` | 完整原件哈希；绑定、终态、时间、阶段与父汇总等字段 |
| 内层 exit.json | 4862 | `47938143b5499680bb7fa47134fd1d8d061419763c81e74bb71a031755e490e2` | 完整原件读取 |
| 外层 owned/exit.json | 1171 | `879375e853aff7b72e89535bd90e31e59ff6c2a06a36c29adc6c51ca81a097eb` | 完整原件读取 |
| library-private 原日志 | 568953 | `60ee3d5aefad1984a4e487e74213dbd9452ccb5121a546d5a8c8650784877cad` | 完整原件哈希；两个失败、父／子汇总及三个新回归的原行 |

完整文件计算哈希与逐行阅读是不同范围。报告组完整读回内外 launch／exit；934590 字节 Gate 原件读取绑定、终态、阶段和父汇总等选定字段；568953 字节 private 日志读取两个失败诊断、父／子汇总与三个新回归原行，没有声称逐行审计全部用例。两份策略日志分别为 22851、5661 字节，报告组核对完整哈希并读取父 unittest 汇总与选定尾部，详细身份见事实 JSON。

冻结与对象预取资料只读取原件身份和选定时间字段，完整摘要保留在事实 JSON；报告组没有新执行对象预取、构建或验收。

本稿公开结果与必要来源，不复制私人路径、会话身份或原日志中的夹具载荷。后继诊断结论、源码修复及执行结果将分别记录；在新原件出现前，当前状态保持为 **Gate19 失败、后续消费者未执行**。[完整公开事实字段](gate-Scf358-r19-failure-public-facts.json)

