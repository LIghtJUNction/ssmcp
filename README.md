# ssmcp

### 让AI管理你的多台设备，无需公网IP。
> 客户端可以随便改！自由创造
**支持 Astra Pro 集群，在你自己的开发环境中协同完成开发任务。**

> 还能远程控制你朋友的电脑

> 最值钱的用法是榨干Astra Pro

[![Donate](https://donate.lmm.best/badge.svg?project=ssmcp&lang=zh-CN&layout=compact&theme=dark&width=480&animation=steam)](https://donate.lmm.best/?project=ssmcp)

ssmcp 把多个已授权的 Astra Pro 会话连接到你的开发机，围绕同一项目分工、交换进度、执行命令并汇总结果。命令和工具在你的开发环境中运行，所需结果与上下文通过已授权的会话传递。

在 Linux 开发机上，SSMCP 默认用内嵌的 **zrun 运行时**启动和管理 Agent 发起的命令。Agent 可以一次提交多条独立命令，并从各自的作业读取输出与退出码。[zrun 与批量命令执行](#zrun-与批量命令执行)

SSMCP 针对长时间运行的 Agent 任务优化原生异步执行与事件驱动等待。我们的目标是满足金融级时间敏感场景对计时精度、事件交付和结果可追溯性的要求。可通过单调截止时间、从事件到回执的延迟和断线后的重复执行次数来评估进展。[时间与长任务的验证口径](docs/time-primitives.md#时间敏感与长任务)

**Linux 与 macOS 是首要支持目标。** 功能开发、兼容性修复和日常验收优先覆盖这两个平台；Windows 客户端持续适配，并提供对应的原生测试与发布附件。

同一个账号可以同时连接多台设备，Agent 通过一个 MCP 端点选择具体设备和会话。**受控设备无需登录你的 AI 账号**：AI 应用持有 MCP 授权，设备持有自己的 SSMCP 授权；命令在被选中的设备上执行。CLI 面向专业开发者，适合脚本、服务管理和诊断；Windows 用户的日常连接与任务管理优先使用[桌面版](apps/ssmcp-desktop/README.md)。

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

长任务可保留同一个后台等待记录，HTTP 调用中断后继续读取结果；[定时提示词](docs/time-prompts.md)和 [Worker 事件订阅](docs/events.md#automatic-worker-subscriptions)把原操作的新情况带回会话。人类发言与自动提示统一显示在右侧，并可引用原消息继续回答。[等待与恢复](docs/await.md) · [输入与引用回复](docs/client-input.md#统一的输入与引用回复)

Agent 使用 `working`、`idle`、`offline`、`dreaming`、`retiring` 五种生命周期状态。空闲维护只使用已有授权与现有 Agent；真实用户输入可以抢占维护，结果未知的工作保留原记录，不能自动重放。[生命周期与维护](docs/workers.md#member-lifecycle-and-bounded-maintenance)

## 快速开始

### 1. 选择客户端并安装

Windows 用户推荐桌面版。桌面版复用 CLI 的会话和权限系统，提供连接、任务进度和按需展开的诊断界面。下载前核对[平台构建与发布状态](docs/clients.md)；源码和 CI 中的构建结果不表示所有平台已有正式发布包。

需要命令行、自动化或后台服务的专业开发者可安装 CLI：

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

由被控端用户在愿意开放的项目目录运行 `ssmcp 用户名@中继服务器`。这里填写控制者在该中继已注册的用户名，被控端无需登录。控制者用 MCP 应用完成该中继的 OAuth 授权。连接成功后，直接运行 `ssmcp` 会复用上一次连接。使用 `ssmcp 用户名@中继服务器 --register` 注册新用户名，账号及公告设置在中继网页管理。

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

## 一个 Agent 控制多台设备

### 连接与地址

`alice@relay.example.com` 表示中继上的账号，不限定只能有一台设备在线。在两台设备各自愿意开放的工作目录中运行，例如：

```sh
# 开发笔记本
ssmcp alice@relay.example.com --name dev --title "本地开发"

# 构建机器；--daemon 保存后台会话定义
ssmcp alice@relay.example.com --name build --daemon
```

每台设备需要安装并启动 SSMCP，不需要安装或登录 AI 应用。可使用设备授权邀请，也可使用上文的 `用户名@中继服务器` 连接方式；这些授权由设备用户决定，不能从一个 AI 账号推导出所有机器的控制权限。账号登录、设备接入凭据和 AI 应用的 MCP OAuth 授权分别管理。

当前源码为完成身份签名协商的账号连接返回以下地址信息；实际运行版本以 `list` 的字段和实时 schema 为准：

| 字段 | 用途与生命周期 |
| --- | --- |
| `routing.device_id` | 服务器、经验证账号与安装签名密钥共同确定的设备身份；网络重连和主机改名不改变它 |
| `routing.session_id` | 设备下的保存会话定义或当前前台进程；保存定义重启后保持，删除重建定义或重开前台进程会改变 |
| `id` / `routing.connection_id` | 这一次在线连接的 ID；每次重连变化，工具调用必须使用当前值 |
| `routing.alias` | 例如 `dev@laptop.local`，用于阅读；同名允许重复，需要完整 ID 消歧 |
| `routing.alias_kind` | 固定为 `display_only`，明确标签的含义 |

这里的 `.local` 标签不会自动注册 DNS 或 mDNS，也不表示能从互联网解析。真正的 `.local` mDNS 名称有链路本地语义；公网域名需另外配置。旧版、匿名或兼容连接可以继续使用当前连接 ID，但没有完成证明时不会伪造稳定设备身份。[地址语义与安全模型](docs/device-control-plane.md#三层身份)

### 发现后逐设备调用

只在 AI 应用中配置一次 `https://relay.example.com/mcp`。调用公开的 `list` 工具，查看当前授权范围内的 `clients`。需要重找某个已记录的设备和会话时，给 `list` 传入从先前结果取得的完整 `device_id`、`session_id`；这两个字段只过滤发现结果，不执行操作。

选定两条具体连接后，先分别取得它们的工具 schema。各设备的操作系统、已安装工具和工作区可以不同：

```javascript
const targets = ["CURRENT_CLIENT_ID_A", "CURRENT_CLIENT_ID_B"];
return await Promise.all(targets.map(id =>
  tools.search(id, {name_prefix: "ssmcp.exec.", include_schemas: true})
));
```

确认工具参数后，再对明确选中的 ID 操作。例如分别查看两台机器的项目状态：

```javascript
const targets = ["CURRENT_CLIENT_ID_A", "CURRENT_CLIENT_ID_B"];
return await Promise.allSettled(targets.map(async id => ({
  client_id: id,
  result: await exec.run(id, {
    command: ["git", "status", "--short"],
    wait_ms: 5000
  })
})));
```

把示例占位符替换为此次 `list` 返回的真实 ID。产品不会按相同名字、账号或设备组自动广播命令；逐目标结果仍需分别检查。身份、会话 ACL、原生权限和在途调用边界会继续校验，知道设备 ID 不等于拥有操作权限。

### 断线、离线和当前阶段

重连后应先重新发现稳定设备／会话对应的当前连接，再读取原任务的可验证状态。旧连接 ID 不会自动转向新连接，命令超时也不能当作“没有执行”。保存任务 ID、worker claim、提交和输出游标；回执不确定时保持 `unknown`，先核实实际效果。

有已授权的 007 服务时，Agent 先串行读取其状态和保存会话目录。新版本的 `ssmcp.007.recover` 仅恢复设备 ID 和定义 ID 都匹配、且未被明确停止的原定义；活动管理器只返回现状。旧版 007 缺少该能力、目标没有保存定义、权限被撤销或机器离线时，应保留检查点并报告阻塞。恢复连接不会恢复已结束进程的内存任务，也不会自动重放命令。[恢复协议](docs/recovery.md)

| 阶段 | 当前范围 |
| --- | --- |
| P0 | 同账号多设备、签名设备身份、稳定保存会话 ID、单 MCP 端点发现和精确连接路由、受限离线目录、恢复目标核对 |
| P1 · roadmap | 经认证加密的通用设备间直连、ICE/STUN 与必要的 TCP 路径、失败回退中继、保持设备身份的签名密钥轮换、迁移和 NAT 变化验收 |
| P2 · roadmap | 设备组与搜索、持久目录、可选真实名称发现、更大规模与界面优化 |

P0 的命令数据经过中继，当前传输标记为 `relay`。现有显式配对的 TCP 服务能力也不代表已经实现通用 ICE/P2P 数据平面；只有抓包与故障验证完成后才会声明相应直连能力。公网 IP 本身不保证路径可达。

当前每个账号最多 128 条在线连接；最近会话目录最多 8192 项，离线记录最多保留到 broker 重启或断线后 24 小时，且仅所有者可见。安装签名密钥保存在本机私有账号目录；复制它会复制设备身份，删除它会改变设备 ID。登录凭据续期不等于签名密钥轮换。更完整的限制、信任边界、分阶段设计和验收条件见[多设备控制平面](docs/device-control-plane.md)。

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
| 邀请协作者 | `/invite` 生成主 Agent 说明；`/invite worker` 生成关联 worker 池的接入说明 |
| 管理 worker 席位 | `/worker ls` 查看状态；`/worker +N`、`/worker -N` 调整席位数 |
| 查看原生工具 | 通过 MCP 工具目录或 `ssmcp --list-tools` 查看 |
| 浏览工具输出 | 默认显示状态与最多两行已脱敏结果；点击 `Click expand` 或选中后按 Enter 展开 |
| 查看成员事件 | 在成员列表右键点击进程或线程，查看当前进程保留的对应事件；Esc 返回 |
| 查看详细日志 | `/logs` 打开 Logs；其他对话页可用右键切换日志 |
| 展开状态侧栏 | **F3** 或 `/sidebar`；再按一次收起，F8 保存当前设置 |
| 复制与帮助 | `/copy` 复制最近回复；**F7** 原生选择；`/help` 打开完整帮助 |

旧版内置 Codex、Claude Code、Pi 和 OpenCode 导入已移除。请删除启动脚本中的相应 `--load-harness` 参数，改用独立 MCP 客户端或在本地显式运行命令。现有配置与历史文件可继续保留。

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

`enabled`、`window_ms`、`kinds` 和 `strategy` 可调整；`strategy` 是建议文本。长等待使用公开 `await` 或 `events` 工具：`await` 的单次读取窗口默认 20 秒、最长 55 秒，公开 `events.wait` 默认且最长 55 秒；JavaScript 内的 `events.wait` 默认且最多等待 1 秒。后台观察期限与这些读取窗口分别计算。跨窗口保留 `wait_id`，处理事件后分别确认事件与等待结果游标；输入事件只负责唤醒，正文仍通过原输入通道领取。[订阅、绑定与等待示例](docs/events.md) · [JavaScript 短等待](docs/code-mode.md#events)

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

SSMCP 默认启用原生命令、文件、会话和 Zuse CLI 管理工具。第三方工具可以使用独立的 MCP 客户端接入；`ssmcp.harnesses.load` 仅支持显式配置的 SSMCP peer。[迁移与连接说明](docs/optional-harnesses.md)

`--input` 默认从终端接收聊天消息，`--agent` 默认由下一位调用方领取。用 `ssmcp --list-harnesses` 查看支持项；启动时可用 `--load-harness ssmcp` 连接明确指定并已认证的 MCP 网关，连接参数放在 `--` 之后。[客户端输入](docs/client-input.md) · [连接参数](docs/agent-entry.md)

`ssmcp local --socket` 提供默认路径的本机 HTTP/MCP Unix socket，`--socket PATH` 可指定私有路径。时间工具支持当前 UTC、跨时区时间差和倒计时，终端直接显示运行时长与剩余时间，`/wait` 可查看详情。[本机连接](docs/local-mode.md) · [时间原语](docs/time-primitives.md)

- **外部开发工具**：通过各自独立的 MCP 客户端接入，或显式从本地终端运行。
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

从 Code Mode 调用已有 MCP 网关时，先配置 peer，再发现 `peer.` 工具。RC 内嵌配置、文件导入和运行时卸载收录在设计稿中。[接入教程](docs/mcp-peer-tutorial.md) · [RC 设计](docs/mcp-server-rc.md)


其他客户端的源码入口与各端验收范围见[客户端](docs/clients.md)；HTTP 接入与配对协议见[GUI 网关](docs/gui-gateway.md)。

### 命令执行、在途去重与结果缓存

#### zrun 与批量命令执行

zrun 是 Project Z 的进程运行时，也提供独立的 Linux 交互 shell。SSMCP 在 Linux 客户端内链接 zrun，用它启动子进程、等待退出、回收进程，并通过作业接口交付输出。Agent 调用 `exec.run`、`exec.start` 或用 `exec.parallel` 提交多条命令都会进入宿主的执行后端。这条默认路径省去了为每条命令再启动一个命令解释器的开销。macOS 的普通管道命令也使用 zrun 进程后端。Windows 由平台原生后端管理进程。[进程后端](src/backend/process.rs) · [zrun 项目](https://github.com/LIghtJUNction/zrun)

命令用参数数组表示。`["git", "status", "--short"]` 直接启动 Git，带空格的文件名也只占一个参数。需要管道、重定向等 shell 语法时，在数组中写明解释器。下面同时查询工作区状态和已跟踪文件数。`clientId` 取自已授权设备的 `list` 结果。第二条需要设备的 `PATH` 中有 `zrun`。

```javascript
const jobs = await exec.parallel(clientId, [
  {command: ["git", "status", "--short"], mode: "pipes"},
  {command: ["zrun", "-c", "git ls-files | wc -l"], mode: "pipes"},
], {concurrency: 2, wait_ms: 3000});
return jobs;
```

`exec.start` 返回 `job_id`，适合稍后用 `exec.wait` 等待或用 `exec.read` 取回结果。等待窗口到期后作业继续运行，沿用同一 `job_id` 即可。需要终端输入和控制键时可显式使用 `mode: "pty"`。指定 `bash -c` 的命令继续遵循 Bash 语法，已有脚本可以沿用原解释器。[Code Mode 命令接口](docs/code-mode.md#exec)

配套 zrun 提供原生 Tab 命令和路径补全、历史建议与多行粘贴。要把本地 zrun 直接接入支持 MCP 的 AI 客户端，可运行 `zrun ai config` 获取 stdio 配置。其 `shell_run_compact` 工具把连续相同的完整行合并计数，保留退出码，并分别摘要标准输出和错误输出。`output_read` 可分页读取已捕获的原始字节。[zrun 的 AI 与 shell 接口](https://github.com/LIghtJUNction/zrun/blob/main/docs/agent-shell.md)

#### 在途去重与结果缓存

默认情况下，每次 Agent 命令调用独立执行。对于输入版本明确、可重复的只读管道命令，可显式传入 `reuse: {input_version: "…", ttl_ms: 0}`，让同一授权线程中并发的等价请求共享同一 `job_id`。设置正的 `ttl_ms` 后，完整成功结果可在有效期内复用，最长 5 分钟。调用方负责在实际输入变化时更换 `input_version`。回执中的 `reuse.status` 区分 `started`、`joined` 和 `cached`，可据此核对实际启动数。[宿主复用实现](src/backend/shell/reuse.rs)

### 管理 Zuse CLI

```sh
ssmcp computer version --json
ssmcp computer download --json
ssmcp computer install DOWNLOAD_ID_FROM_REPLY --json
```

版本来自实际 CLI 的 `--version`。下载选择官方稳定版并核验资产身份、大小与 SHA-256；安装仅写入当前管理器的私有目录，不覆盖外部 Zuse、不修改 PATH。使用返回的可执行路径运行任务；卸载必须提供真实 `installation_id`，且所有权和字节仍匹配。[四个管理工具与示例](docs/computer-use.md)

旧控制开关、兼容入口和默认 `Super + Alt + C` 快捷键已移除。独立 Zuse CLI 可直接使用，无需先切换控制权或进入 takeover 流程。[Zuse 管理与执行](docs/computer-use.md)

通用 Shell 与独立 Zuse 仍遵循操作系统权限；需要隔离时配置实际的账户、会话或沙箱边界。

### 命令执行统计

Statistics 的「命令执行」区统计当前进程本地原生执行入口（包括终端 `!`）。启动调用、成功 spawn 和启动错误分别显示，wait/read 不增加计数。最近 20 项保留任务按状态显示，耗时榜取其中已结束的前 5 项。统计不显示命令参数、目录、环境或输出，也不合并浏览器、中继和其他 worker 的数字。Linux 在任务回收后按该命令的 wait4 记录分别排列用户态加内核态 CPU 时间与峰值 RSS。内核可能将命令自行回收的后代计入，RSS 不是整棵进程树的并发峰值。未取得回收记录或其他平台显示不可用。缓存和去重指标目前未接入。Linux 的 `/track-command COMMAND` 显式启用原生任务独占目录跟踪。Stats 在任务结束后显示该目录相对空目录的逻辑字节净变化，并标记账本不完整状态。普通 `!` 保持原行为，Stats 不扫描共享目录、工作树或全盘。已验证 Agent 的任务账本继续通过原有授权任务状态读取。

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

原生文件变更支持路径预检、显式选择的 Linux 用户与项目规则，以及独立的安全裁决流。
未配置策略时保留普通文件操作；配置失效后的阻止与恢复方式、只读裁决查询和覆盖范围见
[原生文件安全预检](docs/workspace-safety.md)。

[安全与隔离 RFC](docs/safety-rfc.md) 记录威胁模型、阶段门槛与实验设计。

#### 文件隔离覆盖范围

现有规则预检约束原生变更请求。原生读取、目录枚举和传输尚未接入安全目录的
内容隔离，`.ssmcp/safety/` 也未在普通 Agent 的执行视图中替换为空占位。
当前没有由服务端签发的专用安全 Agent 权限。协作角色文本不授予规则文件的访问权。
查看脱敏裁决仍通过所属会话的授权事件流，或者本地私有 attach 只读租约。

Code Mode 的 JavaScript 沙箱不挂载工作区。由它发起的原生工具调用在宿主侧执行。
原生 Shell、PTY 和 Worker 执行路径仍可获得客户端 OS 用户已有的文件访问权限。
Worker cgroup 约束资源用量，文件隔离尚未接入。当前部署不能依靠此预检向普通
Agent 隐藏安全目录原始数据，或强制 Git 元数据只读。

Git 元数据隔离还需覆盖工作树中的 `.git` 目录或指针文件，以及仓库解析后的
gitdir 和 common-dir。普通仓库、链接 worktree 和 submodule 的这些路径可不同。
现有宿主 Git 状态查看禁用可选锁。已有的客户端后台 fetch 可更新 Git 对象和
远程跟踪引用，属于另一条宿主路径。本草稿未新增任何可代 Agent 写 Git 元数据的接口。

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

