# SSMCP 0.10.21 迭代记录

> 工作稿：发布验收和实际发布结果尚待填写，不能作为已经发布的证明。

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
| 真实终端性能脚本 | 已编写 6 个场景；AST 与独立审查通过，尚未执行 |
| 真实用户内容 | 未纳入公开补丁或性能样本；测试全部使用合成输入 |
| 提升幅度 | 尚无可比较的实测结果，不报告加速比 |

六个场景覆盖 2,738 字节多行 UTF-8、64 KiB、恰好 4 MiB、超限拒绝、16 KiB 普通输入，以及工具持续输出期间的粘贴。脚本分别记录最后一次 PTY 写入与首个可见、真正可用的引用；写入 PTY 完成不代表客户端已读完或保存完成。比较基线和候选必须使用相同的九个测试辅助文件，并由外层回执证明源码、锁文件和二进制身份。驱动器中的 source revision 字段本身只是调用方声明。

另一个已观察到的因素是磁盘提交延迟。同一设备上三次合成 2,738 字节写入，写入本身分别约为 0.027、0.465、0.057 毫秒，而 fsync 分别约为 10,842.582、290.753、1,088.451 毫秒。这只是三个环境诊断样本，不能作为产品基线或百分位统计；后续压力读数已经降低，也不能据此把全部 I/O 归因于某一个任务。[环境记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/paste-environment-io-observation.json) SHA256 为 `2ba20189795d410f6a9d905f2b5260a5f779c219cc595fd9ba0cb0f0df03ce20`。

控制流复核确认：保存运行在异步后台，界面以非阻塞方式检查结果，所以可以继续输入、删除和移动光标；普通 Enter、`/send` 和问题答案提交仍等待保存任务完成。等待期间按下的 Enter 不会自动重放。用户改动草稿后，保存完成会保留新草稿并提示已保存引用，不能把“可以继续编辑”理解为“引用一定自动加入变更后的草稿”。本次补丁保留 fsync、私有权限、八行自动引用策略及 InputStamp 保护，不能单凭 CPU 路径优化宣称持久化长尾已经消失。

先前一次只读剪贴板诊断的调用虽然超时，后来按原任务 ID 找回了完整结果：原生任务退出 0，总时长 51 毫秒，其中列出剪贴板格式用时 1.15 毫秒。该探针没有读取剪贴板内容；当前格式样本也不是最初的文本粘贴，因此不能用它反推原始故障原因，或将调用超时算成剪贴板读取耗时。详见[回收的诊断回执](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/clipboard-format-probe-recovered.json)。


### 测量入口与基线构建

终端粘贴和 Ctrl+V 需要分别测量。带终端粘贴标记的字节经过此次批处理路径；Ctrl+V 则由剪贴板适配器取得完整文本，直接进入异步保存。两条路径都会执行文件持久化，不能把终端字节处理的改善直接当作 Ctrl+V 的加速。现有剪贴板功能测试没有覆盖八行以上纯文本的性能样本，正在补充独立的 2,738 字节 Ctrl+V 用例：真实客户端只接收一个触发字节，私有模拟 wl-paste 提供合成内容。它检验实际客户端适配器与保存路径，不代表真实系统剪贴板性能。

实测前还发现临时目录位置会改变测量对象：默认 /tmp 是 tmpfs，用户工作区位于 /home 的 btrfs。基线与候选必须把测试目录放到与工作区相同的文件系统，并记录实际位置、st_dev 和文件系统类型；内存盘结果不足以证明磁盘持久化延迟已经改善。btrfs 子卷的 st_dev 与 mountinfo 设备字段可能不同，文件系统类型按路径所属挂载点判断。[目录与文件系统记录](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/paste-benchmark-filesystem-observation.json)保存原始观察及范围。

修复前基线 `b156254b45c015a3994b8be1475b423167ac980f` 的 library fixture 编译实际失败，Cargo 返回 101：时间模块不能访问仅限上级模块的 invalidate，以及测试给需要 Path 的参数传入了字符串。完整输出只有这两个编译错误，未生成绑定该源码的根包可执行产物，后续 release bins 和 decoder 构建尚未进行。因此 b156 不能作为性能比较基线。[独立失败核验](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/baseline-b156-failure-review-20261120.json)保留源码、依赖、日志与精确时间；失败记录不会改写为成功。

截至本检查点，唯一集成负责人已形成新的本地提交 `c53c5ea681cb0840f1c41c9e8d31489abac4fd37`：仅将 invalidate 的可见性改为 crate 内可用，并在原有 TTY 测试中显式构造 Path，两处各一行。已读完整差异及提交回执，Cargo.lock 摘要保持 `6c76ff5b75b161aeb691ebc6be129ba719aae82b09c8c0d50ae42f6d817c604d`。格式检查记录通过；新提交的编译成功、可执行产物和测速结果仍须等待实际构建回执。

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

[独立控制器审查](https://github.com/LIghtJUNction/ssmcp/blob/docs/command-await-delivery-20261120/docs/reports/command-await-delivery-20261120/independent-review-final25-2e917-v1.json)明确保留剩余门禁：Gate10 adapter 尚未纳入冻结包，实际通过的 gate、外层 launcher 与最终源码/依赖/二进制身份仍需接通。当前控制器中的旧 r3 provenance 不能认证 Gate10。25 套产品验收、单独的 P1 集成与粘贴性能样本也尚不能写成已通过。

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
