# v0.10.27 发布来源核验

这组脚本只读取既有发布，对指定的源码、工作流运行、原始派发回执和公开产物进行核对。它不创建标签、不派发工作流、不发布版本，也不执行安装。

2026-11-22 09:18:31–09:21:43 UTC 的实际运行已通过。固定来源如下：

- 源码 S 与工作流源码 P：`61a4de1cf522358691998ecb724b4a8518b91c47`
- 工作流：[45002392978，attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/45002392978)
- 正式发布：[v0.10.27](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.27)
- 完整原始核验报告：427915 B，SHA-256 `98abea32d57e75ac6a617a0c1094ef22f728ff6d753bfc0db3d4fb6536f349c5`
- [公开核验摘要](../publication-verification-summary.json)：19484 B，SHA-256 `5b2c04d3e7be3c0f7a8d8115a639df5d624346e9ced5fd32f94521899087fa00`

核验取得全部 16 个公开文件的真实字节，将它们与 8 个必要 Actions 产物的外层 ZIP、内部文件和构建日志分别对应；17 个成功任务的最初 checkout 与步骤窗口全部核对。该次运行共有 19 个任务，Crates 发布和生产服务器部署两项 skipped。全部 Actions 产物共有 15 个，核验所要求的发布产物是其中明确限定的 8 个。

Crates 注册表仍为 deferred，未声称验证注册表可用性。该核验依赖已审工作流、固定 action 和 GitHub runner；它不提供对恶意构建器的独立证明。

## 文件职责

| 文件 | 职责 |
| --- | --- |
| `verify_published_manual_main.py` | 固定配方摘要，绑定源码、原始派发证据、运行身份、清单和全部公开资产 |
| `manual_main_provenance.py` | 核对 job 原始日志中的 checkout、步骤、上传/下载及资产字节关系 |
| `dispatch_receipt_adapter.py` | 从独立读取的原始 native 回执与 HTTP 捕获核对真实派发；拒绝仅有自填状态的替代证据 |
| `gh` | 可选的只读网络适配层；首请求不变，仅在 EOF 后有限重试 |
| `version-adaptation-review.json`、`manual-provenance-v27.patch` | 记录 v27 版本常量与明确不部署策略的精确适配 |

v27 的服务器策略严格要求 `deployment_deferred=null`，因为固定工作流没有该版本的历史延期项，而原始输入明确是 `deploy_server=false`。完整字典相等检查继续执行。Crates 延期常量不受此调整影响。

## 运行输入

调用者需要完整、独立的源码 checkout，包括三个固定 gitlink 对象，以及在原始派发时独立保存的 HTTP 捕获与完整 native 执行回执。路径和原始内容属于本机证据，未随公开文档上传。必须从真实记录取得这些输入及其绑定，不能根据预期结果重写一份 JSON。

示例中的变量指向这些既有证据：

```sh
python3 verify_published_manual_main.py \
  61a4de1cf522358691998ecb724b4a8518b91c47 45002392978 \
  --workflow-sha 61a4de1cf522358691998ecb724b4a8518b91c47 \
  --checkout "$SSMCP_SOURCE_CHECKOUT" \
  --report-dir "$SSMCP_REPORT_DIR" \
  --dispatch-receipt "$SSMCP_DISPATCH_CAPTURE" \
  --dispatch-receipt-sha256 90bd3783c040682f427580d3275c18ec94d6f2d0925a50c9eff51f8a54623d7c \
  --native-execution-receipt "$SSMCP_NATIVE_RECEIPT" \
  --native-execution-receipt-sha256 4666e972ffbfaa753d9915ad5a56290288c4d39195d5ac06bf9fe2cfe3c9da06 \
  --native-execution-job-id "$SSMCP_ORIGINAL_NATIVE_JOB_ID" \
  --native-capture-path "$SSMCP_ORIGINAL_CAPTURE_PATH"
```

默认使用 PATH 中的 GitHub CLI。实际通过的运行使用原 gh 2.105.0，并将同目录中的可选适配器仅前置到该次子进程的 PATH；环境变量 `SSMCP_H2_REAL_GH` 指向独立原始 gh 的绝对路径，`SSMCP_H2_HTTP_READ_LOG` 指向本次私有 JSONL 审计文件。没有修改系统 PATH。

该适配器只接受指定参数形状的 `gh api` 默认 GET。其他调用直接交给原 gh。每个读取最多三次，共享 55 秒预算；只有非零 EOF 才会重试，第二、三次保留原资源路径及全部查询参数并追加 nonce。成功响应原字节返回，超时明确失败，原请求与实际请求均记入审计。实际通过的一轮共记录 35 次尝试、33 次成功读取、2 次 EOF 重试、0 次超时。

网络适配器为 4243 B，SHA-256 `858e79381f0488178dd3d72e2e45ebaaaf2fb6b1353f9537b668ffd4cfd2c9a3`。运行使用的原始 gh 摘要为 `fa7cff06d4db9eee98e00b10ac93beebfe1822109ac04a6cbbf9d806c21d0073`。它没有改变上述三个核验模块的任何校验分支。

