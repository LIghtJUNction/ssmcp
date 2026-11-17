'use strict';
// System Node.js worker. bubblewrap provides isolation; node:vm only shapes the API.
const vm = require('node:vm');
const readline = require('node:readline');
const { types } = require('node:util');
const { createHash } = require('node:crypto');
const encode = JSON.stringify;
const decode = JSON.parse;
const bytes = Buffer.byteLength;
const MAX_FRAME = 4 * 1024 * 1024;
const MAX_RESULT = 1024 * 1024;
const MAX_LOG = 64 * 1024;
const MAX_IMAGE_BYTES = 400 * 1024;
const MAX_IMAGES = 4;
const MAX_IMAGE_ENCODED_BYTES = 2 * 1024 * 1024;
const MAX_CALLS = 128;
const MAX_PENDING = 16;
const pending = new Map();
const unhandled = new Map();
let started = false;
let finished = false;
let nextId = 0;
let logBytes = 0;
let imageCount = 0;
let imageBytes = 0;
let rejectionOverflow = false;

function bounded(text, limit) {
  if (bytes(text, 'utf8') <= limit) return text;
  let end = Math.min(text.length, limit);
  while (end > 0 && bytes(text.slice(0, end), 'utf8') > limit - 16) end--;
  return text.slice(0, end) + ' [truncated]';
}

// Do not invoke user-defined getters, toString or inspect hooks while reporting errors.
function message(error) {
  if (typeof error === 'string') return bounded(error, 4096);
  if (types.isNativeError(error)) {
    const field = Object.getOwnPropertyDescriptor(error, 'message');
    if (field && typeof field.value === 'string') return bounded(field.value, 4096);
  }
  if (typeof error === 'number' || typeof error === 'boolean' || typeof error === 'bigint') {
    return bounded(String(error), 4096);
  }
  return 'JavaScript execution failed';
}

function send(frame) {
  if (finished) return false;
  const line = encode(frame) + '\n';
  if (bytes(line, 'utf8') > MAX_FRAME) throw new Error('code_mode protocol frame exceeds 4 MiB');
  process.stdout.write(line);
  return true;
}

function finish(frame, status) {
  if (finished) return;
  finished = true;
  input.close();
  try {
    process.stdout.write(encode(frame) + '\n', () => process.exit(status));
  } catch {
    process.exit(1);
  }
}

function fail(error) {
  let detail = message(error);
  if (pending.size) {
    detail += '; submitted native operations may continue; inspect their state before retrying';
  }
  finish({ type: 'error', message: bounded(detail, 4096) }, 1);
}

process.stdout.on('error', () => process.exit(1));
process.on('uncaughtException', fail);
process.on('unhandledRejection', (reason, promise) => {
  if (unhandled.size >= 128) rejectionOverflow = true;
  else unhandled.set(promise, reason);
});
process.on('rejectionHandled', promise => unhandled.delete(promise));

function bridge(kind, clientId, name, encoded) {
  const failure = error => Promise.resolve(encode({ ok: false, error }));
  if (finished) return failure('code_mode already finished');
  if (typeof clientId !== 'string' || clientId.length === 0 || bytes(clientId, 'utf8') > 128) {
    return failure('client ID must be a nonempty string of at most 128 UTF-8 bytes');
  }
  if (typeof encoded !== 'string' || bytes(encoded, 'utf8') > MAX_FRAME / 2) {
    return failure('native arguments exceed the bridge limit');
  }
  let args;
  try { args = decode(encoded); } catch { return failure('native arguments must be JSON'); }
  let request;
  if (kind === 'call') {
    if (typeof name !== 'string' || !/^[A-Za-z0-9_.-]{1,128}$/.test(name)
        || !args || typeof args !== 'object' || Array.isArray(args)) {
      return failure('invalid native tool name or arguments');
    }
    request = { op: 'call', client_id: clientId, name, arguments: args };
  } else if (kind === 'builtin') {
    if (typeof name !== 'string' || !/^[A-Za-z0-9.]{1,64}$/.test(name)
        || !args || typeof args !== 'object' || Array.isArray(args)) {
      return failure('invalid builtin name or arguments');
    }
    request = { op: 'builtin', name, arguments: args };
  } else if (kind === 'tools') {
    if (args !== null && (typeof args !== 'string' || bytes(args, 'utf8') > 128)) {
      return failure('tool cursor must be null or a string of at most 128 bytes');
    }
    request = { op: 'tools', client_id: clientId, cursor: args };
  } else {
    return failure('unknown code_mode host request');
  }
  if (nextId >= MAX_CALLS) return failure('code_mode exceeds 128 native calls');
  if (pending.size >= MAX_PENDING) return failure('code_mode exceeds 16 concurrent native calls');
  const id = ++nextId;
  return new Promise(resolve => {
    pending.set(id, resolve);
    try { send({ type: 'call', id, request }); }
    catch (error) {
      pending.delete(id);
      resolve(encode({ ok: false, error: message(error) }));
    }
  });
}

