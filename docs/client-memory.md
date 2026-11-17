# 客户端内存占用与优化验证

**严格的 10 MB 目标尚未全面达到。** 本机已安装提供方场景中，完成 40 条命令后再空闲 35 秒，客户端 RSS + Swap 末值从 **350.36 MB 降至 15.70 MB**，下降约 **95.5%**；发现完成后的采样峰值仍为 **76.21 MB**。最小场景峰值 **9.04 MB**，通过预算；合成目录场景峰值 **10.64 MB**，未通过。[基线报告](reports/client-memory-2026-11-18/baseline-installed.json) · [优化报告](reports/client-memory-2026-11-18/optimized-installed.json)

## 口径与实测结果

所有表格采用**十进制 MB**：`MB = KiB × 1024 ÷ 1,000,000`，10 MB 即 10,000,000 字节。统计 Linux `/proc` 的客户端 **RSS + Swap**，避免把换出内存当作节省。Codex、Claude、Shell、转发辅助程序等其他进程，以及磁盘归档大小和未计入客户端 RSS 的系统写缓存，均需另行统计。[复现脚本](../scripts/benchmark_client_memory.py)

受控基准运行真实客户端和 **120×38 TUI**，由本地模拟 SSH 转发程序驱动协议，没有真实 Broker 网络或 TLS 开销；不启用 `--tty` 外部观察窗口，不发起模型轮次。命令输出逐条核对，完成记录不主动 `forget`。[运行来源](reports/client-memory-2026-11-18/provenance.json)

| 场景 | 扫描轮数 | 已验证并保留的命令 | 工作负载前／后空闲 | 提供方 |
| --- | ---: | ---: | --- | --- |
| 已安装 | 4 | 40 | 各 35 秒 | Codex、Claude 就绪；其余三个不可用 |
| 最小 | 4 | 40 | 各 4 秒 | 外部提供方不可用 |
| 合成 | 2 | 10 | 各 4 秒 | 1,500 个合成原生工具，描述主体各 2,300 字节 |

**主表：客户端 RSS + Swap（MB，越低越好）。** “工作后末值”遵循上表各自的命令数量和空闲时长；已安装场景为 40 条命令后再空闲 35 秒。“后续峰值”对应原始报告 `steady_and_workload_max_rss_swap_kib`，包括初始发现结束后的后台刷新、目录读取、命令输出和后续检查。

| 版本／场景 | 工作后末值 MB | 首次发现峰值 MB | 后续峰值 MB | 最终工具数 |
| --- | ---: | ---: | ---: | ---: |
| [基线／最小](reports/client-memory-2026-11-18/baseline-minimal.json) | 12.16 | 8.63 | 12.79 | 100 |
| [优化／最小](reports/client-memory-2026-11-18/optimized-minimal.json) | 8.84 | 7.82 | 9.04 | 100 |
| [基线／已安装](reports/client-memory-2026-11-18/baseline-installed.json) | 350.36 | 138.87 | 352.31 | 1450 |
| [优化／已安装](reports/client-memory-2026-11-18/optimized-installed.json) | 15.70 | 72.52 | 76.21 | 1450 |
| [优化／合成](reports/client-memory-2026-11-18/optimized-synthetic.json) | 9.87 | 9.38 | 10.64 | 1603 |

两段 35 秒空闲覆盖 Codex 的 30 秒刷新周期。优化版工作后空闲阶段中位数为 **14.25 MB**，仅是统计摘要，不能解释为持续稳定的上限；峰值和末值仍超过目标。50 毫秒采样也可能遗漏更短的尖峰，原始报告另保留不含 Swap 的内核 RSS 高水位。三种优化场景均为 `workload.ok=true`，其中只有最小场景的严格预算 `passed=true`。

