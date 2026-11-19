# ssmcp 历史恢复与继续迭代记录

日期：2026-11-20；面向用户的时间采用 Asia/Taipei。

## 恢复结论

本轮已找回本地会话、Git 提交、正式发布记录与历史验收文档之间的对应关系，并把已经完成但未合并的 listener 历史分支接回主线。恢复时的正式版本是 **v0.10.13**，旧 Google 文档中的 v0.10.12 Windows 失败已经修复；007 恢复服务则确实还在运行旧版，本轮已完成现场升级。

当前主线检查点是 [b43cf1f](https://github.com/LIghtJUNction/ssmcp/commit/b43cf1f38bff28b04947fc82178bd7c26cbeb627)。它包含 [PR #101](https://github.com/LIghtJUNction/ssmcp/pull/101) 的 007 现场验收，以及 [PR #102](https://github.com/LIghtJUNction/ssmcp/pull/102) 的 listener 整合和左右滚动修复。源码合入与正式发布分别记录：此检查点没有创建新版本或重新部署服务器。

## 历史请求与实际落点

| 历史事项 | 恢复时的证据 | 本轮处理 |
| --- | --- | --- |
| v0.10.13 正式发布 | 源码 `4e3132d`，Release run 44610211292 的 19 个任务成功，约 04:51 完成 | 确认发布与部署记录，关闭旧 v0.10.12 失败判断 |
| 007 恢复服务升级 | 专用运行映像仍为 v0.7.4、协议 5；网关要求协议 7，进程存活但持续重连 | 官方 v0.10.13 包校验、预检、升级及真实 MCP 往返已通过 |
| listener、线程来源与传输 | 历史分支 `57d3065` 含核心实现 `1001a0d`，当时尚未完整合入 | 与当前主线整合、解决冲突、重新验收后经 PR #102 合并 |
| 命令窗口与整页分别滚动 | 已有命令窗口，但正文左右两半没有分流 | 左半区滚命令输出，右半区滚整段会话；PR #100 已随整合进入主线 |
| zuse 作为子模块及 computer-use 库 | 恢复时主线没有该依赖，历史中只有接入计划 | 独立候选正在接入，固定已推送上游提交并保留既有输入保护 |
| ztui 更名为 ztuio | 本轮现场追加协同要求；ssmcp 内嵌版本比旧独立库更新 | 上游先归并完整 API 与渲染优化，本轮在独立工作树迁移消费者 |

发布依据：[v0.10.13](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.13)、[正式工作流](https://github.com/LIghtJUNction/ssmcp/actions/runs/44610211292)。历史 listener 依据：[原始验收](https://github.com/LIghtJUNction/ssmcp/blob/57d3065ed2d65f579371e3cde170ebc6186f0ed0/docs/reports/thread-listeners-20261119/README.zh-CN.md)。会话中的授权需求用于确认工作范围，不把私有会话正文或完整客户端标识复制进公开仓库。

## 007 的现场结果

[完整验收记录](../007-upgrade-20261120/README.zh-CN.md)与[机器可读证据](../007-upgrade-20261120/evidence.json)已经进入主线。

升级后的专用恢复服务运行 v0.10.13、协议 7，并恢复 connected 状态。两轮独立 MCP 请求分别调用 status 与 list，验证同一新进程和同一连接能够持续响应。十份保存会话、独立程序快照、配置及 systemd unit 的校验保持；原有十一个客户端的进程启动身份与运行映像保持。旧恢复映像的独立备份已保留。

这里验证的是恢复服务专用副本。开发机日常 CLI 文件仍是原先的 v0.10.12，本次没有把“007 已升级”写成“所有客户端都升级”。

## listener 与终端滚动的整合结果

冻结产品源码为 `70aebd09d56cfa21cd0229da6c0e3c3cdaae2200`。后续 `9989076` 只调整两个旧 E2E 收尾夹具，`714a8c9` 添加脱敏报告与截图。全部端到端检查使用同一组冻结二进制；原始摘要与具体断言见[整合验收报告](../listener-integration-20261120/README.md)和[validation.json](../listener-integration-20261120/validation.json)。

| 检查 | 本轮结果 |
| --- | --- |
| 格式、库测试编译、CLI/daemon/frontend/VT 解码器构建 | 通过 |
| 严格 all-target Clippy | exit 0 |
| 定向 Rust 测试 | 56 项通过，按测试名称去重 |
| 真实 CLI/PTY 强制发送 | 118 项检查通过 |
| HTTP/Unix 双程序 listener | 73 项检查通过 |
| 真实 CLI/PTY 用户等待 | 121 项检查通过 |
| local 与真实 Chromium OAuth | 44 项检查通过 |
| 左右滚动行为 | 120、49、48 列状态回归通过；120 与 48 列 PNG 人工核验通过 |

这些数量属于不同测试程序，不能相加解释为互不重复的功能。Cargo 仍报告既有 `qrcodegen` 未使用依赖警告；严格 Clippy 成功不被写成“零警告”。本轮没有运行全部 1,502 项库测试，也没有把 Linux 本机验收扩展为 Windows/macOS 运行结论。

两个失败都得到了具体定位。新工作树需要显式选择已有 Playwright 模块；旧退出夹具也需要遵循已经建立的 Ctrl-C 行为：有草稿先清空，空输入先提示，再确认退出。修正后两个完整套件重新通过，冻结产品二进制没有因此改变。

滚动分区用一个直接规则实现：鼠标横坐标位于对话正文左半区且指向命令窗口时，只改该命令的内部偏移；位于右半区时，只改会话偏移。回归同时检查另一命令窗口、草稿和光标状态，因此能区分“画面看起来滚了”与“正确的状态发生了变化”。

## 当前依赖协同检查点

zuse 的新可集成源码为 [c0dbfda315f8cf1cd03ddb0a3db15f3dcab22d41](https://github.com/LIghtJUNction/zuse/commit/c0dbfda315f8cf1cd03ddb0a3db15f3dcab22d41)。它的清单版本仍是 0.1.0，已发布 crate 的正式标签源码为 `dd57e38`；二者不能仅凭版本字符串视为同一源码。ssmcp 将以子模块提交固定构建来源，独立 zuse 项目正在开发的未提交 Wayland 修改不混入本轮候选。

ztuio 的 canonical 源码为 [9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e](https://github.com/LIghtJUNction/ztuio/commit/9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e)。迁移要求是复用该子模块，保留从 ssmcp 内嵌 0.1.2 合入的公共接口和 ANSI 渲染改进，并同步当前 Cargo、导入、打包与发布脚本中的名称。

本轮第一次 registry 核对确认 zsys 0.1.1 已发布且未撤回；ztuio 0.1.0 和 zplugin 0.3.1 当时仍未发布。随后确认 ztuio 0.1.0、zplugin 0.3.1 与宏包均已发布。zharness 当前正式版本仍为 0.5.0，精确依赖 zplugin 0.3.0；新的 shared-loader 消费者候选保留在独立分支，待匹配的 zharness 来源完成后再升级，不阻挡本轮 v0.10.14 发布。

## 后续工作的边界

zuse、ztuio 及后续 zplugin 依赖变动需要各自的冻结候选和本机验证；listener 的通过结果不会自动证明新依赖正确。更完整的跨会话生命周期与内存优化保留为后续独立任务。日常验证遵循项目的本地检查规则，没有为本轮触发或等待普通 CI。

本报告会随本轮依赖接入完成补充最终结果。公开记录只保留源码、版本、测试和布局证据，不包含密钥、生产主机信息或私人会话内容。

## v0.10.14 发布准备检查点

用户在本轮明确要求及时正式发布。组合产品源码固定到 `b49da12ac890f4d2b00c42d9dcf05b97e2054bac`，已包含 zuse、ztuio 迁移和等待用户时的 ASCII 休眠界面。CLI、daemon 和可选 frontend 构建结果均为 0.10.14；格式、严格 all-target Clippy 和 73 项 Python 发布校验通过。

原生插件 28 项定向验收已通过，包含显式执行的真实 cdylib 合约；每次执行前后都核对冻结 libtest 与插件摘要。休眠新增六项回归通过；实际 PTY、完整默认库测试和更严格的取消回执校验正在收尾。旧 local_frame 测试的宽侧栏文案被错误套用到 compact 布局，正在按当前实际显示修正测试预期，产品 ELF 不因测试夹具修改而改变。

当前安装和首次连接文档已经通过 [PR #105](https://github.com/LIghtJUNction/ssmcp/pull/105) 合入主线，统一使用 HTTPS/WSS 和 `--allow 用户名@服务器`。正式发布前还会记录完成的真实桌面、终端和插件验收。此准备检查点尚不代表 tag、Release、crate 或生产部署已完成。
