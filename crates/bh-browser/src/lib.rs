//! Browser runtime: process lifecycle, sessions, navigation, input, screenshots.
//!
//! Nothing here knows about tasks, locators, or models. It owns CDP and turns it
//! into a small, boring API.

use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use bh_cdp::{Cdp, Event};
use bh_core::{BhError, MouseButton, Point, Rect, Result, Viewport};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

const DEFAULT_CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

#[derive(Debug, Clone)]
pub struct BrowserOptions {
    pub executable: Option<String>,
    pub headless: bool,
    pub user_data_dir: Option<PathBuf>,
    pub viewport: Viewport,
    pub download_dir: Option<PathBuf>,
    /// Accept `confirm`/`prompt` dialogs instead of dismissing them.
    pub accept_confirm: bool,
    /// Attach to a Chrome that is already running instead of launching one.
    /// `http(s)://host:port` is resolved through `/json/version`, a `ws(s)://`
    /// URL is used as given, and a bare `host:port` means `http://`.
    /// `BH_CDP_URL` is consulted when this is `None`.
    pub cdp_url: Option<String>,
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            executable: None,
            headless: false,
            user_data_dir: None,
            viewport: Viewport::default(),
            download_dir: None,
            accept_confirm: false,
            cdp_url: None,
        }
    }
}

pub struct Browser {
    cdp: Arc<Cdp>,
    child: Option<Child>,
    pub port: u16,
    download_dir: Option<PathBuf>,
    /// Targets this process created. Only used when attached to a browser we
    /// do not own: killing our own child closes them anyway, but a remote
    /// Chrome would otherwise keep one tab per completed task.
    targets: std::sync::Mutex<Vec<String>>,
}

pub struct Session {
    cdp: Arc<Cdp>,
    pub target_id: String,
    pub session_id: String,
}

/// Arms download tracking for a step. Create it before the click that exports,
/// then `wait` after: the events are edge-triggered, not state.
pub struct DownloadGuard {
    rx: broadcast::Receiver<Event>,
    dir: PathBuf,
    filename: Option<String>,
    /// Once a download completes, the path stays here: callers may poll
    /// repeatedly and must all observe the same result.
    done: Option<PathBuf>,
}

impl DownloadGuard {
    fn handle(&mut self, ev: &Event) -> Result<Option<PathBuf>> {
        match ev.method.as_str() {
            "Browser.downloadWillBegin" => {
                self.filename = ev.params["suggestedFilename"].as_str().map(str::to_string);
            }
            "Browser.downloadProgress" => match ev.params["state"].as_str() {
                Some("completed") => {
                    let name = self.filename.take().ok_or_else(|| {
                        BhError::Browser("download completed without a filename".into())
                    })?;
                    return Ok(Some(self.dir.join(sanitize(&name))));
                }
                Some("canceled") => {
                    return Err(BhError::Action("download canceled".into()));
                }
                _ => {}
            },
            _ => {}
        }
        Ok(None)
    }

    pub async fn wait(mut self, timeout: Duration) -> Result<PathBuf> {
        if let Some(path) = self.done.clone() {
            return Ok(path);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(BhError::Timeout(timeout.as_millis() as u64));
            }
            let ev: Event = match tokio::time::timeout(left, self.rx.recv()).await {
                Err(_) => return Err(BhError::Timeout(timeout.as_millis() as u64)),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(BhError::Browser("event stream closed".into()));
                }
                Ok(Ok(ev)) => ev,
            };
            if let Some(path) = self.handle(&ev)? {
                self.done = Some(path.clone());
                return Ok(path);
            }
        }
    }

    /// Non-blocking variant, so a step can watch a failure signal while it
    /// waits for the file instead of blocking on one event stream.
    pub fn try_wait(&mut self) -> Result<Option<PathBuf>> {
        if let Some(path) = self.done.clone() {
            return Ok(Some(path));
        }
        loop {
            match self.rx.try_recv() {
                Ok(ev) => {
                    if let Some(path) = self.handle(&ev)? {
                        self.done = Some(path.clone());
                        return Ok(Some(path));
                    }
                }
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => {
                    return Err(BhError::Browser("event stream closed".into()));
                }
            }
        }
    }
}

