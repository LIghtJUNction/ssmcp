# SSMCP 当前迭代：迁移至 Zuse CLI，源码调整进行中

**更新于 2026-11-21 14:54 UTC。** 原任务收到新的架构要求：SSMCP 移除内置电脑控制功能，桌面操作直接交由 **Zuse CLI** 完成；**浏览器控制同样迁移**。保留的 computer 工具仅承担工具**版本查询、下载、安装、卸载**。[本次范围变更记录](architecture-zuse-cli-scope-change-public-facts.json)

当前候选 `02aa4dcd18786062ef44be6c76422dff054fbfed` 的后续运行／界面验收和发布已暂停，源码窗口已交回原作者实施迁移。新实现尚无报告组已核对的固定提交，不能写成“迁移完成”，也不能把方向描述为仅默认关闭原功能。

## 已完成的旧候选里程碑

- **Gate20：35 个阶段全部通过。** 报告组已独立读取原件；[完整报告](current-iteration-Gate20-passed.md)和[最终事实](gate-S02aa-r20-final-public-facts.json)保持原样。
- **N20：验收方报告一次来源校验通过**，errors 为空，产品执行次数为 0。报告组目前收到的是执行方回执，尚未独立读取其 proof／receipt 原件；文件身份保留在范围变更记录中。
- 当前候选尚未启动后续 runtime／UI 产品运行。这次暂停来自新架构要求，不将它记为运行测试失败，也不把旧候选成绩转移给后继源码。

## 后续说明的适用范围

迁移期间，旧内置 Computer／Browser 执行器、输入开关、generation gate 与相关 UI 记录均保留原来源。不能据此声称这些机制已约束直接执行的 Zuse CLI。聊天、消息发送、引用、历史等无关功能不因本次要求被推定移除。

旧 H2 准备物仍是**旧发布配方下的静态、未执行材料**；若实际迁移改变配方或资产，需要依照新源码核对，不能仅替换 SHA 就认定适配。附后的旧控制边界与 EOF 分析同样属于历史资料，没有新增操作系统隔离或权限改造结论。

下一步跟进原作者的实际补丁、完成回执及相应验证范围，继续推进发布。现阶段不新增执行链，不提前声明新架构或新版本已经验证完成。

---

## 历史检查点原文

以下内容保留各自记录时间、源码归属和验收范围。

# SSMCP 迭代记录

> 工作稿：发布验收和实际发布结果尚待填写，不能作为已经发布的证明。

## 当前状态（截至 2026-11-21 02:28 UTC）

