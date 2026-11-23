# SSMCP 当前迭代：Zuse CLI 迁移已进入源码

**更新于 2026-11-21 15:19:08 UTC。** 当前已核对开发分支 `work/zuse-cli-only-20261121` 的固定提交 [`659354f`](https://github.com/LIghtJUNction/ssmcp/commit/659354f9b6361b862bd3069c1df47fdf7d65b2bd)。相对旧候选 `02aa4dc`，该快照包含 **7 个提交、43 个文件变化**；它仍处于开发和集成阶段，报告组尚未取得新架构的编译、产品运行或发布通过回执。[源码快照与审查事实](migration-zuse-cli-659354-development-public-facts.json)

## 已进入代码的变化

迁移方向保持不变：实际桌面及浏览器操作直接使用 **Zuse CLI**；SSMCP 的 computer 工具仅负责**版本查询、下载、安装、卸载**。已读取的注册规则和 JavaScript 入口对应 `computer.version`、`computer.download`、`computer.install`、`computer.uninstall`，不在此提前承诺最终参数或运行结果。

该固定 diff 已移除旧 CDP、Playwright、Omarchy 实现及隐藏 worker 入口，并移除直接的 `zuse-computer` Rust 依赖。新归档与发行包处理模块已经提交；其余生命周期集成、锁文件和子模块状态仍以原负责人的最终源码为准。[固定源码差异](https://github.com/LIghtJUNction/ssmcp/compare/02aa4dcd18786062ef44be6c76422dff054fbfed...659354f9b6361b862bd3069c1df47fdf7d65b2bd)

源码负责人已回报，由现有 ChatGPT 协作者在独立分支逐步提交，未启动新 native 模型，也未恢复旧 native 作者会话。仓库清理仍由原负责人执行；报告组目前未取得清理完成回执，不记录已释放空间。

## 本快照的两项静态发现

| 问题 | 已核对依据与影响 | 当前状态 |
|---|---|---|
| Windows ZIP 被误要求 Unix 执行权限 | [新 archive.rs](https://github.com/LIghtJUNction/ssmcp/blob/659354f9b6361b862bd3069c1df47fdf7d65b2bd/src/computer/archive.rs) 对 `zuse.exe` 也要求 Unix 执行位；[官方 Windows 配方](https://github.com/LIghtJUNction/zuse/blob/830fcefd2277cbba46536c67d206f29058ba7fb3/scripts/release.py) 未改变 [CPython 的 DOS-origin 默认值](https://github.com/python/cpython/blob/ebf955df7a89ed0c7968f79faec1de49f61ed7cb/Lib/zipfile/__init__.py)，[zip 6.0.0](https://github.com/zip-rs/zip2/blob/abfc23d19e5c6038cba27eb3a897bc34c6d5bf86/src/types.rs) 据此给出无执行位的普通文件 mode。 | 已交原作者。未下载实际 ZIP、未运行安装，不称为实机复现。 |
| 退役快捷键编码可能进入草稿 | paste 外收到旧 Kitty Super+Alt+C 序列时，[键盘解码](https://github.com/LIghtJUNction/ssmcp/blob/659354f9b6361b862bd3069c1df47fdf7d65b2bd/src/client/terminal.rs)会把它送入普通字符路径；ESC 可触发返回／恢复逻辑，其余编码可被[插入草稿](https://github.com/LIghtJUNction/ssmcp/blob/659354f9b6361b862bd3069c1df47fdf7d65b2bd/src/client/terminal/conversation.rs)。 | 已交原作者。条件是该序列确实到达应用；paste 内字面内容应继续保留。 |

报告组已补读上述关键代码路径与 Windows ZIP 配方／库规则。对 `/send`、引用准备、补全和回执主路径的限定静态审查未见直接改写；这不构成整套迁移的兼容性或运行通过结论。

## 版本来源与后续验证范围

预编译下载模块针对 GitHub Release。2026-11-21 15:09:24 UTC 的独立读取确认 [GitHub 正式发行包为 v0.3.0](https://github.com/LIghtJUNction/zuse/releases/tag/v0.3.0)，[官方 crate 索引](https://github.com/rust-lang/crates.io-index/blob/master/zu/se/zuse-computer)当时另有未撤回的 `zuse-computer 0.4.0`；二者属于不同渠道。这里没有执行下载、安装、卸载或真实桌面／浏览器操作。

旧 `tests/zuse_worker_entry.rs` 已删除，原对应 worker 验证目标及旧阶段总数不能继续照搬。原 H2 准备物仍是静态、未执行材料；其中四个 gitlink／子模块的假设需按最终源码处理。六个发布 recipe 在本快照未变，这本身不支持放宽原 workflow、源码、锁图和资产字节绑定要求。新验证范围由实际集成后的代码确定。

## 保留的旧候选里程碑

**Gate20 的 35 个阶段通过**仍只属于旧 `02aa4dc`，原件已由报告组独立读取；[完整报告](current-iteration-Gate20-passed.md)及[最终事实](gate-S02aa-r20-final-public-facts.json)保持原样。**N20 一次来源校验通过**目前为原验收方报告，报告组未独立读取 proof／receipt；后续旧候选产品运行及发布已因架构变更暂停。

上述旧结果不会转移给新源码。旧内置输入开关、generation gate、控制边界与 EOF 分析均保留历史归属，不据此声称它们约束独立 Zuse CLI。下一步跟进两项修正、最终集成提交及必要验证，继续推进发布。
