# 工作区同步与端口映射

ssmcp 通过已经认证的客户端连接，在两个工作区之间传文件或转发 TCP。使用 `list` 选择连接，通过 `code_mode` 操作文件、同步任务和端口映射；标准 MCP resources 或 `resources.*` 可读取动态资源。多个 agent 的成员发现与消息交换使用独立的 [`collab` 工具](agent-collaboration.md)。

## 先确定每一端在哪里

| 名称 | 实际位置 | 怎样标识 |
| --- | --- | --- |
| 开发机工作区 | 启动 ssmcp 的项目目录 | `list` 返回的客户端 ID、hostname 和 cwd |
| 调用方工作区 | ChatGPT 沙箱、另一台电脑或另一个项目目录 | 在该环境启动独立 ssmcp 客户端后返回的 ID |
| 中转服务器 | 运行 ssmcpd 和隔离 JavaScript 的机器 | 负责认证、路由和托管任务；不充当任一工作区 |

`code_mode` 在服务器运行。源路径和目标路径始终相对于所选客户端的启动目录；端口始终属于所选客户端的网络环境。要把 ChatGPT 沙箱作为一端，需要该沙箱能够运行客户端、访问同一中继的 HTTPS/WebSocket 入口，并按当前账号访问规则完成授权。

在两端各自的共享目录启动客户端，使用当前服务器支持的身份授权方式。例如：

```sh
cd /path/to/project
ssmcp YOUR_USERNAME@ssmcp.example --name development
```

```sh
cd /path/to/workspace
ssmcp YOUR_USERNAME@ssmcp.example --name workspace
```

先调用 `list`，核对两个客户端的 `hostname` 和 `cwd`。下文示例放入 `code_mode.code`，其中 `developmentId` 和 `workspaceId` 应替换为实际发现的 ID。客户端重连会更换 ID，应重新发现。不同客户端可以运行任意 `-t` 后端，工作区能力与后端类型无关。

## 放在项目内的临时工作区

项目启动目录是代码和配置的根。SSMCP 将它创建的临时作业放在这个项目的 `.ssmcp/tmp/jobs/`，将它创建的 Git 工作树放在 `.ssmcp/worktrees/`。同一台机器上的不同项目各自管理这些目录。恢复副本放在 `.ssmcp/recovery/`。

### 独立 Git 工作树

在 Linux 或 macOS 的 Git 项目根目录运行以下命令。省略版本时从当前 `HEAD` 创建分离检出。

```sh
ssmcp worktree create
```

回执中的 `id` 标识这次分配，`path` 是项目内的相对路径。构建、编辑和检查都可以在这个路径下进行。等所有使用者结束后，执行 `ssmcp worktree release ID`，将 `ID` 换成回执值。释放会记录时间，同一 ID 可以重复释放。默认保留 24 小时，客户端在启动、运行期间每五分钟和重新加载时处理到期项。`ssmcp cleanup --dry-run` 可以预览，`ssmcp cleanup` 可以按同样的项目规则执行。

SSMCP 在清理前核对创建记录、目录身份、Git 的工作树登记和本地修改。仍在租用、有未提交或未跟踪内容、Git 加锁、含项目当前提交尚未包含的提交等情况，会保留检出。移入 `.ssmcp/worktrees/` 的已有目录仍由创建者管理。SSMCP 会核对自己的创建与释放记录。

### 单个命令的临时目录

已验证的 Agent 可在 Linux 管道作业中传入 `managed_workspace: true`，让客户端为这一条命令选择专用工作目录。项目根路径由 `SSMCP_PROJECT_ROOT` 提供。下面的例子在临时目录生成一份报告，随后按同一作业 ID 读取输出。

```javascript
const job = await exec.start(clientId, {
  command: ["sh", "-c", 'git -C "$SSMCP_PROJECT_ROOT" rev-parse HEAD > revision.txt && cat revision.txt'],
  managed_workspace: true
});
const state = await exec.wait(clientId, job.job_id, { wait_ms: 4000 });
const page = await exec.read(clientId, { job_id: job.job_id, max_bytes: 1024 });
return { job_id: job.job_id, running: state.running, stdout: page.stdout };
```

