//! stdio transport for the MCP server.
//!
//! Handles stdin/stdout I/O for the MCP JSON-RPC protocol.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use serde_json::Value;

use crate::mcp::server::{CancelMap, Daemon, handle};

/// Run the stdio MCP daemon until stdin closes.
/// Never returns Err on client garbage — only on fatal IO.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let daemon = Arc::new(Daemon::new().await?);
    let (tx, mut rx) = mpsc::channel::<String>(256);

    // Single writer: response lines can never interleave.
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            // A broken stdout (client died, pipe closed) must not be
            // swallowed: every later response would be silently
            // dropped while the daemon pretends to serve. Log the
            // real cause and stop — the client is gone.
            if let Err(e) = out.write_all(line.as_bytes()).await {
                eprintln!("[mcp] stdout write failed, shutting down: {e}");
                std::process::exit(1);
            }
            if let Err(e) = out.write_all(b"\n").await {
                eprintln!("[mcp] stdout write failed, shutting down: {e}");
                std::process::exit(1);
            }
            if let Err(e) = out.flush().await {
                eprintln!("[mcp] stdout flush failed, shutting down: {e}");
                std::process::exit(1);
            }
        }
    });

    // Cancellation registry: request-id → cancel sender. The MCP
    // client fires notifications/cancelled with a requestId; the
    // in-flight tool observes it (fetch/search abort via select,
    // crawl stops its workers gracefully and persists its resume
    // token before returning).
    let cancels: CancelMap = Arc::new(Mutex::new(std::collections::HashMap::new()));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // Cancellation notifications are handled inline — they must
        // reach the running tool NOW, not after a spawn.
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && v.get("id").is_none()
            && v.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
            && let Some(rid) = v.pointer("/params/requestId").and_then(Value::as_i64)
            && let Some(sender) = cancels.lock().unwrap().remove(&rid)
        {
            let _ = sender.send(true);
            continue;
        }
        let daemon = Arc::clone(&daemon);
        let tx = tx.clone();
        let cancels = Arc::clone(&cancels);
        tokio::spawn(async move {
            if let Some(resp) = handle(&daemon, &line, &cancels, &tx).await {
                let _ = tx.send(resp).await;
            }
        });
    }

    // stdin EOF: graceful shutdown, no orphan browsers.
    drop(tx);
    daemon.shutdown().await;
    let _ = writer.await;
    Ok(())
}
