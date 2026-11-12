//! Integration tests for Task 22: prompt construction and the in-memory
//! session manager. Ports Go v0.13.1 `internal/agent/agentsmd_test.go`,
//! `manager_acquire_test.go`, `session_boundary_test.go` (bind/roots aspects),
//! and `session_symlink_race_test.go` (symlink-resolved cwd aspects).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use subagent_mcp::agent::{
    AgentError, DEFAULT_MAX_TURNS, DEFAULT_SYSTEM_PROMPT, SessionError, SessionManager,
    SessionOptions, effective_writable_roots, load_agents_md, system_prompt,
};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::{
    DeltaSink, Message, Provider, ProviderError, Role, Thread, ThreadCallbacks, ToolCall,
    TurnRequest, TurnResult, Usage,
};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeProvider;

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        Ok(TurnResult::default())
    }
}

fn provider() -> Arc<dyn Provider> {
    Arc::new(FakeProvider)
}

/// A `Thread` implementation used to prove it is a first-class trait object.
struct FakeThread;

#[async_trait]
impl Thread for FakeThread {
    async fn run(
        &self,
        _ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        _callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        Ok(String::new())
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Default options with an empty cwd (binding is skipped) so manager tests stay
/// fast and do not touch the filesystem.
fn options() -> SessionOptions {
    SessionOptions {
        provider: provider(),
        thread: None,
        model: String::new(),
        reasoning_effort: String::new(),
        effort_sent: None,
        cwd: PathBuf::new(),
        sandbox: Sandbox::DangerFullAccess,
        approval: ApprovalPolicy::Never,
        system_prompt: String::new(),
        max_turns: 0,
        max_nudges: 0,
        writable_roots: Vec::new(),
        max_history_bytes: 0,
        max_total_tokens: 0,
        run_deadline: None,
    }
}

// ---------------------------------------------------------------------------
// Small filesystem helpers (Go testutil equivalents)
// ---------------------------------------------------------------------------

fn write(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn eval_dir(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

/// Keep the `TempDir` guard alive for the whole test while returning its
/// canonical, symlink-resolved path (Go `evalDir(t, t.TempDir())`).
fn tmp_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = eval_dir(dir.path());
    (dir, path)
}

/// Go `testutil.GitAncestor`: any ancestor (inclusive) holding a `.git` entry.
fn has_git_ancestor(dir: &Path) -> bool {
    let mut cur = dir.to_path_buf();
    loop {
        if std::fs::symlink_metadata(cur.join(".git")).is_ok() {
            return true;
        }
        match cur.parent() {
            Some(p) if p != cur && !p.as_os_str().is_empty() => cur = p.to_path_buf(),
            _ => return false,
        }
    }
}

fn go_quote(s: &str) -> String {
    subagent_mcp::util::json::go_quote(s)
}

// ---------------------------------------------------------------------------
// Prompt: default text, base override, developer append, AGENTS.md
// ---------------------------------------------------------------------------

#[test]
fn default_system_prompt_text_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let got = system_prompt(None, None, dir.path()).unwrap();
    assert_eq!(got, DEFAULT_SYSTEM_PROMPT);
    assert!(got.starts_with("You are subagent-mcp, a coding agent."));
    assert!(got.ends_with("how you verified the result."));
    assert!(!got.ends_with('\n'));
}

#[test]
fn base_override_and_developer_append() {
    let dir = tempfile::tempdir().unwrap();

    // A non-empty base replaces the default entirely.
    assert_eq!(
        system_prompt(Some("custom base"), None, dir.path()).unwrap(),
        "custom base"
    );
    // An explicitly empty base still uses the default (Go: `!= ""`).
    assert_eq!(
        system_prompt(Some(""), None, dir.path()).unwrap(),
        DEFAULT_SYSTEM_PROMPT
    );
    // Developer instructions append with a blank-line separator.
    assert_eq!(
        system_prompt(None, Some("dev instructions"), dir.path()).unwrap(),
        format!("{DEFAULT_SYSTEM_PROMPT}\n\ndev instructions")
    );
    // An empty developer append adds nothing.
    assert_eq!(
        system_prompt(None, Some(""), dir.path()).unwrap(),
        DEFAULT_SYSTEM_PROMPT
    );
}

#[test]
fn load_agents_md_root_to_cwd() {
    let (_dir, root) = tmp_dir();
    write(&root.join(".git/HEAD"), b"ref: refs/heads/main\n");
    write(&root.join("AGENTS.md"), b"root rules");
    write(&root.join("svc/AGENTS.md"), b"svc rules");
    let cwd = root.join("svc/pkg");
    std::fs::create_dir_all(&cwd).unwrap();

    let got = load_agents_md(&cwd).unwrap();
    let want = format!(
        "<agents_md path={}>\nroot rules\n</agents_md>\n\n<agents_md path={}>\nsvc rules\n</agents_md>",
        go_quote(&root.join("AGENTS.md").to_string_lossy()),
        go_quote(&root.join("svc/AGENTS.md").to_string_lossy()),
    );
    assert_eq!(got, want);
}

#[test]
fn load_agents_md_outside_repo_reads_only_cwd() {
    let parent = tempfile::tempdir().unwrap();
    let parent = eval_dir(parent.path());
    if has_git_ancestor(&parent) {
        // The temp dir sits inside a git checkout, so cwd is not outside a repo.
        return;
    }
    write(&parent.join("AGENTS.md"), b"parent rules");
    let cwd = parent.join("child");
    write(&cwd.join("AGENTS.md"), b"child rules");

    let got = load_agents_md(&cwd).unwrap();
    assert!(!got.contains("parent rules"), "got: {got}");
    assert!(got.contains("child rules"), "got: {got}");
}

#[test]
fn load_agents_md_none_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(load_agents_md(dir.path()).unwrap(), "");
}

#[test]
fn load_agents_md_truncates_on_rune_boundary() {
    let (_dir, cwd) = tmp_dir();
    // 3-byte runes; 32768 is not a multiple of 3, so a naive byte cut splits a rune.
    let content = "中".repeat(20000);
    write(&cwd.join("AGENTS.md"), content.as_bytes());

    let got = load_agents_md(&cwd).unwrap();
    assert!(got.is_char_boundary(got.len()), "must be valid UTF-8");
    assert!(
        got.contains("[AGENTS.md content truncated at 32768 bytes]"),
        "missing truncation note: {got}"
    );
    let kept = got.matches('中').count() * 3;
    assert!(kept <= 32768, "kept {kept} bytes of content, want <= 32768");
}

#[test]
fn load_agents_md_truncation_keeps_content_after_invalid_byte() {
    let (_dir, cwd) = tmp_dir();
    let mut data = b"rule-\xe9-".to_vec();
    data.extend(std::iter::repeat_n(b'x', 40000));
    write(&cwd.join("AGENTS.md"), &data);

    let got = load_agents_md(&cwd).unwrap();
    let kept = got.matches('x').count();
    assert!(
        kept >= 32768 - 16,
        "kept {kept} content bytes, want about 32768"
    );
}

#[test]
fn load_agents_md_read_error_fails() {
    let (_dir, cwd) = tmp_dir();
    // A directory named AGENTS.md makes os.ReadFile fail with a non-ENOENT error.
    std::fs::create_dir(cwd.join("AGENTS.md")).unwrap();
    let err = load_agents_md(&cwd).unwrap_err();
    assert!(err.message.contains("AGENTS.md"), "error: {err}");
}

#[test]
fn load_agents_md_resolves_symlinked_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    write(&real.join("AGENTS.md"), b"real rules");
    let link = dir.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&real, &link).unwrap();

    let got = load_agents_md(&link).unwrap();
    let real_agents = eval_dir(&real).join("AGENTS.md");
    assert!(
        got.contains(&go_quote(&real_agents.to_string_lossy())),
        "cwd symlink was not resolved: {got}"
    );
    assert!(!got.contains("link"), "symlink spelling leaked: {got}");
}

