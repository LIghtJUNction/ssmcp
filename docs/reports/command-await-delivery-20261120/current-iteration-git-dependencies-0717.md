# SSMCP 0.10.25：Git 依赖已更新，继续发布适配与完整验收

本轮已分三次提交并推送，源码检查点为 **`1a2f7591ad0abb53907c048dcaed028434e17c47`**，目录树为 `cb24179edb9900c591afdabfe1b4678c2d50c5f9`。改动包括本地启动错误处理、账户验收场景同步，以及四项内部 Git 依赖声明和 zuse 更新。执行回执确认这轮源码工作于 **2026-11-21 07:08:21 UTC** 正常结束，工作树干净。[本轮源码与回执事实表](source-E7-startup-account-git-public-facts.json)

**新的源码检查点还没有编译或执行完整验收。** 发布脚本仍需适配“内部项目使用 Git 最新来源、对外发行使用版本号”的规则；最终候选尚未固定。此前 W 的通过结果和失败记录继续归属于 W，未移用为新源码的成绩。

## 本轮实际提交

| 提交 | 已提交的改动 | 当前验证范围 |
|---|---|---|
| [7c1cd94](https://github.com/LIghtJUNction/ssmcp/commit/7c1cd94e08636617f4fded83f86048d3c012c7c3) | 本地输出目录被拒绝时停止反复重连；补充工作区、历史和网页打开失败诊断 | 源码、静态记录；没有运行测试 |
| [a53a0c5](https://github.com/LIghtJUNction/ssmcp/commit/a53a0c5047833ded61193a0b0ce548937e89e959) | 匿名账户夹具完成服务端退出后再进入下一场景；保存脱敏失败上下文 | 原断言保留，尚未执行新脚本 |
| [1a2f759](https://github.com/LIghtJUNction/ssmcp/commit/1a2f7591ad0abb53907c048dcaed028434e17c47) | 四项直接 Git 依赖去掉固定版本选择器；zuse 锁项和子模块同步到 0.3.0 来源 | 回执记录依赖解析成功；没有编译或产品验收 |

三次提交依次基于 PR #164 合并点 **P2 `3f678a7`**，形成 P2 → 7c1cd94 → a53a0c5 → 1a2f759 的连续提交链。源码事实表包含完整 commit、tree、差异范围和执行边界。本轮一共涉及 12 个路径；三次普通推送退出码均在回执中记录为 0，公开 GitHub 提交已直接读回。

## 内部依赖使用 Git 最新来源

用户已明确要求：项目之间直接依赖 Git 最新提交，对外发布时再使用版本号。本轮移除了 `zuse`、`ztuio`、`zharness`、`zplugin` 四项直接 Git 依赖的精确 registry 版本字段，保留原 canonical Git URL、包别名及 feature，没有加入固定 `branch`、`tag` 或 `rev` 选择器。

Git 声明决定更新时从哪里获取源码，锁文件记录这次实际解析出的提交。二者共同保证开发依赖可以更新，也能准确复现一次构建。

| 项目 | 较早检查点 | 本轮源码检查点 |
|---|---|---|
| zuse 包版本 | 0.2.2 | 0.3.0 |
| Cargo.lock 中的 zuse revision | `a1bb8fd2cbce0a9d85134999883c578358065242` | `830fcefd2277cbba46536c67d206f29058ba7fb3` |
| `crates/zuse` gitlink | 同上 | 与本轮 Cargo.lock revision 相同 |
| 另外三个 gitlink | 既有值 | 本轮没有发生差异 |
| 直接依赖的版本选择器 | 四项 `version = "=…"` | 已移除；实际包版本仍由 manifest 和锁文件记录 |

06:40:59 UTC 读到的上游默认分支仍为 a1bb，是当时有效的历史观察。随后上游于 06:46:06 UTC 合并 PR #2，新的默认分支实际读到 830fcefd；本轮采用后一个来源。原时间快照没有改写。[历史观察](zuse-update-source-observation.json) · [后续来源及用户规则](git-latest-dependency-policy-public-facts.json)

源码回执记录以下命令退出码均为 0：针对 zuse 的精确提交更新、`cargo fetch --locked`、`cargo metadata --offline --locked --format-version 1`。这证明回执记录了依赖解析和获取成功，不能当作编译、测试或完整依赖图独立复核通过。本次报告提取没有再打开其引用的 1,485,599 字节 metadata 原文、全部默认分支观察文件或同锁核验文件。[解析记录与读取范围](source-E7-startup-account-git-public-facts.json)

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

## 原现场 socket 与独立的受控实测

用户要求测试的原 socket 路径在 **06:59:32 UTC** 实际预检时不存在，记录为 `FileNotFoundError / ENOENT`，应用发送字节为 0。因此原会话的交付测试尚未完成。[原预检事实](live-input-socket-preflight-public-facts.json)

随后，原执行者在新建的隔离本地会话中实际完成一次 socket 实测，完成回执观察时间为 **07:25:28 UTC**。使用的是当前 PATH 入口报告的 `ssmcp 0.10.25`，大小为 15,751,344 字节，前后 SHA256 均为 `54651b3fd9611210a838ffa5ddc1e032dc40d7b71ea2cf163487e4b32085c7b9`；该二进制尚未绑定到 W 或最终候选源码。

本次有 **1 次产品启动、1 次 socket 发送、1 次确定性 Python fixture 调用、0 次模型调用**。76 字节原文完整保留，缺失的 file 和 skill 各返回一条 unavailable 警告；写端半关闭后收到 1 条 JSON、EOF 和 `status=completed`。客户端及外层进程均退出 0、已回收，清理无错误，端点已不存在。[实测结果与四份原件摘要](current-entry-socket-smoke-public-facts.json)

这是该隔离本地入口的一次真实通过结果，不表示测到了先前已消失的在线 socket，也不计入 W 的 18 项运行检查、UI、P1 或最终源码验收。报告只提取四份结构化原件；原始认证日志没有读取或公开。

## 已有验收与当前未执行范围

| 范围 | 实际对应源码 | 结果 |
|---|---|---|
| 本轮源码与依赖解析 | 1a2f759 | 源码阶段退出 0；解析、格式和差异检查按回执记录；未编译或执行产品测试 |
| 当前 PATH 的隔离 socket 实测 | 0.10.25／摘要 54651b3f，源码尚未绑定 | 1 次发送、1 次 fixture 调用、完整回执及清理通过；原指定会话仍未测 |
| 原 35 阶段检查 | W / r14 | 全部通过；包括 2,057 个隔离库测试、6 个宿主库测试、101 个普通 integration、1 个显式 worker，以及另行执行的 14 个 ignored 测试 |
| 真实进程与传输 | W | 12 项隔离客户端与 6 项传输检查通过 |
| 首轮完整界面流程 | W | 24 套中启动 7 套：6 通过、1 失败、17 未执行；通过套件共 503 条记录 |
| 单次账户诊断 | W 产物 | 69 条通过；未复现旧失败，未加入 503 |
| 本轮完整构建、UI 与 P1 | 最终候选待固定 | 尚未执行 |
| 正式发布与公开产物核验 | 最终来源待绑定 | 尚未完成 |

W 失败 producer 中的 61 条 true 与 1 条 false 单独保留，未并入 503；`human_finish` 仅由 `interactions` 覆盖。W 首轮未执行 `session_groups` 和 `composer`，所以没有用旧 V 的通过记录补齐 W。剩余 ignored 项与多用户驱动也按实际运行边界记录。

上述事实分别见 [W 构建检查](gate-checkpoint-W-r14-completed.json)、[W 运行检查](runtime-W-r14-public-facts.json)、[W 首轮界面失败](ui24-W-account-compat-first-public-facts.json)和[单次诊断及补存退出回执](account-compat-diagnostic-W-public-facts.json)。W、V、U 的详细过程保存在[此前完整报告](https://github.com/LIghtJUNction/ssmcp/blob/a363927a414d122ac562e42187cab1257f47b3d8/docs/reports/command-await-delivery-20261120/current-iteration-W-r14-0539.md)。

## 发布适配与接下来的工作

现有发布脚本仍要求开发 manifest 上携带精确 registry 版本，与当前内部 Git 声明冲突；外发归一化代码也直接读取旧字段。因此发布适配需要同时处理开发来源校验、外发版本映射和包内容证明。本轮三个提交未修改发布配方，下一增量仍由原作者处理，并补齐类型标注和初始清理阶段的错误分类。

发布协调方已明确下一增量的合同：本轮保留六个平台的版本化 GitHub 二进制、安装器及 16 个公开资产；crate 发布显式延后，且与服务端部署延后分开记录。finalize 与 GitHub 发布阶段将使用重新核验的 `verified-source` 产物，不把它冒称为 `verified-crate`。registry 路径仍保留独立的版本映射、checksum、包字节和 dry-run 校验。**这是已确认的实施方向，尚待实际新源码和运行回执证明完成。** [发布合同记录](publication-plan-git-source-deferred-crate-public-facts.json)

zuse 0.3.0 的 Git 源已存在；较早对 registry 版本 API 的实际读取为 404。源码可用、构建输入已锁定、外部包已发布是三个分别记录的状态。后续发布范围和依赖可用性，以新工作流和真实回执为准。[来源与发布状态观察](git-latest-dependency-policy-public-facts.json)

[原 manual-main 核验器](https://github.com/LIghtJUNction/ssmcp/tree/2257fb1d235bba59e8a0d1747a4f4afc6348bad3/docs/reports/command-await-delivery-20261120/publication-verifier-manual-main-unbound) 已静态交付，尚未运行。其旧版本假设也需要按实际新发布配方对齐；最终产品提交 S、main 工作流提交 P、tag、run 和原始请求回执均未绑定。新的静态工作副本已准备，不以旧核验器的完成状态代替新流程验证。

用户新增的 `ssmcp --version --all` 依赖版本树已纳入后续增量，尚未实现或验证；控制快捷键与直接运行 zuse CLI 的实际权限边界也在只读核查。

接下来完成发布适配，固定最终源码和依赖，由原执行者按原流程进行构建、真实进程、完整界面与 P1 验收，再核对正式发行及公开产物。每一阶段继续记录实际来源、失败和未执行项。

[Google Docs 当前报告](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit)
