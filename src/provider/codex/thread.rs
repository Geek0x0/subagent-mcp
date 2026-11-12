//! Codex thread lifecycle, approvals, and event forwarding.
//!
//! Ports Go `internal/provider/codexappserver/thread.go`. A thread owns one
//! app-server subscription for its whole life. A router task forwards the
//! thread's notifications into the active turn's queue and answers every server
//! request no turn consumes with `-32601 not supported by subagent-mcp`, even
//! while a turn is idle or blocked in an approval.
//!
//! Turns are serialized by the session. The router may still observe a
//! `turn/completed` that the run never dequeued; that completion ends the turn,
//! so a later run does not try to interrupt a turn that is already over.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::policy::{ApprovalPolicy, Sandbox};
use crate::provider::codex::client::{
    Client, ClientError, Message, Subscription, UNSUPPORTED_REQUEST, ctx_error, wait_ctx,
};
use crate::provider::codex::events::{self, NotificationParams};
use crate::provider::codex::pool::{Conn, Pool};
use crate::provider::{ProviderError, Thread, ThreadCallbacks, ThreadOptions};
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::run::RunContext;

/// One shared budget for settling a turn left running by an earlier run.
const TURN_SETTLE_BUDGET: Duration = Duration::from_secs(30);
/// After cancellation, how long the turn stays interruptible.
const CANCEL_INTERRUPT_BUDGET: Duration = Duration::from_secs(30);
/// Best-effort interrupt RPC budget.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Close's shared interrupt/unsubscribe budget.
const CLOSE_BUDGET: Duration = Duration::from_secs(5);

/// Exact subagent-mcp-authored error after a thread is closed.
const THREAD_CLOSED: &str = "codex thread closed; start a new session";
/// Exact subagent-mcp-authored error when the app-server generation changed
/// before the thread's first turn completed.
const SESSION_LOST: &str = "codex session lost; start a new session";
/// Exact subagent-mcp-authored error when an earlier turn is still running.
const STILL_RUNNING: &str = "codex is still running the previous turn of this thread; wait for it to finish or start a new session";

/// Delegates one Codex thread to a pooled app-server connection.
pub struct CodexThread {
    id: String,
    client: Arc<Client>,
    generation: u64,
    pool: Arc<Pool>,
    opts: ThreadOptions,
    inner: Arc<Mutex<ThreadInner>>,
    closed_signal: CancellationToken,
    router_cancel: CancellationToken,
    router: Mutex<Option<JoinHandle<()>>>,
}

struct ThreadInner {
    conn: Arc<Conn>,
    completed: bool,
    closed: bool,
    active: Option<ActiveTurn>,
    stray: Option<StrayTurn>,
}

struct ActiveTurn {
    /// Guarded by [`ThreadInner`].
    id: String,
    /// Shared with Close, which may outlive this turn being cleared.
    learned_id: Arc<Mutex<String>>,
    queue: Subscription,
    cancel: CancellationToken,
    id_known: CancellationToken,
    done: CancellationToken,
    completed: bool,
    /// `turn/start` ended without a server verdict, so the turn may exist.
    uncertain: bool,
    /// Close gave up waiting for the id; Run must interrupt once it arrives.
    close_timed_out: bool,
}

impl ActiveTurn {
    fn learn(&mut self, id: String) {
        if self.id.is_empty() && !id.is_empty() {
            self.id.clone_from(&id);
            *self.learned_id.lock().unwrap() = id;
            self.id_known.cancel();
        }
    }
}

/// A turn that may still be running after its Run returned.
struct StrayTurn {
    id: String,
    generation: u64,
    id_known: CancellationToken,
    done: CancellationToken,
}

