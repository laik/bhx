//! Arbitrating an ambiguous locator: the model chooses between candidates the
//! resolver already found, and only when it is sure.
//!
//! The ordinary behaviour — several matches is an error — is the safe one, and
//! it stays the default. This module exists for the case where the page really
//! does have two "导出" buttons and a task has decided it would rather have a
//! confident guess than a stop. Two things make that acceptable:
//!
//! * the model sees exactly the candidates resolution produced, from the same
//!   observation they came from, and cannot add or rename any of them;
//! * the choice is recorded on the action (`arbitration`), so a click that
//!   acted on a guess is still identifiable as one in a trace.
//!
//! Below the confidence floor it declines and the original error stands. A
//! wrong pick here acts on the wrong element — it does not merely mislabel an
//! outcome, which is why the floor defaults higher than `semantic_verify`'s.

use bh_action::LocatorArbiter;
use bh_core::{ArbitrationContext, ArbitrationDecision, ArbiterChoice, ElementInfo};

/// What a candidate is: role, accessible name, value, usability. This is the
/// only part of it a model can reason about, so it is also what
/// "indistinguishable" has to be judged on.
fn essence(e: &ElementInfo) -> String {
    let name = if e.name.trim().is_empty() { "(unnamed)" } else { e.name.as_str() };
    let mut d = format!("{} \"{name}\"", if e.role.is_empty() { "element" } else { &e.role });
    if let Some(v) = &e.value {
        d.push_str(&format!(" value=\"{v}\""));
    }
    if !e.is_enabled() {
        d.push_str(" (disabled)");
    }
    d
}

/// The candidate ids handed to the model. `c1..cN` rather than the elements'
/// own ids because DOM-level candidates have no element id, and because the
/// model must not be able to invent an id that was never offered.
fn describe(options: &[ElementInfo]) -> Vec<(String, String)> {
    options
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut d = essence(e);
            // An AX candidate also appears in the page state the model is
            // shown, so name that id: it lets a capable model point at the
            // line it read instead of guessing from position.
            if !e.eid.is_empty() {
                d.push_str(&format!(" (listed as {} in the page state)", e.eid));
            }
            (format!("c{}", i + 1), d)
        })
        .collect()
}

/// True when the candidates would all be described the same way. Measured, not
/// assumed: with two buttons both named 导出, a local choice model answered
/// *confidently* (0.67 and 0.83 across two orderings) and stuck to the same
/// element both times — it was reading the id, not the page, because the page
/// said nothing to tell them apart. No confidence floor catches that, since
/// the confidence is real. Declining is the honest answer, and the error says
/// what would fix it.
fn indistinguishable(options: &[ElementInfo]) -> bool {
    let descriptions: Vec<String> = options.iter().map(essence).collect();
    match descriptions.first() {
        None => true,
        Some(first) => descriptions.iter().all(|d| d == first),
    }
}

pub struct ModelArbiter {
    vision: std::sync::Arc<crate::vision::Vision>,
    goal: String,
    min_confidence: f64,
    max_candidates: usize,
}

impl ModelArbiter {
    pub fn new(
        vision: std::sync::Arc<crate::vision::Vision>,
        goal: impl Into<String>,
        min_confidence: f64,
        max_candidates: usize,
    ) -> Self {
        Self { vision, goal: goal.into(), min_confidence, max_candidates }
    }
}

