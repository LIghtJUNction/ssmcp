# 客户端：Web、桌面、Android 与 iOS

SSMCP 的图形客户端让你从浏览器、Windows / Linux 桌面或手机继续使用已有的 CLI 会话。界面读取真实对话、终端和 worker 状态，并把明确提交的操作交给 CLI。会话、执行、输入队列和 worker 调度仍由 CLI 持有，因此关闭一个界面不会结束后台工作。[GUI 协议与适配器](gui-bridge.md) · [会话实现](../src/session.rs)

## 选择一个入口

| 客户端 | 适合的使用方式 | 代码与连接方式 | 入口 |
| --- | --- | --- | --- |
| Web | 在浏览器中查看会话、继续对话和管理现有资源 | HTML、CSS、原生 ES modules；同源 HTTP gateway | [Web 使用说明](../apps/ssmcp-web/README.md) |
| Windows / Linux 桌面 | 在运行 CLI 的电脑上使用原生窗口与 VT 终端画面 | Rust、zgpui、zbridge；直接使用本地 `RemoteFrontend` | [桌面使用说明](../apps/ssmcp-desktop/README.md) |
| Android | 在手机上启动 CLI 或访问已有网关 | Kotlin、Android 系统 WebView；Android 8 / API 26 起 | [Android 使用说明](../apps/ssmcp-android/README.md) |
| iOS | 使用系统原生导航和对话界面 | SwiftUI、URLSession、HTTPS；iOS 16 起 | [iOS 使用说明](../apps/ssmcp-ios/README.md) |

四端都围绕会话、对话、事件、终端和 worker 展开。Web 和桌面把会话导航放在侧边，正文使用宽留白与简洁状态提示；Android 复用 Web 的窄屏布局，iOS 使用原生页面切换，让对话、资源和操作状态各自有明确位置。页面只开放当前连接实际提供的能力，未配置 worker 池或聊天能力的会话会显示相应空状态。[Web 界面](../apps/ssmcp-web/index.html) · [桌面界面](../apps/ssmcp-desktop/src/ui.rs) · [Android 界面](../apps/ssmcp-android/app/src/main/java/dev/ssmcp/mobile/MainActivity.kt) · [iOS 界面](../apps/ssmcp-ios/SSMCP/ContentView.swift)

## 连接已有会话

### Web：由 CLI 同源提供页面与 API

