//! Task 25A: CLI flags, `--check-config`, and `check::run`.
//!
//! Ports the non-runtime half of Go `cmd/subagent-mcp/main_test.go` and
//! `check_test.go`. The serve lifecycle, signals, and EOF cleanup belong to
//! Task 25B and live in `tests/cli_runtime.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde_json::{Value, json};
use subagent_mcp::check::{self, AdapterFactory, Built};
use subagent_mcp::config::{Api, AutoConfig, Config, ModelConfig, ProviderConfig};
use subagent_mcp::provider::{
    Agent, AuthChecker, AuthStatus, DeltaSink, ModelInfo, ModelLister, Provider, ProviderError,
    Thread, ThreadCallbacks, ThreadOptions, TurnRequest, TurnResult,
};
use subagent_mcp::util::run::RunContext;

const CHECK_KEY: &str = "SUBAGENT_CHECK_TEST_API_KEY";
const CHECK_SECRET: &str = "sk-check-child-secret";

// ---------------------------------------------------------------------------
// CLI process helpers
// ---------------------------------------------------------------------------

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_subagent-mcp")
}

/// Run the binary with a clean environment: no inherited `SUBAGENT_MCP_*`
/// configuration, and a temporary `HOME`.
fn run_cli(args: &[&str], envs: &[(&str, &str)], home: &Path) -> Output {
    let mut cmd = Command::new(binary());
    cmd.args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    {
        // Windows networking and the default config path use these; clearing
        // them breaks child-process loopback connections and home lookup.
        cmd.env("USERPROFILE", home);
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
    }
    for (name, value) in envs {
        cmd.env(name, value);
    }
    cmd.output().unwrap()
}

fn write_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

fn chat_config(base: &str, env_key: &str) -> String {
    format!(
        r#"
[providers.test]
api = "chat-completions"
base_url = "{base}"
env_key = "{env_key}"
default_model = "test-model"
models = [{{ id = "test-model" }}]
"#
    )
}

// ---------------------------------------------------------------------------
// Fake chat-completions HTTP server (stand-in for Go's testutil.FakeChat)
// ---------------------------------------------------------------------------

struct FakeChat {
    base: String,
    turn_count: Arc<AtomicUsize>,
}

fn spawn_fake_chat(models: &[&str], models_status: u16, turns: Vec<String>) -> FakeChat {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let data: Vec<Value> = models
        .iter()
        .map(|id| json!({"id": id, "object": "model"}))
        .collect();
    let models_body = Arc::new(
        json!({"object": "list", "data": data})
            .to_string()
            .into_bytes(),
    );
    let turns = Arc::new(turns);
    let turn_count = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&turn_count);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some((path, _body)) = read_http_request(&mut stream) else {
                continue;
            };
            if path == "/models" {
                write_http(&mut stream, models_status, "application/json", &models_body);
            } else {
                let index = count.fetch_add(1, Ordering::SeqCst);
                let body = turns
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| sse_text("no scripted turn"));
                write_http(&mut stream, 200, "text/event-stream", body.as_bytes());
            }
        }
    });

    FakeChat { base, turn_count }
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
        401 => "Unauthorized",
        500 => "Internal Server Error",
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

fn sse_tool_call(name: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":"{}"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
    )
}

// ---------------------------------------------------------------------------
// CLI flag parsing / help / version
// ---------------------------------------------------------------------------

#[test]
fn version_prints_expected() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["--version", "-version"] {
        let out = run_cli(&[flag], &[], home.path());
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "subagent-mcp 0.14.0\n"
        );
    }
}

#[test]
fn unknown_flag_prints_go_error_and_usage() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["--bogus", "-x"] {
        let out = run_cli(&[flag], &[], home.path());
        assert_eq!(out.status.code(), Some(2), "flag {flag}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let name = flag.trim_start_matches('-');
        assert!(
            stderr.contains(&format!("flag provided but not defined: -{name}")),
            "stderr = {stderr}"
        );
        assert!(stderr.contains("Usage of"), "stderr = {stderr}");
    }
}

#[test]
fn bad_flag_syntax_prints_go_error_and_usage() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["-=x", "---x"] {
        let out = run_cli(&[flag], &[], home.path());
        assert_eq!(out.status.code(), Some(2), "flag {flag}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("bad flag syntax: {flag}")),
            "stderr = {stderr}"
        );
        assert!(stderr.contains("Usage of"), "stderr = {stderr}");
    }
}

