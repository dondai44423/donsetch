//! stdio transport for the MCP server.
//!
//! Handles stdin/stdout I/O for the MCP JSON-RPC protocol.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use serde_json::Value;

use crate::mcp::compat::ModeCell;
use crate::mcp::server::{CancelMap, Daemon, cancel_key, handle};

/// One stdin line, classified.
enum Incoming {
    /// A line to hand to the JSON-RPC dispatcher.
    Request(String),
    /// A line that is not UTF-8: the client is still there, so
    /// this is not EOF. Reason text for the error response.
    Malformed(String),
    /// stdin closed, or a real read error.
    Eof,
}

/// `Lines::next_line` reports a line that isn't UTF-8 as
/// `Err(InvalidData)` -- after consuming it, so the reader is
/// positioned at the next line. Treating that Err as EOF (the old
/// `while let Ok(Some(..))`) shut the whole daemon down on one bad
/// byte from the client, mid-session, with every in-flight tool
/// call orphaned. Only a real read error or EOF ends the loop.
async fn next_incoming<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
) -> Incoming {
    match lines.next_line().await {
        Ok(Some(l)) => Incoming::Request(l),
        Ok(None) => Incoming::Eof,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Incoming::Malformed(e.to_string()),
        Err(e) => {
            eprintln!("[mcp] stdin read failed, shutting down: {e}");
            Incoming::Eof
        }
    }
}

/// JSON-RPC parse error for a line the dispatcher never saw
/// (same shape `handle` emits for unparseable JSON).
fn parse_error(reason: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": null,
        "error": { "code": -32700, "message": format!("parse error: {reason}") }
    })
    .to_string()
}

/// Run the stdio MCP daemon until stdin closes.
/// Never returns Err on client garbage : only on fatal IO.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let daemon = Arc::new(Daemon::new().await?);
    daemon.start_prober();
    let (tx, mut rx) = mpsc::channel::<String>(256);

    // Single writer: response lines can never interleave.
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            // A broken stdout (client died, pipe closed) must not be
            // swallowed: every later response would be silently
            // dropped while the daemon pretends to serve. Log the
            // real cause and stop : the client is gone.
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
    // Issue #27 compat mode: one session per process; the initialize
    // handshake records what the client renders.
    let mode = Arc::new(ModeCell::new());

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match next_incoming(&mut lines).await {
            Incoming::Request(l) => l,
            Incoming::Malformed(reason) => {
                let _ = tx.send(parse_error(&reason)).await;
                continue;
            }
            Incoming::Eof => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // Cancellation notifications are handled inline : they must
        // reach the running tool NOW, not after a spawn.
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && v.get("id").is_none()
            && v.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
            && let Some(rid) = v.pointer("/params/requestId").and_then(cancel_key)
            && let Some(sender) = cancels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&rid)
        {
            let _ = sender.send(true);
            continue;
        }
        let daemon = Arc::clone(&daemon);
        let tx = tx.clone();
        let cancels = Arc::clone(&cancels);
        let mode = Arc::clone(&mode);
        tokio::spawn(async move {
            if let Some(resp) = handle(&daemon, &line, &cancels, &tx, &mode).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    // `Lines::next_line` yields Err(InvalidData) for a line that
    // isn't UTF-8. The loop used to treat any Err as EOF, so one
    // stray byte from the client (a pasted path in a legacy
    // codepage, a truncated multibyte char) shut the daemon down
    // mid-session. The bad line must be answered and skipped;
    // the request after it must still be served.
    #[tokio::test]
    async fn invalid_utf8_line_is_skipped_not_eof() {
        let input: &[u8] = b"{\"a\":1}\n\xff\xfe garbage\n{\"b\":2}\n";
        let mut lines = BufReader::new(input).lines();
        assert!(
            matches!(next_incoming(&mut lines).await, Incoming::Request(l) if l == "{\"a\":1}")
        );
        assert!(matches!(
            next_incoming(&mut lines).await,
            Incoming::Malformed(_)
        ));
        assert!(
            matches!(next_incoming(&mut lines).await, Incoming::Request(l) if l == "{\"b\":2}")
        );
        assert!(matches!(next_incoming(&mut lines).await, Incoming::Eof));
    }

    #[test]
    fn malformed_line_gets_a_jsonrpc_parse_error() {
        let v: Value = serde_json::from_str(&parse_error("stream did not contain valid UTF-8"))
            .expect("valid json");
        assert_eq!(v["jsonrpc"], "2.0");
        assert!(v["id"].is_null());
        assert_eq!(v["error"]["code"], -32700);
        assert!(v["error"]["message"].as_str().unwrap().contains("UTF-8"));
    }
}
