# SSMCP 当前迭代：核心功能与门户已合入，正在完成发布验收

**更新于 2026-11-22 00:27:35 UTC。** 已完成下列本机验证的来源为 [508888bc](https://github.com/LIghtJUNction/ssmcp/commit/508888bc11bf6ca5ce90316361effed38a3160f8)。该来源已通过格式检查、全部 target 的严格 Clippy、**2,079 项常规库测试、另行运行的 2 项必需隔离检查，以及 9 项 two-tools 检查**，并构建了客户端、前端、服务端和终端快照解码器。另有 11 项 ignored 测试未计作通过。后继 [eb319166](https://github.com/LIghtJUNction/ssmcp/commit/eb319166ec8d09deafe4a0da4e6bbe2e04b3705c) 仅调整三份验收脚本，V28 构建与后续场景验证正在推进。[结构化证据](unified-source-and-cli-matrix-public-facts.json)

三个对应修正已提交：Linux E2E 清单补齐四个文件管理入口；用户等待脚本严格核对主动断连后的授权失效回执；007 测试 relay 补全正式登记所需的握手信息。以下失败结果属于修正前的 508888bc，后继 eb319166 的对应验收继续运行。六平台 CI 的四个 Windows/macOS 作业已经通过，两个 Linux 作业因清单断言失败。12 组本机 CLI 的结果为 **9 组通过、3 组失败**，原失败回执保留。**v0.10.26 正式 Release、H2 发布核验及独立服务器升级尚未执行。**

## 当前验证结果

下表按实际运行的源码版本记录结果。后继源码与共用测试脚本确定后，执行侧将完成对应验收绑定。

| 验证范围 | 实际来源 | 已取得的结果 |
| --- | --- | --- |
| 格式、严格 Clippy、库编译 | 508888bc | 通过；V27 构建回执保存 |
| 库测试与必需隔离检查 | 508888bc | 2,079 项常规测试通过，另行执行 2 项必需隔离检查通过；11 项其他 ignored 排除 |
| two-tools | 508888bc | 9 项全部通过 |
| 四个候选二进制 | 508888bc | ssmcp、ssmcp-frontend、ssmcpd、vt_snapshot 构建通过 |
| 文件管理与清理 CLI | 508888bc | 真实 native write 与 9 次 CLI 调用，27 项检查通过；11 个验收自有进程回收 |
| OAuth 补充脚本 | 508888bc | 19 项动态客户端注册（DCR）检查通过 |
| 12 组原有 CLI 场景 | 508888bc | 9 组通过、3 组失败，详见下表 |
| worker 双池运行 | 664c264d | 202 项通过仅属于 664c264d；最终候选及共用脚本的对应绑定与验收尚待完成 |
| 门户视觉检查 | 664c264d | 首页、中文指南、支付表单在 1,440 px 与 390 px 宽度完成实际截图检查；页面来自匿名或合成导出 |

### 六平台 CI

[CI 44965404683，attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/44965404683) 的六个平台作业均已结束：

| 平台 | 结果 | 当前需要处理的内容 |
| --- | --- | --- |
| Linux x64 | 失败 | 第 23 步 E2E 工具清单断言 |
| Linux ARM64 | 失败 | 同一工具清单断言 |
| Windows x64 | 通过 | 无失败步骤 |
| Windows ARM64 | 通过 | 无失败步骤 |
| macOS ARM64 | 通过 | 无失败步骤 |
| macOS Intel | 通过 | 无失败步骤 |

两个 Linux 原始日志在前 38 条检查通过后，均于 `scripts/e2e.py:604` 的精确目录比较失败。实际目录多出 `ssmcp.files.cleanup`、`ssmcp.files.config`、`ssmcp.files.configure` 和 `ssmcp.files.list`；缺项为零，重复项为零，无效 schema 为零。eb319166 已将四个已实现入口加入期望集合，继续保留精确相等、数量和 schema 检查。该次 E2E 在此失败；后继来源的联合流程将在本机构建完成后继续验收。[固定源码](https://github.com/LIghtJUNction/ssmcp/blob/508888bc11bf6ca5ce90316361effed38a3160f8/scripts/e2e.py)

### 12 组本机 CLI

| 场景组 | 结果 | 已观察到的边界 |
| --- | --- | --- |
| account-interactive、agent-entry | 2 组通过 | 对应账户及 agent 入口场景完成 |
| local、local-socket、context-reload | 3 组通过 | 对应本地、socket 与上下文重载场景完成 |
| message-references、terminal-control | 2 组通过 | 对应消息引用与终端控制场景完成 |
| user-wait | 失败 | 172 项检查后，主动断连触发外层 unavailable；后继脚本已新增具体授权失效断言，等待重验；结束许可规则保持严格 |
| service-portable、session-service | 2 组通过 | 对应服务场景完成 |
| 007、007-upgrade | 2 组失败 | 正式登记超时；后继脚本已补全测试 relay 的握手，等待重验 |

全部子命令的要求继续按功能场景验收。帮助文字、解析结果和 wrapper 退出状态各自保留其检查范围。新增清理、工具仓库、worker 和 agent 面板的结果也按对应源码及脚本单独记录。

## 门户：首页、分层指南与支付页面

门户已分三批提交，并完整保留在当前验收来源中。

| 提交 | 交付内容 |
| --- | --- |
| [b14da9df](https://github.com/LIghtJUNction/ssmcp/commit/b14da9dfd07d62a63bc156edaaf838d716264f14) | 白底蓝色的克制首页，两个主要入口、紧凑连接说明、三步安装引导与明确的下一步提示 |
| [0eaae46c](https://github.com/LIghtJUNction/ssmcp/commit/0eaae46c66bd930ecec5fd7f3796b88eb6e44782) | 中英文分层指南、手机目录、文件与技能引用、异步协作、消息回执及故障处理；电脑操作说明改为独立 Zuse CLI |
| [9ae07362](https://github.com/LIghtJUNction/ssmcp/commit/9ae0736258fbc68f089ac415b454ba00baecd325) | 流量余额、真实金额、有效期、订单列表、收银台交接和支付状态的展示整理 |

[16789d36](https://github.com/LIghtJUNction/ssmcp/commit/16789d3687391f6e0facf4eac5df74e6c2d588ba) 在指南中继续补充 worker 池、可编辑工具仓库、CLI resume、结束批准，以及待回答问题与正在阻塞等待的区别。后续格式化保留了已有页面和付款条件。

付款页面直接显示服务端提供的价格、金额范围、期限和订单状态。提交字段、CSRF、订单所有权、通知验签、入账条件和收银台 no-referrer 链接均保留；页面返回或刷新不会授予额度。跨月已付款状态继续提示联系管理员处理退款，不显示已退款。视觉检查覆盖匿名或合成页面的布局；真实账户付款未执行。服务器升级后的在线页面核对由部署流程继续完成。

## worker：完整执行循环与实际资源限制

worker 已接入持久本地 supervisor、FIFO 领取、配置的本地执行器、完成回收与再次等待。专用面板显示空闲、工作、离线、总数及任务状态，不提供聊天输入框。未配置执行器时保持离线并明确拒绝任务。

资源预算按“当前可用系统容量 ÷ 已登记 worker 数 N × 系数 k”分配，默认 k=1。空闲槽也计入 N；新池加入时，先将现有池带到空闲检查点，降低并核验旧池限额，再接纳新槽。退出时确认自有进程树和 cgroup 已清空，再减少登记计数。N 下降后，既有池可以暂时保留更严格的限额，直到下一次准入重分配。

当前硬限制依赖 Linux 的 user-systemd 与 cgroup v2，共享计数覆盖同一系统账号、共享登记目录及相同系统管理命名空间。其他账号和隔离挂载空间各有自己的登记范围。Windows/macOS 当前没有等价的硬限额执行路径。无法建立所需隔离时保持离线；k 大于 1 会按用户配置放大预算。

在 664c264d 上，两个真实池已完成 202 项检查，覆盖资源限额、FIFO、幂等、取消、断连回收、终端恢复及同账号多池重分配。验收使用合成执行器。202 项通过仅属于 664c264d；最终候选及共用脚本的对应绑定与验收尚待完成。

## 文件登记、自定义清理与现存目录

文件管理已接入创建登记、list/cleanup/config/configure、对应 CLI、自定义规则、启动清理、每五分钟自动清理及 reload 触发。默认临时文件保留 24 小时，总量预算 512 MiB。新建普通写入登记为 user_file；temporary:true 限于 .ssmcp/tmp 下的新建文件。未知文件、用户文件、持久输入及仍持有活动租约的文件受到保留规则保护，未完成的扫描不会进入删除。

删除流程先把待处理对象原子移到同一父目录的私有隔离目录，再核对实际移入对象的身份，最后删除该隔离对象。身份不符时尝试不覆盖恢复；原名称已被新文件占用时保留双方，并通过 CleanupRecovery 报告恢复位置。这样，扫描后原路径被外部写入替换时，新文件仍得到保留。[修复提交](https://github.com/LIghtJUNction/ssmcp/commit/57edcec5d684ad35555009817a35cbd7ec9c88c8)

固定 508888bc 的独立窄复核确认了隔离、身份核验、不覆盖恢复、活动租约以及创建/清理锁顺序。对应替换与恢复冲突测试已写入源码。本机真实清理验收已在该来源完成 9 次 CLI 调用、27 项检查，并回收 11 个验收自有进程。长期活跃保护继续由持有者保留 Registration 或 FileLease；单次 record_created 完成登记后释放短期租约。

项目工作区的旧发布临时目录此前已归档并逐文件核验，然后移除。处理 2,023 项，普通文件内容 49,873,179 字节；47 个输入文件保留，工作区 .ssmcp 当时仅剩 inputs。归档继续保留，结果仅覆盖这次明确清点的目录。相关证据保留在结构化事实的历史部分。

## Agents、等待与可编辑工具仓库

Agents 面板使用稳定成员身份打开只读历史，鼠标操作对应实际绘制的成员或运行。同名成员、列表重排和延迟事件按该身份定位；历史浏览保留草稿、光标、收件人及当前会话。窄终端页脚优先显示返回和历史入口，既有 42 组尺寸断言保留。真实 PTY 场景与 agent 面板补充验收继续随最终候选完成。

say、ui.message 和 code-mode 消息入口支持严格布尔参数 finish_task:true。消息 ACK 后进入原有 ui.wait(intent:"finish")；结束许可要求当前账号、连接、成员及输入阶段一致，并由真实 Human 的 Yes 完成且消费有效决定。超时或未获批准时，结果明确要求 Agent 再次调用阻塞等待；No、取消和新输入不会授予结束许可。user-wait 后继脚本已补齐主动断连后具体错误的核对，结束批准条件保持不变。

本地等待显示取决于实际等待入口。原生 ui.wait 进入客户端的用户等待状态；协作收件和执行结果读取使用各自的等待通道。远端的等待提示还依赖双方已协商的显示能力。指南已经说明这些入口，以及“有待回答的问题”和“正在阻塞等待”的区别；本机具体可见状态继续由最终等待场景核验。

独立 [ssmcp-tools 仓库](https://github.com/LIghtJUNction/ssmcp-tools) 已建立，产品已接入 rc 加载、用户覆盖和独立更新检查。工具内容继续通过现有宿主的身份与权限边界执行。只读加载并发、实际客户端重载回执和工具目录同步已进入本轮源码与验收工作。

## 发布与服务器升级

当前工作按以下结果收束：

1. 收齐 eb319166 上 Linux 工具清单、user-wait 断连边界和 007 登记场景的验收终态，保留严格断言。
2. 将 worker、清理和 agent 面板验收绑定到最终产品候选及共用脚本，收齐最终六平台 CI。
3. 按既有 S/P 来源约定完成标签、Release、资产和 H2 核验；开发机使用正常同步保留用户已有修改。
4. 完成已授权的独立服务器升级，并核对新版首页、指南、登录及付款页面的实际服务状态。

发布工作流的服务器部署开关继续保持关闭；独立服务器升级已获授权，单独推进。正式发布来源 S、实际工作流来源 P 和 Release run 尚未绑定。本快照的 Release API 查询返回 404。历史 v0.10.25 发布失败记录及原始来源保留在既有报告中。

## 证据索引

当前事实文件按来源保存完成回执、CI 作业、功能范围和历史快照。V27 的构建、库测试、two-tools 与候选构建回执已取得精确大小和 SHA-256；OAuth 与清理补充结果也保存各自回执身份。完整运行日志由执行侧保留，日常同步使用终态摘要和指定回执。

- [当前候选 eb319166](https://github.com/LIghtJUNction/ssmcp/commit/eb319166ec8d09deafe4a0da4e6bbe2e04b3705c)
- [已验证来源 508888bc](https://github.com/LIghtJUNction/ssmcp/commit/508888bc11bf6ca5ce90316361effed38a3160f8)
- [本轮六平台 CI](https://github.com/LIghtJUNction/ssmcp/actions/runs/44965404683)
- [当前结构化事实](unified-source-and-cli-matrix-public-facts.json)
- [上一份阶段快照](https://github.com/LIghtJUNction/ssmcp/blob/dc91093c9a139160c50e94953509c60f810b48ec/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [报告与历史检查点](README.md)

