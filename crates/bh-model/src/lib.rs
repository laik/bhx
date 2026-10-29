//! Model layer: capability-based adapters and the router in front of them.
//!
//! The contract is deliberately narrower than "an LLM that can do things":
//! an adapter is shown a state (or an image) and returns a *proposal* — an
//! element id or a normalized box. It never gets a browser handle, and the
//! router checks every field before the proposal reaches the executor.
//!
//! Providers are configuration, not identity. Two adapters ship in the crate:
//! an OpenAI-compatible chat endpoint that most local and hosted servers
//! expose, and an optional typed-decision sidecar (Laya) for the two
//! capabilities that are really multiple-choice — `Locate` and `Verify`.
//! The local adapter is tried first; the remote one is the fallback.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use std::collections::HashSet;

use async_trait::async_trait;
use bh_core::{BhError, Result, Step};
use serde::{Deserialize, Serialize};

pub mod laya;
pub mod openai;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Turn a goal plus the current page state into workflow steps.
    Plan,
    /// Pick one of the candidate element ids.
    Locate,
    /// Ground a description in a screenshot and return a normalized box.
    VisualLocate,
    /// Judge whether the current state shows the goal achieved.
    Verify,
    /// Propose a corrected replacement for a step that already failed.
    Repair,
}

#[derive(Clone)]
pub struct ImageData {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub struct ModelRequest {
    pub capability: Capability,
    pub instructions: String,
    pub state: Option<String>,
    /// Ordered (id, description). Order is priority; the caller has already
    /// pre-filtered to a size the model can actually rank.
    pub criteria: Vec<(String, String)>,
    pub image: Option<ImageData>,
    pub timeout_ms: u64,
}

impl ModelRequest {
    pub fn visual(description: &str, goal: &str) -> Self {
        Self {
            capability: Capability::VisualLocate,
            instructions: description.to_string(),
            state: Some(goal.to_string()),
            criteria: Vec::new(),
            image: None,
            timeout_ms: 8_000,
        }
    }

    /// `state` is the page as the observer sees it (URL, title, controls).
    pub fn plan(goal: &str, state: &str) -> Self {
        Self {
            capability: Capability::Plan,
            instructions: goal.to_string(),
            state: Some(state.to_string()),
            criteria: Vec::new(),
            image: None,
            timeout_ms: 30_000,
        }
    }

    /// Ask whether `goal` is achieved in `state`. The two choices are fixed so
    /// the answer is a decision, not free text the caller has to parse.
    pub fn verify(goal: &str, state: &str) -> Self {
        Self {
            capability: Capability::Verify,
            instructions: goal.to_string(),
            state: Some(state.to_string()),
            criteria: vec![
                ("achieved".into(), "the page state shows the goal completed".into()),
                (
                    "not_achieved".into(),
                    "the page state does not show the goal, or shows an error".into(),
                ),
            ],
            image: None,
            timeout_ms: 15_000,
        }
    }

    /// Pick one of `candidates` — ordered, highest priority first — for `goal`.
    /// `state` is the observer's text (it already starts with `Task: <goal>`),
    /// so a choice model can see both the task and the page it must act on.
    pub fn locate(goal: &str, state: &str, candidates: Vec<(String, String)>) -> Self {
        Self {
            capability: Capability::Locate,
            instructions: goal.to_string(),
            state: Some(state.to_string()),
            criteria: candidates,
            image: None,
            timeout_ms: 10_000,
        }
    }

