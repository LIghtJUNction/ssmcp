# 会话标题、客户端重载与长输出护卫

这三个功能让 agent 能够识别会话任务、重新加载已安装的客户端，并按需读取大量输出。公开 MCP 工具仍是 `list` 和 `code_mode`；`reload`、`outputs` 是 `code_mode` 内的 JavaScript 原语。

## 用标题说明本次任务

`--title` 把任务说明放进客户端元数据，agent 调用 `list` 就能看到它。标题支持中文和空格，最多 512 个 UTF-8 字节；空值、纯空白和控制字符会被拒绝。

```sh
ssmcp --name dev --title "完善 ssmcp 客户端重载和输出转储" \
  --allow octocat@github my-ssmcp
```

也可以写成 `--title="完善客户端重载"`。将 `octocat` 替换为获准访问的 GitHub 用户名，并使用已配置的 SSH 目标。

| 字段 | 含义 | 选择客户端时的作用 |
| --- | --- | --- |
| `id` | 服务端为这条连接分配的唯一 ID | 调用工具时必须使用它；重连后改变 |
| `name` | `--name` 设置的显示名称 | 帮助辨认机器或工作区，允许重名 |
| `title` | `--title` 设置的本次任务说明 | 帮助理解任务，允许重名 |
| `cwd` | 客户端启动目录 | 帮助核对所选项目 |

标题保持原文，并出现在连接信息、`list`、指定客户端详情和 `code_mode` 的 `clients` 数组里。它是操作者提供的描述，不会自动执行命令，也不改变 `--allow` 的访问规则。未设置标题时，元数据省略 `title` 字段。[参数与元数据实现](../src/client.rs) · [字段验证](../src/wire.rs)

在 `code_mode` 中先检查匹配数量，再用唯一 ID 操作目标：

```javascript
const matches = clients.filter(c =>
  c.name === "dev" && c.title === "完善 ssmcp 客户端重载和输出转储"
);
if (matches.length !== 1) {
  throw new Error("请通过 list 核对会话的 ID、标题和 cwd");
}
return matches[0];
```

带标题的握手需要支持 `title` 字段的服务端。旧服务端会拒绝未知的握手字段，因此首次使用前要升级服务端；未设置标题时保持原有元数据形状，新服务端也能接受旧客户端。

## 用 reload 重新加载客户端

`await reload(clientId)` 请求目标客户端从原启动入口重新执行，适用于已经安装新版二进制后的重载。它保留启动参数、启动目录和环境，因此 `--name`、`--title`、`--allow`、SSH 配置和其他参数会继续生效；它本身不会下载、编译或安装更新。

将新版安装到该客户端启动时使用的入口路径后，在 `code_mode` 执行：

```javascript
return await reload(clientId);
```

原生工具调用形式为 `await call(clientId, "ssmcp.client.reload", {})`。两种形式都经过现有客户端授权检查，不接受额外参数。[JavaScript 原语](../src/code_mode.js) · [重载实现](../src/client/reload.rs)

重载先确认请求，再关闭旧客户端的资源并重新执行程序。客户端检查启动入口仍可执行；有其他进行中的工具请求时，重载返回 busy。接受请求后，它返回 `accepted: true` 和 `status: "restarting"`，等待服务端回执后再开始重启。后台命令、本机 harness、同步和端口转发会随旧连接停止，不能依靠 `reload` 保留这些运行状态。

`accepted: true` 表示重载已接受，仍需确认新连接出现。旧 `clientId` 随连接关闭而失效；随后再次调用公开 `list`，通过标题、名字和 `cwd` 核对新连接，使用新的 ID。每次 `code_mode` 执行的 `clients` 是当次快照，旧快照不会自动更新。

| 授权方式 | 重载后的行为 |
| --- | --- |
| `--allow ...@github` | 新连接继续声明相同允许列表。已有 GitHub OAuth 授权仍有效且身份匹配时，可继续发现和调用新连接 |
| 一次性登录码 | 旧连接的授权失效。重连生成新登录码，需要重新完成 OAuth 登录 |

重载保留安装入口，包括入口末端的符号链接，因此更新该链接指向的新版本也能生效。它不会继续执行已被替换的旧程序映像。使用 `reload` 前需同时升级客户端和服务端，使两端都支持重载回执；旧客户端首次升级仍需手动重新启动。

## 长输出自动转储

长输出护卫在输出超过 **32 KiB** 或 **500 行** 时，将输出保存到客户端机器的 `/tmp/ssmcp/<uid>/output-<id>.log`，返回元数据供 agent 按需读取。`<uid>` 是运行客户端的本机用户 ID；路径属于该客户端机器，不属于 MCP 服务器或调用方沙箱。

流式捕获会保留触发阈值之前的前缀，并继续追加后续字节。短输出保持原来的内联形式；长输出返回文件 ID、路径、SHA-256、字节数、行数、完成状态和过期时间，避免把全部日志塞进一次工具返回。[输出存储实现](../src/output.rs)

不同输出来源有不同的文件范围：

