# SSMCP Android

打开 APK 后，手机上的 SSMCP CLI 在应用私有目录启动工作台，Android 系统 WebView 加载 CLI 提供的页面。页面、会话目录、输入队列和控制租约与桌面浏览器使用同一套实现。APK 适用于 Android 8（API 26）及以上的 arm64 设备和 x86_64 模拟器。请使用支持现代 JavaScript 的系统 WebView。

原生设置跟随 Android 系统语言，支持英文和简体中文，其他语言默认显示英文。工作台页面使用 WebView 的语言偏好。原生文案分别维护在 `res/values/strings.xml` 和 `res/values-zh/strings.xml`。

## 在手机上启动

打开应用后，CLI 在设备回环地址上启动工作台。应用从私有配对文件读取地址与令牌。要新建会话，请在工作台填写用户名和中继地址，阅读确认项，再点击「连接」。首次使用需要按页面提示完成设备授权。

应用以普通 Android 应用身份运行，工作目录为私有的 `files/workspace`。从「高级设置」选择「停止本机」会退出这个 APK 启动的会话和网关。切到已有网关也会停止原本机进程。系统终止应用进程时，CLI 的所有权管道随之关闭。下次打开应用会使用保存的启动设置。

## 连接已有网关

在「高级设置」选择「已有网关」，填写提供 SSMCP 工作台及 `/api/gui/v1` 的 HTTPS 源地址，选择该网关生成的 `pairing.json`。应用使用 Android Keystore 保存绑定到源地址的配对凭据。

| 网关位置 | 地址 |
| --- | --- |
| 远端机器 | 网关配置的 HTTPS 源，例如 `https://gui.example`。反向代理须保留公共 Host 和 Origin。 |
| 当前手机或模拟器 | 同一设备上的 `http://127.0.0.1:实际端口`。已有模块的默认工作台在 `http://127.0.0.1:42387/`，需要该实例的配对凭据。 |
| 模拟器或 USB 手机上的开发机网关 | 若开发机的回环网关监听 8787，执行 `adb reverse tcp:8787 tcp:8787`，在 APK 填写 `http://127.0.0.1:8787`。模拟器的 `10.0.2.2` 指向宿主机，访问它会改变 HTTP Host，且无法提供工作台所需的浏览器安全上下文。 |

WebView 使用系统证书校验，只允许当前网关源，配对通过目标源明确的一次性消息交给现有表单。远端页面无法调用原生启动命令。可在「高级设置」删除已保存的配对。

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
