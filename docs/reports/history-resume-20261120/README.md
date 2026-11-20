# ssmcp 历史恢复与继续迭代记录

日期：2026-11-20；面向用户的时间采用 Asia/Taipei。

## 恢复结论与本轮边界

本轮找回了本地会话、Git 提交、正式 Release、运行进程与验收记录的对应关系。恢复时的正式版本是 **v0.10.13**，旧文档中的 v0.10.12 Windows 发布失败已经修复；007 恢复服务当时仍运行 v0.7.4，本轮已完成专用副本的现场升级。此前未完整合入的线程监听和终端滚动分支也已经接回主线。

本轮 v0.10.14 已整合账号接入、线程监听、zuse 桌面工作进程、独立 ztuio、等待用户时的休眠界面，以及真实账号端到端检查发现的启动撤权竞态修复。较早完整组合 `78442aa0849cc1a168db039ff7427ea076594409` 已经通过并合入主线；发布前又发现旧客户端默认握手及私有 Xvfb 分配的实际问题，后续来源和验证分别记录，不把较早的通过结果套用到新代码。

**本文件当前是正式发布前的验收记录。** 旧客户端兼容、私有显示分配与 OpenCode 取消归档修复已通过 PR #106、#107 合入；当前产品主线为 `7fcccdde53d2b977172f0d1012fd641958435854`。最终固定产品 `6d9a522` 与仅调整测试的 `0a6615d` 已完成 **1512 通过、0 失败、21 ignored** 的完整库门禁，实际退出 0，耗时 145.905 秒；产物与源码前后未变。此前失败按原始批次保留。PR #108 已提供明确的发布后暂缓部署选项，本轮正式发布会先核对六平台资产与 crate，再单独安排生产切换：现有其他会话仍有运行中的受管作业，中继重启可能取消它们。当前尚未创建 v0.10.14 tag 或切换生产；本版不含下一轮交互功能或 0.2.1 依赖组合。

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

日常开发连接实际仍运行 v0.10.12。新版冻结 CLI 只继承其账号配置引用，执行只读 whoami 后由当前服务器确认该账号已注册。007 与开发连接使用相同账号存储引用；007 发送空 allow，当前 broker 对已认证的空名单仍允许账号自身。该检查没有读取凭据正文、重新登录、修改启动配置或重启客户端。**007 的空名单路径在源码上兼容，但普通客户端不能据此类推。** 现场 v0.10.12 与官方 v0.10.13 普通客户端会将凭据中的 canonical GitHub principal 写入 Hello.allow，relay 原样发送；新版严格名单在 metadata 校验阶段拒绝旧格式。后续实际盘点发现十四个普通 v0.10.12 会话，另有一个独立 v0.10.13 007，因此不直接重载这些工作会话。现已完成仅限已认证账号自身的旧默认握手归一化与真实旧 CLI 验收；现场 v0.10.12 的来源不被冒称为官方资产。这个十五客户端盘点是当时快照，不代表后续新增客户端的版本和空闲状态已经验过。服务器切换后的真实重新连接仍须单独验收。

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

### 旧普通客户端的已认证自身兼容

兼容处理位于收到 Hello 后、常规 metadata 校验前。只有有效设备注册已确认 GitHub 身份，并且名单恰有一个与该身份 canonical principal 逐字节相同的条目，才归一化为账号自身。授权仍由 provider 与稳定 subject 决定，不进行新名字查找，不增加访问权限。匿名连接、其他身份、重复项、混合列表及非 canonical 格式继续拒绝，原 metadata 校验与前后两次注册有效性复核保留。

