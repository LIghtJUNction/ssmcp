# ssmcp 历史恢复与继续迭代记录

日期：2026-11-20；面向用户的时间采用 Asia/Taipei。

## 恢复结论与本轮边界

本轮找回了本地会话、Git 提交、正式 Release、运行进程与验收记录的对应关系。恢复时的正式版本是 **v0.10.13**，旧文档中的 v0.10.12 Windows 发布失败已经修复；007 恢复服务当时仍运行 v0.7.4，本轮已完成专用副本的现场升级。此前未完整合入的线程监听和终端滚动分支也已经接回主线。

本轮 v0.10.14 产品源码冻结于 `78442aa0849cc1a168db039ff7427ea076594409`。它整合账号接入、线程监听、zuse 桌面工作进程、独立 ztuio、等待用户时的休眠界面，以及真实账号端到端检查发现的启动撤权竞态修复。后续仅记录证据和发布说明的提交与产品源码分别标注。

**本文件当前是正式发布前的验收记录。** 新产品的格式、严格 Clippy、三类 bin 构建、完整库测试与 47 项账号回归均已通过，整合已经 PR #104 合入主线 `19458a18e320e4045281fc9228039e6c1edbd300`。发布前审计发现旧普通客户端默认握手与严格名单不兼容，正在处理这一真实部署问题。尚未创建 v0.10.14 tag 或进行生产部署；正式结果以实际 Release、工作流和 crate 记录为准。

## 找回的历史与实际处理

| 事项 | 恢复时的证据 | 本轮结果 |
| --- | --- | --- |
| v0.10.13 正式发布 | 源码 `4e3132d`；Release run 44610211292 的 19 个任务成功，约 04:51 完成 | 确认发布与部署记录，纠正旧 v0.10.12 失败判断 |
| 007 恢复服务 | 实际映像 v0.7.4、wire 5；网关要求 wire 7，进程存活但持续重连 | 升级官方 v0.10.13，校验映像并完成真实 MCP 往返 |
| 线程监听与来源约束 | 历史分支 `57d3065` 包含核心实现 `1001a0d`，未完整合入 | 整合、修正收尾夹具并重新验收，经 PR #102 合并 |
| 命令与整页分别滚动 | 原命令窗口未按正文左右两半分流 | 左半区滚动指向命令的内部输出，右半区滚动会话；PR #100 随整合进入主线 |
| zuse computer-use 接入 | 恢复时只有接入计划 | 固定子模块，通过普通受管进程调用 Desktop API，保留既有输入保护 |
| ztui → ztuio | SSMCP 内嵌 0.1.2 比旧独立库更新 | 上游归并完整运行时接口与绘制改进，消费者改用 canonical 子模块 |
| 等待用户的休眠画面 | 本轮追加的界面要求 | 全画面压暗与 ASCII 标识；点击唤醒不消耗等待，首个键盘输入仍可编辑 |
| 及时正式发布与依赖传播 | 本轮确认的维护要求 | 固定 v0.10.14 发布边界；新依赖组合由 Project Z 聚合，再由 SSMCP 验收 |

