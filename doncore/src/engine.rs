//! The engine: one fetch, end to end — wire → verdict → runtime → bounce.

use std::time::Instant;

use ego_tree::NodeRef;
use scraper::node::Node;

use crate::identity::Identity;
use crate::rt::{self, ScriptSlot};
use crate::state::{Cache, Check, HostStore};
use crate::wire::{ReqKind, Wire, WireReq, WireResp};

pub struct Engine {
    pub wire: Wire,
    pub identity_json: String,
    pub cache: Option<Cache>,
    pub hosts: Option<HostStore>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsMode {
    Auto,
    Always,
    Never,
}

impl JsMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

pub struct Outcome {
    pub status: u16,
    /// The final response's headers — the response whose body became the
    /// output — so a product reads content-type etc. without a second call.
    pub headers: Vec<(String, String)>,
    pub url: String,
    pub label: String,
    pub html: String,
    pub text: String,
    pub scripts_total: usize,
    pub scripts_failed: usize,
    pub net_calls: usize,
    pub jar_len: usize,
    pub ms_total: u128,
    pub phases: Vec<(String, u128)>,
    pub errors: Vec<String>,
    pub net_log: Vec<rt::NetRecord>,
    pub cache: Option<CacheOutcome>,
    /// Wave W / D5 (2026-10-24): the largest inline JSON state object the
    /// document carried (`ytInitialData`, `__NEXT_DATA__`, Nuxt, IG's
    /// `_sharedData` — the class is "big JSON literal assigned to a global"),
    /// surfaced when the final content stayed thin. The content a
    /// JS-dependent page ships in its own HTML is content; the extractor
    /// walks it instead of failing "did not hydrate".
    pub state: Option<StateJson>,
}

/// A harvested inline state object: its global name and the JSON text.
#[derive(Clone, Debug)]
pub struct StateJson {
    pub name: String,
    pub json: String,
}

/// How the state plane served this document. Typed: consumers map it;
/// nobody parses the human note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheState {
    /// Cache disabled for this engine.
    Off,
    /// No usable entry; the wire answer stands as fetched.
    Miss,
    /// Served from a provably fresh stored entry — no request made.
    Fresh,
    /// A stored entry was revalidated (304) and the stored body merged.
    Revalidated,
    /// The wire answer was written to the cache ("stored") or the cache
    /// declined it ("not cacheable").
    Stored,
}

/// The cache's own account of a document fetch: the typed state plus the
/// human detail the CLI prints.
#[derive(Clone, Debug)]
pub struct CacheOutcome {
    pub state: CacheState,
    pub detail: String,
}

impl std::fmt::Display for CacheOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// The product's knobs for one document fetch. Defaults keep the runtime's
/// path byte-identical; the swap threads its lanes and session flows here.
#[derive(Clone, Debug, Default)]
pub struct FetchOpts {
    /// Egress override: the document GETs ride this http(s) proxy. (The JS
    /// phase's subresource fetches follow once their plumbing lands.)
    pub proxy: Option<String>,
    /// Suppress the jar on the document GETs (cold probes: send none,
    /// store none). The runtime path always rides the jar.
    pub no_cookies: bool,
    /// Extra request headers merged ahead of any conditionals (referer,
    /// persona accept-language, a legacy UA).
    pub extra_headers: Vec<(String, String)>,
    /// Per-hop redirect consult for the document GETs: the product's
    /// rules/SSRF gate runs BEFORE each hop target is dialed.
    pub hop_consult: Option<crate::wire::HopConsult>,
}

struct DocFetch {
    resp: WireResp,
    cache: Option<CacheOutcome>,
    phase: String,
}

impl Engine {
    pub fn new(
        wire: Wire,
        ident: Identity,
        cache: Option<Cache>,
        hosts: Option<HostStore>,
    ) -> Self {
        let identity_json = ident.json();
        Self {
            wire,
            identity_json,
            cache,
            hosts,
        }
    }

