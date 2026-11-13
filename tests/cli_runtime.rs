//! Task 25B: the serve lifecycle, signals, EOF cleanup, and runtime hardening.
//!
//! Ports the runtime half of Go `cmd/subagent-mcp/main_test.go` and
//! `runtime_test.go`. The process is the real `subagent-mcp` binary driven over
//! MCP stdio, because the lifecycle under test is exactly the boundary between
//! the OS pipes, the signal handler, and `std::process::exit`.
//!
//! The signal tests are Unix-only (Windows has no SIGTERM); the platform files
//! repeat the SIGTERM-with-stdin-open invariant per platform. The environment
//! hardening tests are Linux-only, as in Go.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The production shutdown budget; assertions match the 10 s promise.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);
/// Go's `TestMainStopsOnSignal` fails when shutdown waits out the grace period.
#[cfg(unix)]
const PROMPT_SHUTDOWN: Duration = Duration::from_secs(8);
/// Generous bound for process startup and one MCP round trip.
const MCP_TIMEOUT: Duration = Duration::from_secs(20);

const API_KEY: &str = "SUBAGENT_RUNTIME_TEST_KEY";
const API_SECRET: &str = "sk-runtime-child-secret";
#[cfg(target_os = "linux")]
const PREFIX_MARKER: &str = "SUBAGENT_MCP_RUNTIME_MARKER";
#[cfg(target_os = "linux")]
const PREFIX_MARKER_VALUE: &str = "prefix-marker-value";
#[cfg(target_os = "linux")]
const VISIBLE_MARKER: &str = "SUBAGENT_RUNTIME_VISIBLE_MARKER";
#[cfg(target_os = "linux")]
const VISIBLE_MARKER_VALUE: &str = "visible-marker-value";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_subagent-mcp")
}

#[cfg(target_os = "linux")]
#[test]
fn check_config_reports_tls_construction_error_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text("http://127.0.0.1:1"));
    let output = Command::new(binary())
        .arg("--check-config")
        .arg(config)
        .env(API_KEY, "test-key")
        .env("SSL_CERT_FILE", dir.path().join("missing.pem"))
        .env("SSL_CERT_DIR", dir.path().join("missing-dir"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("FAIL:") && stdout.contains("tls:"),
        "stdout: {stdout}; stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn sigterm_with_full_stdout_and_one_worker_is_bounded() {
    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path(), &config_text("http://127.0.0.1:1"));
    let mut child = Reap(
        Command::new(binary())
            .env("HOME", home.path())
            .env("SUBAGENT_MCP_CONFIG", config)
            .env("TOKIO_WORKER_THREADS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0", "id":1,"method":"initialize","params":{"capabilities":{},"protocolVersion":"2024-11-05","clientInfo":{"name":"blocked-test","version":"1"}}})).unwrap();
    let mut response = String::new();
    stdout.read_line(&mut response).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&response).unwrap()["id"], 1);
    let flood = std::thread::spawn(move || {
        for id in 2..2002 {
            if writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"method":"ping"})
            )
            .is_err()
            {
                break;
            }
        }
    });
    // Leave stdout open and unread until process exit; replies fill its pipe.
    std::thread::sleep(Duration::from_millis(750));
    let start = Instant::now();
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGTERM);
    }
    loop {
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            start.elapsed() < SHUTDOWN_BUDGET + Duration::from_secs(2),
            "shutdown hung with unread stdout"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdout);
    flood.join().unwrap();
}

// ---------------------------------------------------------------------------
// Fake chat-completions server
// ---------------------------------------------------------------------------

