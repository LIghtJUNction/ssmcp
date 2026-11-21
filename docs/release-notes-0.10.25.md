# SSMCP 0.10.25

## 启动和聊天

`--input` 选择聊天输入，默认 `terminal`。`--agent` 选择聊天处理方，默认 `next-caller`，由下一次符合条件的工具调用领取。用 `--list-harnesses` 查看支持的原生协议名称，用 `--load-harness` 选择启动时加载的协议。原生尾参放在 `--` 后。只保存了旧协议选择 `--agent` 的会话需用新选项重建。[客户端输入](client-input.md) · [原生协议加载](agent-entry.md)

`--input none` 关闭本连接的聊天框、人类提示和相关工具。远端连接必须收到中继确认关闭人工聊天的真实 ACK；缺少确认时连接会退出。执行、已协商的时间观察、独立原生消息和重载仍按原有权限提供。成员可继续互发原生消息。启用聊天的连接可通过 `ssmcp.ui.reply` 回传已认证线程的进度和实际结果。人工输入与决定以真实用户提交为准。[线程消息](thread-messaging.md) · [问题与回复](finish-and-questions.md)

## 查看调用信息和返回 ChatGPT

在当前已保存的对话中，`/openai` 查看最近观察到的调用标识。`/openai-session` 和 `/openai-subject` 分别查看对应字段。界面显示来源、缺失状态和观察时间。标识来自受支持且已验证的调用线程，可用于关联请求。

用 `/chatgpt set <URL>` 保存当前对话的 ChatGPT 链接，再用 `/chatgpt open` 或 `/chatgpt` 请求浏览器打开。`/chatgpt copy` 复制网址，`/chatgpt clear` 删除该对话保存的网址。`/plugin-docs` 打开官方 Plugins 参考。打开或复制均由显式命令触发；标识不会被拼成网址，也不会自动发送消息。[命令说明](commands.md#查看-openai-调用标识和打开-chatgpt)

## 引用、历史和粘贴

普通消息引用缺失的 `@文件` 时会显示警告，消息仍会发送并保留引用原文。`$skill` 按有效作用域解析，项目来源优先。使用技能前应通过返回的读取入口查看完整内容。原生消息和组发保留字面正文。[引用规则](client-input.md) · [原生消息](thread-messaging.md)

已归属会话的回复保留原历史归属，并沿用原有幂等回执。终端批量解码粘贴内容，保留按键顺序、输入限制和草稿。显式选择 Next caller 后使用 `next` 目标投递。[会话历史](conversation-history.md) · [粘贴回归定义](../src/client/terminal/conversation/tests/paste_batch.rs)

## 原生图片

同一次原生工具调用返回的多张图片分别显示在独立卡片中。预览按各自的事件 ID 关联，保留来源、原会话和调用身份。工具摘要继续独立计数。[图片事件](../src/client/terminal/events.rs#L1550) · [会话与调用回归定义](../src/client/terminal/events/source_tests.rs#L456)

## 独立 Zuse CLI

桌面和浏览器主操作直接调用独立 Zuse CLI，经 SSMCP 通用 exec 管理进程、输入和输出。`computer` 原生工具只保留 `version/download/install/uninstall`：版本来自实际 `--version`；下载验证官方稳定版资产；安装使用私有管理目录；卸载要求真实 `installation_id` 与所有权、文件身份、字节一致。不覆盖外部安装，也不修改 PATH。[工具与 CLI 参数](computer-use.md)

旧桌面、CDP、Omarchy、Playwright 专用桥和内部 Zuse worker 已撤去，没有隐藏执行 fallback。默认 `Super + Alt + C` 快捷键移除；显式 `on/off/toggle/status` 与 `/computer` 保留为旧版 SSMCP 兼容状态入口，只供仍遵守旧门禁的会话读取，不控制本版工具、普通 exec 或独立 Zuse CLI。状态查询不初始化文件，独立 CLI 不增加 takeover 前置流程。[兼容开关](computer-use.md#兼容旧版-ssmcp-的手动开关)OAuth 浏览器登录、显式本地导航和外部 MCP 通用转发保留。Windows 安装器不再自动安装浏览器依赖，旧 `SkipBrowserDependencies` 仅作为无操作兼容参数接受。[浏览器工作流](browser-desktop.md) · [Windows 安装范围](windows-browser-setup.md)

通用 exec 和独立 CLI 仍使用客户端的操作系统权限；不宣称通过界面状态就能撤销同一系统用户下其他程序的桌面访问。

## 依赖和版本

内部跨仓库依赖使用规范 Git 来源，实际提交由 Cargo.lock 记录。Rust 直接源码保留 zharness、zplugin 和 ztuio 三个 gitlink；Zuse 已从 Rust 依赖与 gitlink 中移除，改为独立 CLI 生命周期管理。归档处理的实际依赖版本同样以锁文件为准。[依赖来源和核对方法](dependencies.md)

`ssmcp --version` 保持单行输出。`ssmcp --version --all` 离线显示构建期 Cargo.lock 中从 ssmcp 可达的依赖树，包含构建、开发和各锁定目标的依赖。实际链接的代码随平台和 features 而变。独立 Zuse CLI 的安装版本另用 `ssmcp computer version` 查询。

## 发布状态

本次准备六个平台的 GitHub 发布包和安装器。crates.io 发布延期，等待独立的 registry 包验证；生产 relay 部署也延期，以保留现有会话。最终构建、运行验收、发布与安装以实际工作流和发布清单为准。远端 `--input none` 的确认以实际中继 ACK 为准，本机 socket 使用记录不能代替这一证据。[发布流程](releases.md)
