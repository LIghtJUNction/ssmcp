# ssmcp-desktop

双击桌面应用，或执行 `ssmcp gui` 打开连接表单。在 `ssmcp 用户名 @ 中继服务器` 两个输入框中填写账号和地址。默认中继是 `ssmcp.lmm.best`，也可以填自建服务器。首次使用默认中继时，阅读用户协议与隐私政策并勾选同意。还没有账号时，点击「注册账号」前往当前中继。

点击「连接」后，桌面窗口为所选中继启动自己的会话核心，并核对核心进程的身份。侧栏只列出本窗口创建的会话。连接成功后保存本机桌面设置。以后打开应用即可使用同一目标；侧栏的「连接设置」可更换。命令行也可用 `ssmcp alice@ssmcp.lmm.best --gui` 指定账号。Linux 和 Windows 均使用 zgpui 原生窗口。把 `ssmcp` 和桌面可执行文件装在同一目录，或将 CLI 放入 PATH。

「+」或 `/new` 新建会话并切到新标签。`/cd 目录` 切换当前会话内后续命令使用的工作目录。会话标签保留各自的消息草稿。终端、事件和回执可从侧栏打开。

## 可以完成的工作

| 页面 | 实际数据与操作 |
| --- | --- |
| 本地会话 | 常规窗口显示自己创建的会话；通过 `--session` 或 `--directory` 启动的观察窗口可查看本机已有会话 |
| 对话 | 读取当前进程保留的共享对话，选择主会话或实际 agent 线程，通过 `chat.send` 提交显式输入 |
| 事件 | 读取带原始序号的实时事件；展示当前已应用游标与保留窗口缺口 |
| 终端 | 从真实 `data_base64` 输出建立 VT 文本画面，显式写入、回车、Ctrl+C、Tab、方向键、调整全部终端尺寸 |
| Worker | 分页读取实际站位、登记状态、任务、进度和用时；使用当前 `pool_generation` 调整目标容量 |
| 回执 | 保留原请求 ID、原后端代次及真实操作结果；输入队列单独显示 CLI 的投递状态 |

已有会话的观察界面默认只读。只有取得控制租约，而且当前能力确实公布相应命令，写入按钮才可用。关闭窗口会释放租约并结束由本窗口启动的核心进程。通过 --session 或 --directory 附加的已有 CLI 会话继续运行。

## 启动

打开本机连接表单或已有会话：

```sh
ssmcp-desktop
ssmcp-desktop --session my-session
```

使用 `--session` 进入已有会话界面后，可从左侧选择其他已保存会话。程序通过公开的 `RemoteFrontend::connect_named` 解析会话，不猜测私有数据目录。

明确申请控制可以在窗口内点击「申请控制」，也可以在启动时指定：

```sh
ssmcp-desktop --session my-session --control
```

需要连接手工管理的 attach 服务时，可以显式传入其受保护服务目录：

```sh
ssmcp-desktop --directory /absolute/private/session-directory
```

认证和 lease 全部由 CLI 的现有 IPC 实现处理。桌面状态不会保留或输出 descriptor、token、lease，也不会展示会话列表中的 `state_directory`。连接失败时展示可操作的状态说明，不把底层原始认证错误输出到 UI 或日志。

可用启动选项：

| 选项 | 作用 |
| --- | --- |
| `--target USER@RELAY` | 在原生表单中使用给定目标，首次连接在此确认 |
| `--session NAME` | 连接已有命名会话 |
| `--directory PATH` | 连接显式指定的已有服务目录，与 `--session` 互斥 |
| `--control` | 为指定目标申请控制租约；省略时只观察 |
| `--font FAMILY` | 使用已安装的系统字体族；不下载字体 |
| `--exit-after-ms N` | 真实窗口在指定时限后关闭，供截图和验收使用；最大一小时 |

默认字体为 Linux 的 `Noto Sans CJK SC` 和 Windows 的 `Microsoft YaHei UI`。zgpui 使用系统字体回退；Linux 需要安装能覆盖中文的字体。界面支持 Unicode 输入、系统剪贴板、Tab 焦点切换和滚轮。

## 源码和构建

要求 Rust / Cargo **1.100 或更新版本**，以及完整 ssmcp 仓库。应用使用独立 Cargo 工作区，以 `path = "../.."` 复用 CLI 的真实连接实现。

