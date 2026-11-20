# computer-use：原生电脑、浏览器与 Omarchy 操作

computer-use 是 ssmcp 客户端内置的控制能力。它提供 Linux、Windows 和 macOS 桌面适配器、直接使用 Chrome DevTools Protocol（CDP）的浏览器适配器，以及 Omarchy 系统原语。操作在连接的开发机执行，结构化结果和截图通过该客户端的现有连接返回；桌面和 CDP 原生接口不调用模型，也不依赖另一个 MCP 控制服务；复用浏览器扩展时，可选的 Playwright 桥会连接已安装的原生 MCP server。

当前名称统一为 `computer-use`，专用客户端与用户服务名称为 `ssmcp-computer-use`。已有调用继续使用 `ssmcp.computer.*` 和 `ssmcp.browser.*`；Omarchy 新增 `ssmcp.omarchy.*`。名称调整不改变客户端 ID 的路由规则。实现入口见 [电脑适配器](../src/computer.rs)、[浏览器适配器](../src/browser.rs)和 [Omarchy 适配器](../src/omarchy.rs)。

## zuse 后端与平台适配

桌面核心固定使用 `zuse-computer 0.2.2`，由 SSMCP 的受管工作进程执行。SSMCP 负责连接授权、私有桌面、输入取消与清理，以及需要保留的平台身份和结果确认。状态结果的 `zuse` 字段包含实际库版本、后端、可用性和探测失败原因；`engine_routes` 按操作给出真正使用的引擎。[zuse 适配入口](../src/computer/zuse_backend.rs) · [路由与状态汇总](../src/computer/routing.rs)

| 操作 | Linux | Windows / macOS |
| --- | --- | --- |
| 窗口枚举 | zuse | zuse |
| 默认截图 | zuse | zuse |
| 聚焦与键鼠输入 | zuse | 保留 SSMCP 原生身份核验、前台确认与释放清理 |
| 显示器信息 | SSMCP 原生适配器 | SSMCP 原生适配器 |
| 请求 cursor=true 时的截图路由 | X11 使用 zuse；Hyprland / Wayland 使用原生适配器 | SSMCP 原生适配器 |

引擎名称表示调用路径。操作能否执行，仍以对应 `capabilities` 项的 `available` 和 `reason` 为准。zuse 探测失败时，窗口枚举与默认截图会明确报告不可用。Windows/macOS 将带鼠标截图的能力单独报告为 `capabilities.screenshot_with_cursor`，保留其平台依赖和权限结果；Linux 的 `engine_routes.screenshot_with_cursor` 仅说明请求路由。X11 当前不会合成鼠标图像，截图元数据中的 `cursor: false` 如实表示这一限制。

Windows 公共窗口 ID 将 zuse 返回的 HWND 与 PID 组合为 `hwnd:<hex>:<pid>`。把 `computer.windows` 返回的 ID 原样传给 `computer.focus` 即可；聚焦前和等待前台确认期间都会再次检查 PID。内部 zuse worker 继续使用自身原始 ID。macOS 保留已有的窗口几何匹配、唯一性检查和前台确认。[Windows 身份转换](../src/computer/windows_identity.rs) · [Windows 焦点确认](../src/computer/windows.rs)

本次依赖升级使用 zuse 的 Desktop API。本文的浏览器 CDP 仍由 SSMCP 浏览器适配器提供；zuse 的 Browser、Android 和 iOS API 尚未接入这组桌面工具。精确依赖来源见[子模块说明](dependencies.md)。

## 默认使用私有桌面

`computer` / `cua` 的观察与输入默认选择本连接拥有的私有桌面。没有私有桌面时，`status` 仅返回平台支持、`can_start` 和缺失 helper；显示器、窗口、截图及输入不会读取或操作宿主桌面，也不会因为只读请求而启动桌面。[默认分派](../src/computer/isolated.rs)

Linux 上首次调用 `computer.app_launch(id, {command: [...]})` 会先校验 argv 和可执行文件，再自动创建 **1280 × 800** 私有 Xvfb 桌面。后续启动复用它；需要其他尺寸时先显式调用 `desktop_start`。应用使用独立 DISPLAY、Xauthority、运行目录和 D-Bus 会话。关闭客户端或调用 `desktop_stop` 只清理该连接拥有的私有桌面与应用，不把原本的宿主应用移动进去。[生命周期与环境](../src/computer/isolated.rs)

