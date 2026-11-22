# SSMCP 服务备份：准备、采集与离线核验

`scripts/backup_ssmcp.py` 为下列八个固定路径生成可审阅的备份计划，并在另行授权后采集私有归档。默认运行只打印计划，不连接 SSH、不读取生产配置、不创建文件。本工具没有部署、重启、停写、恢复或解压入口。

本次交付是源码和离线样例验证。样例通过不表示生产备份已完成，也不表示生产服务可以切换。真实采集仍需负责人安排受控业务停写窗口，并取得当时的预检证据。

## 1. 固定范围

| 路径 | 保存内容 |
| --- | --- |
| `/etc/ssmcpd` | 服务配置及其目录内容 |
| `/var/lib/ssmcpd` | 持久账户、授权、订单等服务状态 |
| `/etc/systemd/system/ssmcpd.service.d` | 服务覆盖配置 |
| `/usr/lib/systemd/system/ssmcpd.service` | 服务单元 |
| `/etc/nginx/conf.d/ssmcp.lmm.best.conf` | 此服务的代理配置 |
| `/usr/local/lib/ssmcp/release-1d6b55320fff-ad3af5835f8a` | 本次明确指定的已安装版本目录 |
| `/usr/local/libexec/ssmcp/deploy-release` | 已安装部署入口 |
| `/usr/local/libexec/ssmcp/deploy-entry` | 已安装命令入口 |

八个根路径必须全部存在。工具不自动扩展范围，不把 `/run`、套接字、其他服务或新的发布目录加入归档；范围内出现不支持的节点会导致失败。更换版本目录应先修改固定范围并重新审阅，不能把旧计划当作新服务器状态的证明。

目录读取逐层使用 Linux 文件描述符与 `O_NOFOLLOW`。符号链接只保存链接文本，不读取其目标；解析后的目标必须仍在八个根路径内，否则失败。内部硬链接保存为指向先前成员的 tar 链接，源文件系统上范围外的其他硬链接名称不在清单内。硬链接形式的符号链接暂不支持，会明确拒绝。换行文件名按 JSON/PAX 编码处理，不按行切分。

## 2. 计划与执行条件

以下两个命令都只输出计划：

```sh
python3 scripts/backup_ssmcp.py
python3 scripts/backup_ssmcp.py capture
```

真实采集需要同时提供 `capture --execute`、不存在的新目的目录，以及权限为 `0600` 的停写窗口记录。目的目录的父目录必须由当前用户所有且为 `0700`。工具设置 `umask 077`，新目录为 `0700`，所有输出文件为 `0600`；已有目的目录绝不复用或覆盖。

停写窗口必须覆盖账户、订单和授权写入的所有来源。负责人应先确认入站限制、后台任务和其他写入路径，再提供有时限的书面记录。会话数为零、入口看似空闲、前后摘要相同，都不能证明这段时间存在原子写入排除。本工具只核验记录的结构、范围与有效期，不实现或证明停写机制。

窗口 JSON 的字段如下。`roots` 使用源码 `ROOTS` 中的八个相对路径，顺序一致。示例中的布尔值保持为假，不能直接用于采集；只有记录真实执行授权和真实停写证据后才可变更。

```json
{
  "schema": "ssmcp-controlled-write-window-v1",
  "host": "archczy",
  "roots": ["从 scripts/backup_ssmcp.py 的 ROOTS 逐项填写"],
  "accounts_orders_authorization_writes_blocked": false,
  "archive_execution_authorized": false,
  "service_switch_authorized": false,
  "approval_reference": "真实的采集授权记录位置",
  "write_fence_evidence": "真实的停写机制、覆盖范围及持有者记录位置",
  "source_identity": {
    "machine_id_sha256": "目标 /etc/machine-id 去除首尾空白后的 SHA-256",
    "boot_id": "目标 /proc/sys/kernel/random/boot_id",
    "pid": 0,
    "start_ticks": 0,
    "exe_path": "当时 /proc/PID/exe 指向的范围内绝对路径",
    "exe_sha256": "当时实际运行二进制的 SHA-256"
  },
  "preflight_sha256": "独立保存的新预检记录的 SHA-256",
  "preflight_at_utc": "带时区的新预检时间",
  "started_at_utc": "带时区的停写窗口开始时间",
  "expires_at_utc": "带时区的停写窗口到期时间"
}
```