#[test]
fn system_prompt_appends_agents_md_then_developer() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("AGENTS.md"), b"project rules");

    let got = system_prompt(Some("BASE"), Some("DEV"), dir.path()).unwrap();
    let agents = load_agents_md(dir.path()).unwrap();
    assert_eq!(got, format!("BASE\n\n{agents}\n\nDEV"));
}

// ---------------------------------------------------------------------------
// Session creation: UUID v4 IDs, defaults, bound cwd, roots, bind errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_uses_uuid_v4_and_deepseek_defaults() {
    let m = SessionManager::new();
    let a = m.create(options()).await.unwrap();
    let b = m.create(options()).await.unwrap();

    let ua = Uuid::parse_str(&a.id).unwrap();
    let ub = Uuid::parse_str(&b.id).unwrap();
    assert_eq!(ua.get_version_num(), 4, "id {} is not v4", a.id);
    assert_eq!(ub.get_version_num(), 4, "id {} is not v4", b.id);
    assert_ne!(a.id, b.id, "session ids must be unique");

    assert_eq!(a.options.model, "deepseek-v4-pro");
    assert_eq!(a.options.reasoning_effort, "high");
    assert_eq!(a.options.effort_sent.as_deref(), Some("high"));
    assert_eq!(a.options.max_turns, DEFAULT_MAX_TURNS);
    assert_eq!(a.options.max_turns, 50);
    assert_eq!(a.options.system_prompt, DEFAULT_SYSTEM_PROMPT);
}