struct FakeChat {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

/// Serves the scripted turns in order and records each request body. A turn is
/// one SSE body; `/models` is answered separately because a live check may ask
/// for it.
fn spawn_fake_chat(turns: Vec<String>) -> FakeChat {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let turns = Arc::new(Mutex::new(VecDeque::from(turns)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let turn_queue = Arc::clone(&turns);
    let recorded = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some((path, body)) = read_http_request(&mut stream) else {
                continue;
            };
            if path.ends_with("/models") {
                let body = json!({
                    "object": "list",
                    "data": [{"id": "test-model", "object": "model"}],
                })
                .to_string();
                write_http(&mut stream, 200, "application/json", body.as_bytes());
                continue;
            }
            recorded
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            let turn = turn_queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| sse_text("done"));
            write_http(&mut stream, 200, "text/event-stream", turn.as_bytes());
        }
    });
    FakeChat { base, requests }
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find_bytes(&buf, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..end]).into_owned();
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let start = end + 4;
            while buf.len() < start + length {
                let read = stream.read(&mut chunk).ok()?;
                if read == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..read]);
            }
            let path = headers
                .lines()
                .next()?
                .split_whitespace()
                .nth(1)?
                .to_string();
            let body_end = (start + length).min(buf.len());
            return Some((path, buf[start..body_end].to_vec()));
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_http(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    )
}

fn sse_tool_call(name: &str, arguments: &Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":arguments.to_string()}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
    )
}

// ---------------------------------------------------------------------------
// Config and process harness
// ---------------------------------------------------------------------------

fn config_text(base: &str) -> String {
    format!(
        r#"
[providers.test]
api = "chat-completions"
base_url = "{base}"
env_key = "{API_KEY}"
default_model = "test-model"
models = [{{ id = "test-model" }}]
"#
    )
}

fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

/// A running `subagent-mcp` with MCP stdio pipes and background readers.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: std::sync::mpsc::Receiver<String>,
    stderr: Arc<Mutex<String>>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(config: &Path, home: &Path, envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(binary());
        command
            .env_clear()
            .env("HOME", home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("SUBAGENT_MCP_CONFIG", config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            command.env("USERPROFILE", home);
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        for (name, value) in envs {
            command.env(name, value);
        }

        let mut child = command.spawn().expect("spawn subagent-mcp");
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr_pipe = child.stderr.take().expect("stderr is piped");

        let (sender, frames) = std::sync::mpsc::channel();
        let stdout_reader = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let stderr_reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => sink.lock().unwrap().push_str(&line),
                }
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            frames,
            stderr,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        }
    }

    fn send(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{frame}").expect("write frame");
        stdin.flush().expect("flush frame");
    }

    fn recv_frame(&self, timeout: Duration) -> Option<Value> {
        let line = self.frames.recv_timeout(timeout).ok()?;
        match serde_json::from_str(&line) {
            Ok(value) => Some(value),
            Err(error) => panic!("invalid JSON frame {line:?}: {error}"),
        }
    }

    fn wait_for_id(&self, id: i64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(frame) = self.recv_frame(remaining) else {
                panic!(
                    "no response for id {id} within {timeout:?}; stderr:\n{}",
                    self.stderr()
                );
            };
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }

    #[cfg(unix)]
    fn wait_for_event(&self, kind: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(frame) = self.recv_frame(remaining) else {
                panic!(
                    "no {kind:?} event within {timeout:?}; stderr:\n{}",
                    self.stderr()
                );
            };
            if frame.get("method").and_then(Value::as_str) == Some("subagent/event")
                && frame["params"]["msg"]["type"].as_str() == Some(kind)
            {
                return frame;
            }
        }
    }

    fn initialize(&mut self) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "runtime-test", "version": "1"}
            }
        }));
        self.wait_for_id(1, MCP_TIMEOUT)
    }

    fn call_subagent(&mut self, id: i64, cwd: &Path) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "subagent",
                "arguments": {
                    "prompt": "run the test command",
                    "provider": "test",
                    "model": "test-model",
                    "cwd": cwd.to_str().expect("cwd is UTF-8"),
                    "sandbox": "danger-full-access",
                    "approval-policy": "never",
                    "config": {"max_turns": 3}
                }
            }
        }));
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "server did not exit within {timeout:?}; stderr:\n{}",
                    self.stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(unix)]
fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            panic!("{} was not created within {timeout:?}", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kill a descendant that intentionally left the process group, so a failed or
/// immediate-exit test cannot leave a `sleep` behind.
#[cfg(unix)]
fn kill_escaped(path: &Path) {
    if let Ok(text) = std::fs::read_to_string(path)
        && let Ok(pid) = text.trim().parse::<i32>()
    {
        // SAFETY: best-effort SIGKILL of a PID the test itself started.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// A shell command whose direct child is in the process group (so cancellation
/// kills it) and whose `setsid` descendant holds the inherited pipes (so the
/// managed runner waits one `WaitDelay` before abandoning them). That window is
/// what the second-signal test needs; the pid file lets the test reap it.
#[cfg(unix)]
fn escaped_sleep_command() -> String {
    "echo started > started.txt\nsetsid sleep 5 &\necho $! > escaped.pid\nsleep 5".to_string()
}

// ---------------------------------------------------------------------------
// Serve: protocol wiring and startup
// ---------------------------------------------------------------------------

#[test]
fn serve_answers_initialize_and_lists_tools() {
    let fake = spawn_fake_chat(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut server = Server::start(&config, home.path(), &[(API_KEY, API_SECRET)]);
    let init = server.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "subagent-mcp");
    assert_eq!(init["result"]["serverInfo"]["version"], "0.14.0");
    assert_eq!(init["result"]["capabilities"], json!({"tools": {}}));

    server.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = server.wait_for_id(2, MCP_TIMEOUT);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"subagent"), "tools = {names:?}");
    assert!(names.contains(&"subagent-reply"), "tools = {names:?}");

    server.close_stdin();
    let status = server.wait_exit(SHUTDOWN_BUDGET);
    assert!(
        status.success(),
        "exit = {status:?}; stderr:\n{}",
        server.stderr()
    );
}

#[test]
fn serve_honors_subagent_tool_name() {
    let fake = spawn_fake_chat(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut server = Server::start(
        &config,
        home.path(),
        &[(API_KEY, API_SECRET), ("SUBAGENT_MCP_TOOL_NAME", "helper")],
    );
    server.initialize();
    server.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let list = server.wait_for_id(2, MCP_TIMEOUT);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"helper"), "tools = {names:?}");
    assert!(names.contains(&"helper-reply"), "tools = {names:?}");

    server.close_stdin();
    assert!(server.wait_exit(SHUTDOWN_BUDGET).success());
}

/// Go `TestMainStartsWithoutAnyProviderKeySet`: a missing key is a per-call
/// error, never a startup failure.
#[test]
fn serve_starts_without_any_provider_key_set() {
    let fake = spawn_fake_chat(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut server = Server::start(&config, home.path(), &[]);
    let init = server.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "subagent-mcp");

    server.close_stdin();
    assert!(
        server.wait_exit(SHUTDOWN_BUDGET).success(),
        "stderr:\n{}",
        server.stderr()
    );
}

// ---------------------------------------------------------------------------
// EOF cleanup
// ---------------------------------------------------------------------------

