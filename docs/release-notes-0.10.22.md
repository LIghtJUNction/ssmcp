# SSMCP 0.10.22

## 发布修正

0.10.22 修正 0.10.21 正式发布中暴露的非 Linux 客户端编译问题，并保留本轮已经整合的连接、聊天、计时和终端功能。0.10.21 的标签与发布说明保留原样；0.10.22 使用独立版本和制品，验收结果以它自己的 Release 工作流为准。[0.10.21 失败的发布运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/44832665122) · [0.10.21 发布说明](release-notes-0.10.21.md)

连接另一 MCP 网关的 peer 客户端改用跨平台公共函数校验资源名，保持原有资源名规则，避免依赖仅在 Linux 编译的 Broker 模块。HTTP 客户端按平台构造可选的 Unix socket 连接，消除 Windows 构建中的无用可变变量；Unix 测试夹具规范化临时目录，兼容 macOS 的 `/var` 路径映射，生产路径检查保持原有要求。[资源名校验](../src/username.rs) · [Peer 客户端](../src/backend/peer.rs)

UI 验收夹具同步本轮实际交互：成员行右键打开该线程历史，带电脑控制徽标的底栏可按可用宽度压缩地址，本地会话预览不把虚构任务当成真实运行计时。修正的是这些检查的输入与预期，产品的线程路由、显示边界和任务计时语义保持原有设计。[终端交互](client-ui.md) · [时间原语](time-primitives.md) · [本机连接](local-mode.md)

## 本机连接与 Agent 入口

Linux 的 `ssmcp local --socket` 在现有本机 HTTP/MCP 服务上增加 Unix socket。默认使用 `$XDG_RUNTIME_DIR/ssmcp/mcp.sock`，未设置运行目录时使用 `/tmp/ssmcp-<有效 UID>/mcp.sock`；`--socket PATH` 可指定私有绝对路径。相同系统 UID 的程序可通过该 socket 连接，原有 Host、Origin 和 OAuth 校验继续生效，回环 HTTP 入口也继续可用。活动监听器不会被覆盖，正常关闭只清理本次持有的 socket。[本机连接与 curl 示例](local-mode.md)

`--agent` 显式选择要连接的原生协议，支持 `codex`、`claude-code`、`pi`、`opencode`、`playwright`，并可用 `--agent list` 查看支持项。启动只进行协议初始化与工具目录加载，实际模型任务仍需另行调用。`--agent ssmcp` 通过指定 endpoint 和令牌环境变量连接经过认证的 MCP 网关；Linux 上还可选择对端 Unix socket。[Agent 入口与协议范围](agent-entry.md)

原生参数在 `--` 之后按原样传递，保存会话和重载都会保留参数顺序、空参数及看起来像 ssmcp 选项的字面值。重载保留当前连接选择、启动目录和文件访问根，不会因默认连接配置后来改变而切换目标。[连接上下文](connection-contexts.md) · [重载验收](../scripts/e2e_connection_context_reload.py)

## 进程、线程与真实回复

远程 `ssmcp attach` 默认进入聊天输入与输出；原有 Shell PTY 通过 `--terminal` 或 `-i` 显式选择。`ssmcp list` 的数字对应有期限、绑定账号与服务器的目录快照；选择线程时发送到精确身份，选择进程时固定该次目录中的接收集合。[目录与 attach](client-accounts.md)

`ssmcp send` 和 `ssmcp wait` 支持把真实执行结果接入命令管道。最终结果写入标准输出，进度写入标准错误；等待支持任意一个、全部或指定数量的不同输入结果。交付回执和运行进度不计作完成，超时也不会取消接收方工作。接收方在下一次符合交付条件的原生调用中取得请求，这些接口不会自动启动模型。[发送、等待与计数](client-accounts.md) · [原生回复确认](../src/http/session_io.rs)

