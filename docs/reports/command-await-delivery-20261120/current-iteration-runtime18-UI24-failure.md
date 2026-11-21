# SSMCP 0.10.25：runtime18 全部通过，UI24 首轮停在 worker 取消 EOF 检查

**记录截止：2026-11-21 12:05:33 UTC。** 本轮继续使用同一合并提交 **`b8d94f5e12788ed19d1b74eecdcc068ea9fc0078`**（S_b8d），tree **`1391701c52db5066e7a25bbe3a32c1d5a82c47d5`**。原 Gate18 的 35 阶段与随后 N18 只读来源证明均已通过；新增的 **runtime18，6 项 transport＋12 项 private MCP，全部通过**。紧接的 UI24 首轮实际启动 4 个 suite，前 3 个通过，第 4 个 `zuse_worker` 失败，后 20 个未运行。[本次结果事实](runtime18-UI24-Sb8-first-result-public-facts.json)

UI 已通过的三个父 suite 分别记录 **53、137、74** 项通过检查，合计 **264**。失败 suite 的 whole-text 子报告先记录 5 个用例通过，再报 `timed out: worker observed cancellation EOF`；这 5 个子用例不加到 264 中，也不把失败 suite 记为通过。

本次观察到的是子程序返回 1 后触发的 **`CalledProcessError`**。worker suite 耗时 4.426 秒，子报告耗时 1.631 秒；原件没有显示父 subprocess 的 120 秒或外层 suite 的 900 秒超时。**具体根因尚未建立**，原执行者正在定位。当前报告先保存已发生的结果，不以猜测原因替代失败记录。

## 同一源码下，各层结果分别记录

| 范围 | 本轮结果 | 证据边界 |
|---|---|---|
| Gate18 原 35 阶段 | 35／35 passed；内外退出回执均为 0 | 原编译、策略、Rust 与构建范围；详细表保留在上一报告 |
| N18 一次来源证明 | 只读解析证明通过 | 0 产品测试、0 Cargo 调用，不另算一批产品测试 |
| runtime18 | 6 transport＋12 private MCP，18／18 passed | 输入与聊天收件者的运行烟测；不等于完整 UI、P1 或发布 |
| UI24 首轮 | 4 个启动、3 个通过、1 个失败、20 个未运行；alias 覆盖 0 | 仅前 3 个父 suite 的 264 项通过检查计入成功数 |
| 后续消费者 | controls、多 UID、version_all、P1、两项 native 补充未执行 | 依据验收协调者的终态通报；没有迁入历史成绩 |
| 正式发布及生产操作 | 未 dispatch；未安装、生产重载或部署 | H2 仍未绑定、未执行 |

上一份 [Gate18 完整报告](current-iteration-Gate18-passed.md)独立保留，其中的 35 阶段表、原 Gate16／17 失败、E15 一行平台修复、检查点与 H2 准备记录均未覆盖。当前 UI 失败不抹去同源码已完成的 Gate18／N18／runtime18 结果；这些通过结果也不能把 UI24 补成通过。

## runtime18：实际输入与聊天收件者运行结果

运行始于 **11:53:04 UTC**，结束于 **11:53:13 UTC**。内层 driver 记录 **8.524 秒**，外层记录 **9.220 秒**；外层退出 0、未超时，内外错误列表为空。这是一次指定环境下的执行耗时，**不是吞吐量、响应延迟或跨版本性能基准**，也不能将两个时间相加。

以下名称与原报告逐项一致。中文说明只概括该检查的范围，不扩大为所有输入模式或所有连接均已验证。

