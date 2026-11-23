# 公共 RAM 过期与自动清扫：源码可行性设计

**状态：仅完成有界源码分析，尚未实现或运行。** 固定观察源为 `6759712e1b51f83690580897aee7efb01ac32f86`；已冻结的 `35b1357` 和 `bb7631b` 保持各自独立输入身份。本方案不构成整体验收通过或改变原门禁的批准。

## 问题与已核事实

真实输出 store 属于客户端进程。现有账户服务夹具只建立 Broker、账户/OAuth 和公共 router，另行启动真实客户端。把服务端的另一份内存改成“过期”，无法证明客户端收到的公共工具调用访问了同一份状态。

已核实的归属链为：

| 位置 | 实际接缝 |
| --- | --- |
| `Process::with_observer` | 保存传入的 output Arc；terminal 与 shell 使用它的 clone |
| `Runtime::with_session_control` | 从 `backend.output()` 取得同一份 store |
| `Runtime::output()` | 返回保存的 Arc 的 clone |
| `Provider::Output` | 直接对该 Runtime 的 `self.output` 调用 `perform` |
| `client::connect_inner` | 创建真实 Backend/Runtime，并绑定实际 client ID |

来源：[Process](https://github.com/LIghtJUNction/ssmcp/blob/6759712e1b51f83690580897aee7efb01ac32f86/src/runtime/native_plugins/process.rs)、[Runtime](https://github.com/LIghtJUNction/ssmcp/blob/6759712e1b51f83690580897aee7efb01ac32f86/src/runtime.rs)、[Client](https://github.com/LIghtJUNction/ssmcp/blob/6759712e1b51f83690580897aee7efb01ac32f86/src/client.rs)。

客户端入口有两个确定的包装障碍：`run()` 读取进程 argv；`connect_inner()` 取得 `current_exe()` 并交给运输子进程。库测试不能把自身的测试参数和 lib-tests 可执行文件直接当作普通客户端使用。

## 建议的后继修改范围

### 客户端测试入口

在 `src/client.rs` 增加仅测试构建可用的私有接缝，专用 ignored fixture 复用现有 `Options::parse(iter<OsString>)` 与真实连接路径。正常产品入口继续使用原来的当前可执行文件；测试入口显式提供已校验路径、大小和 SHA 的真实 `ssmcp` 运输可执行文件。

真实 Runtime 创建并绑定 client ID 后，使用定向 callback 或 oneshot 把该次连接的 Runtime 交给 fixture。fixture 调用 `runtime.output()`，核对同一对象，记录 client ID、连接尝试和目标 artifact ID。避免全局按名称寻找 store，也无需为取得 Arc 引入整套 session attachment 状态。

拟议的专用模块可放在 `src/client/output_expiry_fixture.rs`，仅在 `cfg(all(test, unix))` 下编译。该路径和入口是新提案，当前源码尚未提供。

### 内存状态与清扫见证

在 `src/output.rs` 中增加有界、仅测试可用的操作：

- 只接受已存在、已正常完成且属于该 fixture 的精确 ID；
- 在真实 `Capture::finish()` 之后将其 `expires_at_unix` 设为过去，避免 finish 再次刷新过期时间；
- 保留默认 900 秒 Policy，不直接删除条目，也不强制改变 active 标志；
- 在真实定时清扫执行的同一次状态临界区，记录目标 ID 确由该轮清扫移除以及释放的字节；
- 观察逻辑只记录事实，不执行清扫，不另写替代的过期算法。

该 store 必须在活跃 Tokio runtime 中创建并持续受到调度，原实现才会启动持有 Weak 的 60 秒后台循环。普通同步 store 外面等待一段时间，不会自动得到这个循环。

### Python 编排

只为过期场景启动 fixture-backed client，其余阶段保留原客户端。可新增 `scripts/e2e_output_expiry.py` 帮助模块，并在后继提交中衔接 `Unified.expiration`。

继续复用 AccountGateway、隔离登录环境、OAuth、连接记录校验以及公共 `tools/call → code_mode → native output` 路由。lib-test 有测试框架 stdout，不能套用普通客户端“第一行就是连接 JSON”的解析；需要专用私有 ready/control 记录。

进程应在等待 ready 之前登记到 Harness 的自有清理范围。就绪失败、控制断连、超时与正常结束均沿原清理流程回收，保留真实错误和独立清理结果。私有控制接口不增加生产 MCP 工具、TTL 参数或时钟覆盖。

来源：[AccountGateway](https://github.com/LIghtJUNction/ssmcp/blob/6759712e1b51f83690580897aee7efb01ac32f86/scripts/e2e_account_gateway.py)、[relay fixture](https://github.com/LIghtJUNction/ssmcp/blob/6759712e1b51f83690580897aee7efb01ac32f86/src/http/relay_composer_fixture.rs)。

## 必须保留的证据顺序

1. 经真实公共调用生成并完成输出，读取其实际内容，固定 ID 和摘要。
2. 在同一客户端的同一 store 中老化该已完成条目。
3. 自动清扫观察期间暂停该 store 的公开读取、查询、手动 cleanup 以及输出分配／追加；`Capture::metadata/read_range` 也不能用作等待条件。
4. 先取得真实定时清扫对精确目标的因果见证，再通过公共 read/stat 取得实际不可用响应。
5. 核对新鲜／活动对照条目仍保留，并记录正常结束与失败路径的清理结果。

这些顺序限制有具体原因：公开 read/stat、Capture 读取、spill/room 等路径都会自行执行过期清理。单看“读取后条目消失”、一次 cleanup 计数或一个定时 tick，都不足以证明自动清扫移除了目标。

## 时间安排需要明确选择

整体 240 秒预算和 900 秒生产 TTL 保持不变；旧测试本来就加速时间戳，不需要新增自然等待 900 秒的要求。

| 方案 | 所需安排 | 必须明确的变化 |
| --- | --- | --- |
| 等一次真实清扫周期 | 单场景约 70–75 秒上限，全部计入整体 240 秒 | 明确改变原 5 秒场景观察范围；需集成负责人决定 |
| 保留 5 秒观察窗 | 先预热，并从实际 Tokio Sleep.deadline 与轮次取得信号，在确认的清扫前老化目标 | 当前没有这个信号，需要仅测试的周期观察；所有预热仍计入 240 秒，错过轮次不能隐瞒重试或重置总期限 |

较小的工程实现是第一种；保留原 5 秒观察窗需要额外同步。两者目前均未实现或运行。没有明确决定或没有有效见证时，继续保留 `bb7631b` 的 `not_exercised → incomplete / exit 1`。

## 结论的边界

拟议夹具覆盖私有 loopback **HTTP/WS 上的生产账户、OAuth、Broker 与 native 路由**。它不包含生产 TLS 握手。普通 Rust 过期单测可以另行证明已过期 RAM 条目的机制，不能代替本场景，也不能声称实际观察了 900 秒自然流逝或 60 秒扫描。

未来执行必须使用同一固定源码构建且实际通过所需 CI 的预编译 lib-tests 与真实 ssmcp，并保存精确输入指纹、选择到的测试、原始响应、定时见证和清理结果。当前这些产品运行结果全部为空。
