//! P2 · the runtime: QuickJS + the browser-shaped environment + the net
//! bridge into the wire. One fresh realm per fetch, one structural deadline.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rquickjs::{Context, Function, Runtime};
use rquickjs_sys as qjs;

use crate::wire::{Jar, ReqKind, Wire, WireReq};

pub enum ScriptSlot {
    Inline(String),
    External(String),
    /// `<script type="module">` — deferred pass, real ESM semantics.
    InlineModule(String),
    ExternalModule(String),
}

#[derive(Clone, Debug)]
pub struct NetRecord {
    pub method: String,
    pub url: String,
    pub status: u16,
    pub ms: u128,
    pub len: usize,
    pub ctype: String,
}

pub struct RunReport {
    pub scripts_total: usize,
    pub scripts_failed: usize,
    pub errors: Vec<String>,
    pub console: Vec<String>,
    pub net_calls: usize,
    pub timed_out: bool,
    pub settle_rounds: u32,
    pub nav_html: String,
    pub nav_url: String,
    pub nav_status: u16,
    /// page-initiated navigation (location.reload/assign/replace/href):
    /// (kind, url). None when the page never navigated.
    pub nav_req: Option<(String, String)>,
    /// challenge stall watchdog fired: nothing ADVANCED for
    /// CHALLENGE_STALL_MS (heartbeat-only posts don't count) — round
    /// stopped early instead of burning the budget.
    pub stalled: bool,
    pub stalled_ms: u128,
    /// Non-challenge idle stop: no content/net/jar change for this many ms
    /// (0 = the loop ended for another reason). The stall watchdog's sibling.
    pub idle_ms: u128,
    /// A non-challenge stop_at bound ended a settle loop (silent between-eval
    /// check; the interrupt path reports via `timed_out` instead).
    pub bound_stop: bool,
}

const PRELUDE: &str = include_str!("../assets/prelude.js");

/// ST-4 surface tables — generated from the locally installed Chromium's
/// census enumeration (tools/gen_surface.py). Evaluated before the prelude so
/// the surface materializer at the prelude's tail can consume `__SURF`.
const SURF_TABLES: &str = include_str!("../assets/surface_tables.js");
const SURF_WIN_TABLES: &str = include_str!("../assets/surf_win_tables.js");
/// ST-7 WH (e2) measured statics on pre-existing classes (tools/gen_e2_statics.py).
const E2_TABLES: &str = include_str!("../assets/e2_statics_tables.js");
const EVENT_TABLES: &str = include_str!("../assets/event_tables.js");
const HTML_TABLES: &str = include_str!("../assets/html_tables.js");
/// ST-11 U2b-4 (2026-10-22): the own-order pass tables
/// (tools/gen_order_tables.py) — reorder groups + member repairs.
const ORDER_TABLES: &str = include_str!("../assets/order_tables.js");
/// ST-11 U2b-5 (2026-10-22): the SVG league tables (tools/gen_svg_tables.py).
const SVG_TABLES: &str = include_str!("../assets/svg_tables.js");
/// ST-11 U2b-6 (2026-10-22): the singleton/tail tables
/// (tools/gen_u2b6_tables.py) — interface-prototype rebuilds.
const U2B6_TABLES: &str = include_str!("../assets/u2b6_tables.js");
const QBC_SURF: &[u8] = include_bytes!("../assets/qbc/surface_tables.qbc");
const QBC_SURF_WIN: &[u8] = include_bytes!("../assets/qbc/surf_win_tables.qbc");
const QBC_E2: &[u8] = include_bytes!("../assets/qbc/e2_statics_tables.qbc");
const QBC_EVENT: &[u8] = include_bytes!("../assets/qbc/event_tables.qbc");
const QBC_HTML: &[u8] = include_bytes!("../assets/qbc/html_tables.qbc");
const QBC_ORDER: &[u8] = include_bytes!("../assets/qbc/order_tables.qbc");
const QBC_SVG: &[u8] = include_bytes!("../assets/qbc/svg_tables.qbc");
const QBC_U2B6: &[u8] = include_bytes!("../assets/qbc/u2b6_tables.qbc");
const QBC_PRELUDE: &[u8] = include_bytes!("../assets/qbc/prelude.qbc");

/// Wrap a JS function as a native-shaped callable (see `__mkNativeC` in the
/// prelude bootstrap). A plain `fn` item with explicit `'js` — the HRTB a
/// nested closure cannot express. The closure captures only the registry
/// index — NEVER a `Ctx` (Ctx is refcounted: JS_DupContext on clone, freed
/// on drop; a Ctx stored in heap-closure state keeps its context alive past
/// JS_FreeRuntime and trips the gc_obj_list assertion) and never a JS value.
/// The context is taken at call time off the function object itself.
fn native_wrap<'js>(ctx: rquickjs::Ctx<'js>, id: usize) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx,
        move |fself: rquickjs::function::FuncArg<Function<'js>>,
              this: rquickjs::function::This<rquickjs::Value<'js>>,
              args: rquickjs::function::Rest<rquickjs::Value<'js>>|
              -> rquickjs::Result<rquickjs::Value<'js>> {
            let dci: rquickjs::Object = fself.0.ctx().eval("__DCI")?;
            let reg: rquickjs::Object = dci.get("natives")?;
            let f: Function = reg.get(id as u32)?;
            f.call((this, args))
        },
    )
}

const PROBE_SNIPPET: &str = r#";try{globalThis.__AJDUMP=(function(){try{var F=(typeof globalThis.__AJ2==='function')?globalThis.__AJ2:((typeof aj==='function')?aj:null);if(!F){return 'NO-AJ'}var o={};for(var i=0;i<6000;i++){try{var v=F(i);if(typeof v!=='undefined'){o[i]=String(v).slice(0,90)}}catch(e){}}return JSON.stringify(o)}catch(e){return 'ERR:'+e}})()}catch(e){}"#;
const PROBE2_SNIPPET: &str = r#";try{globalThis.__PROBE2=JSON.stringify({wd:typeof window.document,gd:typeof document,same:(typeof window.document!=='undefined')&&window.document===document,we:typeof window.event,loc:typeof window.location,topEq:window.top===window,wt:typeof window.top,docBody:typeof document.body,gebi:typeof document.getElementById,dqs:typeof document.querySelector,bqs:typeof (document.body&&document.body.querySelector),gn:typeof globalThis,mk:typeof MkvrI5,mkw:typeof window.MkvrI5,mkp:Object.getOwnPropertyNames(window).filter(function(n){return /kvr|Mkv/.test(n)}).slice(0,30)})}catch(e){globalThis.__PROBE2='ERR:'+e}"#;
const PENDING_SCRIPTS_EXPR: &str = "__DCI.pendingScripts ? __DCI.pendingScripts.length : 0";

/// How much text the document body holds — the content-presence sample for
/// the content-stable early-out. This is the HIDDEN-text view (script/style
/// excluded): page code sees Chrome-true textContent, but the settle
/// thresholds were tuned against the old hidden numbers, so the prelude
/// exposes `__DCI.pageTextLen` for exactly this sample.
const TEXT_LEN_EXPR: &str =
    "(function(){try{return __DCI&&__DCI.pageTextLen?__DCI.pageTextLen():0}catch(e){return 0}})()";

/// Content-stable early-out: an ordinary busy page (ad/CMP/analytics timer
/// chains keep firing forever — bloomberg carried 18KB of content and burned
/// 91s settling) is DONE once its text is substantial and stops growing.
/// Never applies while a challenge is in flight — those documents must run
/// their full dance. Returns true when the settle loop should stop.
fn content_settled(
    ctx: &rquickjs::Context,
    tag: &str,
    challenge: bool,
    loop_elapsed_ms: u128,
    prev_len: &mut i64,
    stable: &mut u32,
    sampled_at: &mut Instant,
) -> bool {
    if challenge || loop_elapsed_ms < 900 || sampled_at.elapsed().as_millis() < 400 {
        return false;
    }
    *sampled_at = Instant::now();
    let len: i64 = ctx.with(|ctx| ctx.eval::<i64, _>(TEXT_LEN_EXPR).unwrap_or(0));
    // One tier (sweep #5 tightening): the same >=600-char bar the pipeline
    // treats as "content", proven stable across two ~400ms samples. The old
    // >=2000-first / >=600-after-6s split made flat 600-2000 pages (airbnb:
    // 602 chars flat from 1.2s) wait out six seconds for nothing — stability
    // IS the proof it will not grow; elapsed time was not.
    let threshold: i64 = 600;
    if len >= threshold && *prev_len >= 0 && (len - *prev_len).abs() <= 32 {
        *stable += 1;
    } else {
        *stable = 0;
    }
    *prev_len = len;
    let settled = *stable >= 2;
    if std::env::var("DONCORE_TRACE").is_ok() {
        eprintln!(
            "[settle] {tag} loop={loop_elapsed_ms}ms len={len} th={threshold} stable={} settled={settled}",
            *stable
        );
    }
    settled
}
/// Challenge stall watchdog: a challenge round that stops ADVANCING must not
/// burn the budget (universality draw #1: two CF-widget sites spent their
/// whole budget posting a meow/food heartbeat with zero progress, and
/// Chrome-headless loops the same sites). An ADVANCE is: a non-heartbeat
/// message delivery (prelude __pmAdv), a JS-initiated wire call, a cookie,
/// a pending/new dynamic script, or >=120ms of real JS work in one sample.
/// meow/food (and fail/overrunBegin) posts are liveness — never progress.
/// Never applies to non-challenge pages (content_settled owns those).
const CHALLENGE_STALL_MS: u128 = 1500;
const CHALLENGE_STALL_SAMPLE_MS: u128 = 100;
const CHALLENGE_JS_BUSY_MS: u128 = 120;
/// Runtime override for the stall/busy bounds (experiments): parse `name`
/// or fall back to the compiled default.
fn env_ms(name: &str, default: u128) -> u128 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u128>().ok())
        .unwrap_or(default)
}

struct StallWatch {
    last_progress: Instant,
    last_adv: i64,
    last_net: usize,
    last_jar: usize,
    sampled_at: Instant,
    busy_ms: u128,
    trace: bool,
}

