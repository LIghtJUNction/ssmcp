# ssmcp 调用者保留修复验收

日期：2026-11-18（Asia/Taipei）。状态：源码修复与本机回归通过。

本轮修复 Agents 列表在高频调用和断线重连后遗漏调用者的问题。代码提交为 [`e26c576`](https://github.com/LIghtJUNction/ssmcp/commit/e26c57660221d14669d7a0e482006e9091cad553)，基线为 `3570af1507031e83eede4abeb0853e609f039f46`。

## 修复后的行为

- 同一调用者连续完成 32 次调用后，其他仍有审计记录的调用者继续显示，原有选择键和精确 run 审计入口保持可用。
- 断线留下的 32 条未完成调用不会遮住新连接已经完成的调用；旧调用保持未知状态，新调用正确显示为完成。
- 调用者的最后一条审计记录被正常淘汰后，该调用者才从列表消失。审计仍采用现有 128 条、2 MiB 上限。

## 实现

`AuditView::observed_controllers()` 原先先把原始调用截为 32 条，再交给 `CallerTracker` 归组。一个调用者即可占满窗口，使其他仍在审计存储中的调用者提前消失。现在该入口读取既有 `MAX_ENTRIES` 保留窗口，再复用原有归组逻辑；未增加持久存储或依赖。

生产改动及两项回归测试均位于 [`src/client/audit_view.rs`](https://github.com/LIghtJUNction/ssmcp/blob/e26c57660221d14669d7a0e482006e9091cad553/src/client/audit_view.rs)。

## 验证

| 检查 | 结果 | 覆盖范围 |
| --- | --- | --- |
| 审计视图测试 | 6 passed，0 failed | 新增两项回归、Unicode 边界、历史容量、精确 run 选择与调用状态。 |
| 调用者相关测试 | 13 passed，0 failed | 调用归组、稳定选择键、断线状态、鼠标命中身份及相关调用者契约。 |
| Rust 格式检查 | 通过 | 修改后的 `audit_view.rs`。 |

执行命令：

```sh
cargo test --offline --locked --lib client::audit_view::tests -j 4
cargo test --offline --locked --lib caller -j 4
rustfmt --edition 2024 --check src/client/audit_view.rs
```

两项新增用例都能在基线上触发“应有两个调用者、实际只剩一个”的断言，修复后通过。两个测试筛选范围有交集，表中分别记录各次运行结果。

## 结论与证据对应

| 结论 | 对应回归证据 |
| --- | --- |
| 高频调用保留其他调用者及其稳定选择键 | `retained_callers_survive_repeated_calls_until_history_eviction` |
| 保持原有历史容量和真实淘汰行为 | 同一测试继续写入至 `MAX_ENTRIES`，验证旧调用者在最后记录淘汰后消失。 |
| 旧连接遗留调用不遮蔽新调用者 | `stale_running_calls_do_not_hide_a_completed_caller_after_reconnect` |
| 保持既有调用者选择和审计行为 | 审计视图与调用者相关测试集全部通过。 |

原始本机日志位于 `out/retained-callers-20261118/`：`audit-regressions.log`、`caller-regressions.log`、`format.log` 和 `validation.json`。
