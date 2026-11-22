# H2 原始日志读取兼容修正

本目录保留 v0.10.26 发布核验器的一处取数兼容修正：`api_bytes` 调用 `gh api` 时增加官方 CLI 帮助中列出的 `--allow-escape-sequences`。原始 stdout 仍由 Python 的 PIPE 捕获为字节，进入既有解析与哈希核对；原始日志不向终端渲染。

## 依据与范围

首次正式 H2 在读取作业 `137064905326` 的日志时因 `gh api` 退出 1 而失败，原报告保留。随后同一接口的一次读取返回 EOF，另一种传输设置下明确返回终端控制字符保护错误。使用默认环境并增加上述参数后，实际取得 **78,834 字节**，含 **1,412 个 ESC 字节**，退出 0 且 stderr 为空。该后继针对可观察的 CLI 输出限制；不将它当作首次 EOF 的确定原因。

[原始审阅包](https://github.com/LIghtJUNction/ssmcp/tree/432900420ef4fbe093ad284cb49e66e1e2892800/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-zuse-cli-v26-unbound) 保留原字节。主模块仅新增一个 argv 字符串；去除该字符串后，AST 与原模块相同。来源、派发回执、作业、日志窗口、artifact、公开文件和哈希判定保持原定义。另两个模块及六份配方清单逐字节相同。

## 可复核材料

- [单行差异](changes-from-reviewed-v26.patch)
- [字节身份和诊断范围](compatibility-identity.json)
- [主核验器](verify_published_manual_main.py)
- [作业与产物来源核验](manual_main_provenance.py)
- [原始派发回执解析](dispatch_receipt_adapter.py)
- [既有配方](reviewed-recipe.json)

本次输入保持源码 `1d6b55320fff6533663dad2627891960b3b5c5cc`、工作流 `c26b37fc3912cf1ed5e8d2a6143c5c857446f6ce` 和 Release [44975626721 attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/44975626721)。实际核验结果由后续回执记录，本说明不把准备状态记为通过。
