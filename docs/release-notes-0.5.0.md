# ssmcp 0.5.0 升级说明

0.5.0 将客户端统一为一个 Shell 入口：同一条连接提供终端、独立命令、文件和上下文操作，并发现本机已安装的 Codex、Claude Code、Pi、OpenCode 原生工具。命令可以并行启动、留在后台、分次等待和读取输出；更新客户端后可通过 `reload` 重新连接。[统一入口与工具发现][backend] · [命令实现][shell]

## 升级时先修改启动命令

旧版的 `-t`、`--type`、`--backend` 和尾随 Agent 参数已经移除。首次迁移时，先升级 Linux 网关 `ssmcpd`，再删除旧参数并用新版客户端建立连接；账户、模型和扩展继续在各 CLI 自己的配置中设置。需要指定本机 Shell 时保留使用 `--shell`。[参数解析与迁移测试][client]

| 旧用法 | 0.5.0 的用法 |
| --- | --- |
| 用 `ssmcp -t codex ...` 或 `--type` 选择单个后端 | 运行 `ssmcp [选项] user@host`；把所需 CLI 放入客户端的 `PATH`，由客户端发现原生工具 |
| 通过 `--backend` 或尾随参数传入某个 Agent 的配置 | 使用该 CLI 的原生配置；ssmcp 不再转发这组启动参数 |
| 按 `client.type === "codex"` 选择连接 | 新客户端的 `type` 为 `shell`；按 `id`、`name` 或 `title` 选择连接，再通过公开 `list` 的 `client_id` 参数读取工具目录 |

例如，在准备共享的项目目录启动：

```sh
ssmcp --name dev --title "维护当前项目" user@host
```

`--name` 用于识别设备或工作区，`--title` 描述本次任务。选配的 CLI 不存在或其能力发现失败时，Shell 和其他成功发现的工具仍可使用。工具保留各自的原生 schema 与权限流程；完整调用方式见[后端与工具目录][backends-doc]。

## 命令：并行、后台管理与等待

