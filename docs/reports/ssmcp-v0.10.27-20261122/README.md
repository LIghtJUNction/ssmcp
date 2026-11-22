# SSMCP v0.10.27：常用命令、插件管理与远程 worker

> 交付完成：正式 Release、完整发布来源核验、本机 Cargo 客户端与 oh-my-ztty 更新均已完成。记录日期：2026-11-22。

SSMCP v0.10.27 已正式发布。本版补齐了终端内的版本、关于信息和插件管理入口，增加可组合的路径查询，并把远程 worker 的启动收敛为 `ssmcp --worker N`。已有连接配置和登录状态时，不需要再为默认 worker 选择本地模型执行器。

代码通过 [PR181](https://github.com/LIghtJUNction/ssmcp/pull/181) 合入。正式源码与本轮功能验收候选的 Git tree 完全相同；正式 Release 的完整工作流也已成功。本轮独立 H2 核验进一步确认了原始派发、任务日志、构建产物和全部公开文件的对应关系，实际结果为 passed。

## 1. 最简单的使用方式

普通会话和 worker 会话分别使用：

```sh
ssmcp
ssmcp --worker 2
```

`--worker 2` 创建两个可以领取远程任务的槽位，默认不启动 Codex、Claude 或其他本地模型。任务的提交、领取、取消和完成通过现有已认证工具通道处理；自然语言任务不会被自动解释为 shell 命令。

worker 界面为每个槽位显示真实状态和独立日志，没有聊天输入框。左右键切换槽位，上下键或 PgUp/PgDn 滚动日志，Home 查看较早记录，End 恢复跟随最新日志；切换槽位会保留各自的滚动位置。Ctrl+C 关闭当前会话。具体行为见本版固定的 [worker 文档](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/docs/workers.md)。

这两个槽位是远程任务容量，不代表额外启动了两个模型进程，也不宣称为外部 agent 提供本机 CPU 或内存硬隔离。

### 终端内查看版本、关于信息和插件

| 命令 | 实际用途 |
| --- | --- |
| `/version` | 显示当前正在运行的版本 |
| `/about` | 打开独立的关于页；版本、项目与许可证信息集中展示 |
| `/help` | 保留原帮助入口，不与关于页混用 |
| `/plugin help` | 查看本地原生插件管理用法；无需装载插件 |
| `/plugin list` | 异步显示当前客户端的真实插件列表 |

插件管理提供 `list`、`load`、`unload`。它管理当前客户端的本地原生插件，不是插件商店，也不等同于 tools Git 仓库或 RC 配置管理。加载路径作为单个参数传入，不执行 shell 展开；异步结果显示在消息区域。预览模式可查看帮助，但会拒绝真实管理操作并保留输入。源码入口为 [interface/plugins.rs](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/src/client/interface/plugins.rs)。

本轮真实连接验证执行了 `/plugin list`，没有实际装载或卸载动态库。结果显示空插件列表及说明，前后目标、空草稿和空队列均保持不变；这不扩展为“已有非空草稿或已选择收件人”的全部真实连接组合都已测试。

### 查询和组合实际路径

```sh
ssmcp paths --list
ssmcp paths config tools history --json
```

路径查询支持按名称选择、组合和 JSON 输出，保持请求顺序并去重。查询本身不创建目录、不读取凭据内容。帮助和参数以本版 [paths.rs](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/src/client/paths.rs) 为准。

## 2. 正式源码与发布状态

| 项目 | 固定结果 |
| --- | --- |
| 版本 | **v0.10.27** |
| 正式源码 S；本次发布工作流源码 P | `61a4de1cf522358691998ecb724b4a8518b91c47` |
| 正式源码 tree | `beca471b32d2a89616772ef83c65bca84bc51b7f` |
| 功能验收候选 | `b4d6672420ba78f8ec045d37f95e388707b907ad`，与正式源码为同一 tree |
| 合入记录 | [PR181](https://github.com/LIghtJUNction/ssmcp/pull/181) |
| 正式 Release | [v0.10.27](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.27) |
| Release 工作流 | [run 45002392978](https://github.com/LIghtJUNction/ssmcp/actions/runs/45002392978)，**success** |
| 工作流任务统计 | **17 success**；`publish_crates`、`deploy_server` 两项按策略 skipped |

本次发布采用 `git-source-v1`。GitHub Release 已成功，Crates 注册表发布仍为延期状态；本次 Actions 没有执行服务器部署。这三个结果分别记录，不将 skipped 计入 success。

部署策略也区分“历史版本的持久延期记录”和“这次明确不部署”。固定 [release.yml](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/.github/workflows/release.yml) 的服务器延期表截至 0.10.26，没有 0.10.27。本次原始 dispatch 明确选择 `deploy_server=false`，因此应严格核对：

```json
{
  "publish": "true",
  "deploy_server": "false",
  "version": "0.10.27",
  "deployment_deferred": null
}
```

同一工作流的 Crates 发布策略明确将 0.10.27 列入 `deferred`，不受上述 `null` 影响。

## 3. 实际完成的验收

各行来自本轮对应范围的实际回执，分别统计，不将交叠的测试相加为一个总数。

| 范围 | 实际结果 | 已验证的主要行为 |
| --- | --- | --- |
| 真实远程 worker 会话 | **31/31 项通过** | 两槽位、两条不同任务、独立领取、权限拒绝、取消占槽、完成幂等、独立滚动日志和清理 |
| paths CLI | **12 个场景通过** | 组合与去重、输出格式、默认路径、无副作用，以及非法参数应当失败的场景 |
| 真实 `/plugin list` | **通过** | 当前客户端真实插件列表可见；目标、空草稿、空队列前后不变 |
| 客户端 UI | **66 项通过** | 原 UI 流程及新增版本、独立关于页、插件帮助和预览拒绝流程 |
| Rust all-targets | **2255 pass / 0 fail / 33 ignored** | 同候选的完整四线程运行通过；ignored 未计作已执行通过 |
| 原生插件契约 | **另 1 项通过** | 独立原生插件契约验收 |
| 格式与静态检查 | **通过** | `cargo fmt --all -- --check` 与严格 Clippy；保留既有未使用 manifest key 提示 |
| 发布策略 | **118 项通过** | 发布规则检查；正式发布的结论另取自 Release 工作流 |

### 保留首次失败与后续原样验证

首次并行库测试实际为 **2133 pass / 7 fail / 16 ignored**。该次失败记录保留，没有将其改写为通过，也没有在失败后直接生成验收候选。

随后保持源码、超时和断言不变，逐项重放原七个失败测试，七项全部通过；再以 `--test-threads=4` 完整运行 all-targets，得到 **2255 pass / 0 fail / 33 ignored**。首次库测试与后续 all-targets 的统计范围不同，本文保留各自原始口径。单项重放通过不代表已经确定最初所有失败的根因。

用于复核的入口包括：

```sh
# Rust 全目标测试；本轮完整收口使用四线程。
cargo test --locked --all-targets -- --test-threads=4

# 固定发布工作流中的发布策略检查。
python -m unittest discover -s scripts -p 'test_release.py' -v

# 面向用户的只读查询与独立 worker 入口。
ssmcp --version
ssmcp paths --list
ssmcp paths config tools history --json
ssmcp --worker 2 --no-watchdog
```

上述入口便于复核，不是一次新执行的结果；真实 worker 验收还包含已认证参与者的任务操作和独立 PTY 观察，不由启动命令本身证明通过。

### worker 验收覆盖了什么

真实连接中提交了两条不同任务，分别占用两个槽位。错误的参与者不能完成别人的 claim，也不能读取不属于自己的任务结果；同一 claim 的重复领取保持身份不变，重复完成不会重复记账。

取消请求发出后，任务保持占槽，直到实际领取者使用原 claim 报告取消完成才释放。另一条任务独立完成并保留自己的结果。最终两个槽位回到空闲。界面同时验证了单槽位暂停滚动、切换槽位后恢复原位置，以及 End 回到实时跟随；导航没有出现聊天输入框。

共享日志记录任务生命周期与结果字节数，不公开其他参与者的原始 prompt 或结果正文。验收未启动额外模型；自有客户端和解码器均正常退出，终端设置恢复，没有清理错误或残留自有子进程，原开发会话保持在线。

## 4. 需要明确保留的边界

远程任务完成结果是**已认证 agent 的报告**，不是 SSMCP 对外部机器执行过程的独立证明。任务投递或 ACK 也不等于任务已经执行完毕。

按照固定 [任务状态实现](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/src/worker/queue.rs)，worker 客户端连接关闭后，未确认任务进入 `unknown`，不会自动重放可能已经产生副作用的操作。当前没有外部领取者心跳自动释放机制；领取者静默时，槽位仍保持占用。本次真实验收在两条任务均收尾后关闭连接，未把忙碌断连或外部领取者失联列作已测场景。

本轮 worker PTY 夹具关闭颜色，确认的是状态内容、独立滚动和输入边界，不把这些截图当作配色验收。`/plugin load` 与 `/plugin unload` 的真实动态库生命周期也未冒称由空列表查询覆盖。

## 5. H2 发布核验：准备与传输记录

H2 独立核验本次正式发布的来源、工作流执行记录和资产对应关系。此前遇到的三类问题分别发生在准备或只读取证环节：

| 类别 | 已查明的问题 | 本次处理与结论边界 |
| --- | --- | --- |
| 源码对象准备 | 编译工作树内没有三个子模块的独立 Git 检出，不能满足核验器读取固定 Git 对象的要求 | 使用独立完整检出，精确绑定正式 S、tree 与三个 gitlink；不修改原编译工作树。Cargo 使用的是锁定的 Git dependencies，不能仅凭目录未检出认定原构建使用了不同依赖源码 |
| 只读 API 传输 | 原生 `gh` 的 GET 调用出现 EOF，整套取证无法继续 | 仅在本次 H2 进程的 PATH 中使用传输适配；限定 GET，非零 EOF 最多额外两次，总预算 55 秒。白名单内的只读 GET 后续尝试追加 nonce，原资源路径和既有查询参数不变；成功响应字节不改，各次退出状态、原请求、实际请求和输出摘要均记录；超时明确失败 |
| 核验器版本常量 | 原 provenance 模块针对 v0.10.26，并要求其历史服务器延期原因字符串 | 按固定 v27 工作流适配版本常量与 `deployment_deferred=null`；完整字典相等检查、Crates 延期、原 dispatch 绑定及其他核验控制流保持 |

这些准备与取证失败的原始记录继续保留。适配后的完整核验于 2026-11-22 09:18:31–09:21:43 UTC 运行，实际 exit 0、双 EOF、报告状态 passed。核验取得全部 16 个公开文件，检查 8 个必要 Actions 产物及 17 个成功 job 的最初 checkout；该运行全部 Actions 产物为 15 个。只读 API 审计共记录 35 次尝试、33 次成功读取、2 次 EOF 重试、0 次超时。

此前完整核验在公开 job `137146692672` 的日志读取上连续三次遇到 EOF，随后两次新 GET 均取得相同的 **78088 B** 日志，SHA-256 为 `e121569873c0e219b0f5a23bdd104d7d232012c17ed9117a17fc1d1e4d8bd240`。后续运行又在固定 attempt 1 的 jobs 查询遇到同类 EOF；使用新请求复核时，默认传输和 HTTP/1.1 均成功返回 **66233 B**、19 个 job，SHA-256 为 `1b185def88a2a5077d71d82324365c045e22ca72dff733b150b8cef4ceedb8df`。这些观察不确定网络故障根因。最终传输后继仅对原白名单接受的 GET，在 EOF 后的第 2/3 次尝试追加 nonce；原 URL 路径、查询参数、运行和 job 选择均保留。适配程序为 **4243 B / `858e79381f0488178dd3d72e2e45ebaaaf2fb6b1353f9537b668ffd4cfd2c9a3`**。

本次版本适配后的 `manual_main_provenance.py` 为 **43791 B**，SHA-256：

```text
74aaa82cea8889006aa214e4354160dc6f7974082d7206846cafd9487d36fe19
```

静态比较确认，差分只含三个版本字符串、服务器延期原因 `None` 及解释文字；逆变换可逐字还原原件。主核验器与 dispatch adapter 相对上一准备版本未变。传输适配不替换核验结果、不滤掉失败断言，也不改原始 dispatch 回执。

## 6. 交付结果

| 项目 | 当前记录 |
| --- | --- |
| 正式 Release v0.10.27 | **已成功**；公开 run 45002392978，17 success、2 skipped |
| H2 完整发布核验 | **已通过**；完整原始报告 427915 B / `98abea32d57e75ac6a617a0c1094ef22f728ff6d753bfc0db3d4fb6536f349c5`；[公开摘要](publication-verification-summary.json) |
| 开发机实际安装 | **已安装 `ssmcp 0.10.27`**；Cargo 元数据与实际文件一致，辅助程序和既有进程保留；[安装摘要](cargo-installation-summary.json) |
| oh-my-ztty 集成 | **已激活并读回**；官方 0.2.10 基底上的本地修订，补全源码 `335e387a`，内置官方 SSMCP 0.10.27；[安装摘要](oh-my-ztty-installation-summary.json) |
| 最终 Google 文档 | [SSMCP v0.10.27 迭代与交付记录](https://docs.google.com/document/d/1vFu-7hJXi14AsjYB1dbldVBja3GUImEIXvau9lx-oks)，内容已更新并逐字读回 |

本机 Cargo 安装文件为 **16083264 B / `d733fec9fb71e599afb785d10af75181a4d1b7548de2787c30a83ff23fd797ae`**，与同一固定 S 的本机 release 构建产物一致。Cargo 的两份登记都由新 0.10.27 来源拥有 `ssmcp`，旧 0.10.26 来源继续拥有 `ssmcp-frontend`、`ssmcpd`；两个辅助程序的文件身份保持不变，原有 16 个 SSMCP 相关进程均以相同启动标识继续运行。

官方 Linux x86_64 压缩包为 **11427031 B / `f33208908c5878326aa0401a899271272e19a43f6ed2482baa80f89a7e26c5dd`**，其中客户端为 **16072584 B / `77ef144f67d066df15f8cd7fcbfb5009e4c925ba96253fcb469e5625f9a5dc74`**。官方包与本机 Cargo 构建来自同一 S，二进制摘要分别记录。本次 oh-my-ztty 实际使用上述官方 `77ef…` 客户端及同一归档内的 JavaScript runner。

### oh-my-ztty 的实际组合与兼容修复

本轮固定采用正式 [oh-my-ztty v0.2.10](https://github.com/LIghtJUNction/oh-my-ztty/releases/tag/v0.2.10) 基底，Release `454302797`，tag commit `9048bc1c6a20647e38893d4e1e84727f096dab98`。选取基底时，已观察到的 v0.2.11 源码尚无正式 Release，因此仅将它作为源码差分参考。

实际安装保留基底其他文件和原始 BUILD/依赖锁来源，覆盖两个补全文件及官方 SSMCP 客户端和 runner。新 BUILD 用 `local-revision` 明确记录这个组合。实际入口输出 `oh-my-ztty 0.2.10`，套件中的客户端输出 `ssmcp 0.10.27`，`current` 指向：

```text
releases/0.2.10-linux-x86_64-completion-335e387a-ssmcp-77ef144f67d0
```

真实用户目录的 `refresh`、`check` 均为 `current`；激活与最终读回也成功。安装前的 0.2.8 目录保留，原用户偏好和 profile 链接与原观察一致，安装操作没有变更服务。完整读回为 **10850 B / `d871d9212e7d05104d2ea4170916406a3ef5cd0f1319ac0df01ab03cc6da4f7c`**。

现场发现一个 **1919225 B** 的正常个人补全文件被受管文件的 1 MiB 限制误拒。修复只改变个人文件的读取方式：正文流式计算完整 SHA，命令声明只读取有界首行；受管文件的校验和原补全优先级保持。用户文件内容和模式保持不变。唯一新增回归实际 **1 test / OK**，覆盖大文件、完整指纹、尾部修改失效和 generation 更新；随后在真实用户配置中完成刷新、检查与激活。

修复通过 [PR11](https://github.com/LIghtJUNction/oh-my-ztty/pull/11) 正常合入：候选 `335e387af36bdf67fcc0b9dc67927dacb9825a72`，合并 `c4d94d20018ece30a729239e46c8d5e7dfe34782`。套件主线的既有 [来源同步提交](https://github.com/LIghtJUNction/oh-my-ztty/commit/0fd1d49127d6d537a5ea1d0309db3dcc35be3c43) 已将 SSMCP lock 与 gitlink 指向正式 S61a，因此没有重复创建来源同步变更；安装 BUILD 则分别保留官方基底的历史来源和本次覆盖来源。

### 按请求执行 git pull

已在 SSMCP 主仓库执行 `git pull --ff-only --no-rebase --no-recurse-submodules`。该次命令 exit 0，返回“已经是最新的”；当时 HEAD 为正式 S61a。已有修改、未跟踪文件及子模块 HEAD 的前后记录一致。原始操作回执为 **6986 B / `1ddf291e495c98aa25f6441b1b293733f969a040b7800e7da73d615691b51bc2`**，未将用户已有修改纳入本轮提交。

## 7. code_mode 的源码位置

代码模式与 SSMCP 在同一个仓库中。固定本次正式源码可直接查看：

- [src/code_mode.rs](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/src/code_mode.rs)：Rust 入口。
- [src/code_mode.js](https://github.com/LIghtJUNction/ssmcp/blob/61a4de1cf522358691998ecb724b4a8518b91c47/src/code_mode.js)：JavaScript helper。
- [src/code_mode/](https://github.com/LIghtJUNction/ssmcp/tree/61a4de1cf522358691998ecb724b4a8518b91c47/src/code_mode)：分拆的 Rust 实现。

## 8. 公开核对索引

| 对象 | 大小 | SHA-256 |
| --- | ---: | --- |
| worker 31 项协议与界面结果汇总 | 97073 B | `0d0c730a368fd928cc2adf2d48a04248c2af4ff0ddb8e51bc38b9f7b4e93adb7` |
| worker PTY 收尾回执 | 8193 B | `435159de3ddca66a33b6193811564e598a6abbc9494c9e88ad63c7e4442e6525` |
| paths 12 场景结果 | 16802 B | `4c3e09ab7d608cecab2b0616167cf3977c607d5d17abe7edf92fcefb31f99a8f` |
| 真实 `/plugin list` PTY 回执 | 6136 B | `2499fc60fefd696582d537c675df16fa0a5282cfe882d659a73a0eb2734fcde8` |
| 本轮功能验收候选二进制；非正式安装资产 | 237491592 B | `30c5cb0bf4f953d1ce8f32ecc386d5f23b5d9cb4c4a338b72bcd49fed0e5f09e` |
| v27 provenance 适配模块 | 43791 B | `74aaa82cea8889006aa214e4354160dc6f7974082d7206846cafd9487d36fe19` |

表中原始私有回执不随本文上传；这里只给出用于对应已核结果的大小与摘要。公开源码、合入和发布记录可直接访问：

- [正式源码 S61a](https://github.com/LIghtJUNction/ssmcp/commit/61a4de1cf522358691998ecb724b4a8518b91c47)
- [实测候选 b4d667](https://github.com/LIghtJUNction/ssmcp/commit/b4d6672420ba78f8ec045d37f95e388707b907ad)
- [PR181](https://github.com/LIghtJUNction/ssmcp/pull/181)
- [Release v0.10.27](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.27)
- [Release 工作流 45002392978](https://github.com/LIghtJUNction/ssmcp/actions/runs/45002392978)
