#!/usr/bin/env python3
"""doncore patch: quickjs proxy-trap stack-overflow guards.

Patches the VENDORED quickjs.c used by rquickjs-sys (cargo registry copy).
Idempotent: skips if the guard marker is already present.

WHY: a cyclic prototype chain routed through a Proxy (CreepJS
"failed at chain cycle error": Object.setPrototypeOf(p, Object.create(p)))
walks forever in JS_GetPropertyInternal <-> js_proxy_get native recursion —
QuickJS's plain-object cycle check stops at a proxy's NULL proto slot and
the proxy path forwards to the target, so the cycle is never detected.
Real Chromium converts the runaway walk into a catchable RangeError
("Maximum call stack size exceeded"); unpatched QuickJS ABORTS the process
(8MB native stack overflow, no JS catch possible).

The guard reuses quickjs's own idiom (JS_CallInternal etc.):
js_check_stack_overflow + JS_ThrowStackOverflow -> RangeError with the
same message string as V8. Chrome-observable parity.

Run from anywhere: python3 patches/quickjs-proxy-stack-overflow.py
"""
import glob, re, sys

cands = glob.glob.home if False else glob.glob(
    __import__('os').path.expanduser(
        '~/.cargo/registry/src/*/rquickjs-sys-0.14.0/quickjs/quickjs.c'))
if not cands:
    sys.exit('vendored quickjs.c not found under ~/.cargo/registry')
path = cands[0]
s = open(path).read()
if 'doncore: proxy traps can recurse' in s:
    print('already patched:', path); sys.exit(0)

guards = [
  ("""static int js_proxy_has(JSContext *ctx, JSValueConst obj, JSAtom atom)
{""",
   """static int js_proxy_has(JSContext *ctx, JSValueConst obj, JSAtom atom)
{
    /* doncore: proxy traps can recurse thru the prototype of s->target
       (cyclic __proto__ thru a proxy is not caught by the plain-object
       cycle walk, which stops at a proxy's NULL proto slot); the runaway
       native walk must surface as a catchable RangeError like V8 does. */
    if (js_check_stack_overflow(ctx->rt, 0)) {
        JS_ThrowStackOverflow(ctx);
        return -1;
    }"""),
  ("""static JSValue js_proxy_get(JSContext *ctx, JSValueConst obj, JSAtom atom,
                            JSValueConst receiver)
{""",
   """static JSValue js_proxy_get(JSContext *ctx, JSValueConst obj, JSAtom atom,
                            JSValueConst receiver)
{
    /* doncore: see js_proxy_has */
    if (js_check_stack_overflow(ctx->rt, 0))
        return JS_ThrowStackOverflow(ctx);"""),
  ("""static int js_proxy_set(JSContext *ctx, JSValueConst obj, JSAtom atom,
                        JSValueConst value, JSValueConst receiver, int flags)
{""",
   """static int js_proxy_set(JSContext *ctx, JSValueConst obj, JSAtom atom,
                        JSValueConst value, JSValueConst receiver, int flags)
{
    /* doncore: see js_proxy_has */
    if (js_check_stack_overflow(ctx->rt, 0)) {
        JS_ThrowStackOverflow(ctx);
        return -1;
    }"""),
]
n = 0
for anchor, repl in guards:
    assert s.count(anchor) == 1, 'anchor miss: ' + anchor[:60]
    s = s.replace(anchor, repl); n += 1
m = re.search(r'static int js_proxy_delete_property\(JSContext \*ctx, JSValueConst obj,[^{]*\)\n\{', s)
assert m, 'delete_property sig'
s = s.replace(m.group(0), m.group(0) + """
    /* doncore: see js_proxy_has */
    if (js_check_stack_overflow(ctx->rt, 0)) {
        JS_ThrowStackOverflow(ctx);
        return -1;
    }""")
open(path, 'w').write(s)
print('guards applied:', n + 1, '->', path)