| 来源 | 转储内容与范围 |
| --- | --- |
| 交互式 PTY | 整个终端会话的合并输出，包含提示符和终端控制字节；不是单条命令的日志 |
| 其他超大的原生工具结果 | ssmcp 实际收到的完整 MCP 结果 JSON，`format` 为 `mcp_tool_result_json` |

PTY 的 `terminal.read` 遇到长输出时返回 `spilled: true`、`artifact` 和 `range`。`range.start`、`range.end` 标记本次请求覆盖的字节区间，`next_cursor` 用于下一次终端读取；需要正文时根据 `artifact.id` 读取转储文件。原生工具自己已经截断或未提供的输出不能通过护卫恢复。[PTY 捕获实现](../src/pty.rs) · [原生结果护卫](../src/runtime.rs)

### 如何理解元数据

`artifact` 描述一个时间点上已经保存的原始字节。文件仍在增长时，后续读取到的大小和摘要可能变化；使用 `outputs.stat` 可以重新取得快照。

| 字段 | 含义 |
| --- | --- |
| `id`、`path` | 转储文件的标识与客户端本机绝对路径 |
| `scope` | 捕获来源，例如 `terminal_session` 或 `native_tool_result_json` |
| `bytes` | 当前快照已保存的字节数 |
| `sha256`、`sha256_scope` | 原始文件前 `bytes` 字节的 SHA-256；`first_bytes_snapshot` 表明摘要对应这个快照 |
| `lines` | 按换行符计数；末尾没有换行的非空记录也算一行 |
| `observed_bytes` | 捕获器已经收到的总字节数；发生容量限制时可能大于 `bytes` |
| `complete` | 捕获已经结束且没有丢失错误时为 `true`；运行中和不完整文件为 `false` |
| `error` | 存储失败、超限或提前终止的原因；出现时不能把文件当作完整输出 |
| `expires_at_unix` | 根据最近写入或完成时间计算的过期时间，单位为 Unix 秒 |
| `retention_secs`、`max_bytes` | 保留时间和单文件容量上限 |

`complete` 表示捕获完整性，命令成功与否仍由退出码、超时或取消状态决定。保存完整的失败日志可以是 `complete: true`；正在运行的命令则还没有最终捕获结果。

### 按需读取正文

`outputs.read` 按字节偏移读取一小段文件，默认读取 16 KiB，每次最多 32 KiB。后续调用继续使用返回的 `next_offset`，不需要把全部文件重新返回给 agent。

```javascript
return await outputs.read(clientId, {
  id: artifactId,
  offset: 0,
  max_bytes: 16384
});
```

返回值包括 `text`、`data_base64`、`bytes`、`next_offset`、`eof` 和该段的 SHA-256。`text` 用于阅读；原始二进制或被字节边界切开的 UTF-8 字符以 `data_base64` 为准。此处 `sha256_scope` 为 `returned_chunk`，摘要只覆盖返回的片段。

`eof: true` 只表示已经到达本次读取所见的文件末尾；活动文件还可能增长。要检查整个文件的当前摘要、行数和完成状态，使用：

```javascript
return await outputs.stat(clientId, { id: artifactId });
```

这些操作也能通过 `ssmcp.output.read` 和 `ssmcp.output.stat` 原生工具调用。它们按转储 ID 访问，不接受任意文件路径；工作区文件仍使用 `fs` 原语。

## 自动清理与容量

转储区自动删除过期文件，并按时间淘汰超出容量的已结束文件。客户端初始化输出存储、创建转储文件时会清理一次；客户端运行期间每 60 秒再清理一次。

| 策略 | 默认值与行为 |
| --- | --- |
| 保留时间 | 24 小时；按最近写入或完成时间计算 |
| 已结束文件总量 | 512 MiB；超出后先删除最旧文件 |
| 单个文件上限 | 256 MiB；超出后保留已捕获前缀，返回不完整状态及错误 |
| 活动文件 | 写入期间持有文件锁，自动清理跳过它们 |
| 其他文件 | 只清理符合 ssmcp 命名及元数据格式的文件，不处理无关文件 |

已经创建转储文件后，单文件超限或磁盘写入失败会保留已写入的前缀，并标记 `complete: false` 和错误。若文件创建本身失败，结果会明确报错，可能返回 `artifact: null`、`output_error`，或原生工具错误，没有可读的转储文件。总量限制针对已结束文件；仍在写入的文件不会为了满足总量限制被删除，所以活动输出可能使实际占用暂时高于 512 MiB。

需要立即执行同一清理策略时，在 `code_mode` 调用：

```javascript
return await outputs.cleanup(clientId);
```

原生调用形式为 `await call(clientId, "ssmcp.output.cleanup", {})`。返回值包含删除文件数、删除字节数、保留字节数和活动文件数；该调用不会无条件清空目录。客户端全部退出期间没有清理任务运行，下次启动会继续清理。重载不会立即删除已结束的输出文件，只要它们尚未过期或被容量策略淘汰，就可以通过新客户端 ID 继续读取。

`--max-inline-output` 控制的是本机日志展示行数，与此处返回给 agent 的 32 KiB / 500 行护卫阈值独立。完整终端展示设置见[日志与主题配置](logging.md)。
