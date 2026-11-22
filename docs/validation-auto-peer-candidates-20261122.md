# 自动 P2P 候选：实现与隔离验收

验证日期：2026-11-22（Asia/Taipei）。

## 结论与交付边界

本补丁在 PR [#179](https://github.com/LIghtJUNction/ssmcp/pull/179) 的双向 TLS 认证直连和中继回退之上，增加本地网段授权范围内的自动 host 候选发现、候选签名交换和有界建连。两端不再需要预填对端 IP 和专用 TLS 端口，但仍需配置应用服务、配对密钥和允许的网段。

主动 NAT 穿透仍未实现。STUN、标准 ICE、TCP 同时打开、UPnP、反向拨号和活动连接迁移均不属于本补丁。两个 NAT 后没有现成可达 TCP 路径的设备继续使用中继。公网双 NAT 的端到端验收也尚未完成。

源码基线固定为 PR 的 `cdcc9ecf240a98436f7d9d750acadefd9f9977a7`；它的父版本为 `add39e18ff92680ff3ac0c3e911407956e20b228`。本补丁功能与测试提交为 `02e58fb6ce99e6c0987bb31e82de81ee49800504`，后续文档提交不改变已测试的生产代码。本次没有将测试结果外推到其他主线提交或生产服务部署。`Cargo.toml` 与 `Cargo.lock` 没有改变，也没有向仓库加入二进制。

## 原有实现与本次改变

| 层次 | PR #179 | 本补丁 |
| --- | --- | --- |
| 目标地址 | 两端预先配置一个固定 TCP 候选 | 可选择固定候选，或显式 CIDR 内的本机接口发现 |
| 端口 | 预先指定；目标端可配置静态转换对应的 `bind` | 自动模式由操作系统分配并预留临时 TCP 端口 |
| 候选传递 | 固定候选留在本地配置 | 自动候选通过已认证的中继控制面交换 |
| 配对认证 | 配对密钥认证 mapping 与新证书 | 独立签名域再绑定整个有序候选数组 |
| 数据通道 | 每条应用连接使用 TLS 1.3 双向认证 | 沿用同一格式及证书验证机制 |
| 协商能力 | `peer_ports_v1` | 固定模式保留 v1；自动模式使用 `peer_ports_auto_v1` |
| 不可达处理 | 按 `transport` 回退或失败 | 沿用语义，增加已认证空候选的回退 |
| NAT 穿透 | 未实现 | 仍未实现 |

生产改动集中在 `src/peer_port.rs`、`src/peer_port/candidates.rs`、`src/peer_port/transport.rs`、`src/wire/peer_port.rs` 和既有 capability 传递路径。使用方法见 [工作区说明](workspaces.md#自动发现本机候选)。

`docs/workspaces.md` 原有“沙箱需要访问 SSH 入口”的接入说明也已纠正。当前 `--server https://… --allow …` 启动 `ssmcp __relay` 子进程，经 WSS `/api/client/connect` 连接中继；旧 SSH 设备入口已经拒绝执行。`register_ssh` 等内部名称和 `render::event("ssh", …)` 日志标签仍有历史残留，看到该标签不代表 P2P 走了 SSH。本补丁不重构这一共用控制连接模块；后续清理错误文案时还需保留额度拒绝识别的兼容前缀。

### 建连过程

1. 两端在私有配置中选择自动模式，客户端与中继协商独立能力。只有双方模式相同才尝试直连。
2. 目标端调用 Unix `getifaddrs`，筛选已启用接口中符合本地 CIDR 的地址，排序、去重，最多保留四个。它不广播、不访问 STUN 服务，也不扫描网段。
3. 目标端为各地址绑定临时 TCP socket，在签名前确定实际端口。此时只预留，不接受连接。应用监听端提交已签名的空候选列表。
4. 两端验证对方授权。完整 mapping、角色、客户端 ID、应用端点、证书、随机 offer ID、有效期和候选数组均进入 HMAC。应用监听端在发出任何候选 TCP 连接前，还检查自己的 CIDR 策略。
5. 目标端通过授权后才把预留 socket 转为 listener。应用监听端最多并发探测两个地址，单次最多一秒，总选择窗口最多两秒。成功探测不连接应用目标服务；完成 TLS 认证和协议确认后才开放应用入口。
6. 映射选定一个地址。每条应用流重新连接该地址并进行 mTLS；这沿用 PR #179 的数据面，并没有保留探测连接作为 NAT 隧道。

## 安全与兼容性约束

| 约束 | 实现及验收依据 |
| --- | --- |
| 明确的本地网络授权 | 自动模式要求 1–16 个规范 CIDR；拒绝域名、`/0`、带主机位的 CIDR、缺失模式以及自动 `bind` 覆盖。 |
| 认证先于拨号 | 篡改、签名有效但越界、重复候选均在拨号前拒绝；受控 TCP trap 验证没有收到连接。 |
| 空候选不能掩盖认证失败 | `peer_available=false` 和空列表仍要验证配对签名；错误密钥不会被转换成不可达回退。 |
| 端口暴露顺序 | 未验证对端之前仅绑定候选 socket；测试确认它占用端口但不接受 TCP。 |
| 地址处理 | 拒绝未指定地址、组播、适用前缀下的 IPv4 广播、链路本地、IPv4 映射 IPv6、带 scope/flow 标识的 IPv6；回环要求单独允许的回环 CIDR。 |
| 有界资源 | 最多四个候选，两个并发探测；多个目标 listener 共享既有两个活动连接名额。 |
| 生命周期 | 保留服务端五秒协商窗口、每秒授权复核和客户端六秒租约。关闭会终止活动流并释放所有候选 listener；待处理 offer 到期释放预留 socket。 |
| 授权边界 | 保留调用方对两端的访问控制、控制归属能力和原生工作区网络 provider 校验；hook 配置仍不能绕过。 |
| 固定协议兼容 | 固定 offer 不序列化新字段，v1 HMAC transcript 保持原格式；新能力不会被协商为旧能力。配置加载时再次核对模式。 |
| 应用数据 | 只在建立映射时选路；没有应用数据重放，没有传输中途自动迁移。 |

自动模式会向中继披露允许网段内的候选 IP 与临时端口。这是相对固定模式新增的信息披露，必须由本机配置明确启用；配对密钥和证书私钥不离开客户端。应使用具体的 LAN/VPN 网段，而不是宽泛网段。[RFC 8445 §19.1](https://www.rfc-editor.org/rfc/rfc8445.html#section-19.1) 讨论了候选地址披露与接口选择问题。

## 隔离方法

构建使用开发机独立 worktree，源码与主工作区分开。Cargo 使用既有共享目标目录和外层 owner lock；构建固定版本后立即复制验证二进制，避免后续共享目标中的同名文件被覆盖。依赖通过 `--locked --offline` 使用缓存，没有为测试启动模型或修改生产服务。

### 进程与协议

测试创建独立临时目录、私有配对文件、回环 HTTP/WebSocket 中继及本地测试身份。真实 CLI 子进程清空继承环境，使用隔离 HOME、认证目录和会话目录，不读取日常登录凭据。应用流经过真实 TCP 和 TLS；只用测试身份替代外部 OAuth 登录。

每个传输场景发送 131,101 字节，关闭发送方向后接收 196,619 字节，逐字节比较。直连要求 `relay_payload_bytes=0`；中继要求 `relay_payload_bytes=327720`。这是内容完整性与半关闭验收，不是吞吐量基准，也不表示控制流量为零。

### 两个网络命名空间

`src/peer_port/tests/netns.rs` 与 `scripts/peer_netns.py` 创建带唯一所有权标签的 Docker internal 网络和两个真实 CLI 容器。应用监听端另有独立的初始私网，以便断开通向目标的第二个接口。测试直接读取 `/proc/self/ns/net`，确认两端网络命名空间不同。配置只填写共享私网的子网；客户端自行发现接口 IP 和临时 TLS 端口。

控制面和测试应用通过私有 Unix socket 代理接入。直连数据路径在两个容器的私网接口之间传输；随后断开应用监听端的私网接口，保持 Unix 控制通道，验证不可达回退及 `direct_only` 失败。测试不修改宿主防火墙。

容器使用已有镜像（`--pull=never`）、只读系统挂载、清除 capabilities 和 `no-new-privileges`。清理只针对精确名称及所有权标签，正常结束还检查网络已经删除。

这个环境证明的是同一可路由私网中的跨网络命名空间行为。它没有两个 NAT 网关或两条公网接入，不能作为双 NAT 打洞证据。

## 本轮执行结果

| 检查 | 结果 |
| --- | --- |
| `cargo build --locked --offline --bin ssmcp` | 通过；固定验证二进制见下方摘要 |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --locked --offline --lib peer_port -- --include-ignored --nocapture --test-threads=1` | 32 通过、0 失败、0 忽略 |
| `client::relay::tests` | 7 通过 |
| `bridge::tests` | 4 通过 |
| `workspace::tests` | 11 通过 |
| `runtime::hooks::tests` | 1 通过 |
| `broker::allow` | 31 通过 |
| `wire::tests` | 13 通过 |
| `cargo clippy --locked --offline --all-targets -- -D warnings` | 退出码 0；有一条 Cargo 清单警告，见下文 |

上述相关路径共 99 项测试通过。没有声称执行全仓库所有测试。P2P 专项实际测试时间为 35.90 秒，不计构建时间；该时间不作为性能结论。

Clippy 完成时 Cargo 提示 `unused dependency qrcodegen`。该依赖已存在于 PR #179 基线的清单中，本补丁未改依赖，也没有为消除这一提示扩大修改范围。测试结束后独立检查确认没有带本次所有权标签的残余容器或网络。

双命名空间测试在实际分配的 `172.20.0.0/16` 子网中完成直连和明确中继传输，断开共享接口后 `prefer_direct` 回退并完成传输，`direct_only` 返回不可达且没有剩余映射。三次传输分别记录中继应用字节数 0、327720、327720。

多候选场景本轮收集到两个真实候选，策略仅允许 `127.0.0.1/32` 与 `::1/128`，因此包含 IPv4 和 IPv6 回环候选。测试没有单独断言应用流最终选中了 IPv6，不能将此扩展为完整 IPv6 端到端验收。命令、退出码、日志摘要与 SHA-256 见 [机器可读验证记录](reports/auto-peer-20261122/verification.json)。

### 真实 CLI 的十项组合

| 应用监听端 | 目标端 | 请求 | 预期实际路径 |
| --- | --- | --- | --- |
| 新自动 | 新自动 | `direct_only` | direct |
| 新自动 | 新自动 | `prefer_direct` | direct |
| 新自动 | 新自动 | `relay` | relay |
| 新自动 | 新固定 | `prefer_direct` | relay |
| 新固定 | 新自动 | `prefer_direct` | relay |
| 新固定 | 新固定 | `direct_only` | direct |
| 新自动 | PR #179 原版固定 | `prefer_direct` | relay |
| 新固定 | PR #179 原版固定 | `direct_only` | direct |
| 新自动 | PR 之前的客户端 | `prefer_direct` | relay |
| 新固定 | PR 之前的客户端 | `prefer_direct` | relay |

这些组合使用本补丁的测试中继。没有运行旧版中继二进制的端到端组合；旧服务器能力缺失由协商测试和兼容代码覆盖。PR #179 之前的服务器拒绝 `transport` 字段，调用方需省略该字段，沿用旧的中继接口。

### 相关测试构成

`peer_port` 名称过滤器包含 32 项：原有回归 11、配置模式一致性 1、CIDR/接口策略 5、自动模式场景 11、真实 CLI 矩阵 1、双命名空间 1、offer 编解码 2。其中三个依赖运行环境的测试默认忽略，本轮用 `--include-ignored` 显式执行。能力精确回显测试另位于 `client::relay::tests`。

自动模式场景覆盖无预填候选建连、首候选失败后选择后续候选、明确 relay、空发现、自动/固定混用、错误配对、候选篡改与越界、授权前预留、到期释放、TLS 停滞时关闭，以及配置权限边界。原有固定模式回归继续覆盖无客户端证书、授权撤销、租约过期及重放拒绝。

### 验收中修正的问题

最初扩展验收为 30 项通过、2 项失败。一个失败来自读取当前 Docker inspect 中不存在的 `NetworkSettings.SandboxID`；改为实际 Linux 网络命名空间标识。

另一个失败在真实 CLI 的首轮 Prepare，错误为 `expired or duplicate peer offer`。中继注册早于 CLI 的 Backend、Runtime 和工具目录初始化；原测试只等待客户端出现在列表便开始五秒授权窗口。代码检查确认这处拒绝发生在候选枚举及拨号之前，同一未修改生产二进制的后续十项矩阵也全部通过。新测试先完成带原有授权的只读 `tools/list` 往返，确认运行时就绪后再创建 mapping。生产超时没有延长，授权没有重放。历史日志不能精确拆分当时的启动排队、磁盘持久化与系统调度成本；本次结果限定为运行时就绪后的建连。

独立代码复核还发现并修正两处边界：注册后配置模式必须与精确协商能力一致；没有本机接口枚举实现的平台需要签名空候选，再经授权后回退。

后续命名空间验收已完成直连和强制中继传输，但本机的 Kiri Docker 兼容运行时拒绝断开容器的第一个网络。测试因此给应用监听端保留一个独立初始私网，把可达目标的共享私网作为可断开的第二个网络；没有放宽客户端容器权限，也没有修改宿主防火墙。

## 实际覆盖与缺口

| 条件 | 本次结论 |
| --- | --- |
| Linux 回环、真实 CLI、两个可路由私网命名空间 | 由本轮执行结果和日志确定；不是模拟网络栈。 |
| 多候选 | 构造首个不可达候选及两个真实回环候选；收集包含 IPv6，未单独断言应用流地址族。 |
| 四候选满容量 | 代码及反序列化限制为四个，五个被拒绝；未强制四接口满容量，也未独立测量探测并发峰值。 |
| macOS | 使用同一 Unix 枚举实现；没有本轮编译或运行验收。 |
| Windows | 自动目标端返回签名空候选；原生接口发现未实现，未做 Windows 运行验收。固定模式保留。 |
| 旧服务器 | 能力协商代码及单元测试覆盖；未启动旧版中继进行真实端到端验证。 |
| 实体 LAN、真实 VPN、多网卡公网 IPv4/IPv6 | 可适用的代码路径存在，但本轮没有对应实体环境验收。 |
| 自动模式真实控制链路丢失 | 共享租约清理路径保留；自动测试覆盖主动 shutdown，固定回归覆盖租约及撤权。未单列自动模式真实 WebSocket 丢线计时测试。 |
| 吞吐、长连接、丢包、高时延 | 未做性能或可靠性基准；字节往返不能替代这些测试。 |
| 换网、映射寿命、活动流迁移 | 新 mapping 会重新发现；现存 mapping 不重新选路，没有 ICE restart 或连接迁移。 |
| 双 NAT、CGNAT、端点相关映射/过滤 | 主动穿透没有实现；公网双 NAT 验收没有进行。 |

## 为什么本次没有添加一个 STUN 查询就宣布完成打洞

STUN Binding 查询反射的是请求所用传输在服务器观察到的源地址与端口，见 [RFC 8489 §6.3.1.1](https://www.rfc-editor.org/rfc/rfc8489.html#section-6.3.1.1)。其 [§6.2.1](https://www.rfc-editor.org/rfc/rfc8489.html#section-6.2.1) 与 [§6.2.2](https://www.rfc-editor.org/rfc/rfc8489.html#section-6.2.2) 分别定义 UDP/DTLS 和 TCP/TLS 传输；因此，不能直接把 UDP 查询结果当成现有 TCP/TLS 数据面的可达地址。

[RFC 6544](https://www.rfc-editor.org/rfc/rfc6544.html) 的 ICE-TCP 涉及主动、被动及同时打开候选，连接检查和选中连接；[附录 B](https://www.rfc-editor.org/rfc/rfc6544.html#appendix-B) 进一步涉及同一本地端口的绑定与 socket 复用。当前实现会关闭探测连接，再为每条应用流新建 TLS。即使偶然打通一次 TCP，后续新连接也不能据此保证穿透相同 NAT。

因此，后续实现应先决定数据面是否保留和复用经过检查的连接，再实现候选收集、端口复用/同时打开、检查与选择、映射保持和失败回退。若改用 UDP/QUIC，还需处理新的承载方式、认证绑定和多流语义。将这些工作强塞进当前小补丁，会扩大协议与兼容性变更范围。

### 公网双 NAT 后续验收清单

需要两个分别处于独立公网接入及 NAT 后的实际客户端，并保留当前中继作为控制面。测试应记录候选来源、实际选中路径和中继应用字节计数，至少分别覆盖：

1. 两端普通家庭 NAT、至少一端 CGNAT，以及端点相关映射/过滤环境；失败时应明确回退。
2. 无预先端口映射的双向建连、NAT hairpin、IPv6 入站防火墙和 IPv4/IPv6 组合。
3. 映射到期、闲置后的新流、地址重绑定、换网及中继控制链路断开。
4. 错误证书/密钥、候选篡改、越界拨号和授权撤销；不能以绕过认证提高成功率。
5. 双向大流量、半关闭、长连接和关闭清理；分别报告成功率与中继回退比例。

在主动穿透实现前，上述环境只能验证既有可达路径或中继回退，不能给出打洞成功率。

## 复现与来源

验证 worktree：`/home/lightjunction/Documents/GITHUB/ssmcp/.worktrees/auto-peer-candidates-20261122`。

环境：Linux 7.3.5-arch1-1 x86_64、glibc 2.44、Rust/Cargo 1.100.0；Docker 兼容运行时报告 27.5.1，本地容器实现为 Kiri。

本轮构建二进制与日志目录：`/var/tmp/ssmcp-auto-peer-20261122`。该目录属于本地验收产物，不进入 Git。

| 验证二进制 | 源码 | SHA-256 |
| --- | --- | --- |
| `ssmcp-pre179` | `add39e18ff92680ff3ac0c3e911407956e20b228` | `065ca6e26fde23f8757e9c6863e05922f3d18c8736498030a3625d67a0a8fa6e` |
| `ssmcp-pr179` | `cdcc9ecf240a98436f7d9d750acadefd9f9977a7` | `cb392998119ac1273dede6dd9ffdcf4b3243f62b3a60807ce3e2d8068e61b2fd` |
| `ssmcp-auto` | `02e58fb6ce99e6c0987bb31e82de81ee49800504` 的生产代码；后续测试/文档编辑不改变该二进制 | `2807cd0ce560063d203c1d6b6c867d093f06b93f32901063268bdc50d23a6dcc` |

先分别在上述源码版本构建并保存固定客户端。旧客户端路径是可选测试输入，但要复现十项完整矩阵，必须同时提供两份。然后在本补丁源码下运行：

```sh
export CARGO_TARGET_DIR=/var/tmp/project-z-cargo-target
export SSMCP_TEST_PEER_CLIENT=/var/tmp/ssmcp-auto-peer-20261122/bin/ssmcp-auto
export SSMCP_TEST_PR179_CLIENT=/var/tmp/ssmcp-auto-peer-20261122/bin/ssmcp-pr179
export SSMCP_TEST_OLD_PEER_CLIENT=/var/tmp/ssmcp-auto-peer-20261122/bin/ssmcp-pre179
export SSMCP_TEST_PEER_NETNS_WRAPPER="$PWD/scripts/peer_netns.py"

flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo fmt --all -- --check
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib peer_port -- --include-ignored --nocapture --test-threads=1
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib client::relay::tests
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib bridge::tests
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib workspace::tests
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib runtime::hooks::tests
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib broker::allow
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo test --locked --offline --lib wire::tests
flock "$CARGO_TARGET_DIR/.project-z-cargo-owner.lock" cargo clippy --locked --offline --all-targets -- -D warnings
```

`--offline` 要求依赖已缓存。命名空间测试需要 Linux、Docker、Python 3、`readlink` 及本地已有的 `alpine:latest` 镜像；脚本不会主动拉取镜像。共享构建目录的 owner lock 是本开发机的工作约定。
