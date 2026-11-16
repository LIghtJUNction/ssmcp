# ssmcp 0.6.0 验收记录

记录日期：2026-11-16。目标版本：0.6.0。本文按证据来源区分实现、协议测试、实机操作、发布与部署，不把其中一项的成功代替其他项。

**当前是交付过程中的证据快照，尚未签署统一 Harness 架构全部完成。** 历史合并基础为 `bdb0a8d`。本轮 Rust 结果绑定远程头 `7d3bd590c38f972e83b56d98dfe6015bac352c1c` 加工作区 Diff 摘要，而不是已发布提交；之后的 Linux 终端检查另以运行二进制摘要记录；最终提交绑定、CI、安装与部署仍需分别完成。0.5.1 的已发布结论保留在[历史记录](validation-0.5.1.md)，不能代替本轮验证。

[最终门禁与交付记录](reports/final-0.6.0-gates.json) 保存每项检查的状态、命令、源码身份、结果与证据位置。`passed` 只适用于该项记录的源码快照；`pending` 表示尚未形成相应证据，不等于测试失败；`partial` 表示该组合场景已有实测，但表中仍列有未覆盖的部分。未执行项目的次数、退出码和耗时保持空值，不预填 0。

2026-11-16 的 GitHub 插件查询确认 [PR #7](https://github.com/LIghtJUNction/ssmcp/pull/7) 为 open、`draft:false`、`mergeable:true`，头提交为上述 `7d3bd590…`。合并冲突状态不表示测试或发布条件已经满足；精确观察字段见机器可读记录。

## 已核实证据

[机器可读摘要](reports/unified-0.6.0-evidence.json) 保存本节日志摘要、SHA-256 和完整原生工具描述。原始日志位于开发机：

```text
/home/lightjunction/Documents/GITHUB/ssmcp/target/ssmcp-unified-task/
```

| 项目 | 实际结果 | 范围与边界 |
| --- | --- | --- |
| 恢复合并后的 `cargo check --all-targets` | 成功；`resume-check.log` | 编译检查，不代表所有运行测试或后续修改通过 |
| `release_client` | 4 passed、0 failed；`resume-release-client.log` | 原生 argv/env/cwd、独立 stdout/stderr、退出码、取消、文件/技能/规则契约 |
| 原生适配器与进程定向测试 | 汇总中 fmt/check/process/shell/opencode/rpc/pi/program 均 exit 0；`resume-native-results.json` | 其中进程 7 passed、RPC 7 passed、OpenCode 14 passed；真实 OpenCode 端点测试未运行，不能记为实机接入 |
| 发布与部署脚本 | 开发机完整测试 46 passed（协调执行任务 6）；新增部署测试在 Cloud 另有 6 passed | stage/apply 逻辑使用临时目录与 mock 系统服务，覆盖版本闸与回滚；未修改任何服务器，完整开发机日志待归档 |
| Codex 真实 MCP 工具 | 历史实机 1 passed、0 failed；`codex-real-mcp-smoke-4.log` | 同一次测试实际发现工具 Schema，并通过原生调用接口得到非错误结果；最终提交需复验 |
| 部署前服务器清单 | Arch Linux x86_64，`ssmcpd 0.4.1` | `resume-deploy-inventory.json`；这是升级前状态，不是 0.6.0 部署成功证据 |

### 本轮 Rust 门禁

本轮任务 `4f753a35d9e2f723431a9a34fb8d945f-e` 的最终退出码为 0，耗时 69.368 秒；已直接核对 `target/acceptance/rust-gates.json` 和同一连接的完整 stdout。8 个步骤均通过：

| 步骤 | 结果 | 耗时 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | exit 0 | 0.465 秒 |
| 全目标 clippy，`-D warnings` | exit 0 | 7.729 秒 |
| 全目标 Rust 测试 | 376 passed、0 failed、18 ignored；16 个 suite 输出 | 54.441 秒 |
| Node 与隔离执行器，包含 ignored | 11 passed、0 failed | 2.520 秒 |
| 两个公开 MCP 工具，包含 ignored | 7 passed、0 failed | 1.117 秒 |
| Workspace bridge 的 ignored 检查 | 3 passed、0 failed | 2.570 秒 |
| Context helpers 的 ignored 检查 | 2 passed、0 failed | 0.315 秒 |
| 客户端与 Broker 构建 | exit 0 | 0.115 秒 |

全目标结果中，库测试为 294 passed、0 failed、4 ignored，其余 suite 合计 82 passed、0 failed、14 ignored。后续定向检查与这次运行有重叠，因此不把各次 passed 相加为唯一测试数量。未执行项的具体原因和最终覆盖范围仍需在最终记录中逐项保留。

该运行的源码身份为 base `7d3bd590c38f972e83b56d98dfe6015bac352c1c`，工作区 Diff SHA-256 为 `a64be6692edb6e11e6ddf81493e76796de17bd546bbb94e59f5177e95dc939da`。报告文件 SHA-256 为 `4b7ec9ae89fdced0b8af0143c979d1c2ec72e682a341fcddb0f31fe264344551`。这条身份记录不证明后续改动或 GitHub CI 已通过。

原始输出已在同一连接内归档，并校验字节数与摘要，位置均相对隔离工作区：

| 文件 | 字节数 | SHA-256 |
| --- | ---: | --- |
| `target/acceptance/rust-gates-4f753a35-e-stdout.log` | 3,906 | `e8611370e1301d91c20df1cb927717967e09fd568fa5bb89db6eceb2346fe3d7` |
| `target/acceptance/rust-gates-4f753a35-e-stderr.log` | 315,107 | `68eb23bd5bda3292a2c4d4789638b66d791a0b962401a192e56cb573b760090c` |

stderr 对应完整 artifact `68aa1acc8b0d21e2c68bb9fe2d764122`，共 3,997 行，`complete:true`。归档文件权限为 0600；私钥头、Bearer、GitHub/AWS 凭据形状和敏感赋值的初步模式检查均为 0 个匹配，这不证明任意秘密都已排除。公开仓库与 Drive 只保存脱敏摘要、校验值和必要证据；原始日志留在受控目录。

日志焦点的同批次切换与极矮窗口恢复已追加修复；下述 Linux 真实 PTY 检查通过了这两个操作场景。独立 Rust 定向结果仍应按对应日志记录，旧界面的端到端检查数量不能沿用为新面板的实机验收。

部署版本测试具体覆盖：必填稳定版本号；拒绝错误或重复的包身份字段；暂存二进制版本不符时不启动服务；成功 stage 绑定版本、提交和包摘要；拒绝旧版本、不同提交、不同包或过期 receipt；安装后版本不符时恢复旧包与 checkpoint。它们在 [`scripts/test_release.py`](../scripts/test_release.py) 中，随现有发布脚本检查运行。

复核日志内容时需先区分原生错误和桥接错误。旧 Codex 日志中部分用户配置的 MCP Server 缺少凭据、登录或可执行入口，原生层明确报告 unavailable；成功调用另一服务器不应隐藏这些诊断，也不证明所有已配置服务器可用。

### 本轮在线原生目录与只读调用

2026-11-16 13:55:06.998 UTC 的[在线验证摘要](reports/native-validation-20261116-summary.json)记录了运行中的 0.6.0 客户端。运行二进制 SHA-256 为 `f4f64248fa447e1cb40e8e76636a6467a0a24d4d28cae5422e999d80ad6b8e4d`；它包含第一批 UI 与目录缓存修复，尚不包含右侧面板。以下结果适用于这个运行镜像，最终发布镜像仍需绑定对应证据。

| 检查 | 实际结果 |
| --- | --- |
| 完整目录分页 | 23 页、1,416 个互不重复的路由名称，其中 Codex MCP 工具 1,154 个；同一快照完整读取 |
| 原生 Schema 保真 | 发现的 `codex.mcp.linux-computer-use.doctor` 描述与原生描述比较一致，仅路由名称不同；完整样本含 `inputSchema`、`outputSchema` 与 `$defs` |
| 原生 MCP 状态长输出 | 409,879 字节，13 页完整读取，SHA-256 核对一致，`complete:true` |
| 原生工具真实调用 | 只读 `doctor` 调用 2.542 秒；返回 `isError:false`，保留 `content` 和对象类型的 `structuredContent` |
| 审批与模型请求 | 该只读工具无需审批；调用前后未决审批均为 0，未发送审批回复，未触发模型请求 |

原生状态输出的 SHA-256 为 `2f44a840e928ef50c955166a29cd2241ec1dd73bb6b892f6fec1edd653c4e8e2`。这次样本不覆盖需要审批的真实工具，也不证明截图或输入权限已可用。[完整工具描述](reports/native-tool-sample-20261116.json)只包含原生接口说明；[证据完整性清单](reports/native-validation-20261116-integrity.json)列出私有记录的字节数与摘要，不包含原始返回值。

同次运行中，Codex `0.169.0` 与 Claude Code `2.1.325` 经发现后分别报告 ready；Pi `1.5.2` 与 OpenCode `2.3.0` 已安装但未配置安全的既有原生入口，报告 unavailable。这是实际支持范围，不能改写成四类均成功接入。完整目录、用户环境具体值和原始服务结果未上传公开仓库。

### 真实工具枚举与调用样本

历史实机日志发现的工具名称为 `codex.mcp.linux-computer-use.doctor`。它是此次发现的样本，不能硬编码进自动路由。其原生参数 Schema 为：

```json
{"properties":{},"type":"object"}
```

工具描述同时包含完整 `outputSchema` 与 `$defs`；[证据摘要](reports/unified-0.6.0-evidence.json) 原样保留这个描述。日志随后出现 `mcpServer/tool/call` 请求和成功响应，返回保留 `content`、`structuredContent`、`isError:false`。只读诊断明确标记截图捕获尚未验证，这个结果不证明截图授权或完整桌面控制可用。

公开摘要只保留明确标注的响应摘录，不提交运行时环境和私人路径的完整内容。原始结果保存在受控日志内；摘要中的 SHA-256 可用于核对其来源。该调用没有覆盖原生交互审批分支；审批传播仍需用相应测试及实际可观察的权限路径验证。

## 十二项验收账本

以下“待最终签署”表示需要把证据绑定最终提交并完成该项的实际运行范围，不表示已经观察到失败。提供者被安全地标记 unavailable 是正确诊断，但不能改写成成功接入。

| 验收项 | 已有证据或实现 | 最终需补的证据 |
| --- | --- | --- |
| 不安装 Harness 仍可连接并执行 Shell | Shell 为独立基础能力；启动与 SSH fixture | 最终二进制在无 Harness 环境的 SSH、命令和退出记录 |
| 仅注册真实安装且实际可用的工具 | 二进制、版本、握手、目录检查；不可用诊断 | 实机目录与安装清单对照，确认没有过期路由 |
| 四类 Harness 独立发现、单故障隔离 | 并行发现；Pi/OpenCode 只连安全既有入口 | 每类真实接入结果或具体 unavailable 原因，及单提供者失效后的其他调用 |
| `list` / `code_mode` 目录一致且 Schema 保真 | 同一注册表、分页快照与命名空间 | 同一在线连接的两条目录路径与原生 Schema 样本 |
| Codex MCP 内部工具动态枚举并真实调用 | 历史实机目录加成功调用 | 最终提交复验；目录增删、断连与审批传播的对应记录 |
| 至少两项 Shell 命令独立并发 | 独立进程、分离输出与任务 ID | 同时存活的任务状态、交错事件、各自最终退出码 |
| 后台 stdout/stderr 实时且不重复 | 实时增量事件；read/wait 不重复展示 | wait 前已经出现输出的终端记录及无重复检查 |
| 文件、Diff、技能审计准确 | 成功提交点事件，三态快照；发现与读取分离 | 真实读写 Diff、失败写入无伪成功、技能发现/读取终端样本 |
| 超时、取消、断线、reload 清理 | 连接拥有任务的生命周期与注册表失效 | 进程后代清理、旧 ID/目录不可路由、重连后的资源检查 |
| 长输出完整保存、分页、SHA、过期 | 共用 OutputStore 与原有阈值/配额 | 各输出路径的完整分页和摘要核对、自动清理及不完整错误样本 |
| 原生审批与客户端授权不被绕过 | 原生请求/响应转发、连接访问和输出归属检查 | SSH/OAuth/客户端隔离、拒绝/待审批状态及跨连接读取拒绝 |
| Rust、SSH/OAuth/PTY E2E 与 CI 全通过 | 已有部分定向测试与编译记录 | 最终提交全目标测试、clippy、原生平台 CI、端到端运行 URL/日志 |

Pi 和 OpenCode 的原生接口范围见[发现说明](backends.md)。当前没有证据证明用户的真实 Pi socket 或 OpenCode 服务已经接入；不能用 fixture 的成功代替这两项实机结论。发现阶段不得安装依赖、触发模型或通过禁用审批扩展来制造可用状态。

## 发布、安装与部署记录要求

发布记录需包含最终源码 SHA、PR、CI 运行 URL、固定 `v0.6.0` 标签和公开制品摘要。GitHub Release 与 crates.io 是独立渠道，分别记录成功或失败；标签、仓库版本号或草稿 Release 不能代表附件已可下载。[发布流程](releases.md)

客户端安装需记录下载制品、SHA-256、实际入口与 `ssmcp --version`。正在运行的旧客户端不会因磁盘文件替换自动更新；reload 先确认接收，然后必须观察新连接、重新发现目录并核验实际版本。新连接不继承旧连接的输出读取权。[重载与输出](session-control.md)

部署前实机状态已读取：SSH 目标 `archczy`，服务单元位于 `/usr/lib/systemd/system`，执行入口为 `/usr/bin/ssmcpd`，已安装 `ssmcp-git 0.4.1.r70.g655342c-1`；门户为独立服务。0.6.0 候选 Arch 包只可从通过 CI 的固定提交构建，保留包摘要与可恢复旧包。[部署助手](../scripts/deploy_server.py)

stage/apply 使用同一 `--expected-version 0.6.0`、提交和候选包摘要。最终材料应保留两个 JSON 报告，包含 staged/installed/running 版本、生产租约、DCR 重启恢复、门户、本地及公网健康、未授权拒绝、回滚 checkpoint 和最终二进制摘要。实际 apply 由独立 SSH/systemd 作业持有；升级期间管理用 MCP 连接中断不能使回滚驱动一起消失。[部署命令](deployment.md#日志和受控升级)

## 最终交付门禁

以下表格用于补齐最终提交后的结果，完整字段在 [JSON 记录](reports/final-0.6.0-gates.json) 中。每项结果都需要命令或操作、源码身份、环境、退出状态和可核对的日志或 URL。

| 项目 | 当前记录 | 仍需形成的交付证据 |
| --- | --- | --- |
| Rust 格式、clippy、全目标测试、构建 | 已通过上述工作区快照 | 最终提交绑定；键盘边界修复的定向复验 |
| 发布与部署脚本 | 既有开发机 46 项通过 | 与最终提交绑定的日志 |
| SSH / OAuth / PTY 与统一执行审计 | 有前序协议与端到端结果 | 最终版本的对应日志，不能替代新前端的实机交互 |
| Codex 真实目录与调用 | 有历史真实样本 | 最终二进制同一次发现、Schema 与调用结果 |
| Pi / OpenCode 真实外部服务 | pending | 实际端点接入，或保留具体不可用原因与支持范围 |
| 新面板真实终端交互 | Linux 73 项断言通过，14.173 秒 | 已绑定运行二进制；下表明确区分完整场景与部分覆盖，其他平台仍待实际记录 |
| 六平台 CI | pending | 最终源码 SHA、workflow URL 与各平台 job 结果 |
| GitHub Release / crates.io | pending | 分别记录公开版本、制品清单与摘要 |
| 客户端安装与 reload | pending | 安装入口、版本、二进制摘要及新连接 |
| 服务端 stage/apply 与线上复验 | pending | 同一版本、提交和包摘要；运行版本、授权、健康及恢复记录 |
| Google Drive 同步 | pending | 本轮摘要文件清单、具体目录与报告 URL |

### 真实终端交互验收

[Linux 实机报告](reports/dashboard-20261116-report.json)来自开发机上的受控前台 PTY，使用独立 OpenSSH 公钥认证、OAuth 和 VT 屏幕解码器检查实际进程与终端行为。73 项断言全部通过，耗时 14.173 秒；其中包含不同前端模式重复运行的登录检查，不能称为 73 个互不重复的 UI 场景。本次没有调用已安装 Harness 的模型。

运行客户端 SHA-256 为 `6d114b685c644a510c674118d612156ebd5e2ee9f7b53717758a2272bc0f9821`，Broker 为 `e81a099e690c06e57ef21c8dd85af88a020081af167fcd9a38a0ac93fb2137ae`；报告 SHA-256 为 `e589ecb15ece149a64f28cfd1f401373a9f8864e3ed5b2214c2ce18fbbf8e8ab`。记录覆盖 106、80、160 列与极矮窗口，原始显示捕获为 85,432 字节，完整摘要保存在报告中。原始 PTY 内容未随公共报告上传。

| 编号 | 场景与判定 | 状态 |
| --- | --- | --- |
| U01 | 106×58 下左区 77 列、右区 28 列，日志、Shell 与侧栏互不覆盖 | Linux 通过 |
| U02 | 106→80→160→106 列缩放和实际子 PTY 尺寸已验证；80 列下单独的 F6 操作尚未列入报告 | 部分覆盖 |
| U03 | 日志焦点缩至 4 行后返回 Shell 焦点和可见光标；恢复高度后尺寸正确 | Linux 通过 |
| U04 | F6、PgUp、Home/End、Esc 与真实历史输出已验证；PgDn 尚待对应记录 | 部分覆盖 |
| U05 | 同一批次 F6→PgUp→普通字符，输入进入 PTY，日志回到最新输出 | Linux 通过 |
| U06 | 宽字符、组合字符、普通输入和后台输出隔离已验证；持续输出时的退格与括号粘贴未单列 | 部分覆盖 |
| U07 | 两个独立任务及超过 6 秒的静默计时已验证，无伪百分比；真实 total 和跨来源同号的实测未纳入本报告 | 部分覆盖 |
| U08 | stdout/stderr 在执行中各显示一次，read/wait 不重放，也不覆盖手工 Shell | Linux 通过 |
| U09 | 成功的实际文件写入显示审计和 Diff；技能状态、失败写入及完整 Diff 查询的证据另列 | 部分覆盖 |
| U10 | 真实仓库观察和修改后的状态刷新已验证；冲突、过期与不可用状态未在本次注入 | 部分覆盖 |
| U11 | NO_COLOR、关闭面板、JSONL、非 TTY 与内层 clear/alternate screen 隔离已验证；敏感字段注入的证据另列 | 部分覆盖 |
| U12 | Ctrl-C 正常退出恢复屏幕、光标、输入模式和原始 termios；受控失败与任务清理另行记录 | 部分覆盖 |

Windows 与 macOS 的真实交互结果必须单独记录，构建成功不能代替键盘、缩放和退出恢复。渲染 fixture 可以证明确定的输出排版，但不能替代真实终端输入。本次 UI 审阅发现的两个焦点边界已经修复，并分别通过了 U03、U05 对应的真实操作检查。上表的部分覆盖不改变已通过断言的结论，也不把未覆盖部分记为通过。

### PR 讨论与自动审阅状态

只读查询返回 6 条 PR 时间线消息，没有返回带文件行号的修改建议。Sourcery 因改动超过其 150,000 字符限制未完成审阅；Codex Security Review 在 `31c3323` 上报告 failed，配套消息说明付费账户已达到用量限制。自动审阅因此没有形成完成结论，也不能被记为安全检查通过。摘要中的 unresolved 标记没有附带可核查的具体 finding；保留原状态和原因，未发送评论、重试审阅或触发模型。[审阅摘要](https://github.com/LIghtJUNction/ssmcp/pull/7#issuecomment-6912712325) · [额度说明](https://github.com/LIghtJUNction/ssmcp/pull/7#issuecomment-6916701956) · [大小限制](https://github.com/LIghtJUNction/ssmcp/pull/7#issuecomment-6916700732)

## 材料归档

Git 保存实现、项目技能、测试、文档和上述脱敏摘要；原始运行日志、候选包与回滚材料按权限保存，不把 OAuth 凭据、私钥、环境变量或敏感文件正文提交到公开仓库。Google Drive 同步应记录具体文件夹路径、报告 URL、上传文件清单与校验值；上传失败时如实保留未同步状态。

已有 [Google Drive 验收目录](https://drive.google.com/drive/folders/1_ks6zQwTGMTcm3X3vkmfp1wqXwiZxWZ7)。本轮新增摘要的上传结果、具体目录、文件清单与报告 URL 仍为 pending；最终公开 Release 和部署后版本也尚待填入实际证据。
