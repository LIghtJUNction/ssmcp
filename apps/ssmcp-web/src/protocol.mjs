// zbridge v1: see docs/gui-bridge.md and src/session/attach/zbridge_adapter.rs.
export const VERSION = 1;
export const MAX_FRAME_BYTES = 256 * 1024;
export const MAX_PAGE_BYTES = 128 * 1024;
export const MAX_ARGUMENT_BYTES = 32 * 1024;
export const MAX_EVENTS = 128;
export const MAX_V1_WAIT_MS = 1000;
export const MAX_OBSERVATION_MS = 55_000;
const MAX_COUNTER = 18_446_744_073_709_551_615n;
const encoder = new TextEncoder();
const errorCodes = new Set([
  'invalid_request', 'unsupported_version', 'denied', 'closed', 'unavailable',
  'stale_generation', 'invalid_cursor', 'too_large', 'conflict', 'internal',
]);

export class ProtocolError extends Error {
  constructor(message, code = 'invalid_response') {
    super(message);
    this.name = 'ProtocolError';
    this.code = code;
  }
}

export class BridgeError extends Error {
  constructor(error, response) {
    super(error.message);
    this.name = 'BridgeError';
    this.code = error.code;
    this.effect = error.effect;
    this.responseGeneration = response.generation ?? null;
    this.requestId = response.id;
  }
}

export function byteLength(value) {
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  if (typeof text !== 'string') throw new ProtocolError('数据不是有效 JSON。');
  return encoder.encode(text).byteLength;
}

export function object(value, label = '对象') {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new ProtocolError(`${label}必须是 JSON object。`);
  }
  return value;
}

function fields(value, allowed, required = allowed, label = '响应') {
  object(value, label);
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new ProtocolError(`${label}包含未声明字段：${key}`);
  }
  for (const key of required) {
    if (!Object.hasOwn(value, key)) throw new ProtocolError(`${label}缺少字段：${key}`);
  }
}

export function identifier(value, label = '标识') {
  if (typeof value !== 'string' || !/^[A-Za-z0-9_.:-]{1,128}$/.test(value)) {
    throw new ProtocolError(`${label}不是有效的 zbridge 标识。`);
  }
  return value;
}

function integer(value, min, max, label) {
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new ProtocolError(`${label}超出允许范围。`);
  }
  return value;
}

function boolean(value, label) {
  if (typeof value !== 'boolean') throw new ProtocolError(`${label}必须是布尔值。`);
}

function text(value, maximum, label) {
  if (typeof value !== 'string' || byteLength(value) > maximum) {
    throw new ProtocolError(`${label}超过文字限制。`);
  }
}

export function counter(value, label = '游标') {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,19})$/.test(value)
      || BigInt(value) > MAX_COUNTER) {
    throw new ProtocolError(`${label}必须是 u64 范围内的规范十进制字符串。`);
  }
  return value;
}

export function compareCounters(left, right) {
  const a = BigInt(counter(left));
  const b = BigInt(counter(right));
  return a < b ? -1 : a > b ? 1 : 0;
}

export function requestId(prefix = 'web') {
  const random = globalThis.crypto?.randomUUID?.();
  if (!random) throw new ProtocolError('浏览器缺少安全随机数支持，请使用 HTTPS 或本机地址。');
  return `${prefix}_${random}`;
}

export function createRequest(operation, id = requestId()) {
  const request = { version: VERSION, id, operation: JSON.parse(JSON.stringify(operation)) };
  validateRequest(request);
  Object.freeze(request.operation);
  return Object.freeze(request);
}

