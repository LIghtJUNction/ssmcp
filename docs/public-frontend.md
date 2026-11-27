# 独立公共前端与稳定核心

`ssmcp-frontend` 是独立的 Linux 进程，渲染公开指南和仓库内已知的静态资源。`ssmcpd` 核心持有 OAuth、账户存储、访问控制、流量账本、设备 WebSocket 与工具调用。更新公共前端时只替换并重启 renderer，不关闭核心的设备连接。[前端实现](../src/portal/frontend.rs) · [服务单元](../packaging/ssmcp-frontend.service)

这次拆分的边界有限：登录流程、动态账户页面和所有账户写操作仍由核心处理。它不是把整个 Portal 搬到另一个进程，也不是为不受信任的插件提供通用执行接口。账户授权持久化解决的是核心重启后有效凭据的恢复，见[OAuth 持久化](oauth-durability.md)；核心本身重启仍会中断网络传输。

## 数据经过哪里

| 请求 | 处理进程 | 发给 renderer 的数据 |
| --- | --- | --- |
| `/guide/`、`/guide/en`、`/guide/zh` | 核心转给 renderer；失败时内置页面 | 固定语言、核心配置中的公开服务器地址 |
| `/guide/style.css` | 核心转给 renderer；失败时内置 CSS | 固定操作名 |
| 已知静态资产 | 核心验证哈希后返回；失败时按哈希回退 | 已知资产名，例如 `portal.css` |
| OAuth、账户 API、计费、设备连接、工具调用 | 核心 | 不转发 |

HTTP 的 Cookie、Authorization、Host、查询参数和账户 subject 都不会进入公共前端协议。请求使用带标签的枚举，拒绝未知字段和未知资产名；没有任意文件读取、代理地址或命令执行操作。公开服务器地址来自核心配置，而非请求的 Host。[路由边界](../src/portal/guide.rs) · [资产映射](../src/portal/assets.rs)

核心只接受匹配协议版本、内容类型和 SHA-256 的响应。带内容哈希的 URL 必须返回对应字节：更新后的 renderer 可以提供新哈希资产，旧核心模板引用的旧哈希仍从内置资产返回。不会把新 CSS 偷换到旧哈希地址下。

## 启用

先构建独立程序：

```sh
cargo build --locked --release --bin ssmcp-frontend
```

首次安装时把程序放到服务单元指定的 `/usr/local/bin/ssmcp-frontend`，把 [ssmcp-frontend.service](../packaging/ssmcp-frontend.service) 安装到系统服务目录。单元沿用服务账户 `ssmcpd`、组 `ssmcp`，创建 `0700` 的 `/run/ssmcp-frontend`；socket 为 `0600`。这份单元不读取核心配置，不拥有账户数据库，并限制网络地址族为 Unix socket；默认账户和配置目录也通过单元设为不可访问。

在核心现有 TOML 的 `[portal]` 下加入：

```toml
[portal]
frontend_socket = "/run/ssmcp-frontend/render.sock"
# 原来的 state_dir、gateway_socket、流量配置等保留。
```

首次让核心加载该设置需要按既有维护流程重启核心一次；这一步会中断当前连接。此后只更新公开指南或静态资产时，不需要再重启核心。renderer 尚未启动、暂时退出或超时时，核心继续提供内置公开页面。

未使用 systemd 时，先由服务账户创建专用 `0700` 目录，再运行：

```sh
ssmcp-frontend --socket /absolute/private-directory/render.sock
```

核心和 worker 使用同一服务 UID。连接双方核验 Unix peer credentials；父目录逐级用 no-follow 描述符打开，拒绝符号链接、非预期所有者和不安全权限。worker 的单写锁防止两个进程抢占同一个 socket；正常退出只移除自己创建的 inode。配置不迁移或删除现有用户 socket。[私有路径实现](../src/local_socket.rs)

## 只更新公共前端

将验证过的新 `ssmcp-frontend` 文件先安装到同一目录下的候选路径，再用同文件系统原子替换程序路径，最后只重启 `ssmcp-frontend.service`。保持核心二进制、核心 unit 和持久状态不变。内核让已经运行的程序继续持有旧 ELF；新的 worker 进程使用替换后的程序。核心每次渲染请求重新连接私有 socket，因此后续页面请求可以使用新 worker。

示例部署步骤应由操作者在自己的维护流程中执行；本次开发验收没有安装或重启任何真实服务。包含动态账户逻辑、鉴权或协议变化的更新仍属于核心更新，不能只替换公共 renderer。

## 有界行为与失败回退

核心最多同时等待 16 个 renderer 请求，每个请求总超时 250 毫秒；超额立即使用内置内容。worker 最多同时处理 16 条连接，每条连接的处理上限为 2 秒。请求帧最多 4 KiB，响应元数据最多 1 KiB，正文最多 2 MiB。空闲进程等待事件，不进行后台轮询。退出时丢弃拥有的 JoinSet，取消 handler 并关闭其 socket；不启动子模型或外部命令。

没有配置 renderer 时，原来的公开页面直接由核心提供。renderer 错误、未知协议、错误哈希、超大响应和读取超时都不会使工具调用或 OAuth 依赖它。新资产哈希不存在时仍返回 404，不用不匹配的旧内容填充。

## 验收入口

[定向测试](../src/portal/frontend/tests.rs)覆盖公共请求白名单、私有路径/竞争进程、正常 IPC、超时与超大响应、内容哈希、敏感 HTTP header 不转发，以及真实 worker 替换。

真实替换回归保持一个核心 HTTP 服务和一条已验证的设备 WebSocket，使用私有账户及正常测试授权绑定。启动第一个 worker 后停止它，再启动第二个 worker；前后验证同一设备连接仍能 Ping/Pong、同一会话对象仍在线、原 OAuth AT 仍被核心接受，授权快照没有被前端操作改写。设置 `SSMCP_FRONTEND_TEST_BINARY` 可明确运行刚构建的独立 ELF；没有设置时使用同样生产 serve 函数的私有测试进程入口。

```sh
cargo build --locked --offline --bin ssmcp-frontend
SSMCP_FRONTEND_TEST_BINARY="$PWD/target/debug/ssmcp-frontend" \
  cargo test --locked --offline --lib portal::frontend:: -- --test-threads=2
cargo test --locked --offline --lib portal::assets::
cargo test --locked --offline --lib portal::guide::
```

测试代码存在不代表某个候选已通过。发布时应将实际结果、源码提交、ELF SHA-256 和服务配置绑定；部署后的公网反向代理与发行版 systemd 策略需在对应环境另行核验。
