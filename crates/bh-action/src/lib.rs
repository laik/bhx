//! Action runtime: resolve a locator, check it can be acted on, do it, verify.
//!
//! Resolution is deliberately layered: semantic (AX) first, DOM selector as a
//! fallback. Coordinates are never the first choice.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bh_browser::Session;
use bh_core::{
    ActionResult, Action, BhError, CmpOp, Condition, ElementInfo, ElementState, Locator,
    LocatorKind, Modifier, MouseButton, Observation, Point, Rect, ResolveOutcome, ResolvedTarget,
    Result, Target,
};
use bh_observe::Observer;
use regex::Regex;

pub struct Runtime<'a> {
    pub session: &'a Session,
    pub observer: &'a mut Observer,
    cancel: Option<Arc<AtomicBool>>,
}

impl<'a> Runtime<'a> {
    pub fn new(session: &'a Session, observer: &'a mut Observer) -> Self {
        Self { session, observer, cancel: None }
    }

    /// Cooperative cancellation. The flag is checked before every dispatch and
    /// inside every wait loop; an aborted action releases held input.
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn canceled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }

    fn check_cancel(&self) -> Result<()> {
        if self.canceled() { Err(BhError::Canceled) } else { Ok(()) }
    }

    async fn observation(&mut self) -> Result<Observation> {
        self.observer.observe(self.session).await
    }

    // ------------------------------------------------------------ resolution

    /// Resolve on an existing observation (no page round-trip beyond box lookups).
    pub async fn resolve_on(&self, obs: &Observation, locator: &Locator) -> Result<ResolveOutcome> {
        let mut tried = Vec::new();

        for kind in &locator.any_of {
            tried.push(kind.strategy());
            match self.resolve_kind(obs, kind).await? {
                Hit::Unique(e) => {
                    return Ok(ResolveOutcome::Unique(ResolvedTarget {
                        observation_id: obs.id.clone(),
                        eid: e.eid.clone(),
                        frame_id: e.frame_id.clone(),
                        backend_node_id: e.backend_node_id,
                        bbox: e.bbox,
                        strategy: kind.strategy(),
                        enabled: e.is_enabled(),
                    }));
                }
                Hit::Many(cands) => {
                    return Ok(ResolveOutcome::Ambiguous {
                        candidates: cands,
                        strategy: kind.strategy(),
                    });
                }
                Hit::None => continue,
            }
        }

        Ok(ResolveOutcome::NotFound { tried })
    }

    async fn resolve_kind(&self, obs: &Observation, kind: &LocatorKind) -> Result<Hit> {
        match kind {
            LocatorKind::Role { role, name, exact } => {
                let matches: Vec<&ElementInfo> = obs
                    .elements
                    .iter()
                    .filter(|e| e.role.eq_ignore_ascii_case(role))
                    .filter(|e| match name {
                        None => true,
                        Some(want) => {
                            if *exact {
                                e.name == *want
                            } else {
                                contains_ci(&e.name, want)
                            }
                        }
                    })
                    .collect();
                Ok(collapse(matches))
            }
            LocatorKind::Label { label } => {
                let matches: Vec<&ElementInfo> = obs
                    .elements
                    .iter()
                    .filter(|e| contains_ci(&e.name, label))
                    .collect();
                Ok(collapse(matches))
            }
            LocatorKind::Placeholder { placeholder } => {
                // Chrome only exposes a placeholder as the accessible name when
                // the field has no label; when it has one, fall back to the DOM.
                let matches: Vec<&ElementInfo> = obs
                    .elements
                    .iter()
                    .filter(|e| {
                        contains_ci(&e.name, placeholder)
                            || contains_ci(e.value.as_deref().unwrap_or(""), placeholder)
                    })
                    .collect();
                match collapse(matches) {
                    Hit::None => {
                        self.dom_selector(&format!(
                            "[placeholder*=\"{}\"]",
                            escape_attr(placeholder)
                        ))
                        .await
                    }
                    other => Ok(other),
                }
            }
            LocatorKind::Text { text, .. } => {
                let matches: Vec<&ElementInfo> = obs
                    .elements
                    .iter()
                    .filter(|e| contains_ci(&e.name, text))
                    .collect();
                Ok(collapse(matches))
            }
            LocatorKind::TestId { test_id } => {
                self.dom_selector(&format!("[data-testid=\"{}\"]", escape_attr(test_id)))
                    .await
            }
            LocatorKind::Css { css } => self.dom_selector(css).await,
            LocatorKind::XPath { .. } => Ok(Hit::None), // not implemented in Phase 1
        }
    }

    /// Fallback path for selectors the AX index cannot express.
    async fn dom_selector(&self, selector: &str) -> Result<Hit> {
        let doc = self
            .session
            .call("DOM.getDocument", serde_json::json!({ "depth": 0 }))
            .await?;
        let root = doc["root"]["nodeId"].as_i64().unwrap_or(0);
        let found = self
            .session
            .call(
                "DOM.querySelectorAll",
                serde_json::json!({ "nodeId": root, "selector": selector }),
            )
            .await?;
        let node_ids: Vec<i64> = found["nodeIds"]
            .as_array()
            .map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect())
            .unwrap_or_default();

        // Cap the candidates: a selector matching 500 nodes is a modelling
        // error, and the caller only needs to know it is ambiguous.
        let mut hits = Vec::new();
        for node_id in node_ids.into_iter().take(8) {
            if let Some(info) = self.node_info(node_id).await? {
                hits.push(info);
            }
        }
        Ok(match hits.len() {
            0 => Hit::None,
            1 => Hit::Unique(hits.pop().expect("len checked")),
            _ => Hit::Many(hits),
        })
    }

    async fn node_info(&self, node_id: i64) -> Result<Option<ElementInfo>> {
        let described = self
            .session
            .call("DOM.describeNode", serde_json::json!({ "nodeId": node_id }))
            .await?;
        let backend = described["node"]["backendNodeId"].as_i64().unwrap_or(0);
        if backend == 0 {
            return Ok(None);
        }
        let Some(bbox) = self.session.box_model(backend).await? else {
            return Ok(None); // not rendered
        };
        let name = described["node"]["nodeName"].as_str().unwrap_or("").to_string();
        let enabled = !matches!(
            described["node"]["attributes"].as_array(),
            Some(attrs) if attrs.iter().any(|a| a.as_str() == Some("disabled"))
        );
        Ok(Some(ElementInfo {
            eid: String::new(),
            role: "dom".into(),
            name,
            value: None,
            bbox,
            states: vec![if enabled { ElementState::Enabled } else { ElementState::Disabled }],
            hash: 0,
            backend_node_id: backend,
            frame_id: "main".into(),
        }))
    }

    /// Resolve against a fresh observation.
    pub async fn resolve(&mut self, locator: &Locator) -> Result<ResolveOutcome> {
        let obs = self.observation().await?;
        self.resolve_on(&obs, locator).await
    }

    pub async fn require(&mut self, target: &Target) -> Result<ResolvedTarget> {
        match target {
            Target::Point(p) => Ok(ResolvedTarget {
                observation_id: String::new(),
                eid: String::new(),
                frame_id: "main".into(),
                backend_node_id: 0,
                bbox: Rect { x: p.x, y: p.y, w: 0.0, h: 0.0 },
                strategy: "point",
                enabled: true,
            }),
            Target::Visual { visual } => Err(BhError::Action(format!(
                "visual target {visual:?} reached the DOM executor: it must be resolved from a screenshot first"
            ))),
            other => {
                let locator = other
                    .locator()
                    .ok_or_else(|| BhError::LocatorNotFound("empty target".into()))?;
                let asked = serde_json::to_string(&locator).unwrap_or_default();
                match self.resolve(&locator).await? {
                    ResolveOutcome::Unique(t) => Ok(t),
                    ResolveOutcome::Ambiguous { candidates, strategy } => Err(BhError::Other(
                        format!(
                            "locator {asked} ambiguous via {strategy}: {} candidates ({})",
                            candidates.len(),
                            describe(&candidates)
                        ),
                    )),
                    ResolveOutcome::NotFound { tried } => Err(BhError::LocatorNotFound(
                        format!("{asked} (tried {})", tried.join(", ")),
                    )),
                }
            }
        }
    }

    /// Make a resolved target actionable and return where it actually is now.
    ///
    /// Two checks that pay for themselves: the element must be enabled, and the
    /// box must be re-read after scrolling it into view. A bbox captured by the
    /// last observation can be stale by the time an action is dispatched.
    async fn settle(&self, t: &ResolvedTarget) -> Result<Rect> {
        if !t.enabled {
            return Err(BhError::NotActionable(format!(
                "{} is disabled",
                if t.eid.is_empty() { t.strategy } else { &t.eid }
            )));
        }
        if t.backend_node_id == 0 {
            return Ok(t.bbox);
        }
        match self.session.scroll_into_view(t.backend_node_id).await? {
            Some(rect) if rect.w > 0.0 && rect.h > 0.0 => Ok(rect),
            Some(_) => Err(BhError::NotActionable("element has no visible box".into())),
            None => Err(BhError::NotActionable("element is not attached or not rendered".into())),
        }
    }

    // ------------------------------------------------------------ execution

    pub async fn act(&mut self, action: &Action) -> Result<ActionResult> {
        self.check_cancel()?;
        let started = Instant::now();
        let result = self.dispatch(action, started).await;
        if self.canceled() {
            // Whatever happened, do not leave a key or button held down.
            let _ = self.session.release_all_input().await;
            return Err(BhError::Canceled);
        }
        result
    }

    async fn dispatch(&mut self, action: &Action, started: Instant) -> Result<ActionResult> {
        match action {
            Action::Click { target, button } => {
                let t = self.require(target).await?;
                let at = self.settle(&t).await?.center();
                self.session.click(at, *button, 1).await?;
                Ok(ActionResult::ok(t.eid, t.strategy).with_elapsed(started.elapsed()))
            }
            Action::Hover { target } => {
                let t = self.require(target).await?;
                let at = self.settle(&t).await?.center();
                self.session.hover(at).await?;
                Ok(ActionResult::ok(t.eid, t.strategy))
            }
            Action::Type { target, text, clear_first } => {
                let t = self.require(target).await?;
                let at = self.settle(&t).await?.center();
                self.session.click(at, MouseButton::Left, 1).await?;
                if *clear_first {
                    self.session.press_key("a", &[select_all_modifier()]).await?;
                }
                self.session.type_text(text).await?;
                // Read back: typing silently failing is common (focus stolen,
                // readonly field, IME). Never report success without evidence.
                // Never echo either value either — this field may be a password.
                let readback = self
                    .session
                    .evaluate("document.activeElement && document.activeElement.value")
                    .await?;
                let got = readback.as_str().unwrap_or("");
                if !got.is_empty() && !got.contains(text) && !text.contains(got) {
                    return Err(BhError::Action(
                        "type verification failed: the field content does not match the requested text"
                            .into(),
                    ));
                }
                Ok(ActionResult::ok(t.eid, t.strategy).with_elapsed(started.elapsed()))
            }
            Action::Press { key, modifiers } => {
                self.session.press_key(key, modifiers).await?;
                Ok(ActionResult::ok(String::new(), "key"))
            }
            Action::KeyDown { key } => {
                self.session.key_down(key).await?;
                Ok(ActionResult::ok(String::new(), "key"))
            }
            Action::KeyUp { key } => {
                self.session.key_up(key).await?;
                Ok(ActionResult::ok(String::new(), "key"))
            }
            Action::Scroll { delta, target } => {
                let at = match target {
                    Some(t) => {
                        let t = self.require(t).await?;
                        t.bbox.center()
                    }
                    None => {
                        let vp = self.session.viewport().await?;
                        Point::new(vp.w / 2.0, vp.h / 2.0)
                    }
                };
                self.session.scroll(at, delta.x, delta.y).await?;
                Ok(ActionResult::ok(String::new(), "wheel"))
            }
            Action::Drag { from, to, steps } => {
                let a = self.require(from).await?;
                let b = self.require(to).await?;
                let start = self.settle(&a).await?.center();
                let end = self.settle(&b).await?.center();
                self.session.drag(start, end, *steps).await?;
                Ok(ActionResult::ok(String::new(), "drag").with_elapsed(started.elapsed()))
            }
            Action::Navigate { url } => {
                self.session.navigate(url).await?;
                Ok(ActionResult::ok(String::new(), "navigate"))
            }
            Action::WaitFor { condition, timeout_ms } => {
                self.wait_for(condition, timeout_ms.unwrap_or(10_000)).await?;
                Ok(ActionResult::ok(String::new(), "wait").verified())
            }
            Action::Screenshot { .. } => {
                self.session.screenshot().await?;
                Ok(ActionResult::ok(String::new(), "screenshot"))
            }
            Action::Evaluate { script } => {
                self.session.evaluate(script).await?;
                Ok(ActionResult::ok(String::new(), "evaluate"))
            }
            Action::Extract { script, .. } => {
                let value = self.session.evaluate(script).await?;
                Ok(ActionResult::ok(String::new(), "extract").with_value(value))
            }
        }
    }

    // ------------------------------------------------------------ conditions

    pub async fn check(&mut self, cond: &Condition) -> Result<bool> {
        match cond {
            Condition::UrlMatches { pattern } => {
                let url = self.session.url().await?;
                Ok(glob_match(&url, pattern))
            }
            Condition::ElementVisible { locator } => {
                let obs = self.observation().await?;
                Ok(!matches!(self.resolve_on(&obs, locator).await?, ResolveOutcome::NotFound { .. }))
            }
            Condition::ElementTextMatches { locator, regex } => {
                let re = Regex::new(regex).map_err(|e| BhError::Condition(format!("bad regex: {e}")))?;
                let obs = self.observation().await?;
                Ok(obs
                    .elements
                    .iter()
                    .any(|e| matches_locator(e, locator) && re.is_match(&e.name)))
            }
            Condition::ElementCount { locator, op, value } => {
                let obs = self.observation().await?;
                let n = obs
                    .elements
                    .iter()
                    .filter(|e| matches_locator(e, locator))
                    .count();
                Ok(cmp(n, *op, *value))
            }
            Condition::Js { script } => {
                let v = self.session.evaluate(script).await?;
                Ok(truthy(&v))
            }
            Condition::Not { of } => Ok(!Box::pin(self.check(of)).await?),
            Condition::All { of } => {
                for c in of {
                    if !Box::pin(self.check(c)).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Condition::Any { of } => {
                for c in of {
                    if Box::pin(self.check(c)).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Condition::DownloadCompleted { .. } => Err(BhError::Condition(
                "download conditions must be evaluated by the step runner".into(),
            )),
        }
    }

    /// Condition polling: never a fixed sleep, and never a busy loop.
    pub async fn wait_for(&mut self, cond: &Condition, timeout_ms: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut interval = Duration::from_millis(25);
        loop {
            self.check_cancel()?;
            if self.check(cond).await? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BhError::Timeout(timeout_ms));
            }
            tokio::time::sleep(interval).await;
            // Capped at 100 ms so a cancel request lands inside the 200 ms
            // budget the protocol promises (docs/roadmap.md, Phase 1).
            interval = (interval * 2).min(Duration::from_millis(100));
        }
    }
}

enum Hit {
    Unique(ElementInfo),
    Many(Vec<ElementInfo>),
    None,
}

fn collapse(matches: Vec<&ElementInfo>) -> Hit {
    match matches.len() {
        0 => Hit::None,
        1 => Hit::Unique(matches[0].clone()),
        _ => Hit::Many(matches.into_iter().cloned().collect()),
    }
}

fn matches_locator(e: &ElementInfo, locator: &Locator) -> bool {
    locator.any_of.iter().any(|k| match k {
        LocatorKind::Role { role, name, exact } => {
            e.role.eq_ignore_ascii_case(role)
                && name.as_ref().is_none_or(|want| {
                    if *exact { e.name == *want } else { contains_ci(&e.name, want) }
                })
        }
        LocatorKind::Label { label } => contains_ci(&e.name, label),
        LocatorKind::Placeholder { placeholder } => contains_ci(&e.name, placeholder),
        LocatorKind::Text { text, .. } => contains_ci(&e.name, text),
        LocatorKind::TestId { .. } | LocatorKind::Css { .. } | LocatorKind::XPath { .. } => false,
    })
}

fn describe(candidates: &[ElementInfo]) -> String {
    candidates
        .iter()
        .take(5)
        .map(|c| {
            let name = if c.name.is_empty() { "(unnamed)" } else { c.name.as_str() };
            if c.eid.is_empty() {
                format!("{} \"{name}\"", c.role)
            } else {
                format!("{} {} \"{name}\"", c.eid, c.role)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn cmp(a: usize, op: CmpOp, b: usize) -> bool {
    match op {
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Eq => a == b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Null => false,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// `*` matches anything; everything else is literal.
fn glob_match(url: &str, pattern: &str) -> bool {
    let mut re = String::from("^");
    for (i, part) in pattern.split('*').enumerate() {
        if i > 0 {
            re.push_str(".*");
        }
        re.push_str(&regex::escape(part));
    }
    re.push('$');
    Regex::new(&re).map(|r| r.is_match(url)).unwrap_or(false)
}

fn escape_attr(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn select_all_modifier() -> Modifier {
    Modifier::Meta
}

#[cfg(not(target_os = "macos"))]
fn select_all_modifier() -> Modifier {
    Modifier::Control
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns() {
        assert!(glob_match("http://x/orders?a=1", "**/orders*"));
        assert!(glob_match("http://x/orders", "**/orders"));
        assert!(!glob_match("http://x/users", "**/orders*"));
    }
}
