// The engine's host layer. Runs inside V8, in the fetcher's own process.
// It provides exactly what a challenge page needs: a small DOM, a script loader
// that executes the page's own scripts in document order, and the browser
// surfaces the challenge reads. No layout, no rendering, no browser.
(function () {
  "use strict";
  // The host keeps the only reference to the engine's ops. The page never sees
  // `Deno`: a browser has none, and a page that could call an op directly would be
  // outside the host layer's checks (the Rust side enforces them regardless).
  const core = Deno.core;
  const ops = core.ops;

  // ---------- base64 without Node ----------
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  function btoa(input) {
    let out = "";
    for (let i = 0; i < input.length; i += 3) {
      const a = input.charCodeAt(i) & 0xff;
      const b = i + 1 < input.length ? input.charCodeAt(i + 1) & 0xff : 0;
      const c = i + 2 < input.length ? input.charCodeAt(i + 2) & 0xff : 0;
      out += B64[a >> 2] + B64[((a & 3) << 4) | (b >> 4)];
      out += i + 1 < input.length ? B64[((b & 15) << 2) | (c >> 6)] : "=";
      out += i + 2 < input.length ? B64[c & 63] : "=";
    }
    return out;
  }
  function atob(input) {
    const clean = String(input).replace(/[^A-Za-z0-9+/=]/g, "");
    let out = "";
    for (let i = 0; i < clean.length; i += 4) {
      const n = (B64.indexOf(clean[i]) << 18) | (B64.indexOf(clean[i + 1]) << 12) |
        ((B64.indexOf(clean[i + 2]) & 63) << 6) | (B64.indexOf(clean[i + 3]) & 63);
      out += String.fromCharCode((n >> 16) & 255);
      if (clean[i + 2] !== "=") out += String.fromCharCode((n >> 8) & 255);
      if (clean[i + 3] !== "=") out += String.fromCharCode(n & 255);
    }
    return out;
  }

  // ---------- what V8 alone doesn't ship ----------
  // WHATWG URL: parsing and every setter run in the `url` crate (op_url), which
  // implements the URL Standard's API; searchParams is live in both directions,
  // as a page that does `u.searchParams.set(...)` then reads `u.href` expects.
  const URL_PARTS = ["href", "origin", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"];
  const URL_STATE = new WeakMap();
  const QUERY_OWNER = new WeakMap();
  function urlParse(input, base, setter, value) {
    const out = ops.op_url(String(input), base === undefined ? "" : String(base), setter || "", value === undefined ? "" : String(value));
    return out ? JSON.parse(out) : null;
  }
  class URLShim {
    constructor(input, base) {
      if (arguments.length === 0) throw new TypeError("Failed to construct 'URL': 1 argument required, but only 0 present.");
      const baseHref = base === undefined ? undefined : String(base);
      if (baseHref !== undefined && !urlParse(baseHref)) throw new TypeError("Failed to construct 'URL': Invalid base URL");
      const parts = urlParse(String(input), baseHref);
      if (!parts) throw new TypeError("Failed to construct 'URL': Invalid URL");
      URL_STATE.set(this, { parts, query: null });
    }
    static canParse(input, base) { try { new URLShim(input, base); return true; } catch (e) { return false; } }
    static parse(input, base) { try { return new URLShim(input, base); } catch (e) { return null; } }
    get searchParams() {
      const state = URL_STATE.get(this);
      if (!state.query) { state.query = new URLSearchParamsShim(state.parts.search); QUERY_OWNER.set(state.query, this); }
      return state.query;
    }
    toString() { return URL_STATE.get(this).parts.href; }
    toJSON() { return URL_STATE.get(this).parts.href; }
    get [Symbol.toStringTag]() { return "URL"; }
  }
  for (const part of URL_PARTS) {
    Object.defineProperty(URLShim.prototype, part, {
      configurable: true, enumerable: true,
      get() { return URL_STATE.get(this).parts[part]; },
      set: part === "origin" ? undefined : function (value) {
        const state = URL_STATE.get(this);
        const next = urlParse(state.parts.href, undefined, part, value);
        if (!next) throw new TypeError("Failed to set the '" + part + "' property on 'URL': Invalid URL");
        state.parts = next;
        if (state.query && (part === "search" || part === "href")) state.query._load(next.search);
      },
    });
  }
  // application/x-www-form-urlencoded, byte-exact with the URL Standard.
  const isHex = (c) => (c >= 48 && c <= 57) || (c >= 65 && c <= 70) || (c >= 97 && c <= 102);
  function formDecode(text) {
    const bytes = core.encode(String(text).replace(/\+/g, " "));
    const out = [];
    for (let i = 0; i < bytes.length; i++) {
      if (bytes[i] === 37 && i + 2 < bytes.length && isHex(bytes[i + 1]) && isHex(bytes[i + 2])) {
        out.push(parseInt(String.fromCharCode(bytes[i + 1], bytes[i + 2]), 16)); i += 2;
      } else out.push(bytes[i]);
    }
    return core.decode(new Uint8Array(out));
  }
  function formEncode(text) {
    const bytes = core.encode(String(text));
    let out = "";
    for (const b of bytes) {
      if (b === 32) out += "+";
      else if ((b >= 48 && b <= 57) || (b >= 65 && b <= 90) || (b >= 97 && b <= 122) || b === 42 || b === 45 || b === 46 || b === 95) out += String.fromCharCode(b);
      else out += "%" + (b < 16 ? "0" : "") + b.toString(16).toUpperCase();
    }
    return out;
  }
  class URLSearchParamsShim {
    constructor(init) {
      this._list = [];
      if (init instanceof URLSearchParamsShim) this._list = init._list.map((pair) => pair.slice());
      else if (init !== null && typeof init === "object") {
        if (typeof init[Symbol.iterator] === "function") {
          for (const pair of init) {
            const items = Array.from(pair);
            if (items.length !== 2) throw new TypeError("Failed to construct 'URLSearchParams': The provided value cannot be converted to a sequence.");
            this._list.push([String(items[0]), String(items[1])]);
          }
        } else for (const key of Object.keys(init)) this._list.push([key, String(init[key])]);
      } else if (init !== undefined) this._load(String(init));
    }
    _load(query) {
      this._list = [];
      for (const part of String(query).replace(/^\?/, "").split("&")) {
        if (!part) continue;
        const at = part.indexOf("=");
        this._list.push(at < 0 ? [formDecode(part), ""] : [formDecode(part.slice(0, at)), formDecode(part.slice(at + 1))]);
      }
    }
    _update() {
      const owner = QUERY_OWNER.get(this);
      if (!owner) return;
      const state = URL_STATE.get(owner);
      state.parts = urlParse(state.parts.href, undefined, "search", this.toString()) || state.parts;
    }
    get size() { return this._list.length; }
    append(name, value) { this._list.push([String(name), String(value)]); this._update(); }
    delete(name, value) { this._list = this._list.filter(([k, v]) => !(k === String(name) && (value === undefined || v === String(value)))); this._update(); }
    get(name) { const hit = this._list.find(([k]) => k === String(name)); return hit ? hit[1] : null; }
    getAll(name) { return this._list.filter(([k]) => k === String(name)).map(([, v]) => v); }
    has(name, value) { return this._list.some(([k, v]) => k === String(name) && (value === undefined || v === String(value))); }
    set(name, value) {
      const key = String(name), at = this._list.findIndex(([k]) => k === key);
      if (at < 0) this._list.push([key, String(value)]);
      else { this._list[at][1] = String(value); this._list = this._list.filter(([k], i) => k !== key || i === at); }
      this._update();
    }
    sort() { this._list = this._list.map((pair, i) => [pair, i]).sort((a, b) => (a[0][0] < b[0][0] ? -1 : a[0][0] > b[0][0] ? 1 : a[1] - b[1])).map(([pair]) => pair); this._update(); }
    forEach(callback, thisArg) { for (const [k, v] of this._list.slice()) callback.call(thisArg, v, k, this); }
    *entries() { for (const pair of this._list) yield pair.slice(); }
    *keys() { for (const [k] of this._list) yield k; }
    *values() { for (const [, v] of this._list) yield v; }
    [Symbol.iterator]() { return this.entries(); }
    toString() { return this._list.map(([k, v]) => formEncode(k) + "=" + formEncode(v)).join("&"); }
    get [Symbol.toStringTag]() { return "URLSearchParams"; }
  }
  // Text and bytes go through V8's own UTF-8 codec (the engine's core), so a
  // proof computed over bytes is the proof a browser computes.
  class TextEncoderShim {
    get encoding() { return "utf-8"; }
    encode(s) { return core.encode(s === undefined ? "" : String(s)); }
    encodeInto(s, target) {
      const bytes = core.encode(String(s));
      const written = Math.min(bytes.length, target.length);
      target.set(bytes.subarray(0, written));
      return { read: String(s).length, written };
    }
  }
  class TextDecoderShim {
    constructor(label) { this.encoding = String(label || "utf-8").toLowerCase(); }
    decode(buf) {
      if (buf === undefined || buf === null) return "";
      return core.decode(toBytes(buf));
    }
  }

  // ---------- bytes ----------
  // Every binary surface (Blob, FileReader, FormData, request bodies) carries real
  // bytes. A browser sends what a page put in a buffer; sending its string form
  // ("[object ArrayBuffer]") is a different request, and a vendor sees it.
  function toBytes(part) {
    if (part instanceof BlobShim) return part._bytes;
    if (part instanceof ArrayBuffer) return new Uint8Array(part);
    if (ArrayBuffer.isView(part)) return new Uint8Array(part.buffer, part.byteOffset, part.byteLength);
    return core.encode(String(part));
  }
  function concatBytes(chunks) {
    let total = 0;
    for (const chunk of chunks) total += chunk.length;
    const out = new Uint8Array(total);
    let at = 0;
    for (const chunk of chunks) { out.set(chunk, at); at += chunk.length; }
    return out;
  }
  function bytesToBase64(bytes) {
    const parts = [];
    let chunk = "";
    for (let i = 0; i < bytes.length; i += 3) {
      const a = bytes[i], b = i + 1 < bytes.length ? bytes[i + 1] : 0, c = i + 2 < bytes.length ? bytes[i + 2] : 0;
      chunk += B64[a >> 2] + B64[((a & 3) << 4) | (b >> 4)] +
        (i + 1 < bytes.length ? B64[((b & 15) << 2) | (c >> 6)] : "=") +
        (i + 2 < bytes.length ? B64[c & 63] : "=");
      if (chunk.length >= 8192) { parts.push(chunk); chunk = ""; }
    }
    parts.push(chunk);
    return parts.join("");
  }
  function base64ToBytes(text) {
    const clean = String(text).replace(/[^A-Za-z0-9+/]/g, "");
    const out = new Uint8Array(Math.floor(clean.length * 3 / 4));
    let at = 0;
    for (let i = 0; i < clean.length; i += 4) {
      const n = (B64.indexOf(clean[i]) << 18) | (B64.indexOf(clean[i + 1]) << 12) |
        ((i + 2 < clean.length ? B64.indexOf(clean[i + 2]) : 0) << 6) | (i + 3 < clean.length ? B64.indexOf(clean[i + 3]) : 0);
      if (at < out.length) out[at++] = (n >> 16) & 255;
      if (i + 2 < clean.length && at < out.length) out[at++] = (n >> 8) & 255;
      if (i + 3 < clean.length && at < out.length) out[at++] = n & 255;
    }
    return out;
  }

  // V8 alone has none of these; the challenge reads them.
  globalThis.URL = URLShim;
  globalThis.URLSearchParams = URLSearchParamsShim;
  globalThis.TextEncoder = TextEncoderShim;
  globalThis.TextDecoder = TextDecoderShim;
  class BlobShim {
    constructor(parts, options) {
      const chunks = [];
      for (const part of parts || []) chunks.push(toBytes(part).slice());
      this._bytes = concatBytes(chunks);
      // A browser lowercases the type and drops one it cannot represent.
      const type = String((options && options.type) || "");
      this.type = /^[\x20-\x7e]*$/.test(type) ? type.toLowerCase() : "";
      note("log", "blob " + this._bytes.length + "B " + (this.type || "untyped"));
    }
    get size() { return this._bytes.length; }
    text() { return Promise.resolve(core.decode(this._bytes)); }
    arrayBuffer() { return Promise.resolve(this._bytes.slice().buffer); }
    bytes() { return Promise.resolve(this._bytes.slice()); }
    slice(start, end, type) {
      const size = this._bytes.length;
      const clamp = (value, fallback) => {
        if (value === undefined) return fallback;
        const n = Math.trunc(Number(value)) || 0;
        return n < 0 ? Math.max(size + n, 0) : Math.min(n, size);
      };
      const from = clamp(start, 0), to = clamp(end, size);
      return new BlobShim([this._bytes.subarray(from, Math.max(from, to))], { type: type === undefined ? "" : type });
    }
  }
  globalThis.Blob = BlobShim;
  class FileReaderShim {
    constructor() { this.result = null; this.error = null; this.readyState = 0; this._listeners = {}; }
    addEventListener(kind, handler) { (this._listeners[kind] = this._listeners[kind] || []).push(handler); }
    removeEventListener(kind, handler) { this._listeners[kind] = (this._listeners[kind] || []).filter((h) => h !== handler); }
    _read(blob, convert) {
      this.readyState = 1;
      const bytes = blob instanceof BlobShim ? blob._bytes : toBytes(blob);
      // Completion is asynchronous in a browser; a page that attaches onload after
      // calling read* still sees it fire.
      Promise.resolve().then(() => {
        this.result = convert(bytes, blob);
        this.readyState = 2;
        const event = { type: "load", target: this };
        for (const kind of ["load", "loadend"]) {
          const inline = this["on" + kind];
          if (typeof inline === "function") { try { inline.call(this, { type: kind, target: this }); } catch (e) { note("log", "filereader " + kind + ": " + (e && e.message)); } }
          for (const handler of this._listeners[kind] || []) { try { handler.call(this, { type: kind, target: this }); } catch (e) {} }
        }
        return event;
      });
    }
    readAsDataURL(blob) {
      this._read(blob, (bytes, source) => "data:" + ((source && source.type) || "application/octet-stream") + ";base64," + bytesToBase64(bytes));
    }
    readAsText(blob) { this._read(blob, (bytes) => core.decode(bytes)); }
    readAsArrayBuffer(blob) { this._read(blob, (bytes) => bytes.slice().buffer); }
    readAsBinaryString(blob) { this._read(blob, (bytes) => { let s = ""; for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]); return s; }); }
    abort() {}
  }
  globalThis.FileReader = FileReaderShim;
  // Entries in insertion order, repeats allowed, as a browser's FormData keeps them.
  class FormDataShim {
    constructor() { this._entries = []; }
    _value(value, filename) {
      if (value instanceof BlobShim) {
        const file = Object.create(BlobShim.prototype);
        file._bytes = value._bytes; file.type = value.type;
        file.name = filename !== undefined ? String(filename) : (value.name !== undefined ? value.name : "blob");
        return file;
      }
      return String(value);
    }
    append(k, v, filename) { this._entries.push([String(k), this._value(v, filename)]); }
    set(k, v, filename) {
      const key = String(k);
      const value = this._value(v, filename);
      const at = this._entries.findIndex(([name]) => name === key);
      if (at < 0) { this._entries.push([key, value]); return; }
      // The first entry keeps its place; later repeats go.
      this._entries = this._entries.filter(([name], index) => name !== key || index === at);
      this._entries[at][1] = value;
    }
    get(k) { const hit = this._entries.find(([name]) => name === String(k)); return hit ? hit[1] : null; }
    getAll(k) { return this._entries.filter(([name]) => name === String(k)).map(([, value]) => value); }
    has(k) { return this._entries.some(([name]) => name === String(k)); }
    delete(k) { this._entries = this._entries.filter(([name]) => name !== String(k)); }
    entries() { return this._entries.map((entry) => entry.slice())[Symbol.iterator](); }
    keys() { return this._entries.map(([name]) => name)[Symbol.iterator](); }
    values() { return this._entries.map(([, value]) => value)[Symbol.iterator](); }
    forEach(fn, self) { for (const [name, value] of this._entries) fn.call(self, value, name, this); }
    [Symbol.iterator]() { return this.entries(); }
  }
  globalThis.FormData = FormDataShim;
  // Chrome's multipart boundary: a page's server can read its shape.
  const BOUNDARY_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  function webkitBoundary() {
    const hex = ops.op_random(16);
    let out = "----WebKitFormBoundary";
    for (let i = 0; i < 16; i++) out += BOUNDARY_CHARS[parseInt(hex.substr(i * 2, 2), 16) % BOUNDARY_CHARS.length];
    return out;
  }
  // multipart/form-data exactly as Blink writes it: CRLF lines, quoted names with
  // `"` and newlines escaped, a filename and a type for every Blob entry.
  function encodeMultipart(form) {
    const boundary = webkitBoundary();
    const escape = (s) => String(s).replace(/\r/g, "%0D").replace(/\n/g, "%0A").replace(/"/g, "%22");
    const chunks = [];
    for (const [name, value] of form._entries) {
      let head = "--" + boundary + "\r\nContent-Disposition: form-data; name=\"" + escape(name) + "\"";
      if (value instanceof BlobShim) {
        head += "; filename=\"" + escape(value.name !== undefined ? value.name : "blob") + "\"\r\nContent-Type: " + (value.type || "application/octet-stream") + "\r\n\r\n";
        chunks.push(core.encode(head), value._bytes, core.encode("\r\n"));
      } else {
        head += "\r\n\r\n";
        chunks.push(core.encode(head + String(value).replace(/\r(?!\n)|(?<!\r)\n/g, "\r\n") + "\r\n"));
      }
    }
    chunks.push(core.encode("--" + boundary + "--\r\n"));
    return { bytes: concatBytes(chunks), type: "multipart/form-data; boundary=" + boundary };
  }
  // A request body as bytes plus the content type a browser would infer for it.
  function encodeBody(body) {
    if (body === undefined || body === null) return { bytes: null, type: null };
    if (body instanceof FormDataShim) return encodeMultipart(body);
    if (body instanceof BlobShim) return { bytes: body._bytes, type: body.type || null };
    if (body instanceof URLSearchParamsShim) return { bytes: core.encode(body.toString()), type: "application/x-www-form-urlencoded;charset=UTF-8" };
    if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) return { bytes: toBytes(body), type: null };
    return { bytes: core.encode(String(body)), type: "text/plain;charset=UTF-8" };
  }

  // The challenge computes its proof in a Worker built from a Blob URL. Without
  // that path it falls back to a token the page will not accept.
  const blobUrls = new Map();
  URLShim.createObjectURL = (blob) => {
    const text = blob instanceof BlobShim ? core.decode(blob._bytes) : String(blob);
    note("log", "createObjectURL " + text.length + "B");
    const id = "blob:" + (M && M.page ? new URLShim(M.page.url).origin : "null") + "/" + ops.op_random(16).replace(/^(.{8})(.{4})(.{4})(.{4})/, "$1-$2-$3-$4-");
    blobUrls.set(id, text);
    return id;
  };
  URLShim.revokeObjectURL = (id) => { blobUrls.delete(String(id)); };
  class WorkerShim {
    constructor(url) {
      this._url = String(url);
      note("log", "worker " + this._url.slice(0, 50));
      this.onmessage = null;
      this._listeners = [];
      this._source = blobUrls.get(this._url) || null;
    }
    addEventListener(kind, handler) { if (kind === "message") this._listeners.push(handler); }
    postMessage(data) {
      let source = this._source;
      if (source == null) {
        try {
          const response = netFetch(this._url, { method: "GET", __dest: "worker", mode: "same-origin" });
          source = response.textSync || "";
        } catch (e) { note("log", "worker load: " + (e && e.message)); return; }
      }
      const workerSelf = {
        onmessage: null,
        postMessage: (message) => {
          const event = { data: message };
          if (this.onmessage) { try { this.onmessage(event); } catch (e) { note("log", "worker reply: " + (e && e.message)); } }
          for (const listener of this._listeners) { try { listener(event); } catch (e) {} }
        },
        close() {},
        addEventListener(kind, handler) { if (kind === "message") workerSelf.onmessage = handler; },
      };
      const previousSelf = globalThis.self;
      const previousPost = globalThis.postMessage;
      globalThis.self = workerSelf;
      globalThis.postMessage = workerSelf.postMessage;
      try {
        const outcome = core.evalContext(String(source), this._url.startsWith("blob:") ? this._url : new URL(this._url, M.page.url).href);
        if (outcome && outcome[1]) throw outcome[1].thrown;
        const deliver = { data };
        if (workerSelf.onmessage) workerSelf.onmessage(deliver);
      } catch (e) {
        note("log", "worker: " + (e && e.message));
      } finally {
        globalThis.self = previousSelf;
        globalThis.postMessage = previousPost;
      }
    }
    terminate() {}
  }
  globalThis.Worker = WorkerShim;

  // ---------- per-mint state ----------
  let M = null;
  const note = (kind, name) => {
    if (!M) return;
    if (kind === "surface") M.surfaces.add(name);
    else if (kind === "log") M.logs.push(name);
  };

  // ---------- WebCrypto ----------
  // Every primitive runs in BoringSSL (op_crypto). A key's material lives in a
  // WeakMap the page cannot reach, as a CryptoKey's slots do in a browser. What
  // is not implemented rejects with the name Chrome uses, never with a fake
  // result: an empty ciphertext is a signal the vendor reads as a bot.
  class DOMExceptionShim extends Error {
    constructor(message = "", name = "Error") { super(message); Object.defineProperty(this, "name", { value: name, configurable: true, writable: true }); }
    get code() { return ({ IndexSizeError: 1, NotFoundError: 8, NotSupportedError: 9, InvalidStateError: 11, SyntaxError: 12, InvalidAccessError: 15, TypeMismatchError: 17, QuotaExceededError: 22, DataCloneError: 25 })[this.name] || 0; }
  }
  if (typeof globalThis.DOMException !== "function") globalThis.DOMException = DOMExceptionShim;
  const DOMErr = globalThis.DOMException;
  const fail = (name, message) => new DOMErr(message || "", name);

  const KEYS = new WeakMap();
  class CryptoKeyShim {
    constructor() { throw new TypeError("Illegal constructor"); }
    get type() { return KEYS.get(this).type; }
    get extractable() { return KEYS.get(this).extractable; }
    get algorithm() { return KEYS.get(this).algorithm; }
    get usages() { return KEYS.get(this).usages; }
    get [Symbol.toStringTag]() { return "CryptoKey"; }
  }
  globalThis.CryptoKey = CryptoKeyShim;
  function makeKey(type, extractable, algorithm, usages, bytes) {
    const key = Object.create(CryptoKeyShim.prototype);
    KEYS.set(key, { type, extractable: !!extractable, algorithm, usages: Array.from(usages), bytes });
    return key;
  }
  function slotOf(key, usage, name) {
    const slot = key && KEYS.get(key);
    if (!slot) throw new TypeError("parameter is not of type 'CryptoKey'.");
    if (name && slot.algorithm.name !== name) throw fail("InvalidAccessError", "The requested operation is not valid for the provided key");
    if (usage && !slot.usages.includes(usage)) throw fail("InvalidAccessError", "key.usages does not permit this operation");
    return slot;
  }

  const ALGORITHMS = ["AES-GCM", "AES-CBC", "AES-CTR", "AES-KW", "HMAC", "PBKDF2", "HKDF", "SHA-1", "SHA-256", "SHA-384", "SHA-512",
    "RSA-OAEP", "RSA-PSS", "RSASSA-PKCS1-v1_5", "ECDSA", "ECDH", "Ed25519", "X25519"];
  const SECRET_USAGES = {
    "AES-GCM": ["encrypt", "decrypt", "wrapKey", "unwrapKey"], "AES-CBC": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
    "AES-CTR": ["encrypt", "decrypt", "wrapKey", "unwrapKey"], "AES-KW": ["wrapKey", "unwrapKey"],
    HMAC: ["sign", "verify"], PBKDF2: ["deriveBits", "deriveKey"], HKDF: ["deriveBits", "deriveKey"],
  };
  function normalize(algorithm) {
    const given = typeof algorithm === "string" ? { name: algorithm } : algorithm;
    if (!given || typeof given !== "object" || given.name === undefined) throw new TypeError("Algorithm: name: Missing or not a string");
    const wanted = String(given.name).toUpperCase();
    const name = ALGORITHMS.find((known) => known.toUpperCase() === wanted);
    if (!name) throw fail("NotSupportedError", "Algorithm: Unrecognized name");
    return Object.assign({}, given, { name });
  }
  const hashOf = (hash) => {
    if (hash === undefined) throw new TypeError("Algorithm: hash: Missing or not an AlgorithmIdentifier");
    const name = normalize(hash).name;
    if (!/^SHA-/.test(name)) throw fail("NotSupportedError", "Algorithm: Unrecognized name");
    return name;
  };
  const blockBits = (hash) => (hash === "SHA-384" || hash === "SHA-512" ? 1024 : 512);
  function sourceBytes(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data.slice(0));
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
    throw new TypeError("parameter is not of type '(ArrayBuffer or ArrayBufferView)'.");
  }
  const hexOf = (bytes) => { let out = ""; for (let i = 0; i < bytes.length; i++) out += (bytes[i] < 16 ? "0" : "") + bytes[i].toString(16); return out; };
  const randomBytes = (n) => { const hex = ops.op_random(n); const out = new Uint8Array(n); for (let i = 0; i < n; i++) out[i] = parseInt(hex.substr(i * 2, 2), 16); return out; };
  const b64url = (bytes) => bytesToBase64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const NO_BYTES = new Uint8Array(0);
  function primitive(operation, params, key, data) {
    const out = ops.op_crypto(operation, JSON.stringify(params), key || NO_BYTES, data || NO_BYTES);
    if (out[0] !== 0) {
      note("log", "webcrypto " + operation + " " + params.name + ": " + core.decode(out.subarray(1)));
      throw out[0] === 1 ? fail("NotSupportedError", "Algorithm: Unrecognized name") : fail("OperationError", "");
    }
    return out.slice(1).buffer;
  }
  function cipherParams(algorithm) {
    const params = { name: algorithm.name };
    if (algorithm.name === "AES-GCM") {
      params.iv = hexOf(sourceBytes(algorithm.iv));
      if (algorithm.additionalData !== undefined) params.additionalData = hexOf(sourceBytes(algorithm.additionalData));
      if (algorithm.tagLength !== undefined) params.tagLength = Number(algorithm.tagLength);
    } else if (algorithm.name === "AES-CBC") {
      params.iv = hexOf(sourceBytes(algorithm.iv));
      if (params.iv.length !== 32) throw fail("OperationError", "The \"iv\" has an unexpected length -- must be 16 bytes");
    } else if (algorithm.name === "AES-CTR") {
      // BoringSSL counts over the whole block; WebCrypto's `length` only differs
      // when the counter wraps, which needs 2^length blocks.
      params.iv = hexOf(sourceBytes(algorithm.counter));
      const length = Number(algorithm.length);
      if (params.iv.length !== 32 || !(length >= 1 && length <= 128)) throw fail("OperationError", "");
    } else if (algorithm.name !== "AES-KW") {
      throw fail("NotSupportedError", "Algorithm: Unrecognized name");
    }
    return params;
  }
  function cipher(operation, algorithm, key, data, usage) {
    algorithm = normalize(algorithm);
    const params = cipherParams(algorithm);
    const slot = slotOf(key, usage || operation, algorithm.name);
    const input = sourceBytes(data);
    if (DEBUG && operation === "encrypt") note("log", "encrypt-plain " + core.decode(input));
    return primitive(operation, params, slot.bytes, input);
  }
  function importSecret(format, keyData, algorithm, extractable, usages) {
    algorithm = normalize(algorithm);
    const name = algorithm.name;
    if (!SECRET_USAGES[name]) throw fail("NotSupportedError", "Algorithm: Unrecognized name");
    usages = Array.from(usages || []);
    for (const usage of usages) if (!SECRET_USAGES[name].includes(usage)) throw fail("SyntaxError", "Cannot create a key using the specified key usages.");
    let bytes;
    if (format === "raw") bytes = sourceBytes(keyData);
    else if (format === "jwk" && name !== "PBKDF2" && name !== "HKDF") {
      if (!keyData || typeof keyData !== "object") throw new TypeError("Key data must be an object for JWK import");
      if (keyData.kty !== "oct" || typeof keyData.k !== "string") throw fail("DataError", "The JWK \"kty\" member was not \"oct\"");
      if (keyData.ext === false && extractable) throw fail("DataError", "The JWK \"ext\" member was inconsistent with that specified by the Web Crypto call");
      bytes = base64ToBytes(keyData.k.replace(/-/g, "+").replace(/_/g, "/"));
    } else throw fail("NotSupportedError", "Unsupported import key format for algorithm");
    if (!usages.length) throw fail("SyntaxError", "Usages cannot be empty when creating a key.");
    let shape;
    if (name.startsWith("AES-")) {
      if (![16, 24, 32].includes(bytes.length)) throw fail("DataError", "AES key data must be 128 or 256 bits");
      shape = { name, length: bytes.length * 8 };
    } else if (name === "HMAC") {
      const hash = hashOf(algorithm.hash);
      if (!bytes.length) throw fail("DataError", "HMAC key data must not be empty");
      shape = { name, hash: { name: hash }, length: algorithm.length !== undefined ? Number(algorithm.length) : bytes.length * 8 };
    } else {
      if (extractable) throw fail("SyntaxError", "Key derivation keys must not be extractable");
      shape = { name };
    }
    return makeKey("secret", extractable, shape, usages, bytes);
  }
  function exportSecret(format, key) {
    const slot = slotOf(key);
    if (!slot.extractable) throw fail("InvalidAccessError", "key is not extractable");
    if (format === "raw") return slot.bytes.slice().buffer;
    if (format !== "jwk") throw fail("NotSupportedError", "Unsupported export key format for algorithm");
    const { name, length, hash } = slot.algorithm;
    const alg = name === "HMAC" ? "HS" + hash.name.slice(4) : name.startsWith("AES-") ? "A" + length + name.slice(4) : undefined;
    if (!alg) throw fail("NotSupportedError", "Unsupported export key format for algorithm");
    return { alg, ext: true, k: b64url(slot.bytes), key_ops: slot.usages.slice(), kty: "oct" };
  }
  function derivedLength(algorithm) {
    algorithm = normalize(algorithm);
    if (algorithm.name.startsWith("AES-")) {
      const length = Number(algorithm.length);
      if (![128, 192, 256].includes(length)) throw fail("OperationError", "AES key length must be 128 or 256 bits");
      return length;
    }
    if (algorithm.name === "HMAC") return algorithm.length !== undefined ? Number(algorithm.length) : blockBits(hashOf(algorithm.hash));
    throw fail("NotSupportedError", "Algorithm: Unrecognized name");
  }
  function deriveBits(algorithm, baseKey, length, usage) {
    algorithm = normalize(algorithm);
    const slot = slotOf(baseKey, usage, algorithm.name);
    const params = { name: algorithm.name, hash: hashOf(algorithm.hash), salt: hexOf(sourceBytes(algorithm.salt)), length: Number(length) };
    if (algorithm.name === "PBKDF2") params.iterations = Number(algorithm.iterations);
    else if (algorithm.name === "HKDF") params.info = hexOf(sourceBytes(algorithm.info));
    else throw fail("NotSupportedError", "Algorithm: Unrecognized name");
    return primitive("derive-bits", params, slot.bytes, NO_BYTES);
  }
  const later = (name, work) => {
    note("surface", "crypto.subtle." + name);
    return new Promise((resolve, reject) => { try { resolve(work()); } catch (error) { reject(error); } });
  };
  class SubtleCryptoShim {
    constructor() { throw new TypeError("Illegal constructor"); }
    digest(algorithm, data) {
      return later("digest", () => primitive("digest", { name: hashOf(algorithm) }, NO_BYTES, sourceBytes(data)));
    }
    importKey(format, keyData, algorithm, extractable, usages) {
      return later("importKey", () => importSecret(format, keyData, algorithm, extractable, usages));
    }
    exportKey(format, key) { return later("exportKey", () => exportSecret(format, key)); }
    generateKey(algorithm, extractable, usages) {
      return later("generateKey", () => {
        const normal = normalize(algorithm);
        if (!SECRET_USAGES[normal.name] || normal.name === "PBKDF2" || normal.name === "HKDF") throw fail("NotSupportedError", "Algorithm: Unrecognized name");
        return importSecret("raw", randomBytes(derivedLength(normal) / 8), normal, extractable, usages);
      });
    }
    encrypt(algorithm, key, data) { return later("encrypt", () => cipher("encrypt", algorithm, key, data)); }
    decrypt(algorithm, key, data) { return later("decrypt", () => cipher("decrypt", algorithm, key, data)); }
    sign(algorithm, key, data) {
      return later("sign", () => {
        const normal = normalize(algorithm);
        if (normal.name !== "HMAC") throw fail("NotSupportedError", "Algorithm: Unrecognized name");
        const slot = slotOf(key, "sign", "HMAC");
        return primitive("sign", { name: "HMAC", hash: slot.algorithm.hash.name }, slot.bytes, sourceBytes(data));
      });
    }
    verify(algorithm, key, signature, data) {
      return later("verify", () => {
        const normal = normalize(algorithm);
        if (normal.name !== "HMAC") throw fail("NotSupportedError", "Algorithm: Unrecognized name");
        const slot = slotOf(key, "verify", "HMAC");
        const expected = new Uint8Array(primitive("sign", { name: "HMAC", hash: slot.algorithm.hash.name }, slot.bytes, sourceBytes(data)));
        const given = sourceBytes(signature);
        let diff = expected.length ^ given.length;
        for (let i = 0; i < expected.length; i++) diff |= expected[i] ^ (given[i] | 0);
        return diff === 0;
      });
    }
    deriveBits(algorithm, baseKey, length) {
      return later("deriveBits", () => deriveBits(algorithm, baseKey, length, "deriveBits"));
    }
    deriveKey(algorithm, baseKey, derivedKeyAlgorithm, extractable, usages) {
      return later("deriveKey", () => {
        const bits = deriveBits(algorithm, baseKey, derivedLength(derivedKeyAlgorithm), "deriveKey");
        return importSecret("raw", bits, derivedKeyAlgorithm, extractable, usages);
      });
    }
    wrapKey(format, key, wrappingKey, wrapAlgorithm) {
      return later("wrapKey", () => {
        const exported = exportSecret(format, key);
        const bytes = format === "jwk" ? core.encode(JSON.stringify(exported)) : new Uint8Array(exported);
        return cipher("encrypt", wrapAlgorithm, wrappingKey, bytes, "wrapKey");
      });
    }
    unwrapKey(format, wrapped, unwrappingKey, unwrapAlgorithm, unwrappedAlgorithm, extractable, usages) {
      return later("unwrapKey", () => {
        const plain = new Uint8Array(cipher("decrypt", unwrapAlgorithm, unwrappingKey, wrapped, "unwrapKey"));
        let keyData = plain;
        if (format === "jwk") {
          try { keyData = JSON.parse(core.decode(plain)); } catch (_) { throw fail("DataError", "The JWK could not be parsed"); }
        }
        return importSecret(format, keyData, unwrappedAlgorithm, extractable, usages);
      });
    }
    get [Symbol.toStringTag]() { return "SubtleCrypto"; }
  }
  globalThis.SubtleCrypto = SubtleCryptoShim;
  const SUBTLE = Object.create(SubtleCryptoShim.prototype);
  // Debug only: every SubtleCrypto call and its inputs, to diff against a reference.
  function traceSubtle() {
    const show = (value) => {
      if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
        const bytes = value instanceof ArrayBuffer ? new Uint8Array(value) : new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        let hex = "";
        for (let i = 0; i < Math.min(bytes.length, 48); i++) hex += bytes[i].toString(16).padStart(2, "0");
        return "bytes" + bytes.length + ":" + hex;
      }
      if (value instanceof CryptoKeyShim) return "key:" + JSON.stringify(value.algorithm);
      try { return JSON.stringify(value, (k, v) => (v instanceof ArrayBuffer || ArrayBuffer.isView(v) ? show(v) : v)).slice(0, 300); } catch (e) { return String(value); }
    };
    for (const name of Object.getOwnPropertyNames(SubtleCryptoShim.prototype)) {
      const original = SubtleCryptoShim.prototype[name];
      if (name === "constructor" || typeof original !== "function") continue;
      SUBTLE[name] = function (...args) {
        note("log", "subtle " + name + " " + args.map(show).join(" | "));
        return original.apply(this, args);
      };
    }
  }

  function jarFor(host) {
    if (!M.jar.has(host)) M.jar.set(host, new Map());
    return M.jar.get(host);
  }
  const hostMatches = (host, key) => {
    const domain = String(key).replace(/^\./, "").toLowerCase();
    const target = String(host).toLowerCase();
    return target === domain || target.endsWith("." + domain);
  };
  const nowSeconds = () => Math.floor(Date.now() / 1000);
  // A cookie the page stored: its value and the lifetime the site declared for it.
  const live = (entry) => entry.expires === null || entry.expires > nowSeconds();
  const cookieHeader = (host) => {
    const parts = [];
    for (const [key, jar] of M.jar) {
      if (!hostMatches(host, key)) continue;
      for (const [name, entry] of jar) if (live(entry)) parts.push(`${name}=${entry.value}`);
    }
    return parts.join("; ");
  };
  // One Set-Cookie / document.cookie string, with the lifetime it declares.
  function parseSetCookie(raw) {
    const parts = String(raw).split(";");
    const pair = parts[0];
    const i = pair.indexOf("=");
    if (i <= 0) return null;
    const cookie = { name: pair.slice(0, i).trim(), value: pair.slice(i + 1).trim(), maxAge: null, expires: null, path: "/", secure: false, httpOnly: false, attrs: parts.slice(1) };
    for (const attribute of cookie.attrs) {
      const j = attribute.indexOf("=");
      const key = (j < 0 ? attribute : attribute.slice(0, j)).trim().toLowerCase();
      const value = j < 0 ? "" : attribute.slice(j + 1).trim();
      if (key === "max-age") { const n = parseInt(value, 10); if (!isNaN(n)) cookie.maxAge = n; }
      else if (key === "expires") { const t = Date.parse(value); if (!isNaN(t)) cookie.expires = Math.floor(t / 1000); }
      else if (key === "path" && value) cookie.path = value;
      else if (key === "secure") cookie.secure = true;
      else if (key === "httponly") cookie.httpOnly = true;
    }
    return cookie;
  }
  // Store or delete one parsed cookie the way a browser's jar does. Max-Age wins
  // over Expires; a non-positive lifetime deletes.
  function storeCookie(key, cookie) {
    const expires = cookie.maxAge !== null ? nowSeconds() + cookie.maxAge : cookie.expires;
    if (cookie.value === "" || (expires !== null && expires <= nowSeconds())) {
      jarFor(key).delete(cookie.name);
      note("log", "cookie delete " + cookie.name + " @" + key);
      return;
    }
    jarFor(key).set(cookie.name, { value: cookie.value, expires, path: cookie.path, secure: cookie.secure, httpOnly: cookie.httpOnly });
    note("log", "cookie " + cookie.name + " len " + cookie.value.length + " @" + key + " " + (expires === null ? "session" : "ttl " + (expires - nowSeconds()) + "s") + (cookie.secure ? " secure" : "") + (cookie.httpOnly ? " httponly" : ""));
  }
  const cookieKey = (host, attributes) => {
    // A Domain attribute decides which host the cookie belongs to; without it,
    // the cookie is host-only, exactly as a browser treats it.
    let key = host;
    for (const attribute of attributes) {
      const [name, value] = String(attribute).split("=");
      if (name.trim().toLowerCase() === "domain" && value) {
        key = value.trim().toLowerCase();
        if (!key.startsWith(".")) key = "." + key;
      }
    }
    // RFC 6265 5.3 steps 5-6: a Domain must cover the host and not be a public
    // suffix (unless it is the host itself); otherwise the browser drops it.
    if (key !== host) {
      const bare = key.replace(/^\./, "");
      if (!hostMatches(host, key)) return null;
      if (bare !== host && (!bare.includes(".") || TWO_LABEL_SUFFIXES.has(bare))) return null;
    }
    return key;
  };
  function absorb(host, headers) {
    for (const [name, value] of headers) {
      if (String(name).toLowerCase() !== "set-cookie") continue;
      const cookie = parseSetCookie(value);
      const key = cookie ? cookieKey(host, cookie.attrs) : null;
      if (key) storeCookie(key, cookie);
      else if (cookie) note("log", "cookie rejected " + cookie.name + " for " + host);
    }
  }

  // ---------- network: the fetcher's own client, through one op ----------
  // eTLD+1, mirrored from the Rust side (`http::registrable`).
  const TWO_LABEL_SUFFIXES = new Set(["co.uk", "org.uk", "ac.uk", "gov.uk", "com.au", "net.au", "org.au", "co.jp", "ne.jp", "or.jp", "co.nz", "co.in", "co.kr", "com.br", "com.cn", "com.mx", "com.tr", "com.sg", "com.hk", "com.tw", "co.za", "com.ar"]);
  function registrable(host) {
    const labels = String(host).toLowerCase().replace(/\.$/, "").split(".");
    if (labels.length <= 2 || /^[\d.]+$/.test(host)) return labels.join(".");
    const keep = TWO_LABEL_SUFFIXES.has(labels.slice(-2).join(".")) ? 3 : 2;
    return labels.slice(-keep).join(".");
  }
  function fetchSite(page, target) {
    if (page.origin === target.origin) return "same-origin";
    if (page.protocol === target.protocol && registrable(page.hostname) === registrable(target.hostname)) return "same-site";
    return "cross-site";
  }
  // The referrer under Chrome's default policy, strict-origin-when-cross-origin.
  function referrerFor(page, target) {
    if (page.protocol === "https:" && target.protocol !== "https:") return null;
    if (page.origin === target.origin) return page.origin + page.pathname + page.search;
    return page.origin + "/";
  }
  // [name, value] pairs a browser adds to a subresource request.
  function fetchMetadata(page, target, method, dest, mode, credentials) {
    const out = [];
    const site = fetchSite(page, target);
    const referrer = referrerFor(page, target);
    // Origin: on every request with a body, and on a cross-origin CORS request.
    if ((method !== "GET" && method !== "HEAD") || (mode === "cors" && site !== "same-origin")) out.push(["origin", page.origin]);
    out.push(["sec-fetch-site", site], ["sec-fetch-mode", mode], ["sec-fetch-dest", dest]);
    // A cross-site request that carries credentials says whether storage access
    // is active; with third-party cookies allowed (Chrome's default) it is.
    if (site === "cross-site" && credentials === "include") out.push(["sec-fetch-storage-access", "active"]);
    if (referrer) out.push(["referer", referrer]);
    return out;
  }
  function netFetch(url, opts) {
    opts = opts || {};
    // `fetch` accepts strings, URL objects and Requests; take the address from any.
    const input = url && url.url ? url.url : (url && url.href ? url.href : url);
    // Everything the page asks for is resolved against the page, as a browser does.
    let absolute = String(input);
    try { absolute = new URL(absolute, M.page.url).href; } catch (e) { /* keep as given */ }
    // Headers arrive as a plain object, a Headers instance or [[name, value]] pairs.
    const headers = {};
    const given = opts.headers || (url && url.headers) || {};
    if (given._m instanceof Map) for (const [k, v] of given._m) headers[k] = v;
    else if (Array.isArray(given)) for (const [k, v] of given) headers[String(k)] = String(v);
    else for (const k of Object.keys(given)) headers[k] = String(given[k]);
    const has = (name) => Object.keys(headers).some((key) => key.toLowerCase() === name);
    const method = String(opts.method || (url && url.method) || "GET").toUpperCase();
    // The fetch metadata a browser attaches to this kind of request (Fetch spec;
    // header set measured from the reference browser): the vendor reads it.
    const dest = opts.__dest || "empty";
    const mode = String(opts.mode || (url && url.mode) || (dest === "script" ? "no-cors" : "cors"));
    const credentials = String(opts.credentials || (url && url.credentials) || "same-origin");
    let sameOrigin = true;
    try {
      const pageUrl = new URL(M.page.url);
      const target = new URL(absolute);
      sameOrigin = target.origin === pageUrl.origin;
      const meta = fetchMetadata(pageUrl, target, method, dest, mode, credentials);
      for (const [name, value] of meta) if (!has(name)) headers[name] = value;
    } catch (e) { /* non-absolute URL: leave headers as they are */ }
    if (!has("accept")) headers["accept"] = "*/*";
    // The request's cache mode (Fetch, "HTTP-network-or-cache fetch"): no-store
    // and reload ask the network for a fresh copy, no-cache revalidates.
    const cache = String(opts.cache || (url && url.cache) || "default");
    if ((cache === "no-store" || cache === "reload") && !has("pragma")) headers["pragma"] = "no-cache";
    if ((cache === "no-store" || cache === "reload") && !has("cache-control")) headers["cache-control"] = "no-cache";
    if (cache === "no-cache" && !has("cache-control")) headers["cache-control"] = "max-age=0";
    const encoded = encodeBody(opts.body !== undefined ? opts.body : (url && url.body));
    if (encoded.type) {
      // A multipart body owns its content type (the boundary is in it); for the
      // rest the page's own header wins, as in a browser.
      if (encoded.type.startsWith("multipart/")) {
        for (const key of Object.keys(headers)) if (/^content-type$/i.test(key)) delete headers[key];
        headers["content-type"] = encoded.type;
      } else if (!has("content-type")) {
        headers["content-type"] = encoded.type;
      }
    }
    const bodyBytes = encoded.bytes || new Uint8Array(0);
    if (method === "POST") note("log", "post " + absolute.slice(0, 130) + " " + bodyBytes.length + "B headers " + JSON.stringify(headers) + " :: " + core.decode(bodyBytes.subarray(0, DEBUG ? 40000 : 2400)));
    else if (DEBUG) note("log", "request " + method + " " + absolute.slice(0, 130) + " headers " + JSON.stringify(headers));
    for (const key of Object.keys(headers)) {
      if (/^x-aws-waf-token$/i.test(key)) note("log", "token header " + key + " len " + String(headers[key]).length);
    }
    let targetHost = "";
    try { targetHost = new URL(absolute).hostname; } catch (e) {}
    // Cookies follow the request's credentials mode; "-" tells the op to send none.
    const sendCookies = credentials === "include" || (credentials === "same-origin" && sameOrigin) || mode === "no-cors" || mode === "navigate";
    const jarCookie = sendCookies ? cookieHeader(targetHost) : "-";
    if (DEBUG) note("log", "cookies " + targetHost + " " + (sendCookies ? String(jarCookie).split(";").map((c) => { const at = c.indexOf("="); const v = c.slice(at + 1); return c.slice(0, at).trim() + ":" + v.length + ":" + v.slice(37, 53); }).join(",") : "omitted"));
    // The preload pass fetched the document's own script/style set while V8 was
    // booting, so a script waits (here) only for its own bytes; a URL the
    // scanner never took answers instantly and goes live. One spend per URL.
    let res;
    let prefetched = null;
    if (M.prefetch && method === "GET" && bodyBytes.length === 0) {
      const raw = ops.op_prefetch_wait(absolute, 1500);
      if (raw) prefetched = JSON.parse(raw);
    }
    if (prefetched) {
      if (prefetched.error) throw new TypeError("Failed to fetch (" + prefetched.error + ")");
      res = { status: prefetched.status, url: prefetched.url || absolute, headers: prefetched.headers || [], body: prefetched.body || "" };
      if (prefetched.body_b64 !== undefined) res.body_b64 = prefetched.body_b64;
    } else {
      const raw = ops.op_fetch(method, absolute, JSON.stringify(Object.entries(headers)), bodyBytes, jarCookie || (sendCookies ? "" : "-"), "subresource");
      res = JSON.parse(raw);
    }
    // The response's own bytes, when they are not text: a binary payload must
    // reach the page unchanged.
    const responseBytes = () => (res.body_b64 !== undefined ? base64ToBytes(res.body_b64) : core.encode(res.body || ""));
    note("log", "net " + method + " " + String(absolute).slice(0, 100) + " -> " + (res.error || res.status));
    // The content lane's wire tap: the requests an app's own program makes are
    // what M3 mines request contracts from. Inert in a mint (no transcript).
    if (M.transcript) {
      const text = String(res.body || "");
      let kept = null;
      if (/^\s*[\[{]/.test(text) && text.length <= 262144 && M.transcriptBytes + text.length <= 1048576) {
        kept = text; M.transcriptBytes += text.length;
      }
      // The request side too: the mined contract replays what the app sent,
      // not a reconstruction. A body is kept only inside the tap's own budget.
      let keptRequest = null;
      if (bodyBytes.length && bodyBytes.length <= 65536 && M.transcriptRequestBytes + bodyBytes.length <= 524288) {
        keptRequest = bytesToBase64(bodyBytes);
        M.transcriptRequestBytes += bodyBytes.length;
      }
      M.transcript.push({
        method, url: String(absolute), status: res.status, bytes: text.length, kind: dest, body: kept,
        reqHeaders: Object.entries(headers), reqBody: keptRequest,
      });
    }
    if (String(absolute).includes("/inputs")) note("log", "inputs body " + String(res.body || "").slice(0, 1200));
    if (res.error) throw new TypeError("Failed to fetch (" + res.error + ")");
    if (method === "POST") note("log", "post body: " + String(res.body || "").slice(0, 120));
    // A Set-Cookie on any response is state the challenge may need next.
    try { absorb(new URL(absolute).hostname, res.headers); } catch (e) { note("log", "absorb: " + (e && e.message)); }
    // Set-Cookie is never visible to a page's script in a browser.
    const visible = res.headers.filter(([k]) => !/^set-cookie2?$/i.test(k));
    const headerView = {
      get: (n) => {
        const hits = visible.filter(([k]) => k.toLowerCase() === String(n).toLowerCase());
        return hits.length ? hits.map(([, v]) => v).join(", ") : null;
      },
      has: (n) => visible.some(([k]) => k.toLowerCase() === String(n).toLowerCase()),
      forEach: (fn) => { for (const [k, v] of visible) fn(v, k.toLowerCase()); },
      entries: () => visible.map(([k, v]) => [k.toLowerCase(), v])[Symbol.iterator](),
      [Symbol.iterator]: () => visible.map(([k, v]) => [k.toLowerCase(), v])[Symbol.iterator](),
    };
    const response = {
      ok: res.status >= 200 && res.status < 300,
      status: res.status,
      statusText: "",
      url: res.url,
      redirected: res.url !== absolute,
      type: "basic",
      headers: headerView,
      text: () => Promise.resolve(res.body),
      textSync: res.body,
      bytesSync: responseBytes,
      rawHeaders: visible,
      json: () => Promise.resolve().then(() => JSON.parse(res.body)),
      arrayBuffer: () => Promise.resolve(responseBytes().slice().buffer),
      blob: () => Promise.resolve(new BlobShim([responseBytes()], { type: headerView.get("content-type") || "" })),
      clone() { return response; },
    };
    return response;
  }
  // A thenable that resolves synchronously: the challenge's .then() chains run
  // without needing an event loop, and the fetch is already done.
  const thenable = (value) => ({ then: (resolve) => { try { return resolve(value); } catch (e) { throw e; } }, catch: () => thenable(value) });

  // ---------- canvas / webgl constants ----------
  class Ctx2D {
    constructor(el) { this.canvas = el; this._font = "10px sans-serif"; this._fill = "#000000"; }
    fillText() {} strokeText() {}
    fillRect() {} strokeRect() {} clearRect() {}
    beginPath() {} closePath() {} moveTo() {} lineTo() {} quadraticCurveTo() {} arc() {} rect() {} fill() {} stroke() {} clip() {}
    isPointInPath() { note("surface", "ctx2d.isPointInPath"); return false; }
    save() {} restore() {} translate() {} rotate() {} scale() {} setTransform() {} resetTransform() {}
    measureText(t) { return { width: [...String(t)].length * 7.31, actualBoundingBoxAscent: 8.8, actualBoundingBoxDescent: 2.1, fontBoundingBoxAscent: 11, fontBoundingBoxDescent: 3 }; }
    createLinearGradient() { return { addColorStop() {} }; }
    createRadialGradient() { return { addColorStop() {} }; }
    createPattern() { return {}; }
    getImageData(x, y, w, h) {
      const d = new Uint8ClampedArray(w * h * 4);
      for (let i = 0; i < d.length; i++) d[i] = (i * 37 + 11) & 255;
      return { data: d, width: w, height: h };
    }
    putImageData() {}
    get font() { return this._font; } set font(v) { this._font = v; }
    get fillStyle() { return this._fill; } set fillStyle(v) { this._fill = v; }
    get strokeStyle() { return "#000"; } set strokeStyle(v) {}
    get textBaseline() { return "alphabetic"; } set textBaseline(v) {}
    get textAlign() { return "start"; } set textAlign(v) {}
    get globalAlpha() { return 1; } set globalAlpha(v) {}
    get globalCompositeOperation() { return "source-over"; } set globalCompositeOperation(v) {}
    get lineWidth() { return 1; } set lineWidth(v) {}
    get shadowBlur() { return 0; } set shadowBlur(v) {}
    get shadowColor() { return "rgba(0, 0, 0, 0)"; } set shadowColor(v) {}
    get filter() { return "none"; } set filter(v) {}
  }
  // The minting identity as its scripts see it (Identity::script_view): set per
  // mint, read by the surfaces below so the page's view never contradicts the wire.
  let IDENTITY = null;
  // V5_ENGINE_LOG: the host also logs what the page encrypts (its signals).
  let DEBUG = false;

  // Media answers follow Chrome's rules, measured against Chrome 156 on Linux:
  // a known container alone is "maybe" (a few audio types "probably"); with
  // codecs, "probably" when every codec is one that container plays, else "".
  // AWS WAF compares canPlayType with MediaSource.isTypeSupported, so the two
  // derive from one table.
  const MEDIA_CONTAINERS = {
    "audio/mpeg": "probably", "audio/mp3": "probably", "audio/aac": "probably", "audio/flac": "probably",
    "audio/mp4": "maybe", "audio/x-m4a": "maybe", "audio/ogg": "maybe", "audio/wav": "maybe", "audio/x-wav": "maybe",
    "audio/webm": "maybe", "video/mp4": "maybe", "video/webm": "maybe", "video/ogg": "maybe", "video/x-matroska": "maybe",
    "application/vnd.apple.mpegurl": "maybe", "application/x-mpegurl": "maybe",
  };
  const MEDIA_CODECS = {
    mp4: /^(avc[13]\.[0-9a-f]{6}|hev1\..+|hvc1\..+|av01\..+|vp09\..+|mp4a\.(40\.(2|5|29)|69|6b)|opus|flac|mp3)$/i,
    webm: /^(vp8(\.0)?|vp9(\.0)?|vp09\..+|av01\..+|vorbis|opus)$/i,
    ogg: /^(vorbis|opus|flac)$/i,
    wav: /^1$/,
  };
  function mediaType(type) {
    const [head, ...params] = String(type).split(";");
    const mime = head.trim().toLowerCase();
    const codecParam = params.map((p) => p.trim()).find((p) => /^codecs=/i.test(p));
    const codecs = codecParam ? codecParam.slice(7).replace(/^"|"$/g, "").split(",").map((c) => c.trim()).filter(Boolean) : null;
    const family = (mime.split("/")[1] || "").replace(/^x-/, "").replace("m4a", "mp4");
    return { mime, codecs, family };
  }
  function canPlayType(type) {
    note("surface", "media.canPlayType");
    const { mime, codecs, family } = mediaType(type);
    const base = MEDIA_CONTAINERS[mime];
    if (!base) return "";
    if (!codecs) return base;
    const rule = MEDIA_CODECS[family];
    return rule && codecs.length && codecs.every((codec) => rule.test(codec)) ? "probably" : "";
  }
  function isTypeSupported(type) {
    note("surface", "MediaSource.isTypeSupported");
    const { mime, codecs, family } = mediaType(type);
    if (!codecs) return mime === "audio/mpeg" || mime === "audio/aac";
    return (family === "mp4" || family === "webm") && /^(audio|video)\//.test(mime) && canPlayType(type) === "probably";
  }
  class MediaSourceShim { static isTypeSupported(type) { return isTypeSupported(type); } }

  // An element's style declaration knows the properties AWS WAF's capability
  // collector probes, as Chrome's does; anything a page sets is kept.
  const STYLE_PROPERTIES = ["textShadow", "WebkitTextStroke", "boxShadow", "borderRadius", "borderImage", "opacity", "transform", "transition", "display", "position", "visibility"];
  function makeStyle() {
    const style = { cssText: "", length: 0 };
    for (const name of STYLE_PROPERTIES) style[name] = "";
    style.setProperty = function (name, value) { this[name] = String(value); };
    style.getPropertyValue = function (name) { return this[name] || ""; };
    style.removeProperty = function (name) { const old = this[name] || ""; this[name] = ""; return old; };
    return style;
  }

  // Chrome's WebGL 1 extension list and limits on ANGLE (measured, Chrome 156).
  const WEBGL_EXTENSIONS = ["ANGLE_instanced_arrays", "EXT_blend_minmax", "EXT_clip_control", "EXT_color_buffer_half_float", "EXT_depth_clamp",
    "EXT_disjoint_timer_query", "EXT_float_blend", "EXT_frag_depth", "EXT_polygon_offset_clamp", "EXT_texture_compression_bptc",
    "EXT_texture_compression_rgtc", "EXT_texture_filter_anisotropic", "EXT_texture_mirror_clamp_to_edge", "EXT_sRGB", "KHR_parallel_shader_compile",
    "OES_element_index_uint", "OES_fbo_render_mipmap", "OES_standard_derivatives", "OES_texture_float", "OES_texture_float_linear",
    "OES_texture_half_float", "OES_texture_half_float_linear", "OES_vertex_array_object", "WEBGL_blend_func_extended", "WEBGL_color_buffer_float",
    "WEBGL_compressed_texture_astc", "WEBGL_compressed_texture_etc", "WEBGL_compressed_texture_etc1", "WEBGL_compressed_texture_s3tc",
    "WEBGL_compressed_texture_s3tc_srgb", "WEBGL_debug_renderer_info", "WEBGL_debug_shaders", "WEBGL_depth_texture", "WEBGL_draw_buffers",
    "WEBGL_lose_context", "WEBGL_multi_draw"];
  class GL {
    getParameter(p) {
      const gpu = (IDENTITY && IDENTITY.webgl) || {};
      const map = {
        7936: "WebKit", 7937: "WebKit WebGL", 7938: "WebGL 1.0 (OpenGL ES 2.0 Chromium)",
        35724: "WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)",
        37445: gpu.unmaskedVendor || "Google Inc. (Intel)",
        37446: gpu.unmaskedRenderer || "ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6)",
        3379: 16384, 34921: 16, 35660: 32, 35661: 64, 36347: 1024, 36348: 32, 36349: 1024, 34076: 16384, 34024: 16384,
        34930: 32, 3410: 8, 3411: 8, 3412: 8, 3413: 8, 3414: 24, 3415: 0,
        3386: new Int32Array([16384, 16384]), 33902: new Float32Array([1, 7.375]), 33901: new Float32Array([1, 255]),
      };
      note("surface", "webgl.getParameter");
      return map[p] !== undefined ? map[p] : null;
    }
    getExtension(n) {
      if (n === "WEBGL_debug_renderer_info") return { UNMASKED_VENDOR_WEBGL: 37445, UNMASKED_RENDERER_WEBGL: 37446 };
      if (n === "WEBGL_lose_context") return { loseContext() {}, restoreContext() {} };
      return WEBGL_EXTENSIONS.includes(n) ? {} : null;
    }
    getSupportedExtensions() { note("surface", "webgl.getSupportedExtensions"); return WEBGL_EXTENSIONS.slice(); }
    getContextAttributes() { return { alpha: true, antialias: true, depth: true, desynchronized: false, failIfMajorPerformanceCaveat: false, powerPreference: "default", premultipliedAlpha: true, preserveDrawingBuffer: false, stencil: false, xrCompatible: false }; }
    getError() { return 0; }
    getShaderPrecisionFormat(shader, type) { return type >= 36339 ? { precision: 0, rangeMin: 31, rangeMax: 30 } : { precision: 23, rangeMin: 127, rangeMax: 127 }; }
    createBuffer() { return {}; } bindBuffer() {} bufferData() {} enable() {} disable() {} clear() {}
    clearColor() {} viewport() {} drawArrays() {} drawElements() {} useProgram() {} createProgram() { return {}; }
    createShader() { return {}; } shaderSource() {} compileShader() {} attachShader() {} linkProgram() {}
    getProgramParameter() { return true; } getShaderParameter() { return true; } getAttribLocation() { return 0; }
    getUniformLocation() { return {}; } uniform1f() {} uniform2f() {}
    readPixels(x, y, w, h, format, type, pixels) {
      if (pixels && pixels.length) for (let i = 0; i < pixels.length; i++) pixels[i] = (i * 29 + 7) & 255;
    }
  }
  // The WebGL enums a collector names instead of hard-coding (gl.VENDOR, ...).
  Object.assign(GL.prototype, {
    VENDOR: 7936, RENDERER: 7937, VERSION: 7938, SHADING_LANGUAGE_VERSION: 35724, MAX_TEXTURE_SIZE: 3379,
    MAX_VIEWPORT_DIMS: 3386, MAX_RENDERBUFFER_SIZE: 34024, MAX_CUBE_MAP_TEXTURE_SIZE: 34076, MAX_VERTEX_ATTRIBS: 34921,
    MAX_VERTEX_UNIFORM_VECTORS: 36347, MAX_VARYING_VECTORS: 36348, MAX_FRAGMENT_UNIFORM_VECTORS: 36349,
    MAX_TEXTURE_IMAGE_UNITS: 34930, MAX_VERTEX_TEXTURE_IMAGE_UNITS: 35660, MAX_COMBINED_TEXTURE_IMAGE_UNITS: 35661,
    ALIASED_LINE_WIDTH_RANGE: 33902, ALIASED_POINT_SIZE_RANGE: 33901, RED_BITS: 3410, GREEN_BITS: 3411, BLUE_BITS: 3412,
    ALPHA_BITS: 3413, DEPTH_BITS: 3414, STENCIL_BITS: 3415, VERTEX_SHADER: 35633, FRAGMENT_SHADER: 35632,
    LOW_FLOAT: 36336, MEDIUM_FLOAT: 36337, HIGH_FLOAT: 36338, LOW_INT: 36339, MEDIUM_INT: 36340, HIGH_INT: 36341,
    ARRAY_BUFFER: 34962, STATIC_DRAW: 35044, FLOAT: 5126, TRIANGLE_STRIP: 5, COLOR_BUFFER_BIT: 16384,
    COMPILE_STATUS: 35713, LINK_STATUS: 35714, NO_ERROR: 0, drawingBufferWidth: 300, drawingBufferHeight: 150,
  });
  function makeCanvas() {
    const c = {
      width: 300, height: 150, style: {}, nodeName: "CANVAS", tagName: "CANVAS",
      getContext(kind) {
        if (kind === "2d") return new Ctx2D(c);
        if (kind === "webgl" || kind === "experimental-webgl") return new GL();
        return null;
      },
      toDataURL() {
        // A plausible rendered canvas: a provider that hashes real pixels sees a
        // full-size image, not a 100-byte placeholder.
        let body = "iVBORw0KGgoAAAANSUhEUgAAABQAAAAUCAYAAACNiR0NAAAAh0lEQVR42u3TsQ3AIAwEwI+0";
        const chunk = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODk=";
        while (body.length < 24000) body += chunk;
        return "data:image/png;base64," + body;
      },
      toBlob(cb) { cb(null); },
      addEventListener() {},
      getBoundingClientRect() { return { x: 0, y: 0, width: c.width, height: c.height, top: 0, left: 0, right: c.width, bottom: c.height }; },
    };
    return c;
  }

  // navigator.plugins and mimeTypes as every desktop browser lists them since
  // 2023: five PDF viewer entries sharing two MIME types; mobile lists none.
  // AWS WAF's plugin collectors read name, description and each MIME type.
  class PluginShim { get [Symbol.toStringTag]() { return "Plugin"; } item(i) { return this[i] || null; } namedItem(n) { for (let i = 0; i < this.length; i++) if (this[i].type === n) return this[i]; return null; } }
  class MimeTypeShim { get [Symbol.toStringTag]() { return "MimeType"; } }
  class PluginArrayShim { get [Symbol.toStringTag]() { return "PluginArray"; } item(i) { return this[i] || null; } namedItem(n) { for (let i = 0; i < this.length; i++) if (this[i].name === n) return this[i]; return null; } refresh() {} *[Symbol.iterator]() { for (let i = 0; i < this.length; i++) yield this[i]; } }
  class MimeTypeArrayShim { get [Symbol.toStringTag]() { return "MimeTypeArray"; } item(i) { return this[i] || null; } namedItem(n) { for (let i = 0; i < this.length; i++) if (this[i].type === n) return this[i]; return null; } *[Symbol.iterator]() { for (let i = 0; i < this.length; i++) yield this[i]; } }
  function pdfPlugins(enabled) {
    const plugins = new PluginArrayShim(), mimeTypes = new MimeTypeArrayShim();
    const names = enabled ? ["PDF Viewer", "Chrome PDF Viewer", "Chromium PDF Viewer", "Microsoft Edge PDF Viewer", "WebKit built-in PDF"] : [];
    const types = enabled ? ["application/pdf", "text/pdf"] : [];
    names.forEach((name, index) => {
      const plugin = Object.assign(new PluginShim(), { name, filename: "internal-pdf-viewer", description: "Portable Document Format", length: types.length });
      types.forEach((type, at) => { plugin[at] = Object.assign(new MimeTypeShim(), { type, suffixes: "pdf", description: "Portable Document Format", enabledPlugin: plugin }); });
      plugins[index] = plugin;
    });
    plugins.length = names.length;
    types.forEach((type, at) => { mimeTypes[at] = plugins[0][at]; });
    mimeTypes.length = types.length;
    return { plugins, mimeTypes };
  }

  // ---------- DOM ----------
  function makeEnvironment(page) {
    const host = new URL(page.url).hostname;
    const ident = IDENTITY = page.identity;
    DEBUG = !!page.debug;
    if (DEBUG) traceSubtle();
    const pdf = pdfPlugins(ident.pdfViewer);
    const listeners = new Map();
    const timers = [];
    let timerId = 1;
    // performance.now() as Chrome reports it: milliseconds since navigation
    // start, coarsened to 100 us (Chrome's resolution outside cross-origin
    // isolation). Navigation started when the fetcher asked for the document;
    // the page's scripts run that long after it.
    const navigationMs = Math.max(0, Number(page.navigationMs) || 0);
    const now = () => Math.floor((ops.op_now() + navigationMs) * 10) / 10;
    const timeOrigin = Math.floor((Date.now() - (ops.op_now() + navigationMs)) * 10) / 10;

    class El {
      constructor(tag) {
        this.tagName = String(tag).toUpperCase(); this.nodeName = this.tagName;
        this.style = makeStyle(); this.dataset = {}; this.childNodes = []; this.children = [];
        if (this.tagName === "AUDIO" || this.tagName === "VIDEO") this.canPlayType = canPlayType;
        this.attributes = {}; this.parentNode = null; this.textContent = ""; this.innerHTML = "";
        this.id = ""; this.className = ""; this.nonce = ""; this.src = ""; this.type = "";
        this.async = true; this.defer = false; this.onload = null; this.onerror = null;
        this.width = 300; this.height = 150; this.clientWidth = 300; this.clientHeight = 150;
        this.offsetWidth = 300; this.offsetHeight = 150; this.offsetTop = 0; this.offsetLeft = 0;
        this.classList = { add() {}, remove() {}, toggle() {}, contains() { return false; }, replace() {} };
      }
      prepend(...nodes) { for (const node of nodes) this.appendChild(node); }
      append(...nodes) { for (const node of nodes) this.appendChild(node); }
      insertAdjacentHTML() {} insertAdjacentElement() { return null; } insertAdjacentText() {}
      remove() {} replaceChildren() {} closest() { return null; } matches() { return false; }
      contains() { return false; } getClientRects() { return []; } scrollIntoView() {}
      setAttribute(k, v) { this.attributes[k] = String(v); }
      getAttribute(k) { return this.attributes[k] ?? null; }
      removeAttribute(k) { delete this.attributes[k]; }
      hasAttribute(k) { return k in this.attributes; }
      appendChild(c) {
        this.childNodes.push(c);
        if (c && !c.parentNode) c.parentNode = this;
        if (c && c.tagName === "SCRIPT") M.loader(c);
        return c;
      }
      insertBefore(c) { return this.appendChild(c); }
      removeChild(c) { this.childNodes = this.childNodes.filter((x) => x !== c); return c; }
      replaceChild(c) { return c; }
      addEventListener(t, f) { (listeners.get(this) || listeners.set(this, new Map()).get(this)).set(t, f); }
      removeEventListener() {}
      dispatchEvent() { return true; }
      getBoundingClientRect() { return { x: 0, y: 0, width: 300, height: 150, top: 0, left: 0, right: 300, bottom: 150 }; }
      getContext() { return null; }
      focus() {} blur() {} click() {}
      cloneNode() { return new El(this.tagName.toLowerCase()); }
      querySelector() { return null; } querySelectorAll() { return []; } getElementsByTagName() { return []; }
    }

    // The page's final step is a navigation carrying the challenge token; the
    // host records it so the fetcher's retry lands where the browser would.
    let href = page.url;
    const parsedUrl = new URL(page.url);
    const location = {
      get href() { return href; },
      set href(value) { href = String(value); M.redirect = href; note("log", "navigate " + href.slice(0, 140)); },
      origin: parsedUrl.origin, hostname: host, host,
      protocol: parsedUrl.protocol, pathname: parsedUrl.pathname,
      search: parsedUrl.search, hash: parsedUrl.hash,
      reload() { M.redirect = href; note("log", "reload"); },
      replace(value) { this.href = value; },
      assign(value) { this.href = value; },
    };
    const byId = new Map();
    const elementFor = (id) => {
      const key = String(id);
      if (!byId.has(key)) { const el = new El("div"); el.id = key; byId.set(key, el); }
      return byId.get(key);
    };
    const document = {
      nodeType: 9, title: "", referrer: page.referrer || "", readyState: "loading",
      visibilityState: "visible", hidden: false, hasFocus: () => true, activeElement: null,
      documentElement: Object.assign(new El("html"), { clientWidth: 1920, clientHeight: 1080 }),
      body: Object.assign(new El("body"), { clientWidth: 1920, clientHeight: 1080 }),
      head: new El("head"),
      currentScript: null,
      get cookie() { return cookieHeader(host); },
      set cookie(v) {
        const cookie = parseSetCookie(v);
        const key = cookie ? cookieKey(host, cookie.attrs) : null;
        if (key) storeCookie(key, cookie);
        else if (cookie) note("log", "cookie rejected " + cookie.name + " for " + host);
      },
      createElement(tag) {
        const t = String(tag).toLowerCase();
        if (t === "canvas") { const c = makeCanvas(); return Object.assign(new El("canvas"), c); }
        return new El(t);
      },
      createTextNode: (t) => ({ textContent: t }),
      createDocumentFragment: () => new El("#fragment"),
      getElementById: (id) => elementFor(id),
      querySelector(selector) {
        const text = String(selector);
        if (text.startsWith("#")) return elementFor(text.slice(1));
        return null;
      },
      querySelectorAll(selector) {
        const text = String(selector);
        if (text === "script") return scriptEls;
        const one = this.querySelector(selector);
        return one ? [one] : [];
      },
      getElementsByTagName(tag) {
        const name = String(tag).toLowerCase();
        if (name === "script") return scriptEls;
        if (name === "head") return [document.head];
        if (name === "body") return [document.body];
        return [];
      },
      getElementsByClassName: () => [], elementFromPoint: () => null,
      addEventListener(t, f) { (listeners.get(document) || listeners.set(document, new Map()).get(document)).set(t, f); },
      removeEventListener() {}, dispatchEvent() { return true; },
      fonts: { check: () => true, ready: Promise.resolve(), add() {}, load: () => Promise.resolve([]) },
      location,
      createEvent: () => ({ initEvent() {}, initCustomEvent() {} }),
      visibilityState: "visible", hidden: false, hasFocus: () => true, readyState: "loading",
    };

    // The page's own script tags exist in the document; a challenge may find its
    // base URL or its worker source through them.
    const scriptEls = [];
    {
      const re = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
      let match;
      while ((match = re.exec(page.html))) {
        const el = new El("script");
        const src = /src\s*=\s*"([^"]*)"/i.exec(match[1]);
        if (src) el.src = src[1];
        const nonce = /nonce\s*=\s*"([^"]*)"/i.exec(match[1]);
        if (nonce) el.nonce = nonce[1];
        el.textContent = match[2];
        // The tag is in the document, and `document.currentScript.parentNode` is
        // how an inline integration (GTM, a WAF bootstrap) finds where to put
        // the element it owns. The host has one head node; the script hangs
        // there, as a page's script tag hangs off its parent element. Set the
        // link directly: `appendChild` would run the script a second time.
        el.parentNode = document.head;
        document.head.childNodes.push(el);
        scriptEls.push(el);
      }
      document.scripts = scriptEls;
    }

    // A form's controls, read live: namedItem must see a field the page appends
    // while onsubmit runs, which is how these gates fold the query string in.
    function formControls(form) {
      const controls = () => form.childNodes.filter((child) => child && child.name);
      return {
        get length() { return controls().length; },
        item(index) { return controls()[index] || null; },
        namedItem(name) { return controls().find((control) => control.name === String(name)) || null; },
      };
    }

    // The navigation a browser makes on submit: a GET form replaces the action's
    // query with its controls, encoded as a browser encodes them. A method this
    // host does not run says so instead of sending a request the page did not
    // ask for.
    function submitForm(form) {
      const event = { type: "submit", target: form, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
      if (typeof form.onsubmit === "function") {
        let allowed;
        try { allowed = form.onsubmit.call(form, event); }
        catch (e) { note("log", "onsubmit: " + (e && e.message)); return; }
        if (allowed === false || event.defaultPrevented) return;
      }
      if (form.method !== "get") { note("log", "form submit: method " + form.method + " is not run"); return; }
      const pairs = [];
      for (const control of form.childNodes) {
        if (!control || !control.name) continue;
        if (["submit", "button", "image", "reset", "file"].includes(String(control.type))) continue;
        if (control.disabled) continue;
        pairs.push(formEncode(control.name) + "=" + formEncode(String(control.value)));
      }
      let target;
      try { target = new URL(form.action || M.page.url, M.page.url); }
      catch (e) { note("log", "form action: " + (e && e.message)); return; }
      if (pairs.length) target.search = pairs.join("&");
      else { target.search = ""; }
      M.redirect = target.href;
      note("surface", "form submit");
      note("log", "form submit " + M.redirect.slice(0, 140));
    }

    // The page's own <form> elements, with the fields the HTML declares. A JS
    // challenge may compute one field's value and submit the form as its last
    // step (Reddit's gate does exactly that): the host builds the navigation a
    // browser would make and the fetcher performs it. Only fields the page
    // ships or appends are sent, in document order; nothing is invented.
    const formEls = [];
    {
      const formRe = /<form\b([^>]*)>([\s\S]*?)<\/form>/gi;
      let formMatch;
      while ((formMatch = formRe.exec(page.html))) {
        const attrs = formMatch[1] || "";
        const form = new El("form");
        form.method = ((/method\s*=\s*["']?([a-zA-Z]+)/i.exec(attrs) || [])[1] || "get").toLowerCase();
        form.action = (/action\s*=\s*["']([^"']*)["']/i.exec(attrs) || [])[1] || "";
        form.onsubmit = null;
        const fieldRe = /<input\b([^>]*)>/gi;
        let fieldMatch;
        while ((fieldMatch = fieldRe.exec(formMatch[2] || ""))) {
          const fieldAttrs = fieldMatch[1] || "";
          const name = (/name\s*=\s*["']([^"']*)["']/i.exec(fieldAttrs) || [])[1];
          if (!name) continue;
          const field = new El("input");
          field.name = name;
          field.value = (/value\s*=\s*["']([^"']*)["']/i.exec(fieldAttrs) || [])[1] || "";
          field.type = ((/type\s*=\s*["']([^"']*)["']/i.exec(fieldAttrs) || [])[1] || "text").toLowerCase();
          form.appendChild(field);
        }
        form.elements = formControls(form);
        form.submit = () => submitForm(form);
        form.requestSubmit = () => submitForm(form);
        formEls.push(form);
      }
      document.forms = formEls;
    }

    const navigator = {
      userAgent: ident.userAgent, appVersion: ident.appVersion, appName: "Netscape",
      appCodeName: "Mozilla", platform: ident.platform, vendor: ident.vendor, vendorSub: "",
      product: "Gecko", productSub: "20030107", language: ident.languages[0], languages: ident.languages.slice(),
      hardwareConcurrency: ident.hardwareConcurrency, deviceMemory: 8, maxTouchPoints: ident.maxTouchPoints, cookieEnabled: true,
      geolocation: { getCurrentPosition() {}, watchPosition() { return 1; }, clearWatch() {} },
      sendBeacon: (u, d) => { try { netFetch(u, { method: "POST", body: d, mode: "no-cors", credentials: "include" }); } catch (e) {} return true; },
      onLine: true, webdriver: false, doNotTrack: null,
      plugins: pdf.plugins,
      mimeTypes: pdf.mimeTypes,
      userAgentData: {
        brands: ident.brands.map((b) => ({ brand: b.brand, version: b.version })),
        mobile: ident.mobile, platform: ident.uaPlatform,
        getHighEntropyValues(hints) {
          // Values follow the user agent's own platform and major version.
          const major = (/Chrome\/(\d+)/.exec(ident.userAgent) || [])[1] || "0";
          const full = major + ".0.0.0";
          const arm = ident.mobile || /Mac/.test(ident.uaPlatform);
          const all = {
            architecture: arm ? "arm" : "x86", bitness: "64", model: "", mobile: ident.mobile, platform: ident.uaPlatform,
            platformVersion: ({ Windows: "15.0.0", macOS: "15.5.0", Android: "14.0.0", Linux: "6.8.0" })[ident.uaPlatform] || "",
            uaFullVersion: full, wow64: false, formFactors: [ident.mobile ? "Mobile" : "Desktop"],
            fullVersionList: ident.brands.map((b) => ({ brand: b.brand, version: /^\d+$/.test(b.version) && b.version === major ? full : b.version + ".0.0.0" })),
          };
          const out = { brands: this.brands, mobile: ident.mobile, platform: ident.uaPlatform };
          for (const hint of Array.from(hints || [])) if (hint in all) out[hint] = all[hint];
          return Promise.resolve(out);
        },
        toJSON() { return { brands: this.brands, mobile: this.mobile, platform: this.platform }; },
      },
      connection: { effectiveType: "4g", rtt: 50, downlink: 10, saveData: false },
      permissions: { query: (desc) => Promise.resolve({ state: "prompt", name: desc && desc.name, onchange: null }) },
      getBattery: () => Promise.resolve({ charging: true, chargingTime: 0, dischargingTime: Infinity, level: 1, onchargingchange: null, onlevelchange: null, addEventListener() {}, removeEventListener() {} }),
      mediaDevices: { enumerateDevices: () => Promise.resolve([]), getUserMedia: () => Promise.reject(new Error("NotAllowedError")) },
      storage: { estimate: () => Promise.resolve({ quota: 300000000000, usage: 0 }), persisted: () => Promise.resolve(false) },
      pdfViewerEnabled: ident.pdfViewer,
      webkitTemporaryStorage: { queryUsageAndQuota: (callback) => callback(0, 300000000000) },
    };
    const screen = Object.assign({ colorDepth: 24, pixelDepth: 24, availLeft: 0, availTop: 0 }, ident.screen, { orientation: { type: ident.mobile ? "portrait-primary" : "landscape-primary", angle: 0 } });
    const storage = () => { const m = new Map(); return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k), clear: () => m.clear(), key: (i) => [...m.keys()][i] ?? null, get length() { return m.size; } }; };

    // Instrumented: the signal collector reads these immediately before it
    // builds its worker. A getter tells us whether that collector ever runs.
    Object.defineProperty(navigator, "hardwareConcurrency", { configurable: true, get() { note("log", "nav.hardwareConcurrency"); return ident.hardwareConcurrency; } });
    Object.defineProperty(navigator, "platform", { configurable: true, get() { note("log", "nav.platform"); return ident.platform; } });

    const win = {
      document, navigator, screen, location,
      innerWidth: 1920, innerHeight: 1080, outerWidth: 1920, outerHeight: 1080, devicePixelRatio: 1,
      scrollX: 0, scrollY: 0, pageXOffset: 0, pageYOffset: 0, screenX: 0, screenY: 0, screenLeft: 0, screenTop: 0,
      name: "", closed: false, opener: null, isSecureContext: true, crossOriginIsolated: false,
      origin: new URL(page.url).origin,
      localStorage: storage(), sessionStorage: storage(),
      history: { length: 2, state: null, pushState() {}, replaceState() {}, back() {}, forward() {}, go() {} },
      crypto: {
        // WebCrypto: an integer typed array of at most 65536 bytes, filled whole.
        getRandomValues(a) {
          note("surface", "crypto.getRandomValues");
          const integer = a instanceof Int8Array || a instanceof Uint8Array || a instanceof Uint8ClampedArray || a instanceof Int16Array || a instanceof Uint16Array || a instanceof Int32Array || a instanceof Uint32Array || a instanceof BigInt64Array || a instanceof BigUint64Array;
          if (!integer) throw new DOMExceptionShim("The provided ArrayBufferView is of type '" + (a && a.constructor && a.constructor.name) + "', which is not an integer array type.", "TypeMismatchError");
          if (a.byteLength > 65536) throw new DOMExceptionShim("The ArrayBufferView's byte length (" + a.byteLength + ") exceeds the number of bytes of entropy available via this API (65536).", "QuotaExceededError");
          const hex = ops.op_random(a.byteLength);
          const bytes = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
          for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
          return a;
        },
        // RFC 9562 version 4: version nibble 4, variant bits 10.
        randomUUID() {
          const h = ops.op_random(16);
          const variant = ((parseInt(h[16], 16) & 0x3) | 0x8).toString(16);
          return h.slice(0, 8) + "-" + h.slice(8, 12) + "-4" + h.slice(13, 16) + "-" + variant + h.slice(17, 20) + "-" + h.slice(20, 32);
        },
        subtle: SUBTLE,
      },
      performance: { now: () => now(), timeOrigin, getEntriesByType: (t) => (t === "navigation" ? [{ type: "navigate", name: page.url, entryType: "navigation", startTime: 0, duration: 0, responseEnd: navigationMs }] : []), getEntriesByName: () => [], mark() {}, measure() {}, clearMarks() {}, clearMeasures() {} },
      setTimeout: (fn, ms) => { const id = timerId++; timers.push({ id, at: ops.op_now() + (ms || 0), fn, args: Array.prototype.slice.call(arguments, 2) }); return id; },
      clearTimeout: (id) => { const t = timers.find((x) => x.id === id); if (t) t.cancelled = true; },
      setInterval: (fn, ms) => { const id = timerId++; timers.push({ id, every: Math.max(ms || 0, 1), at: ops.op_now() + (ms || 0), fn, args: [], repeat: true }); return id; },
      clearInterval: (id) => { const t = timers.find((x) => x.id === id); if (t) t.cancelled = true; },
      queueMicrotask: (fn) => { try { fn(); } catch {} },
      requestAnimationFrame: (cb) => win.setTimeout(() => cb(now()), 16),
      cancelAnimationFrame: (id) => win.clearTimeout(id),
      requestIdleCallback: (cb) => win.setTimeout(() => cb({ didTimeout: false, timeRemaining: () => 50 }), 1),
      cancelIdleCallback: (id) => win.clearTimeout(id),
      atob, btoa,
      fetch: (u, o) => thenable(netFetch(u, o)),
      Headers: class {
        constructor(init) {
          this._m = new Map();
          if (init && init._m instanceof Map) { for (const [k, v] of init._m) this._m.set(k, v); }
          else if (Array.isArray(init)) { for (const [k, v] of init) this._m.set(String(k).toLowerCase(), String(v)); }
          else if (init) { for (const k of Object.keys(init)) this._m.set(String(k).toLowerCase(), String(init[k])); }
        }
        get(k) { const key = String(k).toLowerCase(); return this._m.has(key) ? this._m.get(key) : null; }
        set(k, v) { this._m.set(String(k).toLowerCase(), String(v)); }
        has(k) { return this._m.has(String(k).toLowerCase()); }
        delete(k) { this._m.delete(String(k).toLowerCase()); }
        append(k, v) { this._m.set(String(k).toLowerCase(), String(v)); }
        entries() { return this._m.entries(); }
        keys() { return this._m.keys(); }
        values() { return this._m.values(); }
        forEach(fn) { for (const [k, v] of this._m) fn(v, k, this); }
        [Symbol.iterator]() { return this._m.entries(); }
      },
      Request: class {
        constructor(input, init) {
          init = init || {};
          this.url = input && input.url ? input.url : String(input);
          this.method = String(init.method || "GET").toUpperCase();
          this.headers = new (this.constructor === win.Request ? win.Headers : Object)() ;
          this.headers = new win.Headers(init.headers || (input && input.headers) || {});
          this.body = init.body != null ? init.body : (input && input.body);
          this.credentials = init.credentials || "same-origin";
          this.mode = init.mode || "cors";
          this.signal = init.signal || null;
        }
        clone() { return new win.Request(this.url, { method: this.method, headers: this.headers, body: this.body }); }
        text() { return Promise.resolve(String(this.body == null ? "" : this.body)); }
        json() { return Promise.resolve(JSON.parse(String(this.body == null ? "null" : this.body))); }
      },
      Response: class {
        constructor(body, init) {
          init = init || {};
          this.body = body == null ? "" : body;
          this.status = init.status || 200;
          this.ok = this.status >= 200 && this.status < 300;
          this.statusText = init.statusText || "";
          this.headers = new win.Headers(init.headers || {});
          this.url = init.url || "";
          this.type = "basic";
        }
        text() { return Promise.resolve(String(this.body)); }
        json() { return Promise.resolve(JSON.parse(String(this.body || "null"))); }
        arrayBuffer() { return Promise.resolve(new TextEncoder().encode(String(this.body)).buffer); }
        clone() { return new win.Response(this.body, { status: this.status, headers: this.headers, url: this.url }); }
      },
      AbortSignal: class {
        constructor() { this.aborted = false; this.reason = undefined; this.onabort = null; this._listeners = []; }
        addEventListener(kind, handler) { if (kind === "abort") this._listeners.push(handler); }
        removeEventListener() {}
        dispatchEvent() { return true; }
      },
      AbortController: class {
        constructor() { this.signal = new win.AbortSignal(); }
        abort(reason) {
          this.signal.aborted = true; this.signal.reason = reason;
          if (this.signal.onabort) { try { this.signal.onabort({ type: "abort" }); } catch (e) {} }
          for (const handler of this.signal._listeners) { try { handler({ type: "abort" }); } catch (e) {} }
        }
      },
      File: class extends BlobShim {
        constructor(parts, name, options) { super(parts, options); this.name = String(name); this.lastModified = Date.now(); }
      },
      ImageData: class {
        constructor(a, b, c) {
          if (typeof a === "number") { this.width = a; this.height = b; this.data = c || new Uint8ClampedArray(a * b * 4); }
          else { this.data = a; this.width = b; this.height = c; }
        }
      },
      MessageChannel: class {
        constructor() {
          const channel = this;
          this.port1 = { onmessage: null, postMessage: (message) => { const handler = channel.port2.onmessage; if (handler) handler({ data: message }); }, start() {}, close() {}, addEventListener(kind, handler) { if (kind === "message") this.onmessage = handler; } };
          this.port2 = { onmessage: null, postMessage: (message) => { const handler = channel.port1.onmessage; if (handler) handler({ data: message }); }, start() {}, close() {}, addEventListener(kind, handler) { if (kind === "message") this.onmessage = handler; } };
        }
      },
      structuredClone: (value) => { try { return JSON.parse(JSON.stringify(value)); } catch (e) { return value; } },
      PerformanceObserver: class { observe() {} disconnect() {} takeRecords() { return []; } },
      // Constructors a collector may use for `instanceof` or to build a probe.
      CanvasRenderingContext2D: class {}, WebGLRenderingContext: class {}, WebGL2RenderingContext: class {},
      Path2D: class { addPath() {} moveTo() {} lineTo() {} arc() {} rect() {} },
      ImageBitmap: class { constructor() { this.width = 1; this.height = 1; } close() {} },
      OffscreenCanvas: class {
        constructor(width, height) { this.width = width; this.height = height; }
        getContext(kind) { return kind === "2d" ? new Ctx2D(this) : new GL(); }
        convertToBlob() { return Promise.resolve(new Blob([])); }
        transferToImageBitmap() { return new win.ImageBitmap(); }
      },
      createImageBitmap: () => Promise.resolve({ width: 1, height: 1, close() {} }),
      FontFace: class { constructor(family, source) { this.family = family; this.status = "loaded"; } load() { return Promise.resolve(this); } },
      Screen: class {}, Navigator: class {}, Document: class {}, Window: class {},
      HTMLCanvasElement: class {}, HTMLElement: class {}, Element: class {}, HTMLImageElement: class {},
      Image: class { constructor() { this.width = 0; this.height = 0; } set src(v) {} },
      XMLHttpRequest: class {
        constructor() { this.readyState = 0; this.status = 0; this.responseText = ""; this._h = {}; }
        open(m, u) { this._m = m; this._u = u; this.readyState = 1; }
        setRequestHeader(k, v) { this._h[k] = v; }
        addEventListener(t, f) { this["on" + t] = f; }
        send(b) {
          try {
            const r = netFetch(this._u, { method: this._m, headers: this._h, body: b, credentials: this.withCredentials ? "include" : "same-origin" });
            this.status = r.status; this.responseText = r.textSync; this._headers = r.rawHeaders; this.responseURL = r.url; this.readyState = 4;
            if (this.responseType === "json") {
              try { this.response = JSON.parse(this.responseText); } catch (e) { this.response = null; }
            } else if (this.responseType === "arraybuffer") {
              this.response = r.bytesSync().slice().buffer;
            } else if (this.responseType === "blob") {
              this.response = new BlobShim([r.bytesSync()], { type: r.headers.get("content-type") || "" });
            } else {
              this.response = this.responseText;
            }
            if (this.onreadystatechange) this.onreadystatechange();
            if (this.onload) this.onload();
          } catch (e) { this.readyState = 4; this.status = 0; if (this.onerror) this.onerror(); }
        }
        abort() {}
        getResponseHeader(name) {
          const header = (this._headers || []).find(([key]) => String(key).toLowerCase() === String(name).toLowerCase());
          return header ? header[1] : null;
        }
        getAllResponseHeaders() { return (this._headers || []).map(([k, v]) => k + ": " + v).join("\r\n"); }
      },
      addEventListener(t, f) { (listeners.get(win) || listeners.set(win, new Map()).get(win)).set(t, f); },
      removeEventListener() {}, dispatchEvent() { return true; }, postMessage() {},
      matchMedia: () => ({ matches: false, media: "", addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {} }),
      getComputedStyle: () => ({ getPropertyValue: () => "", fontFamily: "Arial", length: 0 }),
      Image: class { constructor() { this.width = 0; this.height = 0; } set src(v) {} },
      Audio: class { constructor() { this.duration = 0; } play() { return Promise.resolve(); } pause() {} canPlayType(type) { return canPlayType(type); } },
      MediaSource: MediaSourceShim,
      AudioContext: class {
        constructor() { this.sampleRate = 48000; this.state = "running"; this.destination = {}; }
        createOscillator() { return { type: "sine", frequency: { value: 440, setValueAtTime() {} }, connect() {}, start() {}, stop() {}, onended: null }; }
        createAnalyser() { return { fftSize: 2048, frequencyBinCount: 1024, connect() {}, getFloatFrequencyData(a) { a.fill(-100); }, getByteFrequencyData(a) { a.fill(0); } }; }
        createGain() { return { gain: { value: 1, setValueAtTime() {} }, connect() {} }; }
        createDynamicsCompressor() { return { threshold: { value: -24 }, knee: { value: 30 }, ratio: { value: 12 }, attack: { value: 0.003 }, release: { value: 0.25 }, connect() {} }; }
        createBufferSource() { return { buffer: null, connect() {}, start() {} }; }
        createBiquadFilter() { return { type: "lowpass", frequency: { value: 350 }, connect() {} }; }
        createBuffer(c, l, r) { return { length: l, sampleRate: r, numberOfChannels: c, getChannelData: () => new Float32Array(l) }; }
        close() { return Promise.resolve(); }
      },
      OfflineAudioContext: class {
        constructor(c, l, r) { this.sampleRate = r; this.length = l; }
        createOscillator() { return { frequency: { value: 440 }, connect() {}, start() {} }; }
        createGain() { return { gain: { value: 1 }, connect() {} }; }
        createDynamicsCompressor() { return { connect() {} }; }
        startRendering() { return Promise.resolve({ getChannelData: () => new Float32Array(1024), length: 1024, sampleRate: 44100 }); }
      },
      RTCPeerConnection: class { constructor() {} createDataChannel() { return {}; } createOffer() { return Promise.resolve({ type: "offer", sdp: "v=0" }); } setLocalDescription() { return Promise.resolve(); } close() {} },
      Notification: { permission: "default", requestPermission: () => Promise.resolve("default") },
      WebSocket: class { constructor() { note("surface", "WebSocket"); } send() {} close() {} addEventListener() {} },
      MutationObserver: class { observe() {} disconnect() {} takeRecords() { return []; } },
      IntersectionObserver: class { observe() {} disconnect() {} },
      ResizeObserver: class { observe() {} disconnect() {} },
      console: { log: (...a) => note("log", a.join(" ")), warn: (...a) => note("log", a.join(" ")), error: (...a) => note("log", a.join(" ")), info: (...a) => note("log", a.join(" ")), debug() {}, trace() {} },
      chrome: {
        app: { isInstalled: false, InstallState: { DISABLED: "disabled" }, RunningState: { CANNOT_RUN: "cannot_run" } },
        runtime: {},
        loadTimes: () => ({ requestTime: 0, startLoadTime: 0, commitLoadTime: 0, finishDocumentLoadTime: 0, finishLoadTime: 0, firstPaintTime: 0, firstPaintAfterLoadTime: 0, navigationType: "Other", wasFetchedViaSpdy: true, wasNpnNegotiated: true, npnNegotiatedProtocol: "h2", wasAlternateProtocolAvailable: false, connectionInfo: "h2" }),
        csi: () => ({ onloadT: Date.now(), startE: Date.now() - 1200, pageT: 1200, tran: 15 }),
      },
    };
    win.window = win; win.self = win; win.globalThis = win; win.top = win; win.parent = win; win.frames = win;

    // Any host member a challenge reads that we don't provide becomes a harmless
    // function, and the name is logged. A surface is added only when a measured
    // challenge needs it.
    const stub = (obj, label) => new Proxy(obj, {
      get(t, k) {
        if (k in t) return t[k];
        if (typeof k === "symbol") return undefined;
        const id = label + "." + String(k);
        if (!M.surfaces.has(id)) { M.surfaces.add(id); note("log", "surface " + id); }
        return function () { return undefined; };
      },
      has() { return true; },
    });
    win.__stub = stub;
    return { win, document, listeners, timers, host, scriptEls };
  }

  function settle() {
    // V8 only drains promise callbacks at a checkpoint; the host drives it.
    try { core.runMicrotasks(); } catch (e) { note("log", "microtasks: " + (e && e.message)); }
  }
  // A classic script, compiled under its own URL (an inline script under the
  // page's): its top-level declarations join the shared global lexical scope and
  // its stack frames name the script, exactly as in a browser.
  function runScript(code, filename) {
    let name = String(filename || "");
    try { name = new URL(name || M.page.url, M.page.url).href; } catch (e) { name = M.page.url; }
    const outcome = core.evalContext(String(code), name);
    const error = outcome && outcome[1];
    if (error) {
      const thrown = error.thrown;
      note("log", "script error " + name.slice(0, 90) + ": " + (thrown && thrown.message ? thrown.message : String(thrown)));
    }
    settle();
  }

  function fire(env, type) {
    for (const target of [env.document, env.win]) {
      for (const key of [target]) {
        const map = env.listeners.get(key);
        const fn = map && map.get(type);
        if (fn) { try { fn({ type, target }); } catch (e) { note("log", "handler " + type + ": " + (e && e.message)); } }
      }
      const inline = target["on" + type];
      if (typeof inline === "function") { try { inline({ type, target }); } catch (e) { note("log", "inline " + type + ": " + (e && e.message)); } }
    }
  }

  function drain(env, limitMs) {
    const start = ops.op_now();
    for (let i = 0; i < 200; i++) {
      const now = ops.op_now();
      const live = env.timers.filter((t) => !t.cancelled);
      if (!live.length) break;
      live.sort((a, b) => a.at - b.at);
      const next = live[0];
      const wait = Math.max(0, next.at - now);
      if (ops.op_now() - start + wait > limitMs) break;
      if (wait > 0) ops.op_sleep(wait);
      const due = env.timers.filter((t) => !t.cancelled && t.at <= ops.op_now());
      for (const t of due) {
        if (t.repeat) t.at = ops.op_now() + t.every; else t.cancelled = true;
        try { t.fn.apply(null, t.args); } catch (e) { note("log", "timer: " + (e && (e.stack || e.message))); }
        settle();
      }
    }
  }

  // The page's bootstrap chooses between a full token and a light refresh. Wrap
  // the integration once it exists so the choice is visible in the log.
  function wrapIntegration() {
    note("log", "wrap sees AwsWafIntegration=" + (typeof globalThis.AwsWafIntegration));
    const integration = globalThis.AwsWafIntegration;
    if (!integration || integration.__v5wrapped) return;
    integration.__v5wrapped = true;
    note("log", "integration: " + Object.keys(integration).join(","));
    for (const name of ["saveReferrer", "checkForceRefresh", "getToken", "forceRefreshToken", "hasToken", "fetch"]) {
      const original = integration[name];
      if (typeof original !== "function") continue;
      integration[name] = function (...args) {
        let result;
        try { result = original.apply(this, args); }
        catch (e) { note("log", "integration " + name + " threw: " + (e && e.message)); throw e; }
        try {
          if (result && typeof result.then === "function") {
            result.then(
              (value) => note("log", "integration " + name + " -> " + String(value).slice(0, 60)),
              (error) => note("log", "integration " + name + " reject: " + (error && error.message)),
            );
          } else {
            note("log", "integration " + name + " -> " + String(result).slice(0, 60));
          }
        } catch (e) { /* logging only */ }
        return result;
      };
    }
  }


  // WebAssembly memory is mapped by V8 itself, outside its heap and outside the
  // ArrayBuffer allocator the Rust side meters, so the host layer keeps the budget:
  // every memory a page asks for, directly or through a module it instantiates, is
  // charged against one total. The browser surface (constructors, instanceof,
  // exports) is unchanged; only a request past the budget throws, as V8 does for
  // an allocation it cannot make.
  function guardWebAssembly(budgetBytes) {
    const W = globalThis.WebAssembly;
    if (!W || !W.Memory || !W.Instance || !W.Module) return;
    const PAGE = 65536;
    let used = 0;
    const charge = (bytes) => {
      const wanted = bytes > 0 ? bytes : 0;
      if (used + wanted > budgetBytes) throw new RangeError("WebAssembly: memory budget of " + (budgetBytes >> 20) + " MB exhausted");
      used += wanted;
    };
    // Named originals: a function expression's own name shadows an outer binding
    // of the same name inside its body, and the guards must reach the originals.
    const OriginalMemory = W.Memory, OriginalModule = W.Module, OriginalInstance = W.Instance;
    const compile = W.compile, instantiate = W.instantiate;
    const pagesOf = (descriptor) => Number(descriptor && (descriptor.initial !== undefined ? descriptor.initial : descriptor.minimum)) || 0;

    // The memory a module declares for itself, read from its binary's memory section.
    const declared = new WeakMap();
    function memoryPages(source) {
      try {
        const b = source instanceof ArrayBuffer ? new Uint8Array(source) : new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
        let i = 8, total = 0;
        const u32 = () => { let r = 0, s = 0, byte; do { byte = b[i++]; r += (byte & 0x7f) * Math.pow(2, s); s += 7; } while ((byte & 0x80) && s < 35); return r; };
        while (i < b.length) {
          const id = b[i++]; const size = u32(); const end = i + size;
          if (id === 5) { let n = u32(); while (n-- > 0) { const flags = u32(); total += u32(); if (flags & 1) u32(); } }
          i = end;
        }
        return total;
      } catch (e) { return 0; }
    }
    const construct = (Original, Guarded) => {
      Guarded.prototype = Original.prototype;
      Object.defineProperty(Original.prototype, "constructor", { value: Guarded, writable: true, configurable: true });
      for (const key of Object.getOwnPropertyNames(Original)) {
        if (["length", "name", "prototype"].includes(key)) continue;
        try { Guarded[key] = Original[key]; } catch (e) { /* static already present */ }
      }
      return Guarded;
    };
    const needNew = (target, name) => { if (!target) throw new TypeError("Constructor WebAssembly." + name + " requires 'new'"); };

    const GMemory = construct(OriginalMemory, function Memory(descriptor) {
      needNew(new.target, "Memory");
      charge(pagesOf(descriptor) * PAGE);
      return Reflect.construct(OriginalMemory, [descriptor]);
    });
    const originalGrow = OriginalMemory.prototype.grow;
    OriginalMemory.prototype.grow = function grow(delta) { charge(Number(delta) * PAGE); return originalGrow.call(this, delta); };

    const GModule = construct(OriginalModule, function Module(bytes) {
      needNew(new.target, "Module");
      const module = Reflect.construct(OriginalModule, [bytes]);
      declared.set(module, memoryPages(bytes));
      return module;
    });
    const GInstance = construct(OriginalInstance, function Instance(module, imports) {
      needNew(new.target, "Instance");
      charge((declared.get(module) || 0) * PAGE);
      return Reflect.construct(OriginalInstance, [module, imports]);
    });
    const defineStatic = (name, value) => Object.defineProperty(W, name, { value, writable: true, configurable: true, enumerable: false });
    defineStatic("Memory", GMemory);
    defineStatic("Module", GModule);
    defineStatic("Instance", GInstance);
    if (typeof compile === "function") {
      defineStatic("compile", function compile_(bytes) {
        try {
          const pages = memoryPages(bytes);
          return compile.call(W, bytes).then((module) => { declared.set(module, pages); return module; });
        } catch (e) { return Promise.reject(e); }
      });
    }
    if (typeof instantiate === "function") {
      defineStatic("instantiate", function instantiate_(source, imports) {
        try {
          if (source instanceof OriginalModule) {
            charge((declared.get(source) || 0) * PAGE);
            return instantiate.call(W, source, imports);
          }
          charge(memoryPages(source) * PAGE);
          return instantiate.call(W, source, imports).then((result) => { declared.set(result.module, memoryPages(source)); return result; });
        } catch (e) { return Promise.reject(e); }
      });
    }
  }

  // A classic <script src>: a no-cors GET with script fetch metadata.
  function loadScript(src) {
    try {
      const response = netFetch(src, { __dest: "script", mode: "no-cors", credentials: "include" });
      return { status: response.status, body: response.status >= 200 && response.status < 300 ? response.textSync : "", error: response.status >= 400 ? "status " + response.status : null };
    } catch (e) {
      return { error: String(e && e.message) };
    }
  }

  globalThis.__v5Mint = function () {
    try { delete globalThis.Deno; } catch (e) { /* checked below */ }
    if (typeof globalThis.Deno !== "undefined") throw new Error("engine: Deno is reachable from the page");
    const page = JSON.parse(ops.op_page());
    guardWebAssembly(page.wasmBudget || 128 * 1024 * 1024);
    M = { jar: new Map(), surfaces: new Set(), logs: [], loader: null, loaded: new Set(), started: ops.op_now(), page: JSON.parse(ops.op_page()) };
    // The fetcher's own cookies for the page host are the jar's starting state:
    // host-only, because the header that carried them names no domain.
    try {
      const pageHost = new URL(page.url).hostname;
      for (const pair of String(page.cookie || "").split(";")) {
        const at = pair.indexOf("=");
        if (at > 0) jarFor(pageHost).set(pair.slice(0, at).trim(), { value: pair.slice(at + 1).trim(), expires: null, path: "/", secure: false, httpOnly: false });
      }
    } catch (e) { note("log", "seed jar: " + (e && e.message)); }
    const env = makeEnvironment(page);
    M.doc = env.document;
    M.loader = (el) => {
      if (el.src) {
        if (M.loaded.has(el.src)) return;
        M.loaded.add(el.src);
        let host = "";
        try { host = new URL(el.src, M.page.url).hostname; } catch (e) {}
        const parsed = loadScript(el.src);
        if (!parsed.error) { M.doc.currentScript = el; runScript(parsed.body, el.src); M.doc.currentScript = null; }
        wrapIntegration();
        if (el.onload) { try { el.onload(); } catch {} }
      } else {
        runScript(el.textContent || "", M.page.url);
        wrapIntegration();
        if (el.onload) { try { el.onload(); } catch {} }
      }
    };
    M.loaded = new Set();

    const win = env.win;
    // Install the host on the real global so page scripts see it.
    for (const key of Object.keys(win)) {
      if (key === "globalThis") continue;
      globalThis[key] = win[key];
    }
    globalThis.window = globalThis;
    globalThis.self = globalThis;
    globalThis.top = globalThis;
    globalThis.parent = globalThis;
    globalThis.frames = globalThis;
    globalThis.document = win.document;
    globalThis.navigator = win.navigator;
    globalThis.location = win.location;
    globalThis.crypto = win.crypto;
    globalThis.performance = win.performance;
    globalThis.fetch = win.fetch;
    globalThis.XMLHttpRequest = win.XMLHttpRequest;
    globalThis.AudioContext = win.AudioContext;
    globalThis.OfflineAudioContext = win.OfflineAudioContext;
    globalThis.chrome = win.chrome;

    // The page the caller already fetched is the one to run: asking again inside
    // the same window adds a challenge attempt and can escalate the vendor.
    const html = page.html;

    note("log", "env Worker=" + (typeof globalThis.Worker) + " Blob=" + (typeof globalThis.Blob) + " fetch=" + (typeof globalThis.fetch) + " coURL=" + (typeof URL.createObjectURL) + " cib=" + (typeof globalThis.crypto) + "/" + (typeof (globalThis.crypto && globalThis.crypto.subtle && globalThis.crypto.subtle.importKey)));
    const beforeGlobals = new Set(Object.getOwnPropertyNames(globalThis));
    // Render-blocking stylesheets are requested before the scripts after them run,
    // as a browser's parser does; the body is not used (no layout).
    const linkRe = /<link\b([^>]*)>/gi;
    let link;
    while ((link = linkRe.exec(html))) {
      const attrs = link[1];
      if (!/\brel=["']?stylesheet\b/i.test(attrs)) continue;
      const href = (attrs.match(/href=["']([^"']+)["']/) || [])[1];
      if (!href) continue;
      try {
        const response = netFetch(href, { __dest: "style", mode: "no-cors", credentials: "include", headers: { accept: "text/css,*/*;q=0.1" } });
        note("log", "style " + href.slice(0, 70) + " -> " + response.status);
      } catch (e) { note("log", "style " + href.slice(0, 70) + ": " + (e && e.message)); }
    }
    const scripts = [];
    const re = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
    let m;
    while ((m = re.exec(html))) scripts.push({ attrs: m[1], body: m[2] });
    note("log", "page " + page.html.length + "B, scripts " + scripts.length);
    for (let index = 0; index < scripts.length; index++) {
      const s = scripts[index];
      const element = env.scriptEls ? env.scriptEls[index] : null;
      const src = (s.attrs.match(/src="([^"]+)"/) || [])[1];
      let absoluteSrc = src;
      try { absoluteSrc = new URL(src, M.page.url).href; } catch (e) { /* keep as given */ }
      if (src) {
        if (!M.loaded.has(absoluteSrc)) {
          M.loaded.add(absoluteSrc);
          const parsed = loadScript(absoluteSrc);
          note("log", "script " + absoluteSrc.slice(0, 70) + " -> " + (parsed.error || parsed.status) + " " + String(parsed.body || "").length);
          if (!parsed.error) {
            M.doc.currentScript = element;
            runScript(parsed.body, absoluteSrc);
            M.doc.currentScript = null;
            wrapIntegration();
          }
        }
      } else if (s.body.trim()) {
        // `document.currentScript` is part of the page's contract while a script runs.
        M.doc.currentScript = element;
        runScript(s.body, M.page.url);
        M.doc.currentScript = null;
        wrapIntegration();
      }
    }

    const addedGlobals = Object.getOwnPropertyNames(globalThis).filter((key) => !beforeGlobals.has(key));
    try {
      const cs = globalThis.ChallengeScript;
      if (cs) {
        note("log", "ChallengeScript: " + Object.getOwnPropertyNames(cs).map((k) => k + ":" + typeof cs[k]).join(","));
        for (const k of Object.getOwnPropertyNames(cs)) {
          const v = cs[k];
          if (v && typeof v === "object") note("log", "  " + k + " = " + Object.getOwnPropertyNames(v).slice(0, 30).join(","));
        }
      }
    } catch (e) { note("log", "ChallengeScript probe: " + (e && e.message)); }
    for (const name of ["a0_0x1972", "a0_0x1fad", "a0_0x38fe", "a0_0x250c"]) {
      const value = globalThis[name];
      if (value) note("log", "global " + name + ": " + typeof value + " keys " + (typeof value === "object" ? Object.getOwnPropertyNames(value).slice(0, 20).join(",") : String(value).slice(0, 80)));
    }
    note("log", "loaded " + M.loaded.size + ", aws " + (typeof globalThis.AwsWafIntegration) + ", new globals: " + addedGlobals.join(","));
    env.document.readyState = "interactive";
    fire(env, "readystatechange");
    fire(env, "DOMContentLoaded");
    env.document.readyState = "complete";
    fire(env, "load");
    fire(env, "pageshow");
    // Let every promise chain the page started reach its end, then run its timers.
    for (let round = 0; round < 12; round++) {
      settle();
      drain(env, 2000);
      settle();
    }

    // The fetcher spends the credential through its own client: measured equal
    // or better than spending here, and it keeps one retry policy in one place.
    note("log", "redirect " + (M.redirect || "none"));

    // Whatever the page's own code stored is the result.
    const cookies = [];
    for (const [h, jar] of M.jar) {
      if (!hostMatches(env.host, h)) continue;
      for (const [name, entry] of jar) if (live(entry)) cookies.push({ name, value: entry.value, domain: h, path: entry.path, secure: entry.secure, httpOnly: entry.httpOnly, expires: entry.expires });
    }
    ops.op_result(JSON.stringify({
      cookies,
      surfaces: [...M.surfaces],
      redirect: M.redirect || null,
      logs: M.logs.slice(0, DEBUG ? 5000 : 200),
      ms: ops.op_now() - M.started,
    }));
  };

  // ---------- the content lane ----------
  // A second mode of the same engine: run an application page's own program and
  // return the document it assembles. The host below is a data-plane DOM: real
  // nodes, real attributes, real text, live selectors. There is no cascade, no
  // layout and no raster; a program that needs pixels is classified by the
  // caller, never emulated here. Every byte still leaves through op_fetch on the
  // fetcher's own identity wire, in this process.
  //
  // The challenge host above is frozen: mint behavior is measured and must not
  // move. This section shares only module-scope plumbing (the jar, netFetch, the
  // URL/WebCrypto/bytes shims, notes, settle, runScript) and adds its own tree,
  // selector engine, observers and pump. A method the host does not implement is
  // logged on touch (the element prototype proxy below), never guessed at.

  const VOID_TAGS = new Set(["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"]);
  const RAW_TEXT_TAGS = new Set(["script", "style", "xmp", "iframe", "noembed", "noframes", "plaintext"]);
  const camelToKebab = (name) => String(name).replace(/[A-Z]/g, (c) => "-" + c.toLowerCase());
  const kebabToCamel = (name) => String(name).replace(/-([a-z])/g, (_m, c) => c.toUpperCase());
  const escapeText = (value) => String(value).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const escapeAttr = (value) => String(value).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

  // ----- serialization and visible text -----
  // Module scope: the pump and the content entry read these; the tree they walk
  // is whatever document is handed in.
  function serializeInto(node, write, depth) {
    if (depth > 500) return;
    if (node.nodeType === 3) {
      const parentTag = node.parentNode && node.parentNode.tagName ? node.parentNode.tagName.toLowerCase() : "";
      write(RAW_TEXT_TAGS.has(parentTag) ? node.nodeValue : escapeText(node.nodeValue));
      return;
    }
    if (node.nodeType !== 1) {
      for (const child of node.childNodes) serializeInto(child, write, depth + 1);
      return;
    }
    const tag = node.tagName.toLowerCase();
    write("<" + tag);
    for (const [name, value] of node._attrs) write(" " + name + (value === "" ? "" : "=\"" + escapeAttr(value) + "\""));
    write(">");
    if (VOID_TAGS.has(tag)) return;
    for (const child of node.childNodes) serializeInto(child, write, depth + 1);
    write("</" + tag + ">");
  }

  function visibleText(root) {
    const out = [];
    (function walk(node, depth) {
      if (depth > 500) return;
      for (const child of node.childNodes) {
        if (child.nodeType === 3) { out.push(child.nodeValue); continue; }
        if (child.nodeType !== 1) continue;
        const tag = child.tagName;
        if (tag === "SCRIPT" || tag === "STYLE" || tag === "NOSCRIPT" || tag === "TEMPLATE") continue;
        if (child._attrs.has("hidden")) continue;
        if (child._attrs.get("aria-hidden") === "true") continue;
        const display = child.style && String(child.style.display || "").toLowerCase();
        const visibility = child.style && String(child.style.visibility || "").toLowerCase();
        if (display === "none" || visibility === "hidden") continue;
        walk(child, depth + 1);
      }
    })(root, 0);
    return out.join(" ").replace(/[\s\u00a0]+/g, " ").trim();
  }

  // ----- selector matching over the live tree -----
  // The subset frameworks and app code actually query with: tag, class, id,
  // attributes (all operators, the `i` flag), the four combinators, :not (a
  // full list), structural pseudos (`:nth-child(an+b)`, `:first-child`,
  // `:last-child`, `:only-child`, `:empty`, `:root`), `:has`, and the state
  // pseudos (:hover, :focus, ...) that never match here as they never match an
  // unfocused, unhovered page. Pseudo-elements never match. Anything else is a
  // SyntaxError, as in Chrome.
  function selectorError(text) {
    const error = new Error("'" + String(text).slice(0, 80) + "' is not a valid selector");
    error.name = "SyntaxError";
    return error;
  }

  function parseSelector(text) {
    const input = String(text);
    let i = 0;
    const skip = () => { while (i < input.length && /\s/.test(input[i])) i++; };
    const ident = () => {
      let out = "";
      while (i < input.length) {
        const c = input[i];
        if (/[a-zA-Z0-9_-]/.test(c)) { out += c; i++; }
        else if (c === "\\" && i + 1 < input.length) { out += input[i + 1]; i += 2; }
        else break;
      }
      return out;
    };
    const attribute = () => {
      i++; // [
      skip();
      const name = ident();
      if (!name) throw selectorError(text);
      skip();
      let op = null;
      let value = "";
      const two = input.slice(i, i + 2);
      if (["~=", "|=", "^=", "$=", "*="].includes(two)) { op = two; i += 2; }
      else if (input[i] === "=") { op = "="; i += 1; }
      if (op) {
        skip();
        const quote = input[i];
        if (quote === '"' || quote === "'") {
          i++;
          while (i < input.length && input[i] !== quote) { value += input[i]; i++; }
          if (input[i] !== quote) throw selectorError(text);
          i++;
        } else {
          const bare = ident();
          if (!bare) throw selectorError(text);
          value = bare;
        }
        skip();
      }
      let ci = false;
      if (input[i] === "i" || input[i] === "I") { ci = true; i++; skip(); }
      if (input[i] !== "]") throw selectorError(text);
      i++;
      return { t: "attr", name, op, value, ci };
    };
    const compound = () => {
      const simples = [];
      for (;;) {
        const c = input[i];
        if (c === undefined) break;
        if (c === "*") { simples.push({ t: "any" }); i++; }
        else if (c === "#") { i++; const v = ident(); if (!v) throw selectorError(text); simples.push({ t: "id", v }); }
        else if (c === ".") { i++; const v = ident(); if (!v) throw selectorError(text); simples.push({ t: "class", v }); }
        else if (c === "[") simples.push(attribute());
        else if (c === ":") {
          i++;
          if (input[i] === ":") { i++; ident(); continue; }
          const name = ident().toLowerCase();
          if (!name) throw selectorError(text);
          let arg = null;
          if (input[i] === "(") {
            let depth = 1;
            let j = i + 1;
            let raw = "";
            while (j < input.length) {
              const ch = input[j];
              if (ch === "(") depth++;
              else if (ch === ")") { depth--; if (depth === 0) break; }
              raw += ch; j++;
            }
            if (depth !== 0) throw selectorError(text);
            i = j + 1;
            arg = raw;
          }
          simples.push({ t: "pseudo", name, arg });
        }
        else if (/[a-zA-Z_\\]/.test(c)) simples.push({ t: "tag", v: ident() });
        else break;
      }
      if (!simples.length) throw selectorError(text);
      return simples;
    };
    const complex = () => {
      const parts = [{ combinator: null, simples: compound() }];
      for (;;) {
        skip();
        if (i >= input.length || input[i] === "," || input[i] === ")") break;
        let combinator = " ";
        if (input[i] === ">" || input[i] === "+" || input[i] === "~") { combinator = input[i]; i++; skip(); }
        parts.push({ combinator, simples: compound() });
      }
      return parts;
    };
    const list = [];
    for (;;) {
      skip();
      if (i >= input.length) break;
      list.push(complex());
      skip();
      if (input[i] === ",") { i++; continue; }
      if (i < input.length) throw selectorError(text);
    }
    if (!list.length) throw selectorError(text);
    return list;
  }

  function allElements(root) {
    const out = [];
    (function walk(node) {
      for (const child of node.childNodes) {
        if (child.nodeType === 1) { out.push(child); walk(child); }
      }
    })(root);
    return out;
  }

  function elementSiblings(el) {
    return el.parentNode ? el.parentNode.childNodes.filter((n) => n.nodeType === 1) : [el];
  }

  function matchesPseudo(el, simple) {
    const name = simple.name;
    if (name === "not") {
      const inner = parseSelector(simple.arg || "");
      return !inner.some((parts) => matchesComplex(el, parts));
    }
    if (name === "is" || name === "where") {
      const inner = parseSelector(simple.arg || "");
      return inner.some((parts) => matchesComplex(el, parts));
    }
    if (name === "has") {
      const inner = parseSelector(simple.arg || "");
      const scope = allElements(el);
      return scope.some((candidate) => inner.some((parts) => matchesComplex(candidate, parts)));
    }
    if (name === "root") return el.ownerDocument && el.ownerDocument.documentElement === el;
    if (name === "first-child") return elementSiblings(el)[0] === el;
    if (name === "last-child") {
      const siblings = elementSiblings(el);
      return siblings[siblings.length - 1] === el;
    }
    if (name === "only-child") return elementSiblings(el).length === 1;
    if (name === "empty") {
      return !el.childNodes.some((n) => n.nodeType === 1 || (n.nodeType === 3 && n.nodeValue.length > 0));
    }
    if (name === "nth-child" || name === "nth-of-type") {
      const arg = String(simple.arg || "").replace(/\s+/g, "").toLowerCase();
      let siblings = elementSiblings(el);
      if (name === "nth-of-type") siblings = siblings.filter((n) => n.tagName === el.tagName);
      const index = siblings.indexOf(el) + 1;
      if (arg === "odd") return index % 2 === 1;
      if (arg === "even") return index % 2 === 0;
      const m = /^([+-]?\d*)n(?:([+-]?\d+))?$/.exec(arg);
      if (m) {
        const a = m[1] === "" || m[1] === "+" ? 1 : m[1] === "-" ? -1 : parseInt(m[1], 10);
        const b = m[2] ? parseInt(m[2], 10) : 0;
        if (a === 0) return index === b;
        const diff = index - b;
        return diff % a === 0 && diff / a >= 0;
      }
      const exact = parseInt(arg, 10);
      return !isNaN(exact) && index === exact;
    }
    // State pseudos on a page nobody interacts with: never matching is the
    // browser's own answer, minus the focus the page might give itself.
    return false;
  }

  function matchesSimple(el, simple) {
    switch (simple.t) {
      case "any": return true;
      case "tag": return el.tagName.toLowerCase() === simple.v.toLowerCase();
      case "id": return el.getAttribute("id") === simple.v;
      case "class": return el.classList.contains(simple.v);
      case "attr": {
        const value = el.getAttribute(simple.name);
        if (value === null) return false;
        if (!simple.op) return true;
        const actual = simple.ci ? value.toLowerCase() : value;
        const wanted = simple.ci ? simple.value.toLowerCase() : simple.value;
        switch (simple.op) {
          case "=": return actual === wanted;
          case "~=": return wanted !== "" && actual.split(/\s+/).includes(wanted);
          case "|=": return actual === wanted || actual.startsWith(wanted + "-");
          case "^=": return wanted !== "" && actual.startsWith(wanted);
          case "$=": return wanted !== "" && actual.endsWith(wanted);
          case "*=": return wanted !== "" && actual.includes(wanted);
        }
        return false;
      }
      case "pseudo": return matchesPseudo(el, simple);
    }
    return false;
  }

  function matchesComplex(el, parts) {
    const last = parts.length - 1;
    if (!parts[last].simples.every((simple) => matchesSimple(el, simple))) return false;
    let current = el;
    for (let index = last; index > 0; index--) {
      const combinator = parts[index].combinator;
      const target = parts[index - 1].simples;
      const compound = (node) => node && node.nodeType === 1 && target.every((simple) => matchesSimple(node, simple));
      if (combinator === ">") {
        if (!compound(current.parentNode)) return false;
        current = current.parentNode;
      } else if (combinator === "+") {
        const sibling = current.previousSibling;
        if (!compound(sibling)) return false;
        current = sibling;
      } else if (combinator === "~") {
        let sibling = current.previousSibling;
        let hit = null;
        while (sibling) { if (compound(sibling)) { hit = sibling; break; } sibling = sibling.previousSibling; }
        if (!hit) return false;
        current = hit;
      } else {
        let ancestor = current.parentNode;
        let hit = null;
        while (ancestor && ancestor.nodeType === 1) { if (compound(ancestor)) { hit = ancestor; break; } ancestor = ancestor.parentNode; }
        if (!hit) return false;
        current = hit;
      }
    }
    return true;
  }

  function collectSelect(doc, scope, selectorText) {
    const list = parseSelector(selectorText);
    const candidates = scope.nodeType === 9 ? allElements(doc) : allElements(scope);
    const out = [];
    for (const candidate of candidates) {
      if (list.some((parts) => matchesComplex(candidate, parts))) out.push(candidate);
    }
    return out;
  }

  // ----- module scripts (ESM) -----
  // The modern web's script class: Vite/Rollup/Next production bundles. A module
  // graph executes synchronously on the host's own wire: fetch, transform the
  // statement-level ESM surface (static imports, named/default/namespace
  // bindings, export maps, re-exports, export *, import.meta, dynamic import),
  // and evaluate dependencies first with the exports object registered before
  // evaluation (a cycle sees the partial namespace, as ESM does).
  //
  // Exports are live getters on the namespace; NAMED imports are snapshots of
  // the binding at import time (a module-level `let` reassigned after import is
  // not observed). Measured bundles export stable references; a bundle that
  // needs more is a logged finding, never guessed at. A construct the transform
  // cannot parse skips the whole module with a named log line: honest failure,
  // not a half-run program.
  const MODULES = new Map();
  let INLINE_MODULE = 0;
  const MODULE_ENV = { MODE: "production", PROD: true, DEV: false, BASE_URL: "/", SSR: false };

  function skipQuoted(source, start) {
    const n = source.length;
    const quote = source[start];
    let i = start + 1;
    while (i < n) {
      const c = source[i];
      if (c === "\\") { i += 2; continue; }
      if (c === quote) return i + 1;
      i++;
    }
    return n;
  }
  function skipLineComment(source, start) {
    let i = start + 2;
    while (i < source.length && source[i] !== "\n") i++;
    return i;
  }
  function skipBlockComment(source, start) {
    let i = start + 2;
    while (i < source.length && !(source[i] === "*" && source[i + 1] === "/")) i++;
    return Math.min(i + 2, source.length);
  }
  function skipTemplateLiteral(source, start) {
    const n = source.length;
    let i = start + 1;
    while (i < n) {
      const c = source[i];
      if (c === "\\") { i += 2; continue; }
      if (c === "`") return i + 1;
      if (c === "$" && source[i + 1] === "{") { i = skipBracedExpression(source, i + 2); continue; }
      i++;
    }
    return n;
  }
  function skipBracedExpression(source, start) {
    const n = source.length;
    let i = start;
    let depth = 0;
    while (i < n) {
      const c = source[i];
      if (c === '"' || c === "'") { i = skipQuoted(source, i); continue; }
      if (c === "`") { i = skipTemplateLiteral(source, i); continue; }
      if (c === "/" && source[i + 1] === "/") { i = skipLineComment(source, i); continue; }
      if (c === "/" && source[i + 1] === "*") { i = skipBlockComment(source, i); continue; }
      if (c === "{") { depth++; i++; continue; }
      if (c === "}") { if (depth === 0) return i + 1; depth--; i++; continue; }
      i++;
    }
    return n;
  }
  /// The ranges the statement scanner must never rewrite: strings, template
  /// literals, comments and regex literals. Whether a `/` opens a regex or
  /// divides is decided by the PREVIOUS SIGNIFICANT TOKEN, not by the previous
  /// character: char-level heuristics desync on minified bundles (one false
  /// range cascades and swallows real statements), and a regex literal may
  /// contain quotes that must not start a string. A regex that crosses a
  /// newline or runs past any real literal's size is treated as a division.
  const REGEX_AFTER_KEYWORDS = new Set(["return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else", "yield", "await"]);
  function regexCanStartAfter(token, value) {
    if (token === "ident" || token === "number" || token === "string" || token === "regex" || token === "template" || token === "close") return false;
    if (token === "keyword") return REGEX_AFTER_KEYWORDS.has(value);
    return true;
  }
  function skipRegexLiteral(source, start) {
    const n = source.length;
    const cap = Math.min(n, start + 8192);
    let i = start + 1;
    let inClass = false;
    while (i < cap) {
      const c = source[i];
      if (c === "\\") { i += 2; continue; }
      if (c === "\n") return start;
      if (c === "[") { inClass = true; i++; continue; }
      if (c === "]") { inClass = false; i++; continue; }
      if (c === "/" && !inClass) {
        i++;
        while (i < n && /[a-z]/i.test(source[i])) i++;
        return i;
      }
      i++;
    }
    return start;
  }
  function protectedRanges(source) {
    const ranges = [];
    const n = source.length;
    let i = 0;
    let token = "";
    let value = "";
    while (i < n) {
      const c = source[i];
      if (c === '"' || c === "'") { const end = skipQuoted(source, i); ranges.push([i, end]); i = end; token = "string"; value = ""; continue; }
      if (c === "`") { const end = skipTemplateLiteral(source, i); ranges.push([i, end]); i = end; token = "template"; value = ""; continue; }
      if (c === "/" && source[i + 1] === "/") { const end = skipLineComment(source, i); ranges.push([i, end]); i = end; continue; }
      if (c === "/" && source[i + 1] === "*") { const end = skipBlockComment(source, i); ranges.push([i, end]); i = end; continue; }
      if (c === "/" && regexCanStartAfter(token, value)) {
        const end = skipRegexLiteral(source, i);
        if (end > i) { ranges.push([i, end]); i = end; token = "regex"; value = ""; continue; }
      }
      if (/[A-Za-z_$]/.test(c)) {
        let j = i + 1;
        while (j < n && /[\w$]/.test(source[j])) j++;
        value = source.slice(i, j);
        token = REGEX_AFTER_KEYWORDS.has(value) ? "keyword" : "ident";
        i = j;
        continue;
      }
      if (/[0-9]/.test(c)) { let j = i + 1; while (j < n && /[\w.$]/.test(source[j])) j++; i = j; token = "number"; value = ""; continue; }
      if (/\s/.test(c)) { i++; continue; }
      if (c === ")" || c === "]" || c === "}") { token = "close"; value = c; i++; continue; }
      token = "punct"; value = c;
      i++;
    }
    return ranges;
  }
  /// The first `;` at bracket depth zero after `start`, skipping protected
  /// ranges. Statements in bundles are semicolon-terminated; a scan that reaches
  /// the end without one is a transform error, not a guess.
  function statementEnd(source, start, ranges) {
    const n = source.length;
    let i = start;
    let depth = 0;
    let rangeAt = 0;
    while (rangeAt < ranges.length && ranges[rangeAt][1] <= i) rangeAt++;
    while (i < n) {
      if (rangeAt < ranges.length && ranges[rangeAt][0] === i) { i = ranges[rangeAt][1]; rangeAt++; continue; }
      const c = source[i];
      if (c === "(" || c === "[" || c === "{") depth++;
      else if (c === ")" || c === "]" || c === "}") { if (depth > 0) depth--; }
      else if (c === ";" && depth === 0) return i + 1;
      i++;
    }
    return n;
  }
  /// The end of a brace-bodied declaration's statement: the matching `}` of the
  /// body at depth zero (plus an optional trailing `;`).
  function braceStatementEnd(source, start, ranges) {
    const n = source.length;
    let i = start;
    let rangeAt = 0;
    while (rangeAt < ranges.length && ranges[rangeAt][1] <= i) rangeAt++;
    // The declaration head runs to the first `{` at paren depth zero, so an
    // object literal in a default parameter is not mistaken for the body.
    let parens = 0;
    while (i < n) {
      if (rangeAt < ranges.length && ranges[rangeAt][0] === i) { i = ranges[rangeAt][1]; rangeAt++; continue; }
      const c = source[i];
      if (c === "(") { parens++; i++; continue; }
      if (c === ")") { if (parens > 0) parens--; i++; continue; }
      if (c === "{" && parens === 0) break;
      i++;
    }
    if (i >= n) return n;
    let depth = 0;
    while (i < n) {
      if (rangeAt < ranges.length && ranges[rangeAt][0] === i) { i = ranges[rangeAt][1]; rangeAt++; continue; }
      const c = source[i];
      if (c === "{") depth++;
      else if (c === "}") { depth--; if (depth === 0) { i++; break; } }
      i++;
    }
    while (i < n && (source[i] === ";" || /\s/.test(source[i]))) i++;
    return i;
  }
  const IDENTIFIER = "[A-Za-z_$][\\w$]*";
  const IDENTIFIER_RE = /^[A-Za-z_$][\w$]*$/;
  /// `{a, b as c}` -> [[a, a], [b, c]]. Throws on anything else: a name the
  /// transform cannot prove is left for the honest failure path.
  function parseNameList(text) {
    const out = [];
    for (const piece of String(text).split(",")) {
      const trimmed = piece.trim();
      if (!trimmed) continue;
      const parts = trimmed.split(/\s+as\s+/);
      if (parts.length === 1) {
        if (!IDENTIFIER_RE.test(trimmed)) throw new Error("unsupported export name: " + trimmed.slice(0, 40));
        out.push([trimmed, trimmed]);
      } else if (parts.length === 2) {
        const from = parts[0].trim();
        const to = parts[1].trim();
        if (!IDENTIFIER_RE.test(from) || !IDENTIFIER_RE.test(to)) throw new Error("unsupported export alias: " + trimmed.slice(0, 40));
        out.push([from, to]);
      } else throw new Error("unsupported export list: " + trimmed.slice(0, 40));
    }
    return out;
  }
  /// `import` clause forms bundles emit: default, * as ns, {a, b as c}, and the
  /// combinations of default with either.
  function parseImportClause(clause) {
    const result = { defaultName: null, namespace: null, names: [] };
    let text = clause.trim();
    if (!text) return result;
    const take = (pattern) => {
      const match = pattern.exec(text);
      if (!match) return null;
      text = text.slice(match[0].length).replace(/^\s*,\s*/, "");
      return match;
    };
    if (!text.startsWith("{") && !text.startsWith("*")) {
      const match = take(new RegExp("^(" + IDENTIFIER + ")"));
      if (!match) throw new Error("unsupported import clause: " + clause.slice(0, 60));
      result.defaultName = match[1];
    }
    if (text.startsWith("*")) {
      const match = take(/^\*\s+as\s+([A-Za-z_$][\w$]*)/);
      if (!match) throw new Error("unsupported namespace import: " + clause.slice(0, 60));
      result.namespace = match[1];
    } else if (text.startsWith("{")) {
      const close = text.indexOf("}");
      if (close < 0) throw new Error("unterminated import list");
      result.names.push(...parseNameList(text.slice(1, close)));
      text = text.slice(close + 1);
    }
    if (text.trim()) throw new Error("unsupported import clause tail: " + text.slice(0, 60));
    return result;
  }
  /// Rewrite one module source into the wrapper body the factory evaluates.
  function skipSpaces(source, i) {
    while (i < source.length && /\s/.test(source[i])) i++;
    return i;
  }
  function readWord(source, i) {
    let j = i;
    while (j < source.length && /[\w$]/.test(source[j])) j++;
    return { word: source.slice(i, j), end: j };
  }
  function readString(source, i) {
    if (source[i] !== '"' && source[i] !== "'") return null;
    const end = skipQuoted(source, i);
    return { text: source.slice(i + 1, end - 1), end };
  }
  /// The index after the `}` matching the `{` at `open`, skipping protected
  /// ranges; -1 when the brace never closes.
  function matchingBrace(source, open, ranges) {
    const n = source.length;
    let i = open;
    let depth = 0;
    let rangeAt = 0;
    while (rangeAt < ranges.length && ranges[rangeAt][1] <= i) rangeAt++;
    while (i < n) {
      if (rangeAt < ranges.length && ranges[rangeAt][0] === i) { i = ranges[rangeAt][1]; rangeAt++; continue; }
      const c = source[i];
      if (c === "{") depth++;
      else if (c === "}") { depth--; if (depth === 0) return i + 1; }
      i++;
    }
    return -1;
  }
  /// One `import` statement at `start`: `{clause, specifier, end}`, or null when
  /// the token is not a statement (`import.meta`, dynamic `import(`, or a shape
  /// this parser cannot prove).
  function parseImportAt(source, start, ranges) {
    let i = skipSpaces(source, start + 6);
    const sideEffect = readString(source, i);
    if (sideEffect) {
      const k = skipSpaces(source, sideEffect.end);
      if (source[k] !== ";") return null;
      return { clause: "", specifier: sideEffect.text, end: k + 1 };
    }
    const clauseStart = i;
    let k = i;
    // Optional default binding, then the comma before the next clause part.
    if (/[A-Za-z_$]/.test(source[k] || "")) {
      const binding = readWord(source, k);
      k = skipSpaces(source, binding.end);
      if (source[k] === ",") k = skipSpaces(source, k + 1);
    }
    if (source[k] === "*") {
      k = skipSpaces(source, k + 1);
      const asWord = readWord(source, k);
      if (asWord.word !== "as") return null;
      k = skipSpaces(source, asWord.end);
      const name = readWord(source, k);
      if (!IDENTIFIER_RE.test(name.word)) return null;
      k = skipSpaces(source, name.end);
    } else if (source[k] === "{") {
      const close = matchingBrace(source, k, ranges);
      if (close < 0) return null;
      k = skipSpaces(source, close);
    } else return null;
    const fromWord = readWord(source, k);
    if (fromWord.word !== "from") return null;
    const clause = source.slice(clauseStart, k).trim();
    k = skipSpaces(source, fromWord.end);
    const specifier = readString(source, k);
    if (!specifier) return null;
    k = skipSpaces(source, specifier.end);
    if (source[k] !== ";") return null;
    return { clause, specifier: specifier.text, end: k + 1 };
  }

  /// Rewrite one module source into the wrapper body the factory evaluates.
  /// Discovery is token-aware and depth-aware, never line-based: production
  /// bundles are one minified line (Vite, Rollup), so a statement may begin
  /// anywhere; `import` and `export` are reserved words at module depth only.
  function transformModule(source, url) {
    const ranges = protectedRanges(source);
    const edits = []; // {start, end, text}
    let temp = 0;
    const n = source.length;
    let i = 0;
    let depth = 0;
    let rangeAt = 0;
    while (i < n) {
      // A statement jump can leap over protected ranges (an import's own
      // specifier string sits inside its span): advance the cursor past them, or
      // the walk reads string content as code from here on.
      while (rangeAt < ranges.length && ranges[rangeAt][1] <= i) rangeAt++;
      if (rangeAt < ranges.length && ranges[rangeAt][0] === i) { i = ranges[rangeAt][1]; rangeAt++; continue; }
      const c = source[i];
      if (c === "(" || c === "[" || c === "{") { depth++; i++; continue; }
      if (c === ")" || c === "]" || c === "}") { if (depth > 0) depth--; i++; continue; }
      if (depth === 0 && /[A-Za-z_$]/.test(c)) {
        const word = readWord(source, i);
        if (word.word === "import") {
          const statement = parseImportAt(source, i, ranges);
          if (statement) {
            const clause = parseImportClause(statement.clause);
            const moduleName = "__m" + (temp++);
            const lines = ["const " + moduleName + " = __import(" + JSON.stringify(statement.specifier) + ");"];
            if (clause.defaultName) lines.push("const " + clause.defaultName + " = " + moduleName + ".default;");
            if (clause.namespace) lines.push("const " + clause.namespace + " = " + moduleName + ";");
            for (const [imported, local] of clause.names) lines.push("const " + local + " = " + moduleName + "." + imported + ";");
            edits.push({ start: i, end: statement.end, text: lines.join("\n") });
            i = statement.end;
            continue;
          }
        } else if (word.word === "export") {
          const at = skipSpaces(source, word.end);
          const next = source[at];
          if (next === "{") {
            const close = matchingBrace(source, at, ranges);
            let handled = false;
            if (close > 0) {
              let k = skipSpaces(source, close);
              let specifier = null;
              const fromWord = readWord(source, k);
              if (fromWord.word === "from") {
                k = skipSpaces(source, fromWord.end);
                const s = readString(source, k);
                if (s) { specifier = s.text; k = skipSpaces(source, s.end); }
              }
              if (source[k] === ";") {
                const pairs = parseNameList(source.slice(at + 1, close - 1));
                const end = k + 1;
                if (specifier) {
                  const moduleName = "__re" + (temp++);
                  const lines = ["const " + moduleName + " = __import(" + JSON.stringify(specifier) + ");"];
                  for (const [exported, local] of pairs) lines.push("__export(" + JSON.stringify(local) + ", () => " + moduleName + "." + exported + ");");
                  edits.push({ start: i, end, text: lines.join("\n") });
                } else {
                  const lines = pairs.map(([local, exported]) => "__export(" + JSON.stringify(exported) + ", () => " + local + ");");
                  edits.push({ start: i, end, text: lines.join("\n") });
                }
                i = end;
                handled = true;
              }
            }
            if (!handled) { i = word.end; continue; }
            continue;
          }
          if (next === "*") {
            let k = skipSpaces(source, at + 1);
            let namespace = null;
            const asWord = readWord(source, k);
            if (asWord.word === "as") {
              k = skipSpaces(source, asWord.end);
              const name = readWord(source, k);
              namespace = name.word;
              k = skipSpaces(source, name.end);
            }
            const fromWord = readWord(source, k);
            if (fromWord.word === "from") {
              k = skipSpaces(source, fromWord.end);
              const s = readString(source, k);
              if (s) {
                k = skipSpaces(source, s.end);
                if (source[k] === ";") {
                  const end = k + 1;
                  edits.push({ start: i, end, text: namespace
                    ? "const __star" + (temp++) + " = __import(" + JSON.stringify(s.text) + ");\n__export(" + JSON.stringify(namespace) + ", () => __star" + (temp - 1) + ");"
                    : "__exportStar(__import(" + JSON.stringify(s.text) + "));" });
                  i = end;
                  continue;
                }
              }
            }
          } else {
            const declaration = readWord(source, at);
            if (declaration.word === "default") {
              const declStart = i;
              const restStart = skipSpaces(source, declaration.end);
              const rest = source.slice(restStart);
              const named = new RegExp("^((?:async\\s+)?function\\s*\\*?\\s*(" + IDENTIFIER + ")|class\\s+(" + IDENTIFIER + "))").exec(rest);
              if (named) {
                const end = braceStatementEnd(source, restStart, ranges);
                const header = source.slice(restStart, end).replace(/;$/, "");
                const name = named[2] || named[3];
                edits.push({ start: declStart, end, text: header + "\n__export(\"default\", () => " + name + ");" });
              } else if (/^(?:async\s+)?function\s*\*?\s*\(|^class\s*\{/.test(rest)) {
                const end = braceStatementEnd(source, restStart, ranges);
                const body = source.slice(restStart, end).replace(/;$/, "");
                edits.push({ start: declStart, end, text: "const __default" + (temp++) + " = " + body + ";\n__export(\"default\", () => __default" + (temp - 1) + ");" });
              } else {
                const end = statementEnd(source, restStart, ranges);
                const expression = source.slice(restStart, end).replace(/;$/, "");
                edits.push({ start: declStart, end, text: "const __default" + (temp++) + " = (" + expression + ");\n__export(\"default\", () => __default" + (temp - 1) + ");" });
              }
              i = declStart + 6;
              continue;
            }
            if (declaration.word === "const" || declaration.word === "let" || declaration.word === "var") {
              const declStart = i;
              const end = statementEnd(source, declStart, ranges);
              const statement = source.slice(declStart, end);
              // The emitted declaration keeps its keyword; the name scan works on
              // the declarators alone (the keyword itself is not a name).
              const declarationText = statement.replace(/^export\s+/, "");
              const names = [];
              let scanDepth = 0;
              let word2 = "";
              let expectName = true;
              const body = declarationText.replace(/^(const|let|var)\s+/, "");
              for (let k = 0; k < body.length; k++) {
                const c2 = body[k];
                if (c2 === "(" || c2 === "[" || c2 === "{") { scanDepth++; continue; }
                if (c2 === ")" || c2 === "]" || c2 === "}") { scanDepth--; continue; }
                if (scanDepth === 0 && expectName) {
                  const wordMatch = new RegExp(IDENTIFIER).exec(body.slice(k));
                  if (wordMatch && wordMatch.index === 0) { word2 = wordMatch[0]; k += word2.length - 1; expectName = false; continue; }
                  expectName = false;
                }
                if (scanDepth === 0 && c2 === "," ) { expectName = true; continue; }
                if (scanDepth === 0 && c2 === "=") { if (word2) names.push(word2); expectName = false; }
              }
              if (word2) names.push(word2);
              const unique = Array.from(new Set(names.filter((name) => IDENTIFIER_RE.test(name))));
              const exports = unique.map((name) => "__export(" + JSON.stringify(name) + ", () => " + name + ");").join("\n");
              edits.push({ start: declStart, end, text: declarationText + "\n" + exports });
              i = end;
              continue;
            }
            if (declaration.word === "function" || declaration.word === "class" || declaration.word === "async") {
              const declStart = i;
              const head = new RegExp("^((?:async\\s+)?function\\s*\\*?\\s*|class\\s+)(" + IDENTIFIER + ")").exec(source.slice(at));
              if (head) {
                const end = braceStatementEnd(source, declStart, ranges);
                const body = source.slice(declStart, end).replace(/^export\s+/, "").replace(/;$/, "");
                edits.push({ start: declStart, end, text: body + "\n__export(" + JSON.stringify(head[2]) + ", () => " + head[2] + ");" });
                i = end;
                continue;
              }
            }
          }
        }
        i = word.end;
        continue;
      }
      i++;
    }
    edits.sort((a, b) => a.start - b.start);
    // One pass over the source: statement edits replace their spans; everything
    // else is copied, with `import.meta` and dynamic `import(` rewritten outside
    // protected ranges.
    let out = "";
    let at = 0;
    let copyAt = 0;
    let editAt = 0;
    while (at < n) {
      // Same discipline as the walk: an edit span can contain protected ranges
      // (a specifier string), so the cursor advances past everything the jump
      // covered before the next token can be read.
      while (copyAt < ranges.length && ranges[copyAt][1] <= at) copyAt++;
      if (editAt < edits.length && edits[editAt].start === at) {
        out += edits[editAt].text;
        at = edits[editAt].end;
        editAt++;
        continue;
      }
      if (copyAt < ranges.length && ranges[copyAt][0] === at) {
        out += source.slice(at, ranges[copyAt][1]);
        at = ranges[copyAt][1];
        copyAt++;
        continue;
      }
      const c = source[at];
      const before = at === 0 ? "" : source[at - 1];
      const boundary = before === "" || !/[$\w]/.test(before);
      if (boundary && source.startsWith("import", at)) {
        const afterWord = source[at + 6] || "";
        if (afterWord === "." && source.startsWith("meta", at + 7)) {
          out += "importMeta";
          at += 11;
          continue;
        }
        let probe = at + 6;
        while (probe < n && (source[probe] === " " || source[probe] === "\t")) probe++;
        if (source[probe] === "(") {
          out += "__dynamicImport";
          at += 6;
          continue;
        }
      }
      out += c;
      at++;
    }
    if (editAt < edits.length) throw new Error("transform left an edit unapplied");
    return out;
  }

  function exportStarInto(define, from) {
    for (const key of Object.keys(from)) {
      if (key === "default") continue;
      const source = from;
      define(key, () => source[key]);
    }
  }

  /// Run one module graph node: transform, evaluate with its dependency function
  /// bound to this module's URL, register the exports object before evaluation.
  function evaluateModuleSource(source, url) {
    const record = { exports: {}, state: "loading" };
    MODULES.set(url, record);
    let code;
    try {
      code = transformModule(String(source), url);
    } catch (error) {
      note("log", "module transform " + String(url).slice(0, 90) + ": " + (error && error.message));
      record.state = "failed";
      return record.exports;
    }
    const compile = (text) => {
      const compiled = core.evalContext("(function (__export, __exportStar, __import, __dynamicImport, importMeta) {\"use strict\";\n" + text + "\n})", url);
      const thrown = compiled && compiled[1];
      if (thrown) throw thrown.thrown || thrown;
      return compiled[0];
    };
    let factory = null;
    try {
      factory = compile(code);
    } catch (error) {
      const message = String((error && error.message) || error);
      // A quote inside a regex literal can hide an `import.meta` from the
      // transform. The retry is logged and bounded to that one class: it either
      // compiles the module or fails visibly, as before.
      if (message.indexOf("import.meta") < 0) {
        note("log", "module " + String(url).slice(0, 90) + ": " + message);
        record.state = "failed";
        return record.exports;
      }
      try {
        factory = compile(String(code).split("import.meta").join("importMeta"));
        note("log", "module " + String(url).slice(0, 90) + ": lenient import.meta replacement");
      } catch (retryError) {
        note("log", "module " + String(url).slice(0, 90) + ": " + String((retryError && retryError.message) || retryError));
        record.state = "failed";
        return record.exports;
      }
    }
    try {
      const define = (name, getter) => Object.defineProperty(record.exports, name, { get: getter, enumerable: true, configurable: true });
      factory(
        define,
        (from) => exportStarInto(define, from),
        (specifier) => importModule(String(specifier), url),
        (specifier) => Promise.resolve().then(() => importModule(String(specifier), url)),
        { url, resolve: (specifier) => new URLShim(String(specifier), url).href, env: MODULE_ENV },
      );
    } catch (error) {
      note("log", "module " + String(url).slice(0, 90) + ": " + (error && error.message));
      record.state = "failed";
    }
    if (record.state !== "failed") record.state = "done";
    return record.exports;
  }

  function importModule(specifier, base) {
    let absolute;
    try {
      absolute = new URLShim(String(specifier), base).href;
    } catch (error) {
      note("log", "module specifier " + String(specifier).slice(0, 80) + ": " + (error && error.message));
      return {};
    }
    return importAbsolute(absolute);
  }

  function importAbsolute(url) {
    const existing = MODULES.get(url);
    if (existing) return existing.exports;
    let parsed;
    try {
      parsed = loadScript(url);
    } catch (error) {
      parsed = { error: String(error && error.message) };
    }
    if (parsed.error || !(parsed.status >= 200 && parsed.status < 300)) {
      note("log", "module fetch " + String(url).slice(0, 110) + " -> " + (parsed.error || parsed.status));
      const empty = { exports: {}, state: "failed" };
      MODULES.set(url, empty);
      return empty.exports;
    }
    return evaluateModuleSource(parsed.body, url);
  }

  function makeContentEnvironment(page) {
    const host = new URL(page.url).hostname;
    const ident = IDENTITY = page.identity;
    DEBUG = !!page.debug;
    const listeners = new Map();
    const timers = [];
    const rafs = [];
    let timerId = 1;
    let rafId = 1;
    const navigationMs = Math.max(0, Number(page.navigationMs) || 0);
    const now = () => Math.floor((ops.op_now() + navigationMs) * 10) / 10;
    const timeOrigin = Math.floor((Date.now() - (ops.op_now() + navigationMs)) * 10) / 10;
    const viewportWidth = 1920;
    const viewportHeight = 1080;

    // ----- nodes -----
    // EventTarget is the base the DOM inheritance chain starts from; every node
    // is one, and libraries extend it directly.
    class EventTargetShim {
      addEventListener(type, handler, options) {
        if (typeof handler !== "function" && !(handler && typeof handler.handleEvent === "function")) return;
        if (!listeners.has(this)) listeners.set(this, new Map());
        const map = listeners.get(this);
        if (!map.has(type)) map.set(type, []);
        map.get(type).push({ handler, once: !!(options && options.once) });
      }
      removeEventListener(type, handler) {
        const map = listeners.get(this);
        if (!map || !map.has(type)) return;
        map.set(type, map.get(type).filter((entry) => entry.handler !== handler));
      }
      dispatchEvent(event) { return dispatchOn(this, event); }
    }

    class VNode extends EventTargetShim {
      constructor(ownerDocument) {
        super();
        this.ownerDocument = ownerDocument;
        this.parentNode = null;
        this.childNodes = [];
      }
      get parentElement() { return this.parentNode && this.parentNode.nodeType === 1 ? this.parentNode : null; }
      get firstChild() { return this.childNodes[0] || null; }
      get lastChild() { return this.childNodes[this.childNodes.length - 1] || null; }
      get previousSibling() {
        if (!this.parentNode) return null;
        const siblings = this.parentNode.childNodes;
        const at = siblings.indexOf(this);
        return at > 0 ? siblings[at - 1] : null;
      }
      get nextSibling() {
        if (!this.parentNode) return null;
        const siblings = this.parentNode.childNodes;
        const at = siblings.indexOf(this);
        return at >= 0 && at + 1 < siblings.length ? siblings[at + 1] : null;
      }
      get isConnected() {
        let node = this;
        while (node.parentNode) node = node.parentNode;
        return node.nodeType === 9;
      }
      getRootNode() {
        let node = this;
        while (node.parentNode) node = node.parentNode;
        return node;
      }
      hasChildNodes() { return this.childNodes.length > 0; }
      contains(other) {
        for (let node = other; node; node = node.parentNode) if (node === this) return true;
        return false;
      }
      appendChild(node) { return this.insertBefore(node, null); }
      insertBefore(node, reference) {
        if (!node) return node;
        if (node.nodeType === 11) {
          for (const child of node.childNodes.slice()) this.insertBefore(child, reference);
          return node;
        }
        if (node.parentNode) node.parentNode.removeChild(node);
        node.parentNode = this;
        if (reference == null) this.childNodes.push(node);
        else {
          const at = this.childNodes.indexOf(reference);
          if (at < 0) this.childNodes.push(node);
          else this.childNodes.splice(at, 0, node);
        }
        if (this.ownerDocument && this.ownerDocument._record) {
          this.ownerDocument._record({ type: "childList", target: this, addedNodes: [node], removedNodes: [] });
        }
        if (node.nodeType === 1 && node.tagName === "SCRIPT" && this.isConnected && this._contentEnv && this._contentEnv.armed) {
          this._contentEnv.runScript(node);
        }
        return node;
      }
      removeChild(node) {
        const at = this.childNodes.indexOf(node);
        if (at >= 0) {
          this.childNodes.splice(at, 1);
          node.parentNode = null;
          if (this.ownerDocument && this.ownerDocument._record) {
            this.ownerDocument._record({ type: "childList", target: this, addedNodes: [], removedNodes: [node] });
          }
        }
        return node;
      }
      replaceChild(next, previous) {
        this.insertBefore(next, previous);
        this.removeChild(previous);
        return previous;
      }
      append(...nodes) { this._addMany(nodes, null); }
      prepend(...nodes) { this._addMany(nodes, this.firstChild); }
      _addMany(nodes, reference) {
        for (const node of nodes) {
          const child = typeof node === "string" ? this.ownerDocument.createTextNode(node) : node;
          this.insertBefore(child, reference);
        }
      }
      remove() { if (this.parentNode) this.parentNode.removeChild(this); }
      replaceWith(...nodes) {
        if (!this.parentNode) return;
        const reference = this.nextSibling;
        const parent = this.parentNode;
        parent.removeChild(this);
        for (const node of nodes) parent.insertBefore(typeof node === "string" ? parent.ownerDocument.createTextNode(node) : node, reference);
      }
      get textContent() {
        if (this.nodeType === 3) return this.nodeValue;
        let out = "";
        (function walk(node) {
          for (const child of node.childNodes) {
            if (child.nodeType === 3) out += child.nodeValue;
            else walk(child);
          }
        })(this);
        return out;
      }
      set textContent(value) {
        for (const child of this.childNodes) child.parentNode = null;
        this.childNodes = [];
        if (value !== "" && value != null) this.insertBefore(this.ownerDocument.createTextNode(String(value)), null);
      }
    }

    // Dispatch along the path a browser takes: the target, its ancestors, the
    // document, the window; capture phase is not modeled (nothing measured needs
    // it), bubble and inline handlers are. stopPropagation and preventDefault
    // behave as the spec says.
    function dispatchOn(target, event) {
      if (!event || typeof event !== "object") return true;
      event.target = event.target || target;
      const path = [target];
      let node = target;
      while (node && node.parentNode) { node = node.parentNode; path.push(node); }
      if (target !== win) {
        if (path[path.length - 1] !== doc) path.push(doc);
        path.push(win);
      }
      if (event.bubbles === false) path.length = 1;
      let stopped = false;
      event.stopPropagation = function () { stopped = true; };
      event.stopImmediatePropagation = function () { stopped = true; };
      if (!event.preventDefault) event.preventDefault = function () { this.defaultPrevented = true; };
      for (const current of path) {
        event.currentTarget = current;
        const map = listeners.get(current);
        const list = map && map.get(event.type);
        if (list) {
          for (const entry of list.slice()) {
            try { entry.handler.handleEvent ? entry.handler.handleEvent.call(entry.handler, event) : entry.handler.call(current, event); }
            catch (error) { note("log", "handler " + event.type + ": " + (error && error.message)); }
            if (entry.once) current.removeEventListener(event.type, entry.handler);
            if (stopped) break;
          }
        }
        const inline = current["on" + event.type];
        if (typeof inline === "function" && !stopped) {
          try { inline.call(current, event); } catch (error) { note("log", "inline " + event.type + ": " + (error && error.message)); }
        }
        if (stopped) break;
      }
      return !event.defaultPrevented;
    }

    class VText extends VNode {
      constructor(ownerDocument, data) {
        super(ownerDocument);
        this.nodeType = 3;
        this.nodeName = "#text";
        this._data = String(data);
      }
      get nodeValue() { return this._data; }
      set nodeValue(value) {
        const old = this._data;
        this._data = String(value);
        if (this.ownerDocument && this.ownerDocument._record) {
          this.ownerDocument._record({ type: "characterData", target: this, oldValue: old });
        }
      }
      get data() { return this._data; }
      set data(value) { this.nodeValue = value; }
      get textContent() { return this._data; }
      set textContent(value) { this.nodeValue = value; }
      get length() { return this._data.length; }
    }

    class VFragment extends VNode {
      constructor(ownerDocument) {
        super(ownerDocument);
        this.nodeType = 11;
        this.nodeName = "#document-fragment";
      }
    }

    // A shadow root is a document fragment with a host: the data plane needs the
    // interface for `instanceof` checks and for `attachShadow` styles, never a
    // rendering boundary. Its children are not serialized into outerHTML, as in
    // a browser.
    class VShadowRoot extends VFragment {
      constructor(host, mode) {
        super(host.ownerDocument);
        this.host = host;
        this.mode = mode === "closed" ? "closed" : "open";
      }
    }

    const REFLECTED = {
      id: "id", title: "title", lang: "lang", dir: "dir", name: "name", type: "type", rel: "rel",
      media: "media", target: "target", action: "action", method: "method", placeholder: "placeholder",
      alt: "alt", src: "src", href: "href", htmlFor: "for", accept: "accept", acceptCharset: "accept-charset",
      autocomplete: "autocomplete", contentEditable: "contenteditable", crossOrigin: "crossorigin",
      decoding: "decoding", integrity: "integrity", loading: "loading", referrerPolicy: "referrerpolicy",
      sizes: "sizes", scope: "scope", role: "role", slot: "slot", nonce: "nonce", srcset: "srcset",
      useMap: "usemap", label: "label", dirName: "dirname", preload: "preload", kind: "kind",
    };
    const BOOLEAN_ATTRS = ["hidden", "disabled", "checked", "selected", "async", "defer", "noModule",
      "readOnly", "required", "multiple", "autofocus", "open", "playsInline", "muted", "loop", "controls"];

    class VElement extends VNode {
      constructor(ownerDocument, tag) {
        super(ownerDocument);
        this.nodeType = 1;
        this.tagName = String(tag).toUpperCase();
        this.nodeName = this.tagName;
        this.localName = String(tag).toLowerCase();
        this.namespaceURI = "http://www.w3.org/1999/xhtml";
        this._attrs = new Map();
        this.style = this._makeStyle();
        this.dataset = this._makeDataset();
        this.classList = this._makeClassList();
        this._contentEnv = null;
        if (this.tagName === "CANVAS") {
          const canvas = this;
          canvas.width = 300;
          canvas.height = 150;
          canvas.getContext = (kind) => (kind === "2d" ? new Ctx2D(canvas) : kind === "webgl" || kind === "experimental-webgl" ? new GL() : null);
          canvas.toDataURL = () => {
            let body = "iVBORw0KGgoAAAANSUhEUgAAABQAAAAUCAYAAACNiR0NAAAAh0lEQVR42u3TsQ3AIAwEwI+0";
            const chunk = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODk=";
            while (body.length < 24000) body += chunk;
            return "data:image/png;base64," + body;
          };
          canvas.toBlob = (callback) => { try { callback(null); } catch (error) { /* as Chrome */ } };
        }
        if (this.tagName === "STYLE" || this.tagName === "LINK") {
          // A style element's CSSOM view: emotion and every CSS-in-JS layer insert
          // rules through it. The rules are kept as data, never applied to a
          // layout (there is none).
          const owner = this;
          const rules = [];
          this.sheet = {
            get cssRules() { return rules; },
            get ownerNode() { return owner; },
            insertRule(rule, index) {
              const at = index === undefined ? rules.length : Math.max(0, Math.min(index, rules.length));
              rules.splice(at, 0, { cssText: String(rule) });
              return at;
            },
            deleteRule(index) { rules.splice(index, 1); },
            replaceSync(text) {
              rules.length = 0;
              for (const part of String(text).split("}")) if (part.trim()) rules.push({ cssText: part + "}" });
            },
            replace(text) { this.replaceSync(text); return Promise.resolve(); },
          };
        }
      }
      _makeStyle() {
        const element = this;
        const store = new Map();
        const sync = () => {
          if (!store.size) { element._attrs.delete("style"); return; }
          const text = Array.from(store).map(([name, value]) => name + ":" + value).join(";");
          element._attrs.set("style", text);
        };
        const api = {
          get cssText() { return Array.from(store).map(([name, value]) => name + ":" + value).join(";"); },
          set cssText(value) {
            store.clear();
            for (const part of String(value).split(";")) {
              const at = part.indexOf(":");
              if (at > 0) store.set(part.slice(0, at).trim().toLowerCase(), part.slice(at + 1).trim());
            }
            sync();
          },
          setProperty(name, value) { store.set(String(name).toLowerCase(), String(value)); sync(); },
          getPropertyValue(name) { return store.get(String(name).toLowerCase()) || ""; },
          removeProperty(name) {
            const old = store.get(String(name).toLowerCase()) || "";
            store.delete(String(name).toLowerCase());
            sync();
            return old;
          },
          item(index) { return Array.from(store.keys())[index] || ""; },
          get length() { return store.size; },
        };
        return new Proxy(api, {
          get(target, key) {
            if (key in target || typeof key === "symbol") return Reflect.get(target, key);
            return store.get(camelToKebab(key)) || "";
          },
          set(target, key, value) {
            if (key === "cssText") { target.cssText = value; return true; }
            if (typeof key === "string") {
              const name = camelToKebab(key);
              if (value === "" || value == null) store.delete(name);
              else store.set(name, String(value));
              sync();
            }
            return true;
          },
        });
      }
      _makeDataset() {
        const element = this;
        return new Proxy({}, {
          get(_target, key) {
            if (typeof key !== "string") return undefined;
            return element._attrs.get("data-" + camelToKebab(key));
          },
          set(_target, key, value) {
            if (typeof key !== "string") return false;
            element.setAttribute("data-" + camelToKebab(key), String(value));
            return true;
          },
          deleteProperty(_target, key) {
            if (typeof key !== "string") return false;
            element.removeAttribute("data-" + camelToKebab(key));
            return true;
          },
          has(_target, key) {
            return typeof key === "string" && element._attrs.has("data-" + camelToKebab(key));
          },
        });
      }
      _makeClassList() {
        const element = this;
        const read = () => (element.getAttribute("class") || "").split(/\s+/).filter(Boolean);
        const write = (list) => element.setAttribute("class", list.join(" "));
        return {
          add(...names) { const list = read(); for (const name of names) if (!list.includes(String(name))) list.push(String(name)); write(list); },
          remove(...names) { const drop = names.map(String); write(read().filter((name) => !drop.includes(name))); },
          toggle(name, force) {
            const list = read();
            const has = list.includes(String(name));
            const want = force === undefined ? !has : !!force;
            if (want && !has) list.push(String(name));
            write(want ? list : list.filter((value) => value !== String(name)));
            return want;
          },
          contains(name) { return read().includes(String(name)); },
          replace(from, to) {
            const list = read();
            const at = list.indexOf(String(from));
            if (at < 0) return false;
            list[at] = String(to);
            write(list);
            return true;
          },
          item(index) { return read()[index] || null; },
          get length() { return read().length; },
          get value() { return element.getAttribute("class") || ""; },
          forEach(callback, thisArg) { read().forEach((name) => callback.call(thisArg, name, name, this)); },
          entries() { return read().map((name, at) => [at, name])[Symbol.iterator](); },
          keys() { return read().keys(); },
          values() { return read().values(); },
          [Symbol.iterator]() { return read()[Symbol.iterator](); },
          toString() { return element.getAttribute("class") || ""; },
        };
      }
      get className() { return this.getAttribute("class") || ""; }
      set className(value) { this.setAttribute("class", String(value)); }
      get hidden() { return this._attrs.has("hidden"); }
      set hidden(value) { if (value) this.setAttribute("hidden", ""); else this.removeAttribute("hidden"); }
      getAttribute(name) {
        const key = String(name).toLowerCase();
        return this._attrs.has(key) ? this._attrs.get(key) : null;
      }
      setAttribute(name, value) {
        const key = String(name).toLowerCase();
        const old = this._attrs.has(key) ? this._attrs.get(key) : null;
        this._attrs.set(key, String(value));
        if (this.ownerDocument && this.ownerDocument._record) {
          this.ownerDocument._record({ type: "attributes", target: this, attributeName: key, oldValue: old });
        }
      }
      setAttributeNS(_namespace, name, value) { this.setAttribute(name, value); }
      getAttributeNS(_namespace, name) { return this.getAttribute(name); }
      removeAttribute(name) {
        const key = String(name).toLowerCase();
        if (!this._attrs.has(key)) return;
        const old = this._attrs.get(key);
        this._attrs.delete(key);
        if (this.ownerDocument && this.ownerDocument._record) {
          this.ownerDocument._record({ type: "attributes", target: this, attributeName: key, oldValue: old });
        }
      }
      removeAttributeNS(_namespace, name) { this.removeAttribute(name); }
      hasAttribute(name) { return this._attrs.has(String(name).toLowerCase()); }
      hasAttributes() { return this._attrs.size > 0; }
      toggleAttribute(name, force) {
        const key = String(name).toLowerCase();
        const has = this._attrs.has(key);
        const want = force === undefined ? !has : !!force;
        if (want && !has) this.setAttribute(key, "");
        else if (!want && has) this.removeAttribute(key);
        return want;
      }
      getAttributeNames() { return Array.from(this._attrs.keys()); }
      get children() { return this.childNodes.filter((node) => node.nodeType === 1); }
      get firstElementChild() { return this.children[0] || null; }
      get lastElementChild() { const list = this.children; return list[list.length - 1] || null; }
      get childElementCount() { return this.children.length; }
      get nextElementSibling() {
        let node = this.nextSibling;
        while (node && node.nodeType !== 1) node = node.nextSibling;
        return node;
      }
      get previousElementSibling() {
        let node = this.previousSibling;
        while (node && node.nodeType !== 1) node = node.previousSibling;
        return node;
      }
      get innerHTML() {
        let out = "";
        for (const child of this.childNodes) serializeInto(child, (text) => { out += text; }, 0);
        return out;
      }
      set innerHTML(value) {
        const tree = JSON.parse(ops.op_parse_html(String(value), "fragment"));
        if (tree && tree.error) throw new Error(tree.error);
        for (const child of this.childNodes) child.parentNode = null;
        this.childNodes = [];
        // Scripts inserted through innerHTML never execute, as in a browser.
        const env = this._contentEnv;
        const wasArmed = env ? env.armed : false;
        if (env) env.armed = false;
        try {
          for (const spec of tree) {
            const child = buildSpec(spec);
            if (child) { child.parentNode = this; this.childNodes.push(child); }
          }
        } finally {
          if (env) env.armed = wasArmed;
        }
        if (this.ownerDocument && this.ownerDocument._record) {
          this.ownerDocument._record({ type: "childList", target: this, addedNodes: [], removedNodes: [] });
        }
      }
      get outerHTML() {
        let out = "";
        serializeInto(this, (text) => { out += text; }, 0);
        return out;
      }
      set outerHTML(value) {
        if (!this.parentNode) return;
        const fragment = this.ownerDocument.createElement("div");
        fragment.innerHTML = String(value);
        const reference = this.nextSibling;
        const parent = this.parentNode;
        parent.removeChild(this);
        for (const child of fragment.childNodes.slice()) parent.insertBefore(child, reference);
      }
      insertAdjacentHTML(position, html) {
        const fragment = this.ownerDocument.createElement("div");
        fragment.innerHTML = String(html);
        insertAdjacent(this, position, fragment.childNodes.slice());
      }
      insertAdjacentElement(position, element) { insertAdjacent(this, position, [element]); return element; }
      insertAdjacentText(position, text) { insertAdjacent(this, position, [this.ownerDocument.createTextNode(String(text))]); }
      setHTMLUnsafe(html) { this.innerHTML = html; }
      getHTML() { return this.innerHTML; }
      getBoundingClientRect() {
        const box = this === this.ownerDocument.documentElement || this === this.ownerDocument.body
          ? { x: 0, y: 0, width: viewportWidth, height: viewportHeight, top: 0, left: 0, right: viewportWidth, bottom: viewportHeight }
          : { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 };
        note("surface", "element.getBoundingClientRect");
        return box;
      }
      getClientRects() { note("surface", "element.getClientRects"); return []; }
      get clientWidth() { return this === this.ownerDocument.documentElement || this === this.ownerDocument.body ? viewportWidth : 0; }
      get clientHeight() { return this === this.ownerDocument.documentElement || this === this.ownerDocument.body ? viewportHeight : 0; }
      get offsetWidth() { return this.clientWidth; }
      get offsetHeight() { return this.clientHeight; }
      offsetTop = 0;
      offsetLeft = 0;
      scrollTop = 0;
      scrollLeft = 0;
      scrollWidth = viewportWidth;
      scrollHeight = viewportHeight;
      get width() { return Number(this.getAttribute("width")) || 0; }
      set width(value) { this.setAttribute("width", String(value)); }
      get height() { return Number(this.getAttribute("height")) || 0; }
      set height(value) { this.setAttribute("height", String(value)); }
      get value() { return this.getAttribute("value") || ""; }
      set value(value) { this.setAttribute("value", String(value)); }
      get checked() { return this._attrs.has("checked"); }
      set checked(value) { if (value) this.setAttribute("checked", ""); else this.removeAttribute("checked"); }
      querySelector(selector) { return collectSelect(this.ownerDocument, this, selector)[0] || null; }
      querySelectorAll(selector) { return collectSelect(this.ownerDocument, this, selector); }
      getElementsByTagName(tag) {
        const wanted = String(tag).toLowerCase();
        return allElements(this).filter((element) => wanted === "*" || element.tagName.toLowerCase() === wanted);
      }
      getElementsByClassName(names) {
        const wanted = String(names).split(/\s+/).filter(Boolean);
        return allElements(this).filter((element) => wanted.every((name) => element.classList.contains(name)));
      }
      matches(selector) {
        const list = parseSelector(selector);
        return list.some((parts) => matchesComplex(this, parts));
      }
      webkitMatchesSelector(selector) { return this.matches(selector); }
      closest(selector) {
        const list = parseSelector(selector);
        let node = this;
        while (node && node.nodeType === 1) {
          if (list.some((parts) => matchesComplex(node, parts))) return node;
          node = node.parentNode;
        }
        return null;
      }
      focus() { if (this.ownerDocument) this.ownerDocument.activeElement = this; }
      blur() {}
      click() { note("log", "click " + this.tagName + " (not dispatched: no interaction in the lane)"); }
      scrollIntoView() {}
      animate() { return { finished: Promise.resolve(), cancel() {}, play() {}, pause() {}, onfinish: null }; }
      attachShadow(init) {
        note("surface", "element.attachShadow");
        if (!this._shadowRoot) this._shadowRoot = new VShadowRoot(this, init && init.mode);
        return this._shadowRoot;
      }
      get shadowRoot() { return this._shadowRoot || null; }
      cloneNode(deep) {
        const copy = this.ownerDocument.createElement(this.localName);
        for (const [name, value] of this._attrs) copy._attrs.set(name, value);
        if (deep) {
          for (const child of this.childNodes) {
            const cloned = child.nodeType === 3 ? this.ownerDocument.createTextNode(child.nodeValue) : child.cloneNode(true);
            cloned.parentNode = copy;
            copy.childNodes.push(cloned);
          }
        }
        return copy;
      }
      getContext() { return null; }
      set src(value) { this.setAttribute("src", String(value)); }
      get src() { return this.getAttribute("src") || ""; }
    }

    for (const key of Object.keys(REFLECTED)) {
      if (key in VElement.prototype) continue;
      Object.defineProperty(VElement.prototype, key, {
        configurable: true,
        get() { return this.getAttribute(REFLECTED[key]) || ""; },
        set(value) { this.setAttribute(REFLECTED[key], String(value)); },
      });
    }
    for (const key of BOOLEAN_ATTRS) {
      const attr = camelToKebab(key);
      if (key in VElement.prototype) continue;
      Object.defineProperty(VElement.prototype, key, {
        configurable: true,
        get() { return this._attrs.has(attr); },
        set(value) { if (value) this.setAttribute(attr, ""); else this.removeAttribute(attr); },
      });
    }

    // A member the host does not implement is a harmless, logged function on
    // any element instance: the same discipline as the challenge host. (A class
    // prototype is read-only, so the stub wraps each created element.)
    const elementStub = {
      get(target, key, receiver) {
        if (typeof key === "symbol" || Reflect.has(target, key)) return Reflect.get(target, key, receiver);
        // A browser answers `undefined` for a member it does not have, and
        // feature checks (React's own internal keys among them) depend on that
        // answer. The touch is logged so a measured gap can be filled; a guess
        // is never returned in its place.
        note("surface", "element." + String(key));
        return undefined;
      },
    };

    // HTMLDivElement and friends: the constructor names frameworks check.
    const TAG_CLASSES = {
      HTMLElement: "HTMLHTMLElement", HTMLDivElement: "HTMLDivElement", HTMLSpanElement: "HTMLSpanElement",
      HTMLAnchorElement: "HTMLAnchorElement", HTMLInputElement: "HTMLInputElement", HTMLButtonElement: "HTMLButtonElement",
      HTMLFormElement: "HTMLFormElement", HTMLImageElement: "HTMLImageElement", HTMLCanvasElement: "HTMLCanvasElement",
      HTMLScriptElement: "HTMLScriptElement", HTMLStyleElement: "HTMLStyleElement", HTMLLinkElement: "HTMLLinkElement",
      HTMLMetaElement: "HTMLMetaElement", HTMLHeadElement: "HTMLHeadElement", HTMLBodyElement: "HTMLBodyElement",
      HTMLParagraphElement: "HTMLParagraphElement", HTMLTemplateElement: "HTMLTemplateElement",
    };
    const tagClassFor = (tag) => {
      const upper = String(tag).toUpperCase();
      const name = "HTML" + (upper === "A" ? "Anchor" : upper === "IMG" ? "Image" : upper === "P" ? "Paragraph"
        : upper === "INPUT" ? "Input" : upper === "BUTTON" ? "Button" : upper === "FORM" ? "Form"
        : upper === "CANVAS" ? "Canvas" : upper === "SCRIPT" ? "Script" : upper === "STYLE" ? "Style"
        : upper === "LINK" ? "Link" : upper === "META" ? "Meta" : upper === "HEAD" ? "Head"
        : upper === "BODY" ? "Body" : upper === "TEMPLATE" ? "Template" : upper === "DIV" ? "Div"
        : upper === "SPAN" ? "Span" : upper.charAt(0) + upper.slice(1).toLowerCase()) + "Element";
      return name;
    };
    const TAG_CLASS_NAMES = new Map();
    for (const tag of ["div", "span", "a", "input", "button", "form", "img", "canvas", "script", "style", "link", "meta", "head", "body", "p", "template", "section", "nav", "header", "footer", "ul", "ol", "li", "table", "thead", "tbody", "tr", "td", "th", "select", "option", "textarea", "label", "article", "aside", "main", "h1", "h2", "h3", "h4", "h5", "h6", "svg", "video", "audio", "iframe", "dialog"]) {
      TAG_CLASS_NAMES.set(tag, tagClassFor(tag));
    }

    class VDocument extends VNode {
      constructor(url) {
        super(null);
        this.ownerDocument = this;
        this.nodeType = 9;
        this.nodeName = "#document";
        this.url = url;
        this.documentURI = url;
        this.readyState = "loading";
        this.title = "";
        this.referrer = "";
        this.currentScript = null;
        this.activeElement = null;
        this.visibilityState = "visible";
        this.hidden = false;
        this._observers = [];
        this._records = [];
        this._mutations = 0;
        this._flushScheduled = false;
        this.documentElement = null;
        this.head = null;
        this.body = null;
        this.scripts = [];
        this.documentMode = "css1Compat";
        this.compatMode = "CSS1Compat";
        this.characterSet = "UTF-8";
        this.charset = "UTF-8";
        this.contentType = "text/html";
      }
      _record(record) {
        this._mutations++;
        if (!this._observers.length) return;
        for (const observer of this._observers) {
          if (observer._watches(record)) observer._records.push(record);
        }
        if (!this._flushScheduled) {
          this._flushScheduled = true;
          const document = this;
          Promise.resolve().then(() => {
            document._flushScheduled = false;
            for (const observer of document._observers.slice()) {
              if (!observer._records.length) continue;
              const records = observer._records;
              observer._records = [];
              try { observer._callback.call(observer, records, observer); }
              catch (error) { note("log", "mutation observer: " + (error && error.message)); }
            }
          });
        }
      }
      get defaultView() { return win; }
      get location() { return location; }
      get cookie() { return cookieHeader(host); }
      set cookie(value) {
        const cookie = parseSetCookie(value);
        const key = cookie ? cookieKey(host, cookie.attrs) : null;
        if (key) storeCookie(key, cookie);
        else if (cookie) note("log", "cookie rejected " + cookie.name + " for " + host);
      }
      createElement(tag) {
        const element = createRawElement(tag);
        return element;
      }
      createElementNS(_namespace, tag) { return this.createElement(tag); }
      createTextNode(data) { return new VText(this, data); }
      createComment(data) { return new VText(this, data); }
      createDocumentFragment() { return new VFragment(this); }
      createAttribute(name) { return { name: String(name), value: "", ownerElement: null }; }
      createEvent(_type) {
        const event = { type: "", bubbles: false, cancelable: false, defaultPrevented: false, target: null, currentTarget: null, timeStamp: now(), preventDefault() { this.defaultPrevented = true; }, stopPropagation() {}, stopImmediatePropagation() {} };
        event.initEvent = function (type, bubbles, cancelable) { this.type = type; this.bubbles = !!bubbles; this.cancelable = !!cancelable; };
        event.initCustomEvent = function (type, bubbles, cancelable, detail) { this.initEvent(type, bubbles, cancelable); this.detail = detail; };
        return event;
      }
      getElementById(id) {
        const wanted = String(id);
        return allElements(this).find((element) => element.getAttribute("id") === wanted) || null;
      }
      querySelector(selector) { return collectSelect(this, this, selector)[0] || null; }
      querySelectorAll(selector) { return collectSelect(this, this, selector); }
      getElementsByTagName(tag) {
        const wanted = String(tag).toLowerCase();
        const list = allElements(this).filter((element) => wanted === "*" || element.tagName.toLowerCase() === wanted);
        if (wanted === "script") return this.scripts;
        return list;
      }
      getElementsByClassName(names) {
        const wanted = String(names).split(/\s+/).filter(Boolean);
        return allElements(this).filter((element) => wanted.every((name) => element.classList.contains(name)));
      }
      getElementsByName(name) {
        return allElements(this).filter((element) => element.getAttribute("name") === String(name));
      }
      get forms() { return allElements(this).filter((element) => element.tagName === "FORM"); }
      get styleSheets() { return allElements(this).filter((element) => (element.tagName === "STYLE" || element.tagName === "LINK") && element.sheet).map((element) => element.sheet); }
      get implementation() {
        return {
          createHTMLDocument: (title) => { const scratch = new VDocument(page.url); if (title) scratch.title = String(title); return scratch; },
          createDocument: () => new VDocument(page.url),
          createDocumentType: () => ({}),
          hasFeature: () => true,
        };
      }
      get images() { return allElements(this).filter((element) => element.tagName === "IMG"); }
      get links() {
        return allElements(this).filter((element) => (element.tagName === "A" || element.tagName === "AREA") && element.hasAttribute("href"));
      }
      elementFromPoint() { return null; }
      elementsFromPoint() { return []; }
      hasFocus() { return true; }
      execCommand() { return false; }
      write(...parts) { note("log", "document.write skipped: " + String(parts.join("")).slice(0, 80)); }
      writeln(...parts) { this.write(...parts); }
      open() { return this; }
      close() {}
      fonts = { check: () => true, ready: Promise.resolve(), add() {}, load: () => Promise.resolve([]) };
    }

    function insertAdjacent(el, position, nodes) {
      const where = String(position).toLowerCase();
      if (where === "beforebegin") { const parent = el.parentNode; if (!parent) return; for (const node of nodes) parent.insertBefore(node, el); }
      else if (where === "afterbegin") { const reference = el.firstChild; for (const node of nodes) el.insertBefore(node, reference); }
      else if (where === "beforeend") { for (const node of nodes) el.appendChild(node); }
      else if (where === "afterend") { const parent = el.parentNode; if (!parent) return; const reference = el.nextSibling; for (const node of nodes) parent.insertBefore(node, reference); }
    }

    // ----- the document and its tree -----
    let parts = urlParse(page.url) || {
      href: page.url, origin: "", protocol: "https:", hostname: host, host,
      pathname: "/", search: "", hash: "", username: "", password: "", port: "",
    };
    const doc = new VDocument(page.url);
    doc.ownerDocument = doc;

    function navigateTo(value) {
      const next = urlParse(value, parts.href);
      if (!next) return;
      parts = next;
      M.navigate = parts.href;
      note("log", "navigate " + String(parts.href).slice(0, 140));
    }
    const setLocationPart = (name, value) => {
      const next = urlParse(parts.href, undefined, name, value);
      if (next) parts = next;
    };
    const location = {
      get href() { return parts.href; },
      set href(value) { navigateTo(value); },
      get origin() { return parts.origin; },
      get protocol() { return parts.protocol; },
      set protocol(value) { setLocationPart("protocol", value); },
      get host() { return parts.host; },
      set host(value) { setLocationPart("host", value); },
      get hostname() { return parts.hostname; },
      set hostname(value) { setLocationPart("hostname", value); },
      get port() { return parts.port; },
      set port(value) { setLocationPart("port", value); },
      get pathname() { return parts.pathname; },
      set pathname(value) { setLocationPart("pathname", value); },
      get search() { return parts.search; },
      set search(value) { setLocationPart("search", value); },
      get hash() { return parts.hash; },
      set hash(value) { setLocationPart("hash", value); },
      get username() { return parts.username; },
      get password() { return parts.password; },
      assign(value) { navigateTo(value); },
      replace(value) { navigateTo(value); },
      reload() { note("log", "reload (not performed in the lane)"); },
      toString() { return parts.href; },
      valueOf() { return parts.href; },
    };

    // The tree parser is Rust (html5ever). The host materializes its tree format
    // (`[tag, attrs, ...children]`, text as a string) into live nodes.
    function createRawElement(tag) {
      const element = new VElement(doc, tag);
      element._contentEnv = env;
      return new Proxy(element, elementStub);
    }
    function buildSpec(spec) {
      if (typeof spec === "string") return new VText(doc, spec);
      const tag = String(spec[0]);
      const attributes = spec[1] || {};
      const element = createRawElement(tag);
      for (const name of Object.keys(attributes)) element._attrs.set(name, String(attributes[name]));
      for (let index = 2; index < spec.length; index++) {
        const child = buildSpec(spec[index]);
        if (child) { child.parentNode = element; element.childNodes.push(child); }
      }
      return element;
    }

    const env = { armed: false, timers, rafs, host, win: null, doc, runScript: null };

    // ----- storage, history, events, observers -----
    const storage = () => {
      const map = new Map();
      return {
        getItem: (key) => (map.has(String(key)) ? map.get(String(key)) : null),
        setItem: (key, value) => map.set(String(key), String(value)),
        removeItem: (key) => map.delete(String(key)),
        clear: () => map.clear(),
        key: (index) => Array.from(map.keys())[index] ?? null,
        get length() { return map.size; },
      };
    };
    const historyState = { length: 2, state: null, scrollRestoration: "auto" };
    // Fetch API pieces the content host owns: a content run starts in a fresh
    // isolate and cannot rely on another mode having run first.
    class HeadersShim {
      constructor(init) {
        this._m = new Map();
        if (init && init._m instanceof Map) { for (const [name, value] of init._m) this._m.set(name, value); }
        else if (Array.isArray(init)) { for (const [name, value] of init) this._m.set(String(name).toLowerCase(), String(value)); }
        else if (init) { for (const name of Object.keys(init)) this._m.set(String(name).toLowerCase(), String(init[name])); }
      }
      get(name) { const key = String(name).toLowerCase(); return this._m.has(key) ? this._m.get(key) : null; }
      set(name, value) { this._m.set(String(name).toLowerCase(), String(value)); }
      has(name) { return this._m.has(String(name).toLowerCase()); }
      delete(name) { this._m.delete(String(name).toLowerCase()); }
      append(name, value) { this._m.set(String(name).toLowerCase(), String(value)); }
      entries() { return this._m.entries(); }
      keys() { return this._m.keys(); }
      values() { return this._m.values(); }
      forEach(callback) { for (const [name, value] of this._m) callback(value, name, this); }
      [Symbol.iterator]() { return this._m.entries(); }
    }
    class RequestShim {
      constructor(input, init) {
        init = init || {};
        this.url = input && input.url ? input.url : String(input);
        this.method = String(init.method || "GET").toUpperCase();
        this.headers = new HeadersShim(init.headers || (input && input.headers) || {});
        this.body = init.body != null ? init.body : (input && input.body);
        this.credentials = init.credentials || "same-origin";
        this.mode = init.mode || "cors";
        this.signal = init.signal || null;
      }
      clone() { return new RequestShim(this.url, { method: this.method, headers: this.headers, body: this.body }); }
      text() { return Promise.resolve(String(this.body == null ? "" : this.body)); }
      json() { return Promise.resolve(JSON.parse(String(this.body == null ? "null" : this.body))); }
    }
    const win = {};
    const history = {
      get length() { return historyState.length; },
      get state() { return historyState.state; },
      scrollRestoration: "auto",
      pushState(state, _title, url) {
        historyState.state = state;
        historyState.length++;
        if (url != null) { const next = urlParse(url, parts.href); if (next) parts = next; }
      },
      replaceState(state, _title, url) {
        historyState.state = state;
        if (url != null) { const next = urlParse(url, parts.href); if (next) parts = next; }
      },
      back() { note("log", "history.back (not performed in the lane)"); },
      forward() { note("log", "history.forward (not performed in the lane)"); },
      go() { note("log", "history.go (not performed in the lane)"); },
    };

    class MutationObserverShim {
      constructor(callback) {
        this._callback = callback;
        this._records = [];
        this._targets = [];
        doc._observers.push(this);
      }
      _watches(record) {
        for (const entry of this._targets) {
          const target = entry.target;
          const options = entry.options || {};
          const related = record.target === target || (options.subtree && target.contains && target.contains(record.target));
          if (!related) continue;
          if (record.type === "childList" && options.childList) return true;
          if (record.type === "attributes" && options.attributes) {
            if (!options.attributeFilter || options.attributeFilter.includes(record.attributeName)) return true;
          }
          if (record.type === "characterData" && options.characterData) return true;
        }
        return false;
      }
      observe(target, options) {
        this._targets.push({ target, options: options || {} });
      }
      disconnect() {
        this._targets = [];
        this._records = [];
        const at = doc._observers.indexOf(this);
        if (at >= 0) doc._observers.splice(at, 1);
      }
      takeRecords() {
        const records = this._records;
        this._records = [];
        return records;
      }
    }

    const fireObserver = (callback, entries, observer) => {
      try { callback.call(observer, entries, observer); }
      catch (error) { note("log", "observer: " + (error && error.message)); }
    };
    // The lane's viewport contract: the window is the viewport and everything in
    // it is visible, so a lazy loader loads. One entry per observe.
    const windowBox = () => ({ x: 0, y: 0, width: viewportWidth, height: viewportHeight, top: 0, left: 0, right: viewportWidth, bottom: viewportHeight });

    class IntersectionObserverShim {
      constructor(callback, options) {
        this._callback = callback;
        this.root = (options && options.root) || null;
        this.rootMargin = (options && options.rootMargin) || "0px";
        this.thresholds = (options && options.threshold) ? [].concat(options.threshold) : [0];
        this._targets = new Set();
        note("surface", "IntersectionObserver");
      }
      observe(target) {
        if (this._targets.has(target)) return;
        this._targets.add(target);
        win.setTimeout(() => {
          if (!this._targets.has(target)) return;
          const box = windowBox();
          fireObserver(this._callback, [{ target, isIntersecting: true, intersectionRatio: 1, isVisible: true, time: now(), boundingClientRect: box, intersectionRect: box, rootBounds: box }], this);
        }, 0);
      }
      unobserve(target) { this._targets.delete(target); }
      disconnect() { this._targets.clear(); }
      takeRecords() { return []; }
    }

    class ResizeObserverShim {
      constructor(callback) {
        this._callback = callback;
        this._targets = new Set();
        note("surface", "ResizeObserver");
      }
      observe(target) {
        if (this._targets.has(target)) return;
        this._targets.add(target);
        win.setTimeout(() => {
          if (!this._targets.has(target)) return;
          const size = { inlineSize: viewportWidth, blockSize: viewportHeight };
          fireObserver(this._callback, [{ target, contentRect: windowBox(), borderBoxSize: [size], contentBoxSize: [size], devicePixelContentBoxSize: [size] }], this);
        }, 0);
      }
      unobserve(target) { this._targets.delete(target); }
      disconnect() { this._targets.clear(); }
    }

    class EventShim {
      constructor(type, init) {
        init = init || {};
        this.type = String(type || "");
        this.bubbles = !!init.bubbles;
        this.cancelable = !!init.cancelable;
        this.composed = !!init.composed;
        this.defaultPrevented = false;
        this.target = null;
        this.currentTarget = null;
        this.timeStamp = now();
        this.isTrusted = false;
      }
      preventDefault() { this.defaultPrevented = true; }
      stopPropagation() {}
      stopImmediatePropagation() {}
      composedPath() { return []; }
      initEvent(type, bubbles, cancelable) { this.type = String(type); this.bubbles = !!bubbles; this.cancelable = !!cancelable; }
    }
    class CustomEventShim extends EventShim {
      constructor(type, init) { super(type, init); this.detail = (init && init.detail) !== undefined ? init.detail : null; }
    }
    // The DOM event family: the constructors frameworks build events with, each
    // carrying the fields its name implies from the init dictionary.
    const eventClass = (name, fields) => {
      const cls = class extends EventShim {
        constructor(type, init) {
          super(type, init);
          init = init || {};
          for (const field of fields) this[field] = init[field] !== undefined ? init[field] : null;
        }
        get [Symbol.toStringTag]() { return name; }
      };
      try { Object.defineProperty(cls, "name", { value: name, configurable: true }); } catch (error) { /* frozen */ }
      return cls;
    };

    // ----- media queries -----
    function evalMediaFeature(name, value) {
      const width = viewportWidth;
      const height = viewportHeight;
      const numeric = value === null ? null : parseFloat(value);
      switch (name) {
        case "width": return numeric === null ? true : width === numeric;
        case "min-width": return numeric !== null && width >= numeric;
        case "max-width": return numeric !== null && width <= numeric;
        case "height": return numeric === null ? true : height === numeric;
        case "min-height": return numeric !== null && height >= numeric;
        case "max-height": return numeric !== null && height <= numeric;
        case "orientation": return String(value).toLowerCase() === (width >= height ? "landscape" : "portrait");
        case "prefers-color-scheme": return String(value).toLowerCase() === "light";
        case "prefers-reduced-motion": return String(value).toLowerCase() === "no-preference";
        case "prefers-contrast": return String(value).toLowerCase() === "no-preference";
        case "hover": case "any-hover": return String(value).toLowerCase() === "hover";
        case "pointer": case "any-pointer": return String(value).toLowerCase() === "fine";
        case "display-mode": return String(value).toLowerCase() === "browser";
        case "resolution": case "min-resolution": case "max-resolution": return true;
        case "color": return true;
        case "monochrome": return String(value) === "0" || numeric === 0;
        case "scripting": return String(value).toLowerCase() === "enabled";
        case "update": return String(value).toLowerCase() === "fast";
        default: note("surface", "matchMedia:" + name); return false;
      }
    }
    function evalMediaQuery(query) {
      let text = String(query).trim().toLowerCase();
      let negate = false;
      if (text.startsWith("not ")) { negate = true; text = text.slice(4); }
      let result = true;
      for (const part of text.split(/\s+and\s+/)) {
        const piece = part.trim();
        if (!piece) continue;
        if (piece === "all" || piece === "screen") continue;
        if (piece === "print") { result = false; continue; }
        const feature = /^\(\s*([a-z-]+)\s*:\s*(.+?)\s*\)$/.exec(piece);
        const bare = /^\(\s*([a-z-]+)\s*\)$/.exec(piece);
        if (feature) { if (!evalMediaFeature(feature[1], feature[2])) result = false; }
        else if (bare) { if (!evalMediaFeature(bare[1], null)) result = false; }
        else { note("log", "matchMedia: unparsed " + piece); result = false; }
      }
      return negate ? !result : result;
    }
    class MediaQueryListShim {
      constructor(query) {
        this.media = String(query);
        this.matches = this.media.split(",").some((part) => evalMediaQuery(part));
        this.onchange = null;
      }
      addEventListener() {}
      removeEventListener() {}
      addListener() {}
      removeListener() {}
      dispatchEvent() { return true; }
    }
    const matchMedia = (query) => new MediaQueryListShim(query);

    // ----- computed style: declared values and tag defaults, never a cascade -----
    const BLOCK_TAGS = new Set(["ADDRESS", "ARTICLE", "ASIDE", "BLOCKQUOTE", "BODY", "DD", "DIV", "DL", "DT", "FIELDSET", "FIGCAPTION", "FIGURE", "FOOTER", "FORM", "H1", "H2", "H3", "H4", "H5", "H6", "HEADER", "HR", "HTML", "LI", "MAIN", "NAV", "OL", "P", "PRE", "SECTION", "TABLE", "UL"]);
    function getComputedStyle(element) {
      note("surface", "getComputedStyle");
      const declared = {};
      if (element && element._attrs && element._attrs.has("style")) {
        for (const part of element._attrs.get("style").split(";")) {
          const at = part.indexOf(":");
          if (at > 0) declared[kebabToCamel(part.slice(0, at).trim())] = part.slice(at + 1).trim();
        }
      }
      const tag = element && element.tagName ? element.tagName : "";
      const display = declared.display || (BLOCK_TAGS.has(tag) ? "block" : tag === "HEAD" || tag === "META" || tag === "LINK" || tag === "SCRIPT" || tag === "STYLE" || tag === "TITLE" || tag === "TEMPLATE" ? "none" : tag === "TABLE" ? "table" : tag === "TR" ? "table-row" : tag === "TD" || tag === "TH" ? "table-cell" : tag === "LI" ? "list-item" : "inline");
      const api = {
        display,
        visibility: declared.visibility || "visible",
        opacity: declared.opacity || "1",
        position: declared.position || "static",
        getPropertyValue(name) { const key = kebabToCamel(String(name)); return declared[key] !== undefined ? declared[key] : key === "display" ? display : ""; },
      };
      return new Proxy(api, {
        get(target, key) {
          if (key in target || typeof key === "symbol") return Reflect.get(target, key);
          return declared[key] !== undefined ? declared[key] : "";
        },
      });
    }

    // ----- timers and frames -----
    win.setTimeout = (fn, ms, ...args) => {
      const id = timerId++;
      timers.push({ id, at: ops.op_now() + Math.max(0, Number(ms) || 0), fn, args, repeat: false });
      return id;
    };
    win.clearTimeout = (id) => { const timer = timers.find((entry) => entry.id === id); if (timer) timer.cancelled = true; };
    win.setInterval = (fn, ms, ...args) => {
      const id = timerId++;
      const every = Math.max(1, Number(ms) || 0);
      timers.push({ id, at: ops.op_now() + every, fn, args, repeat: true, every });
      return id;
    };
    win.clearInterval = (id) => win.clearTimeout(id);
    win.requestAnimationFrame = (fn) => {
      const id = rafId++;
      rafs.push({ id, fn });
      return id;
    };
    win.cancelAnimationFrame = (id) => {
      const at = rafs.findIndex((entry) => entry.id === id);
      if (at >= 0) rafs.splice(at, 1);
    };
    win.queueMicrotask = (fn) => { Promise.resolve().then(() => { try { fn(); } catch (error) { note("log", "microtask: " + (error && error.message)); } }); };
    win.requestIdleCallback = (fn) => win.setTimeout(() => { try { fn({ didTimeout: false, timeRemaining: () => 50 }); } catch (error) { note("log", "idle: " + (error && error.message)); } }, 0);
    win.cancelIdleCallback = (id) => win.clearTimeout(id);

    // Console arguments keep an Error's stack: a framework's own error report is
    // the cheapest diagnosis the lane gets, and String(error) drops it.
    const formatLogArg = (value) => {
      if (value && typeof value === "object" && typeof value.stack === "string") return value.stack;
      if (value && typeof value === "object") {
        try { return JSON.stringify(value); } catch (error) { return String(value); }
      }
      return String(value);
    };

    // ----- the window surface -----
    Object.assign(win, {
      document: doc,
      location,
      history,
      name: "",
      closed: false,
      opener: null,
      isSecureContext: true,
      crossOriginIsolated: false,
      origin: parts.origin,
      innerWidth: viewportWidth,
      innerHeight: viewportHeight,
      outerWidth: viewportWidth,
      outerHeight: viewportHeight,
      devicePixelRatio: 1,
      scrollX: 0,
      scrollY: 0,
      pageXOffset: 0,
      pageYOffset: 0,
      screenX: 0,
      screenY: 0,
      screenLeft: 0,
      screenTop: 0,
      localStorage: storage(),
      sessionStorage: storage(),
      performance: {
        now: () => now(),
        timeOrigin,
        getEntriesByType: (type) => (type === "navigation" ? [{ type: "navigate", name: page.url, entryType: "navigation", startTime: 0, duration: 0, responseEnd: navigationMs }] : []),
        getEntriesByName: () => [],
        getEntries: () => [],
        mark() {}, measure() {}, clearMarks() {}, clearMeasures() {},
      },
      crypto: {
        getRandomValues(a) {
          note("surface", "crypto.getRandomValues");
          const integer = a instanceof Int8Array || a instanceof Uint8Array || a instanceof Uint8ClampedArray || a instanceof Int16Array || a instanceof Uint16Array || a instanceof Int32Array || a instanceof Uint32Array || a instanceof BigInt64Array || a instanceof BigUint64Array;
          if (!integer) throw new DOMExceptionShim("The provided ArrayBufferView is of type '" + (a && a.constructor && a.constructor.name) + "', which is not an integer array type.", "TypeMismatchError");
          if (a.byteLength > 65536) throw new DOMExceptionShim("The ArrayBufferView's byte length (" + a.byteLength + ") exceeds the number of bytes of entropy available via this API (65536).", "QuotaExceededError");
          const hex = ops.op_random(a.byteLength);
          const bytes = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
          for (let index = 0; index < bytes.length; index++) bytes[index] = parseInt(hex.substr(index * 2, 2), 16);
          return a;
        },
        randomUUID() {
          const hex = ops.op_random(16);
          const variant = ((parseInt(hex[16], 16) & 0x3) | 0x8).toString(16);
          return hex.slice(0, 8) + "-" + hex.slice(8, 12) + "-4" + hex.slice(13, 16) + "-" + variant + hex.slice(17, 20) + "-" + hex.slice(20, 32);
        },
        subtle: SUBTLE,
      },
      fetch: (input, init) => thenable(netFetch(input, init)),
      atob, btoa,
      structuredClone: (value) => { try { return JSON.parse(JSON.stringify(value)); } catch (error) { return value; } },
      matchMedia,
      getComputedStyle,
      MutationObserver: MutationObserverShim,
      IntersectionObserver: IntersectionObserverShim,
      ResizeObserver: ResizeObserverShim,
      Event: EventShim,
      CustomEvent: CustomEventShim,
      MessageEvent: eventClass("MessageEvent", ["data", "origin", "lastEventId", "source", "ports"]),
      ErrorEvent: eventClass("ErrorEvent", ["message", "filename", "lineno", "colno", "error"]),
      UIEvent: eventClass("UIEvent", ["detail", "view"]),
      MouseEvent: eventClass("MouseEvent", ["clientX", "clientY", "screenX", "screenY", "button", "buttons", "relatedTarget"]),
      PointerEvent: eventClass("PointerEvent", ["pointerId", "pointerType", "clientX", "clientY", "button", "buttons"]),
      KeyboardEvent: eventClass("KeyboardEvent", ["key", "code", "keyCode", "which", "repeat", "altKey", "ctrlKey", "metaKey", "shiftKey"]),
      FocusEvent: eventClass("FocusEvent", ["relatedTarget"]),
      InputEvent: eventClass("InputEvent", ["data", "inputType", "isComposing"]),
      SubmitEvent: eventClass("SubmitEvent", ["submitter"]),
      TouchEvent: eventClass("TouchEvent", ["touches", "targetTouches", "changedTouches"]),
      WheelEvent: eventClass("WheelEvent", ["deltaX", "deltaY", "deltaZ", "deltaMode"]),
      DragEvent: eventClass("DragEvent", ["dataTransfer"]),
      ProgressEvent: eventClass("ProgressEvent", ["lengthComputable", "loaded", "total"]),
      StorageEvent: eventClass("StorageEvent", ["key", "oldValue", "newValue", "url", "storageArea"]),
      PopStateEvent: eventClass("PopStateEvent", ["state"]),
      HashChangeEvent: eventClass("HashChangeEvent", ["oldURL", "newURL"]),
      PageTransitionEvent: eventClass("PageTransitionEvent", ["persisted"]),
      BeforeUnloadEvent: eventClass("BeforeUnloadEvent", []),
      CloseEvent: eventClass("CloseEvent", ["code", "reason", "wasClean"]),
      PromiseRejectionEvent: eventClass("PromiseRejectionEvent", ["promise", "reason"]),
      CSS: {
        supports: () => true,
        escape: (value) => String(value).replace(/[^a-zA-Z0-9_\u00A0-\uFFFF-]/g, (ch) => "\\" + ch),
      },
      Worker: class {
        constructor(url) { note("surface", "Worker"); this._url = String(url); }
        postMessage() {} terminate() {} addEventListener() {} removeEventListener() {}
      },
      BroadcastChannel: class {
        constructor(name) { this.name = String(name); }
        postMessage() {} close() {} addEventListener() {} removeEventListener() {}
      },
      getSelection: () => ({ rangeCount: 0, toString: () => "", addRange() {}, removeAllRanges() {}, getRangeAt: () => null, collapse() {} }),
      scroll: () => {}, scrollTo: () => {}, scrollBy: () => {},
      visualViewport: { width: viewportWidth, height: viewportHeight, scale: 1, addEventListener() {}, removeEventListener() {} },
      reportError: (error) => { note("log", "reportError: " + (error && (error.message || String(error)))); },
      // WHATWG Streams, minimal but real enough to construct and wire. The lane's
      // model is synchronous: a reader that truly streams gets an immediate end,
      // and the surface is logged when touched.
      ReadableStream: class {
        constructor(source) { this._source = source || null; this.locked = false; note("surface", "ReadableStream"); }
        getReader() {
          this.locked = true;
          return { read: () => Promise.resolve({ done: true, value: undefined }), releaseLock: () => { this.locked = false; }, cancel: () => Promise.resolve(), closed: Promise.resolve() };
        }
        cancel() { return Promise.resolve(); }
        tee() { return [new win.ReadableStream(), new win.ReadableStream()]; }
        pipeTo() { return Promise.resolve(); }
        pipeThrough(transform) { return transform && transform.readable ? transform.readable : new win.ReadableStream(); }
        [Symbol.asyncIterator]() { return { next: () => Promise.resolve({ done: true, value: undefined }) }; }
      },
      WritableStream: class {
        constructor(sink) { this._sink = sink || null; this.locked = false; note("surface", "WritableStream"); }
        getWriter() {
          this.locked = true;
          const sink = this._sink;
          return {
            write(chunk) { if (sink && typeof sink.write === "function") { try { sink.write(chunk); } catch (error) { /* as logged */ } } return Promise.resolve(); },
            close() { if (sink && typeof sink.close === "function") { try { sink.close(); } catch (error) { /* as logged */ } } return Promise.resolve(); },
            abort() { return Promise.resolve(); },
            releaseLock() { /* released */ },
          };
        }
        close() { if (this._sink && typeof this._sink.close === "function") { try { this._sink.close(); } catch (error) { /* as logged */ } } return Promise.resolve(); }
        abort() { return Promise.resolve(); }
      },
      TransformStream: class {
        constructor(transformer) { this.readable = new win.ReadableStream(); this.writable = new win.WritableStream(null); this._transformer = transformer || null; note("surface", "TransformStream"); }
      },
      TextEncoderStream: class {
        constructor() { const stream = new win.TransformStream(); this.readable = stream.readable; this.writable = stream.writable; this.encoding = "utf-8"; note("surface", "TextEncoderStream"); }
      },
      TextDecoderStream: class {
        constructor() { const stream = new win.TransformStream(); this.readable = stream.readable; this.writable = stream.writable; this.encoding = "utf-8"; note("surface", "TextDecoderStream"); }
      },
      Response: class {
        constructor(body, init) {
          init = init || {};
          this.body = body == null ? "" : body;
          this.status = init.status || 200;
          this.ok = this.status >= 200 && this.status < 300;
          this.statusText = init.statusText || "";
          this.headers = new HeadersShim(init.headers || {});
          this.url = init.url || "";
          this.type = "basic";
        }
        text() { return Promise.resolve(String(this.body)); }
        json() { return Promise.resolve(JSON.parse(String(this.body || "null"))); }
        arrayBuffer() { return Promise.resolve(new TextEncoder().encode(String(this.body)).buffer); }
        clone() { return new win.Response(this.body, { status: this.status, headers: this.headers, url: this.url }); }
      },
      AbortSignal: class {
        constructor() { this.aborted = false; this.reason = undefined; this.onabort = null; this._listeners = []; }
        addEventListener(kind, handler) { if (kind === "abort") this._listeners.push(handler); }
        removeEventListener() {}
        dispatchEvent() { return true; }
        throwIfAborted() { if (this.aborted) throw this.reason; }
        static abort(reason) { const signal = new win.AbortSignal(); signal.aborted = true; signal.reason = reason; return signal; }
        static timeout(ms) {
          const signal = new win.AbortSignal();
          win.setTimeout(() => { signal.aborted = true; signal.reason = new DOMExceptionShim("signal timed out", "TimeoutError"); }, ms);
          return signal;
        }
        static any() { return new win.AbortSignal(); }
      },
      AbortController: class {
        constructor() { this.signal = new win.AbortSignal(); }
        abort(reason) {
          this.signal.aborted = true;
          this.signal.reason = reason;
          if (this.signal.onabort) { try { this.signal.onabort({ type: "abort" }); } catch (error) { /* as Chrome */ } }
          for (const handler of this.signal._listeners) { try { handler({ type: "abort" }); } catch (error) { /* as Chrome */ } }
        }
      },
      Image: class {
        constructor() { this.width = 0; this.height = 0; this.complete = true; this.naturalWidth = 0; this.naturalHeight = 0; }
        set src(value) { this._src = String(value); win.setTimeout(() => { if (typeof this.onload === "function") { try { this.onload({ type: "load", target: this }); } catch (error) { /* as Chrome */ } } }, 0); }
        get src() { return this._src || ""; }
        addEventListener() {}
        removeEventListener() {}
        decode() { return Promise.resolve(); }
      },
      Audio: class {
        constructor() { this.duration = 0; this.currentTime = 0; this.paused = true; }
        play() { this.paused = false; return Promise.resolve(); }
        pause() { this.paused = true; }
        canPlayType(type) { return canPlayType(type); }
        addEventListener() {}
      },
      MediaSource: MediaSourceShim,
      AudioContext: class {
        constructor() { this.sampleRate = 48000; this.state = "running"; this.destination = {}; }
        createOscillator() { return { type: "sine", frequency: { value: 440, setValueAtTime() {} }, connect() {}, start() {}, stop() {}, onended: null }; }
        createAnalyser() { return { fftSize: 2048, frequencyBinCount: 1024, connect() {}, getFloatFrequencyData(a) { a.fill(-100); }, getByteFrequencyData(a) { a.fill(0); } }; }
        createGain() { return { gain: { value: 1, setValueAtTime() {} }, connect() {} }; }
        createDynamicsCompressor() { return { threshold: { value: -24 }, knee: { value: 30 }, ratio: { value: 12 }, attack: { value: 0.003 }, release: { value: 0.25 }, connect() {} }; }
        createBufferSource() { return { buffer: null, connect() {}, start() {} }; }
        createBiquadFilter() { return { type: "lowpass", frequency: { value: 350 }, connect() {} }; }
        createBuffer(channels, length, rate) { return { length, sampleRate: rate, numberOfChannels: channels, getChannelData: () => new Float32Array(length) }; }
        close() { return Promise.resolve(); }
      },
      OfflineAudioContext: class {
        constructor(channels, length, rate) { this.sampleRate = rate; this.length = length; }
        createOscillator() { return { frequency: { value: 440 }, connect() {}, start() {} }; }
        createGain() { return { gain: { value: 1 }, connect() {} }; }
        createDynamicsCompressor() { return { connect() {} }; }
        startRendering() { return Promise.resolve({ getChannelData: () => new Float32Array(1024), length: 1024, sampleRate: 44100 }); }
      },
      RTCPeerConnection: class { constructor() {} createDataChannel() { return {}; } createOffer() { return Promise.resolve({ type: "offer", sdp: "v=0" }); } setLocalDescription() { return Promise.resolve(); } close() {} },
      Notification: { permission: "default", requestPermission: () => Promise.resolve("default") },
      WebSocket: class { constructor() { note("surface", "WebSocket"); } send() {} close() {} addEventListener() {} },
      EventSource: class { constructor() { note("surface", "EventSource"); } close() {} addEventListener() {} },
      PerformanceObserver: class { observe() {} disconnect() {} takeRecords() { return []; } },
      CanvasRenderingContext2D: class {}, WebGLRenderingContext: class {}, WebGL2RenderingContext: class {},
      Path2D: class { addPath() {} moveTo() {} lineTo() {} arc() {} rect() {} },
      ImageBitmap: class { constructor() { this.width = 1; this.height = 1; } close() {} },
      OffscreenCanvas: class {
        constructor(width, height) { this.width = width; this.height = height; }
        getContext(kind) { return kind === "2d" ? new Ctx2D(this) : new GL(); }
        convertToBlob() { return Promise.resolve(new Blob([])); }
        transferToImageBitmap() { return new win.ImageBitmap(); }
      },
      createImageBitmap: () => Promise.resolve({ width: 1, height: 1, close() {} }),
      FontFace: class { constructor(family, source) { this.family = family; this.status = "loaded"; } load() { return Promise.resolve(this); } },
      Screen: class {}, Navigator: class {}, Document: class {}, Window: class {},
      EventTarget: EventTargetShim, CSSStyleDeclaration: class {},
      Node: VNode, Element: VElement, HTMLElement: VElement, Text: VText, CharacterData: VText, DocumentFragment: VFragment, ShadowRoot: VShadowRoot,
      HTMLCanvasElement: VElement, SVGElement: VElement, HTMLImageElement: VElement,
      HTMLScriptElement: VElement, HTMLTemplateElement: VElement,
      customElements: { define() {}, get() { return undefined; }, whenDefined: () => Promise.resolve(), upgrade() {} },
      DOMParser: class {
        constructor() { note("surface", "DOMParser"); }
        parseFromString(text, type) {
          const parsed = JSON.parse(ops.op_parse_html(String(text), "document"));
          if (parsed && parsed.error) return doc;
          const scratch = new VDocument(page.url);
          const body = buildSpec(parsed);
          scratch.documentElement = body;
          scratch.body = body;
          return scratch;
        }
      },
      XMLSerializer: class { serializeToString(node) { let out = ""; serializeInto(node, (text) => { out += text; }, 0); return out; } },
      TextEncoder: TextEncoderShim,
      TextDecoder: TextDecoderShim,
      URL: URLShim,
      URLSearchParams: URLSearchParamsShim,
      Blob: BlobShim,
      File: class extends BlobShim {
        constructor(blobParts, name, options) { super(blobParts, options); this.name = String(name); this.lastModified = Date.now(); }
      },
      FileReader: FileReaderShim,
      FormData: FormDataShim,
      Headers: HeadersShim,
      Request: RequestShim,
      console: {
        log: (...args) => note("log", args.map(formatLogArg).join(" ")),
        warn: (...args) => note("log", args.map(formatLogArg).join(" ")),
        error: (...args) => note("log", args.map(formatLogArg).join(" ")),
        info: (...args) => note("log", args.map(formatLogArg).join(" ")),
        debug() {}, trace() {}, dir() {}, table() {}, group() {}, groupEnd() {}, time() {}, timeEnd() {},
      },
      chrome: {
        app: { isInstalled: false, InstallState: { DISABLED: "disabled" }, RunningState: { CANNOT_RUN: "cannot_run" } },
        runtime: {},
        loadTimes: () => ({ requestTime: 0, startLoadTime: 0, commitLoadTime: 0, finishDocumentLoadTime: 0, finishLoadTime: 0, firstPaintTime: 0, firstPaintAfterLoadTime: 0, navigationType: "Other", wasFetchedViaSpdy: true, wasNpnNegotiated: true, npnNegotiatedProtocol: "h2", wasAlternateProtocolAvailable: false, connectionInfo: "h2" }),
        csi: () => ({ onloadT: Date.now(), startE: Date.now() - 1200, pageT: 1200, tran: 15 }),
      },
    });

    // Tag-named element classes: `instanceof` answers true for every element (an
    // over-approximation that keeps boot paths alive; a program that needs exact
    // per-tag constructors is a measured finding, never invented here).
    for (const [tag, name] of TAG_CLASS_NAMES) {
      win[name] = VElement;
    }
    // Every standard HTML element interface name, so `x instanceof HTML...`
    // checks (React's own commit path does one) never meet an undefined
    // constructor.
    for (const name of [
      "HTMLAnchorElement", "HTMLAreaElement", "HTMLAudioElement", "HTMLBRElement", "HTMLBaseElement",
      "HTMLBodyElement", "HTMLButtonElement", "HTMLCanvasElement", "HTMLDListElement", "HTMLDataElement",
      "HTMLDataListElement", "HTMLDetailsElement", "HTMLDialogElement", "HTMLDivElement", "HTMLEmbedElement",
      "HTMLFieldSetElement", "HTMLFormElement", "HTMLHRElement", "HTMLHeadElement", "HTMLHeadingElement",
      "HTMLHtmlElement", "HTMLIFrameElement", "HTMLImageElement", "HTMLInputElement", "HTMLLIElement",
      "HTMLLabelElement", "HTMLLegendElement", "HTMLLinkElement", "HTMLMapElement", "HTMLMediaElement",
      "HTMLMenuElement", "HTMLMetaElement", "HTMLMeterElement", "HTMLModElement", "HTMLOListElement",
      "HTMLObjectElement", "HTMLOptGroupElement", "HTMLOptionElement", "HTMLOutputElement",
      "HTMLParagraphElement", "HTMLParamElement", "HTMLPictureElement", "HTMLPreElement",
      "HTMLProgressElement", "HTMLQuoteElement", "HTMLScriptElement", "HTMLSelectElement", "HTMLSlotElement",
      "HTMLSourceElement", "HTMLSpanElement", "HTMLStyleElement", "HTMLTableCaptionElement",
      "HTMLTableCellElement", "HTMLTableColElement", "HTMLTableElement", "HTMLTableRowElement",
      "HTMLTableSectionElement", "HTMLTemplateElement", "HTMLTextAreaElement", "HTMLTimeElement",
      "HTMLTitleElement", "HTMLTrackElement", "HTMLUListElement", "HTMLUnknownElement", "HTMLVideoElement",
    ]) {
      if (!(name in win)) win[name] = VElement;
    }
    // The remaining DOM interface names programs `instanceof`-check or build
    // events with; the list shapes alias Array because that is what the host's
    // query methods return.
    win.NodeList = Array;
    win.HTMLCollection = Array;
    win.DOMTokenList = Array;
    win.NamedNodeMap = Array;
    win.FileList = Array;
    win.TouchList = Array;
    win.DataTransfer = class {
      constructor() { this.files = []; this.items = []; this.types = []; this.dropEffect = "none"; this.effectAllowed = "all"; }
      setData() {} getData() { return ""; } clearData() {} setDragImage() {}
    };
    win.DOMRect = class {
      constructor(x = 0, y = 0, width = 0, height = 0) {
        this.x = x; this.y = y; this.width = width; this.height = height;
        this.top = y; this.left = x; this.right = x + width; this.bottom = y + height;
      }
      static fromRect(rect) { return new this((rect && rect.x) || 0, (rect && rect.y) || 0, (rect && rect.width) || 0, (rect && rect.height) || 0); }
    };
    win.DOMRectReadOnly = win.DOMRect;
    win.Range = class {
      constructor() { this.startContainer = null; this.endContainer = null; this.startOffset = 0; this.endOffset = 0; this.collapsed = true; }
      setStart() {} setEnd() {} collapse() {} selectNodeContents() {}
      getBoundingClientRect() { return new win.DOMRect(); }
      getClientRects() { return []; }
      toString() { return ""; }
    };
    win.Selection = class {};
    win.ClipboardEvent = eventClass("ClipboardEvent", ["clipboardData"]);
    win.AnimationEvent = eventClass("AnimationEvent", ["animationName", "elapsedTime"]);
    win.TransitionEvent = eventClass("TransitionEvent", ["propertyName", "elapsedTime"]);
    win.Touch = class { constructor(init) { init = init || {}; this.identifier = init.identifier || 0; this.clientX = init.clientX || 0; this.clientY = init.clientY || 0; this.target = init.target || null; } };
    win.ResizeObserverEntry = class {};
    win.IntersectionObserverEntry = class {};

    // navigator and screen come from the identity's own measured view, so the
    // page's program never sees a fact that contradicts the wire.
    const navigator = {
      userAgent: ident.userAgent, appVersion: ident.appVersion, appName: "Netscape",
      appCodeName: "Mozilla", platform: ident.platform, vendor: ident.vendor, vendorSub: "",
      product: "Gecko", productSub: "20030107", language: ident.languages[0], languages: ident.languages.slice(),
      hardwareConcurrency: ident.hardwareConcurrency, deviceMemory: 8, maxTouchPoints: ident.maxTouchPoints, cookieEnabled: true,
      geolocation: { getCurrentPosition() {}, watchPosition() { return 1; }, clearWatch() {} },
      sendBeacon: (url, data) => { try { netFetch(url, { method: "POST", body: data, mode: "no-cors", credentials: "include" }); } catch (error) {} return true; },
      onLine: true, webdriver: false, doNotTrack: null,
      plugins: pdfPlugins(ident.pdfViewer).plugins,
      mimeTypes: pdfPlugins(ident.pdfViewer).mimeTypes,
      userAgentData: {
        brands: ident.brands.map((b) => ({ brand: b.brand, version: b.version })),
        mobile: ident.mobile, platform: ident.uaPlatform,
        getHighEntropyValues(hints) {
          const major = (/Chrome\/(\d+)/.exec(ident.userAgent) || [])[1] || "0";
          const full = major + ".0.0.0";
          const all = {
            architecture: "x86", bitness: "64", model: "", mobile: ident.mobile, platform: ident.uaPlatform,
            platformVersion: ({ Windows: "15.0.0", macOS: "15.5.0", Android: "14.0.0", Linux: "6.8.0" })[ident.uaPlatform] || "",
            uaFullVersion: full, wow64: false, formFactors: [ident.mobile ? "Mobile" : "Desktop"],
            fullVersionList: ident.brands.map((b) => ({ brand: b.brand, version: /^\d+$/.test(b.version) && b.version === major ? full : b.version + ".0.0.0" })),
          };
          const out = { brands: this.brands, mobile: ident.mobile, platform: ident.uaPlatform };
          for (const hint of Array.from(hints || [])) if (hint in all) out[hint] = all[hint];
          return Promise.resolve(out);
        },
        toJSON() { return { brands: this.brands, mobile: this.mobile, platform: this.platform }; },
      },
      connection: { effectiveType: "4g", rtt: 50, downlink: 10, saveData: false },
      permissions: { query: (description) => Promise.resolve({ state: "prompt", name: description && description.name, onchange: null }) },
      getBattery: () => Promise.resolve({ charging: true, chargingTime: 0, dischargingTime: Infinity, level: 1, onchargingchange: null, onlevelchange: null, addEventListener() {}, removeEventListener() {} }),
      mediaDevices: { enumerateDevices: () => Promise.resolve([]), getUserMedia: () => Promise.reject(new Error("NotAllowedError")) },
      storage: { estimate: () => Promise.resolve({ quota: 300000000000, usage: 0 }), persisted: () => Promise.resolve(false) },
      pdfViewerEnabled: ident.pdfViewer,
      webkitTemporaryStorage: { queryUsageAndQuota: (callback) => callback(0, 300000000000) },
    };
    win.navigator = navigator;
    win.screen = Object.assign({ colorDepth: 24, pixelDepth: 24, availLeft: 0, availTop: 0 }, ident.screen, { orientation: { type: ident.mobile ? "portrait-primary" : "landscape-primary", angle: 0 } });
    win.window = win;
    win.self = win;
    win.top = win;
    win.parent = win;
    win.frames = win;
    win.addEventListener = (type, handler, options) => {
      if (typeof handler !== "function" && !(handler && typeof handler.handleEvent === "function")) return;
      if (!listeners.has(win)) listeners.set(win, new Map());
      const map = listeners.get(win);
      if (!map.has(type)) map.set(type, []);
      map.get(type).push({ handler, once: !!(options && options.once) });
    };
    win.removeEventListener = (type, handler) => {
      const map = listeners.get(win);
      if (!map || !map.has(type)) return;
      map.set(type, map.get(type).filter((entry) => entry.handler !== handler));
    };
    win.dispatchEvent = (event) => dispatchOn(win, event);

    // window.location = "..." is a navigation, as in a browser.
    Object.defineProperty(win, "location", { configurable: true, get() { return location; }, set(value) { navigateTo(String(value)); } });
    Object.defineProperty(doc, "defaultView", { configurable: true, get() { return win; } });

    win.XMLHttpRequest = class {
      constructor() { this.readyState = 0; this.status = 0; this.responseText = ""; this._headers = {}; this.onreadystatechange = null; this.onload = null; this.onerror = null; this.responseType = ""; this.withCredentials = false; }
      open(method, url) { this._method = method; this._url = url; this.readyState = 1; }
      setRequestHeader(name, value) { this._headers[name] = value; }
      addEventListener(type, handler) { this["on" + type] = handler; }
      send(body) {
        try {
          const response = netFetch(this._url, { method: this._method, headers: this._headers, body, credentials: this.withCredentials ? "include" : "same-origin" });
          this.status = response.status;
          this.responseText = response.textSync;
          this._responseHeaders = response.rawHeaders;
          this.responseURL = response.url;
          this.readyState = 4;
          if (this.responseType === "json") { try { this.response = JSON.parse(this.responseText); } catch (error) { this.response = null; } }
          else if (this.responseType === "arraybuffer") { this.response = response.bytesSync().slice().buffer; }
          else if (this.responseType === "blob") { this.response = new BlobShim([response.bytesSync()], { type: response.headers.get("content-type") || "" }); }
          else { this.response = this.responseText; }
          if (this.onreadystatechange) this.onreadystatechange();
          if (this.onload) this.onload();
        } catch (error) {
          this.readyState = 4;
          this.status = 0;
          if (this.onerror) this.onerror();
        }
      }
      abort() {}
      getResponseHeader(name) {
        const header = (this._responseHeaders || []).find(([key]) => String(key).toLowerCase() === String(name).toLowerCase());
        return header ? header[1] : null;
      }
      getAllResponseHeaders() { return (this._responseHeaders || []).map(([key, value]) => key + ": " + value).join("\r\n"); }
    };
    // Attach the content environment to every node created from here on.
    env.win = win;

    return env;
  }

  // The pump: settle microtasks, run timers due inside a small real-time window,
  // flush animation frames, and decide from the contract. The run ends on
  // content (visible text at the target), quiescence (nothing mutating, no timer
  // due soon), or the lane's wall clock. Never on a guess about rendering.
  function contentPump(env) {
    const doc = env.doc;
    const target = Math.max(1, Number(M.page.textTarget) || 200);
    const deadline = Math.max(200, Number(M.page.deadlineMs) || 2500);
    const overBudget = () => ops.op_now() - M.started >= deadline;
    let quiet = 0;
    for (let round = 0; round < 600; round++) {
      if (overBudget()) return "budget";
      const mutationsBefore = doc._mutations;
      const requestsBefore = M.transcript.length;
      settle();
      runTimers(env);
      flushFrames(env);
      settle();
      if (visibleText(doc).length >= target) return "content";
      const mutated = doc._mutations !== mutationsBefore || M.transcript.length !== requestsBefore;
      const dueSoon = env.timers.some((timer) => !timer.cancelled && timer.at - ops.op_now() <= 1000);
      if (!mutated && !dueSoon) {
        if (++quiet >= 3) return "quiescent";
      } else quiet = 0;
    }
    return "budget";
  }

  function runTimers(env) {
    for (let fired = 0; fired < 500; fired++) {
      if (ops.op_now() - M.started >= (Number(M.page.deadlineMs) || 2500)) return;
      const live = env.timers.filter((timer) => !timer.cancelled).sort((a, b) => a.at - b.at);
      if (!live.length) return;
      const next = live[0];
      const wait = next.at - ops.op_now();
      if (wait > 250) return;
      if (wait > 0) ops.op_sleep(Math.ceil(wait));
      const due = env.timers.filter((timer) => !timer.cancelled && timer.at <= ops.op_now() + 0.5);
      for (const timer of due) {
        if (timer.repeat) timer.at = ops.op_now() + timer.every;
        else timer.cancelled = true;
        try { timer.fn.apply(null, timer.args); } catch (error) { note("log", "timer: " + (error && (error.stack || error.message))); }
        settle();
      }
    }
  }

  function flushFrames(env) {
    const pending = env.rafs.splice(0, env.rafs.length);
    for (const entry of pending) {
      try { entry.fn(ops.op_now()); } catch (error) { note("log", "raf: " + (error && error.message)); }
      settle();
    }
  }

  globalThis.__v5Content = function () {
    try {
      try { delete globalThis.Deno; } catch (error) { /* checked below */ }
      if (typeof globalThis.Deno !== "undefined") throw new Error("engine: Deno is reachable from the page");
      const page = JSON.parse(ops.op_page());
      guardWebAssembly(page.wasmBudget || 128 * 1024 * 1024);
      M = {
        jar: new Map(), surfaces: new Set(), logs: [], loader: null, loaded: new Set(),
        started: ops.op_now(), page, mode: "content", transcript: [], transcriptBytes: 0, transcriptRequestBytes: 0,
      };
      try {
        const pageHost = new URL(page.url).hostname;
        for (const pair of String(page.cookie || "").split(";")) {
          const at = pair.indexOf("=");
          if (at > 0) jarFor(pageHost).set(pair.slice(0, at).trim(), { value: pair.slice(at + 1).trim(), expires: null, path: "/", secure: false, httpOnly: false });
        }
      } catch (error) { note("log", "seed jar: " + (error && error.message)); }

      const env = makeContentEnvironment(page);
      M.doc = env.doc;
      // The preload pass runs on the lane's own threads, underneath this run;
      // the host waits per URL through op_prefetch_wait.
      M.prefetch = !!page.prefetch;
      note("log", "prefetch " + (M.prefetch ? "on" : "off"));
      env.runScript = (element) => {
        const type = (element.getAttribute("type") || "").toLowerCase();
        if (type === "module") {
          note("surface", "script.module");
          const src = element.getAttribute("src");
          if (src) {
            let absolute = src;
            try { absolute = new URL(src, M.page.url).href; } catch (error) { /* as given */ }
            importAbsolute(absolute);
          } else if ((element.textContent || "").trim()) {
            evaluateModuleSource(element.textContent, M.page.url + "#module-" + (++INLINE_MODULE));
          }
          if (element.onload) { try { element.onload(); } catch (error) { note("log", "script onload: " + (error && error.message)); } }
          return;
        }
        if (type && !/^(text|application)\/(javascript|ecmascript)$/.test(type)) { note("log", "script type " + type + " skipped"); return; }
        const src = element.getAttribute("src");
        if (src) {
          let absolute = src;
          try { absolute = new URL(src, M.page.url).href; } catch (error) { /* as given */ }
          if (M.loaded.has(absolute)) return;
          M.loaded.add(absolute);
          const parsed = loadScript(absolute);
          note("log", "script " + absolute.slice(0, 70) + " -> " + (parsed.error || parsed.status) + " " + String(parsed.body || "").length);
          if (!parsed.error) { M.doc.currentScript = element; runScript(parsed.body, absolute); M.doc.currentScript = null; }
          if (element.onload) { try { element.onload(); } catch (error) { note("log", "script onload: " + (error && error.message)); } }
        } else if ((element.textContent || "").trim()) {
          M.doc.currentScript = element;
          runScript(element.textContent, M.page.url);
          M.doc.currentScript = null;
        }
      };

      // Build the document tree (html5ever parsed it on the Rust side).
      const tree = JSON.parse(ops.op_parse_html(page.html, "document"));
      if (tree && tree.error) throw new Error(tree.error);
      const build = (spec) => {
        if (typeof spec === "string") return env.doc.createTextNode(spec);
        const element = env.doc.createElement(spec[0]);
        const attributes = spec[1] || {};
        for (const name of Object.keys(attributes)) element._attrs.set(name, String(attributes[name]));
        for (let index = 2; index < spec.length; index++) {
          const child = build(spec[index]);
          if (child) { child.parentNode = element; element.childNodes.push(child); }
        }
        return element;
      };
      const html = build(tree);
      if (html.tagName !== "HTML") throw new Error("engine: parsed document has no html element");
      const doc = env.doc;
      doc.documentElement = html;
      html.parentNode = doc;
      doc.childNodes = [html];
      doc.head = html.childNodes.find((node) => node.nodeType === 1 && node.tagName === "HEAD") || null;
      if (!doc.head) {
        doc.head = doc.createElement("head");
        doc.head.parentNode = html;
        html.childNodes.unshift(doc.head);
      }
      doc.body = html.childNodes.find((node) => node.nodeType === 1 && node.tagName === "BODY") || null;
      if (!doc.body) {
        doc.body = doc.createElement("body");
        doc.body.parentNode = html;
        html.childNodes.push(doc.body);
      }
      doc.scripts = [];
      (function walk(node) {
        for (const child of node.childNodes) {
          if (child.nodeType !== 1) continue;
          if (child.tagName === "SCRIPT") doc.scripts.push(child);
          walk(child);
        }
      })(html);
      const titleElement = doc.head.getElementsByTagName("title")[0];
      doc.title = titleElement ? titleElement.textContent : "";

      // Install the host on the real global so page scripts see it.
      for (const key of Object.keys(env.win)) {
        if (key === "globalThis" || key === "window") continue;
        globalThis[key] = env.win[key];
      }
      globalThis.window = globalThis;
      globalThis.self = globalThis;
      globalThis.top = globalThis;
      globalThis.parent = globalThis;
      globalThis.frames = globalThis;
      globalThis.document = doc;
      globalThis.navigator = env.win.navigator;
      globalThis.location = env.win.location;
      globalThis.history = env.win.history;
      globalThis.performance = env.win.performance;
      globalThis.crypto = env.win.crypto;
      globalThis.fetch = env.win.fetch;
      globalThis.XMLHttpRequest = env.win.XMLHttpRequest;
      globalThis.matchMedia = env.win.matchMedia;
      globalThis.getComputedStyle = env.win.getComputedStyle;
      globalThis.MutationObserver = env.win.MutationObserver;
      globalThis.IntersectionObserver = env.win.IntersectionObserver;
      globalThis.ResizeObserver = env.win.ResizeObserver;
      globalThis.Event = env.win.Event;
      globalThis.CustomEvent = env.win.CustomEvent;
      globalThis.chrome = env.win.chrome;

      // The contract decides before any program runs: a document whose own text
      // already passes needs no JS, and every request it would have made is a
      // cost paid for nothing.
      const startTarget = Math.max(1, Number(page.textTarget) || 200);
      let stop;
      if (visibleText(doc).length >= startTarget) {
        stop = "content";
        note("log", "contract met on the served document, no program run");
      } else {
      // Run the initial document's own scripts in document order; stylesheets
      // are requested where a browser would request them (before the scripts
      // that follow). Module scripts are the measured M2 gap: logged, not run.
      const order = [];
      (function walk(node) {
        for (const child of node.childNodes) {
          if (child.nodeType !== 1) continue;
          const tag = child.tagName;
          if (tag === "LINK") {
            const rel = (child.getAttribute("rel") || "").toLowerCase();
            if (rel.split(/\s+/).includes("stylesheet") && child.getAttribute("href")) order.push({ kind: "style", element: child });
          } else if (tag === "SCRIPT") {
            order.push({ kind: "script", element: child });
          }
          walk(child);
        }
      })(html);
      env.armed = true;
      // Module scripts are deferred by the spec: classic scripts run first, then
      // the module graph in document order (this whole run is post-parse, so
      // deferral is just which queue runs last).
      const moduleQueue = [];
      for (const item of order) {
        if (item.kind === "style") {
          const href = item.element.getAttribute("href");
          try {
            const response = netFetch(href, { __dest: "style", mode: "no-cors", credentials: "include", headers: { accept: "text/css,*/*;q=0.1" } });
            note("log", "style " + String(href).slice(0, 70) + " -> " + response.status);
          } catch (error) { note("log", "style " + String(href).slice(0, 70) + ": " + (error && error.message)); }
        } else if ((item.element.getAttribute("type") || "").toLowerCase() === "module") {
          moduleQueue.push(item.element);
        } else {
          env.runScript(item.element);
        }
      }
      for (const element of moduleQueue) env.runScript(element);
      settle();
      doc.readyState = "interactive";
      doc.dispatchEvent({ type: "readystatechange", bubbles: false });
      doc.dispatchEvent({ type: "DOMContentLoaded", bubbles: true });
      doc.readyState = "complete";
      env.win.dispatchEvent({ type: "load", bubbles: false });
      env.win.dispatchEvent({ type: "pageshow", bubbles: false });

      stop = contentPump(env);
      settle();
      }

      // Serialize the tree the program built. Past the cap the run is an honest
      // failure, never a truncated document.
      const cap = Number(M.page.documentCap) || 16 * 1024 * 1024;
      const pieces = ["<!DOCTYPE html>"];
      let size = 0;
      let tooLarge = false;
      serializeInto(doc.documentElement, (text) => {
        if (tooLarge) return;
        size += text.length;
        if (size > cap) { tooLarge = true; return; }
        pieces.push(text);
      }, 0);
      if (tooLarge) {
        ops.op_result(JSON.stringify({ error: "document exceeds the lane cap of " + (cap >> 20) + " MB", stop: "error" }));
        return;
      }

      const cookies = [];
      for (const [cookieHost, jar] of M.jar) {
        if (!hostMatches(env.host, cookieHost)) continue;
        for (const [name, entry] of jar) if (live(entry)) cookies.push({ name, value: entry.value, domain: cookieHost, path: entry.path, secure: entry.secure, httpOnly: entry.httpOnly, expires: entry.expires });
      }
      ops.op_result(JSON.stringify({
        document: pieces.join(""),
        stop,
        textChars: visibleText(doc).length,
        visited: env.win.location.href,
        navigate: M.navigate || null,
        transcript: M.transcript,
        cookies,
        surfaces: [...M.surfaces],
        logs: M.logs.slice(0, DEBUG ? 5000 : 200),
        ms: ops.op_now() - M.started,
      }));
    } catch (error) {
      try {
        note("log", "content lane: " + (error && (error.stack || error.message)));
        ops.op_result(JSON.stringify({ error: String(error && (error.message || error)), stop: "error", logs: M ? M.logs.slice(0, 200) : [] }));
      } catch (inner) { /* the op refused or V8 is being torn down */ }
    }
  };
})();
