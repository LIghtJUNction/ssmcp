# SSMCP Android

打开 APK 后，手机上的 SSMCP CLI 在应用私有目录启动工作台，Android 系统 WebView 加载 CLI 提供的页面。页面、会话目录、输入队列和控制租约与桌面浏览器使用同一套实现。APK 适用于 Android 8（API 26）及以上的 arm64 设备和 x86_64 模拟器。请使用支持现代 JavaScript 的系统 WebView。

## 在手机上启动

首次启动运行 `ssmcp gui serve`。工作台绑定设备回环的随机端口，APK 从 CLI 私有配对文件读取实际地址和令牌。空会话目录表示这个应用目前没有可查看的本机会话。

打开 **启动设置 → 本机 CLI**，填写命令，点击 **打开**。例如：

```text
ssmcp alice@relay.example --name phone
```

这会在手机上启动一个使用指定中继账户的前台 CLI，显示名称为 `phone`。省略 `--name phone` 可使用 CLI 的默认名称。APK 将用户名、服务器及可选名称作为独立参数交给随包的可执行文件。网页显示会话的实际连接状态；中继账号和网络需要已就绪。应用以普通 Android 应用身份运行，工作目录为私有的 `files/workspace`。

返回桌面可随后重新打开工作台。**停止本机** 会退出这个 APK 启动的会话和网关。更换启动命令或切到已有网关也会结束原本机进程。系统终止应用进程后，CLI 的所有权管道随之关闭。下一次打开应用按保存的启动设置运行。

## 连接已有网关

在 **启动设置 → 已有网关** 中填写提供 SSMCP 工作台页面及 `/api/gui/v1` 的 HTTPS 源地址。可在启动设置输入并保存该源的令牌，也可以进入工作台后输入令牌或选择网关提供的私有 `pairing.json`。原生启动设置用 Android Keystore 的 AES-GCM 密钥加密保存令牌，并将密文绑定到源地址。工作台内输入的令牌仅用于当前页面。

| 网关位置 | 地址 |
| --- | --- |
| 远端机器 | 网关配置的 HTTPS 源，例如 `https://gui.example`。反向代理须保留公共 Host 和 Origin。 |
| 当前手机或模拟器 | 同一设备上的 `http://127.0.0.1:实际端口`。已有模块的默认工作台在 `http://127.0.0.1:42387/`，需要该实例的配对凭据。 |
| 模拟器或 USB 手机上的开发机网关 | 若开发机的回环网关监听 8787，执行 `adb reverse tcp:8787 tcp:8787`，在 APK 填写 `http://127.0.0.1:8787`。模拟器的 `10.0.2.2` 指向宿主机，访问它会改变 HTTP Host，且无法提供工作台所需的浏览器安全上下文。 |

WebView 使用系统证书校验，只允许当前网关源，配对通过目标源明确的一次性消息交给现有表单。远端页面无法调用原生启动命令。删除原生配对可在启动设置完成。

## 查看会话和输入

工作台默认以只读模式连接。需要操作时，选择会话和 **控制会话**，取得原生控制租约后，使用后端实际提供的对话、终端或 worker 功能。聊天入队回执确认本地接纳。请求结果、观察游标、缺口恢复和页面记录的保留规则见[工作台说明](../ssmcp-web/README.md)。断开前可核对操作记录；更换页面或应用进程结束会丢失页面内的草稿和记录。旧 APK 留下的操作记录可在启动设置中查看。

## 构建与验证

使用 JDK 17+、Gradle 8.10.2、AGP 8.7.3、Kotlin 2.0.21、SDK / Build Tools 35 以及原生构建脚本要求的 Android NDK。APK 的 versionName 取自根 Cargo.toml，versionCode 沿用发行流程递增的 Android 发行编号。应用 ID 保持为 `dev.ssmcp.mobile`；覆盖安装还需相同签名。

在应用目录执行：

```sh
./build-native.sh
gradle :app:assembleDebug :app:assembleDebugAndroidTest
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk
adb shell am instrument -w dev.ssmcp.mobile.test/dev.ssmcp.mobile.ProtocolInstrumentation
```

在全新安装的测试设备或隔离的 Android 用户中运行 instrumentation。用例启动 APK 内的 CLI 和随机端口网关，让系统 WebView 执行真实工作台模块，完成配对、读取本机真实会话、申请控制、暂停观察并释放租约。中继目标限于测试设备的回环，不需要远端账号。
