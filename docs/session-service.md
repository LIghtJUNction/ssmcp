# 常驻会话服务与 attach

ssmcp 服务保存具名客户端会话，终端通过 attach 接入它们。会话继续拥有 SSH 连接、PTY 和命令；关闭 attach 窗口只分离显示与输入端。显式 close 会停止会话并保留配置，start 可以再次启动。

## Linux：安装、启用、启动

先完成正常的 SSH 公钥与主机配置，再安装客户端服务。系统服务始终以明确指定的普通用户运行，使用该用户的 HOME、状态目录和 SSH 配置：

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

在项目目录创建会话，或用 `--cwd` 明确指定工作目录。连接参数使用已经配置的 SSH 目标，例如：

```sh
ssmcp sessions create dev --cwd /path/to/project \
  --watchdog --allow octocat@github my-ssmcp

ssmcp sessions list --json
ssmcp sessions status dev --json
ssmcp sessions restart dev
ssmcp sessions close dev
ssmcp sessions start dev
ssmcp sessions remove dev
```

将 `octocat@github`、项目路径和 `my-ssmcp` 替换为自己的配置。名称直接跟在 create 后，不重复传 `--name`。同名 create 明确拒绝覆盖。`--watchdog` 控制失败重连和崩溃恢复；手动 close 优先于该策略。

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

## 连接与分离终端

```sh
ssmcp attach
ssmcp attach list
ssmcp attach dev
ssmcp attach --read-only dev
ssmcp attach -- list
```

裸 attach 始终列出保存的会话；不会自动选择唯一会话，也不创建不存在的会话。最后一条用于名称恰好为 `list` 的会话。

交互前端复用正常日志渲染与终端面板，保留真实脚本、文件操作、输出和状态。Ctrl-] 分离并恢复本地终端模式；Ctrl-C 发送给会话中选中的 PTY。F7 切换独立终端，F6 切换到可滚动日志区域。只读前端不发送输入，不改变工作终端尺寸。

一个会话同时只允许一个交互输入前端，其他连接可使用只读模式。输入控制权带租约，异常关闭的前端不会永久占住会话。历史事件保存在有界缓冲区；早期内容已经滚动丢失时会明确标记缺口。初次 SSH 连接失败时仍能 attach 查看重试日志；在后端尚未就绪时，输入会被拒绝，不排队留给未来连接执行。

分离后会话中的命令继续运行。SSH 连接重建、显式 restart 或关闭会话会结束原后端及其连接所属任务；已经连接原后端的前端会明确结束，重新 attach 同名会话即可连接新实例。这个边界与普通窗口分离不同。

## 原子管理工具

只有显式使用 `--session-control` 的连接注册会话管理工具。给专用管理连接授予这一能力：

```sh
ssmcp sessions create manager --cwd /path/to/projects \
  --session-control --watchdog --allow octocat@github my-ssmcp
```

通过公开 list 选择它的唯一 client ID，再调用：

```javascript
const inventory = await sessions.list(managerClientId);
const created = await sessions.create(managerClientId, {
  name: "worker",
  cwd: "project-a",
  title: "Run project checks",
  watchdog: true,
  reconnect_delay_secs: 5
});
const current = await sessions.status(managerClientId, { name: "worker" });
await sessions.restart(managerClientId, {
  name: "worker",
  expected_generation: current.generation
});
const replacement = await sessions.status(managerClientId, { name: "worker" });
await sessions.close(managerClientId, {
  name: "worker",
  expected_generation: replacement.generation
});
```

`inventory.sessions` 是会话数组。便捷入口依赖新版服务端的 code_mode 脚本；原生形式保持为 `call(managerClientId, "ssmcp.session.create", {...})`，返回 MCP structuredContent。工具名为 `ssmcp.session.list/status/create/restart/close`，公开 MCP 入口仍只有 list 与 code_mode。

新建会话继承管理连接的 SSH 目标、配置和 GitHub 允许列表；原子参数不允许替换身份或传入任意启动 argv。cwd 必须是该连接工作区内的现有目录，拒绝父级遍历和越界符号链接。子会话默认不继承 session-control 授权。带 `expected_generation` 的操作在串行管理事务中检查实例是否变化，避免基于旧状态关闭新进程。

关闭或重启管理连接自身，应使用另一个管理连接或本机 CLI。自操作会在发起停止前被拒绝，保证管理请求的结果仍能返回。

## 兼容、升级与本地通信

原有 `--daemon` 在管理服务运行时创建受管会话；没有管理服务时仍支持原来的 per-session native 管理。旧 native 定义保持原有管理方式，列表明确标出它们。前台进程不会自动变成托管会话；需要 attach 的工作从 sessions create 或后台模式启动。

会话继续运行创建时保存的可执行文件快照。restart 保留该快照及配置；升级主安装入口后，要升级已有会话版本，应处理好其输出、remove 旧定义，再用相同连接参数重新创建。这样重启不会悄悄改变工作会话的软件版本。

本地管理与 attach 通道只监听 loopback，并使用随机令牌和当前用户私有描述文件进行认证。Unix 状态目录与文件分别要求0700、0600；Windows 使用当前用户的受保护 DACL。认证信息不进入普通状态、日志或截图。管理器通过持有的进程对象、运行锁及 generation 判断所有权，不使用旧 PID 记录去终止未知进程。

设计参考：[Zellij session commands](https://zellij.dev/documentation/commands)、[systemd.service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)。平台安装与限制见[平台说明](client-service-platforms.md)，原有后台与输出转储功能见[会话控制](session-control.md)。
