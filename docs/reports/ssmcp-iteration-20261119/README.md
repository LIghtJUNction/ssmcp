# ssmcp 门户、终端与运行体验迭代

记录日期：2026-11-19。本文对应正式版本 v0.10.13 及此前相关验收。门户先随 v0.10.10 上线；本轮后续功能与 Windows 测试修正已随 v0.10.13 正式发布和部署，Release 的 19 项任务全部成功。生产核对确认实际服务器运行 v0.10.13；007 现场验收确认保存和运行映像均为官方 v0.10.13、协议 7、connected。v0.10.11 和 v0.10.12 的失败记录及标签继续保留，未将旧候选的结果冒充新版本验收。[正式发布与生产核对](checks/production-release-0.10.13.json) · [007 现场验收](checks/007-live-validation-20261119.json)

## 门户：明确入口，减轻视觉负担

[线上门户](https://ssmcp.lmm.best/) 已换成浅色底、宽留白和新的品牌标志。首页把安装与源码入口放在清楚的位置，长说明分段展示，减少同时争夺注意力的边框、颜色和信息块。这里的简洁来自信息优先级：先让人明白可以做什么、如何开始，再展开细节。

![已部署的门户首页](images/portal-live-desktop.jpg)

此图来自实际线上页面。浏览器验证了首页和安装锚点，桌面没有横向溢出。v0.10.10 的正式 Release 共 19 个任务成功；服务健康检查通过，六个带指纹的静态资源与构建产物的字节、长度和 MIME 一致。部署检查对应 tag 源码 `52a531a59e9ec9236a76eafc0f4f068799e44b15`，不能用来证明后续终端修改已经上线。[部署证据](checks/production-release-0.10.10.json)

## 终端：先读内容，再看状态

请求队列现在以正文为主，短编号、状态、等待时间和接收者放到下一行。请求之间留出空行；选中状态明确。详情页保留完整编号和正文，鼠标点击绑定完整身份，避免列表滚动或换行后操作到另一个请求。[PR82](https://github.com/LIghtJUNction/ssmcp/pull/82)

![48 列终端的请求列表，使用合成数据](images/queue-requests-48.png)

连续成功的文件读取会折叠成可展开摘要。这样，读取大量技能文件时不会把真正的结果、修改和错误挤出视野。摘要只是显示层的组织方式：原始事件、顺序和完整路径仍保留；错误、命令、写入以及不同调用者之间的边界会终止分组。自动遍历跳过 `.trash`，明确指定路径时仍可访问。[PR85](https://github.com/LIghtJUNction/ssmcp/pull/85) · [读取投影验证](checks/read-activity-projection-checkpoint-20261119.json)

![连续读取折叠后，命令、修改与错误仍独立显示](images/read-activity-collapsed-120.png)

命令输出支持独立展开、折叠和滚动，展开后可核对完整命令或脚本。任务可以显示简短用途标题，实际执行参数不受标题影响。`ssmcp logout` 和终端 `/logout` 在退出界面后显示完整结果；真实验证覆盖远端成功、远端暂时失败和新登录凭据替换旧凭据三种情况。[真实终端验收](checks/terminal-acceptance-20261119.json)

## `@` 文件选择：点中哪一项，就发送哪一项

实际复现了鼠标选择候选项后没有正确完成引用的问题，现已修复鼠标命中与 UTF-8 光标的对应关系。点击只完成被选中的文件引用，随后按 Enter 才发送。真实 CLI/PTY 验证通过 68 项，覆盖普通路径、中文与空格、宽窄窗口、鼠标选择非首项，以及发送前文件消失时保留草稿和接收者。源码为 `24a4d50`，测试脚本为 `302bd2b`；后者只调整测试夹具。[PR88](https://github.com/LIghtJUNction/ssmcp/pull/88) · [完整验证](checks/at-reference-mouse-validation-20261119.json)

![窄终端中鼠标完成的中文文件引用，使用合成数据](images/at-mouse-completed-narrow.png)

## 强制发送：区分排队、投递和确认接收

[PR93](https://github.com/LIghtJUNction/ssmcp/pull/93) 已合入主线 `80e534c`。普通 Enter 继续把请求加入 Queue；显式 `/send <正文>` 则交给当前连接上唯一注册的监听程序。没有监听程序时，界面显示“发送失败：没有程序在监听”，正文、引用与接收者保留。

![没有监听程序时，失败提示和原草稿同时保留](images/force-no-listener-120.png)

底层 `ssmcp.ui.listener` 提供 `open/read/ack/close/status`。监听租约为 30 秒，同一时刻只允许一个监听器和一条待确认的强制消息；监听器需要在 5 秒内明确确认。`read` 只说明消息已经被提供给调用方，只有接受回执才显示“监听程序已接收”。这份回执不能证明模型已经接收或执行任务；未来主动调用模型的适配器可以建立在此协议上，本轮没有实现或调用该适配器。

拒绝、断线、超时和接收确认是不同结果。已经提供给监听器但没有确认的消息显示“结果未知”，不会自动重发或混入普通 Queue。重复读取保留同一投递编号；相同回执可以重试，冲突或迟到回执不能改写结果。即使用户重新输入相同文字，只要草稿修订已经变化，旧回执也不能清除新草稿。

![窄终端中的未知结果与保留的草稿](images/force-unknown-48.png)

冻结 CLI 的真实 PTY 验收通过 **116 / 116 项检查**，包含无监听、拒绝、接受、未知结果、精确文件引用、普通 Queue 隔离，以及实际 WSS 断线后的旧租约拒绝和无重放。第一次接口模块测试发现旧工具数量断言仍为 11；改为 12 并显式检查 listener 后，61 项接口测试、10 项监听测试和 1 项观察隔离回归全部通过。该修订只改变测试断言，产品 CLI 与真实 PTY 使用同一冻结二进制。初次失败结果保留在证据中。[实际 PTY 报告](checks/force-send-pty-report-20261119.json) · [构建与修复后复测](checks/force-send-build-validation-20261119.json) · [独立源码审阅](checks/force-send-listener-source-review-20261119.json)

## 等待用户：阻塞等待与准确交付

等待用户原语复用现有 Queue。它不额外启动模型，也不把系统通知当成用户回复。调用方可以在 `code_mode` 中使用：

```javascript
return await ui.wait(clientId, {
  timeout_ms: 20000,
  intent: "need_user",
  reason: "请确认下一步要处理的文件",
  notify: true
});
```

一次等待允许 1–25 秒，`intent` 可为 `need_user` 或 `finish`。服务器只考虑当前已认证拥有者、已经验证的 agent 绑定以及真正匹配的队列消息。队列有变化时唤醒等待；在最终响应提交前再次核对授权、消息身份和修订。只有同一响应中的 `user_wait` 回执与 `user_messages` 才能支持“已经交付”的结论。等待内部拿到的预留不会提前把消息标为 Delivered；取消、编辑、撤回或权限撤销会使相应预留失效。[协调器 PR97](https://github.com/LIghtJUNction/ssmcp/pull/97) · [源码复核](checks/user-wait-coordinator-source-review-20261119.json)

终端通过 `/wait` 查看正在等待的原因、编号、截止状态和通知结果；可以使用明确的唯一编号前缀取消某次等待。普通草稿不会遮住等待提示，错误、滚动、菜单和详情等已有交互仍保持自己的显示优先级。取消等待不会清空草稿、文件引用、接收者或普通 Queue。详情按真实换行显示，并逐行转义终端控制字符。[终端 PR95](https://github.com/LIghtJUNction/ssmcp/pull/95)

超时会返回继续等待的提示，不代表用户同意结束。提示钩子能处理显式的 `finish` 意图，以及经过本机确认的 `ui.message` / `ui.present` 最终答复。普通 MCP 服务看不到任意外部客户端自行产生的最终文本，因此本轮没有宣称可以拦截所有 ChatGPT 或其他客户端的结束行为，也没有把提示约束描述为不可绕过的控制机制。

系统通知按同一等待事件去重，并设置冷却与明确的执行时限。结果区分已提交、不可用和失败；`user_seen` 保持未知。Linux 的一次真实通知冒烟通过，并验证同事件重复调用被去重；这只证明通知已提交，不能证明用户看到了它。macOS 与 Windows 路径有实现与结构检查，尚未做真实桌面通知验收。[通知 PR94](https://github.com/LIghtJUNction/ssmcp/pull/94) · [一次真实 Linux 通知记录](checks/native-notification-smoke-20261119.json)

冻结源码 `a7f9077` 已通过 fmt、严格 clippy、等待相关 28 项、消息协调 31 项、接口 68 项、通知 8 项，以及完整终端 234 项测试。另有实际 Node/Bubblewrap 的上下文助手测试 2 项通过。提交前撤权和队列竞争由协调端 Rust 回归进一步验证。这些范围有重叠，不相加作为独立测试总数。真实 CLI/PTY 的最终验收另行通过 **119/119 项检查**，使用同一冻结 CLI；覆盖精确接收者、认证目标隔离与消息修订、取消、超时、断线、原有草稿及消息回执，没有省略检查。两次后续夹具修订分别等待精确成员出现在异步镜像中，以及容纳 48 列画面的真实软换行；产品源码保持不变。初次产品缺陷、夹具失败和非验收诊断都保留，未把诊断次数计入通过总数。[最终构建记录](checks/wait-local-final-build-20261119.json) · [真实 PTY 报告](checks/user-wait-pty-report-20261119.json) · [历次结果](checks/user-wait-acceptance-history-20261119.json)

![等待详情按真实多行显示，使用合成数据](images/user-wait-inspect-120.png)

## 纯本地运行：同一套工具，入口留在当前机器

Linux 上可以在项目目录运行：

```sh
ssmcp local --name dev --title "本地开发"
```

程序在本机回环地址启动 MCP 入口和终端会话，无需远程 ssmcp 服务器或 GitHub 登录。执行 `/logs` 查看本次 MCP 地址和一次性登录码；在同一台机器上支持 Streamable HTTP 与 OAuth 的 MCP 客户端中连接，再用当前会话的登录码授权。`/login` 会解释这个本地连接流程。退出本地会话会关闭监听与其运行环境，新的会话使用新的授权信息。[本地模式 PR96](https://github.com/LIghtJUNction/ssmcp/pull/96) · [使用说明](https://github.com/LIghtJUNction/ssmcp/blob/34b1f55966b0b5aa0af7c283660a5a56952b3652/docs/local-mode.md)

执行 `code_mode` 仍需要已经安装的 Node.js、Bubblewrap 和 `prlimit`，以及允许本次隔离操作的 Linux namespace 策略。本地模式把同版本的 `code-mode.js` 编进 CLI，启动时写入本次私有目录并明确交给执行器。因此单独复制 CLI 不会漏掉配套脚本；它也不会因为找不到沙箱依赖而退回无沙箱执行。

入口使用 `0700` 私有目录、`0600` 脚本和同有效 UID 校验的 Unix socket。HTTP 请求继续校验准确的 Host / Origin，并使用一次性授权码、PKCE、访问令牌和撤销逻辑。文件、命令、队列、强制发送和等待用户都进入原来的执行与授权路径。

### 使用 `.local` 名称

如果机器已经有正确的 `.local` 名称解析和受访问设备信任的 HTTPS 反向代理，可以固定监听端口并声明外部地址：

```sh
ssmcp local \
  --listen 127.0.0.1:8765 \
  --public-url https://workstation.local
```

名称解析负责找到机器，TLS 负责验证服务器并加密连接，反向代理把外部请求转到本机监听端口。ssmcp 不会仅凭这个参数自动完成 mDNS 发布、证书签发、信任安装或防火墙配置。当前验收覆盖本地网关和相关参数、来源与 Cookie 规则；没有把它写成已部署一套真实局域网 mDNS/TLS 环境。回环地址也只指向访问者自己的机器，云端 MCP 客户端不能通过 `127.0.0.1` 自动访问用户电脑。

最终冻结 CLI `a7f9077` 的真实浏览器与三描述符 PTY 闭环通过 **43/43 项检查**：客户端被复制到没有外部脚本的安装前缀，相关 runner 环境变量被清除；等待期间，夹具核对实际 Bubblewrap 绑定的是本次私有脚本，并验证内容哈希、拥有者及权限。浏览器授权文案、Cookie、PKCE、原生文件和命令、真实用户消息回执、空等待超时、授权码重放后的拒绝，以及退出后的终端、进程、目录和监听清理均通过。没有调用模型，也没有借用调试构建的源码脚本回退来充当嵌入脚本验收。[真实闭环报告](checks/local-mode-a7f9077-validation.json) · [授权页视觉检查](checks/local-consent-visual-review-20261119.json)

## 007 后台恢复服务：让保存的旧版本可以明确升级

升级前的现场排查发现，007 的 systemd 用户服务仍在运行，保存的程序是 v0.7.4，连接处于反复重连状态。保存文件与实际运行映像都与此前安装记录中的 SHA-256 完全一致；其对应源码使用协议 5，而当前网关只接受协议 7。这解释了旧程序无法完成注册。旧协议版本来自精确映像与源码的对应核对，现场缺失的协议字段仍记为未知。日常 CLI 更新没有解决它，因为 007 本来就保存独立的程序副本，重复安装也有意保留这个副本。进程活着与连接可用，是两个不同状态。[只读实际安装预检](checks/007-upgrade-live-preflight-20261119.json) · [现场基线](checks/007-live-baseline-20261119.json) · [协议与安装记录核对](checks/007-protocol-diagnosis-20261119.json)

[PR98](https://github.com/LIghtJUNction/ssmcp/pull/98) 增加显式升级命令：

```sh
ssmcp 007 upgrade --dry-run --json
ssmcp 007 upgrade --json
ssmcp 007 status --json
```

升级先确认 systemd 实际加载的服务属于这份安装，再保存独立的旧程序备份，暂存当前正在执行的 CLI，原子替换恢复程序并只重启 `ssmcp-007.service`。只有新 MainPID、实际程序映像身份与本次操作后的 `connected` 观察全部匹配，才报告成功。额外 drop-in、待重新加载的定义、其他安装或无法确认的运行进程会使操作停止。

激活失败时会尝试回滚，并分别报告旧文件是否恢复、服务是否启动、连接是否恢复。旧文件仍然保留；如果旧版本本身已经不兼容网关，恢复字节不能被写成恢复了连接。配置中的日常客户端路径、工作区、服务器，以及既有完整会话的定义、程序快照和独立生命周期都保留。[升级源码复核](checks/007-upgrade-root-review-20261119.json)

冻结源码 `21ea0ac` 通过 fmt、严格 clippy、12 项 007 测试和 46 项会话相关测试。隔离的升级夹具另通过 **14/14 项检查**：使用真实恢复进程、真实认证连接、真实完整会话和运行中的任务，服务管理器使用私有替身。成功升级、另一安装被拒绝、故意触发启动失败后的回滚，均验证了既有会话与任务持续运行、文件原样保留。此夹具从同一候选的独立副本开始；它不能代替旧 v0.7.4 保存副本在真实 systemd 上的迁移验证。[冻结构建](checks/007-upgrade-build-20261119.json) · [升级事务与实际回归报告](checks/007-upgrade-validation-20261119.json)

原有恢复流程另通过 **47/47 项检查**，包含 32 项生命周期检查和 15 项真实生产 Broker/HTTP 路由检查。两次 HTTP `code_mode` 调用到达原 client ID、原 PID，完成观察帧之后保持连接；测试没有用重新连接来满足这条断言。首次运行因旧 v5 断言失败，原记录保留，修正为当前 v7 后完整通过。

实际旧安装此前的 `--dry-run` 已通过。控制连接恢复后，22:02 UTC 的新预检发现该服务已经更新为 v0.10.13，并处于 `connected`；systemd 记录的新进程启动时间为 21:15:10 UTC。本轮没有再次执行实际 `upgrade` 或重启，原更新的执行者未确认，因此这里只记录可直接验证的现状与保全结果。[恢复后现场预检](checks/007-live-existing-upgrade-preflight-20261119.json)

保存映像和实际运行映像的 SHA-256 均与下载后验证的官方 v0.10.13 程序一致。保留的旧程序备份与原 v0.7.4 保存文件和运行映像完全匹配，权限为 `0700`；10 份保存会话的定义与快照、配置及服务定义均与原始基线一致。现场没有正在运行的托管会话，运行中任务的保全依据仍是前述独立隔离验收。[官方程序校验](checks/007-official-candidate-0.10.13.json) · [最终现场状态](checks/007-live-final-state-20261119.json)

22:07 UTC 又经两个独立的 `code_mode` 响应，实际调用 `ssmcp.007.status` 与 `ssmcp.007.list`：前者返回 v0.10.13／协议 7，后者返回 10 份保存会话。两次调用后仍是原来的 client ID 和 PID，连接保持 `connected`；核对期间另一个 `ssmcp.service` 也保持原有进程身份。该结果验证了完成观察帧不会使恢复连接断开，未启动新会话或调用模型。[两次实际 MCP 调用与完整保全记录](checks/007-live-validation-20261119.json)

## 服务器所有者的额度显示

已核对现有服务器策略：所有者的稳定 GitHub 主体已配置为无限额度，账号未封禁，策略无需修改。界面现在将服务端明确返回的无限额度显示为 `∞`，并保留无障碍文字说明。缺失、部分数据或请求失败仍显示未知；不会因账号名字或错误而自行推断无限。24 项定点测试通过，测试源码 `db11132`，合并提交 `d60ef8d`。[PR89](https://github.com/LIghtJUNction/ssmcp/pull/89) · [验证记录](checks/owner-unlimited-quota-validation-20261119.json)

## 两层插件化：状态与执行一起迁移

[PR91](https://github.com/LIghtJUNction/ssmcp/pull/91) 已合入主分支 `d312cfa`。内核保留权限、路由代次、审计、输出边界和共享生命周期；能力插件持有实际后端状态，通过版本化的 zplugin 静态 ABI 执行、取回结果并清理资源。

| 能力插件 | 负责的能力 |
| --- | --- |
| Files | 工作区文件、上下文授权与引用、rsync |
| Desktop | 浏览器、计算机操作、Omarchy；共享原本的本地授权实例 |
| Process | 终端、独立命令、UI 能力以及显式加载的外部协议适配器 |

这些内建插件目前通过 `from_static` 装入同一个程序，不等同于已经提供任意第三方插件安装或热替换功能。它们共享一个延迟启动的执行线程，该线程并发轮询任务；各插件不各自增加一个常驻执行线程。现有动态 Extension v1 的兼容路径保留。

迁移保持原先的权限与生命周期边界：过期路由在执行前拒绝；同一个观察状态和请求身份传入插件；图像、结构化内容与 `isError` 保留；取消后先结束活跃任务，再调用各插件的清理。旧的有界等待策略仍适用，没有把清理返回成功夸大成新的逐进程认证保证。

冻结源码 `d4b0558` 的库/测试编译、fmt、严格 clippy 以及实际 CLI 回归全部通过。真实 ABI 测试覆盖并发、取消、目录代次、超过旧扩展额度的大结果、图像与共享文件权限；实际 CLI 覆盖原生工具、按需加载适配器、桌面、会话及文件前端。外部模型协议使用隔离的确定性夹具，未调用模型服务。[验证记录](checks/native-plugin-validation-20261119.json) · [独立源码审阅](checks/native-plugin-source-review-20261119.json) · [项目插件设计](https://github.com/LIghtJUNction/ssmcp/blob/d312cfa8036e433d7861782b4d928cee2320bc85/docs/native-plugins.md)

## 切换系统用户后可以独立运行

旧问题不只是临时目录的黏滞位。共享目录即使允许创建文件，程序仍可能因为目录拥有者不匹配而拒绝使用；通过 `su` 继承的终端描述符可以写入，重新打开原用户的 PTY 路径却可能被权限拒绝。这是两个独立的失败点。

输出目录现在按有效 UID 隔离到 `/tmp/ssmcp-<UID>`，目录权限为 `0700`，文件为 `0600`。每个系统身份管理自己的输出，旧目录和现存文件会保留。终端路径遇到权限错误时，使用经过会话与设备一致性核对的控制终端描述符；它不会修改父进程所持描述符的非阻塞标志。

真实双 UID 验证通过 13 项，包括跨用户读拒绝、旧目录保留、错误权限拒绝、同一控制终端写入、不同终端拒绝以及父描述符标志不变。正常输出模块另有 29 项测试通过。[PR84](https://github.com/LIghtJUNction/ssmcp/pull/84) · [跨用户运行验证](checks/per-user-runtime.json)

## 性能：报告实际测到的范围

ANSI 帧差分的优化已经做过同机成对测量。六轮交替运行，每轮 50,000 帧，使用相同编译选项与相同基准程序；所有原始测量和离群点都保留。

| 场景 | 修改前中位耗时 | 修改后中位耗时 |
| --- | ---: | ---: |
| 首帧 | 2771 ns | 1775 ns |
| 共享缓冲区且画面不变 | 439 ns | 188 ns |
| 所有行变化 | 2901 ns | 2048 ns |
| 调整尺寸 | 2659 ns | 1727 ns |

内容相同但内存分配不同的对照场景，成对耗时比中位数为 1.004，基本持平。13 个契约测试和逐字节对照保护终端输出的一致性。这些数据只衡量帧差分及写入复用字符串，不包括布局、网络或真实终端绘制，不能推导整个应用提速同样比例。[基准记录](checks/ztui-frame-benchmark-20261119.json) · [契约验证](checks/ztui-frame-performance-validation.json)

`ztui-core` 的性能源码使用独立补丁版本 `0.1.2`；根依赖、锁文件和示例保持一致，避免复用已经发布的 `0.1.1` 包版本。[PR83](https://github.com/LIghtJUNction/ssmcp/pull/83) · [包版本验证](checks/ztui-publication-version-20261119.json)

### 内存专项

guardian 自执行实验通过了实际任务生命周期与故障注入检查，但没有达到降低内存的目标，因此没有合并到主分支。相同 debug 构建、相同负载下，默认 guardian 的 PSS 中位数从 1995 KiB 增加到 3487 KiB，父进程与 guardian 合计从 21351 KiB 增加到 22818 KiB；小负载场景也没有改善。匿名页虽有减少，新的程序映像增加了总成本，不能只选择好看的匿名页数字报告。实验与测量保留在 [PR87](https://github.com/LIghtJUNction/ssmcp/pull/87)，该 PR 已关闭。后续仍需验证更合适的常驻内存优化；上述实验二进制不会作为正式版本发布。

另一项候选是折叠后释放 EventView 的正文渲染缓存。真实路径核对排除了最初设想：生产 `/logs` 使用原始终端输出，Conversation 读取组只保留来源与路径，不缓存所读取文件的全文。实际 PTY 读取 12 个各 2 KiB 的文档，只产生 720 字节、12 行的读取记录；33 项可达性与恢复检查通过。四个展开/折叠阶段的前台 PSS 中位数约 23,021–23,053 KiB，使用的是同一份未修改程序，不能解释成优化效果。因此也没有提交这个缺乏收益证据的产品补丁。[真实路径测量](checks/event-cache-real-pty-probe12-20261119.json) · [独立复核](checks/event-cache-candidate-review-20261119.json)

内存目标目前没有被标记为完成。两条调查都保留了原始结果，既不降低历史容量来制造改善，也不把内部缓存字节数与操作系统的实际占用混在一起。

## 验证记录如何对应版本

| 源码或版本 | 已完成的验证 | 可以支持的结论 |
| --- | --- | --- |
| v0.10.10 / `52a531a` | 正式 Release 19 项任务、生产健康与六个静态资源校验、实际浏览器检查 | 门户已部署，验证时服务器运行该版本 |
| `2f0886a` | 18 组实际 CLI/PTY 场景；另有终端 211 项、守护进程 182 项、Linux portable 42 项 | 账号与 WSS 迁移后的已记录主要流程通过 |
| `68e52bd` | 新队列与读取摘要；真实注销 35 项、composer 87 项；更新断言后的 client UI 56 项、canvas 68 项 | 新界面及注销行为在该冻结候选中通过 |
| `24a4d50` | 完整终端 222 项、composer 14 项、context 8 项、output 29 项；真实 `@` 引用 68 项；fmt 和严格 clippy 通过 | 鼠标引用修复的稳定候选通过，未包含 guardian 实验 |
| `d4b0558` / 合并 `d312cfa` | 运行时 41、后端 83、注册 4、上下文 8、终端 222、输出 29；8 组实际 CLI 场景及 `@` 引用 68；严格 clippy | 内建两层插件化的对应路径在 Linux 候选通过 |
| `acfdc0e` / 修订后 `9dcc311` / 合并 `80e534c` | 真实强制发送 PTY 116 项；监听 10、观察隔离 1、修复后接口 61；终端 226、运行时 41；构建、fmt、严格 clippy | 单监听器强制发送在对应 Linux 候选通过，未调用模型 |
| `db11132` | 账号 6 项、门户 10 项、会话页 7 项、所有者策略 1 项 | `∞` 仅用于服务端明确的无限额度 |
| `68cc1ff`（未合并实验） | 完整终端模块 222 项、context 8 项、output 29 项；真实 CLI core 125 项、unified 105 项 | 集成队列、读取摘要、跨用户运行与 guardian 候选后的对应回归通过 |

这些组之间有覆盖重叠，不把数量相加作为独立测试总数。`68e52bd` 的初次验证确实包含一个旧队列布局单元断言和两个旧完整编号列表断言失败；原始记录保留，后续修订与复测另行记载。`68cc1ff` 的完整模块结果包含嵌套测试，早期范围较窄的筛选不作为完整模块通过的依据。[旧统一验收](checks/final-account-acceptance-20261119.json) · [稳定界面构建验收](checks/stable-ui-validation-20261119.json) · [未合并实验的完整模块记录](checks/guardian-candidate-unit-validation.json)

## 尚未宣称完成的部分

Windows 与 macOS 的正式平台门禁通过，不能替代其真实桌面通知验收；通知实际送达用户视野仍保持未知。纯本地入口的完整实测范围是 Linux；`.local` 名称解析与可信 TLS 仍由既有网络环境提供。内存专项没有得到可交付的收益，相关实验结论与未合并分支继续保留。

后续提出的逻辑 session/thread、广播或全屏休眠界面扩展不属于本次 v0.10.13 的验收范围，其实现与状态应以对应后续提交及独立验证为准。本次已验收的是托管进程会话及其连接、恢复与生命周期管理，不将后续主线变更套用到本报告。

本次 v0.10.13 没有将跨独立 MCP endpoint 的多会话互联作为新增实现交付，也未将设计中的 `remotes.resolve`、`target.call` 或 `target.codeMode` 写成已实现能力。设计明确区分目标身份、原始结果、未知执行结果和独立任务生命周期；完整建议见[多会话路由设计](multi-session-routing-design.md)。

## 正式发布状态

v0.10.11 的正式 Release `44563443850` 在 Linux x64 的工作区测试门禁失败后停止。失败原因是 `cc_backend` 测试使用进程号和时钟生成临时目录名，同一时钟刻度可能碰撞；发布、crates 发布与部署步骤均未执行。该标签没有移动，也没有在相同源码上盲目重试。

[PR92](https://github.com/LIghtJUNction/ssmcp/pull/92) 增加进程内计数器，并补上同一时钟刻度下创建两个隔离目录的回归。对应实际运行通过 6 项，1 项真实外部模型测试按原策略忽略。此修复已合入 `7438e61`。该次发布失败后，生产保持已有的 v0.10.10；后续版本仍须通过自己的正式门禁。[发布失败记录](checks/release-0.10.11-failure-20261119.json) · [夹具修复验证](checks/cc-fixture-directory-validation-20261119.json)

### v0.10.12 正式发布被 Windows 门禁拦住

正式 Bump `44601617428` 已成功，Release [`44601683739`](https://github.com/LIghtJUNction/ssmcp/actions/runs/44601683739) 的 Windows x64 与 ARM64 测试均出现同样两项失败：测试调用了 Windows 明确不支持的自动私有粘贴入口，并要求关闭监听后的 TCP 连接必须在 1 秒内失败。[PR99](https://github.com/LIghtJUNction/ssmcp/pull/99) 已按实际平台合同修正测试并合入主线 `3e84fe48`，保留公共引用、授权与资源清理的检查，生产实现未变。标签源码为 `c57fb6d7a25cb5967811180cfc77214ade63c0d2`，其唯一父提交是完成验收后合入的 `571a9879`，只修改版本清单与锁文件。此前的 v0.10.11 标签保持不变。

该次正式 Release 最终为 13 项成功、2 项失败、4 项跳过：六个平台构建、两套 Linux 和两套 macOS 门禁，以及两个已安装服务器运行测试通过；两项失败均为上述 Windows 测试。GitHub Release、crates 发布与部署均跳过，两个版本发布接口返回 404。该次失败后的独立只读核对确认生产当时仍为 v0.10.10，二进制及六个静态资源与既有正式部署一致。旧标签保持不变；修正后的源码已纳入下述 v0.10.13 正式补丁发布。[失败与生产状态记录](checks/release-0.10.12-final-failure.json) · [正式启动记录](checks/stable-release-0.10.12-started.json) · [主线与冻结候选的逐字核对](checks/final-main-source-equivalence.json)

PR99 的 Linux 定点验证通过 fmt、严格 clippy、库测试构建，以及实际执行的 Files 模块 **3/3 项测试**。验证使用冻结测试程序 `b4581446…`；合并提交的完整 Git tree 与 PR head `0dc0ce99` 相同。记录保留了此前连接中断、没有最终退出状态的尝试，未把该尝试算作通过。修订后，Unix 仍验证私有粘贴与引用展开，非 Unix 明确核对现有“不支持”错误；监听清理要求同一地址在关闭前绑定失败、关闭后重新绑定成功。这组定点验证的实际执行平台为 Linux；后续 Windows 正式门禁已在 v0.10.13 完整通过，007 的当前运行与会话保全则使用独立现场记录核对。[定点验证](checks/windows-files-fixture-validation-20261119.json) · [合并树核对](checks/windows-files-main-merge-validation.json)

### v0.10.13 正式发布、生产核对与清理完成

正式 Bump [`44610146313`](https://github.com/LIghtJUNction/ssmcp/actions/runs/44610146313) 成功后，Release [`44610211292`](https://github.com/LIghtJUNction/ssmcp/actions/runs/44610211292) 的 attempt 1 完成 **19/19 项任务**。Windows x64 与 ARM64 的完整检查均通过，六个平台程序包和两套已安装服务器运行验证全部通过。发布标签指向 `4e3132d60600dca5053af81cdefa6962c3058ac4`，唯一父提交为已经验收的 `3e84fe48`，差异仅为 Cargo.toml 和 Cargo.lock 中的版本号。[正式启动与源码对应记录](checks/stable-release-0.10.13-started.json)

[GitHub v0.10.13](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.13) 已正式发布 16 项 assets；crates.io 的 `ssmcp 0.10.13` 和 `ztui-core 0.1.2` 均经实时接口确认存在且未撤回。自动 CD 已完成，运行程序与正式目标哈希一致；21:59:23 UTC 的独立只读核对再次确认实际 `/proc/MainPID/exe` 为 `ssmcpd 0.10.13`，检查前后 PID 稳定，直接入口与公开入口健康检查均通过，主页及全部 6 项指纹资源的长度、SHA-256 和 MIME 一致。[发布与当前生产实测](checks/production-release-0.10.13.json) · [官方 CD 回执](checks/deployment-receipt-0.10.13.json)

部署清除了 23,015,657 bytes 临时材料；22:03 UTC 核对时，本次服务器部署目录仅保留 intent 与 receipt，共 7,307 bytes。stage、大型临时件和已无引用的旧 managed runtime 均已清理，服务器空闲空间为 17,453,576,192 bytes。旧 v0.10.10 恢复材料已在开发机统一备份位置保存并再次校验：15 个文件，归档 6,778,880 bytes，内容、权限、归属与链接信息可核对；恢复时需从开发机传回服务器。业务数据库未纳入这次程序恢复包。

007 的最终现场结果以其独立章节与证据为准：新预检时升级已经存在，本轮保留现状并补充只读验收，没有重复升级。正式发布与现场观察分别记录，避免把“已发布”、systemd “运行中”和 MCP “实际可用”混成同一个结论。

## 文件与证据

本报告附带经过检查的公开证据和实际渲染图片。除门户首页外，终端图全部使用合成数据。原始用户截图中的账号或一次性授权信息不进入交付包；证据中的私有机器路径被替换为说明性占位符，源码与二进制哈希、失败结果和测量数字保留。

[证据清单](evidence-manifest.json) 列出文件哈希。代码在 [LIghtJUNction/ssmcp](https://github.com/LIghtJUNction/ssmcp)，线上入口为 [ssmcp.lmm.best](https://ssmcp.lmm.best/)。 完整云端文档见 [Google Docs 迭代与验收报告](https://docs.google.com/document/d/1UwbjVcVW8q8DZ37HcsItcKr6dqYHX46qHR93BMB0i_Q/edit)。