优化版四轮扫描和最终复查均返回 **1,450 个工具、23 页**，摘要一致；基线目录在测试中由 1,397 增至 1,450，两个版本的内容摘要也不同。这是实际安装版本的比较，不是固定目录的严格消融实验。[目录及刷新记录](reports/client-memory-2026-11-18/optimized-installed.json)

## 实现改动与行为边界

- **目录共享与按需展开。** 描述符以紧凑字节保存，较大项压缩；提供方、注册表和分页快照共享内容。刷新复用相同描述符及路由名称的分配，变化项才替换，但仍更新提供方 generation，使旧句柄失效。生成的 Codex schema 留在私有目录，只常驻索引并按需读取。[目录](../src/catalog.rs) · [注册表](../src/registry.rs) · [Codex](../src/backend/codex.rs)
- **历史和完成输出设限。** 每个事件环使用紧凑 JSON，限制 256 KiB／1,024 条；后台回放保持 2 MiB／4,096 条。命令流运行时保留 64 KiB 尾部，成功结束后释放重复尾部；不超过 4 KiB 的输出可留一份内存捕获，较大输出从归档读取。失败保留尾部与错误，归档缺失或缩短报错。另清理关闭终端的观察器流计数。[事件](../src/backend/events.rs) · [回放](../src/session/attach.rs) · [命令](../src/backend/shell.rs) · [归档](../src/output.rs) · [观察器](../src/observer.rs)
- **减少启动分配并归还空闲页。** 浏览器 HTTP 客户端延迟创建。Linux GNU 入口在 Tokio 和工作线程启动前设置 64 KiB `M_MMAP_THRESHOLD`；存在 `MALLOC_MMAP_THRESHOLD_`、`GLIBC_TUNABLES` 或 `LD_PRELOAD` 时跳过。显式阈值关闭动态调整，可能增加映射开销；刷新或回复结束后的 `malloc_trim` 仅归还空闲页。其他平台不执行这些 GNU 调整。[入口](../src/bin/ssmcp.rs) · [回收](../src/memory.rs) · [glibc 说明](https://sourceware.org/glibc/manual/latest/html_node/Malloc-Tunable-Parameters.html)
- **缩小代码体积。** Release 使用 `opt-level="z"`、fat LTO、单代码生成单元与符号裁剪。没有逐项消融，不能把总收益单独归给某个改动。[配置](../Cargo.toml)

本地可见滚动历史由 **512 行缩短至 128 行**。原始 PTY、命令与事件捕获仍受现有策略约束：默认 24 小时、已结束归档总量 512 MiB、单文件 256 MiB；小捕获可能仅在内存，不能承诺每条界面提示都持久化。磁盘和容量错误仍须检查。[终端](../src/client/terminal.rs) · [保留策略](session-control.md)

## 测试、真实连接与安装

原始记录逐组汇总为 **16 组、146 项通过、0 失败、2 项真实环境探测被忽略**，覆盖目录保真、游标、旧句柄失效、事件淘汰、归档读取、终端历史与观察器复用。不是完整测试套件或所有平台均通过的声明；前台短时基准也不证明后台会话、外部观察窗口或长期高吞吐负载的上限。[测试记录](reports/client-memory-2026-11-18/tests.json)

另以现有认证建立了独立的真实 HTTPS/WebSocket 连接至 `https://ssmcp.lmm.best`：`printf` 返回精确 `memory-live-ok\n` 和退出码 0；Codex、Claude 为 ready，未发起模型轮次。该连接使用 JSONL、无 TUI、无 `--tty`；75 秒监控末值 **15.88 MB**，采样峰值 **77.45 MB**。这是独立连接验证，不混入上面的受控基准表；验证后仅停止本任务的临时客户端。[连接测量](reports/client-memory-2026-11-18/live-validation.json) · [调用结果](reports/client-memory-2026-11-18/live-call-results.json)

**安装已完成。** `/home/lightjunction/.cargo/bin/ssmcp` 已原子替换为优化版，`--version` 为 `ssmcp 0.7.4`，`--help` 退出码 0。前版已备份至 `/home/lightjunction/Backups/ssmcp/client-memory-2026-11-18/ssmcp-before-8271d106f45a`。已有会话未重启，新启动使用新版。[安装记录](reports/client-memory-2026-11-18/installation.json)

## 来源与证据

优化版源码为 `d1f1071dc47f746d057225a2f240cbf640172a46`，二进制 **7,034,096 字节**，SHA-256 为：

```text
2f40a5c2d61586171d3243f8fd453c48af403255ab4238b6ed731cd1f7818436
```

三个优化报告均匹配此 SHA，运行期间文件未变化。性能基线为 `f685725b975ad4cc46f1eb28833b1a74c876e87dda18f3ce898fbc5195191cca`，源码版本未确认；修改从 `eeda60b` 开始，后合入主线 `811bf371`，不能宣称同提交的严格 A/B。升级前备份 `8271d106…` 与性能基线是不同二进制。[来源清单](reports/client-memory-2026-11-18/provenance.json) · [源码记录](reports/client-memory-2026-11-18/source.json) · [备份路径](reports/client-memory-2026-11-18/backup.json) · [源码对比](https://github.com/LIghtJUNction/ssmcp/compare/811bf371d652e7a279677a7addde8d4f0ea3f727...d1f1071dc47f746d057225a2f240cbf640172a46)

## 复现

以下为开发机留存路径，其他机器需替换并核对 SHA。预算参数 `9.5367431640625 MiB` 等于严格 10 MB；超预算时脚本返回失败但仍保存报告，需分别检查 `passed` 与 `workload.ok`。

最终测量未设置 `MALLOC_*`、`MIMALLOC_*`、`GLIBC_TUNABLES` 或 `LD_PRELOAD` 覆盖。请在专用 Bash 会话执行下面的复现块；开头先清理这些变量，避免分配器配置影响结果。

```bash
for memory_allocator_env in $(compgen -e); do
  case "$memory_allocator_env" in
    MALLOC_*|MIMALLOC_*|GLIBC_TUNABLES|LD_PRELOAD) unset "$memory_allocator_env" ;;
  esac
done
unset memory_allocator_env

cd /home/lightjunction/Documents/GITHUB/ssmcp/.worktrees/client-memory
memory_baseline="/home/lightjunction/Backups/ssmcp/client-memory-2026-11-18/ssmcp-baseline-f685725b975a"
memory_optimized="/home/lightjunction/.cache/ssmcp-memory/ssmcp-memory-candidate-v7"

for memory_mode in installed minimal; do
  memory_idle=4
  if [ "$memory_mode" = installed ]; then memory_idle=35; fi
  python3 scripts/benchmark_client_memory.py \
    --binary "$memory_baseline" --providers "$memory_mode" \
    --scans 4 --commands 40 --command-bytes 512 \
    --idle-seconds "$memory_idle" --sample-ms 50 --catalog-metrics \
    --report "/tmp/ssmcp-memory-rerun-baseline-$memory_mode.json"
  python3 scripts/benchmark_client_memory.py \
    --binary "$memory_optimized" --providers "$memory_mode" \
    --scans 4 --commands 40 --command-bytes 512 \
    --idle-seconds "$memory_idle" --sample-ms 50 --catalog-metrics \
    --max-footprint-mib 9.5367431640625 \
    --report "/tmp/ssmcp-memory-rerun-optimized-$memory_mode.json"
done

python3 scripts/benchmark_client_memory.py \
  --binary "$memory_optimized" --providers synthetic \
  --tools 1500 --description-bytes 2300 --scans 2 --commands 10 \
  --command-bytes 512 --idle-seconds 4 --sample-ms 50 --catalog-metrics \
  --max-footprint-mib 9.5367431640625 \
  --report /tmp/ssmcp-memory-rerun-optimized-synthetic.json
```
