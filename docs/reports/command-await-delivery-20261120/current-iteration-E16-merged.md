# SSMCP 0.10.25：E16 取消 EOF 源码修复已合并，新候选仍待原定验收

**E16 固定来源观察截止：2026-11-21 12:38 UTC；新增用户反馈通报截止：12:43 UTC。** [PR #171](https://github.com/LIghtJUNction/ssmcp/pull/171) 已于 **12:34:56 UTC** 正常合并。当前提交为 **`9362d2232e1ca57d976308b10df6ed6b4b167332`**，tree 为 **`ac58ef7ea25b69033c1f84d69d3ff614f6385473`**。E16 针对上轮 worker 取消 EOF 观察失败调整了提交与取消的锁，并新增一项回归定义；**本阶段没有运行 Cargo、编译、Rust 测试、产品或验收**。[E16 来源事实](source-E16-worker-eof-merged-public-facts.json)

原作者阶段实际退出 0，格式检查和 `git diff --check` 均退出 0，96 条 Git 记录也都退出 0。**这些记录不等于 96 项测试**，退出 0 也不表示 worker 故障已经通过运行复验。当前仍待新源码上的原定 Gate19 与后续消费者结果；尚无可报告的 Gate19 通过结果。末节另记随后收到的新用户反馈及源码协调者通报，不把它们写成已复现或已修复。

旧 S_b8d 的 Gate18／N18／runtime18 通过结果继续有效地归属于旧提交；UI24 首轮第 4 项失败、264 项父检查通过、20 项未运行的记录也完整保留。当前候选同时包含 PR170 的 zuse 来源更新和 E16 补丁，不能直接继承旧二进制的验收结论。

## 合并来源：一项自身修复，保留已合入依赖更新

原作者于 **12:27:58.216851 UTC** 沿原会话开始，**12:32:05.807571 UTC** 正常退出，error 为 null。作者和 rustc 包装器身份保持；报告组没有另启模型或运行产品。

| 身份 | 固定提交或树 | 含义 |
|---|---|---|
| 作者原起点 | `bef89f57aea0167b830096d596a2cb39fd86f5cd` | 原 E15 作者提交 |
| 快进后的基线 D | `dae9d52c734b5d316bf90d2317a51e3992f68085` | 已包含正常合并的 PR170 |
| E16 作者提交 | `561ef5a94763877893b0575e04b6da480761499c` | 只修改 `src/computer/zuse_backend.rs` |
| 当前主分支合并提交 | `9362d2232e1ca57d976308b10df6ed6b4b167332` | 真实父提交为 D 与 E16 作者提交 |
| 作者最终树／合并树 | `ac58ef7ea25b69033c1f84d69d3ff614f6385473` | 树相同，合并补丁与作者补丁相同 |

E16 **自身补丁只有一个路径，56 行增加、8 行删除**，其中包括新回归定义。相对于作者旧起点，整个阶段还带入 PR170 的 `Cargo.lock` 和 `crates/zuse`，合计三个路径；这两项不能写成 E16 又独立修改了一遍依赖。作者普通推送一次，退出 0，没有强推。推送时出现的凭据存储 D-Bus 告警未阻止推送，远端作者提交身份另由 GitHub 读取确认。[作者固定补丁](https://github.com/LIghtJUNction/ssmcp/commit/561ef5a94763877893b0575e04b6da480761499c) · [实际合并提交](https://github.com/LIghtJUNction/ssmcp/commit/9362d2232e1ca57d976308b10df6ed6b4b167332)

工作树、索引和四个子模块记录为干净，SSMCP 包版本仍为 **0.10.25**。Git commit 时间为 12:34:55 UTC，PR 合并时间为 12:34:56 UTC，二者分别记录。

## 修复改变了什么：取消通知不再排在原生操作的长锁后面

取消有两个不同阶段：**先让其他线程知道“不再接收后续动作”，再等待已接纳动作与必要清理结束**。把取消状态写成功，不等于原生调用已经返回，更不等于所有按键、指针或清理动作都已结束。

按固定源码的静态审阅，原路径在执行原生提交期间持有取消状态锁。原生操作若持续阻塞，另一个线程要发布取消状态，也可能先等这把锁。E16 将“原生提交必须串行”与“取消状态需要同步”拆成两把职责不同的锁：

| 锁或检查 | E16 中的职责 | 保留的边界 |
|---|---|---|
| submission mutex | 串行覆盖 ticket 等待与完整原生提交 | 没有把原生输入改成可同时提交 |
| cancellation state mutex | 短暂读取或写入取消状态，不再跨原生操作长期持有 | 发布取消与等待操作结束是两件事 |
| ticket 闭包内的取消检查 | 在提交路径再次读取取消状态 | 最后一次检查与真正原生调用之间仍不是原子操作 |

因此，修复的直接目标是让取消状态的发布不必等待正在执行的原生提交释放同一把状态锁。**它没有承诺强行中断已被接纳的 native 调用，也没有把 cancel 返回当成 cleanup 完成。** 最后检查与原生调用之间仍存在并发窗口，不能写成“cancel 返回后绝不会再出现输入”或“所有取消路径立即生效”。

原生调用的原始结果仍保留；delay 的条件变量关系、X11 的不重试规则与清理协议未改。观察类取消保留原行为，本补丁不把所有取消入口统一成即时取消。上述是源码层面的具体机制与限制，**尚没有绑定本次新提交的运行复验，不能据此宣告上轮失败已消失**。[当前固定源码](https://github.com/LIghtJUNction/ssmcp/blob/9362d2232e1ca57d976308b10df6ed6b4b167332/src/computer/zuse_backend.rs) · [静态审阅记录](worker-cancellation-eof-static-review.md)

## 新回归只完成定义，未编译或执行

新用例为：

`cancellation_returns_while_native_submit_is_blocked_and_preserves_its_error`

它进入现有默认库测试范围，使用通道控制同一生产 submit 入口的时序：让原生提交闭包暂时阻塞，再发出取消，并观察取消是否可以先返回；随后释放阻塞、等待两个线程结束，再检查结果。定义还检查原已接纳调用的 native 错误是否保留，以及取消后的后续提交是否被拒绝。

这里有三项必须保留的限制：

- 两次通道等待结束后，无论是否发生超时，都先释放阻塞并 join 两个线程，再执行断言；这避免单靠失败断言遗留由夹具控制的阻塞线程。
- `join` 本身没有超时。这不是对任意实现、任意调度或所有线程路径“必定有界结束”的证明。
- 新定义没有声称覆盖“第二个提交已并发排队”的所有交错；目前也没有任何实际执行成绩。

该回归计划进入**原有 library-private 阶段**，不是另加一套验收门槛。原 `scripts/e2e_x11_text.py` 与 `scripts/e2e_zuse_worker.py` 保持原字节，既有 **0.7 秒**取消 EOF 夹具条件没有放宽，父 subprocess 的 120 秒和外层 suite 的 900 秒预算也没有因本次补丁修改。[未执行的回归与保留项](source-E16-worker-eof-merged-public-facts.json)

## 本阶段实际检查到哪里

| 记录 | 实际结果 | 能说明什么 |
|---|---|---|
| `rustfmt --check --edition 2024 --config skip_children=true src/computer/zuse_backend.rs` | 退出 0，空输出 | 修改文件符合格式要求 |
| `git diff --check` | 退出 0，空输出 | 差异未触发该格式检查 |
| 96 条 Git 回执记录 | 全部退出 0 | 提交、差异、引用等静态操作记录；不是测试数 |
| 固定补丁静态审阅 | 报告组与直接源码审阅者给出 READY | 锁职责、结果保留和限定范围已静态核对；不代表编译或运行通过 |
| 作者检查 helper、Cargo、编译、测试、产品、验收 | 未执行 | 本阶段没有可转移到新候选的执行成绩 |

新测试目前连编译结果也没有。原作者完成源码任务的退出 0，与产品／回归测试退出 0 是不同证据，报告分别记录。

### PR170 的 zuse 来源继续保留

[PR #170](https://github.com/LIghtJUNction/ssmcp/pull/170) 已将 zuse 从 `ac5713c2bfc3cc4848e89107cc1a6929ec48e2dc` 更新为 **`8d37a7df6fd94df1d0939eb6800c238e71f5835d`**，包版本仍为 **0.3.0**。E16 保留这一更新，子模块实际 checkout 与最终 gitlink 相同且干净。当前 `Cargo.lock` 为 **66476 字节**，SHA-256 `b8caf2e4f747094a0bcd12842b1fe7894d7d956efa1f8b756f17a16518b9d393`。

E16 相对于基线 D 没有改变 Cargo.toml、Cargo.lock、另外三个 gitlink、六份发布配方、相关 E2E 夹具或 host-control 文件。这里说明的是固定来源一致性，不是“上游永远最新”，也不是旧私有编译缓存已具备新对象。源码侧仍须按既有流程使用精确的 8d37 来源。[当前锁文件](https://github.com/LIghtJUNction/ssmcp/blob/9362d2232e1ca57d976308b10df6ed6b4b167332/Cargo.lock)

## 旧结果保留，新源码不继承旧成绩

| 源码与检查 | 已发生的结果 | 当前处理 |
|---|---|---|
| S_b8d Gate18 | 原 35 阶段全通过，两个结束回执均为 0，九输入前后相同 | 仍归属 `b8d94f5…`，不转记到当前 9362d22 |
| S_b8d N18 | 只读来源解析证明通过，0 产品测试、0 Cargo | 保留原证明，不证明新依赖与新补丁已编译 |
| S_b8d runtime18 | 6 transport＋12 private MCP 全通过 | 内层 8.524 秒、外层 9.220 秒是一次执行耗时，不是性能基准 |
| S_b8d UI24 首轮 | 4 个启动，3 个通过，第 4 个 zuse_worker 失败，20 个未运行；alias 0 | 父检查 53＋137＋74＝264；5 个 child 用例另记、不相加 |
| 当前 9362d22 | E16 源码正常合并，格式与 Git 检查完成 | 尚待新源码的原定 Gate19 与消费者执行结果 |

旧 UI 的失败为 **`timed out: worker observed cancellation EOF`**。worker suite 耗时 4.426 秒，whole-text child 耗时 1.631 秒，父日志为 `CalledProcessError`、child 退出 1；不是父 120 秒或外层 900 秒超时。四个 suite 的 parent-reaped 字段不独立证明全部后代回收。当前 E16 针对这条失败路径修复源码，仍须由后续真实结果确认效果。[上一份运行／UI 报告](current-iteration-runtime18-UI24-failure.md) · [S_b8d 原事实](runtime18-UI24-Sb8-first-result-public-facts.json)

更早的 Gate16 为 **4 通过、1 栈溢出失败、30 未运行**，没有完整库汇总；Gate17 为 **7 通过、1 Windows GNU lint 失败、27 未运行**。W 的 UI 首轮 **6 个通过、503 个父检查、账户兼容失败、17 个未运行**，之后单独 69 项诊断不覆盖原失败。这些历史继续保存在 [Gate18 完整报告](current-iteration-Gate18-passed.md)、[Gate16 事实](gate-S647-r16-failure-public-facts.json)与 [Gate17 事实](gate-S401-r17-failure-public-facts.json)。

旧 private 本地 relay 的 none 测试不证明生产 relay ACK 或部署完成；受控 socket 烟测不回填原入口 ENOENT。本轮源码合并也没有提供这些环境的新增运行结论。

## 当前执行、发布与恢复状态

源码协调者已经记录用户希望发布的请求；报告组读到了检查点中的请求记录，尚未独立读回原始交付消息。原发布目标继续保留，但**发布请求不是验收通过回执**。当前要等待既有新源码 Gate19 和消费者结果，再依据真实来源与执行结果更新状态；截至本快照没有正式 release dispatch、安装、生产重载或部署。

原 Gate19 仍是 **35 阶段**，新回归进入现有 private 库阶段，检查范围没有扩大。源码协调者最新通报：Gate19 尚未创建或启动；精确 zuse 对象预取的首次尝试因共享 target 锁忙退出 **75**、双 EOF、**零 mutation**。这只是准备步骤受阻，不是编译、测试或 Gate19 执行失败；报告组未据此启动任何替代任务。后续消费者仍暂停，等待原源码侧的正式交接。

H2-CI 三模块仍对应 [固定 6f6bb09](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)。此前本体哈希已匹配，当前仍未 import／execute、未绑定新的 S／P／run／dispatch。E16 没有更改六份配方；配方不变不等于核验器已运行。crate publication 延期与生产 relay 部署延期分别保留。

### 恢复检查点是启动时快照，不是当前运行状态

当前恢复用 `continuation.json` 为 **18692 字节**，SHA-256 **`fde3655b67726ad74dbf7d78613ed03f3f9676a2fddc3630de6ccfefb5739622`**，保存时间为 **12:29:58.915519 UTC**。它来源于 E16 已启动时的协调检查点，内容中的“E16 started”反映那个时点，**不能据此把已在 12:32:05 退出的 E16 写成仍在运行**。

恢复脚本和配置未改，仍处于待触发状态；即使将来写出启动标记，脚本也只会记录 `connection_confirmed=false`，不把进程存活当成连接确认。自然恢复未触发，报告组读取时 completed 与相关日志不存在。原 3289、6119、10806、12528 字节的检查点历史继续保留，不覆盖旧身份。更新上下文资料不证明系统已重启、图形窗口已打开或旧会话已恢复。[恢复与历史检查点范围](current-iteration-Gate18-passed.md)

统一 Computer Use 开关仍是应用内许可机制，**`os_permission_boundary=false`**。这次取消锁调整不建立操作系统权限隔离，不能承诺阻止同 UID 任意程序、独立 zuse 或外部插件绕过应用控制。没有修改生产账户、权限或安装隔离服务；后续设计仍见 [权限边界草案](computer-use-permission-boundary-design-draft.md)。

## 原件身份及本稿读取范围

以下区分报告组完整读取的原件、仅计算完整哈希并提取字段的原件，以及另从固定 GitHub 提交读取的实际补丁。原件可能包含私人环境信息，正文不复制本机 HOME、会话身份或凭据。

| 原件 | 字节数 | SHA-256 | 报告组读取范围 |
|---|---:|---|---|
| launch-receipt.json | 1919 | `77fa8f9879eb920e2cb343a9a50eb12da6c8219108fd44374d72874bdfa15fe3` | 完整原件 |
| author-exit.json | 2079 | `c27adc650853a5dc2548a253ecc0f2e5049e6aea7a707c6141c339e8daf2a08d` | 完整原件 |
| source-changes-receipt.json | 59586 | `49bf73f25ce08ba7a6f67ec1693b8bc0b3853ea0c423896a03cdb24987f4de3f` | 完整原件哈希；来源、变更、范围、清洁状态及完整格式／提交／推送记录；未逐条独审全部 Git 输出 |
| author-final.full.diff | 4724 | `86302de147c0b9f5faea27ba4e72fbf2cde18ef04cf63ef048deb9d32a1a1011` | 文件身份由源码回执记录；实际完整自有补丁另从固定 GitHub 提交读取 |
| author-own-fixes.full.diff | 3923 | `0c3375448fc6132ba1ed73a2e4cf2fd121c67c8c4ff0f70b6cac04f870e40dff` | 文件身份由源码回执记录；实际完整自有补丁另从固定 GitHub 提交读取 |

修改后 `src/computer/zuse_backend.rs` 为 **99165 字节**，blob 为 `f18d7e8f723140b06214b294d95f33c237fc414f`，SHA-256 `8d46d29815fc096e560efee0e9ffbf4336b48c1605c98c1723835ef02f0113e6`。这些来源身份可用于后续对照，但不是运行证明。完整公开字段见 [E16 合并事实](source-E16-worker-eof-merged-public-facts.json)。

## 新增用户反馈，待源码组定位

源码协调者随后收到并查看了新的用户截图与反馈：“这个页面我继续按右键，为什么不能切换到 6/8 页？”据协调者描述，截图显示 Questions **5/8**、**0 waiting／No requests yet**，底部仍有 pending question 提示。源码组正在只读检查按键路由。

截图所运行的可执行文件与源码 SHA 尚未绑定，不能把该症状归为 S9362 的已复现故障。本稿没有独立复现该操作，也没有确认根因或收到修复完成结果。若需要最小修复，仍由原作者会话处理，再统一进入原定验收；**目前不把 E17 写成已经启动或完成**。E16 已合并的状态与原发布目标均保持，不因这条新反馈虚构 Gate19 已启动。

报告组继续 **0 Cargo／metadata／构建／产品／测试／target 写入和排队重任务**，仅整理本地文稿；不运行 H2、不增加验收条件，也不另开源码作者或执行者。

