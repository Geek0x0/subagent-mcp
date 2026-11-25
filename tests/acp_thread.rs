#![cfg(unix)]
//! `AcpThread` against the scripted fake agent in fixtures/acp_fake_agent.py.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::acp::thread::{self, Spawn};
use subagent_mcp::provider::{Thread, ThreadCallbacks, ThreadOptions};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

fn fixture() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/acp_fake_agent.py"
    )
    .to_string()
}

fn python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("python3"))
        .find(|p| p.is_file())
}

/// `kill_all` reaps every ACP child in the process, so its test excludes
/// the others.
static KILL_ALL: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

macro_rules! need_python {
    () => {
        let _shared = KILL_ALL.read().await;
        if python3().is_none() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
    };
}

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn log(&self) -> PathBuf {
        self.dir.path().join("log.jsonl")
    }
    fn pid_file(&self) -> PathBuf {
        self.dir.path().join("pid")
    }
    fn spawn(&self, mode: &str, extra: &[(&str, &str)]) -> Spawn {
        let mut env = BTreeMap::new();
        env.insert("ACP_FAKE_MODE".into(), mode.into());
        env.insert("ACP_FAKE_LOG".into(), self.log().display().to_string());
        env.insert("ACP_FAKE_PID".into(), self.pid_file().display().to_string());
        for (k, v) in extra {
            env.insert(k.to_string(), v.to_string());
        }
        let mut s = Spawn::new("fake", "python3");
        s.args = vec![fixture()];
        s.env = env;
        s.cancel_grace = Duration::from_millis(200);
        s
    }
    fn messages(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
    fn prompts(&self) -> Vec<String> {
        self.messages()
            .iter()
            .filter(|m| m["method"] == "session/prompt")
            .map(|m| {
                m["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
    fn pid(&self) -> i32 {
        std::fs::read_to_string(self.pid_file())
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
    fn cwd(&self) -> &Path {
        self.dir.path()
    }
}

fn opts(cwd: &Path) -> ThreadOptions {
    ThreadOptions {
        model: "default".into(),
        cwd: cwd.to_path_buf(),
        sandbox: Sandbox::WorkspaceWrite,
        approval_policy: ApprovalPolicy::OnRequest,
        base_instructions: None,
        developer_instructions: None,
        writable_roots: Vec::new(),
        ephemeral: true,
    }
}

/// Approver with a fixed answer that counts calls; `Hang` waits for its ctx.
#[derive(Clone)]
enum Answer {
    Yes,
    No,
    Hang,
}

struct TestApprover {
    answer: Answer,
    calls: Mutex<u32>,
    saw_cancel: Mutex<bool>,
    entered: tokio::sync::Notify,
}

impl TestApprover {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: Mutex::new(0),
            saw_cancel: Mutex::new(false),
            entered: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait::async_trait]
impl Approver for TestApprover {
    async fn approve(&self, ctx: &RunContext, _req: ApprovalRequest) -> bool {
        *self.calls.lock().unwrap() += 1;
        self.entered.notify_one();
        match self.answer {
            Answer::Yes => true,
            Answer::No => false,
            Answer::Hang => {
                ctx.cancel.cancelled().await;
                *self.saw_cancel.lock().unwrap() = true;
                false
            }
        }
    }
}

type Events = Arc<Mutex<Vec<Value>>>;

fn callbacks(approver: Arc<TestApprover>) -> (ThreadCallbacks, Events) {
    let events: Events = Arc::default();
    let sink = Arc::clone(&events);
    (
        ThreadCallbacks {
            emit: Arc::new(move |_kind, ev| sink.lock().unwrap().push(ev)),
            approve: approver,
        },
        events,
    )
}

fn of_type(events: &Events, ty: &str) -> Vec<Value> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .cloned()
        .collect()
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn start(fake: &Fake, spawn: Spawn) -> thread::AcpThread {
    thread::start(&ctx(), spawn, opts(fake.cwd()))
        .await
        .unwrap()
}

async fn start_err(fake: &Fake, spawn: Spawn) -> String {
    match thread::start(&ctx(), spawn, opts(fake.cwd())).await {
        Ok(_) => panic!("start succeeded"),
        Err(e) => e.message,
    }
}

#[tokio::test]
async fn echo_turn_returns_text() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("echo", &[])).await;
    let (cb, events) = callbacks(TestApprover::new(Answer::Yes));
    let out = t.run(ctx(), "hi".into(), None, cb).await.unwrap();
    assert_eq!(out, "hello world");
    assert_eq!(of_type(&events, "agent_message_delta").len(), 2);
    let prompt = fake
        .messages()
        .into_iter()
        .find(|m| m["method"] == "session/prompt")
        .unwrap();
    assert_eq!(prompt["params"]["sessionId"], "sess-1");
    assert_eq!(prompt["params"]["prompt"][0]["type"], "text");
    t.close().await.unwrap();
}

#[tokio::test]
async fn initialize_advertises_no_fs_terminal() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("echo", &[])).await;
    let first = fake.messages().into_iter().next().unwrap();
    assert_eq!(first["method"], "initialize");
    assert_eq!(first["params"]["protocolVersion"], 1);
    assert_eq!(
        first["params"]["clientCapabilities"],
        serde_json::json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false})
    );
    let new = &fake.messages()[1];
    assert_eq!(new["method"], "session/new");
    assert_eq!(new["params"]["mcpServers"], serde_json::json!([]));
    t.close().await.unwrap();
}

