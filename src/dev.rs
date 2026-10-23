//! Dev/internal commands (`donsetch dev ...`) : extraction
//! debugging, raw fetch probes, TLS fingerprint checks, ghost
//! browser operations. Not part of the agent surface.

use crate::egress::proxy;
use crate::extract;
use crate::fetch::client::Fetcher;
use crate::ghost;
use crate::profile::BrowserProfile;

pub async fn dispatch(args: &[String]) {
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    match cmd {
        "extract" => extract_cmd(&args[1..]).await,
        "probe" => probe_cmd(&args[1..]).await,
        "fingerprint" => fingerprint_cmd(&args[1..]).await,
        "resume-test" => resume_test_cmd(&args[1..]).await,
        "ghost" => ghost_cmd(&args[1..]).await,
        _ => {
            crate::diag!("dev subcommands: extract | probe | fingerprint | ghost | resume-test");
            crate::diag!();
            crate::diag!("  extract --input <file>     Extract from a local HTML/PDF file");
            crate::diag!("  probe <url> [proxy] [dump]  Raw fetch with diagnostics");
            crate::diag!("  fingerprint [url]            TLS/HTTP fingerprint check");
            crate::diag!("  resume-test <url>            3x fetch to test connection reuse");
            crate::diag!("  ghost <solve|render|shot> <url>  Ghost browser ops");
        }
    }
}

async fn extract_cmd(args: &[String]) {
    let mut input_file: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut opts = extract::ExtractOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--focus" => {
                i += 1;
                opts.focus = args.get(i).cloned();
            }
            "--max" => {
                i += 1;
                opts.max_chars = args.get(i).and_then(|s| s.parse().ok());
            }
            "--offset" => {
                i += 1;
                opts.offset = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--selector" => {
                i += 1;
                opts.selector = args.get(i).cloned();
            }
            "--links" => {
                opts.include_links = true;
            }
            "--media" => {
                opts.include_media = true;
            }
            "--input" => {
                i += 1;
                input_file = args.get(i).cloned();
            }
            "--url" => {
                i += 1;
                base_url = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }
    let Some(path) = input_file else {
        crate::diag!(
            "usage: donsetch dev extract --input <file> [--url base] [--focus q] [--max n] [--offset n] [--selector css] [--links] [--media]"
        );
        return;
    };
    let body = std::fs::read(&path).expect("read input");
    let sniff_ct = if body.starts_with(b"%PDF-") {
        "application/pdf"
    } else {
        "text/html"
    };
    let t0 = std::time::Instant::now();
    let ex = match extract::extract(
        &body,
        sniff_ct,
        base_url.as_deref().unwrap_or("https://local/"),
        &opts,
    ) {
        Ok(e) => e,
        Err(e) => {
            crate::diag!("error: {e}");
            std::process::exit(1);
        }
    };
    crate::diag!(
        "--- extract={:.1}ms blocks={}/{} chars={}/{}",
        t0.elapsed().as_secs_f64() * 1000.0,
        ex.blocks_shown,
        ex.blocks_total,
        ex.markdown.len(),
        ex.total_chars,
    );
    print!("{}", ex.markdown);
}

async fn probe_cmd(args: &[String]) {
    let u = args.first().map(String::as_str).unwrap_or("");
    let px = args.get(1).and_then(|s| proxy::Proxy::parse(s).ok());
    let f = Fetcher::new(BrowserProfile::host_default()).expect("fetcher");
    match f.fetch_once_via(u, &[], px.as_ref(), false, None).await {
        Ok(out) => {
            println!(
                "status={} alpn={} bytes={} verdict={:?} t={:.2}s",
                out.status,
                out.alpn,
                out.body.len(),
                out.verdict,
                out.elapsed.as_secs_f64()
            );
            if let Some(p) = args.get(2) {
                std::fs::write(p, &out.body).ok();
                crate::diag!("dumped -> {p}");
            }
        }
        Err(e) => println!("error: {e}"),
    }
}

async fn fingerprint_cmd(args: &[String]) {
    let url = args
        .first()
        .cloned()
        .unwrap_or_else(|| "https://tls.peet.ws/api/all".into());
    let fetcher = Fetcher::new(BrowserProfile::host_default()).expect("fetcher init");
    match fetcher.fetch(&url).await {
        Ok(out) => {
            println!(
                "status: {} alpn: {} elapsed: {:?}",
                out.status, out.alpn, out.elapsed
            );
            println!("{}", String::from_utf8_lossy(&out.body));
        }
        Err(e) => {
            crate::diag!("error: {e}");
            std::process::exit(1);
        }
    }
}

