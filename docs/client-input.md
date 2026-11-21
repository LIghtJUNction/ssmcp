# 客户端输入、引用与本地沟通

本文对应当前源码中的客户端交互。连接、工具目录与权限以运行中的客户端实际返回为准；本页不表示某台设备已经安装或重载这些功能。

## 启动时选择输入和处理方

`--input` 默认是 `terminal`，从终端聊天框接收消息。`--agent` 默认是 `next-caller`，让下一次符合条件的工具调用领取要求。可以只改其中一个。默认配置不会主动运行模型。需要启动时预加载协议可另用 [`--load-harness`](agent-entry.md)。显式选择的原生收件人、组发和 `/send` 仍走各自的投递路径。

```sh
ssmcp local --agent codex
printf '%s\n' '请检查 @还没创建.md，价格 $100 保持原文' | ssmcp local --input pipe
ssmcp local --input mcp-stdio
```

用 `ssmcp local --input none` 禁用当前连接的聊天框、聊天提示和人的回复或确认工具。普通命令、终端、文件、审计、独立原生消息和已启用的时间原语继续可用。调用本连接的普通问题、等待或回复工具会返回 `chat_disabled`。全局退役的工具继续返回 `tool_retired`。此模式支持独立的 `--load-harness`，显式聊天处理方仅能使用默认的 `next-caller`。该选择可随保存会话和重载保留。账户连接需要 relay 确认该能力；旧 relay 仍可使用默认聊天模式。[输入选项](../src/client/chat_io/options.rs) · [工具准入](../src/broker.rs)

`local` 是 Linux 私有会话。其他平台可以把示例中的 `local` 换成已登录的 `USERNAME@SERVER`。管道按 UTF-8 行提交，接受没有换行的最后一行。到达 EOF 后会话仍可让下一位调用方领取已入队的消息。MCP stdio 按行读写 JSON-RPC，客户端应保持连接至收到回复，关闭 stdin 会结束会话。它只在 stdout 输出协议帧。管道回执和日志使用 stderr。本机非终端输入的原生 MCP URL、一次性 OAuth 登录码及有效期也会直接显示在操作者的 stderr。登录码与下面的输入令牌分开使用。Windows 支持 Web 和 MCP HTTP，使用管道、MCP stdio 或 Unix socket 会收到明确的平台错误。

Web 与 MCP HTTP 都需要显式指定回环 host、port 和私有令牌文件。下面的命令生成一个随机令牌并启动 Web 输入。打开打印的回环地址，把该文件中的内容填入页面的 Input token 字段。

```sh
token_file=$(mktemp /tmp/ssmcp-input-token.XXXXXX)
openssl rand -hex 32 > "$token_file"
ssmcp local --input web --input-host 127.0.0.1 --input-port 8181 --input-token-file "$token_file"
```

改用 `--input mcp-http` 即在相同地址的 `/mcp` 提供 Streamable HTTP。为 MCP 客户端配置 `Authorization: Bearer …`，令牌由私有文件或客户端的私有存储提供。两个 MCP 入口都公开 `chat_submit`，参数是 `{"text":"消息正文"}`。令牌文件须归当前系统用户独占读取。删除或更换令牌会拒绝原授权的新输入，并取消尚在等待的原授权派发。已经入队或交付的消息仍按原队列规则处理。[MCP 传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) · [入口实现](../src/client/chat_io/transport.rs)

Linux 的 `ssmcp local --input sock` 默认监听 `/tmp/ssmcp-UID/chat-input.sock`。`--input-socket` 可指定另一个绝对路径，其父目录须为当前用户独占的私有目录。每个同 UID 的连接发送一行 UTF-8 文本，发完可关闭写半端，保留读半端接收实际 JSON 回执。完全关闭连接会取消仍在等待的确认，已入队的消息不会被自动撤回。已在监听的 socket 会导致启动失败。

