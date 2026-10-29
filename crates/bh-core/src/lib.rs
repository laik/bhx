//! Protocol types shared by every crate.
//!
//! These mirror `docs/protocol.md`. Anything that crosses a crate boundary lives
//! here; nothing in this crate knows about CDP, tokio, or the filesystem.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bumped whenever the serialized form of an [`Observation`] changes.
/// The local decision model's thresholds are measured against this format.
pub const STATE_FORMAT_VERSION: u32 = 1;

pub type Result<T> = std::result::Result<T, BhError>;

#[derive(Debug, thiserror::Error)]
pub enum BhError {
    #[error("browser: {0}")]
    Browser(String),
    #[error("cdp: {0}")]
    Cdp(String),
    #[error("locator not found: {0}")]
    LocatorNotFound(String),
    #[error("locator ambiguous: {0} candidates matched")]
    LocatorAmbiguous(usize),
    #[error("element not actionable: {0}")]
    NotActionable(String),
    #[error("action failed: {0}")]
    Action(String),
    #[error("condition not met: {0}")]
    Condition(String),
    #[error("timeout after {0} ms")]
    Timeout(u64),
    #[error("canceled")]
    Canceled,
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for BhError {
    fn from(e: serde_json::Error) -> Self {
        BhError::Cdp(format!("json: {e}"))
    }
}

// ---------------------------------------------------------------- geometry

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Viewport {
    pub w: f64,
    pub h: f64,
    pub dpr: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self { w: 1280.0, h: 800.0, dpr: 1.0, scroll_x: 0.0, scroll_y: 0.0 }
    }
}

