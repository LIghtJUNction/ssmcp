import { counter, compareCounters } from './protocol.mjs';

const names = {
  'ssmcp.tool_call': '工具调用', answer: '回复', progress: '进展', 'ssmcp.status': '状态',
  'ssmcp.worker.started': 'Worker 开始', 'ssmcp.worker.progress': 'Worker 进展',
  'ssmcp.worker.completed': 'Worker 完成', 'ssmcp.worker.idle': 'Worker 空闲',
  'ssmcp.event': '事件', 'ssmcp.terminal': '终端输出', 'ssmcp.finish': '调用结束',
  'ssmcp.backend': '后端状态', receipt: '操作回执', notice: '观察记录',
};
export const kindLabel = kind => names[kind] || kind;
export const shortId = value => typeof value === 'string' ? value.length > 20 ? `${value.slice(0, 10)}…${value.slice(-6)}` : value : '—';

export function mayClearDraft(status, submitted, current) {
  return status === 'confirmed' && ['text', 'target', 'connection', 'generation', 'epoch']
    .every(key => submitted[key] === current[key]);
}

export function plain(value, maximum = 8192) {
  if (typeof value !== 'string') return '';
  return value.replace(/\x1B\][^\x07]*(?:\x07|\x1B\\)/g, '').replace(/\x1B\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]/g, '').slice(0, maximum);
}

export function decodeBase64(encoded, maximum = 16_384) {
  if (typeof encoded !== 'string' || encoded.length > 256 * 1024) return '';
  try {
    const binary = atob(encoded);
    const bytes = Uint8Array.from(binary.slice(0, maximum), char => char.charCodeAt(0));
    return plain(new TextDecoder().decode(bytes), maximum);
  } catch { return ''; }
}

export function textProjection(value) {
  if (!value || typeof value !== 'object') return plain(String(value ?? ''));
  const parts = ['text', 'message', 'summary', 'stdout', 'stderr', 'output', 'chunk']
    .map(key => plain(value[key])).filter(Boolean);
  if (parts.length) return parts.join('\n').slice(0, 8192);
  const decoded = decodeBase64(value.data_base64 ?? value.params?.deltaBase64, 8192);
  if (decoded) return decoded;
  if (value.command !== undefined) return plain(typeof value.command === 'string' ? value.command : JSON.stringify(value.command));
  return plain(value.type ?? '事件');
}

/** Identity is projected only from the authenticated envelope, never payload.source. */
export function projectEvent(event, generation) {
  const frame = event.value ?? {};
  const value = frame.kind === 'event' ? frame.value ?? {} : frame;
  let kind = value.type ?? event.topic;
  if (value.type === 'ssmcp.message') kind = value.kind === 'answer' ? 'answer' : 'progress';
  const source = frame.source && typeof frame.source === 'object' ? frame.source : { kind: 'unattributed' };
  const text = frame.kind === 'terminal' ? decodeBase64(frame.data_base64) : textProjection(value);
  return {
    id: `event:${generation}:${event.sequence}`, generation, sequence: event.sequence,
    kind, text, source, topic: event.topic,
    terminalId: frame.kind === 'terminal' ? frame.terminal_id : null,
    title: value.tool || kindLabel(kind), status: value.status ?? value.phase ?? null,
    metadata: event,
  };
}

export function projectHistory(event, generation) {
  counter(event.sequence, '对话事件序号');
  return {
    id: `event:${generation}:${event.sequence}`, generation, sequence: event.sequence,
    kind: event.kind || 'event', text: plain(event.text), source: event.source ?? { kind: 'unattributed' },
    title: event.value?.tool || kindLabel(event.kind || 'event'),
    status: event.status ?? event.phase ?? null, metadata: event,
  };
}

export function validateHistory(value, cursor) {
  if (!value || value.schema_version !== 1 || !Array.isArray(value.events) || value.events.length > 64
      || typeof value.has_more !== 'boolean' || typeof value.gap !== 'boolean') throw new Error('对话历史格式无效。');
  for (const key of ['start_cursor', 'next_cursor', 'end_cursor']) counter(value[key], key);
  if (compareCounters(value.start_cursor, value.next_cursor) > 0 || compareCounters(value.next_cursor, value.end_cursor) > 0
      || value.has_more !== (compareCounters(value.next_cursor, value.end_cursor) < 0)) throw new Error('对话历史游标无效。');
  if (cursor !== undefined && (compareCounters(value.next_cursor, cursor) < 0 || value.gap !== (compareCounters(cursor, value.start_cursor) < 0))) throw new Error('对话历史与请求游标不匹配。');
  let previous = null;
  for (const event of value.events) {
    counter(event.sequence);
    if (compareCounters(event.sequence, value.start_cursor) < 0 || compareCounters(event.sequence, value.next_cursor) >= 0
        || (previous !== null && compareCounters(event.sequence, previous) <= 0)
        || (cursor !== undefined && compareCounters(event.sequence, cursor) < 0)) throw new Error('对话历史事件顺序无效。');
    previous = event.sequence;
  }
  return value;
}

export function duration(value) {
  try {
    const seconds = BigInt(counter(value)) / 1000n;
    if (seconds < 60n) return `${seconds} 秒`;
    if (seconds < 3600n) return `${seconds / 60n} 分 ${seconds % 60n} 秒`;
    return `${seconds / 3600n} 时 ${seconds % 3600n / 60n} 分`;
  } catch { return '—'; }
}

// Metadata is for inspection, never a source of HTML or command authority.
export function redact(value, depth = 0) {
  if (depth > 8) return '[层级已截断]';
  if (typeof value === 'string') return plain(value, 8192);
  if (Array.isArray(value)) return value.slice(0, 128).map(item => redact(item, depth + 1));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).slice(0, 128).map(([key, item]) => [
      key, /token|secret|password|authorization|cookie/i.test(key) ? '[已隐藏]' : redact(item, depth + 1),
    ]));
  }
  return value;
}