async fn resume_test_cmd(args: &[String]) {
    let url = args.first().expect("usage: donsetch dev resume-test <url>");
    let fetcher = Fetcher::new(BrowserProfile::host_default()).expect("fetcher init");
    for i in 1..=3 {
        match fetcher.fetch(url).await {
            Ok(out) => println!(
                "fetch{i}: status={} alpn={} elapsed={:?} bytes={} cache={:?} pooled={}",
                out.status,
                out.alpn,
                out.elapsed,
                out.body.len(),
                out.cache,
                out.used_pool
            ),
            Err(e) => {
                crate::diag!("fetch{i} error: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn ghost_cmd(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("help");
    let profile = BrowserProfile::host_default();
    match sub {
        "solve" => {
            let url = args.get(1).expect("usage: ghost solve <url>");
            let t0 = std::time::Instant::now();
            let mut g = ghost::Ghost::launch(&profile, None).await.expect("launch");
            crate::diag!("launched in {:.0}ms", t0.elapsed().as_secs_f64() * 1000.0);
            match ghost::ops::solve(&mut g, url, std::time::Duration::from_secs(30))
                .await
                .expect("solve")
            {
                ghost::ops::SolveOutcome::Solved(r) => {
                    println!("SOLVED in {:?}", r.took);
                    println!("clearance: {}", ghost::ops::has_clearance(&r.cookies));
                    println!("cookies: {}", r.cookies.len());
                    for c in &r.cookies {
                        println!("  {} (domain {})", c.name, c.domain);
                    }
                }
                ghost::ops::SolveOutcome::CaptchaWalled => {
                    println!("CAPTCHA-WALLED (honest dead end)")
                }
                ghost::ops::SolveOutcome::TimedOut => println!("TIMED OUT"),
            }
            g.kill().await;
        }
        "render" => {
            let url = args
                .get(1)
                .expect("usage: ghost render <url> [htmldump] [mddump]");
            let mut g = ghost::Ghost::launch(&profile, None).await.expect("launch");
            let html = ghost::ops::render(&mut g, url, std::time::Duration::from_secs(30))
                .await
                .expect("render");
            println!("rendered {} bytes", html.len());
            if let Some(p) = args.get(2) {
                std::fs::write(p, &html).expect("write htmldump");
                crate::diag!("dom -> {p}");
            }
            let ex = extract::extract(
                html.as_bytes(),
                extract::charset::GHOST_TEXT_CT,
                url,
                &extract::ExtractOptions::default(),
            )
            .expect("extract");
            if let Some(p) = args.get(3) {
                std::fs::write(p, &ex.markdown).expect("write mddump");
                crate::diag!("markdown -> {p}");
            }
            print!("{}", &ex.markdown[..ex.markdown.len().min(2000)]);
            crate::diag!(
                "--- thin={} kind={:?} blocks={}",
                ex.thin,
                ex.content_kind,
                ex.blocks_total
            );
            g.kill().await;
        }
        "shot" => {
            let url = args.get(1).expect("usage: ghost shot <url> [path]");
            let path = args.get(2).cloned().unwrap_or_else(|| {
                crate::paths::screenshots_dir()
                    .join("ghost.png")
                    .to_string_lossy()
                    .into_owned()
            });
            let mut g = ghost::Ghost::launch(&profile, None).await.expect("launch");
            g.navigate(url).await.expect("nav");
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            g.screenshot(&path).await.expect("shot");
            println!("shot -> {path}");
            g.kill().await;
        }
        "selftest" => {
            let mut g = ghost::Ghost::launch(&profile, None).await.expect("launch");
            match ghost::ops::selftest(&mut g).await {
                Ok(j) => println!("{j}"),
                Err(e) => crate::diag!("selftest: {e}"),
            }
            g.kill().await;
        }
        "freeze-check" => {
            let mut g = ghost::Ghost::launch(&profile, None).await.expect("launch");
            let h1 = ghost::ops::render(
                &mut g,
                "https://example.com",
                std::time::Duration::from_secs(15),
            )
            .await
            .expect("render1");
            println!("render1: {}B", h1.len());
            g.freeze();
            println!("frozen");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let alive = g.thaw();
            println!("thawed alive={alive}");
            let h2 = ghost::ops::render(
                &mut g,
                "https://example.com",
                std::time::Duration::from_secs(15),
            )
            .await
            .expect("render2");
            println!("render2 after thaw: {}B", h2.len());
            g.kill().await;
        }
        _ => {
            crate::diag!("ghost subcommands: solve | render | shot | selftest | freeze-check")
        }
    }
}
