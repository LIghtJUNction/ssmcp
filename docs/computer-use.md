# Zuse CLI 管理与桌面、浏览器任务

SSMCP 的四个远程 `computer` 工具管理独立 Zuse CLI。本机命令可调用该 CLI 观察明确指定的窗口或页面容器。SSMCP 不内置桌面适配器、CDP 浏览器、Omarchy 动作、Playwright 专用桥或内部 Zuse worker，也不在调用失败时切换到这些实现。

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

`--json` 输出完整 MCP 结果；默认输出结果正文。下载结果正文中读取 `download_id`，安装结果正文中读取 `installation_id` 与可执行文件 `path`。这些命令不会自动把后续步骤串起来。SSMCP 自身的版本仍由 `ssmcp --version` 查询。

## 本机限定目标观察

`ssmcp computer check` 使用实际选中的 CLI 读取 `--version` 和 `schema`，然后用无桌面 I/O 的计时器测试 `eval`、从标准输入读取的 `run -` 和保留状态的 `shell`。观察适配器支持从 0.4.2 起的 0.4.x 合同。每次观察会核对该操作的参数类型、必填项与只读标记；CLI 缺失、版本不匹配、进程退出失败或回执不完整都会给出错误。

在本机显式授权后，每次调用指定一个目标。下面的大写参数须替换为已确认的实际值：

```sh
ssmcp computer observation status
ssmcp computer observation on
ssmcp computer locate-window WINDOW_ID "EXACT_LABEL"
ssmcp computer observe-tab LOOPBACK_PORT PAGE_TARGET_ID "CSS_CONTAINER"
ssmcp computer observation off
```

- `locate-window` 通过 `eval --read-only` 调用 `target.locate`。窗口必须有明确的原生 ID，并能由 Zuse 验证映射状态、进程身份和唯一节点。回执还须与请求的窗口 ID 和标签一致。
- `observe-tab` 使用 `run -` 将固定程序送给 CLI。它只连接 `127.0.0.1` 上指定页面的 DevTools WebSocket，读取一个唯一匹配的 DOM 容器并断开连接。Zuse 的连接回执可能含有标签页列表，SSMCP 只输出所选容器。端口必须属于已确认的本机浏览器。
- 观察授权有独立的默认关闭记录。它与旧版键鼠许可分别存储。每个请求绑定授权时的代次，关闭后再开启也不能恢复旧请求。用 Ctrl+C 取消单次调用，用 `observation off` 撤销本机观察；显式 `ssmcp computer off` 或 `/computer off` 同时关闭两套许可。四个远程管理工具的目录保持原样。
- 当前 CLI 没有旧内置库的逐次输入派发许可回调。需要该保证的键鼠输入、后台点击和等待后自动输入仍待接入。普通 exec 作业具有独立的 OS 权限，不能用来绕过此许可。

[本机调用与回执校验](../src/computer/inspect.rs) · [观察许可与撤销](../src/computer_control.rs)

## 兼容旧版 SSMCP 的手动开关

默认 `Super + Alt + C` 快捷键已移除。以下入口保留为显式、手动的旧版兼容命令：

```sh
ssmcp computer status --json
ssmcp computer off
ssmcp computer on
ssmcp computer toggle
```

聊天中使用 `/computer [status|on|off|toggle]`，省略参数时查看状态；CLI 的 `ssmcp computer` 默认仍是 Zuse `version`。兼容命令不会安装全局按键绑定，也没有新增 `computer.on/off/toggle/status` 原生工具。四个管理工具保持不变，`ssmcp.ui.get` 中的 `computer` 仅是只读快照，远程 `ui.action` 不提供这些开关动作。[本地解析](../src/client/computer_control.rs) · [手动 UI 接线](../src/client/interface/computer.rs)

兼容状态只供仍运行且遵守旧门禁的 SSMCP 版本读取。Unix 使用 `/var/tmp/ssmcp-host-input-<有效 UID>/state.json`，Windows 使用 `%LOCALAPPDATA%/ssmcp/host-input/state.json`；继续使用原来的 schema 1、`enabled`、16 字节 `generation` 和稳定文件锁协议。状态改变会更新代次。旧动作消费者仍自行持有 `input.lock`；兼容开关不替换或夺取这个锁，发布 Off 不必等旧输入 lease 释放。[兼容核心](../src/computer_control.rs) · [原协议读写](../src/computer_control/storage.rs)

`status` 不创建目录、不初始化默认 On，也不改变已有状态。无有效旧记录时返回 `available:false, mode:"unset"`；读取或安全检查失败时返回 `mode:"unavailable"` 和实际错误。只有显式 on/off/toggle 才写状态；记录缺失时，toggle 按旧缺失记录的 Off 值切换为 On，不冒充此前已有有效状态。损坏记录不能被 On 或 toggle 静默接受，显式 Off 可以按原协议修复损坏内容，仍须通过所有权、权限和文件身份检查。

所有兼容输出都标明 `compatibility_only:true`、`controls_this_version:false`、`standalone_zuse_controlled:false`。当前版本没有这些旧动作消费者；普通 exec 和独立 Zuse CLI 不读这个开关，常规使用无需先运行 On 或取得 takeover。显式 Off 也会撤销本机限定目标观察，结果包含 `scoped_observations_disabled:true`。旧状态成功发布不证明正在执行的旧操作已经全部退出、已送达的动作被撤销，或独立 CLI 已停止。

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

旧的 `ssmcp.browser.*`、`ssmcp.omarchy.*`、`cua` / `browser` / `omarchy` 便捷命名空间，以及 `computer` 中的截图、键鼠、窗口和私有桌面动作已撤去。`ssmcp setup browser`、Playwright 专用加载与内部 worker 入口也不再提供。迁移旧调用时，先核对目标与授权。需要原有逐次输入许可的动作仍待 CLI 支持。

显式手动 Off 同时关闭上文两套许可，默认快捷键已移除。普通 exec 仍使用启动客户端的操作系统权限；SSMCP 不宣称能够撤销同一系统用户通过其他 Shell 或程序获得的桌面权限。需要操作系统隔离时，应配置明确的账户、会话或沙箱边界。

OAuth 登录在浏览器中打开授权页、用户显式打开链接，以及已授权外部 MCP 的通用转发仍保留；这些能力不恢复旧内置动作工具。历史实机报告只记录当时实现，不作为这次迁移的验证成绩。