impl Browser {
    pub async fn launch(opts: BrowserOptions) -> Result<Self> {
        if let Some(endpoint) = opts.cdp_url.clone().or_else(|| std::env::var("BH_CDP_URL").ok()) {
            return Self::attach(&endpoint, opts).await;
        }
        let port = free_port()?;
        let profile = opts.user_data_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("bhx-profile-{}", ulid::Ulid::new()))
        });
        std::fs::create_dir_all(&profile)
            .map_err(|e| BhError::Browser(format!("profile dir: {e}")))?;

        let exe = opts
            .executable
            .clone()
            .or_else(|| std::env::var("BH_CHROME").ok())
            .unwrap_or_else(|| DEFAULT_CHROME.to_string());

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg(format!(
                "--window-size={},{}",
                opts.viewport.w as u32, opts.viewport.h as u32
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if opts.headless {
            cmd.arg("--headless=new");
        }
        cmd.arg("about:blank");

        let child = cmd
            .spawn()
            .map_err(|e| BhError::Browser(format!("spawn {exe}: {e}")))?;

        let ws = wait_for_browser(port, Duration::from_secs(20)).await?;
        let cdp = Arc::new(Cdp::connect(&ws).await?);

        Ok(Self {
            cdp,
            child: Some(child),
            port,
            download_dir: opts.download_dir,
            targets: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Attaches to a Chrome we did not start. We therefore never kill it:
    /// `child` stays `None`, so `shutdown` only closes the target we created.
    async fn attach(endpoint: &str, opts: BrowserOptions) -> Result<Self> {
        let ws = resolve_cdp_endpoint(endpoint).await?;
        let cdp = Arc::new(Cdp::connect(&ws).await?);
        Ok(Self {
            cdp,
            child: None,
            port: 0,
            download_dir: opts.download_dir,
            targets: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub async fn new_session(&self, opts: &BrowserOptions) -> Result<Session> {
        let created = self
            .cdp
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = created["targetId"]
            .as_str()
            .ok_or_else(|| BhError::Browser("no targetId".into()))?
            .to_string();
        self.targets.lock().expect("targets lock").push(target_id.clone());

        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = attached["sessionId"]
            .as_str()
            .ok_or_else(|| BhError::Browser("no sessionId".into()))?
            .to_string();

        let session = Session {
            cdp: self.cdp.clone(),
            target_id,
            session_id,
        };

        for domain in ["Page.enable", "Runtime.enable", "DOM.enable", "Accessibility.enable"] {
            session.call(domain, json!({})).await?;
        }

        session
            .call(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": opts.viewport.w,
                    "height": opts.viewport.h,
                    "deviceScaleFactor": opts.viewport.dpr,
                    "mobile": false
                }),
            )
            .await?;

        if let Some(dir) = &self.download_dir {
            std::fs::create_dir_all(dir).ok();
            self.cdp
                .call(
                    "Browser.setDownloadBehavior",
                    json!({
                        "behavior": "allow",
                        "downloadPath": dir.to_string_lossy(),
                        "eventsEnabled": true
                    }),
                )
                .await?;
        }

        // A JS dialog blocks the renderer until someone answers it. Left alone,
        // one surprise `alert()` hangs the task forever. Phase 1 policy: dismiss
        // (never confirm) everything except `beforeunload`, and record it.
        // Escalating dialogs to a decision model is a later phase, not a reason
        // to hang now.
        let cdp = self.cdp.clone();
        let sid = session.session_id.clone();
        let accept_confirm = opts.accept_confirm;
        tokio::spawn(async move {
            let mut rx = cdp.subscribe();
            while let Ok(ev) = rx.recv().await {
                if ev.method != "Page.javascriptDialogOpening" {
                    continue;
                }
                if ev.session_id.as_deref() != Some(sid.as_str()) {
                    continue;
                }
                let kind = ev.params["type"].as_str().unwrap_or("alert").to_string();
                // alert() carries no choice; confirm()/prompt() do, and the
                // task policy has to opt in before we answer them.
                let accept = match kind.as_str() {
                    "confirm" | "prompt" => accept_confirm,
                    _ => true,
                };
                tracing::warn!(
                    dialog = %kind,
                    message = ev.params["message"].as_str().unwrap_or(""),
                    accept,
                    "auto-handled javascript dialog"
                );
                let _ = cdp
                    .call_session(
                        &sid,
                        "Page.handleJavaScriptDialog",
                        json!({ "accept": accept }),
                    )
                    .await;
            }
        });

        Ok(session)
    }

    /// Wait for the next download to complete and return its file path.
    pub async fn wait_download(&self, timeout: Duration) -> Result<PathBuf> {
        self.watch_downloads()?.wait(timeout).await
    }

    /// Arm a download watcher *before* the action that triggers the download.
    /// A broadcast receiver only sees events sent after it subscribed.
    pub fn watch_downloads(&self) -> Result<DownloadGuard> {
        let dir = self
            .download_dir
            .clone()
            .ok_or_else(|| BhError::Browser("no download directory configured".into()))?;
        Ok(DownloadGuard {
            rx: self.cdp.subscribe(),
            dir,
            filename: None,
            done: None,
        })
    }

    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        self.close_attached_targets();
    }

    /// Closes the targets we created on a browser we do not own. CDP calls are
    /// async, `shutdown` is not (it runs from `Drop`), so the work is spawned
    /// on the current runtime and the caller waits a bounded time for it.
    /// Killing our own child needs none of this: the tabs die with it.
    fn close_attached_targets(&self) {
        let targets = std::mem::take(&mut *self.targets.lock().expect("targets lock"));
        if targets.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cdp = self.cdp.clone();
        let (done, wait) = std::sync::mpsc::channel();
        handle.spawn(async move {
            for target in targets {
                let _ = cdp.call("Target.closeTarget", json!({ "targetId": target })).await;
            }
            let _ = done.send(());
        });
        // A remote browser that has gone away must not hang teardown.
        let _ = wait.recv_timeout(Duration::from_secs(2));
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Session {
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.call_session(&self.session_id, method, params).await
    }

    pub fn cdp(&self) -> &Arc<Cdp> {
        &self.cdp
    }

    pub async fn navigate(&self, url: &str) -> Result<()> {
        // Subscribe first: the load event can fire before navigate returns.
        let rx = self.cdp.subscribe();
        self.call("Page.navigate", json!({ "url": url })).await?;

        let wait = self
            .cdp
            .wait_event(rx, "Page.loadEventFired", Some(&self.session_id), Duration::from_secs(30))
            .await;

        if wait.is_err() {
            // Fallback for pages that never fire loadEventFired (e.g. blocked).
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let state = self.evaluate("document.readyState").await?;
                if state == "complete" {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(BhError::Timeout(10_000));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Ok(())
    }

    pub async fn url(&self) -> Result<String> {
        Ok(self
            .evaluate("location.href")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self
            .evaluate("document.title")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.cdp.evaluate(&self.session_id, expression).await
    }

    /// Evaluate with DOM access but without page globals. Use this for
    /// caller-supplied scripts; page state that lives on `window` is only
    /// visible through `evaluate`.
    pub async fn evaluate_isolated(&self, expression: &str) -> Result<Value> {
        self.cdp.evaluate_isolated(&self.session_id, expression).await
    }

    pub async fn viewport(&self) -> Result<Viewport> {
        let m = self.call("Page.getLayoutMetrics", json!({})).await?;
        let vp = &m["cssLayoutViewport"];
        let dpr = self.evaluate("window.devicePixelRatio").await?;
        let scroll = self
            .evaluate("({x: window.scrollX, y: window.scrollY})")
            .await?;
        Ok(Viewport {
            w: vp["clientWidth"].as_f64().unwrap_or(1280.0),
            h: vp["clientHeight"].as_f64().unwrap_or(800.0),
            dpr: dpr.as_f64().unwrap_or(1.0),
            scroll_x: scroll["x"].as_f64().unwrap_or(0.0),
            scroll_y: scroll["y"].as_f64().unwrap_or(0.0),
        })
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.capture_screenshot(json!({ "format": "jpeg", "quality": 70 }))
            .await
    }

    /// Lossless capture for grounding models: a JPEG artifact around a thin
    /// border or a small control can move the bbox by several pixels.
    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        self.capture_screenshot(json!({ "format": "png" })).await
    }

    async fn capture_screenshot(&self, params: Value) -> Result<Vec<u8>> {
        let r = self.call("Page.captureScreenshot", params).await?;
        let b64 = r["data"]
            .as_str()
            .ok_or_else(|| BhError::Browser("empty screenshot".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| BhError::Other(format!("decode screenshot: {e}")))
    }

    pub async fn box_model(&self, backend_node_id: i64) -> Result<Option<Rect>> {
        // "Could not compute box model" is CDP's way of saying the node has no
        // layout box (display:none, detached, zero-sized). That is an answer,
        // not a failure: no box means not clickable.
        let r = match self
            .call("DOM.getBoxModel", json!({ "backendNodeId": backend_node_id }))
            .await
        {
            Ok(r) => r,
            Err(BhError::Cdp(msg)) if msg.contains("box model") => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(content) = r["model"]["content"].as_array() else {
            return Ok(None);
        };
        let q: Vec<f64> = content.iter().filter_map(Value::as_f64).collect();
        if q.len() < 8 {
            return Ok(None);
        }
        let xs = [q[0], q[2], q[4], q[6]];
        let ys = [q[1], q[3], q[5], q[7]];
        let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Ok(Some(Rect {
            x: min_x,
            y: min_y,
            w: max_x - min_x,
            h: max_y - min_y,
        }))
    }

    /// Bring a node into the viewport if needed, then report where it landed.
    /// Coordinates from a stale box model are the most common cause of clicks
    /// that "succeed" and hit nothing.
    pub async fn scroll_into_view(&self, backend_node_id: i64) -> Result<Option<Rect>> {
        self.call(
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": backend_node_id }),
        )
        .await?;
        self.box_model(backend_node_id).await
    }

    /// Click without a preceding `mouseMoved`: moving the pointer costs ~16 ms
    /// (a frame), while press+release costs ~0.4 ms. See docs/benchmarks.md.
    pub async fn click(&self, at: Point, button: MouseButton, click_count: u32) -> Result<()> {
        let b = button_name(button);
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": at.x, "y": at.y, "button": b, "clickCount": click_count }),
            )
            .await?;
        }
        Ok(())
    }

    /// Press, move in steps, release. Intermediate moves matter: pages that
    /// implement drag themselves only see a gesture if `mouseMoved` events
    /// arrive between press and release.
    pub async fn drag(&self, from: Point, to: Point, steps: u32) -> Result<()> {
        let steps = steps.clamp(1, 64);
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": from.x, "y": from.y }),
        )
        .await?;
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": from.x, "y": from.y, "button": "left", "buttons": 1, "clickCount": 1 }),
        )
        .await?;
        for i in 1..=steps {
            let f = f64::from(i) / f64::from(steps);
            self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": from.x + (to.x - from.x) * f,
                    "y": from.y + (to.y - from.y) * f,
                    "button": "left",
                    "buttons": 1,
                }),
            )
            .await?;
        }
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": to.x, "y": to.y, "button": "left", "buttons": 0, "clickCount": 1 }),
        )
        .await?;
        Ok(())
    }

    pub async fn hover(&self, at: Point) -> Result<()> {        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": at.x, "y": at.y }),
        )
        .await?;
        Ok(())
    }

    pub async fn scroll(&self, at: Point, dx: f64, dy: f64) -> Result<()> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": at.x, "y": at.y, "deltaX": dx, "deltaY": dy }),
        )
        .await?;
        Ok(())
    }

    /// Type into whatever currently has focus.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if ch.is_ascii() && !ch.is_control() {
                let key = ch.to_string();
                let vk = if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase() as i64
                } else {
                    0
                };
                let code = code_for_char(ch);
                self.call(
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": "keyDown", "key": key, "code": code,
                        "text": key, "unmodifiedText": key,
                        "windowsVirtualKeyCode": vk
                    }),
                )
                .await?;
                self.call(
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": "keyUp", "key": key, "code": code,
                        "windowsVirtualKeyCode": vk
                    }),
                )
                .await?;
            } else {
                // Non-ASCII goes through the IME path; it still fires input events.
                self.call("Input.insertText", json!({ "text": ch.to_string() }))
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn press_key(&self, key: &str, modifiers: &[bh_core::Modifier]) -> Result<()> {
        let (code, vk) = key_def(key);
        let mask: i64 = modifiers.iter().map(|m| modifier_bit(*m)).sum();
        self.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown", "key": key, "code": code,
                "windowsVirtualKeyCode": vk, "modifiers": mask
            }),
        )
        .await?;
        self.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp", "key": key, "code": code,
                "windowsVirtualKeyCode": vk, "modifiers": mask
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn key_down(&self, key: &str) -> Result<()> {
        let (code, vk) = key_def(key);
        self.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk }),
        )
        .await?;
        Ok(())
    }

    pub async fn key_up(&self, key: &str) -> Result<()> {
        let (code, vk) = key_def(key);
        self.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk }),
        )
        .await?;
        Ok(())
    }

    /// Release everything this session may have left pressed. Safe to call twice.
    pub async fn release_all_input(&self) -> Result<()> {
        // Best effort: these are the keys the controller is allowed to hold.
        for key in ["KeyW", "KeyA", "KeyS", "KeyD", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Space"] {
            let _ = self.key_up(key).await;
        }
        Ok(())
    }
}

