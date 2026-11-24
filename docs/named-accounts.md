# 用户名入口与服务器管理

客户端用一个命令选择服务器和自己的用户名：

```sh
ssmcp alice@ssmcp.example.com
```

第一次连接时，客户端打开 GitHub 授权页面。页面显示申请的 ssmcp 用户名和 GitHub 身份；确认后绑定账号并继续连接。以后执行同一命令会复用这台服务器的凭据。无需先执行独立的登录命令。

## 用户名与身份

用户名是这台服务器上的名称。GitHub 数字 ID 用于确认账号归属；GitHub 登录名变化不会把 ssmcp 用户名交给其他人。不同服务器分别管理自己的用户名和凭据。

- 长度为 3–32 个 ASCII 字符，大小写统一为小写。
- 以字母开头，以字母或数字结尾；中间可用字母、数字、下划线和连字符。
- `admin`、`root`、`system`、`ssmcp` 等系统名称，以及内部使用的 `gh-` 前缀被保留。完整规则见 [username.rs](../src/username.rs)。
- 同一服务器内用户名唯一，一个 GitHub 账号绑定一个用户名。
- 已注册用户可以重新登录自己的名字。其他 GitHub 账号无法取得该名字。
- 已有账号首次使用新入口时绑定所选名称，原有设备归属和授权继续使用原 GitHub 数字 ID。

名称占用检查用于提前提示。最终绑定在服务器的同一次存储事务中完成；两个账号同时申请同名时只有一个成功。

## 修改用户名

登录门户后，在首页账户信息中选择“修改用户名”。改名后 GitHub 身份、设备归属、既有授权和登录会话继续有效，旧用户名保留给原账号。相同用户名提交、失败请求和 CLI 的首次用户名绑定均不计入改名次数。

服务器默认每个账号在 UTC 自然月内可成功改名一次。账户页显示本月剩余次数，服务器重启也会保留用量。管理员可以在 `ssmcpd.toml` 的 `[portal]` 段设置 `monthly_username_rename_limit`，例如设为 `3`，再重启服务。设为 `0` 会关闭账户页的改名功能。

## 日常连接

```sh
# 自定义服务器端口。
ssmcp alice@ssmcp.example.com:8443

# 没有桌面浏览器时，复制终端显示的链接到浏览器完成确认。
ssmcp --no-browser alice@ssmcp.example.com

# 为当前设备指定便于辨认的会话名。
ssmcp --name workstation alice@ssmcp.example.com

# 在另一个终端查看在线设备、进入终端。
ssmcp whoami
ssmcp list
ssmcp -i
```

凭据按服务器隔离保存。客户端验证服务器返回的 GitHub 身份和用户名，凭据过期或收到认证失效响应时，前台连接会重新发起授权。服务器暂时不可用、限流或拒绝访问时保留本地凭据，并显示具体错误。后台会话需要在前台完成授权，不会自行打开浏览器。

`ssmcp username@server` 使用 HTTPS/WSS。维护旧 SSH 连接时显式选择 `--ssh`，或使用已有的 SSH 专用参数；SSH 详细配置见[客户端配置](configuration.md)。

## 配置管理员

先用目标用户名完成一次连接和 GitHub 绑定，例如：

```sh
ssmcp maintainer@ssmcp.example.com
```

随后通过服务器自己的 SSH 管理账号登录，编辑 `/etc/ssmcpd/ssmcpd.toml`。在已有的 `[portal]` 段中配置：

```toml
[portal]
state_dir = "/var/lib/ssmcpd/portal"
gateway_socket = "/run/ssmcpd/management.sock"
audit_dir = "/var/lib/ssmcpd/audit"
admin_usernames = ["maintainer"]
default_rate_limit_bps = 8388608
```

保留配置文件中现有的 `[server]` 和 `[auth.github]` 设置。GitHub OAuth 回调地址仍为 `https://你的服务器/oauth/callback/github`。客户端 API、门户和连接注册应由同一个 ssmcpd 服务处理，示例配置见 [ssmcpd.toml.example](../packaging/ssmcpd.toml.example)。

```sh
sudo systemctl restart ssmcpd
```

服务启动时把管理员用户名解析为已绑定的 GitHub 数字 ID。未注册或被封禁的名字会使启动报错，管理员需要先完成注册或纠正配置。服务器配置决定管理员身份，网页提交的用户名和角色字段不能授予权限。

管理员用对应 GitHub 账号登录门户后，打开 `/account/admin`。

## 用户和连接管理

| 操作 | 生效范围 |
| --- | --- |
| 查看用户 | 本服务器注册的用户名、GitHub 身份、封禁状态、在线连接和限速设置 |
| 查看连接 | 连接名称、设备、工作目录、所有者及连接时间 |
| 断开连接 | 结束选中的当前连接；客户端启用自动重连时可再次建立连接 |
| 封禁用户 | 持久保存封禁状态，撤销客户端凭据、邀请、设备授权、门户会话和 MCP 授权，关闭该用户的连接并拒绝重连 |
| 解除封禁 | 允许重新授权和连接；此前撤销的凭据不会恢复 |
| 调整限速 | 更新该用户全部连接共用的中转预算，现有连接随即采用新值 |
| 恢复默认限速 | 清除该用户的覆盖值，采用服务器默认设置 |

管理员不能封禁当前操作账号。普通用户无法提交管理员操作。每次管理写入都验证登录身份、管理员权限、页面来源和防跨站请求令牌，并记录操作主体和目标账号。审计不记录登录令牌、工具参数或工具输出。

## 限速的单位和范围

默认每用户上传 **8 MiB/s**、下载 **8 MiB/s**，两个方向分别限速；每方向约为 **67.1 Mbit/s**。同一账号开多个连接共用预算，重连不会重新获得一个独立额度。管理员页面用 MiB/s 显示和编辑，服务器配置使用字节/秒。

`default_rate_limit_bps` 和单用户覆盖值允许 1–1,073,741,824 字节/秒。配置默认值需要重启服务；门户修改单用户值立即影响当前连接，并保存到账号存储。

限速统计 ssmcp 中转的应用消息字节，不控制用户电脑上其他应用的网络，也不计算 TLS/TCP 协议开销。WebSocket 消息按完整消息转发，大消息先等待共享额度；短时间的网络发送仍可能表现为消息级突发。预算允许最多 64 KiB 的初始突发。GitHub OAuth 和安装包下载沿用各自的请求限制。

## 验证实现

- [客户端入口和凭据](../src/client/account.rs)
- [用户名规则](../src/username.rs)
- [账号存储和原子绑定](../src/portal/store.rs)
- [浏览器确认和设备登录](../src/portal/setup.rs)
- [账号策略、凭据撤销和断开连接](../src/broker/management.rs)
- [共享带宽预算](../src/user_policy.rs)
- [HTTP/WSS 集成测试](../src/device.rs)
- [管理员页面权限和表单测试](../src/portal/dashboard_tests.rs)