    /// The document fetch through the state plane (phase 1: THE CACHE).
    /// A stored body is served only when provably fresh; stale entries
    /// revalidate (304 → serve, 200 → replace, other → evict + serve);
    /// a strict-CAS mismatch falls back to one unconditional refetch.
    /// Any cache trouble is a miss — the cache cannot fail a fetch.
    fn fetch_doc(
        &self,
        url: &str,
        opts: &FetchOpts,
        extra: Vec<(String, String)>,
    ) -> Result<DocFetch, String> {
        // The product's headers lead; a caller's own extras follow.
        let extra: Vec<(String, String)> =
            opts.extra_headers.iter().cloned().chain(extra).collect();
        // The stored representation belongs to the exact request context
        // (v4 semantics): URL, egress route with credentials, and the
        // cookie/header identity actually sent — never the URL alone.
        let cookie = if opts.no_cookies {
            None
        } else {
            let jar = self.wire.jar.lock().unwrap_or_else(|p| p.into_inner());
            jar.header_for(url)
        };
        let key = crate::state::representation_key(
            url,
            opts.proxy.as_deref(),
            opts.no_cookies,
            &extra,
            cookie.as_deref(),
        );
        if let Some(c) = &self.cache {
            match c.check(&key) {
                Check::Fresh(hit) => {
                    return Ok(DocFetch {
                        resp: WireResp {
                            status: 200,
                            final_url: hit.final_url,
                            headers: hit.headers,
                            body: hit.body,
                            ms: 0,
                            ..Default::default()
                        },
                        cache: Some(CacheOutcome {
                            state: CacheState::Fresh,
                            detail: format!("fresh (age {}s)", hit.age),
                        }),
                        phase: "cache".into(),
                    });
                }
                Check::Revalidate { conds, snap } => {
                    let mut hdrs = extra.clone();
                    hdrs.extend(conds);
                    let r = self.wire.fetch(WireReq {
                        method: "GET".into(),
                        url: url.to_string(),
                        kind: ReqKind::Document,
                        extra_headers: hdrs,
                        body: None,
                        no_cookies: opts.no_cookies,
                        proxy: opts.proxy.clone(),
                        hop_consult: opts.hop_consult.clone(),
                        ..Default::default()
                    })?;
                    // Only a 304 to the entry's own URL can validate it:
                    // one reached through a redirect hop answers for the
                    // hop's target, which has no snapshot here (v4
                    // semantics: conditionals are first-hop only). Compare
                    // canonicalized: the wire normalizes a bare authority
                    // (`https://host` -> `https://host/`), so a byte compare
                    // would reject the entry's own revalidation.
                    if r.status == 304 && same_url(&r.final_url, url) {
                        if let Some((body, headers)) = c.commit(&snap, &r.headers, r.ms) {
                            return Ok(DocFetch {
                                resp: WireResp {
                                    status: 200,
                                    final_url: snap.final_url.clone(),
                                    headers,
                                    body,
                                    ms: r.ms,
                                    ..Default::default()
                                },
                                cache: Some(CacheOutcome {
                                    state: CacheState::Revalidated,
                                    detail: format!("revalidated (304, {}ms)", r.ms),
                                }),
                                phase: "wire".into(),
                            });
                        }
                        let r2 = self.wire.fetch(WireReq {
                            method: "GET".into(),
                            url: url.to_string(),
                            kind: ReqKind::Document,
                            extra_headers: extra,
                            body: None,
                            no_cookies: opts.no_cookies,
                            proxy: opts.proxy.clone(),
                            hop_consult: opts.hop_consult.clone(),
                            ..Default::default()
                        })?;
                        if r2.status == 200 {
                            c.store(&key, url, &r2);
                        }
                        return Ok(DocFetch {
                            resp: r2,
                            cache: Some(CacheOutcome {
                                state: CacheState::Stored,
                                detail: "refetched (validator mismatch)".into(),
                            }),
                            phase: "wire".into(),
                        });
                    }
                    if r.status == 200 {
                        c.store(&key, url, &r);
                        let detail = format!("updated ({}ms)", r.ms);
                        return Ok(DocFetch {
                            resp: r,
                            cache: Some(CacheOutcome {
                                state: CacheState::Stored,
                                detail,
                            }),
                            phase: "wire".into(),
                        });
                    }
                    c.remove(&key);
                    return Ok(DocFetch {
                        resp: r,
                        cache: Some(CacheOutcome {
                            state: CacheState::Miss,
                            detail: "reval-refused".into(),
                        }),
                        phase: "wire".into(),
                    });
                }
                Check::Miss => {}
            }
        }
        let r = self.wire.fetch(WireReq {
            method: "GET".into(),
            url: url.to_string(),
            kind: ReqKind::Document,
            extra_headers: extra,
            body: None,
            no_cookies: opts.no_cookies,
            proxy: opts.proxy.clone(),
            hop_consult: opts.hop_consult.clone(),
            ..Default::default()
        })?;
        if let Some(c) = &self.cache {
            if r.status == 200 {
                let stored = c.store(&key, url, &r);
                let (state, detail) = if stored {
                    (
                        CacheState::Stored,
                        format!("stored ({} KB)", r.body.len() / 1024),
                    )
                } else {
                    (CacheState::Miss, "miss (not cacheable)".to_string())
                };
                return Ok(DocFetch {
                    resp: r,
                    cache: Some(CacheOutcome { state, detail }),
                    phase: "wire".into(),
                });
            }
            c.remove(&key);
        }
        let net = if self.cache.is_some() {
            Some(CacheOutcome {
                state: CacheState::Miss,
                detail: "miss".to_string(),
            })
        } else {
            None
        };
        Ok(DocFetch {
            resp: r,
            cache: net,
            phase: "wire".into(),
        })
    }

    pub fn fetch(&self, url: &str, js: JsMode, budget_ms: u64) -> Result<Outcome, String> {
        self.fetch_with(url, js, budget_ms, &FetchOpts::default())
    }

