# 后台操作浏览器与独立桌面

ssmcp 将网页输入发送给浏览器协议，将 Linux 原生应用输入发送给独立的 Xvfb 显示。两条路径都可以在用户继续使用电脑时工作。需要直接操作用户当前桌面时，每次输入必须显式传入 `desktop: "shared"`。实现见[浏览器](../src/browser.rs)、[连续动作](../src/browser/actions.rs)与[独立桌面](../src/computer/isolated.rs)。

鼠标抢占来自自动化和用户共用同一套桌面输入状态。提高移动速度无法消除这种冲突。网页可以通过目标标签的协议接收输入；必须使用桌面输入的程序则运行在另一套显示会话里。这是本轮两种后台路径的设计依据。

## 配置浏览器环境

```sh
ssmcp setup browser --check
ssmcp setup browser --install
ssmcp setup browser --check --json
```

不带参数时只检查环境。`--install` 使用已有 Node.js 18+ 和 npm，把固定的 `@playwright/mcp@0.0.85` 安装到 ssmcp 用户数据目录，验证版本后替换旧安装；重复执行会复用已验证的同版本包。安装与普通浏览器 profile 分开，不修改全局 npm 包。Node.js、npm、浏览器或桌面辅助程序缺失时，检查结果给出对应原因和后续命令。[配置实现](../src/client/onboarding/browser_setup.rs)

| 检查状态 | 含义与下一步 |
| --- | --- |
| `needs_runtime` | 安装 Node.js 18+ 与 npm，再执行 `--install` |
| `needs_provider` | 执行 `--install` 安装或修复 Playwright MCP |
| `needs_extension` | 在要复用的 Chrome、Edge 或 Chromium profile 中安装并启用 Playwright Extension |
| `extension_update_required` | 扩展更新到 0.5.0 或以上 |
| `awaiting_pairing` | 服务握手与工具枚举完成，首次浏览器连接仍需原生配对确认 |

`--check` 检查安装清单和 MCP 服务，不连接用户标签页。它不会把“扩展文件存在”报告成“已经配对”。安装后重启或 reload ssmcp 客户端，让工具目录重新发现 `playwright.*`。[Playwright 适配器](../src/backend/playwright.rs)

## 复用当前浏览器与插件

