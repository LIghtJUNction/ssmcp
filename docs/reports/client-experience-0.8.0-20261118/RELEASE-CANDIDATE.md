# 0.8.0 首次发布候选：release 构建与安装布局验收

首次候选的 Linux release 构建成功，命令耗时 **370.52 秒**。最初直接使用 Cargo 构建目录运行 composer 时，因缺少已安装布局中的 `code-mode.js` runner 而失败；随后通过 `make install` 安装到隔离 `PREFIX`，实际安装的 release 二进制完成 **composer 88 项、relay composer 45 项、observer 7 项**检查。[构建记录](release-build.json) · [首次失败摘要](release-initial-attempt.json) · [安装后门禁](release-installed-gates.json)

本文固定记录历史首次候选：release 构建源码为 [`0efb04bfbd7f7b0f616f03a6123608cbffc3643e`](https://github.com/LIghtJUNction/ssmcp/commit/0efb04bfbd7f7b0f616f03a6123608cbffc3643e)，实现源码对应已验证的 `8d8172765644bfecdf881a5a62262bde93415e53`。后续安装包固定提交为 `12e1d57e6c39af4f1a5a66282752cb1b06bd6225`；打包、staging 和安装操作由独立报告记录。本页不表示后续主线合并后的最终候选，也不改写此前的 [debug 验收](README.md)。

## 构建、首次失败与安装后结果

构建命令为 `cargo build --locked --release --bins --example vt_snapshot -j1`，退出码为 `0`；`ssmcp`、`ssmcpd` 均报告版本 `0.8.0`。[release-build.json](release-build.json)

| 阶段 | 结果 | 命令耗时（秒） | 证据 |
| --- | --- | ---: | --- |
| release 构建 | 成功，退出 0 | 370.52 | [构建记录](release-build.json) |
| 裸构建目录 UI preview | 38 项通过，退出 0 | 3.49 | [首次尝试摘要](release-initial-attempt.json)、[UI smoke](release-ui-smoke.json) |
| 裸构建目录 composer | 完成前 18 项后失败，退出 1 | 1.02 | [诊断摘要](release-initial-attempt.json) |
| 隔离安装目录 composer | 88 项通过，退出 0 | 14.5 | [门禁](release-installed-gates.json)、[详细报告](release-composer-e2e.json) |
| 隔离安装目录 relay composer | 45 项通过，退出 0 | 6.68 | [门禁](release-installed-gates.json)、[详细报告](release-relay-composer-e2e.json) |
| 隔离安装目录 observer | 7 项通过，退出 0 | 12.79 | [门禁](release-installed-gates.json)、[详细报告](release-observer-e2e.json) |

首次 composer 的实际错误为：

```text
code_mode runner is missing; run make install-server or set SSMCP_CODE_MODE_RUNNER to its installed absolute path
```

这次失败保留为失败记录。`make install` 将 release 二进制放入隔离前缀的 `bin/`，并安装 `libexec/ssmcp/code-mode.js`；后续检查从这个安装布局运行，没有将首次失败改成通过。runner 的哈希也保存在安装门禁中。[安装规则](../../../Makefile) · [release-installed-gates.json](release-installed-gates.json)

表中是外层命令耗时；composer 和 relay composer 详细报告的内部耗时分别为 **14.156 秒、6.33 秒**。observer 与 UI smoke 的详细报告没有独立耗时字段。门禁摘要与详细报告属于相同运行，不重复计数；首次通过的 38 项 preview 也不冒充安装后重新执行的检查。

## 验证范围

SSH composer 使用实际安装的 **release `ssmcp` 和 release `ssmcpd`**，经过隔离 OpenSSH、公钥认证、OAuth、真实 PTY 与独立终端解码器，验证队列提交、取回编辑、定向收件人、草稿保留和实际 `/reload`。[release-composer-e2e.json](release-composer-e2e.json)

Relay composer 使用 **release 客户端与 debug 库测试可执行文件中的生产 Broker/router**。该 router fixture 的源码仍为 `8d817276…`，哈希与 debug 验收相同；它不等于 release daemon 的账号 relay 运行。场景验证真实 DCR、PKCE、consent、token exchange、队列 CAS、同账号重载和安全交接，上游 GitHub 为合成提供方，网络为回环 HTTP/WS；报告明确 `tls_verified: false`、`real_github_account: false`、`models_started: false`。[release-relay-composer-e2e.json](release-relay-composer-e2e.json)

Observer 使用 recording launcher 启动实际 release viewer；UI smoke 使用生产 compositor、样例事件和隔离 PTY。这两项分别验证 viewer 行为与界面交互，不作为特定桌面终端外观或真实用户会话的证明。[Observer](release-observer-e2e.json) · [UI smoke](release-ui-smoke.json)

## 制品哈希

以下哈希在构建记录、安装门禁和可交叉核对的详细报告中一致。debug fixture 单列，避免与 release daemon 混淆。

| 产物 | 配置／来源 | SHA-256 |
| --- | --- | --- |
| `ssmcp` | release，`0efb04bf…` | `732c1b4873d31887390ba06b3d2bc890dc231a1f440526df728eb12164b2d1ae` |
| `ssmcpd` | release，`0efb04bf…` | `711aebe278162c7f5633c863dcbda8bceb17c925eb3531b51ba349bfab420e87` |
| `vt_snapshot` | release，`0efb04bf…` | `ec6372dc5a077b457944be0b64b5bcb5fdf411c099454e02a58cf9f38d61364b` |
| `code-mode.js` | 隔离安装目录中的 runner | `0a4902726c7098036db84186c1e3aeb84d7ab335571f87def891632cc28bca99` |
| Router fixture | debug 库测试程序，`8d817276…` | `d4ee3f9cc023567cc4c0b283da031cd9cdee5a1cf544f50b34784c616c948518` |

## 194 个输入文件的一致性

[source-manifest.json](source-manifest.json) 保存 **194 个不同相对路径**及其 SHA-256。集合来自 `git ls-files src tests scripts examples Cargo.toml Cargo.lock rust-toolchain.toml Makefile .github/workflows/ci.yml`，不扩大为整个仓库或全部安装包内容的清单。

本地逐文件复算与清单全部相符；维护者观察的远端源码提交为 `0efb04bf…`，同一集合也为 194 个文件，`git status --porcelain` 为空。两端的聚合摘要一致：

```text
1a21ba2738777b41a920201ad439262ac9a514c4270b7bb400d193e539fc4953
```

聚合算法为 `sha256(json.dumps(files, sort_keys=True, separators=(',', ':')).encode())`，其中 `files` 仅为相对路径到文件 SHA-256 的字典。清单明确区分本地复算结果与维护者提供的远端观察，未补填未记录的执行时刻。

## 公开副本与诊断摘要

六份原始报告的公开副本为 [release-build.json](release-build.json)、[release-installed-gates.json](release-installed-gates.json)、[release-composer-e2e.json](release-composer-e2e.json)、[release-relay-composer-e2e.json](release-relay-composer-e2e.json)、[release-observer-e2e.json](release-observer-e2e.json)、[release-ui-smoke.json](release-ui-smoke.json)。仅对其中 **6 个字符串值的机器路径**作规范化并统一 JSON 排版：源码根、构建根、安装前缀与临时输出目录分别表示为 `${SOURCE_ROOT}`、`${BUILD_ROOT}`、`${INSTALL_PREFIX}`、`${EVIDENCE_TMP}`；其余字段、数值、PID、断言、哈希和退出码不变。

[release-initial-attempt.json](release-initial-attempt.json) 明确标为派生的 `diagnostic_summary`，保留首次尝试的源码、profile、二进制摘要、38／18 项检查进展、退出码、耗时和 missing runner 原因。它不公开原始 daemon 详细日志、用户标识或会话 ID，也不是完整日志的逐字副本。私有回滚归档的实际路径不包含在本批发布文件中。

拆分前原始合并 JSON 的 SHA-256 为：

```text
29d24a9c8188901f949eeecf0f351e14d58f8973022120251806b8dc2305368d
```
