# 原生工具与按需 harness

ssmcp 默认只启用自己的原生工具。命令执行、文件、会话、用户交互和工具目录不要求用户安装 Claude Code、Codex、Pi、OpenCode 或 Playwright MCP。优先使用已注册的 `ssmcp.exec.*`、`ssmcp.fs.*`、`ssmcp.browser.*` 等原生接口。

浏览器、桌面功能仍需要对应系统的实际运行组件，例如 Chromium 或 Linux 私有桌面的 Xvfb。这些运行组件与可选的外部 Agent/MCP harness 不同。Windows 浏览器组件准备见[安装说明](windows-browser-setup.md)。

## 默认启动做什么

客户端启动时不扫描或启动上述五个外部 harness，不运行它们的 `--version`，也没有定时后台探测。工具目录只包含当前真实注册的能力；读目录、执行原生命令和读写文件不会顺带加载外部 harness。

`ssmcp.harnesses.list` 只读取缓存状态：

| 状态 | 含义 |
|---|---|
| `not_loaded` | 未请求加载，也未探测是否安装；`executable` 和 `version` 为 `null` |
| `loading` | 用户已明确请求该项，正在进行初始化 |
| `ready` | 已完成该项的原生握手和工具枚举 |
| `unavailable` | 该次显式加载失败，`reason` 保留具体原因 |

`not_loaded` 不表示“没有安装”，`ready` 也不表示已经调用模型或执行某个工具。

## 只加载需要的一项

通过现有 `call` 调用原生入口，不需要新增 relay helper：

```js
const status = await call(clientId, "ssmcp.harnesses.list", {});
return status;
```

确实需要外部能力时，再明确选择 provider：

```js
const loaded = await call(clientId, "ssmcp.harnesses.load", {
  provider: "playwright"
});
return loaded;
```

provider 只接受 `codex`、`claude`、`pi`、`opencode`、`playwright`。加载只处理选中的一项，不安装软件，不扫描其他 provider。成功后，再使用 `tools.search` 或客户端工具目录读取实际名称和完整 schema；不要预先编造工具名。

普通浏览器任务应先使用原生 `ssmcp.browser.*`。只有需要已明确选定的浏览器扩展会话等外部能力时，才加载 Playwright MCP；加载、原生审批与浏览器配对分别按该工具的真实协议进行。

本机输入 `/cc`、`/codex` 或 `/pi` 也是明确使用该 harness 的请求。这些命令异步走同一个原生 load 入口，然后显示所选 provider 的真实目录、schema 或执行显式 JSON 调用。`ssmcp.ui.harness` 保持只读，它不会因查询而加载任何 provider。

## 缓存、重试与关闭

同一 provider 的并发首次加载共享一次初始化。一个等待者取消，不会取消其他等待者所需的初始化。成功和失败结果都在当前客户端连接中缓存；普通查询或后续默认 load 不会反复启动进程。

修正安装或配置后，显式刷新这一项：

```js
return await call(clientId, "ssmcp.harnesses.load", {
  provider: "claude",
  refresh: true
});
```

刷新只影响选中的 provider；未变化的原生工具仍可用。正在加载时的刷新请求共享同一个初始化，不并排启动第二个。客户端关闭会取消未完成的加载，关闭其拥有的协议进程并撤销注册，不在连接结束后发布新工具。

外部工具仍保留自己的 schema、会话和审批方式。加载协议桥不发起模型回合；显式调用模型接口时，才遵循该接口的原生行为。拼错工具名会明确拒绝，不会触发全量扫描或猜测另一个 harness。

实现沿用现有工具 registry 与 wire 7，不另建代理、消息队列或会话历史。加载入口与并发控制见 [backend/lazy.rs](../src/backend/lazy.rs)，真实协议初始化和目录替换见 [backend.rs](../src/backend.rs)。
