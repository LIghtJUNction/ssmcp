# 直接依赖迁移记录（2026-11-20）

这次迁移将七个超出原版本约束的直接依赖升级到审查时的最新稳定版，并补齐 HTTP/TLS 与摘要格式化的必要调用点。四个直接 Z 库保持原版本和 gitlink。源码准备阶段已完成格式化和差分检查；**锁文件解析、编译、运行回归及六平台验证尚待最终合流后的统一门禁**，本记录不构成发布或部署回执。

## 版本与功能开关

版本来自固定的 [crates.io 官方索引快照][index]，排除了撤回版与预发布版。七份发布包均按该索引的 checksum 核对后读取源码。

| 直接依赖 | 原锁定版本 | 迁移目标 | 显式 features |
| --- | --- | --- | --- |
| base64 | 0.22.1 | 0.23.1 | `std` |
| getrandom | 0.3.4 | 0.4.3 | 无 |
| tokio-tungstenite | 0.29.0 | 0.30.0 | `connect, rustls-tls-webpki-roots` |
| reqwest | 0.12.28 | 0.13.5 | `json, form, query, rustls-no-provider` |
| sha2 | 0.10.9 | 0.11.1 | `alloc` |
| toml | 0.8.23 | 1.2.1+spec-1.1.0 | `std, serde, parse, display` |
| toml_edit | 0.22.27 | 0.25.19+spec-1.1.0 | `parse, display` |

以上七项都使用 `default-features = false`。TOML 两项的版本约束省略 `+spec-1.1.0` 构建元数据；最终解析版本由 `Cargo.lock` 记录。项目声明的 Rust 版本仍为 1.100。

直接 Z 依赖固定为：

| 库 | 版本 | gitlink |
| --- | --- | --- |
| zharness | 0.6.0 | `db4206d3b74e37d57ff052948962f55514081d4b` |
| zplugin | 0.3.1 | `00d566eedcff7b2932e90b9dfeb44910c783d305` |
| ztuio | 0.1.0 | `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e` |
| zuse-computer | 0.2.2 | `94c837551a7a2fd8ec9f577a3e2f57309bd5ce5a` |

## HTTP 的默认信任方式保持明确

reqwest 0.13 调整了 TLS 功能组合，并将表单与查询参数接口拆为可选功能。直接沿用 0.12 的 feature 名称不可行；只启用新的默认 TLS 也会改变加密提供方和证书信任来源。[上游功能定义][reqwest-cargo]与[实际构造分支][reqwest-client]是本次选择的依据。

新的私有 `src/http_client.rs::builder` 为每个普通 HTTP 客户端传入完整的 Rustls 配置：

- 使用已有的 `ring` 提供方，不安装或替换进程全局提供方。
- 使用随 `webpki-roots` 发布的根证书集合。
- 保留 TLS 1.2 和 TLS 1.3、服务器证书及域名验证、SNI 和无客户端证书认证。
- ALPN 显式为 `http/1.1`；本项目原 reqwest 功能组合没有启用 HTTP/2。

这些设置对应[旧 0.12.28 普通 Rustls 分支][reqwest-old-client]及本项目现有 Rustls feature 组合。0.13 的 `BuiltRustls` 分支直接使用传入配置，不再替调用者补充 ALPN，因此这里必须明确配置。[新分支源码][reqwest-client]

`tls_backend_preconfigured` 会绕过 reqwest 普通 TLS 分支对根证书、CRL、危险验证开关、TLS 版本范围、SNI 和客户端身份等选项的处理。后续新增这些选项时，应修改传入的 `ClientConfig`，不能假设再调用普通 TLS builder 方法会覆盖它。当前所有既有调用链没有这些后续修改；账户路径的专用证书配置仍在默认 helper 之后覆盖，顺序没有倒置。

代理、重定向、`https_only`、连接及总超时、DNS 解析覆盖、User-Agent 和连接池设置仍保留在各自原调用链中。helper 没有统一添加 `no_proxy`，也没有改变原先允许系统代理的调用。账户专用配置和 `trust` 回归中的 `resolve(host, address)` 均保留。

### 构造点清单

迁移前共 **32 个文件、47 个 reqwest 客户端构造点**。其中 `src` 内为 27 个文件、42 处，另有五个独立 integration test crates。以下数量仅计原调用点，不计新增 helper 自身。

