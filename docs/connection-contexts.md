# 选择本地或远程连接

连接 context 给一个目标起名字，例如 `here` 表示本机，`work` 表示一个 HTTPS 中继。随后用同一个 `--context` 参数选择目标，或为新连接保存默认选择。名字只负责找到端点；中继按设备指定的账号名单验证控制者。[选择实现](../src/client/connection_context.rs) · [账号与授权](client-accounts.md)

本文描述当前源码中的命令。运行 `ssmcp context --help` 可确认已安装版本是否支持。远程 context 使用 HTTPS；本地 context 使用现有 Linux 前台模式，其运行前提见[本地运行](local-mode.md#运行前提)。

## 建立两个目标

在开发机上创建本地目标和中继目标：

~~~sh
ssmcp context create here --local
ssmcp context create work --server https://relay.example.com
ssmcp context list
~~~

把示例地址换成自己的中继 origin，即协议、主机名和可选端口。不要附加 `/mcp`、账号密码、查询参数或片段。创建远程 context 会保存规范化的 HTTPS 地址，不会发起登录或连接。

名字长 1–64 个 ASCII 字符，以字母或数字开头，后面可使用字母、数字、点、下划线和连字符。重复创建同名目标会报错。

连接中继时，为这条被控连接指定允许的账号。

~~~sh
ssmcp --context work --username alice --name dev
~~~

本地目标可以直接启动：

~~~sh
ssmcp --context here --name local-dev
~~~

创建 context 不会改变终端所在目录。新连接仍使用当前工作区和已有连接选项；本地服务的监听地址、公开 URL 等参数仍由[本地模式](local-mode.md)管理。

## 临时选择与默认选择

`--context` 只影响本次命令。例如：

~~~sh
ssmcp --context work --username alice
~~~

在支持这种环境赋值语法的 Shell 中，也可以为一条命令指定：

~~~sh
SSMCP_CONTEXT=work ssmcp --username alice
~~~

要保存新连接的默认目标：

~~~sh
ssmcp context use work
ssmcp context show
ssmcp --name dev --username alice
~~~

SSMCP 按下列规则选择新连接的目标。

| 输入 | 实际规则 |
| --- | --- |
| 同时指定 `--context` 与另一个显式目标 | 报错；显式目标包括 `--server`、`local` 和 `用户名@服务器` |
| 仅指定 `--context NAME` | 使用该命名目标，优先于环境变量和保存的默认选择 |
| 指定显式目标，没有 `--context` | 使用显式目标，优先于 `SSMCP_CONTEXT` 和保存的默认选择 |
| 没有显式目标，设置了 `SSMCP_CONTEXT` | 使用环境变量指定的命名目标 |
| 没有以上选择，执行过 `context use NAME` | 使用配置文件中保存的 `current` |
| 没有选择命名目标 | 沿用原有服务器回退：`[client].server`、最后登录的服务器、内置地址 |

被选中的名字不存在、环境变量为空或名字格式无效时，命令会明确失败。一个过期的保存选择可以通过有效的 `--context NAME` 临时覆盖，或用 `context use --default` 清除。配置文件本身损坏或包含无效端点时，需要先修正配置。

`ssmcp list` 读取本机会话。查看中继上的在线设备时，在授权的 MCP 应用中使用 `list`。本地模式的 MCP 地址和授权方式见[本地运行](local-mode.md)。`config`、`ui`、`update` 等本地管理命令不接受前置 `--context`。

这些是 SSMCP 自己的选择规则。命名连接目标的组织方式参考了 [Docker contexts](https://docs.docker.com/engine/manage-resources/contexts/)，并不依赖 Docker 的环境变量或配置文件。

## 查看实际选择

~~~sh
ssmcp context show --json
ssmcp context inspect work
ssmcp context list --json
~~~

当保存的默认选择为 `work`，且没有临时覆盖时，`show --json` 输出：

~~~json
{
  "name": "work",
  "endpoint": "https://relay.example.com",
  "selection_source": "current"
}
~~~

`selection_source` 解释这次结果从哪里来：

| 值 | 含义 |
| --- | --- |
| `command_line` | 本次的 `--context` |
| `environment` | `SSMCP_CONTEXT` |
| `current` | 保存的默认命名目标 |
| `legacy` | 原有服务器回退；`name` 为 `null` |
| `argument` | `inspect NAME` 明确查询的那个名字 |

`list --json` 分别返回 `current` 和 `selected`：前者表示持久默认，后者表示本次环境或参数解析后选中的条目。查看命令读取本地配置，不建立连接，也不检查目标是否在线。

`ssmcp config get server` 继续显示原有服务器回退的结果。需要知道命名 context 的实际选择时，使用 `context show` 或 `context inspect`。

## 恢复默认与移除名字

~~~sh
ssmcp context use --default
ssmcp context rm work
~~~

`use --default` 清除保存的 `current`，让后续无覆盖的命令沿用原有服务器回退；它不会删除目标，也不表示切换到本机。它只编辑配置文件，因此已设置的 `SSMCP_CONTEXT` 仍然生效。在 Shell 中执行 `unset SSMCP_CONTEXT` 后，可用 `context show` 检查回退结果。

删除当前保存的默认目标会被拒绝。先选择另一个目标或清除默认，再执行 `rm`。删除名字不退出账号、不删除服务器公钥信任记录，也不停止已经运行的连接。

## 配置、账号与会话

`ssmcp config path` 给出实际配置文件位置。命名目标写入同一份 TOML：

~~~toml
[contexts]
current = "work"

[contexts.entries.here]
endpoint = "local"

[contexts.entries.work]
endpoint = "https://relay.example.com"
~~~

端点字段只接受本地模式或 HTTPS origin，不保存账号令牌、TLS 私钥或正在运行的任务。命令编辑时保留已有 UI、浏览器和 Git 配置。配置文件路径仍遵循 `SSMCP_CLIENT_CONFIG` 等[现有规则](client-server.md#直接编辑配置文件)。

切换中继后，请指定在该中继已登记的用户名。远程操作者通过 MCP 应用授权新中继。服务器公钥固定使用独立的[信任配置](client-server.md#信任公钥写到哪里)。

本地 context 沿用 Linux 前台运行方式，不增加后台会话管理。选择在创建连接时解析一次。保存的 session 记录解析后的具体目标；恢复旧 session、连接重试、正在执行的任务和已有问答队列继续使用原目标。`context use` 只改变以后创建的连接。连接 context 也不改变 `--context-root` 的文件访问范围，或 `/context`、`/switch` 管理的对话上下文。

## 排查选择问题

| 现象 | 检查方式 |
| --- | --- |
| 修改了默认选择，实际目标没变 | 查看 `context show --json` 的来源，检查 `SSMCP_CONTEXT` 或本次显式参数 |
| 提示未知 context | 用 `context list` 核对名字；环境变量失效时先清除它，或用有效的 `--context` 查询 |
| 提示两个显式目标冲突 | 保留一个选择入口，例如只用 `--context work` |
| 本地 context 不能执行账号命令 | 为该命令选择 HTTPS context，或显式指定中继 |
| 旧 session 仍连接旧目标 | 这是保存目标的恢复行为；需要另一个目标时创建新连接 |

参数和持久化边界由[模块测试](../src/client/connection_context.rs)及[实际 CLI 测试](../tests/connection_context.rs)覆盖；这些文档不代替某个发布版本的验收记录。
