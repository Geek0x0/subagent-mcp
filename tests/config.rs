use std::path::PathBuf;

use subagent_mcp::config;

// Port of internal/config/config_test.go (Go subagent-mcp v0.13.1). Expected
// strings are the exact Go error text with the temp file path replaced by the
// <PATH> token; assert_load_err substitutes the real path before comparing.
// Subagent-mcp-authored errors are asserted with assert_eq! (never `contains`).

const VALID_TOML: &str = include_str!("fixtures/config/valid.toml");

const CODEX_TOML: &str = r#"
[providers.codex]
api = "codex-app-server"
default_model = "gpt-6-astra"
models = [{ id = "gpt-6-astra" }]
"#;

const AUTO_PROVIDER_TOML: &str = r#"
[providers.deepseek]
api = "chat-completions"
env_key = "DS_TEST_KEY"
default_model = "deepseek-flash"
models = [{ id = "deepseek-flash" }]
"#;

const KEY_SECRET: &str = "sk-test-secret-value-9f3a";

fn write_config(content: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, content).unwrap();
    set_mode(&path, 0o600);
    (dir, path)
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) {}

fn repl(base: &str, old: &str, new: &str) -> String {
    base.replacen(old, new, 1)
}

fn chat(body: &str) -> String {
    format!(
        "[providers.gw]\napi = \"chat-completions\"\nenv_key = \"GW_KEY\"\ndefault_model = \"m\"\nmodels = [{{ id = \"m\" }}]\n{body}"
    )
}

fn with_auto(block: &str) -> String {
    format!("{AUTO_PROVIDER_TOML}{block}")
}

fn assert_load_err(cases: &[(&str, String, &str)]) {
    for (label, content, template) in cases {
        let (dir, path) = write_config(content);
        let err = config::load(&path).unwrap_err().to_string();
        let expected = template.replace("<PATH>", &path.display().to_string());
        assert_eq!(err, expected, "case: {label}");
        drop(dir);
    }
}

// ---------------------------------------------------------------------------
// Missing file, unknown keys, active_provider, TOML parse line/column
// ---------------------------------------------------------------------------

#[test]
fn missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    let err = config::load(&missing).unwrap_err().to_string();
    assert_eq!(
        err,
        format!(
            "config file {} not found; copy config.example.toml from the subagent-mcp repository to that path or set SUBAGENT_MCP_CONFIG",
            missing.display()
        )
    );
}

#[test]
fn unknown_keys_and_active_provider() {
    let legacy = "\nactive_provider = \"deepseek\"\n\n[providers.deepseek]\napi = \"chat-completions\"\nenv_key = \"DS_TEST_KEY\"\ndefault_model = \"deepseek-flash\"\nmodels = [{ id = \"deepseek-flash\" }]\n";
    assert_load_err(&[
        (
            "unknown top key",
            "typo = 1\n".into(),
            r#"config file <PATH>: unknown keys: typo"#,
        ),
        (
            "unknown provider key",
            repl(
                VALID_TOML,
                "env_key = \"ANT_TEST_KEY\"",
                "env_key = \"ANT_TEST_KEY\"\nmodel = \"x\"",
            ),
            r#"config file <PATH>: unknown keys: providers.anthropic.model"#,
        ),
        (
            "active_provider special message",
            legacy.into(),
            r#"config file <PATH>: active_provider was removed in subagent-mcp 0.8.0; delete that line and select a provider per call with the start tool's provider argument instead"#,
        ),
        (
            "active_provider wins over other unknown keys",
            format!("typo = 1\n{legacy}"),
            r#"config file <PATH>: active_provider was removed in subagent-mcp 0.8.0; delete that line and select a provider per call with the start tool's provider argument instead"#,
        ),
        (
            "unknown keys are reported in document order",
            format!(
                "zebra = 1\ntypo = 2\n{VALID_TOML}[auto]\nenv_key = \"K\"\nmodel = \"m\"\nbogus = 1\n"
            ),
            r#"config file <PATH>: unknown keys: zebra, typo, auto.bogus"#,
        ),
        (
            "unknown key inside a model entry",
            repl(
                VALID_TOML,
                "{ id = \"claude-sonnet-5\" }",
                "{ id = \"claude-sonnet-5\", extra = 1 }",
            ),
            r#"config file <PATH>: unknown keys: providers.anthropic.extra"#,
        ),
        (
            "auto unknown key",
            with_auto("[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\nbogus = 1\n"),
            r#"config file <PATH>: unknown keys: auto.bogus"#,
        ),
        (
            "mistyped key never quotes its value",
            repl(
                VALID_TOML,
                "env_key = \"ANT_TEST_KEY\"",
                &format!("kee = \"{KEY_SECRET}\""),
            ),
            r#"config file <PATH>: unknown keys: providers.anthropic.kee"#,
        ),
    ]);
}

