/** Read-only display of the advertised time snapshot. Never acknowledges a result. */
import { plain } from './presentation.mjs';

const finite = value => Number.isSafeInteger(value) && value >= 0;
const object = value => value && typeof value === 'object' && !Array.isArray(value);
const list = value => Array.isArray(value) ? value.slice(0, 256) : [];
const readyStates = new Set(['unread', 'read_attempt', 'acknowledged']);

export function timeDuration(value) {
  if (!finite(value)) return '未知';
  if (value < 1000) return `${value} 毫秒`;
  const seconds = Math.floor(value / 1000);
  if (seconds < 60) return `${seconds} 秒`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`;
  return `${Math.floor(seconds / 3600)} 时 ${Math.floor(seconds % 3600 / 60)} 分 ${seconds % 60} 秒`;
}

export function timeStamp(value) {
  if (!finite(value)) return '未知';
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? '未知' : date.toISOString().replace('T', ' ').replace('Z', ' UTC');
}

export function timeEntries(snapshot) {
  const entries = new Map();
  // A repeated snapshot must not count the same observer episode twice.
  for (const entry of [...list(snapshot?.recent), ...list(snapshot?.active)]) {
    if (!object(entry) || typeof entry.id !== 'string' || !entry.id) continue;
    entries.set(`${entry.id}\0${entry.episode ?? ''}`, entry);
  }
  return [...entries.values()];
}

export function waitLifecycle(entry) {
  const state = (key, label, tone = '') => ({ key, label, tone });
  if (entry.status === 'unknown' || entry.status === 'disconnected' || entry.wait?.outcome === 'unknown') {
    return state('unknown', '结果未知', 'unknown');
  }
  const durable = object(entry.durable) ? entry.durable : null;
  if (durable) {
    if (durable.state === 'waiting' && entry.status === 'running' && !durable.result_status) {
      return state('active', '后台等待中');
    }
    if (durable.result_status === 'cancelled') return state('cancelled', '等待已取消');
    if (durable.result_status === 'timed_out') return state('timed_out', '等待生命周期已到期');
    const unavailable = { unauthorized: '观察权限已失效', unknown_target: '观察目标未知', unavailable: '观察不可用', busy: '观察繁忙' };
    if (unavailable[durable.result_status]) return state('unknown', unavailable[durable.result_status], 'unknown');
    if (durable.result_status === 'ready' && finite(durable.completed_at_ms)) {
      if (durable.state === 'acknowledged') return state('acknowledged', '已显式确认', 'confirmed');
      if (durable.state !== 'cached') return state('unknown', '完成记录状态未知', 'unknown');
      return finite(durable.first_read_at_ms)
        ? state('read_attempt', '已尝试读取 · 未确认')
        : state('unread', '已完成 · 等待读取');
    }
    return state('unknown', '结果未知', 'unknown');
  }
  switch (entry.wait?.outcome) {
    case 'triggered': return state('legacy_completed', '条件已触发 · 读取状态未知');
    case 'window_expired': return state('window_expired', '本轮读取窗口已结束');
    case 'observation_stopped': return state('cancelled', '观察已停止');
  }
  switch (entry.status) {
    case 'running': return state('active', entry.kind === 'task' ? '任务进行中' : entry.kind === 'timer' ? '计时中' : '观察中');
    case 'elapsed': return state('elapsed', '计时已到期');
    case 'expired': return state('window_expired', '本轮读取窗口已结束');
    case 'cancelled': return state('cancelled', '已取消');
    case 'returned': return state('legacy_completed', '条件已触发 · 完成元数据未知');
    default: return state('unknown', '结果未知', 'unknown');
  }
}

export function waitStatistics(snapshot) {
  const entries = timeEntries(snapshot).filter(entry => entry.wait || entry.durable);
  const counts = { active: 0, unread: 0, read_attempt: 0, acknowledged: 0, timed_out: 0, cancelled: 0, unknown: 0, window_expired: 0, legacy_completed: 0 };
  const samples = [], completed = [];
  for (const entry of entries) {
    const key = waitLifecycle(entry).key;
    if (key in counts) counts[key] += 1;
    const durable = entry.durable;
    // Ready means the requested condition returned, not that an underlying job succeeded.
    if (readyStates.has(key) && durable?.result_status === 'ready'
        && finite(durable.completed_at_ms) && finite(durable.elapsed_total_ms)) {
      samples.push(durable.elapsed_total_ms); completed.push(durable.completed_at_ms);
    }
  }
  samples.sort((a, b) => a - b);
  const percentile = p => samples.length ? samples[Math.ceil(samples.length * p) - 1] : null;
  return {
    retained: entries.length, counts, sampleCount: samples.length,
    meanMs: samples.length ? Math.round(samples.reduce((sum, ms) => sum + ms, 0) / samples.length) : null,
    p50Ms: percentile(0.5), p95Ms: percentile(0.95),
    earliestCompletedAt: completed.length ? Math.min(...completed) : null,
    latestCompletedAt: completed.length ? Math.max(...completed) : null,
  };
}

/** Whitelist display fields; private bodies and prompt variables never become metadata rows. */
export function waitRows(entry) {
  const durable = object(entry.durable) ? entry.durable : null;
  const rows = [
    ['观察编号', plain(entry.id, 256)],
    ['实际已用', timeDuration(entry.elapsed_ms)],
    ['目标', list(entry.wait?.targets).map(value => plain(value, 512)).filter(Boolean).join('\n') || '未提供'],
    ['引用', list(entry.wait?.references).map(value => plain(value, 1024)).filter(Boolean).join('\n') || '未提供'],
  ];
  if (durable) {
    rows.push(
      ['生命周期上限', timeDuration(durable.lifetime_ms)],
      ['生命周期剩余', entry.status === 'running' ? timeDuration(entry.remaining_ms) : '已结束或未知'],
      ['本轮读取窗口', timeDuration(durable.read_window_ms)],
      ['开始时间', timeStamp(durable.started_at_ms)],
      ['真实完成时间', timeStamp(durable.completed_at_ms)],
      ['首次读取尝试', timeStamp(durable.first_read_at_ms)],
      ['最近读取尝试', timeStamp(durable.last_read_at_ms)],
      ['显式确认时间', timeStamp(durable.acknowledged_at_ms)],
      ['首次读取尝试延迟', timeDuration(durable.delivery_lag_ms)],
      ['服务端保留截止', timeStamp(durable.retained_until_ms)],
      ['读取窗口次数', finite(durable.read_windows) ? String(durable.read_windows) : '未知'],
      ['结果重读次数', finite(durable.replay_count) ? String(durable.replay_count) : '未知'],
    );
  } else {
    rows.push(['本轮读取窗口', timeDuration(entry.wait?.window_ms)], ['窗口剩余', timeDuration(entry.remaining_ms)],
      ['完成 / 读取 / 确认时间', '当前记录未提供'], ['首次读取尝试延迟', '未知']);
  }
  if (entry.deadline_unix_ms !== null && entry.deadline_unix_ms !== undefined) rows.push(['操作截止时间', timeStamp(entry.deadline_unix_ms)]);
  if (entry.attribution?.agent) rows.push(['观察者', `${plain(entry.attribution.agent.name, 128)} @ ${plain(entry.attribution.account?.login, 128)}`]);
  return rows;
}

export function promptRows(prompt) {
  const status = { scheduled: '已安排', paused: '已暂停', completed: '已完成', cancelled: '已取消', expired: '已到期', disconnected: '连接已断开' };
  return [
    ['计划编号', plain(prompt?.id, 256)], ['状态', status[prompt?.status] ?? '未知'],
    ['下次触发', prompt?.status === 'scheduled' ? timeStamp(prompt.next_fire_unix_ms) : '当前无下一次触发'],
    ['间隔', prompt?.interval_ms === null ? '单次计划' : timeDuration(prompt?.interval_ms)],
    ['待确认', finite(prompt?.pending_count) ? String(prompt.pending_count) : '未知'],
    ['合并或错过的触发', finite(prompt?.missed_count) ? String(prompt.missed_count) : '未知'],
    ['最近触发', timeStamp(prompt?.last_fired_at_unix_ms)],
    ['计划到期', timeStamp(prompt?.expires_at_unix_ms)],
    ['作用范围', prompt?.scope === 'native_connection' ? '当前原生连接' : '未知'],
  ];
}
