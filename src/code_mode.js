'use strict';
// System Node.js worker. bubblewrap provides isolation; node:vm only shapes the API.
const vm = require('node:vm');
const readline = require('node:readline');
const { types } = require('node:util');
const encode = JSON.stringify;
const decode = JSON.parse;
const bytes = Buffer.byteLength;
const MAX_FRAME = 4 * 1024 * 1024;
const MAX_RESULT = 1024 * 1024;
const MAX_LOG = 64 * 1024;
const MAX_CALLS = 128;
const MAX_PENDING = 16;
const pending = new Map();
const unhandled = new Map();
let started = false;
let finished = false;
let nextId = 0;
let logBytes = 0;
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

const bootstrap = `(() => {
  const native = globalThis.__ssmcp_host;
  const output = globalThis.__ssmcp_log;
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
  const tools = async (id, cursor = null) => {
    if (cursor !== null && typeof cursor !== 'string') {
      throw new Failure('tool cursor must be a string or null');
    }
    return dispatch('tools', id, '', cursor);
  };
  const builtin = (name, args = {}) => dispatch('builtin', '_', name, args);
  const workspaceResult = async (id, name, args = {}) => {
    const reply = await call(id, name, args);
    if (reply.isError) {
      const detail = reply.content?.find(item => item.type === 'text')?.text;
      throw new Failure(typeof detail === 'string' ? detail.slice(0, 4096) : 'workspace operation failed');
    }
    if (!reply.structuredContent) throw new Failure('workspace client returned no structured result');
    return reply.structuredContent;
  };
  const file = (name, id, args) => workspaceResult(id, 'ssmcp.fs.' + name, args);
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
    fs: { value: fs, enumerable: true },
    context: { value: context, enumerable: true },
    skills: { value: skills, enumerable: true },
    agents: { value: agents, enumerable: true },
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
