import test from 'node:test';
import assert from 'node:assert/strict';
import { timeDuration, timeStamp, timeEntries, waitLifecycle, waitStatistics, waitRows, promptRows } from '../src/waits.mjs';

function completed(id, ms, extra = {}) {
  return { id, episode: 1, kind: 'relay_wait', status: 'returned', elapsed_ms: ms, wait: { outcome: 'triggered', targets: ['worker result'], references: ['task original-id'] },
    durable: { state: 'cached', result_status: 'ready', elapsed_total_ms: ms, completed_at_ms: 1_795_000_000_000 + ms, ...extra } };
}

test('statistics use distinct actually completed ready observations and nearest-rank percentiles', () => {
  const a = completed('a', 1000), b = completed('b', 9000, { first_read_at_ms: 1_795_000_010_000 });
  const c = completed('c', 2000, { state: 'acknowledged', acknowledged_at_ms: 1_795_000_020_000 });
  const cancelled = completed('cancelled', 999999, { result_status: 'cancelled' });
  const unknown = { ...completed('unknown', 999999), status: 'unknown' };
  const timeout = completed('timeout', 999999, { result_status: 'timed_out' });
  const incomplete = completed('incomplete', 999999, { completed_at_ms: null });
  const legacy = { id: 'legacy', status: 'returned', elapsed_ms: 999999, wait: { outcome: 'triggered' } };
  const snapshot = { active: [], recent: [a, a, b, c, cancelled, unknown, timeout, incomplete, legacy] };
  const stats = waitStatistics(snapshot);
  assert.equal(timeEntries(snapshot).length, 8);
  assert.equal(stats.sampleCount, 3);
  assert.equal(stats.meanMs, 4000);
  assert.equal(stats.p50Ms, 2000);
  assert.equal(stats.p95Ms, 9000);
  assert.deepEqual([stats.counts.unread, stats.counts.read_attempt, stats.counts.acknowledged], [1, 1, 1]);
  assert.deepEqual([stats.counts.cancelled, stats.counts.unknown, stats.counts.timed_out, stats.counts.legacy_completed], [1, 2, 1, 1]);
  assert.equal(stats.earliestCompletedAt, a.durable.completed_at_ms);
  assert.equal(stats.latestCompletedAt, b.durable.completed_at_ms);
});

test('missing data is unknown, genuine zero duration remains zero, and window expiry is not a job failure', () => {
  assert.equal(timeDuration(null), 'Unknown');
  assert.equal(timeDuration(undefined), 'Unknown');
  assert.equal(timeDuration(0), '0 ms');
  assert.equal(timeStamp(null), 'Unknown');
  assert.equal(waitStatistics({}).meanMs, null);
  assert.equal(waitLifecycle({ status: 'expired', wait: { outcome: 'window_expired' } }).key, 'window_expired');
  assert.equal(waitStatistics({ recent: [completed('zero', 0)] }).meanMs, 0);
  assert.equal(waitLifecycle({ ...completed('stale', 123), status: 'unknown' }).key, 'unknown');
});

test('display keeps lifetime, read window, retrieval attempt and acknowledgement separate', () => {
  const entry = completed('full', 70000, { lifetime_ms: 3600000, read_window_ms: 55000,
    first_read_at_ms: 1_795_000_071_000, delivery_lag_ms: 1000, acknowledged_at_ms: null,
    read_windows: 2, replay_count: 0, private_result: 'PRIVATE_RESULT' });
  const rows = Object.fromEntries(waitRows(entry));
  assert.equal(rows['Lifetime limit'], '1 h 0 min 0 s');
  assert.equal(rows['Current read window'], '55 s');
  assert.equal(rows['Explicit acknowledgment'], 'Unknown');
  assert.equal(rows['First read attempt latency'], '1 s');
  assert.ok(!JSON.stringify(rows).includes('PRIVATE_RESULT'));
  assert.equal(waitLifecycle(entry).key, 'read_attempt');
});

test('prompt summary exposes scheduling metadata only and preserves pending and missed counts', () => {
  const rows = Object.fromEntries(promptRows({ id: 'prompt-1', status: 'paused', interval_ms: 1000,
    next_fire_unix_ms: null, pending_count: 1, missed_count: 3, scope: 'native_connection',
    template: 'PRIVATE_TEMPLATE', variables: { credential: 'PRIVATE_VALUE' } }));
  assert.equal(rows['Next trigger'], 'No upcoming trigger');
  assert.equal(rows['Pending acknowledgment'], '1');
  assert.equal(rows['Combined or missed triggers'], '3');
  assert.ok(!JSON.stringify(rows).includes('PRIVATE_'));
});
