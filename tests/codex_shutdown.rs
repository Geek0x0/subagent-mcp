#![cfg(unix)]
//! Isolated integration binary: production shutdown closes every global pool.

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use subagent_mcp::provider::codex::pool::{self, Pool};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn production_shutdown_allows_eof_flush_and_kills_stubborn_child() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app-server.sh");
    let marker = dir.path().join("eof-marker");
    let pid_file = dir.path().join("pid");
    let script = format!(
        "{}\nsleep 0.1\nprintf flushed > '{}'\n",
        include_str!("fixtures/codex/app-server.sh"),
        marker.display()
    )
    .replacen(
        "IFS= read",
        &format!("echo $$ > '{}'\nIFS= read", pid_file.display()),
        1,
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    };
    let command = path.to_string_lossy().into_owned();
    let pool = Pool::for_command(&command);
    let conn = tokio::time::timeout(Duration::from_secs(10), pool.get(&ctx))
        .await
        .unwrap()
        .unwrap();
    pool::shutdown();
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "flushed");
    assert!(conn.client.is_done());
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "graceful child must be reaped"
    );
    pool::remove_pool(&command);

    let script = include_str!("fixtures/codex/app-server-hold.sh").replacen(
        "IFS= read",
        &format!("echo $$ > '{}'\nIFS= read", pid_file.display()),
        1,
    );
    std::fs::write(&path, script).unwrap();
    let pool = Pool::for_command(&command);
    let conn = tokio::time::timeout(Duration::from_secs(10), pool.get(&ctx))
        .await
        .unwrap()
        .unwrap();
    let start = Instant::now();
    pool::shutdown();
    assert!(start.elapsed() >= Duration::from_secs(5));
    assert!(start.elapsed() < Duration::from_secs(8));
    assert!(conn.client.is_done());
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "stubborn child must be reaped"
    );
    pool::remove_pool(&command);
}
