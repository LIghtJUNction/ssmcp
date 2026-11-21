# SSMCP 当前迭代：统一源码已固定，全子命令验证准备中

**更新于 2026-11-21 19:32:48 UTC。** 候选已固定为 [fc650a9](https://github.com/LIghtJUNction/ssmcp/commit/fc650a91bdab60dea96ee7e18c15bec3af9f9b32)，包含 `resume` 修复、正式 CI 夹具修正，以及正常合入的 TLS、JSON 错误回执和最新依赖改动。与最近实际读取的主线 `834522e` 相比，候选向前十三个提交、涉及十四个文件。**V12 完成源码与依赖同步后，因共享构建资源等待超时退出；编译和产品测试均未启动，v0.10.26 尚未发布。** [完整阶段事实](unified-source-and-cli-matrix-public-facts.json)

用户新增的“检查，测试所有子命令”已列入本轮必要工作。命令入口已静态盘点，执行将使用最终候选和隔离环境，逐项记录真实子命令的退出码、关键结果与原件。帮助输出只证明入口解析，不能替代正常功能执行。

## 已合入的实际修正

| 范围 | 当前候选中的变化 |
|---|---|
| `ssmcp resume` | 单独识别命令，接入当前工作区的历史选择器。空历史提示后正常退出且不联网；有历史时要求交互终端，由用户选择，保留原工作区、归档与租约校验 |
| 恢复边界 | 临时聊天布局不落盘；不自动选择最近历史、不重放请求，也不恢复模型、PTY 或成员凭据 |
| Linux 协议夹具 | 正向集合明确五个实际提供者，退役 Playwright 有独立调用陷阱；另一份 CLI 目录夹具也已同步五项集合 |
| 安装安全测试 | 保留每个 Store 独立的测试目标，Windows ARM64 仍运行原有五项安装／卸载安全测试；没有以平台条件跳过它们 |
| 主线并行修正 | 正常合入独立进程 TLS 初始化测试、官方平台映射，以及 `computer --json` 的错误回执处理 |
| 发布策略 | 根包为 0.10.26，GitHub 二进制发行继续准备；registry 发布和生产服务器部署显式延期 |

`computer --json` 的操作失败会在 stdout 输出单行 MCP JSON 结果，包含 `isError: true` 和脱敏说明，并以非零状态退出。语法或管理器初始化错误走 stderr。未安装 Zuse 的版本查询仍可成功返回 `available: false`。这些行为来自已合入主线的代码与文档，最终候选的实测结果仍待收齐。[当前命令说明](https://github.com/LIghtJUNction/ssmcp/blob/fc650a91bdab60dea96ee7e18c15bec3af9f9b32/docs/computer-use.md)

## 全子命令矩阵范围

下面是待执行矩阵的范围，**不是通过清单**。会为有副作用的命令使用私有配置、私有会话、测试中继或已有服务替身；不会把真实系统注册、外部 OAuth、外部提供者与更新安装路径的未执行情况隐藏在帮助检查中。

| 命令族 | 需要分别核对的入口 |
|---|---|
| 信息与启动 | 帮助、版本与完整版本树、协议目录、列工具；裸启动、账号目标、服务器／context 选择、Linux local、CLI resume |
| 账号与目录 | login／setup、whoami、logout、list／ls、远端会话目录和交互选择 |
| 连接与终端 | 远程 process／thread attach、只读聊天、显式 PTY、本地保存会话 attach，及名称消歧 |
| 消息 | send 的目标、stdin、请求 ID、等待形式；wait 的 any／all／gather／count 及别名 |
| context 与 config | context 创建、列举、查看、检查、选择、移除；config path／get／set／unset，以及真实私有持久化后的读回 |
| trust | add／list／remove／path，私有公钥或 pin 的写入与读回 |
| Zuse 生命周期 | computer version／download／install／uninstall，成功、结构化失败和未安装状态 |
| 更新与界面 | update／check；ui show／check／init／preview，包括静态预览和无效配置 |
| 保存会话 | sessions create／list／status／start／stop／close／restart／logs／auth／remove |
| 服务管理 | service install／uninstall／start／stop／restart／status／run，区分 dry-run、替身验证与实际系统注册 |
| 007 | install／upgrade／run／start／stop／status／uninstall，以及 007 sessions 的列表与生命周期 |
| 内部与退役入口 | 内部 helper 由对应夹具覆盖；invite、join、旧 browser setup 和电脑控制开关等退役形式应明确拒绝 |

聊天 attach 与 Shell PTY、`sessions` 与 `service` 生命周期、`local --socket` 与聊天 `--input sock` 均是不同路径。TUI 的 `/resume` 等斜杠命令以及原生 MCP 工具不计作顶层 CLI 子命令。这里不把所有参数组合展开成笛卡尔积；结果表会明确每一条实际场景的覆盖边界。

## 当前验证与历史尝试

最终 `fc650a9` 的 V12 原件已完整读回：源码与依赖同步成功且工作区干净，随后等待共享 Cargo 锁超时，退出码 1；没有启动 toolchain、编译或产品测试，没有生成库测试程序。回执为 27564 字节，SHA-256 为 `5355c7ee25edcd404b86e40f152ae92af89546ce81c1f699ffe44bf9f9186564`。这是资源排队结果，不能称为编译或产品失败。执行侧正在评估专属构建目标目录，继续同一固定源码的既定验证。

| 来源 | 已取得的实际结果 |
|---|---|
| `4c6cd21` | Node／发布／部署脚本 64／118／35 项通过；库 private 1972 通过、0 失败，另有 host 6 项和必需 code-mode 2 项通过；Linux 与 Windows GNU 严格 Clippy 通过 |
| `6e9336b` V9 | 格式检查退出 1，未编译、未运行产品测试；三处排版已由后继提交修正 |
| `d67ae61` V10 | 新库测试程序构建通过；没有把它作为后续合流源码的测试成绩 |
| `84256bf` V11 | 新库测试程序构建通过；随后测试任务等待共享锁 180 秒到期，`tests=[]`，没有执行产品测试 |
| `fc650a9` V12 | 源码和依赖同步成功；共享资源等待超时，编译、测试均未启动 |

V11 排队结果的原件为 **289 字节**，SHA-256 为 `cebd058b340edee3adac19ab7cc4375d6047a4b8feae4296784ff482afbc6d70`；退出码 1 表示未取得资源，不能称为测试断言失败。上述记录均保留原源码身份，旧成功不会迁移到最终候选，旧失败与未执行也不会被后续成功覆盖。

## 依赖与发布核验

最终源码的六份发布配方已重新读取，字节数、SHA-256 与 Git blob 全部匹配[固定 432900 的 H2 包](https://github.com/LIghtJUNction/ssmcp/tree/432900420ef4fbe093ad284cb49e66e1e2892800/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-v26-unbound)。完整递归树共 1299 项、未截断。

| 身份 | 当前实际值 |
|---|---|
| Cargo.toml | 9795 字节；SHA-256 `3e2bf8c01e41d1a292798154a9e2bac455222d6a6ad286e7a4ff4ae42ac25b94` |
| Cargo.lock | 67037 字节；SHA-256 `0ab882b9801d48dcdf92ef6b5217752a5dc34bf75deddae62e84fefd14090465` |
| zcall | 0.4.2，revision `aaac54236f7ce0f4e1559c6e3b78d680d9dabf41` |
| zharness | 0.6.0，锁文件与 gitlink 均为 `8907262ce6c8494f2849f8e5abd84f7a3da2e405` |
| 其他直接 gitlink | zplugin `592a1be`、ztuio `9c632b3`，身份保留 |

H2 会按实际候选读取锁文件、依赖图与 gitlink，无需把新依赖 SHA 写成固定常量。旧静态包中的 gitlink 观察仍属于旧配方来源，不能改称它描述最终源码。**H2 仍未导入或执行，实际发布运行也未绑定。**

现有发布合同将产品检查及资产构建固定到候选 S；实际工作流运行的主线 P 需包含 S，并保持六份发布配方一致。合同没有要求整个 S／P tree 相等，后续主线的普通代码改动不会自动进入本次 tag 资产。这是已审工作流的现有行为，没有放宽原始回执、checkout、job、artifact 或公开文件核验。[发布工作流](https://github.com/LIghtJUNction/ssmcp/blob/fc650a91bdab60dea96ee7e18c15bec3af9f9b32/.github/workflows/release.yml)

## 文档中的正确现用入口

服务文档还残留旧 `invite` 推荐和 `--ssh` 示例，已列入发布后的最小清理草案。当前可使用以下 OAuth 形式：

```sh
ssmcp alice@relay.example
ssmcp login --server https://relay.example
ssmcp sessions create workstation alice@relay.example
```

示例中的账号、服务器与会话名应替换为自己的值。本轮保持冻结产品源码不变，正确入口先在这份说明中明确；不把旧文档残留当作现行命令应成功的依据。

## 发布与同步状态

v0.10.25 的 [Release 运行 44934221685](https://github.com/LIghtJUNction/ssmcp/actions/runs/44934221685)仍保留失败终态：12 个 job 成功、3 失败、4 跳过；旧标签未移动，旧运行未重跑，原生完整返回缺口也未拼装补造。

开发机主工作区上次完成的正常同步仍是 `d4887ae`，用户修改和本地依赖状态已保留；最终候选在独立验证工作区推进。后续主线合并与再次同步将以实际回执记录，当前不能声称用户正在运行的客户端已升级。下一步继续收齐最终源码的构建、全子命令矩阵、相关 E2E、安装生命周期和正式发布。
