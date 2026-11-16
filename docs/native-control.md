# 原生电脑与浏览器控制

ssmcp 在客户端内提供电脑和浏览器控制。连接后，使用原有的 `list` 发现 `ssmcp.computer.*`、`ssmcp.browser.*`，通过 `code_mode` 调用。操作在开发机执行，截图和结构化结果沿原有 SSH 连接返回。

电脑控制把鼠标、键盘、窗口和截图请求交给当前桌面会话的系统接口。浏览器控制由 Rust 直接使用 Chrome DevTools Protocol（CDP），连接系统安装的 Chromium、Chrome 或 Edge。两组能力不调用模型，不需要启动其他 MCP 服务器，也不需要选择 `-t` 模式。

## 能力与运行条件

| 能力 | 当前实现 | 环境要求 |
| --- | --- | --- |
| 浏览器 | 启动、附着、标签页、导航、页面快照、元素点击与填表、键盘、滚动、执行页面 JavaScript、截图 | 已安装支持 CDP 的 Chromium 系浏览器 |
| Hyprland 桌面 | 显示器、窗口、聚焦、截图、移动、点击、拖拽、键盘、文字、滚轮 | `hyprctl`、`grim`、`wtype`、已运行的 `ydotoold` 与 `ydotool` |
| X11 桌面 | 同组桌面操作 | `xdotool`、`xprop`、ImageMagick；聚焦要求窗口管理器支持 |
| 其他 Wayland 桌面 | 按会话协议报告可用的键盘、当前指针点击和滚轮能力 | 以 `computer.status` 的逐项诊断为准 |
| macOS / Windows 桌面 | 返回明确的不可用诊断 | 当前没有桌面输入与截图适配器；浏览器 CDP 接口可单独使用 |

Firefox 的原生浏览器协议没有接入。页面快照读取主文档；跨域 iframe、浏览器工具栏和系统对话框可使用受支持桌面会话的截图与输入能力处理。

客户端会探测已安装依赖，不自动安装软件、修改桌面权限或启动特权输入服务。Hyprland 支持旧 dispatcher 和新版 Lua dispatcher。旧服务器使用新客户端时仍可发现原生工具，并可通过 `call(id, "ssmcp.browser.status", {})` 这样的原生调用使用它们。`computer`、`browser` 便捷方法及 `image()` 图片显示通道需要同时更新服务端。

## 发现与调用

先调用 `list({})`，从名称、任务标题和工作目录选择目标客户端，再按 ID 调用 `list({client_id})`。目录可能分页，继续传入返回的 `nextCursor`，不要把显示名称当成连接身份。

以下源码放入 `code_mode` 的 `code` 参数：

```javascript
const id = "CLIENT_ID_FROM_LIST";
const desktop = await computer.status(id);
const browserState = await browser.status(id);
return { desktop, browser: browserState };
```

