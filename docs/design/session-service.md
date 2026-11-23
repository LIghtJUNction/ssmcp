# 常驻服务与本机会话：接口约定

状态：已实现的接口约定。运行验证结果和平台覆盖范围见[最终验收报告](../reports/session-service-2026-11-17/)；本文不替代平台现场验证。

## 目标与边界

ssmcp 的常驻客户端服务负责保存和管理本机会话，终端前端负责显示和输入。关闭 attach 前端不结束工作会话；显式关闭会话才保存停止意图并终止其拥有的执行树。远端 MCP 网关仍使用 ssmcpd，客户端服务使用 ssmcp，两者保留独立的运行身份、服务名和配置。

借鉴 Zellij 的具名会话与 attach/detach 生命周期。用户要求的裸 `ssmcp attach` 固定等于 `ssmcp attach list`；不会因为只有一个会话就自动接入，也不隐式创建缺失会话。[Zellij commands](https://zellij.dev/documentation/commands) · [Detach](https://zellij.dev/documentation/keybindings-possible-actions#detach)

## 命令语义

| 入口 | 行为 |
| --- | --- |
| `ssmcp service install` | 安装当前用户的持久服务定义 |
| `ssmcp service install --system --user USER` | 安装系统 scope 的定义，明确普通用户运行身份 |
| `sudo systemctl enable --now ssmcp` | Linux 系统服务启用并立即启动 |
| `ssmcp sessions create NAME [options] HOST` | 保存新会话并交给正在运行的管理服务启动 |
| `ssmcp sessions restart NAME` | 确认旧实例退出后启动新的 generation |
| `ssmcp sessions close NAME` | 持久保存停止意图并停止执行树，保留配置 |
| `ssmcp sessions start NAME` | 显式重新启动已保存的停止会话 |
| `ssmcp sessions remove NAME` | 停止并删除配置；与 close 分开 |
| `ssmcp attach [NAME]` | 有名字接入现有会话，无名字列出会话 |
| `ssmcp attach --read-only NAME` | 只读观察，不取得输入和尺寸控制权 |
| `ssmcp attach -- list` | 接入名称恰好为 list 的会话 |

交互 attach 使用 Ctrl-] 分离；Ctrl-C 传给选中的 PTY。一个会话同时只允许一个交互输入前端，其他连接可只读观察，避免输入与窗口尺寸相互覆盖。

## 系统服务

系统服务以明确配置的普通用户运行，不能因安装时使用 sudo 而让项目命令获得 root 身份。Linux 使用持久 systemd unit，macOS 区分登录用户 LaunchAgent 与启动时 LaunchDaemon，Windows 区分交互用户任务与 SCM 原生服务。

Linux `systemctl` 默认是系统 scope，`--user` 是用户 scope；`enable --now` 同时启用和启动。Windows SCM 入口必须提供 ServiceMain 和控制处理接口，及时报告运行与停止状态。macOS LaunchAgent 在用户注销时结束；Windows Session 0 服务也不等同于登录桌面。桌面工具应选用户 scope。[systemctl](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html) · [systemd.service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) · [Apple launchd](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html) · [Windows control handler](https://learn.microsoft.com/en-us/windows/win32/services/service-control-handler-function) · [Interactive services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services)

## 原子能力与授权

新增原生工具 `ssmcp.session.list/status/create/restart/close`，经现有 `list` 与 `code_mode` 发现和调用。JavaScript 使用 `call(clientId, "ssmcp.session.<operation>", arguments)`，其中 operation 使用目录中的精确操作名。

管理连接须显式使用 `--session-control`。新建会话继承该连接的 SSH 目标、SSH 配置和 GitHub 允许列表；原子参数不接受任意 argv、身份或允许列表。工作目录必须在该连接已授权的工作区内，子会话默认不继承管理授权。重启或关闭管理连接自身须通过另一个管理连接或本机 CLI，以免请求在返回确认前切断自己。

管理 IPC 只监听 loopback，使用随机令牌和当前用户私有描述文件；不公开令牌、身份文件或完整启动参数。状态中的 generation 用来区分同名会话的新旧运行实例；可通过 expected_generation 拒绝基于过期状态的操作。

## 生命周期与验收标准

关闭先持久保存停止意图，再终止所属执行树。管理服务恢复时只恢复期望运行的受管会话，已关闭会话不得被 watchdog 复活。重启在同一串行管理事务中完成停止确认和新实例建立；进程状态与 Broker 连接状态分别报告，不能把进程已启动写成已连通。

现有 native 后台会话保留原管理机制；新受管会话明确记录管理类型。每个会话继续使用可验证的可执行文件快照，避免无意运行被替换的程序。attach 接入原进程，不重跑启动命令；重启导致原前端失效时明确结束旧连接，不重放输入。

验收应覆盖：同名并发创建、restart/close 竞争、停止确认、服务重启恢复、关闭后不恢复、输入与尺寸归属、detach 后任务持续运行、再次 attach 的输出连续性、源码日志脱敏、跨用户 IPC 拒绝，以及三种平台的构建和各自可执行的原生验证。不能用交叉编译结果代替未执行的平台现场验证。
