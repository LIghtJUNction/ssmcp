# H2：适配独立 Zuse CLI 迁移的静态后继

本目录是 **0.10.25 / manual-main / git-source-v1** 发行核验器的静态准备版本。已审配方来自固定源码 35ff6e4eb8b463aeb090230e9557fb08fb22b458，只适用于 GitHub 二进制发行与 crate 延期的组合。

**它尚未执行，也尚未绑定最终产品提交 S、main 工作流提交 P、tag、运行编号或原始 dispatch 回执。** 这里的配方来源不是已选定的最终候选。静态语法与源码审阅不能代替真实发布核验；本目录不会触发发布、创建标签、安装程序或部署服务器。

## 与旧配方对应的必要变化

[前一份 H2 CI 固定版本](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)保留原样。以下变化来自已退役的内置电脑／浏览器执行器及新的 CLI 安装管理；它们不转移任何旧候选的运行成绩。

| 旧规则 | 当前固定配方 | 保留的约束 |
|---|---|---|
| 四个 gitlink，直接依赖包含 zuse-computer | 三个 gitlink：zharness、zplugin、ztuio；直接 Project Z 分类与 release.py 一致 | receipt 路径和 commit 集合必须等于最终 S 的真实递归 Git tree，每个子模块 tree 继续核对 |
| Build 主上传位于 step 12；Windows 共三次上传 | 全部平台主上传为 step 8，唯一上传 occurrence 0、total 1 | 对应 action、job、时间窗口、artifact ID 与字节全部核对 |
| Windows browser/bootstrap 诊断 step 10/11 | 仅删除这两项已退役的 Build 检查 | finalize 自身 step 10/11 的 verified-source 与 finalized-release-assets 上传仍保留 |
| 六份旧配方大小与摘要 | 四份已变、两份逐字节未变；全部重新固定于已审源码 | 最终 S 是 P 的祖先，且六份配方在 S/P 均等于独立固定字节 |

[固定 Release](https://github.com/LIghtJUNction/ssmcp/blob/35ff6e4eb8b463aeb090230e9557fb08fb22b458/.github/workflows/release.yml) · [固定 CI](https://github.com/LIghtJUNction/ssmcp/blob/35ff6e4eb8b463aeb090230e9557fb08fb22b458/.github/workflows/ci.yml) · [固定 release.py](https://github.com/LIghtJUNction/ssmcp/blob/35ff6e4eb8b463aeb090230e9557fb08fb22b458/scripts/release.py)

独立 CI 调试客户端的既有规则继续保留：Preserve the Linux candidate client 仍是 API step 20；Release 调用两个 Linux job 时必须为 skipped。Linux x86／ARM 实际上传计数保持 2／1。它不参与正式发布证据。十九个 job、十六个公开文件、八份必要发布 artifact 及 Windows CRLF sidecar 核验均保留。

原 dispatch 解析器逐字节不变。三个模块只做 AST 语法解析和静态审阅，没有 import 或执行；实际 Actions 日志兼容性仍须在真实发布时确认。

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
| 源码证据 | 提交、tree、manifest、Cargo.lock、三个 gitlink、Git 依赖图与工具链记录相互核对 |
| 完成条件 | 原始请求、实际 run/attempt、初始 checkout、具体 step、artifact ID、外层 ZIP 与内部文件、公开下载字节全部对应 |

本核验器不支持 `legacy-tag-v1`、build-only 或 registry 发布模式。独立 registry 发布仍由产品发布脚本的另一条严格路径处理；将来核验通过也不证明 crates.io 可安装、服务器已部署或用户机器已升级。

## 文件对应关系

- `verify_published_manual_main.py`：CLI、原始 dispatch 与运行身份、固定配方、候选源码和依赖图、公开附件及总结果。
- `manual_main_provenance.py`：作业日志与 step、初始 checkout、upload/download artifact 身份及字节核对。
- `dispatch_receipt_adapter.py`：只解析原始请求捕获与独立原生执行回执；保持旧版本字节不变。
- `reviewed-recipe.json`：实际已审阅源码及六份配方的大小和 SHA256。
- `preparation-identity.json`：模块、变更文件及静态审阅范围；所有未来运行绑定保持空值。
- `changes-from-H2-CI.patch`：与前一份 H2 CI 的两个模块、说明和配方差异；身份与静态核对结果另列。

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

