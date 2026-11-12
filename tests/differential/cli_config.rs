//! CLI and `--check-config` differential smoke tests.
//!
//! These run both binaries with the same argv[0], environment, working
//! directory, and config path, then compare the exit code and normalized
//! stdout/stderr. The reference binary is the pinned Go 0.13.1 release.

use std::path::Path;

use crate::support::{BaselineManifest, normalize, reference_or_skip, run_cli, rust_binary};

fn assert_cli_matches(args: &[&str], envs: &[(&str, &str)], home: &Path) {
    let Some(reference) = reference_or_skip() else {
        return;
    };

    let go = run_cli(&reference.path(), args, envs, home);
    let rust = run_cli(Path::new(rust_binary()), args, envs, home);

    let go_ctx = normalize::NormalizeContext::new().with_temp_root(home);
    let rust_ctx = normalize::NormalizeContext::new().with_temp_root(home);

    let go_stdout = normalize::text(&String::from_utf8_lossy(&go.stdout), &go_ctx);
    let rust_stdout = normalize::text(&String::from_utf8_lossy(&rust.stdout), &rust_ctx);
    let go_stderr = normalize::text(&String::from_utf8_lossy(&go.stderr), &go_ctx);
    let rust_stderr = normalize::text(&String::from_utf8_lossy(&rust.stderr), &rust_ctx);

    assert_eq!(
        rust.status.code(),
        go.status.code(),
        "exit code differs for {args:?}\nGo stdout:\n{}\nRust stdout:\n{}\nGo stderr:\n{}\nRust stderr:\n{}",
        String::from_utf8_lossy(&go.stdout),
        String::from_utf8_lossy(&rust.stdout),
        String::from_utf8_lossy(&go.stderr),
        String::from_utf8_lossy(&rust.stderr),
    );
    assert_eq!(
        rust_stdout,
        go_stdout,
        "normalized stdout differs for {args:?}\nGo raw stdout:\n{}\nRust raw stdout:\n{}",
        String::from_utf8_lossy(&go.stdout),
        String::from_utf8_lossy(&rust.stdout),
    );
    assert_eq!(
        rust_stderr,
        go_stderr,
        "normalized stderr differs for {args:?}\nGo raw stderr:\n{}\nRust raw stderr:\n{}",
        String::from_utf8_lossy(&go.stderr),
        String::from_utf8_lossy(&rust.stderr),
    );
}

#[test]
fn differential_baseline_manifest_pins_global_constraints() {
    let manifest = BaselineManifest::pinned().expect("tests/differential/baseline.lock parses");

    assert_eq!(manifest.version, "0.13.1");
    assert_eq!(manifest.commit, "fe563a12ad30046c0d5be27fdddf9f18d39e8e47");
    assert_eq!(manifest.repo, "Geek0x0/subagent-mcp");
    assert_eq!(manifest.artifact, "subagent-mcp_0.13.1_linux_amd64.tar.gz");
    assert_eq!(
        manifest.sha256,
        "7ead1f164a1c8e05a4cf5e5d2f22f19746ffa5f315f44f13b0b505c6034f13dc"
    );
    assert_eq!(manifest.binary, "subagent-mcp");
    assert_eq!(
        manifest.artifact_url(),
        "https://github.com/Geek0x0/subagent-mcp/releases/download/v0.13.1/subagent-mcp_0.13.1_linux_amd64.tar.gz"
    );
}

#[test]
fn differential_baseline_artifact_checksum_matches() {
    let Some(reference) = reference_or_skip() else {
        return;
    };
    reference
        .verify_checksum()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(reference.manifest().version, "0.13.1");
    assert!(
        reference.archive_path().is_file(),
        "archive path = {}",
        reference.archive_path().display()
    );

    let home = tempfile::tempdir().unwrap();
    let output = run_cli(&reference.path(), &["--version"], &[], home.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr = {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "subagent-mcp 0.13.1\n"
    );
}

#[test]
fn differential_version_matches_reference() {
    let home = tempfile::tempdir().unwrap();
    assert_cli_matches(&["--version"], &[], home.path());
}

#[test]
fn differential_unknown_flag_matches_reference() {
    let home = tempfile::tempdir().unwrap();
    assert_cli_matches(&["--bogus"], &[], home.path());
}

#[test]
fn differential_check_config_missing_file_matches_reference() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing.toml");
    assert_cli_matches(
        &["--check-config", missing.to_str().unwrap()],
        &[],
        home.path(),
    );
}

#[test]
fn differential_check_config_unset_key_matches_reference() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
[providers.test]
api = "chat-completions"
base_url = "https://api.invalid"
env_key = "SUBAGENT_DIFFERENTIAL_UNSET_KEY"
default_model = "test-model"
models = [{ id = "test-model" }]
"#,
    )
    .unwrap();

    assert_cli_matches(
        &["--check-config", config.to_str().unwrap()],
        &[],
        home.path(),
    );
}