作业状态仍需读取时保留作业 ID。运行时在作业被遗忘、被保留列表淘汰或连接正常结束后，核对进程树和产物归属，再尝试回收专用临时目录。检测到后续改动、嵌套 Git 工作树或状态不确定时会保留现场。普通命令继续使用它所指定的工作目录。

## 文件读写

`fs` 提供少量结构化方法，不需要为每次读文件拼接 Shell 命令：

```js
const selected = clients.find(c => c.name === "development");
if (!selected) throw new Error("development is offline");
return await fs.list(selected.id, { path: ".", max_entries: 100 });
```

```js
return await fs.read(developmentId, {
  path: "README.md", offset: 0, max_bytes: 32768
});
```

读取返回 `data_base64`、`offset`、`next_offset`、`size` 和 `eof`。单次最多 256 KiB，二进制内容不会经过文本转换。目录分页使用 `cursor` 和返回的 `next_cursor`；目录变化后需重新开始分页。

```js
return await fs.write(workspaceId, {
  path: "notes/hello.txt",
  data_base64: "aGVsbG8K",
  create_parents: true,
  overwrite: false
});
```

写入先准备临时文件，再原子提交。默认拒绝覆盖，显式 `overwrite:true` 才替换已有普通文件。`fs.mkdir(id,{path,parents:true})` 创建目录，`fs.stat(id,{path})` 读取元数据。文件读写仅支持普通文件；路径穿越和符号链接路径被拒绝；这些限制只描述工作区内置方法，所选原生 Shell 或 Agent 仍遵循它自己的权限。

原生 UTF-8 文件读写、技能摘要和 AGENTS.md 视图参见 [上下文与文本接口](context.md)。

## 命令任务产物与软额度

Linux 客户端上的已验证 Agent 可以在 `exec.start` 传入 `track_artifacts: true`，为一次原生命令任务启用追踪，默认软额度为 20 GB（20,000,000,000 字节）。正整数 `artifact_budget_bytes` 覆盖默认值，旧调用单独传入该参数仍会启用追踪。持久产物保存在 `.ssmcp/task-artifacts/` 下，显式选择临时模式的产物保存在 `.ssmcp/tmp/jobs/` 下。每个目录都由本次任务独占。跨多条命令共享的构建缓存仍属于调用者管理的共享路径。

```js
const job = await exec.start(clientId, {
  command: ["sh", "-c", 'cargo build --target-dir "$SSMCP_ARTIFACT_DIR/target"'],
  track_artifacts: true,
});
return { job_id: job.job_id, directory: job.artifacts.directory };
```

启动结果中的 `artifacts.directory` 相对于所选客户端的工作区。harness 可以用原有授权下的 `fs.writeText`、`fs.write` 等工具写入该目录。命令及其子进程得到指向同一目录的 `TMPDIR` 和 `SSMCP_ARTIFACT_DIR`，本次显式启用时这两个变量优先于 `env` 中的同名值。示例通过本次命令的 `--target-dir` 指定 Cargo 产物位置。普通命令省略产物选项时继续使用原有环境和启动行为。

```js
const status = await exec.status(clientId, jobId);
return status.artifacts;
```

同一已验证 Agent 在原连接上调用 `exec.status` 或 `exec.read` 时，harness 合并短时间内的目录事件，并返回当前 `logical_bytes`、`soft_limit_bytes` 和 `incomplete`。在 `exec.read` 里这些字段位于 `snapshot.artifacts`，当前官方 `exec.run` 也会带回该读取结果。首次观察到用量超过额度时，该次结果携带一个 `notice`，列出至多三个最大的已计量文件，并提示只检查已经确认可重建的产物。文件名只是展示数据。观察到用量降至额度的 80% 或以下后，下一次超额可再次提醒。提醒随读取响应返回一次，传输中断时没有单独的确认或重发。其他 Agent 和共享任务列表不会收到这条提醒。

