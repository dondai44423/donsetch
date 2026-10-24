//! doncore — v0.1 dev CLI for the single-engine fetch system.
//!
//!   doncore info
//!   doncore fetch <url> [--js auto|always|never] [--budget ms] [--dump path]
//!                        [--harvest] [--full]

use doncore::engine::{floor_char_boundary, Engine, JsMode};
use doncore::state::{Cache, HostStore, JarStore};
use doncore::{rt, state};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    match cmd {
        "info" => info(),
        "fetch" => fetch(&args[2..]),
        "qbc" => {
            if args.len() < 4 {
                eprintln!("usage: doncore qbc <in.js> <out.qbc>");
                std::process::exit(2);
            }
            if let Err(e) = rt::write_asset_qbc(&args[2], &args[3]) {
                eprintln!("qbc: {e}");
                std::process::exit(1);
            }
            // one-shot generator: exit before Runtime teardown — the self-check
            // leaks a few GC objects on purpose, no need to unwind for a CLI.
            std::process::exit(0);
        }
        "cache" => cache_cmd(&args[2..]),
        "jar" => jar_cmd(&args[2..]),
        "hosts" => hosts_cmd(&args[2..]),
        _ => {
            println!("doncore v0.1 — single-engine fetch system (dev CLI)");
            println!("  doncore info");
            println!("  doncore fetch <url> [--js auto|always|never] [--budget ms] [--dump PATH] [--harvest] [--full] [--persona 149|156] [--no-cache] [--no-jar] [--no-hosts]");
            println!("  doncore cache stats|prune|clear");
            println!("  doncore jar stats|clear");
            println!("  doncore hosts stats|clear");
        }
    }
}

/// `--resolve host:port:ip` → the wire's pin map. An IPv6 address is
/// bracketed (`host:port:[::1]`) — this box's DNS answers IPv6 first, so the
/// bare form would be the common case, not the exotic one. Bad entries are
/// named and skipped (a typo must not silently change egress).
fn parse_resolves(resolves: &[String]) -> Vec<(String, std::net::SocketAddr)> {
    let mut map: Vec<(String, std::net::SocketAddr)> = Vec::new();
    for r in resolves {
        let bad = |why: &str| eprintln!("doncore: bad --resolve ({why}): {r}");
        let (host_port, ip) = if let Some(inner) = r.strip_suffix(']') {
            match inner.rsplit_once('[') {
                Some((hp, ip)) => (hp.strip_suffix(':').unwrap_or(hp), format!("[{ip}]")),
                None => {
                    bad("unclosed address");
                    continue;
                }
            }
        } else {
            match r.rsplit_once(':') {
                Some((hp, ip)) => (hp, ip.to_string()),
                None => {
                    bad("want host:port:ip");
                    continue;
                }
            }
        };
        let Some((host, port)) = host_port.rsplit_once(':') else {
            bad("want host:port:ip");
            continue;
        };
        match format!("{ip}:{port}").parse::<std::net::SocketAddr>() {
            Ok(addr) => map.push((host.to_string(), addr)),
            Err(_) => bad("addr"),
        }
    }
    map
}

fn info() {
    let (_, ident) = doncore::client::chrome("154", Vec::new());
    println!("UA: {}", ident.ua);
    println!("chrome major: {}", ident.chrome_major);
    println!("locale: {} langs: {:?}", ident.locale, ident.languages);
    println!(
        "tz: {} cores: {} mem: {}GB",
        ident.tz, ident.cores, ident.memory_gb
    );
    println!(
        "screen: {}x{} avail {}x{} outer_h {} dpr {}",
        ident.screen_w, ident.screen_h, ident.avail_w, ident.avail_h, ident.outer_h, ident.dpr
    );
    println!("gpu: {} / {}", ident.gpu_vendor, ident.gpu_renderer);
}

/// Exit flush: the jar's debounce must not swallow the last write.
fn flush_jar(engine: &Engine) {
    engine
        .wire
        .jar
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .persist_if_dirty(true);
}

/// Exit flush: the learned store's debounce must not swallow the last write.
fn flush_hosts(engine: &Engine) {
    if let Some(h) = &engine.hosts {
        h.persist_if_dirty(true);
    }
}

