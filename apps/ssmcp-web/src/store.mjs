import { byteLength, compareCounters } from './protocol.mjs';
import { redact } from './presentation.mjs';

const messageOwner = row => row.source?.recipient_agent_id ?? row.source?.agent_id
  ?? (typeof row.metadata?.target === 'string' ? row.metadata.target : row.metadata?.target?.agent_id) ?? 'main';

export class Timeline {
  constructor({ limit = 300, maxBytes = 768 * 1024 } = {}) {
    this.limit = limit; this.maxBytes = maxBytes; this.rows = []; this.bytes = 0; this.dropped = 0;
  }
  clear() { this.rows = []; this.bytes = 0; this.dropped = 0; }
  put(row) {
    let safe = { ...row, metadata: redact(row.metadata) };
    const existing = this.rows.findIndex(item => item.id === safe.id
      || safe.messageId && item.messageId === safe.messageId && item.generation === safe.generation
        && messageOwner(item) === messageOwner(safe));
    if (existing >= 0) {
      const previous = this.rows[existing];
      if (previous.sequence && !safe.sequence) {
        safe = { ...previous, queueStatus: safe.status };
      } else if (previous.sequence) {
        safe = { ...safe, id: previous.id, sequence: previous.sequence };
      }
    }
    const size = byteLength(safe);
    if (size > this.maxBytes) { this.dropped += 1; return false; }
    if (existing >= 0) { this.bytes -= this.rows[existing]._bytes; this.rows.splice(existing, 1); }
    safe._bytes = size;
    if (safe.sequence) {
      const next = this.rows.findIndex(item => item.generation === safe.generation && item.sequence
        && compareCounters(item.sequence, safe.sequence) > 0);
      this.rows.splice(next < 0 ? this.rows.length : next, 0, safe);
    } else if (existing >= 0) this.rows.splice(existing, 0, safe);
    else this.rows.push(safe);
    this.bytes += size;
    while (this.rows.length > this.limit || this.bytes > this.maxBytes) {
      this.bytes -= this.rows.shift()._bytes;
      this.dropped += 1;
    }
    return existing < 0;
  }
  page({ offset = 0, limit = 40, filter = () => true } = {}) {
    const rows = this.rows.filter(filter);
    const end = Math.max(0, rows.length - offset);
    const start = Math.max(0, end - limit);
    return { rows: rows.slice(start, end), older: start > 0, newer: offset > 0, total: rows.length, dropped: this.dropped };
  }
}

export class CommandLedger {
  constructor(limit = 64) { this.limit = limit; this.records = []; }
  add(record) {
    if (this.records.some(item => item.requestId === record.requestId)) throw new Error('请求 ID 已存在；不能重复派发命令。');
    if (this.records.length >= this.limit) {
      const removable = this.records.findIndex(item => ['confirmed', 'rejected'].includes(item.status));
      if (removable < 0) throw new Error('待确认操作已达到内存上限。请先核对现有请求。');
      this.records.splice(removable, 1);
    }
    this.records.push({ ...record, status: 'sending' });
    return this.records.at(-1);
  }
  update(requestId, patch) {
    const record = this.records.find(item => item.requestId === requestId);
    if (!record) throw new Error('找不到原始命令记录。');
    const safe = redact(patch);
    if (safe.receipt && byteLength(safe.receipt) > 8192) {
      safe.receipt = { truncated: true, preview: JSON.stringify(safe.receipt).slice(0, 8192) };
    }
    Object.assign(record, safe);
    return record;
  }
}
