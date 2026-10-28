//! One page under programmatic control.
//!
//! The MCP `browser_*` tools and the REST `/v1/sessions/{id}/*` endpoints are
//! the same operations over different transports. Keeping the operations here
//! means the two front doors cannot drift apart — the isolated-world default
//! for `evaluate`, the way an action log is rendered, and the screenshot
//! encoding are decided once.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bh_action::Runtime;
use bh_browser::{Browser, BrowserOptions, Session};
use bh_core::{Action, BhError, Condition, Result, Viewport};
use bh_observe::{ObserveOptions, Observer};
use serde_json::Value;

pub struct Page {
    browser: Browser,
    session: Session,
    observer: Observer,
}

/// One observation plus the optional screenshot the caller asked for. A failed
/// screenshot is reported, not raised: the element index is still usable.
pub struct Observed {
    pub text: String,
    pub truncated: bool,
    pub elements: usize,
    pub screenshot: Option<Vec<u8>>,
    pub screenshot_error: Option<String>,
}

/// What a batch of actions did. `failed` is true when the batch stopped at the
/// first error; `log` keeps the successful prefix so the caller can see where.
pub struct Acted {
    pub log: String,
    pub failed: bool,
}

impl Page {
    pub async fn open(artifacts: PathBuf, headless: bool) -> Result<Self> {
        std::fs::create_dir_all(&artifacts)
            .map_err(|e| BhError::Other(format!("artifacts dir {}: {e}", artifacts.display())))?;
        let opts = BrowserOptions {
            headless,
            download_dir: Some(artifacts),
            viewport: Viewport::default(),
            ..Default::default()
        };
        let browser = Browser::launch(opts.clone()).await?;
        let session = browser.new_session(&opts).await?;
        Ok(Self {
            browser,
            session,
            observer: Observer::new(ObserveOptions::default()),
        })
    }

    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Disjoint borrows for the step runner, which wants all three at once.
    pub fn parts_mut(&mut self) -> (&Browser, &Session, &mut Observer) {
        (&self.browser, &self.session, &mut self.observer)
    }

    pub async fn navigate(&self, url: &str) -> Result<(String, String)> {
        self.session.navigate(url).await?;
        Ok((
            self.session.url().await.unwrap_or_else(|_| url.to_string()),
            self.session.title().await.unwrap_or_default(),
        ))
    }

    pub async fn observe(
        &mut self,
        goal: &str,
        max_elements: Option<usize>,
        screenshot: bool,
    ) -> Result<Observed> {
        if let Some(max) = max_elements {
            self.observer.set_max_elements(max as usize);
        }
        let obs = self.observer.observe(&self.session).await?;
        let mut text = obs.to_state_text(goal);
        if obs.truncated {
            text.push_str("(truncated; raise max_elements to see more)\n");
        }
        let (png, shot_error) = match screenshot {
            true => match self.session.screenshot().await {
                Ok(bytes) => (Some(bytes), None),
                Err(e) => (None, Some(e.to_string())),
            },
            false => (None, None),
        };
        Ok(Observed {
            text,
            truncated: obs.truncated,
            elements: obs.elements.len(),
            screenshot: png,
            screenshot_error: shot_error,
        })
    }

    /// Dispatch actions in order. Stops at the first failure; the log keeps
    /// what already happened so a partial batch is not mistaken for none.
    pub async fn act(
        &mut self,
        actions: &[Action],
        observe_after: bool,
        cancel: Arc<AtomicBool>,
    ) -> Result<Acted> {
        let mut rt = Runtime::new(&self.session, &mut self.observer).with_cancel(cancel);
        let mut log = String::new();
        let mut failed = false;
        for (i, action) in actions.iter().enumerate() {
            match rt.act(action).await {
                Ok(result) => log.push_str(&format!(
                    "[{i}] ok {} {} {} ms\n",
                    result.strategy, result.eid, result.elapsed_ms
                )),
                Err(e) => {
                    log.push_str(&format!("[{i}] failed: {e}\n"));
                    failed = true;
                    break;
                }
            }
        }
        if !failed && observe_after {
            match self.observer.observe(&self.session).await {
                Ok(obs) => log.push_str(&obs.to_state_text("")),
                Err(e) => log.push_str(&format!("(observe failed: {e})\n")),
            }
        }
        Ok(Acted { log, failed })
    }

    /// `timeout_ms = 0` samples once; a positive timeout waits until the
    /// condition holds or the deadline passes. A timeout is a `false`, not an
    /// error — "it is not true yet" is an answer.
    pub async fn check(
        &mut self,
        cond: &Condition,
        timeout_ms: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<bool> {
        let mut rt = Runtime::new(&self.session, &mut self.observer).with_cancel(cancel);
        if timeout_ms == 0 {
            rt.check(cond).await
        } else {
            match rt.wait_for(cond, timeout_ms).await {
                Ok(()) => Ok(true),
                Err(BhError::Timeout(_)) => Ok(false),
                Err(e) => Err(e),
            }
        }
    }

    pub async fn verify(&mut self, goal: &str, threshold: f64) -> Result<bh_model::VerifyReport> {
        if goal.trim().is_empty() {
            return Err(BhError::Other("verify goal is empty".into()));
        }
        if !(0.0..=1.0).contains(&threshold) {
            return Err(BhError::Other(format!(
                "verify threshold {threshold} is outside 0..1"
            )));
        }
        let obs = self.observer.observe(&self.session).await?;
        let state = obs.to_state_text(goal);
        let router = bh_model::ModelRouter::from_env(1);
        if !router.is_configured() {
            return Err(BhError::Other(
                "no model adapter configured for Verify; set BH_LLM_URL (or BH_VLM_URL)".into(),
            ));
        }
        bh_model::verify(&router, goal, &state, threshold, None).await
    }

    /// `world` is `"isolated"` (the default for caller-supplied JS) or
    /// `"main"` when the caller explicitly needs the page's own globals.
    pub async fn evaluate(&self, script: &str, world: &str) -> Result<Value> {
        match world {
            "isolated" => self.session.evaluate_isolated(script).await,
            "main" => self.session.evaluate(script).await,
            other => Err(BhError::Other(format!(
                "world must be \"isolated\" or \"main\", got {other:?}"
            ))),
        }
    }

    /// JPEG, sized for a model's context. The MCP tools return this.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.session.screenshot().await
    }

    /// Lossless PNG for callers that will measure pixels rather than look at
    /// the image; the REST screenshot endpoint uses this.
    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        self.session.screenshot_png().await
    }

    pub fn shutdown(&mut self) {
        self.browser.shutdown();
    }
}