历史依据：[v0.10.13 Release](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.13)、[正式工作流](https://github.com/LIghtJUNction/ssmcp/actions/runs/44610211292)、[原始 listener 验收](https://github.com/LIghtJUNction/ssmcp/blob/57d3065ed2d65f579371e3cde170ebc6186f0ed0/docs/reports/thread-listeners-20261119/README.zh-CN.md)。[PR #101](https://github.com/LIghtJUNction/ssmcp/pull/101)、[PR #102](https://github.com/LIghtJUNction/ssmcp/pull/102)、[PR #104](https://github.com/LIghtJUNction/ssmcp/pull/104)与[当前接入文档 PR #105](https://github.com/LIghtJUNction/ssmcp/pull/105)保留分阶段来源。

## 007 与实际开发连接

007 的专用恢复映像从 v0.7.4 升级为官方 v0.10.13、wire 7，并恢复 connected 状态。原升级验收核对了十份保存定义、配置与 systemd unit，并确认当时原有十一个客户端保持。这里只升级恢复服务专用副本，没有把日常 CLI 或全部客户端算作已升级。[升级报告](../007-upgrade-20261120/README.zh-CN.md) · [原始证据](../007-upgrade-20261120/evidence.json)

发布前再次核对的活跃客户端基线已增加到十四个，两个独立快照中的客户端集合相同。007 的两次真实 status、systemd 主进程、运行映像和本地连接状态对应同一实例；官方 ELF SHA-256 仍为 `5ad6682a2e63e865b9a0ada78ce0d1b9b7799cf95e6bdad15c4661ecc106a7aa`。旧保存会话中的历史参数不作为该服务的实际启动证据。

日常开发连接实际仍运行 v0.10.12。新版冻结 CLI 只继承其账号配置引用，执行只读 whoami 后由当前服务器确认该账号已注册。007 与开发连接使用相同账号存储引用；007 发送空 allow，当前 broker 对已认证的空名单仍允许账号自身。该检查没有读取凭据正文、重新登录、修改启动配置或重启客户端。**007 的空名单路径在源码上兼容，但普通客户端不能据此类推。** 官方 v0.10.12/v0.10.13 普通客户端会将凭据中的 canonical GitHub principal 写入 Hello.allow，relay 原样发送；新版严格名单在 metadata 校验阶段拒绝旧格式。后续实际盘点发现十四个普通 v0.10.12 会话，另有一个独立 v0.10.13 007，因此不直接重载这些工作会话。当前正在审查和验证仅限已认证账号自身的旧默认握手归一化，其他名单校验保持严格。服务器切换后的真实重新连接仍须单独验收。

## 产品实现与行为

### 线程监听和终端滚动

HTTP 与 Unix socket listener 依据经过认证的线程及连接代际读取和投递状态。队列回执保留实际来源，旧连接不能冒充当前连接。滚动分流直接使用正文中的鼠标位置：左半区且指向命令窗口时改变该命令偏移；右半区改变会话偏移。回归同时检查其他命令窗口、草稿和光标，确认实际改动的状态。

冻结产品 `70aebd09d56cfa21cd0229da6c0e3c3cdaae2200` 的验证结果为：56 个去重后的定向 Rust 测试、118 项真实 CLI/PTY 强制发送检查、73 项 HTTP/Unix listener 检查、121 项用户等待检查、44 项 local 与真实 Chromium OAuth 检查通过；120、49、48 列滚动状态回归及 120/48 列 PNG 人工核验通过。格式、构建和严格 all-target Clippy 通过。[整合报告](../listener-integration-20261120/README.md) · [验证数据](../listener-integration-20261120/validation.json)

### zuse 与独立 ztuio

每次受支持的桌面请求启动一个普通操作系统工作进程，直接调用 zuse Rust Desktop API。父进程保留输入锁，直到输入释放且工作进程实际退出，并清理自己创建的截图目录。CLI 与 Linux daemon 注册相同入口；嵌入调用方缺少入口时在启动子进程前失败。这里没有额外模型或第二套代理循环。

既有 MCP Schema、隔离桌面默认选择与主机控制授权继续生效。Windows/macOS 输入与状态、明确请求光标的截图和显示器拓扑仍走兼容适配器；上游新增移动端或 DSL 能力不会自动成为 SSMCP 接口。已使用私有 Xvfb 桌面验证实际输入、截图、Unicode 与取消后输入状态恢复，未在宿主桌面发送测试输入。[桌面验收](../zuse-integration-20261120/validation.json) · [私有桌面 MCP](../zuse-integration-20261120/private-desktop-mcp.json)

ztuio 0.1.0 的运行时已与原内嵌 0.1.2 核对：公共接口、ANSI 差分帧与渲染优化保留。独立库的 13 项契约、格式、严格 Clippy、源码打包与消费者打包通过。历史 benchmark 的一个 unchanged_clone 计时项目未保留，旧 JSON 指标名称仍有 ztui；该差异不被写成运行时 API 退化，也没有复制旧核心回应用仓库。[来源与验收](../ztuio-integration-20261120/README.md)

### 等待用户时的界面

休眠遮罩只表示等待状态。点击唤醒后，原来的等待 ID、future、草稿、队列和浏览位置保留；真正发送时才交付输入。首个字符、Unicode 输入和粘贴继续正常编辑。同一等待轮次的重复等待不会反复盖住已唤醒的画面。输入屏障覆盖完整鼠标手势，并处理等待关闭与重画之间的顺序，避免点击穿透；Copy 模式在鼠标抬起后恢复原捕获。

新增 6 项休眠回归及 7 项定向终端检查通过；真实已认证 CLI/PTY 完整套件为 127 项检查通过，覆盖唤醒、发送、取消、超时和重连。120 列、48 列的实际 PTY 截图经过人工核验，中文草稿、缩进、等待与队列提示无重叠或越界。Frame 测试渲染图与真实 PTY 截图在证据中分别标记。[报告与截图](../user-wait-sleep-20261120/README.md)

### 启动即撤权的竞态修复

新版账号夹具保留了“WebSocket 刚准备好就收到 4001 撤权”的完整场景，并在旧冻结 CLI 上发现真实失败。未登录设备虽然采用 Account transport，但没有保存 credential；父进程原来用是否保存 relay 配置决定是否在子进程退出后读取最后的拒绝消息，可能先观察到正常退出，从而漏读 AUTH_EXPIRED 并错误重连。

修复 `e7f5711a3474275b9d5308a809c726636366d78a` 将三个分支条件改为实际 transport 类型，复用原有有界尾部读取及永久失败处理。独立源码复核通过，local transport 行为未变。旧 CLI 的 46 项诊断结果明确排除了该失败场景；它不是完整账号通过结果。修复后使用新构建 CLI 完整执行 47 项，全部通过，耗时 10.139 秒；执行前后 CLI SHA-256 均为 `e8f80ea13c0a2ba156e4a6c0628904859fe9c2228933bca224758822b3c910ec`。[完整账号验收](../account-relay-current-20261120/README.md)

账号套件使用真实 CLI、WebSocket 与 native shell，OAuth 和中继由自有 loopback 服务提供。它验证登录存储、身份、访问名单、账号归属、401/403/4001、正常断线重连、撤权停止重连、Ctrl-C 和注销；不将合成 OAuth 服务表述为真实 GitHub 登录验收。

## 验收来源与覆盖范围

| 验证批次 | 固定来源 | 已完成结果 |
| --- | --- | --- |
| listener 与左右滚动 | 产品 `70aebd0`；测试收尾修正 `9989076` | 上述 Rust、PTY、传输及 Chromium 套件通过 |
| zuse、ztuio、休眠初始组合 | 产品 `b49da12` | 格式、严格 all-target Clippy、三类 bin 构建、73 项 Python 发布检查通过 |
| 原生插件 | 冻结 b49 CLI/libtest 和真实 cdylib | 28 项通过，包含显式 ignored 的真实动态库合约 |
| zuse 电脑操作 | 冻结 b49 CLI/daemon | 36 项普通电脑测试、2 项显式私有桌面测试、1 项嵌入入口测试，以及严格 worker/34 项私有桌面 MCP 检查通过 |
| 休眠界面 | 冻结 b49 CLI 与对应测试来源 | 6 项新增、7 项定向测试和 127 项真实 PTY 检查通过 |
| 默认库完整回归 | 测试夹具来源 `fab35e1` | 1498 通过、0 失败、18 ignored；test-threads=2 |
| 当前 MCP 目录端到端回归 | 测试来源 `4aa018b`，冻结 b49 产品 | base 125、agents 475、unified 105、daemon 182 项检查通过 |
| 启动撤权修复后的组合 | 产品 `78442aa` | 格式、严格 Clippy、三类 bin 与 libtest 构建通过；1498 通过、0 失败、18 ignored；47 项账号回归通过 |

这些批次不可直接相加作为互不重复的测试总数。默认库测试的 18 项 ignored 并非全部执行；有外部依赖的私有桌面、嵌入入口和真实 cdylib 在独立批次显式执行。初次高并行库测试的失败分开处理：资源时序问题经过逐项串行复核，六处稳定失败则修正已过时的账号、图片默认折叠与能力探测夹具。随后完整 1516 项中的 1498 项通过、18 项保留 ignored。

默认库通过的来源 `fab35e1` 相对 b49 只修改测试夹具；已有源码等价证明不能用于掩盖后续真实产品修复 `e7f5711`。因此启动撤权变更重新构建，并使用新 CLI 执行完整账号回归和完整默认库测试。最终库运行耗时 123.255 秒，实际子进程与封装器退出码均为 0，冻结产物摘要保持。首轮产品也通过，但证据解析器错误计入 fixture 子进程摘要；修正解析器后重跑取得最终回执，记录错误没有被隐藏。[最终构建清单](../zuse-integration-20261120/final-frozen-build.json) · [最终库回执](../zuse-integration-20261120/final-library-gate.json)此前未变动的桌面与绘制路径保留各自明确的原始产物来源。[库门禁](../zuse-integration-20261120/library-gate.json) · [早期源码等价证明](../zuse-integration-20261120/product-source-equivalence.json) · [插件验收](../native-plugin-validation-20261120/README.zh-CN.md) · [MCP 目录回归](../release-e2e-catalog-20261120/README.md)

本机实际运行覆盖 Linux；Windows/macOS 的运行结论需来自正式 Release 对应原生 runner。Cargo 仍有既有 qrcodegen 未使用依赖提示，严格 Clippy 成功不被写成零警告。日常验证遵循项目本地检查约定，没有将普通 CI 或自动 review 作为迭代等待条件；正式发布工作流单独执行。

## 固定依赖与后续组合

| v0.10.14 中的库 | 精确版本 | Git 来源 |
| --- | --- | --- |
| ztuio | 0.1.0 | `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e` |
| zuse-computer | 0.1.0，Rust 库名 zuse | `c0dbfda315f8cf1cd03ddb0a3db15f3dcab22d41` |
| zharness | 0.5.0 | `84e370450f1d047d43119bf81caf73f742ba6b0a` |
| zplugin | 0.3.0 | `3e758b5c21af1bad1cfaf10264b43fe15440e69c` |

zuse gitlink 与 registry 0.1.0 标签提交不同，但本次链接的 Desktop 库和平台实现已逐文件核对为相同；上游额外 CLI/Android 内容不被当成已接入的 SSMCP 能力。ztuio 0.1.0 的下载包摘要也已核对。[固定构建与来源](../zuse-integration-20261120/frozen-build.json) · [Desktop registry 等价核验](../zuse-integration-20261120/registry-desktop-compatibility.json) · [依赖规则](../../dependencies.md)

后续组合由 Project Z 的 `coord/shared-cohort-20261120` 负责：zharness 0.5.1、zplugin/macros 0.3.1、zuse-computer 0.2.0、ztuio 0.1.0，共享 zsys 0.1.1。已推送的部分来源 `9c13277d1eacb68c2378f692a4d4660cb92f0c62` 和证据 `aecb7962a817a7b8ac049d586609e66cf3ca4cc6` 只完成 zharness/zplugin 配对与锁图，仍含旧 zuse。该部分的 registry/tag/图校验、73 项发布脚本检查和 75 项 zplugin native/Wasm 测试通过；中止排队的 zharness 测试没有结果，新的 SSMCP 应用套件也尚未运行。它不属于本版最终依赖，不被称为完整四库传播闭环。

聚合者已交付产品来源 `829abbf333622c6f260d51003f5f3bc78659e6e5` 与证据提交 `70d847ee3b529f16292297b20b80b2d06a43a9d5`：四库 pin、registry 包摘要、单一锁图、73 项发布检查和最小 Linux CLI 构建通过，CLI 尚未执行。[聚合验证](https://github.com/LIghtJUNction/ssmcp/blob/70d847ee3b529f16292297b20b80b2d06a43a9d5/docs/reports/shared-cohort-20261120/validation.json)

SSMCP 源码审查已发现并接手两项 Wayland 消费者问题：输入前状态拒绝被误归类为清理不确定；逐字符调用使后续非法控制字符之前的文本前缀可能已输入。独立应用分支已推送修复与回归来源 `2b81e41c09a55f70011279385405c18280ff824e`，包含已冻结应用基线与聚合来源；格式检查通过，尚未编译或执行新回归，不计作实际运行通过。SSMCP 维护者继续负责完整应用、原生插件、PTY 和桌面验收，并明确下一次应用发布边界。oh-my-ztty 的接手方仍等待包含 zuse 0.2.0 的正式 SSMCP 以及 Linux x86_64/ARM64 归档摘要。库发布或发送通知不足以关闭该依赖链。

## 发布与保留事项

正式 tag 必须引用已进入 main 的精确提交，并验证版本、锁文件及所有递归子模块。正式工作流执行六个平台构建、安装与更新检查、完整集成检查、源码打包、crate 校验发布、GitHub 归档与服务器部署。创建 tag 或启动工作流不等同于全部成功；发布后还要确认实际服务和开发恢复连接。

更完整的跨会话生命周期与内存优化仍保留为独立迭代事项，不以本轮 listener 或休眠检查冒充完成。公开报告保留源码、摘要、断言与布局证据，私有会话正文、凭据和运行环境细节不进入公开仓库。
