//! MCP (Model Context Protocol) server over stdio.
//!
//! Two levels of tool, deliberately:
//!
//! * `browser_*` drive the current page — observe, dispatch, verify. Use these
//!   when the agent is still deciding what to do.
//! * `task_*` hand the whole job to the task engine: persisted state,
//!   checkpoints, approvals, artifacts. Use these when the work is known and
//!   has to survive the process.
//!
//! Tool results are text the model can read plus optional images; failures come
//! back as `isError: true` rather than JSON-RPC errors, so the agent can
//! recover instead of crashing.

pub mod http;
pub mod page;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;
use bh_core::{Action, Condition, ControllerPolicy, TaskKind, TaskSpec};
use bh_exec::{Runner, TaskReport};
use bh_task::{Engine as TaskEngine, RunOptions};
use page::Page;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{Mutex, watch};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "bhx";

// ------------------------------------------------------------------ protocol

struct Rpc {
    method: String,
    params: Value,
}

/// What a tool hands back to the model.
struct ToolOutput {
    text: String,
    images: Vec<String>,
    is_error: bool,
}

impl ToolOutput {
    fn text(s: impl Into<String>) -> Self {
        Self { text: s.into(), images: Vec::new(), is_error: false }
    }

    fn error(s: impl Into<String>) -> Self {
        Self { text: s.into(), images: Vec::new(), is_error: true }
    }

    fn to_content(&self) -> Vec<Value> {
        let mut content = vec![json!({ "type": "text", "text": self.text })];
        for data in &self.images {
            content.push(json!({ "type": "image", "data": data, "mimeType": "image/jpeg" }));
        }
        content
    }
}

type State = Arc<Mutex<Option<Page>>>;
type EngineState = Arc<Mutex<Option<TaskEngine>>>;

/// A background interactive task. The policy channel is how an update reaches
/// a loop that is already running; `cancel` is how it stops without waiting
/// for the current policy to finish.
struct InteractiveJob {
    cancel: Arc<AtomicBool>,
    policy: watch::Sender<ControllerPolicy>,
    error: Arc<StdMutex<Option<String>>>,
    done: Arc<AtomicBool>,
}

impl InteractiveJob {
    fn running(&self) -> bool {
        !self.done.load(Ordering::Relaxed)
    }
}

/// Jobs started by this server process. Deliberately in memory: an interactive
/// task cannot be resumed, so a server restart loses nothing it could continue.
type Jobs = Arc<Mutex<HashMap<String, Arc<InteractiveJob>>>>;

pub async fn serve_stdio(default_artifacts: PathBuf, home: PathBuf) -> anyhow::Result<()> {
    let state: State = Arc::new(Mutex::new(None));
    let engine: EngineState = Arc::new(Mutex::new(match TaskEngine::open(&home) {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!(error = %e, home = %home.display(), "task store unavailable; task_* tools will fail");
            None
        }
    }));
    let jobs: Jobs = Arc::default();
    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let out = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                // A malformed line cannot be answered (no id), but silence
                // would look like a hang; say so on stderr.
                tracing::warn!(error = %e, "ignoring unparseable line");
                continue;
            }
        };

        let method = msg["method"].as_str().unwrap_or("").to_string();
        let params = msg["params"].clone();

        if msg.get("id").is_none() {
            if method == "notifications/cancelled" {
                let key = request_key(&params["requestId"]);
                if let Some(flag) = cancels.lock().await.get(&key) {
                    flag.store(true, Ordering::Relaxed);
                    tracing::info!(%key, "cancel requested");
                }
            }
            continue;
        }

        let id = msg["id"].clone();
        let key = request_key(&id);
        let cancel = Arc::new(AtomicBool::new(false));
        cancels.lock().await.insert(key.clone(), cancel.clone());

        let state = state.clone();
        let engine = engine.clone();
        let jobs = jobs.clone();
        let home = home.clone();
        let out = out.clone();
        let cancels = cancels.clone();
        let artifacts = default_artifacts.clone();
        let rpc = Rpc { method, params };

        tokio::spawn(async move {
            let response =
                match dispatch(&state, &engine, &jobs, &home, &artifacts, &rpc, cancel).await {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32603, "message": e }
                }),
            };
            let mut w = out.lock().await;
            let _ = w.write_all(format!("{response}\n").as_bytes()).await;
            let _ = w.flush().await;
            cancels.lock().await.remove(&key);
        });
    }

    // stdin closed: drop the browser so no orphan Chrome is left behind.
    if let Some(mut page) = state.lock().await.take() {
        page.shutdown();
    }
    Ok(())
}

fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

async fn dispatch(
    state: &State,
    engine: &EngineState,
    jobs: &Jobs,
    home: &Path,
    artifacts: &PathBuf,
    rpc: &Rpc,
    cancel: Arc<AtomicBool>,
) -> std::result::Result<Value, String> {
    match rpc.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = rpc.params["name"].as_str().unwrap_or_default().to_string();
            let args = rpc.params["arguments"].clone();
            let output = call_tool(state, engine, jobs, home, artifacts, &name, args, cancel).await;
            Ok(json!({ "content": output.to_content(), "isError": output.is_error }))
        }
        other => Err(format!("method not found: {other}")),
    }
}

async fn call_tool(
    state: &State,
    engine: &EngineState,
    jobs: &Jobs,
    home: &Path,
    artifacts: &PathBuf,
    name: &str,
    args: Value,
    cancel: Arc<AtomicBool>,
) -> ToolOutput {
    // Task tools own their browser and their store; they must not drag the
    // interactive session open just to answer a question about a task.
    if name.starts_with("task_") || name.starts_with("artifact_") {
        return task_tool(engine, name, args, cancel).await;
    }
    // Interactive jobs run in the background, own their browser, and are
    // addressed by task id. `browser_close` must not touch them.
    if name.starts_with("interactive_") {
        return interactive_tool(engine, jobs, home, name, args).await;
    }

    if name == "browser_close" {
        let mut guard = state.lock().await;
        if let Some(mut page) = guard.take() {
            page.shutdown();
        }
        return ToolOutput::text("session closed");
    }

    let mut guard = state.lock().await;

    // An explicit create_session replaces whatever is running.
    if name == "browser_create_session" {
        *guard = None;
        let headless = args["headless"].as_bool().unwrap_or(true);
        let dir = args["download_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| artifacts.clone());
        return match Page::open(dir, headless).await {
            Ok(page) => {
                *guard = Some(page);
                ToolOutput::text("session ready")
            }
            Err(e) => ToolOutput::error(format!("open browser: {e}")),
        };
    }

    if guard.is_none() {
        let headless = args["headless"].as_bool().unwrap_or(true);
        match Page::open(artifacts.clone(), headless).await {
            Ok(page) => *guard = Some(page),
            Err(e) => return ToolOutput::error(format!("open browser: {e}")),
        }
    }
    let Some(page) = guard.as_mut() else {
        return ToolOutput::error("no session");
    };

    match name {
        "browser_navigate" => {
            let Some(url) = args["url"].as_str() else {
                return ToolOutput::error("missing url");
            };
            match page.navigate(url).await {
                Ok((url, title)) => ToolOutput::text(format!("{url}\n{title}")),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "browser_observe" => {
            let goal = args["goal"].as_str().unwrap_or("");
            let max = args["max_elements"].as_u64().map(|m| m as usize);
            let want_shot = args["screenshot"].as_bool().unwrap_or(false);
            match page.observe(goal, max, want_shot).await {
                Ok(obs) => {
                    let mut out = ToolOutput::text(obs.text);
                    // A failed screenshot does not invalidate the element index.
                    if let Some(e) = obs.screenshot_error {
                        out.text.push_str(&format!("(screenshot failed: {e})\n"));
                    }
                    if let Some(bytes) = obs.screenshot {
                        out.images
                            .push(base64::engine::general_purpose::STANDARD.encode(bytes));
                    }
                    out
                }
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "browser_execute_actions" => {
            let actions: Vec<Action> = match serde_json::from_value(args["actions"].clone()) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("bad actions: {e}")),
            };
            if actions.is_empty() {
                return ToolOutput::error("no actions");
            }
            let observe_after = args["observe_after"].as_bool().unwrap_or(false);
            match page.act(&actions, observe_after, cancel).await {
                Ok(acted) => ToolOutput {
                    text: acted.log,
                    images: vec![],
                    is_error: acted.failed,
                },
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "browser_check_condition" => {
            let cond: Condition = match serde_json::from_value(args["condition"].clone()) {
                Ok(c) => c,
                Err(e) => return ToolOutput::error(format!("bad condition: {e}")),
            };
            let timeout = args["timeout_ms"].as_u64().unwrap_or(0);
            match page.check(&cond, timeout, cancel).await {
                Ok(v) => ToolOutput::text(if v { "true" } else { "false" }),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "browser_verify" => {
            let Some(goal) = args["goal"].as_str() else {
                return ToolOutput::error("missing goal");
            };
            let threshold = args["threshold"].as_f64().unwrap_or(0.6);
            match page.verify(goal, threshold).await {
                Ok(report) => {
                    let verdict = match report.verdict {
                        bh_model::Verdict::Achieved => "achieved",
                        bh_model::Verdict::NotAchieved => "not_achieved",
                        bh_model::Verdict::Uncertain => "uncertain",
                    };
                    ToolOutput::text(format!(
                        "{verdict}\nconfidence: {:.2} (threshold {threshold:.2})\nmodel: {}\nreason: {}",
                        report.confidence,
                        report.model,
                        report.reason.unwrap_or_default()
                    ))
                }
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "browser_evaluate" => {
            let Some(script) = args["script"].as_str() else {
                return ToolOutput::error("missing script");
            };
            let world = args["world"].as_str().unwrap_or("isolated");
            match page.evaluate(script, world).await {
                Ok(v) => ToolOutput::text(
                    serde_json::to_string(&v).unwrap_or_else(|_| "null".into()),
                ),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "browser_screenshot" => match page.screenshot().await {
            Ok(bytes) => ToolOutput {
                text: format!("{} bytes", bytes.len()),
                images: vec![base64::engine::general_purpose::STANDARD.encode(bytes)],
                is_error: false,
            },
            Err(e) => ToolOutput::error(e.to_string()),
        },
        "browser_run_task" => {
            let spec: TaskSpec = match serde_json::from_value(args["task"].clone()) {
                Ok(s) => s,
                Err(e) => return ToolOutput::error(format!("bad task: {e}")),
            };
            let (browser, session, observer) = page.parts_mut();
            let report = Runner::new(browser, session, observer)
                .with_cancel(cancel)
                .run(&spec)
                .await;
            ToolOutput::text(render_report(&report))
        }
        other => ToolOutput::error(format!("unknown tool: {other}")),
    }
}

/// Task-level tools. One lock: a task run owns the store while it runs, so a
/// concurrent status call waits rather than reading a half-written row.
async fn task_tool(
    state: &EngineState,
    name: &str,
    args: Value,
    cancel: Arc<AtomicBool>,
) -> ToolOutput {
    let mut guard = state.lock().await;
    let Some(engine) = guard.as_mut() else {
        return ToolOutput::error("task store unavailable (see server stderr)");
    };
    let id = args["task_id"].as_str().unwrap_or("").to_string();
    let headless = args["headless"].as_bool().unwrap_or(true);

    match name {
        "task_run" => {
            let spec: TaskSpec = match serde_json::from_value(args["task"].clone()) {
                Ok(s) => s,
                Err(e) => return ToolOutput::error(format!("bad task: {e}")),
            };
            let opts = RunOptions {
                headless,
                cancel: Some(cancel),
                controller_updates: None,
            };
            match engine.run(spec, opts).await {
                Ok(o) => ToolOutput::text(format!(
                    "task_id: {}\nstate: {}\n{}",
                    o.task_id,
                    o.state,
                    o.report.map(|v| v.to_string()).unwrap_or_default()
                )),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "task_resume" => {
            let opts = RunOptions {
                headless,
                cancel: Some(cancel),
                controller_updates: None,
            };
            match engine.resume(&id, opts).await {
                Ok(o) => ToolOutput::text(format!("task_id: {}\nstate: {}", o.task_id, o.state)),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "task_status" => match render_task(engine, &id) {
            Ok(text) => ToolOutput::text(text),
            Err(e) => ToolOutput::error(e),
        },
        "task_artifacts" => match engine.store().artifacts(&id) {
            Ok(rows) if rows.is_empty() => ToolOutput::text("(no artifacts)"),
            Ok(rows) => ToolOutput::text(
                rows
                    .iter()
                    .map(|a| {
                        format!(
                            "{}\t{} bytes\tsha256={}\t{}",
                            a.name.as_deref().unwrap_or(&a.id),
                            a.bytes,
                            a.sha256.as_deref().unwrap_or("-"),
                            a.path
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(e) => ToolOutput::error(e.to_string()),
        },
        "artifact_list" => match engine.store().artifacts(&id) {
            Ok(rows) if rows.is_empty() => ToolOutput::text("(no artifacts)"),
            Ok(rows) => ToolOutput::text(
                rows
                    .iter()
                    .map(|a| {
                        format!(
                            "id={}\tname={}\tkind={}\t{} bytes\tsha256={}",
                            a.id,
                            a.name.as_deref().unwrap_or("-"),
                            a.kind,
                            a.bytes,
                            a.sha256.as_deref().unwrap_or("-")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(e) => ToolOutput::error(e.to_string()),
        },
        "artifact_metadata" => {
            let key = args["name"].as_str().unwrap_or("");
            let rows = match engine.store().artifacts(&id) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            match artifact_row(&rows, key) {
                Some(a) => ToolOutput::text(format!(
                    "id: {}\nname: {}\nkind: {}\nbytes: {}\nsha256: {}\npath: {}",
                    a.id,
                    a.name.as_deref().unwrap_or("-"),
                    a.kind,
                    a.bytes,
                    a.sha256.as_deref().unwrap_or("-"),
                    a.path
                )),
                None => ToolOutput::error(format!("no artifact named {key:?}")),
            }
        }
        "artifact_fetch" => {
            let key = args["name"].as_str().unwrap_or("");
            let max = args["max_bytes"].as_u64().unwrap_or(5 * 1024 * 1024) as usize;
            let rows = match engine.store().artifacts(&id) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let Some(row) = artifact_row(&rows, key) else {
                return ToolOutput::error(format!("no artifact named {key:?}"));
            };
            let base = engine.task_dir(&id).join("artifacts");
            let path = match confined_path(&base, &row.path) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e),
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) if b.len() <= max => b,
                Ok(b) => {
                    return ToolOutput::error(format!(
                        "artifact is {} bytes, over the {max} byte limit; raise max_bytes",
                        b.len()
                    ));
                }
                Err(e) => return ToolOutput::error(format!("read {}: {e}", path.display())),
            };
            match String::from_utf8(bytes) {
                // Text artifacts (CSV/JSON/TXT) are more useful verbatim than
                // base64; anything that is not valid UTF-8 is encoded.
                Ok(text) => ToolOutput::text(text),
                Err(e) => ToolOutput::text(base64::engine::general_purpose::STANDARD.encode(e.as_bytes())),
            }
        }
        "task_list" => {
            let limit = args["limit"].as_u64().unwrap_or(20) as usize;
            match engine.store().tasks(limit) {
                Ok(tasks) if tasks.is_empty() => ToolOutput::text("(no tasks)"),
                Ok(tasks) => ToolOutput::text(
                    tasks
                        .iter()
                        .map(|t| format!("{}  {:<16} {}", t.id, t.state, t.goal))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "task_approve" => {
            let reject = args["decision"].as_str() == Some("reject");
            let reason = args["reason"].as_str().unwrap_or("rejected via MCP");
            let result = if reject {
                engine.reject(&id, reason).map(|n| format!("rejected {n} approval(s)"))
            } else {
                engine.approve(&id).map(|n| format!("approved {n} approval(s)"))
            };
            match result {
                Ok(msg) => {
                    let next = if reject { "rejected" } else { "approved" };
                    ToolOutput::text(format!("{msg}; task {id} is {next}. Call task_resume to continue."))
                }
                Err(e) => ToolOutput::error(e.to_string()),
            }
        }
        "task_cancel" => match engine.cancel(&id) {
            Ok(()) => ToolOutput::text(format!("task {id} canceled")),
            Err(e) => ToolOutput::error(e.to_string()),
        },
        other => ToolOutput::error(format!("unknown tool: {other}")),
    }
}

// --------------------------------------------------------- interactive jobs

/// Realtime interaction over MCP. `task_run` runs an interactive task to
/// completion inside one call, which is useless for a controller meant to run
/// for minutes: these tools start it in the background, let the caller swap
/// rules mid-run, and stop it without waiting for a goal.
async fn interactive_tool(
    engine: &EngineState,
    jobs: &Jobs,
    home: &Path,
    name: &str,
    args: Value,
) -> ToolOutput {
    let id = args["task_id"].as_str().unwrap_or("").trim().to_string();

    match name {
        "interactive_start" => {
            let mut spec: TaskSpec = match serde_json::from_value(args["task"].clone()) {
                Ok(s) => s,
                Err(e) => return ToolOutput::error(format!("bad task: {e}")),
            };
            if spec.kind != TaskKind::Interactive {
                return ToolOutput::error(
                    "interactive_start needs kind=interactive; use task_run for workflow tasks",
                );
            }
            if let Err(e) = bh_task::check_spec(&spec) {
                return ToolOutput::error(e.to_string());
            }
            let Some(policy) = spec.controller.clone() else {
                return ToolOutput::error("interactive task has no controller policy");
            };
            // The engine checks this again before it navigates; checking here
            // means the tool answers with an error instead of starting a job
            // that is already doomed.
            if let Some(url) = &policy.start_url {
                if !spec.policy.host_allowed(url) {
                    return ToolOutput::error(format!(
                        "navigation to {url} blocked: host not in allowed_hosts"
                    ));
                }
            }
            let id = spec
                .id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| ulid::Ulid::new().to_string());
            spec.id = Some(id.clone());
            let headless = args["headless"].as_bool().unwrap_or(true);

            let mut guard = jobs.lock().await;
            if guard.contains_key(&id) {
                return ToolOutput::error(format!(
                    "interactive job {id} already exists; pick another id or stop it first"
                ));
            }
            let cancel = Arc::new(AtomicBool::new(false));
            let (tx, rx) = watch::channel(policy.clone());
            let job = Arc::new(InteractiveJob {
                cancel: cancel.clone(),
                policy: tx,
                error: Arc::new(StdMutex::new(None)),
                done: Arc::new(AtomicBool::new(false)),
            });
            guard.insert(id.clone(), job.clone());
            drop(guard);

            // Its own engine and its own browser: a workflow `task_run` holding
            // the shared engine must not freeze the controller (or be frozen by
            // it). WAL + busy_timeout keep the two connections honest.
            let home = home.to_path_buf();
            let job_id = id.clone();
            tokio::spawn(async move {
                let result = match TaskEngine::open(&home) {
                    Ok(mut engine) => engine
                        .run(
                            spec,
                            RunOptions {
                                headless,
                                cancel: Some(cancel),
                                controller_updates: Some(rx),
                            },
                        )
                        .await
                        .map(|o| o.state),
                    Err(e) => Err(e),
                };
                if let Err(e) = result {
                    tracing::warn!(task_id = %job_id, error = %e, "interactive job failed");
                    *job.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                }
                job.done.store(true, Ordering::SeqCst);
            });

            ToolOutput::text(format!(
                "task_id: {id}\nstate: starting\npolicy_version: {}\n\
                 Use interactive_status to follow it, interactive_update_policy to replace the rules, \
                 interactive_stop to end it.",
                policy.policy_version
            ))
        }

        "interactive_update_policy" => {
            let policy: ControllerPolicy = match serde_json::from_value(args["policy"].clone()) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(format!("bad policy: {e}")),
            };
            let Some(job) = jobs.lock().await.get(&id).cloned() else {
                return ToolOutput::error(format!("no interactive job {id}"));
            };
            if !job.running() {
                return ToolOutput::error(format!("interactive job {id} has finished"));
            }
            let current = job.policy.borrow().policy_version;
            if policy.policy_version <= current {
                return ToolOutput::error(format!(
                    "policy_version {} is not newer than {current}; a late update is refused",
                    policy.policy_version
                ));
            }
            let version = policy.policy_version;
            let rules = policy.rules.len();
            // The controller adopts at its next tick. Only the newest version
            // survives in the channel: intermediate policies are not history.
            if job.policy.send(policy).is_err() {
                return ToolOutput::error(format!("interactive job {id} has finished"));
            }
            ToolOutput::text(format!(
                "policy_version {version} queued for {id} ({rules} rule(s)); adopted at the next tick"
            ))
        }

        "interactive_status" => {
            let job = jobs.lock().await.get(&id).cloned();
            let mut out = String::new();
            if let Some(j) = &job {
                let running = j.running();
                out.push_str(&format!("job: {}\n", if running { "running" } else { "finished" }));
                out.push_str(&format!("policy_version: {}\n", j.policy.borrow().policy_version));
                if let Some(e) = j.error.lock().unwrap_or_else(|p| p.into_inner()).as_deref() {
                    out.push_str(&format!("error: {e}\n"));
                }
            }
            let mut guard = engine.lock().await;
            let Some(engine) = guard.as_mut() else {
                return ToolOutput::error("task store unavailable (see server stderr)");
            };
            match engine.store().task(&id) {
                Ok(t) => {
                    out.push_str(&format!("task: {} {}\n", t.id, t.state));
                    let events = engine.store().events(&id, 0).unwrap_or_default();
                    for e in events.iter().rev().take(3).rev() {
                        out.push_str(&format!("  {} {}\n", e.kind, e.data));
                    }
                }
                Err(_) if job.is_some() => out.push_str("task: starting\n"),
                Err(_) => {
                    return ToolOutput::error(format!(
                        "no interactive job {id} and no stored task by that id"
                    ));
                }
            }
            ToolOutput::text(out)
        }

        "interactive_stop" => {
            let Some(job) = jobs.lock().await.get(&id).cloned() else {
                return ToolOutput::error(format!("no interactive job {id}"));
            };
            if !job.running() {
                return ToolOutput::text(format!("interactive job {id} has already finished"));
            }
            job.cancel.store(true, Ordering::Relaxed);
            // The loop notices within a tick, releases input, waits one
            // reporting cycle, then closes its browser. Wait for that so the
            // answer states the outcome instead of "stopping".
            for _ in 0..50 {
                if !job.running() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let mut guard = engine.lock().await;
            let state = guard
                .as_mut()
                .and_then(|e| e.store().task(&id).ok())
                .map(|t| t.state)
                .unwrap_or_else(|| "stopping".into());
            ToolOutput::text(format!("task_id: {id}\nstate: {state}"))
        }

        other => ToolOutput::error(format!("unknown tool: {other}")),
    }
}

fn render_task(engine: &TaskEngine, id: &str) -> std::result::Result<String, String> {
    let task = engine.store().task(id).map_err(|e| e.to_string())?;
    let mut s = format!("{}  {}  {}\n", task.id, task.state, task.goal);
    s.push_str(&format!("created {}\n", task.created_at));
    for st in engine.store().steps(id).map_err(|e| e.to_string())? {
        s.push_str(&format!(
            "  {:<10} {:<10} verified={:<6} attempts={:<3} {}\n",
            st.step_id,
            st.state,
            st.verified.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            st.attempts,
            st.goal
        ));
        if let Some(e) = st.error {
            s.push_str(&format!("    {e}\n"));
        }
    }
    for a in engine.store().approvals(id).map_err(|e| e.to_string())? {
        s.push_str(&format!(
            "  approval {:<10} {:<10} {}\n",
            a.step_id.unwrap_or_else(|| "-".into()),
            a.decision.unwrap_or_else(|| "PENDING".into()),
            a.reason
        ));
    }
    for a in engine.store().artifacts(id).map_err(|e| e.to_string())? {
        s.push_str(&format!("  artifact {} ({} bytes)\n", a.path, a.bytes));
    }
    if let Some(c) = engine.store().last_checkpoint(id).map_err(|e| e.to_string())? {
        s.push_str(&format!("  checkpoint seq={} {}\n", c.seq, c.state));
    }
    Ok(s)
}

fn artifact_row<'a>(rows: &'a [bh_task::ArtifactRow], key: &str) -> Option<&'a bh_task::ArtifactRow> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    rows.iter().find(|a| a.id == key || a.name.as_deref() == Some(key))
}

/// Resolve a stored artifact path and prove it is inside the task's artifact
/// directory. The row's name comes from the site, so neither the name nor the
/// path may be trusted as a filesystem path on its own.
fn confined_path(base: &std::path::Path, row_path: &str) -> std::result::Result<PathBuf, String> {
    let base = base
        .canonicalize()
        .map_err(|e| format!("artifacts dir {}: {e}", base.display()))?;
    let path = std::path::Path::new(row_path)
        .canonicalize()
        .map_err(|e| format!("artifact {row_path}: {e}"))?;
    if !path.starts_with(&base) {
        return Err(format!(
            "artifact path {} is outside {}",
            path.display(),
            base.display()
        ));
    }
    Ok(path)
}

fn render_report(r: &TaskReport) -> String {
    let mut s = format!("{} in {} ms\n", status_word(r), r.elapsed_ms);
    for step in &r.steps {
        s.push_str(&format!(
            "{} {:?} verified={} {} ms {} {}\n",
            step.id,
            step.status,
            step.verified,
            step.elapsed_ms,
            step.goal,
            step.error.clone().unwrap_or_default()
        ));
    }
    for a in &r.artifacts {
        s.push_str(&format!("artifact: {a}\n"));
    }
    s
}

fn status_word(r: &TaskReport) -> &'static str {
    match r.status {
        bh_exec::TaskStatus::Succeeded => "SUCCEEDED",
        bh_exec::TaskStatus::Failed => "FAILED",
        bh_exec::TaskStatus::WaitingApproval => "WAITING_APPROVAL",
    }
}

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "browser_create_session",
            "description": "Start a browser session (the first tool call starts one automatically).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "headless": { "type": "boolean", "default": true },
                    "download_dir": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "browser_navigate",
            "description": "Navigate the active page and wait for load.",
            "inputSchema": {
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }
        }),
        json!({
            "name": "browser_observe",
            "description": "Return the interactive elements of the current page as a numbered index (e1, e2, ...) with role, name and value. This is the canonical way to find what to act on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string" },
                    "max_elements": { "type": "integer", "minimum": 1 },
                    "screenshot": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "browser_execute_actions",
            "description": "Run actions in order against the current page. Targets accept {\"role\":\"button\",\"name\":\"导出 CSV\"}, {\"label\":\"用户名\"}, {\"placeholder\":\"...\"}, {\"test_id\":\"...\"}, {\"css\":\"#id\"} or {\"x\":1,\"y\":2}. Stops at the first failure.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "actions": { "type": "array", "items": { "type": "object" }, "minItems": 1 },
                    "observe_after": { "type": "boolean", "default": false }
                },
                "required": ["actions"]
            }
        }),
        json!({
            "name": "browser_check_condition",
            "description": "Evaluate a success condition now, or wait up to timeout_ms for it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "condition": { "type": "object" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "default": 0 }
                },
                "required": ["condition"]
            }
        }),
        json!({
            "name": "browser_verify",
            "description": "Ask a text model whether the current page state shows the goal achieved. Returns achieved / not_achieved / uncertain (below threshold) plus confidence and reason. This is a second opinion, not an action: use browser_check_condition for mechanical checks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string" },
                    "threshold": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.6 }
                },
                "required": ["goal"]
            }
        }),
        json!({
            "name": "browser_evaluate",
            "description": "Evaluate a JavaScript expression and return its value. Defaults to an isolated world: it sees the DOM but not page-defined globals, so page scripts cannot shadow what you read. Pass world=main only when you need a page global.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "script": { "type": "string" },
                    "world": { "type": "string", "enum": ["isolated", "main"], "default": "isolated" }
                },
                "required": ["script"]
            }
        }),
        json!({
            "name": "browser_run_task",
            "description": "Run a full task spec (steps with preconditions, actions, success conditions, retries). Use this when the work is known up front; prefer observe+execute_actions when it is not.",
            "inputSchema": {
                "type": "object",
                "properties": { "task": { "type": "object" } },
                "required": ["task"]
            }
        }),
        json!({
            "name": "task_run",
            "description": "Run a task spec through the task engine: persisted state, checkpoints, approvals, artifacts. An empty workflow with mode=auto and start_url is planned against that page before the first step runs. Returns the task id — use task_status to follow it and task_resume after an approval.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "object" },
                    "headless": { "type": "boolean", "default": true }
                },
                "required": ["task"]
            }
        }),
        json!({
            "name": "interactive_start",
            "description": "Start an interactive (controller) task in the background and return immediately with its task_id. The controller samples page state at controller.observe.max_hz and dispatches rule actions at tick_hz; it is not in the model loop. Use interactive_update_policy to change the rules while it runs, interactive_status to follow it, interactive_stop to end it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "object" },
                    "headless": { "type": "boolean", "default": true }
                },
                "required": ["task"]
            }
        }),
        json!({
            "name": "interactive_status",
            "description": "Live state of an interactive job: running or finished, the policy version in force, and the last controller events (ticks, samples, actions, stop reason).",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "interactive_update_policy",
            "description": "Replace the controller policy of a running interactive job. policy_version must be strictly newer than the running one; the loop adopts it at its next tick and late updates are refused. A new policy can shorten the deadline but cannot extend it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "policy": { "type": "object" }
                },
                "required": ["task_id", "policy"]
            }
        }),
        json!({
            "name": "interactive_stop",
            "description": "Stop a running interactive job: the controller releases every held input, lets the page report it, and closes its browser. Waits up to 2.5 s for the final state.",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "task_status",
            "description": "Task state, steps (with verified flags), pending approvals and artifacts.",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "task_list",
            "description": "Recent tasks, most recent first.",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 1, "default": 20 } }
            }
        }),
        json!({
            "name": "task_artifacts",
            "description": "List a task's artifacts: name, size, sha256 and the on-disk path.",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "artifact_list",
            "description": "List a task's artifacts by id, name, kind and size. Use the id or name with artifact_metadata / artifact_fetch.",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "artifact_metadata",
            "description": "Metadata for one artifact: id, name, kind, size, sha256, path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "name": { "type": "string", "description": "artifact name or id" }
                },
                "required": ["task_id", "name"]
            }
        }),
        json!({
            "name": "artifact_fetch",
            "description": "Fetch one artifact's bytes as text (UTF-8 artifacts such as CSV/JSON) or base64. Capped at max_bytes so a large file cannot flood the context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "name": { "type": "string", "description": "artifact name or id" },
                    "max_bytes": { "type": "integer", "minimum": 1, "default": 5242880 }
                },
                "required": ["task_id", "name"]
            }
        }),
        json!({
            "name": "task_resume",
            "description": "Resume a task from its last checkpoint. A step interrupted by a crash asks for approval first instead of being retried.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "headless": { "type": "boolean", "default": true }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "task_approve",
            "description": "Approve or reject a waiting task's pending approvals. Does not resume it; call task_resume afterwards.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "decision": { "type": "string", "enum": ["approve", "reject"], "default": "approve" },
                    "reason": { "type": "string" }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "task_cancel",
            "description": "Cancel a task. A running task stops between steps.",
            "inputSchema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "browser_screenshot",
            "description": "Capture the viewport as a JPEG image.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "browser_close",
            "description": "Close the browser session.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(id: &str, name: Option<&str>, path: PathBuf) -> bh_task::ArtifactRow {
        bh_task::ArtifactRow {
            id: id.into(),
            kind: "download".into(),
            name: name.map(str::to_string),
            path: path.to_string_lossy().into_owned(),
            bytes: 3,
            sha256: Some("abc".into()),
        }
    }

    #[test]
    fn artifacts_are_found_by_exact_name_or_id_only() {
        let rows = vec![
            artifact("a1", Some("orders.csv"), PathBuf::from("/tmp/x")),
            artifact("a2", None, PathBuf::from("/tmp/y")),
        ];
        assert_eq!(artifact_row(&rows, "a1").unwrap().id, "a1");
        assert_eq!(artifact_row(&rows, "orders.csv").unwrap().id, "a1");
        assert!(artifact_row(&rows, "").is_none());
        assert!(artifact_row(&rows, "a2").is_some());
        assert!(artifact_row(&rows, "orders").is_none());
    }

    #[test]
    fn a_path_outside_the_task_directory_is_refused() {
        let base = std::env::temp_dir().join(format!("bhx-confine-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("bhx-outside-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("ok.csv"), b"a,b\n").unwrap();
        std::fs::write(&outside, b"nope").unwrap();

        assert!(confined_path(&base, base.join("ok.csv").to_str().unwrap()).is_ok());
        assert!(confined_path(&base, outside.to_str().unwrap()).is_err());
        assert!(confined_path(&base, base.join("../escape").to_str().unwrap()).is_err());

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_file(&outside).ok();
    }
}
