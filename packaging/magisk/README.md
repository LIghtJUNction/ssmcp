# SSMCP for KernelSU / Magisk

适用于 Android 8 及以上版本的 arm64-v8a 设备。下载 [ssmcp-magisk-arm64.zip](https://github.com/LIghtJUNction/ssmcp/releases/latest/download/ssmcp-magisk-arm64.zip)，使用模块管理器刷入。首次安装会尝试打开本机的安装引导。打开模块 WebUI 也会进入引导。首次配置前，模块只运行本地网页服务。

1. 输入你的 SSMCP 用户名和中继域名。已有账号可直接连接。新账号选定名字后，点击 GitHub 注册，完成绑定后返回连接。
2. 阅读并确认 root 风险。使用官方中继时还需阅读并接受用户协议和隐私政策。官方中继每月赠送 1 GB（1,000,000,000 字节），额外流量由用户在中继网站购买。
3. 配置完成后，打开会话即可输入消息。

配置完成后，打开模块 WebUI，系统浏览器会显示这台手机的 SSMCP 会话工作台。首页直接打开本机配置的会话。模块在启动设备时恢复已确认的中继连接。停用时，在模块管理器中禁用模块并重启设备。

没有 WebUI 的管理器可点模块的「操作」直接打开。需要修改配置时，从模块 WebUI 选择「设备配置」。如果手机已运行 Zuse，可在同一入口选择「Zuse 工作台」。

模块会话从 `/data/adb/ssmcp/workspace` 启动。可把已安装的 Android 可执行文件放在 `/data/adb/ssmcp/home/.local/bin`，新启动的模块会话会在这里查找工具。终端内的 `/update --check` 会检查已发布的 Magisk ZIP。

配置位于 `/data/adb/ssmcp/ssmcp.rc`。也可以直接写入一行 `ssmcp username@server`，模块重启后按这条命令连接。刷入更新时会保留目录里的配置和本地状态。更新时，从项目 Releases 下载新版 `ssmcp-magisk-arm64.zip`，在模块管理器中刷入。Termux 用户可用 `ssmcp update --check` 检查版本，或用 `ssmcp update` 更新安装在 `$PREFIX/bin` 的 Android 原生 CLI。

本地工作台和引导使用回环地址。模块从私有目录读取当前进程的配对凭据，通过启动地址的片段交给浏览器。请勿分享启动 URL。移除模块并重启后，数据仍保留在 `/data/adb/ssmcp`。

## 构建

安装 Kam、Android NDK、`cargo-ndk` 和 Rust arm64 Android target。在本目录执行：

```sh
rustup target add aarch64-linux-android
kam export
kam check
kam build
unzip -t dist/ssmcp-magisk-arm64.zip
```

## WebUI 交互回归

在仓库根目录执行（需要已有 Playwright 和 Chromium）：

```sh
PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs CHROMIUM_PATH=/path/to/chromium \
  node scripts/test_magisk_console_browser.mjs
python3 scripts/test_magisk_installer_state.py
```

浏览器回归加载实际模块入口，在 320 和 390 像素视口检查自动打开、失败重试、设备配置及无管理器桥接时的提示。安装器测试使用隔离的 Android 目录和启动器，检查首次设置、更新、已确认账号与配置变更的跳转。