impl StallWatch {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_progress: now,
            last_adv: -1,
            last_net: 0,
            last_jar: 0,
            sampled_at: now,
            busy_ms: 0,
            trace: std::env::var("DONCORE_TRACE").is_ok(),
        }
    }

    fn note_js(&mut self, dt_ms: u128) {
        if dt_ms > self.busy_ms {
            self.busy_ms = dt_ms;
        }
    }

    fn mark_progress(&mut self, what: &str, t0: Instant) {
        self.last_progress = Instant::now();
        if self.trace {
            eprintln!("[adv] {what} t={}ms", t0.elapsed().as_millis());
        }
    }

    /// Sample ~every 100ms. Some(silence_ms) once nothing has advanced for
    /// CHALLENGE_STALL_MS — the caller stops the round.
    fn sample(
        &mut self,
        ctx: &rquickjs::Context,
        jar: &Arc<Mutex<Jar>>,
        harvest: &Rc<RefCell<Vec<NetRecord>>>,
        t0: Instant,
    ) -> Option<u128> {
        if self.sampled_at.elapsed().as_millis() < CHALLENGE_STALL_SAMPLE_MS {
            return None;
        }
        self.sampled_at = Instant::now();
        let adv: i64 = ctx
            .with(|ctx| {
                ctx.eval::<i64, _>("(typeof __DCI.pmAdv === \"function\") ? __DCI.pmAdv() : -1")
            })
            .unwrap_or(-1);
        let net = harvest.borrow().len();
        let jlen = jar.lock().unwrap_or_else(|p| p.into_inner()).len();
        let pend: i32 = ctx
            .with(|ctx| ctx.eval::<i32, _>(PENDING_SCRIPTS_EXPR).unwrap_or(0))
            .max(0);
        let busy = self.busy_ms >= env_ms("DONCORE_JSBUSY_MS", CHALLENGE_JS_BUSY_MS);
        self.busy_ms = 0;
        let mut why = "";
        if adv >= 0 && adv != self.last_adv {
            why = "pm";
        } else if net != self.last_net {
            why = "net";
        } else if jlen != self.last_jar {
            why = "jar";
        } else if pend > 0 {
            why = "pend";
        } else if busy {
            why = "busy";
        }
        if !why.is_empty() {
            if adv >= 0 {
                self.last_adv = adv;
            }
            self.last_net = net;
            self.last_jar = jlen;
            self.last_progress = Instant::now();
            if self.trace {
                eprintln!("[adv] {why} t={}ms", t0.elapsed().as_millis());
            }
            return None;
        }
        let silence = self.last_progress.elapsed().as_millis();
        if silence >= env_ms("DONCORE_STALL_MS", CHALLENGE_STALL_MS) {
            if self.trace {
                eprintln!("[stall] no advance for {silence}ms — stopping challenge round");
            }
            return Some(silence);
        }
        None
    }
}

/// Non-challenge idle bound — the stall watchdog's sibling. The runtime may
/// keep firing timers forever with zero content movement (roblox burned 95s
/// of timer churn at 51 chars; amazon 20s at 213; isolarcloud 12s at 26;
/// booking 20s at 214). Progress = text-length change (>8 chars), a wire
/// call, or a jar change. Timers and JS busy are NOT progress. Never applies
/// to challenge pages (StallWatch owns those) and never fires inside the
/// grace window.
const IDLE_STOP_MS: u128 = 1500;
const IDLE_GRACE_MS: u128 = 700;
const IDLE_SAMPLE_MS: u128 = 250;

struct IdleWatch {
    last_progress: Instant,
    last_len: i64,
    last_net: usize,
    last_jar: usize,
    sampled_at: Instant,
    trace: bool,
}

impl IdleWatch {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_progress: now,
            last_len: -1,
            last_net: 0,
            last_jar: 0,
            sampled_at: now,
            trace: std::env::var("DONCORE_TRACE").is_ok(),
        }
    }

    /// Sample ~every IDLE_SAMPLE_MS. Some(silence_ms) once nothing has
    /// changed for IDLE_STOP_MS after the grace window — the caller stops.
    fn sample(
        &mut self,
        ctx: &rquickjs::Context,
        jar: &Arc<Mutex<Jar>>,
        harvest: &Rc<RefCell<Vec<NetRecord>>>,
        t0: Instant,
    ) -> Option<u128> {
        if self.sampled_at.elapsed().as_millis() < IDLE_SAMPLE_MS {
            return None;
        }
        self.sampled_at = Instant::now();
        let len: i64 = ctx.with(|ctx| ctx.eval::<i64, _>(TEXT_LEN_EXPR).unwrap_or(0));
        let net = harvest.borrow().len();
        let jlen = jar.lock().unwrap_or_else(|p| p.into_inner()).len();
        if self.last_len < 0
            || (len - self.last_len).abs() > 8
            || net != self.last_net
            || jlen != self.last_jar
        {
            self.last_progress = Instant::now();
        }
        self.last_len = len;
        self.last_net = net;
        self.last_jar = jlen;
        if t0.elapsed().as_millis() < IDLE_GRACE_MS {
            self.last_progress = Instant::now();
            return None;
        }
        let silence = self.last_progress.elapsed().as_millis();
        if silence >= IDLE_STOP_MS {
            if self.trace {
                eprintln!("[idle] no progress for {silence}ms — stopping (non-challenge)");
            }
            return Some(silence);
        }
        None
    }
}

// TRACE-only: wrap the api.js message listener so its receives and throws
// surface in the console mirror (the live stall: requestExtraParams accepted
// but never answered).
const API_PROBE_SNIPPET: &str = r#";(function(){try{var __ae=window.addEventListener;window.addEventListener=function(t,f,o){if(t==='message'&&typeof f==='function'){var w=function(ev){try{var d=(ev&&ev.data)||{};__DCI.log('API-RECV ev='+String(d.event)+' @'+Date.now())}catch(eR1){}try{return f.apply(this,arguments)}catch(e){__DCI.log('API-THREW '+String(e&&e.message).slice(0,170)+' @@ '+String(e&&e.stack).split(String.fromCharCode(10)).slice(0,5).join(' | ').slice(0,360));throw e}};var __rr=__ae.call(this,t,w,o);try{__DCI.log('API-REG @'+Date.now())}catch(eR2){}return __rr}return __ae.apply(this,arguments)}}catch(e){}})();"#;

pub fn run_document(
    identity_json: &str,
    page_url: &str,
    tree_json: &str,
    scripts: &[ScriptSlot],
    wire: &Wire,
    budget_ms: u64,
    challenge: bool,
) -> Result<(String, RunReport, Vec<NetRecord>), String> {
    // The JS runtime must live on a thread whose stack is DEEPER than the
    // QuickJS cap set below. On the 8MB main thread the cap can never trip,
    // js_check_stack_overflow is dead code, and any runaway native
    // recursion (a cyclic __proto__ routed through a Proxy — CreepJS's
    // chain-cycle tests hit this) aborts the whole process instead of
    // surfacing as the catchable RangeError a real browser throws.
    // 64MB is lazily committed; the 16MB cap trips well inside it.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, || {
                run_document_inner(
                    identity_json,
                    page_url,
                    tree_json,
                    scripts,
                    wire,
                    budget_ms,
                    challenge,
                )
            })
            .map_err(|e| format!("js thread spawn: {e}"))?
            .join()
            .map_err(|_| "js thread panicked".to_string())?
    })
}