`--agent codex`、`--agent claude` 和 `--agent opencode` 使用本机安装的 CLI。每条普通消息启动一次处理，沿用各 CLI 的配置与非交互权限策略。进程正常退出并返回正文后，客户端将实际回复保存到发起时的会话，再返回 `completed`。发起正文使用原会话中的请求 ID。失败会报告退出状态与可识别的错误类别，并保留草稿。单个标准流最多读取 64 KiB，历史中的长回复会按 16 KiB 的消息限制保留带提示的预览。若需接入自己的处理程序，用 `--agent exec --agent-exec /绝对路径/程序`，可选的 `--agent-argv-file` 是私有 JSON 字符串数组文件。程序接收固定参数、分隔符 `--` 和一个完整的消息参数。为另一个 SSMCP 选择 `--agent ssmcp --agent-peer "$recipient" --agent-account "$account"`，其中 `recipient` 是已认证消息列表中的完整收件人 ID，`account` 是对应的 `github:账号数字ID`。收件人不可用或账号不符会报错。[程序处理实现](../src/client/chat_io/process.rs) · [原生消息与收据](thread-messaging.md)

程序路由使用现有 `@文件` 和 `$技能` 准备流程。缺失引用保留正文并给出提示。原生 SSMCP 收件人收到字面正文。队列的 `queued` 与原生收据的 `included` 或 `acknowledged` 表示各自已确认的投递阶段。若回执为 `unconfirmed`，先核对请求 ID 和队列，客户端不会自动重发。保存会话与重载保留两个选项和已解析的连接目标，令牌内容留在私有文件中。管道与 MCP stdio 需要前台标准流，终端程序路由需要前台仪表板。后台会话可禁用聊天，也可用 Web、MCP HTTP 或 Linux socket 接收消息并选择处理方。

## 1. 发送要求与选择 Agent

使用默认处理方时，在前台客户端底部输入要求，按 Enter 入队。输入框立即留给下一份草稿；`sending` 表示仍在等服务器确认，`delivered` 只表示要求已附加到一次符合目标的工具响应，并不证明模型已经阅读或执行。

光标在草稿开头时按 ← 打开 Agent 面板，↑/↓ 选择，Enter/→ 设为收件人并返回会话；Esc/← 返回时保留原收件人、草稿与光标。`Next caller` 是下一次有资格领取要求的调用方。即使启动时配置了其他处理方，显式选中 `Next caller` 仍走队列；在同一面板里可切回配置的处理方。普通被观察调用只供查看，定向投递使用实际登记成员的 ID。

