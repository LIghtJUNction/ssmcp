# SSMCP 0.10.39

## 客户端与协作

- macOS 与 Windows 客户端通过共享文本读取 Dreaming 指引。Linux 客户端沿用同一份文本。邀请和结束提示说明何时可以请求离线，以及如何等待人类对该请求的答复。
- 该补丁接续 0.10.38 标签中的主协调者状态栏、Dreaming 计时、普通事件提醒合并和 Linux 剪贴板文本兼容更新。

## 配套 zrun

- Linux 原生交互 shell 支持在光标位置按 Tab 补全命令和路径。上下方向键浏览近期的本地历史，右方向键或 End 接受灰色建议。多行粘贴后按回车才执行。
- `zrun ai config` 输出供 MCP 客户端使用的连接配置。`zrun ai connect --stdio` 建立本地控制通道。`zrun ai run` 返回命令的退出码及分别处理的标准输出和错误输出，`zrun ai compact` 可直接读取已有文本或二进制流。
- MCP 工具 `shell_run_compact` 折叠连续相同的完整行，并为较长输出生成有界预览。`output_read` 支持按页取回已捕获的原文，读取不会重新执行命令。短输出可继续使用原始 `shell_run`。同一端点最多保留 64 份捕获，每份最长保留 5 分钟。
