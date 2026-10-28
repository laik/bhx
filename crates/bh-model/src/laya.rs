//! Laya typed-decision sidecar adapter.
//!
//! Laya is a non-autoregressive encoder, not a chat model: it answers one
//! choice question over options the caller supplies and never generates text.
//! That is exactly the shape of `Capability::Locate` and `Capability::Verify`,
//! and exactly not the shape of `Plan` or `Repair` — so this adapter refuses
//! those instead of pretending a choice is a step list.
//!
//! The sidecar (`sidecars/laya/decide.py`) is a long-lived Python process that
//! loads the model once (~5 s) and then answers one JSON line per request.
//! This adapter owns that lifecycle: spawn lazily on first use, wait for the
//! `{"ready":true,"backend":...}` handshake, keep the process warm across
//! calls, and kill it if an exchange times out or the pipe breaks so the next
//! request starts a fresh one. A fresh process per request would pay the model
//! load every time; a never-restarted process turns one hung inference into a
//! permanently wedged task. Adapters built from the same configuration share
//! one process, so reconstructing a router does not reload the graph.
//!
//! Configuration (leaving `BH_LAYA_DECIDE` unset means no Laya adapter):
//!
//! - `BH_LAYA_DECIDE`             path to `decide.py`
//! - `BH_LAYA_PYTHON`             interpreter (default `python3`)
//! - `BH_LAYA_STARTUP_TIMEOUT_MS` handshake budget (default 20000)
//! - `BH_LAYA_MAX_CRITERIA`       refuse more candidates than this (default 40)
//!
//! Credentials never reach this adapter: it only sees the observer's state
//! text, where password fields are already rendered as `***`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use bh_core::{BhError, Result};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::{Capability, ModelAdapter, ModelProposal, ModelRequest};

/// Above ~10 candidates the sidecar's own top probability collapses. It is a
/// warning there; here it is a hard limit against callers that never filter.
const DEFAULT_MAX_CRITERIA: usize = 40;
const RANK_WARNING_ABOVE: usize = 10;

pub struct LayaSidecar {
    python: String,
    script: PathBuf,
    startup: Duration,
    max_criteria: usize,
    /// `None` until the first request; an exchange that fails takes the
    /// process out so the next request spawns a new one.
    child: Mutex<Option<Running>>,
    starts: AtomicU32,
}

