// DonCore prelude — the browser-shaped environment (v0.1).
// Everything here is DEFINED ONCE per realm from the identity record.
// No detection-time patching: one coherent story, chosen in advance.
// Host functions available: __wireCall __cookieGet __cookieSet __cookieAll
//                           __log __nowMs

// ── WH (window-surface hygiene, 2026-10-18): the whole prelude body lives
// inside this closure, so engine internals are NOT own properties of the page
// window — a real browser exposes no such names. The page-facing surface and the
// host bridge are re-exported at the end of the closure.
(function () {
// which the host-side __mkNativeC wrapper reads; the closure keeps a local
// alias so internal code is unchanged.
var __NATIVES = (__DCI.natives = []);
var __DTF, __ETStore, __ElemC, __NodeC, __collectPorts, __deepCloneNode,
    __dwActive, __dwLast, __fireError, __lastXFrame, __mkRange, __mkRectList, __secrets;
// -- WH Phase 2 + ST-11 U2b-1: the host bridge lives on ONE page-invisible
// object — a GLOBAL LEXICAL binding (`let __DCI`, created by the host at
// boot), not a property of globalThis: window enumeration, `in` checks and
// own-name counts cannot see it (measured vs Chromium 154: no such name).
// Host functions and JS-side bridge entry points are properties of it.
// transient host state (nav flags, blob-URL mints, worker/lifecycle spots):
// keeps every runtime engine name off the page window (one hidden namespace).
__DCI.t = __DCI.t || {};
var __clearSrc = (typeof globalThis.__clearSource === "function") ? globalThis.__clearSource : null;
var __wireCall = __DCI.wireCall, __nowMs = __DCI.nowMs, __sha256 = __DCI.sha256,
    __sleep = __DCI.sleep, __cookieGet = __DCI.cookieGet, __cookieSet = __DCI.cookieSet,
    __cookieAll = __DCI.cookieAll, __log = __DCI.log, __mkNativeC = __DCI.mkNativeC,
    __dumpFile = __DCI.dumpFile;
var IDENT = __DCI.IDENTITY;
var PAGE_URL = __DCI.PAGE_URL || "https://example.com/";

// ── debug tracer (env-gated, DONCORE_TRACE=1): logs property misses with
//    the callsite inside the page script. Diagnostic only, zero effect
//    when __TRACE_ON is unset.
var __TRACE = !!__DCI.TRACE_ON;

// ── native-shaping factory ──────────────────────────────────────────────
// `__native(name, len, fn)` returns a native-shaped callable wrapping fn:
// a callable class instance in the engine (no bytecode), so its
// Function.prototype.toString renders as `function NAME() { [native code] }`
// exactly like a built-in — vendored quickjs is patched to V8's single-line
// native shape (stock multiline form made eval 37 chars, a PhantomJS tell).
// name/length are pinned to the V8 descriptor shape:
// {writable:false, enumerable:false, configurable:true}.
// Getters pass 'get X' as the name (V8's getter .name), giving
// `function get X() { [native code] }`. Never patch Function.prototype.toString.
function __native(name, len, fn) {
  var id = __NATIVES.push(fn) - 1;
  var f = __mkNativeC(id);
  try {
    Object.defineProperty(f, "name", { value: name, writable: false, enumerable: false, configurable: true });
    Object.defineProperty(f, "length", { value: len, writable: false, enumerable: false, configurable: true });
  } catch (e) {}
  return f;
}
// __gGet/__gSet: native-shaped element accessors plus the receiver guard —
// reading (or writing) on the prototype itself or any non-node throws
// Chrome's "Illegal invocation"; the crawler reads every probed accessor on
// the proto object and counts a non-throwing read as a lie.
function __gGet(name, fn) {
  return __native("get " + name, 0, function () {
    if (!(this && typeof this.nodeType === "number")) throw new TypeError("Illegal invocation");
    return fn.call(this);
  });
}
function __gSet(name, fn) {
  return __native("set " + name, 1, function (v) {
    if (!(this && typeof this.nodeType === "number")) throw new TypeError("Illegal invocation");
    return fn.call(this, v);
  });
}
// ST-7 receiver brands (measured: ctormacc sweep vs Chrome 154) — every member
// of the ST-7 interfaces brand-checks its receiver: a read/write/call on a
// non-instance (the prototype itself, {}, Object.create(proto), a primitive)
// throws TypeError "Illegal invocation" exactly as Chrome does. Instances
// carry __hide(this, "_s7b", "<Interface>"); __s7w wraps a member fn,
// __s7k is the inline guard; __s7B maps prototype -> tag.
var __s7B = new WeakMap();
function __s7w(P, fn) { var t = __s7B.get(P); return function () { if (t && __g(this, "_s7b") !== t) throw new TypeError("Illegal invocation"); return fn.apply(this, arguments); }; }
function __s7k(t, k) { if (__g(t, "_s7b") !== k) throw new TypeError("Illegal invocation"); }
// ST-7 (remainder 4, 2026-10-19): read the hidden Blob text store — Blob and
// File both hide `_b_txt`; null when absent (the string fallback in
// createObjectURL / FileReader still applies for non-Blob inputs).
function __blobTxt(b) { try { var t = __g(b, "_b_txt"); return t === undefined ? null : t; } catch (e) { return null; } }

  // ── ST-8 (2026-10-20): Blob parts are BYTES ─────────────────────────────
  // Measured vs Chrome 154 (blobparts probe): String(part) made a typed array
  // contribute its DECIMAL DIGITS and a nested Blob/ArrayBuffer contribute
  // "[object …]", and _b_sz counted UTF-16 units. The widget posts its work
  // stage program into a blob worker that way, so our worker got a 13-byte
  // garbage program and the work stage never started. _b_bin is the truth now.
  function __utf8Enc(s) {
    s = String(s);
    var n = s.length, out = [], i, c;
    for (i = 0; i < n; i++) {
      c = s.charCodeAt(i);
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
      else if (c >= 0xd800 && c <= 0xdbff && i + 1 < n) {
        var d = s.charCodeAt(i + 1);
        if (d >= 0xdc00 && d <= 0xdfff) { var cp = 0x10000 + ((c - 0xd800) << 10) + (d - 0xdc00); i++; out.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 63), 0x80 | ((cp >> 6) & 63), 0x80 | (cp & 63)); }
        else out.push(0xef, 0xbf, 0xbd);
      } else if (c >= 0xd800 && c <= 0xdfff) out.push(0xef, 0xbf, 0xbd);
      else out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
    return new Uint8Array(out);
  }
  // UTF-8 decode with Chrome's replacement-character behaviour on invalid or
  // truncated sequences (never throws, never reads past the end).
  function __utf8Dec(u8) {
    if (!u8 || u8.length === undefined) return "";
    var n = u8.length, out = "", i = 0;
    while (i < n) {
      var b = u8[i++], need, cp, ok = true, j;
      if (b < 0x80) { out += String.fromCharCode(b); continue; }
      else if (b >= 0xc2 && b < 0xe0) { need = 1; cp = b & 31; }
      else if (b >= 0xe0 && b < 0xf0) { need = 2; cp = b & 15; }
      else if (b >= 0xf0 && b < 0xf5) { need = 3; cp = b & 7; }
      else { out += "\ufffd"; continue; }
      if (i + need > n) { out += "\ufffd"; break; }
      for (j = 0; j < need; j++) {
        var c2 = u8[i + j];
        if (c2 < 0x80 || c2 > 0xbf) { ok = false; break; }
        cp = (cp << 6) | (c2 & 63);
      }
      if (!ok) { out += "\ufffd"; continue; }
      i += need;
      if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) { out += "\ufffd"; continue; }
      if (cp > 0xffff) { cp -= 0x10000; out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 1023)); }
      else out += String.fromCharCode(cp);
    }
    return out;
  }
  function __blobBin(o) { try { var b = __g(o, "_b_bin"); return (b && b.length !== undefined) ? b : null; } catch (e) { return null; } }
  // WebIDL BlobPart: BufferSource (ArrayBuffer / TypedArray / DataView) and
  // Blob contribute BYTES; any other object goes through USVString, exactly as
  // Chrome does (a plain [1,2,3] part becomes "1,2,3", not bytes).
  function __blobPartBytes(p) {
    try {
      if (p === null || p === undefined) return __utf8Enc(String(p));
      if (typeof p === "string") return __utf8Enc(p);
      if (typeof ArrayBuffer !== "undefined" && p instanceof ArrayBuffer) return new Uint8Array(p.slice(0));
      if (typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView && ArrayBuffer.isView(p)) return new Uint8Array(p.buffer.slice(p.byteOffset, p.byteOffset + p.byteLength));
      var inner = __blobBin(p);
      if (inner) return inner;
      return __utf8Enc(String(p));
    } catch (e) { return __utf8Enc(String(p)); }
  }
  // A Blob COPIES its parts at construction (measured), so the result is always
  // a fresh array.
  function __blobConcat(parts) {
    var list = [], total = 0, i;
    parts = parts || [];
    for (i = 0; i < parts.length; i++) { var b = __blobPartBytes(parts[i]); list.push(b); total += b.length; }
    var out = new Uint8Array(total), at = 0;
    for (i = 0; i < list.length; i++) { out.set(list[i], at); at += list[i].length; }
    return out;
  }
  function __blobSet(o, parts, ty) {
    var u8 = __blobConcat(parts);
    __hide(o, "_b_bin", u8);
    __hide(o, "_b_txt", __utf8Dec(u8));
    __hide(o, "_b_sz", u8.length);
    __hide(o, "_b_ty", ty === undefined ? "" : String(ty));
    return u8;
  }
  function __blobUuid() {
    var h = "0123456789abcdef", s = "", a = new Uint8Array(16), i;
    try {
      if (globalThis.crypto && typeof globalThis.crypto.getRandomValues === "function") globalThis.crypto.getRandomValues(a);
      else for (i = 0; i < 16; i++) a[i] = Math.floor(Math.random() * 256);
    } catch (e) { for (i = 0; i < 16; i++) a[i] = Math.floor(Math.random() * 256); }
    a[6] = (a[6] & 15) | 64; a[8] = (a[8] & 63) | 128;
    for (i = 0; i < 16; i++) { s += h[a[i] >> 4] + h[a[i] & 15]; if (i === 3 || i === 5 || i === 7 || i === 9) s += "-"; }
    return s;
  }
  // A blob: URL is served from the page's own store, never the wire.
  function __blobFetch(u, method) {
    var m = String(method || "GET").toUpperCase();
    var bin = (__DCI.t.couBin || {})[u];
    if (!bin || (m !== "GET" && m !== "HEAD")) return Promise.reject(new TypeError("Failed to fetch"));
    var ty = (__DCI.t.couTy || {})[u] || "";
    var hdrs = new Headers();
    if (ty) hdrs.append("content-type", ty);
    var body = m === "HEAD" ? "" : __utf8Dec(bin);
    var resp = {
      ok: true, status: 200, statusText: "OK", url: u, redirected: false, type: "basic",
      headers: hdrs, bodyUsed: false,
      text: function () { return Promise.resolve(body); },
      json: function () { return Promise.resolve(JSON.parse(body)); },
      arrayBuffer: function () { var b = new ArrayBuffer(bin.length); new Uint8Array(b).set(bin); return Promise.resolve(b); },
      clone: function () { return resp; }
    };
    if (typeof __TRACE !== "undefined" && __TRACE) { try { __log("BLOB-FETCH " + m + " " + u + " -> 200 (" + bin.length + "b)"); } catch (e) {} }
    return Promise.resolve(resp);
  }
// ST-7 §20: the per-realm Selection singleton slot (initialized at boot end).
var __SEL7 = null;
// TRACE-only diagnostic: catch trap-less-proxy chains (a get on a proxy
// whose handler lacks `get` falls through its TARGET natively; a chain of
// those that loops = native stack overflow, unkillable by the JS interrupt
// handler). Log every trap-less proxy creation + its creator stack.
var __PXN = 0;
var __STPN = 0;
if (__TRACE) {
  try {
    var __OSP = Object.setPrototypeOf;
    Object.setPrototypeOf = function (o, pr) {
      if (__STPN < 60 && o && pr !== undefined) {
        __STPN++;
        try {
          var isPx = false;
          try { isPx = Object.prototype.toString.call(pr) === "[object Proxy]"; } catch (e2) {}
          __dumpFile("stplog", "stp#" + __STPN + " obj=" + Object.prototype.toString.call(o) + " proto=" + Object.prototype.toString.call(pr) + (isPx ? " PROXY-PROTO" : "") + " stack=" + String(new Error().stack).split("\n").slice(1, 4).join(" < ").replace(/[\\n\\r]/g, " ").slice(0, 240) + "\n");
        } catch (e) {}
      }
      return __OSP(o, pr);
    };
  } catch (e) {}
  try {
    (function () {
      var OP = globalThis.Proxy;
      globalThis.Proxy = new OP(OP, {
        construct: function (t, a, nt) {
          var h = a[1];
          var getOk = false;
          try { getOk = h && typeof h.get === "function"; } catch (e) {}
          if (!getOk) {
            if (__PXN < 60) {
              __PXN++;
              try {
                __dumpFile("pxlog", "px#" + __PXN + " tg=" + Object.prototype.toString.call(a[0]) + " hkeys=" + (h ? Object.keys(h).join("|") : "null") + " stack=" + String(new Error().stack).split("\n").slice(1, 4).join(" < ").replace(/[\\n\\r]/g, " ").slice(0, 260) + "\n");
              } catch (e) {}
            }
          }
          return new t(a[0], a[1]);
        },
      });
    })();
  } catch (e) {}
}
var __missLeft = 500;
function __mkTracer(tag, __base) {
  try {
    return new Proxy(__base || Object.prototype, {
      get: function (t, k, rcv) {
        var v = Reflect.get(t, k, rcv === undefined ? t : rcv);
        if (v === undefined && typeof k === "string" && k !== "then" && k !== "toJSON" && k !== "__w" && k.indexOf("__") !== 0) {
          if (__missLeft > 0) {
            try {(globalThis.__missRing=globalThis.__missRing||[]).push(String(tag)+"."+String(k)); if (globalThis.__missRing.length>60) globalThis.__missRing.shift();} catch (e) {}
            __missLeft--;
            try {
              __log("MISS " + tag + "." + k + " | " + String(new Error().stack).split("\n").slice(1, 3).join(" >> "));
            } catch (e) {}
          }
        }
        return v;
      },
      set: function (t, k, v, rcv) { return Reflect.set(t, k, v, rcv === undefined ? t : rcv); }
    });
  } catch (e) { return Object.prototype; }
}
function __traceProto(obj, tag) {
  if (!__TRACE || !obj || (typeof obj !== "object" && typeof obj !== "function")) return;
  // CHAIN-PRESERVING: the tracer must sit BETWEEN obj and its ORIGINAL
  // prototype (a Proxy over the real parent), never REPLACE the chain with a
  // bare Object.prototype proxy — that severed the shell doc/win from their
  // event methods under TRACE and silently killed the widget boot (2026-10-17).
  try { Object.setPrototypeOf(obj, __mkTracer(tag, Object.getPrototypeOf(obj))); } catch (e) {}
}

// ───────────────────────────── events ────────────────────────────────
// ST-8 unit 7 (dyn3, measured vs Chromium 154 same-hour — sc1..sc4): handler
// CONTENT attributes compile to callable functions, like Chrome's: a parsed
// `onload="..."` attribute and `setAttribute('onload', ...)` both leave
// `typeof el.onload === "function"` (name = the attribute, one `event`
// parameter). The compiled function lives in the hidden `_sf_<name>` slot
// the materialized element accessors already read (on* names default null);
// a body that will not compile yields a function that throws the compilation
// error when invoked (the HTML spec's event-handler fallback). Without this,
// every fire site that guards `typeof el["on"+type] === "function"` — scDone/
// dynDone for script load/error events (dup5/sl2/sl3), __imgLoad for images —
// stayed dark for attribute handlers.
function __onAttrCompile(name, body) {
  try {
    return new Function("return function " + name + "(event) { " + String(body) + "\n}")();
  } catch (e) {
    return function () { throw e; };
  }
}
function __onAttrHook(el, name, raw) {
  if (raw === null || raw === undefined) {
    __hide(el, "_sf_" + name, undefined);
  } else {
    __hide(el, "_sf_" + name, __onAttrCompile(name, String(raw)));
  }
}
function __isOnAttrName(n) {
  return typeof n === "string" && n.length > 2 && n.charCodeAt(0) === 111 && n.charCodeAt(1) === 110;
}
function __elStore(o) {
  var L = __g(o, "__l");
  if (L === undefined) { L = {}; __hide(o, "__l", L); }
  return L;
}
function addEventTarget(obj) {
  obj.addEventListener = function (t, f, cap) {
    if (__TRACE) { try { (globalThis.__aelLog = globalThis.__aelLog || []).push(String(t) + "|" + String(new Error().stack).split("\n")[1]); if (globalThis.__aelLog.length > 400) globalThis.__aelLog.shift(); } catch (e) {} }
    var L = __elStore(this);
    (L[t] = L[t] || []).push(f);
    // Wave W (2026-10-24): capture registration. Resource events (load /
    // error on link/img/script) do not bubble, but Chrome runs CAPTURE
    // listeners on window/document/ancestors during the descent — the
    // CSS-gated-boot pattern (x.com: document.addEventListener('load',
    // startEntry, true) arms the app entry on the stylesheet's load) is
    // dead without it. Stored beside the normal list (target-level
    // dispatch keeps working); __fireResource walks this store.
    if (cap === true) {
      var C = __g(this, "__LC");
      if (C === undefined) { C = {}; __hide(this, "__LC", C); }
      (C[t] = C[t] || []).push(f);
    }
  };
  obj.removeEventListener = function (t, f) {
    var L = __elStore(this);
    var a = L[t] || [];
    var i = a.indexOf(f);
    if (i >= 0) a.splice(i, 1);
    var C = __g(this, "__LC");
    if (C && C[t]) {
      var j = C[t].indexOf(f);
      if (j >= 0) C[t].splice(j, 1);
    }
  };
  obj.dispatchEvent = function (e) {
    if (!e || !e.type) return true;
    var L = __elStore(this);
    var a = (L[e.type] || []).slice();
    for (var i = 0; i < a.length; i++) {
      try {
        var r = a[i].call(this, e);
        if (r && typeof r.then === "function") {
          r.then(null, function (err) {
            __log("listener-rejection [" + e.type + "]: " + (err && (err.message || err.stack) || err));
          });
        }
      } catch (err) {
        __log("listener " + e.type + ": " + err + " @@ " + String((err && err.stack) || "").split(String.fromCharCode(10)).slice(0, 4).join(" | ").slice(0, 240))
        try { if (__TRACE) __srdump("listenerstk", String(e.type) + " :: " + String(err && err.stack)); } catch (_st) {}
        try { if (__TRACE && globalThis.__RD && globalThis.__RD.length) __log("RDTAIL " + globalThis.__RD.slice(-16).join(" ").slice(0, 520)); } catch (_rd) {}
        try { if (__TRACE && globalThis.__RD && globalThis.__RD.length) __srdump("rdtail", String(err).slice(0, 80) + " :: " + globalThis.__RD.slice(-16).join(" ").slice(0, 520)); } catch (_rd2) {}
        try { if (typeof __fireError === "function") __fireError(err, this); } catch (_fe) {}
      }
    }
    return true;
  };
  return obj;
}

// ───────────────────────────── timers ────────────────────────────────
var __timers = (__DCI.timers = []);
var __timerSeq = 1;
var __rafQ = [];
var __rafSeq = 1;
function setTimeout(fn, ms) {
  var d = Number(ms);
  if (!isFinite(d) || d < 0) d = 0;
  if (__TRACE) { try { var __stT = String(new Error().stack); (globalThis.__timerLog = globalThis.__timerLog || []).push("T" + d + "|" + __stT.split("\n")[1]); if (globalThis.__timerLog.length > 400) globalThis.__timerLog.shift(); if (__stT.indexOf("<eval") >= 0 && typeof fn === "function") { try { globalThis.__WIVLOG = globalThis.__WIVLOG || []; globalThis.__WIVLOG.push("T" + d + " " + String(fn).replace(/[\n\r]/g, " ").slice(0, 420)); } catch (eL) {} } } catch (e) {} }
  var __targs = Array.prototype.slice.call(arguments, 2); __timers.push({ id: __timerSeq, fn: fn, kind: "t", ms: d, due: __nowMs() + d, args: __targs });
  return __timerSeq++;
}
function setInterval(fn, ms) {
  var d = Number(ms);
  if (!isFinite(d) || d < 0) d = 0;
  if (__TRACE) { try { var __st = String(new Error().stack); (globalThis.__timerLog = globalThis.__timerLog || []).push("I" + d + "|" + __st.split("\n")[1]); if (globalThis.__timerLog.length > 400) globalThis.__timerLog.shift(); if (__st.indexOf("<eval") >= 0 && typeof fn === "function") { try { globalThis.__WIVLOG = globalThis.__WIVLOG || []; globalThis.__WIVLOG.push("I" + d + " " + String(fn).replace(/[\n\r]/g, " ").slice(0, 420)); } catch (eL2) {} } } catch (e) {} }
  var __iargs = Array.prototype.slice.call(arguments, 2); __timers.push({ id: __timerSeq, fn: fn, kind: "i", ms: d, due: __nowMs() + d, args: __iargs });
  return __timerSeq++;
}
function clearTimeout(id) {
  for (var i = 0; i < __timers.length; i++) if (__timers[i].id === id) { __timers.splice(i, 1); return; }
}
var clearInterval = clearTimeout;
function requestAnimationFrame(fn) { __rafQ.push(fn); return __rafSeq++; }
function cancelAnimationFrame(id) {}
function queueMicrotask(fn) { try{ if(typeof __EBBS==='function'&&(globalThis.__mt=(globalThis.__mt||0)+1)<=80) __EBBS('MT'); }catch(_x){} try { fn(); } catch (e) { try{ if(typeof __EBBS==='function')__EBBS('MT-ERR '+String(e).slice(0,90)); }catch(_y){} __log("microtask: " + e); try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }

// ───────────────────────────── DOM ──────────────────────────────────
function HTMLElement() {
  if (new.target === undefined) throw new TypeError("Failed to construct 'HTMLElement': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  throw new TypeError("Failed to construct 'HTMLElement': Illegal constructor");
}
var EP = HTMLElement.prototype;
function __docErr(msg) {
  var __de = null;
  try { if (typeof DOMException === "function") { __de = new DOMException(msg, "HierarchyRequestError"); try { Error.captureStackTrace(__de); } catch (e1) {} } } catch (e0) { __de = null; }
  if (!__de) { __de = new Error(msg); __de.name = "HierarchyRequestError"; }
  return __de;
}
function TextNode() {
  if (new.target === undefined) throw new TypeError("Failed to construct 'Text': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  var __td0 = arguments[0] === undefined ? "" : String(arguments[0]);
  __mk(this);
  __hide(this, "_sf_data", __td0);
  __ns(this, "text", __td0);
}
var TP = TextNode.prototype;

Object.defineProperty(EP, "children", {
  configurable: true,
  get: __gGet("children", function () { return this.childNodes.filter(function (c) { return c.nodeType === 1; }); }),
});
Object.defineProperty(EP, "firstChild", { get: __gGet("firstChild", function () { return this.childNodes[0] || null; }), configurable: true });
Object.defineProperty(EP, "lastChild", { get: __gGet("lastChild", function () { return this.childNodes[this.childNodes.length - 1] || null; }), configurable: true });
Object.defineProperty(EP, "firstElementChild", { get: __gGet("firstElementChild", function () { return this.children[0] || null; }), configurable: true });
Object.defineProperty(EP, "lastElementChild", { get: __gGet("lastElementChild", function () { var c = this.children; return c[c.length - 1] || null; }), configurable: true });
Object.defineProperty(EP, "childElementCount", { get: __gGet("childElementCount", function () { return this.children.length; }), configurable: true });
Object.defineProperty(EP, "nextSibling", {
  configurable: true,
  get: __gGet("nextSibling", function () {
    var p = this.parentNode; if (!p) return null;
    var i = p.childNodes.indexOf(this); return p.childNodes[i + 1] || null;
  }),
});
Object.defineProperty(EP, "previousSibling", {
  configurable: true,
  get: __gGet("previousSibling", function () {
    var p = this.parentNode; if (!p) return null;
    var i = p.childNodes.indexOf(this); return i > 0 ? p.childNodes[i - 1] : null;
  }),
});
Object.defineProperty(TP, "parentNode", { get: function () { return __mr(this, "__parent") || null; }, configurable: true });
// parentElement: the parent when it is an Element, else null (spec Node
// semantics; live CF api.js reads document.body.parentElement and dies
// reading .length of undefined when this is missing).
Object.defineProperty(EP, "parentElement", { get: __gGet("parentElement", function () { var p = this.parentNode; return p && p.nodeType === 1 ? p : null; }), enumerable: true, configurable: true });
Object.defineProperty(TP, "parentElement", { get: function () { var p = __mr(this, "__parent"); return p && p.nodeType === 1 ? p : null; }, enumerable: true, configurable: true });
// attributes: a NamedNodeMap-shaped view of this.attrs (arrays with .length
// and index access; the live CF api.js reads c.attributes.length and
// attributes[i].name while assembling extraParams).
Object.defineProperty(EP, "attributes", {
  get: __gGet("attributes", function () {
    var a = __mr(this, "attrs") || {}, out = [];
    for (var k in a) { if (Object.prototype.hasOwnProperty.call(a, k)) out.push({ name: k, value: String(a[k]) }); }
    out.item = function (i) { return this[i] || null; };
    out.getNamedItem = function (n) { for (var i = 0; i < this.length; i++) if (this[i].name === n) return this[i]; return null; };
    return out;
  }),
  enumerable: true, configurable: true,
});
Object.defineProperty(TP, "nextSibling", {
  configurable: true,
  get: function () {
    var p = __mr(this, "__parent"); if (!p) return null;
    var i = p.childNodes.indexOf(this); return p.childNodes[i + 1] || null;
  },
});
Object.defineProperty(TP, "textContent", { configurable: true, get: function () { return __mr(this, "text"); }, set: function (v) { __ns(this, "text", String(v == null ? "" : v)); } });
Object.defineProperty(TP, "nodeValue", { configurable: true, get: function () { return __mr(this, "text"); }, set: function (v) { __ns(this, "text", String(v == null ? "" : v)); } });

function __realmT(n) { try { return __ownerDocOf(n) === document ? "M" : "S"; } catch (e) { return "?"; } }
function __stk2() { try { return String(new Error().stack).split(String.fromCharCode(10)).slice(2, 4).join("|").slice(0, 140); } catch (e) { return "-"; } }
function __stk7() { try { return String(new Error().stack).split(String.fromCharCode(10)).slice(2, 8).join("|").slice(0, 380); } catch (e) { return "-"; } }
function __srdump(tag, line) { try { var A = globalThis.__SRD = globalThis.__SRD || {}; var arr = A[tag] = A[tag] || []; arr.push(line); if (arr.length > 200) arr.shift(); if (typeof __dumpFile === "function") __dumpFile(tag, arr.join(String.fromCharCode(10))); } catch (e) {} return ""; }
// ST-11 U2b-5 attribute-name semantics: HTML-namespace names are lowercased
// (Chrome), SVG-namespace lookups are exact/case-sensitive. (The name/NS
// readers are completed at the Element level in the tail.)
function __attrKey(el, n) { var s = String(n); return (__g(el, "_sv_ns") === "svg") ? s : s.toLowerCase(); }
EP.getAttribute = function (n) { var v = __mr(this, "attrs")[__attrKey(this, n)]; return v === undefined ? null : v; };
EP.setAttribute = function (n, v) {
  var k = __attrKey(this, n);
  __mr(this, "attrs")[k] = String(v);
  if (__isOnAttrName(k)) __onAttrHook(this, k, String(v));
  if (__TRACE && k === "id") { try { if ((globalThis.__IDSETB = (globalThis.__IDSETB || 0) + 1) <= 80) __log("IDSET-A " + (__mr(this, "tag") || this.nodeName) + "#" + String(v).slice(0, 34) + " r=" + __realmT(this)); } catch (e) {} }
  if (__mr(this, "tag") === "canvas" && (k === "width" || k === "height")) {
    var num = parseInt(v, 10);
    this[k] = isNaN(num) ? 0 : num;
  }
  if (__mr(this, "tag") === "img" && k === "src") {
    try { __imgLoad(this); } catch (e) {}
  }
  if (__mr(this, "tag") === "iframe" && k === "src") {
    if (__TRACE) { try { __log("IFRAME-SRC " + String(v).slice(0, 120) + " connected=" + __isConnected(this)); } catch (e) {} }
    try { __iframeLoad(this); } catch (e) {}
  }
};
EP.removeAttribute = function (n) { var k = __attrKey(this, n); if (__isOnAttrName(k)) __hide(this, "_sf_" + k, undefined); delete __mr(this, "attrs")[k]; };
EP.hasAttribute = function (n) { return __mr(this, "attrs")[__attrKey(this, n)] !== undefined; };
Object.defineProperty(EP, "id", {
  enumerable: true,
  configurable: true,
  get: __gGet("id", function () { return __mr(this, "attrs").id || ""; }), set: __gSet("id", function (v) { __mr(this, "attrs").id = String(v); if (__TRACE) { try { if ((globalThis.__IDSETN = (globalThis.__IDSETN || 0) + 1) <= 120) __log("IDSET " + (__mr(this, "tag") || this.nodeName) + "#" + String(v).slice(0, 34) + " r=" + __realmT(this) + " stk=" + __stk2()); } catch (e) {} } }),
});
Object.defineProperty(EP, "className", {
  configurable: true,
  get: __gGet("className", function () { return __mr(this, "attrs")["class"] || ""; }),
  set: __gSet("className", function (v) { __mr(this, "attrs")["class"] = String(v); }),
});
Object.defineProperty(EP, "classList", {
  configurable: true,
  enumerable: true,
  get: __gGet("classList", function () {
    var self = this;
    function list() { return (__mr(self, "attrs")["class"] || "").split(/\s+/).filter(Boolean); }
    function save(a) { __mr(self, "attrs")["class"] = a.join(" "); }
    var o = {
      add: function () { var a = list(); for (var i = 0; i < arguments.length; i++) if (a.indexOf(arguments[i]) < 0) a.push(arguments[i]); save(a); },
      remove: function () { var a = list(), rm = Array.prototype.slice.call(arguments); for (var i = 0; i < rm.length; i++) a = a.filter(function (c) { return c !== rm[i]; }); save(a); },
      contains: function (c) { return list().indexOf(c) >= 0; },
      toggle: function (c) { var a = list(); var has = a.indexOf(c) >= 0; if (has) a = a.filter(function (x) { return x !== c; }); else a.push(c); save(a); return !has; },
      item: function (i) { return list()[i] || null; },
      get length() { return list().length; },
      get value() { return list().join(" "); },
      toString: function () { return list().join(" "); },
    };
    try { Object.defineProperty(o, Symbol.toStringTag, { value: "DOMTokenList", configurable: true }); } catch (e) {}
    return o;
  }),
  set: __gSet("classList", function (v) { this.className = String(v); }),
});
Object.defineProperty(EP, "dataset", {
  configurable: true,
  get: __gGet("dataset", function () {
    var self = this;
    var d = {};
    Object.keys(__mr(this, "attrs")).forEach(function (k) {
      if (k.indexOf("data-") === 0) {
        var camel = k.slice(5).replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
        Object.defineProperty(d, camel, {
          get: function () { return __mr(self, "attrs")[k]; },
          set: function (v) { __mr(self, "attrs")[k] = String(v); },
        });
      }
    });
    return d;
  }),
});
Object.defineProperty(EP, "innerHTML", {
  configurable: true,
  get: __gGet("innerHTML", function () {
    var host = __mr(this, "tag") === "template" && this.content ? this.content : this;
    if (__mr(this, "tag") === "script" || __mr(this, "tag") === "style") {
      var __s = "";
      for (var __k = 0; __k < host.childNodes.length; __k++) {
        var __n2 = host.childNodes[__k];
        __s += __n2.nodeType === 3 ? __mr(__n2, "text") : serializeNode(__n2);
      }
      return __s;
    }
    return host.childNodes.map(function (__m) { return serializeNode(__m); }).join("");
  }),
  set: __gSet("innerHTML", function (v) {
    var host = __mr(this, "tag") === "template" && this.content ? this.content : this;
    host.childNodes.length = 0;
    var nodes = parseFragment(String(v == null ? "" : v), __g(host, "_sv_ns") === "svg");
    for (var i = 0; i < nodes.length; i++) { __DCI.nc.link(nodes[i], host); host.childNodes.push(nodes[i]); }
    if (__TRACE && (host.nodeType === 11 || __realmT(host) === "S")) { try { if ((globalThis.__IHS = (globalThis.__IHS || 0) + 1) <= 200) __log("IHTML-S " + (__mr(host, "tag") || host.nodeName) + " nt=" + host.nodeType + " r=" + __realmT(host) + " sr=" + (__mr(host, "__srId", "_srId") || "-") + " n=" + nodes.length + " kids=" + host.childNodes.length + " stk=" + __stk2()); } catch (e) {} }
  }),
});
Object.defineProperty(EP, "outerHTML", { get: __gGet("outerHTML", function () { return serializeNode(this); }), set: __gSet("outerHTML", function (v) {
  var nodes = parseFragment(String(v == null ? "" : v), __g(this, "_sv_ns") === "svg");
  var p = this.parentNode;
  if (!p || !nodes) return;
  var idx = p.childNodes.indexOf(this);
  if (idx < 0) return;
  p.childNodes.splice(idx, 1);
  for (var i = 0; i < nodes.length; i++) { __DCI.nc.link(nodes[i], p); p.childNodes.splice(idx + i, 0, nodes[i]); }
}), enumerable: true, configurable: true });
Object.defineProperty(EP, "textContent", {
  configurable: true,
  get: __gGet("textContent", function () { return textContentAll(this); }),
  set: __gSet("textContent", function (v) {
    this.childNodes.length = 0;
    var t = makeText(String(v == null ? "" : v));
    __nsf(t, "parentNode", this); __ns(t, "__parent", this);
    this.childNodes.push(t);
    if (__mr(this, "tag") === "script") { try { if (__isConnected(this)) __prepScript(this); } catch (e) {} }
  }),
});
// innerText: Chrome exposes it (getters/setters on Element); sites set it
// directly and read it back. It was missing entirely — bare assignments
// became invisible expandos and reads threw as "of null" hits elsewhere.
Object.defineProperty(EP, "innerText", {
  configurable: true,
  get: __gGet("innerText", function () { var __r = (__mr(this, "tag") === "script" || __mr(this, "tag") === "style"); if (!__r) return textContentOf(this); var __o = ""; for (var __i = 0; __i < this.childNodes.length; __i++) { var __c = this.childNodes[__i]; if (__c.nodeType === 3) __o += __mr(__c, "text"); } return __o; }),
  set: __gSet("innerText", function (v) {
    this.childNodes.length = 0;
    var t = makeText(String(v == null ? "" : v));
    __nsf(t, "parentNode", this); __ns(t, "__parent", this);
    this.childNodes.push(t);
  }),
});
EP.appendChild = function (n) {
  var host = __mr(this, "tag") === "template" && this.content ? this.content : this;
  if (host.nodeType === 9) {
    if (n.nodeType === 3) { throw __docErr("Failed to execute 'appendChild' on 'Node': Nodes of type '#text' may not be inserted inside nodes of type '#document'."); }
    if (n.nodeType === 1) { for (var __di = 0; __di < host.childNodes.length; __di++) { if (host.childNodes[__di].nodeType === 1) { throw __docErr("Failed to execute 'appendChild' on 'Node': Only one element on document allowed."); } } }
  }
  if (n.parentNode) n.parentNode.removeChild(n);
  __nsf(n, "parentNode", host); __ns(n, "__parent", host);
  host.childNodes.push(n);
  if (__TRACE && (__realmT(host) === "S" || __realmT(n) === "S")) { try { if ((globalThis.__APPS = (globalThis.__APPS || 0) + 1) <= 240) __log("APP-S " + (__mr(host, "tag") || host.nodeName) + " nt=" + host.nodeType + " rH=" + __realmT(host) + " rC=" + __realmT(n) + " <- " + (__mr(n, "tag") || n.nodeName) + "#" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " kids=" + host.childNodes.length + " stk=" + __stk2()); } catch (e) {} }
  if (__TRACE && n && n.nodeType === 1 && ((__mr(n, "attrs") && __mr(n, "attrs").id) || __mr(host, "tag") === "body" || host.nodeType === 11)) { try { if ((globalThis.__APPN = (globalThis.__APPN || 0) + 1) <= 150) __log("APP " + (__mr(host, "tag") || host.nodeName) + " nt=" + host.nodeType + " r=" + __realmT(host) + " <- " + __mr(n, "tag") + "#" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " kids=" + host.childNodes.length + " stk=" + __stk2()); } catch (e) {} }
  if (__TRACE && host.nodeType === 11) { try { if ((globalThis.__SRAPP = (globalThis.__SRAPP || 0) + 1) <= 80) __log("SR-APPEND sr=" + (__mr(host, "__srId", "_srId") || "?") + " child=" + (__mr(n, "tag") || n.nodeName || "?") + " id=" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " kids=" + host.childNodes.length); } catch (e) {} } if (__TRACE && n && n.nodeType === 1 && (n.shadowRoot || (__mr(n, "attrs") && String(__mr(n, "attrs").id || "").indexOf("cf-chl-widget-") === 0))) { try { __log("WAPP parent=" + (__mr(host, "tag") || host.tagName) + " child=" + __mr(n, "tag") + " id=" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " childHasShadow=" + !!n.shadowRoot + " parentKids=" + host.childNodes.length + " parentConnected=" + __isConnected(host === this ? this : host)); } catch (e) {} }
  if (n.nodeType === 1 && __mr(n, "tag") === "script") {
    var __scode = "";
    try { __scode = String(n.textContent || __mr(n, "text") || ""); } catch (e0) { __scode = ""; }
    var __ssrc = (__mr(n, "attrs") && __mr(n, "attrs").src) || n.src || "";
    var __stype = String((__mr(n, "attrs") && __mr(n, "attrs").type) || "").toLowerCase();
    if (__TRACE) { try { globalThis.__SAZ = (globalThis.__SAZ || 0) + 1; if (globalThis.__SAZ <= 250) __log("SCRIPT-ANY #" + globalThis.__SAZ + " src=" + (__ssrc ? String(__ssrc).slice(0, 44) : "-") + " len=" + __scode.length + " type=" + __stype + " stk=" + __stk2()); } catch (e) {} }
    if (__TRACE && !__ssrc && __scode && (__stype === "" || /javascript|ecmascript/i.test(__stype))) {
      try { if ((globalThis.__SA = (globalThis.__SA || 0) + 1) <= 300) __log("SCRIPT-APPEND inline len=" + __scode.length + " stk=" + __stk2()); } catch (e) {}
      try { new Function(__scode); } catch (__se) { try { __log("SCRIPT-SYNTAX-SOFT " + String(__se.message || __se).slice(0, 120) + " stk=" + __stk2()); } catch (e2) {} }
    }
    try { if (__isConnected(this)) __prepScript(n); else __markHold(n); } catch (e) {}
  }
  try { __ifKick(n); } catch (eK) {}
  return n;
};
EP.insertBefore = function (n, ref) {
  var host = __mr(this, "tag") === "template" && this.content ? this.content : this;
  if (host.nodeType === 9) {
    if (n.nodeType === 3) { throw __docErr("Failed to execute 'insertBefore' on 'Node': Nodes of type '#text' may not be inserted inside nodes of type '#document'."); }
    if (n.nodeType === 1) { for (var __dj = 0; __dj < host.childNodes.length; __dj++) { if (host.childNodes[__dj].nodeType === 1) { throw __docErr("Failed to execute 'insertBefore' on 'Node': Only one element on document allowed."); } } }
  }
  if (n.parentNode) n.parentNode.removeChild(n);
  __nsf(n, "parentNode", host); __ns(n, "__parent", host);
  var i = ref ? host.childNodes.indexOf(ref) : -1;
  if (i < 0) host.childNodes.push(n); else host.childNodes.splice(i, 0, n);
  if (__TRACE && (__realmT(host) === "S" || __realmT(n) === "S")) { try { if ((globalThis.__INSS = (globalThis.__INSS || 0) + 1) <= 160) __log("INS-S " + (__mr(host, "tag") || host.nodeName) + " nt=" + host.nodeType + " rH=" + __realmT(host) + " rC=" + __realmT(n) + " <- " + (__mr(n, "tag") || n.nodeName) + "#" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " kids=" + host.childNodes.length + " stk=" + __stk2()); } catch (e) {} }
  if (__TRACE && n && n.nodeType === 1 && ((__mr(n, "attrs") && __mr(n, "attrs").id) || __mr(host, "tag") === "body")) { try { if ((globalThis.__APPN2 = (globalThis.__APPN2 || 0) + 1) <= 100) __log("APP-I " + (__mr(host, "tag") || host.nodeName) + " r=" + __realmT(host) + " <- " + __mr(n, "tag") + "#" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " kids=" + host.childNodes.length + " stk=" + __stk2()); } catch (e) {} }
  if (__TRACE && host.nodeType === 11) { try { if ((globalThis.__SRAPP2 = (globalThis.__SRAPP2 || 0) + 1) <= 80) __log("SR-INSAFTER sr=" + (__mr(host, "__srId", "_srId") || "?") + " child=" + (__mr(n, "tag") || n.nodeName || "?") + " id=" + ((__mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " kids=" + host.childNodes.length); } catch (e) {} }
  if (n.nodeType === 1 && __mr(n, "tag") === "script") {
    var __scode2 = "";
    try { __scode2 = String(n.textContent || __mr(n, "text") || ""); } catch (e0) { __scode2 = ""; }
    var __ssrc2 = (__mr(n, "attrs") && __mr(n, "attrs").src) || n.src || "";
    var __stype2 = String((__mr(n, "attrs") && __mr(n, "attrs").type) || "").toLowerCase();
    if (__TRACE) { try { globalThis.__SAZ = (globalThis.__SAZ || 0) + 1; if (globalThis.__SAZ <= 250) __log("SCRIPT-ANY-INS #" + globalThis.__SAZ + " src=" + (__ssrc2 ? String(__ssrc2).slice(0, 44) : "-") + " len=" + __scode2.length + " type=" + __stype2 + " stk=" + __stk2()); } catch (e) {} }
    if (__TRACE && !__ssrc2 && __scode2 && (__stype2 === "" || /javascript|ecmascript/i.test(__stype2))) {
      try { if ((globalThis.__SA = (globalThis.__SA || 0) + 1) <= 300) __log("SCRIPT-APPEND-INS inline len=" + __scode2.length + " stk=" + __stk2()); } catch (e) {}
      try { new Function(__scode2); } catch (__se2) { try { __log("SCRIPT-SYNTAX-SOFT-INS " + String(__se2.message || __se2).slice(0, 120) + " stk=" + __stk2()); } catch (e2) {} }
    }
    try { if (__isConnected(this)) __prepScript(n); else __markHold(n); } catch (e) {}
  }
  try { __ifKick(n); } catch (eK) {}
  return n;
};
EP.removeChild = function (n) {
  if (__TRACE && (__realmT(this) === "S" || (n && __realmT(n) === "S") || this.nodeType === 11)) { try { if ((globalThis.__CRM = (globalThis.__CRM || 0) + 1) <= 160) __log("CREM " + (__mr(this, "tag") || this.nodeName) + " nt=" + this.nodeType + " r=" + __realmT(this) + " <- " + ((n && (__mr(n, "tag") || n.nodeName)) || "?") + "#" + ((n && __mr(n, "attrs") && __mr(n, "attrs").id) || "-") + " stk=" + __stk2()); } catch (e) {} }
  var i = this.childNodes.indexOf(n);
  if (i >= 0) { this.childNodes.splice(i, 1); __nsf(n, "parentNode", null); __ns(n, "__parent", null); }
  return n;
};
EP.replaceChild = function (n, old) { this.insertBefore(n, old); this.removeChild(old); return old; };
EP.contains = function (n) {
  var c = n;
  while (c) { if (c === this) return true; c = c.parentNode; }
  return false;
};
EP.getElementsByTagName = function (tag) {
  var want = String(tag).toLowerCase();
  var out = [];
  (function walk(n) {
    for (var i = 0; i < n.childNodes.length; i++) {
      var c = n.childNodes[i];
      if (c.nodeType === 1) {
        if (want === "*" || __mr(c, "tag") === want) out.push(c);
        walk(c);
      }
    }
  })(this);
  return out;
};
EP.getElementsByClassName = function (cls) {
  var out = [];
  var want = String(cls).split(/\s+/).filter(Boolean);
  (function walk(n) {
    for (var i = 0; i < n.childNodes.length; i++) {
      var c = n.childNodes[i];
      if (c.nodeType === 1) {
        var have = (__mr(c, "attrs")["class"] || "").split(/\s+/);
        var ok = want.every(function (w) { return have.indexOf(w) >= 0; });
        if (ok) out.push(c);
        walk(c);
      }
    }
  })(this);
  return out;
};
EP.querySelector = function (sel) {
  var l = selectorAll(this, sel);
  if (__TRACE && String(sel).charAt(0) === "#" && this.nodeType === 11) { try { __log("SHADOW-QS sr=" + (__mr(this, "__srId", "_srId") || "?") + " host=" + ((this.host && ((__mr(this.host, "attrs") && __mr(this.host, "attrs").id) || __mr(this.host, "tag"))) || "?") + " " + String(sel) + " -> " + (l[0] ? "FOUND" : "NULL") + " kids=" + this.childNodes.length + " ids=[" + (function (n) { var o = []; for (var i = 0; i < n.childNodes.length; i++) { var c = n.childNodes[i]; if (c.nodeType === 1) o.push(String(__mr(c, "attrs") && __mr(c, "attrs").id)); } return o.join(","); })(this) + "] stk=" + __stk7() + __srdump("shadowq", "sr=" + (__mr(this, "__srId", "_srId") || "?") + " " + String(sel) + " " + (l[0] ? "FOUND" : "NULL") + " kids=" + this.childNodes.length + " stk=" + __stk7())); } catch (e) {} }
  if (__TRACE && (String(sel) === ".main-wrapper" || String(sel) === ".main-content") && (globalThis.__QSM = (globalThis.__QSM || 0) + 1) <= 200) { try { __log("QSMAIN " + String(sel) + " on=" + (__mr(this, "tag") || this.nodeName) + " nt=" + this.nodeType + " r=" + __realmT(this) + " sr=" + (__mr(this, "__srId", "_srId") || "-") + " -> " + (l[0] ? "FOUND" : "NULL") + " kids=" + this.childNodes.length + " stk=" + __stk2()); } catch (e) {} }
  if (__TRACE && !l[0] && String(sel).charAt(0) === "#" && this.nodeType !== 11) { try { if ((globalThis.__QMISS = (globalThis.__QMISS || 0) + 1) <= 400) { __log("QMISS sel=" + String(sel) + " on=" + (__mr(this, "tag") || this.nodeName) + " r=" + __realmT(this) + " stk=" + __stk7()); __srdump("qmiss", "sel=" + String(sel) + " on=" + (__mr(this, "tag") || this.nodeName) + " r=" + __realmT(this) + " stk=" + __stk7()); } } catch (e) {} }
  return l[0] || null;
};
EP.querySelectorAll = function (sel) { var r = selectorAll(this, sel); return __DCI.__U6NL ? __DCI.__U6NL(r) : r; }; // U2b-6: NodeList
EP.closest = function (sel) {
  var n = this;
  while (n && n.nodeType === 1) { if (matchesSimple(n, sel)) return n; n = n.parentNode; }
  return null;
};
EP.matches = function (sel) { return matchesSimple(this, sel); };
// Node.prototype.isConnected: the turnstile watchcat gates its liveness meow
// on wrapper.isConnected; a missing property reads as undefined -> "not
// connected" -> the widget is declared missing and the flow dies silently.
Object.defineProperty(EP, "isConnected", {
  get: __gGet("isConnected", function () { return __isConnected(this); }),
  configurable: true,
});

// Node.prototype.ownerDocument: every node knows the document of its tree.
// Chrome fixes a node's document at creation and adopts it on insertion; we
// mirror that with a parent/host walk (post-insert truth) plus a creation
// stamp (__odoc, written by the per-document factories) for detached nodes.
// reCAPTCHA's Closure layer resolves nodes with
//   doc = node.nodeType == 9 ? node : (node.ownerDocument || node.document)
// and an undefined here crashed its script (reddit's rotated challenge).
function __ownerDocOf(n) {
  if (!n || n.nodeType === 9) return null;
  var p = __mr(n, "__parent") || n.parentNode || n.host || __g(n, "_sf_host") || null, st;
  while (p) {
    if (p.nodeType === 9) return p;
    if (st === undefined && __mr(p, "__odoc") !== undefined && __mr(p, "__odoc") !== null) st = __mr(p, "__odoc");
    p = __mr(p, "__parent") || p.parentNode || p.host || __g(p, "_sf_host") || null;
  }
  if (st !== undefined) return st;
  if (__mr(n, "__odoc") !== undefined && __mr(n, "__odoc") !== null) return __mr(n, "__odoc");
  return document;
}
[EP, TP].forEach(function (P) {
  try { Object.defineProperty(P, "ownerDocument", { get: __gGet("ownerDocument", function () { return __ownerDocOf(this); }), configurable: true }); } catch (e) {}
});
EP.getBoundingClientRect = function () {
  return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
};
EP.getClientRects = function () { return []; };
EP.scrollIntoView = function () {};
EP.focus = function () { if (document.activeElement !== this) __hide(document, "_rh_activeElement", this); };
EP.blur = function () {};
EP.click = function () { this.dispatchEvent({ type: "click", target: this, preventDefault: function () {}, stopPropagation: function () {} }); };
EP.setPointerCapture = function () {};
EP.addEventListener = addEventTarget({}).addEventListener;
EP.removeEventListener = addEventTarget({}).removeEventListener;
EP.dispatchEvent = addEventTarget({}).dispatchEvent;
EP.attachShadow = function (init) {
  // a real shadow root: full element tree with working selectors. The
  // turnstile widget builds its UI inside one and queries it back.
  var sr = makeElement("#shadow-root");
  __hide(sr, "_sf_nodeType", 11);
  __hide(sr, "_sf_tagName", "#shadow-root");
  __hide(sr, "_sf_nodeName", "#shadow-root");
  sr.host = this;
  sr.mode = (init && init.mode) || "open";
  __ns(sr, "__odoc", __ownerDocOf(this));
  __ns(sr, "__srId", (__DCI.t.srSeq = (__DCI.t.srSeq || 0) + 1), "_srId");
  if (__TRACE) { try { __log("SR-ATTACH id=" + __mr(sr, "__srId", "_srId") + " host=" + ((__mr(this, "attrs") && __mr(this, "attrs").id) || __mr(this, "tag") || "?") + " mode=" + sr.mode + " priorRoot=" + (this.shadowRoot ? "yes" : "no") + " r=" + __realmT(this) + " stk=" + __stk7() + __srdump("srattach", "id=" + __mr(sr, "__srId", "_srId") + " host=" + ((__mr(this, "attrs") && __mr(this, "attrs").id) || __mr(this, "tag") || "?") + " mode=" + sr.mode + " realm=" + __realmT(this) + " stk=" + __stk7())); } catch (e) {} }
  // instanceof fidelity: the turnstile watchcat gates its liveness meow on
  // `shadowRoot instanceof ShadowRoot`; put the bank's ShadowRoot.prototype
  // into the chain (it inherits the element methods through EP below).
  try { if (globalThis.ShadowRoot && globalThis.ShadowRoot.prototype) Object.setPrototypeOf(sr, globalThis.ShadowRoot.prototype); } catch (e) {}
  // §16 leaves Element.prototype.shadowRoot getter-only (Chrome truth); the
  // materialized getter reads the hidden store, so a plain assignment is a
  // silent no-op and read-back returns "". Store where the getter looks;
  // closed mode reads back null (Chrome) while still returning sr.
  __hide(this, "_sf_shadowRoot", sr.mode === "closed" ? null : sr);
  return sr;
};
EP.remove = function () { if (this.parentNode) this.parentNode.removeChild(this); };
EP.replaceWith = function () { if (this.parentNode) { this.parentNode.insertBefore(arguments[0], this); this.parentNode.removeChild(this); } };
EP.before = function () { if (this.parentNode) for (var i = 0; i < arguments.length; i++) this.parentNode.insertBefore(arguments[i], this); };
EP.after = function () {
  if (!this.parentNode) return;
  var p = this.parentNode, ref = this.nextSibling;
  for (var i = 0; i < arguments.length; i++) p.insertBefore(arguments[i], ref);
};
EP.append = function () { for (var i = 0; i < arguments.length; i++) this.appendChild(arguments[i]); };
EP.prepend = function () { var ref = this.firstChild; for (var i = 0; i < arguments.length; i++) this.insertBefore(arguments[i], ref); };
EP.hasChildNodes = function () { return this.childNodes.length > 0; };
EP.cloneNode = function (deep) {
  if (__TRACE) { try { if ((globalThis.__CLN = (globalThis.__CLN || 0) + 1) <= 100) __log("CLONE " + (__mr(this, "tag") || this.nodeName) + " nt=" + this.nodeType + " deep=" + !!deep + " r=" + __realmT(this) + " stk=" + __stk2()); } catch (e) {} }
  var c = makeElement(__mr(this, "tag"), Object.assign({}, __mr(this, "attrs")), __g(this, "_sv_ns") === "svg" ? "svg" : (__g(this, "_sv_ns") === "math" ? "math" : undefined));
  __ns(c, "__odoc", __ownerDocOf(this));
  c.innerHTML = this.innerHTML;
  if (deep) for (var i = 0; i < this.childNodes.length; i++) c.appendChild(this.childNodes[i].cloneNode ? this.childNodes[i].cloneNode(true) : makeText(__mr(this.childNodes[i], "text") || ""));
  return c;
};
EP.getElementsByName = function () { return []; };
EP.animate = function () { return { cancel: function () {}, finished: Promise.resolve(), play: function () {}, pause: function () {} }; };

// per-tag constructors so `x instanceof HTMLDivElement` holds
var TAG_CTORS = {};
var __TAG_NAME_SPECIAL = { a: "HTMLAnchorElement", img: "HTMLImageElement", br: "HTMLBRElement", hr: "HTMLHRElement", p: "HTMLParagraphElement", li: "HTMLLIElement", ol: "HTMLOListElement", ul: "HTMLUListElement", dl: "HTMLDListElement", td: "HTMLTableCellElement", th: "HTMLTableCellElement", tr: "HTMLTableRowElement", caption: "HTMLTableCaptionElement", iframe: "HTMLIFrameElement", textarea: "HTMLTextAreaElement", blockquote: "HTMLQuoteElement", fieldset: "HTMLFieldSetElement", h1: "HTMLHeadingElement", h2: "HTMLHeadingElement", h3: "HTMLHeadingElement", h4: "HTMLHeadingElement", h5: "HTMLHeadingElement", h6: "HTMLHeadingElement", main: "HTMLElement", nav: "HTMLElement", header: "HTMLElement", footer: "HTMLElement", section: "HTMLElement", article: "HTMLElement", aside: "HTMLElement" };
function __tagCtorName(tag) {
  if (!tag || tag.charAt(0) === "#") return null;
  var nm = __TAG_NAME_SPECIAL[tag];
  if (!nm) {
    if (tag.indexOf("-") >= 0) nm = "HTMLUnknownElement";
    else nm = "HTML" + tag.charAt(0).toUpperCase() + tag.slice(1) + "Element";
  }
  return nm;
}
function __nameTagCtor(C, tag) {
  var nm = __tagCtorName(tag);
  if (!nm) return;
  try { Object.defineProperty(C, "name", { value: nm, configurable: true }); } catch (e) {}
}
function tagCtor(tag) {
  if (TAG_CTORS[tag]) return TAG_CTORS[tag];
  var tnm = __tagCtorName(tag);
  var C = tnm ? function () {
    if (new.target === undefined) throw new TypeError("Failed to construct '" + tnm + "': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
    throw new TypeError("Failed to construct '" + tnm + "': Illegal constructor");
  } : function () {};
  C.prototype = Object.create(EP);
  TAG_CTORS[tag] = C;
  __nameTagCtor(C, tag);
  return C;
}
function makeElement(tag, attrs, ns) {
  var n;
  if (ns === "svg") {
    // ST-11 U2b-5: SVG-namespace construction — the tag arrives with the
    // parser's case adjustments already applied and is kept as-is
    // (tagName/localName); the class comes from the measured SVG map.
    tag = String(tag);
    var __scn = (SVGNS && SVGNS[tag]) || "SVGElement";
    var __scl = globalThis[__scn] || globalThis.SVGElement;
    n = Object.create(__scl.prototype);
    __mk(n);
    __hide(n, "_sf_nodeType", 1);
    __ns(n, "tag", tag);
    __hide(n, "_sf_tagName", tag);
    __hide(n, "_sf_localName", tag);
    __hide(n, "_sf_prefix", null);
    __hide(n, "_sf_namespaceURI", "http://www.w3.org/2000/svg");
    __hide(n, "_sv_ns", "svg");
  } else {
    tag = String(tag).toLowerCase();
    n = Object.create(tagCtor(tag).prototype);
    __mk(n);
    __hide(n, "_sf_nodeType", 1);
    __ns(n, "tag", tag);
    __hide(n, "_sf_tagName", ns === "math" ? tag : tag.toUpperCase());
    __hide(n, "_sf_localName", tag);
    __hide(n, "_sf_prefix", null);
    __hide(n, "_sf_namespaceURI", ns === "math" ? "http://www.w3.org/1998/Math/MathML" : "http://www.w3.org/1999/xhtml");
    __hide(n, "_sv_ns", ns === "math" ? "math" : "html");
  }
  __ns(n, "attrs", attrs || {});  __ns(n, "attrs", attrs || {});
  // ST-8 unit 7 (dyn3): handler content attributes compile at creation — a
  // markup onload=/onerror= (or one created through innerHTML) reads back as
  // a callable function like Chrome's (measured sc1/sc3: typeof "function").
  for (var __hak in __mr(n, "attrs")) {
    if (__isOnAttrName(__hak)) __onAttrHook(n, __hak, __mr(n, "attrs")[__hak]);
  }
  __hide(n, "_sf_childNodes", []);
  __ns(n, "__l", {});
  __ns(n, "__parent", null);
  __hide(n, "_sf_parentNode", null);
  __ns(n, "__style", null);
  __hide(n, "_sf_nodeName", n.tagName);
  __ns(n, "__hidden", false);
  __hide(n, "_sf_offsetWidth", 0); __hide(n, "_sf_offsetHeight", 0); __hide(n, "_sf_offsetTop", 0); __hide(n, "_sf_offsetLeft", 0);
  __hide(n, "_sf_scrollWidth", 0); __hide(n, "_sf_scrollHeight", 0); __hide(n, "_sf_scrollTop", 0); __hide(n, "_sf_scrollLeft", 0);
  __hide(n, "_sf_clientWidth", 0); __hide(n, "_sf_clientHeight", 0);
  // ST-11 U2: script src/async/defer/type live on HTMLScriptElement.prototype.
  if (tag === "canvas") {
    __ns(n, "__width", 300); __ns(n, "__height", 150);
  }
  if (tag === "template") {
    // HTMLTemplateElement: children live in .content (a DocumentFragment),
    // never in the template's own childNodes. Widget code builds UI in a
    // template and queries it back via template.content.querySelector.
    var frag = __DCI.nc.fragment(n);
    __hide(n, "_sf_content", frag);
  }
  // ST-11 U2: form members live on HTMLFormElement.prototype (native).
  if (tag === "input" || tag === "textarea" || tag === "select") {
    __hide(n, "_sf_value", attrs && attrs.value !== undefined ? String(attrs.value) : "");
  }
  // ST-11 U2: img complete/naturalWidth/naturalHeight defaults come from the prototype.
  return n;
}

// ── native-accessor properties: shape parity with Chrome ────────────
// In real Chrome these live on prototypes as accessors, not as own
// data properties on instances; integrity checks compare own-ness.
function makeStyle() {
  var props = {};
  var s = {
    setProperty: function (k, v) { props[String(k)] = String(v); __hide(s, "_sf_" + String(k).replace(/-([a-z])/g, function (m, c) { return c.toUpperCase(); }), String(v)); },
    getPropertyValue: function (k) { return props[String(k)] || ""; },
    removeProperty: function (k) { var p = props[String(k)]; delete props[String(k)]; __hide(s, "_sf_" + String(k).replace(/-([a-z])/g, function (m, c) { return c.toUpperCase(); }), undefined); return p || ""; },
    get cssText() {
      var out = "";
      Object.keys(props).forEach(function (k) { out += k + ": " + props[k] + "; "; });
      return out ? out.slice(0, -1) : "";
    },
    set cssText(v) {
      props = {};
      String(v).split(";").forEach(function (part) {
        var i = part.indexOf(":");
        if (i > 0) props[part.slice(0, i).trim()] = part.slice(i + 1).trim();
        if (i > 0) __hide(s, "_sf_" + part.slice(0, i).trim().replace(/-([a-z])/g, function (m, c) { return c.toUpperCase(); }), part.slice(i + 1).trim());
      });
    },
  };
  __hide(s, "_s7b", "CSSStyleDeclaration");
  return s;
}
Object.defineProperty(EP, "style", {
  configurable: true,
  get: __gGet("style", function () { return __mr(this, "__style") || (__ns(this, "__style", makeStyle())); }),
  set: __gSet("style", function (v) { __ns(this, "__style", makeStyle()); if (v != null) __mr(this, "__style").cssText = String(v); }),
});
Object.defineProperty(EP, "hidden", {
  enumerable: true,
  configurable: true,
  get: __gGet("hidden", function () { return !!__mr(this, "__hidden"); }),
  set: __gSet("hidden", function (v) { __ns(this, "__hidden", !!v); }),
});
function __isConnected(n) {
  var p = n;
  while (p) {
    // a shadow root connects through its host chain (real browsers: nodes in
    // a shadow tree are connected when the host is)
    if (p.nodeType === 11 && (p.host || __g(p, "_sf_host"))) { p = p.host || __g(p, "_sf_host"); continue; }
    if (!p.parentNode) break;
    p = p.parentNode;
  }
  return p === document || p === document.documentElement || p === __g(document, "__root") || (!!p && p.nodeType === 9);
}
function __iframeShell(n) {
  if (__mr(n, "__win")) return __mr(n, "__win");
  var doc = makeElement("#document");
  __nsf(doc, "nodeType", 9);
  __nrh(doc, "readyState", "complete");
  __nrh(doc, "compatMode", "BackCompat");
  __nrh(doc, "contentType", "text/html");
  __nrh(doc, "characterSet", "UTF-8");
  __nrh(doc, "defaultView", null);
  __nrh(doc, "cookie", "");
  __nrh(doc, "title", "");
  __nrh(doc, "URL", "about:blank");
  __nrh(doc, "head", null); __nrh(doc, "body", null);
  __nrh(doc, "forms", []); __nrh(doc, "images", []); __nrh(doc, "links", []); __nrh(doc, "scripts", []);
  var html = makeElement("html"), head = makeElement("head"), body = makeElement("body");
  __nrh(doc, "documentElement", html);
  __nrh(doc, "head", head); __nrh(doc, "body", body);
  html.appendChild(head); html.appendChild(body);
  __nsf(html, "parentNode", doc); __ns(html, "__parent", doc);
  doc.childNodes.push(html);
  // per-shell factories: in a real frame a node's document is the FRAME's
  // document (frameDoc.createElement('div').ownerDocument === frameDoc);
  // sharing the main factories stamped the main document instead.
  // ST-11 U2: shell doc methods live on Document.prototype.





  // (querySelector moved to the prototype)




  var win = {
    document: doc, frameElement: n,
    location: { href: "about:blank", protocol: "about:", host: "", hostname: "", port: "", pathname: "blank", search: "", hash: "", origin: "null" },
    closed: false, name: "", length: 0,
    navigator: window.navigator,
    screen: window.screen,
    history: window.history,
    performance: window.performance,
    innerWidth: 0, innerHeight: 0, outerWidth: 0, outerHeight: 0,
    devicePixelRatio: window.devicePixelRatio,
    pageXOffset: 0, pageYOffset: 0, screenX: 0, screenY: 0,
    origin: "null", isSecureContext: false,
  };
  win.self = win; win.window = win;
  try { __nrh(doc, "defaultView", win); } catch (e0) {} // a frame's contentDocument.defaultView IS its window (challenge probes check it)
  try { var __hdp = (globalThis.HTMLDocument && globalThis.HTMLDocument.prototype) || Object.getPrototypeOf(document); if (__hdp && Object.getPrototypeOf(doc) !== __hdp) { Object.setPrototypeOf(doc, __hdp); } } catch (e1) {}
  try { var __wlq = win.location; if (__wlq && Object.getPrototypeOf(window.location)) { Object.setPrototypeOf(__wlq, Object.getPrototypeOf(window.location)); __hide(__wlq, "href", String(__wlq.href || "about:blank")); __hide(__wlq, "origin", "null"); Object.defineProperty(__wlq, "toString", { value: __native("toString", 0, function () { return __g(this, "href", ""); }), writable: true, configurable: true }); Object.defineProperty(__wlq, "valueOf", { value: __native("valueOf", 0, function () { return __g(this, "href", ""); }), writable: true, configurable: true }); } } catch (e1c) {}
  try { Object.defineProperty(doc, "location", { get: __native("get location", 0, function () { return win.location; }), set: __native("set location", 1, function (v) { try { win.location.href = String(v); } catch (e2) {} }), enumerable: true, configurable: false }); } catch (e3) {}
  // PARENT PROXY: a real frame's parent.postMessage stamps event.source = the
  // CALLING frame. In one realm we must synthesize it: the shell's parent is a
  // proxy over the main window whose postMessage delivers with source = this
  // shell. Both sides compare identities: the api.js does e.source ===
  // iframe.contentWindow (raw win), the widget does P['parent'] === event.source
  // (this proxy). Keep win.top = the raw top window (frame.top !== frame.self).
  var __parentProxy = new Proxy(window, {
    get: function (t, k) {
      if (k === "postMessage") return function (data, targetOrigin, transfer) {
        __pmj("OUT", "shellfn", targetOrigin, data);
        if (__TRACE) { try { __log("PM-SHELL out tO=" + String(targetOrigin) + " dt=" + JSON.stringify(data).slice(0, 220)); } catch (e) {} }
        var __pa = __pmArgs(targetOrigin, transfer);
        targetOrigin = __pa.to;
        if (targetOrigin !== "*" && targetOrigin !== __winOrigin(PAGE_URL)) { __pmj("DROP-SHELL", "shellfn", targetOrigin, data); return; }
        // the delivered event's origin is the SENDER's origin: this frame's
        // (cross-origin frames make it visible; a same-origin lab hides the
        // bug, and the live api.js drops a wrong origin silently).
        __deliverMessage(window, { data: data, origin: __g(win, "__frameOrigin") || __winOrigin(PAGE_URL), source: win, lastEventId: "", ports: __pa.ports });
      };
      return t[k];
    },
    has: function (t, k) { return k in t; },
    set: function (t, k, v) { try { t[k] = v; } catch (e) {} return true; },
  });
  __hide(win, "__parentProxy", __parentProxy);
  win.top = window; win.parent = __parentProxy;
  // ST-11 U2: shell window/document tags come from the prototypes.
  // realm bridge: sloppy-mode program code (this||self at IIFE level) writes
  // its state to globalThis = the MAIN realm, while reading through this
  // shell's window binding. In a real frame the window IS its globalThis, so
  // reads and writes agree; here they only reconcile if shell misses fall
  // through to the main realm. The widget program's boot chain depends on it.
  try {
    var __bridgeProto = new Proxy(Object.create(null), {
      get: function (t, k) {
        if (k in globalThis) return globalThis[k];
        if (__TRACE) { try { __log("MISS iframewin." + String(k) + " | at " + String(new Error().stack).split("\n")[1]); } catch (e) {} }
        return undefined;
      },
      has: function (t, k) { return k in globalThis; },
    });
    Object.setPrototypeOf(win, __bridgeProto);
    try {
      Object.defineProperty(__bridgeProto, "TEMPORARY", { value: 0, writable: false, enumerable: true, configurable: false });
      Object.defineProperty(__bridgeProto, "PERSISTENT", { value: 1, writable: false, enumerable: true, configurable: false });
      Object.defineProperty(__bridgeProto, "constructor", { value: globalThis.Window, writable: true, enumerable: false, configurable: true });
      Object.defineProperty(__bridgeProto, Symbol.toStringTag, { value: "Window", writable: false, enumerable: false, configurable: true });
      var __wp2 = globalThis.Window && globalThis.Window.prototype ? Object.getPrototypeOf(globalThis.Window.prototype) : null;
      if (__wp2) { Object.setPrototypeOf(__bridgeProto, __wp2); }
    } catch (eB2) {}
  } catch (e) {}
  addEventTarget(win); // own listener store: widget code listens on its window
  win.focus = function () {}; win.blur = function () {};
  // messages posted TO this frame: deliver here with the PARENT as sender
  win.postMessage = function (data, targetOrigin, transfer) {
    var __pa = __pmArgs(targetOrigin, transfer);
    targetOrigin = __pa.to;
    __pmj("OUT", "shellfn", targetOrigin, data);
    if (__TRACE) { try { __log("PM-SHELL out tO=" + String(targetOrigin) + " dt=" + JSON.stringify(data).slice(0, 220)); } catch (e) {} }
    var own = __g(win, "__frameOrigin") || "null";
    if (targetOrigin !== "*" && targetOrigin !== own) { __pmj("DROP-SHELL", "shellfn", targetOrigin, data); return; }
    __deliverMessage(win, {
      data: data,
      origin: __winOrigin(PAGE_URL),
      source: __g(win, "__parentProxy") || window,
      lastEventId: "",
      ports: __pa.ports,
    });
  };
  // a real frame's window schedules with its OWN realm's timer functions:
  // distinct identities from the parent (contentWindow.setTimeout !==
  // window.setTimeout) while using the same clock. Sharing references is a
  // detectable "overridden iframe" tell (deviceandbrowserinfo checks this
  // exact comparison), so the shell carries wrappers instead.
  var __mkTimerFn = (typeof __native === "function") ? __native : function (nm, len, fn) { return function () { return fn.apply(this, arguments); }; };
  win.setTimeout = __mkTimerFn("setTimeout", 1, function (fn, ms) { return setTimeout.apply(null, arguments); });
  win.clearTimeout = __mkTimerFn("clearTimeout", 1, function (id) { return clearTimeout.apply(null, arguments); });
  win.setInterval = __mkTimerFn("setInterval", 1, function (fn, ms) { return setInterval.apply(null, arguments); });
  win.clearInterval = __mkTimerFn("clearInterval", 1, function (id) { return clearInterval.apply(null, arguments); });
  if (__TRACE) {
    // the widget arms its poll/overrun timers through the shell: capture the
    // callback sources so the wait conditions can be decoded offline.
    win.setInterval = function (fn, ms) {
      try { if (typeof fn === "function") __log("WIV-I" + ms + " src=" + String(fn).slice(0, 480)); } catch (e) {}
      return setInterval.apply(null, arguments);
    };
    win.setTimeout = function (fn, ms) {
      try { if (typeof fn === "function" && Number(ms) >= 500) __log("WIV-T" + ms + " src=" + String(fn).slice(0, 480)); } catch (e) {}
      return setTimeout.apply(null, arguments);
    };
  }
  win.getComputedStyle = window.getComputedStyle || function () { return makeStyle(); };
  win.requestAnimationFrame = __mkTimerFn("requestAnimationFrame", 1, function (f) { return (window.requestAnimationFrame || function (g) { return setTimeout(g, 16); })(f); });
  win.cancelAnimationFrame = __mkTimerFn("cancelAnimationFrame", 1, function (id) { return (window.cancelAnimationFrame || clearTimeout)(id); });
  win.matchMedia = window.matchMedia || function () { return { matches: false, addListener: function () {}, removeListener: function () {}, addEventListener: function () {} }; };
  // window methods a real contentWindow carries; challenge probes touch these.
  if (win.eval === undefined && globalThis.eval !== undefined) win.eval = globalThis.eval;
  win.alert = function () {}; win.confirm = function () { return true; }; win.prompt = function () { return null; };
  win.open = function () { return null; }; win.close = function () {}; win.print = function () {}; win.stop = function () {};
  win.scrollTo = function () {}; win.scrollBy = function () {};
  win.requestIdleCallback = window.requestIdleCallback || function (f) { return setTimeout(function () { f({ didTimeout: false, timeRemaining: function () { return 0; } }); }, 1); };
  // realm builtins: a real contentWindow carries its realm's globals;
  // the extraction canaries typeof-check this set on the window itself.
  var __WN = ["Function", "Array", "ArrayBuffer", "Reflect", "Object", "String", "Symbol", "DataView",
    "TextEncoder", "TextDecoder", "Map", "WeakMap", "Set", "WeakSet", "Math", "JSON", "Number", "BigInt",
    "Boolean", "Date", "RegExp", "Error", "TypeError", "RangeError", "SyntaxError", "EvalError",
    "ReferenceError", "URIError", "Promise", "Proxy", "Uint8Array", "Uint8ClampedArray", "Int8Array",
    "Uint16Array", "Int16Array", "Uint32Array", "Int32Array", "Float32Array", "Float64Array",
    "SharedArrayBuffer", "Atomics", "Intl", "parseInt", "parseFloat", "isNaN", "isFinite",
    "encodeURI", "encodeURIComponent", "decodeURI", "decodeURIComponent", "escape", "unescape",
    "atob", "btoa", "queueMicrotask", "structuredClone", "console", "crypto", "fetch", "XMLHttpRequest",
    "WebSocket", "localStorage", "sessionStorage", "AbortController", "AbortSignal", "Event", "CustomEvent",
    "EventTarget", "RTCPeerConnection", "RTCDataChannel", "RTCSessionDescription", "RTCIceCandidate", "MutationObserver", "IntersectionObserver", "ResizeObserver", "OffscreenCanvas", "reportError", "URL",
    "URLSearchParams", "FinalizationRegistry", "WeakRef"];
  for (var __wi = 0; __wi < __WN.length; __wi++) {
    var __nm = __WN[__wi];
    if (win[__nm] === undefined && globalThis[__nm] !== undefined) {
      try { win[__nm] = globalThis[__nm]; } catch (e) {}
    }
  }
  win.globalThis = win;
  __nrh(doc, "defaultView", win);
  __ns(n, "__win", win); __ns(n, "__doc", doc);
  return win;
}

// Image beacons: challenge flows transport collect steps as image loads.
function __imgLoad(el) {
  var raw = __mr(el, "attrs") && __mr(el, "attrs").src;
  if (!raw) return;
  if (String(raw).indexOf("data:") === 0) {
    // data: URLs decode locally, but load/error still fires async — the
    // fpCollect transparent-pixel probe resolves ONLY on this event, and a
    // silent image hangs the whole fingerprint promise chain.
    var ds = String(raw);
    setTimeout(function () {
      var ok = ds.indexOf(",") > 0;
      try { __ns(el, "__complete", true); try { el.complete = true; } catch (e) {} } catch (e) {}
      try { __ns(el, "__imgFailed", !ok); } catch (e) {}
      try { __fireResource(el, ok ? "load" : "error"); } catch (e) {}
    }, 0);
    return;
  }
  var url = resolveUrl(PAGE_URL, String(raw));
  var st = 0;
  try {
    var r = __wireCall("GET", url, "image", "[]", "");
    var j = JSON.parse(r);
    st = j.ok ? j.status : -1;
  } catch (e) { st = -1; }
  try{ if(typeof __EBBS==='function'&&(globalThis.__ig=(globalThis.__ig||0)+1)<=40) __EBBS('IMG-'+String(url).slice(0,70)+'@'+st); }catch(_x){}
  if (__TRACE) { try { __log("IMG " + url + " -> " + st); } catch (e) {} }
  setTimeout(function () {
    try { __ns(el, "__complete", true); try { el.complete = true; } catch (e) {} } catch (e) {}
    var ok = st >= 200 && st < 400;
    try { __ns(el, "__imgFailed", !ok); } catch (e) {}
    try { __fireResource(el, ok ? "load" : "error"); } catch (e) {}
  }, 0);
}
(function () {
  var cp = tagCtor("canvas").prototype;
  Object.defineProperty(cp, "width", {
    enumerable: true, configurable: true,
    get: typeof __native === "function" ? __native("get width", 0, function () { if (!(this && __mr(this, "tag") === "canvas")) throw new TypeError("Illegal invocation"); return __mr(this, "__width") !== undefined ? __mr(this, "__width") : 300; }) : function () { return __mr(this, "__width") !== undefined ? __mr(this, "__width") : 300; },
    set: typeof __native === "function" ? __native("set width", 1, function (v) { if (!(this && __mr(this, "tag") === "canvas")) throw new TypeError("Illegal invocation"); var x = Number(v); __ns(this, "__width", isFinite(x) ? (x >>> 0) : 0); }) : function (v) { var x = Number(v); __ns(this, "__width", isFinite(x) ? (x >>> 0) : 0); },
  });
  Object.defineProperty(cp, "height", {
    enumerable: true, configurable: true,
    get: typeof __native === "function" ? __native("get height", 0, function () { if (!(this && __mr(this, "tag") === "canvas")) throw new TypeError("Illegal invocation"); return __mr(this, "__height") !== undefined ? __mr(this, "__height") : 150; }) : function () { return __mr(this, "__height") !== undefined ? __mr(this, "__height") : 150; },
    set: typeof __native === "function" ? __native("set height", 1, function (v) { if (!(this && __mr(this, "tag") === "canvas")) throw new TypeError("Illegal invocation"); var x = Number(v); __ns(this, "__height", isFinite(x) ? (x >>> 0) : 0); }) : function (v) { var x = Number(v); __ns(this, "__height", isFinite(x) ? (x >>> 0) : 0); },
  });
  var ip = tagCtor("img").prototype;
  function __imgDim(name) {
    return function () {
      var a = __mr(this, "attrs") && __mr(this, "attrs")[name];
      if (a !== undefined && a !== null && a !== "") { var x = parseInt(String(a), 10); if (isFinite(x)) return x; }
      if (__mr(this, "__imgFailed")) return 16; // Chrome's broken-image box (the 16px icon)
      return 0;
    };
  }
  Object.defineProperty(ip, "width", { get: __imgDim("width"), configurable: true });
  Object.defineProperty(ip, "height", { get: __imgDim("height"), configurable: true });
  ["img", "iframe", "script", "source", "track", "embed"].forEach(function (t) {
    var __srcGuard = function () { if (!(this && __mr(this, "tag") === t)) throw new TypeError("Illegal invocation"); };
    Object.defineProperty(tagCtor(t).prototype, "src", {
      get: __native("get src", 0, function () {
        __srcGuard.call(this);
        // browsers resolve .src to an ABSOLUTE url against the document base
        var v = __mr(this, "attrs").src;
        if (v === undefined || v === null) return "";
        try { return resolveUrl(PAGE_URL, String(v)); } catch (e) { return String(v); }
      }),
      set: __native("set src", 1, function (v) {
        __srcGuard.call(this);
        __mr(this, "attrs").src = String(v);
        if (t === "img") { try { __imgLoad(this); } catch (e) {} }
        if (t === "iframe") {
          if (__TRACE) { try { __log("IFRAME-SRC.PROPERTY " + String(v).slice(0, 120) + " connected=" + __isConnected(this)); } catch (e) {} }
          try { __iframeLoad(this); } catch (e) {}
        }
      }),
      enumerable: true, configurable: true,
    });
  });
  // script.text — Chrome owns `text` on HTMLScriptElement.prototype: the
  // getter returns the script's text; the setter mutates the children and
  // (on a connected script) enqueues execution (esc7 measured: connected-set
  // runs, never-attached does not).
  (function () {
    var sp = tagCtor("script").prototype;
    Object.defineProperty(sp, "text", {
      configurable: true,
      get: __gGet("text", function () {
        if (!(this && __mr(this, "tag") === "script")) return undefined;
        var __o = "";
        for (var __i = 0; __i < this.childNodes.length; __i++) { var __c = this.childNodes[__i]; if (__c.nodeType === 3) __o += __mr(__c, "text"); }
        return __o;
      }),
      set: __gSet("text", function (v) {
        if (!(this && __mr(this, "tag") === "script")) return;
        this.childNodes.length = 0;
        var t = makeText(String(v == null ? "" : v));
        __nsf(t, "parentNode", this); __ns(t, "__parent", this);
        this.childNodes.push(t);
        try { if (__isConnected(this)) __prepScript(this); } catch (e) {}
      }),
    });
  })();
  Object.defineProperty(tagCtor("a").prototype, "href", {
    get: __native("get href", 0, function () { if (!(this && __mr(this, "tag") === "a")) throw new TypeError("Illegal invocation"); var v = __mr(this, "attrs").href; if (v === undefined || v === null) return ""; try { return resolveUrl(PAGE_URL, String(v)); } catch (e) { return String(v); } }),
    set: __native("set href", 1, function (v) { if (!(this && __mr(this, "tag") === "a")) throw new TypeError("Illegal invocation"); __mr(this, "attrs").href = String(v); }),
    enumerable: true, configurable: true,
  });
  var ip = tagCtor("iframe").prototype;
  Object.defineProperty(ip, "contentWindow", {
    get: typeof __native === "function" ? __native("get contentWindow", 0, function () { if (!(this && __mr(this, "tag") === "iframe")) throw new TypeError("Illegal invocation"); return __isConnected(this) ? __iframeShell(this) : null; }) : function () { return __isConnected(this) ? __iframeShell(this) : null; },
    enumerable: true, configurable: true,
  });
  Object.defineProperty(ip, "contentDocument", {
    get: typeof __native === "function" ? __native("get contentDocument", 0, function () { if (!(this && __mr(this, "tag") === "iframe")) throw new TypeError("Illegal invocation"); return __isConnected(this) ? (__iframeShell(this), __mr(this, "__doc")) : null; }) : function () { return __isConnected(this) ? (__iframeShell(this), __mr(this, "__doc")) : null; },
    enumerable: true, configurable: true,
  });
})();

// Cross-origin iframes (challenges.cloudflare.com) carry the flow's own
// page + scripts; load them through the wire and run their scripts.
// __ifKick: append-time load trigger. A browser starts an iframe's
// navigation when it becomes connected; the property-set path loads via its
// accessor, but an iframe built from an HTML string (innerHTML → appendChild)
// carries a PARSED src that never went through a setter — DataDome's
// device-check iframe is exactly this shape (measured on g2, 2026-10-18).
function __ifKick(n) {
  if (!n || n.nodeType !== 1) return;
  if (__mr(n, "tag") === "iframe") {
    if (__mr(n, "attrs") && __mr(n, "attrs").src && !__mr(n, "__ifLoaded")) { try { __iframeLoad(n); } catch (e) {} }
    return;
  }
  if (!n.childNodes || !n.childNodes.length) return;
  var subs = [];
  try { subs = n.getElementsByTagName ? n.getElementsByTagName("iframe") : []; } catch (e) { subs = []; }
  for (var i = 0; i < subs.length; i++) {
    var el = subs[i];
    if (el && __mr(el, "attrs") && __mr(el, "attrs").src && !__mr(el, "__ifLoaded")) { try { __iframeLoad(el); } catch (e) {} }
  }
}
// Conservative top-level declaration scan for classic frame scripts (see the
// seeding site in __iframeLoad). Depth-0 only; skips strings/comments/regex
// heuristically. False negatives are safe (status quo); discovered names are
// pre-seeded on the frame window so the with-veil materializes the assignment
// as a window property, browser-style. Measured: DataDome interstitial's
// `var ddm` (read by its deferred bundle as `window.ddm`; g2, 2026-10-18).
function __scanGlobals(code) {
  var out = [];
  var s = String(code);
  var n = s.length, i = 0, depth = 0, prevSig = "";
  var NAME_RE = /[A-Za-z_$][A-Za-z0-9_$]*/y;
  var __KW = { "var":1,"let":1,"const":1,"function":1,"return":1,"if":1,"else":1,"for":1,"while":1,"do":1,"switch":1,"case":1,"default":1,"break":1,"continue":1,"new":1,"delete":1,"typeof":1,"instanceof":1,"in":1,"void":1,"this":1,"null":1,"true":1,"false":1,"class":1,"extends":1,"super":1,"yield":1,"await":1,"import":1,"export":1,"try":1,"catch":1,"finally":1,"throw":1,"with":1,"debugger":1,"enum":1,"implements":1,"interface":1,"package":1,"private":1,"protected":1,"public":1,"eval":1,"arguments":1 };
  var push = function (x) { if (x && !__KW[x] && out.indexOf(x) < 0) out.push(x); };
  var readAt = function (j) { NAME_RE.lastIndex = j; var m = NAME_RE.exec(s); return m ? m[0] : ""; };
  var skipStr = function (j, q) { j++; while (j < n) { var c = s[j]; if (c === "\\") { j += 2; continue; } if (c === q) return j + 1; j++; } return j; };
  var skipToBreak = function (j) { // inside a var initializer, to the list's ',' or ';' at relative depth 0
    var rel = 0;
    while (j < n) {
      var c = s[j], c2 = s[j + 1];
      if (c === "/" && c2 === "/") { while (j < n && s[j] !== "\n") j++; continue; }
      if (c === "/" && c2 === "*") { j += 2; while (j < n && !(s[j] === "*" && s[j + 1] === "/")) j++; j += 2; continue; }
      if (c === "'" || c === '"' || c === "`") { j = skipStr(j, c); continue; }
      if (c === "(" || c === "[" || c === "{") { rel++; j++; continue; }
      if (c === ")" || c === "]" || c === "}") { if (rel === 0) return j; rel--; j++; continue; }
      if (rel === 0 && (c === ";" || c === ",")) return j;
      j++;
    }
    return j;
  };
  while (i < n) {
    var c = s[i], c2 = s[i + 1];
    if (c === "/" && c2 === "/") { while (i < n && s[i] !== "\n") i++; continue; }
    if (c === "/" && c2 === "*") { i += 2; while (i < n && !(s[i] === "*" && s[i + 1] === "/")) i++; i += 2; continue; }
    if (c === "'" || c === '"' || c === "`") { i = skipStr(i, c); prevSig = ")"; continue; }
    if (c === "/" && (prevSig === "" || "(,=:[!&|?;{}".indexOf(prevSig) >= 0)) { // regex literal (heuristic)
      i++; var inCls = false;
      while (i < n) { var rc = s[i]; if (rc === "\\") { i += 2; continue; } if (rc === "[") { inCls = true; i++; continue; } if (rc === "]") { inCls = false; i++; continue; } if (rc === "/" && !inCls) { i++; break; } i++; }
      while (i < n && /[a-z]/i.test(s[i])) i++;
      prevSig = "/"; continue;
    }
    if (c === "{") { depth++; i++; prevSig = c; continue; }
    if (c === "}") { if (depth > 0) depth--; i++; prevSig = c; continue; }
    if (c === "(" || c === ")" || c === "[" || c === "]") { i++; prevSig = c; continue; }
    if (depth === 0 && c === "v" && s.substr(i, 3) === "var" && !/[A-Za-z0-9_$]/.test(s[i + 3] || "") && (i === 0 || !/[A-Za-z0-9_$.]/.test(s[i - 1]))) {
      i += 3;
      var jj = i, guard = 0;
      while (jj < n && guard++ < 220) {
        while (jj < n && /\s/.test(s[jj])) jj++;
        var nm = readAt(jj);
        if (!nm) break;
        push(nm);
        jj += nm.length;
        jj = skipToBreak(jj);
        if (s[jj] === ",") { jj++; continue; }
        break;
      }
      i = jj;
      prevSig = "x";
      continue;
    }
    if (depth === 0 && c === "f" && s.substr(i, 8) === "function" && !/[A-Za-z0-9_$]/.test(s[i + 8] || "") && (i === 0 || !/[A-Za-z0-9_$.]/.test(s[i - 1]))) {
      var jf = i + 8;
      while (jf < n && /\s/.test(s[jf])) jf++;
      var nf = readAt(jf);
      if (nf) push(nf);
      i += 8;
      prevSig = "n";
      continue;
    }
    if (!/\s/.test(c)) prevSig = c;
    i++;
  }
  return out;
}
// Programmatic frame navigation (location.replace/assign/reload inside a
// shell): clear the shell cache and re-run __iframeLoad against the new URL —
// the same path a real frame takes when its document navigates itself
// (measured: DataDome's interstitial redirects to its captcha page via
// location.replace after the devicecheck POST; the old no-op stub silently
// dropped the whole flow; g2, 2026-10-18). Depth-capped so a redirect loop
// cannot nest forever.
function __frameNav(el, u) {
  try {
    if (__TRACE) { try { __log("FRAMENAV -> " + String(u).slice(0, 110)); } catch (e) {} }
    if ((__mr(el, "__navDepth") || 0) >= 3) return;
    __ns(el, "__navDepth", (__mr(el, "__navDepth") || 0) + 1);
    __ns(el, "attrs", __mr(el, "attrs") || {});
    __mr(el, "attrs").src = String(u);
    try { __ns(el, "__win", null); __ns(el, "__doc", null); } catch (e0) {}
    try { __iframeLoad(el); } finally { __ns(el, "__navDepth", (__mr(el, "__navDepth") || 1) - 1); }
  } catch (e) { if (__TRACE) { try { __log("FRAMENAV-ERR " + e); } catch (e2) {} } }
}
function __iframeLoad(el) {
  var raw = __mr(el, "attrs") && __mr(el, "attrs").src;
  if (!raw) return;
  var s = String(raw);
  if (s.indexOf("about:") === 0 || s.indexOf("data:") === 0 || s.indexOf("javascript:") === 0) { __ns(el, "__ifLoaded", 1); return; }
  // marked when a load is initiated; append re-triggers skip via this flag,
  // while a later .src re-set still reloads (the accessor path is unconditional)
  __ns(el, "__ifLoaded", 1);
  var url = resolveUrl(PAGE_URL, s);
  var st = 0;
  var body = "";
  try {
    var r = __wireCall("GET", url, "document", "[]", "");
    var j = JSON.parse(r);
    st = j.ok ? j.status : -1;
    body = (j.ok && j.body) || "";
  } catch (e) { st = -1; }
  if (__TRACE) { try { __log("IFRAME " + url + " -> " + st + " (" + body.length + "b)"); } catch (e) {} }
  if (__TRACE && st >= 200 && st < 300 && body && typeof __dumpFile === "function") { try { __dumpFile("iframepage", String(body)); } catch (eD) {} }
  if (st < 200 || st >= 300 || !body) return;
  try {
    var win = __iframeShell(el);
    var doc = __mr(el, "__doc");
    __nrh(doc, "URL", url);
    __nrh(doc, "readyState", "loading"); // a real navigation starts at loading; scripts see this, not "complete"
    win.location = {
      href: url, protocol: (url.match(/^https?:/) || ["https:"])[0] + "//", host: (url.match(/^https?:\/\/[^\/?#]+/) || ["", ""])[0].replace(/^https?:\/\//, ""),
      hostname: (url.match(/^https?:\/\/([^\/?#]+)/) || ["", ""])[1] || "", port: "", pathname: (url.match(/^https?:\/\/[^\/?#]+(\/[^?#]*)/) || ["", ""])[1] || "/", search: "", hash: "",
      origin: __winOrigin(url),
      assign: function (u) { try { __frameNav(el, resolveUrl(url, String(u))); } catch (e) {} },
      replace: function (u) { try { __frameNav(el, resolveUrl(url, String(u))); } catch (e) {} },
      reload: function () { try { __frameNav(el, url); } catch (e) {} },
      toString: function () { return url; },
    };
    __hide(win, "__frameOrigin", __winOrigin(url));
    try { win.XMLHttpRequest = __mkFrameXHR(url); } catch (eFX) {}
    try { win.fetch = __mkFrameFetch(url); } catch (eFF) {}
    try { __nrh(win, "URL", __mkFrameURL(url)); } catch (eFU) {}
    if (__winOrigin(url) !== __winOrigin(PAGE_URL)) {
      __lastXFrame = { origin: __winOrigin(url), win: win };
      // cross-origin frames see frameElement === null in a real browser
      // (same-origin policy); the widget program branches on it.
      try { win.frameElement = null; } catch (e0) {}
    }
    if (__TRACE) { try { __traceProto(doc, "iframedoc"); } catch (e) {} }
    var nodes = parseFragment(body);
    doc.body.childNodes.length = 0;
    for (var i = 0; i < nodes.length; i++) {
      __nsf(nodes[i], "parentNode", doc.body); __ns(nodes[i], "__parent", doc.body);
      doc.body.childNodes.push(nodes[i]);
    }
    // run child scripts (main-realm execution for v1)
    var scriptEls = doc.body.getElementsByTagName("script");
    if (__TRACE) { try { __log("IFRAME-DBG nodes=" + nodes.length + " scripts=" + scriptEls.length + " bodykids=" + doc.body.childNodes.length); } catch (e) {} }
    // Classic-script order, as a browser runs it: non-deferred scripts in
    // document order, then `defer` scripts in document order (they run after
    // parsing, before DOMContentLoaded). Measured on the DataDome
    // interstitial: its inline driver (defines `var ddm`) must run BEFORE the
    // deferred bundle that reads `window.ddm` (g2, 2026-10-18).
    var __scriptOrder = [];
    for (var __so = 0; __so < scriptEls.length; __so++) { var __sae = __mr(scriptEls[__so], "attrs") || {}; if (__sae.defer === undefined) __scriptOrder.push(scriptEls[__so]); }
    for (var __so2 = 0; __so2 < scriptEls.length; __so2++) { var __sae2 = __mr(scriptEls[__so2], "attrs") || {}; if (__sae2.defer !== undefined) __scriptOrder.push(scriptEls[__so2]); }
    for (var k = 0; k < __scriptOrder.length; k++) {
      var sc = __scriptOrder[k];
      var srcAttr = __mr(sc, "attrs").src;
      // raw script text: textContentOf hides script/style text from page-text
      // extraction (by design); iframe scripts need the real code.
      var code = srcAttr ? "" : (function (n) { var o = ""; for (var q = 0; q < n.childNodes.length; q++) { var c = n.childNodes[q]; if (c.nodeType === 3) o += __mr(c, "text"); } return o; })(sc);
      if (srcAttr) {
        try {
          var u2 = resolveUrl(url, String(srcAttr));
          var r2 = __wireCall("GET", u2, "script", "[]", "");
          var j2 = JSON.parse(r2);
          if (j2.ok) code = j2.body;
          if (__TRACE) { try { __log("IFRAME-SCRIPT " + u2 + " -> " + (j2.ok ? j2.status : "ERR")); } catch (e) {} }
        } catch (e) {}
      }
      if (code) {
        // Iframe scripts run SCOPE-WRAPPED: window/document/self/name bind to
        // this shell (the frame's identity), parent/top bind to the host
        // window. A widget program stomps the loader's module state when it
        // shares the main realm; in a real browser it runs in the frame's own
        // realm and talks to the host only via postMessage. The wrapper gives
        // the frame its own window identity without a second JS realm.
          var __origCode = code;
        try {
          if (__TRACE && code.length > 100000) {
            // the string-table assignment rotates its name per variant (a5, b4, ...): match ANY decoder-array shape
            try { code = code.replace(/([A-Za-z_$][\w$]*)=\[u\((\d+)/, "$1=globalThis.__A5X=[u($2"); } catch (e) {}
            try { if (!globalThis.__wrapDISP) globalThis.__wrapDISP = function (n2, f2) { if (typeof f2 !== "function") return f2; return function () { try { if (typeof __TRACE !== "undefined" && __TRACE && (globalThis.__DISP = (globalThis.__DISP || 0) + 1) <= 400) __log("DISP " + n2 + " n=" + arguments.length); } catch (e) {} return f2.apply(this, arguments); }; }; } catch (e) {}
            // table-reader beacon: record the last table indices read — the
            // stall site names itself. The base reader shape:
            //   function z(A,B){return A=A-N,C=B[A],Array.from(C).reverse().join(``)}
            try {
              code = code.replace(/function ([\w$]+)\(([\w$]+),([\w$]+)\)\{return \2=\2-(\d+),/, 'function $1($2,$3){try{(globalThis.__RD=globalThis.__RD||[]).push("@$1:"+$2),globalThis.__RD.length>40&&globalThis.__RD.shift(),(function(){var t=Date.now();if(!globalThis.__RDST||t-globalThis.__RDST>60){globalThis.__RDST=t;var R=globalThis.__RDS=globalThis.__RDS||[];R.push("@"+t+" | "+globalThis.__RD.slice(-18).join(" "));R.length>200&&R.shift();__dumpFile&&__dumpFile("rdsamp",R.join(String.fromCharCode(10)))}})()}catch(eR){}return $2=$2-$4,');
              code = code.replace(/if\(k===!eZ\)return;/, 'if(__dumpFile&&(globalThis.__kp=(globalThis.__kp||0)+1)<=6){try{__dumpFile("kprobe","k="+String(k)+" type="+typeof k+" kIsFalse="+(k===!eZ))}catch(eK){}}if(k===!eZ)return;');
              code = code.replace(/k=!eZ;continue/, 'try{__dumpFile("kprobe","SET k=false reached")}catch(eK2){}k=!eZ;continue');
              code = code.replace(/function ([\w$]+)\(([\w$]+)\)\{return ([\w$]+)\[\2\]\}/, 'function $1($2){try{(globalThis.__A5X=$3),(globalThis.__RD=globalThis.__RD||[]).push("@$1:"+$2+"="+String($3[$2]).slice(0,42)),globalThis.__RD.length>40&&globalThis.__RD.shift(),(globalThis.__RDSK=globalThis.__RDSK||[]).push("@"+$2+" | "+String(new Error().stack).split(String.fromCharCode(10)).slice(2,4).join(" | ").slice(0,150)),globalThis.__RDSK.length>12&&globalThis.__RDSK.shift(),globalThis.__A5RUN||(globalThis.__A5RUN=1,__dumpFile&&__dumpFile("a5run",$3.map(function(x,i){return i+"="+String(x)}).join(String.fromCharCode(10)))),(function(){var t=Date.now();if(!globalThis.__RDST||t-globalThis.__RDST>60){globalThis.__RDST=t;var R=globalThis.__RDS=globalThis.__RDS||[];R.push("@"+t+" | "+globalThis.__RD.slice(-18).join(" "));R.length>200&&R.shift();__dumpFile&&__dumpFile("rdsamp",R.join(String.fromCharCode(10))+String.fromCharCode(10)+"--- stacks ---"+String.fromCharCode(10)+globalThis.__RDSK.join(String.fromCharCode(10)))}})()}catch(eR){}return $3[$2]}');
            var __dispNames = ["QHuc0", "ycPG8", "CgDs1", "GWmo4", "nqgGN8", "XzQJn5", "AYmdK7", "ouUsd0", "OOXF0", "MPUK6", "RHXmy7", "YVDgD5", "OElR3", "MWrWa2", "cKYwE2", "Afjo3", "YuOpI3", "HDQZU4", "oWPf2"];
            for (var __dn = 0; __dn < __dispNames.length; __dn++) { try { var __dpat = new RegExp('"' + __dispNames[__dn] + '":([A-Za-z_$][A-Za-z0-9_$]*)'); code = code.replace(__dpat, '"' + __dispNames[__dn] + '":globalThis.__wrapDISP("' + __dispNames[__dn] + '",$1)'); } catch (e) {} }
            } catch (e) {}
          }
          if (__TRACE && code.indexOf("bOye5") >= 0 && code.indexOf("_cf_chl_opt") >= 0 && code.length > 100000) {
            // widget-program boot beacon (TRACE only): the program starts by
            // writing its own opt; mark that we got there.
            code = code.replace("window._cf_chl_opt = {", "window._cf_chl_opt = {__wboot:1,", 1);
          }
          var __preKeys = {};
          try { Object.keys(win).forEach(function (k) { __preKeys[k] = 1; }); } catch (e) {}
          var __gKeysBefore = {};
          try { Object.keys(globalThis).forEach(function (k) { __gKeysBefore[k] = 1; }); } catch (e) {}
          var __winPx = null;
          if (__TRACE) { try { __winPx = new Proxy(win, { set: function (t, k, v) { try { if (typeof v === "function") { globalThis.__WSETLOG = globalThis.__WSETLOG || []; globalThis.__WSETLOG.push(String(k) + ":" + String(v).slice(0, 80).replace(/[\n\r]/g, " ")); } } catch (e) {} t[k] = v; return true; }, get: function (t, k) { return t[k]; }, has: function (t, k) { return k in t; } }); } catch (eP) { } }
          (function (window, document, parent, top, self, location, navigator, name, origin, history,
                     addEventListener, removeEventListener, dispatchEvent,
                     setTimeout, setInterval, clearTimeout, clearInterval,
                     requestAnimationFrame, cancelAnimationFrame, XMLHttpRequest, fetch) {
            // DIRECT eval: the code must see this wrapper's scope (window,
            // document, parent, ...). Indirect eval would re-bind everything
            // to the main realm and the wrapper would bind nothing.
            // The IMPLICIT globals (bare addEventListener/setInterval/... in
            // iframe code) must also resolve to THIS frame's versions: a real
            // frame's script sees its own window's methods. Without these
            // params the bare calls fall through to the main realm — the
            // widget's main message handler registered on the WRONG window
            // and never saw a single meow.
            var __savedEval = globalThis.eval, __hadPristine = !!globalThis.__evPristine;
            if (__hadPristine) { try { globalThis.eval = globalThis.__evPristine; } catch (e0) {} } // TRACE's eval override turns direct evals indirect (global scope); restore the intrinsic so this is a REAL direct eval
            try { if (typeof __origCode === "string" && code !== __origCode) { new Function(code); } } catch (__pe) { __log("TRANSFORM-REVERT " + String((__pe && __pe.message) || __pe).slice(0, 140)); code = __origCode; }
            var __wpNames = ["window", "document", "parent", "top", "self", "location", "navigator", "name", "origin", "history", "addEventListener", "removeEventListener", "dispatchEvent", "setTimeout", "setInterval", "clearTimeout", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame", "XMLHttpRequest", "fetch", "eval"];
            var __winScope = win;
            try { __winScope = new Proxy(win, { has: function (t, k) { if (typeof k !== "string") return k in t; if (__wpNames.indexOf(k) >= 0) { var __wd = null; try { __wd = Object.getOwnPropertyDescriptor(t, k); } catch (e) {} return !!(__wd && !__wd.configurable); } return k in t; }, get: function (t, k) { return typeof k === "string" ? t[k] : undefined; }, set: function (t, k, v) { t[k] = v; return true; } }); } catch (eW3) { __winScope = win; }
            // Global materialization (class-level): in a browser a classic
            // script's top-level `var NAME` / `function NAME` become window
            // properties, visible to later scripts and as `window.NAME`.
            // The engine evals each script in its own strict scope (the
            // strictness is load-bearing: bare-call `this` must not leak to
            // the main realm), so declarations cannot be routed — instead a
            // post-eval publish pass reads the values in the same eval scope
            // and copies them onto the frame window, browser-style. Names are
            // also pre-seeded (undefined) so `window.NAME` reads during the
            // script see the hoisted binding, like a browser.
            // (Measured need: DataDome interstitial's `var ddm` read by its
            // deferred bundle as `window.ddm`; g2, 2026-10-18. Guarded: never
            // shadows anything already present on the window or the realm.)
            var __seedNames = [], __pubCode = "";
            try {
              var __seeds = __scanGlobals(code);
              if (__TRACE) { try { var __st1 = __scanGlobals("var aaa=1; var bbb=2;"); __log("SCANTEST " + JSON.stringify(__st1) + " main=" + __seeds.length + " [" + __seeds.slice(0, 8).join(",") + "]"); } catch (eSt) { __log("SCANTEST-ERR " + String(eSt)); } }
              for (var __si = 0; __si < __seeds.length; __si++) {
                var __sn = __seeds[__si];
                if (__wpNames.indexOf(__sn) >= 0) continue;
                if (!(__sn in window)) window[__sn] = undefined;
                __seedNames.push(__sn);
              }
              if (__seedNames.length) {
                var __pexp = [];
                for (var __pj = 0; __pj < __seedNames.length; __pj++) __pexp.push("(typeof " + __seedNames[__pj] + '!=="undefined"?' + __seedNames[__pj] + ":void 0)");
                __pubCode = ";try{window.__pubS=1;window.__pubv=[" + __pexp.join(",") + "]}catch(eP){try{window.__pubErr=String(eP)}catch(e2){}}";
              }
            } catch (eSeed) {}
            if (__TRACE) { try { __log("PREEVAL pubLen=" + __pubCode.length + " clen=" + code.length + " hP=" + (!!__hadPristine) + " evE=" + (eval === globalThis.eval) + " evP=" + (typeof globalThis.__evPristine) + " phead=" + JSON.stringify(__pubCode.slice(0, 44)) + " ctail=" + JSON.stringify(String(code).slice(-48))); } catch (e) {} }
            try { with (__winScope) { eval("\"use strict\";" + code + __pubCode); } } catch (e) { var __iem = "iframe-script: " + String((e && e.name) || "?") + " M=" + String((e && e.message)) + " @ " + String((e && e.stack) || "").split("\n").slice(0, 9).join(" | "); __log(__iem); __DCI.t.iframeErr = __iem; try { (__DCI.t.WLOG = __DCI.t.WLOG || []).push(__iem.slice(0, 200)); } catch (e2) {} }
            try {
              // Transport is the frame window itself: inside a frame script
              // `globalThis` resolves to the frame window (measured e7), and
              // the masked `window` param is the identical, reliable channel.
              if (__seedNames.length && window.__pubv && window.__pubv.length === __seedNames.length) {
                for (var __pk = 0; __pk < __seedNames.length; __pk++) {
                  var __pn = __seedNames[__pk];
                  if (!(__pn in window) || window[__pn] === undefined) { try { window[__pn] = window.__pubv[__pk]; } catch (eW4) {} }
                }
              }
              if (__TRACE) { try { __log("PUBL n=" + __seedNames.length + " S=" + (window.__pubS || 0) + " pubv=" + (window.__pubv ? window.__pubv.length : "-") + " v0t=" + (window.__pubv && window.__pubv.length ? typeof window.__pubv[0] : "-") + " ddmT=" + (typeof window.ddm) + " perr=" + String(window.__pubErr || "-").slice(0, 120)); } catch (e) {} }
              try { delete window.__pubv; delete window.__pubS; delete window.__pubErr; } catch (eX4) {}
            } catch (ePub) {}
            if (__hadPristine) { try { globalThis.eval = __savedEval; } catch (e0) {} }
            __hide(window, "__wrapalive", 1); // wrapper-scope write: hidden store, never an own prop
            if (__TRACE) { try {
              // wrap every NEW function the program put on the shell: the
              // boot dispatcher is one of them; if it is never called the
              // caller died earlier.
              Object.keys(win).forEach(function (k) {
                if (__preKeys[k]) return;
                var v = win[k];
                if (typeof v !== "function") return;
                try {
                  var orig = v;
                  win[k] = function () {
                    var args = Array.prototype.slice.call(arguments, 0, 3).map(function (a) { return typeof a === "string" ? a.slice(0, 40) : typeof a; });
                    __log("WBOOTFN " + k + " CALLED args=[" + args.join(",") + "]");
                    var r = orig.apply(this, arguments);
                    __log("WBOOTFN " + k + " RETURNED " + (r === undefined ? "undef" : typeof r));
                    return r;
                  };
                } catch (eW) {}
              });
            } catch (e) {} }
            if (__TRACE) { try {
              var __newG = [];
              globalThis.__ORIGS = globalThis.__ORIGS || {};
              try { Object.keys(globalThis).forEach(function (k) {
                if (__gKeysBefore[k]) return;
                var v = globalThis[k];
                __newG.push(k + ":" + (typeof v).slice(0, 9) + ":" + String(v).slice(0, 60).replace(/[\n\r]/g, " "));
                if (typeof v === "function") globalThis.__ORIGS[k] = v; // side map only: identity and toString stay pristine
              }); } catch (e) {}
              if (typeof __dumpFile === "function") __dumpFile("gnewkeys", __newG.join("\n"));
              try {
                var __rpTgt = (typeof win.runProgram === "function") ? win.runProgram : globalThis.runProgram;
                if (typeof __rpTgt === "function" && !__rpTgt.__rpWrapped) {
                  __log("RUNPROG-WRAP g=" + typeof globalThis.runProgram + " w=" + typeof win.runProgram + " same=" + (win.runProgram === globalThis.runProgram) + " wonly=" + Object.keys(win).filter(function (k) { return !(k in globalThis); }).slice(0, 60).join(",") + " orig=" + String(globalThis.runProgram).slice(0, 120).replace(/[\n\r]/g, " ") + " worig=" + (typeof win.runProgram === "function" ? String(win.runProgram).slice(0, 120).replace(/[\n\r]/g, " ") : "-"));
                  var __rpOrig = __rpTgt;
                  var __rpNew = function (prog) {
                    __log("RUNPROG len=" + String(prog && prog.length) + " head=" + String(prog).slice(0, 90).replace(/[\n\r]/g, " "));
                    try { var r = __rpOrig.apply(this, arguments); __log("RUNPROG RET " + typeof r); return r; }
                    catch (eR2) { __log("RUNPROG THREW " + String(eR2 && eR2.message).slice(0, 160) + " STK " + String(eR2 && eR2.stack).slice(0, 260)); try { var __st2 = String(eR2 && eR2.stack); __srdump("runprog", "FULL: " + __st2); var __re2 = /:1:(\d+)\)/g, __m3, __cnt = 0; while ((__m3 = __re2.exec(__st2)) && __cnt < 3) { var __o = Number(__m3[1]) - 13; __srdump("runprog", "S" + __cnt + "(" + __o + "): " + String(code).slice(__o - 140, __o + 140)); __cnt++; } __srdump("runprog", "CODELEN: " + String(code).length); } catch (eX) {} throw eR2; }
                  };
                  try { Object.defineProperty(__rpNew, "__rpWrapped", { value: true }); } catch (eW2) {}
                  if (__rpTgt === win.runProgram) { win.runProgram = __rpNew; } else { globalThis.runProgram = __rpNew; }
                }
              } catch (eRP) { __log("RUNPROG WRAP ERR " + eRP); }
              try { var __srcDump = []; Object.keys(globalThis).forEach(function (k) { if (__gKeysBefore[k]) return; var v = globalThis[k]; if (typeof v === "function") __srcDump.push("### " + k + "\n" + String(v).slice(0, 300)); }); if (typeof __dumpFile === "function") __dumpFile("srcdump", __srcDump.join("\n")); } catch (eS) {}
              // find the fail-boot VM (the '|'-dispatch installer) among the
              // new globals and CALL it: does its dispatch run in our env?
              try {
                var __vmName = null;
                Object.keys(globalThis.__ORIGS || {}).forEach(function (k) {
                  if (__vmName) return;
                  var v = globalThis.__ORIGS[k];
                  var src = String(v);
                  if (src.indexOf("|") >= 0 && src.indexOf("switch") >= 0 && src.indexOf("case") >= 0 && src.length > 200) __vmName = k;
                });
                __log("VMSCAN name=" + String(__vmName));
                if (__vmName) {
                  try { globalThis.__ORIGS[__vmName]("test"); __log("VMCALL ok"); } catch (eC) { __log("VMCALL THREW " + String(eC && eC.message).slice(0, 120) + " STACK " + String(eC && eC.stack).slice(0, 200)); }
                }
              } catch (eV) { __log("VMSCAN ERR " + eV); }
              if (globalThis.__WSETLOG && globalThis.__WSETLOG.length && typeof __dumpFile === "function") __dumpFile("wsetlog", globalThis.__WSETLOG.join("\n"));
            } catch (eD3) {} }
            if (__TRACE) { try { __log("WBOOT head=" + JSON.stringify(String(code).slice(0, 160)) + " tail=" + JSON.stringify(String(code).slice(-120))); } catch (e) {} }
            if (__TRACE && code.length > 100000) { try { if (typeof __dumpFile === "function") __dumpFile("iframe-code", code); } catch (eD2) {} }
            if (__TRACE) { try { __log("WBOOT code=" + code.length + " alive=" + (!!__g(win, "__wrapalive")) + " lkeys=" + ((__g(win, "__l") && Object.keys(__g(win, "__l")).join("+")) || "-") + " dkeys=" + ((__g(doc, "__l") && Object.keys(__g(doc, "__l")).join("+")) || "-") + " nkeys=" + Object.keys(win).length + " shellopt=" + (win._cf_chl_opt ? String(win._cf_chl_opt.PgHE4) : "NO") + " THirw9=" + (typeof win.THirw9) + " tt=" + (typeof win.trustedTypes) + " RD=" + String((globalThis.__RD || []).slice(-16).join(",")) + " protoTHirw9=" + (typeof Object.prototype.THirw9)); } catch (e) {} }
            if (__TRACE && globalThis.__A5X && globalThis.__A5X.length) { try { var a5 = globalThis.__A5X;
            try { if (typeof __dumpFile === "function") __dumpFile("a5table", a5.map(function (x, i) { return i + "=" + String(x); }).join("\n")); } catch (eD) {} var __kw = ["init", "postMessage", "overrun", "meow", "food", "complete", "readyState", "DOMContentLoaded", "currentScript", "parent", "fail", "interactive", "loading"]; var __out = []; for (var __q = 0; __q < a5.length; __q++) { var __sv = String(a5[__q]); for (var __r = 0; __r < __kw.length; __r++) { if (__sv.indexOf(__kw[__r]) >= 0) { __out.push(__q + ":" + __sv.slice(0, 90)); break; } } } __log("A5 len=" + a5.length + " hits=" + __out.length); for (var __q2 = 0; __q2 < __out.length && __q2 < 40; __q2++) __log("A5 " + __out[__q2]); } catch (e) { __log("A5DUMP ERR " + e); } }
          // The window/self binding is the WIN itself: a TRACE-only proxy here
          // became `this` for qualified calls (window.addEventListener) and split
          // the __WMS listener store by identity — inbound messages to the shell
          // silently vanished under TRACE (2026-10-18, the 4chan food-handler).
          }).call(win, win, doc, (__g(win, "__parentProxy") || window), window, win, win.location, win.navigator || navigator, "", __g(win, "__frameOrigin") || "null", win.history || history,
            win.addEventListener.bind(win), win.removeEventListener.bind(win), win.dispatchEvent.bind(win),
            win.setTimeout || setTimeout, win.setInterval || setInterval, win.clearTimeout || clearTimeout, win.clearInterval || clearInterval,
            win.requestAnimationFrame || requestAnimationFrame, win.cancelAnimationFrame || cancelAnimationFrame, win.XMLHttpRequest || XMLHttpRequest, win.fetch || fetch);
        } catch (e) { __log("iframe-wrap: " + String((e && e.message) || e)); }
      }
    }
    if (__TRACE) { try { __log("DDEM win._ddem=" + String(win._ddem || "-").slice(0, 220) + " ic=" + (typeof win.interstitialChallenge) + " ddm=" + (typeof win.ddm)); } catch (e) {} }
    // the document lifecycle, as a browser runs it for a frame: after the
    // scripts, DOMContentLoaded (readyState interactive), then load
    // (readyState complete) on the frame's WINDOW and the iframe element.
    // Widget programs gate their boot handshake on exactly these.
    __nrh(doc, "readyState", "interactive");
    var dclEv = { type: "DOMContentLoaded", target: doc, currentTarget: doc, bubbles: false, cancelable: false };
    try { doc.dispatchEvent(dclEv); } catch (e) {}
    try { win.dispatchEvent({ type: "DOMContentLoaded", target: doc, currentTarget: win, bubbles: false, cancelable: false }); } catch (e) {}
    setTimeout(function () {
      __nrh(doc, "readyState", "complete");
      var ev = { type: "load", target: el, currentTarget: el, bubbles: false, cancelable: false };
      if (__TRACE) { try { __log("IFRAME-LOADEVT listeners=" + ((__mr(el, "__l") && Object.keys(__mr(el, "__l")).join(",")) || "-") + " onload=" + (typeof el.onload) + " iframeErr=" + String(__DCI.t.iframeErr || "-") + " calls=" + ((globalThis.__CALLLOG && globalThis.__CALLLOG.length) || 0) + " RD=" + String((globalThis.__RD || []).slice(-24).join(","))); } catch (e) {} }
      try { if (typeof __dumpFile === "function" && globalThis.__CALLLOG) __dumpFile("calllog", "n=" + globalThis.__CALLLOG.length + "\n" + globalThis.__CALLLOG.slice(0, 200).join("\n")); } catch (e) {}
      try { if (typeof __dumpFile === "function" && globalThis.__WIVLOG) __dumpFile("wivlog", globalThis.__WIVLOG.join("\n")); } catch (e) {}
      try { win.dispatchEvent({ type: "load", target: doc, currentTarget: win, bubbles: false, cancelable: false }); } catch (e) {}
      try { if (globalThis.__CALLLOG && globalThis.__CALLLOG.length && typeof __dumpFile === "function") __dumpFile("calllog", globalThis.__CALLLOG.slice(0, 200).join("\n")); } catch (e) {}
      try { if (typeof win.onload === "function") win.onload(ev); } catch (e) {}
      try { if (typeof el.onload === "function") el.onload(ev); } catch (e) {}
      try { el.dispatchEvent(ev); } catch (e) {}
    }, 0);
  } catch (e) { if (__TRACE) { try { __log("IFRAME-PARSE-ERR " + e); } catch (e2) {} } }
  if (__TRACE) {
    var __sdump = function (name, maxd) {
      try {
        if (typeof __dumpFile !== "function") return;
        var lines = ["rs=" + doc.readyState + " bodykids=" + (doc.body ? doc.body.childNodes.length : -1) + " doc=" + (doc === document ? "M" : "S")];
        var walk2 = function (n, d) {
          if (!n || d > maxd) return;
          for (var i = 0; i < n.childNodes.length; i++) {
            var c = n.childNodes[i];
            if (c.nodeType !== 1) continue;
            lines.push(d + " " + (__mr(c, "tag") || "?") + (__mr(c, "attrs") && __mr(c, "attrs").id ? "#" + __mr(c, "attrs").id : "") + (__mr(c, "attrs") && __mr(c, "attrs")["class"] ? "." + String(__mr(c, "attrs")["class"]).split(" ")[0] : "") + (c.shadowRoot ? " SR[kids=" + c.shadowRoot.childNodes.length + "]" : ""));
            walk2(c, d + 1);
          }
        };
        var srRoot = doc.body && doc.body.shadowRoot;
        lines.push("(body SR " + (srRoot ? "id=" + (__mr(srRoot, "__srId", "_srId") || "?") + " kids=" + srRoot.childNodes.length : "none") + ")");
        if (srRoot) walk2(srRoot, 2);
        lines.push("(head kids=" + (doc.head ? doc.head.childNodes.length : -1) + ")");
        if (doc.head) { for (var hh = 0; hh < doc.head.childNodes.length; hh++) { var hc = doc.head.childNodes[hh]; if (hc.nodeType === 1) lines.push("  h " + (__mr(hc, "tag") || "?")); } }
        walk2(doc.body, 0);
        walk2(doc.documentElement, 0);
        __dumpFile(name, lines.join(String.fromCharCode(10)));
      } catch (eS) { try { __dumpFile(name, "ERR " + eS); } catch (eS2) {} }
    };
    var __sdn = (globalThis.__SDSEQ = (globalThis.__SDSEQ || 0) + 1);
    setTimeout(function () { __sdump("sdom-" + __sdn + "-p600", 5); }, 600);
    setTimeout(function () { __sdump("sdom-" + __sdn + "-p1500", 5); }, 1500);
    setTimeout(function () { __sdump("sdom-" + __sdn + "-p2800", 5); }, 2800);
  }
}
function makeText(text) {
  var n = Object.create(TP);
  __mk(n);
  __hide(n, "_sf_nodeType", 3);
  __ns(n, "text", text);
  __hide(n, "_sf_childNodes", []);
  __ns(n, "__l", {});
  __ns(n, "__parent", null);
  __hide(n, "_sf_nodeName", "#text");
  return n;
}
function textContentOf(n) {
  if (n.nodeType === 3) return __mr(n, "text");
  if (__mr(n, "tag") === "script" || __mr(n, "tag") === "style") return "";
  var out = "";
  for (var i = 0; i < n.childNodes.length; i++) out += textContentOf(n.childNodes[i]);
  return out;
}
// Chrome-true textContent (public getter): script/style content included.
// textContentOf above stays as the hidden page-text view (settle loop).
function textContentAll(n) {
  if (n.nodeType === 3) return __mr(n, "text");
  var out = "";
  for (var i = 0; i < n.childNodes.length; i++) out += textContentAll(n.childNodes[i]);
  return out;
}
function serializeNode(n, xml) {
  var xm = (xml === true);
  if (n.nodeType === 3) return escapeText(__mr(n, "text"), xm);
  if (n.nodeType === 8) return "<!--" + __DCI.nc.cdataOf(n) + "-->";
  if (n.nodeType === 10) {
    var __dtn = String(__g(n, "_sf_name") || "html"), __dtp = __g(n, "_sf_publicId"), __dts = __g(n, "_sf_systemId");
    return "<!DOCTYPE " + __dtn + ((__dtp || __dts) ? (" " + (__dtp ? "PUBLIC \"" + __dtp + "\"" : "SYSTEM") + (__dts ? " \"" + __dts + "\"" : "")) : "") + ">";
  }
  if (n.nodeType === 11) {
    var __fs = "";
    for (var __fi = 0; __fi < n.childNodes.length; __fi++) __fs += serializeNode(n.childNodes[__fi], xm);
    return __fs;
  }
  var tag = __mr(n, "tag") || n.tagName.toLowerCase();
  var s = "<" + tag;
  var keys = Object.keys(__mr(n, "attrs") || {});
  for (var i = 0; i < keys.length; i++) s += " " + keys[i] + '="' + escapeAttr(String(__mr(n, "attrs")[keys[i]]), xm) + '"';
  s += ">";
  if (["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"].indexOf(tag) >= 0) return s;
  var raw = !xm && (tag === "script" || tag === "style");
  for (var j = 0; j < n.childNodes.length; j++) {
    var c = n.childNodes[j];
    if (raw && c.nodeType === 3) s += __mr(c, "text");
    else s += serializeNode(c, xm);
  }
  return s + "</" + tag + ">";
}
function escapeText(s, xml) { var r = String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;"); return xml === true ? r : r.replace(/\u00A0/g, "&nbsp;"); }
function escapeAttr(s, xml) { var r = String(s).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;"); return xml === true ? r : r.replace(/\u00A0/g, "&nbsp;"); }

// a small HTML/SVG fragment parser (for innerHTML writes). The mode stack
// follows the spec's foreign-content switch: inside an <svg> subtree tag
// names keep the parser's case adjustments (clipPath/feDropShadow/…),
// attributes get the SVG case adjustments, and the nodes carry the SVG
// namespace; foreignObject/desc/title children parse as HTML again.
function parseFragment(src, svgCtx) {
  var out = [];
  var stack = [out];
  var mstack = [svgCtx ? "svg" : "html"];
  var i = 0;
  function top() { return stack[stack.length - 1]; }
  function __fragAttrs(rest, inSvg) {
    var attrs = {};
    var attrRe = /([^\s=/>]+)(?:\s*=\s*("([^"]*)"|'([^']*)'|[^\s>]*))?/g;
    var am;
    while ((am = attrRe.exec(String(rest)))) {
      if (!am[1]) continue;
      var val = am[3] !== undefined ? am[3] : am[4] !== undefined ? am[4] : am[2] !== undefined ? am[2].replace(/^["']|["']$/g, "") : "";
      var nm = am[1];
      if (inSvg && nm.indexOf(":") < 0 && SVGATTR) { var adjA = SVGATTR[nm]; if (adjA) nm = adjA; }
      attrs[nm] = val;
    }
    return attrs;
  }
  while (i < src.length) {
    var lt = src.indexOf("<", i);
    if (lt < 0) { if (i < src.length) top().push(makeText(src.slice(i))); break; }
    if (lt > i) top().push(makeText(src.slice(i, lt)));
    var gt = src.indexOf(">", lt);
    if (gt < 0) break;
    var inner = src.slice(lt + 1, gt);
    if (inner[0] === "/") {
      if (stack.length > 1) { stack.pop(); mstack.pop(); }
    } else if (inner[0] === "!" && src.charAt(lt + 2) === "-" && src.charAt(lt + 3) === "-") {
      // a real comment node (Chrome keeps source comments in the tree)
      var cend = src.indexOf("-->", lt + 4);
      top().push(__DCI.nc.comment(src.slice(lt + 4, cend < 0 ? src.length : cend)));
      i = cend < 0 ? src.length : cend + 3;
      continue;
    } else if (inner[0] === "!" || inner[0] === "?") {
      // doctype / declaration: skip
    } else {
      var selfclose = inner[inner.length - 1] === "/";
      // RAW-TEXT elements (HTML spec): <script>/<style> content is not
      // markup — a '<' inside JS (shifts, comparisons) must not start a tag.
      // Scan to the literal closing tag and keep everything as one text node.
      var tagm = /^([a-zA-Z][a-zA-Z0-9-]*)/.exec(inner);
      var rtag = tagm ? tagm[1].toLowerCase() : "";
      var m0 = mstack[mstack.length - 1];
      if (rtag === "script" || rtag === "style") {
        var close = src.toLowerCase().indexOf("</" + rtag, gt + 1);
        var endpos = close < 0 ? src.length : close;
        var rawEl;
        if (m0 === "svg") {
          var ra = (SVGADJ && SVGADJ[rtag]) || ["SVGElement", rtag];
          rawEl = makeElement(ra[1], __fragAttrs(inner.slice(rtag.length), true), "svg");
        } else {
          rawEl = makeElement(rtag, __fragAttrs(inner.slice(rtag.length), false));
        }
        if (rawEl.childNodes && endpos > gt + 1) {
          var txt = makeText(src.slice(gt + 1, endpos));
          __nsf(txt, "parentNode", rawEl); __ns(txt, "__parent", rawEl);
          rawEl.childNodes.push(txt);
        }
        top().push(rawEl);
        var after = close < 0 ? src.length : src.indexOf(">", close);
        i = after < 0 ? src.length : after + 1;
        continue;
      }
      var body = selfclose ? inner.slice(0, -1) : inner;
      var m = body.match(/^([a-zA-Z0-9-]+)/);
      if (m) {
        var tag = m[1].toLowerCase();
        var n, childMode;
        if (m0 === "svg") {
          var a = (SVGADJ && SVGADJ[tag]) || ["SVGElement", tag];
          n = makeElement(a[1], __fragAttrs(body.slice(m[1].length), true), "svg");
          childMode = (a[1] === "foreignObject" || a[1] === "desc" || a[1] === "title") ? "html" : "svg";
        } else {
          n = makeElement(tag, __fragAttrs(body.slice(m[1].length), false));
          childMode = (tag === "svg") ? "svg" : "html";
        }
        top().push(n);
        var voidEl = m0 !== "svg" && ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"].indexOf(tag) >= 0;
        if (!selfclose && !voidEl) { stack.push(n.childNodes); mstack.push(childMode); }
      }
    }
    i = gt + 1;
  }
  return out;
}

// minimal selector engine: tag/#id/.class/[attr]/[attr=val], descendant chains
function matchesSimple(n, sel) {
  sel = sel.trim();
  if (sel.indexOf(",") >= 0) {
    var parts = sel.split(",");
    for (var pi = 0; pi < parts.length; pi++) {
      if (matchesSimple(n, parts[pi])) return true;
    }
    return false;
  }
  var re = /([a-zA-Z0-9-]+)|#([\w-]+)|\.([\w-]+)|\[([\w-]+)(?:([~^$*|]?=)(?:"([^"]*)"|'([^']*)'|([^\]]*)))?\]/g;
  var m, tag = null, id = null, cls = [], attrs = [];
  while ((m = re.exec(sel))) {
    if (m[1] && !tag) tag = m[1].toLowerCase();
    else if (m[2]) id = m[2];
    else if (m[3]) cls.push(m[3]);
    else if (m[4]) attrs.push([m[4], m[6] !== undefined ? m[6] : m[7] !== undefined ? m[7] : m[8], m[5] || null]);
  }
  if (tag && tag !== "*" && __mr(n, "tag") !== tag) return false;
  if (id && (__mr(n, "attrs").id || "") !== id) return false;
  for (var i = 0; i < cls.length; i++) if ((__mr(n, "attrs")["class"] || "").split(/\s+/).indexOf(cls[i]) < 0) return false;
  for (var j = 0; j < attrs.length; j++) {
    var k = attrs[j][0], v = attrs[j][1], op = attrs[j][2];
    var have = __mr(n, "attrs")[k];
    if (have === undefined) return false;
    if (op === "=" && String(have) !== String(v)) return false;
    if (op === "~=" && String(have).split(/\s+/).indexOf(v) < 0) return false;
    if (op === "^=" && String(have).indexOf(v) !== 0) return false;
    if (op === "$=" && String(have).slice(-String(v).length) !== v) return false;
    if (op === "*=" && String(have).indexOf(v) < 0) return false;
  }
  return true;
}
function selectorAll(root, sel) {
  var chain = String(sel).trim().split(/\s+/);
  var last = chain[chain.length - 1];
  var rest = chain.slice(0, -1);
  var all = [];
  (function walk(n) {
    for (var i = 0; i < n.childNodes.length; i++) {
      var c = n.childNodes[i];
      if (c.nodeType === 1) { all.push(c); walk(c); }
    }
  })(root);
  var out = [];
  outer: for (var i = 0; i < all.length; i++) {
    var n = all[i];
    if (!matchesSimple(n, last)) continue;
    var p = n.parentNode;
    for (var r = rest.length - 1; r >= 0; r--) {
      var found = false;
      while (p && p.nodeType === 1) {
        if (matchesSimple(p, rest[r])) { found = true; p = p.parentNode; break; }
        p = p.parentNode;
      }
      if (!found) continue outer;
    }
    out.push(n);
  }
  return out;
}

// ───────────────────────────── canvas / gl / audio stubs ─────────────
function __parseColor(c) {
  var m = /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*(?:,\s*([\d.]+)\s*)?\)$/.exec(String(c));
  if (m) return [+m[1] & 255, +m[2] & 255, +m[3] & 255, m[4] === undefined ? 255 : Math.round(parseFloat(m[4]) * 255)];
  var h = /^#([0-9a-fA-F]{6})$/.exec(String(c));
  if (h) { var v = h[1]; return [parseInt(v.slice(0, 2), 16), parseInt(v.slice(2, 4), 16), parseInt(v.slice(4, 6), 16), 255]; }
  var h3 = /^#([0-9a-fA-F]{3})$/.exec(String(c));
  if (h3) { var v3 = h3[1]; return [parseInt(v3[0] + v3[0], 16), parseInt(v3[1] + v3[1], 16), parseInt(v3[2] + v3[2], 16), 255]; }
  return null;
}
function __pxEnsure(cv) {
  var w = (__mr(cv, "__width") !== undefined ? __mr(cv, "__width") : 300) >>> 0;
  var h = (__mr(cv, "__height") !== undefined ? __mr(cv, "__height") : 150) >>> 0;
  if (!cv.__px || cv.__pxW !== w || cv.__pxH !== h) { cv.__px = new Uint8ClampedArray(w * h * 4); cv.__pxW = w; cv.__pxH = h; }
  return cv.__px;
}
function __pxPaint(cv, x, y, w, h, col, clear) {
  var st = __pxEnsure(cv), W = cv.__pxW, H = cv.__pxH;
  x = Math.floor(x); y = Math.floor(y); w = Math.floor(w); h = Math.floor(h);
  var x0 = Math.max(0, x), y0 = Math.max(0, y), x1 = Math.min(W, x + w), y1 = Math.min(H, y + h);
  for (var yy = y0; yy < y1; yy++) for (var xx = x0; xx < x1; xx++) {
    var o = (yy * W + xx) * 4;
    st[o] = clear ? 0 : col[0]; st[o + 1] = clear ? 0 : col[1]; st[o + 2] = clear ? 0 : col[2]; st[o + 3] = clear ? 0 : col[3];
  }
}
function __pxRead(cv, x, y, w, h) {
  var st = __pxEnsure(cv), W = cv.__pxW, H = cv.__pxH;
  var out = new Uint8ClampedArray(Math.max(4, Math.max(0, w) * Math.max(0, h) * 4));
  x = Math.floor(x); y = Math.floor(y); w = Math.floor(w); h = Math.floor(h);
  for (var yy = 0; yy < h; yy++) for (var xx = 0; xx < w; xx++) {
    var sx = x + xx, sy = y + yy;
    if (sx < 0 || sy < 0 || sx >= W || sy >= H) continue;
    var so = (sy * W + sx) * 4, oo = (yy * Math.max(0, w) + xx) * 4;
    out[oo] = st[so]; out[oo + 1] = st[so + 1]; out[oo + 2] = st[so + 2]; out[oo + 3] = st[so + 3];
  }
  return out;
}
function seededBytes(seed, len) {
  var out = new Uint8ClampedArray(len);
  var s = seed % 2147483647;
  for (var i = 0; i < len; i++) { s = (s * 48271) % 2147483647; out[i] = s % 256; }
  return out;
}
function canvasGetContext(type, attrs) {
  if (type === "2d") {
    var calls = 0;
    var dirty = false;
    var ctx = {
      canvas: this,
      fillStyle: "#000000", strokeStyle: "#000000", lineWidth: 1, font: "10px sans-serif",
      textAlign: "start", textBaseline: "alphabetic", globalAlpha: 1, globalCompositeOperation: "source-over",
      shadowBlur: 0, shadowColor: "rgba(0, 0, 0, 0)", lineCap: "butt", lineJoin: "miter",
      save: function () {}, restore: function () {},
      beginPath: function () {}, closePath: function () {},
      moveTo: function () {}, lineTo: function () {}, arc: function () {}, arcTo: function () {},
      bezierCurveTo: function () {}, quadraticCurveTo: function () {}, rect: function () {},
      fill: function () {}, stroke: function () {}, clip: function () {},
      fillRect: function (x, y, w, h) { calls++; dirty = true; var col = __parseColor(this.fillStyle) || [0, 0, 0, 255]; __pxPaint(this.canvas, x, y, w, h, col, false); },
      strokeRect: function (x, y, w, h) { calls++; dirty = true; var col = __parseColor(this.strokeStyle) || [0, 0, 0, 255]; __pxPaint(this.canvas, x, y, w, 1, col, false); __pxPaint(this.canvas, x, y + h - 1, w, 1, col, false); __pxPaint(this.canvas, x, y, 1, h, col, false); __pxPaint(this.canvas, x + w - 1, y, 1, h, col, false); },
      clearRect: function (x, y, w, h) { __pxPaint(this.canvas, x, y, w, h, [0, 0, 0, 0], true); },
      fillText: function () { calls++; dirty = true; }, strokeText: function () { dirty = true; },
      measureText: function (t) { var m = { width: String(t).length * 7.2, actualBoundingBoxAscent: 8, actualBoundingBoxDescent: 2 }; __hide(m, "_s7b", "TextMetrics"); return m; },
      translate: function () {}, rotate: function () {}, scale: function () {}, transform: function () {}, setTransform: function () {}, resetTransform: function () {},
      drawImage: function () {}, putImageData: function () {},
      createLinearGradient: function () { return { addColorStop: function () {} }; },
      createRadialGradient: function () { return { addColorStop: function () {} }; },
      createPattern: function () { return {}; },
      getImageData: function (x, y, w, h) {
        return { data: __pxRead(this.canvas, x, y, w, h), width: w, height: h };
      },
      isPointInPath: function () { return false; },
      getContextAttributes: function () {
        // Chrome 154 truth (measured, gctx probe): alpha, colorSpace,
        // colorType, desynchronized, toneMapping, willReadFrequently — the
        // per-request options are echoed (DataDome's devicecheck reads these).
        var a2 = attrs || {};
        var cs = String(a2.colorSpace || "srgb"); if (cs !== "srgb" && cs !== "display-p3") cs = "srgb";
        var ct = String(a2.colorType || "unorm8"); if (ct !== "unorm8" && ct !== "float16") ct = "unorm8";
        return { alpha: a2.alpha !== undefined ? !!a2.alpha : true, colorSpace: cs, colorType: ct, desynchronized: a2.desynchronized !== undefined ? !!a2.desynchronized : false, toneMapping: { mode: "standard" }, willReadFrequently: a2.willReadFrequently !== undefined ? !!a2.willReadFrequently : false };
      },
      getLineDash: function () { return []; }, setLineDash: function () {},
    };
    __hide(ctx, "_s7b", (this instanceof OffscreenCanvas) ? "OffscreenCanvasRenderingContext2D" : "CanvasRenderingContext2D");
    __traceProto(ctx, "ctx2d");
    return ctx;
  }
  if (type === "webgl" || type === "experimental-webgl" || type === "webgl2") {
    var gl = {
      canvas: this,
      drawingBufferWidth: this.width, drawingBufferHeight: this.height,
      getExtension: function (name) {
        if (name === "WEBGL_debug_renderer_info") return { UNMASKED_VENDOR_WEBGL: 0x9245, UNMASKED_RENDERER_WEBGL: 0x9246 };
        return null;
      },
      getParameter: function (p) {
        if (p === 0x9245) return IDENT.gpuVendor;
        if (p === 0x9246) return IDENT.gpuRenderer;
        if (p === 0x1f00 || p === 0x1f01) return "WebKit";
        if (p === 0x1f02) return "WebGL 1.0 (OpenGL ES 2.0 Chromium)";
        if (p === 0x8b8c || p === 0x8b8d) return 0;
        if (p === 0x0d33 || p === 0x0d3a) return 4096;
        return 4096;
      },
      getSupportedExtensions: function () {
        return ["ANGLE_instanced_arrays", "EXT_blend_minmax", "EXT_color_buffer_half_float", "EXT_disjoint_timer_query", "EXT_float_blend", "EXT_frag_depth", "EXT_shader_texture_lod", "EXT_texture_compression_bptc", "EXT_texture_compression_rgtc", "EXT_texture_filter_anisotropic", "EXT_sRGB", "KHR_parallel_shader_compile", "OES_element_index_uint", "OES_fbo_render_mipmap", "OES_standard_derivatives", "OES_texture_float", "OES_texture_float_linear", "OES_texture_half_float", "OES_texture_half_float_linear", "OES_vertex_array_object", "WEBGL_color_buffer_float", "WEBGL_compressed_texture_s3tc", "WEBGL_compressed_texture_s3tc_srgb", "WEBGL_debug_renderer_info", "WEBGL_debug_shaders", "WEBGL_depth_texture", "WEBGL_draw_buffers", "WEBGL_lose_context", "WEBGL_multi_draw"];
      },
      getShaderPrecisionFormat: function () { return { rangeMin: 127, rangeMax: 127, precision: 23 }; },
      createBuffer: function () { return {}; }, createTexture: function () { return {}; }, createProgram: function () { return {}; },
      createShader: function () { return {}; }, shaderSource: function () {}, compileShader: function () {}, attachShader: function () {},
      linkProgram: function () {}, useProgram: function () {}, getShaderParameter: function () { return true; },
      getProgramParameter: function () { return true; }, getShaderInfoLog: function () { return ""; }, getProgramInfoLog: function () { return ""; },
      getAttribLocation: function () { return 0; }, getUniformLocation: function () { return 0; },
      bindBuffer: function () {}, bufferData: function () {}, enableVertexAttribArray: function () {},
      vertexAttribPointer: function () {}, drawArrays: function () {}, drawElements: function () {},
      enable: function () {}, disable: function () {}, clear: function () {}, clearColor: function () {},
      viewport: function () {}, getError: function () { return 0; },
      // Chrome 154 truth (measured, gctx probe): 10 fields, powerPreference
      // echoes the requested hint (DataDome devicecheck task 3 sends
      // {powerPreference:'low-power'} and reads it back).
      getContextAttributes: function () {
        var a = attrs || {};
        var pp = String(a.powerPreference || "default"); if (pp !== "low-power" && pp !== "high-performance") pp = "default";
        return { alpha: a.alpha !== undefined ? !!a.alpha : true, antialias: a.antialias !== undefined ? !!a.antialias : true, depth: a.depth !== undefined ? !!a.depth : true, desynchronized: a.desynchronized !== undefined ? !!a.desynchronized : false, failIfMajorPerformanceCaveat: a.failIfMajorPerformanceCaveat !== undefined ? !!a.failIfMajorPerformanceCaveat : false, powerPreference: pp, premultipliedAlpha: a.premultipliedAlpha !== undefined ? !!a.premultipliedAlpha : true, preserveDrawingBuffer: a.preserveDrawingBuffer !== undefined ? !!a.preserveDrawingBuffer : false, stencil: a.stencil !== undefined ? !!a.stencil : false, xrCompatible: false };
      },
    };
    __hide(gl, "_s7b", type === "webgl2" ? "WebGL2RenderingContext" : "WebGLRenderingContext");
    __traceProto(gl, "gl");
    return gl;
  }
  return null;
}
// OffscreenCanvas: Chromium exposes it window-side and inside workers, and
// its WebGL must report the same machine truth as the main realm. The
// device-page worker reads vendor/renderer from it and compares them with
// the page's values; without the class the worker threw before reporting
// (server: hasInconsistentWorkerValues).
function OffscreenCanvas(w, h) {
  if (!(this instanceof OffscreenCanvas)) throw new TypeError("Failed to construct 'OffscreenCanvas': Please use the 'new' operator");
  __hide(this, "_s7b", "OffscreenCanvas");
  this.width = Math.floor(Number(w) || 0);
  this.height = Math.floor(Number(h) || 0);
}
OffscreenCanvas.prototype.getContext = __native("getContext", 1, function (type, attrs) { if (__g(this, "_s7b") !== "OffscreenCanvas") throw new TypeError("Illegal invocation"); return canvasGetContext.apply(this, arguments); });
try { Object.defineProperty(OffscreenCanvas.prototype, Symbol.toStringTag, { value: "OffscreenCanvas", configurable: true }); } catch (e) {}
function canvasToDataURL() {
  // v0.1: stable placeholder; real rasterization is a Phase-1 fidelity item.
  return "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
}
function AudioStub() {}
function makeOfflineAudioContext(nc, len, sr) {
  var ctx = {
    sampleRate: sr || 44100, length: len || 44100, numberOfChannels: nc || 1,
    currentTime: 0, state: "suspended", destination: { channelCount: nc || 1 },
    oncomplete: null,
    createOscillator: function () {
      return { type: "sine", frequency: { value: 440, setValueAtTime: function () {} }, detune: { value: 0 }, connect: function () {}, start: function () {}, stop: function () {}, addEventListener: function () {} };
    },
    createDynamicsCompressor: function () {
      return { threshold: { value: -24, setValueAtTime: function () {} }, knee: { value: 30 }, ratio: { value: 12 }, attack: { value: 0.003 }, release: { value: 0.25 }, connect: function () {} };
    },
    createGain: function () { return { gain: { value: 1, setValueAtTime: function () {} }, connect: function () {} }; },
    createScriptProcessor: function () { return { connect: function () {}, onaudioprocess: null }; },
    createAnalyser: function () { return { connect: function () {}, getFloatFrequencyData: function () {} }; },
    decodeAudioData: function (b, ok) { if (ok) setTimeout(function () { ok(null); }, 0); },
    startRendering: function () {
      var self = this;
      return new Promise(function (resolve) {
        var channels = [];
        for (var c = 0; c < self.numberOfChannels; c++) {
          var data = new Float32Array(128);
          var s = 1234567 + c * 999983;
          for (var i = 0; i < data.length; i++) { s = (s * 48271) % 2147483647; data[i] = (s / 2147483647 - 0.5) * 0.0001; }
          channels.push(data);
        }
        resolve({ sampleRate: self.sampleRate, length: self.length, numberOfChannels: self.numberOfChannels, getChannelData: function (i) { return channels[i] || new Float32Array(128); } });
      });
    },
  };
  return ctx;
}
function OfflineAudioContext(nc, len, sr) { return makeOfflineAudioContext(nc, len, sr); }
function AudioContext() { var c = makeOfflineAudioContext(2, 44100, 44100); c.state = "running"; return c; }

// ───────────────────────────── window ───────────────────────────────
var window = globalThis;
window.window = window; window.self = window; window.top = window; window.parent = window;
window.frames = []; window.frameElement = null; window.opener = null; window.closed = false;
window.length = 0;
window.name = "";
try { Object.defineProperty(globalThis, Symbol.toStringTag, { value: "Window", configurable: true }); } catch (e) {}

// TRACE-only: watch _cf_chl_opt slot traffic (writes + undefined reads)
if (__TRACE) {
  (function () {
    var __optLog = [];
    globalThis.__optLog = __optLog;
    var __optCur = null;
    try {
      Object.defineProperty(window, "_cf_chl_opt", {
        configurable: true, enumerable: true,
        get: function () { return __optCur; },
        set: function (v) {
          __optCur = v;
          if (__optLog.length < 400) {
            try {
              var __ks = v && typeof v === "object" ? Object.keys(v).slice(0, 40).join(",") : typeof v;
              var __stO = String(new Error().stack).split("\n");
              var __srcO = "";
              for (var __so = 1; __so < __stO.length && __so < 6; __so++) { var __mo = __stO[__so].match(/<input>:(\d+):(\d+)/); if (__mo) { __srcO = __mo[1] + ":" + __mo[2]; break; } }
              __optLog.push("SETOBJ " + __ks + (__srcO ? " @" + __srcO : ""));
            } catch (eO) {}
          }
          try {
            __optCur = new Proxy(v, {
              get: function (t, k) {
                var r = t[k];
                if (r === undefined && typeof k === "string" && __optLog.length < 400) {
                  var __srcG = "";
                  try {
                    var __stG = String(new Error().stack).split("\n");
                    for (var __sg = 1; __sg < __stG.length && __sg < 6; __sg++) { var __mg = __stG[__sg].match(/<input>:(\d+):(\d+)/); if (__mg) { __srcG = __mg[1] + ":" + __mg[2]; break; } }
                  } catch (eG) {}
                  __optLog.push("GET? " + k + (__srcG ? " @" + __srcG : ""));
                }
                return r;
              },
              set: function (t, k, val) {
                if (__optLog.length < 400) {
                  var d;
                  try {
                    d = val === null ? "null"
                      : val === undefined ? "undefined"
                      : val.nodeType !== undefined
                        ? "node:" + String(__mr(val, "tag") || val.nodeName)
                        : typeof val === "object" || typeof val === "function"
                          ? (typeof val === "object" ? "obj[" + Object.keys(val).slice(0, 8).join(",") + "]" : typeof val)
                          : String(val).slice(0, 40);
                    var st = String(new Error().stack).split("\n");
                    var src = "";
                    for (var si = 1; si < st.length && si < 6; si++) {
                      var m = st[si].match(/<input>:(\d+):(\d+)/);
                      if (m) { src = m[1] + ":" + m[2]; break; }
                    }
                    __optLog.push("SET " + String(k) + "=" + d + (src ? " @" + src : ""));
                  } catch (e) { __optLog.push("SET " + String(k) + "=?"); }
                }
                t[k] = val; return true;
              },
            });
          } catch (e) {}
        },
      });
    } catch (e) {}
  })();
}
// TRACE-only: eval observability for the decoder-factory chain
if (__TRACE) {
  try {
    var __evRaw = globalThis.eval;
    var __evWrap = function (s) {
      try {
        __log("EVALCALL len=" + String(s).length + " head=" + String(s).slice(0, 220) + " stk=" + String((new Error()).stack || "").split("\n").slice(1, 5).join(" | "));
        if (String(s).length > 400000 && typeof __dumpFile === "function") { try { __dumpFile("eval-big", String(s)); } catch (e) {} }
      } catch (e) {}
      return __evRaw(s);
    };
    try { globalThis.eval = __evWrap; globalThis.__evPristine = __evRaw; __log("EVALOVR ok=" + (globalThis.eval === __evWrap)); } catch (e) { __log("EVALOVR FAIL " + e); }
    var __probeR = (0, eval)("function __EvalProbe(){return 4242}; __EvalProbe();");
    __log("EVALTEST " + __probeR + " probeType=" + (typeof globalThis.__EvalProbe));
  } catch (e) { __log("EVALTEST ERR " + e); }
}
window.devicePixelRatio = IDENT.dpr;
window.innerWidth = IDENT.screenW; window.innerHeight = IDENT.availH - 133;
window.outerWidth = IDENT.screenW; window.outerHeight = IDENT.outerH;
window.screenX = 0; window.screenY = 0;
window.scrollX = 0; window.scrollY = 0; window.pageXOffset = 0; window.pageYOffset = 0;
window.isSecureContext = PAGE_URL.indexOf("https:") === 0;
window.crossOriginIsolated = false;
window.origin = (function (u) { var m = u.match(/^(https?:)\/\/([^\/]+)/); return m ? m[1] + "//" + m[2] : u; })(PAGE_URL);

addEventTarget(window);
window.onload = null; window.onerror = null; window.onunhandledrejection = null;

window.navigator = (function () {
  var major = IDENT.chromeMajor;
  var brands = [
    { brand: "Not A(Brand", version: "99" },
    { brand: "Chromium", version: major },
  ];
  var nav = {
    userAgent: IDENT.ua,
    appVersion: IDENT.ua.replace(/^Mozilla\//, ""),
    appName: "Netscape",
    appCodeName: "Mozilla",
    product: "Gecko",
    productSub: "20030107",
    vendor: "Google Inc.",
    vendorSub: "",
    platform: "Linux x86_64",
    language: IDENT.locale,
    languages: IDENT.languages,
    hardwareConcurrency: IDENT.cores,
    deviceMemory: IDENT.memoryGb,
    maxTouchPoints: 0,
    doNotTrack: null,
    cookieEnabled: true,
    onLine: true,
    pdfViewerEnabled: true,
    plugins: (function () {
      var names = [["PDF Viewer", "internal-pdf-viewer", "Portable Document Format"], ["Chrome PDF Viewer", "internal-pdf-viewer", "Portable Document Format"], ["Chromium PDF Viewer", "internal-pdf-viewer", "Portable Document Format"], ["Microsoft Edge PDF Viewer", "internal-pdf-viewer", "Portable Document Format"], ["WebKit built-in PDF", "internal-pdf-viewer", "Portable Document Format"]];
      var arr = names.map(function (p) { return { name: p[0], description: p[2], filename: p[1], length: 2 }; });
      arr.item = function (i) { return this[i] || null; };
      arr.namedItem = function (n) { for (var i = 0; i < this.length; i++) if (this[i].name === n) return this[i]; return null; };
      arr.refresh = function () {};
      arr.length = names.length;
      return arr;
    })(),
    mimeTypes: (function () {
      var arr = [{ type: "application/pdf", suffixes: "pdf", description: "Portable Document Format" }, { type: "text/pdf", suffixes: "pdf", description: "Portable Document Format" }];
      arr.item = function (i) { return this[i] || null; };
      arr.namedItem = function (n) { for (var i = 0; i < this.length; i++) if (this[i].type === n) return this[i]; return null; };
      arr.length = arr.length;
      return arr;
    })(),
    userAgentData: {
      brands: brands,
      mobile: false,
      platform: IDENT.platform === "Linux" ? "Linux" : IDENT.platform,
      getHighEntropyValues: function (keys) {
        return Promise.resolve({
          architecture: "x86", bitness: "64", model: "", platformVersion: IDENT.platformVersion || "6.6.0",
          uaFullVersion: major + ".0.0.0",
          fullVersionList: brands.map(function (b) { return { brand: b.brand, version: b.version + ".0.0.0" }; }),
        });
      },
      toJSON: function () { return { brands: brands, mobile: false, platform: "Linux" }; },
    },
    permissions: {
      query: function (d) { return Promise.resolve({ state: "prompt", name: (d && d.name) || "", onchange: null, addEventListener: function () {}, removeEventListener: function () {} }); },
    },
    clipboard: { writeText: function () { return Promise.resolve(); }, readText: function () { return Promise.resolve(""); } },
    javaEnabled: function () { return false; },
    getUserMedia: function (c, onOk, onErr) {
      if (arguments.length < 3) throw new TypeError("Failed to execute 'getUserMedia' on 'Navigator': 3 arguments required, but only " + arguments.length + " present.");
      c = c || {};
      if (!c.audio && !c.video) throw new TypeError("Failed to execute 'getUserMedia' on 'Navigator': At least one of audio and video must be requested");
      setTimeout(function () { try { if (typeof onErr === "function") onErr({ name: "NotAllowedError", message: "Permission denied" }); } catch (e) {} }, 0);
    },
    webkitGetUserMedia: function (c, onOk, onErr) {
      if (arguments.length < 3) throw new TypeError("Failed to execute 'webkitGetUserMedia' on 'Navigator': 3 arguments required, but only " + arguments.length + " present.");
      c = c || {};
      if (!c.audio && !c.video) throw new TypeError("Failed to execute 'webkitGetUserMedia' on 'Navigator': At least one of audio and video must be requested");
      setTimeout(function () { try { if (typeof onErr === "function") onErr({ name: "NotAllowedError", message: "Permission denied" }); } catch (e) {} }, 0);
    },
    sendBeacon: function (url, data) {
      if (arguments.length < 1) throw new TypeError("Failed to execute 'sendBeacon' on 'Navigator': 1 argument required, but only 0 present.");
      try {
        var u = resolveUrl(PAGE_URL, String(url));
        var body = data === undefined || data === null ? "" : String(data);
        var r = __wireCall("POST", u, "xhr", JSON.stringify([["Content-Type", "text/plain;charset=UTF-8"]]), body);
        if (__TRACE) { try { var j = JSON.parse(r); __log("BEACON " + u + " -> " + (j.ok ? j.status : "ERR")); } catch (e) {} }
        return true;
      } catch (e) { return false; }
    },
    getBattery: function () {
      // Machine truth when a battery exists (Chrome reads the system too);
      // desktops keep Chrome's fallback of level 1 / charging true.
      var lvl = (typeof IDENT.batteryLevel === "number") ? IDENT.batteryLevel : 1;
      var chg = (typeof IDENT.batteryCharging === "boolean") ? IDENT.batteryCharging : true;
      return Promise.resolve({ charging: chg, level: lvl, chargingTime: 0, dischargingTime: Infinity });
    },
    registerProtocolHandler: function () {},
    geolocation: { getCurrentPosition: function (ok) { if (ok) setTimeout(function () { ok({ coords: { latitude: 0, longitude: 0, accuracy: 1 } }); }, 0); }, watchPosition: function () { return 0; }, clearWatch: function () {} },
  };
  return nav;
})();

// ── ST-5: Permissions / PermissionStatus / NetworkInformation / StorageManager
// (chrome-truth measured in probe-st5d on the installed Chromium 154,
// 2026-10-18: prototype own-name orders, descriptor flags, ctor messages,
// method lengths — replicated byte-exact). The instances ride the §15
// rehome/additions pipeline like every other navigator value; the ctor
// families get noProtoW + source-clear via the §12 finish list, and the
// EventTarget parentage is linked there once EventTarget exists.) ──
(function () {
  var g = globalThis;
  function nm4(C, nm) { try { Object.defineProperty(C, "name", { value: nm, configurable: true }); } catch (e) {} }
  function noCon(C) { try { delete C.prototype.constructor; } catch (e) {} }
  function mth(C, nm, ln, fn) {
    try { Object.defineProperty(C.prototype, nm, { value: __native(nm, ln, fn), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  }
  function sacc(C, nm, store, dflt) {
    try { Object.defineProperty(C.prototype, nm, { get: __native("get " + nm, 0, function () { if (!(this instanceof C)) throw new TypeError("Illegal invocation"); var h = __g(this, store); return h === undefined ? dflt : h; }), enumerable: true, configurable: true }); } catch (e) {}
  }
  function hacc(C, nm, store) {
    try {
      Object.defineProperty(C.prototype, nm, {
        get: __native("get " + nm, 0, function () { if (!(this instanceof C)) throw new TypeError("Illegal invocation"); return __g(this, store, null); }),
        set: __native("set " + nm, 1, function (v) { if (!(this instanceof C)) throw new TypeError("Illegal invocation"); __hide(this, store, v); }),
        enumerable: true, configurable: true });
    } catch (e) {}
  }
  function setCon(C) {
    try { Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, enumerable: false, configurable: true }); } catch (e) {}
  }
  function tag(C, t) {
    try { Object.defineProperty(C.prototype, Symbol.toStringTag, { value: t, configurable: true }); } catch (e) {}
  }

  // Permissions — proto order [query, constructor]
  var Permissions = function () {
    if (new.target === undefined) throw new TypeError("Illegal constructor");
    throw new TypeError("Failed to construct 'Permissions': Illegal constructor");
  };
  nm4(Permissions, "Permissions");
  noCon(Permissions);
  mth(Permissions, "query", 1, function (d) {
    if (arguments.length < 1) return Promise.reject(new TypeError("Failed to execute 'query' on 'Permissions': 1 argument required, but only 0 present."));
    if (!(this instanceof Permissions)) return Promise.reject(new TypeError("Failed to execute 'query' on 'Permissions': Illegal invocation"));
    if (d !== null && d !== undefined && typeof d !== "object") return Promise.reject(new TypeError("Failed to execute 'query' on 'Permissions': parameter 1 is not of type 'object'."));
    var nm = (d === null || d === undefined) ? undefined : d.name;
    if (nm !== undefined) {
      var KNOWN = { geolocation: 1, notifications: 1, "persistent-storage": 1, push: 1, "screen-wake-lock": 1, nfc: 1, bluetooth: 1, "background-fetch": 1, "background-sync": 1, "ambient-light-sensor": 1, accelerometer: 1, gyroscope: 1, magnetometer: 1, "clipboard-read": 1, "clipboard-write": 1, "display-capture": 1, "payment-handler": 1, "idle-detection": 1, "periodic-background-sync": 1, "speaker-selection": 1, "top-level-storage-access": 1, "window-management": 1, "local-fonts": 1, "storage-access": 1, camera: 1, microphone: 1, "captured-surface-control": 1 };
      if (!Object.prototype.hasOwnProperty.call(KNOWN, String(nm))) {
        return Promise.reject(new TypeError("Failed to execute 'query' on 'Permissions': Failed to read the 'name' property from 'PermissionDescriptor': The provided value '" + String(nm) + "' is not a valid enum value of type PermissionName."));
      }
    }
    var st = Object.create(PermissionStatus.prototype);
    __hide(st, "_ps_name", nm === undefined ? "" : String(nm));
    __hide(st, "_ps_state", "prompt");
    return Promise.resolve(st);
  });
  setCon(Permissions);
  tag(Permissions, "Permissions");

  // PermissionStatus — proto order [name, state, onchange, constructor]
  var PermissionStatus = function () {
    if (new.target === undefined) throw new TypeError("Illegal constructor");
    throw new TypeError("Failed to construct 'PermissionStatus': Illegal constructor");
  };
  nm4(PermissionStatus, "PermissionStatus");
  noCon(PermissionStatus);
  sacc(PermissionStatus, "name", "_ps_name", "");
  sacc(PermissionStatus, "state", "_ps_state", "prompt");
  hacc(PermissionStatus, "onchange", "_ps_onchange");
  setCon(PermissionStatus);
  tag(PermissionStatus, "PermissionStatus");

  // NetworkInformation — proto order [onchange, effectiveType, rtt, downlink, saveData, constructor]
  var NetworkInformation = function () {
    if (new.target === undefined) throw new TypeError("Illegal constructor");
    throw new TypeError("Failed to construct 'NetworkInformation': Illegal constructor");
  };
  nm4(NetworkInformation, "NetworkInformation");
  noCon(NetworkInformation);
  hacc(NetworkInformation, "onchange", "_ni_onchange");
  sacc(NetworkInformation, "effectiveType", "_ni_et", "4g");
  sacc(NetworkInformation, "rtt", "_ni_rtt", 100);
  sacc(NetworkInformation, "downlink", "_ni_dl", 1.65);
  sacc(NetworkInformation, "saveData", "_ni_sd", false);
  setCon(NetworkInformation);
  tag(NetworkInformation, "NetworkInformation");

  // StorageManager — proto order [estimate, persisted, constructor, getDirectory, persist]
  var StorageManager = function () {
    if (new.target === undefined) throw new TypeError("Illegal constructor");
    throw new TypeError("Failed to construct 'StorageManager': Illegal constructor");
  };
  nm4(StorageManager, "StorageManager");
  noCon(StorageManager);
  mth(StorageManager, "estimate", 0, function () {
    if (!(this instanceof StorageManager)) throw new TypeError("Failed to execute 'estimate' on 'StorageManager': Illegal invocation");
    return Promise.resolve({ quota: 10737418240, usage: 0, usageDetails: {} });
  });
  mth(StorageManager, "persisted", 0, function () {
    if (!(this instanceof StorageManager)) throw new TypeError("Failed to execute 'persisted' on 'StorageManager': Illegal invocation");
    return Promise.resolve(false);
  });
  setCon(StorageManager);
  mth(StorageManager, "getDirectory", 0, function () {
    if (!(this instanceof StorageManager)) throw new TypeError("Failed to execute 'getDirectory' on 'StorageManager': Illegal invocation");
    return Promise.resolve(__opfsDir(""));
  });
  mth(StorageManager, "persist", 0, function () {
    if (!(this instanceof StorageManager)) throw new TypeError("Failed to execute 'persist' on 'StorageManager': Illegal invocation");
    return new Promise(function () {});
  });
  tag(StorageManager, "StorageManager");

  var ni = Object.create(NetworkInformation.prototype);
  __hide(ni, "_ni_et", "4g"); __hide(ni, "_ni_rtt", 100); __hide(ni, "_ni_dl", 1.65); __hide(ni, "_ni_sd", false);
  window.navigator.permissions = Object.create(Permissions.prototype);
  window.navigator.connection = ni;
  window.navigator.storage = Object.create(StorageManager.prototype);
  g.Permissions = Permissions;
  g.PermissionStatus = PermissionStatus;
  g.NetworkInformation = NetworkInformation;
  g.StorageManager = StorageManager;
})();

window.screen = {
  width: IDENT.screenW, height: IDENT.screenH,
  availWidth: IDENT.availW, availHeight: IDENT.availH,
  availLeft: 0, availTop: 0,
  colorDepth: 24, pixelDepth: 24,
  orientation: { type: "landscape-primary", angle: 0, onchange: null, lock: function () { return Promise.resolve(); }, unlock: function () {} },
  isExtended: false,
};

window.location = (function (u) {
  var m = u.match(/^(https?:)\/\/([^\/?#]*)(\/[^?#]*)?(\?[^#]*)?(#.*)?$/);
  var host = m ? m[2] : "example.com";
  var parts = host.split(":");
  var _lu = u;
  function _locParts() {
    var mm = _lu.match(/^(https?:)\/\/([^\/?#]*)(\/[^?#]*)?(\?[^#]*)?(#.*)?$/);
    var h2 = mm ? mm[2] : "example.com";
    var ps = h2.split(":");
    return { protocol: mm ? mm[1] + "//" : "https://", host: h2, hostname: ps[0], port: ps[1] || "",
      pathname: mm && mm[3] ? mm[3] : "/", search: mm && mm[4] ? mm[4] : "", hash: mm && mm[5] ? mm[5] : "",
      origin: (mm ? mm[1] + "//" + h2 : "https://example.com") };
  }
  function _lset(nm, v) {
    var pp = _locParts(); var s = String(v);
    if (nm === "protocol") pp.protocol = s.replace(/:?$/, ":");
    else if (nm === "host") { pp.host = s; pp.hostname = s.split(":")[0]; pp.port = s.split(":")[1] || ""; }
    else if (nm === "hostname") { pp.hostname = s; pp.host = s + (pp.port ? ":" + pp.port : ""); }
    else if (nm === "port") { pp.port = s; pp.host = pp.hostname + (s ? ":" + s : ""); }
    else pp[nm] = s;
    _lu = pp.protocol + "//" + pp.host + pp.pathname + pp.search + pp.hash;
  }
  // Chrome truth: exactly these 15 own props, in this order (probe-msgs locOwn).
  var loc = Object.create((window.Location && window.Location.prototype) || Object.prototype);
  function _lacc(nm, hs) {
    var d = { get: __native("get " + nm, 0, function () { var pp = _locParts(); return nm === "origin" ? pp.origin : pp[nm]; }), enumerable: true, configurable: false };
    if (hs) d.set = __native("set " + nm, 1, function (v) { _lset(nm, v); });
    return d;
  }
  function _ldef(nm, val, ln, w, e) { try { Object.defineProperty(loc, nm, { value: __native(nm, ln || 0, val), writable: !!w, enumerable: !!e, configurable: false }); } catch (e) {} }
  _ldef("valueOf", function () { return _lu; }, 0, false, false);
  Object.defineProperty(loc, "ancestorOrigins", { get: __native("get ancestorOrigins", 0, function () { return { length: 0, contains: function () { return false; }, item: function () { return null; } }; }), enumerable: true, configurable: false });
  Object.defineProperty(loc, "href", { get: __native("get href", 0, function () { return _lu; }), set: __native("set href", 1, function (v) { __nav("assign", resolveUrl(_lu, String(v))); }), enumerable: true, configurable: false });
  Object.defineProperty(loc, "origin", _lacc("origin", false));
  Object.defineProperty(loc, "protocol", _lacc("protocol", true));
  Object.defineProperty(loc, "host", _lacc("host", true));
  Object.defineProperty(loc, "hostname", _lacc("hostname", true));
  Object.defineProperty(loc, "port", _lacc("port", true));
  Object.defineProperty(loc, "pathname", _lacc("pathname", true));
  Object.defineProperty(loc, "search", _lacc("search", true));
  Object.defineProperty(loc, "hash", _lacc("hash", true));
  _ldef("assign", function (x) { __nav("assign", resolveUrl(_lu, String(x))); }, 1, false, true);
  _ldef("reload", function () { __nav("reload", _lu); }, 0, false, true);
  _ldef("replace", function (x) { __nav("replace", resolveUrl(_lu, String(x))); }, 1, false, true);
  _ldef("toString", function () { return _lu; }, 0, false, true);
  // page-initiated navigation. A real browser tears the document down:
  // timers stop firing, queued scripts never run. The engine acts on __navReq.
  function __nav(kind, url) {
    try { __log("NAV " + kind + " " + String(url)); } catch (e) {}
    if (__DCI.t.navReq) return;
    __DCI.t.navReq = { kind: kind, url: String(url) };
    try { __timers.length = 0; } catch (e) {}
    try { __rafQ.length = 0; } catch (e) {}
  }
  return loc;
})(PAGE_URL);

window.history = (function () {
  var h = Object.create((window.History && window.History.prototype) || Object.prototype);
  __hide(h, "_h_len", 1); __hide(h, "_h_state", null); __hide(h, "_h_scroll", "auto"); __hide(h, "_s7b", "History");
  return h;
})();

window.performance = (function () {
  var p = Object.create((window.Performance && window.Performance.prototype) || Object.prototype);
  __hide(p, "_p_to", __DCI.BOOT_MS);
  __hide(p, "_p_timing", { navigationStart: __DCI.BOOT_MS - 120, domLoading: __DCI.BOOT_MS - 60, domComplete: __DCI.BOOT_MS });
  __hide(p, "_p_nav", { type: 0, redirectCount: 0 });
  __hide(p, "_p_mem", { jsHeapSizeLimit: 4294705152, totalJSHeapSize: 41943040, usedJSHeapSize: 33554432 });
  return p;
})();

// ── console: Chrome's own-name order + native-shaped methods (probe-st5d,
// 2026-10-18 — toString integrity: console.log/error/... must render native).
// The heap numbers are the engine's standing memory model (same constants as
// performance.memory); console.memory is a {get,set} accessor like Chrome's.
window.console = (function () {
  var C = {};
  function mk(nm, ln, fn) { return __native(nm, ln, fn); }
  function noop() {}
  C.debug = mk("debug", 0, noop);
  C.error = mk("error", 0, function () { __log("error: " + Array.prototype.join.call(arguments, " ")); });
  C.info = mk("info", 0, function () { __log("info: " + Array.prototype.join.call(arguments, " ")); });
  C.log = mk("log", 0, function () {
    var j = Array.prototype.join.call(arguments, " ");
    if (j.indexOf("diff check at") >= 0 && arguments.length > 1) { try { __dumpFile("fpjson", String(arguments[1])); } catch (e) {} }
    __log("log: " + j);
  });
  C.warn = mk("warn", 0, function () { __log("warn: " + Array.prototype.join.call(arguments, " ")); });
  C.dir = mk("dir", 0, noop); C.dirxml = mk("dirxml", 0, noop); C.table = mk("table", 0, noop);
  C.trace = mk("trace", 0, noop);
  C.group = mk("group", 0, noop); C.groupCollapsed = mk("groupCollapsed", 0, noop); C.groupEnd = mk("groupEnd", 0, noop);
  C.clear = mk("clear", 0, noop);
  C.count = mk("count", 0, noop); C.countReset = mk("countReset", 0, noop); C.assert = mk("assert", 0, noop);
  C.profile = mk("profile", 0, noop); C.profileEnd = mk("profileEnd", 0, noop);
  C.time = mk("time", 0, noop); C.timeLog = mk("timeLog", 0, noop); C.timeEnd = mk("timeEnd", 0, noop); C.timeStamp = mk("timeStamp", 0, noop);
  C.context = mk("context", 1, function () {});
  C.createTask = mk("createTask", 0, function () { return { run: function (f) { return f(); } }; });
  Object.defineProperty(C, "memory", {
    get: __native("get memory", 0, function () { return { jsHeapSizeLimit: 4294705152, totalJSHeapSize: 41943040, usedJSHeapSize: 33554432 }; }),
    set: __native("set memory", 1, function (v) {}),
    enumerable: true, configurable: true,
  });
  try { Object.setPrototypeOf(C, {}); } catch (e) {}
  return C;
})();

// ST-11 U2b-1: Chrome order [loadTimes, csi, app]; csi/loadTimes render
// anonymous (`function () { [native code] }`, measured H 42d98d98).
window.chrome = {
  loadTimes: __native("", 0, function () { return {}; }),
  csi: __native("", 0, function () { return { startE: __DCI.BOOT_MS, onloadT: __DCI.BOOT_MS + 120, pageT: 120.5, tran: 15 }; }),
  app: { isInstalled: false, InstallState: { DISABLED: "disabled", INSTALLED: "installed", NOT_INSTALLED: "not_installed" }, RunningState: { CANNOT_RUN: "cannot_run", READY_TO_RUN: "ready_to_run", RUNNING: "running" } },
};

// ── window.external + External (chrome-truth, probed 2026-10-18): accessor
// {get,set} on window with [Replaceable]-style set; prototype carries
// [AddSearchProvider, IsSearchProviderInstalled, constructor]; both calls are
// native-shaped no-ops returning undefined; instances own nothing;
// [object External]. ──
(function () {
  try {
    var XP = {};
    var XC = function External() {
      if (new.target === undefined) throw new TypeError("Illegal constructor");
      throw new TypeError("Failed to construct 'External': Illegal constructor");
    };
    try { Object.defineProperty(XC, "prototype", { value: XP, writable: false, enumerable: false, configurable: false }); } catch (e) {}
    Object.defineProperty(XP, "AddSearchProvider", { value: __native("AddSearchProvider", 0, function () {}), writable: true, enumerable: true, configurable: true });
    Object.defineProperty(XP, "IsSearchProviderInstalled", { value: __native("IsSearchProviderInstalled", 0, function () {}), writable: true, enumerable: true, configurable: true });
    Object.defineProperty(XP, "constructor", { value: XC, writable: true, configurable: true });
    try { Object.defineProperty(XP, Symbol.toStringTag, { value: "External", configurable: true }); } catch (e) {}
    var XO = Object.create(XP);
    Object.defineProperty(window, "external", {
      get: __native("get external", 0, function () { return XO; }),
      set: __native("set external", 1, function (v) { try { Object.defineProperty(window, "external", { value: v, writable: true, enumerable: true, configurable: true }); } catch (e) {} }),
      enumerable: true, configurable: true,
    });
    globalThis.External = XC; window.External = XC;
  } catch (e) {}
})();

window.crypto = {
  getRandomValues: function (a) { for (var i = 0; i < a.length; i++) a[i] = Math.floor(Math.random() * 256); return a; },
  randomUUID: function () {
    return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, function (c) {
      var r = (Math.random() * 16) | 0; return (c === "x" ? r : (r & 0x3) | 0x8).toString(16);
    });
  },
};
// NOTE: do NOT pin `subtle` to undefined here — a non-configurable defineProperty
// silently kills the crypto.subtle shim assigned later in this file, and real
// Chrome HAS crypto.subtle (its absence is itself a bot tell).

function makeStorage() {
  var data = {};
  var st = {}; // Chrome: the storage surface lives on Storage.prototype; instances own nothing
  var METHOD_NAMES = { length: 1, clear: 1, getItem: 1, key: 1, removeItem: 1, setItem: 1 };
  try {
    var proxy = new Proxy(st, {
      get: function (t, k) { if (k in t) return t[k]; return Object.prototype.hasOwnProperty.call(data, k) ? data[k] : undefined; },
      set: function (t, k, v) { if (METHOD_NAMES[k] || k in t) { return true; } data[String(k)] = String(v); return true; },
    });
    __hide(st, "_st_data", data); __hide(proxy, "_st_data", data);
    return proxy;
  } catch (e) {
    __hide(st, "_st_data", data);
    return st;
  }
}
window.localStorage = makeStorage();
window.sessionStorage = makeStorage();

window.matchMedia = function (q) {
  var m = String(q);
  var res = { matches: false, media: m, onchange: null, addListener: function () {}, removeListener: function () {}, addEventListener: function () {}, removeEventListener: function () {}, dispatchEvent: function () { return false; } };
  var dpr = window.devicePixelRatio || 1;
  var sw = (window.screen && window.screen.width) || 0;
  var sh = (window.screen && window.screen.height) || 0;
  function evalFeature(f) {
    f = f.trim();
    if (/^\(pointer:\s*fine\)$/.test(f)) return true;
    if (/^\(hover:\s*hover\)$/.test(f)) return true;
    if (/prefers-color-scheme:\s*light/.test(f)) return true;
    if (/prefers-color-scheme:\s*dark/.test(f)) return false;
    if (/prefers-reduced-motion/.test(f)) return false;
    var mm = /^\((min-|max-)?([a-z-]+)\s*:\s*([0-9.]+)\s*(px|dppx|dpi)?\)$/.exec(f);
    if (!mm) return false;
    var mod = mm[1] || "", feat = mm[2], val = parseFloat(mm[3]), unit = mm[4] || "";
    var actual;
    if (feat === "width") actual = window.innerWidth;
    else if (feat === "height") actual = window.innerHeight;
    else if (feat === "device-width") actual = sw;
    else if (feat === "device-height") actual = sh;
    else if (feat === "resolution" || feat === "device-pixel-ratio") { actual = dpr; if (unit === "dpi") val = val / 96; }
    else return false;
    if (mod === "min-") return actual >= val;
    if (mod === "max-") return actual <= val;
    return actual === val;
  }
  var parts = m.split(/\s+and\s+/i);
  var all = true;
  for (var pi = 0; pi < parts.length; pi++) { if (!evalFeature(parts[pi])) { all = false; break; } }
  res.matches = all;
  return res;
};

// ── computed style: the CSSOM VALUE MODEL ────────────────────────────────
// There is no layout engine, but computed values are not layout: they are
// the cascade of spec initial values + UA defaults + author styles +
// inheritance, and every one of those is knowable here. Sites read them as
// ARITHMETIC (measured: YouTube computed
// getComputedStyle(documentElement).fontSize.replace("px","") — the old
// stub returned undefined and the .replace killed the whole kevlar module,
// 0 net calls, no hydration). So the engine answers with the value a
// browser would give for everything it can know, and the spec initial
// otherwise. Deliberately bounded: simple selectors (no combinators), a
// small but honest UA table, inheritance for the inherited set.
(function () {
  // CSS initial values (the strings Chrome reports for a defaulted element).
  var INIT = {
    "font-size": "16px", "font-family": "Times New Roman", "font-weight": "400",
    "font-style": "normal", "font-variant": "normal", "font-stretch": "100%",
    "line-height": "normal", "color": "rgb(0, 0, 0)", "text-align": "start",
    "text-indent": "0px", "text-transform": "none", "text-decoration": "none",
    "letter-spacing": "normal", "word-spacing": "0px", "white-space": "normal",
    "direction": "ltr", "unicode-bidi": "normal", "visibility": "visible",
    "display": "block", "position": "static", "float": "none", "clear": "none",
    "opacity": "1", "z-index": "auto", "transform": "none",
    "transform-origin": "50% 50%", "transition": "all 0s ease 0s",
    "animation": "none 0s ease 0s 1 normal none running", "cursor": "auto",
    "overflow": "visible", "overflow-x": "visible", "overflow-y": "visible",
    "width": "auto", "height": "auto", "min-width": "auto", "min-height": "auto",
    "max-width": "none", "max-height": "none",
    "margin": "0px", "margin-top": "0px", "margin-right": "0px",
    "margin-bottom": "0px", "margin-left": "0px",
    "padding": "0px", "padding-top": "0px", "padding-right": "0px",
    "padding-bottom": "0px", "padding-left": "0px",
    "border": "0px none rgb(0, 0, 0)", "border-top-width": "0px",
    "border-right-width": "0px", "border-bottom-width": "0px",
    "border-left-width": "0px", "border-style": "none", "border-color": "rgb(0, 0, 0)",
    "background-color": "rgba(0, 0, 0, 0)", "background-image": "none",
    "background-position": "0% 0%", "background-repeat": "repeat", "background-size": "auto",
    "top": "auto", "right": "auto", "bottom": "auto", "left": "auto",
    "box-sizing": "content-box", "vertical-align": "baseline", "content": "normal",
    "list-style-type": "disc", "list-style-position": "outside",
    "border-collapse": "separate", "table-layout": "auto", "caption-side": "top",
    "flex-direction": "row", "flex-wrap": "nowrap", "justify-content": "normal",
    "align-items": "normal", "align-self": "auto", "flex-grow": "0", "flex-shrink": "1",
    "flex-basis": "auto", "order": "0", "gap": "normal", "row-gap": "normal",
    "column-gap": "normal", "grid-auto-flow": "row", "grid-template-columns": "none",
    "grid-template-rows": "none", "object-fit": "fill", "pointer-events": "auto",
    "user-select": "auto", "outline": "rgb(0, 0, 0) none 0px", "resize": "none",
    "clip": "auto", "filter": "none", "mix-blend-mode": "normal", "isolation": "auto",
    "aspect-ratio": "auto", "inset": "auto", "text-overflow": "clip",
    "word-break": "normal", "overflow-wrap": "normal", "tab-size": "8",
    "border-radius": "0px", "box-shadow": "none", "columns": "auto",
  };
  var INH = { "font-size": 1, "font-family": 1, "font-weight": 1, "font-style": 1,
    "font-variant": 1, "font-stretch": 1, "line-height": 1, "color": 1,
    "text-align": 1, "text-indent": 1, "text-transform": 1, "letter-spacing": 1,
    "word-spacing": 1, "white-space": 1, "direction": 1, "unicode-bidi": 1,
    "visibility": 1, "cursor": 1, "list-style-type": 1, "list-style-position": 1,
    "caption-side": 1, "pointer-events": 1, "user-select": 1, tabSize: 0 };
  // UA defaults that are commonly READ (Chrome 154 values, same-hour oracle).
  var UA = {
    "html": { display: "block" }, "body": { display: "block", margin: "8px" },
    "head": { display: "none" }, "script": { display: "none" }, "style": { display: "none" },
    "link": { display: "none" }, "meta": { display: "none" }, "title": { display: "none" },
    "base": { display: "none" }, "template": { display: "none" }, "noscript": { display: "none" },
    "p": { display: "block", margin: "16px 0px" }, "div": { display: "block" },
    "h1": { display: "block", "font-size": "2em", "font-weight": "700", margin: "21.44px 0px" },
    "h2": { display: "block", "font-size": "1.5em", "font-weight": "700", margin: "19.92px 0px" },
    "h3": { display: "block", "font-size": "1.17em", "font-weight": "700", margin: "18.72px 0px" },
    "h4": { display: "block", "font-weight": "700", margin: "21.28px 0px" },
    "h5": { display: "block", "font-size": "0.83em", "font-weight": "700", margin: "22.08px 0px" },
    "h6": { display: "block", "font-size": "0.67em", "font-weight": "700", margin: "24.96px 0px" },
    "ul": { display: "block", margin: "16px 0px", "padding-left": "40px" },
    "ol": { display: "block", margin: "16px 0px", "padding-left": "40px" },
    "li": { display: "list-item" }, "table": { display: "table" },
    "thead": { display: "table-header-group" }, "tbody": { display: "table-row-group" },
    "tr": { display: "table-row" }, "td": { display: "table-cell", padding: "1px" },
    "th": { display: "table-cell", padding: "1px", "font-weight": "700", "text-align": "center" },
    "a": { display: "inline", color: "rgb(0, 0, 238)", "text-decoration": "underline", cursor: "pointer" },
    "img": { display: "inline" }, "input": { display: "inline-block" },
    "button": { display: "inline-block" }, "select": { display: "inline-block" },
    "textarea": { display: "inline-block" }, "iframe": { display: "block" },
    "video": { display: "inline" }, "canvas": { display: "inline" },
    "span": { display: "inline" }, "strong": { display: "inline", "font-weight": "700" },
    "b": { display: "inline", "font-weight": "700" }, "em": { display: "inline", "font-style": "italic" },
    "i": { display: "inline", "font-style": "italic" }, "br": { display: "inline" },
    "hr": { display: "block", margin: "8px auto" }, "pre": { display: "block", "font-family": "monospace", "white-space": "pre" },
    "code": { display: "inline", "font-family": "monospace" },
    "label": { display: "inline", cursor: "default" }, "form": { display: "block" },
    "nav": { display: "block" }, "main": { display: "block" }, "section": { display: "block" },
    "header": { display: "block" }, "footer": { display: "block" }, "article": { display: "block" },
    "aside": { display: "block" }, "figure": { display: "block", margin: "16px 0px" },
    "svg": { display: "inline" }, "path": { display: "inline" },
  };
  var CAMEL = {}, DASH = {};
  for (var pk in INIT) {
    var cp = pk.replace(/-([a-z])/g, function (m, c) { return c.toUpperCase(); });
    if (pk === "float") cp = "cssFloat";
    CAMEL[pk] = cp; DASH[cp] = pk;
  }
  function parseDecls(text, out) {
    if (!text) return out;
    var parts = String(text).split(";");
    for (var i = 0; i < parts.length; i++) {
      var d = parts[i], c = d.indexOf(":");
      if (c < 0) continue;
      var k = d.slice(0, c).trim().toLowerCase(), v = d.slice(c + 1).trim();
      if (!k || !v) continue;
      if (k.indexOf("--") === 0) continue;
      out[k] = v.replace(/\s*!important\s*$/i, "");
    }
    return out;
  }
  // Simple-selector matcher: tag, #id, .class, [attr], * — combinators and
  // pseudo-classes are skipped (a value we cannot honestly place is not
  // invented). The universal `:root` and `html` are honored.
  function selMatches(sel, el) {
    sel = sel.trim();
    if (!sel) return -1;
    if (sel === ":root") sel = "html";
    else if (sel.indexOf(":") >= 0 && sel.replace(/:root|::?[a-z-]+(\([^)]*\))?/g, "").trim() !== sel.replace(/:root/g, "html").trim() && /[:>+~,\s]/.test(sel)) return -1;
    var m = /^([a-zA-Z][\w-]*)?((?:[.#\[][^.#\[]*\]*|\.-?[\w-]+|#[\w-]+)*)$/.exec(sel);
    if (!m || sel.indexOf(" ") >= 0 || /[>+~]/.test(sel)) return -1;
    var spec = 0;
    var tag = m[1];
    if (!tag && !m[2]) return -1;
    if (tag) {
      var et = String((__mr(el, "attrs") && __mr(el, "tag")) || __mr(el, "tag") || "").toLowerCase();
      if (et !== tag.toLowerCase()) return -1;
      spec += 1;
    }
    var rest = m[2] || "", cls = rest.match(/\.-?[\w-]+/g) || [], ids = rest.match(/#[\w-]+/g) || [], ats = rest.match(/\[[^\]]*\]/g) || [];
    var attrs = __mr(el, "attrs") || {};
    for (var i = 0; i < ids.length; i++) {
      if (String(attrs.id || "") !== ids[i].slice(1)) return -1;
      spec += 100;
    }
    var cl = String(attrs.class || "").split(/\s+/);
    for (var j = 0; j < cls.length; j++) {
      var want = cls[j].slice(1);
      if (cl.indexOf(want) < 0) return -1;
      spec += 10;
    }
    for (var a = 0; a < ats.length; a++) {
      var mm = /^\[\s*([\w-]+)(?:\s*([~|^$*]?=)\s*"?([^"\]]*)"?)?\s*\]$/.exec(ats[a]);
      if (!mm) return -1;
      var av = attrs[mm[1]];
      if (av === undefined) return -1;
      if (mm[2]) {
        av = String(av);
        if (mm[2] === "=" && av !== mm[3]) return -1;
        if (mm[2] === "^=" && av.indexOf(mm[3]) !== 0) return -1;
        if (mm[2] === "$=" && av.slice(-mm[3].length) !== mm[3]) return -1;
        if (mm[2] === "*=" && av.indexOf(mm[3]) < 0) return -1;
      }
      spec += 10;
    }
    return spec;
  }
  var __sheetCache = { at: 0, rules: null };
  function sheetRules() {
    var now = Date.now();
    if (__sheetCache.rules && now - __sheetCache.at < 250) return __sheetCache.rules;
    var rules = [];
    try {
      var stys = document.getElementsByTagName("style");
      for (var i = 0; i < stys.length; i++) {
        var txt = "";
        try { txt = stys[i].textContent || ""; } catch (e) { txt = ""; }
        txt = String(txt).replace(/\/\*[\s\S]*?\*\//g, "");
        var re = /([^{}]+)\{([^{}]*)\}/g, mm;
        while ((mm = re.exec(txt))) {
          var sels = mm[1].split(",");
          var decls = parseDecls(mm[2], {});
          for (var s = 0; s < sels.length; s++) rules.push({ sel: sels[s], decls: decls });
        }
      }
    } catch (e) {}
    __sheetCache.at = now; __sheetCache.rules = rules;
    return rules;
  }
  function ownDecls(el) {
    var out = {};
    try {
      var st = __mr(el, "__style");
      if (st) {
        if (st.__props) parseDecls(st.__props, out);
        for (var k in st) if (INIT[k] !== undefined || DASH[k] !== undefined) out[DASH[k] || k] = String(st[k]);
        for (var pk2 in INIT) {
          var cv = __g(st, "_sf_" + CAMEL[pk2]);
          if (cv !== undefined) out[pk2] = String(cv);
        }
      }
    } catch (e) {}
    try { parseDecls((__mr(el, "attrs") || {}).style, out); } catch (e) {}
    return out;
  }
  function emPx(v, base) {
    var s = String(v);
    if (/^-?[\d.]+px$/.test(s)) return s;
    if (/^-?[\d.]+(\.[\d]+)?em$/.test(s)) return (parseFloat(s) * base) + "px";
    if (/^-?[\d.]+%$/.test(s)) return (parseFloat(s) / 100 * base) + "px";
    if (/^-?[\d.]+pt$/.test(s)) return (parseFloat(s) * 4 / 3) + "px";
    if (/^-?[\d.]+rem$/.test(s)) return (parseFloat(s) * 16) + "px";
    return s;
  }
  function resolved(el, prop) {
    // cascade winner: UA (score 0) < author rules (specificity + order) < inline
    var best, bestScore = -1;
    try {
      var at = __mr(el, "attrs") || {};
      var tag = String(at.tag || "").toLowerCase();
      var ua = UA[tag];
      if (ua && ua[prop] !== undefined) { best = ua[prop]; bestScore = 0; }
      var rules = sheetRules();
      for (var i = 0; i < rules.length; i++) {
        if (!(prop in rules[i].decls)) continue;
        var sp = selMatches(rules[i].sel, el);
        if (sp < 0) continue;
        var score = sp * 100000 + i;
        if (score >= bestScore) { best = rules[i].decls[prop]; bestScore = score + 1; }
      }
      var own = ownDecls(el);
      if (prop in own) best = own[prop];
    } catch (e) {}
    return best;
  }
  function fontPx(el, depth) {
    if (!el || depth > 30) return 16;
    var v = resolved(el, "font-size");
    if (v === undefined) return fontPx(parentOf(el), depth + 1);
    var s = String(v);
    if (/^-?[\d.]+px$/.test(s)) return parseFloat(s) || 16;
    if (/^-?[\d.]+rem$/.test(s)) return parseFloat(s) * 16;
    if (/^-?[\d.]+em$/.test(s)) return fontPx(parentOf(el), depth + 1) * parseFloat(s);
    if (/^-?[\d.]+%$/.test(s)) return fontPx(parentOf(el), depth + 1) * parseFloat(s) / 100;
    if (/^-?[\d.]+pt$/.test(s)) return parseFloat(s) * 4 / 3;
    return parseFloat(s) || 16;
  }
  function parentOf(el) {
    try { return __mr(el, "__parent") || null; } catch (e) { return null; }
  }
  function inherited(el, prop, depth) {
    if (!el || depth > 40) return INIT[prop];
    var v = resolved(el, prop);
    if (v !== undefined) return v;
    return inherited(parentOf(el), prop, depth + 1);
  }
  var __gcsCache = (typeof WeakMap === "function") ? new WeakMap() : null;
  function cacheFor(el) {
    if (!__gcsCache || !el || typeof el !== "object") return null;
    var now = Date.now(), c = __gcsCache.get(el);
    if (c && now - c.t < 200) return c;
    c = { t: now, v: {} };
    try { __gcsCache.set(el, c); } catch (e) { return null; }
    return c;
  }
  function computedValue(el, prop) {
    var c = cacheFor(el);
    if (c && c.v[prop] !== undefined) return c.v[prop];
    var v;
    if (INH[prop] !== undefined) v = inherited(el, prop, 0);
    else v = resolved(el, prop);
    if (v === undefined) v = INIT[prop];
    if (v === undefined) return "";
    if (prop === "font-size") v = emPx(v, fontPx(parentOf(el), 0));
    else if (/(width|height|margin|padding|top|right|bottom|left|gap|spacing|indent)/.test(prop)) v = emPx(v, fontPx(el, 0));
    var out = String(v);
    if (c) c.v[prop] = out;
    return out;
  }
  window.getComputedStyle = function (el) {
    var st = {
      // the live CF api.js reads the transform/position/display matrix off
      // this object while assembling extraParams; a missing field used to
      // kill its requestExtraParams reply (reading .length of undefined).
      getPropertyValue: function (p) {
        var k = String(p == null ? "" : p);
        if (st[k] !== undefined) return String(st[k]);
        return "";
      },
    };
    st.propertyValue = st.getPropertyValue;
    for (var pk in INIT) {
      var cp = CAMEL[pk];
      var val = computedValue(el, pk);
      try {
        Object.defineProperty(st, cp, { get: __native("get " + cp, 0, (function (P) { return function () { return computedValue(el, P); }; })(pk)), enumerable: true, configurable: true });
        if (cp !== pk) Object.defineProperty(st, pk, { get: __native("get " + pk, 0, (function (P) { return function () { return computedValue(el, P); }; })(pk)), enumerable: false, configurable: true });
      } catch (e) { if (val !== undefined) st[cp] = val; }
    }
    try { Object.defineProperty(st, "_s7b", { value: "CSSStyleDeclaration", configurable: true }); } catch (e) {}
    return st;
  };
})();
window.scrollTo = function () {}; window.scrollBy = function () {};
window.alert = function () {}; window.confirm = function () { return true; }; window.prompt = function () { return null; };
window.open = function () { return null; }; window.close = function () {}; window.focus = function () {}; window.blur = function () {};

// ── postMessage: cross-frame messaging (the turnstile/chl widget path) ──
// A real browser queues the message as a task on the receiving window and
// stamps it with the SENDER's origin. Since iframe scripts execute in this
// realm, the sender is inferred: a specific targetOrigin means a
// cross-frame send (the widget's frame is the sender); '*' means a
// same-window post.
function __winOrigin(u) {
  var m = String(u).match(/^https?:\/\/[^\/?#]+/);
  return m ? m[0] : "null";
}
// window.postMessage argument shapes: legacy (message, targetOrigin, transfer)
// and modern (message, {transfer, targetOrigin}); the observed Chrome oracle
// says postMessage(msg, [array]) delivers with NO ports (the array converts as
// an empty options dict), so only the two explicit transfer shapes collect
// ports. Returns {to, ports} for the deliver call.
function __pmArgs(targetOrigin, transfer) {
  var ports = [];
  try {
    if (targetOrigin && typeof targetOrigin === "object") {
      var isArr = Object.prototype.toString.call(targetOrigin) === "[object Array]";
      if (isArr) {
        transfer = null; targetOrigin = "*";
      } else if (targetOrigin.transfer !== undefined || targetOrigin.targetOrigin !== undefined) {
        transfer = targetOrigin.transfer !== undefined ? targetOrigin.transfer : null;
        targetOrigin = targetOrigin.targetOrigin == null ? "*" : targetOrigin.targetOrigin;
      } else {
        transfer = null; targetOrigin = "*";
      }
    }
    if (transfer && typeof __collectPorts === "function") ports = __collectPorts(transfer);
  } catch (e) {}
  return { to: String(targetOrigin == null ? "*" : targetOrigin), ports: ports };
}
__lastXFrame = null; // {origin, win} of the last cross-origin iframe
var __pmAdvN = 0; // challenge ADVANCE deliveries (heartbeat excluded) — stall watchdog input
__DCI.pmAdv = function () { return __pmAdvN; };
function __pmj(kind, side, tO, data) {
  if (!__TRACE) return;
  try {
    var J = globalThis.__PMJ = globalThis.__PMJ || [];
    var d = data && typeof data === "object" ? data : {};
    if (String(d.event) === "fail" || String(d.event) === "overrunBegin") { try { J.push("STACK for " + String(d.event) + ": " + String(new Error().stack).split(String.fromCharCode(10)).slice(1, 9).join(" ||| ").slice(0, 700)); } catch (eS) {} }
    J.push("[" + kind + "] " + side + " tO=" + String(tO) + " ev=" + String(d.event) + " src=" + String(d.source) + " mode=" + String(d.mode) + " seq=" + String(d.seq) + " code=" + String(d.code) + " rcV=" + String(d.rcV) + " frMd=" + String(d.frMd) + " cOut=" + String(d.cfChlOut).slice(0, 60) + " cOutS=" + String(d.cfChlOutS).slice(0, 40) + " keys=" + Object.keys(d).join("+") + " data=" + (d.data !== undefined ? String(d.data).slice(0, 120) : "-"));
    if (J.length > 300) J.shift();
    if (typeof __dumpFile === "function") __dumpFile("pmj", J.join(String.fromCharCode(10)));
    // ST-6: full-payload companion (TRACE only) — the summary line drops the
    // field VALUES (rcV/cData/nextRcV); this keeps raw JSON, capped, for the
    // engine-vs-Chrome exchange diff.
    try {
      var F = globalThis.__PMJF = globalThis.__PMJF || [];
      var fj = "-";
      try { fj = JSON.stringify(data); } catch (eJ) { fj = String(data); }
      F.push("[" + kind + "] " + side + " tO=" + String(tO) + " @" + Date.now() + " " + String(fj).slice(0, 1600));
      if (F.length > 80) F.shift();
      if (typeof __dumpFile === "function") __dumpFile("pmjf", F.join(String.fromCharCode(10)));
    } catch (eF) {}
  } catch (e) {}
}
function __deliverMessage(win, ev) {
  __pmj("DELIVER", (win === window ? "main" : "shell"), "-", ev && ev.data);
  try { var __evn = (ev && ev.data && typeof ev.data === "object") ? String(ev.data.event || "") : ""; if (__evn !== "meow" && __evn !== "food" && __evn !== "fail" && __evn !== "overrunBegin") __pmAdvN++; } catch (eA) {}
  if (__TRACE) { try { __log("PM-DELIVER to=" + (win === window ? "main" : "shell") + " dt=" + JSON.stringify(ev && ev.data).slice(0, 220) + " @" + Date.now()); } catch (e) {} }
  if (__TRACE) { try { var __MS = globalThis.__PMSTORE = globalThis.__PMSTORE || []; __MS.push((win === window ? "main" : "shell") + " @" + Date.now() + " msgL=" + ((__g(win, "__l") && __g(win, "__l")["message"] && __g(win, "__l")["message"].length) || 0) + " lkeys=" + ((__g(win, "__l") && Object.keys(__g(win, "__l")).join("+")) || "-")); if (__MS.length > 200) __MS.shift(); if (typeof __dumpFile === "function") __dumpFile("pmstore", __MS.join(String.fromCharCode(10))); } catch (e) {} }
  setTimeout(function () {
    __evSet(ev, "type", "message");
    __evSet(ev, "target", win);
    __evSet(ev, "currentTarget", win);
    __evSet(ev, "isTrusted", true); // real MessageEvents carry isTrusted; api.js gates on it (Eu = isTrusted && ...)
    try {
      if (typeof win.onmessage === "function") {
        try { win.onmessage(ev); } catch (e) { try { __fireError(e, win); } catch (_e) {} }
      }
      if (win.dispatchEvent) {
        try {
          win.dispatchEvent(ev);
        } catch (e) {
          if (__TRACE) { try { var __MS2 = globalThis.__PMSTORE = globalThis.__PMSTORE || []; __MS2.push("DISP-THREW to=" + (win === window ? "main" : "shell") + " " + String(e && e.message).slice(0, 200)); if (typeof __dumpFile === "function") __dumpFile("pmstore", __MS2.join(String.fromCharCode(10))); } catch (e2) {} }
          try { __fireError(e, win); } catch (_e) {}
        }
      }
    } catch (e) { __log("message dispatch: " + e); }
  }, 0);
}
window.postMessage = function (data, targetOrigin, transfer) {
  var __pa = __pmArgs(targetOrigin, transfer);
  targetOrigin = __pa.to;
  __pmj("OUT", "mainfn", targetOrigin, data);
  if (__TRACE) { try { __log("PM-MAIN out tO=" + String(targetOrigin) + " dt=" + JSON.stringify(data).slice(0, 220) + " mainLkeys=" + ((__g(window, "__l") && Object.keys(__g(window, "__l")).join("+")) || "-")); } catch (e) {} }
  var pageOrigin = __winOrigin(PAGE_URL);
  if (targetOrigin !== "*" && targetOrigin !== pageOrigin) return; // browser drops it
  var frame = targetOrigin !== "*" ? __lastXFrame : null;
  __deliverMessage(window, {
    data: data,
    origin: frame ? frame.origin : pageOrigin,
    source: frame ? frame.win : window,
    lastEventId: "",
    ports: __pa.ports,
  });
};
window.print = function () {};

window.CSS = { supports: function () { return true; }, escape: function (s) { return String(s).replace(/[^a-zA-Z0-9_-]/g, function (c) { return "\\" + c; }); } };

// storage events / lifecycle
window.requestIdleCallback = function (fn) { return setTimeout(function () { fn({ didTimeout: false, timeRemaining: function () { return 50; } }); }, 1); };
window.cancelIdleCallback = __native("cancelIdleCallback", 1, function (id) { clearTimeout(id); });

// ───────────────────────────── network ──────────────────────────────
function Headers(init) {
  if (new.target === undefined) throw new TypeError("Failed to construct 'Headers': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  this.__h = {};
  var self = this;
  if (init && typeof init === "object") {
    Object.keys(init).forEach(function (k) { self.__h[k.toLowerCase()] = String(init[k]); });
  }
  this.append = function (k, v) { this.__h[String(k).toLowerCase()] = String(v); };
  this.set = this.append;
  this.get = function (k) { var v = this.__h[String(k).toLowerCase()]; return v === undefined ? null : v; };
  this.has = function (k) { return this.__h[String(k).toLowerCase()] !== undefined; };
  this.delete = function (k) { delete this.__h[String(k).toLowerCase()]; };
  this.forEach = function (cb) { Object.keys(this.__h).forEach(function (k) { cb(self.__h[k], k, self); }); };
}
function bytesToText(b) {
  // naive latin1→utf8-ish for v0.1 (real decoding is a Phase-1 item)
  return b;
}
(function () {
  // ST-11 U2b-1: Chrome renders `function Image() { [native code] }` — the
  // old anonymous expression rendered `function () { [native code] }`. The
  // wrapper keeps behavior + the shared img prototype (Chrome:
  // Image.prototype === HTMLImageElement.prototype), length 0.
  var __imgOrig = function (w, h) {
    var el = makeElement("img");
    if (w !== undefined) __mr(el, "attrs").width = String(w);
    if (h !== undefined) __mr(el, "attrs").height = String(h);
    return el;
  };
  var IM = function Image() { return __imgOrig.apply(null, arguments); };
  try { Object.defineProperty(IM, "prototype", { value: tagCtor("img").prototype, writable: false, enumerable: false, configurable: false }); } catch (e) {}
  if (__clearSrc) { try { __clearSrc(IM); } catch (e) {} }
  window.Image = IM;
})();

function doFetch(input, init) {
  init = init || {};
  var url = typeof input === "string" ? input : input && input.url;
  var method = String((init.method || (input && input.method) || "GET")).toUpperCase();
  var hdrs = new Headers(init.headers || (input && input.headers));
  var extra = [];
  hdrs.forEach(function (v, k) { extra.push([k, v]); });
  var body = init.body !== undefined && init.body !== null ? String(init.body) : "";
  var abs = resolveUrl(PAGE_URL, String(url));
  // ST-8: measured defect — fetch(createObjectURL(blob)) went to the WIRE and
  // returned the origin server's 404 page (460b) where Chrome returns the
  // blob's own bytes (34b). A blob: URL is this realm's own store; test the
  // RAW input, because resolveUrl would rewrite the scheme away.
  if (String(url).indexOf("blob:") === 0) return __blobFetch(String(url), method);
  var r = __wireCall(method, abs, "xhr", JSON.stringify(extra), body);
  var j;
  try { j = JSON.parse(r); } catch (e) { j = { ok: false, error: "bridge parse" }; }
  if (!j.ok) {
    if (__TRACE) { try { __log("FETCH " + method + " " + abs + " -> ERR " + (j.error || "")); } catch (e) {} }
    return Promise.reject(new TypeError("Failed to fetch"));
  }
  if (__TRACE) { try { __log("FETCH " + method + " " + abs + " -> " + j.status + " (" + (j.body ? j.body.length : 0) + "b)"); } catch (e) {} }
  try{ if(typeof __EBBS==='function'&&(globalThis.__fb=(globalThis.__fb||0)+1)<=60) __EBBS('FTCH-'+method+' '+String(abs).slice(0,70)+'@'+j.status); }catch(_x){}
  var respHeaders = new Headers();
  (j.headers || []).forEach(function (pair) { if (String(pair[0]).toLowerCase() !== "set-cookie") respHeaders.append(pair[0], pair[1]); });
  var resp = {
    ok: j.status >= 200 && j.status < 300,
    status: j.status, statusText: "",
    url: j.url, redirected: j.url !== abs, type: "basic",
    headers: respHeaders, bodyUsed: false,
    text: function () { return Promise.resolve(bytesToText(j.body)); },
    json: function () { return Promise.resolve(JSON.parse(j.body)); },
    arrayBuffer: function () { var s = j.body; var b = new ArrayBuffer(s.length); var v = new Uint8Array(b); for (var i = 0; i < s.length; i++) v[i] = s.charCodeAt(i) & 255; return Promise.resolve(b); },
    clone: function () { return resp; },
  };
  return Promise.resolve(resp);
}
window.fetch = function (input, init) {
  try { return doFetch(input, init); } catch (e) { return Promise.reject(e); }
};
window.Request = function Request(u, i) {
  if (new.target === undefined) throw new TypeError("Failed to construct 'Request': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  if (arguments.length < 1) throw new TypeError("Failed to construct 'Request': 1 argument required, but only 0 present.");
  __hide(this, "url", String(u)); __hide(this, "method", (i && i.method) || "GET");
};
window.Response = { json: function (v) { return { json: function () { return Promise.resolve(v); } }; } };

function XMLHttpRequest() {
  __hide(this, "_xr_readyState", 0); __hide(this, "_xr_status", 0); __hide(this, "_xr_statusText", ""); __hide(this, "_xr_responseText", ""); __hide(this, "_xr_response", "");
  this.timeout = 0; this.withCredentials = false;
  this.onreadystatechange = null; this.onload = null; this.onerror = null; this.onloadend = null;
  __hide(this, "_xhdrs", []); __hide(this, "_xmethod", "GET"); __hide(this, "_xurl", ""); __hide(this, "_xsent", false); __hide(this, "_xaborted", false);
}
// ── frame-scoped wire clients: a cross-origin frame resolves relative URLs
// against its OWN document, not the host page (Chrome does; the CF widget
// shell posts its submissions from its frame origin — the live 400'd
// /fo/ ray-2 went to 4chan.org instead of challenges.cloudflare.com). Bound
// per frame in __iframeLoad as win.XMLHttpRequest / win.fetch; the script
// wrapper passes them through (win.XMLHttpRequest || XMLHttpRequest).
function __mkFrameXHR(base) {
  // NB: inside the named function expression `XMLHttpRequest` refers to the
  // NFE itself — capture the realm's constructor first (otherwise infinite
  // recursion: new XMLHttpRequest() calls the NFE).
  var Orig = XMLHttpRequest;
  var f = function XMLHttpRequest() {
    var x = new Orig();
    try { __hide(x, "_xbase", base); } catch (e) {}
    return x;
  };
  try { f.prototype = Orig.prototype; } catch (e) {}
  return f;
}
function __mkFrameFetch(base) {
  var main = fetch;
  return function fetch(u, init) {
    try { if (typeof u === "string") u = resolveUrl(base, u); } catch (e) {}
    return main(u, init);
  };
}
// Frame-scoped URL: a frame's URL.createObjectURL must mint blob URLs with
// the FRAME's origin (Chrome: blob:<creator-realm origin>/<uuid>; measured
// local, 2026-10-18: frame 8351 → Chrome blob:http://127.0.0.1:8351/…, engine
// minted the main page's 8333; live: DataDome's devicecheck worker minted
// blob:https://www.g2.com/… where Chrome mints blob:https://geo.captcha-
// delivery.com/…). Bound per frame as win.URL like the wire clients; the
// veil resolves it frame-first (win own prop, not a masked param name).
function __mkFrameURL(base) {
  var org = __winOrigin(base);
  var F = function (u, b) { return new window.URL(u, b); };
  try { F.prototype = window.URL.prototype; } catch (e) {}
  F.createObjectURL = function (__b) {
    try {
      var __u = "blob:" + org + "/" + __blobUuid();
      var __n = __couStore(__u, __b);
      if (typeof __TRACE !== "undefined" && __TRACE) __log("COU-FRAME -> " + __u + " bytes=" + __n);
      return __u;
    } catch (e) { return "blob:" + org + "/" + __blobUuid(); }
  };
  F.revokeObjectURL = function () {};
  return F;
}
XMLHttpRequest.prototype.open = function (m, u) { __hide(this, "_xmethod", String(m).toUpperCase()); var __u0 = String(u); __hide(this, "_xurl", __u0.indexOf("blob:") === 0 ? __u0 : resolveUrl(__g(this, "_xbase") || PAGE_URL, __u0)); __hide(this, "_xsent", false); __hide(this, "_xaborted", false); __hide(this, "_xr_readyState", 1); };
XMLHttpRequest.prototype.setRequestHeader = function (k, v) { var H = __g(this, "_xhdrs"); if (!H) { H = []; __hide(this, "_xhdrs", H); } H.push([String(k), String(v)]); };
XMLHttpRequest.prototype.getResponseHeader = function (k) { var RH = __g(this, "_xrh"); var v = RH && RH[String(k).toLowerCase()]; return v === undefined ? null : v; };
XMLHttpRequest.prototype.getAllResponseHeaders = function () {
  var out = ""; var RH = __g(this, "_xrh") || {};
  Object.keys(RH).forEach(function (k) { out += k + ": " + RH[k] + "\r\n"; });
  return out;
};
XMLHttpRequest.prototype.abort = function () {
  var self = this;
  var rs = this.readyState;
  if (rs === 0) return;
  if (rs === 4) { __hide(this, "_xr_readyState", 0); __hide(this, "_xr_status", 0); __hide(this, "_xr_statusText", ""); __hide(this, "_xr_responseText", ""); __hide(this, "_xr_response", ""); return; }
  if (__g(this, "_xsent", false)) {
    __hide(this, "_xaborted", true);
    __hide(this, "_xr_readyState", 0); __hide(this, "_xr_status", 0);
    setTimeout(function () {
      if (self.onabort) { try { self.onabort(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
      self.dispatchEvent({ type: "abort" });
      if (self.onloadend) { try { self.onloadend(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
      self.dispatchEvent({ type: "loadend" });
    }, 0);
  }
};
XMLHttpRequest.prototype.send = function (body) {
  var self = this;
  __hide(this, "_xsent", true);
  var __xurl = String(__g(this, "_xurl") || "");
  if (__xurl.indexOf("blob:") === 0) {
    var __bbin = (__DCI.t.couBin || {})[__xurl];
    setTimeout(function () {
      if (__g(self, "_xaborted")) return;
      if (!__bbin) {
        __hide(self, "_xr_readyState", 4); __hide(self, "_xr_status", 0);
        if (self.onerror) { try { self.onerror(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
        self.dispatchEvent({ type: "error" });
        return;
      }
      var __xrT = __utf8Dec(__bbin); __hide(self, "_xr_status", 200); __hide(self, "_xr_responseText", __xrT); __hide(self, "_xr_response", __xrT);
      var __rh = {}; var __bty = (__DCI.t.couTy || {})[__xurl]; if (__bty) __rh["content-type"] = __bty;
      __hide(self, "_xrh", __rh);
      __hide(self, "_xr_readyState", 4);
      if (self.onreadystatechange) { try { self.onreadystatechange(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
      if (self.onload) { try { self.onload(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
      self.dispatchEvent({ type: "readystatechange" });
      self.dispatchEvent({ type: "load" });
      self.dispatchEvent({ type: "loadend" });
    }, 0);
    return;
  }
  if (__TRACE) { try { if (__xurl.indexOf("/eb/") >= 0 && typeof __dumpFile === "function") __dumpFile("ebbody", String(body)); } catch (eEBB) {} }
  var r = __wireCall(__g(this, "_xmethod"), __xurl, "xhr", JSON.stringify(__g(this, "_xhdrs") || []), body === undefined || body === null ? "" : String(body));
  try{ if(typeof __EBBS==='function'&&(globalThis.__xb=(globalThis.__xb||0)+1)<=60) __EBBS('XHRS-'+__g(this,"_xmethod")+' '+__xurl.slice(0,70)); }catch(_x){}
  var j;
  try { j = JSON.parse(r); } catch (e) { j = { ok: false }; }
  if (__TRACE) { try { __log("XHR " + __g(this, "_xmethod") + " " + __xurl + " -> " + (j.ok ? j.status : "ERR")); } catch (e) {} }
  setTimeout(function () {
    if (__g(self, "_xaborted")) return;
    if (!j.ok) {
      __hide(self, "_xr_readyState", 4); __hide(self, "_xr_status", 0);
      if (self.onerror) { try { self.onerror(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
      self.dispatchEvent({ type: "error" });
      return;
    }
    __hide(self, "_xr_status", j.status);
    try{ if(typeof __EBBS==='function'&&(globalThis.__xbd=(globalThis.__xbd||0)+1)<=60) __EBBS('XHRD-'+self.status+' '+__xurl.slice(0,60)); }catch(_x){}
    __hide(self, "_xr_responseText", j.body); __hide(self, "_xr_response", j.body);
    var __rh = {};
    (j.headers || []).forEach(function (p) { __rh[String(p[0]).toLowerCase()] = p[1]; });
    __hide(self, "_xrh", __rh);
    __hide(self, "_xr_readyState", 4);
    if (self.onreadystatechange) { try { self.onreadystatechange(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
    if (self.onload) { try { self.onload(); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} } }
    self.dispatchEvent({ type: "readystatechange" });
    self.dispatchEvent({ type: "load" });
    self.dispatchEvent({ type: "loadend" });
  }, 0);
};

function WebSocket(url) {
  if (new.target === undefined) throw new TypeError("Failed to construct 'WebSocket': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  if (arguments.length < 1) throw new TypeError("Failed to construct 'WebSocket': 1 argument required, but only 0 present.");
  __hide(this, "url", String(url)); __hide(this, "readyState", 0); this.onopen = null; this.onmessage = null; this.onerror = null; this.onclose = null;
}
WebSocket.prototype.send = function () {};
WebSocket.prototype.close = function () {};
WebSocket.CONNECTING = 0; WebSocket.OPEN = 1; WebSocket.CLOSING = 2; WebSocket.CLOSED = 3;

// ─────────────── Worker: real blob-sandbox worker + message bridge ───────────────
// CF's widget creates a blob worker as an eval sandbox: the bootstrap registers
// onmessage, gates EVERY delivery on isTrusted && origin==='' && source===null,
// then eval()s the posted program inside the worker scope. Programs use
// self.navigator / fetch / performance / OPFS / postMessage and the widget's
// pipeline advances on the replies. One QuickJS thread: the worker scope is a
// `with`-object over a fresh scope (self/onmessage/postMessage/...), delivery is
// async through the timer queue, and fetch/XHR ride the engine's wire like
// every other node.
function Worker(url) {
  if (new.target === undefined) throw new TypeError("Failed to construct 'Worker': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  if (arguments.length < 1) throw new TypeError("Failed to construct 'Worker': 1 argument required, but only 0 present.");
  this.url = String(url);
  this.onmessage = null; this.onerror = null;
  this.__listeners = []; this.__q = []; this.__dead = false; this.__started = false; this.__scope = null;
  var __org = "null";
  try {
    // The script URL carries the realm: a blob: URL embeds its minting origin
    // (Chrome: the worker's location.origin is the blob's origin).
    // Frame-created workers previously read the MAIN realm's origin — the
    // DataDome devicecheck worker (minted in the geo.captcha-delivery.com
    // frame) reported www.g2.com (measured 2026-10-18, local blobprobe:
    // worker location.origin 8333 vs Chrome's 8351).
    var __us = String(url);
    var __bm = __us.match(/^blob:([a-zA-Z][\w+.-]*:\/\/[^\/?#]+)/);
    if (__bm) __org = __bm[1];
    else __org = (__g(globalThis, "__frameOrigin") && String(__g(globalThis, "__frameOrigin")).indexOf("http") === 0) ? String(__g(globalThis, "__frameOrigin")) : ((window.location && window.location.origin) || "null");
  } catch (e0) {}
  this.__origin = __org;
  try{ if(typeof __EBBS==='function'&&(globalThis.__wkb=(globalThis.__wkb||0)+1)<=40) __EBBS('WK-'+String(url).slice(0,60)); }catch(e){}
  try { if (typeof __TRACE !== "undefined" && __TRACE) { __log("WK-NEW url=" + String(url).slice(0,90)); var __bsrc = __DCI.t.couBlobs && __DCI.t.couBlobs[String(url)]; if (__bsrc != null) { if (__dumpFile) __dumpFile("wk-src-" + ((globalThis.__WKN = (globalThis.__WKN || 0) + 1)), String(__bsrc)); __log("WK-SRC len=" + String(__bsrc).length + " head=" + JSON.stringify(String(__bsrc).slice(0, 200))); } } } catch (e) {}
  var __wself = this;
  setTimeout(function () { __startWorker(__wself); }, 0);
}
function __cloneMsg(d) {
  try { if (d === null || typeof d !== "object") return d; return JSON.parse(JSON.stringify(d)); } catch (e) { return d; }
}
function __mkWorkerTT() {
  var __pols = {};
  return {
    createPolicy: function (name, rules) {
      var n = String(name);
      if (__pols[n]) return __pols[n];
      rules = rules || {};
      var pol = {
        name: n,
        createHTML: function (s) { s = String(s == null ? "" : s); return rules.createHTML ? rules.createHTML(s) : s; },
        createScript: function (s) { s = String(s == null ? "" : s); return rules.createScript ? rules.createScript(s) : s; },
        createScriptURL: function (s) { s = String(s == null ? "" : s); return rules.createScriptURL ? rules.createScriptURL(s) : s; },
      };
      __pols[n] = pol;
      try { if (typeof __TRACE !== "undefined" && __TRACE) __log("WK-TT-POLICY " + n); } catch (e) {}
      return pol;
    },
    getPolicyNames: function () { var o = []; for (var k in __pols) if (Object.prototype.hasOwnProperty.call(__pols, k)) o.push(k); return o; },
    isHTML: function () { return false; }, isScript: function () { return false; }, isScriptURL: function () { return false; },
    emptyHTML: "", emptyScript: "",
  };
}
function __opfsStore() { __DCI.t.opfs = __DCI.t.opfs || {}; return __DCI.t.opfs; }
function __opfsFile(name) {
  var f = { kind: "file", name: String(name) };
  f.getFile = function () {
    var txt = __opfsStore()[f.name] || "";
    return Promise.resolve({ name: f.name, size: txt.length, type: "", lastModified: Date.now(), text: function () { return Promise.resolve(txt); }, arrayBuffer: function () { return Promise.resolve(new ArrayBuffer(0)); } });
  };
  f.createWritable = function () {
    var buf = "";
    return Promise.resolve({
      write: function (data) { try { buf += String(data); } catch (e) {} return Promise.resolve(); },
      truncate: function () { buf = ""; return Promise.resolve(); },
      seek: function () { return Promise.resolve(); },
      close: function () { __opfsStore()[f.name] = buf; return Promise.resolve(); },
    });
  };
  f.createSyncAccessHandle = function () { return Promise.reject(new Error("NoModificationAllowedError")); };
  return f;
}
function __opfsDir(name) {
  var d = { kind: "directory", name: String(name || "") };
  d.getFileHandle = function (n) { return Promise.resolve(__opfsFile(String(n))); };
  d.getDirectoryHandle = function (n) { return Promise.resolve(__opfsDir(String(n))); };
  d.removeEntry = function (n) { try { delete __opfsStore()[String(n)]; } catch (e) {} return Promise.resolve(); };
  d.resolve = function () { return Promise.resolve(null); };
  return d;
}
// ── worker realm classes: Chrome's DedicatedWorkerGlobalScope chain ────
// Workers identify BY SHAPE in the wild (creepjs: `IS_WORKER_SCOPE =
// !self.document && self.WorkerGlobalScope`), and lie instruments search
// WorkerNavigator.prototype like any other interface. So the worker scope
// is a real DedicatedWorkerGlobalScope (chain → WorkerGlobalScope →
// EventTarget) and navigator/location are WorkerNavigator/WorkerLocation
// instances with native-shaped, brand-checked accessors on the prototype —
// no own data props. Values stay the machine's real values (differential
// oracle), same as every other surface. (2026-10-18, worker-realm leak
// unit; measured diff vs Chrome: /var/tmp/st-proto/wkfid2-*.)
var __WKC = null;
function __wkWorkerClasses() {
  if (__WKC) return __WKC;
  function mkC(name, proto, tag) {
    var C = function () { throw new TypeError("Illegal constructor"); };
    try { Object.defineProperty(C, "name", { value: name, configurable: true }); } catch (e) {}
    C.prototype = Object.create(proto);
    try { Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C.prototype, Symbol.toStringTag, { value: tag, configurable: true }); } catch (e) {}
    return C;
  }
  var ETp = (typeof globalThis.EventTarget !== "undefined" && globalThis.EventTarget && globalThis.EventTarget.prototype) ? globalThis.EventTarget.prototype : Object.prototype;
  var WGS = mkC("WorkerGlobalScope", ETp, "WorkerGlobalScope");
  var DWGS = mkC("DedicatedWorkerGlobalScope", WGS.prototype, "DedicatedWorkerGlobalScope");
  var WN = mkC("WorkerNavigator", Object.prototype, "WorkerNavigator");
  var WL = mkC("WorkerLocation", Object.prototype, "WorkerLocation");
  function acc(C, name, val) {
    var get = __native("get " + name, 0, function () {
      if (!(this instanceof C)) throw new TypeError("Illegal invocation");
      return val;
    });
    try { Object.defineProperty(C.prototype, name, { get: get, enumerable: true, configurable: true }); } catch (e) {}
  }
  var navSrc = (window && window.navigator) || {};
  var wkStorage = {
    estimate: function () { return Promise.resolve({ quota: 10737418240, usage: 0, usageDetails: {} }); },
    persist: function () { return new Promise(function () {}); },
    persisted: function () { return Promise.resolve(false); },
    getDirectory: function () { return Promise.resolve(__opfsDir("")); },
  };
  // Chrome's WorkerNavigator.prototype own-name list and ORDER (wkfid2,
  // 2026-10-18): hardwareConcurrency..connection, constructor, hid..
  // storageBuckets. cookieEnabled/webdriver do NOT exist on the worker side.
  try { delete WN.prototype.constructor; } catch (e) {}
  var navA = [
    ["hardwareConcurrency", navSrc.hardwareConcurrency],
    ["appCodeName", "Mozilla"], ["appName", "Netscape"], ["appVersion", navSrc.appVersion],
    ["platform", navSrc.platform], ["product", "Gecko"], ["userAgent", navSrc.userAgent],
    ["language", navSrc.language], ["languages", navSrc.languages], ["onLine", true],
    ["connection", navSrc.connection || { effectiveType: "4g", rtt: 50, downlink: 10, saveData: false, onchange: null }],
  ];
  for (var nk = 0; nk < navA.length; nk++) acc(WN, navA[nk][0], navA[nk][1]);
  try { Object.defineProperty(WN.prototype, "constructor", { value: WN, writable: true, configurable: true }); } catch (e) {}
  var navB = [
    ["hid", {}], ["mediaCapabilities", {}],
    ["permissions", navSrc.permissions || { query: function () { return Promise.resolve({ state: "prompt", onchange: null }); } }],
    ["serial", {}], ["usb", {}],
    ["deviceMemory", navSrc.deviceMemory], ["userAgentData", navSrc.userAgentData],
    ["locks", navSrc.locks || { request: function (n, f) { return Promise.resolve(f ? f({ name: String(n) }) : undefined); } }],
    ["storage", navSrc.storage || wkStorage],
    ["gpu", navSrc.gpu || {}], ["storageBuckets", {}],
  ];
  for (nk = 0; nk < navB.length; nk++) acc(WN, navB[nk][0], navB[nk][1]);
  try {
    Object.defineProperty(WL.prototype, "href", { get: __native("get href", 0, function () { if (!(this instanceof WL)) throw new TypeError("Illegal invocation"); return __g(this, "href", ""); }), enumerable: true, configurable: true });
    Object.defineProperty(WL.prototype, "origin", { get: __native("get origin", 0, function () { if (!(this instanceof WL)) throw new TypeError("Illegal invocation"); return __g(this, "origin", ""); }), enumerable: true, configurable: true });
    Object.defineProperty(WL.prototype, "toString", { value: __native("toString", 0, function () { if (!(this instanceof WL)) throw new TypeError("Illegal invocation"); return __g(this, "href", ""); }), writable: true, configurable: true });
  } catch (e) {}
  __WKC = {
    wgs: WGS, dwgs: DWGS, wn: WN, wl: WL,
    mkNav: function () { return Object.create(WN.prototype); },
    mkLoc: function (url, origin) { var loc = Object.create(WL.prototype); __hide(loc, "href", String(url)); __hide(loc, "origin", String(origin || "")); return loc; },
  };
  return __WKC;
}
function __mkWorkerErrorView() {
  // Chrome gives every Worker its own realm: Error.prepareStackTrace and
  // Error.stackTraceLimit start fresh there, and worker writes must never
  // touch the page realm's values (the device-page worker check leaves its
  // prepareStackTrace installed; leaking it into the page breaks Castle's
  // createRequestToken, which then resolves .stack to a function value).
  var st = { pst: undefined, stl: 10 };
  var view = new Proxy(Error, {
    get: function (t, p) {
      if (p === "prepareStackTrace") return st.pst;
      if (p === "stackTraceLimit") return st.stl;
      return Reflect.get(t, p, view);
    },
    set: function (t, p, v) {
      if (p === "prepareStackTrace") { st.pst = v; return true; }
      if (p === "stackTraceLimit") { st.stl = v; return true; }
      return Reflect.set(t, p, v, view);
    },
  });
  return view;
}
var __WKES = null, __WKCA = null, __WKIDB = null, __WKTTF = null, __WKIB = null, __WKCIB = null;
function __wkEventSource() {
  if (__WKES) return __WKES;
  var C = function EventSource(url) {
    if (new.target === undefined) throw new TypeError("Failed to construct 'EventSource': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
    __hide(this, "url", String(url)); __hide(this, "readyState", 0); __hide(this, "withCredentials", false);
    this.onopen = null; this.onmessage = null; this.onerror = null;
  };
  C.CONNECTING = 0; C.OPEN = 1; C.CLOSED = 2;
  try { Object.defineProperty(C, "name", { value: "EventSource", configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, "close", { value: __native("close", 0, function () { if (!(this instanceof C)) throw new TypeError("Illegal invocation"); __hide(this, "readyState", 2); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, Symbol.toStringTag, { value: "EventSource", configurable: true }); } catch (e) {}
  __WKES = C; return C;
}
function __wkCaches() {
  if (__WKCA) return __WKCA;
  var stores = {};
  function mkCache(n) {
    var store = {};
    return {
      match: function (req) { var k = String((req && req.url) || req); return Promise.resolve(store[k]); },
      put: function (req, res) { store[String((req && req.url) || req)] = res; return Promise.resolve(); },
      add: function () { return Promise.resolve(); },
      addAll: function () { return Promise.resolve(); },
      delete: function (req) { var k = String((req && req.url) || req); var had = k in store; delete store[k]; return Promise.resolve(had); },
      keys: function () { return Promise.resolve(Object.keys(store)); },
    };
  }
  __WKCA = {
    open: function (n) { var k = String(n); if (!stores[k]) stores[k] = mkCache(k); return Promise.resolve(stores[k]); },
    has: function (n) { return Promise.resolve(String(n) in stores); },
    delete: function (n) { var k = String(n); var had = k in stores; delete stores[k]; return Promise.resolve(had); },
    keys: function () { return Promise.resolve(Object.keys(stores)); },
    match: function () { return Promise.resolve(undefined); },
  };
  return __WKCA;
}
function __wkIdb() {
  if (__WKIDB) return __WKIDB;
  function mkReq(name) {
    var req = { result: undefined, error: null, readyState: "done", onsuccess: null, onerror: null, onupgradeneeded: null, source: null, transaction: null };
    req.result = { name: String(name), version: 1,
      objectStoreNames: { length: 0, contains: function () { return false; }, item: function () { return null; } },
      createObjectStore: function (nm) { return { name: String(nm), keyPath: "", autoIncrement: false, createIndex: function () { return {}; }, index: function () { return {}; } }; },
      transaction: function () { return { objectStore: function () { return { get: function () { return mkReq(""); }, put: function () { return mkReq(""); }, add: function () { return mkReq(""); }, delete: function () { return mkReq(""); }, getAll: function () { return mkReq(""); }, clear: function () { return mkReq(""); } }; }, oncomplete: null, onerror: null, onabort: null, commit: function () {} }; },
      close: function () {},
    };
    return req;
  }
  __WKIDB = {
    open: function (n) { var req = mkReq(n); setTimeout(function () { try { if (typeof req.onupgradeneeded === "function") req.onupgradeneeded({ target: req }); } catch (e) {} try { if (typeof req.onsuccess === "function") req.onsuccess({ target: req }); } catch (e) {} }, 0); return req; },
    deleteDatabase: function (n) { return mkReq(n); },
    databases: function () { return Promise.resolve([]); },
    cmp: function (a, b) { return a < b ? -1 : a > b ? 1 : 0; },
  };
  return __WKIDB;
}
function __wkTTF() {
  if (__WKTTF) return __WKTTF;
  var C = function TrustedTypePolicyFactory() { throw new TypeError("Illegal constructor"); };
  try { Object.defineProperty(C, "name", { value: "TrustedTypePolicyFactory", configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, Symbol.toStringTag, { value: "TrustedTypePolicyFactory", configurable: true }); } catch (e) {}
  __WKTTF = C; return C;
}
function __wkImageBitmap() {
  if (__WKIB) return __WKIB;
  var C = function ImageBitmap() { throw new TypeError("Illegal constructor"); };
  try { Object.defineProperty(C, "name", { value: "ImageBitmap", configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, "width", { get: __native("get width", 0, function () { if (!(this instanceof C)) throw new TypeError("Illegal invocation"); return __g(this, "_w", 0); }), enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, "height", { get: __native("get height", 0, function () { if (!(this instanceof C)) throw new TypeError("Illegal invocation"); return __g(this, "_h", 0); }), enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, "close", { value: __native("close", 0, function () {}), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(C.prototype, Symbol.toStringTag, { value: "ImageBitmap", configurable: true }); } catch (e) {}
  __WKIB = C; return C;
}
function __wkCreateImageBitmap() {
  if (__WKCIB) return __WKCIB;
  var C = __wkImageBitmap();
  __WKCIB = __native("createImageBitmap", 1, function () { var b = Object.create(C.prototype); __hide(b, "_w", 0); __hide(b, "_h", 0); return Promise.resolve(b); });
  return __WKCIB;
}
function __mkWorkerScope(w) {
  var scope = {};
  scope.self = scope;
  scope.globalThis = scope;
  scope.onmessage = null;
  scope.onmessageerror = null;
  scope.postMessage = function (d) { __workerToCreator(w, d); };
  scope.__ml = [];
  scope.addEventListener = function (t, f) { if (String(t) === "message" && typeof f === "function") scope.__ml.push(f); };
  scope.removeEventListener = function (t, f) { if (String(t) === "message") { var o = []; for (var i = 0; i < scope.__ml.length; i++) if (scope.__ml[i] !== f) o.push(scope.__ml[i]); scope.__ml = o; } };
  scope.dispatchEvent = function (ev) { if (ev && String(ev.type) === "message") __fireWorkerOnMessage(w, ev); return true; };
  scope.close = function () { w.__dead = true; };
  scope.importScripts = function () {};
  scope.setTimeout = setTimeout; scope.setInterval = setInterval; scope.clearTimeout = clearTimeout; scope.clearInterval = clearInterval;
  scope.queueMicrotask = function (f) { try { Promise.resolve().then(f); } catch (e) { setTimeout(f, 0); } };
  scope.fetch = window.fetch;
  scope.XMLHttpRequest = XMLHttpRequest;
  scope.EventSource = __wkEventSource();
  scope.caches = __wkCaches();
  scope.indexedDB = __wkIdb();
  scope.TrustedTypePolicyFactory = __wkTTF();
  scope.ImageBitmap = __wkImageBitmap();
  scope.createImageBitmap = __wkCreateImageBitmap();
  scope.performance = {
    now: function () { return (typeof performance !== "undefined" && performance.now) ? performance.now() : Date.now(); },
    timeOrigin: (typeof performance !== "undefined" && performance.timeOrigin) || Date.now(),
    getEntriesByType: function () { return []; }, getEntries: function () { return []; },
    mark: function () {}, measure: function () {}, clearMarks: function () {}, clearMeasures: function () {},
  };
  scope.console = console;
  scope.crypto = globalThis.crypto;
  scope.Error = __mkWorkerErrorView();
  scope.atob = (typeof atob !== "undefined") ? atob : undefined;
  scope.btoa = (typeof btoa !== "undefined") ? btoa : undefined;
  scope.TextEncoder = globalThis.TextEncoder; scope.TextDecoder = globalThis.TextDecoder;
  __nrh(scope, "URL", globalThis.URL); scope.URLSearchParams = globalThis.URLSearchParams;
  scope.Blob = globalThis.Blob;
  scope.OffscreenCanvas = OffscreenCanvas;
  scope.trustedTypes = __mkWorkerTT();
  scope.structuredClone = __cloneMsg;
  var WC = __wkWorkerClasses();
  try { Object.setPrototypeOf(scope, WC.dwgs.prototype); } catch (e) {}
  scope.WorkerGlobalScope = WC.wgs;
  scope.DedicatedWorkerGlobalScope = WC.dwgs;
  scope.WorkerNavigator = WC.wn;
  scope.WorkerLocation = WC.wl;
  scope.name = "";
  scope.navigator = WC.mkNav();
  scope.location = WC.mkLoc(w.url, String(w.__origin || ""));
    try { for (var __wk2 in scope) { var __fv2 = scope[__wk2]; if (typeof __fv2 === "function" && __clearSrc) __clearSrc(__fv2); else if (__fv2 && typeof __fv2 === "object") { try { for (var __wk3 in __fv2) { var __fv3 = __fv2[__wk3]; if (typeof __fv3 === "function" && __clearSrc) __clearSrc(__fv3); } } catch (e2) {} } } } catch (e1) {}
return scope;
}
function __workerError(w, msg) {
  try { __log("WORKER-ERR " + msg); } catch (e) {}
  try {
    var ev = { type: "error", message: String(msg), filename: String(w.url), lineno: 0, colno: 0, isTrusted: true };
    if (typeof w.onerror === "function") { try { w.onerror(ev); } catch (e1) { try { if (typeof __fireError === "function") __fireError(e1); } catch (_fe) {} } }
    var ls = w.__listeners || [];
    for (var i = 0; i < ls.length; i++) { try { ls[i].call(w, ev); } catch (e2) { try { if (typeof __fireError === "function") __fireError(e2); } catch (_fe) {} } }
    try { if (typeof w.dispatchEvent === "function") w.dispatchEvent(ev); } catch (e3) {}
  } catch (e) {}
}
function __fireWorkerOnMessage(w, ev) {
  if (w.__dead || !w.__scope) return;
  var sc = w.__scope;
  try { if (typeof sc.onmessage === "function") sc.onmessage(ev); } catch (e) { __workerError(w, "onmessage: " + String((e && e.message) || e)); }
  var ls = sc.__ml || [];
  for (var i = 0; i < ls.length; i++) { try { ls[i].call(sc, ev); } catch (e2) { __workerError(w, "listener: " + String((e2 && e2.message) || e2)); } }
}
function __deliverToWorker(w, d) {
  if (w.__dead || !w.__scope) return;
  var sc = w.__scope;
  var ev = {
    data: __cloneMsg(d), type: "message", isTrusted: true, origin: "", source: null,
    ports: [], lastEventId: "", target: sc, currentTarget: sc,
  };
  __fireWorkerOnMessage(w, ev);
}
function __workerToCreator(w, d) {
  setTimeout(function () {
    if (w.__dead) return;
    var ev = {
      data: __cloneMsg(d), type: "message", isTrusted: true, origin: "", source: null,
      ports: [], lastEventId: "", target: w, currentTarget: w,
    };
    try { if (typeof w.onmessage === "function") w.onmessage(ev); } catch (e) { __log("worker->page onmessage: " + e); try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} }
    var ls = w.__listeners || [];
    for (var i = 0; i < ls.length; i++) { try { ls[i].call(w, ev); } catch (e2) { __log("worker->page listener: " + e2); } }
    try { if (typeof w.dispatchEvent === "function") w.dispatchEvent(ev); } catch (e3) {}
  }, 0);
}
function __runWorkerSource(__wk, __svv, __scp) {
  var __savedEval = globalThis.eval, __hadPristine = !!globalThis.__evPristine;
  var __pristineEval = globalThis.__evPristine || globalThis.eval;
  if (__hadPristine) { try { globalThis.eval = globalThis.__evPristine; } catch (e0) {} }
  var ok = true, err = "", __out = null;
  try {
    // Param-bound worker scope, the proven iframe-shell technique: closures the
    // script creates (its onmessage handler) keep THIS environment alive, so
    // the programs it later eval()s still resolve self/navigator/postMessage
    // here instead of falling through to the page realm.
    __out = (function (self, globalThis, onmessage, onmessageerror, postMessage, addEventListener, removeEventListener, dispatchEvent, close, importScripts, location, navigator, setTimeout, setInterval, clearTimeout, clearInterval, queueMicrotask, fetch, XMLHttpRequest, EventSource, caches, indexedDB, TrustedTypePolicyFactory, ImageBitmap, createImageBitmap, performance, console, crypto, atob, btoa, TextEncoder, TextDecoder, URL, URLSearchParams, Blob, trustedTypes, structuredClone, Error, OffscreenCanvas, eval) {
      // Chrome workers have NO page-realm globals; without these shadows the
      // eval chain falls through to the page realm and leaks them (typeof
      // window === "object" inside a worker = the classic bot tell). Bare
      // reads yield undefined — typeof checks match Chrome exactly; a real
      // unresolvable ReferenceError is impossible while the page realm is
      // the outer scope (banked). Names = measured diff vs Chrome, wkfid2.
      var window, document, screen, getComputedStyle, matchMedia, history, localStorage, sessionStorage, top, parent, frames, alert, confirm, prompt, innerWidth, innerHeight, outerWidth, outerHeight, scrollX, scrollY, pageXOffset, devicePixelRatio, requestIdleCallback, Image, Node, Element, DOMParser, XMLSerializer, AudioContext, BaseAudioContext, OfflineAudioContext, CanvasRenderingContext2D, Navigator;
      var WorkerGlobalScope = __scp.WorkerGlobalScope, DedicatedWorkerGlobalScope = __scp.DedicatedWorkerGlobalScope, WorkerNavigator = __scp.WorkerNavigator, WorkerLocation = __scp.WorkerLocation;
      eval(__svv);
      return { onmessage: onmessage, onmessageerror: onmessageerror };
    })(__scp.self, __scp.globalThis, __scp.onmessage, __scp.onmessageerror, __scp.postMessage, __scp.addEventListener, __scp.removeEventListener, __scp.dispatchEvent, __scp.close, __scp.importScripts, __scp.location, __scp.navigator, setTimeout, setInterval, clearTimeout, clearInterval, __scp.queueMicrotask, __scp.fetch, __scp.XMLHttpRequest, __scp.EventSource, __scp.caches, __scp.indexedDB, __scp.TrustedTypePolicyFactory, __scp.ImageBitmap, __scp.createImageBitmap, __scp.performance, __scp.console, __scp.crypto, __scp.atob, __scp.btoa, __scp.TextEncoder, __scp.TextDecoder, __scp.URL, __scp.URLSearchParams, __scp.Blob, __scp.trustedTypes, __scp.structuredClone, __scp.Error, __scp.OffscreenCanvas, __pristineEval);
  }
  catch (e) { ok = false; err = String((e && e.name) || "?") + " M=" + String((e && e.message)) + " @ " + String((e && e.stack) || "").split(String.fromCharCode(10)).slice(0, 6).join(" | "); }
  if (__hadPristine) { try { globalThis.eval = __savedEval; } catch (e0) {} }
  if (__out) {
    if (typeof __out.onmessage === "function") __scp.onmessage = __out.onmessage;
    if (typeof __out.onmessageerror === "function") __scp.onmessageerror = __out.onmessageerror;
  }
  try { if (typeof __TRACE !== "undefined" && __TRACE) __log("WK-RUN len=" + String(__svv).length + " ok=" + ok + (ok ? "" : " " + err.slice(0, 200))); } catch (e) {}
  if (!ok) __workerError(__wk, "worker-script: " + err.slice(0, 300));
}
function __drainWorkerQueue(w) {
  var q = w.__q; w.__q = [];
  for (var i = 0; i < q.length; i++) { (function (d) { setTimeout(function () { __deliverToWorker(w, d); }, 0); })(q[i]); }
}
function __startWorker(w) {
  if (w.__dead || w.__started) return;
  var src = null;
  try { src = __DCI.t.couBlobs && __DCI.t.couBlobs[w.url]; } catch (e) {}
  if (src == null) {
    try {
      window.fetch(w.url).then(function (r) { return r.text(); }).then(function (txt) {
        if (w.__dead || w.__started) return;
        w.__scope = __mkWorkerScope(w);
        w.__started = true;
        __runWorkerSource(w, String(txt), w.__scope);
        __drainWorkerQueue(w);
      }).catch(function (e) { __workerError(w, "worker fetch: " + String(e)); });
    } catch (e) { __workerError(w, "worker fetch: " + String(e)); }
    return;
  }
  w.__scope = __mkWorkerScope(w);
  w.__started = true;
  __runWorkerSource(w, String(src), w.__scope);
  __drainWorkerQueue(w);
}
// Worker.prototype — Chrome's own-name order [onmessage, postMessage,
// terminate, constructor, onerror] with native-shaped members (probe-st5d,
// 2026-10-18). Event handlers live in a hidden store (Chrome's Worker
// instances carry zero own props for them); addEventListener /
// removeEventListener are inherited from EventTarget.prototype (linked in
// §12) and feed the hidden __l store — delivery calls dispatchEvent, so that
// store is honored alongside the legacy __listeners array.
(function () {
  var WP = Worker.prototype;
  try { delete WP.constructor; } catch (e) {}
  try { delete WP.postMessage; } catch (e) {}
  try { delete WP.terminate; } catch (e) {}
  try { delete WP.addEventListener; } catch (e) {}
  try { delete WP.removeEventListener; } catch (e) {}
  var pm = function (d) {
    try{ if(typeof __EBBS==='function'&&(globalThis.__wkp=(globalThis.__wkp||0)+1)<=40) __EBBS('WKPM-'+String(d).slice(0,40)); }catch(e){}
    try { if (typeof __TRACE !== "undefined" && __TRACE && (globalThis.__WKPM = (globalThis.__WKPM || 0) + 1) <= 60) { __log("WKPM len=" + String(d).length + " head=" + JSON.stringify(String(d).slice(0, 240))); if (typeof __dumpFile === "function" && String(d).length > 60) __dumpFile("wkpm-" + globalThis.__WKPM, String(d)); } } catch (e) {}
    if (this.__dead) return;
    if (!this.__started) { this.__q.push(d); return; }
    var w = this;
    setTimeout(function () { __deliverToWorker(w, d); }, 0);
  };
  var tm = function () { this.__dead = true; this.__q = []; };
  function wg() { if (!(this instanceof Worker)) throw new TypeError("Illegal invocation"); return __g(this, "_w_onmsg", null); }
  function ws(v) { if (!(this instanceof Worker)) throw new TypeError("Illegal invocation"); __hide(this, "_w_onmsg", v); }
  function eg() { if (!(this instanceof Worker)) throw new TypeError("Illegal invocation"); return __g(this, "_w_onerr", null); }
  function es(v) { if (!(this instanceof Worker)) throw new TypeError("Illegal invocation"); __hide(this, "_w_onerr", v); }
  try { Object.defineProperty(WP, "onmessage", { get: __native("get onmessage", 0, wg), set: __native("set onmessage", 1, ws), enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(WP, "postMessage", { value: __native("postMessage", 1, function (d) { return pm.call(this, d); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(WP, "terminate", { value: __native("terminate", 0, function () { return tm.call(this); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(WP, "constructor", { value: Worker, writable: true, enumerable: false, configurable: true }); } catch (e) {}
  try { Object.defineProperty(WP, "onerror", { get: __native("get onerror", 0, eg), set: __native("set onerror", 1, es), enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(WP, Symbol.toStringTag, { value: "Worker", configurable: true }); } catch (e) {}
})();

// ───────────────────────────── document ─────────────────────────────
var document = addEventTarget({
  // the document is a Node (nodeType 9) in a real browser: the CF api.js
  // walker starts at it (createNodeIterator(document, ...)) and a bare
  // object with no nodeType/childNodes reads as a broken element.
  nodeType: 9, nodeName: "#document", childNodes: [],
  readyState: "loading",
  visibilityState: "visible", hidden: false,
  characterSet: "UTF-8", charset: "UTF-8", inputEncoding: "UTF-8",
  contentType: "text/html", compatMode: "CSS1Compat",
  referrer: "", URL: PAGE_URL, documentURI: PAGE_URL,
  domain: window.location.hostname,
  lastModified: new Date().toString(),
  activeElement: null, currentScript: null,
  styleSheets: [], scripts: [], images: [], forms: [], links: [], embeds: [], plugins: [],
  all: [],
  scrollingElement: null,
  fonts: { ready: Promise.resolve(), check: function () { return true; }, load: function () { return Promise.resolve([]); }, addEventListener: function () {}, forEach: function () {} },
  createElement: function (tag) {
    var ts = String(tag);
    if (!/^[a-zA-Z][a-zA-Z0-9._-]*$/.test(ts)) { var ce = new DOMException("Failed to execute 'createElement' on 'Document': The tag name provided ('" + ts + "') is not a valid name.", "InvalidCharacterError"); try { Error.captureStackTrace(ce); } catch (e1) {} throw ce; }
    if (__TRACE && __missLeft > 0) { __log("CREATE " + tag); __missLeft--; }
    var __sdA = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document;
    var n = makeElement(ts);
    if (ts === "script") { __hide(n, "_sf_async_default", true); }
    __ns(n, "__odoc", __sdA);
    return n;
  },
  createElementNS: function (ns, tag) { var __sdB = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document; return __mkNS(ns, String(tag), __sdB); },
  createTextNode: function (t) { var n = makeText(String(t)); return n; },
  createDocumentFragment: function () { var f = makeElement("#fragment"); __nsf(f, "nodeType", 11); return f; },
  createComment: function (t) { var n = makeElement("#comment"); __nsf(n, "nodeType", 8); __ns(n, "text", String(t)); return n; },
  createEvent: __native("createEvent", 1, function (type) {
    var __nt = undefined; try { __nt = __g(this, "_sf_nodeType"); } catch (e0) {}
    if (__nt !== 9) throw new TypeError("Illegal invocation");
    var t = String(type);
    var map = { Event: "Event", Events: "Event", HTMLEvents: "Event", MouseEvent: "MouseEvent", MouseEvents: "MouseEvent", UIEvent: "UIEvent", UIEvents: "UIEvent", KeyboardEvent: "KeyboardEvent", KeyboardEvents: "KeyboardEvent", MessageEvent: "MessageEvent", CustomEvent: "CustomEvent", FocusEvent: "FocusEvent", CompositionEvent: "CompositionEvent", TransitionEvent: "TransitionEvent", StorageEvent: "StorageEvent", HashChangeEvent: "HashChangeEvent", BeforeUnloadEvent: "BeforeUnloadEvent", DragEvent: "DragEvent" };
    var cname = map[t];
    var C = cname ? window[cname] : null;
    if (typeof C !== "function") { var __ce = new DOMException("Failed to execute 'createEvent' on 'Document': The provided event type ('" + t + "') is invalid.", "NotSupportedError"); try { Error.captureStackTrace(__ce); } catch (e1) {} throw __ce; }
    var e = Object.create(C.prototype);
    __evInit(e, cname, "", undefined);
    if (typeof e.initEvent !== "function") { e.initEvent = function (x) { this.type = String(x); }; e.preventDefault = function () {}; e.stopPropagation = function () {}; }
    return e;
  }),
  createRange: function () { return { selectNodeContents: function () {}, createContextualFragment: function (h) { return makeElement("div"); } }; },
  elementFromPoint: function () { return null; },
  elementsFromPoint: function () { return []; },
  getElementById: function (id) {
    var __sdC = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document;
    var rt = __g(__sdC, "__root") || __sdC.documentElement;
    var r = rt ? findById(rt, String(id)) : null;
    if (__TRACE && __missLeft > 0) { __log("GEBI " + id + " -> " + (r ? (__mr(r, "tag") || "?") : "null")); __missLeft--; }
    return r;
  },
  getElementsByTagName: function (t) { var __sdD = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document; var rt = __g(__sdD, "__root") || __sdD.documentElement; if (!rt) return []; var __gA = rt.getElementsByTagName(t); var __gT = String(t).toLowerCase(); if (__gT === "*" || __gT === "html") { var __gO = [rt]; for (var __gI = 0; __gI < __gA.length; __gI++) __gO.push(__gA[__gI]); return __gO; } return __gA; },
  getElementsByClassName: function (c) { var __sdE = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document; var rt = __g(__sdE, "__root") || __sdE.documentElement; return rt ? rt.getElementsByClassName(c) : []; },
  getElementsByName: function () { return []; },
  querySelector: function (s) {
    var __sdF = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document;
    var rt = __g(__sdF, "__root") || __sdF.documentElement;
    var r = rt ? (selectorAll(rt, s)[0] || null) : null;
    if (__TRACE && __missLeft > 0) { __log("QS " + s + " -> " + (r ? (__mr(r, "tag") || "?") : "null")); __missLeft--; }
    return r;
  },
  querySelectorAll: function (s) { var __sdG = (this && typeof this.nodeType === "number" && this.nodeType === 9) ? this : document; var rt = __g(__sdG, "__root") || __sdG.documentElement; var rr = rt ? selectorAll(rt, s) : []; return __DCI.__U6NL ? __DCI.__U6NL(rr) : rr; },
  write: function () { var s = ""; for (var i = 0; i < arguments.length; i++) s += String(arguments[i]); __dwWrite(s, false); },
  writeln: function () { var s = ""; for (var i = 0; i < arguments.length; i++) s += String(arguments[i]); __dwWrite(s, true); },
  open: function () {}, close: function () {},
  execCommand: function () { return false; },
  hasFocus: function () { return true; },
  exitFullscreen: function () { return Promise.resolve(); },
  elementFromPoint: function () { return null; },
  adoptNode: function (n) { return n; },
  importNode: function (n, deep) { try { return (typeof __deepCloneNode === "function" && deep) ? __deepCloneNode(n) : n; } catch (e) { return n; } },
  getSelection: function () { return __SEL7; },
});
__nrh(document, "documentElement", null);
__nrh(document, "head", null);
__nrh(document, "body", null);
__nrh(document, "scrollingElement", null);
// document.implementation: jQuery's load-time feature detection calls
// createHTMLDocument (and parseHTML uses it too); its absence failed
// jquery.min.js outright on sannysoft and starved everything downstream.
document.implementation = {
  createHTMLDocument: function (title) {
    var d = {};
    __nsf(d, "nodeType", 9); __nsf(d, "nodeName", "#document"); __nrh(d, "compatMode", "CSS1Compat");
    __nrh(d, "title", title === undefined ? "" : String(title));
    var html = makeElement("html"), head = makeElement("head"), body = makeElement("body");
    __nsf(head, "parentNode", html); __ns(head, "__parent", html); __nsf(body, "parentNode", html); __ns(body, "__parent", html);
    __nsf(html, "parentNode", d); __ns(html, "__parent", d);
    html.childNodes.push(head, body);
    __nrh(d, "documentElement", html); __nrh(d, "head", head); __nrh(d, "body", body); __nsf(d, "childNodes", [html]);
    d.createElement = function (t) { var n = makeElement(t); __ns(n, "__odoc", d); return n; };
    d.createElementNS = function (ns, t) { return __mkNS(ns, String(t), d); };
    d.createTextNode = function (t) { return makeText(String(t)); };
    d.createComment = function (t) { return __DCI.nc.comment(t); };
    d.createDocumentFragment = function () { return __DCI.nc.fragment(); };
    d.querySelector = function (s) { var r = selectorAll(d.documentElement, s); if (__TRACE && !r[0] && String(s).charAt(0) === "#") { try { if ((globalThis.__QMISSD2 = (globalThis.__QMISSD2 || 0) + 1) <= 400) { __srdump("qmiss", "sel=" + String(s) + " on=d stk=" + __stk7()); } } catch (e) {} } return r[0] || null; };
    d.querySelectorAll = function (s) { var r = selectorAll(d.documentElement, s); return __DCI.__U6NL ? __DCI.__U6NL(r) : r; }; // U2b-6: NodeList
    d.getElementById = function (id) { var __ge2 = findById(d.documentElement, String(id)); if (__TRACE && !__ge2) { try { if ((globalThis.__GMISS2 = (globalThis.__GMISS2 || 0) + 1) <= 400) { __srdump("getid", "id=" + String(id) + " stk=" + __stk7()); } } catch (e) {} } return __ge2; };
    d.getElementsByTagName = function (t) { return d.documentElement.getElementsByTagName(t); };
    d.getElementsByClassName = function (c) { return d.documentElement.getElementsByClassName(c); };
    d.implementation = document.implementation;
    __nrh(d, "defaultView", window);
    return d;
  },
  createDocument: function () { return document.implementation.createHTMLDocument(""); },
  createDocumentType: function (name, pub, sys) { return { name: String(name), publicId: String(pub || ""), systemId: String(sys || "") }; },
  hasFeature: function () { return true; },
};
Object.defineProperty(document, "ownerDocument", { value: null, configurable: true }); // documents have no owner (Chrome: null)
Object.defineProperty(document, "cookie", {
  get: function () { return __cookieAll(); },
  set: function (v) { __cookieSet(String(v)); },
  configurable: true,
});
Object.defineProperty(document, "title", {
  get: function () {
    var t = __g(document, "__title");
    if (t) return textContentOf(t);
    return "";
  },
  set: function (v) {
    var t = __g(document, "__title");
    if (!t) {
      t = makeElement("title");
      __hide(document, "__title", t);
      if (document.head) document.head.appendChild(t);
    }
    t.textContent = String(v);
  },
  configurable: true,
});
Object.defineProperty(document, "location", { get: __native("get location", 0, function () { return window.location; }), set: __native("set location", 1, function (v) { try { window.location.href = String(v); } catch (e) { __log("document.location = " + v); } }), enumerable: true, configurable: false });

function findById(n, id) {
  for (var i = 0; i < n.childNodes.length; i++) {
    var c = n.childNodes[i];
    if (c.nodeType === 1) {
      if (__mr(c, "attrs").id === id) return c;
      var r = findById(c, id);
      if (r) return r;
    }
  }
  return null;
}

// ST-11 U2: HTMLDocument tag lives on the prototype (Chrome); the instance owns nothing.
// document.defaultView: the main document's window — the standard surface
// challenge code resolves by (node.ownerDocument.defaultView).getComputedStyle.
// The shell docs set theirs in __iframeShell; the main doc had NONE.
Object.defineProperty(document, "defaultView", { get: function () { return window; }, configurable: true });
// WH: the document object must be reachable as a global from INSIDE the
// closure too — sections 12-16 read g.document (9 sites) and the tree loader
// is registered through it. (window self-exports at the window.window line.)
globalThis.document = document;
document.__load = function (tree) {
  function build(t) {
    if (t.t === "text") return makeText(__mr(t, "text") || "");
    if (t.t === "comment") return __DCI.nc.comment(__mr(t, "text") || "");
    if (t.t === "doctype") return __DCI.nc.doctype(t.name || "html", t.publicId || "", t.systemId || "");
    var n = makeElement(__mr(t, "tag") || "div", __mr(t, "attrs") || {}, __mr(t, "ns") === "svg" ? "svg" : (__mr(t, "ns") === "math" ? "math" : undefined));
    var kids = t.children || [];
    for (var i = 0; i < kids.length; i++) {
      var c = build(kids[i]);
      __DCI.nc.link(c, n);
      n.childNodes.push(c);
    }
    return n;
  }
  var root = build(tree);
  if (root.nodeType !== 1) root = makeElement("html");
  __hide(document, "__root", root);
  __hide(document, "_rh_documentElement", root);
  __nsf(root, "parentNode", document); __ns(root, "__parent", document);
  var __dtnode = (tree && tree.doctype) ? __DCI.nc.doctype(tree.doctype.name || "html", tree.doctype.publicId || "", tree.doctype.systemId || "") : null;
  __DCI.nc.seedDocument(document, __dtnode, root);
  (function findSpecial(n) {
    for (var i = 0; i < n.childNodes.length; i++) {
      var c = n.childNodes[i];
      if (c.nodeType !== 1) continue;
      if (__mr(c, "tag") === "head" && !document.head) __hide(document, "_rh_head", c);
      if (__mr(c, "tag") === "body" && !document.body) __hide(document, "_rh_body", c);
      if (__mr(c, "tag") === "title" && !__g(document, "__title")) __hide(document, "__title", c);
      findNodesOfType(document, c);
      findSpecial(c);
    }
  })(root);
  if (!document.head) __hide(document, "_rh_head", makeElement("head"));
  if (!document.body) __hide(document, "_rh_body", makeElement("body"));
  __hide(document, "_rh_scrollingElement", document.body);
  __hide(document, "_rh_all", document.getElementsByTagName("*"));
  return true;
};
function findNodesOfType(doc, n) {
  if (__mr(n, "tag") === "script") doc.scripts.push(n);
  if (__mr(n, "tag") === "link") doc.links.push(n);
  if (__mr(n, "tag") === "img") doc.images.push(n);
  if (__mr(n, "tag") === "form") doc.forms.push(n);
}

// ── document.write (parse-time insert, main document only) ───────────
// rt.rs calls __dwBegin(key, mode) before evaluating an initial script
// whose source mentions document.write; the matching <script> element is
// marked and text written by the script splices in as its next sibling —
// the DOM a parser produces for plain-text writes. Non-matches (dynamic
// or late writes, shell-doc writes) stay no-ops: Chrome's document.open()
// wipe for post-parse writes is not emulated. Markup-looking strings are
// inserted as TEXT (bounded approximation).
__dwActive = null;
__dwLast = null;
__DCI.dwBegin = function (key, mode) {
  __dwActive = null;
  __dwLast = null;
  var list = document.scripts || [];
  for (var i = 0; i < list.length; i++) {
    var el = list[i];
    if (el.__dwUsed) continue;
    var m = false;
    if (mode === "src") {
      var es = String(el.src || (__mr(el, "attrs") && __mr(el, "attrs").src) || "");
      m = es === key || String((__mr(el, "attrs") && __mr(el, "attrs").src) || "") === key;
      // The engine resolves srcs with url::Url; the element's src getter uses
      // the prelude's resolveUrl — "./x" stays /./x on one side only. The
      // "/./" normalization makes the two forms meet (scoped here; resolveUrl
      // itself keeps its behavior for every other caller).
      if (!m) m = es.replace(/\/\.\//g, "/") === key.replace(/\/\.\//g, "/");
    } else {
      var t = "";
      for (var j = 0; j < el.childNodes.length; j++) if (el.childNodes[j].nodeType === 3) t += __mr(el.childNodes[j], "text");
      m = t === key;
      if (!m && t) m = t.replace(/\r/g, "") === key.replace(/\r/g, "");
    }
    if (m) { el.__dwUsed = true; __dwActive = el; return true; }
  }
  if (__TRACE) { try { __log("DW-MISS mode=" + mode + " len=" + ((key && key.length) || 0)); } catch (e) {} }
  return false;
};
__DCI.dwEnd = function () { __dwActive = null; __dwLast = null; };
function __dwWrite(s, nl) {
  var el = __dwActive;
  if (!el || !el.parentNode) return;
  s = String(s) + (nl ? "\n" : "");
  if (!s) return;
  var t = makeText(s);
  __ns(t, "__odoc", __ownerDocOf(el));
  var parent = el.parentNode;
  var anchor = __dwLast || el;
  var idx = parent.childNodes.indexOf(anchor);
  if (idx < 0) parent.childNodes.push(t); else parent.childNodes.splice(idx + 1, 0, t);
  __nsf(t, "parentNode", parent); __ns(t, "__parent", parent);
  __dwLast = t;
}
document.__serialize = function () { var rt = __g(document, "__root"); return rt ? serializeNode(rt) : ""; };
// page-text length for the host settle loop — the hidden-text view
// (script/style excluded), preserved after textContent went Chrome-true:
// the early-out thresholds were tuned against these numbers.
__DCI.pageTextLen = function () {
  try {
    var b = globalThis.document && globalThis.document.body;
    if (!b) return 0;
    var t = textContentOf(b);
    return t ? t.length : 0;
  } catch (e) { return 0; }
};



// ───────────────────── dynamic scripts (CF interstitial pattern) ─────
__DCI.pendingScripts = [];
__DCI.pendingEls = [];
// ── ST-7 WH (h): dynamic script preparation timing ──────────────────────
// Chrome (measured, dyn/dyn2 probes 2026-10-20): a script element is PREPARED
// exactly ONCE. A classic inline script executes SYNCHRONOUSLY inside the call
// that prepares it — becoming connected, a text set while connected, or an
// ancestor connecting later — while src and module scripts join the fetch
// queue. Measured: a removed-and-re-attached element does NOT run again, two
// text sets before connect run only the last, an already-started element never
// re-runs on a later text set, a script born in a detached subtree waits for
// the connect, and a connected shadow tree counts as connected.
var __prepEval = null;
function __scriptEval() {
  if (__prepEval) return __prepEval;
  var f = null;
  try { if (typeof globalThis.__evPristine === "function") f = globalThis.__evPristine; } catch (e0) {}
  if (!f) { try { f = (0, eval); } catch (e1) { try { f = eval; } catch (e2) { f = null; } } }
  __prepEval = f;
  return f;
}
function __markHold(n) {
  var p = n, i = 0;
  while (p && i < 200) {
    try { __hide(p, "_scHold", true); } catch (e) {}
    if (p.nodeType === 9) break;
    if (p.nodeType === 11 && (p.host || __g(p, "_sf_host"))) { p = p.host || __g(p, "_sf_host"); i++; continue; }
    p = p.parentNode || __mr(p, "__parent");
    i++;
  }
}
function __prepTree(n) {
  if (!n || n.nodeType !== 1) return;
  if (__mr(n, "tag") === "script") { __prepScript(n); return; }
  var ss = null;
  try { ss = n.getElementsByTagName ? n.getElementsByTagName("script") : null; } catch (e) { ss = null; }
  if (ss) for (var i = 0; i < ss.length; i++) __prepScript(ss[i]);
  try { __hide(n, "_scHold", false); } catch (e2) {}
}
function __prepConnect(host, n) {
  if (!n || n.nodeType !== 1) return;
  if (__mr(n, "tag") !== "script" && __g(n, "_scHold") !== true) return;
  var conn = false;
  try { conn = !!__isConnected(host); } catch (e) { conn = false; }
  if (!conn) return;
  __prepTree(n);
}
function __prepScript(n) {
  if (!n || n.nodeType !== 1 || __mr(n, "tag") !== "script") return;
  if (__g(n, "_scPrepd") === true) return;                  // prepared once (measured E/K/L)
  if (__DCI.t.navReq || __DCI.t.navHtml) return;             // navigating away: scripts never execute
  var ty = "";
  try { ty = String((__mr(n, "attrs") && __mr(n, "attrs").type) || "").toLowerCase(); } catch (e) {}
  if (ty && ty !== "module" && !/javascript|ecmascript/i.test(ty)) return;   // non-JS types never execute
  var src = String((__mr(n, "attrs") && __mr(n, "attrs").src) || n.src || "");
  var code = "";
  // a data: script is SOURCE, not a location — Chrome decodes it locally and
  // never dials it; the wire rejects the scheme (facebook's ServerJS chain
  // died 47/47 on exactly that). Decoded into code; src="" makes it inline.
  if (src.indexOf("data:") === 0) {
    try {
      var __dm = /^data:[^,]*?(;base64)?,([\s\S]*)$/.exec(src);
      if (__dm) code = __dm[1] ? atob(__dm[2]) : decodeURIComponent(__dm[2]);
    } catch (eData) { code = ""; }
    src = "";
  }
  if (!src && !code) { try { code = String(n.textContent || __mr(n, "text") || ""); } catch (e2) { code = ""; } }
  if (!src && !code) return;
  var conn = false;
  try { conn = !!__isConnected(n); } catch (e3) { conn = false; }
  if (!conn) { __markHold(n); return; }                      // detached: waits for the connect
  __hide(n, "_scPrepd", true);
  if (src || ty === "module") { __DCI.pendingScripts.push({ src: String(src), code: String(code), mod: ty === "module" ? 1 : 0 }); __DCI.pendingEls.push(n); return; }
  var ev = __scriptEval();
  if (typeof ev !== "function") return;
  // measured dyn5/dyn7: a dynamic classic script runs with currentScript set
  // and the enclosing script's value is restored immediately after it returns
  try {
    try { __DCI.curBeginEl(n); } catch (e5) {}
    try { ev(code); }
    catch (e4) { try { if (typeof __fireError === "function") __fireError(e4); } catch (e6) {} }
  } finally { try { __DCI.curEnd(); } catch (e7) {} }
}
__DCI.takeScripts = function () {
  if (__DCI.t.navReq || __DCI.t.navHtml) return "[]"; // navigating: the queue died with the document
  // ONE per call: the host runs each dyn script as its own round with a
  // settle between rounds (pacing). Returning the whole queue made the host
  // drop every item after the first; splice keeps the rest queued.
  var out = __DCI.pendingScripts.splice(0, 1);
  return JSON.stringify(out);
};

// ───────────────────── Wave W: stylesheet loads + resource events ─────
// Chrome fetches <link rel=stylesheet> and fires `load`/`error` on the
// element when the fetch settles (capture-phase listeners on window and
// document see them; they never bubble). Sites gate app boot on this
// (x.com's loader inserts its entry module from a capture-phase load
// listener); before this, stylesheets were never fetched and the events
// never fired — the whole CSS-gated hydration class died as "did not
// hydrate" with zero net calls.
function __capFire(o, t, ev) {
  try {
    var C = __g(o, "__LC");
    if (!C || !C[t]) return;
    var a = C[t].slice();
    for (var i = 0; i < a.length; i++) {
      try { ev.currentTarget = o; a[i].call(o, ev); } catch (err) {
        __log("capture " + t + ": " + err);
        try { if (typeof __fireError === "function") __fireError(err, o); } catch (_fe) {}
      }
    }
  } catch (e0) {}
}
function __fireResource(el, type) {
  var ev = { type: type, target: el, currentTarget: el, bubbles: false, cancelable: false };
  // capture descent: window -> document -> element ancestors -> element
  __capFire(window, type, ev);
  __capFire(document, type, ev);
  try {
    var p = el, i = 0;
    var chain = [];
    while (p && i < 100) { chain.push(p); if (p.nodeType === 9) break; p = p.parentNode || __mr(p, "__parent"); i++; }
    for (var j = chain.length - 1; j >= 0; j--) {
      if (chain[j] === el) continue;
      // window/document were fired explicitly above — the DOM chain stops at
      // document (nodeType 9); firing it twice double-delivers the event.
      if (chain[j] === document) continue;
      __capFire(chain[j], type, ev);
    }
  } catch (e1) {}
  // target level: on<type> attribute handler + own listeners
  try {
    var h = el["on" + type];
    if (typeof h === "function") { ev.currentTarget = el; h.call(el, ev); }
  } catch (e2) { try { if (typeof __fireError === "function") __fireError(e2, el); } catch (_fe2) {} }
  try { el.dispatchEvent(ev); } catch (e3) {}
}
__DCI.styleEls = [];
__DCI.takeStyles = function () {
  if (__DCI.t.navReq || __DCI.t.navHtml) return "[]";
  var out = [];
  try {
    var links = document.getElementsByTagName("link");
    for (var i = 0; i < links.length; i++) {
      var el = links[i];
      if (__g(el, "_stTook") === true) continue;
      var rel = String(((__mr(el, "attrs") && __mr(el, "attrs").rel) || el.rel) || "").toLowerCase();
      if (rel.indexOf("stylesheet") < 0) continue;
      var href = String(((__mr(el, "attrs") && __mr(el, "attrs").href) || el.href) || "");
      if (!href) continue;
      __hide(el, "_stTook", true);
      var idx = __DCI.styleEls.length;
      __DCI.styleEls.push(el);
      out.push({ href: href, i: idx });
    }
  } catch (e) {}
  return JSON.stringify(out);
};
__DCI.styleDone = function (i, ok) {
  var el = __DCI.styleEls[i];
  if (!el) return;
  __fireResource(el, ok ? "load" : "error");
};
// ST-8 unit 7 (measured vs Chromium 154 same-hour, dynsrc2/dynsrc3): a
// dynamically inserted external script fires `load` on its element once it
// finished — including a syntax-broken one and one that threw at runtime — a
// fetch failure fires `error` (handler and addEventListener both), and an
// inline dynamic script fires neither (Chrome also fires none).
__DCI.dynDone = function (ok, fire) {
  var el = __DCI.pendingEls.shift();
  if (!el) return true;
  if (fire === 0) return true;
  var type = ok ? "load" : "error";
  try {
    var h = el["on" + type];
    if (typeof h === "function") { try { h.call(el, { type: type, target: el }); } catch (eH) { try { if (typeof __fireError === "function") __fireError(eH); } catch (_e2) {} } }
  } catch (e) {}
  try { el.dispatchEvent({ type: type }); } catch (e2) {}
  return true;
};
// ST-8 unit 7 (dyn3, measured vs Chromium 154 same-hour: dyn-sl/sl2/sl3/
// sl5/sl6): a PARSER-INSERTED (static) EXTERNAL script fires `load` on its
// element once its fetch succeeded — including syntax-broken and throwing
// bodies — and `error` when the fetch failed. INLINE scripts (classic and
// module, static and dynamic — measured dyn-inl2) fire neither. Elements are
// consumed in document order, so a duplicated URL's second script fires on
// its own element (dyn-dup5: both elements fire). Called from the static
// pass in rt.rs.
__DCI.scDone = function (key, ok) {
  var list = null;
  try { list = document.scripts || []; } catch (e) { list = null; }
  if (!list) return false;
  for (var i = 0; i < list.length; i++) {
    var el = list[i], m = false;
    if (__g(el, "_scEvDone", false) === true) continue;
    var es = String(el.src || (__mr(el, "attrs") && __mr(el, "attrs").src) || "");
    m = es === key || String((__mr(el, "attrs") && __mr(el, "attrs").src) || "") === key;
    if (!m) m = es.replace(/\/\.\//g, "/") === String(key).replace(/\/\.\//g, "/");
    if (m) {
      __hide(el, "_scEvDone", true);
      var type = ok ? "load" : "error";
      try {
        var h = el["on" + type];
        if (typeof h === "function") { try { h.call(el, { type: type, target: el }); } catch (eH) { try { if (typeof __fireError === "function") __fireError(eH); } catch (_e2) {} } }
      } catch (e) {}
      try { el.dispatchEvent({ type: type }); } catch (e2) {}
      return true;
    }
  }
  return false;
};

// ── crypto.subtle (host-backed SHA-256 for the PoW stage) ───────────
if (globalThis.crypto && !globalThis.crypto.subtle) {
  var __notSupported = function (name) {
    return function () { return Promise.reject(new Error("NotSupportedError: " + name)); };
  };
  globalThis.crypto.subtle = {
    digest: function (alg, data) {
      return new Promise(function (resolve, reject) {
        try {
          var bytes;
          if (data instanceof ArrayBuffer) bytes = Array.from(new Uint8Array(data));
          else if (data && data.buffer) bytes = Array.from(new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength));
          else if (data && typeof data.length === "number") bytes = Array.from(data);
          else bytes = [];
          var name = String((typeof alg === "string" ? alg : (alg && alg.name) || "SHA-256")).toUpperCase().replace("-", "");
          if (name !== "SHA256") { reject(new Error("NotSupportedError: " + name)); return; }
          var out = new Uint8Array(__sha256(bytes));
          resolve(out.buffer);
        } catch (e) { reject(e); }
      });
    },
    importKey: __notSupported("importKey"),
    deriveBits: __notSupported("deriveBits"),
    encrypt: __notSupported("encrypt"),
    decrypt: __notSupported("decrypt"),
    sign: __notSupported("sign"),
    verify: __notSupported("verify"),
    generateKey: __notSupported("generateKey"),
    exportKey: __notSupported("exportKey"),
    wrapKey: __notSupported("wrapKey"),
    unwrapKey: __notSupported("unwrapKey"),
  };
}

// ── Trusted Types: the CF widget + api gate their boot on presence. Provide the
// API surface (policies apply their own rules; no CSP enforcement — the engine
// has no CSP, and the widget's own policy code supplies the rules).
if (typeof globalThis.trustedTypes === "undefined") {
  (function () {
    var __policies = {};
    function __mkPolicy(name, rules) {
      rules = rules || {};
      var pol = {
        name: String(name),
        createHTML: function (s) { s = String(s == null ? "" : s); return rules.createHTML ? rules.createHTML(s) : s; },
        createScript: function (s) { s = String(s == null ? "" : s); return rules.createScript ? rules.createScript(s) : s; },
        createScriptURL: function (s) { s = String(s == null ? "" : s); return rules.createScriptURL ? rules.createScriptURL(s) : s; },
      };
      try { Object.defineProperty(pol, Symbol.toStringTag, { value: "TrustedTypePolicy" }); } catch (e) {}
      return pol;
    }
    var tt = {
      createPolicy: function (name, rules) {
        var n = String(name);
        if (__policies[n]) return __policies[n];
        var pol = __mkPolicy(n, rules);
        __policies[n] = pol;
        if (typeof __TRACE !== "undefined" && __TRACE) { try { __log("TT-POLICY " + n); } catch (e3) {} }
        return pol;
      },
      getPolicyNames: function () { var o = []; for (var k in __policies) if (Object.prototype.hasOwnProperty.call(__policies, k)) o.push(k); return o; },
      isHTML: function () { return false; },
      isScript: function () { return false; },
      isScriptURL: function () { return false; },
      emptyHTML: "",
      emptyScript: "",
    };
    try { Object.defineProperty(tt, Symbol.toStringTag, { value: "TrustedTypePolicyFactory", configurable: true }); } catch (e) {}
    try { globalThis.trustedTypes = tt; } catch (e) {}
    try { window.trustedTypes = tt; } catch (e) {}
  })();
}

// ── missing realm builtins (parity with Chrome's window set) ────────
if (typeof globalThis.TextEncoder === "undefined") {
  var __TE = function TextEncoder() { __hide(this, "encoding", "utf-8"); };
  __TE.prototype.encode = function (str) {
    str = String(str === undefined ? "" : str);
    var bytes = [];
    for (var i = 0; i < str.length; i++) {
      var c = str.charCodeAt(i);
      if (c < 0x80) bytes.push(c);
      else if (c < 0x800) { bytes.push(0xc0 | (c >> 6), 0x80 | (c & 63)); }
      else if (c >= 0xd800 && c <= 0xdbff && i + 1 < str.length) {
        var c2 = str.charCodeAt(i + 1);
        if (c2 >= 0xdc00 && c2 <= 0xdfff) {
          var cp = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00);
          bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 63), 0x80 | ((cp >> 6) & 63), 0x80 | (cp & 63));
          i++;
        } else bytes.push(0xef, 0xbf, 0xbd);
      } else if (c >= 0xdc00 && c <= 0xdfff) { bytes.push(0xef, 0xbf, 0xbd); }
      else { bytes.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63)); }
    }
    return new Uint8Array(bytes);
  };
  globalThis.TextEncoder = __TE;
  var __TD = function TextDecoder(lbl) { __hide(this, "encoding", lbl || "utf-8"); __hide(this, "fatal", false); __hide(this, "ignoreBOM", false); };
  __TD.prototype.decode = function (buf) {
    if (!buf) return "";
    var b = (buf instanceof ArrayBuffer)
      ? new Uint8Array(buf)
      : (buf && buf.buffer ? new Uint8Array(buf.buffer, buf.byteOffset || 0, buf.byteLength) : new Uint8Array(buf));
    var out = "";
    for (var i = 0; i < b.length; ) {
      var c = b[i];
      if (c < 0x80) { out += String.fromCharCode(c); i++; }
      else if (c < 0xe0) { out += String.fromCharCode(((c & 31) << 6) | (b[i + 1] & 63)); i += 2; }
      else if (c < 0xf0) { out += String.fromCharCode(((c & 15) << 12) | ((b[i + 1] & 63) << 6) | (b[i + 2] & 63)); i += 3; }
      else {
        var cp = ((c & 7) << 18) | ((b[i + 1] & 63) << 12) | ((b[i + 2] & 63) << 6) | (b[i + 3] & 63);
        cp -= 0x10000;
        out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 1023));
        i += 4;
      }
    }
    return out;
  };
  globalThis.TextDecoder = __TD;
}
if (typeof globalThis.Intl === "undefined" || !globalThis.Intl.DateTimeFormat) {
  var __DTF = function DateTimeFormat(locale, opts) { this._l = locale; this._o = opts || {}; };
  __DTF.prototype.resolvedOptions = function () {
    return { locale: "en-US", calendar: "gregory", numberingSystem: "latn", timeZone: IDENT.tz || "UTC" };
  };
  __DTF.prototype.format = function (d) { return String(new Date(d === undefined ? Date.now() : d)); };
  __DTF.prototype.formatToParts = function (d) { return [{ type: "literal", value: String(new Date(d === undefined ? Date.now() : d)) }]; };
  __DTF.supportedLocalesOf = function (l) { return l || []; };
  globalThis.Intl = {
    DateTimeFormat: __DTF,
    NumberFormat: function NumberFormat() {
      this.resolvedOptions = function () { return { locale: "en-US", numberingSystem: "latn" }; };
      this.format = function (x) { return String(x); };
      this.formatToParts = function (x) { return [{ type: "literal", value: String(x) }]; };
    },
    Collator: function Collator() {
      this.compare = function (a, b) { return a < b ? -1 : (a > b ? 1 : 0); };
      this.resolvedOptions = function () { return { locale: "en-US" }; };
    },
    getCanonicalLocales: function (l) { return l || []; },
    supportedValuesOf: function () { return []; },
  };
}
if (typeof globalThis.EventTarget === "undefined") {
  globalThis.EventTarget = function EventTarget() {
    if (new.target === undefined) throw new TypeError("Failed to construct 'EventTarget': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
    // WH (a2): constructed instances carry the trio guard brand. A bare
    // Object.create(EventTarget.prototype) mimics the chain but has no
    // internal slots - Chrome throws Illegal invocation (measured plcg).
    __hide(this, "_et7", 1);
  };
  globalThis.EventTarget.prototype.addEventListener = function () {};
  globalThis.EventTarget.prototype.removeEventListener = function () {};
  globalThis.EventTarget.prototype.dispatchEvent = function () { return true; };
}
if (typeof globalThis.Event === "undefined") {
  globalThis.Event = function Event(type, opts) {
    opts = opts || {};
    this.type = String(type);
    this.bubbles = !!opts.bubbles;
    this.cancelable = !!opts.cancelable;
    this.composed = !!opts.composed;
    this.defaultPrevented = false;
    this.target = null;
  };
  globalThis.Event.prototype.preventDefault = function () { this.defaultPrevented = true; };
  globalThis.Event.prototype.stopPropagation = function () {};
  globalThis.Event.prototype.stopImmediatePropagation = function () {};
  globalThis.CustomEvent = function CustomEvent(type, opts) {
    globalThis.Event.call(this, type, opts);
    this.detail = (opts && opts.detail !== undefined) ? opts.detail : null;
  };
  globalThis.CustomEvent.prototype = Object.create(globalThis.Event.prototype);
}
if (typeof globalThis.AbortController === "undefined") {
  // ST-7 (measured ctormsg/ctormsg3, Chrome 154): AbortSignal is NOT
  // constructible in Chrome — both call forms throw; AbortController mints a
  // per-instance signal; proto own-order + statics (abort/any/timeout) mirror.
  (function () {
    function tag(P, nm) { try { Object.defineProperty(P, Symbol.toStringTag, { value: nm, configurable: true }); } catch (e) {} }
    function sig() {
      var s = Object.create(globalThis.AbortSignal.prototype);
      __hide(s, "_as_ab", false); __hide(s, "_as_rs", undefined); __hide(s, "_as_ob", null); __hide(s, "_s7b", "AbortSignal");
      return s;
    }
    var AS = function AbortSignal() {
      if (new.target !== undefined) throw new TypeError("Failed to construct 'AbortSignal': Illegal constructor");
      throw new TypeError("Illegal constructor");
    };
    var SP = AS.prototype;
    try { Object.defineProperty(SP, "aborted", { get: __native("get aborted", 0, function () { __s7k(this, "AbortSignal"); return __g(this, "_as_ab") === true; }), enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(SP, "reason", { get: __native("get reason", 0, function () { __s7k(this, "AbortSignal"); return __g(this, "_as_rs"); }), enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(SP, "onabort", { get: __native("get onabort", 0, function () { __s7k(this, "AbortSignal"); var h = __g(this, "_as_ob"); return h === undefined ? null : h; }), set: __native("set onabort", 1, function (v) { __s7k(this, "AbortSignal"); __hide(this, "_as_ob", typeof v === "function" ? v : null); }), enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(SP, "throwIfAborted", { value: __native("throwIfAborted", 0, function () { __s7k(this, "AbortSignal"); if (__g(this, "_as_ab") === true) { var r = __g(this, "_as_rs"); if (r !== undefined) throw r; throw new DOMException("signal is aborted without reason", "AbortError"); } }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    try { var cd = Object.getOwnPropertyDescriptor(SP, "constructor"); if (cd) { delete SP.constructor; Object.defineProperty(SP, "constructor", cd); } } catch (e) {}
    tag(SP, "AbortSignal");
    try { if (globalThis.EventTarget && globalThis.EventTarget.prototype) Object.setPrototypeOf(SP, globalThis.EventTarget.prototype); } catch (e) {}
    try { Object.defineProperty(AS, "abort", { value: __native("abort", 0, function () { var reason = arguments[0]; var s = sig(); __hide(s, "_as_ab", true); __hide(s, "_as_rs", reason !== undefined ? reason : new DOMException("signal is aborted without reason", "AbortError")); return s; }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(AS, "any", { value: __native("any", 1, function (signals) { var s = sig(); try { for (var i = 0; i < signals.length; i++) { if (__g(signals[i], "_as_ab") === true) { __hide(s, "_as_ab", true); __hide(s, "_as_rs", __g(signals[i], "_as_rs")); break; } } } catch (e2) {} return s; }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(AS, "timeout", { value: __native("timeout", 1, function (ms) { var s = sig(); setTimeout(function () { if (__g(s, "_as_ab") !== true) { __hide(s, "_as_ab", true); __hide(s, "_as_rs", new DOMException("signal timed out", "TimeoutError")); } }, Number(ms) || 0); return s; }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    globalThis.AbortSignal = AS;
    var AC = function AbortController() {
      if (new.target === undefined) throw new TypeError("Failed to construct 'AbortController': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      __hide(this, "_ac_sig", sig()); __hide(this, "_s7b", "AbortController");
    };
    var AP = AC.prototype;
    try { Object.defineProperty(AP, "signal", { get: __native("get signal", 0, function () { __s7k(this, "AbortController"); var s = __g(this, "_ac_sig"); if (s === undefined) { s = sig(); __hide(this, "_ac_sig", s); } return s; }), enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(AP, "abort", { value: __native("abort", 0, function (reason) { __s7k(this, "AbortController"); var s = __g(this, "_ac_sig"); if (s === undefined || __g(s, "_as_ab") === true) return; __hide(s, "_as_ab", true); __hide(s, "_as_rs", reason); var h = __g(s, "_as_ob"); if (typeof h === "function") { try { h.call(s, { type: "abort", target: s }); } catch (e2) { try { if (typeof __fireError === "function") __fireError(e2); } catch (_fe) {} } } }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    try { var cd2 = Object.getOwnPropertyDescriptor(AP, "constructor"); if (cd2) { delete AP.constructor; Object.defineProperty(AP, "constructor", cd2); } } catch (e) {}
    tag(AP, "AbortController");
    globalThis.AbortController = AC;
  })();
}
if (typeof globalThis.structuredClone === "undefined") {
  globalThis.structuredClone = function structuredClone(v) {
    try { return JSON.parse(JSON.stringify(v)); } catch (e) { return v; }
  };
}
if (typeof globalThis.MutationObserver === "undefined") {
  // ST-7 (measured ctormsg3, Chrome 154): 1-arg guards, proto own-order
  // (members, constructor last), IntersectionObserver option getters + the
  // trackVisibility/delay validation, toStringTag per interface.
  (function () {
    function tag(P, nm) { try { Object.defineProperty(P, Symbol.toStringTag, { value: nm, configurable: true }); } catch (e) {} }
    function mth(P, nm, ln, fn) { try { Object.defineProperty(P, nm, { value: __native(nm, ln, __s7w(P, fn)), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
    function acc(P, nm, fn) { try { Object.defineProperty(P, nm, { get: __native("get " + nm, 0, __s7w(P, fn)), enumerable: true, configurable: true }); } catch (e) {} }
    function last(P) { try { var d = Object.getOwnPropertyDescriptor(P, "constructor"); if (d) { delete P.constructor; Object.defineProperty(P, "constructor", d); } } catch (e) {} }
    var MO = function MutationObserver(cb) {
      if (new.target === undefined) throw new TypeError("Failed to construct 'MutationObserver': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      if (arguments.length < 1) throw new TypeError("Failed to construct 'MutationObserver': 1 argument required, but only " + arguments.length + " present.");
      __hide(this, "_mo_cb", cb); __hide(this, "_mo_recs", []); __hide(this, "_s7b", "MutationObserver");
    };
    var MP = MO.prototype; __s7B.set(MP, "MutationObserver");
    mth(MP, "disconnect", 0, function () { __hide(this, "_mo_recs", []); });
    mth(MP, "observe", 1, function () {});
    mth(MP, "takeRecords", 0, function () { var r = __g(this, "_mo_recs"); __hide(this, "_mo_recs", []); return r === undefined ? [] : r; });
    last(MP); tag(MP, "MutationObserver");
    globalThis.MutationObserver = MO;
    var IO = function IntersectionObserver(cb) {
      if (new.target === undefined) throw new TypeError("Failed to construct 'IntersectionObserver': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      if (arguments.length < 1) throw new TypeError("Failed to construct 'IntersectionObserver': 1 argument required, but only " + arguments.length + " present.");
      var opts = arguments[1] || {};
      if (opts.trackVisibility && !(typeof opts.delay === "number" && opts.delay >= 100)) throw new TypeError("Failed to construct 'IntersectionObserver': To enable the 'trackVisibility' option, you must also use a 'delay' option with a value of at least 100. Visibility is more expensive to compute than the basic intersection; enabling this option may negatively affect your page's performance. Please make sure you *really* need visibility tracking before enabling the 'trackVisibility' option.");
      __hide(this, "_io_cb", cb);
      __hide(this, "_io_root", opts.root !== undefined ? opts.root : null);
      __hide(this, "_io_rm", opts.rootMargin !== undefined ? String(opts.rootMargin) : "0px 0px 0px 0px");
      __hide(this, "_io_sm", opts.scrollMargin !== undefined ? String(opts.scrollMargin) : "0px 0px 0px 0px");
      var th = opts.threshold; var ths = [0];
      if (typeof th === "number") ths = [th];
      else if (th && typeof th.length === "number") { ths = []; for (var i = 0; i < th.length; i++) ths.push(Number(th[i])); }
      __hide(this, "_io_th", ths);
      __hide(this, "_io_delay", typeof opts.delay === "number" ? opts.delay : 0);
      __hide(this, "_io_tv", !!opts.trackVisibility); __hide(this, "_s7b", "IntersectionObserver");
    };
    var IP = IO.prototype; __s7B.set(IP, "IntersectionObserver");
    acc(IP, "root", function () { var v = __g(this, "_io_root"); return v === undefined ? null : v; });
    acc(IP, "rootMargin", function () { var v = __g(this, "_io_rm"); return v === undefined ? "0px 0px 0px 0px" : v; });
    acc(IP, "scrollMargin", function () { var v = __g(this, "_io_sm"); return v === undefined ? "0px 0px 0px 0px" : v; });
    acc(IP, "thresholds", function () { var v = __g(this, "_io_th"); return v === undefined ? [0] : v; });
    acc(IP, "delay", function () { var v = __g(this, "_io_delay"); return v === undefined ? 0 : v; });
    acc(IP, "trackVisibility", function () { return __g(this, "_io_tv") === true; });
    mth(IP, "disconnect", 0, function () {});
    mth(IP, "observe", 1, function () {});
    mth(IP, "takeRecords", 0, function () { return []; });
    mth(IP, "unobserve", 1, function () {});
    last(IP); tag(IP, "IntersectionObserver");
    globalThis.IntersectionObserver = IO;
    var RO = function ResizeObserver(cb) {
      if (new.target === undefined) throw new TypeError("Failed to construct 'ResizeObserver': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      if (arguments.length < 1) throw new TypeError("Failed to construct 'ResizeObserver': 1 argument required, but only " + arguments.length + " present.");
      __hide(this, "_ro_cb", cb); __hide(this, "_s7b", "ResizeObserver");
    };
    var RP = RO.prototype; __s7B.set(RP, "ResizeObserver");
    mth(RP, "disconnect", 0, function () {});
    mth(RP, "observe", 1, function () {});
    mth(RP, "unobserve", 1, function () {});
    last(RP); tag(RP, "ResizeObserver");
    globalThis.ResizeObserver = RO;
  })();
}
if (typeof globalThis.PerformanceObserver === "undefined") {
  var __PO = function PerformanceObserver(cb) { this.__cb = typeof cb === "function" ? cb : null; };
  // ST-11 U2b-1: Chrome shapes — native member texts, own order
  // [disconnect, observe, takeRecords, constructor].
  (function (P) {
    function m(nm, ln, fn) { try { Object.defineProperty(P, nm, { value: __native(nm, ln, fn), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
    m("disconnect", 0, function () {});
    m("observe", 0, function () {});
    m("takeRecords", 0, function () { return []; });
    try { var d = Object.getOwnPropertyDescriptor(P, "constructor"); if (d) { delete P.constructor; Object.defineProperty(P, "constructor", d); } } catch (e) {}
  })(__PO.prototype);
  __PO.supportedEntryTypes = ["element", "event", "first-input", "largest-contentful-paint", "layout-shift", "longtask", "navigation", "paint", "resource", "visibility-state"];
  globalThis.PerformanceObserver = __PO;
}
if (typeof globalThis.reportError === "undefined") {
  globalThis.reportError = function (e) { try { __log("reportError: " + e); } catch (_) {} };
}

// ── tracer sweep: attach miss-logging under the main namespaces ─────
// (DONCORE_TRACE_SWEEP — its own switch: the wrappers are slow enough to
// skew timing, so plain DONCORE_TRACE stays pass-through log verbosity.)
if (__TRACE && __DCI.TRACE_SWEEP) {
  var __rgopd = Reflect.getOwnPropertyDescriptor;
  // SAFE subject label: never INVOKE getters. Reading o.nodeName on the
  // document ran through a guarded accessor -> ownNT() -> GOPD(document,
  // "nodeType") -> this wrapper -> read o.nodeName -> ... — unbounded
  // recursion that survived on the deep stack and burned the whole budget
  // under TRACE (found 2026-10-18; tr-t0: 65k identical OGOPD lines).
  var __subj = function (o) {
    try {
      if (!o) return typeof o;
      var d = __rgopd(o, "nodeName");
      if (d && typeof d.value === "string") return d.value;
      var t = __rgopd(o, Symbol.toStringTag);
      if (t && typeof t.value === "string") return t.value;
      return typeof o === "object" ? "Object" : typeof o;
    } catch (e) { return typeof o; }
  };
  Reflect.getOwnPropertyDescriptor = function (o, k) {
    var r = __rgopd(o, k);
    try {
      __log(
        "GOPD " + k + " on " + __subj(o) + " -> " +
        (r ? "descr(get:" + typeof r.get + ",set:" + typeof r.set + ",w:" + r.writable + ",v:" + typeof r.value : "undefined")
      );
    } catch (e) {}
    return r;
  };
  var __rh = Reflect.has;
  Reflect.has = function (o, k) {
    var r = __rh(o, k);
    try { __log("HAS " + k + " on " + __subj(o) + " -> " + r); } catch (e) {}
    return r;
  };
  var __ogopd = Object.getOwnPropertyDescriptor;
  Object.getOwnPropertyDescriptor = function (o, k) {
    var r = __ogopd(o, k);
    try {
      __log(
        "OGOPD " + k + " on " + __subj(o) + " -> " +
        (r ? "descr(get:" + typeof r.get + ",v:" + typeof r.value : "undefined")
      );
    } catch (e) {}
    return r;
  };
  // Reflect miss tracer: log any missing method name (read-only; no stubs)
  var __rapply = Reflect.apply;
  var __rconstruct = Reflect.construct;
  Reflect.apply = function (t, thisArg, args) {
    try { globalThis.__LASTRAPPLY = Object.prototype.toString.call(t) + " name=" + (t && t.name) + " str=" + String(t).slice(0, 60); } catch (e) {}
    if (typeof t !== "function") {
      try { __log("RAPPLY-MISS typeof=" + typeof t + " t=" + String(t).slice(0, 60)); } catch (e) {}
      try { __log("RAPPLY-MISS args=" + JSON.stringify(args).slice(0, 220)); } catch (e) { try { __log("RAPPLY-MISS args-unjson len=" + ((args && args.length) || "?")); } catch (e2) {} }
    }
    return __rapply(t, thisArg, args);
  };
  Reflect.construct = function (t, args, nt) {
    if (typeof t !== "function") {
      try { __log("RCONSTRUCT-MISS typeof=" + typeof t + " t=" + String(t).slice(0, 60)); } catch (e) {}
      try { __log("RCONSTRUCT-MISS args=" + JSON.stringify(args).slice(0, 220)); } catch (e) { try { __log("RCONSTRUCT-MISS args-unjson len=" + ((args && args.length) || "?")); } catch (e2) {} }
    }
    try {
      // a 2-arg call must stay 2-arg: forwarding an explicit undefined
      // newTarget throws "not a constructor" in every spec engine.
      return nt === undefined ? __rconstruct(t, args) : __rconstruct(t, args, nt);
    } catch (e) {
      try {
        __log("RCONSTRUCT-THREW name=" + ((t && t.name) || "?") + " tag=" + Object.prototype.toString.call(t) +
          " str=" + String(t).slice(0, 160) +
          " keys=" + (t ? Object.keys(t).slice(0, 12).join(",") : "?") +
          " args=" + (args ? Array.prototype.slice.call(args, 0, 3).map(function (a) { return Object.prototype.toString.call(a); }).join("|") : "?") +
          " arglen=" + (args && typeof args.length !== "undefined" ? String(args.length) : "?") +
          " msg=" + String((e && e.name) || "?") + ": " + String((e && e.message) || "?") +
          " ntT=" + (nt === undefined ? "undef" : typeof nt) + " argl=" + arguments.length +
          " tEQA=" + (t === Array) + " tEQB=" + (t === BroadcastChannel) +
          " stk=" + String(new Error().stack || "").split("\n").slice(1, 5).join(" | "));
      } catch (e2) {}
      throw e;
    }
  };
  globalThis.Reflect = new Proxy(Reflect, {
    get: function (t, k) {
      var v = t[k];
      if (v === undefined && typeof k === "string") {
        __log("REFLECT-MISS " + JSON.stringify(k));
      }
      return v;
    },
  });
  __traceProto(EP, "el");
  __traceProto(TP, "text");
  __traceProto(document, "document");
  if (__TRACE) { try { __traceProto(window, "window"); } catch (e) {} }
  try { __traceProto(navigator, "navigator"); } catch (e) {}
  try { __traceProto(screen, "screen"); } catch (e) {}
  try { __traceProto(location, "location"); } catch (e) {}
  try { __traceProto(history, "history"); } catch (e) {}
  try { __traceProto(performance, "performance"); } catch (e) {}
  try { __traceProto(crypto, "crypto"); } catch (e) {}
  try { __traceProto(localStorage, "localStorage"); } catch (e) {}
  try { __traceProto(sessionStorage, "sessionStorage"); } catch (e) {}
  try { __traceProto(navigator.userAgentData, "uad"); } catch (e) {}
  try { __traceProto(navigator.plugins, "plugins"); } catch (e) {}
  try { __traceProto(navigator.mimeTypes, "mimeTypes"); } catch (e) {}
}

// forensics: snapshot of pending async work at stall time (TRACE runs)
__DCI.forensics = function () {
  try {
    var s = { nT: __timers.length, t: [], raf: __rafQ.length, dcl: !!__DCI.t.dclFired, ld: !!__DCI.t.loadFired };
    for (var i = 0; i < __timers.length && i < 16; i++) {
      var t = __timers[i];
      s.t.push((t.kind || "t") + (t.ms | 0) + "~" + Math.max(0, ((t.due || 0) - __nowMs()) | 0));
    }
    if (globalThis.__timerLog) s.tl = globalThis.__timerLog.slice(-30);
    if (globalThis.__aelLog) s.al = globalThis.__aelLog.slice(-30);
    if (globalThis.__optLog) s.ol = globalThis.__optLog.slice(-80);
    try { s.wl = Object.keys(__g(window, "__l") || {}).join("/"); } catch (e) {}
    try { if (__DCI.t.WLOG && __DCI.t.WLOG.length) s.wlog = __DCI.t.WLOG.slice(-12); } catch (e) {}
    try { s.dl = Object.keys(__g(document, "__l") || {}).join("/"); } catch (e) {}
    try { s.ifs = document.getElementsByTagName("iframe").length; } catch (e) {}
    return JSON.stringify(s);
  } catch (e) { return "forensics-err:" + e; }
};

// uncaught-exception reporting (browser fidelity: uncaught timer exceptions fire window 'error')
__fireError = function (e, tgt) {
  try {
    if (__DCI.t.inFireErr) return;
    __DCI.t.inFireErr = true;
    var nm = String((e && e.name) || "Error");
    var msg = (e && e.message !== undefined && e.message !== null) ? String(e.message) : "";
    var full = "Uncaught " + nm + (msg ? ": " + msg : "");
    var fn = "", ln = 0, co = 0;
    try {
      var st = String((e && e.stack) || "");
      var m2 = /<input>:(\d+):(\d+)/.exec(st) || /:(\d+):(\d+)/.exec(st);
      if (m2) { ln = Number(m2[1]) || 0; co = Number(m2[2]) || 0; }
      var copt = globalThis._cf_chl_opt || {};
      var base = "";
      try { base = globalThis.location.origin || (String(globalThis.location.href).match(/^[a-z]+:\/\/[^\/]+/) || [""])[0]; } catch (_b) {}
      fn = base + "/cdn-cgi/challenge-platform/h/" + (copt["uzyK3"] || "b") + "/orchestrate/chl_page/v1?ray=" + (copt["UORq8"] || "");
    } catch (_f) {}
    if (__TRACE) { try { __log("ERRDISP " + String(full).slice(0, 260) + " @@ " + String(fn) + ":" + ln + ":" + co + " STK " + String(e && e.stack || "").split("\n").slice(0, 6).join(" >> ").slice(0, 400)); } catch (eEL) {} }
    // fire at the window the error belongs to: an error in a shell frame's
    // realm reaches that frame's own 'error' listeners (browser semantics),
    // everything else reports at the top window.
    var W = (tgt && tgt !== window && tgt.window === tgt && typeof tgt.dispatchEvent === "function") ? tgt : window;
    try { if (typeof W.onerror === "function") W.onerror(full, fn, ln, co, e); } catch (_on) {}
    var evE = { type: "error", message: full, filename: fn, lineno: ln, colno: co, error: e };
    __evSet(evE, "isTrusted", true);
    W.dispatchEvent(evE);
  } catch (_e) {} finally { __DCI.t.inFireErr = false; }
};

// ───────────────────────────── lifecycle ────────────────────────────
__DCI.settle = function () {
  var ran = 0;
  var budget = 2000;
  if (__DCI.t.navReq || __DCI.t.navHtml) return 0; // navigating: the document is dead
  // ── lifecycle events, BEFORE the timers (measured, dyn3) ──────────────
  // Chrome fires DOMContentLoaded the moment parsing ends — ahead of any timer
  // armed during the parse (dcl@18, t0@23) — and load once the document's
  // resources are done, never waiting on the page's own timers (load@19 with a
  // 50 ms timer still pending). "Resources done" here = the dynamic-script
  // queue is empty (the host fetches scripts synchronously).
  // __DCI.t.parseHeld holds both while the host is still parsing the main
  // document; realms the host never marks are unaffected.
  if (__DCI.t.parseHeld !== true && !__DCI.t.dclFired) {
    __DCI.t.dclFired = true;
    __hide(document, "_rh_readyState", "interactive");
    try { window.dispatchEvent({ type: "DOMContentLoaded" }); } catch (e) {}
    try { document.dispatchEvent({ type: "DOMContentLoaded" }); } catch (e) {}
    ran++;
  }
  if (__DCI.t.parseHeld !== true && !__DCI.t.loadFired && !(__DCI.pendingScripts && __DCI.pendingScripts.length)) {
    __DCI.t.loadFired = true;
    __hide(document, "_rh_readyState", "complete");
    try { window.dispatchEvent({ type: "load" }); } catch (e) {}
    try { document.dispatchEvent({ type: "load" }); } catch (e) {}
    ran++;
  }
  // (the lifecycle block above runs first: a browser's DCL/load tasks precede
  // the page's own timers)
  // NOTE: dyn scripts do NOT pre-empt timers here. A script that blocks the
  // timer chain stalls challenge state machines (the lab submission died to
  // that); a script that waits for a 3s gap starves behind dense tick loops
  // (the live precursor died to THAT). The engine (rt.rs) resolves it with a
  // dynamic settle horizon: with scripts pending it exits at the next
  // >=300ms pause and evals them there. Timers always keep firing.
  while (__timers.length && ran < budget) {
    // browsers run timers in due order — pick the earliest-due, not queue order
    var bi = 0;
    for (var i = 1; i < __timers.length; i++) {
      var a = __timers[i], b = __timers[bi];
      if ((a.due || 0) < (b.due || 0) || ((a.due || 0) === (b.due || 0) && (a.id || 0) < (b.id || 0))) bi = i;
    }
    var t = __timers[bi];
    var wait = (t.due || 0) - __nowMs();
    if (wait > 0 && wait > 30) {
      // measure + compress (DONCORE_FAST_TICKS): long-wait returns are counted
      // per (kind,ms) bucket; a bucket that recurs is a CADENCE (tick loop),
      // not a deadline. FTK-COMP logs the bucket at compression time.
      var __bk = (t.kind || "t") + (t.ms | 0);
      if (!__DCI.t.waitBk) __DCI.t.waitBk = {};
      __DCI.t.waitBk[__bk] = (__DCI.t.waitBk[__bk] || 0) + 1;
      if (__DCI.TRACE_ON) { try { __log("FTK " + __bk + " #" + __DCI.t.waitBk[__bk] + " w=" + (wait | 0)); } catch (e) {} }
      if (__DCI.FAST_TICKS && (__DCI.t.waitBk[__bk] || 0) >= 6) {
        wait = 20;
      }
      if (wait > 30) {
        return -(wait < 2000000000 ? (wait | 0) : 2000000000);
      }
    }
    if (wait > 0) {
      __sleep(wait);
    }
    var __isInt = (t.kind === "i");
    __timers.splice(bi, 1);
    if (__isInt) __timers.push(t);
    try { if (t.kind === "t" && t.fn) { t.fn.__armCount = (t.fn.__armCount || 0) + 1; } } catch (e) {}
    try { if (t.kind === "i") { t.fires = (t.fires || 0) + 1; } } catch (e) {}
    try { t.fn.apply(null, t.args || []); } catch (e) { try{ if(typeof __EBBS==='function')__EBBS('TMRF-ERR '+String((e&&e.message)||e).slice(0,120)); }catch(_x){} __log("timer: " + String((e && e.name) || "?") + ": " + String((e && e.message) || e) + " @@ " + String((e && e.stack) || "").split("\n").slice(0, 12).join(" >> ") + " lastcall=" + String(globalThis.__LASTCALL) + " on " + String(globalThis.__LASTOBJ) + " lra=" + String(globalThis.__LASTRAPPLY)); try { __fireError(e); } catch (_e2) {} }
    if (__isInt) {
      // An interval stays visible to clearTimeout during its own callback, so
      // the common `var iv=setInterval(...clearInterval(iv)...)` idiom can
      // cancel it; a callback-cleared interval spliced itself out — don't re-arm.
      if (__timers.indexOf(t) >= 0) {
        // interval tick compression (opt-in, DONCORE_FAST_TICKS): after 3 real
        // fires, an interval that is the realm's pending work re-arms at ~20ms —
        // its cadence is scheduling. Watchdog conditions read Date.now() and
        // stay wall-clock true.
        var __idelay = (t.ms || 0);
        if (__DCI.FAST_TICKS && (t.fires || 0) >= 3 && __idelay >= 250) __idelay = 20;
        t.due = __nowMs() + __idelay;
      }
    }
    ran++;
  }
  while (__rafQ.length && ran < budget) {
    var f = __rafQ.shift();
    try { f(__nowMs()); } catch (e) { __log("raf: " + e); try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} }
    ran++;
  }
  return ran;
};

// constructor surface
window.HTMLElement = HTMLElement;
window.Element = HTMLElement;
window.Node = HTMLElement;
window.Text = TextNode;
window.Event = function (t) {}; // U2b-2: state via Na+__evInit; writes were duplicated
window.CustomEvent = function (t) {};
window.MouseEvent = function (t) {};
window.PointerEvent = function (t) {};
window.KeyboardEvent = function (t) {};
try { var __ctorNames = { Event: window.Event, CustomEvent: window.CustomEvent, MouseEvent: window.MouseEvent, PointerEvent: window.PointerEvent, KeyboardEvent: window.KeyboardEvent }; for (var __cn in __ctorNames) { try { if (!__ctorNames[__cn].name) Object.defineProperty(__ctorNames[__cn], "name", { value: __cn, configurable: true }); } catch (e2) {} } } catch (e) {}
window.HTMLCanvasElement = tagCtor("canvas");
window.HTMLDivElement = tagCtor("div");
window.HTMLSpanElement = tagCtor("span");
window.HTMLAnchorElement = tagCtor("a");
window.HTMLInputElement = tagCtor("input");
window.HTMLImageElement = tagCtor("img");
window.HTMLFormElement = tagCtor("form");
window.HTMLIFrameElement = tagCtor("iframe");
window.HTMLScriptElement = tagCtor("script");
window.HTMLStyleElement = tagCtor("style");
window.HTMLBodyElement = tagCtor("body");
window.HTMLHeadElement = tagCtor("head");
window.HTMLHtmlElement = tagCtor("html");
window.OfflineAudioContext = OfflineAudioContext;
window.AudioContext = AudioContext;
window.WebGLRenderingContext = function () {};
window.CanvasRenderingContext2D = function () {};
window.XMLHttpRequest = XMLHttpRequest;
window.WebSocket = WebSocket;
window.Worker = Worker;
window.OffscreenCanvas = OffscreenCanvas;
window.Headers = Headers;
__nrh(window, "URL", (function () {
  function URL(u, base) {
    if (new.target === undefined) throw new TypeError("Failed to construct 'URL': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
    if (arguments.length === 0) throw new TypeError("Failed to construct 'URL': 1 argument required, but only 0 present.");
    var href = String(u);
    // 2-arg form: resolve a relative reference against the base URL
    if (base !== undefined && !/^[a-zA-Z][\w+.-]*:/.test(href)) {
      var bs = String(base);
      var broot = bs.match(/^(https?:\/\/[^\/?#]+)/);
      if (broot) {
        if (href.charAt(0) === "#") href = bs.replace(/#.*$/, "") + href;
        else if (href.charAt(0) === "/") href = broot[1] + href;
        else href = bs.replace(/[^\/?#\s]*$/, "") + href;
      }
    }
    this.href = href;
    var m = href.match(/^(https?:)\/\/([^\/?#]*)([^?#]*)/);
    this.protocol = m ? m[1] : (href.match(/^[a-zA-Z][\w+.-]*:/) || [""])[0];
    this.host = m ? m[2] : "";
    var p = (m ? m[2] : "").split(":");
    this.hostname = p[0]; this.port = p[1] || "";
    this.pathname = m && m[3] ? m[3] : "";
    this.search = (href.match(/\?[^#]*/) || [""])[0];
    this.hash = (href.match(/#.*/) || [""])[0];
    // .origin: "null" for opaque, scheme://host for http(s) — the turnstile
    // loader computes its iframeOrigin with it and silently drops every
    // postMessage when it reads undefined.
    var om = href.match(/^(https?:\/\/[^\/?#]+)/);
    __hide(this, "origin", om ? om[1] : "null");
  }
  // ST-8: a random v4 UUID (Chrome's shape; the old fixed counter was the same
  // URL on every page and run) and the blob's BYTES + type kept alongside the
  // text, because fetch()/XHR of a blob: URL must come from here, not the wire.
  function __couStore(__u, __b) {
    var __bin = __blobBin(__b);
    if (!__bin) { var __t = __blobTxt(__b); __bin = __utf8Enc(__t != null ? __t : String(__b)); }
    __DCI.t.couBlobs = __DCI.t.couBlobs || {}; __DCI.t.couBlobs[__u] = __utf8Dec(__bin);
    __DCI.t.couBin = __DCI.t.couBin || {}; __DCI.t.couBin[__u] = __bin;
    __DCI.t.couTy = __DCI.t.couTy || {}; __DCI.t.couTy[__u] = __g(__b, "_b_ty") || "";
    return __bin.length;
  }
  URL.createObjectURL = function (__b) { try { var __id = __blobUuid(); var __org = "null"; try { __org = (__g(globalThis, "__frameOrigin") && String(__g(globalThis, "__frameOrigin")).indexOf("http") === 0) ? __g(globalThis, "__frameOrigin") : ((window.location && window.location.origin) || "null"); } catch (e0) {} var __u = "blob:" + __org + "/" + __id; var __n = __couStore(__u, __b); if (typeof __TRACE !== "undefined" && __TRACE) __log("COU -> " + __u + " bytes=" + __n + " stk=" + __stk2()); return __u; } catch (e) { return "blob:https://unknown/" + __blobUuid(); } };
  URL.revokeObjectURL = function () {};
  return URL;
})());
window.URLSearchParams = function (q) {
  this.__pairs = [];
  var s = String(q || "");
  if (s[0] === "?") s = s.slice(1);
  var self = this;
  if (s) {
    s.split("&").forEach(function (kv) {
      if (!kv) return;
      var i = kv.indexOf("=");
      var k = i < 0 ? kv : kv.slice(0, i);
      var v = i < 0 ? "" : kv.slice(i + 1);
      self.__pairs.push([decodeURIComponent(k), decodeURIComponent(v)]);
    });
  }
  this.get = function (k) { for (var i = 0; i < this.__pairs.length; i++) if (this.__pairs[i][0] === k) return this.__pairs[i][1]; return null; };
  this.getAll = function (k) { return this.__pairs.filter(function (p) { return p[0] === k; }).map(function (p) { return p[1]; }); };
  this.has = function (k) { return this.get(k) !== null; };
  this.forEach = function (cb, thisArg) { for (var i = 0; i < this.__pairs.length; i++) cb.call(thisArg, this.__pairs[i][1], this.__pairs[i][0], this); };
  this.append = function (k, v) { this.__pairs.push([String(k), String(v)]); };
  this.set = function (k, v) { this.delete(k); this.append(k, v); };
  this.delete = function (k) { this.__pairs = this.__pairs.filter(function (p) { return p[0] !== k; }); };
  this.toString = function () { return this.__pairs.map(function (p) { return encodeURIComponent(p[0]) + "=" + encodeURIComponent(p[1]); }).join("&"); };
};

// ───────────────────── form submission → wire navigation ─────────────
function resolveUrl(base, rel) {
  rel = String(rel == null ? "" : rel).trim();
  if (!rel) return base;
  if (/^https?:\/\//i.test(rel)) return rel;
  var m = base.match(/^(https?:\/\/[^\/]+)(\/[^?#]*)?/);
  var origin = m ? m[1] : "https://example.com";
  if (rel[0] === "/") return origin + rel;
  var path = m && m[2] ? m[2] : "/";
  var dir = path.slice(0, path.lastIndexOf("/") + 1);
  return origin + dir + rel;
}
function __formSubmit(form) {
  var method = String(__mr(form, "attrs").method || form.method || "get").toUpperCase();
  try {
    if (form.onsubmit) {
      var r = form.onsubmit({ target: form, preventDefault: function () {}, stopPropagation: function () {} });
      if (r === false) return;
    }
  } catch (e) { __log("onsubmit: " + e); try { if (typeof __fireError === "function") __fireError(e); } catch (_fe) {} }
  form.dispatchEvent({ type: "submit", target: form, preventDefault: function () {} });
  var pairs = [];
  var ctrls = form.querySelectorAll("input,select,textarea,button");
  for (var i = 0; i < ctrls.length; i++) {
    var c = ctrls[i];
    var nm = c.name !== undefined && c.name !== "" ? c.name : __mr(c, "attrs").name;
    if (nm === undefined || nm === "") continue;
    var v = c.value !== undefined ? c.value : (__mr(c, "attrs").value || "");
    pairs.push(encodeURIComponent(nm) + "=" + encodeURIComponent(v));
  }
  var q = pairs.join("&");
  var target = resolveUrl(PAGE_URL, __mr(form, "attrs").action || "");
  var url = target, body = "", extra = [["Referer", PAGE_URL]];
  if (method === "POST") {
    body = q;
    extra.push(["Content-Type", "application/x-www-form-urlencoded"]);
  } else {
    url = target + (target.indexOf("?") >= 0 ? "&" : "?") + q;
  }
  var r = __wireCall(method, url, "document", JSON.stringify(extra), body);
  try {
    var j = JSON.parse(r);
    if (j.ok) {
      __DCI.t.navHtml = j.body;
      __DCI.t.navUrl = j.url;
      __DCI.t.navStatus = j.status;
      // a real browser tears the old document down on form navigation:
      // timers stop, queued scripts never run (same as __nav's contract).
      try { __timers.length = 0; } catch (e) {}
      try { __rafQ.length = 0; } catch (e) {}
    } else {
      __log("form submit failed: " + (j.error || "unknown"));
    }
  } catch (e) { __log("form submit parse: " + e); }
}

// keep handler for the engine
__secrets = undefined;

// ── LT capability set (challenge battery: Blob/Worker/ReadableStream.pipeTo/BigInt/crypto.getRandomValues/performance.getEntries/PerformanceObserver) ──
(function () {
  // Blob: rebuilt as a real constructible class in the ST-7 §12.1b block
  // below (remainder 4, 2026-10-19).
  if (typeof globalThis.Worker === "undefined") {
    var __DC_Worker = function Worker(url) { this.url = String(url); this.onmessage = null; try{ if(typeof __EBBS==='function'&&(globalThis.__wkb=(globalThis.__wkb||0)+1)<=40) __EBBS('WK-'+String(url).slice(0,60)); }catch(e){} try { if (typeof __TRACE !== "undefined" && __TRACE) { __log("WK-NEW url=" + String(url).slice(0,90)); var __bsrc = __DCI.t.couBlobs && __DCI.t.couBlobs[String(url)]; if (__bsrc != null) { if (__dumpFile) __dumpFile("wk-src-" + ((globalThis.__WKN = (globalThis.__WKN || 0) + 1)), String(__bsrc)); __log("WK-SRC len=" + String(__bsrc).length + " head=" + JSON.stringify(String(__bsrc).slice(0, 200))); } } } catch (e) {} };
    __DC_Worker.prototype.postMessage = function (d) { try{ if(typeof __EBBS==='function'&&(globalThis.__wkp=(globalThis.__wkp||0)+1)<=40) __EBBS('WKPM-'+String(d).slice(0,40)); }catch(e){} try { if (typeof __TRACE !== "undefined" && __TRACE && (globalThis.__WKPM = (globalThis.__WKPM || 0) + 1) <= 60) __log("WKPM len=" + String(d).length + " head=" + JSON.stringify(String(d).slice(0, 120))); } catch (e) {} };
    __DC_Worker.prototype.terminate = function () {};
    __DC_Worker.prototype.addEventListener = function () {};
    __DC_Worker.prototype.removeEventListener = function () {};
    globalThis.Worker = __DC_Worker;
    window.Worker = __DC_Worker;
  }
  if (typeof globalThis.ReadableStream === "undefined") {
    var __DC_RS = function ReadableStream() { __hide(this, "locked", false); };
    __DC_RS.prototype.pipeTo = function () { return Promise.resolve(); };
    __DC_RS.prototype.cancel = function () { return Promise.resolve(); };
    __DC_RS.prototype.tee = function () { return [new __DC_RS(), new __DC_RS()]; };
    __DC_RS.prototype.getReader = function () {
      return {
        read: function () { return Promise.resolve({ done: true, value: undefined }); },
        cancel: function () { return Promise.resolve(); },
        releaseLock: function () {},
      };
    };
    try { Object.defineProperty(__DC_RS.prototype, Symbol.toStringTag, { value: "ReadableStream", configurable: true }); } catch (e) {}
    globalThis.ReadableStream = __DC_RS;
    window.ReadableStream = __DC_RS;
  } else if (globalThis.ReadableStream.prototype && globalThis.ReadableStream.prototype.pipeTo === undefined) {
    globalThis.ReadableStream.prototype.pipeTo = function () { return Promise.resolve(); };
  }
  try {
    if (globalThis.URL) {
      if (typeof globalThis.URL.createObjectURL !== "function") {
        globalThis.URL.createObjectURL = function () { return "blob:x/__dc-" + ((Math.random() * 1e18) | 0); };
      }
      if (typeof globalThis.URL.revokeObjectURL !== "function") {
        globalThis.URL.revokeObjectURL = function () {};
      }
    }
  } catch (e) {}
  try { if (typeof globalThis.BigInt === "undefined" && typeof BigInt !== "undefined") globalThis.BigInt = BigInt; } catch (e) {}
  try { if (window.navigator && window.navigator.cookieEnabled === undefined) __hide(window.navigator, "_rh_cookieEnabled", true); } catch (e) {}
  if (__TRACE) {
    try {
      __log("CAPS rs=" + (typeof globalThis.ReadableStream) +
        " rsp=" + (globalThis.ReadableStream && globalThis.ReadableStream.prototype ? typeof globalThis.ReadableStream.prototype.pipeTo : "n/a") +
        " big=" + (typeof globalThis.BigInt) +
        " grv=" + (typeof (globalThis.crypto && globalThis.crypto.getRandomValues)) +
        " ge=" + (typeof (globalThis.performance && globalThis.performance.getEntries)) +
        " po=" + (typeof globalThis.PerformanceObserver) +
        " ce=" + (window.navigator ? String(window.navigator.cookieEnabled) : "no-nav") +
        " blob=" + (typeof globalThis.Blob) +
        " co=" + (typeof (globalThis.URL && globalThis.URL.createObjectURL)) +
        " rev=" + (typeof (globalThis.URL && globalThis.URL.revokeObjectURL)) +
        " wk=" + (typeof globalThis.Worker));
    } catch (e) { __log("CAPS ERR " + e); }
  }
})();

// ── navigator.gpu (WebGPU — desktop Chrome exposes it; CF probes it) ──
(function () {
  try {
    if (!window.navigator || window.navigator.gpu !== undefined) return;
    var gpu = {};
    gpu.requestAdapter = function () { return Promise.resolve(null); };
    gpu.getPreferredCanvasFormat = function () { return "bgra8unorm"; };
    var feats = ["readonly_and_readwrite_storage_textures", "packed_4x8_integer_dot_product", "unrestricted_pointer_parameters", "pointer_composite_access"];
    var lang = {};
    lang.has = function (f) { return feats.indexOf(String(f)) !== -1; };
    Object.defineProperty(lang, "size", { get: function () { return feats.length; }, configurable: true });
    lang[Symbol.iterator] = function () { var i = 0; return { next: function () { return i < feats.length ? { value: feats[i++], done: false } : { value: undefined, done: true }; } }; };
    gpu.wgslLanguageFeatures = lang;
    try { Object.defineProperty(gpu, Symbol.toStringTag, { value: "GPU" }); } catch (e) {}
    Object.defineProperty(window.navigator, "gpu", { get: function () { return gpu; }, enumerable: true, configurable: true });
  } catch (e) {}
})();

// ── WebRTC (RTCPeerConnection — the challenge runs a local RTC session) ──
(function () {
  try {
    if (typeof window.RTCPeerConnection !== "undefined") return;

    function rtcTag(o, t) { try { Object.defineProperty(o, Symbol.toStringTag, { value: t }); } catch (e) {} }
    function rtcHex(n) { var s = ""; while (s.length < n) s += ((Math.random() * 16) | 0).toString(16); return s.slice(0, n); }
    function rtcDigits(n) { var s = ""; while (s.length < n) s += ((Math.random() * 10) | 0); return s.slice(0, n); }
    function rtcEv(type, extra) { var e = { type: type, target: null, currentTarget: null, bubbles: false, cancelable: false }; if (extra) { for (var k in extra) e[k] = extra[k]; } return e; }
    function rtcTarget(o) {
      __ns(o, "__l", {});
      o.addEventListener = function (t, f) {
        try{ if(typeof __EBBS==='function'&&(globalThis.__rae=(globalThis.__rae||0)+1)<=120) __EBBS('RTC-AE '+String(t)); }catch(__ra){} if (typeof f === "function") (__mr(this, "__l")[String(t)] = __mr(this, "__l")[String(t)] || []).push(f); };
      o.removeEventListener = function (t, f) { var a = __mr(this, "__l")[String(t)] || []; var i = a.indexOf(f); if (i >= 0) a.splice(i, 1); };
      o.dispatchEvent = function (e) {
        if (!e || !e.type) return true;
        if (!e.target) __evSet(e, "target", this);
      try{ if(typeof __EBBS==='function'&&(globalThis.__rtd=(globalThis.__rtd||0)+1)<=80) __EBBS('RTD-'+String(e.type)+' n='+((__mr(this, "__l")&&__mr(this, "__l")[e.type]&&__mr(this, "__l")[e.type].length)||0)+(typeof this['on'+e.type]==='function'?'+on':'')+' igs='+String(this.iceGatheringState)+' ics='+String(this.iceConnectionState)+' ss='+String(this.signalingState)); }catch(_x){}
        __evSet(e, "currentTarget", this);
        var hs = [];
        var h = this["on" + e.type];
        if (typeof h === "function") hs.push(h);
        hs = hs.concat((__mr(this, "__l")[e.type] || []).slice());
        for (var i = 0; i < hs.length; i++) { try { hs[i].call(this, e); } catch (err) { try { if (typeof __fireError === "function") __fireError(err); } catch (_fe) {} } }
        return true;
      };
      return o;
    }
    function __rtcB(s) { try { if (typeof __EBBS === "function") __EBBS(s); } catch (e) {} }

    function RTCSessionDescription(init) {
      if (!init) init = {};
      this.type = init.type == null ? null : String(init.type);
      this.sdp = init.sdp == null ? null : String(init.sdp);
      rtcTag(this, "RTCSessionDescription");
    }
    RTCSessionDescription.prototype.toJSON = function () { return { type: this.type, sdp: this.sdp }; };

    function RTCIceCandidate(init) {
      if (!init) init = {};
      __hide(this, "candidate", init.candidate == null ? "" : String(init.candidate));
      __hide(this, "sdpMid", init.sdpMid == null ? null : String(init.sdpMid));
      __hide(this, "sdpMLineIndex", init.sdpMLineIndex == null ? null : Number(init.sdpMLineIndex));
      __hide(this, "usernameFragment", init.usernameFragment == null ? null : String(init.usernameFragment));
      var m = /candidate:(\S+) (\d+) (\S+) (\d+) (\S+) (\d+) typ (\S+)/.exec(this.candidate);
      __hide(this, "foundation", m ? m[1] : ""); __hide(this, "component", m ? m[2] : "");
      __hide(this, "protocol", m ? m[3] : ""); __hide(this, "priority", m ? Number(m[4]) : null);
      __hide(this, "address", m ? m[5] : ""); __hide(this, "port", m ? Number(m[6]) : null); __hide(this, "type", m ? m[7] : "");
      rtcTag(this, "RTCIceCandidate");
    }
    RTCIceCandidate.prototype.toJSON = function () {
      return { candidate: this.candidate, sdpMid: this.sdpMid, sdpMLineIndex: this.sdpMLineIndex, usernameFragment: this.usernameFragment };
    };

    function RTCDataChannel(label, opts) {
      rtcTarget(this);
      __hide(this, "label", String(label == null ? "" : label));
      __hide(this, "ordered", !(opts && opts.ordered === false));
      __hide(this, "maxRetransmits", opts && opts.maxRetransmits != null ? Number(opts.maxRetransmits) : null);
      __hide(this, "maxPacketLifeTime", opts && opts.maxPacketLifeTime != null ? Number(opts.maxPacketLifeTime) : null);
      __hide(this, "negotiated", !!(opts && opts.negotiated));
      __hide(this, "id", null);
      __hide(this, "protocol", (opts && opts.protocol) ? String(opts.protocol) : "");
      __hide(this, "readyState", "connecting");
      __hide(this, "bufferedAmount", 0);
      this.bufferedAmountLowThreshold = 0;
      this.binaryType = "blob";
      rtcTag(this, "RTCDataChannel");
    }
    RTCDataChannel.prototype.send = function () {};
    RTCDataChannel.prototype.close = function () {
      if (this.readyState === "closed") return;
      __hide(this, "readyState", "closed");
      try { this.dispatchEvent(rtcEv("close")); } catch (e) {}
    };

    function __sdpOffer() {
      var fp = []; while (fp.length < 32) fp.push(rtcHex(2).toUpperCase());
      return ["v=0",
        "o=- " + rtcDigits(16) + " 2 IN IP4 127.0.0.1",
        "s=-",
        "t=0 0",
        "a=group:BUNDLE 0",
        "a=extmap-allow-mixed",
        "a=msid-semantic: WMS",
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
        "c=IN IP4 0.0.0.0",
        "a=ice-ufrag:" + rtcHex(4),
        "a=ice-pwd:" + rtcHex(24),
        "a=ice-options:trickle",
        "a=fingerprint:sha-256 " + fp.join(":"),
        "a=setup:actpass",
        "a=mid:0",
        "a=sctp-port:5000",
        "a=max-message-size:262144"
      ].join("\r\n") + "\r\n";
    }

    function RTCPeerConnection(cfg) {
      if (new.target === undefined) throw new TypeError("Failed to construct 'RTCPeerConnection': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      rtcTarget(this);
      this.__cfg = (cfg && typeof cfg === "object") ? cfg : {};
      this.onicecandidate = null; this.onicecandidateerror = null; this.onicegatheringstatechange = null;
      this.oniceconnectionstatechange = null; this.onconnectionstatechange = null; this.onsignalingstatechange = null;
      this.ondatachannel = null; this.onnegotiationneeded = null; this.ontrack = null;
      this.__igs = "new"; this.__ics = "new"; this.__cs = "new"; this.__ss = "stable";
      __hide(this, "localDescription", null); __hide(this, "remoteDescription", null);
      __hide(this, "pendingLocalDescription", null); __hide(this, "pendingRemoteDescription", null);
      __hide(this, "currentLocalDescription", null); __hide(this, "currentRemoteDescription", null);
      this.__trickle = null;
      rtcTag(this, "RTCPeerConnection");
    }
    Object.defineProperties(RTCPeerConnection.prototype, {
      iceGatheringState: { get: function () { return this.__igs; }, configurable: true },
      iceConnectionState: { get: function () { return this.__ics; }, configurable: true },
      connectionState: { get: function () { return this.__cs; }, configurable: true },
      signalingState: { get: function () { return this.__ss; }, configurable: true },
      canTrickleIceCandidates: { get: function () { return this.__trickle; }, configurable: true },
    });
    RTCPeerConnection.prototype.createDataChannel = function (label, opts) { return new RTCDataChannel(label, opts); };

    RTCPeerConnection.prototype.createOffer = function () {
      return new Promise(function (res) { setTimeout(function () { res(new RTCSessionDescription({ type: "offer", sdp: __sdpOffer() })); }, 1); });
    };
    RTCPeerConnection.prototype.createAnswer = function () {
      return new Promise(function (res) { setTimeout(function () { res(new RTCSessionDescription({ type: "answer", sdp: __sdpOffer() })); }, 1); });
    };
    RTCPeerConnection.prototype.__gather = function () {
      var self = this;
      setTimeout(function () {
        if (self.__cs === "closed") return;
        self.__igs = "gathering";
        __rtcB("RTC GATHER");
        try { self.dispatchEvent(rtcEv("icegatheringstatechange")); } catch (e) {}
        setTimeout(function () {
          if (self.__cs === "closed") return;
          __rtcB("RTC CAND");
          try { self.dispatchEvent(rtcEv("icecandidate", { candidate: new RTCIceCandidate({ candidate: "candidate:1 1 udp 2122260223 192.168.1.20 " + (49152 + ((Math.random() * 16000) | 0)) + " typ host generation 0 ufrag " + rtcHex(4) + " network-cost 999", sdpMid: "0", sdpMLineIndex: 0, usernameFragment: rtcHex(4) }) })); } catch (e) {}
          setTimeout(function () {
            if (self.__cs === "closed") return;
            self.__igs = "complete";
            __rtcB("RTC COMPLETE");
            try { self.dispatchEvent(rtcEv("icegatheringstatechange")); } catch (e) {}
            try { self.dispatchEvent(rtcEv("icecandidate", { candidate: null })); } catch (e) {}
          }, 5);
        }, 5);
      }, 5);
    };
    RTCPeerConnection.prototype.setLocalDescription = function (d) {
      var self = this;
      return new Promise(function (res) {
        var desc = d ? { type: (d.type == null ? null : String(d.type)), sdp: (d.sdp == null ? "" : String(d.sdp)) } : null;
        __hide(self, "localDescription", desc); __hide(self, "pendingLocalDescription", desc); __hide(self, "currentLocalDescription", desc);
        self.__ss = desc && desc.type === "answer" ? "stable" : "have-local-offer";
        try { self.dispatchEvent(rtcEv("signalingstatechange")); } catch (e) {}
        res();
        self.__gather();
      });
    };
    RTCPeerConnection.prototype.setRemoteDescription = function (d) {
      var self = this;
      return new Promise(function (res) {
        var desc = d ? { type: (d.type == null ? null : String(d.type)), sdp: (d.sdp == null ? "" : String(d.sdp)) } : null;
        __hide(self, "remoteDescription", desc); __hide(self, "pendingRemoteDescription", desc); __hide(self, "currentRemoteDescription", desc);
        self.__ss = "stable";
        try { self.dispatchEvent(rtcEv("signalingstatechange")); } catch (e) {}
        res();
      });
    };
    RTCPeerConnection.prototype.addIceCandidate = function () { return Promise.resolve(); };
    RTCPeerConnection.prototype.getStats = function () { return Promise.resolve(new Map()); };
    RTCPeerConnection.prototype.getConfiguration = function () {
      return { iceServers: (this.__cfg && this.__cfg.iceServers) || [], bundlePolicy: "balanced", rtcpMuxPolicy: "require", iceCandidatePoolSize: 0, iceTransportPolicy: "all" };
    };
    RTCPeerConnection.prototype.setConfiguration = function (c) { this.__cfg = c || {}; };
    RTCPeerConnection.prototype.close = function () {
      if (this.__cs === "closed") return;
      this.__cs = "closed"; this.__ics = "closed"; this.__ss = "closed";
      __rtcB("RTC CLOSE");
      try { this.dispatchEvent(rtcEv("iceconnectionstatechange")); } catch (e) {}
      try { this.dispatchEvent(rtcEv("connectionstatechange")); } catch (e) {}
    };
    RTCPeerConnection.prototype.addTransceiver = function () { return {}; };
    RTCPeerConnection.prototype.getSenders = function () { return []; };
    RTCPeerConnection.prototype.getReceivers = function () { return []; };
    RTCPeerConnection.prototype.restartIce = function () {};

    var G = (typeof globalThis !== "undefined") ? globalThis : window;
    try { if (G.EventTarget && G.EventTarget.prototype) Object.setPrototypeOf(RTCPeerConnection.prototype, G.EventTarget.prototype); } catch (e) {}
    G.RTCPeerConnection = RTCPeerConnection; window.RTCPeerConnection = RTCPeerConnection;
    G.RTCDataChannel = RTCDataChannel; window.RTCDataChannel = RTCDataChannel;
    G.RTCSessionDescription = RTCSessionDescription; window.RTCSessionDescription = RTCSessionDescription;
    G.RTCIceCandidate = RTCIceCandidate; window.RTCIceCandidate = RTCIceCandidate;
    __rtcB("RTC INSTALLED");
  } catch (e) {
    try { __log("RTC INSTALL ERR " + e); } catch (e2) {}
  }
})();

// ── v5.5 fidelity tail: mediaDevices + idle callbacks ──
(function () {
  try {
    var nav = window.navigator;
    if (nav && !nav.mediaDevices) {
      // Chrome-truth (oracle): with no granted permission enumerateDevices
      // lists one entry per kind — audioinput, videoinput, audiooutput — with
      // empty label and empty ids (the values are permission-gated).
      var devices = [
        { deviceId: "", groupId: "", kind: "audioinput", label: "" },
        { deviceId: "", groupId: "", kind: "videoinput", label: "" },
        { deviceId: "", groupId: "", kind: "audiooutput", label: "" },
      ];
      var md = {
        enumerateDevices: function () { return Promise.resolve(devices.map(function (d) { return { deviceId: d.deviceId, groupId: d.groupId, kind: d.kind, label: d.label, toJSON: function () { return this; } }; })); },
        getUserMedia: function () { return Promise.reject(new TypeError("Permission denied")); },
        getDisplayMedia: function () { return Promise.reject(new TypeError("Permission denied")); },
        addEventListener: function () {}, removeEventListener: function () {}, ondevicechange: null,
      };
      try { Object.defineProperty(md, Symbol.toStringTag, { value: "MediaDevices" }); } catch (e) {}
      __hide(md, "_s7b", "MediaDevices");
      try { Object.defineProperty(nav, "mediaDevices", { get: function () { return md; }, configurable: true }); } catch (e) {}
    }
  } catch (e) {}
  try {
    if (!window.requestIdleCallback) window.requestIdleCallback = function (f) { return setTimeout(function () { f({ didTimeout: false, timeRemaining: function () { return 50; } }); }, 1); };
    if (!window.cancelIdleCallback) window.cancelIdleCallback = function (id) { clearTimeout(id); };
  } catch (e) {}
})();

// ── v5.8: HTMLMediaElement.canPlayType — Chrome-truth codec table (measured
// against the installed Chromium). A detector asking "can you play h264?"
// must hear the same answer the real browser gives. ──
(function () {
  function canPlay(t) {
    t = String(t == null ? "" : t).toLowerCase();
    var semi = t.indexOf(";");
    var type = (semi >= 0 ? t.slice(0, semi) : t).trim();
    var tail = semi >= 0 ? t.slice(semi) : "";
    var empty = !/codecs\s*=/i.test(t); // no codecs specified → container-only answer
    function has() { for (var i = 0; i < arguments.length; i++) { if (tail.indexOf(arguments[i]) >= 0) return true; } return false; }
    if (type === "audio/ogg") return has("vorbis", "opus", "flac") ? "probably" : (empty ? "maybe" : "");
    if (type === "audio/mpeg" || type === "audio/mp3") return "probably";
    if (type === "audio/wav" || type === "audio/x-wav" || type === "audio/wave") return "probably";
    if (type === "audio/aac") return "probably";
    if (type === "audio/flac" || type === "audio/x-flac") return "probably";
    if (type === "audio/x-m4a" || type === "audio/m4a" || type === "audio/mp4") return has("mp4a", "aac", "alac", "opus", "flac") ? "probably" : (empty ? "maybe" : "");
    if (type === "audio/webm") return has("opus", "vorbis") ? "probably" : (empty ? "maybe" : "");
    if (type === "video/mp4") return has("avc1", "avc3", "hvc1", "hev1", "av01", "mp4a", "avc") ? "probably" : (empty ? "maybe" : "");
    if (type === "video/webm") return has("vp8", "vp9", "av01", "opus", "vorbis") ? "probably" : (empty ? "maybe" : "");
    if (type === "video/x-matroska") return "maybe";
    return "";
  }
  try { tagCtor("audio").prototype.canPlayType = canPlay; } catch (e) {}
  try { tagCtor("video").prototype.canPlayType = canPlay; } catch (e) {}
})();

// ── v5.8: DOM interface constructor bank (probe-walk fidelity: window.Document, Element, etc.) ──
(function () {
  var g = globalThis;
  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }
  function mk(name, parent) {
    var ex = g[name];
    if (typeof ex === "undefined") {
      ex = function () {
        if (new.target !== undefined) throw new TypeError("Failed to construct '" + name + "': Illegal constructor");
        throw new TypeError("Illegal constructor");
      };
      try { Object.defineProperty(ex, "name", { value: name, configurable: true }); } catch (e) {}
      try { g[name] = ex; window[name] = ex; } catch (e) {}
    } else if (typeof ex === "function" && !ex.name) {
      // an interface that already existed anonymous (e.g. the context stubs)
      // still gets its real constructor name
      try { Object.defineProperty(ex, "name", { value: name, configurable: true }); } catch (e) {}
    }
    if (parent) { var pp = g[parent]; if (pp && pp.prototype) setP(ex.prototype, pp.prototype); }
    try { Object.defineProperty(ex.prototype, "constructor", { value: ex, writable: true, configurable: true }); } catch (e) {}
    return ex;
  }
  var bank = [
    ["EventTarget"], ["Node", "EventTarget"], ["Element", "Node"], ["HTMLElement", "Element"],
    ["Document", "Node"], ["HTMLDocument", "Document"], ["XMLDocument", "Document"],
    ["DocumentFragment", "Node"], ["ShadowRoot", "DocumentFragment"],
    ["CharacterData", "Node"], ["Text", "CharacterData"], ["Comment", "CharacterData"], ["CDATASection", "Text"], ["ProcessingInstruction", "CharacterData"],
    ["Attr", "Node"], ["NamedNodeMap"], ["NodeList"], ["HTMLCollection"], ["DOMTokenList"], ["DOMStringMap"], ["DOMImplementation"],
    ["Range"], ["Selection"], ["TreeWalker"], ["NodeIterator"],
    ["CSSStyleDeclaration"], ["CSSRule"], ["CSSStyleRule"], ["CSSStyleSheet"], ["StyleSheet"], ["StyleSheetList"], ["MediaList"],
    ["DOMRect"], ["DOMRectReadOnly"],
    ["Window", "EventTarget"], ["Navigator"], ["Location"], ["Screen"], ["History"], ["BarProp"],
    ["Performance", "EventTarget"], ["PerformanceTiming"], ["PerformanceNavigation"], ["Storage"],
    ["Event"], ["UIEvent", "Event"], ["MouseEvent", "UIEvent"], ["KeyboardEvent", "UIEvent"], ["FocusEvent", "UIEvent"], ["InputEvent", "UIEvent"], ["WheelEvent", "MouseEvent"], ["PointerEvent", "MouseEvent"], ["TouchEvent", "UIEvent"], ["MessageEvent", "Event"], ["ErrorEvent", "Event"], ["ProgressEvent", "Event"], ["PromiseRejectionEvent", "Event"], ["CloseEvent", "Event"], ["StorageEvent", "Event"], ["HashChangeEvent", "Event"], ["PopStateEvent", "Event"], ["PageTransitionEvent", "Event"], ["BeforeUnloadEvent", "Event"], ["DragEvent", "MouseEvent"], ["CompositionEvent", "UIEvent"], ["AnimationEvent", "Event"], ["TransitionEvent", "Event"], ["ClipboardEvent", "Event"],
    ["HTMLHtmlElement", "HTMLElement"], ["HTMLHeadElement", "HTMLElement"], ["HTMLBodyElement", "HTMLElement"], ["HTMLDivElement", "HTMLElement"], ["HTMLSpanElement", "HTMLElement"], ["HTMLAnchorElement", "HTMLElement"], ["HTMLAreaElement", "HTMLElement"], ["HTMLScriptElement", "HTMLElement"], ["HTMLStyleElement", "HTMLElement"], ["HTMLLinkElement", "HTMLElement"], ["HTMLMetaElement", "HTMLElement"], ["HTMLTitleElement", "HTMLElement"], ["HTMLIFrameElement", "HTMLElement"], ["HTMLCanvasElement", "HTMLElement"], ["HTMLImageElement", "HTMLElement"], ["HTMLInputElement", "HTMLElement"], ["HTMLButtonElement", "HTMLElement"], ["HTMLFormElement", "HTMLElement"], ["HTMLSelectElement", "HTMLElement"], ["HTMLOptionElement", "HTMLElement"], ["HTMLTextAreaElement", "HTMLElement"], ["HTMLTemplateElement", "HTMLElement"], ["HTMLUnknownElement", "HTMLElement"], ["HTMLVideoElement", "HTMLElement"], ["HTMLAudioElement", "HTMLElement"], ["HTMLMediaElement", "HTMLElement"], ["HTMLSourceElement", "HTMLElement"], ["HTMLBRElement", "HTMLElement"], ["HTMLParagraphElement", "HTMLElement"], ["HTMLPreElement", "HTMLElement"], ["HTMLQuoteElement", "HTMLElement"], ["HTMLUListElement", "HTMLElement"], ["HTMLLIElement", "HTMLElement"], ["HTMLOListElement", "HTMLElement"], ["HTMLDListElement", "HTMLElement"], ["HTMLTableElement", "HTMLElement"], ["HTMLTableRowElement", "HTMLElement"], ["HTMLTableCellElement", "HTMLElement"], ["HTMLHeadingElement", "HTMLElement"], ["HTMLHRElement", "HTMLElement"], ["HTMLModElement", "HTMLElement"], ["HTMLFieldSetElement", "HTMLElement"], ["HTMLLabelElement", "HTMLElement"], ["HTMLOutputElement", "HTMLElement"], ["HTMLProgressElement", "HTMLElement"], ["HTMLMeterElement", "HTMLElement"], ["HTMLDetailsElement", "HTMLElement"], ["HTMLSlotElement", "HTMLElement"],
    ["ImageData"], ["File"], ["FileList"], ["FileReader"], ["FormData"], ["URLSearchParams"], ["DOMException"], ["DOMParser"], ["XMLSerializer"],
    ["BroadcastChannel"], ["WebSocket"],
    ["FontFace"], ["FontFaceSet"], ["MediaQueryList"], ["MediaQueryListEvent"],
    ["MutationObserver"], ["IntersectionObserver"], ["ResizeObserver"], ["ReportingObserver"], ["Notification"], ["Clipboard"], ["Geolocation"],
    ["Headers"], ["Request"], ["Response"],
    ["MediaStream"], ["MediaStreamTrack"], ["MediaDeviceInfo"], ["RTCCertificate"], ["RTCRtpSender"], ["RTCRtpReceiver"], ["RTCRtpTransceiver"], ["RTCDTMFSender"],
    ["TextMetrics"], ["Path2D"], ["CanvasRenderingContext2D"], ["WebGLRenderingContext"], ["WebGL2RenderingContext"],
    ["AudioContext"], ["OfflineAudioContext"], ["AudioBuffer"], ["AudioNode"], ["AudioScheduledSourceNode"], ["BaseAudioContext"], ["AudioParam"], ["AudioListener"], ["AudioDestinationNode"], ["AnalyserNode"], ["BiquadFilterNode"], ["DelayNode"], ["DynamicsCompressorNode"], ["GainNode"], ["OscillatorNode"], ["PannerNode"], ["ConvolverNode"], ["StereoPannerNode"], ["ConstantSourceNode"], ["WaveShaperNode"], ["PeriodicWave"], ["ScriptProcessorNode"],
    ["Crypto"], ["SubtleCrypto"], ["CryptoKey"],
    ["SpeechSynthesis"], ["SpeechSynthesisUtterance"], ["SpeechSynthesisVoice"]
  ];
  var n = 0;
  for (var i = 0; i < bank.length; i++) { mk(bank[i][0], bank[i][1] || null); n++; }
  // element instanceof fidelity: the bank's HTML*Element names that map to a
  // real tag become the WORKING tag constructors — document.createElement
  // ('script') instanceof HTMLScriptElement must be true (the turnstile
  // loader's script-tag scan depends on it).
  var __tagOf = { html: "html", head: "head", body: "body", div: "div", span: "span",
    a: "a", area: "area", script: "script", style: "style", link: "link", meta: "meta",
    title: "title", iframe: "iframe", canvas: "canvas", img: "img", input: "input",
    button: "button", form: "form", select: "select", option: "option",
    textarea: "textarea", template: "template", video: "video", audio: "audio",
    source: "source", br: "br", p: "p", pre: "pre", blockquote: "blockquote",
    ul: "ul", li: "li", ol: "ol", dl: "dl", table: "table", tr: "tr",
    th: "th", h1: "h1", h2: "h2", h3: "h3", h4: "h4", h5: "h5", h6: "h6",
    label: "label", fieldset: "fieldset", hr: "hr", slot: "slot", details: "details",
    meter: "meter", progress: "progress", output: "output", map: "map",
    object: "object", embed: "embed", track: "track", picture: "picture" };
  for (var hi = 0; hi < bank.length; hi++) {
    var hn = bank[hi][0];
    var hm = /^HTML([A-Za-z]+)Element$/.exec(hn);
    if (!hm) continue;
    var htag = hm[1].toLowerCase();
    var real = __tagOf[htag];
    if (!real) continue;
    try {
      var C = tagCtor(real);
      g[hn] = C; window[hn] = C;
    } catch (e) {}
  }
  try { if (g.ShadowRoot && g.ShadowRoot.prototype && g.DocumentFragment && g.DocumentFragment.prototype) setP(g.ShadowRoot.prototype, g.DocumentFragment.prototype); } catch (e) {}
  try { if (g.document && g.HTMLDocument) setP(g.document, g.HTMLDocument.prototype); } catch (e) {}
try { __mk(document); } catch (e) {}
  try { if (g.window && g.Window) setP(g.window, g.Window.prototype); } catch (e) {}
  // WH (a, 2026-10-19): Chrome's window chain is window -> Window.prototype
  // ("Window" tag) -> WindowProperties.prototype -> EventTarget.prototype
  // (measured plc, Chrome 154). Engine had Window.prototype -> EventTarget
  // directly and the tag own on the window itself.
  try { if (g.Window && g.Window.prototype) { try { Object.defineProperty(g.Window.prototype, Symbol.toStringTag, { value: "Window", configurable: true }); } catch (e0) {} } } catch (e) {}
  try {
    if (g.Window && g.Window.prototype && g.EventTarget && g.EventTarget.prototype && Object.getPrototypeOf(g.Window.prototype) === g.EventTarget.prototype) {
      var __WPm = Object.create(g.EventTarget.prototype);
      try { Object.defineProperty(__WPm, Symbol.toStringTag, { value: "WindowProperties", configurable: true }); } catch (e1) {}
      setP(g.Window.prototype, __WPm);
    }
  } catch (e) {}
  try { if (g.navigator && g.Navigator) setP(g.navigator, g.Navigator.prototype); } catch (e) {}
  // webdriver lives on Navigator.prototype in Chrome (a getter returning
  // false), NOT as an own data property: sannysoft fails the own-prop check
  // (_.has(navigator, "webdriver")) even when the value is false.
  try { if (g.Navigator && g.Navigator.prototype) Object.defineProperty(g.Navigator.prototype, "webdriver", { get: __native("get webdriver", 0, function () { if (!(this && typeof this.userAgent === "string")) throw new TypeError("Illegal invocation"); return false; }), enumerable: true, configurable: true }); } catch (e) {}
  // Notification statics (Chrome shape: permission gate + requestPermission).
  try {
    if (typeof g.Notification === "function") {
      try { Object.defineProperty(g.Notification, "permission", { get: function () { return "default"; }, configurable: true }); } catch (e) {}
      try { if (typeof g.Notification.requestPermission !== "function") g.Notification.requestPermission = function (cb) { var p = Promise.resolve("default"); if (typeof cb === "function") { try { p.then(cb); } catch (e) {} } return p; }; } catch (e) {}
      try { g.Notification.maxActions = 2; } catch (e) {}
    }
  } catch (e) {}
  try { if (g.location && g.Location) setP(g.location, g.Location.prototype); } catch (e) {}
  try { if (g.screen && g.Screen) setP(g.screen, g.Screen.prototype); } catch (e) {}
  try { if (g.history && g.History) setP(g.history, g.History.prototype); } catch (e) {}
  try { if (g.performance && g.Performance) setP(g.performance, g.Performance.prototype); } catch (e) {}
  try { if (g.localStorage && g.Storage) setP(g.localStorage, g.Storage.prototype); } catch (e) {}
  try { if (g.sessionStorage && g.Storage) setP(g.sessionStorage, g.Storage.prototype); } catch (e) {}
  try {
    var dm = ["createElement","createElementNS","createTextNode","createComment","createDocumentFragment","getElementById","querySelector","querySelectorAll","appendChild","removeChild","insertBefore","replaceChild","contains","hasChildNodes","cloneNode","addEventListener","removeEventListener","dispatchEvent","getElementsByTagName","getElementsByClassName"];
    if (g.document && g.Document) for (var j = 0; j < dm.length; j++) {
      var fn2 = g.document[dm[j]];
      if (typeof fn2 === "function") { try { Object.defineProperty(g.Document.prototype, dm[j], { value: fn2, writable: true, configurable: true }); } catch (e) {} }
    }
  } catch (e) {}
  try { if (typeof __EBBS === "function") __EBBS("IFACES-OK n=" + n); } catch (e) {}
})();

// ── MessageChannel / MessagePort: real channels. recaptcha's module init
// constructs a MessageChannel (task scheduler + RPC carrier) and hands a port
// across postMessage; the fidelity bank's Illegal-constructor stub killed the
// whole script. Chrome shapes measured on the local oracle:
//   MCproto=port1,port2,constructor  MPproto=onmessage,onmessageerror,close,
//   postMessage,start,constructor; no own props on instances; MessagePort is
//   not constructible.
(function () {
  try {
    var PW = new WeakMap(); // port -> {peer,q,started,closed,onmessage,onmessageerror}
    var CW = new WeakMap(); // channel -> {p1,p2}
    function pst(p) { return PW.get(p); }
    function collectPorts(t) {
      var out = [];
      try { if (t && typeof t.length === "number") for (var i = 0; i < t.length; i++) { if (t[i] && pst(t[i])) out.push(t[i]); } } catch (e) {}
      return out;
    }
    function pDeliver(p, data, ports) {
      var s = pst(p); if (!s || s.closed) return;
      var ev = { type: "message", data: data, ports: ports || [], target: p, currentTarget: p, origin: "", source: null, lastEventId: "" };
      __evSet(ev, "isTrusted", true);
      if (typeof s.onmessage === "function") { try { s.onmessage(ev); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_e) {} } }
      try { if (p.dispatchEvent) p.dispatchEvent(ev); } catch (e) { try { if (typeof __fireError === "function") __fireError(e); } catch (_e) {} }
    }
    function pFlush(p) { var s = pst(p); if (!s || !s.started) return; while (s.q.length) { var m = s.q.shift(); pDeliver(p, m.data, m.ports); } }
    function pEnable(p) { var s = pst(p); if (!s || s.started) return; s.started = true; setTimeout(function () { pFlush(p); }, 0); }
    function mkPort() {
      var p = Object.create(MP.prototype);
      PW.set(p, { peer: null, q: [], started: false, closed: false, onmessage: null, onmessageerror: null });
      return p;
    }
    function MessagePort() { throw new TypeError("Failed to construct 'MessagePort': Illegal constructor"); }
    var MP = MessagePort;
    try { delete MP.prototype.constructor; } catch (e) {}
    Object.defineProperty(MP.prototype, "onmessage", { get: function () { var s = pst(this); if (!s) throw new TypeError("Illegal invocation"); return s.onmessage; }, set: function (v) { var s = pst(this); if (!s) throw new TypeError("Illegal invocation"); s.onmessage = typeof v === "function" ? v : null; if (typeof v === "function") pEnable(this); }, configurable: true });
    Object.defineProperty(MP.prototype, "onmessageerror", { get: function () { var s = pst(this); if (!s) throw new TypeError("Illegal invocation"); return s.onmessageerror; }, set: function (v) { var s = pst(this); if (!s) throw new TypeError("Illegal invocation"); s.onmessageerror = typeof v === "function" ? v : null; }, configurable: true });
    Object.defineProperty(MP.prototype, "close", { value: function () { var s = pst(this); if (!s) throw new TypeError("Illegal invocation"); s.closed = true; s.q.length = 0; }, writable: true, configurable: true });
    Object.defineProperty(MP.prototype, "postMessage", { value: function (data) {
      var s = pst(this); if (!s) throw new TypeError("Illegal invocation");
      if (s.closed) return;
      var peer = s.peer, ps = peer && pst(peer);
      if (!ps || ps.closed) return;
      var ports = collectPorts(arguments.length > 1 ? arguments[1] : null);
      setTimeout(function () {
        var ps2 = pst(peer); if (!ps2 || ps2.closed) return;
        var lm = __ETStore ? __ETStore(peer) : null;
        var has = ps2.started || typeof ps2.onmessage === "function" || (lm && lm["message"] && lm["message"].length > 0);
        if (has) { pDeliver(peer, data, ports); } else { ps2.q.push({ data: data, ports: ports }); }
      }, 0);
    }, writable: true, configurable: true });
    Object.defineProperty(MP.prototype, "start", { value: function () { var s = pst(this); if (!s) throw new TypeError("Illegal invocation"); pEnable(this); }, writable: true, configurable: true });
    Object.defineProperty(MP.prototype, "constructor", { value: MP, writable: true, configurable: true });
    try { Object.defineProperty(MP.prototype, Symbol.toStringTag, { value: "MessagePort", configurable: true }); } catch (e) {}
    function MessageChannel() {
      var p1 = mkPort(), p2 = mkPort();
      pst(p1).peer = p2; pst(p2).peer = p1;
      CW.set(this, { p1: p1, p2: p2 });
    }
    var MC = MessageChannel;
    var ET = globalThis.EventTarget;
    try { delete MC.prototype.constructor; } catch (e) {}
    Object.defineProperty(MC.prototype, "port1", { get: function () { var c = CW.get(this); if (!c) throw new TypeError("Illegal invocation"); return c.p1; }, configurable: true });
    Object.defineProperty(MC.prototype, "port2", { get: function () { var c = CW.get(this); if (!c) throw new TypeError("Illegal invocation"); return c.p2; }, configurable: true });
    Object.defineProperty(MC.prototype, "constructor", { value: MC, writable: true, configurable: true });
    try { Object.defineProperty(MC.prototype, Symbol.toStringTag, { value: "MessageChannel", configurable: true }); } catch (e) {}
    // EventTarget trio, inherited (Chrome: MessagePort parents through
    // EventTarget; its prototype is where add/remove/dispatch live). Elements
    // and windows keep their own overrides; this is the inherited fallback.
    try {
      if (ET && ET.prototype) {
        var ETW = new WeakMap();
        function etStore(o) { var m = ETW.get(o); if (!m) { m = {}; ETW.set(o, m); } return m; }
        __ETStore = etStore;
        Object.defineProperty(ET.prototype, "addEventListener", { value: function (t, f) {
          (etStore(this)[t] = etStore(this)[t] || []).push(f);
          if (String(t) === "message") { try { if (pst(this)) pEnable(this); } catch (e) {} }
        }, writable: true, configurable: true });
        Object.defineProperty(ET.prototype, "removeEventListener", { value: function (t, f) { var m = ETW.get(this); if (!m) return; var a = m[t] || []; var i = a.indexOf(f); if (i >= 0) a.splice(i, 1); }, writable: true, configurable: true });
        Object.defineProperty(ET.prototype, "dispatchEvent", { value: function (e) { if (!e || !e.type) return true; var m = ETW.get(this); if (!m) return true; var a = (m[e.type] || []).slice(); for (var i = 0; i < a.length; i++) { try { a[i].call(this, e); } catch (err) { __log("listener " + e.type + ": " + err + " @@ " + String((err && err.stack) || "").split(String.fromCharCode(10)).slice(0, 4).join(" | ").slice(0, 240)); try { if (typeof __fireError === "function") __fireError(err); } catch (_fe) {} } } return true; }, writable: true, configurable: true });
      }
    } catch (e) {}
    try { Object.setPrototypeOf(MP.prototype, ET && ET.prototype ? ET.prototype : Object.prototype); } catch (e) {}
    globalThis.MessageChannel = MC; window.MessageChannel = MC;
    globalThis.MessagePort = MP; window.MessagePort = MP;
    __collectPorts = collectPorts;
  } catch (e) { try { __log("MCHAN INIT ERR " + e); } catch (_e) {} }
})();

// ═══════════════════════════════════════════════════════════════════════
// FIDELITY LAYER — exact-shape surfaces, matched 1:1 against the local
// Chromium oracle (docs in RECEIPTS). Method surfaces are __native-shaped
// (native toString, {length,name} own props, no prototype) BY CONSTRUCTION.
// Values are Chrome's real defaults — nothing fabricated where a default
// exists; geometry is honestly zero because there is no layout engine.
// ═══════════════════════════════════════════════════════════════════════

// hidden backing-field helper: engine bookkeeping must not surface in
// getOwnPropertyNames/keys (Chrome instances show none of it).
var __WMS = __WMS || new WeakMap();
function __hide(o, k, v) { if (__WMS === undefined) __WMS = new WeakMap(); var st = __WMS.get(o); if (!st) { st = {}; __WMS.set(o, st); } st[k] = v; return o; }
function __g(o, k, d) { if (__WMS === undefined) return d; var st = __WMS.get(o); var v = st ? st[k] : undefined; return v === undefined ? d : v; }
function __own(o, k, v) { try { Object.defineProperty(o, k, { value: v, writable: true, enumerable: true, configurable: true }); } catch (e) {} return v; }
// ST-11 U2 (2026-10-21): node state lives in the hidden store, never as own
// props (Chrome nodes own nothing; measured ownprops/keycheck). The shims are
// receiver-safe: an object without the node marker falls through to plain
// property access, so parser records and other plain objects keep their
// semantics. Marker: "\u0001N" in the WeakMap store.
function __mk(o) { __hide(o, "\u0001N", 1); return o; }
function __maron(o) { return __WMS === undefined ? undefined : __WMS.get(o); }
function __mr(o, k, sk) { if (sk === undefined) sk = k; var st = __maron(o); if (st && st["\u0001N"]) { return (sk in st) ? st[sk] : undefined; } return o[k]; }
function __ns(o, k, v, sk) { if (sk === undefined) sk = k; var st = __maron(o); if (st && st["\u0001N"]) { st[sk] = v; return v; } o[k] = v; return v; }
function __nsf(o, k, v) { var st = __maron(o); if (st && st["\u0001N"]) { __hide(o, "_sf_" + k, v); return v; } o[k] = v; return v; }
function __nrh(o, k, v) { var st = __maron(o); if (st && st["\u0001N"]) { __hide(o, "_sf_" + k, v); __hide(o, "_rh_" + k, v); return v; } o[k] = v; return v; }
function __nd(o, k) { var st = __maron(o); if (st && st["\u0001N"]) { delete st[k]; return true; } return delete o[k]; }

function __tag(o, n) { try { Object.defineProperty(o, n, { value: (n === Symbol.toStringTag ? undefined : n), configurable: true }); } catch (e) {} return o; }

// ── element class chain: real Node/Element/HTMLElement levels ─────────
// The engine historically stubbed Node/Element/HTMLElement as ONE shared
// function (no distinct levels). Chrome-truth: HTMLElement>Element>Node>
// EventTarget>Object. Rebuild the chain once, at the base: every tag
// prototype (and therefore every element) inherits through it.
(function () {
  var EVT = globalThis.EventTarget;
  if (!EVT || !EVT.prototype) return;
  function mkCls(name, proto) {
    var C = function () {
      if (new.target !== undefined) throw new TypeError("Failed to construct '" + name + "': Illegal constructor");
      throw new TypeError("Illegal constructor");
    };
    try { Object.defineProperty(C, "name", { value: name, configurable: true }); } catch (e) {}
    C.prototype = Object.create(proto);
    try { Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true }); } catch (e) {}
    // ST-11 U2: per-class string tags are set below; no undefined placeholder.
    return C;
  }
  var NodeC = mkCls("Node", EVT.prototype);
  var ElemC = mkCls("Element", NodeC.prototype);
  var HTMLEl = globalThis.HTMLElement;
  if (HTMLEl && HTMLEl.prototype) {
    try { Object.setPrototypeOf(HTMLEl.prototype, ElemC.prototype); } catch (e) {}
  }
  globalThis.Node = NodeC; window.Node = NodeC;
  globalThis.Element = ElemC; window.Element = ElemC;
  __NodeC = NodeC; __ElemC = ElemC;
})();

// WH (a2): Document.prototype rode the stale bank alias — mk("Document",
// "Node") ran while window.Node was still HTMLElement (alias set earlier), so
// Document.prototype's parent landed on HTMLElement.prototype and survived the
// element-chain rebuild above (measured plcg: doc chain 6 hops vs Chrome 4;
// document instanceof HTMLElement true - Chrome false; document also leaked
// getAttribute/classList/style/innerHTML off the element chain). Chrome:
// HTMLDocument > Document > Node > EventTarget.
try { if (globalThis.Document && globalThis.Document.prototype && globalThis.Node && globalThis.Node.prototype) Object.setPrototypeOf(globalThis.Document.prototype, globalThis.Node.prototype); } catch (e) {}


// ── DOM plumbing: fragment insertion + importNode + Range ─────────────
(function () {
  // deep structural clone (shares the engine's element classes)
  function deepClone(n) {
    if (!n) return null;
    if (n.nodeType === 3) { var t = makeText(n.data !== undefined ? n.data : (n.textContent || "")); __ns(t, "__odoc", __ownerDocOf(n)); return t; }
    if (n.nodeType === 8) return __DCI.nc.comment(n.data !== undefined ? n.data : (n.textContent || ""));
    if (n.nodeType === 10) return __DCI.nc.doctype(n.name, n.publicId, n.systemId);
    if (n.nodeType === 11) {
      var fr = __DCI.nc.fragment(); __hide(fr, "_sf_ownerDocument", __ownerDocOf(n));
      for (var i = 0; i < (n.childNodes || []).length; i++) { var c = deepClone(n.childNodes[i]); __DCI.nc.link(c, fr); fr.childNodes.push(c); }
      return fr;
    }
    var e = makeElement(__mr(n, "tag") || "div", null, __g(n, "_sv_ns") === "svg" ? "svg" : (__g(n, "_sv_ns") === "math" ? "math" : undefined));
    __ns(e, "__odoc", __ownerDocOf(n));
    var a = __mr(n, "attrs") || {};
    for (var k in a) { if (Object.prototype.hasOwnProperty.call(a, k)) __mr(e, "attrs")[k] = a[k]; }
    for (var j = 0; j < (n.childNodes || []).length; j++) { var cc = deepClone(n.childNodes[j]); __nsf(cc, "parentNode", e); __ns(cc, "__parent", e); e.childNodes.push(cc); }
    return e;
  }
  __deepCloneNode = deepClone;
  // importNode: a real clone (options: deep copies children)
  var doc = globalThis.document;
  try {
    Object.defineProperty(doc, "importNode", { value: __native("importNode", 2, function (n, deep) {
      if (!n) throw new TypeError("Failed to execute 'importNode' on 'Document': 1 argument required");
      if (n.nodeType === 11) { var fr = deepClone(n); if (!deep) { __nsf(fr, "childNodes", []); } return fr; }
      return deep ? deepClone(n) : (n.nodeType === 3 ? makeText("") : makeElement(__mr(n, "tag") || "div", null, __g(n, "_sv_ns") === "svg" ? "svg" : undefined));
    }), writable: true, configurable: true, enumerable: true });
  } catch (e) {}
  // fragment-aware insertion: inserting a DocumentFragment splices its children
  var __insBefore = EP.insertBefore;
  EP.insertBefore = function (n, ref) {
    if (n && n.nodeType === 11) {
      var kids = n.childNodes.slice();
      var last = null;
      for (var i = 0; i < kids.length; i++) { last = __insBefore.call(this, kids[i], ref); }
      __nsf(n, "childNodes", []);
      return last || null;
    }
    return __insBefore.call(this, n, ref);
  };
  var __repChild = EP.replaceChild;
  EP.replaceChild = function (n, old) {
    if (n && n.nodeType === 11) {
      var kids = n.childNodes.slice();
      __insBefore.call(this, kids[0], old);
      this.removeChild(old);
      for (var i = 1; i < kids.length; i++) __insBefore.call(this, kids[i], null);
      __nsf(n, "childNodes", []);
      return old;
    }
    return __repChild.call(this, n, old);
  };
  // Range with the surfaces detectors measure
  function __s7offR(n) { try { var c = n && n.parentNode && n.parentNode.childNodes; if (!c) return 0; for (var i = 0; i < c.length; i++) if (c[i] === n) return i; return 0; } catch (e) { return 0; } }
  function __s7kidsR(n) { try { return (n && n.childNodes) ? n.childNodes.length : 0; } catch (e) { return 0; } }
  var RangeC = function Range() {
    if (new.target === undefined) throw new TypeError("Failed to construct 'Range': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
    __hide(this, "_s7b", "Range");
    __hide(this, "_start", null); __hide(this, "_end", null);
  };
  globalThis.Range = RangeC; window.Range = RangeC;
  Object.defineProperties(RangeC.prototype, {
    selectNode: { value: __native("selectNode", 1, function (n) { __s7k(this, "Range"); __hide(this, "_start", n); __hide(this, "_end", n); __hide(this, "_so", __s7offR(n)); __hide(this, "_eo", __s7offR(n) + 1); __hide(this, "_sf_collapsed", false); __hide(this, "_sf_startOffset", __s7offR(n)); __hide(this, "_sf_endOffset", __s7offR(n) + 1); __hide(this, "_sf_startContainer", n); __hide(this, "_sf_endContainer", n); __hide(this, "_sf_commonAncestorContainer", n); }), writable: true, configurable: true, enumerable: true },
    selectNodeContents: { value: __native("selectNodeContents", 1, function (n) { __s7k(this, "Range"); __hide(this, "_start", n); __hide(this, "_end", n); __hide(this, "_so", 0); __hide(this, "_eo", __s7kidsR(n)); __hide(this, "_sf_collapsed", false); __hide(this, "_sf_startOffset", 0); __hide(this, "_sf_endOffset", __s7kidsR(n)); __hide(this, "_sf_startContainer", n); __hide(this, "_sf_endContainer", n); __hide(this, "_sf_commonAncestorContainer", n); }), writable: true, configurable: true, enumerable: true },
    setStart: { value: __native("setStart", 2, function (n, o) { __s7k(this, "Range"); __hide(this, "_start", n); __hide(this, "_so", o); __hide(this, "_sf_startContainer", n); __hide(this, "_sf_startOffset", o); }), writable: true, configurable: true, enumerable: true },
    setEnd: { value: __native("setEnd", 2, function (n, o) { __s7k(this, "Range"); __hide(this, "_end", n); __hide(this, "_eo", o); __hide(this, "_sf_endContainer", n); __hide(this, "_sf_endOffset", o); }), writable: true, configurable: true, enumerable: true },
    collapse: { value: __native("collapse", 1, function (n, o) { __s7k(this, "Range"); __hide(this, "_start", n); __hide(this, "_end", n); __hide(this, "_so", o); __hide(this, "_eo", o); __hide(this, "_sf_collapsed", true); __hide(this, "_sf_startOffset", o); __hide(this, "_sf_endOffset", o); __hide(this, "_sf_startContainer", n); __hide(this, "_sf_endContainer", n); __hide(this, "_sf_commonAncestorContainer", n); }), writable: true, configurable: true, enumerable: true },
    createContextualFragment: { value: __native("createContextualFragment", 1, function (h) { __s7k(this, "Range"); var frag = __DCI.nc.fragment(); var pn = parseFragment(String(h)); for (var i = 0; i < pn.length; i++) { __DCI.nc.link(pn[i], frag); frag.childNodes.push(pn[i]); } return frag; }), writable: true, configurable: true, enumerable: true },
    getClientRects: { value: __native("getClientRects", 0, function () { __s7k(this, "Range"); return __mkRectList([globalThis.DOMRect(0, 0, 0, 0)]); }), writable: true, configurable: true, enumerable: true },
    getBoundingClientRect: { value: __native("getBoundingClientRect", 0, function () { __s7k(this, "Range"); return globalThis.DOMRect(0, 0, 0, 0); }), writable: true, configurable: true, enumerable: true },
  });
  __mkRange = function () { return new RangeC(); };
  if (globalThis.document && document.createRange) {
    try {
      Object.defineProperty(document, "createRange", { value: __native("createRange", 0, function () { return new RangeC(); }), writable: true, configurable: true, enumerable: true });
    } catch (e) {}
  }
  // Element-level method sharing: Chrome puts getElementsByClassName / query /
  // geometry on Element.prototype, which both HTML and SVG elements inherit.
  // Copy the engine's EP-implemented methods onto the rebuilt Element level
  // so SVG elements see them (they don't inherit the HTML prototype).
  try {
    var ElemP = __ElemC && __ElemC.prototype;
    if (ElemP) {
      var names = Object.getOwnPropertyNames(EP);
      for (var ni = 0; ni < names.length; ni++) {
        var nm = names[ni];
        if (nm === "constructor") continue;
        var d = Object.getOwnPropertyDescriptor(EP, nm);
        try { Object.defineProperty(ElemP, nm, d); } catch (e2) {}
      }
    }
  } catch (e) {}
})();

// ── SVG element set (ST-11 U2b-5) ──────────────────────────────────────
// The eight SVG classes the HTML league already referenced are created here
// as guarded constructors on stable prototype objects; the full 97-class
// family (chains, members, behaviours) is materialized at the prelude tail
// from assets/svg_tables.js.
(function () {
  var g = globalThis;
  function mk(name) {
    if (typeof g[name] === "function" && g[name].prototype) return;
    var C = function () {
      if (new.target !== undefined) throw new TypeError("Failed to construct '" + name + "': Illegal constructor");
      throw new TypeError("Illegal constructor");
    };
    try { Object.defineProperty(C, "name", { value: name, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C, "prototype", { writable: false }); } catch (e) {}
    g[name] = C; try { window[name] = C; } catch (e) {}
  }
  var names = ["SVGElement", "SVGGraphicsElement", "SVGSVGElement", "SVGGElement", "SVGTextContentElement", "SVGTextElement", "SVGRect", "SVGAnimatedRect"];
  for (var i = 0; i < names.length; i++) mk(names[i]);
})();

// ── DOMRect / DOMRectReadOnly / DOMRectList (Chrome-truth shapes) ──────
(function () {
  var DRO = function DOMRectReadOnly(x, y, w, h) {
    var r = Object.create(DRO.prototype);
    __hide(__hide(__hide(__hide(r, "_x", Number(x) || 0), "_y", Number(y) || 0), "_w", Number(w) || 0), "_h", Number(h) || 0);
    __hide(r, "_brand", "rect");
    return r;
  };
  globalThis.DOMRectReadOnly = DRO; window.DOMRectReadOnly = DRO;
  Object.defineProperties(DRO.prototype, {
    x: { get: __native("get x", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_x") || 0; }), enumerable: true, configurable: true },
    y: { get: __native("get y", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_y") || 0; }), enumerable: true, configurable: true },
    width: { get: __native("get width", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_w") || 0; }), enumerable: true, configurable: true },
    height: { get: __native("get height", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_h") || 0; }), enumerable: true, configurable: true },
    top: { get: __native("get top", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return Math.min(__g(this, "_y") || 0, (__g(this, "_y") || 0) + (__g(this, "_h") || 0)); }), enumerable: true, configurable: true },
    right: { get: __native("get right", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return Math.max(__g(this, "_x") || 0, (__g(this, "_x") || 0) + (__g(this, "_w") || 0)); }), enumerable: true, configurable: true },
    bottom: { get: __native("get bottom", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return Math.max(__g(this, "_y") || 0, (__g(this, "_y") || 0) + (__g(this, "_h") || 0)); }), enumerable: true, configurable: true },
    left: { get: __native("get left", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return Math.min(__g(this, "_x") || 0, (__g(this, "_x") || 0) + (__g(this, "_w") || 0)); }), enumerable: true, configurable: true },
    toJSON: { value: __native("toJSON", 0, function () { return { x: __g(this, "_x") || 0, y: __g(this, "_y") || 0, width: __g(this, "_w") || 0, height: __g(this, "_h") || 0, top: this.top, right: this.right, bottom: this.bottom, left: this.left }; }), writable: true, configurable: true },
  });
  var DR = function DOMRect(x, y, w, h) {
    var r = Object.create(DR.prototype);
    __hide(__hide(__hide(__hide(r, "_x", Number(x) || 0), "_y", Number(y) || 0), "_w", Number(w) || 0), "_h", Number(h) || 0);
    __hide(r, "_brand", "rect");
    return r;
  };
  globalThis.DOMRect = DR; window.DOMRect = DR;
  DR.prototype = Object.create(DRO.prototype);
  try { Object.defineProperty(DR.prototype, "constructor", { value: DR, writable: true, configurable: true }); } catch (e) {}
  Object.defineProperties(DR.prototype, {
    x: { get: __native("get x", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_x") || 0; }), set: function (v) { __hide(this, "_x", Number(v) || 0); }, enumerable: true, configurable: true },
    y: { get: __native("get y", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_y") || 0; }), set: function (v) { __hide(this, "_y", Number(v) || 0); }, enumerable: true, configurable: true },
    width: { get: __native("get width", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_w") || 0; }), set: function (v) { __hide(this, "_w", Number(v) || 0); }, enumerable: true, configurable: true },
    height: { get: __native("get height", 0, function () { if (__g(this, "_brand") !== "rect") throw new TypeError("Illegal invocation"); return __g(this, "_h") || 0; }), set: function (v) { __hide(this, "_h", Number(v) || 0); }, enumerable: true, configurable: true },
  });
  DR.fromRect = __native("fromRect", 1, function (r) { r = r || {}; return DR(r.x, r.y, r.width, r.height); });
  // DOMRectList: array-like, NOT an Array.
  var DRL = function DOMRectList() {};
  globalThis.DOMRectList = DRL; window.DOMRectList = DRL;
  DRL.prototype = { };
  Object.defineProperties(DRL.prototype, {
    length: { get: __native("get length", 0, function () { return __g(this, "_n") || 0; }), enumerable: true, configurable: true },
    item: { value: __native("item", 1, function (i) { return this[i] || null; }), writable: true, configurable: true },
    constructor: { value: DRL, writable: true, configurable: true },
  });
  try { Object.defineProperty(DRL.prototype, Symbol.toStringTag, { value: "DOMRectList", configurable: true }); } catch (e) {}
  try { DRL.prototype[Symbol.iterator] = __native("[Symbol.iterator]", 0, function () { var self = this, i = 0; return { next: function () { return i < (__g(self, "_n") || 0) ? { value: self[i++], done: false } : { value: undefined, done: true }; } }; }); } catch (e) {}
  __mkRectList = function (rects) {
    var l = Object.create(DRL.prototype);
    __hide(l, "_n", rects.length);
    for (var i = 0; i < rects.length; i++) l[i] = rects[i];
    return l;
  };
  // Element geometry — replace the flat stubs with DOMRect-shaped returns.
  var zeroRect = function () { return DR(0, 0, 0, 0); };
  EP.getBoundingClientRect = __native("getBoundingClientRect", 0, zeroRect);
  EP.getClientRects = __native("getClientRects", 0, function () { return __mkRectList([zeroRect()]); });
  // (getBoxQuads is a Gecko extension Chrome does not expose — the old
  // present-but-undefined own prop read as a prototype lie; removed.)
})();

// ── Plugin / PluginArray / MimeType / MimeTypeArray (Chrome-truth) ─────
(function () {
  var Plugin = function () {};
  globalThis.Plugin = Plugin; window.Plugin = Plugin;
  var PluginArray = function () {};
  globalThis.PluginArray = PluginArray; window.PluginArray = PluginArray;
  var MimeType = function () {};
  globalThis.MimeType = MimeType; window.MimeType = MimeType;
  var MimeTypeArray = function () {};
  globalThis.MimeTypeArray = MimeTypeArray; window.MimeTypeArray = MimeTypeArray;
  var g = function (name, fn) { return __native(name, 0, fn); };
  Plugin.prototype = { };
  Object.defineProperties(Plugin.prototype, {
    name: { get: g("get name", function () { return __g(this, "_name") || ""; }), enumerable: true, configurable: true },
    filename: { get: g("get filename", function () { return __g(this, "_filename") || ""; }), enumerable: true, configurable: true },
    description: { get: g("get description", function () { return __g(this, "_description") || ""; }), enumerable: true, configurable: true },
    length: { get: g("get length", function () { return __g(this, "_mimes") ? __g(this, "_mimes").length : 0; }), enumerable: true, configurable: true },
    item: { value: __native("item", 1, function (i) { return (__g(this, "_mimes") && __g(this, "_mimes")[i]) || null; }), writable: true, configurable: true },
    namedItem: { value: __native("namedItem", 1, function (n) { var m = __g(this, "_mimes") || []; for (var i = 0; i < m.length; i++) if (m[i] && m[i].type === n) return m[i]; return null; }), writable: true, configurable: true },
    constructor: { value: Plugin, writable: true, configurable: true },
  });
  try { Object.defineProperty(Plugin.prototype, Symbol.toStringTag, { value: "Plugin", configurable: true }); } catch (e) {}
  MimeType.prototype = { };
  Object.defineProperties(MimeType.prototype, {
    type: { get: g("get type", function () { return __g(this, "_type") || ""; }), enumerable: true, configurable: true },
    suffixes: { get: g("get suffixes", function () { return __g(this, "_suffixes") || ""; }), enumerable: true, configurable: true },
    description: { get: g("get description", function () { return __g(this, "_description") || ""; }), enumerable: true, configurable: true },
    enabledPlugin: { get: g("get enabledPlugin", function () { return __g(this, "_plugin") || null; }), enumerable: true, configurable: true },
    constructor: { value: MimeType, writable: true, configurable: true },
  });
  try { Object.defineProperty(MimeType.prototype, Symbol.toStringTag, { value: "MimeType", configurable: true }); } catch (e) {}
  PluginArray.prototype = { };
  Object.defineProperties(PluginArray.prototype, {
    length: { get: g("get length", function () { return __g(this, "_n") || 0; }), enumerable: true, configurable: true },
    item: { value: __native("item", 1, function (i) { return this[i] || null; }), writable: true, configurable: true },
    namedItem: { value: __native("namedItem", 1, function (n) { for (var i = 0; i < (__g(this, "_n") || 0); i++) if (this[i] && this[i]._name === n) return this[i]; return null; }), writable: true, configurable: true },
    refresh: { value: __native("refresh", 0, function () { }), writable: true, configurable: true },
    constructor: { value: PluginArray, writable: true, configurable: true },
  });
  try { Object.defineProperty(PluginArray.prototype, Symbol.toStringTag, { value: "PluginArray", configurable: true }); } catch (e) {}
  MimeTypeArray.prototype = { };
  Object.defineProperties(MimeTypeArray.prototype, {
    length: { get: g("get length", function () { return __g(this, "_n") || 0; }), enumerable: true, configurable: true },
    item: { value: __native("item", 1, function (i) { return this[i] || null; }), writable: true, configurable: true },
    namedItem: { value: __native("namedItem", 1, function (n) { for (var i = 0; i < (__g(this, "_n") || 0); i++) if (this[i] && this[i]._type === n) return this[i]; return null; }), writable: true, configurable: true },
    constructor: { value: MimeTypeArray, writable: true, configurable: true },
  });
  try { Object.defineProperty(MimeTypeArray.prototype, Symbol.toStringTag, { value: "MimeTypeArray", configurable: true }); } catch (e) {}
  // Build the PDF-viewer set exactly as Chrome reports it (oracle-verified
  // names; the five entries every Chrome carries).
  var defs = [
    ["PDF Viewer", "internal-pdf-viewer", "Portable Document Format"],
    ["Chrome PDF Viewer", "internal-pdf-viewer", "Portable Document Format"],
    ["Chromium PDF Viewer", "internal-pdf-viewer", "Portable Document Format"],
    ["Microsoft Edge PDF Viewer", "internal-pdf-viewer", "Portable Document Format"],
    ["WebKit built-in PDF", "internal-pdf-viewer", "Portable Document Format"],
  ];
  var mkMime = function (type, suffixes, description, plugin) {
    var m = Object.create(MimeType.prototype);
    __hide(__hide(__hide(__hide(m, "_type", type), "_suffixes", suffixes), "_description", description), "_plugin", plugin);
    return m;
  };
  var allMimes = [];
  var plugins = Object.create(PluginArray.prototype);
  __hide(plugins, "_n", defs.length);
  for (var i = 0; i < defs.length; i++) {
    var pl = Object.create(Plugin.prototype);
    __hide(__hide(__hide(__hide(pl, "_name", defs[i][0]), "_filename", defs[i][1]), "_description", defs[i][2]), "_mimes", null);
    var m1 = mkMime("application/pdf", "pdf", "Portable Document Format", pl);
    var m2 = mkMime("text/pdf", "pdf", "Portable Document Format", pl);
    __hide(pl, "_mimes", [m1, m2]);
    pl[0] = m1; pl[1] = m2;
    try { __hide(pl, "__apdf", m1); Object.defineProperty(pl, "application/pdf", { value: m1, writable: false, configurable: true, enumerable: false }); Object.defineProperty(pl, "text/pdf", { value: m2, writable: false, configurable: true, enumerable: false }); } catch (e) { pl["application/pdf"] = m1; pl["text/pdf"] = m2; }
    plugins[i] = pl;
    // Named entries stay accessible but must not serialize (Chrome's
    // JSON.stringify(navigator.plugins) lists only the indexed entries).
    try { Object.defineProperty(plugins, defs[i][0], { value: pl, writable: false, configurable: true, enumerable: false }); } catch (e) { plugins[defs[i][0]] = pl; }
    if (i === 0) allMimes.push(m1, m2);
  }
  var mimeTypes = Object.create(MimeTypeArray.prototype);
  __hide(mimeTypes, "_n", allMimes.length);
  for (var j = 0; j < allMimes.length; j++) mimeTypes[j] = allMimes[j];
  try { Object.defineProperty(mimeTypes, "application/pdf", { value: allMimes[0], writable: false, configurable: true, enumerable: false }); } catch (e) { mimeTypes["application/pdf"] = allMimes[0]; }
  try { Object.defineProperty(mimeTypes, "text/pdf", { value: allMimes[1], writable: false, configurable: true, enumerable: false }); } catch (e) { mimeTypes["text/pdf"] = allMimes[1]; }
  try { Object.defineProperty(navigator, "plugins", { value: plugins, enumerable: true, configurable: true }); } catch (e) { navigator.plugins = plugins; }
  try { Object.defineProperty(navigator, "mimeTypes", { value: mimeTypes, enumerable: true, configurable: true }); } catch (e) { navigator.mimeTypes = mimeTypes; }
  try { Object.defineProperty(navigator, "pdfViewerEnabled", { value: true, enumerable: true, configurable: true }); } catch (e) {}
})();

// ── NavigatorUAData (prototype-shaped, Chrome-truth) ───────────────────
(function () {
  var UAD = function NavigatorUAData() {};
  globalThis.NavigatorUAData = UAD; window.NavigatorUAData = UAD;
  var ident = __DCI.IDENTITY || {};
  var brands = (navigator.userAgentData && navigator.userAgentData.brands) || [];
  UAD.prototype = { };
  var uad = Object.create(UAD.prototype);
  __hide(__hide(uad, "_brands", brands), "_mobile", false);
  __hide(uad, "_platform", ident.platform === "Linux" ? "Linux" : (ident.platform || "Linux"));
  Object.defineProperties(UAD.prototype, {
    brands: { get: __native("get brands", 0, function () { return __g(this, "_brands") || []; }), enumerable: true, configurable: true },
    mobile: { get: __native("get mobile", 0, function () { return !!__g(this, "_mobile"); }), enumerable: true, configurable: true },
    platform: { get: __native("get platform", 0, function () { return __g(this, "_platform") || "Linux"; }), enumerable: true, configurable: true },
    getHighEntropyValues: { value: __native("getHighEntropyValues", 1, function (keys) {
      var self = this;
      var id = __DCI.IDENTITY || {};
      var all = {
        architecture: "x86", bitness: "64", model: "",
        platform: __g(self, "_platform") || "Linux",
        platformVersion: id.platformVersion || "6.6.0",
        uaFullVersion: (id.chromeMajor || 0) + ".0.0.0",
        fullVersionList: (__g(self, "_brands") || []).map(function (b) { return { brand: b.brand, version: b.version + ".0.0.0" }; }),
        wow64: false,
      };
      var out = {};
      (keys || []).forEach(function (k) { if (k in all) out[k] = all[k]; });
      return Promise.resolve(out);
    }), writable: true, configurable: true },
    toJSON: { value: __native("toJSON", 0, function () { return { brands: __g(this, "_brands") || [], mobile: !!__g(this, "_mobile"), platform: __g(this, "_platform") || "Linux" }; }), writable: true, configurable: true },
    constructor: { value: UAD, writable: true, configurable: true },
  });
  try { Object.defineProperty(UAD.prototype, Symbol.toStringTag, { value: "NavigatorUAData", configurable: true }); } catch (e) {}
  try { Object.defineProperty(navigator, "userAgentData", { value: uad, enumerable: true, configurable: true }); } catch (e) { navigator.userAgentData = uad; }
})();

// ── Intl: callable-without-new + Chrome default resolvedOptions ────────
(function () {
  var id = __DCI.IDENTITY || {};
  var LOC = id.locale || "en-US";
  var TZ = id.tz || "UTC";
  var DTF = function DateTimeFormat(locale, opts) {
    if (!(this instanceof DTF)) return new DTF(locale, opts);
    __hide(this, "_l", locale); __hide(this, "_o", opts || {});
  };
  __DTF = DTF;
  DTF.prototype = { };
  Object.defineProperties(DTF.prototype, {
    resolvedOptions: { value: __native("resolvedOptions", 0, function () {
      return { locale: LOC, calendar: "gregory", numberingSystem: "latn", timeZone: TZ, year: "numeric", month: "numeric", day: "numeric" };
    }), writable: true, configurable: true },
    formatToParts: { value: __native("formatToParts", 1, function (d) { return [{ type: "literal", value: String(new Date(d === undefined ? Date.now() : d)) }]; }), writable: true, configurable: true },
    formatRange: { value: __native("formatRange", 2, function (a, b) { return String(new Date(a)) + " – " + String(new Date(b)); }), writable: true, configurable: true },
    formatRangeToParts: { value: __native("formatRangeToParts", 2, function (a, b) { return [{ type: "literal", value: String(new Date(a)) + " – " + String(new Date(b)), source: "shared" }]; }), writable: true, configurable: true },
    constructor: { value: DTF, writable: true, configurable: true },
  });
  try { Object.defineProperty(DTF.prototype, Symbol.toStringTag, { value: "Intl.DateTimeFormat", configurable: true }); } catch (e) {}
  // Chrome truth: `format` is an accessor returning a per-instance bound
  // function (name "", native ts) — data property was a shape lie (d2).
  try {
    var __fmtImpl = __native("format", 1, function (d) { return String(new Date(d === undefined ? Date.now() : d)); });
    Object.defineProperty(DTF.prototype, "format", {
      get: function () {
        var cached = __g(this, "_fmt");
        if (cached === undefined) {
          var self = this;
          cached = __native("", 1, function (d) { return __fmtImpl.call(self, d); });
          __hide(this, "_fmt", cached);
        }
        return cached;
      },
      configurable: true,
    });
  } catch (e) {}
  DTF.supportedLocalesOf = __native("supportedLocalesOf", 1, function (l) { return l || []; });
  var NF = function NumberFormat(locale, opts) {
    if (!(this instanceof NF)) return new NF(locale, opts);
  };
  Object.defineProperties(NF.prototype, {
    resolvedOptions: { value: __native("resolvedOptions", 0, function () {
      return { locale: LOC, numberingSystem: "latn", style: "decimal", useGrouping: true, minimumIntegerDigits: 1, minimumFractionDigits: 0, maximumFractionDigits: 3 };
    }), writable: true, configurable: true },
    format: { value: __native("format", 1, function (x) { return String(x); }), writable: true, configurable: true },
    formatToParts: { value: __native("formatToParts", 1, function (x) { return [{ type: "integer", value: String(x) }]; }), writable: true, configurable: true },
  });
  NF.supportedLocalesOf = __native("supportedLocalesOf", 1, function (l) { return l || []; });
  var CO = function Collator(locale, opts) {
    if (!(this instanceof CO)) return new CO(locale, opts);
  };
  Object.defineProperties(CO.prototype, {
    compare: { value: __native("compare", 2, function (a, b) { a = String(a); b = String(b); return a < b ? -1 : (a > b ? 1 : 0); }), writable: true, configurable: true },
    resolvedOptions: { value: __native("resolvedOptions", 0, function () {
      return { locale: LOC, usage: "sort", sensitivity: "variant", ignorePunctuation: false, collation: "default", numeric: false, caseFirst: "false" };
    }), writable: true, configurable: true },
  });
  CO.supportedLocalesOf = __native("supportedLocalesOf", 1, function (l) { return l || []; });
  function simpleIntl(name, proto) {
    var C = function (locale, opts) { if (!(this instanceof C)) return new C(locale, opts); };
    Object.defineProperty(C, "name", { value: name, configurable: true });
    C.prototype = Object.create(proto || Object.prototype);
    Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true });
    Object.defineProperty(C.prototype, "resolvedOptions", { value: __native("resolvedOptions", 0, function () { return { locale: LOC }; }), writable: true, configurable: true });
    C.supportedLocalesOf = __native("supportedLocalesOf", 1, function (l) { return l || []; });
    return C;
  }
  var DN = simpleIntl("DisplayNames");
  Object.defineProperty(DN.prototype, "of", { value: __native("of", 1, function (c) { return String(c); }), writable: true, configurable: true });
  var LF = simpleIntl("ListFormat");
  Object.defineProperty(LF.prototype, "format", { value: __native("format", 1, function (l) { return (l || []).join(", "); }), writable: true, configurable: true });
  Object.defineProperty(LF.prototype, "formatToParts", { value: __native("formatToParts", 1, function (l) { return [{ type: "element", value: (l || []).join(", ") }]; }), writable: true, configurable: true });
  var RTF = simpleIntl("RelativeTimeFormat");
  Object.defineProperty(RTF.prototype, "format", { value: __native("format", 2, function (v, u) { return String(v) + " " + String(u); }), writable: true, configurable: true });
  Object.defineProperty(RTF.prototype, "formatToParts", { value: __native("formatToParts", 2, function (v, u) { return [{ type: "literal", value: String(v) + " " + String(u) }]; }), writable: true, configurable: true });
  var PR = simpleIntl("PluralRules");
  Object.defineProperty(PR.prototype, "select", { value: __native("select", 1, function () { return "other"; }), writable: true, configurable: true });
  Object.defineProperty(PR.prototype, "selectRange", { value: __native("selectRange", 2, function () { return "other"; }), writable: true, configurable: true });
  var SG = simpleIntl("Segmenter");
  Object.defineProperty(SG.prototype, "segment", { value: __native("segment", 1, function () { return __mkRectList ? { _n: 0, [Symbol.iterator]: function () { return { next: function () { return { done: true }; } }; } } : null; }), writable: true, configurable: true });
  globalThis.Intl = Intl || {};
  Intl.DateTimeFormat = DTF; Intl.NumberFormat = NF; Intl.Collator = CO;
  Intl.DisplayNames = DN; Intl.ListFormat = LF; Intl.RelativeTimeFormat = RTF; Intl.PluralRules = PR; Intl.Segmenter = SG;
  Intl.getCanonicalLocales = __native("getCanonicalLocales", 1, function (l) { return l || []; });
  Intl.supportedValuesOf = __native("supportedValuesOf", 1, function () { return []; });
})();

// ── SpeechSynthesis (machine-truth: no TTS engine → getVoices() = []) ──
(function () {
  var SS = function SpeechSynthesis() {
    if (new.target === undefined) throw new TypeError("Illegal constructor");
    throw new TypeError("Failed to construct 'SpeechSynthesis': Illegal constructor");
  };
  globalThis.SpeechSynthesis = SS; window.SpeechSynthesis = SS;
  var SU = function SpeechSynthesisUtterance(text) { if (!(this instanceof SU)) return new SU(text); __ns(this, "text", String(text || "")); };
  globalThis.SpeechSynthesisUtterance = SU; window.SpeechSynthesisUtterance = SU;
  var SV = function SpeechSynthesisVoice() {};
  globalThis.SpeechSynthesisVoice = SV; window.SpeechSynthesisVoice = SV;
  var ss = Object.create(SS.prototype);
  // Chrome: the speechSynthesis INSTANCE carries NO own properties — every
  // attribute lives as an accessor on SpeechSynthesis.prototype. Own data
  // props on the instance are exactly what the creepjs prototype lie engine
  // flags; hidden state holds the values instead.
  function ssAttr(name, key, dflt, handler) {
    return {
      get: __native("get " + name, 0, function () { if (!(this instanceof SS)) throw new TypeError("Illegal invocation"); var v = __g(this, key); return v === undefined ? dflt : v; }),
      set: __native("set " + name, 1, function (v) { if (!(this instanceof SS)) throw new TypeError("Illegal invocation"); __hide(this, key, handler ? (typeof v === "function" ? v : null) : !!v); }),
      enumerable: true, configurable: true,
    };
  }
  Object.defineProperties(SS.prototype, {
    getVoices: { value: __native("getVoices", 0, function () { return []; }), writable: true, configurable: true },
    speak: { value: __native("speak", 1, function () { }), writable: true, configurable: true },
    cancel: { value: __native("cancel", 0, function () { }), writable: true, configurable: true },
    pause: { value: __native("pause", 0, function () { }), writable: true, configurable: true },
    resume: { value: __native("resume", 0, function () { }), writable: true, configurable: true },
    speaking: ssAttr("speaking", "_ssSpeaking", false, false),
    pending: ssAttr("pending", "_ssPending", false, false),
    paused: ssAttr("paused", "_ssPaused", false, false),
    onvoiceschanged: ssAttr("onvoiceschanged", "_ssOnvoiceschanged", null, true),
    onstart: ssAttr("onstart", "_ssOnstart", null, true),
    onend: ssAttr("onend", "_ssOnend", null, true),
    onerror: ssAttr("onerror", "_ssOnerror", null, true),
    onpause: ssAttr("onpause", "_ssOnpause", null, true),
    onresume: ssAttr("onresume", "_ssOnresume", null, true),
  });
  try { Object.defineProperty(window, "speechSynthesis", { value: ss, enumerable: true, configurable: true }); } catch (e) { window.speechSynthesis = ss; }
})();

// ── Web Audio: the property matrix creepjs reads, at Chrome's exact
//    defaults (values captured from the local Chromium oracle), plus a
//    deterministic persona-stable render. Geometry-free machine: the
//    frequency data is real silence (-Infinity), like Chrome's.
(function () {
  var ident = __DCI.IDENTITY || {};
  var seed = 1337;
  function param(defaultValue, minValue, maxValue, value) {
    var p = Object.create(AudioParam.prototype);
    __hide(__hide(__hide(__hide(p, "_d", defaultValue), "_v", value !== undefined ? value : defaultValue), "_min", minValue), "_max", maxValue);
    __hide(p, "_ar", "a-rate");
    return p;
  }
  var AudioParam = function () {};
  globalThis.AudioParam = AudioParam; window.AudioParam = AudioParam;
  Object.defineProperties(AudioParam.prototype, {
    value: { get: __native("get value", 0, function () { return __g(this, "_v"); }), set: function (v) { __hide(this, "_v", Number(v)); }, enumerable: true, configurable: true },
    defaultValue: { get: __native("get defaultValue", 0, function () { return __g(this, "_d"); }), enumerable: true, configurable: true },
    minValue: { get: __native("get minValue", 0, function () { return __g(this, "_min"); }), enumerable: true, configurable: true },
    maxValue: { get: __native("get maxValue", 0, function () { return __g(this, "_max"); }), enumerable: true, configurable: true },
    automationRate: { get: __native("get automationRate", 0, function () { return __g(this, "_ar"); }), set: function (v) { __hide(this, "_ar", String(v)); }, enumerable: true, configurable: true },
  });
  ["setValueAtTime", "linearRampToValueAtTime", "exponentialRampToValueAtTime", "setTargetAtTime", "setValueCurveAtTime", "cancelScheduledValues", "cancelAndHoldAtTime"].forEach(function (m) {
    Object.defineProperty(AudioParam.prototype, m, { value: __native(m, 2, function () { return this; }), writable: true, configurable: true });
  });
  var F32MAX = 3.4028234663852886e+38;
  var AudioNode = function () {};
  globalThis.AudioNode = AudioNode; window.AudioNode = AudioNode;
  var AudioScheduledSourceNode = function () {};
  AudioScheduledSourceNode.prototype = Object.create(AudioNode.prototype);
  // EXPORT IT: the last-pass chain fixup parents OscillatorNode <
  // globalThis.AudioScheduledSourceNode. Before this export the bank's
  // (method-less) copy was the global, so the fixup silently re-parented
  // OscillatorNode.prototype off THIS prototype — severing start/stop
  // (creepjs audio group: "oscillator.start is not a function"; caught by
  // the item-#3 re-run 2026-10-18).
  globalThis.AudioScheduledSourceNode = AudioScheduledSourceNode; window.AudioScheduledSourceNode = AudioScheduledSourceNode;
  function node(kind, make) {
    var C = function () {};
    Object.defineProperty(C, "name", { value: kind, configurable: true });
    C.prototype = Object.create(AudioNode.prototype);
    Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true });
    globalThis[kind] = C; window[kind] = C;
    return C;
  }
  // AudioNode.prototype shared attributes (Chrome defaults)
  Object.defineProperties(AudioNode.prototype, {
    connect: { value: __native("connect", 1, function () { return this; }), writable: true, configurable: true },
    disconnect: { value: __native("disconnect", 0, function () { }), writable: true, configurable: true },
    channelCount: { get: __native("get channelCount", 0, function () { return __g(this, "_cc") === undefined ? 2 : __g(this, "_cc"); }), set: function (v) { __hide(this, "_cc", v); }, enumerable: true, configurable: true },
    channelCountMode: { get: __native("get channelCountMode", 0, function () { return __g(this, "_ccm") || "max"; }), set: function (v) { __hide(this, "_ccm", String(v)); }, enumerable: true, configurable: true },
    channelInterpretation: { get: __native("get channelInterpretation", 0, function () { return __g(this, "_ci") || "speakers"; }), set: function (v) { __hide(this, "_ci", String(v)); }, enumerable: true, configurable: true },
    numberOfInputs: { get: __native("get numberOfInputs", 0, function () { return __g(this, "_ni") === undefined ? 1 : __g(this, "_ni"); }), enumerable: true, configurable: true },
    numberOfOutputs: { get: __native("get numberOfOutputs", 0, function () { return __g(this, "_no") === undefined ? 1 : __g(this, "_no"); }), enumerable: true, configurable: true },
    context: { get: __native("get context", 0, function () { return __g(this, "_ctx") || null; }), enumerable: true, configurable: true },
  });
  var analyserProto;
  (function () {
    var AN = node("AnalyserNode");
    Object.defineProperties(AN.prototype, {
      fftSize: { get: __native("get fftSize", 0, function () { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); return __g(this, "_fft") || 2048; }), set: function (v) { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); v = Number(v) || 2048; if (v < 32) v = 32; if (v > 32768) v = 32768; __hide(this, "_fft", v); }, enumerable: true, configurable: true },
      frequencyBinCount: { get: __native("get frequencyBinCount", 0, function () { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); return (__g(this, "_fft") || 2048) / 2; }), enumerable: true, configurable: true },
      minDecibels: { get: __native("get minDecibels", 0, function () { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); return __g(this, "_minDb") === undefined ? -100 : __g(this, "_minDb"); }), set: function (v) { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); __hide(this, "_minDb", Number(v)); }, enumerable: true, configurable: true },
      maxDecibels: { get: __native("get maxDecibels", 0, function () { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); return __g(this, "_maxDb") === undefined ? -30 : __g(this, "_maxDb"); }), set: function (v) { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); __hide(this, "_maxDb", Number(v)); }, enumerable: true, configurable: true },
      smoothingTimeConstant: { get: __native("get smoothingTimeConstant", 0, function () { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); return __g(this, "_stc") === undefined ? 0.8 : __g(this, "_stc"); }), set: function (v) { if (__g(this, "_brand") !== "AnalyserNode") throw new TypeError("Illegal invocation"); __hide(this, "_stc", Number(v)); }, enumerable: true, configurable: true },
      getFloatFrequencyData: { value: __native("getFloatFrequencyData", 1, function (a) { for (var i = 0; i < a.length; i++) a[i] = -Infinity; }), writable: true, configurable: true },
      getByteFrequencyData: { value: __native("getByteFrequencyData", 1, function (a) { for (var i = 0; i < a.length; i++) a[i] = 0; }), writable: true, configurable: true },
      getFloatTimeDomainData: { value: __native("getFloatTimeDomainData", 1, function (a) { for (var i = 0; i < a.length; i++) a[i] = 0; }), writable: true, configurable: true },
      getByteTimeDomainData: { value: __native("getByteTimeDomainData", 1, function (a) { for (var i = 0; i < a.length; i++) a[i] = 128; }), writable: true, configurable: true },
    });
  })();
  (function () {
    var BQ = node("BiquadFilterNode");
    var bqDefs = {
      frequency: [350, 0, 22050], detune: [0, -153600, 153600],
      Q: [1, -F32MAX, F32MAX], gain: [0, -1541.273681640625, 1541.273681640625],
    };
    Object.keys(bqDefs).forEach(function (k) {
      var d = bqDefs[k];
      Object.defineProperty(BQ.prototype, k, { get: __native("get " + k, 0, function () { if (!(this instanceof BQ)) throw new TypeError("Illegal invocation"); var _pv = __g(this, "_" + k); if (!_pv) { _pv = param(d[0], d[1], d[2]); __hide(this, "_" + k, _pv); } return _pv; }), enumerable: true, configurable: true });
    });
    Object.defineProperty(BQ.prototype, "type", { get: __native("get type", 0, function () { if (!(this instanceof BQ)) throw new TypeError("Illegal invocation"); return __g(this, "_type") || "lowpass"; }), set: function (v) { __hide(this, "_type", String(v)); }, enumerable: true, configurable: true });
    Object.defineProperty(BQ.prototype, "getFrequencyResponse", { value: __native("getFrequencyResponse", 3, function () { }), writable: true, configurable: true });
  })();
  (function () {
    var DC = node("DynamicsCompressorNode");
    var dcDefs = { threshold: [-24, -100, 0], knee: [30, 0, 40], ratio: [12, 1, 20], attack: [0.003000000026077032, 0, 1], release: [0.25, 0, 1] };
    Object.keys(dcDefs).forEach(function (k) {
      var d = dcDefs[k];
      Object.defineProperty(DC.prototype, k, { get: __native("get " + k, 0, function () { var _pv = __g(this, "_" + k); if (!_pv) { _pv = param(d[0], d[1], d[2]); __hide(this, "_" + k, _pv); } return _pv; }), enumerable: true, configurable: true });
    });
    Object.defineProperty(DC.prototype, "reduction", { get: __native("get reduction", 0, function () { return 0; }), enumerable: true, configurable: true });
  })();
  (function () {
    var OS = node("OscillatorNode");
    OS.prototype = Object.create(AudioScheduledSourceNode.prototype);
    Object.defineProperty(OS.prototype, "constructor", { value: OS, writable: true, configurable: true });
    Object.defineProperty(OS.prototype, "frequency", { get: __native("get frequency", 0, function () { if (!__g(this, "_frequency")) __hide(this, "_frequency", param(440, -22050, 22050)); return __g(this, "_frequency"); }), enumerable: true, configurable: true });
    Object.defineProperty(OS.prototype, "detune", { get: __native("get detune", 0, function () { if (!__g(this, "_detune")) __hide(this, "_detune", param(0, -153600, 153600)); return __g(this, "_detune"); }), enumerable: true, configurable: true });
    Object.defineProperty(OS.prototype, "type", { get: __native("get type", 0, function () { return __g(this, "_type") || "sine"; }), set: function (v) { __hide(this, "_type", String(v)); }, enumerable: true, configurable: true });
    Object.defineProperty(OS.prototype, "setPeriodicWave", { value: __native("setPeriodicWave", 1, function () { }), writable: true, configurable: true });
  })();
  (function () {
    var GA = node("GainNode");
    Object.defineProperty(GA.prototype, "gain", { get: __native("get gain", 0, function () { if (!__g(this, "_gain")) __hide(this, "_gain", param(1, -F32MAX, F32MAX)); return __g(this, "_gain"); }), enumerable: true, configurable: true });
    var PN = node("PannerNode");
    var DSP = node("StereoPannerNode");
    Object.defineProperty(DSP.prototype, "pan", { get: __native("get pan", 0, function () { if (!__g(this, "_pan")) __hide(this, "_pan", param(0, -1, 1)); return __g(this, "_pan"); }), enumerable: true, configurable: true });
    var DL = node("DelayNode");
    Object.defineProperty(DL.prototype, "delayTime", { get: __native("get delayTime", 0, function () { if (!__g(this, "_delayTime")) __hide(this, "_delayTime", param(0, 0, 180)); return __g(this, "_delayTime"); }), enumerable: true, configurable: true });
    var CN = node("ConstantSourceNode");
    Object.defineProperty(CN.prototype, "offset", { get: __native("get offset", 0, function () { if (!__g(this, "_offset")) __hide(this, "_offset", param(1, -F32MAX, F32MAX)); return __g(this, "_offset"); }), enumerable: true, configurable: true });
  })();
  var AudioDestinationNode = node("AudioDestinationNode");
  // Scheduled-source common surface — Chrome-exact: onended accessor,
  // start/stop length 0, constructor LAST (Chrome own-prop order
  // [onended, start, stop, constructor]).
  Object.defineProperties(AudioScheduledSourceNode.prototype, {
    onended: { get: __native("get onended", 0, function () { return __g(this, "_onended") === undefined ? null : __g(this, "_onended"); }), set: function (v) { __hide(this, "_onended", v); }, enumerable: true, configurable: true },
    start: { value: __native("start", 0, function () { }), writable: true, configurable: true },
    stop: { value: __native("stop", 0, function () { }), writable: true, configurable: true },
    constructor: { value: AudioScheduledSourceNode, writable: true, configurable: true },
  });
  var AudioListener = function () {};
  globalThis.AudioListener = AudioListener; window.AudioListener = AudioListener;
  ["forwardX", "forwardY", "forwardZ"].forEach(function (k) {
    Object.defineProperty(AudioListener.prototype, k, { get: __native("get " + k, 0, function () { if (!this["_" + k]) this["_" + k] = param(k === "forwardZ" ? -1 : 0, -F32MAX, F32MAX); return this["_" + k]; }), enumerable: true, configurable: true });
  });
  ["upX", "upY", "upZ"].forEach(function (k) {
    Object.defineProperty(AudioListener.prototype, k, { get: __native("get " + k, 0, function () { if (!this["_" + k]) this["_" + k] = param(k === "upY" ? 1 : 0, -F32MAX, F32MAX); return this["_" + k]; }), enumerable: true, configurable: true });
  });
  ["positionX", "positionY", "positionZ"].forEach(function (k) {
    Object.defineProperty(AudioListener.prototype, k, { get: __native("get " + k, 0, function () { if (!this["_" + k]) this["_" + k] = param(0, -F32MAX, F32MAX); return this["_" + k]; }), enumerable: true, configurable: true });
  });
  var mkBuffer = function (ch, len, sr) {
    var b = Object.create(AudioBuffer.prototype);
    __hide(__hide(__hide(b, "_ch", ch), "_len", len), "_sr", sr);
    var data = [];
    var s = seed;
    for (var c = 0; c < ch; c++) {
      var d = new Float32Array(len);
      for (var i = 0; i < len; i++) {
        if (i < 4500) { d[i] = 0; continue; }
        s = (s * 48271) % 2147483647; d[i] = (s / 2147483647 - 0.5) * 0.0001;
      }
      data.push(d);
    }
    __hide(b, "_data", data);
    return b;
  };
  var AudioBuffer = function () {};
  globalThis.AudioBuffer = AudioBuffer; window.AudioBuffer = AudioBuffer;
  Object.defineProperties(AudioBuffer.prototype, {
    numberOfChannels: { get: __native("get numberOfChannels", 0, function () { if (!(this instanceof AudioBuffer)) throw new TypeError("Illegal invocation"); return __g(this, "_ch"); }), enumerable: true, configurable: true },
    length: { get: __native("get length", 0, function () { if (!(this instanceof AudioBuffer)) throw new TypeError("Illegal invocation"); return __g(this, "_len"); }), enumerable: true, configurable: true },
    sampleRate: { get: __native("get sampleRate", 0, function () { if (!(this instanceof AudioBuffer)) throw new TypeError("Illegal invocation"); return __g(this, "_sr"); }), enumerable: true, configurable: true },
    duration: { get: __native("get duration", 0, function () { if (!(this instanceof AudioBuffer)) throw new TypeError("Illegal invocation"); return __g(this, "_len") / __g(this, "_sr"); }), enumerable: true, configurable: true },
    getChannelData: { value: __native("getChannelData", 1, function (i) { return __g(this, "_data")[i] || (__g(this, "_data")[i] = new Float32Array(__g(this, "_len"))); }), writable: true, configurable: true },
    copyFromChannel: { value: __native("copyFromChannel", 3, function (dst, ch, off) { var d = __g(this, "_data")[ch] || new Float32Array(__g(this, "_len")); off = off || 0; for (var i = 0; i < dst.length && off + i < d.length; i++) dst[i] = d[off + i]; }), writable: true, configurable: true },
    copyToChannel: { value: __native("copyToChannel", 3, function (src, ch, off) { var d = __g(this, "_data")[ch] || (__g(this, "_data")[ch] = new Float32Array(__g(this, "_len"))); off = off || 0; for (var i = 0; i < src.length && off + i < d.length; i++) d[off + i] = src[i]; }), writable: true, configurable: true },
  });
  function mkContext(numberOfChannels, length, sampleRate, __proto) {
    var c = Object.create(__proto || AudioContext.prototype);
    __hide(__hide(__hide(c, "_nc", numberOfChannels || 2), "_len", length || 0), "_sr", sampleRate || 44100);
    __hide(__hide(c, "_state", "suspended"), "_time", 0);
    __hide(c, "destination", Object.create(AudioDestinationNode.prototype));
    __hide(__hide(__hide(c.destination, "_ni", 0), "_no", 1), "_ctx", c);
    __hide(c.destination, "_maxcc", 2);
    __hide(c, "listener", Object.create(AudioListener.prototype));
    c.oncomplete = null; c.onsinkchange = null; c.onerror = null; c.onstatechange = null;
    try { addEventTarget(c); } catch (e) {}
    return c;
  }
  var AudioContext = function AudioContext(opts) {
    if (!(this instanceof AudioContext)) return new AudioContext(opts);
    var c = mkContext(2, 0, (opts && opts.sampleRate) || 44100);
    __hide(c, "_state", "running");
    return c;
  };
  globalThis.AudioContext = AudioContext; window.AudioContext = AudioContext;
  var OfflineAudioContext = function OfflineAudioContext(nc, len, sr) {
    if (!(this instanceof OfflineAudioContext)) return new OfflineAudioContext(nc, len, sr);
    return mkContext(nc, len, sr, OfflineAudioContext.prototype);
  };
  globalThis.OfflineAudioContext = OfflineAudioContext; window.OfflineAudioContext = OfflineAudioContext;
  var named = ["createOscillator", "createDynamicsCompressor", "createGain", "createAnalyser", "createBiquadFilter", "createScriptProcessor", "createPanner", "createStereoPanner", "createDelay", "createConstantSource", "createChannelMerger", "createChannelSplitter", "createConvolver", "createWaveShaper", "createPeriodicWave"];
  named.forEach(function (m) {
    Object.defineProperty(AudioContext.prototype, m, { value: __native(m, 1, function () {
      var C = { createOscillator: globalThis.OscillatorNode, createDynamicsCompressor: globalThis.DynamicsCompressorNode, createGain: globalThis.GainNode, createAnalyser: globalThis.AnalyserNode, createBiquadFilter: globalThis.BiquadFilterNode, createPanner: globalThis.PannerNode, createStereoPanner: globalThis.StereoPannerNode, createDelay: globalThis.DelayNode, createConstantSource: globalThis.ConstantSourceNode, createConvolver: globalThis.ConvolverNode, createWaveShaper: globalThis.WaveShaperNode, createScriptProcessor: globalThis.ScriptProcessorNode, createChannelMerger: globalThis.AudioNode, createChannelSplitter: globalThis.AudioNode, createPeriodicWave: globalThis.PeriodicWave }[m];
      var n = Object.create(C.prototype);
      __hide(n, "_ctx", this);
      try { __hide(n, "_brand", C.name); } catch (e2) {}
      return n;
    }), writable: true, configurable: true });
  });
  Object.defineProperties(AudioContext.prototype, {
    sampleRate: { get: __native("get sampleRate", 0, function () { return __g(this, "_sr"); }), enumerable: true, configurable: true },
    currentTime: { get: __native("get currentTime", 0, function () { return __g(this, "_time"); }), enumerable: true, configurable: true },
    state: { get: __native("get state", 0, function () { return __g(this, "_state"); }), enumerable: true, configurable: true },
    baseLatency: { get: __native("get baseLatency", 0, function () { return 0.01; }), enumerable: true, configurable: true },
    outputLatency: { get: __native("get outputLatency", 0, function () { return 0; }), enumerable: true, configurable: true },
    length: { get: __native("get length", 0, function () { return __g(this, "_len"); }), enumerable: true, configurable: true },
    destination: { value: null, writable: true, configurable: true },
    listener: { value: null, writable: true, configurable: true },
    oncomplete: { value: null, writable: true, configurable: true },
    resume: { value: __native("resume", 0, function () { __hide(this, "_state", "running"); return Promise.resolve(); }), writable: true, configurable: true },
    suspend: { value: __native("suspend", 0, function () { __hide(this, "_state", "suspended"); return Promise.resolve(); }), writable: true, configurable: true },
    close: { value: __native("close", 0, function () { __hide(this, "_state", "closed"); return Promise.resolve(); }), writable: true, configurable: true },
    startRendering: { value: __native("startRendering", 0, function () {
      var self = this;
      __hide(self, "_state", "running");
      return new Promise(function (resolve) {
        setTimeout(function () {
          var buf = mkBuffer(__g(self, "_nc"), __g(self, "_len"), __g(self, "_sr"));
          var ev = { type: "complete", renderedBuffer: buf, target: self };
          try { self.dispatchEvent(ev); } catch (e) {}
          if (typeof self.oncomplete === "function") { try { self.oncomplete(ev); } catch (e2) { try { if (typeof __fireError === "function") __fireError(e2); } catch (_fe) {} } }
          resolve(buf);
        }, 0);
      });
    }), writable: true, configurable: true },
    decodeAudioData: { value: __native("decodeAudioData", 2, function (b, ok) { var self = this; return new Promise(function (resolve) { var buf = mkBuffer(1, 44100, __g(self, "_sr")); if (ok) ok(buf); resolve(buf); }); }), writable: true, configurable: true },
    createBuffer: { value: __native("createBuffer", 3, function (ch, len, sr) { return mkBuffer(ch, len, sr); }), writable: true, configurable: true },
    getOutputTimestamp: { value: __native("getOutputTimestamp", 0, function () { return { contextTime: __g(this, "_time"), performanceTime: globalThis.performance ? performance.now() : 0 }; }), writable: true, configurable: true },
  });
  // leave the OLD stub bound where scripts already hold it: replace globals wholesale
  window.OfflineAudioContext = OfflineAudioContext;
  window.AudioContext = AudioContext;
})();

// ── SURFACE HARDENING — the probed DOM method surface: native-shaped +
// brand-checked (Chrome behavior: wrong-receiver call throws "Illegal
// invocation"; the functions carry no prototype, native toString, and the
// {length,name} own-property shape — all by construction via __native).
(function () {
  if (typeof __native !== "function") return;
  var done = [];
  function wrapFn(orig, name, kind) {
    for (var i = 0; i < done.length; i++) if (done[i] === orig) return orig;
    var w = __native(name, orig.length || 0, function () {
      var t = this;
      var ok = kind === "any" ? (!!t && typeof t.nodeType === "number") : kind === "doc" ? (!!t && t.nodeType === 9) : (!!t && (t.nodeType === 1 || t.nodeType === 11));
      if (!ok) throw new TypeError("Illegal invocation");
      return orig.apply(t, arguments);
    });
    done.push(w);
    return w;
  }
  try {
    var D = globalThis.Document;
    var docNames = ["createElement","createElementNS","createTextNode","createComment","createDocumentFragment","getElementById","getElementsByClassName","getElementsByTagName","querySelector","querySelectorAll","appendChild","removeChild","insertBefore","replaceChild","contains","hasChildNodes","cloneNode","getElementsByName","importNode","adoptNode","addEventListener","removeEventListener","dispatchEvent"];
    if (D && D.prototype) for (var i = 0; i < docNames.length; i++) {
      var nm = docNames[i], f = D.prototype[nm];
      if (typeof f === "function") { try { Object.defineProperty(D.prototype, nm, { value: wrapFn(f, nm, "doc"), writable: true, configurable: true }); } catch (e) {} }
    }
  } catch (e) {}
  try {
    var EP2 = (__ElemC && __ElemC.prototype) || (globalThis.Element && globalThis.Element.prototype);
    if (EP2) {
      var names = Object.getOwnPropertyNames(EP2);
      for (var j = 0; j < names.length; j++) {
        var n2 = names[j];
        if (n2 === "constructor") continue;
        var d2 = Object.getOwnPropertyDescriptor(EP2, n2);
        if (d2 && typeof d2.value === "function") {
          try { Object.defineProperty(EP2, n2, { value: wrapFn(d2.value, n2, "el"), writable: d2.writable, configurable: d2.configurable, enumerable: d2.enumerable }); } catch (e) {}
        }
      }
    }
  } catch (e) {}
  // HTMLElement.prototype (EP) carries the REAL implementations live pages
  // resolve first (elements hit EP before the Element-level copies), so the
  // whole own method surface gets the same native shape + node brand. The
  // guard accepts every node kind — documents, fragments, shadow roots.
  try {
    if (typeof EP !== "undefined" && EP) {
      var epn = Object.getOwnPropertyNames(EP);
      for (var je = 0; je < epn.length; je++) {
        var eN = epn[je];
        if (eN === "constructor") continue;
        var eD = Object.getOwnPropertyDescriptor(EP, eN);
        if (eD && typeof eD.value === "function") {
          var eNat = false;
          try { eNat = String(Function.prototype.toString.call(eD.value)).indexOf("[native code]") !== -1; } catch (e1) {}
          if (eNat) continue;
          try { Object.defineProperty(EP, eN, { value: wrapFn(eD.value, eN, "any"), writable: eD.writable, configurable: eD.configurable, enumerable: eD.enumerable }); } catch (e2) {}
        }
      }
    }
  } catch (e) {}
  try {
    var N = globalThis.Node;
    if (N && N.prototype) {
      var trio = ["appendChild", "insertBefore", "replaceChild", "removeChild"];
      for (var k = 0; k < trio.length; k++) {
        var tn = trio[k];
        var tf = N.prototype[tn] || (typeof EP !== "undefined" && EP && EP[tn]);
        if (typeof tf === "function") { try { Object.defineProperty(N.prototype, tn, { value: wrapFn(tf, tn, "el"), writable: true, configurable: true }); } catch (e) {} }
      }
    }
  } catch (e) {}
  // name every element class constructed before the naming landed
  try {
    for (var t in TAG_CTORS) { if (TAG_CTORS[t]) __nameTagCtor(TAG_CTORS[t], t); }
  } catch (e) {}
})();

// TRACE: full console mirror to file (the engine's display prints only a tail)
if (__TRACE) {
  try {
    var __logNative = __DCI.log;
    if (typeof __logNative === "function") {
      __DCI.log = function (s) {
        try {
          var L = globalThis.__LOGF = globalThis.__LOGF || [];
          L.push(String(s));
          if (L.length % 50 === 0 && typeof __dumpFile === "function") __dumpFile("logf", L.join(String.fromCharCode(10)));
        } catch (eL) {}
        return __logNative(s);
      };
      __log = __DCI.log;
    }
  } catch (e) {}
}

// TRACE: network-attempt journal (did the widget try a request at all?)
if (__TRACE) {
  try {
    var __netj = function (kind, a, b) {
      try {
        var J = globalThis.__NETJ = globalThis.__NETJ || [];
        J.push("[" + kind + "] " + String(a) + " " + String(b));
        if (J.length > 200) J.shift();
        if (typeof __dumpFile === "function") __dumpFile("netj", J.join(String.fromCharCode(10)));
      } catch (e) {}
    };
    if (globalThis.XMLHttpRequest && XMLHttpRequest.prototype && XMLHttpRequest.prototype.open) {
      var __xopen = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function (m, u) { __netj("XHR-open", m, u); return __xopen.apply(this, arguments); };
      var __xsend = XMLHttpRequest.prototype.send;
      if (__xsend) XMLHttpRequest.prototype.send = function (b) { __netj("XHR-send", __g(this, "_xurl") || "?", (b ? String(b).slice(0, 60) : "-")); return __xsend.apply(this, arguments); };
    }
    if (globalThis.fetch) {
      var __fetch0 = globalThis.fetch;
      globalThis.fetch = function (u, init) { __netj("fetch", (init && init.method) || "GET", typeof u === "string" ? u : (u && u.url)); return __fetch0.apply(this, arguments); };
      window.fetch = globalThis.fetch;
    }
  } catch (e) {}
}

// ── interface parentage + toStringTag — real Chromium shapes for the probe
//    walk (StereoPannerNode < AudioNode < EventTarget < Object, ...). Runs
//    LAST so it wires the final classes every earlier section defined (the
//    Web Audio section rebuilds AudioNode after the bank).
(function () {
  var g = globalThis;
  var chains = [
    ["EventTarget"],
    ["AudioNode", "EventTarget"], ["AudioScheduledSourceNode", "AudioNode"],
    ["BaseAudioContext", "EventTarget"],
    ["AudioContext", "BaseAudioContext"], ["OfflineAudioContext", "BaseAudioContext"],
    ["AudioDestinationNode", "AudioNode"], ["AnalyserNode", "AudioNode"], ["BiquadFilterNode", "AudioNode"],
    ["DelayNode", "AudioNode"], ["DynamicsCompressorNode", "AudioNode"], ["GainNode", "AudioNode"],
    ["OscillatorNode", "AudioScheduledSourceNode"], ["ConstantSourceNode", "AudioScheduledSourceNode"],
    ["PannerNode", "AudioNode"], ["ConvolverNode", "AudioNode"], ["StereoPannerNode", "AudioNode"],
    ["WaveShaperNode", "AudioNode"], ["ScriptProcessorNode", "AudioNode"],
    ["SpeechSynthesis", "EventTarget"], ["SpeechSynthesisUtterance", "EventTarget"],
    ["Crypto"], ["SubtleCrypto"], ["CryptoKey"], ["PeriodicWave"],
    ["AudioBuffer"], ["AudioParam"], ["AudioListener"], ["SpeechSynthesisVoice"],
    // character-data family: the bank ran mk("CharacterData","Node") while
    // window.Node was still aliased to HTMLElement, so text nodes ended up
    // instanceof Element (the CF api.js walker paints them as broken
    // elements). Chrome: Text < CharacterData < Node < EventTarget.
    ["CharacterData", "Node"], ["Text", "CharacterData"], ["Comment", "CharacterData"],
    ["CDATASection", "Text"], ["ProcessingInstruction", "CharacterData"]
  ];
  for (var ci = 0; ci < chains.length; ci++) {
    try {
      var c = g[chains[ci][0]];
      if (typeof c !== "function" || !c.prototype) continue;
      var pn = chains[ci][1];
      if (pn) {
        var p = g[pn];
        if (typeof p === "function" && p.prototype) Object.setPrototypeOf(c.prototype, p.prototype);
      }
      if (!Object.getOwnPropertyDescriptor(c.prototype, Symbol.toStringTag)) {
        Object.defineProperty(c.prototype, Symbol.toStringTag, { value: chains[ci][0], writable: false, enumerable: false, configurable: true });
      }
      if (chains[ci][0] === "Text") { try { Object.defineProperty(c, "name", { value: "Text", configurable: true }); } catch (e2) {} }
    } catch (e) {}
  }
})();

// ── NodeFilter + Node type constants + createNodeIterator/createTreeWalker ──
// (the live CF api.js fingerprints the DOM with createNodeIterator +
// NodeFilter.SHOW_*; the missing NodeFilter killed its requestExtraParams
// reply — the "NodeFilter is not defined" throw in the api's own walker.
// Runs LAST: the bank/Web-Audio sections replace Node and build the final
// document after the earlier sections.)
(function () {
  try {
    var NF = function NodeFilter() {};
    var CONST = {
      SHOW_ALL: 4294967295, SHOW_ELEMENT: 1, SHOW_ATTRIBUTE: 2, SHOW_TEXT: 4,
      SHOW_CDATA_SECTION: 8, SHOW_ENTITY_REFERENCE: 16, SHOW_ENTITY: 32,
      SHOW_PROCESSING_INSTRUCTION: 64, SHOW_COMMENT: 128, SHOW_DOCUMENT: 256,
      SHOW_DOCUMENT_TYPE: 512, SHOW_DOCUMENT_FRAGMENT: 1024, SHOW_NOTATION: 2048,
    };
    for (var k in CONST) { try { Object.defineProperty(NF, k, { value: CONST[k], enumerable: true }); } catch (e) {} }
    globalThis.NodeFilter = NF; window.NodeFilter = NF;
  } catch (e) {}
  try {
    var N = globalThis.Node;
    var NC = { ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4, ENTITY_REFERENCE_NODE: 5, ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7, COMMENT_NODE: 8, DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10, DOCUMENT_FRAGMENT_NODE: 11, NOTATION_NODE: 12 };
    if (N) { for (var k2 in NC) { try { Object.defineProperty(N, k2, { value: NC[k2], enumerable: true }); } catch (e) {} } }
  } catch (e) {}
  try {
    var doc = globalThis.document || (typeof document !== "undefined" ? document : null);
    function tbitOf(n) { return n.nodeType === 1 ? 1 : (n.nodeType === 3 ? 4 : (n.nodeType === 8 ? 128 : (n.nodeType === 9 ? 256 : 0))); }
      if (doc && typeof doc.createNodeIterator !== "function") {
      doc.createNodeIterator = function (root, whatToShow) {
        var st = [root];
        var ws = (whatToShow === undefined ? 4294967295 : whatToShow) >>> 0;
        var it = {
          root: root, whatToShow: whatToShow === undefined ? 4294967295 : whatToShow,
          referenceNode: null, pointerBeforeReferenceNode: true,
          nextNode: function () {
            while (st.length) {
              var n = st.pop();
              if (n.childNodes) { for (var i = n.childNodes.length - 1; i >= 0; i--) st.push(n.childNodes[i]); }
              var tb = tbitOf(n);
              if (ws & tb) { __hide(it, "referenceNode", n); return n; }
            }
            return null;
          },
          previousNode: function () { return null; },
          detach: function () {},
        };
        return it;
      };
    }
    if (doc && typeof doc.createTreeWalker !== "function") {
      doc.createTreeWalker = function (root, whatToShow) {
        var it = doc.createNodeIterator(root, whatToShow);
        var w = { root: root, whatToShow: it.whatToShow, currentNode: root, nextNode: it.nextNode, previousNode: it.previousNode };
        return w;
      };
    }
  } catch (e) {}
})();

// ── ST-4 SURFACE MATERIALIZER — census-exact own-property sets ─────────────
// Runs LAST. Enforces the creepjs prototype-page census surfaces measured
// from the local Chromium 154 oracle (assets/surface_tables.js, generated by
// tools/gen_surface.py):
//   1. creates the interface classes the engine lacks;
//   2. moves flattened implementations to their Chrome-correct prototypes
//      (Node / EventTarget), then deletes the flattened leftovers;
//   3. materializes the missing surface (consts / methods / accessors) with
//      Chrome's descriptor flags;
//   4. re-homes singleton instance state (document / navigator / screen) onto
//      native-shaped prototype accessors — the census reads
//      Object.getOwnPropertyDescriptor(instance, name) for every searched
//      accessor name (getUndefinedValueLie), so a colliding own prop is a lie.
(function () {
  var S = __DCI.SURF;
  if (!S || typeof __native !== "function") return;
  var g = globalThis;

  function protoOf(n) { var C = g[n]; return C && C.prototype ? C.prototype : null; }
  function hasOwn(p, n) { return !!p && Object.prototype.hasOwnProperty.call(p, n); }
  function del(p, n) { try { delete p[n]; } catch (e) {} }
  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }

  // 1 ── create the missing interfaces
  try {
    for (var ci = 0; ci < S.create.length; ci++) {
      var cn = S.create[ci];
      if (typeof g[cn] !== "undefined" && g[cn]) continue;
      var C = (function (inm) {
        return function () {
          if (new.target !== undefined) throw new TypeError("Failed to construct '" + inm + "': Illegal constructor");
          throw new TypeError("Illegal constructor");
        };
      })(cn);
      try { Object.defineProperty(C, "name", { value: cn, configurable: true }); } catch (e) {}
      g[cn] = C; try { window[cn] = C; } catch (e) {}
      if (cn === "MediaDevices" && g.EventTarget && g.EventTarget.prototype) setP(C.prototype, g.EventTarget.prototype);
      try { Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true }); } catch (e) {}
      try { Object.defineProperty(C.prototype, Symbol.toStringTag, { value: cn, configurable: true }); } catch (e) {}
    }
  } catch (e) {}

  // 2 ── receiver guards (wrong receiver throws "Illegal invocation" — the
  // census's illegal-probe reads every accessor on the prototype object).
  // Node-family guards read nodeType as an OWN descriptor only: once
  // Node.prototype.nodeType exists (materialized below), a bare this.nodeType
  // read on a prototype receiver would re-enter that accessor and recurse.
  var NAV = null, SCR = null;
  try { NAV = g.navigator; } catch (e) {}
  try { SCR = g.screen; } catch (e) {}
  // ST-7 wave 3 (same-minute Chrome 154): promise-returning members answer a
  // non-instance receiver with a REJECTED promise naming member+interface
  // ("Failed to execute '<nm>' on '<Iface>': Illegal invocation"), not a sync
  // throw; a receiver that passes the guard returns a promise.
  var PROM7 = {
    "Navigator": { "clearAppBadge": 1, "clearOriginJoinedAdInterestGroups": 1, "createAuctionNonce": 1, "deprecatedReplaceInURN": 1, "deprecatedURNToURL": 1, "getBattery": 1, "getInstalledRelatedApps": 1, "getInterestGroupAdAuctionData": 1, "joinAdInterestGroup": 1, "leaveAdInterestGroup": 1, "requestMIDIAccess": 1, "requestMediaKeySystemAccess": 1, "runAdAuction": 1, "setAppBadge": 1 },
    "Document": { "browsingTopics": 1, "exitFullscreen": 1, "exitPictureInPicture": 1, "hasPrivateToken": 1, "hasRedemptionRecord": 1, "hasStorageAccess": 1, "hasUnpartitionedCookieAccess": 1, "requestStorageAccess": 1 },
    "Element": { "requestFullscreen": 1, "requestPointerLock": 1, "scroll": 1, "scrollBy": 1, "scrollIntoView": 1, "scrollTo": 1 },
    "MediaDevices": { "enumerateDevices": 1, "getDisplayMedia": 1, "getUserMedia": 1 },
    "OffscreenCanvas": { "convertToBlob": 1 },
    "WebGLRenderingContext": { "makeXRCompatible": 1 },
    "WebGL2RenderingContext": { "makeXRCompatible": 1 }
  };
  function ownNT(t) {
    try {
      var d = Object.getOwnPropertyDescriptor(t, "nodeType");
      if (d && typeof d.value === "number") return d.value;
      var h = __g(t, "_sf_nodeType");
      return typeof h === "number" ? h : undefined;
    } catch (e) { return undefined; }
  }
  function guardFor(iface, proto) {
    if (iface === "Navigator") return function () { if (this !== NAV) throw new TypeError("Illegal invocation"); };
    if (iface === "Screen") return function () { if (this !== SCR) throw new TypeError("Illegal invocation"); };
    if (iface === "Document") return function () { if (ownNT(this) !== 9) throw new TypeError("Illegal invocation"); };
    if (iface === "HTMLElement" || iface === "Element" || iface === "Node" || iface === "HTMLIFrameElement" || iface === "HTMLCanvasElement") {
      return function () { if (ownNT(this) === undefined) throw new TypeError("Illegal invocation"); };
    }
    if (iface === "CanvasRenderingContext2D") return function () { __s7k(this, "CanvasRenderingContext2D"); };
    if (iface === "OffscreenCanvasRenderingContext2D") return function () { __s7k(this, "OffscreenCanvasRenderingContext2D"); };
    if (iface === "WebGLRenderingContext") return function () { __s7k(this, "WebGLRenderingContext"); };
    if (iface === "WebGL2RenderingContext") return function () { __s7k(this, "WebGL2RenderingContext"); };
    if (iface === "Range") return function () { __s7k(this, "Range"); };
    if (iface === "CSSStyleDeclaration") return function () { __s7k(this, "CSSStyleDeclaration"); };
    if (iface === "TextMetrics") return function () { __s7k(this, "TextMetrics"); };
    if (iface === "MediaDevices") return function () { __s7k(this, "MediaDevices"); };
    if (iface === "OffscreenCanvas") return function () { __s7k(this, "OffscreenCanvas"); };
    if (iface === "IntersectionObserverEntry") return function () { __s7k(this, "IntersectionObserverEntry"); };
    return function () { if (!this || this === proto) throw new TypeError("Illegal invocation"); };
  }

  // 3 ── move flattened impls to their Chrome level (before the delete pass)
  function findImpl(name) {
    var srcs = [];
    try { if (typeof EP !== "undefined" && EP) srcs.push(EP); } catch (e) {}
    if (__ElemC && __ElemC.prototype) srcs.push(__ElemC.prototype);
    if (g.Document && g.Document.prototype) srcs.push(g.Document.prototype);
    for (var i = 0; i < srcs.length; i++) if (hasOwn(srcs[i], name)) return Object.getOwnPropertyDescriptor(srcs[i], name);
    return null;
  }
  try {
    var Np = protoOf("Node");
    if (Np && S.move && S.move.Node) for (var mi = 0; mi < S.move.Node.length; mi++) {
      var mn = S.move.Node[mi], md = findImpl(mn);
      if (!md) continue;
      try {
        if (md.get || md.set) Object.defineProperty(Np, mn, { get: md.get, set: md.set, enumerable: true, configurable: true });
        else Object.defineProperty(Np, mn, { value: md.value, writable: true, enumerable: true, configurable: true });
      } catch (e) {}
    }
  } catch (e) {}
  // EventTarget trio: fresh bodies over a lazy listener store — deliberately
  // not the node-guarded copies, so non-node targets (e.g. performance, which
  // used to ride the old no-op stub on this prototype) keep working. WH (a):
  // the receiver brand is the chain — EVTp must be in the receiver's
  // prototype chain ({} -> Illegal invocation, Chrome-measured plc; every
  // real target — window/element/document/performance/... — passes). WH (a2,
  // plcg): the chain alone over-accepts — Object.create(EVTp) and
  // Object.create(<instance>) hit or cross EVTp without slots and Chrome
  // throws Illegal invocation. Constructed instances carry the hidden _et7
  // brand (set by the ctor) and are the only direct-proto hit allowed; a
  // branded hop mid-chain means the receiver wraps an instance - rejected.
  function __etOk(t) {
    try {
      if (t === null || t === undefined || t === EVTp) return false;
      if (__g(t, "_et7") === 1) return true;
      var p = Object.getPrototypeOf(t), i = 0;
      if (p === EVTp) return false;
      while (p && i < 8) {
        if (__g(p, "_et7") === 1) return false;
        if (p === EVTp) return true;
        p = Object.getPrototypeOf(p); i++;
      }
    } catch (e) {}
    return false;
  }
  try {
    var EVTp = g.EventTarget && g.EventTarget.prototype;
    if (EVTp) {
      var trio = addEventTarget({});
      var trioNames = ["addEventListener", "removeEventListener", "dispatchEvent"];
      for (var ti = 0; ti < trioNames.length; ti++) {
        (function (tn) {
          var orig = trio[tn];
          var need = tn === "dispatchEvent" ? 1 : 2;
          var f = __native(tn, orig.length || 1, function () {
            if (!__etOk(this)) throw new TypeError("Illegal invocation");
            // WH (a2): Chromium's arity gate, order and text verbatim
            // (measured plcg): receiver first - call({}) with 0 args stays
            // Illegal invocation; call(null) coerces this to window, then
            // this arity TypeError fires.
            if (arguments.length < need) throw new TypeError("Failed to execute '" + tn + "' on 'EventTarget': " + need + " argument" + (need === 1 ? "" : "s") + " required, but only " + arguments.length + " present.");
            return orig.apply(this, arguments);
          });
          try { Object.defineProperty(EVTp, tn, { value: f, writable: true, enumerable: true, configurable: true }); } catch (e) {}
        })(trioNames[ti]);
      }
    }
  } catch (e) {}

  // 4 ── singleton re-home: every searched accessor name the instance still
  // owns moves onto a native-shaped prototype accessor. Existing accessor
  // logic is preserved; data values get a hidden per-instance store seeded
  // with the current value. document.__load() and friends keep writing
  // through the inherited setter (plain assignment), which stores hidden.
  function rehome(iface, inst) {
    var p = protoOf(iface), spec = S.add[iface];
    if (!p || !inst || !spec || !spec.accs) return;
    var G = guardFor(iface, p);
    for (var i = 0; i < spec.accs.length; i++) {
      (function (nm) {
        if (!hasOwn(inst, nm)) return;
        var d = Object.getOwnPropertyDescriptor(inst, nm);
        var get, set;
        if (d.get || d.set) {
          var og = d.get, os = d.set;
          get = __native("get " + nm, 0, function () { G.call(this); var h2 = __g(this, "_sf_" + nm); if (h2 !== undefined) { return h2; } return og ? og.call(this) : undefined; });
          if (os) set = __native("set " + nm, 1, function (v) { G.call(this); return os.call(this, v); });
        } else {
          var seed = d.value;
          get = __native("get " + nm, 0, function () { G.call(this); var h = __g(this, "_rh_" + nm); return h === undefined ? seed : h; });
          set = __native("set " + nm, 1, function (v) { G.call(this); __hide(this, "_rh_" + nm, v === undefined ? seed : v); });
        }
        try { Object.defineProperty(p, nm, { get: get, set: set, enumerable: true, configurable: true }); } catch (e) { return; }
        del(inst, nm);
      })(spec.accs[i][0]);
    }
  }
  try { rehome("Document", g.document); } catch (e) {}
  try { rehome("Navigator", g.navigator); } catch (e) {}
  try { rehome("Screen", g.screen); } catch (e) {}

  // 5 ── additions (consts / methods / accessors), skip anything already own
  function defaultFor(n) {
    if ((n.charCodeAt(0) === 111 && n.charCodeAt(1) === 110) && n !== "onLine") return null;
    if (n.indexOf("aria") === 0) return null;
    var B = { autofocus: false, draggable: false, spellcheck: true, translate: true, inert: false, isContentEditable: false, contentEditable: "inherit", onLine: true, shadowRoot: null };
    return Object.prototype.hasOwnProperty.call(B, n) ? B[n] : "";
  }
  try {
    for (var iface in S.add) {
      var p = protoOf(iface);
      if (!p) continue;
      var spec = S.add[iface], i;
      if (spec.consts) for (i = 0; i < spec.consts.length; i++) {
        var cv = spec.consts[i];
        if (hasOwn(p, cv[0])) continue;
        try {
          Object.defineProperty(p, cv[0], { value: cv[1], enumerable: true, configurable: true, writable: false });
          // ST-11 U2b-4: configurable in passing so the own-order pass can move
          // consts; the tail seal pass restores configurable:false.
          (__DCI.ordSeal || (__DCI.ordSeal = [])).push([p, cv[0]]);
        } catch (e) {}
      }
      var G = guardFor(iface, p);
      if (spec.methods) for (i = 0; i < spec.methods.length; i++) {
        (function (nm, ln, en, Gd, IF, pp) {
          if (hasOwn(p, nm)) return;
          var f = __native(nm, ln, pp ? function () {
            try { Gd.call(this); } catch (e) { return Promise.reject(new TypeError("Failed to execute '" + nm + "' on '" + IF + "': Illegal invocation")); }
            return Promise.resolve();
          } : function () { Gd.call(this); return undefined; });
          try { Object.defineProperty(p, nm, { value: f, writable: true, enumerable: en !== 0, configurable: true }); } catch (e) {}
        })(spec.methods[i][0], spec.methods[i][1], spec.methods[i].length > 2 ? spec.methods[i][2] : 1, G, iface, PROM7[iface] && PROM7[iface][spec.methods[i][0]]);
      }
      if (spec.accs) for (i = 0; i < spec.accs.length; i++) {
        (function (nm, hs, en, Gd, Pp) {
          if (hasOwn(Pp, nm)) return;
          var dflt = defaultFor(nm);
          var get = __native("get " + nm, 0, function () { Gd.call(this); var h = __g(this, "_sf_" + nm); if (__TRACE && nm === "shadowRoot") { try { var __srMsg = "host=" + ((__mr(this, "attrs") && __mr(this, "attrs").id) || __mr(this, "tag") || "?") + " v=" + (h === undefined ? String(dflt) : (h === null ? "null" : "SR#" + (__mr(h, "__srId", "_srId") || "?"))) + " realm=" + __realmT(this) + " stk=" + __stk7(); if ((globalThis.__SRRD = (globalThis.__SRRD || 0) + 1) <= 60) __log("SR-READ " + __srMsg); __srdump("srread", __srMsg); } catch (e) {} } return h === undefined ? dflt : h; });
          var d = { get: get, enumerable: en !== 0, configurable: true };
          if (hs) {
            d.set = __native("set " + nm, 1, function (v) { Gd.call(this); __hide(this, "_sf_" + nm, v); });
          } else {
            // ST-11 U2: no write fallback — Chrome's getter-only names ignore
            // plain assignment (sloppy) / throw (strict); engine writes go to
            // the hidden store via the shims.
          }
          try { Object.defineProperty(Pp, nm, d); } catch (e) {}
        })(spec.accs[i][0], spec.accs[i][1], spec.accs[i][2] !== undefined ? spec.accs[i][2] : 1, G, p);
      }
    }
  } catch (e) {}

  // 6 ── delete the flattened leftovers (after the moves above)
  try {
    for (var dif in S.del) {
      var dp = protoOf(dif);
      if (!dp) continue;
      var dl = S.del[dif];
      for (var di = 0; di < dl.length; di++) del(dp, dl[di]);
    }
  } catch (e) {}

  // 7 ── navigator instance purity: Chrome's navigator carries ZERO own
  // props. The six legacy data methods move onto Navigator.prototype as
  // native-shaped wrappers over the real implementations.
  try {
    var NAVP = protoOf("Navigator");
    var sixNames = ["javaEnabled", "getUserMedia", "webkitGetUserMedia", "sendBeacon", "getBattery", "registerProtocolHandler"];
    var sixLens = { javaEnabled: 0, getUserMedia: 3, webkitGetUserMedia: 3, sendBeacon: 1, getBattery: 0, registerProtocolHandler: 2 };
    if (NAVP && NAV) for (var s6 = 0; s6 < sixNames.length; s6++) {
      (function (nm) {
        if (!hasOwn(NAV, nm)) return;
        var d = Object.getOwnPropertyDescriptor(NAV, nm);
        if (!d || typeof d.value !== "function") return;
        var impl = d.value;
        var f = __native(nm, sixLens[nm], function () {
          if (this !== NAV) {
            if (nm === "getBattery") return Promise.reject(new TypeError("Failed to execute 'getBattery' on 'Navigator': Illegal invocation"));
            throw new TypeError("Illegal invocation");
          }
          return impl.apply(this, arguments);
        });
        try { Object.defineProperty(NAVP, nm, { value: f, writable: true, enumerable: true, configurable: true }); } catch (e) { return; }
        del(NAV, nm);
      })(sixNames[s6]);
    }
  } catch (e) {}

  // 8 ── Navigator constructor, native-shaped (fpCollect reads
  // Navigator.prototype.constructor through Function.prototype.toString).
  try {
    var NAVP2 = protoOf("Navigator");
    if (NAVP2 && typeof g.Navigator === "function") {
      var NAVC = __native("Navigator", 0, function () { throw new TypeError("Illegal constructor"); });
      try { Object.defineProperty(NAVC, "prototype", { value: NAVP2, writable: false, enumerable: false, configurable: false }); } catch (e) {}
      try { Object.defineProperty(NAVP2, "constructor", { value: NAVC, writable: true, enumerable: false, configurable: true }); } catch (e) {}
      g.Navigator = NAVC; try { window.Navigator = NAVC; } catch (e) {}
    }
  } catch (e) {}

  // 9 ── Chrome enumeration order for the two prototypes the navigator-chain
  // walk reads: order is part of the fingerprint.
  function orderProps(p, order) {
    if (!p || !order || !order.length) return;
    var descs = Object.create(null), names = Object.getOwnPropertyNames(p), i, nm;
    for (i = 0; i < names.length; i++) { try { descs[names[i]] = Object.getOwnPropertyDescriptor(p, names[i]); } catch (e) {} }
    for (i = 0; i < names.length; i++) { try { delete p[names[i]]; } catch (e) {} }
    for (i = 0; i < order.length; i++) { nm = order[i]; if (descs[nm]) { try { Object.defineProperty(p, nm, descs[nm]); } catch (e) {} descs[nm] = null; } }
    for (i = 0; i < names.length; i++) { nm = names[i]; if (descs[nm]) { try { Object.defineProperty(p, nm, descs[nm]); } catch (e) {} descs[nm] = null; } }
  }
  try { if (S.order) orderProps(protoOf("Navigator"), S.order.Navigator); } catch (e) {}
  try { if (S.order) orderProps(Object.prototype, S.order.Object); } catch (e) {}

  // 10 ── Document.prototype.createEvent: the real (native) implementation,
  // not the additions stub — inherited calls must behave like chrome's.
  try {
    var DocP = protoOf("Document");
    if (DocP && g.document && hasOwn(g.document, "createEvent")) {
      try { Object.defineProperty(DocP, "createEvent", { value: g.document.createEvent, writable: true, enumerable: true, configurable: true }); } catch (e) {}
    }
  } catch (e) {}

  // 11 ── Element-level geometry: the Element method copy (see "Element-level
  // method sharing" above) predates the DOMRect-shaped rewrite of EP's
  // geometry, and the flattening delete pass then removed EP's only proper
  // copies (Chrome hosts getClientRects / getBoundingClientRect on
  // Element.prototype — never on HTMLElement.prototype). Assert the DOMRect
  // versions on the Element level LAST: creepjs's rects check reads the call
  // result, and the stale empty-array stub failed it (2026-10-18).
  try {
    var ELP = protoOf("Element") || (__ElemC && __ElemC.prototype);
    var okEl = function (t) { return !!t && (t.nodeType === 1 || t.nodeType === 11); };
    if (ELP) {
      try { Object.defineProperty(ELP, "getClientRects", { value: __native("getClientRects", 0, function () { if (!okEl(this)) throw new TypeError("Illegal invocation"); return __mkRectList([g.DOMRect(0, 0, 0, 0)]); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
      try { Object.defineProperty(ELP, "getBoundingClientRect", { value: __native("getBoundingClientRect", 0, function () { if (!okEl(this)) throw new TypeError("Illegal invocation"); return g.DOMRect(0, 0, 0, 0); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    }
  } catch (e) {}
})();

// ── ST-4 §12-§13: constructor bank / event ctors — Chrome-truth call/new ────
// Measured 2026-10-18 (probe-c2/c3/c4/c5, probe-msgs). Every interface ctor is
// a REAL, constructible JS function whose guard mirrors Chrome's exact
// TypeError text; __clearSource (quickjs.c: js_clear_fn_source) strips the
// stored source so Function.prototype.toString renders the native form. The
// global helper is deleted on first use. Shared stub literals (mk / mkCls /
// tagCtor / the materializer's factory) are cleared via any one member — a
// single call clears the family.
(function () {
  var g = globalThis;
  var cs = g.__clearSource;
  try { delete g.__clearSource; } catch (e) {}

  function pleaseMsg(n) { return "Failed to construct '" + n + "': Please use the 'new' operator, this DOM object constructor cannot be called as a function."; }
  function illegalMsg(n) { return "Failed to construct '" + n + "': Illegal constructor"; }
  function argMsg(n, k, got) { return "Failed to construct '" + n + "': " + k + " argument" + (k > 1 ? "s" : "") + " required, but only " + got + " present."; }
  function noProtoW(C) { try { Object.defineProperty(C, "prototype", { writable: false }); } catch (e) {} }
  function setCon(proto, C) { try { Object.defineProperty(proto, "constructor", { value: C, writable: true, enumerable: false, configurable: true }); } catch (e) {} }
  function finish(name) {
    var C = g[name];
    if (typeof C !== "function") return C;
    noProtoW(C);
    if (typeof cs === "function") { try { cs(C); } catch (e) {} }
    return C;
  }
  function buildCtor(name, o) {
    o = o || {};
    var F = function () {
      if (new.target === undefined) {
        if (o.callIllegal) throw new TypeError("Illegal constructor");
        throw new TypeError(pleaseMsg(name));
      }
      if (o.nwIllegal) throw new TypeError(illegalMsg(name));
      if (o.min !== undefined && arguments.length < o.min) throw new TypeError(argMsg(name, o.min, arguments.length));
      if (o.build) return o.build.apply(this, arguments);
    };
    try { Object.defineProperty(F, "name", { value: name, configurable: true }); } catch (e) {}
    try { Object.defineProperty(F, "length", { value: o.len === undefined ? 0 : o.len, configurable: true }); } catch (e) {}
    var proto = o.proto || (g[name] && g[name].prototype) || F.prototype;
    try { Object.defineProperty(F, "prototype", { value: proto, writable: false, enumerable: false, configurable: false }); } catch (e) {}
    setCon(proto, F);
    if (typeof cs === "function") { try { cs(F); } catch (e) {} }
    g[name] = F; try { window[name] = F; } catch (e) {}
    return F;
  }

  // helper: parse a string into an HTML document (DOMParser / parseHTML*).
  // Route top-level nodes the way the HTML parser does: html/head/body
  // wrappers merge into the skeleton, everything else lands in body.
  // parseFragment returns a top-level NODE ARRAY (not a fragment node) —
  // reading `.childNodes` off it silently emptied every parsed document
  // (measured: DataDome's htmlDecode → empty referer/url → interstitial
  // POST rejected 400; g2, 2026-10-18).
  function __parseDoc(src) {
    try {
      var nodes = parseFragment(String(src));
      var d = document.implementation.createHTMLDocument("");
      var kids = !nodes ? [] : (nodes.childNodes ? nodes.childNodes.slice() : nodes.slice());
      var moveInto = function (dst, node) { __nsf(node, "parentNode", dst); __ns(node, "__parent", dst); try { dst.childNodes.push(node); } catch (e) {} };
      for (var i = 0; i < kids.length; i++) {
        var c = kids[i];
        if (c && c.nodeType === 1 && __mr(c, "tag") === "html") {
          for (var j = 0; j < c.childNodes.length; j++) {
            var hc = c.childNodes[j];
            if (hc && hc.nodeType === 1 && __mr(hc, "tag") === "head") { for (var hk = 0; hk < hc.childNodes.length; hk++) moveInto(d.head, hc.childNodes[hk]); }
            else if (hc && hc.nodeType === 1 && __mr(hc, "tag") === "body") { for (var bk = 0; bk < hc.childNodes.length; bk++) moveInto(d.body, hc.childNodes[bk]); }
            else moveInto(d.body, hc);
          }
          continue;
        }
        if (c && c.nodeType === 1 && __mr(c, "tag") === "head") { for (var h2 = 0; h2 < c.childNodes.length; h2++) moveInto(d.head, c.childNodes[h2]); continue; }
        if (c && c.nodeType === 1 && __mr(c, "tag") === "body") { for (var b2 = 0; b2 < c.childNodes.length; b2++) moveInto(d.body, c.childNodes[b2]); continue; }
        moveInto(d.body, c);
      }
      return d;
    } catch (e) { return document.implementation.createHTMLDocument(""); }
  }

  // 12.1 ── constructible interfaces (please-use-new on call)
  try { buildCtor("DOMParser", { len: 0, build: function () { __hide(this, "_s7b", "DOMParser"); } }); } catch (e) {}
  try {
    var DPC = g.DOMParser;
    if (DPC && DPC.prototype) {
      Object.defineProperty(DPC.prototype, "parseFromString", { value: __native("parseFromString", 2, function (str) { if (__g(this, "_s7b") !== "DOMParser") throw new TypeError("Illegal invocation"); return __parseDoc(String(str)); }), writable: true, enumerable: true, configurable: true });
    }
  } catch (e) {}
  try {
    var DocC = buildCtor("Document", { len: 0, build: function () {
      __hide(this, "_sf_nodeType", 9); __hide(this, "_sf_nodeName", "#document");
      __hide(this, "_sf_childNodes", []); __hide(this, "_sf_compatMode", "CSS1Compat"); __hide(this, "_sf_contentType", "application/xml");
      __hide(this, "_sf_URL", "about:blank"); __hide(this, "_sf_documentURI", "about:blank"); __hide(this, "_sf_readyState", "complete");
    } });
    var ds = [["parseHTMLUnsafe", 1], ["parseHTML", 1]];
    for (var di = 0; di < ds.length; di++) {
      (function (nm, ln) {
        try { Object.defineProperty(DocC, nm, { value: __native(nm, ln, function (h) { return __parseDoc(String(h)); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
      })(ds[di][0], ds[di][1]);
    }
  } catch (e) {}
  try {
    var NAVP3 = g.Navigator && g.Navigator.prototype;
    if (NAVP3) buildCtor("Navigator", { callIllegal: true, nwIllegal: true, len: 0, proto: NAVP3 });
  } catch (e) {}
  try {
    var RC = buildCtor("Response", { len: 0, build: function (body, init) {
      init = init || {};
      var st = init.status === undefined ? 200 : (init.status | 0);
      __hide(this, "_r_body", body === undefined || body === null ? "" : String(body));
      __hide(this, "_r_status", st);
      __hide(this, "_r_ok", st >= 200 && st < 300);
      try { __hide(this, "_r_headers", new Headers(init.headers || {})); } catch (e) {}
    } });
    if (RC && RC.prototype) {
      var RP = RC.prototype;
      function respFrom(body, st) {
        var r = Object.create(RP);
        __hide(r, "_r_body", body); __hide(r, "_r_status", st); __hide(r, "_r_ok", st >= 200 && st < 300);
        try { __hide(r, "_r_headers", new Headers()); } catch (e) {}
        return r;
      }
      function rget(nm, fn) { try { Object.defineProperty(RP, nm, { get: __native("get " + nm, 0, function () { return fn(this); }), enumerable: true, configurable: true }); } catch (e) {} }
      Object.defineProperty(RP, "text", { value: __native("text", 0, function () { var b = __g(this, "_r_body"); return Promise.resolve(b === undefined ? "" : b); }), writable: true, enumerable: true, configurable: true });
      Object.defineProperty(RP, "json", { value: __native("json", 0, function () { return Promise.resolve(JSON.parse(__g(this, "_r_body") || "")); }), writable: true, enumerable: true, configurable: true });
      Object.defineProperty(RP, "arrayBuffer", { value: __native("arrayBuffer", 0, function () { var s = __g(this, "_r_body") || ""; var b = new ArrayBuffer(s.length); var v = new Uint8Array(b); for (var i = 0; i < s.length; i++) v[i] = s.charCodeAt(i) & 255; return Promise.resolve(b); }), writable: true, enumerable: true, configurable: true });
      Object.defineProperty(RP, "clone", { value: __native("clone", 0, function () { return respFrom(__g(this, "_r_body") || "", __g(this, "_r_status") === undefined ? 200 : __g(this, "_r_status")); }), writable: true, enumerable: true, configurable: true });
      rget("ok", function (t) { return __g(t, "_r_ok") === undefined ? true : __g(t, "_r_ok"); });
      rget("status", function (t) { return __g(t, "_r_status") === undefined ? 200 : __g(t, "_r_status"); });
      rget("statusText", function () { return ""; });
      rget("url", function () { return ""; });
      rget("type", function () { return "default"; });
      rget("redirected", function () { return false; });
      rget("bodyUsed", function () { return false; });
      rget("headers", function (t) { var h = __g(t, "_r_headers"); return h === undefined ? new Headers() : h; });
      try { Object.defineProperty(RC, "error", { value: __native("error", 0, function () { return respFrom("", 0); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
      try { Object.defineProperty(RC, "json", { value: __native("json", 1, function (data) { return respFrom(JSON.stringify(data), 200); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
      try { Object.defineProperty(RC, "redirect", { value: __native("redirect", 2, function (url, status) { return respFrom("", status === undefined ? 302 : (status | 0)); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    }
  } catch (e) {}
  try {
    var NotifC = buildCtor("Notification", { len: 1, min: 1, build: function (title, options) {
      __hide(this, "_n_title", String(title));
      __hide(this, "_n_body", options && options.body !== undefined ? String(options.body) : "");
    } });
    try { Object.defineProperty(NotifC, "permission", { get: function () { return "default"; }, configurable: true }); } catch (e) {}
    try { Object.defineProperty(NotifC, "maxActions", { value: 2, writable: false, enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(NotifC, "requestPermission", { value: __native("requestPermission", 0, function (cb) { var p = Promise.resolve("default"); if (typeof cb === "function") { try { p.then(cb); } catch (e) {} } return p; }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  } catch (e) {}

  // 12.1b ── ST-7 (measured ctormsg3, Chrome 154): seven classes the bank held
  // as Illegal, rebuilt constructible — exact messages/lengths, proto own-order
  // (constructor last), member flags, instance values.
  function __s7tag(P, nm) { try { Object.defineProperty(P, Symbol.toStringTag, { value: nm, configurable: true }); } catch (e) {} }
  function __s7m(P, nm, ln, fn) { try { Object.defineProperty(P, nm, { value: __native(nm, ln, __s7w(P, fn)), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
  function __s7g(P, nm, fn, setFn) { try { Object.defineProperty(P, nm, { get: __native("get " + nm, 0, __s7w(P, fn)), set: setFn ? __native("set " + nm, 1, __s7w(P, setFn)) : undefined, enumerable: true, configurable: true }); } catch (e) {} }
  function __s7reset(P) {
    var cd = null;
    try { cd = Object.getOwnPropertyDescriptor(P, "constructor"); } catch (e) {}
    try { var ns = Object.getOwnPropertyNames(P); for (var i = 0; i < ns.length; i++) { try { delete P[ns[i]]; } catch (e2) {} } } catch (e) {}
    return cd;
  }
  function __s7ctor(P, cd) { if (cd) { try { Object.defineProperty(P, "constructor", cd); } catch (e) {} } }
  try {
    var XSC = buildCtor("XMLSerializer", { len: 0, build: function () { __hide(this, "_s7b", "XMLSerializer"); } });
    var XP = XSC.prototype; var xcd = __s7reset(XP); __s7B.set(XP, "XMLSerializer");
    __s7m(XP, "serializeToString", 1, function (node) {
      if (arguments.length < 1) throw new TypeError("Failed to execute 'serializeToString' on 'XMLSerializer': 1 argument required, but only 0 present.");
      var s = serializeNode(node, true);
      if (node && node.nodeType === 1) {
        var gt = s.indexOf(">"); var sp = s.indexOf(" ");
        var at = (sp === -1 || gt < sp) ? gt : sp;
        if (at >= 0 && s.slice(0, gt).indexOf("xmlns") === -1) s = s.slice(0, at) + ' xmlns="http://www.w3.org/1999/xhtml"' + s.slice(at);
      }
      return s;
    });
    __s7ctor(XP, xcd); __s7tag(XP, "XMLSerializer");
  } catch (e) {}
  try {
    var FDC = buildCtor("FormData", { len: 0, build: function (form) { __hide(this, "_s7b", "FormData"); __hide(this, "_fd", []); } });
    var FDP = FDC.prototype; var fdcd = __s7reset(FDP); __s7B.set(FDP, "FormData");
    function __s7fdPairs(t) { var a = __g(t, "_fd"); return a === undefined ? [] : a; }
    function __s7fdIter(t, kind) {
      var a = __s7fdPairs(t).slice(); var i = 0; var it = {};
      it.next = function () { if (i >= a.length) return { value: undefined, done: true }; var e = a[i++]; return { value: kind === 0 ? [e[0], e[1]] : (kind === 1 ? e[0] : e[1]), done: false }; };
      try { it[Symbol.iterator] = function () { return this; }; } catch (e) {}
      return it;
    }
    __s7m(FDP, "append", 2, function (n, v) { if (arguments.length < 2) throw new TypeError("Failed to execute 'append' on 'FormData': 2 arguments required, but only " + arguments.length + " present."); __s7fdPairs(this).push([String(n), v]); });
    __s7m(FDP, "delete", 1, function (n) { var a = __s7fdPairs(this); n = String(n); for (var i = a.length - 1; i >= 0; i--) if (a[i][0] === n) a.splice(i, 1); });
    __s7m(FDP, "get", 1, function (n) { var a = __s7fdPairs(this); n = String(n); for (var i = 0; i < a.length; i++) if (a[i][0] === n) return a[i][1]; return null; });
    __s7m(FDP, "getAll", 1, function (n) { var a = __s7fdPairs(this); n = String(n); var o = []; for (var i = 0; i < a.length; i++) if (a[i][0] === n) o.push(a[i][1]); return o; });
    __s7m(FDP, "has", 1, function (n) { var a = __s7fdPairs(this); n = String(n); for (var i = 0; i < a.length; i++) if (a[i][0] === n) return true; return false; });
    __s7m(FDP, "set", 2, function (n, v) { var a = __s7fdPairs(this); n = String(n); var f = false; for (var i = 0; i < a.length; i++) { if (a[i][0] === n) { if (!f) { a[i][1] = v; f = true; } else { a.splice(i, 1); i--; } } } if (!f) a.push([n, v]); });
    __s7m(FDP, "entries", 0, function () { return __s7fdIter(this, 0); });
    __s7m(FDP, "forEach", 1, function (cb, thisArg) { var a = __s7fdPairs(this).slice(); for (var i = 0; i < a.length; i++) cb.call(thisArg, a[i][1], a[i][0], this); });
    __s7m(FDP, "keys", 0, function () { return __s7fdIter(this, 1); });
    __s7m(FDP, "values", 0, function () { return __s7fdIter(this, 2); });
    __s7ctor(FDP, fdcd); __s7tag(FDP, "FormData");
  } catch (e) {}
  try {
    var P2C = buildCtor("Path2D", { len: 0, build: function () { __hide(this, "_s7b", "Path2D"); } });
    var P2P = P2C.prototype; var p2cd = __s7reset(P2P); __s7B.set(P2P, "Path2D");
    var p2m = [["addPath", 1], ["roundRect", 4], ["arc", 5], ["arcTo", 5], ["bezierCurveTo", 6], ["closePath", 0], ["ellipse", 7], ["lineTo", 2], ["moveTo", 2], ["quadraticCurveTo", 4], ["rect", 4]];
    for (var p2i = 0; p2i < p2m.length; p2i++) __s7m(P2P, p2m[p2i][0], p2m[p2i][1], function () {});
    __s7ctor(P2P, p2cd); __s7tag(P2P, "Path2D");
  } catch (e) {}
  try {
    var FRC = buildCtor("FileReader", { len: 0, build: function () { __hide(this, "_fr_state", 0); __hide(this, "_fr_result", null); __hide(this, "_fr_error", null); __hide(this, "_s7b", "FileReader"); } });
    var FRP = FRC.prototype; var frcd = __s7reset(FRP); __s7B.set(FRP, "FileReader");
    function __s7fire(t, nm) { var h = __g(t, "_fr_" + nm); if (typeof h === "function") { try { h.call(t, { type: nm.slice(2), target: t }); } catch (e2) { try { if (typeof __fireError === "function") __fireError(e2); } catch (_fe) {} } } }
    function __s7start(t, kind, blob) {
      __hide(t, "_fr_state", 1);
      var txt = ""; var bt = __blobTxt(blob); if (bt == null) { try { if (blob && blob.__txt !== undefined) bt = String(blob.__txt); } catch (e) {} } txt = bt == null ? "" : String(bt);
      setTimeout(function () {
        var res;
        if (kind === "readAsArrayBuffer") { var b = new ArrayBuffer(txt.length); var v = new Uint8Array(b); for (var i = 0; i < txt.length; i++) v[i] = txt.charCodeAt(i) & 255; res = b; }
        else if (kind === "readAsDataURL") { var b64 = ""; try { b64 = btoa(txt); } catch (e) {} res = "data:" + ((blob && blob.type) || "application/octet-stream") + ";base64," + b64; }
        else res = txt;
        __hide(t, "_fr_state", 2); __hide(t, "_fr_result", res);
        __s7fire(t, "onloadstart"); __s7fire(t, "onprogress"); __s7fire(t, "onload"); __s7fire(t, "onloadend");
      }, 0);
    }
    __s7g(FRP, "readyState", function () { var v = __g(this, "_fr_state"); return v === undefined ? 0 : v; });
    __s7g(FRP, "result", function () { var v = __g(this, "_fr_result"); return v === undefined ? null : v; });
    __s7g(FRP, "error", function () { var v = __g(this, "_fr_error"); return v === undefined ? null : v; });
    var frh = ["onloadstart", "onprogress", "onload", "onabort", "onerror", "onloadend"];
    for (var fri = 0; fri < frh.length; fri++) (function (nm) {
      __s7g(FRP, nm, function () { var v = __g(this, "_fr_" + nm); return v === undefined ? null : v; }, function (v) { __hide(this, "_fr_" + nm, typeof v === "function" ? v : null); });
    })(frh[fri]);
    try { Object.defineProperty(FRP, "EMPTY", { value: 0, writable: false, enumerable: true, configurable: false }); } catch (e) {}
    try { Object.defineProperty(FRP, "LOADING", { value: 1, writable: false, enumerable: true, configurable: false }); } catch (e) {}
    try { Object.defineProperty(FRP, "DONE", { value: 2, writable: false, enumerable: true, configurable: false }); } catch (e) {}
    try { Object.defineProperty(FRC, "EMPTY", { value: 0, writable: false, enumerable: true, configurable: false }); } catch (e) {}
    try { Object.defineProperty(FRC, "LOADING", { value: 1, writable: false, enumerable: true, configurable: false }); } catch (e) {}
    try { Object.defineProperty(FRC, "DONE", { value: 2, writable: false, enumerable: true, configurable: false }); } catch (e) {}
    __s7m(FRP, "abort", 0, function () { if (__g(this, "_fr_state") === 1) { __hide(this, "_fr_state", 2); __s7fire(this, "onabort"); __s7fire(this, "onloadend"); } });
    __s7m(FRP, "readAsArrayBuffer", 1, function (blob) { if (arguments.length < 1) throw new TypeError("Failed to execute 'readAsArrayBuffer' on 'FileReader': 1 argument required, but only 0 present."); __s7start(this, "readAsArrayBuffer", blob); });
    __s7m(FRP, "readAsBinaryString", 1, function (blob) { if (arguments.length < 1) throw new TypeError("Failed to execute 'readAsBinaryString' on 'FileReader': 1 argument required, but only 0 present."); __s7start(this, "readAsBinaryString", blob); });
    __s7m(FRP, "readAsDataURL", 1, function (blob) { if (arguments.length < 1) throw new TypeError("Failed to execute 'readAsDataURL' on 'FileReader': 1 argument required, but only 0 present."); __s7start(this, "readAsDataURL", blob); });
    __s7m(FRP, "readAsText", 1, function (blob) { if (arguments.length < 1) throw new TypeError("Failed to execute 'readAsText' on 'FileReader': 1 argument required, but only 0 present."); __s7start(this, "readAsText", blob); });
    __s7ctor(FRP, frcd); __s7tag(FRP, "FileReader");
  } catch (e) {}
  try {
    // ST-7 (remainder 4, 2026-10-19) — Blob rebuilt as a real constructible
    // class: ZERO own props (hidden store _b_txt/_b_sz/_b_ty), Chrome's exact
    // proto surface (measured same-hour vs Chrome 154, probe instblob):
    // size,type,arrayBuffer,slice,stream,text,bytes,textStream + ctor; every
    // method length 0; promise members reject with "Failed to execute '<nm>'
    // on 'Blob': Illegal invocation"; sync members and accessors throw plain
    // Illegal invocation; Blob members ACCEPT File receivers (File inherits
    // Blob's internal slots — measured: Blob.size on a File = 2).
    function __bBlob(t) { var b; try { b = __g(t, "_s7b"); } catch (e) { b = undefined; } return !(b === "Blob" || b === "File"); }
    function __bm(nm, ln, fn) { try { Object.defineProperty(BLP, nm, { value: __native(nm, ln, function () { if (__bBlob(this)) throw new TypeError("Illegal invocation"); return fn.apply(this, arguments); }), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
    function __bp(nm, fn) { try { Object.defineProperty(BLP, nm, { value: __native(nm, 0, function () { if (__bBlob(this)) return Promise.reject(new TypeError("Failed to execute '" + nm + "' on 'Blob': Illegal invocation")); return fn.apply(this, arguments); }), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
    function __bg(nm, fn) { try { Object.defineProperty(BLP, nm, { get: __native("get " + nm, 0, function () { if (__bBlob(this)) throw new TypeError("Illegal invocation"); return fn.call(this); }), enumerable: true, configurable: true }); } catch (e) {} }
     var BLC = buildCtor("Blob", { len: 0, build: function (parts, opts) { __hide(this, "_s7b", "Blob");
       parts = parts || []; opts = opts || {};
       var ty = opts.type === undefined ? "" : String(opts.type);
       var bin = __blobSet(this, parts, ty);
       try { if (typeof __TRACE !== "undefined" && __TRACE) __log("BLOB bytes=" + bin.length + " type=" + ty + " head=" + JSON.stringify(__blobTxt(this).slice(0, 160))); } catch (e) {}
     } });
    var BLP = BLC.prototype; var blcd = __s7reset(BLP); __s7B.set(BLP, "Blob");
    __bg("size", function () { var v = __g(this, "_b_sz"); return v === undefined ? 0 : v; });
    __bg("type", function () { var v = __g(this, "_b_ty"); return v === undefined ? "" : v; });
     __bp("arrayBuffer", function () { var s = __blobBin(this); var b = new ArrayBuffer(s ? s.length : 0); if (s) new Uint8Array(b).set(s); return Promise.resolve(b); });
     __bm("slice", 0, function (start, end, type) {
       var s = __blobBin(this) || new Uint8Array(0);
       var st = start === undefined ? 0 : Number(start) | 0;
       var en = end === undefined ? s.length : Number(end) | 0;
       if (st < 0) st = Math.max(s.length + st, 0);
       if (en < 0) en = Math.max(s.length + en, 0);
       if (en > s.length) en = s.length;
       if (st > en) st = en;
       return new BLC([s.slice(st, en)], { type: type === undefined ? "" : String(type) });
     });
    __bm("stream", 0, function () { return new g.ReadableStream(); });
    __bp("text", function () { return Promise.resolve(__blobTxt(this) || ""); });
     __bp("bytes", function () { var s = __blobBin(this); return Promise.resolve(s ? new Uint8Array(s) : new Uint8Array(0)); });
    __bm("textStream", 0, function () { return new g.ReadableStream(); });
    __s7ctor(BLP, blcd); __s7tag(BLP, "Blob");
  } catch (e) {}
  try {
    var FC = buildCtor("File", { len: 2, min: 2, build: function (parts, name, opts) { __hide(this, "_s7b", "File");
      opts = opts || {};
      __hide(this, "_f_name", String(name));
      var lm = opts.lastModified !== undefined ? Number(opts.lastModified) : Date.now();
      __hide(this, "_f_lm", lm);
       var ty = opts.type !== undefined ? String(opts.type) : "";
       __blobSet(this, parts || [], ty);
    } });
    var FCP = FC.prototype; var fcd = __s7reset(FCP); __s7B.set(FCP, "File");
    try { if (g.Blob && g.Blob.prototype) Object.setPrototypeOf(FCP, g.Blob.prototype); } catch (e) {}
    __s7g(FCP, "name", function () { var v = __g(this, "_f_name"); return v === undefined ? "" : v; });
    __s7g(FCP, "lastModified", function () { var v = __g(this, "_f_lm"); return v === undefined ? 0 : v; });
    __s7g(FCP, "lastModifiedDate", function () { var v = __g(this, "_f_lm"); return new Date(v === undefined ? 0 : v); });
    __s7g(FCP, "webkitRelativePath", function () { return ""; });
    __s7ctor(FCP, fcd); __s7tag(FCP, "File");
  } catch (e) {}
  try {
    var IDC = buildCtor("ImageData", { len: 2, min: 2, build: function (a, b, c) { __hide(this, "_s7b", "ImageData");
      var w, h, data;
      if (typeof a === "number") { w = a | 0; h = b === undefined ? 0 : b | 0;
        if (!(w > 0) || !(h > 0)) throw new DOMException("Failed to construct 'ImageData': The source width is zero or not a number.", "IndexSizeError");
        data = new Uint8ClampedArray(w * h * 4);
      } else { data = a; w = b === undefined ? 0 : b | 0;
        if (!(w > 0)) throw new DOMException("Failed to construct 'ImageData': The source width is zero or not a number.", "IndexSizeError");
        if (data && (data.length % 4) !== 0) throw new DOMException("Failed to construct 'ImageData': The input data length is not a multiple of 4.", "InvalidStateError");
        h = c === undefined ? (w > 0 && data && data.length ? ((data.length / 4 / w) | 0) : 0) : c | 0;
      }
      __hide(this, "_id_w", w); __hide(this, "_id_h", h); __hide(this, "_id_d", data);
      try { Object.defineProperty(this, "data", { value: data, writable: false, enumerable: true, configurable: true }); } catch (e) {}
    } });
    var IDP = IDC.prototype; var idcd = __s7reset(IDP); __s7B.set(IDP, "ImageData");
    __s7g(IDP, "width", function () { var v = __g(this, "_id_w"); return v === undefined ? 0 : v; });
    __s7g(IDP, "height", function () { var v = __g(this, "_id_h"); return v === undefined ? 0 : v; });
    __s7g(IDP, "colorSpace", function () { return "srgb"; });
    __s7g(IDP, "data", function () { return __g(this, "_id_d"); });
    __s7g(IDP, "pixelFormat", function () { return "rgba-unorm8"; });
    __s7ctor(IDP, idcd); __s7tag(IDP, "ImageData");
  } catch (e) {}
  try {
    var FFC = buildCtor("FontFace", { len: 2, min: 2, build: function (family, source, descriptors) { __hide(this, "_s7b", "FontFace");
      var d = descriptors || {};
      __hide(this, "_ff_family", String(family));
      __hide(this, "_ff_source", typeof source === "string" ? source : String(source));
      __hide(this, "_ff_style", d.style !== undefined ? String(d.style) : "normal");
      __hide(this, "_ff_weight", d.weight !== undefined ? String(d.weight) : "normal");
      __hide(this, "_ff_width", "normal");
      __hide(this, "_ff_stretch", d.stretch !== undefined ? String(d.stretch) : "normal");
      __hide(this, "_ff_unicodeRange", "U+0-10FFFF");
      __hide(this, "_ff_variant", d.variant !== undefined ? String(d.variant) : "normal");
      __hide(this, "_ff_featureSettings", d.featureSettings !== undefined ? String(d.featureSettings) : "normal");
      __hide(this, "_ff_display", d.display !== undefined ? String(d.display) : "auto");
      __hide(this, "_ff_ascentOverride", "normal");
      __hide(this, "_ff_descentOverride", "normal");
      __hide(this, "_ff_lineGapOverride", "normal");
      __hide(this, "_ff_sizeAdjust", "100%");
      __hide(this, "_ff_status", "unloaded");
      __hide(this, "_ff_variationSettings", d.variationSettings !== undefined ? String(d.variationSettings) : "normal");
    } });
    var FFP = FFC.prototype; var ffcd = __s7reset(FFP); __s7B.set(FFP, "FontFace");
    function __s7ffGet(key, dflt) { return function () { var v = __g(this, key); return v === undefined ? dflt : v; }; }
    function __s7ffSet(key) { return function (v) { __hide(this, key, v === null || v === undefined ? "" : String(v)); }; }
    __s7g(FFP, "family", __s7ffGet("_ff_family", ""), __s7ffSet("_ff_family"));
    __s7g(FFP, "style", __s7ffGet("_ff_style", "normal"), __s7ffSet("_ff_style"));
    __s7g(FFP, "weight", __s7ffGet("_ff_weight", "normal"), __s7ffSet("_ff_weight"));
    __s7g(FFP, "width", __s7ffGet("_ff_width", "normal"), __s7ffSet("_ff_width"));
    __s7g(FFP, "stretch", __s7ffGet("_ff_stretch", "normal"), __s7ffSet("_ff_stretch"));
    __s7g(FFP, "unicodeRange", __s7ffGet("_ff_unicodeRange", "U+0-10FFFF"), __s7ffSet("_ff_unicodeRange"));
    __s7g(FFP, "variant", __s7ffGet("_ff_variant", "normal"), __s7ffSet("_ff_variant"));
    __s7g(FFP, "featureSettings", __s7ffGet("_ff_featureSettings", "normal"), __s7ffSet("_ff_featureSettings"));
    __s7g(FFP, "display", __s7ffGet("_ff_display", "auto"), __s7ffSet("_ff_display"));
    __s7g(FFP, "ascentOverride", __s7ffGet("_ff_ascentOverride", "normal"), __s7ffSet("_ff_ascentOverride"));
    __s7g(FFP, "descentOverride", __s7ffGet("_ff_descentOverride", "normal"), __s7ffSet("_ff_descentOverride"));
    __s7g(FFP, "lineGapOverride", __s7ffGet("_ff_lineGapOverride", "normal"), __s7ffSet("_ff_lineGapOverride"));
    __s7g(FFP, "sizeAdjust", __s7ffGet("_ff_sizeAdjust", "100%"), __s7ffSet("_ff_sizeAdjust"));
    __s7g(FFP, "status", __s7ffGet("_ff_status", "unloaded"));
    try { Object.defineProperty(FFP, "loaded", { get: __native("get loaded", 0, function () { if (__g(this, "_s7b") !== "FontFace") return Promise.reject(new TypeError("Failed to read the 'loaded' property from 'FontFace': Illegal invocation")); var p = __g(this, "_ff_loaded"); if (p === undefined) { p = Promise.resolve(this); __hide(this, "_ff_loaded", p); } return p; }), enumerable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(FFP, "load", { value: __native("load", 0, function () { if (__g(this, "_s7b") !== "FontFace") return Promise.reject(new TypeError("Failed to execute 'load' on 'FontFace': Illegal invocation")); __hide(this, "_ff_status", "loaded"); return this.loaded; }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    __s7g(FFP, "variationSettings", __s7ffGet("_ff_variationSettings", "normal"), __s7ffSet("_ff_variationSettings"));
    __s7ctor(FFP, ffcd); __s7tag(FFP, "FontFace");
  } catch (e) {}
  // 12.2 ── native-shaped non-constructible interfaces (source-clear + lock)
  try { if (g.Text) { try { Object.defineProperty(g.Text, "name", { value: "Text", configurable: true }); } catch (e) {} } } catch (e) {}
  try { Object.defineProperty(g.Headers, "length", { value: 0, configurable: true }); } catch (e) {}
  try { Object.defineProperty(g.Request, "length", { value: 1, configurable: true }); } catch (e) {}
  try { if (typeof cs === "function" && typeof TAG_CTORS !== "undefined") { cs(TAG_CTORS.div); cs(TAG_CTORS.canvas); } } catch (e) {}
  try {
    ["Element", "Node", "HTMLElement", "HTMLCanvasElement", "HTMLDivElement", "EventTarget", "Text", "History", "IntersectionObserverEntry", "Location", "MediaDevices", "OffscreenCanvasRenderingContext2D", "Performance", "Permissions", "Screen", "Selection", "SpeechSynthesis", "Storage", "Window", "External", "Headers", "Request", "WebSocket", "Range"].forEach(finish);
  } catch (e) {}
  // ST-5: the new ctor families — lock + clear source; EventTarget parentage
  // once it exists (EventTarget is built by the bank above).
  try { finish("PermissionStatus"); finish("NetworkInformation"); finish("StorageManager"); } catch (e) {}
  // ST-7: the census-gap seven + the Abort/observer family — native toString + lock.
  try { ["XMLSerializer", "FormData", "Path2D", "FileReader", "Blob", "File", "ImageData", "FontFace", "AbortSignal", "AbortController", "MutationObserver", "IntersectionObserver", "ResizeObserver"].forEach(finish); } catch (e) {}
  try {
    var ETpf = g.EventTarget && g.EventTarget.prototype;
    if (ETpf) {
      if (g.PermissionStatus && g.PermissionStatus.prototype) Object.setPrototypeOf(g.PermissionStatus.prototype, ETpf);
      if (g.NetworkInformation && g.NetworkInformation.prototype) Object.setPrototypeOf(g.NetworkInformation.prototype, ETpf);
      if (g.Worker && g.Worker.prototype) Object.setPrototypeOf(g.Worker.prototype, ETpf);
    }
  } catch (e) {}
  try { if (typeof cs === "function" && g.Worker) cs(g.Worker); } catch (e) {}
  try { if (g.Worker) Object.defineProperty(g.Worker, "prototype", { writable: false }); } catch (e) {}

  // 12.3 ── static constants (Chrome append order)
  function statics(C, list) {
    if (typeof C !== "function") return;
    for (var i = 0; i < list.length; i++) {
      try { Object.defineProperty(C, list[i][0], { value: list[i][1], enumerable: true }); } catch (e) {}
    }
  }
  try { statics(g.Node, [["DOCUMENT_POSITION_DISCONNECTED", 1], ["DOCUMENT_POSITION_PRECEDING", 2], ["DOCUMENT_POSITION_FOLLOWING", 4], ["DOCUMENT_POSITION_CONTAINS", 8], ["DOCUMENT_POSITION_CONTAINED_BY", 16], ["DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 32]]); } catch (e) {}
  try { statics(g.Range, [["START_TO_START", 0], ["START_TO_END", 1], ["END_TO_END", 2], ["END_TO_START", 3]]); } catch (e) {}
  try { statics(g.Window, [["TEMPORARY", 0], ["PERSISTENT", 1]]); } catch (e) {}

  // 13 ── event constructors (all real, constructible; Chrome's messages)
  var EVT = [
    ["Event", null],
    ["UIEvent", "Event"], ["MouseEvent", "UIEvent"], ["KeyboardEvent", "UIEvent"],
    ["FocusEvent", "UIEvent"], ["InputEvent", "UIEvent"], ["WheelEvent", "MouseEvent"],
    ["PointerEvent", "MouseEvent"], ["TouchEvent", "UIEvent"], ["DragEvent", "MouseEvent"],
    ["CompositionEvent", "UIEvent"],
    ["MessageEvent", "Event", { msg: true }], ["ErrorEvent", "Event"], ["ProgressEvent", "Event"],
    ["PromiseRejectionEvent", "Event", { min: 2, len: 2 }],
    ["CloseEvent", "Event"], ["StorageEvent", "Event"], ["HashChangeEvent", "Event"],
    ["PopStateEvent", "Event"], ["PageTransitionEvent", "Event"],
    ["BeforeUnloadEvent", "Event", { callIllegal: true, nwIllegal: true, len: 0 }],
    ["AnimationEvent", "Event"], ["TransitionEvent", "Event"], ["ClipboardEvent", "Event"],
    ["CustomEvent", "Event", { detail: true }],
    ["ToggleEvent", "Event"], ["TrackEvent", "Event"], ["SecurityPolicyViolationEvent", "Event"]
  ];
  var EVBUILT = {};
  function touchList() { var o = { length: 0, item: function () { return null; }, identifiedTouch: function () { return null; } }; try { if (window.TouchList && window.TouchList.prototype) Object.setPrototypeOf(o, window.TouchList.prototype); } catch (e) {} return o; }
  function evtBuild(name, o) {
    var F = function (t) {
      if (new.target === undefined) {
        if (o.callIllegal) throw new TypeError("Illegal constructor");
        throw new TypeError(pleaseMsg(name));
      }
      if (o.nwIllegal) throw new TypeError(illegalMsg(name));
      var min = o.min === undefined ? 1 : o.min;
      if (arguments.length < min) throw new TypeError(argMsg(name, min, arguments.length));
      var init = arguments[1];
      var st;
      // U2b-2 perf: base members ride the measured accessor defaults (table
      // dvals); only per-instance identity carriers are minted here (touch
      // lists, message ports). __evInit writes type/timeStamp after this.
      if (o.touch || o.msg) st = __evStore(this);
      if (o.touch) {
        st["_t_touches"] = touchList();
        st["_t_targetTouches"] = touchList();
        st["_t_changedTouches"] = touchList();
        st["_t_altKey"] = false; st["_t_metaKey"] = false;
        st["_t_ctrlKey"] = false; st["_t_shiftKey"] = false;
      }
      if (o.msg) {
        st["_ev_data"] = init && init.data !== undefined ? init.data : null;
        st["_ev_origin"] = init && init.origin !== undefined ? String(init.origin) : "";
        st["_ev_lastEventId"] = init && init.lastEventId !== undefined ? String(init.lastEventId) : "";
        st["_ev_source"] = init && init.source !== undefined ? init.source : null;
        st["_ev_ports"] = init && init.ports !== undefined ? init.ports : [];
      }
    };
    try { Object.defineProperty(F, "name", { value: name, configurable: true }); } catch (e) {}
    try { Object.defineProperty(F, "length", { value: o.len === undefined ? 1 : o.len, configurable: true }); } catch (e) {}
    var proto = (g[name] && g[name].prototype) || {};
    try { Object.defineProperty(F, "prototype", { value: proto, writable: false, enumerable: false, configurable: false }); } catch (e) {}
    setCon(proto, F);
    if (typeof cs === "function") { try { cs(F); } catch (e) {} }
    g[name] = F; try { window[name] = F; } catch (e) {}
    EVBUILT[name] = { F: F, proto: proto };
    return F;
  }
  try {
    var ei;
    for (ei = 0; ei < EVT.length; ei++) {
      var en = EVT[ei][0], eo = EVT[ei][2] || {};
      if (en === "TouchEvent") eo.touch = true;
      if (en === "PromiseRejectionEvent") eo.pr = true;
      evtBuild(en, eo);
    }
    for (ei = 0; ei < EVT.length; ei++) {
      var eb = EVBUILT[EVT[ei][0]], pn = EVT[ei][1];
      if (eb && pn && EVBUILT[pn]) { try { Object.setPrototypeOf(eb.proto, EVBUILT[pn].proto); } catch (e) {} }
    }
    // TouchEvent deep surface: accessor order = Chrome's own-property order
    var TCP = EVBUILT.TouchEvent.proto;
    try { delete TCP.constructor; } catch (e) {}
    function tdef(nm, key, dflt) {
      try {
        Object.defineProperty(TCP, nm, { get: __native("get " + nm, 0, function () { if (!this || this === TCP) throw new TypeError("Illegal invocation"); var v = __g(this, key); return v === undefined ? dflt : v; }), enumerable: true, configurable: true });
      } catch (e) {}
    }
    tdef("touches", "_t_touches", touchList());
    tdef("targetTouches", "_t_targetTouches", touchList());
    tdef("changedTouches", "_t_changedTouches", touchList());
    tdef("altKey", "_t_altKey", false);
    tdef("metaKey", "_t_metaKey", false);
    tdef("ctrlKey", "_t_ctrlKey", false);
    tdef("shiftKey", "_t_shiftKey", false);
    setCon(TCP, EVBUILT.TouchEvent.F);
  } catch (e) {}

  // 12.4 ── BroadcastChannel: Chrome-CONSTRUCTIBLE (the bank stub above is
  // not — the CF challenge VM constructs one mid-round; Chrome serves a real
  // channel). Truth measured 2026-10-18 (ctorcensus.html, Chrome 154):
  // length 1; bare-call + argument-count TypeErrors verbatim; prototype own
  // order [name,onmessage,onmessageerror,close,postMessage,constructor];
  // chain BroadcastChannel>EventTarget>Object; ctor desc w:true/e:false;
  // `name` is a prototype accessor (instances own NOTHING); toStringTag on
  // the prototype; postMessage after close throws InvalidStateError with
  // Chrome's text. Delivery: same-realm same-name channels only (a
  // single-context fetch engine has no cross-tab peers), asynchronous.
  try {
    var __bcReg = {};
    // WH (a): the trio on EventTarget.prototype is the native-shaped, guarded
    // set installed by the §7 materializer — this block used to clobber it
    // with plain addEventTarget copies (measured plc: name/native-shape lost).
    var ETPb = g.EventTarget && g.EventTarget.prototype;
    var BCP = Object.create(ETPb || Object.prototype);
    var BC = function BroadcastChannel(name) {
      if (new.target === undefined) throw new TypeError(pleaseMsg("BroadcastChannel"));
      if (arguments.length < 1) throw new TypeError(argMsg("BroadcastChannel", 1, arguments.length));
      __hide(this, "_bcname", String(name));
      __hide(this, "_bcclosed", false);
      var reg = __bcReg[String(name)];
      if (!reg) { reg = __bcReg[String(name)] = []; }
      reg.push(this);
    };
    try {
      Object.defineProperty(BCP, "name", {
        get: __native("get name", 0, function () { if (!(this instanceof BC)) throw new TypeError("Illegal invocation"); return __g(this, "_bcname", ""); }),
        enumerable: true, configurable: true,
      });
    } catch (e) {}
    function __bcAcc(nm, key) {
      try {
        Object.defineProperty(BCP, nm, {
          get: __native("get " + nm, 0, function () { if (!(this instanceof BC)) throw new TypeError("Illegal invocation"); var v = __g(this, key); return v === undefined ? null : v; }),
          set: __native("set " + nm, 1, function (v) { if (!(this instanceof BC)) throw new TypeError("Illegal invocation"); __hide(this, key, typeof v === "function" ? v : null); }),
          enumerable: true, configurable: true,
        });
      } catch (e) {}
    }
    __bcAcc("onmessage", "_bcom");
    __bcAcc("onmessageerror", "_bome");
    try {
      Object.defineProperty(BCP, "close", { value: __native("close", 0, function () {
        if (!(this instanceof BC)) throw new TypeError("Illegal invocation");
        if (__g(this, "_bcclosed", false)) return;
        __hide(this, "_bcclosed", true);
        var reg = __bcReg[__g(this, "_bcname", "")];
        if (reg) { var i = reg.indexOf(this); if (i >= 0) reg.splice(i, 1); }
      }), writable: true, enumerable: true, configurable: true });
    } catch (e) {}
    try {
      Object.defineProperty(BCP, "postMessage", { value: __native("postMessage", 1, function (data) {
        if (!(this instanceof BC)) throw new TypeError("Illegal invocation");
        if (__g(this, "_bcclosed", false)) {
          var de = null;
          try { de = new DOMException("Failed to execute 'postMessage' on 'BroadcastChannel': Channel is closed", "InvalidStateError"); try { Error.captureStackTrace(de); } catch (e1) {} } catch (e0) { de = null; }
          if (de) throw de;
          throw new TypeError("Failed to execute 'postMessage' on 'BroadcastChannel': Channel is closed");
        }
        var reg = __bcReg[__g(this, "_bcname", "")] || [];
        var self = this;
        try {
          setTimeout(function () {
            for (var i = 0; i < reg.length; i++) {
              var ch = reg[i];
              if (ch === self || __g(ch, "_bcclosed", false)) continue;
              var evt;
              try { evt = new MessageEvent("message", { data: data, source: self }); } catch (eE) { evt = { type: "message", data: data }; }
              if (!evt.type) __evSet(evt, "type", "message");
              __evSet(evt, "target", ch); __evSet(evt, "currentTarget", ch);
              __evSet(evt, "isTrusted", true);
              var h = __g(ch, "_bcom", null);
              if (typeof h === "function") { try { h.call(ch, evt); } catch (eH) {} }
              try { if (typeof ch.dispatchEvent === "function") ch.dispatchEvent(evt); } catch (eD) {}
            }
          }, 0);
        } catch (eT) {}
      }), writable: true, enumerable: true, configurable: true });
    } catch (e) {}
    try { Object.defineProperty(BCP, Symbol.toStringTag, { value: "BroadcastChannel", configurable: true }); } catch (e) {}
    try { Object.defineProperty(BC, "prototype", { value: BCP, writable: false, enumerable: false, configurable: false }); } catch (e) {}
    setCon(BCP, BC);
    if (typeof cs === "function") { try { cs(BC); } catch (e) {} }
    g.BroadcastChannel = BC;
    try { window.BroadcastChannel = BC; } catch (e) {}
  } catch (e) {}
})();

// ── ST-4 §15: singleton instance purity (document / history / performance / storage) ──
// Chrome truth measured 2026-10-18 (probe-c4 inst.*, probe-msgs locOwn/histOwn/
// histProto/perfOwn/perfProto/storOwn/storProto):
//   document own = [location]; history own = []; performance own = []; storage own = [].
(function () {
  var g = globalThis;
  var doc = g.document;
  try {
    var DocP = g.Document && g.Document.prototype;
    function docG(t) { if (t === doc) return; if (!t || t.nodeType !== 9) throw new TypeError("Illegal invocation"); }
    // 15.1 ── move the real method impls onto Document.prototype (native-shaped,
    // doc-guarded; every name already exists on the census surface — value swap).
    var MOVE = [["write", 0], ["writeln", 0], ["open", 0], ["close", 0], ["execCommand", 1], ["hasFocus", 0], ["exitFullscreen", 0], ["adoptNode", 1], ["importNode", 1], ["getSelection", 0], ["elementFromPoint", 2], ["elementsFromPoint", 2], ["getElementsByName", 1], ["createRange", 0], ["createNodeIterator", 1], ["createTreeWalker", 1]];
    for (var i = 0; i < MOVE.length; i++) {
      (function (nm, ln) {
        if (typeof doc[nm] !== "function") return;
        var impl = doc[nm];
        try { Object.defineProperty(DocP, nm, { value: __native(nm, ln, function () { docG(this); return impl.apply(this, arguments); }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
      })(MOVE[i][0], MOVE[i][1]);
    }
    // 15.2 ── hidden seeds for the Node-level data props, then purge every own
    // prop except `location` (Chrome's document carries exactly that one).
    __hide(doc, "_sf_nodeType", 9);
    __hide(doc, "_sf_nodeName", "#document");
    __hide(doc, "_sf_childNodes", doc.childNodes || []);
    __hide(doc, "_sf_doctype", { name: "html", publicId: "", systemId: "", nodeType: 10, nodeName: "html", nodeValue: null, textContent: "", ownerDocument: null, parentNode: null, childNodes: [] });
    try { if (typeof doc.__load === "function") __DCI.dcLoad = doc.__load; } catch (e) {}
    try { if (typeof doc.__serialize === "function") __DCI.dcSerialize = doc.__serialize; } catch (e) {}
    var own = Object.getOwnPropertyNames(doc);
    for (i = 0; i < own.length; i++) { if (own[i] !== "location") { try { delete doc[own[i]]; } catch (e) {} } }
  } catch (e) {}
  // 15.3 ── History: chrome's own list is EMPTY and history has NO event trio
  // (measured: history.addEventListener undefined, History.prototype ->
  // Object.prototype); surface in chrome's order [length, scrollRestoration,
  // state, back, forward, go, pushState, replaceState, constructor].
  try {
    var HP = g.History && g.History.prototype;
    if (HP) {
      try { delete HP.constructor; } catch (e) {}
      function hAcc(nm, key, hs, dflt) {
        var d = { get: __native("get " + nm, 0, function () { if (__g(this, "_s7b") !== "History") throw new TypeError("Illegal invocation"); var v = __g(this, key); return v === undefined ? dflt : v; }), enumerable: true, configurable: true };
        if (hs) d.set = __native("set " + nm, 1, function (v) { if (__g(this, "_s7b") !== "History") throw new TypeError("Illegal invocation"); __hide(this, key, v); });
        return d;
      }
      Object.defineProperty(HP, "length", hAcc("length", "_h_len", false, 1));
      Object.defineProperty(HP, "scrollRestoration", hAcc("scrollRestoration", "_h_scroll", true, "auto"));
      Object.defineProperty(HP, "state", hAcc("state", "_h_state", false, null));
      function hM(nm, ln) { try { Object.defineProperty(HP, nm, { value: __native(nm, ln, function () { if (__g(this, "_s7b") !== "History") throw new TypeError("Illegal invocation"); }), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
      hM("back", 0); hM("forward", 0); hM("go", 0); hM("pushState", 2); hM("replaceState", 2);
      try { Object.defineProperty(HP, "constructor", { value: g.History, writable: true, enumerable: false, configurable: true }); } catch (e) {}
    }
  } catch (e) {}
  // 15.4 ── Performance surface in chrome's order (timeOrigin … interactionCount).
  try {
    var PP = g.Performance && g.Performance.prototype;
    if (PP) {
      try { delete PP.constructor; } catch (e) {}
      function pAcc(nm, fn, hs) {
        var d = { get: __native("get " + nm, 0, function () { if (!this || this === PP) throw new TypeError("Illegal invocation"); return fn(this); }), enumerable: true, configurable: true };
        if (hs) d.set = __native("set " + nm, 1, function (v) { if (!this || this === PP) throw new TypeError("Illegal invocation"); __hide(this, "_p_" + nm, v); });
        return d;
      }
      function pM(nm, ln, fn) { try { Object.defineProperty(PP, nm, { value: __native(nm, ln, function () { if (!this || this === PP) throw new TypeError("Illegal invocation"); return fn ? fn(this) : undefined; }), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
      Object.defineProperty(PP, "timeOrigin", pAcc("timeOrigin", function (t) { var v = __g(t, "_p_to"); return v === undefined ? __DCI.BOOT_MS : v; }, false));
      Object.defineProperty(PP, "onresourcetimingbufferfull", pAcc("onresourcetimingbufferfull", function (t) { var v = __g(t, "_p_onresourcetimingbufferfull"); return v === undefined ? null : v; }, true));
      pM("clearMarks", 0); pM("clearMeasures", 0); pM("clearResourceTimings", 0);
      pM("getEntries", 0, function () { return []; });
      pM("getEntriesByName", 1, function () { return []; });
      pM("getEntriesByType", 1, function () { return []; });
      pM("mark", 1); pM("measure", 1); pM("setResourceTimingBufferSize", 1);
      pM("toJSON", 0, function () { return {}; });
      try { Object.defineProperty(PP, "now", { value: __native("now", 0, function () { if (!this || this === PP) throw new TypeError("Illegal invocation"); return __nowMs() - __DCI.BOOT_MS; }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
      try { Object.defineProperty(PP, "constructor", { value: g.Performance, writable: true, enumerable: false, configurable: true }); } catch (e) {}
      Object.defineProperty(PP, "timing", pAcc("timing", function (t) { return __g(t, "_p_timing"); }, false));
      Object.defineProperty(PP, "navigation", pAcc("navigation", function (t) { return __g(t, "_p_nav"); }, false));
      Object.defineProperty(PP, "memory", pAcc("memory", function (t) { return __g(t, "_p_mem"); }, false));
      Object.defineProperty(PP, "eventCounts", pAcc("eventCounts", function () { return {}; }, false));
      Object.defineProperty(PP, "interactionCount", pAcc("interactionCount", function () { return 0; }, false));
    }
  } catch (e) {}
  // 15.5 ── Storage surface, chrome's order [length, clear, getItem, key,
  // removeItem, setItem, constructor]; per-instance data in the hidden store.
  try {
    var SP = g.Storage && g.Storage.prototype;
    if (SP) {
      try { delete SP.constructor; } catch (e) {}
      function stoData(t) { var d = __g(t, "_st_data"); if (d === undefined) throw new TypeError("Illegal invocation"); return d; }
      Object.defineProperty(SP, "length", { get: __native("get length", 0, function () { if (!this || this === SP) throw new TypeError("Illegal invocation"); return Object.keys(stoData(this)).length; }), enumerable: true, configurable: true });
      function sM(nm, ln, fn) { try { Object.defineProperty(SP, nm, { value: __native(nm, ln, function () { var d = stoData(this); return fn(d, arguments); }), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
      sM("clear", 0, function (d) { for (var k in d) { if (Object.prototype.hasOwnProperty.call(d, k)) delete d[k]; } });
      sM("getItem", 1, function (d, a) { var k = String(a[0]); return Object.prototype.hasOwnProperty.call(d, k) ? d[k] : null; });
      sM("key", 1, function (d, a) { var ks = Object.keys(d); var kv = ks[a[0]]; return kv !== undefined ? kv : null; });
      sM("removeItem", 1, function (d, a) { delete d[String(a[0])]; });
      sM("setItem", 2, function (d, a) { d[String(a[0])] = String(a[1]); });
      try { Object.defineProperty(SP, "constructor", { value: g.Storage, writable: true, enumerable: false, configurable: true }); } catch (e) {}
    }
  } catch (e) {}
})();

// ── ST-4 §16: descriptor exactness — drop the engine-added setters Chrome lacks ──
// Measured (probe-d1, 2026-10-18): 176 accessor names where the engine carried a
// setter while Chrome is getter-only. Redefined in place (position, getter and
// flags preserved) with an explicit undefined setter. The engine's own writes to
// these names convert to hidden stores / direct own props (done above).
(function () {
  var FIX = {
    "CSSStyleDeclaration": ["length", "parentRule"],
    "CanvasRenderingContext2D": ["canvas"],
    "Document": ["URL", "activeElement", "activeViewTransition", "all", "anchors", "applets", "characterSet", "charset", "childElementCount", "children", "compatMode", "contentType", "currentScript", "customElementRegistry", "doctype", "documentElement", "documentURI", "embeds", "featurePolicy", "firstElementChild", "fonts", "forms", "fragmentDirective", "head", "hidden", "images", "implementation", "inputEncoding", "lastElementChild", "lastModified", "links", "pictureInPictureElement", "pictureInPictureEnabled", "plugins", "pointerLockElement", "prerendering", "readyState", "referrer", "rootElement", "scripts", "scrollingElement", "styleSheets", "timeline", "visibilityState", "wasDiscarded", "webkitCurrentFullScreenElement", "webkitFullscreenElement", "webkitFullscreenEnabled", "webkitHidden", "webkitIsFullScreen", "webkitVisibilityState", "xmlEncoding"],
    "Element": ["activeViewTransition", "assignedSlot", "clientHeight", "clientLeft", "clientTop", "clientWidth", "currentCSSZoom", "customElementRegistry", "localName", "namespaceURI", "nextElementSibling", "prefix", "previousElementSibling", "scrollHeight", "scrollWidth", "shadowRoot", "tagName"],
    "FontFace": ["status"],
    "HTMLElement": ["attributeStyleMap", "isContentEditable", "offsetHeight", "offsetLeft", "offsetParent", "offsetTop", "offsetWidth"],
    "HTMLIFrameElement": ["featurePolicy"],
    "IntersectionObserverEntry": ["boundingClientRect", "intersectionRatio", "intersectionRect", "isIntersecting", "isVisible", "rootBounds", "target", "time"],
    "Navigator": ["appCodeName", "appName", "appVersion", "clipboard", "connection", "cookieEnabled", "cpuPerformance", "credentials", "deprecatedRunAdAuctionEnforcesKAnonymity", "deviceMemory", "devicePosture", "doNotTrack", "geolocation", "hardwareConcurrency", "hid", "ink", "keyboard", "language", "languages", "locks", "login", "managed", "maxTouchPoints", "mediaCapabilities", "mediaSession", "mimeTypes", "onLine", "pdfViewerEnabled", "permissions", "platform", "plugins", "presentation", "product", "productSub", "protectedAudience", "scheduling", "serial", "serviceWorker", "storage", "storageBuckets", "usb", "userActivation", "userAgent", "userAgentData", "vendor", "vendorSub", "virtualKeyboard", "wakeLock", "webkitPersistentStorage", "webkitTemporaryStorage", "windowControlsOverlay", "xr"],
    "Node": ["baseURI", "childNodes", "nodeName", "nodeType", "parentNode"],
    "OffscreenCanvasRenderingContext2D": ["canvas"],
    "Range": ["commonAncestorContainer"],
    "Screen": ["availHeight", "availLeft", "availTop", "availWidth", "colorDepth", "height", "isExtended", "orientation", "pixelDepth", "width"],
    "TextMetrics": ["actualBoundingBoxAscent", "actualBoundingBoxDescent", "actualBoundingBoxLeft", "actualBoundingBoxRight", "alphabeticBaseline", "fontBoundingBoxAscent", "fontBoundingBoxDescent", "hangingBaseline", "ideographicBaseline", "width"],
    "WebGL2RenderingContext": ["canvas", "drawingBufferFormat", "drawingBufferHeight", "drawingBufferWidth"],
    "WebGLRenderingContext": ["canvas", "drawingBufferFormat", "drawingBufferHeight", "drawingBufferWidth"]
  };
  for (var iface in FIX) {
    var C = globalThis[iface];
    var P = C && C.prototype;
    if (!P) continue;
    var names = FIX[iface];
    for (var i = 0; i < names.length; i++) {
      try {
        var d = Object.getOwnPropertyDescriptor(P, names[i]);
        if (d && d.set) Object.defineProperty(P, names[i], { set: undefined });
      } catch (e) {}
    }
  }
})();

// ── ST-7 §17: Date receiver semantics — every Date.prototype member answers
// a non-Date receiver with V8's exact TypeError (same-minute Chrome 154):
// getters/valueOf → "this is not a Date object."; setters and string methods
// → "Method Date.prototype.<nm> called on incompatible receiver " + receiver
// ({} renders "#<Object>"); toJSON ducks through this.toISOString;
// toGMTString is the same function object as toUTCString, as in Chrome.
(function () {
  try {
    var DP = Date.prototype;
    var dget = DP.getTime;
    function isD(t) { try { dget.call(t); return true; } catch (e) { return false; } }
    function recv(t) {
      try {
        if (Object.getPrototypeOf(t) === Object.prototype) return "#<Object>";
        return Object.prototype.toString.call(t);
      } catch (e) { return "#<Object>"; }
    }
    function wrapDate(nm, nameOut, msg) {
      var d;
      try { d = Object.getOwnPropertyDescriptor(DP, nm); } catch (e) { return; }
      if (!d || typeof d.value !== "function") return;
      var impl = d.value;
      var f = __native(nameOut, impl.length, function () {
        if (!isD(this)) throw new TypeError(msg(nm, this));
        return impl.apply(this, arguments);
      });
      try { Object.defineProperty(DP, nm, { value: f, writable: d.writable !== false, enumerable: d.enumerable === true, configurable: d.configurable !== false }); } catch (e) {}
    }
    var i;
    var STD = ["valueOf", "getTime", "getDate", "getDay", "getFullYear", "getHours", "getMilliseconds", "getMinutes", "getMonth", "getSeconds", "getTimezoneOffset", "getUTCDate", "getUTCDay", "getUTCFullYear", "getUTCHours", "getUTCMilliseconds", "getUTCMinutes", "getUTCMonth", "getUTCSeconds"];
    var METH = ["getYear", "setYear", "setDate", "setFullYear", "setHours", "setMilliseconds", "setMinutes", "setMonth", "setSeconds", "setTime", "setUTCDate", "setUTCFullYear", "setUTCHours", "setUTCMilliseconds", "setUTCMinutes", "setUTCMonth", "setUTCSeconds", "toString", "toDateString", "toTimeString", "toUTCString", "toISOString", "toLocaleString", "toLocaleDateString", "toLocaleTimeString", "toTemporalInstant"];
    for (i = 0; i < STD.length; i++) wrapDate(STD[i], STD[i], function () { return "this is not a Date object."; });
    for (i = 0; i < METH.length; i++) wrapDate(METH[i], METH[i], function (nm, t) { return "Method Date.prototype." + nm + " called on incompatible receiver " + recv(t); });
    // toGMTString === toUTCString (same function object, name "toUTCString")
    try {
      var dg = Object.getOwnPropertyDescriptor(DP, "toGMTString");
      var du = Object.getOwnPropertyDescriptor(DP, "toUTCString");
      if (dg && du && typeof du.value === "function" && !(typeof dg.value === "function" && dg.value === du.value)) {
        Object.defineProperty(DP, "toGMTString", { value: du.value, writable: dg.writable !== false, enumerable: dg.enumerable === true, configurable: dg.configurable !== false });
      }
    } catch (e) {}
    // toJSON: V8 ducks through this.toISOString
    (function () {
      var d;
      try { d = Object.getOwnPropertyDescriptor(DP, "toJSON"); } catch (e) { return; }
      if (!d || typeof d.value !== "function") return;
      var impl = d.value;
      var f = __native("toJSON", 1, function (k) {
        if (!isD(this)) {
          if (this === null || this === undefined) throw new TypeError("Cannot convert undefined or null to object");
          var o = Object(this);
          var iso = o.toISOString;
          if (typeof iso !== "function") throw new TypeError("toISOString is not a function");
          return iso.apply(o, arguments);
        }
        return impl.apply(this, arguments);
      });
      try { Object.defineProperty(DP, "toJSON", { value: f, writable: d.writable !== false, enumerable: d.enumerable === true, configurable: d.configurable !== false }); } catch (e) {}
    })();
  } catch (e) {}
})();

// ── ST-7 §18: legacy-lenient event handlers — Chrome 154 measured: these
// handler attributes are lenient — a non-instance receiver reads undefined
// and the setter is a no-op instead of "Illegal invocation" (onclick et al.
// stay strict; the materializer's guards already match those). Engine gaps
// measured by ctormacc3x: Document.onmouseenter/onmouseleave/
// onreadystatechange, HTMLElement.onmouseenter/onmouseleave.
(function () {
  function lenientPair(proto, nm, kind) {
    if (!proto) return;
    try { if (Object.prototype.hasOwnProperty.call(proto, nm)) return; } catch (e) { return; }
    function ok(t) {
      try {
        var d = Object.getOwnPropertyDescriptor(t, "nodeType");
        var nt = (d && typeof d.value === "number") ? d.value : __g(t, "_sf_nodeType");
        if (kind === "doc") return nt === 9;
        return typeof nt === "number";
      } catch (e) { return false; }
    }
    var get = __native("get " + nm, 0, function () { if (!ok(this)) return undefined; var h = __g(this, "_sf_" + nm); return h === undefined ? null : h; });
    var set = __native("set " + nm, 1, function (v) { if (!ok(this)) return; __hide(this, "_sf_" + nm, v); });
    try { Object.defineProperty(proto, nm, { get: get, set: set, enumerable: true, configurable: true }); } catch (e) {}
  }
  if (__TRACE) { try { var __d18 = []; try { __d18.push("gD:" + (g && g.Document ? "Y" : "N")); } catch (e) { __d18.push("gRef"); } var __Dc = globalThis.Document && globalThis.Document.prototype; __d18.push("Dc:" + (!!__Dc)); if (__Dc) { __d18.push("len:" + Object.getOwnPropertyNames(__Dc).length); __d18.push("has:" + Object.prototype.hasOwnProperty.call(__Dc, "onmouseenter")); __d18.push("ext:" + Object.isExtensible(__Dc)); } __DCI.s18 = __d18.join("|"); } catch (e) {} }
  try {
    var D7 = globalThis.Document && globalThis.Document.prototype;
    if (D7) { lenientPair(D7, "onmouseenter", "doc"); lenientPair(D7, "onmouseleave", "doc"); lenientPair(D7, "onreadystatechange", "doc"); }
  } catch (e) {}
  try {
    var H7 = globalThis.HTMLElement && globalThis.HTMLElement.prototype;
    if (H7) { lenientPair(H7, "onmouseenter", "el"); lenientPair(H7, "onmouseleave", "el"); }
  } catch (e) {}
})();

// ── ST-7 §19: hasOwn-skipped promise members — Document.exitFullscreen and
// Element.scrollIntoView already carry engine impls (a moved Document method
// and the EP scroll stub), so the §5 promise flavor never applied. Chrome
// answers a non-instance receiver with the same rejected promise as the rest
// of the promise-class surface.
(function () {
  function promWrap(proto, nm, IF) {
    if (!proto) return;
    var d;
    try { d = Object.getOwnPropertyDescriptor(proto, nm); } catch (e) { return; }
    if (!d || typeof d.value !== "function") return;
    var impl = d.value;
    var f = __native(nm, impl.length, function () {
      try { impl.apply(this, arguments); } catch (e) {
        if (e instanceof TypeError && String(e.message) === "Illegal invocation") return Promise.reject(new TypeError("Failed to execute '" + nm + "' on '" + IF + "': Illegal invocation"));
        throw e;
      }
      return Promise.resolve();
    });
    try { Object.defineProperty(proto, nm, { value: f, writable: d.writable !== false, enumerable: d.enumerable === true, configurable: d.configurable !== false }); } catch (e) {}
  }
  try { promWrap(globalThis.Document && globalThis.Document.prototype, "exitFullscreen", "Document"); } catch (e) {}
  try { promWrap(globalThis.Element && globalThis.Element.prototype, "scrollIntoView", "Element"); } catch (e) {}
})();

// ── ST-7 §20: Selection materialization — the last measured surface gap.
// Chrome 154 truth (sel.html / wsel.html, same-hour): 29 proto members in
// order (12 getter-only accessors, then 17 methods — all e:true); the ctor was
// already bank-built; ONE per-realm Selection object, zero own props
// ([object Selection]); window.getSelection is an OWN data prop on window (not
// on Window.prototype); document.getSelection is a distinct fn returning the
// same object; receiver checks strict (proto/{}/Object.create(proto) ->
// "Illegal invocation"); exact errors: addRange arity + "parameter 1 is not of
// type 'Range'", getRangeAt IndexSizeError over a uint32 index, removeRange
// NotFoundError, collapse IndexSizeError, extend InvalidStateError on empty,
// caret direction "none".
(function () {
  var g = globalThis;
  try {
    var C = g.Selection;
    if (!C || !C.prototype) return;
    var P = C.prototype;
    function S7(s) { return __g(s, "_selst"); }
    function mkRange7(an, ao, fn, fo) {
      var r;
      try { r = __mkRange(); } catch (e) { return null; }
      __hide(r, "_start", an); __hide(r, "_end", fn);
      __hide(r, "_so", ao); __hide(r, "_eo", fo);
      __hide(r, "_sf_collapsed", ao === fo);
      __hide(r, "_sf_startOffset", ao); __hide(r, "_sf_endOffset", fo);
      __hide(r, "_sf_startContainer", an); __hide(r, "_sf_endContainer", fn);
      __hide(r, "_sf_commonAncestorContainer", an);
      return r;
    }
    function off7(r, which) {
      var v = __g(r, which);
      if (typeof v === "number") return v;
      if (which === "_eo") { try { var re7 = __g(r, "_end"); return re7 && re7.childNodes ? re7.childNodes.length : 0; } catch (e) { return 0; } }
      return 0;
    }
    function kids7(n) { try { return n && n.childNodes ? n.childNodes.length : 0; } catch (e) { return 0; } }
    function offR7(n) { try { var c = n && n.parentNode && n.parentNode.childNodes; if (!c) return 0; for (var i = 0; i < c.length; i++) if (c[i] === n) return i; return 0; } catch (e) { return 0; } }
    function syncFirst(s) {
      var r = s.ranges[0];
      if (!r) { s.an = null; s.ao = 0; s.fn = null; s.fo = 0; s.dir = "forward"; return; }
      s.an = __g(r, "_start") || null; s.ao = off7(r, "_so");
      s.fn = __g(r, "_end") || null; s.fo = off7(r, "_eo");
      s.dir = (s.an === s.fn) ? (s.ao <= s.fo ? "forward" : "backward") : "forward";
    }
    function clr7(s) { s.ranges = []; s.an = null; s.ao = 0; s.fn = null; s.fo = 0; s.dir = "forward"; }
    function coll7(s) { return !s.ranges.length || (s.an === s.fn && s.ao === s.fo); }
    var SEL = Object.create(P);
    __hide(SEL, "_s7b", "Selection");
    __hide(SEL, "_selst", { ranges: [], an: null, ao: 0, fn: null, fo: 0, dir: "forward" });
    __SEL7 = SEL;

    function acc(nm, fn) {
      try { Object.defineProperty(P, nm, { get: __native("get " + nm, 0, function () { __s7k(this, "Selection"); return fn.call(this); }), enumerable: true, configurable: true }); } catch (e) {}
    }
    acc("anchorNode", function () { var s = S7(this); return s && s.an ? s.an : null; });
    acc("anchorOffset", function () { var s = S7(this); return s ? s.ao : 0; });
    acc("focusNode", function () { var s = S7(this); return s && s.fn ? s.fn : null; });
    acc("focusOffset", function () { var s = S7(this); return s ? s.fo : 0; });
    acc("isCollapsed", function () { var s = S7(this); return s ? coll7(s) : true; });
    acc("rangeCount", function () { var s = S7(this); return s ? s.ranges.length : 0; });
    acc("type", function () { var s = S7(this); if (!s || !s.ranges.length) return "None"; return (s.an === s.fn && s.ao === s.fo) ? "Caret" : "Range"; });
    acc("direction", function () { var s = S7(this); if (!s || !s.ranges.length) return "none"; if (s.an === s.fn && s.ao === s.fo) return "none"; return s.dir || "forward"; });
    acc("baseNode", function () { var s = S7(this); return s && s.an ? s.an : null; });
    acc("baseOffset", function () { var s = S7(this); return s ? s.ao : 0; });
    acc("extentNode", function () { var s = S7(this); return s && s.fn ? s.fn : null; });
    acc("extentOffset", function () { var s = S7(this); return s ? s.fo : 0; });

    var NEED7 = { addRange: 1, collapse: 1, collapseToEnd: 0, collapseToStart: 0, containsNode: 1, deleteFromDocument: 0, empty: 0, extend: 1, getComposedRanges: 0, getRangeAt: 1, modify: 0, removeAllRanges: 0, removeRange: 1, selectAllChildren: 1, setBaseAndExtent: 4, setPosition: 1, toString: 0 };
    function m(nm, ln, fn) {
      try { Object.defineProperty(P, nm, { value: __native(nm, ln, function () {
        __s7k(this, "Selection");
        var need = NEED7[nm] || 0;
        if (arguments.length < need) throw new TypeError("Failed to execute '" + nm + "' on 'Selection': " + need + " argument" + (need === 1 ? "" : "s") + " required, but only " + arguments.length + " present.");
        return fn.apply(this, arguments);
      }), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    }
    m("addRange", 1, function (r) {
      var s = S7(this);
      if (!r || __g(r, "_s7b") !== "Range") throw new TypeError("Failed to execute 'addRange' on 'Selection': parameter 1 is not of type 'Range'.");
      if (s.ranges.indexOf(r) < 0) s.ranges.push(r);
      syncFirst(s);
    });
    m("collapse", 1, function (node, offset) {
      var s = S7(this);
      if (node === null || node === undefined) { clr7(s); return; }
      var o = Number(offset) || 0;
      if (o > kids7(node)) throw new DOMException("Failed to execute 'collapse' on 'Selection': There is no child at offset " + o + ".", "IndexSizeError");
      s.ranges = [mkRange7(node, o, node, o)];
      s.an = node; s.ao = o; s.fn = node; s.fo = o; s.dir = "forward";
    });
    m("collapseToEnd", 0, function () {
      var s = S7(this);
      if (!s.ranges.length) return;
      var r = s.ranges[0];
      var a = __g(r, "_end"), o = off7(r, "_eo");
      s.ranges = [mkRange7(a, o, a, o)];
      s.an = a; s.ao = o; s.fn = a; s.fo = o; s.dir = "forward";
    });
    m("collapseToStart", 0, function () {
      var s = S7(this);
      if (!s.ranges.length) return;
      var r = s.ranges[0];
      var f = __g(r, "_start"), o = off7(r, "_so");
      s.ranges = [mkRange7(f, o, f, o)];
      s.an = f; s.ao = o; s.fn = f; s.fo = o; s.dir = "forward";
    });
    m("containsNode", 1, function (n, partial) {
      var s = S7(this);
      if (!n || !n.nodeType) return false;
      for (var i = 0; i < s.ranges.length; i++) {
        var r = s.ranges[i];
        var rs7 = __g(r, "_start");
        if (!rs7) continue;
        var p = n;
        while (p && p.parentNode && p.parentNode !== rs7) p = p.parentNode;
        if (p && p.parentNode === rs7) {
          var ix = offR7(p);
          if (ix >= off7(r, "_so") && (ix + 1) <= off7(r, "_eo")) return true;
        } else if (n === rs7 && partial) { return true; }
      }
      return false;
    });
    m("deleteFromDocument", 0, function () {
      var s = S7(this);
      if (!s.ranges.length) return;
      var r = s.ranges[0];
      try {
        var rs7 = __g(r, "_start"), re7 = __g(r, "_end");
        if (rs7 && rs7 === re7 && rs7.childNodes) {
          var so = off7(r, "_so"), eo = off7(r, "_eo");
          if (eo > so) rs7.childNodes.splice(so, eo - so);
          s.ranges = [mkRange7(rs7, so, rs7, so)];
          s.an = rs7; s.ao = so; s.fn = rs7; s.fo = so; s.dir = "forward";
        }
      } catch (e) {}
    });
    m("empty", 0, function () { clr7(S7(this)); });
    m("extend", 2, function (node, offset) {
      var s = S7(this);
      if (!s.ranges.length) throw new DOMException("Failed to execute 'extend' on 'Selection': This Selection object doesn't have any Ranges.", "InvalidStateError");
      var o = Number(offset) || 0;
      s.fn = node; s.fo = o;
      if (s.an === s.fn) {
        var lo = s.ao <= s.fo ? s.ao : s.fo, hi = s.ao <= s.fo ? s.fo : s.ao;
        s.dir = s.ao <= s.fo ? "forward" : "backward";
        s.ranges = [mkRange7(s.an, lo, s.fn, hi)];
      } else { s.dir = "forward"; }
    });
    m("getComposedRanges", 0, function () { var s = S7(this); return s.ranges.slice(); });
    m("getRangeAt", 1, function (i) {
      var s = S7(this);
      var ix = i >>> 0;
      if (!(ix < s.ranges.length)) throw new DOMException("Failed to execute 'getRangeAt' on 'Selection': " + ix + " is not a valid index.", "IndexSizeError");
      return s.ranges[ix];
    });
    m("modify", 0, function () {});
    m("removeAllRanges", 0, function () { clr7(S7(this)); });
    m("removeRange", 1, function (r) {
      var s = S7(this);
      var ix = s.ranges.indexOf(r);
      if (ix < 0) throw new DOMException("Failed to execute 'removeRange' on 'Selection': Range not found.", "NotFoundError");
      s.ranges.splice(ix, 1);
      syncFirst(s);
    });
    m("selectAllChildren", 1, function (n) {
      var s = S7(this);
      var k = kids7(n);
      s.ranges = [mkRange7(n, 0, n, k)];
      s.an = n; s.ao = 0; s.fn = n; s.fo = k; s.dir = "forward";
    });
    m("setBaseAndExtent", 4, function (an, ao, fn, fo) {
      var s = S7(this);
      var a = Number(ao) || 0, f = Number(fo) || 0;
      s.an = an; s.ao = a; s.fn = fn; s.fo = f;
      s.dir = (an === fn) ? (a <= f ? "forward" : "backward") : "forward";
      if (an === fn) { var lo = a <= f ? a : f, hi = a <= f ? f : a; s.ranges = [mkRange7(an, lo, fn, hi)]; }
      else s.ranges = [mkRange7(an, a, fn, f)];
    });
    m("setPosition", 1, function (node, offset) {
      var s = S7(this);
      if (node === null || node === undefined) { clr7(s); return; }
      var o = Number(offset) || 0;
      if (o > kids7(node)) throw new DOMException("Failed to execute 'setPosition' on 'Selection': There is no child at offset " + o + ".", "IndexSizeError");
      s.ranges = [mkRange7(node, o, node, o)];
      s.an = node; s.ao = o; s.fn = node; s.fo = o; s.dir = "forward";
    });
    m("toString", 0, function () {
      var s = S7(this);
      if (!s.ranges.length || coll7(s)) return "";
      var r = s.ranges[0];
      var rs7 = __g(r, "_start");
      if (!rs7) return "";
      var v = rs7.textContent;
      return v == null ? "" : String(v);
    });

    // Symbol.toStringTag (Chrome: {value:"Selection", w:false, e:false, c:true})
    try { Object.defineProperty(P, Symbol.toStringTag, { value: "Selection", writable: false, enumerable: false, configurable: true }); } catch (e) {}

    // ctor LAST in own order (Chrome): capture, delete, re-add
    try {
      var dC = Object.getOwnPropertyDescriptor(P, "constructor");
      if (dC) {
        try { delete P.constructor; } catch (e) {}
        Object.defineProperty(P, "constructor", { value: dC.value, writable: dC.writable !== false, enumerable: dC.enumerable === true, configurable: dC.configurable !== false });
      }
    } catch (e) {}

    // window.getSelection: own data prop on window (measured w/e/c true);
    // receiver must be the window (proto-of-globalThis check lets frame-window
    // objects pass, {} and Object.prototype-based fakes fail).
    try {
      var W = __native("getSelection", 0, function () {
        var ok = this === g;
        if (!ok) { try { ok = !!this && typeof this === "object" && Object.getPrototypeOf(this) === Object.getPrototypeOf(g); } catch (e0) {} }
        if (!ok) throw new TypeError("Illegal invocation");
        return __SEL7;
      });
      Object.defineProperty(g, "getSelection", { value: W, writable: true, enumerable: true, configurable: true });
    } catch (e) {}
  } catch (e) {}
})();

// ── ST-7 §21: the Range chain — Chrome 154 measured: Range.prototype's
// [[Prototype]] is an interposed legacy "NodeRange" prototype (owns
// startContainer/endContainer), whose own prototype is AbstractRange (owns
// startOffset/endOffset/collapsed), whose own prototype is Object.prototype.
// AbstractRange is an own window data prop {w:true, e:false, c:true}; ctor
// msgs "Illegal constructor" / "Failed to construct 'AbstractRange': Illegal
// constructor"; accessors getter-only e:true c:true, names "get <nm>", guard
// "Illegal invocation"; instance values read the hidden _sf_* store.
(function () {
  var g = globalThis;
  try {
    if (g.AbstractRange || !g.Range || !g.Range.prototype) return;
    var R = g.Range;
    var AR = function () {
      if (new.target === undefined) throw new TypeError("Illegal constructor");
      throw new TypeError("Failed to construct 'AbstractRange': Illegal constructor");
    };
    try { Object.defineProperty(AR, "name", { value: "AbstractRange", configurable: true }); } catch (e) {}
    try { Object.defineProperty(AR, "length", { value: 0, configurable: true }); } catch (e) {}
    var ARp = {};
    try { Object.defineProperty(AR, "prototype", { value: ARp, writable: false, enumerable: false, configurable: false }); } catch (e) {}
    function defAcc(proto, nm, getter) {
      try { Object.defineProperty(proto, nm, { get: getter, enumerable: true, configurable: true }); } catch (e) {}
    }
    defAcc(ARp, "startOffset", __native("get startOffset", 0, function () { __s7k(this, "Range"); var v = __g(this, "_sf_startOffset"); return typeof v === "number" ? v : 0; }));
    defAcc(ARp, "endOffset", __native("get endOffset", 0, function () { __s7k(this, "Range"); var v = __g(this, "_sf_endOffset"); return typeof v === "number" ? v : 0; }));
    defAcc(ARp, "collapsed", __native("get collapsed", 0, function () { __s7k(this, "Range"); var v = __g(this, "_sf_collapsed"); return typeof v === "boolean" ? v : true; }));
    try { Object.defineProperty(ARp, "constructor", { value: AR, writable: true, enumerable: false, configurable: true }); } catch (e) {}
    try { Object.defineProperty(ARp, Symbol.toStringTag, { value: "AbstractRange", writable: false, enumerable: false, configurable: true }); } catch (e) {}
    var NR = function () {
      if (new.target === undefined) throw new TypeError("Illegal constructor");
      throw new TypeError("Failed to construct 'NodeRange': Illegal constructor");
    };
    try { Object.defineProperty(NR, "name", { value: "NodeRange", configurable: true }); } catch (e) {}
    try { Object.defineProperty(NR, "length", { value: 0, configurable: true }); } catch (e) {}
    var NRp = Object.create(ARp);
    try { Object.defineProperty(NR, "prototype", { value: NRp, writable: false, enumerable: false, configurable: false }); } catch (e) {}
    defAcc(NRp, "startContainer", __native("get startContainer", 0, function () { __s7k(this, "Range"); var v = __g(this, "_sf_startContainer"); return v === undefined ? null : v; }));
    defAcc(NRp, "endContainer", __native("get endContainer", 0, function () { __s7k(this, "Range"); var v = __g(this, "_sf_endContainer"); return v === undefined ? null : v; }));
    try { Object.defineProperty(NRp, "constructor", { value: NR, writable: true, enumerable: false, configurable: true }); } catch (e) {}
    try { Object.defineProperty(NRp, Symbol.toStringTag, { value: "NodeRange", writable: false, enumerable: false, configurable: true }); } catch (e) {}
    try { Object.setPrototypeOf(R.prototype, NRp); } catch (e) {}
    try { Object.defineProperty(g, "AbstractRange", { value: AR, writable: true, enumerable: false, configurable: true }); } catch (e) {}
  } catch (e) {}
})();

// ── WH: re-export the page-facing surface a real browser exposes on window,

// ── WH: re-export the page-facing surface a real browser exposes on window,
// plus the host bridge names. Anything not listed here stays inside the closure.
try { globalThis.window = globalThis; } catch (e) { try { __log("WH-MISS window"); } catch (e1) {} }
try { globalThis.document = document; } catch (e) { try { __log("WH-MISS document " + e); } catch (e1) {} }
try { globalThis.setTimeout = setTimeout; } catch (e) { try { __log("WH-MISS setTimeout " + e); } catch (e1) {} }
try { globalThis.clearTimeout = clearTimeout; } catch (e) { try { __log("WH-MISS clearTimeout " + e); } catch (e1) {} }
try { globalThis.setInterval = setInterval; } catch (e) { try { __log("WH-MISS setInterval " + e); } catch (e1) {} }
try { globalThis.clearInterval = clearInterval; } catch (e) { try { __log("WH-MISS clearInterval " + e); } catch (e1) {} }
try { globalThis.requestAnimationFrame = requestAnimationFrame; } catch (e) { try { __log("WH-MISS requestAnimationFrame " + e); } catch (e1) {} }
try { globalThis.cancelAnimationFrame = cancelAnimationFrame; } catch (e) { try { __log("WH-MISS cancelAnimationFrame " + e); } catch (e1) {} }
try { globalThis.queueMicrotask = __native("queueMicrotask", 1, queueMicrotask); } catch (e) { try { __log("WH-MISS queueMicrotask " + e); } catch (e1) {} }
try { globalThis.XMLHttpRequest = XMLHttpRequest; } catch (e) { try { __log("WH-MISS XMLHttpRequest " + e); } catch (e1) {} }
try { globalThis.Headers = Headers; } catch (e) { try { __log("WH-MISS Headers " + e); } catch (e1) {} }
try { globalThis.Worker = Worker; } catch (e) { try { __log("WH-MISS Worker " + e); } catch (e1) {} }
try { globalThis.WebSocket = WebSocket; } catch (e) { try { __log("WH-MISS WebSocket " + e); } catch (e1) {} }
try { globalThis.OffscreenCanvas = OffscreenCanvas; } catch (e) { try { __log("WH-MISS OffscreenCanvas " + e); } catch (e1) {} }
try { globalThis.HTMLElement = HTMLElement; } catch (e) { try { __log("WH-MISS HTMLElement " + e); } catch (e1) {} }
// AudioContext / OfflineAudioContext are NOT re-exported here: the rich Web
// Audio classes are exported by their own section, while the closure-scope
// names at this point are the v0.1 stubs — re-exporting them clobbered the
// rich classes (creepjs audio regression, caught by the ST-10 re-proof).
// WH (a): the event trio and the toStringTag are inherited — window ->
// Window.prototype ("Window") -> WindowProperties.prototype ->
// EventTarget.prototype — as in Chrome (measured plc). The boot-time own
// copies must not survive on the page window.
try { delete globalThis.addEventListener; } catch (e) {}
try { delete globalThis.removeEventListener; } catch (e) {}
try { delete globalThis.dispatchEvent; } catch (e) {}
try { delete globalThis[Symbol.toStringTag]; } catch (e) {}

// QuickJS built-ins that a real Chrome does not expose on window (V8 has no
// InternalError; SharedArrayBuffer is COI-gated in Chrome and absent in the
// oracle). Removed from the page surface; the engine never constructs them.
try { delete globalThis.InternalError; } catch (e) {}
try { delete globalThis.SharedArrayBuffer; } catch (e) {}
// ── WH (d) window enumerability parity (measured vs Chromium 154): the boot
// materialization left ~230 interface constructors ENUMERABLE on the page
// window while Chrome owns them non-enumerable, and 169 names Chrome owns as
// enumerable were missing. After this block Object.keys(window) equals
// Chrome's set (239). Value internals of the added API stubs are honest
// approximations; kinds/names/lengths/descriptors are measured-exact.
(function () {
  function defNE(nm) { try { Object.defineProperty(window, nm, { enumerable: false }); } catch (e) {} }
  function defEN(nm) { try { Object.defineProperty(window, nm, { enumerable: true }); } catch (e) {} }
  function defF(nm, len, fn) { try { Object.defineProperty(window, nm, { value: __native(nm, len, fn), writable: true, enumerable: true, configurable: true }); } catch (e) {} }
  function defAG(nm, getFn) { try { Object.defineProperty(window, nm, { get: __native("get " + nm, 0, getFn), enumerable: true, configurable: true }); } catch (e) {} }
  function defAGS(nm, dflt) { try { Object.defineProperty(window, nm, { get: __native("get " + nm, 0, function () { var v = __g(window, "_wv_" + nm); return v === undefined ? dflt : v; }), set: __native("set " + nm, 1, function (v) { try { __hide(window, "_wv_" + nm, v); } catch (e) {} }), enumerable: true, configurable: true }); } catch (e) {} }
  function mkTag(o, t) { try { Object.defineProperty(o, Symbol.toStringTag, { value: t, configurable: true }); } catch (e) {} return o; }
  function once(f) { var done = false, v; return function () { if (!done) { done = true; v = f(); } return v; }; }
  var i;
  var NE = ["AbortController","AbortSignal","AnalyserNode","AnimationEvent","Attr","AudioBuffer","AudioContext","AudioDestinationNode","AudioListener","AudioNode","AudioParam","AudioScheduledSourceNode","BarProp","BaseAudioContext","BeforeUnloadEvent","BiquadFilterNode","Blob","BroadcastChannel","CDATASection","CSS","CSSRule","CSSStyleDeclaration","CSSStyleRule","CSSStyleSheet","CanvasRenderingContext2D","CharacterData","Clipboard","ClipboardEvent","CloseEvent","Comment","CompositionEvent","ConstantSourceNode","ConvolverNode","Crypto","CryptoKey","CustomEvent","DOMImplementation","DOMParser","DOMRect","DOMRectList","DOMRectReadOnly","DOMStringMap","DOMTokenList","DelayNode","Document","DocumentFragment","DragEvent","DynamicsCompressorNode","Element","ErrorEvent","Event","EventTarget","External","File","FileList","FileReader","FocusEvent","FontFace","FontFaceSet","FormData","GainNode","Geolocation","HTMLAnchorElement","HTMLAreaElement","HTMLAudioElement","HTMLBRElement","HTMLBodyElement","HTMLButtonElement","HTMLCanvasElement","HTMLCollection","HTMLDListElement","HTMLDetailsElement","HTMLDivElement","HTMLDocument","HTMLElement","HTMLFieldSetElement","HTMLFormElement","HTMLHRElement","HTMLHeadElement","HTMLHeadingElement","HTMLHtmlElement","HTMLIFrameElement","HTMLImageElement","HTMLInputElement","HTMLLIElement","HTMLLabelElement","HTMLLinkElement","HTMLMediaElement","HTMLMetaElement","HTMLMeterElement","HTMLModElement","HTMLOListElement","HTMLOptionElement","HTMLOutputElement","HTMLParagraphElement","HTMLPreElement","HTMLProgressElement","HTMLQuoteElement","HTMLScriptElement","HTMLSelectElement","HTMLSlotElement","HTMLSourceElement","HTMLSpanElement","HTMLStyleElement","HTMLTableCellElement","HTMLTableElement","HTMLTableRowElement","HTMLTemplateElement","HTMLTextAreaElement","HTMLTitleElement","HTMLUListElement","HTMLUnknownElement","HTMLVideoElement","HashChangeEvent","Headers","History","Image","ImageData","InputEvent","IntersectionObserver","IntersectionObserverEntry","Intl","KeyboardEvent","Location","MediaDeviceInfo","MediaDevices","MediaList","MediaQueryList","MediaQueryListEvent","MediaStream","MediaStreamTrack","MessageChannel","MessageEvent","MessagePort","MimeType","MimeTypeArray","MouseEvent","MutationObserver","NamedNodeMap","Navigator","NavigatorUAData","NetworkInformation","Node","NodeFilter","NodeIterator","NodeList","Notification","OfflineAudioContext","OffscreenCanvas","OffscreenCanvasRenderingContext2D","OscillatorNode","PageTransitionEvent","PannerNode","Path2D","Performance","PerformanceNavigation","PerformanceObserver","PerformanceTiming","PeriodicWave","PermissionStatus","Permissions","Plugin","PluginArray","PointerEvent","PopStateEvent","ProcessingInstruction","ProgressEvent","PromiseRejectionEvent","RTCCertificate","RTCDTMFSender","RTCDataChannel","RTCIceCandidate","RTCPeerConnection","RTCRtpReceiver","RTCRtpSender","RTCRtpTransceiver","RTCSessionDescription","Range","ReadableStream","ReportingObserver","Request","ResizeObserver","Response","SVGAnimatedRect","SVGElement","SVGGElement","SVGGraphicsElement","SVGRect","SVGSVGElement","SVGTextContentElement","SVGTextElement","Screen","ScriptProcessorNode","SecurityPolicyViolationEvent","Selection","ShadowRoot","SpeechSynthesis","SpeechSynthesisUtterance","SpeechSynthesisVoice","StereoPannerNode","Storage","StorageEvent","StorageManager","StyleSheet","StyleSheetList","SubtleCrypto","Text","TextDecoder","TextEncoder","TextMetrics","ToggleEvent","TouchEvent","TrackEvent","TransitionEvent","TreeWalker","UIEvent","URL","URLSearchParams","WaveShaperNode","WebGL2RenderingContext","WebGLRenderingContext","WebSocket","WheelEvent","Window","Worker","XMLDocument","XMLHttpRequest","XMLSerializer","console"];
  for (i = 0; i < NE.length; i++) defNE(NE[i]);
  // ST-11 U2b-1: __DCI is a global lexical binding — NOT an own window
  // prop; the old defNE("__DCI") would now CREATE one (removed).
  var EN = ["atob", "btoa", "queueMicrotask"];
  for (i = 0; i < EN.length; i++) defEN(EN[i]);
  // window event-handler attributes: accessor pair backed by a hidden store
  // (Chrome descriptor {get,set,enumerable:true,configurable:true}; getter
  // "get on<x>"/0; default null; a non-callable set stores null).
  var ON = ["onabort","onafterprint","onanimationcancel","onanimationend","onanimationiteration","onanimationstart","onappinstalled","onauxclick","onbeforeinput","onbeforeinstallprompt","onbeforematch","onbeforeprint","onbeforetoggle","onbeforeunload","onbeforexrselect","onblur","oncancel","oncanplay","oncanplaythrough","onchange","onclick","onclose","oncommand","oncontentvisibilityautostatechange","oncontextlost","oncontextmenu","oncontextrestored","oncuechange","ondblclick","ondevicemotion","ondeviceorientation","ondeviceorientationabsolute","ondrag","ondragend","ondragenter","ondragleave","ondragover","ondragstart","ondrop","ondurationchange","onemptied","onended","onfocus","onformdata","ongamepadconnected","ongamepaddisconnected","ongotpointercapture","onhashchange","oninput","oninvalid","onkeydown","onkeypress","onkeyup","onlanguagechange","onloadeddata","onloadedmetadata","onloadstart","onlostpointercapture","onmessage","onmessageerror","onmousedown","onmouseenter","onmouseleave","onmousemove","onmouseout","onmouseover","onmouseup","onmousewheel","onoffline","ononline","onpagehide","onpagereveal","onpageshow","onpageswap","onpause","onplay","onplaying","onpointercancel","onpointerdown","onpointerenter","onpointerleave","onpointermove","onpointerout","onpointerover","onpointerrawupdate","onpointerup","onpopstate","onprogress","onratechange","onrejectionhandled","onreset","onresize","onscroll","onscrollend","onscrollsnapchange","onscrollsnapchanging","onsearch","onsecuritypolicyviolation","onseeked","onseeking","onselect","onselectionchange","onselectstart","onslotchange","onstalled","onstorage","onsubmit","onsuspend","ontimeupdate","ontoggle","ontransitioncancel","ontransitionend","ontransitionrun","ontransitionstart","onunload","onvolumechange","onwaiting","onwebkitanimationend","onwebkitanimationiteration","onwebkitanimationstart","onwebkittransitionend","onwheel","onload","onerror","onunhandledrejection"];
  for (i = 0; i < ON.length; i++) {
    (function (nm) {
      try {
        Object.defineProperty(window, nm, {
          get: __native("get " + nm, 0, function () { var v = __g(window, "_wv_" + nm); return v === undefined ? null : v; }),
          set: __native("set " + nm, 1, function (f) { try { __hide(window, "_wv_" + nm, typeof f === "function" ? f : null); } catch (e) {} }),
          enumerable: true, configurable: true,
        });
      } catch (e) {}
    })(ON[i]);
  }
  // ── the 47 Chrome-owned API names ─────────────────────────────────────
  function barProp() { try { return Object.create(window.BarProp.prototype); } catch (e) { return {}; } }
  function pj(msg, name) { return function () { try { return Promise.reject(new DOMException(msg, name)); } catch (e) { return Promise.reject(new Error(msg)); } }; }
  var __WV_NAV = null;
  defAGS("clientInformation", window.navigator);
  defAGS("event", undefined);
  defAGS("locationbar", barProp());
  defAGS("menubar", barProp());
  defAGS("personalbar", barProp());
  defAGS("scrollbars", barProp());
  defAGS("statusbar", barProp());
  defAGS("toolbar", barProp());
  defAGS("screenLeft", 0);
  defAGS("screenTop", 0);
  defAGS("status", "");
  defAGS("scheduler", mkTag({ postTask: function (a, b) { var cb = typeof a === "function" ? a : b; return Promise.resolve().then(function () { return cb ? cb() : undefined; }); }, yield: function () { return Promise.resolve(); } }, "Scheduler"));
  defAG("caches", function () { return __wkCaches(); });
  defAG("indexedDB", function () { return __wkIdb(); });
  defAG("credentialless", function () { return false; });
  defAG("originAgentCluster", function () { return false; });
  defAG("cookieStore", once(function () { return mkTag({ get: function () { return Promise.resolve(null); }, getAll: function () { return Promise.resolve([]); }, set: function () { return Promise.resolve(); }, delete: function () { return Promise.resolve(); } }, "CookieStore"); }));
  defAG("customElements", once(function () { return mkTag({ define: function () {}, get: function () { return undefined; }, getName: function () { return null; }, whenDefined: function () { return Promise.resolve(); }, upgrade: function () {} }, "CustomElementRegistry"); }));
  defAG("crashReport", once(function () { return mkTag({ getAll: function () { return Promise.resolve([]); } }, "CrashReportContext"); }));
  defAG("launchQueue", once(function () { return mkTag({ setConsumer: function () {} }, "LaunchQueue"); }));
  defAG("fence", function () { return null; }); // ST-11 U2b-1: Chrome returns null outside fenced frames (measured).
  defAG("documentPictureInPicture", once(function () { return mkTag({ window: null, requestWindow: pj("Document Picture-in-Picture is not allowed.", "NotAllowedError") }, "DocumentPictureInPicture"); }));
  defAG("styleMedia", once(function () { return mkTag({ type: "screen", matchMedium: function () { return false; } }, "StyleMedia"); }));
  function __wmkNav() {
    if (__WV_NAV) return __WV_NAV;
    var cur = mkTag({ url: String((window.location && window.location.href) || ""), key: "0000000000000000", id: "00000000-0000-0000-0000-000000000000", index: 0, sameDocument: true, getState: function () { return undefined; } }, "NavigationHistoryEntry");
    var res = function () { return { committed: Promise.resolve(cur), finished: Promise.resolve(cur) }; };
    __WV_NAV = mkTag({ currentEntry: cur, canGoBack: false, canGoForward: false, transition: null, activation: null, entries: function () { return [cur]; }, navigate: res, reload: res, traverseTo: res, back: function () {}, forward: function () {}, updateCurrentEntry: function () {} }, "Navigation");
    try { addEventTarget(__WV_NAV); } catch (e) {}
    return __WV_NAV;
  }
  defAGS("navigation", __wmkNav());
  var __WV_VP = null, __WV_VV = null;
  function __wmkVV() {
    if (__WV_VV) return __WV_VV;
    __WV_VV = mkTag({ width: (window.innerWidth | 0), height: (window.innerHeight | 0), offsetLeft: 0, offsetTop: 0, pageLeft: 0, pageTop: 0, scale: 1, onresize: null, onscroll: null, onscrollend: null }, "VisualViewport");
    try { addEventTarget(__WV_VV); } catch (e) {}
    return __WV_VV;
  }
  defAGS("visualViewport", __wmkVV());
  defF("captureEvents", 0, function () {});
  defF("releaseEvents", 0, function () {});
  defF("find", 0, function () { return false; });
  defF("getScreenDetails", 0, pj("Permission denied", "NotAllowedError"));
  defF("queryLocalFonts", 0, pj("Permission denied", "NotAllowedError"));
  defF("showDirectoryPicker", 0, pj("Must be handling a user gesture to show a file picker.", "SecurityError"));
  defF("showOpenFilePicker", 0, pj("Must be handling a user gesture to show a file picker.", "SecurityError"));
  defF("showSaveFilePicker", 0, pj("Must be handling a user gesture to show a file picker.", "SecurityError"));
  defF("fetchLater", 1, function () { return { activated: false }; });
  defF("moveBy", 2, function () {});
  defF("moveTo", 2, function () {});
  defF("resizeBy", 2, function () {});
  defF("resizeTo", 2, function () {});
  defF("scroll", 0, function () {});
  defF("stop", 0, function () {});
  defF("requestResize", 0, function () { return Promise.resolve(); });
  defF("webkitRequestAnimationFrame", 1, function (cb) { return window.requestAnimationFrame(cb); });
  defF("webkitCancelAnimationFrame", 1, function (h) { return window.cancelAnimationFrame(h); });
  defF("webkitRequestFileSystem", 3, function () {});
  defF("webkitResolveLocalFileSystemURL", 2, function () {});
  try { Object.defineProperty(window, "createImageBitmap", { value: __wkCreateImageBitmap(), writable: true, enumerable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(window, "viewport", { get: __native("get viewport", 0, function () { var v = __g(window, "_wv_viewport"); if (v !== undefined) return v; if (__WV_VP) return __WV_VP; __WV_VP = mkTag({ width: (window.innerWidth | 0), height: (window.innerHeight | 0), offsetLeft: 0, offsetTop: 0, scale: 1 }, "Viewport"); return __WV_VP; }), set: __native("set viewport", 1, function (v) { try { __hide(window, "_wv_viewport", v); } catch (e) {} }), enumerable: true, configurable: true }); } catch (e) {}
})();

// ── function surface (swept vs Chromium 154): every engine-defined host
// function must render as native — its stored JS source would leak engine
// internals and Chrome shows [native code] for all of them. js_clear_fn_source
// (captured at boot) strips the source; identity, prototype and behavior are
// untouched. Lengths below are Chrome-measured where they differed.
(function () {
  var i, k;
  var FN = ["matchMedia","getComputedStyle","scrollTo","scrollBy","alert","confirm","prompt","open","close","focus","blur","postMessage","print","requestIdleCallback","cancelIdleCallback","Image","fetch","TextEncoder","TextDecoder","structuredClone","PerformanceObserver","reportError","OfflineAudioContext","AudioContext","WebGLRenderingContext","CanvasRenderingContext2D","XMLHttpRequest","OffscreenCanvas","URL","URLSearchParams","ReadableStream","RTCPeerConnection","RTCDataChannel","RTCSessionDescription","RTCIceCandidate","DOMRect","DOMRectReadOnly","AudioBuffer","AudioNode","AudioScheduledSourceNode","AudioParam","AudioListener","AudioDestinationNode","AnalyserNode","BiquadFilterNode","DelayNode","DynamicsCompressorNode","GainNode","OscillatorNode","PannerNode","StereoPannerNode","ConstantSourceNode","SpeechSynthesisUtterance","SpeechSynthesisVoice","MessageChannel","MessagePort","SVGRect","SVGElement","SVGGraphicsElement","SVGSVGElement","SVGGElement","SVGTextContentElement","SVGTextElement","SVGAnimatedRect","DOMRectList","Plugin","PluginArray","MimeType","MimeTypeArray","NavigatorUAData","NodeFilter","AbstractRange","setTimeout","clearTimeout","setInterval","clearInterval","requestAnimationFrame","cancelAnimationFrame"];
  for (i = 0; i < FN.length; i++) { try { var f = window[FN[i]]; if (typeof f === "function" && __clearSrc) __clearSrc(f); } catch (e) {} }
  var PATHS = [["crypto","getRandomValues"],["crypto","randomUUID"],["crypto","subtle","encrypt"],["crypto","subtle","decrypt"],["crypto","subtle","sign"],["crypto","subtle","verify"],["crypto","subtle","digest"],["crypto","subtle","generateKey"],["crypto","subtle","deriveBits"],["crypto","subtle","importKey"],["crypto","subtle","exportKey"],["crypto","subtle","wrapKey"],["crypto","subtle","unwrapKey"],["CSS","supports"],["CSS","escape"],["screen","orientation","lock"],["screen","orientation","unlock"],["indexedDB","open"],["indexedDB","deleteDatabase"],["indexedDB","databases"],["indexedDB","cmp"],["caches","open"],["caches","has"],["caches","delete"],["caches","keys"],["caches","match"],["chrome","csi"],["chrome","loadTimes"],["navigator","clipboard","writeText"],["navigator","clipboard","readText"],["navigator","mediaDevices","getUserMedia"],["navigator","mediaDevices","enumerateDevices"],["navigator","geolocation","getCurrentPosition"],["navigator","geolocation","watchPosition"]];
  for (i = 0; i < PATHS.length; i++) { try { var cur = window; for (k = 0; k < PATHS[i].length; k++) cur = cur[PATHS[i][k]]; if (typeof cur === "function" && __clearSrc) __clearSrc(cur); } catch (e) {} }
  var LN = [["AnalyserNode",1],["AudioBuffer",1],["BiquadFilterNode",1],["ConstantSourceNode",1],["ConvolverNode",1],["DOMRect",0],["DOMRectReadOnly",0],["DelayNode",1],["DynamicsCompressorNode",1],["GainNode",1],["Image",0],["MediaQueryListEvent",1],["OfflineAudioContext",1],["OscillatorNode",1],["PannerNode",1],["PeriodicWave",1],["RTCDataChannel",0],["RTCIceCandidate",0],["RTCPeerConnection",0],["RTCSessionDescription",0],["ReportingObserver",1],["SpeechSynthesisUtterance",0],["StereoPannerNode",1],["TextDecoder",0],["URL",1],["URLSearchParams",0],["WaveShaperNode",1],["clearInterval",0],["clearTimeout",0],["fetch",1],["postMessage",1],["setInterval",1],["setTimeout",1]];
  for (i = 0; i < LN.length; i++) { try { var g = window[LN[i][0]]; if (typeof g === "function") Object.defineProperty(g, "length", { value: LN[i][1], writable: false, enumerable: false, configurable: true }); } catch (e) {} }
})();
// ── (e) window own-surface materializer — the 695 Chrome-owned window names ──
// Every window property the same-hour Chromium 154 owns that the engine lacked:
// exact descriptor flags {w:true,e:false,c:true}, fn name/length/native ts,
// per-name new/call semantics (measured, incl. the CSSMath* DOMException
// SyntaxError family), measured proto parents + toStringTag, statics with
// measured flags/lengths, GPU constant namespaces, Temporal + WebAssembly
// namespace surfaces (shallow; deep member behavior banked), the legacy
// offscreenBuffering accessor, and the two (d)-noted gaps (crypto.subtle.
// deriveKey, CSS.registerProperty). Table: assets/surf_win_tables.js
// (generated by tools/gen_surf_win.py). Deep per-class member surfaces are
// tier-2 (banked, RECEIPTS 44).
(function () {
  var T = __DCI.SURFW;
  if (!T) return;
  var g = globalThis;
  var made = {};
  function defW(nm, v) { try { Object.defineProperty(window, nm, { value: v, writable: true, enumerable: false, configurable: true }); } catch (e) {} }
  function defD(o, k, v, w, e, c) { try { Object.defineProperty(o, k, { value: v, writable: !!w, enumerable: !!e, configurable: !!c }); } catch (e2) {} }
  function defDF(o, k, v, fl) { defD(o, k, v, fl.charAt(0) === "1", fl.charAt(1) === "1", fl.charAt(2) === "1"); }
  function tagOf(o, s) { try { Object.defineProperty(o, Symbol.toStringTag, { value: s, configurable: true }); } catch (e) {} }
  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }
  function mkC(nm, ln, mode, reqN, nE, nM, cE, cM, tagn) {
    var n = mode.charAt(0), c = mode.charAt(1);
    var C = function () {
      if (new.target !== undefined) {
        if (nM && arguments.length === 0) throw (nE === "RangeError" ? new RangeError(nM) : new TypeError(nM));
        if (n === "i") throw new TypeError("Failed to construct '" + nm + "': Illegal constructor");
        if (n === "r" && arguments.length < reqN) throw new TypeError("Failed to construct '" + nm + "': " + reqN + (reqN === 1 ? " argument" : " arguments") + " required, but only " + arguments.length + " present.");
        if (n === "e" && arguments.length === 0) {
          var dm = "Failed to construct '" + nm + "': Arguments can't be empty";
          if (typeof g.DOMException === "function") throw new g.DOMException(dm, "SyntaxError");
          throw new TypeError(dm);
        }
        return;
      }
      if (cM) throw (cE === "RangeError" ? new RangeError(cM) : new TypeError(cM));
      if (c === "u") throw new TypeError("Failed to construct '" + nm + "': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      throw new TypeError("Illegal constructor");
    };
    try { Object.defineProperty(C, "name", { value: nm, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C, "length", { value: ln, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C, "prototype", { writable: false }); } catch (e) {}
    try { Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true }); } catch (e) {}
    tagOf(C.prototype, tagn || nm);
    if (__clearSrc) __clearSrc(C);
    made[nm] = C;
    return C;
  }
  function mkE(nm, ln) {
    var C = function () {
      var e = new Error(arguments.length ? String(arguments[0]) : "");
      try { Object.setPrototypeOf(e, C.prototype); } catch (x) {}
      return e;
    };
    try { Object.defineProperty(C, "name", { value: nm, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C, "length", { value: ln, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C, "prototype", { value: Object.create(g.Error ? g.Error.prototype : Object.prototype), writable: false }); } catch (e) {}
    try { Object.defineProperty(C.prototype, "constructor", { value: C, writable: true, configurable: true }); } catch (e) {}
    try { Object.defineProperty(C.prototype, "name", { value: nm, writable: true, enumerable: false, configurable: true }); } catch (e) {}
    if (__clearSrc) __clearSrc(C);
    made[nm] = C;
    return C;
  }
  var i, j, row, C, x;
  // 1 — interface stubs (parents wired in phase 2; statics + pair extras here)
  if (T.fn) for (i = 0; i < T.fn.length; i++) {
    row = T.fn[i];
    C = mkC(row[0], row[1], row[2], row[3], "", "", "", "", null);
    if (row[4]) for (j = 0; j < row[4].length; j++) defW(row[4][j], C);
    if (row[5]) for (j = 0; j < row[5].length; j++) {
      x = row[5][j];
      if (x[1] === 1) defDF(C, x[0], x[2], x[3]);
      else defDF(C, x[0], __native(x[0], x[2], function () {}), x[3]);
    }
    defW(row[0], C);
  }
  // 2 — proto parents (two-phase: parents may be later in the list)
  if (T.par) for (var pn in T.par) { try { var pp = g[T.par[pn]]; if (made[pn] && pp && pp.prototype) setP(made[pn].prototype, pp.prototype); } catch (e) {} }
  // 3 — tag overrides (interface's own instance tag != its name)
  if (__mr(T, "tag")) for (var tn in __mr(T, "tag")) { try { if (made[tn]) tagOf(made[tn].prototype, __mr(T, "tag")[tn]); } catch (e) {} }
  // 4a — webkit-family coherence (measured): MediaStream's engine class is the
  // generic bank illegal-ctor; Chrome's constructs with no args, call →
  // use-new, parent EventTarget. Rebuilt on the SAME prototype object.
  try {
    var MSold = g.MediaStream;
    if (typeof MSold === "function" && MSold.prototype) {
      var MS = function () {
        if (new.target === undefined) throw new TypeError("Failed to construct 'MediaStream': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
      };
      try { Object.defineProperty(MS, "name", { value: "MediaStream", configurable: true }); } catch (e) {}
      try { Object.defineProperty(MS, "length", { value: 0, configurable: true }); } catch (e) {}
      try { Object.defineProperty(MS, "prototype", { value: MSold.prototype, writable: false }); } catch (e) {}
      try { Object.defineProperty(MSold.prototype, "constructor", { value: MS, writable: true, configurable: true }); } catch (e) {}
      if (g.EventTarget && g.EventTarget.prototype) setP(MS.prototype, g.EventTarget.prototype);
      tagOf(MS.prototype, "MediaStream");
      if (__clearSrc) __clearSrc(MS);
      g.MediaStream = MS;
    }
  } catch (e) {}
  // 4 — webkit aliases = the existing engine class (identity like Chrome) +
  // Chrome's WebIDL prototype-property shape {w:false} on the alias targets.
  if (T.alias) for (i = 0; i < T.alias.length; i++) { try { var av = g[T.alias[i][1]]; if (typeof av === "function") { try { Object.defineProperty(av, "prototype", { writable: false }); } catch (eP) {} defW(T.alias[i][0], av); } } catch (e) {} }
  // 5 — GPU constant namespaces (plain object + tag + read-only enum consts)
  if (T.gpu) for (i = 0; i < T.gpu.length; i++) {
    var GO = {}; tagOf(GO, T.gpu[i][0]);
    var cs = T.gpu[i][1];
    for (j = 0; j < cs.length; j++) defD(GO, cs[j][0], cs[j][1], false, true, false);
    defW(T.gpu[i][0], GO);
  }
  // 6 — namespace objects (Temporal / WebAssembly, member shape only)
  if (T.ns) for (i = 0; i < T.ns.length; i++) {
    var nso = T.ns[i], NO = {}; tagOf(NO, nso[0]);
    var ms = nso[1];
    for (j = 0; j < ms.length; j++) {
      var m = ms[j], mv, wf = m[9] || "101";
      if (m[1] === "o") { mv = {}; tagOf(mv, nso[0] + "." + m[0]); }
      else if (m[1] === "p") { mv = __native(m[0], m[2], function () {}); }
      else if (m[1] === "e") { mv = mkE(m[0], m[2]); }
      else { mv = mkC(m[0], m[2], m[3], m[4], m[5], m[6], m[7], m[8], nso[0] + "." + m[0]); }
      defDF(NO, m[0], mv, wf);
    }
    defW(nso[0], NO);
  }
  // 7 — Temporal.Now: anonymous native getters {e:false,c:true}
  try {
    var nowO = g.Temporal && g.Temporal.Now;
    if (nowO) {
      var gn = ["instant", "timeZoneId", "plainDateTimeISO", "zonedDateTimeISO", "plainDateISO", "plainTimeISO"];
      for (j = 0; j < gn.length; j++) try { Object.defineProperty(nowO, gn[j], { get: __native("", 0, function () { return undefined; }), enumerable: false, configurable: true }); } catch (e) {}
    }
  } catch (e) {}
  // 8 — legacy accessor: offscreenBuffering (stored value, chrome-measured)
  try {
    Object.defineProperty(window, "offscreenBuffering", {
      get: __native("get offscreenBuffering", 0, function () { var v = __g(window, "_wv_ob"); return v === undefined ? true : v; }),
      set: __native("set offscreenBuffering", 1, function (v) { try { __hide(window, "_wv_ob", v); } catch (e) {} }),
      enumerable: false, configurable: true,
    });
  } catch (e) {}
  // 9 — the two (d)-noted gaps: crypto.subtle.deriveKey + CSS.registerProperty
  try {
    var sub = g.crypto && g.crypto.subtle;
    if (sub && typeof sub.deriveKey === "undefined") {
      var dk = function () { return Promise.reject(new Error("NotSupportedError: deriveKey")); };
      try { Object.defineProperty(dk, "name", { value: "deriveKey", configurable: true }); Object.defineProperty(dk, "length", { value: 5, configurable: true }); } catch (e) {}
      if (__clearSrc) __clearSrc(dk);
      defD(sub, "deriveKey", dk, true, true, true);
    }
  } catch (e) {}
  try {
    if (g.CSS && typeof g.CSS.registerProperty === "undefined") {
      var rp = function () {};
      try { Object.defineProperty(rp, "name", { value: "registerProperty", configurable: true }); Object.defineProperty(rp, "length", { value: 1, configurable: true }); } catch (e) {}
      if (__clearSrc) __clearSrc(rp);
      defD(g.CSS, "registerProperty", rp, true, true, true);
    }
  } catch (e) {}
    // 10 — SharedWorker: shape + real sandbox runner (ST-10 catches: the generic
  // constructible left instances bare — port missing, `port.start()` TypeError;
  // then a shape-only port stalled the page on its worker give-up). Chrome
  // truth (local oracle, same hour): prototype own order exactly
  // port,constructor,onerror; port = enumerable per-instance MessagePort
  // accessor, no setter, stable identity, "Illegal invocation" on a wrong
  // receiver; onerror = nullable accessor (get len 0 / set len 1); instances
  // carry zero own props. Construction runs the script in the existing worker
  // sandbox (same runner as Worker — it fetches and runs the script in the
  // worker realm and posts real data) and bridges through the port pair:
  // sandbox postMessage -> p2.postMessage -> page port; page port postMessage
  // -> p2.onmessage -> sandbox. Banked for its own unit: the SharedWorkerGlobal-
  // Scope flavor + connect/ports semantics (sandbox runs the dedicated branch).
  try {
    var SWold = made["SharedWorker"];
    if (SWold && g.Worker && g.MessageChannel) {
      var SPW = new WeakMap(), SOW = new WeakMap();
      var SW = function SharedWorker(url, opts) {
        if (new.target === undefined) throw new TypeError("Failed to construct 'SharedWorker': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
        if (arguments.length < 1) throw new TypeError("Failed to construct 'SharedWorker': 1 argument required, but only 0 present.");
        var self0 = this;
        var mc = new g.MessageChannel();
        var p1 = mc.port1, p2 = mc.port2;
        var w = new g.Worker(String(url));
        p2.onmessage = function (ev) { try { w.postMessage(ev.data); } catch (e) {} };
        w.onmessage = function (ev) { try { p2.postMessage(ev.data); } catch (e) {} };
        w.onerror = function (ev) { var h = SOW.get(self0); if (typeof h === "function") { try { h.call(self0, ev); } catch (e) {} } };
        SPW.set(self0, p1);
      };
      try { Object.defineProperty(SW, "name", { value: "SharedWorker", configurable: true }); } catch (e) {}
      try { Object.defineProperty(SW, "length", { value: 1, configurable: true }); } catch (e) {}
      try { Object.defineProperty(SW, "prototype", { value: SWold.prototype, writable: false }); } catch (e) {}
      if (__clearSrc) __clearSrc(SW);
      made["SharedWorker"] = SW;
      defW("SharedWorker", SW);
      try { delete SWold.prototype.port; } catch (e) {}
      try { delete SWold.prototype.constructor; } catch (e) {}
      try { delete SWold.prototype.onerror; } catch (e) {}
      try {
        Object.defineProperty(SWold.prototype, "port", {
          get: __native("get port", 0, function () {
            if (!(this instanceof SW)) throw new TypeError("Illegal invocation");
            var p = SPW.get(this);
            if (!p) { p = new g.MessageChannel().port1; SPW.set(this, p); }
            return p;
          }),
          enumerable: true, configurable: true,
        });
      } catch (e) {}
      try { Object.defineProperty(SWold.prototype, "constructor", { value: SW, writable: true, configurable: true }); } catch (e) {}
      try {
        Object.defineProperty(SWold.prototype, "onerror", {
          get: __native("get onerror", 0, function () {
            if (!(this instanceof SW)) throw new TypeError("Illegal invocation");
            var v = SOW.get(this); return v === undefined ? null : v;
          }),
          set: __native("set onerror", 1, function (v) {
            if (!(this instanceof SW)) throw new TypeError("Illegal invocation");
            SOW.set(this, typeof v === "function" ? v : null);
          }),
          enumerable: true, configurable: true,
        });
      } catch (e) {}
    }

  } catch (e) {}
})();
// ── ST-7 §22: the non-element node class layer — Comment / CharacterData /
// DocumentFragment / Attr / DocumentType / ShadowRoot + document shape + the
// live collection views (HTMLCollection / NamedNodeMap) + EventTarget.when +
// the custom-element registry.  Chrome truth measured 2026-10-20 (probes
// nc/nc2/nc3/nc4, same-hour installed Chromium 154): every descriptor flag,
// method length, V8 message and default value below comes from that capture.
// Class-level only — no per-site logic anywhere.
//
// Shape rules (all measured):
//   · instances carry ZERO own props: state lives in the hidden store
//     (__hide/__g), so Object.getOwnPropertyNames(instance) === [] as in Chrome;
//     Node.prototype's materialized accessors read the _sf_* keys.
//   · every prototype's own-name list and ORDER equals Chrome's; every member
//     is native-shaped (__native) with the measured length and flags.
//   · a wrong receiver throws TypeError "Illegal invocation".
//   · class-layer parents: makeElement/makeText objects keep their own props.
(function () {
  var g = globalThis;
  function P(n) { var C = g[n]; return C && C.prototype ? C.prototype : null; }
  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }
  function tagOf(o, s) { try { Object.defineProperty(o, Symbol.toStringTag, { value: s, writable: false, enumerable: false, configurable: true }); } catch (e) {} }
  function mfn(C, nm, ln, fn, en) {
    try { Object.defineProperty(C.prototype, nm, { value: __native(nm, ln, fn), writable: true, enumerable: en === undefined ? true : !!en, configurable: true }); } catch (e) {}
  }
  function macc(C, nm, get, set, en) {
    var d = { get: get, enumerable: en === undefined ? true : !!en, configurable: true };
    if (set) d.set = set;
    try { Object.defineProperty(C.prototype, nm, d); } catch (e) {}
  }
  function inv() { return new TypeError("Illegal invocation"); }
  function ng(name, fn) { return __native("get " + name, 0, fn); }
  function ns(name, fn) { return __native("set " + name, 1, fn); }
  function hid(t, k, v) { __hide(t, "_sf_" + k, v); }
  function arity(iface, fn, need, have) {
    throw new TypeError("Failed to execute '" + fn + "' on '" + iface + "': " + need + " argument" + (need === 1 ? "" : "s") + " required, but only " + have + " present.");
  }
  function hierr(msg) {
    var d = new DOMException(msg, "HierarchyRequestError");
    try { Error.captureStackTrace(d); } catch (e) {}
    return d;
  }

  // ── nodeType read (own data prop first — engine-era elements/text keep it) ─
  function nodeTypeOf(t) {
    try {
      var d = Object.getOwnPropertyDescriptor(t, "nodeType");
      if (d && typeof d.value === "number") return d.value;
      var h = __g(t, "_sf_nodeType");
      return typeof h === "number" ? h : undefined;
    } catch (e) { return undefined; }
  }

  // ── character data: Chrome's CharacterData store is the node's data ──────
  // Text keeps the engine's `.text` as the single store (makeText builds
  // instances without the ctor), so data/length/substringData/textContent/
  // nodeValue all route through it and stay coherent (measured nc6 t.dataSet
  // "xy|xy|xy"); Comment carries the hidden _sf_data.
  function cdata(t) {
    var nt = nodeTypeOf(t);
    if (nt === 3 && __mr(t, "text") !== undefined) return String(__mr(t, "text"));
    var d = Object.getOwnPropertyDescriptor(t, "data");
    if (d && d.value !== undefined) return String(d.value);
    var h = __g(t, "_sf_data");
    return h === undefined ? "" : String(h);
  }
  function cdataSet(t, v) {
    var s = String(v), nt = nodeTypeOf(t);
    if (nt === 3) {
      try { __ns(t, "text", s); } catch (e) {}
      var d0 = Object.getOwnPropertyDescriptor(t, "data");
      if (d0 && d0.value !== undefined && d0.writable) d0.value = s;
      var d1 = Object.getOwnPropertyDescriptor(t, "nodeValue");
      if (d1 && d1.value !== undefined && d1.writable) d1.value = s;
      __hide(t, "_sf_data", s);
      return s;
    }
    var d = Object.getOwnPropertyDescriptor(t, "data");
    if (d && d.value !== undefined && d.writable) { d.value = s; return s; }
    __hide(t, "_sf_data", s);
    return s;
  }
  function cdataGuard(t) { var nt = nodeTypeOf(t); if (nt !== 3 && nt !== 8) throw inv(); }
  function cdataOffset(t, off, fn) {
    var n = Number(off) >>> 0, len = cdata(t).length;
    if (n > len) {
      var e = new DOMException("Failed to execute '" + fn + "' on 'CharacterData': The offset " + n + " is greater than the node's length (" + len + ").", "IndexSizeError");
      try { Error.captureStackTrace(e); } catch (e1) {}
      throw e;
    }
    return n;
  }

  // ── ChildNode mixin (after/before/remove/replaceWith) over the parent ────
  function asNode(x) { return (typeof x === "string" || typeof x === "number") ? makeText(String(x)) : x; }
  function argList(args) { var a = []; for (var i = 0; i < args.length; i++) a.push(args[i]); return a; }
  function cnRemove(t) { var p = t.parentNode; if (p) p.removeChild(t); }
  function cnInsert(t, nodes, after) {
    var p = t.parentNode; if (!p) return;
    var ref = after ? t.nextSibling : t;
    for (var i = 0; i < nodes.length; i++) p.insertBefore(asNode(nodes[i]), ref);
  }
  function cnReplaceWith(t, nodes) {
    var p = t.parentNode; if (!p) return;
    var ref = t.nextSibling;
    p.removeChild(t);
    for (var i = 0; i < nodes.length; i++) p.insertBefore(asNode(nodes[i]), ref);
  }

  // ── instance factories: zero own props, hidden state only ────────────────
  function mkNode(iface, nt, nn) {
    var o = Object.create(P(iface) || Object.prototype);
    __mk(o);
    __hide(o, "_s7b", iface);
    hid(o, "nodeType", nt);
    hid(o, "nodeName", nn);
    hid(o, "childNodes", []);
    hid(o, "parentNode", null);
    return o;
  }
  function ncComment(data) {
    var c = mkNode("Comment", 8, "#comment");
    __hide(c, "_sf_data", String(data));
    return c;
  }
  function ncFragment(host) {
    var f = mkNode("DocumentFragment", 11, "#document-fragment");
    if (host) __hide(f, "_tmplHost", host);
    return f;
  }
  function ncDocType(name, pub, sys) {
    var d = mkNode("DocumentType", 10, String(name));
    __hide(d, "_sf_name", String(name));
    __hide(d, "_sf_publicId", String(pub == null ? "" : pub));
    __hide(d, "_sf_systemId", String(sys == null ? "" : sys));
    return d;
  }
  function ncAttr(name, value, ownerEl) {
    var a = mkNode("Attr", 2, String(name));
    __hide(a, "_sf_name", String(name));
    __hide(a, "_sf_localName", String(name));
    __hide(a, "_sf_value", value === undefined ? "" : String(value));
    __hide(a, "_sf_ownerElement", ownerEl === undefined ? null : ownerEl);
    return a;
  }
  function ncShadowRoot(hostEl, init) {
    var sr = mkNode("ShadowRoot", 11, "#document-fragment");
    __hide(sr, "_sf_host", hostEl);
    __hide(sr, "_sf_mode", (init && init.mode) || "open");
    __hide(sr, "_srId", (__DCI.t.srSeq = (__DCI.t.srSeq || 0) + 1));
    __hide(sr, "_sf_delegatesFocus", !!(init && init.delegatesFocus));
    __hide(sr, "_sf_slotAssignment", (init && init.slotAssignment) === "manual" ? "manual" : "named");
    __hide(sr, "_sf_serializable", !!(init && init.serializable));
    __hide(sr, "_sf_clonable", !!(init && init.clonable));
    return sr;
  }
  // a class-layer node keeps its parent in the hidden store, never as an own
  // prop (Chrome instances own nothing) — engine-era nodes keep the old shape.
  function ncLink(n, p) {
    if (!n) return n;
    if (__g(n, "_s7b")) { hid(n, "parentNode", p); return n; }
    try { __nsf(n, "parentNode", p); } catch (e) {}
    try { __ns(n, "__parent", p); } catch (e2) {}
    return n;
  }
  // after an engine-era link site ran, strip the own props a class-layer node
  // must not carry
  function ncUnspill(n, p) {
    if (!n || !__g(n, "_s7b")) return n;
    try { __nd(n, "__parent"); } catch (e) {}
    hid(n, "parentNode", p === undefined ? null : p);
    return n;
  }

  // ── CharacterData.prototype (Chrome order + measured flags) ──────────────
  var CD = g.CharacterData;
  if (CD && CD.prototype) {
    macc(CD, "data",
      ng("data", function () { cdataGuard(this); return cdata(this); }),
      ns("data", function (v) { cdataGuard(this); cdataSet(this, v); }));
    macc(CD, "length", ng("length", function () { cdataGuard(this); return cdata(this).length; }));
    macc(CD, "previousElementSibling", ng("previousElementSibling", function () {
      cdataGuard(this); var p = this.parentNode; if (!p) return null;
      var k = p.childNodes, i = k.indexOf(this);
      for (i = i - 1; i >= 0; i--) if (k[i] && k[i].nodeType === 1) return k[i];
      return null;
    }));
    macc(CD, "nextElementSibling", ng("nextElementSibling", function () {
      cdataGuard(this); var p = this.parentNode; if (!p) return null;
      var k = p.childNodes, i = k.indexOf(this);
      for (i = i + 1; i < k.length; i++) if (k[i] && k[i].nodeType === 1) return k[i];
      return null;
    }));
    mfn(CD, "after", 0, function () { cdataGuard(this); cnInsert(this, argList(arguments), true); });
    mfn(CD, "appendData", 1, function (d) {
      cdataGuard(this); if (arguments.length < 1) arity("CharacterData", "appendData", 1, 0);
      cdataSet(this, cdata(this) + String(d));
    });
    mfn(CD, "before", 0, function () { cdataGuard(this); cnInsert(this, argList(arguments), false); });
    mfn(CD, "deleteData", 2, function (off, cnt) {
      cdataGuard(this); if (arguments.length < 2) arity("CharacterData", "deleteData", 2, arguments.length);
      var s = cdata(this), o = cdataOffset(this, off, "deleteData"), c = Number(cnt) >>> 0;
      cdataSet(this, s.slice(0, o) + s.slice(o + c));
    });
    mfn(CD, "insertData", 2, function (off, d) {
      cdataGuard(this); if (arguments.length < 2) arity("CharacterData", "insertData", 2, arguments.length);
      var s = cdata(this), o = cdataOffset(this, off, "insertData");
      cdataSet(this, s.slice(0, o) + String(d) + s.slice(o));
    });
    mfn(CD, "remove", 0, function () { cdataGuard(this); cnRemove(this); });
    mfn(CD, "replaceData", 3, function (off, cnt, d) {
      cdataGuard(this); if (arguments.length < 3) arity("CharacterData", "replaceData", 3, arguments.length);
      var s = cdata(this), o = cdataOffset(this, off, "replaceData"), c = Number(cnt) >>> 0;
      cdataSet(this, s.slice(0, o) + String(d) + s.slice(o + c));
    });
    mfn(CD, "replaceWith", 0, function () { cdataGuard(this); cnReplaceWith(this, argList(arguments)); });
    mfn(CD, "substringData", 2, function (off, cnt) {
      cdataGuard(this); if (arguments.length < 2) arity("CharacterData", "substringData", 2, arguments.length);
      var s = cdata(this), o = cdataOffset(this, off, "substringData");
      return s.substr(o, Number(cnt) >>> 0);
    });
  }

  // ── Attr.prototype (value is live while the attribute is attached) ───────
  var AT = g.Attr;
  if (AT && AT.prototype) {
    function attrGuard(t) { if (__g(t, "_s7b") !== "Attr") throw inv(); }
    function attrVal(t) {
      var el = __g(t, "_sf_ownerElement"), nm = __g(t, "_sf_name");
      if (el && __mr(el, "attrs") && Object.prototype.hasOwnProperty.call(__mr(el, "attrs"), nm)) return String(__mr(el, "attrs")[nm]);
      var h = __g(t, "_sf_value");
      return h === undefined ? "" : String(h);
    }
    macc(AT, "namespaceURI", ng("namespaceURI", function () { attrGuard(this); return null; }));
    macc(AT, "prefix", ng("prefix", function () { attrGuard(this); return null; }));
    macc(AT, "localName", ng("localName", function () { attrGuard(this); return __g(this, "_sf_localName"); }));
    macc(AT, "name", ng("name", function () { attrGuard(this); return __g(this, "_sf_name"); }));
    macc(AT, "value",
      ng("value", function () { attrGuard(this); return attrVal(this); }),
      ns("value", function (v) {
        attrGuard(this);
        var el = __g(this, "_sf_ownerElement"), nm = __g(this, "_sf_name");
        if (el && __mr(el, "attrs")) __mr(el, "attrs")[nm] = String(v); else __hide(this, "_sf_value", String(v));
      }));
    macc(AT, "ownerElement", ng("ownerElement", function () { attrGuard(this); return __g(this, "_sf_ownerElement"); }));
    macc(AT, "specified", ng("specified", function () { attrGuard(this); return true; }));
  }

  // ── DocumentType.prototype ───────────────────────────────────────────────
  var DT = g.DocumentType;
  if (DT && DT.prototype) {
    function dtGuard(t) { if (__g(t, "_s7b") !== "DocumentType") throw inv(); }
    macc(DT, "name", ng("name", function () { dtGuard(this); return __g(this, "_sf_name"); }));
    macc(DT, "publicId", ng("publicId", function () { dtGuard(this); return __g(this, "_sf_publicId"); }));
    macc(DT, "systemId", ng("systemId", function () { dtGuard(this); return __g(this, "_sf_systemId"); }));
    mfn(DT, "after", 0, function () { dtGuard(this); cnInsert(this, argList(arguments), true); });
    mfn(DT, "before", 0, function () { dtGuard(this); cnInsert(this, argList(arguments), false); });
    mfn(DT, "remove", 0, function () { dtGuard(this); cnRemove(this); });
    mfn(DT, "replaceWith", 0, function () { dtGuard(this); cnReplaceWith(this, argList(arguments)); });
  }

  // ── live collection views ────────────────────────────────────────────────
  // Chrome instances carry own index/name props ({data, !writable, enumerable})
  // and are LIVE: a handle kept across mutations reflects them (measured
  // ah.live / ch.live / ah.ownAfter / ch.ownAfter). Implemented as a Proxy whose
  // traps synthesize those own props from the underlying list on every access.
  var __VIEWS = new WeakMap();   // proxy -> {list, named, keys, kind}
  function viewState(t, kind) {
    var st = __VIEWS.get(t);
    if (!st || st.kind !== kind) throw inv();
    return st;
  }
  function mkView(protoName, kind, listFn, namedFn, keysFn) {
    var proto = P(protoName) || Object.prototype;
    var target = Object.create(proto);
    var proxy = new Proxy(target, {
      get: function (t, k, r) {
        if (typeof k === "string" && k !== "length") {
          var items = listFn();
          if (/^(0|[1-9][0-9]*)$/.test(k)) { var i = +k; return i < items.length ? items[i] : undefined; }
          var hit = namedFn(items, k);
          if (hit !== undefined) return hit;
        }
        return Reflect.get(t, k, r);
      },
      has: function (t, k) {
        if (typeof k === "string") {
          if (/^(0|[1-9][0-9]*)$/.test(k)) return +k < listFn().length;
          if (namedFn(listFn(), k) !== undefined) return true;
        }
        return Reflect.has(t, k);
      },
      ownKeys: function () {
        var items = listFn(), ks = [], nm = keysFn(), i;
        for (i = 0; i < items.length; i++) ks.push(String(i));
        for (i = 0; i < nm.length; i++) if (ks.indexOf(nm[i]) < 0) ks.push(nm[i]);
        return ks;
      },
      getOwnPropertyDescriptor: function (t, k) {
        if (typeof k === "string") {
          var items = listFn();
          if (/^(0|[1-9][0-9]*)$/.test(k)) {
            var i = +k;
            if (i < items.length) return { value: items[i], writable: false, enumerable: true, configurable: true };
          }
          var hit = namedFn(items, k);
          // [LegacyUnenumerableNamedProperties]: the named accessors are own
          // props but NOT enumerable (measured ch.namedDesc=e0, ah.dx=e0)
          if (hit !== undefined) return { value: hit, writable: false, enumerable: false, configurable: true };
        }
        return Reflect.getOwnPropertyDescriptor(t, k);
      },
      // Chrome's synthesized own props are non-writable: writes and deletes
      // report success and change nothing observable.
      set: function () { return true; },
      defineProperty: function () { return true; },
      deleteProperty: function () { return true; },
      setPrototypeOf: function () { return true; },
      preventExtensions: function () { return true; },
    });
    __VIEWS.set(proxy, { list: listFn, named: namedFn, keys: keysFn, kind: kind });
    return proxy;
  }
  function elKids(n) {
    var out = [], k = (n && n.childNodes) || [];
    for (var i = 0; i < k.length; i++) if (k[i] && k[i].nodeType === 1) out.push(k[i]);
    return out;
  }
  function idNameOf(el) { var a = el && __mr(el, "attrs"); if (!a) return null; return (a.id || a.name) ? String(a.id || a.name) : null; }
  function childrenView(n) {
    var c = __g(n, "_collChildren");
    if (c) return c;
    var owner = n;
    c = mkView("HTMLCollection", "coll",
      function () { return elKids(owner); },
      function (items, key) {
        var k = String(key).toLowerCase();
        for (var i = 0; i < items.length; i++) {
          var a = __mr(items[i], "attrs") || {};
          if (String(a.id || "").toLowerCase() === k || String(a.name || "").toLowerCase() === k) return items[i];
        }
        return undefined;
      },
      function () {
        var items = elKids(owner), out = [], i, n2;
        for (i = 0; i < items.length; i++) { n2 = idNameOf(items[i]); if (n2) out.push(n2); }
        return out;
      });
    __hide(n, "_collChildren", c);
    return c;
  }
  // NamedNodeMap: live over element.attrs. Attr objects are cached per element
  // so identity is stable (measured ah.id1 / ah.itemId), while value reads the
  // element's live store (measured ah.valSet / n.idAfterSet).
  function attrItem(el, name) {
    var cache = __g(el, "_attrCache");
    if (!cache) { cache = {}; __hide(el, "_attrCache", cache); }
    var a = cache[name];
    if (!a) { a = ncAttr(name, (__mr(el, "attrs") || {})[name], el); cache[name] = a; }
    return a;
  }
  function attrNames(el) {
    var out = [], a = __mr(el, "attrs") || {};
    for (var k in a) if (Object.prototype.hasOwnProperty.call(a, k)) out.push(k);
    return out;
  }
  function attrsView(el) {
    var v = __g(el, "_attrView");
    if (v) return v;
    var owner = el;
    v = mkView("NamedNodeMap", "nnm",
      function () {
        var names = attrNames(owner), out = [];
        for (var i = 0; i < names.length; i++) out.push(attrItem(owner, names[i]));
        return out;
      },
      function (items, key) {
        var k = String(key).toLowerCase(), a = __mr(owner, "attrs") || {};
        for (var nm in a) if (Object.prototype.hasOwnProperty.call(a, nm) && nm.toLowerCase() === k) return attrItem(owner, nm);
        return undefined;
      },
      function () {
        var names = attrNames(owner), out = [], i;
        for (i = 0; i < names.length; i++) out.push(names[i]);
        return out;
      });
    __hide(el, "_attrView", v);
    // removeNamedItem / setNamedItem act on this element
    __VIEWS.get(v).el = el;
    return v;
  }
  var HC = g.HTMLCollection;
  if (HC && HC.prototype) {
    macc(HC, "length", ng("length", function () { return viewState(this, "coll").list().length; }));
    mfn(HC, "item", 1, function (i) {
      var st = viewState(this, "coll"); if (arguments.length < 1) arity("HTMLCollection", "item", 1, 0);
      var n = Number(i) >>> 0, items = st.list();
      return items[n] === undefined ? null : items[n];
    });
    mfn(HC, "namedItem", 1, function (nm) {
      var st = viewState(this, "coll"); if (arguments.length < 1) arity("HTMLCollection", "namedItem", 1, 0);
      var hit = st.named(st.list(), String(nm));
      return hit === undefined ? null : hit;
    });
    if (!Object.getOwnPropertyDescriptor(HC.prototype, Symbol.iterator)) {
      try {
        Object.defineProperty(HC.prototype, Symbol.iterator, {
          value: __native("[Symbol.iterator]", 0, function () {
            var self = this;
            return { i: 0, next: function () { var items = viewState(self, "coll").list(); return this.i < items.length ? { value: items[this.i++], done: false } : { value: undefined, done: true }; } };
          }),
          writable: true, enumerable: false, configurable: true,
        });
      } catch (e) {}
    }
  }
  var NNM = g.NamedNodeMap;
  if (NNM && NNM.prototype) {
    macc(NNM, "length", ng("length", function () { return viewState(this, "nnm").list().length; }));
    mfn(NNM, "getNamedItem", 1, function (nm) {
      var st = viewState(this, "nnm"); if (arguments.length < 1) arity("NamedNodeMap", "getNamedItem", 1, 0);
      var hit = st.named(st.list(), String(nm));
      return hit === undefined ? null : hit;
    });
    mfn(NNM, "getNamedItemNS", 2, function (ns2, nm) {
      var st = viewState(this, "nnm"); if (arguments.length < 2) arity("NamedNodeMap", "getNamedItemNS", 2, arguments.length);
      var hit = st.named(st.list(), String(nm));
      return hit === undefined ? null : hit;
    });
    mfn(NNM, "item", 1, function (i) {
      var st = viewState(this, "nnm"); if (arguments.length < 1) arity("NamedNodeMap", "item", 1, 0);
      var n = Number(i) >>> 0, items = st.list();
      return items[n] === undefined ? null : items[n];
    });
    mfn(NNM, "removeNamedItem", 1, function (nm) {
      var st = viewState(this, "nnm"); if (arguments.length < 1) arity("NamedNodeMap", "removeNamedItem", 1, 0);
      var el = st.el || null, want = String(nm).toLowerCase(), found = null, a;
      for (a in (__mr(el, "attrs") || {})) if (Object.prototype.hasOwnProperty.call(__mr(el, "attrs"), a) && a.toLowerCase() === want) { found = a; break; }
      if (found === null) throw new DOMException("Failed to execute 'removeNamedItem' on 'NamedNodeMap': No item with name '" + String(nm) + "' was found.", "NotFoundError");
      var out = attrItem(el, found);
      delete __mr(el, "attrs")[found];
      return out;
    });
    mfn(NNM, "removeNamedItemNS", 2, function (ns2, nm) { return this.removeNamedItem(nm); });
    mfn(NNM, "setNamedItem", 1, function (a) {
      var st = viewState(this, "nnm"); if (arguments.length < 1) arity("NamedNodeMap", "setNamedItem", 1, 0);
      if (!a || __g(a, "_s7b") !== "Attr") throw new TypeError("Failed to execute 'setNamedItem' on 'NamedNodeMap': parameter 1 is not of type 'Attr'.");
      var el = st.el || null, nm = __g(a, "_sf_name");
      if (el) { __mr(el, "attrs")[nm] = String(a.value); __hide(a, "_sf_ownerElement", el); }
      return null;
    });
    mfn(NNM, "setNamedItemNS", 1, function (a) { return this.setNamedItem(a); });
  }

  // ── DocumentFragment.prototype (ParentNode surface; Chrome's own list) ───
  var DF = g.DocumentFragment;
  if (DF && DF.prototype) {
    function dfGuard(t) { var b = __g(t, "_s7b"); if (b !== "DocumentFragment" && b !== "ShadowRoot") throw inv(); }
    macc(DF, "children", ng("children", function () { dfGuard(this); return childrenView(this); }));
    macc(DF, "firstElementChild", ng("firstElementChild", function () { dfGuard(this); var c = childrenView(this); return c.length ? c[0] : null; }));
    macc(DF, "lastElementChild", ng("lastElementChild", function () { dfGuard(this); var c = childrenView(this); return c.length ? c[c.length - 1] : null; }));
    macc(DF, "childElementCount", ng("childElementCount", function () { dfGuard(this); return childrenView(this).length; }));
    mfn(DF, "append", 0, function () { dfGuard(this); for (var i = 0; i < arguments.length; i++) this.appendChild(asNode(arguments[i])); });
    mfn(DF, "getElementById", 1, function (id) {
      dfGuard(this); if (arguments.length < 1) arity("DocumentFragment", "getElementById", 1, 0);
      return findById(this, String(id));
    });
    mfn(DF, "moveBefore", 2, function (node, ref) {
      dfGuard(this); if (arguments.length < 2) arity("DocumentFragment", "moveBefore", 2, arguments.length);
      if (node && node.parentNode === this) this.removeChild(node);
      this.insertBefore(node, ref);
    });
    mfn(DF, "prepend", 0, function () {
      dfGuard(this);
      var ref = this.firstChild;
      for (var i = 0; i < arguments.length; i++) this.insertBefore(asNode(arguments[i]), ref);
    });
    mfn(DF, "querySelector", 1, function (sel) {
      dfGuard(this); if (arguments.length < 1) arity("DocumentFragment", "querySelector", 1, 0);
      var l = selectorAll(this, String(sel));
      return l[0] || null;
    });
    mfn(DF, "querySelectorAll", 1, function (sel) {
      dfGuard(this); if (arguments.length < 1) arity("DocumentFragment", "querySelectorAll", 1, 0);
      var r = selectorAll(this, String(sel)); return __DCI.__U6NL ? __DCI.__U6NL(r) : r; // U2b-6: NodeList
    });
    mfn(DF, "replaceChildren", 0, function () {
      dfGuard(this);
      this.childNodes.length = 0;
      for (var i = 0; i < arguments.length; i++) this.appendChild(asNode(arguments[i]));
    });
  }

  // ── ShadowRoot.prototype (Chrome order + measured defaults) ─────────────
  var SR = g.ShadowRoot;
  if (SR && SR.prototype) {
    function srGuard(t) { if (__g(t, "_s7b") !== "ShadowRoot") throw inv(); }
    function srHTML(t) { var s = "", k = t.childNodes; for (var i = 0; i < k.length; i++) s += serializeNode(k[i]); return s; }
    function srSetHTML(t, v) {
      t.childNodes.length = 0;
      var nodes = parseFragment(String(v == null ? "" : v));
      for (var i = 0; i < nodes.length; i++) { ncLink(nodes[i], t); t.childNodes.push(nodes[i]); }
    }
    macc(SR, "mode", ng("mode", function () { srGuard(this); return __g(this, "_sf_mode"); }));
    macc(SR, "host", ng("host", function () { srGuard(this); return __g(this, "_sf_host"); }));
    macc(SR, "onslotchange",
      ng("onslotchange", function () { srGuard(this); var h = __g(this, "_sf_onslotchange"); return h === undefined ? null : h; }),
      ns("onslotchange", function (v) {
        srGuard(this);
        var prev = __g(this, "_sf_onslotchange");
        __hide(this, "_sf_onslotchange", typeof v === "function" ? v : null);
        try {
          if (typeof prev === "function") this.removeEventListener("slotchange", prev);
          if (typeof v === "function") this.addEventListener("slotchange", v);
        } catch (e) {}
      }));
    macc(SR, "innerHTML",
      ng("innerHTML", function () { srGuard(this); return srHTML(this); }),
      ns("innerHTML", function (v) { srGuard(this); srSetHTML(this, v); }));
    macc(SR, "delegatesFocus", ng("delegatesFocus", function () { srGuard(this); return !!__g(this, "_sf_delegatesFocus"); }));
    macc(SR, "slotAssignment", ng("slotAssignment", function () { srGuard(this); return __g(this, "_sf_slotAssignment") || "named"; }));
    macc(SR, "serializable", ng("serializable", function () { srGuard(this); return !!__g(this, "_sf_serializable"); }));
    macc(SR, "clonable", ng("clonable", function () { srGuard(this); return !!__g(this, "_sf_clonable"); }));
    macc(SR, "activeElement", ng("activeElement", function () { srGuard(this); return null; }));
    macc(SR, "styleSheets", ng("styleSheets", function () { srGuard(this); return []; }));
    macc(SR, "pointerLockElement", ng("pointerLockElement", function () { srGuard(this); return null; }));
    macc(SR, "fullscreenElement",
      ng("fullscreenElement", function () { srGuard(this); return null; }),
      ns("fullscreenElement", function (v) { srGuard(this); }));
    macc(SR, "adoptedStyleSheets",
      ng("adoptedStyleSheets", function () { srGuard(this); return []; }),
      ns("adoptedStyleSheets", function (v) { srGuard(this); }));
    macc(SR, "pictureInPictureElement", ng("pictureInPictureElement", function () { srGuard(this); return null; }));
    macc(SR, "customElementRegistry", ng("customElementRegistry", function () { srGuard(this); return ncRegistry(); }));
    mfn(SR, "elementFromPoint", 2, function (x, y) {
      srGuard(this); if (arguments.length < 2) arity("ShadowRoot", "elementFromPoint", 2, arguments.length);
      // no layout engine here: the document's own honest answer (null) is the
      // same truth this engine tells for document.elementFromPoint.
      return document.elementFromPoint ? document.elementFromPoint(x, y) : null;
    });
    mfn(SR, "elementsFromPoint", 2, function (x, y) {
      srGuard(this); if (arguments.length < 2) arity("ShadowRoot", "elementsFromPoint", 2, arguments.length);
      return document.elementsFromPoint ? document.elementsFromPoint(x, y) : [];
    });
    mfn(SR, "getAnimations", 0, function () { srGuard(this); return []; });
    mfn(SR, "getHTML", 0, function () { srGuard(this); return srHTML(this); });
    mfn(SR, "getSelection", 0, function () { srGuard(this); return document.getSelection ? document.getSelection() : null; });
    mfn(SR, "setHTMLUnsafe", 1, function (v) {
      srGuard(this); if (arguments.length < 1) arity("ShadowRoot", "setHTMLUnsafe", 1, 0);
      srSetHTML(this, v);
    });
    mfn(SR, "setHTML", 1, function (v) {
      srGuard(this); if (arguments.length < 1) arity("ShadowRoot", "setHTML", 1, 0);
      srSetHTML(this, v);
    });
    macc(SR, "referenceTarget",
      ng("referenceTarget", function () { srGuard(this); return null; }),
      ns("referenceTarget", function (v) { srGuard(this); }));
  }

  // ── custom-element registry (document / Element / ShadowRoot expose one;
  // the engine returned "" before — a detector-readable string) ────────────
  var __regInst = null;
  function ncRegistry() {
    if (__regInst) return __regInst;
    var C = g.CustomElementRegistry;
    var r = Object.create((C && C.prototype) || Object.prototype);
    __hide(r, "_s7b", "CustomElementRegistry");
    __hide(r, "_ceMap", {});
    __hide(r, "_ceNames", new WeakMap());
    __regInst = r;
    return r;
  }
  var CER = g.CustomElementRegistry;
  if (CER && CER.prototype) {
    function regGuard(t) { if (__g(t, "_s7b") !== "CustomElementRegistry") { if (__DCI.TRACE_ON) { var __d; try { __d = Object.prototype.toString.call(t) + " ctor=" + (t && t.constructor && t.constructor.name) + " s7b=" + String(__g(t, "_s7b")) + " o7b=" + (t && typeof t === "object" ? String(Object.getOwnPropertyNames(t).slice(0, 6)) : typeof t) + " isW=" + (typeof window !== "undefined" && window.customElements === t); } catch (eD) { __d = "unreadable"; } throw new TypeError("Illegal invocation [regGuard this=" + __d + "]"); } throw inv(); } }
    mfn(CER, "define", 3, function (name, ctor, opts) {
      regGuard(this); if (arguments.length < 2) arity("CustomElementRegistry", "define", 2, arguments.length);
      var map = __g(this, "_ceMap"), names = __g(this, "_ceNames"), n = String(name);
      if (Object.prototype.hasOwnProperty.call(map, n)) throw new DOMException("Failed to execute 'define' on 'CustomElementRegistry': the name \"" + n + "\" has already been used with this registry", "NotSupportedError");
      map[n] = ctor;
      try { names.set(ctor, n); } catch (e) {}
    });
    mfn(CER, "get", 1, function (name) {
      regGuard(this); if (arguments.length < 1) arity("CustomElementRegistry", "get", 1, 0);
      var map = __g(this, "_ceMap"), n = String(name);
      return Object.prototype.hasOwnProperty.call(map, n) ? map[n] : undefined;
    });
    mfn(CER, "getName", 1, function (ctor) {
      regGuard(this); if (arguments.length < 1) arity("CustomElementRegistry", "getName", 1, 0);
      var names = __g(this, "_ceNames");
      try { return names.has(ctor) ? names.get(ctor) : null; } catch (e) { return null; }
    });
    mfn(CER, "whenDefined", 1, function (name) {
      regGuard(this); if (arguments.length < 1) arity("CustomElementRegistry", "whenDefined", 1, 0);
      return Promise.resolve(undefined);
    });
    mfn(CER, "upgrade", 1, function (node) { regGuard(this); });
    mfn(CER, "initialize", 1, function (node) { regGuard(this); return node; });
  }
  // The window-level `customElements` binding was the table-layer stub (defAG,
  // ~7576): a plain object with a noop define. The U2b-6 materializer then
  // chained that stub onto the REBUILT CustomElementRegistry.prototype, whose
  // define/get are brand-guarded — so a page calling
  // window.customElements.define(...) threw Illegal invocation and its module
  // bootstrap died (measured: YouTube's kevlar_base_module — the whole SPA
  // starved: 40/40 scripts ok, net calls 0). The window binding must be the
  // ONE registry, the same branded instance document.customElementRegistry
  // returns. (Class rule: where a real factory exists, it owns the global; the
  // boot fallback stays only as the pre-CER placeholder.)
  try {
    Object.defineProperty(g, "customElements", {
      get: __native("get customElements", 0, function () { return ncRegistry(); }),
      enumerable: true, configurable: true,
    });
  } catch (e) {}
  try {
    var ELP = g.Element && g.Element.prototype;
    if (ELP) {
      Object.defineProperty(ELP, "customElementRegistry", {
        get: __native("get customElementRegistry", 0, function () {
          if (nodeTypeOf(this) === undefined) throw inv();
          return ncRegistry();
        }),
        enumerable: true, configurable: true,
      });
    }
  } catch (e) {}

  // ── Element-level members the class layer redefines ─────────────────────
  // Chrome keeps children/attributes on Element.prototype and neither on
  // HTMLElement (measured nc5: el.childrenOwn=true, ht.childrenOwn=false), so
  // they are defined at the Element level and removed from the HTML level.
  var ELM = g.Element && g.Element.prototype;
  function setOn(proto, nm, desc) { if (proto && desc) { try { Object.defineProperty(proto, nm, desc); } catch (e) {} } }
  function drop(proto, nm) { try { if (proto && Object.prototype.hasOwnProperty.call(proto, nm)) delete proto[nm]; } catch (e) {} }
  function pfn(proto, nm, ln, fn, en) { if (!proto) return; try { Object.defineProperty(proto, nm, { value: __native(nm, ln, fn), writable: true, enumerable: en === undefined ? true : !!en, configurable: true }); } catch (e) {} }
  function pacc(proto, nm, get, set, en) { if (!proto) return; var d = { get: get, enumerable: en === undefined ? true : !!en, configurable: true }; if (set) d.set = set; try { Object.defineProperty(proto, nm, d); } catch (e) {} }
  setOn(ELM, "children", { configurable: true, get: __gGet("children", function () { return childrenView(this); }) });
  setOn(ELM, "attributes", { configurable: true, get: __gGet("attributes", function () { return attrsView(this); }), enumerable: true });
  drop(EP, "children");
  drop(EP, "attributes");

  // ── Node.prototype refinements: parentNode set, textContent, cloneNode ──
  var NT = P("Node");
  if (NT) {
    try {
      Object.defineProperty(NT, "parentNode", {
        get: __native("get parentNode", 0, function () {
          if (nodeTypeOf(this) === undefined) throw inv();
          var d = Object.getOwnPropertyDescriptor(this, "parentNode");
          if (d && d.value !== undefined) return d.value;
          var h = __g(this, "_sf_parentNode");
          if (h !== undefined) return h;
          if (__mr(this, "__parent") !== undefined) return __mr(this, "__parent");
          return null;
        }),
        enumerable: true, configurable: true,
      });
    } catch (e) {}
    try {
      Object.defineProperty(NT, "textContent", {
        get: __native("get textContent", 0, function () {
          var nt = nodeTypeOf(this); if (nt === undefined) throw inv();
          if (nt === 3 || nt === 8) return cdata(this);
          if (nt === 10) return null;               // Chrome: doctype textContent is null
          return textContentAll(this);
        }),
        set: __native("set textContent", 1, function (v) {
          var nt = nodeTypeOf(this); if (nt === undefined) throw inv();
          if (nt === 3 || nt === 8) { cdataSet(this, String(v == null ? "" : v)); return; }
          this.childNodes.length = 0;
          if (v == null || v === "") return;
          var t = makeText(String(v));
          ncLink(t, this);
          this.childNodes.push(t);
          if (__mr(this, "tag") === "script") { try { if (__isConnected(this)) __prepScript(this); } catch (e) {} }
        }),
        enumerable: true, configurable: true,
      });
    } catch (e) {}
    var __epClone = null;
    try { __epClone = EP.cloneNode; } catch (e) {}
    try {
      Object.defineProperty(NT, "cloneNode", {
        value: __native("cloneNode", 1, function (deep) {
          var nt = nodeTypeOf(this); if (nt === undefined) throw inv();
          var c, i, kids;
          if (nt === 3) return makeText(__mr(this, "text") === undefined ? cdata(this) : String(__mr(this, "text")));
          if (nt === 8) return ncComment(cdata(this));
          if (nt === 10) return ncDocType(__g(this, "_sf_name"), __g(this, "_sf_publicId"), __g(this, "_sf_systemId"));
          if (nt === 2) return ncAttr(__g(this, "_sf_name"), __g(this, "_sf_value"), null);
          if (nt === 11) {
            c = ncFragment();
            if (deep) {
              kids = this.childNodes;
              for (i = 0; i < kids.length; i++) {
                var cl = kids[i].cloneNode ? kids[i].cloneNode(true) : null;
                if (cl) { ncLink(cl, c); c.childNodes.push(cl); }
              }
            }
            return c;
          }
          if (__epClone) return __epClone.call(this, deep);
          return null;
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
  }

  // ── fragment-transparent insertion + cycle guard on appendChild/insertBefore
  // Chrome: appending a DocumentFragment inserts its CHILDREN and empties the
  // fragment (measured x.fragInsert); a node may never become its own
  // descendant (measured m.appendChildCycle).
  function containsNode(parent, n) {
    // Chrome: the insertion is refused when the PARENT is the node itself or a
    // descendant of it ("The new child element contains the parent.")
    var p = parent;
    while (p) { if (p === n) return true; p = p.parentNode || __mr(p, "__parent") || null; }
    return false;
  }
  // Chrome's locations (measured nc5): appendChild/insertBefore/removeChild
  // live on Node.prototype only; append/prepend/attachShadow on
  // Element.prototype only. Nothing of this set stays on HTMLElement.
  var NTapp = NT && NT.appendChild, NTins = NT && NT.insertBefore, NTrms = NT && NT.removeChild;
  function hostOf(t) { return (__mr(t, "tag") === "template" && t.content) ? t.content : t; }
  if (NT) {
    try {
      Object.defineProperty(NT, "appendChild", {
        value: __native("appendChild", 1, function (n) {
          if (nodeTypeOf(this) === undefined) throw inv();
          var self = this, host = hostOf(self), i;
          if (n && n.nodeType === 11) {
            var kids = n.childNodes.slice();
            for (i = 0; i < kids.length; i++) { n.childNodes.splice(n.childNodes.indexOf(kids[i]), 1); NTapp.call(self, kids[i]); ncUnspill(kids[i], host); }
            return n;
          }
          if (containsNode(host, n)) throw hierr("Failed to execute 'appendChild' on 'Node': The new child element contains the parent.");
          var r = NTapp.call(self, n);
          ncUnspill(n, host);
          try { __prepConnect(self, n); } catch (e0) {}
          return r;
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
    try {
      Object.defineProperty(NT, "insertBefore", {
        value: __native("insertBefore", 2, function (n, ref) {
          if (nodeTypeOf(this) === undefined) throw inv();
          var self = this, host = hostOf(self), i;
          if (!ref) return self.appendChild(n);
          if (n && n.nodeType === 11) {
            var kids = n.childNodes.slice();
            for (i = 0; i < kids.length; i++) { n.childNodes.splice(n.childNodes.indexOf(kids[i]), 1); NTins.call(self, kids[i], ref); ncUnspill(kids[i], host); }
            return n;
          }
          if (containsNode(host, n)) throw hierr("Failed to execute 'insertBefore' on 'Node': The new child element contains the parent.");
          var r = NTins.call(self, n, ref);
          ncUnspill(n, host);
          try { __prepConnect(self, n); } catch (e0) {}
          return r;
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
    var NTrep = NT.replaceChild;
    try {
      Object.defineProperty(NT, "replaceChild", {
        value: __native("replaceChild", 2, function (n, old) {
          if (nodeTypeOf(this) === undefined) throw inv();
          var self = this;
          if (n && n.nodeType === 11) {
            // Chrome: a fragment replaces `old` with its children (measured
            // x.fragReplCh pattern on the ParentNode surface)
            var kids = n.childNodes.slice(), last = null;
            for (var i = 0; i < kids.length; i++) { n.childNodes.splice(n.childNodes.indexOf(kids[i]), 1); last = NTrep.call(self, kids[i], old); old = last; }
            return last;
          }
          var r = NTrep.call(self, n, old);
          ncUnspill(n, self);
          try { __prepConnect(self, n); } catch (e0) {}
          return r;
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
    try {
      Object.defineProperty(NT, "removeChild", {
        value: __native("removeChild", 1, function (n) {
          if (nodeTypeOf(this) === undefined) throw inv();
          var r = NTrms.call(this, n);
          ncUnspill(n, null);
          return r;
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
  }
  setOn(ELM, "append", {
    value: __native("append", 0, function () { if (nodeTypeOf(this) === undefined) throw inv(); for (var i = 0; i < arguments.length; i++) this.appendChild(asNode(arguments[i])); }),
    writable: true, enumerable: true, configurable: true,
  });
  setOn(ELM, "prepend", {
    value: __native("prepend", 0, function () {
      if (nodeTypeOf(this) === undefined) throw inv();
      var ref = this.firstChild;
      for (var i = 0; i < arguments.length; i++) this.insertBefore(asNode(arguments[i]), ref);
    }),
    writable: true, enumerable: true, configurable: true,
  });
  setOn(ELM, "attachShadow", {
    value: __native("attachShadow", 1, function (init) {
      if (nodeTypeOf(this) === undefined) throw inv();
      var sr = ncShadowRoot(this, init);
      __hide(this, "_sf_shadowRoot", __g(sr, "_sf_mode") === "closed" ? null : sr);
      if (__TRACE) { try { __log("SR-ATTACH id=" + __g(sr, "_srId") + " host=" + ((__mr(this, "attrs") && __mr(this, "attrs").id) || __mr(this, "tag") || "?") + " mode=" + __g(sr, "_sf_mode") + " priorRoot=" + (this.shadowRoot ? "yes" : "no") + " r=" + __realmT(this) + " stk=" + __stk7()); } catch (e) {} }
      return sr;
    }),
    writable: true, enumerable: true, configurable: true,
  });
  drop(EP, "appendChild"); drop(EP, "insertBefore"); drop(EP, "removeChild");
  drop(EP, "append"); drop(EP, "prepend"); drop(EP, "attachShadow");
  drop(ELM, "appendChild"); drop(ELM, "insertBefore"); drop(ELM, "removeChild");
  // ── keep the Element-level copies in sync (SVG elements resolve through
  // Element.prototype, not the HTML level) ────────────────────────────────
  try {
    if (ELM) {
      // Element level: the ParentNode/mixin members only. appendChild /
      // insertBefore / removeChild are Node-level in Chrome and must NOT be
      // own props of Element.prototype (measured el.appendChildOwn=false,
      // probe owners: document.appendChild@NP).
      var sync = ["children", "attributes", "append", "prepend", "attachShadow"];
      for (var si = 0; si < sync.length; si++) {
        var dd = ELM && Object.getOwnPropertyDescriptor(ELM, sync[si]);
        if (!dd) continue;
        setOn(ELM, sync[si], dd);
      }
      drop(ELM, "appendChild"); drop(ELM, "insertBefore"); drop(ELM, "removeChild");
    }
  } catch (e) {}

  // ── EventTarget.when (Chrome 154: returns an Observable) + subscribe ─────
  var ET = g.EventTarget;
  if (ET && ET.prototype) {
    var ETp = ET.prototype;
    // the trio is enumerable in Chrome (measured mp.forin)
    ["addEventListener", "removeEventListener", "dispatchEvent"].forEach(function (nm) {
      try { var d = Object.getOwnPropertyDescriptor(ETp, nm); if (d && !d.enumerable) Object.defineProperty(ETp, nm, { enumerable: true }); } catch (e) {}
    });
    function etGuard(t) {
      var p = t, i = 0;
      while (p && i < 12) { if (p === ETp) return; p = Object.getPrototypeOf(p); i++; }
      throw inv();
    }
    function etWhen(self, type, opts) {
      var t = String(type), ob = Object.create(P("Observable") || Object.prototype);
      __hide(ob, "_s7b", "Observable");
      __hide(ob, "_whenTarget", self);
      __hide(ob, "_whenType", t);
      __hide(ob, "_whenSignal", (opts && opts.signal) || null);
      return ob;
    }
    mfn(ET, "when", 1, function (type, opts) {
      if (arguments.length < 1) arity("EventTarget", "when", 1, 0);
      etGuard(this);
      return etWhen(this, type, opts);
    });
    var OB = g.Observable;
    if (OB && OB.prototype) {
      try { if (!Object.getOwnPropertyDescriptor(OB.prototype, Symbol.toStringTag)) tagOf(OB.prototype, "Observable"); } catch (e) {}
      mfn(OB, "subscribe", 0, function (observer) {
        var tgt = __g(this, "_whenTarget");
        if (!tgt) return undefined;                      // an Observable with no engine source
        var fn = (typeof observer === "function") ? observer
          : (observer && typeof observer.next === "function") ? function (ev) { try { observer.next(ev); } catch (e) {} }
            : null;
        if (!fn) return undefined;
        var sig = __g(this, "_whenSignal"), type = __g(this, "_whenType");
        function handler(ev) { if (sig && sig.aborted) return; fn(ev); }
        try { tgt.addEventListener(type, handler, sig ? { signal: sig } : undefined); }
        catch (e) { try { tgt.addEventListener(type, handler); } catch (e2) {} }
        return undefined;                                // measured: Chrome returns undefined
      });
    }
  }

  // ── prototype parents + tags (the bank's parents resolved through the
  // HTMLElement alias at build time — measured pp.fragParent / pp.srParent) ─
  var NP = P("Node"), CDp = P("CharacterData"), DFp = P("DocumentFragment"), Tp = P("Text"), DPp = P("Document");
  var PAR = [
    ["Attr", NP], ["DocumentType", NP], ["DocumentFragment", NP],
    ["ShadowRoot", DFp], ["CharacterData", NP], ["Text", CDp], ["Comment", CDp],
    ["CDATASection", Tp], ["ProcessingInstruction", CDp],
    ["Document", NP], ["HTMLDocument", DPp], ["XMLDocument", DPp],
    ["Element", NP], ["HTMLElement", P("Element")],
  ];
  for (var pi = 0; pi < PAR.length; pi++) {
    var cp = P(PAR[pi][0]);
    if (cp && PAR[pi][1]) setP(cp, PAR[pi][1]);
  }
  var TAGS = ["Comment", "CharacterData", "DocumentFragment", "Attr", "DocumentType", "ShadowRoot", "HTMLCollection", "NamedNodeMap"];
  for (var ti = 0; ti < TAGS.length; ti++) {
    var tp = P(TAGS[ti]);
    if (tp) tagOf(tp, TAGS[ti]);
  }

  // ── Document.prototype: the class-layer factories ─────────────────────
  // The document's create* bodies live on Document.prototype (the literal's
  // own props are purged in §15.2); the names already exist there, so these
  // redefinitions keep Chrome's own-name order intact.
  var DPR = P("Document");
  function docFn(nm, ln, fn) {
    return __native(nm, ln, function () {
      if (nodeTypeOf(this) !== 9) throw new TypeError("Illegal invocation");
      return fn.apply(this, arguments);
    });
  }
  if (DPR) {
    try {
      Object.defineProperty(DPR, "createComment", {
        value: docFn("createComment", 1, function (t) {
          if (arguments.length < 1) arity("Document", "createComment", 1, 0);
          return ncComment(t);
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
    try {
      Object.defineProperty(DPR, "createDocumentFragment", {
        value: docFn("createDocumentFragment", 0, function () { return ncFragment(); }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
    try {
      Object.defineProperty(DPR, "createAttribute", {
        value: docFn("createAttribute", 1, function (n) {
          if (arguments.length < 1) arity("Document", "createAttribute", 1, 0);
          var raw = String(n), k = raw.toLowerCase();
          if (!/^[a-zA-Z][a-zA-Z0-9._-]*$/.test(k)) {
            var ce = new DOMException("Failed to execute 'createAttribute' on 'Document': The localName provided ('" + raw + "') contains an invalid character.", "InvalidCharacterError");
            try { Error.captureStackTrace(ce); } catch (e1) {}
            throw ce;
          }
          return ncAttr(k, "", null);
        }),
        writable: true, enumerable: true, configurable: true,
      });
    } catch (e) {}
  }

  // ── MessagePort.prototype members: Chrome's are enumerable and native ────
  // shaped (measured mp.forin / mp.close / mp.onmessage …) — the 4.5-era block
  // defined them non-enumerable with source-visible bodies.
  var MPp = g.MessagePort && g.MessagePort.prototype;
  if (MPp) {
    ["onmessage", "onmessageerror"].forEach(function (nm) {
      var d = Object.getOwnPropertyDescriptor(MPp, nm);
      if (!d || !d.get) return;
      var g0 = d.get, s0 = d.set;
      try {
        Object.defineProperty(MPp, nm, {
          get: __native("get " + nm, 0, function () { return g0.call(this); }),
          set: s0 ? __native("set " + nm, 1, function (v) { return s0.call(this, v); }) : undefined,
          enumerable: true, configurable: true,
        });
      } catch (e) {}
    });
    [["close", 0], ["postMessage", 1], ["start", 0]].forEach(function (pair) {
      var nm = pair[0], ln = pair[1], d = Object.getOwnPropertyDescriptor(MPp, nm);
      if (!d || typeof d.value !== "function") return;
      var f0 = d.value;
      try {
        Object.defineProperty(MPp, nm, {
          value: __native(nm, ln, function () { return f0.apply(this, arguments); }),
          writable: true, enumerable: true, configurable: true,
        });
      } catch (e) {}
    });
  }

  // ── Text.prototype: Chrome's own four (measured nc6 txt.own: wholeText,
  // assignedSlot, splitText, constructor). Three of the engine's legacy
  // accessors here — nextSibling / textContent / nodeValue, all Node.prototype
  // members in Chrome — are NON-configurable, so they cannot be removed (the
  // bank also pins Text.prototype non-writable, forbidding a prototype swap);
  // recorded as a residual. The three configurable duplicates and all of the
  // duplicated behaviour are gone: Text answers through Node.prototype now.
  var Tp = P("Text");
  if (Tp) {
    ["parentNode", "parentElement", "ownerDocument", "nextSibling", "textContent", "nodeValue"].forEach(function (nm) { drop(Tp, nm); });
    pacc(Tp, "wholeText", ng("wholeText", function () {
      if (nodeTypeOf(this) !== 3) throw inv();
      var out = cdata(this), p = this.parentNode;
      if (p) {
        var k = p.childNodes, i = k.indexOf(this), j;
        for (j = i - 1; j >= 0 && k[j] && k[j].nodeType === 3; j--) out = cdata(k[j]) + out;
        for (j = i + 1; j < k.length && k[j] && k[j].nodeType === 3; j++) out = out + cdata(k[j]);
      }
      return out;
    }));
    pacc(Tp, "assignedSlot", ng("assignedSlot", function () { if (nodeTypeOf(this) !== 3) throw inv(); return null; }));
    pfn(Tp, "splitText", 1, function (off) {
      if (nodeTypeOf(this) !== 3) throw inv();
      if (arguments.length < 1) arity("Text", "splitText", 1, 0);
      var s = cdata(this), n = Number(off) >>> 0;
      if (n > s.length) {
        var e = new DOMException("Failed to execute 'splitText' on 'Text': The offset " + n + " is larger than the Text node's length.", "IndexSizeError");
        try { Error.captureStackTrace(e); } catch (e1) {}
        throw e;
      }
      var tail = s.slice(n);
      cdataSet(this, s.slice(0, n));
      var nn2 = makeText(tail);
      var p = this.parentNode;
      if (p) p.insertBefore(nn2, this.nextSibling);
      return nn2;
    });
  }

  // ── member ORDER: Chrome's own-name order has constructor LAST on these
  // prototypes (ShadowRoot's sits between setHTML and referenceTarget, and
  // EventTarget lists dispatch before remove — measured nc5 et.own) — the
  // bank/surface passes created them in another order, so each prototype's own
  // props are rebuilt in the measured order.
  // The legacy DOM prototypes' own-name ORDER is Chrome-measured too (nc5,
  // same-hour Chromium: membership already 1:1, order was constructor-first).
  // =====================================================================
  // ST-11 U2: element member relocation + interface tags.
  // Measured same-hour against Chrome 154 (tools/pages/tagproto.html,
  // ttag.html, tagtable.html). Everything here is class-level: the
  // engine's own member shapes, moved from the instances to the class
  // prototypes, with Chrome's kind/name/length/flag conventions. The tag
  // table at the bottom is generated from the Chrome interface sweep.
  // =====================================================================
  (function () {
    var G = globalThis;
    function brand(tag, self) { if (!(self && __mr(self, "tag") === tag)) { throw new TypeError("Illegal invocation"); } }
    function accDef(P, nm, get, set) {
      try {
        var d = { get: __native("get " + nm, 0, get), enumerable: true, configurable: true };
        if (set) { d.set = __native("set " + nm, 1, set); }
        Object.defineProperty(P, nm, d);
      } catch (e) {}
    }
    function methDef(P, nm, ln, f) {
      try { Object.defineProperty(P, nm, { value: __native(nm, ln, f), writable: true, enumerable: true, configurable: true }); } catch (e) {}
    }
    function tagP(tag) { try { var C = tagCtor(tag); return C && C.prototype ? C.prototype : null; } catch (e) { return null; } }
    function attrOf(self) { var a = __mr(self, "attrs"); return a || {}; }
    function boolAttr(a, nm) { var v = a[nm]; return v !== undefined && v !== null && v !== false; }
    function boolAttrSet(a, nm, v) { if (v) { a[nm] = true; } else { try { delete a[nm]; } catch (e) {} } }
    function reorder(P, names, ctor) {
      try {
        var ds = [], i, d;
        for (i = 0; i < names.length; i++) { d = Object.getOwnPropertyDescriptor(P, names[i]); if (d) { ds.push([names[i], d]); } }
        for (i = 0; i < ds.length; i++) { try { delete P[ds[i][0]]; } catch (e) {} }
        for (i = 0; i < ds.length; i++) { try { Object.defineProperty(P, ds[i][0], ds[i][1]); } catch (e) {} }
        if (ctor) {
          try { delete P.constructor; } catch (e) {}
          try { Object.defineProperty(P, "constructor", { value: ctor, writable: true, enumerable: false, configurable: true }); } catch (e) {}
        }
      } catch (e) {}
    }

    // ── script: src/type/async/defer/text on the prototype (Chrome order) ──
    var SP = tagP("script");
    if (SP) {
      accDef(SP, "type", function () { brand("script", this); var v = attrOf(this).type; return v === undefined || v === null ? "" : String(v); },
                          function (v) { brand("script", this); attrOf(this).type = String(v); });
      accDef(SP, "async", function () { brand("script", this); var v = attrOf(this).async; if (v !== undefined) { return v !== null && v !== false; } return !!__g(this, "_sf_async_default"); },
                           function (v) { brand("script", this); var a = attrOf(this); if (v) { a.async = true; } else { try { delete a.async; } catch (e) {} __hide(this, "_sf_async_default", false); } });
      accDef(SP, "defer", function () { brand("script", this); return boolAttr(attrOf(this), "defer"); },
                           function (v) { brand("script", this); boolAttrSet(attrOf(this), "defer", v); });
      try { var tds = Object.getOwnPropertyDescriptor(SP, "text"); if (tds) { Object.defineProperty(SP, "text", { get: tds.get, set: tds.set, enumerable: true, configurable: true }); } } catch (e) {}
      reorder(SP, ["src", "type", "async", "defer", "text"], tagCtor("script"));
    }

    // ── canvas: methods + width/height accessors (Chrome shapes) ──
    var CP = tagP("canvas");
    if (CP) {
      methDef(CP, "getContext", 1, function (t) { brand("canvas", this); if (__TRACE) { try { __log("GC " + String(t)); } catch (e) {} } return canvasGetContext.apply(this, arguments); });
      methDef(CP, "toDataURL", 0, function () { brand("canvas", this); return canvasToDataURL.apply(this, arguments); });
      methDef(CP, "toBlob", 1, function (cb) { brand("canvas", this); if (cb) { setTimeout(function () { cb(null); }, 0); } });
      try {
        var wc = Object.getOwnPropertyDescriptor(CP, "width");
        if (wc && wc.get) { Object.defineProperty(CP, "width", { get: __native("get width", 0, function () { brand("canvas", this); return wc.get.call(this); }), set: wc.set ? __native("set width", 1, function (v) { brand("canvas", this); return wc.set.call(this, v); }) : undefined, enumerable: true, configurable: true }); }
        var hc = Object.getOwnPropertyDescriptor(CP, "height");
        if (hc && hc.get) { Object.defineProperty(CP, "height", { get: __native("get height", 0, function () { brand("canvas", this); return hc.get.call(this); }), set: hc.set ? __native("set height", 1, function (v) { brand("canvas", this); return hc.set.call(this, v); }) : undefined, enumerable: true, configurable: true }); }
      } catch (e) {}
      reorder(CP, ["width", "height", "captureStream", "getContext", "toBlob", "toDataURL", "transferControlToOffscreen"], tagCtor("canvas"));
    }

    // ── form: method/action/elements + requestSubmit/submit/reset/checkValidity ──
    var FP = tagP("form");
    if (FP) {
      accDef(FP, "method", function () { brand("form", this); var v = attrOf(this).method; return v === undefined || v === null ? "get" : String(v); },
                          function (v) { brand("form", this); attrOf(this).method = String(v); });
      accDef(FP, "action", function () { brand("form", this); var v = attrOf(this).action; if (v === undefined || v === null) { try { return resolveUrl(PAGE_URL, ""); } catch (e) { return ""; } } try { return resolveUrl(PAGE_URL, String(v)); } catch (e) { return String(v); } },
                          function (v) { brand("form", this); attrOf(this).action = String(v); });
      accDef(FP, "elements", function () {
        brand("form", this);
        var ctrls = this.querySelectorAll("input,select,textarea,button");
        ctrls.namedItem = function (name) { for (var i = 0; i < this.length; i++) { var el = this[i]; var nm = el.name; if (nm === undefined || nm === "") { var a = __mr(el, "attrs"); nm = a ? a.name : undefined; } if (nm === name) { return el; } } return null; };
        ctrls.item = function (i) { return this[i] || null; };
        return ctrls;
      });
      methDef(FP, "requestSubmit", 0, function () { brand("form", this); __formSubmit(this); });
      methDef(FP, "reset", 0, function () { brand("form", this); });
      methDef(FP, "submit", 0, function () { brand("form", this); __formSubmit(this); });
      methDef(FP, "checkValidity", 0, function () { brand("form", this); return true; });
      reorder(FP, ["action", "method", "elements", "requestSubmit", "reset", "submit", "checkValidity"], tagCtor("form"));
    }

    // ── input: checked/disabled/name/type/value (state in the store) ──
    var IP = tagP("input");
    if (IP) {
      accDef(IP, "checked", function () { brand("input", this); return !!__g(this, "_sf_checked"); },
                         function (v) { brand("input", this); __hide(this, "_sf_checked", !!v); });
      accDef(IP, "disabled", function () { brand("input", this); return boolAttr(attrOf(this), "disabled"); },
                          function (v) { brand("input", this); boolAttrSet(attrOf(this), "disabled", v); });
      accDef(IP, "name", function () { brand("input", this); var v = attrOf(this).name; return v === undefined || v === null ? "" : String(v); },
                      function (v) { brand("input", this); attrOf(this).name = String(v); });
      accDef(IP, "type", function () { brand("input", this); var v = attrOf(this).type; var s = v === undefined || v === null ? "" : String(v).toLowerCase(); return s === "" ? "text" : s; },
                      function (v) { brand("input", this); attrOf(this).type = String(v); });
      accDef(IP, "value", function () { brand("input", this); var v = __g(this, "_sf_value"); return v === undefined ? "" : String(v); },
                       function (v) { brand("input", this); __hide(this, "_sf_value", String(v)); });
      reorder(IP, ["checked", "disabled", "name", "type", "value"], tagCtor("input"));
    }

    // ── textarea: disabled/name/value + class ctor ──
    var TXA = tagP("textarea");
    if (TXA) {
      accDef(TXA, "disabled", function () { brand("textarea", this); return boolAttr(attrOf(this), "disabled"); },
                           function (v) { brand("textarea", this); boolAttrSet(attrOf(this), "disabled", v); });
      accDef(TXA, "name", function () { brand("textarea", this); var v = attrOf(this).name; return v === undefined || v === null ? "" : String(v); },
                       function (v) { brand("textarea", this); attrOf(this).name = String(v); });
      accDef(TXA, "value", function () { brand("textarea", this); var v = __g(this, "_sf_value"); return v === undefined ? "" : String(v); },
                        function (v) { brand("textarea", this); __hide(this, "_sf_value", String(v)); });
      reorder(TXA, ["disabled", "name", "value"], tagCtor("textarea"));
    }

    // ── select: disabled/name/value (same store) ──
    var SE = tagP("select");
    if (SE) {
      accDef(SE, "disabled", function () { brand("select", this); return boolAttr(attrOf(this), "disabled"); },
                          function (v) { brand("select", this); boolAttrSet(attrOf(this), "disabled", v); });
      accDef(SE, "name", function () { brand("select", this); var v = attrOf(this).name; return v === undefined || v === null ? "" : String(v); },
                      function (v) { brand("select", this); attrOf(this).name = String(v); });
      accDef(SE, "value", function () { brand("select", this); var v = __g(this, "_sf_value"); return v === undefined ? "" : String(v); },
                       function (v) { brand("select", this); __hide(this, "_sf_value", String(v)); });
      reorder(SE, ["disabled", "name", "value"], tagCtor("select"));
    }

    // ── img: complete/naturalWidth/naturalHeight getters; width/height native ──
    var IM = tagP("img");
    if (IM) {
      accDef(IM, "complete", function () { brand("img", this); var v = __g(this, "__complete"); return v === undefined ? true : !!v; });
      accDef(IM, "naturalWidth", function () { brand("img", this); var v = __g(this, "__naturalWidth"); return v === undefined ? 0 : v; });
      accDef(IM, "naturalHeight", function () { brand("img", this); var v = __g(this, "__naturalHeight"); return v === undefined ? 0 : v; });
      try {
        var wm = Object.getOwnPropertyDescriptor(IM, "width");
        if (wm && wm.get) { Object.defineProperty(IM, "width", { get: __native("get width", 0, function () { brand("img", this); return wm.get.call(this); }), set: __native("set width", 1, function (v) { brand("img", this); var a = __mr(this, "attrs"); if (a) { a.width = String(v); } }), enumerable: true, configurable: true }); }
        var hm = Object.getOwnPropertyDescriptor(IM, "height");
        if (hm && hm.get) { Object.defineProperty(IM, "height", { get: __native("get height", 0, function () { brand("img", this); return hm.get.call(this); }), set: __native("set height", 1, function (v) { brand("img", this); var a = __mr(this, "attrs"); if (a) { a.height = String(v); } }), enumerable: true, configurable: true }); }
      } catch (e) {}
      reorder(IM, ["src", "width", "height", "naturalWidth", "naturalHeight", "complete"], tagCtor("img"));
    }

    // ── template: content getter + class ctor ──
    var TPL = tagP("template");
    if (TPL) {
      accDef(TPL, "content", function () { brand("template", this); var v = __g(this, "_sf_content"); return v === undefined ? null : v; });
      reorder(TPL, ["content"], tagCtor("template"));
    }

    // ── div: align (Chrome owns it on HTMLDivElement.prototype; nodesurf3
    //    measured [align, constructor] — same-hour) ──
    var DP = tagP("div");
    if (DP) {
      accDef(DP, "align", function () { brand("div", this); var v = attrOf(this).align; return v === undefined || v === null ? "" : String(v); },
                          function (v) { brand("div", this); attrOf(this).align = String(v); });
      reorder(DP, ["align"], tagCtor("div"));
    }

    // ── Window.prototype: Chrome's own surface is exactly [TEMPORARY,
    //    PERSISTENT, constructor] (keycheck2 F8 same-hour: 3:Window). Add
    //    the two storage constants ahead of a trailing constructor. ──
    try {
      var WP = G.Window && G.Window.prototype;
      if (WP) {
        var wpCtor = Object.getOwnPropertyDescriptor(WP, "constructor");
        try { delete WP.constructor; } catch (e) {}
        try { Object.defineProperty(WP, "TEMPORARY", { value: 0, writable: false, enumerable: true, configurable: false }); } catch (e) {}
        try { Object.defineProperty(WP, "PERSISTENT", { value: 1, writable: false, enumerable: true, configurable: false }); } catch (e) {}
        if (wpCtor) { try { Object.defineProperty(WP, "constructor", wpCtor); } catch (e) {} }
        else { try { Object.defineProperty(WP, "constructor", { value: G.Window, writable: true, enumerable: false, configurable: true }); } catch (e) {} }
      }
    } catch (e) {}

    // ===== generated: Chrome 154 interface tags (tagtable sweep, 2026-10-21) =====
    // U2TP: prototype-level tags where Chrome's value == interface name.
    // U2TO: object-level namespace tags (Math, JSON, ...). U2TA/U2TOA: the
    // measured alias exceptions. SVG classes are excluded (parked band).
    var U2TP = "Symbol,Promise,ArrayBuffer,DataView,Map,BigInt,Set,WeakMap,WeakSet,FinalizationRegistry,WeakRef,DisposableStack,AsyncDisposableStack,XPathResult,XPathExpression,XPathEvaluator,XMLSerializer,XMLHttpRequestUpload,XMLHttpRequestEventTarget,XMLHttpRequest,XMLDocument,WritableStreamDefaultWriter,WritableStreamDefaultController,WritableStream,Worker,WindowControlsOverlayGeometryChangeEvent,WindowControlsOverlay,Window,WheelEvent,WebSocket,WebGLVertexArrayObject,WebGLUniformLocation,WebGLTransformFeedback,WebGLTexture,WebGLSync,WebGLShaderPrecisionFormat,WebGLShader,WebGLSampler,WebGLRenderingContext,WebGLRenderbuffer,WebGLQuery,WebGLProgram,WebGLObject,WebGLFramebuffer,WebGLContextEvent,WebGLBuffer,WebGLActiveInfo,WebGL2RenderingContext,WaveShaperNode,VisualViewport,VisibilityStateEntry,VirtualKeyboardGeometryChangeEvent,ViewTransitionTypeSet,ViewTransition,ViewTimeline,VideoPlaybackQuality,VideoFrame,VideoColorSpace,ValidityState,VTTCue,UserActivation,URLSearchParams,URLPattern,URL,UIEvent,TrustedTypePolicyFactory,TrustedTypePolicy,TrustedScriptURL,TrustedScript,TrustedHTML,TreeWalker,TransitionEvent,TransformStreamDefaultController,TransformStream,TrackEvent,TouchList,TouchEvent,Touch,ToggleEvent,TimeRanges,TextUpdateEvent,TextTrackList,TextTrackCueList,TextTrackCue,TextTrack,TextMetrics,TextFormatUpdateEvent,TextFormat,TextEvent,TextEncoderStream,TextEncoder,TextDecoderStream,TextDecoder,Text,TaskSignal,TaskPriorityChangeEvent,TaskController,TaskAttributionTiming,SyncManager,Subscriber,SubmitEvent,StyleSheetList,StyleSheet,StylePropertyMapReadOnly,StylePropertyMap,StorageEvent,Storage,StereoPannerNode,StaticRange,SourceBufferList,SourceBuffer,ShadowRoot,Selection,SecurityPolicyViolationEvent,ScrollTimeline,ScriptProcessorNode,ScreenOrientation,Screen,Scheduling,Scheduler,Response,ResizeObserverSize,ResizeObserverEntry,ResizeObserver,Request,ReportingObserver,ReportBody,ReadableStreamDefaultReader,ReadableStreamDefaultController,ReadableStreamBYOBRequest,ReadableStreamBYOBReader,ReadableStream,ReadableByteStreamController,Range,RadioNodeList,RTCTrackEvent,RTCStatsReport,RTCSessionDescription,RTCSctpTransport,RTCRtpTransceiver,RTCRtpSender,RTCRtpReceiver,RTCPeerConnectionIceEvent,RTCPeerConnectionIceErrorEvent,RTCPeerConnection,RTCIceTransport,RTCIceCandidate,RTCErrorEvent,RTCError,RTCEncodedVideoFrame,RTCEncodedAudioFrame,RTCDtlsTransport,RTCDataChannelEvent,RTCDTMFToneChangeEvent,RTCDTMFSender,RTCCertificate,PromiseRejectionEvent,ProgressEvent,ProcessingInstruction,PopStateEvent,PointerEvent,PluginArray,Plugin,PictureInPictureWindow,PictureInPictureEvent,PermissionsPolicy,Permissions,PermissionStatus,PeriodicWave,PerformanceTimingConfidence,PerformanceTiming,PerformanceSoftNavigation,PerformanceServerTiming,PerformanceResourceTiming,PerformancePaintTiming,PerformanceObserverEntryList,PerformanceObserver,PerformanceNavigationTiming,PerformanceNavigation,PerformanceMeasure,PerformanceMark,PerformanceLongTaskTiming,PerformanceEventTiming,PerformanceEntry,PerformanceElementTiming,Performance,Path2D,PannerNode,PageTransitionEvent,PageRevealEvent,OverconstrainedError,OscillatorNode,OffscreenCanvasRenderingContext2D,OffscreenCanvas,OfflineAudioContext,OfflineAudioCompletionEvent,Observable,NodeList,NodeIterator,Node,NetworkInformation,NavigatorUAData,Navigator,NavigationTransition,NavigationPrecommitController,NavigationHistoryEntry,NavigationDestination,NavigationCurrentEntryChangeEvent,NavigationActivation,Navigation,NavigateEvent,NamedNodeMap,MutationRecord,MutationObserver,MouseEvent,MimeTypeArray,MimeType,MessagePort,MessageEvent,MessageChannel,MediaStreamTrackVideoStats,MediaStreamTrackProcessor,MediaStreamTrackGenerator,MediaStreamTrackEvent,MediaStreamTrackAudioStats,MediaStreamTrack,MediaStreamEvent,MediaStreamAudioSourceNode,MediaStreamAudioDestinationNode,MediaStream,MediaSourceHandle,MediaSource,MediaRecorder,MediaQueryListEvent,MediaQueryList,MediaList,MediaError,MediaEncryptedEvent,MediaElementAudioSourceNode,MediaCapabilities,MathMLElement,Location,LayoutShiftAttribution,LayoutShift,LargestContentfulPaint,KeyframeEffect,KeyboardEvent,IntersectionObserverEntry,IntersectionObserver,InterestEvent,InteractionContentfulPaint,InputEvent,InputDeviceInfo,InputDeviceCapabilities,Ink,ImageData,ImageBitmapRenderingContext,ImageBitmap,IdleDeadline,IIRFilterNode,IDBVersionChangeEvent,IDBTransaction,IDBRequest,IDBRecord,IDBOpenDBRequest,IDBObjectStore,IDBKeyRange,IDBIndex,IDBFactory,IDBDatabase,IDBCursorWithValue,IDBCursor,History,HighlightRegistry,Highlight,Headers,HashChangeEvent,HTMLVideoElement,HTMLUnknownElement,HTMLUListElement,HTMLTrackElement,HTMLTitleElement,HTMLTimeElement,HTMLTextAreaElement,HTMLTemplateElement,HTMLTableSectionElement,HTMLTableRowElement,HTMLTableElement,HTMLTableColElement,HTMLTableCellElement,HTMLTableCaptionElement,HTMLStyleElement,HTMLSpanElement,HTMLSourceElement,HTMLSlotElement,HTMLSelectedContentElement,HTMLSelectElement,HTMLScriptElement,HTMLQuoteElement,HTMLProgressElement,HTMLPreElement,HTMLPictureElement,HTMLParamElement,HTMLParagraphElement,HTMLOutputElement,HTMLOptionsCollection,HTMLOptionElement,HTMLOptGroupElement,HTMLObjectElement,HTMLOListElement,HTMLModElement,HTMLMeterElement,HTMLMetaElement,HTMLMenuElement,HTMLMediaElement,HTMLMarqueeElement,HTMLMapElement,HTMLLinkElement,HTMLLegendElement,HTMLLabelElement,HTMLLIElement,HTMLInputElement,HTMLImageElement,HTMLIFrameElement,HTMLHtmlElement,HTMLHeadingElement,HTMLHeadElement,HTMLHRElement,HTMLFrameSetElement,HTMLFrameElement,HTMLFormElement,HTMLFormControlsCollection,HTMLFontElement,HTMLFieldSetElement,HTMLEmbedElement,HTMLElement,HTMLDocument,HTMLDivElement,HTMLDirectoryElement,HTMLDialogElement,HTMLDetailsElement,HTMLDataListElement,HTMLDataElement,HTMLDListElement,HTMLCollection,HTMLCanvasElement,HTMLButtonElement,HTMLBodyElement,HTMLBaseElement,HTMLBRElement,HTMLAudioElement,HTMLAreaElement,HTMLAnchorElement,HTMLAllCollection,GeolocationPositionError,GeolocationPosition,GeolocationCoordinates,Geolocation,GamepadHapticActuator,GamepadEvent,GamepadButton,Gamepad,GainNode,FormDataEvent,FormData,FontFaceSetLoadEvent,FontFaceSet,FontFace,FocusEvent,FileReader,FileList,File,External,EventTarget,EventSource,EventCounts,Event,ErrorEvent,EncodedVideoChunk,EncodedAudioChunk,ElementInternals,Element,EditContext,DynamicsCompressorNode,DragEvent,DocumentType,DocumentTimeline,DocumentFragment,Document,DelegatedInkTrailPresenter,DelayNode,DecompressionStream,DataTransferItemList,DataTransferItem,DataTransfer,DOMTokenList,DOMStringMap,DOMStringList,DOMRectReadOnly,DOMRectList,DOMRect,DOMQuad,DOMPointReadOnly,DOMPoint,DOMParser,DOMMatrixReadOnly,DOMMatrix,DOMImplementation,DOMException,DOMError,CustomStateSet,CustomEvent,CustomElementRegistry,Crypto,CountQueuingStrategy,ConvolverNode,ContentVisibilityAutoStateChangeEvent,ConstantSourceNode,CompressionStream,CompositionEvent,Comment,CommandEvent,CloseWatcher,CloseEvent,ClipboardEvent,CharacterData,CharacterBoundsUpdateEvent,ChannelSplitterNode,ChannelMergerNode,CaretPosition,CanvasRenderingContext2D,CanvasPattern,CanvasGradient,CanvasCaptureMediaStreamTrack,CSSViewTransitionRule,CSSVariableReferenceValue,CSSUnparsedValue,CSSUnitValue,CSSTranslate,CSSTransition,CSSTransformValue,CSSTransformComponent,CSSSupportsRule,CSSStyleValue,CSSStyleSheet,CSSStyleRule,CSSStyleDeclaration,CSSStartingStyleRule,CSSSkewY,CSSSkewX,CSSSkew,CSSScopeRule,CSSScale,CSSRuleList,CSSRule,CSSRotate,CSSPropertyRule,CSSPositionValue,CSSPositionTryRule,CSSPositionTryDescriptors,CSSPerspective,CSSPageRule,CSSNumericValue,CSSNumericArray,CSSNestedDeclarations,CSSNamespaceRule,CSSMediaRule,CSSMatrixComponent,CSSMathValue,CSSMathSum,CSSMathProduct,CSSMathNegate,CSSMathMin,CSSMathMax,CSSMathInvert,CSSMathClamp,CSSMarginRule,CSSLayerStatementRule,CSSLayerBlockRule,CSSKeywordValue,CSSKeyframesRule,CSSKeyframeRule,CSSImportRule,CSSImageValue,CSSGroupingRule,CSSFontPaletteValuesRule,CSSFontFaceRule,CSSCounterStyleRule,CSSContainerRule,CSSConditionRule,CSSAnimation,CSPViolationReportBody,CDATASection,ByteLengthQueuingStrategy,BrowserCaptureMediaStreamTrack,BroadcastChannel,BlobEvent,Blob,BiquadFilterNode,BeforeUnloadEvent,BeforeInstallPromptEvent,BaseAudioContext,BarProp,AudioWorkletNode,AudioSinkInfo,AudioScheduledSourceNode,AudioProcessingEvent,AudioParamMap,AudioParam,AudioNode,AudioListener,AudioDestinationNode,AudioData,AudioContext,AudioBufferSourceNode,AudioBuffer,Attr,AnimationTimeline,AnimationPlaybackEvent,AnimationEvent,AnimationEffect,Animation,AnalyserNode,AbstractRange,AbortSignal,AbortController,AbsoluteOrientationSensor,Accelerometer,AudioDecoder,AudioEncoder,AudioWorklet,BatteryManager,Cache,CacheStorage,Clipboard,ClipboardChangeEvent,ClipboardItem,CookieChangeEvent,CookieStore,CookieStoreManager,CreateMonitor,Credential,CredentialsContainer,CryptoKey,DeviceMotionEvent,DeviceMotionEventAcceleration,DeviceMotionEventRotationRate,DeviceOrientationEvent,FederatedCredential,GPU,GPUAdapter,GPUAdapterInfo,GPUBindGroup,GPUBindGroupLayout,GPUBuffer,GPUCanvasContext,GPUCommandBuffer,GPUCommandEncoder,GPUCompilationInfo,GPUCompilationMessage,GPUComputePassEncoder,GPUComputePipeline,GPUDevice,GPUDeviceLostInfo,GPUError,GPUExternalTexture,GPUInternalError,GPUOutOfMemoryError,GPUPipelineError,GPUPipelineLayout,GPUQuerySet,GPUQueue,GPURenderBundle,GPURenderBundleEncoder,GPURenderPassEncoder,GPURenderPipeline,GPUSampler,GPUShaderModule,GPUSupportedFeatures,GPUSupportedLimits,GPUTexture,GPUTextureView,GPUUncapturedErrorEvent,GPUValidationError,GravitySensor,Gyroscope,IdleDetector,ImageCapture,ImageDecoder,ImageTrack,ImageTrackList,Keyboard,KeyboardLayoutMap,LinearAccelerationSensor,MIDIAccess,MIDIConnectionEvent,MIDIInput,MIDIInputMap,MIDIMessageEvent,MIDIOutput,MIDIOutputMap,MIDIPort,MediaDeviceInfo,MediaDevices,MediaKeyMessageEvent,MediaKeySession,MediaKeyStatusMap,MediaKeySystemAccess,MediaKeys,NavigationPreloadManager,NavigatorManagedData,OrientationSensor,PasswordCredential,ProtectedAudience,RelativeOrientationSensor,ScreenDetailed,ScreenDetails,Sensor,SensorErrorEvent,ServiceWorkerRegistration,StorageManager,SubtleCrypto,VideoDecoder,VideoEncoder,VirtualKeyboard,WGSLLanguageFeatures,WebTransport,WebTransportBidirectionalStream,WebTransportDatagramDuplexStream,WebTransportError,Worklet,XRDOMOverlayState,XRLayer,XRWebGLBinding,AudioPlaybackStats,AuthenticatorAssertionResponse,AuthenticatorAttestationResponse,AuthenticatorResponse,PublicKeyCredential,CaptureController,CrashReportContext,DevicePosture,DigitalCredential,DocumentPictureInPicture,EyeDropper,FetchLaterResult,FileSystemDirectoryHandle,FileSystemFileHandle,FileSystemHandle,FileSystemWritableFileStream,FileSystemObserver,FontData,FragmentDirective,HID,HIDConnectionEvent,HIDDevice,HIDInputReportEvent,IdentityCredential,IdentityCredentialError,IdentityProvider,NavigatorLogin,LanguageDetector,LanguageModel,Lock,LockManager,ServiceWorker,ServiceWorkerContainer,NotRestoredReasonDetails,NotRestoredReasons,OTPCredential,PaymentAddress,PaymentRequest,PaymentRequestUpdateEvent,PaymentResponse,PaymentManager,PaymentMethodChangeEvent,Presentation,PresentationAvailability,PresentationConnection,PresentationConnectionAvailableEvent,PresentationConnectionCloseEvent,PresentationConnectionList,PresentationReceiver,PresentationRequest,PressureObserver,PressureRecord,Serial,SerialPort,SpeechRecognitionPhrase,StorageBucket,StorageBucketManager,Summarizer,Translator,USB,USBAlternateInterface,USBConfiguration,USBConnectionEvent,USBDevice,USBEndpoint,USBInTransferResult,USBInterface,USBIsochronousInTransferPacket,USBIsochronousInTransferResult,USBIsochronousOutTransferPacket,USBIsochronousOutTransferResult,USBOutTransferResult,WakeLock,WakeLockSentinel,XRAnchor,XRAnchorSet,XRBoundedReferenceSpace,XRCPUDepthInformation,XRCamera,XRDepthInformation,XRFrame,XRHand,XRHitTestResult,XRHitTestSource,XRInputSource,XRInputSourceArray,XRInputSourceEvent,XRInputSourcesChangeEvent,XRJointPose,XRJointSpace,XRLightEstimate,XRLightProbe,XRPose,XRRay,XRReferenceSpace,XRReferenceSpaceEvent,XRRenderState,XRRigidTransform,XRSession,XRSessionEvent,XRSpace,XRSystem,XRTransientInputHitTestResult,XRTransientInputHitTestSource,XRView,XRViewerPose,XRViewport,XRWebGLDepthInformation,XRWebGLLayer,XRCompositionLayer,XRProjectionLayer,XRCubeLayer,XRCylinderLayer,XREquirectLayer,XRLayerEvent,XRQuadLayer,XRSubImage,XRWebGLSubImage,XRPlane,XRPlaneSet,XRVisibilityMaskChangeEvent,XSLTProcessor,AnimationTrigger,BackgroundFetchManager,BackgroundFetchRecord,BackgroundFetchRegistration,CSSFontFeatureValuesRule,CSSFunctionDeclarations,CSSFunctionDescriptors,CSSFunctionRule,CSSPseudoElement,ChapterInformation,CropTarget,DocumentPictureInPictureEvent,Fence,FencedFrameConfig,HTMLFencedFrameElement,HTMLCameraElement,HTMLMicrophoneElement,HTMLGeolocationElement,HTMLUserMediaElement,IntegrityViolationReportBody,LaunchParams,LaunchQueue,MediaMetadata,MediaSession,NodeRange,OpaqueRange,Notification,Origin,PageSwapEvent,PerformanceLongAnimationFrameTiming,PerformanceScriptTiming,RTCDataChannel,PeriodicSyncManager,Profiler,PushManager,PushSubscription,PushSubscriptionOptions,QuotaExceededError,RTCRtpScriptTransform,RemotePlayback,RestrictionTarget,Sanitizer,SharedWorker,SnapEvent,SpeechGrammar,SpeechGrammarList,SpeechRecognition,SpeechRecognitionErrorEvent,SpeechRecognitionEvent,SpeechSynthesis,SpeechSynthesisErrorEvent,SpeechSynthesisEvent,SpeechSynthesisUtterance,SpeechSynthesisVoice,TimelineTrigger,TimelineTriggerRange,TimelineTriggerRangeList,Viewport,WebSocketError,WebSocketStream";
    var U2TO = "JSON,Math,Intl,Atomics,Reflect,CSS,Temporal,WebAssembly,GPUBufferUsage,GPUColorWrite,GPUMapMode,GPUShaderStage,GPUTextureUsage";
    var U2TA = {"Option": "HTMLOptionElement", "Image": "HTMLImageElement", "Audio": "HTMLAudioElement", "webkitURL": "URL", "webkitRTCPeerConnection": "RTCPeerConnection", "webkitMediaStream": "MediaStream", "WebKitMutationObserver": "MutationObserver", "WebKitCSSMatrix": "DOMMatrix", "FeaturePolicy": "PermissionsPolicy", "webkitSpeechGrammar": "SpeechGrammar", "webkitSpeechGrammarList": "SpeechGrammarList", "webkitSpeechRecognition": "SpeechRecognition", "webkitSpeechRecognitionError": "SpeechRecognitionErrorEvent", "webkitSpeechRecognitionEvent": "SpeechRecognitionEvent"};
    var U2TOA = {};
    try {
      var l1 = U2TP.split(",");
      for (var i1 = 0; i1 < l1.length; i1++) { var n1 = l1[i1]; if (!n1) { continue; } var C1 = G[n1]; if (!C1 || !C1.prototype) { continue; } try { var d1 = Object.getOwnPropertyDescriptor(C1.prototype, Symbol.toStringTag); if (!(d1 && d1.value === n1)) { Object.defineProperty(C1.prototype, Symbol.toStringTag, { value: n1, configurable: true }); } } catch (e) {} }
      var l2 = U2TO.split(",");
      for (var i2 = 0; i2 < l2.length; i2++) { var n2 = l2[i2]; if (!n2) { continue; } var C2 = G[n2]; if (!C2) { continue; } try { var d2 = Object.getOwnPropertyDescriptor(C2, Symbol.toStringTag); if (!(d2 && d2.value === n2)) { Object.defineProperty(C2, Symbol.toStringTag, { value: n2, configurable: true }); } } catch (e) {} }
      for (var n3 in U2TA) { var C3 = G[n3]; if (!C3 || !C3.prototype) { continue; } try { var d3 = Object.getOwnPropertyDescriptor(C3.prototype, Symbol.toStringTag); if (!(d3 && d3.value === U2TA[n3])) { Object.defineProperty(C3.prototype, Symbol.toStringTag, { value: U2TA[n3], configurable: true }); } } catch (e) {} }
      for (var n4 in U2TOA) { var C4 = G[n4]; if (!C4) { continue; } try { var d4 = Object.getOwnPropertyDescriptor(C4, Symbol.toStringTag); if (!(d4 && d4.value === U2TOA[n4])) { Object.defineProperty(C4, Symbol.toStringTag, { value: U2TOA[n4], configurable: true }); } } catch (e) {} }
    } catch (e) {}
  })();

  var ORDER_MEASURED = {
    "Element": ["namespaceURI", "prefix", "localName", "tagName", "id", "className", "classList", "slot", "attributes", "shadowRoot", "part", "assignedSlot", "innerHTML", "outerHTML", "scrollTop", "scrollLeft", "scrollWidth", "scrollHeight", "clientTop", "clientLeft", "clientWidth", "clientHeight", "onbeforecopy", "onbeforecut", "onbeforepaste", "onsearch", "elementTiming", "customElementRegistry", "onfullscreenchange", "onfullscreenerror", "onwebkitfullscreenchange", "onwebkitfullscreenerror", "activeViewTransition", "role", "ariaAtomic", "ariaAutoComplete", "ariaBusy", "ariaBrailleLabel", "ariaBrailleRoleDescription", "ariaChecked", "ariaColCount", "ariaColIndex", "ariaColSpan", "ariaCurrent", "ariaDescription", "ariaDisabled", "ariaExpanded", "ariaHasPopup", "ariaHidden", "ariaInvalid", "ariaKeyShortcuts", "ariaLabel", "ariaLevel", "ariaLive", "ariaModal", "ariaMultiLine", "ariaMultiSelectable", "ariaOrientation", "ariaPlaceholder", "ariaPosInSet", "ariaPressed", "ariaReadOnly", "ariaRelevant", "ariaRequired", "ariaRoleDescription", "ariaRowCount", "ariaRowIndex", "ariaRowSpan", "ariaSelected", "ariaSetSize", "ariaSort", "ariaValueMax", "ariaValueMin", "ariaValueNow", "ariaValueText", "children", "firstElementChild", "lastElementChild", "childElementCount", "previousElementSibling", "nextElementSibling", "after", "animate", "append", "attachShadow", "before", "checkVisibility", "closest", "computedStyleMap", "getAnimations", "getAttribute", "getAttributeNS", "getAttributeNames", "getAttributeNode", "getAttributeNodeNS", "getBoundingClientRect", "getClientRects", "getElementsByClassName", "getElementsByTagName", "getElementsByTagNameNS", "getHTML", "hasAttribute", "hasAttributeNS", "hasAttributes", "hasPointerCapture", "insertAdjacentElement", "insertAdjacentHTML", "insertAdjacentText", "matches", "moveBefore", "prepend", "querySelector", "querySelectorAll", "releasePointerCapture", "remove", "removeAttribute", "removeAttributeNS", "removeAttributeNode", "replaceChildren", "replaceWith", "requestFullscreen", "requestPointerLock", "scroll", "scrollBy", "scrollIntoView", "scrollIntoViewIfNeeded", "scrollTo", "setAttribute", "setAttributeNS", "setAttributeNode", "setAttributeNodeNS", "setHTMLUnsafe", "setPointerCapture", "startViewTransition", "toggleAttribute", "webkitMatchesSelector", "webkitRequestFullScreen", "webkitRequestFullscreen", "currentCSSZoom", "ariaColIndexText", "ariaRowIndexText", "ariaActionsElements", "ariaActiveDescendantElement", "ariaControlsElements", "ariaDescribedByElements", "ariaDetailsElements", "ariaErrorMessageElements", "ariaFlowToElements", "ariaLabelledByElements", "ariaNotify", "pseudo", "setHTML", "constructor"],
    "HTMLElement": ["title", "lang", "translate", "dir", "hidden", "inert", "accessKey", "draggable", "spellcheck", "autocapitalize", "editContext", "contentEditable", "enterKeyHint", "isContentEditable", "inputMode", "virtualKeyboardPolicy", "offsetParent", "offsetTop", "offsetLeft", "offsetWidth", "offsetHeight", "popover", "innerText", "outerText", "writingSuggestions", "onabort", "onbeforeinput", "onbeforematch", "onbeforetoggle", "onblur", "oncancel", "oncanplay", "oncanplaythrough", "onchange", "onclick", "onclose", "oncommand", "oncontentvisibilityautostatechange", "oncontextlost", "oncontextmenu", "oncontextrestored", "oncuechange", "ondblclick", "ondrag", "ondragend", "ondragenter", "ondragleave", "ondragover", "ondragstart", "ondrop", "ondurationchange", "onemptied", "onended", "onerror", "onfocus", "onformdata", "oninput", "oninvalid", "onkeydown", "onkeypress", "onkeyup", "onload", "onloadeddata", "onloadedmetadata", "onloadstart", "onmousedown", "onmouseenter", "onmouseleave", "onmousemove", "onmouseout", "onmouseover", "onmouseup", "onmousewheel", "onpause", "onplay", "onplaying", "onprogress", "onratechange", "onreset", "onresize", "onscroll", "onscrollend", "onsecuritypolicyviolation", "onseeked", "onseeking", "onselect", "onslotchange", "onstalled", "onsubmit", "onsuspend", "ontimeupdate", "ontoggle", "onvolumechange", "onwaiting", "onwebkitanimationend", "onwebkitanimationiteration", "onwebkitanimationstart", "onwebkittransitionend", "onwheel", "onauxclick", "ongotpointercapture", "onlostpointercapture", "onpointerdown", "onpointermove", "onpointerup", "onpointercancel", "onpointerover", "onpointerout", "onpointerenter", "onpointerleave", "onselectstart", "onselectionchange", "onanimationcancel", "onanimationend", "onanimationiteration", "onanimationstart", "ontransitionrun", "ontransitionstart", "ontransitionend", "ontransitioncancel", "onbeforexrselect", "oncopy", "oncut", "onpaste", "dataset", "nonce", "autofocus", "tabIndex", "style", "attributeStyleMap", "attachInternals", "blur", "click", "focus", "hidePopover", "showPopover", "togglePopover", "autocorrect", "onscrollsnapchange", "onscrollsnapchanging", "focusGroup", "focusGroupStart", "constructor", "onpointerrawupdate"],
    "Node": ["nodeType", "nodeName", "baseURI", "isConnected", "ownerDocument", "parentNode", "parentElement", "childNodes", "firstChild", "lastChild", "previousSibling", "nextSibling", "nodeValue", "textContent", "ELEMENT_NODE", "ATTRIBUTE_NODE", "TEXT_NODE", "CDATA_SECTION_NODE", "ENTITY_REFERENCE_NODE", "ENTITY_NODE", "PROCESSING_INSTRUCTION_NODE", "COMMENT_NODE", "DOCUMENT_NODE", "DOCUMENT_TYPE_NODE", "DOCUMENT_FRAGMENT_NODE", "NOTATION_NODE", "DOCUMENT_POSITION_DISCONNECTED", "DOCUMENT_POSITION_PRECEDING", "DOCUMENT_POSITION_FOLLOWING", "DOCUMENT_POSITION_CONTAINS", "DOCUMENT_POSITION_CONTAINED_BY", "DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", "appendChild", "cloneNode", "compareDocumentPosition", "contains", "getRootNode", "hasChildNodes", "insertBefore", "isDefaultNamespace", "isEqualNode", "isSameNode", "lookupNamespaceURI", "lookupPrefix", "normalize", "removeChild", "replaceChild", "constructor"],
    "EventTarget": ["addEventListener", "dispatchEvent", "removeEventListener", "when", "constructor"],
  };
  var ORDER = {
    "Comment": ["constructor"],
    "Text": ["wholeText", "assignedSlot", "splitText", "constructor"],
    "CharacterData": ["data", "length", "previousElementSibling", "nextElementSibling", "after", "appendData", "before", "deleteData", "insertData", "remove", "replaceData", "replaceWith", "substringData", "constructor"],
    "Attr": ["namespaceURI", "prefix", "localName", "name", "value", "ownerElement", "specified", "constructor"],
    "DocumentType": ["name", "publicId", "systemId", "after", "before", "remove", "replaceWith", "constructor"],
    "DocumentFragment": ["children", "firstElementChild", "lastElementChild", "childElementCount", "append", "getElementById", "moveBefore", "prepend", "querySelector", "querySelectorAll", "replaceChildren", "constructor"],
    "HTMLCollection": ["length", "item", "namedItem", "constructor"],
    "NamedNodeMap": ["length", "getNamedItem", "getNamedItemNS", "item", "removeNamedItem", "removeNamedItemNS", "setNamedItem", "setNamedItemNS", "constructor"],
    "ShadowRoot": ["mode", "host", "onslotchange", "innerHTML", "delegatesFocus", "slotAssignment", "serializable", "clonable", "activeElement", "styleSheets", "pointerLockElement", "fullscreenElement", "adoptedStyleSheets", "pictureInPictureElement", "customElementRegistry", "elementFromPoint", "elementsFromPoint", "getAnimations", "getHTML", "getSelection", "setHTMLUnsafe", "setHTML", "constructor", "referenceTarget"],
  };
  for (var mk1 in ORDER_MEASURED) ORDER[mk1] = ORDER_MEASURED[mk1];
  for (var on in ORDER) {
    var op = P(on);
    if (!op) continue;
    var names = ORDER[on], ds = [], oi;
    for (oi = 0; oi < names.length; oi++) ds.push([names[oi], Object.getOwnPropertyDescriptor(op, names[oi])]);
    for (oi = 0; oi < names.length; oi++) { try { delete op[names[oi]]; } catch (e) {} }
    for (oi = 0; oi < names.length; oi++) { if (ds[oi][1]) { try { Object.defineProperty(op, ds[oi][0], ds[oi][1]); } catch (e) {} } }
  }

  // ── NamedNodeMap is iterable in Chrome (measured S.NamedNodeMap.iterator) ─
  if (NNM && NNM.prototype && !Object.getOwnPropertyDescriptor(NNM.prototype, Symbol.iterator)) {
    try {
      Object.defineProperty(NNM.prototype, Symbol.iterator, {
        value: __native("[Symbol.iterator]", 0, function () {
          var self = this;
          return { i: 0, next: function () { var items = viewState(self, "nnm").list(); return this.i < items.length ? { value: items[this.i++], done: false } : { value: undefined, done: true }; } };
        }),
        writable: true, enumerable: false, configurable: true,
      });
    } catch (e) {}
  }

  // document.firstElementChild / lastElementChild: live off the element list
  if (DPR) {
    ["firstElementChild", "lastElementChild"].forEach(function (nm) {
      try {
        Object.defineProperty(DPR, nm, {
          get: __native("get " + nm, 0, function () {
            if (nodeTypeOf(this) !== 9) throw new TypeError("Illegal invocation");
            var k = elKids(this);
            if (!k.length) return null;
            return nm === "firstElementChild" ? k[0] : k[k.length - 1];
          }),
          enumerable: true, configurable: true,
        });
      } catch (e) {}
    });
  }

  // ── Node.prototype.nodeValue: CharacterData answers its data (Chrome's
  // Node.prototype getter is where nodeValue lives; measured c.nodeValue) ──
  if (NT) {
    try {
      Object.defineProperty(NT, "nodeValue", {
        get: __native("get nodeValue", 0, function () {
          var nt = nodeTypeOf(this); if (nt === undefined) throw inv();
          if (nt === 3 || nt === 8) return cdata(this);
          return null;
        }),
        set: __native("set nodeValue", 1, function (v) {
          var nt = nodeTypeOf(this); if (nt === undefined) throw inv();
          if (nt === 3 || nt === 8) cdataSet(this, v == null ? "" : String(v));
        }),
        enumerable: true, configurable: true,
      });
    } catch (e) {}
  }

  // ── document shape seeds (Document.prototype reads the _sf_* stores) ────
  function ncSeedDocument(doc, doctypeNode, root) {
    var kids = [];
    if (doctypeNode) { ncLink(doctypeNode, doc); kids.push(doctypeNode); }
    if (root) kids.push(root);
    hid(doc, "childNodes", kids);
    __nrh(doc, "doctype", doctypeNode || null);
    __nrh(doc, "children", childrenView(doc));
    __nrh(doc, "childElementCount", elKids(doc).length);
    __nrh(doc, "customElementRegistry", ncRegistry());
    try { if (typeof __g(doc, "_rh_documentElement") !== "undefined") __nrh(doc, "documentElement", root); } catch (e) {}
    return kids;
  }
  __DCI.s8 = { native: __native, hide: __hide, g: __g, arity: arity, P: P,
    tagOf: tagOf, setP: setP, brand: __s7k, ng: ng, inv: inv };
  __DCI.nc = {
    comment: ncComment, fragment: ncFragment, doctype: ncDocType, attr: ncAttr,
    shadow: ncShadowRoot, link: ncLink, unspill: ncUnspill, cdataOf: cdata,
    seedDocument: ncSeedDocument, registry: ncRegistry, childrenView: childrenView, attrsView: attrsView,
  };
})();

  // ── §23 ST-7 WH (e2): the measured statics layer on pre-existing classes ─
  // Applied from the generated table (assets/e2_statics_tables.js, tools/
  // gen_e2_statics.py): 139 constants, the 2 array accessors and the 43
  // function statics the (e) sweep banked as fnOwn residue — every descriptor,
  // name, length and own-name ORDER is a same-hour Chromium 154 measurement
  // (e2/e2b/e2c/e2d/e2f/e2g captures; RECEIPTS part 46).
  //
  // Behaviour policy for the function statics: mirror the MEASURED answer;
  // where a native answer depends on a stack the engine does not own (codecs,
  // on-device models, authenticators, a certificate authority) the measured
  // set is encoded and everything outside it answers the negative — the
  // residual is recorded in RECEIPTS, never disguised.
  (function () {
    var S = __DCI && __DCI.s8;
    var T = (typeof globalThis.__E2S === "object" && globalThis.__E2S) || null;
    if (!S || !T) return;
    var g = globalThis, NAT = S.native;
    var hide = S.hide, get = S.g, arity = S.arity, P = S.P, tagOf = S.tagOf, brand = S.brand;
    function cls(n) { var C = g[n]; return (typeof C === "function") ? C : null; }
    function TErr(m) { var e = new TypeError(m); try { Error.captureStackTrace(e); } catch (x) {} return e; }
    // measured: the promise-returning statics REJECT with the V8 message
    // instead of throwing it (WebIDL converts the exception into a rejection).
    function arityErr(iface, fn, need, have) {
      return TErr("Failed to execute '" + fn + "' on '" + iface + "': " + need + " argument" + (need === 1 ? "" : "s") + " required, but only " + have + " present.");
    }
    function Dex(m, n) { var e = new DOMException(m, n); try { Error.captureStackTrace(e); } catch (x) {} return e; }
    function ok(v) { return Promise.resolve(v); }
    function no(e) { return Promise.reject(e); }
    function pend() { return new Promise(function () {}); }   // measured: the native never settles
    function def(o, n, d) { try { Object.defineProperty(o, n, d); } catch (e) {} }
    function sfn(C, nm, ln, fn) { def(C, nm, { value: NAT(nm, ln, fn), writable: true, enumerable: true, configurable: true }); }
    function sadd(C, nm, v) { def(C, nm, { value: v, writable: false, enumerable: true, configurable: false }); }
    function sacc(C, nm, fn) { def(C, nm, { get: NAT("get " + nm, 0, fn), enumerable: true, configurable: true }); }

    // ── 23.1 measured function statics: the behaviour table ────────────────
    var IMPL = {};

    // webkitURL.canParse/parse — real; measured: parse(0 args) → TypeError on
    // 'URL', parse("nope") → null, canParse("/a", base) → true.
    function absUrl(u, base) {
      var s = String(u);
      var ABS = /^[a-zA-Z][a-zA-Z0-9+.\-]*:/;
      if (base === undefined || base === null) {
        // measured: a bare relative string is NOT a serialised URL (Chrome's
        // URL()/canParse/parse throw or answer false/null for "nope")
        if (!ABS.test(s)) throw TErr("Invalid URL");
        return new g.URL(s);
      }
      var bs = String(base);
      if (!ABS.test(bs)) throw TErr("Invalid URL");
      var b = new g.URL(bs);
      if (ABS.test(s)) return new g.URL(s);
      if (s.slice(0, 2) === "//") return new g.URL(b.protocol + s);
      if (s.charAt(0) === "/") return new g.URL(b.origin + s);
      if (s.charAt(0) === "#") return new g.URL(String(b.href).split("#")[0] + s);
      if (s.charAt(0) === "?") return new g.URL(String(b.href).split("?")[0].split("#")[0] + s);
      var p = String(b.href).split("?")[0].split("#")[0];
      return new g.URL(p.slice(0, p.lastIndexOf("/") + 1) + s);
    }
    IMPL["webkitURL.canParse"] = function (u, base) { try { absUrl(u, base); return true; } catch (e) { return false; } };
    IMPL["webkitURL.parse"] = function (u, base) {
      if (arguments.length < 1) arity("URL", "parse", 1, 0);
      try { return absUrl(u, base); } catch (e) { return null; }
    };

    // IDBKeyRange — real key ranges (no IndexedDB stack needed): measured proto
    // accessors + includes, and the measured bound/lowerBound/upperBound/only
    // open-bound asymmetry (the unbounded side is OPEN).
    function krMint(l, u, lo, uo) {
      var k = Object.create(P("IDBKeyRange") || Object.prototype);
      hide(k, "_s7b", "IDBKeyRange");
      hide(k, "_krL", l); hide(k, "_krU", u); hide(k, "_krLo", !!lo); hide(k, "_krUo", !!uo);
      return k;
    }
    function krIncl(self, key) {
      brand(self, "IDBKeyRange");
      var l = get(self, "_krL"), u = get(self, "_krU"), lo = get(self, "_krLo"), uo = get(self, "_krUo");
      if (l !== undefined) { if (typeof key !== typeof l) return false; if (key < l || (key === l && lo)) return false; }
      if (u !== undefined) { if (typeof key !== typeof u) return false; if (key > u || (key === u && uo)) return false; }
      return true;
    }
    IMPL["IDBKeyRange.only"] = function (v) {
      if (arguments.length < 1) arity("IDBKeyRange", "only", 1, 0);
      return krMint(v, v, false, false);
    };
    IMPL["IDBKeyRange.lowerBound"] = function (v, open) {
      if (arguments.length < 1) arity("IDBKeyRange", "lowerBound", 1, 0);
      return krMint(v, undefined, !!open, true);
    };
    IMPL["IDBKeyRange.upperBound"] = function (v, open) {
      if (arguments.length < 1) arity("IDBKeyRange", "upperBound", 1, 0);
      return krMint(undefined, v, true, !!open);
    };
    IMPL["IDBKeyRange.bound"] = function (l, u, lo, uo) {
      if (arguments.length < 2) arity("IDBKeyRange", "bound", 2, arguments.length);
      if (typeof l === "number" && typeof u === "number" && l > u) {
        throw Dex("Failed to execute 'bound' on 'IDBKeyRange': The lower key is greater than the upper key.", "DataError");
      }
      return krMint(l, u, !!lo, !!uo);
    };
    (function () {
      var KRp = P("IDBKeyRange");
      if (!KRp) return;
      var acc = [["lower", "_krL"], ["upper", "_krU"], ["lowerOpen", "_krLo"], ["upperOpen", "_krUo"]];
      acc.forEach(function (a) {
        def(KRp, a[0], { get: NAT("get " + a[0], 0, function () { brand(this, "IDBKeyRange"); return get(this, a[1]); }), enumerable: true, configurable: true });
      });
      def(KRp, "includes", { value: NAT("includes", 1, function (key) { return krIncl(this, key); }), writable: true, enumerable: true, configurable: true });
      try { var cd = Object.getOwnPropertyDescriptor(KRp, "constructor"); if (cd && cd.configurable) { delete KRp.constructor; Object.defineProperty(KRp, "constructor", cd); } } catch (e) {}
      tagOf(KRp, "IDBKeyRange");
    })();

    // MediaRecorder.isTypeSupported — the measured answer table (this box's
    // Chromium: webm/mp4/matroska containers, vp8/vp9/avc1/h264/opus codecs,
    // "" → true, everything else false; e2b/e2f sweep). natively it is a codec
    // probe the engine does not own — the residual is recorded.
    var MR_BASE = { "video/webm": 1, "audio/webm": 1, "video/mp4": 1, "audio/mp4": 1, "video/x-matroska": 1 };
    var MR_CODEC = { vp8: 1, vp9: 1, avc1: 1, avc3: 1, h264: 1, opus: 1, vorbis: 1, mp4a: 1 };
    IMPL["MediaRecorder.isTypeSupported"] = function (t) {
      if (arguments.length < 1) arity("MediaRecorder", "isTypeSupported", 1, 0);
      var s = String(t).trim();
      if (s === "") return true;
      var parts = s.split(";"), base = parts[0].trim().toLowerCase();
      if (!MR_BASE[base]) return false;
      for (var i = 1; i < parts.length; i++) {
        var p = parts[i].trim().toLowerCase();
        if (p.slice(0, 7) !== "codecs=") continue;
        var list = p.slice(7).replace(/"/g, "").split(",");
        for (var j = 0; j < list.length; j++) {
          var c = list[j].trim().toLowerCase();
          if (c !== "" && !MR_CODEC[c.split(".")[0]]) return false;
        }
      }
      return true;
    };

    // ImageDecoder.isTypeSupported → Promise<bool> (measured: png/jpeg/gif/bmp/
    // webp/avif true, svg+xml + unknown false).
    var IM_OK = { "image/png": 1, "image/jpeg": 1, "image/gif": 1, "image/bmp": 1, "image/webp": 1, "image/avif": 1 };
    IMPL["ImageDecoder.isTypeSupported"] = function (t) {
      return ok(!!IM_OK[String(t).trim().toLowerCase().split(";")[0]]);
    };

    // VideoDecoder/VideoEncoder.isConfigSupported → Promise<{config,supported}>
    // with Chrome's measured normalized config (member order included) and the
    // measured codec-set answer.
    var VC_OK = { vp8: 1, vp09: 1, av01: 1, avc1: 1, avc3: 1, h264: 1, hevc: 1 };
    function codecOk(c) { return !!VC_OK[String(c).split(".")[0].toLowerCase()]; }
    IMPL["VideoDecoder.isConfigSupported"] = function (cfg) {
      if (arguments.length < 1) return no(arityErr("VideoDecoder", "isConfigSupported", 1, 0));
      if (!cfg || cfg.codec === undefined) {
        return no(TErr("Failed to execute 'isConfigSupported' on 'VideoDecoder': Failed to read the 'codec' property from 'VideoDecoderConfig': Required member is undefined."));
      }
      var out = { codec: String(cfg.codec) };
      if (cfg.codedHeight !== undefined) out.codedHeight = cfg.codedHeight;
      if (cfg.codedWidth !== undefined) out.codedWidth = cfg.codedWidth;
      out.flip = cfg.flip === undefined ? false : !!cfg.flip;
      out.hardwareAcceleration = cfg.hardwareAcceleration === undefined ? "no-preference" : cfg.hardwareAcceleration;
      out.rotation = cfg.rotation === undefined ? 0 : cfg.rotation;
      return ok({ config: out, supported: codecOk(cfg.codec) });
    };
    IMPL["VideoEncoder.isConfigSupported"] = function (cfg) {
      if (arguments.length < 1) return no(arityErr("VideoEncoder", "isConfigSupported", 1, 0));
      if (!cfg || cfg.height === undefined) {
        return no(TErr("Failed to execute 'isConfigSupported' on 'VideoEncoder': Failed to read the 'height' property from 'VideoEncoderConfig': Required member is undefined."));
      }
      if (cfg.width === undefined) {
        return no(TErr("Failed to execute 'isConfigSupported' on 'VideoEncoder': Failed to read the 'width' property from 'VideoEncoderConfig': Required member is undefined."));
      }
      var out = {
        alpha: cfg.alpha === undefined ? "discard" : cfg.alpha,
        bitrateMode: cfg.bitrateMode === undefined ? "variable" : cfg.bitrateMode,
        codec: String(cfg.codec),
        contentHint: cfg.contentHint === undefined ? "" : cfg.contentHint,
        hardwareAcceleration: cfg.hardwareAcceleration === undefined ? "no-preference" : cfg.hardwareAcceleration,
        height: cfg.height,
        latencyMode: cfg.latencyMode === undefined ? "quality" : cfg.latencyMode,
        width: cfg.width,
      };
      return ok({ config: out, supported: codecOk(cfg.codec) });
    };

    // PublicKeyCredential — the measured capability/availability answers (this
    // box's Chromium: conditional mediation available, no platform
    // authenticator), the measured permission-message order, and the JSON
    // entry points (measured own-name order + defaults; base64url → buffer).
    function b64url(s) {
      var t = String(s === undefined || s === null ? "" : s).replace(/-/g, "+").replace(/_/g, "/");
      while (t.length % 4) t += "=";
      var bin = "";
      try { bin = (typeof atob === "function") ? atob(t) : ""; } catch (e) { bin = ""; }
      var buf = new ArrayBuffer(bin.length), u8 = new Uint8Array(buf);
      for (var i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i) & 255;
      return buf;
    }
    IMPL["PublicKeyCredential.getClientCapabilities"] = function () {
      return ok({
        "conditionalCreate": true, "conditionalGet": true, "extension:appid": true, "extension:appidExclude": true,
        "extension:cmtgKey": false, "extension:credBlob": true, "extension:credProps": true,
        "extension:credentialProtectionPolicy": true, "extension:crossDeviceFallbackUrl": false,
        "extension:enforceCredentialProtectionPolicy": true, "extension:getCredBlob": true,
        "extension:hmacCreateSecret": true, "extension:largeBlob": true, "extension:minPinLength": true,
        "extension:payment": false, "extension:prf": true, "hybridTransport": true, "immediateGet": true,
        "passkeyPlatformAuthenticator": true, "relatedOrigins": true, "signalAllAcceptedCredentials": true,
        "signalCurrentUserDetails": true, "signalUnknownCredential": true, "userVerifyingPlatformAuthenticator": false,
      });
    };
    IMPL["PublicKeyCredential.isConditionalMediationAvailable"] = function () { return ok(true); };
    IMPL["PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable"] = function () { return ok(false); };
    IMPL["PublicKeyCredential.parseCreationOptionsFromJSON"] = function (o) {
      if (arguments.length < 1) arity("PublicKeyCredential", "parseCreationOptionsFromJSON", 1, 0);
      if (!o || o.challenge === undefined) throw TErr("Failed to execute 'parseCreationOptionsFromJSON' on 'PublicKeyCredential': Failed to read the 'challenge' property from 'PublicKeyCredentialCreationOptionsJSON': Required member is undefined.");
      if (o.pubKeyCredParams === undefined) throw TErr("Failed to execute 'parseCreationOptionsFromJSON' on 'PublicKeyCredential': Failed to read the 'pubKeyCredParams' property from 'PublicKeyCredentialCreationOptionsJSON': Required member is undefined.");
      if (o.rp === undefined) throw TErr("Failed to execute 'parseCreationOptionsFromJSON' on 'PublicKeyCredential': Failed to read the 'rp' property from 'PublicKeyCredentialCreationOptionsJSON': Required member is undefined.");
      if (o.user === undefined) throw TErr("Failed to execute 'parseCreationOptionsFromJSON' on 'PublicKeyCredential': Failed to read the 'user' property from 'PublicKeyCredentialCreationOptionsJSON': Required member is undefined.");
      var out = {};
      out.attestation = o.attestation === undefined ? "none" : o.attestation;
      out.challenge = b64url(o.challenge);
      out.excludeCredentials = (o.excludeCredentials || []).map(function (c) {
        return { id: b64url(c && c.id), transports: (c && c.transports) === undefined ? [] : c.transports, type: c && c.type === undefined ? "public-key" : c.type };
      });
      out.hints = o.hints === undefined ? [] : o.hints.slice();
      out.pubKeyCredParams = (o.pubKeyCredParams || []).map(function (p) {
        return { alg: p && p.alg, type: p && p.type === undefined ? "public-key" : p.type };
      });
      out.rp = { name: o.rp && o.rp.name, id: o.rp && o.rp.id };
      out.user = { name: o.user && o.user.name, displayName: o.user && o.user.displayName, id: b64url(o.user && o.user.id) };
      return out;
    };
    IMPL["PublicKeyCredential.parseRequestOptionsFromJSON"] = function (o) {
      if (arguments.length < 1) arity("PublicKeyCredential", "parseRequestOptionsFromJSON", 1, 0);
      if (!o || o.challenge === undefined) throw TErr("Failed to execute 'parseRequestOptionsFromJSON' on 'PublicKeyCredential': Failed to read the 'challenge' property from 'PublicKeyCredentialRequestOptionsJSON': Required member is undefined.");
      var out = {};
      out.allowCredentials = (o.allowCredentials || []).map(function (c) {
        return { id: b64url(c && c.id), transports: (c && c.transports) === undefined ? [] : c.transports, type: c && c.type === undefined ? "public-key" : c.type };
      });
      out.challenge = b64url(o.challenge);
      out.hints = o.hints === undefined ? [] : o.hints.slice();
      out.userVerification = o.userVerification === undefined ? "preferred" : o.userVerification;
      return out;
    };
    (function () {
      var sig = function (m) {
        return function (o) {
          if (arguments.length < 1) return no(arityErr("PublicKeyCredential", m, 1, 0));
          // measured on a non-registered origin (127.0.0.1:8333): the native
          // rejects this way; the engine owns no credential store.
          return no(Dex("This is an invalid domain.", "SecurityError"));
        };
      };
      IMPL["PublicKeyCredential.signalAllAcceptedCredentials"] = sig("signalAllAcceptedCredentials");
      IMPL["PublicKeyCredential.signalCurrentUserDetails"] = sig("signalCurrentUserDetails");
      IMPL["PublicKeyCredential.signalUnknownCredential"] = sig("signalUnknownCredential");
    })();

    // DeviceMotion/DeviceOrientation.requestPermission — measured "granted" on
    // this box's Chromium (headless, no sensor prompts).
    IMPL["DeviceMotionEvent.requestPermission"] = function () { return ok("granted"); };
    IMPL["DeviceOrientationEvent.requestPermission"] = function () { return ok("granted"); };

    // DigitalCredential.userAgentAllowsProtocol — measured true for every
    // string tried (openid4vp, openid4vci, garbage); 0 args → TypeError.
    IMPL["DigitalCredential.userAgentAllowsProtocol"] = function (p) {
      if (arguments.length < 1) arity("DigitalCredential", "userAgentAllowsProtocol", 1, 0);
      return true;
    };

    // Identity* — measured failure paths (configURL read first, then the
    // token refusal); close() is measured to return undefined.
    var IDCFG = "Failed to read the 'configURL' property from 'IdentityProviderConfig': Required member is undefined.";
    IMPL["IdentityCredential.disconnect"] = function (c) {
      if (arguments.length < 1) return no(arityErr("IdentityCredential", "disconnect", 1, 0));
      if (!c || c.configURL === undefined) return no(TErr("Failed to execute 'disconnect' on 'IdentityCredential': " + IDCFG));
      return no(Dex("Not allowed to provide a token.", "NotAllowedError"));
    };
    IMPL["IdentityProvider.close"] = function () { return undefined; };
    IMPL["IdentityProvider.getUserInfo"] = function (c) {
      if (arguments.length < 1) return no(arityErr("IdentityProvider", "getUserInfo", 1, 0));
      if (!c || c.configURL === undefined) return no(TErr("Failed to execute 'getUserInfo' on 'IdentityProvider': " + IDCFG));
      return no(Dex("Not allowed to provide a token.", "NotAllowedError"));
    };
    IMPL["IdentityProvider.resolve"] = function (c) {
      if (arguments.length < 1) return no(arityErr("IdentityProvider", "resolve", 1, 0));
      if (!c || c.configURL === undefined) return no(TErr("Failed to execute 'resolve' on 'IdentityProvider': " + IDCFG));
      return no(Dex("Not allowed to provide a token.", "NotAllowedError"));
    };

    // Built-in AI families — measured: LanguageModel/Summarizer stay PENDING
    // (no on-device model here), Translator answers "downloadable" and refuses
    // creation without a user gesture, LanguageDetector answers
    // "downloadable" (the native flapped to an undefined rejection once —
    // recorded), wrong-typed options raise the measured TypeErrors.
    function notOptsErr(iface, meth, tname, o) {
      return TErr("Failed to execute '" + meth + "' on '" + iface + "': The provided value is not of type '" + tname + "'.");
    }
    function notOpts(iface, meth, tname, o) {
      if (o === null || typeof o !== "object") throw notOptsErr(iface, meth, tname, o);
    }
    IMPL["LanguageModel.availability"] = function (o) { if (arguments.length > 0 && (o === null || typeof o !== "object")) return no(notOptsErr("LanguageModel", "availability", "LanguageModelCreateCoreOptions", o)); return pend(); };
    IMPL["LanguageModel.create"] = function (o) { if (arguments.length > 0 && (o === null || typeof o !== "object")) return no(notOptsErr("LanguageModel", "create", "LanguageModelCreateOptions", o)); return pend(); };
    IMPL["Summarizer.availability"] = function (o) { if (arguments.length > 0 && (o === null || typeof o !== "object")) return no(notOptsErr("Summarizer", "availability", "SummarizerCreateCoreOptions", o)); return pend(); };
    IMPL["Summarizer.create"] = function (o) { if (arguments.length > 0 && (o === null || typeof o !== "object")) return no(notOptsErr("Summarizer", "create", "SummarizerCreateOptions", o)); return pend(); };
    IMPL["LanguageDetector.availability"] = function (o) { if (arguments.length > 0 && (o === null || typeof o !== "object")) return no(notOptsErr("LanguageDetector", "availability", "LanguageDetectorCreateCoreOptions", o)); return ok("downloadable"); };
    IMPL["LanguageDetector.create"] = function (o) {
      if (arguments.length < 1) return no(undefined);                       // measured: rejected undefined
      if (o === null || typeof o !== "object") return no(notOptsErr("LanguageDetector", "create", "LanguageDetectorCreateOptions", o));
      return no(Dex("Requires a user gesture when availability is \"downloading\" or \"downloadable\".", "NotAllowedError"));
    };
    IMPL["Translator.availability"] = function (o) {
      if (arguments.length < 1) return no(arityErr("Translator", "availability", 1, 0));
      if (o === null || typeof o !== "object") return no(notOptsErr("Translator", "availability", "TranslatorCreateCoreOptions", o));
      if (o.sourceLanguage === undefined) return no(TErr("Failed to execute 'availability' on 'Translator': Failed to read the 'sourceLanguage' property from 'TranslatorCreateCoreOptions': Required member is undefined."));
      if (o.targetLanguage === undefined) return no(TErr("Failed to execute 'availability' on 'Translator': Failed to read the 'targetLanguage' property from 'TranslatorCreateCoreOptions': Required member is undefined."));
      return ok("downloadable");
    };
    IMPL["Translator.create"] = function (o) {
      if (arguments.length < 1) return no(arityErr("Translator", "create", 1, 0));
      if (o === null || typeof o !== "object") return no(notOptsErr("Translator", "create", "TranslatorCreateOptions", o));
      return no(Dex("Requires a user gesture when availability is \"downloading\" or \"downloadable\".", "NotAllowedError"));
    };

    // SpeechRecognition (+ the webkit alias) — measured: available({langs}) →
    // "available", install({langs}) → false, wrong-typed options → TypeError.
    function srAvail(o) {
      if (arguments.length < 1) return no(arityErr("SpeechRecognition", "available", 1, 0));
      if (o === null || typeof o !== "object") return no(notOptsErr("SpeechRecognition", "available", "SpeechRecognitionOptions", o));
      return ok("available");
    }
    function srInstall(o) {
      if (arguments.length < 1) return no(arityErr("SpeechRecognition", "install", 1, 0));
      if (o === null || typeof o !== "object") return no(notOptsErr("SpeechRecognition", "install", "SpeechRecognitionOptions", o));
      return ok(false);
    }
    IMPL["SpeechRecognition.available"] = srAvail;
    IMPL["SpeechRecognition.install"] = srInstall;
    IMPL["webkitSpeechRecognition.available"] = srAvail;
    IMPL["webkitSpeechRecognition.install"] = srInstall;

    // RestrictionTarget.fromElement(el) → Promise<RestrictionTarget> (measured
    // instance: no own props, tag [object RestrictionTarget]).
    IMPL["RestrictionTarget.fromElement"] = function (el) {
      if (arguments.length < 1) return no(arityErr("RestrictionTarget", "fromElement", 1, 0));
      if (!el || el.nodeType !== 1) {
        return no(TErr("Failed to execute 'fromElement' on 'RestrictionTarget': The provided value is not of type 'Element'."));
      }
      var rt = Object.create(P("RestrictionTarget") || Object.prototype);
      hide(rt, "_s7b", "RestrictionTarget");
      hide(rt, "_rtEl", el);
      return ok(rt);
    };

    // PaymentRequest — measured: the capabilities call rejects
    // NotSupportedError "The feature is not enabled.", the availability call
    // resolves "unavailable-feature-not-enabled".
    IMPL["PaymentRequest.getSecurePaymentConfirmationCapabilities"] = function () {
      return no(Dex("The feature is not enabled.", "NotSupportedError"));
    };
    IMPL["PaymentRequest.securePaymentConfirmationAvailability"] = function () { return ok("unavailable-feature-not-enabled"); };

    // webkitRTCPeerConnection.generateCertificate → Promise<RTCCertificate>
    // (measured instance: no own props, expires a number, getFingerprints() a
    // sha-256 entry). The fingerprint VALUE cannot be mirrored — the native
    // signs a real certificate and the engine has no key material: the shape is
    // real, the payload is synthesized and recorded as residual.
    function fpVal() {
      var h = "0123456789abcdef", s = "";
      for (var i = 0; i < 32; i++) { var b = (Math.random() * 256) | 0; s += (i ? ":" : "") + h.charAt(b >> 4) + h.charAt(b & 15); }
      return s;
    }
    IMPL["webkitRTCPeerConnection.generateCertificate"] = function (cfg) {
      if (arguments.length < 1) return no(TErr("Failed to execute 'generateCertificate' on 'RTCPeerConnection': 1 argument required, but only 0 present."));
      var cert = Object.create(P("RTCCertificate") || Object.prototype);
      hide(cert, "_s7b", "RTCCertificate");
      hide(cert, "_certExp", Date.now() + 2592000000);
      hide(cert, "_certFp", [{ algorithm: "sha-256", value: fpVal() }]);
      return ok(cert);
    };
    (function () {
      var RCp = P("RTCCertificate");
      if (!RCp) return;
      def(RCp, "expires", { get: NAT("get expires", 0, function () { brand(this, "RTCCertificate"); var v = get(this, "_certExp"); return v === undefined ? 0 : v; }), enumerable: true, configurable: true });
      def(RCp, "getFingerprints", { value: NAT("getFingerprints", 0, function () { brand(this, "RTCCertificate"); var v = get(this, "_certFp"); return v === undefined ? [] : v.slice(); }), writable: true, enumerable: true, configurable: true });
      try { var cd2 = Object.getOwnPropertyDescriptor(RCp, "constructor"); if (cd2 && cd2.configurable) { delete RCp.constructor; Object.defineProperty(RCp, "constructor", cd2); } } catch (e) {}
      tagOf(RCp, "RTCCertificate");
    })();

    // Observable.from — real iteration (measured: array → 1,2,3 then complete;
    // promise → its value then complete; anything else → the measured
    // TypeError). subscribe() gains the source branch; the when-Observable
    // path from §22 is untouched.
    IMPL["Observable.from"] = function (v) {
      if (arguments.length < 1) arity("Observable", "from", 1, 0);
      var ob = Object.create(P("Observable") || Object.prototype);
      hide(ob, "_s7b", "Observable");
      if (v && typeof v.subscribe === "function") return v;
      if (v && typeof v.then === "function") { hide(ob, "_fromP", v); return ob; }
      if (v && typeof v[g.Symbol.iterator] === "function") { hide(ob, "_fromI", v); return ob; }
      if (v && typeof v[g.Symbol.asyncIterator] === "function") { hide(ob, "_fromP", v); return ob; }
      throw TErr("Failed to execute 'from' on 'Observable': Cannot convert value to an Observable. Input value must be an Observable, async iterable, iterable, or Promise.");
    };
    (function () {
      var OBp = P("Observable");
      if (!OBp) return;
      var prev = OBp.subscribe;
      if (typeof prev !== "function") return;
      def(OBp, "subscribe", { value: NAT("subscribe", 0, function (observer) {
        var items = get(this, "_fromI"), pr = get(this, "_fromP");
        if (items === undefined && pr === undefined) return prev.call(this, observer);
        function N(v) { try { if (typeof observer === "function") observer(v); else if (observer && typeof observer.next === "function") observer.next(v); } catch (e) {} }
        function C() { try { if (observer && typeof observer.complete === "function") observer.complete(); } catch (e) {} }
        function E(e) { try { if (observer && typeof observer.error === "function") observer.error(e); } catch (e2) {} }
        if (items !== undefined) {
          try { var it = items[g.Symbol.iterator](), n; while (!(n = it.next()).done) N(n.value); }
          catch (e) { E(e); return undefined; }
          C();
          return undefined;
        }
        pr.then(function (v) { N(v); C(); }, function (e) { E(e); });
        return undefined;
      }), writable: true, enumerable: true, configurable: true });
    })();

    // Origin.from(str) → Origin (measured: a serialized URL parses, anything
    // else raises the measured TypeError).
    IMPL["Origin.from"] = function (s) {
      var u;
      try { u = absUrl(s, undefined); }
      catch (e) { throw TErr("Failed to execute 'from' on 'Origin': The string provided cannot be parsed as a serialized URL."); }
      var o = Object.create(P("Origin") || Object.prototype);
      hide(o, "_s7b", "Origin");
      hide(o, "_origin", String(u.origin));
      return o;
    };

    // ── 23.2 the measured rows ─────────────────────────────────────────────
    for (var i = 0; i < T.k.length; i++) {
      var CK = cls(T.k[i][0]);
      if (CK) sadd(CK, T.k[i][1], T.k[i][2]);
    }
    var ARRS = { "PressureObserver.knownSources": ["cpu"], "PushManager.supportedContentEncodings": ["aes128gcm", "aesgcm"] };
    for (var j = 0; j < T.g.length; j++) {
      var CG = cls(T.g[j][0]), key = T.g[j][0] + "." + T.g[j][1], arr = ARRS[key];
      if (CG && arr) sacc(CG, T.g[j][1], (function (a) { return function () { return a; }; })(arr));
    }
    var unmapped = [];
    for (var m = 0; m < T.f.length; m++) {
      var CF = cls(T.f[m][0]), fkey = T.f[m][0] + "." + T.f[m][1], impl = IMPL[fkey];
      if (!CF) continue;
      if (!impl) { unmapped.push(fkey); continue; }   // never install a guessed body
      sfn(CF, T.f[m][1], T.f[m][3], impl);
    }
    // own-name ORDER: re-append each class's configurable members in Chrome's
    // measured order (length/name/prototype are non-configurable and already
    // first; webkitURL's canParse/parse must precede createObjectURL).
    for (var cname in T.o) {
      if (!Object.prototype.hasOwnProperty.call(T.o, cname)) continue;
      var CO = cls(cname);
      if (!CO) continue;
      var want = T.o[cname];
      for (var w = 0; w < want.length; w++) {
        // length/name/prototype are the function's own non-configurable slots
        // and Chrome lists them first: never move them.
        if (want[w] === "length" || want[w] === "name" || want[w] === "prototype") continue;
        var d = null;
        try { d = Object.getOwnPropertyDescriptor(CO, want[w]); } catch (e) {}
        if (!d || !d.configurable) continue;
        try { delete CO[want[w]]; Object.defineProperty(CO, want[w], d); } catch (e) {}
      }
    }
    if (__TRACE && unmapped.length) { try { __log("E2-UNMAPPED " + unmapped.join(",")); } catch (e) {} }
    // the table is consumed: drop the global (tglob2: the page window's own
    // '__*' names must be '-' apart from the single accepted __DCI)
    try { delete g.__E2S; } catch (e) { try { g.__E2S = undefined; } catch (e2) {} }
  })();


// ── document.currentScript (ST-7 WH (h)) ────────────────────────────────
// Chrome (measured dyn5/dyn6/dyn7): the accessor lives on Document.prototype,
// getter only, enumerable+configurable, never an own prop of the instance. It
// is set for the duration of a PARSER-inserted and of a dynamically-inserted
// CLASSIC script — nested dynamic scripts restore the outer value, the value
// survives the enclosing script's microtask checkpoint, a timer task sees null,
// and module scripts never set it. The engine served a null stub, so analytics
// snippets reading document.currentScript.dataset/src saw nothing.
var __curStack = [];
function __curFind(key, mode) {
  var list = null;
  try { list = document.scripts || []; } catch (e) { list = null; }
  if (!list) return null;
  for (var i = 0; i < list.length; i++) {
    var el = list[i], m = false;
    if (mode === "src") {
      var es = String(el.src || (__mr(el, "attrs") && __mr(el, "attrs").src) || "");
      m = es === key || String((__mr(el, "attrs") && __mr(el, "attrs").src) || "") === key;
      if (!m) m = es.replace(/\/\.\//g, "/") === String(key).replace(/\/\.\//g, "/");
    } else {
      var t = "";
      for (var j = 0; j < el.childNodes.length; j++) if (el.childNodes[j].nodeType === 3) t += __mr(el.childNodes[j], "text");
      m = t === key;
      if (!m && t) m = t.replace(/\r/g, "") === String(key).replace(/\r/g, "");
    }
    if (m) return el;
  }
  return null;
}
__DCI.curBeginKey = function (key, mode) {
  __curStack.push(__g(document, "_rh_curScript", null));
  var el = null;
  try { el = __curFind(String(key), mode); } catch (e) { el = null; }
  __hide(document, "_rh_curScript", el);
  return true;
};
__DCI.curBeginEl = function (el) {
  __curStack.push(__g(document, "_rh_curScript", null));
  __hide(document, "_rh_curScript", el || null);
  return true;
};
__DCI.curEnd = function () {
  var prev = __curStack.length ? __curStack.pop() : null;
  __hide(document, "_rh_curScript", prev === undefined ? null : prev);
  return true;
};
(function () {
  var DP = (globalThis.Document && globalThis.Document.prototype) || null;
  if (!DP) return;
  try {
    Object.defineProperty(DP, "currentScript", {
      get: __native("get currentScript", 0, function () {
        // Chrome: a foreign receiver throws Illegal invocation (measured
        // ctormacc3 Document.currentScript.g0/.r); the census reads the getter
        // off the prototype, so this also keeps its lie-check on the getter path
        if (!(this && this.nodeType === 9)) throw new TypeError("Illegal invocation");
        return __g(document, "_rh_curScript", null);
      }),
      enumerable: true, configurable: true,
    });
  } catch (e) {}
})();

// §26 · XMLHttpRequest family — Chrome-shape wiring (ST-8 unit 6; every value
// below is measured against the same-hour Chromium 154 via xhrsurf{1..7}.html,
// 2026-10-20). Chrome truth: XMLHttpRequest.prototype → XMLHttpRequestEventTarget
// .prototype → EventTarget.prototype; the 7 load-handler accessors live on XET;
// upload is a lazy XMLHttpRequestUpload instance with stable identity; the XHR
// instance itself carries ZERO own props (all values slot-backed, hidden store).
(function () {
  try {
    var G = globalThis;
    var XHR = G.XMLHttpRequest, XET = G.XMLHttpRequestEventTarget, XUP = G.XMLHttpRequestUpload;
    var ETp = G.EventTarget && G.EventTarget.prototype;
    if (!XHR || !XHR.prototype || !XET || !XET.prototype || !XUP || !XUP.prototype || !ETp) return;
    if (__g(XHR.prototype, "_x8wired") === 8) return; // idempotent
    __hide(XHR.prototype, "_x8wired", 8);
    try { if (Object.getPrototypeOf(XET.prototype) !== ETp) Object.setPrototypeOf(XET.prototype, ETp); } catch (e) {}
    try { if (Object.getPrototypeOf(XHR.prototype) !== XET.prototype) Object.setPrototypeOf(XHR.prototype, XET.prototype); } catch (e) {}
    function acc(proto, nm, get, set) {
      try { Object.defineProperty(proto, nm, { get: get, set: set, enumerable: true, configurable: true }); } catch (e) {}
    }
    // the 7 load-handler accessors on XET (measured own-name order)
    var HND = ["onloadstart", "onprogress", "onabort", "onerror", "onload", "ontimeout", "onloadend"];
    for (var hi = 0; hi < HND.length; hi++) {
      (function (nm) {
        acc(XET.prototype, nm,
          __native("get " + nm, 0, function () { var v = __g(this, "_xh" + nm); return v === undefined ? null : v; }),
          __native("set " + nm, 1, function (v) { __hide(this, "_xh" + nm, typeof v === "function" ? v : null); }));
      })(HND[hi]);
    }
    // constructor moves last in own-name order (Chrome inserts handlers first)
    try {
      var cD = Object.getOwnPropertyDescriptor(XET.prototype, "constructor");
      if (cD) {
        delete XET.prototype.constructor;
        Object.defineProperty(XET.prototype, "constructor", { value: cD.value, writable: cD.writable === undefined ? true : cD.writable, enumerable: false, configurable: cD.configurable === undefined ? true : cD.configurable });
      }
    } catch (e) {}
    // onreadystatechange is XHR-only (measured: not on XET)
    acc(XHR.prototype, "onreadystatechange",
      __native("get onreadystatechange", 0, function () { var v = __g(this, "_xhorsc"); return v === undefined ? null : v; }),
      __native("set onreadystatechange", 1, function (v) { __hide(this, "_xhorsc", typeof v === "function" ? v : null); }));
    // values in the hidden store; Chrome keeps none of these as instance own props
    var DAT = [["readyState", 0], ["status", 0], ["statusText", ""], ["responseText", ""], ["response", ""], ["timeout", 0], ["responseURL", ""], ["responseXML", null]];
    for (var di = 0; di < DAT.length; di++) {
      (function (nm, dflt) {
        acc(XHR.prototype, nm,
          __native("get " + nm, 0, function () { var v = __g(this, "_xr_" + nm); return v === undefined ? dflt : v; }),
          __native("set " + nm, 1, function (v) { __hide(this, "_xr_" + nm, v); }));
      })(DAT[di][0], DAT[di][1]);
    }
    // withCredentials: boolean-coerced, default false (measured)
    acc(XHR.prototype, "withCredentials",
      __native("get withCredentials", 0, function () { return __g(this, "_xwc", false); }),
      __native("set withCredentials", 1, function (v) { __hide(this, "_xwc", !!v); }));
    // responseType: enum-validated, default ""; invalid values silently ignored (measured)
    acc(XHR.prototype, "responseType",
      __native("get responseType", 0, function () { return __g(this, "_xrt", ""); }),
      __native("set responseType", 1, function (v) { v = String(v); if (v === "" || v === "text" || v === "json" || v === "arraybuffer" || v === "blob" || v === "document") __hide(this, "_xrt", v); }));
    // upload: lazy XMLHttpRequestUpload instance, stable identity, assignment ignored (measured)
    acc(XHR.prototype, "upload",
      __native("get upload", 0, function () { var u = __g(this, "_xup"); if (u === undefined) { u = Object.create(XUP.prototype); __hide(this, "_xup", u); } return u; }),
      __native("set upload", 1, function (v) {}));
    // overrideMimeType: method, length 1, silent on invalid input (measured)
    try {
      Object.defineProperty(XHR.prototype, "overrideMimeType", {
        value: __native("overrideMimeType", 1, function (m) { __hide(this, "_xmime", String(m)); }),
        writable: true, enumerable: true, configurable: true
      });
    } catch (e) {}
    // WebIDL constants on the prototype and the interface object (measured desc)
    var CONSTS = [["UNSENT", 0], ["OPENED", 1], ["HEADERS_RECEIVED", 2], ["LOADING", 3], ["DONE", 4]];
    for (var ci = 0; ci < CONSTS.length; ci++) {
      (function (nm, val) {
        // ST-11 U2b-4: configurable in passing (own-order pass moves them);
        // the tail seal pass restores the measured non-configurable shape.
        var d = { value: val, writable: false, enumerable: true, configurable: true };
        try { Object.defineProperty(XHR.prototype, nm, d); (__DCI.ordSeal || (__DCI.ordSeal = [])).push([XHR.prototype, nm]); } catch (e) {}
        try { Object.defineProperty(XHR, nm, d); (__DCI.ordSeal || (__DCI.ordSeal = [])).push([XHR, nm]); } catch (e) {}
      })(CONSTS[ci][0], CONSTS[ci][1]);
    }
    // setAttributionReporting: arity gate + accept (measured: no-arg text; any argument accepted)
    try {
      Object.defineProperty(XHR.prototype, "setAttributionReporting", {
        value: __native("setAttributionReporting", 1, function (o) {
          if (arguments.length < 1) throw new TypeError("Failed to execute 'setAttributionReporting' on 'XMLHttpRequest': 1 argument required, but only 0 present.");
        }),
        writable: true, enumerable: true, configurable: true
      });
    } catch (e) {}
    // setPrivateToken: arity gate + the two measured error shapes, verbatim.
    // (Residual, documented: a valid-enum path is not implemented — no valid
    // value was observed against Chrome 154.)
    try {
      Object.defineProperty(XHR.prototype, "setPrivateToken", {
        value: __native("setPrivateToken", 1, function (o) {
          if (arguments.length < 1) throw new TypeError("Failed to execute 'setPrivateToken' on 'XMLHttpRequest': 1 argument required, but only 0 present.");
          var op = (o === null || o === undefined) ? undefined : o.operation;
          if (op === undefined || op === null) throw new TypeError("Failed to execute 'setPrivateToken' on 'XMLHttpRequest': Failed to read the 'operation' property from 'PrivateToken': Required member is undefined.");
          throw new TypeError("Failed to execute 'setPrivateToken' on 'XMLHttpRequest': Failed to read the 'operation' property from 'PrivateToken': The provided value '" + String(op) + "' is not a valid enum value of type OperationType.");
        }),
        writable: true, enumerable: true, configurable: true
      });
    } catch (e) {}
    // @@toStringTag data props (measured: value, not enumerable, configurable)
    try { Object.defineProperty(XHR.prototype, Symbol.toStringTag, { value: "XMLHttpRequest", configurable: true }); } catch (e) {}
    try { Object.defineProperty(XUP.prototype, Symbol.toStringTag, { value: "XMLHttpRequestUpload", configurable: true }); } catch (e) {}
    try { Object.defineProperty(XET.prototype, Symbol.toStringTag, { value: "XMLHttpRequestEventTarget", configurable: true }); } catch (e) {}
  } catch (e) { try { __log("XHR-FAMILY " + e); } catch (e1) {} }
})();
// st8xhr STAMP 1792497350

// ═══ ST-11 U2b-1 (2026-10-21): window attribute surface, function metadata,
// measured singles — same-hour Chromium 154 oracle (deepdiff hunter v2 U1).
// (a) 35 window attributes were plain DATA props; Chrome owns them as
//     ACCESSORS (per-prop flags from the A| rows) with [Replaceable] setter
//     semantics (asg| probes: a set replaces the prop with stored data).
// (b) 24 non-ctor host fns rendered anonymous and kept a stray .prototype;
//     Chrome: named native text, no prototype.
// (c) ctor renders (Audio/NodeRange), lengths, e0->e1 flags, Screen
//     order+EventTarget link, console tag.
(function () {
  var W = window;
  var i, nm, j;
  // ── (a) window attributes → measured accessors ──────────────────────────
  var NAMES = ["window", "self", "top", "parent", "frames", "frameElement", "opener", "closed", "length", "name", "devicePixelRatio", "innerWidth", "innerHeight", "outerWidth", "outerHeight", "screenX", "screenY", "scrollX", "scrollY", "pageXOffset", "pageYOffset", "isSecureContext", "crossOriginIsolated", "origin", "navigator", "screen", "location", "history", "crypto", "localStorage", "sessionStorage", "performance", "trustedTypes", "speechSynthesis", "document"];
  var G1S1 = { self: 1, name: 1, frames: 1, length: 1, opener: 1, parent: 1, origin: 1, screen: 1, innerWidth: 1, innerHeight: 1, scrollX: 1, pageXOffset: 1, scrollY: 1, pageYOffset: 1, screenX: 1, screenY: 1, outerWidth: 1, outerHeight: 1, devicePixelRatio: 1, performance: 1, location: 1 };
  var C0 = { window: 1, top: 1, document: 1, location: 1 };
  var V = {};
  for (i = 0; i < NAMES.length; i++) { nm = NAMES[i]; try { V[nm] = W[nm]; } catch (e) { V[nm] = undefined; } }
  function repSet(nm) {
    // [Replaceable]: assignment replaces the accessor with stored data.
    return __native("set " + nm, 1, function (v) {
      try { Object.defineProperty(W, nm, { value: v, writable: true, enumerable: true, configurable: true }); } catch (e) {}
    });
  }
  function conv(nm, v, hasSet, conf) {
    try {
      var d = { get: (nm === "frames") ? __native("get frames", 0, function () { return W; }) : __native("get " + nm, 0, function () { return v; }), enumerable: true, configurable: true };
      if (nm === "location") d.set = __native("set location", 1, function (v2) { try { W.location.href = String(v2); } catch (e) {} });
      else if (hasSet) d.set = repSet(nm);
      Object.defineProperty(W, nm, d);
      // ST-11 U2b-4: the four c0 getters are configurable in passing so the
      // own-order pass can move them; the tail seal pass restores c0.
      if (!conf) (__DCI.ordSeal || (__DCI.ordSeal = [])).push([W, nm]);
    } catch (e) {}
  }
  for (i = 0; i < NAMES.length; i++) { nm = NAMES[i]; conv(nm, V[nm], !!G1S1[nm], !C0[nm]); }
  // ── (b) function metadata: named natives, no prototype ──────────────────
  var RW = [["matchMedia", 1], ["getComputedStyle", 1], ["scrollTo", 0], ["scrollBy", 0], ["alert", 0], ["confirm", 0], ["prompt", 0], ["open", 0], ["close", 0], ["focus", 0], ["blur", 0], ["postMessage", 1], ["print", 0], ["requestIdleCallback", 1], ["fetch", 1], ["structuredClone", 1], ["reportError", 1], ["NodeFilter", 0], ["setTimeout", 1], ["clearTimeout", 0], ["setInterval", 1], ["clearInterval", 0], ["requestAnimationFrame", 1], ["cancelAnimationFrame", 1]];
  for (i = 0; i < RW.length; i++) {
    nm = RW[i][0];
    try {
      var f = W[nm];
      if (typeof f === "function") {
        var w = __native(nm, RW[i][1], f);
        try {
          var ps = Object.getOwnPropertyNames(f);
          for (j = 0; j < ps.length; j++) {
            if (ps[j] === "name" || ps[j] === "length" || ps[j] === "prototype") continue;
            var pd = Object.getOwnPropertyDescriptor(f, ps[j]);
            if (pd) Object.defineProperty(w, ps[j], pd);
          }
        } catch (e) {}
        W[nm] = w;
      }
    } catch (e) {}
  }
  // ── (c) measured singles ────────────────────────────────────────────────
  // Audio.prototype.constructor is HTMLAudioElement in Chrome; the Range
  // chain's NodeRange layer points at window.NodeRange.
  try { if (W.Audio && W.Audio.prototype && W.HTMLAudioElement) Object.defineProperty(W.Audio.prototype, "constructor", { value: W.HTMLAudioElement, writable: true, enumerable: false, configurable: true }); } catch (e) {}
  try { if (W.Range && W.Range.prototype && W.NodeRange) { var RP1 = Object.getPrototypeOf(W.Range.prototype); if (RP1 && RP1 !== Object.prototype) Object.defineProperty(RP1, "constructor", { value: W.NodeRange, writable: true, enumerable: false, configurable: true }); } } catch (e) {}
  // lengths (Chrome-measured)
  try { Object.defineProperty(W.AudioContext, "length", { value: 0, writable: false, enumerable: false, configurable: true }); } catch (e) {}
  try { Object.defineProperty(W.RTCDTMFToneChangeEvent, "length", { value: 1, writable: false, enumerable: false, configurable: true }); } catch (e) {}
  try { Object.defineProperty(W.Selection.prototype.extend, "length", { value: 1, writable: false, enumerable: false, configurable: true }); } catch (e) {}
  // e0->e1 member flags
  var PFT = [[W.DOMRectList, ["item"]], [W.Plugin, ["item", "namedItem"]], [W.PluginArray, ["item", "namedItem", "refresh"]], [W.MimeTypeArray, ["item", "namedItem"]], [W.NavigatorUAData, ["getHighEntropyValues", "toJSON"]], [W.AudioScheduledSourceNode, ["start", "stop"]], [W.MessageChannel, ["port1", "port2"]]];
  for (i = 0; i < PFT.length; i++) {
    var C = PFT[i][0]; if (!C || !C.prototype) continue;
    for (j = 0; j < PFT[i][1].length; j++) { try { Object.defineProperty(C.prototype, PFT[i][1][j], { enumerable: true }); } catch (e) {} }
  }
  // Screen.prototype: Chrome own order + EventTarget link (depth 3)
  try {
    var SP = W.Screen && W.Screen.prototype;
    if (SP) {
      if (W.EventTarget && W.EventTarget.prototype) { try { Object.setPrototypeOf(SP, W.EventTarget.prototype); } catch (e) {} }
      var ORD = ["availWidth", "availHeight", "width", "height", "colorDepth", "pixelDepth", "availLeft", "availTop", "orientation", "constructor", "onchange", "isExtended"];
      var sd = [];
      for (i = 0; i < ORD.length; i++) sd.push([ORD[i], Object.getOwnPropertyDescriptor(SP, ORD[i])]);
      for (i = 0; i < sd.length; i++) { try { if (sd[i][1]) delete SP[sd[i][0]]; } catch (e) {} }
      for (i = 0; i < sd.length; i++) { try { if (sd[i][1]) Object.defineProperty(SP, sd[i][0], sd[i][1]); } catch (e) {} }
    }
  } catch (e) {}
  // console owns @@toStringTag "console" (Chrome).
  try { if (W.console) Object.defineProperty(W.console, Symbol.toStringTag, { value: "console", writable: false, enumerable: false, configurable: true }); } catch (e) {}
})();
// ST-11 U2b-1 END


// ═══ ST-11 U2b-2 (2026-10-22): the event family — 91 classes, one unit.
// Measured vs same-hour Chromium 154 (deepdiff W| rows + evtprobe 1/2/3):
//   · member surfaces from __DCI.EVNT (assets/event_tables.js): exact own
//     order, kinds, flags, getter names, native fn texts;
//   · instance shape: Chrome instances own ONLY `isTrusted` (a-01g1s0 own
//     accessor); the legacy own data props move to the hidden store, read
//     through the prototype accessors;
//   · construction: per-class accept/reject rules with the measured messages;
//   · options dicts applied per class over the whole prototype chain.
// Chains stay as built (Blink does NOT put EventTarget in event chains).
var __EVINITK = {};
var __EVTRUST = null;
function __evBrand(o) {
  if (o === null || (typeof o !== "object" && typeof o !== "function") || __g(o, "\u0001E") === undefined) throw new TypeError("Illegal invocation");
}
function __evSet(o, k, v) {
  if (o !== null && typeof o === "object" && __g(o, "\u0001E") !== undefined) { __hide(o, "_ev_" + k, v); return v; }
  try { o[k] = v; } catch (e) {}
  return v;
}
function __evCoerce(v, ty) {
  if (ty === "s") return String(v);
  if (ty === "n") { var x = Number(v); return (x === x && x !== Infinity && x !== -Infinity) ? x : 0; }
  if (ty === "b") return !!v;
  return v;
}
var __evCk = -1, __evLast = 0;
function __evNow() {
  // The engine's performance.now() costs ~5.5us/call (measured prof4: 113ms/20k
  // vs Date.now 3ms/20k). timeStamp needs the same timeline, not the same
  // microsecond: Date.now + offset (one perfNow at boot) is monotonic ms and
  // ~15x cheaper; the offset makes the value identical to performance.now()
  // at ms resolution.
  var d = Date.now();
  if (__evCk < 0) { try { __evCk = (typeof performance !== "undefined" && performance.now) ? (performance.now() - d) : 0; } catch (e) { __evCk = 0; } }
  var v = d + __evCk;
  if (v < __evLast) v = __evLast;
  __evLast = v;
  return v;
}
function __evDefault(code) {
  if (!code) return null;
  if (code.charAt(1) === "=") {
    var v = code.slice(2), c = code.charAt(0);
    if (c === "n") { var x = Number(v); return (x === x) ? x : 0; }
    if (c === "s") return v;
    if (c === "b") return v === "1";
    if (c === "o") { if (v === "[]") return []; if (v === "p") return (typeof Promise === "function" ? Promise.resolve() : null); return null; }
    return null;
  }
  var c2 = code.charAt(0);
  if (c2 === "n") return 0;
  if (c2 === "s") return "";
  if (c2 === "b") return code === "b1";
  if (c2 === "u") return undefined;
  return null;
}
function __evGate(name, rule, args, init) {
  if (!rule) return;
  var k = rule.k;
  if (k === "illegal") throw new TypeError("Failed to construct '" + name + "': Illegal constructor");
  if (k === "vt" && !(rule.z && args.length === 0)) throw new TypeError("Failed to construct '" + name + "': The provided value is not of type '" + (rule.vt || (name + "Init")) + "'.");
  if (k === "req") {
    if (args.length < 2) throw new TypeError("Failed to construct '" + name + "': " + (rule.n || 2) + " arguments required, but only " + args.length + " present.");
    if (rule.rm && (init === null || init === undefined || init[rule.rm] === undefined)) throw new TypeError("Failed to construct '" + name + "': Failed to read the '" + rule.rm + "' property from '" + (rule.rt || (name + "Init")) + "': Required member is undefined.");
  }
  if (rule.mn && args.length < rule.mn) {
    var mn = rule.mn;
    throw new TypeError("Failed to construct '" + name + "': " + mn + " argument" + (mn > 1 ? "s" : "") + " required, but only " + args.length + " present.");
  }
}
function __evStore(o) { var st = __WMS.get(o); if (!st) { st = {}; __WMS.set(o, st); } return st; }
function __evInit(o, name, t, init) {
  var st = __evStore(o);
  st["\u0001E"] = 1;
  try { var ns = Object.getOwnPropertyNames(o); for (var q = 0; q < ns.length; q++) { if (ns[q] !== "isTrusted") delete o[ns[q]]; } } catch (e) {}
  st["_ev_type"] = t === undefined ? "" : String(t);
  st["_ev_timeStamp"] = __evNow();
  if (init) {
    var km = __EVINITK[name];
    if (km) for (var ik in init) {
      var tk2 = km[ik];
      if (tk2) { var iv = init[ik]; if (iv !== undefined) st[tk2[0]] = __evCoerce(iv, tk2[1]); }
    }
  }
  if (!__EVTRUST) __EVTRUST = __native("get isTrusted", 0, function () { return !!__g(this, "_ev_isTrusted"); });
  try { Object.defineProperty(o, "isTrusted", { get: __EVTRUST, enumerable: true, configurable: false }); } catch (e) {}
}
function __evMethod(name) {
  switch (name) {
    case "composedPath": return function () { return []; };
    case "preventDefault": return function () { var st = __WMS.get(this); if (st && st["_ev_cancelable"]) st["_ev_defaultPrevented"] = true; };
    case "stopPropagation": case "stopImmediatePropagation": return function () { var st = __WMS.get(this); if (st) st["_ev_cancelBubble"] = true; };
    case "initEvent": return function (t, b, c) { var st = __WMS.get(this) || __evStore(this); st["_ev_type"] = String(t); st["_ev_bubbles"] = !!b; st["_ev_cancelable"] = !!c; st["_ev_defaultPrevented"] = false; st["_ev_cancelBubble"] = false; };
    case "initCustomEvent": return function (t, b, c, d) { var st = __WMS.get(this) || __evStore(this); st["_ev_type"] = String(t); st["_ev_bubbles"] = !!b; st["_ev_cancelable"] = !!c; st["_ev_detail"] = d; };
    case "initUIEvent": return function (t, b, c, v, d) { var st = __WMS.get(this) || __evStore(this); st["_ev_type"] = String(t); st["_ev_bubbles"] = !!b; st["_ev_cancelable"] = !!c; st["_ev_view"] = v === undefined ? null : v; st["_ev_detail"] = d === undefined ? 0 : Number(d); };
    case "getModifierState": return function () { return false; };
    case "initMouseEvent": return function () { var a = arguments, st = __WMS.get(this) || __evStore(this); st["_ev_type"] = String(a[0]); st["_ev_bubbles"] = !!a[1]; st["_ev_cancelable"] = !!a[2]; st["_ev_view"] = a[3] || null; st["_ev_detail"] = Number(a[4] || 0); st["_ev_screenX"] = Number(a[5] || 0); st["_ev_screenY"] = Number(a[6] || 0); st["_ev_clientX"] = Number(a[7] || 0); st["_ev_clientY"] = Number(a[8] || 0); st["_ev_ctrlKey"] = !!a[9]; st["_ev_altKey"] = !!a[10]; st["_ev_shiftKey"] = !!a[11]; st["_ev_metaKey"] = !!a[12]; st["_ev_button"] = Number(a[13] || 0); st["_ev_relatedTarget"] = a[14] === undefined ? null : a[14]; };
    case "initMessageEvent": return function () { var a = arguments, st = __WMS.get(this) || __evStore(this); st["_ev_type"] = String(a[0]); st["_ev_bubbles"] = !!a[1]; st["_ev_cancelable"] = !!a[2]; st["_ev_data"] = a[3] === undefined ? null : a[3]; st["_ev_origin"] = a[4] === undefined ? "" : String(a[4]); st["_ev_lastEventId"] = a[5] === undefined ? "" : String(a[5]); st["_ev_source"] = a[6] === undefined ? null : a[6]; st["_ev_ports"] = a[7] === undefined ? [] : a[7]; };
    default: return function () {};
  }
}
(function () {
  var T = __DCI.EVNT;
  if (!T) return;
  var W = window;
  var PMAP = (typeof WeakMap === "function") ? new WeakMap() : null;
  var nm2, C2, info2, P, q, own;
  function initKeysFor(nm) {
    var out = {}, guard = 0, cur = nm, seen = {};
    while (cur && guard++ < 12) {
      var info = T[cur];
      if (info && info.m) {
        for (var i = 0; i < info.m.length; i++) {
          var tk = info.m[i];
          if (tk[1] !== "a") continue;
          if (seen[tk[0]]) continue;
          seen[tk[0]] = 1;
          var key = "_ev_" + tk[0];
          if (nm === "TouchEvent" && cur === "TouchEvent") key = "_t_" + tk[0];
          out[tk[0]] = [key, (tk[4] || "o").charAt(0)];
        }
      }
      var PP = (W[cur] && W[cur].prototype) || null;
      var P1 = PP ? Object.getPrototypeOf(PP) : null;
      cur = (P1 && PMAP) ? PMAP.get(P1) : null;
    }
    return out;
  }
  // 1 — rebuild every event ctor as a measured-gated wrapper over the original
  for (nm2 in T) {
    info2 = T[nm2];
    C2 = W[nm2];
    if (typeof C2 !== "function" || !C2.prototype) continue;
    if (PMAP) { try { PMAP.set(C2.prototype, nm2); } catch (e) {} }
    var ln2 = parseInt(info2.l, 10);
    if (!(ln2 >= 0)) ln2 = 1;
    var Na = (function (name, Orig, rule) {
      return function () {
        if (new.target === undefined) throw new TypeError("Failed to construct '" + name + "': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
        __evGate(name, rule, arguments, arguments.length > 1 ? arguments[1] : undefined);
        if (rule.b) {
          var mproto = (new.target && new.target.prototype) ? new.target.prototype : Orig.prototype;
          var mo = Object.create(mproto);
          __evInit(mo, name, arguments.length > 0 ? arguments[0] : undefined, arguments.length > 1 ? arguments[1] : undefined);
          return mo;
        }
        var o = Reflect.construct(Orig, arguments, new.target);
        __evInit(o, name, arguments.length > 0 ? arguments[0] : undefined, arguments.length > 1 ? arguments[1] : undefined);
        return o;
      };
    })(nm2, C2, info2.c);
    try { Object.defineProperty(Na, "name", { value: nm2, configurable: true }); } catch (e) {}
    try { Object.defineProperty(Na, "length", { value: ln2, configurable: true }); } catch (e) {}
    try { Object.defineProperty(Na, "prototype", Object.getOwnPropertyDescriptor(C2, "prototype")); } catch (e) {}
    try {
      var ps = Object.getOwnPropertyNames(C2);
      for (var pi = 0; pi < ps.length; pi++) {
        if (ps[pi] === "name" || ps[pi] === "length" || ps[pi] === "prototype") continue;
        var pd = Object.getOwnPropertyDescriptor(C2, ps[pi]);
        if (pd) Object.defineProperty(Na, ps[pi], pd);
      }
    } catch (e) {}
    if (__clearSrc) { try { __clearSrc(Na); } catch (e) {} }
    W[nm2] = Na;
  }
  // 2 — init key maps (own + inherited accessor members)
  for (nm2 in T) { __EVINITK[nm2] = initKeysFor(nm2); }
  // 3 — member surfaces from the table (skip the two already-exact classes)
  for (nm2 in T) {
    info2 = T[nm2];
    C2 = W[nm2];
    if (!C2 || !C2.prototype) continue;
    P = C2.prototype;
    if (info2.x) {
      try { Object.defineProperty(P, "constructor", { value: C2, writable: true, enumerable: false, configurable: true }); } catch (e) {}
      continue;
    }
    own = Object.getOwnPropertyNames(P);
    for (q = 0; q < own.length; q++) { try { delete P[own[q]]; } catch (e) {} }
    for (var mi = 0; mi < info2.m.length; mi++) {
      (function (toks, cls) {
        var name = toks[0], kind = toks[1], fl = toks[2], gs = toks[3], extra = toks[4];
        var off = fl.charAt(0) === "-" ? 1 : 0;
        var c = fl.charAt(off) === "1", en = fl.charAt(off + 1) === "1", w = fl.charAt(off + 2) === "1";
        try {
          if (name === "constructor") { Object.defineProperty(P, "constructor", { value: W[cls], writable: w, enumerable: en, configurable: c }); return; }
          if (kind === "a") {
            var key = "_ev_" + name, dval = __evDefault(extra || "o=null");
            var getter = __native("get " + name, 0, function () {
              if (this === null || typeof this !== "object") throw new TypeError("Illegal invocation");
              var st = __WMS.get(this);
              if (!st || st["\u0001E"] === undefined) throw new TypeError("Illegal invocation");
              if (name === "srcElement") { var tv = st["_ev_target"]; return tv === undefined ? null : tv; }
              if (name === "returnValue") return !st["_ev_defaultPrevented"];
              var v = st[key]; return v === undefined ? dval : v;
            });
            var d = { get: getter, enumerable: en, configurable: c };
            if (gs === "g1s1") {
              d.set = __native("set " + name, 1, function (v) {
                if (this === null || typeof this !== "object") throw new TypeError("Illegal invocation");
                var st = __WMS.get(this);
                if (!st || st["\u0001E"] === undefined) throw new TypeError("Illegal invocation");
                if (name === "returnValue") { st["_ev_defaultPrevented"] = v === false; return; }
                if (name === "cancelBubble") { st["_ev_cancelBubble"] = !!v; return; }
                st[key] = __evCoerce(v, (extra || "o").charAt(0));
              });
            }
            Object.defineProperty(P, name, d);
          } else if (kind === "f") {
            var impl = __evMethod(name);
            Object.defineProperty(P, name, { value: __native(name, parseInt(extra, 10) || 0, function () { __evBrand(this); return impl.apply(this, arguments); }), writable: w, enumerable: en, configurable: c });
          } else if (kind === "d") {
            var val = extra === "undefined" ? undefined : Number(extra);
            Object.defineProperty(P, name, { value: val, writable: w, enumerable: en, configurable: c });
          } else {
            Object.defineProperty(P, name, { value: undefined, writable: w, enumerable: en, configurable: c });
          }
        } catch (e) {}
      })(info2.m[mi], nm2);
    }
  }
  // measured alias: Chrome's webkitSpeechRecognitionEvent IS SpeechRecognitionEvent
  // (identical ctor H + members); the shared proto's constructor must point at
  // the canonical Na (the second materialize pass overwrote it).
  try {
    if (W.webkitSpeechRecognitionEvent && W.SpeechRecognitionEvent) {
      W.webkitSpeechRecognitionEvent = W.SpeechRecognitionEvent;
      Object.defineProperty(W.SpeechRecognitionEvent.prototype, "constructor", { value: W.SpeechRecognitionEvent, writable: true, enumerable: false, configurable: true });
    }
  } catch (e) {}
})();
// ST-11 U2b-2 END


// ═══ ST-11 U2b-3 (2026-10-22): the HTML family — 75-class surface rebuild
// (74 HTML/image/collection classes + DOMTokenList, pulled in for the iframe
// sandbox member), the media chain, and the on* matrix — table-driven
// (assets/html_tables.js). Measured vs same-hour Chromium 154 (deepdiff W|
// self rows + u2b3-surf 1-3). Policy: measured order/flags/kinds/fn-lengths;
// existing impls are REUSED in place; new members get store-backed accessors
// with measured defaults and measured behaviours for the probed calls.
// No per-site logic anywhere in this section.
(function () {
  var T = __DCI.HTMT;
  if (!T) return;
  var W = window;
  var EP = W.Element && W.Element.prototype;
  var HEP = W.HTMLElement && W.HTMLElement.prototype;
  var HC = W.HTMLCollection;
  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }
  function tagObj(o, t) { try { Object.defineProperty(o, Symbol.toStringTag, { value: t, configurable: true }); } catch (e) {} return o; }
  function coerceN(v) { var x = Number(v); return (x === x) ? x : 0; }
  function domEx(msg, name) { try { return new W.DOMException(msg, name); } catch (e) { var er = new Error(msg); try { er.name = name; } catch (e2) {} return er; } }
  function rejectP(msg, name) { return Promise.reject(domEx(msg, name)); }
  function streamStub() { try { if (typeof W.MediaStream === 'function') { var s = new W.MediaStream(); if (s) return s; } } catch (e) {} return tagObj({ id: '', active: false }, 'MediaStream'); }

  // ── measured defaults ────────────────────────────────────────────────
  function validState() {
    var v = tagObj({}, 'ValidityState');
    var ks = ['valueMissing', 'typeMismatch', 'patternMismatch', 'tooLong', 'tooShort', 'rangeUnderflow', 'rangeOverflow', 'stepMismatch', 'badInput', 'customError'];
    for (var i = 0; i < ks.length; i++) { try { Object.defineProperty(v, ks[i], { value: false, enumerable: true, configurable: true }); } catch (e) {} }
    try { Object.defineProperty(v, 'valid', { value: true, enumerable: true, configurable: true }); } catch (e) {}
    return v;
  }
  function mkTokenList(el, attr) {
    var proto = W.DOMTokenList && W.DOMTokenList.prototype;
    var o = Object.create(proto || Object.prototype);
    __hide(o, '_h3c', 'DOMTokenList');
    if (el) { __hide(o, '_h3el', el); __hide(o, '_h3attr', attr); }
    return o;
  }
  function decObj(t) {
    if (t === 'TimeRanges') return tagObj({ length: 0 }, 'TimeRanges');
    if (t === 'TextTrackList') return tagObj({ length: 0 }, 'TextTrackList');
    if (t === 'TextTrack') return tagObj({ kind: '', label: '', language: '', id: '', mode: 'disabled' }, 'TextTrack');
    if (t === 'NodeList') return tagObj({ length: 0 }, 'NodeList');
    if (t === 'ValidityState') return validState();
    if (t === 'RemotePlayback') return tagObj({ state: 'disconnected' }, 'RemotePlayback');
    if (t === 'DOMTokenList') return mkTokenList(null);
    if (t === 'HTMLCollection') return mkColl('HTMLCollection', function () { return []; });
    if (t === 'HTMLFormControlsCollection' || t === 'HTMLOptionsCollection') return mkColl(t, function () { return []; });
    return null;
  }
  function dec(code) {
    if (!code || code === '?') return null;
    if (code === 'null') return null;
    if (code === 'u') return undefined;
    if (code === 'fn') return null;
    var c = code.charAt(0);
    if (c === 'n') return Number(code.slice(2));
    if (c === 's') { var s = code.slice(2); if (s.charAt(s.length - 1) === '~') return ''; return s; }
    if (c === 'b') return code === 'b:1';
    if (code.indexOf('arr:') === 0) return [];
    if (code.indexOf('obj:') === 0) return decObj(code.slice(4));
    return null;
  }
  function coerce(code, v) { if (code === 'null') return v; var c = (code || '').charAt(0); if (c === 'n') return coerceN(v); if (c === 's') return String(v); if (c === 'b') return !!v; return v; }

  // ── guards ───────────────────────────────────────────────────────────
  function guardNode(t) { if (!(t && typeof t.nodeType === 'number')) throw new TypeError('Illegal invocation'); }
  function guardColl(t) { if (t === null || t === undefined || typeof t !== 'object' || __g(t, '_h3c') === undefined) throw new TypeError('Illegal invocation'); }
  function guard(cls, t) { if (cls === 'DOMTokenList' || /Collection$/.test(cls)) guardColl(t); else guardNode(t); }

  // ── generic builders ─────────────────────────────────────────────────
  function genGet(cls, name, code) { return __native('get ' + name, 0, function () { guard(cls, this); var v = __g(this, '_h3_' + name); return v === undefined ? dec(code) : v; }); }
  function genSet(cls, name, code) { return __native('set ' + name, 1, function (v) { guard(cls, this); __hide(this, '_h3_' + name, coerce(code, v)); }); }
  function genFn(cls, name, len) { return __native(name, len, function () { guard(cls, this); }); }

  // ── token lists + collections ────────────────────────────────────────
  function tlTokens(t) {
    var el = __g(t, '_h3el');
    if (el) { var a = __mr(el, 'attrs') || {}; var s = a[__g(t, '_h3attr')]; return s ? String(s).split(/\s+/).filter(Boolean) : []; }
    var arr = __g(t, '_h3toks'); if (!arr) { arr = []; __hide(t, '_h3toks', arr); }
    return arr;
  }
  function tlSave(t, arr) {
    var el = __g(t, '_h3el');
    if (el) { __mr(el, 'attrs')[__g(t, '_h3attr')] = arr.join(' '); return; }
    __hide(t, '_h3toks', arr);
  }
  function mkColl(cls, listFn) {
    var proto = W[cls] && W[cls].prototype;
    var o = Object.create(proto || Object.prototype);
    __hide(o, '_h3c', cls);
    __hide(o, '_h3list', listFn);
    return o;
  }
  function collList(t) { var f = __g(t, '_h3list'); return f ? f() : []; }
  function collItemImpl(t, i, argc, cls) {
    if (argc < 1) throw new TypeError("Failed to execute 'item' on '" + cls + "': 1 argument required, but only 0 present.");
    var l = collList(t), n = Number(i) >>> 0;
    return n < l.length ? l[n] : null;
  }
  function collNamedImpl(t, nm, argc, cls) {
    if (argc < 1) throw new TypeError("Failed to execute 'namedItem' on '" + cls + "': 1 argument required, but only 0 present.");
    var l = collList(t), k = String(nm), a, i;
    for (i = 0; i < l.length; i++) { a = __mr(l[i], 'attrs') || {}; if (String(a.id || '') === k || String(a.name || '') === k) return l[i]; }
    return null;
  }
  function collItem(cls, len) { var L2 = (len >= 0) ? len : 1; return __native('item', L2, function (i) { guardColl(this); return collItemImpl(this, i, arguments.length, cls); }); }
  function collNamed(cls, len) { var L2 = (len >= 0) ? len : 1; return __native('namedItem', L2, function (nm) { guardColl(this); return collNamedImpl(this, nm, arguments.length, cls); }); }
  function formControls(f) {
    var out = [];
    (function walk(nd) {
      var k = nd && nd.childNodes; if (!k) return;
      for (var i = 0; i < k.length; i++) {
        var c = k[i]; if (!c || c.nodeType !== 1) continue;
        var tg = __mr(c, 'tag');
        if (tg === 'input' || tg === 'select' || tg === 'textarea' || tg === 'button' || tg === 'fieldset' || tg === 'object' || tg === 'output') out.push(c);
        walk(c);
      }
    })(f);
    return out;
  }
  function selectOptions(sel) {
    var out = [];
    (function walk(nd) {
      var k = nd && nd.childNodes; if (!k) return;
      for (var i = 0; i < k.length; i++) {
        var c = k[i]; if (!c || c.nodeType !== 1) continue;
        if (__mr(c, 'tag') === 'option') out.push(c);
        walk(c);
      }
    })(sel);
    return out;
  }
  function refreshIndices(c) {
    var l = collList(c);
    for (var i = 0; i < l.length && i < 128; i++) { try { Object.defineProperty(c, String(i), { value: l[i], writable: false, enumerable: true, configurable: true }); } catch (e) {} }
  }

  // ── measured behaviours (u2b3-surf pass 2/3) ─────────────────────────
  var SP = {};
  function boolG(name) { return __native('get ' + name, 0, function () { guardNode(this); var v = __g(this, '_h3_' + name); return v === undefined ? false : !!v; }); }
  function boolS(name) { return __native('set ' + name, 1, function (v) { guardNode(this); __hide(this, '_h3_' + name, !!v); }); }
  function attrSet(name, attr) { return __native('set ' + name, 1, function (v) { guardNode(this); __mr(this, 'attrs')[attr] = String(v); }); }
  function attrEnumG(name, attr, ok, dflt) {
    return __native('get ' + name, 0, function () {
      guardNode(this);
      var v = __mr(this, 'attrs')[attr];
      var s = (v === undefined || v === null || v === '') ? '' : String(v).toLowerCase();
      return (s && ok[s]) ? s : dflt;
    });
  }
  function urlGet(name, attr, pageFallback) {
    return __native('get ' + name, 0, function () {
      guardNode(this);
      var v = __mr(this, 'attrs')[attr];
      var s = (v === undefined || v === null) ? '' : String(v);
      if (s === '' && !pageFallback) return '';
      try { return resolveUrl(PAGE_URL, s); } catch (e) { return s; }
    });
  }
  function urlSet(name, attr) { return __native('set ' + name, 1, function (v) { guardNode(this); __mr(this, 'attrs')[attr] = String(v); }); }

  // iframe — booleans + enums + the measured object members
  (function () {
    var names = ['allowFullscreen', 'allowPaymentRequest', 'adAuctionHeaders', 'browsingTopics', 'credentialless'];
    for (var i = 0; i < names.length; i++) SP['HTMLIFrameElement|' + names[i]] = { g: boolG, s: boolS };
  })();
  var RP_OK = { '': 1, 'no-referrer': 1, 'no-referrer-when-downgrade': 1, 'origin': 1, 'origin-when-cross-origin': 1, 'same-origin': 1, 'strict-origin': 1, 'strict-origin-when-cross-origin': 1, 'unsafe-url': 1 };
  SP['HTMLIFrameElement|loading'] = { g: function (n) { return attrEnumG(n, 'loading', { eager: 1, lazy: 1, auto: 1 }, 'auto'); }, s: function (n) { return attrSet(n, 'loading'); } };
  SP['HTMLIFrameElement|referrerPolicy'] = { g: function (n) { return attrEnumG(n, 'referrerpolicy', RP_OK, ''); }, s: function (n) { return attrSet(n, 'referrerpolicy'); } };
  SP['HTMLIFrameElement|sandbox'] = {
    g: function (n) { return __native('get ' + n, 0, function () { guardNode(this); var tl = __g(this, '_h3_sandbox_tl'); if (!tl) { tl = mkTokenList(this, 'sandbox'); __hide(this, '_h3_sandbox_tl', tl); } return tl; }); },
    s: function (n) { return __native('set ' + n, 1, function (v) { guardNode(this); __mr(this, 'attrs').sandbox = String(v); }); }
  };
  SP['HTMLIFrameElement|featurePolicy'] = { g: function (n) { return __native('get ' + n, 0, function () { guardNode(this); return tagObj({ allowedFeatures: function () { return []; }, allowsFeature: function () { return false; }, features: function () { return []; }, getAllowlistForFeature: function () { return []; } }, 'PermissionsPolicy'); }); } };
  SP['HTMLIFrameElement|longDesc'] = { g: function (n) { return urlGet(n, 'longdesc', false); }, s: function (n) { return urlSet(n, 'longdesc'); } };

  // form — enums + the elements collection
  var ENC_OK = { 'application/x-www-form-urlencoded': 1, 'multipart/form-data': 1, 'text/plain': 1 };
  SP['HTMLFormElement|method'] = { g: function (n) { return attrEnumG(n, 'method', { get: 1, post: 1, dialog: 1 }, 'get'); }, s: function (n) { return attrSet(n, 'method'); } };
  SP['HTMLFormElement|enctype'] = { g: function (n) { return attrEnumG(n, 'enctype', ENC_OK, 'application/x-www-form-urlencoded'); }, s: function (n) { return attrSet(n, 'enctype'); } };
  SP['HTMLFormElement|encoding'] = { g: function (n) { return attrEnumG(n, 'enctype', ENC_OK, 'application/x-www-form-urlencoded'); }, s: function (n) { return attrSet(n, 'enctype'); } };
  SP['HTMLFormElement|action'] = { g: function (n) { return urlGet(n, 'action', true); }, s: function (n) { return urlSet(n, 'action'); } };
  SP['HTMLFormElement|elements'] = { g: function (n) { return __native('get ' + n, 0, function () { guardNode(this); var c = __g(this, '_h3_els'); if (!c) { var f = this; c = mkColl('HTMLFormControlsCollection', function () { return formControls(f); }); __hide(this, '_h3_els', c); } refreshIndices(c); return c; }); } };

  // input — enums, URL reflection, the measured methods
  var TYPE_OK = { text: 1, search: 1, tel: 1, url: 1, email: 1, password: 1, date: 1, month: 1, week: 1, time: 1, 'datetime-local': 1, number: 1, range: 1, color: 1, checkbox: 1, radio: 1, file: 1, submit: 1, image: 1, reset: 1, button: 1, hidden: 1 };
  var STEP_TYPES = { number: 1, range: 1, date: 1, month: 1, week: 1, time: 1, 'datetime-local': 1 };
  function inputTypeOf(el) { var v = __mr(el, 'attrs').type; var s = (v === undefined || v === null || v === '') ? 'text' : String(v).toLowerCase(); return TYPE_OK[s] ? s : 'text'; }
  SP['HTMLInputElement|type'] = { g: function (n) { return attrEnumG(n, 'type', TYPE_OK, 'text'); }, s: function (n) { return attrSet(n, 'type'); } };
  SP['HTMLInputElement|formAction'] = { g: function (n) { return urlGet(n, 'formaction', true); }, s: function (n) { return urlSet(n, 'formaction'); } };
  SP['HTMLInputElement|checkValidity'] = { f: function () { return __native('checkValidity', 0, function () { guardNode(this); return true; }); } };
  SP['HTMLInputElement|select'] = { f: function () { return __native('select', 0, function () { guardNode(this); var v = __g(this, '_sf_value'); __hide(this, '_h3_selectionStart', 0); __hide(this, '_h3_selectionEnd', String(v === undefined ? '' : v).length); }); } };
  SP['HTMLInputElement|setSelectionRange'] = { f: function () { return __native('setSelectionRange', 2, function (a, b) { guardNode(this); __hide(this, '_h3_selectionStart', coerceN(a)); __hide(this, '_h3_selectionEnd', coerceN(b)); }); } };
  SP['HTMLInputElement|setCustomValidity'] = { f: function () { return __native('setCustomValidity', 1, function (m) { guardNode(this); __hide(this, '_h3_validationMessage', String(m)); }); } };
  function stepper(nm) { return { f: function () { return __native(nm, 0, function () { guardNode(this); if (!STEP_TYPES[inputTypeOf(this)]) throw domEx("Failed to execute '" + nm + "' on 'HTMLInputElement': This form element is not steppable.", 'InvalidStateError'); }); } }; }
  SP['HTMLInputElement|stepUp'] = stepper('stepUp');
  SP['HTMLInputElement|stepDown'] = stepper('stepDown');
  SP['HTMLButtonElement|formAction'] = { g: function (n) { return urlGet(n, 'formaction', true); }, s: function (n) { return urlSet(n, 'formaction'); } };
  SP['HTMLBaseElement|href'] = { g: function (n) { return urlGet(n, 'href', true); }, s: function (n) { return urlSet(n, 'href'); } };

  // select — options collection + the measured value/length semantics
  SP['HTMLSelectElement|options'] = { g: function (n) { return __native('get ' + n, 0, function () { guardNode(this); var c = __g(this, '_h3_opts'); if (!c) { var s0 = this; c = mkColl('HTMLOptionsCollection', function () { return selectOptions(s0); }); __hide(this, '_h3_opts', c); __hide(c, '_h3sel', s0); } refreshIndices(c); return c; }); } };
  SP['HTMLSelectElement|length'] = {
    g: function () { return __native('get length', 0, function () { guardNode(this); return selectOptions(this).length; }); },
    s: function () { return __native('set length', 1, function () { guardNode(this); }); }
  };
  SP['HTMLSelectElement|value'] = {
    g: function () { return __native('get value', 0, function () { guardNode(this); var v = __g(this, '_sf_value'); return v === undefined ? '' : String(v); }); },
    s: function () { return __native('set value', 1, function (v) { guardNode(this); var s = String(v), l = selectOptions(this), hit = false, i; for (i = 0; i < l.length; i++) { var a = __mr(l[i], 'attrs') || {}; var ov = a.value === undefined ? '' : String(a.value); if (ov === s) { hit = true; break; } } __hide(this, '_sf_value', hit ? s : ''); }); }
  };
  SP['HTMLSelectElement|item'] = { f: function () { return __native('item', 1, function (i) { guardNode(this); var l = selectOptions(this), n = Number(i) >>> 0; return n < l.length ? l[n] : null; }); } };
  SP['HTMLSelectElement|namedItem'] = { f: function () { return __native('namedItem', 1, function (nm) { guardNode(this); var k = String(nm), l = selectOptions(this), a, i; for (i = 0; i < l.length; i++) { a = __mr(l[i], 'attrs') || {}; if (String(a.id || a.name || '') === k) return l[i]; } return null; }); } };

  // media — canPlayType (moved + renamed), the measured promise methods
  var oldCanPlay = null;
  SP['HTMLMediaElement|canPlayType'] = { f: function (n, len) { var old = oldCanPlay; return __native('canPlayType', 1, function (t) { guardNode(this); if (!old) return ''; return old.call(this, t); }); } };
  SP['HTMLMediaElement|play'] = { f: function () { return __native('play', 0, function () { guardNode(this); return rejectP("play() failed because the user didn't interact with the document first. https://goo.gl/xX8pDD", 'NotAllowedError'); }); } };
  SP['HTMLMediaElement|setSinkId'] = { f: function () { return __native('setSinkId', 1, function () { guardNode(this); return Promise.resolve(); }); } };
  SP['HTMLMediaElement|setMediaKeys'] = { f: function () { return __native('setMediaKeys', 1, function () { guardNode(this); return Promise.resolve(); }); } };
  SP['HTMLMediaElement|captureStream'] = { f: function () { return __native('captureStream', 0, function () { guardNode(this); return streamStub(); }); } };
  SP['HTMLMediaElement|addTextTrack'] = { f: function () { return __native('addTextTrack', 1, function () { guardNode(this); return decObj('TextTrack'); }); } };
  var __rvfcN = 0;
  SP['HTMLVideoElement|requestVideoFrameCallback'] = { f: function () { return __native('requestVideoFrameCallback', 1, function () { guardNode(this); return ++__rvfcN; }); } };
  SP['HTMLVideoElement|getVideoPlaybackQuality'] = { f: function () { return __native('getVideoPlaybackQuality', 0, function () { guardNode(this); return tagObj({ creationTime: 0, totalVideoFrames: 0, droppedVideoFrames: 0 }, 'VideoPlaybackQuality'); }); } };
  SP['HTMLImageElement|decode'] = { f: function () { return __native('decode', 0, function () { guardNode(this); return rejectP('The source image cannot be decoded.', 'EncodingError'); }); } };

  // collections
  SP['HTMLAllCollection|length'] = { g: function () { return __native('get length', 0, function () { guardColl(this); return collList(this).length; }); } };
  SP['HTMLAllCollection|item'] = { f: function (n, len) { return collItem('HTMLAllCollection', len); } };
  SP['HTMLAllCollection|namedItem'] = { f: function (n, len) { return collNamed('HTMLAllCollection', len); } };
  SP['HTMLFormControlsCollection|namedItem'] = { f: function (n, len) { return collNamed('HTMLFormControlsCollection', len); } };
  SP['HTMLOptionsCollection|length'] = { g: function () { return __native('get length', 0, function () { guardColl(this); return collList(this).length; }); }, s: function () { return __native('set length', 1, function () { guardColl(this); }); } };

  // DOMTokenList (pulled in by the sandbox member)
  SP['DOMTokenList|length'] = { g: function () { return __native('get length', 0, function () { guardColl(this); return tlTokens(this).length; }); } };
  SP['DOMTokenList|value'] = {
    g: function () { return __native('get value', 0, function () { guardColl(this); return tlTokens(this).join(' '); }); },
    s: function () { return __native('set value', 1, function (v) { guardColl(this); tlSave(this, String(v).split(/\s+/).filter(Boolean)); }); }
  };
  SP['DOMTokenList|add'] = { f: function () { return __native('add', 0, function () { guardColl(this); var a = tlTokens(this); for (var i = 0; i < arguments.length; i++) { var t = String(arguments[i]); if (a.indexOf(t) < 0) a.push(t); } tlSave(this, a); }); } };
  SP['DOMTokenList|remove'] = { f: function () { return __native('remove', 0, function () { guardColl(this); var a = tlTokens(this); for (var i = 0; i < arguments.length; i++) { var x = a.indexOf(String(arguments[i])); if (x >= 0) a.splice(x, 1); } tlSave(this, a); }); } };
  SP['DOMTokenList|contains'] = { f: function () { return __native('contains', 1, function (t) { guardColl(this); return tlTokens(this).indexOf(String(t)) >= 0; }); } };
  SP['DOMTokenList|toggle'] = { f: function () { return __native('toggle', 1, function (t, force) { guardColl(this); var a = tlTokens(this), x = String(t), i = a.indexOf(x); if (i >= 0) { if (force === undefined || force === false) { a.splice(i, 1); tlSave(this, a); return false; } return true; } if (force === true || force === undefined) { a.push(x); tlSave(this, a); return true; } return false; }); } };
  SP['DOMTokenList|item'] = { f: function () { return __native('item', 1, function (i) { guardColl(this); var a = tlTokens(this), n = Number(i) >>> 0; return n < a.length ? a[n] : null; }); } };
  SP['DOMTokenList|replace'] = { f: function () { return __native('replace', 2, function (o, n2) { guardColl(this); var a = tlTokens(this), x = String(o), i = a.indexOf(x); if (i < 0) return false; a[i] = String(n2); tlSave(this, a); return true; }); } };
  SP['DOMTokenList|supports'] = { f: function () { return __native('supports', 1, function () { guardColl(this); return true; }); } };
  SP['DOMTokenList|forEach'] = { f: function () { return __native('forEach', 1, function (cb, thisArg) { guardColl(this); var a = tlTokens(this); for (var i = 0; i < a.length; i++) cb.call(thisArg, a[i], i, this); }); } };
  function tlIterOf(mode) {
    return __native(mode, 0, function () {
      guardColl(this);
      var a = tlTokens(this), i = 0;
      return { next: function () { if (i >= a.length) return { value: undefined, done: true }; var v = a[i], ix = i; i++; return { value: mode === 'keys' ? ix : (mode === 'entries' ? [ix, v] : v), done: false }; } };
    });
  }
  SP['DOMTokenList|entries'] = { f: function () { return tlIterOf('entries'); } };
  SP['DOMTokenList|keys'] = { f: function () { return tlIterOf('keys'); } };
  SP['DOMTokenList|values'] = { f: function () { return tlIterOf('values'); } };
  SP['DOMTokenList|toString'] = { f: function () { return __native('toString', 0, function () { guardColl(this); return tlTokens(this).join(' '); }); } };

  // ── iteration-3 measured tails (u2b3-surf post2 pairs, same-hour Chrome) ──
  function onVal(v) { if (typeof v === 'function') return v; if (v !== null && typeof v === 'object' && typeof v.handleEvent === 'function') return v; return null; }
  function onGet(name) { return __native('get ' + name, 0, function () { guardNode(this); var v = __g(this, '_h3_' + name); return v === undefined ? null : v; }); }
  function onSet(name) { return __native('set ' + name, 1, function (v) { guardNode(this); __hide(this, '_h3_' + name, onVal(v)); }); }
  function enumSP(ok, unset, inval) {
    if (inval === undefined) inval = unset;
    return {
      g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); var v = __g(this, '_h3_' + name); if (v === undefined) return unset; v = String(v); return ok[v] ? v : inval; }); },
      s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); var s = String(v).toLowerCase(); __hide(this, '_h3_' + name, ok[s] ? s : inval); }); }
    };
  }
  function tokenListSP(attr) {
    return {
      g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); var key = '_h3_tl_' + name; var tl = __g(this, key); if (!tl) { tl = mkTokenList(this, attr); __hide(this, key, tl); } return tl; }); },
      s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); var tl = mkTokenList(this, attr); tlSave(tl, String(v).split(/\s+/).filter(Boolean)); }); }
    };
  }
  function elemSetSP() {
    return { s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); if (v === null || v === undefined) { __hide(this, '_h3_' + name, null); return; } if (!(typeof v === 'object' && typeof v.nodeType === 'number')) throw new TypeError("Failed to set the '" + name + "' property: The provided value is not of type 'Element'."); __hide(this, '_h3_' + name, v); }); } };
  }
  function absURL(el) { var h = __mr(el, 'attrs').href; if (h === undefined || h === null || h === '') return null; try { return resolveUrl(PAGE_URL, String(h)); } catch (e) { return String(h); } }
  function uParse(s) { var m = /^([a-zA-Z][a-zA-Z0-9+.-]*):\/\/(?:([^\/?#@]*)@)?([^\/?#:]*)(?::([0-9]*))?([^?#]*)(?:\?([^#]*))?(?:#(.*))?$/.exec(s); if (!m) return null; var auth = m[2] || '', us = '', pw = ''; if (auth) { var k = auth.indexOf(':'); if (k >= 0) { us = auth.slice(0, k); pw = auth.slice(k + 1); } else us = auth; } return { sc: m[1], us: us, pw: pw, ho: m[3], po: m[4] || '', pa: m[5] || '', qu: m[6], fr: m[7] }; }
  function uSer(u) { var s = u.sc + '://'; if (u.us || u.pw) s += u.us + (u.pw ? ':' + u.pw : '') + '@'; s += u.ho; if (u.po) s += ':' + u.po; s += u.pa || '/'; if (u.qu !== undefined) s += '?' + u.qu; if (u.fr !== undefined) s += '#' + u.fr; return s; }
  function urlPartG(name) {
    return __native('get ' + name, 0, function () {
      guardNode(this);
      var abs = absURL(this);
      var u = abs ? uParse(abs) : null;
      if (!u) return name === 'protocol' ? ':' : '';
      if (name === 'protocol') return u.sc + ':';
      if (name === 'username') return u.us;
      if (name === 'password') return u.pw;
      if (name === 'host') return u.ho + (u.po ? ':' + u.po : '');
      if (name === 'hostname') return u.ho;
      if (name === 'port') return u.po;
      if (name === 'pathname') return u.pa || '/';
      if (name === 'search') return u.qu === undefined ? '' : '?' + u.qu;
      return u.fr === undefined ? '' : '#' + u.fr;
    });
  }
  function urlPartS(name) {
    return __native('set ' + name, 1, function (v) {
      guardNode(this);
      var abs = absURL(this);
      if (!abs) return;
      var u = uParse(abs);
      if (!u) return;
      v = String(v).trim();
      if (name === 'protocol') { var sc = v.replace(/:$/, ''); if (!/^[a-zA-Z][a-zA-Z0-9+.-]*$/.test(sc)) return; u.sc = sc; }
      else if (name === 'username') u.us = v;
      else if (name === 'password') u.pw = v;
      else if (name === 'host') { var hp = v.split(':'); u.ho = hp[0]; u.po = hp[1] || ''; }
      else if (name === 'hostname') u.ho = v;
      else if (name === 'port') { if (!/^[0-9]*$/.test(v)) return; u.po = v; }
      else if (name === 'pathname') u.pa = v.charAt(0) === '/' ? v : '/' + v;
      else if (name === 'search') u.qu = v.replace(/^\?/, '');
      else u.fr = v.replace(/^#/, '');
      __mr(this, 'attrs').href = uSer(u);
    });
  }
  function optSel(o) { return __g(o, '_h3_dirtySel') ? !!__g(o, '_sf_selected') : !!(__g(o, '_sf_selected') || __g(o, '_sf_defselected')); }
  function optSetSel(o, v) { __hide(o, '_sf_selected', !!v); __hide(o, '_h3_dirtySel', true); }
  function selSetLen(sel, v) {
    var n = Number(v) >>> 0, l = selectOptions(sel);
    while (l.length > n) { var rm = l.pop(); try { if (rm.parentNode) rm.parentNode.removeChild(rm); } catch (e) {} }
    while (l.length < n) { try { var o = W.document.createElement('option'); sel.appendChild(o); l.push(o); } catch (e) { break; } }
  }
  function meterG(name) { return __native('get ' + name, 0, function () { guardNode(this); var v = __g(this, '_h3_' + name); if (v === undefined) v = 0; var a = __mr(this, 'attrs'); var mn = a.min === undefined ? 0 : Number(a.min); var mx = a.max === undefined ? 1 : Number(a.max); if (!(mn === mn)) mn = 0; if (!(mx === mx)) mx = 1; return v < mn ? mn : (v > mx ? mx : v); }); }
  function meterS(name) { return __native('set ' + name, 1, function (v) { guardNode(this); var x = Number(v); if (!(x === x)) x = 0; __hide(this, '_h3_' + name, x); }); }
  SP['HTMLImageElement|crossOrigin'] = enumSP({ anonymous: 1, 'use-credentials': 1 }, null, 'anonymous');
  SP['HTMLLinkElement|crossOrigin'] = SP['HTMLImageElement|crossOrigin'];
  SP['HTMLScriptElement|crossOrigin'] = SP['HTMLImageElement|crossOrigin'];
  SP['HTMLMediaElement|crossOrigin'] = SP['HTMLImageElement|crossOrigin'];
  var RP2 = { g: function (n) { return attrEnumG(n, 'referrerpolicy', RP_OK, ''); }, s: function (n) { return attrSet(n, 'referrerpolicy'); } };
  SP['HTMLAnchorElement|referrerPolicy'] = RP2;
  SP['HTMLAreaElement|referrerPolicy'] = RP2;
  SP['HTMLLinkElement|referrerPolicy'] = RP2;
  SP['HTMLScriptElement|referrerPolicy'] = RP2;
  SP['HTMLImageElement|referrerPolicy'] = RP2;
  SP['HTMLImageElement|decoding'] = enumSP({ sync: 1, async: 1, auto: 1 }, 'auto');
  SP['HTMLImageElement|fetchPriority'] = enumSP({ high: 1, low: 1, auto: 1 }, 'auto');
  SP['HTMLLinkElement|fetchPriority'] = SP['HTMLImageElement|fetchPriority'];
  SP['HTMLScriptElement|fetchPriority'] = SP['HTMLImageElement|fetchPriority'];
  SP['HTMLImageElement|loading'] = enumSP({ eager: 1, lazy: 1, auto: 1 }, 'auto');
  SP['HTMLMediaElement|loading'] = enumSP({ eager: 1, lazy: 1 }, 'eager');
  SP['HTMLMediaElement|preload'] = enumSP({ none: 1, metadata: 1, auto: 1 }, 'metadata');
  SP['HTMLFormElement|autocomplete'] = enumSP({ on: 1, off: 1 }, 'on');
  SP['HTMLInputElement|autocomplete'] = enumSP({ on: 1, off: 1 }, '');
  SP['HTMLSelectElement|autocomplete'] = SP['HTMLInputElement|autocomplete'];
  SP['HTMLTextAreaElement|autocomplete'] = SP['HTMLInputElement|autocomplete'];
  var FE2 = enumSP({ 'application/x-www-form-urlencoded': 1, 'multipart/form-data': 1, 'text/plain': 1 }, '', 'application/x-www-form-urlencoded');
  SP['HTMLButtonElement|formEnctype'] = FE2;
  SP['HTMLInputElement|formEnctype'] = FE2;
  var FM2 = enumSP({ get: 1, post: 1, dialog: 1 }, '', 'get');
  SP['HTMLButtonElement|formMethod'] = FM2;
  SP['HTMLInputElement|formMethod'] = FM2;
  SP['HTMLButtonElement|type'] = enumSP({ submit: 1, reset: 1, button: 1 }, 'submit');
  SP['HTMLButtonElement|popoverTargetAction'] = enumSP({ toggle: 1, show: 1, hide: 1 }, 'toggle');
  SP['HTMLInputElement|popoverTargetAction'] = SP['HTMLButtonElement|popoverTargetAction'];
  SP['HTMLButtonElement|command'] = enumSP({ toggle: 1, show: 1, close: 1, hide: 1, 'request-close': 1, 'show-modal': 1 }, '');
  SP['HTMLDialogElement|closedBy'] = enumSP({ any: 1, closerequest: 1, none: 1 }, 'none');
  SP['HTMLLinkElement|as'] = enumSP({ script: 1, style: 1, fetch: 1, image: 1, font: 1, audio: 1, video: 1, track: 1 }, '');
  SP['HTMLTableCellElement|scope'] = enumSP({ row: 1, col: 1, rowgroup: 1, colgroup: 1 }, '');
  SP['HTMLTrackElement|kind'] = enumSP({ subtitles: 1, captions: 1, descriptions: 1, chapters: 1, metadata: 1 }, 'subtitles', 'metadata');
  SP['HTMLTemplateElement|shadowRootMode'] = enumSP({ open: 1, closed: 1 }, '');
  SP['HTMLTemplateElement|shadowRootSlotAssignment'] = enumSP({ named: 1, manual: 1 }, 'named');
  SP['HTMLAnchorElement|relList'] = tokenListSP('rel');
  SP['HTMLAreaElement|relList'] = tokenListSP('rel');
  SP['HTMLFormElement|relList'] = tokenListSP('rel');
  SP['HTMLLinkElement|relList'] = tokenListSP('rel');
  SP['HTMLLinkElement|sizes'] = tokenListSP('sizes');
  SP['HTMLLinkElement|blocking'] = tokenListSP('blocking');
  SP['HTMLStyleElement|blocking'] = tokenListSP('blocking');
  SP['HTMLScriptElement|blocking'] = tokenListSP('blocking');
  SP['HTMLMediaElement|controlsList'] = tokenListSP('controlslist');
  SP['HTMLOutputElement|htmlFor'] = tokenListSP('for');
  SP['HTMLFencedFrameElement|sandbox'] = tokenListSP('sandbox');
  var EON2 = elemSetSP();
  SP['HTMLButtonElement|popoverTargetElement'] = { s: EON2.s };
  SP['HTMLInputElement|popoverTargetElement'] = { s: EON2.s };
  SP['HTMLButtonElement|commandForElement'] = { s: EON2.s };
  SP['HTMLAnchorElement|interestForElement'] = { s: EON2.s };
  SP['HTMLAreaElement|interestForElement'] = { s: EON2.s };
  SP['HTMLButtonElement|interestForElement'] = { s: EON2.s };
  SP['HTMLTableElement|caption'] = { s: EON2.s };
  SP['HTMLTableElement|tHead'] = { s: EON2.s };
  SP['HTMLTableElement|tFoot'] = { s: EON2.s };
  SP['HTMLFencedFrameElement|config'] = { s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); if (v !== null && typeof v !== 'object') throw new TypeError("Failed to set the 'config' property: The provided value is not of type 'FencedFrameConfig'."); __hide(this, '_h3_' + name, v); }); } };
  SP['HTMLInputElement|files'] = { s: function (name) { return __native('set ' + name, 1, function () { guardNode(this); throw new TypeError("Failed to set the 'files' property on 'HTMLInputElement': The provided value is not of type 'FileList'."); }); } };
  SP['HTMLInputElement|valueAsDate'] = { s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); if (v !== null && !(v instanceof W.Date)) throw new TypeError("Failed to set the 'valueAsDate' property on 'HTMLInputElement': The provided value is not of type 'Date'."); __hide(this, '_h3_' + name, v); }); } };
  var NUMBERY = { number: 1, range: 1, date: 1, month: 1, week: 1, time: 1, 'datetime-local': 1 };
  SP['HTMLInputElement|valueAsNumber'] = { s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); if (!NUMBERY[inputTypeOf(this)]) throw domEx("Failed to set the 'valueAsNumber' property on 'HTMLInputElement': This input element does not support Number values.", 'InvalidStateError'); __hide(this, '_h3_' + name, coerceN(v)); }); } };
  SP['HTMLMediaElement|volume'] = { s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); var x = Number(v); if (!(x >= 0 && x <= 1)) throw domEx("Failed to set the 'volume' property on 'HTMLMediaElement': The volume provided (" + String(v) + ") is outside the range [0, 1].", 'IndexSizeError'); __hide(this, '_h3_' + name, x); }); } };
  SP['HTMLStyleElement|disabled'] = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); return false; }); }, s: function (name) { return __native('set ' + name, 1, function () { guardNode(this); }); } };
  SP['HTMLInputElement|checked'] = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); if (__g(this, '_h3_dirtyCk')) return !!__g(this, '_sf_checked'); return !!(__g(this, '_sf_checked') || __g(this, '_sf_defchecked')); }); }, s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); __hide(this, '_sf_checked', !!v); __hide(this, '_h3_dirtyCk', true); }); } };
  SP['HTMLInputElement|defaultChecked'] = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); return !!__g(this, '_sf_defchecked'); }); }, s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); __hide(this, '_sf_defchecked', !!v); }); } };
  SP['HTMLOptionElement|selected'] = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); return optSel(this); }); }, s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); optSetSel(this, v); }); } };
  SP['HTMLOptionElement|defaultSelected'] = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); return !!__g(this, '_sf_defselected'); }); }, s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); __hide(this, '_sf_defselected', !!v); }); } };
  SP['HTMLMeterElement|value'] = { g: function (n) { return meterG(n); }, s: function (n) { return meterS(n); } };
  SP['HTMLProgressElement|value'] = { g: function (n) { return meterG(n); }, s: function (n) { return meterS(n); } };
  SP['HTMLInputElement|height'] = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); if (inputTypeOf(this) !== 'image') return 0; var v = __g(this, '_h3_' + name); return v === undefined ? 0 : v; }); }, s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); __hide(this, '_h3_' + name, Number(v) || 0); }); } };
  SP['HTMLInputElement|width'] = SP['HTMLInputElement|height'];
  function selValLen(el) { var v = __g(el, '_sf_value'); return String(v === undefined ? '' : v).length; }
  var ISP = { g: function (name) { return __native('get ' + name, 0, function () { guardNode(this); var v = __g(this, '_h3_' + name); if (v === undefined) v = 0; var l = selValLen(this); return v > l ? l : v; }); }, s: function (name) { return __native('set ' + name, 1, function (v) { guardNode(this); var x = coerceN(v), l = selValLen(this); if (x < 0) x = 0; if (x > l) x = l; __hide(this, '_h3_' + name, x); }); } };
  SP['HTMLInputElement|selectionStart'] = ISP;
  SP['HTMLInputElement|selectionEnd'] = ISP;
  SP['HTMLTextAreaElement|selectionStart'] = ISP;
  SP['HTMLTextAreaElement|selectionEnd'] = ISP;
  var SDIR = enumSP({ forward: 1, backward: 1, none: 1 }, 'forward');
  SP['HTMLInputElement|selectionDirection'] = SDIR;
  SP['HTMLTextAreaElement|selectionDirection'] = SDIR;
  SP['HTMLSelectElement|length'] = { g: function () { return __native('get length', 0, function () { guardNode(this); return selectOptions(this).length; }); }, s: function () { return __native('set length', 1, function (v) { guardNode(this); selSetLen(this, v); }); } };
  SP['HTMLOptionsCollection|length'] = { g: function () { return __native('get length', 0, function () { guardColl(this); return collList(this).length; }); }, s: function () { return __native('set length', 1, function (v) { guardColl(this); var s0 = __g(this, '_h3sel'); if (s0) selSetLen(s0, v); }); } };
  SP['HTMLSelectElement|selectedIndex'] = { g: function () { return __native('get selectedIndex', 0, function () { guardNode(this); var l = selectOptions(this); for (var i = 0; i < l.length; i++) { if (optSel(l[i])) return i; } return -1; }); }, s: function () { return __native('set selectedIndex', 1, function (v) { guardNode(this); var l = selectOptions(this), n = Number(v) >> 0; for (var i = 0; i < l.length; i++) optSetSel(l[i], i === n); }); } };
  SP['HTMLOptionsCollection|selectedIndex'] = { g: function () { return __native('get selectedIndex', 0, function () { guardColl(this); var l = collList(this); for (var i = 0; i < l.length; i++) { if (optSel(l[i])) return i; } return -1; }); }, s: function () { return __native('set selectedIndex', 1, function (v) { guardColl(this); var l = collList(this), n = Number(v) >> 0; for (var i = 0; i < l.length; i++) optSetSel(l[i], i === n); }); } };
  SP['HTMLFrameElement|longDesc'] = { g: function (n) { return urlGet(n, 'longdesc', false); }, s: function (n) { return urlSet(n, 'longdesc'); } };
  SP['HTMLImageElement|lowsrc'] = { g: function (n) { return urlGet(n, 'lowsrc', false); }, s: function (n) { return urlSet(n, 'lowsrc'); } };
  SP['HTMLImageElement|longDesc'] = SP['HTMLFrameElement|longDesc'];
  SP['HTMLModElement|cite'] = { g: function (n) { return urlGet(n, 'cite', false); }, s: function (n) { return urlSet(n, 'cite'); } };
  SP['HTMLQuoteElement|cite'] = SP['HTMLModElement|cite'];
  SP['HTMLObjectElement|codeBase'] = { g: function (n) { return urlGet(n, 'codebase', false); }, s: function (n) { return urlSet(n, 'codebase'); } };
  var URLP = ['protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'hash'];
  for (var uix = 0; uix < URLP.length; uix++) {
    SP['HTMLAnchorElement|' + URLP[uix]] = { g: function (n) { return urlPartG(n); }, s: function (n) { return urlPartS(n); } };
    SP['HTMLAreaElement|' + URLP[uix]] = SP['HTMLAnchorElement|' + URLP[uix]];
  }

  // ── A · Element/HTMLElement token fixes (the 11 measured) ────────────
  function fixAccE(P, nm) { try { var d = Object.getOwnPropertyDescriptor(P, nm); if (d && (d.get || d.set)) Object.defineProperty(P, nm, { get: d.get, set: d.set, enumerable: true, configurable: true }); } catch (e) {} }
  function fixLen(P, nm, L) { try { var d = Object.getOwnPropertyDescriptor(P, nm); if (d && typeof d.value === 'function') { var old = d.value; Object.defineProperty(P, nm, { value: __native(nm, L, function () { return old.apply(this, arguments); }), writable: true, enumerable: true, configurable: true }); } } catch (e) {} }
  if (EP) {
    var EA = ['className', 'innerHTML', 'children', 'firstElementChild', 'lastElementChild', 'childElementCount'];
    for (var ea = 0; ea < EA.length; ea++) fixAccE(EP, EA[ea]);
    fixLen(EP, 'animate', 1);
    fixLen(EP, 'setPointerCapture', 1);
  }
  if (HEP) {
    var HA = ['innerText', 'dataset', 'style'];
    for (var ha = 0; ha < HA.length; ha++) fixAccE(HEP, HA[ha]);
  }

  // ── B · the media chain ──────────────────────────────────────────────
  var MP = W.HTMLMediaElement && W.HTMLMediaElement.prototype;
  var VP = W.HTMLVideoElement && W.HTMLVideoElement.prototype;
  var AP = W.HTMLAudioElement && W.HTMLAudioElement.prototype;
  try { var dcp = VP && Object.getOwnPropertyDescriptor(VP, 'canPlayType'); if (dcp && typeof dcp.value === 'function') oldCanPlay = dcp.value; } catch (e) {}
  if (VP && MP) setP(VP, MP);
  if (AP && MP) setP(AP, MP);
  if (MP && HEP) setP(MP, HEP);

  // ── C · the table-driven rebuild ─────────────────────────────────────
  function effDesc(P, name, ex) {
    if (ex && (ex.get || ex.set)) return ex;
    var q = null; try { q = P ? Object.getPrototypeOf(P) : null; } catch (e) {}
    while (q) {
      var d = null; try { d = Object.getOwnPropertyDescriptor(q, name); } catch (e) {}
      if (d && (d.get || d.set)) return d;
      try { q = Object.getPrototypeOf(q); } catch (e) { q = null; }
    }
    return ex || null;
  }
  function defMember(cls, P, tok, ex) {
    var name = tok[0], kind = tok[1], fl = tok[2], gs = tok[3], extra = tok[4];
    if (name === 'constructor') {
      try { Object.defineProperty(P, 'constructor', { value: W[cls], writable: true, enumerable: false, configurable: true }); } catch (e) {}
      return;
    }
    var sp = SP[cls + '|' + name];
    try {
      if (kind === 'a') {
        var c = fl.charAt(1) === '1', e = fl.charAt(2) === '1';
        var g1 = gs.charAt(1) === '1', s1 = gs.charAt(3) === '1';
        var isOn = /^on[a-z]/.test(name);
        var ruse = isOn ? ((ex && (ex.get || ex.set)) ? ex : null) : effDesc(P, name, ex);
        var gFn = (sp && sp.g) ? sp.g(name) : (ruse && ruse.get) ? ruse.get : isOn ? onGet(name) : genGet(cls, name, extra);
        var sFn;
        if (s1) sFn = (sp && sp.s) ? sp.s(name) : (ruse && ruse.set) ? ruse.set : isOn ? onSet(name) : genSet(cls, name, extra);
        Object.defineProperty(P, name, { get: gFn, set: sFn, enumerable: e, configurable: c });
      } else if (kind === 'f') {
        var fc = fl.charAt(0) === '1', fe = fl.charAt(1) === '1', fw = fl.charAt(2) === '1';
        var len = parseInt(extra, 10); if (!(len >= 0)) len = 0;
        var fv = (sp && sp.f) ? sp.f(name, len) : (ex && typeof ex.value === 'function') ? ex.value : genFn(cls, name, len);
        Object.defineProperty(P, name, { value: fv, writable: fw, enumerable: fe, configurable: fc });
      } else {
        var dc = fl.charAt(0) === '1', de = fl.charAt(1) === '1', dw = fl.charAt(2) === '1';
        Object.defineProperty(P, name, { value: dec(extra), writable: dw, enumerable: de, configurable: dc });
      }
    } catch (e) {}
  }
  var seen = [];
  for (var cls in T) {
    if (cls === 'HTMLElement') continue;
    var C = W[cls];
    if (!C || !C.prototype) continue;
    var P = C.prototype;
    if (seen.indexOf(P) >= 0) continue;
    seen.push(P);
    var mem = T[cls].m, ex = {}, ns = [];
    try { ns = Object.getOwnPropertyNames(P); } catch (e) {}
    var i;
    for (i = 0; i < ns.length; i++) { try { ex[ns[i]] = Object.getOwnPropertyDescriptor(P, ns[i]); } catch (e) {} }
    for (i = 0; i < ns.length; i++) { try { delete P[ns[i]]; } catch (e) {} }
    for (i = 0; i < mem.length; i++) { defMember(cls, P, mem[i], ex[mem[i][0]]); }
    try { Object.defineProperty(P, Symbol.toStringTag, { value: cls, configurable: true }); } catch (e) {}
  }

  // ── D · HTMLCollection union impls (proxied views + mkColl objects) ──
  if (HC && HC.prototype) {
    try {
      var hcL = Object.getOwnPropertyDescriptor(HC.prototype, 'length');
      if (hcL && hcL.get) Object.defineProperty(HC.prototype, 'length', { get: __native('get length', 0, function () { if (__g(this, '_h3list') !== undefined) return collList(this).length; return hcL.get.call(this); }), enumerable: true, configurable: true });
      var hcI = Object.getOwnPropertyDescriptor(HC.prototype, 'item');
      if (hcI && typeof hcI.value === 'function') { var oldI = hcI.value; Object.defineProperty(HC.prototype, 'item', { value: __native('item', 1, function (i) { if (__g(this, '_h3list') !== undefined) return collItemImpl(this, i, arguments.length, 'HTMLCollection'); return oldI.apply(this, arguments); }), writable: true, enumerable: true, configurable: true }); }
      var hcN = Object.getOwnPropertyDescriptor(HC.prototype, 'namedItem');
      if (hcN && typeof hcN.value === 'function') { var oldN = hcN.value; Object.defineProperty(HC.prototype, 'namedItem', { value: __native('namedItem', 1, function (nm) { if (__g(this, '_h3list') !== undefined) return collNamedImpl(this, nm, arguments.length, 'HTMLCollection'); return oldN.apply(this, arguments); }), writable: true, enumerable: true, configurable: true }); }
    } catch (e) {}
  }

  // ── E · canvas.captureStream (measured: engine returned undefined) ───
  var CP = W.HTMLCanvasElement && W.HTMLCanvasElement.prototype;
  if (CP) { try { var cs = Object.getOwnPropertyDescriptor(CP, 'captureStream'); if (cs && typeof cs.value === 'function') Object.defineProperty(CP, 'captureStream', { value: __native('captureStream', 0, function () { guardNode(this); return streamStub(); }), writable: true, enumerable: true, configurable: true }); } catch (e) {} }

  // ── F · createElement tag → class wiring (T rows, same-hour Chrome) ──
  var TAGMAP = { a: 'HTMLAnchorElement', abbr: 'HTMLElement', address: 'HTMLElement', area: 'HTMLAreaElement', article: 'HTMLElement', aside: 'HTMLElement', audio: 'HTMLAudioElement', b: 'HTMLElement', base: 'HTMLBaseElement', bdi: 'HTMLElement', bdo: 'HTMLElement', big: 'HTMLElement', blockquote: 'HTMLQuoteElement', body: 'HTMLBodyElement', br: 'HTMLBRElement', button: 'HTMLButtonElement', canvas: 'HTMLCanvasElement', caption: 'HTMLTableCaptionElement', center: 'HTMLElement', cite: 'HTMLElement', code: 'HTMLElement', col: 'HTMLTableColElement', colgroup: 'HTMLTableColElement', data: 'HTMLDataElement', datalist: 'HTMLDataListElement', dd: 'HTMLElement', del: 'HTMLModElement', details: 'HTMLDetailsElement', dfn: 'HTMLElement', dialog: 'HTMLDialogElement', dir: 'HTMLDirectoryElement', div: 'HTMLDivElement', dl: 'HTMLDListElement', dt: 'HTMLElement', em: 'HTMLElement', embed: 'HTMLEmbedElement', fieldset: 'HTMLFieldSetElement', figcaption: 'HTMLElement', figure: 'HTMLElement', font: 'HTMLFontElement', footer: 'HTMLElement', form: 'HTMLFormElement', frame: 'HTMLFrameElement', frameset: 'HTMLFrameSetElement', h1: 'HTMLHeadingElement', h2: 'HTMLHeadingElement', h3: 'HTMLHeadingElement', h4: 'HTMLHeadingElement', h5: 'HTMLHeadingElement', h6: 'HTMLHeadingElement', head: 'HTMLHeadElement', header: 'HTMLElement', hgroup: 'HTMLElement', hr: 'HTMLHRElement', html: 'HTMLHtmlElement', i: 'HTMLElement', iframe: 'HTMLIFrameElement', img: 'HTMLImageElement', input: 'HTMLInputElement', ins: 'HTMLModElement', kbd: 'HTMLElement', label: 'HTMLLabelElement', legend: 'HTMLLegendElement', li: 'HTMLLIElement', link: 'HTMLLinkElement', main: 'HTMLElement', map: 'HTMLMapElement', mark: 'HTMLElement', marquee: 'HTMLMarqueeElement', menu: 'HTMLMenuElement', meta: 'HTMLMetaElement', meter: 'HTMLMeterElement', nav: 'HTMLElement', noscript: 'HTMLElement', object: 'HTMLObjectElement', ol: 'HTMLOListElement', optgroup: 'HTMLOptGroupElement', option: 'HTMLOptionElement', output: 'HTMLOutputElement', p: 'HTMLParagraphElement', param: 'HTMLParamElement', picture: 'HTMLPictureElement', plaintext: 'HTMLElement', pre: 'HTMLPreElement', progress: 'HTMLProgressElement', q: 'HTMLQuoteElement', rb: 'HTMLElement', rp: 'HTMLElement', rt: 'HTMLElement', rtc: 'HTMLElement', ruby: 'HTMLElement', s: 'HTMLElement', samp: 'HTMLElement', script: 'HTMLScriptElement', search: 'HTMLElement', section: 'HTMLElement', select: 'HTMLSelectElement', slot: 'HTMLSlotElement', small: 'HTMLElement', source: 'HTMLSourceElement', span: 'HTMLSpanElement', strike: 'HTMLElement', strong: 'HTMLElement', style: 'HTMLStyleElement', sub: 'HTMLElement', summary: 'HTMLElement', sup: 'HTMLElement', table: 'HTMLTableElement', tbody: 'HTMLTableSectionElement', td: 'HTMLTableCellElement', template: 'HTMLTemplateElement', textarea: 'HTMLTextAreaElement', tfoot: 'HTMLTableSectionElement', th: 'HTMLTableCellElement', thead: 'HTMLTableSectionElement', time: 'HTMLTimeElement', title: 'HTMLTitleElement', tr: 'HTMLTableRowElement', track: 'HTMLTrackElement', tt: 'HTMLElement', u: 'HTMLElement', ul: 'HTMLUListElement', var: 'HTMLElement', video: 'HTMLVideoElement', wbr: 'HTMLElement', xmp: 'HTMLPreElement', fencedframe: 'HTMLFencedFrameElement', selectedcontent: 'HTMLSelectedContentElement', geolocation: 'HTMLGeolocationElement', camera: 'HTMLCameraElement', microphone: 'HTMLMicrophoneElement', usermedia: 'HTMLUserMediaElement', portal: 'HTMLUnknownElement', 'x-foobar': 'HTMLElement', zzcustom: 'HTMLUnknownElement' };
  for (var tkey in TAGMAP) { var TC = W[TAGMAP[tkey]]; if (TC && TC.prototype) TAG_CTORS[tkey] = TC; }
  var _origTagCtor = tagCtor;
  tagCtor = function (tag) {
    var cn = TAGMAP[tag];
    if (cn === undefined && tag && tag.charAt(0) !== '#' && tag.indexOf('-') < 0) cn = 'HTMLUnknownElement';
    if (cn) { var C2 = W[cn]; if (C2 && C2.prototype) { TAG_CTORS[tag] = C2; return C2; } }
    return _origTagCtor(tag);
  };
})();
// ST-11 U2b-3 END

// ═══ ST-11 U2b-5 (2026-10-22): the SVG league — the full 97-class family
// built from assets/svg_tables.js (deepdiff W|SVG* self/p1 rows + chains +
// toStringTags; u2b5-surf 1-4 for defaults/behaviours). Policy identical to
// U2b-3: measured order/flags/kinds/fn-lengths; existing impls reused in
// place where they exist; new members get store-backed accessors with
// measured defaults and measured behaviours. No per-site logic anywhere.
var SVGNS = null, SVGADJ = null, SVGATTR = null, __mkNS = null;
(function () {
  var T = __DCI.SVGT;
  if (!T) return;
  var W = window, g = globalThis;
  var ELP = (W.Element && W.Element.prototype) || null;

  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }
  function inv() { return new TypeError('Illegal invocation'); }
  function tagObj(o, t) { try { Object.defineProperty(o, Symbol.toStringTag, { value: t, configurable: true }); } catch (e) {} return o; }
  function domEx(msg, name) { try { return new W.DOMException(msg, name); } catch (e) { var er = new Error(msg); try { er.name = name; } catch (e2) {} return er; } }
  function coerceN(v) { var x = Number(v); return (x === x) ? x : 0; }
  function dec(code) {
    if (!code || code === '?' || code === 'fn') return null;
    if (code === 'null') return null;
    if (code === 'u') return undefined;
    var c = code.charAt(0);
    if (c === 'n') return Number(code.slice(2));
    if (c === 's') { var s = code.slice(2); if (s.charAt(s.length - 1) === '~') return ''; return s; }
    if (c === 'b') return code === 'b:1';
    return null;
  }
  function coerce(code, v) { if (code === 'null') return v; var c = (code || '').charAt(0); if (c === 'n') return coerceN(v); if (c === 's') return String(v); if (c === 'b') return !!v; return v; }

  // ── receivers: element classes guard nodeType 1, value classes the mint brand ──
  var VALUE = { SVGAnimatedAngle: 1, SVGAnimatedBoolean: 1, SVGAnimatedEnumeration: 1, SVGAnimatedInteger: 1, SVGAnimatedLength: 1, SVGAnimatedLengthList: 1, SVGAnimatedNumber: 1, SVGAnimatedNumberList: 1, SVGAnimatedPreserveAspectRatio: 1, SVGAnimatedRect: 1, SVGAnimatedString: 1, SVGAnimatedTransformList: 1, SVGAngle: 1, SVGLength: 1, SVGLengthList: 1, SVGMatrix: 1, SVGNumber: 1, SVGNumberList: 1, SVGPoint: 1, SVGPointList: 1, SVGPreserveAspectRatio: 1, SVGRect: 1, SVGStringList: 1, SVGTransform: 1, SVGTransformList: 1, SVGUnitTypes: 1 };
  function gE(t) { if (!(t && typeof t === 'object' && t.nodeType === 1)) throw inv(); }
  function gV(t, cls) { if (!(t && typeof t === 'object' && __g(t, '_svb') === cls)) throw inv(); }
  function guard(cls, t) { if (VALUE[cls]) gV(t, cls); else gE(t); }
  function mint(cls, el, nm) {
    var C = W[cls];
    if (!C || !C.prototype) return undefined;
    var o = Object.create(C.prototype);
    __hide(o, '_svb', cls);
    if (el) { __hide(o, '_sv_el', el); __hide(o, '_sv_nm', nm); }
    return o;
  }
  function elOf(o) { return __g(o, '_sv_el'); }
  function nmOf(o) { return __g(o, '_sv_nm'); }
  function readRef(el, nm) {
    var a = __mr(el, 'attrs'), k = (nm === 'className') ? 'class' : nm, v = a[k];
    if (v === undefined && nm === 'href') { v = a['xlink:href']; if (v === undefined) v = a['href']; }
    if (v === undefined && nm === 'in1') v = a['in'];
    return v;
  }
  function writeRef(el, nm, v) { __mr(el, 'attrs')[(nm === 'className') ? 'class' : nm] = String(v); }
  function numList(s) {
    if (s === undefined) return [];
    return String(s).split(/[\s,]+/).filter(function (x) { return x !== ''; }).map(Number).filter(function (x) { return x === x; });
  }

  // SVGLength / SVGAngle value forms
  var UNITS = { '': 1, 'px': 5, 'cm': 6, 'mm': 7, 'in': 8, 'pt': 9, 'pc': 10, '%': 2, 'em': 3, 'ex': 4 };
  function lenParts(s) {
    if (s === undefined) return [0, 1, '0'];
    s = String(s).trim();
    var m = /^([+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?)\s*([a-z%]*)$/i.exec(s);
    if (!m) return [0, 1, s];
    var u = UNITS[m[2].toLowerCase()];
    return [parseFloat(m[1]), u === undefined ? 1 : u, s];
  }
  var AUNITS = { '': 1, 'deg': 2, 'rad': 3, 'grad': 4 };
  function angParts(s) {
    if (s === undefined) return [0, 1, '0'];
    s = String(s).trim();
    var m = /^([+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?)\s*([a-z%]*)$/i.exec(s);
    if (!m) return [0, 1, s];
    var u = AUNITS[m[2].toLowerCase()];
    return [parseFloat(m[1]), u === undefined ? 1 : u, s];
  }

  // ── matrix math (getCTM/getScreenCTM) + the transform attribute ──
  function matMul(A, B) {
    return [A[0] * B[0] + A[2] * B[1], A[1] * B[0] + A[3] * B[1],
      A[0] * B[2] + A[2] * B[3], A[1] * B[2] + A[3] * B[3],
      A[0] * B[4] + A[2] * B[5] + A[4], A[1] * B[4] + A[3] * B[5] + A[5]];
  }
  function matOfTransformAttr(s) {
    var M = [1, 0, 0, 1, 0, 0];
    if (!s) return M;
    var re = /(matrix|translate|scale|rotate|skewX|skewY)\s*\(([^)]*)\)/g, m;
    while ((m = re.exec(String(s)))) {
      var a = numList(m[2]), T = [1, 0, 0, 1, 0, 0], rad;
      if (m[1] === 'matrix' && a.length >= 6) T = [a[0], a[1], a[2], a[3], a[4], a[5]];
      else if (m[1] === 'translate') T = [1, 0, 0, 1, a[0] || 0, a.length > 1 ? a[1] : 0];
      else if (m[1] === 'scale') T = [a[0] || 0, 0, 0, a.length > 1 ? a[1] : (a[0] || 0), 0, 0];
      else if (m[1] === 'rotate') { rad = (a[0] || 0) * Math.PI / 180; var cs = Math.cos(rad), sn = Math.sin(rad); T = [cs, sn, -sn, cs, 0, 0]; if (a.length > 2) T = matMul(matMul([1, 0, 0, 1, a[1], a[2]], T), [1, 0, 0, 1, -a[1], -a[2]]); }
      else if (m[1] === 'skewX') { rad = (a[0] || 0) * Math.PI / 180; T = [1, 0, Math.tan(rad), 1, 0, 0]; }
      else if (m[1] === 'skewY') { rad = (a[0] || 0) * Math.PI / 180; T = [1, Math.tan(rad), 0, 1, 0, 0]; }
      M = matMul(M, T);
    }
    return M;
  }
  function vbMatrix(svg) {
    var a = __mr(svg, 'attrs'), vb = a['viewBox'];
    if (vb === undefined) return [1, 0, 0, 1, 0, 0];
    var p = numList(vb);
    if (p.length < 4 || !(p[2] > 0) || !(p[3] > 0)) return [1, 0, 0, 1, 0, 0];
    var W_ = parseFloat(a['width']), H_ = parseFloat(a['height']);
    if (!(W_ > 0)) W_ = 100;
    if (!(H_ > 0)) H_ = 100;
    var pa = a['preserveAspectRatio'] === undefined ? '' : String(a['preserveAspectRatio']);
    var paL = pa.toLowerCase();
    if (paL.indexOf('none') >= 0) { var sx0 = W_ / p[2], sy0 = H_ / p[3]; return [sx0, 0, 0, sy0, -p[0] * sx0, -p[1] * sy0]; }
    var slice = paL.indexOf('slice') >= 0;
    var sx = W_ / p[2], sy = H_ / p[3];
    var s = slice ? Math.max(sx, sy) : Math.min(sx, sy);
    var ax = paL.indexOf('xmin') >= 0 ? 0 : paL.indexOf('xmax') >= 0 ? 2 : 1;
    var ay = paL.indexOf('ymin') >= 0 ? 0 : paL.indexOf('ymax') >= 0 ? 2 : 1;
    return [s, 0, 0, s, -p[0] * s + (W_ - p[2] * s) * ax / 2, -p[1] * s + (H_ - p[3] * s) * ay / 2];
  }
  function upEl(el) { return __mr(el, '__parent') || el.parentNode; }
  function ctmOf(el) {
    var M = [1, 0, 0, 1, 0, 0], chain = [], p = el, vp = null;
    while (p && p.nodeType === 1) {
      chain.push(p);
      if (__mr(p, 'tag') === 'svg') { vp = p; break; }
      p = upEl(p);
    }
    for (var i = 0; i < chain.length; i++) M = matMul(matOfTransformAttr(__mr(chain[i], 'attrs').transform), M);
    if (vp) M = matMul(vbMatrix(vp), M);
    return M;
  }
  function mkMat(M) {
    var m = mint('SVGMatrix');
    if (!m) return m;
    var ks = ['a', 'b', 'c', 'd', 'e', 'f'];
    for (var i = 0; i < 6; i++) __hide(m, '_sv_' + ks[i], M[i]);
    return m;
  }

  // ── path / shape geometry (u2b5-surf measures: path length + pointAtLength) ──
  function numAttr(n, name, dflt) { var v = __mr(n, 'attrs')[name]; var x = parseFloat(v); return (x === x) ? x : dflt; }
  function pathPoly(d) {
    if (!d) return null;
    var toks = String(d).match(/[a-zA-Z]|-?\d*\.?\d+(?:e[-+]?\d+)?/gi);
    if (!toks) return null;
    var pts = [], x = 0, y = 0, sx = 0, sy = 0, cmd = '', i = 0;
    function num() { var v = parseFloat(toks[i++]); return (v === v) ? v : 0; }
    while (i < toks.length) {
      var t = toks[i];
      if (/^[a-zA-Z]$/.test(t)) { cmd = t; i++; }
      if (!cmd) { i++; continue; }
      var c = cmd.toUpperCase();
      if (c === 'M') { x = num(); y = num(); sx = x; sy = y; pts.push([x, y]); cmd = (cmd === 'M') ? 'L' : 'l'; }
      else if (c === 'L') { x = num(); y = num(); pts.push([x, y]); }
      else if (c === 'H') { x = num(); pts.push([x, y]); }
      else if (c === 'V') { y = num(); pts.push([x, y]); }
      else if (c === 'Z') { pts.push([sx, sy]); }
      else if (c === 'C') { num(); num(); x = num(); y = num(); num(); num(); pts.push([x, y]); }
      else if (c === 'S' || c === 'Q') { num(); num(); x = num(); y = num(); pts.push([x, y]); }
      else if (c === 'T') { x = num(); y = num(); pts.push([x, y]); }
      else if (c === 'A') { num(); num(); num(); num(); num(); x = num(); y = num(); pts.push([x, y]); }
      else { i++; }
    }
    return pts;
  }
  function polyLen(pts) { var L = 0; for (var i = 1; i < pts.length; i++) { var dx = pts[i][0] - pts[i - 1][0], dy = pts[i][1] - pts[i - 1][1]; L += Math.sqrt(dx * dx + dy * dy); } return L; }
  function polyPoint(pts, at) {
    var acc = 0;
    for (var i = 1; i < pts.length; i++) {
      var dx = pts[i][0] - pts[i - 1][0], dy = pts[i][1] - pts[i - 1][1], l = Math.sqrt(dx * dx + dy * dy);
      if (l > 0 && acc + l >= at) { var f = (at - acc) / l; return [pts[i - 1][0] + dx * f, pts[i - 1][1] + dy * f]; }
      acc += l;
    }
    return pts.length ? [pts[pts.length - 1][0], pts[pts.length - 1][1]] : [0, 0];
  }
  function shapePts(n) {
    var tag = __mr(n, 'tag');
    if (tag === 'path') return pathPoly(__mr(n, 'attrs').d);
    if (tag === 'rect') { var x = numAttr(n, 'x', 0), y = numAttr(n, 'y', 0), w = numAttr(n, 'width', 0), h = numAttr(n, 'height', 0); return [[x, y], [x + w, y], [x + w, y + h], [x, y + h], [x, y]]; }
    if (tag === 'line') return [[numAttr(n, 'x1', 0), numAttr(n, 'y1', 0)], [numAttr(n, 'x2', 0), numAttr(n, 'y2', 0)]];
    if (tag === 'polyline' || tag === 'polygon') {
      var raw = numList(__mr(n, 'attrs').points), out = [];
      for (var i = 0; i + 1 < raw.length; i += 2) out.push([raw[i], raw[i + 1]]);
      return (tag === 'polygon' && out.length) ? out.concat([out[0]]) : out;
    }
    return null;
  }
  function totalLen(n) {
    var tag = __mr(n, 'tag');
    if (tag === 'circle') return Math.fround(2 * Math.PI * numAttr(n, 'r', 0));
    if (tag === 'ellipse') { var rx = numAttr(n, 'rx', 0), ry = numAttr(n, 'ry', 0); if (!(rx > 0) || !(ry > 0)) return 0; return Math.fround(Math.PI * (3 * (rx + ry) - Math.sqrt((3 * rx + ry) * (rx + 3 * ry)))); }
    var pts = shapePts(n);
    return pts ? Math.fround(polyLen(pts)) : 0;
  }
  function bboxOf(n) {
    var tag = __mr(n, 'tag');
    if (tag === 'rect') { var x = numAttr(n, 'x', 0), y = numAttr(n, 'y', 0), w = numAttr(n, 'width', 0), h = numAttr(n, 'height', 0); return [x, y, w, h]; }
    if (tag === 'circle') { var cx = numAttr(n, 'cx', 0), cy = numAttr(n, 'cy', 0), r = numAttr(n, 'r', 0); return [cx - r, cy - r, 2 * r, 2 * r]; }
    if (tag === 'ellipse') { var ex = numAttr(n, 'cx', 0), ey = numAttr(n, 'cy', 0), rx = numAttr(n, 'rx', 0), ry = numAttr(n, 'ry', 0); return [ex - rx, ey - ry, 2 * rx, 2 * ry]; }
    if (tag === 'line') { var x1 = numAttr(n, 'x1', 0), y1 = numAttr(n, 'y1', 0), x2 = numAttr(n, 'x2', 0), y2 = numAttr(n, 'y2', 0); return [Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1)]; }
    if (tag === 'polyline' || tag === 'polygon' || tag === 'path') {
      var pts = shapePts(n);
      if (!pts || !pts.length) return null;
      var mnx = pts[0][0], mny = pts[0][1], mxx = mnx, mxy = mny;
      for (var i = 1; i < pts.length; i++) { var p = pts[i]; if (p[0] < mnx) mnx = p[0]; if (p[1] < mny) mny = p[1]; if (p[0] > mxx) mxx = p[0]; if (p[1] > mxy) mxy = p[1]; }
      return [mnx, mny, mxx - mnx, mxy - mny];
    }
    return null;
  }
  function unionBBox(n) {
    var b = bboxOf(n), kids = n.childNodes || [], i;
    for (i = 0; i < kids.length; i++) {
      var c = kids[i];
      if (!c || c.nodeType !== 1) continue;
      var cb = unionBBox(c);
      if (!cb) continue;
      var Tt = matOfTransformAttr(__mr(c, 'attrs').transform);
      var xs = [], ys = [];
      var corners = [[cb[0], cb[1]], [cb[0] + cb[2], cb[1]], [cb[0], cb[1] + cb[3]], [cb[0] + cb[2], cb[1] + cb[3]]];
      for (var j = 0; j < 4; j++) { xs.push(Tt[0] * corners[j][0] + Tt[2] * corners[j][1] + Tt[4]); ys.push(Tt[1] * corners[j][0] + Tt[3] * corners[j][1] + Tt[5]); }
      var cbx = Math.min.apply(null, xs), cby = Math.min.apply(null, ys), cbX = Math.max.apply(null, xs), cbY = Math.max.apply(null, ys);
      cb = [cbx, cby, cbX - cbx, cbY - cby];
      if (!b) b = cb;
      else {
        var x0 = Math.min(b[0], cb[0]), y0 = Math.min(b[1], cb[1]), x1 = Math.max(b[0] + b[2], cb[0] + cb[2]), y1 = Math.max(b[1] + b[3], cb[1] + cb[3]);
        b = [x0, y0, x1 - x0, y1 - y0];
      }
    }
    return b;
  }
  function mkBox(b) {
    var r = mint('SVGRect');
    if (!r) return r;
    __hide(r, '_sv_x', Math.fround(b ? b[0] : 0));
    __hide(r, '_sv_y', Math.fround(b ? b[1] : 0));
    __hide(r, '_sv_width', Math.fround(b ? b[2] : 0));
    __hide(r, '_sv_height', Math.fround(b ? b[3] : 0));
    return r;
  }
  function mkPoint(x, y) { var p = mint('SVGPoint'); if (p) { __hide(p, '_sv_x', x); __hide(p, '_sv_y', y); } return p; }
  function textChars(n) {
    var s = '', kids = n.childNodes || [];
    for (var i = 0; i < kids.length; i++) {
      var c = kids[i];
      if (c.nodeType === 3) s += __mr(c, 'text') || '';
      else if (c.nodeType === 1) s += textChars(c);
    }
    return s;
  }
  function nearestSvg(el, any) {
    var p = upEl(el), first = null;
    while (p && p.nodeType === 1) {
      var tg = __mr(p, 'tag');
      if (tg === 'svg') { if (!any) return p; first = first || p; }
      else if (any && (tg === 'symbol' || tg === 'image' || tg === 'marker' || tg === 'pattern')) { first = first || p; }
      p = upEl(p);
    }
    return any ? first : null;
  }

  // ── native-shaped accessor/fn factories ──
  function nget(name, fn) { return __native('get ' + name, 0, fn); }
  function nset(name, fn) { return __native('set ' + name, 1, fn); }
  function nfn(name, len, fn) { return __native(name, len, fn); }
  function genGet(cls, name, code) { return nget(name, function () { guard(cls, this); var v = __g(this, '_sv_' + name); return v === undefined ? dec(code) : v; }); }
  function genSet(cls, name, code) { return nset(name, function (v) { guard(cls, this); __hide(this, '_sv_' + name, coerce(code, v)); }); }
  function genFn(cls, name, len) { return nfn(name, len, function () { guard(cls, this); }); }
  function onVal(v) { if (typeof v === 'function') return v; if (v !== null && typeof v === 'object' && typeof v.handleEvent === 'function') return v; return null; }
  function onGet(name) { return nget(name, function () { gE(this); var v = __g(this, '_sv_' + name); return v === undefined ? null : v; }); }
  function onSet(name) { return nset(name, function (v) { gE(this); __hide(this, '_sv_' + name, onVal(v)); }); }
  function animG(cls, read) { return function (n) { return nget(n, function () { gV(this, cls); return read(this); }); }; }
  function animS(cls, write) { return function (n) { return nset(n, function (v) { gV(this, cls); write(this, v); }); }; }
  function elemG(read) { return function (n) { return nget(n, function () { gE(this); return read.call(this); }); }; }
  function elemS(write) { return function (n) { return nset(n, function (v) { gE(this); return write.call(this, v); }); }; }

  // ── object-typed members (obj: codes): token lists, maps, styles, animated ──
  function mkTokenList(el, attr) {
    var o = Object.create((W.DOMTokenList && W.DOMTokenList.prototype) || Object.prototype);
    __hide(o, '_h3c', 'DOMTokenList');
    if (el) { __hide(o, '_h3el', el); __hide(o, '_h3attr', attr); }
    return o;
  }
  function mkDomStringMap(el) {
    var d = Object.create((W.DOMStringMap && W.DOMStringMap.prototype) || Object.prototype);
    var a = __mr(el, 'attrs');
    for (var k in a) {
      if (k.indexOf('data-') !== 0) continue;
      (function (k2) {
        var camel = k2.slice(5).replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
        try {
          Object.defineProperty(d, camel, {
            get: nget(camel, function () { var v = __mr(el, 'attrs')[k2]; return v === undefined ? undefined : String(v); }),
            set: nset(camel, function (v) { __mr(el, 'attrs')[k2] = String(v); }),
            enumerable: true, configurable: true,
          });
        } catch (e) {}
      })(k);
    }
    return d;
  }
  function elStyle(el) {
    var s = __mr(el, '__style');
    if (!s) {
      s = makeStyle();
      try { if (W.CSSStyleDeclaration && W.CSSStyleDeclaration.prototype) setP(s, W.CSSStyleDeclaration.prototype); } catch (e) {}
      __ns(el, '__style', s);
    }
    return s;
  }
  var OBJGEN = {
    'DOMTokenList': function (el, name) { return mkTokenList(el, name === 'relList' ? 'rel' : name === 'focusGroup' ? 'focusgroup' : name); },
    'DOMStringMap': function (el) { return mkDomStringMap(el); },
    'CSSStyleDeclaration': function (el) { return elStyle(el); },
    'StylePropertyMap': function () { return tagObj({}, 'StylePropertyMap'); },
  };
  function objGetter(cls, name, code) {
    return nget(name, function () {
      guard(cls, this);
      var ck = '_svc_' + name, o = __g(this, ck);
      if (o === undefined) {
        if (OBJGEN[code]) o = OBJGEN[code](this, name);
        else { var C = W[code]; o = (C && C.prototype) ? mint(code, this, name) : null; }
        if (o !== undefined && o !== null) __hide(this, ck, o);
      }
      return o;
    });
  }

  // ── measured behaviours (u2b5-surf 1-4) ─────────────────────────────
  var SP = {};

  // SVGLength: bound readings come from the attribute, writes go back to it.
  function lenOf(o, which) {
    var el = elOf(o);
    var st = __g(o, '_sv_' + which);
    if (st !== undefined) return st;
    if (el) return lenParts(readRef(el, nmOf(o)))[which === 'value' ? 0 : which === 'unitType' ? 1 : 2];
    return which === 'value' ? 0 : which === 'unitType' ? 1 : '0';
  }
  SP['SVGLength|value'] = {
    g: animG('SVGLength', function (o) { return lenOf(o, 'value'); }),
    s: animS('SVGLength', function (o, v) { var x = coerceN(v); __hide(o, '_sv_value', x); var el = elOf(o); if (el) writeRef(el, nmOf(o), String(x)); }),
  };
  SP['SVGLength|unitType'] = {
    g: animG('SVGLength', function (o) { return lenOf(o, 'unitType'); }),
    s: animS('SVGLength', function (o, v) { __hide(o, '_sv_unitType', coerceN(v)); }),
  };
  SP['SVGLength|valueAsString'] = {
    g: animG('SVGLength', function (o) { var st = __g(o, '_sv_valueAsString'); if (st !== undefined) return st; return lenOf(o, 'valueAsString'); }),
    s: animS('SVGLength', function (o, v) {
      var s = String(v);
      if (!/^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?\s*[a-z%]*$/i.test(s.trim())) throw domEx("Failed to execute 'valueAsString' on 'SVGLength': The provided value is not a valid length.", 'SyntaxError');
      var p = lenParts(s);
      __hide(o, '_sv_valueAsString', s);
      __hide(o, '_sv_value', p[0]);
      __hide(o, '_sv_unitType', p[1]);
      var el = elOf(o); if (el) writeRef(el, nmOf(o), s);
    }),
  };
  SP['SVGLength|valueInSpecifiedUnits'] = {
    g: animG('SVGLength', function (o) { var st = __g(o, '_sv_valueInSpecifiedUnits'); if (st !== undefined) return st; return lenOf(o, 'value'); }),
    s: animS('SVGLength', function (o, v) { __hide(o, '_sv_valueInSpecifiedUnits', coerceN(v)); }),
  };
  SP['SVGLength|newValueSpecifiedUnits'] = { f: function (n, len) { return nfn(n, len, function (u, v) { gV(this, 'SVGLength'); __hide(this, '_sv_unitType', coerceN(u)); __hide(this, '_sv_value', coerceN(v)); }); } };
  SP['SVGLength|convertToSpecifiedUnits'] = { f: function (n, len) { return nfn(n, len, function (u) { gV(this, 'SVGLength'); __hide(this, '_sv_unitType', coerceN(u)); }); } };

  // SVGAngle: same pattern, angle units.
  function angOf(o, which) {
    var el = elOf(o);
    var st = __g(o, '_sv_' + which);
    if (st !== undefined) return st;
    if (el) return angParts(readRef(el, nmOf(o)))[which === 'value' ? 0 : which === 'unitType' ? 1 : 2];
    return which === 'value' ? 0 : which === 'unitType' ? 1 : '0';
  }
  SP['SVGAngle|value'] = {
    g: animG('SVGAngle', function (o) { return angOf(o, 'value'); }),
    s: animS('SVGAngle', function (o, v) { var x = coerceN(v); __hide(o, '_sv_value', x); var el = elOf(o); if (el) writeRef(el, nmOf(o), String(x)); }),
  };
  SP['SVGAngle|unitType'] = {
    g: animG('SVGAngle', function (o) { return angOf(o, 'unitType'); }),
    s: animS('SVGAngle', function (o, v) { __hide(o, '_sv_unitType', coerceN(v)); }),
  };
  SP['SVGAngle|valueAsString'] = {
    g: animG('SVGAngle', function (o) { var st = __g(o, '_sv_valueAsString'); if (st !== undefined) return st; return angOf(o, 'valueAsString'); }),
    s: animS('SVGAngle', function (o, v) { var s = String(v); if (!/^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?\s*[a-z%]*$/i.test(s.trim())) throw domEx("Failed to execute 'valueAsString' on 'SVGAngle': The provided value is not a valid angle.", 'SyntaxError'); var p = angParts(s); __hide(o, '_sv_valueAsString', s); __hide(o, '_sv_value', p[0]); __hide(o, '_sv_unitType', p[1]); var el = elOf(o); if (el) writeRef(el, nmOf(o), s); }),
  };
  SP['SVGAngle|valueInSpecifiedUnits'] = {
    g: animG('SVGAngle', function (o) { var st = __g(o, '_sv_valueInSpecifiedUnits'); if (st !== undefined) return st; return angOf(o, 'value'); }),
    s: animS('SVGAngle', function (o, v) { __hide(o, '_sv_valueInSpecifiedUnits', coerceN(v)); }),
  };
  SP['SVGAngle|newValueSpecifiedUnits'] = { f: function (n, len) { return nfn(n, len, function (u, v) { gV(this, 'SVGAngle'); __hide(this, '_sv_unitType', coerceN(u)); __hide(this, '_sv_value', coerceN(v)); }); } };
  SP['SVGAngle|convertToSpecifiedUnits'] = { f: function (n, len) { return nfn(n, len, function (u) { gV(this, 'SVGAngle'); __hide(this, '_sv_unitType', coerceN(u)); }); } };

  // SVGRect: stores first (getBBox/createSVGRect), else the bound viewBox attr.
  function rectRead(o, name) {
    var st = __g(o, '_sv_' + name);
    if (st !== undefined) return st;
    var el = elOf(o);
    if (el) { var p = numList(readRef(el, nmOf(o))); var idx = { x: 0, y: 1, width: 2, height: 3 }[name]; return p[idx] || 0; }
    return 0;
  }
  ['x', 'y', 'width', 'height'].forEach(function (nm) {
    SP['SVGRect|' + nm] = {
      g: animG('SVGRect', function (o) { return rectRead(o, nm); }),
      s: animS('SVGRect', function (o, v) { __hide(o, '_sv_' + nm, coerceN(v)); }),
    };
  });

  // SVGPreserveAspectRatio (align n:6 default, meet 1; meetOrSlice set throws).
  var PA_ALIGN = { 'none': 1, 'xminymin': 2, 'xmidymin': 3, 'xmaxymin': 4, 'xminymid': 5, 'xmidymid': 6, 'xmaxymid': 7, 'xminymax': 8, 'xmidymax': 9, 'xmaxymax': 10 };
  function paParts(o) { var el = elOf(o); if (!el) return []; var raw = readRef(el, nmOf(o)); return raw === undefined ? [] : String(raw).trim().split(/\s+/); }
  SP['SVGPreserveAspectRatio|align'] = {
    g: animG('SVGPreserveAspectRatio', function (o) { var st = __g(o, '_sv_align'); if (st !== undefined) return st; var t = paParts(o); if (!t.length) return 6; return PA_ALIGN[t[0].toLowerCase()] || 6; }),
    s: animS('SVGPreserveAspectRatio', function (o, v) { __hide(o, '_sv_align', coerceN(v)); }),
  };
  SP['SVGPreserveAspectRatio|meetOrSlice'] = {
    g: animG('SVGPreserveAspectRatio', function (o) { var st = __g(o, '_sv_meetOrSlice'); if (st !== undefined) return st; var t = paParts(o); if (t.length > 1 && t[1] === 'slice') return 2; return 1; }),
    s: animS('SVGPreserveAspectRatio', function () { throw domEx("Failed to set the 'meetOrSlice' property on 'SVGPreserveAspectRatio': The provided value is not supported.", 'NotSupportedError'); }),
  };

  // SVGTransform + the transform list.
  SP['SVGTransform|matrix'] = { g: animG('SVGTransform', function (o) { var m = __g(o, '_sv_m'); if (!m) { m = mint('SVGMatrix'); __hide(o, '_sv_m', m); } return m; }) };
  function mtxWrite(o, M) {
    var m = __g(o, '_sv_m'); if (!m) { m = mint('SVGMatrix'); __hide(o, '_sv_m', m); }
    var ks = ['a', 'b', 'c', 'd', 'e', 'f'];
    for (var i = 0; i < 6; i++) __hide(m, '_sv_' + ks[i], M[i]);
  }
  function mtxRead(t, i, dflt) { var x = t ? Number(t[['a', 'b', 'c', 'd', 'e', 'f'][i]]) : NaN; return (x === x) ? x : dflt; }
  SP['SVGTransform|setMatrix'] = { f: function (n, len) { return nfn(n, len, function (t) { gV(this, 'SVGTransform'); mtxWrite(this, [mtxRead(t, 0, 1), mtxRead(t, 1, 0), mtxRead(t, 2, 0), mtxRead(t, 3, 1), mtxRead(t, 4, 0), mtxRead(t, 5, 0)]); }); } };
  SP['SVGTransform|setTranslate'] = { f: function (n, len) { return nfn(n, len, function (x, y) { gV(this, 'SVGTransform'); mtxWrite(this, [1, 0, 0, 1, coerceN(x), coerceN(y)]); __hide(this, '_sv_type', 2); }); } };
  SP['SVGTransform|setScale'] = { f: function (n, len) { return nfn(n, len, function (x, y) { gV(this, 'SVGTransform'); mtxWrite(this, [coerceN(x), 0, 0, y === undefined ? coerceN(x) : coerceN(y), 0, 0]); __hide(this, '_sv_type', 3); }); } };
  SP['SVGTransform|setRotate'] = { f: function (n, len) { return nfn(n, len, function (a) { gV(this, 'SVGTransform'); var r = coerceN(a) * Math.PI / 180, cs = Math.cos(r), sn = Math.sin(r); mtxWrite(this, [cs, sn, -sn, cs, 0, 0]); __hide(this, '_sv_type', 4); __hide(this, '_sv_angle', coerceN(a)); }); } };
  SP['SVGTransform|setSkewX'] = { f: function (n, len) { return nfn(n, len, function (a) { gV(this, 'SVGTransform'); mtxWrite(this, [1, 0, Math.tan(coerceN(a) * Math.PI / 180), 1, 0, 0]); __hide(this, '_sv_type', 5); }); } };
  SP['SVGTransform|setSkewY'] = { f: function (n, len) { return nfn(n, len, function (a) { gV(this, 'SVGTransform'); mtxWrite(this, [1, Math.tan(coerceN(a) * Math.PI / 180), 0, 1, 0, 0]); __hide(this, '_sv_type', 6); }); } };
  function listCount(o) {
    var el = elOf(o);
    if (!el) { var st = __g(o, '_sv_v'); return st === undefined ? 0 : st; }
    var raw = readRef(el, nmOf(o));
    if (raw === undefined) return 0;
    var m = String(raw).match(/[a-zA-Z]+\s*\(/g);
    return m ? m.length : 0;
  }
  SP['SVGTransformList|numberOfItems'] = { g: animG('SVGTransformList', function (o) { return listCount(o); }) };
  SP['SVGTransformList|length'] = { g: animG('SVGTransformList', function (o) { return listCount(o); }) };
  function splitCount(o) {
    var el = elOf(o);
    if (!el) return 0;
    var raw = readRef(el, nmOf(o));
    return raw === undefined ? 0 : numList(raw).length;
  }
  SP['SVGLengthList|numberOfItems'] = { g: animG('SVGLengthList', function (o) { return splitCount(o); }) };
  SP['SVGLengthList|length'] = { g: animG('SVGLengthList', function (o) { return splitCount(o); }) };
  SP['SVGNumberList|numberOfItems'] = { g: animG('SVGNumberList', function (o) { return splitCount(o); }) };
  SP['SVGNumberList|length'] = { g: animG('SVGNumberList', function (o) { return splitCount(o); }) };
  SP['SVGStringList|numberOfItems'] = { g: animG('SVGStringList', function (o) { var el = elOf(o); if (!el) return 0; var raw = readRef(el, nmOf(o)); return raw === undefined ? 0 : String(raw).split(/[\s,]+/).filter(function (x) { return x !== ''; }).length; }) };
  SP['SVGStringList|length'] = { g: animG('SVGStringList', function (o) { var el = elOf(o); if (!el) return 0; var raw = readRef(el, nmOf(o)); return raw === undefined ? 0 : String(raw).split(/[\s,]+/).filter(function (x) { return x !== ''; }).length; }) };
  SP['SVGPointList|numberOfItems'] = { g: animG('SVGPointList', function (o) { var el = elOf(o); if (!el) return 0; var raw = readRef(el, nmOf(o)); return raw === undefined ? 0 : Math.floor(numList(raw).length / 2); }) };
  SP['SVGPointList|length'] = { g: animG('SVGPointList', function (o) { var el = elOf(o); if (!el) return 0; var raw = readRef(el, nmOf(o)); return raw === undefined ? 0 : Math.floor(numList(raw).length / 2); }) };

  // The SVGAnimated* family (bound to the element + member name when there is one).
  function astrRead(o) {
    var el = elOf(o);
    if (el) { var raw = readRef(el, nmOf(o)); return raw === undefined ? '' : String(raw); }
    var h = __g(o, '_sv_v');
    return h === undefined ? '' : h;
  }
  SP['SVGAnimatedString|baseVal'] = {
    g: animG('SVGAnimatedString', astrRead),
    s: animS('SVGAnimatedString', function (o, v) { var el = elOf(o); if (el) writeRef(el, nmOf(o), v); else __hide(o, '_sv_v', String(v)); }),
  };
  SP['SVGAnimatedString|animVal'] = { g: animG('SVGAnimatedString', astrRead) };
  function animCache(o, which, make) { var k = '_sv_' + which, v = __g(o, k); if (v === undefined) { v = make(); if (v !== undefined) __hide(o, k, v); } return v; }
  SP['SVGAnimatedLength|baseVal'] = { g: animG('SVGAnimatedLength', function (o) { return animCache(o, 'b', function () { return mint('SVGLength', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedLength|animVal'] = { g: animG('SVGAnimatedLength', function (o) { return animCache(o, 'a', function () { return mint('SVGLength', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedRect|baseVal'] = { g: animG('SVGAnimatedRect', function (o) { return animCache(o, 'b', function () { return mint('SVGRect', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedRect|animVal'] = { g: animG('SVGAnimatedRect', function (o) { return animCache(o, 'a', function () { return mint('SVGRect', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedPreserveAspectRatio|baseVal'] = { g: animG('SVGAnimatedPreserveAspectRatio', function (o) { return animCache(o, 'b', function () { return mint('SVGPreserveAspectRatio', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedPreserveAspectRatio|animVal'] = { g: animG('SVGAnimatedPreserveAspectRatio', function (o) { return animCache(o, 'a', function () { return mint('SVGPreserveAspectRatio', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedAngle|baseVal'] = { g: animG('SVGAnimatedAngle', function (o) { return animCache(o, 'b', function () { return mint('SVGAngle', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedAngle|animVal'] = { g: animG('SVGAnimatedAngle', function (o) { return animCache(o, 'a', function () { return mint('SVGAngle', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedTransformList|baseVal'] = { g: animG('SVGAnimatedTransformList', function (o) { return animCache(o, 'b', function () { return mint('SVGTransformList', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedTransformList|animVal'] = { g: animG('SVGAnimatedTransformList', function (o) { return animCache(o, 'a', function () { return mint('SVGTransformList', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedLengthList|baseVal'] = { g: animG('SVGAnimatedLengthList', function (o) { return animCache(o, 'b', function () { return mint('SVGLengthList', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedLengthList|animVal'] = { g: animG('SVGAnimatedLengthList', function (o) { return animCache(o, 'a', function () { return mint('SVGLengthList', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedNumberList|baseVal'] = { g: animG('SVGAnimatedNumberList', function (o) { return animCache(o, 'b', function () { return mint('SVGNumberList', elOf(o), nmOf(o)); }); }) };
  SP['SVGAnimatedNumberList|animVal'] = { g: animG('SVGAnimatedNumberList', function (o) { return animCache(o, 'a', function () { return mint('SVGNumberList', elOf(o), nmOf(o)); }); }) };
  function scalarRead(o, dflt, parse) {
    var st = __g(o, '_sv_v');
    if (st !== undefined) return st;
    var el = elOf(o);
    if (el) { var raw = readRef(el, nmOf(o)); if (raw !== undefined) { var x = parse(raw); if (x !== undefined) return x; } }
    return dflt;
  }
  function scalarWrite(o, v) { __hide(o, '_sv_v', v); var el = elOf(o); if (el) writeRef(el, nmOf(o), String(v)); }
  SP['SVGAnimatedNumber|baseVal'] = { g: animG('SVGAnimatedNumber', function (o) { return scalarRead(o, 0, function (r) { var x = parseFloat(r); return (x === x) ? x : undefined; }); }), s: animS('SVGAnimatedNumber', function (o, v) { scalarWrite(o, coerceN(v)); }) };
  SP['SVGAnimatedNumber|animVal'] = { g: animG('SVGAnimatedNumber', function (o) { return scalarRead(o, 0, function (r) { var x = parseFloat(r); return (x === x) ? x : undefined; }); }) };
  SP['SVGAnimatedInteger|baseVal'] = { g: animG('SVGAnimatedInteger', function (o) { return scalarRead(o, 1, function (r) { var x = parseInt(r, 10); return (x === x) ? x : undefined; }); }), s: animS('SVGAnimatedInteger', function (o, v) { scalarWrite(o, Math.trunc(coerceN(v))); }) };
  SP['SVGAnimatedInteger|animVal'] = { g: animG('SVGAnimatedInteger', function (o) { return scalarRead(o, 1, function (r) { var x = parseInt(r, 10); return (x === x) ? x : undefined; }); }) };
  SP['SVGAnimatedEnumeration|baseVal'] = { g: animG('SVGAnimatedEnumeration', function (o) { return scalarRead(o, 1, function (r) { if (/^\d+$/.test(String(r).trim())) return parseInt(r, 10); return undefined; }); }), s: animS('SVGAnimatedEnumeration', function (o, v) { scalarWrite(o, coerceN(v)); }) };
  SP['SVGAnimatedEnumeration|animVal'] = { g: animG('SVGAnimatedEnumeration', function (o) { return scalarRead(o, 1, function (r) { if (/^\d+$/.test(String(r).trim())) return parseInt(r, 10); return undefined; }); }) };
  SP['SVGAnimatedBoolean|baseVal'] = { g: animG('SVGAnimatedBoolean', function (o) { return scalarRead(o, false, function (r) { var s = String(r); return s === 'true' || s === '1' ? true : s === 'false' || s === '0' ? false : undefined; }); }), s: animS('SVGAnimatedBoolean', function (o, v) { scalarWrite(o, !!v); }) };
  SP['SVGAnimatedBoolean|animVal'] = { g: animG('SVGAnimatedBoolean', function (o) { return scalarRead(o, false, function (r) { var s = String(r); return s === 'true' || s === '1' ? true : s === 'false' || s === '0' ? false : undefined; }); }) };

  // ── element behaviours ──────────────────────────────────────────────
  SP['SVGElement|ownerSVGElement'] = { g: elemG(function () { return nearestSvg(this, false); }) };
  SP['SVGElement|viewportElement'] = { g: elemG(function () { return nearestSvg(this, true); }) };
  SP['SVGElement|style'] = { g: elemG(function () { return elStyle(this); }), s: elemS(function (v) { elStyle(this).cssText = String(v); }) };
  SP['SVGElement|focusGroup'] = { s: elemS(function (v) { __mr(this, 'attrs')['focusgroup'] = String(v); }) };
  SP['SVGGraphicsElement|getBBox'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mkBox(unionBBox(this)); }); } };
  SP['SVGGraphicsElement|getCTM'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mkMat(ctmOf(this)); }); } };
  SP['SVGGraphicsElement|getScreenCTM'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mkMat(ctmOf(this)); }); } };
  SP['SVGGraphicsElement|nearestViewportElement'] = { g: elemG(function () { return nearestSvg(this, true); }) };
  SP['SVGGraphicsElement|farthestViewportElement'] = { g: elemG(function () { var p = upEl(this), best = null; while (p && p.nodeType === 1) { if (__mr(p, 'tag') === 'svg') best = p; p = upEl(p); } return best; }) };
  SP['SVGGeometryElement|getTotalLength'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return totalLen(this); }); } };
  SP['SVGGeometryElement|getPointAtLength'] = { f: function (n, len) { return nfn(n, len, function (d) { gE(this); var pts = shapePts(this) || []; var p = polyPoint(pts, coerceN(d)); return mkPoint(Math.fround(p[0]), Math.fround(p[1])); }); } };
  SP['SVGGeometryElement|isPointInFill'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return false; }); } };
  SP['SVGGeometryElement|isPointInStroke'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return false; }); } };
  SP['SVGTextContentElement|getComputedTextLength'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return 0; }); } };
  SP['SVGTextContentElement|getNumberOfChars'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return textChars(this).length; }); } };
  SP['SVGTextContentElement|getSubStringLength'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return 0; }); } };
  SP['SVGTextContentElement|getExtentOfChar'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mkBox(null); }); } };
  SP['SVGTextContentElement|getStartPositionOfChar'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mkPoint(0, 0); }); } };
  SP['SVGTextContentElement|getEndPositionOfChar'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mkPoint(0, 0); }); } };
  SP['SVGTextContentElement|getRotationOfChar'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return 0; }); } };
  SP['SVGTextContentElement|getCharNumAtPosition'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return -1; }); } };
  SP['SVGTextContentElement|selectSubString'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|animationsPaused'] = { f: function (n, len) { return nfn('animationsPaused', 0, function () { gE(this); return false; }); } };
  SP['SVGSVGElement|checkEnclosure'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return false; }); } };
  SP['SVGSVGElement|checkIntersection'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return false; }); } };
  SP['SVGSVGElement|createSVGAngle'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGAngle'); }); } };
  SP['SVGSVGElement|createSVGLength'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGLength'); }); } };
  SP['SVGSVGElement|createSVGMatrix'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGMatrix'); }); } };
  SP['SVGSVGElement|createSVGNumber'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGNumber'); }); } };
  SP['SVGSVGElement|createSVGPoint'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGPoint'); }); } };
  SP['SVGSVGElement|createSVGRect'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGRect'); }); } };
  SP['SVGSVGElement|createSVGTransform'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return mint('SVGTransform'); }); } };
  SP['SVGSVGElement|createSVGTransformFromMatrix'] = { f: function (n, len) { return nfn(n, len, function (m) { gE(this); var t = mint('SVGTransform'); mtxWrite(t, [mtxRead(m, 0, 1), mtxRead(m, 1, 0), mtxRead(m, 2, 0), mtxRead(m, 3, 1), mtxRead(m, 4, 0), mtxRead(m, 5, 0)]); return t; }); } };
  SP['SVGSVGElement|currentScale'] = { g: elemG(function () { return 1; }), s: elemS(function () {}) };
  SP['SVGSVGElement|currentTranslate'] = { s: elemS(function (v) { var p = __g(this, '_svc_currentTranslate'); if (!p) { p = mint('SVGPoint', this, 'currentTranslate'); __hide(this, '_svc_currentTranslate', p); } if (v) { __hide(p, '_sv_x', coerceN(v.x)); __hide(p, '_sv_y', coerceN(v.y)); } }) };
  SP['SVGSVGElement|deselectAll'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|forceRedraw'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|getCurrentTime'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return 0; }); } };
  SP['SVGSVGElement|getElementById'] = { f: function (n, len) { return nfn(n, len, function (id) { gE(this); return findById(this, String(id)); }); } };
  SP['SVGSVGElement|getEnclosureList'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return Object.create((W.NodeList && W.NodeList.prototype) || Object.prototype); }); } };
  SP['SVGSVGElement|getIntersectionList'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return Object.create((W.NodeList && W.NodeList.prototype) || Object.prototype); }); } };
  SP['SVGSVGElement|pauseAnimations'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|setCurrentTime'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|suspendRedraw'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); return 1; }); } };
  SP['SVGSVGElement|unpauseAnimations'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|unsuspendRedraw'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  SP['SVGSVGElement|unsuspendRedrawAll'] = { f: function (n, len) { return nfn(n, len, function () { gE(this); }); } };
  function zoomRead(el) {
    var st = __g(el, '_sv_zoomAndPan');
    if (st !== undefined) return st;
    var raw = __mr(el, 'attrs')['zoomAndPan'];
    if (raw !== undefined) { var s = String(raw).toLowerCase(); if (s === 'disable') return 1; if (s === 'magnify') return 2; if (s === 'unknown') return 0; }
    return 2;
  }
  SP['SVGSVGElement|zoomAndPan'] = { g: elemG(function () { return zoomRead(this); }), s: elemS(function (v) { var n = coerceN(v); __hide(this, '_sv_zoomAndPan', (n === 0 || n === 1 || n === 2) ? n : 0); }) };
  SP['SVGViewElement|zoomAndPan'] = { g: elemG(function () { return zoomRead(this); }), s: elemS(function (v) { var n = coerceN(v); __hide(this, '_sv_zoomAndPan', (n === 0 || n === 1 || n === 2) ? n : 0); }) };
  var RP_OK = { '': 1, 'no-referrer': 1, 'no-referrer-when-downgrade': 1, 'origin': 1, 'origin-when-cross-origin': 1, 'same-origin': 1, 'strict-origin': 1, 'strict-origin-when-cross-origin': 1, 'unsafe-url': 1 };
  SP['SVGAElement|referrerPolicy'] = { g: elemG(function () { var v = __mr(this, 'attrs')['referrerpolicy']; if (v === undefined) return ''; var s = String(v).toLowerCase(); return RP_OK[s] ? s : ''; }), s: elemS(function (v) { __mr(this, 'attrs')['referrerpolicy'] = String(v); }) };
  SP['SVGAElement|interestForElement'] = { s: elemS(function () { throw new TypeError("Failed to set the 'interestForElement' property on 'SVGAElement': The provided value is not of type 'Element'."); }) };
  SP['SVGAElement|relList'] = { s: elemS(function (v) { __mr(this, 'attrs')['rel'] = String(v); }) };
  SP['SVGImageElement|crossOrigin'] = { g: elemG(function () { var v = __mr(this, 'attrs')['crossorigin']; if (v === undefined) return null; var s = String(v); if (s === '' || s === 'anonymous' || s === 'use-credentials') return s === '' ? 'anonymous' : s; return 'anonymous'; }), s: elemS(function (v) { __mr(this, 'attrs')['crossorigin'] = String(v); }) };
  SP['SVGImageElement|decoding'] = { g: elemG(function () { var v = __mr(this, 'attrs')['decoding']; if (v === undefined) return 'auto'; var s = String(v).toLowerCase(); return (s === 'sync' || s === 'async' || s === 'auto') ? s : 'auto'; }), s: elemS(function (v) { __mr(this, 'attrs')['decoding'] = String(v); }) };
  SP['SVGStyleElement|disabled'] = { g: elemG(function () { return false; }), s: elemS(function () {}) };

  // ── reuse helpers + the table-driven rebuild ────────────────────────
  function effDesc(P, name, ex) {
    if (ex && (ex.get || ex.set)) return ex;
    var q = null; try { q = P ? Object.getPrototypeOf(P) : null; } catch (e) {}
    while (q) {
      var d = null; try { d = Object.getOwnPropertyDescriptor(q, name); } catch (e) {}
      if (d && (d.get || d.set)) return d;
      try { q = Object.getPrototypeOf(q); } catch (e) { q = null; }
    }
    return ex || null;
  }
  function defMember(cls, P, tok, ex) {
    var name = tok[0], kind = tok[1], fl = tok[2], gs = tok[3], extra = tok[4];
    if (name === 'constructor') {
      try { Object.defineProperty(P, 'constructor', { value: W[cls], writable: true, enumerable: false, configurable: true }); } catch (e) {}
      return;
    }
    var sp = SP[cls + '|' + name];
    try {
      if (kind === 'a') {
        var c = fl.charAt(1) === '1', e = fl.charAt(2) === '1';
        var s1 = gs.charAt(3) === '1';
        var isOn = /^on[a-z]/.test(name);
        var ruse = isOn ? ((ex && (ex.get || ex.set)) ? ex : null) : effDesc(P, name, ex);
        var gFn;
        if (sp && sp.g) gFn = sp.g(name);
        else if (extra.indexOf('obj:') === 0) gFn = objGetter(cls, name, extra.slice(4));
        else if (ruse && ruse.get) gFn = ruse.get;
        else if (isOn) gFn = onGet(name);
        else gFn = genGet(cls, name, extra);
        var sFn;
        if (s1) sFn = (sp && sp.s) ? sp.s(name) : (ruse && ruse.set) ? ruse.set : isOn ? onSet(name) : genSet(cls, name, extra);
        Object.defineProperty(P, name, { get: gFn, set: sFn, enumerable: e, configurable: c });
      } else if (kind === 'f') {
        var fc = fl.charAt(0) === '1', fe = fl.charAt(1) === '1', fw = fl.charAt(2) === '1';
        var len = parseInt(extra, 10); if (!(len >= 0)) len = 0;
        var fv = (sp && sp.f) ? sp.f(name, len) : (ex && typeof ex.value === 'function') ? ex.value : genFn(cls, name, len);
        Object.defineProperty(P, name, { value: fv, writable: fw, enumerable: fe, configurable: fc });
      } else {
        var dc = fl.charAt(0) === '1', de = fl.charAt(1) === '1', dw = fl.charAt(2) === '1';
        Object.defineProperty(P, name, { value: dec(extra), writable: dw, enumerable: de, configurable: dc });
      }
    } catch (e) {}
  }
  var seen = [];
  for (var cls in T.cls) {
    var C = W[cls];
    if (!C || !C.prototype) continue;
    var P = C.prototype;
    if (seen.indexOf(P) >= 0) continue;
    seen.push(P);
    var par = T.cls[cls].p;
    setP(P, par === '-' ? Object.prototype : (par === 'Element' ? (ELP || Object.prototype) : (W[par] && W[par].prototype) || Object.prototype));
    var mem = T.cls[cls].m, ex = {}, ns = [];
    try { ns = Object.getOwnPropertyNames(P); } catch (e) {}
    var i;
    for (i = 0; i < ns.length; i++) { try { ex[ns[i]] = Object.getOwnPropertyDescriptor(P, ns[i]); } catch (e) {} }
    for (i = 0; i < ns.length; i++) { try { delete P[ns[i]]; } catch (e) {} }
    for (i = 0; i < mem.length; i++) { defMember(cls, P, mem[i], ex[mem[i][0]]); }
    try { Object.defineProperty(P, Symbol.toStringTag, { value: cls, configurable: true }); } catch (e) {}
  }

  // ── constructor constants (install-if-missing; e.g. the measured CP pair) ──
  for (var ccls in T.consts) {
    var CK = W[ccls];
    if (!CK) continue;
    var kv = T.consts[ccls];
    for (var cnm in kv) {
      if (Object.prototype.hasOwnProperty.call(CK, cnm)) continue;
      try { Object.defineProperty(CK, cnm, { value: kv[cnm], writable: false, enumerable: true, configurable: false }); } catch (e) {}
    }
  }

  // ── attr-name reads completed at the Element level — surf Q3/Q1 rows: the
  //    surface stubs return undefined; real impls replace them in place so
  //    flags and own-order are kept and the HTML level stays clean ──
  if (ELP) {
    var __fx = function (name, len, fn) {
      var d = Object.getOwnPropertyDescriptor(ELP, name);
      Object.defineProperty(ELP, name, {
        value: __native(name, len, fn),
        writable: d ? d.writable : true,
        enumerable: d ? d.enumerable : true,
        configurable: d ? d.configurable : true
      });
    };
    __fx("getAttributeNames", 0, function () {
      if (!this || typeof this.nodeType !== "number") throw inv();
      return Object.keys(__mr(this, "attrs"));
    });
    __fx("getAttributeNS", 2, function (ns, local) {
      if (!this || typeof this.nodeType !== "number") throw inv();
      var a = __mr(this, "attrs"), L = String(local), nsv = (ns === null || ns === undefined) ? "" : String(ns);
      var v;
      if (nsv === "http://www.w3.org/1999/xlink") v = a["xlink:" + L];
      else if (nsv === "http://www.w3.org/XML/1998/namespace") v = a["xml:" + L];
      else if (nsv === "http://www.w3.org/2000/xmlns/") v = a["xmlns:" + L];
      else if (nsv === "") { v = a[L]; if (v === undefined) v = a[L.toLowerCase()]; }
      return v === undefined ? null : v;
    });
  }

  // ── the runtime maps + TAG_CTORS purge + createElementNS dispatch ──
  SVGNS = T.tag.ns;
  SVGADJ = T.tag.adj;
  SVGATTR = T.tag.attr;
  for (var tkn in TAG_CTORS) {
    var CC = TAG_CTORS[tkn];
    try { if (CC && typeof CC.name === 'string' && CC.name.indexOf('SVG') === 0) delete TAG_CTORS[tkn]; } catch (e) {}
  }
  __mkNS = function (ns, tag, doc) {
    var t = String(tag), n;
    if (ns === 'http://www.w3.org/2000/svg') {
      var cn = (SVGNS && SVGNS[t]) || 'SVGElement';
      var C2 = g[cn] || g.SVGElement;
      n = Object.create(C2.prototype);
      __mk(n);
      __hide(n, '_sf_nodeType', 1);
      __ns(n, 'tag', t);
      __hide(n, '_sf_tagName', t);
      __hide(n, '_sf_localName', t);
      __hide(n, '_sf_prefix', null);
      __hide(n, '_sf_namespaceURI', String(ns));
      __hide(n, '_sv_ns', 'svg');
    } else {
      var C3 = tagCtor(t);
      n = Object.create(C3.prototype);
      __mk(n);
      __hide(n, '_sf_nodeType', 1);
      __ns(n, 'tag', t.toLowerCase());
      __hide(n, '_sf_tagName', t.toUpperCase());
      __hide(n, '_sf_localName', t);
      __hide(n, '_sf_prefix', null);
      __hide(n, '_sf_namespaceURI', (ns === null || ns === undefined || ns === '') ? 'http://www.w3.org/1999/xhtml' : String(ns));
      __hide(n, '_sv_ns', (ns === 'http://www.w3.org/1998/Math/MathML') ? 'math' : 'html');
    }
    __hide(n, '_sf_childNodes', []);
    __ns(n, '__l', {});
    __ns(n, '__parent', null);
    __hide(n, '_sf_parentNode', null);
    __ns(n, '__style', null);
    __hide(n, '_sf_nodeName', n.tagName);
    __ns(n, '__hidden', false);
    __hide(n, '_sf_offsetWidth', 0); __hide(n, '_sf_offsetHeight', 0); __hide(n, '_sf_offsetTop', 0); __hide(n, '_sf_offsetLeft', 0);
    __hide(n, '_sf_scrollWidth', 0); __hide(n, '_sf_scrollHeight', 0); __hide(n, '_sf_scrollTop', 0); __hide(n, '_sf_scrollLeft', 0);
    __hide(n, '_sf_clientWidth', 0); __hide(n, '_sf_clientHeight', 0);
    __ns(n, 'attrs', {});
    if (t === 'script') { __hide(n, '_sf_async_default', true); }
    __ns(n, '__odoc', doc || document);
    return n;
  };
})();

// ═══ ST-11 U2b-6 (2026-10-22): singletons + tails — the interface-prototype
// materializer, built from assets/u2b6_tables.js (deepdiff W| rows, same-hour
// local Chromium + the u2b6-consts probe). Every broken class prototype is
// rebuilt to Chrome truth (exact members/order/kinds/flags/gs/lengths; existing
// impls reused, new members native-shaped by construction); singleton objects
// move onto their class prototypes; Intl/Atomics/CSS are rebuilt in place;
// shared-prototype aliases are wired (locked protos get a dump-exact clone);
// chain-only children get their missing hop; NodeList gets its behaviours and
// the public querySelectorAll sites build real instances; and the frame realm
// is completed: the frame window is rebuilt to the top window's measured
// own-surface (order + descriptors) with frame-identity overrides, the top
// window carries its frame-indexed props and a live `length`. No per-site
// logic anywhere.
(function () {
  var T = __DCI.U2B6;
  if (!T) return;
  var W = window, g = globalThis;

  function setP(o, p) { try { Object.setPrototypeOf(o, p); } catch (e) { try { o.__proto__ = p; } catch (e2) {} } }
  function inv() { return new TypeError('Illegal invocation'); }
  function nget(name, fn) { return __native('get ' + name, 0, fn); }
  function nset(name, fn) { return __native('set ' + name, 1, fn); }
  function nfn(name, len, fn) { return __native(name, len, fn); }
  function coerceN(v) { var x = Number(v); return (x === x) ? x : 0; }
  function dec(code) {
    if (!code || code === '?') return undefined;
    var c = code.charAt(0);
    if (c === 'n') return Number(code.slice(2));
    return undefined;
  }

  // Minted classes for tags Chrome has no global for (StyleMedia): local, NOT
  // attached to the window (a window own-prop would leak into the surface).
  var MINTED = {};
  function mintCls(name) {
    if (MINTED[name]) return MINTED[name];
    var C = function () { throw new TypeError('Illegal constructor'); };
    try { Object.defineProperty(C, 'name', { value: name, configurable: true }); } catch (e) {}
    MINTED[name] = C;
    return C;
  }
  function clsObj(name) { return W[name] || g[name] || MINTED[name]; }

  // ── measured behaviours (grow as probes pin them) ───────────────────────
  var SP = {};
  function nlItems(o) { var it = __g(o, '_nl_items'); return it || []; }
  SP['NodeList|length'] = { g: function (n) { return nget(n, function () { return nlItems(this).length; }); } };
  SP['NodeList|item'] = { f: function (n, len) { return nfn(n, len, function (i) { var it = nlItems(this); i = coerceN(i); var v = it[i]; return v === undefined ? null : v; }); } };
  SP['NodeList|forEach'] = { f: function (n, len) { return nfn(n, len, function (cb, thisArg) { var it = nlItems(this); for (var i = 0; i < it.length; i++) cb.call(thisArg, it[i], i, this); }); } };
  SP['NodeList|entries'] = { f: function (n, len) { return nfn(n, len, function () { var it = nlItems(this), a = [], i; for (i = 0; i < it.length; i++) a.push([i, it[i]]); return a[Symbol.iterator](); }); } };
  SP['NodeList|keys'] = { f: function (n, len) { return nfn(n, len, function () { var it = nlItems(this), a = [], i; for (i = 0; i < it.length; i++) a.push(i); return a[Symbol.iterator](); }); } };
  SP['NodeList|values'] = { f: function (n, len) { return nfn(n, len, function () { var it = nlItems(this), a = [], i; for (i = 0; i < it.length; i++) a.push(it[i]); return a[Symbol.iterator](); }); } };

  // Request.clone — the tables added Chrome's member shape (F9); give it the
  // one honest behaviour the engine's minimal Request state supports.
  SP['Request|clone'] = { f: function (n, len) { return nfn(n, len, function () { var R = W.Request || g.Request; return new R(this.url, { method: this.method }); }); } };

  // BaseAudioContext spawn methods — the engine's own bodies (the class pass
  // deletes the AudioContext.prototype originals; Chrome keeps them on
  // BaseAudioContext). Findings: OfflineAudioContext-based instruments
  // (audit audio-stable, creepjs audio) live on these.
  function __u6spawn(cn) {
    return function (n, len) { return nfn(n, len, function () { var C = g[cn]; var nd = Object.create(C.prototype); __hide(nd, '_ctx', this); try { __hide(nd, '_brand', C.name); } catch (e2) {} return nd; }); };
  }
  SP['BaseAudioContext|createOscillator'] = { f: __u6spawn('OscillatorNode') };
  SP['BaseAudioContext|createDynamicsCompressor'] = { f: __u6spawn('DynamicsCompressorNode') };
  SP['BaseAudioContext|createGain'] = { f: __u6spawn('GainNode') };
  SP['BaseAudioContext|createAnalyser'] = { f: __u6spawn('AnalyserNode') };
  SP['BaseAudioContext|createBiquadFilter'] = { f: __u6spawn('BiquadFilterNode') };
  SP['BaseAudioContext|createScriptProcessor'] = { f: __u6spawn('ScriptProcessorNode') };
  SP['BaseAudioContext|createPanner'] = { f: __u6spawn('PannerNode') };
  SP['BaseAudioContext|createStereoPanner'] = { f: __u6spawn('StereoPannerNode') };
  SP['BaseAudioContext|createDelay'] = { f: __u6spawn('DelayNode') };
  SP['BaseAudioContext|createConstantSource'] = { f: __u6spawn('ConstantSourceNode') };
  SP['BaseAudioContext|createChannelMerger'] = { f: __u6spawn('AudioNode') };
  SP['BaseAudioContext|createChannelSplitter'] = { f: __u6spawn('AudioNode') };
  SP['BaseAudioContext|createConvolver'] = { f: __u6spawn('ConvolverNode') };
  SP['BaseAudioContext|createWaveShaper'] = { f: __u6spawn('WaveShaperNode') };
  SP['BaseAudioContext|createPeriodicWave'] = { f: __u6spawn('PeriodicWave') };
  // OfflineAudioContext render — the engine's own mkBuffer body (seed 1337, the
  // sub-4500 zeros, the LCG), reachable on OAC.prototype only (Chrome's home).
  SP['OfflineAudioContext|startRendering'] = { f: function (n, len) { return nfn(n, len, function () {
    var self = this;
    __hide(self, '_state', 'running');
    return new Promise(function (resolve) {
      setTimeout(function () {
        var nc = __g(self, '_nc') || 1, blen = __g(self, '_len') || 0, sr = __g(self, '_sr') || 44100;
        var b = Object.create(g.AudioBuffer.prototype);
        __hide(__hide(__hide(b, '_ch', nc), '_len', blen), '_sr', sr);
        var data = [], s = 1337, c, i;
        for (c = 0; c < nc; c++) {
          var d = new Float32Array(blen);
          for (i = 0; i < blen; i++) {
            if (i < 4500) { d[i] = 0; continue; }
            s = (s * 48271) % 2147483647; d[i] = (s / 2147483647 - 0.5) * 0.0001;
          }
          data.push(d);
        }
        __hide(b, '_data', data);
        var ev = { type: 'complete', renderedBuffer: b, target: self };
        try { self.dispatchEvent(ev); } catch (e) {}
        if (typeof self.oncomplete === 'function') { try { self.oncomplete(ev); } catch (e2) {} }
        resolve(b);
      }, 0);
    });
  }); } };
  SP['OfflineAudioContext|length'] = { g: function (n) { return nget(n, function () { return __g(this, '_len'); }); } };
  // ST-11 U4: the class pass deleted the AudioContext.prototype originals for
  // these three (Chrome keeps them on BaseAudioContext); the U2b-6 audio work
  // restored length/startRendering only. Same reader pattern — engine slots
  // _sr/_time/_state are set by mkContext.
  SP['BaseAudioContext|sampleRate'] = { g: function (n) { return nget(n, function () { return __g(this, '_sr'); }); } };
  SP['BaseAudioContext|currentTime'] = { g: function (n) { return nget(n, function () { return __g(this, '_time'); }); } };
  SP['BaseAudioContext|state'] = { g: function (n) { return nget(n, function () { return __g(this, '_state'); }); } };

  // NodeList instances for the public selector API (wrapped at the call sites).
  __DCI.__U6NL = function (arr) {
    var L = Object.create((W.NodeList && W.NodeList.prototype) || Object.prototype);
    __hide(L, '_nl_items', arr || []);
    var n = arr ? arr.length : 0;
    for (var i = 0; i < n; i++) {
      try { Object.defineProperty(L, String(i), { value: arr[i], writable: false, enumerable: true, configurable: true }); } catch (e) {}
    }
    return L;
  };

  // ── member factory (ported from the U2b-5 rebuild contract) ─────────────
  var ELEMG = /Element$/;
  // A stub reader also falls back to the plain-name hidden slot: engine ctors
  // write instance state with __hide(this, "name", …) (a plain assignment would
  // be silently swallowed by a getter-only rebuilt accessor).
  function genGet(cls, name) {
    if (ELEMG.test(cls)) return nget(name, function () { if (!(this && typeof this.nodeType === 'number')) throw inv(); var v = __g(this, '_u6_' + cls + '_' + name); if (v === undefined) v = __g(this, name); return v; });
    return nget(name, function () { var v = __g(this, '_u6_' + cls + '_' + name); if (v === undefined) v = __g(this, name); return v; });
  }
  function genSet(cls, name) {
    if (ELEMG.test(cls)) return nset(name, function (v) { if (!(this && typeof this.nodeType === 'number')) throw inv(); __hide(this, '_u6_' + cls + '_' + name, v); });
    return nset(name, function (v) { __hide(this, '_u6_' + cls + '_' + name, v); });
  }
  function genFn(cls, name, len) {
    if (ELEMG.test(cls)) return nfn(name, len, function () { if (!(this && typeof this.nodeType === 'number')) throw inv(); });
    return nfn(name, len, function () {});
  }

  // A ctor-like function (an interface class): its prototype carries members.
  // Such values are kept verbatim by defM — __native rewraps are method-shaped
  // (non-constructible, no prototype) and must never replace a class (the
  // Intl namespace bug: every Intl.* ctor became a non-constructible stub).
  function ctorLike(fn) {
    if (typeof fn !== 'function') return false;
    var p; try { p = fn.prototype; } catch (e) { return false; }
    if (!p || typeof p !== 'object') return false;
    try {
      var ns = Object.getOwnPropertyNames(p), i;
      for (i = 0; i < ns.length; i++) if (ns[i] !== 'constructor') return true;
      return !!Object.getOwnPropertyDescriptor(p, Symbol.toStringTag);
    } catch (e) { return false; }
  }

  function defM(cls, P, tok, ex) {
    var name = tok[0], kind = tok[1], fl = tok[2], gs = tok[3], extra = tok[4];
    if (name === 'constructor') {
      var C = clsObj(cls);
      try {
        if (kind === 'a') {
          Object.defineProperty(P, 'constructor', {
            get: nget('get constructor', function () { return C; }),
            set: nset('set constructor', function () {}),
            enumerable: fl.charAt(2) === '1', configurable: fl.charAt(1) === '1'
          });
        } else {
          Object.defineProperty(P, 'constructor', {
            value: C, writable: fl.charAt(2) === '1',
            enumerable: fl.charAt(1) === '1', configurable: fl.charAt(0) === '1'
          });
        }
      } catch (e) {}
      return;
    }
    var sp = SP[cls + '|' + name];
    try {
      if (kind === 'a') {
        var c = fl.charAt(1) === '1', e = fl.charAt(2) === '1';
        var gb = gs.charAt(1) === '1', sb = gs.charAt(3) === '1';
        var gFn = (sp && sp.g) ? sp.g(name) : (ex && typeof ex.get === 'function') ? ex.get : genGet(cls, name);
        var sFn;
        if (sb) sFn = (sp && sp.s) ? sp.s(name) : (ex && typeof ex.set === 'function') ? ex.set : genSet(cls, name);
        Object.defineProperty(P, name, { get: gFn, set: sFn, enumerable: e, configurable: c });
      } else if (kind === 'f') {
        var fc = fl.charAt(0) === '1', fe = fl.charAt(1) === '1', fw = fl.charAt(2) === '1';
        var len = parseInt(extra, 10); if (!(len >= 0)) len = 0;
        var fv;
        if (sp && sp.f) fv = sp.f(name, len);
        else if (ex && typeof ex.value === 'function') {
          if (ctorLike(ex.value)) {
            // interface class: keep identity + constructibility; pin the
            // spec name/length (the engine's own may be off — e.g. the
            // engine's Intl.DateTimeFormat.length was 2 vs Chrome's 0);
            // strip the stored source so toString renders the native form
            // (js_clear_fn_source — the ctor-bank mechanism)
            fv = ex.value;
            try { if (fv.name !== name) Object.defineProperty(fv, 'name', { value: name, configurable: true }); } catch (e) {}
            try { if (fv.length !== len) Object.defineProperty(fv, 'length', { value: len, configurable: true }); } catch (e) {}
            try { if (__clearSrc) __clearSrc(fv); } catch (e) {}
          } else fv = __native(name, len, ex.value);
        }
        else fv = genFn(cls, name, len);
        Object.defineProperty(P, name, { value: fv, writable: fw, enumerable: fe, configurable: fc });
      } else {
        var dc = fl.charAt(0) === '1', de = fl.charAt(1) === '1', dw = fl.charAt(2) === '1';
        Object.defineProperty(P, name, { value: dec(extra), writable: dw, enumerable: de, configurable: dc });
      }
    } catch (e) {}
  }

  function applyTag(P, spec, name) {
    try {
      var td = Object.getOwnPropertyDescriptor(P, Symbol.toStringTag);
      if (td && td.configurable) delete P[Symbol.toStringTag];
    } catch (e) {}
    if (spec.tk === 'data') {
      try { Object.defineProperty(P, Symbol.toStringTag, { value: name, configurable: true }); } catch (e) {}
    } else if (spec.tk === 'acc') {
      try {
        Object.defineProperty(P, Symbol.toStringTag, {
          get: __native('get [Symbol.toStringTag]', 0, function () { return spec.tv; }),
          set: __native('set [Symbol.toStringTag]', 1, function () {}),
          enumerable: false, configurable: true
        });
      } catch (e) {}
    }
  }

  var miss = [], built = 0, memb = 0, aliased = 0, aliFall = [], objFixed = 0, selfFixed = 0, hopped = 0;

  // ── capture pass: singleton objects — own members off (incl. a free-standing
  //    toStringTag Chrome keeps on the prototype only), values seeded ────────
  var OBJEX = {};
  for (var onm in T.obj) {
    var O = W[onm] || g[onm];
    var ocls = T.obj[onm];
    if (!O) { miss.push('obj:' + onm); continue; }
    var exs = {}, ns2 = [];
    try { ns2 = Object.getOwnPropertyNames(O); } catch (e) {}
    for (var i2 = 0; i2 < ns2.length; i2++) {
      var d2 = null; try { d2 = Object.getOwnPropertyDescriptor(O, ns2[i2]); } catch (e) {}
      if (!d2) continue;
      exs[ns2[i2]] = d2;
      if ('value' in d2 && d2.value !== undefined) __hide(O, '_u6_' + ocls + '_' + ns2[i2], d2.value);
      if (d2.configurable) { try { delete O[ns2[i2]]; } catch (e) {} }
    }
    try { var td0 = Object.getOwnPropertyDescriptor(O, Symbol.toStringTag); if (td0 && td0.configurable) delete O[Symbol.toStringTag]; } catch (e) {}
    OBJEX[ocls] = exs;
  }

  // ── prototype rebuild: delete-configurable, re-add in Chrome order ──────
  function rebuildP(cls, P, spec, extraEx) {
    var ex = {}, ns = [];
    try { ns = Object.getOwnPropertyNames(P); } catch (e) {}
    for (var i = 0; i < ns.length; i++) {
      var d = null; try { d = Object.getOwnPropertyDescriptor(P, ns[i]); } catch (e) {}
      if (d) {
        ex[ns[i]] = d;
        if (d.configurable) { try { delete P[ns[i]]; } catch (e) {} }
      }
    }
    if (extraEx) { for (var k in extraEx) { if (!ex[k]) ex[k] = extraEx[k]; } }
    var m = spec.m;
    for (var j = 0; j < m.length; j++) { defM(cls, P, m[j], ex[m[j][0]]); memb++; }
    applyTag(P, spec, cls);
    built++;
  }

  // AudioContext spawn methods: the engine defines them on AudioContext.prototype,
  // but Chrome keeps them on BaseAudioContext.prototype — the class rebuild would
  // delete them from AudioContext (its spec has none) before BaseAudioContext's
  // pass can recycle them. Capture them here and hand them to that pass.
  var __u6AUD = {};
  try {
    var __u6ACP = clsObj('AudioContext') && clsObj('AudioContext').prototype;
    var __u6BM = (T.cls['BaseAudioContext'] || { m: [] }).m;
    for (var __u6bi = 0; __u6bi < __u6BM.length; __u6bi++) {
      var __u6bn = __u6BM[__u6bi][0];
      var __u6bd = null; try { __u6bd = Object.getOwnPropertyDescriptor(__u6ACP, __u6bn); } catch (e) {}
      if (__u6bd && typeof __u6bd.value === 'function') __u6AUD[__u6bn] = __u6bd.value;
    }
  } catch (e) {}

  // ── class pass ──────────────────────────────────────────────────────────
  for (var cnm in T.cls) {
    var C = clsObj(cnm);
    if (!C || !C.prototype) { C = mintCls(cnm); }
    rebuildP(cnm, C.prototype, T.cls[cnm], cnm === 'BaseAudioContext' ? __u6AUD : OBJEX[cnm]);
    var pp = T.cls[cnm].p;
    if (pp === '-') setP(C.prototype, Object.prototype);
    else if (pp && pp !== '?') {
      var PC = clsObj(pp);
      if (PC && PC.prototype) setP(C.prototype, PC.prototype);
      else miss.push('parent:' + pp + '>' + cnm);
    }
  }

  // NodeList iteration: the class spec is string-keyed, so the rebuilt prototype
  // lacks Chrome's [Symbol.iterator] (= values). Wire the alias — spread /
  // for-of / destructuring over querySelectorAll results (the universal
  // bundled-code pattern; plain arrays did this pre-U2b-6).
  try {
    var __NLp = clsObj('NodeList') && clsObj('NodeList').prototype;
    if (__NLp && !Object.getOwnPropertyDescriptor(__NLp, Symbol.iterator) && __NLp.values) {
      Object.defineProperty(__NLp, Symbol.iterator, { value: __NLp.values, writable: true, enumerable: false, configurable: true });
    }
  } catch (e) {}

  // ── chain-only children (self matched, the hop was missing) ─────────────
  for (var hnm in T.hop) {
    var HC = clsObj(hnm), HP = clsObj(T.hop[hnm]);
    if (HC && HC.prototype && HP && HP.prototype) { setP(HC.prototype, HP.prototype); hopped++; }
    else miss.push('hop:' + hnm);
  }

  // ── self-object rebuild (Intl / Atomics / CSS) ──────────────────────────
  for (var snm in T.selfobj) {
    var SO = W[snm] || g[snm];
    if (!SO) { miss.push('self:' + snm); continue; }
    rebuildP(snm, SO, T.selfobj[snm], null);
    selfFixed++;
  }

  // ── singleton objects: chain onto their class prototypes ────────────────
  for (var onm2 in T.obj) {
    var O2 = W[onm2] || g[onm2];
    var OC = clsObj(T.obj[onm2]);
    if (!O2 || !OC || !OC.prototype) { miss.push('objchain:' + onm2); continue; }
    setP(O2, OC.prototype);
    objFixed++;
  }

  // ── shared-prototype aliases (Option/webkit*) ───────────────────────────
  for (var anm in T.alias) {
    var F = W[anm] || g[anm];
    var Tgt = clsObj(T.alias[anm]);
    if (!F || !Tgt || !Tgt.prototype) { miss.push('alias:' + anm); continue; }
    if (F.prototype === Tgt.prototype) { aliased++; continue; }
    var ok = false;
    try { F.prototype = Tgt.prototype; ok = (F.prototype === Tgt.prototype); } catch (e) {}
    if (!ok) { try { Object.defineProperty(F, 'prototype', { value: Tgt.prototype, writable: false, configurable: true }); ok = (F.prototype === Tgt.prototype); } catch (e) {} }
    if (ok) { aliased++; }
    else {
      aliFall.push(anm);
      // fallback: clear the stub's own shape, then clone the target
      // prototype's CURRENT own descriptors (order included) + tag + chain.
      if (F.prototype && Tgt.prototype) {
        var fns = []; try { fns = Object.getOwnPropertyNames(F.prototype); } catch (e) {}
        for (var fi = 0; fi < fns.length; fi++) {
          var fd = null; try { fd = Object.getOwnPropertyDescriptor(F.prototype, fns[fi]); } catch (e) {}
          if (fd && fd.configurable) { try { delete F.prototype[fns[fi]]; } catch (e) {} }
        }
        try { var ftag = Object.getOwnPropertyDescriptor(F.prototype, Symbol.toStringTag); if (ftag && ftag.configurable) delete F.prototype[Symbol.toStringTag]; } catch (e) {}
        var tns = []; try { tns = Object.getOwnPropertyNames(Tgt.prototype); } catch (e) {}
        for (var ti = 0; ti < tns.length; ti++) {
          var tdn = null; try { tdn = Object.getOwnPropertyDescriptor(Tgt.prototype, tns[ti]); } catch (e) {}
          if (!tdn) continue;
          try { Object.defineProperty(F.prototype, tns[ti], tdn); } catch (e) {}
        }
        try { var ttag = Object.getOwnPropertyDescriptor(Tgt.prototype, Symbol.toStringTag); if (ttag) Object.defineProperty(F.prototype, Symbol.toStringTag, ttag); } catch (e) {}
        setP(F.prototype, Object.getPrototypeOf(Tgt.prototype));
      }
    }
  }

  // ═══ the frame realm ════════════════════════════════════════════════════
  // The frame window is rebuilt to the top window's measured own-surface
  // (names, order and descriptors == Chrome's frame row) with the bridge's
  // frame-identity values kept where they differ (document/location/self/
  // window/top/parent/frameElement/name/closed/length/globalThis + the frame
  // behaviours: postMessage, timers, focus/blur, …). Chrome's frame-indexed
  // props ("0","1") are top-window-only and never cloned.
  // the base own-surface NAMES (tail-time = pre-page). The frame rebuild
  // filters the LIVE window names through this set, so it sees the
  // post-order-pass order and never page-added globals.
  var __u6BASESET = {};
  try { var __u6bn = Object.getOwnPropertyNames(window); for (var __u6bi = 0; __u6bi < __u6bn.length; __u6bi++) __u6BASESET[__u6bn[__u6bi]] = 1; } catch (e) {}
  __DCI.t.__u6base = __u6BASESET;

  function __u6FrameWin(fw, el) {
    if (!fw || __g(fw, '_u6fw')) return fw;
    __hide(fw, '_u6fw', true);
    var keep = {}, i, ns = [];
    try { ns = Object.getOwnPropertyNames(fw); } catch (e) {}
    for (i = 0; i < ns.length; i++) { var d = null; try { d = Object.getOwnPropertyDescriptor(fw, ns[i]); } catch (e) {} if (d) keep[ns[i]] = d; }
    for (i = 0; i < ns.length; i++) {
      var d2 = keep[ns[i]];
      if (d2 && d2.configurable) { try { delete fw[ns[i]]; } catch (e) {} }
    }
    try { var td = Object.getOwnPropertyDescriptor(fw, Symbol.toStringTag); if (td && td.configurable) delete fw[Symbol.toStringTag]; } catch (e) {}
    var tn = [];
    try { tn = Object.getOwnPropertyNames(window); } catch (e) {}
    var bset = __DCI.t.__u6base || {};
    for (i = 0; i < tn.length; i++) {
      var name = tn[i];
      if (!bset[name]) continue;
      if (/^\d+$/.test(name)) continue;
      var td2 = null; try { td2 = Object.getOwnPropertyDescriptor(window, name); } catch (e) {}
      if (!td2) continue;
      var kd = keep[name];
      try {
        if (kd) {
          var kv = ('value' in kd) ? kd.value : undefined;
          if (td2.get || td2.set) {
            Object.defineProperty(fw, name, {
              get: __native('get ' + name, 0, (function (val) { return function () { return val; }; })(kv)),
              set: td2.set ? __native('set ' + name, 1, function () {}) : undefined,
              enumerable: td2.enumerable, configurable: td2.configurable
            });
          } else if (typeof kv === 'function') {
            var wlen = (typeof window[name] === 'function') ? window[name].length : 0;
            Object.defineProperty(fw, name, { value: __native(name, wlen, kv), writable: td2.writable, enumerable: td2.enumerable, configurable: td2.configurable });
          } else {
            Object.defineProperty(fw, name, { value: kv, writable: td2.writable, enumerable: td2.enumerable, configurable: td2.configurable });
          }
        } else {
          Object.defineProperty(fw, name, td2);
        }
      } catch (e) {}
    }
    return fw;
  }

  // Top-window frame-indexed props ("0","1",…) + live count for `length`.
  function __u6WinIdx() {
    var frames = [];
    try { frames = selectorAll(document, 'iframe'); } catch (e) { frames = []; }
    var prev = __DCI.t.__u6frmN || 0;
    var prevNames = __DCI.t.__u6fnm || [];
    var nowNames = [];
    for (var i = 0; i < frames.length; i++) {
      var fw = null; try { fw = __iframeShell(frames[i]); } catch (e) {}
      if (!fw) continue;
      try { Object.defineProperty(window, String(i), { value: fw, writable: false, enumerable: true, configurable: true }); } catch (e) {}
      var fnm = '';
      try { var nvv = frames[i].name; if (nvv) fnm = String(nvv); } catch (e) {}
      if (!fnm) { try { var aa = __mr(frames[i], 'attrs'); if (aa && aa.name) fnm = String(aa.name); } catch (e) {} }
      if (fnm) {
        try { Object.defineProperty(window, fnm, { value: fw, writable: false, enumerable: true, configurable: true }); nowNames.push(fnm); } catch (e) {}
      }
    }
    for (var j = frames.length; j < prev; j++) {
      try { var dj = Object.getOwnPropertyDescriptor(window, String(j)); if (dj && dj.configurable) delete window[String(j)]; } catch (e) {}
    }
    for (var k2 = 0; k2 < prevNames.length; k2++) {
      if (nowNames.indexOf(prevNames[k2]) < 0) {
        try { var dk = Object.getOwnPropertyDescriptor(window, prevNames[k2]); if (dk && dk.configurable && dk.writable === false) delete window[prevNames[k2]]; } catch (e) {}
      }
    }
    __DCI.t.__u6frmN = frames.length;
    __DCI.t.__u6fnm = nowNames;
  }

  function __u6HasIframe(n) {
    if (!n || n.nodeType !== 1) return false;
    if (__mr(n, 'tag') === 'iframe') return true;
    try { return selectorAll(n, 'iframe').length > 0; } catch (e) { return false; }
  }

  // connect/disconnect hooks: refresh the index when the tree gains/loses frames
  var NP = W.Node && W.Node.prototype;
  if (NP) {
    var __origAC = NP.appendChild;
    if (typeof __origAC === 'function') {
      NP.appendChild = __native('appendChild', __origAC.length, function (n) {
        var r = __origAC.call(this, n);
        try { if (__u6HasIframe(n)) __u6WinIdx(); } catch (e) {}
        return r;
      });
    }
    var __origIB = NP.insertBefore;
    if (typeof __origIB === 'function') {
      NP.insertBefore = __native('insertBefore', __origIB.length, function (n, ref) {
        var r = __origIB.call(this, n, ref);
        try { if (__u6HasIframe(n)) __u6WinIdx(); } catch (e) {}
        return r;
      });
    }
    var __origRC = NP.removeChild;
    if (typeof __origRC === 'function') {
      NP.removeChild = __native('removeChild', __origRC.length, function (n) {
        var was = __u6HasIframe(n);
        var r = __origRC.call(this, n);
        try { if (was) __u6WinIdx(); } catch (e) {}
        return r;
      });
    }
  }

  // every frame-window build completes the frame surface
  try {
    var __origIFS = __iframeShell;
    __iframeShell = function (n) {
      var fw = __origIFS(n);
      try { __u6FrameWin(fw, n); } catch (e) {}
      return fw;
    };
  } catch (e) {}

  // `length` reflects the live child-frame count (shape/flags kept from the
  // U2b-1 [Replaceable] accessor).
  try {
    var ld = Object.getOwnPropertyDescriptor(window, 'length');
    if (ld && ld.configurable) {
      Object.defineProperty(window, 'length', {
        get: __native('get length', 0, function () { return __DCI.t.__u6frmN || 0; }),
        set: ld.set, enumerable: ld.enumerable, configurable: ld.configurable
      });
    }
  } catch (e) {}

  try { __u6WinIdx(); } catch (e) {}

  try {
    __log('U2B6 built=' + built + ' members=' + memb + ' obj=' + objFixed + ' self=' + selfFixed +
      ' hop=' + hopped + ' alias=' + aliased + (aliFall.length ? (' aliasFall=' + aliFall.join(',')) : '') +
      ' frames=' + (__DCI.t.__u6frmN || 0) +
      ' miss=' + miss.length + (miss.length ? (' [' + miss.slice(0, 8).join(',') + ']') : ''));
  } catch (e) {}
})();

// ═══ ST-11 U2b-4 (2026-10-22): the own-order pass (deepdiff W| rows vs the
// same-hour Chromium 154) + member repairs + the seal pass. G = [namesCSV,
// [X,N,...]] reorder groups; F = [opsCSV, [X,N,...]] repairs (ops: e/l/s/c/g/
// d/n — enumerable, fn length, setter, configurable, getter, data flags,
// nativize). Reorder keeps the longest C-prefix already embeddable in the
// live order (bounded below by the last non-configurable member), deletes the
// rest and re-adds them in C order with captured descriptors. The seal pass
// re-locks every name the const paths created configurable in passing
// (__DCI.ordSeal) — the measured non-configurable shape, flags preserved.
(function () {
  try {
    var T = (typeof __DCI !== "undefined" && __DCI) ? __DCI.ORD : null;
    var G = globalThis;
    if (!T || !G) return;
    function pick(X, N) {
      var o = G[X];
      if (o === undefined || o === null) return null;
      try { if (typeof o === "function") o = o.prototype; } catch (e) { return null; }
      for (var h = 0; h < N && o !== null && o !== undefined; h++) {
        try { o = Object.getPrototypeOf(o); } catch (e) { return null; }
      }
      return (o !== null && typeof o === "object") ? o : null;
    }
    var seen = [], fixed = 0, skipped = 0;
    var i, j, g, keys, names, k;
    for (i = 0; i < T.G.length; i++) {
      try {
        g = T.G[i]; names = g[0].split(","); keys = g[1];
        for (j = 0; j < keys.length; j += 2) {
          var o = pick(keys[j], keys[j + 1]);
          if (!o || seen.indexOf(o) >= 0) continue;
          seen.push(o);
          var own = Object.getOwnPropertyNames(o);
          if (own.length !== names.length) { skipped++; try { __log("U2B4 skip-size " + keys[j] + "|" + keys[j + 1]); } catch (e) {} continue; }
          var ds = [], d, ok = true;
          for (k = 0; k < names.length; k++) {
            d = Object.getOwnPropertyDescriptor(o, names[k]);
            if (!d) { ok = false; break; }
            ds.push(d);
          }
          if (!ok) { skipped++; try { __log("U2B4 skip-desc " + keys[j] + "|" + keys[j + 1]); } catch (e) {} continue; }
          var low = 0;
          for (k = 0; k < names.length; k++) if (!ds[k].configurable) low = k + 1;
          var p = 0;
          for (k = 0; k < own.length && p < names.length; k++) if (own[k] === names[p]) p++;
          if (p < low) { skipped++; try { __log("U2B4 skip-infeasible " + keys[j] + "|" + keys[j + 1] + " p=" + p + " low=" + low); } catch (e) {} continue; }
          if (p >= names.length) continue;
          for (k = p; k < names.length; k++) { try { delete o[names[k]]; } catch (e) {} }
          for (k = p; k < names.length; k++) { try { Object.defineProperty(o, names[k], ds[k]); } catch (e) {} }
          fixed++;
        }
      } catch (e) {}
    }
    for (i = 0; i < T.F.length; i++) {
      try {
        g = T.F[i]; keys = g[1];
        var parts = g[0].split(",");
        for (j = 0; j < keys.length; j += 2) {
          var o2 = pick(keys[j], keys[j + 1]);
          if (!o2) continue;
          for (var q = 0; q < parts.length; q++) {
            try {
              var cn2 = parts[q].indexOf(":");
              var nm2 = parts[q].slice(0, cn2), ops = parts[q].slice(cn2 + 1);
              var dd = Object.getOwnPropertyDescriptor(o2, nm2);
              if (!dd) continue;
              var val = dd.value, w = dd.writable, e2 = dd.enumerable, c2 = dd.configurable;
              var gg = dd.get, ss = dd.set, pos = 0;
              while (pos < ops.length) {
                var op = ops.charAt(pos); pos++;
                var num = "";
                while (pos < ops.length && ops.charAt(pos) >= "0" && ops.charAt(pos) <= "9") { num += ops.charAt(pos); pos++; }
                if (op === "e") e2 = num === "1";
                else if (op === "c") c2 = num === "1";
                else if (op === "w") w = num === "1";
                else if (op === "s") { if (num === "0") ss = undefined; }
                else if (op === "g") { if (num === "0") gg = undefined; }
                else if (op === "l") { if (typeof val === "function") { try { Object.defineProperty(val, "length", { value: num | 0, writable: false, enumerable: false, configurable: true }); } catch (e) {} } }
                else if (op === "n") { if (typeof val === "function") val = __native(nm2, num | 0, val); }
                else if (op === "d") {
                  var de = ops.charAt(pos), dc = ops.charAt(pos + 1), dw = ops.charAt(pos + 2);
                  pos += 3; e2 = de === "1"; c2 = dc === "1"; w = dw === "1";
                }
              }
              if (gg || ss) Object.defineProperty(o2, nm2, { get: gg, set: ss, enumerable: e2, configurable: c2 });
              else Object.defineProperty(o2, nm2, { value: val, writable: w, enumerable: e2, configurable: c2 });
            } catch (e) {}
          }
        }
      } catch (e) {}
    }
    var seal = (typeof __DCI !== "undefined" && __DCI && __DCI.ordSeal) || [];
    for (i = 0; i < seal.length; i++) {
      try {
        var so = seal[i][0], sn = seal[i][1];
        var sd = Object.getOwnPropertyDescriptor(so, sn);
        if (sd && sd.configurable) {
          if (sd.get || sd.set) Object.defineProperty(so, sn, { get: sd.get, set: sd.set, enumerable: sd.enumerable, configurable: false });
          else Object.defineProperty(so, sn, { value: sd.value, writable: sd.writable, enumerable: sd.enumerable, configurable: false });
        }
      } catch (e) {}
    }
    try { __log("U2B4 ord fixed=" + fixed + " skipped=" + skipped + " seal=" + seal.length); } catch (e) {}
  } catch (e) {}
})();
// ST-11 U2b-4 END
}).call(globalThis);
