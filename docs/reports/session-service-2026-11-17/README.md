# ssmcp 常驻服务与具名会话验收

日期：2026-11-17（Asia/Taipei）

## 结果

Linux 开发机已经安装并启用 **ssmcp 0.6.4** 的 `ssmcp.service`，服务以普通用户 `lightjunction` 运行。验收时系统状态为 `enabled / active / running`，运行中进程、安装入口及构建产物的 SHA-256 一致。验收时，保留的管理会话 `ssmcp-manager` 已连接现网网关。

五个原子工具已通过现网调用验证：`ssmcp.session.list`、`status`、`create`、`restart`、`close`。临时验收会话已关闭并删除。裸 `ssmcp attach` 与 `ssmcp attach list` 返回相同会话名称集合。

主体实现已通过 [PR #11](https://github.com/LIghtJUNction/ssmcp/pull/11) 合入。旧网关兼容修复及手册同步也已通过 [PR #12](https://github.com/LIghtJUNction/ssmcp/pull/12) 合入；本报告归档上述已执行的验收证据。完整运行验证的代码基线为 [868e92f5d2b39237a2bed62f45f9a666cd4f0192](https://github.com/LIghtJUNction/ssmcp/commit/868e92f5d2b39237a2bed62f45f9a666cd4f0192)；以下测试数据绑定此精确提交。

安装入口：`/home/lightjunction/.cargo/bin/ssmcp`

服务定义：`/etc/systemd/system/ssmcp.service`

二进制 SHA-256：

```text
5351b98f827922d376572905140bba3afc2edbf649c339c6a0316c09eb546b4f
```

依据：[安装记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/linux-install.json)、[最终状态与清理记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/live-session-lifecycle.json)、[现网原子工具记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/live-atomic-tools.json)、[构建记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/release-build.json)。

## 归档时的连接复核

现网原子操作的成功观测时间是 2026-11-17 13:58（Asia/Taipei）。归档期间再次调用 ssmcp 的公开 list 时，MCP 入口返回 `UNAVAILABLE / Connection failed`。这次错误发生在连接入口层，不能据此判定本机 systemd 服务已停止；当前无法重新读取机器上的服务或会话状态。已完成的安装、连接、原子操作及 attach 验收记录保持原样，详见 [连接复核记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/connection-recheck.json)。

## 常用命令

标准 Linux 管理命令使用 `systemctl`。安装会创建持久 unit、启用并启动服务；后面的 `enable --now` 可以重复执行。

```sh
sudo "$(command -v ssmcp)" service install --system --user "$(id -un)"
sudo systemctl enable --now ssmcp

ssmcp service status --system --json
systemctl is-enabled ssmcp
systemctl is-active ssmcp
journalctl -u ssmcp -f
```

会话命令以服务配置的普通用户执行：

```sh
# 当前可连接的管理会话
ssmcp attach
ssmcp attach list
ssmcp attach ssmcp-manager

# 在项目目录新建一个工作会话
ssmcp sessions create dev --cwd /path/to/project \
  --watchdog --allow lightjunction@github ssmcp@ssmcp.lmm.best

ssmcp sessions status dev --json
ssmcp sessions restart dev
ssmcp sessions close dev
ssmcp sessions start dev
ssmcp sessions remove dev

# 只读观察；名称恰好为 list 时使用 -- 消除歧义
ssmcp attach --read-only dev
ssmcp attach -- list
```

创建前应换成实际项目目录、自己的 GitHub 允许列表和已配置的 SSH 目标。同名 create 会拒绝覆盖。运行中的会话不能再次 start；需要替换实例时使用 restart。

| 操作 | 保证的行为 |
| --- | --- |
| create | 新建保存定义并启动工作进程；网关连通需另外观察 `connection_state` |
| restart | 等待旧执行树退出，再启动新的 generation |
| close / stop | 保存停止意图，确认工作进程停止，保留配置 |
| start | 显式启动保留的已停止会话 |
| remove | 先确认停止，再删除保存定义 |
| 停止管理服务 | 清理服务拥有的工作进程，保留原来的运行意图 |
| 启动管理服务 | 恢复期望运行的会话；已经 close 的会话保持停止 |
| 退出 attach | 分离显示和输入端，会话及其命令继续运行 |

Ctrl-] 分离 attach 并恢复本地终端模式；Ctrl-C 作用于会话中选中的 PTY。F7 切换终端，F6 聚焦可滚动日志。每个会话只有一个输入前端，其余连接可使用只读模式；只读前端不向后端发送输入或修改 PTY 尺寸。

