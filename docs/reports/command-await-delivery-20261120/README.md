# SSMCP 验证报告索引

产品来源固定为 [1d6b5532](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc)。本目录收录文档负责人在 **2026-11-22 02:11:34 UTC** 固定的报告快照，原始文档提交为 [16fd69f842d0a16c614ccd2268adffbefb89a66b](https://github.com/LIghtJUNction/ssmcp/commit/16fd69f842d0a16c614ccd2268adffbefb89a66b)。下列矩阵和结构化事实保留该提交的原文档字节；本页是新增的阅读入口。文档整合本身不产生新的构建、测试或发布结果。

## 阅读顺序

| 资料 | 内容 |
| --- | --- |
| [场景与源码版本矩阵](current-iteration-unified-source-and-cli-matrix.md) | 按实际源码版本区分通过、失败、环境阻塞与待执行项 |
| [结构化事实与回执身份](unified-source-and-cli-matrix-public-facts.json) | 固定来源、运行范围、计数及原始回执身份 |
| [完整历史报告与检查点](https://github.com/LIghtJUNction/ssmcp/blob/16fd69f842d0a16c614ccd2268adffbefb89a66b/docs/reports/command-await-delivery-20261120/README.md) | 文档负责人保留的完整历史段落及其原位置链接 |

每项结果属于矩阵注明的源码与运行记录。CI、正式 Release、H2 核验和服务器升级分别以各自回执为依据；报告整合不会把旧版本成绩转记为新版本成绩，也不会把准备状态改写为完成。

## 来源身份

| 对象 | 固定身份 |
| --- | --- |
| 产品源码 S | `1d6b55320fff6533663dad2627891960b3b5c5cc` |
| 产品源码树 | `fc28406bb20a3d1f6f9441ca34016357ab0ff39c` |
| 场景矩阵原 Git blob | `8abc487d1f0f69191dccece54a12160a6e871c5e` |
| 结构化事实原 Git blob | `3c6c4ae408c45cd5a42e01fcf35de3873fd2c5ea` |
| 完整历史 README 原 Git blob | `b8c663b700490f2c1de71287b81ab9b7720fd426` |

[0.10.26 发布说明](https://github.com/LIghtJUNction/ssmcp/blob/1d6b55320fff6533663dad2627891960b3b5c5cc/docs/release-notes-0.10.26.md) 保留在产品源码中。完整历史 README 保留在上表对应的文档提交，本目录不裁剪或改写其历史尾部。

## H2 历史准备资料

| 固定目录 | 范围 |
| --- | --- |
| [v0.10.26 H2 静态准备包](https://github.com/LIghtJUNction/ssmcp/tree/432900420ef4fbe093ad284cb49e66e1e2892800/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-v26-unbound) | 保留该提交时的未绑定准备状态 |
| [v0.10.25 H2 原准备包](https://github.com/LIghtJUNction/ssmcp/tree/840629d64e3915bbe2b17496aa600d067e34b7e3/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-unbound) | 保留旧版本的来源与说明 |

这些链接指向规范的历史目录，本次三文件整合不附带 H2 执行代码。历史准备资料本身不是执行证明；实际发布核验结果以矩阵和事实 JSON 中的独立来源、运行与回执为准。