#[async_trait::async_trait]
impl LocatorArbiter for ModelArbiter {
    async fn choose(&self, ctx: &ArbitrationContext<'_>) -> ArbitrationDecision {
        if ctx.options.len() > self.max_candidates {
            // Too many matches: this is a bad locator, not a hard question.
            // Decline without spending a model call on it.
            tracing::warn!(
                candidates = ctx.options.len(),
                max = self.max_candidates,
                strategy = ctx.strategy,
                "ambiguous locator has too many candidates to arbitrate"
            );
            return ArbitrationDecision::Decline {
                note: Some(format!(
                    "{} candidates is more than the {} this task allows; \
                     the locator needs narrowing, not choosing",
                    ctx.options.len(),
                    self.max_candidates
                )),
            };
        }

        if indistinguishable(ctx.options) {
            // Nothing in the page tells these apart, so any answer is a coin
            // flip the model happens to sound sure about. Ask the author to
            // narrow the locator instead.
            tracing::warn!(
                candidates = ctx.options.len(),
                strategy = ctx.strategy,
                "ambiguous candidates are indistinguishable; a model cannot choose"
            );
            return ArbitrationDecision::Decline {
                note: Some(
                    "the matches carry the same role and name, so nothing on the page \
                     tells them apart and no model can choose between them; use a more \
                     specific locator (css, test_id, or a containing scope)".to_string(),
                ),
            };
        }

        let goal = if self.goal.trim().is_empty() { ctx.asked } else { self.goal.as_str() };
        let state = ctx.observation.to_state_text(goal);
        let candidates = describe(ctx.options);
        let asked = ctx.asked;
        let instruction = format!(
            "A step asked for this target and several elements match it: {asked}\n\
             Which of the candidates is the one the step means?"
        );

        let report = match self.vision.locate_candidates(goal, &state, &instruction, candidates).await {
            Ok(r) => r,
            Err(e) => {
                // An unavailable model is not a licence to guess. The caller
                // keeps the original ambiguity error and says why.
                tracing::warn!(error = %e, strategy = ctx.strategy, "ambiguity arbiter unavailable");
                return ArbitrationDecision::Decline {
                    note: Some(format!("the model could not answer: {e}")),
                };
            }
        };

        let index = match report.choice.as_deref().and_then(|c| c.strip_prefix('c')) {
            None => {
                // No usable answer: the model refused, or invented an id (the
                // router already discarded that proposal). Not a licence to act.
                tracing::info!(choice = ?report.choice, "ambiguity arbiter gave no usable choice");
                return ArbitrationDecision::Decline {
                    note: Some("the model did not pick one of the candidates".to_string()),
                };
            }
            Some(digits) => match digits.parse::<usize>() {
                Ok(n) if (1..=ctx.options.len()).contains(&n) => n,
                _ => {
                    tracing::info!(choice = report.choice.as_deref().unwrap_or(""),
                                        candidates = ctx.options.len(),
                                        "ambiguity arbiter named a candidate that was not offered");
                    return ArbitrationDecision::Decline {
                        note: Some(format!(
                            "the model picked {:?}, which was not among the {} candidates",
                            report.choice,
                            ctx.options.len()
                        )),
                    };
                }
            },
        };

        if report.confidence < self.min_confidence {
            tracing::info!(
                confidence = report.confidence,
                floor = self.min_confidence,
                "ambiguity arbiter declined: below the confidence floor"
            );
            return ArbitrationDecision::Decline {
                note: Some(format!(
                    "the model was only {} confident, below this task's floor of {}",
                    report.confidence, self.min_confidence
                )),
            };
        }

        ArbitrationDecision::Pick(ArbiterChoice {
            index: index - 1,
            model: report.model,
            confidence: report.confidence,
            reason: report.reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bh_core::{ElementInfo, ElementState, Observation, Rect};
    use bh_model::{Capability, ModelAdapter, ModelProposal, ModelRouter};
    use std::sync::Arc;

    struct Stub {
        proposal: ModelProposal,
        fail: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelAdapter for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        fn supports(&self, c: Capability) -> bool {
            c == Capability::Locate
        }
        async fn propose(&self, req: &ModelRequest) -> bh_core::Result<ModelProposal> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(bh_core::BhError::Other("stub is down".into()));
            }
            // Record what was asked so the assertions can inspect the prompt.
            STASH.with(|s| *s.borrow_mut() = Some(req.clone()));
            Ok(self.proposal.clone())
        }
    }

    use bh_model::ModelRequest;
    thread_local! {
        static STASH: std::cell::RefCell<Option<ModelRequest>> = const { std::cell::RefCell::new(None) };
    }

    fn el(eid: &str, role: &str, name: &str) -> ElementInfo {
        ElementInfo {
            eid: eid.into(),
            role: role.into(),
            name: name.into(),
            value: None,
            bbox: Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 },
            states: vec![ElementState::Enabled, ElementState::Visible],
            hash: 1,
            backend_node_id: 7,
            frame_id: "main".into(),
        }
    }

