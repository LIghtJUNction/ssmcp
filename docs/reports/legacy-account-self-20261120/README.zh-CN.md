# 旧账号客户端的自身接入兼容

## 范围与依据

本修正基于应用主线 19458a18e320e4045281fc9228039e6c1edbd300，保持原有依赖与应用版本。官方 v0.10.12、v0.10.13 的账号启动路径会把已验证凭据的自身 GitHub principal 写入 Hello.allow，即使用户没有指定共享对象；两版 client.rs 与 relay.rs 的相关文件内容相同。新服务的通用名单语法只接受服务器账号，因此会在元数据校验阶段拒绝这个旧默认值。

兼容处理只位于已认证账号连接的 Hello 边界。名单必须恰好一项，并且逐字节等于该 registration 的已验证 GitHub identity 对应的规范 principal，才转换为已有的空名单自身语义。实际 ACL 仍来自凭据绑定的 provider 与稳定 subject，不通过名单文字查找 GitHub 用户，也不把 GitHub login 当作服务器用户名。

## 保留的边界

外人 principal、混合名单、重复项、非规范大小写、空白变体和匿名连接继续经过原来的严格校验。Hello 的名称、路径、主机名、标题校验以及账号、凭据、封禁和授权撤销检查均保留。CLI 和 wire 的通用语法没有扩展，也没有恢复邀请、注册设备或 SSH 接入路径。

旧版权限面板只理解 GitHub principal，无法验证当前服务器的 username@server 快照。因此，只有被识别为旧自身默认值的连接不在 Ready 中接受 session_access_v1。后续 Access 请求返回明确的客户端升级提示，不能据此改动 ACL。control_attribution_v1 仍独立按照连接的能力协商决定；原本没有协商该能力的客户端不会收到 attribution 字段。现代空名单、现代显式服务器名单及匿名连接的协商规则保持原样。

## 验证边界

新增四项真实 WebSocket 回归使用彼此不同的 GitHub login、服务器 username 和 subject，覆盖自身接入、原生调用往返、能力协商、拒绝旧权限编辑、严格名单负例、其他元数据校验，以及 HTTP 升级后、Hello 前发生注销或封禁的情况。既有共享连接在登记人注销后仍允许其他已授权控制者工作的语义不改变。

此提交是源码检查点：已做独立 rustfmt 和 git diff 空白检查，尚未执行 Cargo 构建或新增 Rust 测试。统一构建、完整测试、官方旧 CLI 的真实接入与跨账号隔离验收由后续冻结候选完成；不能把本记录视为已部署或已验证线上重连。

## 可定位的源码

- 兼容入口与 Ready/Access 协商：src/broker.rs，attach_stream_with_traffic。
- 真实 WebSocket 回归：src/broker/allow.rs，legacy_account_self_ 前缀的四项测试。
- 保持严格的通用入口：src/wire.rs 的 ClientMetadata::validate，src/broker/allow.rs 的 resolve_initial_allow。
- 已验证账号来源与存活检查：src/device.rs 的 authenticate 和 Registration::is_live。

## 历史来源

- [官方 v0.10.12 的 apply_account](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.12/src/client.rs) 与 [v0.10.13 对应文件](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.13/src/client.rs)：本次读取的完整文件 blob 均为 72c225579e5be17a609b500a2b9311ccb3848bdd。
- [基线认证与 registration 来源](https://github.com/LIghtJUNction/ssmcp/blob/19458a18e320e4045281fc9228039e6c1edbd300/src/device.rs)：凭据记录先绑定稳定 subject，再读取账号身份；Hello 不能提供该 registration。
- [基线严格名单解析与账号存活检查](https://github.com/LIghtJUNction/ssmcp/blob/19458a18e320e4045281fc9228039e6c1edbd300/src/broker/allow.rs)：空名单直接采用 registration.identity；显式名单使用服务器账号解析，随后再次检查 registration.is_live。
