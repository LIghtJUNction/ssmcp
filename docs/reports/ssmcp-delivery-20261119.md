# SSMCP 账户流量、支付与桌面交接交付记录

更新日期：2026-11-19（Asia/Taipei）

## 已交付内容

| 项目 | 已确认状态 |
| --- | --- |
| 项目所有者管理员身份 | 已在实际服务配置与账户记录中核实，无需修改账户 |
| 月度免费流量与管理员无限额度 | [PR #36](https://github.com/LIghtJUNction/ssmcp/pull/36) 已合入主分支 |
| Waffo 支付与月度库存 | [PR #37](https://github.com/LIghtJUNction/ssmcp/pull/37) 已合入主分支，默认关闭销售 |
| 主桌面本地交接保护 | [PR #38](https://github.com/LIghtJUNction/ssmcp/pull/38) 已合入，合并提交 532a031 |
| 调用者列表保留修复 | e26c576 已合入；[验证记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/retained-callers-20261118.md) 已提交 |
| 服务条款、隐私、退款与开店交接 | 模板与完整中文提示词已提交到主分支 |
| 新客户端与 0.10 整合 | 其他工作分支继续推进；本记录不宣称最终验收或发布完成 |

代码合入、正式发布和运行中程序升级是不同状态。以下结果以冻结源码、实际测试输出和 GitHub 合并记录为依据。

## 账户与月度流量

普通账户每个自然月获得 1 GB 免费额度，这里 1 GB 为 1,000,000,000 字节；默认月份边界使用 UTC+8。付费额度只属于购买时的月份，月底到期。管理员的 API 流量上限和剩余额度为 null，门户显示“无限”；既有速率限制仍保留。角色由服务端账户记录决定，客户端不能通过参数自行提升权限。

账本按账户与月份持久化，在转发前写入。重连、重启不会重置已用额度；同一付款订单不会重复发放额度。记账对象是实现规定的应用协议数据，不宣称等同于网卡、TLS 或运营商计费字节。转发失败后的已预扣字节不自动返还，重试会再次计入；旧 SSH 控制消息的既有边界保持并记录在实现中。

冻结功能提交为 `fe8bf21d202ac5d404eb625a02edbfb72706ecc6`，PR #36 合并提交为 `52df22ee282141e095c2aa17bc53c95d062df80a`。

## 支付与库存

默认单价为人民币 1 元/GB，销售关闭、月度可售库存为 0。运营者可以配置网关、商户、店铺、商品、环境、单价、币种、月份偏移和月度可售容量。

下单需要已验证账户、同源请求和 CSRF 校验。服务端保存购买人、月份、库存与基础价格；收到原始请求体 RSA 签名验证通过的支付通知后，再核对订单、身份、币种、税费和实际扣款金额，按订单标识幂等发放额度。旧月份的迟到付款进入人工处理路径，不移到新月份发额度。库存仅在支付平台提供可信的终态取消证明后释放，本地超时不足以证明订单不能继续付款。

实现明确使用税外基础价格，分别核对基础金额、税额、应付总额和实际扣款。账本绑定环境、商户、店铺与商品；测试环境需要独立 state_dir。

**真实收款尚有明确的平台限制待解决。**官方永久商品链接和公开 Store Slug 能直接创建结账，可能绕过本应用的账户与库存入口。严格回调核对能阻止错误发额度，但无法阻止平台先向没有本地订单的买家收费。尚未验证能够封闭这些入口的平台设置，因此商品应保持测试或未发布状态，不能把隐藏链接视为问题已经解决。

支付冻结提交为 `c86ba97dc6e533d4959f8caa8a0360f64f458ec5`。完整配置与边界见 [commerce.md](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/commerce.md)。平台依据：[商品与永久购买链接](https://docs.waffo.ai/features/products)、[创建结账会话](https://docs.waffo.ai/api-reference/endpoints/orders/create-checkout-session)。

## 桌面操作交接

原生 computer 工具默认使用私有桌面。显式选择主桌面仍可观察；聚焦、移动、点击、拖拽、按键、输入和滚动需要用户在本地终端执行 `/computer host`。默认交接 15 秒，可指定 1–60 秒。`/computer stop`、本地终端继续输入、到期、新的交接或客户端关闭会使旧授权失效。

远端工具不能自行开启或延长交接。取消正在进行的调用时，按键和按钮释放清理完成后才让本进程的下一次主桌面输入进入；清理失败则继续锁定主桌面输入。排队中的旧调用不会因重新交接而恢复，重连不会继承授权。

检测范围是 ssmcp 本地终端，不是其他应用的全部键鼠活动；串行锁覆盖同一个客户端进程。CDP、Omarchy、外部 harness 和 shell 执行保持各自能力，这项措施不是操作系统沙箱。验证使用假输入与释放任务，没有向真实桌面发送测试键鼠事件。Windows/macOS 真机行为仍需相应平台验收。

功能提交为 `efb3d5654af2d2d72150a662bdeda336d041d6fa`，合并提交为 `532a031b493d9d1f19184c7c12c25ef7f8d89cde`。使用说明见 [computer-use.md](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/computer-use.md)。

## 验证记录

| 范围 | 实际结果 | 版本与限制 |
| --- | --- | --- |
| 月度额度、认证、HTTP/WS/SSH、持久化 | 111 passed，0 failed | fe8bf21；独立 Cargo target |
| 调用者保留修复 | audit_view 6 passed；caller 筛选 13 passed | 两组存在重叠，不相加为唯一用例总数 |
| 支付库 | 26 passed，0 failed | c86ba97；新的税费、取消、账本范围用例实际命中 |
| 支付服务端配置 | 2 passed，0 failed，9 filtered out | c86ba97，Cargo exit 0；含编译 179.16 秒；独立 target |
| 主桌面交接 | 29 passed，0 failed，4 个既有 ignored | Linux 假输入与取消/释放任务 |
| 本地交接命令解析 | 1 passed，0 failed | /computer；不包含已移除的旧 exit 实现 |
| 主桌面最终编译 | cargo check 通过，exit 0，496.73 秒 | tree 1686da4b8e2259bac9531f9c8cfdccff2815e7cb |

支付配置补验的原始日志为支付工作树中的 `.local/payment-final-verification-20261119/ssmcpd-commerce.log`，核验后的结果为 `verified-result.json`。Cargo 实际退出码为 0，两个确切测试名均匹配，验证前后的 HEAD 一致。辅助汇总脚本首次对输出分隔符转义有误，随后依据原始日志的确切测试名校正；原始日志和初次汇总均保留。

主桌面最终编译记录位于隔离工作树的 `.local/computer-validation/final_core_check.json` 和 `final_core_check.log`；功能结果在 `results.json`、`computer.log` 与 `handoff_command.log`。编译 JSON 记录修改提交消息前的 beb864f，经直接核对，其 tree 与已推送的 efb3d56 完全一致。旧 local_exit 测试不作为当前 /exit、/quit 的验收证据。

此前共享测试输出目录曾出现旧二进制命中零用例的问题，后续使用独立 target 并核对实际用例名。超时任务没有计作通过。本轮保留开发机已有的 rustc 串行 wrapper 和锁，没有删除锁或终止他人构建。

## 客户端与发布交接

Logs/Tasks、横向面板、技能来源、ASCII 宠物和退出命令属于 [PR #39](https://github.com/LIghtJUNction/ssmcp/pull/39) 及后续整合。截止本记录核验，PR #39 仍为 draft；已记录的 581 项通过与 1 项失败之后，测试夹具已有修正，不能用较早的 41 项 PTY 结果代替最终版本验收。

客户端账号名和剩余流量已有通用服务端接口：`/api/client/whoami` 返回顶层 `identity` 与 `traffic`。身份字段包含 `provider`、`subject`、`login`、`principal`、`username`；可以优先显示登录名，另列不相同的站内用户名。只有 traffic 对象存在且 limit_bytes、remaining_bytes 显式为 null 才表示无限。未登录、请求失败、旧服务器缺字段或尚未取得数据，都应显示对应状态，不能显示为 0 或无限。该契约和 PR #38 的合并结果已经通过开发机工作对话交给现有整合流程。

另外的工作分支正在推进 ssmcp update、门户与登录会话改进以及运行时预检。本轮没有重复接管这些活跃实现。为推进日志面板交付，另建立独立工作树，从 PR #39 冻结提交继续最小集成验收，结果将单独更新。相关入口包括 [PR #34](https://github.com/LIghtJUNction/ssmcp/pull/34)、[PR #40](https://github.com/LIghtJUNction/ssmcp/pull/40)、[PR #41](https://github.com/LIghtJUNction/ssmcp/pull/41)。

直接查询 GitHub Release 列表时，最新正式公开版本为 [v0.6.3](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.6.3)，另有 v0.5.1。主分支曾读取到 Cargo 版本 0.9.4；版本号提交不等同于正式发布成功。本记录没有取得整个新客户端最终发布、安装和运行中程序升级的完整证据，因此不把这些状态列为完成。

## 店铺提示词与协议模板

按照用户最后的安排，店铺创建交由专门的浏览器代理执行。本轮提供完整中文提示词，包含产品定位、价格、月底到期、库存与身份入口、测试环境、公开结账限制和需要回传的配置字段；本轮没有创建新店铺、发起真实付款或填写虚构经营主体资料。

- [完整店铺创建提示词](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/commerce/store-setup-agent-prompt.zh-CN.md)
- [服务条款模板](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/terms-of-service.template.zh-CN.md)
- [隐私政策模板](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/privacy-policy.template.zh-CN.md)
- [退款政策模板](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/refund-policy.template.zh-CN.md)
- [模板填写说明](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/README.md)

经营主体、联系方式、保存期限和退款细则仍需真实运营资料。模板不是已经完成的商户法律页面。
