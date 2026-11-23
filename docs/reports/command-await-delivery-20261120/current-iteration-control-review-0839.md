# SSMCP 0.10.25：源码增量已结束，三处控制问题待修

本轮 E9 源码阶段已于 **2026-11-21 08:30:20 UTC** 正常结束，最终提交为 **`27e9733fa539c400c532474e076971eba03f845a`**，目录树为 `205138cdce8368956a4e96ed87df3285f9f5011f`。五次普通推送退出码均为 0，回执记录工作树与子模块干净。它包含离线依赖版本树、统一 Computer Use On/Off、内部 Git 依赖刷新，以及写帧边界和夹具清理修正。[E9 完成、窄检查与待修事实](source-E9-completed-control-review-public-facts.json)

**106 项发布策略测试及 `cargo check --tests --locked --offline --jobs 1` 已在该工作树上通过；仍有三处控制问题需要源码修复。** 最后检查前后的源码与输入指纹一致，回收完成，但没有运行 Rust 回归、实际桌面操作或最终 35 阶段验收。CDP 锁顺序、Playwright 已发送但未获回复的结果处理，以及 Omarchy 清理时的锁生命周期尚未闭合。

E8 源码阶段已于 **2026-11-21 07:42:19 UTC** 正常结束，三次普通推送退出码均为 0，最终提交为 **`f6ca9d0d1c1a1a088c03f14fa7c7f785732c386b`**，目录树为 `4852c9a5c2d928e46ae4b291a4d27e5229998a18`，工作树与子模块干净。其中发布配方固定于父提交 `879ba17`。[E8 结束回执与源码事实](source-E8-completed-public-facts.json)它在此前三次 Git 依赖与本地启动改动之后，加入了实际的 `git-source-v1` 发布流程：六个平台的 GitHub 安装包使用已验证的 Git 来源，crate 发布单独延期。[发布源码与合同事实](source-E8-release-contract-public-facts.json)

**最终候选仍未固定，整套运行验收尚未开始。** E7、E8、E9 的源码阶段均已结束，剩余窄修继续由同一原作者处理。此前 W 的通过结果和失败记录继续归属于 W。

## 本轮实际提交