窗口最多 30 分钟，预检记录也必须在 30 分钟内。执行者负责核对预检中的真实主机身份、运行进程、版本、目录、会话与写入限制，并在整个采集期间持有停写机制。采集会重新读取主机标识摘要、boot ID、systemd MainPID、进程启动 ticks 和实际可执行文件摘要，要求与窗口中的 `source_identity` 一致，并在每次清单扫描前后复核。这些身份探测路径不加入归档。缺项、目标变化或窗口过期都会失败；`preflight_sha256` 本身仍是外部证据引用，不证明业务已停写。

确认上述条件后，未来的采集命令形式为：

```sh
python3 scripts/backup_ssmcp.py capture --execute \
  --host archczy \
  --known-hosts /home/lightjunction/.ssh/known_hosts_archczy \
  --window /private/path/write-window.json \
  --output /home/lightjunction/Backups/servers/backup-YYYYMMDDTHHMMSSZ
```

SSH 使用现有认证环境、`BatchMode=yes`、严格主机密钥校验、禁止更新主机密钥，并关闭连接复用；远端只执行 `sudo -n python3 -B` 的只读采集代码。脚本不读取或输出 SSH 私钥，不在生产机写临时归档。远端需要 Linux、Python 3.11 及以上版本、完整读取八个路径及其扩展属性的权限。历史上认证成功或空间充裕不替代当次检查。

## 3. 归档与结果文件

采集先取得源清单，再由远端重新取得清单并核对摘要，随后流式输出 PAX tar.gz，最后再次比较源清单。清单包含节点类型、数值 UID/GID、权限位、源大小、纳秒修改时间、设备与 inode、硬链接数量和关系、符号链接文本、文件内容 SHA-256，以及可读取的扩展属性。普通文件按块读取，不把整个版本二进制载入内存。

清单和前后检查用于检测观察到的漂移。它们不能捕获所有瞬时变化，也不能替代停写条件；归档中的文件还必须逐项与初始清单核对，避免仅依赖一次前后比较。完整成功要求远端采集正常退出、SSH 返回零、流式写入成功、源清单无漂移，以及离线验证全部通过。

| 文件 | 含义 |
| --- | --- |
| `write-window.json` | 本次外部授权和停写声明的私有副本 |
| `inventory.partial.json` | 首次采集的原始清单，可能因后续失败留下 |
| `inventory.stderr`、`capture.stderr` | 私有诊断输出，不自动打印到控制台 |
| `archive.partial.tar.gz` | 尚未得到成功结论的归档；失败后保留供核查 |
| `inventory.json` | 验证采用的规范化清单 |
| `archive.tar.gz` | 内容验证成功后的归档名称 |
| `verification.ready.json` | 已写完并同步的私有核验记录；单独存在不表示采集成功 |
| `verified.json` | 最后以排他硬链接发布的唯一成功标记，包含全档 SHA、清单 SHA、大小、退出状态及限制 |

工具根据实际清单估算未压缩 tar 及压缩最坏情况，检查当前可用空间，并保留 256 MiB 余量。传输中并行读取 stdout 与 stderr，检查各自字节数和可用空间；每个 SSH 进程的诊断输出上限为 1 MiB，超限失败。文件写入采用排他创建；归档定名使用不会覆盖既有目标的链接操作。没有 `verified.json` 的目录均不得按成功备份使用，即使已经出现 `archive.tar.gz`。

成功标记在归档、清单、核验记录及目录完成必要同步之后才发布。核验记录先写入 `verification.ready.json` 并同步，目录同步成功后，再以排他硬链接创建 `verified.json`；临时核验记录与目录同步失败都发生在成功标记之前。最终发布后不再执行可能失败的写入或同步，保留 ready 副本。最终标记这个新目录项没有后置目录同步，断电可能使其消失，因而结果明确记录 `final_marker_name_not_directory_synced`；这会保守地留下无成功标记的备份，不承诺成功标记的断电持久性。需要另行核查后才能采用缺少标记的材料。

超时、连接中断、非零退出或不完整输出都使采集失败。工具不自动重试，也不把未知结果当成成功。核查已有私有文件和原操作状态后，负责人才能决定是否另开新的采集窗口与新目的目录。

## 4. 离线核验与属性边界

离线核验只读已下载的私有目录，不连接生产，也不把文件解压到任何目标：

```sh
python3 scripts/backup_ssmcp.py verify /private/path/completed-bundle
```

