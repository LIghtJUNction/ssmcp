# 0.10.25 发布路线与源码身份：只读复核

复核时间：2026-11-21 02:11 UTC。本文记录源码与平台规则，不表示已经创建标签、触发工作流或发布制品。旧 publication verifier 和 v25 UNBOUND 副本均未修改、未执行。

## 当前前置条件

当前公开 main 的复核快照为 **P = `f33ed8f14fc14aff05a2e4098743eb07e8527c06`**。此前候选 **C = `d29dae7e2b9ff6794f17f3c96c0cc37977936084`** 的本地验证流程在库测试阶段失败，未完成整套验收。后继 **Q = `647bce2ec9cf74beab43adea831c2e0712d28ea8`** 已冻结，唯一变化是前台模式测试显式声明 `--input terminal`；Q 的最终运行结果仍待取得。

P 尚未纳入 C/Q，且 P 的发布工作流没有 0.10.25 的持久延后部署记录。因此，当前源码不能直接视为已具备可执行的正式发布路线。若后续选择 main 上的手动发布，应先完成候选验证、让 main 纳入最终候选及 deferred25，再按实际的新 main 提交重新核对工作流身份。[P 工作流](https://github.com/LIghtJUNction/ssmcp/blob/f33ed8f14fc14aff05a2e4098743eb07e8527c06/.github/workflows/release.yml) · [C→Q 单行差异](https://github.com/LIghtJUNction/ssmcp/commit/647bce2ec9cf74beab43adea831c2e0712d28ea8)

## 旧工具的 push-only 条件来自哪里

现存最早的校验器前身为 9,531 字节、SHA-256 `d8042c6448f8d0b35c543b879bbe9bb913be3ab5fb7fd72a93157ba96a1682c8`。其第 50–51 行已经要求运行 head 等于候选，并要求 `event=push`、`head_branch=tag`。2026-11-20 18:10:52 UTC 的离线合同记录登记了它与 a238 后继的身份；该时间是验证记录时间，不是已知的最初编写时间。

随后 [90a83b0b 的路径参数版](https://github.com/LIghtJUNction/ssmcp/blob/90a83b0b74d7553d9c34470c96f5a3233d984b74/docs/reports/command-await-delivery-20261120/publication-verifier/README.md)明确保留这项约束。可归属的实施记录属于助手设计并继承的工具合同；本次可恢复材料没有证明“用户只允许 tag push”。不能因为文档反复继承同一句条件，就将其升级为用户禁令，也不能据此宣称历史正式发布已经验证了 tag-push 路线。

这不改变旧工具的实际行为：它仍会拒绝 `workflow_dispatch`。如果采用另一条发布路线，需要独立、完整的身份核验设计；不能只删除旧工具的事件与 head 断言。

## 跳过标记与事件的边界

C/Q 提交带有 `[skip ci]`。GitHub 的跳过规则覆盖 `push` 和 `pull_request`，而 tag push 属于 `push`。官方资料没有给出 tag 豁免，也未细化“只创建一个指向已有提交的 tag”时如何选择提交消息。因此，这是需要核实的触发条件；本轮没有实际 tag push 结果，不能记为已触发或已跳过。[跳过规则](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/skip-workflow-runs) · [事件定义](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push)

`create` 和 `workflow_dispatch` 是独立事件，不属于该跳过指令范围。但本项目的 `create` 验证入口只接受 `release-check` 分支创建，tag 创建不满足条件。凭据引起的事件抑制是另一项条件；本轮没有实际凭据证据，不把未触发归因为 `GITHUB_TOKEN`。

## main 手动发布需要核对两份身份

| 身份 | 表示什么 |
| --- | --- |
| workflow 定义提交 P | dispatch 时 main 的工作流及相对路径 reusable 定义；决定步骤、条件和内联部署政策 |
| tag 源码提交 S | 输入 tag 解析后，实际 checkout 并经 validate 输出的提交；决定产品源码及 checkout 中的脚本 |

手动运行的 `run.head_sha` 和 `head_branch` 描述触发该运行的 main 提交与分支，不能直接当成输入 tag 的源码身份。后续 checkout 不会把工作流定义改成 tag 里的 YAML。相对路径 reusable 使用调用方同一提交的定义。[GitHub 上下文](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts) · [reusable 语法](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_iduses)

现有源码链的约束如下。表中的 S 以输入 tag 已被校验为该提交为前提。

| 作业 | 源码或制品来源 |
| --- | --- |
| validate | checkout 输入 tag，递归子模块和完整历史；验证 tag commit 等于 HEAD、版本一致、HEAD 属于 main 祖先链、主仓受跟踪文件无改动、子模块精确且无未提交或未跟踪源码 |
| checks 六个平台 | reusable 定义来自 P；传入并 checkout 已校验的 S |
| build 六个目标 | checkout S，运行其中的编译、打包和安装脚本 |
| Linux runtime 两个目标 | checkout S，读取同一 run 的构建 artifact |
| finalize | checkout S，再次验证 tag，生成 verified crate 和 manifest |
| publish_crates / publish_release | checkout S，读取同一 run 的对应 artifacts，再次核对 tag 后发布 |
| deploy_server | 没有 checkout；使用 P 的 reusable 定义，并将校验后的 tag/SHA 传入部署过程 |

来源：[P release.yml](https://github.com/LIghtJUNction/ssmcp/blob/f33ed8f14fc14aff05a2e4098743eb07e8527c06/.github/workflows/release.yml)、[P ci.yml](https://github.com/LIghtJUNction/ssmcp/blob/f33ed8f14fc14aff05a2e4098743eb07e8527c06/.github/workflows/ci.yml)、[P deploy-server.yml](https://github.com/LIghtJUNction/ssmcp/blob/f33ed8f14fc14aff05a2e4098743eb07e8527c06/.github/workflows/deploy-server.yml)、[源码校验函数](https://github.com/LIghtJUNction/ssmcp/blob/d29dae7e2b9ff6794f17f3c96c0cc37977936084/scripts/release.py#L397-L412)。本轮 P/C 的 `release.py`、`ci.yml` 和 `deploy-server.yml` 各自字节相同；Q 的单行测试变化没有改变这些文件。

## 延后部署不能只看 tag 文件

P 的内联 deferred 表覆盖 0.10.15–0.10.23；C/Q 另加入 24 和 25。main dispatch 即使 checkout C/Q，实际执行的内联政策仍来自 P。显式 `deploy_server=false` 会阻断本次部署；但对 P 而言，省略输入或传入 true，在其余发布校验通过时会允许 0.10.25 部署。因此，显式 false 的单次效果不能替代 main 中持久的 deferred25 记录。[实际政策](https://github.com/LIghtJUNction/ssmcp/blob/f33ed8f14fc14aff05a2e4098743eb07e8527c06/.github/workflows/release.yml#L69-L122)

政策测试还有不同的读取位置：`test_release.py` 从当前 checkout 的 `release.yml` 提取政策。main dispatch checkout C/Q 后，测试验证的是 tag 文件；它不能证明 P 的实际内联政策也包含 deferred25。[测试读取位置](https://github.com/LIghtJUNction/ssmcp/blob/d29dae7e2b9ff6794f17f3c96c0cc37977936084/scripts/test_release.py#L71-L134)

## 如采用独立 dispatch 核验，必须保留的证据

应分别固定工作流定义 P、最终 tag 源码 S 和实际 formal run，再核对运行事件、main ref、部署政策、每个实际 checkout/build、同一 run 的 artifact 身份，以及 tag、manifest、crate 和发布资产的一致性。manifest 中的 source commit 绑定 verified crate，不能单独证明六个平台二进制的编译来源；还需要真实构建日志与 artifact 对应关系。

成功且延后部署时，现有源码拓扑仍预期 19 个 job：18 成功、deploy_server 跳过；资产仍为 16 个，manifest 中为 8 条 artifact 记录。这些是未来核验条件，不是已有运行结果。顶层工作流按 tag 分组，但 `publish_release` 共用队列；正式发布前仍须协调另一发布线的实际状态，不能取消他人的运行或假定队列空闲。

本次只读复核未触发发布、未更改已有校验器，也未把 C 的失败或其他提交的成功移记为 Q 的运行结果。

## 02:28 UTC 补充：实际证据与可执行合同

Q 的35项本地 Gate、外层退出与五份构建产物回执已取得，见[Q 检查点](gate-checkpoint-v25-0227.json)。UI夹具仍有待原作者修正的旧状态条件，最终候选 S 尚未确定。02:28 UTC 回读的 main 仍为 f33；未来 P 不能预先填成当前 main。

[GitHub 当前 dispatch API](https://docs.github.com/en/rest/actions/workflows?apiVersion=2026-03-10#create-a-workflow-dispatch-event)在固定版本 `2026-03-10` 下成功返回200，包含 `workflow_run_id`、`run_url` 与 `html_url`。新的请求证据应保存实际 endpoint、版本、发送的脱敏 body（明确 main、tag、deploy_server=false、build_only=false）及实际响应，用服务端返回的 run ID 绑定。GET run 不提供原始 inputs；政策输出 false 或 deploy skipped 也不能反推请求明确传了 false。本地回执是由操作方保存的原始工具证据依赖，其摘要只能证明保存内容一致，不能单独证明请求确实发出。

已通过只读接口取得两份历史 attempt 原始日志 ZIP，未启动工作流：

| 历史运行 | 原始ZIP字节数 | SHA-256 | 实际布局 |
| --- | ---: | --- | --- |
| [v0.10.20成功run44710769033](https://github.com/LIghtJUNction/ssmcp/actions/runs/44710769033)，attempt1 | 969117 | `eb901970ee3d7e3a9728afbd883188d40229d8b7f36d13a117176bfa552cb1b2` | 18份整job日志＋18份system.txt，无独立step文件 |
| [v0.10.23失败run44843331041](https://github.com/LIghtJUNction/ssmcp/actions/runs/44843331041)，attempt1 | 1070804 | `6325fa49806e18c1a63cf81b383ca5223c2b0b86d0fb60cb33c9db4e6fcdb94f` | 15份已执行job日志＋15份system.txt，无独立step文件 |

system.txt记录服务端条件求值及runner排队信息，没有步骤内容边界。顶层文件数字是排列序号，不能当成job ID。因此后继核验应以固定attempt的jobs API给出的真实job ID读取日志，并核对最早的固定 checkout、有序输入与唯一commit输出；不能扫描全日志，发现后来补印的匹配SHA就接受。后续构建、上传、下载还应结合精确步骤状态、已审配方、artifact ID/name/size/digest及实际ZIP字节，把内层发布包接到manifest与公开asset。

这一合同信任 GitHub runner、固定actions和已审P/S配方，不宣称能抵抗恶意runner或恶意构建程序。缺失、错值、重复或无法唯一归属的证据应拒绝通过。外层Actions ZIP摘要与内层tar.gz/zip摘要分别核对，不能要求两者相同，也不能依日志中并发下载摘要的出现顺序配对。

v20成功运行只提供旧流程的日志格式及跨job artifact ID桥接样本。它当时的verified-crates只包含crate，未包含当前的JSON源码回执，publish_release也没有现代receipt下载步骤。独立后继仍保留当前合同，不为了让历史样本通过而删除现代crate/source/tree检查。代码仅在新目录静态准备，旧两版保持原字节；本说明不表示后继已经执行或正式发布通过。
