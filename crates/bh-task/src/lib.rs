//! Task engine: the thing that makes a task a task.
//!
//! It owns state (SQLite), artifacts, checkpoints and the approval gate.
//! `bh-exec` runs one step and tells the truth about it; this module decides
//! what happens next, persists it, and makes recovery from a killed process
//! possible without pretending the interrupted step never happened.

pub mod schedule;
pub mod store;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bh_browser::{Browser, BrowserOptions};
use bh_core::{Action, BhError, OnUncertain, Result, Step, TaskKind, TaskSpec, Viewport};
use bh_exec::controller::{self, StopReason, TickSummary};
use bh_exec::vision::Vision;
use bh_exec::{Runner, StepStatus};
use bh_observe::{ObserveOptions, Observer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub use store::{
    ArtifactRow, ApprovalRow, CheckpointRow, EventRow, NewSchedule, ScheduleRow, StepRow, Store,
    TaskRow,
};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub headless: bool,
    /// Set by MCP cancellation or a signal handler. The runner checks it
    /// between actions, so nothing is left mid-gesture.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Interactive tasks only: a running controller swaps to a policy sent
    /// here when its `policy_version` is newer. Ignored by workflow tasks.
    pub controller_updates: Option<tokio::sync::watch::Receiver<bh_core::ControllerPolicy>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self { headless: true, cancel: None, controller_updates: None }
    }
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub task_id: String,
    pub state: String,
    /// The task summary as written to `tasks.result_json`, when the run got
    /// far enough to produce one.
    pub report: Option<Value>,
}

impl RunOutcome {
    pub fn succeeded(&self) -> bool {
        self.state == "succeeded"
    }
}

