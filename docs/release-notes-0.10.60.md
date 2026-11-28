# 首次连接与控制者授权

首次连接中继时，浏览器会用中英文引导你核对设备验证码、确认账号属于你或你明确许可的朋友，并确认该账号能控制会话、运行命令和访问会话可读的文件。首次使用时，可填一个可用的中继用户名，再登录 GitHub 完成注册。

在同一台设备上连接其他控制者，需要在本机显式批准。终端会给出包含实际账号的命令。也可从 `ssmcp controllers list` 核对待批准的账号，在本机运行 `ssmcp controllers approve user@server`，或运行 `ssmcp controllers revoke user@server` 撤销批准。请把 `user@server` 换成你核对过的中继账号。

Web 工作台和 Magisk 模块的首次连接也要求分别确认账号和控制权限。Magisk 页面明确提示远程会话可使用 Android root 权限。

使用 `ssmcp user@server --control-only` 可以启动一个系统 Shell，直接看 Agent 执行的命令、输出和退出状态。加上 `--daemon --name control` 可在后台运行，并用 `ssmcp sessions logs control --pretty` 查看日志。
