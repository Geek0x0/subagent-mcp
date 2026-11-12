//! Adapter registry, construction seam, and process-wide shutdown hooks.
//!
//! Ports Go `internal/provider/factory.go`. Adapters register a constructor
//! against their config `api` identifier; [`new`] looks it up and builds the
//! provider. Adapters also register a [`ShutdownFunc`] via
//! [`register_shutdown`]; the server calls [`shutdown`] once as the process
//! exits, running every hook in registration order. This module is
//! deliberately behavior-free beyond registration and lookup: it holds no TLS,
//! HTTP, SSE, or adapter logic. Later adapter tasks (chat-completions,
//! responses, messages, codex-app-server) call [`register`] and
//! [`register_shutdown`] from their initialization.

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

/// A hook that releases the process-wide resources one adapter family owns,
/// such as a shared child process (Go `ShutdownFunc`). `Arc` lets [`shutdown`]
/// snapshot the list and call hooks outside the registry lock, like Go's
/// `RLock` copy.
pub type ShutdownFunc = Arc<dyn Fn() + Send + Sync>;

/// `api` identifier -> constructor, populated by adapters at startup.
static REGISTRY: LazyLock<Mutex<HashMap<String, Constructor>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Shutdown hooks, in registration order (Go `shutdowns []ShutdownFunc`).
static SHUTDOWNS: LazyLock<Mutex<Vec<ShutdownFunc>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Make an adapter available for a config `api` value.
pub fn register(api: impl Into<String>, ctor: Constructor) {
    REGISTRY.lock().unwrap().insert(api.into(), ctor);
}

/// Register a process-wide cleanup hook. `None` is ignored (Go ignores a nil
/// `ShutdownFunc`), so an adapter whose cleanup only exists on some platforms
/// can pass `None`.
pub fn register_shutdown(f: Option<ShutdownFunc>) {
    let Some(f) = f else {
        return;
    };
    SHUTDOWNS.lock().unwrap().push(f);
}

/// Release every registered process-wide resource, in registration order. It
/// is safe when no adapter was ever constructed, and every call runs every
/// registration (Go `Shutdown`: the exit path is reachable from both a signal
/// and a closed client, with nothing else guarding it).
pub fn shutdown() {
    // Snapshot under the lock, then call outside it so a hook may itself
    // register or inspect the registry without deadlocking.
    let pending: Vec<ShutdownFunc> = SHUTDOWNS.lock().unwrap().clone();
    for f in pending {
        f();
    }
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
