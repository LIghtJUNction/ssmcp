# 首次连接与账号授权

当前客户端通过 HTTPS / WSS 连接中继服务器。先让控制者在该服务器注册账号，再在被控设备明确填写允许的账号；设备本身可以不登录。[账号与设备模型](client-accounts.md)

## 1. 控制者注册并登录

在控制者的终端运行：

~~~sh
ssmcp alice@relay.example
~~~

把 `alice` 换成要在中继注册的用户名，把 `relay.example` 换成实际服务器。首次使用时，客户端引导浏览器完成 GitHub 身份确认及用户名绑定；已有有效登录则复用。授权后命令继续连接当前设备，另开终端可检查账号：

~~~sh
ssmcp whoami --server https://relay.example
~~~

用户名是该中继保存的账号名称，采用 3–32 个 ASCII 字符，首字符为字母、末字符为字母或数字，中间还可使用 `_` 和 `-`。它绑定稳定账号 ID；GitHub 昵称、终端显示名和 agent 名称不能替代它。凭据按服务器分别保存。[用户名规则](named-accounts.md) · [登录与保存](../src/client/account.rs)

## 2. 在被控设备开放项目目录

先按[安装说明](releases.md)安装客户端，在要提供给工具操作的项目目录运行：

~~~sh
cd ~/projects/my-project
ssmcp --server https://relay.example \
  --allow alice@relay.example --name device-b
~~~

此命令明确允许 `alice` 控制这条连接，不要求被控设备保存账号登录。共享给多个已注册账号时，重复 `--allow` 或使用逗号分隔：

~~~sh
ssmcp --server https://relay.example \
  --allow alice@relay.example,bob@relay.example --name shared-device
~~~

`--allow` 中的服务器必须与连接服务器一致，非默认端口也必须一致。显式名单只授权列出的人；本机已有登录不会自动加入名单。被控设备没有有效登录时必须给出名单；已登录设备省略名单则仅允许自己。[解析与访问检查](configuration.md#github-allow)

## 3. 登录后查看和操作设备

控制者使用同一个服务器的已保存登录：

~~~sh
ssmcp list --server https://relay.example
ssmcp -i --server https://relay.example --client SESSION_ID
~~~

把 `SESSION_ID` 换成列表返回的设备 ID。Ctrl-] 离开远程控制界面，设备进程继续运行。`ssmcp sessions list` 查看本机保存的后台会话，与账号的在线设备列表用途不同。[命令说明](commands.md) · [后台会话](session-service.md)

在 ChatGPT 等 MCP 应用中，添加服务器的 `https://relay.example/mcp` 地址并完成该应用的 OAuth 授权。CLI 登录与 MCP 应用授权分别管理；两者均需真实账号身份，且只能访问授权名单匹配的设备。[客户端账号](client-accounts.md)

## 服务器、重连与撤销

可通过 `ssmcp config set server https://relay.example` 保存默认服务器；也可继续在命令中显式传入 `--server`。公开 origin、TLS 与服务器公钥固定的配置见[服务器连接](client-server.md)。

看门狗默认开启，连接中断后会重试；`--no-watchdog` 可关闭。重新连接仍须通过服务器的当前账号和授权检查。停止被控设备进程会关闭该连接；控制者退出登录或失去权限时，不能继续操作共享设备，但不会因此关闭其他获准控制者的连接。[连接生命周期](client-accounts.md#修改名单与连接生命周期)

当前版本使用 `--allow 用户名@服务器` 完成设备授权。旧客户端的 SSH 目标、`--ssh`、登录码以及 CLI 的 `invite` / `join` 不能用于这个流程；会话内 agent 的 `collab.join` 仍保留。[迁移说明](configuration.md#session-lifetime)

## 常见问题

| 现象 | 处理 |
| --- | --- |
| 未保存登录，也没有授权名单 | 在被控设备加 `--allow 用户名@服务器`，或先完成账号登录 |
| 提示账号未知 | 控制者先在目标中继注册用户名，不能直接填写 GitHub 昵称 |
| 提示授权目标属于其他服务器 | 核对 `--server`、名单中的主机、协议和端口 |
| 控制者的列表为空 | 核对登录账号、服务器、设备的名单及在线状态 |
| MCP 应用没有权限，但 CLI 可以访问 | 在该 MCP 应用中完成独立 OAuth 授权 |
| 旧服务端未确认账号访问能力 | 更新实际运行的服务端；新客户端不会回退到旧授权方式 |

服务端必须启用账号存储和 GitHub OAuth，并转发设备 WebSocket、账号 API、MCP 与 OAuth 路由。只更新本机客户端不会更新远端服务。[部署说明](deployment.md)
