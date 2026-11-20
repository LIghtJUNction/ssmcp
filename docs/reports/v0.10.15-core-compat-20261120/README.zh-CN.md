# v0.10.15 原生插件与账号兼容回执（e30a149）

本记录证明冻结源码 `e30a14997d92adc5d19ec21de934dcc5a1eba562` 的三项补充验收通过，并从已有默认库测试日志独立核对了 13 项 WebSocket/响应交付用例。它不代表完整 workspace 或交互界面验收通过：同一次默认库测试仍为 **1584 通过、2 失败、22 忽略**，尚未进入 integration tests。所有结果均归属本记录的冻结产物，不转借给后续源码或二进制。

## 结果与证据

| 验收范围 | 实际结果 | 外层用时 | 证据 |
| --- | --- | --- | --- |
| 显式 ignored 的原生插件运行时契约 | 1 通过，exit 0 | 6.226 秒 | `core-compat/native-plugin.stdout.log` |
| 官方 v0.10.13 与实测 v0.10.12 旧 CLI | 41 检查通过，exit 0 | 10.888 秒 | `core-compat/legacy-client-compat.json` |
| 当前 CLI 账号与中继生命周期 | 47 检查通过，exit 0 | 14.645 秒 | `core-compat/modern-account.json` |
| 默认测试中已有的原生消息与问答交付 | 13 项均为 `ok`，未重复运行 | 属于原默认集合 | `workspace-default-suite/workspace.log` |
| 同一次默认库测试整体 | 1584 / 2 / 22，失败 | 153.81 秒 | 同上；不能据局部通过改写整体结论 |

三项补充验收在 2026-11-20 03:16:19–03:16:51 UTC 顺序运行。每项独立设置 180 秒上限，均正常完成，没有超时或重试。机器可读回执、完整检查名称、原始证据 SHA256 与脚本哈希见 [evidence.json](evidence.json)。

原始证据位于构建工作树的 `target/zuse-validation/e30a14997d92adc5d19ec21de934dcc5a1eba562/`。公开回执只保留相对路径、源码和内容哈希；私人绝对路径、凭据与现场客户端标识不写入仓库。

## 实际验证范围

