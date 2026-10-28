//! REST + WebSocket front door.
//!
//! The MCP server is for agents that can speak stdio; this is for everything
//! else. Both sit on the same engine and the same store — a task started over
//! HTTP shows up in `bhx task ls`, and one started from the CLI can be followed
//! over the event stream.
//!
//! The event stream is a *tail of the store*, not an in-process broadcast:
//! a client that reconnects with `?after=N` gets every event it missed, and a
//! restarted server does not have to remember who was watching.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bh_core::{Action, TaskSpec, host_allowed};
use bh_task::{Engine, RunOptions, TaskRow};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::page::Page;

/// A page driven through REST. Sessions have their own browser, so two
/// sessions can navigate independently, and closing one leaves the other alone.
struct SessionEntry {
    page: Mutex<Page>,
    /// Empty means "no restriction", matching `Policy.allowed_hosts`.
    allowed_hosts: Vec<String>,
}

type Sessions = Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>;

#[derive(Clone)]
struct App {
    home: PathBuf,
    /// Tasks this process is driving, used to cancel a controller promptly.
    /// Workflow steps also notice `state = canceled` in the store, but the
    /// controller only checks its flag between ticks.
    runs: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    sessions: Sessions,
}

impl App {
    fn engine(&self) -> Result<Engine, Response> {
        Engine::open(&self.home).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    }

    async fn session(&self, id: &str) -> Result<Arc<SessionEntry>, Response> {
        match self.sessions.lock().await.get(id).cloned() {
            Some(s) => Ok(s),
            None => Err(err(StatusCode::NOT_FOUND, format!("no session {id}"))),
        }
    }
}

pub async fn serve_http(addr: &str, home: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(&home)?;
    let app = App { home, runs: Arc::default(), sessions: Arc::default() };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "REST/WS listening");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

fn router(app: App) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/v1/sessions/{id}",
            get(session_info).delete(close_session),
        )
        .route("/v1/sessions/{id}/navigate", post(session_navigate))
        .route("/v1/sessions/{id}/observe", post(session_observe))
        .route("/v1/sessions/{id}/actions", post(session_actions))
        .route("/v1/sessions/{id}/condition", post(session_condition))
        .route("/v1/sessions/{id}/verify", post(session_verify))
        .route("/v1/sessions/{id}/evaluate", post(session_evaluate))
        .route("/v1/sessions/{id}/screenshot", get(session_screenshot))
        .route("/v1/tasks", get(list_tasks).post(start_task))
        .route("/v1/tasks/{id}", get(task_status))
        .route("/v1/tasks/{id}/resume", post(resume_task))
        .route("/v1/tasks/{id}/approve", post(approve_task))
        .route("/v1/tasks/{id}/reject", post(reject_task))
        .route("/v1/tasks/{id}/cancel", post(cancel_task))
        .route("/v1/tasks/{id}/events", get(events))
        .route("/v1/tasks/{id}/events/ws", get(events_ws))
        .route("/v1/tasks/{id}/artifacts", get(list_artifacts))
        .route("/v1/tasks/{id}/artifacts/{name}", get(download_artifact))
        .with_state(app)
}

// ------------------------------------------------------------------ health

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "name": "bhx",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": "0.1",
        "sessions": true,
        "tasks": true,
    }))
}

// ---------------------------------------------------------------- sessions

/// The REST mirror of MCP's `browser_*` tools. HTTP is stateless, so the page
/// has to be named: every call takes an id from `POST /v1/sessions`.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct NewSession {
    headless: bool,
    download_dir: Option<String>,
    /// Empty means no restriction. When set, both `navigate` and any
    /// `navigate` action inside a batch are checked before they run.
    allowed_hosts: Vec<String>,
}

impl Default for NewSession {
    fn default() -> Self {
        Self { headless: true, download_dir: None, allowed_hosts: Vec::new() }
    }
}

