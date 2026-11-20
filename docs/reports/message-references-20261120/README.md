# SSMCP 迭代验收与部署状态 · 2026-11-20

## 结论与当前状态

本轮完成互发消息回执、精确收件人广播、`@文件` 与 `$技能` 引用、强制发送收件人保护，以及 zuse-computer 0.2.2 接入修正。产品源码固定在 **b39a8e96c7234b6fb097d388e97614a2102b9810**；后续 **5033cf3e3e89d24a133f18a6b2861b49a70cfc0e** 仅修改验收脚本与测试夹具，产品源码未改变。

本地默认 Rust 测试合计 **1709 项通过、0 项失败**。真实终端、HTTP/Unix 监听程序和 GTK 私有桌面验收全部通过。**生产激活仍待安排：在线中继是 0.10.13，新版客户端的授权策略被旧服务拒绝；11 月 20 日 15:00（台北时间）左右的快照确认至少有 4 个原生执行任务正在运行。**

本报告区分安装文件、运行进程与服务器版本。新功能的端到端通过记录来自本轮隔离验收，不能视为生产中继已经部署。

| 层次 | 最后核验结果 |
| --- | --- |
| 本轮独立候选安装 | 三个程序均为 0.10.15，SHA256 与受测 release 完全一致 |
| 共享 Cargo 安装入口 | 复查时为 0.10.14，Cargo 记录指向主工作区；本轮曾成功安装并核验 0.10.15，随后该入口发生另一次安装改写 |
| 恢复后的项目连接 | `ssmcp-message-references-compat-20261120`，实际进程为 0.10.13，已连接且原生执行验证成功 |
| 生产中继 | `ssmcpd 0.10.13`，服务运行中，尚未切换 |
| 本轮 0.10.15 新建连接 | 授权握手不兼容，已停止该新会话的重试 |