- 20 GB 额度是软提醒。越线时命令继续运行。默认保留目录和文件，可在命令退出后继续计量，直到任务被遗忘、从保留列表淘汰或连接关闭。
- 计量的是专用目录中可确认的单链接普通文件的逻辑长度。稀疏文件、压缩、写时复制、reflink 和快照使它与 Btrfs 实际占用不同。目录项、符号链接、已知硬链接和跨挂载路径不计入。共享路径、外部服务以及不遵守目录变量的程序需要单独管理。这个目录基于现有工作区权限，具备相同系统账号权限的程序也能写入，归属按落盘位置判断。
- Linux 需要 `openat2`、`inotify`、可用的 `/proc/self/fd` 和工作区写权限。每次采样最多读取 256 KiB 的事件，正常增量只复核变化路径。队列溢出或超过界限时将 `incomplete` 标为 `true`，仅对该任务目录重建账本，每次最多检查 8,192 个条目、深度 24。成功重建会恢复当前计量，历史缺口仍标记为 incomplete；无法重建时 `logical_bytes` 为 `null`。通知只反映读取时观察到的水位。两次采样间出现并消失的增量、Linux 不报告的修改（例如部分 mmap 写入或目录外硬链接别名的变化）可能不会被观测。

命令启动或产物跟踪初始化失败时，客户端会尝试回收本次创建、身份可确认且仍为空的任务目录。已含文件、被替换或无法确认身份的目录会保留。成功启动后的产物按所选的持久或临时模式处理。

### 临时产物的关闭和 reload 回收

在 `exec.start` 显式传入 `temporary_artifacts: true`，可为任务独占、可丢弃的产物启用关闭回收，同时启用产物追踪和同一个软额度。需要保留的产物用默认的持久模式（仅传 `track_artifacts: true`），或在关闭前将需保留的结果写到临时目录之外。备份、共享缓存和仍在被其他服务使用的路径应保留在临时目录之外。

正常关闭与 reload 走同一条清理路径。只处理当前客户端持有的临时任务，且要求已确认 supervisor 正常退出并回收任务树、账本覆盖完整、没有活跃任务引用，同时对每个普通文件取得 Linux 独占文件租约。租约不可用或存在已打开的数据文件时保留整个任务。回收前复核目录和文件身份，临时隔离目录名，随后按已核对的父目录句柄逐项移除。遇到链接、跨挂载或身份变化会停下，不跟随链接或接管另一连接的目录。具备同一系统账号权限的程序仍可修改工作区，使用临时模式应保证任务独占，避免在回收期间重命名或修改该目录。

客户端最多等待回收线程 750 毫秒，每个任务最多检查 2,048 个条目。超时后不会开始新的删除操作，已进入内核的文件系统调用仍受操作系统调度。日志记录已回收文件数和逻辑字节；退出状态不可验证、事件丢失、并发使用、超出界限或其他回收失败时记录保留项。中途失败可留下原目录或日志所示的隔离名。回收只使用本次连接持有的有界记录，覆盖部分已忘记或从列表淘汰的临时任务。异常进程退出和后续启动会保留落盘产物，后续客户端不会扫描或接管无归属目录。配置、工具、状态、工作树和其他 `.ssmcp` 内容均不在回收范围内。

## 目录同步

两端需要通过系统包管理器安装 `rsync`。ssmcp 使用系统 rsync 的协议和增量传输；协议字节沿现有客户端连接转发，不经过模型文本。

同步复制源目录的内容。两端目录必须已经存在，可以先按任务需要调用 `fs.mkdir`。第一版保留普通文件内容、目录结构和修改时间，跳过符号链接和设备，不启用所有权、权限位、ACL 或扩展属性的保留；它适合工作文件同步，完整系统备份应使用相应备份工具。

先预演，查看将发生的变更：

```js
return await rsync({
  source: { client_id: developmentId, path: "src/" },
  destination: { client_id: workspaceId, path: "src/" },
  dry_run: true,
  delete: false
});
```

`sync` 是 `rsync` 的别名。两个方法返回托管任务的 `id` 和当前状态，任务可在这次 JavaScript 调用结束后继续。查询一次即可获得截至当时的状态：

```js
return await jobs.wait(jobId, { wait_ms: 5000 });
```

`jobs.status(jobId)` 立即返回状态，`jobs.list()` 返回当前授权下的任务摘要，`jobs.cancel(jobId)` 请求停止并清理。任务完成后检查 `state`、两端 `exit_code` 和变更日志；失败时读取 `error`。详细日志只保留在单任务结果中，避免任务列表变得过大。