| 调用 | 尚未创建私有桌面时 |
| --- | --- |
| `computer.status` / `desktop_status` | 返回准备状态，不启动应用或显示服务 |
| `computer.monitors/windows/screenshot` | 返回先启动应用或私有桌面的明确提示 |
| `computer.move/click/key/type/scroll/drag/focus` | 返回同一提示，不触碰宿主键鼠 |
| `computer.app_launch` | 校验通过后自动创建私有桌面并启动指定程序 |
| `computer.desktop_start` | 显式创建指定大小的私有桌面 |
| `computer.desktop_stop` | 没有私有桌面时返回 `stopped:false` |

私有桌面目前需要 Linux 和 Xvfb 及返回的 helpers。Windows/macOS 默认状态明确报告不支持私有桌面，不自动切换到宿主。只有单次调用明确传入 `desktop: "host"`，才选择下表对应的宿主适配器；该参数不会改变后续默认选择。浏览器页面控制和 Omarchy 是独立接口，桌面显示隔离也不改变应用的本机文件与进程权限。[schema 与只读标记](../src/computer.rs)

## 主桌面输入需要本地短时交接

主桌面观察仍可显式使用 `desktop: "host"`。聚焦、移动、点击、拖拽、按键、文本输入和滚动则默认拒绝；远端工具参数不能开启或续期。优先使用私有桌面或已连接的后台浏览器，避免移动用户正在使用的鼠标和焦点。

确实需要主桌面时，用户在 **ssmcp 本地终端** 输入 `/computer host`，交接默认 15 秒；可使用 `/computer host 30` 指定 1–60 秒。`/computer status` 查看当前状态，`/computer stop` 立即撤销。在 ssmcp 终端再次输入键盘、鼠标或粘贴内容也会撤销；时间到、重新交接或客户端关闭都会使旧调用失效。排队中的旧调用不会因后续交接而恢复执行。

取消会中断正在等待或执行的输入，并运行已有的按键、按钮释放清理；清理完成前，本进程内其他主桌面输入保持排队。若释放清理失败，本进程的后续主桌面输入被锁住，避免继续叠加输入。私有桌面仍按连接隔离，客户端重连不继承交接状态。[本地交接与取消](../src/computer/host_control.rs) · [输入分派](../src/computer.rs)

本轮检测范围是 **ssmcp 本地终端的输入**，不声称能够捕获其他应用中的全部键鼠活动；串行锁也只覆盖同一个客户端进程，不是整个操作系统的全局锁。需要继续在其他应用工作时，保持主桌面输入锁定并使用后台控制。浏览器 CDP、Omarchy、外部 harness 及任意 shell 命令拥有各自能力，这个护栏不把具有本机执行权限的工具变成安全沙箱。

这项交接机制的本地验证在 Linux 上完成，使用假输入、计时和释放任务检查锁定、撤销与清理，没有向真实宿主桌面发送键鼠事件。Windows/macOS 的运行行为仍需对应平台的发布验收，不能由这些测试推定真机通过。

## 运行条件与平台范围

| 平台或会话 | 原生实现 | 运行条件 |
| --- | --- | --- |
| Linux · 私有 Xvfb | 默认独立桌面与应用；不使用宿主显示与输入 | `Xvfb`、`xdotool`、`magick`、`xprop`、`dbus-run-session`、`dbus-daemon` |
| Linux · Hyprland（显式 host） | 显示器、窗口、聚焦、截图和鼠标键盘输入；兼容旧 dispatcher 与新版 Lua dispatcher | 当前图形会话中的 `hyprctl`、`grim`、`wtype`、`ydotool` 及可访问的 `ydotoold` |
| Linux · X11（显式 host） | 同组桌面操作 | `xdotool`、`xprop`、ImageMagick；窗口聚焦还依赖窗口管理器 |
| Linux · 其他 Wayland（显式 host） | 按实际会话协议和已安装工具报告可用操作 | 显式 `computer.status(id,{desktop:"host"})` 后读取 `structuredContent.capabilities` |
| Windows（显式 host） | Win32 显示器与窗口 API、`SetForegroundWindow`、`SendInput`、GDI/GDI+ 截图 | 当前已登录且解锁的交互桌面；受 Windows 前台窗口策略和输入权限限制 |
| macOS（显式 host） | Quartz 显示器、窗口与输入事件；Accessibility 聚焦；系统 `screencapture` 和 `sips` 截图 | 当前图形会话；对应的辅助功能、事件发送和屏幕录制权限 |
| 三个平台的浏览器 | Rust 直接连接 Chromium CDP；启动、附着、标签页、快照、元素操作和截图 | 系统安装了兼容的 Chromium、Chrome 或 Edge |
| Linux · Omarchy | 21 个结构化系统原语，调用安装在本机的 Omarchy 命令和 Hyprland 接口 | 可识别的 Omarchy 安装；每项功能所需命令与设备可用 |

