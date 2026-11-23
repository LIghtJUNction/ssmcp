# SSMCP Android

原生 Android 客户端：Kotlin、标准 `Activity` / `View`、系统 HTTPS 和 Android Keystore。没有 WebView、Compose、AndroidX、第三方网络库或应用层分析服务。最低 Android 8（API 26）。[协议依据](../../docs/gui-bridge.md)是当前 CLI 的 zbridge 适配器；界面读取真实能力并据此开放操作。

## 连接

先启动提供 `/api/gui/v1` 的 SSMCP gateway，并通过自己的 HTTPS 代理转发。CLI 示例中的端口是显式配置，不是默认值：

```sh
ssmcp gui serve --listen 127.0.0.1:8787 \
  --assets /absolute/apps/ssmcp-web \
  --pairing-file /absolute/private/gui-pairing.json \
  --public-origin https://gui.example
```

在客户端填写 HTTPS 源地址（例如 `https://gui.example`）和 pairing token。令牌仅出现在 `Authorization: Bearer …`，不进入 URL 或应用日志。系统证书校验保持开启，重定向被拒绝。令牌用 Keystore 的 AES-GCM 密钥加密保存，并绑定该源地址；可以在断开后删除配对。应用禁用备份与窗口截图。

默认只读。关闭“Observe only”后，点击会话并明确申请独占控制租约，才可能得到终端输入、调整容量或聊天提交能力。权限仍由 core 决定。

## 界面

- **Sessions**：分页读取已有会话；打开和释放连接。断开不停止后台会话。
- **对话**：真实成员线程、带来源的保留记录、消息队列和中文编辑器。`chat.send` 回执只确认本地入队；不表示模型已经接收或完成。
- **Activity**：会话和 worker 事件，最多保留 300 条。
- **Terminals**：真实终端列表、每终端最近 32 KiB 的 UTF-8 输出、明确输入以及调整全部终端尺寸。它是文本视图，不是完整 ANSI 终端模拟器；不会自动添加换行。
- **Workers**：实际登记、工作、待命、离线与退出状态，16 站位分页；resize 使用当前 `pool_generation`。

只有 core 宣告的资源和命令会开放。尚未提供 `threads` / `conversation` / `chat.queue` / `chat.send` 的 core 会显示能力不可用。

## 协议与资源限制

HTTP transport 集中在 `Gateway.kt`。路径固定为 `GET /sessions`、`POST /connections`、`POST /connections/{id}/bridge`、`POST /connections/{id}/events`、`DELETE /connections/{id}`，都在 `/api/gui/v1` 下。bridge 请求保持 v1 的 `version/id/operation`，返回 `body.kind/value`；原始 zbridge `events` 仍只支持 1000 ms，本客户端使用 gateway 的有界事件观察接口。

一次完整的观察与状态刷新共用 55 秒绝对截止。事件长等最多 50 秒；由事件触发的缺口协调或对话刷新继承本轮剩余时间。手动刷新中的 capabilities、历史页和全部状态读取也共用一个截止时间。进入后台或点击断开会立即取消读取；已发送的命令继续等待原回执。

已收到的事件页和历史页先保留为待消费页。遇到 retention gap，只有同一 generation 的权威状态全部读取成功后才消费该页、推进 `next_cursor`；失败或暂停后继续处理原页。旧刷新晚到时必须通过读取周期和游标校验，不能覆盖新快照。generation 改变后重新读取能力，原命令仍属于原代次。实现位于 [ClientController.kt](app/src/main/java/dev/ssmcp/mobile/ClientController.kt) 和 [Gateway.kt](app/src/main/java/dev/ssmcp/mobile/Gateway.kt)。

所有协议计数器保持规范十进制字符串，检查 `0…18446744073709551615`；不经过浮点数。只渲染已支持的资源 schema 1；其它版本不进入支持集合，返回未知 state 版本会停止该次刷新。帧上限 256 KiB，事件页上限 128 KiB；会话最多缓存 128 项、时间线和对话各 300 项、终端最多 16 个。

请求日志最多 20 条、总量最多 512 KiB。它在发送前保存原请求 ID、连接、代次、目标和 `unknown` 标记；聊天与终端输入同时保留完整原文，便于重启后核对。原文以有界 Base64 编码存入私有应用 preferences，Base64 仅用于编码；应用禁用系统备份。验证回执确认或明确拒绝后删除正文。未确定的记录不会被新请求淘汰；20 条均待确认时暂停新的修改操作。令牌仍只存在配对存储中。实现位于 [CommandJournal.kt](app/src/main/java/dev/ssmcp/mobile/CommandJournal.kt)。

命令只有明确点击才发出。网络中断、回包无法关联或代次变化时，结果保留为 `unknown`，没有应用层自动重发路径。打开和关闭连接也按租约修改处理，原 open request ID 或 close connection ID 保持可查。聊天内外层共用一个请求 ID，并核对回执目标和本地队列接纳字段。编辑器只在回执已确认、连接、代次、目标和正文仍与提交时一致时清空；其它情况保留草稿和原始未知输入。

## 构建与测试

工程使用 AGP 8.7.3、Kotlin 2.0.21、Gradle 8.10.2、SDK / Build Tools 35 和 JDK 17+。使用已有 Gradle 安装及缓存，不在源码库保存 wrapper JAR、APK 或 SDK。

```sh
gradle --offline :app:assembleDebug :app:assembleDebugAndroidTest
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk
adb shell am instrument -w dev.ssmcp.mobile.test/dev.ssmcp.mobile.ProtocolInstrumentation
```

平台 instrumentation 使用真实 `org.json`、preferences 和 controller 执行器。协议回归覆盖 counter / u64 边界、页顺序、schema 门控、租约与聊天回执关联、未知结果、草稿上下文和原文持久保存。可控制的 gateway 测试替身运行实际 controller，验证缺口页刷新失败后暂停与恢复时不重新拉取、不重复消费，以及多个状态请求累计超时后不覆盖原快照。测试不需要额外框架或网络服务。

构建通过、测试 APK 编译和真实设备 instrumentation 执行是不同的验收项。端到端验收还需真实 HTTPS gateway、控制租约、前后台切换和断网测试；本 README 不将源码检查当作设备验收。