// ---------------------------------------------------------------- observation

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementState {
    Enabled,
    Disabled,
    Visible,
    Hidden,
    Checked,
    Focused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementInfo {
    pub eid: String,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub bbox: Rect,
    #[serde(default)]
    pub states: Vec<ElementState>,
    pub hash: u64,

    /// Internal: how to reach the node through CDP. Never sent to a model.
    #[serde(skip)]
    pub backend_node_id: i64,
    #[serde(skip)]
    pub frame_id: String,
}

impl ElementInfo {
    pub fn is_enabled(&self) -> bool {
        !self.states.contains(&ElementState::Disabled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    pub seq: u64,
    pub state_format_version: u32,
    pub url: String,
    pub title: String,
    pub viewport: Viewport,
    pub elements: Vec<ElementInfo>,
    pub truncated: bool,
}

impl Observation {
    pub fn find(&self, eid: &str) -> Option<&ElementInfo> {
        self.elements.iter().find(|e| e.eid == eid)
    }

    /// Compact, stable, token-cheap rendering. This is the exact text a decision
    /// model sees, so changes here invalidate measured thresholds.
    pub fn to_state_text(&self, goal: &str) -> String {
        let mut s = format!("Task: {goal}\nURL: {}\nTitle: {}\nControls:\n", self.url, self.title);
        for e in &self.elements {
            let name = if e.name.is_empty() { "(unnamed)" } else { e.name.as_str() };
            s.push_str(&format!("{}  {} \"{}\"", e.eid, e.role, name));
            if let Some(v) = &e.value {
                s.push_str(&format!(" value=\"{v}\""));
            }
            if !e.is_enabled() {
                s.push_str(" disabled");
            }
            s.push('\n');
        }
        s
    }
}

// ---------------------------------------------------------------- locators

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum LocatorKind {
    Role {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        exact: bool,
    },
    Label { label: String },
    TestId { test_id: String },
    Placeholder { placeholder: String },
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    Css { css: String },
    XPath { xpath: String },
}

impl LocatorKind {
    pub fn strategy(&self) -> &'static str {
        match self {
            LocatorKind::Role { .. } => "role+name",
            LocatorKind::Label { .. } => "label",
            LocatorKind::TestId { .. } => "test_id",
            LocatorKind::Placeholder { .. } => "placeholder",
            LocatorKind::Text { .. } => "text",
            LocatorKind::Css { .. } => "css",
            LocatorKind::XPath { .. } => "xpath",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Locator {
    pub any_of: Vec<LocatorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<Box<Locator>>,
}

/// Accepts both the canonical `{"any_of":[{"role":"button"}]}` and the
/// shorthand every human (and model) writes: `{"role":"button","name":"导出"}`.
/// The shorthand must not deserialize as an empty `any_of` — that would resolve
/// to nothing and look like "element not found" instead of a bad request.
impl<'de> Deserialize<'de> for Locator {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Full {
                any_of: Vec<LocatorKind>,
                #[serde(default)]
                within: Option<Box<Locator>>,
            },
            Kind(LocatorKind),
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Full { any_of, within } => Locator { any_of, within },
            Repr::Kind(kind) => Locator::new(kind),
        })
    }
}

impl Locator {
    pub fn new(kind: LocatorKind) -> Self {
        Self { any_of: vec![kind], within: None }
    }

    pub fn role(role: &str, name: &str) -> Self {
        Self::new(LocatorKind::Role {
            role: role.into(),
            name: Some(name.into()),
            exact: false,
        })
    }

    pub fn css(sel: &str) -> Self {
        Self::new(LocatorKind::Css { css: sel.into() })
    }

    pub fn label(text: &str) -> Self {
        Self::new(LocatorKind::Label { label: text.into() })
    }
}

/// Accepts a locator (`{"any_of":[...]}` or the single-kind shorthand
/// `{"role":"button","name":"Export"}`), a raw point (`{"x":1,"y":2}`), or a
/// visual description (`{"visual":"the red button"}`) that is resolved from a
/// screenshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Target {
    Visual {
        /// What to find in the picture, in words a grounding model can use.
        visual: String,
    },
    Locator(Locator),
    Point(Point),
}

impl Target {
    pub fn locator(&self) -> Option<Locator> {
        match self {
            Target::Locator(l) => Some(l.clone()),
            Target::Visual { .. } | Target::Point(_) => None,
        }
    }

    pub fn visual(&self) -> Option<&str> {
        match self {
            Target::Visual { visual } => Some(visual),
            _ => None,
        }
    }
}

impl From<Locator> for Target {
    fn from(l: Locator) -> Self {
        Target::Locator(l)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub observation_id: String,
    pub eid: String,
    pub frame_id: String,
    pub backend_node_id: i64,
    pub bbox: Rect,
    pub strategy: &'static str,
    pub enabled: bool,
    /// Set when a model picked this element out of several matches. Carried on
    /// the target so every action that uses it can report the guess.
    pub arbitration: Option<Arbitration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// True only when a success condition was supplied *and* satisfied. An
    /// action that merely dispatched without error is not verified.
    pub verified: bool,
    pub eid: String,
    pub strategy: String,
    pub elapsed_ms: u64,
    /// The value produced by an extract action. `None` for everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Set when a model chose between several matching elements: what was
    /// asked, which candidates were offered, what it picked and how sure it
    /// was. An action that acted on a guess must be identifiable after the
    /// fact, or a wrong click looks like a correct one in the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbitration: Option<serde_json::Value>,
}

impl ActionResult {
    pub fn ok(eid: impl Into<String>, strategy: &str) -> Self {
        Self {
            verified: false,
            eid: eid.into(),
            strategy: strategy.to_string(),
            elapsed_ms: 0,
            value: None,
            arbitration: None,
        }
    }

    pub fn with_value(mut self, value: serde_json::Value) -> Self {
        self.value = Some(value);
        self
    }

    pub fn with_arbitration(mut self, a: Option<Arbitration>) -> Self {
        self.arbitration = a.map(|a| serde_json::to_value(a).unwrap_or(serde_json::Value::Null));
        self
    }

    pub fn verified(mut self) -> Self {
        self.verified = true;
        self
    }

    pub fn with_elapsed(mut self, d: Duration) -> Self {
        self.elapsed_ms = d.as_millis() as u64;
        self
    }
}

#[derive(Debug, Clone)]
pub enum ResolveOutcome {
    Unique(ResolvedTarget),
    Ambiguous { candidates: Vec<ElementInfo>, strategy: &'static str },
    NotFound { tried: Vec<&'static str> },
}

// ---------------------------------------------------------------- actions

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Alt,
    Control,
    Meta,
    Shift,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Click {
        target: Target,
        #[serde(default)]
        button: MouseButton,
    },
    Hover {
        target: Target,
    },
    Type {
        target: Target,
        text: String,
        #[serde(default)]
        clear_first: bool,
    },
    Press {
        key: String,
        #[serde(default)]
        modifiers: Vec<Modifier>,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    Scroll {
        #[serde(default)]
        delta: Point,
        #[serde(default)]
        target: Option<Target>,
    },
    /// Press at `from`, move in steps, release at `to`. Both ends may be a
    /// locator, a point or a visual description, so a canvas knob can be found
    /// in a screenshot and a drop zone in the same one.
    Drag {
        from: Target,
        to: Target,
        #[serde(default = "drag_steps")]
        steps: u32,
    },
    Navigate {
        url: String,
    },
    WaitFor {
        condition: Condition,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Screenshot {
        #[serde(default)]
        name: Option<String>,
    },
    Evaluate {
        script: String,
    },
    /// Reads structured data out of the page. The script must evaluate to JSON;
    /// the step runner writes that value into the task's artifacts, because a
    /// result nobody can read later is not a result.
    Extract {
        name: String,
        script: String,
    },
}

// ---------------------------------------------------------------- conditions

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    UrlMatches { pattern: String },
    ElementVisible { locator: Locator },
    ElementTextMatches { locator: Locator, regex: String },
    ElementCount { locator: Locator, op: CmpOp, value: usize },
    Js { script: String },
    Not { of: Box<Condition> },
    All { of: Vec<Condition> },
    Any { of: Vec<Condition> },
    /// Evaluated by the step runner, not by the action runtime: it has to arm
    /// the download watcher before the action that triggers it.
    DownloadCompleted {
        filename_regex: String,
        #[serde(default = "one")]
        min_bytes: u64,
    },
}

fn one() -> u64 {
    1
}

fn drag_steps() -> u32 {
    8
}

impl Condition {
    pub fn url_contains(p: &str) -> Self {
        Condition::UrlMatches { pattern: p.into() }
    }

    pub fn is_download(&self) -> bool {
        matches!(self, Condition::DownloadCompleted { .. })
    }
}

// ---------------------------------------------------------------- tasks

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Workflow,
    Interactive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    #[default]
    ReadOnly,
    Reversible,
    Irreversible,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    #[default]
    Auto,
    Dom,
    Vision,
    Interactive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    #[default]
    Escalate,
    Reobserve,
    AskModel,
    Abort,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub on_exhausted: OnExhausted,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 1, backoff_ms: 500, on_exhausted: OnExhausted::Escalate }
    }
}

/// What to do when the semantic check cannot confirm the goal at the caller's
/// confidence threshold. There is no "assume achieved": the three choices are
/// to ask a human, fail, or explicitly accept an uncertain result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnUncertain {
    #[default]
    WaitHuman,
    Fail,
    Continue,
}

