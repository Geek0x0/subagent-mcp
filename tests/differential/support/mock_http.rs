//! A blocking recording HTTP mock for the differential scenarios.
//!
//! The pinned Go binary and the Rust adapters both talk plain HTTP/1.1, so the
//! mock is a small `std::net` server instead of a tokio task: it works from
//! both `#[tokio::test]` scenarios and the synchronous MCP-replay scenarios.
//! Every request is recorded (method, path, headers, parsed body) and each
//! configured route answers with its queued bodies in order.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// One recorded provider request.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// Header names lowercased, values trimmed. Repeat headers keep the last.
    pub headers: HashMap<String, String>,
    /// Body parsed as JSON when possible, else the raw UTF-8 text.
    pub body: Value,
}

impl RecordedRequest {
    /// The wire-golden shape: `{method, path, headers, body}`.
    pub fn to_golden(&self) -> Value {
        serde_json::json!({
            "method": self.method,
            "path": self.path,
            "headers": self.headers,
            "body": self.body,
        })
    }
}

/// Route prefix -> queued reply bodies (consumed in order).
type Routes = Arc<Mutex<Vec<(String, VecDeque<String>)>>>;

pub struct MockHttp {
    pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
    routes: Routes,
    listener: TcpListener,
    /// Applied before every reply, so a scenario can hold a response open
    /// long enough to cancel mid-turn.
    delay: Arc<AtomicU64>,
}

impl MockHttp {
    /// Bind `127.0.0.1:0` and start the accept loop on a background thread.
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock HTTP server");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes: Routes = Arc::new(Mutex::new(Vec::new()));
        let delay = Arc::new(AtomicU64::new(0));
        let server = Self {
            requests: Arc::clone(&requests),
            routes: Arc::clone(&routes),
            listener,
            delay: Arc::clone(&delay),
        };
        let accept = TcpListener::try_clone(&server.listener).expect("clone listener");
        let requests_loop = Arc::clone(&requests);
        let routes_loop = Arc::clone(&routes);
        let delay_loop = Arc::clone(&delay);
        thread::spawn(move || {
            for stream in accept.incoming() {
                let Ok(mut stream) = stream else { break };
                let routes = Arc::clone(&routes_loop);
                let requests = Arc::clone(&requests_loop);
                let delay = Arc::clone(&delay_loop);
                // One request per connection (`Connection: close`), which is
                // all the differential scenarios need; loop anyway so a
                // keep-alive client cannot wedge the mock.
                while let Some(request) = read_request(&mut stream) {
                    let reply = next_reply(&routes, &request.path);
                    requests.lock().unwrap().push(request);
                    let pause = delay.load(Ordering::SeqCst);
                    if pause > 0 {
                        thread::sleep(Duration::from_millis(pause));
                    }
                    if write_reply(&mut stream, &reply).is_err() {
                        break;
                    }
                }
            }
        });
        server
    }

    /// Hold every reply for `millis` before sending it.
    pub fn set_delay_ms(&self, millis: u64) {
        self.delay.store(millis, Ordering::SeqCst);
    }

    /// Queue `bodies` (in order) for request paths starting with `prefix`.
    pub fn route(&self, prefix: &str, bodies: &[String]) {
        let mut queue = VecDeque::new();
        queue.extend(bodies.iter().cloned());
        self.routes
            .lock()
            .unwrap()
            .push((prefix.to_string(), queue));
    }

    /// Queue one body for every path (used when a scenario has one route).
    pub fn route_all(&self, bodies: &[String]) {
        self.route("", bodies);
    }

    /// The recorded requests so far.
    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// `http://127.0.0.1:<port>` for config files.
    pub fn url(&self) -> String {
        format!("http://{}", self.listener.local_addr().expect("local addr"))
    }
}

/// Read one HTTP/1.1 request; `None` on EOF or a parse failure.
fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    // Headers end at the first CRLFCRLF.
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => buffer.push(byte[0]),
            Err(_) => return None,
        }
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 && stream.read_exact(&mut body).is_err() {
        return None;
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    Some(RecordedRequest {
        method,
        path,
        headers,
        body: parsed,
    })
}

fn next_reply(routes: &Routes, path: &str) -> String {
    let mut routes = routes.lock().unwrap();
    // Longest matching prefix wins, so "/v1/systemone" beats "".
    let best = routes
        .iter()
        .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(prefix, _)| prefix.clone());
    if let Some(prefix) = best
        && let Some((_, queue)) = routes.iter_mut().find(|(p, _)| *p == prefix)
    {
        return queue.pop_front().unwrap_or_default();
    }
    String::new()
}

fn write_reply(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let content_type = if body.starts_with('{') {
        "application/json"
    } else {
        "text/event-stream"
    };
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes())?;
    stream.flush()
}