| 提交 | 已提交的改动 | 当前验证范围 |
|---|---|---|
| [7c1cd94](https://github.com/LIghtJUNction/ssmcp/commit/7c1cd94e08636617f4fded83f86048d3c012c7c3) | 本地输出目录被拒绝时停止反复重连；补充工作区、历史和网页打开失败诊断 | 源码、静态记录；没有运行测试 |
| [a53a0c5](https://github.com/LIghtJUNction/ssmcp/commit/a53a0c5047833ded61193a0b0ce548937e89e959) | 匿名账户夹具完成服务端退出后再进入下一场景；保存脱敏失败上下文 | 原断言保留，尚未执行新脚本 |
| [1a2f759](https://github.com/LIghtJUNction/ssmcp/commit/1a2f7591ad0abb53907c048dcaed028434e17c47) | 四项直接 Git 依赖去掉固定版本选择器；zuse 锁项和子模块同步到 0.3.0 来源 | 回执记录依赖解析成功；没有编译或产品验收 |
| [9e1a06c](https://github.com/LIghtJUNction/ssmcp/commit/9e1a06c4c6a04fe6a7ac7919c9cee9ab497ac95b) | 补充错误类型生命周期和初始输出清理阶段的分类 | 当时新增断言的字符串调用已在 71851cf 修正；没有把后续修正记成旧提交已通过 |
| [879ba17](https://github.com/LIghtJUNction/ssmcp/commit/879ba17fc3ba4bad6a26a85494b79e29acf1938a) | Git 源二进制发行、独立 registry 导出及旧标签发布分支 | 已读取固定源码并静态审阅；尚无新流程运行结果 |
| [f6ca9d0](https://github.com/LIghtJUNction/ssmcp/commit/f6ca9d0d1c1a1a088c03f14fa7c7f785732c386b) | 在既有 HTTP/WebSocket 测试中扩展同账号双会话元数据乱序完成检查 | 回归定义已提交，尚未运行；没有新增独立 Rust 测试项 |
| [71851cf](https://github.com/LIghtJUNction/ssmcp/commit/71851cf221076e661b5c243a63d585fb1b04fdaf) | 离线依赖版本树、早期版本命令路由，以及 boxed error 断言修正 | 四个文件完成静态核对；三个新回归定义尚未运行 |
| [2619686](https://github.com/LIghtJUNction/ssmcp/commit/26196862691774cab333e1f110b2fdb9c15e5b97) | 统一 Computer、CDP Browser、Omarchy 和 Playwright 的控制开关，迁移配置及界面文案 | 固定源码静态审阅发现四项问题；其中一项后续已补修，其余三项待修 |
| [43fde19](https://github.com/LIghtJUNction/ssmcp/commit/43fde1904cd37b006f033b07682068ebab7bb7be) | 更新 zharness、zplugin 的 gitlink 及五个包的 Git 锁来源 | 固定差异与解析回执字段已读；不是实际版本树命令的执行结果 |
| [8ce982c](https://github.com/LIghtJUNction/ssmcp/commit/8ce982cd2f37e3bbb3c6a218a7388fefdbf0c7e5) | Playwright 等待 writer 锁后再次检查 permit；另恢复两个构造方法的非测试可见性 | 写帧前复验缺口已静态闭合，未执行撤权复现 |
| [27e9733](https://github.com/LIghtJUNction/ssmcp/commit/27e9733fa539c400c532474e076971eba03f845a) | 夹具显式调用其自有 Playwright 清理方法 | 最后两项窄检查通过；原断言保留，Rust 夹具未运行 |

E7 三次提交依次基于 PR #164 合并点 **P2 `3f678a7`**，形成 P2 → 7c1cd94 → a53a0c5 → 1a2f759 的连续提交链。源码事实表包含完整 commit、tree、差异范围和执行边界。E7 三次提交一共涉及 12 个路径；E7 三次普通推送退出码均在回执中记录为 0，公开 GitHub 提交已直接读回。

## 内部依赖使用 Git 最新来源

用户已明确要求：项目之间直接依赖 Git 最新提交，对外发布时再使用版本号。本轮移除了 `zuse`、`ztuio`、`zharness`、`zplugin` 四项直接 Git 依赖的精确 registry 版本字段，保留原 canonical Git URL、包别名及 feature，没有加入固定 `branch`、`tag` 或 `rev` 选择器。

Git 声明决定更新时从哪里获取源码，锁文件记录这次实际解析出的提交。二者共同保证开发依赖可以更新，也能准确复现一次构建。

| 项目 | 较早检查点 | E7 源码检查点 |
|---|---|---|
| zuse 包版本 | 0.2.2 | 0.3.0 |
| Cargo.lock 中的 zuse revision | `a1bb8fd2cbce0a9d85134999883c578358065242` | `830fcefd2277cbba46536c67d206f29058ba7fb3` |
| `crates/zuse` gitlink | 同上 | 与本轮 Cargo.lock revision 相同 |
| 另外三个 gitlink | 既有值 | E7 阶段没有发生差异；后续 E9 变化见下表 |
| 直接依赖的版本选择器 | 四项 `version = "=…"` | 已移除；实际包版本仍由 manifest 和锁文件记录 |

06:40:59 UTC 读到的上游默认分支仍为 a1bb，是当时有效的历史观察。随后上游于 06:46:06 UTC 合并 PR #2，新的默认分支实际读到 830fcefd；本轮采用后一个来源。原时间快照没有改写。[历史观察](zuse-update-source-observation.json) · [后续来源及用户规则](git-latest-dependency-policy-public-facts.json)

源码回执记录以下命令退出码均为 0：针对 zuse 的精确提交更新、`cargo fetch --locked`、`cargo metadata --offline --locked --format-version 1`。这证明回执记录了依赖解析和获取成功，不能当作编译、测试或完整依赖图独立复核通过。本次报告提取没有再打开其引用的 1,485,599 字节 metadata 原文、全部默认分支观察文件或同锁核验文件。[解析记录与读取范围](source-E7-startup-account-git-public-facts.json)

E9 又在固定 `43fde19` 更新了实际锁定来源。源码回执记录 **08:19:47–08:19:49 UTC** 对 11 个 Git 仓库默认分支的观察，四项直接依赖对应如下。这里是该次观察与构建锁的对应关系，不保证远端之后不会继续更新。

| 直接依赖 | E9 锁定提交 | 相对 E7 |
|---|---|---|
| zuse 0.3.0 | `830fcefd2277cbba46536c67d206f29058ba7fb3` | 保持不变 |
| ztuio 0.1.0 | `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e` | 保持不变 |
| zharness 0.6.0 | `fb2948f78467dd92b216aabda6f18a5645f0e7f6` | 更新锁项与 gitlink |
| zplugin 0.3.1 | `592a1be623b81a775026c470e6f0efe91fcf78f3` | 更新锁项与 gitlink |

实际差异涉及 zcall、zharness、zmcpio、zplugin 和 zplugin-macros 五个包；各包版本号没有因这次 Git revision 更新而改动。`Cargo.lock` 仍为 66,476 字节，新 SHA256 为 `0f213ad0db728a3448d70009141375711b098f4e9fd69b1334cc8217746e4f14`。本报告读取了源码回执内的观察、feature 与 checkout 身份字段，并核对公开提交差异；没有再次打开其引用的 44,984 字节 resolver 原件或运行 Cargo。[E9 依赖快照及读取范围](source-E9-completed-control-review-public-facts.json)

## 本地启动和 Linux 多用户处理

当本地输出目录无法创建、打开或通过所有权与权限检查时，新源码会携带具体目录、操作和底层 I/O 错误，并停止 watchdog 对这种确定性本地拒绝的反复重连。瞬态传输错误仍按原 watchdog 设置处理。工作区解析错误也会说明失败步骤和路径；本地记忆失败时，不再笼统保证历史和草稿可用。

这些改动补充了失败处理与诊断。原有普通 Unix 输出仍按有效用户身份分到 `/tmp/ssmcp-<有效 UID>`，终端回退仍验证控制终端身份和前台进程组。新增及扩展的回归用例已经写入源码，本轮尚未执行。[实现及回归定义](source-E7-startup-account-git-public-facts.json) · [已有隔离机制](linux-multiuser-source-findings.json)

现场只读记录发现了一个系统候选：`/usr/bin/ssmcp` 的 ELF 大小和摘要与包管理器记录相符，登记版本为 `0.9.1.r377.gee05244-1`。报错用户当时实际选中的 PATH、wrapper、会话固定入口及版本仍未知，因此没有把这个候选直接认定为报错程序。

工作区位于另一用户的 0700 私有 HOME 内，且未返回 access ACL；普通用户不能遍历这条路径。观察者不能读取另一用户的私有 HOME 则符合正常隔离，不能据此认定该用户自己的 HOME 不可用。此次没有改用户目录权限或安装程序。[现场观察、推断及未确定项](linux-multiuser-runtime-observation-public-facts.json)

仓库已有双 UID 输出和继承 PTY 驱动，使用预构建库测试产物执行两个显式 ignored worker。它与完整登录流程的覆盖范围不同；本轮仍待原执行者记录实际结果，未计入已完成的 W 检查。

## 账户验收场景同步

原 W 首轮界面验收在 `new-main:another_subject_cannot_list_connection` 停止。保存的原记录没有实际 HTTP 状态和响应体，后来一次 69 项定点诊断又未复现，所以旧失败的具体原因仍未确定。

这次源码修正解决已发现的场景边界：关闭本地匿名 peer 并确认其 worker 退出后，再等待该确切 client ID 从服务端列表消失，然后进入后续 self-only 场景。本地 socket 关闭与服务端清理完成是两个不同事件，下一场景现在明确等待后一个事件。

原完整 `HTTP 200 且 clients == []` 条件及已知 client ID 跨账户调用的 `404` 条件均保留，没有过滤掉其他仍在列表中的会话。失败时会保存脱敏、限长的状态和响应；清理会尝试所有步骤并保留主异常。此次仅修改验收夹具，没有修改产品 ACL，也没有声称新脚本已经通过。[固定差异及边界](source-E7-startup-account-git-public-facts.json)

## 已合并的元数据与网页命令

[PR #164](https://github.com/LIghtJUNction/ssmcp/pull/164) 已于 06:41:36 UTC 合并。它加入当前已保存对话的 OpenAI 调用上下文，以及明确触发的网页和剪贴板操作；这些功能包含在本轮后继源码中，运行验收尚待完成。

| 命令 | 已提交的行为 |
|---|---|
| `/openai`、`/openai-session`、`/openai-subject` | 查看当前对话观察到的调用标识、来源、缺失状态和观察时间 |
| `/chatgpt set <URL>` | 保存用户明确提供的真实会话网址 |
| `/chatgpt open` 或 `/chatgpt` | 请求本机浏览器打开保存的网址 |
| `/chatgpt copy`、`/chatgpt clear` | 复制网址，或仅删除当前对话保存的网址 |
| `/plugin-docs` | 打开官方 Plugins 参考 |

官方文档中的 session、subject 是匿名关联标识；本实现把它们与真实网页地址分别保存，不从匿名标识拼接对话 URL。浏览器启动请求、实际网页加载和用户在网页发送“继续”也分别处理。新启动诊断提交还补充了打开失败时的复制或手动打开提示。[官方字段说明](https://developers.openai.com/plugins/reference#_meta-fields-the-client-provides) · [固定命令说明](https://github.com/LIghtJUNction/ssmcp/blob/1a2f7591ad0abb53907c048dcaed028434e17c47/docs/commands.md) · [已合并源码事实](source-openai-caller-C-C2-public-facts.json)

f6 的回归定义使用同一 bearer 的两个重叠 HTTP 调用和实际 WebSocket Frame，再通过生产 CallScope/sidecar 接口记录各自捕获的历史 owner；当前选择 B 时先完成 B、再完成 A，检查元数据仍归属原会话。它使用 fixture Reply，并非完整客户端连接循环或真实 exec.wait 作业的端到端运行。现有断言保留，该定义尚未执行。[准确范围](source-E8-completed-public-facts.json)

## 原现场 socket 与独立的受控实测

用户要求测试的原 socket 路径在 **06:59:32 UTC** 实际预检时不存在，记录为 `FileNotFoundError / ENOENT`，应用发送字节为 0。因此原会话的交付测试尚未完成。[原预检事实](live-input-socket-preflight-public-facts.json)

随后，原执行者在新建的隔离本地会话中实际完成一次 socket 实测，完成回执观察时间为 **07:25:28 UTC**。使用的是当前 PATH 入口报告的 `ssmcp 0.10.25`，大小为 15,751,344 字节，前后 SHA256 均为 `54651b3fd9611210a838ffa5ddc1e032dc40d7b71ea2cf163487e4b32085c7b9`；该二进制尚未绑定到 W 或最终候选源码。

本次有 **1 次产品启动、1 次 socket 发送、1 次确定性 Python fixture 调用、0 次模型调用**。76 字节原文完整保留，缺失的 file 和 skill 各返回一条 unavailable 警告；写端半关闭后收到 1 条 JSON、EOF 和 `status=completed`。客户端及外层进程均退出 0、已回收，清理无错误，端点已不存在。[实测结果与四份原件摘要](current-entry-socket-smoke-public-facts.json)

这是该隔离本地入口的一次真实通过结果，不表示测到了先前已消失的在线 socket，也不计入 W 的 18 项运行检查、UI、P1 或最终源码验收。报告仅包含结构化回执与摘要，未公开原始认证日志。

`--input none` 的现场报错仍是另一项未闭合结果：客户端未得到 relay 对关闭人工聊天能力的确认，启动退出 1。当前没有生产端能力确认的在线通过回执；本地 socket 测试不能证明这一模式已修复。服务器部署仍延期，也没有通过自动退回人工输入来绕过该要求。

## 已有验收与当前未执行范围

| 范围 | 实际对应源码 | 结果 |
|---|---|---|
| E7 源码与依赖解析 | 1a2f759 | 源码阶段退出 0；解析、格式和差异检查按回执记录；未编译或执行产品测试 |
| E8 源码阶段 | f6ca9d0；发布配方 879ba17 | 阶段退出 0、三次推送和静态核对完成；尚无编译、测试或发布运行结果 |
| E9 版本树与统一控制源码 | 71851cf → 27e9733 | 源码阶段结束，五次推送；13 个新增或扩展 Rust 回归定义尚未执行 |
| E9 最后窄检查 | 27e9733 工作树，前后源码指纹一致 | 106 项发布策略测试及 cargo check --tests 通过；不是 Rust 测试运行或完整产品验收 |
| 当前 PATH 的隔离 socket 实测 | 0.10.25／摘要 54651b3f，源码尚未绑定 | 1 次发送、1 次 fixture 调用、完整回执及清理通过；原指定会话仍未测 |
| 原 35 阶段检查 | W / r14 | 全部通过；包括 2,057 个隔离库测试、6 个宿主库测试、101 个普通 integration、1 个显式 worker，以及另行执行的 14 个 ignored 测试 |
| 真实进程与传输 | W | 12 项隔离客户端与 6 项传输检查通过 |
| 首轮完整界面流程 | W | 24 套中启动 7 套：6 通过、1 失败、17 未执行；通过套件共 503 条记录 |
| 单次账户诊断 | W 产物 | 69 条通过；未复现旧失败，未加入 503 |
| 本轮完整构建、UI 与 P1 | 最终候选待固定 | 尚未执行 |
| 正式发布与公开产物核验 | 最终来源待绑定 | 尚未完成 |

W 失败 producer 中的 61 条 true 与 1 条 false 单独保留，未并入 503；`human_finish` 仅由 `interactions` 覆盖。W 首轮未执行 `session_groups` 和 `composer`，所以没有用旧 V 的通过记录补齐 W。剩余 ignored 项与多用户驱动也按实际运行边界记录。

上述事实分别见 [W 构建检查](gate-checkpoint-W-r14-completed.json)、[W 运行检查](runtime-W-r14-public-facts.json)、[W 首轮界面失败](ui24-W-account-compat-first-public-facts.json)和[单次诊断及补存退出回执](account-compat-diagnostic-W-public-facts.json)。W、V、U 的详细过程保存在[此前完整报告](https://github.com/LIghtJUNction/ssmcp/blob/a363927a414d122ac562e42187cab1257f47b3d8/docs/reports/command-await-delivery-20261120/current-iteration-W-r14-0539.md)。

## 已提交的发布适配

此前开发 manifest 中的四项内部 Git 依赖已经去掉 registry 版本选择器。E8 现在从已检出的 Git 包和锁文件读取实际版本，正式对外需要 registry 包时，再派生单独的版本 manifest、锁文件与包验证流程。开发依赖来源与外部包格式分别处理。[实际发布脚本](https://github.com/LIghtJUNction/ssmcp/blob/879ba17fc3ba4bad6a26a85494b79e29acf1938a/scripts/release.py)

0.10.25 的正式 GitHub 发行采用 `git-source-v1`，保留六个平台 archive、六份 sidecar、两个安装器、SHA256SUMS 与 release-manifest.json，共 16 个公开附件。manifest 的八条 artifact 记录只对应六个平台 archive 与两个安装器。crate 状态明确为 `deferred`，`verified_crate` 为 null；这与服务器部署延期分别记录。

finalize 生成 `verified-source`，记录 commit、tree、manifest、Cargo.lock、子模块、依赖图和工具链；另以 `finalized-release-assets` 保留最终 16 个文件。publish_release 必须下载这两份具名产物，重新计算并比较已有 manifest 与 SHA256SUMS 的原始字节，随后才发布。这样可以追查公开下载文件与完成验证的产物是否一致。

静态工作流合同仍有 19 个 job，但本轮预期为 **17 个成功、publish_crates 与 deploy_server 两个跳过**。发布核验必须检查八份必要 Actions artifacts：六份平台构建、verified-source 和 finalized-release-assets；其他 CI 测试证据可以另外存在。上述数量是新源码的要求，**尚不是实际运行结果**。[固定合同及读取边界](source-E8-release-contract-public-facts.json)

旧版不可变标签通过明确的 legacy-tag-v1 分支处理；0.10.25 不能悄悄回退到旧流程。registry 路径仍保留独立的版本映射、归一化锁图、checksum、包字节和 dry-run 校验，本轮不会把二进制发行冒称为 crates.io 发布。较早对 zuse 0.3.0 registry API 的 404 仍只是当时的观察。[依赖来源观察](git-latest-dependency-policy-public-facts.json)

## H2 发布核验器

H2 已按实际 E8 配方完成静态适配、AST 解析和独立源码复核，未发现阻断性合同错配。三份模块的字节摘要及修改差异均已记录，**没有 import 或运行核验器，没有执行测试、Cargo、产品、dispatch 或正式发布**。[H2 使用说明与文件](publication-verifier-git-source-unbound/README.md) · [静态准备身份表](publication-verifier-git-source-unbound/preparation-identity.json)

新模块要求 17 次实际初始 checkout，并按 upload/download 的具体步骤、运行编号和 artifact ID 核对八份发布产物。六个平台 archive 与公开下载逐字节相等，最终附件 ZIP 的 16 个成员也必须全部对应公开文件；两个 installer 还须与最终 S 的脚本原字节相等。原始 dispatch 解析器保持原字节。

[原 H 核验器](https://github.com/LIghtJUNction/ssmcp/tree/2257fb1d235bba59e8a0d1747a4f4afc6348bad3/docs/reports/command-await-delivery-20261120/publication-verifier-manual-main-unbound) 继续保留。H2 仅处理本轮 0.10.25 / manual-main / git-source-v1 / crate 延期合同；legacy、registry 和 build-only 不属于它的通过范围。

最终产品提交 S、main 工作流提交 P、tag、run 和原始请求回执均未绑定。校验器将要求 S 是 P 的真实祖先，并核对 S/P 中六份发布配方与事先审阅的固定字节；不能仅靠相同版本号或相同目录树推断提交关系。SHA256 用于核对字节，实际执行仍须由原始工具回执、run/attempt、step 与 artifact ID 证明。

## 依赖版本树与统一控制开关

用户要求的 `ssmcp --version --all` 已在 71851cf 提交。它从二进制内嵌的构建期 Cargo.lock 显示从 ssmcp 可达的依赖树，按 name、version、source 区分包；重复身份、歧义和缺失依赖会报错，共享节点和环分别标记。完整字符串生成成功后才输出，普通 `--version` 继续只输出应用版本与换行。

输出与文档明确说明：锁图包含构建、开发和各目标的依赖，实际链接代码仍随目标与 features 改变。已审阅的版本处理代码不在运行时读取工作区的锁文件、查询网络或执行 Cargo。版本树本身没有给出主仓 commit 或完整锁文件摘要，仍须结合发行 manifest 和源码回执判断构建来源。该增量完成独立静态审阅，后继 27e9733 已通过测试目标的类型检查；版本命令和三个新回归仍未实际执行。[版本树固定源码](source-E9-version-tree-public-facts.json) · [后继检查记录](source-E9-completed-control-review-public-facts.json)

统一 Computer Use On/Off 已在 2619686 进入源码，覆盖 SSMCP 内的 Computer host/isolated、CDP Browser、Omarchy，以及启用时的 Playwright 控制和变更路径。合同要求 Off 阻止新控制，并取消尚未发送的旧请求；再次 On 不恢复旧 generation 的 ticket。只读观察、自有资源清理与默认 isolated 路由保留，已经送达的动作不能承诺回滚。

固定源码审阅发现的四项问题按后继提交分别记录：

| 路径 | 问题与当前状态 |
|---|---|
| CDP 同页操作 | direct input 先取输入租约再取 page 锁，act 路径顺序相反；并发操作可互相等待。仍待源码修复，未执行复现。 |
| Playwright 写入队列 | 8ce982c 已在取得 writer 锁后、创建半帧清理 guard 和首次写入前重新检查 permit。固定 27e9733 调用链静态核对闭合；无 permit 的普通 RPC 检查为空操作。尚无运行验证。 |
| Playwright 已发送但无回复 | On 状态下的该错误分支仍需如实记录未知或部分完成、禁止盲目重放，并保留实际确认回复。仍待源码修复。 |
| Omarchy 取消与清理 | 外层取消可能使输入租约和串行锁先于异步 shutdown 完成而释放。仍待源码修复。 |

上述问题来自固定源码的静态可达分析，没有执行复现。写入队列修正也不意味着 Off 与首字节发送已经成为跨进程原子操作。深层取消仍可以关闭当前操作拥有的 Playwright driver，不能把发送层写前拒绝不污染通道扩大为永不清理 driver。[问题状态及来源](source-E9-completed-control-review-public-facts.json)

持久状态与配置迁移的窄审没有发现额外的旧 Off 自动开启或旧 ticket 复活问题；旧浏览器 focus_policy 只迁移展示设置，不写入控制开关。现有 e2e_computer_controls 定义覆盖 CLI、双终端状态同步、generation、草稿/快捷键和重启后 Off 持久性，没有执行真实的四类后端控制动作。另有一处“Background browser control remains enabled”旧错误文案已交原作者修正；它是文案错配，不是已证实的开关绕过。

这个开关约束应用内入口，同一系统用户的 Shell、独立程序以及本机切换命令仍拥有原有主机权限。当前源码明确 `os_permission_boundary=false`，不承诺阻止同 UID 任意程序直接控制桌面。要建立系统边界，需要让 agent 无法直接取得桌面权限，由唯一受控入口执行获准动作；该方案仍在准备，当前没有修改生产账户、权限或安装隔离服务。

## E9 检查记录与后续整合

原作者从 **07:58:56 UTC** 开始 E9，于 **08:30:20 UTC** 结束。原退出回执已完整读取，源码回执按整件字节计算摘要并读取了提交、依赖、检查尝试和未执行项等字段；没有宣称逐条独立审阅全部 199 条源码命令记录。

最后一次检查从 **08:25:39 UTC 至 08:28:21 UTC**，两个实际命令均退出 0，源码和输入前后保持一致，自有进程回收完成。发布策略 stderr 明确记录 `Ran 106 tests` 与 `OK`，Cargo 输出明确完成 dev profile 的检查。策略夹具中的模拟发布文本不表示发生了真实 tag、crate 发布或 GitHub Release。13 个新增或扩展 Rust 回归定义尚未执行。

七次检查尝试全部保留：一次作者调用身份预检失败、两次 ProcessLookupError 停止、一次源码变化保护停止，以及三次窄检查通过。较早两次通过仅按各自记录的工作树指纹归档；本报告未将其独立核定为 71851cf 或 8ce982c 不可变提交的验收，也不转移给 27e9733。最后提交仅引用第七轮检查。此前 07:49:51 UTC 的 E9 启动锁预检失败也继续保留，它没有启动作者模型或执行 Cargo。[完整状态与原件摘要](source-E9-completed-control-review-public-facts.json) · [启动历史](source-E9-launch-public-facts.json)

[发布说明审阅草稿](release-notes-review-unbound/README.md)已单独交付，正文、补丁、固定原件及证据复核均已推送并逐件读回。草稿按 2619686 的状态冻结；交付说明列出后续依赖刷新、写帧修正与三项剩余问题，原作者整合时须按最新事实更新，不能直接套用到已变化的文档。

接下来由同一原作者完成三项控制窄修、旧文案和命令兼容性收尾，再正常合并。从实际 main 合并结果选定最终 S 并核对 tree、差异和父链，之后由原执行者完成构建、真实进程、多用户、完整界面与 P1 验收，再核对正式发行及公开产物。即使合并发生 squash，也先固定实际合并提交再运行最终验收，不把旧成绩移给新提交。H2 的六份固定发布配方在 E9 改动路径中均未变化，核验器仍未执行。

[Google Docs 当前报告](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit)