验证器先核对整个压缩文件的 SHA-256，再完整读取并限制 gzip 解压长度，检查压缩流的校验和与尾部。随后逐项校验 tar 成员，拒绝越界、额外、重复、缺失或不规范的路径，核对文件内容、权限、所有权、时间与链接。成员之后必须只有合法的零填充，拼接第二个归档或追加隐藏内容也会失败。验证器从不调用路径解压操作。

Linux 可读取的 xattr 以原始字节的 base64 放入每个成员的 `SSMCP.metadata` PAX 字段。文件系统通过 `system.posix_acl_*` 暴露的 ACL 也按原始字节保存；属性读取不支持或报错时失败，不静默忽略。验证通过只证明这些字节和采集清单一致，不能证明所有操作系统 ACL 都可见，也不能证明在另一台机器恢复后的权限语义。

**普通 tar 解压不会自动应用本工具的自定义扩展属性字段。** 本交付没有恢复器，也不承诺一条 tar 命令即可完整恢复服务。恢复必须另行设计并核验目标文件系统、ACL、xattr、硬链接、权限和服务状态。PAX 的基本成员信息可由标准工具读取；这不等于扩展属性恢复已验证。

归档与同目录 receipt 的摘要用于发现数据不一致，不提供独立真实性证明。能同时改写归档和 receipt 的一方可以重新生成摘要，因此应把审核过的 receipt 摘要保存在独立可信的位置。清单、属性、链接文本和诊断也可能敏感，必须和归档一起保密；不要上传到 Git。

## 5. 与部署和回滚的关系

`archive_consistency: verified_against_inventory` 只表示归档与采集清单一致。结果分别记录 `archive_content_verified`、`metadata_recorded` 和 `metadata_restore_verified`，后者始终为 `false`。所有结果中的 `service_switch_authorized` 也均为 `false`。设备号和 inode 是来源与硬链接关联证据，不承诺恢复原值。服务切换仍需要独立的兼容性、会话与写入条件核验，以及负责人批准的执行流程。

现有 `deploy_release.py` 的回滚材料格式与本工具不同，不能直接互换。已安装的旧 `deploy-release` 不支持 `--stage-only`，不能根据仓库新源码假定线上入口具有该选项。新部署入口的安装、替换与执行都不属于本备份工具。

旧账户读取器不接受新状态中的 `generated_handle`、`handle_aliases`。回退旧二进制时，必须在受控窗口内使用与之匹配的迁移前账户快照。新版本发生新的账户、撤销授权或订单写入后，不得直接恢复旧状态或删除这些字段来冒充兼容；应按实际状态采用前向修复。备份可读并不消除状态回退造成的数据损失。

## 6. 验证证据与审阅范围

运行 `python3 -B -m unittest discover -s scripts -p test_backup_ssmcp.py -v` 可重现离线样例。每个样例使用私有临时目录构造八个根路径，并在结束时清理归档；不会调用真实 SSH，也不会读取生产文件。

| 要验证的结论 | 样例证据 | 尚未覆盖的证据 |
| --- | --- | --- |
| 默认命令不会执行采集 | 默认模式与缺少 `--execute` 时禁止进程启动和目录创建 | 无需生产操作 |
| 可检测损坏和归档范围变化 | 截断、CRC、全档 SHA、重复、额外、路径穿越、尾部追加 | 生产文件规模下的采集耗时 |
| 内容与基本属性可核对 | 内容和权限漂移、纳秒时间、内部软硬链接、换行名称、二进制 xattr | 目标文件系统上的恢复语义 |
| 失败不产生成功结论 | 真实本地子进程非零退出、私有 partial、缺少成功 receipt | 真实 SSH 连接异常场景 |
| 输出私有且不能覆盖 | 完整样例 bundle 的 `0700/0600`、再次创建失败 | 生产主机的停写机制与新预检 |

审阅重点是范围是否符合这次服务、命令和术语是否明确、样例能否支撑结论、缺失的生产证据是否标出，以及停写和恢复边界是否被夸大。当前证据只支撑源码与离线样例结论，不能升级为生产备份或恢复成功。

实现依据：[Python tarfile 文档](https://docs.python.org/3/library/tarfile.html)、[GNU tar 扩展属性说明](https://www.gnu.org/software/tar/manual/html_node/Extended-File-Attributes.html)。文档按用户指定的 [Research Paper Writing Skills](https://github.com/Master-cai/Research-Paper-Writing-Skills) 做了结论与证据对应检查。
