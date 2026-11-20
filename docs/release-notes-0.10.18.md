# SSMCP 0.10.18

本文是 0.10.18 的发行准备说明，功能范围以 `34b636615a867a5826e738495662925324567c73` 为基线。**本机最终门禁尚未通过，不能据此宣布正式发布或生产升级完成。** 后续测试修正及最终制品需要各自对应的源码、测试和发布回执。

## 文件与技能引用

`@文件` 是正文的补充信息。文件尚未创建、不可读、越过工作区边界或无法展开时，客户端保留原文并给出不可用提示，允许消息继续发送；有效引用仍携带真实元数据和预览，不会伪造读取结果。支持引号路径、`$skill-name` 和 `$"007-helper"` 这样的技能名称，`$100` 保留为金额文字。[引用准备](../src/client/references.rs) · [补全](../src/client/completion.rs)

正文恰好达到 16 KiB 时，不可用引用的额外信息不会挤掉正文，其提示保留在本地回执；有效附件不会为凑字节数被静默丢弃。正文自身超限仍会报错。重试复用已经准备的数据，编辑后重新准备。引用展开适用于用户要求队列与监听投递；原生参与者消息仍按纯文本发送，发送路径不会替接收设备读取本机文件。[输入边界](client-input.md) · [前轮专项证据](reports/message-references-20261120/evidence/references.json)

## 消息回执、广播与聊天框监听

已确认的消息 ID 和投递阶段不会被迟到的错误覆盖；旧回执不会清空后来编辑的新草稿。新中继提供 `messages.broadcast(clientId, {recipient_ids, text, request_id})`，一次指定 1–32 个精确且互不重复的收件人，返回逐个结果。重试去重同时绑定完整收件人集合与正文；部分成功明确返回，不承诺外部副作用恰好执行一次。[消息协议](thread-messaging.md) · [广播实现](../src/messaging/broadcast.rs)

`ssmcp.ui.listener` 提供 `open/read/ack/close/status`，网络入口与 Linux 私有 Unix socket 共用单个有效监听租约。账号和线程来自经过认证的调用上下文。`/send` 等待接收程序确认；无人监听、拒收或结果不明时保留草稿。选中不支持该命令的原生收件人时明确拒绝，不会改投其他人或退化成普通消息。连接中断会清理旧投递回执，并把未确认的本机强制发送保留在对话记录中、给出本地提示。[监听协议](thread-listeners.md) · [监听实现](../src/client/interface/listener.rs)

`accepted` 仅表示接收程序确认收到，不代表模型已执行。自动调用模型的常驻转发程序没有包含在这些监听原语中。

## 只读查看完整命令

命令事件保留标题与实际命令；展开工具记录或任务详情可查看经过既有审计脱敏的参数和多行脚本。短标题用于识别任务，不替代实际命令。任务详情可滚动，查看、折叠或返回列表不会重跑命令、发送草稿或取消任务；输入焦点与内容区操作分开处理。[任务详情](../src/client/terminal/conversation/tasks.rs) · [事件保留](../src/client/render.rs) · [输入边界测试](../src/client/terminal/events/command_visibility_tests.rs)

这仍受本机记录保留和显示容量约束，不承诺恢复已经被清理的历史输出。该功能的测试夹具在本候选中通过专用测试入口注入事件，尚不能用“能编译”替代最终交互验收。

## zuse-computer 0.2.2 与私有桌面

桌面引擎精确依赖 `zuse-computer = 0.2.2`，固定子模块提交为 `94c837551a7a2fd8ec9f577a3e2f57309bd5ce5a`。SSMCP 继续负责连接权限、私有桌面、目标身份检查和取消清理。Windows 公共窗口 ID 统一为带 HWND 与 PID 的格式，使枚举结果可用于受保护的聚焦操作；内部 zuse worker 保留引擎原始 ID。[依赖](../Cargo.toml) · [窗口身份转换](../src/computer/windows_identity.rs)

状态结果的 `engine_routes` 报告每项操作的实际引擎、版本、后端和不可用原因。引擎探测失败时相关操作明确不可用。X11 当前不会合成光标，截图元数据如实返回 `cursor: false`。原生输入一旦派发，随后取消可能发生在文本已改变之后；返回结果会说明这一点，不自动重发或回滚。[computer use 说明](computer-use.md) · [zuse 适配](../src/computer/zuse_backend.rs)

前轮私有 Xvfb / GTK 验收覆盖了错误目标拒绝、Unicode 输入、选区替换与取消清理；这些记录绑定当时的产品提交，不能替代 0.10.18 最终源码的验收。Windows GNU 的交叉目标检查也不能表述成 Windows 或 macOS 真机运行验证。[前轮桌面证据](reports/message-references-20261120/evidence/desktop.json) · [GTK 回执](reports/message-references-20261120/evidence/gtk-text.json)

## 旧中继兼容与预期提示

已登录客户端连接旧中继时，只有精确的注册前名单格式拒绝才会触发一次已验证自身账号重试。显式 `--account-compat` 要求名单恰好为保存登录所对应的一个真实注册账号，并核对相同服务器、稳定身份和用户名；不会发起 OAuth、注册用户名或把其他本机账号加入名单。持久后台会话应显式保存该兼容开关。[账号兼容说明](client-accounts.md)

以下提示是有意保留的状态信息：

- 缺失文件或未知技能：正文仍发送，引用显示不可用。
- 认证兼容连接：提示当前账号拥有连接、沿用其额度，并停用访问名单编辑。
- 不支持的 `/send` 收件人或不可用桌面操作：明确拒绝，不能据此显示“已执行”。
- 窄终端可能省略长警告文字；完整警告仍以结构化回执为准。

新客户端连接旧中继不会给中继补装新接口。生产 code_mode 尚未导出的 `messages` helper，仍需对应中继版本才能使用。

## 验证与发布状态

2026-11-20 的准备记录如下，必须与最终发行结果分开阅读：

| 检查 | 已取得的结果 |
| --- | --- |
| 固定候选源码 | `34b636615a867a5826e738495662925324567c73` |
| 候选库测试编译 | 通过 |
| 隔离库测试分区 | 1650 通过、8 失败、22 ignored、4 filtered；进程退出码 101 |
| 最终本机门禁 | 尚未取得全部通过结论 |
| 正式制品与安装 | 本说明不构成 GitHub Release、crates.io 或目标设备安装成功的回执 |
| 生产中继切换 | 暂缓；发布与部署分开处理 |

上述候选结果来自本轮 `release17/candidate-gates.json` 回执；它的完整路径为 `/var/tmp/ssmcp-convergence-verification-20261120/release17/candidate-gates.json`。其中嵌套 worker 的单项通过摘要没有重复累加。之前 `28039e8` 的通过记录以及前轮消息、监听和桌面专项记录，均不冒充这次候选的最终通过结果。[前轮记录及范围](reports/message-references-20261120/README.md)

正式发布仍要求匹配的不可变 tag、完整提交、锁文件与子模块身份、官方构建、归档和包校验。活动任务尚未完成或交接时，保持发布与服务器切换分离；watchdog 恢复连接不等于保存旧连接的原生任务。仅凭版本提交、上传附件或客户端重新出现，均不能宣布生产升级完成。[发布流程](releases.md) · [部署流程](continuous-deployment.md) · [延后切换的依据](reports/release-deferred-deploy-20261120/README.md)
