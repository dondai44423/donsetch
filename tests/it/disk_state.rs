//! `state.no_disk_state` reaches the fetch engine: with it, a fetch leaves
//! no learned state or cached page under the cache root; without it, it does.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

fn page_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = "<html><head><title>T</title></head><body><main><h1>T</h1>\
                <p>A paragraph long enough to count as the content of this page, twice over.</p></main></body></html>";
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: max-age=60\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}/")
}

fn fetch_with(cache: &std::path::Path, no_disk_state: bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_donsetch"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("DONSETCH_")
            || k.to_string_lossy().starts_with("DONSEEK_")
        {
            cmd.env_remove(&k);
        }
    }
    cmd.args(["fetch", &page_server(), "--quiet"])
        .env("DONSETCH_NO_CONFIG_FILE", "1")
        .env("DONSETCH_ALLOW_PRIVATE_EGRESS", "1")
        .env("DONSETCH_CACHE_DIR", cache);
    if no_disk_state {
        cmd.env("DONSETCH_STATE__NO_DISK_STATE", "true");
    }
    let out = cmd.output().expect("cli runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn no_disk_state_keeps_the_fetch_engine_off_disk() {
    let root = std::env::temp_dir().join(format!("donsetch-diskstate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (off, on) = (root.join("off"), root.join("on"));
    std::fs::create_dir_all(&off).unwrap();
    std::fs::create_dir_all(&on).unwrap();

    fetch_with(&off, true);
    assert!(
        !off.join("v5").exists(),
        "v5 state written despite no_disk_state"
    );

    fetch_with(&on, false);
    assert!(
        on.join("v5").join("state.json").exists(),
        "v5 state not written by default"
    );
    let _ = std::fs::remove_dir_all(&root);
}
