# 工作区同步与端口映射

ssmcp 通过已经认证的客户端连接，在两个工作区之间传文件或转发 TCP。公开 MCP 工具仍然只有 `list` 和 `code_mode`，文件、同步任务和端口映射由 `code_mode` 编排；资源同时提供标准 MCP 读取入口。

## 先确定每一端在哪里

| 名称 | 实际位置 | 怎样标识 |
| --- | --- | --- |
| 开发机工作区 | 启动 ssmcp 的项目目录 | `list` 返回的客户端 ID、hostname 和 cwd |
| 调用方工作区 | ChatGPT 沙箱、另一台电脑或另一个项目目录 | 在该环境启动独立 ssmcp 客户端后返回的 ID |
| 中转服务器 | 运行 ssmcpd 和隔离 JavaScript 的机器 | 负责认证、路由和托管任务；不充当任一工作区 |

`code_mode` 在服务器运行。源路径和目标路径始终相对于所选客户端的启动目录；端口始终属于所选客户端的网络环境。要把 ChatGPT 沙箱作为一端，需要该沙箱能够运行客户端、访问 SSH 入口，并拥有已登记的公钥。

在两端各自的共享目录启动客户端，使用当前服务器支持的身份授权方式。例如：

```sh
cd /path/to/project
ssmcp --name development --allow YOUR_GITHUB_LOGIN@github user@ssmcp.example
```

```sh
cd /path/to/workspace
ssmcp --name workspace --allow YOUR_GITHUB_LOGIN@github user@ssmcp.example
```

先调用 `list`，核对两个客户端的 `hostname` 和 `cwd`。下文示例放入 `code_mode.code`，其中 `developmentId` 和 `workspaceId` 应替换为实际发现的 ID。客户端重连会更换 ID，应重新发现。不同客户端可以运行任意 `-t` 后端，工作区能力与后端类型无关。

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

服务端同时管理最多 2 个同步任务、4 个端口映射和 2 条正在转发的连接，以保留普通工具调用的并发额度。每客户端最多 64 个 TCP 句柄、8 个 rsync 端点；协议分块 32 KiB，同步日志有上限。任务历史保留最多 32 条，已结束条目按创建时间淘汰，待清理条目继续保留。

服务端使用系统 Node.js、bubblewrap 和 util-linux 提供的 `prlimit`；仅文件同步的两端另需 rsync。更新服务器和参与工作的客户端后，新能力才可发现；私有连接协议继续使用 v4。既有原生工具的权限、审批和 OAuth 规则保持原有语义。

参见 [验证记录](validation-workspace-bridge.md) 了解已执行的测试、实际覆盖范围和接入条件。
