# SSMCP iOS

原生 SwiftUI / URLSession 客户端，最低 iOS 16，仅依赖系统框架。包含 Xcode 工程、共享 scheme 和 XCTest 协议测试。**当前工作环境没有 Xcode、Swift 或 iOS SDK，尚未编译或在 iPhone 上验证。**

## 从 iPhone 连接

在首页填写 SSMCP 用户名和中继地址，点“连接”。应用通过系统 WebSocket 直接向所选中继的 `/api/client/connect` 登记。中继确认账号后，iPhone 创建一个独立的 wire v7 设备会话，并显示中继返回的 64 位十六进制客户端 ID。每次重新连接都会得到新会话。首次使用未注册的用户名时，可在中继拒绝后打开该中继的注册页，完成注册再连接。

设备通过系统 TLS 校验证书，把中继确认的 GitHub 数字身份存入本机 Keychain。账号身份变化时需要用户重新核对。普通连接只允许该账号发来的会话查询，以及具备账号、agent 和运行上下文的消息。用户可以从 iPhone 给下一位 agent 发消息，并查看中继返回的排队或投递结果。iOS 本机会话只提供这些能力。进入后台或主动断开会结束该前台会话。传输中断时保留待核对的原始输入。

## 高级网关

点击首页“高级”可使用 `/api/gui/v1` HTTPS gateway。填写源地址和 pairing token；不猜测 CLI 的默认端口。令牌只进入 Authorization header，以 `WhenUnlockedThisDeviceOnly` 存于 Keychain，可在断开后删除。没有证书校验绕过、URL 凭据或请求正文日志；重定向被拒绝。

配对后，输入 `用户名@中继地址`，选择“新建并控制会话”。支持创建的网关在网关主机上运行这次请求的新会话，iOS 只连接创建回执中的 `live:` 目标。断开后可以重新连接本次配对最近创建的会话。旧网关会提示不支持创建。

要使用既有会话，展开“连接已有会话”，选择“读取已有会话”后点选目标。此入口默认只观察。申请控制后，由 core 决定是否授予独占输入租约。五个原生页面提供：

- 分页会话列表、连接和释放租约；关闭连接保留后台会话。
- 中文对话编辑器、真实线程来源、保留历史和消息队列。提交只确认本地队列接纳，不代表模型已经处理。
- 最多 300 条会话 / worker 事件。
- 真实终端列表、最多 16 终端 × 32 KiB 文本输出、明确输入和调整全部终端尺寸。输出不是 ANSI 模拟器。
- 实际 worker 站位与容量；resize 使用读取到的 `pool_generation`。

界面以实际 capabilities 门控；未提供的聊天或任务能力不会伪装为可用。屏幕在应用离开活跃状态时覆盖内容；iOS 不提供与 Android `FLAG_SECURE` 等价的通用截图禁止能力。

## 网关观察和不确定结果

bridge 遵守 [zbridge v1](../../docs/gui-bridge.md) 的字段和回包关联。所有 counter 使用规范十进制字符串并检查 u64 上限，绝不经过 Double。Transport 逐块接收，在 256 KiB 限制处取消超大响应；事件页另限 128 KiB，最多 4 个并行 HTTP 请求。

一次完整观察与状态刷新共用 55 秒绝对截止。gateway 事件长等最多 50 秒，随后触发的 gap 或对话协调继承该窗口剩余时间；手动刷新中的 capabilities、历史和全部状态请求也共用一个截止时间。进入后台或断开会立即取消读取，已发送的命令继续等候原回执。

已收到的事件页和历史页先保留为待消费页。gap 必须先完成同一 generation 的权威状态刷新，再消费页和推进 cursor；失败或暂停会保留原页和旧游标。每次保存待消费页都核对当前读取周期，旧周期晚到的结果不会覆盖更新后的快照或让游标倒退。generation 变化重新读能力，旧命令保留原代次。只支持资源 schema 1；未支持版本不会按 v1 渲染。实现位于 [ClientModel.swift](SSMCP/ClientModel.swift) 与 [Gateway.swift](SSMCP/Gateway.swift)。

命令传输中断或无法验证回包时显示 `unknown`，没有应用层自动重发。打开和关闭连接同样按租约修改处理，原 open request ID 或 close connection ID 保持可查。聊天内外层共用同一个请求 ID，回执必须匹配目标、message ID 和本地队列接纳字段；原生 uncertain / unknown 状态继续保留待确认。

请求日志最多 20 条、总量最多 512 KiB。发送前原子保存原请求 ID、连接、代次、目标及 unknown 标记；聊天和终端输入同时保存完整原文。文件位于应用私有 Application Support，启用完整文件保护；原文使用有界 Base64 编码。已确认或明确拒绝后删除正文，未知记录不会被新请求淘汰；容量全被未知记录占用时暂停新的修改操作。配对令牌独立存于 Keychain。实现位于 [PairingStore.swift](SSMCP/PairingStore.swift)。

草稿只在回执已确认，且连接、代次、目标和正文都仍与提交时一致时清空。其它情况保留编辑内容，并可查看原始未知输入。事件与对话列表分别限 300 项，服务端历史来自当前进程的保留记录。

## Xcode 验收

在装有 iOS SDK 的 Mac 上打开 `SSMCP.xcodeproj`，选择自己的签名团队用于实机；模拟器可直接构建。工程没有第三方 package 或下载脚本。

```sh
SSMCP_IOS_DEVICE="$(xcrun simctl list devices available --json | python3 -c '
import json, sys
runtimes = json.load(sys.stdin)["devices"]
print(next(d["udid"] for runtime, devices in runtimes.items() if "iOS" in runtime
           for d in devices if d.get("isAvailable") and "iPhone" in d["name"]))
')"
xcodebuild -project SSMCP.xcodeproj -scheme SSMCP \
  -destination "platform=iOS Simulator,id=$SSMCP_IOS_DEVICE" \
  -derivedDataPath DerivedData CODE_SIGNING_ALLOWED=NO build-for-testing
xcodebuild -project SSMCP.xcodeproj -scheme SSMCP \
  -destination "platform=iOS Simulator,id=$SSMCP_IOS_DEVICE" \
  -derivedDataPath DerivedData CODE_SIGNING_ALLOWED=NO test-without-building
```

`SSMCPTests` 中的协议测试覆盖 counter 精度与边界、gap、回包关联、未知状态、只读能力、schema 门控、草稿上下文和请求日志。模型集成测试通过可控制的 gateway 执行真实刷新流程，覆盖缺口页失败后恢复、累计读取超时、暂停后晚到状态及两个刷新交错时的历史游标。完整测试入口为共享 `SSMCP` scheme。

这些测试仍需在 Mac 上实际编译和运行。真实 gateway 验收还包括 Keychain、控制租约、前后台切换、断网 unknown 和原生界面。`DerivedData`、xcresult、archive、IPA 和签名资料都不进入 Git。

`DirectRelayTests` 覆盖独立设备登记、身份变化、越权调用、精确回执以及通过本机 WebSocket 发送与接收。可选的 `testAgainstRealRustAccountRouterWhenProvided` 接受仓库隔离账号路由器的回环 URL。将该测试夹具 `ready.json` 中的 `server_url` 作为 `TEST_RUNNER_SSMCP_IOS_FIXTURE_ORIGIN` 传给上述 `xcodebuild test-without-building` 即可运行。它同时连接两部模拟设备、校验各自的新 ID，并核对原始消息回执。
