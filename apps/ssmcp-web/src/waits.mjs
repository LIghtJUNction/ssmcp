import { t } from './locale.mjs';
/** Read-only display of the advertised time snapshot. Never acknowledges a result. */
import { plain } from './presentation.mjs';

const finite = value => Number.isSafeInteger(value) && value >= 0;
const object = value => value && typeof value === 'object' && !Array.isArray(value);
const list = value => Array.isArray(value) ? value.slice(0, 256) : [];
const readyStates = new Set(['unread', 'read_attempt', 'acknowledged']);

export function timeDuration(value) {
  if (!finite(value)) return t('unknown');
  if (value < 1000) return t('durationMilliseconds', value);
  const seconds = Math.floor(value / 1000);
  if (seconds < 60) return t('durationSeconds', seconds);
  if (seconds < 3600) return t('durationMinutes', Math.floor(seconds / 60), seconds % 60);
  return t('durationHoursSeconds', Math.floor(seconds / 3600), Math.floor(seconds % 3600 / 60), seconds % 60);
}

export function timeStamp(value) {
  if (!finite(value)) return t('unknown');
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? t('unknown') : date.toISOString().replace('T', ' ').replace('Z', ' UTC');
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
    return state('unknown', t('resultUnknown'), 'unknown');
  }
  const durable = object(entry.durable) ? entry.durable : null;
  if (durable) {
    if (durable.state === 'waiting' && entry.status === 'running' && !durable.result_status) {
      return state('active', t('waitingInBackground'));
    }
    if (durable.result_status === 'cancelled') return state('cancelled', t('waitCanceled'));
    if (durable.result_status === 'timed_out') return state('timed_out', t('waitLifetimeExpired'));
    const unavailable = { unauthorized: t('observationPermissionExpired'), unknown_target: t('unknownObservationTarget'), unavailable: t('observationUnavailable'), busy: t('observationBusy') };
    if (unavailable[durable.result_status]) return state('unknown', unavailable[durable.result_status], 'unknown');
    if (durable.result_status === 'ready' && finite(durable.completed_at_ms)) {
      if (durable.state === 'acknowledged') return state('acknowledged', t('acknowledged'), 'confirmed');
      if (durable.state !== 'cached') return state('unknown', t('unknownCompletionRecord'), 'unknown');
      return finite(durable.first_read_at_ms)
        ? state('read_attempt', t('readAttemptUnconfirmed'))
        : state('unread', t('readyToRead'));
    }
    return state('unknown', t('resultUnknown'), 'unknown');
  }
  switch (entry.wait?.outcome) {
    case 'triggered': return state('legacy_completed', t('unknownReadStatus'));
    case 'window_expired': return state('window_expired', t('readWindowEnded'));
    case 'observation_stopped': return state('cancelled', t('observationStopped'));
  }
  switch (entry.status) {
    case 'running': return state('active', entry.kind === 'task' ? t('taskInProgress') : entry.kind === 'timer' ? t('timerRunning') : t('watchInProgress'));
    case 'elapsed': return state('elapsed', t('timerExpired'));
    case 'expired': return state('window_expired', t('readWindowEnded'));
    case 'cancelled': return state('cancelled', t('canceled'));
    case 'returned': return state('legacy_completed', t('unknownCompletionMetadata'));
    default: return state('unknown', t('resultUnknown'), 'unknown');
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
    [t('observationId'), plain(entry.id, 256)],
    [t('elapsed'), timeDuration(entry.elapsed_ms)],
    [t('target'), list(entry.wait?.targets).map(value => plain(value, 512)).filter(Boolean).join('\n') || t('unavailable')],
    [t('references'), list(entry.wait?.references).map(value => plain(value, 1024)).filter(Boolean).join('\n') || t('unavailable')],
  ];
  if (durable) {
    rows.push(
      [t('lifetimeLimit'), timeDuration(durable.lifetime_ms)],
      [t('lifetimeRemaining'), entry.status === 'running' ? timeDuration(entry.remaining_ms) : t('endedOrUnknown')],
      [t('currentReadWindow'), timeDuration(durable.read_window_ms)],
      [t('startedAt'), timeStamp(durable.started_at_ms)],
      [t('actualCompletion'), timeStamp(durable.completed_at_ms)],
      [t('firstReadAttempt'), timeStamp(durable.first_read_at_ms)],
      [t('mostRecentRead'), timeStamp(durable.last_read_at_ms)],
      [t('explicitAcknowledgment'), timeStamp(durable.acknowledged_at_ms)],
      [t('firstReadLatency'), timeDuration(durable.delivery_lag_ms)],
      [t('serverRetentionDeadline'), timeStamp(durable.retained_until_ms)],
      [t('readWindows'), finite(durable.read_windows) ? String(durable.read_windows) : t('unknown')],
      [t('resultRereads'), finite(durable.replay_count) ? String(durable.replay_count) : t('unknown')],
    );
  } else {
    rows.push([t('currentReadWindow'), timeDuration(entry.wait?.window_ms)], [t('readWindowRemaining'), timeDuration(entry.remaining_ms)],
      [t('waitCompletionReadAck'), t('noWaitRecord')], [t('firstReadLatency'), t('unknown')]);
  }
  if (entry.deadline_unix_ms !== null && entry.deadline_unix_ms !== undefined) rows.push([t('operationDeadline'), timeStamp(entry.deadline_unix_ms)]);
  if (entry.attribution?.agent) rows.push([t('observer'), `${plain(entry.attribution.agent.name, 128)} @ ${plain(entry.attribution.account?.login, 128)}`]);
  return rows;
}

export function promptRows(prompt) {
  const status = { scheduled: t('scheduled'), paused: t('paused'), completed: t('completed'), cancelled: t('canceled'), expired: t('expired'), disconnected: t('connectionClosed') };
  return [
    [t('scheduleId'), plain(prompt?.id, 256)], [t('status'), status[prompt?.status] ?? t('unknown')],
    [t('nextTrigger'), prompt?.status === 'scheduled' ? timeStamp(prompt.next_fire_unix_ms) : t('noNextTrigger')],
    [t('interval'), prompt?.interval_ms === null ? t('once') : timeDuration(prompt?.interval_ms)],
    [t('pendingAcknowledgment'), finite(prompt?.pending_count) ? String(prompt.pending_count) : t('unknown')],
    [t('missedTriggers'), finite(prompt?.missed_count) ? String(prompt.missed_count) : t('unknown')],
    [t('latestTrigger'), timeStamp(prompt?.last_fired_at_unix_ms)],
    [t('scheduleExpiry'), timeStamp(prompt?.expires_at_unix_ms)],
    [t('scope'), prompt?.scope === 'native_connection' ? t('nativeConnection') : t('unknown')],
  ];
}