#[test]
fn help_exits_zero_and_prints_usage() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["-h", "--help"] {
        let out = run_cli(&[flag], &[], home.path());
        assert_eq!(out.status.code(), Some(0), "flag {flag}");
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        assert!(combined.contains("Usage of"), "output = {combined}");
        assert!(combined.contains("check-config"), "output = {combined}");
    }
}

// ---------------------------------------------------------------------------
// `--check-config`
// ---------------------------------------------------------------------------

#[test]
fn check_config_ok_prints_path_and_result() {
    let fake = spawn_fake_chat(&["test-model"], 200, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &chat_config(&fake.base, CHECK_KEY));
    let path_text = path.to_str().unwrap();

    let out = run_cli(
        &["--check-config", path_text],
        &[(CHECK_KEY, CHECK_SECRET)],
        dir.path(),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for want in [
        "config",
        "OK",
        path_text,
        "provider test (chat-completions)",
        "  key    SUBAGENT_CHECK_TEST_API_KEY   set",
        "  api    ",
        "  model  test-model     OK",
        "result   PASS (1 checked, 0 skipped, 0 failed)",
    ] {
        assert!(stdout.contains(want), "stdout = {stdout}\nmissing {want}");
    }
    assert!(
        !stdout.contains(CHECK_SECRET),
        "stdout leaked the API key value: {stdout}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains(CHECK_SECRET),
        "stderr leaked the API key value: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_config_uses_home_default_path() {
    let fake = spawn_fake_chat(&["test-model"], 200, Vec::new());
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join(".config").join("subagent-mcp");
    std::fs::create_dir_all(&config_dir).unwrap();
    let path = write_config(&config_dir, &chat_config(&fake.base, CHECK_KEY));

    let out = run_cli(
        &["--check-config"],
        &[(CHECK_KEY, CHECK_SECRET)],
        home.path(),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(path.to_str().unwrap()), "stdout = {stdout}");
    assert!(stdout.contains("result   PASS (1 checked, 0 skipped, 0 failed)"));
}

#[test]
fn check_config_single_dash_uses_default_path() {
    let fake = spawn_fake_chat(&["test-model"], 200, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &chat_config(&fake.base, CHECK_KEY));
    let path_text = path.to_str().unwrap();

    let out = run_cli(
        &["-check-config"],
        &[
            ("SUBAGENT_MCP_CONFIG", path_text),
            (CHECK_KEY, CHECK_SECRET),
        ],
        dir.path(),
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(path_text), "stdout = {stdout}");
    assert!(stdout.contains("result   PASS (1 checked, 0 skipped, 0 failed)"));
}

#[test]
fn check_config_live_flags_round_trip() {
    for live_flag in ["-live", "--live"] {
        let fake = spawn_fake_chat(
            &["test-model"],
            200,
            vec![sse_tool_call("get_secret_number"), sse_text("4217")],
        );
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &chat_config(&fake.base, CHECK_KEY));
        let path_text = path.to_str().unwrap();

        let out = run_cli(
            &["--check-config", live_flag, path_text],
            &[(CHECK_KEY, CHECK_SECRET)],
            dir.path(),
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "flag {live_flag}; stderr = {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("--live will make real, billed API calls"),
            "stdout = {stdout}"
        );
        assert!(
            stdout.contains("  live   test-model   OK (tool call + follow-up succeeded, "),
            "stdout = {stdout}"
        );
        assert!(stdout.contains("result   PASS (1 checked, 0 skipped, 0 failed)"));
        assert_eq!(fake.turn_count.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn check_config_missing_path_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    let missing_text = missing.to_str().unwrap();

    let out = run_cli(&["--check-config", missing_text], &[], dir.path());
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for want in ["config", missing_text, "FAIL"] {
        assert!(stdout.contains(want), "stdout = {stdout}");
    }
}

#[test]
fn check_config_all_keys_unset_exits_one() {
    let fake = spawn_fake_chat(&["test-model"], 200, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &chat_config(&fake.base, CHECK_KEY));

    let out = run_cli(&["--check-config", path.to_str().unwrap()], &[], dir.path());
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("  key    SUBAGENT_CHECK_TEST_API_KEY   not set, skipped"),
        "stdout = {stdout}"
    );
    assert!(
        stdout.contains(
            "result   FAIL (0 checked, 1 skipped, 0 failed) — no configured provider has its key set"
        ),
        "stdout = {stdout}"
    );
    assert!(!stdout.contains("  api    "), "stdout = {stdout}");
}

#[test]
fn check_config_provider_failure_exits_one() {
    let fake = spawn_fake_chat(&["test-model"], 401, Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &chat_config(&fake.base, CHECK_KEY));

    let out = run_cli(
        &["--check-config", path.to_str().unwrap()],
        &[(CHECK_KEY, CHECK_SECRET)],
        dir.path(),
    );
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for want in [
        "  api    ",
        "FAIL: ",
        "401",
        "result   FAIL (1 checked, 0 skipped, 1 failed)",
    ] {
        assert!(stdout.contains(want), "stdout = {stdout}");
    }
    assert!(!stdout.contains(CHECK_SECRET), "stdout = {stdout}");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains(CHECK_SECRET),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Main dispatch before serve
// ---------------------------------------------------------------------------

#[test]
fn missing_config_file_reports_example() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    let missing_text = missing.to_str().unwrap();

    let out = run_cli(&[], &[("SUBAGENT_MCP_CONFIG", missing_text)], dir.path());
    assert_ne!(out.status.code(), Some(0));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains(missing_text), "output = {combined}");
    assert!(
        combined.contains("config.example.toml"),
        "output = {combined}"
    );
}

#[test]
fn invalid_tool_name_is_rejected_before_serve() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        dir.path(),
        r#"
[providers.test]
api = "chat-completions"
env_key = "SUBAGENT_TEST_API_KEY"
default_model = "test-model"
models = [{ id = "test-model" }]
"#,
    );
    let path_text = path.to_str().unwrap();

    for name in ["bad name", "codex-reply"] {
        let out = run_cli(
            &[],
            &[
                ("SUBAGENT_MCP_CONFIG", path_text),
                ("SUBAGENT_MCP_TOOL_NAME", name),
            ],
            dir.path(),
        );
        assert_ne!(out.status.code(), Some(0), "name {name}");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            combined.contains("SUBAGENT_MCP_TOOL_NAME"),
            "output = {combined}"
        );
    }
}

