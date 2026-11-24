# 在 Code Mode 中调用一个 MCP peer

连接一个已获授权的 MCP 网关，并从本机 SSMCP 会话调用它的工具。这个教程用现有的 `ssmcp` harness。在开始前，准备网关公布的完整 Streamable HTTP MCP URL，并通过该网关的 OAuth 流程取得 bearer。使用你能控制的测试网关。[认证与传输参数](agent-entry.md)

## 启动带 MCP peer 的会话

在 Bash 中执行以下命令。把 URL 换成网关实际公布的地址，保留路径。

~~~bash
read -r -s -p 'Peer OAuth bearer: ' SSMCP_PEER_TOKEN
export SSMCP_PEER_TOKEN
printf '\n'
ssmcp local --load-harness ssmcp -- \
  --endpoint https://peer.example/alice \
  --token-env SSMCP_PEER_TOKEN
~~~

终端的日志会显示 `ssmcp ready` 和已加载的工具数量。这个窗口负责保存连接。退出后，在启动它的 shell 执行 `unset SSMCP_PEER_TOKEN`。

用另一个终端或 MCP 应用连接本机 SSMCP 会话。在本机会话输入 `/logs` 取得 URL 和一次性授权码。授权后，在控制端调用 `list` 并记下本机会话的 ID。 [连接本机 MCP](local-mode.md)

## 检查目录并调用一个工具

将下列代码中的 `CLIENT_ID` 换成刚才发现的 ID，然后在 `code_mode` 执行。

~~~js
const id = "CLIENT_ID";
const status = await call(id, "ssmcp.harnesses.list", {});
const catalog = await tools.search(id, {
  name_prefix: "peer.",
  limit: 10,
  include_schemas: true,
});
return { status, catalog };
~~~

查看 `peer.` 下的真实工具名和 `inputSchema`。从返回目录选择一个只读工具，按 schema 填好参数，再执行 `call(id, toolName, arguments)`。在结果中检查 `isError` 和工具返回的数据。工具集合由你连接的网关决定。

## 刷新已配置网关的目录

网关的工具有变化时，用下面的调用刷新这条会话的目录。

~~~js
const id = "CLIENT_ID";
const refreshed = await call(id, "ssmcp.harnesses.load", {
  provider: "ssmcp",
  refresh: true,
});
return refreshed;
~~~

收到 `ready` 后重新搜索 `peer.`。这次操作沿用启动时设定的 URL 和认证。更换网关或 bearer 时，需要用新参数启动会话。关闭本机 SSMCP 窗口会断开它持有的 peer 连接。

## 从工具仓库复用这段流程

在本机 SSMCP 的工作目录执行 `ssmcp tools status`，确认已启用的源码层。若尚无项目层，可用 `ssmcp tools init project --git` 创建它。把下列导出合入项目的 `tools.js` 工厂，保留已有导出。

~~~js
core => ({
  refreshConfiguredPeer: id => core.call(id, "ssmcp.harnesses.load", {
    provider: "ssmcp",
    refresh: true,
  }),
})
~~~

在新的 `code_mode` 调用里执行 `return await refreshConfiguredPeer("CLIENT_ID");`。先查看外层 `structuredContent.tool_sources` 是否标记了项目层及当前文件的散列，再检查 `ready`。工厂只定义函数，调用函数时才会刷新目录。[工具仓库与来源回执](tool-repositories.md)

[通过 RC 管理多个 MCP 服务器的设计稿](mcp-server-rc.md)记录下一阶段的配置格式和生命周期。
