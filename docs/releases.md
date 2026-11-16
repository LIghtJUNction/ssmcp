# 安装与发布

ssmcp 的 Release 提供六种原生客户端。安装脚本自动选择系统和 CPU 架构，下载归档并核对 SHA-256，运行版本检查后替换客户端。默认安装到当前用户目录。

## 一键安装或更新

Linux 与 macOS：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh
```

默认路径是 `~/.local/bin/ssmcp`。目录尚未加入 `PATH` 时，脚本会显示需要添加的目录。固定版本和安装目录：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh -s -- --version v0.5.0 --dir "$HOME/.local/bin"
```

Windows PowerShell：

```powershell
irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex
```

默认路径是 `%LOCALAPPDATA%\ssmcp\bin\ssmcp.exe`，脚本会将目录加入用户 `PATH` 和当前 PowerShell 的 `PATH`。固定版本、指定目录或保持现有 `PATH`：

```powershell
& ([scriptblock]::Create((irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1))) -Version v0.5.0 -InstallDir "$env:LOCALAPPDATA\ssmcp\bin" -NoModifyPath
```

两套脚本都接受 `SSMCP_VERSION` 和 `SSMCP_INSTALL_DIR` 环境变量。下载失败、校验失败、版本不符或二进制无法运行时，现有客户端保持原样。Windows 更新正在使用的程序时，会先保留旧可执行文件；如果操作系统暂时阻止删除，脚本会显示它的实际位置。

安装后，在要交给 agent 操作的项目目录启动连接：

```sh
ssmcp --version
ssmcp --title "迭代当前项目" user@host
```

客户端调用系统 OpenSSH `ssh`，请用系统包管理器提供它。SSH 公钥和服务器入口配置见[配置指南](configuration.md)。已连接客户端更新后，可以在 `code_mode` 中调用 `reload(clientId)`，让新二进制接管会话。当前原生 rsync 传输工具仅在 Linux 客户端注册。

## 预编译归档

每个 Release 都包含以下六个归档、两套安装脚本和 `SHA256SUMS`。每个归档另有 `.sha256` 校验文件，`release-manifest.json` 记录源码提交、文件大小和摘要。归档名保持稳定，版本由 Release 的 tag 标识，便于使用 GitHub 的 `latest/download` 地址。

| 系统 | CPU | 归档 | 内容 |
| --- | --- | --- | --- |
| Linux，glibc 2.35+ | x86_64 | `ssmcp-x86_64-unknown-linux-gnu.tar.gz` | `ssmcp`、`ssmcpd`、服务端运行脚本与配置模板 |
| Linux，glibc 2.35+ | ARM64 | `ssmcp-aarch64-unknown-linux-gnu.tar.gz` | `ssmcp`、`ssmcpd`、服务端运行脚本与配置模板 |
| macOS | Intel | `ssmcp-x86_64-apple-darwin.tar.gz` | `ssmcp` |
| macOS | Apple Silicon | `ssmcp-aarch64-apple-darwin.tar.gz` | `ssmcp` |
| Windows | x86_64 | `ssmcp-x86_64-pc-windows-msvc.zip` | `ssmcp.exe` |
| Windows | ARM64 | `ssmcp-aarch64-pc-windows-msvc.zip` | `ssmcp.exe` |

安装脚本安装客户端。服务端运行在 Linux，通过 OpenSSH、systemd 与 bubblewrap 提供入口和代码执行隔离；部署步骤见[服务端部署](deployment.md)。Linux 归档在 Ubuntu 22.04 构建，其他平台分别在对应架构的 macOS、Windows runner 构建和执行。实际覆盖以该 Release 的 Actions 记录为准。[发布矩阵](../.github/workflows/release.yml)

## 发布一个版本

向 `main` 推送 `Cargo.toml` 的版本更新就会启动 **Release**。工作流固定使用这次推送的提交，等全部检查、构建和 Cargo 包验证通过后，再创建对应 tag 和发布版本。同一版本已有 tag 时，普通 `main` 推送会跳过发布。[发布工作流](../.github/workflows/release.yml)

GitHub Actions 中的 **Bump version** 工作流接受 `patch`、`minor`、`major`。它在 `main` 同步更新 `Cargo.toml` 与 `Cargo.lock` 并推送提交，随后显式启动 **Release**。这样由 `GITHUB_TOKEN` 创建的版本提交也会进入发布流程。[版本工作流](../.github/workflows/bump.yml)

也可以在 `main` 提交版本更新后自行创建 tag：

```sh
git tag -a v0.5.0 -m "ssmcp v0.5.0"
git push origin v0.5.0
```

**Release** 按以下顺序推进：

1. 确认 Cargo 采用稳定版本号，并且对应提交已经在 `main` 历史中；tag 触发时还要验证 tag 与版本一致。
2. 运行完整 Linux 测试、SSH/OAuth/PTY 集成测试，以及 macOS 和 Windows 客户端测试。
3. 在六种原生环境构建归档、运行可执行文件，并验证安装、更新、损坏下载和版本不符的处理。
4. 确认全部归档齐全，生成 `SHA256SUMS`，验证 Cargo 包，再创建缺少的 tag 并发布到 crates.io。
5. 创建 GitHub Release，附加六个归档、两套安装脚本和校验清单。

仓库需配置 `CARGO_REGISTRY_TOKEN` secret；现有 `CARGO_REGISTY_TOKEN` 拼写也可使用。工作流把凭据只传入 crate 发布检查与上传步骤。新版本缺少凭据或任一检查失败都会中止发布。crates.io 已有相同版本时会核对包内容；有差异或版本被撤回时停止，并要求更换版本。[发布脚本](../scripts/release.py)

GitHub Release 先保持草稿，等所有资产上传并核对完成后才公开。重试会复用内容相同的已有文件；同名资产内容不符时停止，不覆盖已经上传的版本。

检查失败时修复 `main` 后再更新 `Cargo.toml`，或从 **Release** 手动入口不填 tag 重新验证当前 `main`。tag 已经创建后的发布失败，可以重跑失败的 job，也可以在手动入口填写该 tag。若 crates.io 已有该版本，脚本要求当前 Cargo 包的校验值与已发布包一致，再继续上传 Release 资产。已经发布的版本对应不同源码时，需要从 `main` 发布新版本。