/// Structural checks the engine applies before doing any work. Public because
/// a caller that starts a task in the background (the MCP `interactive_*`
/// tools) must reject a bad spec while it still has someone to answer.
pub fn check_spec(spec: &TaskSpec) -> Result<()> {
    // A bad confidence floor would silently turn "only a sure guess" into
    // "any guess at all", so it is refused before any work starts.
    if let Some(a) = &spec.policy.arbitrate_ambiguous {
        a.validate().map_err(BhError::Other)?;
    }
    match spec.kind {
        TaskKind::Workflow if spec.steps.is_empty() => {
            if spec.mode != "auto" {
                return Err(BhError::Other(
                    "workflow task has no steps (set mode=auto to let the engine plan)".into(),
                ));
            }
            if spec.goal.trim().is_empty() {
                return Err(BhError::Other("auto planning needs a non-empty goal".into()));
            }
        }
        TaskKind::Interactive => {
            let Some(c) = &spec.controller else {
                return Err(BhError::Other("interactive task has no controller policy".into()));
            };
            if spec.policy.semantic_verify.is_some() {
                return Err(BhError::Other(
                    "policy.semantic_verify applies to workflow tasks; interactive tasks end on a controller goal"
                        .into(),
                ));
            }
            if c.start_url.is_none() {
                return Err(BhError::Other("interactive task has no controller.start_url".into()));
            }
            if c.observe.script.trim().is_empty() {
                return Err(BhError::Other(
                    "interactive task has no controller.observe.script".into(),
                ));
            }
            if let Some(d) = &c.decide {
                d.validate().map_err(BhError::Other)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub struct Engine {
    store: Store,
    home: PathBuf,
}

impl Engine {
    /// `home` holds `bhx.db`, `tasks/<id>/`, `profiles/<id>/`.
    pub fn open(home: impl AsRef<Path>) -> Result<Self> {
        let home = home.as_ref().to_path_buf();
        std::fs::create_dir_all(home.join("tasks"))
            .map_err(|e| BhError::Other(format!("create tasks dir: {e}")))?;
        std::fs::create_dir_all(home.join("profiles"))
            .map_err(|e| BhError::Other(format!("create profiles dir: {e}")))?;
        let store = Store::open(&home.join("bhx.db"))?;
        Ok(Self { store, home })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// A task left in `running` belongs to a dead process.
    pub fn reap(&self) -> Result<Vec<String>> {
        self.store.reap_running()
    }

    // ------------------------------------------------------------------ run

    pub async fn run(&mut self, spec: TaskSpec, opts: RunOptions) -> Result<RunOutcome> {
        check_spec(&spec)?;
        let mut spec = normalize(spec)?;
        if spec.policy.max_wall_ms == 0 {
            spec.policy.max_wall_ms = 600_000;
        }

        let id = spec
            .id
            .clone()
            .unwrap_or_else(|| ulid::Ulid::new().to_string());
        let dir = self.task_dir(&id);
        for sub in ["obs", "shots", "artifacts", "tmp"] {
            std::fs::create_dir_all(dir.join(sub))
                .map_err(|e| BhError::Other(format!("create {sub}: {e}")))?;
        }

        // Credentials stay in memory: `stored` has the placeholders and no
        // values; `execute` resolves a runnable copy after any planning.
        let stored = spec_for_disk(&spec);
        std::fs::write(dir.join("task.json"), stored.to_string())
            .map_err(|e| BhError::Other(format!("write task.json: {e}")))?;

        self.store.create_task(&store::NewTask {
            id: &id,
            kind: match spec.kind {
                bh_core::TaskKind::Workflow => "workflow",
                bh_core::TaskKind::Interactive => "interactive",
            },
            goal: &spec.goal,
            mode: &spec.mode,
            policy_json: &serde_json::to_string(&spec.policy).unwrap_or_default(),
            session_id: &id,
        })?;
        self.log(
            &id,
            "task.created",
            json!({
                "goal": spec.goal,
                "steps": spec.steps.len(),
                "kind": if spec.kind == TaskKind::Interactive { "interactive" } else { "workflow" },
            }),
        );

        self.execute(&id, &spec, 0, Vec::new(), opts).await
    }

    // --------------------------------------------------------------- resume

    pub async fn resume(&mut self, task_id: &str, opts: RunOptions) -> Result<RunOutcome> {
        let task = self.store.task(task_id)?;
        if matches!(task.state.as_str(), "succeeded" | "failed" | "canceled") {
            return Ok(RunOutcome {
                task_id: task_id.to_string(),
                state: task.state,
                report: None,
            });
        }
        if task.kind == "interactive" {
            // There is no checkpoint to resume from: the controller's value is
            // in its input stream, and replaying it from the start is not a
            // recovery, it is a second run.
            return Err(BhError::Other(format!(
                "task {task_id} is interactive; it cannot be resumed"
            )));
        }

        let spec_path = self.task_dir(task_id).join("task.json");
        let raw = std::fs::read_to_string(&spec_path)
            .map_err(|e| BhError::Other(format!("read {}: {e}", spec_path.display())))?;
        let spec: TaskSpec = serde_json::from_str(&raw)?;
        let mut spec = normalize(spec)?;
        if spec.policy.max_wall_ms == 0 {
            spec.policy.max_wall_ms = 600_000;
        }

        let approvals = self.store.approvals(task_id)?;
        let pending: Vec<_> = approvals.iter().filter(|a| a.decision.is_none()).collect();
        if !pending.is_empty() {
            return Ok(RunOutcome {
                task_id: task_id.to_string(),
                state: "waiting_approval".into(),
                report: None,
            });
        }

        // A step that was in flight when the process died is never retried on
        // its own: the side effect may already have happened.
        let interrupted = self.store.interrupted_steps(task_id)?;
        if !interrupted.is_empty() && approvals.is_empty() {
            let step = &interrupted[0];
            let approval = self.store.request_approval(
                task_id,
                Some(&step.step_id),
                "process died while this step was running; side effects are unknown",
                &json!({ "next_index": step.idx }),
            )?;
            self.store.set_task_state(task_id, "waiting_approval")?;
            self.log(
                task_id,
                "approval.requested",
                json!({ "approval_id": approval, "step_id": step.step_id, "reason": "crashed mid-step" }),
            );
            return Ok(RunOutcome {
                task_id: task_id.to_string(),
                state: "waiting_approval".into(),
                report: None,
            });
        }

        let approved: Vec<String> = approvals
            .iter()
            .filter(|a| a.decision.as_deref() == Some("approved"))
            .filter_map(|a| a.step_id.clone())
            .collect();
        let start = approvals
            .iter()
            .filter(|a| a.decision.as_deref() == Some("approved"))
            .filter_map(|a| a.payload["next_index"].as_u64().map(|n| n as usize))
            .min()
            .unwrap_or_else(|| {
                self.store
                    .last_checkpoint(task_id)
                    .ok()
                    .flatten()
                    .and_then(|c| c.state["next_index"].as_u64().map(|n| n as usize))
                    .unwrap_or(0)
            });

        if start >= spec.steps.len() {
            // A human approved an uncertain semantic verdict for an already
            // finished workflow: the verdict is the result, so finish without
            // launching another browser or asking the model again.
            if let Some(approved) = approvals.iter().find(|a| {
                a.decision.as_deref() == Some("approved")
                    && a.step_id.is_none()
                    && a.payload.get("semantic").is_some()
            }) {
                let summary = json!({
                    "task_id": task_id,
                    "state": "succeeded",
                    "steps": [],
                    "semantic": approved.payload["semantic"].clone(),
                    "resumed": true,
                });
                self.finish(task_id, "succeeded", Some(&summary), None)?;
                self.log(task_id, "task.finished", summary.clone());
                return Ok(RunOutcome {
                    task_id: task_id.to_string(),
                    state: "succeeded".into(),
                    report: Some(summary),
                });
            }
            // With a semantic check configured, reaching the end is not proof:
            // the previous process may have died before asking the model.
            if spec.policy.semantic_verify.is_none() {
                self.finish(task_id, "succeeded", None, None)?;
                return Ok(RunOutcome {
                    task_id: task_id.to_string(),
                    state: "succeeded".into(),
                    report: None,
                });
            }
        }

        self.log(task_id, "task.resumed", json!({ "from_step": start }));
        self.execute(task_id, &spec, start, approved, opts).await
    }

    pub fn approve(&self, task_id: &str) -> Result<usize> {
        let n = self.store.decide_approval(task_id, "approved")?;
        if n == 0 {
            return Err(BhError::Other(format!("task {task_id} has no pending approval")));
        }
        self.store.set_task_state(task_id, "paused")?;
        self.log(task_id, "approval.decided", json!({ "decision": "approved" }));
        Ok(n)
    }

    pub fn reject(&self, task_id: &str, reason: &str) -> Result<usize> {
        let n = self.store.decide_approval(task_id, "rejected")?;
        if n == 0 {
            return Err(BhError::Other(format!("task {task_id} has no pending approval")));
        }
        self.finish(
            task_id,
            "failed",
            None,
            Some(&json!({ "message": format!("approval rejected: {reason}") })),
        )?;
        Ok(n)
    }

    pub fn cancel(&self, task_id: &str) -> Result<()> {
        let task = self.store.task(task_id)?;
        if matches!(task.state.as_str(), "succeeded" | "failed" | "canceled") {
            return Err(BhError::Other(format!("task {task_id} already {}", task.state)));
        }
        self.store.set_task_state(task_id, "canceled")?;
        self.finish(task_id, "canceled", None, Some(&json!({ "message": "canceled by user" })))?;
        Ok(())
    }

    // -------------------------------------------------------------- execute

    async fn execute(
        &mut self,
        id: &str,
        spec: &TaskSpec,
        start: usize,
        approved: Vec<String>,
        opts: RunOptions,
    ) -> Result<RunOutcome> {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(spec.policy.max_wall_ms);

        let dir = self.task_dir(id);
        let profile = self.home.join("profiles").join(id);
        let downloads = dir.join("tmp");
        let _lock = TaskLock::acquire(&dir)?;

        // A killed harness leaves its browser behind, holding the profile.
        reap_profile_browsers(&profile);

        let browser_opts = BrowserOptions {
            headless: opts.headless,
            user_data_dir: Some(profile),
            download_dir: Some(downloads.clone()),
            viewport: Viewport::default(),
            executable: None,
            accept_confirm: spec.policy.accept_confirm,
            cdp_url: None,
        };
        // A launch failure is the task's failure, not a dangling `pending` row:
        // record it and still return the error so the caller exits non-zero.
        let fail_launch = |engine: &Engine, e: &BhError| -> Result<()> {
            let msg = e.to_string();
            engine.finish(
                id,
                "failed",
                None,
                Some(&json!({ "message": msg, "phase": "launch" })),
            )?;
            engine.log(
                id,
                "task.finished",
                json!({ "task_id": id, "state": "failed", "error": msg, "phase": "launch" }),
            );
            Ok(())
        };
        let browser = match Browser::launch(browser_opts.clone()).await {
            Ok(b) => b,
            Err(e) => {
                fail_launch(self, &e)?;
                return Err(e);
            }
        };
        let session = match browser.new_session(&browser_opts).await {
            Ok(s) => s,
            Err(e) => {
                fail_launch(self, &e)?;
                return Err(e);
            }
        };
        let mut observer = Observer::new(ObserveOptions {
            max_elements: spec.policy.observation.max_elements,
        });
        // One router per task: the model budget in the spec applies to the
        // whole task, and `tasks.model_calls` is read back out of it.
        let vision = Vision::from_env(spec.policy.max_model_calls);

        // Resuming: put the page back where the checkpoint left it.
        if start > 0 {
            if let Some(cp) = self.store.last_checkpoint(id)? {
                if let Some(url) = cp.state["url"].as_str() {
                    if let Err(e) = session.navigate(url).await {
                        tracing::warn!(error = %e, "could not restore page for resume");
                    }
                }
            }
        }

        if spec.kind == TaskKind::Interactive {
            let runnable = spec.resolve_credentials();
            return self
                .execute_controller(
                    id,
                    &runnable,
                    &session,
                    &vision,
                    opts.cancel,
                    opts.controller_updates,
                    started,
                )
                .await;
        }

        self.store.set_task_state(id, "running")?;
        self.log(id, "task.started", json!({ "from_step": start }));

        // An empty workflow is planned against the real page, not against a
        // guess. The planned spec is written back to task.json before the
        // first step runs, so a resume continues the plan instead of asking
        // for a new one (and possibly getting different steps).
        let mut stored_spec = spec.clone();
        let mut runnable = spec.resolve_credentials();
        if spec.steps.is_empty() {
            if start > 0 {
                let msg = "task has no planned steps to resume from".to_string();
                self.finish(id, "failed", None, Some(&json!({ "message": msg })))?;
                self.log(
                    id,
                    "task.finished",
                    json!({ "task_id": id, "state": "failed", "error": msg }),
                );
                return Ok(RunOutcome {
                    task_id: id.into(),
                    state: "failed".into(),
                    report: Some(json!({
                        "task_id": id,
                        "state": "failed",
                        "steps": [],
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "error": msg,
                    })),
                });
            }
            match self.plan_workflow(id, spec, &session, &mut observer, &vision).await {
                Ok(planned) => {
                    self.store.set_model_calls(id, vision.calls())?;
                    std::fs::write(dir.join("task.json"), spec_for_disk(&planned).to_string())
                        .map_err(|e| BhError::Other(format!("write planned task.json: {e}")))?;
                    stored_spec = planned;
                    runnable = stored_spec.resolve_credentials();
                }
                Err(e) => {
                    self.store.set_model_calls(id, vision.calls())?;
                    let msg = format!("auto planning failed: {e}");
                    self.log(id, "plan.failed", json!({ "error": msg }));
                    self.finish(
                        id,
                        "failed",
                        None,
                        Some(&json!({ "message": msg, "phase": "planning" })),
                    )?;
                    self.log(
                        id,
                        "task.finished",
                        json!({ "task_id": id, "state": "failed", "error": msg, "phase": "planning" }),
                    );
                    return Ok(RunOutcome {
                        task_id: id.into(),
                        state: "failed".into(),
                        report: Some(json!({
                            "task_id": id,
                            "state": "failed",
                            "steps": [],
                            "elapsed_ms": started.elapsed().as_millis() as u64,
                            "error": msg,
                            "phase": "planning",
                        })),
                    });
                }
            }
        }
        let spec = &stored_spec;
        let steps = &runnable.steps;

        let cancel = opts.cancel.clone();
        let canceled = || {
            cancel
                .as_ref()
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        };

        let approved_set: HashSet<String> = approved.iter().cloned().collect();
        let mut reports: Vec<Value> = Vec::new();
        let mut final_state = "succeeded";

        for i in start..steps.len() {
            if canceled() || self.store.task(id)?.state == "canceled" {
                final_state = "canceled";
                break;
            }
            if Instant::now() >= deadline {
                self.finish(
                    id,
                    "failed",
                    None,
                    Some(&json!({ "message": "task wall clock budget exhausted" })),
                )?;
                return Ok(RunOutcome { task_id: id.into(), state: "failed".into(), report: Some(json!({ "state": "failed", "error": "wall clock budget exhausted" })) });
            }

            let step = &steps[i];
            let step_id = step.id.clone();
            // Persist the authored form (placeholders intact), never the
            // runnable copy whose action text may contain credential values.
            let stored_step = stored_spec.steps.get(i).unwrap_or(step);
            let spec_json = serde_json::to_string(stored_step).unwrap_or_default();
            self.store
                .upsert_step(id, &step_id, i, &step.goal, &spec_json, "pending")?;

            // Approval gate: nothing of an unapproved step runs.
            if step.needs_approval(&spec.policy) && !approved_set.contains(&step_id) {
                let reason = if step.side_effect == bh_core::SideEffect::Irreversible {
                    "irreversible step requires approval"
                } else {
                    "step requires approval"
                };
                let approval = self.store.request_approval(
                    id,
                    Some(&step_id),
                    reason,
                    &json!({ "next_index": i, "side_effect": step.side_effect }),
                )?;
                self.store.finish_step(id, &step_id, "pending", None, None)?;
                self.store.set_task_state(id, "waiting_approval")?;
                self.log(
                    id,
                    "approval.requested",
                    json!({ "approval_id": approval, "step_id": step_id, "reason": reason }),
                );
                return Ok(RunOutcome {
                    task_id: id.to_string(),
                    state: "waiting_approval".into(),
                    report: None,
                });
            }

            self.store.start_step(id, &step_id)?;
            self.log(id, "step.started", json!({ "step_id": step_id, "goal": step.goal }));

            let mut artifacts: Vec<String> = Vec::new();
            let (report, action_records) = {
                let mut runner = Runner::new(&browser, &session, &mut observer)
                    .with_approved(approved.clone())
                    .with_vision(vision.clone())
                    .with_allowed_hosts(spec.policy.allowed_hosts.clone())
                    .with_task_goal(spec.goal.clone())
                    .with_artifacts_dir(downloads.clone());
                if let Some(flag) = cancel.clone() {
                    runner = runner.with_cancel(flag);
                }
                if let Some(a) = &spec.policy.arbitrate_ambiguous {
                    // Constructed only when the policy asked: with no arbiter
                    // attached, an ambiguous locator fails exactly as before.
                    runner = runner.with_arbiter(std::sync::Arc::new(
                        bh_exec::arbiter::ModelArbiter::new(
                            vision.clone(),
                            spec.goal.clone(),
                            a.min_confidence,
                            a.max_candidates,
                        ),
                    ));
                }
                let report = runner.run_step(&step_id, step, deadline, &mut artifacts).await;
                (report, runner.take_actions())
            };
            // Written after the step returns, not while it runs: the action log
            // is complete and ordered, but a client watching the event stream
            // sees a step's actions arrive as a group. `elapsed_ms` and `index`
            // keep the replay faithful despite the delay.
            for rec in &action_records {
                self.log(
                    id,
                    "action.executed",
                    json!({
                        "step_id": step_id,
                        "index": rec.index,
                        "type": rec.kind,
                        "ok": rec.ok,
                        "elapsed_ms": rec.elapsed_ms,
                        "escalated": rec.escalated,
                        "arbitration": rec.arbitration,
                        "error": rec.error,
                    }),
                );
            }
            self.store.set_model_calls(id, vision.calls())?;

            // Move downloaded artifacts into the task directory and index them.
            let indexed = self.index_artifacts(id, &step_id, artifacts)?;

            // A frame of what the step ended on. Failed steps always get one:
            // it is the picture someone will want after the fact. The rest of
            // the sequence is opt-in (policy.trace_screenshots) because it
            // costs a round-trip and disk per step.
            if spec.policy.trace_screenshots || matches!(report.status, StepStatus::Failed) {
                // `&session` is `Send`; `&self` is not, so the await lives here
                // and only the synchronous bookkeeping takes `self`.
                match session.screenshot().await {
                    Ok(bytes) => self.store_step_shot(id, &step_id, i, bytes),
                    Err(e) => tracing::warn!(step = %step_id, error = %e, "step screenshot failed"),
                }
            }

            let verified = report.verified;
            self.store.finish_step(
                id,
                &step_id,
                match report.status {
                    StepStatus::Succeeded => "succeeded",
                    StepStatus::WaitingApproval => "pending",
                    StepStatus::Failed => "failed",
                    StepStatus::Skipped => "skipped",
                },
                if verified { Some(true) } else { Some(false) },
                report.error.as_deref(),
            )?;
            self.log(
                id,
                "step.finished",
                json!({
                    "step_id": step_id,
                    "status": report.status,
                    "verified": verified,
                    "attempts": report.attempts,
                    "elapsed_ms": report.elapsed_ms,
                    "error": report.error,
                    "repair": report.repair,
                }),
            );
            reports.push(json!({
                "step_id": step_id,
                "status": report.status,
                "verified": verified,
                "attempts": report.attempts,
                "elapsed_ms": report.elapsed_ms,
                "error": report.error,
                "repair": report.repair,
                "artifacts": indexed,
            }));

            if canceled() {
                final_state = "canceled";
                break;
            }

            match report.status {
                StepStatus::Succeeded => {
                    let url = session.url().await.unwrap_or_default();
                    let seq = self.store.save_checkpoint(
                        id,
                        Some(&step_id),
                        &json!({ "next_index": i + 1, "url": url, "completed": i + 1 }),
                    )?;
                    self.log(
                        id,
                        "checkpoint.saved",
                        json!({ "seq": seq, "next_index": i + 1, "url": url }),
                    );
                }
                _ => {
                    final_state = "failed";
                    break;
                }
            }
        }

        if final_state == "succeeded" && Instant::now() > deadline {
            final_state = "failed";
        }

        // Mechanical success says every step did what it declared; it does not
        // say the result is right. This second opinion is opt-in, and its
        // three-way answer is never rounded into a boolean.
        let mut semantic: Option<Value> = None;
        let mut final_error: Option<Value> = None;
        if final_state == "succeeded" {
            if let Some(cfg) = &spec.policy.semantic_verify {
                let goal = cfg.goal(&spec.goal);
                let checked = async {
                    let obs = observer.observe(&session).await?;
                    let state = obs.to_state_text(goal);
                    // Captured after the observation, so the picture matches
                    // the state text the model is given.
                    let image = if cfg.include_screenshot {
                        Some(vision.screenshot_image(&session).await?)
                    } else {
                        None
                    };
                    let report = vision
                        .semantic_verify(goal, &state, cfg.threshold, image.clone())
                        .await?;
                    Ok::<_, BhError>((report, image))
                }
                .await;
                self.store.set_model_calls(id, vision.calls())?;
                match checked {
                    Ok((report, image)) => {
                        let mut value = serde_json::to_value(&report).unwrap_or(Value::Null);
                        if let Some(image) = &image {
                            value["screenshot"] = self.store_verify_shot(id, image);
                        }
                        self.log(id, "task.verified", value.clone());
                        semantic = Some(value);
                        match report.verdict {
                            bh_model::Verdict::Achieved => {}
                            bh_model::Verdict::NotAchieved => {
                                final_state = "failed";
                                final_error = Some(json!({
                                    "message": "semantic verification did not confirm the goal",
                                    "verdict": "not_achieved",
                                }));
                            }
                            bh_model::Verdict::Uncertain => match cfg.on_uncertain {
                                OnUncertain::Continue => {}
                                OnUncertain::Fail => {
                                    final_state = "failed";
                                    final_error = Some(json!({
                                        "message": "semantic verification was uncertain and policy says fail",
                                        "verdict": "uncertain",
                                    }));
                                }
                                OnUncertain::WaitHuman => {
                                    let reason = "semantic verification is uncertain; a human must confirm the result";
                                    let approval = self.store.request_approval(
                                        id,
                                        None,
                                        reason,
                                        &json!({
                                            "next_index": spec.steps.len(),
                                            "semantic": semantic,
                                        }),
                                    )?;
                                    self.store.set_task_state(id, "waiting_approval")?;
                                    self.log(
                                        id,
                                        "approval.requested",
                                        json!({
                                            "approval_id": approval,
                                            "step_id": Value::Null,
                                            "reason": reason,
                                            "semantic": semantic,
                                        }),
                                    );
                                    return Ok(RunOutcome {
                                        task_id: id.to_string(),
                                        state: "waiting_approval".into(),
                                        report: None,
                                    });
                                }
                            },
                        }
                    }
                    Err(e) => {
                        // A configured check that cannot run is a failure, not
                        // an implicit pass: otherwise a missing model silently
                        // turns semantic verification off.
                        self.log(id, "task.verify_error", json!({ "error": e.to_string() }));
                        final_state = "failed";
                        final_error = Some(json!({
                            "message": format!("semantic verification failed: {e}"),
                        }));
                    }
                }
            }
        }

        let mut summary = json!({
            "task_id": id,
            "state": final_state,
            "steps": reports,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        if let Some(semantic) = &semantic {
            summary["semantic"] = semantic.clone();
        }
        self.finish(id, final_state, Some(&summary), final_error.as_ref())?;
        self.log(id, "task.finished", summary.clone());

        Ok(RunOutcome {
            task_id: id.to_string(),
            state: final_state.to_string(),
            report: Some(summary),
        })
    }

    // -------------------------------------------------------------- planning

    /// Plan an empty workflow against the page the task will actually see.
    ///
    /// The proposal gets the same model-side schema validation as `bhx plan`,
    /// plus task-specific checks that only the engine knows: every navigation
    /// must stay inside `allowed_hosts`, and every `${cred:name}` must name a
    /// credential the caller supplied. A proposal is not executed until it has
    /// been persisted, so a crash never leaves a half-planned task.
    async fn plan_workflow(
        &mut self,
        id: &str,
        spec: &TaskSpec,
        session: &bh_browser::Session,
        observer: &mut Observer,
        vision: &Vision,
    ) -> Result<TaskSpec> {
        if !vision.router().is_configured() {
            return Err(BhError::Other(
                "planning_unavailable: no model adapter configured for Plan".into(),
            ));
        }
        if let Some(url) = &spec.start_url {
            if !spec.policy.host_allowed(url) {
                return Err(BhError::Other(format!(
                    "planning start_url {url} blocked: host not in allowed_hosts"
                )));
            }
            session.navigate(url).await?;
        }
        self.log(
            id,
            "plan.requested",
            json!({ "goal": spec.goal, "start_url": spec.start_url }),
        );

        let obs = observer.observe(session).await?;
        let mut state = obs.to_state_text("");
        if obs.truncated {
            state.push_str(&format!("(truncated at {} elements)\n", obs.elements.len()));
        }
        let proposal =
            vision.router().propose(&bh_model::ModelRequest::plan(&spec.goal, &state)).await?;
        let Some(steps) = proposal.steps else {
            return Err(BhError::Other("auto planning produced no steps".into()));
        };
        let mut planned = spec.clone();
        planned.steps = steps;
        let planned = normalize(planned)?;
        validate_planned_steps(&planned)?;

        let summary: Vec<Value> = planned
            .steps
            .iter()
            .map(|s| json!({ "id": s.id, "goal": s.goal }))
            .collect();
        self.log(
            id,
            "plan.created",
            json!({
                "model": proposal.model,
                "confidence": proposal.confidence,
                "reason": proposal.reason,
                "steps": summary,
            }),
        );
        Ok(planned)
    }

    // -------------------------------------------------------------- helpers

    /// Interactive tasks share the browser and the store with workflow tasks,
    /// but not the step machine: there are no steps to checkpoint.
    async fn execute_controller(
        &mut self,
        id: &str,
        spec: &TaskSpec,
        session: &bh_browser::Session,
        vision: &std::sync::Arc<bh_exec::vision::Vision>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        updates: Option<tokio::sync::watch::Receiver<bh_core::ControllerPolicy>>,
        started: Instant,
    ) -> Result<RunOutcome> {
        let policy = spec
            .controller
            .as_ref()
            .ok_or_else(|| BhError::Other("interactive task has no controller policy".into()))?;

        // A policy that asks for decisions needs the model up **before the page
        // starts moving**: navigating first and warming on the first question
        // means the world's opening seconds run with no strategy at all, which
        // is not a slow decision but a missed one. Measured on the plane task:
        // a 4.4 s sidecar load paid after navigation put the first accepted
        // strategy at 5.48 s, past a decision window that closed at 5.22 s,
        // and the task lost three runs in a row for that reason alone.
        if policy.decide.is_some() {
            let failures = vision.warm_for_decisions().await;
            if !failures.is_empty() {
                tracing::warn!(
                    models = %Self::failers(&failures),
                    "decision models could not be warmed; the loop will still run and ask"
                );
            }
        }

        if let Some(url) = &policy.start_url {
            if !spec.policy.host_allowed(url) {
                return self.fail_controller(
                    id,
                    started,
                    format!("navigation to {url} blocked: host not in allowed_hosts"),
                );
            }
            if let Err(e) = session.navigate(url).await {
                return self.fail_controller(id, started, e.to_string());
            }
        }

        self.store.set_task_state(id, "running")?;
        let mut decision_handle: Option<(
            tokio::task::JoinHandle<()>,
            tokio::sync::watch::Sender<bool>,
        )> = None;
        self.log(
            id,
            "interactive.started",
            json!({
                "policy_version": policy.policy_version,
                "tick_hz": policy.tick_hz,
                "rules": policy.rules.len(),
            }),
        );

        // The sink takes `&mut self` on purpose: a shared `&Engine` held across
        // an await would make the whole task future require `Engine: Sync`,
        // and rusqlite's Connection is Send but not Sync.
        let mut sink = |t: &TickSummary| {
            self.log_mut(id, "controller.tick_summary", serde_json::to_value(t).unwrap_or(Value::Null));
        };
        // A model that picks strategies runs beside the loop, never inside it:
        // the measured cost of one local decision (150–500 ms) is more than
        // ten ticks, so asking in-line would stop the controller. It also only
        // starts when the policy asks: a plain rule loop spends no calls and
        // runs no extra tasks.
        let decision_rx = if policy.decide.is_some() {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let session2 = session.clone();
            // With no live-update channel there is nothing to watch, but the
            // asker still needs the sender to stay alive: a dropped sender
            // makes `changed()` fail and would end the task after one round.
            let (policy_tx, policy_rx) = match &updates {
                Some(rx) => (None, rx.clone()),
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(policy.clone());
                    (Some(tx), rx)
                }
            };
            let vision = vision.clone();
            let handle = tokio::spawn(async move {
                let _keep = policy_tx;
                controller::decision_task(&session2, policy_rx, stop_rx, vision, tx).await;
            });
            decision_handle = Some((handle, stop_tx));
            Some(rx)
        } else {
            None
        };

        let outcome =
            controller::run_with_updates(session, policy.clone(), updates, cancel, &mut sink, decision_rx)
                .await;
        if let Some((handle, stop_tx)) = decision_handle.take() {
            let _ = stop_tx.send(true);
            // The asker polls between rounds, so it stops within one interval;
            // a stuck model call is waited on rather than aborted, because the
            // call itself may still hold the sidecar.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        }

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => return self.fail_controller(id, started, e.to_string()),
        };

        let state = match outcome.why {
            StopReason::GoalMet => "succeeded",
            // A terminal rule is the author saying "that is the end". Without a
            // goal predicate there is nothing else to check; with one, the goal
            // still has to hold.
            StopReason::TerminalRule => {
                if policy.goal.script.trim().is_empty() {
                    "succeeded"
                } else {
                    "failed"
                }
            }
            StopReason::Canceled => "canceled",
            StopReason::Deadline | StopReason::Anomaly => "failed",
        };

        // Interactive tasks spend the same budget as workflow ones, so the
        // count has to be written back here too: it was left at 0 while the
        // stop event reported accepted decisions, which reads as "the model
        // cost nothing" to anyone reading `task ls` or `stats`.
        self.store.set_model_calls(id, vision.calls())?;

        let summary = json!({
            "task_id": id,
            "state": state,
            "controller": serde_json::to_value(&outcome).unwrap_or(Value::Null),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        self.log(
            id,
            "interactive.stopped",
            json!({
                "why": outcome.why,
                "ticks": outcome.ticks,
                "samples": outcome.samples,
                "actions": outcome.actions,
                "evals": outcome.evals,
                "tick_ms_p50": outcome.tick_ms_p50,
                "tick_ms_p95": outcome.tick_ms_p95,
                "tick_ms_max": outcome.tick_ms_max,
                "elapsed_ms": outcome.elapsed_ms,
                "policy_version": outcome.policy_version,
                "decisions": outcome.decisions,
                "decision_log": outcome.decision_log,
                "rules": outcome.rules,
                "error": outcome.error,
            }),
        );
        self.finish(id, state, Some(&summary), None)?;
        self.log(id, "task.finished", summary.clone());

        Ok(RunOutcome {
            task_id: id.to_string(),
            state: state.to_string(),
            report: Some(summary),
        })
    }

    /// Join model names for a log field; the failures themselves are already
    /// in the message.
    fn failers(items: &[String]) -> String {
        items.join("; ")
    }

    fn fail_controller(
        &self,
        id: &str,
        started: Instant,
        error: String,
    ) -> Result<RunOutcome> {
        let summary = json!({
            "task_id": id,
            "state": "failed",
            "controller": { "why": "anomaly", "error": error },
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        self.finish(id, "failed", Some(&summary), Some(&json!({ "message": error })))?;
        self.log(id, "task.finished", summary.clone());
        Ok(RunOutcome {
            task_id: id.to_string(),
            state: "failed".into(),
            report: Some(summary),
        })
    }

    fn log_mut(&mut self, task_id: &str, kind: &str, data: Value) {
        self.log(task_id, kind, data);
    }

    fn finish(
        &self,
        id: &str,
        state: &str,
        result: Option<&Value>,
        error: Option<&Value>,
    ) -> Result<()> {
        self.store.finish_task(id, state, result, error)
    }

    pub fn event(&self, task_id: &str, kind: &str, data: Value) -> Result<i64> {
        let seq = self.store.append_event(task_id, kind, &data)?;
        let line = json!({ "seq": seq, "ts": store::now(), "type": kind, "data": data });
        let path = self.task_dir(task_id).join("events.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        Ok(seq)
    }

    /// Events are diagnostics. Losing one is worth a warning, never a failed
    /// task, so the engine's own call sites go through here.
    fn log(&self, task_id: &str, kind: &str, data: Value) {
        if let Err(e) = self.event(task_id, kind, data) {
            tracing::warn!(error = %e, task_id, kind, "could not append event");
        }
    }

    /// One JPEG per step, stored like any other artifact so `trace --out`
    /// copies it into the bundle and the player can show it. Never fails the
    /// task: a browser that cannot screenshot is a missing picture, not a
    /// failed step.
    ///
    /// Takes the bytes, not the session: capturing is the only `await`, and
    /// it happens in `execute` before this call. Keeping `&Engine` out of that
    /// future is what keeps task futures `Send` for `serve` and MCP.
    /// Writes the exact PNG the verifier was shown, so the judgement can be
    /// audited later. Best effort like step screenshots: a failed write is
    /// logged and never changes the verdict.
    fn store_verify_shot(&self, task_id: &str, image: &bh_model::ImageData) -> Value {
        let digest = hex_sha256(&image.bytes);
        let mut info = json!({
            "name": "verify-final.png",
            "bytes": image.bytes.len(),
            "width": image.width,
            "height": image.height,
            "sha256": digest,
        });
        let dest = self.task_dir(task_id).join("artifacts");
        let path = dest.join("verify-final.png");
        let written = std::fs::create_dir_all(&dest)
            .and_then(|()| std::fs::write(&path, &image.bytes));
        if let Err(e) = written {
            tracing::warn!(error = %e, "could not write the verification screenshot");
            info["error"] = json!(e.to_string());
            return info;
        }
        if let Err(e) = self.store.add_artifact(
            task_id,
            "screenshot",
            Some("verify-final.png"),
            &path.to_string_lossy(),
            image.bytes.len() as i64,
            Some(&digest),
        ) {
            tracing::warn!(error = %e, "could not index the verification screenshot");
        }
        info["path"] = json!(path.to_string_lossy());
        let _ = self.log(task_id, "task.verify.screenshot", info.clone());
        info
    }

    fn store_step_shot(&self, task_id: &str, step_id: &str, index: usize, bytes: Vec<u8>) {
        let dest = self.task_dir(task_id).join("artifacts");
        if let Err(e) = std::fs::create_dir_all(&dest) {
            tracing::warn!(error = %e, "could not create the artifact directory");
            return;
        }
        let safe: String = step_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let name = format!("step-{:02}-{safe}.jpg", index + 1);
        let path = dest.join(&name);
        if let Err(e) = std::fs::write(&path, &bytes) {
            tracing::warn!(error = %e, "could not write the step screenshot");
            return;
        }
        let digest = hex_sha256(&bytes);
        if let Err(e) = self.store.add_artifact(
            task_id,
            "screenshot",
            Some(&name),
            &path.to_string_lossy(),
            bytes.len() as i64,
            Some(&digest),
        ) {
            tracing::warn!(error = %e, "could not index the step screenshot");
        }
        let _ = self.log(
            task_id,
            "step.screenshot",
            json!({
                "step_id": step_id,
                "name": name,
                "path": path.to_string_lossy(),
                "bytes": bytes.len(),
                "sha256": digest,
            }),
        );
    }

    fn index_artifacts(
        &self,
        task_id: &str,
        step_id: &str,
        paths: Vec<String>,
    ) -> Result<Vec<String>> {
        let dest = self.task_dir(task_id).join("artifacts");
        let mut out = Vec::new();
        for path in paths {
            let src = PathBuf::from(&path);
            let name = src
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("artifact")
                .to_string();
            let mut target = dest.join(&name);
            let mut n = 1;
            while target.exists() {
                target = dest.join(format!("{step_id}-{n}-{name}"));
                n += 1;
            }
            let bytes = std::fs::read(&src).unwrap_or_default();
            let digest = hex_sha256(&bytes);
            std::fs::rename(&src, &target).or_else(|_| {
                std::fs::copy(&src, &target).map(|_| ()).and_then(|_| std::fs::remove_file(&src))
            })
            .map_err(|e| BhError::Other(format!("move artifact {name}: {e}")))?;
            let kind = if name.ends_with(".json") { "extract" } else { "download" };
            self.store.add_artifact(
                task_id,
                kind,
                Some(&name),
                &target.to_string_lossy(),
                bytes.len() as i64,
                Some(&digest),
            )?;
            out.push(target.to_string_lossy().to_string());
        }
        Ok(out)
    }

    pub fn task_dir(&self, task_id: &str) -> PathBuf {
        self.home.join("tasks").join(task_id)
    }
}

/// One process per task directory. A lock left behind by a killed process is
/// stale rather than authoritative: the pid decides.
pub struct TaskLock {
    path: PathBuf,
}

impl TaskLock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| BhError::Other(format!("create {}: {e}", dir.display())))?;
        let path = dir.join(".lock");
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok());
                if holder.is_some_and(process_alive) {
                    return Err(BhError::Other(format!(
                        "task is locked by pid {} ({}); if it is not running, delete the lock file",
                        holder.unwrap(),
                        path.display()
                    )));
                }
                let _ = std::fs::remove_file(&path);
                Self::acquire(dir)
            }
            Err(e) => Err(BhError::Other(format!("lock {}: {e}", path.display()))),
        }
    }
}

impl Drop for TaskLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Chrome children outlive `kill -9` of the harness. They hold the profile's
/// singleton lock, so a resume would be forwarded to a browser we cannot drive.
fn reap_profile_browsers(profile: &Path) {
    let pattern = profile.to_string_lossy().to_string();
    let Ok(out) = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(&pattern)
        .output()
    else {
        return;
    };
    let mut killed = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status();
        killed += 1;
    }
    if killed > 0 {
        tracing::warn!(pid_count = killed, profile = %profile.display(), "killed browsers left over from a previous run");
    }
}

/// Fill in step ids and reject duplicates: approvals and checkpoints key on
/// the step id, so it has to exist before anything runs.
fn normalize(mut spec: TaskSpec) -> Result<TaskSpec> {
    if let Some(cfg) = &mut spec.policy.semantic_verify {
        if !cfg.threshold.is_finite() || !(0.0..=1.0).contains(&cfg.threshold) {
            return Err(BhError::Other(format!(
                "policy.semantic_verify.threshold {} is outside 0..1",
                cfg.threshold
            )));
        }
        if cfg.goal.as_deref().is_some_and(|g| g.trim().is_empty()) {
            cfg.goal = None;
        }
    }
    let mut seen = HashSet::new();
    for (i, step) in spec.steps.iter_mut().enumerate() {
        if step.id.is_empty() {
            step.id = format!("s{}", i + 1);
        }
        if !seen.insert(step.id.clone()) {
            return Err(BhError::Other(format!("duplicate step id {:?}", step.id)));
        }
    }
    Ok(spec)
}

fn validate_planned_steps(spec: &TaskSpec) -> Result<()> {
    for step in &spec.steps {
        for action in &step.actions {
            match action {
                Action::Navigate { url } if !spec.policy.host_allowed(url) => {
                    return Err(BhError::Other(format!(
                        "planning rejected: step {:?} navigates to {url}, outside allowed_hosts",
                        step.id
                    )));
                }
                Action::Type { text, .. } => {
                    // The planner may only reference credentials the caller
                    // actually supplied. Typing `${cred:typo}` literally is
                    // not a secret leak, but it is a silent mis-execution.
                    let mut rest = text.as_str();
                    while let Some(at) = rest.find("${cred:") {
                        let after = &rest[at + "${cred:".len()..];
                        let Some(end) = after.find('}') else {
                            return Err(BhError::Other(format!(
                                "planning rejected: step {:?} has an unterminated credential placeholder",
                                step.id
                            )));
                        };
                        let name = &after[..end];
                        if !spec.credentials.contains_key(name) {
                            return Err(BhError::Other(format!(
                                "planning rejected: step {:?} uses credential {name:?}, which the task does not hold",
                                step.id
                            )));
                        }
                        rest = &after[end + 1..];
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// What goes to `task.json`: no credentials, and any literal value of a
/// credential is scrubbed from action text as well.
fn spec_for_disk(spec: &TaskSpec) -> Value {
    let mut v = serde_json::to_value(spec).unwrap_or(Value::Null);
    let secrets: Vec<String> = spec.credentials.values().cloned().collect();
    v["credentials"] = Value::Object(Default::default());
    scrub(&mut v, &secrets);
    v
}

fn scrub(v: &mut Value, secrets: &[String]) {
    match v {
        Value::String(s) => {
            for secret in secrets {
                if !secret.is_empty() && s.contains(secret.as_str()) {
                    *s = s.replace(secret.as_str(), "[REDACTED]");
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| scrub(x, secrets)),
        Value::Object(o) => {
            o.iter_mut().for_each(|(_, x)| scrub(x, secrets));
        }
        _ => {}
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Step payloads are stored as JSON; this keeps `Step` and the store in sync.
#[allow(dead_code)]
fn step_json(step: &Step) -> BTreeMap<String, Value> {
    serde_json::to_value(step)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// Actions that touch credentials are only allowed through the in-memory spec.
pub fn is_secret_action(action: &Action) -> bool {
    matches!(action, Action::Type { text, .. } if text.contains("${cred:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_scrubbed_from_disk_json() {
        let mut spec = TaskSpec::default();
        spec.goal = "login".into();
        spec.credentials.insert("pass".into(), "hunter2".into());
        spec.steps.push(Step {
            id: "s1".into(),
            actions: vec![Action::Type {
                target: bh_core::Target::Locator(bh_core::Locator::label("密码")),
                text: "${cred:pass}".into(),
                clear_first: true,
            }],
            ..Default::default()
        });
        let resolved = spec.resolve_credentials();
        assert_eq!(
            match &resolved.steps[0].actions[0] {
                Action::Type { text, .. } => text.clone(),
                _ => unreachable!(),
            },
            "hunter2"
        );
        let disk = spec_for_disk(&spec);
        let text = disk.to_string();
        assert!(!text.contains("hunter2"), "credential leaked: {text}");
        assert!(disk["credentials"].as_object().unwrap().is_empty());
    }

    #[test]
    fn check_spec_answers_before_any_work_starts() {
        use bh_core::{ControllerObserve, ControllerPolicy, TaskKind};

        let mut interactive = TaskSpec {
            kind: TaskKind::Interactive,
            ..Default::default()
        };
        assert!(check_spec(&interactive).is_err(), "no controller");

        interactive.controller = Some(ControllerPolicy {
            start_url: Some("http://127.0.0.1/".into()),
            observe: ControllerObserve { script: "() => ({})".into(), ..Default::default() },
            ..Default::default()
        });
        assert!(check_spec(&interactive).is_ok());

        let mut no_script = interactive.clone();
        no_script.controller.as_mut().unwrap().observe.script = "   ".into();
        assert!(check_spec(&no_script).is_err(), "empty observe script");

        let mut verified = interactive;
        verified.policy.semantic_verify = Some(Default::default());
        assert!(check_spec(&verified).is_err(), "semantic_verify is workflow-only");
    }

    #[test]
    fn normalize_assigns_and_rejects_ids() {
        let mut spec = TaskSpec::default();
        spec.steps.push(Step::default());
        spec.steps.push(Step::default());
        let n = normalize(spec.clone()).unwrap();
        assert_eq!(n.steps[0].id, "s1");
        assert_eq!(n.steps[1].id, "s2");

        let mut dup = spec;
        dup.steps[0].id = "a".into();
        dup.steps[1].id = "a".into();
        assert!(normalize(dup).is_err());
    }

    #[test]
    fn a_planned_navigation_must_stay_inside_allowed_hosts() {
        let mut spec = TaskSpec::default();
        spec.policy.allowed_hosts = vec!["127.0.0.1".into()];
        spec.steps.push(Step {
            id: "s1".into(),
            actions: vec![Action::Navigate { url: "http://evil.example/".into() }],
            ..Default::default()
        });
        let err = validate_planned_steps(&spec).unwrap_err().to_string();
        assert!(err.contains("outside allowed_hosts"), "{err}");

        spec.steps[0].actions =
            vec![Action::Navigate { url: "http://127.0.0.1:8766/".into() }];
        assert!(validate_planned_steps(&spec).is_ok());
    }

    #[test]
    fn a_planned_step_may_only_reference_supplied_credentials() {
        let text = |value: &str| {
            vec![Action::Type {
                target: bh_core::Target::Locator(bh_core::Locator::label("密码")),
                text: value.into(),
                clear_first: true,
            }]
        };
        let mut spec = TaskSpec::default();
        spec.credentials.insert("password".into(), "hunter2".into());
        spec.steps.push(Step { id: "s1".into(), actions: text("${cred:password}"), ..Default::default() });
        assert!(validate_planned_steps(&spec).is_ok());

        spec.steps[0].actions = text("${cred:typo}");
        let err = validate_planned_steps(&spec).unwrap_err().to_string();
        assert!(err.contains("does not hold"), "{err}");

        spec.steps[0].actions = text("${cred:password");
        assert!(validate_planned_steps(&spec).is_err());
    }
}