impl CodexThread {
    /// The app-server thread id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The client of the connection that started this thread.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// The pool generation this thread started on.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    fn active_id(&self) -> String {
        self.inner
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|active| active.id.clone())
            .unwrap_or_default()
    }

    fn start_router(&self, conn: &Arc<Conn>) {
        let subscription = conn.client.subscribe(&self.id);
        let inner = self.inner.clone();
        let cancel = self.router_cancel.clone();
        let thread_id = self.id.clone();
        let client = conn.client.clone();
        let conn = conn.clone();
        let handle = tokio::spawn(async move {
            let background = RunContext {
                cancel: CancellationToken::new(),
                deadline: None,
            };
            loop {
                let message = tokio::select! {
                    _ = cancel.cancelled() => return,
                    message = subscription.next(&background) => message,
                };
                match message {
                    Ok(message) => {
                        route_message(&inner, &conn, &client, &thread_id, message).await;
                    }
                    Err(error) => {
                        // The subscription is closed (unsubscribe or EOF). Close
                        // the turn queue too, so a Run blocked in queue.next wakes
                        // and rejects any requests still queued for it.
                        let queued = {
                            let mut inner = inner.lock().unwrap();
                            if Arc::ptr_eq(&inner.conn, &conn) {
                                inner
                                    .active
                                    .as_mut()
                                    .map(|active| active.queue.close(error))
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            }
                        };
                        for message in queued {
                            if let Some(id) = &message.id {
                                let _ = client.respond_error(id, -32601, UNSUPPORTED_REQUEST).await;
                            }
                        }
                        return;
                    }
                }
            }
        });
        if let Some(old) = self.router.lock().unwrap().replace(handle) {
            old.abort();
        }
    }

    async fn run_impl(
        &self,
        ctx: RunContext,
        prompt: String,
        effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        if self.is_closed() {
            return Err(message_error(THREAD_CLOSED));
        }
        let conn = self.pool.get(&ctx).await.map_err(provider_error)?;
        let (old, completed, closed) = {
            let inner = self.inner.lock().unwrap();
            (inner.conn.clone(), inner.completed, inner.closed)
        };
        if closed {
            return Err(message_error(THREAD_CLOSED));
        }
        if conn.generation != old.generation {
            if !completed {
                return Err(message_error(SESSION_LOST));
            }
            let mut params = thread_settings(&self.opts);
            params.insert("threadId".to_string(), json!(self.id));
            if let Err(error) = conn
                .client
                .call(&ctx, "thread/resume", Some(Value::Object(params)))
                .await
            {
                return Err(provider_error(conn.failure(error).await));
            }
        }
        self.settle_stray(&ctx, &conn).await?;

        // The turn's own context outlives the caller's cancellation by 30s so
        // the turn/start reply can still be read and the turn interrupted.
        let run_cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let run_ctx = RunContext {
            cancel: run_cancel.clone(),
            deadline: None,
        };
        let watcher = spawn_cancel_watcher(&ctx, &run_cancel, &stop);

        let active = ActiveTurn {
            id: String::new(),
            learned_id: Arc::new(Mutex::new(String::new())),
            queue: Subscription::new(),
            cancel: run_cancel.clone(),
            id_known: CancellationToken::new(),
            done: CancellationToken::new(),
            completed: false,
            uncertain: false,
            close_timed_out: false,
        };
        let queue = active.queue.clone();

        let switched = {
            let inner = self.inner.lock().unwrap();
            !Arc::ptr_eq(&inner.conn, &conn)
        };
        if switched {
            let old_client = { self.inner.lock().unwrap().conn.client.clone() };
            old_client.unsubscribe(&self.id).await;
            self.inner.lock().unwrap().conn = conn.clone();
            self.start_router(&conn);
        }
        let installed = {
            let mut inner = self.inner.lock().unwrap();
            if inner.closed {
                false
            } else {
                inner.active = Some(active);
                true
            }
        };
        if !installed {
            stop.cancel();
            watcher.abort();
            return Err(message_error(THREAD_CLOSED));
        }

        let result = self
            .run_turn(&ctx, &run_ctx, &conn, &queue, prompt, effort, &callbacks)
            .await;
        self.finish(&conn).await;
        stop.cancel();
        watcher.abort();
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        ctx: &RunContext,
        run_ctx: &RunContext,
        conn: &Arc<Conn>,
        queue: &Subscription,
        prompt: String,
        effort: Option<String>,
        callbacks: &ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        let mut params = Map::new();
        params.insert("threadId".to_string(), json!(self.id));
        params.insert(
            "input".to_string(),
            json!([{"type": "text", "text": prompt}]),
        );
        params.insert("effort".to_string(), json!(effort.unwrap_or_default()));
        if self.opts.sandbox == Sandbox::WorkspaceWrite && !self.opts.writable_roots.is_empty() {
            params.insert(
                "sandboxPolicy".to_string(),
                json!({
                    "type": "workspaceWrite",
                    "writableRoots": self
                        .opts
                        .writable_roots
                        .iter()
                        .map(|root| root.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                    "networkAccess": false,
                }),
            );
        }
        let result = match conn
            .client
            .call(run_ctx, "turn/start", Some(Value::Object(params)))
            .await
        {
            Ok(result) => result,
            Err(error) => return Err(self.start_failed(ctx, conn, error).await),
        };
        let turn_id = match required_id(&result, "turn/start", "turn") {
            Ok(turn_id) => turn_id,
            Err(message) => {
                // The server may still have started a turn, so record it as
                // uncertain and let the stray-turn path settle it.
                return Err(self
                    .start_failed(ctx, conn, ClientError::message(message))
                    .await);
            }
        };
        let (closed, late_interrupt) = {
            let mut inner = self.inner.lock().unwrap();
            let late = match inner.active.as_mut() {
                Some(active) => {
                    active.learn(turn_id.clone());
                    active.close_timed_out
                }
                None => false,
            };
            (inner.closed, late)
        };
        if closed {
            if late_interrupt {
                interrupt_turn(&conn.client, &self.id, &turn_id, INTERRUPT_TIMEOUT).await;
            }
            return Err(message_error(THREAD_CLOSED));
        }

        let mut last = String::new();
        let mut final_text: Option<String> = None;
        let mut paths: HashMap<String, String> = HashMap::new();
        loop {
            if self.is_closed() {
                return Err(message_error(THREAD_CLOSED));
            }
            if let Some(error) = ctx_error(ctx) {
                self.interrupt(run_ctx, conn, queue, callbacks).await;
                return Err(provider_error(error));
            }
            let message = match queue.next(ctx).await {
                Ok(message) => message,
                Err(error) => {
                    if ctx_error(ctx).is_some() {
                        continue;
                    }
                    return Err(self.run_error(ctx, conn, error).await);
                }
            };
            if message.id.is_some() {
                if let Err(error) = self.approve(ctx, conn, &message, &paths, callbacks).await {
                    return Err(self.run_error(ctx, conn, error).await);
                }
                continue;
            }
            let active_id = self.active_id();
            let Some(params) = turn_notification(&message, &active_id) else {
                continue;
            };
            if message.method.as_deref() == Some("item/started") && params.item.kind == "fileChange"
            {
                paths.insert(params.item.id.clone(), params.item.paths().join(", "));
            }
            if message.method.as_deref() == Some("item/completed")
                && params.item.kind == "agentMessage"
            {
                last.clone_from(&params.item.text);
                if params.item.phase == "final_answer" {
                    final_text = Some(last.clone());
                }
            }
            self.emit(&message, callbacks);
            if message.method.as_deref() == Some("turn/completed") {
                self.mark_completed();
                if let Some(error) = ctx_error(ctx) {
                    return Err(provider_error(error));
                }
                if params.turn.status == "failed" {
                    return Err(message_error(params.turn.error.message));
                }
                return Ok(final_text.unwrap_or(last));
            }
        }
    }

    fn emit(&self, message: &Message, callbacks: &ThreadCallbacks) {
        let Some(raw) = message.params.as_deref() else {
            return;
        };
        let Ok(params) = serde_json::from_str::<Value>(raw.get()) else {
            return;
        };
        let method = message.method.as_deref().unwrap_or("");
        if let Some(event) = events::map_notification(method, &params) {
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (callbacks.emit)(&kind, event);
        }
    }

    async fn approve(
        &self,
        ctx: &RunContext,
        conn: &Arc<Conn>,
        message: &Message,
        paths: &HashMap<String, String>,
        callbacks: &ThreadCallbacks,
    ) -> Result<(), ClientError> {
        let Some(id) = message.id.as_ref() else {
            return Ok(());
        };
        let method = message.method.as_deref().unwrap_or("");
        let Some(params) = events::parse_approval(message.params.as_deref()) else {
            return reject_request(conn, id).await;
        };
        if params.turn_id != self.active_id() || !approval_method(method) {
            return reject_request(conn, id).await;
        }
        let mut command = params.command.clone().unwrap_or_default();
        if command.is_empty() {
            command = params.network_approval_context.host.clone();
        }
        let mut request = ApprovalRequest {
            thread_id: self.id.clone(),
            tool: "shell".to_string(),
            command: non_empty(command),
            path: None,
            reason: params.reason.clone(),
        };
        if method == "item/fileChange/requestApproval" {
            request.tool = "apply_patch".to_string();
            request.command = None;
            request.path = non_empty(paths.get(&params.item_id).cloned().unwrap_or_default());
        }
        let client_done = conn.client.done();
        let decision = decide_approval(
            ctx,
            callbacks.approve.as_ref(),
            request,
            &client_done,
            &self.closed_signal,
        )
        .await;
        conn.client.respond(id, json!({"decision": decision})).await
    }

    /// Stop the turn after the caller's cancellation and drain it to completion.
    async fn interrupt(
        &self,
        run_ctx: &RunContext,
        conn: &Arc<Conn>,
        queue: &Subscription,
        callbacks: &ThreadCallbacks,
    ) {
        let ctx = run_ctx.child();
        let call_ctx = ctx.child();
        let client = conn.client.clone();
        let thread_id = self.id.clone();
        let turn_id = self.active_id();
        let call_thread = thread_id.clone();
        let call_turn = turn_id.clone();
        let spawned_ctx = call_ctx.clone();
        let handle = tokio::spawn(async move {
            interrupt_turn_ctx(&client, &call_thread, &call_turn, &spawned_ctx).await;
        });
        let deny = ThreadCallbacks {
            emit: callbacks.emit.clone(),
            approve: Arc::new(Deny),
        };
        loop {
            let message = match queue.next(&ctx).await {
                Ok(message) => message,
                Err(_) => break,
            };
            if message.id.is_some() {
                // No new elicitations during interrupt; valid approvals are denied.
                let _ = self
                    .approve(&ctx, conn, &message, &HashMap::new(), &deny)
                    .await;
                continue;
            }
            let active_id = self.active_id();
            if turn_notification(&message, &active_id).is_none() {
                continue;
            }
            self.emit(&message, callbacks);
            if message.method.as_deref() == Some("turn/completed") {
                self.mark_completed();
                break;
            }
        }
        call_ctx.cancel.cancel();
        let _ = handle.await;
    }

    fn mark_completed(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.completed = true;
        if let Some(active) = inner.active.as_mut() {
            active.completed = true;
        }
    }

    async fn run_error(
        &self,
        ctx: &RunContext,
        conn: &Arc<Conn>,
        error: ClientError,
    ) -> ProviderError {
        let closed = self.inner.lock().unwrap().closed;
        if closed {
            return message_error(THREAD_CLOSED);
        }
        if let Some(error) = ctx_error(ctx) {
            return provider_error(error);
        }
        provider_error(conn.failure(error).await)
    }

    async fn start_failed(
        &self,
        ctx: &RunContext,
        conn: &Arc<Conn>,
        error: ClientError,
    ) -> ProviderError {
        let (id, closed) = {
            let mut inner = self.inner.lock().unwrap();
            if error.as_rpc().is_none()
                && let Some(active) = inner.active.as_mut()
            {
                active.uncertain = true;
            }
            (
                inner
                    .active
                    .as_ref()
                    .map(|active| active.id.clone())
                    .unwrap_or_default(),
                inner.closed,
            )
        };
        if !id.is_empty() && !closed && ctx_error(ctx).is_some() {
            interrupt_turn(&conn.client, &self.id, &id, INTERRUPT_TIMEOUT).await;
        }
        self.run_error(ctx, conn, error).await
    }

    async fn finish(&self, conn: &Arc<Conn>) {
        let queued = {
            let mut inner = self.inner.lock().unwrap();
            let Some(active) = inner.active.take() else {
                return;
            };
            if !active.completed && !inner.closed && (!active.id.is_empty() || active.uncertain) {
                let stray = StrayTurn {
                    id: active.id.clone(),
                    generation: conn.generation,
                    id_known: CancellationToken::new(),
                    done: CancellationToken::new(),
                };
                if !stray.id.is_empty() {
                    stray.id_known.cancel();
                }
                inner.stray = Some(stray);
            }
            active.done.cancel();
            active.queue.close(ClientError::message("turn finished"))
        };
        for message in queued {
            if let Some(id) = &message.id {
                let _ = conn
                    .client
                    .respond_error(id, -32601, UNSUPPORTED_REQUEST)
                    .await;
            }
        }
    }

    async fn settle_stray(&self, ctx: &RunContext, conn: &Arc<Conn>) -> Result<(), ProviderError> {
        let (id, id_known, done) = {
            let mut inner = self.inner.lock().unwrap();
            if inner
                .stray
                .as_ref()
                .is_some_and(|stray| stray.generation != conn.generation)
            {
                // The app-server process that ran it is gone.
                inner.stray = None;
            }
            match &inner.stray {
                Some(stray) => (stray.id.clone(), stray.id_known.clone(), stray.done.clone()),
                None => return Ok(()),
            }
        };
        let settle = with_timeout(ctx, TURN_SETTLE_BUDGET);
        if !id.is_empty() {
            return self.wait_stray(&settle, conn, &id, &done).await;
        }
        tokio::select! {
            _ = done.cancelled() => Ok(()),
            _ = id_known.cancelled() => {
                let id = self
                    .inner
                    .lock()
                    .unwrap()
                    .stray
                    .as_ref()
                    .map(|stray| stray.id.clone())
                    .unwrap_or_default();
                self.wait_stray(&settle, conn, &id, &done).await
            }
            _ = wait_ctx(&settle) => Err(settle_error(&settle)),
        }
    }

    async fn wait_stray(
        &self,
        settle: &RunContext,
        conn: &Arc<Conn>,
        id: &str,
        done: &CancellationToken,
    ) -> Result<(), ProviderError> {
        let call_ctx = settle.child();
        let client = conn.client.clone();
        let thread_id = self.id.clone();
        let turn_id = id.to_string();
        let spawned_ctx = call_ctx.clone();
        let handle = tokio::spawn(async move {
            interrupt_turn_ctx(&client, &thread_id, &turn_id, &spawned_ctx).await;
        });
        // Wait for the completion while the interrupt RPC is in flight: the turn
        // may finish before, or long after, the RPC is answered.
        let result = tokio::select! {
            _ = done.cancelled() => Ok(()),
            _ = wait_ctx(settle) => Err(settle_error(settle)),
        };
        call_ctx.cancel.cancel();
        let _ = handle.await;
        result
    }

    async fn close_impl(&self) -> Result<(), ProviderError> {
        let (conn, active_id, active_cancel, id_known, done, learned_id, active_present, stray_id) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.closed {
                return Ok(());
            }
            inner.closed = true;
            self.closed_signal.cancel();
            let conn = inner.conn.clone();
            let (active_id, active_cancel, id_known, done, learned_id, active_present) =
                match &inner.active {
                    Some(active) => (
                        active.id.clone(),
                        Some(active.cancel.clone()),
                        Some(active.id_known.clone()),
                        Some(active.done.clone()),
                        Some(active.learned_id.clone()),
                        true,
                    ),
                    None => (String::new(), None, None, None, None, false),
                };
            let stray_id = inner
                .stray
                .as_ref()
                .map(|stray| stray.id.clone())
                .unwrap_or_default();
            (
                conn,
                active_id,
                active_cancel,
                id_known,
                done,
                learned_id,
                active_present,
                stray_id,
            )
        };
        let mut turn_id = if active_present { active_id } else { stray_id };
        if active_present
            && let Some(cancel) = &active_cancel
            && !turn_id.is_empty()
        {
            cancel.cancel();
        }
        let close_ctx = RunContext {
            cancel: CancellationToken::new(),
            deadline: Some(tokio::time::Instant::now() + CLOSE_BUDGET),
        };
        if active_present && turn_id.is_empty() {
            // turn/start is still in flight. Cancelling it would discard the
            // reply that names the turn, leaving Codex running it unobserved.
            let id_known = id_known.expect("active turn has an id signal");
            let done = done.expect("active turn has a done signal");
            let timed_out = tokio::select! {
                _ = id_known.cancelled() => {
                    if let Some(cancel) = &active_cancel {
                        cancel.cancel();
                    }
                    false
                }
                _ = done.cancelled() => false,
                _ = wait_ctx(&close_ctx) => true,
            };
            if timed_out && let Some(cancel) = active_cancel.clone() {
                tokio::spawn(async move {
                    tokio::time::sleep(TURN_SETTLE_BUDGET).await;
                    cancel.cancel();
                });
            }
            turn_id = learned_id
                .map(|learned| learned.lock().unwrap().clone())
                .unwrap_or_default();
            let close_timed_out = turn_id.is_empty() && timed_out;
            if let Some(active) = self.inner.lock().unwrap().active.as_mut() {
                active.close_timed_out = close_timed_out;
            }
        }
        conn.client.unsubscribe(&self.id).await;
        if !turn_id.is_empty() && conn.client.error().is_none() {
            let _ = conn
                .client
                .call(
                    &close_ctx,
                    "turn/interrupt",
                    Some(json!({"threadId": self.id, "turnId": turn_id})),
                )
                .await;
        }
        let _ = conn
            .client
            .call(
                &close_ctx,
                "thread/unsubscribe",
                Some(json!({"threadId": self.id})),
            )
            .await;
        Ok(())
    }
}

