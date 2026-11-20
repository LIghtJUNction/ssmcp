# 选择中转服务器与固定服务器公钥

客户端把服务器地址和信任配置保存在本机。本文说明原有服务器回退和 HTTPS 公钥固定；命名 context 的优先级及本地／远程切换见[选择连接目标](connection-contexts.md)。登录、远程会话列表和新建 HTTPS / WSS 连接先解析显式目标或 context，再使用这里的服务器回退。每个服务器的账号凭据仍单独保存。[地址选择实现](../src/client/account.rs) · [配置命令](../src/client/config.rs)

## 设置回退服务器

用命令保存默认地址，再在该服务器登录：

~~~sh
ssmcp config set server https://relay.example.com
ssmcp config get server
ssmcp login --server https://relay.example.com
ssmcp whoami --server https://relay.example.com
~~~

`config set` 只修改 `[client].server`，保留已有 `[ui]` 设置与注释。`config get server` 输出原有服务器回退的结果；命名 context 生效时，用 `context show` 查看新连接的实际选择。`config path` 输出实际配置文件路径。移除显式设置使用 `ssmcp config unset server`，随后回退到最后登录的服务器；想明确恢复内置地址，可执行 `ssmcp config set server https://ssmcp.lmm.best`。

单次命令可以指定另一个服务器：

~~~sh
ssmcp login --server https://other.example.com
ssmcp whoami --server https://other.example.com

# --server 也可以放在账号子命令前。
ssmcp --server https://other.example.com whoami
~~~

`--server` 对本次命令生效，不改写 `[client].server`。`login` 成功后会更新登录记录中的默认服务器；配置文件里的显式设置仍优先。账号 API 与新建连接都使用实际选中的中继。切换到另一台服务器时，先在该服务器登录，客户端不会借用其他服务器的令牌。

### 直接编辑配置文件

运行 `ssmcp config path` 找到文件，在现有文件中添加或修改：

~~~toml
[client]
server = "https://relay.example.com"

[ui]
theme = "minimal"
~~~

已有 `[client]` 或 `[ui]` 表时，在原表中修改，保留自己的 UI 选项。地址须为完整 HTTPS origin，也就是协议、主机名和可选端口；例如 `https://relay.example.com:8443`。不填写 `/mcp`、其他路径、用户名、密码、查询参数或片段。HTTP 仅用于 `127.0.0.1`、`[::1]` 等字面量回环地址的本地测试。

没有命名 context 或其他显式连接目标时，服务器回退按以下顺序选择：

| 优先级 | 来源 | 使用方式 |
| --- | --- | --- |
| 1 | 当前命令的 `--server` | `ssmcp whoami --server https://relay.example.com` |
| 2 | 配置文件的 `[client].server` | 编辑 TOML，或运行 `ssmcp config set server URL` |
| 3 | 最后登录的服务器 | 由成功的 `ssmcp login` 更新账号存储中的 `default` |
| 4 | 内置地址 | `https://ssmcp.lmm.best` |

配置文件位置按以下顺序选择：

| 环境 | 实际路径 |
| --- | --- |
| 设置了 `SSMCP_CLIENT_CONFIG` | 该环境变量指定的绝对文件路径 |
| 设置了 `XDG_CONFIG_HOME` | `$XDG_CONFIG_HOME/ssmcp/config.toml` |
| Linux / macOS 默认 | `$HOME/.config/ssmcp/config.toml` |
| Windows 默认 | `%USERPROFILE%\.config\ssmcp\config.toml`；设置了 `HOME` 时优先使用 `HOME` |

`--ui-config` 和 `SSMCP_UI_CONFIG` 只选择显示配置。要改变服务器设置所用的配置文件，使用 `SSMCP_CLIENT_CONFIG`。账号凭据目录由 `SSMCP_AUTH_HOME` 等账号设置管理，详见[账号与凭据](client-accounts.md)。

## 信任公钥写到哪里

服务器公钥写入 **`trusted-servers.toml`**，它与 `ssmcp config path` 输出的配置文件位于同一目录。默认路径为 `~/.config/ssmcp/trusted-servers.toml`；自定义 `SSMCP_CLIENT_CONFIG` 后，信任文件也跟随到该文件的目录。用下面的命令查看实际位置和已保存的信任记录：

~~~sh
ssmcp trust path
ssmcp trust list
~~~

这里使用 HTTPS 入口证书中的 **SubjectPublicKeyInfo（SPKI）公钥**。TLS 由 Nginx、Caddy 或 CDN 终止时，应登记该入口向客户端提供的公钥。这些公钥用于 HTTPS / WSS 的 TLS 校验；现行客户端不通过 SSH 连接设备。