`/queue` 可以取回、编辑和取消仍可撤回的要求。编辑必须等服务器确认取回；交付已经发生时不能撤销。具体状态及失败恢复见[队列说明](client-ui.md#输入要求与队列)。

### 查看当前计时

有活动计时时，会话分隔栏显示倒计时剩余、已计时或本轮观察剩余。观察预算耗尽后，底层任务仍可能继续运行。`/wait` 打开带完整 ID 的当前快照；分隔栏继续更新。在客户端能力中查看 `timing.primitives_available` 查询该连接是否启用了时间工具。`ssmcp.time.now` 的单调计数只能在同一个 `monotonic_scope` 内比较。[计时显示](../src/client/terminal/conversation/timing.rs) · [时间原语](../src/runtime/time.rs)

## 2. 用 @ 引用文件，用 $ 引用技能

在符号前留空格或中文标点，带空格的路径放在引号中：

```text
请检查 @src/main.rs 和 @"docs/设计 说明.md"，按 $lint-check 的要求给出修改建议。
```

| 输入 | 处理方式 |
| --- | --- |
| `@src/main.rs` | 引用当前客户端工作区内的文件 |
| `@"docs/设计 说明.md"` | 引用带空格的路径；也支持单引号 |
| `$skill-name` | 按技能元数据的真实名称选择有效技能，依次采用项目、显式上下文根、已安装全局技能和内置来源 |
| `$"007-helper"` | 引用数字开头的技能名称；普通 `$100` 金额保持文字 |
| `@还未创建.rs`、`$未安装技能` | 正文照常发送，附上引用不可用的提示 |
| `\@literal`、`\$literal` | 保留为普通文字；反引号代码中的符号也不展开 |

键入 `@` 或 `$` 后，输入框会异步枚举真实文件或有效技能，继续输入可筛选。↑/↓ 选择，Tab 或 Enter 补全；补全引用不会立即发送要求，随后再按 Enter 才提交。目录候选以 `/` 结尾，选中后继续浏览该目录；带空格的路径自动加引号，光标前后的其他文字保留。Esc 收起候选，Shift-Tab 仍可切换焦点。本地监听目标下，`/send <正文>` 的正文也支持相同补全；第一次 Enter 接受候选，下一次 Enter 才发送。原生单收件人与 `/send` 组发使用纯文本，不显示引用候选。其他以 `!` 或 `/` 开头的本地命令、代码片段与邮箱中的符号不会触发引用补全。

`@notes.txt，$lint-check。` 中的中文逗号、句号、括号等会分开引用与正文；名称本身包含标点时使用引号，例如 `@"设计，说明.md"`。候选为空时会显示“仍可发送”，可直接提交保留的引用文字。

文件补全只枚举当前目录层级；技能补全读取有效技能索引，包含客户端启动时登记的已有安装目录。同名技能按下面的来源顺序选择，不扫描整个用户目录。菜单最多保留 64 个匹配项，索引有边界或结果不完整时会提示。目录与技能元数据使用短期缓存，枚举失败保留草稿；发送时仍重新执行引用检查。[补全实现与限制](../src/client/completion.rs)

文件引用的可用性只影响附带资料。文件不存在、不可读、是目录、超出大小限制或位于读取范围之外时，消息正文仍然发送，引用附上 `status:"unavailable"` 和具体原因。找不到或无法读取的 `$技能` 采用相同处理；同一条消息里的有效文件和技能继续正常展开。队列接受消息后，终端显示 `Queued · reference notice`；强制发送到监听程序时，在程序确认接受后显示引用提示。

可读取的文件仍须为工作区内的普通文件。`..`、符号链接和工作区外路径只保留为引用文字及提示；相应条目没有文件内容、预览或读取动作。工作区内的绝对路径会转成相对路径。

引用随原有文本消息投递。UTF-8 文本附最多 1 KiB 预览，图片和二进制附路径、类型、大小及原生读取方法；技能附来源和 `ssmcp.skills.read` 参数，agent 应完整读取技能后再应用。预览不会替代完整内容，也不会把路径伪装成已上传的副本。文件在后续读取前仍可能被用户修改。

引用展开发生在网络发送前。未闭合引号、引用数或实际消息字节数超限仍会保留草稿并提示修正；缺失文件或技能本身不会阻止发送。相同请求重试复用已经准备好的正文与提示，取回编辑显示原稿。若此前缺失的文件已经创建，重新编辑并发送才会读取新内容。正文已经占满 16 KiB 时，不可用引用的详细提示保存在本地队列，传输仍使用完整原文；消息入队确认后显示提示。重试和会话重载继续使用同一份正文与提示，显式编辑后才重新解析。有效文件的附带资料仍遵守 16 KiB 传输预算。主要限制如下：

| 范围 | 上限 |
| --- | --- |
| 一条要求中的文件及技能引用 | 8 个 |
| 普通文件引用 | 64 MiB |
| 文本预览 | 1 KiB |
| 完整技能文档读取 | 256 KiB |
| 草稿及展开后的单条要求 | 各 16 KiB UTF-8 字节 |

### 技能从哪里来

有效索引按来源顺序选择同名技能，较早的来源优先；同一物理文档也会去重。项目工作区优先，其次是按配置顺序登记的显式上下文根，然后是已安装的全局技能目录，最后是内置 `ssmcp` 技能。项目和显式上下文根默认依次查找 `.agents/skills`、`.codex/skills`、`.claude/skills`、`skills`；同一根内按这些搜索路径的顺序处理重名项。[索引与覆盖规则](../src/context.rs)

正常客户端启动会依次登记下列已存在的技能目录。环境变量有值时使用对应值，否则使用表中的回退位置；来源路径须为绝对路径。缺失目录会跳过，无法读取或发现不完整的来源会在诊断中标明。

| 顺序 | 全局技能目录 |
| --- | --- |
| 1 | `~/.agents/skills` |
| 2 | `$CODEX_HOME/skills`，回退到 `~/.codex/skills` |
| 3 | `$CLAUDE_CONFIG_DIR/skills`，回退到 `~/.claude/skills` |
| 4 | `$XDG_CONFIG_HOME/ssmcp/skills`，其次使用 `$APPDATA/ssmcp/skills`，再回退到 `~/.config/ssmcp/skills` |
| 5 | `~/.ssmcp/skills` |

这里的 home 来自 `HOME`，不存在时使用 `USERPROFILE`。全局目录中的符号链接只有指向含普通 `SKILL.md` 文件的确切技能包时，才会单独登记目标包；不会因此登记目标的父目录。相同规范路径只登记一次，原安装入口保留在 `aliases` 中。来源根在客户端启动时绑定；新增来源目录、安装新的外部链接包或修改链接目标后，需要重启客户端。已登记普通目录中的技能文件会在重新生成索引时读取。[安装目录与链接发现](../src/context/skill_sources.rs)

查询时先区分“一个来源”和“合并后的有效索引”。`ssmcp.context.roots` 列出实际登记的来源；`ssmcp.skills.list` 默认只查工作区根，传入 `{"scope":"effective"}` 才跨来源合并。`ssmcp.skills.view` 在同时省略 `root` 和 `scope` 时默认生成有效索引；要检查单个来源，使用返回的根 ID，并选择 `scope:"root"`。查询参数的 `scope` 与每条技能的来源范围不是同一字段含义。

| 返回字段 | 含义 |
| --- | --- |
| 技能的 `scope` | `project`、`global` 或 `builtin`；显式附加的上下文根归入 `global` |
| `root.id`、`root.path` | 后续读取所用的来源 ID，以及规范目录或内置来源地址 |
| 列表中的 `source`、`origin_path` | 发现技能的搜索入口与完整文档来源；内置技能的 `source` 为 `embedded` |
| `shadowed` | 有效索引中被较早来源覆盖或去重的其他来源；`same_document` 标明是否为同一物理文档 |
| `root.aliases` | 指向同一规范来源的原安装路径，不是额外的技能名称 |

读取正文时，将所选条目的 `root.id` 和 `path` 原样传给 `ssmcp.skills.read`。该接口只接受所选根内的相对 `SKILL.md` 路径，拒绝父目录穿越；返回完整 UTF-8 正文、来源、字节数和 SHA-256，超过指定上限就报错，不返回截断指令。内置技能使用 `root:"builtin"` 与 `path:"ssmcp/SKILL.md"`，完整文档上限同样为 256 KiB。

自动发现的全局来源和链接包只授予技能文档读取，不扩展普通文件工作区，也不能用于读取 `AGENTS.md` 或包内任意资源。技能需要辅助文件时，仍须使用已有授权范围内的相应文件接口。列表的元数据和补全预览用于选择技能，应用技能前应读取完整正文。[读取边界与实现](../src/context.rs)

[工作区文件接口](workspaces.md) · [技能与来源](context.md) · [引用实现](../src/client/references.rs)

## 3. 图片由原生接口读取，本地显示缩略图

### 从剪贴板或文件准备输入

| 入口 | 本地行为 |
| --- | --- |
| Ctrl+V 或 `/paste` | 读取系统剪贴板；小文本直接插入，长文本与图片保存为私有 `@` 引用；Linux 桌面文件选择按整批导入 |
| `/attach "images/设计 图.png"` | 工作区相对路径沿用原有引用与图片处理；显式绝对路径或本地 file URI 支持批量复制成私有引用 |
| `/copy` | 将最近一条已显示的 Agent 消息原文复制到系统剪贴板；可复制缓存最多 16 KiB，原文包含 Markdown 标记 |

这些动作只准备可编辑输入，**不会自动提交要求、改变收件人或调用模型**。操作异步完成；期间若草稿、答案或收件人已改变，结果不会覆盖新内容。单个图片、长文本或 handoff 保存后会显示可恢复的引用；整批文件只有完整插入成功才保留，拒绝插入或草稿已改变时回滚本批副本。剪贴板工具缺失、超时或读取失败会提示；命令失败时，仅在编辑器未改变的情况下恢复原命令，便于重试。终端若自行处理 Ctrl+V，可用 `/paste` 明确读取剪贴板；终端原有的普通文本粘贴仍可使用。

Linux 使用已安装的 Wayland/X11 剪贴板工具，macOS 和 Windows 使用各自的系统接口；不会自动安装工具。**私有图片、长文本、handoff 和文件批次保存目前只支持 Unix**，Windows 可先保存工作区文件再用 `@path`；工作区相对路径指向普通文件时，`/attach` 不另存副本。普通剪贴板文字即使看起来像路径，也保留为文字，不暗中读取该路径。

最近回复超过 16 KiB 时，复制缓存会清空，`/copy` 明确显示无可用回复，不会悄悄复制较旧内容；长回复仍可通过历史或输出归档读取。

图片源文件最多 **16 MiB、33,554,432 像素、单边 16384 像素**。小图保留原字节；需要处理时，将最长边限制到 4096 像素，再按实际编码大小缩小到 **400 KiB 原始文件字节**以内。PNG 透明度保留；处理结果会注明重编码或原始/结果尺寸。GIF/WebP 需要转码时只保存静态首帧，并显示提示。用户源文件不被覆盖。直接键入 `@image` 不触发这一步图像处理，仍适用下面的原生读取限制。

[输入任务与图像边界](../src/client/attachments.rs) · [剪贴板接口](../src/client/clipboard.rs)


### 文件选择与整批回滚

Linux 剪贴板支持 `text/uri-list` 和 GNOME 的 `x-special/gnome-copied-files`。其中 cut 选择也只复制，ssmcp 不移动或删除源文件。显式导入可写为：

```text
/attach "/tmp/设计 说明.txt" "/tmp/screenshot.png"
```

每批最多 **8 个普通文件、单文件 16 MiB、总计 32 MiB**。只接受本地绝对路径、本地 file URI 或显式 home 路径，拒绝远程 URI、父目录穿越、符号链接和 FIFO；源文件读取期间若发生变化，整批失败。所有源文件与图片先通过检查，再创建私有副本。图片使用同一 400 KiB 处理和 `ssmcp.fs.read_image` 读取路径，不增加第二份附件队列或内联图片协议。

批次完整进入草稿后才保留文件；错误、草稿变化或空间不足时，异步清理仅删除该批新建且身份仍匹配的副本。源文件和后来替换的其他文件不被删除。已经接受的副本由用户手工管理。直接 `@path` 的工作区边界不变：外部文件须经上述显式导入后，Agent 才能通过工作区内引用读取。

[事务文件导入实现](../src/client/references/files.rs)

### 原生读取与本地缩略图

`@image.png` 等引用提供 `ssmcp.fs.read_image` 读取动作。该接口支持 PNG、JPEG、GIF、WebP，每张压缩文件最多 **400 KiB（409600 字节）**，参数是工作区相对路径，不支持分片或静默缩放：

```javascript
const result = await call("CLIENT_ID_FROM_LIST", "ssmcp.fs.read_image", {
  path: "images/example.png"
});
image(result);
return result.structuredContent;
```

返回包含真正的 MCP image block，以及路径、MIME、大小和 SHA-256 元数据。`image(result)` 将图片交给当前支持图片的调用环境；本地日志不会逐字段打印 base64。

**本地缩略图在原生工具实际返回图片时出现。** 前台 TUI 将其解码成终端彩色字符缩略图；关闭颜色时用灰度字符。图片引用提交本身不会预先渲染尚未读取的内容。缩略图解码限制为每边不超过 4096 像素、解码像素不超过 64 MiB；损坏图片或超出解码限制时显示提示，原始 MCP 返回值保持不变。缩略图用于快速查看，完整图片仍由原生读取结果承载。

原生读取会检查头部签名，完整解码由预览环节处理。PNG/JPEG 可以沿用旧 relay 的图片输出校验；GIF/WebP 还需要承载 `code_mode` 的 relay 更新 MIME 校验。只更新客户端不会让旧 relay 的 `image()` 自动接受 GIF/WebP。这里没有升级 wire 协议。

[原生读取实现](../src/workspace/image.rs) · [本地预览边界](../src/client/terminal/images.rs)

## 4. 长粘贴保存为文件，再继续写要求

终端括号粘贴的正文**超过 4096 个 UTF-8 字节，或至少有 8 行**时，客户端把完整原文存到当前工作区的私有输入文件，并在草稿中插入引用。4096 字节且不足 8 行的文本仍直接内联；字节数不等于字符数：

```text
@".ssmcp/inputs/paste-<随机ID>.txt"
请根据上面的材料找出前三个问题，并给出修改顺序。
```

一次粘贴最多 **4 MiB**。小文本直接进入当前编辑器；长文本保留 UTF-8、CR 和 LF 的原始字节，通过同一个异步输入任务写文件，再按草稿 revision、收件人与答案编辑状态确认是否插入。输入期间可继续编辑，迟到结果不会覆盖新内容。插入前检查空间；超过上限明确提示另存文件，不静默截断原文。

正常大小的写入失败后，原文保留在当前进程的恢复缓冲中，原有草稿、光标与收件人不被替换。界面显示保留字节数；`/paste-retry` 重试同一原文，仍失败则继续保留，`/paste-retry discard` 才明确丢弃它。恢复缓冲不写入公共 UI 快照，不跨客户端进程重启保存。已选择原生消息目标时，自动保存得到的路径仍是普通正文，发送它不会复制文件到目标设备。

自动保存目前支持 **Unix（Linux/macOS）**。目录通过无符号链接跟随的方式逐段打开，`.ssmcp/inputs` 为 0700，文件为 0600，随机名称不会覆盖已有文件；符号链接和 FIFO 等特殊路径被拒绝。Windows 当前提示先手工保存工作区文件，再用 `@path` 引用。

成功保存的输入文件不会自动删除，避免 agent 尚未读取时内容消失。确认不再使用后，可手工清理 `.ssmcp/inputs`；在自己的项目中将该目录加入忽略规则，避免提交粘贴材料。它与要求队列、会话历史分别保存。

[长粘贴存储实现](../src/client/references/paste.rs)

## 5. Agent 的本地消息与非阻塞提问

连接后的本地沟通采用三个明确接口：**进度与结果发 `ssmcp.ui.message`，问题发 `ssmcp.ui.ask`，随后用 `ssmcp.ui.questions` 读取答案。** 这是本地通信约定；ssmcp 不拦截或自动搬运外部 ChatGPT、Claude、Codex 等聊天界面的输出。

给本机用户发送 Markdown 进度：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.ui.message", {
  text: "## 进度\n\n正在检查输入恢复和文件引用。下一步核对错误路径。",
  kind: "progress",
  idempotency_key: "input-review-progress-001"
});
```

`kind` 接受 `progress`（默认）或 `answer`，两者都按真实会话归属提交，最终回复不再需要结束审批。原有幂等键仍会复用相同内容的回执，并拒绝同一键的不同内容；旧 `finish_request_id` 字段保留为 `null`。[消息提交](../src/client/interface/messages.rs) · [入口退役说明](finish-and-questions.md#已退役的入口)

两种消息都使用固定 `[AGENT]` 标签和客户端的 Markdown 排版，写入其归属会话的 JSONL，记录使用 `role: "assistant"`、`kind: "agent_message"`。已验证线程的回复沿用它首次发起原生调用时绑定的本地会话。切换到其他历史后，回复仍可保存到原会话。[本地消息的会话归属](client-ui.md#本地消息与回答问题)

回执中的 `history_saved: true` 表示已写入与 `message_id` 同 ID 的 JSONL 记录。写入失败时工具返回错误。保存成功后，如果原会话未选中，`shown` 为 `false`。`shown: true` 表示消息已交给本地显示，回执不记录用户是否阅读。

需要重试时，使用原来的 `idempotency_key`、`text` 和 `kind`。匹配的重试返回原 ID 和 `duplicate: true`，不重复显示或追加历史；同一 key 搭配不同正文或 kind 会报错。key 限 1–96 个 ASCII 字母、数字、下划线或连字符。缓存最多保留 64 个新增 key，连接重连时保留，客户端进程重启后清空；这不是跨进程的永久去重保证。正文上限为 16 KiB UTF-8，只允许换行、制表符这两种控制字符。

`/copy` 复制最近显示的 Agent 文本。`/resume` 恢复后，复制缓存来自所选 JSONL 会话中最后一条已保存的 assistant 文本；恢复过程不会重新显示为新消息或重复写入历史。大于 16 KiB 的恢复文本不进入复制缓存。

本仓库的 relay 源码还提供 `say(clientId, text)` 和 `say(clientId, {text, kind, idempotency_key})` 简写，以及 `ui.message(clientId, arguments)`。这些 helper 随 relay 代码提供；单独更新客户端不会给已运行的旧 relay 添加 JS 全局。普通 `call(clientId, "ssmcp.ui.message", arguments)` 直接使用已注册的原生工具。当前 relay 源码把 `ui.message` 视为可领取 “next caller” 要求的交互；精确发给某个 Agent 的要求仍需 `collab.bind` 认证，显示名称不能代替绑定。

提问会立即返回问题 ID，agent 可继续不依赖答案的工作：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.ui.ask", {
  title: "优先顺序",
  question: "下一步先处理哪一项？也可以直接填写其他要求。",
  options: ["输入与编辑", "显示与历史"],
  idempotency_key: "priority-question-001"
});
```

