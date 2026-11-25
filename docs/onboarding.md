# 首次连接与账号授权

被控设备在选定的项目目录启动客户端。控制者通过 MCP 应用授权自己的中继账号。[账号与设备](client-accounts.md)

## 1. 取得控制者的用户名

控制者用 GitHub 登录中继的账户网页，在账户页查看平台用户名。设备启动命令中的 `alice` 就是这个用户名。`relay.example` 换成同一中继的域名和端口。[用户名与账户设置](named-accounts.md)

## 2. 在被控设备开放项目目录

先按[安装说明](releases.md)安装客户端，在要提供给工具操作的项目目录运行：

~~~sh
cd ~/projects/my-project
ssmcp alice@relay.example --name device-b
~~~

这条命令允许同一中继的 `alice` 账号控制本连接。`device-b` 是控制者在 MCP 应用中看到的设备名称。设备使用者可在当前连接的 Access 页面调整允许名单。[解析与访问检查](configuration.md#github-allow)

## 3. 在控制端查找设备

在 ChatGPT 等 MCP 应用中添加 `https://relay.example/mcp`，通过 OAuth 授权名单中的账号。在该应用调用 `list`，按设备名称和项目目录找到刚启动的连接，使用返回的客户端 ID 查询这台设备的工具。

如果要在被控设备本机查看本机会话，执行 `ssmcp list` 或 `ssmcp list --json`。用 `ssmcp attach NAME` 接入列表中的运行中会话。按 Ctrl-] 后会话继续运行。[后台会话](session-service.md)

## 服务器、重连与撤销

可通过 `ssmcp config set server https://relay.example` 保存默认服务器；也可继续在命令中显式传入 `--server`。公开 origin、TLS 与服务器公钥固定的配置见[服务器连接](client-server.md)。

看门狗默认开启，连接中断后会重试；`--no-watchdog` 可关闭。重新连接仍须通过服务器的当前账号和授权检查。停止被控设备进程会关闭该连接；控制者退出登录或失去权限时，不能继续操作共享设备，但不会因此关闭其他获准控制者的连接。[连接生命周期](client-accounts.md#修改名单与连接生命周期)

设备端用 `ssmcp 用户名@服务器` 指定控制者。旧客户端的 SSH 目标、`--ssh`、登录码以及 CLI 的 `invite` / `join` 不能用于这个流程；会话内 agent 的 `collab.join` 仍保留。[迁移说明](configuration.md#session-lifetime)

## 常见问题

| 现象 | 处理 |
| --- | --- |
| 首次启动时提示选择账号 | 在被控设备执行 `ssmcp 用户名@中继服务器` |
| 提示账号未知 | 控制者先在目标中继注册用户名，不能直接填写 GitHub 昵称 |
| 提示授权目标属于其他服务器 | 核对 `--server`、名单中的主机、协议和端口 |
| 控制者的列表为空 | 核对登录账号、服务器、设备的名单及在线状态 |
| 本机列表中有会话，MCP 应用的列表为空 | 确认该会话已连接，并核对 MCP 的中继地址、授权账号与设备名单 |
| 旧服务端未确认账号访问能力 | 更新实际运行的服务端；新客户端不会回退到旧授权方式 |

服务端必须启用账号存储和 GitHub OAuth，并转发设备 WebSocket、账号 API、MCP 与 OAuth 路由。只更新本机客户端不会更新远端服务。[部署说明](deployment.md)
