# 版本发布与跨平台安装

仓库的 `Release` 工作流负责六个平台的原生测试、预编译归档、crates.io 发布和 GitHub Release。本文描述已配置的流程，不代表某个新版本已经发布；可下载版本以 [GitHub Releases](https://github.com/LIghtJUNction/ssmcp/releases)、[crates.io](https://crates.io/crates/ssmcp) 和对应 [Actions 运行结果](https://github.com/LIghtJUNction/ssmcp/actions/workflows/release.yml) 为准。[工作流](../.github/workflows/release.yml) · [发布脚本](../scripts/release.py)

0.6.0 的客户端不再接受 `-t`、`--type`、`--backend`，改为统一发现和路由原生能力。安装前按[版本迁移说明](release-notes-0.6.0.md)检查启动命令；当前版本的实际发布、安装与部署结果见[0.6.0 验收记录](validation-0.6.0.md)。原有 0.5.1 验证记录继续保留为历史证据。

## 平台和下载内容

六个目标分别在对应架构的 GitHub runner 上构建。Linux 归档包含客户端 `ssmcp` 和网关 `ssmcpd`；macOS、Windows 归档只包含客户端。网关的 HTTPS/WSS 设备接入、Unix socket 与代码隔离运行在 Linux。[平台入口](../src/bin/ssmcpd.rs)

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

两套脚本也接受 `SSMCP_VERSION` 和 `SSMCP_INSTALL_DIR` 环境变量。Windows 更新时会先保留旧可执行文件；系统暂时不允许删除正在使用的旧程序时，脚本会显示保留位置。安装器更新磁盘上的程序，不改变已经连接的进程；在原入口更新后可按[会话控制](session-control.md)调用 `reload(clientId)`，或退出并重启。连接通过 HTTPS/WSS；账号与设备授权步骤见[首次连接](onboarding.md)。

## Windows：PowerShell 安装预编译客户端

Windows 客户端通过 HTTPS/WSS 连接中继，可直接在 PowerShell 使用。

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
& $client --server https://relay.example --name windows-dev --title '维护当前项目' --allow alice@relay.example
```

把最后一行的服务器和已注册账号替换为实际值，并先进入要共享的项目目录。程序按版本保存在 `%LOCALAPPDATA%\ssmcp`，可以把包含 `ssmcp.exe` 的目录加入用户 `Path` 后使用短命令。设备可不登录，但控制者必须完成该服务器的账号登录。[连接配置](configuration.md) · [PowerShell 解压文档](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.archive/expand-archive)

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

核验步骤按操作系统选择一条。确认 `~/.local/bin` 已在 `PATH` 中后，在项目目录运行 `ssmcp --server https://relay.example --name dev --title "维护当前项目" --allow alice@relay.example`，替换为实际服务器和已注册账号。安装网关还需要配置 HTTPS、OAuth、账号存储与系统隔离依赖，不能只把 `ssmcpd` 复制到服务器就视为部署完成。

## 维护者：固定源码后正式发布

Release 保留六个平台的官方构建与发布验证。日常迭代仍以本机检查为主；普通 main push 不会触发 Release。手动预检只生成 Actions artifacts，不创建 tag、GitHub Release 或 crates.io 版本。

| 入口 | 固定的源码 | 是否发布 |
| --- | --- | --- |
| 在 main 手动运行 Release，tag 留空 | 当次 checkout 的 main | 否 |
| 创建或快进更新精确命名的 release-check 分支 | main；该分支仅是既有预检入口 | 否 |
| 推送 v* tag | tag 对应且已进入 main 的 commit | 是 |
| 在 main 手动运行 Release，填写已有 tag | 该 tag 的同一 commit | 是；用于首次发布或失败重试 |

空 tag 始终表示预检，不能用 `build_only=false` 把它变成隐式发布。填写 tag 时不能同时启用 `build_only`。分支创建事件只接受既有的 release-check 入口；没有新增日常自动 CI 触发器。

```sh
# 只验证当前 main。
gh workflow run release.yml --ref main

# 重试已经创建的固定 tag；不会递增版本或移动 tag。
gh workflow run release.yml --ref main -f tag=vX.Y.Z
```

### 一次操作递增版本并发布

Bump version 只接受 main 上的手动调用。默认依次更新应用版本与锁文件、提交并普通快进推送、创建不可移动的匹配 tag，再显式调用 Release。快捷入口省去重复手工操作，保留完整检查、六个平台构建和摘要校验。

| 输入 | 默认值 | 作用 |
| --- | --- | --- |
| level | patch | patch、minor、major 或 custom |
| custom_version | 空 | custom 时必填；不带 v，必须比当前版本大，可使用合法 SemVer prerelease |
| dry_run | false | 只验证并显示计划；不写文件、不提交、不打 tag、不发布 |
| publish | true | 版本提交后启动正式发布；false 时仅提交应用版本 |

下列 custom 示例中的版本应替换为高于当前 manifest 的目标版本。

dry_run 优先于 publish。未选择 custom 却填写 custom_version 会报错，防止输入被忽略。普通三级递增要求当前版本为稳定版；从预发布版本升级时使用 custom。

```sh
# 一键递增 minor 并进入完整正式发布。
gh workflow run bump.yml --ref main -f level=minor

# 预览指定版本，不产生仓库或发布变更。
gh workflow run bump.yml --ref main \
  -f level=custom -f custom_version=0.10.0 -f dry_run=true

# 仅递增版本，先保留在 main，之后再选择发布时机。
gh workflow run bump.yml --ref main -f level=patch -f publish=false

# 本机也可检查计划；此命令不调用 Cargo 或远端 API。
python3 scripts/release.py bump custom --custom-version 0.10.0 --dry-run
```

Bump **只更新 ssmcp 的版本**。ztuio、zharness、zplugin 的 manifest、版本与固定 gitlink 不随应用递增；独立库有实际改动时先在自己的仓库处理版本与发布，再更新 SSMCP 的精确依赖和锁文件。脚本在任何写入前检查应用、本地库和 lockfile 的一致性。

GitHub Token 推送的 tag 不会自行触发另一个 push 工作流，因此 Bump 的 dispatch_release job 显式传入刚创建的 tag。失败后重跑已有 tag 的 Release，或只重跑失败的 dispatch_release job；不要重复点击 Bump 递增出第二个版本。[Bump 工作流](../.github/workflows/bump.yml) · [GitHub 触发规则](https://docs.github.com/en/actions/concepts/security/github_token)

### 子模块是固定源码的一部分

Release、Bump 和正式发布复用的 checks 都递归初始化子模块。它们读取主仓库 commit 中的 gitlink，不追踪子库分支的最新状态。发布前脚本递归核对各子模块已经初始化、HEAD 与 gitlink 相同、没有修改或未跟踪源码；不完整检出会直接报错。

```sh
git submodule update --init --recursive
python3 scripts/release.py validate --build-only
```

ztuio 的包名和 Rust 导入名均为 ztuio。ztuio、zharness 和 zplugin 均作为固定 Git 子模块，从父 workspace 排除；应用使用 path 加精确 registry version，根级 patch.crates-io 将 zharness 的传递 zplugin 依赖统一到同一份本地源码。发布脚本核对这个关系，避免本地加载两份不兼容的 Rust 类型。

源码包仍须实际通过 `cargo package --locked --package ssmcp` 的验证。path 构建成功不等于发布后的 registry 依赖可用；独立依赖尚未发布、版本已撤回或打包字节不一致时必须停止，不能用本地 patch 掩盖。GitHub 自动生成的源码归档不展开 gitlink；需要完整源码时应递归 clone 固定 tag。

### 正式发布顺序

validate 检查 tag、manifest、lockfile、main ancestry、工作区与子模块身份。checks 复用已有 CI；build 矩阵保留 Linux、macOS、Windows 各 x86_64/ARM64 的优化构建、可执行文件冒烟及安装器检查。finalize 在完整检查和六个构建都成功后，重新核对身份、全部六个平台归档和两份安装脚本，打包验证 ssmcp 应用，并保存 verified-crates artifact。

checks 在各原生 runner 上先显式构建 native_plugin_fixture 动态库，再单独执行精确的 native_plugin_runtime_contract ignored 测试。测试本身不会启动 Cargo，缺少预构建制品不会被当作通过。

Windows 官方 build job 另运行 `scripts/test_windows_browser_bootstrap.ps1`，使用该 runner 的原生 PowerShell 和隔离 helper fixtures，并上传独立 JSON 证据。这检查下载、版本、哈希与错误路径，不安装真实浏览器依赖，也不等同于所有用户桌面环境的实机验收。报告放在 out，不混入发行附件集合。

同一 Windows 正式发布矩阵还运行 [`scripts/smoke_windows_browser.ps1`](../scripts/smoke_windows_browser.ps1)：只导入生产安装器的依赖函数，在隔离临时目录真实安装官方 Node 24 LTS、Playwright 1.65.1 与 Chromium，再执行一次复用安装。它使用本 job 新构建的客户端核对 `setup browser --native --check --json` 的精确路径，并让 Chromium 无窗口渲染本地页面。父进程以 Windows Job Object 收口自有进程树，结束后恢复用户环境、删除临时依赖；失败、超时或清理异常都阻止该平台通过。独立 JSON 仅记录版本、摘要、架构与检查状态，不记录原环境或 npm 输出。Windows ARM64 的客户端和 Node 必须匹配 ARM64；Chromium 按 Playwright 实际产物运行并记录其 PE 架构，不能把系统支持的 x64 仿真写成原生 ARM64 浏览器。只有对应 Windows runner 的成功报告才算这项真实验证完成；本机语法检查和 Linux fixtures 不替代它。[Playwright 1.65.1 Chromium 下载映射](https://github.com/microsoft/playwright/blob/v1.65.1/packages/playwright-core/src/server/registry/index.ts)

发布依赖顺序如下：

| 包 | 发布责任与校验 |
| --- | --- |
| zplugin-macros → zplugin → zharness | 独立仓库发布；SSMCP 的发布预检要求使用的 zplugin、zharness 精确版本已存在且未撤回 |
| ztuio → ssmcp | ztuio 由独立仓库发布；应用预检要求精确版本存在且未撤回，应用工作流只上传 ssmcp |
| zuse-computer → ssmcp | 桌面引擎由独立仓库发布；当前精确依赖为 0.2.2，应用预检同样要求版本存在且未撤回，并核对固定子模块与锁文件；SSMCP 工作流不重新发布该依赖 |

独立库的 registry 版本应对应其仓库的固定源码，SSMCP 不从消费方仓库重新发布依赖。应用源码包通过实际 registry 依赖打包验证；已发布应用的摘要必须与验证包一致，冲突时停止。

publish_crates 在首次上传前核对整个已验证包集合，并先检查独立 registry 依赖。应用包尚未发布时上传 ssmcp；上传后再次查询 crates.io checksum。GitHub Release 与 crates.io job 在 finalize 后分别执行并报告结果；其中一个成功不会冒充另一个成功。

GitHub Release 先创建或找回 draft，上传完整附件并检查大小、摘要后才公开。相同 tag 重跑会复用匹配资产，发现不同内容则停止，不覆盖已发布附件。release-manifest.json 记录应用 commit、递归子模块的 path/commit/tree，以及各归档和安装脚本的 SHA-256。[发布流程](../.github/workflows/release.yml) · [发布校验与回归](../scripts/test_release.py)

## crates.io 凭据与失败重跑

仓库 Actions secret CARGO_REGISTRY_TOKEN 必须有权发布ssmcp 应用包。工作流保留历史拼写 CARGO_REGISTY_TOKEN 的兼容，标准名称优先；凭据只传入 crates 发布步骤，不写入源码或发行附件。没有启用未经配置的 OIDC 凭据替代。

发布读取 finalize 留存的验证包，再以 `cargo publish --locked --no-verify --registry crates-io --package <包名>` 上传；no-verify 复用前一阶段完成的真实验证。相同版本只有在 registry checksum 与验证包完全一致时才跳过；版本已撤回或内容冲突则停止。ztuio、zharness、zplugin 的新版本必须先从各自仓库发布，SSMCP 工作流不会替它们递增版本或创建 release。

已有 tag 的任务失败时，使用该 tag 重试，保持同一套源码、子模块和制品。修复源码或依赖后需要新 commit、新版本和新 tag，不移动旧 tag。固定归档时间戳用于减少非源码差异，不构成跨任意机器的可重复二进制构建承诺。

## 发布证据如何判断

| 声明 | 所需证据 |
| --- | --- |
| 发布流程已经配置 | 本仓库 workflow、脚本及本机回归 |
| 固定源码已通过官方构建 | 对应 commit 和子模块的 checks、六平台 build、finalize 结果 |
| 二进制已正式发布 | GitHub Release 附件、大小与 SHA-256 |
| crates 包已发布 | crates.io 精确版本、未撤回状态与匹配 checksum |
| 已安装或已部署 | 目标机器上的安装/服务版本与独立回执 |

发布说明保留安装方式，并附加匹配版本的 docs/release-notes-版本号.md。GitHub 预编译归档使用 release-manifest.json；网关 Publisher 的同源原始客户端 release.json 是独立制品，不能用一个清单冒充另一个。0.10.15–0.10.22 在当前工作流的 deferred_versions 中，正式发布仍保留预编译归档和包发布，但不会自动部署生产 relay。其他版本是否部署取决于工作流的部署策略与 deploy_server 输入；维护者应核对当次 validate 输出。发布、服务端部署和客户端安装分别记录结果，源码推送不代表服务或用户客户端已更新。