这一旧格式分支不协商 session_access_v1，并在尝试访问名单修改时要求升级；独立的控制来源能力仍可协商。新客户端的名单流程保持原有验证。产品修改来源为 [90a16cfa](https://github.com/LIghtJUNction/ssmcp/commit/90a16cfa3b9e13d63e0c184b93772985cc1ecde9)，固定构建来源为 `f9acbfc0102fa77b2e44c95724a05d6e55ea74bf`。

固定 CLI SHA-256 为 `9b7fdb2c4f9c5b385047d34c880af7082b1ba785e55c6e06dc4ad1714fb66c69`。broker 57/57、真实旧 CLI 41/41、新账号 47/47 通过，包括官方 v0.10.13 和现场 v0.10.12 的默认/显式自身名单、实际 native shell、第二账号禁止列出与调用、稳定 client ID 与不发生重连循环。[兼容报告](https://github.com/LIghtJUNction/ssmcp/blob/979973595a3172b6489c18f662fab4ef6e5b80f1/docs/reports/legacy-client-compat-20261120/README.md)

同一固定 libtest 首轮完整运行实际为 **1501 通过、1 失败、18 ignored，退出 101**。失败项是 OpenCode attach 生命周期回归中的五秒订阅关闭等待；同一 ELF 串行精确重放 1/1 通过，耗时 6.276 秒，没有放宽断言或修改超时。完整 gate 仍待最终组合重跑；首次失败保留。[首轮记录](https://github.com/LIghtJUNction/ssmcp/blob/979973595a3172b6489c18f662fab4ef6e5b80f1/docs/reports/legacy-account-self-20261120/library-first-attempt.json) · [精确重放](https://github.com/LIghtJUNction/ssmcp/blob/979973595a3172b6489c18f662fab4ef6e5b80f1/docs/reports/legacy-account-self-20261120/opencode-exact-replay.json)

### 私有 X11 显示分配

原私有桌面仅传入 `Xvfb -displayfd`，可能从显示号 0 开始尝试。修复改为显式高位显示号、同 UID 稳定文件锁与私有 cookie，排除宿主 DISPLAY、现有文件/抽象套接字及保留项。启动成功必须同时满足：ready 输出等于所请求号码、套接字确实属于本次 Xvfb 子进程、进程和父子关系仍有效、记录的 inode 没有被替换。清理只操作自己确认拥有的套接字；取消期间保留分配锁，直到所属进程树回收。固定环境清除继承的 Wayland 连接描述符。

源码复核还确认 X server 的部分网络监听行为与编译配置有关，因此显式加入 `-nopn`，使任一所需监听冲突均不能当作成功。已核对本机 Xvfb 21.1.26 对应的官方源代码；显式显示号分支不会改为从零搜索。宿主桌面此前的现场恢复由其他会话处理，本修复不以改动宿主套接字、cookie 或终端默认配置作为手段。

组合源码 `796e7cefc4348fef09f0f1dbba094c75375e9298` 首次编译报 E0509，尚未到达测试阶段；类型修正 `f9dc07fd91a970e826da2b0961b91ca90d2c55f5` 和共享安全夹具随后进入冻结来源 `3298bfb98497e5d47661d94bcfec2ea39f006dbb`。该来源格式、严格 all-target Clippy、libtest、worker 入口及三类 bin 构建通过。Computer 单元 41/41（另 8 ignored）、4 个显式私有桌面/分配器回归、嵌入入口、MCP 34 项、CLI worker 27 项与 daemon worker 27 项通过。首次 MCP 调用缺少 fixture 环境变量，在启动桌面前失败；补齐环境后通过，原失败记录保留。

同一固定产物的旧 CLI 41/41 和新账号 47/47 再次通过，前后摘要保持。完整库结果为 **1506 通过、1 失败、21 ignored，退出 101，耗时 126.641 秒**；失败仍是 OpenCode attach 生命周期等待订阅关闭，因此没有将这个来源视为完整通过。[3298 账号证据](https://github.com/LIghtJUNction/ssmcp/tree/6e0931796da3ad7092a41722115d0f094dd15b4e/docs/reports/legacy-client-compat-20261120/allocator-3298) · [修复与发布 PR #107](https://github.com/LIghtJUNction/ssmcp/pull/107)

### OpenCode 取消时的部分输出归档

同一冻结产物的受限系统调用跟踪记录到：取消订阅后的归档清理在文件锁上阻塞约 9.098 秒，而 HTTP 连接尚未关闭。原实现先在异步任务析构中同步保存部分输出，然后才释放网络响应。跟踪本身在预设上限终止，只用于解释阻塞位置，不计为测试通过。

修复把归档状态移交给独立的阻塞工作，网络响应留在原任务中及时释放。正常 shutdown 先取消并等待订阅结束，再等待部分归档完成，最后关闭事件记录；shutdown future 被取消、对象在运行时外释放或排队中的阻塞工作随运行时结束时，仍有独立清理所有者。部分原始字节及未完成标记保留，没有通过丢弃输出或放宽五秒断言修复问题。

固定产品来源为 [6d9a522](https://github.com/LIghtJUNction/ssmcp/commit/6d9a522195a402ab05d1575150db18315b4e37c1)。新增回归主动占住私有输出锁，要求连接关闭和异步心跳先完成，释放锁后再核对部分归档的实际原始字节；另有运行时关闭时排队归档恰好完成一次的回归。修正审计错误投影的测试后，5 项新增回归与原生命周期回归全部通过；相关 OpenCode 用例也在完整库运行中通过。审计记录按既有策略隐藏任意错误正文，测试通过 owner-checked 输出 stat 核对实际未完成标记、错误与原始字节，没有改变审计策略。

随后完整库暴露了一项任务面板测试问题：它把“成功消费刷新”误写成“每次都必须触发重绘”。中间快照可能在同一时钟刻度内完全一致，因此不重绘符合既有行为和保留鼠标命中的测试。仅调整测试判据的最终来源 `0a6615d0c325dfb364c9005c01a4a63fefa5fad7` 保留首次加载、最终完成的重绘要求，以及中间加载结束、无错误、实际详情与 EOF 断言。最后完整库 **1512 通过、0 失败、21 ignored，退出 0，145.905 秒**，libtest SHA-256 为 `5cb460a9afef5f0b9ce2d04a6fcdf0a8ce0e4d76deb520705854351cc1512352`。

最终 CLI、daemon、frontend、worker 入口和 cdylib 均复用 6d9 的固定产物；两个后续文件差异均完全位于 cfg(test) 内，非测试前缀与 cleanup 实现逐字节一致。桌面与账号路径相对 3298 也有精确 blob 等价证明。因此每组原执行证据保持明确来源，不通过混合哈希推导未经执行的结果。[最终组件报告](../private-display-allocation-20261120/README.md) · [完整验收与等价记录](../private-display-allocation-20261120/validation.json) · [保留的失败与诊断](../private-display-allocation-20261120/prior-results.json)

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
| 旧账号自身握手兼容 | 产品 `f9acbfc`，证据头 `9799735` | 格式、严格 Clippy、构建；broker 57、旧 CLI 41、新账号 47 通过；完整库首轮 1501/1/18，精确重放 1/1 通过，全库 gate 尚未通过 |
| 私有显示分配组合 | `3298bfb` | 格式、严格 Clippy、构建、Computer 41、4 个显式桌面回归、嵌入入口、MCP 34、CLI/daemon worker 各 27、旧 CLI 41/新账号 47 通过；完整库 1506/1/21，OpenCode 收尾未过 |
| OpenCode 取消归档与最终库门禁 | 产品 `6d9a522`；测试 `0a6615d` | 严格构建通过；6 项 OpenCode 定点、任务面板定点通过；完整库 1512 通过、0 失败、21 ignored，退出 0，145.905 秒 |

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

后续组合由 Project Z 的 `coord/shared-cohort-20261120` 聚合：zharness 0.5.1、zplugin/macros 0.3.1、zuse-computer 0.2.1、ztuio 0.1.0，共享 zsys 0.1.1。较早候选使用 zuse 0.2.0，下面保留来源演进。已推送的部分来源 `9c13277d1eacb68c2378f692a4d4660cb92f0c62` 和证据 `aecb7962a817a7b8ac049d586609e66cf3ca4cc6` 只完成 zharness/zplugin 配对与锁图，仍含旧 zuse。该部分的 registry/tag/图校验、73 项发布脚本检查和 75 项 zplugin native/Wasm 测试通过；中止排队的 zharness 测试没有结果，新的 SSMCP 应用套件也尚未运行。它不属于本版最终依赖，不被称为完整四库传播闭环。

聚合者已交付产品来源 `829abbf333622c6f260d51003f5f3bc78659e6e5` 与证据提交 `70d847ee3b529f16292297b20b80b2d06a43a9d5`：四库 pin、registry 包摘要、单一锁图、73 项发布检查和最小 Linux CLI 构建通过，CLI 尚未执行。[聚合验证](https://github.com/LIghtJUNction/ssmcp/blob/70d847ee3b529f16292297b20b80b2d06a43a9d5/docs/reports/shared-cohort-20261120/validation.json)

随后已独立核对聚合提交 [ecc450f1](https://github.com/LIghtJUNction/ssmcp/commit/ecc450f17012f8120d7d49470ce9599ada65456f)：父来源为 `70d847e`，三项产品修改将精确依赖、lock 中的版本与 gitlink 升到 zuse-computer 0.2.1 / `d930769005d307c42639e8abedc81ab039427e40`；另有独立 JSON 证据。依赖集合未变化，locked/offline/no-deps metadata 与发布脚本 73 项检查通过，尚无该组合的 SSMCP 应用编译或运行结果。上游 [v0.2.1 Release](https://github.com/LIghtJUNction/zuse/releases/tag/v0.2.1) 已发布，[正式工作流 44648994167](https://github.com/LIghtJUNction/zuse/actions/runs/44648994167) 已独立查询为 completed/success；不能据此声称 SSMCP 已采用并发布。

SSMCP 源码审查已发现并接手两项 Wayland 消费者问题：输入前状态拒绝被误归类为清理不确定；逐字符调用使后续非法控制字符之前的文本前缀可能已输入。独立应用分支已推送修复与回归来源 `2b81e41c09a55f70011279385405c18280ff824e`，包含已冻结应用基线与聚合来源；格式检查通过，尚未编译或执行新回归，不计作实际运行通过。SSMCP 维护者继续负责完整应用、原生插件、PTY 和桌面验收，并明确下一次应用发布边界。下一应用候选将合入 0.2.1 聚合来源与最终兼容/私有显示修复，先交付应用来源和定向运行结果。oh-my-ztty 等消费者仍需正式 SSMCP 以及 Linux x86_64/ARM64 归档摘要。库发布或发送通知不足以关闭该依赖链。



Project Z 已交付后续整合来源 [ec718237](https://github.com/LIghtJUNction/ssmcp/commit/ec718237892354cd7d45f8fdf181fd2b8981c7a5)，由 78442 应用基线、四库聚合、Wayland 修正和私有显示修复组成，并补齐 X11 整串文本调用与输入前拒绝分类。单字符拆分会破坏上游对整个字符串的预检，因此 X11 文字以整串调用，取消若发生在派发之后会明确说明文字可能已改变、不要重试。这个来源尚未包含随后 v0.10.14 的旧账号与 OpenCode 修复；下一版将从完整已合并主线接入它和上述交互组合，先统一版本与冻结，再执行应用验收。不能把上游发布或仅源码整合记为 SSMCP 运行通过。

## 下一轮交互功能：源码检查点，尚未交付可用版本

这组要求在独立分支实现，当前 v0.10.14 冻结范围不包含它们：

- 结束申请必须有理由，默认 No，可写评论，使用有界等待。同一经验证线程不能自己批准 Yes；用户和其他已授权、可验证的参与者可以作决定。审批绑定当前输入修订与连接，新的输入会使旧批准失效。它约束 SSMCP 显式结束入口，不能拦截外部聊天产品的最终回复。
- 异步问题立即返回问题 ID；Shift+Up 跳转待答问题。用户答案仅投递到原提问账户和经验证线程的下一个合格工具响应。显示名称不能充当身份；没有线程绑定时明确降为原账户手动读取。
- 原生消息从实际在线描述符选择稳定收件人，正文始终按纯文本处理。显示名或文件读取失败不会自动变成收件人；同名对象必须明确选择。broker 回执分别记录 queued、included、acknowledged 或 expired；included 只证明进入响应，不证明阅读或执行。agent 来源独立记录，不能伪装为 USER REQUIREMENTS。
- 长粘贴超过 4 KiB 或至少 8 行时安全落到 .ssmcp/inputs，显示路径和字节数；短文本仍内联。写入失败保留原文供恢复，选择路径不自动发送。

完整交互组合已固定到 [ded54a7d](https://github.com/LIghtJUNction/ssmcp/commit/ded54a7dd493bf0bc91d6252bc97cabf9d8239ff)，产品源码基线为 `4a7907c523e6391b710635918355ef382cd62563`。其中核心 `c20eae3a7cbcb203810e2a8b5b66c62437610588` 为原生消息增加来源与目标的真实连接绑定；发送方单独查询即可回收失效回执，成功离开、撤权与现有心跳上的成员到期检查也会释放未完成正文，不依赖收件方再次轮询。已确认回执保留，容量按实际序列化字节计算。收件人选择器在没有外发消息时也会刷新，内容未变的轮询保持安静。

这一组合目前完成源码整合、格式及 Python 语法检查，新增 Rust/真实 WS/PTY 均尚未执行。真实 PTY runner 已覆盖 120/48 列、默认 No、显式 Yes、Shift+Up、账号与经验证 agent 的精确收件、回执和粘贴恢复；既有展示/上下文场景按实际意图区分 progress 与需批准的 answer。测试使用正常 loopback OAuth 与合成上游身份，不冒称真实 GitHub 登录。[组合验收入口](https://github.com/LIghtJUNction/ssmcp/blob/ded54a7dd493bf0bc91d6252bc97cabf9d8239ff/docs/reports/next-interactions-20261120/README.md)列出具体执行命令和固定解码器。原生消息 mailbox 受 live broker/connection 生命周期约束，不承诺服务器重启后持久保存。

## 发布与保留事项

正式 tag 必须引用已进入 main 的精确提交，并验证版本、锁文件及所有递归子模块。正式工作流执行六个平台构建、安装与更新检查、完整集成检查、源码打包、crate 校验发布及 GitHub 归档。服务器部署默认仍紧随发布；需要保护实际在运行的受管作业时，main 的显式 workflow_dispatch 可以设置 `deploy_server=false`，随后使用既有 Deploy server 流程及该正式 tag/manifest commit 单独切换。

本轮核查确认：服务器停止会断开客户端，而现有客户端收尾会取消受管原生命令；自动重连并不等于作业保活。因此当前生产切换等待相关工作负载维护者的正常完成或检查点交接，没有停止或重载其他客户端来清空门禁。部署选项通过 PR #108 合入，81 项发布/策略检查与 20 项部署保障检查在本地通过；当前工作流独立解析布尔策略，旧 tag 重试不依赖旧源码提供新字段，原 tag、main、lock、子模块及资产校验均保留。[发布与部署分步说明](../release-deferred-deploy-20261120/README.md)

创建 tag、发布资产和部署成功是分别核验的状态。推送 tag 后须确认没有另一条自动发布工作流排队切服，再显式启动本轮暂缓部署的完整 Release 工作流；同 tag concurrency 仅串行，不会覆盖另一运行的部署选择。后续生产切换保留既有 staging、摘要、健康与自动回滚检查，最终单独核对部署回执和连接恢复。

更完整的跨会话生命周期与内存优化仍保留为独立迭代事项，不以本轮 listener 或休眠检查冒充完成。公开报告保留源码、摘要、断言与布局证据，私有会话正文、凭据和运行环境细节不进入公开仓库。
