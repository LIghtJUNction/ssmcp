# ztuio 独立仓库接入验收

SSMCP 已在源码候选 `631fc9f9078fd6173d28f9efb204d587e741ba56` 中改为消费独立的 `ztuio = 0.1.0`，子模块固定在 `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e`。本报告记录独立库、发布脚本与消费者打包的本地证据；最终 SSMCP 产品回归在包含 zuse 和休眠界面的组合候选上进行。

## 已验证结果

| 范围 | 命令或证据 | 结果 |
| --- | --- | --- |
| 独立库源码 | 干净 checkout 固定为 canonical SHA | 与子模块 pin 相同 |
| 格式 | `cargo +1.100.0 fmt --all -- --check` | 通过 |
| 库契约 | `cargo +1.100.0 test --locked --jobs 1` | 13 项通过 |
| 严格检查 | `cargo +1.100.0 clippy --locked --all-targets --jobs 1 -- -D warnings` | 通过 |
| 独立库打包 | `cargo +1.100.0 package --locked --no-verify` | 通过，产物与已发布 crate 的 SHA256 完全相同 |
| 发布脚本 | `python3 scripts/test_release.py` | 72 项通过 |
| 消费者格式 | `cargo fmt --package ssmcp -- --check` | 通过 |
| 消费者打包 | `cargo package --locked --no-verify --package ssmcp` | 通过，634 个文件，8,805,421 字节压缩包 |

13 项契约覆盖首帧精确 ANSI 序列、等内容不同分配地址不重绘、单行更新、仅光标和模式变化、窗口几何与分区边界、Unicode 与 SGR 透明传递、固定缓冲区错误传播、空帧不写入、填充边界以及所有 u16 坐标十进制位数边界。完整测试名称见 [core-contracts.log](core-contracts.log)。

本地产生的 `ztuio-0.1.0.crate` SHA256：

```text
1349d024486abe9aa5d7e6ff4f62b118401a1da5627c63c57942617169e475ca
```

该值与 crates.io 的 0.1.0 发布 checksum 一致。发布状态由主代理于 2026-11-19 21:50:02 UTC 独立查询 crates.io API 确认：版本存在、未被 yank。机器可读证据见 [core-validation.json](core-validation.json)。

## 改动及依据

旧的 `crates/ztui` 普通源码副本已从当前目录移除，其历史仍在 Git 中。新 `crates/ztuio` 是独立仓库子模块，明确排除在消费者 workspace 自动成员之外。消费者只更新 crate 名称、精确依赖版本、导入、锁文件、当前依赖文档和发布配置。

对比表 [source-equivalence.json](source-equivalence.json) 保留了旧树和 canonical 树的逐文件哈希：

- 四个运行时源文件中，三个逐字节相同；`src/row.rs` 仅有注释中的名称替换。
- 合同测试和 fixed-buffer 示例只变化命名空间或注释。公共类型、函数签名、ANSI 输出及错误传播代码保持一致。
- canonical benchmark 省略了旧树的 `unchanged_clone` 三行计时场景，并继续使用两个历史 `ztui_*` JSON 指标名。此处未修改上游，也未声称 benchmark 全部相同；“不同分配地址但内容相同不重绘”的合同测试仍存在并已通过。

发布流程由消费者只发布 SSMCP；ztuio 在其独立仓库发布。预检验证 checkout 的包名与 library 名、精确版本、registry 中已有的版本及 checksum 一致性。原有 zplugin/zharness 匹配约束继续保留。历史验收文档中的旧名称继续作为当时事实保存。

## 验证环境与边界

本地 Rust 工具链为 1.100.0，所有真实编译保持机器原有 rustc-serial wrapper 和 jobs=1。测试过程曾等待跨项目 Cargo 缓存锁；没有覆盖全局配置、跳过串行锁或触发日常 CI。

在主仓库内部的嵌套 worktree 中，`cargo fmt --all` 会沿独立依赖目录找到外层仓库的 workspace。消费者使用 package-scoped fmt；独立库在主仓库外的干净 checkout 验证，保持相同 commit 和绝对 target 缓存。这是验证目录选择，不修改 canonical 的 Cargo 配置。

消费者打包检查使用 631fc9f 时的 0.10.13 清单，只是依赖迁移预检，不代表正式版本已发布。最终 0.10.14 的版本、完整 ELF、终端回归、休眠交互和发布记录由组合候选的验收报告绑定。此报告不把先前 0.10.13 二进制的测试结果归到新的依赖树。

原始本地路径已在日志中替换为可读占位符。该目录仅保存可审查的结果、测试输出和哈希。
