# ssmcp 0.5.1 交付与验证记录

日期：2026-11-16。开发分支：`main`。发布源码提交：[`c75309a9acab27c684c0beaaf9d22fd991200760`](https://github.com/LIghtJUNction/ssmcp/commit/c75309a9acab27c684c0beaaf9d22fd991200760)，版本 0.5.1。下文的正式发布检查均对应此不可变提交。

## 交付状态

**0.5.1 已公开发布，安装验收完成。** [GitHub Release](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.5.1) 于 2026-11-16 09:33:57 UTC 公开，包含 16 个已上传附件：六份预编译归档、六份独立校验文件、两套安装器、`SHA256SUMS` 和 `release-manifest.json`。[crates.io 0.5.1](https://crates.io/crates/ssmcp/0.5.1) 也已发布，CI 已核对远端包摘要。

[正式发布运行 43838379446](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446) 第 2 次运行最终为 `completed / success`。六平台原生检查、六份归档构建与安装验收、Cargo 源码包验证和两个发布渠道全部成功。公开标签仍指向上述 `c75309a`，没有移动标签或替换已发布 crate。

开发机另从公开 `latest` 地址下载了安装器，核对其 SHA-256 和公开 manifest 的源码提交后，完成首次安装及固定版本的重复更新；两次均退出 0，并实际运行 `--help` 和 `--version`，版本均为 `ssmcp 0.5.1`。两次安装得到的 Linux x86_64 客户端字节相同，SHA-256 为 `736c2902e112acb0a81bde4f62a7829a903238c9fb36fd020bab79ae39c953a5`。

Linux / macOS 一键安装：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh
```

Windows PowerShell 一键安装：

```powershell
irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex
```

安装器安装当前用户的客户端。Linux 归档另附网关 `ssmcpd`，服务安装及升级见[部署说明](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/docs/deployment.md)。固定版本下载与完整校验记录见 [SHA256SUMS](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/SHA256SUMS) 和 [release-manifest.json](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/release-manifest.json)。

## 功能变化及使用方式

### 独立命令与并行执行

每个命令有自己的进程、stdout、stderr 和 `job_id`，多个任务不再争用同一个交互终端。命令入口使用参数数组，不隐式展开 Shell 语法。需要管道时可以明确调用本机 Shell，例如 `['sh', '-c', 'producer | consumer']`。实现及边界见[命令管理器](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/backend/shell.rs)。

新版网关提供 `exec.run` 和 `exec.parallel`，常见检查可直接写为：

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");

return await exec.parallel(client.id, [
  { command: ["git", "status", "--short"] },
  { command: ["git", "diff", "--stat"] }
], { concurrency: 2 });
```

结果按输入顺序返回，每项为 `fulfilled` 或 `rejected`。调用成功仍需检查实际命令的 `running` 和 `exit_code`。一次最多提交 32 项，编排并发数为 1–8；长任务进入后台后仍占用客户端运行名额。客户端最多同时运行 32 个任务，最多保留 128 条任务记录；只淘汰已结束的旧记录，启动失败不会提前删除已有历史。[JavaScript helper](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/code_mode.js)

### 后台管理与等待

| 操作 | 作用 |
| --- | --- |
| `exec.start` | 启动命令，立即取得 `job_id` |
| `exec.list`、`exec.status` | 查看任务、退出码、耗时和完成状态 |
| `exec.wait` | 等待任务结束，或在本次等待期限到达后返回 |
| `exec.read` | 按 stdout、stderr 各自的字节游标读取增量输出 |
| `exec.events` | 读取启动、输出和结束事件 |
| `exec.cancel` | 停止受管理的进程及可归属的子进程，可重复调用 |
| `exec.forget` | 删除已经结束的任务记录 |

执行期限和等待期限是两件事。`timeout_ms: 0` 表示不给命令设置执行期限；`exec.run` 和 `exec.wait` 的 `wait_ms` 默认 5000 毫秒，`exec.read` 默认 0，最长均为 60000 毫秒，只决定当前调用等多久。等待到期后任务继续运行，下一次沿用同一个 `job_id`。外层 `code_mode` 超时也不会把已启动的命令自动重跑或自动取消。[命令指南](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/docs/commands.md)

```javascript
const started = await exec.start(clientId, {
  command: ["cargo", "build", "--release"],
  timeout_ms: 900000
});
return await exec.wait(clientId, started.job_id, { wait_ms: 5000 });
```

需要完整日志时使用返回的输出文件信息。超过 32 KiB 或 500 行的输出转入文件；单次读取和内存尾部均有界，输出是否完整由 `complete`、`error` 等字段明确表达。文件默认保留 24 小时，并受总量上限约束。[输出存储实现](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/output.rs)

### 重载与迁移

安装更新后的可执行文件后，可在新版网关的 `code_mode` 中调用：

```javascript
return await reload(clientId);
```

重载先完成请求确认，再关闭旧连接所属资源，并按原参数、目录和环境重新启动。重连产生新的客户端 ID，因此后续应重新调用公开 `list`。重载会停止旧连接的受管理后台命令和原生工具进程；任务 ID 不迁移。Unix 清理覆盖同一会话和仍可追溯的子树，已经脱离该归属关系的守护进程需要自身的停止机制。登录码模式的旧 OAuth 令牌失效，需要重新授权。重载不会主动删除输出文件；尚未过期且未被总量配额清理的文件，仍可按原 artifact ID 读取。[重载实现](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/client/reload.rs) · [真实 SSH 回归](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/scripts/e2e_session_features.py)

0.5.0 将客户端统一为 Shell 入口，并发现本机已安装的 Codex、Claude Code、Pi、OpenCode。旧的 `-t`、`--type`、`--backend` 及尾随 Agent 参数已经移除。首次迁移应先升级 Linux 网关，再移除旧参数启动客户端；以后才可沿用原启动参数直接重载。原生 CLI 的账户、模型和扩展继续使用各 CLI 自己的配置。[迁移说明](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/docs/release-notes-0.5.0.md)

### 客户端显示

终端显示任务 ID、命令、工作目录、耗时及退出状态，stdout 与 stderr 按任务标识分组，文件修改提供受限的差异预览；长输出保存位置可直接定位。支持 `--log-format jsonl`、`--verbose`、`--no-color` 和 `NO_COLOR`。结构化输出与终端美化分开，自动化程序可继续读取事件字段。[渲染实现与样本测试](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/client/render.rs)

[终端事件排版样本](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/terminal-logs.png`）

图片是固定事件输入的排版样本，用于检查颜色、层次和换行，不代表生产运行耗时。当前提交的真实渲染输出另保存在 CI 的 `terminal-preview` artifact 中。

## 自动发现并修复的问题

| 问题 | 观察到的影响 | 修复与验证 |
| --- | --- | --- |
| 动态工具目录在两页之间变化 | 第二页报目录已变更，工具无法完整列出 | 每次扫描保存固定快照，续页不重新发现工具；新扫描仍看见更新，交错扫描 E2E 通过 |
| 可选原生工具的目录读取挂起 | 阻碍客户端注册和 Shell 使用 | 每个原生 CLI 的完整工具目录读取各有 5 秒预算；单个目录超时后保留 Shell 和其他可用工具，启动回归通过 |
| Windows ConPTY 只输出 `ESC[6n` | 命令一直等待，未实际执行 | 读线程识别分片光标查询、经输入队列回复后开放命令；两个 Windows 架构真实执行及 resize 通过 |
| macOS 交互 Shell 退出后仍有后台任务 | 输出无法到 EOF，子进程残留 | 保留进程身份到清理完成，覆盖同一会话中的独立进程组；后台任务停止与 EOF 回归在两种 macOS 架构通过 |
| 父进程关闭账户存储后锁仍短暂保留 | ARM 并行测试立即重开存储失败 | 存储对象显式释放锁，验证重复描述符仍存活时也能正确重开 |
| 任务启动失败、输出捕获异常 | 历史可能提前丢失，输出完整性可能不清楚 | 启动成功后才淘汰旧历史；捕获失败明确结束并标记不完整，相关回归通过 |
| 多平台实现合并后的编译与静态检查问题 | 某些平台无法产生可用客户端 | 在各原生 runner 编译和执行测试，并修复实际报告的问题后重新验证 |
| GitHub 草稿创建后查询 404 | 归档已验证，但发布在上传前停止 | 查找包含草稿的 Release 集合，后续固定使用 Release ID；复用原草稿完成上传，公开后原 CI 作业幂等重试成功 |

工具目录快照每个连接最多四份，序列化 JSON 合计最多 64 MiB，有效期 120 秒；过期或被淘汰的游标明确要求重新开始扫描。[Runtime 快照](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/runtime.rs) · [原生目录发现](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/backend/mod.rs)

ConPTY 的握手要求与 [Microsoft 的 CreatePseudoConsole 文档](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole)一致；文件锁的继承及显式解锁行为依据 [Linux flock 手册](https://man7.org/linux/man-pages/man2/flock.2.html)。具体实现见[便携终端](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/pty_portable.rs)、[进程生命周期](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/backend/process_portable.rs)和[账户存储](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/src/portal/store.rs)。

## 验证证据

### 六个平台原生 CI

| 平台 | 库测试通过数 | 原生命令与文件集成测试 | 完整 CI 记录 |
| --- | ---: | ---: | --- |
| Linux x86_64 | 199 | 4 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446/job/133200781319) |
| Linux ARM64 | 199 | 4 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446/job/133200780900) |
| macOS x86_64 | 107 | 4 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446/job/133200781112) |
| macOS ARM64 | 107 | 4 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446/job/133200780981) |
| Windows x86_64 | 101 | 4 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446/job/133200781063) |
| Windows ARM64 | 101 | 4 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446/job/133200781086) |

这些数字是各自运行中的通过数，不把同一测试在两种架构上执行算作新增测试。完整 CI 还包含 Clippy、其他集成测试、构建、命令行帮助与版本检查。需要额外安装真实 OpenCode v2 并提供隔离配置的可选测试没有执行；原生工具发现使用协议 fixture，没有调用外部模型。

### Linux 端到端流程

以下检查在 Linux x86_64 和 ARM64 CI 中均成功，也在开发机对相应功能进行了实际验证：

| 检查 | 结果 | 覆盖内容 |
| --- | ---: | --- |
| DCR 持久化与真实守护进程重启 | 8 项通过 | 注册信息跨重启保留 |
| 基础 SSH / OAuth / HTTP / PTY | 143 项通过 | 认证、协议、终端和公开工具调用 |
| 会话、重载与输出文件 | 66 项通过 | 重载确认、令牌失效、重连、完整输出与按页读取 |
| 工具发现组合 | 301 项通过 | Shell 与四种原生工具、合并目录及动态分页，共六种场景 |
| 系统 Node.js / bubblewrap | 11 项通过 | 实际隔离运行时 |
| 公开双工具接口 | 7 项通过 | `list` 与 `code_mode` 的真实运行路径 |
| 客户端启动集成 | 9 项通过 | 启动期限、能力发现失败和挂起时的行为 |

CI 还在安装后的 systemd 服务限制下运行了隔离测试，验证了工作区桥接和上下文 helper。发布脚本的 29 项单元测试、ShellCheck 与 workflow 的 actionlint 也已通过。完整范围以[工作流](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/.github/workflows/ci.yml)和上述运行日志为准。

### 发布恢复与后续自动发布

实际首次发布发现草稿读取缺陷后，修复已增量提交至 `main` 的 [`a89852c`](https://github.com/LIghtJUNction/ssmcp/commit/a89852ceff89f55645effc5a1a2a735aed9e25fc)，随后并入 [`95f2496`](https://github.com/LIghtJUNction/ssmcp/commit/95f24966fdd7dfe16bbb79f8dfce93f0b9a36d52) 的 Release ID 身份核对与重试保护。在 `95f2496` 上实际运行的 **40 项发布测试全部通过**，覆盖草稿分页查找、首次创建、部分上传续传、已公开版本重复执行、HTTP 错误区分以及同标签多草稿歧义。

恢复使用正式 CI 已经通过验收的原始归档，复用草稿 `446823489`，对每个附件核对大小和 SHA-256 后公开。随后重跑原发布作业，`publish_release` 与 `publish_crates` 均成功；这验证了已公开资产的重复运行不会重新创建草稿或覆盖不同内容。运行时二进制和 crate 仍对应 `c75309a`，后续发布脚本修复属于 `main` 的增量改进。

## 发布机制

日常开发保持在 `main`。推送与 `Cargo.toml`、`Cargo.lock` 一致的 `vX.Y.Z` 标签会自动触发 Release；该提交必须已经进入 `main`。手动 `Bump version` 工作流提供版本同步、提交、创建标签和启动发布入口。普通 `main` push 与 PR 不触发独立 CI；手动 Release 留空标签只做完整预检，不公开版本。

发布固定标签对应的提交，先运行六平台检查、优化构建和原生安装验收，再核对归档及 Cargo 源码包。GitHub Release 先以草稿上传并核对附件，再公开；crates.io 发布独立运行。已经存在的标签和附件不会被强制移动或覆盖。[发布工作流](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/.github/workflows/release.yml) · [修复后的发布校验脚本](https://github.com/LIghtJUNction/ssmcp/blob/95f24966fdd7dfe16bbb79f8dfce93f0b9a36d52/scripts/release.py)

发布矩阵包含 Linux、macOS、Windows 各两种架构。Linux GNU 归档在 Ubuntu 22.04 构建并执行，安装基线为 glibc 2.35，包含客户端和 Linux 网关；其他平台归档包含客户端。客户端使用系统 OpenSSH。Linux 原生 rsync、网关服务的具体安装步骤见[平台与发布文档](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/docs/releases.md)。

安装器先核对 SHA-256 并执行版本检查，验证成功后再替换已有程序；测试覆盖首次安装、更新、损坏下载、错误版本与失败回滚。Windows 在更新运行中的程序时先改名保留旧文件；若系统暂不允许删除旧文件，会明确显示保留路径。[安装器验收入口](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/scripts/test_installers.py)

## 运行范围

本轮完成项目代码、CI、预编译发布链路与安装器的验证。现有共享线上网关没有被重启或替换；新 `exec.*` 编排 helper 和 `reload` 需要网关与客户端配套升级。使用新客户端原生 `ssmcp.exec.*` 工具的启动、并行、等待、读取、取消与清理，已通过当前连接在开发机实际执行。首次升级步骤详见[0.5.0 迁移说明](https://github.com/LIghtJUNction/ssmcp/blob/c75309a9acab27c684c0beaaf9d22fd991200760/docs/release-notes-0.5.0.md)。
