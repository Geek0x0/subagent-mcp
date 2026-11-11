//! Strict TOML config loading, schema, defaults and validation.
//!
//! Ports Go `internal/config/config.go` (subagent-mcp v0.13.1) exactly: the
//! user-visible error text, the defaults applied by `Load`, and the order of
//! validation checks. Values are decoded by walking the expected schema over a
//! `toml_edit` document (which preserves source spans), so unknown keys can be
//! reported in document order and type errors can carry a line/column without
//! ever echoing the offending value (an API key must not leak into an error).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use toml_edit::{Document, Item, TableLike, Value as TomlValue};

use crate::util::fs::eval_symlinks;
use crate::util::json::go_quote;

// --- Constants (Go config.go) ---------------------------------------------

const API_CHAT: &str = "chat-completions";
const API_RESPONSES: &str = "responses";
const API_MESSAGES: &str = "messages";
const API_CODEX: &str = "codex-app-server";

const EFFORT_VALUES: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
const EFFORT_VALUES_DISPLAY: &str = "[none minimal low medium high xhigh max ultra]";

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64000;
const MAX_NUDGES_LIMIT: i64 = 10;

const DEFAULT_AUTO_BASE_URL: &str = "https://api.typesafe.ai";
const DEFAULT_AUTO_TIMEOUT_SECONDS: i64 = 20;
const MAX_AUTO_TIMEOUT_SECONDS: i64 = 600;

const RESERVED_BODY_FIELDS: [&str; 7] = [
    "model",
    "messages",
    "stream",
    "stream_options",
    "tools",
    "tool_choice",
    "max_tokens",
];

// --- Public interface ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    ChatCompletions,
    Responses,
    Messages,
    CodexAppServer,
}