function log(line) {
  if (finished || typeof line !== 'string') return false;
  const length = bytes(line, 'utf8') + 1;
  if (logBytes + length > MAX_LOG) return false;
  logBytes += length;
  send({ type: 'log', line });
  return true;
}

// Images have a separate bounded channel so base64 never becomes a text log.
function emitImage(encoded) {
  const failure = error => encode({ ok: false, error });
  if (finished) return failure('code_mode already finished');
  if (typeof encoded !== 'string' || bytes(encoded, 'utf8') > MAX_IMAGE_BYTES * 2) {
    return failure('image content exceeds 400 KiB');
  }
  let item;
  try { item = decode(encoded); } catch { return failure('image content must be JSON'); }
  if (!item || item.type !== 'image' || !['image/png', 'image/jpeg'].includes(item.mimeType)
      || typeof item.data !== 'string' || !item.data
      || item.data.length > Math.ceil(MAX_IMAGE_BYTES / 3) * 4
      || item.data.length % 4 !== 0 || /[^A-Za-z0-9+/=]/.test(item.data)) {
    return failure('image requires PNG or JPEG content and canonical base64 data within 400 KiB');
  }
  const data = Buffer.from(item.data, 'base64');
  if (data.length > MAX_IMAGE_BYTES || data.toString('base64') !== item.data) {
    return failure('image requires canonical base64 data within 400 KiB');
  }
  const magic = item.mimeType === 'image/png'
    ? data.subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10]))
    : data.length >= 3 && data[0] === 255 && data[1] === 216 && data[2] === 255;
  if (!magic) return failure('image bytes do not match their PNG or JPEG MIME type');
  if (imageCount >= MAX_IMAGES || imageBytes + item.data.length > MAX_IMAGE_ENCODED_BYTES) {
    return failure('code_mode images exceed 4 images or 2 MiB of encoded data');
  }
  imageCount++;
  imageBytes += item.data.length;
  send({ type: 'image', content: { type: 'image', mimeType: item.mimeType, data: item.data } });
  return encode({ ok: true });
}

// The VM receives only this bounded string-to-digest bridge, never crypto objects.
function digestCatalog(encoded) {
  if (typeof encoded !== 'string' || bytes(encoded, 'utf8') > MAX_FRAME) {
    return '';
  }
  try { return createHash('sha256').update(encoded, 'utf8').digest('hex'); }
  catch { return ''; }
}

