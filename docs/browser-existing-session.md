# 用 Zuse 连接已有浏览器

用户指定现有 Chrome 或其他浏览器时，先确认那一个进程与会话是否能通过所选 Zuse CLI 的真实接口访问。SSMCP 负责运行 CLI 和返回结果；不再提供 `ssmcp.browser.discover`、内置 CDP 控制或 Playwright 专用桥。[当前边界](browser-desktop.md)

## 先确认接口与目标

1. 用 `ssmcp computer version --json` 或 `computer.version(clientId)` 读取实际 CLI 版本与路径。
2. 通过通用 exec 读取该程序的 `--help` 和 `schema`，确认支持的浏览器操作。
3. 使用操作者明确提供或已观察到的调试端点。不要猜端口、读取扩展令牌，或因连接失败而自动打开另一个 profile。
4. 在同一 Zuse 进程里读取真实标签页目录，再选择用户指定的页面；页面 ID 来自该次目录，不能复用旧 SSMCP 的 target ID。

[Zuse 当前文档](https://github.com/LIghtJUNction/zuse#background-browser) 中，`browser.launch` 创建隔离的临时 profile；`browser.connect` 连接明确的 loopback 调试端点并创建一个新页面；`browser.tabs` 与 `browser.select` 用于发现并选择已有页面。这些是 **Zuse 程序里的操作名**，不是 SSMCP 的 JavaScript helper。执行前以已安装版本的 schema 为准，并把连接本身可能创建页面的行为纳入操作范围。

## 保持会话与实际结果

需要多轮观察和操作时，使用 Zuse 文档提供的持久模式，并保留承载该进程的 exec job。一次性 CLI 退出后，不应假定下一次调用仍拥有相同浏览器会话。相关步骤按顺序执行，输出从该任务的 stdout/stderr 和实际产物读取。[Zuse 进程状态](https://github.com/LIghtJUNction/zuse#readme) · [exec 生命周期](code-mode.md#exec)

取消 exec job 后，先核实进程状态，再重新观察浏览器。进程退出不等于页面变更被撤销；不要仅因响应未返回而重复点击、提交或支付。保留真实错误与部分结果，不把缺失接口包装成成功。

## 登录与外部工具

SSMCP 的账号登录仍可打开浏览器授权页；`--no-browser` 仍用于不自动打开图形浏览器的登录流程。这与自动化接口相互独立。

调用方应用的 Browser 插件，以及已授权外部 MCP 的通用转发，继续由各自真实目录、会话和审批机制约束。SSMCP 不从扩展安装文件推断可用连接，也不恢复已撤去的专用浏览器适配器。指定通过 SSMCP 执行的桌面与浏览器任务，应直接使用 Zuse CLI。

过去的现场清单和验证记录保留在历史报告中，不代表当前客户端仍提供那些旧工具。
