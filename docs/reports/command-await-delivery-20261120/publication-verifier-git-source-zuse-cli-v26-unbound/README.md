# H2：v0.10.26 独立 Zuse CLI 发布的静态后继

本目录准备 **0.10.26 / manual-main / git-source-v1** 发行核验器。六份配方直接读取自固定源码 [4c6cd21](https://github.com/LIghtJUNction/ssmcp/commit/4c6cd21b2ab46c11febbac7c1921e302da49f4b2)，适用于 GitHub 二进制发行、crate 延期和服务器部署延期的组合。

**本包尚未执行，运行核验使用的 S、P、tag、run 和原始 dispatch 回执均未绑定。** 已审配方来源与实际运行参数分别记录；静态源码、字节和语法审阅不能代替正式发布核验。本目录不会触发发布、创建标签、安装程序或部署服务器。

## 与 v0.10.25 静态包的必要差异

[原 v0.10.25 H2 CLI 包](https://github.com/LIghtJUNction/ssmcp/tree/840629d64e3915bbe2b17496aa600d067e34b7e3/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-unbound)保留原字节。v0.10.25 的正式 CI 失败和原生返回保存缺口保留在[阶段报告](../current-iteration-Release-CI-followup.md)，不转记为 v0.10.26 的成绩。

| 变化 | 当前固定来源 | 保留的约束 |
|---|---|---|
| 两份 Python 模块的版本绑定改为 0.10.26，说明和错误文字同步 | 固定新版本的 manifest、Cargo 根包、标签和 validate 输出 | schema-2、git-source-v1、crate deferred 和原始运行身份规则 |
| Release 工作流与 release.py 显式增加 0.10.26 的延期策略 | 同时保留 0.10.25；没有把后续任意版本一律延期 | GitHub-only、相同延期 reason、deploy_server=false；没有增加或移除 job、step 或 artifact |
| 更新已审来源和两份变化配方的大小／SHA-256 | 其余四份配方逐字节相同；六份 Git blob 身份均实际核对 | 最终 S 是 P 的祖先，S/P 六配方均须等于独立审阅字节 |

固定源码的完整递归 tree 已实际读回，未截断，包含 zharness、zplugin、ztuio 三个 gitlink，提交身份与原包一致。三个模块只做 AST 语法和静态差分审阅，没有 import 或执行。

原 dispatch 解析器逐字节不变。新的版本绑定不接受手工拼装的原生回执，也不会把后续 snapshot 当成丢失的原始返回。运行参数与回执仍由原执行者从真实观察取得。

[固定 Release](https://github.com/LIghtJUNction/ssmcp/blob/4c6cd21b2ab46c11febbac7c1921e302da49f4b2/.github/workflows/release.yml) · [固定 CI](https://github.com/LIghtJUNction/ssmcp/blob/4c6cd21b2ab46c11febbac7c1921e302da49f4b2/.github/workflows/ci.yml) · [固定 release.py](https://github.com/LIghtJUNction/ssmcp/blob/4c6cd21b2ab46c11febbac7c1921e302da49f4b2/scripts/release.py)

原结构约束继续保留：Build 打包／安装器／唯一上传分别是 API step 6／7／8；finalize 的两次上传仍是 step 10／11。Linux 的 `Preserve the Linux candidate client` 仍为 API step 20，Release 调用时须 skipped；两份 Linux job 的上传数仍为 2／1。该辅助候选不作为正式发布证据。十九个 job、十六个公开文件、八份必要发布 artifact 和 Windows CRLF sidecar 核验规则保持不变。

## 适用范围

| 项目 | 必须满足的合同 |
|---|---|
| 产品与版本 | `ssmcp`，`0.10.26`，正式标签 `v0.10.26` |
| 触发方式 | 对 `main` 执行 `workflow_dispatch`，显式 `build_only=false`、`deploy_server=false` |
| 来源身份 | S 必须是真实 Git 提交；P 是实际执行工作流的 main 提交；S 是 P 的真实祖先，六份发布配方在 S/P 上等于已审阅字节 |
| 发布模式 | `git-source-v1`；crate 状态与固定 reason 必须为 `deferred`，`verified_crate` 必须为 null |
| 作业 | 19 个预期 job；17 个成功，`publish_crates` 与 `deploy_server` 跳过 |
| 公开附件 | 六个平台的 archive、六份 sidecar、两个安装器、SHA256SUMS、release-manifest.json，共 16 个 |
| 发布证据附件 | 六份构建 artifact、`verified-source`、`finalized-release-assets`，共八份必要附件；无关 CI 测试证据可另外存在 |
| 源码证据 | 提交、tree、manifest、Cargo.lock、三个 gitlink、Git 依赖图与工具链记录相互核对 |
| 完成条件 | 原始请求、实际 run/attempt、初始 checkout、具体 step、artifact ID、外层 ZIP 与内部文件、公开下载字节全部对应 |

本核验器不支持 `legacy-tag-v1`、build-only 或 registry 发布模式。独立 registry 发布仍由产品发布脚本的另一条严格路径处理；将来核验通过也不证明 crates.io 可安装、服务器已部署或用户机器已升级。

## 文件对应关系

- `verify_published_manual_main.py`：CLI、原始 dispatch 与运行身份、固定配方、候选源码和依赖图、公开附件及总结果。
- `manual_main_provenance.py`：作业日志与 step、初始 checkout、upload/download artifact 身份及字节核对。
- `dispatch_receipt_adapter.py`：只解析原始请求捕获与独立原生执行回执；保持旧版本字节不变。
- `reviewed-recipe.json`：实际已审阅源码及六份配方的大小和 SHA256。
- `preparation-identity.json`：模块、变更文件及静态审阅范围；所有未来运行绑定保持空值。
- `changes-from-H2-CLI-v25.patch`：相对固定 840629 原包的两个模块、说明和配方差异；身份与静态核对结果另列。

[原 H 保留在固定提交 2257fb1](https://github.com/LIghtJUNction/ssmcp/tree/2257fb1d235bba59e8a0d1747a4f4afc6348bad3/docs/reports/command-await-delivery-20261120/publication-verifier-manual-main-unbound)。本静态后继保留原 H2 和原 H，没有改写任何历史执行成绩。

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

