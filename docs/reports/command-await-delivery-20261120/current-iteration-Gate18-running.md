# SSMCP 0.10.25：Windows GNU 检查已通过，Gate18 继续运行

**记录截止：2026-11-21 11:28:04 UTC。** 原 35 阶段完整检查正在实际提交 **`b8d94f5e12788ed19d1b74eecdcc068ea9fc0078`**、tree **`1391701c52db5066e7a25bbe3a32c1d5a82c47d5`** 上运行，**前 33 个阶段均通过**。此前失败的 Windows GNU Clippy 已退出 **0**，耗时 **53.22 秒**，原 `-D warnings` 规则保持。这是针对新源码的实际验证；整套 Gate18 尚未完成。[Gate18 进展原件摘要](gate-Sb8d-r18-progress-public-facts.json)

本轮 private 库 **2101** 项、host **6** 项也已通过，结果来自新源码的原始 Gate 汇总。报告组此时尚未逐行读取本轮四个指定回归的日志，因此不将旧 Gate17 的逐行记录写成本轮的新读回。Windows GNU 的成功属于交叉编译静态检查，原生 Windows 运行不在这条证据范围内。

E15 平台修复通过 [PR #169](https://github.com/LIghtJUNction/ssmcp/pull/169) 于 **11:12:59 UTC** 正常合并；Git commit 时间为 11:12:58。真实父提交为 S401 与作者 `bef89f57aea0167b830096d596a2cb39fd86f5cd`，合并树与作者树一致。作者阶段只做格式和 Git 检查；之后的 Gate18 才提供本段新增的编译与测试结果。[E15 源码、作者范围与合并事实](source-E15-history-cfg-public-facts.json)

原 Gate16 的栈溢出及 S401 Gate17 的 Windows 失败保留为历史结果，均不覆盖或迁移。一次性重启恢复入口也已配置；其最近独立读取状态为 **11:15 UTC 尚未消费**，下一次重启后的图形登录与连接恢复仍待实测。[Gate17 失败](gate-S401-r17-failure-public-facts.json) · [恢复配置事实](reboot-once-prepared-public-facts.json)

## Gate18 的已完成阶段

下表列出此前阻塞点所在的前八阶段；其后的集成编译、19 个普通集成分区、独立 worker 入口和四个 mandatory ignored 阶段也已通过，总计 33 阶段。

| 阶段 | 退出码 | 原阶段耗时 | 实际结果 |
|---|---:|---:|---|
| release-policy | 0 | 11.34 秒 | 通过 |
| deployment-policy | 0 | 0.46 秒 | 通过 |
| fmt | 0 | 4.70 秒 | 通过 |
| library-compile | 0 | 141.06 秒 | 通过 |
| library-private | 0 | 44.56 秒 | 2101 passed、0 failed、26 ignored、6 filtered |
| library-host | 0 | 0.02 秒 | 6 passed、0 failed、0 ignored、2127 filtered |
| clippy | 0 | 88.84 秒 | 本机目标通过 |
| windows-gnu-clippy | 0 | 53.22 秒 | 原平台 helper 错误在本轮检查中解除 |

后续阶段实际记录：integration-compile 退出 0、84.04 秒；19 个普通集成分区合计 **101 passed**，独立 zuse worker 入口 **1 passed**；原定四个 ignored 阶段合计 **14 passed**，其中库 2 项、集成 12 项。它们来自本轮各分区的原始汇总，不包含未运行的其他 ignored 定义。

最后 **2 个阶段尚无完成结果**，这不等于它们全部未启动。原结果状态仍为 `running`，内层与外层结束回执当时均不存在；未启动下游运行、UI、多 UID、P1、两项补充 native ignored 或正式发布核验。

外层于 **11:18:15.727562 UTC** 启动，取得源码锁；内层实际取得 target 排他锁，九项启动输入哈希与准备清单一致。run_id 为 `3084787068b4155c51b6e440b6b66202`。原合同保持 **35 阶段、26 个 Rust 阶段、20 个 integration targets、14 个必跑 ignored 测试、6 个 host 库测试**。

报告组完整读取了两个原始 launch：owned launch 为 **1006 字节**、SHA-256 `4f3fc26174ae5e0de0b1dfc30f9d0b79630040fdd32ff428216ee319b27eac20`；内层 launch 为 **3636 字节**、SHA-256 `ab5a314e1b2df311e28a3bc2250e2ca97773160edac18f975afc0a6c2186ad9e`。11:28 的可变结果快照为 **1244873 字节**、SHA-256 `d35b62b0d664186f067cfcc9af2168fb490d60429c3ada1e86ad9c2e4ccbe95a`，报告读取了 33 个阶段的实际退出码、耗时和测试汇总。

准备清单是 **11:17:56 UTC** 的独立快照，保留其尚未启动的 false/null 字段；后来的 launch 才证明实际执行。helper 没有重新获取上游默认分支观察，不将既有观察时点写成当前最新。报告组及三个直接协作者已针对本次真实源码与新启动环境重新确认资源窗口，原历史确认没有冒充新确认。

## E15：使定义与调用者的平台范围一致

该辅助函数原先标为 `#[cfg(test)]`，意味着所有测试目标都会编入它；三处调用都位于 `device` 模块，而整个模块只在 Linux 编译。Windows 测试目标因此留下了没有调用者的定义，现有 `-D warnings` 规则将 dead_code 诊断作为错误。

E15 只修改定义上的条件：

```rust
#[cfg(all(test, target_os = "linux"))]
```

函数体、Linux 调用、测试断言和警告规则均保留。生产构建原本就不编入这个测试 helper；改动的作用是让测试定义与真实调用范围一致。报告组通过固定 GitHub 提交的完整差异核对了一行替换，并读取旧定义、三处调用及模块的 Linux 条件；没有把这一静态核对当作跨平台运行成功。[固定补丁](https://github.com/LIghtJUNction/ssmcp/commit/bef89f57aea0167b830096d596a2cb39fd86f5cd)

原作者沿原会话于 **11:00:18.362404 UTC** 启动，**11:02:38.850542 UTC** 退出 0，error=null，作者和 rustc 包装器身份均保持。作者先从 5a45be0 快进到已合并的 S401，两者树相同，再形成 bef89f5。工作树、索引和子模块干净，版本仍为 0.10.25。推送记录中的凭据存储 D-Bus 告警保留为非致命现象；普通 push 与随后远端引用读取实际退出 0，作者分支为 bef89f5。

| 本阶段记录 | 实际范围 |
|---|---|
| `rustfmt --check --edition 2024 --config skip_children=true src/client/interface/history.rs` | 退出 0，空输出；只做格式检查 |
| `git diff --check` | 退出 0，空输出 |
| 106 条 post_commit_git_checks | 原回执里的 Git 静态检查记录，全部退出 0；**不是 106 项测试** |
| Cargo、编译、产品、Rust 测试、Windows GNU 复查 | 原源码回执明确未执行 |
| 新增测试定义 | 0 |

Cargo manifest、锁文件、gitlink、十一项内部 Git 来源、六份发布配方，以及 host_control、zuse_backend、E2E 脚本均未改。H2-CI 的配方字节因此保持，新源码不要求重新生成一套仅换标签的核验器；未来运行仍须绑定真实的 S、P 和实际发行回执。

SSMCP 连接在本阶段结束后曾短暂不可用，随后已识别同一项目的新连接并恢复协调。报告组没有重载客户端或重新启动作者模型；旧源码回执、Gate16 和 Gate17 失败记录均保留。下面继续列明 E14 已合并改动与 S401 的实际 Gate17 结果。

## E15 原件身份

| 原件 | 字节数 | SHA-256 | 报告组读取 |
|---|---:|---|---|
| launch-receipt.json | 1927 | `313efd4feb5a8b618a745d137bf1abb69bdfcac4954c0602ae8307deaeeaed51` | 全文 |
| author-exit.json | 2087 | `6574e51b585bde69b991681d2939b3deb7f16912a0eb8b30b7d2550993950fa1` | 全文 |
| source-changes-receipt.json | 42150 | `db2be1c75eced427b99b655bc6c7d920308d812ac68acf8c3fe1bf6fbfd3c79a` | 全文哈希；修订、变更、保留项、scope、push 与远端读取等选定字段，未逐条审计全部静态命令输出 |
| author-own-fixes.full.diff | 685 | `9aa345f0dc47c047f6b59a51c42905a35b351066a708a3c473e6906278bda81c` | 身份由原回执记录；实际完整补丁另从 GitHub 固定 commit 读取 |

## 一次性重启恢复：已配置，尚未执行

原用户需求记录于 **11:00:48.686 UTC**，要求在重启后打开 ztty、进入项目目录并恢复 SSMCP 会话。源码协调者报告于 **11:10:48.341282 UTC** 完成配置及非启动型 `--check`，退出 0。报告组于 11:15 UTC 另行读取现有文件，没有再次运行脚本、模拟打开窗口或重启系统。

入口位于 `~/.config/autostart/ssmcp-resume-once.desktop`，恢复资料位于 `~/.local/state/work-maintainer/ssmcp-resume-once-20261121/`。原入口调用 `restore.py --autostart`；脚本使用现有的 `ztty-launch` 和已安装的 `ssmcp`，进入 SSMCP 项目目录后启动终端中的连接。没有安装产品、重载客户端或自动启动模型。

脚本比较当前 boot ID 与配置中的 armed boot ID：相同时跳过，只有下一次不同 boot 的图形登录才尝试启动；还要求项目目录、两个可执行文件和 DISPLAY 可用。非阻塞 flock 防止同时启动；已有 completed 标记则直接返回。读取时 armed boot 与当前 boot 相同，completed、恢复日志、终端启动日志和锁文件均不存在，因此本次入口仍未消费。

**completed 标记只表示启动了子进程并观察它存活一秒。** 它并不证明窗口已呈现、SSMCP 已连接，或旧 UI 会话已经选中；记录中 `connection_confirmed=false`。脚本只在桌面入口内容仍与保存版本相同时删除该入口，随后等待终端子进程退出。

当前准备的是恢复项目连接与同目录上下文。已安装 CLI 没有被配置为按原 UI history ID 自动选择旧会话；旧会话仍可通过 `/resume` 选择。保存的 `continuation.json` 是接续工作的检查点资料，脚本不会自动把它作为模型请求执行，也不会重放旧请求。当前连接已恢复，不能反推它由这份新脚本启动。

| 恢复文件 | 字节数 | SHA-256 | 报告组读取范围 |
|---|---:|---|---|
| ssmcp-resume-once.desktop | 395 | `07c79a14595a9283b75277c723b5f30f40a688dea5171ea25ef92cb0dda0d6f9` | 全文；与 config 保存入口一致 |
| restore.py | 3210 | `17338cd335ff5183fddc4b77399fab74c5f9cc2dd1f225d7cfbc920d80c64af9` | 全文静态读取，未执行 |
| config.json | 849 | `deee5596a0f7c7b49957214d82e99d92ff7cdc1bff1329dc6fe8dc492dfda621` | 全文哈希及路径可用性、boot、入口一致性等选定字段 |
| continuation.json | 3289 | `b788e2e1458df579ff7284bdbc7fb1968574a98167e358a1bc0b02bd052c7c19` | 全文哈希与顶层键，未逐项审阅全部检查点内容 |

恢复目录权限为 0700，配置、检查点和桌面入口为 0600。这里记录的是实际准备状态，下一次重启与图形登录的执行结果仍须另行确认，不作为 Gate18 的附加启动条件。

## E14 已合并改动及其作用

| 改动 | 解决的问题 | 本轮证据与边界 |
|---|---|---|
| Num Lock 清理状态 | 原生调用报错时，可能已有按键输入，不能直接认定释放完成 | 固定补丁审阅；两个 helper 回归在 Gate17 中实际通过；没有真实设备短写或 IPC 故障复现 |
| 独立 CI 的调试客户端上传条件 | 调试 artifact 的名称与正式发布下载通配符重叠 | 固定源码与官方工作流语义审阅；没有真实 Release 运行结果 |
| E2E 工具清单 | 新增工具需要进入精确名称集合 | 固定补丁审阅，名称／数量／schema 断言保留；脚本尚未执行 |
| E13 权限 future 包装及主分支浏览器改动 | 处理此前库测试栈溢出风险 | 原失败用例与小栈回归在 Gate17 中通过；没有将单次运行结果表述为完整根因证明 |

### Num Lock：错误不等于已释放

输入操作可能在发送按键按下后，才遇到后续写入或释放错误。因此，仅凭调用返回错误，不能证明系统没有产生输入，也不能证明释放已经完成。

[959782e](https://github.com/LIghtJUNction/ssmcp/commit/959782ed9c4da0b17e8b02746fa79d3db130fab1) 将 Num Lock 调用接入 `execute_num_lock`：先解析参数、检查取消状态和控制许可，再调用原生操作；原生操作一旦返回错误，先将 `cleanup_ok` 置为 `false`，再传播原错误。现有 worker 回复、父进程 `input_pending` 和清理等待路径继续处理未确认状态。

派发前取消或许可拒绝不会调用原生闭包，也不改写原清理状态；成功调用不会把此前的 `false` 恢复为 `true`。补丁没有加入重试、toggle 重放或扩大其他输入的预检白名单。报告组与独立审阅者核对了固定补丁和本地传播路径。

上游短写与回退释放的描述来自作者回执和源码审阅，未在真实设备上复现。两个新回归使用 helper 层合成操作：它们没有在 E14 窄检查时执行，随后在 S401 的 Gate17 中明确通过。这个先后关系在事实文件中分别记录。

### CI：保留严格发布集合

主分支此前加入 `ssmcp-linux-x86_64-candidate` 调试产物，其名称与 Release 的 `ssmcp-*` 下载通配符重叠。如果 Release 调用可复用 CI 时也上传该产物，调试文件会进入正式产物目录，随后不符合发布脚本的精确文件集合要求。

[662c34c](https://github.com/LIghtJUNction/ssmcp/commit/662c34c298fedc4b3ab28fc77015bd81f41f6ee4) 保留 artifact 名称、动作固定版本、路径及保留期限，只补充独立 CI 条件：

```yaml
if: matrix.runner == 'ubuntu-24.04' && github.event_name == 'workflow_dispatch' && github.workflow == 'CI'
```

GitHub 官方文档说明，可复用工作流中的 `github` 上下文来自调用方。由名为 `Release` 的工作流调用时，最后一项条件不成立。这是固定源码与官方语义支持的静态推断，**尚未观察真实 Release 中的跳过结果**。[官方上下文说明](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations#github-context)

Linux 两个 job 的上传数量仍须为 **2／1**，原下载通配符、严格文件集合和最终附件合同保持。没有给调试文件增加发布例外。

### E2E 与异步包装

[5a45be0](https://github.com/LIghtJUNction/ssmcp/commit/5a45be0401caf7fcd462fa1783c157e7f4c77019) 在 `scripts/e2e.py` 中加入跨平台的 `ssmcp.computer.watch_until_click`，以及 Linux 的 `move_path`、`keyboard_state`、`num_lock`。调用该集合的 `e2e_agents` 随之同步。精确名称、数量和输入 schema 检查保持；本次没有运行 E2E 脚本，不能将清单修正写成已完成 UI 回归。

E13 的 [e3d8f00](https://github.com/LIghtJUNction/ssmcp/commit/e3d8f00f559da097627988407ea6b14ec00e92f6) 在权限包装构造时使用 `Box::pin`，保留控制许可、租约、取消和完成后的复验路径。构造时机发生变化，`Box::pin` 也不能独自证明没有栈上临时值。原 25 秒产品期限与测试断言未放宽。主分支浏览器包装改动与此修复现在处于同一树；Gate17 的通过结果证明这些具体回归在本次配置下成功运行，没有隔离出各项改动的独立贡献。[E13 历史事实](source-E13-completed-stack-fix-public-facts.json)

## E14 源码、提交和两次窄检查

原作者沿原会话于 **10:26:57 UTC** 开始、**10:35:30 UTC** 正常退出。两次普通推送分别到达 3b76417 与 5a45be0，均退出 0。四个作者提交如下，不能把提交数量误写为推送次数。

| 提交 | 内容 | 范围 |
|---|---|---|
| [3b76417](https://github.com/LIghtJUNction/ssmcp/commit/3b7641756c511174bf9830d5cd32b769b9ea3edb) | 固定主分支 5f4a354 合入作者 e3d8f00 | ort 正常双父合并，无冲突 |
| [959782e](https://github.com/LIghtJUNction/ssmcp/commit/959782ed9c4da0b17e8b02746fa79d3db130fab1) | Num Lock 清理状态 | 77 行增加、7 行删除 |
| [662c34c](https://github.com/LIghtJUNction/ssmcp/commit/662c34c298fedc4b3ab28fc77015bd81f41f6ee4) | 独立 CI 条件 | 1 行替换 |
| [5a45be0](https://github.com/LIghtJUNction/ssmcp/commit/5a45be0401caf7fcd462fa1783c157e7f4c77019) | 精确 E2E 清单 | 7 行增加 |

导入的主分支增量包含六个提交、十四个路径；E14 合并后自己的三项修复涉及三个路径，共 **85 行增加、8 行删除**。完整阶段相对 e3d8f00 涉及十五个路径。工作树、索引和子模块干净，包版本仍为 0.10.25；没有改 Cargo manifest、Cargo.lock 或 gitlink。

PR #168 于 **10:38:37 UTC** 实际合并。报告组分别读取 PR、main 和合并提交：两个真实父提交为 `5f4a35482c6353750248ddee8761e5fd3daffad6` 与 `5a45be0401caf7fcd462fa1783c157e7f4c77019`，合并树与作者最终树相同。作者检查仍归属于作者提交；Gate17 则实际绑定合并后的 S401。

| 检查尝试 | 时间（UTC） | 原结果 |
|---|---|---|
| 首次 | 10:30:28.806–10:30:28.820 | `blocked`、退出 75；共享 target 锁忙，0 个命令启动，没有测试或 Cargo 检查结果 |
| 第二次 | 10:33:13.515–10:34:50.682 | 两个阶段通过、退出 0；输入和源码前后相同，自有后代清理完成 |

第二次执行 **106 项发布策略测试，输出 7.350 秒、OK**；阶段耗时约 7.626 秒。随后 `cargo check --tests --locked --offline --jobs 1` 耗时约 85.620 秒，Cargo 输出 **1 分 25 秒**。编译检查不执行 Rust 测试函数。

四份源码快照均为 **542,640 字节**，SHA-256 为 `1975b01c49fc6fe2fffcdcd538069d0925897b2acc0abbb276b6a5750ffd8bb9`，head=5a45be0、tree=be0e780、未跟踪文件数为零。原结果中的八项输入前后相同，锁保持到清理完成。报告组读取了这些字段和四份完整输出日志，没有重新逐字审阅四份完整源码快照。

首次锁忙没有取得源码快照，锁持有者也没有被可靠确定。启动回执里的 e3d8f00 是作者阶段初始身份，不能补写成首次检查已验证的源码指纹。[两次原记录与日志身份](source-E14-merged-public-facts.json)

## Gate17：通过的部分与实际失败

Gate17 实际执行时间为 **10:45:44.924–10:50:52.782 UTC**，run_id 为 `c8419923004aaefebff434397674ce47`。原合同继续为 35 阶段、26 个 Rust 阶段、20 个 integration targets、14 个必跑 ignored 测试和 6 个 host 库测试，阶段没有增减。

| 阶段 | 退出码 | 原阶段耗时 | 结果 |
|---|---:|---:|---|
| release-policy | 0 | 7.67 秒 | 通过 |
| deployment-policy | 0 | 0.36 秒 | 通过 |
| fmt | 0 | 3.06 秒 | 通过 |
| library-compile | 0 | 102.99 秒 | 通过 |
| library-private | 0 | 54.01 秒 | 2101 passed、0 failed、26 ignored、6 filtered |
| library-host | 0 | 0.02 秒 | 6 passed、0 failed、0 ignored、2127 filtered |
| clippy | 0 | 75.52 秒 | 本机目标通过 |
| windows-gnu-clippy | 101 | 52.26 秒 | 失败；后 27 阶段未运行 |

private 库父进程汇总报告 **53.99 秒**，与外层阶段耗时分别记录。日志中的三个子进程各自汇总没有重复加到 2101 中。报告组直接读到以下四行 `ok`：

- `browser::tests::act_deadline_includes_time_queued_without_starting_input`
- `browser::tests::browser_dispatch_future_fits_a_small_stack_budget`
- `computer::zuse_backend::tests::num_lock_pre_dispatch_cancellation_and_success_preserve_cleanup`
- `computer::zuse_backend::tests::num_lock_uncertain_native_write_marks_worker_cleanup_unconfirmed`

Windows GNU 目标的原致命错误为 `src/client/interface/history.rs:1077:15` 的 `resume_test_history` 未使用；`-D warnings` 将 dead_code 升为错误。日志还出现 qrcodegen manifest warning，它没有被误列为这次已记录的唯一编译错误。没有降低警告规则或跳过阶段。在 Gate17 的历史失败快照中，尚未读取后继修复的启动或结果；后继 E15 现已合并，本文上方单独记录了新源码在 Gate18 中的 Windows GNU 检查通过。

外层启动回执确认源码锁，内层 launch 确认 target 排他锁。结束回执显示九项固定输入前后相同，验收工作树干净并仍为 S401；内外层退出码均为 1。外层记录源码锁保持到子进程返回，这一字段本身不等于“所有后代均已独立证明回收”。

准备清单的 `execution_started=false`、锁 false、run_id null 是 **10:44:19 UTC 的启动前快照**；实际启动和结束由后来的原回执单独证明，没有改写旧快照。十一项 Git 默认分支来源保留 **09:36 UTC 的原观察**，Gate17 helper 没有重新获取，不宣称启动时再次确认了上游最新值。

## 后续验收范围

| 范围 | 当前状态 |
|---|---|
| E15 作者阶段 | bef89f5 一行修复，格式／Git 检查通过；没有运行编译或测试 |
| E14 作者窄检查 | 5a45be0 上第二次通过；第一次锁忙记录保留 |
| 新源码 Gate18 | b8d94f5 上运行中，前 33 阶段通过，最后 2 阶段尚无完成结果 |
| 历史 Gate17 | S401 上 7 通过、1 失败、27 未运行 |
| 原栈溢出与两个 Num Lock 回归 | S401 的 private 库阶段实际通过 |
| 同源码运行、UI、多 UID、P1 | 未执行 |
| `ssmcp --version --all` 实际命令 | 未执行 |
| 指针轨迹与 Chromium watch 两项补充 native ignored | 计划在同一最终源码和库 ELF 上另列执行；尚未运行 |
| H2-CI 发布核验 | 静态准备完成，未绑定、未执行 |
| 正式 release dispatch、安装、生产重载 | 未执行 |

Gate17 记录库 ELF 的 SHA-256 为 `152e01dbe95041023b72a7f4a41a47489caeb073e07d3baa2f272cae96c3af62`。报告组没有重新对 ELF 哈希；该值按原 Gate 回执归属保存。完整 Gate 失败后，没有据此启动下游消费者。

## H2-CI 静态交付与系统边界

发布核验器后继已固定在 [6f6bb09](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)，八个文件均从固定 commit 全文读回。它更新 CI 配方身份，并要求真实发布时两个 Linux job 的 **API step 20，`Preserve the Linux candidate client`，结论必须为 `skipped`**。当前 CI 为 15,351 字节，SHA-256 `d60f0b21ce4c16440d3dd39c1fb3d53fb21de3bb889a30c4ed4f8ae7822f9317`；另外五个配方文件固定身份不变。

**19 个 job、17 次初始 checkout、8 件必要 release artifacts、16 个公开文件**与 Linux 上传数量 2／1 的合同保持；dispatch 回执适配模块逐字不变。原 [H2 2e8e58a](https://github.com/LIghtJUNction/ssmcp/tree/2e8e58ac1a7047900feac8501878b80e64f4bd52/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-unbound)保留。

`recipe_source_commit=5a45be0…` 是配方审阅锚点，不是未来运行时 S／P 绑定。该配方字节在 S401 上相同。新版只完成 AST、接口和静态差异核对，没有 import、执行或 H2 测试，没有真实 Actions 日志兼容性验证；S、P、tag、run 和原始 dispatch 回执仍未绑定。

此前的本地启动错误处理、账户退出同步、会话元数据、网页命令、离线版本树和统一 Computer Use 开关继续保留，详细语义见 [E13 历史快照](current-iteration-E13-reviewed.md)。内部直接依赖继续使用 canonical Git 默认分支声明，锁文件记录本次实际来源；zuse 仍为 `ac5713c…`、0.3.0、只声明 std，没有启用 vision。[E12 来源事实](source-E12-merged-zuse-public-facts.json)

统一控制开关属于应用内许可机制，当前 **`os_permission_boundary=false`**。只要进程仍持有桌面连接、设备、浏览器控制入口或可执行替代路径，应用里的 Off 就不能单独证明操作系统已经收回全部能力。[操作系统权限边界草案](computer-use-permission-boundary-design-draft.md)讨论受信任启动器、完整后代域、管理授权、预算和撤权代次，目前仍是设计；没有修改生产账户、权限或隔离服务，也不是新 Gate 的附加启动条件。

## 历史成绩不跨源码转移

| 历史记录 | 原结果及保留范围 |
|---|---|
| W=4de1731…，Gate14 | 原 35 阶段通过；private 2057 passed、23 ignored、6 filtered；其余分区按原记录保存 |
| W 的运行验收 | 18 项通过，范围为 12 private＋6 transport |
| W 的 UI24 首次 | 7 个场景启动，6 个通过，共 503 个通过断言；账户兼容失败，17 个场景未运行；后来的单独 69 项账户诊断不并入或覆盖首次结果 |
| S647 的 Gate16 | 4 通过、1 栈溢出失败、30 未运行；没有完整 libtest 汇总，不能编造整批库测试数量 |
| e3d8f00 的 E13 | 两项窄检查通过；当时没有 Rust 测试执行 |
| S401 的 Gate17 | 本文新增的 7 通过、1 Windows lint 失败、27 未运行；独立记录具体库回归成功 |

Gate16 的原栈溢出仍保存于 [失败事实文件](gate-S647-r16-failure-public-facts.json)。旧 socket 烟测、缺少真实 relay ACK 的 `--input none` 失败、多 UID 准备脚本和未运行的 UI 场景也保留原范围，没有并入 S401 的通过结果。

## 原件身份与报告读取范围

原始日志可能含私有环境信息，公开交付只保存必要身份和选定字段。完整表见 [E14 事实](source-E14-merged-public-facts.json)与 [Gate17 事实](gate-S401-r17-failure-public-facts.json)。

| 原件 | 字节数 | SHA-256 | 本次读取 |
|---|---:|---|---|
| E14 author-exit.json | 2192 | `372ced9e531df6ab8af5af5cfc02b68c52ce112d57cd700dbff693f93f57c3f9` | 全文 |
| E14 source-changes-receipt.json | 57498 | `89b3d513cbe3a4bf4d376b6ebceee219924027416505454708051ccb8b86abfb` | 全文哈希；修订、合并、路径、推送和检查等字段 |
| E14 首次检查 result.json | 2748 | `f278c2cc6798ba3fad9739fa9d4dc51d144442e642276f7a1fec41af9d505fa9` | 全文 |
| E14 第二次检查 result.json | 89054 | `a2dd0104e8b153f515fc4535e283e7f4ce8881062a4b489c9161c10e8376f3e5` | 全文哈希；命令、时间、状态、输入、源码和清理字段 |
| Gate17 candidate-gates.json | 938126 | `c37f5e748a484abfb89304c3fbdd3266dd124d2cbed0971ffd570c7f764d2353` | 全文哈希；八个阶段、时间、测试汇总及身份 |
| Gate17 private 库日志 | 565943 | `c018ee01962443fd689f419d6c4205619dfb0ab029221bd01e0bd30821487a77` | 全文哈希；四条具体回归和父汇总，未逐行审计全部用例 |
| Gate17 Windows GNU 日志 | 1399 | `5131beda45373e29e8ce9a3b244e39afb6404890750f932d6492c9c8e95c08dc` | 全文哈希；致命诊断与相邻行 |
| Gate17 内层 exit.json | 4860 | `8d8ce512bd4147fd62cda8cb4f90e0fa7eb60e2b165f53355d0b791451629e92` | 全文 |
| Gate17 外层 exit.json | 1172 | `f1b48df6b888522ba57dd5332a08a8a6528dc0bb98b3c85abfc4bb365e40ad5f` | 全文 |

下一步由原检查控制器完成本次 35 阶段检查并核对终态、清理与二进制身份；后续运行和 UI 验收继续绑定同一源码及核实后的二进制。报告组继续只读核对原件并同步 GitHub 与 Google 文档，不并行启动产品构建或测试，也不把 S401 的成绩转记给后继提交。
