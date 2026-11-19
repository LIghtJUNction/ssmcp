# ssmcp 运行时预检与安装验收

日期：2026-11-19（Asia/Taipei）

`ssmcpd check-runtime` 已完成，真实 release 服务程序通过普通环境 **7/7** 和完整 systemd 服务限制下 **7/7** 的安装验收。运行时改动已经进入 [日志面板集成 PR #39](https://github.com/LIghtJUNction/ssmcp/pull/39)。本报告不把这组运行时结果当作整个日志客户端、正式发布包或生产部署已通过的证明。

## 解决的问题

HTTP 健康检查和 OAuth 检查没有执行 JavaScript runner，可能遗漏“能发现客户端、但 code_mode 无法执行”的故障。预检实际运行固定 JavaScript 程序，让依赖、安装路径和沙箱故障在网关接收请求前暴露。[核心实现](https://github.com/LIghtJUNction/ssmcp/commit/f0d83c90ef0ba5ab816ac9b541b11dd9cc85a0fa)

| 入口 | 行为 |
| --- | --- |
| `ssmcpd check-runtime` | 在读取网关配置之前，经既有 Node.js、bubblewrap、prlimit 和 runner 执行固定程序。客户端列表为空，原生调用被拒绝。 |
| 成功 | 标准输出为 `{"code_mode":"ok"}`，退出码 0。 |
| 失败 | 非零退出，并报告依赖、安装路径、沙箱启动或返回结果问题。 |
| systemd | `ExecStartPre` 在实际服务用户、环境和限制下执行预检，通过后才启动网关。 |
| 隔离部署 | 暂存服务执行同一预检和打包服务限制，失败时不产生通过凭据。 |
| Linux 发布 | x86_64、ARM64 原生 Ubuntu 24.04 任务验收安装后的对应 release archive；发布收尾依赖两项通过。 |

发布构建继续使用既有 Ubuntu 22.04 环境。[工作流与安装检查](https://github.com/LIghtJUNction/ssmcp/commit/5c60997c202a58e0c31cd2dfcefa7b3b15bd4dfb)

## 实际 release 二进制验收

验收源码固定为 [af3787ebfec05f59272623da2e2cd8776ab1455e](https://github.com/LIghtJUNction/ssmcp/commit/af3787ebfec05f59272623da2e2cd8776ab1455e)。`cargo build --locked --release --bin ssmcpd` 成功，耗时 2 分 18 秒。随后使用实际二进制和配套 runner 执行 `scripts/test_server_runtime.py`。安装前缀包含空格，配置文件故意不存在，用以证明预检无需配置或凭据。

| 场景 | 普通环境 | systemd 限制下 |
| --- | --- | --- |
| 完整安装且前缀包含空格 | 通过 | 通过 |
| 移走安装目录的 runner，明确失败 | 通过 | 通过 |
| 默认 runner 缺失，合法绝对覆盖路径可用 | 通过 | 通过 |
| 拒绝相对覆盖路径 | 通过 | 通过 |
| 拒绝不存在的覆盖文件 | 通过 | 通过 |
| 拒绝目录作为覆盖路径 | 通过 | 通过 |
| 恢复默认 runner 后再次成功 | 通过 | 通过 |

systemd 验收以非 root 开发用户执行，`PATH=/usr/bin:/bin`，启用打包服务全部九项限制：NoNewPrivileges、PrivateTmp、PrivateDevices、ProtectSystem、ProtectHome、ProtectKernelTunables、ProtectControlGroups、RestrictAddressFamilies、ReadWritePaths，另设 UMask=0077。临时 systemd 验收单元和暂存目录已经清理。

| 产物 | SHA-256 |
| --- | --- |
| release ssmcpd | `66ecba784e87219f348477627b648ac7ddf4a95c040c4dc772fb138cec4bf6d9` |
| 配套 JavaScript runner | `5f37606c045d0c0843127157878770cc2f32ab4273648369c26952425a662cba` |

这些结果验证的是实际 release 二进制的安装布局和沙箱执行。正式发布 archive 的下载、内容完整性与安装门禁仍需要正式发布流程执行，不能由本次二进制验收替代。

## 其他验证及集成修复

| 验证 | 结果与对应范围 |
| --- | --- |
| 完整 code_mode 模块 | 33 项通过，包含真实系统沙箱；开发机源码 [683cfb5](https://github.com/LIghtJUNction/ssmcp/commit/683cfb5)。 |
| server_runtime CLI | 4 项通过，包含 ignored 真实沙箱用例；同一原生验证。 |
| setup_server | 10 项通过，覆盖配置迁移、失败恢复、隔离、启动预检和限制一致性。 |
| release tooling | 58 项通过，覆盖发布条件、版本和产物约束。 |
| Rust 格式、工作流检查 | cargo fmt 与 actionlint 通过。 |
| 最后一次完整 CI 的客户端平台 | macOS Intel/ARM、Windows x64/ARM 四项通过，源码为 59ef9c6。 |
| 最后一次完整 CI 的 Linux | x64/ARM 均完成 Rust、真实运行时、CLI、systemd 限制和重启检查；随后在真实 SSH/OAuth/HTTP/PTY 套件中的过期 readUri 夹具断言失败。完整 CI 因此不是全绿。 |

最后一次完整 CI 为 [run 44372266403](https://github.com/LIghtJUNction/ssmcp/actions/runs/44372266403)。修正后的 readUri 夹具已进入 [b9596e6](https://github.com/LIghtJUNction/ssmcp/commit/b9596e6303628706fd2656bf80c6e4e928dfdacb)：逐项核验实际 client/root/path，回读原文，再严格比较其余原生字段。该修正与日志、懒加载组合后的完整验收尚未完成。

[ab0d111](https://github.com/LIghtJUNction/ssmcp/commit/ab0d111) 为 Unix HistoryLock 增加事务结束时显式解锁，避免 fork 继承文件描述符导致已结束事务继续占锁；确定性回归仍验证真实持锁竞争被拒绝。它还修正上下文测试的绝对工作区 identity 和 Windows 引用夹具。[af3787e](https://github.com/LIghtJUNction/ssmcp/commit/af3787e) 与 [59ef9c6](https://github.com/LIghtJUNction/ssmcp/commit/59ef9c6) 保留精确公共工具和原生工具目录断言。

## 当前交付边界与恢复入口

[PR #40](https://github.com/LIghtJUNction/ssmcp/pull/40) 保留运行时独立审阅入口；其代码已经作为祖先进入 [PR #39](https://github.com/LIghtJUNction/ssmcp/pull/39)。本线程没有替换正在使用的客户端，也没有执行生产部署或正式发布。

组合候选 b9596e6 的开发机验收使用独立工作树和后台单元。首次任务在 Git 获取提交阶段超时，尚未进入编译；其后已发出携带连接环境的重试请求，但调用未完成，不能确定重试是否启动或结束。随后 ssmcp 有时限只读重连也失败，当前工作区被标记为 unavailable。

恢复后先读取以下已知位置的实际状态，不重复启动不明状态的任务：

- 工作树：`.worktrees/logs-acceptance-b9596e6-20261119`。
- 状态与日志：`.local/logs-acceptance-b9596e6-20261119/result.json`、`unit.log`、`unit-retry1.log`。
- 原单元：`ssmcp-logs-acceptance-b9596e6-20261119`。
- 未确认的重试单元：`ssmcp-logs-acceptance-b9596e6-20261119-retry1`。

检查确切 HEAD、二进制哈希、退出码和脚本来源后，再继续统一候选验收与发布。不得把其他分支的测试数量合并为当前候选的通过结果。