/// A client that closes its end immediately is a clean EOF: the process exits 0
/// without waiting for a signal.
#[test]
fn eof_with_stdin_closed_exits_zero() {
    let fake = spawn_fake_chat(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut command = Command::new(binary());
    command
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("SUBAGENT_MCP_CONFIG", &config)
        .env(API_KEY, API_SECRET)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        command.env("USERPROFILE", home.path());
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    let mut child = command.spawn().expect("spawn subagent-mcp");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if start.elapsed() >= SHUTDOWN_BUDGET {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server did not exit within {SHUTDOWN_BUDGET:?} of EOF");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "exit = {status:?}");
}

/// EOF while a call is in flight cancels the call instead of running it to
/// completion against a client that can no longer receive the result.
#[cfg(unix)]
#[test]
fn eof_cancels_in_flight_call_and_exits_zero() {
    let fake = spawn_fake_chat(vec![sse_tool_call(
        "shell",
        &json!({"command": escaped_sleep_command()}),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));
    let escaped = dir.path().join("escaped.pid");

    let mut server = Server::start(&config, home.path(), &[(API_KEY, API_SECRET)]);
    server.initialize();
    server.call_subagent(2, dir.path());
    server.wait_for_event("exec_command_begin", MCP_TIMEOUT);
    wait_for_file(&dir.path().join("started.txt"), MCP_TIMEOUT);

    let start = Instant::now();
    server.close_stdin();
    let status = server.wait_exit(SHUTDOWN_BUDGET);
    kill_escaped(&escaped);
    assert!(
        status.success(),
        "exit = {status:?}; stderr:\n{}",
        server.stderr()
    );
    assert!(
        start.elapsed() < PROMPT_SHUTDOWN,
        "EOF cleanup waited out the grace period: {:?}",
        start.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// Go `TestMainStopsOnSignal`: SIGTERM while stdin is open and idle stops the
/// server cleanly, without waiting out the ten-second grace period.
#[cfg(unix)]
#[test]
fn sigterm_with_stdin_open_exits_within_budget() {
    let fake = spawn_fake_chat(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut server = Server::start(&config, home.path(), &[(API_KEY, API_SECRET)]);
    server.initialize();

    let start = Instant::now();
    let pid = i32::try_from(server.child.id()).expect("pid fits i32");
    // SAFETY: the child was started by this test and has not been reaped.
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "send SIGTERM: {}",
        std::io::Error::last_os_error()
    );

    let status = server.wait_exit(SHUTDOWN_BUDGET);
    assert!(
        status.success(),
        "exit = {status:?}; stderr:\n{}",
        server.stderr()
    );
    assert!(
        start.elapsed() < PROMPT_SHUTDOWN,
        "shutdown waited out the grace period: {:?}",
        start.elapsed()
    );
}

/// A second signal abandons cleanup and exits 130. The in-flight shell call
/// keeps shutdown busy for one `WaitDelay`, so the second signal lands while the
/// process is genuinely cleaning up rather than after it has exited.
#[cfg(unix)]
#[test]
fn second_sigterm_exits_130() {
    if Command::new("setsid")
        .arg("true")
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        eprintln!("skip: setsid is unavailable");
        return;
    }

    let fake = spawn_fake_chat(vec![sse_tool_call(
        "shell",
        &json!({"command": escaped_sleep_command()}),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));
    let escaped = dir.path().join("escaped.pid");

    let mut server = Server::start(&config, home.path(), &[(API_KEY, API_SECRET)]);
    server.initialize();
    server.call_subagent(2, dir.path());
    server.wait_for_event("exec_command_begin", MCP_TIMEOUT);
    wait_for_file(&dir.path().join("started.txt"), MCP_TIMEOUT);

    let pid = i32::try_from(server.child.id()).expect("pid fits i32");
    // SAFETY: the child was started by this test and has not been reaped.
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "send first SIGTERM: {}",
        std::io::Error::last_os_error()
    );
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: same live child; a second signal is the documented abort path.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let status = server.wait_exit(SHUTDOWN_BUDGET);
    kill_escaped(&escaped);
    assert_eq!(
        status.code(),
        Some(130),
        "exit = {status:?}; stderr:\n{}",
        server.stderr()
    );
}

/// SIGTERM with an in-flight call still shuts down within the budget: the call
/// is cancelled, the session is closed, and the process reports success.
#[cfg(unix)]
#[test]
fn sigterm_cancels_in_flight_call_and_exits_zero() {
    let fake = spawn_fake_chat(vec![sse_tool_call(
        "shell",
        &json!({"command": escaped_sleep_command()}),
    )]);
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));
    let escaped = dir.path().join("escaped.pid");

    let mut server = Server::start(&config, home.path(), &[(API_KEY, API_SECRET)]);
    server.initialize();
    server.call_subagent(2, dir.path());
    server.wait_for_event("exec_command_begin", MCP_TIMEOUT);
    wait_for_file(&dir.path().join("started.txt"), MCP_TIMEOUT);

    let pid = i32::try_from(server.child.id()).expect("pid fits i32");
    // SAFETY: the child was started by this test and has not been reaped.
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "send SIGTERM: {}",
        std::io::Error::last_os_error()
    );

    let start = Instant::now();
    let status = server.wait_exit(SHUTDOWN_BUDGET);
    kill_escaped(&escaped);
    assert!(
        status.success(),
        "exit = {status:?}; stderr:\n{}",
        server.stderr()
    );
    assert!(
        start.elapsed() < PROMPT_SHUTDOWN,
        "shutdown waited out the grace period: {:?}",
        start.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Runtime hardening (Linux)
// ---------------------------------------------------------------------------

/// Go `TestServerEnvironmentIsNotReadableThroughProc`, driven through the real
/// serve path: the shell child cannot see a configured provider key, any
/// `SUBAGENT_MCP_` variable, or the server's `/proc/<pid>/environ`.
#[cfg(target_os = "linux")]
#[test]
fn runtime_scrubs_provider_key_from_shell_children() {
    // Root can read a non-dumpable process's /proc entry, so the `/proc` half
    // of the Go test is meaningful only for an unprivileged caller.
    let unprivileged = unsafe { libc::geteuid() } != 0;
    let mut command = "env > env.txt".to_string();
    if unprivileged {
        command.push_str("\ncat /proc/$PPID/environ >> env.txt 2>&1 || true");
    }
    let fake = spawn_fake_chat(vec![sse_tool_call("shell", &json!({"command": command}))]);
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut server = Server::start(
        &config,
        home.path(),
        &[
            (API_KEY, API_SECRET),
            (PREFIX_MARKER, PREFIX_MARKER_VALUE),
            (VISIBLE_MARKER, VISIBLE_MARKER_VALUE),
        ],
    );
    server.initialize();
    server.call_subagent(2, dir.path());
    let response = server.wait_for_id(2, MCP_TIMEOUT);
    assert!(
        response.get("error").is_none(),
        "tools/call failed: {response}"
    );

    let env = std::fs::read_to_string(dir.path().join("env.txt")).expect("env.txt");
    assert!(
        env.contains(VISIBLE_MARKER_VALUE),
        "positive control marker is missing; env dump:\n{env}"
    );
    assert!(
        !env.contains(API_SECRET),
        "shell child saw the provider key:\n{env}"
    );
    assert!(
        !env.contains(PREFIX_MARKER_VALUE),
        "shell child saw a SUBAGENT_MCP_ variable:\n{env}"
    );
    if unprivileged {
        assert!(
            !env.contains(API_SECRET),
            "shell child read the server environment through /proc:\n{env}"
        );
    }

    server.close_stdin();
    assert!(server.wait_exit(SHUTDOWN_BUDGET).success());
}

/// The loaded config is in the protected-file table, so the model cannot read
/// it with `read_file`. The tool result travels back in the next provider turn;
/// inspect that request body because the tool error is not the final answer.
#[test]
fn runtime_protects_the_config_file_from_read_file() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let config_path = config_path
        .to_str()
        .expect("config path is UTF-8")
        .to_string();

    let fake = spawn_fake_chat(vec![
        sse_tool_call("read_file", &json!({"path": config_path})),
        sse_text("done"),
    ]);
    let config = write_config(dir.path(), &config_text(&fake.base));

    let mut server = Server::start(&config, home.path(), &[(API_KEY, API_SECRET)]);
    server.initialize();
    server.call_subagent(2, dir.path());
    let response = server.wait_for_id(2, MCP_TIMEOUT);
    assert!(
        response.get("error").is_none(),
        "tools/call failed: {response}"
    );
    assert_eq!(response["result"]["structuredContent"]["content"], "done");

    let requests = fake.requests.lock().unwrap().clone();
    assert!(
        requests.len() >= 2,
        "expected a follow-up turn, got {} request(s)",
        requests.len()
    );
    assert!(
        requests[1].contains("refusing to read a protected subagent-mcp file"),
        "the config file was not protected; follow-up body:\n{}",
        requests[1]
    );

    server.close_stdin();
    assert!(server.wait_exit(SHUTDOWN_BUDGET).success());
}