impl Drop for CodexThread {
    fn drop(&mut self) {
        self.router_cancel.cancel();
        if let Ok(mut router) = self.router.lock()
            && let Some(handle) = router.take()
        {
            handle.abort();
        }
    }
}

#[async_trait::async_trait]
impl Thread for CodexThread {
    async fn run(
        &self,
        ctx: RunContext,
        prompt: String,
        effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        self.run_impl(ctx, prompt, effort, callbacks).await
    }

    async fn close(&self) -> Result<(), ProviderError> {
        self.close_impl().await
    }
}

/// Start one thread on a pooled app-server connection.
pub async fn start_thread(
    ctx: &RunContext,
    pool: Arc<Pool>,
    opts: ThreadOptions,
) -> Result<CodexThread, ProviderError> {
    let conn = pool.get(ctx).await.map_err(provider_error)?;
    let mut params = thread_settings(&opts);
    params.insert("ephemeral".to_string(), json!(opts.ephemeral));
    let result = match conn
        .client
        .call(ctx, "thread/start", Some(Value::Object(params)))
        .await
    {
        Ok(result) => result,
        Err(error) => return Err(provider_error(conn.failure(error).await)),
    };
    // A required response that cannot name its thread is a protocol failure:
    // continuing with an empty id would subscribe to nothing and accept any
    // later notification.
    let id = required_id(&result, "thread/start", "thread").map_err(message_error)?;
    let inner = Arc::new(Mutex::new(ThreadInner {
        conn: conn.clone(),
        completed: false,
        closed: false,
        active: None,
        stray: None,
    }));
    let thread = CodexThread {
        id,
        client: conn.client.clone(),
        generation: conn.generation,
        pool,
        opts,
        inner,
        closed_signal: CancellationToken::new(),
        router_cancel: CancellationToken::new(),
        router: Mutex::new(None),
    };
    thread.start_router(&conn);
    Ok(thread)
}

