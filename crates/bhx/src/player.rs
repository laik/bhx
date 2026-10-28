//! The replay player that ships with `bhx trace --out`.
//!
//! The bundle is a directory on disk, so the player has to survive being opened
//! straight from `file://`: one HTML file, no network, no build step, no model.
//! Everything it shows comes from `trace.json`, which is embedded verbatim.
//!
//! Two things it deliberately does not do: it does not re-run the task (replay
//! is the record, not a second execution), and it does not show action payloads
//! (the trace never carried them — see `docs/interfaces.md`, invariant 18).

use serde_json::Value;

const TEMPLATE: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<title>bhx trace __TASK_ID__</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {
    --bg:#0f1115; --panel:#171a21; --line:#2a2f3a; --text:#e6e8ee; --muted:#9aa3b2;
    --ok:#15803d; --bad:#b91c1c; --warn:#b45309; --accent:#3b82f6; --act:#38bdf8;
  }
  * { box-sizing: border-box; }
  body { margin:0; background:var(--bg); color:var(--text);
         font:14px/1.5 -apple-system, "PingFang SC", system-ui, sans-serif; }
  header { padding:16px 20px; border-bottom:1px solid var(--line); background:var(--panel); }
  .title { display:flex; gap:10px; align-items:baseline; flex-wrap:wrap; }
  .title h1 { font-size:16px; margin:0; font-weight:600; }
  .badge { padding:2px 9px; border-radius:999px; font-size:12px; font-weight:600; }
  .badge.succeeded { background:var(--ok); color:#fff; }
  .badge.failed, .badge.cancelled { background:var(--bad); color:#fff; }
  .badge.waiting_approval, .badge.waiting_human, .badge.running { background:var(--warn); color:#fff; }
  .badge.pending { background:#374151; color:#e5e7eb; }
  .meta { color:var(--muted); font-size:12px; margin-top:6px; display:flex; gap:16px; flex-wrap:wrap; }
  .controls { display:flex; gap:10px; align-items:center; margin-top:12px; flex-wrap:wrap; }
  button, select { font:inherit; color:var(--text); background:#20252e; border:1px solid var(--line);
                   border-radius:8px; padding:6px 12px; cursor:pointer; }
  button:hover, select:hover { background:#262c36; }
  button.primary { background:var(--accent); border-color:var(--accent); color:#fff; }
  #scrub { flex:1; min-width:120px; accent-color:var(--accent); }
  #clock { color:var(--muted); font-variant-numeric:tabular-nums; min-width:110px; text-align:right; }
  main { display:grid; grid-template-columns: 300px minmax(0,1fr) 320px; gap:0; height:calc(100vh - 118px); }
  aside, section { overflow:auto; padding:16px; }
  aside { border-right:1px solid var(--line); }
  #detail { border-right:none; border-left:1px solid var(--line); }
  h2 { font-size:12px; text-transform:uppercase; letter-spacing:.05em; color:var(--muted); margin:0 0 10px; }
  .step { border:1px solid var(--line); border-radius:10px; padding:10px 12px; margin-bottom:10px; background:var(--panel); }
  .step.active { border-color:var(--accent); }
  .step .row { display:flex; justify-content:space-between; gap:8px; align-items:baseline; }
  .step .sid { font-weight:600; }
  .step .goal { color:var(--muted); font-size:12px; margin-top:2px; }
  .step .dur { color:var(--muted); font-size:12px; margin-top:2px; font-variant-numeric:tabular-nums; }
  .step .err { color:#fca5a5; font-size:12px; margin-top:6px; word-break:break-word; }
  .step .acts { margin-top:8px; display:flex; flex-direction:column; gap:4px; }
  .act { display:flex; justify-content:space-between; gap:8px; font-size:12px; color:var(--muted); cursor:pointer; }
  .act:hover { color:var(--text); }
  .act .t { color:var(--act); }
  .act.bad .t { color:#fca5a5; }
  .filters { display:flex; flex-wrap:wrap; gap:6px 14px; margin-bottom:12px; font-size:12px; color:var(--muted); }
  .filters label { display:flex; gap:5px; align-items:center; cursor:pointer; }
  ol.timeline { list-style:none; margin:0; padding:0; }
  .ev { display:grid; grid-template-columns: 70px 30px minmax(0,1fr); gap:8px; align-items:baseline;
        padding:6px 8px; border-radius:8px; border:1px solid transparent; cursor:pointer; }
  .ev:hover { background:#191d25; }
  .ev.sel { border-color:var(--accent); background:#1b2230; }
  .ev.playing { background:#1f2937; }
  .ev .when { color:var(--muted); font-variant-numeric:tabular-nums; font-size:12px; }
  .ev .seq { color:#4b5563; font-size:12px; text-align:right; }
  .ev .type { font-weight:600; }
  .ev .type.action { color:var(--act); }
  .ev .sub { color:var(--muted); font-size:12px; }
  .ev .sub code { color:#cbd5e1; }
  .bar { height:4px; border-radius:2px; background:var(--accent); margin-top:3px; min-width:2px; }
  .bar.bad { background:var(--bad); }
  .art { border:1px solid var(--line); border-radius:8px; padding:8px 10px; margin-bottom:8px; background:var(--panel); }
  .thumb { display:block; max-width:100%; margin-top:8px; border-radius:6px; border:1px solid var(--line); }
  .art a { color:var(--accent); text-decoration:none; word-break:break-all; }
  .art .sha { color:var(--muted); font-size:11px; font-family:ui-monospace, monospace; word-break:break-all; }
  pre { background:#0c0e13; border:1px solid var(--line); border-radius:8px; padding:10px;
        font:12px/1.45 ui-monospace, SFMono-Regular, monospace; white-space:pre-wrap; word-break:break-word; margin:8px 0 0; }
  .empty { color:var(--muted); font-size:12px; }
  @media (max-width: 900px) {
    main { grid-template-columns: 1fr; height:auto; }
    aside, section { border:none; border-top:1px solid var(--line); max-height:none; }
    #detail { border-left:none; }
  }
</style>
</head>
<body>
<header>
  <div class="title"><h1 id="goal"></h1><span class="badge" id="state"></span></div>
  <div class="meta" id="meta"></div>
  <div class="controls">
    <button class="primary" id="play">▶ 回放</button>
    <select id="speed" title="回放速度">
      <option value="0.5">0.5×</option>
      <option value="1" selected>1×</option>
      <option value="2">2×</option>
      <option value="4">4×</option>
    </select>
    <input id="scrub" type="range" min="0" max="0" value="0" step="1" aria-label="事件进度">
    <span id="clock"></span>
  </div>
</header>
<main>
  <aside>
    <h2>步骤</h2>
    <div id="steps"></div>
    <h2 style="margin-top:16px">产物</h2>
    <div id="artifacts"></div>
  </aside>
  <section>
    <h2>事件时间线</h2>
    <div class="filters" id="filters"></div>
    <ol class="timeline" id="timeline"></ol>
  </section>
  <aside id="detail">
    <h2>详情</h2>
    <div id="detail-body" class="empty">点一条事件看它的数据。</div>
  </aside>
</main>
<script id="trace-data" type="application/json">__TRACE_JSON__</script>
<script>
(() => {
  const D = JSON.parse(document.getElementById("trace-data").textContent);
  const t0 = Date.parse(D.task.created_at) || 0;
  const rel = ts => (ts ? Math.max(0, Date.parse(ts) - t0) : null);
  const ms = v => (v == null ? "–" : v < 1000 ? Math.round(v) + " ms" : (v / 1000).toFixed(2) + " s");
  const ok = (cond, cls) => (cond ? cls : "");

  document.getElementById("goal").textContent = D.task.goal || "(no goal)";
  const state = document.getElementById("state");
  state.textContent = D.task.state;
  state.className = "badge " + D.task.state;

  const endRel = rel(D.task.finished_at) ?? (D.events.length ? rel(D.events[D.events.length - 1].ts) : 0);
  const actions = D.events.filter(e => e.type === "action.executed");
  const failed = actions.filter(e => e.data && e.data.ok === false).length;
  const meta = [
    D.task.id,
    "耗时 " + ms(endRel),
    actions.length + " 个动作" + (failed ? "（失败 " + failed + "）" : ""),
    "模型调用 " + D.task.model_calls,
    D.events.length + " 条事件",
  ];
  document.getElementById("meta").replaceChildren(...meta.map(t => {
    const s = document.createElement("span"); s.textContent = t; return s;
  }));

  // ------------------------------------------------------- artifact files
  // `trace --out` copies every artifact as `<index>-<name>`; the player links
  // to exactly the same names, and maps a screenshot event's path to its file.
  const artName = (a, i) => a.name || (a.path || "").split("/").pop() || ("artifact-" + i);
  const artHref = (a, i) => "artifacts/" + encodeURIComponent(String(i).padStart(2, "0") + "-" + artName(a, i));
  const files = new Map();
  D.artifacts.forEach((a, i) => files.set(a.path, { name: artName(a, i), href: artHref(a, i) }));
  const shotOf = new Map();
  for (const e of D.events) {
    if (e.type === "step.screenshot" && e.data && e.data.step_id) shotOf.set(e.data.step_id, e);
  }
  function thumb(path, maxWidth) {
    const f = files.get(path);
    if (!f) return null;
    const img = document.createElement("img");
    img.className = "thumb";
    img.src = f.href;
    img.alt = "截图";
    img.loading = "lazy";
    if (maxWidth) img.style.maxWidth = maxWidth;
    return img;
  }

  // ---------------------------------------------------------------- steps
  const byStep = new Map();
  for (const e of actions) {
    const sid = (e.data && e.data.step_id) || "";
    if (!byStep.has(sid)) byStep.set(sid, []);
    byStep.get(sid).push(e);
  }
  const stepsEl = document.getElementById("steps");
  if (!D.steps.length) stepsEl.innerHTML = '<p class="empty">没有步骤记录。</p>';
  for (const s of D.steps) {
    const box = document.createElement("div");
    box.className = "step";
    box.dataset.step = s.step_id;
    const dur = s.started_at && s.finished_at ? rel(s.finished_at) - rel(s.started_at) : null;
    box.innerHTML = `
      <div class="row"><span class="sid"></span><span class="badge ${s.state}"></span></div>
      <div class="goal"></div>
      <div class="dur"></div>`;
    box.querySelector(".sid").textContent = s.step_id;
    const badge = box.querySelector(".badge");
    badge.textContent = s.state + (s.verified ? " · 已验证" : "");
    box.querySelector(".goal").textContent = s.goal || "";
    box.querySelector(".dur").textContent = `尝试 ${s.attempts} · ${ms(dur)}`;
    if (s.error) {
      const err = document.createElement("div");
      err.className = "err";
      err.textContent = s.error;
      box.appendChild(err);
    }
    const shot = shotOf.get(s.step_id);
    const img = shot && thumb(shot.data.path);
    if (img) box.appendChild(img);
    const list = document.createElement("div");
    list.className = "acts";
    for (const e of byStep.get(s.step_id) || []) {
      const a = document.createElement("div");
      a.className = "act " + ok(e.data.ok, "bad");
      a.innerHTML = '<span class="t"></span><span></span>';
      a.children[0].textContent = e.data.type;
      a.children[1].textContent = (e.data.ok ? "" : "失败 · ") + ms(e.data.elapsed_ms);
      a.onclick = () => select(e);
      list.appendChild(a);
    }
    if (list.children.length) box.appendChild(list);
    stepsEl.appendChild(box);
  }

  // ------------------------------------------------------------ artifacts
  const artEl = document.getElementById("artifacts");
  if (!D.artifacts.length) artEl.innerHTML = '<p class="empty">没有产物。</p>';
  for (const [i, a] of D.artifacts.entries()) {
    const box = document.createElement("div");
    box.className = "art";
    const link = document.createElement("a");
    link.textContent = artName(a, i);
    link.href = artHref(a, i);
    const sha = document.createElement("div");
    sha.className = "sha";
    sha.textContent = (a.bytes ?? "?") + " B · sha256 " + (a.sha256 || "–");
    box.append(link, sha);
    artEl.appendChild(box);
  }

  // -------------------------------------------------------------- filters
  const kinds = [...new Set(D.events.map(e => e.type))];
  const hidden = new Set();
  const filtersEl = document.getElementById("filters");
  for (const k of kinds) {
    const label = document.createElement("label");
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = true;
    cb.onchange = () => { cb.checked ? hidden.delete(k) : hidden.add(k); draw(); };
    label.append(cb, document.createTextNode(k));
    filtersEl.appendChild(label);
  }

  // ------------------------------------------------------------- timeline
  const timeline = document.getElementById("timeline");
  const maxMs = Math.max(1, ...actions.map(e => e.data.elapsed_ms || 0));
  let rows = [];
  function describe(e) {
    const d = e.data || {};
    switch (e.type) {
      case "action.executed": {
        const bits = [`<code>${esc(d.type)}</code>`, esc(d.step_id || "")];
        if (d.escalated) bits.push("视觉回落");
        if (d.error) bits.push("error: " + esc(String(d.error)));
        return bits.join(" · ");
      }
      case "step.started": return esc(d.goal || d.step_id || "");
      case "step.finished": return [esc(d.step_id), esc(d.status), d.attempts != null ? d.attempts + " 次" : ""].filter(Boolean).join(" · ");
      case "task.finished": return "state " + esc(d.state) + (d.error ? " · " + esc(String(d.error)) : "");
      case "step.screenshot": return [esc(d.step_id), esc(d.name || ""), d.bytes != null ? d.bytes + " B" : ""].filter(Boolean).join(" · ");
      case "task.verify.screenshot":
        return [esc(d.name || ""), d.width && d.height ? d.width + "×" + d.height : "",
                d.bytes != null ? d.bytes + " B" : ""].filter(Boolean).join(" · ");
      default: return esc(short(JSON.stringify(d)));
    }
  }
  function short(s) { return s && s.length > 140 ? s.slice(0, 140) + "…" : (s || ""); }
  function esc(s) {
    return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  }

  function draw() {
    rows = D.events.filter(e => !hidden.has(e.type));
    timeline.replaceChildren();
    for (const e of rows) {
      const li = document.createElement("li");
      li.className = "ev";
      const when = rel(e.ts);
      li.innerHTML = `
        <span class="when"></span>
        <span class="seq"></span>
        <span><span class="type ${e.type === "action.executed" ? "action" : ""}"></span>
          <div class="sub"></div></span>`;
      li.children[0].textContent = ms(when);
      li.children[1].textContent = e.seq;
      li.children[2].querySelector(".type").textContent = e.type;
      const sub = li.children[2].querySelector(".sub");
      sub.innerHTML = describe(e);
      if (e.type === "step.screenshot" || e.type === "task.verify.screenshot") {
        const pic = thumb(e.data.path, "240px");
        if (pic) sub.appendChild(pic);
      }
      if (e.type === "action.executed" && e.data.elapsed_ms) {
        const bar = document.createElement("div");
        bar.className = "bar" + ok(e.data.ok === false, "bad");
        bar.style.width = Math.max(2, Math.round((e.data.elapsed_ms / maxMs) * 160)) + " px";
        sub.appendChild(bar);
      }
      li.onclick = () => select(e);
      timeline.appendChild(li);
    }
    if (!rows.length) timeline.innerHTML = '<p class="empty">这些类型都被过滤掉了。</p>';
    scrub.max = Math.max(0, rows.length - 1);
    scrub.value = Math.min(scrub.value, scrub.max);
    clock.textContent = rows.length ? "0 / " + rows.length : "—";
  }

  // -------------------------------------------------------------- playback
  const playBtn = document.getElementById("play");
  const scrub = document.getElementById("scrub");
  const clock = document.getElementById("clock");
  let idx = -1, timer = null, playing = false;

  function select(e) {
    for (const n of timeline.querySelectorAll(".ev")) n.classList.remove("sel");
    const pos = rows.indexOf(e);
    if (pos >= 0) {
      timeline.children[pos].classList.add("sel");
      timeline.children[pos].scrollIntoView({ block: "nearest" });
      scrub.value = pos;
      clock.textContent = (pos + 1) + " / " + rows.length + " · +" + ms(rel(e.ts));
    }
    const body = document.getElementById("detail-body");
    body.innerHTML = "";
    const head = document.createElement("div");
    head.className = "sub";
    head.textContent = e.type + " · seq " + e.seq + " · +" + ms(rel(e.ts));
    const pre = document.createElement("pre");
    pre.textContent = JSON.stringify(e.data, null, 2);
    body.append(head);
    if (e.type === "step.screenshot" || e.type === "task.verify.screenshot") {
      const pic = thumb(e.data.path);
      if (pic) body.append(pic);
    }
    body.append(pre);
  }

  function step(dt) {
    if (idx + 1 >= rows.length) { stop(); return; }
    idx += 1;
    const cur = rows[idx];
    select(cur);
    for (const n of timeline.querySelectorAll(".ev")) n.classList.remove("playing");
    const next = rows[idx + 1];
    const gap = next ? Math.min(1000, Math.max(120, rel(next.ts) - rel(cur.ts))) : 0;
    timer = setTimeout(() => step(), gap / Number(document.getElementById("speed").value));
  }

  function start() {
    if (!rows.length) return;
    if (idx + 1 >= rows.length) idx = -1;
    playing = true;
    playBtn.textContent = "❚❚ 暂停";
    step();
  }
  function stop() {
    playing = false;
    clearTimeout(timer);
    playBtn.textContent = "▶ 回放";
  }
  playBtn.onclick = () => (playing ? stop() : start());
  scrub.oninput = () => {
    stop();
    idx = Number(scrub.value) - 1;
    if (idx >= 0 && idx < rows.length) select(rows[idx]);
    clock.textContent = (idx + 1) + " / " + rows.length;
  };
  window.addEventListener("keydown", ev => {
    if (ev.key !== " " || /^(INPUT|SELECT|BUTTON|TEXTAREA)$/.test(ev.target.tagName)) return;
    ev.preventDefault();
    playing ? stop() : start();
  });

  draw();
  // Open on the failure if there is one: that is what people come to see.
  const bad = rows.find(e => e.type === "action.executed" && e.data && e.data.ok === false);
  if (bad) select(bad); else if (rows.length) select(rows[rows.length - 1]);
})();
</script>
</body>
</html>
"#;

/// Renders the player for one trace bundle. The JSON goes into a
/// `<script type="application/json">` block, so every `<` is escaped as `<`:
/// valid JSON, and impossible for task-provided text (an error message, a page
/// title) to close the script tag and inject markup.
pub fn render(bundle: &Value) -> String {
    let id = bundle["task"]["id"].as_str().unwrap_or("task");
    let data = serde_json::to_string(bundle)
        .unwrap_or_else(|_| "{}".into())
        .replace('<', "\\u003c");
    TEMPLATE
        .replace("__TASK_ID__", &escape_html(id))
        .replace("__TRACE_JSON__", &data)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bundle(goal: &str, err: &str) -> Value {
        json!({
            "task": { "id": "01TEST", "kind": "workflow", "goal": goal, "mode": "auto",
                      "state": "failed", "model_calls": 0,
                      "created_at": "2026-10-28T00:00:00.000Z",
                      "updated_at": "2026-10-28T00:00:01.000Z",
                      "finished_at": "2026-10-28T00:00:01.000Z" },
            "steps": [{ "step_id": "s1", "idx": 0, "goal": "登录", "state": "failed",
                        "attempts": 1, "verified": false,
                        "started_at": "2026-10-28T00:00:00.100Z",
                        "finished_at": "2026-10-28T00:00:00.900Z", "error": err }],
            "events": [{ "seq": 1, "ts": "2026-10-28T00:00:00.500Z",
                         "type": "action.executed",
                         "data": { "step_id": "s1", "index": 0, "type": "type",
                                   "ok": false, "elapsed_ms": 31, "error": err } }],
            "artifacts": []
        })
    }

    #[test]
    fn a_bundle_renders_the_player_with_its_task_id() {
        let html = render(&bundle("登录后台", "nope"));
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<title>bhx trace 01TEST</title>"));
        assert!(html.contains("\"01TEST\""));
        assert!(!html.contains("__TRACE_JSON__"));
    }

    #[test]
    fn page_text_cannot_close_the_data_script_tag() {
        let html = render(&bundle("</script><script>alert(1)</script>", "</script>"));
        assert!(!html.contains("</script><script>alert(1)"));
        assert!(!html.contains("\"</script>\""));
        // The escaped form still round-trips as JSON.
        let start = html.find("<script id=\"trace-data\"").unwrap();
        let body = &html[start..];
        let open = body.find('>').unwrap() + 1;
        let close = body.find("</script>").unwrap();
        let data: Value = serde_json::from_str(&body[open..close]).expect("embedded JSON parses");
        assert_eq!(data["task"]["goal"], "</script><script>alert(1)</script>");
    }

    #[test]
    fn the_player_is_self_contained() {
        let html = render(&bundle("x", "y"));
        for bad in ["src=\"http", "href=\"http", "@import", "fetch(", "XMLHttpRequest"] {
            assert!(!html.contains(bad), "player must not reach the network: {bad}");
        }
    }
}