#[tokio::test]
async fn instructions_prepended_once() {
    need_python!();
    let fake = Fake::new();
    let mut o = opts(fake.cwd());
    o.base_instructions = Some("be brief".into());
    o.developer_instructions = Some("dev note".into());
    let t = thread::start(&ctx(), fake.spawn("echo", &[]), o)
        .await
        .unwrap();
    for _ in 0..2 {
        let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
        t.run(ctx(), "task".into(), None, cb).await.unwrap();
    }
    let prompts = fake.prompts();
    assert!(prompts[0].starts_with("<instructions>"), "{}", prompts[0]);
    assert!(prompts[0].contains("be brief") && prompts[0].contains("dev note"));
    assert!(prompts[0].contains("</instructions>\n\ntask"));
    assert_eq!(prompts[1], "task");
    t.close().await.unwrap();
}

#[tokio::test]
async fn effort_warns_once() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("echo", &[])).await;
    let (cb, events) = callbacks(TestApprover::new(Answer::Yes));
    for _ in 0..2 {
        t.run(ctx(), "x".into(), Some("high".into()), cb.clone())
            .await
            .unwrap();
    }
    let warnings = of_type(&events, "warning");
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0]["message"],
        "acp: effort is not supported and was ignored"
    );
    t.close().await.unwrap();
}

#[tokio::test]
async fn set_model_unsupported_warns() {
    need_python!();
    for mode in ["set_model_unsupported", "set_model_bad_params"] {
        let fake = Fake::new();
        let mut o = opts(fake.cwd());
        o.model = "fancy".into();
        let t = thread::start(&ctx(), fake.spawn(mode, &[]), o)
            .await
            .unwrap();
        let (cb, events) = callbacks(TestApprover::new(Answer::Yes));
        assert_eq!(
            t.run(ctx(), "x".into(), None, cb).await.unwrap(),
            "hello world"
        );
        let warnings = of_type(&events, "warning");
        assert_eq!(warnings.len(), 1, "{mode}");
        assert!(warnings[0]["message"].as_str().unwrap().contains("fancy"));
        let set = fake
            .messages()
            .into_iter()
            .find(|m| m["method"] == "session/set_model")
            .unwrap();
        assert_eq!(set["params"]["modelId"], "fancy");
        t.close().await.unwrap();
    }
}

#[tokio::test]
async fn permission_approved_selects_allow_once() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("permission", &[])).await;
    let approver = TestApprover::new(Answer::Yes);
    let (cb, _) = callbacks(Arc::clone(&approver));
    assert_eq!(
        t.run(ctx(), "x".into(), None, cb).await.unwrap(),
        "opt-allow"
    );
    assert_eq!(*approver.calls.lock().unwrap(), 1);
    t.close().await.unwrap();
}

