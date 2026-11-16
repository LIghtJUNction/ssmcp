# computer-use：原生电脑、浏览器与 Omarchy 操作

computer-use 是 ssmcp 客户端内置的控制能力。它提供 Linux、Windows 和 macOS 桌面适配器、直接使用 Chrome DevTools Protocol（CDP）的浏览器适配器，以及 Omarchy 系统原语。操作在连接的开发机执行，结构化结果和截图通过原有 SSH 连接返回；这些接口不调用模型，也不依赖另一个 MCP 控制服务。

当前名称统一为 `computer-use`，专用客户端与用户服务名称为 `ssmcp-computer-use`。已有调用继续使用 `ssmcp.computer.*` 和 `ssmcp.browser.*`；Omarchy 新增 `ssmcp.omarchy.*`。名称调整不改变客户端 ID 的路由规则。实现入口见 [电脑适配器](../src/computer.rs)、[浏览器适配器](../src/browser.rs)和 [Omarchy 适配器](../src/omarchy.rs)。

## 运行条件与平台范围

| 平台或会话 | 原生实现 | 运行条件 |
| --- | --- | --- |
| Linux · Hyprland | 显示器、窗口、聚焦、截图和鼠标键盘输入；兼容旧 dispatcher 与新版 Lua dispatcher | 当前图形会话中的 `hyprctl`、`grim`、`wtype`、`ydotool` 及可访问的 `ydotoold` |
| Linux · X11 | 同组桌面操作 | `xdotool`、`xprop`、ImageMagick；窗口聚焦还依赖窗口管理器 |
| Linux · 其他 Wayland | 按实际会话协议和已安装工具报告可用操作 | 以 `computer.status.capabilities` 的逐项诊断为准 |
| Windows | Win32 显示器与窗口 API、`SetForegroundWindow`、`SendInput`、GDI/GDI+ 截图 | 当前已登录且解锁的交互桌面；受 Windows 前台窗口策略和输入权限限制 |
| macOS | Quartz 显示器、窗口与输入事件；Accessibility 聚焦；系统 `screencapture` 和 `sips` 截图 | 当前图形会话；对应的辅助功能、事件发送和屏幕录制权限 |
| 三个平台的浏览器 | Rust 直接连接 Chromium CDP；启动、附着、标签页、快照、元素操作和截图 | 系统安装了兼容的 Chromium、Chrome 或 Edge |
| Linux · Omarchy | 21 个结构化系统原语，调用安装在本机的 Omarchy 命令和 Hyprland 接口 | 可识别的 Omarchy 安装；每项功能所需命令与设备可用 |

客户端会检查当前环境并返回缺失依赖或权限的原因。桌面接口的 `status` 调用不会安装软件、申请提权、弹出 macOS 授权提示或改变桌面设置。Firefox 的浏览器协议尚未接入；CDP 页面快照读取主文档，系统对话框和浏览器工具栏可通过桌面接口操作。

**三平台适配器已经实现；运行证据按平台分别记录。** Linux/Omarchy 开发机可以进行真实只读检查和端到端验证。Windows/macOS 的交叉编译、静态检查和原生运行验证属于不同证据，交叉编译通过不能证明截图、焦点、DPI 或系统权限在真机上可用。当前验证结果见 [computer-use 验收记录](validation-computer-use.md)；早期 Linux/CDP 结果保留在[历史验收记录](validation-native-control.md)。

### Windows 权限与坐标

Windows 输入需要位于当前活动的交互桌面。普通权限进程不能通过 `SendInput` 向更高完整性级别的应用注入输入；锁屏、UAC 安全桌面和非交互窗口站也不在这个适配器的控制范围内。接口会返回诊断，且不会自动申请管理员权限。这个限制来自 Windows 的 [SendInput/UIPI 规则](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)。