fn run_document_inner(
    identity_json: &str,
    page_url: &str,
    tree_json: &str,
    scripts: &[ScriptSlot],
    wire: &Wire,
    budget_ms: u64,
    challenge: bool,
) -> Result<(String, RunReport, Vec<NetRecord>), String> {
    let rt = Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    // Headroom for the obfuscated bundles' deep call chains. NOTE:
    // rquickjs silently treats limits > 16 MiB as 0 (check disabled) —
    // 16 MiB is the ceiling, and it MUST stay below the 64MB thread stack
    // so the guard trips INSIDE it: that is what makes runaway recursion
    // (cyclic __proto__ through a Proxy) a catchable RangeError instead
    // of a process abort.
    rt.set_max_stack_size(16 * 1024 * 1024);
    // ESM: module scripts and their imports resolve/fetch through the wire
    // (synchronous loader — the wire's worker thread owns the IO).
    // ST-8 unit 7 (dyn3): module FETCH failures (self and import) are
    // recorded so module element events follow Chrome's measured rule —
    // `error` only when some fetch failed, `load` for parse/throw/resolution
    // failures (dyn-sl2..sl6).
    let mod_netfail = Rc::new(Cell::new(false));
    rt.set_loader(
        ModResolver,
        ModLoader {
            wire: wire.clone(),
            page_url: page_url.to_string(),
            netfail: Rc::clone(&mod_netfail),
        },
    );
    // The ACTIVE stop time: starts at the hard structural deadline (the call
    // may not exceed its budget); every progress marker — a script eval, a
    // fired timer batch — bumps it to now + 2000ms (capped by the hard
    // deadline). So a page may never burrow inside one synchronous JS
    // stretch longer than ~2s without progress (booking's AWS-WAF dance:
    // ~19s in a single settle callback; its only net call measured 156ms),
    // while real work that keeps producing events (stockx's 22-chunk init,
    // ~4s in QuickJS) is exempt — cutting mid-eval lost 12k chars. The
    // IdleWatch covers the between-iterations case with a typed note.
    let hard_deadline = Instant::now() + Duration::from_millis(budget_ms);
    let stop_at = Rc::new(Cell::new(hard_deadline));
    fn bump(stop: &std::cell::Cell<Instant>, hard: Instant, challenge: bool) {
        if challenge {
            // Challenge rounds keep the full budget — the stall watchdog
            // owns their no-advance case, and their compute stretches must
            // not be interrupted by this bound.
            return;
        }
        let n = Instant::now() + Duration::from_millis(2000);
        stop.set(if n < hard { n } else { hard });
    }
    let timed_out = Rc::new(Cell::new(false));
    {
        let t = Rc::clone(&timed_out);
        let s = Rc::clone(&stop_at);
        rt.set_interrupt_handler(Some(Box::new(move || {
            if Instant::now() >= s.get() {
                t.set(true);
                true
            } else {
                false
            }
        })));
    }
    let ctx = Context::full(&rt).map_err(|e| format!("context: {e}"))?;
    let harvest: Rc<RefCell<Vec<NetRecord>>> = Rc::new(RefCell::new(Vec::new()));
    let console: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let job_errors: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let last_dyn: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let jar = wire.jar.clone();
    let page_host = url::Url::parse(page_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    // ── host functions + bootstrap + prelude + tree load ─────────────
    ctx.with(|ctx| -> Result<(), String> {
        let wire1 = wire.clone();
        let h1 = Rc::clone(&harvest);
        let call = Function::new(
            ctx.clone(),
            move |method: String,
                  url: String,
                  kind: String,
                  extra_json: String,
                  body: String| -> String {
                let kind = match kind.as_str() {
                    "document" => ReqKind::Document,
                    "script" => ReqKind::Script,
                    "image" => ReqKind::Image,
                    _ => ReqKind::Xhr,
                };
                let (m0, e0, b0) = (method.clone(), extra_json.clone(), body.clone());
                let extra: Vec<(String, String)> =
                    serde_json::from_str(&extra_json).unwrap_or_default();
                let req = WireReq {
                    method: method.clone(),
                    url: url.clone(),
                    kind,
                    extra_headers: extra,
                    body: if body.is_empty() { None } else { Some(body) },
                    ..Default::default()
                };
                match wire1.fetch(req) {
                    Ok(r) => {
                        let ctype = r
                            .headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        h1.borrow_mut().push(NetRecord {
                            method,
                            url: url.clone(),
                            status: r.status,
                            ms: r.ms,
                            len: r.body.len(),
                            ctype: ctype.clone(),
                        });
                        if std::env::var("DONCORE_DUMP_NET").is_ok() {
                            use std::sync::atomic::{AtomicUsize, Ordering};
                            static NET_N: AtomicUsize = AtomicUsize::new(0);
                            let n = NET_N.fetch_add(1, Ordering::SeqCst);
                            let j = serde_json::json!({
                                "method": m0, "url": url, "kind": format!("{kind:?}"),
                                "status": r.status, "ms": r.ms, "ctype": ctype,
                                "req_headers": e0, "req_body": b0,
                                "resp_headers": r.headers, "resp_body": r.body,
                            });
                            let _ = std::fs::write(
                                format!("scratch/net-{n:03}.json"),
                                serde_json::to_vec(&j).unwrap_or_default(),
                            );
                        }
                        serde_json::json!({
                            "ok": true, "status": r.status, "url": r.final_url,
                            "headers": r.headers, "body": r.body, "ms": r.ms, "ctype": ctype,
                        })
                        .to_string()
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
                }
            },
        )
        .map_err(|e| format!("host fn: {e}"))?;
        // The bridge namespace: every host<->JS contact point lives on this ONE
        // object, so the page window carries a single engine name (`__DCI`).
        ctx.eval::<(), _>("let __DCI = {}; /* ST-11 U2b-1: global LEXICAL binding — not an own window prop */").map_err(|e| format!("dci: {e}"))?;
        let dci: rquickjs::Object = ctx.eval("__DCI").map_err(|e| format!("dci: {e}"))?;
        dci
            .set("wireCall", call)
            .map_err(|e| format!("set: {e}"))?;

        let now = Function::new(ctx.clone(), || -> f64 { epoch_ms() })
            .map_err(|e| format!("host fn: {e}"))?;
        dci.set("nowMs", now).map_err(|e| format!("set: {e}"))?;

        let sha = Function::new(ctx.clone(), |input: Vec<u8>| -> Vec<u8> {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&input);
            h.finalize().to_vec()
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("sha256", sha).map_err(|e| format!("set: {e}"))?;

        let slp = Function::new(ctx.clone(), |ms: f64| {
            let m = ms.clamp(0.0, 200.0) as u64;
            std::thread::sleep(std::time::Duration::from_millis(m));
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("sleep", slp).map_err(|e| format!("set: {e}"))?;

        let jar1 = jar.clone();
        let cget = Function::new(ctx.clone(), move |name: String| -> String {
            jar1.lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&name)
                .unwrap_or_default()
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("cookieGet", cget).map_err(|e| format!("set: {e}"))?;

        let jar2 = jar.clone();
        let host2 = page_host.clone();
        let cset = Function::new(ctx.clone(), move |v: String| {
            jar2.lock()
                .unwrap_or_else(|p| p.into_inner())
                .set_js_cookie(&v, &host2);
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("cookieSet", cset).map_err(|e| format!("set: {e}"))?;

        let jar3 = jar.clone();
        let host3 = page_host.clone();
        let call_ = Function::new(ctx.clone(), move || -> String {
            jar3.lock()
                .unwrap_or_else(|p| p.into_inner())
                .all(Some(&host3))
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("cookieAll", call_).map_err(|e| format!("set: {e}"))?;

        let con = Rc::clone(&console);
        let console_cap: usize = if std::env::var("DONCORE_TRACE").is_ok() {
            8000
        } else {
            200
        };
        let log = Function::new(ctx.clone(), move |s: String| {
            let mut c = con.borrow_mut();
            if c.len() < console_cap {
                c.push(s);
            }
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("log", log).map_err(|e| format!("set: {e}"))?;

        let dumpf = Function::new(ctx.clone(), |name: String, body: String| {
            if std::env::var("DONCORE_DUMP_DYN").is_ok() {
                let _ = std::fs::write(format!("scratch/eval-{name}.js"), body.as_bytes());
            }
        })
        .map_err(|e| format!("host fn: {e}"))?;
        dci.set("dumpFile", dumpf).map_err(|e| format!("set: {e}"))?;

        // __mkNativeC: wrap a JS function as a native-shaped callable. The
        // wrapper is a callable class instance — no bytecode — so
        // Function.prototype.toString renders it as
        //   function NAME() { [native code] }
        // exactly the shape real browsers emit for built-ins (the vendored
        // rquickjs-sys patch sets V8's single-line suffix — the stock
        // multiline form made eval 37 chars, the PhantomJS fingerprint).
        // The prelude's `__native(name, len, fn)` helper pins name/length
        // to the V8 descriptor shape.
        // This is how synthesized surfaces stay native-shaped BY
        // CONSTRUCTION — no Function.prototype.toString patching anywhere.
        let mk_native = Function::new(ctx.clone(), native_wrap).map_err(|e| format!("host fn: {e}"))?;
        dci.set("mkNativeC", mk_native).map_err(|e| format!("set: {e}"))?;

        let boot = format!(
            "__DCI.IDENTITY = {}; __DCI.PAGE_URL = {}; __DCI.BOOT_MS = Date.now();",
            identity_json,
            js_string(page_url)
        );
        ctx.eval::<(), _>(boot).map_err(|e| format!("boot: {e}"))?;
        if std::env::var("DONCORE_TRACE").is_ok() {
            let _ = ctx.eval::<(), _>("__DCI.TRACE_ON=true;");
        }
        if std::env::var("DONCORE_FAST_TICKS").is_ok() {
            // tick-chain compression: challenge state machines re-arm the same
            // callback on a ~550ms timeout dozens of times; the cadence is
            // scheduling, not a real deadline. Compressed per-fn after 3 arms;
            // intervals and one-shot waits keep real time.
            let _ = ctx.eval::<(), _>("__DCI.FAST_TICKS=true;");
        }
        let serr = eval_asset(&ctx, SURF_TABLES, QBC_SURF, "surf");
        if !serr.is_empty() {
            return Err(format!("surface tables: {serr}"));
        }
        let werr = eval_asset(&ctx, SURF_WIN_TABLES, QBC_SURF_WIN, "surfwin");
        if !werr.is_empty() {
            return Err(format!("surf win tables: {werr}"));
        }
        let e2err = eval_asset(&ctx, E2_TABLES, QBC_E2, "e2");
        if !e2err.is_empty() {
            return Err(format!("e2 statics table: {e2err}"));
        }
        let everr = eval_asset(&ctx, EVENT_TABLES, QBC_EVENT, "event");
        if !everr.is_empty() {
            return Err(format!("event tables: {everr}"));
        }
        let hterr = eval_asset(&ctx, HTML_TABLES, QBC_HTML, "html");
        if !hterr.is_empty() {
            return Err(format!("html tables: {hterr}"));
        }
        let orerr = eval_asset(&ctx, ORDER_TABLES, QBC_ORDER, "order");
        if !orerr.is_empty() {
            return Err(format!("order tables: {orerr}"));
        }
        let sverr = eval_asset(&ctx, SVG_TABLES, QBC_SVG, "svg");
        if !sverr.is_empty() {
            return Err(format!("svg tables: {sverr}"));
        }
        let u6err = eval_asset(&ctx, U2B6_TABLES, QBC_U2B6, "u2b6");
        if !u6err.is_empty() {
            return Err(format!("u2b6 tables: {u6err}"));
        }
        let perr = eval_asset(&ctx, PRELUDE, QBC_PRELUDE, "prelude");
        // ST-7 WH (h): hold DOMContentLoaded/load through the parse
        let _ = ctx.eval::<(), _>("__DCI.t.parseHeld = true;");
        if !perr.is_empty() {
            return Err(format!("prelude: {perr}"));
        }
        // Surface the real tree-load exception: a bare eval here reports only
        // "Exception generated by QuickJS", which cost a blind rebuild cycle
        // during ST-4; the catch keeps the actual stack in the error.
        let load = format!(
            "try{{__DCI.dcLoad(JSON.parse({}));}}catch(e){{__DCI.treeErr=String((e&&e.name)||\"?\")+\" | \"+String((e&&e.message)||\"\")+\" | \"+String((e && (e.stack||e.message)) || e);}}",
            js_string(tree_json)
        );
        ctx.eval::<(), _>(load).map_err(|e| format!("tree: {e}"))?;
        let terr = ctx.eval::<String, _>("String(__DCI.treeErr||'')").unwrap_or_default();
        if !terr.is_empty() {
            return Err(format!("tree: {terr}"));
        }
        Ok(())
    })?;

    // ── scripts, in document order ───────────────────────────────────
    let mut report = RunReport {
        scripts_total: scripts.len(),
        scripts_failed: 0,
        errors: Vec::new(),
        console: Vec::new(),
        net_calls: 0,
        timed_out: false,
        settle_rounds: 0,
        nav_html: String::new(),
        nav_url: String::new(),
        nav_status: 0,
        nav_req: None,
        stalled: false,
        stalled_ms: 0,
        idle_ms: 0,
        bound_stop: false,
    };
    // Browser-faithful script loading: overlap the external fetches (the
    // wire runs them with browser-like concurrency), execute strictly in
    // document order. Serial per-script RTTs were the whole cost of the
    // script-heavy rows (twitch 5.5s / stockx 4.6s / huntington 7.4s were
    // init-bound in sweep #5). Challenge pages keep the serial path — their
    // timing is part of the dance.
    let mut ext_results: std::collections::VecDeque<_> = if challenge {
        std::collections::VecDeque::new()
    } else {
        let reqs: Vec<WireReq> = scripts
            .iter()
            .filter_map(|slot| match slot {
                ScriptSlot::External(u) => Some(WireReq {
                    method: "GET".into(),
                    url: u.clone(),
                    kind: ReqKind::Script,
                    extra_headers: vec![("referer".into(), page_url.to_string())],
                    body: None,
                    ..Default::default()
                }),
                _ => None, // inline + module slots (modules fetch in the deferred pass)
            })
            .collect();
        wire.fetch_many(reqs).into()
    };
    // Module scripts are implicitly deferred: classic scripts run in document
    // order first, module scripts in document order after (browser execution
    // semantics); module eval goes through real ESM (Module::declare + the
    // runtime loader) instead of global eval.
    let mut ordered: Vec<&ScriptSlot> = Vec::with_capacity(scripts.len());
    for slot in scripts.iter() {
        if !matches!(
            slot,
            ScriptSlot::InlineModule(_) | ScriptSlot::ExternalModule(_)
        ) {
            ordered.push(slot);
        }
    }
    for slot in scripts.iter() {
        if matches!(
            slot,
            ScriptSlot::InlineModule(_) | ScriptSlot::ExternalModule(_)
        ) {
            ordered.push(slot);
        }
    }
    let mut mod_seq = 0usize;
    // ST-8 unit 7 (dyn3): the document module map — a same-URL module script
    // is fetched+declared ONCE per run across the static and dynamic passes
    // (measured vs Chromium 154: stat+stat mc=1, stat+dyn mc=1, dyn+dyn mc=1).
    // Every element still fires — with the module's own OUTCOME (dyn-sl5:
    // Chrome fires `error` on every element when an import fetch failed,
    // `load` otherwise). A URL is recorded once its fetch succeeded; the
    // outcome is filled after the eval — even when the body failed to parse
    // or threw.
    let mut mod_done: Vec<(String, bool)> = Vec::new();
    for slot in ordered {
        if timed_out.get() {
            break;
        }
        if matches!(
            slot,
            ScriptSlot::InlineModule(_) | ScriptSlot::ExternalModule(_)
        ) {
            mod_seq += 1;
            let (src, name) = match slot {
                ScriptSlot::InlineModule(s) => (s.clone(), format!("{page_url}#mod{mod_seq}")),
                ScriptSlot::ExternalModule(u) => {
                    if let Some(dok) = mod_done.iter().find(|(m, _)| m == u).map(|(_, ok)| *ok) {
                        if std::env::var("DONCORE_TRACE").is_ok() {
                            eprintln!("static_mod_dup: {u}");
                        }
                        let key = u.clone();
                        let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, dok));
                        continue;
                    }
                    let r = wire.fetch(WireReq {
                        method: "GET".into(),
                        url: u.clone(),
                        kind: ReqKind::Script,
                        extra_headers: vec![("referer".into(), page_url.to_string())],
                        body: None,
                        ..Default::default()
                    });
                    match r {
                        Ok(r) if (200..300).contains(&r.status) => {
                            mod_done.push((u.clone(), true));
                            (r.body, u.clone())
                        }
                        Ok(r) => {
                            report.scripts_failed += 1;
                            if report.errors.len() < 40 {
                                report.errors.push(format!("{u}: status {}", r.status));
                            }
                            let key = u.clone();
                            let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, false));
                            continue;
                        }
                        Err(e) => {
                            report.scripts_failed += 1;
                            if report.errors.len() < 40 {
                                report.errors.push(format!("{u}: {e}"));
                            }
                            let key = u.clone();
                            let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, false));
                            continue;
                        }
                    }
                }
                _ => continue,
            };
            let is_ext_mod = matches!(slot, ScriptSlot::ExternalModule(_));
            if src.trim().is_empty() {
                if is_ext_mod {
                    let key = name.clone();
                    let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, true));
                }
                continue;
            }
            mod_netfail.set(false);
            let err: String = ctx.with(|ctx| eval_module_catch(&ctx, &name, &src));
            if !err.is_empty() {
                report.scripts_failed += 1;
                if report.errors.len() < 40 {
                    let cut: String = err.chars().take(160).collect();
                    report.errors.push(format!("{name}: {cut}"));
                }
            }
            if is_ext_mod {
                let key = name.clone();
                let ok = !mod_netfail.get();
                if let Some(e) = mod_done.iter_mut().find(|(m, _)| m == &key) {
                    e.1 = ok;
                }
                let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, ok));
            }
            pump_jobs(&rt, 4000, &job_errors);
            continue;
        }
        // document.write target: remember the external URL (if any) — the
        // prelude matches the <script> element by it, or by inline source.
        let ext_url: Option<String> = match &slot {
            ScriptSlot::External(u) => Some(u.clone()),
            _ => None,
        };
        let (src, origin) = match slot {
            ScriptSlot::Inline(s) => (s.clone(), "inline".to_string()),
            ScriptSlot::External(u) => {
                let r = if challenge {
                    wire.fetch(WireReq {
                        method: "GET".into(),
                        url: u.clone(),
                        kind: ReqKind::Script,
                        extra_headers: vec![("referer".into(), page_url.to_string())],
                        body: None,
                        ..Default::default()
                    })
                } else {
                    ext_results
                        .pop_front()
                        .unwrap_or_else(|| Err("wire worker gone".to_string()))
                };
                match r {
                    Ok(r) if (200..300).contains(&r.status) => (r.body, u.clone()),
                    Ok(r) => {
                        report.scripts_failed += 1;
                        report.errors.push(format!("{u}: status {}", r.status));
                        let key = u.clone();
                        let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, false));
                        continue;
                    }
                    Err(e) => {
                        report.scripts_failed += 1;
                        report.errors.push(format!("{u}: {e}"));
                        let key = u.clone();
                        let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, false));
                        continue;
                    }
                }
            }
            _ => continue,
        };
        // document.write: mark the matching <script> as the write target
        // before evaluating (only when the source can call it); cleared
        // after microtasks so late writes no-op instead of landing stale.
        let dw_marking = if src.contains("document.write") {
            let mode = if ext_url.is_some() { "src" } else { "code" };
            let key = ext_url.clone().unwrap_or_else(|| src.clone());
            let _ = ctx.with(|ctx| {
                let dci: rquickjs::Object = match ctx.eval("__DCI") {
                    Ok(d) => d,
                    Err(_) => return,
                };
                if dci.set("DW_KEY", key.clone()).is_err() {
                    return;
                }
                if dci.set("DW_MODE", mode).is_err() {
                    return;
                }
                let _ = ctx.eval::<(), _>("__DCI.dwBegin(__DCI.DW_KEY, __DCI.DW_MODE)");
            });
            true
        } else {
            false
        };
        // ST-7 WH (h): document.currentScript for the running parser script
        // (dyn5: parser:SCRIPT#ps); ends after the microtask pump below.
        {
            let key = ext_url.clone().unwrap_or_else(|| src.clone());
            let mode = if ext_url.is_some() { "src" } else { "code" };
            let _ = ctx.with(|ctx| {
                let dci: rquickjs::Object = match ctx.eval("__DCI") {
                    Ok(d) => d,
                    Err(_) => return,
                };
                if dci.set("CUR_KEY", key.clone()).is_err() {
                    return;
                }
                if dci.set("CUR_MODE", mode).is_err() {
                    return;
                }
                let _ = ctx.eval::<(), _>("__DCI.curBeginKey(__DCI.CUR_KEY, __DCI.CUR_MODE)");
            });
        }
        let err: String = ctx.with(|ctx| eval_catch(&ctx, &src));
        if !err.is_empty() {
            report.scripts_failed += 1;
            if report.errors.len() < 40 {
                let cut: String = err.chars().take(160).collect();
                if origin == "inline" {
                    let head: String = src.trim_start().chars().take(52).collect();
                    let head = head.replace('\n', " ");
                    report.errors.push(format!("inline [{head}]: {cut}"));
                } else {
                    report.errors.push(format!("{origin}: {cut}"));
                }
            }
        }
        pump_jobs(&rt, 4000, &job_errors);
        // ST-7 WH (h): the parser script is done (its microtasks ran with the
        // value still set — measured dyn7 micro:#ps)
        let _ = ctx.with(|ctx| ctx.eval::<(), _>("__DCI.curEnd()"));
        if dw_marking {
            let _ = ctx.with(|ctx| ctx.eval::<(), _>("__DCI.dwEnd()"));
        }
        // ST-8 unit 7 (dyn3): the parser-inserted external script's element
        // fires `load` once it ran — Chrome fires it even for syntax-broken and
        // throwing bodies; fetch failures fired `error` above; inline fires none.
        if let Some(u) = ext_url.as_ref() {
            let key = u.clone();
            let _ = ctx.with(|ctx| sc_done_js(&ctx, &key, true));
        }
    }

    // ── settle: microtasks + timers + lifecycle events ───────────────
    // __settle returns: >0 fired, 0 nothing left, -(wait_ms) earliest pending timer.
    // Timers are honored up to a horizon (a browser never exits with a due timer);
    // waits are sliced so the interrupt deadline stays live.
    // Challenge stall watchdog state (None for non-challenge pages).
    let mut stall_watch: Option<StallWatch> = if challenge {
        Some(StallWatch::new())
    } else {
        None
    };
    let mut idle_watch: Option<IdleWatch> = if challenge {
        None
    } else {
        Some(IdleWatch::new())
    };
    let mut challenge_stalled = false;
    let mut stalled_ms: u128 = 0;
    let mut idle_stop_ms: u128 = 0;
    let mut bound_stop = false;
    let mut settle_iters = 0;
    let settle_t0 = Instant::now();
    let mut early_prev_len: i64 = -1;
    let mut early_stable: u32 = 0;
    let mut early_sampled_at = Instant::now();
    // ST-7 WH (h): the parse is over — release the lifecycle hold so the
    // first settle fires DOMContentLoaded/load ahead of the page timers.
    let _ = ctx.with(|ctx| ctx.eval::<(), _>("__DCI.t.parseHeld = false;"));
    bump(&stop_at, hard_deadline, challenge);
    loop {
        if timed_out.get() || Instant::now() >= stop_at.get() || settle_iters > 5000 {
            if !timed_out.get() && !challenge && Instant::now() >= stop_at.get() {
                bound_stop = true;
            }
            if std::env::var("DONCORE_TRACE").is_ok() {
                eprintln!(
                    "[loop] s1 stop: timed_out={} t={}ms iters={}",
                    timed_out.get(),
                    settle_t0.elapsed().as_millis(),
                    settle_iters
                );
            }
            break;
        }
        settle_iters += 1;
        let js_t0 = Instant::now();
        let fired: i32 = ctx.with(|ctx| ctx.eval::<i32, _>("__DCI.settle()").unwrap_or(0));
        pump_jobs(&rt, 4000, &job_errors);
        report.settle_rounds += 1;
        if let Some(sw) = stall_watch.as_mut() {
            sw.note_js(js_t0.elapsed().as_millis());
            if let Some(ms) = sw.sample(&ctx, &jar, &harvest, settle_t0) {
                challenge_stalled = true;
                stalled_ms = ms;
                break;
            }
        }
        if content_settled(
            &ctx,
            "s1",
            challenge,
            settle_t0.elapsed().as_millis(),
            &mut early_prev_len,
            &mut early_stable,
            &mut early_sampled_at,
        ) {
            break;
        }
        // A dense tick chain (an interval at a sub-horizon cadence) must not
        // starve the dyn <script> queue: with scripts pending, settle at most
        // ~400ms continuously, then hand the queue its task turn (the live
        // precursor main.js starved behind the challenge's own poll chain).
        if settle_t0.elapsed().as_millis() > 400 {
            let pending: i32 = ctx
                .with(|ctx| ctx.eval::<i32, _>(PENDING_SCRIPTS_EXPR).unwrap_or(0))
                .max(0);
            if pending > 0 {
                break;
            }
        }
        if let Some(iw) = idle_watch.as_mut() {
            if let Some(ms) = iw.sample(&ctx, &jar, &harvest, settle_t0) {
                idle_stop_ms = ms;
                break;
            }
        }
        if fired > 0 {
            bump(&stop_at, hard_deadline, challenge);
            continue;
        }
        if fired == 0 {
            if std::env::var("DONCORE_TRACE").is_ok() {
                eprintln!(
                    "[loop] s1 idle (fired=0) t={}ms",
                    settle_t0.elapsed().as_millis()
                );
            }
            break;
        }
        let w = (-fired) as u64;
        // Dynamic horizon: a pending dyn script must not starve behind a
        // dense tick chain (the live precursor case), but it also must not
        // BLOCK the timer chain (the lab submission case — the pre-empt
        // regression). With scripts pending, exit at the next >=300ms pause
        // instead of demanding a 3s gap; timers keep firing either way.
        let horizon: u64 = {
            let pending: i32 = ctx
                .with(|ctx| ctx.eval::<i32, _>(PENDING_SCRIPTS_EXPR).unwrap_or(0))
                .max(0);
            if pending > 0 {
                300
            } else {
                3000
            }
        };
        if w > horizon {
            if std::env::var("DONCORE_TRACE").is_ok() {
                eprintln!(
                    "[loop] s1 horizon-far w={w}ms t={}ms",
                    settle_t0.elapsed().as_millis()
                );
            }
            break; // far-future timer only: nothing near is coming
        }
        let s = w.min(50).max(1);
        let src = format!("__DCI.sleep({s})");
        ctx.with(move |ctx| {
            let _ = ctx.eval::<(), _>(src);
        });
    }

    // ── dynamic <script> insertions (CF interstitial pattern) ────────
    let mut dyn_rounds = 0;
    let mut dyn_dump_seq = 0usize;
    loop {
        if dyn_rounds >= 36 || timed_out.get() || challenge_stalled {
            break;
        }
        let batch_json: String = ctx
            .with(|ctx| ctx.eval::<String, _>("__DCI.takeScripts()"))
            .unwrap_or_else(|_| "[]".to_string());
        let items: Vec<serde_json::Value> = serde_json::from_str(&batch_json).unwrap_or_default();
        if items.is_empty() {
            break;
        }
        dyn_rounds += 1;
        // drain the widget-call log BEFORE this round's eval: challenge
        // bundles wipe custom globals on boot (anti-tamper), which would
        // erase the previous round's evidence.
        if std::env::var("DONCORE_TRACE").is_ok() {
            let drained: String = ctx.with(|ctx| {
                ctx.eval::<String, _>(
                    "(__DCI.t.WLOG&&__DCI.t.WLOG.length)?__DCI.t.WLOG.splice(0,99).join(';'):''",
                )
                .unwrap_or_default()
            });
            if !drained.is_empty() {
                eprintln!("[wlog] {drained}");
            }
        }
        // ONE script per round: a browser runs each script load as its own
        // task with timers firing between tasks. Batching evals back-to-back
        // starves 0-delay callbacks armed by earlier scripts (the turnstile
        // loader's onload dispatch died to that) and lets a later script's
        // state resets land before earlier scripts' async work ran at all.
        for item in items.iter().take(1) {
            if timed_out.get() {
                break;
            }
            let src = item.get("src").and_then(|v| v.as_str()).unwrap_or("");
            let code = item.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let is_mod = item.get("mod").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
            // ST-8 unit 7 (dyn5): every processed round is progress — dup
            // skips and fetch failures included. A run of dup rounds used to
            // skip the pre-eval bump below and the ACTIVE stop time expired
            // mid-drain (sl5: cut at 2.1s, last script never fired).
            bump(&stop_at, hard_deadline, challenge);
            let mut resolved_url: String = String::new();
            let mut body = if !src.is_empty() {
                report.scripts_total += 1;
                resolved_url = url::Url::parse(page_url)
                    .ok()
                    .and_then(|b| b.join(src).ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| src.to_string());
                let resolved = resolved_url.clone();
                if std::env::var("DONCORE_TRACE").is_ok() {
                    eprintln!("dyn-fetch: {resolved}");
                }
                let dup_ok = if is_mod {
                    mod_done
                        .iter()
                        .find(|(m, _)| m == &resolved_url)
                        .map(|(_, ok)| *ok)
                } else {
                    None
                };
                if let Some(dok) = dup_ok {
                    if std::env::var("DONCORE_TRACE").is_ok() {
                        eprintln!("dyn-mod-dup: {resolved}");
                    }
                    let _ = ctx.with(|ctx| {
                        ctx.eval::<bool, _>(if dok {
                            "__DCI.dynDone(1)"
                        } else {
                            "__DCI.dynDone(0)"
                        })
                    });
                    continue;
                }
                match wire.fetch(WireReq {
                    method: "GET".into(),
                    url: resolved.clone(),
                    kind: ReqKind::Script,
                    extra_headers: vec![("referer".into(), page_url.to_string())],
                    body: None,
                    ..Default::default()
                }) {
                    Ok(r) if (200..300).contains(&r.status) => {
                        if is_mod {
                            mod_done.push((resolved_url.clone(), true));
                        }
                        r.body
                    }
                    Ok(r) => {
                        report.scripts_failed += 1;
                        if report.errors.len() < 40 {
                            report
                                .errors
                                .push(format!("{resolved}: status {}", r.status));
                        }
                        let _ = ctx.with(|ctx| ctx.eval::<bool, _>("__DCI.dynDone(0)"));
                        continue;
                    }
                    Err(e) => {
                        report.scripts_failed += 1;
                        if report.errors.len() < 40 {
                            report.errors.push(format!("{resolved}: {e}"));
                        }
                        let _ = ctx.with(|ctx| ctx.eval::<bool, _>("__DCI.dynDone(0)"));
                        continue;
                    }
                }
            } else {
                report.scripts_total += 1;
                code.to_string()
            };
            if body.trim().is_empty() {
                let _ = ctx.with(|ctx| {
                    ctx.eval::<bool, _>(if is_mod && resolved_url.is_empty() {
                        "__DCI.dynDone(1, 0)"
                    } else {
                        "__DCI.dynDone(1)"
                    })
                });
                continue;
            }
            if let Some(sw) = stall_watch.as_mut() {
                sw.mark_progress("dyn", settle_t0);
            }
            if std::env::var("DONCORE_PATCH_DYN").is_ok() {
                let mut b2 = String::with_capacity(body.len() + 1024);
                b2.push_str(
                    "globalThis.__CT2=function(n,o){globalThis.__LASTCALL=n;try{globalThis.__LASTOBJ=(o===null?\"null\":(typeof o)+\":\"+(o&&o.constructor&&o.constructor.name||\"?\")+\":keys[\"+Object.keys(o||{}).slice(0,10).join(\",\")+\"]\"+(typeof o[\"length\"]===\"number\"?\" len=\"+o[\"length\"]:\"\"))}catch(e){globalThis.__LASTOBJ=\"err\"}};",
                );
                b2.push_str(&patch_calltracer(&body));
                body = b2;
            }
            if std::env::var("DONCORE_PROBE_DYN").is_ok() {
                body = body.replacen("aj=a5,", "aj=a5,(globalThis.__AJ2=a5),", 1);
                body.push_str(PROBE_SNIPPET);
                body.push_str(PROBE2_SNIPPET);
            }
            *last_dyn.borrow_mut() = body.clone();
            if std::env::var("DONCORE_DUMP_DYN").is_ok() {
                dyn_dump_seq += 1;
                let _ = std::fs::write(format!("scratch/dyn-run-{dyn_dump_seq}.js"), &body);
            }
            let origin_label = src;
            if std::env::var("DONCORE_TRACE").is_ok()
                && body.contains("case\"requestExtraParams\":{")
            {
                // api.js boot beacon: surface what the loader resolved
                let from = "Pe=V.params.get(\"onload\")";
                let to = "Pe=V.params.get(\"onload\"),(__DCI.t.WLOG=__DCI.t.WLOG||[]).push('PeV='+String(Pe)+' src='+String(V&&V.src).slice(0,80))";
                if body.contains(from) {
                    body = body.replacen(from, to, 1);
                }
                body = format!("{API_PROBE_SNIPPET}{body}");
                // FRH: where does the requestExtraParams case stop? (the send
                // runs through me()->Zr(); Zr's origin guard and the post
                // itself are the two silent exits)
                for (a, b) in [
                    ("case\"requestExtraParams\":{", "case\"requestExtraParams\":{__DCI.log('FRH-IN');"),
                    ("var Xe=De(o,p);", "var Xe=De(o,p);__DCI.log('FRH-XE '+(Xe?1:0));"),
                    (
                        "function me(e,t,r){Zr(e.contentWindow,t,r)}",
                        "function me(e,t,r){__DCI.log('FRH-ME r='+String(r)+'/'+typeof r+' win='+(e?typeof e.contentWindow:'no-e')+' ev='+String(t&&t.event));Zr(e.contentWindow,t,r)}",
                    ),
                    (
                        "function Zr(e,t,r){if(r===void 0||r===\"\"){if(0)var n;return}",
                        "function Zr(e,t,r){__DCI.log('FRH-ZR r='+String(r)+'/'+typeof r+' ev='+String(t&&t.event));if(r===void 0||r===\"\"){if(0)var n;return}",
                    ),
                    // the listener's gate chain: each pass/fail logs (Eu -> Tu ->
                    // widgetMap -> p -> xu -> P). One lap names the drop point.
                    ("if(Eu(i)){", "if(Eu(i)){__DCI.log('FRH-EU1');"),
                    ("if(!Tu(i)){", "if(!Tu(i)){__DCI.log('FRH-TU0');"),
                    (
                        "if(!(v.widgetId===\"\"||!y.widgetMap.has(v.widgetId))){",
                        "if(!(v.widgetId===\"\"||!y.widgetMap.has(v.widgetId))){__DCI.log('FRH-WM1 '+String(v.widgetId)+' map='+Array.from(y.widgetMap.keys()).join('|'));",
                    ),
                    (
                        "if(p!==void 0){if(!xu(i,p,h)){",
                        "if(p!==void 0){__DCI.log('FRH-P1');if(!xu(i,p,h)){",
                    ),
                    ("if(!xu(i,p,h)){", "if(!xu(i,p,h)){__DCI.log('FRH-XU0');"),
                    (
                        "P(v)}}}};y.msgHandler=I",
                        "__DCI.log('FRH-PRAN '+String(v.event));P(v)}}}};y.msgHandler=I",
                    ),
                    (
                        "function R(e){console.warn(",
                        "function R(e){__DCI.log('FRH-R '+String(e).slice(0,140));console.warn(",
                    ),
                    ("o.isResetting=!1;", "o.isResetting=!1;__DCI.log('FRH-A');"),
                    ("function oi(e,t){", "function oi(e,t){__DCI.log('FRH-OI-IN');"),
                    (
                        "&&(pe=void 0),me(Xe,Ce({",
                        "&&(pe=void 0),__DCI.log('FRH-PRE'),me(Xe,Ce({",
                    ),
                    (
                        "a+=Vr(m.tagName.toLowerCase());",
                        "a+=Vr((m.tagName===undefined?(__DCI.log('AA-BAD nt='+String(m.nodeType)+' nodeName='+String(m&&m.nodeName)+' ctor='+String(m&&m.constructor&&m.constructor.name)+' tag='+String(m&&(m.tag!==undefined?m.tag:m.tagName))),'unknown'):m.tagName).toLowerCase());",
                    ),
                ] {
                    if body.contains(a) {
                        body = body.replacen(a, b, 1);
                    } else {
                        eprintln!("[frh] anchor missing: {a}");
                    }
                }
            }
            if is_mod {
                mod_netfail.set(false);
            }
            let err: String = if is_mod {
                // ST-8 unit 7: a dynamic module goes through real ESM (same as
                // the static deferred pass), named by its resolved URL; an
                // inline dynamic module resolves against the page URL.
                let modname = if resolved_url.is_empty() {
                    format!("{page_url}#dynmod{dyn_rounds}")
                } else {
                    resolved_url.clone()
                };
                ctx.with(|ctx| eval_module_catch(&ctx, &modname, &body))
            } else {
                ctx.with(|ctx| eval_catch(&ctx, &body))
            };
            if std::env::var("DONCORE_TRACE").is_ok() {
                // widget-path probe: after each dyn script, surface the
                // turnstile handshake state (the loader exposes window.turnstile;
                // the page defines the onload callback the loader looks up).
                let probe: String = ctx.with(|ctx| {
                    // install call shims once (idempotent): they record calls
                    // into __DCI.t.WLOG for the wprobe to drain.
                    let _ = ctx.eval::<(), _>(r#"try {
  if (!__DCI.t.WLOG) {
    __DCI.t.WLOG = [];
    var __wrap = function (obj, name, tag) {
      try {
        var o = obj[name];
        if (typeof o !== 'function') return;
        obj[name] = function () {
          var a = Array.prototype.slice.call(arguments).map(function (x) { return typeof x === 'object' && x ? (x.tagName ? 'el:' + String(x.tagName).toLowerCase() : 'obj' + (x.sitekey ? ':sk' : '')) : String(x).slice(0, 40); }).join(',');
          __DCI.t.WLOG.push(tag + '(' + a + ')');
          try { var r = o.apply(this, arguments); __DCI.t.WLOG.push(tag + '->ok' + (r === undefined ? '' : ':' + String(r).slice(0, 20))); return r; }
          catch (e) { __DCI.t.WLOG.push(tag + '->THREW ' + String(e && e.message).slice(0, 90)); throw e; }
        };
      } catch (e) {}
    };
    var __wrapNow = function () {
      try { if (window.DraY9 && !window.DraY9.__wrapped) { var o = window.DraY9; window.DraY9 = function () { __DCI.t.WLOG.push('DraY9(' + Array.prototype.slice.call(arguments).map(function (x) { return typeof x; }).join(',') + ')'); try { var r = o.apply(this, arguments); __DCI.t.WLOG.push('DraY9->ok'); return r; } catch (e) { __DCI.t.WLOG.push('DraY9->THREW ' + String(e && e.message).slice(0, 90)); throw e; } }; window.DraY9.__wrapped = 1; } } catch (e) {}
      try { if (window.turnstile && !window.turnstile.__wrapped) { __wrap(window.turnstile, 'render', 'TS.render'); __wrap(window.turnstile, 'execute', 'TS.execute'); __wrap(window.turnstile, 'reset', 'TS.reset'); window.turnstile.__wrapped = 1; } } catch (e) {}
    };
    __wrapNow();
    var __late = setInterval(function () { __wrapNow(); }, 60);
    setTimeout(function () { clearInterval(__late); }, 4000);
  }
} catch (e) {}"#);
                    ctx.eval::<String, _>(
                        "(function(){try{setTimeout(function(){(__DCI.t.WLOG=__DCI.t.WLOG||[]).push('PROBE-T0-FIRED')},0);var sc=document.querySelectorAll('script');var srcs=[];for(var i=0;i<sc.length;i++){srcs.push(String(sc[i].src||sc[i].getAttribute&&sc[i].getAttribute('src')||'').slice(0,50))}return 'WPROBE turnstile='+(typeof window.turnstile)+' DraY9='+(typeof window.DraY9)+' ifs='+document.getElementsByTagName('iframe').length+' glEe8='+(window._cf_chl_opt&&window._cf_chl_opt.glEe8?String(window._cf_chl_opt.glEe8.tag||window._cf_chl_opt.glEe8):'-')+' wlog='+(__DCI.t.WLOG?__DCI.t.WLOG.length+'['+__DCI.t.WLOG.slice(-6).join(';')+']':'-')+' scripts=['+srcs.join('|')+']'+' timers='+(__DCI.timers?__DCI.timers.length+':'+__DCI.timers.slice(0,6).map(function(t){return (t.kind||'t')+(t.ms|0)}).join(','):'private')}catch(e){return 'WPROBE err '+e}})()",
                    )
                    .unwrap_or_else(|_| "WPROBE eval-failed".into())
                });
                eprintln!("[wprobe] {} (after {})", probe, origin_label);
            }
            if std::env::var("DONCORE_PROBE_DYN").is_ok() {
                let d: String = ctx.with(|ctx| {
                    ctx.eval::<String, _>("globalThis.__AJDUMP || ''")
                        .unwrap_or_default()
                });
                if !d.is_empty() {
                    let _ = std::fs::write("scratch/ajdump.json", &d);
                }
                let d2: String = ctx.with(|ctx| {
                    ctx.eval::<String, _>("globalThis.__PROBE2 || ''")
                        .unwrap_or_default()
                });
                if !d2.is_empty() {
                    let _ = std::fs::write("scratch/probe2.json", &d2);
                }
            }
            if !err.is_empty() {
                report.scripts_failed += 1;
                if std::env::var("DONCORE_DUMP_DYN").is_ok() {
                    let n = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let p = format!("scratch/dyn-{n}.js");
                    let _ = std::fs::write(&p, &body);
                    report.errors.push(format!("dumped {p}"));
                }
                if report.errors.len() < 40 {
                    let cut: String = err.chars().take(160).collect();
                    let snip = err_snippet(&err, &body);
                    let last: String = ctx.with(|ctx| {
                        ctx.eval::<String, _>(
                            "String(globalThis.__LASTCALL)+' on '+String(globalThis.__LASTOBJ)",
                        )
                        .unwrap_or_default()
                    });
                    report
                        .errors
                        .push(format!("dyn-script: {cut} {snip} lastcall={last}"));
                }
            }
            // element events (measured vs Chromium 154 same-hour: dyn-sl2..sl6,
            // dyn-dup3/4/5, dyn-inl2): `load` once the fetch succeeded — even
            // when the body was syntax-broken or threw; `error` only when a
            // fetch failed (self — handled above; an import — via the loader
            // flag); inline module scripts fire no element event.
            let mod_ok = !mod_netfail.get();
            if is_mod && !resolved_url.is_empty() {
                if let Some(e) = mod_done.iter_mut().find(|(m, _)| m == &resolved_url) {
                    e.1 = mod_ok;
                }
            }
            let ev_ok = if is_mod { mod_ok } else { true };
            let fire = if is_mod && resolved_url.is_empty() {
                0
            } else {
                1
            };
            let _ = ctx.with(|ctx| {
                ctx.eval::<bool, _>(if fire == 0 {
                    "__DCI.dynDone(1, 0)"
                } else if ev_ok {
                    "__DCI.dynDone(1)"
                } else {
                    "__DCI.dynDone(0)"
                })
            });
            pump_jobs(&rt, 4000, &job_errors);
        }
        let mut dyn_settle_iters = 0;
        let dyn_settle_t0 = Instant::now();
        let mut early2_prev_len: i64 = -1;
        let mut early2_stable: u32 = 0;
        let mut early2_sampled_at = Instant::now();
        loop {
            if timed_out.get() || Instant::now() >= stop_at.get() || dyn_settle_iters > 5000 {
                if !timed_out.get() && !challenge && Instant::now() >= stop_at.get() {
                    bound_stop = true;
                }
                if std::env::var("DONCORE_TRACE").is_ok() {
                    eprintln!(
                        "[loop] s2 stop: timed_out={} t={}ms iters={}",
                        timed_out.get(),
                        dyn_settle_t0.elapsed().as_millis(),
                        dyn_settle_iters
                    );
                }
                break;
            }
            dyn_settle_iters += 1;
            let js_t0 = Instant::now();
            let fired: i32 = ctx.with(|ctx| ctx.eval::<i32, _>("__DCI.settle()").unwrap_or(0));
            pump_jobs(&rt, 4000, &job_errors);
            report.settle_rounds += 1;
            if let Some(sw) = stall_watch.as_mut() {
                sw.note_js(js_t0.elapsed().as_millis());
                if let Some(ms) = sw.sample(&ctx, &jar, &harvest, settle_t0) {
                    challenge_stalled = true;
                    stalled_ms = ms;
                    break;
                }
            }
            if content_settled(
                &ctx,
                "s2",
                challenge,
                dyn_settle_t0.elapsed().as_millis(),
                &mut early2_prev_len,
                &mut early2_stable,
                &mut early2_sampled_at,
            ) {
                break;
            }
            // Pending-script pacing: kept at the lab-tuned 400ms — ST-8
            // unit 7 measured Chrome fetch-ready (~2-15ms/round); the faster
            // prototype matched structurally but shifted the CF eval-probe
            // window (banked, RECEIPTS 56; revisit when the work stage
            // unlocks).
            if dyn_settle_t0.elapsed().as_millis() > 400 {
                let pending: i32 = ctx
                    .with(|ctx| ctx.eval::<i32, _>(PENDING_SCRIPTS_EXPR).unwrap_or(0))
                    .max(0);
                if pending > 0 {
                    break;
                }
            }
            if let Some(iw) = idle_watch.as_mut() {
                if let Some(ms) = iw.sample(&ctx, &jar, &harvest, settle_t0) {
                    idle_stop_ms = ms;
                    break;
                }
            }
            if fired > 0 {
                bump(&stop_at, hard_deadline, challenge);
                continue;
            }
            if fired == 0 {
                if std::env::var("DONCORE_TRACE").is_ok() {
                    eprintln!(
                        "[loop] s2 idle (fired=0) t={}ms",
                        dyn_settle_t0.elapsed().as_millis()
                    );
                }
                break;
            }
            let w = (-fired) as u64;
            if w > 3000 {
                if std::env::var("DONCORE_TRACE").is_ok() {
                    eprintln!(
                        "[loop] s2 horizon-far w={w}ms t={}ms",
                        dyn_settle_t0.elapsed().as_millis()
                    );
                }
                break;
            }
            let s = w.min(50).max(1);
            let src = format!("__DCI.sleep({s})");
            ctx.with(move |ctx| {
                let _ = ctx.eval::<(), _>(src);
            });
        }
    }

    // Housekeeping evals (serialize/collect) must not be cut by the
    // no-progress bound — an interrupt here returned an empty DOM and lost
    // the page's text (roblox 363 -> 51 in m10). The bound is for runaway
    // page code, not for our own final reads. A budget-exhausted run left
    // the stop in the past and re-cut every read that polled the interrupt
    // (ST-11 U2: heavier guards made the final serialize trip the poll
    // counter -> 28KB DOM -> 0); housekeeping now gets a forward stop.
    stop_at.set(std::cmp::max(
        hard_deadline,
        Instant::now() + Duration::from_secs(10),
    ));

    let html: String = ctx
        .with(|ctx| ctx.eval::<String, _>("__DCI.dcSerialize()"))
        .unwrap_or_default();
    if std::env::var("DONCORE_TRACE").is_ok() {
        let tlen: i64 = ctx.with(|ctx| ctx.eval::<i64, _>(TEXT_LEN_EXPR).unwrap_or(-1));
        let _ = std::fs::write("scratch/dom-last.html", &html);
        eprintln!("[dom] serialize_len={} body_text={tlen}", html.len());
    }
    let nav_html: String = ctx
        .with(|ctx| {
            ctx.eval::<String, _>("typeof __DCI.t.navHtml === 'string' ? __DCI.t.navHtml : ''")
        })
        .unwrap_or_default();
    let nav_url: String = ctx
        .with(|ctx| {
            ctx.eval::<String, _>("typeof __DCI.t.navUrl === 'string' ? __DCI.t.navUrl : ''")
        })
        .unwrap_or_default();
    let nav_status: u16 = ctx
        .with(|ctx| {
            ctx.eval::<i32, _>("typeof __DCI.t.navStatus === 'number' ? __DCI.t.navStatus|0 : 0")
        })
        .unwrap_or(0) as u16;
    let nav_req: Option<(String, String)> = ctx
        .with(|ctx| {
            ctx.eval::<String, _>(
                "(__DCI.t.navReq && typeof __DCI.t.navReq === 'object') ? __DCI.t.navReq.kind + ' ' + __DCI.t.navReq.url : ''",
            )
        })
        .ok()
        .and_then(|s| s.split_once(' ').map(|(k, u)| (k.to_string(), u.to_string())));
    if std::env::var("DONCORE_TRACE").is_ok() {
        let _ = ctx.with(|ctx| ctx.eval::<(), _>("__DCI.log('FORENSICS ' + __DCI.forensics() + ' MK=' + (typeof MkvrI5) + ' AW=' + (typeof AwUcP3))"));
    }
    let console_out = console.borrow().clone();
    let harvest_out = harvest.borrow().clone();
    let dbg: String = ctx
        .with(|ctx| {
            ctx.eval::<String, _>(
                "JSON.stringify({opt: Object.keys(window._cf_chl_opt||{}).slice(0,44), wf: Object.keys(window).filter(function(k){return typeof window[k]==='function'}).slice(0,60), txcand: [\"postMessage\",\"fetch\",\"console\",\"addEventListener\",\"dispatchEvent\",\"_cf_chl_opt\",\"_cf_chl_state\"].map(function(k){return k+\"=\"+typeof window[k]})})",
            )
        })
        .unwrap_or_default();
    if std::env::var("DONCORE_DEBUG").is_ok() {
        report.errors.push(format!("debug {dbg}"));
    }
    for j in job_errors.borrow().iter().take(8) {
        report.errors.push(j.clone());
    }
    report.console = console_out;
    // bind console errors to their source region via the last dynamic script
    {
        let body = last_dyn.borrow().clone();
        if !body.is_empty() {
            for c in report.console.iter_mut() {
                if c.contains("<input>:") && !c.contains("[src:") {
                    let snip = err_snippet(c, &body);
                    if !snip.is_empty() {
                        c.push_str(" [src:");
                        c.push_str(&snip);
                        c.push(']');
                    }
                }
            }
        }
    }
    report.net_calls = harvest_out.len();
    report.timed_out = timed_out.get();
    report.nav_html = nav_html;
    report.nav_url = nav_url;
    report.nav_status = nav_status;
    report.nav_req = nav_req;
    report.stalled = challenge_stalled;
    report.stalled_ms = stalled_ms;
    report.idle_ms = idle_stop_ms;
    report.bound_stop = bound_stop;
    Ok((html, report, harvest_out))
}

