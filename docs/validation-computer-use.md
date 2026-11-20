# computer-use 验证记录

三平台桌面适配器、原生 CDP 浏览器控制、21 个 Omarchy 系统原语和首次连接向导已经进入源码。开发机的 `ssmcp-computer-use` 会话已通过真实连接验证：47 个原生工具可发现，11 个 Omarchy 只读 getter 成功，合成浏览器页面完成中文输入、点击、读回和 JPEG 截图。这里的浏览器操作经过 ssmcp 原生接口，没有调用另一个浏览器控制器或托管模型。

**实现与开发机运行态已验证；最终提交的六平台 CI 被现有登录钥匙串锁定所阻塞，尚未产生运行记录。** 最终源码为 [dbb98b020b8a35f1e7ff2184540fbebdf0e3d7f1](https://github.com/LIghtJUNction/ssmcp/commit/dbb98b020b8a35f1e7ff2184540fbebdf0e3d7f1)，相对 be42 仅修改四个测试 fixture。完整本地 gate 和最终 CLI 验收使用 be42 的不可变产品二进制；运行中的兼容客户端来自 592b。下文分别记录三种版本和证据，没有将旧提交的成功结果写成 dbb 的全平台通过。

接口、坐标与运行条件见 [computer-use 文档](computer-use.md)；授权、公钥和 SSH 配置流程见 [首次连接文档](onboarding.md)。完整命令、计数、耗时与哈希见 [机器可读报告](reports/computer-use-20261117.json)。

## 版本与证据范围

| 对象 | 不可变版本 | 本报告采用的证据 |
| --- | --- | --- |
| 最终仓库代码与测试 | [dbb98b0](https://github.com/LIghtJUNction/ssmcp/commit/dbb98b020b8a35f1e7ff2184540fbebdf0e3d7f1) | 现有 login 钥匙串锁定，六平台 CI 尚无 run；相对 be42 仅修测试同步、HTTP 等待预算和完成标记发布 |
| 完整本地 gate 的产品 | [be42b03](https://github.com/LIghtJUNction/ssmcp/commit/be42b03fc2651b830ff0f0f3ee1427590344f730) | Linux 完整 gate、归档二进制的 8 项真实 CLI／PTY 验收、76 项终端 dashboard 检查 |
| 开发机实际运行客户端 | [592b0a3](https://github.com/LIghtJUNction/ssmcp/commit/592b0a3ab729c403d6380d7169775bd8979ef5d1) | 独立构建和 143／47 项 E2E 后上线；真实原生工具、Omarchy 只读与浏览器操作 |

源码支持、操作系统 runner 和真实桌面验收分别回答不同问题。

| 层次 | 已观察的内容 | 能证明的范围 |
| --- | --- | --- |
| 实现支持 | Linux Hyprland／X11、Windows Win32、macOS Quartz／Accessibility 适配器及原生 CDP | 对应实现、参数校验、权限诊断、资源限制和释放代码已存在 |
| 原生操作系统 runner | Windows、macOS、Linux 的编译、Clippy、自动化测试 | 具体提交在相应系统上的可构建性与测试结果 |
| 真实连接与只读系统调用 | 开发机 Hyprland 能力发现、Omarchy 11 个 getter、原生 browser 合成页面 | 本次连接、系统读取、CDP 输入与图片返回链路 |
| 真实物理桌面交互 | 本轮尚未执行 click、type、drag | 本报告不对物理桌面输入作通过结论 |

Windows 的 GDI／BGRA／JPEG 单元测试操作内存图像；macOS 专属测试不要求桌面授权。它们不覆盖已登录桌面的权限、焦点、屏幕截图或混合 DPI。旧版 [native-control 历史记录](validation-native-control.md) 仍属于其原版本。

## 最终平台 CI

dbb98b0 的 CI 派发被开发机现有凭据存储阻塞：Secret Service 确认 login 集合 Locked=true，按已配置 GitHub 账号匹配的条目为 unlocked=0、locked=1。官方 GitHub API 的只读网络探测返回 HTTP 200，耗时 0.24056 秒；问题不在公网连接。GitHub 插件的仓库读写已经完成，但当前插件没有 workflow dispatch 工具。受期限约束的凭据读取已停止，没有绕过钥匙串，也没有生成该 SHA 的 CI run。

继续验收需要用户在开发机解锁已有的 login 钥匙串，然后使用现有 GitHub 凭据派发相同代码 SHA：

```sh
gh workflow run ci.yml --repo LIghtJUNction/ssmcp --ref main -f ref=dbb98b020b8a35f1e7ff2184540fbebdf0e3d7f1
```

阻塞证据 `final-ci-auth-block.json` 的 SHA-256 为 `aa5dbee6f02b2fc9823ee77a8123b87bb44b1b408d02e67acf85369addb6d400`。这项外部认证条件尚未满足，因此本报告不宣称最终 CI 全通过。

此前 be42 的 [CI 43938106333](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333) 已结束，四个 job 成功、两个 job 失败，具体如下。它不是“整个工作流通过”。

| 平台 | be42 job 结果 | 明确观察 |
| --- | --- | --- |
| Linux x64 | [失败](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333/job/133547987175) | fixture 的 HTTP 10 秒等待先于产品的 20／30 秒期限结束；最终测试提交改为 35 秒，产品期限未改 |
| Linux ARM64 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333/job/133547987900) | 整个 job 成功 |
| Windows x64 | [失败](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333/job/133547987728) | lib 271 通过、2 失败、1 忽略；两项 stdin 测试错误地用 2 毫秒等待推断管道写满 |
| Windows ARM64 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333/job/133547987558) | lib 273 通过、1 忽略；release_client 4 通过；7 项 Windows adapter 测试、PowerShell 7 与系统 PowerShell 5.1 helper 通过 |
| macOS ARM64 | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333/job/133547988111) | lib 317 通过、2 忽略；release_client 4 通过；7 项 adapter、5 项 Homebrew 路由和 3 项真实下载字节上限 fixture 通过 |
| macOS Intel | [成功](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333/job/133547987718) | 同样为 lib 317 通过、2 忽略；release_client 4 通过；adapter、Homebrew 路由和字节上限 fixture 全部通过 |

两个 macOS job 的 lib 测试分别耗时 6.92 与 8.35 秒，三项真实下载 fixture 分别耗时 35.948 与 36.573 秒。其汇总文件 `ci-43938106333/macos-summary.json` 的 SHA-256 为 `46041e2eae6db1194a4f3ea1f2c6050b060219cecc122d79d385f46385c5a29a`。

最终 stdin 测试保留同一个确实返回 Pending 的写 future 和已经持有的 stdin 锁，再检查取消／期限通知能否先于锁释放到达。它不再依赖 Windows 管道缓冲容量或短时调度猜测。Linux 上的 Windows lib 交叉编译和 PE 链接已通过。修复后的原生 Windows 执行受上述 CI 认证阻塞影响，尚未完成，不能将交叉编译写成原生测试通过。

## be42 的完整 Linux 本地 gate

完整 gate 于 2026-11-16 19:20:12–19:23:54 UTC 运行。121 个源码文件在各步骤前后保持相同，源码集合 SHA-256 为 `b69c7dc15971ab7cf2dcd6e13248e63c7df99884a0bf56f72c132c0ec9c4040c`。环境采用 `CARGO_BUILD_JOBS=1`、`RUST_TEST_THREADS=2`；完整 451 项 lib 集合没有跳过下载 fixture，其中 444 通过、7 项按测试声明忽略。

| 检查 | 结果 | 命令耗时 |
| --- | --- | ---: |
| cargo fmt 检查 | 通过 | 1.234 秒 |
| all-targets Clippy，warnings 视为错误 | 通过 | 26.382 秒 |
| 全部 lib 测试 | 444 通过、7 忽略 | 79.344 秒，含编译；测试本身 21.80 秒 |
| binary 单元测试 | 5 通过 | 34.385 秒 |
| 14 个 integration target | 81 通过、14 忽略 | 64.136 秒 |
| binaries 与 vt_snapshot 构建 | 通过 | 1.519 秒 |
| core E2E | 143 项通过 | 6.626 秒 |
| computer-use E2E | 47 项通过 | 7.179 秒 |
| Omarchy 只读 smoke | 1 项测试、11 个 getter 全部成功 | 0.686 秒 |

主证据 `linux-final-validation-attempt2-gates.json` 的 SHA-256 为 `be318f8746f0cccab99a52ef316d342038e617d076eed996674d1b70d6c04264`。computer-use E2E 没有使用 `--desktop`，观察到的 Chromium 为 157.0.8103.38。修复 HTTP fixture 等待预算后，同一个 be42 二进制的 47 项检查再次通过，耗时 6.108 秒；该次证据单独保存，没有覆盖完整 gate。

| 归档二进制 | SHA-256 |
| --- | --- |
| ssmcp | `a87130337ce6278cf060aa5af01bb4b2a9d96b7475ba1e75ac3f46c380e6503b` |
| ssmcpd | `70d8ab0b57c7c2124c8ac1ac47f143a752d09d7392a1b5a2fa28ed1b50cd1152` |
| vt_snapshot | `cb7c8833429fa395b961d29741bfaece749189fddabb9827742c6f9bce153c35` |

三份文件保存于开发机 `target/computer-use-task/artifacts/be42b03fc2651b830ff0f0f3ee1427590344f730/`。后续兼容构建复用 build 目录，但没有覆盖这份归档。

## 首次连接、SSH 与门户

最终真实 CLI／PTY fixture 使用上述归档的 ssmcp 0.6.1，测试脚本来自 dbb98b0，SHA-256 为 `97d752fd88b0f011a78fbc5c671460f5a5268259757dbc483411ed63aedf6480`。八项全部通过，整体退出码 0，耗时 38.241 秒；二进制和 fixture 的前后哈希均未变化。

| 场景 | 已验证行为 |
| --- | --- |
| 首次连接失败后 Ctrl-C | 打开合成授权入口，取消 grant，保持原配置 |
| 显式 --no-setup | 失败直接退出，不请求 setup API、不打开浏览器 |
| 非 TTY | 失败直接退出，不进入交互向导 |
| 探测收到 TERM | SSH 和 ProxyCommand 已停止，外层 keeper 仍存活，PTY 尚未关闭 |
| 探测达到期限 | 同样在 PTY 关闭前确认进程树清理，随后能够进入并取消向导 |
| 主机密钥确认时取消 | 真实 OpenSSH 提示，恢复终端模式 |
| 密钥口令提示时取消 | 真实加密私钥提示，秘密不回显，恢复终端模式 |
| 提供口令完成认证 | 临时 sshd 实际接受加密密钥认证；秘密不回显，恢复终端模式 |

每个 CLI 子进程使用测试拥有的私有 HOME／USERPROFILE／.ssh。测试使用回环合成门户、临时 sshd、自有密钥和浏览器日志桩，没有修改调用者的真实 SSH 目录，也没有向真实 GitHub 身份登记公钥。三项提示场景还检查私钥不变、CLI 没有调用 keygen 和终端设置恢复。

最终报告 `onboarding-cli-e2e-be42-after-atomic-marker.json` 的 SHA-256 为 `65e7691111e051dc4913ea8b53219fa31dc7f87cb2f43e0682fba19e606137ed`；wrapper summary 的 SHA-256 为 `1da72c878d68dc82b7797e445afc59e4de0302651e1b15fc5f1e9ba0ca396516`。这里 `--no-setup` 与非 TTY 场景的进程退出码 1 是预期断言，不是 fixture 失败。

门户的早期定向测试为 16 项通过，使用实际回环 HTTP 请求覆盖明确同意、同源 CSRF、账户隔离、过期与重放、单次新公钥登记、相同密钥幂等重试和 GitHub 回调返回绑定。最终 be42 的完整 lib gate 重新覆盖了门户与配置测试。独立的 Unix 配置锁回归还保留一个重复 fd，验证锁守卫退出会显式解锁，同时保留排他和重复保存幂等断言。

### 授权页视觉检查

通过 ssmcp 原生 browser 检查生产 consent renderer 导出的合成页面，使用虚构账户与比较码。1265 × 873 的桌面布局和 500 × 1113 的窄布局均未观察到横向溢出，批准与拒绝按钮可辨认。Chromium 将请求的 390 px 视口限制为 500 px，因此不将这次观察写成 390 px 手机布局验收。任务拥有的预览浏览器、客户端和服务已经关闭。

| 仓库图片 | 字节数 | SHA-256 |
| --- | ---: | --- |
| [computer-use-consent-desktop.jpg](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/computer-use-consent-desktop.jpg`） | 48726 | `7042f5cbec9a8fa2d5aca6d1b2f56a7346722de9bd433b62c4740e1fae91f392` |
| [computer-use-consent-narrow.jpg](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/computer-use-consent-narrow.jpg`） | 37636 | `99289f0c8895bac65ed6db0e123ef05c8d691cea4600e46603e5f443b846f5b1` |

图片按原字节复制，没有编辑。视觉检查没有点击真实账户授权，也没有发送物理桌面输入。

[合成授权页的桌面布局](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/computer-use-consent-desktop.jpg`）

[合成授权页的 500 px 窄布局](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/computer-use-consent-narrow.jpg`）

## 下载、镜像与安装边界

检查执行实际生产下载／解压辅助代码和隔离 fixture，没有运行系统安装。详细期限与镜像策略见 [首次连接文档](onboarding.md)。

| 检查 | 结果与范围 |
| --- | --- |
| Shell 未知长度字节上限 | 真实 curl 读取本地 TLS chunked 响应；移除 curl 自带 max-filesize 参数，仍验证精确 65536 字节的内核写入限制、正常长度成功及后续下载不继承限制。Linux 与 be42 两个真实 macOS runner 均通过三项测试 |
| Shell 安装器行为 | 合成版本程序覆盖安装／更新、镜像回退、停滞、篡改、版本不符和旧安装保留 |
| PowerShell 生产 helpers | 已验证的便携 PowerShell 7.7/Linux 和原生 Windows runner 的 PowerShell 7／系统 5.1 运行实际辅助代码，覆盖独立计数字节上限、期限、根目录提取、重复条目和路径别名 |
| Homebrew | 五项实际假 brew 子进程 fixture 检查官方→TUNA→USTC 路由、验证失败不安装、每路期限、共享预算与安装阶段不受下载计时器终止；没有执行真实 Homebrew 安装 |
| APT | 七项计划、URL、缓存、配置与回环 HTTP 测试覆盖低速回退和相同长度篡改拒绝，最终 be42 完整 gate 已覆盖生产代理继承的最后修复 |
| APT 原生认证门 | 真实 APT 3.0.3 的正常 install 路径配合自有 dpkg sentinel；可信完整缓存到达 sentinel，未认证、继承认证绕过、缺缓存和篡改缓存均在 dpkg 前拒绝 |

ZIP 的 ExternalAttributes 检查需要 .NET Framework 4.7.2 或更高能力；单独的“PowerShell 5.1”版本号不能证明该 API 存在。安装器与解压代码会先检查能力；客户端优先选择已安装的 PowerShell 7。镜像只提供下载字节，预期摘要来自官方或受信任包管理器元数据，不能把镜像自己的摘要直接当作可信来源。

APT sentinel 只记录是否走到安装门，不执行软件安装。Homebrew fixture 只验证编排与进程行为。这些检查不应被写成开发机已成功安装系统包。

## 开发机运行状态与公网边界

开发机的新会话名和 systemd unit 为 `ssmcp-computer-use`／`ssmcp-computer-use.service`。它运行真实构建的 592b 客户端，SHA-256 为 `874c27a7989c541a724c86a5f660ec895926e4f6f25e038347b3709e8aa4ff5c`。独立构建、143 项 core E2E 和 47 项 computer-use E2E 均已通过，证据为 `runtime-compatible-gates.json`，SHA-256 `68004da6d317978f52fa76af3df3babd919a88563128973f42997d8f98256389`。

使用兼容提交有明确原因：并行 main 的终端改动将 wire 协议升级为 5，当前公网 broker 只接受 4。be42 客户端因此被服务器拒绝。运行态采用本来就使用 wire 4 的 592b 提交；没有伪造协议号，没有回退 main，也没有自动升级公网 broker。12 个关键 computer-use、Omarchy、onboarding、ASKPASS、依赖镜像和 SSH 配置文件已逐项确认与 be42 相同。

真实连接观察到以下结果：

- 11 个 computer、15 个 browser、21 个 Omarchy 工具，共 47 个原生接口。
- Hyprland 的 9 项能力发现均 available；这说明当前 helper 与会话可发现，不是物理输入验收。
- Omarchy 4.5.0-3 的 status、themes、backgrounds、fonts、audio、brightness、toggles、workspaces、apps、battery、network 共 11 个只读 getter 成功；没有修改主题、音量或工作区。
- 任务拥有的 headless 浏览器通过原生接口完成启动、打开合成页面、snapshot、中文 fill、click、结果读回、JPEG 截图及 disconnect；结束后 connected=false。
- JPEG 为 800 × 413、13711 字节。图片从原生 screenshot 的 MCP image block 返回；公网服务当前没有 `image()` 便捷 helper，这不影响已验证的原生图片返回链路。
- 四个模型后端均显示 unavailable、tools=0；本次原生能力验收没有依赖托管模型。

[computer-use-runtime-browser.jpg](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/computer-use-runtime-browser.jpg`） 保存了这次运行态截图的原字节。这是任务拥有的本地合成验证页面，不是 GitHub OAuth 真登录或真实账户授权。

[通过 ssmcp 原生接口操作的合成浏览器验证页](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/computer-use-runtime-browser.jpg`）

旧 `ssmcp-native-controls.service` 已于 2026-11-16 19:57:18 UTC 完成交接检查，状态为 inactive／dead、MainPID=0；新 `ssmcp-computer-use.service` 为 active／running，NRestarts=0。最终实时摘要 `development-runtime-verification.json` 的 SHA-256 为 `ca9fc4ad57172c0e73068047520ef051c0c21dd9504134788e05bf1a91f7de4a`。浏览器 JPEG 的 SHA-256 为 `d19d23713d97c825b09398c6e0050f5262da49d99c7f30941689c2a4ebeaf02e`。

公网 broker 没有由本轮任务部署，正式 Release 也没有由本轮发布。公共 `GET /setup` 返回 401 空响应，只能说明入口要求认证，不能用它判断新 setup API 已部署。新的首次连接向导仍需要匹配的服务端更新，main 的 wire 5 客户端也需要匹配 broker。

## 验证中发现并修复的问题

保留失败记录的目的，是让后续读者能区分产品缺陷、测试自身的竞争和环境中断。没有通过跳过平台、删除断言或扩大产品期限来改写结果。

| 发现 | 修复与后续证据 |
| --- | --- |
| macOS InputRelease 使用结构体更新语法触发 E0509 | 完整初始化字段，保留 Drop 释放；后续两个 macOS 原生 job 通过 |
| Linux Hyprland fixture 误在 macOS 执行 | 仅将相应测试限定为 Linux，产品分派不变 |
| Darwin 输出锁冷创建 ENOENT 被全局 store 缓存，连带 23 项测试失败 | exclusive create，仅 AlreadyExists 时普通打开；保留 dirfd、no-follow、所有权、链接数和锁检查。集成采用并发合入的 [PR #8](https://github.com/LIghtJUNction/ssmcp/pull/8) 及其 32 × 8 冷创建／inode／符号链接测试，两个 macOS 完整 lib 后续通过 |
| Apple shell 的 ulimit -f 单位与 Linux 不同 | Apple Bash 3.2 使用 1024 字节，原实现误按 512 换算，65536 上限实际变成 131072。按真实 shell 单位换算，保留精确内核限额断言，两个 macOS 三项 fixture 全通过 |
| SSH 外层期限后 ProxyCommand 仍存活 | 进程树所有权与私有 ASKPASS 桥保留交互提示，同时清理 SSH 子树；最终八项真实 CLI／PTY 全通过 |
| Unix fork 继承 flock 后，单纯关闭父 fd 未释放锁 | 真实 fork 对照复现，Unix 锁守卫显式解锁；Windows 保留原有单字节范围锁语义；排他与幂等测试通过 |
| 普通连接因找不到相邻 ssh-keygen 而绕开 PATH ssh | 普通连接保留 PATH ssh，setup 仍检查成对依赖；reload 与完整 gate 通过 |
| 显式 -i 无效时可能选中其他默认密钥 | 立即报告指定路径与原因，不进入命名、上传或配置；真实流程回归已覆盖 |
| Windows stdin fixture 用 2 毫秒猜测管道写满 | 改为同一 Pending future 和持锁握手；没有修改产品 stdin 代码；交叉编译通过，最终原生 CI 受锁定钥匙串阻塞 |
| 浏览器 fixture 的 10 秒 transport 早于产品期限 | 仅改 fixture 为 35 秒，47 项断言和产品期限不变，重新通过 |
| CLI 加密认证完成文件先出现、JSON 后写入 | 先完成临时文件再原子替换；原 7／8 失败保留，最终 8／8 通过 |
| 大输出 fixture 用固定等待造成竞争 | 最终采用 PR #8 的双向 socket 握手，保留输出、截断与恢复断言，完整 lib 通过 |

Darwin 冷创建判断与 [Go 官方 issue](https://github.com/golang/go/issues/81246) 和 Apple 公布的 XNU 路径相符；没有收集内核 trace，因此不把相似机制写成已证明的内核根因。文件上限单位则直接核对了 [Apple Bash 3.2 源码](https://github.com/apple-oss-distributions/bash/blob/main/bash-3.2/builtins/ulimit.def)，并由真实 macOS fixture 验证。

| 历史 CI | 对应版本与结果 |
| --- | --- |
| [43920293970](https://github.com/LIghtJUNction/ssmcp/actions/runs/43920293970) | 2b8e539：Windows 成功；Mac E0509；Linux 精确工具目录遗漏 Omarchy |
| [43920977650](https://github.com/LIghtJUNction/ssmcp/actions/runs/43920977650) | 835250d：Windows 两架构成功；Mac 暴露 Hyprland 测试范围与冷创建竞争 |
| [43929965349](https://github.com/LIghtJUNction/ssmcp/actions/runs/43929965349) | 663b2cd：Windows 两架构及系统 PowerShell 5.1 成功；Mac openpty fixture 的 const／mut 指针不符，随后只修测试参数 |
| [43935775400](https://github.com/LIghtJUNction/ssmcp/actions/runs/43935775400) | 592b0a3：Windows 两架构成功；Mac 原生 Rust 测试通过但最终 shell 字节上限失败；Linux dashboard fixture 后续修复 |
| [43938106333](https://github.com/LIghtJUNction/ssmcp/actions/runs/43938106333) | be42：两个 Mac、Windows ARM64、Linux ARM64 整 job 成功；另两项测试同步／等待预算由最终 test-only 提交修复 |

592b 的一次本地 gate 在 fmt 和 Clippy 通过后被 systemd-oomd 中断。19:09:47 UTC，旧客户端服务及 146 个进程被杀，19:09:52 自动重启；lib 尚未产出结果。这轮既不算通过，也不记为断言失败。后续 be42 在较低并发下完成全部 gate，没有改变系统内存设置。早期分阶段快照、初次 CLI 清理失败和原子标记修复前失败都独立保留在 JSON，不覆盖后续结果。

## 尚未覆盖的范围

本轮没有进行真实 Windows／macOS／Linux 物理桌面的 click、type、drag 验收。权限授予／拒绝、锁屏、焦点策略、多屏截图、Retina／混合 DPI，以及取消时实际按键释放，仍需要在对应系统的独立测试窗口中观察并绑定二进制哈希。合成浏览器输入、内存图像和只读系统发现分别只证明各自链路。

报告只记录确实观察到的命令、计数、时间、提交与哈希。缺少精确数据时，JSON 保留 null；测试原始日志留在开发机证据目录及 GitHub Actions，公开报告不复制授权秘密或完整 CI 日志。