#[test]
fn parse_and_type_errors_report_line_column_and_never_leak_the_key() {
    // A value of the wrong type and a broken basic string are library/decode
    // errors, so their text is semantic, but the subagent-mcp wrapper
    // "config file <PATH>: line R, column C: ..." must be present and the
    // secret must never appear in it.
    // The line/column are Go's exact numbers (line 15; column 7 / 33), pinned
    // here so the span-preserving parse is exercised. The trailing text is a
    // Rust decode description: semantic, but the wrapper is subagent-mcp's.
    let wrong_type = repl(
        VALID_TOML,
        "env_key = \"ANT_TEST_KEY\"",
        &format!("key = [\"{KEY_SECRET}\"]"),
    );
    let (dir, path) = write_config(&wrong_type);
    let err = config::load(&path).unwrap_err().to_string();
    assert!(
        err.starts_with(&format!(
            "config file {}: line 15, column 7: ",
            path.display()
        )),
        "wrong type error lost the Go line/column wrapper: {err}"
    );
    assert!(
        err.contains("toml:"),
        "wrong type error lost semantic text: {err}"
    );
    assert!(
        !err.contains(KEY_SECRET),
        "wrong type error leaks the key: {err}"
    );
    drop(dir);

    let broken = repl(
        VALID_TOML,
        "env_key = \"ANT_TEST_KEY\"",
        &format!("key = \"{KEY_SECRET}"),
    );
    let (dir, path) = write_config(&broken);
    let err = config::load(&path).unwrap_err().to_string();
    assert!(
        err.starts_with(&format!(
            "config file {}: line 15, column 33: ",
            path.display()
        )),
        "broken syntax error lost the Go line/column wrapper: {err}"
    );
    assert!(
        !err.contains(KEY_SECRET),
        "broken syntax error leaks the key: {err}"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// Provider validation (exact Go text)
// ---------------------------------------------------------------------------

#[test]
fn provider_validation_errors() {
    assert_load_err(&[
        (
            "bad api",
            repl(VALID_TOML, "api = \"messages\"", "api = \"grpc\""),
            r#"config file <PATH>: providers.anthropic.api must be one of chat-completions, responses, messages, codex-app-server; got "grpc""#,
        ),
        (
            "missing env_key",
            repl(VALID_TOML, "env_key = \"ANT_TEST_KEY\"", ""),
            r#"config file <PATH>: providers.anthropic.env_key or providers.anthropic.key is required"#,
        ),
        (
            "codex env_key",
            format!("{CODEX_TOML}env_key = \"X\"\n"),
            r#"config file <PATH>: providers.codex.env_key is not used by codex-app-server: authentication goes through codex login"#,
        ),
        (
            "codex base_url",
            format!("{CODEX_TOML}base_url = \"https://x\"\n"),
            r#"config file <PATH>: providers.codex.base_url is not used by codex-app-server"#,
        ),
        (
            "codex max_output_tokens",
            repl(
                CODEX_TOML,
                "default_model = \"gpt-6-astra\"",
                "default_model = \"gpt-6-astra\"\nmax_output_tokens = 4096",
            ),
            r#"config file <PATH>: providers.codex.max_output_tokens is not used by codex-app-server: the app-server applies its own limits"#,
        ),
        (
            "native command",
            format!("{VALID_TOML}command = \"codex\"\n"),
            r#"config file <PATH>: providers.anthropic.command is only valid for codex-app-server"#,
        ),
        (
            "chat-completions command",
            repl(
                VALID_TOML,
                "api = \"chat-completions\"",
                "api = \"chat-completions\"\ncommand = \"codex\"",
            ),
            r#"config file <PATH>: providers.deepseek.command is only valid for codex-app-server"#,
        ),
        (
            "responses command",
            repl(
                VALID_TOML,
                "api = \"chat-completions\"",
                "api = \"responses\"\ncommand = \"codex\"",
            ),
            r#"config file <PATH>: providers.deepseek.command is only valid for codex-app-server"#,
        ),
        (
            "codex missing default_model",
            repl(CODEX_TOML, "default_model = \"gpt-6-astra\"", ""),
            r#"config file <PATH>: providers.codex.default_model "" must be one of providers.codex.models"#,
        ),
        (
            "default not in models",
            repl(
                VALID_TOML,
                "default_model = \"claude-sonnet-5\"",
                "default_model = \"claude-opus-5\"",
            ),
            r#"config file <PATH>: providers.anthropic.default_model "claude-opus-5" must be one of providers.anthropic.models"#,
        ),
        (
            "empty models",
            repl(
                VALID_TOML,
                "models = [{ id = \"claude-sonnet-5\" }]",
                "models = []",
            ),
            r#"config file <PATH>: providers.anthropic.models must list at least one model"#,
        ),
        (
            "duplicate model",
            repl(
                VALID_TOML,
                "{ id = \"deepseek-v4-pro\" }",
                "{ id = \"deepseek-flash\" }",
            ),
            r#"config file <PATH>: providers.deepseek.models has duplicate id "deepseek-flash""#,
        ),
        (
            "bad effort key",
            repl(VALID_TOML, "medium = \"high\"", "turbo = \"high\""),
            r#"config file <PATH>: providers.deepseek.effort_map entry "turbo" = "high": keys and values must be one of [none minimal low medium high xhigh max ultra]"#,
        ),
        (
            "bad effort value",
            repl(VALID_TOML, "medium = \"high\"", "medium = \"turbo\""),
            r#"config file <PATH>: providers.deepseek.effort_map entry "medium" = "turbo": keys and values must be one of [none minimal low medium high xhigh max ultra]"#,
        ),
        (
            "negative max tokens",
            repl(
                VALID_TOML,
                "api = \"messages\"",
                "api = \"messages\"\nmax_output_tokens = -1",
            ),
            r#"config file <PATH>: providers.anthropic.max_output_tokens must be positive"#,
        ),
        (
            "negative max nudges",
            repl(
                VALID_TOML,
                "api = \"messages\"",
                "api = \"messages\"\nmax_nudges = -1",
            ),
            r#"config file <PATH>: providers.anthropic.max_nudges must be between 0 and 10"#,
        ),
        (
            "excessive max nudges",
            repl(
                VALID_TOML,
                "api = \"messages\"",
                "api = \"messages\"\nmax_nudges = 11",
            ),
            r#"config file <PATH>: providers.anthropic.max_nudges must be between 0 and 10"#,
        ),
        (
            "invalid provider name",
            repl(
                VALID_TOML,
                "[providers.deepseek]",
                "[providers.\"bad name!\"]",
            ),
            r#"config file <PATH>: providers "bad name!" is not a valid provider name; use only letters, digits, '.', '_', or '-'"#,
        ),
        (
            "no providers",
            "providers = {}\n".into(),
            r#"config file <PATH>: providers must define at least one provider"#,
        ),
    ]);
}

#[test]
fn extra_body_validation_errors() {
    assert_load_err(&[
        (
            "reserved field",
            chat("[providers.gw.extra_body]\nmodel = \"x\"\n"),
            r#"config file <PATH>: providers.gw.extra_body.model is set by the server and cannot be overridden"#,
        ),
        (
            "wrong api",
            repl(&chat("[providers.gw.extra_body]\nseed = 1\n"), "\"chat-completions\"", "\"responses\""),
            r#"config file <PATH>: providers.gw.extra_body is only supported for api = "chat-completions""#,
        ),
        (
            "max_tokens override",
            "[providers.deepseek]\napi = \"chat-completions\"\nenv_key = \"DS_TEST_KEY\"\ndefault_model = \"m\"\nmax_output_tokens = 8192\nmodels = [{ id = \"m\" }]\n\n[providers.deepseek.extra_body]\nmax_tokens = 4096\n".into(),
            r#"config file <PATH>: providers.deepseek.extra_body.max_tokens is set by the server from max_output_tokens; set providers.deepseek.max_output_tokens instead, or use extra_body.max_completion_tokens for a model that requires it"#,
        ),
        (
            "two output limits at once",
            "[providers.openai]\napi = \"chat-completions\"\nenv_key = \"OAI_TEST_KEY\"\ndefault_model = \"m\"\nmax_output_tokens = 8192\nmodels = [{ id = \"m\" }]\n\n[providers.openai.extra_body]\nmax_completion_tokens = 1024\n".into(),
            r#"config file <PATH>: providers.openai sets both max_output_tokens and extra_body.max_completion_tokens, which would send both limits; keep max_completion_tokens and remove max_output_tokens, or the other way round"#,
        ),
    ]);
}

#[test]
fn auto_validation_errors() {
    assert_load_err(&[
        (
            "missing env_key",
            with_auto("[auto]\nmodel = \"jev-latest\"\n"),
            r#"config file <PATH>: auto.env_key or auto.key is required"#,
        ),
        (
            "missing model",
            with_auto("[auto]\nenv_key = \"AUTO_TEST_KEY\"\n"),
            r#"config file <PATH>: auto.model is required"#,
        ),
        (
            "bad base_url",
            with_auto(
                "[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\nbase_url = \"ftp://typesafe\"\n",
            ),
            r#"config file <PATH>: auto.base_url "ftp://typesafe" must be an absolute http or https URL"#,
        ),
        (
            "both endpoints",
            with_auto(
                "[auto]\nenv_key = \"KEY\"\nmodel = \"clef\"\nbase_url = \"https://example.com\"\nurl = \"https://example.com/run\"\n",
            ),
            r#"config file <PATH>: auto.url and auto.base_url cannot both be set"#,
        ),
        (
            "bad url",
            with_auto("[auto]\nenv_key = \"KEY\"\nmodel = \"clef\"\nurl = \"ftp://example.com\"\n"),
            r#"config file <PATH>: auto.url "ftp://example.com" must be an absolute http or https URL"#,
        ),
        (
            "url query",
            with_auto(
                "[auto]\nenv_key = \"KEY\"\nmodel = \"clef\"\nurl = \"https://example.com/run?x=1\"\n",
            ),
            r#"config file <PATH>: auto.url "https://example.com/run?x=1" must be an absolute http or https URL"#,
        ),
        (
            "url fragment",
            with_auto(
                "[auto]\nenv_key = \"KEY\"\nmodel = \"clef\"\nurl = \"https://example.com/run#x\"\n",
            ),
            r#"config file <PATH>: auto.url "https://example.com/run#x" must be an absolute http or https URL"#,
        ),
        (
            "base_url query",
            with_auto(
                "[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\nbase_url = \"https://typesafe.example?tenant=one\"\n",
            ),
            r#"config file <PATH>: auto.base_url "https://typesafe.example?tenant=one" must be an absolute http or https URL"#,
        ),
        (
            "base_url fragment",
            with_auto(
                "[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\nbase_url = \"https://typesafe.example#systemone\"\n",
            ),
            r#"config file <PATH>: auto.base_url "https://typesafe.example#systemone" must be an absolute http or https URL"#,
        ),
        (
            "negative timeout",
            with_auto(
                "[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\ntimeout_seconds = -1\n",
            ),
            r#"config file <PATH>: auto.timeout_seconds must be 0 (default) or between 1 and 600"#,
        ),
        (
            "too long timeout",
            with_auto(
                "[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\ntimeout_seconds = 601\n",
            ),
            r#"config file <PATH>: auto.timeout_seconds must be 0 (default) or between 1 and 600"#,
        ),
    ]);
}

#[test]
fn reserved_auto_model_id() {
    assert_load_err(&[(
        "auto is reserved",
        "\n[providers.deepseek]\napi = \"chat-completions\"\nenv_key = \"DS_TEST_KEY\"\ndefault_model = \"auto\"\nmodels = [{ id = \"auto\" }]\n".into(),
        r#"config file <PATH>: providers.deepseek.models[0].id "auto" is reserved for the start tool's auto model selection; rename it"#,
    )]);
}

#[test]
fn key_and_env_key_exclusivity() {
    assert_load_err(&[
        (
            "provider both",
            repl(
                VALID_TOML,
                "env_key = \"ANT_TEST_KEY\"",
                &format!("env_key = \"ANT_TEST_KEY\"\nkey = \"{KEY_SECRET}\""),
            ),
            r#"config file <PATH>: providers.anthropic.env_key and providers.anthropic.key cannot both be set"#,
        ),
        (
            "provider neither",
            repl(VALID_TOML, "env_key = \"ANT_TEST_KEY\"", ""),
            r#"config file <PATH>: providers.anthropic.env_key or providers.anthropic.key is required"#,
        ),
        (
            "codex key",
            format!("{CODEX_TOML}key = \"{KEY_SECRET}\"\n"),
            r#"config file <PATH>: providers.codex.key is not used by codex-app-server: authentication goes through codex login"#,
        ),
        (
            "auto both",
            format!(
                "{VALID_TOML}\n[auto]\nmodel = \"jev-latest\"\nenv_key = \"AUTO_TEST_KEY\"\nkey = \"{KEY_SECRET}\"\n"
            ),
            r#"config file <PATH>: auto.env_key and auto.key cannot both be set"#,
        ),
        (
            "auto neither",
            format!("{VALID_TOML}\n[auto]\nmodel = \"jev-latest\"\n"),
            r#"config file <PATH>: auto.env_key or auto.key is required"#,
        ),
    ]);
}

// ---------------------------------------------------------------------------
// Non-Windows mode check
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn key_requires_private_file_mode() {
    let key_toml = repl(
        VALID_TOML,
        "env_key = \"ANT_TEST_KEY\"",
        &format!("key = \"{KEY_SECRET}\""),
    );
    for mode in [0o644u32, 0o640, 0o604] {
        let (dir, path) = write_config(&key_toml);
        set_mode(&path, mode);
        let err = config::load(&path).unwrap_err().to_string();
        let expected = format!(
            "config file {} holds an API key but its mode is {:04o}, so other users can read it; run: chmod 600 {}",
            path.display(),
            mode,
            path.display()
        );
        assert_eq!(err, expected, "mode {mode:04o}");
        assert!(!err.contains(KEY_SECRET), "mode error leaks the key: {err}");
        drop(dir);
    }
    // 0600 is accepted.
    let (dir, path) = write_config(&key_toml);
    set_mode(&path, 0o600);
    assert!(config::load(&path).is_ok(), "mode 0600 should be accepted");
    drop(dir);
    // An env_key-only config names no secret and may stay world-readable.
    let (dir, path) = write_config(VALID_TOML);
    set_mode(&path, 0o644);
    assert!(
        config::load(&path).is_ok(),
        "env_key-only config with mode 0644 should be accepted"
    );
    drop(dir);
}

// ---------------------------------------------------------------------------
// Default path
// ---------------------------------------------------------------------------

#[test]
fn default_path_from_env_and_home() {
    // Only this test touches these variables; restore them afterwards so the
    // rest of the binary (which never reads them) is unaffected.
    let old_cfg = std::env::var("SUBAGENT_MCP_CONFIG").ok();
    let old_home = std::env::var("HOME").ok();

    unsafe {
        std::env::set_var("SUBAGENT_MCP_CONFIG", "/custom/config.toml");
    }
    assert_eq!(
        config::default_path().unwrap(),
        PathBuf::from("/custom/config.toml")
    );

    unsafe {
        std::env::set_var("SUBAGENT_MCP_CONFIG", "");
        std::env::set_var("HOME", "/home/test");
    }
    assert_eq!(
        config::default_path().unwrap(),
        PathBuf::from("/home/test/.config/subagent-mcp/config.toml")
    );

    unsafe {
        match old_cfg {
            Some(v) => std::env::set_var("SUBAGENT_MCP_CONFIG", v),
            None => std::env::remove_var("SUBAGENT_MCP_CONFIG"),
        }
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

// ---------------------------------------------------------------------------
// Successful loads, defaults, and schema mapping
// ---------------------------------------------------------------------------

#[test]
fn load_valid_applies_defaults_and_schema() {
    let (dir, path) = write_config(VALID_TOML);
    let cfg = config::load(&path).unwrap();

    let ds = &cfg.providers["deepseek"];
    assert_eq!(ds.api, config::Api::ChatCompletions);
    assert_eq!(ds.base_url, "https://api.deepseek.com");
    assert_eq!(ds.env_key.as_deref(), Some("DS_TEST_KEY"));
    assert_eq!(ds.key, None);
    assert_eq!(ds.command, None);
    assert_eq!(ds.default_model, "deepseek-flash");
    assert_eq!(ds.models.len(), 2);
    assert_eq!(ds.models[0].id, "deepseek-flash");
    assert_eq!(ds.models[0].description, "fast");
    assert_eq!(ds.models[1].id, "deepseek-v4-pro");
    assert_eq!(ds.models[1].description, "");
    assert_eq!(
        ds.effort_map.get("medium").map(String::as_str),
        Some("high")
    );
    assert_eq!(ds.effort_map.get("xhigh").map(String::as_str), Some("max"));
    assert_eq!(ds.max_output_tokens, None);
    assert_eq!(ds.max_nudges, None);
    assert_eq!(ds.extra_body, None);

    let an = &cfg.providers["anthropic"];
    assert_eq!(an.api, config::Api::Messages);
    assert_eq!(an.base_url, "https://api.anthropic.com");
    assert_eq!(an.max_output_tokens, Some(64000));

    assert!(cfg.path.is_absolute());
    assert_eq!(cfg.auto, None);
    drop(dir);
}

#[test]
fn api_specific_base_urls_and_messages_output_token_default() {
    let content = r#"
[providers.chat]
api = "chat-completions"
env_key = "K"
default_model = "m"
models = [{ id = "m" }]

[providers.resp]
api = "responses"
env_key = "K"
default_model = "m"
models = [{ id = "m" }]

[providers.msg]
api = "messages"
env_key = "K"
default_model = "m"
models = [{ id = "m" }]
"#;
    let (dir, path) = write_config(content);
    let cfg = config::load(&path).unwrap();
    assert_eq!(cfg.providers["chat"].base_url, "https://api.openai.com/v1");
    assert_eq!(cfg.providers["resp"].base_url, "https://api.openai.com/v1");
    assert_eq!(cfg.providers["msg"].base_url, "https://api.anthropic.com");
    assert_eq!(cfg.providers["msg"].max_output_tokens, Some(64000));
    assert_eq!(cfg.providers["chat"].max_output_tokens, None);
    assert_eq!(cfg.providers["resp"].max_output_tokens, None);
    drop(dir);
}

#[test]
fn path_is_resolved_to_absolute() {
    let (dir, path) = write_config(VALID_TOML);
    let with_dot = dir.path().join(".").join("config.toml");
    let cfg = config::load(&with_dot).unwrap();
    assert!(cfg.path.is_absolute());
    assert_eq!(cfg.path, std::fs::canonicalize(&path).unwrap());
    drop(dir);
}

#[test]
fn codex_app_server_provider() {
    let (dir, path) = write_config(CODEX_TOML);
    let cfg = config::load(&path).unwrap();
    let c = &cfg.providers["codex"];
    assert_eq!(c.api, config::Api::CodexAppServer);
    assert_eq!(c.base_url, "");
    assert_eq!(c.command, None);
    assert_eq!(c.env_key, None);
    assert_eq!(c.key, None);
    assert_eq!(c.max_output_tokens, None);
    drop(dir);
}

#[test]
fn codex_command_is_accepted() {
    let (dir, path) = write_config(&format!("{CODEX_TOML}command = \"/opt/codex/bin/codex\"\n"));
    let cfg = config::load(&path).unwrap();
    assert_eq!(
        cfg.providers["codex"].command.as_deref(),
        Some("/opt/codex/bin/codex")
    );
    drop(dir);
}

#[test]
fn auto_defaults() {
    let content = with_auto("[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\n");
    let (dir, path) = write_config(&content);
    let cfg = config::load(&path).unwrap();
    let auto = cfg.auto.unwrap();
    assert_eq!(auto.base_url, "https://api.typesafe.ai");
    assert_eq!(auto.timeout_seconds, 20);
    assert_eq!(auto.url, "");
    assert_eq!(auto.env_key.as_deref(), Some("AUTO_TEST_KEY"));
    assert_eq!(auto.key, None);
    assert_eq!(auto.model, "jev-latest");
    drop(dir);
}

#[test]
fn auto_url_endpoint() {
    let content = with_auto(
        "[auto]\nenv_key = \"AUTO_TEST_KEY\"\nmodel = \"jev-latest\"\nurl = \"https://example.com/ai/run/@cf/cloudflare/clef\"\n",
    );
    let (dir, path) = write_config(&content);
    let cfg = config::load(&path).unwrap();
    let auto = cfg.auto.unwrap();
    assert_eq!(auto.url, "https://example.com/ai/run/@cf/cloudflare/clef");
    assert_eq!(auto.base_url, "");
    drop(dir);
}

#[test]
fn extra_body_parses_nested_values() {
    let content = chat("[providers.gw.extra_body.provider]\nignore = [\"relace\"]\n");
    let (dir, path) = write_config(&content);
    let cfg = config::load(&path).unwrap();
    let eb = cfg.providers["gw"].extra_body.as_ref().unwrap();
    let provider = eb.get("provider").unwrap();
    assert_eq!(provider["ignore"][0].as_str(), Some("relace"));
    drop(dir);
}

#[test]
fn extra_body_max_completion_tokens_alone_is_accepted() {
    let content = "[providers.openai]\napi = \"chat-completions\"\nenv_key = \"OAI_TEST_KEY\"\ndefault_model = \"m\"\nmodels = [{ id = \"m\" }]\n\n[providers.openai.extra_body]\nmax_completion_tokens = 4096\n";
    let (dir, path) = write_config(content);
    let cfg = config::load(&path).unwrap();
    let eb = cfg.providers["openai"].extra_body.as_ref().unwrap();
    assert_eq!(
        eb.get("max_completion_tokens").unwrap().as_i64(),
        Some(4096)
    );
    drop(dir);
}

#[test]
fn extra_body_arbitrary_keys_are_allowed() {
    let content = chat("[providers.gw.extra_body]\nseed = 1\nprovider = { only = [\"x\"] }\n");
    let (dir, path) = write_config(&content);
    assert!(config::load(&path).is_ok());
    drop(dir);
}

// ---------------------------------------------------------------------------
// Fixture files
// ---------------------------------------------------------------------------

#[test]
fn valid_fixtures_load() {
    for fixture in [
        include_str!("fixtures/config/valid.toml"),
        include_str!("fixtures/config/codex.toml"),
        include_str!("fixtures/config/auto.toml"),
    ] {
        let (dir, path) = write_config(fixture);
        assert!(
            config::load(&path).is_ok(),
            "fixture failed to load:\n{fixture}"
        );
        drop(dir);
    }
}

#[test]
fn legacy_active_provider_fixture_is_rejected() {
    let (dir, path) = write_config(include_str!("fixtures/config/active_provider.toml"));
    let err = config::load(&path).unwrap_err().to_string();
    let expected = format!(
        "config file {}: active_provider was removed in subagent-mcp 0.8.0; delete that line and select a provider per call with the start tool's provider argument instead",
        path.display()
    );
    assert_eq!(err, expected);
    drop(dir);
}