struct Running {
    /// Kept only for ownership: dropping it kills the process
    /// (`kill_on_drop`), which is how `shutdown` and timeout cleanup work.
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl LayaSidecar {
    /// `Err` means Laya is configured but unusable (a missing script); `None`
    /// means it is not configured and the router should not offer it.
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let Some(script) = env("BH_LAYA_DECIDE") else {
            return Ok(None);
        };
        let script = PathBuf::from(script);
        if !script.is_file() {
            return Err(BhError::Other(format!(
                "BH_LAYA_DECIDE points to a missing file: {}",
                script.display()
            )));
        }
        let python = env("BH_LAYA_PYTHON").unwrap_or_else(|| "python3".into());
        let startup = Duration::from_millis(env("BH_LAYA_STARTUP_TIMEOUT_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(20_000));
        let max_criteria = env("BH_LAYA_MAX_CRITERIA")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MAX_CRITERIA);
        Ok(Some(shared(python, script, startup, max_criteria)))
    }

    fn new(python: String, script: PathBuf, startup: Duration, max_criteria: usize) -> Self {
        Self {
            python,
            script,
            startup,
            max_criteria,
            child: Mutex::new(None),
            starts: AtomicU32::new(0),
        }
    }

    /// How many sidecar processes this adapter has launched. Diagnostics (and
    /// tests): a value above one means a process was lost or a spawn failed.
    pub fn starts(&self) -> u32 {
        self.starts.load(Ordering::SeqCst)
    }

    /// Stop the sidecar now instead of at drop. Safe to call when none runs.
    pub async fn shutdown(&self) {
        self.child.lock().await.take();
    }

    /// The request line handed to the sidecar. Split out so the mapping from
    /// capability to question can be tested without a model.
    fn body(&self, req: &ModelRequest) -> Result<Value> {
        if req.image.is_some() {
            return Err(BhError::Other(
                "laya has no vision; send screenshots to a visual model".into(),
            ));
        }
        if req.criteria.is_empty() {
            return Err(BhError::Other(
                "laya needs candidate ids to choose from (criteria is empty)".into(),
            ));
        }
        if req.criteria.len() > self.max_criteria {
            return Err(BhError::Other(format!(
                "laya got {} candidates, more than the {} it will rank; prefilter first",
                req.criteria.len(),
                self.max_criteria
            )));
        }
        if req.criteria.len() > RANK_WARNING_ABOVE {
            tracing::warn!(
                candidates = req.criteria.len(),
                "laya top probability collapses above ~10 candidates; prefilter for accuracy"
            );
        }
        let state = req.state.as_deref().unwrap_or("").trim();
        if state.is_empty() {
            return Err(BhError::Other("laya was given no page state".into()));
        }
        let instructions = match req.capability {
            Capability::Verify => {
                "Does the page state show the task's goal achieved? Pick the option that matches."
            }
            Capability::Locate => "Which option should be used next to make progress on the task?",
            other => {
                return Err(BhError::Other(format!(
                    "laya cannot do {other:?}: it answers choice questions, it does not generate steps"
                )));
            }
        };
        let mut criteria = Map::new();
        for (id, description) in &req.criteria {
            if criteria.insert(id.clone(), Value::String(description.clone())).is_some() {
                return Err(BhError::Other(format!("duplicate candidate id {id:?}")));
            }
        }
        Ok(json!({
            "state": state,
            "instructions": instructions,
            "criteria": Value::Object(criteria),
        }))
    }

    async fn ask(&self, body: &Value, wait: Duration) -> Result<Value> {
        let mut guard = self.child.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn().await?);
        }
        let line = serde_json::to_string(body)
            .map_err(|e| BhError::Other(format!("laya request is not JSON: {e}")))?;
        let exchanged = {
            let running = guard.as_mut().expect("spawned above");
            exchange(running, &line, wait).await
        };
        match exchanged {
            Ok(reply) => Ok(reply),
            Err(e) => {
                // The pipe is in an unknown state (timeout mid-line, broken
                // write, EOF). Drop the process rather than trust it again.
                guard.take();
                Err(e)
            }
        }
    }

    async fn spawn(&self) -> Result<Running> {
        let mut cmd = Command::new(&self.python);
        cmd.arg(&self.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            BhError::Other(format!(
                "spawn laya sidecar ({} {}): {e}",
                self.python,
                self.script.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BhError::Other("laya sidecar has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BhError::Other("laya sidecar has no stdout".into()))?;
        let mut stdout = BufReader::new(stdout).lines();

        let ready = match timeout(self.startup, stdout.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                return Err(BhError::Other(
                    "laya sidecar exited before the ready handshake".into(),
                ));
            }
            Ok(Err(e)) => {
                return Err(BhError::Other(format!("reading laya handshake: {e}")));
            }
            Err(_) => {
                return Err(BhError::Timeout(
                    self.startup.as_millis().try_into().unwrap_or(u64::MAX),
                ));
            }
        };
        let backend = parse_ready(&ready)?;
        self.starts.fetch_add(1, Ordering::SeqCst);
        tracing::info!(
            backend = %backend,
            script = %self.script.display(),
            "laya sidecar ready"
        );
        Ok(Running { _child: child, stdin, stdout })
    }
}

#[async_trait]
impl ModelAdapter for LayaSidecar {
    fn name(&self) -> &str {
        "laya"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(capability, Capability::Locate | Capability::Verify)
    }

    async fn propose(&self, req: &ModelRequest) -> Result<ModelProposal> {
        let body = self.body(req)?;
        let reply = self
            .ask(&body, Duration::from_millis(req.timeout_ms.max(1)))
            .await?;
        if let Some(warning) = reply["warning"].as_str() {
            tracing::warn!(warning = %warning, "laya sidecar");
        }
        proposal(&reply, req)
    }
}

/// One sidecar per configuration for the life of the process.
///
/// Callers that build a router per request (the MCP `browser_verify` tool
/// does) would otherwise load an 800 MB graph before every answer. Sharing is
/// safe because an adapter only ever has one exchange in flight; the model is
/// deterministic and the budget still belongs to the router, not the sidecar.
fn shared(python: String, script: PathBuf, startup: Duration, max_criteria: usize) -> Arc<LayaSidecar> {
    static REGISTRY: OnceLock<StdMutex<HashMap<String, Arc<LayaSidecar>>>> = OnceLock::new();
    let key = format!(
        "{python}\u{0}{}\u{0}{}\u{0}{max_criteria}",
        script.display(),
        startup.as_millis()
    );
    let registry = REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()));
    // A poisoned lock here only means another thread panicked while looking
    // up; the map itself is still consistent.
    let mut map = registry.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key)
        .or_insert_with(|| Arc::new(LayaSidecar::new(python, script, startup, max_criteria)))
        .clone()
}

