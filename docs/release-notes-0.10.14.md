# v0.10.14

本版整合此前已完成的账号、线程监听与终端交互工作，并接入固定版本的 zuse 和独立 ztuio。

## 账号与设备接入

已认证的旧普通客户端仍可使用与自身身份完全一致的默认 GitHub principal 完成握手；该兼容分支只授权已验证的账号自身，匿名或混合名单仍拒绝。旧客户端不能借此修改新版访问名单，相关操作会提示升级。

设备通过 HTTPS/WSS 连接中继，使用 `--allow 用户名@服务器` 明确允许控制者。被控设备可以不保存登录；控制者必须在该服务器完成真实账号认证。显式名单只授权列出的人，已登录设备省略名单时只允许自己。启动阶段即收到撤权通知的未登录设备会正确退出，并停止自动重连。

~~~sh
# 控制者先在目标服务器注册或登录。
ssmcp alice@relay.example

# 被控设备在项目目录启动。
ssmcp --server https://relay.example \
  --allow alice@relay.example --name device-b

# 控制者查看和操作获准设备。
ssmcp list --server https://relay.example
ssmcp -i --server https://relay.example --client SESSION_ID
~~~

**升级现有脚本时，请同时检查客户端与实际运行的服务端。** 旧 SSH 设备目标、`--ssh`、`@github` 名单及 CLI 的 `invite` / `join` 已退出当前流程；MCP 的 `invite` 也已移除，会话内 agent 的 `collab.join` 保留。MCP 应用使用服务器 `/mcp` 的独立 OAuth 授权。

流量按经过验证的实际控制账号归属，按本月上传与下载累计值中较大的一项计量。共享设备不会因其中一个控制者的额度错误而关闭其他获准控制者的连接。管理员可分别配置免费与付费账号的月度赠送额，界面显示真实额度与下次重置时间。

参见[首次连接](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/onboarding.md)、[账号与访问](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/client-accounts.md)和[配置](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/configuration.md)。

## 终端与等待用户输入

- agent 等待用户输入时压暗终端画面，显示 ASCII 休眠标识。点击只解除遮罩，保留当前等待、草稿、要求队列及浏览位置；首个键盘输入照常处理。同一等待轮次的重试不会反复盖住已经唤醒的画面。
- 鼠标在对话正文左半区指向命令窗口时，滚动该命令的内部输出；右半区滚动整段会话，各窗口保留自己的位置。
- 输入区域支持 Alt+Up / Alt+Down 调整高度，显示当前模式；排队要求提供确认的交付回执，并可取回继续编辑。
- 保留原生文字选择，显示紧凑图片缩略图、当前上下行速率，并减少后台刷新造成的画面跳动。

休眠输入屏障覆盖完整鼠标手势，并处理等待结束与重画之间的事件顺序，避免点击穿透；Copy 模式会在鼠标抬起后恢复原捕获状态。参见[客户端界面](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/client-ui.md)与[等待机制](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/user-wait.md)。

## 线程监听与核心服务

程序监听器支持 HTTP 与 Unix socket，按经过认证的线程及连接代际约束投递和状态读取。队列回执保留实际来源，旧连接的监听状态不能冒充当前连接。[线程监听器](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/thread-listeners.md)

OpenCode 取消订阅时会先释放网络连接，并在后台保留部分原始输出；shutdown 等待归档完成后再关闭事件记录。磁盘归档锁不再阻塞取消任务的析构。

启用 OAuth 持久存储后，有效账号授权可在核心重启后恢复，保留原到期时间和撤销状态。未完成的登录及旧的连接绑定授权不跨进程恢复。[授权持久化](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/oauth-durability.md)

新增可选公共前端进程，负责公开指南和已知静态资产；OAuth、动态账号、计费与设备连接仍由核心处理。未配置该进程时继续使用内置页面。[公共前端](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/public-frontend.md)

## zuse 与 ztuio

zuse-computer 0.1.0 以 git 子模块固定到 `c0dbfda315f8cf1cd03ddb0a3db15f3dcab22d41`，由受管工作进程直接调用 Rust Desktop API。父进程跟踪取消、输入释放和退出，并清理自己的截图临时目录。现有 MCP Schema、隔离桌面默认选择与主机桌面授权检查保持一致。Windows/macOS 输入与状态、明确请求光标的截图路径及显示器拓扑继续使用既有兼容适配器。

Linux 私有桌面改用显式高位 X11 显示号，在启动时核对实际子进程、套接字与 ready 回执，并使用私有认证 cookie。分配和取消清理保留同 UID 排他锁，避免私有桌面抢占宿主显示号；测试夹具采用同样的隔离边界。

终端基础库改用独立发布的 ztuio 0.1.0，固定到 `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e`，保留原内嵌 0.1.2 的公共接口和绘制优化。应用仓库不再保存另一份终端核心，正式发布只发布 ssmcp；独立依赖分别维护和发布。

本版继续使用经过配对的 zharness 0.5.0 与 zplugin 0.3.0。zharness 0.5.1、zplugin/macros 0.3.1 与 zuse-computer 0.2.1 的组合升级单独验证，未包含在本版中。[依赖与构建](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/dependencies.md)

## 安装与验证

通过官方安装器安装的客户端可运行 `ssmcp update --check` 查看新版本，再运行 `ssmcp update`。通过 Cargo、Homebrew、Scoop 或系统包管理器安装的客户端，请用原包管理器升级。安装器替换磁盘上的程序，正在运行的会话仍需从已更新入口重启或按现有受控重载流程接管；服务端需要单独部署。

正式资产发布和生产服务切换分别保留回执。发布归档不代表已有服务器或正在运行的会话已经升级；若运行中的受管作业需要先交接，可以显式暂缓服务器部署，再使用同一正式 tag 与源码执行既有部署流程。

归档、安装器、SHA256SUMS 和 release-manifest.json 随正式 Release 提供。manifest 固定发布源码、子模块及归档摘要；正式工作流另记录六个平台构建、安装验证、crate 发布与服务器部署结果。具体检查和实际覆盖范围见[本轮恢复与验收报告](https://github.com/LIghtJUNction/ssmcp/blob/v0.10.14/docs/reports/history-resume-20261120/README.md)。
