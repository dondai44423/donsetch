//! A local HTTP server for offline tests: no network, no third party.
//!
//! Each connection is read to the end of its request head (and body), handed to a
//! closure, and answered with whatever the closure returns. A reply may be a
//! normal response, a redirect, or a hostile stream (endless, trickled), so the
//! envelope can be tested against servers that misbehave on purpose.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[allow(dead_code)] // fixtures read what they need; the rest documents the request
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub enum Reply {
    /// A complete response.
    Full {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// A 302 to `location`.
    Redirect(String),
    /// A 200 with a declared length it never finishes: one byte every `every`.
    Trickle { every: Duration },
    /// A 200 with no length that never ends: `chunk` bytes, as fast as the client reads.
    Endless { chunk: usize },
    /// A complete 200 sent `chunk` bytes at a time, one chunk every `every`:
    /// a large file on a slow link.
    Paced {
        body: Vec<u8>,
        chunk: usize,
        every: Duration,
    },
    /// Accept, read the request, answer nothing and hold the socket open.
    Silence,
}

impl Reply {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self::Full {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        }
    }
}

pub struct Server {
    pub addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Listen on an ephemeral port of `bind` (an address in 127.0.0.0/8).
    pub fn on(bind: &str, handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind((bind, 0)).expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let handler = Arc::clone(&handler);
                            let seen = Arc::clone(&seen);
                            let stop = Arc::clone(&stop);
                            std::thread::spawn(move || {
                                serve(stream, handler.as_ref(), &seen, &stop)
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
        }
        Self { addr, seen, stop }
    }

    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        Self::on("127.0.0.1", handler)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Paths requested so far, in arrival order.
    pub fn hits(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn hit_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn serve(
    mut stream: TcpStream,
    handler: &(dyn Fn(&Request) -> Reply + Send + Sync),
    seen: &Mutex<Vec<String>>,
    stop: &AtomicBool,
) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    seen.lock().unwrap().push(request.path.clone());
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    match handler(&request) {
        Reply::Full {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
        Reply::Redirect(location) => {
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
        }
        Reply::Trickle { every } => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
            );
            while !stop.load(Ordering::SeqCst) {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(every);
            }
        }
        Reply::Paced { body, chunk, every } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            for piece in body.chunks(chunk.max(1)) {
                if stop.load(Ordering::SeqCst) || stream.write_all(piece).is_err() {
                    break;
                }
                let _ = stream.flush();
                std::thread::sleep(every);
            }
        }
        Reply::Endless { chunk } => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let block = vec![b'x'; chunk];
            while !stop.load(Ordering::SeqCst) {
                if stream.write_all(&block).is_err() {
                    break;
                }
            }
        }
        Reply::Silence => {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
