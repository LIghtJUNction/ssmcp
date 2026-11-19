# v0.10.14 候选的原生插件回归验收

冻结源码 [b49da12ac890f4d2b00c42d9dcf05b97e2054bac](https://github.com/LIghtJUNction/ssmcp/commit/b49da12ac890f4d2b00c42d9dcf05b97e2054bac) 的原生插件定向回归共 **28 项通过、0 项失败**。其中包含显式执行的真实 cdylib 集成合约。测试直接复用已冻结的执行文件；本次验收没有调用 Cargo 或编译器，没有修改产品代码。

## 被测对象

本机环境为 Linux / x86_64，应用 manifest 版本为 0.10.14。锁文件记录的原生插件依赖为 zplugin 0.3.0、zplugin-macros 0.3.0、zharness 0.5.0。本报告的结果只归属于上述冻结提交及下列产物。

| 产物 | SHA256 |
|---|---|
| ssmcp-da29c9e7034e9a42 | b74d457c9934557a6588107c4478e68804f238a71e7e677c998d57b143a54793 |
| libnative_plugin_fixture.so | f432324f95b5a3de32bf5c82e81a6fadd017fd97071ceab6c9e81f7202b6858a |

执行前检查了源码 HEAD、已跟踪文件状态、休眠 UI 源文件 blob、冻结清单和两份产物摘要；执行后重新计算两份产物摘要，结果完全相同。完整时间、源码 gitlink、锁文件摘要、每个测试名称及日志摘要保存在 [evidence.json](evidence.json)。

## 执行结果

| 测试组 | 通过 | 失败 | 说明 |
|---|---:|---:|---|
| 原生宿主与 ABI | 8 | 0 | 已执行全部匹配测试 |
| 动态扩展目录、预算和 worker | 9 | 0 | 默认跳过的真实合约另行执行 |
| 内建 files 与 process/lazy | 10 | 0 | 已执行全部匹配测试 |
| 真实 cdylib 集成合约 | 1 | 0 | 已执行全部匹配测试 |
| **合计** | **28** | **0** | 每项成功测试只计一次 |

动态扩展常规组中的真实 cdylib 合约带有 ignore 标记，因此常规调用先显示 1 项 ignored。随后使用精确名称和 --ignored 执行该合约，得到 1 项通过；合计中没有遗漏或重复计算它。

原生宿主组覆盖 ABI 结果只能消费一次、未轮询任务释放、取消时销毁真实 Future、调用审计范围、注册代际复查、并发等待和关闭顺序。内建插件组覆盖文件操作取消后的 listener 释放、上下文授权不被扩张，以及可选进程提供者的惰性初始化、失败缓存、共享初始化和关闭。

[真实动态扩展合约](https://github.com/LIghtJUNction/ssmcp/blob/b49da12ac890f4d2b00c42d9dcf05b97e2054bac/src/runtime/plugins/tests.rs)在自己的临时工作区及 HOME/state/cache 中加载实际共享库。它检查中文路径与 Unicode 参数、原生工具目录保真、hook 拒绝后插件调用计数不增加、业务错误与无效输出、超大结果归档、卸载后的旧路由失效、重新加载后的代际隔离，以及关闭后 worker 结束。这里验证的是注册和执行生命周期；采用 Pinned 策略的库映像仍遵循既有驻留约定。

## 复用冻结产物

在包含 frozen/ 的证据目录中执行以下命令。共享库需保留在同一 profile 的 frozen/debug/examples/ 下，真实合约会按测试执行文件的位置寻找它，并自行创建隔离的运行目录。

```sh
native_test_binary=frozen/debug/deps/ssmcp-da29c9e7034e9a42
"$native_test_binary" runtime::native_host:: --test-threads=1 --nocapture
"$native_test_binary" runtime::plugins::tests:: --test-threads=1 --nocapture
"$native_test_binary" runtime::native_plugins:: --test-threads=1 --nocapture
"$native_test_binary" --exact runtime::plugins::tests::native_plugin_runtime_contract --ignored --test-threads=1 --nocapture
```

外层验收清除了两个测试子进程选择变量 SSMCP_NATIVE_PLUGIN_CONTRACT_ROOT 和 SSMCP_NATIVE_PLUGIN_FIXTURE_LIBRARY，确保实际执行隔离目录创建和共享库复制步骤。公开证据没有账号、服务地址、令牌、个人目录或完整客户端 ID；原始执行输出留在本机私有验收目录。

## 范围

这是 Linux 上针对原生插件的冻结产物验收，不构成整个库测试集或其他操作系统的通过证明。后续 test-only 修正及完整默认库测试应记录自己的实际编译提交与产物摘要。休眠 UI 的实际截图、电脑操作和发布验证也由各自的验收记录提供。
