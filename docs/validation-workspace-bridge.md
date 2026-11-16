# Workspace bridge 验证记录

验证日期：2026-11-16。

本记录覆盖动态 MCP 资源、文件同步、双向 TCP 映射及任务取消的开发机测试。测试通过真实 HTTP 和 OAuth 入口调用 code_mode，使用系统 Node.js、bubblewrap、prlimit 和 rsync。两个测试客户端通过真实 wire 协议分别持有独立目录，并执行项目的 Workspace 和 Rsync 实现。

## 运行环境

| 程序 | 实际路径 | 版本 |
| --- | --- | --- |
| Node.js | /usr/bin/node | v22.23.5 |
| bubblewrap | /usr/bin/bwrap | 0.13.1 |
| prlimit | /usr/bin/prlimit | util-linux 2.42.4 |
| rsync | /usr/bin/rsync | 3.5.2-g41bbd895，协议 33 |

运行测试时，将 /usr/bin:/bin 放在该次命令的 PATH 前部，并保留原有 Cargo 可执行文件。开发机默认 PATH 中有 mise 的 Node.js shim；该 shim 依赖隔离环境外的配置，不能作为本测试所要求的系统 Node.js。测试没有修改全局 PATH、系统配置或命名空间策略。

## 结果与证据

| 验证目标 | 测试 | 结果 |
| --- | --- | --- |
| 资源分页、端点权限、连接失效、URI 校验 | cargo test resources::tests --lib | 4 通过 |
| 真实 OAuth 和 HTTP 资源读取，覆盖两代 MCP 协议 | cargo test --test resources | 4 通过 |
| 512 KiB 以上二进制、隐藏文件、子目录、默认 dry-run、正向与反向同步、显式删除、文件根目录约束 | real_rsync_dry_run_forward_reverse_delete_and_rooted_fs | 通过 |
| 两个方向的 TCP 映射，128 KiB 以上数据，写侧半关闭，映射权限与关闭清理 | tcp_mappings_work_in_both_directions_preserve_half_close_and_cleanup | 通过 |
| 活跃同步取消，所有已获取的 rsync 端点清理 | cancelling_an_active_sync_closes_every_owned_rsync_endpoint | 通过 |
| code_mode 新增接口向认证 host bridge 分发 | workspace_helpers_dispatch_through_the_authenticated_host_bridge | 通过 |
| 异常保留日志，原生错误可观察 | errors_keep_logs_and_native_errors_remain_observable | 通过 |

取消测试建立一个 64 MiB 稀疏文件，等待两个 rsync 端点都进入数据传输后调用 jobs.cancel。任务进入 cancelled 状态时，每个客户端上已开始的 rsync 端点数与已关闭的端点数相等。

TCP 测试先在客户端一监听并连接客户端二的目标端口，再交换两个客户端的角色。每次请求发送完后关闭写侧，继续读取完整回包；关闭映射后，新连接失败，两个客户端的原生网络句柄列表均为空。测试还确认其他登录授权不能关闭该映射。

文件同步测试在默认 dry-run 后检查目标目录未创建文件；实际传输后逐字节比较二进制文件。交换源和目标后验证反向文件到达，再以 delete=true 验证删除传播。未指定 delete 时，目标已有文件保留。

## 同步语义

源和目标都是各自客户端共享根目录内已经存在的目录；可先通过 fs.mkdir 创建目标目录。同步复制的是源目录内的内容，不会再套一层源目录名称。默认 dry_run=true、delete=false；实际写入必须显式设置 dry_run=false，删除目标多余文件必须显式设置 delete=true。

交换 source 与 destination 即可反向同步。该操作不执行两端同时修改的冲突合并；指定源端是本次同步的数据来源。用户需要保留两端独立修改时，应先预览并处理冲突。

## 可复现命令

先记录当前 Cargo 路径，再为测试选择系统运行时：

~~~sh
SSMCP_CARGO="$(command -v cargo)"
export PATH="/usr/bin:/bin:$PATH"

"$SSMCP_CARGO" test resources::tests --lib
"$SSMCP_CARGO" test --test resources
"$SSMCP_CARGO" test --test workspace_bridge -- --ignored --test-threads=1
"$SSMCP_CARGO" test --lib code_mode::tests::workspace_helpers_dispatch_through_the_authenticated_host_bridge
"$SSMCP_CARGO" test --lib code_mode::tests::errors_keep_logs_and_native_errors_remain_observable
~~~

workspace_bridge 测试依赖真实系统程序和 Linux 命名空间，因此显式标为 ignored，需在满足这些条件的环境中使用 --ignored 执行。测试不以不隔离的 Node.js 代替 bubblewrap。

## 运行时修复

测试发现原有进程数限制在创建隔离命名空间之前生效。当开发机同一用户已经运行大量线程时，外层 RLIMIT_NPROC=128 会令 bubblewrap 创建命名空间失败，返回 Resource temporarily unavailable。

修复将进程数上限移到已创建的用户命名空间内，由 prlimit 设置为 128。CPU、内存、文件描述符和 core dump 限制仍在外层保留；只读挂载、独立网络、禁止嵌套用户命名空间及权限删除均保持。测试中没有增加对主机目录或网络的访问能力。

## 证据边界

这些端到端测试在同一开发机上运行两个独立客户端和目录，覆盖实际 MCP 请求、OAuth、隔离 JavaScript、wire 调度、系统 rsync 字节流及 TCP 数据转发。它们不单独证明公网 SSH 链路、网络断连恢复或不同物理主机之间的带宽表现。

此验证记录尚未包含当前 ChatGPT 沙箱注册为客户端后的跨主机传输结果。当前测试证据来自开发机上的两个真实 wire 测试客户端；实际沙箱连接情况应另以连接和传输结果确认。

本记录没有宣称发布或生产部署，也没有把双向可发起同步描述为自动冲突合并。上述资源、传输和清理语义由对应测试直接验证。

## 当前沙箱连接探测

通过本环境的 HTTP 传输请求 ssmcp 服务的 HTTPS healthz 得到 HTTP 200，说明该 HTTP 路径可达。直接创建到 ssmcp.lmm.best:22 的 socket 时，getaddrinfo 返回 Temporary failure in name resolution；当前沙箱也没有本地 SSH 密钥目录或可用 SSH agent。

这些结果不能等同于 HTTPS 不可用，也没有完成 SSH 客户端注册。当前沙箱与开发机之间的实际传输仍需在建立客户端连接后单独验证。

## 清理改动后的复验

桥接清理逻辑更新后，以同一系统 PATH 再次执行：

~~~sh
"$SSMCP_CARGO" test --test workspace_bridge -- --ignored --test-threads=1
~~~

结果为 3 通过、0 失败、0 忽略；测试执行时间 2.45 秒，不含编译。此时间用于记录测试运行，不是文件同步或端口转发的性能基准。
