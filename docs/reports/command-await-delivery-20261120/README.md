# SSMCP 0.10.21 迭代记录

> 工作稿：发布验收和实际发布结果尚待填写，不能作为已经发布的证明。

## 当前状态（2026-11-20 21:55 UTC）

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
