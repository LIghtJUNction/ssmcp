# 临时图片链接

`link` 为已授权客户端工作区中的图片创建临时下载链接。本文描述候选源码契约，不表示当前服务已经部署或开放该功能；使用前先确认实际 MCP 工具目录与服务版本。

## 调用入口

这是**顶层 MCP 工具**，原始名称为 `link`，连接器中显示为 `ssmcp_link`。应直接调用该入口；`call(clientId, "ssmcp.link", ...)` 不是这个功能的调用方式。

创建时，服务端沿当前调用者的授权访问目标客户端，并调用其原生 `ssmcp.fs.read_image` 读取图片。单张图片仍受 **400 KiB** 限制。创建响应提供 URL、MCP `resource_link` 和链接 `id`，不返回图片 base64。

## 操作与字段

| 操作 | 输入 | 结果 |
| --- | --- | --- |
| 创建链接 | `action: "create"`、`client_id`、`path`，可选 `ttl_secs` | 返回临时 URL、`resource_link` 与 `id` |
| 清理链接 | `action: "cleanup"`、创建响应中的 `id` | 清理该链接；仅原创建者可执行 |

| 创建字段 | 含义 |
| --- | --- |
| `client_id` | 从当前客户端列表取得的实际目标 ID |
| `path` | 目标客户端工作区内、可由 `ssmcp.fs.read_image` 读取的图片路径 |
| `ttl_secs` | 有效期秒数；默认 120，最多 900 |

直接向顶层 MCP `link` 传入以下扁平 JSON。将 `CLIENT_ID_FROM_LIST` 和图片路径替换为实际值；省略 `ttl_secs` 时使用默认有效期：

```json
{"action":"create","client_id":"CLIENT_ID_FROM_LIST","path":"images/example.png","ttl_secs":120}
```

清理时仍调用同一个顶层 MCP `link`，将 `LINK_ID_FROM_CREATE` 替换为创建响应中的 `id`：

```json
{"action":"cleanup","id":"LINK_ID_FROM_CREATE"}
```

## 下载与失效

**第一次 GET 请求会消耗链接。** 浏览器打开图片、聊天预览或下载器的预取，只要发起 GET，也会占用这一次下载。准备把链接交给目标使用时，避免提前用 GET 预览；需要先查看响应头，可发送 HEAD，HEAD 不消耗链接。

不再需要的链接可由原创建者使用返回的 `id` 执行 `cleanup`。未使用的链接也会在 TTL 到期后失效；过期或已消耗的链接不能作为长期图片地址。

## 部署与容量

图片保存在服务端有界 RAM 中，总预算为 **64 MiB、最多 128 个对象**。公开下载入口只由 `ssmcpd` 的 public router 提供，生成 URL 的基础地址来自服务端配置的 `public_url`；仅有客户端图片读取能力，不代表当前服务已有这个下载入口。

相关说明：[图片读取与输入](client-input.md) · [MCP 调用与代码模式](code-mode.md)
