# zplugin / zsys 消费者源码候选

本分支准备了共享动态库加载器的消费者接入，状态为 **source-only，等待正式版本**。它固定 zplugin 的新源码并用 Cargo 真实解析 zsys 依赖；此记录不代表 zplugin 0.3.1 已发布，也不代表该候选已经通过 SSMCP 的 Rust 集成验收。

## 固定来源

| 项目 | 本候选使用的来源 | 版本 |
|---|---|---|
| SSMCP 基线 | `b43cf1f38bff28b04947fc82178bd7c26cbeb627` | 保持应用版本 |
| zplugin | [a3b0ae802cb039c073194c1ddcf290f28bbbbea6](https://github.com/LIghtJUNction/zplugin/commit/a3b0ae802cb039c073194c1ddcf290f28bbbbea6) | 源码 manifest 仍为 0.3.0 |
| zharness | [84e370450f1d047d43119bf81caf73f742ba6b0a](https://github.com/LIghtJUNction/zharness/commit/84e370450f1d047d43119bf81caf73f742ba6b0a) | 0.5.0，精确依赖 zplugin =0.3.0 |
| zsys | crates.io，摘要见 [evidence.json](evidence.json) 和根 Cargo.lock | 0.1.1，zplugin 的传递依赖 |

根 manifest 继续使用 zplugin `=0.3.0`、`default-features = false` 和 `std/macros`；这与所固定源码的真实版本一致。没有在父仓库伪造 0.3.1，也没有修改 zharness 子模块的 manifest 来掩盖版本约束。

## 接口与生命周期核对

[上游变更](https://github.com/LIghtJUNction/zplugin/compare/3e758b5c21af1bad1cfaf10264b43fe15440e69c...a3b0ae802cb039c073194c1ddcf290f28bbbbea6)将系统动态库操作委托给 zsys。检查过的 loader API、v1 ABI 定义和调用侧签名保持兼容；宏与 runtime 的相应调整为等价的条件表达式改写。上游 MSRV 提高到 Rust 1.95，低于 SSMCP 已声明的 Rust 1.100。

[zsys 0.1.1 的 loader](https://github.com/LIghtJUNction/zsys/blob/v0.1.1/src/dylib.rs)仍将句柄限制在当前线程；丢弃句柄不会自动卸载映像。显式关闭前仍要求对象、缓冲区、任务、线程、回调和函数指针不再使用该库。SSMCP 的动态扩展继续使用 `UnloadPolicy::Pinned`，内建插件继续由同一 local executor 持有 ABI 对象和任务。本次核对没有改变这些所有权边界。

## 已验证与未验证

真实 `cargo update --offline -p zplugin` 成功，只新增 zsys 0.1.1 及 zplugin 到它的依赖边。Cargo.lock 中的注册表摘要由 Cargo 生成；没有手填 checksum，也没有为 zsys 添加路径覆盖。解析命令仅对当前进程清空 `RUSTC_WRAPPER`，避免版本/目标信息探测等待机器上的全局编译串行锁；未修改全局配置，未运行 crate 编译。

现有 `python3 -B -m unittest discover -s scripts -p test_release.py` 的 70 项检查通过。对真实 manifest 调用 `registry_dependencies` 也通过，并保留 zharness 与 SSMCP 使用同一精确 zplugin 版本的门禁。这些检查证明当前源码依赖关系自洽；完整 Rust 构建、原生插件回归和正式注册表打包尚未验证此候选。

## 正式接入的剩余条件

[0.3.1 发布流水线 44621346286](https://github.com/LIghtJUNction/zplugin/actions/runs/44621346286)在 2026-11-19 21:44:09 UTC 以 failure 结束，publish job 被跳过。四个失败步骤均在独立编译 Wasm guest 时报告 `E0463: can't find crate for zplugin`；错误位置为 `examples/wasm/guest.rs:4:5`。详细 job ID 与步骤见 evidence.json。该失败记录不代表原生消费者 API 编译失败。

正式接入需要上游给出已发布且可解析的 zplugin 版本、对应不可变源码提交，以及精确依赖该版本的 zharness 来源。届时一起更新两个 gitlink、根 manifest 和真实 Cargo.lock，再运行合并候选的 Rust 与原生插件验收。当前源码中的 loader 已变化，但包版本仍为 0.3.0；因此在这些条件完成前，本分支不得作为已完成的 0.3.1 升级合入发布主线。
