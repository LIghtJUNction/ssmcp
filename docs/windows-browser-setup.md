# Windows native browser dependencies

Windows 安装脚本默认一起准备 Node.js、固定版本的 Playwright npm 包和 Chromium。Node 执行安装和验证，Playwright 下载匹配的 Chromium，实际浏览器操作由 ssmcp 内置的 CDP 工具完成。安装过程不依赖其他 AI harness，也不会安装、注册或启动 `@playwright/mcp`。

两种入口使用同一段依赖实现：GitHub Release 的 [install.ps1](../scripts/install.ps1)，以及网关提供的 [/client/install.ps1](../src/client_downloads/install.ps1)。网关脚本嵌入 `ssmcpd`；发布时需要配套更新服务端脚本和 Windows 客户端，不能把新脚本与不支持原生依赖检查的旧客户端混用。

## 安装内容

| 组件 | 处理方式 |
| --- | --- |
| Node.js 与 npm | 复用可运行的原生 Node.js 22、24 或 26 及其同目录 npm。缺失时，从 Node.js 官方站点下载最新的 24 LTS Windows ZIP，核对官方 SHA-256 后安装到 `%LOCALAPPDATA%\ssmcp\runtime`。 |
| Playwright | 在 `%LOCALAPPDATA%\ssmcp\browser\playwright-1.65.1` 安装固定的 `playwright@1.65.1`。已有匹配版本通过本地检查后直接复用。npm 安装禁用 lifecycle scripts，不修改项目的 `package.json` 或全局 npm 包。 |
| Chromium | 使用上述 Playwright 包自己的 CLI 执行 `install chromium`，保留版本匹配关系。沿用 Playwright 的浏览器缓存；已有匹配浏览器时由 CLI 复用。 |

Playwright 官方目前支持 Windows 11+、Windows Server 2019+。脚本提供 x64 和 ARM64 的 Node.js 下载选择；是否真正可用，以后面的实际启动检查为准。[系统要求](https://playwright.dev/docs/intro#system-requirements) · [Node.js 下载与 LTS 版本](https://nodejs.org/en/download/) · [浏览器安装和缓存](https://playwright.dev/docs/browsers)

脚本不需要替换系统 Node.js，也不会改写其他应用的 Node 版本。安装器原有的 `-NoModifyPath` 仍只控制 ssmcp 命令目录的 PATH 设置；原生浏览器依赖通过下面两个专用变量找到。

## 当前会话与以后的终端

安装成功时，脚本同时设置当前进程和 Windows 用户环境中的两个绝对路径：

| 变量 | 用途与优先级 |
| --- | --- |
| `SSMCP_NODE` | ssmcp 使用的原生 Node 可执行文件。设置后优先于 PATH；显式安装 Playwright MCP 时，npm 也由这个 Node 执行其相邻的 `npm-cli.js`。 |
| `SSMCP_BROWSER_EXECUTABLE` | 原生浏览器工具默认使用的 Chromium 可执行文件。工具调用显式指定的 `executable` 优先，其次是此变量，最后才是 PATH 和系统浏览器位置。 |

显式覆盖路径无效时，客户端会报错，不会悄悄选择另一个系统版本。依赖安装器保留并验证已有的有效覆盖；默认情况下保存本次验证通过的原生路径。已经打开的终端进程保留旧环境，需要关闭并重新打开终端应用。

检查当前进程实际解析的结果：

```powershell
ssmcp setup browser --native --check --json
```

返回值的 `runtime.node` 和 `runtime.browser_executable` 是实际路径，错误写在对应的 `node_error` / `browser_error` 字段。此检查只读取原生依赖，不扫描外部 harness、不注册扩展，也不修改客户端配置。

安装脚本先用无窗口的新浏览器配置启动、关闭所选 Chromium，再恢复原来的 PATH，调用此检查核对两个实际路径。只有验证通过且用户环境保存成功，才继续替换已安装的 ssmcp；网关入口也只在此后执行邀请接入。

## 失败与可选安装

下载校验、npm、Chromium 安装或启动、路径解析、用户环境保存中的任何一步失败，脚本都会返回错误，不会宣布整套安装完成或继续消耗邀请。已下载到用户目录的可用依赖可以保留，供修复网络或配置后重试。

只需要基础客户端时，可以明确跳过浏览器依赖：

```powershell
.\install.ps1 -SkipBrowserDependencies
```

这会显示依赖已跳过，不能作为浏览器功能就绪的证明。外部 Playwright MCP 仍是独立的显式可选集成；只有用户要求使用时，才调用其安装和接入流程。[现有浏览器会话](browser-existing-session.md)

## 验证范围

```powershell
pwsh -NoProfile -NonInteractive -File scripts/test_windows_browser_bootstrap.ps1 -Out out/windows-browser-bootstrap.json
```

该 fixture 执行两份脚本中的真实依赖函数，并用临时文件、假下载源和用户环境存储替身覆盖复用、新装、失败、路径优先级以及原生检查调用。它不会下载或执行 Windows 依赖。Linux 上运行 PowerShell 的结果只证明解析和 mock 行为；Windows 实机安装、Chromium 启动和新终端环境继承需要单独记录。

版本与安装方式依据：[Playwright 官方 CLI](https://playwright.dev/docs/test-cli#install-browsers)、[固定包的发布元数据](https://registry.npmjs.org/playwright/1.65.1)、[Node.js 官方发布文件](https://nodejs.org/dist/latest-v24.x/SHASUMS256.txt)。
