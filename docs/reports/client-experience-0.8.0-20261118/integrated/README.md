# 0.8.0 集成候选验收记录

本目录记录客户端体验改进与 PR #21 内存优化合并后的验证。Rust 门禁、release 构建、隔离安装及六组客户端场景均已通过：默认库测试 **729 通过、0 失败、8 个 ignored**，六组场景共 **299 项功能检查**。另一次 synthetic 协议流程样本通过，未设置性能预算。dashboard 首次失败与修订后成功分别保留；本集成候选的打包、staging 和用户入口安装／发布仍为 **pending**。[Rust 门禁](rust-gates.json) · [Release 构建](release-build.json) · [最终场景门禁](release-ui-gates.json)

## 源码与制品来源

| 对象 | 固定来源 | 说明 |
| --- | --- | --- |
| 运行实现与构建输入 | [`51ec5adcc166d8d10748732d8f62948341a860d9`](https://github.com/LIghtJUNction/ssmcp/commit/51ec5adcc166d8d10748732d8f62948341a860d9) | 本轮 Rust 门禁与 release 二进制对应此提交 |
| 合入的 main | [`a9fbd53c4d52c8e178cbb32defc297defe135a3f`](https://github.com/LIghtJUNction/ssmcp/commit/a9fbd53c4d52c8e178cbb32defc297defe135a3f) | 包含 PR #21 内存优化 |
| Dashboard 重跑夹具 | [`54f90d250a520ca54778d4f0da0bce2700e0064c`](https://github.com/LIghtJUNction/ssmcp/commit/54f90d250a520ca54778d4f0da0bce2700e0064c) | 仅修订 `scripts/e2e_dashboard.py` 的滚动边界断言；没有更换运行实现 |
| 历史首次候选 | `0efb04bf…`／`12e1d57e…` | 属于[上一轮验收](../RELEASE-CANDIDATE.md)，不能作为本集成候选的测试、安装包或部署证明 |

[source-manifest.json](source-manifest.json) 记录构建来源的 **197 个输入文件**，`source_clean` 与 `local_remote_inputs_match` 均为 `true`，聚合 SHA-256 为：

```text
b3606c57040a35d5cd189c88443be6dd27678374564a30bcee58b33b200c01d2
```

该清单对应 `51ec5ad…`；[最终场景门禁](release-ui-gates.json) 另记录 `54f90d2…` 测试来源和 dashboard 脚本哈希。执行器确认 `runtime_inputs_unchanged: true`、`rebuilt: false`，即本次夹具修订没有重建或更换被测 release 二进制；测试结束时 `source_clean_after_tests: true`。

本轮 release 二进制经 `make install DESTDIR=${INSTALLED_PREFIX} PREFIX=/usr` 安装到隔离目录，构建报告确认安装后的两份二进制、runner 和终端解码器与对应构建产物一致。以下是已安装二进制的 SHA-256。[构建与安装记录](release-build.json)

| 产物 | SHA-256 |
| --- | --- |
| `ssmcp` | `eddf5b9b988a37bc397ee0a0ae84022a8a3d519cddc06a5391e1cc10af9797a0` |
| `ssmcpd` | `a8330214a507293131a97092f4940bab8b47f8abd3f4e2367a2db1768d750cd3` |

## 已归档的 Rust 门禁

[rust-gates.json](rust-gates.json) 中各阶段均退出 `0`。表中耗时是外层命令耗时；fixture 编译使用 `--no-run`，不等于执行测试。

| 阶段 | 结果 | 命令耗时（秒） |
| --- | --- | ---: |
| `cargo fmt --all -- --check` | 通过 | 2.36 |
| `cargo clippy --locked --all-targets -j1 -- -D warnings` | 通过 | 82.23 |
| 库测试程序编译 | 通过 | 138.83 |
| 默认 `cargo test` | 各目标均无失败，明细见下表 | 205.4 |
| HTTP `two_tools --include-ignored` | 9 通过、0 ignored | 2.05 |
| `code_mode::tests --include-ignored` | 22 通过、0 ignored、715 filtered | 4.7 |
| `context_helpers --ignored` | 2 通过、0 ignored | 1.71 |

| 默认测试类别 | 通过 | ignored | 说明 |
| --- | ---: | ---: | --- |
| 库测试 | 729 | 8 | 测试程序内部耗时 40.67 秒 |
| `ssmcpd` 二进制测试 | 9 | 0 | 与库测试分别记录 |
| 18 个 integration targets | 100 | 16 | 逐目标结果完整保存在门禁 JSON 中 |
| `ssmcp` 二进制测试与 doctests | 0 | 0 | 没有测试用例 |

HTTP 的 9 项包含默认已通过的 2 项与默认 ignored 的 7 项；code_mode 的 22 项也与默认库测试重叠；context helpers 的 2 项是显式执行默认 ignored 的用例。因此不将这些阶段相加为“新增通过总数”，也不将其余 ignored 项自动视为通过。

## Release 与客户端场景结果

以下结果均有本目录中的执行记录。最终门禁 `status: passed`，六组功能场景合计 **38 + 88 + 84 + 37 + 45 + 7 = 299 项**；preview 与 composer 复用本轮首轮已通过的记录，既不重复计数，也不加上首次 dashboard 失败前的 54 项。Synthetic 流程没有功能检查计数，不计入 299。[最终场景门禁](release-ui-gates.json)

| 项目 | 当前状态 | 已观察结果／待补证据 |
| --- | --- | --- |
| release 构建 | **通过** | 退出 0，命令耗时 **342.953 秒**；[构建记录](release-build.json) |
| 隔离目录安装 | **通过** | 退出 0，命令耗时 **0.066 秒**，产物哈希一致；[安装记录](release-build.json) |
| UI preview | **通过** | **38 项**，退出 0，命令耗时 **3.872 秒**；[详细报告](preview-e2e.json)。样例事件与隔离 PTY，无真实服务连接 |
| SSH composer | **通过** | **88 项**，退出 0，命令耗时 **15.094 秒**；[首轮记录](release-initial-attempt.json)、[详细报告](composer-e2e.json) |
| Dashboard 首次运行 | **失败，保留记录** | **54 项通过后退出 1**，命令耗时 **29.726 秒**；[首轮记录](release-initial-attempt.json)、[详细失败报告](dashboard-initial-attempt.json) |
| Dashboard 修订夹具重跑 | **通过** | **84 项**，退出 0，命令耗时 **25.867 秒**，精确 Home 边界检查通过；[详细报告](dashboard-e2e.json) |
| Account relay | **通过** | **37 项**，退出 0，命令耗时 **5.127 秒**；真实客户端连接合成账号网关；[详细报告](account-relay-e2e.json) |
| Relay composer | **通过** | **45 项**，退出 0，命令耗时 **6.276 秒**；release 客户端与 debug router fixture；[详细报告](relay-composer-e2e.json) |
| TTY observer | **通过** | **7 项**，退出 0，命令耗时 **12.740 秒**；recording launcher 启动实际 viewer；[详细报告](observer-e2e.json) |
| Synthetic benchmark | **协议流程通过** | 退出 0，命令耗时 **2.47 秒**；未设置性能预算；[详细报告](synthetic-benchmark.json) |
| 本集成候选的安装包 | **pending** | 待填固定打包提交、包摘要和包内二进制对应关系 |
| 本集成候选的隔离 staging | **pending** | 待填检查结果、清理状态及生产服务是否发生变化 |
| 本集成候选的用户入口安装／发布 | **pending** | 待填实际安装入口或发布产物；历史候选操作不移记到本轮 |

表中耗时均为外层命令计时。详细报告内部计时分别为 composer **14.911 秒**、dashboard 首次运行 **29.436 秒**、dashboard 重跑 **25.578 秒**、relay composer **6.093 秒**、synthetic 流程 **2.093 秒**；计时边界不同，不能混用。Account relay、observer 与 preview 的详细报告没有独立耗时字段。首轮汇总因 dashboard 失败保留 `status: failed`，其中 preview 与 composer 的通过结果仍分别有效。[首轮汇总](release-initial-attempt.json) · [最终汇总](release-ui-gates.json)

### Dashboard 首次失败与夹具修订

首次失败发生在 Home 滚动边界检查，错误为 `timed out waiting for Home at the retained history boundary`。旧夹具按此前 **512 行**的本地滚动历史预期寻找更早记录，合入实现已将这部分可见历史限制为 **128 行**，旧断言因而不再符合当前契约。[首次失败报告](dashboard-initial-attempt.json) · [内存优化中的历史边界](../../../../docs/client-memory.md)

`54f90d2…` 仅调整测试：从实际 history 长度和 scrollback 偏移计算最早可见记录，核对精确的 128 行边界，验证重复 Home 不会越界，并检查 End 恢复实时尾部、偏移回到 `0`。这不是把首次失败改成通过；首次诊断与修订后结果应分别保留。[夹具修订](https://github.com/LIghtJUNction/ssmcp/commit/54f90d250a520ca54778d4f0da0bce2700e0064c)

修订后 84 项通过，`log_history_boundary` 保存了以下实际值：[dashboard-e2e.json](dashboard-e2e.json)

| 指标 | 实测值 |
| --- | ---: |
| `retained_rows` | 128 |
| `paged_offset` | 35 |
| `expected_oldest_record` | 541 |
| `observed_oldest_record` | 541 |
| `repeated_home_offset` | 128 |
| `end_offset` | 0 |

期望与实际最早记录一致，重复 Home 保持在保留边界，End 回到零偏移。最终门禁记录 `preserved_evidence_unchanged: true`，并保留首次汇总、dashboard 报告和日志的摘要；首次失败没有被重跑结果覆盖。[证据保留记录](release-ui-gates.json)

### Synthetic 样本的解释

本次样本使用 **500 个 synthetic 工具、2 次目录扫描、8 条命令**，配置 `idle_seconds: 0.5`，在真实客户端 TUI 中完成协议流程。报告为 `workload.ok: true`、`client_exit_code: 0`、`binary_unchanged_during_run: true`。这是合成提供方的兼容性样本，不是实际模型调用。[synthetic-benchmark.json](synthetic-benchmark.json)

采样间隔为 50 毫秒，统计对象是客户端进程 RSS + Swap，排除子运行时。`budget_mib` 与 `max_footprint_mib` 均为 `null`，因此 `passed: true` 不代表通过了内存上限或性能改进门槛。该短样本没有对照组，不用于声称长期峰值、稳定上限或相对历史版本的改进；本页不复用历史 MB 数值。[执行器解释](release-ui-gates.json)

## 能力与证据边界

队列中的 `delivered` 表示要求已附加到符合目标的 `code_mode` 响应，不表示模型已经阅读或执行。取回编辑需等待服务器确认；交付不确定的记录不自动重发。要求队列需要协议 v7，源码审计需要 v6；UI 配置重载与实际客户端 `/reload` 的行为不同。[客户端界面契约](../../../../docs/client-ui.md)

128 行限制作用于本地可见滚动历史，不能推导为原始命令输出只剩 128 行，也不能承诺所有界面内容永久归档。输出读取继续受捕获状态、归档保留期和容量限制。本轮报告不复用历史内存基准的 MB 数值，也不据实现合并声明新的 RSS、峰值或性能预算已通过。[内存与输出边界](../../../../docs/client-memory.md)

SSH composer 与 dashboard 使用实际 release 客户端和 daemon、隔离 OpenSSH、OAuth 与真实 PTY。Account relay 使用真实客户端和 Python 合成账号网关；relay composer 使用 release 客户端与 `51ec5ad…` 的 debug 库测试程序，后者运行生产 Broker/router，并通过真实 DCR、PKCE、consent、token exchange 和队列实现验证账号路径。[Composer](composer-e2e.json) · [Dashboard](dashboard-e2e.json) · [Account relay](account-relay-e2e.json) · [Relay composer](relay-composer-e2e.json)

Relay composer 的上游 GitHub 是合成提供方，传输是回环 HTTP/WS，报告明确 `tls_verified: false`、`real_github_account: false`、`models_started: false`；不作为公网 TLS、真实 GitHub 授权或 release daemon 账号 relay 的证明。Observer 的 recording launcher 也不等于特定桌面终端的人工视觉验收。[Relay 范围](relay-composer-e2e.json) · [Observer 范围](observer-e2e.json)

本轮测试证据已归档；本集成候选的安装包、staging 和用户入口安装／发布仍保留 pending，待各自真实记录补齐。