| 依赖 | 来源和记录 |
| --- | --- |
| zgpui | 使用 Git 默认分支；应用 lockfile 记录本轮验证的确切提交 |
| zbridge | 与 ssmcp、zgpui 共用 `https://github.com/LIghtJUNction/zbridge.git` 来源；应用 lockfile 与根 crate 保持同一提交 |
| ssmcp | 同一源码 checkout 的根 crate |

内部跨仓库依赖的 manifest 不写 version、branch、tag 或 rev。尤其要保持 zbridge 的规范 Git 来源一致：同一协议 crate 的不同 Cargo source 会生成不同的 Rust 类型，破坏 `RemoteFrontend` 与 `BridgeModel` 的直接互操作。

上游变更公开后，在仓库根目录刷新应用的独立锁文件，并核对它与根 crate 使用同一组内部来源：

```sh
cargo update --manifest-path apps/ssmcp-desktop/Cargo.toml
cargo metadata --manifest-path apps/ssmcp-desktop/Cargo.toml --locked --format-version 1
```

随后保留 `apps/ssmcp-desktop/Cargo.lock`，正常构建使用 `--locked`。生成 lockfile 不代表构建或验收已经通过。

### Linux

本机需要 C 编译器、归档器、pkg-config、X11、FreeType 和 Fontconfig 开发文件。默认 OpenGL 构建还需要 GL 开发文件与可用 GLX 显示；Wayland 桌面通过 XWayland 运行。

```sh
cargo build --manifest-path apps/ssmcp-desktop/Cargo.toml --locked --release
apps/ssmcp-desktop/target/release/ssmcp-desktop
```

没有 GLX 的 X11 环境可使用真实软件渲染后端：

```sh
cargo build --manifest-path apps/ssmcp-desktop/Cargo.toml \
  --locked --release --features linux-software
```

该构建仍使用原生 X11 窗口。软件后端不创建 GLX 上下文，也不链接 OpenGL。项目没有 WebView、浏览器子进程或前端 JavaScript 运行时。

### Windows

在 Windows 的 Visual Studio Native Tools 环境中，使用 MSVC Rust 工具链构建同一 manifest。zgpui 的构建脚本调用 `cl` / `lib` 并链接系统 `user32`、`gdi32`、`opengl32`、`kernel32`。

```powershell
cargo build --manifest-path apps/ssmcp-desktop/Cargo.toml --locked --release
```

Linux 上已有 Windows GNU 工具链时，可执行交叉构建：

```sh
CC=x86_64-w64-mingw32-gcc \
AR=x86_64-w64-mingw32-ar \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
cargo build --manifest-path apps/ssmcp-desktop/Cargo.toml \
  --locked --release --target x86_64-pc-windows-gnu
```

交叉编译验证和 Windows 实机交互验收是不同检查。当前源码没有安装器、自更新器或系统服务注册逻辑。

## 已有会话的连接与一致性

原生窗口只在主线程绘制。一个独立的 current-thread Tokio 线程持有 `RemoteFrontend`、`BridgeModel` 和 `EventObserver`。两者通过容量为 16 的操作队列和有界视图快照通信。空闲窗口等待系统事件，后台未改变时不重复绘制。

所有写入来自明确点击。请求保留点击时的本地连接序号与 `generation`；如果执行前连接或目标已经改变，后台拒绝这次操作。已经发送但未收到可验证回执的命令保留 `unknown`，继续关联原请求和原代次。客户端不会在超时、断开、能力刷新或重新连接时重发这些命令。

`chat.send` 回执必须匹配原请求 ID 与目标，包含有效消息 ID，且明确标记 `confirmation: local_queue_admission`。未知状态或不完整确认会保留为 `unknown`。只有连接、代次、发送目标和草稿原文全部匹配时，成功确认才会清空该草稿。终端控制键的回执保留尚未发送的终端文字。

`chat.send` 的成功回执仅证明 **CLI 本地输入队列接收了消息**。它不证明模型已经读取，终端写入回执也不证明终端里的命令已经完成。Worker 缩容的 `resize_pending` 表示仍在等待释放站位。