fn parse_ready(line: &str) -> Result<String> {
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| {
        BhError::Other(format!(
            "laya handshake is not JSON ({}): {}",
            e,
            clip(line)
        ))
    })?;
    if v["ready"].as_bool() != Some(true) {
        return Err(BhError::Other(format!(
            "laya handshake is not ready: {}",
            clip(line)
        )));
    }
    Ok(v["backend"].as_str().unwrap_or("unknown").to_string())
}

async fn exchange(r: &mut Running, line: &str, wait: Duration) -> Result<Value> {
    let io = async {
        r.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| io_err("write", e))?;
        r.stdin.write_all(b"\n").await.map_err(|e| io_err("write", e))?;
        r.stdin.flush().await.map_err(|e| io_err("flush", e))?;
        loop {
            let Some(text) = r.stdout.next_line().await.map_err(|e| io_err("read", e))? else {
                return Err(BhError::Other(
                    "laya sidecar closed its output (backend died?)".into(),
                ));
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(text) {
                Ok(v) if v.is_object() => return Ok(v),
                // Python warnings on stdout must not desynchronise the
                // request/reply pairing. They are noise, not answers.
                _ => tracing::debug!(line = text, "ignoring non-JSON line from laya sidecar"),
            }
        }
    };
    match timeout(wait, io).await {
        Ok(reply) => reply,
        Err(_) => Err(BhError::Timeout(
            wait.as_millis().try_into().unwrap_or(u64::MAX),
        )),
    }
}

/// Map the sidecar's reply onto the router's proposal shape. Every field the
/// router validates is either present or an error here — a missing confidence
/// must not silently become 0.0, which would read as "uncertain".
fn proposal(reply: &Value, req: &ModelRequest) -> Result<ModelProposal> {
    if let Some(e) = reply["error"].as_str() {
        return Err(BhError::Other(format!("laya sidecar error: {e}")));
    }
    let choice = reply["choice"]
        .as_str()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| BhError::Other(format!("laya reply has no choice: {}", clip(&reply.to_string()))))?
        .to_string();
    let confidence = reply["conf"]
        .as_f64()
        .or_else(|| reply["confidence"].as_f64())
        .or_else(|| best_prob(reply))
        .ok_or_else(|| BhError::Other("laya reply has no confidence".into()))?;
    let backend = reply["backend"].as_str().unwrap_or("unknown");
    let ms = reply["ms"].as_u64().unwrap_or(0);
    let n = reply["n_criteria"]
        .as_u64()
        .unwrap_or(req.criteria.len() as u64);
    Ok(ModelProposal {
        choice: Some(choice.clone()),
        confidence,
        reason: Some(format!(
            "{backend} chose {choice} (p={confidence:.2}, {n} candidate(s), {ms} ms)"
        )),
        model: "laya".into(),
        ..Default::default()
    })
}

fn best_prob(reply: &Value) -> Option<f64> {
    let mut best: Option<f64> = None;
    for p in reply["probs"].as_object()?.values().filter_map(Value::as_f64) {
        best = Some(best.map_or(p, |b| b.max(p)));
    }
    best
}