const bootstrap = `(() => {
  const native = globalThis.__ssmcp_host;
  const output = globalThis.__ssmcp_log;
  const visual = globalThis.__ssmcp_image;
  const digest = globalThis.__ssmcp_digest;
  const parse = JSON.parse;
  const stringify = JSON.stringify;
  const freeze = Object.freeze;
  const keys = Object.keys;
  const isArray = Array.isArray;
  const text = String;
  const Failure = Error;
  const deepFreeze = value => {
    if (value && typeof value === 'object') {
      for (const key of keys(value)) deepFreeze(value[key]);
      freeze(value);
    }
    return value;
  };
  const clients = deepFreeze(parse(globalThis.__ssmcp_clients));
  delete globalThis.__ssmcp_host;
  delete globalThis.__ssmcp_log;
  delete globalThis.__ssmcp_image;
  delete globalThis.__ssmcp_digest;
  delete globalThis.__ssmcp_clients;
  const dispatch = async (kind, id, name, args) => {
    if (typeof id !== 'string' || typeof name !== 'string') {
      throw new Failure('client ID and tool name must be strings');
    }
    const reply = parse(await native(kind, id, name, stringify(args)));
    if (!reply.ok) throw new Failure(reply.error);
    return reply.value;
  };
  const call = async (id, name, args = {}) => {
    if (!args || typeof args !== 'object' || isArray(args)) {
      throw new Failure('tool arguments must be a JSON object');
    }
    return dispatch('call', id, name, args);
  };
  const image = value => {
    if (value?.isError) {
      const detail = value.content?.find(item => item?.type === 'text')?.text;
      throw new Failure('image() cannot display a failed native tool result'
        + (typeof detail === 'string' ? ': ' + detail.slice(0, 4096) : ''));
    }
    const items = value?.type === 'image' ? [value]
      : isArray(value?.content) ? value.content.filter(item => item?.type === 'image') : [];
    if (!items.length) throw new Failure('image() expects an image content block or a native result containing images');
    for (const item of items) {
      const reply = parse(visual(stringify({type:item.type,mimeType:item.mimeType,data:item.data})));
      if (!reply.ok) throw new Failure(reply.error);
    }
  };
  const browser = freeze(Object.fromEntries(
    ['discover','status','launch','connect','tabs','open','navigate','snapshot','click','fill','key','scroll','evaluate','screenshot','close','disconnect']
      .map(name => [name, (id, args = {}) => call(id, 'ssmcp.browser.' + name, args)])
  ));
  const computer = freeze(Object.fromEntries(
    ['status','monitors','windows','focus','screenshot','move','click','drag','key','type','scroll']
      .map(name => [name, (id, args = {}) => call(id, 'ssmcp.computer.' + name, args)])
  ));
  // CUA reuses native desktop and browser calls, preserving complete MCP results.
  const cua = freeze({...computer, browser});
  const omarchy = freeze(Object.fromEntries(
    ["status","themes","theme_set","backgrounds","background_set","fonts","font_set","audio","audio_set","brightness","brightness_set","toggles","toggle_set","workspaces","workspace_switch","window_action","apps","app_launch","menu","battery","network"]
      .map(name => [name, (id, args = {}) => call(id, 'ssmcp.omarchy.' + name, args)])
  ));
  const tools = async (id, cursor = null) => {
    if (cursor !== null && typeof cursor !== 'string') {
      throw new Failure('tool cursor must be a string or null');
    }
    return dispatch('tools', id, '', cursor);
  };

  // Search is a bounded view over the existing authorized catalog pages. It
  // reduces returned text; it does not add a second registry or bypass host
  // authorization, and it does not reduce the first underlying page scan.
  const searchUtf8Bytes = value => {
    let count = 0;
    for (const ch of value) {
      const code = ch.codePointAt(0);
      count += code > 0xffff ? 4 : code > 0x7ff ? 3 : code > 0x7f ? 2 : 1;
    }
    return count;
  };
  const searchCursorValid = value => value === null
    || (typeof value === 'string' && value.length > 0 && searchUtf8Bytes(value) <= 128);
  const searchSnippet = description => {
    if (typeof description !== 'string') return {description:'', description_truncated:false};
    const flat = description.replace(/\\s+/gu, ' ').trim();
    let short = '', count = 0;
    for (const ch of flat) {
      if (count++ === 240) return {description:short + '…', description_truncated:true};
      short += ch;
    }
    return {description:short, description_truncated:false};
  };
  const searchTools = async (id, options = {}) => {
    if (typeof id !== 'string' || !id || searchUtf8Bytes(id) > 128) {
      throw new Failure('tools.search requires a client ID of at most 128 UTF-8 bytes');
    }
    if (!options || typeof options !== 'object' || isArray(options)
        || keys(options).some(key => !['query','limit','include_schemas','cursor','max_pages'].includes(key))) {
      throw new Failure('tools.search accepts query, limit, include_schemas, cursor and max_pages');
    }
    const query = options.query === undefined ? '' : options.query;
    const limit = options.limit === undefined ? 20 : options.limit;
    const includeSchemas = options.include_schemas === undefined ? false : options.include_schemas;
    const maxPages = options.max_pages === undefined ? 32 : options.max_pages;
    if (typeof query !== 'string' || searchUtf8Bytes(query) > 256) {
      throw new Failure('tools.search query must be a string of at most 256 UTF-8 bytes');
    }
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > 50) {
      throw new Failure('tools.search limit must be 1..=50');
    }
    if (typeof includeSchemas !== 'boolean') {
      throw new Failure('tools.search include_schemas must be a boolean');
    }
    if (!Number.isSafeInteger(maxPages) || maxPages < 1 || maxPages > 32) {
      throw new Failure('tools.search max_pages must be 1..=32');
    }
    const needle = query.trim().toLowerCase();
    let cursor = null, offset = 0, expectedPageHash = null, totalScanned = 0;
    let visited = [];
    if (options.cursor !== undefined && options.cursor !== null) {
      if (typeof options.cursor !== 'string' || searchUtf8Bytes(options.cursor) > 12 * 1024) {
        throw new Failure('tools.search cursor must be a returned string of at most 12 KiB');
      }
      let state;
      try { state = parse(options.cursor); }
      catch { throw new Failure('invalid tools.search cursor; restart the search'); }
      if (!state || typeof state !== 'object' || isArray(state)
          || keys(state).length !== 10
          || keys(state).some(key => !['v','client','query','limit','include_schemas','page','offset','page_hash','visited','scanned'].includes(key))
          || state.v !== 1 || state.client !== id || state.query !== needle
          || state.limit !== limit || state.include_schemas !== includeSchemas
          || !searchCursorValid(state.page)
          || !Number.isSafeInteger(state.offset) || state.offset < 0 || state.offset > 64
          || !(state.page_hash === null || (typeof state.page_hash === 'string' && /^[a-f0-9]{64}$/u.test(state.page_hash)))
          || (state.offset > 0 && state.page_hash === null)
          || !Number.isSafeInteger(state.scanned) || state.scanned < 0 || state.scanned > 8192
          || !isArray(state.visited) || state.visited.length > 127
          || state.visited.some(value => !searchCursorValid(value))
          || new Set(state.visited).size !== state.visited.length
          || state.visited.includes(state.page)) {
        throw new Failure('tools.search cursor does not match this client, query or options; restart the search');
      }
      cursor = state.page;
      offset = state.offset;
      expectedPageHash = state.page_hash;
      visited = state.visited;
      totalScanned = state.scanned;
    }
    const matches = [];
    let itemBytes = 2, pagesRead = 0, scanned = 0;
    const result = (complete, reason, pageHash = null) => {
      const continuation = complete ? null : stringify({
        v:1, client:id, query:needle, limit, include_schemas:includeSchemas,
        page:cursor, offset, page_hash:pageHash, visited, scanned:totalScanned,
      });
      if (continuation !== null && searchUtf8Bytes(continuation) > 12 * 1024) {
        throw new Failure('tools.search continuation exceeds 12 KiB; use the original tools() pagination');
      }
      const value = {
        tools:matches, nextCursor:continuation, complete, stopped_reason:reason,
        include_schemas:includeSchemas, namespace_source:'registered_tool_name',
        stats:{pages_read:pagesRead, tools_scanned:scanned, total_tools_scanned:totalScanned},
      };
      if (searchUtf8Bytes(stringify(value)) > 128 * 1024) {
        throw new Failure('tools.search output exceeds 128 KiB; reduce limit or use the original tools() pagination');
      }
      return value;
    };
    for (;;) {
      if (visited.includes(cursor)) throw new Failure('tools.search detected a repeated native cursor');
      // Every page, including a resumed page, re-enters the host tools path.
      // The host checks the current client connection and authorization again.
      const page = await tools(id, cursor);
      pagesRead++;
      if (!page || typeof page !== 'object' || isArray(page)
          || !isArray(page.tools) || page.tools.length > 64
          || !searchCursorValid(page.nextCursor ?? null)) {
        throw new Failure('tools.search received an invalid native catalog page');
      }
      const pageHash = digest(stringify(page));
      if (typeof pageHash !== 'string' || pageHash.length !== 64) {
        throw new Failure('tools.search native catalog digest exceeds its bounded input');
      }
      if (expectedPageHash !== null && pageHash !== expectedPageHash) {
        throw new Failure('tools.search catalog changed while resuming; restart the search');
      }
      if (offset > page.tools.length) {
        throw new Failure('tools.search cursor offset is outside the native page; restart the search');
      }
      const next = page.nextCursor ?? null;
      if (next !== null && (next === cursor || visited.includes(next))) {
        throw new Failure('tools.search detected a repeated native cursor');
      }
      while (offset < page.tools.length) {
        if (totalScanned >= 8192) {
          throw new Failure('tools.search exceeds 8192 scanned tools; use the original tools() pagination');
        }
        const descriptor = page.tools[offset];
        if (!descriptor || typeof descriptor !== 'object' || isArray(descriptor)
            || typeof descriptor.name !== 'string'
            || !/^[A-Za-z0-9_.-]{1,128}$/u.test(descriptor.name)) {
          throw new Failure('tools.search received an invalid native tool descriptor');
        }
        const description = typeof descriptor.description === 'string' ? descriptor.description : '';
        const match = !needle || descriptor.name.toLowerCase().includes(needle)
          || description.toLowerCase().includes(needle);
        if (match) {
          const parts = descriptor.name.split('.');
          // This is the observed routing namespace, not an invented provider ID.
          const namespace = parts[0] === 'ssmcp' ? parts.slice(0, 2).join('.') : parts[0];
          const item = includeSchemas ? descriptor
            : {name:descriptor.name, namespace, ...searchSnippet(description)};
          const size = searchUtf8Bytes(stringify(item)) + (matches.length ? 1 : 0);
          if (size + 2 > 96 * 1024) {
            throw new Failure('tools.search native descriptor exceeds its 96 KiB item budget; retrieve the unchanged descriptor with tools(clientId, nativeCursor) instead');
          }
          if (itemBytes + size > 96 * 1024) return result(false, 'output_budget', pageHash);
          matches.push(item);
          itemBytes += size;
        }
        offset++;
        scanned++;
        totalScanned++;
        if (matches.length >= limit && offset < page.tools.length) {
          return result(false, 'limit', pageHash);
        }
      }
      visited.push(cursor);
      cursor = next;
      offset = 0;
      expectedPageHash = null;
      if (cursor === null) return result(true, null);
      if (visited.length >= 128) {
        throw new Failure('tools.search exceeds 128 catalog pages; use the original tools() pagination');
      }
      if (matches.length >= limit) return result(false, 'limit');
      if (pagesRead >= maxPages) return result(false, 'page_budget');
    }
  };
  Object.defineProperty(tools, 'search', {value:searchTools, enumerable:true});
  freeze(tools);

  const builtin = (name, args = {}) => dispatch('builtin', '_', name, args);
  const collab = freeze(Object.fromEntries(
    ['join', 'list', 'update', 'send', 'receive', 'ack', 'leave'].map(action => [
      action, (clientId, args = {}) => {
        if (!args || typeof args !== 'object' || isArray(args)) {
          throw new Failure('collab arguments must be an object');
        }
        if (action === 'receive' && (args.wait_ms ?? 0) > 1000) {
          throw new Failure('Use the direct collab tool for wait_ms above 1000; it does not occupy a code_mode worker');
        }
        return builtin('collab.' + action, {...args, client_id: clientId});
      }
    ])
  ));
  const workspaceResult = async (id, name, args = {}) => {
    const reply = await call(id, name, args);
    if (reply.isError) {
      const detail = reply.content?.find(item => item.type === 'text')?.text;
      throw new Failure(typeof detail === 'string' ? detail.slice(0, 4096) : name + ' failed');
    }
    if (!reply.structuredContent) throw new Failure(name + ' returned no structured result');
    return reply.structuredContent;
  };
  const file = (name, id, args) => workspaceResult(id, 'ssmcp.fs.' + name, args);
  const reload = id => workspaceResult(id, 'ssmcp.client.reload');
  const ui = freeze(Object.fromEntries(
    ['get', 'set', 'action', 'filter'].map(name =>
      [name, (id, args = {}) => workspaceResult(id, 'ssmcp.ui.' + name, args)])
  ));
  const sessions = freeze(Object.fromEntries(
    ['list', 'status', 'create', 'restart', 'close'].map(name =>
      [name, (id, args = {}) => workspaceResult(id, 'ssmcp.session.' + name, args)])
  ));
  const outputs = freeze({
    read: (id, args) => workspaceResult(id, 'ssmcp.output.read', args),
    stat: (id, args) => workspaceResult(id, 'ssmcp.output.stat', args),
    cleanup: id => workspaceResult(id, 'ssmcp.output.cleanup'),
  });
  const fs = freeze({
    list: (id, args) => file('list', id, args),
    stat: (id, args) => file('stat', id, args),
    read: (id, args) => file('read', id, args),
    write: (id, args) => file('write', id, args),
    mkdir: (id, args) => file('mkdir', id, args),
    readText: (id, args) => file('read_text', id, args),
    writeText: (id, args) => file('write_text', id, args),
    editText: (id, args) => file('edit_text', id, args),
  });
  const context = freeze({
    roots: id => workspaceResult(id, 'ssmcp.context.roots'),
  });
  const skills = freeze({
    list: (id, args) => workspaceResult(id, 'ssmcp.skills.list', args),
    read: (id, args) => workspaceResult(id, 'ssmcp.skills.read', args),
    view: (id, args) => workspaceResult(id, 'ssmcp.skills.view', args),
  });
  const agents = freeze({
    list: (id, args) => workspaceResult(id, 'ssmcp.agents.list', args),
    read: (id, args) => workspaceResult(id, 'ssmcp.agents.read', args),
    resolve: (id, args) => workspaceResult(id, 'ssmcp.agents.resolve', args),
  });
  const execution = (name, id, args) => workspaceResult(id, 'ssmcp.exec.' + name, args);
  const executeCommand = async (id, args) => {
    if (!args || typeof args !== 'object' || isArray(args)) {
      throw new Failure('exec.run requires a command options object');
    }
    const {wait_ms = 5000, max_bytes = 16384, ...startArgs} = args;
    if (!Number.isSafeInteger(wait_ms) || wait_ms < 0 || wait_ms > 60000) {
      throw new Failure('exec.run wait_ms must be 0..=60000; it never cancels the command');
    }
    if (!Number.isSafeInteger(max_bytes) || max_bytes < 1 || max_bytes > 32768) {
      throw new Failure('exec.run max_bytes must be 1..=32768 per stream');
    }
    const started = await execution('start', id, startArgs);
    const job = started.job_id;
    if (typeof job !== 'string' || !job) {
      throw new Failure('Command start returned no job_id; inspect exec.list before retrying');
    }
    try {
      const status = wait_ms === 0 ? started : await execution('wait', id, {job_id:job, wait_ms});
      const output = await execution('read', id, {job_id:job, max_bytes});
      const snapshot = output.snapshot ?? status;
      return {...status, ...snapshot, ...output, snapshot};
    } catch (error) {
      const failure = new Failure('Command started as ' + job + '; ' + (error?.message ?? text(error))
        + '. Inspect exec.status/exec.read for this job before retrying.');
      failure.job_id = job;
      throw failure;
    }
  };
  const parallelCommands = async (id, commands, options = {}) => {
    if (!isArray(commands) || commands.length > 32) {
      throw new Failure('exec.parallel accepts an array of at most 32 command options objects');
    }
    if (!options || typeof options !== 'object' || isArray(options)
        || keys(options).some(key => key !== 'concurrency')) {
      throw new Failure('exec.parallel options only accept concurrency');
    }
    const concurrency = options.concurrency ?? 4;
    if (!Number.isSafeInteger(concurrency) || concurrency < 1 || concurrency > 8) {
      throw new Failure('exec.parallel concurrency must be 1..=8');
    }
    const results = new Array(commands.length);
    let next = 0;
    const worker = async () => {
      while (next < commands.length) {
        const index = next++;
        let job;
        try {
          const command = commands[index];
          if (!command || typeof command !== 'object' || isArray(command)) {
            throw new Failure('each parallel command must be an options object');
          }
          let value = await executeCommand(id, {max_bytes:2048, ...command});
          job = value.job_id;
          // A polling deadline never frees a running process's concurrency slot.
          while (value.running === true) {
            const status = await execution('wait', id, {job_id:job, wait_ms:60000});
            if (typeof status.running !== 'boolean') {
              throw new Failure('command state is unavailable; inspect exec.list before starting more work');
            }
            value = {...value, ...status, snapshot:status};
            if (!status.running) {
              const output = await execution('read', id, {
                job_id:job, max_bytes:command.max_bytes ?? 2048,
              });
              value = {...value, ...output.snapshot, ...output, snapshot:output.snapshot ?? status};
            }
          }
          if (value.running !== false) {
            throw new Failure('command completion is unknown; inspect exec.list before starting more work');
          }
          results[index] = {status:'fulfilled', value};
        } catch (error) {
          const job_id = job ?? error?.job_id;
          results[index] = {
            status:'rejected', reason:(error?.message ?? text(error)).slice(0, 4096),
            ...(job_id ? {job_id} : {}),
          };
          // Observation can fail after launch. Reserve this worker's slot for
          // that possibly running process instead of oversubscribing the host.
          return;
        }
      }
    };
    await Promise.all(Array.from({length:Math.min(concurrency, commands.length)}, worker));
    for (let index = 0; index < results.length; index++) {
      if (!results[index]) results[index] = {
        status:'rejected', not_started:true,
        reason:'Command was not started because a worker could not confirm completion; inspect returned job IDs before retrying.',
      };
    }
    return results;
  };
  const exec = freeze({
    start: (id, args) => execution('start', id, args),
    list: id => execution('list', id),
    status: (id, job) => execution('status', id, {job_id:job}),
    wait: (id, job, args = {}) => execution('wait', id, {...args, job_id:job}),
    read: (id, job, args = {}) => execution('read', id, {...args, job_id:job}),
    write: (id, job, args = {}) => execution('write', id, {...args, job_id:job}),
    cancel: (id, job) => execution('cancel', id, {job_id:job}),
    forget: (id, job) => execution('forget', id, {job_id:job}),
    events: (id, args) => execution('events', id, args),
    run: executeCommand,
    parallel: parallelCommands,
  });
  const terminal = freeze(Object.fromEntries(
    ['open','list','select','close','read','write','status','resize']
      .map(name => [name, (id, args = {}) => workspaceResult(id, 'terminal.' + name, args)])
  ));
  const rsync = (args) => builtin('sync', args);
  const jobs = freeze({
    list: () => builtin('jobs.list'),
    status: id => builtin('jobs.status', {id}),
    wait: (id, args = {}) => builtin('jobs.wait', {...args, id}),
    cancel: id => builtin('jobs.cancel', {id}),
  });
  const ports = freeze({
    open: args => builtin('ports.open', args),
    list: () => builtin('ports.list'),
    close: id => builtin('ports.close', {id}),
  });
  const resources = freeze({
    list: (cursor = null) => builtin('resources.list', {cursor}),
    read: uri => builtin('resources.read', {uri}),
    templates: () => builtin('resources.templates'),
  });
  const print = (...args) => {
    const line = args.map(value => typeof value === 'string' ? value : stringify(value) ?? text(value)).join(' ');
    if (!output(line)) throw new Failure('code_mode log output exceeds 65536 bytes');
  };
  Object.defineProperties(globalThis, {
    clients: { value: clients, enumerable: true },
    call: { value: call, enumerable: true },
    tools: { value: tools, enumerable: true },
    image: { value: image, enumerable: true },
    browser: { value: browser, enumerable: true },
    computer: { value: computer, enumerable: true },
    cua: { value: cua, enumerable: true },
    omarchy: { value: omarchy, enumerable: true },
    reload: { value: reload, enumerable: true },
    ui: { value: ui, enumerable: true },
    sessions: { value: sessions, enumerable: true },
    collab: { value: collab, enumerable: true },
    outputs: { value: outputs, enumerable: true },
    fs: { value: fs, enumerable: true },
    context: { value: context, enumerable: true },
    skills: { value: skills, enumerable: true },
    agents: { value: agents, enumerable: true },
    exec: { value: exec, enumerable: true },
    terminal: { value: terminal, enumerable: true },
    rsync: { value: rsync, enumerable: true },
    sync: { value: rsync, enumerable: true },
    jobs: { value: jobs, enumerable: true },
    ports: { value: ports, enumerable: true },
    resources: { value: resources, enumerable: true },
    print: { value: print, enumerable: true },
    console: { value: freeze({ log: print, warn: print, error: print }), enumerable: true },
  });
})();`;

