# 工作区同步与端口映射：验收记录

验证日期：2026-11-16。

工作区内置能力已在开发机完成本地验收，覆盖真实 HTTP、OAuth、隔离 JavaScript、系统 rsync、TCP 转发和本机 OpenSSH。两套 SSH 脚本分别通过 136 项与 204 项检查；Rust 默认测试通过 179 项，依赖真实运行环境的专项测试另行执行并通过。

本次交付是源码、项目 skill、使用说明及验证记录。生产服务的升级和当前 ChatGPT 沙箱作为客户端的跨主机传输不在本次已完成的验证范围内。

## 验收结果

| 检查 | 结果 |
| --- | --- |
| cargo fmt --all -- --check | 通过 |
| cargo clippy --locked --all-targets -- -D warnings | 通过 |
| cargo test --locked | 179 通过、0 失败、11 忽略 |
| code_mode::tests，包含隔离环境测试 | 9 通过 |
| two_tools，真实 HTTP 入口及公开工具边界 | 3 通过 |
| workspace_bridge，系统 rsync、双向端口、活跃任务取消 | 3 通过 |
| scripts/e2e.py，真实 SSH 基础流程 | 136 项通过 |
| scripts/e2e_agents.py，5 种后端 | 204 项通过 |
| scripts/test_portal_ssh.py，门户安装逻辑 | 5 通过 |
| scripts/test_registry_restart.py，守护进程重启后的 OAuth 登记 | 8 通过 |
| cargo package --locked --allow-dirty --no-verify | 通过，0.4.1 包包含 83 个文件 |
| 项目 skill 的结构校验 | 通过 |

各行记录各自命令的结果，测试覆盖存在重叠。默认忽略的测试涉及实际系统运行环境或后端集成；本次分别显式运行了隔离 JavaScript、HTTP 和工作区桥接测试，并用确定性后端程序执行五种模式的真实 SSH 验收。

发布包包含 skills/ssmcp/SKILL.md、工作区使用说明、验收记录及新增 Rust 模块。make install 将 skill 安装到前缀下的 share/doc/ssmcp/skills/ssmcp，动态资源 ssmcp://skill 也提供同一内容。

## 关键行为及证据

### 文件与目录同步

workspace_bridge 测试建立两个独立客户端和共享目录，通过真实 MCP 请求调用 code_mode，再经 wire 调度进入 Workspace 和 Rsync 实现。传输使用系统 rsync 的实际协议字节流。

测试覆盖超过 512 KiB 的二进制文件、隐藏文件、子目录、默认 dry-run、正向同步、交换端点后的反向同步和显式删除。预演后目标没有新增文件；实际同步后逐字节比较内容。delete 默认关闭，目标已有文件保留；显式开启后验证删除传播。原生 rsync 测试还覆盖带引号和 Unicode 的路径、符号链接隔离及数据上限。

源和目标是各自共享根目录内已经存在的目录，复制的是源目录内的内容。默认 dry_run=true、delete=false。每次同步有一个明确的源；交换端点可反向同步，两端独立修改的冲突由调用方先行处理。

### 端口映射和任务清理

端口测试分别建立两个方向的映射，每次发送超过 128 KiB 的数据。发送端关闭写侧后仍收到完整回复，证明 TCP 半关闭被保留。关闭映射后，新连接失败，两端原生网络句柄列表均为空。测试还验证了其他登录授权无法关闭该映射。

取消测试创建 64 MiB 稀疏文件，等待两个 rsync 端点都开始传输后调用 jobs.cancel。进入 cancelled 状态时，每端开始与关闭的 rsync 端点数一致。

清理逻辑的单元测试占满 Broker 的 16 个原生调用名额，验证忙碌时的重试；持续无法关闭时保留 cleanup_failed 和待清理句柄，再次关闭或取消可继续清理。尚未清理的对象保留在容量统计和历史中。

### 动态资源和权限

资源测试覆盖分页、URI 校验、客户端断开、端点范围、登录身份及授权变化。真实 HTTP/OAuth 测试覆盖两代 MCP 协议的资源请求。公开工具仍严格为 list 和 code_mode；新增能力通过内置 helper、原生目录和标准 MCP resources 提供。

## 真实 SSH 的五后端验收

两套脚本通过本机真实 OpenSSH 公钥认证、OAuth Authorization Code / PKCE S256 和 HTTP MCP 入口。五种模式都执行 fs.mkdir、fs.write、fs.read、fs.stat 的二进制往返，并核对启动目录对应的共享根目录。

| 后端 | 发现的原生工具数 |
| --- | ---: |
| Shell | 22 |
| Codex | 29 |
| Claude Code | 27 |
| Pi | 55 |
| OpenCode | 96 |

每种后端包含 18 个通用内置能力：5 个文件接口、8 个 TCP 接口、5 个 rsync 接口。OpenCode 实际遍历两页能力目录。Shell 使用真实 Linux PTY，其余后端使用确定性的原生协议测试程序；model_calls=false。

测试确认 Codex 无换行的实时输出在命令结束前可见，Shell 输出遵循 PTY 的 echo 设置并避免重复输入日志。基础脚本用时 3.094 秒，五后端脚本用时 3.842 秒；这些数值记录测试执行时间，不作为性能基准。