#[tokio::test]
async fn create_honors_explicit_values() {
    let m = SessionManager::new();
    let mut opts = options();
    opts.model = "my-model".into();
    opts.reasoning_effort = "low".into();
    opts.effort_sent = Some("max".into());
    opts.max_turns = 7;
    opts.system_prompt = "custom".into();
    let s = m.create(opts).await.unwrap();
    assert_eq!(s.options.model, "my-model");
    assert_eq!(s.options.reasoning_effort, "low");
    assert_eq!(s.options.effort_sent.as_deref(), Some("max"));
    assert_eq!(s.options.max_turns, 7);
    assert_eq!(s.options.system_prompt, "custom");
}

#[tokio::test]
async fn acquire_unknown_session_and_busy_errors() {
    let m = SessionManager::new();
    let s = m.create(options()).await.unwrap();

    assert_eq!(
        m.acquire("missing").await.unwrap_err(),
        SessionError::UnknownSession
    );
    assert_eq!(SessionError::UnknownSession.to_string(), "unknown session");

    let lease = m.acquire(&s.id).await.unwrap();
    assert_eq!(lease.id(), s.id);
    assert_eq!(lease.session().id, s.id);
    assert_eq!(lease.options().model, "deepseek-v4-pro");

    assert_eq!(m.acquire(&s.id).await.unwrap_err(), SessionError::Busy);
    assert_eq!(SessionError::Busy.to_string(), "thread is busy");

    drop(lease);
    // After the lease is released, the session is acquirable again.
    let lease2 = m.acquire(&s.id).await.unwrap();
    assert_eq!(lease2.id(), s.id);
}

#[tokio::test]
async fn lease_deref_mut_persists_state_and_blocks_second_acquire() {
    let m = SessionManager::new();
    let s = m.create(options()).await.unwrap();

    let mut lease = m.acquire(&s.id).await.unwrap();
    // Mutate SessionState through DerefMut.
    lease.messages.push(Message {
        role: Role::User,
        text: "hello".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }],
        tool_call_id: None,
        is_error: false,
        opaque: None,
    });
    lease.history_bytes += 42;
    lease.total_usage = Usage {
        input: 3,
        cached: 0,
        output: 5,
        reasoning: 0,
        total: 8,
    };

    // A second acquire while the first lease is alive returns the busy error.
    assert_eq!(m.acquire(&s.id).await.unwrap_err(), SessionError::Busy);
    drop(lease);

    // Mutations persisted into the shared SessionState.
    let lease = m.acquire(&s.id).await.unwrap();
    assert_eq!(lease.messages.len(), 1);
    assert_eq!(lease.messages[0].text, "hello");
    assert_eq!(lease.history_bytes, 42);
    assert_eq!(lease.total_usage.total, 8);
}