fn pump_jobs(rt: &Runtime, cap: usize, errs: &Rc<RefCell<Vec<String>>>) {
    for _ in 0..cap {
        match rt.execute_pending_job() {
            Ok(true) => continue,
            Ok(false) => break,
            Err(je) => {
                let msg: String = je.0.with(|c| {
                    let v = c.catch();
                    if c.globals().set("__tmpjoberr", v).is_err() {
                        return "job exception (unsettable)".to_string();
                    }
                    c.eval::<String, _>(
                        "String((__tmpjoberr && __tmpjoberr.message) ? (__tmpjoberr.message + ' @@ ' + String(__tmpjoberr.stack || '').split('\\n').slice(1, 3).join(' >> ')) : __tmpjoberr)",
                    )
                    .unwrap_or_else(|_| "job exception (unstringifiable)".into())
                });
                let mut v = errs.borrow_mut();
                if v.len() < 20 {
                    v.push(format!("job: {msg}"));
                }
                break;
            }
        }
    }
}

fn qjs_exc_string(raw: *mut qjs::JSContext) -> String {
    unsafe {
        let e = qjs::JS_GetException(raw);
        let mut len: qjs::size_t = 0;
        let cs = qjs::JS_ToCStringLen2(raw, &mut len, e, false);
        let s = if cs.is_null() {
            "unprintable exception".to_string()
        } else {
            std::ffi::CStr::from_ptr(cs).to_string_lossy().into_owned()
        };
        if !cs.is_null() {
            qjs::JS_FreeCString(raw, cs);
        }
        qjs::JS_FreeValue(raw, e);
        s
    }
}

