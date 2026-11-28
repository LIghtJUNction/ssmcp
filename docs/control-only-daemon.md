# 仅控制模式

在希望 Agent 使用的工作目录启动前台连接。把 `user@server` 换成这台设备允许控制的中继账号。

```sh
ssmcp user@server --control-only
```

客户端启动一个系统 Shell，在终端显示命令、输出和退出状态。前台模式的标准输出提供一条 JSON 连接记录，活动日志走标准错误。使用 `--shell /bin/sh` 可选择 Shell，`--log-format jsonl` 可把日志切换为机器可读格式。Ctrl+C 会结束连接并清理它启动的进程。

完成首次设备授权后，可启动一个用户自己的后台会话。

```sh
ssmcp user@server --control-only --daemon --name control
ssmcp sessions status control
ssmcp sessions logs control --lines 50 --pretty
ssmcp sessions stop control
```

`ssmcp sessions logs` 读取运行中会话的有界内存日志。连接断开后，可用 `ssmcp sessions start control` 重新启动。控制请求和命令不会自动重试或重放。

## Agent 怎样操作 Shell

在 MCP 应用中用 `list` 选中本次连接。系统 Shell 的任务 ID 是 `main`。Agent 可以通过 `ssmcp.exec.write` 向它输入命令，用 `ssmcp.exec.read` 读取输出。Shell 的输入回显和输出也会显示在本机日志中。用 `ssmcp.exec.start` 可以执行独立命令并取得单独的任务 ID。

此模式关闭聊天、Worker、外部 Agent 启动和工作流。工具目录限于原生命令管理和能力查询。本机控制者审批及 Linux 的会话和命令内存限制仍然生效。Linux 需要现有的 systemd 用户管理器提供资源委托。