/// Convert a client failure into the provider error surface.
pub(crate) fn provider_error(error: ClientError) -> ProviderError {
    ProviderError {
        message: error.to_string(),
    }
}

fn message_error(message: impl Into<String>) -> ProviderError {
    ProviderError {
        message: message.into(),
    }
}

async fn route_message(
    inner: &Arc<Mutex<ThreadInner>>,
    conn: &Arc<Conn>,
    client: &Arc<Client>,
    thread_id: &str,
    message: Message,
) {
    let reject = {
        let mut inner = inner.lock().unwrap();
        let same_conn = Arc::ptr_eq(&inner.conn, conn);
        if same_conn && message.id.is_none() {
            observe_turn_event(&mut inner, conn, thread_id, &message);
        }
        let mut reject = inner.closed || !same_conn || inner.active.is_none();
        if message.id.is_some() && !reject {
            reject = match events::parse_approval(message.params.as_deref()) {
                Some(params) => {
                    let active = inner.active.as_ref().expect("active turn is present");
                    !approval_method(message.method.as_deref().unwrap_or(""))
                        || params.turn_id.is_empty()
                        || (!active.id.is_empty() && params.turn_id != active.id)
                }
                None => true,
            };
        }
        if !reject && let Some(active) = inner.active.as_mut() {
            active.queue.enqueue(message.clone());
        }
        reject
    };
    if reject && let Some(id) = &message.id {
        let _ = client.respond_error(id, -32601, UNSUPPORTED_REQUEST).await;
    }
}