/// ST-11 U6: compile one boot asset to QuickJS bytecode (tools/gen_qbc.py).
/// Compile-only global script → JS_WriteObject; the loader counterpart lives in
/// try_eval_qbc. One process per asset run; values freed with js_free.
pub fn write_asset_qbc(in_path: &str, out_path: &str) -> Result<(), String> {
    let src = std::fs::read(in_path).map_err(|e| format!("read {in_path}: {e}"))?;
    let rt = Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    let ctx = Context::full(&rt).map_err(|e| format!("context: {e}"))?;
    let bytes = ctx.with(|ctx| -> Result<Vec<u8>, String> {
        let raw = ctx.as_raw().as_ptr();
        let fname = std::ffi::CString::new("<asset>").unwrap();
        let flags = (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as std::ffi::c_int;
        unsafe {
            let fun = qjs::JS_Eval(
                raw,
                src.as_ptr() as *const std::ffi::c_char,
                src.len() as u64,
                fname.as_ptr(),
                flags,
            );
            if qjs::JS_HasException(raw) {
                let e = qjs::JS_GetException(raw);
                qjs::JS_FreeValue(raw, e);
                return Err("compile failed (exception pending)".into());
            }
            let mut sz: u64 = 0;
            let buf = qjs::JS_WriteObject(
                raw,
                &mut sz,
                fun,
                qjs::JS_WRITE_OBJ_BYTECODE as std::ffi::c_int,
            );
            qjs::JS_FreeValue(raw, fun);
            if buf.is_null() || sz == 0 {
                return Err("write_object returned no buffer".into());
            }
            // self-check: read the blob back + eval it against a stubbed __DCI.
            // Blob-integrity probe; env-dependent assets (prelude) may fail here
            // with a *missing-global* error, which is expected — a wrong error
            // (e.g. ReferenceError on a name the source never uses) is not.
            {
                let _ = ctx.eval::<(), _>("let __DCI = {};");
                let raw2 = ctx.as_raw().as_ptr();
                let f2 =
                    qjs::JS_ReadObject(raw2, buf, sz, qjs::JS_READ_OBJ_BYTECODE as std::ffi::c_int);
                if qjs::JS_HasException(raw2) {
                    eprintln!("qbc-selfcheck: read failed: {}", qjs_exc_string(raw2));
                } else {
                    let r2 = qjs::JS_EvalFunction(raw2, f2);
                    if qjs::JS_HasException(raw2) {
                        eprintln!("qbc-selfcheck: eval failed: {}", qjs_exc_string(raw2));
                    } else {
                        eprintln!("qbc-selfcheck: ok");
                        qjs::JS_FreeValue(raw2, r2);
                    }
                }
            }
            let out = std::slice::from_raw_parts(buf, sz as usize).to_vec();
            qjs::js_free(raw, buf as *mut std::ffi::c_void);
            Ok(out)
        }
    })?;
    std::fs::write(out_path, &bytes).map_err(|e| format!("write {out_path}: {e}"))?;
    eprintln!("qbc: {in_path} -> {out_path} ({} bytes)", bytes.len());
    Ok(())
}

/// ST-11 U6: load a precompiled boot asset; on any failure fall back to the
/// source eval so a stale/mismatched blob can never kill boot.
fn eval_asset(ctx: &rquickjs::Ctx, src: &str, qbc: &[u8], name: &str) -> String {
    match try_eval_qbc(ctx, qbc) {
        Ok(()) => {
            if std::env::var("DONCORE_DEBUG").is_ok() {
                eprintln!("[qbc] {name}: ok");
            }
            String::new()
        }
        Err(e) => {
            eprintln!("[qbc] {name}: {e}; falling back to source eval");
            eval_catch(ctx, src)
        }
    }
}

fn try_eval_qbc(ctx: &rquickjs::Ctx, qbc: &[u8]) -> Result<(), String> {
    let raw = ctx.as_raw().as_ptr();
    unsafe {
        let fun = qjs::JS_ReadObject(
            raw,
            qbc.as_ptr(),
            qbc.len() as u64,
            qjs::JS_READ_OBJ_BYTECODE as std::ffi::c_int,
        );
        if qjs::JS_HasException(raw) {
            return Err(format!("read failed: {}", qjs_exc_string(raw)));
        }
        // JS_EvalFunction instantiates + consumes the function-bytecode object
        // (quickjs JS_EvalFunctionInternal → JS_CallFree); free the result.
        let r = qjs::JS_EvalFunction(raw, fun);
        if qjs::JS_HasException(raw) {
            return Err(format!("eval failed: {}", qjs_exc_string(raw)));
        }
        qjs::JS_FreeValue(raw, r);
    }
    Ok(())
}

fn eval_catch(ctx: &rquickjs::Ctx, src: &str) -> String {
    // Script-tag semantics: evaluate as GLOBAL code so top-level
    // let/const/class land in the persistent global lexical environment
    // exactly like a real <script>. The old (0, eval)(src) wrapper was
    // indirect eval: lexical declarations died with each script (sannysoft's
    // fpCollect/fpScanner libs are const-top-level and vanished between
    // scripts). var/function still bind globally, errors surface in the same
    // "message @@ stack" shape.
    let mut opts = rquickjs::context::EvalOptions::default();
    opts.global = true;
    opts.strict = false;
    opts.filename = Some("<input>".to_string());
    if std::env::var("DONCORE_TRACE").is_ok() {
        let head: String = src.chars().take(160).collect();
        eprintln!("[eval] len={} head={:?}", src.len(), head);
    }
    match ctx.eval_with_options::<(), _>(src.as_bytes().to_vec(), opts) {
        Ok(_) => String::new(),
        Err(_) => catch_string(ctx),
    }
}

/// Stringify the pending QuickJS exception in the engine's canonical
/// "message @@ stack" shape (shared by script eval and module eval).
fn catch_string(ctx: &rquickjs::Ctx) -> String {
    // Surface the real error instead of "Exception generated by QuickJS".
    let v = ctx.catch();
    if ctx.globals().set("__tmperr", v).is_err() {
        return "engine: (unsettable exception)".to_string();
    }
    ctx.eval::<String, _>(
        "String((__tmperr && __tmperr.message) ? (__tmperr.message + ' @@ ' + String(__tmperr.stack || '')) : String(__tmperr))",
    )
    .unwrap_or_else(|_| "engine: (unstringifiable exception)".to_string())
}

/// `<script type="module">`: real ESM semantics (strict, module scope, live
/// bindings) through the runtime's module loader.
fn eval_module_catch(ctx: &rquickjs::Ctx, name: &str, src: &str) -> String {
    let declared = match rquickjs::Module::declare(
        ctx.clone(),
        name.as_bytes().to_vec(),
        src.as_bytes().to_vec(),
    ) {
        Ok(m) => m,
        Err(_) => return catch_string(ctx),
    };
    match declared.eval() {
        Ok(_) => String::new(),
        Err(_) => catch_string(ctx),
    }
}

/// ST-8 unit 7 (dyn3): fire the static script element's `load`/`error` event
/// (the prelude matches it by the URL the element carries, consumed in
/// document order).
fn sc_done_js(ctx: &rquickjs::Ctx, key: &str, ok: bool) {
    let _ = ctx.eval::<(), _>(format!("__DCI.scDone({}, {ok})", js_string(key)));
}

/// ESM resolver (minimal): relative/absolute specifiers resolve as URLs
/// against the importer's module name; bare specifiers (import maps) are
/// out of scope and error like a browser without an import map.
struct ModResolver;

impl rquickjs::loader::Resolver for ModResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &rquickjs::Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if name.starts_with("http://") || name.starts_with("https://") || name.starts_with("data:")
        {
            return Ok(name.to_string());
        }
        if name.starts_with('/') || name.starts_with("./") || name.starts_with("../") {
            let b = url::Url::parse(base).map_err(|_| {
                rquickjs::Error::new_from_js_message(
                    "module",
                    "URL",
                    format!("invalid module base {base}"),
                )
            })?;
            return b.join(name).map(|u| u.to_string()).map_err(|_| {
                rquickjs::Error::new_from_js_message(
                    "module",
                    "URL",
                    format!("cannot resolve specifier {name}"),
                )
            });
        }
        Err(rquickjs::Error::new_from_js_message(
            "module",
            "URL",
            format!("Failed to resolve module specifier \"{name}\" (import maps unsupported)"),
        ))
    }
}

