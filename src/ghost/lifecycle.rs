//! Task and stderr ownership from launch through browser teardown.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(super) struct OwnedTask(pub tokio::task::JoinHandle<()>);

impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) type StderrTail = Arc<Mutex<Vec<String>>>;

// Keep allocation bounded even if a native library emits a very long line.
pub(super) async fn stderr_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    line.clear();
    let mut any = false;
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(any);
        }
        any = true;
        let end = bytes.iter().position(|&b| b == b'\n');
        let n = end.map_or(bytes.len(), |i| i + 1);
        let retain = n.min(4096usize.saturating_sub(line.len()));
        line.extend_from_slice(&bytes[..retain]);
        reader.consume(n);
        if end.is_some() {
            return Ok(true);
        }
    }
}

pub(super) fn retain_stderr(tail: &mut Vec<String>, line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        if tail.len() == 6 {
            tail.remove(0);
        }
        tail.push(trimmed.chars().take(200).collect());
    }
}

pub(super) async fn drain_stderr<R: AsyncBufRead + Unpin>(mut reader: R, tail: StderrTail) {
    let mut line = Vec::new();
    while matches!(stderr_line(&mut reader, &mut line).await, Ok(true)) {
        retain_stderr(&mut tail.lock().unwrap_or_else(|p| p.into_inner()), &line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stealth_v3_stderr_is_drained_after_endpoint_and_long_lines_stay_bounded() {
        let mut wire = vec![b'x'; 2 * 1024 * 1024];
        wire.extend_from_slice(b"\n");
        for n in 0..20 {
            wire.extend_from_slice(format!("GPU report {n}\n").as_bytes());
        }
        wire.extend_from_slice(b"GPU final \xff failure");
        let mut reader = tokio::io::BufReader::new(wire.as_slice());
        let mut line = Vec::new();
        assert!(stderr_line(&mut reader, &mut line).await.unwrap());
        assert_eq!(line.len(), 4096);
        let tail = StderrTail::default();
        drain_stderr(reader, tail.clone()).await;
        let lines = tail.lock().unwrap();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "GPU report 15");
        assert!(lines[5].starts_with("GPU final") && lines[5].ends_with("failure"));
        assert!(lines.iter().all(|line| line.len() < 512));
    }

    #[tokio::test]
    async fn stealth_v3_launch_task_owner_cancels_before_ghost_exists() {
        let (ready, started) = tokio::sync::oneshot::channel();
        let (alive, ended) = tokio::sync::oneshot::channel::<()>();
        let owned = OwnedTask(tokio::spawn(async move {
            let _alive = alive;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        started.await.unwrap();
        assert!(!owned.0.is_finished());
        drop(owned);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), ended)
                .await
                .unwrap()
                .is_err(),
            "the task's live sender must be dropped on owner cancellation"
        );
    }
}
