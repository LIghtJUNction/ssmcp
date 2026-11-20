# 0.10.16 跨平台发布门禁修复

0.10.15 的正式工作流 [44693898136](https://github.com/LIghtJUNction/ssmcp/actions/runs/44693898136) 已完成。六个平台的 release 构建和两个 Linux 安装后 server 检查成功，客户端/工作区检查未全部通过，因此 finalize、GitHub Release、crates.io 和部署均跳过。原标签 `v0.10.15` 保持指向 `2f396f44483bd1ded2686e8ebc5a4858dfb646e8`，没有移动或重复执行相同失败源码。

失败收敛为三类：

- Windows x64/ARM：截图目录构建器的可变性仅被 Unix 使用；速率 Ready 状态的生产构造条件是 Unix，而便携平台没有采样器。仅在对应局部声明中处理，不关闭全局告警。
- macOS Intel/ARM：真实 Unix datagram 满队列测试遇到非 WouldBlock 错误。Apple 的 [sendto(2) 文档](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/sendto.2.html)说明 `ENOBUFS` 可表示暂时缺少缓冲区或队列已满。发送端现在只把这个具体 errno 规范为 WouldBlock，保留原始错误作来源；真实满队列/订阅者断开测试原样保留，其他错误不会被吞掉。
- Linux x64/ARM：资源目录测试仍期待已移除的 invite 工具。同步三个旧目录断言，保留完整目录验证、两代协议和原生实际调用，没有恢复邀请功能或放宽账号权限。

0.10.16 使用既有 release.py 完成应用版本和锁文件递增。Release 的 `deferred_versions` 同时保留 0.10.15 和 0.10.16；原 84 项发布政策测试继续执行，两个版本都覆盖 tag push 和各类 dispatch Boolean 输入，始终输出 `deploy_server=false`。只有独立确认不会中断现有连接后，才能安排服务器切换。

[本地检查记录](local-validation.json)只记录实际执行结果；正式六平台结果、registry 发布和服务器部署必须分别核实。