#[tokio::test]
async fn idle_ttl_eviction_and_busy_session_never_evicted() {
    let m = SessionManager::new();
    let acquired = m.create(options()).await.unwrap();
    let expired = m.create(options()).await.unwrap();
    let fresh = m.create(options()).await.unwrap();

    let old = tokio::time::Instant::now() - Duration::from_secs(25 * 3600);
    // Age both `acquired` and `expired` past 24h; keep `fresh` untouched.
    {
        let mut l = m.acquire(&acquired.id).await.unwrap();
        l.last_used = old;
        // Keep this lease alive: the session stays busy.
        {
            let mut l2 = m.acquire(&expired.id).await.unwrap();
            l2.last_used = old;
        } // drop expired lease -> expired is now idle

        // Trigger the eviction sweep by creating a new session.
        let new_session = m.create(options()).await.unwrap();
        assert_eq!(new_session.options.model, "deepseek-v4-pro");

        // `acquired` was expired but busy: it must NOT be evicted.
        assert_eq!(
            m.acquire(&acquired.id).await.unwrap_err(),
            SessionError::Busy,
            "a busy session was handed out / evicted"
        );
    } // drop `l`

    // After releasing the busy lease, `acquired` is still present (not evicted).
    assert!(m.acquire(&acquired.id).await.is_ok());
    // `expired` (idle + older than 24h) was evicted.
    assert_eq!(
        m.acquire(&expired.id).await.unwrap_err(),
        SessionError::UnknownSession
    );
    // `fresh` (idle but within TTL) was not evicted.
    assert!(m.acquire(&fresh.id).await.is_ok());
}

#[tokio::test]
async fn cap_eviction_never_exceeds_256_idle_sessions() {
    let m = SessionManager::new();
    let oldest = m.create(options()).await.unwrap();

    // Make `oldest` strictly the oldest idle session.
    {
        let mut l = m.acquire(&oldest.id).await.unwrap();
        l.last_used = tokio::time::Instant::now() - Duration::from_secs(3600);
    }

    // Fill up to 256 sessions, then insert one more (the sweep must free a slot).
    let mut created = Vec::new();
    for _ in 0..255 {
        created.push(m.create(options()).await.unwrap());
    }
    assert_eq!(created.len() + 1, 256);

    let last = m.create(options()).await.unwrap();
    // The oldest idle session was evicted to make room.
    assert_eq!(
        m.acquire(&oldest.id).await.unwrap_err(),
        SessionError::UnknownSession
    );
    assert!(m.acquire(&last.id).await.is_ok());
    // The 255 middle sessions (not oldest) remain.
    for s in &created {
        assert!(m.acquire(&s.id).await.is_ok());
    }
}

