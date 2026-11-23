# 常驻会话服务与 attach

ssmcp 服务保存具名客户端会话，终端通过 attach 接入它们。会话继续拥有服务器连接、PTY 和命令；关闭 attach 窗口只分离显示与输入端。显式 close 会停止会话并保留配置，start 可以再次启动。

`ssmcp sessions list` 与裸 `ssmcp attach` 列出本机保存会话，`ssmcp attach --local NAME` 连接指定本机会话。查看已授权的在线会话使用 `ssmcp list`；进入远程聊天使用 `ssmcp attach process PROCESS_ID`，其中 `PROCESS_ID` 来自 `ssmcp list`，不是操作系统 PID；进入远程 Shell PTY 使用 `ssmcp -i`，详见[客户端账号与授权](client-accounts.md)。

## Linux：安装、启用、启动

下方示例使用 HTTPS 账号连接。先以运行服务的同一普通用户执行 `ssmcp login --server https://relay.example` 完成授权，再安装客户端服务和创建会话。系统服务始终以明确指定的普通用户运行，使用该用户的 HOME、状态目录和连接配置。

```sh
sudo "$(command -v ssmcp)" service install --system --user "$(id -un)"
sudo systemctl enable --now ssmcp

systemctl is-enabled ssmcp
systemctl is-active ssmcp
ssmcp service status --system --json
```

标准命令是 `systemctl`。系统服务单元名为 `ssmcp.service`，与远端网关的 `ssmcpd.service` 分开。只安装用户 scope 时使用 `ssmcp service install`，随后使用 `systemctl --user enable --now ssmcp`。两种 scope 共用同一用户的状态目录时，管理器锁会拒绝重复实例；选择其中一种常驻方式。

`service install` 会写入持久定义，并通过原生服务管理器启用、启动它；上面的 `enable --now` 可重复执行。`ssmcp service install --dry-run --json` 可查看计划；使用 system scope 时，dry-run 同样支持 `--system --user USER`。macOS、Windows 的完整命令及运行身份要求见[平台服务说明](client-service-platforms.md)。

## 创建和管理

在项目目录创建会话，或用 `--cwd` 明确指定工作目录。连接参数使用账号与中继服务器，例如：

```sh
ssmcp sessions create dev --cwd /path/to/project alice@relay.example

ssmcp sessions list --json
ssmcp sessions status dev --json
ssmcp sessions restart dev
ssmcp sessions close dev
ssmcp sessions start dev
ssmcp sessions remove dev
```

将账号、服务器和项目路径替换为自己的配置。名称直接跟在 create 后，不重复传 `--name`。同名 create 明确拒绝覆盖。

新会话默认启用 watchdog，连接失败后重试，并由管理器恢复失败退出的工作进程。使用 `--no-watchdog` 禁用；`--watchdog` 保留为显式启用的兼容参数。重试默认等待 5 秒，`--reconnect-delay SECONDS` 可设为 1–3600 秒，不需要另加 `--watchdog`，也不能与 `--no-watchdog` 同用。手动 close 优先于自动恢复策略。

| 操作 | 执行结果 | 保存配置 |
| --- | --- | --- |
| create | 创建定义并启动一个工作进程 | 新建 |
| restart | 确认旧执行树退出，再启动新的 generation | 保留 |
| close / stop | 先保存停止意图，再确认执行树停止 | 保留 |
| start | 显式恢复停止的会话 | 保留 |
| remove | 停止并删除该会话定义 | 删除 |
| 停止管理服务 | 清理其拥有的工作进程 | 保留原运行意图 |
| 再启动管理服务 | 恢复期望运行的受管会话 | 已关闭会话保持停止 |

`generation` 是一次工作进程实例的标识。名字保持不变，restart 或服务恢复会生成新 generation，Broker `client_id` 也会重新生成。状态分别报告管理器状态和 Broker 连接状态：create 成功只能证明定义被接收、进程开始启动，实际连通应以 `connection_state: connected` 为准。

## 后台会话的 CPU 与内存限制

CPU 与内存限制通过已有的后台入口设置，保存后随同名会话的 start、restart 和故障恢复重新应用。例如：

```sh
ssmcp --daemon --name limited --input none \
  --cpu-limit 50% --memory-limit 512MiB alice@relay.example

# 已运行常驻管理服务时，也可以使用：
ssmcp sessions create limited-managed --cwd /path/to/project --input none \
  --cpu-limit 200% --memory-limit 1GiB alice@relay.example

ssmcp sessions status limited --json
```