#[tokio::test]
async fn permission_denied_selects_reject_once() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("permission", &[])).await;
    let (cb, _) = callbacks(TestApprover::new(Answer::No));
    assert_eq!(
        t.run(ctx(), "x".into(), None, cb).await.unwrap(),
        "opt-reject"
    );
    t.close().await.unwrap();
}

#[tokio::test]
async fn read_only_auto_rejects_without_approver() {
    need_python!();
    let fake = Fake::new();
    let mut o = opts(fake.cwd());
    o.sandbox = Sandbox::ReadOnly;
    let t = thread::start(&ctx(), fake.spawn("permission", &[]), o)
        .await
        .unwrap();
    let approver = TestApprover::new(Answer::Yes);
    let (cb, _) = callbacks(Arc::clone(&approver));
    assert_eq!(
        t.run(ctx(), "x".into(), None, cb).await.unwrap(),
        "opt-reject"
    );
    assert_eq!(*approver.calls.lock().unwrap(), 0);
    t.close().await.unwrap();
}

#[tokio::test]
async fn cancel_during_approval_sends_cancelled() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("permission", &[("ACP_FAKE_PERMS", "2")])).await;
    let approver = TestApprover::new(Answer::Hang);
    let (cb, _) = callbacks(Arc::clone(&approver));
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    let waiter = Arc::clone(&approver);
    tokio::spawn(async move {
        waiter.entered.notified().await;
        cancel.cancel();
    });
    let started = Instant::now();
    let err = t.run(run_ctx, "x".into(), None, cb).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert!(err.message.contains("cancelled"), "{}", err.message);
    assert!(*approver.saw_cancel.lock().unwrap());
    let msgs = fake.messages();
    assert!(msgs.iter().any(|m| m["method"] == "session/cancel"));
    let answers: Vec<_> = msgs
        .iter()
        .filter(|m| m["id"].as_str().is_some_and(|id| id.starts_with("perm-")))
        .collect();
    assert_eq!(answers.len(), 2, "{msgs:?}");
    for a in answers {
        assert_eq!(a["result"]["outcome"]["outcome"], "cancelled");
    }
    // The agent answered the prompt within grace, so the thread lives on.
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    t.run(ctx(), "y".into(), None, cb).await.unwrap();
    t.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_ctx_returns_promptly() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("hang", &[])).await;
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    assert!(t.run(run_ctx, "x".into(), None, cb).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    t.close().await.unwrap();
}

#[tokio::test]
async fn ignore_cancel_kills_after_grace() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("ignore_cancel", &[])).await;
    let pid = fake.pid();
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    assert!(t.run(run_ctx, "x".into(), None, cb.clone()).await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(!alive(pid));
    let err = t.run(ctx(), "y".into(), None, cb).await.unwrap_err();
    assert!(
        err.message.contains("start a new session"),
        "{}",
        err.message
    );
    t.close().await.unwrap();
}

#[tokio::test]
async fn agent_cancel_without_ctx_cancel_errors() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("agent_cancel", &[])).await;
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    let err = t.run(ctx(), "x".into(), None, cb).await.unwrap_err();
    assert_eq!(
        err.message,
        "acp provider \"fake\": agent cancelled the turn"
    );
    t.close().await.unwrap();
}

#[tokio::test]
async fn max_tokens_returns_partial_with_warning() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("stop:max_tokens", &[])).await;
    let (cb, events) = callbacks(TestApprover::new(Answer::Yes));
    assert_eq!(t.run(ctx(), "x".into(), None, cb).await.unwrap(), "partial");
    let w = of_type(&events, "warning");
    assert_eq!(w[0]["message"], "acp: turn stopped: max_tokens");
    t.close().await.unwrap();
}

#[tokio::test]
async fn refusal_returns_partial_with_warning() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("stop:refusal", &[])).await;
    let (cb, events) = callbacks(TestApprover::new(Answer::Yes));
    assert_eq!(t.run(ctx(), "x".into(), None, cb).await.unwrap(), "partial");
    let w = of_type(&events, "warning");
    assert_eq!(w[0]["message"], "acp: turn stopped: refusal");
    t.close().await.unwrap();
}

