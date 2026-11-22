# SSMCP 0.10.26 已正式发布，H2 核验通过，服务器升级完成

**报告快照：2026-11-22 03:09:32 UTC。**

**结果更新至 2026-11-22 02:56:39 UTC 的升级后检查。** [v0.10.26 正式 Release](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.26) 已于 **02:39:42 UTC** 发布，`draft=false`、`prerelease=false`，16 个资产均处于 uploaded 状态。唯一正式 [Release 工作流 44975626721](https://github.com/LIghtJUNction/ssmcp/actions/runs/44975626721) 已完成：**19 个作业中 17 个成功、2 个按计划跳过、0 个失败**。

标签仍指向已完成 V31 本机验收和六平台 CI 的产品源码 S1d6。验收报告的三文件文档后继已经通过 [PR 180](https://github.com/LIghtJUNction/ssmcp/pull/180) 正常合入主分支。**H2 发布核验已实际通过，独立服务器已升级到 0.10.26；现有客户端继续保持 0.10.25。** 升级后服务 active、运行摘要匹配，已有凭据的身份查询成功。两次 H2 失败记录和中继重连过程均保留。

## 已验收的产品源码

- **源码 S：** `1d6b55320fff6533663dad2627891960b3b5c5cc`
- **源码树：** `fc28406bb20a3d1f6f9441ca34016357ab0ff39c`
- **父提交：** `083ef018e13cdf6672637d41ea1bd13542185d5a`
- **本机执行批次：** V31，使用该候选的实际构建与交接描述符。

12a 修正了 unified 的严格审计事件匹配并补充发布说明；083 只改 Questions、Interactions、relay-composer 三份夹具；1d6 仅恢复前两份脚本的 100755 可执行模式，文件内容零变化。源码树和 blob 的静态比较已闭合，六份发布配方、相关 Cargo 文件及发布说明沿这两个后继提交保持。[固定差异](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc)

此前的 12a 二进制配合候选脚本运行结果保留为历史。本次表格使用新的 1d6/V31 完成回执，不再依赖那组混合来源作为最终本机结论。

## 主分支、标签与正式发布

此次有三个明确分工的提交：S 提供产品源码，D 在 S 上附加最终报告，P 是主分支合并后的工作流入口。

| 身份 | 精确提交 | 已确认的关系 |
| --- | --- | --- |
| 产品源码 S | `1d6b55320fff6533663dad2627891960b3b5c5cc` | V31 与六平台 CI 的实际验收来源；`v0.10.26` 标签指向它 |
| 文档后继 D | `107ffe9b7ccdc65d8184d848311306c0baa2e77a` | 唯一父提交为 S；只加入最终矩阵、事实与简短索引三份报告 |
| 主分支工作流来源 P | `c26b37fc3912cf1ed5e8d2a6143c5c857446f6ce` | PR 180 正常双父合并所得；源码树与 D 完全相同 |

D 的 tree 为 `07445acc7ba5faf80054223be5dfe6a6c768648d`。三份新增文档位于 `docs/reports/command-await-delivery-20261120/`：`README.md`、`current-iteration-unified-source-and-cli-matrix.md`、`unified-source-and-cli-matrix-public-facts.json`。其中矩阵与事实原样复用已发布报告 [16fd69f8](https://github.com/LIghtJUNction/ssmcp/commit/16fd69f842d0a16c614ccd2268adffbefb89a66b) 的 blob；简短索引链接完整历史。报告协调者已独立读回三份文档的精确身份，其他产品文件、gitlink 和文件模式保持 S 的值。[D 固定提交](https://github.com/LIghtJUNction/ssmcp/commit/107ffe9b7ccdc65d8184d848311306c0baa2e77a) · [主分支 P](https://github.com/LIghtJUNction/ssmcp/commit/c26b37fc3912cf1ed5e8d2a6143c5c857446f6ce)

### 正式 Release 的实际终态

| 项目 | 已确认结果 |
| --- | --- |
| 正式运行 | [44975626721](https://github.com/LIghtJUNction/ssmcp/actions/runs/44975626721)，attempt 1，completed／success |
| 触发来源 | `workflow_dispatch`，main，head 为 P `c26b37fc3912cf1ed5e8d2a6143c5c857446f6ce` |
| 标签 | `v0.10.26` → S `1d6b55320fff6533663dad2627891960b3b5c5cc` |
| 原始请求时间 | 02:20:23.470615–02:20:25.218848 UTC |
| 请求输入 | `build_only=false`、`deploy_server=false` |
| 工作流终态时间 | 02:39:45 UTC |
| 公开 Release | [v0.10.26](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.26)，ID 454147144，02:39:42 UTC 发布 |
| 资产 | 16 个，全部 uploaded |
| H2 发布核验 | 02:46:31–02:49:35 UTC 实际执行，退出 0；来源与执行绑定均 verified |
| 独立服务器升级 | 02:52:22–02:52:25 UTC 完成；02:56:39 UTC 升级后检查通过 |

报告协调者在 02:40:52 UTC 独立读取了运行、19 个作业、Release、资产目录和标签原始数据。17 个作业成功；`publish_crates` 与 `deploy_server` 是两个计划跳过项：本版 crates.io 发布继续延期，发布工作流没有执行服务器部署。其后的独立服务器升级按已有授权执行，另有完成回执；两条操作路径分别记录。

16 个资产包括六个平台的二进制归档及六份对应摘要、两个安装器、`release-manifest.json` 和 `SHA256SUMS`。GitHub 的实际发布目录与 uploaded 状态已独立读回；后续 H2 也已完成资产下载、清单、源码和执行来源的关联核验。

发布正文的实际 UTF-8 大小为 **14,534 字节**，SHA-256 为 `765fafd2e6eaef4958e48e6e9dc65077deca2530ab714f42fc4f2b88614f0aed`，与按标签源码 S 准备的预期正文完全一致。正文取源没有被主分支上的报告文档替换。

工作流从 P 读取定义，标签指向产品源码 S。原始请求捕获与完整执行输出均保留，唯一请求返回的 run ID 对应本次成功运行。H2 始终核验同一 S/P、同一 run 和同一原始派发绑定，没有重跑或再次派发 Release。

## 1d6/V31 本机结果

| 范围 | 实际完成结果 | 计数说明 |
| --- | --- | --- |
| 格式与严格 Clippy | 通过 | 对应本轮实际执行 |
| 库测试 | **2,079 项常规测试通过** | 11 项其他 ignored 未执行 |
| 必需系统隔离检查 | **2 项另行执行通过** | 与常规库测试分开记录 |
| wire | **9 项通过** | 本轮协议检查 |
| 候选构建 | 通过 | 客户端、前端、服务端、终端快照程序及库的描述符与实际摘要已交接 |
| 原有 CLI 矩阵 | **12 组全部通过** | 各组自有清理均确认，无清理错误 |
| 账户核心 E2E | 通过 | 独立于下列 unified 等场景 |
| unified 与原 unit 检查 | **105 项 unified 检查及 4 项原 unit 检查通过** | 原审计隔离断言及精确事件匹配保留 |
| dashboard | **80 项检查通过** | 使用最终候选 |
| 原客户端 UI | **10 个叶场景全部通过** | 包括最终 Questions、Interactions、relay-composer |
| daemon | **182 项检查通过** | 使用已有真实用户 bus，清理已确认、错误为空 |
| release profile 与暂存安装 | 真实构建通过，**4 项隔离暂存安装检查通过** | 自有清理通过，错误为空 |

库测试、独立隔离检查、CLI 分组、UI 叶场景和具体检查数具有不同层级，报告分别列示，不拼成一个范围不明的“总通过数”。

### CLI 和 UI 的实际覆盖

12 组原有 CLI 为 account-interactive、agent-entry、local、local-socket、context-reload、message-references、terminal-control、user-wait、service-portable、session-service、007 和 007-upgrade。本轮均取得完整终态，而非只检查帮助文字或参数解析。已给出确切计数的报告如下；另外三组的通过状态保留，不补造未列出的计数。

| 原 CLI 报告 | 检查数 |
| --- | ---: |
| local | 57 |
| message-references | 53 |
| user-wait | 189 |
| terminal-control | 52 |
| agent-entry | 9 |
| service-portable | 42 |
| session-service | 69 |
| 007 | 32 |
| 007-upgrade | 14 |

账户核心另有 **125 项检查通过**，单独列示，不改称“第 13 个原 CLI 套件”。

10 个 UI 叶场景均已在最终候选上完成，执行侧实际读回的报告计数为：

| UI 报告 | 检查数 |
| --- | ---: |
| client | 58 |
| canvas | 72 |
| composer | 87 |
| questions | 43 |
| history | 150 |
| presentations | 57 |
| context | 71 |
| paste | 145 |
| relay-composer | 46 |
| interactions | 172 |

Questions 保留真实视图、输入焦点、光标、精确 pending 数量及非法请求拒绝检查；Interactions 用显式绑定核对原 Human 答案的交付；relay-composer 复用链接时仍严格检查实际 Git 身份。

前一 daemon 尝试因私有运行目录看不到用户 bus，在执行断言前被阻塞。修正测试环境后使用现有用户管理器完成场景，最终 V31 也已通过；没有借助新建管理器来替换既有环境的验收对象。

### release profile 与安装检查的边界

本轮实际构建了 release profile 客户端和服务端，并通过隔离暂存安装的原四项检查。该验收阶段只安装到拥有的暂存区，随后完成自有资源清理；后来发生的正式发布和生产升级另有回执，现有运行客户端保持 0.10.25。

较早的 180 秒准备超时原因仍保留为未知。后续 1d6 的离线 fetch 实际等待全局编译锁约 171 秒后完成，随后格式、Clippy、库与构建均正常结束；编译锁不再是当前阻挡。

## 最终 worker、面板与补充检查

| 场景 | 1d6 实际结果 | 使用范围 |
| --- | --- | --- |
| worker 双池 | **202 项检查通过，7 份真实 unit 观察** | 合成执行器配合真实 systemd/cgroup 资源限制 |
| Agents 面板 | **55 项检查，5 组真实 PTY JSON／PNG 快照通过** | 草稿、选择、成员身份和导航场景 |
| OAuth 补充 | **19 项检查、7 次 CLI 调用通过** | 仅 `client_registrations_only`，即动态客户端注册 DCR |
| 文件管理与清理 | **27 项检查通过，11 个自有进程已回收** | 清理错误为空 |
| 补充组 | gap、007-prefix、update **3 组均通过** | 输入前后一致，聚合错误为空 |

worker 回执记录输入前后一致、自有子进程全部回收、没有强制回收、没有剩余对象或清理错误。7 份真实 unit 观察与 202 项检查分别记录；使用合成执行器验证队列、资源限制和进程生命周期，没有调用外部模型来冒充这些结果。

Agents 面板除自动场景外，验收方还实际查看了本轮宽、窄 PNG：没有布局重叠，草稿、选择状态及身份后缀可见，保存了独立视觉回执。面板清理通过，源码工作区保持干净。

**OAuth 补充只证明 DCR 范围。** 19 项检查和 7 次 CLI 调用没有走真实 access／refresh token 的完整 CLI 链，不能把这组结果写成全部 OAuth 流程覆盖。

三组补充的最终明细已取得：gap 为 **22 个命令项、64 项检查**；007-prefix 为 **9 项检查**，其 362 条记录包含状态轮询，不能当作 362 种命令；update 的 check 和 update 当次均返回 **kept_newer**，因为候选 0.10.26 高于当时公开的 0.10.20，未下载或替换客户端。工具仓库实际提交为 `d80054cf16f38b93a94fb2867fbfbfde78832066`，状态 `current`、`applied: false`。update 的 4 条调用记录还包含只读 curl 环境查询，各项范围分别保留。

## 六平台 CI：六项全部通过

[CI 44973215329，attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/44973215329) 于 **01:49:07 UTC** 派发，实际 run 的 head 为本次 1d6。该运行只派发一次，之后以读取状态跟进。

| 平台 | 当前已读状态 |
| --- | --- |
| Windows x64 | 通过 |
| Windows ARM64 | 通过 |
| macOS ARM64 | 通过 |
| macOS Intel | 通过 |
| Linux x64 | 通过；02:05:28 UTC 完成 |
| Linux ARM64 | 通过；02:06:45 UTC 完成 |

两个 Linux 作业已完成包括 unified、后台会话、watchdog 及后续构建在内的原流程。报告协调者独立读取了 run 和作业终态，确认 attempt 1、head 为 1d6、整体成功且六个作业均成功；这是该固定候选的 CI 实际结果。

## 本轮产品交付范围

### 消息、人工等待与可展开输出

消息继续按认证收件身份路由。普通消息中的缺失文件引用保留原文并给出警告；技能按有效来源解析，使用前读取完整内容。原生消息和组发保留字面正文，交付阶段与接收方实际处理结果分开。

`finish_task: true` 在提交消息后进入有界的真实用户结束等待。默认 No 尚未提交时仍是未决定；只有匹配且已消费的 Human Yes 和最终外层 `may_finish: true` 才允许该次结束。未获批准的返回要求下一次阻塞续等。红色提示与倒计时对应真实人工等待，普通消息 ACK 不授予结束许可。

`exec.read` 可展开当前返回页的 stdout、stderr 和元数据，使用 128 KiB 本地预览预算并标明省略。完整作业日志仍通过实际游标或归档继续读取，历史行不凭空补全。

### worker、普通 Git 工具仓库与文件清理

worker 用 FIFO 原子领取和显式本地执行器工作，运行取消后须确认拥有的进程清理，槽位才重新空闲。当前资源限制依赖 Linux 用户 systemd 与 cgroup v2，按同系统用户的已注册槽位计算上限；独立 Shell、其他用户和外部程序保持各自的操作系统权限。

预写 code_mode 助手存放在普通 [ssmcp-tools Git 仓库](https://github.com/LIghtJUNction/ssmcp-tools)，支持 RC 加载、用户覆盖和独立更新。助手组合可以修改，原宿主身份与授权边界继续生效，更新保留本地修改。

文件登记支持用途分类、自定义清理规则、启动／周期／reload 调度。默认临时保留时间为 24 小时、容量阈值为 512 MiB。普通用户文件、持久输入、未知文件及有效活动租约对象保留；清理先捕获再核对身份，冲突时无覆盖恢复。

桌面与浏览器执行继续交给独立 Zuse CLI；SSMCP 的 computer 工具保留版本查询、下载、安装和卸载四项管理能力。这里没有恢复已撤去的内置浏览器或桌面控制桥。

### 门户与付款体验

首页采用克制留白、少量主要入口和三步入门，中英文指南分层介绍连接、消息引用、worker、工具扩展和权限边界。流量与付款页面显示服务端提供的真实价格、金额范围、有效期和订单状态。

门户原有 1,440 px 与 390 px 视觉证据来自匿名或合成页面。本轮准备回执确认已检查的 508 门户代码与最终 1d6 相同，因而继续保留这组页面证据；本次新增的实际宽窄查看针对 Agents 面板。真实付款、生产登录和升级后的在线页面不在这组视觉结果内。

付款通知核验后才入账，页面回跳不确认付款，跨月到账仍需管理员核对退款。生产服务器现已完成本版升级；本次没有进行真实付款实验，原匿名／合成视觉证据与生产部署回执分别记录。

## H2 通过及独立服务器升级

[0.10.26 源码发布说明](https://github.com/LIghtJUNction/ssmcp/blob/1d6b55320fff6533663dad2627891960b3b5c5cc/docs/release-notes-0.10.26.md) 保留原 9,447 字节并补齐本轮功能，新文件为 **13,772 字节**，SHA-256 为 `1a0b81c5c54c64d5ae6cc9bbab47f068d410663b07024705e9231de90378ec5f`。该源文件与上文完整发布正文分别记录。来源及六份配方的静态核对已闭合，D 没有改动产品发布说明。

### H2 的失败记录、兼容修正与实际通过

第一次核验在读取成功的 macOS ARM 构建作业日志时退出 1。原异常未保留底层 stderr，因此仅凭该回执无法确定直接原因。后续只读诊断另外证明，当前 `gh` 默认会拒绝该日志中的终端控制字符；取得原始日志不需要删去或转换这些字符。

[兼容后继 ab08dac0](https://github.com/LIghtJUNction/ssmcp/commit/ab08dac0e18ced96445de8799839f83375174ddb) 只在 `verify_published_manual_main.py` 的 `api_bytes` 命令参数中增加 **28 字节**的 `"--allow-escape-sequences", `，文件从 48,801 字节变为 48,829 字节，stdout 仍以原始 PIPE 捕获。独立差分核对确认其他字节及断言不变，adapter、provenance 和已审配方的 blob 保持；原核验包也保留。该后继是单独的文档／核验器包，没有修改产品路径。

兼容后继的第一次完整执行又在早期 GET run 元数据处退出 1，原因仍未知。后来默认连接与 HTTP/1 的同一 GET 均成功，不能据此反推那次失败必然是网络或协议问题。最终执行仅在该核验进程设置 `GODEBUG=http2client=0`，继续使用同一个兼容包、S/P、Release run 和原始请求捕获。

**最终 H2 于 02:46:31–02:49:35 UTC 完成，退出 0。** 成功报告为 **399,189 字节**，其中 `provenance` 与 `execution_binding` 均为 `verified: true`。最终证明同时保留前两次失败原件的身份；19 个作业、16 个公开资产、源码与产物的既有核验条件没有放宽，也没有重新发布版本。

### 生产升级与既有凭据连续性

H2 通过后，唯一独立部署操作于 **02:52:22–02:52:25 UTC** 完成，部署来源仍为标签 S1d6／v0.10.26。运行服务的摘要与目标摘要一致：

`ad3af5835f8a14946e68c1f53887354c730900a7b0daee4be71c1c5a132ea564`

中继切换使旧连接失效；恢复连接后只读取同一部署操作的终态，没有再次执行部署。完成回执记录配置、原有 units、runners 和 proxy 保持，自有临时文件清理量为 **26,442,243 字节**。这是回执记录的清理字节数，不据此推算磁盘净释放空间。

部署回执的分层检查结果照原样保留：stage／direct 的 `proxy_pricing_policy_preserved` 和 `single_public_footer` 为 false，public 两项为 true。公开入口与阶段／直接入口的结果分别记录，不能概括成“所有布尔检查均为 true”。

| 对象 | 02:29:04 UTC 升级前 | 02:56:39 UTC 升级后 |
| --- | --- | --- |
| 生产服务 | `ssmcpd 0.10.20`，active | `ssmcpd 0.10.26`，active；运行摘要与目标匹配 |
| 已安装客户端 | `0.10.25` | 仍为 `0.10.25`，原摘要不变 |
| 已保存凭据查询 | `whoami` 退出 0，返回 JSON，stderr 为空 | 已有保存凭据查询成功，返回身份与保存凭据中的主体匹配 |
| 客户端及凭据操作 | 只读基线 | 没有登录、退出登录、客户端安装或客户端重启 |

升级后插件目录与原生调用恢复可用，协作登记重新建立。**本次升级存在连接中断，未宣称零中断；已有账户凭据无须重新登录。** 这份真实凭据连续性检查与前面的 DCR 夹具范围分开记录，没有把 DCR 结果扩写为完整 access／refresh CLI 链。

### 会话文档与最终报告收尾

两篇会话文档已在 P 之后单独修订为 [Q `03f667ab0ea03e2e3f5bc2dde65bc241b5121df9`](https://github.com/LIghtJUNction/ssmcp/commit/03f667ab0ea03e2e3f5bc2dde65bc241b5121df9)，tree 为 `2344a5e0dca144bc95cdddfc5c0e68f2359807f5`，唯一父提交为 P。该提交只改两份文档，15 行新增、14 行删除，更新五项公开工具以及账户登录、本机 attach、远端 attach 的说明；报告协调者已独立读回。产品标签 S 和已经完成的 Release 来源保持不变。

三份历史独立验收工作树已在逐一确认 HEAD、干净状态及没有进程以其为当前目录后正常移除，没有使用 force，操作均退出 0、路径已移除。用户主工作树的既有脏状态前后一致；验收回执、截图以及发布核验使用的源码工作区继续保留。

本稿记录的产品验收、正式发布、H2 和独立服务器升级均已有实际结果。最后的矩阵、事实与简短索引将以 Q 为基线统一整合；Google 文档已于 03:06:04 UTC 更新为发布、H2 和服务器升级完成，报告协调者独立读回并确认历史原文摘要保持一致。最终报告的固定链接与主分支整合由后续实际读回记录，本稿不预写自己的未来提交或合并状态。当前事实记录中残留的 ignored 旧说明随本次文档更新校正；历史检查点原文保持不变。本稿保持 **11 项其他 ignored 未执行** 的实际范围。

## 回执身份与历史

执行侧已按各自回执中的路径、字节数和 SHA-256 实际读取 **26 份最终报告**，确认摘要一致；26 是报告份数，不是测试数量。以下列出本稿已取得完整 SHA-256 的关键回执；未取得完整摘要的条目不根据前缀补造。截图、子进程数和命令次数不再加进测试总数。

| 证据 | 字节数 | SHA-256 |
| --- | ---: | --- |
| 正式 Release 终态独立只读快照 | 164,093 | `83ba2ded096b18f376267b4bf6af676abbd2c0504e2cd56d4c1e178f5dc2ec33` |
| H2 兼容后继核验器 | 48,829 | `ed11f433e549419c2f11b35fb6ca9358822caa8d17356b56df2713930c6ff1d0` |
| H2 成功报告 | 399,189 | `4da9ac1e13589786b76308e61f3a5c41b0a2b2a3a5452bc4a3f1cfa4c9a78a14` |
| H2 最终证明（保留两次失败身份） | 8,849 | `2cf9f7a7bb233e382bbe7daea913d7338042db73973857d8a1b24cb2997ecb97` |
| 独立服务器部署完成回执 | 8,327 | `18ed692a12427b83dc848214caae0f764235ab3aad1fe8cfd25f8b08a00654db` |
| 升级后既有凭据连续性 | 4,594 | `c1e84088d940b31cb98c438f63276faef70374a58ed5adbebc4f028631b631a9` |
| 02:56 升级后只读检查 | 2,406 | `6545ca91670c55c1a8502f7c622fbf4371e8ccbea27ec2dd87c26b3aef760d5f` |
| 02:29 升级前基线 | 818 | `4041a7608f9d306fae1113a757a71ce548b743bcbd97548747e56a1b8a006ad8` |
| 三份历史验收工作树清理 | 1,196 | `04f70c4b9079b65dc8882ac863809eb5dffe118883652b58d24d12f1a25f0d72` |
| 唯一 Release 请求原始捕获 | 2,759 | `5409dab9a284f590e178438c1c98817e994bfe138c587b46f3b10fc68e57fa24` |
| 唯一 Release 请求完整执行输出 | 9,478 | `46189b411083e86216297efa1d346701618160bcf5ecb85743f3727baa4f7197` |
| V31 构建与原 CLI／账户核心交接 | 20,265 | `7e206a8b3af1491b5e90c5aa4ba3031f2640328e32fbec5bd6e5940a4c55df22` |
| release profile 与暂存安装 | 16,968 | `42d2718e6f5473f8fa8460fd022edfbe4b4730cde6bb2fb4de2883c588a8d116` |
| 本次唯一 CI 派发 | 1,317 | `eb7cdb97095dd7ec6d8bb32ea416e65f00e9c710727f222d5b98d4c4339f780b` |
| 最终 worker 回执 | 8,120 | `8084359e3b532514434708b7452e7e8b3fea3c40348444f303c953b076f70d4b` |
| 最终 Agents 面板回执 | 13,134 | `c033bcb779b001bb2aece3dca651b1eae7cb5731bd7b550f478801bcf6b665d6` |
| 本轮宽窄面板视觉回执 | 2,679 | `5467ef4652bbc84aaa0c85395b98be66d04cbd64145ccfb83318c9a3885533e8` |
| OAuth DCR 补充报告 | 19,555 | `516ccf6c688c1a2fa6d5fa4a846fb7c9f5fbf8a74bcc1195b80e152c4a1fd06e` |
| 文件清理回执 | 20,925 | `56a644cf2b95e97d37d417ee75947745459d6698cbc1069c3517bb1a7817f6f0` |
| 三组补充聚合回执 | 11,230 | `b2981f7b1a95d0edc230cc64632067dcb6782772648352517835ab9049bc185c` |
| gap 补充报告 | 32,030 | `0d5887528c9baffbe6dda7299efe0577f6afc5d5e8173884f3c61099dadfd705` |
| 007-prefix 补充报告 | 436,157 | `39982193a497b60e9e651b83638e4adbe7f41d28a09c29cc7804dc854e89150f` |
| update 补充报告 | 11,597 | `94e59a567a06bf120bdbb600a8a276f7dd259def07af52acd01d08f7c9627e85` |

早期失败与原因定位继续保留，不在本次终态汇总中重述全部过程：

- [最终验收报告（16fd69f8）](https://github.com/LIghtJUNction/ssmcp/blob/16fd69f842d0a16c614ccd2268adffbefb89a66b/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [v0.10.26 正式 Release](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.26)
- [正式 Release 工作流](https://github.com/LIghtJUNction/ssmcp/actions/runs/44975626721)
- [1d6 构建等待阶段报告（2bf653f4）](https://github.com/LIghtJUNction/ssmcp/blob/2bf653f413c59ffa4bc065089837545dedf66c2a/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [12a、eb319 及门户完整阶段报告（bba557f2）](https://github.com/LIghtJUNction/ssmcp/blob/bba557f2c710fe3b0e65883db29da8573e2e7014/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [本次六平台 CI](https://github.com/LIghtJUNction/ssmcp/actions/runs/44973215329)
- [结构化事实与回执身份](unified-source-and-cli-matrix-public-facts.json)
- [Google 进展文档](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit)
