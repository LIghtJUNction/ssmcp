# SSMCP 0.10.25：Gate18 原 35 阶段全部通过，后续验收已交接

**Gate18 终态日志核对于 2026-11-21 11:42:49 UTC；恢复检查点于 11:46 UTC、H2 模块于 11:48 UTC、N18 证明于 11:50–11:51 UTC 补充读取。** 原 35 阶段完整检查已在提交 **`b8d94f5e12788ed19d1b74eecdcc068ea9fc0078`**、tree **`1391701c52db5066e7a25bbe3a32c1d5a82c47d5`** 上完成，**35／35 阶段全部退出 0，终态为 `passed`**。内层启动器与外层控制器的结束回执均为 0；九项固定输入前后相同，结束时工作树干净且仍为同一提交。[Gate18 终态事实](gate-Sb8d-r18-final-public-facts.json)

本轮实际结果包括 private 库 **2101 passed**、host **6 passed**、19 个普通集成分区 **101 passed**、独立 worker 入口 **1 passed**，以及原定四个 mandatory ignored 阶段合计 **14 passed**。发布策略与部署策略分别为 **106 项、35 项，均 OK**。这些是各自分区的结果；35 是阶段数，26 是 Rust 结果阶段数，均不是测试用例数。子进程汇总和已包含在 private 库中的四个指定回归不再重复相加。

此前阻塞 Gate17 的 Windows GNU Clippy 在本轮 **53.22 秒、退出 0**，原 `-D warnings` 保持。报告组还从本轮原日志读到两项浏览器回归和两项 Num Lock helper 回归各自的 `ok`。Windows GNU 成功属于交叉编译检查，不代表原生 Windows 运行验收；日志仍有 qrcodegen manifest 告警，不能写成“完全没有告警”。

源码协调者于 **11:39 UTC** 报告已将原消费窗口交接给验收组。既有顺序仍是 **N18 一次来源证明先通过，再串行执行原 runtime18、UI24、controls、多 UID、版本命令、P1 及两项 native 补充**。报告组现已读回 N18 的原始证明与结果：只读核对通过，实际验证调用一次，产品测试和 Cargo 调用均为 0。后续消费者结果仍待原件核对；缺少其回执不代表一定尚未启动，也不能写成已经通过。