export function validateRequest(request) {
  fields(request, ['version', 'id', 'operation']);
  if (request.version !== VERSION) throw new ProtocolError('不支持的 zbridge 请求版本。');
  identifier(request.id, '请求 ID');
  const op = object(request.operation, '操作');
  switch (op.op) {
    case 'capabilities':
      fields(op, ['op']);
      break;
    case 'state':
      fields(op, ['op', 'generation', 'resource', 'query'], ['op', 'generation', 'resource']);
      identifier(op.generation, '后端代次');
      identifier(op.resource, '资源');
      object(op.query === undefined ? {} : op.query, '查询');
      if (byteLength(op.query === undefined ? {} : op.query) > MAX_ARGUMENT_BYTES) throw new ProtocolError('查询过大。');
      break;
    case 'events':
      fields(op, ['op', 'generation', 'cursor', 'limit', 'wait_ms'], ['op', 'generation']);
      identifier(op.generation, '后端代次');
      if (op.cursor !== undefined && op.cursor !== null) counter(op.cursor);
      integer(op.limit === undefined ? 64 : op.limit, 1, MAX_EVENTS, '事件页大小');
      integer(op.wait_ms === undefined ? 0 : op.wait_ms, 0, MAX_V1_WAIT_MS, 'v1 等待时间');
      break;
    case 'command':
      fields(op, ['op', 'generation', 'command', 'arguments'], ['op', 'generation', 'command']);
      identifier(op.generation, '后端代次');
      identifier(op.command, '命令');
      object(op.arguments === undefined ? {} : op.arguments, '参数');
      if (byteLength(op.arguments === undefined ? {} : op.arguments) > MAX_ARGUMENT_BYTES) throw new ProtocolError('命令参数过大。');
      break;
    default:
      throw new ProtocolError('未声明的 zbridge 操作。');
  }
  if (byteLength(request) > MAX_FRAME_BYTES) throw new ProtocolError('请求帧过大。');
  return request;
}

export function validateCapabilities(value) {
  fields(value, ['application', 'mode', 'resources', 'commands', 'events', 'max_frame_bytes', 'max_event_page_bytes']);
  identifier(value.application, '应用标识');
  if (!['observe', 'control'].includes(value.mode)) throw new ProtocolError('能力中的连接模式无效。');
  if (!Array.isArray(value.resources) || !Array.isArray(value.commands)
      || value.resources.length + value.commands.length > 128
      || (value.mode === 'observe' && value.commands.length)) {
    throw new ProtocolError('能力目录或观察模式命令范围无效。');
  }
  boolean(value.events, '事件能力');
  integer(value.max_frame_bytes, 1, MAX_FRAME_BYTES, '帧大小');
  integer(value.max_event_page_bytes, 1, MAX_PAGE_BYTES, '事件页字节数');
  for (const [items, command] of [[value.resources, false], [value.commands, true]]) {
    const seen = new Set();
    for (const item of items) {
      fields(item, command ? ['id', 'description', 'input_schema'] : ['id', 'schema_version', 'description']);
      identifier(item.id);
      if (seen.has(item.id)) throw new ProtocolError('能力目录包含重复标识。');
      seen.add(item.id);
      text(item.description, 1024, '能力描述');
      if (command) {
        object(item.input_schema, '命令 schema');
        if (byteLength(item.input_schema) > MAX_ARGUMENT_BYTES) throw new ProtocolError('命令 schema 过大。');
      } else integer(item.schema_version, 1, 65535, '资源 schema 版本');
    }
  }
  return value;
}

export function validatePage(page, { cursor = null, limit = MAX_EVENTS } = {}) {
  fields(page, ['events', 'start_cursor', 'next_cursor', 'live_cursor', 'retention_gap', 'has_more']);
  if (!Array.isArray(page.events) || page.events.length > limit || page.events.length > MAX_EVENTS) {
    throw new ProtocolError('事件页超出条数限制。');
  }
  for (const key of ['start_cursor', 'next_cursor', 'live_cursor']) counter(page[key], key);
  boolean(page.retention_gap, 'retention_gap');
  boolean(page.has_more, 'has_more');
  if (compareCounters(page.start_cursor, page.next_cursor) > 0
      || compareCounters(page.next_cursor, page.live_cursor) > 0
      || page.has_more !== (compareCounters(page.next_cursor, page.live_cursor) < 0)) {
    throw new ProtocolError('事件页的游标关系无效。');
  }
  let previous = null;
  for (const event of page.events) {
    fields(event, ['sequence', 'topic', 'value']);
    counter(event.sequence, '事件序号');
    identifier(event.topic, '事件主题');
    if (compareCounters(event.sequence, page.start_cursor) < 0
        || compareCounters(event.sequence, page.next_cursor) >= 0
        || (previous !== null && compareCounters(event.sequence, previous) <= 0)) {
      throw new ProtocolError('事件序号不在页面范围内或顺序无效。');
    }
    previous = event.sequence;
  }
  if (cursor !== null && cursor !== undefined) {
    counter(cursor);
    if (compareCounters(page.next_cursor, cursor) < 0
        || page.retention_gap !== (compareCounters(cursor, page.start_cursor) < 0)
        || page.events.some(event => compareCounters(event.sequence, cursor) < 0)) {
      throw new ProtocolError('事件页不属于请求的游标范围。');
    }
  }
  if (byteLength(page) > MAX_PAGE_BYTES) throw new ProtocolError('事件页超过字节限制。');
  return page;
}

