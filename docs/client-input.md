# 客户端输入、引用与本地沟通

本文对应当前源码中的客户端交互。连接、工具目录与权限以运行中的客户端实际返回为准；本页不表示某台设备已经安装或重载这些功能。

## 1. 发送要求与选择 Agent

在前台客户端底部输入要求，按 Enter 入队。输入框立即留给下一份草稿；`sending` 表示仍在等服务器确认，`delivered` 只表示要求已附加到一次符合目标的工具响应，并不证明模型已经阅读或执行。

光标在草稿开头时按 ← 打开 Agent 面板，↑/↓ 选择，Enter/→ 设为收件人并返回会话；Esc/← 返回时保留原收件人、草稿与光标。`Next caller` 是下一次有资格领取要求的调用方。普通被观察调用只供查看，定向投递使用实际登记成员的 ID。

`/queue` 可以取回、编辑和取消仍可撤回的要求。编辑必须等服务器确认取回；交付已经发生时不能撤销。具体状态及失败恢复见[队列说明](client-ui.md#输入要求与队列)。

## 2. 用 @ 引用文件，用 $ 引用技能

在符号前留空格，带空格的路径放在引号中：

```text
请检查 @src/main.rs 和 @"docs/设计 说明.md"，按 $lint-check 的要求给出修改建议。
```

| 输入 | 处理方式 |
| --- | --- |
| `@src/main.rs` | 引用当前客户端工作区内的文件 |
| `@"docs/设计 说明.md"` | 引用带空格的路径；也支持单引号 |
| `$skill-name` | 按真实技能元数据名称选择有效技能，项目配置优先于明确配置的全局来源 |
| `\@literal`、`\$literal` | 保留为普通文字；反引号代码中的符号也不展开 |

键入 `@` 或 `$` 后，输入框会异步枚举真实文件或有效技能，继续输入可筛选。↑/↓ 选择，Tab 或 Enter 补全；补全引用不会立即发送要求，随后再按 Enter 才提交。目录候选以 `/` 结尾，选中后继续浏览该目录；带空格的路径自动加引号，光标前后的其他文字保留。Esc 收起候选，Shift-Tab 仍可切换焦点。以 `!` 或 `/` 开头的本地命令、代码片段与邮箱中的符号不会触发引用补全。

补全只枚举当前目录层级及明确授予的技能来源，项目技能覆盖同名全局技能；不扫描任意用户目录。菜单最多保留 64 个匹配项，索引有边界或结果不完整时会提示。目录与技能元数据使用短期缓存，枚举失败保留草稿；发送时仍重新执行原有引用检查。[补全实现与限制](../src/client/completion.rs)

文件必须存在、可读且为工作区内的普通文件。目录、`..`、符号链接、工作区外路径和超限文件会返回错误，原稿保留。工作区内的绝对路径会转成相对路径；这不会授予读取其他目录的权限。

引用随原有文本消息投递。UTF-8 文本附最多 1 KiB 预览，图片和二进制附路径、类型、大小及原生读取方法；技能附来源和 `ssmcp.skills.read` 参数，agent 应完整读取技能后再应用。预览不会替代完整内容，也不会把路径伪装成已上传的副本。文件在后续读取前仍可能被用户修改。

引用展开发生在网络发送前。确定尚未发送的解析或读取失败会恢复为可编辑状态；相同请求重试复用已经准备好的正文，取回编辑显示原稿，避免引用块反复追加。主要限制如下：

| 范围 | 上限 |
| --- | --- |
| 一条要求中的文件及技能引用 | 8 个 |
| 普通文件引用 | 64 MiB |
| 文本预览 | 1 KiB |
| 完整技能文档读取 | 256 KiB |
| 草稿及展开后的单条要求 | 各 16 KiB UTF-8 字节 |

[工作区文件接口](workspaces.md) · [技能与来源](context.md) · [引用实现](../src/client/references.rs)

## 3. 图片由原生接口读取，本地显示缩略图

### 从剪贴板或文件准备输入

| 入口 | 本地行为 |
| --- | --- |
| Ctrl+V 或 `/paste` | 读取操作系统剪贴板；小文本插入当前草稿或答案，超过 8 KiB 的文本和图片保存为私有文件后插入 `@` 引用 |
| `/attach "images/设计 图.png"` | 导入一个工作区文件；普通文件保留原路径引用，图片经过有界检查与必要处理后保存为私有副本 |
| `/copy` | 将最近一条已显示的 Agent 消息原文复制到系统剪贴板；可复制缓存最多 16 KiB，原文包含 Markdown 标记 |

这些动作只准备可编辑输入，**不会自动提交要求、改变收件人或调用模型**。操作异步完成；期间若草稿、答案或收件人已改变，结果不会覆盖新内容。已经保存的文件会显示可恢复的引用。剪贴板工具缺失、超时或读取失败会提示；命令失败时，仅在编辑器未改变的情况下恢复原命令，便于重试。终端若自行处理 Ctrl+V，可用 `/paste` 明确读取剪贴板；终端原有的普通文本粘贴仍可使用。

Linux 使用已安装的 Wayland/X11 剪贴板工具，macOS 和 Windows 使用各自的系统接口；不会自动安装工具。**私有图片、长文本和 handoff 文件的自动保存目前只支持 Unix**，Windows 可先保存工作区文件再用 `@path`；普通文件的 `/attach` 不需要另存副本。普通剪贴板文字即使看起来像路径，也保留为文字，不暗中读取该路径。

最近回复超过 16 KiB 时，复制缓存会清空，`/copy` 明确显示无可用回复，不会悄悄复制较旧内容；长回复仍可通过历史或输出归档读取。

图片源文件最多 **16 MiB、33,554,432 像素、单边 16384 像素**。小图保留原字节；需要处理时，将最长边限制到 4096 像素，再按实际编码大小缩小到 **400 KiB 原始文件字节**以内。PNG 透明度保留；处理结果会注明重编码或原始/结果尺寸。GIF/WebP 需要转码时只保存静态首帧，并显示提示。用户源文件不被覆盖。直接键入 `@image` 不触发这一步图像处理，仍适用下面的原生读取限制。

[输入任务与图像边界](../src/client/attachments.rs) · [剪贴板接口](../src/client/clipboard.rs)

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

终端的括号粘贴超过 **8 KiB** 时，客户端把完整 UTF-8 文本存到当前工作区的私有输入文件，并在草稿中插入引用：

```text
@".ssmcp/inputs/paste-<随机ID>.txt"
请根据上面的材料找出前三个问题，并给出修改顺序。
```

一次粘贴最多 **4 MiB**。正常的小文本直接进入当前编辑器；长文本不会整份塞进 16 KiB 消息。插入前检查草稿空间，写入失败或超过上限会明确提示，不静默截断原文。

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

`kind` 可选，接受 `progress`（默认）或 `answer`；完成时用 `answer` 给出结果、验证和仍存在的限制。两种消息都使用固定 `[AGENT]` 标签和当前客户端的 Markdown 排版，写入同一份 JSONL 历史。API 的 `kind` 用于显示分类，历史记录仍使用 `role: "assistant"`、`kind: "agent_message"`。消息不会作为用户要求入队。

返回值中的 `message_id` 标识该条消息。`history_saved: true` 表示它已保存为同 ID 的 JSONL 记录；保存失败时仍显示消息，并返回 `history_saved: false`。`shown` 和 `accepted` 表示本地接受显示请求，不证明用户已阅读。

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

问题队列属于当前客户端进程：连接重连时保留，进程重启（包括 `/reload`）时结束。最多 32 个待答问题、64 条保留记录，共用 256 KiB。回答由该客户端的前台 TUI 写入；后台 API 能创建和读取问题，不会使只读 `attach` 获得答题能力。已保存的问题和答案可以出现在会话历史中，但历史不会恢复待答队列。未绑定 Agent 的只读 `ui.questions`、`ui.history` 调用不会领取用户要求。

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

默认工具摘要显示最多 **8 行**，保留尾部并标记省略；会话正文不做工具式摘要。Activity 将每次本地日志发布保留为一条事件，最多 **128 条 / 2 MiB 展示正文**。长工具事件默认折叠，User、Agent、Answer 和图片默认展开。Tab 将焦点切到内容区，↑/↓ 选择事件，Enter 切换折叠，→ 展开，← 收起；点击事件也可切换，滚轮和 PgUp/PgDn 浏览内容。

展开只能显示当前本地保留的展示文本。它不会重新调用工具，也不会恢复摘要中已经省略的原文；完整执行输出仍按任务游标或输出归档读取。过滤视图和其他布局沿用原有日志浏览方式，不与逐事件预览缓存混为一份完整归档。

按 **F7** 进入复制模式：画面冻结并释放鼠标，拖选后使用终端自己的复制快捷键，通常为 Ctrl+Shift+C；F7 或 Esc 返回。支持的终端也可用 Shift+拖选直接调用原生选择。ssmcp 不把复制动作变成发给远端 shell 的输入。

`/help` 或 F1 显示完整帮助，↑/↓、滚轮和 PgUp/PgDn 可滚动，Home/End 到起止位置。`/resume` 选择当前工作区的本地会话历史，展示保存的 Markdown，后续记录继续写入选中的会话。

用户要求在**服务器确认发布后**写入历史，包括前台输入以及无 TUI 客户端通过 `ssmcp.ui.queue` 提交的要求。`requested:true` 只表示本地接收，不能据此认为历史已记录或要求已交付。保存错误会提示，不改变已经确认的队列结果。

历史采用版本化 `ssmcp.conversation.v1` JSONL，并经过既有审计脱敏。恢复只展示内容并选择后续保存位置，不重放旧要求、工具或模型调用，也不恢复外部 harness 会话或成员凭据。路径、记录字段、容量和只读接口见[会话历史格式](conversation-history.md)。

[完整 UI 配置与按键](client-ui.md) · [日志与输出归档](logging.md)

## 8. 在 code_mode 中复用浏览器插件

控制现有 Chrome 时，先区分三件事：本机发现了扩展文件、Playwright MCP 服务已注册工具、浏览器已经批准连接。前两项都不能替代第三项。ssmcp 的 Playwright 适配器复用已安装的 MCP 服务及其 `--extension` 模式；首次连接仍由浏览器中的原生扩展确认，默认后台模式不会替用户批准配对。

先在 `code_mode` 查看元数据、服务状态和实际目录，以下代码不执行浏览器操作：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
const [observed, providers, catalog] = await Promise.all([
  call(clientId, "ssmcp.browser.discover", {}),
  call(clientId, "ssmcp.harnesses.list", {}),
  tools.search(clientId, {
    query: "playwright.",
    include_schemas: true,
    limit: 20
  })
]);
return { observed, providers, catalog };
```

目录有 `nextCursor` 时，沿用相同查询和游标继续读取。根据真实返回的工具名、描述和 `inputSchema` 选择动作，再调用：

```javascript
// 把下面的名称与参数替换为刚才目录实际返回的值。
return await call(
  "CLIENT_ID_FROM_LIST",
  "EXACT_PLAYWRIGHT_TOOL_NAME_FROM_CATALOG",
  { /* 按该工具的 inputSchema 填写实际参数 */ }
);
```

适配器只给原生名称加 `playwright.` 路由前缀，保留工具描述、参数、返回值和原生授权。操作后继续读取该工具的实际结果或快照，再决定下一步；不要根据想象的页面结构、工具名或旧引用连续点击。截图结果包含 MCP image block 时，可用本页图片示例中的 `image(result)` 展示。

找不到可调用工具时，查看 `ssmcp.harnesses.list` 的实际诊断。本机 `ssmcp setup browser --check` 检查已有安装；`ssmcp setup browser` 可保存可用 CLI 的配置；只有明确需要安装时才用 `--install`。配置变化后重启客户端刷新目录。扩展已安装但 MCP CLI 缺失、CLI 不支持后台模式、浏览器尚未批准连接，是不同状态；不要将它们都当成“浏览器已可控制”。

ChatGPT、Claude 等扩展的安装记录仅用于发现。它们只有在发起端会话或客户端真实暴露了可用工具时才能调用；不能凭扩展名称或磁盘文件推断出可通过 `code_mode` 使用的隐藏接口。需要复用已有登录会话时，保留当前浏览器与其原生批准流程。

`browser.*` 是另一条已存在的 Chromium CDP 通道，不是 `playwright.*` 的别名。只有观察到实际可用的 loopback CDP 端点后才连接。`browser.act` 可在同一目标上按顺序执行最多 16 个动作，`browser.wait` 可等待已观察到的 selector 或文字；批次失败时检查已完成数量与失败步骤，不整批自动重放。不要为了代替用户指定的现有 Chrome 而启动另一浏览器。

[现有浏览器与扩展](browser-existing-session.md) · [后台浏览器与桌面](browser-desktop.md) · [原生 Playwright 适配器](../src/backend/playwright.rs)

## 9. 在输入框执行 shell 命令

使用半角 `!` 前缀，例如 `!pwd` 或 `!cargo test`。命令通过当前客户端的原生 exec job 在工作区运行，使用本机配置的 shell，管道和引号由该 shell 解析。命令单独运行；`cd` 只影响这一次命令，可写成 `!cd subdir && cargo test`。

执行是异步的，输出、job ID 和退出码显示在会话里。最多同时运行四个本地命令或 harness 调用。长输出保留原生 output artifact，可用 `ssmcp.output.read` 读取；原生 `ssmcp.exec.cancel` 可以停止显示的 job ID。任务随当前客户端断开或 reload 停止。

命令由本地用户显式提交，使用当前操作系统用户权限；它不进入 Agent 需求队列。预览模式不会执行命令。命令正文与结果进入本地会话记录。

## 10. 命令行帮助

`ssmcp --help` 显示简短入口；`ssmcp --help-full` 展开全部连接参数和示例。子命令使用各自的 `--help`，连接后的完整交互操作使用 `/help`。

下一版本的中文显示语言计划见 [next-version.md](next-version.md)。
