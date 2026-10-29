//! Continuous interaction: a fixed-rate loop that samples page state, matches
//! rules, and dispatches input.
//!
//! The model is not in this loop. Rules are cheap JS predicates over the last
//! sample; the loop keeps running even if nobody is watching. That is the whole
//! point of the controller: 30 Hz cannot wait on a network round trip.
//!
//! Safety is not an afterthought here. Every exit path (goal, deadline,
//! cancel, bad policy, dispatch error) releases held input and leaves the page
//! a short reporting cycle to see the release before the browser goes away;
//! `max_actions_per_sec` is enforced rather than documented.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bh_action::Runtime;
use bh_browser::Session;
use bh_core::{BhError, ControllerPolicy, Result};
use bh_observe::{ObserveOptions, Observer};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;

use crate::vision::Vision;

/// How many consecutive evaluation failures mean the page is not what the
/// policy thinks it is. A single bad sample is noise; a run of them is a bug.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// How long the page gets to observe and report released input before the
/// caller may close the browser. Long enough for a 250 ms telemetry timer to
/// fire at least once; short enough not to matter next to the loop itself.
const RELEASE_SETTLE: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    GoalMet,
    Deadline,
    Canceled,
    Anomaly,
    TerminalRule,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControllerOutcome {
    pub why: StopReason,
    pub ticks: u64,
    pub samples: u64,
    pub actions: u64,
    pub elapsed_ms: u64,
    /// How many JS round trips the predicates cost. Sampling is one per sample;
    /// goal and rule predicates share one per tick (see `verdicts_fn`).
    pub evals: u64,
    /// Tick-start interval percentiles. A 30 Hz loop that spends 20 ms inside a
    /// rule acts like a slower one, and p95 shows that where the mean hides it.
    pub tick_ms_p50: u64,
    pub tick_ms_p95: u64,
    pub tick_ms_max: u64,
    /// Which policy version the loop was running when it stopped. Lets a
    /// caller that sent updates see whether the last one was adopted.
    pub policy_version: u64,
    /// How many times the model was asked and its answer accepted. A
    /// controller that decides often is a controller reacting slowly, and this
    /// is the number that says so.
    pub decisions: u64,
    /// Every round of asking, accepted or not. A count alone cannot be read:
    /// "2 accepted" might be one strategy confirmed twice or an argument the
    /// loop lost five times on the way.
    /// Per-rule activity, in the order the rules first fired. The aggregate
    /// `actions` cannot tell 20 jumps from 19 jumps plus a crash-and-restart —
    /// both are two key events apart. This can.
    pub decision_log: Vec<DecisionRecord>,
    pub rules: Vec<RuleActivity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How often one rule's actions were dispatched, and how many actions that
/// cost. A rule fires on entry, so `entries` is the meaningful count: one jump,
/// one restart, one release.
#[derive(Debug, Clone, Serialize)]
pub struct RuleActivity {
    pub id: String,
    pub entries: u64,
    pub actions: u64,
}

/// Count activity for `id`, appending the entry the first time it fires. A
/// policy update that renames a rule starts a new entry rather than merging.
fn bump(rules: &mut Vec<RuleActivity>, id: &str, actions: u64) {
    match rules.iter_mut().find(|r| r.id == id) {
        Some(r) => {
            r.entries += 1;
            r.actions += actions;
        }
        None => rules.push(RuleActivity { id: id.to_string(), entries: 1, actions }),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TickSummary {
    pub tick: u64,
    pub samples: u64,
    pub actions: u64,
    pub elapsed_ms: u64,
    pub policy_version: u64,
}

/// A decision the model made about which strategy the rules should follow.
#[derive(Debug, Clone, Serialize)]
pub struct ControllerDecision {
    pub id: String,
    pub confidence: f64,
    /// What the model said why, kept verbatim. Some backends put their own
    /// timings in here (the local sidecar reports its inference ms), which is
    /// how "the model was slow" is told apart from "the plumbing was slow".
    pub reason: Option<String>,
    pub model: String,
    /// Set by the asker from the page clock; unused by the loop, which stamps
    /// its own elapsed time so the log lines up with tick summaries.
    /// Page clock when the round was asked (the page's timeline decides
    /// whether a strategy arrived in time), and the model's own latency.
    pub asked_at_ms: u64,
    pub model_ms: u64,
    /// Which policy version's option list produced this. A policy update that
    /// renames an option invalidates an in-flight decision.
    pub policy_version: u64,
}

/// One round of asking, accepted or not. `decisions` alone cannot be read as
/// "the strategy held": two accepted answers might be the same strategy twice,
/// and a run with zero accepted answers might have had five rejected ones.
/// This is the log that makes the difference visible.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionRecord {
    /// Page clock at the moment the question was asked.
    pub asked_at_ms: u64,
    /// Model latency for this round.
    pub model_ms: u64,
    /// When the loop acted on (or refused) it, on the task clock.
    pub applied_at_ms: u64,
    pub id: String,
    pub confidence: f64,
    pub model: String,
    pub applied: bool,
    /// The strategy in force when this answer arrived (`null` on the first).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Runs the model beside the loop, never inside it.
///
/// A measured local decision costs 150–500 ms and a 30 Hz tick has 33. Asking
/// in-line would starve the controller; asking per tick would also spend a
/// budget the task does not have. So this task samples the page on its own
/// schedule, asks at most one question at a time, and publishes whatever it
/// learned. The loop applies the newest value and keeps running with the
/// previous one in the meantime — a stale strategy is a slow reaction, which
/// is a different failure from a stopped loop.
pub async fn decision_task(
    session: &Session,
    mut policy: watch::Receiver<ControllerPolicy>,
    mut cancel: watch::Receiver<bool>,
    vision: Arc<Vision>,
    out: tokio::sync::mpsc::UnboundedSender<ControllerDecision>,
) {
    // Pay the model's start-up cost before the first question, not inside it.
    // Measured: the local sidecar loads in 4.4 s, and paid on the first ask
    // that delay lands after the moment the decision was meant to act on —
    // a game decided in the first five seconds loses before the strategy
    // exists. Failures are logged and ignored: a model that cannot warm up may
    // still answer, and that error belongs to the call that needs it.
    vision.warm_for_decisions().await;

    loop {
        if *cancel.borrow() {
            return;
        }
        // Wait for a policy that asks for decisions. A policy with no `decide`
        // is the common case and must cost nothing.
        let cfg = loop {
            let candidate = policy.borrow().clone();
            if candidate.decide.is_some() {
                break candidate;
            }
            if policy.changed().await.is_err() || *cancel.borrow() {
                return;
            }
        };
        let decide = cfg.decide.clone().expect("checked above");

        // The sample is the controller's own observe script, so the model and
        // the rules reason about the same state text.
        // The page's own clock, so the log answers "was this decision early
        // enough for the game?" rather than "was it early relative to my loop".
        let asked_at = match session.evaluate("Math.round(performance.now())").await {
            Ok(v) => v.as_u64().unwrap_or(0),
            Err(_) => 0,
        };
        let round_start = Instant::now();
        match session.evaluate(&call_fn(&cfg.observe.script, None)).await {
            Ok(sample) if !sample.is_null() => {
                let state = sample.to_string();
                let options: Vec<(String, String)> = decide
                    .options
                    .iter()
                    .map(|o| (o.id.clone(), o.description.clone()))
                    .collect();
                // No extra timeout here on purpose. Only one question is ever
                // outstanding, so a slow answer cannot pile up; and charging a
                // sidecar's one-off model load (measured 4.4 s) against a
                // per-round budget would reject the first decision every time.
                // The adapter owns its own deadline and restarts a wedged
                // sidecar, which is the right place for it.
                match vision
                    .locate_candidates(&decide.question, &state, "", options)
                    .await
                {
                    Ok(r) => {
                        // The asker forwards anything that names an option —
                        // even an id outside the list, even one below any
                        // floor. It judges nothing: it read its config before
                        // the call, the policy may have changed since, and it
                        // cannot know which strategy is in force. Dropping
                        // answers here would also make a run where the model
                        // was refused five times look identical to one where it
                        // was asked twice.
                        let Some(chosen) = r.choice else {
                            tracing::info!("decision gave no choice; strategy unchanged");
                            continue;
                        };
                        let _ = out.send(ControllerDecision {
                            id: chosen,
                            reason: r.reason,
                            confidence: r.confidence,
                            model: r.model,
                            policy_version: cfg.policy_version,
                            asked_at_ms: asked_at,
                            model_ms: round_start.elapsed().as_millis() as u64,
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "decision model call failed; strategy unchanged"),
                }
            }
            other => {
                let why = match other {
                    Ok(_) => "the observe script returned nothing".to_string(),
                    Err(e) => e.to_string(),
                };
                tracing::warn!(error = %why, "decision skipped: no page state to ask about");
            }
        }

        // Wait for the next round, a policy change (new option list), or stop.
        // A closed channel is not a reason to stop asking: the loop re-checks
        // the cancel flag at the top, which is the authority on whether to run.
        tokio::select! {
            _ = tokio::time::sleep(decide.interval()) => {}
            _ = policy.changed() => {}
            r = cancel.changed() => {
                if r.is_err() || *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// Tick length for a policy. Clamped so a typo cannot turn the loop into a
/// busy spin or a once-a-minute sampler.
fn tick_every(policy: &ControllerPolicy) -> Duration {
    let hz = policy.tick_hz.clamp(1, 240) as u64;
    Duration::from_millis((1000 / hz).max(1))
}

/// Sampling gap. Never faster than the tick: one sample per tick is the limit.
fn sample_gap(policy: &ControllerPolicy) -> Duration {
    let hz = policy.observe.max_hz.clamp(1, policy.tick_hz.clamp(1, 240)) as u64;
    Duration::from_millis((1000 / hz).max(1))
}

fn action_limit(policy: &ControllerPolicy) -> usize {
    policy.safety.max_actions_per_sec.clamp(1, 10_000) as usize
}

/// Highest priority first; equal priorities keep the order the author wrote.
fn rule_order(rules: &[bh_core::ControlRule]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rules.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(rules[i].priority));
    order
}

/// The policy's own wall-clock budget in milliseconds, `0` meaning none.
fn budget_ms(policy: &ControllerPolicy) -> u64 {
    let goal = policy.goal.deadline_ms;
    let safety = policy.safety.max_duration_ms;
    match (goal, safety) {
        (0, 0) => 0,
        (0, s) => s,
        (g, 0) => g,
        (g, s) => g.min(s),
    }
}

/// A replacement policy may shorten the deadline but never push it out: a
/// live-updatable deadline would make `max_duration_ms` advisory.
fn tighten_deadline(current: Option<Instant>, started: Instant, policy: &ControllerPolicy) -> Option<Instant> {
    let ms = budget_ms(policy);
    match (current, ms) {
        (None, 0) => None,
        (None, ms) => Some(started + Duration::from_millis(ms)),
        (Some(d), 0) => Some(d),
        (Some(d), ms) => Some(d.min(started + Duration::from_millis(ms))),
    }
}

/// Equal or older versions are late updates from a client that has not seen
/// the last one; adopting them would undo a newer policy.
fn adopts(current: &ControllerPolicy, candidate: &ControllerPolicy) -> bool {
    candidate.policy_version > current.policy_version
}

pub async fn run(
    session: &Session,
    policy: &ControllerPolicy,
    cancel: Option<Arc<AtomicBool>>,
    on_summary: impl FnMut(&TickSummary),
) -> Result<ControllerOutcome> {
    run_with_updates(session, policy.clone(), None, cancel, on_summary, None).await
}

/// Like [`run`], but the caller can replace the policy mid-run. The controller
/// adopts an update only when its `policy_version` is strictly newer, and only
/// at a tick boundary: actions dispatched in the current tick belong to the
/// policy that was in force when the tick started.
pub async fn run_with_updates(
    session: &Session,
    mut policy: ControllerPolicy,
    mut updates: Option<watch::Receiver<ControllerPolicy>>,
    cancel: Option<Arc<AtomicBool>>,
    mut on_summary: impl FnMut(&TickSummary),
    mut decisions: Option<tokio::sync::mpsc::UnboundedReceiver<ControllerDecision>>,
) -> Result<ControllerOutcome> {
    let started = Instant::now();
    let mut deadline = tighten_deadline(None, started, &policy);
    let mut order = rule_order(&policy.rules);

    let mut observer = Observer::new(ObserveOptions { max_elements: 50 });
    let mut rt = Runtime::new(session, &mut observer);
    if let Some(flag) = cancel.clone() {
        rt = rt.with_cancel(flag);
    }

    let canceled = || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));

    let mut ticks = 0u64;
    let mut samples = 0u64;
    let mut actions = 0u64;
    let mut evals = 0u64;
    let mut tick_gaps: Vec<u64> = Vec::new();
    let mut last_tick_start = started;
    let mut last_sample: Option<Instant> = None;
    let mut state: Option<Value> = None;
    let mut consecutive_errors = 0u32;
    let mut last_summary = 0u64;
    // Fingerprint of the rule whose actions already ran: id plus the rule's
    // JSON. A policy update that leaves the rule alone does not re-fire it.
    let mut last_rule: Option<(String, Value)> = None;
    let mut window_start = started;
    let mut window_actions = 0usize;
    let mut rule_activity: Vec<RuleActivity> = Vec::new();
    // The strategy the model last asked for, and how many decisions it made.
    // Counted separately from actions because it is a different kind of cost.
    let mut chosen: Option<ControllerDecision> = None;
    let mut decision_count = 0u64;
    let mut decision_log: Vec<DecisionRecord> = Vec::new();

    // Pacing is measured from an anchor so a policy that changes `tick_hz`
    // restarts cleanly instead of inheriting the old cadence.
    let mut pace_tick = tick_every(&policy);
    let mut pace_anchor = started;
    let mut pace_from = 0u64;

    let finish = |why: StopReason, error: Option<String>, ticks, samples, actions, policy_version| {
        ControllerOutcome {
            why,
            ticks,
            samples,
            actions,
            elapsed_ms: started.elapsed().as_millis() as u64,
            evals: 0,
            tick_ms_p50: 0,
            tick_ms_p95: 0,
            tick_ms_max: 0,
            policy_version,
            decisions: 0,
            decision_log: Vec::new(),
            rules: Vec::new(),
            error,
        }
    };

    let mut stop: Option<ControllerOutcome> = None;

    while stop.is_none() {
        let tick_start = Instant::now();
        if ticks > 0 {
            tick_gaps.push(tick_start.duration_since(last_tick_start).as_millis() as u64);
        }
        last_tick_start = tick_start;
        // A policy swap is adopted at a tick boundary; the tick then runs under
        // exactly one version. Rules keep their entry semantics across a swap
        // when the matched rule did not change.
        if let Some(rx) = updates.as_mut() {
            if rx.has_changed().unwrap_or(false) {
                let candidate = rx.borrow_and_update().clone();
                if adopts(&policy, &candidate) {
                    let version = candidate.policy_version;
                    let rules = candidate.rules.len();
                    policy = candidate;
                    order = rule_order(&policy.rules);
                    deadline = tighten_deadline(deadline, started, &policy);
                    pace_tick = tick_every(&policy);
                    pace_anchor = Instant::now();
                    pace_from = ticks;
                    tracing::info!(policy_version = version, rules, "controller adopted a new policy");
                }
            }
        }

        let sample_gap = sample_gap(&policy);
        let limit = action_limit(&policy);
        let version = policy.policy_version;

        if canceled() {
            stop = Some(finish(StopReason::Canceled, None, ticks, samples, actions, version));
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            stop = Some(finish(StopReason::Deadline, None, ticks, samples, actions, version));
            break;
        }

        // Sample at most `max_hz`; rules are evaluated every tick against the
        // cached sample, so a 30 Hz loop does not become 30 JS round trips.
        let due = last_sample.is_none_or(|t| t.elapsed() >= sample_gap);
        if due && !policy.observe.script.is_empty() {
            last_sample = Some(Instant::now());
            match session.evaluate(&call_fn(&policy.observe.script, None)).await {
                Ok(v) => {
                    if v.is_null() {
                        consecutive_errors += 1;
                    } else {
                        state = Some(v);
                        samples += 1;
                        consecutive_errors = 0;
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors == 1 {
                        tracing::warn!(error = %e, "controller sample failed");
                    }
                }
            }
            if consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                stop = Some(finish(
                    StopReason::Anomaly,
                    Some(format!("observe script failed {consecutive_errors} times in a row")),
                    ticks,
                    samples,
                    actions,
                    version,
                ));
                break;
            }
        }

        // A model decision is part of the page state the rules read, so a rule
        // branches on it exactly like it branches on `s.x`: no new predicate
        // language, no race between "the model's view" and the page's.
        while let Some(mut d) = decisions.as_mut().and_then(|rx| rx.try_recv().ok()) {
            if d.policy_version != policy.policy_version {
                // The option list changed under this answer; it is not a
                // decision about the policy now in force.
                tracing::info!(
                    decision = %d.id,
                    decided_on = d.policy_version,
                    running = policy.policy_version,
                    "ignoring a decision from a superseded policy"
                );
                decision_log.push(DecisionRecord {
                    asked_at_ms: d.asked_at_ms,
                    model_ms: d.model_ms,
                    applied_at_ms: started.elapsed().as_millis() as u64,
                    id: d.id,
                    confidence: d.confidence,
                    model: d.model,
                    applied: false,
                    previous: chosen.as_ref().map(|c| c.id.clone()),
                    reason: Some(format!(
                        "answered on policy version {}, which is no longer in force",
                        d.policy_version
                    )),
                });
                continue;
            }
            // One page-clock read per arriving decision (rare), so the three
            // stamps on a record share one epoch. Mixing the task clock with
            // the page clock made `applied_at_ms` precede `asked_at_ms` —
            // impossible-looking, and the actual cause was that the page loads
            // after the task starts.
            let at_ms = match session.evaluate("Math.round(performance.now())").await {
                Ok(v) => v.as_u64().unwrap_or(0),
                Err(_) => 0,
            };
            let holding = chosen.as_ref().map(|c| c.id.clone());

            // An id the author never offered is not a strategy: the rules that
            // would act on it do not exist. Recorded rather than ignored,
            // because "the model keeps inventing options" is a diagnosis worth
            // being able to make from the trace.
            if !policy
                .decide
                .as_ref()
                .is_some_and(|c| c.options.iter().any(|o| o.id == d.id))
            {
                let id = d.id.clone();
                decision_log.push(DecisionRecord {
                    asked_at_ms: d.asked_at_ms,
                    model_ms: d.model_ms,
                    applied_at_ms: at_ms,
                    id: id.clone(),
                    confidence: d.confidence,
                    model: std::mem::take(&mut d.model),
                    applied: false,
                    previous: holding,
                    reason: Some("not one of this policy's options".to_string()),
                });
                tracing::info!(decision = %id, "decision named an option that does not exist");
                continue;
            }

            if let Some(c) = policy.decide.as_ref().filter(|c| d.confidence < c.min_confidence) {
                let (id, confidence, floor) = (d.id.clone(), d.confidence, c.min_confidence);
                tracing::info!(
                    decision = %id,
                    confidence,
                    floor,
                    "decision rejected: the strategy stays as it was"
                );
                decision_log.push(DecisionRecord {
                    asked_at_ms: d.asked_at_ms,
                    model_ms: d.model_ms,
                    applied_at_ms: at_ms,
                    id,
                    confidence,
                    model: std::mem::take(&mut d.model),
                    applied: false,
                    previous: holding,
                    reason: Some(format!("below min_confidence {floor}, it gave {confidence}")),
                });
                continue;
            }

            let changing = holding.as_deref().is_some_and(|cur| cur != d.id);

            // Hysteresis: changing direction costs more than confirming it.
            // Only the loop can do this — the asker does not know what is in
            // force, and a policy update that clears the strategy must not be
            // judged against a stale one.
            let switch_floor = match (changing, policy.decide.as_ref()) {
                (true, Some(c)) => c.switch_confidence,
                _ => None,
            };
            if let Some(sw) = switch_floor.filter(|floor| d.confidence < *floor) {
                let from = holding.clone().unwrap_or_else(|| "(nothing)".into());
                tracing::info!(
                    decision = %d.id,
                    confidence = d.confidence,
                    switch_floor = sw,
                    holding = %from,
                    "keeping the current strategy: a switch needs more conviction"
                );
                decision_log.push(DecisionRecord {
                    asked_at_ms: d.asked_at_ms,
                    model_ms: d.model_ms,
                    applied_at_ms: at_ms,
                    id: d.id,
                    confidence: d.confidence,
                    model: d.model,
                    applied: false,
                    previous: holding,
                    reason: Some(format!(
                        "switching away from {from} needs confidence {sw}, it gave {}",
                        d.confidence
                    )),
                });
                continue;
            }

            tracing::info!(
                decision = %d.id,
                confidence = d.confidence,
                model_ms = d.model_ms,
                page_ms = d.asked_at_ms,
                holding = holding.as_deref().unwrap_or("(none)"),
                switched = changing,
                "strategy applied"
            );
            decision_log.push(DecisionRecord {
                asked_at_ms: d.asked_at_ms,
                model_ms: d.model_ms,
                applied_at_ms: at_ms,
                id: d.id.clone(),
                confidence: d.confidence,
                model: d.model.clone(),
                applied: true,
                previous: holding,
                reason: d.reason.clone(),
            });
            chosen = Some(d);
            decision_count += 1;
        }
        let bound = chosen.as_ref().map(|d| {
            let mut o = state.clone().unwrap_or(Value::Null);
            if let Some(obj) = o.as_object_mut() {
                obj.insert("d".into(), Value::String(d.id.clone()));
                obj.insert("d_confidence".into(), Value::from(d.confidence));
            }
            o
        });

        if let Some(s) = bound.as_ref().or(state.as_ref()) {
            // One round trip for the goal and every rule predicate. A predicate
            // that throws is reported per predicate, not as a failed batch, so
            // the error accounting below is the same as one-call-per-predicate.
            let goal = policy.goal.script.trim();
            let whens: Vec<Option<&str>> = policy
                .rules
                .iter()
                .map(|r| {
                    let w = r.when.trim();
                    if w.is_empty() { None } else { Some(w) }
                })
                .collect();
            let mut rule_verdicts: Vec<Option<std::result::Result<Value, String>>> =
                vec![None; whens.len()];

            if !goal.is_empty() || whens.iter().any(Option::is_some) {
                evals += 1;
                match session
                    .evaluate(&verdicts_fn((!goal.is_empty()).then_some(goal), &whens, s))
                    .await
                {
                    Ok(v) => {
                        if let Some(g) = v.get("g").and_then(verdict) {
                            match g {
                                Ok(v) if truthy(&v) => {
                                    stop = Some(finish(
                                        StopReason::GoalMet,
                                        None,
                                        ticks,
                                        samples,
                                        actions,
                                        version,
                                    ));
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    consecutive_errors += 1;
                                    if consecutive_errors == 1 {
                                        tracing::warn!(error = %e, "controller goal failed");
                                    }
                                    if consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                                        stop = Some(finish(
                                            StopReason::Anomaly,
                                            Some(format!("goal script failed: {e}")),
                                            ticks,
                                            samples,
                                            actions,
                                            version,
                                        ));
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(rs) = v.get("r").and_then(Value::as_array) {
                            for (i, r) in rule_verdicts.iter_mut().enumerate() {
                                *r = rs.get(i).and_then(verdict);
                            }
                        }
                    }
                    Err(e) => {
                        // A transport failure is one event, not one per rule.
                        consecutive_errors += 1;
                        if consecutive_errors == 1 {
                            tracing::warn!(error = %e, "controller predicates failed");
                        }
                        if consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                            stop = Some(finish(
                                StopReason::Anomaly,
                                Some(format!("predicate batch failed: {e}")),
                                ticks,
                                samples,
                                actions,
                                version,
                            ));
                            break;
                        }
                    }
                }
            }

            // First matching rule wins. Rules that do not match are not errors:
            // a policy is allowed to describe more states than occur.
            let mut matched: Option<usize> = None;
            for &i in &order {
                let rule = &policy.rules[i];
                if rule.when.trim().is_empty() {
                    continue;
                }
                match rule_verdicts[i].take() {
                    Some(Ok(v)) if truthy(&v) => {
                        consecutive_errors = 0;
                        matched = Some(i);
                        break;
                    }
                    Some(Ok(_)) => consecutive_errors = 0,
                    Some(Err(e)) => {
                        consecutive_errors += 1;
                        if consecutive_errors == 1 {
                            tracing::warn!(rule = %rule.id, error = %e, "controller rule failed");
                        }
                        if consecutive_errors > MAX_CONSECUTIVE_ERRORS {
                            stop = Some(finish(
                                StopReason::Anomaly,
                                Some(format!("rule {:?} failed: {e}", rule.id)),
                                ticks,
                                samples,
                                actions,
                                version,
                            ));
                            break;
                        }
                    }
                    // No verdict: the batch did not run or did not answer this
                    // rule. Treat it as "did not match" rather than guessing.
                    None => consecutive_errors = 0,
                }
            }
            if stop.is_some() {
                break;
            }

            // Actions run on rule *entry*, not every tick: holding a key is
            // one key_down, not thirty per second. A rule that stops matching
            // re-arms, so re-entering it presses again.
            if let Some(i) = matched {
                let rule = &policy.rules[i];
                let fingerprint = serde_json::to_value(rule).unwrap_or(Value::Null);
                let fired = last_rule
                    .as_ref()
                    .is_some_and(|(id, fp)| id == &rule.id && fp == &fingerprint);
                if !fired {
                    let mut dispatched = 0u64;
                    for action in &rule.actions {
                        // Sliding one-second window: sleep rather than drop
                        // input, because a dropped key_up leaves a key down.
                        if window_actions >= limit {
                            let left =
                                Duration::from_secs(1).saturating_sub(window_start.elapsed());
                            tokio::time::sleep(left).await;
                            window_start = Instant::now();
                            window_actions = 0;
                        }
                        if canceled() {
                            stop = Some(finish(StopReason::Canceled, None, ticks, samples, actions, version));
                            break;
                        }
                        rt.act(action).await?;
                        actions += 1;
                        window_actions += 1;
                        dispatched += 1;
                    }
                    bump(&mut rule_activity, &rule.id, dispatched);
                    last_rule = Some((rule.id.clone(), fingerprint));
                }
                if stop.is_some() {
                    break;
                }
                if rule.terminal {
                    stop = Some(finish(StopReason::TerminalRule, None, ticks, samples, actions, version));
                    break;
                }
            } else {
                last_rule = None;
            }
        }

        ticks += 1;
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed.saturating_sub(last_summary) >= 1_000 {
            last_summary = elapsed;
            on_summary(&TickSummary {
                tick: ticks,
                samples,
                actions,
                elapsed_ms: elapsed,
                policy_version: version,
            });
        }

        // Pace to the next tick; if we are behind, do not burst to catch up.
        let target = pace_anchor + pace_tick * (ticks - pace_from) as u32;
        let left = target.saturating_duration_since(Instant::now());
        if !left.is_zero() {
            tokio::time::sleep(left).await;
        }
    }

    // Unconditional, on purpose. `release_all_on_exit` exists in the policy
    // format for forward compatibility, but a controller that can leave a key
    // down because a flag was false is not a safety feature.
    let _ = session.release_all_input().await;

    // The release is only real when the page has seen it. A page that records
    // input state on a timer (or posts a beacon from keyup) needs one reporting
    // cycle before the caller tears the browser down; without this wait the
    // final report can be lost with the process. `outcome.elapsed_ms` is
    // measured at `finish`, so the wait does not stretch the reported duration.
    tokio::time::sleep(RELEASE_SETTLE).await;

    let mut outcome = stop.ok_or_else(|| BhError::Other("controller stopped without a reason".into()))?;
    outcome.evals = evals;
    outcome.rules = rule_activity;
    outcome.decisions = decision_count;
    outcome.decision_log = decision_log;
    tick_gaps.sort_unstable();
    outcome.tick_ms_p50 = percentile(&tick_gaps, 50);
    outcome.tick_ms_p95 = percentile(&tick_gaps, 95);
    outcome.tick_ms_max = tick_gaps.last().copied().unwrap_or(0);
    if let Some(e) = &outcome.error {
        tracing::warn!(reason = ?outcome.why, error = %e, "controller stopped on an anomaly");
    }
    Ok(outcome)
}

/// Accepts both `() => ({...})` and a bare expression: a policy author who
/// writes one or the other should not get a silent `undefined` state.
fn call_fn(script: &str, arg: Option<&Value>) -> String {
    let script = script.trim();
    let arg = match arg {
        Some(v) => v.to_string(),
        None => "undefined".into(),
    };
    format!(
        "(() => {{ const __v = ({script}); return typeof __v === 'function' ? __v({arg}) : __v; }})()"
    )
}

/// Goal and every rule predicate in one JS round trip. At 30 Hz with five
/// rules the old loop cost six CDP calls per tick; this costs one. Each
/// predicate keeps its own try/catch, so a single bad expression does not
/// blind the tick, and each keeps the expression-or-function shape
/// `predicate_fn` accepts. Results arrive positionally: `g` for the goal and
/// `r[i]` for rule `i` (`null` where the rule has no predicate).
fn verdicts_fn(goal: Option<&str>, whens: &[Option<&str>], state: &Value) -> String {
    let state = state.to_string();
    let goal = goal.map(|g| format!("__ev({})", pred_src(g))).unwrap_or_else(|| "null".into());
    let rules: Vec<String> = whens
        .iter()
        .map(|w| w.map(|w| format!("__ev({})", pred_src(w))).unwrap_or_else(|| "null".into()))
        .collect();
    format!(
        "(() => {{ const s = {state}; \
         const __ev = (f) => {{ try {{ const v = f(); \
           const r = typeof v === 'function' ? v(s) : v; return {{ok: true, v: r}}; }} \
           catch (e) {{ return {{ok: false, e: String(e)}}; }} }}; \
         return {{g: {goal}, r: [{}]}}; }})()",
        rules.join(", ")
    )
}

fn pred_src(expr: &str) -> String {
    format!("() => ({expr})")
}

/// One predicate's verdict: the value, or the error the expression threw.
fn verdict(v: &Value) -> Option<std::result::Result<Value, String>> {
    let o = v.as_object()?;
    match o.get("ok")?.as_bool()? {
        true => Some(Ok(o.get("v").cloned().unwrap_or(Value::Null))),
        false => Some(Err(
            o.get("e").and_then(|e| e.as_str()).unwrap_or("predicate threw").to_string()
        )),
    }
}

/// Nearest-rank percentile over a small sample. `0` for an empty sample: a
/// controller that never completed two ticks has no interval to report.
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * p).div_ceil(100).clamp(1, sorted.len()) - 1;
    sorted[rank]
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_fn_wraps_arrow_and_bare_expressions() {
        assert!(call_fn("() => 1", None).contains("typeof __v === 'function'"));
        assert!(call_fn("window.x", None).contains("(window.x)"));
    }

    #[test]
    fn predicates_are_batched_into_one_script_and_bind_the_sample_as_s() {
        let js = verdicts_fn(Some("s.x < 100"), &[Some("s.y === 1"), None], &serde_json::json!({"x": 3}));
        assert!(js.contains("const s = {\"x\":3}"), "{js}");
        assert!(js.contains("__ev(() => (s.x < 100))"), "{js}");
        assert!(js.contains("__ev(() => (s.y === 1))"), "{js}");
        // A rule without a predicate is a hole, not a truthy arrow function.
        assert!(js.contains("r: [__ev(() => (s.y === 1)), null]"), "{js}");
        // Each predicate catches its own exception, so one cannot blind the tick.
        assert_eq!(js.matches("catch (e)").count(), 1);
    }

    #[test]
    fn a_goal_is_omitted_rather_than_evaluated_when_empty() {
        let js = verdicts_fn(None, &[Some("s.a")], &serde_json::json!({}));
        assert!(js.contains("return {g: null, r:"), "{js}");
    }

    #[test]
    fn verdicts_separate_values_from_thrown_errors() {
        let ok = verdict(&serde_json::json!({"ok": true, "v": 7})).unwrap();
        assert_eq!(ok, Ok(serde_json::json!(7)));
        let err = verdict(&serde_json::json!({"ok": false, "e": "boom"})).unwrap();
        assert_eq!(err, Err("boom".to_string()));
        assert!(verdict(&Value::Null).is_none(), "null is 'no answer', not an error");
    }

    #[test]
    fn percentiles_use_nearest_rank_and_never_index_past_the_sample() {
        let sample = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&sample, 50), 30);
        assert_eq!(percentile(&sample, 95), 50);
        assert_eq!(percentile(&sample, 100), 50);
        assert_eq!(percentile(&sample, 0), 10);
        assert_eq!(percentile(&[], 95), 0);
    }

    #[test]
    fn truthiness_matches_js_for_the_shapes_we_accept() {
        assert!(truthy(&serde_json::json!(true)));
        assert!(truthy(&serde_json::json!(1)));
        assert!(!truthy(&serde_json::json!(0)));
        assert!(!truthy(&Value::Null));
    }

    fn policy(version: u64) -> ControllerPolicy {
        ControllerPolicy { policy_version: version, ..Default::default() }
    }

    #[test]
    fn only_a_strictly_newer_policy_replaces_the_running_one() {
        assert!(adopts(&policy(1), &policy(2)));
        assert!(!adopts(&policy(2), &policy(2)), "a repeat of the current version is late, not new");
        assert!(!adopts(&policy(3), &policy(2)), "an older version must not undo a newer one");
    }

    #[test]
    fn an_update_can_tighten_the_deadline_but_never_extend_it() {
        let start = Instant::now();
        let mut p = policy(1);
        p.goal.deadline_ms = 5_000;
        let five = tighten_deadline(None, start, &p);
        assert_eq!(five, Some(start + Duration::from_millis(5_000)));

        let mut longer = policy(2);
        longer.goal.deadline_ms = 60_000;
        assert_eq!(tighten_deadline(five, start, &longer), five, "extension is refused");

        let mut shorter = policy(3);
        shorter.goal.deadline_ms = 2_500;
        assert_eq!(
            tighten_deadline(five, start, &shorter),
            Some(start + Duration::from_millis(2_500))
        );

        // A policy with no deadline cannot remove one that is already running.
        let none = policy(4);
        assert_eq!(tighten_deadline(five, start, &none), five);
    }

    #[test]
    fn the_deadline_is_the_tighter_of_goal_and_safety() {
        let mut p = policy(1);
        p.goal.deadline_ms = 3_000;
        p.safety.max_duration_ms = 2_000;
        assert_eq!(budget_ms(&p), 2_000);
        p.safety.max_duration_ms = 0;
        assert_eq!(budget_ms(&p), 3_000);
    }

    #[test]
    fn rules_run_in_priority_order_and_keep_written_order_on_ties() {
        let a = bh_core::ControlRule { id: "a".into(), priority: 1, ..Default::default() };
        let b = bh_core::ControlRule { id: "b".into(), priority: 9, ..Default::default() };
        let c = bh_core::ControlRule { id: "c".into(), priority: 1, ..Default::default() };
        // Written order a, b, c: b first (9), then a and c in that order (tie).
        let order = rule_order(&[a, b, c]);
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn tick_and_sampling_rates_are_clamped() {
        let mut p = policy(1);
        p.tick_hz = 0;
        p.observe.max_hz = 0;
        assert_eq!(tick_every(&p), Duration::from_millis(1000));
        assert_eq!(sample_gap(&p), Duration::from_millis(1000));

        p.tick_hz = 60;
        p.observe.max_hz = 240;
        assert_eq!(tick_every(&p), Duration::from_millis(16));
        assert_eq!(sample_gap(&p), Duration::from_millis(16), "sampling never outruns the tick");
    }
}
