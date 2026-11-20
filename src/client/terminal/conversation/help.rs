//! Localized native UI help. Command tokens remain identical in both languages.
use crate::client::language::Language;

pub(super) fn body(language: Language) -> &'static str {
    language.text(r###"## Write and send
Enter: queue a requirement; continue writing the next draft.
Ctrl+J / Shift+Enter: newline. Tab / Shift+Tab: switch focus.
Alt+Up/Down: increase/decrease visible input rows; keeps your draft and cursor.
Up/Down: move over wrapped input lines; then history.
Empty input + Up: take back the latest queued request before input history. Wait for confirmation; delivered requests cannot be taken back.
Ctrl+C: clear the current input; on empty input, show an exit reminder, then press again to exit. Any other input cancels the exit reminder. Other queued requests stay unchanged.
Left/Right: move cursor; at a draft boundary, move to the adjacent panel. Home/End: line start/end.
Ctrl+A/E: line start/end. Ctrl+U/K: delete before/after cursor.
Ctrl+W: delete word. Backspace/Delete: edit text.

## Agents and questions
Left at draft start: move to Agents; Up/Down select; Enter chooses. Alt+Left/Right always moves between panels.
Esc: return without changing recipient or draft.
Shift+Up or /questions: answer pending questions; repeat for next question.
Enter a choice number or a free answer; Enter saves it.
Esc: question list; Delete in list skips; Esc again returns.
Questions never pause the agent's other work.

## Shell commands
!command: execute with your local shell in the workspace.
Uses the ASCII ! prefix; runs asynchronously with output and exit status here.

## Tasks
/tasks: open Tasks immediately to the right of Logs; refreshes while visible.
Up/Down select a job; Enter opens details; Esc returns to the list or input.
PgUp/PgDn and the mouse wheel scroll.
/tasks cancel ID: explicitly cancel that local job.

## Files, skills and paste
@path or @"path with spaces": reference a workspace file/image.
$skill-name: reference an effective project/global skill.
Paste over 8 KiB: saved as @file; add your request after it.
Text paste limit: 4 MiB; image paste limit: 16 MiB. Input/expanded requirement limit: 16 KiB.
Ctrl+V or /paste: import clipboard text or image.
/attach PATH: import an image as a private file reference.
/copy: copy the latest agent reply to the clipboard.

## GitHub account
/login: prepare GitHub sign-in and open the browser.
/login copy: copy the pending login link; /login link: display it here.
Repeat these commands to use the same pending authorization.
Finish GitHub sign-in and confirm the device; the current session stays connected.

## Review and copy
Tab: browse content. Enter: open or toggle the selected record.
Left/Right: move between canvas panels; Esc: back. Wheel/PgUp/PgDn: scroll content.
Ctrl+Home: oldest retained output. Ctrl+End: return to the latest and follow new activity.
The footer shows how much output is below your position.
Right-click or /logs: move to Logs; Esc/right-click returns to the previous panel.
Logs and Tasks are adjacent canvas panels; Left/Right moves between them.
/raw also toggles Logs. Full output stays available there.
/diff single|split: stack changes or show old/new lines side by side.
Split uses a single column on narrow terminals.
/new [chat|agents|audit|queue|questions|history]: add a panel to the right.
/close: close this panel; the session, drafts and running tasks are kept.
Canvas navigation does not wrap; each panel remembers its selection and scroll position.
F7 or /select: freeze display and use your terminal's native mouse selection.
Copy using your terminal's Copy shortcut or menu (often Ctrl+Shift+C).
F7/Esc resumes updates and restores interface scrolling and clicks.
On terminals supporting it, Shift+drag selects directly without entering copy mode.
/copy still copies only the latest agent reply.

## Tool results
Verified consecutive tool calls share a bounded container.
On the left half, scroll the output, then its container, then the conversation at each edge. The right half always scrolls the conversation.
Enter expands a selected result; PgUp/PgDn and Home/End use the same layers.

## Queued requests
/queue: Up/Down select; Enter take back; i/I inspect without editing.
Delete cancels a revocable request. R copies delivered text.
For uncertain/offline items, R opens explicit review before resending.
Taking back waits for server acknowledgement; delivery may win.
○ means waiting or unconfirmed. ● Read means delivered in a tool response; it does not prove model understanding or execution.

## Computer input
/computer status: inspect the local host-input handoff.
/computer host [seconds]: hand over host input locally; default 15s, maximum 60s.
/computer stop: return control immediately. Any input in this terminal also stops it.
Other applications are not monitored for input; use isolated desktop/background browser while you work.

## Appearance and history
/language en|zh: switch the interface immediately and save the choice. English is the default.
User input, paths, tool output and protocol fields stay unchanged.
F1: help. F2: next view. F3: sidebar. F4: theme.
F5 reloads appearance. F8 saves appearance. F9 changes log category.
/reload restarts the client; /resume reopens saved local conversation.
Conversation history never automatically replays tools or requests.
/handoff [note]: export current context and prepare an @file draft.
Choose the recipient, review the draft, then Enter to send.

## Local commands
"###, r###"## 输入与发送
Enter：将需求加入队列，然后可继续编写下一份草稿。
Ctrl+J / Shift+Enter：换行。Tab / Shift+Tab：切换焦点。
Alt+↑/↓：增加或减少输入框可见行数，保留草稿和光标。
↑/↓：在折行后的输入中移动；到达边缘后浏览输入历史。
空输入框按 ↑：优先取回最近的待发送请求。请等待确认；已送达的请求不能取回。
Ctrl+C：清空当前输入；空输入时先显示退出提醒，再按一次退出。其他按键会取消提醒。其他队列项保持不变。
←/→：移动光标；到达草稿边缘后切换相邻面板。Home/End：行首或行尾。
Ctrl+A/E：行首或行尾。Ctrl+U/K：删除光标前或后的内容。
Ctrl+W：删除前一个词。Backspace/Delete：编辑文本。

## 智能体与问题
草稿开头按 ←：进入智能体面板；↑/↓ 选择，Enter 确认。Alt+←/→ 始终切换面板。
Esc：返回，保留接收方和草稿。
Shift+↑ 或 /questions：回答待处理问题；再次操作进入下一个问题。
输入选项编号或自由回答，按 Enter 保存。
Esc：返回问题列表；在列表中按 Delete 跳过；再次按 Esc 返回。
问题不会暂停智能体的其他工作。

## Shell 命令
!command：在工作区内通过本机 shell 执行命令。
使用 ASCII ! 前缀；命令异步运行，输出和退出状态在此显示。

## 任务
/tasks：在日志右侧打开任务面板；面板可见时刷新状态。
↑/↓ 选择作业，Enter 打开详情，Esc 返回列表或输入框。
PgUp/PgDn 和鼠标滚轮滚动内容。
/tasks cancel ID：明确取消对应的本机作业。

## 文件、技能与粘贴
@path 或 @"含空格的路径"：引用工作区文件或图片。
$skill-name：引用当前项目或全局生效的技能。
超过 8 KiB 的文本粘贴会保存为 @文件；在引用后补充需求。
文本粘贴上限 4 MiB，图片粘贴上限 16 MiB。输入或展开后的需求上限 16 KiB。
Ctrl+V 或 /paste：导入剪贴板文本或图片。
/attach PATH：将图片导入为私有文件引用。
/copy：将智能体最近的回复复制到剪贴板。

## GitHub 账号
/login：准备 GitHub 登录并打开浏览器。
/login copy：复制待处理的登录链接；/login link：在此显示链接。
重复执行这些命令会使用同一次待处理授权。
完成 GitHub 登录并确认设备；当前会话保持连接。

## 浏览与复制
Tab：浏览内容。Enter：打开或切换所选记录的展开状态。
←/→：切换面板；Esc：返回。滚轮 / PgUp / PgDn：滚动内容。
Ctrl+Home：最早保留的输出。Ctrl+End：回到最新内容并跟随新活动。
底栏显示当前位置下方还有多少内容。
右键或 /logs：进入日志；Esc 或右键返回上一面板。
日志与任务是相邻面板，可使用 ←/→ 切换。
/raw 也可切换日志，完整输出仍可在那里查看。
/diff single|split：上下排列差异，或将新旧行并排显示。
窄窗口中的并排模式会使用单列。
/new [chat|agents|audit|queue|questions|history]：在右侧新增面板。
/close：关闭当前面板，保留会话、草稿和运行中的任务。
面板切换不会循环；每个面板保存自己的选择和滚动位置。
F7 或 /select：暂停界面重绘，使用终端原生鼠标拖选。
使用终端的复制快捷键或菜单复制（通常为 Ctrl+Shift+C）。
F7/Esc 恢复更新、界面滚动与点击。
在支持该功能的终端中，可直接 Shift+拖动选择，无需进入选择模式。
/copy 只复制智能体最近的回复。

## 工具结果
连续工具调用按确认的来源分组，显示在高度有上限的区域内。
左半区滚轮优先滚动输出，触边后滚动工具组，再触边后滚动会话。右半区始终滚动整个会话。
Enter 展开选中的结果；PgUp/PgDn 和 Home/End 使用相同层级。

## 请求队列
/queue：↑/↓ 选择；Enter 取回；i/I 只查看，不进入编辑。
Delete 取消仍可撤销的请求。R 将已送达的文本复制为新草稿。
对于未确认或离线条目，R 会先打开检查视图，再由你明确重发。
取回操作需要服务端确认；请求也可能先一步送达。
○ 表示待发送或尚未确认。● 已读仅表示已随工具响应送达，不表示模型已理解或执行。

## 电脑输入
/computer status：查看本机输入授权状态。
/computer host [秒数]：在本机授予主机输入权限，默认 15 秒，最多 60 秒。
/computer stop：立即收回控制。在此终端按任意键也会停止授权。
不会监控其他应用的输入；你工作时可使用隔离桌面或后台浏览器。

## 外观与历史
/language en|zh：立即切换界面语言并保存设置，默认英语。
用户输入、路径、工具输出和协议字段保持原文。
F1：帮助。F2：下一视图。F3：侧栏。F4：主题。
F5 重新加载外观，F8 保存外观，F9 切换日志分类。
/reload 重启客户端；/resume 恢复已保存的本机会话。
会话历史不会自动重放工具或请求。
/handoff [备注]：导出当前上下文并准备 @文件草稿。
选择接收方，检查草稿，再按 Enter 发送。

## 本机命令
"###)
}
