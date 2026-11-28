import { t } from './locale.mjs';
import { counter, compareCounters } from './protocol.mjs';

const names = {
  'ssmcp.tool_call': t('toolCall'), answer: t('answer'), progress: t('progress'), input: t('input'), 'ssmcp.status': t('status'),
  'ssmcp.worker.started': t('workerStarted'), 'ssmcp.worker.progress': t('workerProgress'),
  'ssmcp.worker.completed': t('workerCompleted'), 'ssmcp.worker.idle': t('workerIdle'),
  'ssmcp.event': t('event'), 'ssmcp.terminal': t('terminalOutput'), 'ssmcp.finish': t('callCompleted'),
  'ssmcp.backend': t('backendState'), receipt: t('commandReceipt'), notice: t('observationLog'),
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
  return plain(value.type ?? t('event'));
}

export const inputOriginLabel = origin => ({
  human: t('user'), system_hook: t('systemPrompt'), agent_timer: t('timerPrompt'),
  worker_event: t('workerEvent'), maintenance: t('maintenancePrompt'),
}[origin] || t('input'));
export const validMessageId = value => typeof value === 'string' && /^[A-Za-z0-9_-]{1,128}$/.test(value);

/** Quotes are editable draft text; the existing send receipt covers the whole text. */
export function quoteReply(messageId, text, draft = '', maximum = 16_384) {
  if (!validMessageId(messageId)) throw new Error(t('missingQuoteId'));
  const bytes = value => new TextEncoder().encode(value).length;
  const suffix = t('quoteTruncated');
  let quote = t('quotePrefix') + messageId + '\n> ';
  const reserve = bytes(draft) + bytes('\n\n') + bytes(suffix);
  if (bytes(quote) + reserve > maximum) throw new Error(t('quoteDraftTooLong'));
  let used = 0, truncated = false;
  const clean = plain(text, 32_768);
  for (const char of clean) {
    const addition = char === '\n' ? '\n> ' : char;
    const size = bytes(char);
    if (used + size > 2048 || bytes(quote) + bytes(addition) + reserve > maximum) {
      truncated = true; break;
    }
    quote += addition; used += size;
  }
  if (truncated || clean.length < text.length) quote += suffix;
  return quote + '\n\n' + draft;
}

function messageProjection(value, projected = {}) {
  const native = ['ssmcp.message', 'ssmcp.user_input'].includes(value.type);
  const human = native && (value.type === 'ssmcp.user_input' || value.role === 'user' || projected.input_origin === 'human');
  const inputSide = human || native && (value.kind === 'input' || projected.input_side === true);
  const messageId = native && validMessageId(projected.message_id ?? value.message_id)
    ? projected.message_id ?? value.message_id : null;
  const inputOrigin = human ? 'human' : inputSide ? projected.input_origin ?? value.input_origin ?? 'system_hook' : null;
  return { messageId, inputSide, inputOrigin };
}

/** Identity is projected only from the authenticated envelope, never payload.source. */
export function projectEvent(event, generation) {
  const frame = event.value ?? {};
  const value = frame.kind === 'event' ? frame.value ?? {} : frame;
  let kind = value.type ?? event.topic;
  const message = messageProjection(value);
  if (message.inputSide) kind = 'input';
  else if (value.type === 'ssmcp.message') kind = value.kind === 'answer' ? 'answer' : 'progress';
  const source = frame.source && typeof frame.source === 'object' ? frame.source : { kind: 'unattributed' };
  const text = frame.kind === 'terminal' ? decodeBase64(frame.data_base64) : textProjection(value);
  return {
    id: `event:${generation}:${event.sequence}`, generation, sequence: event.sequence,
    kind, text, source, ...message, topic: event.topic,
    terminalId: frame.kind === 'terminal' ? frame.terminal_id : null,
    title: message.inputSide ? inputOriginLabel(message.inputOrigin) : value.tool || kindLabel(kind), status: value.status ?? value.phase ?? null,
    metadata: event,
  };
}

export function projectHistory(event, generation) {
  counter(event.sequence, t('conversationSequence'));
  const message = messageProjection(event.value ?? {}, event);
  const kind = message.inputSide ? 'input' : event.kind || 'event';
  return {
    id: `event:${generation}:${event.sequence}`, generation, sequence: event.sequence,
    kind, ...message, text: plain(event.text), source: event.source ?? { kind: 'unattributed' },
    title: message.inputSide ? inputOriginLabel(message.inputOrigin) : event.value?.tool || kindLabel(kind),
    status: event.status ?? event.phase ?? null, metadata: event,
  };
}

export function validateHistory(value, cursor) {
  if (!value || value.schema_version !== 1 || !Array.isArray(value.events) || value.events.length > 64
      || typeof value.has_more !== 'boolean' || typeof value.gap !== 'boolean') throw new Error(t('invalidConversationHistory'));
  for (const key of ['start_cursor', 'next_cursor', 'end_cursor']) counter(value[key], key);
  if (compareCounters(value.start_cursor, value.next_cursor) > 0 || compareCounters(value.next_cursor, value.end_cursor) > 0
      || value.has_more !== (compareCounters(value.next_cursor, value.end_cursor) < 0)) throw new Error(t('invalidConversationCursor'));
  if (cursor !== undefined && (compareCounters(value.next_cursor, cursor) < 0 || value.gap !== (compareCounters(cursor, value.start_cursor) < 0))) throw new Error(t('conversationCursorMismatch'));
  let previous = null;
  for (const event of value.events) {
    counter(event.sequence);
    if (compareCounters(event.sequence, value.start_cursor) < 0 || compareCounters(event.sequence, value.next_cursor) >= 0
        || (previous !== null && compareCounters(event.sequence, previous) <= 0)
        || (cursor !== undefined && compareCounters(event.sequence, cursor) < 0)) throw new Error(t('invalidConversationOrder'));
    previous = event.sequence;
  }
  return value;
}

export function duration(value) {
  try {
    const seconds = BigInt(counter(value)) / 1000n;
    if (seconds < 60n) return t('durationSeconds', seconds);
    if (seconds < 3600n) return t('durationMinutes', seconds / 60n, seconds % 60n);
    return t('durationHours', seconds / 3600n, seconds % 3600n / 60n);
  } catch { return '—'; }
}

// Metadata is for inspection, never a source of HTML or command authority.
export function redact(value, depth = 0) {
  if (depth > 8) return t('truncatedNesting');
  if (typeof value === 'string') return plain(value, 8192);
  if (Array.isArray(value)) return value.slice(0, 128).map(item => redact(item, depth + 1));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).slice(0, 128).map(([key, item]) => [
      key, /token|secret|password|authorization|cookie/i.test(key) ? t('redacted') : redact(item, depth + 1),
    ]));
  }
  return value;
}
