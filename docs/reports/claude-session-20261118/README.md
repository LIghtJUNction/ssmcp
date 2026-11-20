# ssmcp 0.8.0 会话交互升级与部署记录

记录时间：2026-11-18（Asia/Taipei）。客户端、当前任务会话和服务器均已运行 0.8.0；生产队列验收通过。历史制品归档清理已于 14:40 完成，正常 ssmcp 访问也已恢复。

## 本轮交付

默认界面采用深色 `chat` 布局：连续活动区、底部输入框和可折叠右侧状态栏。命令输出保留上下文，源码审计按需展开。输入框支持可编辑队列、换行、按实际显示行移动光标、历史回看及斜杠命令补全。

`/agents` 同时展示已登记成员与自动观察到的普通调用。普通调用按服务器运行 ID 区分，可打开对应源码与结果审计；登记成员可以成为定向需求的接收者。名称与模型是展示信息，接收身份由成员凭证确认。

新需求默认交给下一次实际访问当前客户端且符合授权条件的 `code_mode` 调用。正文放入 `structuredContent.user_messages` 和独立的用户要求文本块，原有工具结果继续保留。界面轮询及工具目录查询不会抢先消费队列。已递送仅表示内容进入工具响应，不保证模型已经阅读或执行。

## 操作速查

| 操作 | 行为 |
| --- | --- |
| 输入文字后 Enter | 提交新要求或重新提交编辑内容 |
| Ctrl-J | 换行；Shift-Enter 取决于终端按键支持 |
| 输入框 ↑ / ↓ | 按实际显示行移动，包括自动换行；到边界后取回待交付项或浏览历史 |
| 输入 `/` 后 ↑ / ↓、Tab、Enter、Esc | 选择命令、补全、执行、收起候选并保留文字 |
| `/queue`，↑ / ↓，Enter | 选择尚未交付项并取回编辑；确认取回后修改，再按 Enter 提交 |
| 队列 Delete；→ / ← | 取消可撤回项；查看详情 / 返回 |
| `/agents` | 查看成员与普通调用；Enter 选择成员目标或打开调用审计 |
| `/audit` 或 Ctrl-O | 查看源码审计；Enter / 右键展开，← / 左键返回 |
| F3 或 `/sidebar` | 折叠或展开右侧状态栏 |
| `/reload` 或 `/restart` | 重启客户端，交接草稿与可确认未交付的要求 |
| F5 | 仅重载显示配置 |
| 滚轮、PgUp / PgDn | 直接滚动内容 |
| `/filter 文本`、F9 | 文本筛选；切换全部 / 命令 / 错误类别 |

重启后，目标失效或交付不确定的队列项等待复核，不能自动重复发送。本地 `attach` 目前用于观察后台会话，不转发其自身输入框队列；完整交互交付用于直接连接的前台客户端。

## 验证结果

| 验证 | 结果 |
| --- | --- |
| Rust 格式检查 | 通过 |
| Clippy，全目标、警告视为错误 | 通过 |
| 客户端 Rust 测试 | 299 项通过 |
| 实际渲染器预览 | 38 项通过 |
| SSH 输入与重启端到端验证 | 88 项通过 |
| Relay 输入端到端验证 | 45 项通过 |
| 仪表盘验证 | 84 项通过 |
| 真实生产环境队列验收 | 24 项通过 |

生产验收通过公开 HTTPS、常规 OAuth PKCE 和已有 SSH 授权完成，使用两个逻辑成员，没有启动外部模型。覆盖修改后内容递送、旧内容与取消项不递送、不重复递送、定向隔离、界面观察不消费队列、原生工具往返和测试客户端退出。

测试回执：[验证汇总](validation.json)、[Rust 检查](rust-checks.json)、[发布构建](release-build.json)、[预览](preview.json)、[SSH 输入](composer.json)、[Relay 输入](relay-composer.json)、[仪表盘](dashboard.json)。

实际渲染器示例截图：[默认界面](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/client-ui.png`）、[浅色界面](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/client-ui-light.png`）、[源码审计](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/client-audit.png`）。截图使用固定示例数据。

## 实际安装与上线

开发机系统包已更新到 `ssmcp-git 0.8.0.r266.g7867cb8-1`。默认命令入口已统一指向新版系统程序；当前任务客户端已重启，运行文件的 SHA-256 与包内文件一致。其他已有会话没有被批量重启。

生产服务器 `ssmcpd.service` 已运行 `ssmcpd 0.8.0`。部署后检查确认服务活动、运行程序和安装包一致、配置与发行方保持一致、公网健康及发现端点正常、未授权请求被拒绝，动态客户端注册在重启后保留。现有未配置的客户端自助下载端点保持原状。

已安装包的源码提交为 `7867cb8fd42f2f62aae61b38f87baf92c9b6846b`，运行代码提交为 `9f1008e3966542a8f4d1134451573a605cf29e39`。两者间仅增加测试与文档材料，构建输入一致。后续文档和部署回执提交不改变已验证二进制。

包 SHA-256：`cc74de563471a2f98e2413ad06555cd343ff08231194f30e22d703f179f1f4df`。

证据：[客户端安装回执](client-install.json)、[生产部署与队列回执](production/7867cb8fd42f/deployment.json)。

## 归档清理恢复

初次生产回执保留 `deployed_followup_failed`，记录当时整包归档超时以及同名文件重试失败。正常连接恢复后，独立 cleanup-only 作业于 2026-11-18 14:40（Asia/Taipei）完成，新的[清理恢复回执](production/7867cb8fd42f/cleanup-recovery.json)状态为 `completed`。

25 批、126 个条目、共 211,025,267 字节历史材料均已分批压缩归档并验证，服务器上对应的 25 个根路径已移除。压缩归档共 132,691,684 字节；服务器实际空闲增加 170,254,336 字节，清理后空闲 17,498,570,752 字节。白名单范围内没有剩余可清理根路径，也没有因活跃引用保留的根路径。

每批删除前验证内容哈希、类型、权限、所有者、软硬链接，重新比对服务器 manifest，并检查进程与活动服务引用。升级前的有效回滚备份重新通过完整校验，旧截断归档保留供排查，不作为删除依据。

清理没有再次安装或重启服务。结束时服务 active、PID 与配置未变，公开健康端点返回 200；daemon、runner 和默认客户端的摘要仍与已部署包一致。旧 MCP 授权不保证跨服务器重启保留，本次正常访问已恢复。

## 交付位置

- [PR #23](https://github.com/LIghtJUNction/ssmcp/pull/23)
- [完整操作说明](../../client-ui.md)
- [Google 云盘部署记录](https://docs.google.com/document/d/19JAo0IuvjeTmxlpH7j79pqcj-R_8HR7M0IFwmTqOujE/edit)
