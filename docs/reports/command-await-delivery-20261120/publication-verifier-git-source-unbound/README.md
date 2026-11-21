# H2：Git 来源发行的 manual-main 核验器

本目录是 **0.10.25 / manual-main / git-source-v1** 发行核验器的静态准备版本。它以固定源码 `879ba17fc3ba4bad6a26a85494b79e29acf1938a` 的发布合同为依据，专门处理本轮 GitHub 二进制发行与 crate 延期的组合。

**它尚未执行，也尚未绑定最终产品提交 S、main 工作流提交 P、tag、运行编号或原始 dispatch 回执。** AST 语法解析和源码审阅不能替代真实发布核验。本目录不会触发发布、创建标签、安装程序或部署服务器。

## 适用范围

| 项目 | 必须满足的合同 |
|---|---|
| 产品与版本 | `ssmcp`，`0.10.25`，正式标签 `v0.10.25` |
| 触发方式 | 对 `main` 执行 `workflow_dispatch`，显式 `build_only=false`、`deploy_server=false` |
| 来源身份 | S 必须是真实 Git 提交；P 是实际执行工作流的 main 提交；S 是 P 的真实祖先，六份发布配方在 S/P 上等于已审阅字节 |
| 发布模式 | `git-source-v1`；crate 状态与固定 reason 必须为 `deferred`，`verified_crate` 必须为 null |
| 作业 | 19 个预期 job；17 个成功，`publish_crates` 与 `deploy_server` 跳过 |
| 公开附件 | 六个平台的 archive、六份 sidecar、两个安装器、SHA256SUMS、release-manifest.json，共 16 个 |
| 发布证据附件 | 六份构建 artifact、`verified-source`、`finalized-release-assets`，共八份必要附件；无关 CI 测试证据可另外存在 |
| 源码证据 | 提交、tree、manifest、Cargo.lock、四个 gitlink、Git 依赖图与工具链记录相互核对 |
| 完成条件 | 原始请求、实际 run/attempt、初始 checkout、具体 step、artifact ID、外层 ZIP 与内部文件、公开下载字节全部对应 |

本核验器不支持 `legacy-tag-v1`、build-only 或 registry 发布模式。独立 registry 发布仍由产品发布脚本的另一条严格路径处理；本次通过也不会证明 crates.io 可安装、服务器已部署或用户机器已升级。

## 文件对应关系

- `verify_published_manual_main.py`：CLI、原始 dispatch 与运行身份、固定配方、候选源码和依赖图、公开附件及总结果。
- `manual_main_provenance.py`：作业日志与 step、初始 checkout、upload/download artifact 身份及字节核对。
- `dispatch_receipt_adapter.py`：只解析原始请求捕获与独立原生执行回执；保持旧版本字节不变。
- `reviewed-recipe.json`：实际已审阅源码及六份配方的大小和 SHA256。
- `preparation-identity.json`：模块、变更文件及静态审阅范围；所有未来运行绑定保持空值。
- `changes-from-H.patch`：与原静态版本 H 的逐文件差异。

[原 H 保留在固定提交 2257fb1](https://github.com/LIghtJUNction/ssmcp/tree/2257fb1d235bba59e8a0d1747a4f4afc6348bad3/docs/reports/command-await-delivery-20261120/publication-verifier-manual-main-unbound)。H2 的版本替换不更改任何历史执行成绩。

## 为什么保留两层附件核对

六份平台构建 artifact 证明流水线实际上传了哪些 archive 和 sidecar。`finalized-release-assets` 则保留完成源码验证后的整套 16 个文件，发布 job 必须下载这套文件并重算核验，公开 Release 下载的内容再与之逐字节比较。`verified-source` 保存源码、锁文件和依赖图的实际记录，并与公开 manifest 的相同字段对应。

SHA256 用于比较字节，不能单独证明某条命令执行过。因此还必须保留原始 dispatch 的 HTTP 输入输出、独立工具回执、run/attempt、job/step 及 artifact ID。成功的普通 CI 证据也不等同于对恶意构建器的硬件证明。

## 后续执行形式

下列内容是待原执行者填入真实观察值的命令格式，**当前没有执行**：

```sh
python3 verify_published_manual_main.py PRODUCT_S ACTUAL_RUN_ID \
  --workflow-sha WORKFLOW_P \
  --checkout /absolute/validated/checkout \
  --report-dir /absolute/owned/report \
  --dispatch-receipt /absolute/original/dispatch-capture.json \
  --dispatch-receipt-sha256 ACTUAL_CAPTURE_SHA256 \
  --native-execution-receipt /absolute/original/native-receipt.json \
  --native-execution-receipt-sha256 ACTUAL_NATIVE_RECEIPT_SHA256 \
  --native-execution-job-id ACTUAL_NATIVE_JOB_ID \
  --native-capture-path /absolute/original/native-dispatch-capture.json
```

需要 Python 3.11 或更新版本、可只读访问仓库与 Actions 的 `gh`、完整候选 Git 对象与所需子模块对象。三个模块应位于同一目录。不得使用 `-O` 或 `PYTHONOPTIMIZE` 关闭断言。

提供者必须从独立原始观察获取 capture/native receipt 的 SHA256、job ID 和 native capture path。临时编写的 JSON 及其自算摘要不能代替这些原件。任何配方变更均须重新审阅并更新固定值，不能从待验证流水线的输出自动学习可信值。

