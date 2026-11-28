import { t } from './locale.mjs';
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
  if (typeof text !== 'string') throw new ProtocolError(t('invalidJson'));
  return encoder.encode(text).byteLength;
}

export function object(value, label = t('object')) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new ProtocolError(t('invalidObject', label));
  }
  return value;
}

function fields(value, allowed, required = allowed, label = t('response')) {
  object(value, label);
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new ProtocolError(t('undeclaredField', label, key));
  }
  for (const key of required) {
    if (!Object.hasOwn(value, key)) throw new ProtocolError(t('missingField', label, key));
  }
}

export function identifier(value, label = t('identifier')) {
  if (typeof value !== 'string' || !/^[A-Za-z0-9_.:-]{1,128}$/.test(value)) {
    throw new ProtocolError(t('invalidIdentifier', label));
  }
  return value;
}

function integer(value, min, max, label) {
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new ProtocolError(t('outOfRange', label));
  }
  return value;
}

function boolean(value, label) {
  if (typeof value !== 'boolean') throw new ProtocolError(t('invalidBoolean', label));
}

function text(value, maximum, label) {
  if (typeof value !== 'string' || byteLength(value) > maximum) {
    throw new ProtocolError(t('textTooLong', label));
  }
}

export function counter(value, label = t('cursor')) {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,19})$/.test(value)
      || BigInt(value) > MAX_COUNTER) {
    throw new ProtocolError(t('invalidCounter', label));
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
  if (!random) throw new ProtocolError(t('missingSecureRandom'));
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
  if (request.version !== VERSION) throw new ProtocolError(t('unsupportedBridgeRequest'));
  identifier(request.id, t('requestId'));
  const op = object(request.operation, t('operation'));
  switch (op.op) {
    case 'capabilities':
      fields(op, ['op']);
      break;
    case 'state':
      fields(op, ['op', 'generation', 'resource', 'query'], ['op', 'generation', 'resource']);
      identifier(op.generation, t('backendGeneration'));
      identifier(op.resource, t('resource'));
      object(op.query === undefined ? {} : op.query, t('query'));
      if (byteLength(op.query === undefined ? {} : op.query) > MAX_ARGUMENT_BYTES) throw new ProtocolError(t('queryTooLarge'));
      break;
    case 'events':
      fields(op, ['op', 'generation', 'cursor', 'limit', 'wait_ms'], ['op', 'generation']);
      identifier(op.generation, t('backendGeneration'));
      if (op.cursor !== undefined && op.cursor !== null) counter(op.cursor);
      integer(op.limit === undefined ? 64 : op.limit, 1, MAX_EVENTS, t('eventPageSize'));
      integer(op.wait_ms === undefined ? 0 : op.wait_ms, 0, MAX_V1_WAIT_MS, t('v1WaitTime'));
      break;
    case 'command':
      fields(op, ['op', 'generation', 'command', 'arguments'], ['op', 'generation', 'command']);
      identifier(op.generation, t('backendGeneration'));
      identifier(op.command, t('command'));
      object(op.arguments === undefined ? {} : op.arguments, t('arguments'));
      if (byteLength(op.arguments === undefined ? {} : op.arguments) > MAX_ARGUMENT_BYTES) throw new ProtocolError(t('argumentsTooLarge'));
      break;
    default:
      throw new ProtocolError(t('unknownBridgeOperation'));
  }
  if (byteLength(request) > MAX_FRAME_BYTES) throw new ProtocolError(t('requestTooLarge'));
  return request;
}

export function validateCapabilities(value) {
  fields(value, ['application', 'mode', 'resources', 'commands', 'events', 'max_frame_bytes', 'max_event_page_bytes']);
  identifier(value.application, t('applicationId'));
  if (!['observe', 'control'].includes(value.mode)) throw new ProtocolError(t('invalidCapabilityMode'));
  if (!Array.isArray(value.resources) || !Array.isArray(value.commands)
      || value.resources.length + value.commands.length > 128
      || (value.mode === 'observe' && value.commands.length)) {
    throw new ProtocolError(t('invalidCapabilityScope'));
  }
  boolean(value.events, t('eventCapability'));
  integer(value.max_frame_bytes, 1, MAX_FRAME_BYTES, t('frameSize'));
  integer(value.max_event_page_bytes, 1, MAX_PAGE_BYTES, t('eventPageBytes'));
  for (const [items, command] of [[value.resources, false], [value.commands, true]]) {
    const seen = new Set();
    for (const item of items) {
      fields(item, command ? ['id', 'description', 'input_schema'] : ['id', 'schema_version', 'description']);
      identifier(item.id);
      if (seen.has(item.id)) throw new ProtocolError(t('duplicateCapabilityIds'));
      seen.add(item.id);
      text(item.description, 1024, t('capabilityDescription'));
      if (command) {
        object(item.input_schema, t('commandSchema'));
        if (byteLength(item.input_schema) > MAX_ARGUMENT_BYTES) throw new ProtocolError(t('commandSchemaTooLarge'));
      } else integer(item.schema_version, 1, 65535, t('resourceSchemaVersion'));
    }
  }
  return value;
}

