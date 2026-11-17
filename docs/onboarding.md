# 首次连接与 SSH 配置

`ssmcp setup` 将浏览器身份确认、本机公钥登记和 SSH 配置接成一个交互流程。向导完成后会验证连接，并继续运行当前客户端。已有可用的 SSH 连接可以直接复用；自动向导只在符合下文条件的首次连接失败时出现。

**公钥登记到当前 GitHub 身份对应的 ssmcp 账户。它不会写入 github.com 的账户 SSH keys 设置。** GitHub 用于确认身份，ssmcp 门户保存公钥并同步到目标服务器的 SSH 账户；只有公钥上传，私钥留在本机。[客户端流程](../src/client/onboarding.rs) · [门户接口](../src/portal/setup.rs)

## 开始配置

在要提供给 MCP 的项目目录中运行：

```sh
cd ~/projects/my-project
ssmcp setup
```

省略目标时，`setup` 使用 `ssmcp.lmm.best`。自建服务可以指定 SSH 主机，并在门户与 SSH 地址不同时显式给出门户 origin：

```sh
ssmcp setup --name my-laptop ssh.example.com

ssmcp setup --portal https://accounts.example.com \
  -p 2222 ssh.example.com
```

`--portal` 是账户网页的 HTTPS origin，例如 `https://accounts.example.com`，不包含路径、查询参数或登录凭据。SSH 端口由 `-p` 或已有 SSH 配置决定，门户使用自己 URL 中的端口。已有 SSH 别名可以直接作为目标；向导会通过 `ssh -G` 解析其实际主机和端口。门户和目标 SSH 服务应属于同一套 ssmcp 部署。

正常连接仍使用原来的命令：

```sh
ssmcp my-ssmcp

# 保持手工配置方式，关闭首次失败后的自动向导。
ssmcp --no-setup my-ssmcp

# 为单独的配置文件运行向导。
ssmcp setup -F ~/.ssh/work_config ssh.example.com
```

`setup` 不能与 `--no-setup` 或 `--list-tools` 同时使用。固定用户名的既有手工账户可以继续按[配置教程](configuration.md)管理，并用 `--no-setup` 保持该流程；门户向导使用浏览器授权账户对应的 SSH 用户名。[命令解析与连接入口](../src/client.rs)

## 终端与浏览器中的步骤

1. **核对门户和配对码。** 终端打印门户地址与短码，并调用系统浏览器打开授权页。自动打开失败时，手动访问打印出的完整 URL。
2. **用 GitHub 登录，并单独授权这台电脑。** 浏览器显示电脑名称、短码、GitHub 身份和公钥权限。核对它们来自当前终端，再点击授权按钮。仅完成 GitHub 登录还不够。
3. **检查本机 SSH 工具与账户公钥。** 缺少 `ssh` 或 `ssh-keygen` 时，向导先询问是否安装。工具可用后，向导查找本机 Ed25519 密钥，并与账户已登记公钥比较。
4. **按提示登记已有公钥或创建专用密钥。** 已登记且在本机找到的密钥优先复用；未登记的已有公钥需要终端确认；没有合适密钥时，需明确同意创建专用 Ed25519 密钥。
5. **确认电脑名称。** 直接回车使用方括号中的默认名称；也可以输入新名称。随后保存并检查 SSH 配置。
6. **验证连接并启动客户端。** 第一次连接的主机密钥确认仍由 OpenSSH 处理。公钥同步期间会短暂重试；验证成功后，终端继续显示客户端连接信息。

安装、登记公钥和创建密钥的确认接受 `yes` 或 `y`，大小写不敏感；直接回车表示拒绝。**电脑名称提示的回车含义不同：采用显示的默认值。** 已传入 `--name` 时以它为默认值，否则使用本机主机名；无法得到合适主机名时使用 `computer-use`。名称是客户端的显示信息，和 SSH 连接别名分开。

显式指定 `-i` 时只检查指定密钥；路径或对应 `.pub` 无效会显示具体原因，不改选其他密钥。新建专用密钥前，提示会说明它不设置口令，以便后台重新连接。已有密钥文件不会被覆盖。选择已有加密密钥时，前台由 OpenSSH 请求口令；后台会话需要可用于该会话的 SSH agent，或使用专用密钥。后台启动前还会检查无需交互的认证是否可用。[密钥选择、生成与验证](../src/client/onboarding.rs)

每次浏览器授权有效期为 **10 分钟**，最多允许登记一把新公钥；同一公钥重试是幂等的。完成、失败或取消后，客户端会尝试撤销这次授权；如果网络导致撤销请求未送达，原有到期时间仍生效。短码用于核对请求，设备授权秘密不会放进浏览器 URL。[授权状态与权限边界](../src/portal/setup.rs)

