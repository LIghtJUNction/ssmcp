# SSMCP 0.10.25

## 启动和聊天

`--input` 选择聊天输入，默认 `terminal`。`--agent` 选择聊天处理方，默认 `next-caller`，由下一次符合条件的工具调用领取。用 `--list-harnesses` 查看支持的原生协议名称，用 `--load-harness` 选择启动时加载的协议。原生尾参放在 `--` 后。只保存了旧协议选择 `--agent` 的会话需用新选项重建。[客户端输入](client-input.md) · [原生协议加载](agent-entry.md)

`--input none` 关闭本连接的聊天框、人类提示和相关工具。远端连接必须收到中继确认关闭人工聊天的真实 ACK；缺少确认时连接会退出。执行、已协商的时间观察、独立原生消息和重载仍按原有权限提供。成员可继续互发原生消息。启用聊天的连接可通过 `ssmcp.ui.reply` 回传已认证线程的进度和实际结果。旧结束审批和代理代答入口继续退役。[线程消息](thread-messaging.md) · [问题与回复](finish-and-questions.md)

## 查看调用信息和返回 ChatGPT

在当前已保存的对话中，`/openai` 查看最近观察到的调用标识。`/openai-session` 和 `/openai-subject` 分别查看对应字段。界面显示来源、缺失状态和观察时间。标识来自受支持且已验证的调用线程，可用于关联请求。

用 `/chatgpt set <URL>` 保存当前对话的 ChatGPT 链接，再用 `/chatgpt open` 或 `/chatgpt` 请求浏览器打开。`/chatgpt copy` 复制网址，`/chatgpt clear` 删除该对话保存的网址。`/plugin-docs` 打开官方 Plugins 参考。打开或复制均由显式命令触发；标识不会被拼成网址，也不会自动发送消息。[命令说明](commands.md#查看-openai-调用标识和打开-chatgpt)

## 引用、历史和粘贴

普通消息引用缺失的 `@文件` 时会显示警告，消息仍会发送并保留引用原文。`$skill` 按有效作用域解析，项目来源优先。使用技能前应通过返回的读取入口查看完整内容。原生消息和组发保留字面正文。[引用规则](client-input.md) · [原生消息](thread-messaging.md)

已归属会话的回复保留原历史归属，并沿用原有幂等回执。终端批量解码粘贴内容，保留按键顺序、输入限制和草稿。显式选择 Next caller 后使用 `next` 目标投递。[会话历史](conversation-history.md) · [粘贴回归定义](../src/client/terminal/conversation/tests/paste_batch.rs)

## 原生图片与 Unix 输入租约

同一次原生工具调用返回的多张图片分别显示在独立卡片中。预览按各自的事件 ID 关联，保留来源、原会话和调用身份。工具摘要继续独立计数。[图片事件](../src/client/terminal/events.rs#L1550) · [会话与调用回归定义](../src/client/terminal/events/source_tests.rs#L456)

在 Unix 上，主机输入租约的持有对象释放时会显式解锁。派生进程暂时保留旧文件描述符时，其他请求仍可取得已释放的输入锁。回归覆盖关闭旧描述符后新租约继续有效的情况。[输入租约](../src/computer/host_control/storage.rs#L54) · [文件描述符回归定义](../src/computer/host_control/storage.rs#L418)

## Computer Use On / Off

`/computer on`、`/computer off` 和默认快捷键 Super+Alt+C 控制同一系统用户下的 SSMCP 桌面、CDP 浏览器、Omarchy 和 Playwright 操作。桌面默认仍为 `isolated`，宿主操作显式选择 `host`。Off 阻止新控制并撤销旧请求，再次 On 不会恢复它们。只读观察与自有资源清理继续可用。

CDP 批次保留已确认步骤。Omarchy 自有 helper 回收完成后才允许后续输入。Playwright 发出请求但未收到回复时，关闭本连接拥有的驱动，并明确报告结果未知或部分执行。已经送达应用的操作可能生效，继续前应先观察状态。

这个开关只覆盖 SSMCP 的控制路由。普通 Shell、独立 zuse 和同一系统用户运行的其他程序仍使用原有操作系统权限，也可能调用本机开关。要限制这类程序，需要隔离桌面通道，并通过可撤权的受控代理访问。[电脑控制与适用范围](computer-use.md)

## 依赖和版本

内部跨仓库依赖直接跟踪各仓库的规范 Git 来源，实际提交由 Cargo.lock 记录。本版刷新了 zharness、zplugin、zcall 与 zmcpio 对应的五条锁记录，并同步 zharness、zplugin 的 gitlink。zuse 仍为 0.3.0，提交为 `830fcefd2277cbba46536c67d206f29058ba7fb3`。[依赖来源和核对方法](dependencies.md)

`ssmcp --version` 保持单行输出。`ssmcp --version --all` 离线显示构建期 Cargo.lock 中从 ssmcp 可达的依赖树，包含构建、开发和各锁定目标的依赖。实际链接的代码随平台和 features 而变。

## 发布状态

本次准备六个平台的 GitHub 发布包和安装器。crates.io 发布延期，等待独立的 registry 包验证；生产 relay 部署也延期，以保留现有会话。最终构建、运行验收、发布与安装以实际工作流和发布清单为准。远端 `--input none` 的确认以实际中继 ACK 为准，本机 socket 使用记录不能代替这一证据。[发布流程](releases.md)