| 模块 | 原检查名称 | 结果与范围 |
|---|---|---|
| transport | `mcp_stdio_protocol_only` | MCP stdio 的协议输出边界；passed |
| transport | `pipe_mcp_literal_and_warnings` | 管道／MCP 字面内容与警告；passed |
| transport | `fake_exec_argv_literal` | 模拟执行器 argv 的字面值；passed |
| transport | `fake_exec_nonzero_bounded_diagnostics` | 模拟执行器非零退出的有界诊断；passed |
| transport | `unix_socket_write_half_close_once` | Unix socket 写半关闭与一次交付；passed |
| transport | `terminal_original_history_and_current_draft` | 终端原历史与当前草稿；passed |
| private_mcp | `private_mcp.owned_account_and_two_clients` | 自有测试账号与两个客户端；passed |
| private_mcp | `private_mcp.chat_history_draft_queue_baseline` | 聊天历史、草稿和队列基线；passed |
| private_mcp | `private_mcp.globally_retired_native_tools_absent` | 已全局退役的原生工具不在目录；passed |
| private_mcp | `private_mcp.none_remaining_human_tools_hidden` | none 连接隐藏其余人工聊天工具；passed |
| private_mcp | `private_mcp.concurrent_chat_remaining_human_tools_present` | 并发聊天连接仍保留相应工具；passed |
| private_mcp | `private_mcp.none_stale_human_reply_rejected` | none 连接拒绝旧人工回复调用；passed |
| private_mcp | `private_mcp.none_stale_human_wait_rejected` | none 连接拒绝旧人工等待调用；passed |
| private_mcp | `private_mcp.none_exec_roundtrip` | none 连接仍可完成执行工具往返；passed |
| private_mcp | `private_mcp.none_time_roundtrip` | none 连接仍可完成时间工具往返；passed |
| private_mcp | `private_mcp.none_native_messages_roundtrip` | none 连接仍可完成原生消息往返；passed |
| private_mcp | `private_mcp.none_reload_reconnect_preserves_none` | none 在重载和重连后保持；passed |
| private_mcp | `private_mcp.chat_history_draft_queue_unchanged` | 聊天历史、草稿和队列保持；passed |

private MCP 使用测试自有账号、客户端及本地私有 relay。它证明了该受控环境中，none 连接关闭相关人工聊天入口后，执行、时间和原生消息往返仍可用，重载与重连保持 none；并发聊天连接的相应入口与历史、草稿和队列检查也通过。**这不是生产 relay 对关闭人工聊天能力的真实 ACK，也没有证明生产部署完成。** 原线上 none 缺少 ACK 的失败不据此改写，产品没有因为这份报告获得自动回退许可。

transport 记录 `models_started=false`；private MCP 原报告未记录同字段，公开投影用 null 表示未知。因此报告不把一个模块的明确 false 扩写为另一模块已独立证明的状态。模拟执行器检查也不代表启动了真实 coding backend。

### 来源与产物一致性

runtime 的源码与编译源码来自同一 Gate 导出，执行前后输入相等；记录包含 **1613 个源码文件条目**，规范化源码清单 SHA-256 为 `27749093e31070940d1bb212a09079f8c6ac4e5fa7aaca8fcd5aaf6230796792`。五件产物的身份记录与本次 S_b8d 的 Gate18 记录匹配：

| 产物角色 | 字节数 | 记录的 SHA-256 |
|---|---:|---|
| library | 171932288 | `f329e613d4bbc4fccf5b380107e4e8cbf5f467343c3fcc75453e6299fb40f6c0` |
| ssmcp | 16248008 | `b5113562137983fee8e1e35914e0f5d90e3acc69eecd68473a014ff676d1e794` |
| ssmcp-frontend | 1386584 | `d3069119110ea265a021ae654dfb783d9db4c32b1f2f4e5f4758fc4ee7c8cbda` |
| ssmcpd | 8395024 | `5c3a87c059aaafac8620c471773a764a1680f17ec227db2e8cf5ed3b695d98cb` |
| vt_snapshot | 440984 | `efdbeb3f200fcf4404b2612d836433047ecc90eb0744f345fc971ee7441db97f` |

报告组读取了这些运行输入记录，**没有独立重新哈希五件二进制文件**。规范化源码清单有自己的序列化规则和摘要，不能与其他格式的源码清单 SHA 混为一个值。

runtime 清理字段记录：外层子进程退出 0 并已 reap；transport 的 10 个进程、1 个 decoder 返回码均为 0，5 个模拟进程均退出，socket 目录已移除；private MCP 的 7 个自有进程均退出且返回码为 0，短临时目录已移除。以上是原报告记录的清理范围，报告组没有重新操作进程或执行清理。原生任务退出 0、双 EOF 的状态由验收协调者通报；报告组另行独立读取了原外层结果与内层报告。

## UI24：三个父 suite 通过后停止