/// Learn a turn id from `turn/started`, or settle a stray turn's completion.
fn observe_turn_event(
    inner: &mut ThreadInner,
    conn: &Arc<Conn>,
    thread_id: &str,
    message: &Message,
) {
    let method = message.method.as_deref().unwrap_or("");
    if method != "turn/started" && method != "turn/completed" {
        return;
    }
    let Some(params) = events::parse_params(message.params.as_deref()) else {
        return;
    };
    let turn_id = params.turn.id;
    if turn_id.is_empty() {
        return;
    }
    if let Some(active) = inner.active.as_mut() {
        if method == "turn/started" {
            active.learn(turn_id.clone());
        }
        // The turn is over once its completion reaches the router, even if Run
        // gives up before it dequeues the message.
        if method == "turn/completed" && (active.id.is_empty() || active.id == turn_id) {
            active.completed = true;
        }
        return;
    }
    let mut settled = false;
    if let Some(stray) = inner.stray.as_mut()
        && stray.generation == conn.generation
    {
        if method == "turn/started" && stray.id.is_empty() {
            stray.id = turn_id.clone();
            stray.id_known.cancel();
            let client = conn.client.clone();
            let thread_id = thread_id.to_string();
            let spawned_turn = turn_id.clone();
            tokio::spawn(async move {
                interrupt_turn(&client, &thread_id, &spawned_turn, INTERRUPT_TIMEOUT).await;
            });
        }
        settled = method == "turn/completed" && (stray.id.is_empty() || stray.id == turn_id);
    }
    if settled && let Some(stray) = inner.stray.take() {
        stray.done.cancel();
    }
}