fn io_err(what: &str, e: std::io::Error) -> BhError {
    BhError::Other(format!("laya sidecar {what}: {e}"))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelAdapter as _, ModelRouter};
    use std::path::Path;
    use std::sync::Arc;

    fn stub() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testkit/laya-stub.py")
    }

    fn python_ok() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Process tests need the stub and an interpreter; skip loudly otherwise.
    fn have_sidecar() -> bool {
        if !stub().is_file() {
            eprintln!("skip: {} missing", stub().display());
            return false;
        }
        if !python_ok() {
            eprintln!("skip: python3 not on PATH");
            return false;
        }
        true
    }

    fn sidecar(script: PathBuf) -> LayaSidecar {
        LayaSidecar::new(
            "python3".into(),
            script,
            Duration::from_secs(15),
            DEFAULT_MAX_CRITERIA,
        )
    }

    fn page(task: &str) -> String {
        format!("Task: {task}\nURL: http://127.0.0.1:8766/\nControls:\ne1 button \"导出 CSV\"\n")
    }

    #[test]
    fn supports_only_the_choice_capabilities() {
        let s = LayaSidecar::new("python3".into(), "x.py".into(), Duration::from_secs(1), 10);
        assert!(s.supports(Capability::Verify));
        assert!(s.supports(Capability::Locate));
        assert!(!s.supports(Capability::Plan));
        assert!(!s.supports(Capability::Repair));
        assert!(!s.supports(Capability::VisualLocate));
    }

    #[test]
    fn body_preserves_candidate_order() {
        let s = LayaSidecar::new("python3".into(), "x.py".into(), Duration::from_secs(1), 10);
        let req = ModelRequest::locate(
            "导出失败订单",
            "Task: x\nURL: http://x\n",
            vec![
                ("e7".into(), "button \"导出 CSV\"".into()),
                ("e12".into(), "button \"重置\"".into()),
                ("e3".into(), "link \"订单\"".into()),
            ],
        );
        let body = s.body(&req).unwrap();
        let keys: Vec<&String> = body["criteria"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["e7", "e12", "e3"]);
        assert_eq!(body["state"], "Task: x\nURL: http://x");
        assert!(body["instructions"].as_str().unwrap().contains("option"));
    }

    #[test]
    fn body_refuses_what_a_choice_model_cannot_do() {
        let s = LayaSidecar::new("python3".into(), "x.py".into(), Duration::from_secs(1), 10);
        let mut req = ModelRequest::verify("done", "Task: x");
        req.criteria.clear();
        assert!(s.body(&req).unwrap_err().to_string().contains("candidate"));

        let mut req = ModelRequest::verify("done", "Task: x");
        req.image = Some(crate::ImageData {
            media_type: "image/png",
            bytes: vec![1],
            width: 1,
            height: 1,
        });
        assert!(s.body(&req).unwrap_err().to_string().contains("no vision"));

        let mut req = ModelRequest::verify("done", "Task: x");
        req.criteria = (0..11).map(|i| (format!("e{i}"), format!("opt {i}"))).collect();
        assert!(s.body(&req).unwrap_err().to_string().contains("prefilter"));

        let mut req = ModelRequest::verify("done", "Task: x");
        req.criteria = vec![("e1".into(), "a".into()), ("e1".into(), "b".into())];
        assert!(s.body(&req).unwrap_err().to_string().contains("duplicate"));

        let mut req = ModelRequest::verify("done", "Task: x");
        req.state = Some("   ".into());
        assert!(s.body(&req).unwrap_err().to_string().contains("no page state"));
    }

    #[test]
    fn a_reply_without_confidence_is_an_error_not_uncertain() {
        let req = ModelRequest::verify("done", "Task: x");
        let e = proposal(&json!({"choice": "achieved"}), &req).unwrap_err();
        assert!(e.to_string().contains("no confidence"));
        let e = proposal(&json!({"confidence": 0.9}), &req).unwrap_err();
        assert!(e.to_string().contains("no choice"));
        let e = proposal(&json!({"error": "criteria must contain at least one option"}), &req)
            .unwrap_err();
        assert!(e.to_string().contains("laya sidecar error"));
    }

    #[test]
    fn confidence_falls_back_to_the_probability_table() {
        let req = ModelRequest::verify("done", "Task: x");
        let p = proposal(
            &json!({"choice": "achieved", "probs": {"achieved": 0.83, "not_achieved": 0.17}}),
            &req,
        )
        .unwrap();
        assert!((p.confidence - 0.83).abs() < 1e-9);
        assert_eq!(p.model, "laya");
        assert!(p.reason.unwrap().contains("achieved"));
    }

    #[test]
    fn the_same_configuration_shares_one_sidecar() {
        let a = shared("python3".into(), stub(), Duration::from_secs(1), 40);
        let b = shared("python3".into(), stub(), Duration::from_secs(1), 40);
        assert!(Arc::ptr_eq(&a, &b), "a rebuilt router must reuse the process");
        let other = shared("python3".into(), stub(), Duration::from_secs(1), 41);
        assert!(!Arc::ptr_eq(&a, &other), "different limits are different adapters");
    }

    #[tokio::test]
    async fn answers_two_verifies_with_one_sidecar_process() {
        if !have_sidecar() {
            return;
        }
        let s = sidecar(stub());
        let req = ModelRequest::verify("已登录并看到订单列表", &page("已登录并看到订单列表"));
        for _ in 0..2 {
            let p = s.propose(&req).await.unwrap();
            assert_eq!(p.choice.as_deref(), Some("achieved"));
            assert_eq!(p.model, "laya");
            assert!((p.confidence - 0.9).abs() < 1e-9);
        }
        assert_eq!(s.starts(), 1, "the warm process must be reused");
        s.shutdown().await;
    }

    #[tokio::test]
    async fn a_bad_request_does_not_take_the_process_down() {
        if !have_sidecar() {
            return;
        }
        let s = sidecar(stub());
        let bad = json!({"state": "x", "instructions": "y", "criteria": {}});
        let reply = s.ask(&bad, Duration::from_secs(10)).await.unwrap();
        assert!(reply["error"].is_string());

        let req = ModelRequest::verify("done", &page("done"));
        assert_eq!(s.propose(&req).await.unwrap().choice.as_deref(), Some("achieved"));
        assert_eq!(s.starts(), 1, "an error reply must not restart the sidecar");
        s.shutdown().await;
    }

    #[tokio::test]
    async fn a_hung_request_kills_the_process_and_the_next_one_respawns() {
        if !have_sidecar() {
            return;
        }
        let s = sidecar(stub());
        let mut slow = ModelRequest::verify("x", &page("HANG"));
        slow.timeout_ms = 300;
        let e = s.propose(&slow).await.unwrap_err();
        assert!(matches!(e, BhError::Timeout(_)), "got {e}");

        let req = ModelRequest::verify("x", &page("fine"));
        assert_eq!(s.propose(&req).await.unwrap().choice.as_deref(), Some("achieved"));
        assert_eq!(s.starts(), 2, "a timed-out exchange must not be reused");
        s.shutdown().await;
    }

    #[tokio::test]
    async fn a_script_that_never_says_ready_is_a_timeout() {
        if !python_ok() {
            eprintln!("skip: python3 not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("bhx-laya-silent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("silent.py");
        std::fs::write(&script, "import time\ntime.sleep(30)\n").unwrap();
        let s = LayaSidecar::new(
            "python3".into(),
            script,
            Duration::from_millis(300),
            DEFAULT_MAX_CRITERIA,
        );
        let e = s.propose(&ModelRequest::verify("x", "Task: x")).await.unwrap_err();
        assert!(matches!(e, BhError::Timeout(_)), "got {e}");
    }

    #[tokio::test]
    async fn a_script_that_exits_early_says_so() {
        if !python_ok() {
            eprintln!("skip: python3 not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("bhx-laya-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("exit.py");
        std::fs::write(&script, "import sys\nsys.exit(3)\n").unwrap();
        let s = sidecar(script);
        let e = s.propose(&ModelRequest::verify("x", "Task: x")).await.unwrap_err();
        assert!(e.to_string().contains("before the ready handshake"), "got {e}");
    }

    #[tokio::test]
    async fn the_router_rejects_a_choice_outside_the_candidates() {
        if !have_sidecar() {
            return;
        }
        let router = ModelRouter::new(vec![Arc::new(sidecar(stub()))], 1);
        let req = ModelRequest::locate(
            "export",
            &page("BADCHOICE"),
            vec![("e7".into(), "button \"导出 CSV\"".into())],
        );
        let e = router.propose(&req).await.unwrap_err();
        assert!(e.to_string().contains("not a candidate"), "got {e}");
        assert_eq!(router.calls(), 1);
    }
}
