# ssmcp 0.6.3 更新说明

日期：2026-11-17（Asia/Taipei）。状态：发布候选；本页列明计划纳入的修正，正式 CI、发布产物、安装和部署结果以实际回执为准。

0.6.3 继承 [0.6.2 的运行时、浏览器和工具发现改进](release-notes-0.6.2.md)，同步两处 E2E 夹具的预期。生产 Rust `src/` 的行为不因这两处测试修正而改变。

## 修正严格目录预期中的遗漏

`v0.6.2` 的[正式 Release 运行 44021562188](https://github.com/LIghtJUNction/ssmcp/actions/runs/44021562188) 在两个 Linux job 中均因 `scripts/e2e.py:700` 的严格目录断言失败：`BUILTIN_TOOLS` 缺少已经注册的 `ssmcp.browser.discover`。修正把该工具补入预期集合，保留完整目录匹配、分页和原生 Schema 校验。分页逻辑原本已存在，本次失败不是分页缺陷。[E2E fixture](../scripts/e2e.py)

`e2e_agents.py` 中的 provider 预期也同步适配可选 Playwright。原四类 Harness 的数量、Schema 和审批校验继续保留；缺席 fixture 另外要求 Playwright 为 unavailable、工具数为 0 且没有注册工具。该问题是在失败后的检查中发现，0.6.2 的失败流程尚未执行到它。[Harness fixture](../scripts/e2e_agents.py)

## 保留功能和权限边界

Node 成功探测缓存与非阻塞准备、跨标签页 CDP 并发、只读 `ssmcp.browser.discover`、原生 Playwright MCP provider、`tools.search` 和 Playwright 元数据审计均沿用 0.6.2 的实现。对外仍只有 `list` 与 `code_mode`，原生 Schema、结果和批准机制保持原有契约。[0.6.2 更新说明](release-notes-0.6.2.md)

实际用户 Chrome 的扩展连接仍未完成。已安装扩展的清单、隔离 Chromium 的 E2E 或协议 fixture 不能替代现有登录会话的真实握手和原生批准；发现阶段继续不自动下载依赖或调用 Harness 模型。[现有 Chrome 流程](browser-existing-session.md)

## 验证和历史记录

0.6.2 正式运行的 6 个 build、4 个 Windows/macOS checks 和 validate 成功；两个 Linux library 均为 469 passed / 0 failed / 7 ignored，Node 隔离专项也通过，旧 Node 探测超时未在该次运行中重现。两个 Linux checks 因上述目录 fixture 失败，finalize 与两个 publish 被跳过。[正式运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/44021562188)

`v0.6.2` 固定在 `b80d76143eda8a0f025a887260b065728495f5a9`，不覆盖既有标签或失败记录。0.6.3 使用新的版本和实际验证结果；在最终 CI、发布和部署回执产生前，不宣布新版本已发布或已安装。