// ---------------------------------------------------------------------------
// `check::run` with a stub adapter factory
// ---------------------------------------------------------------------------

struct StubProvider {
    models: Vec<String>,
    models_error: Option<String>,
    auth: AuthStatus,
    auth_error: Option<String>,
    thread_reply: Result<String, String>,
    turns: Mutex<VecDeque<Result<TurnResult, String>>>,
    requests: AtomicUsize,
    thread_closed: Arc<AtomicBool>,
}

impl StubProvider {
    fn new() -> Self {
        Self {
            models: Vec::new(),
            models_error: None,
            auth: AuthStatus::default(),
            auth_error: None,
            thread_reply: Ok("The answer is 4217.".to_string()),
            turns: Mutex::new(VecDeque::new()),
            requests: AtomicUsize::new(0),
            thread_closed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_models(mut self, models: &[&str]) -> Self {
        self.models = models.iter().map(|m| (*m).to_string()).collect();
        self
    }

    fn with_turns(mut self, turns: Vec<Result<TurnResult, String>>) -> Self {
        self.turns = Mutex::new(turns.into());
        self
    }
}

#[async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }

    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        match self.turns.lock().unwrap().pop_front() {
            Some(Ok(result)) => Ok(result),
            Some(Err(message)) => Err(ProviderError { message }),
            None => Err(ProviderError {
                message: "no scripted turn".to_string(),
            }),
        }
    }
}

#[async_trait]
impl ModelLister for StubProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        if let Some(message) = &self.models_error {
            return Err(ProviderError {
                message: message.clone(),
            });
        }
        Ok(self
            .models
            .iter()
            .map(|id| ModelInfo {
                id: id.clone(),
                description: String::new(),
            })
            .collect())
    }
}

#[async_trait]
impl AuthChecker for StubProvider {
    async fn check_auth(&self) -> Result<AuthStatus, ProviderError> {
        if let Some(message) = &self.auth_error {
            return Err(ProviderError {
                message: message.clone(),
            });
        }
        Ok(self.auth.clone())
    }
}

