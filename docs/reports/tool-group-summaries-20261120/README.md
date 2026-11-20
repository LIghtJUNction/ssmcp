# ssmcp 工具组摘要：主线整合与验收

日期：2026-11-20

验证源码：`8725b7b4606e8eb75c3d366c380f991842e0219a`

账户退出整合源码：`4ca66fa12693b1b2db940da78b3c89cb2048425d`（补充验证见第 9 节）

功能入口：[PR #119](https://github.com/LIghtJUNction/ssmcp/pull/119)

本轮将工具组摘要、原生命令卡片和等待结果交付整合到同一份冻结源码。用户现在可以看清每组工具“正在做什么”，展开后仍能读到真实命令；明确折叠后，新结果和摘要更新不会把窗口再次打开。第 11 次真实调用产生的总结提醒，也能经被动等待响应交给 Agent。

本文更新此前 `911bfe9` checkpoint 的报告。第 4、5 节的完整构建、回归与截图对应 `8725b7b4606e8eb75c3d366c380f991842e0219a`；第 9 节单独记录后续账户退出整合。各阶段的成功数量与失败记录分别保存，不混入其他源码的结果。交付范围为源码和隔离验收，运行中的客户端与 relay 尚未部署这个候选。

## 1. 用户能看到的结果

没有摘要时，命令继续作为独立卡片显示真实 argv。即使 Agent 提供了用途标题，用户仍能看到实际执行的程序与参数。包含“读取文件 → 执行命令 → 再读取文件”的连续活动，也不会被切成多个共享同一组 ID、却重复显示全量计数的窗口。

设置摘要后，一个逻辑组对应一个窗口、一个摘要标题。窗口收齐属于它的调用，保留调用总数及成功、失败、运行状态；展开时复用原有命令标题行。未发生历史淘汰时，可见成员数与实际调用数一致；发生淘汰后，累计数量与保留成员分别表达，不能把剩余行数当成全部工作量。[实现：工具窗口与命令行][tool-activity]、[command_rows.rs][command-rows]。

含命令的组首次获得摘要时默认展开，方便直接核对 argv；用户此前已经明确折叠，则优先保留这个选择。之后的新调用、连续刷新、第二次摘要更新和位置恢复都遵守同一选择。普通摘要替换保留内外滚动、选中状态、未发送草稿与光标，不追加聊天正文。新增的折叠意图字段有默认值，旧位置记录可以继续读取。[实现：events.rs][events]、[摘要回归测试][summary-tests]。

## 2. Agent 如何总结，何时再次提醒

原生工具为 `ssmcp.ui.tool_summary`。调用方先读取实际客户端注册的 schema，再操作当前授权范围内的组：

| 操作 | 行为 |
| --- | --- |
| `list` | 返回最近最多 32 个可更新组，以及稳定 `group_id`、摘要、revision、累计与已覆盖调用数；截断时给出相应标记。 |
| `set` | 为准确的组 ID 设置或更新短摘要；可携带 `expected_revision`，版本冲突时拒绝覆盖。 |

摘要为非空、单行纯文本，最多 120 个 Unicode 字符、512 个 UTF-8 字节，并经过既有脱敏处理。计数与执行状态来自实际事件，摘要文本不能改写这些状态。[接口与参数校验][summary-interface]、[组状态与授权][summaries]。

开始、输出、结束属于同一请求时，只算一次。Agent 主动发起的新 `status`、`read`、`wait` 请求各算一次；摘要操作及内部观察不膨胀工作计数。累计 10 次不提醒，第 11 次提醒；成功设置摘要后记录当时的覆盖数量，再新增 11 次才需要更新。同一组、同一 revision 的提醒去重，提醒携带准确组 ID，避免让 Agent 猜测应总结哪个窗口。

授权同时核对原始会话所有权、连接、真实账号，以及已有 Agent／线程绑定。显示名称不能授权。未绑定 Agent 时，同一授权范围可以跨 code_mode 运行更新；绑定后必须精确匹配。重载保存摘要和显示位置，历史组恢复后只读，不恢复旧的编辑权限。新工具的完整契约见[工具组总结说明][summary-doc]。

## 3. 等待响应的交付边界

提醒由真实原生回执进入 relay，再附加到外层响应。JavaScript 自造同名字段不能建立可信提醒。首次尚未读取目录时，授权范围内的只读确认可验证客户端是否支持摘要工具；原生结果超过桥接帧上限时，仅释放对应请求、范围和 revision 的提醒预约，已完成的业务操作不会因此重放。[relay 交付实现][http-summaries]、[提醒恢复修复 PR #131](https://github.com/LIghtJUNction/ssmcp/pull/131)。

本轮补齐公开被动 `await` 的总结交付，同时保留两种等待入口各自的语义：

| 入口 | 总结提醒 | 其他输入交付 |
| --- | --- | --- |
| 公开 `await` 等待执行对象等被动目标 | 返回已获得的工具组提醒。 | 不领取待发送用户要求，不确认问题回答，不取走原生收件箱消息，也不推进会话提醒。 |
| code_mode 内的 `waitFor` | 经原有 code_mode 外层结果交付提醒。 | 继续遵循 code_mode 已有的授权、领取和确认边界。 |
| 明确等待 `user_input` | 采用已有用户输入交付流程。 | 以外层接收状态及对应消息作为交付证据。 |

公开 `await` 不占 JavaScript worker；`waitFor` 保留所在 worker。超时后使用原 `wait_id` 继续观察；新的原生等待请求按真实请求计数，取消只结束观察。最终只读复核可能发生等待，因此回传前再次检查账号、连接和绑定；若期间撤销授权，观察数据与提醒一并扣留。[等待实现][await]、[交付对照测试][delivery-tests]。

完整候选保留主线的 human-only finish 控制：结束请求需要本机用户明确同意，Agent 的 Yes、任务 ready、超时、其他 Agent 消息或总结提醒都不构成同意。已有许可按原生最终回答的规则消费；等待结束状态和工作结束许可是两个不同结果。[结束请求入口][finish]、[外层响应与工具说明][http]。

## 4. 同一份源码的验证结果

验收使用冻结源码及其构建产物，运行前后核对源码、子模块和二进制。构建使用锁定依赖、离线模式与单编译任务；不会把旧二进制的结果归入新候选。

| 构建检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --lib --no-run --locked --offline` | 通过 |
| `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | 通过 |
| `cargo build --bins --example vt_snapshot --locked --offline` | 通过 |

### Rust 回归

验证清单按 20 组筛选组织，覆盖摘要、事件分组、原生命令、等待与输入交付、结束控制，以及历史归属和重载等相关边界。各组实际结果如下：

| 筛选范围 | 通过 | 失败 |
| --- | ---: | ---: |
| 工具组摘要、计数、授权与提醒 | 34 | 0 |
| 终端事件、分组与显示归属 | 102 | 0 |
| Broker 原生交付 | 19 | 0 |
| Runtime 执行 | 10 | 0 |
| 原生 UI 工具目录 | 1 | 0 |
| 历史所有权 | 5 | 0 |
| 待发送撤回 | 1 | 0 |
| 跨会话重试与编辑 | 1 | 0 |
| 消息确认与修订号 | 1 | 0 |
| 投递与撤回竞争 | 1 | 0 |
| 历史归档 | 6 | 0 |
| 会话重载 | 5 | 0 |
| 会话切换后的晚到帧 | 1 | 0 |
| 仅真人批准完成 | 10 | 0 |
| 公共完成请求 | 3 | 0 |
| 公共类型化等待 | 26 | 0 |
| 协作等待 | 3 | 0 |
| 独立命令卡片 | 3 | 0 |
| code_mode waitFor 路由 | 1 | 0 |
| 问题交付与被动观察隔离 | 8 | 0 |

部分筛选会选中相同测试，因此不把逐组数量相加当成唯一测试总数；本报告也不声称执行了整个仓库的全部测试。

### 三个真实 PTY 套件

| 套件 | 通过检查 | 截图 | 主要证据 |
| --- | ---: | ---: | --- |
| 核心工具组摘要 | 68 | 6 | 10／11 边界、再次覆盖、版本与授权拒绝、真实失败调用、展开滚动及历史不增。 |
| readable 命令卡片 | 71 | 8 | 原生命令默认可读，命令与输出布局在宽窄终端下保持可用。 |
| 摘要与命令整合 | 90 | 4 | 纯命令与混合调用各一个窗口，计数准确，明确折叠后保持，实际 argv 可读。 |

三个套件均通过，共生成 **18 张截图**。测试通过真实原生工具、code_mode、WebSocket 和独立 PTY 执行；使用隔离账号与内容夹具，不启动外部模型，不重启现有生产服务。

草稿验证直接比较完整 `{text, cursor}`。核心与摘要命令套件输入 `DRAFT_STAYS intact`，将光标左移 3 字节，再在 120／48 列、刷新、折叠和摘要更新之后核对原文本与 UTF-8 字节偏移。核心套件实际填满并超过有界窗口，执行滚轮动作后比较内外滚动和保留事件数，确认更新没有变成新增聊天。[核心 PTY 脚本][core-pty]。

readable 套件只检查最终输入草稿文本与待发送队列为空；它没有设置非空草稿，也没有独立验证中间光标。这里将其可读性结论与另外两个套件的精确草稿结论分别记录。

摘要命令套件分别执行两个隔离场景：纯 exec，以及 `stat → exec → stat`。每个场景用两次真实命令向夹具文件各追加一次标记；最终内容必须恰为两行，从文件副作用证明总结、刷新、折叠和读取没有重放命令。等待请求单独计数，不冒充新的命令执行。[摘要命令 PTY 脚本][commands-pty]。

命令、逐组结果、套件报告、源码与二进制标识、脚本及截图校验值统一收录于 [report.json](report.json)。凭据检查由各套件执行，报告保留实际检查边界。

## 5. 实际界面

图片来自本候选的 PTY 字节，经 VT 解码后渲染；没有手工重画产品界面。宽屏与窄屏使用同一套真实交互。

PNG 归档保留原目录 `docs/reports/tool-group-summaries-20261120/`；下列链接指向云盘 ZIP，括号中是该目录内的文件名。完整清单见 [归档清单](../generated-png-manifest-20261121.json)。

| 核心摘要阶段 | 120 列 | 48 列 |
| --- | --- | --- |
| 首次摘要、折叠 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-first-folded-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-first-folded-48.png`） |
| 更新摘要、展开并滚动 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-updated-expanded-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-updated-expanded-48.png`） |
| 最终摘要、折叠 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-final-folded-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-final-folded-48.png`） |

| readable 命令阶段 | 120 列 | 48 列 |
| --- | --- | --- |
| 默认命令卡片 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-folded-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-folded-48.png`） |
| 展开后的命令开头 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-expanded-head-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-expanded-head-48.png`） |
| 展开的输出 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-expanded-output-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-expanded-output-48.png`） |
| 展开的源码尾部 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-expanded-source-tail-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `readable/readable-commands-expanded-source-tail-48.png`） |

| 摘要与命令整合 | 120 列 | 48 列 |
| --- | --- | --- |
| 纯命令、用户明确折叠 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `commands/exec-only/summary-commands-exec-only-folded-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `commands/exec-only/summary-commands-exec-only-folded-48.png`） |
| 文件与命令混合、用户明确折叠 | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `commands/mixed/summary-commands-mixed-folded-120.png`） | [查看](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `commands/mixed/summary-commands-mixed-folded-48.png`） |

[展开后的摘要窗口与保留草稿](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `tool-summary-updated-expanded-120.png`）

[窄屏下明确折叠的混合摘要窗口](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `commands/mixed/summary-commands-mixed-folded-48.png`）

## 6. 本轮修复的实际问题

早期实现把可读命令从工具窗口直接排除，导致纯命令组设置摘要后没有标题，混合组则可能重复显示同一组 ID。修复先收齐逻辑组，再决定整个组显示为独立卡片还是摘要窗口；独立卡片与摘要窗口共用真实命令标题。

自动展开原先可能覆盖用户选择。现在将明确折叠意图独立保存，覆盖“先折叠、后加入命令、再首次设置摘要”及重载恢复的路径。旧的可读卡片测试保留，新增测试同时核对唯一成员、宽窄布局和滚动位置。

被动等待原先直接返回对象状态，可能遗漏它已获得的总结提醒。现增加仅处理总结的返回路径，并用 code_mode `waitFor` 作对照；授权在只读复核期间被撤销的情况也纳入回归。此前的错误来源丢失修复、严格列表参数校验和文件读取边界继续纳入回归。前序失败按原源码和尝试编号保留，没有改写成最终成功记录。

## 7. 仓库整理

旧 [PR #109](https://github.com/LIghtJUNction/ssmcp/pull/109)、[PR #111](https://github.com/LIghtJUNction/ssmcp/pull/111)、[PR #112](https://github.com/LIghtJUNction/ssmcp/pull/112) 的内容此前已进入主线。本轮核对差异并补齐合并关系和 GitHub 状态，整合节点为 [4ae22ea](https://github.com/LIghtJUNction/ssmcp/commit/4ae22ea828ee2647b44c275f79679963ba7e88a9)。

已清理 **23 个远程分支、21 个本地分支**。远程删除核对准确 SHA 并读取结果确认；本地采用正常的已合并分支删除方式。活跃工作树和仍被使用的分支保留，名称与原 SHA 见 [branch-cleanup.json](branch-cleanup.json)。

## 8. 五项功能的当前边界

| 用户关心的项目 | 本轮确认的状态与源码依据 |
| --- | --- |
| Agents 页 | 已有参与者、观察到的活动和会话导航；入口在 [conversation.rs][conversation] 与 [activity.rs][activity]。 |
| 待发送 | 已有排队、撤回编辑、取消与接收确认；入口在 [conversation/queue.rs][queue] 与 [client/steering.rs][steering]。 |
| 连续工具调用窗口 | 已有分组、折叠和内部滚动；本轮整合可更新摘要、第 11 次提醒及可读命令，见 [tool_activity.rs][tool-activity]。 |
| 审计页 | 已有按执行轮次查看源码、调用和结果的入口，保留容量有界；入口为 [audit_view.rs][audit] 与 [code_mode.rs][code-mode]。 |
| 聊天框到后端循环 | 已有连接范围内的监听、路由、确认和回传，见 [interface/listener.rs][listener]；完整跨会话公开协议仍是设计提案。 |

第五项不能标记为全部完成。[跨会话监听设计][listener-design]明确区分现有连接型监听与下一阶段：持久会话地址、明确接收端选择、跨连接查询原消息结果、独立协议 schema，以及 zcall／zcalld 的聊天控制、收件箱和上下文服务接线，仍需实现与独立验收。

本候选尚未部署到运行中的客户端或 relay。完整总结体验需要两端支持相应原生工具与外层提醒；本地通过不等于生产上线，也不等于跨平台发布验收。最终状态复核有时间预算，Agent 遇到旧 revision 应重新 list，并在 set 中携带版本条件；当前机制不提供跨轮持久化的提醒待发送队列。

## 9. 后续账户退出整合验证

完整摘要验收完成后，主线又合入了账户连接写错误与晚到授权拒绝的修复。本轮将主线 `c7086f9` 正常合入，形成 `4ca66fa12693b1b2db940da78b3c89cb2048425d`，并针对这条连接生命周期边界补查。

新增逻辑在停止请求派发后，最多再读取两秒终态拒绝；排队帧不会执行为新工具调用。只有真实拒绝被既有规则识别为永久授权失败时，才改变原错误分类。摘要计数、调用来源、原生超大回复恢复和 relay 最终授权仍保留原有实现。[整合差异](https://github.com/LIghtJUNction/ssmcp/compare/21342ea6b3de61883f70f3811f7d67e378091967...4ca66fa12693b1b2db940da78b3c89cb2048425d)

该整合源码的格式检查、库测试构建、严格 workspace／all-target Clippy 与客户端构建均通过。五组针对性结果如下：

| 补充筛选范围 | 通过 | 失败 |
| --- | ---: | ---: |
| 连接退出时的晚到拒绝与队列竞争 | 5 | 0 |
| 永久错误分类 | 5 | 0 |
| 客户端帧协议 | 10 | 0 |
| 摘要计数、来源、授权与提醒 | 34 | 0 |
| 最终交付前重新检查撤销 | 1 | 0 |

使用该源码新构建的客户端，完整账户 relay 夹具一次通过 **47 项检查**。注销后的实际退出码为 **1**，表示授权撤销后的预期错误退出；连接尝试次数为 **2 → 2**。后台子进程回收、撤销后不再重连、最多一次重新授权及凭据删除检查均通过。

这次补充检查使用独立账号夹具，未启动模型或修改运行服务。第 5 节的 18 张界面截图仍归属于原完整验收源码；本节的客户端哈希、命令、五组测试与账户结果单独保存在 [post-integration.json](post-integration.json)。

[tool-activity]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/events/tool_activity.rs
[command-rows]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/events/command_rows.rs
[events]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/events.rs
[summary-tests]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/events/tool_summaries/tests.rs
[summary-interface]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/interface/tool_summary.rs
[summaries]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/events/tool_summaries.rs
[summary-doc]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/docs/tool-group-summaries.md
[http-summaries]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/http/tool_summaries.rs
[await]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/http/await_tool.rs
[delivery-tests]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/http/question_delivery_tests.rs
[finish]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/http/finish_request.rs
[http]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/http.rs
[conversation]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/conversation.rs
[activity]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/activity.rs
[queue]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/terminal/conversation/queue.rs
[steering]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/steering.rs
[audit]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/audit_view.rs
[code-mode]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/code_mode.rs
[listener]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/src/client/interface/listener.rs
[listener-design]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/docs/design/chat-listener-protocol.md
[core-pty]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/scripts/e2e_tool_summaries.py
[commands-pty]: https://github.com/LIghtJUNction/ssmcp/blob/8725b7b4606e8eb75c3d366c380f991842e0219a/scripts/e2e_summary_commands.py
