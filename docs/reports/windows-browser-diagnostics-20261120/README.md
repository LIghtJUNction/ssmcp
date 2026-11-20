# Windows 原生浏览器 smoke 的失败诊断

## 当前结论

正式预检 [44762565095 的 Windows ARM 任务](https://github.com/LIghtJUNction/ssmcp/actions/runs/44762565095/job/136378948988)，源码 `c7086f9545602f45399e7a237995a48440e7d8aa`，在真实浏览器依赖安装阶段失败。保留的报告只有 `clean_dependency_install / MethodInvocationException`，缺少失败操作和内部系统错误码。当前证据不能确定具体安装故障；本次修改补充诊断能力，下一冻结候选仍须通过真实 Windows 门禁。[机器证据](validation.json)

| 正式预检 | 平台 | 已有结果 |
|---|---|---|
| [44754296839](https://github.com/LIghtJUNction/ssmcp/actions/runs/44754296839)，`4c6178e` | Windows ARM | 30 项检查通过，完成依赖安装和 headless 本地页面验证 |
| [44762565095](https://github.com/LIghtJUNction/ssmcp/actions/runs/44762565095)，`c7086f9` | Windows ARM | 16 项前置检查通过，随后依赖安装失败 |
| 同一 `c7086f9` 预检 | Windows x64 | 30 项检查通过，完成依赖安装和 headless 本地页面验证 |

三份报告中的浏览器依赖 helper 和两份 installer 的哈希相同。旧、新 ARM 报告还记录了相同的 Node.js `24.22.3` ZIP、相同大小及 SHA-256；官方校验列表也相同。这些证据排除了这两次下载内容或已记录 helper 字节发生变化，但不能单独判定剩余错误的原因。旧 ARM 成功报告中的 Chromium 是 x64，已实际启动；本次没有增加浏览器必须与客户端同架构的要求。

## 修改范围与读取方式

受测脚本对应提交 [`f9cc781e6814a89952ef6bd89e423b9c802cd972`](https://github.com/LIghtJUNction/ssmcp/commit/f9cc781e6814a89952ef6bd89e423b9c802cd972)。`Get-SmokeFailure` 在已有 `phase`、`exception_type` 之外，记录错误分类、最多六层 CLR 异常类型与数值 `HResult`，以及存在时的 `Win32Exception.NativeErrorCode`。堆栈最多检查 16 KiB，只输出最多八个白名单函数名、来源类型和正整数行号。[实现](../../../scripts/smoke_windows_browser.ps1)

行号按 `source` 解释：

| `source` | 行号基准 |
|---|---|
| `browser_dependency_block` | 从 installer 导入的浏览器依赖 block 的起始行 |
| `download_function` | 单独导入的下载函数定义的起始行 |
| `smoke_script` | smoke 脚本本身 |

前两类是导入片段内的相对行号，需结合函数名、helper 哈希和该提交的源码定位。报告不复制异常消息、原始堆栈、文件路径、命令参数、环境变量值或子进程输出。parent 和 worker 的错误出口使用同一格式化函数；安装步骤、重试、期限、平台判断、清理和成功条件均未修改，两份 installer 与 release workflow 也未修改。

## 本地验证

在 Linux x86_64 上使用独立目录中的官方 PowerShell `7.6.10`，与失败 runner 记录的 PowerShell 版本一致。下载归档 SHA-256 与[官方发布元数据](https://github.com/PowerShell/PowerShell/releases/tag/v7.6.10)一致：`e261cabc91c78a9afe8d1e5f24731ab0d38204735ab2ec94b0af55102652e10d`。没有改动全局 PowerShell 环境。

| 检查 | 结果 |
|---|---|
| 既有 bootstrap fixture 与新增诊断检查 | 53 项通过，退出码 0，3.767 秒 |
| 实际 smoke 在非 Windows 上的保护 | 退出码 1，`ok=false`、`phase=windows_guard`，清理状态完整，0.726 秒 |
| 受测两脚本与提交字节核对、`git diff --check` | 通过 |

新增检查实际触发重复解压时的 `CreateNew` 文件冲突、缺失可执行文件的进程启动错误，以及真实子进程携带 stderr 的非零退出，验证内部 I/O 类型、原生错误码、函数位置和私有内容排除。嵌套异常与递归调用检查六层异常、八帧堆栈的界限。[测试代码](../../../scripts/test_windows_browser_bootstrap.ps1)

这些本地结果证明诊断格式和既有 mock 合同，不能替代 Windows ARM 安装。没有重跑旧预检任务，也没有通过增加等待、吞掉异常或改变门禁取得通过。完整原始报告摘要、文件哈希、检查名称和执行参数见[机器回执](validation.json)；正式安装是否成功仍由新候选的 Windows 任务判定。
