# 原生工具与按需 harness

ssmcp 默认启用命令执行、文件、会话、用户交互、工具目录和 Zuse CLI 管理，不要求安装 Claude Code、Codex、Pi 或 OpenCode。按任务使用实际注册的 `ssmcp.exec.*`、`ssmcp.fs.*` 和四个 `ssmcp.computer.*` 生命周期工具。

桌面和浏览器程序直接由独立 Zuse CLI 执行，经通用 exec 管理进程。SSMCP 不加载内置浏览器、桌面或 Playwright 专用后端，也不自动安装它们的运行组件。[Zuse CLI 管理](computer-use.md) · [Windows 安装范围](windows-browser-setup.md)

## 默认启动做什么

客户端启动时不扫描或启动上述四个外部 harness，不运行它们的 `--version`，也没有定时后台探测。工具目录只包含当前真实注册的能力；读目录、执行原生命令和读写文件不会顺带加载外部 harness。

`ssmcp.harnesses.list` 只读取缓存状态：

| 状态 | 含义 |
|---|---|
| `not_loaded` | 未请求加载，也未探测是否安装；`executable` 和 `version` 为 `null` |
| `loading` | 用户已明确请求该项，正在进行初始化 |
| `ready` | 已完成该项的原生握手和工具枚举 |
| `unavailable` | 该次显式加载失败，`reason` 保留具体原因 |

`not_loaded` 不表示“没有安装”，`ready` 也不表示已经调用模型或执行某个工具。

## 只加载需要的一项

启动时可用 `--load-harness codex` 或 `--load-harness claude-code` 加载一个协议。`ssmcp --list-harnesses` 列出支持的 provider、别名和 peer 参数。启动复用按需加载入口，先完成握手与工具枚举。完整参数与 MCP peer 连接见[协议加载](agent-entry.md)。

通过现有 `call` 调用原生入口，不需要新增 relay helper：

```js
const status = await call(clientId, "ssmcp.harnesses.list", {});
return status;
```

确实需要外部能力时，再明确选择 provider：

```js
const loaded = await call(clientId, "ssmcp.harnesses.load", {
  provider: "codex"
});
return loaded;
```

provider 接受 `codex`、`claude`、`pi`、`opencode`，以及启动时已明确配置 endpoint 和认证变量的 `ssmcp` peer。加载只处理选中的一项，不安装软件，不扫描其他 provider。成功后，再使用 `tools.search` 或客户端工具目录读取实际名称和完整 schema；不要预先编造工具名。

桌面与浏览器操作使用所选 Zuse CLI 的真实接口。已授权的外部 MCP 通用转发仍保留实际目录与原生审批；它不恢复已撤去的 SSMCP 浏览器工具。

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

实现沿用现有工具 registry 和连接协议。加载入口与并发控制见 [process/lazy.rs](../src/runtime/native_plugins/process/lazy.rs)，真实协议初始化和目录替换见 [process.rs](../src/runtime/native_plugins/process.rs)。
