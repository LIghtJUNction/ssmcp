# Project Z 应用消费验收：SSMCP 785e6b0

功能源码固定为 [`785e6b00440b048d00d9da626a33defa99108afe`](https://github.com/LIghtJUNction/ssmcp/commit/785e6b00440b048d00d9da626a33defa99108afe)。本轮已完成这份源码的 Linux 构建、私有桌面生命周期和账户 relay 专项验收。逐项结果、精确二进制哈希、资源限制与保留的失败记录见 [native-validation.json](native-validation.json)。

此源码仍使用开发版本号 `0.10.14`。正式发布由现有唯一发布者负责，其 [v0.10.15 整合草稿 PR 109](https://github.com/LIghtJUNction/ssmcp/pull/109) 还需要接收本轮最终增量并验证组合后的源码。测试用 ELF 没有替换用户安装、正式标签或公开发布资产；oh-my-ztty、zrc 的正式制品传播等待该发布步骤。

## 依赖和源码身份

| 子仓库 | 版本 | 固定提交 |
| --- | --- | --- |
| zharness | 0.5.1 | `e3580c01c7252da090d04e59b59762bc561777d1` |
| zplugin | 0.3.1 | `00d566eedcff7b2932e90b9dfeb44910c783d305` |
| ztuio | 0.1.0 | `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e` |
| zuse-computer | 0.2.1 | `d930769005d307c42639e8abedc81ab039427e40` |

`Cargo.lock` 的 SHA-256 为 `27556db8b55f570c5e5c46e8771faf90795a2dee55d773a2d5459f4a19fa62ba`。原始构建源 `c44c2cfecb6ba5dae3a63d67a3af08c77eb7b2cc` 到最终 785 的 Cargo 输入、锁文件和四个 gitlink 逐字不变；这一范围的唯一 Rust 文件变化是 `src/computer/isolated.rs`，另有 GTK 测试夹具及证据文档的变化。

更早的聚合基线 `ecc450f17012f8120d7d49470ce9599ada65456f` 到最终源码，还包含保留的显示分配、资源归属、E0509 与 Wayland 修正，以及 X11 全文输入适配。`src/client.rs` 同时保留了上游账户传输修复：当 relay 子进程先退出时，所有 `Account` 传输都读取最终授权拒绝，包括没有账户 token 的显式 `--allow` 连接。最后这一点由本次新增的 47 项 relay 专项目单独验证。

## 验收范围与二进制对应关系

各行有不同的源码、环境和测试范围，数量不相加为一个“全平台测试总数”。

| 编译源码 / 夹具源码 | 实际运行环境 | 结果 |
| --- | --- | --- |
| 785 / 785 | 云端 Linux | fmt、workspace/all-targets Clippy `-D warnings`、二进制与测试产物编译通过；私有环境精确单元测试 1 项通过 |
| 785 / 785 | 开发机 Linux | 私有桌面生命周期 lib 用例各 1 项通过；完整 private-desktop MCP 35 项通过 |
| 785 / 785 | 开发机 Linux | 现有 account/relay 脚本 47 项通过，包含无 token 显式授权连接的活动撤销和 Ready 后立即撤销 |
| c44 / c44 | 云端 Linux及两个开发机精确用例 | 云端 computer 单元测试 44 项通过；受云端 Unix socket 限制的 2 项随后在开发机通过。原云端记录中的 9 项 ignored 保留原状态 |
| c44 / c44 | 云端 Linux | 4 项真实 PTY 测试通过 |
| c44 / ddaf62a | 开发机 Linux | client、daemon 各 6 项真实 GTK 全文输入用例通过；补齐精确的 JS 运行文件布局后，基础 MCP 125 项通过 |

最后三行是原始 c44 ELF 的历史结果。785 的生产变化限于私有子进程环境，相关生命周期已使用新 ELF 重测。历史结果按未变化源码复用，原来的二进制身份和失败记录都保留在 JSON 中。[早期云端报告](cloud-validation.json)和[早期本机预检](local-preflight.json)也保持原样。

新构建使用 Rust 1.100.0、仓库 dev/test profile。云端 Debian glibc 2.41 产物以独立副本执行 `strip --strip-unneeded`，原编译产物保留；私有传输后在开发机 glibc 2.44 核对归档、逐文件 SHA-256 和动态链接。最终实际运行的 client SHA-256 为 `59ca27630c17fd73f027f271c1f752fcc7bacb60125de84aed276e39e1141e19`，daemon 为 `1fdb2d509992c52e48266a53f3dd579c91107ff1cf08c76b70516d7927fc77ca`；libtest、独立 worker 入口及原编译产物的完整身份在 JSON 的 `artifact_identity` 中。

## 实际暴露的问题及修复

首次 GTK 原生运行在焦点设置阶段失败，测试案例列表仍为空。夹具把 Xlib 返回的十进制 XID 直接当成 SSMCP 的窗口标识；`ddaf62a` 将两个 READY 读取点规范化成十六进制字符串，保留生产窗口列表成员验证。该次失败没有被写成文本输入通过。

私有桌面随后暴露了真实环境问题：继承的 AT-SPI 地址已清理、私有 bus 可连接，但默认 broker 激活 Registry 返回 `NameHasNoOwner`。独立诊断只改变官方支持的 daemon 偏好后，Registry 注册成功。诊断又揭示两件事：期望失败的 MCP 返回不能走只接受成功的夹具包装器；GVfs 创建的失效私有 FUSE 挂载会使最终目录清理失败。因此该诊断只记已通过的子检查，整轮状态仍为失败。

785 在私有 `Environment.apply` 内对子进程设置 `ATSPI_DBUS_IMPLEMENTATION=dbus-daemon` 和 `GVFS_DISABLE_FUSE=1`。它保留 `GIO_USE_VFS`、私有显示认证、地址隔离及宿主父环境。实现依据是 [AT-SPI 固定上游源](https://github.com/GNOME/at-spi2-core/blob/e021608a1c485c9d024275c12c20210ff7a11bab/bus/at-spi-bus-launcher.c)与 [GIO 环境变量说明](https://docs.gtk.org/gio/overview.html)。新增的精确单元断言确认父环境九项值前后不变，并保留原有 GVfs 值。

最终原生验证故意在外层选择 broker、保留 GVfs，并移除外层的 FUSE 禁用变量。实际通过依赖产品自身的私有环境设置：同一个已认证 Computer 对错误的窗口机器身份和 PID 拒绝混合文本，GTK 保持零字符、零变化；恢复身份后，同一个 Computer 一次插入 `同一实例🙂`。35 项检查还包括真实键盘和点击、JPEG 截图、另一私有测试显示的焦点与指针不变，以及应用、X server 和私有状态的完整退出清理。

独立 client/daemon GTK 用例另外保留了全文 Unicode、字符选区替换和取消后的真实接收端结果。取消案例使用接收端 changed 屏障，而非固定睡眠：输入已派发时报告不确定状态，不重发，也不声称回滚；指针、焦点、按键掩码和键盘映射保持原状。

## 授权传输缺口的定向补齐

基础 MCP 的 125 项覆盖账号路由、OAuth、终端及连接生命周期，但没有证明 `src/client.rs` 的无 token 授权拒绝分支。本轮使用同一个 785 client ELF，实际运行未修改的 `scripts/e2e_account_relay.py`；脚本 SHA-256 为 `1e103edafd58d6e6fcb1307df4406891b6ff6abe3279b8b67db5c113e0d9de8b`。

该专项目使用两个本地合成网关及独立的账户、配置、会话目录。47 项检查明确包含：活动连接被撤销后非零退出且停止 watchdog 重试；Ready 后立即撤销同样停止启动重试；HTTP 401/403 策略拒绝不重连；普通网络关闭仍正确重连；真实 PTY 往返和退出清理。它没有使用真实 GitHub 账号、公共 OAuth、SSH、图形桌面或模型。

## 资源执行记录

| 门禁 | 时间（UTC） | 实际限制与峰值 |
| --- | --- | --- |
| 785 云端构建 | 03:47:29–03:49:49 | 单 job、Nice 19；用户态 RSS 监控，非 cgroup 硬限制；峰值 3,094,396,928 B，全机可用最低 6,605,586,432 B，进程组 swap 0 |
| 785 私有桌面 2 lib + 35 MCP | 03:54:58–03:55:05 | cgroup max 768 MiB / high 512 MiB / swap 0 / CPU 75%；峰值 494,096,384 B，全机可用最低 4,313,755,648 B；OOM/high/max 为 0，`sock_throttled` 为 15 |
| 账户专项目首次准入 | 04:07:25 | 可用 4,257,132,544 B，低于 4 GiB 准入；exit 75，未启动 unit 或测试 |
| 账户专项目实际运行 | 04:08:02–04:08:22 | 收紧任务 max 为 512 MiB / high 384 MiB；峰值 92,499,968 B，全机可用最低 4,293,472,256 B；swap/OOM/high/max 为 0 |

账户专项目没有图形进程、libtest router 或编译器，持续负载为一个 client、relay 与 PTY，另有短命 CLI，因此采用了更小的任务内存上限。全机 3 GiB 保留、256 MiB 入场缓冲及 250 ms 监控均保持；原始未准入收据保留。两个成功原生批次结束后均验证自有 cgroup 为空、unit inactive/dead。

## 发布交接与适用边界

截至公开回执记录时，最终源码的协调请求处于 queued，尚无发布负责人确认送达。现有 PR 109 明确以 v0.10.15 组合上游交互和 Project Z 依赖；下一步是向该候选提供本轮最终增量，并验证组合后的精确源码，随后由唯一发布者生成正式版本和资产。下游套件以正式资产 URL、校验和和源码提交为接收条件。

本报告证明列出的 Linux 消费路径，不替代 Windows/macOS 的完整应用矩阵、真实公网账号登录或生产服务部署。记录区分源码、编译产物、实际执行、诊断失败和正式发布这几个阶段；未执行项、云环境 `EPERM`、资源未准入以及早期清理失败均保留。公开 JSON 采用字段白名单，原始回执以 SHA-256 关联，不包含用户路径、凭据、私有 bus 地址或短时下载链接。
