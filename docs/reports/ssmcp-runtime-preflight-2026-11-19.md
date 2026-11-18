# ssmcp 运行时预检与发布验收

日期：2026-11-19（UTC+8）

本轮实现了 `ssmcpd check-runtime`，并将实际 JavaScript 执行检查接入服务启动、隔离部署与 Linux 发布包验收。改动已推送至 [PR #40](https://github.com/LIghtJUNction/ssmcp/pull/40)，保留为草稿，等待完整编译与真实系统沙箱验收。

## 解决的问题

本轮实际调用中，ssmcp 能列出在线客户端，但 `code_mode` 返回 `code_mode runner is missing`。现有 HTTP 健康检查和 OAuth 检查未执行 JavaScript runner，因此会漏掉这类执行故障。

安装规则已包含 runner。本轮保留既有安装路径与沙箱要求，增加能直接检测故障的执行检查；在线服务的具体安装目录仍需在恢复执行能力后核实。[核心实现提交](https://github.com/LIghtJUNction/ssmcp/commit/f0d83c90ef0ba5ab816ac9b541b11dd9cc85a0fa)

## 实现后的行为

| 入口 | 行为 |
| --- | --- |
| `ssmcpd check-runtime` | 在读取网关配置之前，通过既有 Node.js、bubblewrap、prlimit 和 runner 执行固定程序。客户端列表为空，所有原生调用都被拒绝。 |
| 检查成功 | 标准输出为 `{"code_mode":"ok"}`，退出码为 0。 |
| 检查失败 | 返回非零退出码，并报告依赖、安装路径、沙箱启动或返回结果的具体问题。缺失 runner 的错误包含预期安装路径。 |
| 打包的 systemd 服务 | `ExecStartPre` 在实际服务用户、环境和运行限制下执行检查；通过后才启动网关。 |
| 隔离部署 | 暂存服务执行同一预检，并使用与打包服务一致的运行限制；失败时不生成通过的部署凭据。 |
| Linux 发布 | x86_64 和 ARM64 均下载实际构建的 archive，在原生 Ubuntu 24.04 环境中验证安装后的服务。发布收尾任务依赖此项通过。 |

发布构建继续使用既有 Ubuntu 22.04 构建环境。新增验收没有更改发布触发条件或权限。[发布验收提交](https://github.com/LIghtJUNction/ssmcp/commit/5c60997c202a58e0c31cd2dfcefa7b3b15bd4dfb)

## 已完成的验证

| 验证 | 结果 | 范围 |
| --- | --- | --- |
| `scripts/test_setup_server.py` | 10 项通过 | 配置迁移、失败恢复、部署隔离、启动预检与服务运行限制一致性。 |
| `scripts/test_release.py` | 58 项通过 | 既有发布条件、版本和产物完整性约束。 |
| 新增 Rust 检查的定向验证 | 3 项通过 | 使用原文生产源码、真实 Node runner 和既有测试辅助路径，验证结果校验、原生调用拒绝及协议行为。 |
| `cargo fmt --all -- --check` | 通过 | Rust 格式检查。 |
| actionlint 1.7.12 | 通过 | CI 与 Release 两个工作流。 |
| Python 语法和参数帮助 | 通过 | 新增发布安装验收脚本。 |
| `git diff --check` | 通过 | 补丁空白检查。 |

定向 Rust 验证使用 Rust 1.100.0、Node v24.22.1，临时测试程序编译耗时 16.74 秒，三个测试耗时 0.22 秒。测试直接引用或按原文提取本轮工作树中的相关生产模块，未模拟执行返回值。它覆盖返回值与拒绝调用逻辑，以及真实 Node 通信协议；完整 crate、CLI 集成和 bubblewrap 执行均单独列为待验收项。

三个通过的测试为：

- `runtime_check_rejects_missing_wrong_and_error_results`
- `runtime_check_denies_every_host_operation_and_records_the_attempt`
- `runtime_check_program_uses_the_runner_protocol_without_clients_or_host_calls`

## 已加入、待执行的真实环境验收

`scripts/test_server_runtime.py` 从实际二进制或发布 archive 构造含空格的安装前缀，覆盖七个场景：完整安装成功、移走 runner 后失败、合法绝对覆盖成功、相对覆盖失败、缺失文件覆盖失败、目录覆盖失败，以及恢复默认 runner 后再次成功。每次检查都使用不存在的服务器配置路径，以验证命令无需配置或凭据。

当前环境的完整 Rust 编译被系统以 SIGKILL 终止；bubblewrap 的隔离环境初始化返回 `NETLINK_ROUTE socket: Operation not permitted`。这些结果未计为通过。GitHub CLI 与浏览器当前均没有可用登录会话，远程 CI 未触发。

PR #40 保留草稿。完整验收需在支持项目既有沙箱的开发机或 GitHub Linux runner 上执行，随后再评估合并。在线服务仍需恢复与运行二进制匹配的 `libexec/ssmcp/code-mode.js`，本轮未变更在线部署。

## 交付位置

- [PR #40：全部修改与审阅入口](https://github.com/LIghtJUNction/ssmcp/pull/40)
- [运行时检查、服务启动和部署预检](https://github.com/LIghtJUNction/ssmcp/commit/f0d83c90ef0ba5ab816ac9b541b11dd9cc85a0fa)
- [发布包验收、CI 接入与使用文档](https://github.com/LIghtJUNction/ssmcp/commit/5c60997c202a58e0c31cd2dfcefa7b3b15bd4dfb)
