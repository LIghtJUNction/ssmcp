# 0.9.0 会话交互整合记录

更新日期：2026-11-18（Asia/Taipei）。本记录分别标明已验收图片基线与正在统一验收的扩展源码。

## 已验收并安装的图片基线

源码：[`808703c7e3ff22520185083122ab8844d24fa84d`](https://github.com/LIghtJUNction/ssmcp/commit/808703c7e3ff22520185083122ab8844d24fa84d)。格式检查、严格 Clippy、814 项 Rust 测试通过；9 项按原有约定忽略。

| 真实验收 | 通过断言 |
| --- | ---: |
| 界面预览 | 42 |
| 输入框 | 88 |
| relay 输入交付 | 45 |
| 图片、剪贴板与重载 | 121 |
| 状态面板 | 84 |
| 合计 | 380 |

安装包 `ssmcp-git-0.9.0.r307.g808703c7e3ff-1-x86_64.pkg.tar.zst` 的 SHA256 为 `be28a277ff8d6dee340050ebb08d49df5d3d9e9a2c88d0e390bfa3b704903786`。

本机安装回执为 `installed_verified`，旧包、旧文件字节和原 Cargo 可执行文件 inode 均已保存。Cargo 入口与 mise shim 实际报告 0.9.0；当前任务会话已成功 reload，运行中的客户端 SHA256 为 `ee32a8d7c180f978797f61748d9e40602fc875204c67221b563fceefeb289406`。

服务器由独立后台流程更新，要求备份、安装、运行文件哈希及真实 PNG/v7/say 验收全部完成；最终部署回执另行补入。之前失败的备份批次与诊断保留，不据此宣称服务器部署完成。

## 最新源码整合

[`16a938f32dbd760b68095a5b2fcb3b737c60e7e6`](https://github.com/LIghtJUNction/ssmcp/commit/16a938f32dbd760b68095a5b2fcb3b737c60e7e6) 已合入 PR #25 的最新祖先与实际交互增量，同时保留 v8 图片交付和 v7 文字兼容。此前 `9f1596f`、`42d4e3d` 也已分批推送。

本次整合包含普通文件与长文本粘贴、Backspace 删除图片、/handoff、非阻塞问答、持久历史、原生 Intelligence UI、上下文压缩与独立分支记忆。新要求仍按队列及目标交付；共享自动记忆排除待交付和定向要求。

根据实际 ztty 宿主界面，成功工具事件改为默认单行，进度、答复及错误保持展开；侧栏按调用者分组展示近期调用数和运行数，保留每次调用的详细审计。

默认工具说明从 15,927 bytes 减为 3,388 bytes（减少 78.7%），完整指南由既有 `ssmcp://guide` 按需读取。完整原生 schema、helpers、别名和权限逻辑保留；此变化不减少首次原生目录扫描成本。

扩展源码尚需统一 Rust、真实终端与部署验收。上面的 814/380 仅属于图片基线，不代替扩展版验证。正式 release、最终部署与验收数据完成后更新本记录。

[界面与图片操作](../../client-ui.md) · [输入与文件](../../client-input.md) · [原生 Intelligence UI](../../intelligence-ui.md) · [上下文记忆](../../context-memory.md) · [code_mode 指南](../../code-mode.md)