**旧 CLI 的 41 项检查使用真实生产 Rust Broker/router 和真实旧客户端。** 官方 v0.10.13 与现场观察到的 v0.10.12 各运行默认自身名单、显式自身名单两种场景；四组都完成 Ready、真实原生 shell 调用、另一已验证账号不可见且不可调用、同一连接稳定与受控清理。上游 GitHub 身份服务为合成 fixture，未使用真实用户凭据。v0.10.12 仅声明为已观察的旧 ELF，不声明其官方发布来源。对应实现为冻结版本的 [旧 CLI runner](https://github.com/LIghtJUNction/ssmcp/blob/e30a14997d92adc5d19ec21de934dcc5a1eba562/scripts/e2e_legacy_client_compat.py)。

**现代账号的 47 项检查使用真实当前 CLI 和 Python 模拟的回环网关。** 它覆盖同源 allow、账号与身份隔离、凭据不跨源转发、真实原生 shell 往返、普通网络关闭后的重连、活跃及 Ready 后立即撤权、401/403、注销与有限重新授权。这个脚本不等同于生产 Rust 服务端全链路验收。脚本自身的私有 auth/session/config 环境之外，本轮外层 runner 另外创建了私有 HOME、XDG config/state/data/cache 与 TMPDIR，并仅传递白名单环境；因此没有继承现场 SSMCP 配置。对应实现为 [账号 runner](https://github.com/LIghtJUNction/ssmcp/blob/e30a14997d92adc5d19ec21de934dcc5a1eba562/scripts/e2e_account_relay.py)。

**默认集合中的 9 项消息用例覆盖了实际连接和授权生命周期。** 从原日志逐项确认了设备协商、精确收件人、可信来源、幂等、agent 绑定、OAuth 资源限制、ACL 撤销、目标断线、成员离开、孤儿待发送内容回收，以及同 client ID 更换 Session Arc 后不可投递。发送方查询触发过期的用例不依赖收件方再次轮询。完整测试名称保留在 JSON 回执中。

**另外 4 项响应交付用例验证了已绑定请求的最终授权边界。** 它们覆盖显式只读问答不消费、下一次合格响应投递、原生错误响应仍携带可信答复、最终授权/绑定/原连接复核，以及预算和响应结构检查先于认领。测试采用认证的 HTTP/WebSocket fixture，但原生答案留存由 fixture 模拟；不能把这四项当作真实终端 Yes/No、评论、Shift+Up 或键盘处理无死锁的证明。交互 PTY 验收由独立记录负责。

## 冻结输入与复现

| 输入 | SHA256 |
| --- | --- |
| e30 库测试 ELF | `98589e3c41709229bdc6ee6f10102d803fc9a9bcadf5d050a04be8d051319792` |
| e30 CLI | `ae5d4ec80111f47f0c07dc1d239ef2bbe3679aee7dd6e8f8828bfb28f6c267b8` |
| e30 原生插件共享库 | `26760d18256c7941e88a1323b1238e3dc88cf357581beaa6b9563fb733bf1858` |
| 官方 v0.10.13 CLI | `5ad6682a2e63e865b9a0ada78ce0d1b9b7799cf95e6bdad15c4661ecc106a7aa` |
| 现场观察的 v0.10.12 CLI | `0e2de0037d5ce6ee1c724f1530b171655b8c72e311077619a7903af8ebcde8e3` |

以上五个输入在执行前后哈希一致。四个 Python runner/helper 文件在执行前均与 `git show e30a149:<path>` 逐字节一致，执行后也未变化。旧 CLI 由 runner 复制至私有临时目录后作为普通客户端运行；没有执行 007 服务入口，没有安装替换、重启、重新加载现场客户端或启动模型。

以下为实际入口的脱敏形式；`FROZEN_LIBTEST` 必须仍位于 `frozen/debug/deps/`，共享库保留在相邻 `frozen/debug/examples/`。应先核对上表哈希，并使用前述私有环境。

```sh
"$FROZEN_LIBTEST" --exact runtime::plugins::tests::native_plugin_runtime_contract --ignored --nocapture

python3 scripts/e2e_legacy_client_compat.py \
  --official-v13 "$OFFICIAL_V13" \
  --runtime-v12 "$OBSERVED_V12" \
  --runtime-v12-sha256 0e2de0037d5ce6ee1c724f1530b171655b8c72e311077619a7903af8ebcde8e3 \
  --fixture-binary "$FROZEN_LIBTEST" \
  --server-revision e30a14997d92adc5d19ec21de934dcc5a1eba562 \
  --report "$PRIVATE_EVIDENCE/legacy-client-compat.json"

python3 scripts/e2e_account_relay.py \
  --binary "$FROZEN_CLI" \
  --report "$PRIVATE_EVIDENCE/modern-account.json"
```

## 已知边界与后续修正

目录失败是旧单测仍断言 3 项，而新原生问答目录已经提供 7 项。独立 test-only 提交 [57b2d8b](https://github.com/LIghtJUNction/ssmcp/commit/57b2d8b1b9cd4634117f3f8508912570b15b7758) 改为核对七个确切名称，并保留和加强禁止伪造 human/agent 来源、无已验证调用上下文不能答复或批准的断言。该提交在本回执时仅通过 standalone rustfmt 与 diff 检查，尚未编译或执行，也没有改变本次 e30 冻结产物。

本记录不宣称完整 workspace、跨平台发布、真实终端交互、部署切换或存活作业连续性通过。生产切服后原生作业是否保留不是旧 CLI 自身名单兼容测试的结论；正式发布及部署状态也须查独立回执。

## 结论自审

| 审查问题 | 对应证据或限制 |
| --- | --- |
| 新增了什么可复核结论？ | 三项补充验收的真实退出码、检查数及五个输入前后哈希，另核对已有 13 项默认用例。 |
| 能否确定结果属于哪个版本？ | 固定源码 e30a149、源码树、ELF/共享库和四个脚本哈希；未归属后续 test-only 提交。 |
| 是否把模拟当成真实系统？ | 明确区分真实 Rust router + 旧 CLI、Python 模拟网关 + 当前 CLI，以及模拟原生答案的响应交付 fixture。 |
| 是否遗漏已知失败？ | 保留默认库 2 失败与 integration 尚未执行；不以局部通过覆盖整体状态。 |
| 是否影响现场状态或扩大结论？ | 私有输入副本与环境；没有 Cargo、模型、安装/服务操作，不推断部署连续性或发布完成。 |