运行 `ssmcp alice@relay.example --webui` 即可在本机默认浏览器中打开该连接的对话。Linux 桌面的 **SSMCP Web** 菜单项和 `ssmcp webui` 支持首次配置，也可使用已确认的目标。需要观察已有会话时，运行 `ssmcp gui open` 并选择配对文件。[启动与配对](gui-gateway.md#启动与配对)

`ssmcp gui serve --listen 127.0.0.1:8787` 在指定端口提供内置网页与 API。Node.js 20+ 仅用于可选的开发服务器和前端测试。开发页面可以通过本机网关调试：

```sh
node apps/ssmcp-web/dev-server.mjs --gateway http://127.0.0.1:8787
```

开发页面默认位于 `http://127.0.0.1:4173`，API 仍走同源路径。Web 令牌保存在当前页面的内存中，刷新页面需要重新配对。[Web transport](../apps/ssmcp-web/src/gateway.mjs) · [开发服务器](../apps/ssmcp-web/dev-server.mjs)

### 桌面：直接使用本地连接

桌面客户端在与 CLI 相同的用户身份下读取已有会话，并通过 CLI 的受保护 IPC 取得前端连接。它使用公开的 `connect_named` 解析名称，认证 descriptor 与 lease 留在 `RemoteFrontend` 内部。

```sh
ssmcp-desktop
ssmcp-desktop --session my-session
```

默认进入观察模式。需要控制时，可以在窗口中点击「申请控制」，或对明确的目标使用 `ssmcp-desktop --session my-session --control`。已有手工管理的 attach 服务也可以通过 `--directory /absolute/private/session-directory` 指定；普通会话列表不显示这类私有目录。[桌面连接实现](../apps/ssmcp-desktop/src/engine.rs) · [RemoteFrontend](../src/session/attach/zbridge_adapter/remote.rs)

### Android 与 iOS 连接网关

Android APK 可以直接在本机启动 CLI。在手机上连接远端网关时，填写 HTTPS 源地址和配对令牌，例如 `https://gui.example`。先把本机 gateway 接到实际的 HTTPS 入口，再按部署地址设置 `--public-origin`：

```sh
ssmcp gui serve --listen 127.0.0.1:8787 \
  --assets /absolute/apps/ssmcp-web \
  --pairing-file /absolute/private/gui-pairing.json \
  --public-origin https://gui.example
```

`--public-origin` 声明外部源地址；HTTPS 连接由实际的 TLS 入口提供。代理需保留配置地址对应的 `Host` 和浏览器 `Origin`，该命令本身不申请证书或部署代理。[HTTPS 代理约定](gui-gateway.md#手机与明确的-https-代理)

Android 与 iOS 都使用系统证书校验，拒绝带凭据、额外路径或查询参数的源地址。Android 使用 Keystore 加密保存配对，iOS 使用 Keychain；两端都提供删除已保存配对的操作。[Android WebView](../apps/ssmcp-android/app/src/main/java/dev/ssmcp/mobile/WorkbenchView.kt) · [Android 配对存储](../apps/ssmcp-android/app/src/main/java/dev/ssmcp/mobile/TokenStore.kt) · [iOS transport](../apps/ssmcp-ios/SSMCP/Gateway.swift) · [iOS 配对存储](../apps/ssmcp-ios/SSMCP/PairingStore.swift)

## 观察、控制与实际能力

连接成功后，前端先向 CLI 读取 `capabilities`，得到可用资源和获准命令。默认观察连接可以读状态、历史和事件。发送输入、写入终端或调整容量需要明确申请控制，且对应命令确实存在于本次能力响应中；按钮可见本身不授予权限。

控制租约决定当前前端是否可以写入。多个界面可以观察同一个会话，控制连接则必须服从实际输入租约。CLI 在请求分发、状态修改和异步返回时检查权限，前端不能通过指定 agent 名称或修改页面状态获得新的身份。[能力与权限实现](../src/session/attach/zbridge_adapter.rs)

| 操作 | 共享 CLI 接口 | 用户可以据此确认的事实 |
| --- | --- | --- |
| 查看当前后端 | `runtime` | 当前后端是否在线、可用能力与实时读取位置 |
| 选择对话来源 | `threads`、`conversation` | 真实线程及当前进程保留的来源记录 |
| 发送人类输入 | `chat.send`、`chat.queue` | 本地输入队列接收情况及后续队列状态 |
| 写入终端 | `terminals`、`terminal.write` | 指定终端的实际输出与写入回执 |
| 调整终端尺寸 | `terminal.resize` | 收到明确的尺寸调整回执；窗口缩放不会代替这次操作 |
| 调整 worker 容量 | `workers`、`workers.resize` | 当前池的实际容量、登记和退役状态 |

线程列表里的 agent 表示可以选择的对话接收方，worker 站位表示池中的容量与任务状态。界面根据实际登记关系显示它们；增加尚未登记的站位不会被画成已经工作的模型。[worker 管理](workers.md)

断开操作释放的是前端连接。成功回执应明确后台会话继续运行；如果回执在网络中丢失，就保留释放结果待确认。读取事件、暂停观察或切换页面本身都不会领取 worker 任务，也不会确认消息已读。[连接释放实现](../src/session/attach/zbridge_adapter/remote.rs) · [事件语义](events.md)

## 继续对话：阅读范围与发送目标

历史范围决定显示哪些记录，发送目标决定本次输入交给谁。各端当前的控件关系如下；桌面和 iOS 切换线程时，也会改变下一次提交的接收目标。

| 客户端 | 阅读范围 | 下一次提交的发送目标 |
| --- | --- | --- |
| Web / Android | 「整个会话」「主对话」或具体线程 | 用独立的「接收方」选择器指定 `next` 或当前 agent；切换阅读范围不会改变接收方。[Web 控件](../apps/ssmcp-web/app.mjs) |
| Windows / Linux 桌面 | 「全部来源」「主会话」或具体 agent 线程 | 共用线程选择器：选择具体线程时发给该 agent；选择「全部来源」或「主会话」时发给 `next`。输入框下方显示当前目标。[桌面映射](../apps/ssmcp-desktop/src/model.rs) · [桌面控件](../apps/ssmcp-desktop/src/ui.rs) |
| iOS | `next` 显示共享会话记录，具体 agent 显示对应线程；当前没有独立的主会话筛选 | 共用线程与目标选择器：`next` 交给下一位合格接收者，具体线程交给该 agent。[iOS 控件](../apps/ssmcp-ios/SSMCP/ContentView.swift) |

CLI 的图形输入入口只接受 `next` 或当前真实存在的一个 agent，不接受隐式广播。两种发送目标的含义如下：

| 发送目标 | 含义 |
| --- | --- |
| `next` | 交给当前会话已有的输入队列，由下一位合格接收者领取 |
| 精确 `agent_id` | 交给线程列表中当前存在的那个 agent；目标失效时需要重新核对 |

点击发送后，`chat.send` 的 `local_queue_admission` 回执只说明 **CLI 本地队列接收了输入**。它不证明模型已经读到，更不代表工作已经完成。可以用返回的消息标识和「输入队列」中的实际状态继续核对；未知结果不能因为界面重新连接就再次发送。[聊天适配器](../src/session/attach/zbridge_adapter/chat.rs) · [CLI 输入队列](../src/client/steering.rs)

人类输入保持人类来源。CLI 在真实入队后记录 `local_human`，并把接收线程放在独立的 recipient 字段中；给某个 agent 发消息不会让这条输入变成该 agent 的发言。全部来源视图包含本地人类输入，精确线程视图也可以包含明确发给该线程的输入；未指定接收者的 `next` 不会因此变成一条广播。[来源与历史筛选](../src/session/attach/thread_history.rs)

## 事件、游标与未知结果

游标记录下一次从哪里继续读取，`generation` 则标识这次读取属于哪个后端生命周期。后端或连接发生代次变化时，需要重新读取能力和状态，不能把旧游标或旧操作结果改写成新后端的结果。协议计数器使用完整的十进制字符串，例如 `"9007199254740993"`，避免大序号在浮点数中丢失精度。[zbridge 协议](https://github.com/LIghtJUNction/zbridge/blob/1cfeb5b1f11c26c35cf7456c9b8b60d1f2d5d8de/src/protocol.rs)

图形界面读取的 attach 历史来自有界内存日志。当前实现按 2 MiB 和 4,096 条上限淘汰较早记录，因此它不是永久聊天档案。`retention_gap` 表示原读取位置之前的一部分记录已经不在窗口内；它不提供缺失操作的成功或失败结论。[attach 日志实现](../src/session/attach.rs)

处理一页事件的顺序是：验证回包与原请求的关系，确认代次和序号，遇到缺口先刷新权威状态，完成页面消费后再保存 `next_cursor`。如果刷新失败或观察被取消，保留原安全游标以及尚未消费的页面。这个本地读取进度不等于用户输入已读、协作消息确认或 worker 任务完成。[公开 EventObserver](../src/session/attach/zbridge_adapter/observation.rs) · [GUI 事件接口](gui-bridge.md)

实时观察按有界窗口进行。原生 zbridge `events` 每页等待最多 1 秒；gateway 把这些读取组成最长 55 秒的观察，Web 和移动端通常请求 50 秒，并给包含传输在内的 HTTP 请求设 55 秒截止。桌面使用更短的 1.5 秒观察，并让整轮连接、状态刷新或缺口协调共享 15 秒预算。[桌面读取预算](../apps/ssmcp-desktop/src/engine.rs) · [GUI 事件接口](gui-bridge.md)

Web 和移动端的一轮权威刷新让能力、对话、runtime、终端、worker、线程和输入队列共同使用一个 55 秒截止；事件观察自动触发的缺口或历史核对继承原窗口剩余时间，不逐页重新计时。预算耗尽时保留已收到的数据页和原游标，下一轮继续核对，处理完成后再推进游标。[iOS 控制器](../apps/ssmcp-ios/SSMCP/ClientModel.swift) · [Web 读取窗口](../apps/ssmcp-web/src/client.mjs)

`unknown` 表示没有足够证据确定一次操作的结果。命令可能已到达 CLI，只是回执没有成功返回；前端保留原请求 ID 和原代次，先读取当前状态或原消息队列。它不会把网络重连当成重发理由。终端写入回执也只证明写入被接收，worker 缩容的 `resize_pending` 则表示释放站位的握手还在继续。[命令回执模型](https://github.com/LIghtJUNction/zgpui/blob/d34c41f79ddab343e7468c5192885a82ca487687/src/bridge.rs) · [worker 管理](workers.md)

## 各端当前显示边界

| 项目 | 当前行为 |
| --- | --- |
| 桌面对话输入 | zgpui 原生单行控件，最多 4,096 UTF-8 字节 |
| 桌面终端 | vt100 解析实际控制序列、光标和屏幕文本；单色网格，明确写入操作 |
| Web / Android / iOS 终端 | 有界纯文本输出预览；不呈现完整 VT 屏幕状态 |
| 桌面初始终端尺寸 | 24 × 80 是本地画布默认；CLI 的 `terminal.list` 不返回实际行列，收到尺寸调整回执后才确认本窗口的设置 |
| worker 容量 | 显示已登记、工作、待命、离线和退役状态；增加未登记站位不会被显示成已经工作的模型 |
| 历史与记录 | 前端各自保留有界视图；记录被省略、存在缺口或结果未确定时继续保持这些事实 |

桌面的 VT 投影目前不包含彩色单元、终端图像协议、鼠标直通或任意键盘直通。手机上的终端页面适合检查输出和明确提交文字输入；需要完整交互式终端能力时，仍可使用已有 CLI 终端入口。各端更具体的内存上限、草稿与回执保留方式见对应应用 README。[桌面模型](../apps/ssmcp-desktop/src/model.rs) · [PTY 状态字段](../src/pty.rs) · [Web 文本投影](../apps/ssmcp-web/src/presentation.mjs)

## 从源码构建与检查

以下命令是可复现入口，不代表已经完成相应平台的运行验收。构建过程使用完整仓库与已有工具链；产物位置、签名和系统依赖以各端 README 为准。

### Web

生产部署直接由前面的 gateway 命令提供静态文件。协议与状态测试使用 Node 内置 test runner，无需安装前端运行时依赖：

```sh
cd apps/ssmcp-web
node --test test/*.test.mjs
```

可选浏览器 smoke 入口为 `test/browser-smoke.mjs`，使用环境中已有的 Playwright 与 Chromium。协议 fixture 验证和真实 gateway 操作需要分别记录。[Web package](../apps/ssmcp-web/package.json) · [浏览器检查入口](../apps/ssmcp-web/test/browser-smoke.mjs)

### Windows / Linux 桌面

桌面应用有独立 Cargo 工作区，并复用当前 checkout 的 ssmcp crate。要求 Rust 1.100+；先按桌面 README 生成并保留应用 lockfile，再使用锁定依赖构建：

```sh
cargo test --manifest-path apps/ssmcp-desktop/Cargo.toml --locked
cargo build --manifest-path apps/ssmcp-desktop/Cargo.toml --locked --release
```

Linux 原生后端需要 X11、FreeType、Fontconfig 和相应 C 开发工具；默认构建还需要 OpenGL。`--features linux-software` 使用真实 X11 软件渲染。Windows 可在本机 MSVC 工具链构建，也提供 Windows GNU 交叉构建说明。交叉编译通过与 Windows 实际启动是两项证据。[桌面 manifest](../apps/ssmcp-desktop/Cargo.toml) · [桌面构建说明](../apps/ssmcp-desktop/README.md) · [固定版本 zgpui](https://github.com/LIghtJUNction/zgpui/tree/d34c41f79ddab343e7468c5192885a82ca487687)

### Android

工程使用 JDK 17+、Gradle 8.10.2、AGP 8.7.3、Kotlin 2.0.21 与 Android SDK / Build Tools 35。已有依赖缓存时，从应用目录离线构建：

```sh
cd apps/ssmcp-android
./build-native.sh
gradle --offline :app:assembleDebug :app:assembleDebugAndroidTest
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk
adb shell am instrument -w dev.ssmcp.mobile.test/dev.ssmcp.mobile.ProtocolInstrumentation
```

应用 APK 构建、测试 APK 构建和设备 instrumentation 执行分别记录。最终安装包大小取决于构建类型与当次产物，不以开发中的单次 debug 结果承诺发布体积。[Android 构建配置](../apps/ssmcp-android/app/build.gradle.kts) · [协议测试](../apps/ssmcp-android/app/src/androidTest/java/dev/ssmcp/mobile/ProtocolInstrumentation.kt)

### iOS

在装有 Xcode 与 iOS SDK 的 Mac 上打开 `SSMCP.xcodeproj`，或从应用目录构建模拟器目标。实机使用自己的签名团队：

```sh
cd apps/ssmcp-ios
xcodebuild -project SSMCP.xcodeproj -scheme SSMCP \
  -sdk iphonesimulator -configuration Debug \
  -derivedDataPath DerivedData CODE_SIGNING_ALLOWED=NO build

xcodebuild -project SSMCP.xcodeproj -scheme SSMCP \
  -destination 'platform=iOS Simulator,id=已安装模拟器的UDID' \
  -derivedDataPath DerivedData test
```

将 destination 替换成这台 Mac 实际安装的模拟器。XCTest 协议检查、iPhone 上的界面和前后台切换、真实 HTTPS 会话连接属于不同验收项。[Xcode 工程](../apps/ssmcp-ios/SSMCP.xcodeproj/project.pbxproj) · [共享 scheme](../apps/ssmcp-ios/SSMCP.xcodeproj/xcshareddata/xcschemes/SSMCP.xcscheme) · [协议测试](../apps/ssmcp-ios/SSMCPTests/ProtocolTests.swift)

## 源码依据与版本验收

客户端应以当前 CLI 适配器公布的能力为依据。修改 HTTP 合同时更新 transport；修改资源、命令或来源结构时对照适配器和历史投影，再补对应边界测试。主要事实可从以下位置核对：

| 文档中的事实 | 实现依据 |
| --- | --- |
| 多端共用已有会话与本地控制租约 | [RemoteFrontend](../src/session/attach/zbridge_adapter/remote.rs)、[zbridge 适配器](../src/session/attach/zbridge_adapter.rs) |
| 同源静态页面、真实会话列表与进程级配对 | [GUI gateway 入口](../src/session/gui.rs)、[Gateway 部署与 HTTP 契约](gui-gateway.md) |
| 人类输入仅接收 `next` 或精确当前线程 | [CLI 输入队列](../src/client/steering.rs)、[聊天命令接线](../src/session/attach/zbridge_adapter/chat.rs) |
| 人类来源与接收线程分别记录 | [RecordScope 与历史筛选](../src/session/attach/thread_history.rs) |
| 缺口协调、代次和安全读取位置 | [EventObserver](../src/session/attach/zbridge_adapter/observation.rs)、[GUI bridge 文档](gui-bridge.md) |
| 原生桌面、浏览器与手机各自的界面边界 | [桌面](../apps/ssmcp-desktop/README.md)、[Web](../apps/ssmcp-web/README.md)、[Android](../apps/ssmcp-android/README.md)、[iOS](../apps/ssmcp-ios/README.md) |

版本验收记录应同时给出源码 commit、确实执行的命令、平台和结果。真实会话验收覆盖观察连接、线程选择、输入队列回执、终端输出与明确写入、worker 容量变更，以及断开后 CLI 继续运行。测试 fixture、构建成功和平台实机运行分别记录；没有执行的检查保持未验证。

<!-- 由集成者在实际检查结束后填写本版本验证报告链接；本页不预填 Windows 或 iOS 已运行，不预填最终包体积。 -->

源码库保留源文件、构建配置和文字证据。APK、IPA、可执行文件、截图和录像放入版本发布或独立产物存储，再在验收记录中给出哈希与位置；不把生成的二进制加入 Git。[仓库产物约定](../AGENTS.md)
