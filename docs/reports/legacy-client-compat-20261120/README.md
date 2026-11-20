# 旧版客户端对新版账号中继的兼容验收

## 后续冻结回归

加入私有 X server 分配器修改后的 3298 冻结候选已重新运行并通过 41 + 47 项账号回归；新二进制摘要、退出码和逐项结果见[3298 回归报告](allocator-3298/README.md)。下文保留 f9ac 首轮证据的原始来源与边界。

## 结论

**冻结候选 `f9acbfc0102fa77b2e44c95724a05d6e55ea74bf` 的两套真实端到端验收均通过。** 旧版客户端兼容检查共 41 项，新版账号流程检查共 47 项，两者退出码均为 0。输入的产品二进制和测试文件在执行前后保持一致。

| 验收 | 结果 | 耗时 | 被测对象 |
| --- | --- | --- | --- |
| 旧客户端连接新中继 | 41 项通过 | 16.933 秒 | 已核验正式 v0.10.13 与当前实际运行的 v0.10.12 映像，各测默认自己、显式允许自己两种连接 |
| 新版账号流程 | 47 项通过 | 18.661 秒 | f9ac 冻结的 v0.10.14 CLI，包括启动撤权、注销、凭据隔离和 watchdog |

两套验收并行执行，耗时只用于记录本次运行，没有用作性能比较。测试时间为 2026-11-20 00:13:25–00:13:44 UTC（台北 08:13）。

## 为什么需要兼容这类旧连接

现有旧客户端通过账号凭据连接中继时，会把自己的 GitHub principal 写进 Hello 的 allow 字段。新版使用服务器用户名和中继域名表达授权对象。若直接对旧字段执行新版格式校验，仍在使用旧 CLI 的正常账号会话将无法重新连接。

