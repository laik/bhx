//! Step runner: executes a task's steps in order, with preconditions, success
//! conditions, retries and approval gates.
//!
//! It deliberately owns no persistence and no task state machine — those belong
//! to `bh-task` (Phase 2). What it does own is the honesty rule: a step is
//! `verified` only when a success condition was supplied *and* observed.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

pub mod arbiter;
pub mod controller;
pub mod vision;

use bh_action::Runtime;
use bh_browser::{Browser, DownloadGuard, Session};
use bh_core::{
    Action, BhError, Condition, ExecutorKind, OnExhausted, Policy, Result, SideEffect, Step,
    Target, TaskSpec,
};
use bh_observe::Observer;
use crate::vision::Vision;
use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Succeeded,
    Failed,
    WaitingApproval,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Succeeded,
    Failed,
    WaitingApproval,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub id: String,
    pub goal: String,
    pub status: StepStatus,
    /// Never inferred from `status`: an unverified success is reported as such.
    pub verified: bool,
    pub attempts: u32,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Present only when `retry.on_exhausted=ask_model` actually produced and
    /// ran a replacement. Kept in the report so a repaired success is never
    /// indistinguishable from the original plan working.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskReport {
    pub goal: String,
    pub status: TaskStatus,
    pub verified: bool,
    pub elapsed_ms: u64,
    pub steps: Vec<StepReport>,
    pub artifacts: Vec<String>,
}

/// One dispatched action, as the engine records it. Deliberately narrow: the
/// action's payload is not here, because a `type` action's text may hold a
/// credential value after materialization. Type, index, outcome and timing are
/// enough for a trace and for the failed-action rate.
#[derive(Debug, Clone, Serialize)]
pub struct ActionRecord {
    pub index: usize,
    pub kind: &'static str,
    pub ok: bool,
    pub elapsed_ms: u64,
    /// True when the first attempt failed and a visual fallback was used.
    pub escalated: bool,
    /// Set when a model chose this action's target out of several matches.
    /// Without it a guessed click is indistinguishable from a named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arbitration: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Click { .. } => "click",
        Action::Hover { .. } => "hover",
        Action::Type { .. } => "type",
        Action::Press { .. } => "press",
        Action::KeyDown { .. } => "key_down",
        Action::KeyUp { .. } => "key_up",
        Action::Scroll { .. } => "scroll",
        Action::Drag { .. } => "drag",
        Action::Navigate { .. } => "navigate",
        Action::WaitFor { .. } => "wait_for",
        Action::Screenshot { .. } => "screenshot",
        Action::Evaluate { .. } => "evaluate",
        Action::Extract { .. } => "extract",
    }
}

impl TaskReport {
    pub fn succeeded(&self) -> bool {
        self.status == TaskStatus::Succeeded
    }
}

pub struct Runner<'a> {
    pub browser: &'a Browser,
    pub session: &'a Session,
    pub rt: Runtime<'a>,
    cancel: Option<Arc<AtomicBool>>,
    approved: HashSet<String>,
    /// Where `extract` actions write their JSON. The task engine points this at
    /// the task's download dir so artifacts are indexed like any other.
    artifacts_dir: Option<PathBuf>,
    /// Present when the task may use visual grounding. The router inside it is
    /// shared across steps so the model-call budget is per task, not per step.
    vision: Option<Arc<Vision>>,
    /// Hosts a repaired step may navigate to. Empty means unrestricted, the
    /// same convention as `Policy::allowed_hosts`.
    allowed_hosts: Vec<String>,
    /// Task-level goal given to the repair model. Empty falls back to the
    /// failed step's goal.
    task_goal: String,
    /// Dispatched actions since the last `take_actions`. The engine drains this
    /// after each step, so a step's retries and repair attempt are included.
    actions: Vec<ActionRecord>,
}