    /// As `fetch`, with the product's per-fetch options (egress lane, jar
    /// suppression, caller headers) threaded into the document GETs.
    pub fn fetch_with(
        &self,
        url: &str,
        js: JsMode,
        budget_ms: u64,
        opts: &FetchOpts,
    ) -> Result<Outcome, String> {
        let t0 = Instant::now();
        let mut phases: Vec<(String, u128)> = Vec::new();
        let host_key = host_key_of(url);

        // 1. wire (through the state plane's cache when enabled)
        let jar_rev_start = self
            .wire
            .jar
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .revision();
        let docf = self
            .fetch_doc(url, opts, Vec::new())
            .map_err(|e| format!("wire: {e}"))?;
        let jar_gained = self
            .wire
            .jar
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .revision()
            != jar_rev_start;
        let mut cache: Option<CacheOutcome> = docf.cache;
        let doc = docf.resp;
        let mut final_headers = doc.headers.clone();
        phases.push((docf.phase, doc.ms));
        let mut status = doc.status;
        let final_url = doc.final_url.clone();
        let mut current_html = doc.body.clone();
        if std::env::var("DONCORE_DUMP_RAW").is_ok() {
            let _ = std::fs::write("scratch/raw-page.html", &current_html);
        }
        let mut label = "wire".to_string();
        // the typing block below the challenge cycle needs the budget verdict at
        // the same scope level as the label (mirrors round_stalled/stalled_ms)
        let mut budget_cut = false;
        // Wave W: a runtime round cut by budget or stall produced UNKNOWN
        // evidence, never no-gain evidence (see note_runtime).
        let mut runtime_cut = false;

        let challenge = challenge_detected(&current_html);
        let html_like = looks_html(&current_html);
        let scripts = if html_like {
            collect_scripts(&current_html, &final_url)
        } else {
            Vec::new()
        };
        let crude_len = if html_like {
            crude_text(&current_html).len()
        } else {
            usize::MAX
        };
        let mut scripts_total = scripts.len();
        let mut scripts_failed = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut net_log: Vec<rt::NetRecord> = Vec::new();

        // Loose vendor markers, computed once at function scope (the run
        // gate reads them inside Auto; the typing block reads them after).
        let hint = js_challenge_hint(&current_html);
        let run_js = match js {
            JsMode::Never => false,
            JsMode::Always => html_like,
            JsMode::Auto => {
                // Content-first, and the content gate is the law: substantial
                // wire text means the page is delivered. The runtime is the
                // escalation for THIN challenge pages and THIN shells only.
                // Sweep #5: loose vendor strings (`challenge-platform`,
                // `datadome`) and even strict ones ("just a moment" inside
                // ticketmaster's own content, site-wide `px-captcha`) forced
                // 4-12s JS runs on pages that had already delivered — the
                // gate stops that class cold (nike was the same law, part 15).
                let mut run = html_like && crude_len < 600 && (challenge || !scripts.is_empty());
                // phase 3 (PER-HOST LEARNING): the measured skip — a page
                // shape whose runtime runs never gained content stays off
                // the runtime while the evidence is fresh. Challenges are
                // never skipped, and `--js always` bypasses this entirely.
                // Wave W / D3: a loose challenge hint (challenge-platform,
                // cf-chl, turnstile, datadome…) on a THIN page means the page
                // may be an unsolved challenge — the sweep-#5 law only bars
                // loose strings from forcing RUNS on content pages; here they
                // bar the LEARNED SKIP, or failed solves become permanent
                // thin serves (quora's live 200-variant).
                if run && !challenge && !hint && (200..400).contains(&status) {
                    if let (Some(h), Some(hk)) = (&self.hosts, &host_key) {
                        let key = shape_key(crude_len, scripts.len());
                        if let Some(runs) = h.skip_ok(hk, &key, budget_ms) {
                            run = false;
                            errors.push(format!(
                                "runtime skipped: no content gain in {runs} learned runtime runs on this page shape (--js always overrides)"
                            ));
                            h.note_skipped(hk, &key);
                        }
                    }
                }
                if std::env::var("DONCORE_TRACE").is_ok() {
                    eprintln!(
                        "[js] auto html_like={html_like} challenge={challenge} hint={hint} crude={crude_len} scripts={} run={run}",
                        scripts.len()
                    );
                }
                run
            }
        };

        // phase 3: the learning slots — thin non-challenge runtime runs are
        // the skip's evidence; a challenge run's cookie delta is the
        // clearance-TTL observation.
        let thin_ctx = if run_js
            && !challenge
            && !hint
            && (200..400).contains(&status)
            && html_like
            && crude_len < 600
            && !scripts.is_empty()
        {
            Some((shape_key(crude_len, scripts.len()), crude_len))
        } else {
            None
        };
        let mut jar_snap_before: Vec<(String, String, String)> = Vec::new();

        if run_js && (200..400).contains(&status) || (run_js && challenge) {
            let jar_before = self.wire.jar.lock().unwrap().revision();
            if challenge {
                let g = self.wire.jar.lock().unwrap_or_else(|p| p.into_inner());
                jar_snap_before = g
                    .cookies
                    .iter()
                    .map(|c| (c.name.clone(), c.domain.clone(), c.value.clone()))
                    .collect();
            }
            // The challenge cycle: run the document; when the PAGE navigates
            // (location.reload/assign/href — the CF interstitial reloads
            // itself mid-flow), fetch the target and run THAT document, the
            // way a browser re-runs the challenge on the reloaded page.
            // Bounded: a browser can loop forever; we take 3 nav rounds and
            // then fall through to the bounce.
            let mut page_html = current_html.clone();
            let mut page_url = final_url.clone();
            let mut round = 0u32;
            let mut round_stalled = false;
            let mut stalled_ms: u128 = 0;
            loop {
                let t = Instant::now();
                // One structural deadline for the WHOLE fetch: nav rounds
                // (widget-class sites can reload into the same challenge)
                // share the budget instead of each getting a fresh one.
                let remaining_ms = budget_ms.saturating_sub(t0.elapsed().as_millis() as u64);
                if round > 0 && remaining_ms < 1500 {
                    errors.push("nav budget exhausted".into());
                    break;
                }
                let tree = html_to_tree_json(&page_html);
                // ST-11 U4: pages carry far more than 40 scripts in the wild
                // (fuzz: a 300-script page lost every tail script + the counter;
                // commercial sites run 50-300). 512 keeps the drain budget-bounded
                // while losing nothing real.
                let mut cyc_scripts = collect_scripts(&page_html, &page_url);
                if cyc_scripts.len() > 512 {
                    cyc_scripts.truncate(512);
                    errors.push("script list truncated at 512".into());
                }
                let (dom_html, report, net) = match rt::run_document(
                    &self.identity_json,
                    &page_url,
                    &tree,
                    &cyc_scripts,
                    &self.wire,
                    remaining_ms,
                    challenge,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        phases.push(("runtime-error".into(), t.elapsed().as_millis()));
                        errors.push(format!("runtime: {e}"));
                        break;
                    }
                };
                phases.push((
                    if round == 0 {
                        "runtime".to_string()
                    } else {
                        format!("runtime:nav{round}")
                    },
                    t.elapsed().as_millis(),
                ));
                if round == 0 {
                    scripts_total = report.scripts_total;
                    scripts_failed = report.scripts_failed;
                } else {
                    scripts_total += report.scripts_total;
                    scripts_failed += report.scripts_failed;
                }
                errors.extend(report.errors.iter().cloned());
                let con_take: usize = if std::env::var("DONCORE_TRACE").is_ok() {
                    400
                } else {
                    80
                };
                let con_skip = report.console.len().saturating_sub(con_take);
                for c in report.console.iter().skip(con_skip) {
                    errors.push(format!("console: {c}"));
                }
                net_log = net;
                label = "wire+js".into();
                round_stalled = report.stalled;
                stalled_ms = report.stalled_ms;
                budget_cut = report.timed_out;
                if report.timed_out || report.stalled {
                    runtime_cut = true;
                }
                if report.idle_ms > 0 && !challenge {
                    errors.push(format!(
                        "stopped: page idle — no content/net/cookie change for {}ms (typed no-progress stop)",
                        report.idle_ms
                    ));
                }
                if report.timed_out {
                    if challenge {
                        errors.push("runtime budget exhausted (interrupt fired)".into());
                    } else {
                        errors.push(
                            "stopped: non-challenge runtime bound reached (interrupt fired, no further content)"
                                .into(),
                        );
                    }
                }
                if report.bound_stop && !challenge {
                    errors.push(
                        "stopped: non-challenge runtime bound reached (no further content)".into(),
                    );
                }
                if !dom_html.is_empty() {
                    current_html = dom_html;
                }
                // (a) page-initiated navigation (reload/assign/replace/href)
                if let Some((kind, nav_url)) = &report.nav_req {
                    round += 1;
                    if round > 3 {
                        errors.push("nav rounds exhausted".into());
                        break;
                    }
                    phases.push((format!("nav{round}:{kind}"), 0));
                    match self.fetch_doc(nav_url, opts, vec![("referer".into(), page_url.clone())])
                    {
                        Ok(df) => {
                            cache = df.cache.clone();
                            let d = df.resp;
                            if (200..400).contains(&d.status) && !challenge_detected(&d.body) {
                                current_html = d.body;
                                final_headers = d.headers;
                                label = "wire+js+nav".into();
                                break;
                            }
                            // still a wall (or non-2xx): the reloaded page
                            // runs the challenge again
                            if std::env::var("DONCORE_DEBUG").is_ok() {
                                let _ = std::fs::write(
                                    format!("scratch/nav-round{round}.html"),
                                    &d.body,
                                );
                            }
                            page_html = d.body;
                            page_url = d.final_url;
                            continue;
                        }
                        Err(e) => {
                            errors.push(format!("nav: {e}"));
                            break;
                        }
                    }
                }
                // (b) form-submit navigation (Reddit-class interstitials)
                if !report.nav_html.is_empty() {
                    phases.push(("nav-form".into(), 0));
                    current_html = report.nav_html.clone();
                    label = "wire+js+challenge-form".into();
                    // one extra pass when the landing page is still a shell
                    let txt = crude_text(&current_html);
                    let sc2 = if looks_html(&current_html) {
                        collect_scripts(&current_html, &report.nav_url)
                    } else {
                        Vec::new()
                    };
                    if !sc2.is_empty() && txt.len() < 400 {
                        let t3 = Instant::now();
                        let tree2 = html_to_tree_json(&current_html);
                        let remaining2 = budget_ms
                            .saturating_sub(t0.elapsed().as_millis() as u64)
                            .max(1500);
                        match rt::run_document(
                            &self.identity_json,
                            &report.nav_url,
                            &tree2,
                            &sc2,
                            &self.wire,
                            remaining2,
                            challenge,
                        ) {
                            Ok((dom2, rep2, net2)) => {
                                phases.push(("runtime2".into(), t3.elapsed().as_millis()));
                                scripts_total += rep2.scripts_total;
                                scripts_failed += rep2.scripts_failed;
                                errors.extend(rep2.errors.iter().cloned());
                                net_log.extend(net2);
                                if crude_text(&dom2).len() > txt.len() {
                                    current_html = dom2;
                                    label = "wire+js+challenge-form+render".into();
                                }
                            }
                            Err(e) => errors.push(format!("runtime2: {e}")),
                        }
                    }
                }
                // (c) no navigation: settle for the bounce below
                break;
            }
            // bounce: clearance earned but the page never navigated
            let jar_after = self.wire.jar.lock().unwrap().revision();
            if challenge && jar_after != jar_before && !current_html.is_empty() {
                let t2 = Instant::now();
                if let Ok(df) = self.fetch_doc(url, opts, Vec::new()) {
                    cache = df.cache;
                    let b2 = df.resp;
                    phases.push(("bounce".into(), t2.elapsed().as_millis()));
                    if std::env::var("DONCORE_DEBUG").is_ok() {
                        let _ = std::fs::write(
                            "scratch/bounce.html",
                            format!("status {}\n{}", b2.status, b2.body),
                        );
                    }
                    if (200..400).contains(&b2.status) && !challenge_detected(&b2.body) {
                        status = b2.status;
                        current_html = b2.body;
                        final_headers = b2.headers;
                        label = "wire+js+clearance".into();
                    }
                }
            }
            // Stall-typed wall: the challenge round stopped advancing (stall
            // watchdog) and nothing upgraded the label — honest fast failure
            // instead of a budget burn (draw #1: 20s on a heartbeat-only site).
            if challenge && round_stalled && label == "wire+js" {
                label = "walled".into();
                errors.push(format!(
                    "walled: challenge did not advance — stopped after {stalled_ms}ms of silence (heartbeat-only), no clearance"
                ));
            }
        } else if run_js {
            errors.push(format!("js skipped: status {status}"));
        } else if challenge && !current_html.is_empty() && jar_gained {
            // A strict challenge that isn't shaped like a document gets no
            // runtime cycle, but it did earn a cookie: one warm re-GET,
            // browser-style (the reload carries the new cookie). A 304 to
            // it surfaces as-is — the contract layer rejects an
            // unsolicited conditional-less 304 (never an empty wall).
            let t2 = Instant::now();
            if let Ok(df) = self.fetch_doc(url, opts, Vec::new()) {
                cache = df.cache;
                let b2 = df.resp;
                phases.push(("bounce".into(), t2.elapsed().as_millis()));
                if (200..400).contains(&b2.status) && !challenge_detected(&b2.body) {
                    status = b2.status;
                    current_html = b2.body;
                    final_headers = b2.headers;
                    label = "wire+clearance".into();
                }
            }
        }