export function validatePage(page, { cursor = null, limit = MAX_EVENTS } = {}) {
  fields(page, ['events', 'start_cursor', 'next_cursor', 'live_cursor', 'retention_gap', 'has_more']);
  if (!Array.isArray(page.events) || page.events.length > limit || page.events.length > MAX_EVENTS) {
    throw new ProtocolError(t('tooManyEvents'));
  }
  for (const key of ['start_cursor', 'next_cursor', 'live_cursor']) counter(page[key], key);
  boolean(page.retention_gap, 'retention_gap');
  boolean(page.has_more, 'has_more');
  if (compareCounters(page.start_cursor, page.next_cursor) > 0
      || compareCounters(page.next_cursor, page.live_cursor) > 0
      || page.has_more !== (compareCounters(page.next_cursor, page.live_cursor) < 0)) {
    throw new ProtocolError(t('invalidEventPageCursor'));
  }
  let previous = null;
  for (const event of page.events) {
    fields(event, ['sequence', 'topic', 'value']);
    counter(event.sequence, t('eventSequence'));
    identifier(event.topic, t('eventTopic'));
    if (compareCounters(event.sequence, page.start_cursor) < 0
        || compareCounters(event.sequence, page.next_cursor) >= 0
        || (previous !== null && compareCounters(event.sequence, previous) <= 0)) {
      throw new ProtocolError(t('invalidEventOrder'));
    }
    previous = event.sequence;
  }
  if (cursor !== null && cursor !== undefined) {
    counter(cursor);
    if (compareCounters(page.next_cursor, cursor) < 0
        || page.retention_gap !== (compareCounters(cursor, page.start_cursor) < 0)
        || page.events.some(event => compareCounters(event.sequence, cursor) < 0)) {
      throw new ProtocolError(t('eventPageCursorMismatch'));
    }
  }
  if (byteLength(page) > MAX_PAGE_BYTES) throw new ProtocolError(t('eventPageTooLarge'));
  return page;
}

export function validateResponse(response, request) {
  validateRequest(request);
  fields(response, ['version', 'id', 'generation', 'body'], ['version', 'id', 'body']);
  if (response.version !== VERSION) throw new ProtocolError(t('unsupportedBridgeResponse'));
  identifier(response.id, t('responseId'));
  if (response.id !== request.id) throw new ProtocolError(t('responseIdMismatch'), 'response_mismatch');
  if (byteLength(response) > MAX_FRAME_BYTES) throw new ProtocolError(t('frameTooLarge'));
  if (response.generation !== null && response.generation !== undefined) identifier(response.generation, t('responseGeneration'));
  fields(response.body, ['kind', 'value']);
  const { kind, value } = response.body;
  if (kind === 'error') {
    fields(value, ['code', 'message', 'effect']);
    if (!errorCodes.has(value.code) || !['none', 'may_have_occurred'].includes(value.effect)) {
      throw new ProtocolError(t('invalidErrorReceipt'));
    }
    text(value.message, 1024, t('errorMessage'));
    if (request.operation.op !== 'command' && value.effect === 'may_have_occurred') {
      throw new ProtocolError(t('invalidReadOnlyEffect'));
    }
    return response;
  }
  identifier(response.generation, t('responseGeneration'));
  if (request.operation.generation !== undefined && response.generation !== request.operation.generation) {
    throw new ProtocolError(t('staleResponseGeneration'), 'stale_generation');
  }
  if (kind !== request.operation.op) throw new ProtocolError(t('responseTypeMismatch'), 'response_mismatch');
  switch (kind) {
    case 'capabilities':
      validateCapabilities(value);
      break;
    case 'state':
      fields(value, ['resource', 'schema_version', 'revision', 'value'], ['resource', 'schema_version', 'value']);
      identifier(value.resource);
      if (value.resource !== request.operation.resource) throw new ProtocolError(t('responseResourceMismatch'), 'response_mismatch');
      integer(value.schema_version, 1, 65535, t('stateSchemaVersion'));
      if (value.revision !== null && value.revision !== undefined) counter(value.revision, t('stateRevision'));
      break;
    case 'events':
      validatePage(value, request.operation);
      break;
    case 'command':
      fields(value, ['command', 'value']);
      identifier(value.command);
      if (value.command !== request.operation.command) throw new ProtocolError(t('receiptCommandMismatch'), 'response_mismatch');
      break;
    default:
      throw new ProtocolError(t('unknownResponseType'));
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
    throw new ProtocolError(t('observationResponseMismatch'), 'response_mismatch');
  }
  if (response.generation !== request.generation) {
    throw new ProtocolError(t('staleObservationResponse'), 'stale_generation');
  }
  if (byteLength(response) > MAX_FRAME_BYTES) throw new ProtocolError(t('observationResponseTooLarge'));
  return validatePage(response.page, request);
}
