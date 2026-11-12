//! Adapter registry and construction seam.
//!
//! Ports Go `internal/provider/factory.go`. Adapters register a constructor
//! against their config `api` identifier; [`new`] looks it up and builds the
//! provider. This module is deliberately behavior-free beyond registration and
//! lookup: it holds no TLS, HTTP, SSE, or adapter logic. Later adapter tasks
//! (chat-completions, responses, messages, codex-app-server) call [`register`]
//! from their initialization.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::config::ProviderConfig;
use crate::provider::{Provider, ProviderError};

/// Constructor for one configured provider. It receives the provider name, the
/// resolved config, and the API key, and returns a boxed [`Provider`].
#[allow(clippy::type_complexity)] // the boxed constructor shape is intentional
pub type Constructor = Arc<
    dyn Fn(&str, &ProviderConfig, &str) -> Result<Arc<dyn Provider>, ProviderError> + Send + Sync,
>;

/// `api` identifier -> constructor, populated by adapters at startup.
static REGISTRY: LazyLock<Mutex<HashMap<String, Constructor>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make an adapter available for a config `api` value.
pub fn register(api: impl Into<String>, ctor: Constructor) {
    REGISTRY.lock().unwrap().insert(api.into(), ctor);
}

/// Look up the constructor registered for `api`, if any. Kept separate from
/// [`new`] so the lookup itself is observable without building a provider.
pub fn lookup(api: &str) -> Option<Constructor> {
    REGISTRY.lock().unwrap().get(api).cloned()
}

/// Build the adapter for `cfg.api`. The error text is the exact
/// subagent-mcp-authored string (`fmt.Errorf("unsupported api %q ...")` in Go),
/// so a missing adapter package is reported verbatim to the operator.
pub fn new(
    name: &str,
    cfg: &ProviderConfig,
    api_key: &str,
) -> Result<Arc<dyn Provider>, ProviderError> {
    let api = cfg.api.as_str();
    match lookup(api) {
        Some(ctor) => ctor(name, cfg, api_key),
        None => Err(ProviderError {
            message: format!("unsupported api {api:?} (is its adapter package imported?)"),
        }),
    }
}
