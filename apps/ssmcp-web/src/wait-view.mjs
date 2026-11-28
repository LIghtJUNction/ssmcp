import { t } from './locale.mjs';
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
  for (const [label, value] of [[t('completedSamples'), String(stats.sampleCount)], [t('averageDuration'), timeDuration(stats.meanMs)], ['P50', timeDuration(stats.p50Ms)], ['P95', timeDuration(stats.p95Ms)]]) {
    const metric = node('div', 'metric'); metric.append(node('strong', '', value), node('span', '', label)); summary.append(metric);
  }
  const scope = node('p', 'tiny wait-scope', t('waitStatsScope', stats.retained));
  const range = node('p', 'tiny wait-scope', stats.sampleCount
    ? t('sampleTimeRange', timeStamp(stats.earliestCompletedAt), timeStamp(stats.latestCompletedAt))
    : t('noDurationSamples'));
  const counts = node('p', 'wait-counts', t('waitCounts', stats.counts.active, stats.counts.unread, stats.counts.read_attempt, stats.counts.acknowledged, stats.counts.timed_out, stats.counts.cancelled, stats.counts.unknown, stats.counts.window_expired));
  const content = [node('p', 'tiny wait-refresh-status', status), summary, scope, range, counts];
  const entries = timeEntries(snapshot);
  for (const [title, selected] of [[t('inProgress'), entries.filter(entry => waitLifecycle(entry).key === 'active')], [t('retainedResults'), entries.filter(entry => waitLifecycle(entry).key !== 'active')]]) {
    const section = node('section', 'wait-section'); section.append(node('h3', '', title));
    const grid = node('div', 'wait-list');
    for (const entry of selected) {
      const lifecycle = waitLifecycle(entry), card = node('article', 'wait-card'), heading = node('div', 'wait-card-heading');
      heading.append(node('h4', '', plain(entry.label || entry.id, 256)), node('span', `pill ${lifecycle.tone}`, lifecycle.label));
      const rows = waitRows(entry);
      const primary = new Set([t('elapsed'), t('target'), t('quote'), t('lifetimeLimit'), t('lifetimeRemaining'), t('currentReadWindow'), t('readWindowRemaining'), t('operationDeadline'), t('actualCompletion'), t('firstReadAttempt'), t('explicitAcknowledgment')]);
      const details = node('details', 'wait-details'); details.dataset.key = `${entry.id}\0${entry.episode ?? ''}`;
      details.open = open.has(details.dataset.key);
      details.append(node('summary', '', t('waitIdentifiersAndReads')), fields(rows.filter(([label]) => !primary.has(label))));
      card.append(heading, fields(rows.filter(([label]) => primary.has(label))), details); grid.append(card);
    }
    if (!selected.length) grid.append(node('p', 'quiet', title === t('inProgress') ? t('noActiveWaits') : t('noRetainedResults')));
    section.append(grid); content.push(section);
  }
  const schedules = node('section', 'wait-section'); schedules.append(node('h3', '', t('scheduledPrompts')));
  const scheduleList = node('div', 'wait-list');
  for (const prompt of (Array.isArray(snapshot?.prompts) ? snapshot.prompts : []).slice(0, 256)) {
    if (!prompt || typeof prompt !== 'object') continue;
    const card = node('article', 'wait-card'); card.append(fields(promptRows(prompt))); scheduleList.append(card);
  }
  if (!scheduleList.children.length) scheduleList.append(node('p', 'quiet', Array.isArray(snapshot?.prompts) ? t('noScheduledPrompts') : t('unknownScheduledPrompts')));
  schedules.append(scheduleList); content.push(schedules);
  content.push(node('p', 'tiny wait-scope', t('waitReadGuide')));
  root.replaceChildren(...content); root.dataset.signature = signature;
}