fn button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

fn modifier_bit(m: bh_core::Modifier) -> i64 {
    match m {
        bh_core::Modifier::Alt => 1,
        bh_core::Modifier::Control => 2,
        bh_core::Modifier::Meta => 4,
        bh_core::Modifier::Shift => 8,
    }
}

fn code_for_char(ch: char) -> String {
    if ch.is_ascii_alphabetic() {
        format!("Key{}", ch.to_ascii_uppercase())
    } else if ch.is_ascii_digit() {
        format!("Digit{ch}")
    } else if ch == ' ' {
        "Space".into()
    } else {
        String::new()
    }
}

fn key_def(key: &str) -> (String, i64) {
    let (code, vk) = match key {
        "Enter" => ("Enter", 13),
        "Tab" => ("Tab", 9),
        "Escape" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Delete" => ("Delete", 46),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", 39),
        "Home" => ("Home", 36),
        "End" => ("End", 35),
        "PageUp" => ("PageUp", 33),
        "PageDown" => ("PageDown", 34),
        "Space" => ("Space", 32),
        "KeyW" => ("KeyW", 87),
        "KeyA" => ("KeyA", 65),
        "KeyS" => ("KeyS", 83),
        "KeyD" => ("KeyD", 68),
        other => (other, 0),
    };
    (code.to_string(), vk)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .collect()
}

