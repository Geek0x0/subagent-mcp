//! Codex app-server adapter: JSON-RPC client, per-command child pool, and the
//! optional thread/model/auth capabilities.
//!
//! Ports Go `internal/provider/codexappserver`. Task 13A owns the transport half
//! (`client.go`, `pool.go`, `process.go`); [`thread`] and [`events`] own the
//! thread lifecycle, approvals, event mapping, model listing, and auth check.
//! [`CodexAdapter`] keeps [`Provider::turn`] unsupported because the app-server
//! protocol runs whole threads.

pub mod client;
pub mod events;
pub mod pool;
pub mod thread;

use std::sync::{Arc, Once};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::ProviderConfig;
use crate::provider::{
    Agent, AuthChecker, AuthStatus, DeltaSink, ModelInfo, ModelLister, Provider, ProviderError,
    Thread, ThreadOptions, TurnRequest, TurnResult,
};
use crate::util::run::RunContext;

/// Delegates whole threads to a shared Codex app-server child.
pub struct CodexAdapter {
    name: String,
    cfg: ProviderConfig,
    pool: Arc<pool::Pool>,
}

impl CodexAdapter {
    /// Build an adapter. The child starts lazily and uses Codex's own login.
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        register_shutdown();
        Ok(Self {
            name: name.to_string(),
            cfg: cfg.clone(),
            pool: pool::Pool::for_command(cfg.command.as_deref().unwrap_or_default()),
        })
    }

    /// The configured provider name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The shared per-command pool this adapter uses.
    pub fn pool(&self) -> &Arc<pool::Pool> {
        &self.pool
    }

    /// The resolved provider config.
    pub fn config(&self) -> &ProviderConfig {
        &self.cfg
    }

    /// A context for capability calls that have no caller-supplied run context.
    fn capability_ctx() -> RunContext {
        RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        }
    }
}

#[async_trait]
impl Provider for CodexAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        Err(ProviderError {
            message: "codex-app-server runs whole threads; Turn is not supported".to_string(),
        })
    }
}

#[async_trait]
impl Agent for CodexAdapter {
    async fn start_thread(
        &self,
        ctx: &RunContext,
        opts: ThreadOptions,
    ) -> Result<Box<dyn Thread>, ProviderError> {
        Ok(Box::new(
            thread::start_thread(ctx, self.pool.clone(), opts).await?,
        ))
    }
}

#[async_trait]
impl ModelLister for CodexAdapter {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let ctx = Self::capability_ctx();
        let conn = self.pool.get(&ctx).await.map_err(thread::provider_error)?;
        let mut models = Vec::new();
        let mut cursor = String::new();
        loop {
            // Go sends `{}` on the first page and `{"cursor": ...}` afterwards.
            let params = if cursor.is_empty() {
                Value::Object(Map::new())
            } else {
                json!({"cursor": cursor})
            };
            let result = match conn.client.call(&ctx, "model/list", Some(params)).await {
                Ok(result) => result,
                Err(error) => return Err(thread::provider_error(conn.failure(error).await)),
            };
            if let Some(data) = result.get("data").and_then(Value::as_array) {
                for model in data {
                    if let Some(id) = model.get("id").and_then(Value::as_str) {
                        models.push(ModelInfo {
                            id: id.to_string(),
                            description: String::new(),
                        });
                    }
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                return Ok(models);
            }
        }
    }
}

#[async_trait]
impl AuthChecker for CodexAdapter {
    async fn check_auth(&self) -> Result<AuthStatus, ProviderError> {
        let ctx = Self::capability_ctx();
        let conn = self.pool.get(&ctx).await.map_err(thread::provider_error)?;
        let mut status = AuthStatus::default();
        if let Some((_, rest)) = conn.user_agent.split_once('/') {
            let version = rest.split(' ').next().unwrap_or("");
            if !version.is_empty() {
                status.version = Some(format!("codex-cli {version}"));
            }
        }
        let result = match conn
            .client
            .call(&ctx, "account/read", Some(json!({})))
            .await
        {
            Ok(result) => result,
            Err(error) => return Err(thread::provider_error(conn.failure(error).await)),
        };
        let account = result.get("account").filter(|account| !account.is_null());
        let requires_auth = result
            .get("requiresOpenaiAuth")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if requires_auth && account.is_none() {
            return Err(ProviderError {
                message: "not logged in; run codex login".to_string(),
            });
        }
        if let Some(account) = account {
            let kind = account.get("type").and_then(Value::as_str).unwrap_or("");
            let plan = account
                .get("planType")
                .and_then(Value::as_str)
                .unwrap_or("");
            status.summary = format!("{kind} ({plan})");
        }
        Ok(status)
    }
}

/// Register the pool shutdown hook once, like Go's package `init`.
fn register_shutdown() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        crate::provider::factory::register_shutdown(Some(Arc::new(pool::shutdown)));
    });
}
