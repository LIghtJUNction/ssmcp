# Zuse CLI 使用入口

当前指南为 [Zuse CLI 管理与桌面、浏览器任务](computer-use.md)。SSMCP 的 `computer` 原生工具仅提供 `version/download/install/uninstall`，实际桌面与浏览器操作通过通用 exec 直接运行独立 Zuse CLI。

旧桌面、CDP、Omarchy、Playwright 专用桥和内部 worker 已撤去；没有兼容动作代理或隐藏 fallback。OAuth 浏览器登录和外部 MCP 的通用转发仍保留。

默认 `Super + Alt + C` 已移除；显式 `ssmcp computer on|off|toggle|status` 和 `/computer` 保留为[旧版 SSMCP 兼容开关](computer-use.md#兼容旧版-ssmcp-的手动开关)，只读写仍使用旧协议的会话状态。它不控制当前工具、通用 exec 或独立 Zuse CLI；独立 CLI 无需先切换开关。

[早期原生控制验收记录](validation-native-control.md) 与 [旧 computer-use 验收](validation-computer-use.md) 仅记录当时的实现和环境。
