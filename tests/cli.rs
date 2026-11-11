#[test]
fn version_prints_expected() {
    for flag in ["--version", "-version"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_subagent-mcp"))
            .arg(flag)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "subagent-mcp 0.14.0\n"
        );
    }
}
