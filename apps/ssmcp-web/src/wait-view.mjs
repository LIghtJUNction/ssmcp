import { plain } from './presentation.mjs';
import { timeEntries, timeDuration, timeStamp, waitLifecycle, waitStatistics, waitRows, promptRows } from './waits.mjs';

/** This view only reads an existing snapshot. It never calls a command or ACK API. */
export function renderWaitContent(root, snapshot, status = '') {
  const signature = JSON.stringify([snapshot, status]);
  if (root.dataset.signature === signature) return;
  const open = new Set([...root.querySelectorAll('details[open]')].map(node => node.dataset.key));
  const document = root.ownerDocument;
  const node = (tag, className, text = '') => {
    const result = document.createElement(tag); result.className = className; result.textContent = text; return result;
  };
  const fields = rows => {
    const result = node('dl', 'wait-fields');
    for (const [label, value] of rows) result.append(node('dt', '', label), node('dd', '', value));
    return result;
  };
  const stats = waitStatistics(snapshot);
  const summary = node('div', 'wait-summary');
  for (const [label, value] of [['实际完成样本', String(stats.sampleCount)], ['平均耗时', timeDuration(stats.meanMs)], ['P50', timeDuration(stats.p50Ms)], ['P95', timeDuration(stats.p95Ms)]]) {
    const metric = node('div', 'metric'); metric.append(node('strong', '', value), node('span', '', label)); summary.append(metric);
  }
  const scope = node('p', 'tiny wait-scope', `范围：当前保留的 ${stats.retained} 项观察，非全历史。只统计具有真实完成元数据的就绪记录；取消、超时和未知结果单独计数。`);
  const range = node('p', 'tiny wait-scope', stats.sampleCount
    ? `样本完成时间：${timeStamp(stats.earliestCompletedAt)} — ${timeStamp(stats.latestCompletedAt)}。分位数使用最近秩法。`
    : '尚无符合条件的耗时样本。旧版本缺失的完成和读取时间保持未知。');
  const counts = node('p', 'wait-counts', `进行中 ${stats.counts.active} · 等待读取 ${stats.counts.unread} · 尝试读取未确认 ${stats.counts.read_attempt} · 已确认 ${stats.counts.acknowledged} · 超时 ${stats.counts.timed_out} · 取消 ${stats.counts.cancelled} · 未知 ${stats.counts.unknown} · 读取窗口结束 ${stats.counts.window_expired}`);
  const content = [node('p', 'tiny wait-refresh-status', status), summary, scope, range, counts];
  const entries = timeEntries(snapshot);
  for (const [title, selected] of [['进行中', entries.filter(entry => waitLifecycle(entry).key === 'active')], ['保留的结果', entries.filter(entry => waitLifecycle(entry).key !== 'active')]]) {
    const section = node('section', 'wait-section'); section.append(node('h3', '', title));
    const grid = node('div', 'wait-list');
    for (const entry of selected) {
      const lifecycle = waitLifecycle(entry), card = node('article', 'wait-card'), heading = node('div', 'wait-card-heading');
      heading.append(node('h4', '', plain(entry.label || entry.id, 256)), node('span', `pill ${lifecycle.tone}`, lifecycle.label));
      const rows = waitRows(entry);
      const primary = new Set(['实际已用', '目标', '引用', '生命周期上限', '生命周期剩余', '本轮读取窗口', '窗口剩余', '操作截止时间', '真实完成时间', '首次读取尝试', '显式确认时间']);
      const details = node('details', 'wait-details'); details.dataset.key = `${entry.id}\0${entry.episode ?? ''}`;
      details.open = open.has(details.dataset.key);
      details.append(node('summary', '', '编号与读取记录'), fields(rows.filter(([label]) => !primary.has(label))));
      card.append(heading, fields(rows.filter(([label]) => primary.has(label))), details); grid.append(card);
    }
    if (!selected.length) grid.append(node('p', 'quiet', title === '进行中' ? '当前没有活动计时或等待。' : '当前快照没有保留的结果。'));
    section.append(grid); content.push(section);
  }
  const schedules = node('section', 'wait-section'); schedules.append(node('h3', '', '定时提示计划'));
  const scheduleList = node('div', 'wait-list');
  for (const prompt of (Array.isArray(snapshot?.prompts) ? snapshot.prompts : []).slice(0, 256)) {
    if (!prompt || typeof prompt !== 'object') continue;
    const card = node('article', 'wait-card'); card.append(fields(promptRows(prompt))); scheduleList.append(card);
  }
  if (!scheduleList.children.length) scheduleList.append(node('p', 'quiet', Array.isArray(snapshot?.prompts) ? '当前连接没有定时提示计划。' : '当前连接未提供计划摘要。'));
  schedules.append(scheduleList); content.push(schedules);
  content.push(node('p', 'tiny wait-scope', '读取时间表示服务端获取缓存内容的尝试；只有显式 ACK 表示调用方确认。读取窗口结束不会取消底层任务，条件就绪也不代表底层任务成功。计划正文和变量不会显示在此页。'));
  root.replaceChildren(...content); root.dataset.signature = signature;
}