impl Api {
    /// The wire identifier accepted by the `api` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Api::ChatCompletions => API_CHAT,
            Api::Responses => API_RESPONSES,
            Api::Messages => API_MESSAGES,
            Api::CodexAppServer => API_CODEX,
        }
    }

    fn parse(s: &str) -> Option<Api> {
        match s {
            API_CHAT => Some(Api::ChatCompletions),
            API_RESPONSES => Some(Api::Responses),
            API_MESSAGES => Some(Api::Messages),
            API_CODEX => Some(Api::CodexAppServer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub api: Api,
    pub base_url: String,
    pub env_key: Option<String>,
    pub key: Option<String>,
    pub command: Option<String>,
    pub default_model: String,
    pub models: Vec<ModelConfig>,
    pub effort_map: BTreeMap<String, String>,
    pub max_output_tokens: Option<i64>,
    pub max_nudges: Option<usize>,
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoConfig {
    pub base_url: String,
    pub url: String,
    pub env_key: Option<String>,
    pub key: Option<String>,
    pub model: String,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub path: PathBuf,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub auto: Option<AutoConfig>,
}

#[derive(Debug, Clone)]
pub struct ConfigError {
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

fn err(message: impl Into<String>) -> ConfigError {
    ConfigError {
        message: message.into(),
    }
}

fn opt_str(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Go `os.UserHomeDir` reads `%USERPROFILE%` on Windows and `$HOME` on Unix.
/// Off Windows we additionally accept `USERPROFILE` as a fallback so a
/// Windows-style environment resolves on any host (and is testable off
/// Windows); on Windows only `USERPROFILE` is consulted, exactly like Go.
#[cfg(windows)]
const HOME_ENV_KEYS: [&str; 1] = ["USERPROFILE"];
#[cfg(not(windows))]
const HOME_ENV_KEYS: [&str; 2] = ["HOME", "USERPROFILE"];

/// The exact `os.UserHomeDir` error text for the target platform.
#[cfg(windows)]
const HOME_ENV_ERROR: &str = "%userprofile% is not defined";
#[cfg(not(windows))]
const HOME_ENV_ERROR: &str = "$HOME is not defined";

/// Return `$SUBAGENT_MCP_CONFIG` or `~/.config/subagent-mcp/config.toml`.
pub fn default_path() -> Result<PathBuf, ConfigError> {
    resolve_default_path(
        |key| std::env::var(key).ok(),
        &HOME_ENV_KEYS,
        HOME_ENV_ERROR,
    )
}

/// Pure core of `default_path`, parameterised over the environment so both the
/// Windows and Unix var sets / error texts can be tested on any host.
fn resolve_default_path(
    getenv: impl Fn(&str) -> Option<String>,
    home_keys: &[&str],
    home_error: &str,
) -> Result<PathBuf, ConfigError> {
    if let Some(configured) = getenv("SUBAGENT_MCP_CONFIG").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(configured));
    }
    // Go's UserHomeDir treats an empty value as undefined.
    let home = home_keys
        .iter()
        .find_map(|key| getenv(key).filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            err(format!(
                "resolve home directory for the default config path: {home_error}"
            ))
        })?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("subagent-mcp")
        .join("config.toml"))
}

#[cfg(test)]
mod default_path_tests {
    use super::*;
    use std::collections::HashMap;

    fn fake_env(entries: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    // Go on Windows: %USERPROFILE% resolves home (verified against Go's
    // os.UserHomeDir source, os/file.go, and the default-path integration
    // tests). These inline tests exercise that branch on a Linux test host.
    #[test]
    fn windows_style_home_resolves_via_userprofile() {
        let got = resolve_default_path(
            fake_env(&[("USERPROFILE", "C:\\Users\\tester")]),
            &["USERPROFILE"],
            "%userprofile% is not defined",
        )
        .unwrap();
        assert_eq!(
            got,
            PathBuf::from("C:\\Users\\tester")
                .join(".config")
                .join("subagent-mcp")
                .join("config.toml")
        );
    }

    #[test]
    fn windows_style_error_text_is_go_exact() {
        let err = resolve_default_path(
            fake_env(&[]),
            &["USERPROFILE"],
            "%userprofile% is not defined",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "resolve home directory for the default config path: %userprofile% is not defined"
        );
    }

    #[test]
    fn unix_style_error_text_is_go_exact() {
        let err =
            resolve_default_path(fake_env(&[]), &["HOME"], "$HOME is not defined").unwrap_err();
        assert_eq!(
            err.to_string(),
            "resolve home directory for the default config path: $HOME is not defined"
        );
    }

    #[test]
    fn subagent_mcp_config_wins_over_home() {
        let got = resolve_default_path(
            fake_env(&[
                ("SUBAGENT_MCP_CONFIG", "/custom/config.toml"),
                ("HOME", "/home/test"),
            ]),
            &["HOME"],
            "$HOME is not defined",
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/custom/config.toml"));
    }

    #[test]
    fn empty_home_var_is_treated_as_unset() {
        // A Unix host with HOME empty falls back to USERPROFILE; when that is
        // empty too, Go's Unix error text is produced.
        let err = resolve_default_path(
            fake_env(&[("HOME", ""), ("SUBAGENT_MCP_CONFIG", "")]),
            &["HOME", "USERPROFILE"],
            "$HOME is not defined",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "resolve home directory for the default config path: $HOME is not defined"
        );
    }
}

// --- Load ------------------------------------------------------------------

/// Read, strictly decode, default and validate the config at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(err(format!(
                "config file {} not found; copy config.example.toml from the subagent-mcp repository to that path or set SUBAGENT_MCP_CONFIG",
                path.display()
            )));
        }
        Err(e) => {
            return Err(err(format!("read config file {}: {}", path.display(), e)));
        }
    };
    // Stat the opened file so the mode checked below is the mode of what was read.
    let metadata = file
        .metadata()
        .map_err(|e| err(format!("read config file {}: {}", path.display(), e)))?;
    let mut data = String::new();
    file.read_to_string(&mut data)
        .map_err(|e| err(format!("read config file {}: {}", path.display(), e)))?;

    // Document<String> keeps the parser's source spans; DocumentMut would run
    // despan() and drop them, losing line/column for type errors.
    let doc: Document<String> = match data.parse() {
        Ok(d) => d,
        Err(e) => {
            let (line, column) = e.span().map(|s| line_col(&data, s.start)).unwrap_or((1, 1));
            return Err(err(format!(
                "config file {}: line {}, column {}: {}",
                path.display(),
                line,
                column,
                e.message()
            )));
        }
    };

    // Strict unknown-key detection runs before typed extraction, matching the
    // precedence of go-toml's DisallowUnknownFields StrictMissingError.
    let unknown = collect_unknown(&doc);
    if unknown.iter().any(|k| k == "active_provider") {
        return Err(err(format!(
            "config file {}: active_provider was removed in subagent-mcp 0.8.0; delete that line and select a provider per call with the start tool's provider argument instead",
            path.display()
        )));
    }
    if !unknown.is_empty() {
        return Err(err(format!(
            "config file {}: unknown keys: {}",
            path.display(),
            unknown.join(", ")
        )));
    }

    let mut raw = extract(&doc, path, &data)?;

    // A config that carries an API key must not be readable by other users.
    // Only meaningful off Windows (Go: runtime.GOOS != "windows").
    let holds_key = raw.providers.values().any(|p| !p.key.is_empty())
        || raw.auto.as_ref().is_some_and(|a| !a.key.is_empty());
    #[cfg(unix)]
    if holds_key {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(err(format!(
                "config file {} holds an API key but its mode is {:04o}, so other users can read it; run: chmod 600 {}",
                path.display(),
                mode,
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = (holds_key, &metadata);

    // Go stores filepath.Abs(path); util::fs::eval_symlinks (canonicalize) also
    // yields an absolute path for an existing file.
    let absolute = eval_symlinks(path).map_err(|e| {
        err(format!(
            "resolve config file path {}: {}",
            path.display(),
            e
        ))
    })?;

    // Defaults are applied before validation, exactly like Go Load.
    for p in raw.providers.values_mut() {
        if p.base_url.is_empty() {
            match p.api.as_str() {
                API_MESSAGES => p.base_url = "https://api.anthropic.com".into(),
                API_CHAT | API_RESPONSES => p.base_url = "https://api.openai.com/v1".into(),
                _ => {}
            }
        }
        if p.max_output_tokens == 0 && p.api == API_MESSAGES {
            p.max_output_tokens = DEFAULT_MAX_OUTPUT_TOKENS;
        }
    }
    if let Some(a) = raw.auto.as_mut() {
        if a.base_url.is_empty() && a.url.is_empty() {
            a.base_url = DEFAULT_AUTO_BASE_URL.into();
        }
        if a.timeout_seconds == 0 {
            a.timeout_seconds = DEFAULT_AUTO_TIMEOUT_SECONDS;
        }
    }

    validate(&raw).map_err(|inner| err(format!("config file {}: {}", path.display(), inner)))?;

    Ok(build_config(absolute, raw))
}

/// 1-based line and column (counting UTF-8 chars for the column) of a byte
/// offset, matching go-toml/toml_edit error positions.
fn line_col(data: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(data.len());
    let mut line = 1usize;
    let mut column = 1usize;
    for &b in &data.as_bytes()[..offset] {
        if b == b'\n' {
            line += 1;
            column = 1;
        } else if b & 0xC0 != 0x80 {
            // only UTF-8 leading bytes start a new char
            column += 1;
        }
    }
    (line, column)
}

fn type_error(
    path: &Path,
    data: &str,
    span: Option<std::ops::Range<usize>>,
    field: &str,
    got: &str,
    expected: &str,
) -> ConfigError {
    let (line, column) = span.map(|s| line_col(data, s.start)).unwrap_or((1, 1));
    err(format!(
        "config file {}: line {}, column {}: toml: cannot decode TOML {} into {} of type {}",
        path.display(),
        line,
        column,
        got,
        field,
        expected
    ))
}

fn item_type_err(path: &Path, data: &str, item: &Item, field: &str, expected: &str) -> ConfigError {
    let got = item.as_value().map(|v| v.type_name()).unwrap_or("table");
    type_error(path, data, item.span(), field, got, expected)
}

// --- Strict unknown-key walk -----------------------------------------------
//
// go-toml's strict decoder keeps a "current key" that is reset to the full
// dotted path of each `[table]` / `[[array-table]]` header (EnterTable). An
// unknown key-value reports that header prefix plus the key's own path
// (MissingField); an unknown table header reports its full path (MissingTable).
// Inline tables (and inline arrays) are transparent: assignInlineTable builds a
// fresh path from each element's own keys, so intermediate inline keys never
// enter the prefix and an unknown field inside one is reported as
// `header-prefix + offending-key`.
//
// `toml_edit` distinguishes the shapes: `Item::Table` / `Item::ArrayOfTables`
// come from headers (contribute to the prefix), while `Value::InlineTable` /
// `Value::Array` are inline (transparent). `Table::position().is_some()` marks
// an explicit header, which is where a MissingTable path stops.

fn collect_unknown(doc: &Document<String>) -> Vec<String> {
    let mut out = Vec::new();
    let prefix: Vec<String> = Vec::new();
    for (key, item) in doc.as_table().iter() {
        match key {
            "providers" => walk_providers(item, &prefix, &mut out),
            "auto" => walk_auto(item, &prefix, &mut out),
            _ => report_unknown(key, item, &prefix, &mut out),
        }
    }
    out
}

fn with_key(prefix: &[String], key: &str) -> Vec<String> {
    let mut path = prefix.to_vec();
    path.push(key.to_string());
    path
}

/// Report an unknown key with go-toml's key path. A table-header value is
/// reported with its full (possibly nested) header path; a scalar, inline table
/// or inline array is reported as `prefix + key` only (go-toml does not descend
/// into the value of an unknown field).
fn report_unknown(key: &str, item: &Item, prefix: &[String], out: &mut Vec<String>) {
    let path = with_key(prefix, key);
    match item {
        Item::Table(t) => push_table_paths(t, path, out),
        _ => out.push(path.join(".")),
    }
}

/// Descend the implicit/dotted table chain of a table-header value, emitting a
/// path for every explicit header or dotted leaf it ends in.
fn push_table_paths(t: &toml_edit::Table, path: Vec<String>, out: &mut Vec<String>) {
    if t.position().is_some() {
        // An explicit `[header]`: go-toml's EnterTable stopped here.
        out.push(path.join("."));
        return;
    }
    let mut children = 0;
    for (k, child) in t.iter() {
        children += 1;
        let child_path = with_key(&path, k);
        match child {
            Item::Table(ct) => push_table_paths(ct, child_path, out),
            // An array table is itself a header; a dotted-key leaf ends the
            // dotted path. Both report the child path as-is.
            _ => out.push(child_path.join(".")),
        }
    }
    if children == 0 {
        out.push(path.join("."));
    }
}

/// The `providers` value. A real table header contributes `providers` to the
/// prefix; an inline `providers = { ... }` does not.
fn walk_providers(item: &Item, prefix: &[String], out: &mut Vec<String>) {
    let is_table = matches!(item, Item::Table(_));
    if let Some(t) = item.as_table_like() {
        let p = if is_table {
            with_key(prefix, "providers")
        } else {
            prefix.to_vec()
        };
        for (name, pval) in t.iter() {
            if let Some(pt) = pval.as_table_like() {
                let p2 = if matches!(pval, Item::Table(_)) {
                    with_key(&p, name)
                } else {
                    p.clone()
                };
                walk_provider(pt, &p2, out);
            }
            // A non-table provider value is a type error reported by extract().
        }
    }
}

fn walk_provider(t: &dyn TableLike, prefix: &[String], out: &mut Vec<String>) {
    for (key, item) in t.iter() {
        match key {
            // effort_map and extra_body have dynamic keys; never walked.
            "api" | "base_url" | "env_key" | "key" | "command" | "default_model"
            | "max_output_tokens" | "max_nudges" | "effort_map" | "extra_body" => {}
            "models" => walk_models(item, prefix, out),
            _ => report_unknown(key, item, prefix, out),
        }
    }
}

fn walk_models(item: &Item, prefix: &[String], out: &mut Vec<String>) {
    match item {
        // Inline array of inline tables: transparent, so `models` is not in
        // the path (go-toml rebuilds it from each element's own keys).
        Item::Value(v) if v.as_array().is_some() => {
            for elem in v.as_array().unwrap().iter() {
                if let Some(it) = elem.as_inline_table() {
                    let tl: &dyn TableLike = it;
                    walk_model(tl, prefix, out);
                }
            }
        }
        // `[[...models]]`: the array-table name IS part of the header path.
        Item::ArrayOfTables(aot) => {
            let p = with_key(prefix, "models");
            for t in aot.iter() {
                let tl: &dyn TableLike = t;
                walk_model(tl, &p, out);
            }
        }
        _ => {} // wrong type: type error reported by extract()
    }
}

fn walk_model(t: &dyn TableLike, prefix: &[String], out: &mut Vec<String>) {
    for (key, item) in t.iter() {
        match key {
            "id" | "description" => {}
            _ => report_unknown(key, item, prefix, out),
        }
    }
}

fn walk_auto(item: &Item, prefix: &[String], out: &mut Vec<String>) {
    let is_table = matches!(item, Item::Table(_));
    if let Some(t) = item.as_table_like() {
        let p = if is_table {
            with_key(prefix, "auto")
        } else {
            prefix.to_vec()
        };
        for (key, aitem) in t.iter() {
            match key {
                "base_url" | "url" | "env_key" | "key" | "model" | "timeout_seconds" => {}
                _ => report_unknown(key, aitem, &p, out),
            }
        }
    }
}

// --- Raw (Go-shaped) decoded config ----------------------------------------

#[derive(Default)]
struct RawProvider {
    api: String,
    base_url: String,
    env_key: String,
    key: String,
    command: String,
    default_model: String,
    models: Vec<RawModel>,
    effort_map: BTreeMap<String, String>,
    max_output_tokens: i64,
    max_nudges: i64,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Default)]
struct RawModel {
    id: String,
    description: String,
}

#[derive(Default)]
struct RawAuto {
    base_url: String,
    url: String,
    env_key: String,
    key: String,
    model: String,
    timeout_seconds: i64,
}

struct RawConfig {
    providers: BTreeMap<String, RawProvider>,
    auto: Option<RawAuto>,
}

fn extract(doc: &Document<String>, path: &Path, data: &str) -> Result<RawConfig, ConfigError> {
    let root = doc.as_table();
    let mut providers = BTreeMap::new();
    if let Some(pitem) = root.get("providers") {
        if let Some(pt) = pitem.as_table_like() {
            for (pname, pval) in pt.iter() {
                if let Some(pv) = pval.as_table_like() {
                    providers.insert(pname.to_string(), extract_provider(pname, pv, path, data)?);
                } else {
                    return Err(item_type_err(
                        path,
                        data,
                        pval,
                        &format!("providers.{pname}"),
                        "table",
                    ));
                }
            }
        } else {
            return Err(item_type_err(path, data, pitem, "providers", "table"));
        }
    }

    let auto = if let Some(aitem) = root.get("auto") {
        if let Some(at) = aitem.as_table_like() {
            Some(extract_auto(at, path, data)?)
        } else {
            return Err(item_type_err(path, data, aitem, "auto", "table"));
        }
    } else {
        None
    };

    Ok(RawConfig { providers, auto })
}

fn str_field(
    t: &dyn TableLike,
    key: &str,
    field: &str,
    path: &Path,
    data: &str,
) -> Result<String, ConfigError> {
    match t.get(key) {
        None => Ok(String::new()),
        Some(item) if item.is_none() => Ok(String::new()),
        Some(item) => match item.as_str() {
            Some(s) => Ok(s.to_string()),
            None => Err(item_type_err(path, data, item, field, "string")),
        },
    }
}

fn int_field(
    t: &dyn TableLike,
    key: &str,
    field: &str,
    path: &Path,
    data: &str,
) -> Result<i64, ConfigError> {
    match t.get(key) {
        None => Ok(0),
        Some(item) if item.is_none() => Ok(0),
        Some(item) => match item.as_integer() {
            Some(i) => Ok(i),
            None => Err(item_type_err(path, data, item, field, "integer")),
        },
    }
}

fn extract_provider(
    pname: &str,
    t: &dyn TableLike,
    path: &Path,
    data: &str,
) -> Result<RawProvider, ConfigError> {
    let mut p = RawProvider {
        api: str_field(t, "api", &format!("providers.{pname}.api"), path, data)?,
        base_url: str_field(
            t,
            "base_url",
            &format!("providers.{pname}.base_url"),
            path,
            data,
        )?,
        env_key: str_field(
            t,
            "env_key",
            &format!("providers.{pname}.env_key"),
            path,
            data,
        )?,
        key: str_field(t, "key", &format!("providers.{pname}.key"), path, data)?,
        command: str_field(
            t,
            "command",
            &format!("providers.{pname}.command"),
            path,
            data,
        )?,
        default_model: str_field(
            t,
            "default_model",
            &format!("providers.{pname}.default_model"),
            path,
            data,
        )?,
        models: Vec::new(),
        effort_map: BTreeMap::new(),
        max_output_tokens: int_field(
            t,
            "max_output_tokens",
            &format!("providers.{pname}.max_output_tokens"),
            path,
            data,
        )?,
        max_nudges: int_field(
            t,
            "max_nudges",
            &format!("providers.{pname}.max_nudges"),
            path,
            data,
        )?,
        extra_body: None,
    };

    if let Some(item) = t.get("models") {
        p.models = extract_models(pname, item, path, data)?;
    }
    if let Some(item) = t.get("effort_map") {
        p.effort_map = extract_effort_map(pname, item, path, data)?;
    }
    if let Some(item) = t.get("extra_body") {
        if let Some(map) = item_to_json_map(item) {
            if !map.is_empty() {
                p.extra_body = Some(map);
            }
        } else {
            return Err(item_type_err(
                path,
                data,
                item,
                &format!("providers.{pname}.extra_body"),
                "table",
            ));
        }
    }
    Ok(p)
}

fn extract_models(
    pname: &str,
    item: &Item,
    path: &Path,
    data: &str,
) -> Result<Vec<RawModel>, ConfigError> {
    let mut out = Vec::new();
    let field = format!("providers.{pname}.models");
    if let Some(arr) = item.as_array() {
        for elem in arr.iter() {
            if let Some(it) = elem.as_inline_table() {
                let tl: &dyn TableLike = it;
                let i = out.len();
                out.push(RawModel {
                    id: str_field(tl, "id", &format!("{field}[{i}].id"), path, data)?,
                    description: str_field(
                        tl,
                        "description",
                        &format!("{field}[{i}].description"),
                        path,
                        data,
                    )?,
                });
            } else {
                return Err(type_error(
                    path,
                    data,
                    elem.span(),
                    &field,
                    elem.type_name(),
                    "table",
                ));
            }
        }
    } else if let Some(aot) = item.as_array_of_tables() {
        for t in aot.iter() {
            let tl: &dyn TableLike = t;
            let i = out.len();
            out.push(RawModel {
                id: str_field(tl, "id", &format!("{field}[{i}].id"), path, data)?,
                description: str_field(
                    tl,
                    "description",
                    &format!("{field}[{i}].description"),
                    path,
                    data,
                )?,
            });
        }
    } else {
        return Err(item_type_err(path, data, item, &field, "array"));
    }
    Ok(out)
}

fn extract_effort_map(
    pname: &str,
    item: &Item,
    path: &Path,
    data: &str,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let field = format!("providers.{pname}.effort_map");
    if let Some(t) = item.as_table_like() {
        let mut out = BTreeMap::new();
        for (k, v) in t.iter() {
            match v.as_str() {
                Some(s) => {
                    out.insert(k.to_string(), s.to_string());
                }
                None => {
                    return Err(item_type_err(
                        path,
                        data,
                        v,
                        &format!("{field}.{k}"),
                        "string",
                    ));
                }
            }
        }
        Ok(out)
    } else {
        Err(item_type_err(path, data, item, &field, "table"))
    }
}

fn extract_auto(t: &dyn TableLike, path: &Path, data: &str) -> Result<RawAuto, ConfigError> {
    Ok(RawAuto {
        base_url: str_field(t, "base_url", "auto.base_url", path, data)?,
        url: str_field(t, "url", "auto.url", path, data)?,
        env_key: str_field(t, "env_key", "auto.env_key", path, data)?,
        key: str_field(t, "key", "auto.key", path, data)?,
        model: str_field(t, "model", "auto.model", path, data)?,
        timeout_seconds: int_field(t, "timeout_seconds", "auto.timeout_seconds", path, data)?,
    })
}

// --- toml_edit value -> serde_json::Value (for extra_body) ------------------

fn item_to_json_map(item: &Item) -> Option<serde_json::Map<String, serde_json::Value>> {
    let t = item.as_table_like()?;
    let mut map = serde_json::Map::new();
    for (k, v) in t.iter() {
        if let Some(j) = item_to_json(v) {
            map.insert(k.to_string(), j);
        }
    }
    Some(map)
}

fn item_to_json(item: &Item) -> Option<serde_json::Value> {
    match item {
        Item::None => None,
        Item::Value(v) => Some(value_to_json(v)),
        Item::Table(t) => Some(table_to_json(t)),
        Item::ArrayOfTables(a) => Some(serde_json::Value::Array(
            a.iter().map(table_to_json).collect(),
        )),
    }
}

fn value_to_json(v: &TomlValue) -> serde_json::Value {
    match v {
        TomlValue::String(f) => serde_json::Value::String(f.value().clone()),
        TomlValue::Integer(f) => serde_json::Value::Number(serde_json::Number::from(*f.value())),
        TomlValue::Float(f) => serde_json::Number::from_f64(*f.value())
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        TomlValue::Boolean(f) => serde_json::Value::Bool(*f.value()),
        TomlValue::Datetime(f) => serde_json::Value::String(f.value().to_string()),
        TomlValue::Array(a) => serde_json::Value::Array(a.iter().map(value_to_json).collect()),
        TomlValue::InlineTable(t) => {
            let mut m = serde_json::Map::new();
            for (k, v) in t.iter() {
                m.insert(k.to_string(), value_to_json(v));
            }
            serde_json::Value::Object(m)
        }
    }
}

fn table_to_json(t: &toml_edit::Table) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in t.iter() {
        if let Some(j) = item_to_json(v) {
            m.insert(k.to_string(), j);
        }
    }
    serde_json::Value::Object(m)
}

// --- Validation (exact Go text) --------------------------------------------

fn validate(cfg: &RawConfig) -> Result<(), String> {
    if cfg.providers.is_empty() {
        return Err("providers must define at least one provider".into());
    }
    // BTreeMap iterates in sorted name order, matching Go's sort.Strings(names).
    for (name, p) in &cfg.providers {
        if !provider_name_valid(name) {
            return Err(format!(
                "providers {} is not a valid provider name; use only letters, digits, '.', '_', or '-'",
                go_quote(name)
            ));
        }
        validate_provider(name, p)?;
    }
    if let Some(a) = &cfg.auto {
        validate_auto(a)?;
    }
    Ok(())
}

fn provider_name_valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn validate_provider(name: &str, p: &RawProvider) -> Result<(), String> {
    let prefix = format!("providers.{name}");

    match p.api.as_str() {
        API_CHAT | API_RESPONSES | API_MESSAGES | API_CODEX => {}
        other => {
            return Err(format!(
                "{prefix}.api must be one of {API_CHAT}, {API_RESPONSES}, {API_MESSAGES}, {API_CODEX}; got {}",
                go_quote(other)
            ));
        }
    }

    let is_codex = p.api == API_CODEX;
    if is_codex {
        if !p.env_key.is_empty() {
            return Err(format!(
                "{prefix}.env_key is not used by codex-app-server: authentication goes through codex login"
            ));
        }
        if !p.key.is_empty() {
            return Err(format!(
                "{prefix}.key is not used by codex-app-server: authentication goes through codex login"
            ));
        }
        if !p.base_url.is_empty() {
            return Err(format!("{prefix}.base_url is not used by codex-app-server"));
        }
        if p.max_output_tokens != 0 {
            return Err(format!(
                "{prefix}.max_output_tokens is not used by codex-app-server: the app-server applies its own limits"
            ));
        }
    } else {
        if !p.command.is_empty() {
            return Err(format!(
                "{prefix}.command is only valid for codex-app-server"
            ));
        }
        if p.env_key.is_empty() && p.key.is_empty() {
            return Err(format!("{prefix}.env_key or {prefix}.key is required"));
        }
        if !p.env_key.is_empty() && !p.key.is_empty() {
            return Err(format!(
                "{prefix}.env_key and {prefix}.key cannot both be set"
            ));
        }
    }

    if p.models.is_empty() {
        return Err(format!("{prefix}.models must list at least one model"));
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (i, m) in p.models.iter().enumerate() {
        if m.id.is_empty() {
            return Err(format!("{prefix}.models[{i}].id is required"));
        }
        if m.id == "auto" {
            return Err(format!(
                "{prefix}.models[{i}].id {} is reserved for the start tool's auto model selection; rename it",
                go_quote(&m.id)
            ));
        }
        if seen.contains(m.id.as_str()) {
            return Err(format!(
                "{prefix}.models has duplicate id {}",
                go_quote(&m.id)
            ));
        }
        seen.insert(m.id.as_str());
    }
    if !seen.contains(p.default_model.as_str()) {
        return Err(format!(
            "{prefix}.default_model {} must be one of {prefix}.models",
            go_quote(&p.default_model)
        ));
    }

    for (from, to) in &p.effort_map {
        if !valid_effort(from) || !valid_effort(to) {
            return Err(format!(
                "{prefix}.effort_map entry {} = {}: keys and values must be one of {EFFORT_VALUES_DISPLAY}",
                go_quote(from),
                go_quote(to)
            ));
        }
    }

    if p.max_output_tokens < 0 {
        return Err(format!("{prefix}.max_output_tokens must be positive"));
    }
    if p.api == API_MESSAGES && p.max_output_tokens == 0 {
        return Err(format!(
            "{prefix}.max_output_tokens must be set for {API_MESSAGES} (the API requires max_tokens); {DEFAULT_MAX_OUTPUT_TOKENS} is the usual value"
        ));
    }

    if let Some(eb) = p.extra_body.as_ref().filter(|m| !m.is_empty()) {
        if p.api != API_CHAT {
            return Err(format!(
                "{prefix}.extra_body is only supported for api = {}",
                go_quote(API_CHAT)
            ));
        }
        for name in eb.keys() {
            if name == "max_tokens" {
                return Err(format!(
                    "{prefix}.extra_body.max_tokens is set by the server from max_output_tokens; set {prefix}.max_output_tokens instead, or use extra_body.max_completion_tokens for a model that requires it"
                ));
            }
            if RESERVED_BODY_FIELDS.contains(&name.as_str()) {
                return Err(format!(
                    "{prefix}.extra_body.{name} is set by the server and cannot be overridden"
                ));
            }
        }
        if eb.contains_key("max_completion_tokens") && p.max_output_tokens != 0 {
            return Err(format!(
                "{prefix} sets both max_output_tokens and extra_body.max_completion_tokens, which would send both limits; keep max_completion_tokens and remove max_output_tokens, or the other way round"
            ));
        }
    }

    if p.max_nudges < 0 || p.max_nudges > MAX_NUDGES_LIMIT {
        return Err(format!(
            "{prefix}.max_nudges must be between 0 and {MAX_NUDGES_LIMIT}"
        ));
    }
    Ok(())
}

fn valid_effort(v: &str) -> bool {
    EFFORT_VALUES.contains(&v)
}

fn validate_auto(a: &RawAuto) -> Result<(), String> {
    if a.env_key.is_empty() && a.key.is_empty() {
        return Err("auto.env_key or auto.key is required".into());
    }
    if !a.env_key.is_empty() && !a.key.is_empty() {
        return Err("auto.env_key and auto.key cannot both be set".into());
    }
    if a.model.is_empty() {
        return Err("auto.model is required".into());
    }
    if !a.url.is_empty() && !a.base_url.is_empty() {
        return Err("auto.url and auto.base_url cannot both be set".into());
    }
    let (endpoint, field) = if !a.url.is_empty() {
        (a.url.as_str(), "auto.url")
    } else {
        (a.base_url.as_str(), "auto.base_url")
    };
    if !valid_http_url(endpoint) {
        return Err(format!(
            "{field} {} must be an absolute http or https URL",
            go_quote(endpoint)
        ));
    }
    if a.timeout_seconds < 0 || a.timeout_seconds > MAX_AUTO_TIMEOUT_SECONDS {
        return Err(format!(
            "auto.timeout_seconds must be 0 (default) or between 1 and {MAX_AUTO_TIMEOUT_SECONDS}"
        ));
    }
    Ok(())
}

/// Subset of Go `net/url.Parse` used by config validation: an absolute http or
/// https URL with a non-empty host and no query (RawQuery/ForceQuery) or
/// fragment. No `url` crate dependency is available, so this parses the shape
/// directly, mirroring the `url.Parse` checks that can reject a config URL:
/// control bytes, invalid `%` escapes, the scheme (lowercased), the optional
/// port, and IPv6 brackets.
fn valid_http_url(endpoint: &str) -> bool {
    // Go: stringContainsCTLByte rejects any byte < 0x20 or == 0x7f anywhere.
    // (A tab is legal in a TOML basic string but not in a URL.)
    if endpoint.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    // Go: unescape() rejects any '%' not followed by two hex digits.
    if !valid_percent_escapes(endpoint) {
        return false;
    }
    let Some(colon) = endpoint.find(':') else {
        return false;
    };
    let scheme = &endpoint[..colon];
    if scheme.is_empty()
        || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }
    // Go: url.Scheme = strings.ToLower(url.Scheme) before comparing.
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let Some(after) = endpoint[colon + 1..].strip_prefix("//") else {
        return false; // opaque URL: Host is empty
    };
    let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..auth_end];
    // Go: parseAuthority splits userinfo at the last '@' before parseHost.
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    if hostport.is_empty() {
        return false; // u.Host == ""
    }
    if !valid_host_port(hostport) {
        return false;
    }
    let tail = &after[auth_end..];
    let (before_frag, fragment) = match tail.split_once('#') {
        Some((before, frag)) => (before, frag),
        None => (tail, ""),
    };
    if before_frag.contains('?') {
        return false; // RawQuery != "" or ForceQuery
    }
    if !fragment.is_empty() {
        return false; // u.Fragment != ""
    }
    true
}