`--cpu-limit` 接受正整数百分比，可省略百分号。100% 表示一个 CPU 的时间额度，200% 表示最多两个 CPU 的总时间额度；它不会固定进程到某个核心。`--memory-limit` 接受正整数字节数，或 K/M/G/T、KiB/MiB/GiB/TiB 后缀，每一级按 1024 换算。零、负数、小数、溢出和重复参数会被拒绝。两项均可单独使用；前台连接和本地工具清单不接受这些参数。

限制作用于每个会话的 cgroup，以及正常继承它的子进程。内存额度是整个组的合计，不是每个进程各有一份；`MemoryMax` 不包含单独的 swap 上限。内核在内存超限且无法回收时可能终止组内进程；runner 因此异常退出时仍遵循原有 watchdog 策略。[CPUQuota 与 MemoryMax 的定义](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html)

Linux 上有常驻管理服务时，每个受限 runner 使用独立的 `systemd-run --user --scope`，父子进程归属和停止操作仍由现有管理器负责；管理器按 `--reconnect-delay` 重试失败的 runner。未运行常驻管理服务时，`--daemon` 沿用原有临时 systemd service，并设置 `Restart=on-failure`。安装的常驻管理服务自身也由 systemd 在异常退出后重试，间隔为 5 秒。`--no-watchdog` 禁用会话级自动重试；显式 close/stop 会保存停止意图，正常退出也不会被误判成崩溃。[同步 scope 的进程关系](https://www.freedesktop.org/software/systemd/man/latest/systemd-run.html) · [systemd 重启规则](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)

资源限制要求 Linux、systemd 254 或更新版本、可连接的同用户 systemd manager，以及挂载在 `/sys/fs/cgroup` 的统一 cgroup v2。请求 CPU 或内存限制时，相应控制器必须可用并已委派；`--system` 安装的常驻管理器同样需要访问该用户的 systemd manager。程序不会自动安装服务、启用 linger 或更改控制器委派。macOS、Windows、不可用的用户 manager、旧 systemd 和缺少控制器的环境不能承诺这些限制：请求受限会话会被拒绝或启动失败，不会改为无上限运行。不指定限制时，现有平台后台路径继续可用。

runner 在建立连接前检查自己是否位于预期的会话单元，并读取内核的 `cpu.max`、`memory.max`；缺失、无限制或高于请求值时停止启动。`sessions status --json` 中的 `resource_limits_requested` 仅报告保存的配置。create/daemon 返回成功表示启动请求已接收，不等同于限制已经核验；应继续检查连接状态和服务日志。启动时的检查不代表之后对内核配置的持续监控。

## 连接与分离终端

```sh
ssmcp attach
ssmcp attach list
ssmcp attach dev
ssmcp attach --read-only dev
ssmcp attach -- list
```

裸 attach 始终列出保存的会话；不会自动选择唯一会话，也不创建不存在的会话。最后一条用于名称恰好为 `list` 的会话。

attach 复用客户端界面展示会话事件。日志区滚轮直接回看；`/audit` 打开源码审计，`/filter 文本` 筛选日志，F9 切换日志类别，F1 查看帮助。输入框与内容区通过 Tab 切换焦点，内容区中的 ↑/↓ 用于浏览记录。源码审计需要客户端与服务器都支持协议 v6 或更新版本，旧连接继续提供其已有事件。[界面与原子操作](client-ui.md)

Ctrl-] 分离并恢复本地终端模式，后台会话继续运行。仪表盘中的普通输入不会交给隐藏 PTY；需要直接交互时，在另一终端运行 `ssmcp -i`，或为 attach 使用 `dashboard = false` 的配置。直接终端输入模式中的 Ctrl-C 发送给选中的 PTY。只读前端不发送输入，不改变工作终端尺寸。

当前 attach 不转发自己的输入框要求队列，也不持有后台连接的队列传输。管理后台要求时，通过该在线客户端的 `ssmcp.ui.queue` 原子接口；重新启动后台会话使用 `ssmcp sessions restart NAME`。输入框 `/reload` 的重启与草稿交接功能用于直接持有服务器连接的前台客户端。

一个会话同时只允许一个交互输入前端，其他连接可使用只读模式。输入控制权带租约，异常关闭的前端不会永久占住会话。历史事件保存在有界缓冲区；早期内容已经滚动丢失时会明确标记缺口。初次连接失败时仍能 attach 查看重试日志；在后端尚未就绪时，输入会被拒绝，不排队留给未来连接执行。

分离后会话中的命令继续运行。连接重建、显式 restart 或关闭会话会结束原后端及其连接所属任务；已经连接原后端的前端会明确结束，重新 attach 同名会话即可连接新实例。这个边界与普通窗口分离不同。