async fn create_session(
    State(app): State<App>,
    body: Option<Json<NewSession>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let id = ulid::Ulid::new().to_string();
    // Downloads land in a per-session directory, never in the server's cwd.
    let dir = req
        .download_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| app.home.join("sessions").join(&id).join("downloads"));
    match Page::open(dir, req.headless).await {
        Ok(page) => {
            let url = page.session().url().await.unwrap_or_default();
            let entry = Arc::new(SessionEntry {
                page: Mutex::new(page),
                allowed_hosts: req.allowed_hosts.clone(),
            });
            app.sessions.lock().await.insert(id.clone(), entry);
            (
                StatusCode::CREATED,
                Json(json!({
                    "session_id": id,
                    "url": url,
                    "allowed_hosts": req.allowed_hosts,
                })),
            )
                .into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("open browser: {e}"),
        ),
    }
}

async fn list_sessions(State(app): State<App>) -> Response {
    let entries: Vec<Value> = app
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, e)| json!({ "session_id": id, "allowed_hosts": e.allowed_hosts }))
        .collect();
    Json(Value::Array(entries)).into_response()
}

async fn session_info(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let page = entry.page.lock().await;
    Json(json!({
        "session_id": id,
        "url": page.session().url().await.unwrap_or_default(),
        "title": page.session().title().await.unwrap_or_default(),
        "allowed_hosts": entry.allowed_hosts,
    }))
    .into_response()
}

async fn close_session(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    let entry = app.sessions.lock().await.remove(&id);
    match entry {
        Some(e) => {
            let mut page = e.page.lock().await;
            page.shutdown();
            Json(json!({ "session_id": id, "closed": true })).into_response()
        }
        None => err(StatusCode::NOT_FOUND, format!("no session {id}")),
    }
}

fn host_refusal(url: &str, allowed: &[String]) -> Response {
    err(
        StatusCode::FORBIDDEN,
        format!("host not in allowed_hosts: {url} (allowed: {})", allowed.join(", ")),
    )
}

#[derive(Debug, Deserialize)]
struct NavigateBody {
    url: String,
}