/// Go `TestManagerSweepDoesNotCauseFalseBusy`. The eviction sweep must never
/// make an idle, fresh session look busy: neither an acquire immediately after
/// a sweep nor one racing a sweep concurrently may see a false `ErrBusy` from
/// the sweep's `try_lock_owned` probe.
///
/// Determinism: no sleeps. In Rust `create` (which runs `evict_locked`) holds
/// the manager lock for the whole sweep and drops every probe guard before
/// releasing it; `acquire` also takes the manager lock before its own
/// `try_lock_owned`, so the two are serialized. The outcome (busy == 0) is
/// therefore guaranteed for every interleaving. Total sessions stay below the
/// 256 cap so the target is never evicted (only false-busy is under test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_does_not_cause_false_busy() {
    let manager = Arc::new(SessionManager::new());
    let target = manager.create(options()).await.unwrap();
    // Filler sessions give evict_locked real work to probe.
    for _ in 0..80 {
        manager.create(options()).await.unwrap();
    }

    // "Immediately after": a sweep directly before each acquire. Every probe
    // guard must already be gone, so acquire of the idle target succeeds.
    for _ in 0..10 {
        manager.create(options()).await.unwrap();
        assert!(
            manager.acquire(&target.id).await.is_ok(),
            "false Busy right after a sweep"
        );
    }

    // "While an acquire is in flight": a sweeper task and an acquirer loop run
    // on two worker threads, genuinely contending for the manager lock.
    let sweep_manager = manager.clone();
    let sweeper = tokio::spawn(async move {
        let mut sweeps = 0u32;
        for _ in 0..100 {
            sweep_manager.create(options()).await.unwrap();
            sweeps += 1;
            tokio::task::yield_now().await;
        }
        sweeps
    });

    let mut busy = 0u32;
    for _ in 0..1000 {
        match manager.acquire(&target.id).await {
            Ok(lease) => drop(lease),
            Err(SessionError::Busy) => busy += 1,
            Err(e) => panic!("unexpected acquire error: {e}"),
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(sweeper.await.unwrap(), 100, "sweeper must run every sweep");
    assert_eq!(busy, 0, "acquire saw a false Busy {busy} times");
}

#[tokio::test]
async fn close_cancels_token_and_clears_sessions() {
    let m = SessionManager::new();
    let a = m.create(options()).await.unwrap();
    let b = m.create(options()).await.unwrap();
    let cancel = CancellationToken::new();
    m.close(&cancel).await;
    assert!(cancel.is_cancelled());
    assert_eq!(
        m.acquire(&a.id).await.unwrap_err(),
        SessionError::UnknownSession
    );
    assert_eq!(
        m.acquire(&b.id).await.unwrap_err(),
        SessionError::UnknownSession
    );
}

#[tokio::test]
async fn bound_cwd_is_set_and_missing_root_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = eval_dir(dir.path());

    // A valid workspace-write cwd binds a cwd directory and default roots.
    let m = SessionManager::new();
    let mut opts = options();
    opts.cwd = cwd.clone();
    opts.sandbox = Sandbox::WorkspaceWrite;
    let s = m.create(opts).await.unwrap();
    {
        let lease = m.acquire(&s.id).await.unwrap();
        assert!(lease.bound_cwd.is_some(), "cwd was not bound");
        assert_eq!(lease.bound_cwd.as_ref().unwrap().path, cwd);
        assert!(lease.bind_err.is_none());
        assert!(lease.root_paths.iter().any(|p| p == &cwd));
        assert!(lease.root_paths.iter().any(|p| p == Path::new("/tmp")));
        assert_eq!(
            lease.bound_roots.len(),
            lease.root_paths.len(),
            "bound_roots and root_paths must be 1:1"
        );
    }

    // A missing configured writable root fails closed with a bind error and no
    // bound cwd (Go `TestSessionMissingBoundRootFailsClosed`).
    let missing = dir.path().join("missing");
    let mut bad = options();
    bad.cwd = cwd.clone();
    bad.sandbox = Sandbox::WorkspaceWrite;
    bad.writable_roots = vec![missing];
    let s2 = m.create(bad).await.unwrap();
    {
        let lease = m.acquire(&s2.id).await.unwrap();
        assert!(lease.bound_cwd.is_none(), "bound cwd must be cleared");
        assert!(lease.bound_roots.is_empty(), "held roots must be dropped");
        assert!(lease.root_paths.is_empty(), "root_paths must be cleared");
        let err = lease.bind_err.as_deref().unwrap_or("");
        assert!(err.contains("bind writable root"), "bind_err: {err}");
    }
}

