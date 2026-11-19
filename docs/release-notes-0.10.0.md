# ssmcp 0.10.0

本版让客户端默认使用内置工具，把外部 harness 和原生插件都改为明确选择后才加载。同时完善 Windows 浏览器组件安装、终端图片显示、独立核心库和发布流程。

## 默认原生工具，按需加载外部能力

文件、命令执行、会话、用户交互和原生浏览器工具不要求预装其他 AI harness。启动客户端、读取工具目录或查询 harness 状态不会扫描或启动 Codex、Claude Code、Pi、OpenCode、Playwright MCP。

需要其中一项能力时，调用 `ssmcp.harnesses.load`，然后读取更新后的真实工具目录。同一 provider 的并发加载共享一次初始化；失败保留具体原因，显式 `refresh:true` 才重新尝试。连接关闭后不再发布新工具。原有工具 schema、审批、会话历史和 wire 7 保持各自的含义。[使用方法](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/optional-harnesses.md)

浏览器和桌面功能仍需要操作系统组件。Chromium、Node.js、Xvfb 等运行组件与外部 Agent/MCP harness 分别管理。

## Windows 安装器准备浏览器组件

Windows 安装器默认验证或安装 Node.js 与 npm、固定版本的 `playwright@1.65.1`，并下载匹配的 Chromium。缺少可用 Node 时使用官方 Node 24 LTS Windows ZIP，核对官方 SHA-256。

安装器实际启动并关闭无窗口 Chromium，再用新客户端的 `setup browser --native --check --json` 核对精确路径；通过后才继续替换客户端和执行邀请接入。验证通过的路径保存为 `SSMCP_NODE`、`SSMCP_BROWSER_EXECUTABLE`。显式路径无效会报错，不会偷偷切换到另一版本。

默认不安装或注册 `@playwright/mcp`。只需要基础客户端时，可明确使用 `-SkipBrowserDependencies`。已有终端需要重新打开，才能继承更新后的用户环境。[完整安装说明](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/windows-browser-setup.md)

## 图片使用真实像素或简洁附件卡片

支持 Kitty 图像协议、且完成能力与单元格尺寸探测的终端显示原生图片，并按可见区域裁切、滚动和清理 placement。

无法显示原生图片时，界面显示包含尺寸的紧凑附件卡片，不再用半块字符或 ASCII 模拟图片像素。返回给调用者的原始 MCP 图片字节不受这项界面回退影响。

终端拒绝一张图片时，只撤销该 image ID 的显示，后续图片仍能走原生路径。高噪声 PNG 使用受限的编码输出；超过预算时按既定尺寸限制处理。旧终端若不支持图像协议，需要换用支持的终端；仓库提供 `scripts/ssmcp-ztty.sh`，为每次启动使用独立 socket，避免新版本连接旧 broker。[图片与终端说明](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/terminal-images.md)

## 三个独立核心库，明确的原生插件入口

| 库 | 固定版本 | 用途 |
| --- | --- | --- |
| [ztui](https://github.com/LIghtJUNction/ztui) | `ztui-core 0.1.1` | 终端布局、文本行与差分帧；Rust 库名仍为 `ztui` |
| [zharness](https://github.com/LIghtJUNction/zharness) | `0.5.0` | Extension v1 原生扩展接口 |
| [zplugin](https://github.com/LIghtJUNction/zplugin) | `0.3.0` | 动态库加载与 ABI 校验 |

zharness 与 zplugin 通过 Git 子模块固定到具体提交；ztui-core 保留主线的普通源码目录及 0.1.1 版本。当前独立 ztui 仓库仍为 0.1.0，本轮保留已有 crate 版本，待版本对齐后再迁移。所有库的源码都由父仓库固定，不随外部库分支自动漂移，应用版本递增也不会自动递增这些库。[依赖与源码构建](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/dependencies.md)

新增 `ssmcp.plugins.list`、`ssmcp.plugins.load`、`ssmcp.plugins.unload`。只有显式指定可信动态库路径后，才创建工作线程、校验扩展目录并注册 `plugins.<namespace>.<tool>`。工具保留原始输入/输出 schema 和结果字段，继续经过现有 Runtime hook、注册代际校验和输出归档。

原生插件与客户端拥有同等系统权限。超时不代表同步 native 调用已经停止；卸载撤销路由并请求线程退出，pinned 库映像保留到进程结束。宿主管理的线程、队列、调用缓冲和加载次数均有明确边界。[插件协议与限制](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/native-plugins.md)

## 发布与升级

正式流程构建 Linux、macOS、Windows 的 x86_64 与 ARM64 产物，核对版本、主分支祖先关系及子模块的精确提交。发布清单记录源码、子模块和附件摘要。Windows 矩阵另外执行真实浏览器依赖安装、复用、页面渲染与清理测试；它与不联网的 helper fixture 分别留存证据。

Windows ARM64 的客户端与 Node 使用 ARM64 产物；Chromium 的实际架构写入 smoke 报告，系统支持的 x64 仿真不等同于原生 ARM64 浏览器。

Bump workflow 支持 patch、minor、major 和显式版本、只预览、只提交版本，以及直接进入正式发布。日常开发仍在本地验收。[发布入口和重试方式](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/releases.md)

从源码构建请使用递归 checkout：

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/ssmcp.git
cd ssmcp
cargo build --locked
```

已有 checkout 更新后执行 `git submodule update --init --recursive`。GitHub 自动生成的源码压缩包不展开子模块；可执行文件安装不受此限制。

网关下载脚本嵌入 `ssmcpd`。升级本地客户端或静态指南，不会自动替换正在运行的公共网关；需要新邀请/下载行为时，应确认对应网关也已完成配套升级。
