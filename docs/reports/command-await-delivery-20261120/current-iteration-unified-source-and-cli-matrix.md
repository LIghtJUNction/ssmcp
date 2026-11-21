# SSMCP 当前迭代：Zuse 兼容已合流，排查 Agents 历史与面板

**更新于 2026-11-21 20:19:27 UTC。** 当前候选为 [58bf72e](https://github.com/LIghtJUNction/ssmcp/commit/58bf72e381bb07f28f399fd76adb96f105f72dac)。上一候选 `db6f14b` 的六平台 CI 已全部成功，随后实际验证发现当前官方 Zuse 0.6.0 包可以下载，但安装被新增示例文件阻断。对应兼容修复已正常合流，只增加归档解析和测试两处变化；此前的 `resume`、历史租约与跨平台安装保护均保留。**本轮 v0.10.26 尚未发布。** [完整阶段事实](unified-source-and-cli-matrix-public-facts.json)

新来源的 [CI 44944812920](https://github.com/LIghtJUNction/ssmcp/actions/runs/44944812920) 继续运行；最近实际状态中 Windows x64 与 macOS ARM 已成功，其余平台仍在验证且未出现失败步骤。开发机 V15 完成源码和依赖身份准备后，等待共享构建资源 180 秒到期，未启动编译或产品测试。用户新增的 Agents 历史与面板问题已纳入本轮，正在收敛修复范围；后续仍需完成 12 份分组 CLI 脚本、22 次配置／信任／界面场景、空历史恢复、真实 Zuse 0.6.0 生命周期、私有更新及 007 会话入口验证。旧源结果均保留原始身份。

## 新增的 Agents 历史与面板问题

用户报告“所有 agent 的工具调用历史都一样”，并反馈 Agents 面板缺少实际作用。本轮正在沿实际界面入口、已认证成员身份、事件写入与保留、选中成员过滤，以及面板选择／消息／刷新这条链核对；尚未确认根因，尚未声称修复完成。

当前文档对成员历史已有明确约定：右键按实际点击行的成员 ID 打开，成员视图只显示能核对该 ID 的事件；旧记录或缺少身份的事件保留在进程视图，不按显示名称分配给成员。[查看线程历史](https://github.com/LIghtJUNction/ssmcp/blob/58bf72e381bb07f28f399fd76adb96f105f72dac/docs/client-accounts.md#查看线程历史)

普通 `Call` 的代表运行审计、全局 `/audit`、全局 Logs 与成员历史具有不同范围。排查会先匹配用户实际看到的入口，再验证不同成员的事件和可用操作，避免仅修改显示名称而遗漏归属问题。[Agent 面板与成员身份](https://github.com/LIghtJUNction/ssmcp/blob/58bf72e381bb07f28f399fd76adb96f105f72dac/docs/client-ui.md#agent-面板与成员身份)

## 已合入的实际修正

| 范围 | 当前候选中的变化 |
|---|---|
| `ssmcp resume` | 单独识别命令，接入当前工作区的历史选择器。空历史提示后正常退出且不联网；有历史时要求交互终端，由用户选择，保留工作区、归档与租约校验 |
| 历史会话锁 | 使用已有的 `HistoryLock` 在 Unix 释放会话时显式解锁；即使旧文件描述符的副本仍然打开，新客户端也能取得租约。新增回归还检查关闭旧副本不会解除新拥有者的锁 |
| Windows 管道预期 | `--input pipe` 在非 Unix 平台先被平台检查拒绝，测试现在精确期待这条实际错误。六组非交互拒绝案例和帮助检查均保留 |
| Linux 协议夹具 | 三处正向目录统一为五个实际提供者，移除已退役 Playwright；独立调用陷阱、懒加载状态和无外部探测断言保留 |
| 安装安全测试 | 每个 Store 使用独立测试目标，Windows ARM64 保留原有五项安装／卸载安全测试 |
| 主线并行修正 | 已正常合入独立进程 TLS 初始化、官方平台映射和 `computer --json` 的错误回执修正 |
| Zuse 包兼容 | 接受 `examples/` 下单层 `.py` 数据和 15 个精确技能文件路径；仅允许无执行位的普通文件，有界读取后丢弃，仍只安装唯一目标平台可执行文件 |
| 发布策略 | 根包 0.10.26；继续准备 GitHub 二进制发行，registry 发布和生产服务器部署明确延期 |

历史锁阶段的 `fc650a9 → db6f14b` 包含两个提交、三个文件，合计 48 行增加、15 行删除。产品行为修改集中于历史锁；另外两处为测试预期修正。原有计时测试、等待时限和断言未放宽。[实际差分](https://github.com/LIghtJUNction/ssmcp/compare/fc650a91bdab60dea96ee7e18c15bec3af9f9b32...db6f14b5faea8d434f4107877005b7669c1c940e)

恢复会话的临时聊天布局不落盘；用户仍手动选择历史，不重放请求，也不恢复模型、PTY 或成员凭据。`computer --json` 的操作失败输出一行带 `isError: true` 的脱敏 MCP 结果并非零退出；语法或管理器初始化错误走 stderr。[当前命令说明](https://github.com/LIghtJUNction/ssmcp/blob/db6f14b5faea8d434f4107877005b7669c1c940e/docs/computer-use.md)

## 当前候选的合流核对与验证状态

`58bf72e` 的有序父提交为 `db6f14b` 和主线 `add39e1`。相对 `db6f14b`，实际只修改 `src/computer/archive.rs` 与 `src/computer/tests.rs`，合计 200 行增加、9 行删除。新增五项回归覆盖两种归档的元数据接受、未知或可执行成员、链接以及不安全路径；当前新 CI 将在六个平台实际执行这些回归。[实际合流差分](https://github.com/LIghtJUNction/ssmcp/compare/db6f14b5faea8d434f4107877005b7669c1c940e...58bf72e381bb07f28f399fd76adb96f105f72dac)

静态读取确认：元数据内容校验后丢弃，不安装、不执行，也不作为指令读取。原有路径、文件类型、符号／硬链接、去重、64 MiB 大小、256 成员上限、ZIP 双视图与 CRC、TAR 尾部、外层 digest 和可执行头校验均保留。合流还保留每个 Store 独立的测试目标、Windows 普通安装安全测试，以及既有 `resume` 和历史锁修复。

此处的静态核对不替代当前来源的 CI 或实际安装结果。V15 任务退出码 1，stdout 2927 字节、stderr 0 字节，均到 EOF 且未截断；唯一错误为等待共享 Cargo 资源超时，库测试程序为空，未到 toolchain、编译或产品测试阶段。回执为 25686 字节，SHA-256 `1edc3813bdd768735dc4e891fbc047d2af4134f9c51b3082fa7fa4d723c3bf13`。执行侧先收敛 Agents 修复范围，再安排后继构建。以下是此前 `db6f14b` 已完成的真实结果。

## db6 基线的实际验证

开发机 V14 于 **19:48:01 UTC** 完成：源码与依赖身份、格式检查、锁定依赖获取和库测试程序构建均通过。原生任务退出码 0，stdout 4477 字节、stderr 0 字节，均到 EOF 且未截断；工作区干净。回执为 **30760 字节**，SHA-256 `1fd5cbe29477fbb1d9282c16920b968e86477f27c794a312fbd438fea4112e7f`。新库测试程序为 **160030096 字节**，SHA-256 `24645bc7457f907e3e90e8d81a6724e94e88dd63e695265ac194d87928d8ba95`。这份本机构建记录与下表的 CI 测试记录分别计量。随后一个本机全库测试任务等待共享资源 180 秒到期，`tests=[]`，没有执行产品测试；因此直接复用同一源码已通过的 CI 库测试，不重复全库运行。

| 同一 `db6f14b` 来源的平台 | 当前实际结果 |
|---|---|
| Windows x64 | job 成功；库 1301 通过、0 失败、4 忽略 |
| Windows ARM | job 成功；库 1302 通过、0 失败、4 忽略 |
| macOS Intel | job 成功；库 1423 通过、0 失败、5 忽略 |
| macOS ARM | job 成功；库 1423 通过、0 失败、5 忽略 |
| Linux x64 | job 成功；库 1985 通过、0 失败、13 忽略；发行构建与临时安装检查通过 |
| Linux ARM | job 成功；库 1985 通过、0 失败、13 忽略；发行构建与临时安装检查通过 |

四个已完成平台的日志都逐条确认三项 `resume` 测试和原计时／历史租约测试通过。各平台的 `release_client` 为 5/5，通过单独选择的 native plugin contract 为 1/1，ztuio contracts 为 13/13；`ssmcp` 二进制测试目标为 0 tests、成功结束。两项 macOS 还通过各 3 个流式下载安装边界测试。子进程的重复测试输出没有加进主目标计数。

继承文件描述符的两项历史锁回归带 `#[cfg(unix)]`，两个 macOS 和两个 Linux 日志均逐条通过；Windows 记为平台不适用。两个 Linux 的服务端单元测试还分别通过 13 项，19 个集成测试目标各自合计 102 通过、0 失败、17 忽略。另行执行或子进程过滤测试的结果未重复累加。

两份 Linux 完整日志分别明确记录：agents 469 项、unified 105 项、真实 PTY dashboard 80 项、input/history 150 项以及真实 user systemd 后台会话 182 项通过。固定源码 [e2e_input_history.py](https://github.com/LIghtJUNction/ssmcp/blob/db6f14b5faea8d434f4107877005b7669c1c940e/scripts/e2e_input_history.py) 确实把 `resume` 插入新进程的 argv，并等待自动打开历史选择器；两平台都验证恢复不会重投旧请求或重放原生工具，新请求只追加一次。该结论不单凭日志中沿用的 `/resume` 提示文字。

Linux 安装检查在临时 `DESTDIR` 中完成，四项静默 shell 断言验证客户端、服务端、systemd unit 和 code-mode 脚本存在；证据是实际命令、`bash -e` 和成功的步骤状态。界面 compositor 夹具的 58 项检查明确使用示例事件和隔离 PTY，不连接真实服务；其他会话和界面 E2E 的独立范围保留在事实记录中。这些同源结果可复用于命令覆盖矩阵，尚未映射的子命令仍待补齐。

## db6 本机补充构建与已确认的安装故障

用于补充 CLI 的本机任务已成功构建并保存 `ssmcp`、`ssmcp-frontend` 和 `ssmcpd`。随后 `vt_snapshot` 辅助程序编译达到 180 秒预算，进程退出 -15；其 stderr 只有编译中的提示，没有源码诊断。整个聚合任务因此为失败，不能写成整套构建通过。回执为 **6952 字节**，SHA-256 `1660aa56bcd17e905b93f6b0eb873f418641279a94776e011c13d74059a5ef57`；已有三个主程序可供不依赖该辅助程序的场景使用。

[Zuse v0.6.0](https://github.com/LIghtJUNction/zuse/releases/tag/v0.6.0) 于 18:54:10 UTC 发布。对其当前官方 Linux x64 资产的私有验证已完整读回：版本查询退出 0，下载退出 0，安装实际子命令退出 1，并返回 `isError: true`，错误为 `Unexpected member in the Zuse release archive: examples/session_client.py`。外层任务也退出 1；stdout 2116 字节、stderr 0 字节，均到 EOF 且未截断。

下载资产为 1399658 字节，SHA-256 `6a38b67eddc26a5671658d4ea4b767922e7be78f44e5af480fa3ba14dd3bac15`，与实际 GitHub Release 资产身份一致。观察回执为 **5686 字节**，SHA-256 `9f8355ac9b705f613b608bf3f5b36c9edfe3538c41af30fe75e1ce401617776b`。这一实际失败促成当前窄范围合流；修复后的下载、安装和卸载结果仍待取得。

## 上一轮 CI 的失败及对应修正

[旧 CI 44941345312](https://github.com/LIghtJUNction/ssmcp/actions/runs/44941345312) 固定在 `fc650a9`，终态为两个 macOS 成功、四个平台失败。原始失败保留，后继成功不会覆盖它。

| 平台 | 旧源上的实际失败 | 后继修正 |
|---|---|---|
| Linux x64 | 库 1983 通过、1 失败、13 忽略；原计时测试恢复历史时收到“conversation is open in another client” | 复用显式解锁的历史锁守卫，并新增保留旧文件描述符副本的回归 |
| Windows x64／ARM | 库分别 1300／1301 通过，各 1 失败、4 忽略；测试统一期待前台交互错误，实际先收到 Unix-only pipe 错误 | 按平台精确断言实际错误，保留全部拒绝案例 |
| Linux ARM | 库 1984 通过、0 失败、13 忽略；agents 469 项通过；统一验收在打印 20 条 PASS 后失败 | 统一验收的正向集合移除已退役 Playwright |

Linux ARM 的具体 provider 断言来自执行协调者读取的原始 artifact `14722175948`；独立读取的 job 日志能确认统一验收失败和 artifact 上传成功，没有打印该断言。20 条 PASS 不表示 20 项失败。

## 历史构建尝试

| 来源 | 已取得的实际结果 |
|---|---|
| `4c6cd21` | Node／发布／部署脚本 64／118／35 项通过；库 private 1972 通过、0 失败，另有 host 6 项和必需 code-mode 2 项；Linux 与 Windows GNU 严格 Clippy 通过 |
| `6e9336b` V9 | 格式检查失败，未编译或测试；三处排版由后继提交修正 |
| `d67ae61` V10 | 新库测试程序构建通过，未作为后继合流源码的测试成绩 |
| `84256bf` V11 | 新库测试程序构建通过；随后测试任务等待共享资源 180 秒到期，未执行产品测试 |
| `fc650a9` V12 | 源码与依赖同步成功；等待共享资源 600 秒到期，未启动编译或测试 |
| `fc650a9` V13 | 随后取得同一共享构建资源，19:35:22 UTC 编译成功；旧源 CI 结果见上 |
| `db6f14b` V14 | 19:48:01 UTC 编译成功；新源 CI 和剩余 CLI 验证分别记录 |

执行继续使用既有共享构建目标，没有另建目标目录，也没有终止其他拥有者。等待超时属于资源排队记录；所有旧结果均保留当时源码身份。

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

## 依赖与发布核验

此前已完整读取 `fc650a9` 的六份发布配方；先后实际读取 `db6f14b` 与 `58bf72e` 的完整树，确认六份配方、两份 Cargo 文件及发布说明的九个 Git blob 和字节数均未改变。六份发布配方继续匹配[固定 432900 的 H2 包](https://github.com/LIghtJUNction/ssmcp/tree/432900420ef4fbe093ad284cb49e66e1e2892800/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-v26-unbound)。完整递归树共 1299 项、未截断。

| 身份 | 当前实际值 |
|---|---|
| Cargo.toml | 9795 字节；SHA-256 `3e2bf8c01e41d1a292798154a9e2bac455222d6a6ad286e7a4ff4ae42ac25b94` |
| Cargo.lock | 67037 字节；SHA-256 `0ab882b9801d48dcdf92ef6b5217752a5dc34bf75deddae62e84fefd14090465` |
| zcall | 0.4.2，revision `aaac54236f7ce0f4e1559c6e3b78d680d9dabf41` |
| zharness | 0.6.0，锁文件与 gitlink 均为 `8907262ce6c8494f2849f8e5abd84f7a3da2e405` |
| 其他直接 gitlink | zplugin `592a1be`、ztuio `9c632b3`，身份保留 |

H2 会按实际候选读取锁文件、依赖图与 gitlink，无需把新依赖 SHA 写成固定常量。旧静态包中的 gitlink 观察仍属于旧配方来源，不能改称它描述最终源码。**H2 仍未导入或执行，实际发布运行也未绑定。**

现有发布合同将产品检查及资产构建固定到候选 S；实际工作流运行的主线 P 需包含 S，并保持六份发布配方一致。合同没有要求整个 S／P tree 相等，后续主线的普通代码改动不会自动进入本次 tag 资产。这是已审工作流的现有行为，没有放宽原始回执、checkout、job、artifact 或公开文件核验。[发布工作流](https://github.com/LIghtJUNction/ssmcp/blob/db6f14b5faea8d434f4107877005b7669c1c940e/.github/workflows/release.yml)

## 发布后的文档清理草案

两份服务文档仍含旧 `invite` 推荐及 SSH 启动示例。[最小清理补丁](postrelease-session-docs-cleanup.patch) 已准备并独立复核：仅 `docs/session-service.md` 和 `docs/session-control.md`，14 行增加、13 行删除，**尚未应用**。补丁语法已对照当前 `db6f14b` 代码，保留历史说明并明确当前 OAuth 入口：

- `ssmcp alice@relay.example`
- `ssmcp login --server https://relay.example`
- `ssmcp sessions create workstation alice@relay.example`

远程聊天 attach 与交互式 Shell PTY 的区别也会明确。该草案留待发布后单独提交；当前候选和已固定发布说明保持原身份。

## 发布与同步状态

v0.10.25 的 [Release 运行 44934221685](https://github.com/LIghtJUNction/ssmcp/actions/runs/44934221685)仍保留失败终态：12 个 job 成功、3 失败、4 跳过。旧标签未移动，旧运行未重跑，缺失的原生完整返回未拼装补造。上述两次独立 CI 均不是正式 Release 运行。

开发机主工作区上次完成的正常同步仍是 `d4887ae`，用户修改和本地依赖状态保留；当前候选在独立验证工作区推进。最终正常合并后仍需再次同步。GitHub 于 20:13:05 UTC 的实际读取确认 v0.10.26 标签和 Release 均尚不存在。当前不能声称用户正在运行的客户端已升级；下一步先确定并修复 Agents 历史与面板问题，再收齐最终来源的 CI、全子命令矩阵、空历史恢复和新版 Zuse 安装场景，随后完成正式发布及公开资产核验。
