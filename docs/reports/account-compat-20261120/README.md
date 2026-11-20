# 自身账号兼容修复验收

产品修复来自 `610a59608faa36ec5bf557f6c2dfc38f43eb3cd5` 和 `7292332cd62077fab03e10f946856ef60de583dd`，已由 main 的 `f38d9d1b10e69d0d1a596c85105e4bb6699b0dff` 集成。0.10.15 候选只在此基础上增加版本、发布说明和只读升级预检；没有引入其他功能分支。

[完整隔离验收结果](compatibility-e2e.json)记录 69 项通过，使用真实旧版 0.10.13 daemon 和当前 main daemon。零参数测试的 argv 只有候选路径，使用临时默认服务器与合成凭据；真实 `ssmcp.exec.start/read` 返回 `bare-default-ok`、exit 0。显式兼容在两版 daemon 上均通过真实工具调用，另一账号访问已知 client ID 得到 404。

缺少或过期凭据、错误 subject/principal/注册用户名、多账号、跨 origin、无注册用户名及撤销凭据均被拒绝。新 daemon 撤销控制者凭据后可保留受控 socket；验收检查 whoami 和 call 均返回 401，以及重新连接被拒绝，没有修改服务器原有生命周期。

新 daemon 匿名多账号验收使用真实 daemon 的匿名 wire peer；两名获准账号可以调用，未列账号被拒绝。这一项验证的是服务器协议，不冒充在 loopback HTTP 上完成生产 HTTPS namespace 的 CLI 测试。

上述产品回归产物 SHA256 为 `96872b93bae63af30d4597ea705c4bb043c9a5fe50d0fa034ed7ca3b3a64cf23`。Rust 检查三组分别通过 6、8、9 项，组间有重叠。该产物仍显示 0.10.14；0.10.15 的优化构建另行记录产物摘要，不能将旧二进制冒充新版本。

[本次发布政策检查](release-gates.json)保留原有发布、部署和只读预检测试。GitHub 正式六平台构建、crates.io 发布、生产服务器部署是独立状态；本报告不将本地验收视为已经发布或部署。
