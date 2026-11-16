# 版本发布与跨平台安装

仓库的 `Release` 工作流负责六个平台的原生测试、预编译归档、crates.io 发布和 GitHub Release。本文描述已配置的流程，不代表某个新版本已经发布；可下载版本以 [GitHub Releases](https://github.com/LIghtJUNction/ssmcp/releases)、[crates.io](https://crates.io/crates/ssmcp) 和对应 [Actions 运行结果](https://github.com/LIghtJUNction/ssmcp/actions/workflows/release.yml) 为准。[工作流](../.github/workflows/release.yml) · [发布脚本](../scripts/release.py)

## 平台和下载内容

六个目标分别在对应架构的 GitHub runner 上构建。Linux 归档包含客户端 `ssmcp` 和网关 `ssmcpd`；macOS、Windows 归档只包含客户端。网关的 OpenSSH 接入、Unix socket 与代码隔离仍运行在 Linux。[平台入口](../src/bin/ssmcpd.rs)

| 平台 | Rust target | 工作流 runner | 归档 | 可执行文件 |
| --- | --- | --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `.tar.gz` | `ssmcp`、`ssmcpd` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | `.tar.gz` | `ssmcp`、`ssmcpd` |
| macOS Intel | `x86_64-apple-darwin` | `macos-15-intel` | `.tar.gz` | `ssmcp` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `macos-15` | `.tar.gz` | `ssmcp` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `windows-2025` | `.zip` | `ssmcp.exe` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `windows-11-arm` | `.zip` | `ssmcp.exe` |

归档名为 `ssmcp-<target>.<格式>`，例如 `ssmcp-x86_64-pc-windows-msvc.zip`；版本由 Release tag 标识，文件名保持稳定，便于使用 `latest/download`。可执行文件位于归档根目录。每个归档有独立 `.sha256` 文件；Release 另附 `install.sh`、`install.ps1`、汇总的 `SHA256SUMS` 和记录源码 commit、字节数、SHA-256 的 `release-manifest.json`。汇总校验覆盖六个归档和两套安装脚本。Linux 归档还附带 `code-mode.js` 与 `packaging/`，服务端仍需按[部署指南](deployment.md)配置系统依赖和权限。

原生 rsync 端点目前只在 Linux 客户端注册。macOS／Windows 客户端提供终端、独立命令、文件与上下文操作，但不会伪装成可用的 rsync 端点。跨平台终端分别采用 Linux PTY、macOS 原生 PTY 和 Windows ConPTY。[终端实现](../src/pty_portable.rs) · [rsync 平台范围](../src/rsync.rs)

以上表格是构建目标，不表示对所有旧操作系统版本都完成了运行验证。Linux GNU 产物由 Ubuntu 22.04 构建，安装基线为 glibc 2.35 或更新版本；完整 Linux 集成检查另在 Ubuntu 24.04 运行。这些产物没有承诺 musl 静态链接或兼容更旧的 glibc，需要不同系统基线时可在目标机器从源码编译。[GitHub runner 列表](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

## 从 crates.io 安装客户端

安装 Rust 工具链后，选择已经在 crates.io 发布的版本。只运行客户端时显式指定 `--bin ssmcp`，避免安装仅用于 Linux 网关的 daemon：

```sh
cargo install ssmcp --locked --bin ssmcp
ssmcp --version
```

固定某个已发布版本时追加 `--version 版本号`。仓库的 `main`、GitHub Release 与 crates.io 是三个不同状态，源码中修改版本号不会让 crates.io 自动出现该版本。GitHub Release 成功而 `publish_crates` 失败时，应先下载预编译归档或从该 tag 的源码构建。

## 使用安装脚本安装或更新

包含安装脚本的 Release 可以自动选择系统和 CPU 架构。脚本下载归档与 `SHA256SUMS`，核验摘要并运行新程序的 `--version` 后，再替换当前用户目录中的客户端。下载失败、摘要不符、指定版本不匹配或二进制不能运行时，检查会在替换旧程序前停止。[Unix 安装器](../scripts/install.sh) · [Windows 安装器](../scripts/install.ps1)

Linux 与 macOS：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh
```

默认安装到 `~/.local/bin/ssmcp`。脚本不会修改 shell 配置文件；该目录尚未加入 `PATH` 时会显示提示。固定版本或目录时可以传入 `--version vX.Y.Z --dir "$HOME/.local/bin"`，其中 `vX.Y.Z` 需替换为已发布的稳定版本：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh -s -- --version vX.Y.Z --dir "$HOME/.local/bin"
```

Windows PowerShell：

```powershell
irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex
```

默认安装到 `%LOCALAPPDATA%\ssmcp\bin\ssmcp.exe`，并把目录加入用户 `Path` 及当前 PowerShell 的 `Path`。指定版本、安装目录或保持现有 `Path` 时：

```powershell
& ([scriptblock]::Create((irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1))) -Version vX.Y.Z -InstallDir "$env:LOCALAPPDATA\ssmcp\bin" -NoModifyPath
```

两套脚本也接受 `SSMCP_VERSION` 和 `SSMCP_INSTALL_DIR` 环境变量。Windows 更新时会先保留旧可执行文件；系统暂时不允许删除正在使用的旧程序时，脚本会显示保留位置。安装器更新磁盘上的程序，不改变已经连接的进程；在原入口更新后可按[会话控制](session-control.md)调用 `reload(clientId)`，或退出并重启。连接前仍需系统 OpenSSH Client，Windows 的安装方法见下一节。

## Windows：PowerShell 安装预编译客户端

Windows 客户端直接使用本机 OpenSSH Client，不要求运行 WSL 或安装 Windows SSH Server。先在 PowerShell 检查：

```powershell
Get-Command ssh.exe
ssh -V
```

如果未安装 OpenSSH Client，在管理员 PowerShell 中安装这个 Windows 可选功能，然后重新打开终端：

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0
```

OpenSSH 由 Windows 系统维护；安装方式及可用系统条件见 [Microsoft OpenSSH 文档](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse)。

下面从实际最新稳定 Release 中选择本机架构的归档，核验发布的 SHA-256 后解压。若最新 Release 尚未提供该架构文件，脚本会停止，不会把其他架构当作替代品。

```powershell
$ErrorActionPreference = 'Stop'
$release = Invoke-RestMethod 'https://api.github.com/repos/LIghtJUNction/ssmcp/releases/latest'
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$target = switch ($architecture) {
    'X64' { 'x86_64-pc-windows-msvc' }
    'Arm64' { 'aarch64-pc-windows-msvc' }
    default { throw "Unsupported Windows architecture: $architecture" }
}
$version = $release.tag_name -replace '^v', ''
$archiveName = "ssmcp-$target.zip"
$archive = $release.assets | Where-Object name -eq $archiveName
$checksum = $release.assets | Where-Object name -eq "$archiveName.sha256"
if (-not $archive -or -not $checksum) {
    throw "Release $($release.tag_name) does not contain $archiveName and its checksum."
}
$download = Join-Path $env:TEMP $archiveName
Invoke-WebRequest -UseBasicParsing -Uri $archive.browser_download_url -OutFile $download
$checksumDownload = "$download.sha256"
Invoke-WebRequest -UseBasicParsing -Uri $checksum.browser_download_url -OutFile $checksumDownload
$checksumText = Get-Content -LiteralPath $checksumDownload -Raw
$expected = ($checksumText.Trim() -split '\s+')[0].ToLowerInvariant()
$actual = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToLowerInvariant()
if ($expected -notmatch '^[0-9a-f]{64}$' -or $actual -ne $expected) {
    throw 'The archive SHA-256 does not match the release checksum.'
}
$installRoot = Join-Path $env:LOCALAPPDATA "ssmcp\$version"
Expand-Archive -LiteralPath $download -DestinationPath $installRoot -Force
$client = Join-Path $installRoot 'ssmcp.exe'
& $client --version
& $client --name windows-dev --title '维护当前项目' --allow octocat@github my-ssmcp
```

把最后一行的身份和 SSH 目标替换为实际值，并先进入要共享的项目目录。程序按版本保存在 `%LOCALAPPDATA%\ssmcp`，可以把包含 `ssmcp.exe` 的目录加入用户 `Path` 后使用短命令。Windows 的 OpenSSH 用户配置通常位于 `%USERPROFILE%\.ssh\config`，认证仍使用原有私钥与公钥。[连接配置](configuration.md) · [PowerShell 解压文档](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.archive/expand-archive)

从 PowerShell 启动客户端不意味着内部 shell 一定是 PowerShell：默认使用 `COMSPEC` 对应的 shell，通常为 `cmd.exe`。需要其他 shell 时，用 `--shell` 指定其绝对路径。切换安装版本时停止旧客户端，再启动新版本目录中的程序；`reload` 会重启原入口，本身不切换目录或安装更新。

## Linux 与 macOS：解压安装

在 Releases 选择与目标系统、架构匹配的 `.tar.gz` 及对应 `.sha256` 文件，在同一下载目录核验。以下 `TARGET` 是占位 target，执行前替换为实际文件名：

```sh
# Linux
sha256sum -c ssmcp-TARGET.tar.gz.sha256

# macOS
shasum -a 256 -c ssmcp-TARGET.tar.gz.sha256

ssmcp_unpack_dir=$(mktemp -d)
tar -xzf ssmcp-TARGET.tar.gz -C "$ssmcp_unpack_dir"
mkdir -p "$HOME/.local/bin"
install -m 0755 "$ssmcp_unpack_dir/ssmcp" "$HOME/.local/bin/ssmcp"
"$HOME/.local/bin/ssmcp" --version
rm -rf "$ssmcp_unpack_dir"
```

核验步骤按操作系统选择一条。确认 `~/.local/bin` 已在 `PATH` 中后，在项目目录运行 `ssmcp --name dev --title "维护当前项目" --allow octocat@github my-ssmcp`。安装网关还需要配置 OpenSSH、HTTPS 与系统隔离依赖，不能只把 `ssmcpd` 复制到服务器就视为部署完成。

## 维护者：在 main 自动验证并发布

每次向 `main` 推送都会运行 CI；其中涉及 `Cargo.toml` 的推送还会启动 Release。发布脚本读取 manifest 版本并固定本次事件的源码提交，先完成全部检查和打包，再创建 `vX.Y.Z` tag。同一版本已有 tag 时，普通 main 发布入口会跳过，日常修改不会覆盖已发布版本。[CI](../.github/workflows/ci.yml) · [发布工作流](../.github/workflows/release.yml)

| 触发方式 | 检查的源码 | 构建后是否发布 |
| --- | --- | --- |
| 向 `main` 推送包含 `Cargo.toml` 的修改 | 该次 push 的 commit | 该版本尚无 tag 时发布 |
| 在 `main` 手动运行，`tag` 留空 | 该次 dispatch 的 main commit | 该版本尚无 tag 时发布 |
| 在 `main` 手动运行，`build_only=true`、`tag` 留空 | 该次 dispatch 的 main commit | 否，只生成 Actions artifacts |
| 推送 `v[0-9]*` tag | tag 指向的 commit，必须已进入 `main` | 是 |
| 在 `main` 手动运行并填写已有 tag | 指定 tag 的 commit，必须已进入 `main` | 是，适合重试 |

手动入口位于 GitHub **Actions → Release → Run workflow**。只做预检时勾选 `build_only`，或使用：

```sh
gh workflow run release.yml --ref main -f build_only=true
```

预检和正式发布采用同一组检查：manifest 与 lockfile 的本包版本必须一致，checkout 必须已经进入 `origin/main`，受跟踪源码必须保持干净。`checks` 复用六个平台的 CI，并在 Linux 检查真实 SSH／OAuth／HTTP、系统隔离和服务限制；harness 协议测试使用确定性 fixtures。`build` 矩阵负责各目标的 release build、可执行文件冒烟，以及真实归档的安装、更新、损坏下载、错误版本和下载失败测试。[跨平台原语测试](../tests/release_client.rs)还验证实际文件操作、skills／AGENTS 读取、后台进程的输出、退出码与取消。源码改变后需让新的 commit 重新通过检查。[安装器测试](../scripts/test_installers.py)

`finalize` 等待 `checks` 与全部六个平台构建成功，核对完整归档集，再运行 `cargo package --locked` 验证源码包。正式发布才在此时创建缺少的 tag；已有 tag 必须仍指向本次已验证提交。随后将验证过的 `.crate` 保存为 `verified-crate` artifact，交给独立的 crates 发布 job，避免再次执行源码编译验证。[事件与 tag 校验](../scripts/release.py)

crates.io 与 GitHub Release 两个发布 job 在 `finalize` 成功后并行执行，各自报告结果。GitHub Release 在附件上传并检查后才从 draft 公开；预发布 tag 会标记为 prerelease。发布页保留安装说明，并自动附加仓库中存在的 `docs/release-notes-<版本号>.md`。两条发布路径共用已验证的源码提交，但其中一方的凭据问题不会阻塞另一方。

### 手动递增版本并启动发布

维护者可以直接同步更新 `Cargo.toml` 与 `Cargo.lock` 的本包版本并推送 main，也可以运行 `Bump version`。它只接受 main 上的显式操作，按 `patch`、`minor` 或 `major` 更新版本、提交并普通快进推送，随后显式启动 Release。Bump 不提前创建 tag；tag 仍由 Release 在全部门禁通过后创建。

```sh
gh workflow run bump.yml --ref main -f level=patch
```

Bump 使用 `GITHUB_TOKEN` 推送的提交不会自行触发另一个 push 工作流，因此 `dispatch_release` 单独启动 Release 的 main 入口。版本提交使用 contents 写权限，触发工作流的 job 使用 actions 写权限。[Bump 工作流](../.github/workflows/bump.yml) · [GitHub 触发规则](https://docs.github.com/en/actions/concepts/security/github_token)

若只是 `dispatch_release` 失败，在 main 仍是准备发布的版本时可重跑该 job，或直接调用 `gh workflow run release.yml --ref main`。不要为了重试同一版本再运行一次 Bump，否则会递增第二个版本。

## crates.io 凭据与失败重跑

首次发布前，在仓库 Actions secrets 中配置有权发布 `ssmcp` 的 `CARGO_REGISTRY_TOKEN`。工作流也兼容历史拼写 `CARGO_REGISTY_TOKEN`；两个都存在时使用标准名称。Token 只传入 crates 发布步骤，不写入源码、日志或发行附件。当前工作流没有把未配置的 OIDC 可信发布者当作凭据备用方案。

crates 发布读取 `finalize` 已验证的 `.crate`，检查注册表状态，再运行 `cargo publish --locked --no-verify` 并核对上传后的 checksum；`--no-verify` 复用前一 job 完成的包验证。若相同版本已经存在，只在其字节摘要与验证过的 `.crate` 一致时跳过；摘要不同或该版本被撤回时停止，要求选择新版本。crates 凭据缺失只会让 `publish_crates` 失败，GitHub Release 仍可发布已经通过完整门禁的预编译资产。[发布校验](../scripts/release.py) · [回归测试](../scripts/test_release.py)

同一 commit 的临时故障可以在 GitHub 重跑失败 jobs。自动 main 运行已经创建 tag 后，应选择 **Re-run failed jobs**；完整重跑会重新执行版本探测，并因已有 tag 而跳过。已有 tag 也可以从 main 的手动入口显式重试：

```sh
gh workflow run release.yml --ref main -f tag=vX.Y.Z
```

源码修复后的新 commit 不能通过重跑旧 run 验证：旧 run 固定旧 SHA。tag 尚未创建时，将修复推送 main，再调用 `gh workflow run release.yml --ref main`，或通过包含 `Cargo.toml` 的更新重新触发。tag 已经创建时，源码修复应使用新的版本号，已有 tag 保持不动。

已经公开的 Release 附件不会被静默替换。重跑发现同名附件时校验内容，内容不同则停止。归档时间戳固定用于减少非源码差异，但这不构成跨任意环境的二进制可重复构建承诺。

## 发布证据如何判断

源码中的工作流表示发布流程已经配置；成功构建说明对应 runner 上的测试和可执行文件检查通过；GitHub Release 可下载附件与 crates.io 可查询版本分别证明两个发布动作完成。测试不调用任何模型，也不替代对用户本机每种 harness、插件、MCP 服务器和账户配置的实际验收。

| 文档中的行为 | 可核查依据 |
| --- | --- |
| main 新版本可自动发布，显式预检强制禁发 | [工作流条件](../.github/workflows/release.yml)、`event_mode` 与事件回归测试 |
| 六个平台及源码包验证通过后才创建 tag | `finalize` 对 `checks`、`build` 的依赖与创建 tag 的步骤顺序 |
| 手动 Bump 能启动验证且不提前打 tag | `bump.yml` 的显式 main dispatch 与 `event_mode` |
| 文件、上下文、进程原语有实际运行检查 | [release_client.rs](../tests/release_client.rs) |
| 包和附件必须符合校验摘要 | [release.py](../scripts/release.py)、[test_release.py](../scripts/test_release.py) |
| 某次版本已经发布 | 该版本的 Actions 结果、Release 附件、crates.io 版本记录 |