| 文件（相对仓库） | 数量 | 用途与处理 |
| --- | ---: | --- |
| `src/backend/opencode.rs` | 3 | 两个生产客户端、一个传输夹具；使用私有 helper |
| `src/broker/allow.rs` | 4 | HTTP 权限回归；使用私有 helper |
| `src/browser.rs` | 1 | 浏览器 HTTP 探测；保留原局域调用约束 |
| `src/client/account.rs` | 1 | 默认 helper 后保留 exact-origin 专用 TLS 覆盖 |
| `src/client/local.rs` | 1 | 本地 gateway 回归 |
| `src/client/terminal/guide.rs` | 1 | 指南可用性探测 |
| `src/client/terminal/update_notice/tests.rs` | 1 | 会话结束取消更新请求的回归 |
| `src/client/trust/tests.rs` | 1 | 保留原专用 constructor，仅更新预配置 API 名称 |
| `src/client/update.rs` | 2 | 手动更新检查和 HTTP 夹具 |
| `src/client/update/startup.rs` | 1 | 启动时后台更新检查 |
| `src/client/update/startup/tests.rs` | 2 | 更新缓存、超时等 HTTP 回归 |
| `src/client_downloads.rs` | 1 | 客户端发布资源路由回归 |
| `src/commerce/pancake.rs` | 1 | 生产支付网关请求 |
| `src/commerce/pancake/tests.rs` | 1 | 支付网关夹具 |
| `src/device.rs` | 4 | 账户与连接 HTTP 回归 |
| `src/http.rs` | 1 | OAuth HTTP 回归 |
| `src/identity.rs` | 1 | 身份提供方请求；保留原 HTTPS 限制 |
| `src/oauth/grant_tests.rs` | 1 | 独立 core 进程回归 |
| `src/oauth_github_tests.rs` | 1 | GitHub 身份提供方夹具 |
| `src/observability.rs` | 1 | 健康检查回归 |
| `src/portal/assets.rs` | 2 | 静态资源回归 |
| `src/portal/commerce_tests.rs` | 1 | 支付门户回归 |
| `src/portal/frontend/tests.rs` | 3 | 前端 worker 与代理回归 |
| `src/portal/guide.rs` | 1 | 指南代理回归 |
| `src/portal/setup_tests.rs` | 1 | 账户设置回归 |
| `src/portal/tests.rs` | 3 | 门户会话回归 |
| `src/webhooks.rs` | 1 | 生产 webhook 请求 |
| `tests/workspace_bridge.rs` | 1 | 独立 integration crate，使用测试专用 helper |
| `tests/resources.rs` | 1 | 同上 |
| `tests/two_tools.rs` | 1 | 同上 |
| `tests/collaboration.rs` | 1 | 同上 |
| `tests/oauth_security.rs` | 1 | 同上 |

五个独立测试 crate 通过 `tests/support/http_client.rs` 配置相同的明确 TLS 默认值，生产 helper 保持私有。这样不依赖某个其他测试碰巧先安装全局加密提供方，也不为测试扩大库的公开 API。

## 摘要文本与其他 API 迁移

sha2 0.11 将输出容器换为 `hybrid-array::Array`，现有 `Digest` 调用和到 `[u8; 32]` 的转换仍可用，但该容器没有旧输出类型的 `LowerHex` 实现。[sha2 变更记录][sha2-changelog]、[输出类型定义][crypto-output]、[Array 实现][array-source]

因此，49 处摘要格式化调用显式迁移为逐字节、两位、小写十六进制。46 处库内调用使用私有 `src/hex.rs::encode`；三个独立测试使用自己的逐字节预期值。hash 输入、摘要算法、ID 前后缀和路径规则保持原样。新增已知 SHA-256 向量与零填充回归，目标是保留现有缓存名、会话 ID、资源指纹和校验值，尚待实际运行确认。

其余必要兼容边界如下：

- **getrandom：** 当前项目调用 `fill`，新发布版保留该接口；未改用其他随机来源。[发布源码][getrandom-source]
- **base64：** 保留项目使用的 engine 和编码/解码接口。直接依赖关闭默认功能并不等于最终依赖图禁用 SIMD；reqwest 0.13.5 的 base64 依赖启用默认功能，而 base64 0.23.1 默认包含 `simd-unsafe`。最终统一 feature 图需由锁文件负责人记录。[reqwest manifest][reqwest-cargo]、[base64 manifest][base64-source]
- **WebSocket：** tungstenite 0.30 更严格地验证握手 key。现有手写成功夹具生成 16 字节随机 key；没有为了升级放宽握手或鉴权断言。[上游变更记录][tungstenite-changelog]
- **TOML：** 显式开启现有反序列化、序列化和编辑所需功能。项目使用的 `from_str`、`to_string_pretty`、`Value::try_from`、错误 `span`、`DocumentMut` 仍存在；未采用新低层序列化 API。新版本支持 TOML 1.1，接受的配置语法会相应扩展；本补丁没有增加启动时自动重写配置的行为。[toml 迁移记录][toml-changelog]、[toml_edit 变更记录][tomledit-changelog]

