# 注销后的授权拒绝与写错误竞争

## 结论与证据范围

正式预检 [44754296839 的 Linux x86 任务](https://github.com/LIghtJUNction/ssmcp/actions/runs/44754296839/job/136350165955) 在源码 `4c6178eb811049ce1fce4b6d16803e0e4f7b410d` 的账户注销检查失败。日志记录了 `Broken pipe`、一次 watchdog 重试，随后重新授权被拒绝。旧断言没有打印退出码和连接计数，不能从该日志还原当次两个比较值。

本次修复处理了已连接退出路径中的协议竞争：写操作先失败时，收尾会终止唯一读取任务，尚未读到的最终授权拒绝因此可能丢失。运行在隔离开发机上的修复版本完整账户流程已通过，实际值为 **退出码 1（撤销合同要求的错误退出）、连接次数 2 → 2**；重新授权只发起一次，已撤销凭据被移除。[账户夹具与原严格断言](../../../scripts/e2e_account_relay.py) · [机器验证回执](validation.json)

受测源码为 [`d417a27a47de332dc4a5e0bf4be0c74bf7f1c591`](https://github.com/LIghtJUNction/ssmcp/commit/d417a27a47de332dc4a5e0bf4be0c74bf7f1c591)。产品改动来自 `b4d648b37a89c3740c38e3a22ee21475288ea62a`；后一个提交仅将测试的单次 Future 轮询改为标准库接口，适配现有依赖功能集。

## 修复方式

账户连接因错误退出时，先停止已接收的请求，再在停止读取任务前接收队列中的剩余帧。退出阶段不会执行这些排队操作；继续接收使满队列后面的最终 `Reject` 也有机会到达。整个接收过程共用两秒上限，收到终态即返回，普通帧不能重置期限。[实现：Incoming::after_connection_error 与 connected 清理](../../../src/client.rs)

只有原始 wire `Reject` 被既有 `relay::failure` 识别为永久错误，才替换原始失败。EOF、读取错误、普通拒绝或期限结束都保留原来的错误对象与类型。已知永久错误直接返回；主动停止、重载和其他传输不进入这段收尾接收。[永久错误分类](../../../src/client/relay.rs)

## 本地验证

使用独立工作区、精确子模块、独立 target、Rust 1.100.0、锁定的离线依赖与单任务编译。测试不访问真实 OAuth 账户，不启动模型，也不修改运行中的 relay。

| 检查 | 结果 |
|---|---|
| 新增真实 duplex/read_frame 回归 | 5 项通过，0 失败、0 忽略，2.01 秒 |
| 既有 relay 回归 | 5 项通过 |
| 既有客户端协议回归 | 10 项通过 |
| `cargo build --offline --locked --bin ssmcp` | 通过 |
| 完整 `scripts/e2e_account_relay.py` | 一次通过，47 项检查，13.74 秒 |
| `cargo fmt -- --check`、`git diff --check` | 通过 |
| `cargo clippy --offline --workspace --locked --all-targets -- -D warnings` | 退出码 0，191.969 秒 |

新增回归分别验证晚到的 AUTH/兼容性拒绝、已满帧队列、EOF/无效帧/普通拒绝保留原 `BrokenPipe`、已知永久错误不再等待，以及持续普通帧仍受一个总期限约束。测试明确注入原始 `BrokenPipe` 错误对象，终帧通过真实 duplex 和 `read_frame` 传送，从而固定错误与拒绝的先后次序。[确定性交错测试](../../../src/client/incoming_tests.rs)

完整账户夹具保留了原有的非零退出、连接计数不增加、重新授权次数和凭据删除检查，只补充退出码与前后计数诊断。夹具也验证普通断线恢复、访问撤销、启动期撤销和主动 Ctrl-C 清理。

全部验证完成后，受测工作区干净。客户端二进制 SHA-256 为 `880e94e8e660bbcfcc33302cbb2003a6f77c713dd723b0ed74a3fa16acbbb86f`。Cargo 自身另提示现有 `qrcodegen` 清单依赖未使用；它没有使 Rust/Clippy 门禁失败，本分支未更改依赖。

## 限制

原发布制品的单独本机基线未执行：原生制品下载超时后停止了这条可选路径。基线失败证据来自上述真实 CI；确定性交错回归和完整账户流程使用修改后的源码。没有通过放宽断言、增加夹具等待或重复追逐随机失败取得通过结果。最终发布验收仍须绑定整合后的冻结提交。