## 运行环境与修复

| 程序 | 实际路径 | 版本 |
| --- | --- | --- |
| Node.js | /usr/bin/node | v22.23.5 |
| bubblewrap | /usr/bin/bwrap | 0.13.1 |
| prlimit | /usr/bin/prlimit | util-linux 2.42.4 |
| rsync | /usr/bin/rsync | 3.5.2-g41bbd895，协议 33 |

本次命令将 /usr/bin:/bin 放在其 PATH 前部，同时保留 Cargo 路径。开发机默认 PATH 中的 mise shim 依赖隔离环境外的配置，因此验收选择系统 Node.js。全局 PATH 和系统配置保持原状。

测试发现原有 RLIMIT_NPROC=128 在创建用户命名空间之前生效：开发机用户已有大量线程时，bubblewrap 返回 Resource temporarily unavailable。修复将这一上限移入已创建的用户命名空间，由 prlimit 设置。CPU、内存、文件描述符及 core dump 限制仍在外层保留；只读挂载、独立网络、禁止嵌套用户命名空间和权限删除继续生效。

SSH 验收还暴露了命令输出的部分行缓冲问题：没有换行的 live 要等到命令结束才显示。修复使普通命令分片立即显示，保留跨分片换行、折行与去重状态；diff 标题和 hunk 继续按行解析。13 个渲染测试及真实 SSH 的完成前输出检查均通过。

## 构建来源及日志

本次合入了主分支 2737081 的门户、部署及 OAuth 登记持久化改动。SSH 验收使用独立构建目录，并复制固定二进制后执行，避免并行工作覆盖共享 target/debug 下的同名程序。

构建时 HEAD 为 a23d75287c6244eacfc24d4d81a68ec020bf7062，待合并父提交为 273708129c77f0673b94bebaa08462dc171cf282，包含已解决的合并内容。后续文档校对不改变上述运行行为。

| 二进制 | 验收快照的 SHA-256 |
| --- | --- |
| ssmcp | eee35150a19c3025459f7e06a79958a07c6f399a7e7d5f6dba162944dcc670a0 |
| ssmcpd | 9c11cbfbe78ba750d98d1804ab0d6a9e37504381374a18603b2b4aef230781e0 |

开发机工作区：/home/lightjunction/Documents/GITHUB/ssmcp-workspace-bridge。

- target/workspace-validation/reports/final-2737081/ 保存最终 Rust、隔离、HTTP、工作区、打包、SSH 和登记重启检查的日志及 JSON 报告。
- 该目录内的 staged-build.json 记录构建来源、工作区补丁摘要、二进制摘要和系统程序路径。
- 固定验证二进制位于 target/workspace-validation/bin/；先前门户安装检查的日志保留在 target/workspace-validation/final/。

## 可复现命令

先使用系统运行时，并为本工作区选择独立构建目录：

~~~sh
SSMCP_CARGO="$(command -v cargo)"
export PATH="/usr/bin:/bin:$PATH"
export CARGO_TARGET_DIR="$PWD/target/workspace-validation/build"

"$SSMCP_CARGO" fmt --all -- --check
"$SSMCP_CARGO" clippy --locked --all-targets -- -D warnings
"$SSMCP_CARGO" test --locked
"$SSMCP_CARGO" test --locked --lib code_mode::tests -- --include-ignored --test-threads=1
"$SSMCP_CARGO" test --locked --test two_tools -- --include-ignored --test-threads=1
"$SSMCP_CARGO" test --locked --test workspace_bridge -- --ignored --test-threads=1
"$SSMCP_CARGO" build --locked --bins
python3 scripts/e2e.py --bin-dir "$CARGO_TARGET_DIR/debug"
python3 scripts/e2e_agents.py --bin-dir "$CARGO_TARGET_DIR/debug"
python3 scripts/test_portal_ssh.py
python3 scripts/test_registry_restart.py --bin-dir "$CARGO_TARGET_DIR/debug"
"$SSMCP_CARGO" package --locked --allow-dirty --no-verify
~~~

workspace_bridge 和隔离测试需要相应系统程序及 Linux 命名空间。SSH 脚本另需要 OpenSSH，具体准备方式见项目的部署与验证文档。

## 当前 ChatGPT 沙箱的接入状态

本环境经 HTTP 传输访问 ssmcp 服务的 HTTPS healthz 得到 HTTP 200。直接创建到 ssmcp.lmm.best:22 的 socket 时，getaddrinfo 返回 Temporary failure in name resolution；当前沙箱没有本地 SSH 密钥目录或可用 SSH agent。

因此，当前沙箱尚未注册为传输端。已完成的证据覆盖开发机上的独立工作区、真实本机 SSH、系统 rsync 和 TCP 转发。公网跨主机链路、断网后的恢复及跨机吞吐量，需要在两个实际环境都建立客户端连接后验证。

接入时，两端各自运行更新后的 ssmcp 客户端，服务端运行包含本次功能的 ssmcpd，再通过 list 核对客户端 ID、hostname 与 cwd。操作方法见 [工作区使用说明](workspaces.md)。