## 锁文件与验收交接

`Cargo.lock` 不由本补丁手工改写。应先把最终源码正常合流到干净工作树，再由唯一构建负责人更新锁并记录实际结果。首选保守的工作区更新；Cargo 官方将其定义为保留已有非工作区锁项、按新 manifest 补入必要包的方式。[Cargo update 文档][cargo-update]

```sh
cargo update --workspace
cargo tree --locked -e features -i reqwest@0.13.5
cargo tree --locked -e features -i base64@0.23.1
cargo tree --locked -e features -i rustls
```

需要检查的解析变化包括 reqwest 0.13 的平台验证器依赖、sha2/digest/crypto-common/hybrid-array 新版本链、WebSocket 和 TOML 新版本链。预配置 TLS 不使用平台验证器作为本项目普通 HTTP 的信任来源，但该 feature 仍会带入其平台依赖，六平台编译必须覆盖。不能为了把锁文件压成单版本而强制覆盖仍有合法旧约束的传递依赖；尤其保留 zharness 使用的 zcall 0.4 与 zmcpio 使用的 zcall 0.3 链。

锁定后，统一门禁至少应包含新 `http_client::tests`、`hex::tests`，以及已有的账户 trust、OAuth、WebSocket、TOML 配置保存和资源校验回归。三项 HTTP 回归分别覆盖请求编码、默认拒绝私有 CA、显式 CA 覆盖下的 TLS 1.2/1.3 与 HTTP/1.1 成功路径及错误域名拒绝；拒绝路径检查未发送 HTTP 凭据。这些测试已写入源码，不能将静态审查或格式化成功记为测试通过。

| 主张 | 当前依据 | 状态 |
| --- | --- | --- |
| 七个版本是审查时最新稳定发布 | 固定官方索引、发布包 checksum | 已核对 |
| HTTP 普通信任策略和账户专用覆盖顺序保持明确 | 新旧上游源码、完整调用点差分 | 静态已核对；运行待门禁 |
| 摘要字符串保持旧格式 | 49 处调用点差分、已知值回归 | 静态已核对；运行待门禁 |
| 其他现用 API 可迁移 | 对应发布源码与当前调用集合 | 静态已核对；编译待门禁 |
| 可发布及六平台兼容 | 最终源码、锁文件与同源门禁回执 | 待验证 |

自查：本记录说明的是版本及兼容迁移，没有宣称性能提升；每项设计都有源码依据；格式化与运行结果分开记载；平台与持久化回归仍列为必需证据；传入完整 TLS 配置后普通 builder TLS 选项不再生效这一限制已明确说明。

[index]: https://github.com/rust-lang/crates.io-index/tree/b0d2a0743ad52dcb869c49ef349d0d6da35a9441
[reqwest-cargo]: https://github.com/seanmonstar/reqwest/blob/de55373434f07f42926599dbb5a88550d8e55112/Cargo.toml
[reqwest-client]: https://github.com/seanmonstar/reqwest/blob/de55373434f07f42926599dbb5a88550d8e55112/src/async_impl/client.rs
[reqwest-old-client]: https://github.com/seanmonstar/reqwest/blob/v0.12.28/src/async_impl/client.rs
[sha2-changelog]: https://github.com/RustCrypto/hashes/blob/138dc5a05b93f8e49cda177fc6cfd7cc43673e5d/sha2/CHANGELOG.md
[crypto-output]: https://github.com/RustCrypto/traits/blob/93dee26c6bde3741a197f1c5f6b7baac277705f3/crypto-common/src/lib.rs
[array-source]: https://github.com/RustCrypto/hybrid-array/blob/09310c55b75e2cd408951c7e2e86dba16c63fc68/src/lib.rs
[getrandom-source]: https://docs.rs/getrandom/0.4.3/src/getrandom/lib.rs.html
[base64-source]: https://docs.rs/crate/base64/0.23.1/source/Cargo.toml
[tungstenite-changelog]: https://github.com/snapview/tungstenite-rs/blob/7f4aeaf0944992c5664c8fb8e0d54577c7e18020/CHANGELOG.md
[toml-changelog]: https://github.com/toml-rs/toml/blob/563cb1a5d133e39d1008d4d4148ef35d3bba379f/crates/toml/CHANGELOG.md
[tomledit-changelog]: https://github.com/toml-rs/toml/blob/563cb1a5d133e39d1008d4d4148ef35d3bba379f/crates/toml_edit/CHANGELOG.md
[cargo-update]: https://doc.rust-lang.org/cargo/commands/cargo-update.html