修复候选 Q [`647bce2e`](https://github.com/LIghtJUNction/ssmcp/commit/647bce2ec9cf74beab43adea831c2e0712d28ea8) 的 **35 项本地 Gate 检查已全部通过**。对应 tree 为 `5143f90c05066c95b2df58a9eab1831e05888a62`，版本为 **0.10.25**。02:28 UTC 的 GitHub 回读确认 [PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 仍为开放 draft，head 为 Q、base 为 `f33ed8f1`，尚未合并。本次结果属于 Q；源码负责人正在修正后续界面验收夹具中的旧状态预期，后继提交尚未冻结，UI、实际运行、P1 与正式发布均未完成。详见[本次结构化检查点](gate-checkpoint-v25-0227.json)。

Q 相比失败候选 C 只改一行 Rust 测试输入：为 `--agent codex` 明确加入 `--input terminal`，使该测试进入预期的前台模式校验。原错误文本与“未创建会话目录”断言仍保留，产品实现未改。两项依赖真实主机 socket 环境的测试转入必须执行的六项主机分区；六项都实际通过，未增加忽略项或绕过原安全检查。C 的 2049 通过、3 失败记录继续按原源码和原运行保存。

Q 的 Gate 于 **02:22:17.674951 UTC** 完成，driver、launcher、outer 均退出 0。主审对完整 Gate 文件核对字节数和 SHA-256，并读取身份、最终状态与全部 35 个检查结果；随后完整读取启动器、外层退出和独立完成审阅回执。结果包含 **2050 项普通库测试、6 项主机测试、101 项普通集成测试、1项显式运行的 ignored worker 测试及另外14项必须显式运行的 ignored 测试通过**，覆盖 26 个 Rust 阶段和20个集成测试目标。库原日志汇总为 2050 通过、0 失败、23 忽略、6 过滤，Rust 计时34.49秒；主机原日志为6通过、0失败，Rust计时0.01秒。这里的忽略与过滤计数只描述各自日志分区，不代表把失败略过。

02:34 UTC 计数澄清：此前检查点将20个集成目标的102项通过统称为“普通集成测试”。完成审阅的补充原件和 worker 原日志确认，它们应分为19个普通目标的101项通过，加上使用 `--ignored --exact` 显式执行的1项 worker 测试；另外四个 mandatory-ignored 分组的14项另计。这里只修正分类，原 Gate、测试次数和通过总数均不变。

与原始功能目标对应的 **17 项回归测试**已逐条对照同一 Q 的库日志，每条通过记录恰好出现一次。它们覆盖原生收件人和回执、草稿保护、回复归属、引用预算与冻结重试、真实队列、技能选择和显式发送。17项是2050项的子集，不重复加计，也不代替 PTY 或界面验收。原生收件箱中 `@`、`$` 文件语法仍保持字面文本，不自动扩展本地附件。

完成回执记录五份私有本地产物：库测试程序、`ssmcp`、`ssmcp-frontend`、`ssmcpd` 和终端快照解码器。原构建负责人已独立核对它们的 ELF、摘要、权限及真实 rustc 调用，五项 Cargo `fresh=false`；本次报告者完整读取该回执，没有再次重算五个二进制摘要。九项输入摘要在执行前后相同，结束时源码仍为干净 Q。**这些是本地产物，尚非正式发布的六平台归档。**

进入 UI/runtime 验收前，源码负责人报告静态夹具审阅仍发现“显式选择 Next caller 却期待 null”的旧断言，正在由原作者统一做最小 Python 修正。现有状态值为 `next`；本次没有把这一静态发现写成已运行的 UI 失败，也没有将 Q 的成功移记到后继源码。后继最终源码、对应 Gate 与产品运行回执待取得。

发布路线方面，原 push-only 校验器及其 v25 UNBOUND 副本均保持不变；独立 manual-main 后继正在静态准备，尚未执行。它将分别核对实际 main 工作流提交 P 和最终候选源码 S，保留19个精确 job、16个发布资产、crate、gitlink 和源码字节合同，并增加真实 dispatch 请求与构建 artifact 对应证据。当前 main 仍为 f33，尚未含最终候选和持久 deferred25；P、S、正式 run 都保持未绑定。[发布路线复核](publication-route-review-0227.md)记录助手工具合同的来源、GitHub 事件规则和两份提交的区别。另一条0.10.24发布线的真实负责人身份仍未确认，有限页面中没有 tag/run 不能证明共享发布队列空闲。

## 历史状态（截至 2026-11-21 01:58 UTC）

合流候选 C [`d29dae7e`](https://github.com/LIghtJUNction/ssmcp/commit/d29dae7e2b9ff6794f17f3c96c0cc37977936084) 已正常推送并冻结，tree 为 `bf393700928fd9aa37f2dea8b8683ebe13b114cd`，版本为 **0.10.25**。[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 仍开放、draft、未合并。合并提交 `c0152934` 的两个父提交为 v25 准备提交 V 与公开修复来源 D4，随后 C 只补充发布说明。原作者于 01:43:14 UTC 退出 0；正常推送于 01:45:16 UTC 退出 0；冻结回执时间为 01:46:07 UTC。主审于 01:50:14 UTC 实读冻结原件及工作树，确认 exact C、tree 相等、工作树干净；随后回读作者、静态检查、推送与 GitHub 身份回执。详见[本次结构化记录](frozen-source-v25-0152.json)。

C 中多图片独立卡片、按事件 ID 绑定预览、历史与调用归属，以及 Unix 输入租约显式解锁修复均已合入。五个图像/UI文件与 D4 的 Git blob 完全相同。Dashboard 采用 D4 的完整版本：先等 Enter Apply 消失与物理光标隐藏，再断言包括 audit focus、可见性、filter 和 scrollback 在内的完整 navigation 状态；它与 B/V 把 focus 放进重试条件的方式不同，focus 检查本身保留。新增图片测试仍限于 `test + unix`，源码存在不等于实际 PTY 或 Windows 已验证。

C 的实际 Gate **已失败**。主审在 01:52:15 UTC 读到的 running 快照保留为过程记录；01:58:00 UTC 对最终回执全文计算摘要并读取其身份、完成状态和全部检查条目，确认同一 `run_id=47ff4beb94d03df6834a6460b493a5d7` 在 `library-private` 步骤退出 101。此前 release-policy、deployment-policy、fmt、library-compile 四步记为退出 0，仍不构成完整验收通过。随后完整读取原日志的三处失败块至 EOF：Rust 汇总为 **2049 通过、3 失败、23 忽略、4 过滤，35.62 秒**；外层该步计时为 35.63 秒。两项 socket 测试被 `/tmp` 父目录所有者/权限安全检查拒绝；session 前台模式测试的错误文本断言未满足，原日志没有打印其实际返回错误文本。源码负责人正在基于这些原始失败诊断，并由同一作者修复，不能先放宽产品安全检查或把失败重记为通过。

该次失败尚未完成五份产物交付集、最终产品运行或 P1 验收；发布核验、tag 和正式发布均未因此启动。启动回执中的独占共享目标锁及九项输入摘要保留其实际阶段归属。V 的 11 项策略测试、D4 的运行成绩和 FC/781 的 A/B 数字继续保留原归属；旧阶段的成功不能抵消 C 的实际失败。

[0.10.25 发布核验工具](publication-verifier-v25-unbound/README.md)已准备为独立 **UNBOUND** 副本，代码只改版本号，旧文件与旧发布副本保持。候选 SHA 与正式 run ID 均未绑定，网络核验尚未执行。静态复核保留 tag-push、19 个 job（18 成功加部署跳过）、16 个发布制品、crate、gitlink 和源码字节检查。实际下载范围是 manifest、校验文件及官方 crate；二进制归档和 installer 使用 GitHub asset size/digest 对照，不能写成全部归档已独立下载。

已核对 C 的[发布工作流](https://github.com/LIghtJUNction/ssmcp/blob/d29dae7e2b9ff6794f17f3c96c0cc37977936084/.github/workflows/release.yml)：顶层按 tag 分组且不取消正在运行的任务；`publish_release` 则共用 `github-release-publication` 组，同样 `cancel-in-progress: false`。按 [GitHub 官方规则](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency)，默认同组只有一个 pending 位置，新的排队任务仍可能替换旧 pending。因此正式 tag25 前需与另一发布线协调，并回读实际正式 run 和共用队列；若 24 正式 run 已发起，则先等其结束。01:55 UTC 的实际 GitHub 查询中，tag24 ref 返回 404，latest Release 仍为 0.10.20；最近十条 run 中 D4 分支 create 为 skipped，未见 24 正式 run。这是有限范围的观察，不能证明全局队列空闲。本次没有推 tag 或取消其他任务。

## 历史状态（截至 2026-11-21 01:33 UTC）

0.10.25 准备提交 P [`f4e6ff04`](https://github.com/LIghtJUNction/ssmcp/commit/f4e6ff043eef6686a9013750ffcb8ab66926b9d4) 已公开，tree 为 `d6546dd46464ac1cbb1c3aabc0493638a44649f7`，唯一父提交为 B；[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 仍开放、draft、未合并。主审于 01:25:44 UTC 核对了完整六文件差异、PR 与实际 Cargo 版本。deferred 列表已加入 0.10.25，未延后对照改为 0.10.26；现有 `DeploymentPolicyTests` 的 11 项测试已执行通过，unittest 计时 2.587 秒。

P 仍是准备提交，尚非最终冻结。主审于 01:27:48 UTC 实读的源码冻结工作树仍为 B；正常推送与策略测试回执已核对，但产品构建、运行验收、最终 P1 和正式发布没有因此通过。InputLease 附录只界定固定源码行为及平台解锁约束，未证明实际 Linux FD 持有者或 Windows 泄锁。完整来源、执行范围与时间历史见[本次记录](release-input-lock-review-0122.json)，旧性能数据及编译状态更正保持原归属。

01:33:27 UTC 已独立核对后续公开修复来源 [`d4c4b9c6`](https://github.com/LIghtJUNction/ssmcp/commit/d4c4b9c6fe09a4d7b246ca54a08f5c7b258289fb)及其完整十提交链，其中已有 Unix InputLease 显式释放修复和描述符副本回归。源码负责人将由同一作者正常合入准备分支；本检查点尚无最终合流提交或冻结结论，也没有引用该独立修复线的运行成绩。

## 历史状态（截至 2026-11-21 01:01 UTC）

B 的最终绑定执行与发布已暂停，正在协调版本意图和既有验证任务的占用。根据源码作者与协调者的只读观测，另一条既有发布链正在本机验证 0.10.24，并持有共享构建目标锁；主审尚未独立读取该进程细节，因此此项保留转述限定。本组没有启动或排队新的重任务。原生会话进度已提出本功能链顺延 0.10.25 的计划，但最终版本与后续冻结源码尚未确认。

[`be6da5bf`](https://github.com/LIghtJUNction/ssmcp/commit/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1) 于 00:43:00 UTC 按 0.10.24 公开冻结的事实保持有效，[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 仍为开放 draft；该历史源码身份不等于当前版本协调已经完成。B 的构建和运行验收尚未启动，最终 P1 与正式发布均无通过结论。[冻结源码记录](frozen-source-0047.json)分别保留冻结事实与后续协调状态，原 FC/781 编译和 A/B 证据继续按原源码归属。

主审于 00:50:52 UTC 仅通过 Git 对象读取，确认另一条本地 0.10.24 分支的当时 HEAD 为 `35de95d2924184daf994710b6f54a5c2a89b1218`、tree 为 `8731180414b41beb63220acf43a22957ad457127`。公开 f33 是其祖先，24c 与 B 均不是；读取前后 HEAD 相同。这项证据只确认源码身份和继承关系，没有证明运行中的 validator 绑定该 HEAD，也没有独立确认进程归属或锁持有者，不能把两条链的运行成绩互相替代。01:01 UTC 的 GitHub 回读仍确认 PR #159 头为 B、公开 main 为 f33。

## 历史状态（截至 2026-11-21 00:48 UTC）

候选 B 已公开并冻结为 [`be6da5bf`](https://github.com/LIghtJUNction/ssmcp/commit/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1)，tree 为 `5e8ea273c8f8bd8b0e7b1f87ccc0d19fd594601b`，实际 Cargo 版本现为 **0.10.24**。[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 仍开放、draft、未合并。普通推送于 00:42:08 UTC 退出 0，远端与 GitHub head 相等；00:43:00 UTC 的冻结回执记录工作树干净。

本次进度是源码冻结。24c 与 B 之间已审 CLI、粘贴和回复相关的 18 个源码 Git blob 完全相同，可复用对应静态结论；FC/781 的编译、测试状态和 A/B 数字继续保留原归属。B 的构建和运行尚未开始，等待既有共享计算窗口明确交接，没有启动或排队新的重任务。最终 P1 与正式发布仍待完成，**0.10.24 当前是候选版本，尚非发布完成记录**。见[冻结源码记录](frozen-source-0047.json)；[24c 记录](source-union-0038.json)作为本次增量的历史检查点保留。

## 历史状态（截至 2026-11-21 00:39 UTC）

[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 仍开放且为 draft，公开头为中间合流 [`24c32869`](https://github.com/LIghtJUNction/ssmcp/commit/24c32869ba730f4a91406ebd6b05e18a462bec80)。它合入固定 fc 与 f208，随后调整一条 Composer 测试预期；当前 Cargo 版本仍为 **0.10.22**。普通推送退出 0，远端回读一致，未使用 force。该源码只完成静态检查，没有针对 24c 的构建、测试或性能执行。

粘贴核心路径和七个新增测试在已审范围内保留，CLI 与回复边界静态审阅未见确定阻断。FC 测试已编译、FC/781 A/B 已完成的历史证据均保留原归属，不能记为 24c 通过。后续目标版本已选定 **0.10.24**，同一作者将继续纳入公开 f33；当前尚无最终源码冻结、构建、运行验收、P1 或发布结果。详见[本次源码记录](source-union-0038.json)；下方“版本未选”等表述保留其原观察时间。

## 历史状态（截至 2026-11-21 00:24 UTC）

本次补正文档事实与分段指标，没有新增测试或测量执行。新增七个 Rust 测试已完成静态审阅，并随成功的 `cargo test --lib --no-run` 编入库测试产物；尚未执行。原 [91f 检查点](https://github.com/LIghtJUNction/ssmcp/blob/91f98f84e9320042e382b93e22f7e256df3282b9/docs/reports/command-await-delivery-20261120/README.md)“仅静态”的表述遗漏了编译成功事实，现已注明更正。

固定 fc 对 781 的七项 primary ready 观测及每侧 `n=1` 全部保持原数值。分段数据有增有减：ready 降低不能概括为所有环节都变快。具体见[分段指标记录](candidate-ab-submetrics-0021.json)；最终源码整合仍在进行，fc CI、最终整合验收及发布状态没有因本次文档修正而推进。

## 历史状态（截至 2026-11-21 00:13 UTC）

固定候选 [`fc06728a`](https://github.com/LIghtJUNction/ssmcp/commit/fc06728a36d0b4d2920e1e2bc29885721ced5780) 已完成对 781 完整基线的粘贴 A/B：六场景和 Ctrl-V 两侧均通过，候选两步骤退出 0，源码、产物及载荷身份检查通过，输入前后不变。两轮完整冻结 cohort 的 18 个顶层字段逐值相等；产品源码和二进制分别绑定各自构建回执，不要求相同。

本轮每场景各侧 `n=1`，仅作描述性比较。普通输入 16 KiB 的 ready 观测为 2439.160607 → 27.853111 ms；其余六项见文末配对表。结果只属于固定 fc 候选，最终整合源的性能、验收和发布仍待验证。新增七个 Rust 测试已完成静态审阅并编入成功构建的库测试产物，尚未执行；fc 的 CI 结果尚未确认。（00:24 更正原 91f 对编译事实的遗漏，未新增执行。）

[本次候选记录](candidate-ab-0010.json)链接此前固定检查点；下方全部历史和失败尝试按原时间保留。当前正进行与公开 main 的源码整合，版本尚未选定，本次不是发布完成记录。

## 历史状态（截至 2026-11-20 23:41 UTC）

固定 781、同一 `d2204375` build-only 回执的第四轮粘贴基线已通过：六场景和 Ctrl-V 步骤均退出 0，输入前后不变。每场景只有一次描述性观测；前三轮失败保留原归属。当前冻结工具为 R3，R2 作为历史保存。

原作者正移植粘贴优化，尚无候选结果或 A/B；18 项定向验收、最终 P1 和正式发布继续待验证。[本次事实](paste-attempts-2333.json)引用[固定历史检查点](https://github.com/LIghtJUNction/ssmcp/blob/7b1ca2093a382b5bde5b7bbe288e39cd06276d6c/docs/reports/command-await-delivery-20261120/baseline-build-2253.json)。版本观察另列，不并入 PR #159 验收。

## 历史状态（截至 2026-11-20 23:06 UTC）

固定源码 [`781b87df`](https://github.com/LIghtJUNction/ssmcp/commit/781b87df1d364137b6885f17f631a3e9e991bd58) 的开发机五份构建产物已于 22:53:16.145 UTC 完成，回执状态为 `build_completed / build_only`；三个构建命令及产物保留、metadata、前后检查均完成。五项均为 Cargo `fresh=false`，回执中的实际链接输出、Cargo 对外产物和保留副本在字节数与 SHA256 上一致。此结论来自主审实际读取回执，未另行对五个二进制重算哈希。

随后首轮 PTY 在测量夹具的目标就绪断言处退出 1：17 条账户夹具前置检查通过，但 `samples=[]`、`frames=[]`，Ctrl-V 步骤未执行，尚无粘贴基线或 A/B 数据。对[会话选择分支](https://github.com/LIghtJUNction/ssmcp/blob/781b87df1d364137b6885f17f631a3e9e991bd58/src/client/terminal/conversation.rs#L1271-L1276)及[状态快照](https://github.com/LIghtJUNction/ssmcp/blob/781b87df1d364137b6885f17f631a3e9e991bd58/src/client/terminal/conversation.rs#L175-L179)的检查确认 explicit `member:next` 对应 UI target 为 `next`、`native_recipient` 为 None；旧夹具要求 target 为 None，与该状态不符。后继工作限定为外部夹具条件及相应哈希绑定修正，其实际运行结果待取得。

781 的三平台 CI 通过记录继续保留。18 项定向验收、最终 P1 和正式发布仍未完成，6733 粘贴补丁尚未应用。本次增量见[构建与首轮尝试记录](baseline-build-2253.json)，此前完整事实见[固定历史检查点](https://github.com/LIghtJUNction/ssmcp/blob/a74daa7e0d2ce32f00ec09b6b6e511b7bdcde49c/docs/reports/command-await-delivery-20261120/integration-checkpoint-2241.json)；下方历史按原时间保留。

## 历史状态（截至 2026-11-20 22:45 UTC）

[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 的公开头为 [`781b87df`](https://github.com/LIghtJUNction/ssmcp/commit/781b87df1d364137b6885f17f631a3e9e991bd58)，仍为 draft、未合并。[Session service CI 44829555014](https://github.com/LIghtJUNction/ssmcp/actions/runs/44829555014) 已于 22:36:52 UTC 完成，Linux、Windows、macOS 三个平台全部成功。独立完整日志复核确认了各自的精确 checkout 与实际测试结果；781 的 Linux 外层库批次为 2034 通过、0 失败、23 忽略，38.29 秒。下方 441 的 30.44 秒结果保留为历史，不迁移到新提交。

开发机固定 441 的库和 release 编译均退出 0，随后在保留 release 产物时失败，尚未形成可验收的五份产物。相关修正正处理实际链接产物与保留副本的字节绑定。18 项定向检查已准备但未执行，实际 `--input none` 进程重执行、粘贴 A/B、最终 P1 和正式发布仍待完成。

显式路径版[发布核验工具](https://github.com/LIghtJUNction/ssmcp/blob/90a83b0b74d7553d9c34470c96f5a3233d984b74/docs/reports/command-await-delivery-20261120/publication-verifier/README.md) 已有固定文档提交与经主审实际回读的 [Drive 副本](https://drive.google.com/file/d/1vqDINv8cGfEt2r3NzpE8jSIn3JT-S_If/view?usp=drivesdk)，当前证据仅覆盖语法及本地参数/路径边界。完整来源与范围见[结构化检查点](integration-checkpoint-2241.json)。以下旧时间点均按原状态保留。

## 历史状态（截至 2026-11-20 22:16 UTC）

固定源码 [`441814bf`](https://github.com/LIghtJUNction/ssmcp/commit/441814bf9945e0b49bb3dde170e57762abf8318e) 的 [Session service CI 44826521363](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363) 已于 22:15:37 UTC 完成，Linux、Windows、macOS 三个 job 全部成功。Linux 该次库测试实际为 2034 通过、0 失败、23 忽略，30.44 秒；Windows/macOS 的 session、protocol、构建和 native lifecycle 步骤均成功。本结果只归属于这份源码和本次 CI，旧 c248 失败及其计数继续保留原归属。

开发机已取得构建锁并完成输入身份检查，当前仍在库测试产物的 `--no-run` 编译阶段；五份新产物及开发机产品验收尚未完成。下方 22:11 及更早内容均保留为历史状态，粘贴 A/B、最终 P1 和正式发布结果仍待取得。

本次来源与范围见[结构化检查点](integration-checkpoint-2216.json)。

## 历史状态（截至 2026-11-20 22:11 UTC）

[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 的公开头提交为 [`441814bf`](https://github.com/LIghtJUNction/ssmcp/commit/441814bf9945e0b49bb3dde170e57762abf8318e)，仍为 draft、未合并。它在 42e9 的 `--input none` Snapshot/重载源码修正之后，补充 c248 暴露的 UI 与 portable fixture 修改。[新 CI 44826521363](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363) 在本截止点仍运行中，没有通过结论；开发机也正按 441 构建，新的五份产物尚未完成验收。

c248 的三平台失败保留原归属：Linux 单元测试 2030 通过、3 失败、23 忽略；Windows/macOS 的 session、protocol 和构建通过，随后 native lifecycle 分别记录 40 条 PASS/1 条 FAIL、39 条 PASS/1 条 FAIL，PASS 含重复阶段检查。下方时间点均为历史记录。当前仍无新粘贴 A/B、最终 P1 或正式发布结果。

## 历史状态（2026-11-20 21:55 UTC）

[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 已推送到 [`c248d420`](https://github.com/LIghtJUNction/ssmcp/commit/c248d420e01d04c06be967e84543ce7d883f4cb3)，仍为 draft、未合并。该提交补齐两个 Linux socket 测试调用的新增参数，并将只供 Linux 测试使用的 `connect` 辅助函数限制到相应平台。[自动 CI 44824373375](https://github.com/LIghtJUNction/ssmcp/actions/runs/44824373375) 在 21:55 UTC 实读时仍运行中。固定 tree 为 `0017f29c650f2ecdc2beb80662da1b4e2ba90280`；这次两文件补丁已核对，运行结果待取得。

上一轮 [cae 自动 CI](https://github.com/LIghtJUNction/ssmcp/actions/runs/44823183393) 于 21:49:14 UTC 失败：三平台 fmt 成功、Clippy 步骤失败，产品测试、构建和 native lifecycle 均跳过。独立保留该次失败及更早的 b12 失败。原样转发与 HTTP 默认端口两项问题已在固定源码中修正；新发现的 `--input none` 重载超时路径仍待修正及实际验证。c248 的测试适配没有改变该重载逻辑。

下方各时间点保留为历史状态；粘贴 A/B、最终 P1 和正式发布结果仍待取得。完整来源与当前状态见[本次结构化记录](source-ci-checkpoint-2155.json)。

## 历史状态（2026-11-20 21:26 UTC）

[PR #158](https://github.com/LIghtJUNction/ssmcp/pull/158)、[PR #157](https://github.com/LIghtJUNction/ssmcp/pull/157) 与 [PR #155](https://github.com/LIghtJUNction/ssmcp/pull/155) 已合入，本轮合流检查点为 [`cf0ab581`](https://github.com/LIghtJUNction/ssmcp/commit/cf0ab581700918a37fcccbebb93de2e0cab02841)。context 的三种本地重载已在独立固定源码 bbeb 上完成定向运行；输入第一阶段则以 [draft PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 公开，尚无编译或产品运行验收。已合入、定向运行通过和源码阶段完成是不同进度，不能合并为新主线的完整验收结论。

下方 20:54 内容保留为当时的历史状态。后续合流、`--input none` 联动及时间 UI 工作仍在进行；没有新的粘贴 A/B、最终 P1 或 Release 实际结果。

## 历史状态（2026-11-20 20:54 UTC）

[Session service 运行 44812174207](https://github.com/LIghtJUNction/ssmcp/actions/runs/44812174207) 已在固定源码 `72460414a776ec70bdc8b7c124624dff8faabd5e` 上取得 Linux、Windows、macOS 三平台成功。此结论限于该次工作流；后续候选、完整组合验收和发布仍需各自的实际结果。旧 e079 Windows 失败及 89ca Clippy 失败保留原归属，后续成功不证明旧故障的具体原因。

PR #156 已合并；PR #157 当前仍开放，头提交为 `32807fe15e24f828e45246cd46bcd3b88cb2f297`，与上述受测源码不同。新增输入默认值与禁用输入时的交互范围仍待实现验证；时间功能需对照已有源码补齐并验证实际 UI，详见文末本轮补记。本检查点没有新增性能比较结果，也没有 v0.10.21 已发布的证明。

## 交付内容

本轮围绕终端可读性、跨任务等待和可核验的完成流程推进，另同步实际使用的 Project Z 依赖并清理四个已经合并的远端分支。已取得的功能验证分别绑定其实际受测提交；主线中的整合和迁移检查点不因此自动取得相同的验证结论。最终发布结果需要绑定最终候选提交，不能用某个功能分支的测试代替。

### 命令直接可读

PR #125 阶段实现了相邻命令各自显示实际 argv，带有运行状态和 shell 语法颜色。输出默认展示最后三行；长内联 Python / JavaScript 用紧凑标题保持布局可读。详情入口保留脱敏命令、执行目的和已记录输出，支持独立滚动。命令预览超过保留上限时提示 `/command JOB_ID`；对仍保留的本地任务，该入口可读取完整的脱敏 argv。任务不再保留时显示明确提示。查看或展开历史记录不会重新执行命令。

实现和证据见 [PR #125](https://github.com/LIghtJUNction/ssmcp/pull/125) 与 [固定提交上的验证报告](https://github.com/LIghtJUNction/ssmcp/blob/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/readable-commands-20261120/validation.json)。该阶段的产品源码为 `6f251674269875455cca148299f9ddcf97a8cb5c`；PR #125 内后续提交补充测试及证据，没有改变这份受测产品实现。其后统一 Tools 组的整合另见下文，不沿用本阶段结果作为最终 UI 验收。

### `/send` 引用补全

消息正文支持 `@` 文件和 `$` 技能补全，保留 Unicode 与缩进。当引用候选已加载且有选中项时，Enter 或 Tab 接受候选而不发送；接受候选并收起列表后，再次 Enter 执行显式发送。加载期间保留草稿。找不到引用时仍发送原正文，并附带不可用引用警告。接收者和群组路由保持各自的解析边界。

实现见 [PR #116](https://github.com/LIghtJUNction/ssmcp/pull/116)，导航夹具修复见 [PR #118](https://github.com/LIghtJUNction/ssmcp/pull/118)。[归档证据](https://github.com/LIghtJUNction/ssmcp/tree/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/send-reference-completion-20261120)明确区分产品源码、测试脚本及发布候选。

### 等待与人工结束批准

公共 `await` 和 JavaScript `waitFor` 统一观察六类目标：执行任务、relay 任务、协作成员状态、自己的协作收件、用户输入和已有结束审批请求。普通目标可按任一满足或全部满足聚合；用户输入与结束批准保持单独事务。单轮观察的 `timeout_ms` 预算最多 25 秒；在原身份、客户端连接和等待记录仍有效时，可用同一服务器签发的 wait ID 恢复。这不是整个 HTTP 请求耗时的保证。取消只取消观察，不终止被观察的任务，也不确认消息或批准结束。

结束许可必须来自真人明确选择 Yes，并且匹配当前请求、成员与线程；agent 的 Yes、普通 ready / done、超时都不能替代。许可只能消费一次。输入仅在最终外层工具响应的同步提交边界消费，交付以外层 `user_wait.status = received` 及匹配的 `user_messages` 为准；这不表示远端已读。取消先于该提交时不消费；最终交付降为 `timed_out` 或 `busy` 时，仍可恢复同一 wait ID。

实现见 [PR #130](https://github.com/LIghtJUNction/ssmcp/pull/130)，[验证记录](https://github.com/LIghtJUNction/ssmcp/blob/0128c356746b6a29b2cee811c3374c2f891c581f/docs/validation/await-finish-20261120.md)绑定实际受测源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92`。

这个约束作用于经过 SSMCP 的完成接口。宿主若完全停止调用 SSMCP，SSMCP 无法拦截宿主自己的外部 final。公共 `await` 和 `waitFor` 均需要更新后的 relay。公共 `await` 不占 JavaScript 执行槽，`code_mode` 内的 `waitFor` 仍占用所在执行槽；原生被动 finish 观察需要支持相应操作的客户端。

## 命名的本地与远程连接目标

[PR #153](https://github.com/LIghtJUNction/ssmcp/pull/153) 已把命名 context 合入固定主线提交 `b0a07b047b1532688f06b9df8423e9bfc044c405`。context 把连接目标保存成有名字的配置：建立新连接时先确定目标，再交给已有的本地或 HTTPS 传输。用户可以统一选择入口，凭据仍按服务器 origin 隔离，已保存会话仍保存具体目标。

已有命令包括 `context create NAME --local`、`context create NAME --server URL`、`context list`、`context show`、`context inspect [NAME]`、`context use NAME`、`context use --default` 和 `context rm NAME`；list、show、inspect 支持 JSON 输出。本地连接仍限 Linux。新连接的 context 选择按 `--context`、`SSMCP_CONTEXT`、已保存选择、原服务器默认值的顺序解析。显式 `--server`、`local` 或目标地址可覆盖环境和保存的默认选择；与显式 `--context` 同时出现会报错。不存在的显式名称也会报错，不自动切换到其他目标。

这些是[固定源码](https://github.com/LIghtJUNction/ssmcp/blob/b0a07b047b1532688f06b9df8423e9bfc044c405/src/client/connection_context.rs)与[连接入口](https://github.com/LIghtJUNction/ssmcp/blob/b0a07b047b1532688f06b9df8423e9bfc044c405/src/client.rs)的实现事实。合入本身不表示后续整合候选已通过验收；CLI、已保存会话、前台重载和账户命令的最终运行结果还需绑定共同候选。

## 依赖维护与迁移状态

`zharness` 从 0.5.1 升到 0.6.0，固定源码 `db4206d3b74e37d57ff052948962f55514081d4b`，带入 `zcall 0.4.0` 和 `zguard 0.1.0`。PR #120 对实际使用的 12 个 Project Z 包按仓库身份完成核对，并在该次更新中同步精确版本、gitlink 和 Cargo.lock；核对结论以记录中的查询时间及受测提交为准。每轮迭代及发布前重新核对最新未撤回稳定版的规则已经写入[依赖维护文档](https://github.com/LIghtJUNction/ssmcp/blob/0128c356746b6a29b2cee811c3374c2f891c581f/docs/dependencies.md)。

在 PR #120 对应的历史依赖图中，`zcall 0.3.0` 仍由 `ssmcp → zuse-computer 0.2.2 → zmcpio 0.4.0 → zcall =0.3.0` 保留。这个例外来自已发布上游的精确约束；要消除它，需先有兼容的 zmcpio 和 zuse 正式版本，再由 SSMCP 更新消费者。新的 zharness 调用链已经使用 0.4.0。详细版本、摘要和消费者检查见 [PR #120](https://github.com/LIghtJUNction/ssmcp/pull/120)。

后续集成开始区分 Git 开发依赖图与 registry 发布依赖图：开发源码使用固定 Git 提交，发布流程另核对规范化 crate 的来源、文件摘要和实际 registry 图。最终锁文件、真实构建与发行结果仍需绑定同一候选，不能沿用 PR #120 的历史结论作为这轮迁移通过的证明。

另一项独立静态复核绑定 native 源码 `5d087d1f7363fe5741f6080168037623362280e7`：扫描 535 个 Rust 文件，逐项对照 49 处 SHA-2 输出迁移，44 个相关文件读取前后摘要一致。46 处使用统一的小写逐字节十六进制编码，3 处集成测试采用等价表示；没有发现摘要容器 `LowerHex` 遗留或 ID 前缀、输入顺序、前导零、截断长度变化。Base64 签名、SSH、PKCE 与原始字节消费者保持原路径。49 是审阅的迁移点数，不是通过测试数；该结果仍须由最终源码上的真实编译和运行补全，见[静态审阅记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/sha2-lowerhex-static-review.json)。

## 粘贴延迟修复（正在验证）

用户随后明确要求继续改进，并指出粘贴速度过慢。新增修复已由实现与独立审查两方冻结，文本补丁 SHA256 为 `6733bef7448dab15eea316061c78ca78f8e69111bd0dd0df64fc6a918200b6bc`，共 45,899 字节、四个文件。原始文本差异基于 `8148410bc51e1ae61369fae06d0898148521a983`；集成后的候选提交及实际二进制还需由唯一构建负责人绑定。该基点不是最终验收源码。

修复减少两类重复工作：对带终端粘贴标记的连续普通字节成批处理；对普通输入按批次更新补全，避免每个字符都扫描整个草稿。ESC、导航、提交、补全、PNG 内嵌标记及本地人工输入撤销控制仍保留原顺序。保存中的提示现说明可以继续编辑，变更后的草稿不会被替换。

| 检查 | 当前状态与范围 |
| --- | --- |
| 产品代码与回归用例 | 静态审查无阻断；三个 Rust 文件的格式检查通过 |
| Rust 回归 | 已编写 7 项，尚未在集成候选上执行 |
| 真实终端性能脚本 | 首次基线未完成；分页修正与配套 Ctrl+V 后继脚本已冻结，新的完整比较待实际运行 |
| 真实用户内容 | 未纳入公开补丁或性能样本；测试全部使用合成输入 |
| 提升幅度 | 尚无可比较的实测结果，不报告加速比 |

六个场景覆盖 2,738 字节多行 UTF-8、64 KiB、恰好 4 MiB、超限拒绝、16 KiB 普通输入，以及工具持续输出期间的粘贴。脚本分别记录最后一次 PTY 写入与首个可见、真正可用的引用；写入 PTY 完成不代表客户端已读完或保存完成。比较基线和候选必须使用相同的九个测试辅助文件，并由外层回执证明源码、锁文件和二进制身份。驱动器中的 source revision 字段本身只是调用方声明。

另一个已观察到的因素是磁盘提交延迟。同一设备上三次合成 2,738 字节写入，写入本身分别约为 0.027、0.465、0.057 毫秒，而 fsync 分别约为 10,842.582、290.753、1,088.451 毫秒。这只是三个环境诊断样本，不能作为产品基线或百分位统计；后续压力读数已经降低，也不能据此把全部 I/O 归因于某一个任务。[环境记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/paste-environment-io-observation.json) SHA256 为 `2ba20189795d410f6a9d905f2b5260a5f779c219cc595fd9ba0cb0f0df03ce20`。

控制流复核确认：保存运行在异步后台，界面以非阻塞方式检查结果，所以可以继续输入、删除和移动光标；普通 Enter、`/send` 和问题答案提交仍等待保存任务完成。等待期间按下的 Enter 不会自动重放。用户改动草稿后，保存完成会保留新草稿并提示已保存引用，不能把“可以继续编辑”理解为“引用一定自动加入变更后的草稿”。本次补丁保留 fsync、私有权限、八行自动引用策略及 InputStamp 保护，不能单凭 CPU 路径优化宣称持久化长尾已经消失。

先前一次只读剪贴板诊断的调用虽然超时，后来按原任务 ID 找回了完整结果：原生任务退出 0，总时长 51 毫秒，其中列出剪贴板格式用时 1.15 毫秒。该探针没有读取剪贴板内容；当前格式样本也不是最初的文本粘贴，因此不能用它反推原始故障原因，或将调用超时算成剪贴板读取耗时。详见[回收的诊断回执](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/clipboard-format-probe-recovered.json)。


### 测量入口与基线构建

终端粘贴和 Ctrl+V 需要分别测量。带终端粘贴标记的字节经过此次批处理路径；Ctrl+V 则由剪贴板适配器取得完整文本，直接进入异步保存。两条路径都会执行文件持久化，不能把终端字节处理的改善直接当作 Ctrl+V 的加速。现有剪贴板功能测试没有覆盖八行以上纯文本的性能样本，已冻结独立的 2,738 字节 Ctrl+V 用例（脚本 SHA256 `434348a28727c717179ff6e49f322e8692a8e88a4107fcbcc12fc53cef298437`），实际执行仍待完成：真实客户端只接收一个触发字节，私有模拟 wl-paste 提供合成内容。它检验实际客户端适配器与保存路径，不代表真实系统剪贴板性能。

实测前还发现临时目录位置会改变测量对象：默认 /tmp 是 tmpfs，用户工作区位于 /home 的 btrfs。基线与候选必须把测试目录放到与工作区相同的文件系统，并记录实际位置、st_dev 和文件系统类型；内存盘结果不足以证明磁盘持久化延迟已经改善。btrfs 子卷的 st_dev 与 mountinfo 设备字段可能不同，文件系统类型按路径所属挂载点判断。[目录与文件系统记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/paste-benchmark-filesystem-observation.json)保存原始观察及范围。

修复前基线 `b156254b45c015a3994b8be1475b423167ac980f` 的 library fixture 编译实际失败，Cargo 返回 101：时间模块不能访问仅限上级模块的 invalidate，以及测试给需要 Path 的参数传入了字符串。完整输出只有这两个编译错误，未生成绑定该源码的根包可执行产物，后续 release bins 和 decoder 构建尚未进行。因此 b156 不能作为性能比较基线。[独立失败核验](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/baseline-b156-failure-review-20261120.json)保留源码、依赖、日志与精确时间；失败记录不会改写为成功。

唯一集成负责人随后形成 `c53c5ea681cb0840f1c41c9e8d31489abac4fd37`：仅将 invalidate 的可见性改为 crate 内可用，并在原有 TTY 测试中显式构造 Path，两处各一行。Cargo.lock 摘要保持 `6c76ff5b75b161aeb691ebc6be129ba719aae82b09c8c0d50ae42f6d817c604d`。该基线已在 2026-11-20 19:09:09 UTC 完成库测试夹具、三个 release 程序及 VT decoder 的构建；独立审查核对了 34 份声明文件，并逐一核对五个保留程序与编译输出的字节数、摘要及权限。原构建回执为 39,128 字节、SHA256 `16da6d1611355d18721582451326195629a87060f6b7e9f3ccf16bdf5cd9daaa`。这次工作只完成构建，没有执行 Rust 测试、性能基准或发布验收。[独立构建审查](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/baseline-c53-build-review-20261120.json)保留当时实际读取的证据和适用范围。

随后进入主线的 [PR #152](https://github.com/LIghtJUNction/ssmcp/pull/152) 对应固定提交 `cbe3d0ef1f7fbe1a26ebe367edc646cd8ef429c3`。完整树比较确认，它与 c53 只相差一份新增说明文档；产品源码、Cargo 文件、脚本、测试、工作流和四个 gitlink 均相同。提交和完整树身份仍然不同，c53 的构建回执不能改标为 cbe 的验收。[完整树对照记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/main-cbe-vs-baseline-c53-identity-20261120.json)也明确限定为这两个固定提交。

本检查点另已通过 GitHub 实读主线固定提交 `b0a07b047b1532688f06b9df8423e9bfc044c405`：[PR #153](https://github.com/LIghtJUNction/ssmcp/pull/153) 已合入命名的 local / HTTPS context；其父提交 `4237eea2cab8253b8dff84d81861ba1e1adf4246` 更新了 zuse 的固定源码及锁文件。旧 c53 / 6c76 锁文件身份因此不能代表这份新主线。后续比较仍由唯一构建负责人冻结共同基线，包含最终 context、依赖和必要修复；源码合入本身不表示最终运行验收通过。

首次性能基线服务在 19:31:00–19:31:15 UTC 运行后退出 1。构建负责人报告，前四个场景通过，第五个 ordinary-16k 场景在读取完整草稿投影时触及 8 KiB 上限；报告还记录 57 项检查及输入未变。这里的场景统计来自负责人的转述：独立审查尚未取得其原始 16,024 字节报告（所报 SHA256 `76e4a54d52164c89a57be0a3eba10c11138e3c20d8f730113e4ec047d5476309`）。因此该次运行保留为未完成的基线，不给出加速比；修正分页读取后，基线与候选必须使用同一套实际驱动和辅助文件重新比较。

### 分页驱动修正与 Ctrl+V 后继脚本

针对首次运行遇到的读取上限，新驱动 SHA256 `1300d60939de4df574eb4c0d989bca1c2906a29873a8cd1e68cc7c1b3ae364c8` 改为读取所需标量字段，并以每页 1,024 个 Unicode 字符重建完整草稿。页间总长度、实际字符数与连续 offset 都会检查；完整 UTF-8 正文及最终字节光标的原断言继续保留。独立 AST 对照确认，新驱动仅新增这个投影读取方法，原计时、样例和传输方法不变。c53 与 b0a07 的真实 helper 均经 `observe → response → self.rpc` 调用，仍进入计时期间禁止 RPC 的原保护。

原 Ctrl+V 补充脚本 `434348…` 严格固定旧驱动 `07af…`，因此不能直接传入新驱动。后继脚本 SHA256 为 `db2db3cfd14c1e6be0b36b4605a1c6f4790da4cab5a3515ad352739912ae238f`，只替换一行严格驱动摘要；原启动、身份、文件系统、计时及正确性检查保持原样。旧版本原件已保留，历史拒绝提示仍含旧驱动简称，实际检查以新摘要为准。

[完整交接包](https://drive.google.com/file/d/1LeyqEdcbKnltPifWZWjfS2HhkBDdxASC/view)包含新旧脚本、两份差异与清单，34,556 字节、10 个条目，SHA256 为 `d624a011202f488435de8fcfe68bffbb1aae651ad1985cb6bd619c7f32240d32`。实际下载云盘副本后确认字节完全相同。唯一构建负责人已接收这一组合；目前仍是准备完成，尚无新的实际性能比较。基线和候选须统一使用新驱动、后继 Ctrl+V 脚本与完整辅助文件集合，并重新绑定实际源码、锁文件和五个程序。[窄复核记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/paste-driver-r2-review-20261120.json)保留适用范围。

## 验证证据

| 范围 | 实际结果 | 对应源码或范围 |
| --- | --- | --- |
| `/send` 定向回归 | 32 项通过 | 功能分支的补全、技能、监听和群组检查 |
| `/send` 实际 router / WebSocket / PTY | 114 项通过，48 / 120 列 | 产品源码 `cda2aeb6a61d9950b48ee213cca8e70714cf7e29`；导航脚本在后续提交固定 |
| 命令事件及整帧回归 | 91 项通过 | 产品源码 `6f251674269875455cca148299f9ddcf97a8cb5c` |
| 既有客户端 UI PTY | 56 项通过 | 同一命令显示源码 |
| 实际 native exec / router / PTY | 71 项通过，48 / 120 列 | 同一命令显示源码；开发机完整链路 |
| Canvas 完整 UI PTY | 69 项通过，110 / 60 列 | 产品源码 `4c6178e`，夹具 `38d41d4`；宽窄截图已经复核 |
| await 与人工完成 | 106 项通过，0 项忽略 | 源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92` |
| 账号退出、relay 与协议回归 | 20 项通过，0 项失败或忽略 | 受测源码 `d417a27a47de332dc4a5e0bf4be0c74bf7f1c591` |
| 完整账号 relay 流程 | 47 项通过，夹具退出码 0 | 同一受测源码；合成 OAuth 账号与 loopback WebSocket |
| 完整 Questions | 43 项通过 | 产品 `c7086f9`、夹具 `8e76841`；精确成员等待 |
| 历史选择器 Rust 回归 | 4 项通过，0 项失败 | 库测试源码 `4221d55`，真实字符与提交路径 |
| 完整 InputHistory | 150 项通过 | 产品二进制 `ae8ede2`，PTY 及库服务 `4221d55`；归档恢复与续接 |
| Windows smoke 的有界诊断 | 53 项 PowerShell 检查通过，非 Windows 保护按约定失败退出 | Linux PowerShell 7.6.10；不代表本机执行过 Windows ARM 安装 |
| Project Z 消费者 | 锁定解析、真实动态库契约 1 项、发布规则 91 项通过 | PR #120 的依赖更新，消费者受测源码 `2498561f39420b4c1994dd3380d48d070fd9b467` |
| 严格 Clippy | 命令显示、await、账号修复三份受测源码分别通过 | `cargo clippy --workspace --locked --all-targets -- -D warnings` |

这些计数来自不同范围，不相加冒充一套完整发布验收。Cloud 的完整 native 路由验证受宿主网络命名空间能力限制；所需完整链路已在开发机运行，没有移除隔离要求。归档截图使用合成测试内容，并经过实际视觉检查。

### 首轮整合发布预检

[Run 44754296839](https://github.com/LIghtJUNction/ssmcp/actions/runs/44754296839) 绑定 `4c6178eb811049ce1fce4b6d16803e0e4f7b410d`，结论为失败。六个平台构建、四项 macOS / Windows 客户端检查和两项 Linux 安装运行验证通过；两个 Linux 检查暴露了退出登录重连竞态与日志界面旧测试合同。后续 finalize / 发布步骤跳过。该结果保留为失败证据，不能用成功的构建部分宣称候选通过。

Canvas 修复已通过 [PR #140](https://github.com/LIghtJUNction/ssmcp/pull/140) 合入主线。测试改为验证新的三行命令预览与详情入口，从实际显示的相邻栏读取动态计数，并精确比较 Tasks 的三项有序 argv。保留原始第 0 项失败和第一阶段通过 48 项后遇到旧 Tasks 文案的失败；最终完整 69 项在 14.609 秒内通过，前后二进制摘要一致。[机器报告](https://github.com/LIghtJUNction/ssmcp/tree/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/canvas-command-contract-20261120)绑定精确产品源码和测试脚本；未改产品代码，也没有削弱输出、滚动、草稿、登录拒绝或终端恢复的检查。

账号修复通过 [PR #142](https://github.com/LIghtJUNction/ssmcp/pull/142) 合入：写入先报错时，客户端先以同一个两秒预算读取剩余结束消息，防止尚在队列中的授权失效通知被直接丢弃；收尾不会执行排队工作。只有协议中已知的永久拒绝可以替换原始错误，普通 EOF、读取错误、普通拒绝和超时继续保留原错误，主动停止和重载保持各自路径。

修复后的真实开发机批次通过 20 项 Rust 回归和完整 47 项账号检查，后者耗时 13.74 秒。被撤销客户端按约定以 `1` 退出，连接次数为 **2 → 2**；重新授权只发起一次，旧凭据已删除。严格 Clippy 退出码为 0，耗时 191.969 秒。没有放宽原断言；新增测试固定消息交错顺序，原夹具只补实际数值诊断。[机器回执与边界说明](https://github.com/LIghtJUNction/ssmcp/tree/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/account-logout-revocation-20261120)区分产品提交 `b4d648b`、受测提交 `d417a27` 和只添加证据的提交 `053fc99`。单独下载原制品的本机基线未执行，原始失败证据来自真实 CI。

### 合并修复后的预检

该次冻结候选为 [`c7086f9545602f45399e7a237995a48440e7d8aa`](https://github.com/LIghtJUNction/ssmcp/commit/c7086f9545602f45399e7a237995a48440e7d8aa)，包含 #140、#142 和完整发布说明 #138。独立核对确认各 PR 的最终文件摘要、版本、四个固定子模块和发布策略一致。在针对该候选于 2026-11-20 23:21:24–23:21:42（Asia/Taipei）进行的官方 registry 核对中，12 个 Project Z 包当时的最新未撤回稳定版与前次记录一致。

[预检 44762565095](https://github.com/LIghtJUNction/ssmcp/actions/runs/44762565095) 使用空 tag、`build_only=true`、`deploy_server=false`，最终为失败，18 个任务中 10 个成功、3 个失败、5 个跳过。两个 Linux 架构的账号退出检查、Canvas 69 项及 Composer 87 项均通过；新的 Linux 阻塞在随后的问题交互夹具，它在等待成员列表同步之前查找成员。Windows ARM 浏览器烟雾检查另抛出信息不足的 `MethodInvocationException`；安装脚本、Node 文件与前一成功记录相同，目前不能据此认定具体安装故障。安装运行验证与 finalize 因前置失败跳过，未发布标签、包或资产。

[PR #147](https://github.com/LIghtJUNction/ssmcp/pull/147) 已补足 Windows smoke 的有界诊断：异常类型、数值错误码以及白名单函数位置，不复制原始消息、路径、参数或 stderr。Linux PowerShell 7.6.10 上的 53 项检查通过，真实 smoke 的非 Windows 保护仍以 `1` 退出且 `ok=false`。这项改动保留了安装逻辑、成功条件、期限与清理规则，不能单凭这些本地结果宣称 Windows ARM 的原始安装故障已修复。[三份正式报告对照及验证回执](https://github.com/LIghtJUNction/ssmcp/tree/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/windows-browser-diagnostics-20261120)记录相同的 Node、helper 与 installer 摘要，以及本地验证的适用范围。

精确成员等待的修复首先在产品源码 `c7086f9` 与夹具 `8e76841` 上验证：原 Questions 在 17 项后复现 `StopIteration`，修复后完整通过 43 项；报告内运行时间为 12.885 秒，外层进程为 12.993 秒。同一批次的 InputHistory 在 140 项后发现真实产品焦点缺陷：在 History 页输入 `/archived` 后，列表刷新但焦点仍在编辑框，Home 与 Enter 无法按文档恢复归档。后续 [PR #146](https://github.com/LIghtJUNction/ssmcp/pull/146) 显式把成功打开的历史选择器切到列表焦点，保留恢复后返回输入的行为。产品源码 `ae8ede2a53b802899d57edcfccac933a9909eea3`、库测试与 PTY 夹具 `4221d5594b8ee0bd1fb9ea1d9783d0bb1bebd3f7` 已完成 4 项归档回归与完整 150 项 InputHistory 检查；外层 InputHistory 进程耗时 32.629 秒，产品二进制哈希在测试编译及 PTY 前后保持一致。缺少测试二进制环境变量造成的第 0 项失败、一次新增测试的私有字段访问编译错误，均单独记录，不作为产品运行结果。

### 更大范围整合的候选

[PR #133](https://github.com/LIghtJUNction/ssmcp/pull/133) 随后把统一 Tools 组、保留折叠与滚动、会话目标、启动更新提示、被动等待总结和真人问题的原历史归属等改动合入 `407084c4fcc582f5fef01b772de838f04e2d5484`。这份源码已包含精确成员等待，但尚不包含 PR #146 的历史焦点修复。前述旧功能分支上的结果不能改标为这份整合源码的验证。

该提交的[原生门禁公开记录](https://github.com/LIghtJUNction/ssmcp/pull/133#issuecomment-7044314246)为 1880 项通过、5 项失败、22 项忽略、4 项筛除，阻塞涉及诊断与分组可见性、滚动、阅读导航和输入帧预算断言。[固定提交中的原始失败摘录与摘要](https://github.com/LIghtJUNction/ssmcp/blob/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/ssmcp-zcalld-iteration-20261120/gate8-failed407-root-review.json)保留失败来源；独立整合修复继续处理。[正式预检 44769140252](https://github.com/LIghtJUNction/ssmcp/actions/runs/44769140252)也已失败。最终候选还需包含这些窄修复、历史焦点修复，以及绑定同一源码的组合交互验收与成功发布预检。

本次预检中 [Windows ARM 构建与真实浏览器 smoke](https://github.com/LIghtJUNction/ssmcp/actions/runs/44769140252/job/136402117933)已成功：53 项 bootstrap 检查和 30 项实际安装检查通过，worker 退出 0，真实 smoke 耗时 53.85 秒，清理完整。官方 Node 文件、SHASUMS、helper 与两个 installer 摘要和上一轮失败记录一致。这证明本次安装成功，不能反推旧 `MethodInvocationException` 的具体原因，也没有证据要求修改安装逻辑；同一 run 的 Windows ARM Cargo 回归失败属于另一个检查任务。

历史焦点的产品修复随后通过 `3431054facad9809748567a13b292c8cc7773b67` 合入；[PR #149](https://github.com/LIghtJUNction/ssmcp/pull/149) 再以纯文档改动归档[保留原始来源的验收文本记录](https://github.com/LIghtJUNction/ssmcp/tree/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/questions-history-readiness-20261120)，并补充发布说明。文档合入提交为 `47b9ca9bb83646c8fa8678a095e7dacdd3783572`，没有重新运行或改标旧测试。

后继主线还包含 [PR #151](https://github.com/LIghtJUNction/ssmcp/pull/151) 的七项直接依赖要求及 HTTP 配置封装。该提交是迁移检查点；调用点调整、锁文件解析和实际执行结果仍待收齐。此工作稿不将尚未完成的迁移或 24 套/P1 控制器准备包计作最终验收通过。

清理后的产品检查点为 `8148410bc51e1ae61369fae06d0898148521a983`；后继文档提交 `7ddaeab07be67f25f8277aa37486474710cb1fa3` 修正了外部证据入口，没有修改产品源码。历史清理保留了旧 `55056d3` 检查点的产品源码；既有测试仍按原受测提交和二进制摘要归属，不改标为 `8148410` 的组合验收。后续整合工作正把未发布的窄修复迁移到清理后的分支。

独立静态审查确认，五项失败的组合修复保留了原测试合同：诊断辅助函数确保 Tools 已展开；阅读导航按统一组保留全部原始消息 ID；预算夹具分别覆盖预留额不足与仍有正预算；滚动恢复分别验证固定偏移与跟随尾部。跟随修复只为未恢复的首次展开初始化顶部位置，避免折叠历史恢复后把 `None` 表示的跟随状态改成 `Some(0)`。组合补丁 SHA256 为 `286051be55083391c33ec919b54b29eddc9cf02275a50b98c76a71fe00460780`；这项结论属于补丁审查，仍需确认完整补丁进入最终源码并实际通过回归。

HTTP 封装的独立审查也限定为静态结论：预配置 TLS 只替换 TLS 配置，调用点原有超时、重定向、代理与 HTTPS-only 策略必须随迁移保留。自定义账户证书配置仍需作为完整配置覆盖；私有 CA 的替换测试不能替代真实 SPKI 固定路径的验证。最终以同一候选源码上的调用点、锁文件和执行回执为准。

### 新版 25 套验收控制器的准备状态

独立审查核对了冻结 runner `2e9179768f8537f4a23323ed262f6460b8f57cc459130da3e35ba6400cafb399`、contracts `611a58b0680a09c4f57dadfaca3dcbf20c3d55e3770ec1d140a848ffb9a41022` 和归档的全部 28 个条目。25 套列表包含新增 time_wait；原有延迟检查的 250/500 ms 门槛未改。时间场景要求五种场景、两种宽度的十个完整帧，并核对真正的 composer 标题行。动态等待检查读取自有终端输出，观察期间没有额外按键或 UI/native RPC。

旧版本兼容场景继续使用原来构建的 daemon；清理后的 Git tree/blob 只提供经过摘要核对的相同 runner 字节，没有把历史程序描述为重新构建的产物。现有 47 项离线合同测试的输入和日志已核对，但这些是验收工具测试，不能代替产品执行。

[独立控制器审查](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/independent-review-final25-2e917-v1.json)明确保留剩余门禁：上述原冻结包未包含后续 Gate10 接线，旧 r3 provenance 也不能认证 Gate10。验收负责人随后已提供新的接线准备包；实际通过的 gate、外层 launcher 与最终源码/依赖/二进制身份仍需由真实执行接通。25 套产品验收、单独的 P1 集成与粘贴性能样本也尚不能写成已通过。

### 恢复后的 Session service 实际失败

[Session service 运行 44806975640](https://github.com/LIghtJUNction/ssmcp/actions/runs/44806975640) 绑定 `e0793d778ee1900c9087befeadb38a6a31f39386`，事件为 pull_request、attempt 1，于 2026-11-20 20:04 UTC 结束为失败。三个 job 的实际状态分别为：Linux 在 workspace 单元测试步骤失败，Windows 在协议测试步骤失败，macOS 的该工作流全部成功。Linux 与 Windows 的后续构建和 native lifecycle 步骤均跳过；不能把 macOS 单个 job 的成功写成整份候选通过。

Windows 的完整原日志及同一固定提交中的源码已独立读取。此前 session 测试为 44 通过、0 失败；协议测试为 6 通过、4 失败，耗时 15.04 秒。三个失败取得的是 `SSH authentication timed out`，对应注册首帧等待超过测试中的 10 秒期限；另一个 panic 来自读取 fixture 尚未生成的 hellos.jsonl。该测试先读记录文件，再检查注册返回值，因此缺失文件遮蔽了注册本身的结果。预期的 GitHub 拒绝文案来自模拟对端，日志没有证明真实 GitHub 服务拒绝。

四个失败在同批输出中出现，后续仍有模拟对端用例通过。这支持继续检查并行 fixture 的启动和首帧响应，但没有证明 Python 冷启动、防护扫描或 runner 负载中的某一个就是原因，也不足以只凭猜测放宽超时。[Windows job](https://github.com/LIghtJUNction/ssmcp/actions/runs/44806975640/job/136533435901) 的解码日志文本已原样保全，包括 UTF-8 BOM 与 CRLF：102,472 字节，SHA256 `ae93e5d1d00c3ba8b3d09f8cd4ba0a7756e75411c96e4ba7af0bc9f63d3986ff`。[云盘副本](https://drive.google.com/file/d/1stuctZu2CTVcK_x7FdI8QT5hg0oGvHtR/view)已实际下载核对。完整失败名称和源码关联见[独立失败记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/windows-e079-failure-review-20261120.json)。

修复继续由唯一源码负责人收敛；本次日志审查没有启动新 CI、执行产品测试或修改生产程序。上述失败仍归属于 e079，不改标为包含后续 context、依赖与电脑控制修复的新候选结果。

## 历史清理与证据位置

第一轮协调清理已完成，详见[历史清理记录](https://github.com/LIghtJUNction/ssmcp/blob/0128c356746b6a29b2cee811c3374c2f891c581f/docs/reports/history-cleanup-20261121.json)。独立比较清理前后的完整 endpoint tree，确认产品源码、Cargo.toml、Cargo.lock、发布与 CI 工作流、Makefile 和固定子模块内容未变；清理没有改变既有测试结果。后续提交以清理后的历史为基础。

清理移出了 39 张生成截图与 3 个 ZIP。2026-11-21 01:16（Asia/Taipei）已从 Google Drive 下载外部归档，将这 42 个文件逐项重新计算 Git blob 摘要和字节数，与旧 `55056d332fb2fa20e7bf2ee6a8f5610581ac9f2a` 的完整 tree 清单比对，**42 / 42 一致**。同时记录实际下载内容的 SHA256；[逐文件校验记录](https://github.com/LIghtJUNction/ssmcp/blob/4478fd74a0d7f050945957066068cc49da3e6ae9/docs/reports/command-await-delivery-20261120/external-evidence-verification.json)包含完整文件名、来源和摘要。旧来源号仍标识实际受测版本；报告不再依赖旧分支历史提供生成文件。

| 外部归档 | 范围 | SHA256 |
| --- | --- | --- |
| [生成截图](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj) | 39 张历史验证截图 | `901595a39482e38a30c622b115617e298058a5dabd61aa58efd97f15a2689988` |
| [24 套控制器准备包](https://drive.google.com/file/d/1KIRxKZ8lKaBxo77vL5W4rves4nZLQyX7) | 准备好的控制器；不表示已执行 | `f08d35dadc127158b44e296baf72cd47584e7ee808db963843ff82d855b67142` |
| [P1 控制器准备包](https://drive.google.com/file/d/19MHe3COB8lmSpX-28HCuSSVHUWq_VWsk) | 准备好的移交包；不表示已执行 | `f7a2144b34b5c804dbd428b5abc4844b2ca3e67227679b1897ce99eb7ee21eb9` |
| [既有 zcalld 原生证据](https://drive.google.com/file/d/1OlScR20aKm_dZ4yLj-Z4tKYvzjmgMKj7) | 历史 `ef439` 批次；不表示最终整合候选通过 | `bbbdcce805e23c5cc054708c810e7fe2aac30c3db4f4d3c7c8036588d1740109` |

第二轮清理已完成远端分支映射，清理协调者随后报告 29 个标签的带租约原子更新完成，并恢复文档分支写入。主线的兼容证明与规则提交已独立读回为 `4b73e6b339cd1880c6c7327295189fc96d74cb42`，本报告分支以映射后的 `4478fd74a0d7f050945957066068cc49da3e6ae9` 为继续提交的基础。对清理前 `7ddaeab` 与映射后 `0128c356` 两个完整主线端点树的比较显示，只有 91 个生成文件删除，零新增和零内容修改；产品源码、脚本、Cargo 文件、工作流和 gitlink 相同。[端点校验记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/second-cleanup-endpoint-verification.json)记录完整差异。该独立比较只覆盖这两个当前主线端点，不等于逐项重验全部历史路径、媒体内容或 29 个标签。

针对 `4b73e6b` 中五组发布来源兼容映射，独立静态审阅读取了十个完整 Git tree、五个清理后 commit 以及五份 Release 资产元数据。原始 commit 对象摘要、保留条目摘要、证明文件 pin、删除路径清单和内置映射表均吻合；五组只删除文档媒体，没有改变保留的产品条目。此审阅没有下载 manifest 原字节或媒体，也没有执行部署；当前部署 helper 所需的现代 portal 文件在两个较老版本中原本就不存在，因此映射证明不等于这两个版本可由现代 helper 部署。完整范围见[来源兼容静态记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/history-compat-static-review.json)。

随后另一次实际发布兼容检查下载了五份旧版 manifest 原字节，确认清理前后完全相同，并核对五组 tag 映射与十项 Linux 资产元数据。根审查读取了该回执，并将五组源码和 manifest pin 与上述独立静态记录逐项对齐。此阶段没有重新下载二进制归档，也没有部署。[实际发布兼容回执](https://github.com/LIghtJUNction/ssmcp/blob/ccb91955c25969f4779588d4cebb09b72c00f829/docs/reports/post-publication-release-check-20261121.json) SHA256 为 `f5d16fde47598d5ffa790f43ebfaa3b795ce1eed41b0720f6e122da8bec169f4`。

文本报告、清单和测试输入保留在 Git；生成截图、归档和编译产物存于外部归档或 Release 资产。最终发布报告另需填入实际发布记录，不以这些历史材料替代最终候选的门禁。

## 工作目录恢复检查点

2026-11-20 19:32–19:36 UTC，多个位于 GITHUB 目录下的 SSMCP 迭代工作目录先后无法访问。同一客户端上的早先读取曾成功列出 release 工作目录，后续读取则得到 ENOENT；具体原因尚未查明。主 Git 仓库及部分独立保留文件仍在。工作目录恢复、重新构建和原始运行产物恢复是不同事项。

本线程和直接协作审查者从各自保留的完整原文件恢复了 16 份材料，包括粘贴补丁、原驱动、Ctrl+V 补充脚本、独立审查、旧工作稿与发布核验工具。每份恢复文件都与此前记录的完整 SHA256 一致。归档共 18 个条目，另外两个是恢复清单和范围说明；它没有重新生成缺失的原始构建回执，也没有重建原测速结果。归档大小为 158,998 字节，SHA256 为 `d23a92ff837f26bff83eaf7094c92f412c218105308ffeff6ce0056bf1fb89da`。

恢复包已保存至 [Google Drive](https://drive.google.com/file/d/13Q7GXA2gBBpFnThVfL-auILA4FrL3zI0/view) 和 [文件副本](https://chatgpt.com/library?file_id=libfile_fde3_f-50324c9ad36c8191bd578d1b1139d078_e58c93d727108191bbc40d6c0d09ec81&account_id=personal)。19:49 UTC 实际下载云盘副本后再次计算摘要，字节数、SHA256 与 18 个归档条目均一致。[恢复检查点](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/workspace-recovery-20261120.json)列出每份材料及缺失项。

截至此检查点，本线程尚未恢复原 c53 构建回执、完整源码和依赖输入清单、原先保留的五个二进制，以及首次性能运行的原报告与 VT 帧。独立审查 JSON 记录的是丢失前确实做过的核验，不能充当这些原文件的替代品。唯一构建负责人负责在新的工作目录恢复 Git 源码，并为后续实际构建和运行生成新的回执；共享缓存里的同名程序不能直接继承旧身份。

### 发布核验工具的适用范围

已恢复的 a238 发布核验工具曾通过 8 项离线测试，覆盖 23 个篡改负例；这些只验证核验逻辑，不表示真实发行已经通过。对 c53 的 [schema 与工作流静态复核](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/source-schema-recheck-c53.json)确认了当时 verified_crate 契约、19 个 job 与 16 个资产集合的兼容性。最终候选如继续修改 release.py 或工作流，仍需按实际最终内容核对；正式 tag、run、资产、官方 crate checksum 与源码身份都要等待真实发布后验证。[历史离线回执](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/verified-crate-contract-validation.json)保留原样。

## 分支清理

以下四个远端分支已按已合并 PR 的精确头提交核对并删除，删除前确认没有额外提交和活动工作树占用：

| 分支 | 对应 PR | 删除前的头提交 |
| --- | --- | --- |
| `feat/007-recovery-upgrade-20261119` | [#98](https://github.com/LIghtJUNction/ssmcp/pull/98) | `230993bb22768cade567e0a5fbfacbef45a68621` |
| `feat/wait-user-primitive-20261119` | [#97](https://github.com/LIghtJUNction/ssmcp/pull/97) | `4f35eebc8f7fb18d25b2fde863b454ef3deb4c70` |
| `feat/user-wait-ui-20261119` | [#95](https://github.com/LIghtJUNction/ssmcp/pull/95) | `979fa88dc014653282884902635e4707d5c9900e` |
| `feat/command-panels-20261119` | [#68](https://github.com/LIghtJUNction/ssmcp/pull/68) | `e57983ab88b907669924389b580aae2f6d95aeb6` |

## 实际发布与运行边界

待补充：最终提交、tag、正式发布 run、Release 的 16 项资产及摘要、crates.io 版本与 checksum、报告位置。

本线程未执行生产 relay 部署或活动客户端版本切换。发布包与生产进程是否已经加载该版本是两个独立事实，交付记录应分别报告。

## 本轮补记（2026-11-20 20:54 UTC）

### 保留失败，限定后续成功的范围

[运行 44811035087](https://github.com/LIghtJUNction/ssmcp/actions/runs/44811035087) 的源码为 `89caea1a0e84b56405712bd8fc0cdf4464a4bbc2`。[Linux 原日志](https://github.com/LIghtJUNction/ssmcp/actions/runs/44811035087/job/136547317847) 唯一具体诊断为 `clippy::ptr_arg`：`src/client/connection_context.rs:175:36` 的 `normalize_args` 使用 `&mut Vec<OsString>`，Clippy 建议改成 `&mut [OsString]`；`-D warnings` 将其升为错误，步骤以 101 退出。lib 与 lib test 报告的是同一项诊断，没有产品测试结果可记为通过。

后续 [运行 44812174207](https://github.com/LIghtJUNction/ssmcp/actions/runs/44812174207) 为 pull_request、attempt 1，固定 head 为 `72460414a776ec70bdc8b7c124624dff8faabd5e`，于 20:37:56 UTC 完成且结论为 success。已实读 run 和 jobs 元数据；三平台的 fmt、严格 Clippy、程序构建及 native session lifecycle 步骤均成功，测试范围如下。

| 平台与 job | 实际成功的测试步骤 |
| --- | --- |
| [Linux / 136551249545](https://github.com/LIghtJUNction/ssmcp/actions/runs/44812174207/job/136551249545) | 工作区单元测试；两个独立 session / protocol 筛选步骤跳过 |
| [Windows / 136551250113](https://github.com/LIghtJUNction/ssmcp/actions/runs/44812174207/job/136551250113) | session 与 client::protocol_tests |
| [macOS / 136551250330](https://github.com/LIghtJUNction/ssmcp/actions/runs/44812174207/job/136551250330) | session 与 client::protocol_tests |

这次成功不改写前文 e079 的三个超时和一次缺失记录文件 panic，也不据此认定旧故障由 Python 冷启动、系统负载或其他特定因素引起。尚未读取本次全部原始测试日志，本检查点不新增测试数量或性能数值。

### 合并状态与新增目标

[PR #156](https://github.com/LIghtJUNction/ssmcp/pull/156) 于 20:35:07 UTC 合并到 main，合并提交为 `90c497007f6d6f16c3dd2da6a4599c48787e5ee1`。[PR #157](https://github.com/LIghtJUNction/ssmcp/pull/157) 实读状态仍为 open，最新头提交为 `32807fe15e24f828e45246cd46bcd3b88cb2f297`。该提交的两个父提交为 `72460414` 与 `90c49700`，tree 为 `65c8239db42c0d49e48b343af901d793d0900351`。本检查点未为该头提交取得新的验收结果，不能将 724 的 CI 成功改标给它。

新增需求按目标记录，尚不宣称已实现或验证：

- `--input` 默认使用 `terminal`；`--agent` 默认使用 `next-caller`，内部 wire 值为 `next`。
- 显式 `--input none` 禁用聊天框。关于相关提示 hook 和 agent 面向用户回答工具的禁用，本轮暂按同一 `--input none` 场景解释；此范围及实际行为仍待实现验证。
- 时间功能需补齐用户可见的计时、倒计时和时间间隔行为。固定 `90c497` 的 [`runtime/time.rs`](https://github.com/LIghtJUNction/ssmcp/blob/90c497007f6d6f16c3dd2da6a4599c48787e5ee1/src/runtime/time.rs) 已有 `time.now` / `time.wait` 与基于单调时钟的倒计时；[`conversation/timing.rs`](https://github.com/LIghtJUNction/ssmcp/blob/90c497007f6d6f16c3dd2da6a4599c48787e5ee1/src/client/terminal/conversation/timing.rs) 也已有显示入口。接下来核对实际运行版本、能力协商与界面绘制，源码存在不能替代运行验收。

共同最终候选的组合验收、粘贴与 Ctrl+V 性能比较、正式发布核验仍未完成；本记录不构成 v0.10.21 已发布的证明。

## 合流与定向验证补记（2026-11-20 21:26 UTC）

### 已合入的源码

三个 PR 的合并状态已通过 GitHub 实读。下表时间均为 2026-11-20 UTC。

| PR 与范围 | 合并时间 | 固定合并提交 |
| --- | --- | --- |
| [#158](https://github.com/LIghtJUNction/ssmcp/pull/158)：线程聊天 IO、真实结果等待与身份历史；[源提交 daf7d93c](https://github.com/LIghtJUNction/ssmcp/commit/daf7d93c3dd86a70299a25502dc9308bed9b4647) | 21:01:44 | [2dbad2cb](https://github.com/LIghtJUNction/ssmcp/commit/2dbad2cbf5b802ad8b8aa771f826a54ee90154d3) |
| [#157](https://github.com/LIghtJUNction/ssmcp/pull/157)：session、context 与电脑控制整合 | 21:02:00 | [e7672258](https://github.com/LIghtJUNction/ssmcp/commit/e767225827eec4955ab49bb80629cea8879ec10b) |
| [#155](https://github.com/LIghtJUNction/ssmcp/pull/155)：前台重载目标与 context | 21:02:15 | [cf0ab581](https://github.com/LIghtJUNction/ssmcp/commit/cf0ab581700918a37fcccbebb93de2e0cab02841) |

cf0 的完整 tree 为 `bd2bcce33baffa69b82d69d9cff3abdb85baa0fb`。合并证明源码已进入该检查点，不自动继承其他提交的验收结果。

### 已完成的定向运行

[PR #155 的受测源码 bbeb](https://github.com/LIghtJUNction/ssmcp/commit/bbebfaef5eeefa182776ff03f21d510c1ea4fed3) 与 tree `1356b94a1e43b13185d280fec98451fdd5a6b010` 已完成命令行、环境变量、持久默认选择三种真实本地重载，分别记录 31、32、31 项断言；这些断言不累加为测试数。本检查点完整读取两份 JSON 回执并核对其哈希；主审另已核对开发机原件、不可变 CLI 与续跑驱动的字节数和哈希。

续跑只补齐产物身份及真实重载，8 条命令均退出 0。回执中的 101 项结果来自同一 bbeb 的先前阶段，即 84 项单元结果与 17 项 CLI 结果，没有在续跑中重做。本检查点未独立读完全部继承原日志及最终清理；该范围不等于 cf0 的整体验收、完整 P1 或性能比较。

### 已公开的源码阶段

[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 已实读为 open、draft，固定 head 为 [dcb504c3](https://github.com/LIghtJUNction/ssmcp/commit/dcb504c3947259e8912ac37c3123e46809419116)，tree 为 `cbeb8bdecdb8c6d07f585d4d2b4530d2d300653d`。实现负责人报告第一阶段于 21:09:40 完成、退出 0，仅做格式和差异检查，未执行 Cargo、测试或 PTY，也未纳入本轮粘贴补丁。此阶段存在 Unix/Linux 平台限制，不据此承诺跨平台可用。

后续阶段已开始处理新主线合流、`--input none` 与相关 hook/回答工具联动，以及已有时间能力的 UI 补齐和缺陷修复；尚无新增执行结果。PR #159 仍是待审草稿，不能写成已合入或输入功能验收通过。旧 CI 成功与失败继续绑定原提交，最终 P1、粘贴 A/B 和正式发布结果仍待取得。

### 输入草稿的补充复核

固定 dcb 的只读复核新增两项待修问题：[SSMCP 字面转发路径](https://github.com/LIghtJUNction/ssmcp/blob/dcb504c3947259e8912ac37c3123e46809419116/src/client/chat_io.rs#L214) 会先执行其后并不使用的本地引用准备，准备失败或超时会阻断原样转发；[HTTP 地址比较](https://github.com/LIghtJUNction/ssmcp/blob/dcb504c3947259e8912ac37c3123e46809419116/src/client/chat_io/transport.rs#L122) 在成功绑定合法端口 80 后，会拒绝省略默认端口的等价 Host/Origin。HTTP 默认端口省略的规范依据见 [RFC 9110 第 4.2.3 节](https://www.rfc-editor.org/rfc/rfc9110.html#section-4.2.3)。两项均已交回同一源码作者，尚未取得对应修复提交及运行结果；此复核没有启动产品、模型或新增测试。

## 输入后续源码与 CI 补记（2026-11-20 21:52 UTC）

### 两次失败分别归属

[b12 运行 44820665507](https://github.com/LIghtJUNction/ssmcp/actions/runs/44820665507) 绑定 `b12c43b19ecfa3e14c46a09923b7add9eac94b68`，三平台 fmt 成功、Clippy 失败，后续产品步骤跳过。[Linux job](https://github.com/LIghtJUNction/ssmcp/actions/runs/44820665507/job/136580436019) 完整日志有六处唯一诊断：`account/send.rs:52` 的嵌套 if、`chat_io.rs:251/257` 的多余借用与循环写法、`broker/session_io.rs:706` 的双重引用 clone，以及两处 socket 代码的多余借用。独立复核另读取 [Windows](https://github.com/LIghtJUNction/ssmcp/actions/runs/44820665507/job/136580436017) 与 [macOS](https://github.com/LIghtJUNction/ssmcp/actions/runs/44820665507/job/136580436164) 各三处诊断。Linux 原日志 89,800 字节，SHA256 `128f708ee943f2b5ee9a688f5418bf8bfbe47d548e9747dca1f3662391fc41d7`；[Library 副本](https://chatgpt.com/library?file_id=libfile_fde3_f-9ac4b2bd77ac8191b5017d3ae7bd7716_e58c93d727108191bbc40d6c0d09ec81&account_id=personal)及 [Drive 副本](https://drive.google.com/file/d/1oe1OfYBiD5IzK-BO-IahOkPwdNwm3gBh/view?usp=drivesdk)已保全，Drive 实际下载与原字节一致。

[cae 运行 44823183393](https://github.com/LIghtJUNction/ssmcp/actions/runs/44823183393) 已实际结束为 failure。[Linux job](https://github.com/LIghtJUNction/ssmcp/actions/runs/44823183393/job/136589172337) 在 Clippy 的测试编译中报两个 E0061：`local/socket/tests.rs:181/225` 调用 `Host::start(&settings)`，漏传 `local.rs:259` 新增的 `chat_disabled: bool`；exit 101。独立复核读取 [Windows](https://github.com/LIghtJUNction/ssmcp/actions/runs/44823183393/job/136589171833) 与 [macOS](https://github.com/LIghtJUNction/ssmcp/actions/runs/44823183393/job/136589172305) 各因 `steering.rs:1635:15` 的 `connect` 在 lib test 中未使用而失败，步骤分别退出 1、101。`qrcodegen` 未使用只报 warning。三平台产品步骤全部跳过；这些新阻断不等于 b12 的六处 lint 再次出现。

cae Linux 原日志 85,145 字节，SHA256 `6626494818643d6249ea4c103fe642673e5c31dc036f57a703c52af0610546f7`，已保留原 BOM/LF 至 [Library](https://chatgpt.com/library?file_id=libfile_fde3_f-184d59c2df8c8191be5f48d956b58aac_e58c93d727108191bbc40d6c0d09ec81&account_id=personal) 与 [Drive](https://drive.google.com/file/d/12PJrM3fXrx4a3SIJ0yNRgJ5HvagRi_PC/view?usp=drivesdk)；Drive 实际下载逐字节一致。

### 已公开的源码进展与未决问题

公开提交链为 [b12 合流](https://github.com/LIghtJUNction/ssmcp/commit/b12c43b19ecfa3e14c46a09923b7add9eac94b68)、[9232ba7 路由等修正](https://github.com/LIghtJUNction/ssmcp/commit/9232ba731389d249bbe3600075f3fcdc2e116116)、[bac3102 六处旧 lint 修改](https://github.com/LIghtJUNction/ssmcp/commit/bac3102f60506eb156ef6a5109d07559c16e9603)、[6336a38 输入禁用](https://github.com/LIghtJUNction/ssmcp/commit/6336a38b16ad68d4c859600928e7b23ef4e90156)、[cae 时间 UI 与能力处理](https://github.com/LIghtJUNction/ssmcp/commit/cae35a82683104049d4e30041df6e7e6bb3e6fc5)。cae tree 为 `2657970b36433de80a3cea9de3c5c5d2aba8728a`。

源码阶段于 21:45:18.963771 UTC 完成、退出 0。作者回执记录 69 个变更路径和 20 项成功检查，范围仅为元数据、60 个 Rust 文件格式、4 个 Python 文件 AST 及差异检查；该阶段没有本地编译、Clippy 重跑或运行验收。随后实际公开推送已由 GitHub 核对，不能沿用较早“未推送”的状态。

固定源码审查确认原两项 P2 的修改：[SSMCP 原样转发](https://github.com/LIghtJUNction/ssmcp/blob/cae35a82683104049d4e30041df6e7e6bb3e6fc5/src/client/chat_io.rs#L255-L303) 跳过未使用的本地引用准备并重查准入；[HTTP 默认端口](https://github.com/LIghtJUNction/ssmcp/blob/cae35a82683104049d4e30041df6e7e6bb3e6fc5/src/client/chat_io/transport.rs#L122-L150) 只在绑定 IP 和 scheme 精确匹配时接受省略 80。两项均未运行复现或测试。

新 P2 仍未解决：`--input none` 下 Snapshot 会被[聊天准入检查](https://github.com/LIghtJUNction/ssmcp/blob/cae35a82683104049d4e30041df6e7e6bb3e6fc5/src/broker.rs#L278)拒绝，而[连接状态仍未 ready](https://github.com/LIghtJUNction/ssmcp/blob/cae35a82683104049d4e30041df6e7e6bb3e6fc5/src/client/steering.rs#L1494-L1495)；[重载路径](https://github.com/LIghtJUNction/ssmcp/blob/cae35a82683104049d4e30041df6e7e6bb3e6fc5/src/client.rs#L2194-L2220)据此预计达到 15 秒超时。这是静态路径推断，没有执行复现；保存参数能够保留不代表实际重载已经通过。

### 粘贴入口的职责澄清

主审实读确认以下三份文件职责不同，固定摘要不相互替代。

| 文件角色 | 固定 SHA256 |
| --- | --- |
| `run_frozen_paste.py` 控制器／启动器，36,314 字节 | `4297fd6dc6561aa3a99bd05df20a9ca4abefe7675b9ee98fa42c37f7cc632aaa` |
| 六场景性能驱动 | `1300d60939de4df574eb4c0d989bca1c2906a29873a8cd1e68cc7c1b3ae364c8` |
| Ctrl+V 后继脚本 | `db2db3cfd14c1e6be0b36b4605a1c6f4790da4cab5a3515ad352739912ae238f` |

启动器仍调用 1300 驱动并绑定 db2；db2 的 `FROZEN_SHA` 仍须指向 1300。仅启动器改变不需要重设这个摘要，也不需要另造 Ctrl+V 后继脚本。724 的辅助文件集合只作参考；共同源码、构建与导出清单及五份实际二进制尚未绑定，本轮没有执行粘贴比较。

## c248 失败与后继修正（截至 2026-11-20 22:11 UTC）

### 三个平台的实际失败范围

[c248 运行 44824373375](https://github.com/LIghtJUNction/ssmcp/actions/runs/44824373375) 固定源码为 `c248d420e01d04c06be967e84543ce7d883f4cb3`。三平台 fmt 与严格 Clippy 均成功，后续结果如下；各行计数属于不同执行范围，不相加为整套验收。

| 平台与 job | 实际结果 |
| --- | --- |
| [Linux / 136593082355](https://github.com/LIghtJUNction/ssmcp/actions/runs/44824373375/job/136593082355) | 库测试 2030 通过、3 失败、23 忽略，30.02 秒，exit 101；后续构建与 lifecycle 跳过 |
| [Windows / 136593083004](https://github.com/LIghtJUNction/ssmcp/actions/runs/44824373375/job/136593083004) | session 52 项、protocol 10 项及构建通过；native lifecycle 40 条 PASS 记录、1 条 FAIL 记录（含重复阶段） |
| [macOS / 136593083123](https://github.com/LIghtJUNction/ssmcp/actions/runs/44824373375/job/136593083123) | session、protocol 及构建通过；native lifecycle 39 条 PASS 记录、1 条 FAIL 记录（含重复阶段） |

Linux 三个失败分别是 Agents 列表头右键没有切到 Logs、48 列时钟行的紧凑控制标签不在测试白名单内，以及 Logs 页脚只显示域名而断言要求完整 HTTPS 地址。原日志已独立实读并原样保全：883,294 字节，SHA256 `48666ff1df08987b7abdcb51d09014df4dff94a94583725b26c6295298a4640d`；[Library](https://chatgpt.com/library?file_id=libfile_fde3_f-305066a573c4819186cb4b5306813245_e58c93d727108191bbc40d6c0d09ec81&account_id=personal) 和 [Drive](https://drive.google.com/file/d/1CRymWjoLpoivHhc20knttxO_P2ttaaPR/view?usp=drivesdk) 副本均已保存，Drive 实际下载逐字节一致。

独立复核实际下载并核对了两份 native lifecycle ZIP：Windows artifact `14674230483` 为 2,515 字节，SHA256 `0db05bc524bcf20e10464d7eafbea489dcdb7283b38d5dab1205c0671c234743`；macOS artifact `14673681804` 为 2,384 字节，SHA256 `bb65072d54e6c8946bc1221775abe70248e1919d9a588aba8f2f72401e4b8745`。最终产物中，login start/poll 各为 1、whoami 为 8、unexpected 为 1，connect 分别为 Windows 20、macOS 14。清理阶段会重写这些计数，因此不能把最终文件当作失败断言瞬间独立保存的快照。

源码解释指向 [fixture 第 447 行](https://github.com/LIghtJUNction/ssmcp/blob/c248d420e01d04c06be967e84543ce7d883f4cb3/scripts/e2e_service_portable.py#L447)：`attach absent-fixture` 未带 `--local`，会按 Auto 路径进入 [account attach](https://github.com/LIghtJUNction/ssmcp/blob/c248d420e01d04c06be967e84543ce7d883f4cb3/src/client/account/attach.rs#L147-L166) 的 `GET /api/client/sessions`，继而落入 fixture 的 unexpected 分支。产物没有捕获实际请求路由；这条路径是源码推断，不作为已记录的 HTTP 路由事实。

### 后继源码的修正范围

[42e9a25b](https://github.com/LIghtJUNction/ssmcp/commit/42e9a25bb3a71a5fd06226a2b1d03c845532ef6e) 的 tree 为 `dd5e413dd0e024caf973afa8968fc75f3feef597`，仅处理 `--input none` 的 Snapshot/重载路径。主审与独立源码复核已检查该修改，尚无运行复现；它对应的 [CI 44825725216](https://github.com/LIghtJUNction/ssmcp/actions/runs/44825725216) 已取消，不能作为验收通过。

后继 [441814bf](https://github.com/LIghtJUNction/ssmcp/commit/441814bf9945e0b49bb3dde170e57762abf8318e) 的父提交为 42e9，tree 为 `72f4d06f61b06f62ae22ce1837dd429465ef0b39`。作者报告该源码阶段于 22:08:21 UTC 完成；实际完整补丁为五文件、15 行增加/11 行删除：

- 线程历史鼠标处理返回是否命中；未命中时恢复 Logs 回退，并保留 Escape 返回后的选择。
- 时钟行断言精确接受实际中文长、短控制标签；页脚仍要求最后一个 token 为同一 host 的完整或简写地址。
- portable fixture 的 absent attach 显式加上 `--local`，保留原严格请求计数检查。

这些是固定源码的已审变化；截至 22:11，[441 的新 CI](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363) 仍运行中，未宣称通过。

### 构建产物与执行边界

441 开发机构建仍在进行，新的五份产物尚未验收。另一个被取消的 c248 开发机构建，负责人补记已留下五件套中的一份库测试副本；因此既不是零副本，也不是完整五件套，且未被后续验收消费。这项补记来自构建负责人，不能代替完整产物身份核验。CI、开发机构建、组合验收和正式发布继续分别记录。

## 22:16 后续更新：441 的 CI 已完成

[运行 44826521363](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363) 的实际 head 为 `441814bf9945e0b49bb3dde170e57762abf8318e`，于 22:15:37 UTC 结束为 success。已实读 run 和三个 job 元数据；三平台 fmt、严格 Clippy、程序构建及 native lifecycle 步骤均成功，测试范围分别如下。

| 平台与 job | 实际成功范围 |
| --- | --- |
| [Linux / 136600512288](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363/job/136600512288) | 工作区库测试步骤成功；外层库批次 2034 通过、0 失败、23 忽略、0 筛除，30.44 秒；独立 session/protocol 筛选步骤跳过 |
| [Windows / 136600512121](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363/job/136600512121) | session 与 protocol 步骤成功；仅限 Linux 的工作区测试步骤跳过 |
| [macOS / 136600511790](https://github.com/LIghtJUNction/ssmcp/actions/runs/44826521363/job/136600511790) | session 与 protocol 步骤成功；仅限 Linux 的工作区测试步骤跳过 |

Linux 完整原日志的独立复核确认了精确 checkout 与上述外层计数，三个嵌套子进程的单项摘要没有重复相加。新增 `disabled_chat_keeps_the_outbox_and_advances_reload_without_queue_commands`，以及此前失败的滚动列表鼠标选择、实时钟行、统一页脚三个回归，均在 441 原日志中明确显示 `ok`。`--input none` 的这条回归证据因此已从源码审查推进到固定 441 的真实库测试通过；实际进程重执行、粘贴 A/B 和最终 P1 仍未完成。Windows/macOS 此处不沿用 c248 的旧测试数量。

开发机另在 22:15:38 UTC 实读确认：目标构建锁已取得，源码、metadata 和依赖身份检查已完成，库 `--no-run` 编译自 22:14:50 UTC 起实际运行。尚无完整五份产物或开发机产品测试通过回执；因此本次 CI 成功与后续产物验收继续分开记录。

## 22:45 检查点：781 CI 通过，开发机产物保留仍待修正

### 781 的打包补充与实际 CI 结果

[`781b87df`](https://github.com/LIghtJUNction/ssmcp/commit/781b87df1d364137b6885f17f631a3e9e991bd58) 的父提交为 441，tree 为 `6bf7beb143cfdce3878df856c27785164695430f`。完整补丁只在 `Cargo.toml` 的打包清单增加三行，纳入已有的 `docs/local-mode.md`、`scripts/e2e_local_socket.py` 和 `scripts/e2e_connection_context_reload.py`。

[运行 44829555014](https://github.com/LIghtJUNction/ssmcp/actions/runs/44829555014) 的三平台 fmt、严格 Clippy、构建和 native lifecycle 均成功；实际测试结果如下。Linux 的独立 session/protocol 步骤按平台分工跳过，Windows/macOS 的 Linux 工作区步骤跳过。

| 平台与 job | 本次 Rust 测试结果 | native lifecycle PASS 记录 |
| --- | --- | --- |
| [Linux / 136610732045](https://github.com/LIghtJUNction/ssmcp/actions/runs/44829555014/job/136610732045) | 外层库批次 2034 通过、0 失败、23 忽略、0 筛除，38.29 秒 | 42 |
| [Windows / 136610731818](https://github.com/LIghtJUNction/ssmcp/actions/runs/44829555014/job/136610731818) | session 52 通过，0.43 秒；protocol 10 通过，1.87 秒；均 0 失败、0 忽略 | 41 |
| [macOS / 136610731721](https://github.com/LIghtJUNction/ssmcp/actions/runs/44829555014/job/136610731721) | session 58 通过，0.29 秒；protocol 10 通过，4.80 秒；均 0 失败、0 忽略 | 40 |

三份完整工具日志的独立复核确认了精确 checkout；Linux 三个嵌套单项摘要未加到外层计数，lifecycle 的 PASS 记录也不与 Rust 测试数相加。三份原始 artifact ZIP 均已实际下载并匹配 GitHub digest，报告为 passed、末项登录复用为 PASS。清理后计数均为 login start/poll 各 1、whoami 8、unexpected 0，connect 为 Linux 24、Windows 20、macOS 14；它们仍是清理后的最终文件，不是断言瞬间另存的快照。各 ZIP 身份见结构化记录。

### 441 开发机构建的真实停止点

主审实际读取的构建回执为 50,281 字节，SHA256 `5c3f2c3c971d7fbf908078997edb370a0345ca9cd850ee205226e665ffca9864`。库和 release 的 Cargo 编译均退出 0，失败阶段为 `preserve-build-release-bins`，原错误为 `expected one actual source-matched link compilation for ssmcp, got 0`。保留脚本只认可 release 根目录或 `release/deps`，实际链接输出位于 `release/build/ssmcp/<hash>/out`。

当次只保留一份库测试产物，165,523,552 字节，SHA256 `26344a306712b6ff7db048d7f75cbaed95f15bee0b58ff48db48f4013418a7cd`；decoder 未构建，产品测试、基准及发布核验均未执行。回执记录了两把锁的释放时间。旧 r3 未供后续验收消费；r4 正补齐“实际链接输出 → 对外产物 → 保留副本”的字节绑定，不清缓存、不改全局 wrapper。22:34 的最后阶段观察中，781 开发机新基线尚未启动。18 项准备项为 6 + 11 + 1 项真实 none 重执行，仍待执行；6733 粘贴补丁尚未应用，未产生 A/B 结果。

### 发布核验工具的路径修正

[`90a83b0b` 中的工具](https://github.com/LIghtJUNction/ssmcp/blob/90a83b0b74d7553d9c34470c96f5a3233d984b74/docs/reports/command-await-delivery-20261120/publication-verifier/verify_published_candidate.py) 为 30,229 字节，SHA256 `27804d3605f2d0eecf8434100a193a89829dd7324b6150da5036cafc8bbd0755`；新增必填 `--checkout`、`--report-dir`，保留 candidate/run_id。公开[局部核验记录](https://github.com/LIghtJUNction/ssmcp/blob/90a83b0b74d7553d9c34470c96f5a3233d984b74/docs/reports/command-await-delivery-20261120/publication-verifier/path-argument-validation.json) 记载 12 个核心函数 AST 不变，main 中从报告构造起的发布核验后缀逐字不变；仍要求 tag push 对应的正式发布 run。

[Drive 副本](https://drive.google.com/file/d/1vqDINv8cGfEt2r3NzpE8jSIn3JT-S_If/view?usp=drivesdk) 已由主审实际下载并确认字节数、SHA256 及逐字节相等。本次范围为语法和本地参数/路径检查，原完整 schema 套件未重跑，未执行网络发布核验，也不产生 v0.10.21 已发布结论。

### 781 CI 证据归档

三份工具日志、三份原始 ZIP 和三份提取报告均已保存。日志及 ZIP 的六份 Drive 副本均已实际下载，与各自原件逐字节相等；提取报告保留独立原件，也随对应 ZIP 完整保全。

| 平台 | 工具日志（Drive） | 原始 ZIP（Drive） | 报告原件 |
| --- | --- | --- | --- |
| Linux | [887,092 B](https://drive.google.com/file/d/1FhSvSdXLCusjSy2wsQuspFCMLSHjdbnw/view?usp=drivesdk) | [1,423 B](https://drive.google.com/file/d/1wdd3yKdL-e7VyySUq-YyxHWhmGfB9OKa/view?usp=drivesdk) | [3,763 B](https://chatgpt.com/library?file_id=libfile_fde3_f-ee7a5b362f808191938eb546204e7409_e58c93d727108191bbc40d6c0d09ec81&account_id=personal) |
| Windows | [107,290 B](https://drive.google.com/file/d/1V9eF2Cq7QyLxqaKHRcmWz6jfYt7bgGw0/view?usp=drivesdk) | [1,475 B](https://drive.google.com/file/d/1aRT2J87bSpD1XBmULxNbS2IxI0GwFoWI/view?usp=drivesdk) | [3,770 B](https://chatgpt.com/library?file_id=libfile_fde3_f-59f9c339c4648191bcacee2f27c9a31c_e58c93d727108191bbc40d6c0d09ec81&account_id=personal) |
| macOS | [104,480 B](https://drive.google.com/file/d/1wyKVW6FUFk6AKMbsWyKE6fs9Ety7fZqD/view?usp=drivesdk) | [1,414 B](https://drive.google.com/file/d/1qLWyaOM0MHrxQ9l62-vm7cwjWDAtA_ov/view?usp=drivesdk) | [3,670 B](https://chatgpt.com/library?file_id=libfile_fde3_f-4c994b83bbb48191b116b26b65175926_e58c93d727108191bbc40d6c0d09ec81&account_id=personal) |

日志身份以 GitHub 工具返回的解码文本经 UTF-8 保存后的字节为准：三者均保留 BOM，Linux/macOS 保留 LF，Windows 保留 CRLF。ZIP 身份以实际下载字节为准。完整 SHA256、原保存标识、版本 0 及回读范围见[结构化检查点](integration-checkpoint-2241.json)。这些归档证明本次 CI 证据已保全，不扩展为开发机重执行、粘贴 A/B、P1 或正式发布结果。

## 23:06 检查点：五份构建产物完成，首次测量停在前置断言

### 781 构建完成的范围

本次源码为 [`781b87df`](https://github.com/LIghtJUNction/ssmcp/commit/781b87df1d364137b6885f17f631a3e9e991bd58)，tree 为 `6bf7beb143cfdce3878df856c27785164695430f`。r4 构建回执为 79,446 字节，SHA256 `d2204375614807af510f6c7fd08ec657d212a98f18f504cb21376c63f47ea7c0`；构建进程实际退出 0，两输出流均已结束。以下五项来自回执的实际链接输出、Cargo 对外产物与保留副本绑定：

| 产物 | 字节数 | SHA256 |
| --- | ---: | --- |
| `library-test-fixture` | 165,519,472 | `edea75a764a08d6b4ed76d49d16160ce3750dc9f333cce41e574c46562ac9c81` |
| `ssmcp` | 15,754,224 | `09c7219a0863ebfc97f51246e569b65dd4963493b9e8b7f0885672b6b95c5ac2` |
| `ssmcp-frontend` | 1,386,576 | `229e65b1f52098b7292d9e387e9442d24c9411ab2e34a55fcbe4a6171e0398b5` |
| `ssmcpd` | 8,252,208 | `909133cae9bfc0d9bcefdf4af4740a9bc3c50a1097025a3178bc8e04daa62e86` |
| `vt_snapshot` | 440,984 | `efdbeb3f200fcf4404b2612d836433047ecc90eb0744f345fc971ee7441db97f` |

五项均为 `fresh=false`，前后身份检查通过；该字段只描述选定产物，依赖仍可复用缓存。三个构建命令及全部保留、metadata、后置检查完成，修复了前述 441 保留阶段的阻塞。这里核对的是回执及其声明的产物绑定，未声称主审另行重算二进制哈希。当次产品测试为 0、基准为 0、package gate 为 0，release gate 未执行；旧 441 失败继续保留原归属。

### 首轮 PTY 尚未产生性能样本

执行回执为 23,288 字节，SHA256 `23cf70065f3b4aa72beaeb45fefe0a6006da6fa5d5d1b4667249081db1348cb4`，进程退出 1；失败报告为 7,946 字节，SHA256 `510422922070d1e6c3d831c3a5d41e91812cfa980d21feb36c082baa194ffcd0`，错误为 `AssertionError: the real Next caller input is active`。17 条账户夹具前置断言已通过，但六场景步骤在前置等待失败，样本与帧数组均为空；Ctrl-V 未执行、Cargo 未调用，输入前后相同。这 17 条前置断言不计入尚未执行的 18 项定向验收。

针对固定 781 源码和冻结六场景驱动 `1300d60939de4df574eb4c0d989bca1c2906a29873a8cd1e68cc7c1b3ae364c8` 的独立检查确认：explicit `member:next` 的 UI target 是 `next`，`native_recipient` 仍为 None。旧驱动要求 `target is None and native_recipient is None`，因此未通过前置判断。拟修正为 `target == 'next' and native_recipient is None`，同步派生 Ctrl-V 和相关哈希绑定；六场景、计时定义及时限保持，产品源码和本次五份产物无需重编。当前尚无后继驱动实际通过结论，也无粘贴基线、A/B、最终 P1 或正式发布结果。

## 23:41 检查点：第四轮完整基线通过

| 尝试 | 实际取得 | 停止点或完成范围 |
| --- | --- | --- |
| 1，退出 1 | 17 条账户夹具前置检查通过；0 样本、0 帧 | Next caller 就绪断言失败；Ctrl-V 未执行 |
| 2，退出 1 | 前四场景 passed、4 帧；第五场景已 ready，状态仍为 running | 队列后检查收到 33,730 字节溢出产物；busy、Ctrl-V 未进入 |
| 3，退出 1 | 前五场景 passed；63 条检查、5 帧 | busy 样本创建前 prefix 准备超时；Ctrl-V 未执行 |
| 4，退出 0 | 六场景 passed，80 条检查、6 帧；Ctrl-V passed，31 条检查、1 帧 | 两步完成，输入不变；80/31 含账户夹具，不是单元测试数量 |

四轮均未调用 Cargo，输入前后保持不变。第三轮只确认准备等待的完整标记与折叠预览未匹配，具体裁剪层未确认。R3 将同一 152 字节、2 ms 间隔输出行中的 marker 换序，并更新三个依赖哈希绑定；六场景、计时与时限不变。第四轮通过不改写前三失败，也不把静态核对算成执行。

### 第四轮观测值

| 场景 | 预期反馈 | ready_ms |
| --- | --- | ---: |
| 多行 2,738 B | 引用就绪 | 67.904495 |
| 多行 64 KiB | 引用就绪 | 34.839222 |
| 多行 4 MiB | 引用就绪 | 370.222459 |
| 多行 4 MiB + 1 | 拒绝反馈 | 337.795869 |
| 普通输入 16 KiB | 内联就绪 | 2439.160607 |
| busy 多行 2,738 B | 引用就绪 | 43.186361 |
| Ctrl-V 2,738 B | 引用就绪 | 50.066292 |

每项 `n=1`，不是均值或 A/B。ready 从首次成功 PTY 写入前的时间点起，计入 VT 解码观察成本；4 MiB + 1 测的是拒绝反馈。busy 期间观察到 20 个输出 tick，前后 producer 均运行。Ctrl-V 使用 fake wl-paste 与真实单字节 `0x16`，同一构建的 identity 前后相等；其结果不代表 OS 剪贴板性能。未发送 Enter，也未测量 agent 递送。

### 原回执与报告身份

| 尝试 / 原件 | 字节数 | SHA256 |
| --- | ---: | --- |
| 1 / 回执 | 23,288 | `23cf70065f3b4aa72beaeb45fefe0a6006da6fa5d5d1b4667249081db1348cb4` |
| 1 / 报告 | 7,946 | `510422922070d1e6c3d831c3a5d41e91812cfa980d21feb36c082baa194ffcd0` |
| 2 / 回执 | 23,376 | `2af5fa2aa4548518480bfdb2bcdaa5954a7cab1db3ebbb6905dffe5cc7e50cf1` |
| 2 / 报告 | 16,393 | `239b8156043699336cf4befe9cf07d3596b46d670c19aa66dd06c63244cca841` |
| 3 / 回执 | 23,389 | `1e3428e06d9e4c178a13303aff02a59b912731c563283ea3aa84a55cd105bb17` |
| 3 / 报告 | 18,666 | `ec4cdf40b88f80d2179922c20e5e20601f8a2c330087159c18eb965a02981adb` |
| 4 / 回执 | 29,263 | `aefc6b8a20c8bec808057acae758c60fb44630c5912cb1851cced2a7dc2eef63` |
| 4 / 六场景 | 19,660 | `7b7c9081616aca563d74efe883cd716e6be8da594e2671e283c904a7b096c64c` |
| 4 / Ctrl-V | 13,333 | `df1d570ed1bbcf8e47d6a4e99c94d06357319ebc0ca22a9c5e9d3d82b05498d4` |

### 当前 R3 冻结工具

| 文件 | 字节数 | SHA256 |
| --- | ---: | --- |
| `e2e_paste_performance.py` | 32,716 | `ccdcc2fed932114125bf832be8370bc17ecfd082a18b95822dd2c3153190784e` |
| `e2e_clipboard_paste_performance.py` | 14,624 | `d840813f8135dc5feece1f03e8daff1011726f1144f55ca0f92be271473cbe40` |
| `run_frozen_paste.py` | 54,508 | `88b80f5dcf051d059439968b21443537152544bf33b84c44e0d1dbc9d49a3bfa` |
| `manifest.json` | 3,538 | `4408cd67393d1f9e59cdb33637550e0874ff682d3669eb980757d19c999a3970` |

R2 的完整历史 tuple 仍在 JSON；R3 为当前冻结组合。同源完整 queue 数组投影及 total、无 next_offset 检查，经[固定 781 接口](https://github.com/LIghtJUNction/ssmcp/blob/781b87df1d364137b6885f17f631a3e9e991bd58/src/client/interface.rs#L298-L339)静态核对，排除独立草稿正文；该审查不计作运行结果。

### 版本观察（23:41 UTC）

主审已读 v0.10.21 tag 对象 `30cd4949` 指向 [`147dae30`](https://github.com/LIghtJUNction/ssmcp/commit/147dae309cf37af578c0ac1d3aff2cb6008b0f48)，v0.10.22 的 create 事件指向 [`57d3f367`](https://github.com/LIghtJUNction/ssmcp/commit/57d3f36748f22996d3b161d7a0adf4757e0e4a4c)。另一条 [Release 44836598784](https://github.com/LIghtJUNction/ssmcp/actions/runs/44836598784) 通过 main 的 workflow_dispatch 运行；23:41 实读其 15 个已创建 job，其中 3 个检查失败、2 个仍运行，尚未取得完整发布结果。最新已发布版本仍为 [v0.10.20](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20)。

[PR #159](https://github.com/LIghtJUNction/ssmcp/pull/159) 的 781 仍为 draft；[781 与 57d 的比较](https://github.com/LIghtJUNction/ssmcp/compare/781b87df1d364137b6885f17f631a3e9e991bd58...57d3f36748f22996d3b161d7a0adf4757e0e4a4c)为 diverged（ahead 24、behind 22）。这些标签和另一候选的工作流不构成本轮验收或发布完成证明，最终版本及源码合并关系仍由原执行负责人协调。

## 2026-11-21 00:13 检查点：固定 fc 候选完成粘贴 A/B

### 源码与证据范围

候选 [`fc06728a`](https://github.com/LIghtJUNction/ssmcp/commit/fc06728a36d0b4d2920e1e2bc29885721ced5780) 的 parent 为 781，tree 为 `088fe1cde36fe63a33df1f9b7cfb619ae5f8ee35`。产品改动是批量处理字面量输入、降低补全刷新频率，继续使用已有的异步保存路径。新增[七个 Rust 测试](https://github.com/LIghtJUNction/ssmcp/blob/fc06728a36d0b4d2920e1e2bc29885721ced5780/src/client/terminal/conversation/tests/paste_batch.rs)已完成静态覆盖审查，并编入成功的库测试产物，尚未执行。（本次更正原 91f“仅静态”的遗漏，不是新增测试执行。）

候选构建退出 0，两输出流结束，五份产物保留完成、前后 guard 通过；该回执为 build_only，构建阶段产品测试数为 0。随后冻结 PTY 执行也退出 0：六场景 passed、80 条检查、6 帧；Ctrl-V passed、31 条检查、1 帧。80/31 包含账户夹具，不合并为单元测试数量。

主审实际比对两份执行回执，完整冻结 cohort 的 18 个顶层字段相等，差异键为空，包含九个 helper 及其他配置。候选记录的基线六场景 SHA256 为 `7b7c9081616aca563d74efe883cd716e6be8da594e2671e283c904a7b096c64c`，对应此前完整第四轮；源码、二进制与载荷身份分别核对，两侧各自前后不变。全部来源绑定与五份候选产物身份见[小型事实记录](candidate-ab-0010.json)，历史见[固定原报告](https://github.com/LIghtJUNction/ssmcp/blob/a87381bf9861150a994d2b75c1e31a40ba96bfa0/docs/reports/command-await-delivery-20261120/README.md)。

### 本轮配对观测

下表单位为毫秒，基线为 781，候选为 fc；每场景各侧一次，均通过相同预期与载荷检查。“耗时比”是本次基线 ready_ms 除以候选 ready_ms，不是总体性能倍数。

| 场景 | 基线 ready_ms | 候选 ready_ms | 本轮耗时比 |
| --- | ---: | ---: | ---: |
| 多行 2,738 B | 67.904495 | 14.919875 | 4.551 |
| 多行 64 KiB | 34.839222 | 28.644784 | 1.216 |
| 多行 4 MiB | 370.222459 | 55.933765 | 6.619 |
| 多行 4 MiB + 1（拒绝） | 337.795869 | 44.152384 | 7.651 |
| 普通输入 16 KiB | 2439.160607 | 27.853111 | 87.572 |
| busy 多行 2,738 B | 43.186361 | 29.151625 | 1.481 |
| Ctrl-V 2,738 B | 50.066292 | 46.399351 | 1.079 |

ready 包含 VT 观察与解码成本。4 MiB + 1 的数值是拒绝反馈；Ctrl-V 使用 fake-private wl-paste 和真实单字节 `0x16`，不是 OS 剪贴板 benchmark。没有发送 Enter 或测量 agent 递送。169.0387 秒的 quiet-lock 等待属于执行排队，不是粘贴延迟，也未并入上述数值。这里不作均值、统计显著性或最终整合源性能结论。

### 原回执与报告身份

| 原件 | 字节数 | SHA256 |
| --- | ---: | --- |
| 候选构建 | 79,517 | `8e6154ba2bbb16bab14cfa8d4b0a693775c28656e3dc554ad1e5d5301108ddd7` |
| 候选执行 | 30,277 | `13a685aa21e523c1d42e4c62b4bd61b6fb065fb0d8a5500a819e34218cb1e1bb` |
| 候选六场景 | 21,212 | `4ef95978b201f9a3a8d3c42cd4ded2282ff4e7c802a269242027a32691cc6b49` |
| 候选 Ctrl-V | 13,247 | `af2b6f5a996ae60fe2a0bb4bf632e5c1bb39e015111a205deb3b8bf68d88be7f` |

fc 的 CI 状态尚未确认，不能沿用 781 的三平台通过结论。当前源码正与[公开 main f208](https://github.com/LIghtJUNction/ssmcp/commit/f208a03b5403332214f3acf6ee0b176752b5d2c9)整合；最终整合、定向验收（既有 18 项清单待按最终源码确认）、P1 和正式发布仍未完成，版本未选定，完成状态保持 false。

本截止点另行读取的[其他分支 v0.10.22 发布运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/44836598784)已结束为 failure，更新时间为 2026-11-20 23:50:46 UTC；[GitHub 最新正式版](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20)仍为 v0.10.20。这些观察单独记录，不改变本组候选的验收或发布状态。

## 2026-11-21 00:24 更正与补充：已编译测试及分段观测

### 更正测试状态

原 91f 报告两处“仅静态”表述漏记了编译成功。七个新增 Rust 测试已进入构建回执 `8e6154ba2bbb16bab14cfa8d4b0a693775c28656e3dc554ad1e5d5301108ddd7` 对应的库测试产物，产物 SHA256 为 `6ab9368340abb1ec715b3841ddf814aa2fd2bd9e0f371d622fc841e23806b7f3`。这是既有 `--no-run` 构建的事实补记，测试尚未执行，也没有新增通过数量。[修正后的候选记录](candidate-ab-0010.json)保留原观测截止和全部来源。

### 六场景分段耗时

下表均为毫秒，箭头左侧为 781 基线，右侧为 fc 候选，使用同一批已通过的原报告，每侧各一次。原七项 primary ready 表不变。

| 场景 | PTY 写入完成 | 首次可见 | 最后写入至 ready |
| --- | ---: | ---: | ---: |
| 多行 2,738 B | 0.007923 → 0.009907 | 33.536642 → 14.919875 | 67.896572 → 14.909968 |
| 多行 64 KiB | 3.789682 → 0.291445 | 34.839222 → 28.644784 | 31.049540 → 28.353339 |
| 多行 4 MiB | 330.458831 → 25.966284 | 335.333930 → 55.933765 | 39.763628 → 29.967481 |
| 多行 4 MiB + 1（拒绝） | 328.006441 → 22.376305 | 337.795869 → 44.152384 | 9.789428 → 21.776079 |
| 普通输入 16 KiB | 0.023443 → 0.019377 | 2439.160607 → 27.853111 | 2439.137164 → 27.833734 |
| busy 多行 2,738 B | 0.007733 → 0.008528 | 5.377071 → 29.151625 | 43.178628 → 29.143097 |

“PTY 写入完成”只到 PTY 接受全部字节；“首次可见”是夹具首次观察到的粘贴相关输入／通知帧；“最后写入至 ready”从最后一次 PTY 写入计至观察到 ready。它们不是互斥的三个连续阶段，不能相加；ready 仅等于写入完成耗时加写完后等待。

busy 的首次可见由 5.377071 增至 29.151625 ms，超限场景写完后等待由 9.789428 增至 21.776079 ms。小 2,738 B 和 busy 的写入耗时也略增。主 ready 指标降低与这些局部增加同时存在，不能写成每个环节都更快。

### 写入与背压计数

| 场景 | write_calls：基线 → 候选 | backpressure_waits：基线 → 候选 |
| --- | ---: | ---: |
| 多行 2,738 B | 1 → 1 | 0 → 0 |
| 多行 64 KiB | 13 → 12 | 11 → 11 |
| 多行 4 MiB | 867 → 966 | 862 → 943 |
| 多行 4 MiB + 1（拒绝） | 889 → 984 | 886 → 976 |
| 普通输入 16 KiB | 3 → 2 | 0 → 0 |
| busy 多行 2,738 B | 1 → 1 | 0 → 0 |

4 MiB 与超限场景的写入次数、背压等待次数均增加，虽然其 PTY 写入总耗时降低。事件计数与耗时是不同的观测量，不能互相替代。

Ctrl-V 的触发写入耗时为 **0.006661 → 0.006834 ms**；整个 Ctrl-V 夹具 step 的墙钟耗时为 **3.975164 → 5.381131 s**，后者包括整步夹具工作，不是纯粘贴延迟。原 ready 观测仍为 **50.066292 → 46.399351 ms**。

Preparing 字段为 `null` 表示未观察到，不能填成 0 ms，也不证明该阶段被消除。VT 采样含解码与观察成本；这些单次结果不支持统计或因果泛化。四份原报告的完整 SHA、所有分段值、计数和保留的 `null` 均见[分段指标 JSON](candidate-ab-submetrics-0021.json)。本次没有重测、执行新增 Rust 测试或新增验收门禁。


## 2026-11-21 00:39 源码检查点：固定 24c 中间合流

### 源码身份与检查过程

合并提交 [`b87d8d0f`](https://github.com/LIghtJUNction/ssmcp/commit/b87d8d0f1c47684da9574610f1279d40aefa2f96) 的两个父提交分别为固定 fc 和 f208；其后 [`24c32869`](https://github.com/LIghtJUNction/ssmcp/commit/24c32869ba730f4a91406ebd6b05e18a462bec80) 只调整一条 Composer 预期，tree 为 `d8cb76cefa139d4809edba45f2b701d45e2a59e0`。普通推送于 00:29:31.980906 UTC 退出 0，远端回读相等且工作树干净。它仍是未合并的 draft PR，尚未包含后续公开 f33；[Cargo.toml](https://github.com/LIghtJUNction/ssmcp/blob/24c32869ba730f4a91406ebd6b05e18a462bec80/Cargo.toml) 的实际版本是 0.10.22。

检查过程包含两类退出 1，不能概括为从始至终全部成功。最初三方合并遇到冲突，随后解决并提交；初始 TOML probe 把两个父提交已有的 include 重复项当成新问题，修正为检查“不新增重复”后通过。最终 rustfmt、Python AST/静态契约及合并和 Composer 差异检查退出 0。这些结果属于源码检查，没有运行 Cargo 构建、产品测试或性能测量；原回执的字节数与完整 SHA256 见[结构化记录](source-union-0038.json)。

### 已审保留范围与限制

粘贴核心在已审差异中保持不变。[`paste_literal_prefix`](https://github.com/LIghtJUNction/ssmcp/blob/24c32869ba730f4a91406ebd6b05e18a462bec80/src/client/terminal.rs#L1218-L1233) 仍批量消费首个 ESC 前的字节，将 ESC/分段标记交回原解码路径；[`chat_output` 与批处理](https://github.com/LIghtJUNction/ssmcp/blob/24c32869ba730f4a91406ebd6b05e18a462bec80/src/client/terminal/conversation.rs#L2463-L2589) 保留 UTF-8 暂存、大小限制和输出/按键顺序。InputStamp、异步完成与补全刷新选定区段和 FC 原文逐字节相同。远程 finish/answer 的退役检查按工具名精确匹配，本地 `chat_finish_input` 及其草稿变化保护仍保留。

[七个粘贴测试](https://github.com/LIghtJUNction/ssmcp/blob/24c32869ba730f4a91406ebd6b05e18a462bec80/src/client/terminal/conversation/tests/paste_batch.rs)及其模块注册保留，文件仍为 8,620 B，SHA256 为 `68511f539518cd13a8067598cf660734eefed4436fbeeb24d579a0f9215ef6e9`，与 FC 相同。其静态覆盖仍包括批量工作计数、补全刷新、分段 UTF-8、CR/LF/Tab/ESC 顺序、PNG 不透明缓冲及超限保护；不能据此宣称真实异步完成或 pending 输入竞态已运行通过。**FC 的七个测试已编译不等于 24c 已编译。** 本轮没有重新核验全部传递依赖或性能 driver，也没有把既有 A/B 结果迁移到本次合流。

另行完成的 CLI 接收者/加载参数边界、保存/重载数据路径和回复守卫静态审阅未见确定阻断；provider 运行时 argv 仍未实证。后续由同一作者继续纳入 [`f33ed8f1`](https://github.com/LIghtJUNction/ssmcp/commit/f33ed8f14fc14aff05a2e4098743eb07e8527c06)，目标版本现已选定 0.10.24，属于后续源码计划。最终冻结、构建、定向运行验收、P1 和正式发布继续待完成。

### 独立版本观察

另一发布线的 [v0.10.23 运行 44843331041](https://github.com/LIghtJUNction/ssmcp/actions/runs/44843331041) 已为 completed/failure，更新时间 00:33:03 UTC，工作流来源为 f33。当前最新正式发布仍为 [v0.10.20](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20)，共 16 个 assets。这些观察不构成本组发布成绩，也不改变中间源码版本 0.10.22 与后续目标 0.10.24 的区别。


## 2026-11-21 00:48 源码冻结检查点：候选 B / 0.10.24

### 冻结身份与静态结论归属

本次通过三个提交从 24c 推进至 B，主审已核对公开 Git 对象链、PR head 与 Cargo 版本。

| 提交 | 作用 |
| --- | --- |
| [`10645530`](https://github.com/LIghtJUNction/ssmcp/commit/106455301ebf7bd35a72c0074eb9507be74778db) | 正常合并，两个父提交为 24c 与公开 f33。 |
| [`c8f5d1ec`](https://github.com/LIghtJUNction/ssmcp/commit/c8f5d1ecc25721937f6ebc585247c67cfa75a932) | 准备 0.10.24 源码版本元数据。 |
| [`be6da5bf`](https://github.com/LIghtJUNction/ssmcp/commit/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1) | 只修正 dashboard 夹具的判据，形成当前冻结候选 B。 |

冻结记录从此前 FC 推进到 B，工作树为 clean，四个 gitlink 保持不变；版本准备中的 Cargo.lock 修改仅为本包版本。冻结 Cargo.lock 为 66,476 B，SHA256 为 `e8578cea449acae5281fc723f30bbaba56774cc3f460dd4e449b8381c21be53b`。普通推送未使用 force，完成于 00:42:08.446246 UTC；冻结完成于 00:43:00.589512 UTC。完整四个 gitlink 与提交/tree 绑定见[冻结事实](frozen-source-0047.json)。

已审 CLI、粘贴和回复相关的 18/18 个源码 Git blob 在 24c→B 之间完全相同，因此[24c 的窄范围静态结论](source-union-0038.json)可按对应代码字节复用。这不能迁移任何编译、测试或 A/B 结果：七个新增 Rust 测试在 FC 中已编译但未执行的更正保持不变，B 尚无构建通过结论。原 A/B Git 对象、导出产物与回执未改动，历史性能表继续只描述固定 781 与 FC 的每侧单次观测。

### dashboard 判据修正的依据与边界

旧夹具要求底栏出现 Wheel 且物理光标隐藏。已有失败日志显示 Wheel 缺失、客户端仍存活，但没有打印最终 hide_cursor 或 focus 值；源码中连接版本 notice 的底栏优先级高于 Wheel 提示。因此不能把该失败认定为 ARM 慢，也不能只归因于裁剪。

B 的夹具改为要求 Enter Apply 从底栏消失、物理光标隐藏，并实际读取 `/display/focus` 为 `audit`；产品底栏和超时不变，保留 End 后 scrollback 为零、同批 PageUp 加 x 及后续输入/resize 检查。该修正目前只有源码证据，实际运行效果仍待验证。

### 原件身份与复核层级

下表区分主审完整实读的回执与仅由静态回执登记的源变更原件，避免把哈希登记写成全文独立复核。

| 原件 | 字节数 | SHA256 | 本次证据层级 |
| --- | ---: | --- | --- |
| 源码变更回执 | 47,820 | `df0eb4f02aee4b2bf3b57cf16c495b4d2a390a489e284b0006f2149a26e9f173` | 静态审阅回执登记；主审尚未独立全文读取该原件。 |
| root-static-review.json | 2,528 | `968ec96e5d02ac60dd897b1fe36ca7d1f3a8c4cdaf7c8c08a89ad7a64159cca8` | 主审完整实读。 |
| root-normal-push-receipt.json | 1,072 | `efc4e85ce78aa3a62c750d0d0cb87c87bd2756e8a5afca3ccb2198bfaeb673f0` | 主审完整实读。 |
| source-freeze-be6da5b.json | 2,862 | `518f21c2afe0c148f4d653ce861e8cd6ab0e590b899e21414c91f85eedc597c1` | 主审完整实读。 |

B 构建、运行验收、最终 P1 与正式发布均未完成。执行安排仍等待既有共享计算窗口明确交接，本次没有启动或排队新的重任务。上一节关于其他发布线 v0.10.23 失败和 v0.10.20 正式版的观察保留原截止时间，不作为 B 的发布成绩；本次也未刷新或改写该历史观察。


## 2026-11-21 00:50 后续协调：候选版本需重新确认

源码作者与协调者随后报告，另一条既有发布链已在本机进行 0.10.24 验证并持有共享构建目标锁；原计划共享窗口内的计算任务自身尚未启动。主审尚未独立读取该进程细节，本报告不把转述升级为独立进程核验。源码作者已明确暂停 B 的最终绑定执行和发布，先协调既有验证任务的所有者与版本意图；B 的验收流程未启动，也没有排队新的重任务。

B 的公开提交、普通推送、0.10.24 冻结回执及既有静态审阅仍作为历史事实保留。后续最终源码版本需要重新确认，尚无替代版本的确定选择；本次未改写源码、历史性能结果或任何发布成绩。


## 2026-11-21 01:30 发布输入与 InputLease 审阅

### 0.10.25 准备提交与发布策略验证

公开观察按时间保留：01:22:51 UTC 的工作分支回读仍为 B；01:25:44 UTC 已核对到 [P](https://github.com/LIghtJUNction/ssmcp/commit/f4e6ff043eef6686a9013750ffcb8ab66926b9d4)。P 仅改六个文件，23 行新增、5 行删除，涉及自身版本、部署延后策略及发布文档；没有改 `src` 或 gitlink。其[发布说明](https://github.com/LIghtJUNction/ssmcp/blob/f4e6ff043eef6686a9013750ffcb8ab66926b9d4/docs/release-notes-0.10.25.md)明确列出最终整合、构建运行、发布安装结果待确认。代码保持相同的范围可以复用静态审阅，任何运行成绩均不迁移到 P。

固定 B 的[发布策略](https://github.com/LIghtJUNction/ssmcp/blob/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1/.github/workflows/release.yml#L75-L122)默认 `requested=True`，只有 `workflow_dispatch` 会读取 `deploy_server` 输入；部署输出条件为 `publish == "true" and requested and deferred is None`，延后列表止于 0.10.24。因此，仅把版本升为未列出的 25 并不能继续保证跳过部署；这是固定配置的条件推导，不是已经发生部署。P 已在[源码](https://github.com/LIghtJUNction/ssmcp/blob/f4e6ff043eef6686a9013750ffcb8ab66926b9d4/.github/workflows/release.yml#L104-L118)加入 0.10.25，并在[原测试](https://github.com/LIghtJUNction/ssmcp/blob/f4e6ff043eef6686a9013750ffcb8ab66926b9d4/scripts/test_release.py#L120-L140)加入对应枚举，将未延后对照改为 0.10.26；原 rc 对照和其他断言保留。

主审已完整读取发布策略回执和原 stderr：只运行现有 `DeploymentPolicyTests`，11 条逐项通过，unittest 计时 **2.587 秒**，外层步骤计时 **2.778227721020812 秒**、退出 0；这不是整个 `test_release.py` 测试集的运行结论。回执记录 P 的源码身份与四个输入文件前后一致，没有运行 Cargo 或产品二进制，没有获取共享 target 锁，也没有触发 CI、发布或部署。原正常推送回执时间为 01:23:37.381104 UTC，记载退出 0、无 force、远端由 B 推进到 P；源码冻结工作树保持 B。回执及 stdout/stderr 的完整字节数和 SHA256 见[事实记录](release-input-lock-review-0122.json)。原作者整体进程退出状态未被本报告提升为主审原件复核结论。

### InputLease：测试含义与尚未确定的归因

作者报告了最后 `is_some` 断言失败，主审尚未读取该次原失败日志。固定 B 的 [`try_input`](https://github.com/LIghtJUNction/ssmcp/blob/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1/src/computer/host_control/storage.rs#L218-L228)只打开 `input.lock` 并尝试非阻塞平台锁，不检查 enabled、ticket 或 state.json；若失败位置确为所报断言，则 `None` 只界定输入锁 busy，不能推导成 off 后本来就应返回 None，也不是 state 的两秒等待路径。[原测试](https://github.com/LIghtJUNction/ssmcp/blob/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1/src/computer/host_control/tests.rs#L185-L220)把 lease 保留到 operation 结束、内部 guard 销毁之后：此时先断言 None，再 `drop(lease)`，最后断言 Some。该顺序与测试目的相符，最后 Some 断言应保留。

B 的 InputLease 只含 `File` 和 `Arc<Store>`，没有自定义 Clone、Drop 或显式 unlock。[Linux flock 文档](https://man7.org/linux/man-pages/man2/flock.2.html)说明锁属于打开文件记录：fork/dup 的描述符副本共享锁，任一副本显式解锁即可释放，否则需要所有副本关闭。[fork 文档](https://man7.org/linux/man-pages/man2/fork.2.html)确认描述符继承；[O_CLOEXEC](https://man7.org/linux/man-pages/man2/open.2.html)作用于 exec，不能排除 fork 到 exec 之间的持有窗口。这说明继承副本是一条可能路径，尚无该失败实例的 FD 或进程证据，不能称为已证实根因。P 未改相应产品源码，该静态边界保持不变。

### Windows 显式释放必须匹配原加锁区间

固定 B 的 [LockFileEx 调用](https://github.com/LIghtJUNction/ssmcp/blob/be6da5bf1d9da74e8fd0c19576e63e8aacf4c1c1/src/platform_windows.rs#L267-L297)与 [Rust 1.100.0 的 File::unlock](https://github.com/rust-lang/rust/blob/1.100.0/library/std/src/sys/fs/windows.rs#L486-L499)使用不同的偏移和长度。下表的 MAX 为 `u32::MAX = 4294967295`。

| 调用 | OffsetLow | OffsetHigh | LengthLow | LengthHigh |
| --- | ---: | ---: | ---: | ---: |
| B 的 LockFileEx | MAX | 0 | 1 | 0 |
| Rust File::unlock → UnlockFile | 0 | 0 | MAX | MAX |

微软 [UnlockFile](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-unlockfile) 与 [UnlockFileEx](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-unlockfileex)文档要求释放区间与已有锁精确匹配，更大区间覆盖那一个字节并不满足条件。Rust 该实现的第一次 UnlockFile 失败会直接返回，第二次调用对 ERROR_NOT_LOCKED 的容忍不能补救第一次范围不匹配。因此，如后续给 InputLease 增加显式释放，不能无条件照搬 File::unlock，必须核对平台加锁与解锁的对应关系。这是实现约束，**不是当前 Windows 已泄锁的证据**；本次没有执行 InputLease 复现或修复验证，也没有把 Linux 的可能路径写成实际故障归因。

### 后续公开修复来源（01:33 UTC）

主审已通过 GitHub 提交对象和完整比较确认：[`d4c4b9c6`](https://github.com/LIghtJUNction/ssmcp/commit/d4c4b9c6fe09a4d7b246ca54a08f5c7b258289fb) 的 tree 为 `c64ea74cfb85474f05dd2c8bb3c834c73d10b08e`，在公开 f33 之上有十个提交。其末尾文档提交的父提交是 [`10fdc1c1`](https://github.com/LIghtJUNction/ssmcp/commit/10fdc1c10888c59799bb98908911033bacc3f046)，再前一个为 [`224c2338`](https://github.com/LIghtJUNction/ssmcp/commit/224c2338b3170ebdfe4dde4d4835e5dedb813194)；这两步分别包含实际释锁修复和新回归，不能只读末尾文档差异就遗漏修复。

该来源的 [`storage.rs`](https://github.com/LIghtJUNction/ssmcp/blob/d4c4b9c6fe09a4d7b246ca54a08f5c7b258289fb/src/computer/host_control/storage.rs)新增 Unix 限定的 InputLease Drop：先尝试显式解锁，遇 Interrupted 重试；其他错误结束循环并继续字段析构，不能据此声称异常分支已成功解锁。新增 Unix 回归通过 `try_clone()` 保留描述符副本，要求原 lease 析构后即可取得下一份 lease，并验证随后关闭旧副本不会释放新 lease 的独占锁。源码未把该 Drop 引入 Windows，原取消测试和最终 Some 断言保留。

这里的结论是公开源码及窄范围静态复核。新回归针对共享描述符生命周期，仍不是原失败现场的 FD 取证；本报告未独立核验该来源的运行成绩。该修复尚待与准备提交正常合流，再以新的固定源码完成构建和运行验收。