fn turn_notification(message: &Message, active_id: &str) -> Option<NotificationParams> {
    let params = events::parse_params(message.params.as_deref())?;
    let method = message.method.as_deref().unwrap_or("");
    let id = if method == "turn/started" || method == "turn/completed" {
        let id = params.turn.id.clone();
        if id.is_empty() {
            return None;
        }
        id
    } else {
        params.turn_id.clone()
    };
    (id.is_empty() || id == active_id).then_some(params)
}

fn approval_method(method: &str) -> bool {
    method == "item/commandExecution/requestApproval" || method == "item/fileChange/requestApproval"
}

async fn reject_request(
    conn: &Arc<Conn>,
    id: &serde_json::value::RawValue,
) -> Result<(), ClientError> {
    conn.client
        .respond_error(id, -32601, UNSUPPORTED_REQUEST)
        .await
}

/// Ask the approver only while the run is live, bounded by the child and thread
/// lifetimes. Go skips the callback when `ctx.Err() != nil` and answers
/// `decline`; a pending approval must not outlive a crash or `Close`.
async fn decide_approval(
    ctx: &RunContext,
    approver: &dyn Approver,
    request: ApprovalRequest,
    client_done: &CancellationToken,
    closed_signal: &CancellationToken,
) -> &'static str {
    if ctx_error(ctx).is_some() {
        return "decline";
    }
    let approve_ctx = ctx.child();
    let mut future = Box::pin(approver.approve(&approve_ctx, request));
    let mut decision = "decline";
    let cancelled = tokio::select! {
        approved = &mut future => {
            if approved && ctx_error(&approve_ctx).is_none() {
                decision = "accept";
            }
            false
        }
        _ = client_done.cancelled() => true,
        _ = closed_signal.cancelled() => true,
        _ = wait_ctx(ctx) => true,
    };
    if cancelled {
        approve_ctx.cancel.cancel();
        let _ = future.await;
    }
    decision
}

