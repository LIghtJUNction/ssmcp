# 原生工具与显式 MCP peer

SSMCP 提供原生的命令执行、文件、会话、worker、用户交互和工具目录。查看 `ssmcp --list-tools`，或通过账号授权后的 MCP 目录读取已注册工具。

## 迁移旧启动选项

移除启动脚本或保存会话中的 `--load-harness codex|claude-code|claude|cc|pi|opencode`，以及该选项后由 `--` 分隔的专用参数。需要保留工具配置或会话历史时，继续用原工具读写。SSMCP 不会改写这些文件。曾使用 `/cc`、`/codex`、`/pi` 或 `ssmcp.ui.harness` 的调用方可改用独立 MCP 客户端或显式原生命令 `!` 与 `ssmcp.exec.start`。

`ssmcp --list-harnesses` 列出 `ssmcp` 与 Linux 上可用的 `zrun`。`ssmcp.harnesses.list` 查看连接状态，`ssmcp.harnesses.load` 连接启动时明确配置的网关或私有 socket。读目录不触发进程启动或软件探测。 [显式认证 MCP peer 与 zrun](agent-entry.md)
