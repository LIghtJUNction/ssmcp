# 当前账号中继流程与启动撤权回归验收

## 结论

**完整账号端到端回归通过：47 项检查，0 失败，10.139 秒。** 被测产品提交为 [`78442aa0849cc1a168db039ff7427ea076594409`](https://github.com/LIghtJUNction/ssmcp/commit/78442aa0849cc1a168db039ff7427ea076594409)，版本 `ssmcp 0.10.14`。运行使用从实际 compiler-artifact 独立冻结的只读 CLI；前后哈希相同。

本轮修复由正式发布入口审查触发。旧账号端到端脚本仍执行已退出产品的 `invite / join` 流程；改为当前 `username@relay` / `--allow` 连接后，又在真实 CLI 中复现了启动瞬间撤权的重试错误。测试迁移和产品修复分别提交，原有撤权断言保留。

## 变更和依据

| 范围 | 变更 | 可核对来源 |
| --- | --- | --- |
| 账号端到端测试 | 登录账号 A；无账号凭据的受控端 B 用同源 `--allow` 连接；保留真实 WebSocket 和 native shell 往返 | [7218478](https://github.com/LIghtJUNction/ssmcp/commit/721847874f643ea8c1aa4a4b5cb5ecb5058a138d) |
| 协议测试服务 | 只响应启动时的 `Access(List)` 授权名单读取，拒绝测试范围外的名单编辑 | [0c06583](https://github.com/LIghtJUNction/ssmcp/commit/0c06583b366bba38b81051911094a1107d8f9f14) |
| 注销测试服务 | 前台账号连接可以发起一次新的 OAuth 授权；本场景明确拒绝它，验证没有新 WebSocket，也没有重新保存凭据 | [ce0abe6](https://github.com/LIghtJUNction/ssmcp/commit/ce0abe65d9e1281553ef09680d2ecb0ca1926ab0) |
| 产品启动流程 | 三处最终拒绝消息的保留条件改为实际 `Transport::Account`，覆盖有令牌和无令牌两种账号中继连接 | [e7f5711](https://github.com/LIghtJUNction/ssmcp/commit/e7f5711a3474275b9d5308a809c726636366d78a) |

### 为什么启动瞬间撤权会被误判

账号中继子进程收到 WebSocket Close 4001 后，会向父进程写入 `AUTH_EXPIRED` 拒绝帧，再正常退出。父进程可能先观察到子进程退出，稍后才读到最后一帧。

已有代码用一个有界等待保留这个最终拒绝结果，但三处条件都是 `options.relay.is_some()`。该字段表示账号凭据；当前显式 `--allow` 的受控端不需要保存账号令牌，仍使用相同的 WebSocket 传输。无令牌连接因此跳过等待，把撤权错误变成普通进程退出，watchdog 随后重连。

修复将条件改为实际传输类型，复用原有等待上限和永久失败分类。本地传输不匹配 `Transport::Account`，继续沿用原行为。独立源码复核确认这三个调用点均已覆盖。

对照来源：[启动与等待逻辑](https://github.com/LIghtJUNction/ssmcp/blob/e7f5711a3474275b9d5308a809c726636366d78a/src/client.rs)、[4001 转换为拒绝帧](https://github.com/LIghtJUNction/ssmcp/blob/e7f5711a3474275b9d5308a809c726636366d78a/src/client/relay.rs)。这里验证的是客户端明确支持的 4001 撤权语义，不能据此推断所有真实中继的撤权都采用该关闭码。

## 真实 CLI 验证范围

| 检查组 | 主要断言 |
| --- | --- |
| 退出的旧入口 | SSH、invite、join 在访问网络前明确拒绝；当前流程不调用邀请或 enrollment 接口 |
| 账号身份与凭据 | 设备授权得到已验证身份和服务器用户名；文件权限为 0600；输出不泄露令牌或轮询密钥 |
| 同源与身份边界 | 不借用另一中继的已存凭据；API 重定向不转发凭据；不同稳定身份或不匹配用户名不能接管已有登录 |
| 无令牌受控端 | B 显式允许同源账号，握手不带账号令牌、不保存 A 的凭据；A 能列出它 |
| 实际工具调用 | 经 WebSocket 访问真实 terminal/native exec；子命令看不到中继令牌 |
| 普通断线与撤权 | 普通断线可以重连；活跃会话和 Ready 后立即撤权均停止重试；HTTP 401/403 也停止重试 |
| 退出与注销 | Ctrl-C 清理所拥有的进程；注销撤销有凭据的活跃连接；拒绝重新授权后不恢复连接或凭据 |

测试运行真实产品 CLI、WebSocket 传输子进程和本地 shell。OAuth 提供方及账号中继由仅监听测试进程自有 loopback 端口的 Python 标准库测试服务代替；未使用真实用户凭据，未启动模型或 SSH。结果证明所列客户端契约，不替代生产 OAuth 服务或真实服务器的权限审计。

## 验证证据

- 执行时间：2026-11-19 23:32:28–23:32:38 UTC（台北 11 月 20 日 07:32）。
- 退出码：`0`；完整脚本检查数：`47`；运行时间：`10.139s`。
- CLI SHA-256：`e8f80ea13c0a2ba156e4a6c0628904859fe9c2228933bca224758822b3c910ec`，运行前后相同。
- 测试脚本 SHA-256：`1e103edafd58d6e6fcb1307df4406891b6ff6abe3279b8b67db5c113e0d9de8b`，与产品提交内 `scripts/e2e_account_relay.py` 逐字一致。
- 产品小修单独执行 rustfmt 与 diff 检查；测试脚本通过 Python 编译检查。
- 启动撤权、普通断线重连、HTTP 401/403、注销后拒绝重新授权均包含在最终完整运行内；没有使用诊断副本代替正式脚本。
- 本次证据提交只增加报告，并将 CI 步骤名称中的 “device invitations” 改为 “allow lists”；执行命令和门禁保持原样。

复现命令：

```sh
python3 scripts/e2e_account_relay.py --binary "$FROZEN_CLI" --report account-relay.json
```

旧候选 `b49da12ac890f4d2b00c42d9dcf05b97e2054bac` 的 CLI SHA-256 为 `c80da3aa112d25d9228dfe7a860ed02ceb3aa04cd545eb904d1ebad448bf85f7`。它在完整测试的启动撤权场景误报 “SSH exited before local backend readiness (exit status: 0)” 并重连。这个失败触发了上述产品修复，没有被删除或放宽断言。

为排查后续是否还有独立问题，曾在私有诊断副本中仅跳过已知失败区段，运行其余 46 项检查；该结果明确标记为 `diagnostic_only_early_revocation_excluded`，没有作为完整发布验收。最终记录只采用保留全部场景的仓库脚本。

完整检查名称、冻结哈希和运行结果见本目录 `validation.json` 与 `account-relay.json`。公开证据不包含真实凭据；临时 loopback 端口和测试目录作统一替换，原始私有日志保留在开发机验收目录。