/// Turns what a user writes into a CDP websocket URL.
///
/// `ws(s)://` passes through; anything else is treated as an HTTP endpoint
/// (`host:port` becomes `http://host:port`) and resolved through
/// `/json/version`, which is what a Chrome started with
/// `--remote-debugging-port` serves.
async fn resolve_cdp_endpoint(endpoint: &str) -> Result<String> {
    let e = endpoint.trim();
    if e.is_empty() {
        return Err(BhError::Browser("empty CDP endpoint".into()));
    }
    if e.starts_with("ws://") || e.starts_with("wss://") {
        return Ok(e.to_string());
    }
    let base = if e.starts_with("http://") || e.starts_with("https://") {
        e.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", e.trim_end_matches('/'))
    };
    let url = format!("{base}/json/version");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = {
            let url = url.clone();
            tokio::task::spawn_blocking(move || {
                let mut res = ureq::get(&url).call().ok()?;
                let body = res.body_mut().read_to_string().ok()?;
                let v: Value = serde_json::from_str(&body).ok()?;
                v["webSocketDebuggerUrl"].as_str().map(str::to_string)
            })
            .await
            .map_err(|e| BhError::Browser(e.to_string()))?
        };
        if let Some(ws) = found {
            return Ok(ws);
        }
        if Instant::now() > deadline {
            return Err(BhError::Browser(format!(
                "no CDP endpoint at {endpoint:?} (tried {url}); \
                 start Chrome with --remote-debugging-port or pass a ws:// URL"
            )));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| BhError::Browser(format!("bind: {e}")))?;
    let port = l.local_addr().map_err(|e| BhError::Browser(e.to_string()))?.port();
    drop(l);
    Ok(port)
}

async fn wait_for_browser(port: u16, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let url = format!("http://127.0.0.1:{port}/json/version");
        let found = tokio::task::spawn_blocking(move || {
            let mut res = ureq::get(&url).call().ok()?;
            let body = res.body_mut().read_to_string().ok()?;
            let v: Value = serde_json::from_str(&body).ok()?;
            v["webSocketDebuggerUrl"].as_str().map(str::to_string)
        })
        .await
        .map_err(|e| BhError::Browser(e.to_string()))?;

        if let Some(ws) = found {
            return Ok(ws);
        }
        if Instant::now() > deadline {
            return Err(BhError::Browser(format!(
                "chrome did not expose CDP on port {port} within {timeout:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