Windows 适配器在查询几何信息和移动指针时设置线程级的显示器 DPI 感知，并恢复原线程状态。返回的坐标已经是物理像素，因此 `monitor.scale=1` 表示“一坐标单位等于一物理像素”，不表示系统缩放设置为 100%。多屏布局可能含负坐标。相关系统语义见 [Windows DPI 文档](https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows)。

### macOS 权限与坐标

先读取 `computer.status.permissions`：`accessibility` 控制窗口聚焦，`event_posting` 控制 Quartz 输入事件，`screen_recording` 控制截图。缺失时按照返回的原因，在“系统设置 → 隐私与安全性”中为 ssmcp 可执行文件或启动它的终端授予相应权限；macOS 要求重启进程时再重启客户端。窗口标题在没有屏幕录制权限时可能被系统隐藏。系统设置入口见 Apple 的[辅助功能权限说明](https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac)和[屏幕与系统音频录制说明](https://support.apple.com/guide/mac-help/control-access-screen-system-audio-recording-mchld6aa7d23/mac)。

macOS 输入使用 Quartz 全局桌面点，主显示器左上角为原点，其他显示器可能位于负坐标区域。Retina 和缩放模式下，截图像素不一定与桌面点一一对应；始终使用该张截图返回的 `image_to_desktop`，不要自行假定缩放倍数。Quartz 的显示器与全局坐标接口见 [Apple Display Services](https://developer.apple.com/documentation/CoreGraphics/quartz-display-services)。

## 发现与调用

先调用 `list({})`，根据显示名称、任务标题和工作目录选择目标客户端，再通过 `list({client_id})` 读取它的工具目录和 schema。目录分页时继续传入 `nextCursor`。客户端 ID 是连接身份，显示名称可能重复，也会随配置变化。

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

旧服务器连接新客户端时仍可通过工具目录和 `call` 使用原生接口。`computer`、`browser`、`omarchy` 便捷命名空间及 `image()` 图片显示通道需要同步更新服务端。

## 截图与坐标

`image()` 将截图作为真正的 MCP 图片内容块返回。截图元信息独立作为 JSON 返回，图片编码不会重复写入文本日志。

```javascript
const shot = await computer.screenshot("CLIENT_ID_FROM_LIST", {
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
| `computer.status` | 无 | 平台、会话、逐项能力、依赖与权限诊断 |
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

常用按键名包括 `Return`、`Tab`、`Escape`、`BackSpace` 和方向键。macOS 的 `super` 对应 Command，Windows 对应 Windows 键；macOS 字母键按 ANSI 物理键位映射，文字内容使用 `computer.type`。不要用快捷键接口代替任意 Unicode 文本输入。

Hyprland、Windows 和 macOS 的聚焦实现只发送一次焦点请求，随后读取前台状态，确认目标后才报告成功。系统前台窗口与网页输入框的 DOM 焦点是两个层次：需要物理键盘输入时，先确认前台窗口，再用桌面点击或页面原语激活目标输入区。

同一客户端的电脑动作串行执行，辅助进程和整个动作都有超时。失败和取消路径会尝试释放本次按下的键或按钮；Windows 同时检查 `SendInput` 实际插入的事件数量。输入事件发出不等于目标应用完成业务操作，提交、保存或跳转后仍需读取页面、窗口或截图确认结果。

窗口列表可能包括其他工作区或屏外窗口。macOS 列表限于当前可见普通窗口；不同平台的 `window_id` 格式不同，应完整复制当前返回值。失效或歧义诊断需要刷新窗口目录。

## 浏览器接口

`browser.launch` 使用独立临时 profile 和本机随机调试端口，默认显示窗口；`headless:true` 适合后台验证。`executable` 可指定系统浏览器路径，启动保留 Chromium 沙箱。它直接使用 [Chrome DevTools Protocol](https://chromedevtools.github.io/devtools-protocol/)。

```javascript
const id = "CLIENT_ID_FROM_LIST";
const started = await browser.launch(id, { headless: false });
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
