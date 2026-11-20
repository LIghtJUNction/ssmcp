# SSMCP 0.10.21

## 终端与消息

- **直接阅读实际命令。** 相邻命令保留在同一 Tools 组，默认打开有界共享窗口，各自显示 argv、运行状态与 shell 语法颜色，并预览最后三行输出。手动折叠的选择会保留。长内联脚本保持紧凑标题，详情中保留脱敏源码、执行目的和已记录输出，并支持独立滚动。超出预览上限时提示截断；本地任务仍保留时可经 `/command JOB_ID` 读取完整脱敏 argv。查看历史不会重新执行命令。 [命令显示验证](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/reports/readable-commands-20261120/validation.json)
- **原生消息与组发。** 在获准的设备账户收件箱和已验证线程之间双向发送文本。单发使用精确地址，组发只发送给显式选中的 1–32 个收件人，并分别显示回执。回执记录入队、放入工具响应和确认收取等阶段，任务执行情况由收件方另行报告。 [原生消息指南](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/thread-messaging.md)
- **按所选收件人使用 `/send`。** 命令沿用最近一次明确选择的组、原生收件人或本地目标。原生消息发送原文，本地监听仍准备文件和技能引用。组发部分成功时保留草稿，重试沿用原组及各目标的请求 ID。 [发送与回执](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/force-send-listener.md)
- **在 `/send` 正文中补全引用。** 支持 `@file` 和 `$skill` 候选，保留 Unicode 与缩进；引用候选加载完且有选中项时，Enter 或 Tab 接受候选，收起列表后再按 Enter 才发送；加载期间保留草稿。收件人和群组路由保持各自的解析边界。 [引用补全验证](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/reports/send-reference-completion-20261120/README.md)
- **引用与草稿取回。** 普通要求中的缺失 `@file` 或未知 `$skill` 会提示引用不可用，符合大小限制的正文仍可发送。空输入框按 ↑ 可取回仍待交付的要求，确认后移除对应的待交付记录。跨连接恢复去向未确认的要求时，原尝试保留，另起一份待编辑的请求。 [引用和队列](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#输入要求与队列)
- **身份与历史描述。** 界面区分操作系统 PID、中继连接 ID 和逻辑线程 ID。在线角色短摘要上限为 128 个 UTF-8 字节。本地历史标题与摘要可以单独保存，分别最多 256 和 2048 个 UTF-8 字节。 [成员身份](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#agent-面板与成员身份) · [历史描述](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#本地历史标题与摘要)
- **历史与审计。** 对话按所选历史显示带归属的任务输出和原生回复，Logs 与 Tasks 保留跨会话审计。晚到的回复可写回仍可写的原会话，`/copy` 继续使用所选历史的回复。 [本地消息与历史归属](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-ui.md#本地消息与回答问题)
- **账号退出与撤销。** 修复账号连接在写入先报错时丢失最终授权失效通知的问题，避免退出或撤销登录后因这条竞态再用旧凭据重连。读取结束消息使用统一期限，普通断线、主动停止和重载保留各自的处理规则。 [账号连接修复](https://github.com/LIghtJUNction/ssmcp/pull/142)
- **为会话设置目标。** `/goal set <text>`、`/goal show` 和 `/goal clear` 管理当前会话的单行目标，最多 256 个字符且不超过 1024 个 UTF-8 字节。目标记录用户意图，不会执行任务或授予批准。 [目标命令](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/terminal/conversation/goals.rs) · [保存与限制](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/interface/history/goals.rs)

## 等待与完成

公共 `await` 与 JavaScript `waitFor` 可等待执行任务、relay 任务、协作成员状态、自己的协作收件、用户输入和已有结束审批请求。普通目标支持任一满足或全部满足；用户输入与结束批准使用独立事务。单轮观察预算最多 25 秒；原身份、客户端连接和记录仍有效时，超时可以用同一 wait ID 恢复。取消只停止观察，不终止任务、确认消息或批准结束。公共 `await` 不占用 JavaScript 执行槽；在 code mode 内使用 `waitFor` 仍占用所在的执行槽。 [接口与验证边界](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/validation/await-finish-20261120.md)

`request_finish` 创建或续等人工结束请求；`await` 的 `finish` 目标只观察已有请求，不创建审批。用户选择 No 也能使等待返回 `ready`，但 `completion_permission` 仍为 `null`。决定按请求创建时的原会话归属保存；原归属失效时，不会转写到当前会话。 [结束申请](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/finish-and-questions.md) · [等待接口](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/http/await_tool.rs)

结束许可要求真人明确选择 Yes，且匹配当前请求、成员和线程。agent 的 Yes、普通 ready / done 或超时都不能产生许可；许可只能消费一次。该约束适用于 SSMCP 的完成接口，不能拦截完全停止调用 SSMCP 的宿主自行输出外部 final。

## 依赖与运行版本

`zharness` 升级至 0.6.0，带入 `zcall 0.4.0` 和 `zguard 0.1.0`。每轮迭代及发布前核对实际使用的 Project Z 包，采用最新未撤回稳定版并固定版本、源码与锁文件。`zuse-computer 0.2.2 → zmcpio 0.4.0` 仍按已发布上游约束保留 `zcall =0.3.0`，等待兼容的正式上游版本。 [依赖版本与维护规则](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/dependencies.md)

交互式连接启动时，客户端在后台使用缓存或检查当前平台的已发布资源元数据，提示可用更新。支持版本通知的连接结合 `Ready` 信息显示客户端与服务器版本，区分正在运行的服务器版本和已发布的客户端版本。检查不会自动安装软件或升级服务。 [启动检查](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/update/startup.rs) · [连接版本提示](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/src/client/update/notice.rs)

连接 0.10.13 旧中继时，可用 `--account-compat` 和该中继已验证的自身账号注册连接。原生消息另需客户端与中继支持，`messages.*` helper 也须由中继提供。 [账号兼容说明](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/client-accounts.md) · [能力要求](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.21/docs/thread-messaging.md#兼容范围)

公共 `await` / `waitFor` 需要更新后的中继，被动结束观察需要支持相应操作的客户端。Release 工作流保留 0.10.21 的部署延后规则：发布安装包不会自动重启现有生产 relay，也不会让活动客户端自动加载新版本。

## 验证范围

文中链接的验证报告只适用于各自记录的受测源码、二进制和测试范围，不代表本次整合后的最终门禁已通过。最终发布仍需在冻结的同一源码与二进制上完成验收；本说明不表示已经部署。