## 本地配置如何保存

向导默认修改 Linux/macOS 的 `~/.ssh/config` 或 Windows 的 `%USERPROFILE%\.ssh\config`；使用 `-F` 时修改指定文件。它为目标主机和端口生成稳定的 `ssmcp-…` 别名，把服务器返回的账户名、目标地址、端口和所选私钥路径放进有明确起止标记的管理段。

已有配置实际发生变化时，原始内容先保存为同目录下的 `config.ssmcp-backup-<随机后缀>`，再原子替换配置文件。其他配置段、原有换行和文件末尾格式保留。对相同输入重复运行不会重复添加管理段，也不会在内容未变时生成新备份。符号链接、异常权限、过大的配置或不完整的管理标记会得到明确诊断。[配置写入与幂等测试](../src/client/onboarding/ssh_config.rs)

保存后，向导运行 `ssh -G` 检查 OpenSSH 实际采用的主机、用户名、端口和身份文件，然后执行注册握手。这两步分别验证“配置被正确解析”和“服务器接受本机连接”。本地解析成功不能代替网络连接验证；如果后续连接仍失败，终端会保留诊断和已保存的配置供重试。

专用密钥位于用户目录的 `.ssh/ssmcp/` 下，文件名包含目标别名。名称选择用于本次客户端启动；后台会话会保存启动参数。以后手动启动时，如需固定显示名称，可以继续传入 `--name`。终端会打印实际连接别名和备份位置，后续可使用：

```sh
ssmcp --name my-laptop <终端打印的SSH别名>
```