impl<'a> Runner<'a> {
    pub fn new(browser: &'a Browser, session: &'a Session, observer: &'a mut Observer) -> Self {
        Self {
            browser,
            session,
            rt: Runtime::new(session, observer),
            cancel: None,
            approved: HashSet::new(),
            artifacts_dir: None,
            vision: None,
            allowed_hosts: Vec::new(),
            task_goal: String::new(),
            actions: Vec::new(),
        }
    }

    pub fn with_vision(mut self, vision: Arc<Vision>) -> Self {
        self.vision = Some(vision);
        self
    }

    /// Let a model choose when a locator matches several elements. Only set
    /// this when the task asked for it (`policy.arbitrate_ambiguous`).
    pub fn with_arbiter(mut self, arbiter: Arc<dyn bh_action::LocatorArbiter>) -> Self {
        self.rt = self.rt.with_arbiter(arbiter);
        self
    }

    pub fn with_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allowed_hosts = hosts;
        self
    }

    pub fn with_task_goal(mut self, goal: impl Into<String>) -> Self {
        self.task_goal = goal.into();
        self
    }

    /// Hand the recorded actions to the caller. The runner keeps no history
    /// across steps; this is the only way the engine sees them.
    pub fn take_actions(&mut self) -> Vec<ActionRecord> {
        std::mem::take(&mut self.actions)
    }

    pub fn model_calls(&self) -> u32 {
        self.vision.as_ref().map_or(0, |v| v.calls())
    }

    pub fn with_artifacts_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.artifacts_dir = Some(dir.into());
        self
    }

    /// Steps a human has already approved. Approval lives in the task engine;
    /// the runner only needs to know it must not stop again on those.
    pub fn with_approved(mut self, steps: Vec<String>) -> Self {
        self.approved = steps.into_iter().collect();
        self
    }

    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.rt = self.rt.with_cancel(flag.clone());
        self.cancel = Some(flag);
        self
    }

    fn canceled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }

    pub async fn run(&mut self, spec: &TaskSpec) -> TaskReport {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(spec.policy.max_wall_ms.max(1_000));
        let mut steps = Vec::new();
        let mut artifacts = Vec::new();
        let mut status = TaskStatus::Succeeded;

        for (i, step) in spec.steps.iter().enumerate() {
            if self.canceled() {
                let _ = self.session.release_all_input().await;
                status = TaskStatus::Failed;
                break;
            }
            let id = if step.id.is_empty() {
                format!("s{}", i + 1)
            } else {
                step.id.clone()
            };

            // Approval gate comes first: nothing of an unapproved step runs,
            // not even its preconditions.
            if step.needs_approval(&spec.policy) && !self.approved.contains(&id) {
                steps.push(StepReport {
                    id,
                    goal: step.goal.clone(),
                    status: StepStatus::WaitingApproval,
                    verified: false,
                    attempts: 0,
                    elapsed_ms: 0,
                    error: Some(match step.side_effect {
                        SideEffect::Irreversible => {
                            "irreversible step requires approval (policy.allow_irreversible=false)".into()
                        }
                        _ => "step.requires_approval=true".into(),
                    }),
                    repair: None,
                });
                status = TaskStatus::WaitingApproval;
                break;
            }

            if let Err(e) = self.guard_hosts(step, &spec.policy).await {
                steps.push(StepReport {
                    id,
                    goal: step.goal.clone(),
                    status: StepStatus::Failed,
                    verified: false,
                    attempts: 0,
                    elapsed_ms: 0,
                    error: Some(e.to_string()),
                    repair: None,
                });
                // A host violation is a policy breach: stop the whole task.
                status = TaskStatus::Failed;
                break;
            }

            let report = self.run_step(&id, step, deadline, &mut artifacts).await;
            let failed = report.status == StepStatus::Failed;
            steps.push(report);

            if failed {
                status = TaskStatus::Failed;
                break;
            }
            if Instant::now() > deadline {
                status = TaskStatus::Failed;
                break;
            }
        }

        TaskReport {
            goal: spec.goal.clone(),
            status,
            verified: status == TaskStatus::Succeeded && steps.iter().all(|s| s.verified),
            elapsed_ms: started.elapsed().as_millis() as u64,
            steps,
            artifacts,
        }
    }

    async fn guard_hosts(&self, step: &Step, policy: &Policy) -> Result<()> {
        for action in &step.actions {
            if let Action::Navigate { url } = action {
                if !policy.host_allowed(url) {
                    return Err(BhError::Action(format!(
                        "navigation to {url} blocked: host not in allowed_hosts"
                    )));
                }
            }
        }
        Ok(())
    }

    pub async fn run_step(
        &mut self,
        id: &str,
        step: &Step,
        deadline: Instant,
        artifacts: &mut Vec<String>,
    ) -> StepReport {
        let started = Instant::now();
        let attempts_allowed = step.retry.max_attempts.max(1);
        let mut last_error: Option<String> = None;
        let mut attempts = 0;

        while attempts < attempts_allowed {
            attempts += 1;
            match self.attempt(id, step, deadline, artifacts).await {
                Ok(verified) => {
                    return StepReport {
                        id: id.to_string(),
                        goal: step.goal.clone(),
                        status: StepStatus::Succeeded,
                        verified,
                        attempts,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        error: None,
                        repair: None,
                    };
                }
                Err(e) => {
                    tracing::warn!(step = id, attempt = attempts, error = %e, "step attempt failed");
                    last_error = Some(e.to_string());
                    if attempts < attempts_allowed {
                        tokio::time::sleep(Duration::from_millis(step.retry.backoff_ms)).await;
                    }
                }
            }
        }

        // `ask_model` is the explicit opt-in for a replacement step. The runner
        // still enforces the safety envelope before dispatching anything.
        if step.retry.on_exhausted == OnExhausted::AskModel {
            match self
                .repair_and_run(id, step, last_error.as_deref().unwrap_or("step failed"), deadline, artifacts, attempts, started)
                .await
            {
                Some(Ok(report)) => return report,
                Some(Err(e)) => {
                    tracing::warn!(step = id, error = %e, "repair proposal was not usable");
                    last_error = Some(format!(
                        "{}; repair failed: {e}",
                        last_error.as_deref().unwrap_or("step failed")
                    ));
                }
                None => {}
            }
        }

        StepReport {
            id: id.to_string(),
            goal: step.goal.clone(),
            status: StepStatus::Failed,
            verified: false,
            attempts,
            elapsed_ms: started.elapsed().as_millis() as u64,
            error: last_error,
            repair: None,
        }
    }

    /// Ask the model for a corrected step and run it once.
    ///
    /// Only read-only steps are auto-repaired. A failed step may already have
    /// had its side effect; re-dispatching a model-written version of a
    /// reversible/irreversible action is exactly the duplicate-submit failure
    /// mode the approval gate exists to prevent. The replacement must keep the
    /// original id, may not raise `side_effect`, may not drop an approval
    /// requirement, and may not navigate outside `allowed_hosts`.
    async fn repair_and_run(
        &mut self,
        id: &str,
        step: &Step,
        failure: &str,
        deadline: Instant,
        artifacts: &mut Vec<String>,
        attempts: u32,
        started: Instant,
    ) -> Option<Result<StepReport>> {
        if step.side_effect != SideEffect::ReadOnly || step.requires_approval {
            tracing::warn!(
                step = id,
                side_effect = ?step.side_effect,
                "ask_model requested but only read-only steps may be auto-repaired"
            );
            return None;
        }
        if Instant::now() >= deadline {
            return None;
        }
        let Some(vision) = self.vision.clone() else {
            return Some(Err(BhError::Other(
                "repair was requested but no model adapter is configured".into(),
            )));
        };
        let goal = if self.task_goal.trim().is_empty() {
            step.goal.clone()
        } else {
            self.task_goal.clone()
        };
        let observation = match self.rt.observer.observe(self.session).await {
            Ok(o) => o,
            Err(e) => return Some(Err(e)),
        };
        let state = observation.to_state_text(&goal);
        let proposal = match vision.repair(&goal, &state, step, failure).await {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        if let Err(e) = self.validate_repair(step, &proposal.step) {
            return Some(Err(e));
        }
        tracing::warn!(
            step = id,
            model = %proposal.model,
            confidence = proposal.confidence,
            "running a model-repaired read-only step"
        );

        let mut info = serde_json::json!({
            "model": proposal.model,
            "confidence": proposal.confidence,
            "reason": proposal.reason,
            "step": proposal.step,
        });
        match self.attempt(id, &proposal.step, deadline, artifacts).await {
            Ok(verified) => {
                info["status"] = serde_json::json!("succeeded");
                info["verified"] = serde_json::json!(verified);
                Some(Ok(StepReport {
                    id: id.to_string(),
                    goal: step.goal.clone(),
                    status: StepStatus::Succeeded,
                    verified,
                    attempts: attempts + 1,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    error: None,
                    repair: Some(info),
                }))
            }
            Err(e) => {
                info["status"] = serde_json::json!("failed");
                info["error"] = serde_json::json!(e.to_string());
                Some(Ok(StepReport {
                    id: id.to_string(),
                    goal: step.goal.clone(),
                    status: StepStatus::Failed,
                    verified: false,
                    attempts: attempts + 1,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    error: Some(format!("{failure}; repaired step failed: {e}")),
                    repair: Some(info),
                }))
            }
        }
    }

    fn validate_repair(&self, original: &Step, proposed: &Step) -> Result<()> {
        if proposed.id != original.id {
            return Err(BhError::Other(format!(
                "repair changed the step id ({:?} -> {:?})",
                original.id, proposed.id
            )));
        }
        if proposed.side_effect > original.side_effect {
            return Err(BhError::Other(format!(
                "repair raised side_effect ({:?} -> {:?})",
                original.side_effect, proposed.side_effect
            )));
        }
        if original.requires_approval && !proposed.requires_approval {
            return Err(BhError::Other(
                "repair removed the step's approval requirement".into(),
            ));
        }
        // The step author owns the execution mode and the visual fallback. A
        // repair may fix a locator, not opt the step into a different executor
        // or a model-driven visual retry.
        if proposed.executor != original.executor {
            return Err(BhError::Other(format!(
                "repair changed the executor ({:?} -> {:?})",
                original.executor, proposed.executor
            )));
        }
        if proposed.fallback_visual != original.fallback_visual {
            return Err(BhError::Other(
                "repair changed the step's visual fallback".into(),
            ));
        }
        // Conditions are part of the step contract, not just a success gate:
        // `Js` conditions run in the page's main world. Allow a repair to fix
        // actions; do not let it rewrite what counts as success, what must be
        // true first, or what counts as failure.
        if proposed.success != original.success {
            return Err(BhError::Other(if original.success.is_some() && proposed.success.is_none() {
                "repair dropped the success condition, so the result would be unverifiable".into()
            } else {
                "repair changed the success condition".into()
            }));
        }
        if proposed.preconditions != original.preconditions {
            return Err(BhError::Other(
                "repair changed the step's preconditions".into(),
            ));
        }
        if proposed.fail_when != original.fail_when {
            return Err(BhError::Other(
                "repair changed the step's failure conditions".into(),
            ));
        }
        for action in &proposed.actions {
            // A replacement may reuse a script exactly as the author wrote it,
            // but it may not introduce one. Otherwise a read-only step becomes
            // an arbitrary main-world JS channel, which is a much larger
            // capability than "fix the locator".
            if is_scripted(action) && !original.actions.contains(action) {
                return Err(BhError::Other(
                    "repair introduced a script or file-upload action".into(),
                ));
            }
            match action {
                Action::Navigate { url } if !bh_core::host_allowed(url, &self.allowed_hosts) => {
                    return Err(BhError::Other(format!(
                        "repair navigation to {url} blocked: host not in allowed_hosts"
                    )));
                }
                Action::Type { text, .. } if text.contains("${cred:") => {
                    return Err(BhError::Other(
                        "repair cannot introduce credential placeholders".into(),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn attempt(
        &mut self,
        id: &str,
        step: &Step,
        deadline: Instant,
        artifacts: &mut Vec<String>,
    ) -> Result<bool> {
        let budget = deadline.saturating_duration_since(Instant::now());
        if budget.is_zero() {
            return Err(BhError::Timeout(step.timeout_ms));
        }
        let step_deadline = Instant::now() + Duration::from_millis(step.timeout_ms).min(budget);

        for cond in &step.preconditions {
            let left = step_deadline.saturating_duration_since(Instant::now());
            let ms = left.as_millis().max(1) as u64;
            self.rt
                .wait_for(cond, ms)
                .await
                .map_err(|e| BhError::Condition(format!("precondition not met: {e}")))?;
        }

        // Arm the download watcher before the first action, not after.
        let mut guard = match &step.success {
            Some(c) if contains_download(c) => Some(self.browser.watch_downloads()?),
            _ => None,
        };

        // Baseline for `fail_when`: a condition already true before the action
        // is a leftover from the previous attempt, and only counts again after
        // it clears. A condition that is false now may fire as soon as the
        // action does its damage.
        let mut fail_cleared = Vec::with_capacity(step.fail_when.len());
        for f in &step.fail_when {
            if contains_download(f) {
                fail_cleared.push(false);
            } else {
                fail_cleared.push(!self.rt.check(f).await?);
            }
        }

        for (index, action) in step.actions.iter().enumerate() {
            if self.canceled() {
                let _ = self.session.release_all_input().await;
                return Err(BhError::Canceled);
            }
            if Instant::now() >= step_deadline {
                return Err(BhError::Timeout(step.timeout_ms));
            }
            tracing::debug!(action = ?action, "dispatch");
            let action_started = Instant::now();
            let action = self.materialize(action, &step.goal).await?;
            let kind = action_kind(&action);
            let mut escalated = false;
            let result = match self.rt.act(&action).await {
                Ok(r) => r,
                Err(e) => match self.escalate(step, &action, &e).await? {
                    Some(r) => {
                        escalated = true;
                        r
                    }
                    None => {
                        self.actions.push(ActionRecord {
                            index,
                            kind,
                            ok: false,
                            elapsed_ms: action_started.elapsed().as_millis() as u64,
                            escalated,
                            arbitration: None,
                            error: Some(e.to_string()),
                        });
                        return Err(e);
                    }
                },
            };
            self.actions.push(ActionRecord {
                index,
                kind,
                ok: true,
                elapsed_ms: action_started.elapsed().as_millis() as u64,
                escalated,
                arbitration: result.arbitration.clone(),
                error: None,
            });
            if let (Action::Extract { name, .. }, Some(value)) = (&action, result.value) {
                artifacts.push(self.write_extract(id, name, &value)?);
            }
        }

        match &step.success {
            None => Ok(false), // dispatched, but nothing was proven
            Some(cond) => {
                let verified = verify(
                    &mut self.rt,
                    cond,
                    &step.fail_when,
                    &mut fail_cleared,
                    &mut guard,
                    artifacts,
                    step_deadline,
                )
                .await?;
                if !verified {
                    return Err(BhError::Condition("success condition not met".into()));
                }
                Ok(true)
            }
        }
    }

    /// Turn every `{"visual": "..."}` target into the point it appears at in
    /// the current screenshot. Non-visual targets pass through untouched, so a
    /// step costs zero model calls unless it actually asks to look at pixels.
    async fn materialize(&self, action: &Action, goal: &str) -> Result<Action> {
        let mut action = action.clone();
        match &mut action {
            Action::Click { target, .. }
            | Action::Hover { target }
            | Action::Type { target, .. } => {
                *target = self.resolve_visual(target, goal).await?;
            }
            Action::Drag { from, to, .. } => {
                *from = self.resolve_visual(from, goal).await?;
                *to = self.resolve_visual(to, goal).await?;
            }
            _ => {}
        }
        Ok(action)
    }

    async fn resolve_visual(&self, target: &Target, goal: &str) -> Result<Target> {
        let Some(description) = target.visual() else {
            return Ok(target.clone());
        };
        let vision = self.vision.as_ref().ok_or_else(|| {
            BhError::Action("visual target requires a vision model, but none is configured".into())
        })?;
        let hit = vision.locate(self.session, description, goal).await?;
        tracing::info!(
            description,
            x = hit.point.x,
            y = hit.point.y,
            confidence = hit.confidence,
            "visual target resolved"
        );
        Ok(Target::Point(hit.point))
    }

    /// In-place reroute: a DOM locator that resolves to nothing is retried as
    /// a visual lookup, but only when the step opted in with
    /// `fallback_visual` and its executor is `auto`.
    ///
    /// Deliberately narrow. A missing locator is often a real page change, not
    /// a rendering quirk, and clicking a visually similar control because the
    /// named one vanished is how the wrong button gets pressed. The step has to
    /// say what to look for, and the caller still has to verify the result.
    async fn escalate(
        &mut self,
        step: &Step,
        action: &Action,
        error: &BhError,
    ) -> Result<Option<bh_core::ActionResult>> {
        if step.executor != ExecutorKind::Auto || !matches!(error, BhError::LocatorNotFound(_)) {
            return Ok(None);
        }
        let Some(description) = &step.fallback_visual else {
            return Ok(None);
        };
        let Some(fallback) = visual_fallback(action, description) else {
            return Ok(None);
        };
        tracing::warn!(
            goal = %step.goal,
            error = %error,
            fallback = %description,
            "DOM target missing; rerouting to vision"
        );
        let fallback = self.materialize(&fallback, &step.goal).await?;
        self.rt.act(&fallback).await.map(Some)
    }

    /// Extraction is only worth anything if the value survives the run.
    fn write_extract(&self, step_id: &str, name: &str, value: &serde_json::Value) -> Result<String> {
        let dir = self
            .artifacts_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| BhError::Other(format!("create {}: {e}", dir.display())))?;
        let file = dir.join(format!("{}-{}.json", sanitize(step_id), sanitize(name)));
        let body = serde_json::to_string_pretty(value)
            .map_err(|e| BhError::Other(format!("serialize extract: {e}")))?;
        std::fs::write(&file, body)
            .map_err(|e| BhError::Other(format!("write {}: {e}", file.display())))?;
        Ok(file.to_string_lossy().to_string())
    }
}

/// The subset of actions a visual fallback can express. Typing into a canvas
/// is meaningless, and a drag has two ends that could have failed for different
/// reasons, so neither is silently rerouted.
fn visual_fallback(action: &Action, description: &str) -> Option<Action> {
    match action {
        Action::Click { button, .. } => Some(Action::Click {
            target: Target::Visual { visual: description.to_string() },
            button: *button,
        }),
        Action::Hover { .. } => Some(Action::Hover {
            target: Target::Visual { visual: description.to_string() },
        }),
        _ => None,
    }
}

/// File names come from task authors, so they get the same treatment as any
/// other untrusted input.
fn sanitize(s: &str) -> String {    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if out.is_empty() {
        out.push_str("unnamed");
    }
    out
}

/// Finds a download condition anywhere in the tree, so the watcher can be armed
/// before dispatch.
fn contains_download(c: &Condition) -> bool {
    match c {
        Condition::DownloadCompleted { .. } => true,
        Condition::Not { of } => contains_download(of),
        Condition::All { of } | Condition::Any { of } => of.iter().any(contains_download),
        _ => false,
    }
}

/// Polls a success condition with the same backoff as preconditions: a
/// condition is a predicate that becomes true, not a fact sampled once. A
/// single sample turns every async page into a flaky task.
///
/// `fail_when` is what makes a failed attempt cheap: the page usually knows it
/// failed (an error banner, a 500 toast) long before `success` times out.
/// `cleared[i]` says condition `i` was false before the actions ran, so seeing
/// it true now is news. A condition already true before the actions is stale
/// from the previous attempt and must clear first, otherwise the retry would
/// abort on the banner it was retrying from.
fn verify<'a>(
    rt: &'a mut Runtime<'_>,
    cond: &'a Condition,
    fail_when: &'a [Condition],
    cleared: &'a mut [bool],
    guard: &'a mut Option<DownloadGuard>,
    artifacts: &'a mut Vec<String>,
    deadline: Instant,
) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + 'a>> {
    Box::pin(async move {
        if fail_when.iter().any(contains_download) {
            return Err(BhError::Condition(
                "download conditions are not supported in fail_when".into(),
            ));
        }

        // A download is watched by polling the guard, not by blocking on it:
        // otherwise a page that already showed its error banner would keep us
        // waiting for a file that is never coming.
        if let Condition::DownloadCompleted { filename_regex, min_bytes } = cond {
            let mut backoff = 25;
            loop {
                let g = guard.as_mut().ok_or_else(|| {
                    BhError::Condition("download watcher was not armed before the actions".into())
                })?;
                if let Some(path) = g.try_wait()? {
                    return accept_download(&path, filename_regex, *min_bytes, artifacts);
                }
                for (i, f) in fail_when.iter().enumerate() {
                    if !cleared[i] {
                        if !evaluate(rt, f, guard, artifacts, deadline).await? {
                            cleared[i] = true;
                        }
                    } else if evaluate(rt, f, guard, artifacts, deadline).await? {
                        return Err(fail_condition(f));
                    }
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                tokio::time::sleep(Duration::from_millis(backoff)).await;
                backoff = (backoff * 2).min(100);
            }
        }

        // Nested composites (All/Any/Not) still block inside `evaluate`, so a
        // polled failure signal cannot preempt one. Refuse instead of silently
        // ignoring `fail_when`.
        if contains_download(cond) {
            if !fail_when.is_empty() {
                return Err(BhError::Condition(
                    "fail_when cannot preempt a nested download condition".into(),
                ));
            }
            return evaluate(rt, cond, guard, artifacts, deadline).await;
        }

        let mut backoff = 25;
        loop {
            if evaluate(rt, cond, guard, artifacts, deadline).await? {
                return Ok(true);
            }
            for (i, f) in fail_when.iter().enumerate() {
                if !cleared[i] {
                    if !evaluate(rt, f, guard, artifacts, deadline).await? {
                        cleared[i] = true;
                    }
                } else if evaluate(rt, f, guard, artifacts, deadline).await? {
                    return Err(fail_condition(f));
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(100);
        }
    })
}

fn fail_condition(c: &Condition) -> BhError {
    BhError::Condition(format!(
        "fail_when condition became true: {}",
        serde_json::to_string(c).unwrap_or_default()
    ))
}

/// Shared by the blocking and polling download paths: the conditions are about
/// the file, not about how it was awaited.
fn accept_download(
    path: &Path,
    filename_regex: &str,
    min_bytes: u64,
    artifacts: &mut Vec<String>,
) -> Result<bool> {
    let re = Regex::new(filename_regex)
        .map_err(|e| BhError::Condition(format!("bad filename_regex: {e}")))?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    if !re.is_match(&name) {
        return Err(BhError::Condition(format!(
            "downloaded {name:?} does not match {filename_regex:?}"
        )));
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size < min_bytes {
        return Err(BhError::Condition(format!(
            "downloaded {name:?} is {size} bytes, expected at least {min_bytes}"
        )));
    }
    let recorded = path.to_string_lossy().to_string();
    if !artifacts.iter().any(|a| a == &recorded) {
        artifacts.push(recorded);
    }
    Ok(true)
}

fn evaluate<'a>(
    rt: &'a mut Runtime<'_>,
    cond: &'a Condition,
    guard: &'a mut Option<DownloadGuard>,
    artifacts: &'a mut Vec<String>,
    deadline: Instant,
) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + 'a>> {
    Box::pin(async move {
        match cond {
            Condition::DownloadCompleted { filename_regex, min_bytes } => {
                let g = guard.take().ok_or_else(|| {
                    BhError::Condition("download watcher was not armed before the actions".into())
                })?;
                let left = deadline.saturating_duration_since(Instant::now());
                let path = g.wait(left).await?;
                accept_download(&path, filename_regex, *min_bytes, artifacts)
            }
            Condition::All { of } => {
                for c in of {
                    if !evaluate(rt, c, guard, artifacts, deadline).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Condition::Any { of } => {
                for c in of {
                    if evaluate(rt, c, guard, artifacts, deadline).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Condition::Not { of } => {
                if contains_download(of) {
                    return Err(BhError::Condition(
                        "negated download conditions are not supported".into(),
                    ));
                }
                Ok(!evaluate(rt, of, guard, artifacts, deadline).await?)
            }
            other => rt.check(other).await,
        }
    })
}

/// Actions that execute caller-supplied JavaScript in the page. A model-written
/// step may reuse one of these only if the author already wrote the identical
/// action; it may not add a new channel.
fn is_scripted(action: &Action) -> bool {
    match action {
        Action::Evaluate { .. } | Action::Extract { .. } => true,
        Action::WaitFor { condition, .. } => condition_has_js(condition),
        _ => false,
    }
}

fn condition_has_js(condition: &Condition) -> bool {
    match condition {
        Condition::Js { .. } => true,
        Condition::Not { of } => condition_has_js(of),
        Condition::All { of } | Condition::Any { of } => of.iter().any(condition_has_js),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bh_core::MouseButton;

    #[test]
    fn only_scripted_actions_are_recognized() {
        let eval = Action::Evaluate { script: "1".into() };
        let extract = Action::Extract { name: "x".into(), script: "({})".into() };
        assert!(is_scripted(&eval));
        assert!(is_scripted(&extract));
        assert!(is_scripted(&Action::WaitFor {
            condition: Condition::Not { of: Box::new(Condition::Js { script: "1".into() }) },
            timeout_ms: None,
        }));
        assert!(!is_scripted(&Action::WaitFor {
            condition: Condition::UrlMatches { pattern: "**/ok".into() },
            timeout_ms: None,
        }));
        assert!(!is_scripted(&Action::Click {
            target: bh_core::Target::Locator(bh_core::Locator::role("button", "go")),
            button: MouseButton::Left,
        }));
    }

    #[test]
    fn only_click_and_hover_can_be_rerouted_visually() {
        let click = Action::Click {
            target: Target::Locator(bh_core::Locator::role("button", "Legacy")),
            button: MouseButton::Left,
        };
        let fb = visual_fallback(&click, "the red button").expect("click reroutes");
        match fb {
            Action::Click { target, .. } => {
                assert_eq!(target.visual(), Some("the red button"));
            }
            other => panic!("expected a click, got {other:?}"),
        }
        assert!(visual_fallback(
            &Action::Drag {
                from: Target::Point(bh_core::Point::new(1.0, 1.0)),
                to: Target::Point(bh_core::Point::new(2.0, 2.0)),
                steps: 4,
            },
            "x"
        )
        .is_none());
    }
}