稍后用实际返回的 ID 读取，不要占着工具调用等待用户：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.ui.questions", {
  id: "QUESTION_ID_FROM_ASK"
});
```

用户按 **Shift+↑** 打开问题，输入选项编号或自由回答，Enter 保存。答案使用独立编辑器，原来的要求草稿、目标和光标保留。Esc 返回问题列表；列表中 Delete 跳过问题，再按 Esc 回到会话。读取返回 `pending`、`answered` 或 `skipped`；重复读取不消耗答案。问题的 `idempotency_key` 只用于重试内容完全相同的问题。

问题队列属于当前客户端进程：进程重启（包括 `/reload`）时结束，旧连接记录不会自动交给新线程。最多 32 个待答问题、64 条保留记录，共用 256 KiB。回答可以来自本机 TUI，或同一账号、同一客户端上的另一个已验证 agent；只读 `attach` 不会因此获得答题能力。来源由后端记录，不能通过答案正文声明。

上例没有提供已验证成员绑定，提问方需要手动读取。通过 `code_mode.agent` 或 `collab.bind` 绑定实际线程后，答案可在该线程下一次合格工具响应的 `question_answers` 中交付；只读 `ui.questions` 不消费答案。已保存的问题和答案可以出现在会话历史中，但历史不会恢复待答队列。完整来源、自动交付与确认语义见[异步问题与答案](finish-and-questions.md#异步问题与答案)。

实现与限制：[消息与去重](../src/client/interface/messages.rs)、[本地问答接口](../src/client/interface/questions.rs)、[历史格式与交接](conversation-history.md)。

## 6. /pi、/cc、/codex 使用当前真实工具目录

三个命令共用同一套规则，`/cc` 是 Claude 注册命名空间的本地别名：

| 写法 | 行为 |
| --- | --- |
| `/pi`、`/cc`、`/codex` | 查看该 harness 当前已注册的工具第一页 |
| `/pi tools OFFSET` | 按返回的下一页偏移继续查看；另外两个命令同理 |
| `/pi TOOL_FROM_CATALOG` | 只查看该工具的原生描述和参数 schema |
| `/pi TOOL_FROM_CATALOG {"参数":"值"}` | 提供 JSON 对象后才显式调用该工具；请用真实目录名称与 schema 替换占位内容 |

目录和 schema 查询不会安装插件或发起模型回合。显式调用异步执行，实际行为与授权检查仍由所选原生工具决定。结果在本地消息中展示；长结果提供完整 JSON 的输出归档位置。

没有发现的工具会明确报错。Pi 的 `get_commands` 列出斜杠命令、技能和模板，不是内部 LSP 或模型工具目录；不能由此虚构 `pi.LSP` 等入口。需要某个能力时，先查看当前注册目录和实际 harness 状态。

[真实目录与调用规则](../src/client/interface/harness.rs)

## 7. 折叠事件、选择复制与恢复历史

默认工具摘要显示最多 **8 行**，保留尾部并标记省略；会话正文不做工具式摘要。Activity 将每次本地日志发布保留为一条事件，最多 **128 条 / 2 MiB 展示正文**。长工具事件默认折叠，User、Agent、Answer 正文默认展开；图片先显示紧凑缩略图，点击或 Enter 可展开。Tab 将焦点切到内容区，↑/↓ 选择事件，Enter 切换折叠，→ 展开，← 收起；点击事件也可切换，滚轮和 PgUp/PgDn 浏览内容。

展开只能显示当前本地保留的展示文本。它不会重新调用工具，也不会恢复摘要中已经省略的原文；完整执行输出仍按任务游标或输出归档读取。过滤视图和其他布局沿用原有日志浏览方式，不与逐事件预览缓存混为一份完整归档。

按 **F7** 进入复制模式：画面冻结并释放鼠标，拖选后使用终端自己的复制快捷键，通常为 Ctrl+Shift+C；F7 或 Esc 返回。支持的终端也可用 Shift+拖选直接调用原生选择。ssmcp 不把复制动作变成发给远端 shell 的输入。

`/help` 或 F1 显示完整帮助，↑/↓、滚轮和 PgUp/PgDn 可滚动，Home/End 到起止位置。`/resume` 选择当前工作区的本地会话历史，展示保存的 Markdown，后续记录继续写入选中的会话。

用户要求在**服务器确认发布后**写入历史，包括前台输入以及无 TUI 客户端通过 `ssmcp.ui.queue` 提交的要求。`requested:true` 只表示本地接收，不能据此认为历史已记录或要求已交付。保存错误会提示，不改变已经确认的队列结果。

历史采用版本化 `ssmcp.conversation.v1` JSONL，并经过既有审计脱敏。恢复只展示内容并选择后续保存位置，不重放旧要求、工具或模型调用，也不恢复外部 harness 会话或成员凭据。路径、记录字段、容量和只读接口见[会话历史格式](conversation-history.md)。

[完整 UI 配置与按键](client-ui.md) · [日志与输出归档](logging.md)

## 8. 直接调用 Zuse CLI 操作桌面与浏览器

先查询实际 Zuse CLI，再通过通用 exec 运行它。SSMCP 的 `computer` 命名空间只提供版本查询、下载、安装和卸载；旧浏览器、桌面动作和 Playwright setup 入口已撤去。

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
const reply = await computer.version(clientId);
if (reply.isError) return reply;
const cli = reply.structuredContent;
if (!cli.available) return cli;
return await exec.run(clientId, {
  command: [cli.path, "--help"],
  wait_ms: 5000,
  timeout_ms: 10000
});
```

