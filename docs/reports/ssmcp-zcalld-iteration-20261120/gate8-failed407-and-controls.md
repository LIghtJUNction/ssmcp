# 407 门禁失败与验收控制器准备记录

本记录对应已合流源码 `407084c4fcc582f5fef01b772de838f04e2d5484`，源码树 `f824b238cfa541fb0f8cddcc68e34278f2c62e1b`。**该源码的最终本机门禁失败，24 套终端验收和最终 P1 尚未运行，尚无本轮正式发布或真实用户完成许可。**

## 实际结果

唯一构建负责人使用冻结的干净工作树、正常串行 rustc wrapper、`jobs=1`、已审 driver `1ad9ec76…` 和 outer launcher `99b78f15…` 启动了实际门禁。发布策略、部署策略和格式阶段通过；library-private 阶段以 exit 101 结束。

实际父 libtest 汇总为 **1880 passed、5 failed、22 ignored、4 filtered out**。失败包括日志视图的两条断言、工具组 reload 的滚动位置、阅读导航的原始 ID，以及消息预算正值 fixture。完整失败块和原始文件 SHA256 保存在相邻 JSON，未把通过的前置阶段作为整体成功。

| 原始回执 | SHA256 |
| --- | --- |
| candidate-gates.json | `97450a28c74a226a063ad29d887232da14bf74464f48dadd1718a447038909ed` |
| launcher exit.json | `43e4fd6334151e2f4f40bdf429e9cb930a601a719a14fa7c10fd4f10fc6fee7a` |
| library-private 原始日志 | `663ba3363b1ddd495a4de35db2547b46cf22c461907a919d836b2dca1da32b2b` |

launcher 的最终回执同样为 failed，并确认 source、lock、wrapper、driver 和 Cargo config 未改变，工作树仍干净。该记录是失败证据摘录；原始完整文件保存在 JSON 记录的开发机路径。

[六平台预检 44769140252](https://github.com/LIghtJUNction/ssmcp/actions/runs/44769140252) 已以 workflow_dispatch 启动，API head_sha 精确为 407。此处不声明它已完成或通过；最终结果另存。

## 已封存的验收控制器

`final24-controller-5da61c1d-prepared.zip` 为 83,543 字节，SHA256 `f08d35dadc127158b44e296baf72cd47584e7ee808db963843ff82d855b67142`。它包含 17 个文件：4 个运行文件，以及纯合同、真实 producer 接口审阅和准备回执。52 项纯合同通过，解包到新目录后的 52 项可移植性复验也通过，均不属于产品验收。

P1 handoff 已在开发机的独立目录校验部署，13 个 manifest 文件符合固定包，部署命令 exit 0。P1 控制器仍是 prepared_not_run；它要求实际 passed gate 及实际成功 launcher 回执才会开始运行。

两套控制器保持原有严格谓词：源码与二进制绑定、真实报告、真实 PTY cells、前后哈希、明确字体、物理延迟预算和模拟人工许可边界。后继源码、门禁目录和 launcher hash 变化后，须重新审阅其绑定，不能把本次 407 的失败记录改成成功。

## 后继范围

修复 5 条失败后，还将纳入新收到的明确需求：等待倒计时／已等待时长及时间原语、直接依赖稳定版迁移，以及 PR146 在已合流 8e 后新增的历史选择器焦点修复。

[PR135](https://github.com/LIghtJUNction/ssmcp/pull/135) 的精确 head `25456296…` 已是 407 的祖先，GitHub 固定比较为 ahead 0、behind 111、无文件差异。已以说明评论关闭其遗留跟踪状态；源码早已合流。

[PR146](https://github.com/LIghtJUNction/ssmcp/pull/146) 的实际新 head `ae8ede2a…` 仍有 3 个尚未进入 407 的提交，下一候选须正常合流并重新验证。固定 407、失败回执和正在执行的 407 平台检查保留各自来源。

生产服务与现有客户端保持运行。本记录不授予重启、切换或真实结束许可。
