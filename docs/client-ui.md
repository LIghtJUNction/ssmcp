# 客户端 UI

连接可用的功能以客户端和中继公布的能力为准。[0.10.21 版本说明](release-notes-0.10.21.md)

ssmcp 默认使用 `dark` 主题和 `chat` 布局，状态侧栏默认收起。对话正文占据主区域，工具调用显示为简短摘要，底部输入区用于追加要求。按 F3 或输入 `/sidebar` 打开侧栏，可查看成员、队列与工作区状态；再按一次即可收起。

顶部左侧显示项目，右侧显示当前页；窄窗口使用简短的页面标题。默认深色主题以深灰底区分输入区，不加额外框线，空草稿显示 `Message the agent`。侧栏收起时，账号与本月剩余流量显示在输入区上方的一行中，空间不足时优先显示余量。底部保留连接来源、连接状态和当前操作提示。

启动信息、后端诊断和原始终端记录在 Logs 中查看。按鼠标右键或输入 `/logs` 打开，Esc 返回先前的页。输入的要求先进入可编辑队列，在符合接收目标的后续 `code_mode` 响应中交付；显示设置独立于工具执行。

## 先预览，再连接

```sh
ssmcp ui preview
ssmcp ui preview --theme light
ssmcp ui init
ssmcp ui show
ssmcp ui check
```

`preview` 使用实际终端渲染器展示固定示例，不连接服务器、不启动后端，也不会执行输入的要求。示例成员、命令、输出和耗时只用于检查布局。F1 打开快捷键与本地命令帮助；修改保留在内存，F8 可以保存到配置文件。输入 `/exit`、`/quit`，或按 Ctrl-C / Ctrl-] 退出预览，再运行 `ssmcp` 连接设备。

| 命令 | 用途 |
| --- | --- |
| `ssmcp ui preview` | 交互预览主题、布局和状态面板 |
| `ssmcp ui init` | 创建默认配置；已有文件时停止，保留原文件 |
| `ssmcp ui show` | 以 TOML 输出加载后的配置 |
| `ssmcp ui check` | 检查配置格式、颜色和参数范围 |

这些命令可使用 `--ui-config FILE` 指定配置。`show` 和 `preview` 还接受 `--theme NAME`，便于比较预设。`show` 和 `check` 读取文件；运行中界面的当前状态通过 `ssmcp.ui.get` 获取。

无交互终端时，可以输出一帧真实渲染结果，用于截图或检查不同窗口尺寸：

```sh
ssmcp ui preview --static --rows 36 --cols 120 --theme light
```

`--rows`、`--cols` 范围为 `1`–`1000`，只用于 `--static`，默认 `36` 行、`120` 列。输出包含 ANSI 序列；`NO_COLOR` 可关闭颜色。交互预览需要前台终端、`dashboard = true` 和 `pretty` 或 `pretty-verbose` 日志模式。

## 输入要求与队列

输入框交付功能用于直接连接服务器的前台客户端。后台连接可以通过 `ssmcp.ui.queue` 管理要求；本地 `attach` 目前只观察后台会话，不转发 attach 自己的输入框队列。

在底部输入框写下要求，按 Enter 提交。本地入队成功后，正文保存在客户端队列中，输入框立即清空；如果此前暂存了另一份草稿，则立即恢复其文字、接收目标和光标。此时可以继续写下一条要求，后续输入不会追加到刚提交的正文。队列条目在等待服务器确认时显示 `sending`；确认只更新队列状态，不清空后来输入的草稿。发送失败时，队列记录和当前草稿保留，供用户检查。前台输入与后台 `ssmcp.ui.queue` 提交的要求都在服务器确认接收后写入本地会话历史，保存失败会提示；`requested:true` 只表示已请求处理，不表示已经确认或交付。

默认目标为 `Next caller`，即下一次实际访问本客户端、且仍有授权的 `code_mode` 调用方；初始画布中，也可以在输入框为空或光标位于整份草稿开头时按 ←，从相邻的 Agent 面板选择成员。要求只附加到符合目标的响应，不广播给全部成员。

自定义界面刷新 `ssmcp.ui.*`、查询工具目录，或调用 `capabilities(clientId)` 与 `resources.forClient(clientId)` 的 `list/templates/read` 时，默认不领取这些要求，避免界面轮询抢先消耗队列。后两者对应 `ssmcp.client.capabilities` 和 `ssmcp.resources.list/templates/read` 四个观测原语，原生调用仍记录源码审计。显式通过 `collab.bind` 绑定成员的编排仍可领取符合目标的要求。

`/queue` 打开队列。内容区中 ↑/↓ 选择记录，Enter 请求取回尚未交付的要求，I 只读查看详情，Esc 返回列表，Delete 取消可撤回的待交付项。取回获得服务器确认后，文字进入输入框，修改后按 Enter 重新提交同一条要求。输入框与内容区通过 Tab 或 Shift-Tab 切换。

| 状态 | 含义 |
| --- | --- |
| `queued` | 已确认入队，等待符合目标的 `code_mode` 响应 |
| `editing` | 已取回，交付暂停，等待重新提交 |
| `delivered` | 已附加到响应，无法修改已经交付的内容 |
| `sending` | 正文已存入本地队列，正在等待服务器确认；输入框可继续编辑下一条 |
| `taking-back` / `cancelling` | 取回或取消正在等待确认，当前草稿不变 |
| `uncertain` | 先前交付无法确认，或服务器已裁剪该项历史，需要复核后决定是否补发 |
| `retarget-required` | 连接或原接收目标已经变化，需要重新选择目标 |
| `offline` | 当前未连接，要求尚未发布到服务器 |
| `cancelled` | 已取消，不再等待交付 |

输入框为空时按 ↑，优先尝试取回最新的待交付要求。输入框有字时，↑ 先移动到上一个显示行，到边界后可浏览输入历史。已发出的要求等服务器确认取回后才进入编辑，确定尚未发出的本地待办可以直接取回。普通取回后按 Enter 用原请求 ID 提交修改。服务器确认取回和重新提交时，都会推进该请求的修订号。

确认取回后，客户端按请求 ID 和原始正文移除对应的待交付气泡及 JSONL 记录。同文的其他请求与 Logs 审计仍可核对。若交付先发生，取回会失败并显示最新状态。删除历史记录失败时会另行提示，取回仍有效，磁盘可能留有旧记录。

在队列中对 `uncertain`、`retarget-required` 或 `offline` 项按 R，会显式改为 `Next caller` 并进入复核编辑，须按 Enter 才重新提交。对 `delivered` 项按 R 只复制成一条新草稿。已经发出、仍属于旧连接房间的未知请求保留原记录；显式改投新房间会创建独立请求 ID，先停在 `editing`。新尝试的 `retry_of` 指向原请求，取回新尝试不会撤下旧请求的历史。

复制队列内容时，输入框中的原草稿会暂存。在输入框编辑复制出来的普通草稿时，按 Esc 可恢复原草稿的文字、目标和光标。补全菜单打开时，Esc 先收起菜单。取回编辑和正在提交的队列项继续保留各自状态。已有暂存草稿时，界面会阻止再次复制或取回另一条要求来覆盖它。

已交付只描述 ssmcp 的响应处理，不证明模型已阅读、同意或执行要求。响应可能在网络中丢失，因此交付不确定的记录不会自动重发。没有后续符合目标的调用时，要求继续等待；客户端不会启动模型或立即打断任意外部会话。

单条要求和输入草稿最多 16 KiB UTF-8 字节，允许换行与制表符。服务器每条客户端连接最多保留 64 条待处理要求、64 条近期结束记录，队列总容量为 512 KiB，含封装开销；容量不足时明确返回错误。每次响应最多附加 8 条、合计 64 KiB，其余继续等待。

