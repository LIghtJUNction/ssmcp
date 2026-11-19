# zharness / zplugin 正式配对检查点

本记录对应 [9c13277d1eacb68c2378f692a4d4660cb92f0c62](https://github.com/LIghtJUNction/ssmcp/commit/9c13277d1eacb68c2378f692a4d4660cb92f0c62)。SSMCP 的 zharness 0.5.1、zplugin 与宏包 0.3.1 已在独立分支更新并推送，锁文件来自真实 Cargo 解析。该检查点完成来源、依赖图和部分契约验证，供 Project Z 的四项组合接续；它没有完成整个组合的应用验收，也没有发布新应用版本。

## 固定来源

| 组件 | 版本 | 固定来源 |
|---|---|---|
| zharness | 0.5.1 | [e3580c01c7252da090d04e59b59762bc561777d1](https://github.com/LIghtJUNction/zharness/commit/e3580c01c7252da090d04e59b59762bc561777d1) |
| zplugin / zplugin-macros | 0.3.1 | [00d566eedcff7b2932e90b9dfeb44910c783d305](https://github.com/LIghtJUNction/zplugin/commit/00d566eedcff7b2932e90b9dfeb44910c783d305) |
| zsys | 0.1.1 | crates.io 精确版本，启用 dylib |

GitHub 正式 tag 解引用与以上提交一致。四个 crate 的 registry 记录均未撤回，实际下载归档的 SHA256 与 registry 摘要逐一相同；完整摘要见 [validation.json](validation.json)。应用版本仍为 0.10.14。zcall 0.3.0、zconv 0.4.0、ztuio 0.1.0 保持不变；此分支的 zuse-computer 仍为 0.1.0。

应用继续使用 zplugin 的 std、macros 特性以及 zharness 的 std 特性。根级 patch 将 zharness 的 zplugin 统一到固定子模块；zsys 使用正式 registry 来源。zplugin 的系统加载层改为复用 zsys，原生接口与显式关闭、默认保留映像的约定不变。上游 ABI 文档明确，协作关闭失败可保留映像，不能把超时解释成强制卸载或副作用回滚。

## 已完成验证

| 验证 | 实际结果 |
|---|---|
| 精确来源与下载归档 | 四个发布包摘要匹配；正式 tag 指向预期提交 |
| Cargo 更新与完整依赖图 | locked 解析通过；zharness、zplugin、宏包、zsys、zcall、zconv 各一份 |
| 应用发布 Python 门禁 | 73 通过，0 失败 |
| 独立依赖 registry 预检 | 通过；仍只由应用仓库发布 ssmcp |
| zplugin 库及集成测试 | 75 通过，10 个测试套件，exit 0 |

zplugin 的实际命令启用 std、macros、hot-reload、wasm，并串行执行库与集成测试。RTK 记录测试时间为 0.18 秒，整个步骤含排队和编译为 466.997 秒。这条回执只覆盖该独立库命令；它不代表新配对下的 SSMCP 原生插件端到端测试、全部上游动态库测试或跨平台测试已经完成。

Cargo.lock 的 SHA256 为 ad9899c43afba2598c8906126f07061951f91d980a6983abbb1cae45d2bc2722。来源阶段的独立记录保留在 [source-checkpoint.json](source-checkpoint.json)，后续执行与接手范围以本页及 validation.json 为准。

## 取消与接手

本次交接时，用户确定由 Project Z 聚合克隆中的 coord/shared-cohort-20261120 统一 zplugin 0.3.1、zharness 0.5.1、zuse-computer 0.1.1、ztuio 0.1.0。当前分支仅包含前两项配对，因此保留 9c13277 供聚合 owner 复用，四项收敛与最终测试由该 owner 接续。

在继承的全局编译串行锁排队期间，zplugin 步骤随后完成；接着开始的 zharness 步骤按上述分工取消。取消没有产生 zharness 测试结果，既不记为通过，也不记为测试失败。新 SSMCP Clippy、产品构建、libtest 和新配对原生插件验收均未启动；没有另行触发应用发布。

取消只作用于本次独立测试作业。双流已经 EOF，自有进程已回收，自有 target 锁已释放，其他项目进程和锁文件未改动。旧 v0.10.14 冻结 libtest 与 fixture 再次计算的 SHA256 仍与原记录一致。原始图和执行日志留在私有验收目录，公开记录仅保留结果、命令与摘要。

## 传播维护原则与剩余工作

底层库更新的维护者需要先枚举直接和间接反向依赖，协调现有 owner，再传播精确版本、固定源码和锁文件，完成相应验证并及时提交推送。维护记录必须持续到传播闭环，或列明实际阻碍和接手人；只发布底层库或发送通知不构成完成。新下游应登记到 Project Z 图，并使用其 impact 查询核对影响范围。

本次剩余工作已有明确接手人：Project Z 聚合 SSMCP owner，分支 coord/shared-cohort-20261120。它需要合入两项配对检查点，保留自己确定的 zuse-computer 0.1.1 来源，重新核对最终四项图中无重复 zplugin/zsys，再完成独立 zharness native/Wasm 契约与 SSMCP 集成验收。最终应用版本、正式发布、安装套件和服务部署状态由同一传播链分别记录，不能从本检查点推断已完成。

合并 9c13277 时，Cargo.toml 与 Cargo.lock 的冲突应依据最终四项来源重新解析，不手写摘要。该提交没有修改 zuse 或 ztuio 的 gitlink；保留聚合分支对这些组件的更新。若依赖说明同时新增 zuse 一节，保留该结构与本记录的配对版本、来源和生命周期说明。

后续还收到 oh-my-ztty 套件 owner 的协同回执，提出继续传播 zuse-computer 0.2.0，并等待含该版本的正式 SSMCP tag、源码及 Linux 两种架构的归档摘要和 ABI。该请求已转交主协调与聚合 owner；本检查点没有核验或集成 0.2.0，也不把原定 0.1.1 组合当成这项后续传播已经完成。最终目标以聚合 owner 核实后的不可变来源及发布回执为准。
