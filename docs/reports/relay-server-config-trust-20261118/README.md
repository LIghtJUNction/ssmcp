# ssmcp 默认中转服务器与公钥信任验收

日期：2026-11-18（Asia/Taipei）

实现提交：[91d92db](https://github.com/LIghtJUNction/ssmcp/commit/91d92dbbf5ffc391061008868b19577e874b35e1)。

## 已完成

客户端统一了默认服务器选择：本次命令的 `--server`、配置文件的 `[client].server`、最后登录的服务器、内置 `https://ssmcp.lmm.best`，按此顺序取第一个有效设置。登录、账号查询、邀请、兑换邀请和普通 HTTPS/WSS 会话都使用同一规则；账号令牌继续按完整 origin 隔离。

设置命令：

```sh
ssmcp config set server https://relay.example.com
ssmcp config get server
ssmcp config path
ssmcp login
ssmcp invite
```

单次覆盖：

```sh
ssmcp invite --server https://other.example.com
ssmcp --server https://other.example.com invite
```

默认配置文件为 `~/.config/ssmcp/config.toml`，可直接编辑：

```toml
[client]
server = "https://relay.example.com"
```

命令修改保留原有 UI 设置和注释。自定义文件可使用绝对路径环境变量 `SSMCP_CLIENT_CONFIG`。完整平台路径、删除默认覆盖及登录回退规则见[操作指南](https://github.com/LIghtJUNction/ssmcp/blob/91d92dbbf5ffc391061008868b19577e874b35e1/docs/client-server.md)。

## 服务器公钥信任

`trusted-servers.toml` 与客户端配置位于同一目录，默认是 `~/.config/ssmcp/trusted-servers.toml`。通过可信管理员渠道取得 HTTPS 入口的 PEM 公钥后导入：

```sh
ssmcp trust add --server https://relay.example.com --public-key ./server-public.pem
ssmcp trust path
ssmcp trust list
```

也支持 `--pin sha256//BASE64` 导入 SPKI SHA-256 指纹。信任文件接受完整 PEM 公钥和指纹，可保存多个值用于换钥期间的过渡。

客户端在 TLS 握手中保留证书链、域名、有效期和签名检查，再要求公钥匹配。HTTPS API 与 WSS 使用同一校验规则；错误公钥在发送账号或邀请令牌前被拒绝。无公钥记录时继续使用正常 HTTPS 校验。

对已固定公钥的服务器生成邀请时，命令要求 B 从可信来源预装新版客户端，并先把 A 的可信公钥指纹写入 B 的信任文件，再兑换邀请。此流程避免先执行未经公钥固定校验的远端安装脚本；未配置公钥的邀请保留既有下载安装流程。

## 本机验证

验证在 Linux 开发环境完成，构建并发限制为 1。

| 检查 | 结果 |
| --- | --- |
| `cargo check --bin ssmcp` | 通过 |
| `cargo test --locked --test client_server_config -- --test-threads=1` | 5 项通过 |
| `cargo test --locked --lib client:: -- --test-threads=1` | 195 项通过 |
| `cargo clippy --locked --lib --bins --test client_server_config -- -D warnings` | 通过，无警告 |
| `cargo fmt --all` 与 `git diff --check` | 完成；无空白错误 |

共 **200 项测试通过，0 失败**。CLI 集成测试覆盖手工配置、命令配置、参数优先级、邀请脚本与目标地址一致、服务器凭据隔离、登录与邀请兑换选址、配置错误及重定向拒绝。

9 项信任测试包含受控的真实 HTTPS/WSS 握手：正确公钥成功；错误公钥拒绝且服务端未收到 Authorization；正确公钥也不能绕过未知 CA、错误域名和过期证书检查；还覆盖多公钥轮换、PEM 与指纹一致、完整 origin 绑定、损坏配置和文件权限。

测试证书与私钥均为独立的公开测试夹具，位于 `tests/fixtures/relay-trust/`，不用于生产。没有使用用户提供的真实邀请令牌进行测试。

## 交付范围

本次更新客户端实现、命令帮助、README、示例配置和操作文档。测试没有更改真实账号凭据、默认中转服务器或现有信任记录。

公钥固定适用于 ssmcp CLI 的 HTTPS/WSS；浏览器与其他 MCP 客户端各自管理连接验证。公钥应从独立可信渠道取得。HTTPS 入口更换公钥时，需要先核验并更新信任；使用同一公钥续签证书无需修改。受信任服务器仍负责转发调用，服务器运维本身仍需可信。

本次未发布版本或部署服务器。