        // ST-7 queue item (5), measured 2026-10-20: a wall the engine rendered
        // must never surface as ordinary content, and a challenge the budget cut
        // must not read as success. The worst fetch outcome in this project is a
        // success-looking page that is actually the challenge.
        // Wave W / D3 (2026-10-24): the wall_evidence scan runs on the FINAL
        // html — but a challenge widget RE-RENDERS the DOM, and the re-rendered
        // shell can lose every textual marker ("Just a moment…" lived in the
        // wire document, not the widget's DOM). The engine KNOWS whether it ran
        // a challenge and whether that challenge cleared: challenge && thin
        // output && no clearance label IS a wall, markers or no markers. The
        // pre-JS wire document is what gets served then — it is the honest
        // wall evidence, and the product's detector reads its real markers.
        let mut wire_html = doc.body.clone();
        if label != "wire+js+clearance" && label != "wire+js+nav" {
            let challenge_uncleared =
                challenge && label.starts_with("wire+js") && crude_text(&current_html).len() < 600;
            if let Some(kind) = wall_evidence(&current_html) {
                if label != "walled" {
                    let prev = label.clone();
                    label = "walled".into();
                    errors.push(format!(
                        "walled: {kind} surfaced as the final content (previous label {prev}) — the request was not served, no clearance"
                    ));
                }
            } else if challenge && budget_cut && label == "wire+js" {
                label = "walled".into();
                errors.push(
                    "walled: challenge still in progress when the runtime budget ran out — no clearance"
                        .into(),
                );
            } else if challenge_uncleared && label != "walled" {
                let prev = label.clone();
                label = "walled".into();
                errors.push(format!(
                    "walled: challenge ran ({prev}) but never cleared — no clearance cookie, no navigation, no content"
                ));
            } else if !challenge
                && hint
                && label.starts_with("wire+js")
                && crude_text(&current_html).len() < 600
                && label != "walled"
            {
                // Wave W / D3: the loose-marker challenge shell — the runtime
                // ran, nothing hydrated, and the thin body carries vendor
                // challenge markers (challenge-platform / cf-chl / turnstile /
                // datadome). An unsolved wall, typed honestly.
                label = "walled".into();
                errors.push(
                    "walled: vendor challenge shell never produced content (challenge markers present, no clearance)".into(),
                );
            }
            if label == "walled"
                && (challenge || hint)
                && crude_text(&current_html).len() < 600
                && (challenge_detected(&wire_html) || wall_evidence(&wire_html).is_some())
            {
                // Serve the wall the WIRE fetched, not the widget's re-render:
                // the original document carries the vendor markers the
                // product's detector (and the agent) need to see.
                std::mem::swap(&mut current_html, &mut wire_html);
            }
        }

