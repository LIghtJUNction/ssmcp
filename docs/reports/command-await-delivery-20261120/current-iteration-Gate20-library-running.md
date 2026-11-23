# SSMCP 0.10.25：Gate20 库测试通过，完整验收继续

**记录截止：2026-11-21 13:58:09 UTC。原 Gate20 仍为 running，35 个计划阶段中已有 6 个完整阶段退出 0，内外两份结束回执尚不存在。** 当前源码为 `02aa4dcd18786062ef44be6c76422dff054fbfed`，tree `167e9976d2ac334fb3dfbd58f82288baf0eb9b17`，对应已实际合并的 [PR173](https://github.com/LIghtJUNction/ssmcp/pull/173)。本轮 run ID 为 `2f9197c074824f45545f28952342f25d`。[本轮运行事实](gate-S02aa-r20-library-running-public-facts.json)

新库阶段的父汇总是 **2104 passed、0 failed、26 ignored、6 filtered**。上一轮失败的两个 summary 场景已经在新的测试条件下通过；这不把旧 1 秒条件改写为通过，也不等于整套验收或 UI 已经完成。

## 实际完成的六个阶段

| 阶段 | 退出码 | 阶段耗时 |
|---|---:|---:|
| release-policy | 0 | 10.62 秒 |
| deployment-policy | 0 | 0.62 秒 |
| fmt | 0 | 4.66 秒 |
| library-compile | 0 | 278.12 秒 |
| library-private | 0 | 78.77 秒 |
| library-host | 0 | 0.05 秒 |

private 库父汇总耗时为 **78.72 秒**，其外层阶段耗时为 **78.77 秒**。日志中另外有三个子进程各自的 1 passed／2135 filtered 汇总；这些不能再加到父级 2104 中。host 阶段本次只读取到了退出码 0 与 0.05 秒阶段耗时，尚未读取该日志中的父测试数量，因此不在这里补写它的实际用例总数。

报告组从原 private 日志读到了以下七条测试的实际 `ok`，它们全部已经包含在父级 2104 中：

| 已核对的用例组 | 条目数 | 本次可支持的结论 |
|---|---:|---|
| E18 public await、code-mode waitFor、probe 后撤权三个场景 | 3 | 新 5 秒目标输入下，既有成功与撤权断言通过 |
| 原超时、续等、取消及不重复消费场景 | 1 | 保留原 1 秒输入的这项合同通过 |
| E16 原生提交阻塞时取消及保留原错误 | 1 | 该库回归通过，尚非实机取消 EOF 验证 |
| E17 空 Questions 与非空 Questions 键盘导航 | 2 | 两项库回归通过，尚非真实终端 UI 验证 |

已完成的阶段之外，其他阶段还没有完整结果；不能据此把它们全部标作未启动。原执行者另报告随后进入 clippy；本快照不预填它的结果。

## 测试条件的变化与保留

E18 只修改 `src/http/question_delivery_tests.rs`，117 行增加、36 行删除，未改生产代码。三个成功／撤权场景的目标输入由 **1000 ms 改为 5000 ms**，观察 guard 由该输入、现有 collector 的 1000 ms 和 3000 ms 余量组成。**9000 ms 只约束被包围的观察阶段，不是整测试墙钟上限**：夹具准备及前十次调用在 deadline 之前，撤权调用和 abort 后的 join 也没有全部纳入它。OAuth 与 agent 两轮各自建立 deadline。

code-mode 外层输入仍为 10000 ms；默认 public await helper 仍为 5 秒；原 timeout／resume／cancel 测试仍使用 1000 ms。成功仍要求 `ready`，撤权仍要求 `unauthorized` 且无结果数据／summary，11 次调用、单 wait／probe、ACK 及不消费用户输入等断言保留。本轮已核对的库日志提供了实际结果，但仍不能据此声称生产性能根因已经解决。[固定夹具源码](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/http/question_delivery_tests.rs)

原作者阶段于 13:32:04.273349–13:38:45.338024 UTC 完成，退出 0；该阶段只进行了编辑、格式与 Git 检查，没有编译或执行测试。随后的 Gate20 才产生本报告中的编译和库测试结果。完整作者回执、首次格式失败及修正、117 条 Git 记录和实际合并身份保留在 [E18 报告](current-iteration-E18-summary-fixture.md)及 [E18 来源事实](source-E18-summary-fixture-merged-public-facts.json)。

## 来源、检查范围和输入一致性

报告组已读取当前 binding 中的实际阶段合同：**35 阶段、26 个 Rust 结果阶段、20 个 integration targets、14 项必跑 ignored**。这些是检查范围，不是已经执行完成的数量。binding 记录了原模板逆向字节与归一化 AST 相同、未复制旧成绩；报告组没有重新执行这些比较。外层仅增加 `PYTHONDONTWRITEBYTECODE=1`。

在 **13:58:09.264221 UTC**，报告组独立读取并重算了九项固定输入的 SHA-256，全部与 launch before 及 binding expected 相同。此项读取是在此前 busy 拒绝之后单独成功完成的；早期拒绝不算成功，也不是产品测试失败。**本次是运行中一致性检查，仍没有结束后的哈希比较或最终 Gate 结论。** [九项输入与绑定字段](gate-S02aa-r20-library-running-public-facts.json)

原 source／target 执行窗口继续由原执行者负责。报告组只做必要的回执读取、固定源码核对和文档更新，保持 0 Cargo／metadata／构建／产品／测试／target 写入／H2／dispatch，不增加新的验收任务。

## `/send` 与回执：当前源码口径

本节适用于固定源码 `02aa4dcd18786062ef44be6c76422dff054fbfed`。当前原生收件人与群组都支持 `/send <正文>`；历史材料中“native 目标拒绝 `/send`”的描述不能继续作为当前行为说明。

| 当前目标 | `/send` 的正文处理 | 引用补全 |
|---|---|---|
| 本地 listener | 进入本地 `prepare_references` 流程，再提交给已选监听目标 | 按原有本地输入条件处理 |
| 原生收件人 | 以字面正文交给对应消息传输路径，不隐式读取本机 `@` 文件或 `$` 技能 | 原生收件人模式关闭引用补全 |
| 群组 | 将正文交给所选群组的消息传输路径，不隐式读取本机文件或技能 | 群组输入识别为 `/send` 时关闭；不能扩大为群组下所有普通输入均关闭 |

目标分流发生在创建本地引用准备任务之前；发送仍经过既有的收件人、账户、连接／预览及待回执条件检查。[目标分流与本地准备](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/listener.rs#L33-L155)；[原生正文传递](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/messaging.rs#L544-L654)；[群组正文传递](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/groups.rs#L226-L287)；[补全条件](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation.rs#L959-L975)。

回执描述消息到了哪一步：`queued` 是服务已排队、等待收件人调用；`included` 是消息已纳入收件人的工具响应；`acknowledged` 是收件人确认。它们均不能单独证明对方已经阅读、理解或执行完毕；源码还区分了消息已纳入响应但连接关闭、无法取得确认的情况。[回执文案](https://github.com/LIghtJUNction/ssmcp/blob/02aa4dcd18786062ef44be6c76422dff054fbfed/src/client/terminal/conversation/messaging.rs#L121-L139)

以上是固定源码的分流、正文传递、补全和回执文案核对，没有增加代码或测试任务，也不是新的后台投递或端到端运行结果。

## 旧失败、后续验收与发布

旧 Gate19 仍绑定 `cf358f00f84b73e9892e62dd9b50b0dcc218201c`，保留 **4 通过、1 失败、30 未运行；private 为 2102 passed、2 failed、26 ignored、6 filtered**。第一个失败是 expected ready／actual timed_out；第二个是在 probe 到达屏障等待时 Elapsed，失败迭代尚未执行随后撤权动作。这些历史结果不被本轮新预算下的通过覆盖。[Gate19 原失败报告](current-iteration-Gate19-failure.md)

更早 S_b8d 上的 Gate18、N18、runtime18 和部分 UI 结果各自保留来源，不转移到当前源码。当前还没有原 Gate20 的最终结果，也没有 N20、runtime、UI24、controls、多 UID、version_all、P1 或两项 native 补充的本轮通过结论。后续按原定范围和交接顺序继续。

GitHub 新版本发布请求持续有效。报告组没有执行 H2、创建 tag、dispatch release、安装、生产 reload 或部署；正式发布仍等待原执行者完成既有流程。Computer Use 的 `os_permission_boundary=false` 及现有应用内许可范围没有因本次测试或文档调整而改变。

## 原件身份与读取范围

| 原件 | 字节数 | SHA-256 | 本次使用范围 |
|---|---:|---|---|
| running Gate 快照 | 936680 | `9228d50dc4e625e188c34f4fc55bb7173e2bc05427ab9d9ceb7f5c431fc0d038` | 全文件哈希；来源、状态与六阶段字段 |
| private 库日志 | 565684 | `5a35d6f413635db4d504ceaaf47f570ea5b90a663e0f4dcf837996bc932cb930` | 全文件哈希；父／子汇总及七条测试行，不是全部 stdout 逐行审计 |
| 内层 launch | 3636 | `7c7596d9f1b33bc3ea0cff99bf3bf45e686432960b75b806bf11010af3919fe2` | 此前全文读取 |
| 外层 launch | 1008 | `5d7b288c0e38878bc288c2bd85acef747912153e0f19395d33a0b5f4e71d9ad3` | 此前全文读取 |
| binding | 16561 | `8918b7e3b1b9b23a1c52ba00bcdb6ff3c65ee9b013ea5f356e8d121c281e96be` | 全文件哈希；本次补读 stage contract 与指定控制字段 |

Gate 文件仍在变化，表中摘要仅对应 13:58:09 的快照。此前 [13:52:57 的 E18／Gate20 启动报告](https://github.com/LIghtJUNction/ssmcp/blob/82c20c73eb3892dc41a1f01232b46793bb34b856/docs/reports/command-await-delivery-20261120/current-iteration-E18-summary-fixture.md) 保持原样；新的读取范围和结果在本报告另行记录。