执行已经授权的实际同步时，把 `dry_run` 设为 `false`。只有确实希望删除目标端多余文件时才使用 `delete:true`；它的默认值为 `false`。rsync 根据文件大小和修改时间选择需要更新的文件，传输阶段使用系统 rsync 的增量协议。

反向传输只需交换两个端点：

```js
return await rsync({
  source: { client_id: workspaceId, path: "src/" },
  destination: { client_id: developmentId, path: "src/" },
  dry_run: false,
  delete: false
});
```

每次同步都有一个明确的源。两边同时修改了同一个文件时，先检查差异、确定保留内容，再选择同步方向；本接口不提供冲突合并，也不要同时运行相反方向的复制。避免把两个指向同一物理目录的注册当成两个独立工作区。

## 双向端口映射

映射由“在哪个客户端监听”和“连接哪个客户端的服务”两部分组成。例如，把开发机的 3000 端口接到调用方工作区的 8080：

```js
return await ports.open({
  listen: { client_id: workspaceId, host: "127.0.0.1", port: 8080 },
  target: { client_id: developmentId, host: "127.0.0.1", port: 3000 }
});
```

先确认返回的 `state` 为 `running`。`ports.open` 最多等待 8 秒；若仍为 `starting`，通过 `ports.list()` 找到同一 `id` 并继续观察，待 `listener` 可用后再读取地址。

随后在 **workspaceId 对应的机器** 访问 `http://127.0.0.1:8080`。需要自动选空闲监听端口时使用 `port:0`，读取返回的 `listener.port`。反方向映射交换两个客户端即可。

```js
return await ports.list();
```

```js
return await ports.close(mappingId);
```

映射在调用结束后继续工作，支持双向字节传输和 TCP 半关闭：一端停止发送后，另一端仍可把回复发完。监听和目标只允许回环地址，默认 15 分钟后结束，可用 `ttl_ms` 指定 1 秒至 1 小时。到期、授权失效或客户端断开都会触发清理。

关闭操作会等待有界清理。如果客户端持续忙碌，结果会保留 `cleanup_failed` 和待清理句柄；再次调用关闭或取消可重试，不会把未完成清理报告为成功。

### 两台设备优先直连

已配对的两个客户端可以让应用数据走双向认证的 TLS 1.3 连接。两端仍需连到同一个中继，调用者仍需有权操作两台设备。可以使用固定 TCP 候选，或在明确允许的网段内自动发现本机地址并交换候选；应用监听和目标服务都限于回环地址。两种模式都需要已经可达的 TCP 路径。

在两台设备上各放一份私有 JSON 文件，保存在共享项目目录之外。用 `openssl rand -base64 32` 为这一对设备生成一次随机密钥，通过可信渠道将同一个值填入两份文件的 `pair_key`。每一对设备使用独立密钥。在 Unix 上将文件权限设为 `0600`，在 Windows 上仅向运行客户端的用户授予读取权限。

下面是应用监听端的配置示例。将 `candidate` 改为监听端确实能够访问的目标设备地址与专用 TLS 端口，将 `target` 改为目标设备上要转发的服务。

```json
{
  "role": "listen",
  "listen": "127.0.0.1:0",
  "target": "127.0.0.1:3000",
  "candidate": "192.0.2.20:44443",
  "pair_key": "替换为这对设备的32字节随机密钥的Base64"
}
```

目标设备使用相同的 `listen`、`target`、`candidate` 和密钥，将 `role` 设为 `target`。目标端默认在 `candidate` 的具体地址上接受 TLS 连接。如果网络已配置静态地址转换，可用目标端独有的 `bind` 指定对应的本地 IP 和端口。候选只接受单个 IP 字面量与端口。两台设备各自设置 `SSMCP_PEER_PORT_CONFIG` 为其私有文件路径，然后启动现有的已授权 SSMCP 账号中继连接。配置在新连接建立时加载。

#### 自动发现本机候选

自动模式省去对端 IP 和 TLS 端口的逐台配置。把固定配置中的 `candidate` 替换为 `auto_candidates`，填写允许用于发现与连接的 CIDR 网段。例如，两台设备都接入 `192.168.50.0/24` 时，监听端可使用：

