# SSMCP 0.10.26

本版将桌面和浏览器执行交给独立 Zuse CLI，补齐消息、引用、人工等待和执行输出的交互，并修正正式 CI 暴露的跨平台测试前提。版本调整通过 Bump 工作流统一准备，发布范围为 GitHub 二进制包和安装器。

## 启动和聊天

`--input` 选择聊天输入，默认 `terminal`。`--agent` 选择聊天处理方，默认 `next-caller`，由下一次符合条件的工具调用领取。用 `--list-harnesses` 查看支持的原生协议名称，用 `--load-harness` 选择启动时加载的协议。原生尾参放在 `--` 后。只保存了旧协议选择 `--agent` 的会话需用新选项重建。[客户端输入](client-input.md) · [原生协议加载](agent-entry.md)

`--input none` 关闭本连接的聊天框、人类提示和相关工具。远端连接必须收到中继确认关闭人工聊天的真实 ACK；缺少确认时连接会退出。执行、已协商的时间观察、独立原生消息和重载仍按原有权限提供。成员可继续互发原生消息。启用聊天的连接可通过 `ssmcp.ui.reply` 回传已认证线程的进度和实际结果。人工输入与决定以真实用户提交为准。[线程消息](thread-messaging.md) · [问题与回复](finish-and-questions.md)

原生消息按已认证的精确收件地址互发，设备收件箱和线程收件箱分别路由。`queued`、`included`、`acknowledged` 表示不同的交付阶段；处理结果由接收方另行报告。消息正文中的 `@file` 和 `$skill` 保持字面内容，显式组发使用明确收件人列表。[发送、读取和确认](thread-messaging.md)

## 查看调用信息和返回 ChatGPT

在当前已保存的对话中，`/openai` 查看最近观察到的调用标识。`/openai-session` 和 `/openai-subject` 分别查看对应字段。界面显示来源、缺失状态和观察时间。标识来自受支持且已验证的调用线程，可用于关联请求。