/// ESM loader (minimal): fetches module sources through the wire; records
/// fetch failures in `netfail` (ST-8 unit 7 dyn3: a module element fires
/// `error` only when some fetch failed — self or import).
struct ModLoader {
    wire: Wire,
    page_url: String,
    netfail: Rc<Cell<bool>>,
}

impl rquickjs::loader::Loader for ModLoader {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<rquickjs::Module<'js, rquickjs::module::Declared>> {
        let r = self.wire.fetch(WireReq {
            method: "GET".into(),
            url: name.to_string(),
            kind: ReqKind::Script,
            extra_headers: vec![("referer".into(), self.page_url.clone())],
            body: None,
            ..Default::default()
        });
        let body = match r {
            Ok(r) if (200..300).contains(&r.status) => r.body,
            Ok(r) => {
                self.netfail.set(true);
                return Err(rquickjs::Error::new_from_js_message(
                    "module",
                    "Network",
                    format!("Failed to load module {name}: HTTP {}", r.status),
                ));
            }
            Err(e) => {
                self.netfail.set(true);
                return Err(rquickjs::Error::new_from_js_message(
                    "module",
                    "Network",
                    format!("Failed to load module {name}: {e}"),
                ));
            }
        };
        rquickjs::Module::declare(ctx.clone(), name.as_bytes().to_vec(), body.into_bytes())
    }
}