async fn interrupt_turn(client: &Arc<Client>, thread_id: &str, turn_id: &str, timeout: Duration) {
    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + timeout),
    };
    interrupt_turn_ctx(client, thread_id, turn_id, &ctx).await;
}

async fn interrupt_turn_ctx(
    client: &Arc<Client>,
    thread_id: &str,
    turn_id: &str,
    ctx: &RunContext,
) {
    let _ = client
        .call(
            ctx,
            "turn/interrupt",
            Some(json!({"threadId": thread_id, "turnId": turn_id})),
        )
        .await;
}

fn spawn_cancel_watcher(
    ctx: &RunContext,
    run_cancel: &CancellationToken,
    stop: &CancellationToken,
) -> JoinHandle<()> {
    let parent = ctx.clone();
    let run_cancel = run_cancel.clone();
    let stop = stop.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = wait_ctx(&parent) => {}
            _ = stop.cancelled() => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(CANCEL_INTERRUPT_BUDGET) => run_cancel.cancel(),
            _ = run_cancel.cancelled() => {}
            _ = stop.cancelled() => {}
        }
    })
}

fn with_timeout(ctx: &RunContext, budget: Duration) -> RunContext {
    let deadline = tokio::time::Instant::now() + budget;
    RunContext {
        cancel: ctx.cancel.child_token(),
        deadline: Some(
            ctx.deadline
                .map_or(deadline, |context_deadline| context_deadline.min(deadline)),
        ),
    }
}

