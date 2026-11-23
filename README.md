# ssmcp

### 面向 Astra Pro 的 harness
> 客户端可以随便改！自由创造
**支持 Astra Pro 集群，在你自己的开发环境中协同完成开发任务。**

[![Donate](https://donate.lmm.best/badge.svg?project=ssmcp&lang=zh-CN&layout=compact&theme=dark&width=480&animation=steam)](https://donate.lmm.best/?project=ssmcp)

ssmcp 把多个已授权的 Astra Pro 会话连接到你的开发机，围绕同一项目分工、交换进度、执行命令并汇总结果。命令和工具在你的开发环境中运行，所需结果与上下文通过已授权的会话传递。

你可以让不同会话分别实现功能、检查改动和运行测试，通过成员身份和定向消息协作，把结果提交到同一个项目。[了解集群协作](docs/agent-collaboration.md)

[演示视频](#演示视频) · [快速开始](#快速开始) · [本地与远程目标](docs/connection-contexts.md) · [会话操作](docs/client-input.md) · [工具与 Zuse CLI](docs/computer-use.md) · [编写自己的客户端](docs/client-extension.md) · [部署服务器](docs/deployment.md)

[![Rust](https://img.shields.io/badge/Rust-1.100%2B-64748b?style=flat-square)](Cargo.toml) [![License: MIT](https://img.shields.io/badge/License-MIT-64748b?style=flat-square)](LICENSE) [![Releases](https://img.shields.io/badge/Download-Releases-4285f4?style=flat-square)](https://github.com/LIghtJUNction/ssmcp/releases)

---

## 演示视频

- **中文**：[抖音 · ssmcp 项目介绍](https://v.douyin.com/_3yRZNA-Qls/) · [哔哩哔哩 · ssmcp 项目介绍](https://b23.tv/wA4gzmV)
- **English**：[YouTube · ssmcp Demo](https://youtu.be/9LqpS5BXRTQ?si=kn2Oy9kPiUUPoFxm)

## 一条连接，三种用法

| 你想做的事 | ssmcp 提供什么 |
| --- | --- |
| 让云端 Agent 操作开发机 | 原生 Shell、独立命令任务、文件、Zuse CLI 管理，以及按需加载的 Codex / Claude Code / Pi / OpenCode 工具 |
| 在终端里持续协作 | Markdown 回复、代码高亮、文件和技能引用、问答队列与 worker 分工；运行期间仍可继续输入 |
| 做自己的远程 harness | 客户端拥有工具目录、schema、上下文与 UI；复用协议和客户端扩展 hook，替换自己需要的部分 |

它的基本过程很直接：Agent 选中一个在线客户端，提交带参数的工具调用，客户端在本机执行，再返回结果、图片或可分页读取的输出 ID。工具的名称和参数来自实时目录；安装了一个产品，不代表它的全部内部功能都可以直接调用。

## 快速开始

### 1. 安装

Linux / macOS：

```sh
curl -fsSL https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.sh | sh
```

Windows PowerShell：

```powershell
irm https://github.com/LIghtJUNction/ssmcp/releases/latest/download/install.ps1 | iex
```

也可以使用 `cargo install ssmcp --locked --bin ssmcp`。安装脚本获取已发布版本；源码构建使用下文的完整子模块检出步骤。[平台与安装选项](docs/releases.md)

手动安装的客户端可以直接检查和更新：

~~~sh
ssmcp update --check   # 只检查最新稳定版
ssmcp update           # 校验下载后替换当前安装
~~~

更新使用官方发布的 `SHA256SUMS`，并检查候选程序的版本；下载或校验失败会保留原程序，源码版本较新时不会降级。Cargo、系统包管理器等管理的安装继续使用原来的更新命令。已有会话保持当前程序，重启后使用新版本。[自更新说明](docs/commands.md#更新客户端)

### 2. 在项目目录连接

```sh
ssmcp alice@relay.example.com --name dev --title "开发与测试"
```

把服务器地址换成自己的服务地址。首次连接引导 GitHub 授权，并在该服务器绑定你选择的用户名。后续直接运行 `ssmcp`，即可复用最近的授权账号；没有图形浏览器时使用 `--no-browser`。

客户端主动建立 HTTPS/WSS 连接，无需在设备上开放入站端口。保持终端或后台客户端运行，就能使用它的工作区和工具。

```sh
ssmcp list       # 查看已授权的在线进程与已登记线程
ssmcp -i         # 进入一台设备的交互终端
ssmcp whoami    # 查看当前账号
```

完整步骤见[账号与设备](docs/client-accounts.md)和[默认服务器与可信公钥](docs/client-server.md)。

### 3. 接入 MCP

将终端显示的 `https://你的服务器/mcp` 加入 ChatGPT 或其他支持 OAuth 的 MCP 客户端，再完成该应用的授权。CLI 登录与 MCP 应用授权分别管理。

对外 MCP 入口以实际服务的工具目录为准，当前源码包含以下入口：

| 入口 | 作用 |
| --- | --- |
| `list` | 发现当前身份可访问的设备与客户端工具目录 |
| `code_mode` | 用 JavaScript 组合原生调用，安排顺序、并发和结果读取 |
| `collab` | 同一会话中的成员身份、定向消息、广播与确认 |
| `events` | 订阅事件、显式确认游标、等待状态变化，并配置主控等待建议 |
| `await` | 等待事件、执行任务、成员状态或消息、用户输入，不占用 JavaScript 运行槽 |
| `messages` | 客户端与线程之间的定向消息、收件箱和确认 |
| `link` | 为已授权客户端中的图片创建短期、单次下载链接 |

`link` 的候选实现调用目标客户端的图片读取工具，单图上限 400 KiB；默认 120 秒失效，首次 GET 会消费链接，HEAD 不消费。创建返回 URL、资源链接和清理 ID，不返回图片 base64。是否已部署须查看当前服务目录。[临时图片链接 API](docs/image-links.md)

由被控端用户在愿意开放的项目目录主动运行 `ssmcp --allow 用户名@中继服务器`。这里填写控制者在同一中继上已注册的用户名。被控端无需登录账号，设备接入无需服务器邀请码。控制者仍须登录该账号，MCP 应用须完成该中继的 OAuth 授权。显式名单仅允许列出的账号访问这条设备连接，既有权限检查仍然适用。已有有效登录时，省略名单只允许自己。

主窗口中的 `/invite` 生成会话内工作组协作说明，Agent 通过 `collab.join` 登记会话身份。协作邀请与设备授权各自独立。[账号与授权](docs/client-accounts.md)

先从 `list` 选择确切的客户端 ID，再查询工具的原始 schema：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await tools.search(clientId, {
  query: "ssmcp.exec",
  include_schemas: true
});
```

运行一个独立命令：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await exec.run(clientId, {
  command: ["git", "status", "--short"],
  wait_ms: 5000
});
```

返回值包含任务 ID、运行状态、退出码与输出游标。等待时间到期后，任务可以继续运行；使用同一个 ID 读取或取消。大结果保留在有界内存中，通过返回的输出 ID 分页读取；容量不足或内容被裁剪时会明确标记。[命令接口](docs/commands.md) · [编排与输出](docs/session-control.md)

## 终端就是工作会话

### 只在本机运行

Linux 上可以使用 `ssmcp local` 在当前项目启动本地 MCP 会话，无需远程服务器或 GitHub 登录。默认监听回环地址的可用端口；在终端执行 `/logs`，将本次 MCP 地址加入同机的 OAuth 客户端，并用其中的一次性登录码授权。终端会保持运行，退出后本次连接授权结束。

本地 `code_mode` 还需已安装的 Node.js、Bubblewrap（`bwrap`）和 `prlimit`，以及系统允许创建相应的 Linux namespace。缺失时会明确失败，不会绕过沙箱。同版 `code-mode.js` 已编入 Linux 客户端，每次本地会话使用自己的私有副本，无需额外安装脚本。详见[运行前提](docs/local-mode.md#运行前提)。

已有 `.local` 名称解析和受信任的本机 HTTPS 反向代理时，可运行 `ssmcp local --listen 127.0.0.1:8765 --public-url https://workstation.local`。代理须指向该固定端口；命令不自动配置 mDNS、证书或防火墙。详见[本地运行与 `.local` 入口](docs/local-mode.md)。本段描述源码能力，安装版本是否包含该命令，以 `ssmcp local --help` 为准。

### 使用同一入口选择连接目标

把本机或 HTTPS 中继保存为命名 context，随后用同一个参数选择：

~~~sh
ssmcp context create here --local
ssmcp context create work --server https://relay.example.com
ssmcp --context work --name dev
ssmcp context use here
~~~

`--context` 只选择本次目标，`context use` 保存新连接的默认选择；已有连接和保存的 session 继续使用原目标。用 `ssmcp context show` 查看目标与选择来源。命名目标不保存账号凭据；完整优先级和本地运行前提见[本地与远程目标](docs/connection-contexts.md)。安装版本是否支持，以 `ssmcp context --help` 为准。

### 对话与工具

默认对话页收起侧栏，把主要空间留给正文和输入。你可以在工具运行期间补充要求，也可以指定某个已登记的 Agent 接收。账号与本月剩余流量显示在输入区上方，底部保留连接来源与操作提示。

| 操作 | 用法 |
| --- | --- |
| 查看会话统计 | 在初始对话的草稿开头按 ←，分别查看主会话与 worker 池的连接 ID、OS PID、已登记线程和实际计数 |
| 发给指定 Agent | `/sessions` 或 `/message` 打开收件人选择器，选定后返回输入框发送 |
| 引用文件、图片 | 输入 `@` 枚举工作区；支持中文、带空格路径和目录续补 |
| 粘贴图片 | **Ctrl + V** 或 `/paste`；`/attach PATH` 导入本地图片 |
| 引用技能 | 输入 `$` 枚举有效技能；项目技能覆盖同名全局技能 |
| 接受候选 | ↑ / ↓ 选择，Tab / Enter 补全；补全后再 Enter 发送 |
| 运行本地命令 | `!pwd`、`!cargo test`；输出与退出码回到会话 |
| 查看任务 | `/tasks`；`/tasks cancel JOB_ID` 停止指定命令任务 |
| 回答问题 | **Shift + ↑** 打开待答问题，保留普通草稿 |
| 查看原生工具 | `/pi`、`/cc`、`/codex` 列出真实目录；`TOOL {...}` 显式调用 |
| 浏览工具输出 | 默认显示状态与最多两行已脱敏结果；点击 `Click expand` 或选中后按 Enter 展开 |
| 查看成员事件 | 在成员列表右键点击进程或线程，查看当前进程保留的对应事件；Esc 返回 |
| 查看详细日志 | `/logs` 打开 Logs；其他对话页可用右键切换日志 |
| 展开状态侧栏 | **F3** 或 `/sidebar`；再按一次收起，F8 保存当前设置 |
| 复制与帮助 | `/copy` 复制最近回复；**F7** 原生选择；`/help` 打开完整帮助 |

工具调用成功结束时显示 `done`，运行中、失败、取消或超时显示实际状态。对话页只预览简短结果，详细记录可在 Logs 中查看；仍保留的长输出可按任务游标或输出 ID 读取。

回复支持 Markdown 和常见代码块语法高亮。读取图片后，客户端会探测终端的 Kitty 图像能力；支持时显示清晰图片，其他终端显示有界半块预览或文字说明。超过 8 KiB 的整段粘贴会保存为私有工作区文件并插入引用，便于继续补充文字。

Agent 使用 `ssmcp.ui.message` 把进度与回答显示在这个会话里；重试可带幂等键，回执提供消息 ID 与实际显示状态。使用 `ssmcp.ui.ask` 非阻塞提问，再通过 `ssmcp.ui.questions` 读取答案。这些工具提供本地沟通通道，外部聊天应用仍决定自己的消息显示方式。

`ssmcp.ui.present` 支持文字、卡片、指标、表格、进度、步骤与条形图，复用实时消息与复制通道。[结构化显示](docs/intelligence-ui.md)

未选择接收人时，用户要求由下一次符合条件的单个调用领取；选择成员后，仅绑定该成员身份的调用能领取。**已交付**表示要求进入了工具响应，执行进度仍需由接收者反馈。[输入与交付语义](docs/client-input.md) · [成员协作](docs/agent-collaboration.md)

### 主窗口与 worker 分工

在项目目录的交互终端启动主聊天和关联的 worker 监控窗口：

~~~sh
ssmcp --worker 3
~~~

这会创建三个初始离线的席位。默认 worker 是外部 Agent，继续使用它自己的应用和模型；SSMCP 提供任务队列、工具与回执，不启动模型。主窗口保留聊天输入，worker 窗口默认以全宽时间线展示 Agent 实际上报的进度，并显示当前任务和状态。尚无上报时明确等待；警告、错误和 `unknown` 保持可见。按 **F2** 查看日志与本机 SSMCP 资源详情，**F1** 查看帮助，**T** 查看任务全文。[监控操作](docs/workers.md#use-the-monitor)

在主窗口完成接入：

1. 输入 `/invite` 并按 Enter。复制面板中的说明，粘贴到已获准接入的主 Agent 对话。主 Agent 按说明在控制会话以 `main` 身份加入，再用自己的凭据绑定。
2. 用 `/worker ls` 确认关联池在线且有空位，再输入 `/invite worker` 并按 Enter。把说明粘贴到每个独立、已获准接入的 worker 对话。每个 worker 在池中用 `collab.join` 登记为 `worker`，用自己的私有凭据绑定后调用 `ssmcp.worker.wait` 注册席位并等待任务。
3. 两个邀请命令都会显示可滚动的本地面板并尝试复制。剪贴板不可用时手动复制，按 Esc 关闭面板。凭据由各 Agent 私下保存。池满员、离线或正在调整席位时，等状态恢复再生成 worker 邀请。

之后可在主窗口查看和调整席位；普通主窗口尚无池时，`/worker +N` 会创建关联窗口：

| 命令 | 作用 |
| --- | --- |
| `/worker ls` | 查看席位、注册人数、工作中、待命与离线数量 |
| `/worker +2` | 增加两个离线席位；尚无池时创建关联窗口 |
| `/worker -1` | 请求减少一个席位，等待正在执行的任务和退役确认 |

`registered_workers` 是已注册 worker 数，`working` 是任务执行中，`standby`／`idle` 才表示待命容量；创建席位或复制邀请不会增加待命人数。

协调者派发任务后，worker 保留完整 claim，用 `ssmcp.worker.complete` 报告实际结果，再用新请求 ID 等待下一项任务；等待重试复用原 ID。取消请求不会直接释放执行中的席位，断线造成的 `unknown` 结果也不能自动重放。

退出或重载主窗口会清理它启动的 worker 窗口。单独关闭监控窗口后，主聊天仍可使用。邀请、任务回执、缩容与窗口生命周期见 [Worker 指南](docs/workers.md)。

### 主控何时等待

Code Mode 回复末尾的提示词 hook 根据真实关联池状态，向身份、协调角色和权限均通过检查的主控提供建议。提示有节流和长度上限：有待命 worker 时提示分工；没有独立工作可继续时，建议等待 `user.input_available`、`worker.completed`、`worker.idle` 和 `worker.progress`。这段建议不会自动派发任务或启动模型。

使用已取得的控制会话 ID 和该会话自己的凭据，修改当前成员的等待建议：

~~~javascript
await collab.bind(controlId, controlCredentials);
return await events.policy(controlId, {
  patch: {
    enabled: true,
    window_ms: 55000,
    kinds: ["user.input_available", "worker.completed", "worker.idle", "worker.progress"],
    strategy: "先完成独立工作，空闲时再等待用户输入和 worker 事件。"
  }
});
~~~

`enabled`、`window_ms`、`kinds` 和 `strategy` 可调整；`strategy` 是建议文本。长等待使用公开 `await` 或 `events` 工具，默认且最长为 55 秒；JavaScript 内的 `events.wait` 默认且最多等待 1 秒。跨窗口保留 `wait_id`，处理事件后分别确认事件与等待结果游标；输入事件只负责唤醒，正文仍通过原输入通道领取。[订阅、绑定与等待示例](docs/events.md) · [JavaScript 短等待](docs/code-mode.md#events)

### 从命令行连接对话与传递结果

`ssmcp list` 把当前账号可控制的在线进程及其已登记线程自动组织成树。例如，两条连接各有一个登记成员时，默认输出如下；名称和主机名取自实际连接：

~~~text
  #  NAME / HOST
  1  workstation [process] · devbox
  2  ├─ Main / next unbound caller [main]
  3  └─ reviewer [thread]
  4  worker-pool [process] · devbox
  5  ├─ Main / next unbound caller [main]
  6  └─ worker-a [thread]
Numbers identify this exact list for 5 minutes. Use ssmcp list --json for full IDs.
Process chat input fans out to its listed main and registered threads.
~~~

每个 `[process]` 是一条在线客户端连接，后面显示主机名。其子项 `[main]` 是未绑定成员调用的主接收入口，`[thread]` 是通过 `collab.join` 登记的成员。不同进程各自成树；层级只表示线程登记在哪条连接上。同名进程或线程仍保留各自的编号。[目录来源](src/broker/session_io.rs)

左列数字可直接用于 `attach` 和 `send`：例如 `thread 3` 只选择 reviewer，`process 1` 选择 workstation 及该次列表中的主入口和登记成员。每次 `list`（含 `--json`）保存新的五分钟编号快照；远端顺序变化不会改写已有快照的目标。需要完整进程或线程 ID 时使用 `ssmcp list --json`，在 `selection.entries` 中按 `number` 查找 `id`。显示名称不会被当成线程身份。[选择规则](docs/client-accounts.md#选择进程与线程)

远程 `attach` 默认连接所选对象的聊天输入与输出：

~~~sh
ssmcp list
ssmcp attach thread 3
ssmcp send process 1 '检查当前改动' | ssmcp wait --any
ssmcp send thread THREAD_ID '整理当前结论' |
  ssmcp wait --gather |
  ssmcp send thread NEXT_THREAD_ID --stdin
~~~

`send` 输出请求回执，`wait` 输出接收者的实际最终回复。可以等待任意一条、全部，或用 `--count N` 等待 N 个输入的结果；确认收到输入和进度更新都不算完成。输入在接收者下一次符合条件的原生调用响应中交付，这些命令不启动模型；停止等待或退出 attach 也不取消尚未完成的工作。[收发与等待](docs/client-accounts.md#发送输入与等待实际回复)

聊天收发需要中继与接收端客户端同时支持相应接口，仅更新本机程序不会更新远端服务。请求和回复保存在服务端的有界内存中，最长保留 1 小时，服务端重启会丢失这批状态。原有 Shell 终端仍可用 `ssmcp -i` 或 `ssmcp attach process PROCESS_ID --terminal main`；本地保存会话使用 `ssmcp attach --local NAME`。[兼容与保留边界](docs/client-accounts.md#聊天接口的兼容与保留边界)

成员事件只展示当前进程保留、能够核对真实身份的内容。`ssmcp.ui.events` 按经过验证的归属与线程分页读取；显示名称不会被用来推断线程身份。[实时事件与界面接口](docs/client-ui.md)

### 定时更新 Git 上游

连接存续期间，ssmcp 默认每 300 秒检查当前分支的远端上游并更新对应的远端跟踪引用；无终端和后台连接同样运行。可设为 60–86400 秒，或用 `0` 关闭，修改对新连接生效。

```sh
ssmcp config get git-fetch-interval
ssmcp config set git-fetch-interval 300
ssmcp config set git-fetch-interval 0
```

首次检查、共享调度、超时与更新范围见[定时 Git fetch 配置](docs/configuration.md#background-git-fetch)。

### 先预览，再调整

```sh
ssmcp ui preview
ssmcp ui init
ssmcp ui check
```

内置 `dark`、`codex-dark`、`nord`、`solarized`、`light`、`minimal` 六种主题。通过 `[ui]` 配置调整侧栏、面板顺序和颜色，F5 应用显示配置；`/reload` 则重新启动客户端。终端支持原生图片且侧栏空间足够时显示方形像素二维码，窄窗口显示完整链接；网关提供教程时进入中英文指南，旧网关暂未提供教程时进入门户首页。[界面配置](docs/client-ui.md)

## 启动环境与可执行工具

SSMCP 正常启动时复用 `$XDG_CONFIG_HOME/ssmcp/ssmcprc.toml`；未设置或置空 `XDG_CONFIG_HOME` 时，默认读取 `~/.config/ssmcp/ssmcprc.toml`。主目录取 `HOME`，仅在其缺失时回退到 `USERPROFILE`。若要指定这些定位变量，应在启动前设为绝对路径，不能放进 `[env]`。已有配置保留原 `[tools]`，合并环境条目即可。[配置定位与解析](src/tool_repository.rs)

```toml
version = 1

[env]
COUNT_LABEL = '行数'
```

`[env]` 的变量名使用英文字母、数字和下划线，首字符不能是数字。值必须是字符串，按字面量传入：`$HOME`、`${NAME}`、`~` 不会展开，也不会执行命令。它只补充继承环境中缺失的变量；已有值即使为空也优先。`PATH` 同样遵守这一规则，再按以下顺序加入可执行目录：**项目 `.ssmcp/bin` → 主目录 `.ssmcp/bin` → 有效原 PATH**。只有未继承 `PATH` 时，才使用配置中的 `PATH` 作为尾部。[环境合成](src/client/environment.rs)

这里的项目是启动工作目录，不会向上寻找 Git 根目录。两个 bin 路径在启动时固定为绝对路径，即使目录尚不存在也会预置，但启动过程不创建它们。路径通过平台的拆分、拼接规则处理：Linux 使用冒号，Windows 使用分号。配置只影响新启动的 SSMCP 及其子进程；父终端和系统 PATH 保持原状，已有进程需重新启动才能读取新配置。[启动入口](src/bin/ssmcp.rs)

AI 可以把确定性的重复工作写成小程序，后续按命令名复用。下面提供完整的行数统计程序：在项目目录启动新的 SSMCP 后，于它创建的 Linux 终端执行；需要已安装 `rustc`。

```sh
mkdir -p tools .ssmcp/bin
cat > tools/count_lines.rs <<'RS'
use std::io::{self, Read};
fn main() -> io::Result<()> {
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    let label = std::env::var("COUNT_LABEL").unwrap_or_else(|_| "lines".into());
    println!("{label}: {}", text.lines().count());
    Ok(())
}
RS
rustc tools/count_lines.rs -o .ssmcp/bin/count-lines
printf 'a\nb\n' | count-lines
```

未在外层另设 `COUNT_LABEL` 时，预期输出为 `行数: 2`。Linux 程序需要可执行权限，必要时运行 `chmod +x .ssmcp/bin/count-lines`。Windows 编译输出使用 `.ssmcp\bin\count-lines.exe`；父 PowerShell 中可用 `& .\.ssmcp\bin\count-lines.exe` 显式调用。

将 `tools/count_lines.rs` 等源码和构建说明正常纳入 Git，把 `/.ssmcp/bin/` 加入项目 `.gitignore`，生成的二进制留在本机。跨项目复用时可放入 `~/.ssmcp/bin/`；项目中的同名工具优先。这样每个工具都保留可读源码，并通过普通进程执行获得结果。

## 复用现有工具

ssmcp 默认启用命令、文件、会话和 Zuse CLI 管理工具，不要求安装其他 harness。Codex、Claude Code、Pi 和 OpenCode 默认不探测、不启动；需要时用 `ssmcp.harnesses.load` 显式加载一项，`ssmcp.harnesses.list` 只读取缓存状态。加载后保留该工具的完整 schema、会话和审批方式，实际模型调用仍须显式发起。[按需加载说明](docs/optional-harnesses.md)

`--input` 默认从终端接收聊天消息，`--agent` 默认由下一位调用方领取。要在启动时加载原生协议，请用 `--load-harness codex` 或 `--load-harness claude-code`。用 `ssmcp --list-harnesses` 查看支持项。`--load-harness ssmcp` 可以连接明确指定并已认证的 MCP 网关。[客户端输入](docs/client-input.md) · [协议加载](docs/agent-entry.md)

`ssmcp local --socket` 提供默认路径的本机 HTTP/MCP Unix socket，`--socket PATH` 可指定私有路径。时间工具支持当前 UTC、跨时区时间差和倒计时，终端直接显示运行时长与剩余时间，`/wait` 可查看详情。[本机连接](docs/local-mode.md) · [时间原语](docs/time-primitives.md)

- **开发工具**：Codex、Claude Code、Pi、OpenCode；能力以当前成功注册的目录为准。
- **桌面与浏览器**：直接运行独立 Zuse CLI，通过通用 `exec` 传递 argv、管理任务和读取结果。`computer` 原生工具只提供版本查询、下载、安装、卸载；没有内置动作代理或隐藏 fallback。[Zuse 工作流](docs/computer-use.md) · [Windows 安装范围](docs/windows-browser-setup.md)
- **上下文**：工作区文件、项目指令、有效技能视图和显式只读 context roots。

指定已有浏览器时，先确认所选 Zuse 版本支持的连接方式和实际会话，再操作用户选定的目标。OAuth 登录、用户打开链接和已授权外部 MCP 的通用转发仍保留；它们不恢复旧内置浏览器工具。[现有浏览器连接](docs/browser-existing-session.md) · [code_mode 示例](docs/code-mode.md)

### 项目与全局工具仓库

[ssmcp-tools](https://github.com/LIghtJUNction/ssmcp-tools) 提供 Code Mode 的官方 JavaScript helper；项目和全局 Git 仓库可以维护自己的编排函数。加载顺序为 **官方 → 可信 RC 中按序列出的 overrides → 已启用全局 → 已登记项目**，后加载的同名导出覆盖前者。

| 来源 | 路径 | 启用条件 |
| --- | --- | --- |
| 项目 | 当前工作区的 `./.ssmcp/tools/` | 可信 RC 的 `tools.projects` 精确登记规范化后的工作区绝对路径 |
| 全局 | `~/.ssmcp/tools/` | 可信 RC 中 `tools.global = true` |
| 官方 | `$XDG_DATA_HOME/ssmcp/tools/official`，默认 `~/.local/share/ssmcp/tools/official` | 使用所选客户端的官方源码 |

在要登记的项目目录执行：

~~~sh
ssmcp tools paths
ssmcp tools init project --git
ssmcp tools status
~~~

`paths` 只查询位置。`init` 离线创建 helper 工厂、manifest 和 README，并登记来源；`--git` 初始化独立 Git 仓库，不自动提交或连接远端。全局仓库用 `ssmcp tools init global --git`。已有源码用 `enable project|global` 登记，`disable project|global` 停用并保留文件。目录存在、克隆完成或父目录已登记，都不等于当前项目已启用。

维护本地 helper 时，在 MCP `code_mode` 参数中显式选择其客户端：

~~~json
{
  "tool_client_id": "CLIENT_ID_FROM_LIST",
  "code": "return 'sources loaded';"
}
~~~

检查这次调用外层的 `structuredContent.tool_sources`：它报告实际加载的层、路径、SHA-256 和 Git HEAD／dirty 状态。各仓库独立提交、测试和管理 Git 历史；修改 JavaScript 后用同一 `tool_client_id` 发起新调用即可核对加载结果。新增底层原语仍需要支持它的 CLI／中继版本，helper 覆盖不改变原生权限、身份或参数校验。

`ssmcp tools check` 和 `ssmcp tools update` 只管理官方仓库，项目与全局仓库由各自的 Git 工作流维护。Dreaming 也可使用已有执行、文件和 Git 操作维护这些工作流；初始化本身不创建模型或调度器。[完整命令、可信配置与覆盖规则](docs/tool-repositories.md) · [源码选择与回执](docs/code-mode.md#find-and-iterate-tool-implementations)

其他客户端的源码入口与各端验收范围见[客户端](docs/clients.md)；HTTP 接入与配对协议见[GUI 网关](docs/gui-gateway.md)。

### 命令执行、在途去重与结果缓存

- **命令执行（可用）**。Code Mode 编排调用，宿主负责授权、执行和作业生命周期，原有审批仍适用。用 `exec.start` 启动后台命令并取得 `job_id`，再用 `exec.status`、`exec.read` 或 `exec.wait` 查询、读取输出或等待，用 `exec.cancel` 请求取消。原生 `ssmcp.exec.wait` 省略 `wait_ms` 时等待目标完成，显式 `wait_ms: 0` 则立即读取状态。窗口到期返回 `wait_status: "pending"`，作业继续运行。下次传入同一个 `job_id` 即可读取完成状态。命令默认没有执行期限，可设置 `timeout_ms` 终止超时进程。Code Mode 和中继调用各自仍有传输或编排期限。当前并发容量用尽时拒绝新启动，按负载接纳新命令的等待队列仍在规划中。[Code Mode 命令接口](docs/code-mode.md#exec)
- **在途去重（云端原型）**。同一授权范围内，参数和执行条件一致的受控只读请求同时到达时，可以共享一次尚在运行的执行。取消一个请求不影响其他仍在等待的请求。云端测试中，10 个请求共享 1 次实际执行，节省 9 次启动。生产去重尚未接通，草稿中的 Stats 指标显示 `unavailable`。[合并与计数设计](docs/design/controlled-execution-coalescing.md)
- **命令结果缓存（规划中）**。短时复用符合条件的已完成结果，避免在相同条件下再次启动进程。缓存按工作区版本（generation）校验，工作区标记为已变更、版本不一致或状态未知时按未命中处理。计划采用短 TTL、全局条目数和总字节数上限、单条大小上限，以及低开销的自动过期清理，缓存命中与在途去重分开统计。任意 Shell、写操作、授权与发布判断默认不使用缓存。

### 管理 Zuse CLI

```sh
ssmcp computer version --json
ssmcp computer download --json
ssmcp computer install DOWNLOAD_ID_FROM_REPLY --json
```

版本来自实际 CLI 的 `--version`。下载选择官方稳定版并核验资产身份、大小与 SHA-256；安装仅写入当前管理器的私有目录，不覆盖外部 Zuse、不修改 PATH。使用返回的可执行路径运行任务；卸载必须提供真实 `installation_id`，且所有权和字节仍匹配。[四个管理工具与示例](docs/computer-use.md)

旧控制开关、兼容入口和默认 `Super + Alt + C` 快捷键已移除。独立 Zuse CLI 可直接使用，无需先切换控制权或进入 takeover 流程。[Zuse 管理与执行](docs/computer-use.md)

通用 Shell 与独立 Zuse 仍遵循操作系统权限；需要隔离时配置实际的账户、会话或沙箱边界。

## 独立核心库

[ztuio](https://github.com/LIghtJUNction/ztuio)、[zharness](https://github.com/LIghtJUNction/zharness) 和 [zplugin](https://github.com/LIghtJUNction/zplugin) 作为固定源码的 Rust 依赖参与构建，分别承担终端帧、扩展接口和本机动态库加载。三个库均使用固定 Git 子模块，ztuio 的包名与 Rust 导入名均为 `ztuio`，精确版本为 `0.1.0`。它们独立维护版本，不启动模型或外部 harness。源码构建请使用 `git clone --recurse-submodules`；已有 checkout 使用 `git submodule update --init --recursive`。[依赖与协同开发](docs/dependencies.md)

## 客户端掌握能力，服务端负责连接

服务端负责身份、会话路由、成员通信与受限 JavaScript 编排。客户端负责执行工具、组织上下文、缓存当前输出和绘制界面。增加客户端原语后，调用方可以通过实时目录发现它，并继续使用通用 `call(clientId, name, arguments)`。

因此，你可以重写客户端，接入自己的远程 harness，或只替换 UI。现有界面也通过小接口读取状态和控制任务：

| 边界 | 可复用接口 |
| --- | --- |
| 能力发现 | 客户端版本、原生工具 schema、客户端资源 |
| 执行 | `terminal.*`、`ssmcp.exec.*`、原生 harness 工具 |
| 上下文 | `ssmcp.fs.*`、`ssmcp.skills.*`、`ssmcp.agents.*` |
| 会话界面 | `ssmcp.ui.*`、问答、实时事件、队列与源码记录 |
| 扩展策略 | 可选的客户端 `ToolCallHook`，调用前允许或拒绝，调用后观察结果 |

hook 默认关闭，不内置额外审核流程，也不改变工具原生权限。自定义策略由客户端开发者接入。当前连接统一使用 **wire v7**，旧协议会明确拒绝。[扩展接口与最小客户端](docs/client-extension.md) · [上下文边界](docs/context.md)

### Project Z：ztuio

终端帧差分、几何边界和定宽行已接入固定源码子模块 [ztuio](crates/ztuio/README.md)。这个小库使用 `no_std`，禁止 `unsafe`，没有第三方依赖；缓冲区由调用方提供，Unicode 宽度规则由宿主决定。它只计算该写哪些终端字节，图片协议、终端输入、主题和会话状态仍由客户端管理。

## 运行边界

工具使用客户端进程的操作系统权限；MCP 账号授权控制谁能调用设备。文件 API 受工作区和显式 context roots 限制，Shell 仍按操作系统权限执行。

对话、问题、事件和工具输出只保留在当前进程的有界内存中。退出或重载后，这些实时内容失效；客户端不自动保存会话 JSONL、历史归档或上下文记忆文件。需要长期保留的结果，应在退出前显式导出。账号凭据、连接配置及用户明确导入的文件仍按各自功能管理。[实时内容与保留边界](docs/client-ui.md)

图片导入会在客户端生成有界预览文件，仍以私有文件引用发送；不改变 wire v7。原生图片读取限制为 400 KiB，支持 PNG / JPEG / GIF / WebP。GIF / WebP 经 `code_mode` 返回时还需要支持这些格式的 relay；旧 relay 的图像通道仍只有 PNG / JPEG。长工具输出保留有界预览与内存捕获，展开界面事件只读取当前保留的显示正文。

Linux 上的已验证 Agent 可用 `exec.start(clientId, { …, track_artifacts: true })` 为单个命令任务追踪产物。默认软额度为 20 GB（20,000,000,000 逻辑字节），可用 `artifact_budget_bytes` 覆盖。计量仅覆盖任务在 `.ssmcp/task-artifacts/` 下的独占目录，提醒随任务读取结果返回。[配置与计量范围](docs/workspaces.md#命令任务产物与软额度) 默认保留任务产物。一次性、任务独占产物可显式设为 `temporary_artifacts: true`，正常关闭或 reload 时，客户端只在确认任务树已退出、文件身份和独占性可验证后尝试有界回收。

### 原生文件安全预检

`ssmcp safety` 和 `ssmcp safety status` 显示本地版本的安全覆盖范围。原生
`ssmcp.fs.write`、`ssmcp.fs.write_text`、`ssmcp.fs.edit_text` 和
`ssmcp.fs.mkdir` 在现有工作区权限与文件系统检查之前执行 zguard 预检。
绝对路径和父目录遍历被拒绝。在工具参数中直接命名 `.ssmcp/safety`
及其子路径会要求审批。此版本没有绑定原请求的审批入口，因此这类请求会在执行前停止。

拥有相应会话访问权的调用方可以通过 `ssmcp.fs.events` 分页读取
`ssmcp.safety.decision`，按该流的游标定位记录。安全事件只含工具、策略、
裁决与结果，省略路径、内容、参数和错误细节。文件审计流中其他既有事件仍
遵循各自的披露范围。远程模型或单独的审计服务离线时，本地裁决仍然生效。

当前只提供覆盖状态与会话内事件。CLI 风险队列、跨进程 `inspect`、审计对话、
自定义规则和原请求审批仍待接入。目录名称预检只看直接请求的路径，实际工作区
范围继续由原生文件系统门禁约束。外部进程、传输、删除与密钥外发未纳入此预检。

## 开发

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/ssmcp.git
cd ssmcp
cargo build --locked --bins --example vt_snapshot
cargo run --locked --bin ssmcp -- ui preview
```

`make check` 检查格式和 Clippy；`make test-client-ui` 在隔离的真实 PTY 中验证输入、问答、图片与重载。验证脚本使用测试自己的进程与工作区，worker 双窗口流程见 [E2E 验证脚本](scripts/e2e_worker_windows.py)。

项目持续使用 ssmcp 自己的远程工具开发。[发布与构建](docs/releases.md) · [下一版本计划](docs/next-version.md)

## 文档

| 从这里开始 | 深入实现 |
| --- | --- |
| [账号、设备与临时接入](docs/client-accounts.md) | [工具注册与 harness](docs/backends.md) |
| [输入框与本地工作流](docs/client-input.md) | [客户端扩展与上下文工程](docs/client-extension.md) |
| [主题、布局与快捷键](docs/client-ui.md) | [Zuse、浏览器与能力资源](docs/browser-desktop.md) |
| [部署服务器](docs/deployment.md) | [完整输出与会话控制](docs/session-control.md) |
| [信任与默认服务器](docs/client-server.md) | [Worker 接入与任务池](docs/workers.md) |

<details>
<summary>项目创作记录</summary>

[原创短片《日蚀》 · 24 秒](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/ssmcp-eclipse.mp4`）

</details>

---

[MIT License](LICENSE)