#[tokio::test]
async fn bound_cwd_accepts_symlinked_cwd() {
    // A cwd that is itself a symlink must still bind (Go `BindDirectory` opens
    // the target inode); AGENTS.md discovery resolves the symlink spelling.
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&real, &link).unwrap();

    let m = SessionManager::new();
    let mut opts = options();
    opts.cwd = link.clone();
    opts.sandbox = Sandbox::WorkspaceWrite;
    let s = m.create(opts).await.unwrap();
    let lease = m.acquire(&s.id).await.unwrap();
    assert!(lease.bound_cwd.is_some(), "symlinked cwd was not bound");
    assert!(lease.bind_err.is_none());
    // The bound directory still names the original inode (Go `Check`).
    assert!(lease.bound_cwd.as_ref().unwrap().check().is_ok());
    assert!(lease.root_paths.iter().any(|p| p == &link));
    assert_eq!(lease.bound_roots.len(), lease.root_paths.len());
}

// ---------------------------------------------------------------------------
// I2: writable roots are HELD as fds, deduped by held inode identity, and a
// root swapped after creation is detected (TOCTOU-free).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bound_roots_are_held_and_match_root_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = eval_dir(dir.path());
    let extra = dir.path().join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    let extra = eval_dir(&extra);

    let m = SessionManager::new();
    let mut opts = options();
    opts.cwd = cwd.clone();
    opts.sandbox = Sandbox::WorkspaceWrite;
    opts.writable_roots = vec![extra.clone()];
    let s = m.create(opts).await.unwrap();
    let lease = m.acquire(&s.id).await.unwrap();

    assert!(lease.bound_cwd.is_some());
    assert!(lease.bind_err.is_none());
    // bound_roots is 1:1 with root_paths (Go boundRoots/rootPaths).
    assert_eq!(lease.bound_roots.len(), lease.root_paths.len());
    assert!(lease.root_paths.contains(&cwd));
    assert!(lease.root_paths.contains(&extra));
    assert!(lease.root_paths.iter().any(|p| p == Path::new("/tmp")));

    // Every root is a HELD handle, not a re-stat'able path. On Linux path() is
    // /proc/self/fd/<fd>, which resolves only while that fd stays open.
    #[cfg(target_os = "linux")]
    {
        for root in &lease.bound_roots {
            assert!(
                std::fs::metadata(root.path()).is_ok(),
                "bound root fd is not held/open"
            );
            assert!(root.raw_fd() >= 0, "invalid held root fd");
        }
        // cwd reuse: bound_roots[0] shares the cwd handle (Go reuses boundCwd)
        // — same raw fd, no second open, no double-close.
        assert_eq!(
            lease.bound_roots[0].raw_fd(),
            lease.bound_cwd.as_ref().unwrap().raw_fd(),
            "cwd was opened a second time instead of being reused"
        );
    }
}

