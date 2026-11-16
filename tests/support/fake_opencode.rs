//! A fake `opencode` executable for tests (Unix only).
//!
//! It is a tiny shell script in a temp directory. Every call appends its
//! arguments to a log (`--- call <n>` followed by one `ARG:<arg>` line per
//! argument) and then plays back the n-th scripted reply (stdout, stderr, exit
//! code, an optional sleep in seconds), so tests can assert the exact arguments
//! and the order of calls.

#![cfg(unix)]
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub struct Fake {
    dir: tempfile::TempDir,
    pub bin: PathBuf,
}

impl Fake {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencode");
        let d = dir.path().display();
        let script = format!(
            r#"#!/bin/sh
d="{d}"
if [ "$1" = "--version" ]; then
  echo "--- call v" >> "$d/log"
  printf 'ARG:%s\n' "$@" >> "$d/log"
  if [ -f "$d/version" ]; then cat "$d/version"; else echo "opencode v2.3.0"; fi
  exit 0
fi
n=$(cat "$d/n" 2>/dev/null || echo 0)
echo $((n+1)) > "$d/n"
echo "--- call $n" >> "$d/log"
for a in "$@"; do printf 'ARG:%s\n' "$a" >> "$d/log"; done
if [ -f "$d/r$n.sleep" ]; then sleep "$(cat "$d/r$n.sleep")"; fi
if [ -f "$d/r$n.out" ]; then cat "$d/r$n.out"; fi
if [ -f "$d/r$n.err" ]; then cat "$d/r$n.err" >&2; fi
: > "$d/done$n"
exit "$(cat "$d/r$n.code" 2>/dev/null || echo 0)"
"#
        );
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, bin }
    }

    /// Script the reply of call number `n` (0-based).
    pub fn reply(self, n: usize, stdout: &str, stderr: &str, code: i32) -> Self {
        let d = self.dir.path();
        fs::write(d.join(format!("r{n}.out")), stdout).unwrap();
        fs::write(d.join(format!("r{n}.err")), stderr).unwrap();
        fs::write(d.join(format!("r{n}.code")), code.to_string()).unwrap();
        self
    }

    /// What `opencode --version` prints. A `--version` call never uses up a
    /// scripted reply (and is logged like any call, see [`Fake::calls`]).
    pub fn version(self, text: &str) -> Self {
        fs::write(self.dir.path().join("version"), text).unwrap();
        self
    }

    /// Make call number `n` sleep for `secs` seconds before replying.
    pub fn sleep(self, n: usize, secs: &str) -> Self {
        fs::write(self.dir.path().join(format!("r{n}.sleep")), secs).unwrap();
        self
    }

    /// Whether call number `n` ran to its end (a call that was killed, for
    /// example because the process shut down, never gets there).
    pub fn finished(&self, n: usize) -> bool {
        self.dir.path().join(format!("done{n}")).exists()
    }

    /// Like [`Fake::calls`] without the `--version` calls.
    pub fn api_calls(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| call != &vec!["--version".to_string()])
            .collect()
    }

    /// The arguments of every call so far, one `Vec` per call.
    pub fn calls(&self) -> Vec<Vec<String>> {
        let log = fs::read_to_string(self.dir.path().join("log")).unwrap_or_default();
        let mut calls: Vec<Vec<String>> = Vec::new();
        for line in log.lines() {
            if line.starts_with("--- call ") {
                calls.push(Vec::new());
            } else if let Some(arg) = line.strip_prefix("ARG:") {
                calls.last_mut().unwrap().push(arg.to_string());
            }
        }
        calls
    }
}
