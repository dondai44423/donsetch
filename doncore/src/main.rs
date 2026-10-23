//! doncore — v0.1 dev CLI for the single-engine fetch system.
//!
//!   doncore info
//!   doncore fetch <url> [--js auto|always|never] [--budget ms] [--dump path]
//!                        [--harvest] [--full]

use doncore::{rt, state, wire};
use doncore::engine::{floor_char_boundary, Engine, JsMode};
use doncore::identity::Identity;
use doncore::state::{Cache, HostStore, JarStore};
use doncore::wire::Wire;

use wreq::IntoEmulation;
use wreq_util::{Emulation as UtilEmulation, Platform, Profile};

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
            println!("  doncore fetch <url> [--js auto|always|never] [--budget ms] [--dump PATH] [--harvest] [--full] [--persona 149|154] [--no-cache] [--no-jar] [--no-hosts]");
            println!("  doncore cache stats|prune|clear");
            println!("  doncore jar stats|clear");
            println!("  doncore hosts stats|clear");
        }
    }
}

// ── the engine claims the platform it actually runs on: one coherent story
// for the wire preset + the JS identity on Linux/macOS/Windows (DECIDED
// cross-platform, 2026-10-18). Per-OS paths compile on each target; mac/win
// runtime qualification owed (no host). Never hardcode this box for others.
fn host_platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        Platform::MacOS
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
}

// Fallback UAs only — the emulation preset supplies the real strings; these
// exist so a missing header never fabricates a wrong-platform UA.
fn default_ua() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
    }
    #[cfg(target_os = "macos")]
    {
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
    }
    #[cfg(target_os = "windows")]
    {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
    }
}

// sec-ch-ua-platform-version fallback per OS (kernel / product / NT version);
// the Phase-1 oracle replaces it with this machine's measured value.
fn default_platform_version() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "6.6.0"
    }
    #[cfg(target_os = "macos")]
    {
        "14.0.0"
    }
    #[cfg(target_os = "windows")]
    {
        "10.0.0"
    }
}

fn build_client(persona: &str, resolves: &[String]) -> (Wire, Identity) {
    let util_em = UtilEmulation::builder()
        .profile(Profile::Chrome149)
        .platform(host_platform())
        .build();
    let full = if persona == "154" {
        wire::tune_emulation(util_em.into_emulation())
    } else {
        util_em.into_emulation()
    };
    let ua = full
        .headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(default_ua())
        .to_string();
    let platform_ver = full
        .headers
        .get("sec-ch-ua-platform-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(default_platform_version())
        .trim_matches('"')
        .to_string();
    let ident = Identity::detect(&ua, &platform_ver);
    let mut resolve_map: Vec<(String, std::net::SocketAddr)> = Vec::new();
    for r in resolves {
        if let Some((host_port, ip)) = r.rsplit_once(':') {
            if let Some((host, port)) = host_port.rsplit_once(':') {
                match format!("{ip}:{port}").parse::<std::net::SocketAddr>() {
                    Ok(addr) => resolve_map.push((host.to_string(), addr)),
                    Err(_) => eprintln!("doncore: bad --resolve addr: {r}"),
                }
            } else {
                eprintln!("doncore: bad --resolve (want host:port:ip): {r}");
            }
        } else {
            eprintln!("doncore: bad --resolve (want host:port:ip): {r}");
        }
    }
    let wire = Wire::new(full, resolve_map);
    (wire, ident)
}

fn info() {
    let (_, ident) = build_client("154", &[]);
    println!("UA: {}", ident.ua);
    println!("chrome major: {}", ident.chrome_major);
    println!("locale: {} langs: {:?}", ident.locale, ident.languages);
    println!("tz: {} cores: {} mem: {}GB", ident.tz, ident.cores, ident.memory_gb);
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
    let mut persona = "154".to_string();
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
                budget_ms = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(2500);
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

    let (wire, ident) = build_client(&persona, &resolves);
    if !no_jar {
        if let Some(st) = JarStore::open() {
            wire.jar
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .attach_store(st);
        }
    }
    let cache = if no_cache { None } else { Cache::open() };
    let hosts = if no_hosts { None } else { HostStore::open() };
    let engine = Engine::new(wire, ident, cache, hosts);
    if let Some(c) = &engine.cache {
        c.prune_if_due();
    }
    match engine.fetch(&url, js, budget_ms) {
        Ok(o) => {
            flush_jar(&engine);
            flush_hosts(&engine);
            println!("── doncore fetch ─────────────────────────────");
            println!("url:      {} (status {})", o.url, o.status);
            println!("label:    {}", o.label);
            println!("cache:    {}", o.cache.as_deref().unwrap_or("off"));
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
                let trace = std::env::var("DONCORE_TRACE").is_ok() || std::env::var("DONCORE_DEBUG").is_ok();
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
            let show = if full { o.text.len() } else { o.text.len().min(1500) };
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