```json
{
  "role": "listen",
  "listen": "127.0.0.1:0",
  "target": "127.0.0.1:3000",
  "auto_candidates": ["192.168.50.0/24"],
  "pair_key": "替换为这对设备的32字节随机密钥的Base64"
}
```

目标端使用相同的应用端点、网段与密钥，将 `role` 改为 `target`。`candidate` 与 `auto_candidates` 必须二选一，自动模式不接受 `bind` 覆盖。网段必须写为规范的 IP/CIDR 字面量，例如 `192.168.50.0/24`、`fd12:3456:789a::/64`；最多 16 项，不接受域名、`/0` 或带主机位的网段地址。

目标端从已启用的本机接口中筛选允许的 IPv4/IPv6 地址，排序去重后最多保留 4 个，为每个地址预留操作系统分配的 TCP 端口。预留的端口在对端授权验证前不接受连接。监听端验证完整候选列表的配对签名和自己的网段策略后，以最多 2 个并发探测候选，每个探测最多 1 秒，整个选择阶段最多 2 秒。只有双向 TLS 认证和探测确认成功后，才开放应用监听端口；探测不连接应用目标。映射选定一个地址后，新应用流继续连接这个地址。

网段配置同时约束地址披露与拨号范围。自动候选的 IP 与临时端口会经过已认证的中继控制面，中继可以看到这些地址；配对密钥和证书私钥始终留在客户端。尽量只填写实际需要的局域网或 VPN 网段。候选不能是未指定地址、组播、链路本地地址、IPv4 映射 IPv6、带 scope/flow 标识的 IPv6 或重复地址；回环候选需要单独明确允许 `127.0.0.0/8`（或其子网）或 `::1/128`，适合隔离测试。

本机接口枚举使用 Unix `getifaddrs`，Linux 有运行验收。macOS 使用同一实现，尚未完成平台验收。Windows 目标端目前返回已签名的空候选列表，`prefer_direct` 经双端授权验证后使用中继，`direct_only` 失败；固定候选模式保持原有行为。

自动模式使用独立的 `peer_ports_auto_v1` 能力，仅在双方和中继均支持时启用。固定模式继续使用 `peer_ports_v1`，授权记录和 TLS 数据格式保持兼容。自动与固定模式混用、自动端连接旧客户端时，支持 `transport` 的服务器在 `prefer_direct` 下使用中继；需要和 PR #179 原版直连时，双方继续使用固定配置。PR #179 之前的服务器不接受 `transport` 字段，调用时应省略该字段并沿用原有中继格式。注册后重新读取的配置模式必须与协商结果一致。

#### 选择路径与关闭

```js
return await ports.open({
  listen: { client_id: workspaceId, port: 0 },
  target: { client_id: developmentId, port: 3000 },
  transport: "prefer_direct"
});
```

| `transport` | 建立映射的行为 |
| --- | --- |
| `prefer_direct`（默认） | 双方均启用相同模式时尝试候选，TCP/TLS 选择总时限 2 秒。未协商直连、自动发现为空或候选不可达时使用中继。 |
| `direct_only` | 仅在双方授权且 TLS 探测成功时提供应用监听端口。 |
| `relay` | 使用既有中继通道。 |

`ports.open` 和 `ports.list` 的 `transport` 字段显示实际路径。回退原因记录在 `fallback_reason`。密钥、角色、服务配置或对端认证不匹配会使协商失败。客户端只在建立映射时选路，已传送的应用数据不会重放。旧客户端通过原有中继模式工作，旧服务器请沿用原有调用格式。

每次映射使用新的双端证书。本地配对密钥认证两份授权记录，其中包含确切的客户端 ID、角色、应用端点、候选、证书与有效期。固定候选留在本机配置，自动候选随授权记录发送；两种记录使用不同的签名域。候选篡改、身份或网段策略不匹配均终止协商，不触发中继降级。两端时钟需要与中继同步，服务端签发的协商窗口为 5 秒。中继每秒复核授权，任一端断开、关闭或授权撤销触发清理。控制链路中断时，客户端最长 6 秒后自行终止数据面。每个客户端同时支持一条直连映射，每条映射最多两条活动连接；自动模式的多个监听地址共享这两个连接名额。关闭映射后可以重新发起协商。