        // phase 3 (PER-HOST LEARNING): record what this fetch taught the
        // state plane — outcome, challenge/clearance, runtime evidence.
        if let (Some(h), Some(hk)) = (&self.hosts, &host_key) {
            if let Some((key, before)) = &thin_ctx {
                let gain = crude_text(&current_html).len().saturating_sub(*before) as u64;
                h.note_runtime(hk, key, gain, budget_ms, !runtime_cut);
            }
            if challenge {
                h.note_challenge(hk);
            }
            if label == "wire+js+clearance" {
                let now = crate::state::now_secs();
                let ttl = {
                    let g = self.wire.jar.lock().unwrap_or_else(|p| p.into_inner());
                    g.cookies
                        .iter()
                        .filter(|c| {
                            match jar_snap_before
                                .iter()
                                .find(|(n, d, _)| *n == c.name && *d == c.domain)
                            {
                                Some((_, _, v)) => *v != c.value,
                                None => true,
                            }
                        })
                        .filter_map(|c| c.expires)
                        .map(|e| e.saturating_sub(now))
                        .max()
                };
                h.note_cleared(hk, ttl);
            }
            h.note_fetch(hk, &label, status);
        }

        let text = crude_text(&current_html);
        // Wave W / D5: thin output + a big inline state object in the WIRE
        // document = the content shipped with the page. Harvest it (never
        // for a wall outcome — a challenge shell's state is not content).
        let state = if text.len() < 600 && label != "walled" {
            harvest_state_json(&doc.body)
        } else {
            None
        };
        let jar_len = self.wire.jar.lock().unwrap().len();
        Ok(Outcome {
            state,
            status,
            headers: final_headers,
            url: final_url,
            label,
            html: current_html,
            text,
            scripts_total,
            scripts_failed,
            net_calls: net_log.len(),
            jar_len,
            ms_total: t0.elapsed().as_millis(),
            phases,
            errors,
            net_log,
            cache,
        })
    }
}

// ───────────────────────────── helpers ──────────────────────────────

