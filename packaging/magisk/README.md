# SSMCP for KernelSU / Magisk

适用于 Android 8 及以上版本的 arm64-v8a 设备。下载 [ssmcp-magisk-arm64.zip](https://github.com/LIghtJUNction/ssmcp/releases/latest/download/ssmcp-magisk-arm64.zip)，使用模块管理器刷入。首次安装会尝试打开本机的安装引导。也可从模块 WebUI 点击「打开安装引导」。首次配置前，模块只运行本地网页服务。

1. 输入你的 SSMCP 用户名和中继域名。已有账号可直接连接。新账号选定名字后，点击 GitHub 注册，完成绑定后返回连接。
2. 阅读并确认 root 风险。使用官方中继时还需阅读并接受用户协议和隐私政策。官方中继每月赠送 1 GiB，额外流量由用户在中继网站购买。
3. 配置成功后，完成页显示所选账号和当前中继的 MCP 地址。需要连接 ChatGPT 时，可在此页查看插件设置说明。回到模块管理器，打开 WebUI 即可操作终端。

完成配置后，设备启动主会话 `ssmcp username@server`，并在每次开机后自动启动。Action 打开音量键菜单。按音量＋切换「查看状态」「启动服务」「退出」，按音量−确认，20 秒无按键会自动退出。日常从模块管理器的 WebUI 操作终端，点击终端中的卡片可展开，点击光标所在行或底栏的 ⌨ 打开键盘，按 Tab 补全。中文和组合字符按终端实际列宽排列，前景色与背景色沿用终端输出。单指上下滑动会滚动手指所在的终端面板。在黑色终端内双指捏合可调节字号，重新打开后会记住。标签栏可新建和切换会话。关闭页面后会话继续运行。

用手机上的 `zuse web` 启动 Zuse 工作台之后，点击会话栏的「Zuse」即可打开。工作台监听 `http://127.0.0.1:43497/`。普通浏览器的 `/zuse` 页面可嵌入工作台，模块管理器入口会在系统浏览器中打开它。

模块会话从 `/data/adb/ssmcp/workspace` 启动。可把已安装的 Android 可执行文件放在 `/data/adb/ssmcp/home/.local/bin`，新启动的模块会话会在这里查找工具。终端内的 `/update --check` 会检查已发布的 Magisk ZIP。

配置位于 `/data/adb/ssmcp/ssmcp.rc`。可以手工修改唯一一行 `ssmcp username@server`，重启后重新确认目标。刷入更新时会保留目录里的配置和本地状态。更新时，从项目 Releases 下载新版 `ssmcp-magisk-arm64.zip`，在模块管理器中刷入。Termux 可用 `sh scripts/install-ssmcp-termux.sh update [--check]` 在现有 Debian proot 安装中更新。

终端服务只监听本机回环接口。首次引导通过浏览器 URL 片段传入本地凭据，页面读取后移除片段。请勿分享启动 URL。移除模块并重启后，数据仍保留在 `/data/adb/ssmcp`。

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

浏览器回归从模块的 `webroot/index.html` 加载真实脚本和样式，使用隔离的模块管理器桥接 / HTTP fixture，覆盖 320 / 390 像素手机视口的单元格点击、双向触摸滚动、鼠标滚轮、滑动防误触、双指缩放和会话切换。它验证发往 PTY 的 SGR 鼠标输入，不代替真实 Android 管理器、设备 PTY 与原生 TUI 的端到端验收。
