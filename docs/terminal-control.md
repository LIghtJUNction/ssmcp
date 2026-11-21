# 终端、并行任务与控制者

`code_mode` 提供独立命令任务、多终端和 Zuse CLI 管理；电脑操作直接通过 exec 运行独立 Zuse 程序。先调用 `list` 选择客户端，再读取该客户端的工具目录；旧连接只提供它实际注册的能力。

| 需要完成的工作 | 原语 | 状态保留范围 |
| --- | --- | --- |
| 获取退出码，执行构建、检查或脚本 | `exec.run` | 短等待后可继续读取同一 job |
| 同时执行一组有结束时间的命令 | `exec.parallel` | 返回全部结果；按真实运行状态限制并发 |
| 启动开发服务器或持续任务 | `exec.start` | 保留到任务完成、取消或客户端断开 |
| 运行交互程序，保留 shell 环境 | `terminal.open/write/read` | 每个 terminal 保留自己的 shell 和画面 |
| 操作电脑或浏览器 | 通过 `exec.start/run` 调用独立 Zuse CLI | 状态由所选 CLI 进程维护；SSMCP 保留任务和输出 |

## 前台观察与终端输入

默认 `chat` 布局连续显示执行记录，底部输入框用于追加用户要求；`/queue` 管理队列，`/agents` 选择接收者，`/audit` 浏览源码。Tab 切换输入与内容焦点，内容区中的 ↑/↓ 用于选择，Enter 或 → 展开审计，← 返回列表。滚轮和 PgUp/PgDn 直接滚动当前内容，Ctrl-End 返回最新日志；`/filter 文本` 搜索，F9 切换类别。[客户端 UI](client-ui.md)

仪表盘中的用户要求与本地命令不会写入后台 PTY。要求队列由支持协议 v7 的连接交付给符合目标的 `code_mode` 响应；交互式终端仍由 `terminal.*` 管理。自己的前端可以读取输出、写入键盘字节和调整尺寸。在另一个本地终端运行 `ssmcp -i`，可以选择已授权设备并直接使用它的 PTY；Ctrl-] 离开，Ctrl-C 交给远端程序。

`ssmcp attach NAME` 用于观察本机保存的后台会话，目前不转发 attach 自己的输入框队列。启用仪表盘时它不会把普通输入交给隐藏终端；需要直接交互时使用 `ssmcp -i`，或为 attach 使用 `dashboard = false` 的配置。后台要求可通过该连接的 `ssmcp.ui.queue` 管理。退出 attach 不会停止后台会话。

## 并行命令

每条命令使用独立进程和 stdout/stderr 管道。命令之间互不共享交互式 shell 的当前目录或环境变量；工作目录默认是客户端共享根目录。需要管道、通配符或重定向时，显式执行本机 shell。

```javascript
const id = clients.find(c => c.id === "CLIENT_ID_FROM_LIST")?.id;
if (!id) throw new Error("先通过 list 选择客户端");
return await exec.parallel(id, [
  {command: ["git", "status", "--short"], wait_ms: 0},
  {command: ["git", "diff", "--stat"], wait_ms: 0},
  {command: ["git", "log", "-3", "--oneline"], wait_ms: 0}
], {concurrency: 2});
```

concurrency 的默认值为 4，允许 1–8；一次最多提交 32 条命令。一个 worker 等到当前命令实际结束才启动下一条，因此即使 wait_ms 为 0，运行中的本批命令也不会突破所设上限。初次短等待后，内部以最多 60 秒的等待继续观察。结果按输入顺序返回，逐项包含 fulfilled/value 或 rejected/reason；退出码非零仍需由调用者检查 value.exit_code。

观察失败时，结果尽可能保留 job_id，该 worker 停止启动新任务。未启动的项目标记 not_started:true，可以和“已启动、状态待确认”的项目分开处理。每次 code_mode 仍有 128 次原生调用预算和总时限；外层超时后，已启动任务继续运行，使用 exec.list/status/read 接管，不重复提交整批命令。持续服务使用 exec.start，避免长期占据 parallel 的 worker。