读取所选版本的帮助与 schema，准备针对当前目标的 Zuse 程序，再使用 `exec.start` 的字面 argv 执行。保留 job ID、真实输出和退出状态；需要连续浏览器状态时使用该 CLI 支持的持久进程。用户指定已有浏览器时，不以新 profile 或其他浏览器代替。

下载和安装各自需要明确调用。下载返回 `download_id`，安装返回 `installation_id` 与绝对路径，不自动修改 PATH；卸载只接受仍属该管理器的安装。OAuth 浏览器登录和已授权外部 MCP 通用转发继续保留。[四个管理工具](computer-use.md) · [已有浏览器](browser-existing-session.md)

## 9. 在输入框执行 shell 命令

使用半角 `!` 前缀，例如 `!pwd` 或 `!cargo test`。命令通过当前客户端的原生 exec job 在工作区运行，使用本机配置的 shell，管道和引号由该 shell 解析。命令单独运行；`cd` 只影响这一次命令，可写成 `!cd subdir && cargo test`。

执行是异步的，输出、job ID 和退出码显示在会话里。最多同时运行四个本地命令或 harness 调用。长输出保留原生 output artifact，可用 `ssmcp.output.read` 读取；原生 `ssmcp.exec.cancel` 可以停止显示的 job ID。任务随当前客户端断开或 reload 停止。

命令由本地用户显式提交，使用当前操作系统用户权限；它不进入 Agent 需求队列。预览模式不会执行命令。命令正文与结果进入本地会话记录。

## 10. 命令行帮助

`ssmcp --help` 显示简短入口；`ssmcp --help-full` 展开全部连接参数和示例。子命令使用各自的 `--help`，连接后的完整交互操作使用 `/help`。

下一版本的中文显示语言计划见 [next-version.md](next-version.md)。

## 11. 本地记忆与结构化展示

`/context` 列出记忆分支，`/memory` 查看当前分支；`/compact [SUMMARY]` 保存检查点，`/fork [NAME]` 创建并激活分支，`/switch CONTEXT_ID` 按完整 ID 切换。它们使用统一 JSONL 的来源记录，不重放要求或工具，不启动模型。来源、容量、只读投影和本机客户端与 relay 的交付边界见 [本地上下文记忆](context-memory.md)。

需要卡片、表格、指标或步骤时，Agent 可用通用 `call(clientId, "ssmcp.ui.present", arguments)` 调用已注册的本地展示工具。它与 `ui.message` 共用消息身份、幂等回执和历史，不把展示内容加入用户要求队列。字段和支持的块类型见 [结构化展示](intelligence-ui.md)。