事件观察使用 CLI 公开的 `EventObserver`。每个原生 zbridge 请求等待不超过 1,000 ms，桌面组合观察窗口为 1,500 ms，低于 55 秒上限。原生事件页最多读取 64 条。完整连接、状态刷新或事件缺口协调各自共享 **15 秒绝对截止时间**，全部分页从同一剩余预算中取时间，单页还受 8 秒上限限制。后台在各个有界阶段之间检查排队操作；不完整的刷新不会更新完成时间或发布完整状态。

关闭窗口会通过 stop 通知立即唤醒正在等待的只读 IPC 或事件观察，随后 lease 释放最多再等待 2 秒。已经发出的写入命令不进入这条取消路径，继续等待原回执，最多 12 秒；超时仍保留原请求的未知结果。已验证的历史页游标保留，未消费事件页不会因取消而前进。

所有协议计数器保持为 `Counter(u64)`，在线上和界面中使用完整十进制字符串。事件页通过完整校验后才更新画面，再提交 `next_cursor`。发现 `retention_gap` 时先刷新当前状态，终端解析器重新建立，界面明确标注缺口。代次变化会清理旧的实时投影；旧操作回执仍保留。

## 已有会话观察界面的边界

- 桌面前端可启动指定目标的新会话，也可显式附加到当前用户已有的 CLI 会话。设备 CLI 建立中继连接。控制者通过 MCP 应用完成账号授权。
- 对话与事件各保留至多 320 条、约 256 KiB 文本。早期记录被省略时显示数量；CLI 的上游内存保留窗口仍是最终历史边界。
- 终端显示真实 VT 文本与光标/控制序列产生的文本结果，采用单色网格。当前没有彩色单元、图像协议、鼠标直通或任意按键直通。
- `terminal.list` 不报告实际行列，初始 24 × 80 只是本地画布默认。只有收到本窗口的尺寸调整回执后才把尺寸视为已确认。每个终端画布最多 200 × 300 单元，最多 16 个终端；超过范围会明确标记投影不完整。
- 连接前的终端输出不会被补造。终端初次连接从当前输出开始观察，历史对话使用单独的 `conversation` 资源读取。
- 编辑器使用 zgpui 的原生单行输入控件，输入最多 4,096 UTF-8 字节。CLI 的协议允许更大的消息，但本窗口当前输入控件较小。
- 操作回执最多保留 24 条，只淘汰已确认或已拒绝的终态；等待回执和未知结果持续保留原请求身份。容量全部由未解决请求占用时，界面和后台暂停新增写入。未发送草稿保留在当前窗口内存中，退出后不落盘。关闭窗口时会等待当前有界请求与 lease 释放；底层传输中断时释放状态可能仍待服务过期处理。
- 图形驱动、字体、CLI crate 与解析器也会占用内存。这里列出的集合上限不是整个应用的 RSS 或磁盘体积承诺，实际体积需从发布构建测量。

## 验证

```sh
cargo test --manifest-path apps/ssmcp-desktop/Cargo.toml --locked
cargo build --manifest-path apps/ssmcp-desktop/Cargo.toml --locked --release
```

协议测试覆盖大于 JavaScript 精确整数范围的游标、数字 JSON 拒绝、整页历史校验、重复事件、内存保留上限、认证路径字段省略、未知回执跨重新发现保留、分页不前进拒绝、终端缺口与显式尺寸回执。暂停时钟的异步测试验证多页读取共用 15 秒预算、stop 立即取消挂起读取及保留安全游标；命令进入可取消读取路径会被拒绝。回执回归验证原请求与入队凭据、连接/代次/目标/正文匹配、未解决回执保留和容量满时停止写入。

实际验收应使用运行中的会话完成：观察连接、线程切换、明确发送及队列回执、终端输出和写入、Worker 缩容握手、断开后 CLI 继续运行。使用 `--exit-after-ms` 可以为真实窗口截图提供有界运行时间，不会加载静态演示数据。Linux GPU、Linux 软件渲染与 Windows 实机分别记录结果，未完成的检查保持未验证。

实现入口：[main.rs](src/main.rs) · [engine.rs](src/engine.rs) · [model.rs](src/model.rs) · [ui.rs](src/ui.rs)。CLI 协议依据：[gui-bridge.md](../../docs/gui-bridge.md)、[RemoteFrontend](../../src/session/attach/zbridge_adapter/remote.rs)、[会话资源适配器](../../src/session/attach/zbridge_adapter.rs)。