struct StubThread {
    reply: Result<String, String>,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl Thread for StubThread {
    async fn run(
        &self,
        _ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        _callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        match &self.reply {
            Ok(text) => Ok(text.clone()),
            Err(message) => Err(ProviderError {
                message: message.clone(),
            }),
        }
    }

    async fn close(&self) -> Result<(), ProviderError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Agent for StubProvider {
    async fn start_thread(
        &self,
        _ctx: &RunContext,
        _opts: ThreadOptions,
    ) -> Result<Box<dyn Thread>, ProviderError> {
        Ok(Box::new(StubThread {
            reply: self.thread_reply.clone(),
            closed: Arc::clone(&self.thread_closed),
        }))
    }
}

#[derive(Clone)]
struct StubFactory {
    built: Built,
}

impl AdapterFactory for StubFactory {
    fn build(
        &self,
        _name: &str,
        _cfg: &ProviderConfig,
        _key: &str,
    ) -> Result<Built, ProviderError> {
        Ok(self.built.clone())
    }
}

struct FailingFactory;

impl AdapterFactory for FailingFactory {
    fn build(
        &self,
        _name: &str,
        _cfg: &ProviderConfig,
        _key: &str,
    ) -> Result<Built, ProviderError> {
        Err(ProviderError {
            message: "adapter boom".to_string(),
        })
    }
}

fn built(stub: &Arc<StubProvider>, lister: bool, agent: bool, auth: bool) -> Built {
    let provider: Arc<dyn Provider> = stub.clone();
    let lister: Option<Arc<dyn ModelLister>> = if lister { Some(stub.clone()) } else { None };
    let agent: Option<Arc<dyn Agent>> = if agent { Some(stub.clone()) } else { None };
    let auth: Option<Arc<dyn AuthChecker>> = if auth { Some(stub.clone()) } else { None };
    Built {
        provider,
        lister,
        agent,
        auth,
    }
}

fn provider_config(
    api: Api,
    base_url: &str,
    env_key: Option<&str>,
    key: Option<&str>,
    default_model: &str,
    models: &[&str],
) -> ProviderConfig {
    ProviderConfig {
        api,
        base_url: base_url.to_string(),
        env_key: env_key.map(str::to_string),
        key: key.map(str::to_string),
        command: None,
        default_model: default_model.to_string(),
        models: models
            .iter()
            .map(|id| ModelConfig {
                id: (*id).to_string(),
                description: String::new(),
            })
            .collect(),
        effort_map: BTreeMap::new(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

fn config_with(providers: Vec<(&str, ProviderConfig)>, auto: Option<AutoConfig>) -> Config {
    Config {
        path: PathBuf::from("/tmp/subagent-mcp-test-config.toml"),
        providers: providers
            .into_iter()
            .map(|(name, cfg)| (name.to_string(), cfg))
            .collect(),
        auto,
    }
}

fn run_check(config: &Config, live: bool, factory: &dyn AdapterFactory) -> (i32, String) {
    let mut out = Vec::new();
    let code = check::run_with(&mut out, config, live, factory);
    (code, String::from_utf8(out).unwrap())
}

// Environment changes are process-global; serialize every test that uses them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_env(name: &str, value: &str) {
    // SAFETY: every test that mutates the environment holds ENV_LOCK.
    unsafe { std::env::set_var(name, value) };
}

fn remove_env(name: &str) {
    // SAFETY: every test that mutates the environment holds ENV_LOCK.
    unsafe { std::env::remove_var(name) };
}

#[test]
fn check_run_all_providers_healthy() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let stub = Arc::new(StubProvider::new().with_models(&["deepseek-flash", "deepseek-v4-pro"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.deepseek.invalid",
                Some(CHECK_KEY),
                None,
                "deepseek-flash",
                &["deepseek-flash", "deepseek-v4-pro"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    for want in [
        "provider deepseek (chat-completions)",
        "  key    SUBAGENT_CHECK_TEST_API_KEY   set",
        "  api    https://api.deepseek.invalid   OK (2 models listed)",
        "  model  deepseek-flash     OK",
        "  model  deepseek-v4-pro     OK",
        "result   PASS (1 checked, 0 skipped, 0 failed)",
    ] {
        assert!(out.contains(want), "output missing {want}:\n{out}");
    }
    assert!(!out.contains(CHECK_SECRET), "output leaked a key:\n{out}");
}

#[test]
fn check_run_all_keys_unset_fails() {
    let _guard = env_guard();
    remove_env(CHECK_KEY);

    let stub = Arc::new(StubProvider::new().with_models(&["test-model"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "test",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "test-model",
                &["test-model"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains("  key    SUBAGENT_CHECK_TEST_API_KEY   not set, skipped"),
        "output:\n{out}"
    );
    assert!(
        out.contains(
            "result   FAIL (0 checked, 1 skipped, 0 failed) — no configured provider has its key set"
        ),
        "output:\n{out}"
    );
    assert!(!out.contains("  api    "), "output:\n{out}");
}

#[test]
fn check_run_skipped_provider_does_not_fail() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);
    remove_env("SUBAGENT_CHECK_UNSET_KEY");

    let stub = Arc::new(StubProvider::new().with_models(&["deepseek-flash"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![
            (
                "deepseek",
                provider_config(
                    Api::ChatCompletions,
                    "https://api.deepseek.invalid",
                    Some(CHECK_KEY),
                    None,
                    "deepseek-flash",
                    &["deepseek-flash"],
                ),
            ),
            (
                "skipped",
                provider_config(
                    Api::Messages,
                    "https://api.anthropic.invalid",
                    Some("SUBAGENT_CHECK_UNSET_KEY"),
                    None,
                    "claude-test",
                    &["claude-test"],
                ),
            ),
        ],
        None,
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(
        out.contains("  key    SUBAGENT_CHECK_UNSET_KEY   not set, skipped"),
        "output:\n{out}"
    );
    assert!(
        out.contains("result   PASS (1 checked, 1 skipped, 0 failed)"),
        "output:\n{out}"
    );
}

#[test]
fn check_run_model_listing_failure_skips_live() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let mut stub = StubProvider::new();
    stub.models_error = Some("list models: HTTP 401: unauthorized".to_string());
    let stub = Arc::new(stub);
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "test",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "test-model",
                &["test-model"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, true, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    for want in [
        "--live will make real, billed API calls to each configured provider.",
        "  api    https://api.invalid   FAIL: list models: HTTP 401: unauthorized",
        "result   FAIL (1 checked, 0 skipped, 1 failed)",
    ] {
        assert!(out.contains(want), "output missing {want}:\n{out}");
    }
    assert!(!out.contains("  live   "), "output:\n{out}");
    assert_eq!(stub.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn check_run_missing_model_warns_only() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let stub = Arc::new(StubProvider::new().with_models(&["deepseek-flash"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "deepseek-flash",
                &["deepseek-flash", "deepseek-v4-pro"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(
        out.contains("  model  deepseek-v4-pro     WARN: not in the provider's current model list"),
        "output:\n{out}"
    );
    assert!(!out.contains("FAIL"), "output:\n{out}");
}

#[test]
fn check_run_live_round_trip_succeeds() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let stub = Arc::new(
        StubProvider::new()
            .with_models(&["deepseek-flash"])
            .with_turns(vec![
                Ok(TurnResult {
                    tool_calls: vec![subagent_mcp::provider::ToolCall {
                        id: "call_1".to_string(),
                        name: "get_secret_number".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    ..Default::default()
                }),
                Ok(TurnResult {
                    text: "The secret number is 4217.".to_string(),
                    ..Default::default()
                }),
            ]),
    );
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "deepseek-flash",
                &["deepseek-flash"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, true, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(
        out.contains("  live   deepseek-flash   OK (tool call + follow-up succeeded, "),
        "output:\n{out}"
    );
    assert_eq!(stub.requests.load(Ordering::SeqCst), 2);
}

#[test]
fn check_run_live_failure_reports_follow_up() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let stub = Arc::new(
        StubProvider::new()
            .with_models(&["deepseek-flash"])
            .with_turns(vec![
                Ok(TurnResult {
                    tool_calls: vec![subagent_mcp::provider::ToolCall {
                        id: "call_1".to_string(),
                        name: "get_secret_number".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    ..Default::default()
                }),
                Ok(TurnResult {
                    text: "1234".to_string(),
                    ..Default::default()
                }),
            ]),
    );
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "deepseek-flash",
                &["deepseek-flash"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, true, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains("  live   deepseek-flash   FAIL: follow-up answer \"1234\" does not contain the tool result"),
        "output:\n{out}"
    );
    assert!(
        out.contains("result   FAIL (1 checked, 0 skipped, 1 failed)"),
        "output:\n{out}"
    );
}

#[test]
fn check_run_without_model_lister() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let stub = Arc::new(StubProvider::new().with_turns(vec![
        Ok(TurnResult {
            tool_calls: vec![subagent_mcp::provider::ToolCall {
                id: "call_1".to_string(),
                name: "get_secret_number".to_string(),
                arguments: "{}".to_string(),
            }],
            ..Default::default()
        }),
        Ok(TurnResult {
            text: "4217".to_string(),
            ..Default::default()
        }),
    ]));
    let factory = StubFactory {
        built: built(&stub, false, false, false),
    };
    let config = config_with(
        vec![(
            "custom",
            provider_config(
                Api::ChatCompletions,
                "https://check.invalid",
                Some(CHECK_KEY),
                None,
                "custom-model",
                &["custom-model"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, true, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(
        out.contains("  api    https://check.invalid   OK (model listing not supported)"),
        "output:\n{out}"
    );
    assert!(!out.contains("  model  "), "output:\n{out}");
    assert!(
        out.contains("  live   custom-model   OK (tool call + follow-up succeeded, "),
        "output:\n{out}"
    );
}

#[test]
fn check_run_adapter_error_fails() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);

    let config = config_with(
        vec![(
            "test",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "test-model",
                &["test-model"],
            ),
        )],
        None,
    );

    let (code, out) = run_check(&config, false, &FailingFactory);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains("  api    https://api.invalid   FAIL: adapter boom"),
        "output:\n{out}"
    );
    assert!(
        out.contains("result   FAIL (1 checked, 0 skipped, 1 failed)"),
        "output:\n{out}"
    );
}

#[test]
fn check_run_no_provider_fails() {
    let config = config_with(Vec::new(), None);
    let stub = Arc::new(StubProvider::new());
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains(
            "result   FAIL (0 checked, 0 skipped, 0 failed) — no configured provider has its key set"
        ),
        "output:\n{out}"
    );
}

// ---------------------------------------------------------------------------
// Codex check-config
// ---------------------------------------------------------------------------

fn codex_config() -> Config {
    config_with(
        vec![(
            "codex",
            provider_config(
                Api::CodexAppServer,
                "",
                None,
                None,
                "gpt-6-astra",
                &["gpt-6-astra"],
            ),
        )],
        None,
    )
}

fn codex_stub() -> Arc<StubProvider> {
    let mut stub = StubProvider::new().with_models(&["gpt-6-astra"]);
    stub.auth = AuthStatus {
        summary: "chatgpt (plus)".to_string(),
        version: Some("codex-cli 0.156.1".to_string()),
    };
    Arc::new(stub)
}

#[test]
fn check_run_codex_healthy() {
    let stub = codex_stub();
    let factory = StubFactory {
        built: built(&stub, true, true, true),
    };
    let (code, out) = run_check(&codex_config(), false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    let want = "provider codex (codex-app-server)\n\
        \x20 auth   chatgpt (plus)   OK\n\
        \x20 api    codex-cli 0.156.1   OK (1 models listed)\n\
        \x20 model  gpt-6-astra     OK\n\
        result   PASS (1 checked, 0 skipped, 0 failed)\n";
    assert_eq!(out, want);
}

#[test]
fn check_run_codex_logged_out() {
    let mut stub = codex_stub();
    // Replace the stub after construction to set the auth error.
    let inner = Arc::get_mut(&mut stub).unwrap();
    inner.auth_error = Some("not logged in; run codex login".to_string());
    let factory = StubFactory {
        built: built(&stub, true, true, true),
    };

    let (code, out) = run_check(&codex_config(), true, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains("  auth   FAIL: not logged in; run codex login"),
        "output:\n{out}"
    );
    assert!(
        out.contains("result   FAIL (0 checked, 0 skipped, 1 failed)"),
        "output:\n{out}"
    );
    for unwanted in ["  key    ", "  api    ", "  model  ", "  live   "] {
        assert!(!out.contains(unwanted), "output:\n{out}");
    }
}

#[test]
fn check_run_codex_model_listing_failure_skips_live() {
    let mut stub = codex_stub();
    let inner = Arc::get_mut(&mut stub).unwrap();
    inner.models_error = Some("model listing failed".to_string());
    let factory = StubFactory {
        built: built(&stub, true, true, true),
    };

    let (code, out) = run_check(&codex_config(), true, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    let want = "--live will make real, billed API calls to each configured provider.\n\
        provider codex (codex-app-server)\n\
        \x20 auth   chatgpt (plus)   OK\n\
        \x20 api    FAIL: model listing failed\n\
        result   FAIL (1 checked, 0 skipped, 1 failed)\n";
    assert_eq!(out, want);
}

#[test]
fn check_run_codex_live() {
    let stub = codex_stub();
    let factory = StubFactory {
        built: built(&stub, true, true, true),
    };
    let (code, out) = run_check(&codex_config(), true, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    assert!(
        out.starts_with("--live will make real, billed API calls to each configured provider.\n"),
        "output:\n{out}"
    );
    assert!(
        out.contains("  live   gpt-6-astra   OK (turn succeeded, "),
        "output:\n{out}"
    );
    assert!(
        out.ends_with("result   PASS (1 checked, 0 skipped, 0 failed)\n"),
        "output:\n{out}"
    );
    assert!(stub.thread_closed.load(Ordering::SeqCst));
}

#[test]
fn check_run_codex_live_failure() {
    let mut stub = codex_stub();
    let inner = Arc::get_mut(&mut stub).unwrap();
    inner.thread_reply = Ok("1234".to_string());
    let factory = StubFactory {
        built: built(&stub, true, true, true),
    };

    let (code, out) = run_check(&codex_config(), true, &factory);
    assert_eq!(code, 1, "output:\n{out}");
    assert!(
        out.contains("  live   gpt-6-astra   FAIL: answer \"1234\" does not contain 4217"),
        "output:\n{out}"
    );
    assert!(
        out.contains("result   FAIL (1 checked, 0 skipped, 1 failed)"),
        "output:\n{out}"
    );
}

#[test]
fn check_run_codex_and_native_counting() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);
    remove_env("SUBAGENT_CHECK_UNSET_KEY");

    let stub = codex_stub();
    let factory = StubFactory {
        built: built(&stub, true, true, true),
    };
    let config = config_with(
        vec![
            (
                "codex",
                provider_config(
                    Api::CodexAppServer,
                    "",
                    None,
                    None,
                    "gpt-6-astra",
                    &["gpt-6-astra"],
                ),
            ),
            (
                "native",
                provider_config(
                    Api::ChatCompletions,
                    "https://native.invalid",
                    Some(CHECK_KEY),
                    None,
                    "gpt-6-astra",
                    &["gpt-6-astra"],
                ),
            ),
            (
                "skipped",
                provider_config(
                    Api::ChatCompletions,
                    "https://skipped.invalid",
                    Some("SUBAGENT_CHECK_UNSET_KEY"),
                    None,
                    "unused",
                    &["unused"],
                ),
            ),
        ],
        None,
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    for want in [
        "  auth   chatgpt (plus)   OK",
        "  key    SUBAGENT_CHECK_TEST_API_KEY   set",
        "  api    https://native.invalid   OK (1 models listed)",
        "  model  gpt-6-astra     OK",
        "  key    SUBAGENT_CHECK_UNSET_KEY   not set, skipped",
        "result   PASS (2 checked, 1 skipped, 0 failed)",
    ] {
        assert!(out.contains(want), "output missing {want}:\n{out}");
    }
}

// ---------------------------------------------------------------------------
// Auto section and key sources
// ---------------------------------------------------------------------------

#[test]
fn check_run_reports_auto_section() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);
    set_env("TYPESAFE_TEST_KEY", "jev-secret-value");

    let stub = Arc::new(StubProvider::new().with_models(&["deepseek-flash"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "deepseek-flash",
                &["deepseek-flash"],
            ),
        )],
        Some(AutoConfig {
            base_url: "https://api.typesafe.ai".to_string(),
            url: String::new(),
            env_key: Some("TYPESAFE_TEST_KEY".to_string()),
            key: None,
            model: "jev-latest".to_string(),
            timeout_seconds: 20,
        }),
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    for want in [
        "auto     decision model (jev-latest)",
        "  api    https://api.typesafe.ai",
        "  key    TYPESAFE_TEST_KEY   set",
    ] {
        assert!(out.contains(want), "output missing {want}:\n{out}");
    }
    assert!(!out.contains("jev-secret-value"), "output:\n{out}");
}

#[test]
fn check_run_reports_auto_url_and_warns_when_unset() {
    let _guard = env_guard();
    set_env(CHECK_KEY, CHECK_SECRET);
    remove_env("TYPESAFE_TEST_KEY");

    let stub = Arc::new(StubProvider::new().with_models(&["deepseek-flash"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                Some(CHECK_KEY),
                None,
                "deepseek-flash",
                &["deepseek-flash"],
            ),
        )],
        Some(AutoConfig {
            base_url: String::new(),
            url: "https://example.com/ai/run/clef".to_string(),
            env_key: Some("TYPESAFE_TEST_KEY".to_string()),
            key: None,
            model: "clef".to_string(),
            timeout_seconds: 20,
        }),
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    for want in [
        "auto     decision model (clef)",
        "  api    https://example.com/ai/run/clef",
        "  key    TYPESAFE_TEST_KEY   not set, decision model selection calls will fail",
    ] {
        assert!(out.contains(want), "output missing {want}:\n{out}");
    }
    assert!(!out.contains("FAIL"), "output:\n{out}");
}

#[test]
fn check_run_key_from_config_file_is_never_printed() {
    let _guard = env_guard();
    const FILE_KEY: &str = "sk-file-secret-value-7c21";

    let stub = Arc::new(StubProvider::new().with_models(&["deepseek-flash"]));
    let factory = StubFactory {
        built: built(&stub, true, false, false),
    };
    let config = config_with(
        vec![(
            "deepseek",
            provider_config(
                Api::ChatCompletions,
                "https://api.invalid",
                None,
                Some(FILE_KEY),
                "deepseek-flash",
                &["deepseek-flash"],
            ),
        )],
        Some(AutoConfig {
            base_url: "https://auto.invalid".to_string(),
            url: String::new(),
            env_key: None,
            key: Some(FILE_KEY.to_string()),
            model: "jev-latest".to_string(),
            timeout_seconds: 20,
        }),
    );

    let (code, out) = run_check(&config, false, &factory);
    assert_eq!(code, 0, "output:\n{out}");
    assert_eq!(out.matches("  key    config file   set").count(), 2);
    assert!(!out.contains(FILE_KEY), "output:\n{out}");
}

// ---------------------------------------------------------------------------
// Sandbox report and tool-name validation
// ---------------------------------------------------------------------------

#[test]
fn report_sandbox_prints_one_line() {
    let mut out = Vec::new();
    check::report_sandbox(&mut out);
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.matches('\n').count(), 1, "output = {text}");
    let line = text.trim_end();
    if subagent_mcp::sandbox::available() {
        assert_eq!(line, "sandbox  Landlock   OK");
    } else {
        assert!(line.starts_with("sandbox  Landlock   WARN: "), "{line}");
        assert!(line.contains("exit 126"), "{line}");
    }
}

#[test]
fn env_keys_are_sorted_and_deduplicated() {
    let config = config_with(
        vec![
            (
                "a",
                provider_config(
                    Api::ChatCompletions,
                    "https://a.invalid",
                    Some("Z_KEY"),
                    None,
                    "m",
                    &["m"],
                ),
            ),
            (
                "b",
                provider_config(
                    Api::Messages,
                    "https://b.invalid",
                    Some("A_KEY"),
                    None,
                    "m",
                    &["m"],
                ),
            ),
            (
                "c",
                provider_config(
                    Api::Responses,
                    "https://c.invalid",
                    Some("A_KEY"),
                    None,
                    "m",
                    &["m"],
                ),
            ),
        ],
        Some(AutoConfig {
            base_url: "https://auto.invalid".to_string(),
            url: String::new(),
            env_key: Some("M_KEY".to_string()),
            key: None,
            model: "m".to_string(),
            timeout_seconds: 20,
        }),
    );
    assert_eq!(check::env_keys(&config), vec!["A_KEY", "M_KEY", "Z_KEY"]);
}

#[test]
fn validate_tool_name_matches_go_contract() {
    for valid in ["deepseek", "codex", "a_b-1"] {
        assert!(
            subagent_mcp::cli::validate_tool_name(valid).is_ok(),
            "{valid} should be valid"
        );
    }
    let long = "a".repeat(65);
    for invalid in ["", "has space", "bad/char", long.as_str(), "codex-reply"] {
        let err = subagent_mcp::cli::validate_tool_name(invalid).unwrap_err();
        assert!(
            err.to_string().contains("SUBAGENT_MCP_TOOL_NAME"),
            "error for {invalid:?} = {err}"
        );
    }
}
