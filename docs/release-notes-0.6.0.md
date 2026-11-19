# ssmcp 0.6.0 更新说明

0.6.0 将客户端改为统一能力桥接：始终提供交互式 Shell 和独立命令，在后台发现可安全接入的原生接口。服务端对外仍只有 `list` 与 `code_mode`；工具目录、调用路由、输出存储和事件观察在客户端内统一管理。本文描述代码变更；发布、部署与实机验收状态分别记录在[0.6.0 验收材料](validation-0.6.0.md)，不能由版本号推断。

## 升级时删除后端选择参数

`-t`、`--type`、`--backend` 及后端透传参数不再有效。旧命令中的 SSH 目标、公钥配置、`--name`、`--title`、`--allow` 和日志选项继续使用。例如：

```sh
ssmcp --name workstation --title "修复项目并执行测试" \
  --allow octocat@github user@server
```

客户端不需要安装任何 coding harness 才能连接。交互式终端供人工操作；远程命令通过独立任务运行，不排进 PTY 的输入队列。所有新连接的 `type` 为 `shell`，判断某个原生工具能否调用应读取当前目录，不能再按连接类型选择 Codex 或 Claude。[参数与迁移](backends.md#从旧的后端选择参数迁移)

## 一个目录保留原生能力

`list({client_id})` 与 `tools(clientId)` 读取同一注册表。提供者命名空间分别为 `codex.*`、`claude.*`、`pi.*`、`opencode.*`；名称以实际发现为准。原生描述和 JSON Schema 保留，路由信息不应替代原生参数契约。目录更新和调用验证使用当前提供者状态，连接失效后不能依赖旧目录继续路由。[注册表](../src/backend.rs)

Codex 通过 App Server 的 `mcpServerStatus/list` 枚举已配置、可访问的 MCP 工具，并通过 `mcpServer/tool/call` 真实转发。名称采用 `codex.mcp.<server>.<tool>` 命名空间并进行稳定转义，不硬编码某个 MCP Server。完整分页成功才发布目录；刷新失败、取消或连接失效时清除过期 MCP 路由。结果中的 `content`、`structuredContent`、`isError` 和原生审批状态仍由原生接口决定。[Codex 适配器](../src/backend/codex.rs)

某个接口没有暴露内部工具 Schema 时，ssmcp 不补造 Schema，也不通过模拟人类终端交互制造完整 API。Claude 的工具来自 `claude mcp serve` 的真实 MCP 目录；该入口的权限行为由 Claude 本身决定，不能将它等同于交互终端中的确认体验。[原生接口边界](backends.md)

## 发现不安装依赖或提交模型任务

各提供者独立探测，Shell 不等待可选 Harness 完成初始化。PATH 解析会识别可用入口和 shim；Node 隔离执行器必须使用已安装、参数兼容的真实 Node 二进制，不能把 mise shim 当作运行时传给 `--jitless`。无法确认安全运行的入口报告 unavailable。[程序解析](../src/program.rs) · [启动测试](../tests/client_startup.rs)

| 提供者 | 自动发现的边界 |
| --- | --- |
| Codex | 已安装版本、App Server 原生握手及实际目录；发现不提交 turn |
| Claude Code | 已安装版本与原生 MCP 握手；只注册实际返回的工具 |
| Pi | 只连接 `SSMCP_PI_RPC_SOCKET` 指定的已初始化 Unix JSONL RPC 会话，检查绝对路径、私有权限、所有者、符号链接与 peer UID，再进行有期限的 `get_state` 握手 |
| OpenCode | 只连接 `SSMCP_OPENCODE_URL` 指定的 `http://127.0.0.1:PORT/` 和 `OPENCODE_PASSWORD`；发现仅读取 info 与 OpenAPI，事件订阅延后到明确调用 |

Pi 的四项桥接操作是 `pi.rpc`、`ssmcp.pi.events`、`ssmcp.pi.requests`、`ssmcp.pi.respond`；它们不等于可枚举的模型、插件或内部 MCP 工具。Pi 与 OpenCode 原生启动可能运行用户扩展、安装缓存依赖或启动已有任务，因此 ssmcp 不为枚举而启动这两类服务，也不禁用用户扩展来绕过原生审批。缺少安全既有入口时保留不可用诊断。固定版本的源码依据及精确连接要求见[Pi](backends.md#7-pi有限的原生-rpc-桥接)与[OpenCode](backends.md#8-opencode从-openapi-发现操作)。

## 独立命令与实时观察

`ssmcp.exec.start/list/status/wait/read/cancel/events` 管理独立子进程。每项任务有自己的 ID、工作目录、环境和 stdout/stderr 管道，多个任务可以并发。默认工作目录受授权工作区限制；取消、超时和连接关闭清理该连接拥有的任务。`start` 成功只代表启动成功，命令结果由最终状态、退出码或终止信号决定。[命令管理器](../src/backend/shell.rs)

Linux 使用监督进程管理任务和后代，状态中的 `pid_role` 为 `supervisor`；其他平台为 `command`。PID 角色不能替代最终退出状态。实时 `exec.stdout`、`exec.stderr` 随执行推送，读取历史输出与等待完成不会再次打印同一段内容。真实终端上的交互 PTY 与日志配色分开，关闭颜色不会删除光标、退格等终端控制序列。[进程管理](../src/platform.rs) · [终端渲染](../src/client/render.rs)

## 终端任务与 Git 状态区

交互式终端默认启用 `ui.dashboard`：宽窗口左侧隔离日志和人工 Shell，右侧连续展示活跃任务、耗时、原生进度、Git 状态及最近提交。历史来自实际 HEAD，最多五条，按剩余高度显示短 SHA、相对时间和最多两行标题；HEAD 变化或读取失败时保留明确的 pending、unavailable 或 stale 状态。窗口变化会重新计算布局；少于 96 列或 18 行时收起右栏。任务没有原生总量时只显示活动标记，只有合法的 progress/total 才显示百分比。Git 查询在授权目录后台运行，失败或过期状态不会被显示为干净工作区。[显示与关闭方式](logging.md#固定状态区与窗口尺寸)

任务、请求和工作区路径使用短摘要，stdout/stderr 保留来源；长内联脚本与 JSON 对象在默认 pretty 中折叠。已标明来源的内部发现重复成功摘要会收敛，错误和显式用户调用继续显示。JSONL 保留脱敏事件，输出原文仍按原有存储与查询契约读取。终端模式与控制字符处理不因 `NO_COLOR` 改变。[布局](../src/client/panel.rs) · [终端合成](../src/client/terminal.rs)

## 后台会话与显式重连

新增 `--daemon` 和本地 `ssmcp sessions` 管理命令，可列出、查询、启动、停止、重启、读取日志、显式读取登录码或移除保存的会话。`--watchdog` 在连接失败或断开后重试；`--reconnect-delay SECONDS` 默认 5 秒、范围 1–3600，必须同时启用 watchdog。默认不配置开机或登录自启，手动 stop 会抑制自动恢复。

状态分别报告原生管理器与 Broker 连接；启动成功不表示已在线。后台会话固定保存创建时的可执行文件和 SHA-256，升级须 remove 后重新创建，restart/reload 仍使用保存版本。断线会清理原连接任务，重连重新发现工具，不提供跨连接持久任务。

Linux 使用用户级 transient systemd service，macOS 使用当前用户 launchd，Windows 使用当前登录用户计划任务。Windows 原生进程崩溃恢复最少间隔 60 秒、最多 255 次，与进程内按配置间隔进行的 SSH 重连分开。Linux `sessions logs` 查询实际 unit journal，无法取得时明确退回生命周期记录；macOS/Windows 当前仅提供生命周期记录。后台会话实机验证与跨平台门禁分别记录，既有终端报告不能代替新增后台能力的验收。[操作与升级](session-control.md#后台运行与延时重连) · [日志范围](logging.md#后台会话日志)

## 审计基于已观察结果

文件读取、成功提交的写入及实际文本 Diff 各有事件；快照区分存在、缺失与未知，无法读取的内容不会被当作空文件来猜测增删行数。完整 Diff 可内联或通过同一输出存储查询。技能发现、正文读取与实际调用分别记录；发现 `SKILL.md` 不表示 AI 已使用技能。Harness 调用保留开始、结束、提供者、耗时和原生审批信息，对原生接口无法观察的外部文件副作用标记 unknown。[文件审计](../src/workspace/audit.rs) · [日志说明](logging.md)

事件归档与所有日志模式都使用脱敏副本，隐藏可识别凭据、私钥和敏感正文。任意原生 RPC result/error 不直接展开到日志；调用方获得的原生结果不因显示而改写。脱敏存在识别边界，日志本身不授予读取秘密的权限。[审计实现](../src/audit.rs)

Shell、PTY、Harness、MCP 和审计复用连接级 OutputStore，继续采用 32 KiB／500 行转储阈值、24 小时保留期和配额。元数据包含 artifact ID、路径、SHA-256、字节数、行数、完整性与过期时间；通过 `outputs.read/stat/cleanup` 管理。结构化结果和二进制不被统一改写为纯文本。新连接不能凭相同 OS 用户身份读取旧连接的输出。[输出与重载](session-control.md)

## 发布与部署保持独立校验

0.5.1 已有的六平台构建、源码包校验、固定 tag、附件摘要、安装器和发布重试约束继续生效。0.6.0 的 GitHub Release、crates.io 发布、客户端安装与服务器部署必须分别有成功证据。[发布说明](releases.md)

Arch 升级助手现在必填 `--expected-version`，同时核对包元数据、暂存二进制和已安装二进制；stage receipt 绑定版本、提交、包摘要和生产状态。旧 receipt 不能跨版本复用。原有私有 checkpoint、配置租约、DCR 重启验证和失败回滚保留。生产重启由独立 SSH/systemd 作业持有，执行步骤见[部署说明](deployment.md#日志和受控升级)。

本次版本说明没有宣称所有提供者均已实机接入。发布前还需将最终提交、CI、真实目录与调用、并发任务、终端审计、安装和线上版本记录到[验收材料](validation-0.6.0.md)。
