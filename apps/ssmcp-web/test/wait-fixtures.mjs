const base = 1_795_000_000_000;
function observation(id, status, elapsed, extra = {}) {
  const running = status === null;
  return { id, episode: 1, kind: 'relay_wait', label: `等待 ${id}`, status: running ? 'running' : status === 'ready' ? 'returned' : status === 'timed_out' ? 'expired' : status === 'cancelled' ? 'cancelled' : 'unknown',
    elapsed_ms: elapsed, remaining_ms: running ? 3600000 - elapsed : 0,
    wait: { operation: 'await', mode: 'all', targets: ['已有 worker 的完成状态'], references: ['task original-task-id · generation original-generation'], outcome: running ? 'pending' : status === 'ready' ? 'triggered' : status === 'timed_out' ? 'window_expired' : status === 'cancelled' ? 'observation_stopped' : 'unknown', window_ms: 3600000 },
    durable: { state: running ? 'waiting' : 'cached', result_status: status, elapsed_total_ms: elapsed, lifetime_ms: 3600000,
      read_window_ms: 55000, started_at_ms: base, completed_at_ms: running ? null : base + elapsed,
      first_read_at_ms: null, last_read_at_ms: null, acknowledged_at_ms: null, delivery_lag_ms: null,
      retained_until_ms: running ? null : base + elapsed + 1800000, read_windows: 3, replay_count: 0, cursor: running ? 0 : 1, ...extra } };
}
export const waitFixture = {
  active: [observation('background-long', null, 83000)],
  recent: [observation('completed-unread', 'ready', 81000), observation('read-unacked', 'ready', 70000, { first_read_at_ms: base + 72000, last_read_at_ms: base + 72000, delivery_lag_ms: 2000 }),
    observation('acknowledged', 'ready', 10000, { state: 'acknowledged', first_read_at_ms: base + 11000, acknowledged_at_ms: base + 12000, delivery_lag_ms: 1000 }),
    observation('cancelled', 'cancelled', 2000), observation('timeout', 'timed_out', 3600000), observation('unknown', 'unavailable', 9000),
    { id: 'legacy', kind: 'relay_wait', label: '旧版本观察', status: 'returned', elapsed_ms: 5000, wait: { outcome: 'triggered', targets: ['旧版本目标'], references: [], window_ms: 55000 } }],
  prompts: [{ id: 'prompt-fixture', status: 'scheduled', scope: 'native_connection', created_at_unix_ms: base,
    expires_at_unix_ms: base + 3600000, lifetime_ms: 3600000, next_fire_unix_ms: base + 120000, interval_ms: 60000,
    last_fired_at_unix_ms: base + 60000, pending_count: 1, missed_count: 2, tick: 3, retention_after_ack_ms: 300000 }],
};