要求附加在 `code_mode` 响应的 `structuredContent.user_messages` 和单独的用户要求文本块中，不替换原生输出，也不改变原本的成功或错误状态。`list` 和直接 `collab` 响应不承担这条交付通道。[队列原子接口](#要求队列接口)

为代理包装或压缩工具结果时，应保留 `user_messages` 及对应文本块，并将要求交给代理处理。只转发原生结果里的 `result` 或 `stdout` 会丢掉这条交付通道；界面的已交付状态仍只表示服务器把要求放进了响应。

## 文件、技能与长粘贴

普通要求和本地监听发送支持 `@src/example.rs`、`@"docs/设计 说明.md"` 与 `$review-helper`。路径从客户端工作区查找，须能读取其中的普通文件。工作区内的绝对路径可转为相对路径。含空格的路径可用单引号或双引号。

技能按元数据名称查找，依次使用项目、显式上下文根、已登记的全局技能目录和内置技能。数字开头可写成 `$"007-helper"`，`$100` 保留为金额文字。引用随消息提供技能元数据、来源和 `ssmcp.skills.read` 入口，接收方先读完整技能再使用。

用 `\@example.txt`、`\$review-helper` 或反引号代码保留字面内容。原生消息正文也保持原文。`/raw` 只切换原始日志视图。

缺失、不可读或越界的文件以及未知技能会显示引用警告，合法正文仍可发送。每条要求最多 8 个不同引用，单个文件最多 64 MiB，文本预览最多 1 KiB，完整技能文档最多 256 KiB。原文和展开后正文各受 16 KiB UTF-8 字节限制。空间不足时，不可用引用的提示可只留在本机回执中。

未闭合引号、引用数超限或有效引用使正文超出限制时会报错。普通队列把原始正文保留为可编辑要求，本地监听发送保留输入框草稿。普通队列重试沿用已准备的数据，取回并编辑后重新检查引用。[输入语法与查找顺序](client-input.md#2-用--引用文件用--引用技能)

图片的原生读取接口是 `ssmcp.fs.read_image`，接受工作区相对路径，支持 PNG、JPEG、GIF、WebP，单图最多 400 KiB。原生结果返回后，前台按终端能力显示彩色或灰度缩略图；日志不展开 base64。旧中转端可继续处理 PNG/JPEG，GIF/WebP 还需要中转端使用支持这两种 MIME 的版本。

支持整段粘贴事件的 Unix 终端中，粘贴正文**超过 4096 个 UTF-8 字节，或至少有 8 行**时，会保存到私有的 `.ssmcp/inputs/paste-<随机ID>.txt`，再把带引号的 `@file` 插入草稿。较短文本直接进入当前编辑器。保存保留原文的 UTF-8 与换行字节，在原光标位置插入引用并保留周围草稿与收件人；异步保存期间若用户已换草稿或目标，完成结果只提示可恢复路径，不覆盖新内容。

正常大小的粘贴若因目录、权限或写入错误而失败，原文保留在当前客户端进程内，界面显示保留字节数。`/paste-retry` 重试保存同一份原文，`/paste-retry discard` 明确丢弃保留文本；未处理前不会用新输入任务替换这份待恢复内容。单次最多 4 MiB UTF-8，超限会明确报错。Windows 可手动保存工作区文件再引用。输入文件不会自动删除，请在确认后续读取结束后自行管理。[完整语法与存储边界](client-input.md#4-长粘贴保存为文件再继续写要求) · [阈值与私有写入实现](../src/client/references/paste.rs)

## 工具窗口与滚动

命令输出显示在有限高度的独立窗口内。鼠标位于对话正文左半区时，滚轮优先滚动所指向的命令窗口；达到窗口边缘后仍停留在该窗口。鼠标位于正文右半区时，滚轮始终滚动整段对话。每个命令窗口分别保留自己的位置，滚动不会改动输入草稿。输入框和侧栏不属于这两个正文区域。

## 水平画布与 Agent

画布是一列可扩展的面板序列。初始从左到右为 Agent、对话、源码审计、队列、问题、历史、Logs 与 Tasks；任务列紧跟日志列。按 Alt+← / Alt+→ 切换相邻列；内容区拥有焦点时，普通 ← / → 也切列。输入框内，方向键先移动文字光标，只有到整份草稿的开头或末尾才切到相邻列。到画布边缘会停下，列顺序不循环。

`/new chat` 在当前列右侧新建对话列，也可以使用 `agents`、`audit`、`queue`、`questions` 或 `history`。在 Logs 或 Tasks 上新建时，新列放在这对面板之后，保持日志与任务相邻。`/close` 关闭当前视图并返回左邻，至少保留一列；显式执行 `/logs` 或 `/tasks` 会恢复缺失的对应列。列分别记住选择、滚动、详情与工具展开状态；普通草稿和收件人沿用当前会话。历史列保存各自查看的会话，切列只恢复阅读位置。关闭视图不删除会话或取消任务。

初始对话列左边就是 Agent。进入后 ↑/↓ 移动选择，● 标记当前目标；对已登记成员按 Enter 后返回对话，底部 `To` 显示收件人。Esc 返回对话并保留草稿与光标。切换目标后仍需在输入框按 Enter 才发送要求。

列表突出成员名称与报告状态，同名成员附带缩短的 ID；宽窗口在列表右侧展示角色、状态与身份详情，窄窗口保留紧凑列表和返回提示。自动观察到的普通调用以 `Call` 和 `view only` 标示，选择后进入源码与结果审计；详情中按 Esc 或鼠标左键返回列表，再按 Esc 回到会话。普通调用无需先执行 `collab.join`，查看它不会修改消息收件人。

本地命令入口已移除 `/agents`，命令候选和帮助统一提示方向键入口。

普通调用按调用来源、名称和模型归并，单次运行仍有独立的 `run_id` 可供审计。侧栏预览近期调用；统计区分别显示已登记成员与观测调用方。这些观察记录不提供可认证的收件人身份，定向要求仍使用已登记成员的 `agent_id`。名称或模型缺失时显示未知，不借用其他调用的身份。

## 日志列与任务列

鼠标右键或 `/logs` 聚焦 Logs。日志列显示启动、发现、诊断与原始终端输出，并单独保留滚动位置及文本、类别筛选。按右方向键进入紧邻的 Tasks，向左返回 Logs；Esc 或再次按右键返回触发日志入口时的列。中途在日志与任务间移动不会改写这个返回位置。关闭视图不清除日志、不删除任务。

`/tasks` 立即打开任务列，再异步读取当前客户端的 `ssmcp.exec.list`。列表显示真实任务 ID、命令、运行状态、退出码和耗时；Enter 查看工作目录及输入、输出游标等元数据，Esc 返回列表。任务选择按 ID 保存，列表刷新或重新排序不会把选择悄悄移到另一任务；已移除的任务会明确提示。列表为空、尚未加载和刷新失败各有独立提示。预览模式使用标明为虚构的数据，不访问运行中的任务。

任务面板只读取列表和状态，不读取任务的完整 stdout/stderr，也不会启动命令。取消仍需输入 `/tasks cancel JOB_ID`；取消动作与切换、关闭面板分开。任务列表、日志和对话的显示状态都保存在客户端，服务端沿用通用的连接、授权和工具转发接口。

## 原生消息与收件人

输入 `/message` 或 `/recipients` 打开原生收件人选择器。选择器使用当前服务器返回的账号与稳定 ID；同名对象仍分别显示，显示名称不会替代身份。面板打开时会自动刷新后来上线或离线的对象，关闭后停止仅为收件人列表进行的轮询。

| 收件类型 | 谁能收取 |
| --- | --- |
| Account inbox | 所选设备上的该账号，在后续合格工具调用中收取；这不代表已识别某个模型线程 |
| Verified agent | 所选设备上通过实际成员凭据绑定的那个 agent |

选中对象后按 Enter 返回输入框，草稿保持原样，再按 Enter 才发送。开头的 `@名称` 可引导打开选择器，仍须明确选择。同名文件可写成 `@./example.txt` 或 `@"example.txt"` 来明确引用。恢复草稿时会保留原生目标，再次发送前须核对账号和收件人可用性。[选择与正文处理](../src/client/terminal/conversation/messaging.rs)

原生消息正文按纯文本发送。`@path`、`$skill` 和 `!command` 保留为字面正文。普通 Enter 可能把匹配所选收件人的开头 `@名称` 当作寻址前缀。`/send` 保留命令后正文中的这个前缀。长粘贴保存的文件仍在本机，发送路径文字不会传输文件。

在 `/sessions` 中用空格选择精确收件人，按 `g` 编辑组消息。勾选设备收件箱只选中该账号的收件箱。组发范围最多 32 个明确目标，`/group status` 可查看逐个回执。[终端组发](session-groups.md)

| 界面回执 | 已发生的事实 |
| --- | --- |
| Submitting | 本机已发起发送，等待服务回执；原草稿仍保留 |
| Queued by service | 服务已接收入队，等待目标的后续调用 |
| Included in recipient tool response | 正文已经放进目标工具响应；不表示已阅读或执行 |
| Acknowledged by recipient | 目标显式确认收到 |
| Delivery failed or expired | 发送未确认或回执已失效，先核对状态再决定是否重试 |

`/send 正文` 使用最近明确选择的组、原生收件人或本地目标。原生目标须仍匹配精确连接、账号与线程。选择本地目标时，`Next caller` 对应主线程监听器，指定成员需要同线程监听器。本地发送会准备文件与技能引用。原生单发或组发失败会保留原目标和草稿，且不会改投本地监听器。

只有匹配的接受回执才会清空上下文仍一致的草稿，正文、目标、修订或光标有变化时保留当前输入。组发需要每个目标都已入队或收到更后续的确认，部分成功时保留草稿。组发重试沿用组和子请求 ID。目标已获接受回执时，后续状态查询出错也不会重发。回执只描述交付，agent 执行结果由接收方另行报告。[原生消息协议](thread-messaging.md) · [监听与发送选路](force-send-listener.md)

## 本地命令

输入框中的 `/` 用于本地命令，日志搜索使用 `/filter 文本`。命令候选展开时，↑/↓ 选择，Tab 补全，Enter 执行选中的命令，Esc 收起候选并保留文字；选择候选不会取回排队要求或切换输入历史。

| 命令 | 作用 |
| --- | --- |
| `/help` | 打开可滚动的完整快捷键与本地命令帮助 |
| `/exit` / `/quit` | 正常关闭当前前端并恢复终端；本地 attach 只离开观察会话 |
| `/login` / `/login copy` / `/login link` | 开始 GitHub 客户端授权：打开浏览器 / 复制公开授权链接 / 显示链接 |
| `/diff [single\|split]` | 选择单列或双列 diff；省略参数时显示当前模式 |
| `/reload` / `/restart` | 请求重新启动客户端；两个命令等价 |
| `/queue` | 查看、取回或取消待交付要求 |
| `/message [名称或 ID]` / `/recipients [名称或 ID]` | 打开原生收件人选择器；仍须明确选择后再发送 |
| `/sessions` / `/group list` | 查看连接和线程，或列出当前精确组发目标 |
| `/send 正文` | 向当前所选组、原生收件人或本地监听程序发送 |
| `/paste-retry` / `/paste-retry discard` | 重试保存失败的长粘贴，或明确丢弃该保留原文 |
| `/tasks` / `/tasks cancel JOB_ID` | 打开 Logs 右侧的任务列；显式 `cancel JOB_ID` 才取消指定任务 |
| `/audit` | 浏览 `code_mode` 源码与调用记录 |
| `/logs` | 打开日志列；Esc 或鼠标右键返回先前的列 |
| `/raw` | 切换同一日志面板 |
| `/new [chat\|agents\|audit\|queue\|questions\|history]` | 在当前列右侧新建视图；省略类型时新建对话列 |
| `/close` | 关闭当前视图，保留会话与草稿 |
| `/filter 文本` | 按文本筛选当前保留的日志 |
| `/filter --all` / `--commands` / `--errors` | 切换日志类别；保留已有文本条件 |
| `/theme [名称]` | 指定主题；省略名称时切换到下一预设 |
| `/sidebar` | 展开或收起侧栏 |
| `/clear` | 清空可见日志与屏幕回看缓存，不删除输出归档或会话文件 |
| `/resume` | 列出本工作区的已保存会话，选中后按 Enter 继续该文本历史 |
| `/pi` / `/cc` / `/codex` | 查看对应后端的实际工具目录；`TOOL` 查看 schema，`TOOL {...}` 显式调用 |

`/exit` 与 `/quit` 只接受无参数形式。输入 `/exit extra` 等带参数形式时，界面显示 `Use /exit or /quit without arguments`，保留当前输入并继续运行。合法退出沿正常清理路径关闭前端，恢复终端输入模式、光标、鼠标与括号粘贴状态；预览也使用同一退出入口。本地 attach 的退出只结束观察会话。[本地命令处理](../src/client/terminal/conversation.rs) · [预览清理路径](../src/client/ui_command.rs)

`/reload` 重新启动客户端，F5 和原子 UI 动作 `reload` 只重载显示配置。重启前，客户端尝试取回仍在服务器排队的要求，并通过私有、单次读取的交接文件保留草稿与队列。新连接只继续发布确定尚未交付且目标仍适用的要求；目标成员失效或交付不确定时等待显式复核，不自动重发。

若重启在取回要求之后中止，已获服务器确认、仍属于当前连接的要求恢复排队，沿用原消息 ID 和最新版本。迟到的取回确认也会恢复要求；确认尚未到达时继续等待，用户主动进入的编辑状态保持原样。

重载仍遵守已有会话生命周期，连接所属任务会随旧连接结束；重连后需要确认新的客户端 ID。交接文件用于这次受控重启，不是跨任意退出的永久会话存档。[重载与恢复](session-control.md)

帮助页可用 ↑/↓、滚轮、PgUp/PgDn、Home/End 滚动。按 F1 或 Esc 关闭。`/resume` 使用版本化的 `ssmcp.conversation.v1` JSONL，恢复显示文本并将后续消息写入所选会话；它不会重发旧要求、执行旧工具调用或恢复外部模型会话与凭据。[历史位置、容量与格式](conversation-history.md)

`/pi`、`/cc`、`/codex` 以及后接 `tools [OFFSET]` 只列出当前已注册工具；后接工具名只查看原始描述与参数 schema。显式追加 JSON 对象才异步调用原生工具，沿用其授权要求。`cc` 对应 Claude 的规范工具命名空间；工具缺失时直接报错。Pi RPC 的 `get_commands` 不等于内部 LSP 或模型工具清单，界面不会虚构这类接口。[目录与显式调用示例](client-input.md)

## 本地历史标题与摘要

`/resume` 列出未归档的本地会话。`/archived` 查看已归档会话，Enter 恢复并继续所选项。内容区用 ↑/↓ 移动、PgUp/PgDn 翻页，滚轮也可移动选择。鼠标左键选择条目，点摘要行仍选中同一会话。列表按窗口宽度缩略显示摘要。

用 `ssmcp.ui.history` 的 `list` 或 `metadata` 查找精确会话 ID。用原生工具 `ssmcp.ui.history_update` 修改标题或摘要，分别最多 256 和 2048 个 UTF-8 字节。至少提供一项，省略的字段保持原值。空字符串清除该字段，清除标题后恢复自动标题。仅空白、控制字符和 `null` 均会被拒绝。

```javascript
return await call(clientId, "ssmcp.ui.history_update", {
  id: "conversation_0123456789abcdef0123456789abcdef",
  title: "示例检查",
  summary: "核对了测试项目的输入流程。"
});
```

描述保存在工作区对应的私有历史元数据中，更新时保持 JSONL 正文。其他客户端持有写入租约的会话须等租约释放。[历史格式和读取接口](conversation-history.md)

## 登录与 diff 显示

`/login` 通过已配置服务器的客户端授权接口取得公开登录链接，打开浏览器后完成 GitHub 登录与授权。`/login copy` 把同一流程的公开链接复制到剪贴板，`/login link` 在对话中显示链接。授权等待期间输入框仍可使用；重复命令复用尚未结束的授权流程。凭据由现有账号模块校验并保存，终端只显示公开链接、确认码和结果。登录成功不会重启当前连接；按结果提示在需要时重新连接。

默认 diff 使用单列，按统一补丁顺序显示上下文、删除与新增行。`/diff split` 以旧内容在左、新内容在右的双列方式显示展开的 diff；`/diff single` 切回单列。宽度不足时自动使用单列，窗口恢复后继续按所选模式显示。该选项只改变主对话的展示，原始日志仍保留原文。

## 本地消息与回答问题

主对话用 You、Assistant 或消息提供的作者名标识正文，省去日志前缀、长请求 ID 和 Progress/Answer 分类。文本按当前窗口宽度排版，消息之间留空行。

带本地会话归属的任务输出和回复按当前所选历史显示。旧任务的延迟输出仍可在 Logs 与 Tasks 的保留范围内查阅。普通原生事件没有会话归属时只进入全局 Logs。

已验证线程首次发起原生调用时，客户端按账号、连接和线程绑定当时的本地历史。切换历史后，该线程的 `ssmcp.ui.message` 新回复仍写入原会话。原会话未选中时，写入成功会返回 `history_saved: true` 和 `shown: false`，对话和 `/copy` 继续使用所选历史。原写入资格已释放、会话已归档或租约由其他进程持有时，新写入报错。

幂等重试命中仍保留的回执时会返回 `duplicate: true` 和原回执中的消息 ID、保存与显示状态。`shown` 记录的是原提交时的显示状态。

`attach` 连接到提供会话归属的后台时，按后台所选历史和事件归属显示对话。该流后续缺少选择字段时沿用已知选择，缺少归属的事件留在 Logs。旧后台没有归属信息时保留共享对话视图。先前混在一起的记录按原样供查看，切换到本地历史时仍归共享视图。

迟到的原生图片沿用调用发起时的会话归属，预览附在该次输出自己的占位记录上。

`/reload` 成功打开原会话时，对话正文与保存的 JSONL 对齐。快照之后保存的消息也会在 Logs 的保留范围内显示。JSONL 无法打开时，快照中属于原选中会话的预览仍可只读查看，失败原因会显示在界面和 Logs 中。

Agent 面向本机用户的进度使用 `ssmcp.ui.message`；普通问题用 `ssmcp.ui.ask` 创建后立即返回。按 Shift+↑ 优先进入未回答问题，输入选项编号或自由文本后按 Enter 保存。回答编辑器与主草稿分开，Esc 回到问题列表，再按 Esc 回到会话。普通问题可以用 Delete 跳过；结束申请必须作出独立决定。

回答之后，问题保留在列表中并显示交付状态。绑定了实际提问线程的答案先显示 `Answer queued`，等待该线程的下一次合格调用；`Included in requester tool response` 表示已放入回复，不表示模型读过。未绑定线程的旧式问题提示提问方手动读取；原连接失效的答案不会转给新线程。只读查询本身不会消耗答案。

当前公共接口不允许 agent 代替本机用户回答问题或批准结束。界面保留后端确认的真实来源与修订号；历史 Agent 来源的记录不会被改写成 Human，普通文本中的名字或“yes”也不能伪造来源。尚未提交的本地评论仍显示为 `Your unsent draft · not delivered`，不会冒充已经发送的答案。[问答与交付协议](finish-and-questions.md#异步问题与答案)

### 明确决定是否结束

已验证的提问线程通过 `ui.wait(intent: "finish")` 发起结束等待。存在尚未交付的普通用户输入时，服务先交付输入，不创建新的结束申请；否则，同一次等待会创建或续接 No/Yes 问题。结束申请默认选中 **No**，但尚未提交时仍然是“未决定”。← 选择 No，→ 选择 Yes，或点击对应选项；之后按 Enter 才记录决定。Tab 进入独立的可选评论。评论里输入 `yes` 不会改变选项，Delete 也不会批准或跳过结束申请。

No 表示继续工作；只有本机真实用户的 Yes 才能让原提问线程按精确申请和修订号消费一次批准。服务器最终还要复核权限、原连接、线程及相关输入，才返回 `decided`、`may_finish: true`；仅看到问题已批准，或再次读取已完成记录，都不是新的许可。新的相关用户输入、取消、断线或重连、撤权和绑定失效不会批准结束。用户自己的决定显示为 You，其他 agent 不能代用户批准 Yes。

普通进度、answer 和 reply 消息仍可提交；它们本身不授予结束许可。当前协议要求调用者结束前读取真实用户决定，但不声称能拦截任意外部模型的最终发言或强制它继续运行。

真实人工等待进行时，输入区显示红色 `! 等待你的输入 · 剩余 00:13 · /wait`；多个 episode 显示数量，时间取最短实际剩余值并向上取整到秒。普通计时器和后台任务不显示人工等待红标。进入休眠画面后背景压暗，最后一行保留红色粗体倒计时；具体错误、补全和浏览提示仍有自己的显示优先级。

完整的鼠标点击只唤醒画面，不提交 No/Yes，也不伪造答案；键盘唤醒后继续处理该按键。只要真实等待仍在进行，唤醒后的输入区继续显示倒计时。超时可以由同一提问线程带返回的 `finish_request_id` 续等，不批准结束；同一 episode 的短时重试不会反复遮住已唤醒的画面。普通异步问题本身不触发休眠。`/wait` 查看与取消入口保持不变，结束结果的近期提示为中性的“本轮结束决策已返回”。[结束申请与一次性回执](finish-and-questions.md#申请结束) · [等待与倒计时](user-wait.md)

问题和决定保存在当前客户端进程内；最多 32 个待答问题、64 条保留记录，共用 256 KiB。进程重启或 `/reload` 不恢复这份待答状态；断线后的旧记录可以留作本机查看，但不会自动改投新连接。观察模式的 attach 不代答。[UI 实现](../src/client/terminal/conversation/questions.rs) · [等待遮罩](../src/client/terminal/sleep.rs)

## Agent 面板与成员身份

Agent 页分别显示已登记成员和观测到的调用方，标题分别统计 `members` 与 `callers`。`Next caller` 是后续输入的收件选项，不计为在线成员。侧栏跟随当前选中项，显示它的身份、状态及相关消息；关闭侧栏后，足够宽的主区域会并排显示详情，较窄窗口保留完整列表导航。

已登记成员来自服务器协作成员表，展示名称、角色和成员报告的状态。选择记录并按 Enter，即可将后续输入定向到该成员；名称允许重复，同名成员显示各自的 ID 后缀，目标始终以完整 `agent_id` 区分。成员离线后保留原收件人选择并显示离线提示，需要用户重新选定有效成员。

观测到的调用方按调用来源、名称和模型归并。重复工具调用更新同一行的调用数与运行状态，模型名称也显示在列表中；无法识别身份的调用保留独立记录。选择调用方并按 Enter 会打开其代表调用的审计详情，原来的消息目标、草稿与光标保持不变。显示名称与模型只用于归类，不能作为可接收定向要求的成员凭证。

新活动和列表重新排序按稳定的调用方键或成员 ID 保留选择。鼠标点击按上一帧实际显示的条目定位，避免新调用插入时选中相邻对象。[Agent 页实现](../src/client/terminal/conversation.rs)与[调用方分组](../src/client/activity.rs)

用 `ssmcp.ui.get` 读取 `/identity` 可查看当前客户端的操作系统 `process_id` 与中继连接 `client_id`。重连分配新的 `client_id`，它也可作为当前逻辑 `session_id`。`/steering/agents` 列出的 `agent_id` 是逻辑线程 ID，在协作 API 中也称 `thread_id`。

`collab.join` 和经过认证的 `collab.update` 可用 `summary` 填写在线角色短摘要。它是 `role` 的别名，单次只传其中一个字段，最多 128 个 UTF-8 字节。更新时传 `summary: ""` 清除，省略该字段保留原值。摘要随在线成员租约展示，可在会话页查看。

成员由 `collab.join` 显式加入，租约到期或退出后不再是有效成员。`status` 由成员自行报告，不等于某个模型正在生成内容，也不等于后台命令进程的状态。`agents.*` 用于项目 AGENTS 指令文件，在线成员由 `collab.*` 管理。[成员、邮箱与租约](agent-collaboration.md)

要接收定向要求，agent 在每次 `code_mode` 开始时绑定已经登记的身份：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
await collab.bind(clientId, {
  agent_id: "AGENT_ID_FROM_JOIN",
  token: "PRIVATE_TOKEN_FROM_JOIN"
});
return await exec.list(clientId);
```

绑定只作用于本次编排，同一客户端不能在一次编排中切换到另一成员。服务器在交付前重新检查授权与成员凭证；名称、角色和 `controller.model` 只作展示，不决定接收者。`Next caller` 可以接收未绑定成员的合格调用方，定向要求只进入经过验证的对应成员响应。

要求队列与 `collab` 邮箱分别管理。成员之间的 JSON 消息通过 `collab.send/receive/ack` 交换；输入框中的用户要求由 `ssmcp.ui.queue` 管理，不使用邮箱的 `ack`。

客户端和服务器现在只接受协议 v7，要求队列、成员快照、源码审计和控制者元数据使用同一协议。版本不匹配会明确拒绝连接，不再逐级尝试 v6/v5/v4；更新两端后重新连接。

## 配置文件

配置路径依次选择：

1. `--ui-config FILE`。
2. `SSMCP_UI_CONFIG` 环境变量。
3. `$XDG_CONFIG_HOME/ssmcp/config.toml`。
4. `~/.config/ssmcp/config.toml`，Windows 使用用户目录下的 `.config/ssmcp/config.toml`。

命令行显示选项覆盖对应的文件设置。默认路径不存在时使用内置默认值；显式指定的文件不存在、格式损坏或参数非法时返回错误。修改文件后按 F5 重载，或调用 `ssmcp.ui.action` 的 `reload` 动作。

同一个 TOML 文件可以同时包含 `[client]` 和 `[ui]`。显示配置保存会保留服务器设置；服务器地址与可信公钥的选择规则见[中转服务器与信任](client-server.md)。`SSMCP_CLIENT_CONFIG` 与 `SSMCP_UI_CONFIG` 分别选择客户端和显示配置，不能互相替代。

下面是完整的显示配置示例，可直接作为 TOML 文件使用：

```toml
[ui]
color = true
dashboard = true
log_format = "pretty"
theme = "dark"
layout = "chat"
log_height_percent = 60
sidebar = false
sidebar_width = 37
panels = ["agents", "queue", "controller", "tasks", "progress", "git", "history"]
hide_empty = true
timestamps = false
show_request_id = true
show_raw_json = false
max_inline_output = 8
activity_interval_secs = 5

[ui.pet]
preset = "cat"
text = ""

[ui.colors]
background = ""
foreground = ""
muted = ""
accent = ""
success = ""
warning = ""
error = ""
heading = ""
```

空字符串表示继承所选主题的颜色。可以只写需要修改的字段，其余使用默认值。F8 和 `save` 动作保存当前显示设置；`set` 调用使用 `persist:true` 时也会写入本机配置文件。

已有配置中的 `sidebar = true` 会继续生效。想让下次打开时也收起侧栏，可按 F3 后用 F8 保存，或将配置中的 `sidebar` 改为 `false`。

`dashboard` 与 `log_format` 是启动设置，不能通过 `ssmcp.ui.set` 修改；内置界面运行时，配置重载也不能切换这两项。修改文件后重启客户端即可应用。主题、颜色、布局和面板设置可在运行时调整。

### 侧栏 ASCII 宠物

`[ui.pet]` 控制侧栏底部的小图案。`preset` 接受 `cat`（默认）、`robot`、`custom` 和 `off`；选择 `off` 即关闭。图案随主题颜色显示，只有侧栏有足够空间容纳整幅图案并保留状态区时才出现，窄小窗口会将其隐藏。

自定义图案使用 TOML 多行字面量字符串，空格与反斜杠按原样保存：

```toml
[ui.pet]
preset = "custom"
text = '''
 /\_/\
( o.o )
 > ^ <
'''
```

`custom` 必须提供非空图案；`text` 只接受可打印 ASCII 字符与换行，最多 1024 字节、8 行，每行最多 80 列。制表符、回车、ANSI 控制序列和其他控制字符会被拒绝。它是静态文字配置，不执行命令或加载外部图片。修改后按 F5 重载；通过原子 UI 接口修改 `pet` 后也可用 F8 或 `persist:true` 保存。[配置校验](../src/client/ui/pet.rs) · [侧栏排版](../src/client/terminal/guide/pet.rs)

## 布局与状态面板

| 设置 | 默认值 | 可选值与作用 |
| --- | --- | --- |
| `layout` | `"chat"` | `chat` 显示事件流与底部输入框；`split` 上下显示日志与源码审计；`audit` 展开审计区；`logs` 展开日志 |
| `log_height_percent` | `60` | `10`–`80`，控制 `split` 布局中日志的高度占比 |
| `sidebar` | `false` | 默认收起状态侧栏；F3 或 `/sidebar` 切换显示 |
| `sidebar_width` | `37` | `20`–`60` 列；可用空间不足时收起侧栏 |
| `panels` | 七种面板 | 按数组顺序排列，移除名称即可隐藏对应面板 |
| `hide_empty` | `true` | 收起没有内容的状态面板；成员和队列面板保留入口提示 |
| `dashboard` | `true` | 是否启用终端合成界面；关闭时使用行式输出 |

`panels` 可以是空数组，名称不能重复。面板名称与内容：

| 名称 | 内容 |
| --- | --- |
| `agents` | 当前连接中的登记成员、成员报告的状态与最近普通调用 |
| `queue` | 本地用户要求与交付状态摘要 |
| `controller` | 当前或上次调用方声明的应用、模型信息 |
| `tasks` | 正在运行的任务、命令与耗时 |
| `progress` | 原生工具实际报告的进度 |
| `git` | 当前工作区的分支和改动摘要 |
| `history` | 最近提交 |

`agents` 和 `queue` 只在 `chat` 布局中显示；配置了这两个面板时，即使为空也保留计数和入口提示。`hide_empty` 作用于其余状态面板。

例如，展开源码审计，同时保留任务和工作区状态：

```toml
[ui]
layout = "audit"
sidebar = true
sidebar_width = 26
panels = ["tasks", "git"]
hide_empty = true
```

窗口变化后，布局根据可用行列重新计算。Git 状态来自本地读取；控制者的模型名称来自调用方声明；进度百分比来自工具实际提供的进度与总量。

默认 `chat` 中 F2 切换成员、对话、审计、队列、问题、历史、日志与任务内容，仍保留输入框。`layout` 则决定整个显示方式；使用配置或 `ssmcp.ui.set` 选择 `split`、`audit`、`logs`。这些专用布局保留此前的浏览方式，F2 可以继续切换布局并回到 `chat`。

前台输入用于用户要求和本地命令，不会写入隐藏的 PTY。需要交互终端时，在另一个终端运行 `ssmcp -i`；自定义前端可以直接使用 `terminal.*`。本地 `attach` 用于观察后台会话，目前不转发其前端输入框的要求队列；可通过后台连接的 `ssmcp.ui.queue` 原子接口管理要求。[交互终端与后台输入](terminal-control.md)

## 源码审计与运行状态

审计区按运行 ID 保留调用方提交的 `code_mode.code`，源码经过现有审计脱敏处理。使用 `/audit` 或 Ctrl-O 打开审计，内容区中的 ↑/↓ 选择运行记录，Enter 展开带行号与语法颜色的源码；详情中继续用 ↑/↓ 滚动，Esc 返回列表。Tab 切回输入框后，方向键用于文字编辑和历史浏览。

[源码审计详情：行号与语法颜色](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/client-audit.png`）

*由实际渲染器回放的审计示例；示例源码不会执行。*

审计信息分别记录三个层次的状态：

| 状态 | 含义 | 后续查看 |
| --- | --- | --- |
| `code_mode` 运行 | 这次 JavaScript 编排正在运行、已经返回、失败或超时 | 审计运行 ID、耗时与错误 |
| 原生调用 | 编排发出的某个工具请求及其返回结果 | 调用 ID、工具名、耗时及实际返回的退出码或任务 ID |
| 后台命令 | 设备上的命令进程仍在执行，或已经退出 | `exec.status/read/wait` 和日志中的真实退出状态 |

例如，`exec.start` 返回任务 ID 后，`code_mode` 可以正常结束，命令仍在后台执行。要判断命令是否成功，读取对应任务的 `running` 和 `exit_code`。日志用 `python3 -c · 83 lines` 这类中性标签识别脚本，再分别显示 stdout、stderr、退出码和耗时；`--verbose` 可展开脚本正文。

源码审计使用当前唯一支持的 **协议 v7**。服务器将源码发送给本次编排实际访问的客户端，每次运行按 ID 合并；设备列表中的其他客户端不会收到这份源码。协议不匹配时连接被拒绝，不会退回缺少审计的旧连接。

界面最多保留 **128 次运行、合计 2 MiB** 的审计浏览数据，单段源码显示上限为 **64 KiB**。达到容量后淘汰最旧的运行记录，截断状态随快照返回。屏幕回看与长期输出保存各有容量边界；需要读取归档时使用审计事件返回的 artifact 和 `outputs.read`。

## 日志滚动与筛选

日志中的缓冲工具输出默认保留最多 8 行摘要，取开头与末尾并注明省略；`max_inline_output` 调整这一上限。对话页从已脱敏的记录中显示最多两行结果预览，展开后查看当前保留的正文。用户要求、Agent 消息和回答保持会话正文显示，图片使用摘要或预览，不逐字段打印 base64。显示长度的调整不会改写原生工具返回值。

默认 `chat` 布局的活动页按每次发布显示一条事件。Tab 切到内容区，↑/↓ 选择，Enter 切换工具折叠；鼠标点击同样可选择并切换，←/→ 用于切换相邻列。用户与 Agent 正文保持展开，工具和图片摘要按需打开。事件视图最多保留 128 条、合计 2 MiB 的展示正文，与审计浏览缓存独立。展开只显示本地仍保留的正文，不会找回先前被摘要省略的输出，也不重新执行工具；完整输出需通过相应任务游标或输出归档读取。

滚轮和 PgUp/PgDn 直接滚动当前内容，无需先切换焦点。Ctrl-Home/Ctrl-End 到最早保留日志或最新输出。浏览记录与编辑输入分别处理；输入框中的方向键先按实际显示行移动光标，包括自动换行；到达第一或最后一行后再取回排队内容或浏览历史。内容区中的方向键用于浏览。F7 冻结当前画面并释放鼠标，让终端原生选择与复制；复制通常使用终端的 Ctrl-Shift-C，F7 或 Esc 恢复界面。支持的终端可直接按住 Shift 拖选。

输入 `/filter 文本` 并提交，应用不区分大小写的搜索。搜索使用普通文本包含匹配，保留命中命令的输出和结果上下文。单独输入 `/filter` 清除文本；`/filter --all`、`/filter --commands`、`/filter --errors` 选择类别，F9 在类别间循环：

| 类别 | 显示内容 |
| --- | --- |
| `all` | 全部保留的日志 |
| `commands` | 命令、输出与结果相关记录 |
| `errors` | 包含错误、警告或 stderr 等诊断信息的记录 |

日志筛选和其他日志布局沿用原有屏幕历史显示，不使用活动页的逐事件展开。筛选在当前屏幕历史范围内进行，不改变任务执行和审计归档。文本上限为 256 个 UTF-8 字节。无界面前端可以按游标读取事件，再自行筛选。

## 主题与颜色

[浅色主题的客户端](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/client-ui-light.png`）

*实际渲染器的固定示例，展示浅色主题、输入框与可折叠侧栏。*

| 主题 | 风格 |
| --- | --- |
| `dark` | 默认主题，深色背景、浅色正文与蓝色强调；成功、警告和错误使用独立语义色 |
| `codex-dark` | 冷色强调的深色主题 |
| `nord` | 冷色背景与柔和强调色 |
| `solarized` | Solarized 深色配色 |
| `light` | 浅色背景与深色文字 |
| `minimal` | 跟随终端底色，使用基础语义色 |

默认主题的颜色用于分清结构与状态：蓝色标记调用与交互，绿色标记成功，黄色标记警告，红色标记错误；标签文字仍保留含义。F4 和不带名称的 `/theme` 按表中顺序循环。`/theme light` 等命令直接选择预设。

`[ui.colors]` 按语义覆盖主题，支持以下值：

| 写法 | 含义 |
| --- | --- |
| `#RRGGBB` | 六位十六进制真彩色，例如 `#8ab4f8` |
| `black`、`red`、`green`、`yellow`、`blue`、`magenta`、`cyan`、`white` | ANSI 基础色，`purple` 是 `magenta` 的别名 |
| `bright-` 加上述颜色名 | ANSI 高亮色，例如 `bright-blue`、`bright-purple` |
| `gray`、`grey` | ANSI 高亮黑色 |
| `default` | 终端自己的默认前景或背景色 |
| `""` | 继承当前主题的对应颜色 |

颜色名使用小写，十六进制数字支持大小写。十六进制颜色通过终端真彩色序列输出，最终效果取决于终端的颜色支持。

```toml
[ui]
theme = "minimal"

[ui.colors]
background = "#101214"
foreground = "#e8eaed"
muted = "#9aa0a6"
accent = "#8ab4f8"
success = "#81c995"
warning = "#fdd663"
error = "#f28b82"
heading = "#e8eaed"
```

要沿用终端自己的背景与前景色，可以设置：

```toml
[ui.colors]
background = "default"
foreground = "default"
```

`NO_COLOR`、`--no-color`、`TERM=dumb` 或重定向日志会关闭日志颜色。终端内应用自行输出的颜色由应用控制。JSONL 与后台日志使用结构化事件，主题不会写入原生调用结果。

## 键盘与鼠标操作

下面的操作用于默认 `chat` 布局。F1 显示当前帮助；Tab 或 Shift-Tab 在输入框与内容区之间切换焦点。输入框出现斜杠命令补全时，Tab 优先接受补全，Shift-Tab 仍可切换焦点。

底栏根据当前焦点显示快捷键：空输入提示粘贴、`@` 文件和 `$` 技能，已有草稿提示发送与换行，补全菜单提示选择、插入和关闭；进入对话、审计或队列内容区后，提示随该区域的操作变化。窗口变窄时自动选择较短的提示，状态或错误通知优先显示，完整按键仍可用 F1 或 `/help` 查看。[上下文提示实现](../src/client/terminal/conversation/hints.rs)

| 操作 | 作用 |
| --- | --- |
| 鼠标滚轮、PgUp / PgDn | 直接滚动当前内容 |
| Ctrl-Home / Ctrl-End | 到当前对话或日志的最早保留内容 / 最新输出 |
| Tab / Shift-Tab | 在输入框与内容区之间切换焦点；Tab 优先接受显示中的命令补全 |
| 输入框 ↑ / ↓ | 先按实际显示行移动光标，到边界后浏览输入历史。空输入框按 ↑ 优先尝试取回最新待交付项 |
| Alt+← / Alt+→ | 切换相邻画布列，保留草稿与各列位置 |
| 内容区 ← / → | 切换相邻画布列 |
| 输入框 ← / →、Home / End | 移动光标或到当前逻辑行起止位置；整份草稿边缘的左右键切列 |
| 鼠标右键 | 打开日志列，或返回打开日志前的列 |
| 输入框 Enter | 提交新要求、重新提交编辑内容，或执行本地命令 |
| Ctrl-J、Shift-Enter、反斜杠后按 Enter | 换行；Shift-Enter 需要终端能区分该按键 |
| 内容区 ↑ / ↓ | 选择活动事件，或浏览审计、成员、队列、问题、历史与任务记录 |
| 对话内容区 Enter | 切换所选工具事件的展开状态；点击工具摘要也可切换 |
| 内容区 Home / End | 到当前内容的起止位置 |
| 审计区 Enter / Esc | 展开当前记录 / 返回列表 |
| 任务区 Enter / Esc | 查看任务详情 / 返回任务列表；列表中 Esc 回到输入框 |
| 成员区 Enter | 登记成员设为接收目标；普通调用展开其源码与结果审计 |
| 队列区 Enter / Delete | 取回编辑 / 取消待交付项 |
| 队列区 I / Esc | 只读查看详情 / 返回列表 |
| 队列区 R | 对需复核项显式改为 Next caller 并编辑；对已交付项复制为新草稿 |
| Shift-↑ | 打开待答问题；重复按键切换待答项 |
| 问题回答 Enter / Esc | 保存答案 / 返回问题列表；列表中 Delete 跳过 |
| 历史区 Enter | 恢复所选本地文本历史，不重放工具或要求 |
| `/filter 文本` | 筛选日志 |
| Ctrl-O | 在活动与源码审计间切换 |
| Esc | 关闭帮助或详情、返回输入；普通复制草稿可恢复暂存的原稿，不中断外部模型 |
| F1 | 打开或关闭完整帮助；页内方向键、滚轮、PgUp/PgDn、Home/End 滚动 |
| F2 | 在成员、对话、源码审计、队列、问题、历史、日志与任务之间切换 |
| F3 | 显示或隐藏侧栏 |
| F4 | 切换内置主题 |
| F5 | 从配置文件重载显示设置 |
| F7 | 冻结画面并开启终端鼠标选择；再次 F7 或 Esc 返回 |
| Shift+拖选 | 在支持的终端中直接进行原生文本选择 |
| F8 | 保存当前显示设置 |
| F9 | 在 `all`、`commands`、`errors` 日志类别间切换 |

日志回看使用有界屏幕缓存，命令输出通过任务游标或输出归档读取。

## 原子 UI 接口

客户端提供以下原生工具，和文件、终端、任务接口一样由已授权的连接调用：

| 工具 | 参数 | 结果与作用 |
| --- | --- | --- |
| `ssmcp.ui.get` | `{}` 或 `{path, offset?, limit?}` | 获取完整快照，或按 JSON Pointer 读取字段、分页读取数组与字符串 |
| `ssmcp.ui.set` | `{patch, persist?}` | 合并并验证配置改动，应用后返回当前状态；`persist` 默认 `false` |
| `ssmcp.ui.action` | `{action}` | 执行一个界面动作并返回当前状态 |
| `ssmcp.ui.filter` | `{text?, level?}` | 设置当前日志筛选；省略字段时保留该项 |
| `ssmcp.ui.queue` | `{action, ...}` | 读取或修改本机用户要求队列，见下方动作表 |
| `ssmcp.ui.message` | `{text}` | 向本机用户显示 Markdown 进度或结果，记录为 Agent 消息 |
| `ssmcp.ui.ask` | `{question, title?, options?, idempotency_key?}` | 非阻塞创建本地问题，返回问题 ID |
| `ssmcp.ui.questions` | `{id}` 或 `{state?, offset?, limit?}` | 读取问题与已保存答案，不消费记录 |
| `ssmcp.ui.history` | `{action:"list"/"metadata"/"read"/"search", ...}` | 只读浏览会话元数据、分页正文和关键词命中，见 [历史格式](conversation-history.md) |
| `ssmcp.ui.history_update` | `{id, title?, summary?}` | 更新一个已保存会话的描述，见 [本地历史标题与摘要](#本地历史标题与摘要) |
| `ssmcp.ui.harness` | `{action:"tools", provider, offset?, limit?}` 或 `{action:"inspect", provider, tool}` | 只读发现实际后端目录与原始 schema，不调用模型 |

`patch` 使用工具 schema 列出的可动态修改字段，`colors` 是其嵌套对象。例如 `{theme:"nord", colors:{accent:"#8ab4f8"}}`。颜色设为 `""` 恢复预设；`null` 不是有效配置值。将 `panels` 设为新的数组可以整体替换面板顺序。

空参数 `get` 返回 `schema_version`、`config`、`config_path`、`display`、`activity`、`steering` 和 `questions`。内置界面活动时，`display` 提供内容区尺寸、事件状态、鼠标选择模式、审计记录、筛选与输入焦点；`activity` 提供经过脱敏的控制者、任务、进度、Git 和诊断摘要；`steering` 提供队列、连接支持状态和实际成员快照；`questions` 提供本地提问状态。界面应根据实际返回状态绘制，完整工具 schema 可通过发现接口读取。

在 `code_mode` 中先定位设备和工具：

```javascript
const matches = clients.filter(c => c.name === "dev");
if (matches.length !== 1) throw new Error("Select a unique client ID from list");
const client = matches[0];

return await tools.search(client.id, {
  query: "ssmcp.ui",
  include_schemas: true
});
```

读取输入与浏览状态：

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");

return await call(client.id, "ssmcp.ui.get", {
  path: "/display/conversation"
});
```

自定义界面应按实际需要轮询字段，例如 `/config`、`/display/panes`、`/display/filter` 或 `/steering/connected`。`path` 使用非空 JSON Pointer，返回 `{schema_version:1, path, value}`；键名中的 `~` 和 `/` 分别写成 `~0` 和 `~1`。这样读取输入状态时，不会顺带返回不断增长的源码审计记录。

数组与字符串支持 `offset` 和 `limit`，返回 `total`，有后续内容时提供 `next_offset`。数组偏移按元素计，字符串偏移按 Unicode 码点计；默认从 0 开始，每页分别为 32 个元素或 1024 个码点，`limit` 范围为 1–4096。对象和标量不接受分页参数。

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");

return await call(client.id, "ssmcp.ui.get", {
  path: "/steering/queue",
  offset: 0,
  limit: 4
});
```

投影响应最多 8 KiB；超过时返回明确错误，要求缩小字段或页大小，不会静默截断对象或改成输出附件。单个队列项很大时，可以读取 `/steering/queue/0/text` 并对正文分页。`{}` 保留完整快照的兼容行为，长快照仍遵守通用输出归档规则；持续刷新界面优先使用 `path`，避免为了读取状态而调用会领取要求的工作类接口。

应用颜色与布局，并保存到客户端选择的配置路径：

```javascript
const client = clients.find(c => c.name === "dev");
if (!client) throw new Error("dev is offline");

return await call(client.id, "ssmcp.ui.set", {
  patch: {
    theme: "minimal",
    layout: "chat",
    panels: ["agents", "queue", "tasks", "git"],
    colors: { background: "#101214", accent: "#8ab4f8" }
  },
  persist: true
});
```

可调用的动作：

| `action` | 作用 |
| --- | --- |
| `layout.next` | 切换布局 |
| `sidebar.toggle` | 切换侧栏可见性 |
| `theme.next` | 切换内置主题 |
| `reload` | 读取并应用配置文件 |
| `save` | 保存当前设置 |
| `logs.clear` | 清空内置界面的日志屏幕与回看缓存 |
| `audit.up` / `audit.down` | 选择上一条或下一条记录；详情模式滚动源码 |
| `audit.open` / `audit.back` | 展开当前源码 / 返回列表 |
| `logs.page-up` / `logs.page-down` | 翻阅日志 |
| `logs.end` | 返回最新日志 |

筛选示例：

```javascript
const matches = clients.filter(c => c.name === "dev");
if (matches.length !== 1) throw new Error("Select a unique client ID from list");

return await call(matches[0].id, "ssmcp.ui.filter", {
  text: "cargo",
  level: "commands"
});
```

`text:""` 清除文本条件，`level:"all"` 恢复全部类别。筛选与浏览动作需要内置界面处于活动状态；无界面时使用事件接口构建自己的浏览器。清空日志视图后，命令输出和审计归档继续遵守原有保留规则。UI 操作沿用当前客户端的访问授权，配置保存到当前设备选择的本地文件。

## 要求队列接口

`ssmcp.ui.queue` 提供独立的队列操作，自定义前端无需模拟按键：

| `action` | 参数 | 作用 |
| --- | --- | --- |
| `get` | 无 | 读取连接支持状态、成员、草稿和队列快照 |
| `submit` | `text`、可选 `target` | 创建要求；目标默认 `"next"` |
| `takeback` | `id` | 按当前版本请求取回尚未交付的要求，等待进入 `editing` |
| `edit` | `id`、`text`、可选 `target` | 重新发布已取回的要求；省略目标时保留原目标 |
| `cancel` | `id` | 请求取消尚未交付且可确认状态的要求 |
| `retarget` | `id`、可选 `target` | 显式重新选择目标并进入编辑；返回实际可编辑的 `id`；省略目标时设为 `"next"`，不自动发布 |

`target` 使用 `"next"` 或当前成员快照中的精确 `agent_id`。显式 `null` 无效，文本不能为空白，长度上限为 16 KiB UTF-8 字节。`id` 与成员 ID 必须使用实际返回值。

提交示例：

```javascript
const matches = clients.filter(c => c.name === "dev");
if (matches.length !== 1) throw new Error("Select a unique client ID from list");

return await call(matches[0].id, "ssmcp.ui.queue", {
  action: "submit",
  text: "请先检查命令的真实退出状态，再继续下一步。",
  target: "next"
});
```

变更返回 `{requested:true, id, queue}`，表示本地已接收请求，不表示服务器已经确认交付或取回。随后读取快照，检查对应 ID 的 `status`、`revision` 和 `error`：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.ui.queue", {
  action: "get"
});
```

编辑同一条要求时，先 `takeback`，确认状态已是 `editing`，再 `edit`。如果交付先完成，取回返回错误，记录成为只读。对 `uncertain` 或 `retarget-required` 记录，先复核旧请求，再 `retarget`，使用其返回的 `id` 调用 `edit`。跨房间的已发送请求使用新 ID，原未知记录继续保留；同房间重试和普通取回编辑保持原 ID。不确定旧要求是否已经交付时，不应由前端自动重试。需要再次发送已交付内容时，用 `submit` 创建新记录。

快照包含 `connected`、`supported`、`room_id`、`agents`、`draft`、`queue`、`error` 与 `reloading`。每条要求带目标、版本、状态和可用的交付信息。接口可用于没有内置 TUI 的客户端；队列发布仍需要支持协议 v7 的活动连接。

## 活动、颜色与图片

对话页的工具摘要显示工具名或命令，以及实际运行状态。成功结束的调用显示 `done`；运行中、失败、取消或超时保留对应状态，失败时显示退出码或原因。摘要下方最多显示两行已脱敏结果，失败时优先展示错误内容；`Click expand` 提示可以点击展开，选中事件后按 Enter 也能切换。展开显示当前保留的正文，用户与 Agent 的消息保持展开。

详细记录在 Logs 中查看，长输出继续通过任务游标或输出归档读取。命令、路径、代码与状态使用不同的语义颜色；关闭颜色或设置 `NO_COLOR` 时，仍可通过标签和文字区分内容。

向上滚动后，输入区会显示下方还有多少行，以及 `Ctrl+End` 返回最新输出的提示。`Ctrl+End` 恢复跟随新输出，同时保留草稿和输入焦点。`/logs` 或鼠标右键打开 Logs，`/raw` 切换同一日志列。日志打开时使用滚轮和 PgUp/PgDn 回看，F9 切换日志类别；在共享输入区输入文字会留在 Logs 中继续编辑。按 Esc 或再次按鼠标右键返回原列及阅读位置，草稿与光标保持不变。日志筛选只影响日志视图；界面缓存与完整输出归档分别保留。

重复的连接器登录警告按来源和服务端点合并并累计次数。不同连接器或新的错误继续分别显示。同一调用方的多次编排在侧栏归组；断线时未确认结束的调用显示为未知状态，连接状态和身份选择保持独立。

原生图片需要终端同时确认 Kitty 图片协议和实际字符像素尺寸。确认后，图片按真实像素比例缩放与裁剪；不支持的终端继续使用有界预览或文字说明。窗口大小改变后会重新测量字符尺寸，避免沿用旧字体几何。PNG 粘贴按二进制容器解析，内部控制字节不会触发快捷键；图片未完整到达或缺少外层结束标记时，两秒输入空闲后恢复编辑，保留原有草稿。

侧栏二维码只在完整图片能放下时以方形像素显示，并保留完整链接。网关的公开教程可用时进入中英文指南；旧网关未提供教程时回到门户首页。二维码生成和普通图片编码都不占用输入或显示状态锁。实现与真实终端验收见[终端图片](terminal-images.md)。

帧差分和边界计算使用固定源码子模块 [ztuio 核心](../crates/ztuio/README.md)。该核心不分配堆内存，缓冲区和 Unicode 宽度策略由调用方提供；图片、输入、主题与会话状态继续由客户端管理。

## 构建自己的界面

前端可以分别消费状态、终端字节和执行事件，自行决定布局与颜色。下面的接口分工适合 TUI、桌面面板或 MCP 应用：

| 前端需要的数据或动作 | 接口 |
| --- | --- |
| 客户端版本、能力与资源目录 | `capabilities(clientId)`、`resources.forClient(clientId)` |
| 配置、面板活动与当前显示状态 | `ssmcp.ui.get` |
| 调整和筛选内置界面 | `ssmcp.ui.set`、`ssmcp.ui.action`、`ssmcp.ui.filter` |
| 用户要求、草稿和交付队列 | `ssmcp.ui.queue` |
| Agent 的本地消息、提问与答案 | `ssmcp.ui.message`、`ssmcp.ui.ask`、`ssmcp.ui.questions` |
| 本工作区已保存的会话文本 | `ssmcp.ui.history` |
| 实际后端工具目录与参数 | `ssmcp.ui.harness` |
| 成员通信与本次编排的接收身份 | `collab.*`；`collab.bind` 只在 `code_mode` 中调用 |
| 编排源码与原生调用生命周期 | `ssmcp.audit.events` |
| 列出、创建、选择和关闭交互终端 | `terminal.list/open/select/close` |
| 终端输出、键盘输入和尺寸 | `terminal.read/write/resize` |
| 启动任务、读取输出和取消 | `exec.start/read/write/wait/cancel` |
| 命令的开始、输出和结束事件 | `exec.events` |
| 长输出归档 | `outputs.read` |

例如，前端一次轮询输入状态、布局和队列摘要：

```javascript
const matches = clients.filter(c => c.name === "dev");
if (matches.length !== 1) throw new Error("Select a unique client ID from list");
const id = matches[0].id;

const [conversation, panes, requests] = await Promise.all([
  call(id, "ssmcp.ui.get", { path: "/display/conversation" }),
  call(id, "ssmcp.ui.get", { path: "/display/panes" }),
  call(id, "ssmcp.ui.get", { path: "/steering/queue", limit: 4 })
]);
return { conversation, panes, requests };
```

这些 UI 原子读取，以及能力清单和客户端资源查询，默认不领取 `Next caller` 要求。独立读取 `ssmcp.audit.events`、`exec.events`、终端或输出归档属于工作类调用，会使本次编排具备领取要求的资格；需要由 agent 采集这些事件时，应同时处理响应中的 `user_messages`，定向接收时先绑定身份。

采集事件的调用分别保存每个流返回的 `next_cursor`，后续沿用各自游标，避免重复展示历史内容。检查 `truncated` 和归档状态，以便在缓存不足时读取归档。终端字节使用终端解析器绘制；命令 stdout/stderr 可按游标分开读取。调用结果还应检查原生 `isError`、任务 `running` 与 `exit_code`，让界面显示实际执行状态。

无需内置终端界面时，在配置中设置 `dashboard = false`。使用 `ssmcp --log-format jsonl` 可以将脱敏后的事件交给自己的日志消费者。自定义前端仍通过现有客户端授权访问设备能力。

相关说明：[输入、引用与本地沟通](client-input.md) · [会话历史](conversation-history.md) · [命令与事件](commands.md) · [多终端](terminal-control.md) · [后端与代码模式](backends.md) · [日志与归档](logging.md)
