# SSMCP 当前迭代：核心验证通过，正在完成客户端与服务场景验收

**更新于 2026-11-22 01:08:23 UTC。** 固定来源 [12a84780](https://github.com/LIghtJUNction/ssmcp/commit/12a847803e952420e7b4534f11fd57ae64ee5c32) 已取得严格格式与 Clippy、库测试、候选构建、wire、账户核心、完整 unified 及 dashboard 的本机通过结果。审计夹具修正已在本机完整 unified 流程中通过；剩余原 UI 场景仍有两项失败，daemon 在环境前置条件处阻塞，源码组正在定位。

此前 [eb319166](https://github.com/LIghtJUNction/ssmcp/commit/eb319166ec8d09deafe4a0da4e6bbe2e04b3705c) 的 12 组 CLI、worker、Agents 面板及补充场景已经通过，**这些结果属于 eb319**。该来源的六平台 [CI 44967293207](https://github.com/LIghtJUNction/ssmcp/actions/runs/44967293207) 保持 **4 个作业通过、2 个 Linux 作业失败**；新一轮 CI 尚未启动。当前没有 v0.10.26 正式 Release、H2 发布核验或独立服务器升级结果。

## 来源与结果如何对应

| 来源 | 本次实际状态 | 适用范围 |
| --- | --- | --- |
| [664c264d](https://github.com/LIghtJUNction/ssmcp/commit/664c264db1266de0791fce4cc2b811cafdbff1f9) | 较早 worker 与门户视觉检查来源 | 保留历史结果；不代替后续候选的执行记录 |
| [508888bc](https://github.com/LIghtJUNction/ssmcp/commit/508888bc11bf6ca5ce90316361effed38a3160f8) | 前一版本完成本机构建与库测试，12 组 CLI 为 9 通过、3 失败 | 原失败与回执保留，不因后继通过而改写 |
| **eb319166** | V28 本机结果已取得；六平台 CI 为 4 通过、2 失败 | 12 组 CLI、worker、面板等场景结果仍按此来源保留 |
| **12a84780** | V29 核心构建、库及 unified/dashboard 已通过；剩余场景继续处理 | 新取得的结果见下表，不能扩展为全套通过 |

12a 的完整身份为 `12a847803e952420e7b4534f11fd57ae64ee5c32`，tree 为 `8bc83fd6d446e2352cc7352deb20ccff592b404d`，父提交为 eb319。此次仅修改 `scripts/e2e_unified.py` 与 `docs/release-notes-0.10.26.md`，没有修改 Rust 产品代码、依赖或六份发布配方。[固定提交差异](https://github.com/LIghtJUNction/ssmcp/commit/12a847803e952420e7b4534f11fd57ae64ee5c32)


## 按实际来源记录本机结果

| 验证范围 | 实际来源 | 已取得的结果及边界 |
| --- | --- | --- |
| 格式与严格 Clippy | **12a** | 通过 |
| 库测试与必需隔离检查 | **12a** | **2,079 项常规测试、另行执行的 2 项必需隔离检查通过**；11 项其他 ignored 未执行 |
| wire | **12a** | **9 项通过** |
| 候选构建 | **12a** | 构建回执已取得；不等于正式 Release |
| 账户核心、完整 unified、原有 unit 检查 | **12a** | 通过，unit 检查为 4 项；这是修正后的本机结果 |
| dashboard | **12a** | 通过 |
| 12 组原有 CLI | **eb319** | **12 组全部通过**，包括此前失败的 user-wait、007 和 007-upgrade |
| worker 双池 | **eb319** | **202 项检查通过，7 次真实 cgroup 观察**；使用合成执行器，系统限制走真实路径 |
| Agents 面板 | **eb319** | **55 项检查通过，5 组真实 PTY 快照**；快照不另计为测试 |
| 用户等待 | **eb319** | **189 项检查通过**；属于 12 组 CLI 中的细分结果，不重复相加 |
| OAuth 补充 | **eb319** | **19 项检查、7 次 CLI 调用通过**；不泛称覆盖所有 OAuth 流程 |
| 文件管理与清理 | **eb319** | **27 项检查、9 次 CLI 调用通过**，包括真实原生写入 |
| 其他补充场景 | **eb319** | **3 组通过**：gap、007-prefix、update；更新分支的实际含义见下文 |

eb319 此前同样取得 2,079 项常规库、2 项必需隔离和 9 项 two-tools 通过，四个候选程序已构建并完成执行侧身份检查。12a 的库、wire 和候选构建结果来自本轮 V29 实际执行。

### eb319 的 12 组 CLI 终态

| 场景组 | eb319 结果 |
| --- | --- |
| account-interactive、agent-entry | 2 组通过 |
| local、local-socket、context-reload | 3 组通过 |
| message-references、terminal-control | 2 组通过 |
| user-wait | 1 组通过 |
| service-portable、session-service | 2 组通过 |
| 007、007-upgrade | 2 组通过 |

这 12 组既有场景覆盖账户、本地通信、输入等待、服务及升级流程，各自覆盖范围见对应回执。

用户等待的 189 项中，执行侧已读回真实终端的活动休眠状态、休眠下仍保持醒目的红色等待提示、鼠标唤醒后原生等待仍待决、四个等待槽及第五个 busy、结束请求默认 No、期限到达仍保留草稿等检查。这证明的是候选程序中的该次 PTY 场景，不证明用户正在使用的旧客户端已经升级，也不把协作收件或普通执行结果等待误作 `ui.wait`。

## 六平台 CI：当前仍有两个 Linux 失败

[CI 44967293207，attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/44967293207) 绑定 eb319。六个平台作业均已结束：

| 平台 | 结果 | 实际失败位置 |
| --- | --- | --- |
| Linux x64 | 失败 | 第 27 步：统一执行、审计与连接隔离 |
| Linux ARM64 | 失败 | 同一步骤、同一审计断言 |
| Windows x64 | 通过 | 无失败步骤 |
| Windows ARM64 | 通过 | 无失败步骤 |
| macOS ARM64 | 通过 | 无失败步骤 |
| macOS Intel | 通过 | 无失败步骤 |

此前工具目录多出四个文件管理入口的失败，在 eb319 的账户核心与目录步骤中已通过。当前两个 Linux 失败发生在后续审计隔离检查。

### 审计失败的真实记录与最小修正

两个 Linux 产物已下载并读取。原场景在 74 项检查后，因旧断言要求相关审计记录全部为 `code_mode` 而失败。最初 CI 产物没有保存断言前的另一客户端记录，因此当时只能保留原因待核的边界。

随后在 eb319 实际二进制上进行了一次诊断：**仅提前保存原始记录，旧判定保持原样**。该诊断仍以 1 退出，并在同一断言失败；保存的 37 行为：

- **1 行完整的空启动清理事件**，属于该运行时自己的启动过程。
- **36 行 `code_mode` 事件**，对应该客户端实际启动的 **8 个 run**。

实际记录确认了旧断言遗漏的自有启动事件，为修正检查前提提供了直接依据。

12a 的修正只接受字段完整匹配的、自有运行时的空启动清理事件，并在断言前保存原始审计记录；原有客户端、来源及 run 隔离检查继续保留。它没有宽松接受任意 cleanup、未知事件或其他客户端记录。**12a 已取得完整 unified 流程及原有 4 项 unit 检查的本机通过回执。** 原失败、诊断记录、修正和修正后的本机回执均已保留。六平台 CI 仍需在后续稳定候选上完成。[修正源码](https://github.com/LIghtJUNction/ssmcp/blob/12a847803e952420e7b4534f11fd57ae64ee5c32/scripts/e2e_unified.py)

## 12a 剩余原场景：已通过项、失败与环境阻塞

原 `make test-client-ui` 下的 10 个叶场景已开始运行，目前已报告的结果如下：

| 场景 | 12a 实际结果 |
| --- | --- |
| canvas | 72 项通过 |
| composer | 87 项通过 |
| presentations | 57 项通过 |
| context | 71 项通过 |
| filepaste | 145 项通过 |
| questions | 第二次 Escape 的断言失败；实际快照已有普通 Edit、原草稿及新的 pending banner |
| relay-composer | 重复创建 git 符号链接时出现 `FileExistsError` |

其余叶场景的终态尚未收齐。源码组正在核对 questions 的断言与实际状态，以及 relay-composer 的符号链接准备过程。

另一个 daemon 场景在运行断言前被环境前置条件阻塞：私有 `XDG_RUNTIME_DIR` 隔离了原有用户 systemd bus，已有真实用户 bus 仍在。**该次执行了 0 项 daemon 断言**。源码组正在核对现有用户管理器与测试环境的关系；本快照没有该前置问题修正后的运行结果。

最终 worker、面板和 OAuth 等重复绑定尚未开始；本快照继续引用 eb319 的已完成结果。新的六平台 CI、main 整合、标签、H2 和部署均尚未进行。

## 门户：首页、双语指南与真实支付语义

门户的三批修改均已进入现有源码：简化首页及三步入门、中英文分层指南、流量与付款页面。

| 提交 | 交付内容 |
| --- | --- |
| [b14da9df](https://github.com/LIghtJUNction/ssmcp/commit/b14da9dfd07d62a63bc156edaaf838d716264f14) | 白底蓝色、克制留白、少量主要入口、连接说明和三步安装引导 |
| [0eaae46c](https://github.com/LIghtJUNction/ssmcp/commit/0eaae46c66bd930ecec5fd7f3796b88eb6e44782) | 中英文指南、手机目录、消息与引用、异步协作和故障处理 |
| [9ae07362](https://github.com/LIghtJUNction/ssmcp/commit/9ae0736258fbc68f089ac415b454ba00baecd325) | 真实金额与期限、订单列表、收银台交接和付款状态展示 |

[16789d36](https://github.com/LIghtJUNction/ssmcp/commit/16789d3687391f6e0facf4eac5df74e6c2d588ba) 又补充 worker、普通 Git 工具仓库、CLI resume，以及待答问题与阻塞等待的区别。

视觉检查实际覆盖 1,440 px 和 390 px 的首页、中文指南与支付表单，页面来自匿名或合成导出。截图来自 664 的验收，门户和付款代码在 664、508、eb319 之间字节一致；12a 的两文件差异也未涉及页面代码。此处复用既有截图与源码一致性证据，真实付款及部署后的在线页面另行核对。

付款页面使用服务端给出的价格、金额范围、有效期与订单状态。表单字段、CSRF、订单所有权、支付通知验签、入账条件和收银台 no-referrer 链接保持原有协议。页面回跳或刷新不授予额度；跨月已付款状态要求管理员核对退款，不显示为已退款。实际支付、在线登录和部署后页面仍以各自真实操作结果为准。

## worker、清理与工具扩展

### worker 的执行与资源范围

worker 已接入本地 supervisor、FIFO 原子领取、显式配置的执行器、完成回收和再次等待。面板显示空闲、工作、离线、总槽位及排队状态，没有聊天输入框。默认 `next-caller` 不启动模型；缺少本地执行器或所需硬限制时，槽位保持离线。

CPU 与 RAM 上限按“有效系统容量 ÷ 同一系统用户已登记槽位总数 × k”计算，默认 k=1，空闲槽也计入。它们是上限而非资源预留。新池加入时先降低并核验可调整的既有空闲 scope，再接纳新槽；不能安全调整时延后准入。已有忙任务不会被强行按新份额缩减。退出要确认自有进程树及 cgroup 清空，登记变化后既有池可能暂时保留更严格的限制。

当前执行路径依赖 Linux、用户 systemd 和 cgroup v2。共享计数限于同一系统账号、共享登记目录和相应系统管理范围；其他账号、独立命令及外部程序不在这个池的计数和控制范围内。资源限制控制已登记 worker 的进程资源使用。eb319 本轮 202 项检查和 7 次真实 cgroup 观察已取得通过回执；使用合成执行器，验证队列与进程生命周期。回执记录输入前后相同、清理通过、无强制回收及无剩余对象；这些结论限于本次拥有的执行资源。[Worker 文档](https://github.com/LIghtJUNction/ssmcp/blob/eb319166ec8d09deafe4a0da4e6bbe2e04b3705c/docs/workers.md)

### 文件登记与自定义清理

文件管理支持创建登记、查询、清理和配置，接入客户端启动、每五分钟及 reload 调度。默认临时文件保留时间为 24 小时、容量阈值为 512 MiB；普通写入登记为 `user_file`，显式 `temporary:true` 限于新建的 `.ssmcp/tmp/` 文件。用户文件、持久输入、未登记文件和有效活动租约对象保留；扫描不完整时停止数据清理。

删除前先把对象原子移入同父目录的私有恢复区，再核对实际捕获身份。身份不符时无覆盖恢复；原名称已被新文件占用时保留双方并报告恢复信息。既有 [身份修复](https://github.com/LIghtJUNction/ssmcp/commit/57edcec5d684ad35555009817a35cbd7ec9c88c8) 与静态复核保留，eb319 本轮另有 9 次 CLI、27 项检查的实际通过回执。长期使用仍需持有有效租约，单次创建登记不等于为后续任意 Shell 持续占用文件。

历史工作区清理只覆盖先前明确清点并归档核验的 2,023 项、49,873,179 字节普通文件内容；47 个输入文件当时保留。那次结果仍属历史维护操作，不是本轮自动清理数量，也没有为本报告再次清理现场。[文件管理说明](https://github.com/LIghtJUNction/ssmcp/blob/eb319166ec8d09deafe4a0da4e6bbe2e04b3705c/docs/managed-files.md)

### Agents、人工等待和普通 Git 工具仓库

Agents 面板按稳定成员身份打开只读历史，保持草稿、光标、收件人及当前会话；本轮 eb319 已取得 55 项检查和 5 组真实 PTY 快照。历史浏览、同名成员和列表重排按实际身份处理，不将展示名当作授权身份。

`finish_task: true` 将消息提交与原有 `ui.wait(intent: "finish")` 连接起来。消息 ACK 不表示阅读，也不是结束许可。只有原认证线程取得并消费匹配的真实 Human Yes，最终外层回执返回 `may_finish: true`，才允许这次结束。一次有界等待超时或未获批准时，返回明确要求调用者再次阻塞续等；普通文字、No、取消和新输入都不构成批准。红色倒计时来自真实人工等待状态，不套用于普通协作收件或后台执行等待。

[ssmcp-tools](https://github.com/LIghtJUNction/ssmcp-tools) 是可独立迭代的普通 Git 工具仓库，通过 RC 加载官方助手及用户覆盖，原宿主身份与授权检查继续保留。更新场景本轮实际返回 **kept_newer**：候选 0.10.26 高于当时公开的 0.10.20；工具仓库状态为 current，提交为 d80054c。此结果验证了该次保留较新版本的分支，**没有发生客户端二进制下载替换**。

## 发布说明已补齐，发布与服务器升级尚未执行

12a 已把 worker、文件登记与清理、RC/工具仓库、`finish_task`、门户和付款体验追加到 [0.10.26 发布说明](https://github.com/LIghtJUNction/ssmcp/blob/12a847803e952420e7b4534f11fd57ae64ee5c32/docs/release-notes-0.10.26.md)。原有 9,447 字节完整保留，新文件为 **13,772 字节**，SHA-256 为 `1a0b81c5c54c64d5ae6cc9bbab47f068d410663b07024705e9231de90378ec5f`；报告协调者已完成精确读回。文档提交不是 GitHub Release，也不声明全部验收通过。

既有 H2 准备包仍停留在静态适配与来源核对，没有 import 或执行。本次未改六份发布配方，但尚未绑定正式发布来源 S、工作流来源 P 和 Release run。先前 00:16:24 UTC 的 v0.10.26 Release 查询返回 404；本快照未重新查询。历史 [v0.10.25 失败运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/44934221685) 继续保留。

独立服务器升级已获授权，尚未执行；Release 工作流的服务器部署开关继续保持关闭，两者分开处理。升级前只读对照记录了既有已安装客户端 0.10.25，并使用已保存凭据完成一次 `whoami` 查询，没有新登录或安装变更。此前服务器基线为 0.10.20；这些只是升级前状态，不是新版服务结果。

执行侧继续处理 12a 剩余原 CI 场景。worker、Agents 面板及 OAuth 后续绑定遵循既有协调顺序，等待当前来源稳定；最终 Release、H2 核验和已授权服务器升级仍按原流程取得各自回执后报告。

## 历史与证据范围

508 的两条 Linux 工具目录失败和 12 组 CLI 中的 3 组失败，均保留在 [上一份已发布快照](https://github.com/LIghtJUNction/ssmcp/blob/79d5ec9854aeeeecf082128ce2004b8929ce0286/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)。eb319 已取得对应本机通过，但自己的六平台 CI 又在后续审计步骤失败；两次失败不是同一断言，也不互相覆盖。

下列为本稿使用的固定回执摘要身份。详细原始日志由执行侧保留，计数按各自报告的场景范围列示。

| 证据 | 字节数 | SHA-256 |
| --- | ---: | --- |
| 12a 候选构建回执 | 7,502 | `056ff43eefb7dd7133470cda62fd76054dd6362154c1e2340be0e4e97e141ed6` |
| 12a 完整 unified 与 unit 检查回执 | 32,276 | `c8f485c65b747053e1f2efd4df0e3701a5786f53ea3759ed03516db9da65ee3e` |
| 12a dashboard 回执 | 15,367 | `716c1bfdaee70524ad3f24451080f899a232f899779de87da0cd5b7f716f3407` |
| eb319 本机结果交接 | 11,704 | `5e8799b05030c976c33ba0442e98037cb2fa56af6d1634cde651554872dbf850` |
| worker 回执 | 8,138 | `4a7ecf44abe4e067c21b5a80f53d99f540b8affd49dc1addca1cad3df9864630` |
| Agents 面板回执 | 13,174 | `3f2ba6f327a4da925dc4efc7810a964807198c27997cb149d59f4f704a851eea` |
| 用户等待报告 | 15,148 | `273d975806c53b769c32bf24e8f229e631db32b03e5e3e57abbf14ccb2204fc2` |
| OAuth 补充报告 | 19,556 | `6cf113111d9410edc0add423771b5c1bba06f95917783d881da8654272b7c2f0` |
| 清理回执 | 20,929 | `76adb27b3ac346e0e086e00b2932eae7bf0a5c990b22737654405fc408f724fe` |
| 三组补充回执 | 11,140 | `f11caed2962b0e6362ef3333bda2e7d8b93dfaa2073c1f220d16354be78164ba` |
| 保留旧判定的审计诊断回执 | 36,472 | `e6f66321096c82557ce1da818b0b4950d3877c641b33486df43baf922c9d55ea` |
| 诊断保存的 37 行原始审计记录 | 13,740 | `feab0fcb29b316de1377e3337e3213c13879891a717beeb24a7274e94042e18b` |

- [结构化事实与回执身份](unified-source-and-cli-matrix-public-facts.json)
- [Google 进展文档](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit)
- [报告与历史检查点](README.md)
