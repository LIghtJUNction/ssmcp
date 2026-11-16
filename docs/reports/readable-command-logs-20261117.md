# ssmcp 命令日志可读性修复

日期：2026-11-17（Asia/Taipei）

修复已通过 [PR #9](https://github.com/LIghtJUNction/ssmcp/pull/9) 合入 main，合并提交为 [4a5e807](https://github.com/LIghtJUNction/ssmcp/commit/4a5e807d0ee7ab18354a20c68634dd67e81c21dd)。开发机已安装包含修复的 Release 客户端，程序版本标识仍为 `ssmcp 0.6.1`，本轮没有创建新的 crates.io 版本。

## 用户可见变化

1. 多行 Python、Node、shell 等内联脚本显示带行号的源码。默认完整显示 12 行以内的脚本；长脚本显示前 8 行和末 4 行，并写明省略的行号范围。
2. 命令摘要包含实际源码摘录，例如 `source = Path('Cargo.toml')`。开始、实时输出、退出结果使用对应的任务编号；结果行同时显示命令、退出码和耗时。
3. 外层 Codex 工具调用与内层原生命令通过真实调用 ID 配对，默认只显示一套命令生命周期。并行命令乱序完成、请求编号与进程编号不同，均有回归覆盖。
4. 派发失败和未收到原生响应时保留失败提示；没有退出码时显示 `exit unknown`。现有脱敏流程继续应用于源码展示，原生请求和工具返回值保持原有语义。
5. `--max-inline-output N` 调整源码预览行数，`--verbose` 展开脱敏后的完整源码与审计元数据。源码按终端宽度换行，续行保留源码列。

## 验证结果

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 |
| 日志渲染器回归 | 48 项通过 |
| 原生 RPC 关联回归 | 2 项通过 |
| `cargo test --locked --lib -- --test-threads=4` | 410 通过、0 失败、7 忽略 |
| `cargo build --release --locked --bin ssmcp` | 通过 |
| 已安装客户端的 `--version`、`--help` 与文件一致性检查 | 通过 |
| 80 列与 100 列实际渲染截图 | 已查看，源码、行号、折叠提示和结果可读 |

默认高并发全库运行时，OpenCode 的关闭连接测试出现过一次超时；该测试单独复核及 4 线程全库运行均通过。上述结果来自 Linux 开发机，本轮未触发仓库的手动跨平台 CI。

## 效果图

- [80 列截图](https://github.com/LIghtJUNction/ssmcp/blob/1e14d8d9f8c7964a293ee47e55103ee1a3d0a2ea/docs/assets/readable-logs-80.png)
- [100 列截图](https://github.com/LIghtJUNction/ssmcp/blob/1e14d8d9f8c7964a293ee47e55103ee1a3d0a2ea/docs/assets/readable-logs-100.png)

截图由实际渲染器回放固定事件生成，覆盖 7 行读取脚本、83 行更新脚本、两条并行命令和一次编译失败。测试只渲染这些事件，图中的示例命令没有执行。

## 本机应用

开发机的 `/home/lightjunction/.cargo/bin/ssmcp` 已原子替换为通过验证的 Release 构建。已有进程继续运行原先加载的代码；重新启动对应的 ssmcp 连接后，会使用新的日志界面。

## 代码位置

- [命令与脚本展示](https://github.com/LIghtJUNction/ssmcp/blob/4a5e807d0ee7ab18354a20c68634dd67e81c21dd/src/client/command.rs)
- [日志与任务关联](https://github.com/LIghtJUNction/ssmcp/blob/4a5e807d0ee7ab18354a20c68634dd67e81c21dd/src/client/render.rs)
- [原生 RPC 关联](https://github.com/LIghtJUNction/ssmcp/blob/4a5e807d0ee7ab18354a20c68634dd67e81c21dd/src/backend/rpc.rs)
- [可读日志文档](https://github.com/LIghtJUNction/ssmcp/blob/4a5e807d0ee7ab18354a20c68634dd67e81c21dd/docs/logging.md)
