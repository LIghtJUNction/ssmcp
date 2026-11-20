# SSMCP 0.10.21 迭代记录

> 工作稿：发布验收和实际发布结果尚待填写，不能作为已经发布的证明。

## 交付内容

本轮围绕终端可读性、跨任务等待和可核验的完成流程推进，另同步实际使用的 Project Z 依赖并清理四个已经合并的远端分支。各功能先在独立提交上验证，再合入主线；最终发布结果需要绑定最终候选提交，不能用某个功能分支的测试代替。

### 命令直接可读

相邻命令各自显示实际 argv，带有运行状态和 shell 语法颜色。输出默认展示最后三行；长内联 Python / JavaScript 用紧凑标题保持布局可读。详情入口保留脱敏命令、执行目的和已记录输出，支持独立滚动。命令预览超过保留上限时提示 `/command JOB_ID`；对仍保留的本地任务，该入口可读取完整的脱敏 argv。任务不再保留时显示明确提示。查看或展开历史记录不会重新执行命令。

实现和证据见 [PR #125](https://github.com/LIghtJUNction/ssmcp/pull/125) 与 [固定提交上的验证报告](https://github.com/LIghtJUNction/ssmcp/blob/77b80b138033dd91e7e4c6b6a4a8bb70d31678c5/docs/reports/readable-commands-20261120/validation.json)。该功能的产品源码为 `6f251674269875455cca148299f9ddcf97a8cb5c`；后续提交补充测试及证据，没有改变这份产品实现。

### `/send` 引用补全

消息正文支持 `@` 文件和 `$` 技能补全，保留 Unicode 与缩进。当引用候选已加载且有选中项时，Enter 或 Tab 接受候选而不发送；接受候选并收起列表后，再次 Enter 执行显式发送。加载期间保留草稿。找不到引用时仍发送原正文，并附带不可用引用警告。接收者和群组路由保持各自的解析边界。

实现见 [PR #116](https://github.com/LIghtJUNction/ssmcp/pull/116)，导航夹具修复见 [PR #118](https://github.com/LIghtJUNction/ssmcp/pull/118)。[归档证据](https://github.com/LIghtJUNction/ssmcp/tree/dddcd322dec710c0c537c95fae178ac8d9811307/docs/reports/send-reference-completion-20261120)明确区分产品源码、测试脚本及发布候选。

### 等待与人工结束批准

公共 `await` 和 JavaScript `waitFor` 统一观察六类目标：执行任务、relay 任务、协作成员状态、自己的协作收件、用户输入和已有结束审批请求。普通目标可按任一满足或全部满足聚合；用户输入与结束批准保持单独事务。单轮观察的 `timeout_ms` 预算最多 25 秒；在原身份、客户端连接和等待记录仍有效时，可用同一服务器签发的 wait ID 恢复。这不是整个 HTTP 请求耗时的保证。取消只取消观察，不终止被观察的任务，也不确认消息或批准结束。

结束许可必须来自真人明确选择 Yes，并且匹配当前请求、成员与线程；agent 的 Yes、普通 ready / done、超时都不能替代。许可只能消费一次。输入仅在最终外层工具响应的同步提交边界消费，交付以外层 `user_wait.status = received` 及匹配的 `user_messages` 为准；这不表示远端已读。取消先于该提交时不消费；最终交付降为 `timed_out` 或 `busy` 时，仍可恢复同一 wait ID。

实现见 [PR #130](https://github.com/LIghtJUNction/ssmcp/pull/130)，[验证记录](https://github.com/LIghtJUNction/ssmcp/blob/9702be43cac38af51758731baf7fb31c11633170/docs/validation/await-finish-20261120.md)绑定实际受测源码 `a6b1e29ddc2bac1e730efbfd063e46843731df92`。

这个约束作用于经过 SSMCP 的完成接口。宿主若完全停止调用 SSMCP，SSMCP 无法拦截宿主自己的外部 final。公共 `await` 和 `waitFor` 均需要更新后的 relay。公共 `await` 不占 JavaScript 执行槽，`code_mode` 内的 `waitFor` 仍占用所在执行槽；原生被动 finish 观察需要支持相应操作的客户端。

## 依赖维护

`zharness` 从 0.5.1 升到 0.6.0，固定源码 `db4206d3b74e37d57ff052948962f55514081d4b`，带入 `zcall 0.4.0` 和 `zguard 0.1.0`。本轮按仓库身份核对 12 个实际 Project Z 包，保持精确版本、gitlink 和 Cargo.lock 一致。每轮迭代及发布前重新核对最新未撤回稳定版的规则已经写入[依赖维护文档](https://github.com/LIghtJUNction/ssmcp/blob/ec91a0a9b3109438eba795e7cc107c5865da3828/docs/dependencies.md)。

`zcall 0.3.0` 仍由 `ssmcp → zuse-computer 0.2.2 → zmcpio 0.4.0 → zcall =0.3.0` 保留。这个例外来自已发布上游的精确约束；要消除它，需先有兼容的 zmcpio 和 zuse 正式版本，再由 SSMCP 更新消费者。新的 zharness 调用链已经使用 0.4.0。详细版本、摘要和消费者检查见 [PR #120](https://github.com/LIghtJUNction/ssmcp/pull/120)。

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
| Project Z 消费者 | 锁定解析、真实动态库契约 1 项、发布规则 91 项通过 | PR #120 的依赖更新 |
| 严格 Clippy | 命令显示、await、账号修复三份受测源码分别通过 | `cargo clippy --workspace --locked --all-targets -- -D warnings` |

这些计数来自不同范围，不相加冒充一套完整发布验收。Cloud 的完整 native 路由验证受宿主网络命名空间能力限制；所需完整链路已在开发机运行，没有移除隔离要求。归档截图使用合成测试内容，并经过实际视觉检查。

### 首轮整合发布预检

[Run 44754296839](https://github.com/LIghtJUNction/ssmcp/actions/runs/44754296839) 绑定 `4c6178eb811049ce1fce4b6d16803e0e4f7b410d`，结论为失败。六个平台构建、四项 macOS / Windows 客户端检查和两项 Linux 安装运行验证通过；两个 Linux 检查暴露了退出登录重连竞态与日志界面旧测试合同。后续 finalize / 发布步骤跳过。该结果保留为失败证据，不能用成功的构建部分宣称候选通过。

Canvas 修复已通过 [PR #140](https://github.com/LIghtJUNction/ssmcp/pull/140) 合入主线。测试改为验证新的三行命令预览与详情入口，从实际显示的相邻栏读取动态计数，并精确比较 Tasks 的三项有序 argv。保留原始第 0 项失败和第一阶段通过 48 项后遇到旧 Tasks 文案的失败；最终完整 69 项在 14.609 秒内通过，前后二进制摘要一致。[机器报告及截图](https://github.com/LIghtJUNction/ssmcp/tree/6af1287f4130edcc47cf05bb1003f1f439038f8f/docs/reports/canvas-command-contract-20261120)绑定精确产品源码和测试脚本；未改产品代码，也没有削弱输出、滚动、草稿、登录拒绝或终端恢复的检查。

账号修复通过 [PR #142](https://github.com/LIghtJUNction/ssmcp/pull/142) 合入：写入先报错时，客户端先以同一个两秒预算读取剩余结束消息，防止尚在队列中的授权失效通知被直接丢弃；收尾不会执行排队工作。只有协议中已知的永久拒绝可以替换原始错误，普通 EOF、读取错误、普通拒绝和超时继续保留原错误，主动停止和重载保持各自路径。

修复后的真实开发机批次通过 20 项 Rust 回归和完整 47 项账号检查，后者耗时 13.74 秒。被撤销客户端按约定以 `1` 退出，连接次数为 **2 → 2**；重新授权只发起一次，旧凭据已删除。严格 Clippy 退出码为 0，耗时 191.969 秒。没有放宽原断言；新增测试固定消息交错顺序，原夹具只补实际数值诊断。[机器回执与边界说明](https://github.com/LIghtJUNction/ssmcp/tree/053fc99a0d4c81e4e6ac3bc3a36d65935595267c/docs/reports/account-logout-revocation-20261120)区分产品提交 `b4d648b`、受测提交 `d417a27` 和只添加证据的提交 `053fc99`。单独下载原制品的本机基线未执行，原始失败证据来自真实 CI。

### 合并修复后的预检

该次冻结候选为 [`c7086f9545602f45399e7a237995a48440e7d8aa`](https://github.com/LIghtJUNction/ssmcp/commit/c7086f9545602f45399e7a237995a48440e7d8aa)，包含 #140、#142 和完整发布说明 #138。独立核对确认各 PR 的最终文件摘要、版本、四个固定子模块和发布策略一致。重新查询官方 registry 时，12 个 Project Z 包的最新稳定版没有变化。

[预检 44762565095](https://github.com/LIghtJUNction/ssmcp/actions/runs/44762565095) 使用空 tag、`build_only=true`、`deploy_server=false`，最终为失败，18 个任务中 10 个成功、3 个失败、5 个跳过。两个 Linux 架构的账号退出检查、Canvas 69 项及 Composer 87 项均通过；新的 Linux 阻塞在随后的问题交互夹具，它在等待成员列表同步之前查找成员。Windows ARM 浏览器烟雾检查另抛出信息不足的 `MethodInvocationException`；安装脚本、Node 文件与前一成功记录相同，目前不能据此认定具体安装故障。安装运行验证与 finalize 因前置失败跳过，未发布标签、包或资产。

[PR #147](https://github.com/LIghtJUNction/ssmcp/pull/147) 已补足 Windows smoke 的有界诊断：异常类型、数值错误码以及白名单函数位置，不复制原始消息、路径、参数或 stderr。Linux PowerShell 7.6.10 上的 53 项检查通过，真实 smoke 的非 Windows 保护仍以 `1` 退出且 `ok=false`。这项改动保留了安装逻辑、成功条件、期限与清理规则，不能单凭这些本地结果宣称 Windows ARM 的原始安装故障已修复。[三份正式报告对照及验证回执](https://github.com/LIghtJUNction/ssmcp/tree/5846caf22a4456d3309bcb929744f5996e5aebb4/docs/reports/windows-browser-diagnostics-20261120)记录相同的 Node、helper 与 installer 摘要，以及本地验证的适用范围。

精确成员等待的修复首先在产品源码 `c7086f9` 与夹具 `8e76841` 上验证：原 Questions 在 17 项后复现 `StopIteration`，修复后完整通过 43 项；报告内运行时间为 12.885 秒，外层进程为 12.993 秒。同一批次的 InputHistory 在 140 项后发现真实产品焦点缺陷：在 History 页输入 `/archived` 后，列表刷新但焦点仍在编辑框，Home 与 Enter 无法按文档恢复归档。后续 [PR #146](https://github.com/LIghtJUNction/ssmcp/pull/146) 显式把成功打开的历史选择器切到列表焦点，保留恢复后返回输入的行为。产品源码 `ae8ede2a53b802899d57edcfccac933a9909eea3`、库测试与 PTY 夹具 `4221d5594b8ee0bd1fb9ea1d9783d0bb1bebd3f7` 已完成 4 项归档回归与完整 150 项 InputHistory 检查；外层 InputHistory 进程耗时 32.629 秒，产品二进制哈希在测试编译及 PTY 前后保持一致。缺少测试二进制环境变量造成的第 0 项失败、一次新增测试的私有字段访问编译错误，均单独记录，不作为产品运行结果。

### 更大范围整合的候选

[PR #133](https://github.com/LIghtJUNction/ssmcp/pull/133) 随后把统一 Tools 组、保留折叠与滚动、会话目标、启动更新提示、被动等待总结和真人问题的原历史归属等改动合入 `407084c4fcc582f5fef01b772de838f04e2d5484`。这份源码已包含精确成员等待，但尚不包含 PR #146 的历史焦点修复。前述旧功能分支上的结果不能改标为这份整合源码的验证。

该提交的[原生门禁公开记录](https://github.com/LIghtJUNction/ssmcp/pull/133#issuecomment-7044314246)为 1880 项通过、5 项失败、22 项忽略、4 项筛除，阻塞涉及诊断与分组可见性、滚动、阅读导航和输入帧预算断言。[固定提交中的原始失败摘录与摘要](https://github.com/LIghtJUNction/ssmcp/blob/4d15c3a2f2846adaaf7ddf7c1166e5a014afcbbd/docs/reports/ssmcp-zcalld-iteration-20261120/gate8-failed407-root-review.json)保留失败来源；独立整合修复继续处理。[正式预检 44769140252](https://github.com/LIghtJUNction/ssmcp/actions/runs/44769140252)也已失败。最终候选还需包含这些窄修复、历史焦点修复，以及绑定同一源码的组合交互验收与成功发布预检。

本次预检中 [Windows ARM 构建与真实浏览器 smoke](https://github.com/LIghtJUNction/ssmcp/actions/runs/44769140252/job/136402117933)已成功：53 项 bootstrap 检查和 30 项实际安装检查通过，worker 退出 0，真实 smoke 耗时 53.85 秒，清理完整。官方 Node 文件、SHASUMS、helper 与两个 installer 摘要和上一轮失败记录一致。这证明本次安装成功，不能反推旧 `MethodInvocationException` 的具体原因，也没有证据要求修改安装逻辑；同一 run 的 Windows ARM Cargo 回归失败属于另一个检查任务。

历史焦点的产品修复随后通过 `3431054facad9809748567a13b292c8cc7773b67` 合入；[PR #149](https://github.com/LIghtJUNction/ssmcp/pull/149) 再以纯文档改动归档[精确来源的报告与截图](https://github.com/LIghtJUNction/ssmcp/tree/f635e48e1ce287cbfd1deeb97a462409592e642a/docs/reports/questions-history-readiness-20261120)，并补充发布说明。文档合入提交为 `47b9ca9bb83646c8fa8678a095e7dacdd3783572`，没有重新运行或改标旧测试。

后继主线还包含 [PR #151](https://github.com/LIghtJUNction/ssmcp/pull/151) 的七项直接依赖要求及 HTTP 配置封装。该提交是迁移检查点；调用点调整、锁文件解析和实际执行结果仍待收齐。此工作稿不将尚未完成的迁移或 24 套/P1 控制器准备包计作最终验收通过。

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

现有生产 relay 和活动客户端保持运行，未在本轮切换。发布包与生产进程是否已经加载该版本是两个独立事实，交付记录应分别报告。
