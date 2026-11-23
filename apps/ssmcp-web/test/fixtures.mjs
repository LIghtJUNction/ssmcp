// Fixtures mirror the zbridge v1 contract consumed by the real SSMCP adapter.
import { BridgeClient } from '../src/client.mjs';

export const GENERATION = 'attach.test.backend.2.connection.3';
export function capabilities({ resources = ['runtime'], commands = ['terminal.write'], mode = 'control' } = {}) {
  return {
    application: 'ssmcp', mode,
    resources: resources.map(id => ({ id, schema_version: 1, description: `Actual ${id} state` })),
    commands: commands.map(id => ({ id, description: `Native ${id}`, input_schema: { type: 'object' } })),
    events: true, max_frame_bytes: 261120, max_event_page_bytes: 131072,
  };
}
export function response(request, value, generation = GENERATION, kind = request.operation.op) {
  return { version: 1, id: request.id, generation, body: { kind, value } };
}
export function eventPage({ start = '0', next = '0', live = next, events = [], gap = false } = {}) {
  return { events, start_cursor: start, next_cursor: next, live_cursor: live, retention_gap: gap, has_more: BigInt(next) < BigInt(live) };
}
export function observation(request, page = eventPage()) {
  return { schema: 'ssmcp-gui-events-v1', request_id: request.request_id, generation: request.generation, page };
}
export function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
export function harness(options = {}) {
  const calls = []; let serial = 0;
  const caps = options.caps ?? capabilities();
  const transport = {
    async open(args) { calls.push(['open', args]); return { status: 'connected', connection_id: 'connection.test', session: args.session, read_only: args.read_only, duplicate: false }; },
    async bridge(connection, request, opts) {
      calls.push(['bridge', request, opts]);
      if (options.bridge) { const custom = await options.bridge(request, opts); if (custom !== undefined) return custom; }
      if (request.operation.op === 'capabilities') return response(request, caps);
      if (request.operation.op === 'state') {
        const value = request.operation.resource === 'runtime' ? { backend_active: true, live_cursor: '10', backend_generation: '2' } : {};
        return response(request, { resource: request.operation.resource, schema_version: 1, value });
      }
      if (request.operation.op === 'command') return response(request, { command: request.operation.command, value: { received: true } });
    },
    async observe(connection, request, opts) {
      calls.push(['observe', request, opts]);
      return options.observe ? options.observe(request, opts) : observation(request, eventPage({ next: request.cursor ?? '10', live: request.cursor ?? '10' }));
    },
    async close(id) { calls.push(['close', id]); return { closed: true, session_stopped: false }; },
  };
  const client = new BridgeClient(transport, { makeId: prefix => `${prefix}_${++serial}`, now: options.now ?? (() => 100), sleep: options.sleep ?? (async (_ms, signal) => { if (signal?.aborted) throw new DOMException('Stopped', 'AbortError'); }) });
  if (options.seed !== false) {
    client.connection = { connection_id: 'connection.test', session: 'actual-session', read_only: false };
    client.generation = GENERATION; client.capabilities = caps; client.cursor = '10'; client.status = 'connected';
  }
  return { client, transport, calls, caps };
}