fn settle_error(ctx: &RunContext) -> ProviderError {
    match ctx_error(ctx) {
        Some(error) if !error.is_deadline_exceeded() => provider_error(error),
        _ => message_error(STILL_RUNNING),
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// Extract a required `<object>.id` from a response, matching the client's
/// `decode <method> result` error prefix.
fn required_id(result: &Value, method: &str, object: &str) -> Result<String, String> {
    result
        .get(object)
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("decode {method} result: missing {object} id"))
}

fn thread_settings(opts: &ThreadOptions) -> Map<String, Value> {
    let policy = match opts.approval_policy {
        ApprovalPolicy::OnFailure => "on-request",
        other => approval_name(other),
    };
    let mut params = Map::new();
    params.insert("cwd".to_string(), json!(opts.cwd.to_string_lossy()));
    params.insert("model".to_string(), json!(opts.model));
    params.insert("sandbox".to_string(), json!(sandbox_name(opts.sandbox)));
    params.insert("approvalPolicy".to_string(), json!(policy));
    if let Some(base) = &opts.base_instructions
        && !base.is_empty()
    {
        params.insert("baseInstructions".to_string(), json!(base));
    }
    if let Some(developer) = &opts.developer_instructions
        && !developer.is_empty()
    {
        params.insert("developerInstructions".to_string(), json!(developer));
    }
    params
}

fn sandbox_name(sandbox: Sandbox) -> &'static str {
    match sandbox {
        Sandbox::ReadOnly => "read-only",
        Sandbox::WorkspaceWrite => "workspace-write",
        Sandbox::DangerFullAccess => "danger-full-access",
    }
}

fn approval_name(policy: ApprovalPolicy) -> &'static str {
    match policy {
        ApprovalPolicy::Untrusted => "untrusted",
        ApprovalPolicy::OnRequest => "on-request",
        ApprovalPolicy::OnFailure => "on-failure",
        ApprovalPolicy::Never => "never",
    }
}

/// An approver that denies every request (the interrupt drain path).
struct Deny;

#[async_trait::async_trait]
impl Approver for Deny {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CountingApprover(Arc<AtomicBool>);

    #[async_trait::async_trait]
    impl Approver for CountingApprover {
        async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
            self.0.store(true, Ordering::SeqCst);
            true
        }
    }

    #[tokio::test]
    async fn cancelled_context_skips_approver() {
        let ctx = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        ctx.cancel.cancel();
        let called = Arc::new(AtomicBool::new(false));
        let decision = decide_approval(
            &ctx,
            &CountingApprover(called.clone()),
            ApprovalRequest {
                thread_id: "thread-1".to_string(),
                tool: "shell".to_string(),
                command: Some("ls".to_string()),
                path: None,
                reason: String::new(),
            },
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(decision, "decline");
        assert!(!called.load(Ordering::SeqCst), "approver was invoked");
    }
}