用 `/chatgpt set <URL>` 保存当前对话的 ChatGPT 链接，再用 `/chatgpt open` 或 `/chatgpt` 请求浏览器打开。`/chatgpt copy` 复制网址，`/chatgpt clear` 删除该对话保存的网址。`/plugin-docs` 打开官方 Plugins 参考。打开或复制均由显式命令触发；标识不会被拼成网址，也不会自动发送消息。[命令说明](commands.md#查看-openai-调用标识和打开-chatgpt)

## 引用、历史和粘贴

`ssmcp resume` 提供当前工作区历史的命令行入口，并沿用正常启动选项。没有未归档历史时，提示后以 0 退出，不进入登录或联网流程；有历史时要求交互终端，在正常连接后打开原有历史选择器，由用户明确选择。临时聊天布局不写入配置，选择仍按原工作区、归档状态和租约校验，不自动选择最近一条，也不重放请求或恢复模型、PTY、成员凭据。[会话历史](conversation-history.md)

普通消息引用缺失的 `@文件` 时会显示警告，消息仍会发送并保留引用原文。`$skill` 按有效作用域解析，项目来源优先。使用技能前应通过返回的读取入口查看完整内容。原生消息和组发保留字面正文。[引用规则](client-input.md) · [原生消息](thread-messaging.md)

已归属会话的回复保留原历史归属，并沿用原有幂等回执。终端批量解码粘贴内容，保留按键顺序、输入限制和草稿。显式选择 Next caller 后使用 `next` 目标投递。[会话历史](conversation-history.md) · [粘贴回归定义](../src/client/terminal/conversation/tests/paste_batch.rs)

## Questions 与人工等待

问题在 Questions 中独立回答，普通要求草稿、目标和光标保留。已验证线程的答案按原连接和提问线程交付；手动读取问题不会消费答案。[异步问题](finish-and-questions.md#异步问题与答案)

真实人工等待期间，输入框下方显示红色提示和实际剩余的 `mm:ss`，例如 `! 等待你的输入 · 剩余 00:13 · /wait`。多个等待显示数量，并取最短的实际剩余时间；关闭颜色时仍保留文字。休眠遮罩中倒计时保持可见，唤醒后活动等待继续显示。`/wait` 查看完整等待信息，`/wait cancel` 取消指定等待。[等待与倒计时](user-wait.md)

结束决定由 `ui.wait(intent: "finish")` 请求。只有原提问线程消费与申请、修订号匹配的真实 Human Yes，且最终外层回执返回 `may_finish: true`，才取得这次结束许可。默认 No 尚未提交时仍是未决定；普通消息、超时、取消和唤醒均不构成批准。[结束决定](finish-and-questions.md#申请结束)

## 展开 exec.read 的实际输出

原生 `exec.read` 成功返回后，对应调用事件可展开显示本次读取页的 stdout、stderr、任务状态、游标、EOF 和截断标记。折叠状态保留紧凑元数据；正文与元数据共用 128 KiB 本地预览预算，过长内容标明本地省略，原始工具返回保持原样。

内容按活跃连接和请求归属更新，不回填历史行，也不自动取得整个任务日志。需要完整输出时，继续按任务游标或输出归档读取。[展示实现](../src/client/terminal/events/tool_summaries.rs) · [事件浏览](client-input.md#7-折叠事件选择复制与恢复历史)

## 原生图片

同一次原生工具调用返回的多张图片分别显示在独立卡片中。预览按各自的事件 ID 关联，保留来源、原会话和调用身份。工具摘要继续独立计数。[图片事件](../src/client/terminal/events.rs#L1550) · [会话与调用回归定义](../src/client/terminal/events/source_tests.rs#L456)

## 独立 Zuse CLI

桌面和浏览器主操作直接调用独立 Zuse CLI，经 SSMCP 通用 exec 管理进程、输入和输出。`computer` 原生工具只保留 `version/download/install/uninstall`：版本来自实际 `--version`；下载验证官方稳定版资产；安装使用私有管理目录；卸载要求真实 `installation_id` 与所有权、文件身份、字节一致。不覆盖外部安装，也不修改 PATH。[工具与 CLI 参数](computer-use.md)

旧桌面、CDP、Omarchy、Playwright 专用桥和内部 Zuse worker 已撤去，没有隐藏执行 fallback。旧电脑控制开关、内置执行兼容入口和默认 `Super + Alt + C` 快捷键一并移除，独立 CLI 不增加 takeover 前置流程。OAuth 浏览器登录、显式本地导航和外部 MCP 通用转发保留。Windows 安装器不再自动安装浏览器依赖。[浏览器工作流](browser-desktop.md) · [Windows 安装范围](windows-browser-setup.md)

官方资产下载采用项目显式配置的 TLS 客户端，保留 HTTPS、重定向限制、大小上限与校验和检查。缺少官方目标的平台继续明确报错。

通用 exec 和独立 CLI 仍使用客户端的操作系统权限；不宣称通过界面状态就能撤销同一系统用户下其他程序的桌面访问。

## 版本号入口与跨平台 CI

Bump 工作流支持 `none`、`patch`、`minor`、`major` 和 `custom`，默认 `none` 使用当前版本；`dry_run` 提供预览，`publish` 选择是否继续发布。`none` 仍可以准备当前版本标签并触发 Release。[版本号工作流](../.github/workflows/bump.yml)

版本准备以首次运行的 `github.sha` 为基准，重复运行复用确定的版本提交和标签，只做正常快进。同名标签若已经指向其他提交，会拒绝继续。同一个 Bump run 的后续 attempt 在发布 POST 前停止自动重发；响应不确定时保留原始请求和输出，供核对已有运行。[准备与发布逻辑](../scripts/bump_release.py)

本次同步两类正式 CI 测试前提。原生协议测试明确要求五个实际可选提供者：Codex、Claude、Pi、OpenCode 和 SSMCP；Playwright 不再注册为受支持的夹具，并检查它不会被探测或启动；惰性加载、唯一记录和零外部调用等断言保留。安装安全性测试使用不会执行的合成归档和每个 Store 独立的测试目标，使测试不依赖宿主架构是否有官方 Zuse 资产。测试目标仅在测试构建中启用，生产目标选择和安装／卸载安全检查保留。[协议夹具](../scripts/e2e_agents.py) · [安装安全测试](../src/computer/tests.rs)

## 依赖和版本

内部跨仓库依赖使用规范 Git 来源，实际提交由 Cargo.lock 记录。Rust 直接源码保留 zharness、zplugin 和 ztuio 三个 gitlink；Zuse 已从 Rust 依赖与 gitlink 中移除，改为独立 CLI 生命周期管理。归档处理的实际依赖版本同样以锁文件为准。[依赖来源和核对方法](dependencies.md)

`ssmcp --version` 保持单行输出。`ssmcp --version --all` 离线显示构建期 Cargo.lock 中从 ssmcp 可达的依赖树，包含构建、开发和各锁定目标的依赖。实际链接的代码随平台和 features 而变。独立 Zuse CLI 的安装版本另用 `ssmcp computer version` 查询。

## 发布状态

本次准备六个平台的 GitHub 发布包和安装器。发布策略显式将 0.10.26 的 crates.io 发布延期，等待独立的 registry 包验证；生产 relay 部署也延期，以保留现有会话。最终构建、运行验收、发布与安装以实际工作流和发布清单为准。远端 `--input none` 的确认以实际中继 ACK 为准，本机 socket 使用记录不能代替这一证据。[发布流程](releases.md)