#[tokio::test]
async fn no_token_count_emitted() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("echo", &[])).await;
    let (cb, events) = callbacks(TestApprover::new(Answer::Yes));
    t.run(ctx(), "x".into(), None, cb).await.unwrap();
    assert!(of_type(&events, "token_count").is_empty());
    t.close().await.unwrap();
}

#[tokio::test]
async fn crash_reports_stderr_tail() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("crash", &[])).await;
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    let err = t
        .run(ctx(), "x".into(), None, cb.clone())
        .await
        .unwrap_err();
    assert!(err.message.contains("(stderr:"), "{}", err.message);
    assert!(err.message.contains("fatal crash"), "{}", err.message);
    let err = t.run(ctx(), "x".into(), None, cb).await.unwrap_err();
    assert!(
        err.message.contains("start a new session"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn auth_error_names_auth_methods() {
    need_python!();
    let fake = Fake::new();
    let err = start_err(&fake, fake.spawn("auth", &[])).await;
    assert_eq!(
        err,
        "acp provider \"fake\": authentication required (authMethods: login); log in with the agent itself"
    );
}

#[tokio::test]
async fn bad_version_rejected() {
    need_python!();
    let fake = Fake::new();
    let err = start_err(&fake, fake.spawn("bad_version", &[])).await;
    assert!(err.contains("protocolVersion 99"), "{err}");
}

#[tokio::test]
async fn missing_command_errors() {
    let fake = Fake::new();
    for command in ["/nonexistent/acp", "acp-definitely-missing"] {
        let spawn = Spawn::new("fake", command);
        let started = Instant::now();
        let err = start_err(&fake, spawn).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(err.contains(command), "{err}");
    }
}

#[tokio::test]
async fn banner_tolerated() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("banner", &[])).await;
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    assert_eq!(
        t.run(ctx(), "x".into(), None, cb).await.unwrap(),
        "hello world"
    );
    t.close().await.unwrap();
}

#[tokio::test]
async fn probe_reports_ready_without_agent_info() {
    need_python!();
    let fake = Fake::new();
    let status = thread::probe(&ctx(), fake.spawn("echo", &[]))
        .await
        .unwrap();
    assert!(status.summary.contains("ACP agent"), "{}", status.summary);
    assert_eq!(status.version, None);
}

#[tokio::test]
async fn close_kills_child() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("echo", &[])).await;
    let pid = fake.pid();
    assert!(alive(pid));
    t.close().await.unwrap();
    assert!(!alive(pid));
}

#[tokio::test]
async fn close_kills_process_group_after_normal_exit() {
    need_python!();
    let fake = Fake::new();
    let gc_file = fake.dir.path().join("grandchild");
    let gc_path = gc_file.display().to_string();
    let t = start(
        &fake,
        fake.spawn("echo", &[("ACP_FAKE_GRANDCHILD", gc_path.as_str())]),
    )
    .await;
    let gc: i32 = std::fs::read_to_string(&gc_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(gc));
    // The agent exits on stdin EOF within the grace period; its grandchild
    // must still be torn down with the group.
    t.close().await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while alive(gc) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let gone = !alive(gc);
    if !gone {
        unsafe { libc::kill(gc, libc::SIGKILL) };
    }
    assert!(gone, "grandchild {gc} survived close");
}

#[tokio::test]
async fn close_dead_thread_is_ok() {
    need_python!();
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("crash", &[])).await;
    let (cb, _) = callbacks(TestApprover::new(Answer::Yes));
    let _ = t.run(ctx(), "x".into(), None, cb).await;
    t.close().await.unwrap();
    t.close().await.unwrap();
}

#[tokio::test]
async fn kill_all_reaps_children() {
    let _exclusive = KILL_ALL.write().await;
    if python3().is_none() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let fake = Fake::new();
    let t = start(&fake, fake.spawn("hang", &[])).await;
    let pid = fake.pid();
    assert!(alive(pid));
    thread::kill_all();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child must be reaped");
    drop(t);
}
