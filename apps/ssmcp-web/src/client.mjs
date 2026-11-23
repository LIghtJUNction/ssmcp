import {
  BridgeError, ProtocolError, createRequest, requestId, unwrapResponse,
  validateResponse, validateObservation, counter, identifier,
} from './protocol.mjs';
import { projectEvent, projectHistory, validateHistory } from './presentation.mjs';
import { Timeline, CommandLedger } from './store.mjs';

const supported = ['runtime', 'terminals', 'workers', 'threads', 'chat.queue', 'time'];
const paged = new Set(['workers', 'threads', 'chat.queue']);
const pauseCodes = new Set(['denied', 'closed', 'authentication_required', 'unsupported_version']);
const readTimeout = () => Object.assign(new Error('读取状态的 55 秒窗口已结束，尚未完成同步。'), { code: 'read_window_timeout' });

export function abortableDelay(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new DOMException('观察已暂停。', 'AbortError')); return; }
    const timer = setTimeout(done, ms);
    function done() { signal?.removeEventListener('abort', abort); resolve(); }
    function abort() { clearTimeout(timer); signal?.removeEventListener('abort', abort); reject(new DOMException('观察已暂停。', 'AbortError')); }
    signal?.addEventListener('abort', abort, { once: true });
  });
}

/** Commands and observation have separate lifetimes. An observation abort never cancels a command. */
export class BridgeClient {
  constructor(transport, { makeId = requestId, now = () => Date.now(), sleep = abortableDelay,
    clock = () => performance.now() } = {}) {
    this.transport = transport; this.makeId = makeId; this.now = now; this.sleep = sleep;
    this.clock = clock; this.readChecks = new WeakMap();
    this.listeners = new Set(); this.connection = null; this.generation = null; this.capabilities = null;
    this.states = new Map(); this.queries = new Map(); this.stateErrors = new Map(); this.resourceReads = new Map();
    this.status = 'disconnected'; this.observation = 'paused'; this.error = null;
    this.cursor = null; this.startCursor = null; this.liveCursor = null; this.history = null; this.pendingObservation = null;
    this.timeline = new Timeline(); this.ledger = new CommandLedger();
    this.epoch = 0; this.observer = null; this.observerPromise = null; this.lastRefresh = 0;
    this.openAttempt = null; this.connectionRecords = []; this.reads = new AbortController(); this.refreshing = null;
  }
  subscribe(listener) { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  emit(type, data = {}) { for (const listener of this.listeners) listener({ type, ...data }); }
  hasResource(id) { return this.capabilities?.resources.some(item => item.id === id && item.schema_version === 1) ?? false; }
  hasCommand(id) { return this.connection?.read_only === false && this.capabilities?.mode === 'control' && this.capabilities.commands.some(item => item.id === id); }
  context() { return { connection: this.connection?.connection_id, generation: this.generation, epoch: this.epoch }; }
  current(context) { return context.epoch === this.epoch && context.connection === this.connection?.connection_id && context.generation === this.generation; }
  notice(text, metadata = {}) {
    this.timeline.put({ id: this.makeId('notice'), kind: 'notice', title: '观察记录', text, generation: this.generation, metadata });
    this.emit('timeline');
  }
  connectionRecord(record) {
    if (this.connectionRecords.length >= 32) {
      const index = this.connectionRecords.findIndex(item => ['confirmed', 'rejected'].includes(item.status));
      if (index < 0) throw new Error('未确定连接请求已达到内存上限，请先核对原始连接 ID。');
      this.connectionRecords.splice(index, 1);
    }
    this.connectionRecords.push(record);
    return record;
  }
  checkRead(signal) {
    if (signal?.aborted) throw new DOMException('读取已取消。', 'AbortError');
    if (signal) this.readChecks.get(signal)?.();
  }
  async readWindow(action, { signal = this.reads.signal, maximumMs = 55_000, deadline = this.clock() + Math.min(maximumMs, 55_000) } = {}) {
    const controller = new AbortController();
    const cancel = () => controller.abort();
    if (signal?.aborted) controller.abort();
    else signal?.addEventListener('abort', cancel, { once: true });
    let expired = false, timer;
    const check = () => {
      if (expired || this.clock() >= deadline) throw readTimeout();
      if (controller.signal.aborted) throw new DOMException('读取已取消。', 'AbortError');
    };
    try {
      check();
      timer = setTimeout(() => { expired = true; controller.abort(); }, Math.min(55_000, deadline - this.clock()));
      this.readChecks.set(controller.signal, check);
      const value = await action(controller.signal, check);
      check();
      return value;
    } catch (error) {
      if (expired || this.clock() >= deadline) throw readTimeout();
      throw error;
    } finally { clearTimeout(timer); this.readChecks.delete(controller.signal); signal?.removeEventListener('abort', cancel); }
  }

  async connect(session, readOnly = true) {
    if (this.connection || this.status === 'connecting') throw new Error('请先关闭当前前端连接。');
    const attempt = this.connectionRecord({ requestId: this.makeId('connect'), operation: 'open', session, readOnly, status: 'sending' });
    const epoch = ++this.epoch;
    this.timeline.clear(); this.history = null; this.pendingObservation = null;
    this.status = 'connecting'; this.error = null;
    this.openAttempt = attempt;
    this.emit('status');
    try {
      const result = await this.transport.open({ request_id: this.openAttempt.requestId, session, read_only: readOnly });
      if (epoch !== this.epoch) throw new Error('连接选择已变化。');
      this.connection = result;
      this.openAttempt.connectionId = result.connection_id;
      this.openAttempt.status = 'confirmed';
      this.reads = new AbortController();
      // Successful gateway opens correlate their real capabilities request with
      // the open ID. A fresh read below also detects a generation change on attach.
      if (result.capabilities) validateResponse(result.capabilities, createRequest({ op: 'capabilities' }, this.openAttempt.requestId));
      await this.readWindow(async signal => {
        await this.handshake(signal);
        await this.refreshStates({ includeHistory: true, strict: false, signal });
      });
      if (epoch !== this.epoch) return;
      this.status = 'connected'; this.emit('status');
    } catch (error) {
      if (epoch !== this.epoch) return;
      this.openAttempt.status = this.connection ? 'confirmed' : error.effect === 'may_have_occurred' ? 'unknown' : 'rejected';
      this.openAttempt.error = { code: error.code, message: error.message };
      this.status = this.connection ? 'paused' : 'disconnected'; this.error = error;
      this.emit('status'); throw error;
    }
  }

  async handshake(signal = this.reads.signal) {
    this.checkRead(signal);
    if (!this.connection) throw new Error('尚未连接会话。');
    const connectionId = this.connection.connection_id;
    const epoch = this.epoch;
    const request = createRequest({ op: 'capabilities' }, this.makeId('caps'));
    const response = await this.transport.bridge(connectionId, request, { signal });
    this.checkRead(signal);
    const caps = unwrapResponse(response, request);
    if (caps.application !== 'ssmcp') throw new ProtocolError('这个网关连接不是 SSMCP。');
    if (epoch !== this.epoch || connectionId !== this.connection?.connection_id) return;
    if (this.connection.read_only && caps.mode !== 'observe') throw new ProtocolError('只读连接返回了控制能力。');
    const changed = this.generation !== null && this.generation !== response.generation;
    this.generation = response.generation; this.capabilities = caps;
    if (changed) {
      this.cursor = null; this.history = null; this.pendingObservation = null; this.states.clear(); this.stateErrors.clear(); this.queries.clear(); this.timeline.clear();
      this.notice('后端代次已变化，已重新读取当前状态。旧请求的结果保留在操作记录中。');
    }
    this.emit('capabilities');
  }

  async readState(resource, query = {}, { signal = this.reads.signal, apply = true } = {}) {
    this.checkRead(signal);
    if (!this.hasResource(resource)) throw new Error(`当前连接没有 ${resource} 资源。`);
    const context = this.context();
    const serial = (this.resourceReads.get(resource) ?? 0) + 1;
    this.resourceReads.set(resource, serial);
    if (apply) this.queries.set(resource, query);
    const request = createRequest({ op: 'state', generation: context.generation, resource, query }, this.makeId('state'));
    const response = await this.transport.bridge(context.connection, request, { signal });
    this.checkRead(signal);
    const body = unwrapResponse(response, request);
    if (body.schema_version !== 1) throw new ProtocolError('状态 schema 版本不受支持。');
    if (!this.current(context) || this.resourceReads.get(resource) !== serial) throw new DOMException('状态读取已被较新请求替代。', 'AbortError');
    if (apply) {
      this.states.set(resource, body.value); this.stateErrors.delete(resource); this.queries.set(resource, query);
      if (resource === 'chat.queue' && Array.isArray(body.value?.items)) {
        for (const item of body.value.items.slice(0, 32)) {
          if (typeof item.id !== 'string' || typeof item.text !== 'string') continue;
          this.timeline.put({ id: `queue:${context.generation}:${item.id}`, generation: context.generation,
            kind: 'input', title: '输入队列', text: item.text.slice(0, 16384), status: item.uncertain ? 'unknown' : item.status,
            source: { kind: 'local_queue' }, metadata: item });
        }
        this.emit('timeline');
      }
      if (resource === 'workers' && Array.isArray(body.value?.slots)) {
        for (const slot of body.value.slots.slice(0, 32)) {
          if (typeof slot.task_id !== 'string' || !slot.task_id) continue;
          const progress = typeof slot.progress === 'string' ? slot.progress : slot.progress ? JSON.stringify(slot.progress) : '';
          this.timeline.put({ id: `worker-task:${context.generation}:${body.value.pool_generation}:${slot.task_id}`,
            generation: context.generation, kind: 'worker_snapshot', title: `Worker 任务快照 · 站点 ${slot.slot}`,
            text: [slot.task_summary || slot.task_id, progress].filter(Boolean).join('\n').slice(0, 8192),
            status: slot.state, source: { kind: 'worker_snapshot' },
            metadata: { snapshot: true, pool_generation: body.value.pool_generation, ...slot } });
        }
        this.emit('timeline');
      }
      this.emit('state', { resource });
    }
    return body.value;
  }

  async refreshStates({ includeHistory = false, strict = false, signal = this.reads.signal } = {}) {
    const context = this.context();
    const failures = [];
    // Capture the live boundary before other state reads so later events cannot
    // slip between the snapshot and the first observation request.
    if (this.hasResource('runtime')) {
      try {
        const runtime = await this.readState('runtime', {}, { signal });
        counter(runtime.live_cursor, '当前事件游标');
        if (this.cursor === null && this.current(context)) this.cursor = runtime.live_cursor;
      } catch (error) {
        if (!this.current(context)) return;
        failures.push(error); this.stateErrors.set('runtime', error.message);
      }
    }
    if (!this.current(context)) return;
    const resources = supported.filter(id => id !== 'runtime' && this.hasResource(id));
    const results = await Promise.allSettled(resources.map(resource => this.readState(resource,
      this.queries.get(resource) ?? (paged.has(resource) ? { offset: 0, limit: 16 } : {}), { signal })));
    if (!this.current(context)) return;
    results.forEach((result, index) => {
      if (result.status === 'rejected') { failures.push(result.reason); this.stateErrors.set(resources[index], result.reason.message); }
    });
    if (includeHistory && this.hasResource('conversation')) {
      try { await this.loadHistory({ reset: true, signal }); } catch (error) { failures.push(error); this.stateErrors.set('conversation', error.message); }
    }
    if (!this.current(context)) return;
    this.lastRefresh = this.now(); this.emit('state');
    if (strict && failures.length) throw failures[0];
    const fatal = failures.find(error => error?.code === 'stale_generation' || pauseCodes.has(error?.code));
    if (fatal) throw fatal;
    return failures;
  }

  async refresh() {
    if (this.refreshing) return this.refreshing;
    const wasObserving = Boolean(this.observerPromise);
    this.refreshing = (async () => {
      if (wasObserving) await this.pauseObservation();
      const failures = await this.readWindow(async signal => {
        await this.handshake(signal);
        return this.refreshStates({ includeHistory: this.history === null, signal });
      });
      if (wasObserving) void this.startObservation();
      return failures;
    })();
    try { return await this.refreshing; } finally { this.refreshing = null; }
  }

  async loadHistory({ reset = false, signal = this.reads.signal } = {}) {
    const context = this.context();
    const cursor = reset ? undefined : this.history?.next_cursor;
    const query = { limit: 32, ...(cursor === undefined ? {} : { cursor }) };
    const value = validateHistory(await this.readState('conversation', query, { signal, apply: false }), cursor);
    this.checkRead(signal);
    if (!this.current(context)) return;
    const rows = value.events.map(event => projectHistory(event, context.generation));
    for (const row of rows) this.timeline.put(row);
    this.history = value;
    this.stateErrors.delete('conversation');
    this.emit('timeline'); this.emit('state', { resource: 'conversation' });
    return value;
  }

  startObservation() {
    if (!this.connection || !this.capabilities) throw new Error('尚未连接会话。');
    if (!this.capabilities.events) { this.observation = 'unsupported'; this.emit('status'); return Promise.resolve(); }
    if (this.observerPromise) return this.observerPromise;
    const controller = new AbortController(); this.observer = controller;
    this.observerPromise = this.observeLoop(controller.signal).finally(() => {
      if (this.observer === controller) { this.observer = null; this.observerPromise = null; }
    });
    return this.observerPromise;
  }

  pauseObservation() {
    this.observer?.abort(); this.observation = 'paused'; this.emit('status');
    return this.observerPromise ?? Promise.resolve();
  }

  async observeLoop(signal) {
    let failures = 0; let drain = false;
    const epoch = this.epoch;
    while (!signal.aborted && epoch === this.epoch && this.connection) {
      const context = this.context();
      const started = this.now();
      const deadline = this.clock() + 55_000;
      const held = this.pendingObservation && this.current(this.pendingObservation.context) ? this.pendingObservation : null;
      const request = held?.request ?? { request_id: this.makeId('observe'), generation: context.generation,
        ...(this.cursor === null ? {} : { cursor: this.cursor }), limit: 16, wait_ms: drain ? 0 : 50_000 };
      this.observation = 'observing'; this.emit('status');
      try {
        const { page, rows } = await this.readWindow(async (cycleSignal, check) => {
          const page = held?.page ?? validateObservation(await this.transport.observe(context.connection, request, { signal: cycleSignal }), request);
          if (!this.current(context)) throw new DOMException('已切换连接。', 'AbortError');
          // Keep a verified page before checking cancellation or a deadline that
          // raced its arrival. A later cycle reconciles this same page.
          this.pendingObservation = { context, request, page };
          check();
          const reconcile = page.retention_gap || Boolean(held);
          if (reconcile || (!page.has_more && this.now() - this.lastRefresh >= 10_000)) {
            if (reconcile) { this.observation = 'resyncing'; this.emit('status'); }
            await this.refreshStates({ includeHistory: page.retention_gap, strict: reconcile, signal: cycleSignal });
            check();
          }
          const rows = page.events.map(event => projectEvent(event, context.generation));
          check();
          return { page, rows };
        }, { signal, deadline });
        if (!this.current(context) || signal.aborted) break;
        if (this.clock() >= deadline) throw readTimeout();
        if (page.retention_gap) {
          this.notice('事件保留窗口已移动。已刷新权威状态；缺失的事件不会被补造成成功结果。', {
            requested_cursor: request.cursor, start_cursor: page.start_cursor,
          });
        }
        for (const row of rows) this.timeline.put(row);
        this.cursor = page.next_cursor; this.startCursor = page.start_cursor; this.liveCursor = page.live_cursor;
        this.pendingObservation = null;
        this.error = null; this.status = 'connected'; failures = 0; drain = page.has_more;
        if (rows.length) this.emit('timeline', { added: rows.length });
        this.emit('events', { rows, page }); this.emit('status');
        // A buggy/immediate empty response must not create a CPU/network hot loop.
        await this.sleep(drain ? 20 : Math.max(20, 500 - (this.now() - started)), signal);
      } catch (error) {
        // A newer resource-page read can supersede one reconciliation read.
        // Its AbortError does not mean the user paused this observer.
        if (signal.aborted || epoch !== this.epoch) break;
        this.error = error; this.emit('error', { error });
        if (pauseCodes.has(error.code) || error.status === 401 || error.status === 403
            || (error instanceof ProtocolError && error.code !== 'stale_generation')) {
          this.observation = 'paused'; this.status = 'paused'; this.emit('status'); return;
        }
        this.observation = 'backoff'; this.emit('status');
        failures += 1;
        try {
          await this.sleep(Math.min(30_000, 1000 * 2 ** Math.min(failures - 1, 5)), signal);
          if (error.code === 'stale_generation' || error.code === 'invalid_cursor') {
            await this.readWindow(async readSignal => {
              await this.handshake(readSignal);
              if (error.code === 'invalid_cursor') { this.cursor = null; this.pendingObservation = null; }
              await this.refreshStates({ includeHistory: true, strict: true, signal: readSignal });
            }, { signal });
          }
        } catch (refreshError) {
          if (signal.aborted || epoch !== this.epoch) break;
          this.error = refreshError;
          if (pauseCodes.has(refreshError.code)) { this.status = 'paused'; this.observation = 'paused'; this.emit('status'); return; }
        }
      }
    }
    this.observation = 'paused'; this.emit('status');
  }

  async command(command, arguments_) {
    if (!this.hasCommand(command)) throw new Error('当前连接没有这个控制能力。');
    const context = this.context(); const id = this.makeId('command');
    const args = command === 'chat.send' ? { target: 'next', ...arguments_, request_id: id } : arguments_;
    const request = createRequest({ op: 'command', generation: context.generation, command, arguments: args }, id);
    const record = this.ledger.add({ requestId: id, session: this.connection.session, generation: context.generation, command, createdAt: this.now() });
    this.emit('command', { record });
    try {
      // Intentionally no observation/read AbortSignal here.
      const response = await this.transport.bridge(context.connection, request);
      const value = unwrapResponse(response, request);
      if (command === 'chat.send' && (value.value?.request_id !== id || value.value?.target !== args.target
          || value.value?.confirmation !== 'local_queue_admission' || typeof value.value?.message_id !== 'string'
          || typeof value.value?.duplicate !== 'boolean' || typeof value.value?.status !== 'string')) throw new ProtocolError('输入队列回执与原提交不匹配。', 'response_mismatch');
      if (command === 'chat.send') identifier(value.value.message_id, '输入消息 ID');
      const nativeUnknown = ['unknown', 'uncertain'].includes(value.value?.status)
        || value.value?.uncertain === true || value.value?.effect === 'may_have_occurred';
      this.ledger.update(id, { status: nativeUnknown ? 'unknown' : 'confirmed', receipt: value.value,
        ...(nativeUnknown ? { error: { code: 'unknown', message: '原生回执仍报告结果未确定。', effect: 'may_have_occurred' } } : {}) });
    } catch (error) {
      const rejected = (error instanceof BridgeError || typeof error.effect === 'string') && error.effect === 'none';
      this.ledger.update(id, { status: rejected ? 'rejected' : 'unknown', error: { code: error.code ?? 'unknown', message: error.message, effect: rejected ? 'none' : 'may_have_occurred' } });
    }
    if (this.current(context)) {
      const text = record.status === 'confirmed' ? command === 'chat.send' ? '已收到本地输入队列的入队回执。' : '已收到原生操作回执。'
        : record.status === 'unknown' ? '结果未确定。保留此请求 ID，并核对实际状态；不会自动重发。' : record.error.message;
      this.timeline.put({ id: `command:${id}`, generation: context.generation, kind: 'receipt', title: command,
        text, status: record.status, metadata: record });
      this.emit('timeline');
      // Read-only reconciliation cannot turn unknown into confirmed.
      void this.readWindow(signal => this.refreshStates({ signal })).catch(error => { if (this.current(context)) { this.error = error; this.emit('error', { error }); } });
    }
    this.emit('command', { record });
    return record;
  }

  async disconnect() {
    const connection = this.connection;
    await this.pauseObservation(); this.reads.abort(); ++this.epoch;
    this.status = 'disconnecting'; this.emit('status');
    let failure = null;
    if (connection) {
      const record = this.connectionRecord({ requestId: this.makeId('close'), operation: 'close',
        correlation: 'local_close_record', connectionId: connection.connection_id, session: connection.session, status: 'sending' });
      try { await this.transport.close(connection.connection_id); record.status = 'confirmed'; }
      catch (error) { failure = error; record.status = error.effect === 'none' ? 'rejected' : 'unknown'; record.error = { code: error.code, message: error.message }; }
    }
    this.connection = null; this.generation = null; this.capabilities = null; this.states.clear(); this.queries.clear();
    this.cursor = null; this.startCursor = null; this.liveCursor = null; this.history = null; this.pendingObservation = null; this.stateErrors.clear();
    this.status = 'disconnected'; this.error = failure; this.emit('status'); this.emit('state');
    if (failure) throw failure;
  }
}