fn fetch(args: &[String]) {
    let mut url: Option<String> = None;
    let mut js = JsMode::Auto;
    let mut budget_ms = 2500u64;
    let mut dump: Option<String> = None;
    let mut harvest = false;
    let mut full = false;
    let mut persona = "156".to_string();
    let mut resolves: Vec<String> = Vec::new();
    let mut no_cache = false;
    let mut no_jar = false;
    let mut no_hosts = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--js" => {
                i += 1;
                js = args
                    .get(i)
                    .and_then(|s| JsMode::parse(s))
                    .unwrap_or(JsMode::Auto);
            }
            "--budget" => {
                i += 1;
                budget_ms = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(2500);
            }
            "--dump" => {
                i += 1;
                dump = args.get(i).cloned();
            }
            "--harvest" => harvest = true,
            "--full" => full = true,
            "--no-cache" => no_cache = true,
            "--no-jar" => no_jar = true,
            "--no-hosts" => no_hosts = true,
            "--persona" => {
                i += 1;
                if let Some(p) = args.get(i) {
                    persona = p.clone();
                }
            }
            "--resolve" => {
                i += 1;
                if let Some(r) = args.get(i) {
                    resolves.push(r.clone());
                }
            }
            other => {
                if url.is_none() && !other.starts_with("--") {
                    url = Some(other.to_string());
                }
            }
        }
        i += 1;
    }
    let Some(url) = url else {
        eprintln!("doncore fetch: missing URL");
        std::process::exit(2);
    };

    let engine = doncore::client::open(
        &persona,
        parse_resolves(&resolves),
        doncore::client::Features {
            cache: !no_cache,
            jar: !no_jar,
            hosts: !no_hosts,
        },
    );
    match engine.fetch(&url, js, budget_ms) {
        Ok(o) => {
            flush_jar(&engine);
            flush_hosts(&engine);
            println!("── doncore fetch ─────────────────────────────");
            println!("url:      {} (status {})", o.url, o.status);
            println!("label:    {}", o.label);
            println!(
                "cache:    {}",
                o.cache.as_ref().map(|c| c.detail.as_str()).unwrap_or("off")
            );
            println!(
                "phases:   {}",
                o.phases
                    .iter()
                    .map(|(n, ms)| format!("{n}={ms}ms"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            println!(
                "runtime:  scripts {}/{} ok ({} failed), net calls {}, jar {} cookies",
                o.scripts_total - o.scripts_failed,
                o.scripts_total,
                o.scripts_failed,
                o.net_calls,
                o.jar_len
            );
            if let Some(st) = &o.state {
                println!(
                    "state:    {} ({} KB harvested from inline JSON)",
                    st.name,
                    st.json.len() / 1024
                );
            }
            let ctype = o
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("-");
            println!("headers:  {} (content-type {})", o.headers.len(), ctype);
            println!("total:    {} ms", o.ms_total);
            if std::env::var("DONCORE_DEBUG").is_ok() {
                let jar = engine.wire.jar.lock().unwrap_or_else(|p| p.into_inner());
                for c in &jar.cookies {
                    let cut: String = c.value.chars().take(48).collect();
                    let exp = c
                        .expires
                        .map(|e| format!("expires in {}s", e.saturating_sub(state::now_secs())))
                        .unwrap_or_else(|| "session".to_string());
                    println!("cookie:   {} = {}... ({}, {})", c.name, cut, c.domain, exp);
                }
            }
            if !o.errors.is_empty() {
                let trace = std::env::var("DONCORE_TRACE").is_ok()
                    || std::env::var("DONCORE_DEBUG").is_ok();
                let show = if trace { o.errors.len() } else { 3 };
                println!("warnings: {} (last {show})", o.errors.len());
                let skip = o.errors.len().saturating_sub(show);
                for e in o.errors.iter().skip(skip) {
                    println!("  · {e}");
                }
            }
            if harvest && !o.net_log.is_empty() {
                println!("── harvest (first 12) ──");
                for r in o.net_log.iter().take(40) {
                    println!(
                        "  {} {} → {} {}B {}ms {}",
                        r.method, r.url, r.status, r.len, r.ms, r.ctype
                    );
                }
            }
            println!("── text ({} chars) ──", o.text.len());
            let show = if full {
                o.text.len()
            } else {
                o.text.len().min(1500)
            };
            println!("{}", &o.text[..floor_char_boundary(&o.text, show)]);
            if let Some(path) = dump {
                match std::fs::write(&path, o.html.as_bytes()) {
                    Ok(_) => println!("── html dumped to {path} ({} bytes) ──", o.html.len()),
                    Err(e) => eprintln!("dump failed: {e}"),
                }
            }
        }
        Err(e) => {
            flush_jar(&engine);
            flush_hosts(&engine);
            eprintln!("fetch failed: {e}");
            std::process::exit(1);
        }
    }
}

fn cache_cmd(args: &[String]) {
    let op = args.first().map(String::as_str).unwrap_or("stats");
    let Some(c) = Cache::open() else {
        println!("cache: off (DONCORE_CACHE=off or no usable root)");
        return;
    };
    match op {
        "stats" => println!("{}", c.stats()),
        "prune" => println!("{}", c.prune()),
        "clear" => {
            let n = c.clear();
            println!("cache: cleared {n} files");
        }
        other => {
            eprintln!("doncore cache: unknown op {other} (want stats|prune|clear)");
            std::process::exit(2);
        }
    }
}

fn jar_cmd(args: &[String]) {
    let op = args.first().map(String::as_str).unwrap_or("stats");
    let Some(st) = JarStore::open() else {
        println!("jar: off (DONCORE_JAR=off or no usable root)");
        return;
    };
    match op {
        "stats" => println!("{}", st.stats()),
        "clear" => {
            if st.clear() {
                println!("jar: cleared (file removed)");
            } else {
                println!("jar: nothing to clear");
            }
        }
        other => {
            eprintln!("doncore jar: unknown op {other} (want stats|clear)");
            std::process::exit(2);
        }
    }
}

fn hosts_cmd(args: &[String]) {
    let op = args.first().map(String::as_str).unwrap_or("stats");
    let Some(h) = HostStore::open() else {
        println!("hosts: off (DONCORE_HOSTS=off or no usable root)");
        return;
    };
    match op {
        "stats" => println!("{}", h.stats()),
        "clear" => {
            if h.clear() {
                println!("hosts: cleared (file removed)");
            } else {
                println!("hosts: nothing to clear");
            }
        }
        other => {
            eprintln!("doncore hosts: unknown op {other} (want stats|clear)");
            std::process::exit(2);
        }
    }
}
