# SSMCP 输出生命周期与托管文件清理验收

日期：2026-11-23 UTC

**本轮验收通过。** 同一固定源码下，73 项相关 Rust 回归、27 项 Python 回归、完整 Unified 的 12 阶段／114 项检查，以及真实双 UID／PTY 的 20 项检查均通过。完整 Unified 报告耗时 **100.876 秒**，保留原 **240 秒主体期限**。

功能源码已推送 main：[d0fb4460b119b277423114fa65845acd2dc6e350](https://github.com/LIghtJUNction/ssmcp/commit/d0fb4460b119b277423114fa65845acd2dc6e350)。本机冻结候选为 `6484273dd62e37b3817723af0176a71c8a9c81e8`，二者完整源码树均为 `b4c808e0596bdb3a904598ee6011d590cfe90991`。提交元数据不同，源码树逐项相同。本文与 [acceptance-facts.json](acceptance-facts.json) 随后以文档提交记录这次结果。

## 本轮实际验证

| 范围 | 实际结果 | 说明 |
|---|---|---|
| 格式与差异检查 | 通过 | `cargo fmt --all`、`git diff --check` |
| 严格 Clippy | exit 0 | `cargo clippy --workspace --locked --all-targets -- -D warnings` |
| Rust 针对性回归 | 73 通过，0 失败 | Output 23；公共客户端夹具 3；managed files 21；workspace 6；reload 9；runtime 10；扫描 join 1 |
| Python helper | 19 通过 | 协议、绝对期限、PID 身份与失败清理 |
| Python Unified 接线 | 8 通过 | 阶段结果、证据标记与失败优先级 |
| 原始 Unified | 12 阶段、114 项检查通过 | `coverage_complete=true`；无 coverage gap；无残留观察进程 |
| 真实双用户／PTY | 20 项检查通过 | UID 65534 与 65533；同一测试程序 SHA-256 |

Cargo 清单仍报告 `unused dependency qrcodegen`；严格 Clippy 命令成功不等于全程无警告。普通 Rust 筛选中的两个专用夹具显示为 ignored，随后分别由公共过期和双 UID 场景显式驱动；它们不计入上述 73 项普通回归。

所有 Cargo 步骤使用同一共享 target，并由一个批次外层锁串行保护。实际验收开始和结束均核对源码与二进制身份。最终运行 job 为 `2bfac62651c6a21ef468f209e61157c5-93a`，退出码 0，stdout/stderr 均已到 EOF。

### Unified 各阶段

| 阶段 | 内容 | 结果 | 秒 |
|---|---|---|---:|
| account_oauth | 账号与 OAuth | 通过 | 1.545 |
| catalog_no_harness | 无 Harness 工具目录 | 通过 | 0.373 |
| concurrent_execution | 并发执行 | 通过 | 2.592 |
| utf8_transport | UTF-8 传输 | 通过 | 0.724 |
| output_storage | 输出存储与分页 | 通过 | 11.489 |
| file_skill_audit | 文件、技能与审计 | 通过 | 13.296 |
| client_isolation | 客户端隔离 | 通过 | 1.621 |
| cancel_timeout | 取消与超时 | 通过 | 3.248 |
| native_parent_exit | 原生父进程退出 | 通过 | 0.203 |
| automatic_expiration | 公开自动过期 | 通过 | 63.656 |
| reload | 重载 | 通过 | 1.322 |
| disconnect | 断连 | 通过 | 0.219 |

`coverage_complete` 限于该脚本定义的 **no-Harness Unified runtime**。已安装原生 Harness 的发现、授权和具体提供者验收需使用各自场景；本轮未执行模型调用。

### 实际过期证据

场景耗时 **63.632 秒**；从 arm 请求到收到真实 timer 见证为 **58.986 秒**。arm 返回 `deferred=true`，不含尚未发生的 `new_expires_at_unix`。timer 见证明确记录 `aging_point=timer_sweep_before_expiry`：

| 时间字段 | Unix 秒 |
|---|---:|
| target 原到期时间 | 1795409859 |
| 实际 timer 时间 | 1795409019 |
| timer 锁内注入的到期时间 | 1795409018 |

实际删除的 target 为 1,200 字节，SHA-256 为 `ed48a4a1d311d38f972e479f37bb69f4f4e4d76fc26d224587f34a49d344603e`。随后公开 `ssmcp.output.read` 与 `ssmcp.output.stat` 均拒绝该 ID。fresh 和 active 各 1,200 字节，经公开接口完整读取并核对原哈希；active 在释放后正常完成，内容保持不变。

清理耗时 **0.024 秒**，20 秒预算内收到正常 stop 确认。观察到的 **11 个进程实例全部退出**，夹具退出码 0，无强制回收、无清理错误、无残留实例。双用户测试随后仍使用同一 SHA-256 的测试程序，并验证跨 UID 互拒、同／异 PTY 行为及自有文件和终端清理。

## 变更与原因

输出 ID 指向当前 Runtime 的内存对象。查询和分页读取都必须在该 Runtime 仍持有对象时成立；重载换掉 Runtime 后，旧 ID 应被拒绝，新连接应能产生并读取新输出。脚本因此改为验证真实字节、分页哈希和重载边界。跨用户夹具也使用各进程实际持有的内存 store，保留真实降权 UID 和 PTY 检查。

托管临时文件是另一类资源：注册表记录哪些磁盘文件可以由生命周期清理器处理。当前文档仍承诺启动、每五分钟和重载清理。完整流程暴露出这些运行时调用曾被移除，因此恢复连接后的 startup 扫描、每五分钟且不重叠的扫描，以及 reload 时先停止后端、再清理、最后关闭审计的顺序。现有隔离断言继续要求真实 cleanup 事件。

最初的过期夹具在 arm 时就把目标到期时间前移。审计写入和普通输出共用 store，后续写入也会按需清理已过期对象；因此，目标可能在定时器取得锁之前消失，夹具就无法取得 timer 的实际删除见证。源码与回归确认了这条可行竞争路径，历史失败中具体是哪条事件先删除目标尚未被精确定位。

后继把时间注入移到真实定时器取得 store 锁之后。arm 只记录三个对象的 ID、字节数、哈希及原到期时间；定时器用同一锁和同一次 now 逐项复核，仅将 target 的时间设为 max(now - 1, 0)，再调用原清扫算法。见证从实际被删除的 Entry 生成，随后公开 read/stat 必须拒绝 target，fresh 和 active 必须保留完整字节。任何对象或到期时间被改动、对象提前被其他路径删除，都会使观察失败；失败也不会跳过普通清扫。

这一注入仅存在于测试构建。生产保留期为 900 秒，真实后台周期为 60 秒。证据使用 aging_point=timer_sweep_before_expiry、aging_deferred=true 和 observed_after_arm_seconds，明确区分它与旧版立即 aging 的观测。

## 历史失败与修复

同意页断言曾错误期待上游 GitHub 登录名，而产品按合同展示平台账号地址。修复了精确期望值，并保留同意/拒绝、PKCE、回调、state、issuer 与 resource 检查。

首次完整 Unified 在第七阶段 client_isolation 失败，保留了 76 项检查记录。恢复托管文件生命周期后，原失败阶段及前九阶段通过；后一次在第十阶段 automatic_expiration 失败，保留了 95 项检查记录。两次运行保持 failed，不用后继结果改写历史。

旧版立即 aging 的独立聚焦场景确实曾通过 26 项检查，耗时 64.859 秒。它只证明该次运行通过，不证明不存在竞争窗口，也不计作后继完整流程通过。

## 验证边界

过期场景使用私有 loopback HTTP/WS 的真实账号、OAuth 和公开调用路径。单场景预算 75 秒，完整 Unified 主体预算 240 秒；Python 清理预算 20 秒，Rust 夹具清理预算 15 秒。自然等待 900 秒、TLS 握手，以及自然等待五分钟后观察托管文件周期扫描均未覆盖。周期配置、扫描互斥和退出 join 有源码与针对性回归；真实启动和重载清理另有实际检查。

本轮交付源码和本机验收记录，没有创建新版本发布或执行安装。原主工作区未提交的服务器升级改动保留在原位置，迭代在独立工作树完成。

## 分次提交

| 次序 | GitHub 提交 | 内容 |
|---|---|---|
| 1 | [9971c03](https://github.com/LIghtJUNction/ssmcp/commit/9971c038ed4b04c60d5f6e0d27b2fa07a3567fa6) | 跨 UID RAM 夹具与真实 PTY 检查 |
| 2 | [8c882e8](https://github.com/LIghtJUNction/ssmcp/commit/8c882e8fcbc467ad28874f170fe0bec44bd91415) | RAM 会话脚本及平台账号同意页断言 |
| 3 | [2723648](https://github.com/LIghtJUNction/ssmcp/commit/272364807d8a726d5fab81ec94825f69ddd4c44e) | 公共自动过期夹具初版，保留历史结果 |
| 4 | [be1bacf](https://github.com/LIghtJUNction/ssmcp/commit/be1bacfb70a296a0f1182fa1e4999e5636143ad9) | 恢复托管文件启动、周期及重载清理 |
| 5 | [d0fb446](https://github.com/LIghtJUNction/ssmcp/commit/d0fb4460b119b277423114fa65845acd2dc6e350) | 定时扫描内延后时间注入与严格回归 |

这些结果分组记录，不把旧版聚焦场景、旧失败运行与后继结果相加当作一次总测试数。前两阶段还曾独立验证 53 项会话检查、1,179,809 字节／37 页的精确输出，以及对应平台同意页的 Rust 回归；具体来源见 JSON 中的历史记录。

## 二进制与证据身份

| 程序 | 字节数 | SHA-256 |
|---|---:|---|
| ssmcp | 239,716,112 | `638b939ce5997c4c6fd64d88989b6c7ac8fe466a856389019fd5d731d8ddf51d` |
| ssmcpd | 172,531,120 | `109be58c5384c0f2e87958c800a9df19585d20a306b37604cce28e49086063ba` |
| library_fixture | 378,636,392 | `94147cac952a247ab47683bf81f61df269fcf8584e59cf4fb3566ece07593c77` |

最后一项过期后继仅修改测试构建的过期模块与验收脚本，重新构建后的生产客户端和服务端哈希与托管文件修复版本一致。测试程序哈希变化，与新回归及延后注入实现对应。

可机器读取的完整摘要：[acceptance-facts.json](acceptance-facts.json)。开发机原始本轮记录位于 `~/.local/state/work-maintainer/ssmcp-output-lifetime-20261123/deferred-expiry/`，包括 `build-validation.json`、`candidate.json`、`live/validation.json`、`live/unified.json` 和 `live/multi-user.json`。历史失败继续保存在同级 `expiry/live/` 与 `managed-lifecycle/live/`。

## 对应源码

- [内存输出及仅测试过期模块](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/src/output.rs)
- [公开过期测试客户端](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/src/client/output_expiry_fixture.rs)
- [公开验收控制器](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/scripts/e2e_output_expiry.py)
- [完整 Unified 入口](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/scripts/e2e_unified.py)
- [连接与扫描生命周期](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/src/client.rs)
- [Runtime 清理与审计顺序](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/src/runtime.rs)
- [托管文件合同](https://github.com/LIghtJUNction/ssmcp/blob/d0fb4460b119b277423114fa65845acd2dc6e350/docs/managed-files.md)
