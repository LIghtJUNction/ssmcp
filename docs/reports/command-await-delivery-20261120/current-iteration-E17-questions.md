# SSMCP 0.10.25：Questions 页面导航修复已合并，Gate19 待运行

**记录截止：2026-11-21 12:57:52 UTC。** [PR #172](https://github.com/LIghtJUNction/ssmcp/pull/172) 已于 **12:56:33 UTC** 正常合并。当前主分支为 **`cf358f00f84b73e9892e62dd9b50b0dcc218201c`**，tree 为 **`a9b283173ee46cff86ca4a119566f0ebc488292a`**；真实父提交为 **`9362d2232e1ca57d976308b10df6ed6b4b167332`** 与 **`183d7ada89d88e53b30c4d4bc8c4aef62c1e8d91`**。报告组分别读取了 PR、实际合并提交和 main，合并树与完整补丁均和作者版本一致。[E17 来源事实](source-E17-questions-public-facts.json) · [实际合并提交](https://github.com/LIghtJUNction/ssmcp/commit/cf358f00f84b73e9892e62dd9b50b0dcc218201c)

E17 原作者阶段实际运行于 **12:45:10.507521–12:51:09.343402 UTC**，退出 0；作者提交为 183d7ad。作者完成与随后正常合并均已核实，**尚没有新源码的编译、测试或 Gate19 运行结果**。先前 PR open 的快照已由上述实际合并原件更新。

本次补丁使 Questions **列表页**的左右方向键切换相邻面板，空列表也可以离开；Enter 继续用于打开选中请求，详情编辑和 Finish 的显式 Enter 确认路径保留。两项新增键盘回归只完成定义，**未编译、未执行**。最初格式检查退出 1，修正四处断言排版后，最终格式与 diff 检查均为 0；113 条 Git 记录不是 113 项测试。

## 用户反馈与源码路径：右方向键原先被解释成“打开请求”

源码协调者收到的反馈是：Questions 显示 **5/8**，继续按右方向键却不能到 **6/8**。据协调者描述，截图同时显示没有待处理请求。报告组没有独立读取该截图，其所用可执行文件与源码 SHA 也尚未绑定；因此不能将截图直接写成作者提交或某个主分支上的运行复现。

固定源码的静态检查发现一条与该现象相符的路径：

1. 终端的 CSI／SS3 左右方向键编码，被解码成内部的 Back／Open 操作。
2. 通用横向面板切换路径跳过 Questions，转交 Questions 自己处理。
3. Questions 列表原先将 Open 用于打开选中问题；空列表没有可打开的条目，于是页面留在原处。

从这条静态路径看，右键会被解释为“打开请求”；列表无条目时，这项操作不会改变页面。这是固定源码支持的解释路径，仍不等同于已对截图现场复现并确认根因。[固定作者补丁](https://github.com/LIghtJUNction/ssmcp/commit/183d7ada89d88e53b30c4d4bc8c4aef62c1e8d91)

## 新路由：列表负责翻页，详情保持原编辑操作

新分支放在既有详情处理分支之前：当处于 `!detail` 的列表状态时，Back／Open 分别进入 `canvas_move(-1)`／`canvas_move(+1)`。该条件不依赖 `content_focus`，适用于空列表与有内容的列表；底部也加入面板导航提示。

| 所在状态或按键 | 补丁中的行为 | 没有改变的含义 |
|---|---|---|
| Questions 列表，左／右方向键 | 切换前一个／后一个面板 | 不因空列表而必须先找到一条问题 |
| Questions 列表，Enter | 打开选中的请求 | 方向键不代替 Enter 打开请求 |
| 请求详情编辑器，方向键 | 继续走既有详情编辑分支 | 不把详情中的方向键一概变成面板跳转 |
| Finish 详情的选择与确认 | 保留选择路径和显式 Enter 确认 | 切换面板不会自动作出批准 |
| Esc／Alt 方向键、共享主草稿 | 保留原路径 | 不借此修改草稿、收件者或发送语义 |

面板切换与问题详情编辑是两个不同操作。E17 只在列表状态重新分配左右键的含义；它没有全局重写键盘解码器、canvas 或聊天提交入口。[当前作者版 Questions 源码](https://github.com/LIghtJUNction/ssmcp/blob/183d7ada89d88e53b30c4d4bc8c4aef62c1e8d91/src/client/terminal/conversation/questions.rs)

这些行为来自静态补丁审阅。作者阶段没有运行终端或真实按键回归，因此“代码已改为此路由”与“用户页面已实测恢复切换”仍分别记录。

## 两个文件、两项未执行的键盘回归

| 文件 | 增加／删除 | 作用 |
|---|---:|---|
| `src/client/terminal/conversation/questions.rs` | 3／1 | 列表方向键路由及导航提示 |
| `src/client/terminal/conversation/questions/history_tests.rs` | 99／0 | 新增两项回归定义 |
| **合计** | **102／1** | 没有其他产品路径变更 |

新增用例位于 `client::terminal::conversation::questions::history_tests`：

- `full_keyboard_empty_questions_list_arrows_move_between_panels`
- `full_keyboard_questions_list_arrows_preserve_selection_until_enter`

定义从完整 `chat_keyboard` 按键入口进入，使用隔离的队列与历史。它们覆盖 CSI、SS3 两类方向键编码和两种 focus 值，检查空列表的相邻导航、有内容列表在按 Enter 前保留选择，以及主草稿光标、收件者、队列和发送状态；还检查 pending 决定与回答草稿的保留。原有测试定义字节未改。[回归定义源码](https://github.com/LIghtJUNction/ssmcp/blob/183d7ada89d88e53b30c4d4bc8c4aef62c1e8d91/src/client/terminal/conversation/questions/history_tests.rs)

**这些是已写入的断言与覆盖意图，不是执行结果。** 两个新用例尚未编译，也没有实际按键、PTY 或 UI 验收成绩。它们计划进入原有 private 库阶段，不另加一套验收门槛。

## 作者阶段实际做过的检查

| 步骤 | 实际结果 | 结果范围 |
|---|---|---|
| 首次两文件 `rustfmt --check` | 退出 1 | 四处 `assert_eq` 表达式需要多行排版 |
| 同轮 `git diff --check` | 退出 0 | diff 格式检查完成 |
| 格式调整 | 退出 0 | 修正上述排版 |
| 最终 `rustfmt --check --edition 2024 --config skip_children=true` | 两个修改文件检查退出 0，空输出 | 格式通过，不是编译或测试 |
| 最终 `git diff --check` | 退出 0，空输出 | diff 格式通过 |
| 提交后 113 条 Git 回执记录 | 全部退出 0 | 静态 Git 记录，不是用例数 |
| 固定源码静态审阅 | 报告组与直接源码审阅者为 READY | 不等于运行验收 READY |
| Cargo、作者检查 helper、编译、测试、产品与 Gate | 未执行 | 没有对应新成绩 |

作者沿原会话继续，作者与 rustc 包装器身份保持；报告组没有启动新模型。作者先快进到当时的 main 9362d22，树与 E16 作者最终树相同，再形成 183d7ad；不能因为这次快进的树相同，就把后续新增 Questions 补丁当成已经测试过。

一次普通推送退出 0，远端作者头为 183d7ad；推送当时的记录中，main 为 9362d22。凭据存储 D-Bus 告警保留为非致命现象，没有冒充推送失败。工作树、索引和四个子模块均记录为干净，包版本仍为 **0.10.25**。

## E16 与依赖来源保留，未产生新运行证明

E16 的 `src/computer/zuse_backend.rs` 在 E17 中逐字保留：**99165 字节**，SHA-256 **`8d46d29815fc096e560efee0e9ffbf4336b48c1605c98c1723835ef02f0113e6`**。取消状态锁与 submission 锁分离的补丁、新回归定义以及原生错误保留路径没有再改。

E16 的限定条件仍适用：cancel 返回表示取消状态已发布，不表示已接纳的 native 调用或清理结束；最后一次检查与 native 调用并不原子化。原 0.7 秒 EOF 夹具条件没有放宽。**E17 没有编译或测试 E16，不能把文件哈希相同当成故障已经运行验证解决。**[E16 合并报告](current-iteration-E16-merged.md)

Cargo.toml、Cargo.lock、四个 gitlink、十一项内部 Git 来源和六份发布配方均与固定 main 保持一致。终端主键盘文件、conversation 主文件、canvas、worker E2E 夹具及 host-control 文件也未改。本轮没有再次更新依赖或追逐上游分支头。

### zuse：历史 main 来源与当前分支头是两种观察

本候选继续锁定 **`8d37a7df6fd94df1d0939eb6800c238e71f5835d`**，包名 `zuse-computer`、版本 **0.3.0**。新补齐的来源证据说明它曾是上游默认分支 main 的合并结果，而不是证明它在之后每个时点都仍为 main 最新头。

| 证据时间 | 实际含义 |
|---|---|
| **11:57:18 UTC** | 上游合并事件的生效时间 |
| **12:44:42–12:44:47 UTC** | 源码协调者实际读取 GitHub PR／事件等证据的时间 |
| **12:01:38 UTC** | 本地保存的 ac5713→8d37 快进历史记录时间 |
| **12:46:20.216987 UTC** | 源码协调者读取该本地历史的时间 |

**11:57 的事件时间不能写成 11:57 就已经完成了这次 GET 观察。** 报告组也独立读取了上游 PR 与合并事件身份；本地历史只作为另一个有明确读取时点的来源记录。[上游 PR #5](https://github.com/LIghtJUNction/zuse/pull/5) · [合并事件原接口](https://api.github.com/repos/LIghtJUNction/zuse/issues/events/36857051430)

同轮单独读取的 current head 是 **`0016f96067d1e4672c62be9d8008d210edcdae2d`**，与候选 pin 不同。比较记录为 ahead 5、behind 0、merge base 8d37，且包含合并侧分支；不能将它写成五个连续第一父提交，也不能拿当前祖先关系代替过去的 main 事件证据。E17 **没有**把依赖改到 0016，也不声称 8d37 在本快照仍是最新上游头。[本轮来源字段](source-E17-questions-public-facts.json)

## 原成绩仍归属旧 S_b8d

| 源码及范围 | 已发生的结果 | 本稿边界 |
|---|---|---|
| S_b8d Gate18 | 原 35 阶段全通过 | 属于 `b8d94f5…`，不转移到 E16／E17 |
| S_b8d N18 | 只读来源证明通过，0 产品测试、0 Cargo | 不证明新依赖与新补丁已编译 |
| S_b8d runtime18 | 6 transport＋12 private MCP 全通过 | 是受控输入／收件者运行检查，不是生产 relay ACK |
| S_b8d UI24 首轮 | 4 个启动、3 个通过、第 4 个 zuse_worker 失败、20 个未运行；alias 0 | 53＋137＋74＝264 个父检查；五个 child 用例另记、不相加 |
| E16／E17 新源码 | 源码与格式／Git 记录已核对 | 尚无新源码 Gate、runtime 或 UI 执行成绩 |

旧 worker 报错为 `timed out: worker observed cancellation EOF`，父日志是 child 返回 1 引发的 `CalledProcessError`；worker suite 为 4.426 秒、whole-text child 为 1.631 秒，不是 120／900 秒外层超时。修复定义、静态审阅和格式通过均不能替代后继运行结果。[runtime18／UI24 首次结果](current-iteration-runtime18-UI24-failure.md)

更早 Gate16 的 4 通过／1 栈溢出失败／30 未运行、Gate17 的 7 通过／1 Windows GNU lint 失败／27 未运行，以及 W UI 的 503 个父检查与账户兼容失败继续保存。后来的局部诊断和新源码不回写这些历史。[Gate18 与历史完整报告](current-iteration-Gate18-passed.md)

## 下一步范围：Gate19 待运行，原定验收范围不变

源码协调者已记录用户希望发布的请求，报告组读到了检查点中的请求记录，未独立读回原始交付消息。E17 已正常合并，原发布目标保留。源码协调者最新通报：**Gate19 目前仅为 binder 准备，尚未绑定或启动**；精确 8d37 对象与最终源码冻结仍在准备，没有据此报告编译、产品或新增阶段执行。原 **35 阶段**不变，两个 Questions 回归与 E16 回归计划使用现有 private 库阶段，不新建检查或额外门槛。

原 runtime、UI、controls、多 UID、版本命令、P1 与两项 native 补充保持原消费者范围，等待新源码的正式交接。没有因为旧 S_b8d 通过了某些分区就先填入新候选成绩。

H2-CI 三个固定模块仍属于 [6f6bb09 静态交付](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)。配方未变不等于核验执行：目前仍 **0 H2 import／execute、0 release dispatch、0 安装／生产 reload／部署**。crate publication 与生产 relay 部署的延期是独立事项，均不由源码任务退出 0 自动解除。

统一 Computer Use 开关仍为应用内许可，`os_permission_boundary=false`；本次页面导航修复不改变这个边界，没有修改生产账户或主机权限。旧 none 本地 relay 成绩、原 socket 失败与实际用户 PATH 入口的未知范围也按原记录保留。

### 恢复资料只说明最后一次实读状态

报告组最后独立读取的恢复检查点仍是 **12:29:58.915519 UTC** 保存的 E16-start 快照：**18692 字节**，SHA-256 `fde3655b67726ad74dbf7d78613ed03f3f9676a2fddc3630de6ccfefb5739622`。它不是当前 E17 阶段状态，也不能被称为“当前最新检查点”。

那次读取时脚本和配置未变，入口仍待触发，completed 与相关日志不存在；即使将来写出启动标记，脚本也只会记录 `connection_confirmed=false`，不能由进程存活推断连接已确认。本稿没有读取更新的恢复原件，不能把资料准备、当前连接可用或 E17 正常退出写成系统已重启并恢复会话。[恢复记录范围](current-iteration-E16-merged.md)

## 原件身份与读取范围

| 原件 | 字节数 | SHA-256 | 报告组读取 |
|---|---:|---|---|
| launch-receipt.json | 1937 | `2bc2f79985c89f298071953a2ae0e8c97a4b95a3782209f63becf046981bc73e` | 完整原件读取 |
| author-exit.json | 2097 | `61cbcbb457afba322749f791fbbfadcc081ee5dc640fe67fcf59fc0302647b3e` | 完整原件读取 |
| source-changes-receipt.json | 56669 | `f7019b48443b840a08ce5f160989b1a845684fb65da32bc8cf498c626b266c4a` | 完整文件哈希；身份、范围、保留项及完整格式／提交／推送记录；113 个 Git 返回码已汇总，未逐条独审全部输出 |
| author-final.full.diff／author-own-fixes.full.diff（各一份） | 6267 | `23148a6905ec385c155315ffce2f70dd0fb906e403cd94b3ad73a891d5df18c4` | 两份均为所列大小与哈希；身份由源码回执记录，实际完整补丁另从固定 GitHub 提交读取 |

| 修改后文件 | 字节数 | SHA-256 |
|---|---:|---|
| src/client/terminal/conversation/questions.rs | 41765 | `c82535139da86bc3d571bce93f90c4f16f8de5b9d8922f2608625d8c4a470c35` |
| src/client/terminal/conversation/questions/history_tests.rs | 12034 | `51795f3759c4114e37bf39083ae3eb3856bbf7d0601cf2b69da1fb5315277125` |

完整原件哈希不等于全文逐项审阅。报告组实际完整读取 launch／exit、格式和推送等记录；源码回执的来源、范围、保留项及返回码按所列范围核对，完整补丁另从固定 GitHub 作者提交读取。公开稿不复制私人 HOME、native 会话身份、凭据或截图路径。[完整公开事实字段](source-E17-questions-public-facts.json)

报告组保持 **0 Cargo／metadata／构建／产品／测试／target 写入和排队重任务**，仅完成本地文稿。实际合并已由原件确认；Gate19 启动、回归通过和发布结果，仍须由后续各自的原件分别更新。