先读取 `browser.discover` 和 `ssmcp.harnesses.list`，再从真实工具目录查找 `playwright.*`。Playwright 使用固定参数 `--extension --background --no-tab-group`：配对后的动作保持标签和窗口在后台，也保留用户原有标签分组。它沿用用户选择的浏览器会话、网站登录和扩展环境；标签仍由浏览器扩展管理所属客户端。[上游扩展说明](https://github.com/microsoft/playwright/blob/main/packages/extension/README.md)

```javascript
const client = clients.find(c => c.cwd === "/path/to/project");
if (!client) throw new Error("目标客户端离线");
return {
  browser: await browser.discover(client.id),
  providers: await call(client.id, "ssmcp.harnesses.list", {}),
  tools: await tools.search(client.id, {
    query: "playwright", limit: 10, include_schemas: true
  })
};
```

首次配对页可能显示在前台。上游要求扩展至少 0.5.0；连首次连接页都保持后台还需要正在运行的 Chrome 158.0.8127.2+，以及操作者从扩展界面主动配置的 `PLAYWRIGHT_MCP_EXTENSION_TOKEN`。ssmcp 不读取扩展内部凭据存储。后台网页仍会受到浏览器定时器节流影响。[上游后台模式要求](https://github.com/microsoft/playwright/blob/main/packages/extension/README.md#working-in-the-background)

ChatGPT 扩展的安装状态会显示在检查结果中。ChatGPT 官方 Browser 工具可以使用自身已建立的连接；ssmcp 的独立浏览器控制使用已公开的 Playwright 或 CDP 接口。其他插件只有实际提供并注册了工具接口才会进入可调用目录。[现有浏览器连接](browser-existing-session.md)

## 独立浏览器与连续动作

不需要现有账号会话时，`browser.launch(id,{})` 默认启动无头浏览器，使用独立临时 profile；`headless:false` 显式请求可见窗口。`browser.open` 默认 `background:true`。CDP 动作发送给目标标签，不操作系统指针。[接口实现](../src/browser.rs)

取得一次快照后，可以把填表、点击与等待合并为一次原生调用：

```javascript
const reply = await browser.act(clientId, {
  target_id: targetId,
  actions: [
    { type: "fill", ref: inputRefFromSnapshot, text: "需要输入的内容" },
    { type: "click", ref: buttonRefFromSnapshot },
    { type: "wait", text: "保存成功", timeout_ms: 5000 }
  ]
});
return reply;
```

`act` 最多接收 16 个动作，包含等待标签操作锁在内的总期限为 25 秒。整个批次持有同一标签的操作锁，其他标签可以独立运行。字段、类型、长度与数值范围在首次输入前整体校验；元素引用与选择器在对应动作执行时校验。默认返回完成步骤与新快照，新的快照会替换旧元素引用。[动作实现](../src/browser/actions.rs)

| 字段 | 处理方式 |
| --- | --- |
| `isError: false` | 读取 `structuredContent.completed` 和新 `snapshot` |
| `completed_count` | 已收到成功结果的动作数 |
| `failed_index` | 发生错误的动作索引，从 0 开始；该动作可能已经产生部分效果 |
| `phase: "queue"` | 获取目标或等待标签操作锁时失败，尚未开始本批输入 |
| `phase: "snapshot"` | 输入步骤已完成，随后获取快照失败 |
| `replay_safe: false` | 先观察页面，再决定下一步；不要重放整批输入 |

条件等待也可以单独使用：

```javascript
return await browser.wait(clientId, {
  target_id: targetId,
  selector: "#result",
  text: "保存成功",
  state: "visible",
  timeout_ms: 5000
});
```

`selector` 与 `text` 至少提供一个，同时提供时取交集。`state` 支持 `visible`、`hidden`、`attached`、`detached`，包含排队在内的期限为 1–10000 毫秒。可见元素必须拥有正面积的布局矩形并允许 CSS 可见显示。等待从宿主进程轮询条件，不依赖网页自己的计时器，也不会发送输入。

## Linux 独立桌面

原生桌面程序需要图形显示和一套输入设备。Xvfb 为自动化提供单独的显示，ssmcp 的鼠标、键盘、窗口列表和截图都指向它；用户正在使用的显示不受这些输入影响。辅助程序包括 `Xvfb`、`xauth`、`xdotool`、`xprop` 和 ImageMagick 7 的 `magick`。Openbox 可选；未安装时直接枚举、提升和聚焦可见窗口。

```javascript
const started = await computer.session_start(clientId, {
  width: 1280, height: 800
});
if (started.isError) return started;

return await computer.launch(clientId, {
  command: ["/absolute/path/to/application", "--new-instance"]
});
```

`command` 是参数数组，没有隐式 Shell 展开。`--new-instance` 只是示意，应采用目标程序实际支持的独立实例或 profile 参数。应用的显示环境、XDG 数据目录和会话总线地址与用户桌面分开。该能力隔离显示与应用实例，不限制应用访问当前用户有权读取的文件。

启动成功后，普通 `computer.windows`、`focus`、`click`、`key`、`type` 和 `screenshot` 自动使用私有显示，结果标记 `desktop: "isolated"` 和 `input_scope: "private_virtual_display"`。Xvfb 不监听 TCP；认证文件权限为 0600。停止或断开时清理拥有的应用、显示服务和临时文件。隔离会话未启动或异常退出时，默认输入报错；它不会改用共享桌面。

```javascript
const shot = await computer.screenshot(clientId, { max_width: 1280 });
if (shot.isError) return shot;
image(shot);
return shot.structuredContent;
```

```javascript
return await computer.session_stop(clientId);
```

直接控制用户当前桌面是明确的另一种调用。Linux、Windows 和 macOS 的现有原生适配器都保留这项能力：

```javascript
return await computer.click(clientId, {
  desktop: "shared", x: observedX, y: observedY
});
```

Windows/macOS 当前没有 Xvfb 隔离会话，调用 `session_start` 会说明平台限制。无隔离会话时，只读工具仍可观察共享桌面，返回的 `desktop` 字段说明实际目标。升级后，过去依赖默认共享输入的调用需要加上 `desktop: "shared"`；浏览器操作推荐继续使用浏览器协议。

## 验证入口

协议测试：`node --test scripts/test_cua_facade.mjs`。浏览器端到端测试：`python3 scripts/e2e_computer_use.py`。真实独立桌面测试：`cargo test --lib computer::isolated::tests::real_isolated_desktop_input_capture_and_cleanup -- --ignored --exact`。这些测试分别检查路由与错误结果、真实网页输入和条件等待、真实 X11 输入与进程清理。实际执行结果应附在对应版本的验收记录中。