/// Every '%' must be followed by two hex digits (Go's `unescape`).
fn valid_percent_escapes(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() || !b[i + 1].is_ascii_hexdigit() || !b[i + 2].is_ascii_hexdigit() {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Go's `validOptionalPort`: empty, or a ':' followed by digits only.
fn valid_optional_port(port: &str) -> bool {
    let Some(digits) = port.strip_prefix(':') else {
        return port.is_empty();
    };
    digits.bytes().all(|b| b.is_ascii_digit())
}

/// Go's `parseHost` port/bracket validation (enough for the config's needs).
fn valid_host_port(hostport: &str) -> bool {
    if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 IP-literal; Go also validates the address with netip.ParseAddr.
        let Some(close) = rest.find(']') else {
            return false; // missing ']'
        };
        let inner = &rest[..close];
        if inner.is_empty()
            || !inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.' | b'%'))
        {
            return false;
        }
        return valid_optional_port(&rest[close + 1..]);
    }
    // For http/https Go uses the FIRST colon, so multiple colons are rejected.
    match hostport.find(':') {
        Some(i) => valid_optional_port(&hostport[i..]),
        None => true,
    }
}

// --- Raw -> public mapping -------------------------------------------------

fn build_config(path: PathBuf, raw: RawConfig) -> Config {
    let providers = raw
        .providers
        .into_iter()
        .map(|(name, p)| {
            let api = Api::parse(&p.api).unwrap_or(Api::ChatCompletions);
            let max_output_tokens = if p.max_output_tokens == 0 {
                None
            } else {
                Some(p.max_output_tokens)
            };
            let max_nudges = if p.max_nudges == 0 {
                None
            } else {
                Some(p.max_nudges as usize)
            };
            let models = p
                .models
                .into_iter()
                .map(|m| ModelConfig {
                    id: m.id,
                    description: m.description,
                })
                .collect();
            let pc = ProviderConfig {
                api,
                base_url: p.base_url,
                env_key: opt_str(p.env_key),
                key: opt_str(p.key),
                command: opt_str(p.command),
                default_model: p.default_model,
                models,
                effort_map: p.effort_map,
                max_output_tokens,
                max_nudges,
                extra_body: p.extra_body,
            };
            (name, pc)
        })
        .collect();

    let auto = raw.auto.map(|a| AutoConfig {
        base_url: a.base_url,
        url: a.url,
        env_key: opt_str(a.env_key),
        key: opt_str(a.key),
        model: a.model,
        timeout_seconds: a.timeout_seconds as u64,
    });

    Config {
        path,
        providers,
        auto,
    }
}