UI 运行于 **11:57:16–11:58:38 UTC**，总耗时 **81.774 秒**。原清单含 25 个逻辑 suite、24 个计划执行项；本次实际执行 4 项、独立通过 3 项、alias 覆盖 0，不能按逻辑清单数量增加已执行或已通过数量。

| 顺序 | Suite | 退出码 | 原耗时 | 父 suite 通过检查 |
|---|---|---:|---:|---:|
| 1 | message_references | 0 | 18.679 秒 | 53 |
| 2 | force_send | 0 | 27.454 秒 | 137 |
| 3 | listener_transports | 0 | 10.445 秒 | 74 |
| 4 | zuse_worker | 1 | 4.426 秒 | 未记录可相加的父汇总；不计入 264 |

前三项的父检查合计为 **53＋137＋74＝264**。这三个 suite 的名称分别指向消息引用、强制发送与监听传输；本稿依据父报告的状态和计数记录结果，没有将未逐项复读的断言扩展成额外功能承诺。

其余 **20 项未运行**：`interactions`、`private_desktop`、`account_compat`、`input_history`、`reload_state`、`session_groups`、`client_ui`、`canvas_logs`、`composer`、`questions`、`presentations`、`context_memory`、`file_paste`、`relay_composer`、`goal_startup`、`tool_summaries`、`readable_commands`、`summary_commands`、`physical_latency`、`time_wait`。

UI 的 `inputs-before.json` 与 `inputs-after.json` 均为 **422587 字节**，SHA-256 同为 `3d8f8cd821fc2dac73004fa9bbcf591c025c77a0f47e673446a413b4af4cca7e`；报告组确认两份原字节相同。原输入集合摘要为 `f5f651f3b5f86a698347e1c26cd766bb2da4b3cff1ae820b7a752c3f26e93a1c`，changed 为空。输入集合摘要与两份文件自身摘要是不同对象，分别记录。

### worker 失败：已证明的事实与尚未证明的原因

原路径为 `scripts/e2e_zuse_worker.py:180` 调用 `scripts/e2e_x11_text.py`。whole-text 子报告在私有 display 中记录以下五项 passed：

| 子用例 | 原状态 |
|---|---|
| whole-string rejection for wrong machine | passed |
| fresh worker after corrected machine | passed |
| whole-string rejection for wrong pid | passed |
| fresh worker after corrected pid | passed |
| whole UTF-8 replaces a character selection | passed |

随后记录的错误是 **`timed out: worker observed cancellation EOF`**。完整父日志显示 **`CalledProcessError`，子程序返回非零退出码 1**；没有观察到 subprocess 超时异常。子报告的 1.631 秒与父 suite 的 4.426 秒属于不同层级，不相加，也不拿来替代外层 120／900 秒预算。

目前能够确定：child 在取消 EOF 观察检查处报错并返回非零，导致父 suite 失败，UI 序列随后停止。**为何该 EOF 观察检查未满足，以及应归于产品、夹具还是环境，现有结果尚不能判定。** 不能仅凭错误文字把它归为外层超时、慢平台、产品死锁或已确认的取消传播缺陷。

whole-text 子报告记录自有进程清理 passed；四个已启动 suite 各自的 `owned_child_reaped` 均为 true。**父子进程已 reap 的字段不独立证明全部后代都已回收。** 原生任务非运行态、退出 1、双 EOF 来自验收协调者终态通报；报告组独立读取了 acceptance 原件、完整 worker 日志及完整 whole-text 子报告，没有在现场追加输入或运行新测试。

## 原件身份与报告读取范围

公开稿只保留结果、计数和必要身份；原件中的私人路径、display／窗口身份与测试载荷不在本文复制。