受管会话的 `sessions logs` 提供生命周期记录，实时命令和终端输出通过 attach 查看。SSH 重连、显式 restart 或关闭会话会结束原连接所属后端及任务，随后可重新 attach 同名会话。

完整接口见[常驻会话使用说明](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/session-service.md)及[man 手册](https://github.com/LIghtJUNction/ssmcp/blob/main/packaging/man/ssmcp.1)。

## 原子能力与授权

专用管理连接通过 `--session-control` 显式获得会话管理能力：

```sh
ssmcp sessions create manager --cwd /path/to/projects \
  --session-control --watchdog --allow lightjunction@github ssmcp@ssmcp.lmm.best
```

| 原子工具 | 作用 | 主要参数 |
| --- | --- | --- |
| `ssmcp.session.list` | 列出保存的会话 | 无 |
| `ssmcp.session.status` | 查看单个会话与连接状态 | `name` |
| `ssmcp.session.create` | 新建并启动会话 | `name`、工作区内的 `cwd`，可选标题及 watchdog 配置 |
| `ssmcp.session.restart` | 替换指定实例 | `name`，可选 `expected_generation` |
| `ssmcp.session.close` | 关闭并保存停止状态 | `name`，可选 `expected_generation` |

公开 MCP 入口仍为 list 与 code_mode。旧网关可以通过原生 `call` 调用这些能力；成功结果保存在 `structuredContent`。例如先从当前公开 list 选择管理连接的唯一 ID，再单独执行需要的操作：

```javascript
const result = await call(managerClientId, "ssmcp.session.create", {
  name: "worker",
  cwd: ".",
  watchdog: true,
  reconnect_delay_secs: 30
});
return result;
```

`generation` 表示某一次实际运行的实例。管理服务串行检查状态与执行变更，因此携带旧 generation 的请求不能关闭后来启动的新实例。现网验收实际重启了测试会话，再以旧 generation 请求 close，收到明确拒绝；新实例仍保持运行，随后用正确 generation 成功关闭。

子会话继承管理连接的 SSH 目标、配置和 GitHub 允许列表；原子参数不能替换身份或注入任意启动 argv。创建目录限制在管理连接的工作区内，子会话不继承管理授权。管理连接自身的 restart/close 应通过另一条管理连接或本机 CLI 执行，自操作会在发起停止前被拒绝。

本地管理及 attach 使用私有认证 IPC。工作进程的归属、租约、锁和 generation 共同决定可操作对象，停止流程确认所属执行树退出。

## 三个平台的启动方式

| 平台 | 用户方式 | 系统方式 | 本次实际验证范围 |
| --- | --- | --- | --- |
| Linux | systemd user unit | 普通用户身份的 `ssmcp.service` | 原生 unit 校验、真实系统安装/启动、现网连接与原子调用 |
| macOS | 登录用户的 LaunchAgent | 指定普通用户的 LaunchDaemon | 原生编译、实际前台 supervisor 生命周期、服务定义 dry-run |
| Windows | 用户登录计划任务 | 指定账户的原生 SCM 服务 | 原生编译、实际前台 supervisor 生命周期、服务定义 dry-run |

各平台用户方式均使用：

```sh
ssmcp service install
ssmcp service status --json
ssmcp service restart
ssmcp service stop
```

macOS 需要开机启动时使用显式用户的系统安装：

```sh
sudo "$(command -v ssmcp)" service install --system --user "$(id -un)"
sudo launchctl print system/best.lmm.ssmcp
```

Windows 的系统方式具有真实 SCM dispatcher 和控制处理器；应在提升权限的 PowerShell 中为指定账户安装。密码通过 stdin 交给 SCM：

```powershell
$account = "$env:COMPUTERNAME\$env:USERNAME"
$credential = Get-Credential -UserName $account
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$credential.GetNetworkCredential().Password |
    ssmcp service install --system --user $credential.UserName --password-stdin
Remove-Variable credential

Get-Service ssmcp
Restart-Service ssmcp
```

系统方式与用户方式的桌面可见性不同：macOS 的 LaunchDaemon 和 Windows 的 Session 0 服务不提供用户交互桌面，桌面任务使用用户方式。Linux user manager 要在退出登录后继续运行，需显式配置 lingering。Linux/macOS 的系统安装拒绝 root 工作会话；Windows 系统安装拒绝 LocalSystem。

macOS/Windows 的管理员级安装、实际冷启动或登录触发未在本次环境现场执行；相应结果不计为已通过。Linux 已验证 enabled 状态及真实启动，没有为验收物理重启开发机。完整说明与系统文档链接见[平台服务指南](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/client-service-platforms.md)。

## 验证证据

### Linux 完整门禁与现网

| 检查 | 结果 | 证据 |
| --- | --- | --- |
| rustfmt、全目标 Clippy | 通过 | [local-gates.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/local-gates.json) |
| 库测试 | 507 通过、0 失败、7 忽略 | [local-gates.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/local-gates.json) |
| 本机生命周期与 unit 检查 | 35 项通过 | [portable-linux.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/portable-linux.json) |
| 真实 SSH / OAuth / MCP / PTY attach | 57 项通过 | [session-service.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/session-service.json) |
| 现网原子工具 | 10 项检查通过 | [live-atomic-tools.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/live-atomic-tools.json) |
| 关闭状态、清理、列表等价与最终版本 | 5 项检查通过 | [live-session-lifecycle.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/live-session-lifecycle.json) |
| 已安装会话的只读 attach | 11 项检查通过 | [manager-attach-live.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/manager-attach-live.json) |
| Release 构建与实际 systemd 安装 | 通过 | [release-build.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/release-build.json)、[linux-install.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/linux-install.json) |

### 三平台 CI

[CI 运行 44026424894](https://github.com/LIghtJUNction/ssmcp/actions/runs/44026424894) 在同一精确源码提交上完成：

| 平台 | 会话单测 | 协议握手单测 | 实际生命周期检查 |
| --- | ---: | ---: | ---: |
| Ubuntu 24.04 | 36 | 6 | 35 |
| macOS 15 | 36 | 6 | 33 |
| Windows 2025 | 33 | 6 | 34 |

三平台均通过格式检查、全目标 Clippy 和原生二进制构建。这里的各组检查存在覆盖关系，不相加为一个总测试数。原样报告及 artifact、job 链接和 SHA-256 保存在 [CI provenance](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/ci/provenance.json)。

握手测试通过真实隔离子进程交换协议帧，覆盖 v5 优先、精确拒绝后的单次 v4 协商、旧 SSH 退出后才能重连、v4 选择缓存、再次明确拒绝退出，以及禁止降级的错误场景。四项 Ready 授权字段检查分别使用独立负例。

## attach 实际画面

以下为已安装 `ssmcp-manager` 的只读 attach 真实 VT frame 渲染，具体检查见 [manager-attach-live.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/manager-attach-live.json)。捕获只发送 Ctrl-] 分离，不向后端发送命令。

[已安装管理会话的只读 attach](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/session-service-2026-11-17/manager-attach-live.png`）

画面保留了可选 Codex provider 的 api.x.com AuthRequired 诊断，以及旧协议下缺失 controller 信息的提示。它们是当时可见的真实输出；本次会话管理、连接与 attach 检查均按各自证据记录通过。

隔离的 SSH/OAuth/PTY 验收保留了 57 项原始检查记录，见 [session-service.json](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/session-service-2026-11-17/session-service.json)。其环境与现网管理会话分别记录。

## 兼容与升级边界

现网旧网关曾明确拒绝 v5。0.6.4 仅在初始结构化版本拒绝精确匹配时，完整回收 SSH 再连接一次 v4；SSH 验证、GitHub allow 与 Ready 确认保留。认证失败、EOF、超时或其他拒绝不会触发协议回退。v4 兼容连接不发送远端任务 title，也不具备逐次调用的 controller 信息；本地标题保留。

同一 runner 选定 v4 后会用它进行普通重连。若 v4 仍明确拒绝，当前进程失败退出；已启用的 supervisor watchdog 仍按配置启动新的 runner。旧网关若无法解析最初 Hello、没有返回结构化版本拒绝，不会盲目回退。reload 仍需要服务端 receipt 确认。

每个保存会话使用创建时的不可变二进制快照；restart 保留快照和配置。升级安装入口后，要升级已存在会话，应在合适时机 remove 并重新 create。本次已按该流程重建管理会话，并核对其快照与安装版本一致。

具名会话、attach/detach、只读观察及前端与后台寿命分离的交互参考 [Zellij 会话命令](https://zellij.dev/documentation/commands)。ssmcp 裸 attach 的列表示例和状态语义以[本项目接口约定](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/design/session-service.md)为准。