    fn obs() -> Observation {
        Observation {
            id: "obs_1".into(),
            seq: 1,
            state_format_version: 1,
            url: "http://x/".into(),
            title: "t".into(),
            viewport: Default::default(),
            elements: vec![el("e1", "button", "导出 CSV"), el("e2", "button", "导出 PDF")],
            truncated: false,
        }
    }

    fn arbiter(proposal: ModelProposal, floor: f64, max: usize) -> (ModelArbiter, Arc<Stub>) {
        let stub = Arc::new(Stub {
            proposal,
            fail: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let router = ModelRouter::new(vec![stub.clone() as Arc<dyn ModelAdapter>], 5);
        (ModelArbiter::new(Arc::new(crate::vision::Vision::new(router)), "导出订单", floor, max), stub)
    }

    /// `Some(note)` when the arbiter declined, with its stated reason.
    async fn declined(a: &ModelArbiter, ctx: &ArbitrationContext<'_>) -> Option<Option<String>> {
        match a.choose(ctx).await {
            ArbitrationDecision::Decline { note } => Some(note),
            ArbitrationDecision::Pick(_) => None,
        }
    }

    async fn picked(a: &ModelArbiter, ctx: &ArbitrationContext<'_>) -> ArbiterChoice {
        match a.choose(ctx).await {
            ArbitrationDecision::Pick(c) => c,
            ArbitrationDecision::Decline { note } => {
                panic!("expected a pick, got a decline: {note:?}")
            }
        }
    }

    fn ctx<'a>(o: &'a Observation, opts: &'a [ElementInfo]) -> ArbitrationContext<'a> {
        ArbitrationContext { asked: "{\"role\":\"button\"}", observation: o, options: opts, strategy: "role" }
    }

    #[test]
    fn candidates_that_read_the_same_are_indistinguishable() {
        let same = vec![el("e1", "button", "导出"), el("e2", "button", "导出")];
        assert!(indistinguishable(&same), "only the element id differs, and a model cannot use it");
        // The id reference is added for the model's benefit; it must not make
        // two identical controls look different to this check.
        let described = describe(&same);
        assert_ne!(described[0].1, described[1].1);
        let differ = vec![el("e1", "button", "导出 CSV"), el("e2", "button", "导出 PDF")];
        assert!(!indistinguishable(&differ));
        // A disabled control is a real difference: one of them cannot be used.
        let mut disabled = el("e2", "button", "导出");
        disabled.states = vec![ElementState::Disabled];
        assert!(!indistinguishable(&vec![el("e1", "button", "导出"), disabled]));
    }

    #[tokio::test]
    async fn indistinguishable_candidates_are_declined_without_a_model_call() {
        let o = obs();
        // Two buttons named 导出: the arbiter has nothing to reason over.
        let same = vec![el("e1", "button", "导出"), el("e2", "button", "导出")];
        let (a, stub) = arbiter(
            ModelProposal { choice: Some("c1".into()), confidence: 0.99, ..Default::default() },
            0.7,
            8,
        );
        let note = declined(&a, &ctx(&o, &same)).await.expect("a decline");
        assert!(note.as_deref().unwrap_or("").contains("same role and name"), "says what to fix: {note:?}");
        assert_eq!(stub.calls.load(std::sync::atomic::Ordering::SeqCst), 0, "must not ask");
    }

    #[tokio::test]
    async fn a_confident_pick_becomes_an_index_into_the_candidates() {
        let o = obs();
        let (a, stub) = arbiter(
            ModelProposal { choice: Some("c2".into()), confidence: 0.9, ..Default::default() },
            0.7,
            8,
        );
        let c = picked(&a, &ctx(&o, &o.elements)).await;
        assert_eq!(c.index, 1);
        assert_eq!(c.confidence, 0.9);
        assert_eq!(stub.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn below_the_floor_it_declines_and_the_ambiguity_error_stands() {
        let o = obs();
        let (a, _) = arbiter(
            ModelProposal { choice: Some("c1".into()), confidence: 0.4, ..Default::default() },
            0.7,
            8,
        );
        let note = declined(&a, &ctx(&o, &o.elements)).await.expect("a decline");
        assert!(note.as_deref().unwrap_or("").contains("confident"), "says why: {note:?}");
    }

    #[tokio::test]
    async fn too_many_candidates_cost_no_model_call_at_all() {
        let o = obs();
        let many: Vec<ElementInfo> = (0..12).map(|i| el(&format!("e{i}"), "button", "导出")).collect();
        let (a, stub) = arbiter(
            ModelProposal { choice: Some("c1".into()), confidence: 0.99, ..Default::default() },
            0.7,
            8,
        );
        let note = declined(&a, &ctx(&o, &many)).await.expect("a decline");
        assert!(note.as_deref().unwrap_or("").contains("narrowing"), "says what to fix: {note:?}");
        assert_eq!(stub.calls.load(std::sync::atomic::Ordering::SeqCst), 0, "must not ask");
    }

    #[tokio::test]
    async fn an_unavailable_model_declines_rather_than_guessing() {
        let o = obs();
        let (mut a, _) = arbiter(ModelProposal::default(), 0.7, 8);
        // Rebuild with the failing stub through the constructor to keep the
        // fixture in one place.
        let stub = Arc::new(Stub {
            proposal: ModelProposal::default(),
            fail: true,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let router = ModelRouter::new(vec![stub as Arc<dyn ModelAdapter>], 5);
        a = ModelArbiter::new(Arc::new(crate::vision::Vision::new(router)), "g", 0.7, 8);
        let note = declined(&a, &ctx(&o, &o.elements)).await.expect("a decline");
        assert!(note.as_deref().unwrap_or("").contains("could not answer"), "says the model failed: {note:?}");
    }

    #[tokio::test]
    async fn candidates_are_offered_as_c1_cN_and_cross_reference_the_page_state() {
        let o = obs();
        let (a, _) = arbiter(
            ModelProposal { choice: Some("c1".into()), confidence: 0.9, ..Default::default() },
            0.7,
            8,
        );
        picked(&a, &ctx(&o, &o.elements)).await;
        let req = STASH.with(|s| s.borrow().clone()).expect("the stub was asked");
        let ids: Vec<&str> = req.criteria.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2"]);
        assert!(req.criteria[0].1.contains("导出 CSV"));
        // So a capable model can tie the candidate to the line it already read.
        assert!(req.criteria[0].1.contains("listed as e1"));
        assert_eq!(req.capability, Capability::Locate);
    }

    #[tokio::test]
    async fn an_invented_choice_is_rejected_before_it_can_act() {
        let o = obs();
        // The router validates ids; a model that answers "c9" gets no proposal
        // back at all, so the arbiter has nothing to act on.
        let (a, _) = arbiter(
            ModelProposal { choice: Some("c9".into()), confidence: 0.99, ..Default::default() },
            0.7,
            8,
        );
        // The router rejects the proposal before it reaches us, so the note
        // comes from that path — either way it names the id that was not one
        // of the candidates.
        let note = declined(&a, &ctx(&o, &o.elements)).await.expect("a decline");
        let note = note.unwrap_or_default();
        assert!(note.contains("c9") && note.contains("not a candidate"),
                "says the id was not offered: {note:?}");
    }

    #[tokio::test]
    async fn index_zero_from_a_model_is_not_treated_as_the_first_candidate() {
        let o = obs();
        let (a, _) = arbiter(
            ModelProposal { choice: Some("c0".into()), confidence: 0.99, ..Default::default() },
            0.7,
            8,
        );
        declined(&a, &ctx(&o, &o.elements)).await.expect("a decline");
    }
}
