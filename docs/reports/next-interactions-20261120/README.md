# 下一轮交互组合：验收入口与证据边界

## 当前状态

功能源码基线为 [4a7907c523e6391b710635918355ef382cd62563](https://github.com/LIghtJUNction/ssmcp/commit/4a7907c523e6391b710635918355ef382cd62563)，分支为 `feat/next-interaction-ui-20261120`。它包含结束审批、问题答复、原生收件人、长粘贴文件，以及来源和目标连接的消息生命周期检查。其后的本次交接只调整验收断言、报告标签和本文，不改变产品源码。

**此组合尚未执行 Cargo 构建、Rust 测试、WebSocket 回归或 PTY 验收。** 已完成的检查仅为 standalone rustfmt、diff 检查和 Python 语法检查。此前 v0.10.14 或其他候选的通过记录不能代替这一组合的结果。下一次统一构建须记录最终组合 SHA、依赖 pin、全部 ELF 哈希和原始退出码。

## 固定输入与执行入口

使用同一冻结清单中的 `ssmcp`、`ssmcpd` 和 libtest；已有前端二进制也随产品清单冻结。不在 E2E 中调用 Cargo。以下变量分别指向冻结目录、该 libtest、组合 SHA 和全新的证据目录：`SSMCP_GATE_BIN`、`SSMCP_GATE_FIXTURE`、`SSMCP_GATE_SOURCE`、`SSMCP_GATE_OUT`。

独立 VT 解码器复用仓库内上一轮 listener 冻结文件：

`.worktrees/listener-integrate-20261120/.local/listener-integration-20261120/bin/vt_snapshot`

SHA-256：`70ba4cb707cb572e9b1754dc673c0d57c04973e59fde6e6e6001b1e610a207a8`。将它的绝对路径设为 `SSMCP_GATE_DECODER`；它只解析 ANSI，不是本轮被测产品。执行前后核对产品与解码器哈希。

```bash
export SSMCP_E2E_FIXTURE_BINARY="$SSMCP_GATE_FIXTURE"

python3 scripts/e2e_interactions.py \
  --bin-dir "$SSMCP_GATE_BIN" --fixture-binary "$SSMCP_GATE_FIXTURE" \
  --decoder "$SSMCP_GATE_DECODER" --source-revision "$SSMCP_GATE_SOURCE" \
  --out "$SSMCP_GATE_OUT/interactions"

python3 scripts/e2e_input_history.py \
  --bin-dir "$SSMCP_GATE_BIN" --decoder "$SSMCP_GATE_DECODER" \
  --out "$SSMCP_GATE_OUT/input-history"

python3 scripts/e2e_context_memory.py \
  --bin-dir "$SSMCP_GATE_BIN" --decoder "$SSMCP_GATE_DECODER" \
  --report "$SSMCP_GATE_OUT/context/report.json" --out "$SSMCP_GATE_OUT/context/frames"

python3 scripts/e2e_presentations.py \
  --bin-dir "$SSMCP_GATE_BIN" --decoder "$SSMCP_GATE_DECODER" \
  --out "$SSMCP_GATE_OUT/presentations"
```

按顺序执行，保留首个失败及修正后的回执；不并发扩大本机负载。构建负责人另跑严格 all-target Clippy、相应 Rust/真实 WS 定向测试及最终全库门禁，完整集合使用既定的两个测试线程。

## 下一次必须取得的证据

| 入口 | 需要实际验证的行为 |
| --- | --- |
| `e2e_interactions.py` | Shift+Up 答案保留主草稿，并在原提问者的下次绑定调用交付；真实 finish 等待压暗、点击仅唤醒、默认 No、显式 Yes、独立评论、其他已验证 agent 来源和未发送评论保留；Yes 对应一次最终答复，幂等重试复用回执。 |
| 同一 PTY 脚本的原生消息场景 | 无消息时收件人列表自动刷新；显式选择账号收件箱或精确 agent；同账号未绑定调用不能取走 agent 消息；queued、included、acknowledged 自动刷新，并核对精确 message/recipient ID。观察期间不注入按键或 UI 查询来促成重画。 |
| 同一 PTY 脚本的长粘贴场景 | 4096/4097 字节和 7/8 行边界；UTF-8、换行和缩进原样落盘；写入失败保留原文、草稿和收件人，显式重试恢复。 |
| `e2e_input_history.py`、`e2e_context_memory.py` | 原有附件、历史、复制、幂等和上下文来源/去重门禁。纯展示与上下文素材使用 progress；保留原行为断言。 |
| `e2e_presentations.py` | 保留 kind=answer 和真正结束语义；先通过绑定 finish 与实际 PTY 选择 Yes，再核对呈现、复制、历史、幂等及下一条需求只交付一次。 |

网络测试使用自有 loopback router、正常 OAuth/账号/WS 实现和合成上游身份，不使用用户的真实 GitHub 凭据或现服。所有 HOME、配置、会话、临时文件和 PTY 均隔离；不启动模型或 X server。

消息回执只说明队列接收、放入工具回复、收件方显式确认，**不说明消息已经执行或已被理解**。结束批准也只对应用内对应的最终答复门禁有效。保存真实 PTY 的 120/48 列 VT 数据，再以 `scripts/render_ui_snapshot.py` 渲染 PNG 做人工核验；它们与纯 Frame fixture 截图分别注明来源。报告只保留脱敏证据，不提交 token 或私有日志。