## 原子管理工具

只有显式使用 `--session-control` 的连接注册会话管理工具。给专用管理连接授予这一能力：

```sh
ssmcp sessions create manager --cwd /path/to/projects \
  --session-control alice@relay.example
```

通过公开 list 选择它的唯一 client ID，再调用：

```javascript
const inventoryReply = await call(managerClientId, "ssmcp.session.list", {});
if (inventoryReply.isError) return inventoryReply;
const inventory = inventoryReply.structuredContent;
const created = await call(managerClientId, "ssmcp.session.create", {
  name: "worker",
  cwd: "project-a",
  title: "Run project checks",
  watchdog: true,
  reconnect_delay_secs: 5
});
if (created.isError) return {inventory, created};
const currentReply = await call(managerClientId, "ssmcp.session.status", {name:"worker"});
if (currentReply.isError) return currentReply;
const restarted = await call(managerClientId, "ssmcp.session.restart", {
  name: "worker",
  expected_generation: currentReply.structuredContent.generation
});
if (restarted.isError) return restarted;
const replacement = await call(managerClientId, "ssmcp.session.status", {name:"worker"});
if (replacement.isError) return replacement;
return await call(managerClientId, "ssmcp.session.close", {
  name: "worker",
  expected_generation: replacement.structuredContent.generation
});
```

`inventory.sessions` 是结构化正文中的会话数组。当前 SDK 已删除 `sessions` 便捷入口；使用精确原生名称调用，检查 `isError` 后读取 `structuredContent`。工具名为 `ssmcp.session.list/status/create/restart/close`。公开 MCP 提供 `list`、`code_mode`、`collab`、`await` 和 `messages`；会话管理通过 `code_mode` 调用，`collab` 用于在线客户端内的成员通信。[Agent 协作](agent-collaboration.md)

新建会话继承管理连接的连接目标、配置和账号允许列表；原子参数不允许替换身份或传入任意启动 argv。cwd 必须是该连接工作区内的现有目录，拒绝父级遍历和越界符号链接。子会话默认不继承 session-control 授权。带 `expected_generation` 的操作在串行管理事务中检查实例是否变化，避免基于旧状态关闭新进程。

关闭或重启管理连接自身，应使用另一个管理连接或本机 CLI。自操作会在发起停止前被拒绝，保证管理请求的结果仍能返回。

## 兼容、升级与本地通信

旧网关兼容只在初次注册收到结构化 `Reject` 帧、且拒绝内容匹配协议版本错误时启用。客户端尝试构建支持的兼容版本，随后沿用协商结果处理普通断线重连；支持的版本均被明确拒绝时，该 runner 失败退出。启用了 watchdog 的管理器仍按原有延迟启动新的 runner。网络错误、认证失败和其他拒绝不会触发协议回退。

v4 兼容连接不向网关发送任务 `title`，本地保存的标题保持不变；旧协议也不提供每次调用的 `controller` 信息。账号允许列表与 OAuth 授权规则保持不变。更早的网关如果无法解析初始 Hello、因而没有返回结构化版本拒绝，不会触发回退。`reload` 仍等待并校验服务端 receipt；没有确认时不执行重载，协议回退不会绕过这项校验。

原有 `--daemon` 在管理服务运行时创建受管会话；没有管理服务时仍支持原来的 per-session native 管理。旧 native 定义保持原有管理方式，列表明确标出它们。旧保存会话继续采用 manifest 记录的 watchdog 策略，原来关闭的不会因默认值变化而自动开启。前台进程不会自动变成托管会话；需要 attach 的工作从 sessions create 或后台模式启动。

会话继续运行创建时保存的可执行文件快照。restart 保留该快照及配置；升级主安装入口后，要升级已有会话版本，应处理好其输出、remove 旧定义，再用相同连接参数重新创建。这样重启不会悄悄改变工作会话的软件版本。

本地管理与 attach 通道只监听 loopback，并使用随机令牌和当前用户私有描述文件进行认证。Unix 状态目录与文件分别要求0700、0600；Windows 使用当前用户的受保护 DACL。认证信息不进入普通状态、日志或截图。管理器通过持有的进程对象、运行锁及 generation 判断所有权，不使用旧 PID 记录去终止未知进程。

设计参考：[Zellij session commands](https://zellij.dev/documentation/commands)、[systemd.service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)。平台安装与限制见[平台说明](client-service-platforms.md)，原有后台与输出转储功能见[会话控制](session-control.md)。