## 后台任务与输入管道

exec.start 立即返回 job_id，后续 code_mode 可以继续读取、等待或取消。每个客户端最多同时运行 32 个命令，保存最近 128 个记录；运行中的任务不会被历史淘汰。客户端断开或 reload 时会停止它拥有的任务。

默认 stdin 为 null。需要给进程输入时指定 stdin:"pipe"：

```javascript
const job = await exec.start(clientId, {
  command: ["python3", "-c", "import sys; print(sys.stdin.read().upper())"],
  stdin: "pipe"
});
const input = await exec.write(clientId, job.job_id, {
  data: "hello\n你好\n",
  close_stdin: true
});
const status = await exec.wait(clientId, job.job_id, {wait_ms: 5000});
return {input, status, output: await exec.read(clientId, job.job_id)};
```

exec.write 接受 data（UTF-8）或 data_base64（二选一），单次最多 32 KiB 原始字节。close_stdin:true 在全部提交的字节写完后发送 EOF。结果包含 written、remaining_bytes、timed_out、closed 和累计 stdin 状态；部分写入后等待超时，应只补写剩余字节。取消、执行期限和自然退出会唤醒堵塞或排队的输入操作。审计日志记录输入字节数和状态，不复制输入正文。

exec.read 的 stdout_cursor 和 stderr_cursor 分别推进；完整长输出保存在返回的 artifact 中。exec.cancel 终止任务拥有的进程树，exec.forget 释放已结束记录。

## 多个独立终端

每条连接保留 main 终端，并可创建其他交互式终端，总计最多 16 个。每个终端拥有独立 shell、输入、输出游标和启动目录。它们在后台保留状态，不占据默认仪表盘的显示区域；选择其他终端不会暂停已有进程。

```javascript
const [build, logs] = await Promise.all([
  terminal.open(clientId, {name: "build", cwd: "."}),
  terminal.open(clientId, {name: "logs", cwd: "."})
]);
await terminal.write(clientId, {
  terminal_id: build.terminal_id,
  data: "pwd\n"
});
await terminal.select(clientId, {terminal_id: build.terminal_id});
return {
  terminals: await terminal.list(clientId),
  output: await terminal.read(clientId, {
    terminal_id: build.terminal_id,
    cursor: 0,
    max_bytes: 16384,
    wait_ms: 1000
  })
};
```

`terminal.select` 更新所选终端状态，默认审计界面保持原有布局。自定义前端根据 `terminal.list` 的 ID 选择要呈现的终端，显式把同一个 `terminal_id` 传给 read、write 和 resize；终端字节交给终端解析器处理，不能直接当作普通日志文本。

| 方法 | 参数与行为 |
| --- | --- |
| terminal.open | name、cwd、select 均可省略；cwd 必须在共享根目录内 |
| terminal.list | 返回全部终端、selected、default_terminal_id 和上限 |
| terminal.select | 指定 terminal_id，更新所选 PTY；不会打开默认仪表盘中的 Shell 区域 |
| terminal.write | 指定 terminal_id、data；追加换行执行命令 |
| terminal.read | 指定 terminal_id、cursor、max_bytes、wait_ms；沿用返回的 next_cursor |
| terminal.resize | 指定 terminal_id、rows、cols |
| terminal.status | 返回指定终端的进程与输出状态 |
| terminal.close | 停止并释放额外终端，重复关闭安全；关闭当前终端后回到 main |

省略 terminal_id 的旧 read/write/status/resize 调用始终操作 main，选择其他终端不会改变旧脚本的目标。main 随客户端连接结束。列表中的 cwd 是启动目录；在 shell 内执行 cd 后，使用 pwd 获取动态位置。

## 源码审计与执行结果