`exec.run` 组合启动、等待和一次输出读取，适合短命令。多个独立检查可以在一次 `code_mode` 中并行提交：

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");
return await exec.parallel(client.id, [
  { command: ["git", "status", "--short"] },
  { command: ["git", "diff", "--stat"] }
], { concurrency: 2 });
```

`command` 是程序及参数组成的数组，不自动展开变量、管道或重定向。需要 Shell 语法时显式选择本机 Shell。并行结果按输入顺序保留，每项为 `fulfilled` 或 `rejected`；正常返回仍可能表示命令正在运行或非零退出，应继续检查 `running` 和 `exit_code`。[JavaScript helper][helpers] · [完整命令指南][commands-doc]

| 操作 | 用法与返回含义 |
| --- | --- |
| 启动长命令 | `exec.start(clientId, {command: [...]})` 立即返回 `job_id` |
| 等待命令结束 | `exec.wait(clientId, jobId, {wait_ms: 5000})` 等待结束或本次等待期限到达 |
| 查看后台工作 | `exec.list` 列出保留的任务；`exec.status` 查询状态、退出码与耗时 |
| 读取增量输出 | `exec.read` 分别使用 stdout、stderr 字节游标；`exec.events` 读取任务事件 |
| 停止与清理 | `exec.cancel` 停止受管理的进程树；`exec.forget` 只删除已结束任务的记录 |

等待期限与执行期限分开计算。`wait_ms` 默认 5000 毫秒，最长 60000 毫秒；等待到期后任务继续运行，后续沿用同一 `job_id` 再次等待。`timeout_ms` 默认 0，表示不给进程设置执行期限；外层 `code_mode` 的期限只限制本次编排。一次 `parallel` 最多接受 32 项，`concurrency` 为 1–8；它限制同时进行的启动、等待、读取流程，已进入后台的长任务仍占用客户端的 32 个运行任务名额。[任务生命周期与限制][shell]

## 更新客户端后使用 reload

将新程序安装到原启动入口后，在 `code_mode` 中调用 `return await reload(clientId)`。客户端确认请求并等待服务端回执，再停止旧连接的资源，按原参数、目录和环境重新启动。`reload` 负责重新加载已安装的程序；下载和安装由安装器完成。[重载实现][reload] · [会话控制][sessions-doc]

后台命令、本机 Agent 进程和转发属于当前连接，重载或退出时会停止，任务 ID 不会迁移。重载请求被接受后，再调用公开 `list` 获取新客户端 ID；有其他工具请求仍在执行时，先等待它们结束。有效的 `--allow` GitHub 授权可继续发现符合允许列表的新连接，登录码模式需要重新完成 OAuth 登录。首次迁移旧启动参数时应按上面的新命令启动，后续更新再使用重载。

## 六种客户端归档与安装器

发布矩阵为 Linux、macOS、Windows 各提供 x86_64 和 ARM64 客户端。Linux GNU 归档的安装基线为 glibc 2.35，包含 `ssmcp` 与 `ssmcpd`；macOS 和 Windows 归档包含客户端，网关继续部署在 Linux。客户端仍依赖系统 OpenSSH；原生 rsync 端点目前只在 Linux 注册。[发布矩阵][release-workflow] · [平台与安装说明][releases-doc]

`install.sh` 和 `install.ps1` 根据本机架构下载预编译包，核对 SHA-256 并运行版本检查，成功后再替换当前用户目录中的程序。Windows 更新会先移动旧可执行文件；若运行中的旧程序暂时不能删除，保留文件并报告路径，已完成的新版本安装仍然有效。安装入口位于本发布页上方，也可使用 Release 的固定版本下载地址。[Unix 安装器][install-sh] · [Windows 安装器][install-ps1] · [安装器验证入口][installer-tests]

## 日志与长输出

终端按命令和输出流组织事件，显示任务标识、工作目录、耗时与退出状态；没有换行的进度也能在命令结束前显示。`--log-format jsonl` 保留结构化事件，`--verbose` 展开详细内容，`--no-color` 和 `NO_COLOR` 可关闭颜色。文件操作与 diff 仍保留审计上下文。[终端渲染与回归测试][renderer]

长输出超过 32 KiB 或 500 行后保存转储文件，通过返回的 artifact ID 使用 `outputs.stat`、`outputs.read` 按需查看。进程输出的内存尾部有大小限制，完整日志应检查转储的 `complete` 和 `error`；默认清理保留 24 小时内、总量限制内的已结束输出，长期保留前应导出。[输出实现][output] · [输出范围与清理规则][sessions-doc]

## 验证与发布记录

CI 配置包含六个平台的原生检查与测试、Linux SSH／OAuth 集成测试，以及可下载的终端渲染样本；发布矩阵另对生成的原生归档执行安装与更新验证。Release 在全部检查和源码包验证成功后才创建 tag，再分别发布 GitHub 资产与 crates.io 包。具体版本的结果以对应 [Actions 运行][actions]、[Release 附件][releases] 和 [crates.io 版本][crates] 为准。[CI 配置][ci] · [原生文件与进程测试][native-tests] · [发布校验脚本][release-script]

[backend]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/backend/mod.rs
[shell]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/backend/shell.rs
[client]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/client.rs
[backends-doc]: https://github.com/LIghtJUNction/ssmcp/blob/main/docs/backends.md
[helpers]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/code_mode.js
[commands-doc]: https://github.com/LIghtJUNction/ssmcp/blob/main/docs/commands.md
[reload]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/client/reload.rs
[sessions-doc]: https://github.com/LIghtJUNction/ssmcp/blob/main/docs/session-control.md
[release-workflow]: https://github.com/LIghtJUNction/ssmcp/blob/main/.github/workflows/release.yml
[releases-doc]: https://github.com/LIghtJUNction/ssmcp/blob/main/docs/releases.md
[install-sh]: https://github.com/LIghtJUNction/ssmcp/blob/main/scripts/install.sh
[install-ps1]: https://github.com/LIghtJUNction/ssmcp/blob/main/scripts/install.ps1
[installer-tests]: https://github.com/LIghtJUNction/ssmcp/blob/main/scripts/test_installers.py
[renderer]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/client/render.rs
[output]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/output.rs
[ci]: https://github.com/LIghtJUNction/ssmcp/blob/main/.github/workflows/ci.yml
[native-tests]: https://github.com/LIghtJUNction/ssmcp/blob/main/tests/release_client.rs
[release-script]: https://github.com/LIghtJUNction/ssmcp/blob/main/scripts/release.py
[actions]: https://github.com/LIghtJUNction/ssmcp/actions/workflows/release.yml
[releases]: https://github.com/LIghtJUNction/ssmcp/releases
[crates]: https://crates.io/crates/ssmcp