/// Wall markers: strings the interstitials carry. Measured caveat (sweep
/// #5): they also occur INSIDE some real pages ("just a moment" in
/// ticketmaster's own content, site-wide `px-captcha` script references) —
/// so they only argue the solve path on THIN pages; the content gate in the
/// Auto decision is what keeps content pages safe.
/// Wall vocabulary for the TYPING path only (never a run gate — the Auto gate
/// keeps its own measured list). Chrome-truth classes captured across ST-6:
/// the CF managed/JS wall, the vendor block pages, and the human-verification
/// interstitials (reCAPTCHA v2 as reddit serves it, hCaptcha, turnstile
/// embeds). Applied only to THIN documents (crude text < 600), the same
/// discipline the Auto gate uses, so a long real page that merely mentions
/// these words is never typed as a wall.
fn wall_evidence(html: &str) -> Option<&'static str> {
    if html.is_empty() || crude_text(html).len() >= 600 {
        return None;
    }
    let l = html.to_lowercase();
    const TEXT: [(&str, &str); 9] = [
        (
            "prove your humanity",
            "human-verification text (\"prove your humanity\")",
        ),
        (
            "confirm you are human",
            "human-verification text (\"confirm you are human\")",
        ),
        (
            "prove you are human",
            "human-verification text (\"prove you are human\")",
        ),
        (
            "performing security verification",
            "vendor security-verification page",
        ),
        ("checking your browser", "browser-check interstitial"),
        ("just a moment", "browser-check interstitial"),
        (
            "enable javascript and cookies to continue",
            "browser-check interstitial",
        ),
        ("attention required", "vendor block page"),
        ("verify you are human", "human-verification text"),
    ];
    for (needle, kind) in TEXT {
        if l.contains(needle) {
            return Some(kind);
        }
    }
    const EMBED: [(&str, &str); 4] = [
        ("recaptcha/api.js", "reCAPTCHA challenge embed"),
        ("g-recaptcha", "reCAPTCHA challenge embed"),
        ("hcaptcha.com/1/api.js", "hCaptcha challenge embed"),
        (
            "challenges.cloudflare.com/turnstile",
            "turnstile challenge embed",
        ),
    ];
    for (needle, kind) in EMBED {
        if l.contains(needle) {
            return Some(kind);
        }
    }
    if challenge_detected(html) {
        return Some("challenge page (registered wall markers)");
    }
    None
}

pub fn challenge_detected(html: &str) -> bool {
    let l = html.to_lowercase();
    const WALL: [&str; 10] = [
        "just a moment",
        "checking your browser",
        "verify you are human",
        "are you a robot",
        "enable javascript and cookies to continue",
        "_cf_chl_opt",
        "cf_chl_",
        "px-captcha",
        // DataDome block/captcha class (measured on g2's live 403, 2026-10-18).
        // Reachable only on THIN pages (crude_len < 600 in the Auto gate), so
        // these stay safe per the sweep-#5 lesson.
        "captcha-delivery.com",
        "please enable js and disable any ad blocker",
    ];
    WALL.iter().any(|m| l.contains(m))
}

// W2 NOTE (2026-10-24, moonshot): a `scripts/precursor` marker was tried here
// and REVERTED — measured same-hour: medium.com still walled with it (the CF
// precursor stage needs a real flow, not a classification flip), while
// stackoverflow — a proven-clearable row — degraded from `wire+js+clearance`
// to `wire+js+challenge-form`. The sweep-#5 lesson stands: loose vendor strings
// never gate a decision alone. The precursor flow is its own unit.

/// Loose markers: vendor strings that appear site-wide on real pages (CF's
/// challenge-platform beacon, datadome/turnstile embeds). Diagnostic only
/// (the `[js]` trace) — measured in sweep #5 wasting multi-second JS runs
/// when they triggered; they must never gate a decision by themselves.
fn js_challenge_hint(html: &str) -> bool {
    let l = html.to_lowercase();
    const HINTS: [&str; 6] = [
        "cf-chl",
        "challenge-platform",
        "ddm-",
        "datadome",
        "captcha-delivery",
        "turnstile",
    ];
    HINTS.iter().any(|m| l.contains(m))
}

fn looks_html(body: &str) -> bool {
    let head = &body[..floor_char_boundary(body, body.len().min(4096))].to_lowercase();
    head.contains("<html")
        || head.contains("<!doctype html")
        || head.contains("<head")
        || head.contains("<body")
}

/// Collect script slots in document order from raw HTML.
pub fn collect_scripts(html: &str, base_url: &str) -> Vec<ScriptSlot> {
    let mut out = Vec::new();
    // ASCII-only fold: same byte length, so `lower` offsets index `html`
    // exactly (full fold can change lengths and skew every slice below).
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut pos = 0usize;
    while let Some(off) = lower[pos..].find("<script") {
        let start = pos + off;
        let Some(gt_off) = lower[start..].find('>') else {
            break;
        };
        let gt = start + gt_off;
        let tag = &html[start..gt];
        let after = &lower[gt..];
        let Some(close_off) = after.find("</script") else {
            break;
        };
        let close = gt + close_off;
        let content = &html[gt + 1..close];
        pos = close + 9;

        let attrs = parse_attrs(tag);
        let ty = attrs
            .iter()
            .find(|(k, _)| k == "type")
            .map(|(_, v)| v.to_lowercase())
            .unwrap_or_default();
        let is_code = ty.is_empty()
            || ty.contains("javascript")
            || ty.contains("ecmascript")
            || ty == "module";
        if !is_code {
            continue;
        }
        let is_module = ty == "module";
        if let Some((_, src)) = attrs.iter().find(|(k, _)| k == "src") {
            if src.is_empty() {
                continue;
            }
            let resolved = url::Url::parse(base_url)
                .ok()
                .and_then(|b| b.join(src).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| src.to_string());
            out.push(if is_module {
                ScriptSlot::ExternalModule(resolved)
            } else {
                ScriptSlot::External(resolved)
            });
        } else {
            if content.trim().is_empty() {
                continue;
            }
            if content.len() > 2_000_000 {
                continue;
            }
            out.push(if is_module {
                ScriptSlot::InlineModule(content.to_string())
            } else {
                ScriptSlot::Inline(content.to_string())
            });
        }
        let _ = bytes;
    }
    out
}

