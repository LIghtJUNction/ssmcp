# SSMCP 0.10.21

## 终端与消息

- **直接阅读实际命令。** 相邻命令保留在同一 Tools 组，默认打开有界共享窗口，各自显示 argv、运行状态与 shell 语法颜色，并预览最后三行输出。手动折叠的选择会保留。长内联脚本保持紧凑标题，详情中保留脱敏源码、执行目的和已记录输出，并支持独立滚动。超出预览上限时提示截断；本地任务仍保留时可经 `/command JOB_ID` 读取完整脱敏 argv。查看历史不会重新执行命令。 [命令显示验证](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/reports/readable-commands-20261120/validation.json)
- **原生消息与组发。** 在获准的设备账户收件箱和已验证线程之间双向发送文本。单发使用精确地址，组发只发送给显式选中的 1–32 个收件人，并分别显示回执。回执记录入队、放入工具响应和确认收取等阶段，任务执行情况由收件方另行报告。 [原生消息指南](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/thread-messaging.md)
- **按所选收件人使用 `/send`。** 命令沿用最近一次明确选择的组、原生收件人或本地目标。原生消息发送原文，本地监听仍准备文件和技能引用。组发部分成功时保留草稿，重试沿用原组及各目标的请求 ID。 [发送与回执](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/force-send-listener.md)
- **在 `/send` 正文中补全引用。** 支持 `@file` 和 `$skill` 候选，保留 Unicode 与缩进；引用候选加载完且有选中项时，Enter 或 Tab 接受候选，收起列表后再按 Enter 才发送；加载期间保留草稿。收件人和群组路由保持各自的解析边界。 [引用补全验证](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/reports/send-reference-completion-20261120/README.md)
- **引用与草稿取回。** 普通要求中的缺失 `@file` 或未知 `$skill` 会提示引用不可用，符合大小限制的正文仍可发送。空输入框按 ↑ 可取回仍待交付的要求，确认后移除对应的待交付记录。跨连接恢复去向未确认的要求时，原尝试保留，另起一份待编辑的请求。 [引用和队列](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#输入要求与队列)
- **身份与历史描述。** 界面区分操作系统 PID、中继连接 ID 和逻辑线程 ID。在线角色短摘要上限为 128 个 UTF-8 字节。本地历史标题与摘要可以单独保存，分别最多 256 和 2048 个 UTF-8 字节。 [成员身份](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#agent-面板与成员身份) · [历史描述](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#本地历史标题与摘要)
- **历史与审计。** 对话按所选历史显示带归属的任务输出和原生回复，Logs 与 Tasks 保留跨会话审计。晚到的回复可写回仍可写的原会话，`/copy` 继续使用所选历史的回复。 [本地消息与历史归属](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#本地消息与回答问题)
- **历史列表的键盘焦点。** 打开 `/resume` 或 `/archived` 列表时，即使已经位于 History 页，也会把键盘焦点交给列表。Enter 恢复并继续所选会话后，焦点回到输入框。 [历史恢复操作](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/conversation-history.md)
- **账号退出与撤销。** 修复账号连接在写入先报错时丢失最终授权失效通知的问题，避免退出或撤销登录后因这条竞态再用旧凭据重连。读取结束消息使用统一期限，普通断线、主动停止和重载保留各自的处理规则。 [账号连接修复](https://github.com/LIghtJUNction/ssmcp/pull/142)
- **为会话设置目标。** `/goal set <text>`、`/goal show` 和 `/goal clear` 管理当前会话的单行目标，最多 256 个字符且不超过 1024 个 UTF-8 字节。目标记录用户意图，不会执行任务或授予批准。 [目标命令](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/terminal/conversation/goals.rs) · [保存与限制](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/interface/history/goals.rs)

## 等待、计时与回复

公共 `await` 与 JavaScript `waitFor` 可观察执行任务、relay 任务、协作成员状态、自己的协作收件和用户输入。普通目标支持任一满足或全部满足；单轮观察最多 25 秒，原身份与连接仍有效时可用同一 wait ID 继续观察。取消只停止观察，不终止任务或确认消息。公共 `await` 不占用 JavaScript 执行槽；`waitFor` 使用所在 code mode 的执行槽。 [等待接口](await.md)

`ssmcp.time.now` 返回 UTC 与连接内单调时间；`op:"interval"` 接受 Unix 毫秒或带时区的 RFC3339 时间，返回有符号时间差、绝对时长和分解后的天时分秒。`ssmcp.time.wait` 支持相对时长、绝对截止时间、列出、查询和取消。界面用真实任务开始时刻和截止时间显示“运行”“剩余”“已到期”，`/wait` 展示详情。计时到期不代表任务已经完成。 [时间原语](time-primitives.md)

`request_finish`、`ssmcp.ui.finish`、`ssmcp.ui.answer` 和 `await` 的结束审批目标已退役；直接调用会得到明确错误。强制继续和对话提醒钩子停止生成。正常的 `ui.message` / `ui.present` 进度与最终回复仍保存到其所属对话，最终回复无需再申请结束许可。真实用户继续通过问题界面答题；会话归属、认证和去重仍生效。 [用户问题与最终回复](finish-and-questions.md)

## 本机入口与 Agent 协议

`ssmcp local --socket` 增加本机 HTTP/MCP Unix socket。默认路径是 `$XDG_RUNTIME_DIR/ssmcp/mcp.sock`，未设置运行目录时使用 `/tmp/ssmcp-<有效 UID>/mcp.sock`；`--socket PATH` 可指定私有绝对路径。它沿用实际 HTTP 来源与 OAuth，支持相同 UID 的进程连接。 [本机连接](local-mode.md)

`--agent codex|claude-code|pi|opencode|playwright` 显式选择已有协议适配器；`--agent list` 查看支持项。启动只加载协议和工具目录，模型任务需要另行调用。`--agent ssmcp` 通过明确的 endpoint 与令牌环境变量连接另一个 MCP 网关。原生参数放在 `--` 后，经过保存会话和重载仍按原样保留。 [Agent 入口与兼容范围](agent-entry.md)

电脑控制状态改为“电脑控制：开启／关闭”，窄屏显示“键鼠：开／关”。`/computer on|off|status` 与 `ssmcp computer on|off|toggle|status` 管理当前桌面的鼠标和键盘；同一系统用户的本机连接共享该开关。 [电脑控制](computer-use.md)

`ssmcp send` 将请求交给精确的线程或冻结后的进程收件集合，`ssmcp wait` 将实际最终结果写到标准输出，进度写到标准错误，可接到下一条命令的标准输入。等待支持任一、全部或指定数量的结果，超时不会取消接收方任务。线程列表右键读取所点线程的历史。 [账户与终端接口](client-accounts.md)

## 依赖与运行版本

Project Z 内部跨仓库依赖采用各 GitHub 仓库默认分支源码。Cargo.lock 与源码 gitlink 记录本轮核对的完整提交；下一轮同步会重新核对上游默认分支。当前直接依赖的版本元数据为 zharness 0.6.0、zplugin 0.3.1、ztuio 0.1.0 和 zuse-computer 0.2.2。外部 registry 打包仍需单独验证其规范化依赖图。 [依赖来源与维护规则](dependencies.md)

交互式连接启动时，客户端在后台使用缓存或检查当前平台的已发布资源元数据，提示可用更新。支持版本通知的连接结合 `Ready` 信息显示客户端与服务器版本，区分正在运行的服务器版本和已发布的客户端版本。检查不会自动安装软件或升级服务。 [启动检查](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/update/startup.rs) · [连接版本提示](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/update/notice.rs)

连接 0.10.13 旧中继时，可用 `--account-compat` 和该中继已验证的自身账号注册连接。原生消息另需客户端与中继支持，`messages.*` helper 也须由中继提供。 [账号兼容说明](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-accounts.md) · [能力要求](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/thread-messaging.md#兼容范围)

公共 `await` / `waitFor`、线程请求结果回传及服务端钩子退役需要更新后的中继；时间工具还需要客户端与中继实际协商对应能力。Release 工作流保留 0.10.21 的部署延后规则：发布安装包不会自动重启现有生产 relay，也不会让活动客户端自动加载新版本。

## 验证范围

文中链接的验证报告只适用于各自记录的受测源码、二进制和测试范围，不代表本次整合后的最终门禁已通过。最终发布仍需在冻结的同一源码与二进制上完成验收；本说明不表示已经部署。