async fn session_navigate(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<NavigateBody>,
) -> Response {
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    if !host_allowed(&body.url, &entry.allowed_hosts) {
        return host_refusal(&body.url, &entry.allowed_hosts);
    }
    let page = entry.page.lock().await;
    match page.navigate(&body.url).await {
        Ok((url, title)) => Json(json!({ "url": url, "title": title })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ObserveBody {
    goal: String,
    max_elements: Option<usize>,
    screenshot: bool,
}

impl Default for ObserveBody {
    fn default() -> Self {
        Self { goal: String::new(), max_elements: None, screenshot: false }
    }
}

async fn session_observe(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<ObserveBody>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let mut page = entry.page.lock().await;
    match page.observe(&req.goal, req.max_elements, req.screenshot).await {
        Ok(obs) => {
            let shot = obs
                .screenshot
                .map(|b| base64::engine::general_purpose::STANDARD.encode(b));
            Json(json!({
                "text": obs.text,
                "truncated": obs.truncated,
                "elements": obs.elements,
                "screenshot": shot,
                // The observation screenshot is JPEG (token-sized); the
                // dedicated endpoint serves PNG.
                "screenshot_mime": "image/jpeg",
                "screenshot_error": obs.screenshot_error,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ActionsBody {
    actions: Vec<Action>,
    observe_after: bool,
}

impl Default for ActionsBody {
    fn default() -> Self {
        Self { actions: Vec::new(), observe_after: false }
    }
}

/// 422 here means "the batch stopped at the action named in the log", not
/// "nothing happened": actions before the failure already ran.
async fn session_actions(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ActionsBody>,
) -> Response {
    if body.actions.is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "no actions");
    }
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    // A batch can navigate; the allow-list must cover that path too, and it is
    // checked before the first action so a denied navigation cannot arrive
    // halfway through a click sequence.
    if let Some(url) = body.actions.iter().find_map(|a| match a {
        Action::Navigate { url } => Some(url),
        _ => None,
    }) && !host_allowed(url, &entry.allowed_hosts)
    {
        return host_refusal(url, &entry.allowed_hosts);
    }
    let mut page = entry.page.lock().await;
    match page.act(&body.actions, body.observe_after, Arc::new(AtomicBool::new(false))).await {
        Ok(acted) if acted.failed => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "ok": false, "log": acted.log })),
        )
            .into_response(),
        Ok(acted) => Json(json!({ "ok": true, "log": acted.log })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct ConditionBody {
    condition: bh_core::Condition,
    #[serde(default)]
    timeout_ms: u64,
}

async fn session_condition(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ConditionBody>,
) -> Response {
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let mut page = entry.page.lock().await;
    match page
        .check(&body.condition, body.timeout_ms, Arc::new(AtomicBool::new(false)))
        .await
    {
        Ok(holds) => Json(json!({ "holds": holds })).into_response(),
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct VerifyBody {
    goal: String,
    threshold: Option<f64>,
}

impl Default for VerifyBody {
    fn default() -> Self {
        Self { goal: String::new(), threshold: None }
    }
}

async fn session_verify(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<VerifyBody>,
) -> Response {
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let mut page = entry.page.lock().await;
    let threshold = body.threshold.unwrap_or(0.6);
    match page.verify(&body.goal, threshold).await {
        Ok(report) => Json(json!({
            "verdict": match report.verdict {
                bh_model::Verdict::Achieved => "achieved",
                bh_model::Verdict::NotAchieved => "not_achieved",
                bh_model::Verdict::Uncertain => "uncertain",
            },
            "confidence": report.confidence,
            "threshold": report.threshold,
            "model": report.model,
            "reason": report.reason,
        }))
        .into_response(),
        // A missing adapter is a server-side capability problem, not a bad
        // request — the caller cannot fix it by changing the body.
        Err(e) if e.to_string().starts_with("no model adapter") => {
            err(StatusCode::SERVICE_UNAVAILABLE, e.to_string())
        }
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct EvaluateBody {
    script: String,
    /// `isolated` by default: caller-supplied JS gets the DOM, not the page's
    /// globals. `main` is opt-in, like the MCP tool.
    world: Option<String>,
}

impl Default for EvaluateBody {
    fn default() -> Self {
        Self { script: String::new(), world: None }
    }
}

async fn session_evaluate(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<EvaluateBody>,
) -> Response {
    if body.script.trim().is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "missing script");
    }
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let page = entry.page.lock().await;
    let world = body.world.as_deref().unwrap_or("isolated");
    match page.evaluate(&body.script, world).await {
        Ok(v) => Json(json!({ "result": v })).into_response(),
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    }
}

async fn session_screenshot(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let entry = match app.session(&id).await {
        Ok(e) => e,
        Err(r) => return r,
    };
    let page = entry.page.lock().await;
    match page.screenshot_png().await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/png")],
            bytes,
        )
            .into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

// ------------------------------------------------------------------- tasks

async fn list_tasks(State(app): State<App>, Query(q): Query<ListQuery>) -> Response {
    let engine = match app.engine() {
        Ok(e) => e,
        Err(r) => return r,
    };
    match engine.store().tasks(q.limit.unwrap_or(50).min(500)) {
        Ok(rows) => Json(Value::Array(rows.iter().map(task_row_json).collect())).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn start_task(State(app): State<App>, Json(mut spec): Json<TaskSpec>) -> Response {
    if let Err(msg) = check_spec(&spec) {
        return err(StatusCode::UNPROCESSABLE_ENTITY, msg);
    }
    let id = spec
        .id
        .clone()
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    spec.id = Some(id.clone());

    // A caller-supplied id is an idempotency claim. Refuse a collision instead
    // of letting the engine's UNIQUE constraint surface as a failed task.
    if let Ok(engine) = app.engine()
        && engine.store().task(&id).is_ok()
    {
        return err(StatusCode::CONFLICT, format!("task {id} already exists"));
    }

    let cancel = Arc::new(AtomicBool::new(false));
    app.runs.lock().await.insert(id.clone(), cancel.clone());
    let home = app.home.clone();
    let runs = app.runs.clone();
    let spawned_id = id.clone();
    tokio::spawn(async move {
        let outcome = async {
            let mut engine = Engine::open(&home)?;
            engine
                .run(
                    spec,
                    RunOptions {
                        headless: true,
                        cancel: Some(cancel),
                        controller_updates: None,
                    },
                )
                .await
        }
        .await;
        if let Err(e) = outcome {
            tracing::error!(task = %spawned_id, error = %e, "task run failed");
            // The engine marks failures it can attribute to a step; this is the
            // path where it never got that far. Only touch a task that is still
            // running — a duplicate id or a finished task must not be rewritten.
            if let Ok(engine) = Engine::open(&home)
                && engine
                    .store()
                    .task(&spawned_id)
                    .is_ok_and(|t| t.state == "running")
            {
                let _ = engine.store().set_task_state(&spawned_id, "failed");
            }
        }
        runs.lock().await.remove(&spawned_id);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "task_id": id, "state": "running" })),
    )
        .into_response()
}

async fn resume_task(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    let cancel = Arc::new(AtomicBool::new(false));
    app.runs.lock().await.insert(id.clone(), cancel.clone());
    let home = app.home.clone();
    let runs = app.runs.clone();
    let spawned_id = id.clone();
    tokio::spawn(async move {
        let outcome = async {
            let mut engine = Engine::open(&home)?;
            engine
                .resume(
                    &id,
                    RunOptions {
                        headless: true,
                        cancel: Some(cancel),
                        controller_updates: None,
                    },
                )
                .await
        }
        .await;
        if let Err(e) = outcome {
            tracing::error!(task = %spawned_id, error = %e, "task resume failed");
        }
        runs.lock().await.remove(&spawned_id);
    });
    StatusCode::ACCEPTED.into_response()
}

async fn approve_task(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    match app.engine().and_then(|e| {
        e.approve(&id)
            .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))
    }) {
        Ok(n) => Json(json!({ "task_id": id, "approved": n, "state": "paused" })).into_response(),
        Err(r) => r,
    }
}

async fn reject_task(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    match app.engine().and_then(|e| {
        e.reject(&id, "rejected over REST")
            .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))
    }) {
        Ok(n) => Json(json!({ "task_id": id, "rejected": n, "state": "failed" })).into_response(),
        Err(r) => r,
    }
}

async fn cancel_task(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    if let Some(flag) = app.runs.lock().await.get(&id) {
        flag.store(true, Ordering::SeqCst);
    }
    match app.engine().and_then(|e| {
        e.cancel(&id)
            .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))
    }) {
        Ok(()) => Json(json!({ "task_id": id, "state": "canceled" })).into_response(),
        Err(r) => r,
    }
}

async fn task_status(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    let engine = match app.engine() {
        Ok(e) => e,
        Err(r) => return r,
    };
    match status_json(&engine, &id) {
        Ok(v) => Json(v).into_response(),
        Err(StatusCode::NOT_FOUND) => err(StatusCode::NOT_FOUND, format!("no task {id}")),
        Err(s) => err(s, "could not read task"),
    }
}

fn status_json(engine: &Engine, id: &str) -> Result<Value, StatusCode> {
    let task = engine.store().task(id).map_err(|_| StatusCode::NOT_FOUND)?;
    let steps = engine.store().steps(id).unwrap_or_default();
    let approvals = engine.store().approvals(id).unwrap_or_default();
    let artifacts = engine.store().artifacts(id).unwrap_or_default();
    Ok(json!({
        "task": task_row_json(&task),
        "steps": steps.iter().map(|s| json!({
            "step_id": s.step_id,
            "idx": s.idx,
            "goal": s.goal,
            "state": s.state,
            "attempts": s.attempts,
            "verified": s.verified,
            "error": s.error,
            "started_at": s.started_at,
            "finished_at": s.finished_at,
        })).collect::<Vec<_>>(),
        "approvals": approvals.iter().map(|a| json!({
            "id": a.id,
            "step_id": a.step_id,
            "reason": a.reason,
            "payload": a.payload,
            "decision": a.decision,
        })).collect::<Vec<_>>(),
        "artifacts": artifacts.iter().map(|a| json!({
            "id": a.id,
            "kind": a.kind,
            "name": a.name,
            "bytes": a.bytes,
            "sha256": a.sha256,
            "url": format!("/v1/tasks/{id}/artifacts/{}", url_component(a.name.as_deref().unwrap_or(&a.id))),
        })).collect::<Vec<_>>(),
    }))
}

fn task_row_json(t: &TaskRow) -> Value {
    let result = t
        .result_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let error = t
        .error_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    json!({
        "id": t.id,
        "kind": t.kind,
        "goal": t.goal,
        "mode": t.mode,
        "state": t.state,
        "model_calls": t.model_calls,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
        "finished_at": t.finished_at,
        "result": result,
        "error": error,
    })
}

// ------------------------------------------------------------------ events

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct AfterQuery {
    #[serde(default)]
    after: i64,
}

async fn events(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<AfterQuery>,
) -> Response {
    let engine = match app.engine() {
        Ok(e) => e,
        Err(r) => return r,
    };
    match engine.store().events(&id, q.after.max(0)) {
        Ok(rows) => Json(Value::Array(rows.iter().map(event_json).collect())).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e.to_string()),
    }
}

fn event_json(e: &bh_task::EventRow) -> Value {
    json!({ "seq": e.seq, "ts": e.ts, "type": e.kind, "data": e.data })
}

async fn events_ws(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<AfterQuery>,
) -> Response {
    ws.on_upgrade(move |socket| stream_events(socket, app, id, q.after.max(0)))
}

/// Tail the store until the task is terminal and its final event has been
/// delivered. Polling is deliberate: the store is the source of truth, and a
/// reconnect with `after` behaves exactly like a fresh connection.
async fn stream_events(mut socket: WebSocket, app: App, id: String, mut since: i64) {
    let engine = match Engine::open(&app.home) {
        Ok(e) => e,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    json!({ "type": "stream.error", "message": e.to_string() })
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let mut quiet_ticks = 0;
    loop {
        let rows = match engine.store().events(&id, since) {
            Ok(r) => r,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(
                        json!({ "type": "stream.error", "message": e.to_string() })
                            .to_string()
                            .into(),
                    ))
                    .await;
                return;
            }
        };
        let mut saw_finished = false;
        for row in &rows {
            since = row.seq;
            if row.kind == "task.finished" {
                saw_finished = true;
            }
            if socket
                .send(Message::Text(event_json(row).to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }

        let terminal = engine
            .store()
            .task(&id)
            .map(|t| matches!(t.state.as_str(), "succeeded" | "failed" | "canceled"))
            .unwrap_or(false);
        if saw_finished || (terminal && quiet_ticks >= 2) {
            let _ = socket
                .send(Message::Text(
                    json!({ "type": "stream.closed", "task_id": id, "seq": since })
                        .to_string()
                        .into(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
        quiet_ticks = if rows.is_empty() {
            quiet_ticks + 1
        } else {
            0
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// --------------------------------------------------------------- artifacts

async fn list_artifacts(State(app): State<App>, UrlPath(id): UrlPath<String>) -> Response {
    let engine = match app.engine() {
        Ok(e) => e,
        Err(r) => return r,
    };
    match engine.store().artifacts(&id) {
        Ok(rows) => Json(Value::Array(
            rows.iter()
                .map(|a| {
                    json!({
                        "id": a.id,
                        "kind": a.kind,
                        "name": a.name,
                        "bytes": a.bytes,
                        "sha256": a.sha256,
                        "url": format!("/v1/tasks/{id}/artifacts/{}", url_component(a.name.as_deref().unwrap_or(&a.id))),
                    })
                })
                .collect(),
        ))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn download_artifact(
    State(app): State<App>,
    UrlPath((id, name)): UrlPath<(String, String)>,
) -> Response {
    let engine = match app.engine() {
        Ok(e) => e,
        Err(r) => return r,
    };
    let rows = match engine.store().artifacts(&id) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::NOT_FOUND, e.to_string()),
    };
    let Some(row) = rows
        .iter()
        .find(|a| a.name.as_deref() == Some(name.as_str()) || a.id == name)
    else {
        return err(StatusCode::NOT_FOUND, format!("no artifact {name}"));
    };

    // A stored path is data, not an instruction: only serve files that still
    // live under this task's artifacts directory.
    let base = app.home.join("tasks").join(&id).join("artifacts");
    let (Ok(base), Ok(path)) = (base.canonicalize(), Path::new(&row.path).canonicalize()) else {
        return err(StatusCode::NOT_FOUND, "artifact file is gone");
    };
    if !path.starts_with(&base) {
        return err(StatusCode::FORBIDDEN, "artifact path is outside the task");
    }
    match std::fs::read(&path) {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type(&path))],
            bytes,
        )
            .into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e.to_string()),
    }
}

/// Percent-encode one path segment. Download names come from the site and may
/// contain spaces or CJK; the URL has to be ASCII even when the name is not.
fn url_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => "text/csv; charset=utf-8",
        "json" => "application/json",
        "txt" | "log" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

// ------------------------------------------------------------------- utils

fn check_spec(spec: &TaskSpec) -> Result<(), String> {
    if spec.protocol_version != "0.1" {
        return Err(format!(
            "unsupported protocol_version {:?} (expected \"0.1\")",
            spec.protocol_version
        ));
    }
    match spec.kind {
        bh_core::TaskKind::Workflow if spec.steps.is_empty() => {
            if spec.mode != "auto" {
                Err("workflow task has no steps (set mode=auto to let the engine plan)".into())
            } else if spec.goal.trim().is_empty() {
                Err("auto planning needs a non-empty goal".into())
            } else {
                Ok(())
            }
        }
        bh_core::TaskKind::Interactive => match &spec.controller {
            None => Err("interactive task has no controller".into()),
            Some(c) if c.start_url.is_none() => {
                Err("interactive task needs controller.start_url".into())
            }
            Some(c) if c.observe.script.trim().is_empty() => {
                Err("interactive task needs controller.observe.script".into())
            }
            Some(_) => Ok(()),
        },
        _ if spec.policy.max_wall_ms == 0 => Err("policy.max_wall_ms must be > 0".into()),
        _ => Ok(()),
    }
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TaskSpec {
        TaskSpec::default()
    }

    #[test]
    fn empty_workflow_is_allowed_only_for_auto_planning_with_a_goal() {
        let mut s = spec();
        s.mode = "auto".into();
        s.goal = "do something".into();
        assert!(check_spec(&s).is_ok(), "mode=auto + goal should be accepted");

        s.mode = "scripted".into();
        assert!(check_spec(&s).is_err(), "a non-auto empty workflow must be rejected");

        s.mode = "auto".into();
        s.goal = "   ".into();
        assert!(check_spec(&s).is_err(), "auto planning needs a goal");
    }

    #[test]
    fn interactive_needs_a_start_url_and_script() {
        let mut s = spec();
        s.kind = bh_core::TaskKind::Interactive;
        assert!(check_spec(&s).is_err());
        s.controller = Some(bh_core::ControllerPolicy::default());
        assert!(check_spec(&s).is_err());
        s.controller.as_mut().unwrap().start_url = Some("http://127.0.0.1/".into());
        assert!(check_spec(&s).is_err());
        s.controller.as_mut().unwrap().observe.script = "watch".into();
        assert!(check_spec(&s).is_ok());
    }

    #[test]
    fn artifact_content_types_are_explicit() {
        assert_eq!(content_type(Path::new("a.csv")), "text/csv; charset=utf-8");
        assert_eq!(content_type(Path::new("a.PNG")), "image/png");
        assert_eq!(content_type(Path::new("a.bin")), "application/octet-stream");
    }
}
