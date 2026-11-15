(() => {
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
        if (value && typeof value === "object") {
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
        if (typeof id !== "string" || typeof name !== "string") {
            throw new Failure("client ID and tool name must be strings");
        }
        const reply = parse(await native(kind, id, name, stringify(args)));
        if (!reply.ok) throw new Failure(reply.error);
        return reply.value;
    };
    const call = async (id, name, args = {}) => {
        if (!args || typeof args !== "object" || isArray(args)) {
            throw new Failure("tool arguments must be a JSON object");
        }
        return dispatch("call", id, name, args);
    };
    const tools = async (id, cursor = null) => {
        if (cursor !== null && typeof cursor !== "string") {
            throw new Failure("tool cursor must be a string or null");
        }
        return dispatch("tools", id, "", cursor);
    };
    const print = (...args) => {
        const line = args.map(value => typeof value === "string" ? value : stringify(value) ?? text(value)).join(" ");
        if (!output(line)) throw new Failure("code_mode log output exceeds 65536 bytes");
    };
    Object.defineProperties(globalThis, {
        clients: {value: clients, enumerable: true},
        call: {value: call, enumerable: true},
        tools: {value: tools, enumerable: true},
        print: {value: print, enumerable: true},
        console: {value: freeze({log: print, warn: print, error: print}), enumerable: true},
    });
})();