async function run(frame) {
  if (typeof frame.code !== 'string' || !frame.code.trim() || bytes(frame.code, 'utf8') > 65536
      || !Array.isArray(frame.clients)) throw new Error('invalid code_mode input');
  const clients = encode(frame.clients);
  if (bytes(clients, 'utf8') > MAX_RESULT) throw new Error('client metadata exceeds 1 MiB');
  const context = vm.createContext(Object.assign(Object.create(null), {
    __ssmcp_host: bridge,
    __ssmcp_log: log,
    __ssmcp_image: emitImage,
    __ssmcp_digest: digestCatalog,
    __ssmcp_clients: clients,
  }), { name: 'ssmcp-code-mode', codeGeneration: { strings: false, wasm: false } });
  new vm.Script(bootstrap, { filename: 'ssmcp-api.js' }).runInContext(context, { timeout: 2000 });
  const program = new vm.Script('(async () => {\n"use strict";\n' + frame.code + '\n})()', { filename: 'code_mode.js' });
  const value = await program.runInContext(context, { timeout: 2000 });
  // Give rejected promises a full event-loop turn to reach Node's rejection tracker.
  await new Promise(resolve => setImmediate(resolve));
  if (finished) return;
  if (pending.size) throw new Error('code_mode returned with pending native calls; await every call');
  if (rejectionOverflow || unhandled.size) {
    const reason = unhandled.size ? message(unhandled.values().next().value) : 'too many rejected promises';
    throw new Error('Unhandled promise rejection: ' + reason);
  }
  const result = encode(value === undefined ? null : value);
  if (result === undefined) throw new Error('code_mode return value must be JSON-serializable');
  if (bytes(result, 'utf8') > MAX_RESULT) throw new Error('code_mode result exceeds 1 MiB');
  // Serialization can run a user-defined toJSON method, which must not leave work behind.
  await new Promise(resolve => setImmediate(resolve));
  if (finished) return;
  if (pending.size || rejectionOverflow || unhandled.size) {
    throw new Error('code_mode result serialization left pending calls or rejected promises');
  }
  finish({ type: 'result', value: decode(result) }, 0);
}

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on('line', line => {
  if (finished) return;
  try {
    if (bytes(line, 'utf8') > MAX_FRAME) throw new Error('code_mode input frame exceeds 4 MiB');
    const frame = decode(line);
    if (!frame || typeof frame !== 'object' || Array.isArray(frame)) throw new Error('invalid code_mode protocol frame');
    if (!started) {
      if (frame.type !== 'run') throw new Error('expected code_mode run frame');
      started = true;
      run(frame).catch(fail);
      return;
    }
    if (frame.type !== 'reply' || !Number.isSafeInteger(frame.id) || frame.id < 1
        || typeof frame.ok !== 'boolean' || !pending.has(frame.id)) {
      throw new Error('invalid or unsolicited code_mode reply');
    }
    if (frame.ok ? !Object.hasOwn(frame, 'value') : typeof frame.error !== 'string') {
      throw new Error('invalid code_mode reply result');
    }
    const resolve = pending.get(frame.id);
    pending.delete(frame.id);
    resolve(encode(frame.ok ? { ok: true, value: frame.value } : { ok: false, error: frame.error }));
  } catch (error) {
    fail(error);
  }
});
input.on('close', () => {
  if (!finished) fail(new Error('parent closed code_mode input before completion'));
});
input.on('error', fail);