中途退出不会回滚此前已经完成的本地密钥创建、公钥登记或配置保存。要撤销服务器认证权限，在该 ssmcp 门户删除对应公钥；删除公钥阻止后续 SSH 认证，已建立的连接仍需在本机结束。[账户与公钥管理](configuration.md#更新公钥)

## 什么时候自动进入向导

自动向导要求 **stdin 和 stderr 都连接交互终端**，没有设置 `--no-setup`，并且当前进程尚未成功完成过注册。符合这些条件的连接错误会先显示原始诊断，然后尝试一次向导。[触发条件](../src/client.rs)

| 启动情况 | 行为 |
| --- | --- |
| SSH 与注册握手成功 | 正常启动，不出现向导 |
| 当前进程首次注册前失败，交互终端可用 | 尝试一次向导 |
| 当前进程曾成功连接，随后由 `--watchdog` 重连 | 沿用重连策略，不因后续失败重新打开向导 |
| stdin 或 stderr 非终端，或由后台服务运行 | 输出连接诊断，不打开浏览器或等待交互输入 |
| 显式 `ssmcp setup` | 直接进入向导，要求交互终端 |
| 交互终端运行 `--daemon` | 在创建后台会话前检查连接，必要时先完成向导 |

因此，CI、定时任务和服务应提前配置 SSH。可以先在终端运行 `ssmcp setup`，再用打印出的别名启动后台会话。网络不可达、服务器未启动和主机密钥变更仍需要按其具体诊断处理。[后台会话说明](session-control.md#后台运行与延时重连)

## 缺少 SSH 时安装什么

依赖安装只在终端明确同意后执行。优先使用已发现的 `ssh` 与 `ssh-keygen`；安装后重新检查工具是否可用。[发现与安装实现](../src/client/onboarding/dependencies.rs)

| 系统 | 缺少工具时的处理 |
| --- | --- |
| Linux | 使用已安装的 apt、pacman 或 dnf 获取发行版仓库中的 OpenSSH 客户端包。下载和应用分两步执行；应用阶段使用本机缓存。系统包管理器可能通过 sudo 请求管理员权限。 |
| macOS | 优先使用系统自带工具；需要补齐时使用已有 Homebrew 下载并安装 OpenSSH bottle 与依赖。没有 Homebrew 或当前平台缺少可用 bottle 时，按诊断修复系统工具或自行安装适合该平台的 OpenSSH，再重试。 |
| Windows | 从 Microsoft 的 `PowerShell/Win32-OpenSSH` 官方 Release 元数据取得当前架构的便携包及 SHA-256。验证后放入 `%LOCALAPPDATA%\ssmcp\openssh\packages\`，由 ssmcp 直接选用，不需要安装 SSH 服务或修改全局 PATH。终端显示实际版本标签，包括上游标签中的 Preview。 |

Windows 下载后的 ZIP 校验需要 `ZipArchiveEntry.ExternalAttributes` 能力。便携 SSH 解压优先使用已安装的 PowerShell 7（`pwsh`），再查找 Windows PowerShell。使用 Windows PowerShell 5.1 时，要求 .NET Framework 4.7.2 或更高版本，例如 4.8；仅有“PowerShell 5.1”版本号不足以证明满足条件。安装器和便携 SSH 解压器会先检查该属性，缺少时明确停止，不跳过条目类型与属性验证。[Microsoft API 适用范围](https://learn.microsoft.com/en-us/dotnet/api/system.io.compression.ziparchiveentry.externalattributes?view=netframework-4.8.1)

Windows 的便携 SSH 依赖补齐和 ssmcp 自身的 Release 安装器是两个流程。前者管理专用 SSH 工具；后者安装 `ssmcp.exe`，默认可以把 ssmcp 的安装目录加入用户 PATH。参见[安装与发布说明](releases.md)。

## 下载期限、镜像与可信摘要

ssmcp Release 和 Windows 便携 SSH 包按“官方源 → `ghfast.top` → `gh-proxy.com`”顺序尝试。下载失败、停滞、持续低速或摘要不符时，丢弃该次结果并继续下一条公开下载路径。**可信摘要必须先从官方来源取得**：ssmcp 安装器使用官方 Release 的 `SHA256SUMS`；Windows SSH 依赖使用 GitHub 官方 API 提供的 SHA-256 asset digest。取不到可信摘要时停止安装，不把镜像提供的摘要作为替代信任依据。[Windows 依赖下载](../src/client/onboarding/dependencies.rs) · [Shell 安装器](../scripts/install.sh) · [PowerShell 安装器](../scripts/install.ps1)

镜像只接收公开文件 URL。GitHub 登录、设备授权秘密和公钥登记请求只发给选定的门户，不通过下载镜像。门户 API 拒绝重定向；客户端还会检查浏览器授权 URL 与所选门户同源。

macOS Homebrew 的 bottle 下载按官方源、TUNA、USTC 顺序尝试。回退只在本次子进程中更换 `HOMEBREW_BOTTLE_DOMAIN`，公式元数据和校验值仍沿用 Homebrew 的官方或本机既有来源；不会把 `HOMEBREW_API_DOMAIN` 改为公共镜像。子进程清空私有 registry 凭据，避免把它们转发给自动选择的镜像。若 `brew.env` 中的下载或凭据配置会覆盖这项策略，向导报告对应文件并停止，不修改或绕过集中配置。[Homebrew 回退实现与 fixture](../src/client/onboarding/dependencies/macos_brew.rs) · [Homebrew 环境变量说明](https://docs.brew.sh/Manpage#environment)

Debian／Ubuntu 的 APT 路径先解析具体包、大小和 SHA-256，再下载相同内容。对已识别的官方仓库地址，按原地址的 HTTPS 版本、TUNA、USTC 顺序回退；其他公开仓库只尝试原主机的 HTTPS 地址，不映射到公共镜像。APT 下载沿用本机进程的代理配置。下载后再次解析依赖计划，任何计划、大小或摘要变化都会停止。最终应用仍由正常 APT 路径验证仓库信任与包内容，缺失缓存不能联网补包；仅取得一个下载计划并不等于已经通过仓库认证。pacman 和 dnf 继续使用发行版既有的仓库与镜像配置。[APT 实现与 fixture](../src/client/onboarding/dependencies/linux_apt.rs)

下表列出主要网络预算；“每条路径总时间”包含该次下载的连接与正文读取，失败后下一条路径重新计时。KiB 和 MiB 分别按 1024 和 1024² 字节计算。

| 下载流程 | 连接、首字节与停滞 | 持续低速判断 | 总期限与大小 |
| --- | --- | --- | --- |
| Linux/macOS 的 ssmcp Release 安装器 | 连接最多 8 秒 | 连续 12 秒低于 64 KiB/s | 每条路径最多 180 秒；摘要文件 256 KiB，压缩包 128 MiB。独立文件写入上限同时约束不声明长度的响应。 |
| Windows 的 ssmcp Release 安装器 | 每次连接与响应头最多 8 秒；正文每次等待读取最多 10 秒 | 8 秒窗口内平均低于 64 KiB/s | 每条路径最多 180 秒，重定向共享该期限；摘要文件 256 KiB，压缩包 128 MiB。使用 .NET 流式计数，在超限字节写盘前停止。 |
| Windows 的便携 SSH 包 | 连接 8 秒；从请求开始最多 12 秒收到首字节；后续停滞最多 10 秒 | 8 秒窗口内平均低于 64 KiB/s | 每条路径最多 180 秒；受官方声明大小限制，且不超过 64 MiB。官方元数据最多 30 秒、1 MiB，低速阈值为 1 KiB/s。 |
| APT 包下载 | 连接最多 8 秒；每次响应头最多 12 秒；正文停滞最多 12 秒 | 12 秒窗口达到后，在下一数据块检查平均速度是否低于 64 KiB/s | 每个包的每条路径最多 120 秒；整组包共享 600 秒。每包最多 64 MiB，计划最多 128 包／256 MiB；两次解析计划各最多 20 秒。 |
| pacman／dnf 包下载 | dnf 超时 10 秒；pacman 的 curl 连接超时 8 秒 | dnf 为 64 KiB/s；pacman 连续 12 秒低于 64 KiB/s 时停止 | 下载命令最多 150 秒，终止宽限 5 秒；pacman 单次 curl 最多 120 秒。重用发行版配置的仓库与镜像。 |
| macOS Homebrew 下载 | curl 配置为 8 秒；Homebrew 的部分镜像请求另设 15 秒 | 连续 12 秒低于 64 KiB/s | 每条 fetch 路径硬限 180 秒；三条路径共享 480 秒预算，另留已有进程清理宽限。成功后才开始安装。 |

发行版和 Homebrew 路径继续使用各自的仓库校验机制。Linux/macOS 的下载截止时间用于获取阶段；**已经开始的本机包应用事务没有网络计时器强行终止**。Windows 解压在专用临时目录执行，校验路径并限制条目，验证工具可运行后才切换专用版本。

ssmcp Release 的下载或校验失败保留已有安装。所有公开镜像均失败时，修复官方网络访问或按[发布说明](releases.md)手动取得并核验对应平台包后重试。下载耗时导致十分钟设备授权过期时，可以重新运行 `setup`；它会重新检查已安装的 SSH 工具和本机已有密钥。

## 服务端准备与故障恢复

使用向导前，**客户端和服务器都需要包含本次功能的版本**。服务器必须启用 GitHub OAuth、账户门户和 SSH 公钥同步，并向同一 HTTPS origin 提供 `/setup`、`/api/setup/*` 以及现有 GitHub 回调。只更新客户端不能给旧服务器补上这些 API；修改源码后还需要安装新 `ssmcpd` 并更新实际运行服务。[门户与 SSH 同步](deployment.md#account-portal) · [GitHub OAuth 配置](deployment.md#github-oauth)

| 现象 | 处理 |
| --- | --- |
| 提示服务器未启用 CLI setup，或返回 404 | 更新实际运行的 `ssmcpd`，确认门户功能和反向代理路由；已有手工 SSH 账户可继续使用 `--no-setup`。 |
| 浏览器授权被拒绝或已过期 | 确认是本机发起的请求，再重新运行 `setup`；浏览器登录本身不会替代授权按钮。 |
| 终端提示不能自动打开浏览器 | 手动打开终端给出的同源 URL，并核对短码。 |
| 公钥已登记，短期内仍拒绝 SSH | 等待服务器公钥同步后重试；向导验证阶段最多尝试 6 次，时间预算为 45 秒。 |
| `ssh -G` 检查失败 | 阅读终端给出的配置诊断和备份路径，检查自定义 Include、Host/Match 和身份文件设置。 |
| 前台能连接，后台认证失败 | 为后台会话准备可用的 SSH agent，或重新选择专用密钥；先完成 OpenSSH 主机密钥确认。 |
| 提示需要交互终端 | 在真实终端先运行 `ssmcp setup`，再启动服务或自动化任务。 |

这些说明描述实现行为，不代表当前线上服务已完成升级。仓库的[客户端流程测试](../src/client/onboarding/flow_tests.rs)、[门户 HTTP 测试](../src/portal/setup_tests.rs)、[下载与进程生命周期测试](../src/client/onboarding/dependencies.rs)和[真实 HTTPS 字节上限 fixture](../scripts/test_installer_download_bounds.py)覆盖相应协议及失败路径；原生 Windows/macOS 环境和三平台桌面交互的实际验收范围仍以对应记录为准。电脑与浏览器权限见 [computer-use 指南](computer-use.md)。


## 浏览器操作环境

浏览器环境可以独立于 SSH 配置，不要求先登录门户：

```sh
ssmcp setup browser --check
ssmcp setup browser --install
ssmcp setup browser --check --json
```

默认只检查。`--install` 使用 Node.js 18+ 和 npm，在 ssmcp 用户数据目录安装并验证固定版本的 Playwright MCP，重复执行复用已有版本。检查会区分运行时缺失、provider 缺失、扩展待升级与首次配对，并显示 Linux 独立桌面依赖。浏览器已有 profile、账号和 ChatGPT 扩展保持原有管理方式。首次配对采用扩展自身的确认流程。[完整配置与后台操作](background-control.md)