/// Optional second opinion after every step's mechanical `success` condition
/// has passed: the model reads the final page and judges the task goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticVerify {
    /// Empty means "use the task goal".
    pub goal: Option<String>,
    /// A verdict below this confidence becomes `uncertain`.
    pub threshold: f64,
    pub on_uncertain: OnUncertain,
    /// Send a PNG of the viewport with the question. Off by default: the image
    /// is exactly what was on screen, so enabling it hands the page (and
    /// anything personal on it) to the verifier model.
    pub include_screenshot: bool,
}

impl Default for SemanticVerify {
    fn default() -> Self {
        Self {
            goal: None,
            threshold: 0.6,
            on_uncertain: OnUncertain::WaitHuman,
            include_screenshot: false,
        }
    }
}

impl SemanticVerify {
    pub fn goal<'a>(&'a self, task_goal: &'a str) -> &'a str {
        self.goal
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(task_goal)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservePolicy {
    pub max_elements: usize,
    pub include_screenshot: bool,
}

impl Default for ObservePolicy {
    fn default() -> Self {
        Self { max_elements: 120, include_screenshot: false }
    }
}

/// What an arbiter decided, kept on the action so a guessed click is
/// distinguishable from a named one after the fact.
#[derive(Debug, Clone, Serialize)]
pub struct Arbitration {
    pub strategy: String,
    pub eid: String,
    pub model: String,
    pub confidence: f64,
    pub candidates: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Everything an arbiter is allowed to see: the locator as written, the exact
/// observation the candidates came from, and the candidates in resolution
/// order. An arbiter that re-observed the page could choose an element that
/// was never in question.
#[derive(Debug, Clone, Copy)]
pub struct ArbitrationContext<'a> {
    pub asked: &'a str,
    pub observation: &'a Observation,
    pub options: &'a [ElementInfo],
    pub strategy: &'static str,
}

/// An arbiter's pick: an index into `options`, plus what it cost to decide.
/// Indexed rather than by `eid` because DOM-level candidates have no element id.
#[derive(Debug, Clone)]
pub struct ArbiterChoice {
    pub index: usize,
    pub model: String,
    pub confidence: f64,
    pub reason: Option<String>,
}

/// An arbiter's answer. Declining carries a note because the three reasons to
/// decline need different fixes — a locator the page cannot disambiguate, a
/// guess below the floor, and an unreachable model are not the same problem,
/// and the operator should not have to guess which one they hit.
#[derive(Debug, Clone)]
pub enum ArbitrationDecision {
    Pick(ArbiterChoice),
    Decline { note: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AmbiguityArbiter {
    /// A pick below this is refused and the locator error stands as before.
    /// Higher than the verify threshold on purpose: here a wrong answer acts
    /// on the wrong element, it does not merely mislabel an outcome.
    pub min_confidence: f64,
    /// Candidate ceiling. Past this the choice set is too wide to be reliable
    /// (a selector matching 500 nodes is a modelling error, not a question).
    pub max_candidates: usize,
}

impl Default for AmbiguityArbiter {
    fn default() -> Self {
        Self { min_confidence: 0.7, max_candidates: 8 }
    }
}

impl AmbiguityArbiter {
    /// A misconfigured arbiter must not silently become a permissive one.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.min_confidence.is_finite() || !(0.0..=1.0).contains(&self.min_confidence) {
            return Err(format!(
                "arbitrate_ambiguous.min_confidence {} is outside 0..1",
                self.min_confidence
            ));
        }
        if self.max_candidates == 0 || self.max_candidates > 32 {
            return Err(format!(
                "arbitrate_ambiguous.max_candidates {} is outside 1..=32",
                self.max_candidates
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub max_model_calls: u32,
    pub max_wall_ms: u64,
    pub allow_irreversible: bool,
    /// Whether `confirm()` / `prompt()` boxes may be accepted without a human.
    /// Off by default: a confirm is often the last gate before something
    /// destructive, and clicking through it silently is exactly the failure
    /// mode this harness exists to avoid.
    pub accept_confirm: bool,
    pub allowed_hosts: Vec<String>,
    pub observation: ObservePolicy,
    /// Off by default. Mechanical `success` conditions still apply; this asks
    /// one extra question about whether the result is actually right.
    pub semantic_verify: Option<SemanticVerify>,
    /// When a locator matches several elements, ask a model to pick one instead
    /// of failing. Off by default: "ambiguous means stop" is the safe reading,
    /// and a model guessing between two similar controls is how the wrong button
    /// gets pressed. When set, a pick below `min_confidence` still fails — the
    /// only change is that a *confident* pick is allowed through, and it is
    /// always recorded on the action event.
    pub arbitrate_ambiguous: Option<AmbiguityArbiter>,
    /// Capture a JPEG of the page at the end of every step. Failed steps are
    /// captured regardless; this opts into the full sequence, which is what
    /// makes a trace replayable rather than just readable. Off by default:
    /// a screenshot is a CDP round-trip and the bulkiest file in a task home.
    pub trace_screenshots: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_model_calls: 2,
            max_wall_ms: 600_000,
            allow_irreversible: false,
            accept_confirm: false,
            allowed_hosts: Vec::new(),
            observation: ObservePolicy::default(),
            semantic_verify: None,
            arbitrate_ambiguous: None,
            trace_screenshots: false,
        }
    }
}

impl Policy {
    /// Empty list means "no restriction", matching the protocol.
    pub fn host_allowed(&self, url: &str) -> bool {
        host_allowed(url, &self.allowed_hosts)
    }
}

/// Shared by the task policy and the executor's repair guard so a repaired
/// navigation cannot be checked by a second, subtly different parser.
pub fn host_allowed(url: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host_no_port = host.split(':').next().unwrap_or(host);
    allowed
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host) || h.eq_ignore_ascii_case(host_no_port))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Step {
    pub id: String,
    pub goal: String,
    pub executor: ExecutorKind,
    pub preconditions: Vec<Condition>,
    pub actions: Vec<Action>,
    pub success: Option<Condition>,
    /// Conditions that mean this attempt already failed, so the runner can
    /// stop waiting for `success` and retry now instead of burning the whole
    /// step timeout. Only consulted while waiting on `success`.
    pub fail_when: Vec<Condition>,
    /// Description the vision executor may use if a DOM locator in this step
    /// resolves to nothing. Only applies when `executor` is `auto`: an explicit
    /// `dom` step must fail rather than route around the DOM.
    pub fallback_visual: Option<String>,
    pub timeout_ms: u64,
    pub retry: RetryPolicy,
    pub side_effect: SideEffect,
    pub requires_approval: bool,
}

impl Default for Step {
    fn default() -> Self {
        Self {
            id: String::new(),
            goal: String::new(),
            executor: ExecutorKind::Auto,
            preconditions: Vec::new(),
            actions: Vec::new(),
            success: None,
            fail_when: Vec::new(),
            fallback_visual: None,
            timeout_ms: 30_000,
            retry: RetryPolicy::default(),
            side_effect: SideEffect::ReadOnly,
            requires_approval: false,
        }
    }
}

impl Step {
    /// Approval is required either explicitly or implicitly: irreversible work
    /// is never done without a policy that opted in.
    pub fn needs_approval(&self, policy: &Policy) -> bool {
        self.requires_approval
            || (self.side_effect == SideEffect::Irreversible && !policy.allow_irreversible)
    }
}

/// Continuous interaction: the loop runs inside the harness at a fixed rate,
/// so the model is not in the control path. See docs/protocol.md §9.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerPolicy {
    pub policy_version: u64,
    pub tick_hz: u32,
    /// Where the controller starts. A controller with no page to drive is a
    /// configuration mistake, not a task.
    pub start_url: Option<String>,
    pub observe: ControllerObserve,
    /// Optional: a model picks one of the named options at a low rate, and the
    /// chosen id appears in the sampled state as `s.d`. Rules then branch on
    /// it. This is the only place a model is allowed inside a controller loop,
    /// and it is deliberately not per-tick: a local decision model costs a few
    /// hundred ms, a 30 Hz loop has 33.
    pub decide: Option<ControllerDecide>,
    pub goal: ControllerGoal,
    pub rules: Vec<ControlRule>,
    pub safety: SafetyPolicy,
}

impl Default for ControllerPolicy {
    fn default() -> Self {
        Self {
            policy_version: 1,
            tick_hz: 30,
            start_url: None,
            observe: ControllerObserve::default(),
            decide: None,
            goal: ControllerGoal::default(),
            rules: Vec::new(),
            safety: SafetyPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerObserve {
    /// Only `js` today: a script evaluated in the page returns the state the
    /// rules see. It runs at most `max_hz` times per second, not every tick.
    pub source: String,
    pub script: String,
    pub max_hz: u32,
}

impl Default for ControllerObserve {
    fn default() -> Self {
        Self { source: "js".into(), script: String::new(), max_hz: 10 }
    }
}

/// A low-frequency model decision. The options are authored, not discovered:
/// the model chooses between strategies the task author already wrote rules
/// for, so an unexpected answer cannot invent behaviour the policy does not
/// handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerDecide {
    /// What the model is asked, in the author's words.
    pub question: String,
    /// `id` is what rules compare against (`s.d === "…"`); `description` is
    /// what the model reads. At least two options.
    pub options: Vec<DecisionOption>,
    /// How often to ask, in ms. Clamped up: asking more often than the model
    /// can answer just produces stale decisions and a wasted call.
    pub every_ms: u64,
    /// Below this the previous decision is kept rather than replaced.
    pub min_confidence: f64,
    /// A higher bar for **changing** strategy than for confirming it. Absent
    /// means no hysteresis: one confident-enough answer replaces the current.
    ///
    /// This exists because of a measured failure, not as general hardening: in
    /// the plane scenario Laya chose correctly on the first round and then
    /// switched away before the scoring window opened — three runs, three lost
    /// games. A static probe showed why one threshold cannot catch that: the
    /// same state and the same correct answer scored 0.690 in one option order
    /// and 0.511 in another, so a switch routinely clears a lone floor for
    /// reasons that have nothing to do with the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_confidence: Option<f64>,
}

impl Default for ControllerDecide {
    fn default() -> Self {
        Self {
            question: String::new(),
            options: Vec::new(),
            every_ms: 1_000,
            min_confidence: 0.5,
            switch_confidence: None,
        }
    }
}

impl ControllerDecide {
    /// Checked before the job starts, and again on a live policy update: an
    /// option list of one is not a decision, and a sub-100 ms cadence cannot
    /// be honoured by any model we have measured.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.question.trim().is_empty() {
            return Err("controller.decide.question is empty".into());
        }
        if self.options.len() < 2 {
            return Err(format!(
                "controller.decide needs at least two options (got {}); \
                 with one there is nothing to ask",
                self.options.len()
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for o in &self.options {
            if o.id.trim().is_empty() {
                return Err("controller.decide.options has an empty id".into());
            }
            if !o.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!(
                    "decision id {:?} must be [A-Za-z0-9_-]+: it appears in rule predicates",
                    o.id
                ));
            }
            if o.description.trim().is_empty() {
                return Err(format!("decision option {:?} has no description", o.id));
            }
            if seen.contains(&o.id.as_str()) {
                return Err(format!("duplicate decision id {:?}", o.id));
            }
            seen.push(o.id.as_str());
        }
        if !(0.0..=1.0).contains(&self.min_confidence) || !self.min_confidence.is_finite() {
            return Err(format!(
                "controller.decide.min_confidence {} is outside 0..1",
                self.min_confidence
            ));
        }
        if let Some(sw) = self.switch_confidence {
            if !sw.is_finite() || !(0.0..=1.0).contains(&sw) {
                return Err(format!(
                    "controller.decide.switch_confidence {sw} is outside 0..1"
                ));
            }
            if sw < self.min_confidence {
                return Err(format!(
                    "controller.decide.switch_confidence {sw} is below min_confidence {}; \
                     hysteresis has to be the stricter of the two or it does nothing",
                    self.min_confidence
                ));
            }
        }
        Ok(())
    }

    /// The rate decisions are asked at, floored to something a model can meet.
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.every_ms.max(250))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionOption {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerGoal {
    /// JS predicate over the sampled state: `(s) => s.x >= 100`.
    pub script: String,
    pub deadline_ms: u64,
}

impl Default for ControllerGoal {
    fn default() -> Self {
        Self { script: String::new(), deadline_ms: 30_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlRule {
    pub id: String,
    /// JS predicate over the sampled state. An error skips the rule and counts
    /// as an error; too many in a row fails the task.
    pub when: String,
    /// `do` is a keyword in Rust, not in the protocol.
    #[serde(rename = "do")]
    pub actions: Vec<Action>,
    pub priority: i32,
    /// Stop the loop after this rule's actions have been dispatched.
    pub terminal: bool,
}

impl Default for ControlRule {
    fn default() -> Self {
        Self { id: String::new(), when: String::new(), actions: Vec::new(), priority: 0, terminal: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyPolicy {
    pub max_actions_per_sec: u32,
    pub max_duration_ms: u64,
    pub release_all_on_exit: bool,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self { max_actions_per_sec: 60, max_duration_ms: 30_000, release_all_on_exit: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskSpec {
    pub protocol_version: String,
    pub id: Option<String>,
    pub kind: TaskKind,
    pub goal: String,
    pub mode: String,
    pub policy: Policy,
    pub steps: Vec<Step>,
    /// Page opened before planning when `steps` is empty and `mode = auto`.
    /// Scripted tasks navigate from their own first step; this field is not a
    /// second navigation mechanism for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    /// Present when `kind = interactive`; steps are empty in that case.
    pub controller: Option<ControllerPolicy>,
    /// Secrets referenced from actions as `${cred:name}`. Never persisted and
    /// never logged; the engine strips this map before writing task.json.
    pub credentials: std::collections::BTreeMap<String, String>,
}

impl Default for TaskSpec {
    fn default() -> Self {
        Self {
            protocol_version: "0.1".into(),
            id: None,
            kind: TaskKind::Workflow,
            goal: String::new(),
            mode: "auto".into(),
            policy: Policy::default(),
            steps: Vec::new(),
            start_url: None,
            controller: None,
            credentials: std::collections::BTreeMap::new(),
        }
    }
}

impl TaskSpec {
    /// Replace `${cred:name}` occurrences with values held in memory.
    pub fn resolve_credentials(&self) -> Self {
        let mut out = self.clone();
        for step in &mut out.steps {
            for action in &mut step.actions {
                if let Action::Type { text, .. } = action {
                    *text = substitute(text, &self.credentials);
                }
            }
        }
        if let Some(controller) = &mut out.controller {
            for rule in &mut controller.rules {
                for action in &mut rule.actions {
                    if let Action::Type { text, .. } = action {
                        *text = substitute(text, &self.credentials);
                    }
                }
            }
        }
        out
    }
}

impl TaskSpec {
    /// Credential names this spec references as `${cred:name}`. Only names are
    /// returned; a scheduler can store them and resolve values from the
    /// environment at run time, so no secret ever reaches disk.
    pub fn credential_names(&self) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for step in &self.steps {
            for action in &step.actions {
                if let Action::Type { text, .. } = action {
                    cred_names_in(text, &mut names);
                }
            }
        }
        if let Some(controller) = &self.controller {
            for rule in &controller.rules {
                for action in &rule.actions {
                    if let Action::Type { text, .. } = action {
                        cred_names_in(text, &mut names);
                    }
                }
            }
        }
        names.into_iter().collect()
    }
}

fn cred_names_in(text: &str, out: &mut std::collections::BTreeSet<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("${cred:") {
        let after = &rest[start + "${cred:".len()..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        if !name.is_empty() {
            out.insert(name.to_string());
        }
        rest = &after[end + 1..];
    }
}

fn substitute(text: &str, creds: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (k, v) in creds {
        out = out.replace(&format!("${{cred:{k}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_accepts_three_shapes() {
        let a: Target = serde_json::from_str(r##"{"any_of":[{"css":"#x"}]}"##).unwrap();
        let b: Target = serde_json::from_str(r#"{"role":"button","name":"Export"}"#).unwrap();
        let c: Target = serde_json::from_str(r#"{"x":10,"y":20}"#).unwrap();
        let d: Target = serde_json::from_str(r#"{"visual":"the red button"}"#).unwrap();
        assert_eq!(a.locator().unwrap().any_of.len(), 1);
        // The shorthand must not arrive as an empty `any_of`.
        let b = b.locator().expect("shorthand is a locator");
        assert_eq!(b.any_of.len(), 1);
        assert!(matches!(b.any_of[0], LocatorKind::Role { .. }));
        assert!(matches!(c, Target::Point(_)));
        assert_eq!(d.visual(), Some("the red button"));
    }

    #[test]
    fn action_roundtrip() {
        let a: Action = serde_json::from_str(
            r#"{"type":"click","target":{"role":"button","name":"Export CSV"}}"#,
        )
        .unwrap();
        assert!(matches!(a, Action::Click { .. }));
        let j = serde_json::to_string(&a).unwrap();
        let back: Action = serde_json::from_str(&j).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn state_text_is_stable() {
        let obs = Observation {
            id: "obs_1".into(),
            seq: 1,
            state_format_version: STATE_FORMAT_VERSION,
            url: "http://x/".into(),
            title: "t".into(),
            viewport: Viewport::default(),
            truncated: false,
            elements: vec![ElementInfo {
                eid: "e1".into(),
                role: "button".into(),
                name: "Export".into(),
                value: None,
                bbox: Rect::default(),
                states: vec![ElementState::Enabled],
                hash: 0,
                backend_node_id: 1,
                frame_id: "main".into(),
            }],
        };
        assert!(obs.to_state_text("do it").contains("e1  button \"Export\""));
    }

    fn decide(question: &str, opts: &[(&str, &str)], floor: f64) -> ControllerDecide {
        ControllerDecide {
            question: question.into(),
            options: opts
                .iter()
                .map(|(id, d)| DecisionOption { id: (*id).into(), description: (*d).into() })
                .collect(),
            every_ms: 1000,
            min_confidence: floor,
            switch_confidence: None,
        }
    }

    #[test]
    fn deciding_is_off_unless_the_author_asks_for_it() {
        let p: ControllerPolicy = serde_json::from_str(
            r#"{"tick_hz":30,"observe":{"script":"x"},"goal":{"script":"y"},"rules":[]}"#,
        )
        .unwrap();
        assert!(p.decide.is_none(), "a rule-only controller is the default");
    }

    #[test]
    fn a_decision_needs_at_least_two_real_options() {
        assert!(decide("which lane?", &[("left", "the left lane")], 0.5)
            .validate()
            .unwrap_err()
            .contains("at least two"));
        assert!(decide("", &[("left", "a"), ("right", "b")], 0.5)
            .validate()
            .unwrap_err()
            .contains("question is empty"));
        assert!(decide("q", &[("left", ""), ("right", "b")], 0.5)
            .validate()
            .unwrap_err()
            .contains("no description"));
        assert!(decide("q", &[("left", "a"), ("left", "b")], 0.5)
            .validate()
            .unwrap_err()
            .contains("duplicate"));
        assert!(decide("q", &[("left", "a"), ("right", "b")], 1.5)
            .validate()
            .unwrap_err()
            .contains("outside 0..1"));
    }

    #[test]
    fn an_option_id_must_survive_being_written_into_a_predicate() {
        // Rules compare against `s.d`, so an id with a quote or a space would
        // produce a predicate that cannot parse — caught at submit, not at tick.
        for bad in ["left lane", "left'x", "左", "left;drop"] {
            let e = decide("q", &[(bad, "a"), ("right", "b")], 0.5)
                .validate()
                .unwrap_err();
            assert!(e.contains("must be [A-Za-z0-9_-]+"), "{bad:?} -> {e}");
        }
        assert!(decide("q", &[("cover-left_2", "a"), ("right", "b")], 0.5)
            .validate()
            .is_ok());
    }

    #[test]
    fn an_asked_cadence_below_what_any_model_can_meet_is_raised_not_honoured() {
        // 10 ms asks are a plausible typo; running them would only ever reuse
        // the previous answer. Clamping keeps the intent, drops the fiction.
        assert_eq!(decide("q", &[("a", "1"), ("b", "2")], 0.5).interval().as_millis(), 1000);
        assert_eq!(
            ControllerDecide { every_ms: 10, ..decide("q", &[("a", "1"), ("b", "2")], 0.5) }
                .interval()
                .as_millis(),
            250
        );
    }

    #[test]
    fn hysteresis_must_be_the_stricter_of_the_two_floors() {
        // A switch floor below the confirm floor is not hysteresis; it is a
        // second, looser gate that can never bind. Refusing it at submit beats
        // a task silently doing nothing the author thinks it asked for.
        let e = ControllerDecide {
            switch_confidence: Some(0.3),
            ..decide("q", &[("left", "a"), ("right", "b")], 0.6)
        }
        .validate()
        .unwrap_err();
        assert!(e.contains("stricter of the two"), "{e}");
        assert!(e.contains("0.3") && e.contains("0.6"), "names both numbers: {e}");

        let e = ControllerDecide {
            switch_confidence: Some(1.5),
            ..decide("q", &[("left", "a"), ("right", "b")], 0.6)
        }
        .validate()
        .unwrap_err();
        assert!(e.contains("switch_confidence") && e.contains("outside 0..1"), "{e}");

        // Equal is allowed: it means "a switch needs at least as much
        // conviction as staying", which is a weaker but coherent policy.
        assert!(ControllerDecide {
            switch_confidence: Some(0.6),
            ..decide("q", &[("left", "a"), ("right", "b")], 0.6)
        }
        .validate()
        .is_ok());
        assert!(ControllerDecide {
            switch_confidence: Some(0.9),
            ..decide("q", &[("left", "a"), ("right", "b")], 0.6)
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn a_controller_policy_defaults_to_no_decisions_at_all() {
        let p = ControllerPolicy::default();
        assert!(p.decide.is_none(), "rule-only controllers stay the default");
    }

    fn spec_from(json: &str) -> TaskSpec {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn credential_names_covers_steps_and_controller_rules() {
        let spec = spec_from(
            r##"{
              "goal": "log in",
              "steps": [{"id": "s1", "actions": [
                {"type":"type","target":{"css":"#u"},"text":"admin"},
                {"type":"type","target":{"css":"#p"},"text":"${cred:password}"}
              ]}],
              "controller": {"goal": {"script": "true"}, "rules": [{"id":"r1","when":"true","do":[
                {"type":"type","target":{"css":"#t"},"text":"tok ${cred:token} ${cred:password}"}
              ]}]}
            }"##,
        );
        assert_eq!(spec.credential_names(), vec!["password".to_string(), "token".to_string()]);
    }

    #[test]
    fn credential_names_ignores_plain_text_and_unclosed_references() {
        let spec = spec_from(
            r##"{"steps":[{"id":"s1","actions":[
                {"type":"type","target":{"css":"#x"},"text":"no refs here"},
                {"type":"type","target":{"css":"#y"},"text":"${cred:unterminated"},
                {"type":"type","target":{"css":"#z"},"text":"${cred:}"}
            ]}]}"##,
        );
        assert!(spec.credential_names().is_empty(), "only well-formed, non-empty names count");
    }

    #[test]
    fn names_survive_the_environment_roundtrip() {
        // What a scheduler stores: the spec minus values, plus the names.
        let mut spec = spec_from(
            r##"{"steps":[{"id":"s1","actions":[
                {"type":"type","target":{"css":"#p"},"text":"${cred:password}"}
            ]}]}"##,
        );
        let names = spec.credential_names();
        assert_eq!(names, vec!["password".to_string()]);
        spec.credentials.insert(names[0].clone(), "s3cret".into());
        let resolved = spec.resolve_credentials();
        match &resolved.steps[0].actions[0] {
            Action::Type { text, .. } => assert_eq!(text, "s3cret"),
            other => panic!("expected a type action, got {other:?}"),
        }
    }
}