fn parse_attrs(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // tag looks like `<script src="..." type="...">` or `<script>`
    let body = &tag["<script".len()..];
    let re = regex_lite(body);
    out.extend(re);
    out
}

/// minimal attribute scanner (name, value)
fn regex_lite(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < b.len()
            && !(b[i] as char).is_whitespace()
            && b[i] != b'='
            && b[i] != b'>'
            && b[i] != b'/'
        {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name = s[name_start..i].to_lowercase();
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && (b[i] as char).is_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                value = s[vs..i].to_string();
                if i < b.len() {
                    i += 1;
                }
            } else {
                let vs = i;
                while i < b.len() && !(b[i] as char).is_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = s[vs..i].to_string();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// HTML → simple tree JSON for the runtime DOM.
pub fn html_to_tree_json(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    let root: NodeRef<'_, Node> = doc.tree.root();
    let mut candidate: Option<serde_json::Value> = None;
    let mut doctype: Option<serde_json::Value> = None;
    for child in root.children() {
        if let Node::Doctype(d) = child.value() {
            doctype = Some(serde_json::json!({
                "name": d.name.to_string(),
                "publicId": d.public_id.to_string(),
                "systemId": d.system_id.to_string()
            }));
            continue;
        }
        if let Some(v) = node_to_json(child) {
            if v.get("t").and_then(|t| t.as_str()) == Some("element") {
                candidate = Some(v);
                break;
            }
        }
    }
    // the doctype rides the root node (the runtime puts it at
    // document.childNodes[0], the only place a browser keeps it)
    let mut tree = candidate.unwrap_or_else(
        || serde_json::json!({"t":"element","tag":"html","attrs":{},"children":[]}),
    );
    if let Some(dt) = doctype {
        if let Some(obj) = tree.as_object_mut() {
            obj.insert("doctype".to_string(), dt);
        }
    }
    tree.to_string()
}

fn node_to_json(n: NodeRef<'_, Node>) -> Option<serde_json::Value> {
    match n.value() {
        Node::Element(el) => {
            let tag = el.name.local.to_string();
            // ST-11 U2b-5 (SVG league): the element's namespace rides the tree,
            // and prefixed attributes keep their qualifier (scraper's local-only
            // iterator drops xlink:/xml:).
            let ns_uri: &str = &el.name.ns;
            let ns = match ns_uri {
                "http://www.w3.org/2000/svg" => "svg",
                "http://www.w3.org/1998/Math/MathML" => "math",
                _ => "html",
            };
            let mut attrs = serde_json::Map::new();
            for (name, value) in el.attrs.iter() {
                let key = match &name.prefix {
                    Some(p) => format!("{}:{}", &**p, name.local),
                    None => name.local.to_string(),
                };
                attrs.insert(key, serde_json::Value::String(value.to_string()));
            }
            let mut children = Vec::new();
            for c in n.children() {
                if let Some(v) = node_to_json(c) {
                    children.push(v);
                }
            }
            let mut node = serde_json::json!({
                "t": "element", "tag": tag, "attrs": attrs, "children": children
            });
            if ns != "html" {
                if let Some(obj) = node.as_object_mut() {
                    obj.insert("ns".to_string(), serde_json::Value::String(ns.to_string()));
                }
            }
            Some(node)
        }
        Node::Text(t) => {
            let s = t.to_string();
            if s.is_empty() {
                None
            } else {
                Some(serde_json::json!({"t": "text", "text": s}))
            }
        }
        // Comments are real nodes in a browser tree (Chrome keeps source
        // comments); the runtime materializes them as Comment instances.
        Node::Comment(c) => Some(serde_json::json!({"t": "comment", "text": c.to_string()})),
        // The doctype is a node too: document.childNodes[0] in Chrome.
        Node::Doctype(d) => Some(serde_json::json!({
            "t": "doctype",
            "name": d.name.to_string(),
            "publicId": d.public_id.to_string(),
            "systemId": d.system_id.to_string()
        })),
        _ => None,
    }
}

/// Clamp a byte index down to the nearest char boundary — safe slicing for
/// string heads/tails. Multibyte text must never panic a fetch (draw #2:
/// tarifer.ru, a 'к' crossing byte 4096).
pub fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The learned-store key for a fetch: the URL's authority, lowercase, with
/// an explicit port when the URL carries one.
fn host_key_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// The page-shape bucket the runtime evidence is kept under: crude-text size
/// (b0 <100, b1 <300, b2 <600) × script count (c1 ≤3, c2 ≤15, c3 more).
/// Callers only classify the thin class (crude < 600).
fn shape_key(crude_len: usize, scripts: usize) -> String {
    let b = if crude_len < 100 {
        0
    } else if crude_len < 300 {
        1
    } else {
        2
    };
    let c = if scripts <= 3 {
        1
    } else if scripts <= 15 {
        2
    } else {
        3
    };
    format!("b{b}c{c}")
}

/// Crude text extraction (v0.1 stand-in for DonSift at integration).
pub fn crude_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    // ASCII-only fold: byte-length preserving, so positions found in `lower`
    // are valid char boundaries in `html` (full fold skews them: wikipedia
    // Main_Page panicked here on the 2026-10-20 content).
    let lower = html.to_ascii_lowercase();
    let mut pos = 0usize;
    while pos < html.len() {
        if let Some(off) = lower[pos..].find('<') {
            let tag_start = pos + off;
            out.push_str(&html[pos..tag_start]);
            let rest = &lower[tag_start..];
            if rest.starts_with("<script") || rest.starts_with("<style") {
                let end_tag = if rest.starts_with("<script") {
                    "</script"
                } else {
                    "</style"
                };
                if let Some(eoff) = lower[tag_start..].find(end_tag) {
                    let e = tag_start + eoff;
                    let g = lower[e..]
                        .find('>')
                        .map(|x| e + x + 1)
                        .unwrap_or(lower.len());
                    pos = g;
                    continue;
                } else {
                    break;
                }
            }
            let g = rest.find('>').map(|x| tag_start + x + 1);
            match g {
                Some(g) => pos = g,
                None => break,
            }
        } else {
            out.push_str(&html[pos..]);
            break;
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let mut collapsed = String::with_capacity(decoded.len());
    let mut last_ws = false;
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !last_ws {
                collapsed.push(' ');
            }
            last_ws = true;
        } else {
            collapsed.push(c);
            last_ws = false;
        }
    }
    collapsed.trim().to_string()
}

/// Wave W / D5: find the largest inline JSON state object in a document.
/// The class: an inline `<script>` assigning a big JSON literal to a global
/// (`ytInitialData = {...}`, `window._sharedData = {...}`,
/// `var __NEXT_DATA__ = {...}`). Measured: youtube.com ships its whole
/// homepage this way (962KB document, 163 visible DOM chars) and fails
/// "did not hydrate" with the content in hand. A whole-script JSON body
/// (type="application/json" style) is harvested too.
const STATE_MIN: usize = 8 * 1024;
const STATE_MAX: usize = 2 * 1024 * 1024;

pub fn harvest_state_json(html: &str) -> Option<StateJson> {
    let mut best: Option<StateJson> = None;
    let mut pos = 0usize;
    let lower = html.to_ascii_lowercase();
    while let Some(off) = lower[pos..].find("<script") {
        let start = pos + off;
        let Some(gt_off) = lower[start..].find('>') else {
            break;
        };
        let gt = start + gt_off;
        let Some(close_off) = lower[gt..].find("</script") else {
            break;
        };
        let close = gt + close_off;
        let body = &html[gt + 1..close];
        pos = close + 9;
        if body.len() < STATE_MIN {
            continue;
        }
        let trimmed = body.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Some(st) = take_json(trimmed, STATE_MAX) {
                if st.len() >= STATE_MIN {
                    best = max_state(
                        best,
                        StateJson {
                            name: "inline".into(),
                            json: st,
                        },
                    );
                }
            }
            continue;
        }
        let mut search = 0usize;
        while let Some(eq_off) = body[search..].find('=') {
            let eq = search + eq_off;
            let before = body[..eq].trim_end();
            let lhs_start = before
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '.'))
                .map(|i| i + 1)
                .unwrap_or(0);
            let lhs = &before[lhs_start..];
            let name = lhs.rsplit('.').next().unwrap_or("");
            let after = body[eq + 1..].trim_start();
            let valid_name = name.len() >= 3
                && name.len() <= 48
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$');
            if valid_name && (after.starts_with('{') || after.starts_with('[')) {
                if let Some(st) = take_json(after, STATE_MAX) {
                    if st.len() >= STATE_MIN {
                        best = max_state(
                            best,
                            StateJson {
                                name: name.to_string(),
                                json: st,
                            },
                        );
                    }
                }
            }
            search = eq + 1;
            if search >= body.len() {
                break;
            }
        }
    }
    best
}

