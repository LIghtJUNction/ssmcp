# ssmcp 客户端整合与验收记录

日期：2026-11-18。状态：**本轮验收与本机交付完成**。整合分支：`fix/complete-client-20261118`，已合入 `main`。

本轮已整合图片输入与显示、活动区呈现、会话 attach、调用归组和终端回复提醒。固定源码通过 strict Clippy、完整 workspace 测试及三平台 Session service CI；同版候选的真实原生图片、文件粘贴、教程二维码、composer、attach 和私有桌面验证均已有通过记录。本机 `ssmcp` / `ssmcpd` 已安装为 0.9.1，并通过版本与 release 哈希核对；整合 PR 和三个待合并 PR 均已合并关闭。报告及交付摘要已保存至 Google Drive。以下记录本轮实现、验证范围和实际交付回执。[验证摘要][validation] · [E2E 摘要][e2e]

## 1. 固定版本与 PR 适配

本报告的源码锚点为 [`d4eed29657bda8ea5d2de2ac212df69c3fa51e21`](https://github.com/LIghtJUNction/ssmcp/commit/d4eed29657bda8ea5d2de2ac212df69c3fa51e21)。适配实现先落入整合分支，再以 `ours` 合并记录确认三个 PR head 的祖先关系；记录前后的 tree 均为 `16ad3c8529c69ad155bc7e200612d494a1df8529`。这证明记录祖先时没有再次改变代码，功能覆盖仍由下表和实际测试说明。本轮经 [#32](https://github.com/LIghtJUNction/ssmcp/pull/32) 合入 `main`，实际合并提交为 [`5ce6618325647beae46879fd6afcec2231ca5904`](https://github.com/LIghtJUNction/ssmcp/commit/5ce6618325647beae46879fd6afcec2231ca5904)。GitHub 已确认 #25、#26、#30、#32 均为 `closed` 且 `merged=true`。[祖先关系][ancestry]

| PR 与最终适配 head | 保留的有效增量 | 适配方式 |
| --- | --- | --- |
| [#25](https://github.com/LIghtJUNction/ssmcp/pull/25) · [`aa8b95d`](https://github.com/LIghtJUNction/ssmcp/commit/aa8b95d66655adfd4d97ac8191e2b935bcd57c44) | ztui、原生图片、PNG framing 与恢复、真实回复提醒、调用断线、认证警告归并、二维码及字体变化刷新 | 分项迁移到当前输入、EventView 和 attach 架构，保留本轮兼容性修复。 |
| [#26](https://github.com/LIghtJUNction/ssmcp/pull/26) · [`8253343`](https://github.com/LIghtJUNction/ssmcp/commit/8253343710373f6906d6b99423015553696fdaa1) | 紧凑工具呈现、caller 分组、UI helpers、按需说明 | 沿用 wire-v7、可编辑私有引用和已有历史；未采用 v8 内联图片队列及第二份消息库。 |
| [#30](https://github.com/LIghtJUNction/ssmcp/pull/30) · [`c275028`](https://github.com/LIghtJUNction/ssmcp/commit/c27502879a05c24e2e4495be45d9083548d2b32f) | 默认折叠、关联输出、真实状态、diff 展开、过滤和 raw 回退 | 扩展现有 EventView，保持原生图片与草稿；未引入独立 CompactLog。 |

源码提交、测试脚本和二进制分别记录：最终测试夹具提交为 [`ee05244`](https://github.com/LIghtJUNction/ssmcp/commit/ee05244)，不改变上述产品源码锚点，脚本哈希见各套件报告。[产物清单][artifacts]

| 候选产物 | SHA-256 |
| --- | --- |
| validated `ssmcp` | `c56a977e3a7786e3647c4b54bf3fd74ba751f77958cee10edc5600b831bd7517` |
| validated `ssmcpd` | `dbac9844a0ff09aeadac86ff72f4a435c82687787d7474b158fbfd83734fccfa` |
| release `ssmcp` | `59f29540db09e78dc21d49e17bedc279f7c0721bf5aba8939a7d1ef99d13bc13` |
| release `ssmcpd` | `99a237b1c1c93193c0168f240a7035a7a9f4d56da43dca8e5915b3020aec7a1e` |

## 2. 输入边界与会话行为

PNG 必须先按二进制处理，再决定哪些字节可以进入文本输入。新增 framing 依据 PNG 签名、chunk 长度和 IEND 识别图像边界，因此图像内的粘贴结束序列、Ctrl-C、Ctrl-]、F7 和 Kitty 控制字节不会成为终端操作。导入成功后生成私有文件和可编辑 `@reference`；失败、过期或异步结果迟到时保留当前草稿，不自动排队发送。[输入实现](../../src/client/input/paste.rs) · [附件导入](../../src/client/attachments.rs)

恢复以真实输入进展计时：图像粘贴连续 2 秒没有新输入，清理该次未完成的二进制状态。完整 PNG 缺少外层粘贴结束标记、截断签名和损坏 chunk 都需要退出等待。attach 外层缓存部分结束标记时，会把实际进展传给 Graphics 和 Keyboard，避免内外层在不同时间结束；普通 tick 不延长等待。超时后的输入可重新作为文本处理，未完成图像不会落成附件。[输入状态](../../src/client/input.rs) · [attach decoder](../../src/session/attach/input.rs)

attach 的附件目录绑定到已连接会话的工作目录，即使启动前台的目录不同，也写入会话自己的私有输入位置。为此补充的是附件导入所需的 workspace scope，没有顺带授予 harness、Context 或 Handoff 能力。只读 observer 不能输入命令或改变 writer 的 PTY 尺寸；writer 的 Ctrl-C 送到内层 PTY 中断前台任务，observer 的 Ctrl-C 则 detach。writer 中断后 attach 保持连接；detach 或退出时恢复物理终端模式，并保持服务 worker 的身份与存续。[attach 实现](../../src/session/attach.rs)

真实 attach 套件已验证分块 PNG 原字节、私有权限、不同启动目录、图片内控制序列、超时后恢复、writer 中断及 observer detach。raw/headless 路径继续遵循既有 UTF-8 文本传输契约，粘贴正文绕过快捷键重解析；本报告不将其描述为原生二进制附件导入接口。[attach 验证](../../scripts/e2e_attach_service.py) · [E2E 摘要][e2e]

## 3. 原生图片、二维码与活动区

原生图片由终端能力和像素 cell 查询共同确认后启用。裁剪换算同时考虑图像像素、终端单元格和当前视口，避免滚动时改变像素比例或覆盖输入框。真实 ztty 验证读回窗口像素，确认 128 × 64 单像素网格、显示与隐藏、滚动、resize、清空和 reload。字体改变即使未改变行列数，也要清除旧 placement 并重新查询；selection 暂停绘制时仍执行这项刷新。[图片实现](../../src/client/terminal/images/kitty.rs) · [真实像素验证](../../scripts/e2e_native_images.py)

二维码仅在原生能力已确认、cell 尺寸已知且完整码能放入侧栏时，才后台生成 PNG。每模块使用 8 × 8 方形像素，四周保留四模块白边，显示时不缩放或裁切；空间不足或能力不可用时显示完整 URL。每个实时 session 以 2 秒预算匿名探测同源 `/guide/`，成功使用教程，失败回退同源根路径，静态 preview 不联网。这是客户端选择入口的规则，不能据此断言公网教程已经上线。[终端图片说明](../terminal-images.md)

活动区继续使用一个 EventView。来源、请求和任务标识作为关联元数据，连续 stdout 合并到已有事件，空白 chunk 不再各自形成卡片。常规工具默认折叠，diff、进度和错误保留合适的展开方式；退出码、取消、超时、信号和失败读取真实结果，通用“完成”回执不会覆盖已有终态。`/raw` 可查看原始输出，显式 verbose/raw 启动选项仍进入对应模式。[活动事件](../../src/client/terminal/events.rs) · [状态关联](../../src/client/terminal/events/correlation.rs)

命令、结果、错误、成功、注释和路径使用语义色彩；保留源 ANSI，代码复用 Lexer，并尊重 `NO_COLOR`。过滤、折叠和鼠标定位共享同一事件视图。离开底部后，新活动保持当前视口；底栏显示剩余行数或新活动提示，`Ctrl+End` 回到底部并恢复跟随，手动滚到尾部也会恢复。[界面实现](../../src/client/terminal/conversation.rs)

caller 归组只合并展示，仍保留真实成员、收件人及单次审计身份；断线把旧运行状态标为未知，不把历史 run 自动恢复为运行中。重复 connector 认证警告按来源、同端点和错误细节归并计数，不同错误仍保留；紧凑摘要移除 URL 凭据和敏感路径、查询部分，按完整 stderr 行脱敏，覆盖凭据被不同 chunk 切开的情况。[调用归组](../../src/client/activity.rs) · [诊断归并](../../src/client/render/diagnostics.rs)

## 4. 回复提醒、历史与发布边界

终端要求第一次被真实领取记为 N。在同 owner、resource 和 agent 范围内，再经过两次相关响应仍未收到真实回复确认，才提醒一次。确认必须来自 native `ui.message` 或 `ui.present` 的成功 ACK，包含 `accepted=true`、`duplicate=false` 和有效 message ID；JavaScript 拼出的相似结果、失败和重复 ACK 都不清除等待。旧并发 ACK 不能清除新要求，响应预算不足则延后提醒。[提醒实现](../../src/code_mode/reminders.rs) · [响应接线](../../src/http.rs)

该机制只保存有界领取与确认元数据。持久历史继续使用已有存储，通过 `ui.history` 读取；队列沿用 wire-v7 和私有引用。helper 说明保留能力发现、权限限制、exec 新旧双签名及超时后原生任务可能继续运行的语义，详细契约按需从 guide 读取。[历史接口](../../src/client/interface/history.rs) · [工具说明](../../src/resources.rs)

终端基础组件提取为 `ztui-core`，发布工具先发布并验证该依赖，再发布 ssmcp，显式指定 crates.io，避免本机默认 registry 改变目标。发布工具测试中的模拟成功只证明脚本行为；本轮没有执行 crates.io 发布。本报告不从文本缩短、组件拆分或测试耗时推导性能收益。[发布脚本](../../scripts/release.py) · [组件](../../crates/ztui/Cargo.toml)

## 5. 验证矩阵

| 检查 | 已确认结果 | 证据边界 |
| --- | --- | --- |
| 完整 Rust workspace | **1,161 passed，0 failed，26 ignored** | d4 源码；ignored 未执行，不计入通过。 |
| Release 实际 UI | **47 checks，通过** | 已安装的 release 二进制，隔离真实 PTY；哈希 `59f29540…`。 |
| Cargo 打包 | **workspace --locked 通过** | `ztui-core 0.1.0` 与 `ssmcp 0.9.1`，未执行 crates.io 发布。 |
| Strict all-target Clippy | **通过** | d4 本地日志；三平台另有独立 CI。 |
| Session service CI | **Windows 2025、macOS 15、Ubuntu 24.04 全部通过** | [run 44320086144](https://github.com/LIghtJUNction/ssmcp/actions/runs/44320086144)，对应 d4。 |
| JavaScript helpers | **63 passed** | 本轮既有 helper 日志，单独记录执行范围。 |
| Release tooling | **58 passed** | 发布脚本测试，不代表正式发布已执行。 |
| 原生图片真实像素 | **50 checks，passed** | validated 候选，ztty 0.2.1、私有 Xvfb、X11 像素读回。 |
| Composer | **96 checks，passed** | 同候选；真实队列修订、收件人、草稿及本地 reexec，无模型。 |
| Session / attach | **69 checks，passed；其中 attach 22** | 同候选；旧草稿的 attach 失败已由此轮实际通过记录替换。 |
| 客户端 UI preview | **47 checks，通过** | 隔离 PTY 的主题、过滤、滚动提示、布局、输入及终端恢复；不等同用户前台已更新。 |
| 私有桌面 | **33 checks，passed** | 独立显示上的实际输入与截图；宿主指针及焦点保持。 |
| PNG / 文件粘贴完整套件 | **148 checks，passed** | 同候选；分块二进制、恢复、私有引用、裁剪协议，以及 Chat / Logs 的 Ctrl-C 和精确 termios 恢复。 |
| 教程 / 侧栏 QR 完整套件 | **35 checks，passed** | 四个本地路由 200；390 CSS 像素无横向溢出；zbar 严格解码测试源的根路径回退，小窗口显示 URL。 |

上表同候选指 `c56a977…` / `dbac984…`；release 产物另列哈希。多套 E2E 共用认证与连接前置检查，数量不能相加为独立功能覆盖总数。旧失败批次保留用于追溯：粘贴夹具现使用合法 PNG、IEND 与外层结束后再发 Ctrl-C，QR 夹具按实际 HTTPS 测试源验证根路径回退；两项完整通过结论均来自新的 v2 JSON。[验证摘要][validation] · [E2E 摘要][e2e]

## 6. 交付状态

release 构建和 `cargo package --workspace --locked` 均已通过。本地 Arch 安装包 `0.9.1.r377.gee05244-1` 于 **2026-11-18 22:39（Asia/Taipei）** 安装完成；包内二进制及安装后的三个路径均与固定 release 清单一致。release 客户端另通过 47 项真实 PTY UI 检查。[产物清单][artifacts] · [交付回执][delivery]

| 交付项 | 实际结果 |
| --- | --- |
| GitHub | #25、#26、#30、#32 均已合并并关闭；本地 main 已快进至合并提交 `5ce6618325647beae46879fd6afcec2231ca5904`，随后仅补充本报告与交付回执。 |
| 系统客户端 | `/usr/bin/ssmcp` → `ssmcp 0.9.1`，SHA-256 `59f29540db09e78dc21d49e17bedc279f7c0721bf5aba8939a7d1ef99d13bc13`。 |
| 用户客户端 | `~/.cargo/bin/ssmcp` → `ssmcp 0.9.1`，与系统客户端逐字节一致，避免 PATH 仍选中旧版。 |
| 系统服务程序 | `/usr/bin/ssmcpd` → `ssmcpd 0.9.1`，SHA-256 `99a237b1c1c93193c0168f240a7035a7a9f4d56da43dca8e5915b3020aec7a1e`。 |
| 回退文件 | 替换前的三支二进制已按 SHA-256 留存于开发机本轮 `out/completion-20261118/backup/`，没有覆盖旧备份。 |
| 证据归档 | 验证矩阵、脚本/产物哈希、PR 祖先关系、E2E 摘要与图像均已提交仓库。 |
| Google Drive | `ChatGPT` 目录保存 `ssmcp-client-completion-20261118.md` 与 `ssmcp-client-completion-20261118-evidence.json`；私人访问链接由交付消息提供。 |

安装结果证明磁盘上的文件已更新。没有把旧前台进程仍在运行时，磁盘文件替换成功当成前台已重新加载的证据；本轮未确认用户当前前台已完成切换。原生像素验收使用私有 Xvfb 内的 **ztty 0.2.1**，不等同于用户原有 **ztty 0.1.2** 前台已升级。

本轮没有重启或部署公网 relay / portal，没有执行 crates.io 发布。候选教程的四个本地路由、移动布局与根路径 QR 回退已有实际通过证据；本报告不据此断言公网 `/guide/` 的当前部署状态。安装后本机 `ssmcpd` 与 `ssmcp-portal` 服务仍为 inactive。

### 界面证据

以下图片由真实 PTY 的 VT 单元快照逐格渲染，保留原始颜色与文字；它们不模拟光标闪烁或 Kitty 图片层。原生图片和二维码另有真实 ztty 窗口截图及像素/扫码检查。

[活动区与语义颜色](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/client-completion-20261118/ui-chat.png`）

[滚动提示与返回底部快捷键](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/client-completion-20261118/ui-scroll-hint.png`）

## 7. 证据索引

以下摘要文件已经归档；delivery.json 已记录安装、合并和云盘保存的实际回执。原始记录位于开发机私有目录 `/home/lightjunction/Documents/GITHUB/ssmcp/out/completion-20261118/`，用于追溯，不作为仓库内可下载附件。失败批次保留原状态，最终重跑应新增或明确指向新报告。

| 仓库摘要 | 对应原始证据 |
| --- | --- |
| [validation.json][validation] | `final-clippy.log`、`final-workspace-tests.log`、`js-helpers.log`、`release-tooling-final.log`、固定源码 CI run。 |
| [artifacts.json][artifacts] | `validated-bins-manifest.json`、`release-bins-manifest.json`、`pipeline-state.json`。 |
| [integration-ancestry.json][ancestry] | 同名原始记录，含三个完整 head 与一致 tree。 |
| [e2e-summary.json][e2e] | `native-images-final/report.json`、`composer-final.json`、`final-session-attach/`、`client-ui-verified/ui-smoke.json`、`private-desktop-final.json`，`file-paste-final-v2/report.json`、`guide-final-v2/report.json`；旧失败批次保留。 |
| [delivery.json][delivery] | `install-result.json`、GitHub PR 状态、Google Drive 上传确认；明确未执行的公网部署和 crates.io 发布。 |

[validation]: client-completion-20261118/validation.json
[artifacts]: client-completion-20261118/artifacts.json
[ancestry]: client-completion-20261118/integration-ancestry.json
[e2e]: client-completion-20261118/e2e-summary.json
[delivery]: client-completion-20261118/delivery.json