### 从可信渠道导入

向服务器管理员取得公钥文件，通过独立可信渠道核对服务器地址和公钥指纹，然后导入：

~~~sh
ssmcp trust add --server https://relay.example.com --public-key ./server-public.pem
ssmcp trust list
ssmcp login --server https://relay.example.com
~~~

`server-public.pem` 使用 `-----BEGIN PUBLIC KEY-----` 格式，只包含公开信息。管理员可在 HTTPS 入口所在机器，从实际使用的证书文件提取：

~~~sh
openssl x509 -in fullchain.pem -pubkey -noout > server-public.pem
~~~

该命令导出证书的 SPKI 公钥，不读取或输出服务器私钥。[OpenSSL x509 文档](https://docs.openssl.org/master/man1/openssl-x509/#certificate-printing-options)

若管理员提供的是公钥指纹，也可用 `ssmcp trust add --server URL --pin PIN` 导入。指纹格式是 `sha256//` 加 SPKI DER 字节的 SHA-256 摘要的 Base64 编码。管理员可以从已核验的公钥文件计算：

~~~sh
openssl pkey -pubin -in server-public.pem -outform DER -out server-public.der
openssl dgst -sha256 -binary server-public.der | openssl base64 -A
~~~

在输出前加上 `sha256//` 即为 `--pin` 的值。这里计算的是公钥编码的摘要；直接对 PEM 文本或整个证书文件求哈希会得到不同结果。[OpenSSL pkey 文档](https://docs.openssl.org/master/man1/openssl-pkey/#output-options)

首次登记的信任来源决定了这层保护的效果。通过管理员控制台、已有可信管理渠道或已核验的签名发布信息取得公钥；遇到公钥不匹配时，先向管理员核对变更原因，再更新本机记录。

### 直接编辑信任文件

每个服务器使用一个 `[[servers]]` 条目，`server` 填写与连接地址一致的 HTTPS origin。下例展示一个完整有效的 PEM 格式；**示例公钥只用于说明格式，实际使用时填写自己的服务器公钥**：

~~~toml
[[servers]]
server = "https://relay.example.com"
public_keys = [
    """-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=
-----END PUBLIC KEY-----"""
]
~~~

同一个示例公钥也可写成指纹形式。将上面的整个条目替换为：

~~~toml
[[servers]]
server = "https://relay.example.com"
pins = ["sha256//BuP9j9opu2CrWVV95h7bCuzbIxE0vjDnW0Vfjht5L6k="]
~~~

`public_keys` 与 `pins` 可以同时存在，并可各自包含多个值，便于轮换。一个服务器条目至少需要一个有效公钥或指纹。客户端会把地址规范化，按协议、主机名和端口精确匹配；不同服务器的信任记录互不借用。[信任文件与校验实现](../src/client/trust.rs)

### 连接时怎样校验

登记公钥后，客户端会在 TLS 握手阶段同时检查证书链、域名、有效期和公钥。HTTPS 账号 API 与 WSS 会话连接使用相同规则；公钥与保存的所有值都不匹配时，连接失败，不发送该服务器的账号令牌，也不自动退回普通证书校验。[账号 API](../src/client/account.rs) · [WSS 传输](../src/client/relay.rs)

未登记公钥的服务器使用正常的 HTTPS 证书校验。公钥固定增加了一条身份条件：即使冒充者获得了该域名的另一张有效证书，仍需持有已登记公钥对应的私钥才能通过握手。它适用于 ssmcp CLI 的 HTTPS / WSS 连接；浏览器和其他 MCP 客户端各自管理连接校验。受信任服务器本身仍负责转发工具调用，应由可信操作者维护。

CLI 的临时 `invite` / `join` 已移除。被控设备使用显式 `--allow 用户名@中继服务器`；完整流程见[账号与被控设备](client-accounts.md)。

## 续签、换钥与撤销

证书续签继续使用同一把公钥时，无须修改信任文件。准备更换公钥时，先通过可信渠道分发并添加新公钥，保留旧公钥；服务器切换后验证 `ssmcp whoami` 和会话连接，再从 `trusted-servers.toml` 中删除旧值。使用多个 HTTPS 入口或同时提供不同算法证书时，应登记这些入口实际使用的可信公钥。

要移除某台服务器的全部额外公钥约束：

~~~sh
ssmcp trust remove --server https://relay.example.com
~~~

移除后，新连接继续执行普通 HTTPS 证书校验。配置更新作用于随后创建的连接；已有连接需要重新连接，才能采用更新后的信任记录。