#[tokio::test]
async fn bound_roots_dedupe_by_held_identity() {
    // A configured root with a different spelling of the cwd inode (a symlink)
    // must collapse into ONE held root: dedup uses the held inodes, not a
    // later re-stat of mutable path names.
    let dir = tempfile::tempdir().unwrap();
    let cwd = eval_dir(dir.path());
    let link = dir.path().join("cwd-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&cwd, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&cwd, &link).unwrap();

    let m = SessionManager::new();
    let mut opts = options();
    opts.cwd = cwd.clone();
    opts.sandbox = Sandbox::WorkspaceWrite;
    // cwd spelled twice + a symlink to the same inode: all must dedup to one.
    opts.writable_roots = vec![cwd.clone(), link.clone()];
    let s = m.create(opts).await.unwrap();
    let lease = m.acquire(&s.id).await.unwrap();

    assert_eq!(lease.bound_roots.len(), lease.root_paths.len());
    assert_eq!(
        lease.root_paths.iter().filter(|p| *p == &cwd).count(),
        1,
        "cwd spelling deduped once"
    );
    assert!(
        !lease.root_paths.contains(&link),
        "symlink to the cwd inode must dedup away"
    );
    assert!(lease.root_paths.iter().any(|p| p == Path::new("/tmp")));
    assert!(lease.root_paths.len() >= 2, "cwd + /tmp at minimum");
    // cwd handle is shared, never opened a second time.
    #[cfg(target_os = "linux")]
    assert_eq!(
        lease.bound_roots[0].raw_fd(),
        lease.bound_cwd.as_ref().unwrap().raw_fd(),
    );
}

#[tokio::test]
async fn bound_root_swap_is_detected() {
    // A writable root swapped (renamed + symlinked elsewhere) AFTER creation
    // must be detected: the held fd still names the original inode, so
    // check() fails and the swapped path is never silently trusted.
    let dir = tempfile::tempdir().unwrap();
    let base = eval_dir(dir.path());
    let cwd = base.join("cwd");
    let root = base.join("root");
    let protected = base.join("protected");
    for d in [&cwd, &root, &protected] {
        std::fs::create_dir_all(d).unwrap();
    }

    let m = SessionManager::new();
    let mut opts = options();
    opts.cwd = cwd.clone();
    opts.sandbox = Sandbox::WorkspaceWrite;
    opts.writable_roots = vec![root.clone()];
    let s = m.create(opts).await.unwrap();
    let lease = m.acquire(&s.id).await.unwrap();

    let idx = lease
        .root_paths
        .iter()
        .position(|p| p == &root)
        .expect("configured root present in root_paths");
    let held = Arc::clone(&lease.bound_roots[idx]);
    assert!(held.check().is_ok(), "fresh held root passes check()");

    // On Linux, held.path() is a proc-fd magic link to the held inode; capture
    // its identity before the swap to prove the fd does not follow the path.
    #[cfg(target_os = "linux")]
    let held_ino = {
        use std::os::unix::fs::MetadataExt;
        let st = std::fs::metadata(held.path()).unwrap();
        (st.dev(), st.ino())
    };

    // Swap: rename root -> root.bak, then point `root` at `protected`.
    std::fs::rename(&root, base.join("root.bak")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&protected, &root).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&protected, &root).unwrap();

    // check() re-stats the original path name and must now MISMATCH the held
    // inode: the swapped root is detected, not silently trusted.
    assert!(held.check().is_err(), "swapped root must be detected");
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        // The held fd still resolves to the ORIGINAL inode (TOCTOU-free).
        let st = std::fs::metadata(held.path()).unwrap();
        assert_eq!(
            (st.dev(), st.ino()),
            held_ino,
            "held fd must not follow the swap"
        );
    }
}

#[test]
fn effective_writable_roots_dedup_by_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = eval_dir(dir.path());
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = eval_dir(&other);

    // cwd is duplicated (as first default and as a configured root) and must
    // appear exactly once; /tmp and a distinct configured dir must appear.
    let roots = effective_writable_roots(&cwd, &[cwd.clone(), other.clone()]);
    assert_eq!(roots.iter().filter(|p| *p == &cwd).count(), 1);
    assert!(roots.iter().any(|p| p == Path::new("/tmp")));
    assert!(roots.iter().any(|p| p == &other));
}

// ---------------------------------------------------------------------------
// Public surface sanity: Emitter / AgentError / trait objects
// ---------------------------------------------------------------------------

#[test]
fn agent_error_display_and_emitter_trait_object() {
    let err = AgentError {
        message: "boom".into(),
    };
    assert_eq!(err.to_string(), "boom");
    struct Rec;
    impl subagent_mcp::agent::Emitter for Rec {
        fn emit(&self, _thread_id: &str, _event: serde_json::Value) {}
    }
    let _emitter: Arc<dyn subagent_mcp::agent::Emitter> = Arc::new(Rec);
}

#[test]
fn thread_is_a_first_class_trait_object() {
    let t: Arc<dyn Thread> = Arc::new(FakeThread);
    // prove the Arc<dyn Thread> can be stored in SessionOptions
    let mut opts = options();
    opts.thread = Some(t);
    assert!(opts.thread.is_some());
}