客户端与服务器都支持协议 v6 或更新版本时，审计区和 `ssmcp.audit.events` 可以读取调用方提交的 `code_mode.code`、运行 ID、原生调用及编排结束状态。源码先经过审计脱敏，仅发送给本次编排实际访问的客户端；未被访问的在线设备不会收到。界面中的源码高亮和浏览不执行源码。

编排结束不代表命令进程已经退出。例如 `exec.start` 返回 job_id 后，`code_mode` 可以立即返回；后台命令继续由 `exec.status/read/wait` 观察。stdout、stderr、进程退出码和耗时分别显示，源码中的某个赋值表达式或输出 JSON 的 `exit_code` 字段不会替代真实退出状态。

`ssmcp.audit.events` 与 `exec.events` 是独立事件流，需要分别保存 `next_cursor`。屏幕审计区最多保留 128 次运行、合计 2 MiB 的数据，单段源码上限 64 KiB；读取事件时检查 `truncated` 与归档状态。v4/v5 连接仍可使用其已有能力，但原始源码不会出现在本地事件中；先升级两端并重新连接，再观察新调用。[日志与输出保留](logging.md) · [构建自己的界面](client-ui.md#构建自己的界面)

## 显示当前控制者与模型

将 controller 放在 code_mode 的参数层，身份会跟随该次请求的每个原生调用：

```json
{
  "controller": {
    "name": "ChatGPT",
    "model": "实际会话模型名称"
  },
  "code": "return await exec.list(clients[0].id);",
  "timeout_ms": 15000
}
```

侧栏显示当前活动控制者，请求结束后显示 Last controller；并发请求分别保留自己的身份。普通日志在身份变化时输出一行，轮询不会重复刷同一身份。

模型名称来自调用方声明，界面标记 caller-reported。MCP clientInfo 只提供应用名称；未传模型时显示 Model unknown，不从应用名或历史请求推断。controller.name/model 至少提供一个，拒绝控制字符和超长字段；source 由服务端生成，不接受调用方指定。

控制者元数据自协议 v5 开始提供，源码审计从 v6 开始提供，用户要求队列和本地登记成员快照使用 v7。新版服务端继续接收旧版本客户端，并省略旧端无法识别的字段；list 中的 controller_metadata 表明连接是否支持身份显示。先升级服务端，再升级客户端，随后新建或重载连接。以新连接实际返回的工具目录为准，仅更新仓库文件不会替换正在运行的进程。

## Zuse 程序与图片结果

桌面和浏览器任务使用独立 Zuse CLI。先用 `computer.version` 读取实际可执行路径；管理器只另提供 `download/install/uninstall`，不代理截图或输入动作。[管理命令与直接执行](computer-use.md)

对 CLI 生成的工作区图片，按实际文件路径调用已有文件图片工具。图片字节与 JSON 文本分开：

```javascript
const picture = await call(clientId, "ssmcp.fs.read_image", {
  path: "images/observed.png"
});
image(picture);
return picture.structuredContent;
```

先确认文件确实由该任务产生，并遵守文件工具的工作区、类型与大小限制；上述路径是示例，不代表已捕获截图。[文件图片与预览](client-input.md#原生读取与本地缩略图)

已授权的外部 MCP 工具仍通过其真实目录名称调用，保留 image、structuredContent、isError 和原生审批状态。它们与独立 Zuse 任务都使用通用结果通道，不恢复旧 CUA 或内置浏览器接口。

## 验证入口

- 原生后台输入与取消：src/backend/shell.rs 的进程测试。
- 多终端隔离、关闭和容量：tests/terminals.rs。
- 浏览输入分派、设备回复与独立终端状态：src/client/input.rs、src/client/terminal.rs。
- 源码浏览、日志筛选与布局：src/client/audit_view.rs、scripts/e2e_client_ui.py。
- 并发、调用预算、stdin/terminal helper：scripts/test_execution_helpers.mjs。
- Zuse 生命周期助手与通用图片/错误透传：scripts/test_cua_facade.mjs。
- 真实 SSH、MCP 与客户端画面：scripts/e2e_terminal_control.py。
