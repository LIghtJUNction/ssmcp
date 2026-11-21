# Windows 客户端与 Zuse CLI

Windows 安装器现在只安装经过校验的 SSMCP 客户端。它不自动下载 Node.js、Playwright 或 Chromium，也不执行已撤去的 `ssmcp setup browser`。[Release 安装器](../scripts/install.ps1) · [网关安装器](../src/client_downloads/install.ps1)

## 安装范围

Release 安装器继续从官方 SHA256SUMS 核对公开归档，检查平台与实际客户端版本，再替换目标程序；下载或校验失败时保留既有安装。网关安装器继续核对来源、wire、长度与摘要，安装完成后连接指定服务器。浏览器登录授权仍保留。

旧 `-SkipBrowserDependencies` 参数作为无操作兼容选项接受。无论是否传入，都不会启动浏览器组件安装。Release 安装器的 `-NoModifyPath` 仍控制 **SSMCP 命令目录**是否加入 PATH；这不授权修改 Zuse 或其他程序的路径。

## 查询或管理 Zuse

```powershell
ssmcp computer version --json
ssmcp computer download --json
ssmcp computer install DOWNLOAD_ID_FROM_REPLY --json
```

用真实回包中的 ID 替换占位符。Zuse 版本查询实际执行选定程序的 `--version`；下载只选择官方稳定版及其公布的大小、SHA-256；安装只写入该管理器的私有目录，并返回绝对路径。它不会覆盖已有 PATH/Cargo/系统安装，也不会自动修改 PATH。卸载只接受该管理器拥有且身份仍匹配的 `installation_id`。[完整参数与结果](computer-use.md)

当前 Zuse 管理器配置 Windows x86_64 官方下载目标。SSMCP 的 Windows ARM64 客户端仍独立构建，但当前管理器没有对应 Zuse ARM64 资产映射，会明确拒绝下载；不能把 x86_64 资产当作原生 ARM64 产物。

## 执行桌面与浏览器任务

使用实际返回的 Zuse 可执行路径，通过普通命令或 SSMCP exec 直接运行它的程序。浏览器、显示会话、系统权限和 CLI 自身依赖按所选 Zuse 版本准备；SSMCP 没有内置浏览器或桌面执行 fallback。[直接执行示例](browser-desktop.md)

旧浏览器 bootstrap/smoke 脚本随旧后端退役。正式发布继续验证 SSMCP 归档、安装、更新和下载失败恢复；Zuse CLI 的安装管理由新的生命周期合同覆盖。文档中的验证入口与历史报告不能代替当前候选的实际结果。
