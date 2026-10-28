//! Thin Chrome DevTools Protocol client.
//!
//! One websocket to the browser endpoint. Page-level commands carry a
//! `sessionId` from `Target.attachToTarget`. Events are rebroadcast so callers
//! can wait on them instead of polling.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bh_core::{BhError, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

pub struct Cdp {
    tx: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<Event>,
    next: AtomicU64,
}

impl Cdp {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| BhError::Cdp(format!("connect {ws_url}: {e}")))?;
        let (mut write, mut read) = stream.split();

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let p2 = pending.clone();
        let (events, _) = broadcast::channel(2048);
        let ev_tx = events.clone();

        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = p2.lock().await.remove(&id) {
                        let _ = tx.send(v);
                    }
                    continue;
                }
                let Some(method) = v.get("method").and_then(Value::as_str) else {
                    continue;
                };
                let _ = ev_tx.send(Event {
                    method: method.to_string(),
                    params: v.get("params").cloned().unwrap_or(Value::Null),
                    session_id: v.get("sessionId").and_then(Value::as_str).map(str::to_string),
                });
            }
        });

        Ok(Self {
            tx,
            pending,
            events,
            next: AtomicU64::new(1),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    async fn send(&self, mut msg: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        msg["id"] = json!(id);
        let (otx, orx) = oneshot::channel();
        self.pending.lock().await.insert(id, otx);
        self.tx
            .send(Message::Text(msg.to_string().into()))
            .map_err(|_| BhError::Cdp("websocket closed".into()))?;

        let v = tokio::time::timeout(Duration::from_secs(60), orx)
            .await
            .map_err(|_| BhError::Timeout(60_000))?
            .map_err(|_| BhError::Cdp("response channel dropped".into()))?;

        if let Some(err) = v.get("error") {
            let method = v.get("method").and_then(Value::as_str).unwrap_or("call");
            return Err(BhError::Cdp(format!("{method}: {err}")));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.send(json!({ "method": method, "params": params })).await
    }

    pub async fn call_session(&self, session: &str, method: &str, params: Value) -> Result<Value> {
        self.send(json!({ "method": method, "params": params, "sessionId": session }))
            .await
    }

    /// Wait for the next event matching `method` (and `session`, when given).
    /// Subscribe before triggering the action, otherwise the event can be missed.
    pub async fn wait_event(
        &self,
        mut rx: broadcast::Receiver<Event>,
        method: &str,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BhError::Timeout(timeout.as_millis() as u64));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Err(_) => return Err(BhError::Timeout(timeout.as_millis() as u64)),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(BhError::Cdp("event stream closed".into()));
                }
                Ok(Ok(ev)) => {
                    if ev.method != method {
                        continue;
                    }
                    if let Some(want) = session {
                        if ev.session_id.as_deref() != Some(want) {
                            continue;
                        }
                    }
                    return Ok(ev.params);
                }
            }
        }
    }

    pub async fn evaluate(&self, session: &str, expression: &str) -> Result<Value> {
        self.evaluate_in(session, None, expression).await
    }

    /// Evaluate inside an isolated world: same DOM, no page JavaScript globals.
    /// A navigation destroys the context, so callers create one per call rather
    /// than caching an id that may already be dead.
    pub async fn evaluate_isolated(&self, session: &str, expression: &str) -> Result<Value> {
        let tree = self
            .call_session(session, "Page.getFrameTree", json!({}))
            .await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or_else(|| BhError::Other("no main frame for isolated world".into()))?;
        let world = self
            .call_session(
                session,
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": "bhx-eval" }),
            )
            .await?;
        let ctx = world["executionContextId"]
            .as_i64()
            .ok_or_else(|| BhError::Other("isolated world returned no context".into()))?;
        self.evaluate_in(session, Some(ctx), expression).await
    }

    async fn evaluate_in(
        &self,
        session: &str,
        context_id: Option<i64>,
        expression: &str,
    ) -> Result<Value> {
        let mut params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        });
        if let Some(ctx) = context_id {
            params["contextId"] = json!(ctx);
        }
        let r = self
            .call_session(session, "Runtime.evaluate", params)
            .await?;
        if let Some(details) = r.get("exceptionDetails") {
            return Err(BhError::Other(format!(
                "js exception: {}",
                details.get("text").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        Ok(r.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null))
    }
}
