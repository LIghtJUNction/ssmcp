# /send 引用补全：真实终端验收

本记录保存已经完成的 /send 文件与技能引用补全验收。功能已在 [PR #116](https://github.com/LIghtJUNction/ssmcp/pull/116) 合入，导航夹具修正在 [PR #118](https://github.com/LIghtJUNction/ssmcp/pull/118) 合入。

## 实际行为

在 /send 消息正文输入 @文件 或 $技能 时，选择器可正常打开。第一次 Enter 或 Tab 选择候选项，随后 Enter 才发送正文。缺失引用保留可见提示；接收者选择和群组路由保持原来的含义。

## 验证范围

32 项针对性 Rust 测试通过：补全 9 项、技能选择 2 项、本地监听器 8 项、分组 13 项。真实私有路由器、WebSocket、本地监听器与 PTY 联合验收通过 114 项检查，覆盖 48 列及 120 列。

二进制对应产品源码 cda2aeb6a61d9950b48ee213cca8e70714cf7e29；修正后的夹具内容提交为 daa2cc9ca82d3acf8094ff4978e73ea495fcf524。验收前后输入和可执行文件摘要一致。此处不把后续主分支或新版本自动视为已通过相同验收。

机器可读的检查数量、二进制摘要、夹具摘要和清理结果见 [report.json](report.json)。本页使用合成测试内容，没有启动外部模型，也没有重启现有生产服务。

## 可见结果

PNG 归档保留原目录 `docs/reports/send-reference-completion-20261120/`；下列链接指向云盘 ZIP，括号中是该目录内的文件名。完整清单见 [归档清单](../generated-png-manifest-20261121.json)。

### 48 列文件候选列表

[48 列文件候选列表](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `message-references-send-file-picker-48.png`）

### 120 列技能候选列表

[120 列技能候选列表](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `message-references-send-skill-picker-120.png`）

### 文件选择后保留消息正文

[文件选择后的消息正文](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `message-references-send-file-accepted-120.png`）

### 缺失引用的明确提示

[48 列缺失引用提示](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `message-references-send-missing-file-48.png`）

## 失败记录

前两次 PTY 尝试使用了过时的 Agents 导航名称或标题解析，因此失败；随后修正为实际 Sessions 导航后，第三次完整记录通过 114 项检查。原始失败记录仍保留，没有改写为成功。