/// DEBUG-ONLY: rewrite `obj[ident](...)` to record `ident` in `__LASTCALL`
/// before the call, so a dynamic-name failure names itself.
fn patch_calltracer(src: &str) -> String {
    let cs: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len() + 1024);
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < cs.len() {
        if let Some(q) = quote {
            out.push(cs[i]);
            if cs[i] == '\\' && i + 1 < cs.len() {
                out.push(cs[i + 1]);
                i += 2;
                continue;
            }
            if cs[i] == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if cs[i] == '\'' || cs[i] == '"' || cs[i] == '`' {
            quote = Some(cs[i]);
            out.push(cs[i]);
            i += 1;
            continue;
        }
        if cs[i] == '[' {
            let mut j = i + 1;
            let start = j;
            let is_id_start = |c: char| c.is_ascii_alphabetic() || c == '_' || c == '$';
            if j < cs.len() && is_id_start(cs[j]) {
                j += 1;
                while j < cs.len()
                    && (cs[j].is_ascii_alphanumeric() || cs[j] == '_' || cs[j] == '$')
                {
                    j += 1;
                }
                if j + 1 < cs.len() && cs[j] == ']' && cs[j + 1] == '(' {
                    let ident: String = cs[start..j].iter().collect();
                    // walk back for a simple object expression: ident(.ident)*
                    let mut k = i;
                    while k > 0 {
                        let c = cs[k - 1];
                        if c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.' {
                            k -= 1;
                        } else {
                            break;
                        }
                    }
                    let obj: String = cs[k..i].iter().collect();
                    let obj_ok = !obj.is_empty()
                        && obj.chars().next().map(|c| is_id_start(c)).unwrap_or(false);
                    if obj_ok {
                        out.push_str("(globalThis.__CT2(");
                        out.push_str(&ident);
                        out.push(',');
                        out.push_str(&obj);
                        out.push('[');
                        out.push_str(&ident);
                        out.push_str("],");
                        out.push_str(&obj);
                        out.push_str("),");
                        out.push_str(&obj);
                        out.push('[');
                        out.push_str(&ident);
                        out.push(']');
                        out.push(')');
                    } else {
                        out.push('[');
                        out.push_str(&ident);
                        out.push(']');
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// Pull the first `:line:col` from a QuickJS stack and return that source
/// region of `body` so a failure points at real code, not a mystery.
fn err_snippet(msg: &str, body: &str) -> String {
    let b: Vec<char> = msg.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == ':' {
            let mut j = i + 1;
            let mut n1 = String::new();
            while j < b.len() && b[j].is_ascii_digit() {
                n1.push(b[j]);
                j += 1;
            }
            if !n1.is_empty() && j < b.len() && b[j] == ':' {
                let mut k = j + 1;
                let mut n2 = String::new();
                while k < b.len() && b[k].is_ascii_digit() {
                    n2.push(b[k]);
                    k += 1;
                }
                if !n2.is_empty() {
                    let l: usize = n1.parse().unwrap_or(0);
                    let c: usize = n2.parse().unwrap_or(0);
                    if l > 0 && c > 0 {
                        if let Some(line) = body.lines().nth(l - 1) {
                            let lc: Vec<char> = line.chars().collect();
                            if c <= lc.len() + 1 {
                                let a = c.saturating_sub(300);
                                let z = (c + 300).min(lc.len());
                                let left: String = lc[a..c.saturating_sub(1)].iter().collect();
                                let mark: String =
                                    lc.get(c - 1).map(|ch| ch.to_string()).unwrap_or_default();
                                let right: String = lc[c.min(lc.len())..z].iter().collect();
                                return format!("[{l}:{c}] ...{left}«{mark}»{right}...");
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    String::new()
}

fn epoch_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

pub fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