| 原件 | 字节数 | SHA-256 | 报告组实际读取范围 |
|---|---:|---|---|
| runtime18 launch-result.json | 12348 | `18b430caff0a720fa27ed6cb7120fbe9bffb4f266ebb126e5636f0a4843dae20` | 完整原件哈希；来源、结果、逐项状态、输入与清理等选定字段 |
| runtime18 run/report.json | 62280 | `c11644d45080fb1d99530bd1ca81e3eb936a8e9aa4b702d303af924cfc13bed1` | 完整原件哈希；来源、结果、逐项状态、输入与清理等选定字段 |
| UI24 acceptance.json | 812362 | `fca8d44852d3efc39b79866712b1677ffc9319ca240ee0e0e39ebc1f7ef33856` | 完整原件哈希；顶层结果、全部 24 项计划状态和前四 suite 的选定字段 |
| UI24 inputs-before.json | 422587 | `3d8f8cd821fc2dac73004fa9bbcf591c025c77a0f47e673446a413b4af4cca7e` | 完整原件哈希；before／after 原字节相同 |
| UI24 inputs-after.json | 422587 | `3d8f8cd821fc2dac73004fa9bbcf591c025c77a0f47e673446a413b4af4cca7e` | 完整原件哈希；before／after 原字节相同 |
| worker zuse_worker/run.log | 2321 | `139e20caa2461fc659135021afa5843104d1182f0bf18c7a9b979464c1eb7f68` | 完整日志读取；公开稿省略私有路径 |
| worker whole-text.json | 5254 | `efee59ca2237615a8c45d06e7dc28da8f764575466e9619984a75cb071ad628a` | 完整子报告读取；公开稿省略显示／窗口身份与载荷 |

整份原件计算哈希不等于逐字审阅其中所有内容。runtime 与 acceptance 的读取范围是表中列出的状态、来源、各项结果及选定字段；worker 日志和 whole-text 子报告则完整读取。公开事实投影为 [runtime18／UI24 本轮结果](runtime18-UI24-Sb8-first-result-public-facts.json)。

## 保留的历史、恢复与发布边界

| 记录 | 保留状态 |
|---|---|
| S647 Gate16 | 4 阶段通过、库栈溢出失败、30 阶段未运行；无完整库汇总 |
| S401 Gate17 | 7 阶段通过、Windows GNU lint 失败、27 阶段未运行 |
| W 的 UI24 首轮 | 7 个启动、6 个通过、503 个父检查；账户兼容失败，17 个未运行；后来 69 项诊断不并入 |
| S_b8d Gate18 | 原 35 阶段全通过；Windows GNU 检查修复验证与四个具体库回归按本次记录保存 |
| S_b8d UI24 首轮 | 本文的 4 启动／3 通过／20 未运行和 264 父检查，独立保存，不用历史成绩补齐 |

历史分别见 [Gate16 失败事实](gate-S647-r16-failure-public-facts.json)、[Gate17 失败事实](gate-S401-r17-failure-public-facts.json)及 [上一完整报告中的 W 记录](current-iteration-Gate18-passed.md#历史成绩不跨源码转移)。本轮受控运行也不回填更早的原 socket 入口 ENOENT、生产 relay ACK 或报错用户实际 PATH 入口版本的未知状态。

一次性恢复仍处于准备状态：最近已核对的 `continuation.json` 检查点为 **10806 字节**、SHA-256 `7c01b462a6c7581b6881ae1f3de26f5ae2692b14f13eaf815cdc3cc45fccff37`，原 3289 与 6119 字节副本保留。最近一次独立读取仍是 **11:46 UTC**：脚本及配置哈希未变，completed、恢复日志和终端启动日志不存在。之后本报告没有取得新的自然触发记录，不能把检查点更新或当前连接可用写成重启恢复成功。

H2-CI 三个固定模块继续对应 [6f6bb09](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)。报告组于 **11:48 UTC** 只读核对过开发机模块本体：48892／f891a676…、44191／348fd908…、14758／e1b06222…均与固定版本完整匹配，权限为 0444；没有 import、execute 或重跑 AST。该准备没有产生新合同，也没有正式发布验证结果。crate publication 延期与生产 relay 部署延期分别保留。

统一 Computer Use 开关仍属于应用内许可机制，**`os_permission_boundary=false`**。本次 runtime 与部分 UI 通过不改变这一边界，也不证明能阻止同 UID 任意程序、独立工具或外部插件绕过应用控制。后续 OS 权限设计仍是 [设计草案](computer-use-permission-boundary-design-draft.md)，没有修改生产账户、主机权限或安装隔离服务。

原执行者继续处理当前 worker 失败，后续若有定位、修复或重跑，将另记真实来源与结果。当前 controls、多 UID、version_all、P1 和两项 native 补充按验收协调者终态通报均未执行；不将既有未完成范围转记为通过。报告组保持 **0 Cargo／metadata／构建／产品／测试／target 写入和排队重任务**，不运行 H2，不为本稿创建额外验收条件。
