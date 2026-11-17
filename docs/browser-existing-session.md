# 复用正在使用的 Chrome

用户指定现有 Chrome 时，先确认那一个浏览器的可调用接口，再复用它的标签页和会话。`browser.launch` 创建独立临时 profile，不用于替代这次指定的现有会话；发现失败也不能改用 Chromium 或其他浏览器家族。[原生浏览器实现](../src/browser.rs)

## 先配置运行环境

运行 `ssmcp setup browser --check` 查看 Node/npm、扩展和 provider 状态；显式 `ssmcp setup browser --install` 安装并验证专用 Playwright MCP。适配器默认传入 `--extension --background --no-tab-group`，配对后的动作保留用户前台窗口和标签。首次浏览器原生配对页可能显示在前台。[后台操作与环境配置](background-control.md)

## 先看连接条件

通过 `list` 选定开发机客户端 ID，然后在 `code_mode` 中读取两类证据：

```javascript
const id = "CLIENT_ID_FROM_LIST";
const [browser, providers] = await Promise.all([
  call(id, "ssmcp.browser.discover", {}),
  call(id, "ssmcp.harnesses.list", {})
]);
return { browser, providers };
```

`ssmcp.browser.discover` 只读浏览器、扩展和本机入口元数据，不启动、重启或连接浏览器。扩展清单存在只证明安装文件存在，不能证明启用、配对或授权。`ssmcp.harnesses.list` 报告原生提供者的实际发现状态；CLI 存在、MCP 服务握手成功和浏览器已连接是三个不同结果。[发现与注册](../src/backend/mod.rs)

| 可用证据 | 后续操作 |
| --- | --- |
| 已观察到现有 Chrome 的回环 CDP 端点 | 用 `browser.connect` 显式附着，再读取 tabs/snapshot；断开不终止外部浏览器 |
| Playwright MCP 原生握手与工具枚举成功 | 查询实际 `playwright.*` Schema，再用 `call` 转发；原生扩展确认仍需完成 |
| 只有扩展安装文件，缺少服务入口或连接 | 报告具体缺口，运行显式 setup 安装或修复服务；发现流程不下载安装或创建替代浏览器 |
| ChatGPT 浏览器连接要求 originating context | 在发起操作的浏览器上下文中完成原生连接；不能声称 ssmcp 已经复用该扩展 |
| Claude 原生接口未枚举浏览器工具 | 不把扩展安装视为可调用 API，也不调用 Claude 模型来代为控制 |

## 找到实际工具，再调用

Playwright 适配器优先使用 setup 安装的专用 Playwright MCP，其次使用已有的 `playwright-mcp`；操作者也可通过 `SSMCP_PLAYWRIGHT_MCP=/absolute/path/to/already-installed/cli.js` 指定入口。JavaScript 入口由已验证的真实 Node.js 执行；适配器先检查版本，再以固定 `--extension --background --no-tab-group` 模式执行 MCP 握手和分页枚举。发现不运行 `npx`、`npm`、`corepack` 或 mise shim 去安装依赖。[Playwright 适配器](../src/backend/playwright.rs)

```javascript
const id = "CLIENT_ID_FROM_LIST";
return await tools.search(id, {
  query: "playwright",
  limit: 10,
  include_schemas: true
});
```

使用返回的确切名称与 Schema，不拼接猜测的工具名。MCP 服务可以先提供目录，随后在实际调用时要求浏览器连接或批准；成功枚举不代表已控制标签页。原生事件和待处理请求可通过实际注册的 `ssmcp.playwright.events/requests/respond` 观察与回应，不能自动代替用户批准。

```javascript
return await call("CLIENT_ID_FROM_LIST", "EXACT_TOOL_NAME_FROM_CATALOG", {
  // 按该工具实际 inputSchema 填写参数。
});
```

`call` 保留完整原生 MCP 结果及错误；先检查 `isError`、`structuredContent` 和审批状态。超出内联阈值的结果使用已有输出存储策略，按返回的 artifact 信息分页读取。ssmcp 不读取扩展 token 存储、不生成 token，也不通过 Harness 模型代理浏览器操作。[结果与输出](backends.md#3-用-code_mode-编排调用)

## 减少重复工作

每次 `code_mode` 请求从当前 `clients` 选择目标连接。reload 或看门狗重连会生成新 ID，旧 ID 和目录游标不能继续使用。写操作超时表示结果尚未确认，先检查任务状态或目标文件，再决定是否重试。

`tools.search` 默认只返回匹配工具的名称、路由命名空间和短说明；需要参数时再用 `include_schemas:true` 返回整个原生 descriptor。它减少模型上下文中的无关内容和手工分页往返，首次检索在底层仍需读取原生目录页。保留返回的 `nextCursor` 并保持同一查询选项；完整边界见[目录检索](backends.md#按需查询目录)。

CDP 页面操作按标签页串行，不同标签页可并发读取。对同一页的导航、快照和基于 ref 的输入，按依赖顺序等待；页面变化后重新取得引用。优先使用有界文本与元素快照，只有视觉判断需要时才请求截图。并发测试只证明隔离与顺序，不能据此报告未经测量的速度倍率。[浏览器接口](computer-use.md#浏览器接口)

## 2026-11-17 开发机观察

以下记录来自 Asia/Taipei 时间的本机只读检查，只描述当时这台开发机，不是产品固定版本要求，也不是新版本上线声明。[脱敏清单](reports/browser-existing-session-20261117-inventory.json)

| 项目 | 当次观察 |
| --- | --- |
| 正在运行的 Google Chrome | `157.0.8103.78`；未观察到 CDP 调试参数或 DevToolsActivePort |
| ChatGPT 扩展 | `1.26.1102.11740` 安装清单存在；当前 ChatGPT 工具返回 originating-context 不可用 |
| Playwright Extension | `0.5.0` 安装清单存在；未验证实际授权连接 |
| Playwright MCP server | 在有界已安装入口检查中未找到 `@playwright/mcp`；系统 Playwright `1.65.0` 的 test MCP 不是扩展控制服务 |
| Claude | 已观察的 Chrome profile 与 load-extension 路径中未找到官方扩展；Claude Code native host 文件存在，不等于握手成功 |

`tools.search` 运行在 broker worker，必须更新服务端；`ssmcp.browser.discover` 和 Playwright 适配器运行在客户端，必须更新客户端并重新发现能力。代码提交、本地测试、发布和线上部署分别记录，不能因为帮助文案已更新就宣称线上已经支持这些入口。