代码位于 [PR #111](https://github.com/LIghtJUNction/ssmcp/pull/111)，基于 `integrate/v0.10.15-20261120`；本轮没有将候选合入 main，也没有发布 SSMCP 0.10.15 稳定 Release。机器可读证据见 [validation.json](validation.json) 和 [deployment-status.json](deployment-status.json)。

## 1. 文件与技能引用

发送分为用户正文和引用对象的补充信息。找不到引用对象时，正文仍然成立。因此，缺失、不可读、越界或无法展开的引用转为明确的 `unavailable` 记录与提示，正文继续发送；不会伪造文件内容、预览或可用的读取动作。

- `请创建 @future-missing.txt`：文件尚不存在也能发送。
- `参考 @"资料/说明.txt"，并使用 $reference-fixture`：可用引用继续附带真实信息。
- `$"007-helper"`：支持需要引号的技能名称。
- `$100`：作为金额文字保留，不按技能解析。

引用提示在发送被确认后显示；未知结果、重试和恢复保留原始正文与已准备的数据。正文恰好达到 16 KiB 时，不可用引用的额外元数据不会挤掉正文，相应提示留在本地回执。有效附件不能为了凑字节数被静默丢弃；正文自身超过既定上限仍会报错。

真实终端验证了直接键入、技能选择、有效/缺失引用混合、原文逐字接收和 16 KiB 边界，共 **53 项检查、8 张真实 VT 帧**。原生参与者收件箱继续遵循其纯文本消息语义；引用展开验收覆盖普通用户请求队列与 `/send` 监听通道。

实现：[引用准备](https://github.com/LIghtJUNction/ssmcp/blob/b39a8e96c7234b6fb097d388e97614a2102b9810/src/client/references.rs) · [补全](https://github.com/LIghtJUNction/ssmcp/blob/b39a8e96c7234b6fb097d388e97614a2102b9810/src/client/completion.rs) · [验收回执](evidence/references.json)

## 2. 互发消息与广播

回执按已取得的事实前进：已确认的消息 ID 不能被替换，较晚的错误不能抹去此前确认；已进入接收方流程的消息在断线后保留该阶段与断线事实。草稿仅在匹配的确认到达且用户没有编辑新内容时清空。

新增 `messages.broadcast(clientId, {recipient_ids, text, request_id})`。一次指定 1–32 个精确、互异的收件人，返回逐个结果，允许明确的部分成功。去重绑定完整收件人集合与正文，同一请求不能被改成另一批收件人；子投递 ID 与预留/插入共同处理，避免重试造成已确认消息重复发送。它不宣称外部副作用恰好执行一次。

该接口属于新中继实现。当前生产 code_mode 没有导出这组新消息 helper，因此这里交付的是已实现、已测试的候选接口；生产使用需要中继升级。

实现：[消息协议说明](../../thread-messaging.md) · [广播逻辑](https://github.com/LIghtJUNction/ssmcp/blob/b39a8e96c7234b6fb097d388e97614a2102b9810/src/messaging/broadcast.rs) · [中继广播](https://github.com/LIghtJUNction/ssmcp/blob/b39a8e96c7234b6fb097d388e97614a2102b9810/src/broker/messaging/broadcast.rs)

## 3. 聊天框监听接口

候选已实现 `ssmcp.ui.listener` 的 `open/read/ack/close/status`、网络等待入口，以及 Linux 私有 Unix socket。两种入口共享同一租约和投递状态；一个连接同时只有一个有效监听器。账号和线程来自已认证调用，不能靠展示名称或工具参数冒充。

`/send <正文>` 等待接收程序确认。没有监听器、接收拒绝或结果未知时保留草稿；`accepted` 只表示程序确认接收，不能代表模型已运行。匹配的已登记线程可接收其消息，错误线程不会被改投默认接收者。

本轮修复了选中原生收件人后 `/send` 可能退化成普通文本消息的问题。现在会明确拒绝不支持该命令的原生收件人，保留完整草稿、光标、账号与收件人，不创建监听投递或普通消息。

真实 CLI/PTY 强制发送完成 **132 项检查**；两个独立接收程序通过 HTTP 与 Unix socket 完成 **74 项检查**，包括精确线程、消息 ID、游标、ACK、竞争读取和进程清理。自动调用大模型的常驻转发程序不在本轮范围内。

文档与证据：[监听接口](../../thread-listeners.md) · [强制发送](evidence/force-send.json) · [网络与 Unix 双程序](evidence/listener-transports.json)

## 4. zuse-computer 改进

依赖固定为 **0.2.2**，子模块固定为 `94c837551a7a2fd8ec9f577a3e2f57309bd5ce5a`。zuse 执行桌面基础操作，SSMCP 负责连接授权、私有桌面、取消清理与必要的平台身份确认。

Windows 原先存在实际接口不一致：zuse 返回原始 HWND 和独立 PID，SSMCP 原生 focus 接受带进程身份的窗口 ID。本轮在公共返回边界生成 `hwnd:<hex>:<pid>`，使枚举结果可直接用于聚焦；聚焦前及前台等待期间仍核对 PID。内部 zuse worker 保留自身原始 ID。macOS 原有的几何匹配、唯一性和前台确认继续保留。

状态结果增加逐操作 `engine_routes`，报告实际 zuse 版本、后端、可用性和原因。zuse 探测失败时，其窗口/默认截图操作明确不可用，不借用未调用的引擎伪装成功。Windows/macOS 的输入与带光标截图继续保留原生检查；X11 当前不会合成光标，元数据如实返回 `cursor: false`。

真实私有桌面验收完成 **28 项检查及 6 个 GTK 场景**：错误 machine/PID 时没有输入；修正身份后可正常输入；完整 Unicode 文本与选区替换准确；原生派发后取消明确报告文字可能已改变，不重发或回滚。所有自有 worker/接收器完成清理，实际引擎版本为 **0.2.2**。

Windows GNU 目标的库与程序通过 cargo check 交叉目标检查，未验证 Windows 链接产物或真机运行；macOS 真机运行也不在本轮范围。桌面测试使用两个自有 Xvfb 显示，其中一个是隔离哨兵，不是用户宿主桌面；internal worker 测试也不能视为公共 MCP 持续实例恢复测试。

实现：[computer-use 文档](../../computer-use.md) · [窗口身份转换](https://github.com/LIghtJUNction/ssmcp/blob/b39a8e96c7234b6fb097d388e97614a2102b9810/src/computer/windows_identity.rs) · [桌面验收](evidence/desktop.json) · [GTK 回执](evidence/gtk-text.json)

## 5. 验证汇总

| 验证 | 本轮结果 |
| --- | --- |
| 全部默认库测试 | 1618 通过，0 失败，22 ignored |
| 19 个默认集成目标 | 91 通过，0 失败，18 ignored |
| Rust 默认测试合计 | 1709 通过；不重复累加专项测试或子 worker 的内部摘要 |
| 格式与严格 Clippy | 通过；Cargo 另有已有的 unused qrcodegen manifest 提示 |
| Windows GNU lib/bins | 交叉目标 cargo check 通过；未验证链接或真机运行 |
| 优化版三个程序及 VT 解码器 | 构建通过 |
| 引用真实终端 | 53 项检查通过，8 帧 |
| 强制发送真实终端 | 132 项检查通过 |
| HTTP/Unix 双程序监听 | 74 项检查通过，全部自有进程退出 |
| zuse 私有桌面 | 28 项检查、6 个 GTK 场景通过，实际版本 0.2.2 |

保留的 ignored 项没有被整体开启；验收只显式启动合成账号 router 等自有 fixture。没有启动外部模型进程或使用真实用户登录进行隔离验收。

本轮保留并解决了验证问题：库测试将可信临时目录用例与共享输出清理隔离；旧协作 fixture 把精确只读问答查询错误计入业务调用，修复后仍严格拒绝其他调用及无效凭据；线程列表验收等待真实周期快照；大结果读取检查完整性，避免提前消费 Next caller 消息；Unicode 桌面验收使用符合实际路径的 GTK 接收器。详见 [validation.json](validation.json)。

8 张 PNG 均由本轮真实 VT 帧渲染并逐张检查。48 列下长警告被省略，完整警告保留在结构化回执；技能选择底栏的 `/queue` 与额度标签仍有一处间距不足。这两项不影响输入、选择或发送。

PNG 归档保留原目录 `docs/reports/message-references-20261120/`；下列链接指向云盘 ZIP，括号中是该目录内的文件名。完整清单见 [归档清单](../generated-png-manifest-20261121.json)。

[缺失文件仍可发送，48列](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `screenshots/message-references-missing-before-send-48.png`）

[技能选择，120列](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `screenshots/message-references-skill-picker-120.png`）

## 6. 安装、同步和生产激活

本轮已分阶段提交、推送；产品提交依次为 `320ed8c`、`7c00c25`、`2e0562c`、`b39a8e9`，验收补充为 `2f94719`、`5033cf3`。候选基于集成分支，不代表 main 或生产中继已更新。

独立候选目录：

    /home/lightjunction/.local/share/ssmcp/candidates/0.10.15-b39a8e9/bin/

三个候选程序与受测 release 的 SHA256 完全一致，完整值位于 [deployment-status.json](deployment-status.json)。共享 Cargo 入口在本轮安装验证后又出现 0.10.14 的安装记录；没有确认改写执行者，也没有继续争用该入口。后续应以版本、哈希与运行进程共同确认部署状态。

生产服务现装正式部署 helper 要求已发布稳定 Release、精确 tag/commit 与 manifest、归档大小和 SHA256 校验，没有独立 stage 或 dry-run 入口。切换会重启 `ssmcpd.service`。现有客户端的原生任务可能在断线时被取消，watchdog 重连不能保证任务继续。11 月 20 日 15:00 左右的快照已确认至少 4 个 running exec jobs，这是统计下界，且不覆盖所有其他后台调用，因此本轮保留生产服务与活动任务。

标准账号入口也已做有界实测，旧服务器返回：

> allow entries must use username@github; only GitHub is supported

新版普通后台会话已停止反复重试。项目通过保留的 0.10.13 恢复程序重新连接，并实际执行了工作区/进程版本检查。该恢复会话是原生 systemd-user 连接，没有启动模型，也没有打开可见前台 TUI 窗口。

下一步需安排活动任务完成或保存进度后的切换窗口：通过现有正式发布路径生成与准确提交匹配的制品（可使用 `deploy_server=false`），再明确授权生产切换，核对服务实际版本/哈希、健康状态和客户端重新连接。当前不能把 0.10.15 称为已在线启用。

依据：[部署说明](../../deployment.md) · [实际安装与连接回执](deployment-status.json)

## 7. ztuio 发布与本地依赖

`ztuio 0.1.0` 已发布到 [crates.io](https://crates.io/crates/ztuio/0.1.0)，对应 [GitHub Release v0.1.0](https://github.com/LIghtJUNction/ztuio/releases/tag/v0.1.0)。本轮已核对发布包、13 项包测试与独立 registry 消费构建，没有重复发布同一版本。

原 `cargo install --path . --force` 的问题是本地路径依赖缺少子模块：crate 已发布不能替代 `path = "crates/ztuio"` 所需的本地目录。主工作区子模块已补齐，README 使用 `--recurse-submodules`，候选构建使用固定提交的独立依赖快照。依赖来源见[子模块说明](../../dependencies.md)。