    /// Give the model the failed step, why it failed, and the page it failed on.
    /// The answer is a replacement step, not free-form advice.
    pub fn repair(goal: &str, state: &str, failed: &Step, error: &str) -> Self {
        let step = serde_json::to_string_pretty(failed).unwrap_or_default();
        Self {
            capability: Capability::Repair,
            instructions: goal.to_string(),
            state: Some(format!(
                "Failed step:\n{step}\n\nFailure: {error}\n\nCurrent page:\n{state}"
            )),
            criteria: Vec::new(),
            image: None,
            timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelProposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    /// `[x, y, w, h]`, each in 0..1 of the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox_norm: Option<[f64; 4]>,
    /// Workflow proposed for `Capability::Plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<Step>>,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Achieved,
    NotAchieved,
    /// The model picked a side, but not confidently enough for the caller's
    /// threshold. This is a distinct answer, not a tie to break.
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub verdict: Verdict,
    pub confidence: f64,
    pub threshold: f64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Run `Capability::Verify` and apply the caller's threshold.
///
/// The router already rejects anything that is not one of the two fixed
/// choices; this function adds the threshold semantics that every caller would
/// otherwise have to repeat (and could drift on).
///
/// `image` is optional: adapters that accept images (OpenAI-compatible vision)
/// judge the state and the picture together, and text-only adapters (Laya)
/// refuse rather than silently dropping it.
pub async fn verify(
    router: &ModelRouter,
    goal: &str,
    state: &str,
    threshold: f64,
    image: Option<ImageData>,
) -> Result<VerifyReport> {
    if goal.trim().is_empty() {
        return Err(BhError::Other("verify goal is empty".into()));
    }
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err(BhError::Other(format!(
            "verify threshold {threshold} is outside 0..1"
        )));
    }
    if let Some(img) = &image {
        if img.bytes.is_empty() || img.width == 0 || img.height == 0 {
            return Err(BhError::Other("verify screenshot is empty".into()));
        }
    }
    let mut req = ModelRequest::verify(goal, state);
    req.image = image;
    let p = router.propose(&req).await?;
    let choice = p
        .choice
        .as_deref()
        .ok_or_else(|| BhError::Other("verify proposal has no choice".into()))?;
    let picked = match choice {
        "achieved" => Verdict::Achieved,
        "not_achieved" => Verdict::NotAchieved,
        other => {
            return Err(BhError::Other(format!("unknown verify choice {other:?}")))
        }
    };
    let verdict = if p.confidence >= threshold { picked } else { Verdict::Uncertain };
    Ok(VerifyReport {
        verdict,
        confidence: p.confidence,
        threshold,
        model: p.model,
        reason: p.reason,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocateReport {
    /// One of the candidate ids that were offered, or `None` if the model
    /// answered without picking one. The caller still has to map it.
    pub choice: Option<String>,
    pub confidence: f64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Ask a model to pick one of a fixed set of candidates for the current task.
///
/// The candidates are the whole question: the model may not invent an id (the
/// router already rejects one that is not in `criteria`), and nothing here
/// decides whether the pick is *good enough* — that is the caller's threshold,
/// because a locator arbitration and a next-action suggestion are not equally
/// safe places to be wrong.
pub async fn locate(
    router: &ModelRouter,
    goal: &str,
    state: &str,
    instruction: &str,
    candidates: Vec<(String, String)>,
) -> Result<LocateReport> {
    if candidates.is_empty() {
        return Err(BhError::Other("locate was given no candidates".into()));
    }
    if state.trim().is_empty() {
        return Err(BhError::Other("locate was given no page state".into()));
    }
    let mut req = ModelRequest::locate(goal, state, candidates);
    if !instruction.trim().is_empty() {
        req.instructions = format!("{goal}
{instruction}");
    }
    let p = router.propose(&req).await?;
    Ok(LocateReport {
        choice: p.choice,
        confidence: p.confidence,
        model: p.model,
        reason: p.reason,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairReport {
    pub step: Step,
    pub confidence: f64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Ask for a corrected replacement step.
///
/// This only *proposes*. The caller still has to prove the replacement stays
/// within the original step's safety envelope (same id, side effect no higher,
/// hosts allowed) before running it. The router validates the step schema.
pub async fn repair(
    router: &ModelRouter,
    goal: &str,
    state: &str,
    failed: &Step,
    error: &str,
) -> Result<RepairReport> {
    let p = router
        .propose(&ModelRequest::repair(goal, state, failed, error))
        .await?;
    let steps = p
        .steps
        .ok_or_else(|| BhError::Other("repair proposal has no step".into()))?;
    let step = steps
        .into_iter()
        .next()
        .ok_or_else(|| BhError::Other("repair proposal has zero steps".into()))?;
    Ok(RepairReport {
        step,
        confidence: p.confidence,
        model: p.model,
        reason: p.reason,
    })
}

#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn supports(&self, capability: Capability) -> bool;
    async fn propose(&self, req: &ModelRequest) -> Result<ModelProposal>;

    /// Make the adapter ready to answer *now*, if that costs anything. A
    /// remote endpoint has nothing to do; a local sidecar has seconds of model
    /// load. Returning an error does not fail the task — an adapter that
    /// cannot warm up may still answer, and a warm-up failure must not look
    /// like a decision failure.
    async fn warm(&self) -> Result<()> {
        Ok(())
    }
}

/// Per-task budget and validation in front of every adapter.
pub struct ModelRouter {
    adapters: Vec<Arc<dyn ModelAdapter>>,
    budget: u32,
    calls: AtomicU32,
}

impl ModelRouter {
    pub fn new(adapters: Vec<Arc<dyn ModelAdapter>>, budget: u32) -> Self {
        Self { adapters, budget, calls: AtomicU32::new(0) }
    }

    /// Build from environment. An unconfigured harness has no adapters, and a
    /// visual target then fails with a clear message instead of guessing.
    ///
    /// Order is the fallback chain. The hosted adapter goes first by default:
    /// a local decision sidecar is cheap and fast, but its accuracy on a given
    /// site has to be measured, and a semantic verdict is a safety check that
    /// should not be silently handed to the weaker model. `BH_LAYA_PRIORITY=
    /// first` puts the sidecar in front for deployments that have measured it.
    pub fn from_env(budget: u32) -> Self {
        let laya = match laya::LayaSidecar::from_env() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "ignoring invalid Laya configuration");
                None
            }
        };
        let mut remote: Vec<Arc<dyn ModelAdapter>> = Vec::new();
        match openai::OpenAiCompat::from_env() {
            Ok(list) => remote.extend(list.into_iter().map(|a| Arc::new(a) as Arc<dyn ModelAdapter>)),
            Err(e) => tracing::warn!(error = %e, "ignoring invalid model configuration"),
        }
        let local_first = std::env::var("BH_LAYA_PRIORITY")
            .map(|v| v.trim().eq_ignore_ascii_case("first"))
            .unwrap_or(false);
        let mut adapters: Vec<Arc<dyn ModelAdapter>> = Vec::new();
        match laya {
            Some(local) if local_first => {
                adapters.push(local);
                adapters.extend(remote);
            }
            Some(local) => {
                adapters.extend(remote);
                adapters.push(local);
            }
            None => adapters = remote,
        }
        Self::new(adapters, budget)
    }

    /// Load whatever a model call would need, before anything depends on the
    /// answer. For a strategy loop this is not an optimisation: a 4.4 s sidecar
    /// load paid at the first question lands after the window the decision was
    /// for. Adapters that fail to warm are reported and left in place; the
    /// first real call will surface the same error with its full context.
    pub async fn warm(&self, capability: Capability) -> Vec<String> {
        let mut failures = Vec::new();
        for adapter in self.adapters.iter().filter(|a| a.supports(capability)) {
            if let Err(e) = adapter.warm().await {
                tracing::warn!(model = adapter.name(), error = %e, "model warm-up failed");
                failures.push(format!("{}: {e}", adapter.name()));
            }
        }
        failures
    }

    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn budget(&self) -> u32 {
        self.budget
    }

    pub fn is_configured(&self) -> bool {
        !self.adapters.is_empty()
    }

    pub async fn propose(&self, req: &ModelRequest) -> Result<ModelProposal> {
        let candidates: Vec<&Arc<dyn ModelAdapter>> = self
            .adapters
            .iter()
            .filter(|a| a.supports(req.capability))
            .collect();
        if candidates.is_empty() {
            return Err(BhError::Other(format!(
                "no model adapter configured for {:?}",
                req.capability
            )));
        }

        // Count the attempt, not the success: a model that times out still
        // costs money and still consumes the task's budget.
        let used = self
            .calls
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.budget).then_some(n + 1)
            })
            .map_err(|n| {
                BhError::Other(format!(
                    "model call budget exhausted ({} of {} used)",
                    n, self.budget
                ))
            })?;
        let attempt = used + 1;

        let mut errors = Vec::new();
        for adapter in candidates {
            match adapter.propose(req).await {
                Ok(mut p) => {
                    if p.model.is_empty() {
                        // The adapter knows which model answered; a proposal
                        // that omits the field should not look anonymous.
                        p.model = adapter.name().to_string();
                    }
                    match validate(req, &mut p) {
                        Ok(()) => return Ok(p),
                        Err(e) => {
                            tracing::warn!(model = adapter.name(), attempt, error = %e, "proposal rejected");
                            errors.push(format!("{}: {e}", adapter.name()));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(model = adapter.name(), attempt, error = %e, "model call failed");
                    errors.push(format!("{}: {e}", adapter.name()));
                }
            }
        }

        if errors.is_empty() {
            return Err(BhError::Other(format!(
                "no model adapter configured for {:?}",
                req.capability
            )));
        }
        Err(BhError::Other(format!(
            "no usable proposal: {}",
            errors.join("; ")
        )))
    }
}

/// Schema checks from docs/protocol.md §8. A proposal that fails any of these
/// is discarded, not repaired.
pub fn validate(req: &ModelRequest, p: &mut ModelProposal) -> Result<()> {
    if !p.confidence.is_finite() || !(0.0..=1.0).contains(&p.confidence) {
        return Err(BhError::Other(format!(
            "confidence {} outside 0..1",
            p.confidence
        )));
    }

    if req.capability == Capability::Plan {
        let steps = p
            .steps
            .as_ref()
            .ok_or_else(|| BhError::Other("plan proposal has no steps".into()))?;
        if steps.is_empty() {
            return Err(BhError::Other("plan proposal has zero steps".into()));
        }
        if steps.len() > 50 {
            return Err(BhError::Other(format!(
                "plan proposal has {} steps (max 50)",
                steps.len()
            )));
        }
        let mut ids = HashSet::new();
        for (i, step) in steps.iter().enumerate() {
            if step.goal.trim().is_empty() {
                return Err(BhError::Other(format!("step {i} has an empty goal")));
            }
            if step.actions.is_empty() {
                return Err(BhError::Other(format!("step {i} ({}) has no actions", step.goal)));
            }
            if step.timeout_ms == 0 || step.timeout_ms > 600_000 {
                return Err(BhError::Other(format!(
                    "step {i} timeout {} is outside 1..600000 ms",
                    step.timeout_ms
                )));
            }
            if !step.id.is_empty() && !ids.insert(step.id.as_str()) {
                return Err(BhError::Other(format!("duplicate step id {:?}", step.id)));
            }
        }
    }

    if req.capability == Capability::Repair {
        let steps = p
            .steps
            .as_ref()
            .ok_or_else(|| BhError::Other("repair proposal has no step".into()))?;
        if steps.len() != 1 {
            return Err(BhError::Other(format!(
                "repair proposal must contain exactly one step (got {})",
                steps.len()
            )));
        }
        let step = &steps[0];
        if step.id.trim().is_empty() {
            // The replacement has to be tied back to the failure it repairs.
            return Err(BhError::Other("repair step has an empty id".into()));
        }
        if step.goal.trim().is_empty() {
            return Err(BhError::Other("repair step has an empty goal".into()));
        }
        if step.actions.is_empty() {
            return Err(BhError::Other("repair step has no actions".into()));
        }
        if step.timeout_ms == 0 || step.timeout_ms > 600_000 {
            return Err(BhError::Other(format!(
                "repair step timeout {} is outside 1..600000 ms",
                step.timeout_ms
            )));
        }
        if step.executor == bh_core::ExecutorKind::Interactive {
            return Err(BhError::Other(
                "repair step cannot switch a workflow step to the interactive executor".into(),
            ));
        }
    }

    if !req.criteria.is_empty() {
        match &p.choice {
            Some(c) if req.criteria.iter().any(|(id, _)| id == c) => {}
            Some(c) => return Err(BhError::Other(format!("choice {c:?} is not a candidate"))),
            None => return Err(BhError::Other("no choice in proposal".into())),
        }
    }

    if req.capability == Capability::VisualLocate {
        let b = p
            .bbox_norm
            .ok_or_else(|| BhError::Other("visual proposal has no bbox_norm".into()))?;
        if b.iter().any(|v| !v.is_finite()) {
            return Err(BhError::Other("bbox contains a non-finite number".into()));
        }
        let [x, y, w, h] = b;
        if x < 0.0 || y < 0.0 || w <= 0.0 || h <= 0.0 {
            return Err(BhError::Other(format!("bbox {b:?} has a negative origin or zero size")));
        }
        // A hair over 1 is a rounding artifact; a box well outside is a bad
        // coordinate space (pixels pasted into a normalized field).
        if x + w > 1.000_001 || y + h > 1.000_001 {
            return Err(BhError::Other(format!("bbox {b:?} exceeds the image")));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(ModelProposal);

    #[async_trait]
    impl ModelAdapter for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        fn supports(&self, _: Capability) -> bool {
            true
        }
        async fn propose(&self, _: &ModelRequest) -> Result<ModelProposal> {
            Ok(self.0.clone())
        }
    }

    fn visual_req() -> ModelRequest {
        ModelRequest::visual("the red button", "click it")
    }

    #[tokio::test]
    async fn rejects_a_box_outside_the_image() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                bbox_norm: Some([0.9, 0.1, 0.5, 0.1]),
                confidence: 0.9,
                ..Default::default()
            }))],
            2,
        );
        assert!(router.propose(&visual_req()).await.is_err());
        assert_eq!(router.calls(), 1);
    }

    #[tokio::test]
    async fn rejects_an_invented_choice() {
        let mut req = visual_req();
        req.capability = Capability::Locate;
        req.criteria = vec![("e1".into(), "Export".into())];
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                choice: Some("e99".into()),
                confidence: 0.8,
                ..Default::default()
            }))],
            2,
        );
        assert!(router.propose(&req).await.is_err());
    }

    #[tokio::test]
    async fn budget_is_enforced_and_attempts_are_counted() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                bbox_norm: Some([0.1, 0.1, 0.1, 0.1]),
                confidence: 0.5,
                ..Default::default()
            }))],
            1,
        );
        assert!(router.propose(&visual_req()).await.is_ok());
        assert!(router.propose(&visual_req()).await.is_err());
        assert_eq!(router.calls(), 1);
    }

    #[tokio::test]
    async fn no_adapter_is_a_clear_error() {
        let router = ModelRouter::new(Vec::new(), 2);
        let e = router.propose(&visual_req()).await.unwrap_err();
        assert!(e.to_string().contains("no model adapter"));
        // Nothing was called, so nothing may be charged to the task.
        assert_eq!(router.calls(), 0);
    }

    #[tokio::test]
    async fn verifies_against_fixed_choices() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                choice: Some("achieved".into()),
                confidence: 0.9,
                reason: Some("the order table is visible".into()),
                ..Default::default()
            }))],
            1,
        );
        let req = ModelRequest::verify("logged in with orders visible", "URL: http://x");
        assert_eq!(req.criteria.len(), 2);
        let p = router.propose(&req).await.unwrap();
        assert_eq!(p.choice.as_deref(), Some("achieved"));
        assert_eq!(p.model, "stub");
    }

    #[tokio::test]
    async fn a_verify_choice_outside_the_two_options_is_rejected() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                choice: Some("probably".into()),
                confidence: 0.9,
                ..Default::default()
            }))],
            1,
        );
        assert!(
            router
                .propose(&ModelRequest::verify("done", "URL: http://x"))
                .await
                .is_err()
        );
    }

    fn step(id: &str, goal: &str) -> Step {
        Step {
            id: id.into(),
            goal: goal.into(),
            actions: vec![bh_core::Action::Navigate { url: "http://x".into() }],
            timeout_ms: 5_000,
            ..Default::default()
        }
    }

    fn plan_req() -> ModelRequest {
        ModelRequest::plan("export the orders", "url: http://x")
    }

    #[tokio::test]
    async fn accepts_a_plan_with_steps() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                steps: Some(vec![step("s1", "open"), step("s2", "export")]),
                confidence: 0.6,
                ..Default::default()
            }))],
            2,
        );
        let p = router.propose(&plan_req()).await.unwrap();
        assert_eq!(p.steps.unwrap().len(), 2);
        // The proposal had no model field; the adapter's name fills it in.
        assert_eq!(p.model, "stub");
        assert_eq!(router.calls(), 1);
    }

    #[tokio::test]
    async fn rejects_a_plan_without_steps_or_with_zero_steps() {
        for proposal in [
            ModelProposal { confidence: 0.6, ..Default::default() },
            ModelProposal { steps: Some(Vec::new()), confidence: 0.6, ..Default::default() },
        ] {
            let router = ModelRouter::new(vec![Arc::new(Stub(proposal))], 2);
            assert!(router.propose(&plan_req()).await.is_err());
        }
    }

    #[tokio::test]
    async fn rejects_duplicate_step_ids() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                steps: Some(vec![step("s1", "open"), step("s1", "export")]),
                confidence: 0.6,
                ..Default::default()
            }))],
            2,
        );
        let e = router.propose(&plan_req()).await.unwrap_err();
        assert!(e.to_string().contains("duplicate step id"));
    }

    #[tokio::test]
    async fn rejects_a_step_with_no_actions() {
        let mut s = step("s1", "open");
        s.actions.clear();
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                steps: Some(vec![s]),
                confidence: 0.6,
                ..Default::default()
            }))],
            2,
        );
        assert!(router.propose(&plan_req()).await.is_err());
    }

    #[tokio::test]
    async fn a_confident_verdict_is_passed_through() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                choice: Some("achieved".into()),
                confidence: 0.91,
                reason: Some("two failed rows are listed".into()),
                ..Default::default()
            }))],
            1,
        );
        let r = verify(&router, "failed orders only", "URL: http://x", 0.6, None).await.unwrap();
        assert_eq!(r.verdict, Verdict::Achieved);
        assert_eq!(r.confidence, 0.91);
        assert_eq!(r.model, "stub");
        assert_eq!(r.threshold, 0.6);
    }

    #[tokio::test]
    async fn a_verdict_below_the_threshold_is_uncertain_not_rounded() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                choice: Some("not_achieved".into()),
                confidence: 0.55,
                ..Default::default()
            }))],
            1,
        );
        let r = verify(&router, "export finished", "URL: http://x", 0.6, None).await.unwrap();
        assert_eq!(r.verdict, Verdict::Uncertain);
    }

    #[tokio::test]
    async fn verify_rejects_an_invalid_threshold_before_calling_a_model() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                choice: Some("achieved".into()),
                confidence: 0.9,
                ..Default::default()
            }))],
            1,
        );
        assert!(verify(&router, "done", "URL: http://x", 1.5, None).await.is_err());
        assert!(verify(&router, "done", "URL: http://x", f64::NAN, None).await.is_err());
        assert_eq!(router.calls(), 0);
    }

    #[tokio::test]
    async fn verify_rejects_an_empty_image_before_calling_a_model() {
        let router = ModelRouter::new(vec![], 1);
        let empty = ImageData { media_type: "image/png", bytes: Vec::new(), width: 10, height: 10 };
        let e = verify(&router, "done", "URL: http://x", 0.6, Some(empty))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("screenshot is empty"), "{e}");
        assert_eq!(router.calls(), 0);
    }

    #[test]
    fn repair_request_carries_the_failed_step_and_error() {
        let failed = step("s2", "filter failed orders");
        let req = ModelRequest::repair("export failed orders", "URL: http://x", &failed, "locator not found");
        assert_eq!(req.capability, Capability::Repair);
        assert_eq!(req.instructions, "export failed orders");
        let state = req.state.unwrap();
        assert!(state.contains("\"id\": \"s2\""));
        assert!(state.contains("filter failed orders"));
        assert!(state.contains("locator not found"));
        assert!(state.contains("URL: http://x"));
    }

    #[tokio::test]
    async fn a_repair_must_be_exactly_one_step_with_an_id() {
        for steps in [
            vec![step("s2", "retry")],
            vec![step("s2", "retry"), step("s3", "more")],
        ] {
            let one = steps.len() == 1;
            let router = ModelRouter::new(
                vec![Arc::new(Stub(ModelProposal {
                    steps: Some(steps),
                    confidence: 0.8,
                    ..Default::default()
                }))],
                1,
            );
            let failed = step("s2", "filter");
            let got = repair(&router, "export", "URL: http://x", &failed, "not found").await;
            assert_eq!(got.is_ok(), one);
        }

        let mut no_id = step("", "retry");
        no_id.id.clear();
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                steps: Some(vec![no_id]),
                confidence: 0.8,
                ..Default::default()
            }))],
            1,
        );
        let failed = step("s2", "filter");
        let e = repair(&router, "export", "URL: http://x", &failed, "not found")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("empty id"));
    }

    #[tokio::test]
    async fn accepts_a_single_replacement_step() {
        let router = ModelRouter::new(
            vec![Arc::new(Stub(ModelProposal {
                steps: Some(vec![step("s2", "click the right filter")]),
                confidence: 0.77,
                reason: Some("the old button was removed".into()),
                ..Default::default()
            }))],
            1,
        );
        let failed = step("s2", "filter");
        let r = repair(&router, "export", "URL: http://x", &failed, "not found")
            .await
            .unwrap();
        assert_eq!(r.step.id, "s2");
        assert_eq!(r.step.goal, "click the right filter");
        assert_eq!(r.model, "stub");
        assert_eq!(r.confidence, 0.77);
        assert_eq!(router.calls(), 1);
    }
}
