# Zuse CLI 管理与桌面、浏览器任务

SSMCP 的 `computer` 工具只管理独立的 Zuse CLI：查询版本、下载、安装和卸载。桌面与浏览器程序直接交给 Zuse CLI，通过通用 `exec` 运行。SSMCP 不内置桌面适配器、CDP 浏览器、Omarchy 动作、Playwright 专用桥或内部 Zuse worker，也不在调用失败时切换到这些实现。

本页说明当前源码的接口。已发布的旧客户端可能提供不同目录；先从 `list` 选定客户端，再读取它实际注册的工具和 schema。[管理器](../src/computer.rs) · [CLI 参数](../src/client/computer_control.rs) · [通用命令](code-mode.md#exec)

## 四个管理工具

每个 JavaScript helper 都以实际 `clientId` 为第一个参数，并返回完整原生 MCP 结果。检查 `isError`，再读取 `structuredContent`。

| JavaScript helper | 原生工具 | 参数 | 结果与边界 |
| --- | --- | --- | --- |
| `computer.version(id)` | `ssmcp.computer.version` | `{}` | 实际执行已选 CLI 的 `--version`，返回版本、路径、摘要与是否受管；未安装时返回 `available:false` |
| `computer.download(id, args)` | `ssmcp.computer.download` | 可选 `version` | 下载当前平台的官方稳定版，返回 `download_id` 和 release 身份；不执行、不安装 |
| `computer.install(id, args)` | `ssmcp.computer.install` | `download_id` | 重新核验受管下载，安装到私有目录，返回 `installation_id` 和 `path`；不修改 PATH |
| `computer.uninstall(id, args)` | `ssmcp.computer.uninstall` | `installation_id` | 只删除所有权、文件身份与字节仍匹配的受管安装 |

使用返回的原始 ID；下载 ID 和安装 ID 不能互换。参数不接受任意下载 URL、安装目标或待删除路径。普通 PATH、Cargo 和系统包管理器安装的 Zuse 不会被接管、覆盖或卸载。[工具 schema 与分派](../src/computer.rs) · [所有权与存储](../src/computer/storage.rs)

## 在本机终端管理

```sh
ssmcp computer version --json
ssmcp computer download --json
ssmcp computer install DOWNLOAD_ID_FROM_REPLY --json
ssmcp computer uninstall INSTALLATION_ID_FROM_REPLY --json
```

将示例中的 ID 占位符替换为前一步真实返回的值。省略子命令等同于 `version`。下载也可使用 `ssmcp computer download VERSION --json`，其中 VERSION 必须是稳定版的 `MAJOR.MINOR.PATCH` 或 `vMAJOR.MINOR.PATCH`。不指定版本时请求官方最新稳定版。

`--json` 将完整 MCP 结果作为单行 JSON 写入 stdout。操作失败时，结果含 `isError: true` 和错误说明，进程以非零状态退出。命令语法和管理器初始化错误以非零状态退出，诊断写入 stderr。未安装 Zuse 时，版本查询仍成功并返回 `available: false`。

省略 `--json` 时，成功命令输出结果正文。下载结果正文中读取 `download_id`，安装结果正文中读取 `installation_id` 与可执行文件 `path`。这些命令由调用方逐条执行。SSMCP 自身的版本仍由 `ssmcp --version` 查询。

## 版本、来源与平台

版本查询优先选择 PATH 中的 Zuse；没有 PATH 条目时，再查询该管理目录记录的受管安装。已找到的程序如果损坏、输出身份不符或超时，会返回真实错误，不悄悄改用另一个程序。查询具有三秒期限与输出上限，版本来自实际进程输出，不能用安装记录或 SSMCP 的版本代替。[版本探测](../src/computer/probe.rs)

下载来源限定为 [LIghtJUNction/zuse 的 GitHub Releases](https://github.com/LIghtJUNction/zuse/releases)。管理器核对稳定版本、平台、唯一资产、公布的大小和 SHA-256，再校验下载字节；缺失摘要、平台资产或合法身份时明确拒绝。下载不执行归档中的程序，安装也不启动桌面、浏览器或模型。[发布资产选择](../src/computer/release.rs) · [归档解包](../src/computer/archive.rs)

当前配置的 Zuse 下载目标为 Linux GNU x86_64 / aarch64、macOS x86_64 / aarch64，以及 Windows x86_64。SSMCP 自身支持 Windows ARM64，不代表当前 Zuse 管理器已有对应官方资产；未配置的平台明确返回不支持，不改选其他架构。实际可下载与否还取决于所选官方 release 是否提供该平台资产。

## 直接运行 Zuse

先查询安装，再使用真实返回的绝对路径。下面只读取该 CLI 的帮助：

```js
const reply = await computer.version(clientId);
if (reply.isError) return reply;
const cli = reply.structuredContent;
if (!cli.available) return cli;
return await exec.run(clientId, {
  command: [cli.path, "--help"],
  wait_ms: 5000,
  timeout_ms: 10000,
  max_bytes: 16384
});
```

桌面或浏览器任务的程序与参数以所选 Zuse 版本的 `--help`、`schema` 和 [Zuse 使用指南](https://github.com/LIghtJUNction/zuse#readme) 为准。读取当前目标状态、确认操作范围后，直接运行工作区内准备好的程序：

```js
const job = await exec.start(clientId, {
  command: [zusePath, "run", "automation.zuse"],
  cwd: projectDirectory,
  timeout_ms: 60000
});
return job;
```

这里的 `zusePath`、`projectDirectory` 和脚本必须来自实际查询及当前任务。这个调用执行独立程序；`computer` 工具不解析脚本，也没有动作代理。依赖同一页面或输入状态的操作应在同一 Zuse 程序中按顺序组织。不要把多个独立命令默认视为共享浏览器状态。[命令生命周期](code-mode.md#exec) · [浏览器与目标选择](browser-desktop.md)

保存 `job_id`，通过 `exec.read/wait` 读取实际 stdout、stderr、退出码和完整性。停止时对该任务使用 `exec.cancel`。退出或取消只证明相应进程的状态；已送达的页面或桌面操作可能已经生效，应重新观察目标后再决定是否重试。截图与其他产物按 Zuse 的实际输出处理，不能把普通文本当作 MCP 图片。[输出读取](code-mode.md#outputs)

## 从旧接口迁移

旧的 `ssmcp.browser.*`、`ssmcp.omarchy.*`、`cua` / `browser` / `omarchy` 便捷命名空间，以及 `computer` 中的截图、键鼠、窗口和私有桌面动作已撤去。`ssmcp setup browser`、Playwright 专用加载与内部 worker 入口也不再提供。发现这些旧调用时，改写为独立 Zuse 程序和通用命令调用。

旧控制开关、兼容入口和默认 `Super + Alt + C` 快捷键已移除。独立 Zuse CLI 无需 takeover 前置流程。普通 exec 仍使用启动客户端的操作系统权限；SSMCP 不宣称能够撤销同一系统用户通过其他 Shell 或程序获得的桌面权限。需要操作系统隔离时，应配置明确的账户、会话或沙箱边界。

OAuth 登录在浏览器中打开授权页、用户显式打开链接，以及已授权外部 MCP 的通用转发仍保留；这些能力不恢复旧内置动作工具。历史实机报告只记录当时实现，不作为这次迁移的验证成绩。
