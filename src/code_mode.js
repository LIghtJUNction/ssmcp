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
let loadingSources = true;
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
  if (loadingSources) return failure('tool factories must define helpers without starting native operations');
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
  } else if (kind === 'wait_user') {
    if (!args || typeof args !== 'object' || Array.isArray(args)) {
      return failure('user wait arguments must be a JSON object');
    }
    request = { op: 'wait_user', client_id: clientId, arguments: args };
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
  if (!item || item.type !== 'image' || !['image/png', 'image/jpeg', 'image/gif', 'image/webp'].includes(item.mimeType)
      || typeof item.data !== 'string' || !item.data
      || item.data.length > Math.ceil(MAX_IMAGE_BYTES / 3) * 4
      || item.data.length % 4 !== 0 || /[^A-Za-z0-9+/=]/.test(item.data)) {
    return failure('image requires PNG, JPEG, GIF or WebP content and canonical base64 data within 400 KiB');
  }
  const data = Buffer.from(item.data, 'base64');
  if (data.length > MAX_IMAGE_BYTES || data.toString('base64') !== item.data) {
    return failure('image requires canonical base64 data within 400 KiB');
  }
  // Match output::image_mime_type: check raster headers, not full codec validity.
  const magic = item.mimeType === 'image/png'
    ? data.subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10]))
    : item.mimeType === 'image/jpeg'
      ? data.length >= 3 && data[0] === 255 && data[1] === 216 && data[2] === 255
      : item.mimeType === 'image/gif'
        ? data.subarray(0, 6).equals(Buffer.from('GIF87a'))
          || data.subarray(0, 6).equals(Buffer.from('GIF89a'))
        : data.subarray(0, 4).equals(Buffer.from('RIFF'))
          && data.subarray(8, 12).equals(Buffer.from('WEBP'));
  if (!magic) return failure('image bytes do not match their declared raster MIME type');
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
  const print = (...args) => {
    const line = args.map(value => typeof value === 'string' ? value : stringify(value) ?? text(value)).join(' ');
    if (!output(line)) throw new Failure('code_mode log output exceeds 65536 bytes');
  };
  const resolved = Object.create(null);
  const protectedNames = new Set(['clients','call','image','print','console']);
  Object.defineProperties(globalThis, {
    clients: {value:clients, enumerable:true},
    call: {value:call, enumerable:true},
    image: {value:image, enumerable:true},
    print: {value:print, enumerable:true},
    console: {value:freeze({log:print,warn:print,error:print}), enumerable:true},
    __ssmcp_install_source: {
      configurable:true,
      value:factory => {
        if (typeof factory !== 'function') throw new Failure('tool entry must evaluate to a factory');
        const previous = freeze(Object.assign(Object.create(null), resolved));
        const exports = factory(freeze({call,dispatch,digest,clients,previous}));
        if (!exports || typeof exports !== 'object' || isArray(exports)
            || typeof exports.then === 'function') {
          throw new Failure('tool factory must return a synchronous export object');
        }
        const descriptors = Object.getOwnPropertyDescriptors(exports);
        if (keys(descriptors).length > 128) throw new Failure('tool factory exceeds 128 exports');
        for (const name of keys(descriptors)) {
          const field = descriptors[name];
          if (!/^[A-Za-z][A-Za-z0-9_]{0,63}$/.test(name)
              || ['constructor','prototype'].includes(name) || name.startsWith('__')
              || protectedNames.has(name) || !Object.hasOwn(field,'value')) {
            throw new Failure('invalid or protected tool export: ' + name);
          }
          resolved[name] = field.value;
        }
      },
    },
    __ssmcp_finish_sources: {
      configurable:true,
      value:() => {
        for (const name of keys(resolved)) {
          Object.defineProperty(globalThis, name, {value:resolved[name], enumerable:true});
        }
        delete globalThis.__ssmcp_install_source;
        delete globalThis.__ssmcp_finish_sources;
      },
    },
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
  if (!Array.isArray(frame.tool_sources) || !frame.tool_sources.length || frame.tool_sources.length > 17) {
    throw new Error('code_mode requires an explicit selected tool-source bundle');
  }
  let sourceBytes = 0;
  for (const [index, source] of frame.tool_sources.entries()) {
    if (!source || typeof source !== 'object' || typeof source.code !== 'string'
        || !source.code.trim() || source.code.includes('\0')
        || bytes(source.code, 'utf8') > 128 * 1024
        || (sourceBytes += bytes(source.code, 'utf8')) > 192 * 1024
        || typeof source.sha256 !== 'string'
        || createHash('sha256').update(source.code, 'utf8').digest('hex') !== source.sha256) {
      throw new Error('invalid, oversized, or changed tool-source entry');
    }
    new vm.Script('__ssmcp_install_source((\n' + source.code + '\n));', {
      filename:'ssmcp-tool-source-' + index + '.js',
    }).runInContext(context, {timeout:2000});
  }
  new vm.Script('__ssmcp_finish_sources();', {filename:'ssmcp-tools-ready.js'})
    .runInContext(context, {timeout:2000});
  loadingSources = false;
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
