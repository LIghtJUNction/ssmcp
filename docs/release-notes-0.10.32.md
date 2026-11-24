# SSMCP 0.10.32

- Linux 和 macOS 的 `ssmcp.exec.*` 默认通过进程内的 zrun 库启动和监督命令。管道继续流式返回原始 stdout、stderr，stdin 继续报告实际写入字节数。
- Linux 上的 `mode:"pty"` 使用 zrun 读写终端并等待退出。工作区授权、任务记录、输出游标和 Linux 进程树清理沿用现有规则。
- Windows 保留原生进程、Job Object 和 ConPTY 路径。
- HTTP、TLS 和 WebSocket 继续使用现有依赖，应用仍包含 Tokio。
- Linux 仍可显式加载独立的 zrun 命令服务。默认 `exec` 开箱可用。 [zrun 原生命令与显式服务](agent-entry.md)