便捷方法全部返回完整 MCP 原生结果：检查 `isError`，从 `structuredContent` 读取结构化数据。也可以使用目录中的完整名称：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.computer.status", {});
```


## 显示截图

`image()` 把原生截图作为真正的 MCP 图片内容块返回。截图元信息单独作为 JSON 返回，图片编码不会重复写入文本日志。

```javascript
const shot = await computer.screenshot("CLIENT_ID_FROM_LIST", {
  max_width: 1600,
  quality: 70
});
image(shot);
return shot.structuredContent;
```


捕获单个窗口时，先通过 `computer.windows` 读取它的 `x`、`y`、`width` 和 `height`，把这些字段作为 `region`。每张截图最多 400 KiB，会在必要时降低尺寸；每次 `code_mode` 最多显示 4 张图片，Base64 总量最多 2 MiB。需要更多图片时分次调用。

桌面输入使用全局逻辑坐标。显示器缩放、截图裁剪或缩小后，应使用返回的 `image_to_desktop` 映射：

- `desktop_x = origin_x + image_x * scale_x`
- `desktop_y = origin_y + image_y * scale_y`

例如截图上的横坐标是 400，`origin_x=100`、`scale_x=1.25`，则传给鼠标工具的横坐标是 600。每次窗口移动或显示布局变化后重新观察。

## 电脑接口

| 方法 | 主要参数 | 返回或效果 |
| --- | --- | --- |
| `computer.status` | 无 | 会话类型、逐项能力、依赖与失败原因 |
| `computer.monitors` | 无 | 逻辑尺寸、物理像素、缩放与位置 |
| `computer.windows` | 无 | 最多 100 个窗口及 `window_id`、标题、几何信息 |
| `computer.focus` | `window_id` | 聚焦当前窗口目录中的目标 |
| `computer.screenshot` | 可选 `region`、`max_width`、`quality`、`cursor` | JPEG 图片和坐标映射 |
| `computer.move` | `x, y` | 移动鼠标 |
| `computer.click` | 可选 `x, y`、`button`、`count` | 点击，`x`、`y` 需一起传入 |
| `computer.drag` | `path:[{x,y}, ...]`、可选 `button`、`duration_ms` | 经过 2–64 个点，结束时释放按钮 |
| `computer.key` | `key`、可选 `modifiers` | XKB 键名；修饰键为 `ctrl/alt/shift/super` |
| `computer.type` | `text` | 输入原样 UTF-8 文字，最多 16 KiB |
| `computer.scroll` | `direction`、可选 `steps`、`x,y` | 四方向滚轮 |

Hyprland 的 `focus` 只发送一次聚焦动作，随后最多等待 500 毫秒，读取实际前台窗口地址。只有目标地址匹配才报告成功；不匹配或查询挂起会返回明确错误。焦点结果只对应观察时刻，后续应用或人工操作仍可能改变它。

窗口目录可能包含其他工作区或屏外窗口。聚焦后重新读取窗口和显示器几何信息，再选择可见区域；截图会拒绝超出桌面边界的裁剪。网页输入框的 DOM 焦点与系统前台窗口属于两个层次：需要物理键盘输入时，确认前台窗口，并用电脑点击激活目标输入区。

动作按客户端串行执行。单个辅助进程和整个动作均有超时。鼠标和键盘的失败、取消路径会尝试释放已按下的输入；结果中的失败信息应原样处理，刷新画面后再决定下一步。

## 浏览器接口

`browser.launch` 使用独立临时 profile 和本机随机调试端口。默认显示窗口，传入 `headless:true` 可用于自动化验证。可以用 `executable` 显式指定系统浏览器路径；启动过程保留 Chromium 沙箱。

```javascript
const id = "CLIENT_ID_FROM_LIST";
const started = await browser.launch(id, { headless: false });
if (started.isError) throw new Error(JSON.stringify(started));
const opened = await browser.open(id, { url: "https://example.com" });
if (opened.isError) throw new Error(JSON.stringify(opened));
const target_id = opened.structuredContent.target_id;
return await browser.snapshot(id, { target_id });
```


快照返回主文档文本、页面标题、URL 和元素数组。每个元素包含观察得到的 `ref`。使用该引用点击或填表，避免猜测 CSS 选择器：

```javascript
return await browser.fill("CLIENT_ID_FROM_LIST", {
  target_id: "TARGET_ID_FROM_OPEN_OR_TABS",
  ref: "REF_FROM_CURRENT_SNAPSHOT",
  text: "需要输入的文字"
});
```


元素引用绑定目标标签页与当前快照。导航、元素移除或新快照会使旧引用失效；收到相关错误后重新调用 `snapshot`。快照隐藏密码输入框的值。点击会检查可见性、禁用状态与遮挡。

快照按完整 MCP 结果限制为 24 KiB，并把返回字符串限制为 400 行，避免被通用输出存储转换为文件引用。中文按 UTF-8 边界裁剪；正文、元素或元信息被裁剪时返回相应标记，只有实际返回的元素引用可以继续操作。

| 方法 | 主要参数 | 返回或效果 |
| --- | --- | --- |
| `browser.status` | 无 | 连接、所有权与版本信息 |
| `browser.launch` | 可选 `executable, headless, width, height` | 启动连接自有的浏览器 |
| `browser.connect` | `endpoint` | 附着到已开启调试的本机浏览器 |
| `browser.tabs` | 无 | 当前标签页的 `target_id`、URL、标题 |
| `browser.open` | `url` | 打开标签页，返回 `target_id` |
| `browser.navigate` | `target_id, url` | 导航并等待对应文档 |
| `browser.snapshot` | `target_id`；可选 `max_elements, max_chars` | 有界页面文本与元素 refs |
| `browser.click` | `target_id, ref`；可选 `button, click_count` | CDP 鼠标点击 |
| `browser.fill` | `target_id, ref, text` | 填写或清空输入内容 |
| `browser.key` | `target_id, key`；可选 `modifiers` | 浏览器键名，如 `Enter`；修饰键为 `Control/Shift/Alt/Meta` |
| `browser.scroll` | `target_id`；可选 `delta_x, delta_y` | CSS 像素增量滚动 |
| `browser.evaluate` | `target_id, expression` | 执行页面 JavaScript，返回有界 JSON |
| `browser.screenshot` | `target_id`；可选 `max_width` | 视口 JPEG 截图 |
| `browser.close` | `target_id` | 关闭选中标签页 |
| `browser.disconnect` | 无 | 关闭本次控制连接 |

附着地址只接受使用回环 IP 字面量的 HTTP 地址，例如 `http://127.0.0.1:9222` 或 `http://[::1]:9222`，不接受 `localhost`；返回的 WebSocket 端点必须属于同一地址与端口。不会通过环境代理转发或跟随重定向。已有普通浏览器必须先由操作者开启调试接口，ssmcp 不会读取浏览器 profile 来接管会话。

有界面页面如果处于屏外或没有继续绘制，截图可能超时。交互操作时先把目标带回可见区域；后台自动化验证可以使用 `headless:true`，本轮已验证该路径的实际 JPEG 返回。

断开或客户端退出会结束由 `launch` 创建的浏览器并清理临时 profile。通过 `connect` 附着的浏览器由操作者管理，ssmcp 断开后保留它。桌面原语、浏览器调用与图片传输均使用原有客户端访问控制；审计事件记录工具名、耗时和完成状态。

## 验证

在已安装系统 Chromium 的 Linux 开发机运行：

```sh
cargo build --locked --bins
python3 scripts/e2e_native_control.py --report target/native-control-evidence/headless.json
python3 scripts/e2e_native_control.py --desktop --report target/native-control-evidence/desktop.json
```


第二条命令使用无界面浏览器；第三条会在当前桌面打开临时测试页，操作该测试窗口并截图。测试通过真实 OpenSSH、OAuth PKCE 和 HTTP MCP，检查工具发现、中文填表与清空、按键、点击、滚动、图片内容块、过期引用、进程所有权、桌面缩放和审计分页。页面仅由本机回环服务器提供。

具体版本、实机结果和证据位置见 [原生控制验收记录](validation-native-control.md)。接口参数的最终依据是当前客户端返回的 schema。