E15 平台修复经 [PR #169](https://github.com/LIghtJUNction/ssmcp/pull/169) 于 **11:12:59 UTC** 正常合并；Git commit 时间为 11:12:58。真实父提交为 S401 与作者 `bef89f57aea0167b830096d596a2cb39fd86f5cd`，合并树与作者树一致。作者阶段只做格式和 Git 检查；随后 Gate18 才提供本次合并源码的编译与测试结果。[E15 来源与合并事实](source-E15-history-cfg-public-facts.json)

原 Gate16 栈溢出与 S401 Gate17 Windows 失败继续保留。一次性恢复检查点虽已更新，恢复脚本仍未自然触发；H2-CI 三个固定模块虽已在开发机准备，仍未导入或执行。当前还没有正式 release dispatch、安装或生产重载结果。

## Gate18：原 35 阶段完整结果

外层于 **11:18:15.727562 UTC** 启动；内层 driver 于 **11:18:16.104 UTC** 开始，于 **11:34:58.577 UTC** 完成；外层于 **11:34:58.717774 UTC** 正常结束。原合同保持 **35 阶段、26 个 Rust 结果阶段、20 个 integration targets、14 个必跑 ignored 测试、6 个 host 库测试**，没有增减阶段。

| 序号 | 阶段 | 退出码 | 原阶段耗时 | 实际结果 |
|---|---|---:|---:|---|
| 1 | release-policy | 0 | 11.34 秒 | 106 项策略测试，OK；用例汇总 11.067 秒 |
| 2 | deployment-policy | 0 | 0.46 秒 | 35 项策略测试，OK；用例汇总 0.187 秒 |
| 3 | fmt | 0 | 4.70 秒 | 通过 |
| 4 | library-compile | 0 | 141.06 秒 | 库测试产物编译通过 |
| 5 | library-private | 0 | 44.56 秒 | 2101 passed、0 failed、26 ignored、6 filtered |
| 6 | library-host | 0 | 0.02 秒 | 6 passed、0 failed、2127 filtered |
| 7 | clippy | 0 | 88.84 秒 | 本机目标通过 |
| 8 | windows-gnu-clippy | 0 | 53.22 秒 | Windows GNU 交叉编译检查通过，保持 -D warnings |
| 9 | integration-compile | 0 | 84.04 秒 | 集成测试产物编译通过 |
| 10 | integration-allow_account | 0 | 0.10 秒 | 3 passed、0 failed |
| 11 | integration-cc_backend | 0 | 1.15 秒 | 6 passed、0 failed、1 ignored |
| 12 | integration-client_resources | 0 | 0.11 秒 | 1 passed、0 failed |
| 13 | integration-client_server_config | 0 | 0.43 秒 | 5 passed、0 failed |
| 14 | integration-codex_backend | 0 | 20.81 秒 | 14 passed、0 failed、2 ignored |
| 15 | integration-collaboration | 0 | 0.15 秒 | 3 passed、0 failed |
| 16 | integration-connection_context | 0 | 0.21 秒 | 10 passed、0 failed |
| 17 | integration-context | 0 | 0.21 秒 | 12 passed、0 failed |
| 18 | integration-context_helpers | 0 | 0.01 秒 | 0 passed、0 failed、2 ignored |
| 19 | integration-context_runtime | 0 | 0.16 秒 | 1 passed、0 failed |
| 20 | integration-oauth_security | 0 | 1.15 秒 | 11 passed、0 failed |
| 21 | integration-pi_backend | 0 | 0.28 秒 | 9 passed、0 failed、1 ignored |
| 22 | integration-release_client | 0 | 0.08 秒 | 4 passed、0 failed |
| 23 | integration-resources | 0 | 0.26 秒 | 10 passed、0 failed |
| 24 | integration-rsync | 0 | 0.18 秒 | 3 passed、0 failed |
| 25 | integration-server_runtime | 0 | 0.03 秒 | 3 passed、0 failed、1 ignored |
| 26 | integration-terminals | 0 | 0.73 秒 | 4 passed、0 failed |
| 27 | integration-two_tools | 0 | 0.07 秒 | 2 passed、0 failed、7 ignored |
| 28 | integration-workspace_bridge | 0 | 0.01 秒 | 0 passed、0 failed、3 ignored |
| 29 | integration-zuse_worker_entry | 0 | 0.26 秒 | 1 passed、0 failed |
| 30 | library-code-mode-ignored | 0 | 0.33 秒 | 2 passed、0 failed、2131 filtered |
| 31 | integration-two_tools-ignored | 0 | 2.18 秒 | 7 passed、0 failed、2 filtered |
| 32 | integration-workspace_bridge-ignored | 0 | 2.89 秒 | 3 passed、0 failed |
| 33 | integration-context_helpers-ignored | 0 | 0.25 秒 | 2 passed、0 failed |
| 34 | release | 0 | 359.88 秒 | 本地 release 产物构建通过 |
| 35 | decoder | 0 | 129.48 秒 | VT 解码器产物构建通过 |

private 库父汇总报告 **44.53 秒**，外层阶段耗时为 44.56 秒，两者分别记录。原日志另外出现三个子进程各自的 1 passed 汇总，未再加到 2101 中。private 的 **26 ignored** 和普通集成分区合计 **17 ignored** 保留原摘要；后续明确执行的 14 项 mandatory ignored 单列为库 2 项、集成 12 项，不倒改先前汇总，也不声称其他 ignored 定义都已运行。

报告组从本次 S_b8d 的 private 原日志逐条读到以下四项各一次 `... ok`，它们属于 2101 的子集：

- `browser::tests::browser_dispatch_future_fits_a_small_stack_budget`
- `browser::tests::act_deadline_includes_time_queued_without_starting_input`
- `computer::zuse_backend::tests::num_lock_pre_dispatch_cancellation_and_success_preserve_cleanup`
- `computer::zuse_backend::tests::num_lock_uncertain_native_write_marks_worker_cleanup_unconfirmed`

原失败用例在这个源码和配置下通过，可以据此确认该回归结果；它仍不提供原栈溢出的崩溃帧或各项异步包装改动的独立贡献。两项 Num Lock helper 用例也没有替代真实设备短写、IPC 故障或后续控制验收。`release`、`decoder` 两行属于本地构建阶段，其退出 0 不等于正式 Release 工作流或对外发布已经运行。

### 来源、退出回执与五个产物

外层取得源码锁，内层绑定启动器取得 target 排他锁。九项启动输入哈希匹配准备清单，结束回执确认九项输入前后相同、源码仍为 S_b8d 且干净。源码锁保持至子进程返回，内层记录共享 target 所有者锁覆盖整批；这些字段不等同于独立证明所有后代均已回收。

源码／验收组报告实际核对了以下五个私有产物文件的字节数与 SHA-256。**报告组读取了原 Gate 的库产物元数据，没有独立重新哈希这五个文件**；此表按原提供者的读取范围保存，既有 N18 的只读来源证明现已通过，后续消费者须继续使用它绑定的产物。

| 产物角色 | 字节数 | SHA-256 |
|---|---:|---|
| library | 171932288 | `f329e613d4bbc4fccf5b380107e4e8cbf5f467343c3fcc75453e6299fb40f6c0` |
| ssmcp | 16248008 | `b5113562137983fee8e1e35914e0f5d90e3acc69eecd68473a014ff676d1e794` |
| ssmcpd | 8395024 | `5c3a87c059aaafac8620c471773a764a1680f17ec227db2e8cf5ed3b695d98cb` |
| ssmcp-frontend | 1386584 | `d3069119110ea265a021ae654dfb783d9db4c32b1f2f4e5f4758fc4ee7c8cbda` |
| vt_snapshot | 440984 | `efdbeb3f200fcf4404b2612d836433047ecc90eb0744f345fc971ee7441db97f` |

源码协调者另于 **11:40 UTC** 报告，独立的来源、依赖与编译元数据审阅为 READY：26 个选定 compiler artifacts 与 26 份 invocation 记录对应，`fresh=false`，链接与私有副本元数据相符，源码和依赖元数据前后一致。报告组尚未逐份二次审阅全部 invocation；这项已报告的审阅结论与下面单列的 N18 原始证明分别记录，均不能代替消费者运行通过结论。

### N18：原只读来源证明通过

报告组读回原 `proof.json` 与 `receipt.json`，结果为 **`passed_readonly_parse`**；开始于 **11:44:36.589 UTC**，结束于 **11:44:40.069 UTC**。回执记录 **`validation_invocations=1`、`errors=[]`、`product_test_count=0`、`cargo_invocations=0`**。其中 `execution_authorized=false` 限定这个只读验证入口没有执行产品，不撤销另行交接给原验收组的消费者窗口。

证明的 source path 与 compilation metadata 的 workspace root 均指向本次 Gate18 导出的 source，提交为 S_b8d、tree 为 1391701，库 SHA-256 为本次 f329e613…。回执保存 **16 项 fixed inputs 和 6 项控制文件的 before 记录**；它没有 `*_after` 字段，因此没有用不存在的字段编造前后相等或失败结果。报告组只读取原件，没有重跑验证器或产品。

| N18 原件 | 字节数 | SHA-256 | 报告组读取范围 |
|---|---:|---|---|
| proof.json | 638379 | `3c6c57e9ca83b30d24e30c385629cc50f3b6424a495e05b16b1974d02240ca54` | 完整原件哈希；源码、库身份及选定编译元数据 |
| receipt.json | 68141 | `ea0efb3d1d635c1f40cbbcccef256a4950b310addd826b7c3730cc03415740c5` | 完整原件哈希；状态、时间、调用次数、错误及输入／控制文件身份 |

N18 是来源核对，不计作产品测试；runtime18、UI24、独立控制、多 UID、版本命令、P1 与两项 native 补充仍须各自的实际结果。

### 终态原件与历史进展快照

| 原件 | 字节数 | SHA-256 | 报告组读取范围 |
|---|---:|---|---|
| Gate18 终态 candidate-gates.json | 1309336 | `a3d70ea494cd3ac0d3a1581c45426030f4d4a75484a4deb97e5b35ab84e534ad` | 完整原件哈希；全部 35 阶段结果、来源身份与选定产物字段 |
| Gate18 内层 exit.json | 4804 | `01ad8335859b3b8e0818de24d64089d6ff54138e2ab5bd2628e3707979a592f3` | 全文 |
| Gate18 外层 exit.json | 1170 | `19a7c520b912743f90afab09f29939c1f7ea54e515a3e977dae91ddbc0df90c1` | 全文 |
| candidate2-release-policy.log | 22851 | `88ba53eea3a6ae81d19448901aadb73569d604b320a339a7aa49ef6fe16a8702` | 完整原件哈希；实际父汇总、指定回归或相关诊断，未逐行审计全部日志 |
| candidate2-deployment-policy.log | 5660 | `d2532d7ece44f770692e0bffd7187ba3d8e3331c6d8eda2350357838833fb36e` | 完整原件哈希；实际父汇总、指定回归或相关诊断，未逐行审计全部日志 |
| candidate2-library-private.log | 565863 | `99944965e4c1d9b215b7e99999479817d4ad59fea8ad2853a9ce8d40907ed864` | 完整原件哈希；实际父汇总、指定回归或相关诊断，未逐行审计全部日志 |
| candidate2-library-host.log | 2069 | `6f364fc5c853f32741fb3d62a95a82c3cabe5733497da18fc245292d9ebe5d94` | 完整原件哈希；实际父汇总、指定回归或相关诊断，未逐行审计全部日志 |
| candidate2-windows-gnu-clippy.log | 497 | `df618404c558cb6816e8dedd837a977a78942cff94172c1254e6392ecdbe3655` | 完整原件哈希；实际父汇总、指定回归或相关诊断，未逐行审计全部日志 |

报告组此前完整读取的 owned launch 为 **1006 字节**、SHA-256 `4f3fc26174ae5e0de0b1dfc30f9d0b79630040fdd32ff428216ee319b27eac20`；内层 launch 为 **3636 字节**、SHA-256 `ab5a314e1b2df311e28a3bc2250e2ca97773160edac18f975afc0a6c2186ad9e`。

**11:17:56 UTC** 的准备清单继续保留尚未启动的 false／null 字段；后来的启动与结束原件分别证明实际执行和完成。11:25 的前八阶段通过、11:28 的前 33 阶段通过均是当时的进展快照；11:28 可变结果为 **1,244,873 字节**、SHA-256 `d35b62b0d664186f067cfcc9af2168fb490d60429c3ada1e86ad9c2e4ccbe95a`，不覆盖上表终态原件。helper 没有重新获取上游默认分支观察，不将既有观察时点写成当前最新。报告组保持 0 重任务，不并行占用原执行者窗口。

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

## 一次性重启恢复：检查点已更新，脚本尚未自然触发

原用户需求记录于 **11:00:48.686 UTC**，要求在重启后打开 ztty、进入项目目录并恢复 SSMCP 会话。源码协调者报告于 **11:10:48.341282 UTC** 完成配置及非启动型 `--check`，退出 0。报告组于 11:15 UTC 另行读取现有文件，没有再次运行脚本、模拟打开窗口或重启系统。

入口位于 `~/.config/autostart/ssmcp-resume-once.desktop`，恢复资料位于 `~/.local/state/work-maintainer/ssmcp-resume-once-20261121/`。原入口调用 `restore.py --autostart`；脚本使用现有的 `ztty-launch` 和已安装的 `ssmcp`，进入 SSMCP 项目目录后启动终端中的连接。没有安装产品、重载客户端或自动启动模型。

脚本比较当前 boot ID 与配置中的 armed boot ID：相同时跳过，只有下一次不同 boot 的图形登录才尝试启动；还要求项目目录、两个可执行文件和 DISPLAY 可用。非阻塞 flock 防止同时启动；已有 completed 标记则直接返回。11:15 UTC 读取时，armed boot 与当前 boot 相同，completed、恢复日志、终端启动日志和锁文件均不存在，入口当时尚未消费；后续检查点更新也没有触发恢复脚本。

**completed 标记只表示启动了子进程并观察它存活一秒。** 它并不证明窗口已呈现、SSMCP 已连接，或旧 UI 会话已经选中；记录中 `connection_confirmed=false`。脚本只在桌面入口内容仍与保存版本相同时删除该入口，随后等待终端子进程退出。

当前准备的是恢复项目连接与同目录上下文。已安装 CLI 没有被配置为按原 UI history ID 自动选择旧会话；旧会话仍可通过 `/resume` 选择。保存的 `continuation.json` 是接续工作的检查点资料，脚本不会自动把它作为模型请求执行，也不会重放旧请求。当前连接已恢复，不能反推它由这份新脚本启动。

| 恢复文件 | 字节数 | SHA-256 | 报告组读取范围 |
|---|---:|---|---|
| ssmcp-resume-once.desktop | 395 | `07c79a14595a9283b75277c723b5f30f40a688dea5171ea25ef92cb0dda0d6f9` | 全文；与 config 保存入口一致 |
| restore.py | 3210 | `17338cd335ff5183fddc4b77399fab74c5f9cc2dd1f225d7cfbc920d80c64af9` | 全文静态读取，未执行 |
| config.json | 849 | `deee5596a0f7c7b49957214d82e99d92ff7cdc1bff1329dc6fe8dc492dfda621` | 全文哈希及路径可用性、boot、入口一致性等选定字段 |
| continuation.json 原检查点 | 3289 | `b788e2e1458df579ff7284bdbc7fb1968574a98167e358a1bc0b02bd052c7c19` | 11:15 UTC 的完整原件哈希与顶层键；旧副本保留 |
| continuation.json 第二版检查点 | 6119 | `4211245cfd7ad4bc91e42461fadd96c52adc05d13a41955f00ab48c781fefa96` | 11:31:38.867175 UTC 的历史检查点；旧副本保留 |
| continuation.json 当前检查点 | 10806 | `7c01b462a6c7581b6881ae1f3de26f5ae2692b14f13eaf815cdc3cc45fccff37` | 11:45:19.589019 UTC 更新，11:46 UTC 读回；记录 Gate18 通过和有条件交接，未执行恢复脚本 |

11:15 UTC 的准备核对中，恢复目录权限为 0700，原配置、检查点和桌面入口为 0600。原 3289 字节与第二版 6119 字节副本均保留；当前 10806 字节检查点来源于 **11:43:24.097240 UTC** 保存的协调检查点（33193 字节，SHA-256 `92ad33dfc5a06c7dea37fd68e036926001972b166719b9fd77c6ca909541acb8`），记录 Gate18 的 35 阶段通过与原消费者有条件交接。

报告组于 **11:46 UTC** 只读核对：restore.py 与 config.json 的字节数和哈希未变，completed.json、recovery.log、terminal-startup.log 仍不存在。更新检查点不等于实际恢复成功；下一次重启与图形登录的执行结果仍须另行确认，不作为 Gate18 或下游验收的附加启动条件。

## E14 已合并改动及其作用

| 改动 | 解决的问题 | 本轮证据与边界 |
|---|---|---|
| Num Lock 清理状态 | 原生调用报错时，可能已有按键输入，不能直接认定释放完成 | 固定补丁审阅；两个 helper 回归分别在 S401 Gate17、S_b8d Gate18 中通过；没有真实设备短写或 IPC 故障复现 |
| 独立 CI 的调试客户端上传条件 | 调试 artifact 的名称与正式发布下载通配符重叠 | 固定源码与官方工作流语义审阅；没有真实 Release 运行结果 |
| E2E 工具清单 | 新增工具需要进入精确名称集合 | 固定补丁审阅，名称／数量／schema 断言保留；E14 作者阶段未执行脚本，本次下游结果待核 |
| E13 权限 future 包装及主分支浏览器改动 | 处理此前库测试栈溢出风险 | 原失败用例与小栈回归分别在 Gate17、Gate18 中通过；没有将回归结果表述为完整根因证明 |

### Num Lock：错误不等于已释放

输入操作可能在发送按键按下后，才遇到后续写入或释放错误。因此，仅凭调用返回错误，不能证明系统没有产生输入，也不能证明释放已经完成。

[959782e](https://github.com/LIghtJUNction/ssmcp/commit/959782ed9c4da0b17e8b02746fa79d3db130fab1) 将 Num Lock 调用接入 `execute_num_lock`：先解析参数、检查取消状态和控制许可，再调用原生操作；原生操作一旦返回错误，先将 `cleanup_ok` 置为 `false`，再传播原错误。现有 worker 回复、父进程 `input_pending` 和清理等待路径继续处理未确认状态。

派发前取消或许可拒绝不会调用原生闭包，也不改写原清理状态；成功调用不会把此前的 `false` 恢复为 `true`。补丁没有加入重试、toggle 重放或扩大其他输入的预检白名单。报告组与独立审阅者核对了固定补丁和本地传播路径。

上游短写与回退释放的描述来自作者回执和源码审阅，未在真实设备上复现。两个新回归使用 helper 层合成操作：它们没有在 E14 窄检查时执行，随后在 S401 的 Gate17 中明确通过，本轮 S_b8d Gate18 又分别读到通过原行。各次执行按源码独立记录，没有把 helper 合成操作表述为真实设备复现。

### CI：保留严格发布集合

主分支此前加入 `ssmcp-linux-x86_64-candidate` 调试产物，其名称与 Release 的 `ssmcp-*` 下载通配符重叠。如果 Release 调用可复用 CI 时也上传该产物，调试文件会进入正式产物目录，随后不符合发布脚本的精确文件集合要求。

[662c34c](https://github.com/LIghtJUNction/ssmcp/commit/662c34c298fedc4b3ab28fc77015bd81f41f6ee4) 保留 artifact 名称、动作固定版本、路径及保留期限，只补充独立 CI 条件：

```yaml
if: matrix.runner == 'ubuntu-24.04' && github.event_name == 'workflow_dispatch' && github.workflow == 'CI'
```

GitHub 官方文档说明，可复用工作流中的 `github` 上下文来自调用方。由名为 `Release` 的工作流调用时，最后一项条件不成立。这是固定源码与官方语义支持的静态推断，**尚未观察真实 Release 中的跳过结果**。[官方上下文说明](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations#github-context)

Linux 两个 job 的上传数量仍须为 **2／1**，原下载通配符、严格文件集合和最终附件合同保持。没有给调试文件增加发布例外。

### E2E 与异步包装

[5a45be0](https://github.com/LIghtJUNction/ssmcp/commit/5a45be0401caf7fcd462fa1783c157e7f4c77019) 在 `scripts/e2e.py` 中加入跨平台的 `ssmcp.computer.watch_until_click`，以及 Linux 的 `move_path`、`keyboard_state`、`num_lock`。调用该集合的 `e2e_agents` 随之同步。精确名称、数量和输入 schema 检查保持；E14 作者阶段没有运行 E2E 脚本，清单修正本身不能写成已完成 UI 回归。

E13 的 [e3d8f00](https://github.com/LIghtJUNction/ssmcp/commit/e3d8f00f559da097627988407ea6b14ec00e92f6) 在权限包装构造时使用 `Box::pin`，保留控制许可、租约、取消和完成后的复验路径。构造时机发生变化，`Box::pin` 也不能独自证明没有栈上临时值。原 25 秒产品期限与测试断言未放宽。主分支浏览器包装改动与此修复现在处于同一树；Gate17 的通过结果证明这些具体回归在 Gate17 原配置下成功运行，没有隔离出各项改动的独立贡献。[E13 历史事实](source-E13-completed-stack-fix-public-facts.json)

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

## 历史 Gate17：通过的部分与实际失败

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

| 范围 | 本快照状态 |
|---|---|
| E15 作者阶段 | bef89f5 一行修复；只做格式／Git 检查，无编译或测试；已正常合并为 S_b8d |
| E14 作者窄检查 | 5a45be0 上第二次通过；首次锁忙、0 命令记录保留 |
| 本轮 Gate18 | S_b8d 上 35／35 阶段通过，两个结束回执均为 0，输入和源码前后相同 |
| 历史 Gate17 | S401 上 7 通过、1 失败、27 未运行；没有被后继结果改写 |
| 四个指定库回归 | 本次 S_b8d 原日志逐条为 ok，已包含在 private 2101 中；S401 原记录另存 |
| N18 一次来源证明 | 原证明与结果已读回，passed_readonly_parse；一次调用、0 产品、0 Cargo、errors=[] |
| 原 runtime18／UI24／controls／多 UID／P1 | 仅按已交接状态记录；各消费者是否启动、完成及通过，以后续原件为准 |
| `ssmcp --version --all` 实际命令 | 在原消费范围内；尚未取得本次绑定源码的通过原件核验 |
| 指针轨迹与 Chromium watch 两项 native ignored 补充 | 保持同一最终源码和库 ELF 的原计划；结果仍待独立原件核对，不并入本轮 14 mandatory ignored |
| H2-CI 发布核验 | 三个固定模块已准备且哈希匹配；仍未绑定、未导入、未执行 |
| 正式 release dispatch、安装、生产重载 | 未执行 |

这次交接沿用既有 N18 与消费者次序，没有新建测试条件或扩大本轮阶段。尚无报告组独立通过回执不能反推验收组没有开始工作；同样，交接通知、来源审阅 READY 和 N18 只读通过均不能替代消费者运行结果。

历史 Gate17 库 ELF 的 SHA-256 为 `152e01dbe95041023b72a7f4a41a47489caeb073e07d3baa2f272cae96c3af62`，报告组未重新哈希，按原 Gate 回执保存。Gate17 完整检查失败后未据此启动下游消费者；当前交接使用 Gate18 新产物身份，二者不混用。

## H2-CI 静态交付与系统边界

发布核验器后继已固定在 [6f6bb09](https://github.com/LIghtJUNction/ssmcp/tree/6f6bb09bace5b54c82f47f7eb0ae245f17d5ff2d/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-ci-unbound)，八个文件均从固定 commit 全文读回。它更新 CI 配方身份，并要求真实发布时两个 Linux job 的 **API step 20，`Preserve the Linux candidate client`，结论必须为 `skipped`**。当前 CI 为 15,351 字节，SHA-256 `d60f0b21ce4c16440d3dd39c1fb3d53fb21de3bb889a30c4ed4f8ae7822f9317`；另外五个配方文件固定身份不变。

**19 个 job、17 个成功与 2 个跳过（crate publication／生产部署）、17 次初始 checkout、8 件必要 release artifacts、16 个公开文件**与 Linux 上传数量 2／1 的合同保持；辅助 CI 证据不混入这八件必要发布产物，dispatch 回执适配模块逐字不变。原 [H2 2e8e58a](https://github.com/LIghtJUNction/ssmcp/tree/2e8e58ac1a7047900feac8501878b80e64f4bd52/docs/reports/command-await-delivery-20261120/publication-verifier-git-source-unbound)保留。

`recipe_source_commit=5a45be0…` 是配方审阅锚点，不是未来运行时 S／P 绑定。E15 没有改变这六份配方；原静态审阅不因此变成实际发布验证。新版只完成 AST、接口和静态差异核对，没有 import、执行或 H2 测试，没有真实 Actions 日志兼容性验证；S、P、tag、run 和原始 dispatch 回执仍未绑定。

源码侧已准备三个来自固定 6f6bb09 的模块，写回字节数和 SHA-256 逐项匹配，旧模块目录未动。**11:48 UTC，报告组只读核对开发机三个模块的字节数、SHA-256 与 0444 模式，均与固定版本匹配；未导入或执行。** 它们不是新合同，也不是已经运行的核验：

| 模块 | 字节数 | SHA-256 |
|---|---:|---|
| verify_published_manual_main.py | 48892 | `f891a676f5da3d501660e74a443d156445a5ec4cdec254ace0ab7808271aca2f` |
| manual_main_provenance.py | 44191 | `348fd9083d4c55e693925533028d61340e157000508b711778da5e2d7bfe8f33` |
| dispatch_receipt_adapter.py | 14758 | `e1b062220a5337b495a62ac7f0bf1db748b32727054e2bb27173c019c1c933d4` |

crate publication 的延期与生产 relay 部署的延期是两项独立决定。此核验器保留 git-source-v1、verified-source 和最终公开文件的严格来源合同；不能以本地 Gate18 通过代替正式工作流、公开资产或 registry 发布证据。

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
| S401 的 Gate17 | 历史 7 通过、1 Windows lint 失败、27 未运行；独立记录具体库回归成功 |
| S_b8d 的 Gate18 | 本文终态 35／35 阶段通过；只归属于本次源码和原检查范围，不代填下游验收 |

Gate16 的原栈溢出仍保存于 [失败事实文件](gate-S647-r16-failure-public-facts.json)。旧 socket 烟测、缺少真实 relay ACK 的 `--input none` 失败、多 UID 准备脚本和未运行的 UI 场景继续保留原范围，没有并入 S401 或 S_b8d 的通过结果。受控 socket 烟测也不能回填原入口 ENOENT 的失败；本机系统安装候选与包管理器记录相符，仍不能据此认定报错用户实际 PATH 入口或版本。

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

原 35 阶段检查已完成。N18 原只读来源证明已通过，后续由原验收组按既有顺序串行使用同一 S_b8d 源码及绑定产物完成原消费者范围；报告组随后读取实际回执，分别更新启动、完成与通过状态。报告组继续 0 Cargo／metadata／构建／产品／测试／target 写入和排队重任务，H2 不导入或执行；公开报告仍与 GitHub、Google 文档同步，不把历史成绩或交接通知写成后继验收通过。
