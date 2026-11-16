//! Project resolution and the `opencode api` client.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use subagent_mcp::config::OpenCodeConfig;
use subagent_mcp::rollout::opencode::OpenCodeError;
use subagent_mcp::rollout::opencode::api::{ApiClient, MAX_BODY_BYTES, find_opencode};
use subagent_mcp::rollout::opencode::project::{ProjectRef, pick_project};

#[cfg(unix)]
#[path = "support/fake_opencode.rs"]
mod fake_opencode;

fn project(id: &str, canonical: &Path) -> ProjectRef {
    ProjectRef {
        id: id.to_string(),
        canonical: canonical.to_path_buf(),
    }
}

// --- pick_project -----------------------------------------------------------

#[test]
fn pick_project_prefers_exact_git_root_match() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sub = root.join("pkg");
    std::fs::create_dir(&sub).unwrap();
    let projects = vec![project("nested", &sub), project("root", &root)];
    // The session's git root is `root`, so `root` wins over the nested project.
    let picked = pick_project(&sub, Some(&root), &projects).unwrap();
    assert_eq!(picked.id, "root");
    // Without a git root the exact cwd match wins.
    assert_eq!(pick_project(&sub, None, &projects).unwrap().id, "nested");
}

#[test]
fn pick_project_falls_back_to_nearest_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let deep = root.join("a/b/c");
    std::fs::create_dir_all(&deep).unwrap();
    let projects = vec![project("far", &root), project("near", &root.join("a"))];
    assert_eq!(pick_project(&deep, None, &projects).unwrap().id, "near");
}

#[test]
fn pick_project_never_returns_the_global_project() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let projects = vec![project("global", Path::new("/"))];
    assert!(pick_project(&cwd, None, &projects).is_none());
}

#[cfg(unix)]
#[test]
fn pick_project_resolves_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let projects = vec![project("p", &real)];
    assert_eq!(pick_project(&link, None, &projects).unwrap().id, "p");
}

// --- find_opencode ------------------------------------------------------------

#[test]
fn find_opencode_uses_only_an_existing_configured_path() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("opencode");
    std::fs::write(&bin, "").unwrap();
    let present = OpenCodeConfig {
        bin: Some(bin.clone()),
        ..Default::default()
    };
    assert_eq!(find_opencode(&present), Some(bin));
    let missing = OpenCodeConfig {
        bin: Some(dir.path().join("missing")),
        ..Default::default()
    };
    assert_eq!(find_opencode(&missing), None);
}

// --- ApiClient (fake executable) ---------------------------------------------

#[tokio::test]
async fn missing_binary_is_not_found() {
    let client = ApiClient::new(
        PathBuf::from("/nonexistent/opencode"),
        Duration::from_secs(5),
    );
    assert_eq!(
        client.list_projects().await.unwrap_err(),
        OpenCodeError::NotFound
    );
}

#[cfg(unix)]
mod with_fake {
    use super::*;
    use fake_opencode::Fake;

    fn client(fake: &Fake, timeout: Duration) -> ApiClient {
        ApiClient::new(fake.bin.clone(), timeout)
    }

    #[tokio::test]
    async fn list_projects_parses_the_cli_output() {
        let fake = Fake::new().reply(
            0,
            r#"[{"id":"p1","canonical":"/work/a","vcs":"git"},{"id":"global","canonical":"/"},{"bad":1}]"#,
            "",
            0,
        );
        let projects = client(&fake, Duration::from_secs(5))
            .list_projects()
            .await
            .unwrap();
        assert_eq!(
            projects,
            vec![
                project("p1", Path::new("/work/a")),
                project("global", Path::new("/"))
            ]
        );
        assert_eq!(fake.calls(), vec![vec!["api", "GET", "/api/project"]]);
    }

    #[tokio::test]
    async fn import_sends_the_documented_arguments() {
        let fake = Fake::new().reply(0, r#"{"data":{"id":"ses_1"}}"#, "", 0);
        let body = json!({"info": {"id": "ses_1"}, "messages": []});
        client(&fake, Duration::from_secs(5))
            .import(&body)
            .await
            .unwrap();
        assert_eq!(
            fake.calls(),
            vec![vec![
                "api".to_string(),
                "POST".to_string(),
                "/api/experimental/session/import".to_string(),
                "-H".to_string(),
                "content-type: application/json".to_string(),
                "-d".to_string(),
                body.to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn import_conflict_is_status_409() {
        let fake = Fake::new().reply(
            0,
            r#"{"_tag":"ConflictError","message":"Session already exists"}"#,
            "HTTP 409 Conflict",
            1,
        );
        let err = client(&fake, Duration::from_secs(5))
            .import(&json!({"info": {}, "messages": []}))
            .await
            .unwrap_err();
        assert!(matches!(err, OpenCodeError::Status(409, _)), "{err:?}");
    }

    #[tokio::test]
    async fn a_failure_without_an_http_status_is_other() {
        let fake = Fake::new().reply(0, "", "Error: Server requires a password", 1);
        let err = client(&fake, Duration::from_secs(5))
            .list_projects()
            .await
            .unwrap_err();
        assert!(matches!(err, OpenCodeError::Other(_)), "{err:?}");
    }

    #[tokio::test]
    async fn body_over_the_argument_limit_is_too_large_without_spawning() {
        let fake = Fake::new();
        let body = json!({"info": {}, "messages": ["x".repeat(MAX_BODY_BYTES + 1)]});
        let err = client(&fake, Duration::from_secs(5))
            .import(&body)
            .await
            .unwrap_err();
        assert_eq!(err, OpenCodeError::TooLarge);
        assert!(fake.calls().is_empty(), "opencode must not be spawned");
    }

    #[tokio::test]
    async fn timeout_is_reported() {
        let fake = Fake::new().sleep(0, "5").reply(0, "[]", "", 0);
        let err = client(&fake, Duration::from_millis(300))
            .list_projects()
            .await
            .unwrap_err();
        assert_eq!(err, OpenCodeError::Timeout);
    }

    #[tokio::test]
    async fn version_parses_the_cli_output() {
        let fake = Fake::new().version("opencode v2.3.0\n");
        let version = client(&fake, Duration::from_secs(5))
            .version()
            .await
            .unwrap();
        assert_eq!(version, "2.3.0");
        assert_eq!(fake.calls(), vec![vec!["--version"]]);
        let empty = Fake::new().version("");
        assert!(
            client(&empty, Duration::from_secs(5))
                .version()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn delete_maps_session_not_found_to_404() {
        // `opencode session delete <missing>` prints this on stderr and exits 1.
        let fake = Fake::new().reply(0, "", "Session not found: ses_x", 1);
        let err = client(&fake, Duration::from_secs(5))
            .delete("ses_x")
            .await
            .unwrap_err();
        assert!(matches!(err, OpenCodeError::Status(404, _)), "{err:?}");
        let other = Fake::new().reply(0, "", "disk exploded", 1);
        let err = client(&other, Duration::from_secs(5))
            .delete("ses_x")
            .await
            .unwrap_err();
        assert!(matches!(err, OpenCodeError::Other(_)), "{err:?}");
    }

    #[tokio::test]
    async fn delete_runs_session_delete() {
        let fake = Fake::new().reply(0, "Session ses_1 deleted", "", 0);
        client(&fake, Duration::from_secs(5))
            .delete("ses_1")
            .await
            .unwrap();
        assert_eq!(fake.calls(), vec![vec!["session", "delete", "ses_1"]]);
    }
}
