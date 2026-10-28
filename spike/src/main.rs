//! Phase 0 spike: prove that a thin self-built CDP client is enough, and measure
//! the latency floor of the primitives the harness depends on.
//!
//! This is throwaway code. It deliberately has no abstraction, no retries beyond
//! what is needed to run, and no error taxonomy. What it must produce is real numbers.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PORT: u16 = 9333;

// ---------------------------------------------------------------- CDP client

struct Cdp {
    tx: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next: AtomicU64,
}

impl Cdp {
    async fn connect(ws_url: &str) -> Result<Self> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .context("connect websocket")?;
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
                }
                // Events are ignored here; the real runtime needs them.
            }
        });

        Ok(Self {
            tx,
            pending,
            next: AtomicU64::new(1),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        self.pending.lock().await.insert(id, otx);
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        self.tx
            .send(Message::Text(msg.into()))
            .map_err(|_| anyhow!("cdp write half closed"))?;
        let v = tokio::time::timeout(Duration::from_secs(20), orx)
            .await
            .map_err(|_| anyhow!("{method} timed out"))??;
        if let Some(err) = v.get("error") {
            return Err(anyhow!("{method} failed: {err}"));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------- chrome

fn launch_chrome(profile: &PathBuf) -> Result<Child> {
    let _ = std::fs::remove_dir_all(profile);
    let child = Command::new(CHROME)
        .arg("--headless=new")
        .arg(format!("--remote-debugging-port={PORT}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .arg("--window-size=1280,800")
        .arg("about:blank")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn chrome")?;
    Ok(child)
}

fn http_get(url: &str) -> Result<String> {
    let mut res = ureq::get(url).call().context("http get")?;
    res.body_mut().read_to_string().context("read body")
}

fn http_put(url: &str) -> Result<String> {
    let mut res = ureq::put(url).send_empty().context("http put")?;
    res.body_mut().read_to_string().context("read body")
}

/// Open a fresh page target and return its websocket URL.
/// Chrome requires PUT for /json/new and needs a moment to come up.
async fn open_page() -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last;
    loop {
        match http_put(&format!("http://127.0.0.1:{PORT}/json/new?about:blank")) {
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(v) => {
                    if let Some(ws) = v.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                        return Ok(ws.to_string());
                    }
                    last = format!("no webSocketDebuggerUrl in {body}");
                }
                Err(e) => last = format!("bad json: {e}"),
            },
            Err(e) => last = format!("{e}"),
        }
        if Instant::now() > deadline {
            return Err(anyhow!("could not open a page target: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------- page helpers

async fn navigate(cdp: &Cdp, url: &str) -> Result<()> {
    cdp.call("Page.navigate", json!({ "url": url })).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = cdp
            .call("Runtime.evaluate", json!({ "expression": "document.readyState" }))
            .await?;
        if r["result"]["value"] == "complete" {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(anyhow!("page did not reach readyState=complete"));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[derive(Debug, Clone)]
struct AxEl {
    eid: String,
    role: String,
    name: String,
    backend_id: i64,
}

/// Build the element index prototype: interactive AX nodes with backendNodeId.
async fn element_index(cdp: &Cdp) -> Result<Vec<AxEl>> {
    let r = cdp.call("Accessibility.getFullAXTree", json!({})).await?;
    let nodes = r["nodes"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();

    for n in nodes {
        if n["ignored"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(backend_id) = n["backendDOMNodeId"].as_i64() else {
            continue;
        };
        let role = n["role"]["value"].as_str().unwrap_or("").to_string();
        if !is_interactive(&role) {
            continue;
        }
        let name = n["name"]["value"].as_str().unwrap_or("").to_string();
        out.push(AxEl {
            eid: format!("e{}", out.len() + 1),
            role,
            name,
            backend_id,
        });
    }
    Ok(out)
}

fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "tab"
            | "menuitem"
            | "slider"
            | "spinbutton"
            | "switch"
            | "listbox"
            | "option"
    )
}

async fn box_of(cdp: &Cdp, backend_id: i64) -> Result<Option<[f64; 4]>> {
    let r = cdp
        .call("DOM.getBoxModel", json!({ "backendNodeId": backend_id }))
        .await?;
    let Some(content) = r["model"]["content"].as_array() else {
        return Ok(None);
    };
    let q: Vec<f64> = content
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    if q.len() < 8 {
        return Ok(None);
    }
    let xs = [q[0], q[2], q[4], q[6]];
    let ys = [q[1], q[3], q[5], q[7]];
    let x = xs.iter().sum::<f64>() / 4.0;
    let y = ys.iter().sum::<f64>() / 4.0;
    let w = xs.iter().cloned().fold(f64::MIN, f64::max)
        - xs.iter().cloned().fold(f64::MAX, f64::min);
    let h = ys.iter().cloned().fold(f64::MIN, f64::max)
        - ys.iter().cloned().fold(f64::MAX, f64::min);
    Ok(Some([x, y, w, h]))
}

async fn click_at(cdp: &Cdp, x: f64, y: f64) -> Result<()> {
    let base = json!({ "x": x, "y": y, "button": "left", "clickCount": 1 });
    cdp.call(
        "Input.dispatchMouseEvent",
        merge(base.clone(), json!({ "type": "mouseMoved" })),
    )
    .await?;
    cdp.call(
        "Input.dispatchMouseEvent",
        merge(base.clone(), json!({ "type": "mousePressed" })),
    )
    .await?;
    cdp.call(
        "Input.dispatchMouseEvent",
        merge(base, json!({ "type": "mouseReleased" })),
    )
    .await?;
    Ok(())
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(am), Some(bm)) = (a.as_object_mut(), b.as_object()) {
        for (k, v) in bm {
            am.insert(k.clone(), v.clone());
        }
    }
    a
}

async fn hits(cdp: &Cdp) -> Result<i64> {
    let r = cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": "window.__hits || 0", "returnByValue": true }),
        )
        .await?;
    Ok(r["result"]["value"].as_i64().unwrap_or(0))
}

// ---------------------------------------------------------------- measurement

fn p(stats: &[f64], q: f64) -> f64 {
    let idx = ((stats.len() as f64 * q).ceil() as usize).saturating_sub(1);
    stats[idx.min(stats.len() - 1)]
}

struct Row {
    name: &'static str,
    n: usize,
    p50: f64,
    p95: f64,
    note: String,
}

fn finish(name: &'static str, mut ms: Vec<f64>, note: String) -> Row {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Row {
        name,
        n: ms.len(),
        p50: p(&ms, 0.50),
        p95: p(&ms, 0.95),
        note,
    }
}

fn report(rows: &[Row]) {
    println!("\n| 操作 | n | p50 | p95 | 备注 |");
    println!("|---|---:|---:|---:|---|");
    for r in rows {
        println!("| {} | {} | {:.1} ms | {:.1} ms | {} |", r.name, r.n, r.p50, r.p95, r.note);
    }
}

// ---------------------------------------------------------------- main

#[tokio::main]
async fn main() -> Result<()> {
    let dir = PathBuf::from("/tmp/bhx-spike");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("page.html"), small_page())?;
    std::fs::write(dir.join("large.html"), large_page(1000))?;

    let mut chrome = launch_chrome(&dir.join("profile"))?;
    let result = run(&dir).await;
    let _ = chrome.kill();
    result
}

async fn run(dir: &PathBuf) -> Result<()> {
    let ws = open_page().await?;
    let cdp = Cdp::connect(&ws).await?;
    cdp.call("Page.enable", json!({})).await?;
    cdp.call("Runtime.enable", json!({})).await?;
    cdp.call("DOM.enable", json!({})).await?;
    cdp.call("Accessibility.enable", json!({})).await?;
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 1280, "height": 800, "deviceScaleFactor": 2, "mobile": false }),
    )
    .await?;

    navigate(&cdp, &format!("file://{}/page.html", dir.display())).await?;

    // Warm-up: first call after navigation pays lazy init.
    cdp.call("Runtime.evaluate", json!({ "expression": "1+1" })).await?;
    cdp.call("Accessibility.getFullAXTree", json!({})).await?;

    let mut rows = Vec::new();

    // 1. plain round-trip
    let mut ms = Vec::new();
    for _ in 0..100 {
        let t = Instant::now();
        cdp.call("Runtime.evaluate", json!({ "expression": "1+1" })).await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(finish("Runtime.evaluate（CDP 往返）", ms, "含 JS 执行".into()));

    // 2. AX tree on a small page
    let mut ms = Vec::new();
    let mut ax_bytes = 0usize;
    for _ in 0..30 {
        let t = Instant::now();
        let r = cdp.call("Accessibility.getFullAXTree", json!({})).await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        ax_bytes = serde_json::to_vec(&r)?.len();
    }
    rows.push(finish(
        "Accessibility.getFullAXTree（小页面）",
        ms,
        format!("{} KB, {} 节点", ax_bytes / 1024, "~40"),
    ));

    // 3. element index construction
    let mut ms = Vec::new();
    let mut count = 0;
    for _ in 0..20 {
        let t = Instant::now();
        let idx = element_index(&cdp).await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        count = idx.len();
    }
    rows.push(finish(
        "元素索引构建（AX → 交互元素）",
        ms,
        format!("{} 个元素", count),
    ));

    // 4. box model
    let idx = element_index(&cdp).await?;
    println!("\n元素索引（小页面原型）：");
    for e in &idx {
        println!("  {:<3} {:<10} {}", e.eid, e.role, e.name);
    }

    let button = idx
        .iter()
        .find(|e| e.role == "button" && e.name.contains("Export"))
        .ok_or_else(|| anyhow!("export button not found in index"))?
        .clone();
    let mut ms = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        box_of(&cdp, button.backend_id).await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(finish("DOM.getBoxModel", ms, "backendNodeId → 盒模型".into()));

    // 5. input dispatch
    let bx = box_of(&cdp, button.backend_id).await?.unwrap();
    let (cx, cy) = (bx[0], bx[1]);
    // 5a. mouseMoved alone: it turns out to dominate a whole click
    let mut ms = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        cdp.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": cx, "y": cy }),
        )
        .await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(finish(
        "Input.dispatchMouseEvent（mouseMoved，单次）",
        ms,
        "悬停/移动".into(),
    ));

    let mut ms = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        click_at(&cdp, cx, cy).await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(finish("点击（move + press + release，3 次 CDP 调用）", ms, String::new()));

    // 5b. same click without the move event, to see what the extra call costs
    let mut ms = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        cdp.call(
            "Input.dispatchMouseEvent",
            json!({ "type":"mousePressed", "x":cx, "y":cy, "button":"left", "clickCount":1 }),
        )
        .await?;
        cdp.call(
            "Input.dispatchMouseEvent",
            json!({ "type":"mouseReleased", "x":cx, "y":cy, "button":"left", "clickCount":1 }),
        )
        .await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(finish("点击（press + release，2 次调用）", ms, "省掉 mouseMoved".into()));

    // 5c. keyboard dispatch, the primitive the interactive controller lives on
    let mut ms = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        cdp.call(
            "Input.dispatchKeyEvent",
            json!({ "type":"keyDown", "key":"a", "code":"KeyA", "windowsVirtualKeyCode":65 }),
        )
        .await?;
        cdp.call(
            "Input.dispatchKeyEvent",
            json!({ "type":"keyUp", "key":"a", "code":"KeyA", "windowsVirtualKeyCode":65 }),
        )
        .await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    rows.push(finish("键盘按下+抬起（2 次调用）", ms, "持续按键的最小单元".into()));

    // 6. screenshot
    let mut ms = Vec::new();
    let mut shot_bytes = 0usize;
    for _ in 0..30 {
        let t = Instant::now();
        let r = cdp
            .call(
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": 70 }),
            )
            .await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        shot_bytes = r["data"].as_str().map(str::len).unwrap_or(0) * 3 / 4;
    }
    rows.push(finish(
        "Page.captureScreenshot（1280×800 @2x, JPEG q70）",
        ms,
        format!("~{} KB/张", shot_bytes / 1024),
    ));

    let mut ms = Vec::new();
    let mut png_bytes = 0usize;
    for _ in 0..10 {
        let t = Instant::now();
        let r = cdp
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        png_bytes = r["data"].as_str().map(str::len).unwrap_or(0) * 3 / 4;
    }
    rows.push(finish(
        "Page.captureScreenshot（PNG, 同尺寸）",
        ms,
        format!("~{} KB/张", png_bytes / 1024),
    ));

    // 7. end-to-end click: AX lookup -> box -> click -> verify
    let before = hits(&cdp).await?;
    let mut ms = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let idx = element_index(&cdp).await?;
        let b = idx
            .iter()
            .find(|e| e.role == "button" && e.name.contains("Export"))
            .ok_or_else(|| anyhow!("button disappeared"))?;
        let bx = box_of(&cdp, b.backend_id)
            .await?
            .ok_or_else(|| anyhow!("no box"))?;
        click_at(&cdp, bx[0], bx[1]).await?;
        let now = hits(&cdp).await?;
        if now <= before {
            return Err(anyhow!("click did not register"));
        }
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let after = hits(&cdp).await?;
    rows.push(finish(
        "端到端点击（索引→盒模型→点击→验证）",
        ms,
        format!("命中 {before}→{after}，全部生效"),
    ));

    // 8. large page AX cost
    navigate(&cdp, &format!("file://{}/large.html", dir.display())).await?;
    cdp.call("Accessibility.getFullAXTree", json!({})).await?;
    let mut ms = Vec::new();
    let mut bytes = 0usize;
    let mut nodes = 0usize;
    for _ in 0..10 {
        let t = Instant::now();
        let r = cdp.call("Accessibility.getFullAXTree", json!({})).await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        bytes = serde_json::to_vec(&r)?.len();
        nodes = r["nodes"].as_array().map(Vec::len).unwrap_or(0);
    }
    rows.push(finish(
        "Accessibility.getFullAXTree（1000 行表格）",
        ms,
        format!("{} KB, {} 节点", bytes / 1024, nodes),
    ));

    let idx = element_index(&cdp).await?;
    println!("\n大页面交互元素数：{}", idx.len());
    println!("视口：1280×800 @2x");

    report(&rows);
    Ok(())
}

// ---------------------------------------------------------------- fixtures

fn small_page() -> String {
    r##"<!doctype html><html><head><meta charset="utf-8"><title>spike</title></head>
<body>
<h1>Orders</h1>
<nav><a href="#home">Home</a> <a href="#orders">Orders</a></nav>
<label>Search <input aria-label="query" placeholder="search"></label>
<button id="export"
  onclick="window.__hits=(window.__hits||0)+1;document.getElementById('hits').textContent=window.__hits">
  Export CSV
</button>
<button id="reset" onclick="window.__hits=0;document.getElementById('hits').textContent=0">Reset</button>
<p>hits: <span id="hits">0</span></p>
</body></html>"##
        .to_string()
}

fn large_page(rows: usize) -> String {
    let mut body = String::new();
    for i in 0..rows {
        body.push_str(&format!(
            "<tr><td>ORD-{i:05}</td><td>customer {i}</td><td>failed</td><td>2026-10-28</td></tr>"
        ));
        if i % 20 == 0 {
            body.push_str(&format!("<tr><td><button>Detail {i}</button></td><td></td><td></td><td></td></tr>"));
        }
    }
    format!(
        r##"<!doctype html><html><head><meta charset="utf-8"><title>large</title></head>
<body><h1>Orders</h1>
<table><thead><tr><th>ID</th><th>Customer</th><th>Status</th><th>Date</th></tr></thead>
<tbody>{body}</tbody></table></body></html>"##
    )
}