客户端会检查当前环境并返回缺失依赖或权限的原因。桌面接口的 `status` 调用不会安装软件、申请提权、弹出 macOS 授权提示或改变桌面设置。Firefox 的浏览器协议尚未接入；CDP 页面快照读取主文档，系统对话框和浏览器工具栏可通过桌面接口操作。

**三平台适配器已经实现；运行证据按平台分别记录。** Linux/Omarchy 开发机可以进行真实只读检查和端到端验证。Windows/macOS 的交叉编译、静态检查和原生运行验证属于不同证据，交叉编译通过不能证明截图、焦点、DPI 或系统权限在真机上可用。当前验证结果见 [computer-use 验收记录](validation-computer-use.md)；早期 Linux/CDP 结果保留在[历史验收记录](validation-native-control.md)。

### Windows 权限与坐标

Windows 输入需要位于当前活动的交互桌面。普通权限进程不能通过 `SendInput` 向更高完整性级别的应用注入输入；锁屏、UAC 安全桌面和非交互窗口站也不在这个适配器的控制范围内。接口会返回诊断，且不会自动申请管理员权限。这个限制来自 Windows 的 [SendInput/UIPI 规则](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)。

Windows 适配器在查询几何信息和移动指针时设置线程级的显示器 DPI 感知，并恢复原线程状态。返回的坐标已经是物理像素，因此 `monitor.scale=1` 表示“一坐标单位等于一物理像素”，不表示系统缩放设置为 100%。多屏布局可能含负坐标。相关系统语义见 [Windows DPI 文档](https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows)。

### macOS 权限与坐标

