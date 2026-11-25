# SSMCP for Magisk

Android 8+，arm64-v8a。使用模块管理器安装 ZIP。首次安装会尝试打开本机的安装引导。如果没有自动打开，可从模块 WebUI 或操作按钮进入，点击「打开安装引导」。首次配置前，模块只运行本地网页服务。

1. 输入你的 SSMCP 用户名和中继域名。已有账号可直接连接。新账号选定名字后，点击 GitHub 注册，完成绑定后返回连接。
2. 阅读并确认 root 风险。使用官方中继时还需阅读并接受用户协议和隐私政策。官方中继每月赠送 1 GiB，额外流量由用户在中继网站购买。
3. 配置成功后，完成页显示所选账号和当前中继的 MCP 地址。按「打开设备终端」即可进入会话。需要连接 ChatGPT 时，可在此页查看插件设置说明。

完成配置后，设备启动主会话 `ssmcp username@server`，并在开机后自动恢复。日常从 KernelSU 的 WebUI 可直接查看和操作终端，Magisk 的操作按钮在浏览器打开同一终端。关闭页面后会话继续运行。按「新建」可开额外会话，方向键、Tab、Esc 和 Ctrl 组合键可触控。

配置位于 `/data/adb/ssmcp/ssmcp.rc`。可以手工修改唯一一行 `ssmcp username@server`，重启后重新确认目标。刷入更新时会保留目录里的配置和本地状态。模块内的 `ssmcp update` 检查官方 Release 中的 Magisk ZIP，由 Magisk 刷入更新。Termux 可用 `sh scripts/install-ssmcp-termux.sh update [--check]` 在现有 Debian proot 安装中更新。

终端服务只监听本机回环接口。Magisk 操作通过浏览器 URL 片段传入本地凭据，页面读取后移除片段。持有凭据的浏览器可输入到 root 会话，请勿分享启动 URL。移除模块并重启后，数据仍保留在 `/data/adb/ssmcp`。

## 构建

安装 Kam、Android NDK、`cargo-ndk` 和 Rust arm64 Android target。在本目录执行：

```sh
rustup target add aarch64-linux-android
kam export
kam check
kam build
unzip -t dist/ssmcp-magisk-arm64.zip
```
