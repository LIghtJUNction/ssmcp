# 月度分档额度与恢复时间验收

日期：2026-11-19。基线：`2bd2f1c1ec871a33c83aad4aa9e1d9b15799cd88`。本记录对应独立月度额度分支，未部署或重启实际服务，未给真实账号发放权益。

## 行为

服务器的 `[portal]` 支持 `free_user_monthly_traffic_bytes` 与 `paid_user_monthly_traffic_bytes`。省略时两类账户均保留旧默认 1,000,000,000 字节。付费档位沿用账户的持久 `has_recharged` 标记，只由已核验的生产充值结算/恢复路径写入。测试订单和客户端提供的名称、标签均不赋予付费档位。

月度账本仍按 `legacy_used_bytes + max(uploaded_bytes, downloaded_bytes)` 计量。调整赠送额度保留全部已用量；单账户当月管理员调整优先，并继续接收之后购买的流量包。管理员把额度设为当下默认值时也持久保存该显式设置，避免以后调整默认值覆盖它。只有服务端明确配置的管理员不设上限，0 字节是有限额度。

`TrafficQuota.reset_at` 是服务器所选月份在固定 UTC 偏移下的下一月开始时刻，单位为 Unix 秒。旧服务器缺少该字段时反序列化为未知。`free_bytes` 为已选档位赠送额，`paid_bytes` 继续只表示当月购买的额外包。

配置示例与优先级见[配置说明](../configuration.md#服务端月度赠送额度)。实现见[策略和时间计算](../../src/user_policy.rs)、[账户选择](../../src/portal/store.rs)、[持久账本](../../src/portal/traffic.rs)与[启动配置](../../src/server_main.rs)。

## 实际本地检查

| 检查 | 通过用例 |
| --- | ---: |
| 方向账本、旧日志迁移、恢复与并发约束 | 22 |
| 账户持久策略、可信付费标记、变更/重启不丢用量 | 9 |
| 速率策略、月界计算、旧配额 JSON 兼容、有限零额度 | 8 |
| 本地 HTTP whoami 与真实登录账号范围 | 3 |
| 控制账号流量归属与限额 | 8 |
| 私有 HTTP 签名付款通知、金额/身份错误拒绝、测试/生产档位区别 | 7 |
| 实际账户额度视图 | 1 |
| 服务端配置解析与启动校验 | 12 |
| 合计（去重） | 70 |

另通过 `cargo clippy --locked --offline --all-targets -- -D warnings`、`cargo build --locked --offline --bins`、`cargo fmt --all` 和 `git diff --check`。其中新增 9 个定向测试。测试付款使用仓库的测试 RSA 密钥、随机私有目录和 loopback HTTP，没有访问真实支付接口。

首轮在新增生产标签测试中被真实签名校验拒绝：测试通知的 `mode` 仍硬编码为 `test`。修改私有夹具，使通知正文采用本夹具环境并重新签名后，第二轮上述全部测试通过。没有为让该用例通过而修改付款校验逻辑。一个视图过滤器匹配 0 项，未计入 70 项。

## 来源绑定

测试前后生产源码、测试、Cargo 文件和相关配置文档指纹一致：

- SHA-256：`ec9313cdc453d35b1213820e41eb971ffc06efcdc54dc6f0fe6c87ff66d54ef2`
- 客户端真实版本输出：`ssmcp 0.10.10`
- 客户端 ELF SHA-256：`6528dcd5eabc3fb58c2a3cff67f49554fe21e35c5d8a29f45aab5231dfa85cf9`
- 服务端 ELF SHA-256：`d00d932cbccead0669784697b61b51a860b541ccaa69226deef9619947c7f7d0`

完整命令、退出码、各组日志与初次失败记录保存在开发机该分支工作树的 `out/monthly-traffic-tiers-20261119/`；最终索引为 `gate/verified.json`。源指纹按索引中记录的文件集合计算，不把本说明自身纳入。主线之后的 UI 与核心整合需要对合并树另做定向验证，不能仅凭这些 ELF 的版本号判断其源码。