需要检查宿主权限时，显式调用 `computer.status(id,{desktop:"host"})` 并读取 `structuredContent.permissions`：`accessibility` 控制窗口聚焦，`event_posting` 控制 Quartz 输入事件，`screen_recording` 控制截图。缺失时按照返回的原因，在“系统设置 → 隐私与安全性”中为 ssmcp 可执行文件或启动它的终端授予相应权限；macOS 要求重启进程时再重启客户端。窗口标题在没有屏幕录制权限时可能被系统隐藏。系统设置入口见 Apple 的[辅助功能权限说明](https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac)和[屏幕与系统音频录制说明](https://support.apple.com/guide/mac-help/control-access-screen-system-audio-recording-mchld6aa7d23/mac)。

macOS 输入使用 Quartz 全局桌面点，主显示器左上角为原点，其他显示器可能位于负坐标区域。Retina 和缩放模式下，截图像素不一定与桌面点一一对应；始终使用该张截图返回的 `image_to_desktop`，不要自行假定缩放倍数。Quartz 的显示器与全局坐标接口见 [Apple Display Services](https://developer.apple.com/documentation/CoreGraphics/quartz-display-services)。

## 发现与调用

先调用 `list({})`，根据显示名称、任务标题和工作目录选择目标客户端，再用 `tools.search(clientId,{query:'browser',limit:20})` 查询所需工具，调用前以 `include_schemas:true` 取得完整原生 schema。旧服务端继续使用 `list({client_id})` 分页。客户端 ID 是连接身份，显示名称可能重复，也会随配置变化。

以下源码放入 `code_mode` 的 `code` 参数：

```javascript
const id = "CLIENT_ID_FROM_LIST";
return {
  desktop: await computer.status(id),
  browser: await browser.status(id),
  omarchy: await omarchy.status(id)
};
```

便捷方法返回完整 MCP 结果。先检查 `isError`，再读取 `structuredContent`。参数以当前客户端目录中的 schema 为准，也可以使用完整原生名称：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.computer.status", {});
```

旧服务器连接新客户端时仍可通过工具目录和 `call` 使用原生接口。`tools.search`、`computer`、`browser`、`omarchy` 便捷命名空间及 `image()` 图片显示通道需要同步更新服务端。`ssmcp.browser.discover` 和 Playwright 适配器由客户端提供，需要更新并重新发现客户端能力。

## CUA 入口与 Codex 工具

`code_mode` 内置 `cua`，用于直接操作连接开发机的桌面和浏览器。桌面方法与 `computer` 共用实现，`cua.browser` 与 `browser` 是同一组方法；两种写法接收相同参数、返回相同的完整 MCP 结果。截图、点击、移动、拖拽、按键、文字输入和滚动都直接调用客户端原生工具。

先读取私有桌面准备状态，再启动已安装应用，最后根据返回的截图和坐标映射操作。下面使用已安装的 `xmessage` 作为示例；其他程序需替换为目标机实际存在的可执行文件：

```javascript
const id = "CLIENT_ID_FROM_LIST";
const status = await cua.status(id);
if (status.isError) return status;
const started = await cua.app_launch(id, {
  command: ["xmessage", "ssmcp private desktop"]
});
if (started.isError) return started;

const shot = await cua.screenshot(id, { max_width: 1600 });
image(shot);
return { status: status.structuredContent, screenshot: shot.structuredContent };
```

`image(shot)` 使用现有图片通道显示截图，`shot.structuredContent` 保留尺寸、裁剪区域和 `image_to_desktop` 映射。截图失败时，`image()` 会抛出包含原生错误的异常；也可以先检查 `shot.isError` 自行处理。图片不需要转成文本日志。

电脑接口表中的所有方法都能以 `cua` 调用，例如 `cua.click(id, {x, y})`、`cua.key(id, {key: "Return"})`、`cua.type(id, {text: "你好"})`。浏览器使用 `cua.browser.snapshot(id, {target_id})` 等方法，参数仍以客户端工具目录为准。输入动作按观察结果顺序执行；后台命令使用 `exec.start`，独立命令的并行编排使用 `exec.parallel`。

**Codex 已连接的 CUA MCP 工具继续按其原始 schema 调用。** ssmcp 的 Codex 适配器读取本机 Codex 公开的工具目录，把其中的 MCP 工具注册为 `codex.mcp.*`。工具名可能包含转义或散列，必须复制 `list({client_id})` 返回的完整名称，不能根据服务名称拼接。相应结果中的 `isError`、图片、`structuredContent` 和扩展字段由适配器原样保留，截图可继续交给 `image()`：

```javascript
const id = "CLIENT_ID_FROM_LIST";
const shot = await call(id, "EXACT_CODEX_MCP_TOOL_NAME_FROM_LIST", {
  // 填入该工具实际 inputSchema 要求的截图参数。
});
image(shot);
return shot.structuredContent ?? null;
```

`cua` 入口提供的是 ssmcp 原生电脑能力；Codex MCP 转发只覆盖当前安装版本实际公开的工具。没有被 Codex 枚举的内部 CUA 接口不会被标为可用。这两条调用路径都不需要启动模型回合；原生权限和审批仍由对应工具执行。

运行 `node --test scripts/test_cua_facade.mjs` 可验证真实 `code_mode` JavaScript worker 的路由、完整结果、图片通道和错误传播。该测试使用本地协议桩，不操作真实桌面，也不证明特定平台的截图与输入权限可用；平台运行证据仍以本文的验收记录为准。

## 截图与坐标

`image()` 将截图作为真正的 MCP 图片内容块返回。截图元信息独立作为 JSON 返回，图片编码不会重复写入文本日志。

```javascript
const id = "CLIENT_ID_FROM_LIST";
// xmessage must be installed; choose an actual application from the target machine.
const started = await computer.app_launch(id, {
  command: ["xmessage", "ssmcp private desktop"]
});
if (started.isError) return started;
const shot = await computer.screenshot(id, {
  max_width: 1600,
  quality: 70
});
if (shot.isError) throw new Error(JSON.stringify(shot));
image(shot);
return shot.structuredContent;
```

单张截图最多 400 KiB，必要时降低输出尺寸。每次 `code_mode` 最多显示 4 张图片，Base64 总量最多 2 MiB；需要更多图片时分次调用。截图裁剪的 `region` 使用桌面坐标，可以从刚读取的窗口 `x/y/width/height` 构造；超出桌面边界的裁剪会被拒绝。

| 数据 | 坐标空间 |
| --- | --- |
| Linux 桌面输入与几何信息 | 全局桌面逻辑坐标，`desktop_logical` |
| Windows 桌面输入与几何信息 | 全局桌面物理像素，`desktop_physical` |
| macOS 桌面输入与几何信息 | Quartz 全局桌面点，`desktop_logical` |
| 返回的 JPEG | 图片自身的像素坐标，通过 `image_to_desktop` 转换 |
| CDP 元素和滚动 | 页面视口的 CSS 像素；元素操作直接使用快照中的 `ref` |

图片中的点按以下公式映射到当前平台的桌面坐标：

- `desktop_x = origin_x + image_x * scale_x`
- `desktop_y = origin_y + image_y * scale_y`

例如，图片横坐标为 400，`origin_x=100`、`scale_x=1.25`，则传给桌面鼠标工具的横坐标是 600。窗口移动、页面跳转或显示布局变化后重新观察，再选择输入位置。

## 电脑接口

| 方法 | 主要参数 | 返回或效果 |
| --- | --- | --- |
| `computer.desktop_status` | 无 | 只读检查本连接私有桌面、支持范围与依赖 |
| `computer.desktop_start` | 可选 `width,height` | 显式创建私有桌面；默认 1280 × 800 |
| `computer.desktop_stop` | 无 | 结束本连接私有桌面及其应用 |
| `computer.app_launch` | `command:[program,...args]` | 校验后在私有桌面启动；必要时自动创建桌面 |
| `computer.status` | 可选 `desktop` | 私有桌面准备状态，或所选适配器的能力与权限诊断 |
| `computer.monitors` | 无 | 显示器位置、尺寸、像素信息与坐标空间 |
| `computer.windows` | 无 | 最多 100 个窗口的 `window_id`、标题和几何信息 |
| `computer.focus` | `window_id` | 请求聚焦当前窗口目录中的目标，读取实际焦点确认 |
| `computer.screenshot` | 可选 `region,max_width,quality,cursor` | JPEG 图片与坐标映射 |
| `computer.move` | `x,y` | 移动鼠标 |
| `computer.click` | 可选 `x,y,button,count` | 点击；坐标需一起传入，次数 1–3 |
| `computer.drag` | `path:[{x,y},...]`；可选 `button,duration_ms` | 经过 2–64 个点，持续时间不超过 5 秒 |
| `computer.key` | `key`；可选 `modifiers` | 命名按键或快捷键；修饰键为 `ctrl/alt/shift/super` |
| `computer.type` | `text` | 输入原样 Unicode 文字，最多 16 KiB UTF-8 字节 |
| `computer.scroll` | `direction`；可选 `steps,x,y` | 四方向滚轮，步数 1–100 |

除 `desktop_start`、`desktop_status`、`desktop_stop` 与 `app_launch` 外，表内桌面方法接受可选的 `desktop: "isolated" | "host"`；省略时始终为 `isolated`。`app_launch` 接受 1–128 个 argv 项，每项最多 4096 UTF-8 字节，总计最多 32 KiB，首项必须解析为可执行文件；输入失败不会创建私有桌面。只读标记不包含自动启动行为。

常用按键名包括 `Return`、`Tab`、`Escape`、`BackSpace` 和方向键。macOS 的 `super` 对应 Command，Windows 对应 Windows 键；macOS 字母键按 ANSI 物理键位映射，文字内容使用 `computer.type`。不要用快捷键接口代替任意 Unicode 文本输入。

Hyprland、Windows 和 macOS 的聚焦实现只发送一次焦点请求，随后读取前台状态，确认目标后才报告成功。系统前台窗口与网页输入框的 DOM 焦点是两个层次：需要物理键盘输入时，先确认前台窗口，再用桌面点击或页面原语激活目标输入区。

同一客户端的电脑动作串行执行，辅助进程和整个动作都有超时。失败和取消路径会尝试释放本次按下的键或按钮；Windows 同时检查 `SendInput` 实际插入的事件数量。输入事件发出不等于目标应用完成业务操作，提交、保存或跳转后仍需读取页面、窗口或截图确认结果。

窗口列表可能包括其他工作区或屏外窗口。macOS 列表限于当前可见普通窗口；不同平台的 `window_id` 格式不同，应完整复制当前返回值。失效或歧义诊断需要刷新窗口目录。

## 浏览器接口

用户要求控制正在使用的 Chrome 时，先通过 `call(id,'ssmcp.browser.discover',{})` 读取浏览器与扩展元数据，并检查 `ssmcp.harnesses.list`。Playwright 工具只在本机 MCP server 可启动、握手及目录枚举成功后注册为 `playwright.*`；首次实际控制仍可能需要扩展自己的批准。没有可用桥时报告具体缺口，不能用 `browser.launch` 创建新 profile 或改用 Chromium 代替这次指定的会话。步骤与现场观察见[复用现有 Chrome](browser-existing-session.md)。

已有 CDP 端点可用 `browser.connect` 显式附着；`discover` 本身不会连接、启动或重启浏览器。页面操作按标签页串行，不同标签页可并发读取；同一页面的导航、快照、点击与填表仍按依赖顺序等待。快照能回答的问题优先返回有界文本和元素引用，只有需要视觉判断时再请求截图。

需要独立测试会话时，`browser.launch` 使用独立临时 profile 和本机随机调试端口，默认显示窗口；`headless:true` 适合后台验证。`executable` 可指定系统浏览器路径，启动保留 Chromium 沙箱。它直接使用 [Chrome DevTools Protocol](https://chromedevtools.github.io/devtools-protocol/)。

```javascript
const id = "CLIENT_ID_FROM_LIST";
const started = await browser.launch(id, { headless: true });
if (started.isError) throw new Error(JSON.stringify(started));
const opened = await browser.open(id, { url: "https://example.com" });
if (opened.isError) throw new Error(JSON.stringify(opened));
return await browser.snapshot(id, {
  target_id: opened.structuredContent.target_id
});
```

页面快照返回标题、URL、主文档文本和元素数组。使用本次观察返回的 `ref` 点击或填表：

```javascript
return await browser.fill("CLIENT_ID_FROM_LIST", {
  target_id: "TARGET_ID_FROM_OPEN_OR_TABS",
  ref: "REF_FROM_CURRENT_SNAPSHOT",
  text: "需要输入的文字"
});
```

元素引用绑定标签页与当前快照。导航、元素移除或新快照会使旧引用失效，收到相关错误后重新调用 `snapshot`。快照隐藏密码输入框的值；点击检查元素可见性、禁用状态和遮挡。

快照的完整 MCP 结果最多 24 KiB，返回字符串最多 400 行。中文按 UTF-8 边界裁剪，正文、元素或元信息被裁剪时返回相应标记。只有实际返回的元素引用可以继续操作。

| 方法 | 主要参数 | 返回或效果 |
| --- | --- | --- |
| `call(id,'ssmcp.browser.discover',{})` | 无 | 只读浏览器与扩展元数据；不证明连接可用 |
| `browser.status` | 无 | 连接、所有权和版本 |
| `browser.launch` | 可选 `executable,headless,width,height` | 启动当前连接拥有的浏览器 |
| `browser.connect` | `endpoint` | 附着已开启调试的本机浏览器 |
| `browser.tabs` | 无 | 标签页的 `target_id`、URL、标题 |
| `browser.open` | `url` | 打开标签页并返回 `target_id` |
| `browser.navigate` | `target_id,url` | 导航并等待对应文档 |
| `browser.snapshot` | `target_id`；可选 `max_elements,max_chars` | 有界页面文本与元素引用 |
| `browser.click` | `target_id,ref`；可选 `button,click_count` | CDP 鼠标点击 |
| `browser.fill` | `target_id,ref,text` | 填写或清空输入内容 |
| `browser.key` | `target_id,key`；可选 `modifiers` | 浏览器键名如 `Enter`；修饰键为 `Control/Shift/Alt/Meta` |
| `browser.scroll` | `target_id`；可选 `delta_x,delta_y` | 按 CSS 像素滚动 |
| `browser.evaluate` | `target_id,expression` | 执行页面 JavaScript 并返回有界 JSON |
| `browser.screenshot` | `target_id`；可选 `max_width` | 视口 JPEG 截图 |
| `browser.close` | `target_id` | 关闭指定标签页 |
| `browser.disconnect` | 无 | 结束控制连接 |

附着地址只接受回环 IP 字面量的 HTTP 地址，例如 `http://127.0.0.1:9222` 或 `http://[::1]:9222`，不接受 `localhost`。返回的 WebSocket 端点必须属于相同地址与端口，连接不使用环境代理或跟随重定向。已有浏览器需要由操作者先开启调试接口。

有界面浏览器的页面处于屏外或停止绘制时，截图可能超时。交互时先让页面可见；后台验证可以使用无界面实例。断开或客户端退出会结束本次 `launch` 启动的浏览器并清理临时 profile；`connect` 附着的浏览器由操作者管理，断开后保留它。

## Omarchy 系统原语

Omarchy 原语把系统设置表达成可检查的值，例如“夜灯开启”和“音量 35%”。适配器读取本机安装和命令接口后选择实现，支持 `OMARCHY_PATH`、打包安装目录及早期用户安装目录。Omarchy 自身的命令路由和文件布局见官方 [CLI 文档](https://github.com/omacom/omarchy/blob/quattro/docs/cli-router.md)与[文件布局](https://github.com/omacom/omarchy/blob/quattro/docs/file-layout.md)。

`omarchy.status` 可在三平台调用；非 Omarchy 环境返回明确的不可用诊断。Omarchy 环境仍需检查具体能力：没有电池、没有亮度设备或旧版辅助命令缺少所需接口，都可以单独报告，不影响其他原语。

| 方法 | 主要参数 | 返回或效果 |
| --- | --- | --- |
| `omarchy.status` | 无 | 安装版本、路径、命令和逐项能力 |
| `omarchy.themes` | 无 | 已安装主题名称及当前主题 |
| `omarchy.theme_set` | `name` | 选择枚举中的主题，读取当前主题确认 |
| `omarchy.backgrounds` | 无 | 当前主题可用背景及当前路径 |
| `omarchy.background_set` | `path` | 选择枚举中的背景路径，读取选择状态确认 |
| `omarchy.fonts` | 无 | 已安装等宽字体及当前字体 |
| `omarchy.font_set` | `name` | 选择枚举中的字体，读取字体状态确认 |
| `omarchy.audio` | 无 | 输出音量/静音与默认麦克风状态 |
| `omarchy.audio_set` | `target:output/input`；至少一个 `percent,muted` | 设置 0–100% 音量或明确静音状态并读回 |
| `omarchy.brightness` | 可选 `monitor` | 读取指定或当前聚焦显示器的亮度 |
| `omarchy.brightness_set` | `percent`；可选 `monitor` | 设置 1–100% 亮度并读回 |
| `omarchy.toggles` | 无 | 夜灯、保持唤醒、状态栏显示和动画状态 |
| `omarchy.toggle_set` | `feature,enabled` | 将一个支持的设置变为明确布尔值 |
| `omarchy.workspaces` | 无 | 有界工作区目录和当前工作区 |
| `omarchy.workspace_switch` | `workspace_id` | 切换到 1–9999 的工作区并确认 |
| `omarchy.window_action` | `window_id,action`；按动作传 `enabled` 或 `workspace_id` | 设置浮动/全屏、移动到工作区或请求关闭 |
| `omarchy.apps` | 可选 `query,limit` | 搜索可见桌面应用；默认 40 个，最多 100 个 |
| `omarchy.app_launch` | `app_id` | 请求启动最近一次应用结果中的 ID |
| `omarchy.menu` | `action:show/close`；可选 `route` | 请求显示或关闭 Omarchy 原生菜单 |
| `omarchy.battery` | 无 | 电量、充电状态、功率和容量；支持无电池诊断 |
| `omarchy.network` | 无 | 当前路由的网络类型和链路状态 |

`toggle_set.feature` 为 `nightlight`、`stay_awake`、`bar_visible` 或 `animations`。`window_action.action` 为 `floating`、`fullscreen`、`move_to_workspace` 或 `close`；前两者要求 `enabled`，移动要求 `workspace_id`，关闭不带额外状态参数。全屏定位依赖支持目标窗口的 Lua dispatcher，旧接口缺少相应能力时返回诊断。`menu.route` 为 `root`、`appearance`、`appearance.theme`、`appearance.font`、`setup` 或 `system`。

先读取状态和可选值，再提交明确的设置。下例仅查询，不改变系统：

```javascript
const id = "CLIENT_ID_FROM_LIST";
return {
  status: await omarchy.status(id),
  themes: await omarchy.themes(id),
  settings: await omarchy.toggles(id),
  workspaces: await omarchy.workspaces(id),
  applications: await omarchy.apps(id, { query: "terminal", limit: 10 })
};
```

需要开启夜灯时，使用期望值调用；已经开启会直接返回当前状态：

```javascript
return await omarchy.toggle_set("CLIENT_ID_FROM_LIST", {
  feature: "nightlight",
  enabled: true
});
```

主题、字体和背景必须来自当前枚举；应用 ID 必须来自最近一次实际返回的应用列表；显示器名取自 `computer.monitors`，窗口 ID 取自 `computer.windows`。这些约束让操作对象可以回溯到观察结果。适配器通过固定命令与独立参数执行，不把用户字符串拼成 shell 命令。

设置结果只承诺已经检查的状态。主题选择成功不能证明所有应用已完成重新着色；背景路径匹配不能证明画面已经渲染。应用启动和菜单操作返回请求接受情况，不把进程接受请求写成窗口已出现。关闭窗口采用正常关闭请求；应用显示保存确认框时可以返回 `request_sent:true,closed:false,verified:false`，随后应重新观察。

Omarchy 命令有超时，完整结果最多 24 KiB；列表裁剪时标记 `truncated`。应用扫描遵守桌面文件的隐藏和会话可见性设置。网络查询读取当前链路，不进行 Wi-Fi 扫描、联网探测或返回凭据；它不会据此声称互联网可达。

## 验证与故障定位

先用 `computer.status`、`browser.status` 和 `omarchy.status` 确认能力，再选择对应平台的检查。接口目录和 schema 是参数的最终依据；命令退出成功、请求已接受、状态已验证应按各自含义处理。

Linux 开发环境可以运行：

```sh
cargo build --locked --bins
cargo test --locked omarchy::tests
cargo test --locked omarchy::tests::readonly_live_smoke -- --ignored --nocapture
python3 scripts/e2e_computer_use.py --report target/computer-use-evidence/headless.json
```

Omarchy 的忽略测试仅查询真实系统状态，不切换主题、工作区、音量或电源状态。普通单元测试使用临时目录和伪辅助命令覆盖参数校验、结果上限、状态读回失败和清理路径。

桌面交互验收使用单独的临时测试窗口：

```sh
python3 scripts/e2e_computer_use.py --desktop --report target/computer-use-evidence/desktop.json
```

这条命令会打开并操作当前桌面的测试页、移动指针、输入文字和截图。应在可以操作该测试窗口的会话中运行。端到端脚本检查真实 SSH、OAuth PKCE、HTTP MCP、工具发现、中文输入、元素引用、图片块、进程所有权和审计分页。原语和浏览器调用继续使用客户端既有访问控制；审计记录工具名称、耗时与完成状态。

Windows/macOS 的原生焦点、混合缩放、多屏截图、权限拒绝和取消释放需要对应系统的交互会话验证。完整版本、运行平台、命令与结果记录在 [computer-use 验收记录](validation-computer-use.md)。

## 默认隔离的回归入口

普通库回归通过锁住宿主适配器入口检查默认观察和输入：即使发生错误回退，也只会触发测试超时，不会把事件送到宿主。无效 argv 回归锁住私有桌面创建入口，验证参数与可执行文件在启动前被拒绝；无需真实图形会话。

```sh
cargo test --lib computer::isolated::tests
# 下列 ignored 测试只创建本连接拥有的 Xvfb，不连接宿主显示。
cargo test --lib computer::isolated::tests::app_launch_auto_starts_private_display_and_releases_owned_state -- --ignored --exact
cargo test --lib computer::isolated::tests::private_display_input_capture_and_cleanup -- --ignored --exact
```

真实私有桌面回归需要前述 helpers；应用自动启动用 `true`，原有键入与截图回归还需要 `wish`。这两个入口分别检查自动创建/复用/清理，以及私有应用实际接收输入和截图。列出入口不等于已在当前平台执行成功；实测结果应另行记录。[回归实现](../src/computer/isolated.rs)
