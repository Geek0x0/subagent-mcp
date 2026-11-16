//! Backend selection for the rollout recorder.
//!
//! These tests mutate process-wide environment variables, so they live in their
//! own test binary and are serialized under one mutex.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;
use subagent_mcp::config::{OpenCodeConfig, RolloutConfig, RolloutFormat};
use subagent_mcp::rollout::RolloutRecorder;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn new() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved = ["CODEX_HOME", "SUBAGENT_MCP_ROLLOUT"]
            .iter()
            .map(|&key| (key, std::env::var(key).ok()))
            .collect();
        Self { saved, _lock: lock }
    }

    fn set(&self, key: &str, value: &str) {
        // SAFETY: env mutation is serialized under ENV_LOCK for the whole test.
        unsafe { std::env::set_var(key, value) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: still holding ENV_LOCK.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn config(format: RolloutFormat) -> RolloutConfig {
    RolloutConfig {
        format,
        opencode: OpenCodeConfig::default(),
    }
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// The parsed rollout lines with the volatile timestamp removed.
fn normalized_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).unwrap();
            value.as_object_mut().unwrap().remove("timestamp");
            value
        })
        .collect()
}

#[test]
fn codex_format_creates_the_same_file_as_before() {
    let env = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    env.set("CODEX_HOME", home.path().to_str().unwrap());
    env.set("SUBAGENT_MCP_ROLLOUT", "");
    let cwd = Path::new("/tmp");

    let mut old = RolloutRecorder::create("thread-old").unwrap();
    old.record_error("boom");
    let mut new =
        RolloutRecorder::create_with("thread-new", &config(RolloutFormat::Codex), cwd).unwrap();
    new.record_error("boom");

    let files = jsonl_files(home.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let named = |id: &str| {
        files
            .iter()
            .find(|f| {
                f.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with(&format!("-{id}.jsonl"))
            })
            .unwrap_or_else(|| panic!("no rollout file for {id}: {files:?}"))
            .clone()
    };
    assert!(
        named("thread-new")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("rollout-")
    );
    assert_eq!(
        normalized_lines(&named("thread-old")),
        normalized_lines(&named("thread-new")),
        "create_with(Codex) must write exactly what create() writes"
    );
}

#[test]
fn opencode_format_creates_no_codex_file() {
    let env = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    env.set("CODEX_HOME", home.path().to_str().unwrap());
    env.set("SUBAGENT_MCP_ROLLOUT", "");

    let mut recorder = RolloutRecorder::create_with(
        "thread-oc",
        &config(RolloutFormat::OpenCode),
        Path::new("/tmp"),
    )
    .unwrap();
    recorder.record_error("boom");

    let entries: Vec<_> = fs::read_dir(home.path()).unwrap().collect();
    assert!(
        entries.is_empty(),
        "the OpenCode backend must not touch CODEX_HOME: {entries:?}"
    );
}

#[test]
fn rollout_off_disables_opencode_too() {
    let env = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    env.set("CODEX_HOME", home.path().to_str().unwrap());
    env.set("SUBAGENT_MCP_ROLLOUT", "off");

    for format in [RolloutFormat::Codex, RolloutFormat::OpenCode] {
        let result = RolloutRecorder::create_with("thread-off", &config(format), Path::new("/tmp"));
        assert!(result.is_err(), "{format:?} must be disabled by =off");
    }
    assert!(RolloutRecorder::disabled_by_env());
    assert!(jsonl_files(home.path()).is_empty());
}

#[test]
fn run_ended_is_a_noop_for_codex() {
    let env = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    env.set("CODEX_HOME", home.path().to_str().unwrap());
    env.set("SUBAGENT_MCP_ROLLOUT", "");

    let mut recorder = RolloutRecorder::create_with(
        "thread-noop",
        &config(RolloutFormat::Codex),
        Path::new("/tmp"),
    )
    .unwrap();
    recorder.record_error("boom");
    let path = jsonl_files(home.path()).pop().unwrap();
    let before = fs::read(&path).unwrap();

    recorder.run_ended();
    recorder.note_tool_error("call-1", true);

    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "Codex file must not change"
    );
    assert!(recorder.take_warning().is_none());
}
