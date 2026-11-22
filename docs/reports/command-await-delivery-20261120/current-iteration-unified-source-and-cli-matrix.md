# SSMCP 发布前验收：1d6 本机与六平台 CI 均通过，正式发布待执行

**更新至 2026-11-22 02:11:34 UTC。** 当前候选 [1d6b5532](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc) 已完成 V31 构建、库与协议检查、原 12 组 CLI、账户核心、原 CI 的其余本机场景，以及最终 worker、Agents 面板和补充检查。

唯一的新六平台 [CI 44973215329](https://github.com/LIghtJUNction/ssmcp/actions/runs/44973215329) 已由报告协调者独立读取确认：**completed／success，6 个作业全部 success**。Google 进展文档也已更新到六平台通过的结果。正式 Release、H2 发布核验、主分支整合和独立服务器升级仍待实际执行回执。

## 候选身份与验证范围

- **源码 S：** `1d6b55320fff6533663dad2627891960b3b5c5cc`
- **源码树：** `fc28406bb20a3d1f6f9441ca34016357ab0ff39c`
- **父提交：** `083ef018e13cdf6672637d41ea1bd13542185d5a`
- **本机执行批次：** V31，使用该候选的实际构建与交接描述符。

12a 修正了 unified 的严格审计事件匹配并补充发布说明；083 只改 Questions、Interactions、relay-composer 三份夹具；1d6 仅恢复前两份脚本的 100755 可执行模式，文件内容零变化。源码树和 blob 的静态比较已闭合，六份发布配方、相关 Cargo 文件及发布说明沿这两个后继提交保持。[固定差异](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc)

此前的 12a 二进制配合候选脚本运行结果保留为历史。本次表格使用新的 1d6/V31 完成回执，不再依赖那组混合来源作为最终本机结论。

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

本轮实际构建了 release profile 客户端和服务端，并通过隔离暂存安装的原四项检查。安装目标是验收拥有的暂存区，随后完成自有资源清理。**这些操作没有创建公开 GitHub Release，也没有覆盖当前运行客户端或生产服务。**

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

付款通知核验后才入账，页面回跳不确认付款，跨月到账仍需管理员核对退款。现有客户端和生产服务器尚未安装候选，本机通过没有改变此前线上版本基线。

## 发布准备与下一步

[0.10.26 发布说明](https://github.com/LIghtJUNction/ssmcp/blob/1d6b55320fff6533663dad2627891960b3b5c5cc/docs/release-notes-0.10.26.md) 已补齐 worker、清理、工具仓库、finish_task、门户和支付体验。原 9,447 字节完整保留，新文件为 **13,772 字节**，SHA-256 为 `1a0b81c5c54c64d5ae6cc9bbab47f068d410663b07024705e9231de90378ec5f`。候选身份、发布说明及六份配方的静态来源核对已经闭合，既有 S/P 来源契约保持不变。

在本轮 CI 已通过的基础上，原源码协调者将从 S 准备文档后继 D，只集成最终验收矩阵、事实文件和简短索引；随后按原流程进行主分支整合、Release、H2 和已授权的独立服务器升级。上述提交、发布与部署状态均以各自实际回执为准，本稿记录的是现已完成的验收。

H2 当前只完成静态准备，没有 import 或执行；Release 工作流的服务器部署开关继续关闭。独立服务器升级单独推进，届时核对真实安装与服务状态。两篇 session 文档中的旧入口示例清理仍按发布后的原安排处理，保留后续新增内容。

## 回执身份与历史

执行侧已按各自回执中的路径、字节数和 SHA-256 实际读取 **26 份最终报告**，确认摘要一致；26 是报告份数，不是测试数量。以下列出本稿已取得完整 SHA-256 的关键回执；未取得完整摘要的条目不根据前缀补造。截图、子进程数和命令次数不再加进测试总数。

| 证据 | 字节数 | SHA-256 |
| --- | ---: | --- |
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

- [1d6 构建等待阶段报告（2bf653f4）](https://github.com/LIghtJUNction/ssmcp/blob/2bf653f413c59ffa4bc065089837545dedf66c2a/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [12a、eb319 及门户完整阶段报告（bba557f2）](https://github.com/LIghtJUNction/ssmcp/blob/bba557f2c710fe3b0e65883db29da8573e2e7014/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [本次六平台 CI](https://github.com/LIghtJUNction/ssmcp/actions/runs/44973215329)
- [结构化事实与回执身份](unified-source-and-cli-matrix-public-facts.json)
- [Google 进展文档](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit)