fn max_state(cur: Option<StateJson>, cand: StateJson) -> Option<StateJson> {
    match cur {
        Some(c) if c.json.len() >= cand.json.len() => Some(c),
        _ => Some(cand),
    }
}

/// Extract one JSON value from the start of `s` (object or array), honoring
/// string escapes so braces inside strings cannot end it early. None when no
/// value starts here or it exceeds `cap`.
fn take_json(s: &str, cap: usize) -> Option<String> {
    let b = s.as_bytes();
    if b.is_empty() || (b[0] != b'{' && b[0] != b'[') {
        return None;
    }
    let open = b[0];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &c) in b.iter().enumerate() {
        if i > cap {
            return None;
        }
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            _ if c == open => depth += 1,
            _ if c == close => {
                depth -= 1;
                if depth == 0 {
                    let slice = &s[..=i];
                    return serde_json::from_str::<serde_json::Value>(slice)
                        .ok()
                        .map(|_| slice.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// True when two strings name the same URL once canonicalized. The wire
/// normalizes a bare authority (`https://host` → `https://host/`, host case,
/// default ports), so a byte compare would misjudge the entry's own
/// revalidation as a 304 reached through a redirect hop. A 304 that really
/// arrived through a hop lands on a different URL and stays rejected.
fn same_url(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod wave_w_state_tests {
    use super::*;

    #[test]
    fn wave_w_harvests_the_global_assignment_class() {
        let big = format!("{{\"videos\": [\"{}\"]}}", "title ".repeat(3000));
        let html = format!(
            "<html><head><script>var config = 1;</script><script>window.ytInitialData = {big};</script></head><body><div id=root></div></body></html>"
        );
        let st = harvest_state_json(&html).expect("state must harvest");
        assert_eq!(st.name, "ytInitialData");
        assert!(st.json.len() > 8000);
        let html2 = "<html><body><script>window.__INIT = {\"a\": 1};</script></body></html>";
        assert!(harvest_state_json(html2).is_none());
    }
}