`relay_payload_bytes` 按两个方向累加中继从发送端读到的原始应用字节，每个字节只计一次。控制消息、Base64 与传输封装不计入该值。双方通过直连传输时该值为零，中继仍承担控制流量。

#### 网络覆盖边界

自动发现只枚举本机接口并连接已经签名、位于本地允许网段内的候选，不扫描网段。它适用于局域网、已有路由的私网/VPN，以及确实允许 TCP 入站的接口地址。这一版没有 STUN、ICE、UPnP、反向拨号或 NAT 打洞；两个 NAT 后没有现成可达路径的设备继续使用中继。主动穿透能力仍需实现，公网双 NAT 端到端验收也尚未完成。

[RFC 8489](https://www.rfc-editor.org/rfc/rfc8489.html#section-6.2) 区分 UDP 与 TCP 的 STUN 传输；UDP 查询获得的地址映射不能直接作为本模块的 TCP 候选。[RFC 6544](https://www.rfc-editor.org/rfc/rfc6544.html#appendix-B) 的 TCP 穿透还涉及同一本地端口的绑定、同时打开、候选检查及保留选中的连接。当前数据面每条应用流重新建立 TLS 连接，后续实现 NAT 打洞时需要一并处理连接复用、映射寿命和多流承载。[RFC 8445 §19.1](https://www.rfc-editor.org/rfc/rfc8445.html#section-19.1) 讨论候选地址披露与接口选择，自动模式据此要求本地显式网段授权。

## 动态资源与 skill

服务器实现标准 `resources/list`、`resources/templates/list` 和 `resources/read`。资源内容按当前身份和端点过滤，动态读取重新验证连接和权限：

| URI | 内容 |
| --- | --- |
| `ssmcp://guide` | 环境区分、内置 API 与操作示例 |
| `ssmcp://limits` | 源码、输出、并发、传输等限制 |
| `ssmcp://skill` | 项目随附的使用 skill |
| `ssmcp://clients` | 当前可访问的在线客户端 |
| `ssmcp://clients/{id}/info` | 指定客户端的实时信息 |
| `ssmcp://clients/{id}/tools` | 该客户端的工具 schema 和分页信息 |
| `ssmcp://clients/{id}/agents` | 项目指令全文和来源 |
| `ssmcp://clients/{id}/skills` | 项目技能摘要与全文 readUri |
| `ssmcp://clients/{id}/skills/view` | 项目叠加显式全局根的实时技能视图 |

不方便直接调用 MCP 资源方法的宿主，可以从 `code_mode` 读取同一资源：

```js
return await resources.read("ssmcp://guide");
```

```js
return await resources.list();
```

资源当前采用主动读取，未启用订阅通知。通过 make install 安装时，skill 一并放入安装前缀下的 share/doc/ssmcp/skills/ssmcp。目录中随附 [skills/ssmcp/SKILL.md](../skills/ssmcp/SKILL.md)，可复制到所用 AI 客户端支持的 skills 目录；也可以直接通过 `ssmcp://skill` 读取。它指导 AI 发现客户端、确认环境、预演同步、检查任务状态和关闭映射。

## 容量与部署

服务端同时管理最多 2 个同步任务、4 个端口映射和 2 条通过中继转发的连接，以保留普通工具调用的并发额度。每客户端最多 64 个 TCP 句柄、8 个 rsync 端点；同步协议分块 32 KiB，中继端口转发分块 8 KiB，以适配旧客户端的原生结果大小限制。同步日志有上限。任务历史保留最多 32 条，已结束条目按创建时间淘汰，待清理条目继续保留。

服务端使用系统 Node.js、bubblewrap 和 util-linux 提供的 `prlimit`；仅文件同步的两端另需 rsync。更新服务器和参与工作的客户端后，新能力才可发现；私有连接协议继续使用 v4。既有原生工具的权限、审批和 OAuth 规则保持原有语义。

参见 [工作区桥接验证记录](validation-workspace-bridge.md) 和 [自动候选隔离验收](validation-auto-peer-candidates-20261122.md)，了解各次已执行的测试、实际覆盖范围和接入条件。