请求和回复使用中继内存中的有界保留区，最长保留一小时，服务器重启会丢失其中的请求和回复。线程历史按实际调用时记录的身份筛选，右键读取所点线程的历史；无法归属的旧记录留在进程视图，不补造线程身份。聊天接口需要命令端、接收端和中继共同支持。[保留与兼容边界](client-accounts.md#聊天接口的兼容与保留边界)

原生消息支持明确选定的单个收件人或 1–32 个收件人的组发，并逐个显示入队、放入工具响应和确认收取的回执。`/send` 沿用明确选择的目标；部分成功时保留草稿，重试保留原组和请求标识。回执描述消息交付阶段，任务结果由接收方另行返回。[线程消息](thread-messaging.md) · [发送与监听](force-send-listener.md)

账号退出与撤销会保留最终授权失效通知的处理，避免写入先失败时再用旧凭据重连。读取结束消息使用统一期限，普通断线、主动停止和重载保留各自的处理规则。[账号连接修复](https://github.com/LIghtJUNction/ssmcp/pull/142)

连接状态区分正在运行的客户端、服务器与已发布客户端版本。后台更新检查只提供通知，不会自动安装或切换服务。[更新通知](../src/client/update/notice.rs)

匿名 `--allow` 连接旧中继时，可显式选择 `--account-compat`。该方式仅使用旧中继已经验证的自身账号，每次重连重新验证相同绑定，不会自动增加旧中继缺少的接口。[连接与兼容](client-accounts.md)

## 计时、电脑控制与终端

时间工具把任务开始、等待截止和实际完成区分开来。`ssmcp.time.now` 提供 UTC 与连接内单调时间，`op:"interval"` 计算带时区时间点之间的有符号差和绝对时长；`ssmcp.time.wait` 支持相对或绝对截止时间，以及列出、查询和取消。终端显示“运行”“剩余”“已到期”，`/wait` 查看详情；计时到期不表示任务完成。[时间原语](time-primitives.md)

电脑输入开关在同一系统用户的本机连接之间共享。终端中的 `/computer on|off|status` 和命令行 `ssmcp computer on|off|toggle|status` 控制本机鼠标与键盘输入；界面显示“电脑控制：开启／关闭”，窄屏显示“键鼠：开／关”。[电脑控制与授权边界](computer-use.md)

命令组显示实际 argv、运行状态和有界输出。长脚本可打开详情独立滚动，手动折叠的选择保留，查看命令历史不会重新执行命令。[命令与终端显示](client-ui.md)

草稿保留完整正文和明确的发送目标。`/send` 正文支持文件与技能引用补全，大段粘贴转为私有文件引用，存储失败时保留原始内容供显式重试；空输入框可取回待交付要求，确认后移除对应的待交付记录。[草稿与引用](client-ui.md) · [交互验收](../scripts/e2e_interactions.py)

历史操作沿用当前所选会话的归属。恢复历史列表时把焦点交给列表，继续会话后回到输入框；会话目标、标题和摘要按所属历史保存，晚到的回复写回仍可写的原会话。[历史恢复](conversation-history.md) · [历史与会话目标](client-ui.md)

## 正常回复与真实用户问题

强制继续、结束审批和对话提醒钩子停止生成。`request_finish`、`ssmcp.ui.finish`、`ssmcp.ui.answer` 及 `await` 的结束审批目标已退役，旧名称直接调用会返回明确错误。正常的 `ui.message` 和 `ui.present` 仍展示进度与最终回复，最终回复无需再申请结束许可。[用户问题与最终回复](finish-and-questions.md)

真实用户的问题答案和原生线程结果继续走各自的身份校验与交付流程。会话 owner、访问控制、去重和真实答案来源检查保留，`ssmcp.ui.reply` 仍提交接收方实际产生的线程结果。公共 `await` 与 JavaScript `waitFor` 可观察任务、成员、协作收件和用户输入；取消等待只停止观察。[等待接口](await.md) · [线程结果回传](client-accounts.md)

## 依赖快照与部署边界

本次纠错沿用 0.10.21 受审源码的内部依赖快照。除 ssmcp 自身包版本更新为 0.10.22 外，Cargo.lock 中的依赖身份、提交和校验值，以及各源码 gitlink 保持不变；本轮不追加追赶上游默认分支。后续依赖同步仍按项目既有规则单独核对。[依赖维护规则](dependencies.md) · [锁文件](../Cargo.lock)

Release 工作流对 0.10.22 延后生产服务器部署。该版本的 tag 推送和手动发布重试都会得到 `deploy_server=false`，即使手动输入要求部署也不会由此触发 relay 重启；原有延后版本记录继续保留。发布包、安装客户端与切换服务器分别记录结果，活动客户端也不会因为发布新版本而自动重载。[发布部署策略](../.github/workflows/release.yml) · [策略回归测试](../scripts/test_release.py)

公共等待、线程输入结果回传和服务端钩子退役需要更新后的中继，时间工具还需要客户端与中继协商相应能力。仅更新客户端不能给旧中继增加这些接口；服务器切换仍需单独安排并保护活动会话。[客户端兼容范围](client-accounts.md) · [服务器部署](deployment.md)

## 验证范围

发布记录中的每项验收对应其明确的源码、二进制和平台。此前整合版本的 Linux 单元测试、CLI/PTY 与私有 X11 worker 结果属于各自冻结的候选，不替代 0.10.22 的跨平台发布矩阵。私有 X11 用例验证真实输入、截图和取消清理，不覆盖完整轨迹插值或缓动时序；性能结论也需要单独的受控测量。本说明不表示已部署生产服务器。[发布门禁](releases.md) · [私有 worker 验收](../scripts/e2e_zuse_worker.py)
