# 通过 RC 管理 MCP 服务器

这是设计稿。当前可用的入口是启动参数 `--load-harness ssmcp`、目录状态 `ssmcp.harnesses.list`，以及对已配置 peer 执行的 `ssmcp.harnesses.load`。现有 RC 选择 Code Mode 工具源码层。本文中的 `[mcp]` 和 `mcp.*` 签名待实现。[现有接入教程](mcp-peer-tutorial.md)

## 配置来源

沿用用户的 `ssmcprc.toml`。服务器配置可以内嵌，也可以委托给文件。RC 中的相对路径以 RC 所在目录为基准。导入文件只定义服务器。内嵌项和导入项使用同一 schema，重复名称或未知字段会给出包含来源的错误。

~~~toml
# 提案格式
version = 1

[mcp]
files = ["mcp/servers.toml"]

[mcp.servers.docs]
transport = "streamable-http"
url = "https://docs.example/mcp"
token_env = "DOCS_MCP_TOKEN"
~~~

对应的 `mcp/servers.toml` 中可以添加其他命名服务器。

~~~toml
# 提案格式
[servers.catalog]
transport = "streamable-http"
url = "https://catalog.example/mcp"
token_env = "CATALOG_MCP_TOKEN"
~~~

凭据配置保存引用。持有连接的 SSMCP 客户端解析环境变量，Code Mode 收到连接状态和工具目录。导入文件和仓库只保存可版本化的配置和代码。

## Code Mode 接口

工具仓库导出 `mcp` 辅助函数。每个函数收取目标 SSMCP 的真实连接 ID，由原生工具解析该客户端自己的 RC，连接 MCP 服务和发布目录。源码工厂在初始化时声明函数，运行期间由调用者调用。

| 提案接口 | 行为 |
| --- | --- |
| `mcp.list(clientId)` | 查看已配置的名称、来源和当前状态 |
| `mcp.load(clientId, {name})` | 读取该名称的配置，完成 MCP 握手并发布工具目录 |
| `mcp.unload(clientId, {name})` | 从目录撤下该服务器的工具并关闭对应连接 |
| `mcp.reload(clientId, {name})` | 重读该名称的配置和导入文件，验证替代连接，再更新目录 |

每个服务器有独立的工具名称空间。同名的并发加载共享一次握手。重新加载时，替代连接通过握手和目录读取后才切换。失败时保留旧连接，回执说明采用的配置来源和仍在服务的版本。卸载先停止接受新调用，正在执行的请求按现有取消和超时规则结束。助手源码的刷新发生在下一次 `code_mode` 调用，peer 的重新加载发生在拥有连接的客户端进程内。

## Agent 修改配置和源码

1. 用 `ssmcp tools paths` 确认目标客户端的 RC 路径和启用的源码层。
2. 按用户授权编辑指定的 RC、导入文件或项目工具仓库。保留其他条目及现有 Git 修改。文件写入沿用客户端的文件权限。
3. 变更配置时，先验证语法和来源，再对受影响的名称调用 `mcp.reload`。变更工具仓库时，发起新的 `code_mode` 调用，并检查外层 `tool_sources` 的路径、散列和 Git 状态。
4. 重新发现目标服务器的工具，执行一个已授权的调用。读取回执确认当前生效的配置和目录。

配置变更、原生调用许可和网关 OAuth 分别由各自的运行层执行。改动配置只能影响该客户端获得授权的连接。工具仓库的 Git 更新由现有源码层管理命令负责。

## 验收用例

- 同一个测试网关分别使用内嵌配置和导入文件完成连接，并返回一致的工具 schema。
- 同时连接两个网关。装载或卸载其中一个时，另一个仍可读取自己的工具。
- 更新一条配置后重新加载。重新发现目录，并在工具回执中看到替换版本。
- 将 URL 改成不可用地址并重新加载。调用返回错误，旧目录仍可用。
- 修改工具仓库中的辅助函数。下一次 Code Mode 调用回读新散列，并用它执行一次明确的加载或卸载。