export function validateResponse(response, request) {
  validateRequest(request);
  fields(response, ['version', 'id', 'generation', 'body'], ['version', 'id', 'body']);
  if (response.version !== VERSION) throw new ProtocolError('不支持的 zbridge 响应版本。');
  identifier(response.id, '响应 ID');
  if (response.id !== request.id) throw new ProtocolError('响应 ID 与原请求不一致。', 'response_mismatch');
  if (byteLength(response) > MAX_FRAME_BYTES) throw new ProtocolError('响应帧过大。');
  if (response.generation !== null && response.generation !== undefined) identifier(response.generation, '响应代次');
  fields(response.body, ['kind', 'value']);
  const { kind, value } = response.body;
  if (kind === 'error') {
    fields(value, ['code', 'message', 'effect']);
    if (!errorCodes.has(value.code) || !['none', 'may_have_occurred'].includes(value.effect)) {
      throw new ProtocolError('错误回执字段无效。');
    }
    text(value.message, 1024, '错误说明');
    if (request.operation.op !== 'command' && value.effect === 'may_have_occurred') {
      throw new ProtocolError('只读响应声称产生了命令效果。');
    }
    return response;
  }
  identifier(response.generation, '响应代次');
  if (request.operation.generation !== undefined && response.generation !== request.operation.generation) {
    throw new ProtocolError('响应来自另一个后端代次。', 'stale_generation');
  }
  if (kind !== request.operation.op) throw new ProtocolError('响应类型与原操作不一致。', 'response_mismatch');
  switch (kind) {
    case 'capabilities':
      validateCapabilities(value);
      break;
    case 'state':
      fields(value, ['resource', 'schema_version', 'revision', 'value'], ['resource', 'schema_version', 'value']);
      identifier(value.resource);
      if (value.resource !== request.operation.resource) throw new ProtocolError('状态来自另一个资源。', 'response_mismatch');
      integer(value.schema_version, 1, 65535, '状态 schema 版本');
      if (value.revision !== null && value.revision !== undefined) counter(value.revision, '状态修订号');
      break;
    case 'events':
      validatePage(value, request.operation);
      break;
    case 'command':
      fields(value, ['command', 'value']);
      identifier(value.command);
      if (value.command !== request.operation.command) throw new ProtocolError('回执来自另一个命令。', 'response_mismatch');
      break;
    default:
      throw new ProtocolError('未声明的响应类型。');
  }
  return response;
}

export function unwrapResponse(response, request) {
  validateResponse(response, request);
  if (response.body.kind === 'error') throw new BridgeError(response.body.value, response);
  return response.body.value;
}

export function validateObservation(response, request) {
  fields(response, ['schema', 'request_id', 'generation', 'page']);
  if (response.schema !== 'ssmcp-gui-events-v1' || response.request_id !== request.request_id) {
    throw new ProtocolError('观察响应与原请求不匹配。', 'response_mismatch');
  }
  if (response.generation !== request.generation) {
    throw new ProtocolError('观察响应来自另一个后端代次。', 'stale_generation');
  }
  if (byteLength(response) > MAX_FRAME_BYTES) throw new ProtocolError('观察响应帧过大。');
  return validatePage(response.page, request);
}
