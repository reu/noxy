function encodeUtf8(str) {
  const bytes = [];
  for (let i = 0; i < str.length; i++) {
    let c = str.charCodeAt(i);
    if (c < 0x80) {
      bytes.push(c);
    } else if (c < 0x800) {
      bytes.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
    } else if (c >= 0xd800 && c <= 0xdbff) {
      const hi = c;
      const lo = str.charCodeAt(++i);
      c = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
      bytes.push(
        0xf0 | (c >> 18),
        0x80 | ((c >> 12) & 0x3f),
        0x80 | ((c >> 6) & 0x3f),
        0x80 | (c & 0x3f)
      );
    } else {
      bytes.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
    }
  }
  return new Uint8Array(bytes);
}

class Headers {
  #map;

  constructor(init) {
    this.#map = new Map();
    if (Array.isArray(init)) {
      for (const [k, v] of init) {
        this.#map.set(k.toLowerCase(), String(v));
      }
    }
  }

  get(name) {
    return this.#map.get(name.toLowerCase()) ?? null;
  }
  set(name, value) {
    this.#map.set(name.toLowerCase(), String(value));
  }
  delete(name) {
    this.#map.delete(name.toLowerCase());
  }
  has(name) {
    return this.#map.has(name.toLowerCase());
  }
  entries() {
    return [...this.#map.entries()];
  }
  [Symbol.iterator]() {
    return this.#map[Symbol.iterator]();
  }
}

class Request {
  #bodyConsumed = false;

  constructor(method, url, headers) {
    this.method = method;
    this.url = url;
    this.headers = new Headers(headers);
  }

  async body() {
    if (this.#bodyConsumed) throw new Error("Body already consumed");
    this.#bodyConsumed = true;
    return await Deno.core.ops.op_noxy_req_body();
  }
}

class Response {
  #bodyConsumed = false;
  #customBody;
  #fromUpstream;

  constructor(body, init) {
    init = init ?? {};
    if (typeof body === "string") {
      this.#customBody = encodeUtf8(body);
    } else if (body instanceof Uint8Array) {
      this.#customBody = body;
    } else if (body === null || body === undefined) {
      this.#customBody = new Uint8Array(0);
    } else {
      this.#customBody = body;
    }
    this.status = init.status ?? 200;
    if (init.headers instanceof Headers) {
      this.headers = init.headers;
    } else if (Array.isArray(init.headers)) {
      this.headers = new Headers(init.headers);
    } else if (init.headers && typeof init.headers === "object") {
      this.headers = new Headers(Object.entries(init.headers));
    } else {
      this.headers = new Headers([]);
    }
    this.#fromUpstream = false;
  }

  static _fromUpstream(status, headers) {
    const res = new Response(null, { status, headers });
    res.#fromUpstream = true;
    res.#customBody = undefined;
    res.#bodyConsumed = false;
    return res;
  }

  async body() {
    if (this.#bodyConsumed) throw new Error("Body already consumed");
    this.#bodyConsumed = true;
    if (this.#customBody !== undefined) return this.#customBody;
    return await Deno.core.ops.op_noxy_res_body();
  }

  get _isPassthrough() {
    return this.#fromUpstream && !this.#bodyConsumed && this.#customBody === undefined;
  }

  get _getCustomBody() {
    return this.#customBody;
  }
}

globalThis.Headers = Headers;
globalThis.Request = Request;
globalThis.Response = Response;

globalThis.__noxy_handle = async function (handler, method, url, headers) {
  const req = new Request(method, url, headers);
  let responded = false;

  async function respond(req, newBody) {
    if (responded) throw new Error("respond() already called");
    responded = true;

    const headerEntries = req.headers.entries();
    let bodyBytes = null;
    if (newBody !== undefined && newBody !== null) {
      bodyBytes =
        typeof newBody === "string"
          ? encodeUtf8(newBody)
          : newBody;
    }

    const result = await Deno.core.ops.op_noxy_respond(
      req.method,
      req.url,
      headerEntries,
      bodyBytes
    );

    return Response._fromUpstream(result.status, result.headers);
  }

  const res = await handler(req, respond);

  if (!(res instanceof Response)) {
    throw new Error("Handler must return a Response");
  }

  const headerEntries = res.headers.entries();

  if (res._isPassthrough) {
    Deno.core.ops.op_noxy_finish(res.status, headerEntries, null, true);
    return;
  }

  let body = res._getCustomBody;
  if (body === undefined) {
    body = new Uint8Array(0);
  }

  Deno.core.ops.op_noxy_finish(res.status, headerEntries, body, false);
};