[服务端修复](https://github.com/LIghtJUNction/ssmcp/commit/90a16cfa3b9e13d63e0c184b93772985cc1ecde9)只兼容一种情况：连接已经通过账号凭据认证，而且旧 allow 列表恰好只有凭据所属账号的精确 GitHub principal。服务端将它转换为默认的账号自身授权，再继续执行原有验证和权限判断。

因此，兼容旧格式没有扩大访问范围。客户端凭据确定账号身份，另一账号仍不能列出或调用该账号的会话。其他人的 principal、混合名单、匿名连接和其他无效元数据由独立的真实 WebSocket 回归检查。本报告中的真实旧 CLI 测试没有把客户端提前拒绝的参数，当成服务器的拒绝证据。

旧协议会话不会得到新版 `session_access` 能力声明；修改新版账号授权名单需要更新客户端。普通连接和工具调用继续工作。完整服务器来源见 [f9ac 冻结候选](https://github.com/LIghtJUNction/ssmcp/commit/f9acbfc0102fa77b2e44c95724a05d6e55ea74bf)。

## 客户端来源

| 输入 | 完整性依据 | 本报告的来源表述 |
| --- | --- | --- |
| v0.10.13 | ELF SHA-256 `5ad6682a2e63e865b9a0ada78ce0d1b9b7799cf95e6bdad15c4661ecc106a7aa`，与已核验的正式发布映像一致 | 正式 v0.10.13 客户端 |
| v0.10.12 | ELF SHA-256 `0e2de0037d5ce6ee1c724f1530b171655b8c72e311077619a7903af8ebcde8e3`，与本次只读盘点中的实际运行映像一致 | 当前实际运行的 v0.10.12；没有宣称通过官方发行归档核验 |

测试先把输入 ELF 复制到私有测试目录，再按普通 CLI 入口启动。正式 v0.10.13 的预期哈希固定在脚本内；v0.10.12 的哈希由调用方明确提供，复制前后均检查。测试没有运行 007 服务入口，也没有覆盖任何安装文件。

发布前只读盘点在当时观察到 14 个普通 v0.10.12 连接和 1 个独立 v0.10.13 恢复连接。这个计数描述已观察到的版本，不表示全部会话空闲，也不作为自动重载许可。具体客户端标识、进程信息和工作目录只保存在私有清单中。

## 实际验证了什么

旧版测试包含以下四组，每组均执行相同的真实连接与权限检查：

| 客户端 | 连接方式 | 结果 |
| --- | --- | --- |
| 正式 v0.10.13 | 使用保存的测试账号，默认允许自己 | 通过 |
| 正式 v0.10.13 | 显式 `--allow fixture@github` | 通过 |
| 当前运行的 v0.10.12 | 使用保存的测试账号，默认允许自己 | 通过 |
| 当前运行的 v0.10.12 | 显式 `--allow fixture@github` | 通过 |

每组都从真实 CLI 的连接记录确认它使用单个旧式自身 principal。随后通过真实账号接口列出会话，调用真实 terminal，再启动 `/bin/sh` 并核验输出与退出码。测试等待超过本场景的一次重试间隔，再次检查进程存活、客户端 ID 不变、工具仍可调用，并检查没有异常 watchdog 重试。

第二个账号通过真实账号存储登记为不同 subject，取得独立账号凭据，并由 `whoami` 确认其身份。它对第一个账号的会话列表没有可见项，调用相同客户端的 native tool 得到 `404 client_unavailable`。这些是同一中继上的真实授权隔离检查。

新版 47 项套件同时复跑了完整账号生命周期，包含：

- 退出的 SSH / invite / join 入口在访问网络前拒绝。
- 同源凭据选择、重定向隔离、稳定身份与用户名匹配。
- 无账号令牌的显式 allow 连接，以及真实 native shell 往返。
- 普通断线重连、活跃撤权、Ready 后立即撤权和 HTTP 401/403。
- Ctrl-C 清理，以及注销后拒绝重新授权时不恢复连接或凭据。

## 测试环境与证据绑定

服务端由已构建的新库测试可执行文件启动：

```text
--exact http::relay_composer_fixture::relay_composer_server_fixture --ignored --nocapture
```

该夹具使用实际生产 Broker、HTTP router、账号存储和 WebSocket 实现，仅上游 GitHub 提供方由测试服务代替。两名账号及其令牌均由隔离夹具创建；测试不读取真实用户凭据。

所有子进程使用显式构造的 HOME、XDG、配置、账号、会话和临时工作目录，不继承真实账号环境。Ready 文件权限为 0600，测试目录为 0700。旧 CLI 的 Ready 行读取有 12 秒和 16 KiB 上限，异常部分行不会移除等待截止时间。

| 冻结输入 | SHA-256 |
| --- | --- |
| f9ac 服务端库测试可执行文件 | `09831320a7591b4c06f60b0c7f5475376af73aee3f8219b827701da0cdd9c1d9` |
| f9ac v0.10.14 CLI | `9b7fdb2c4f9c5b385047d34c880af7082b1ba785e55c6e06dc4ad1714fb66c69` |
| 旧客户端回归脚本 | `50d2087b8b5b6d7e730fcd576a721f78b75d9e546f12d4bacfa3c10d8da3527c` |
| 新版账号回归脚本 | `1e103edafd58d6e6fcb1307df4406891b6ff6abe3279b8b67db5c113e0d9de8b` |

两个 ELF 在验收前后哈希相同。运行前还将两个入口脚本、共享 gateway helper、基础 Harness 和 Rust fixture，共五个文件与 f9ac 提交逐字比较。明细见 [validation.json](validation.json)、[legacy.json](legacy.json)、[modern-account.json](modern-account.json)，完整成功记录见 [legacy-checks.txt](legacy-checks.txt) 与 [modern-checks.txt](modern-checks.txt)。

源变更分开保留：[第二身份与真实旧 CLI 夹具](https://github.com/LIghtJUNction/ssmcp/commit/a8cf7bee60f9685046f05db28faf82e63fa9913c)、[有界 Ready 读取](https://github.com/LIghtJUNction/ssmcp/commit/ae09cf30bdbe8490fe59c2f68fc0e042ca19c034)。本证据提交不再改变产品或测试逻辑。

## 复现

使用已核验的正式 v0.10.13 ELF、待测服务器的冻结 libtest，以及可选的当前 v0.10.12 映像：

```sh
python3 scripts/e2e_legacy_client_compat.py \
  --official-v13 "$OFFICIAL_V13_ELF" \
  --runtime-v12 "$OBSERVED_V12_ELF" \
  --runtime-v12-sha256 0e2de0037d5ce6ee1c724f1530b171655b8c72e311077619a7903af8ebcde8e3 \
  --fixture-binary "$PATCHED_LIBTEST" \
  --server-revision f9acbfc0102fa77b2e44c95724a05d6e55ea74bf \
  --report legacy.json

python3 scripts/e2e_account_relay.py \
  --binary "$FROZEN_V14_CLI" --report modern-account.json
```

这两条命令不会调用 Cargo。正式 v0.10.13 映像只以普通客户端运行，所用账号仅存在于自有 loopback 测试服务。

## 结果边界

本报告证明旧客户端可被新服务端按账号自身授权接受，建立真实工具连接，保持本次短时观察中的同一连接，并维持账号之间的隔离。测试没有部署到现服，没有重载现有客户端，也没有验证原 native jobs 跨服务端重启后保留。

服务端部署若关闭现有 WebSocket，客户端及其后台作业仍可能经历断开和清理。临近部署时需要另行核对实际运行工作并协调排空，不能把本报告的握手兼容结果解释为作业跨部署保活。
