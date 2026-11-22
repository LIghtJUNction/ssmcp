# SSMCP 0.10.26 验证报告

**v0.10.26 已正式发布，服务器已升级到 0.10.26。** 产品来源固定为 [1d6b5532](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc)，报告快照固定为 [4dd1314128d0bf95c788568c08cfc19e2fdd29e8](https://github.com/LIghtJUNction/ssmcp/commit/4dd1314128d0bf95c788568c08cfc19e2fdd29e8)。本目录保留该快照的矩阵和事实 JSON 原字节；本页提供精简阅读入口。

## 实际结果

- [六平台 CI](https://github.com/LIghtJUNction/ssmcp/actions/runs/44973215329)：attempt 1，6 个作业全部成功。
- [正式发行](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.26)：16 个公开资产。[发行工作流](https://github.com/LIghtJUNction/ssmcp/actions/runs/44975626721) attempt 1 共 19 个作业，17 个成功，registry 发布和工作流内服务器部署按策略跳过。
- H2 发行完整性核验于 2026-11-22 02:49:35 UTC 通过，覆盖来源、原始派发、作业、artifact 与公开资产字节；此前两次读取失败保留各自原始回执。
- 已授权的独立服务器升级于 02:52:25 UTC 完成，操作 `44975626721-1` 最终为 `deployed`。实际运行版本及文件哈希与发行目标一致。
- 升级后原保存凭据认证成功，同一插件连接的客户端查询和只读调用恢复；开发机客户端仍为原来的 0.10.25。

## 阅读顺序

| 资料 | 内容 |
| --- | --- |
| [场景与源码矩阵](current-iteration-unified-source-and-cli-matrix.md) | 功能行为、实际验证、历史失败与范围说明 |
| [结构化事实与回执身份](unified-source-and-cli-matrix-public-facts.json) | 固定来源、运行计数、完成状态及原始文件摘要 |
| [完整历史报告](https://github.com/LIghtJUNction/ssmcp/blob/4dd1314128d0bf95c788568c08cfc19e2fdd29e8/docs/reports/command-await-delivery-20261120/README.md) | 报告分支保留的完整历史正文 |
| [本版发布说明](https://github.com/LIghtJUNction/ssmcp/blob/1d6b55320fff6533663dad2627891960b3b5c5cc/docs/release-notes-0.10.26.md) | 产品功能与使用方式 |

本机最终源码验证包括 2,079 项常规库测试、另外 2 项真实系统隔离检查和 9 项 two_tools。11 项未运行的 ignored 测试没有计作通过。消息引用 53 项检查覆盖缺失文件提示后仍发送、技能选择和单次送达；基础 @/$ 功能已存在，报告区分既有行为和本轮改进。

OAuth CLI 的 19 项检查、7 次调用覆盖 `client_registrations_only`；包含真实 access/refresh token 的完整导出、导入链路尚未取得 CLI 端到端验证证据。本次原地升级后原凭据可用是单独的实际验证。服务重启造成连接及参与者 ID 变化，不宣称零中断或原全部客户端连续在线。

## 固定来源

| 对象 | 身份 |
| --- | --- |
| 产品源码 | `1d6b55320fff6533663dad2627891960b3b5c5cc` |
| 产品源码树 | `fc28406bb20a3d1f6f9441ca34016357ab0ff39c` |
| PR 180 合并及发行工作流来源 | `c26b37fc3912cf1ed5e8d2a6143c5c857446f6ce` |
| 发布后会话文档修正 | `03f667ab0ea03e2e3f5bc2dde65bc241b5121df9` |
| 最终矩阵 Git blob | `3d6fb6ffbd7698b07c3b9c80a6137152edb991da` |
| 最终事实 JSON Git blob | `98f1634b0de82d3c3ea327073026ef52e86a5fd8` |
| 历史 README Git blob | `cb638d36796e957efdb9efe145639a27ea34988d` |

[PR 180](https://github.com/LIghtJUNction/ssmcp/pull/180) 保留源码整合过程。后续文档提交不改变已发布产品字节，不产生新的构建或测试成绩。

## 核验程序与历史资料

| 固定资料 | 用途 |
| --- | --- |
| [本次成功 H2 使用的响应采集兼容包](https://github.com/LIghtJUNction/ssmcp/tree/ab08dac0e18ced96445de8799839f83375174ddb/docs/reports/command-await-delivery-20261120/publication-verifier-gh-log-compat-20261122) | 仅增加原始响应采集参数，保留全部完整性断言 |
| [v0.10.26 原准备包](https://github.com/LIghtJUNction/ssmcp/tree/432900420ef4fbe093ad284cb49e66e1e2892800/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-v26-unbound) | 保留当时尚未绑定发行运行的状态 |
| [v0.10.25 原准备包](https://github.com/LIghtJUNction/ssmcp/tree/840629d64e3915bbe2b17496aa600d067e34b7e3/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-unbound) | 保留旧版本的来源与说明 |

成功 H2 的回执为 399,189 B，SHA-256 `4da9ac1e13589786b76308e61f3a5c41b0a2b2a3a5452bc4a3f1cfa4c9a78a14`。本次仅整合矩阵、事实 JSON 和此短索引，完整历史与核验代码保留在上面的固定提交。
