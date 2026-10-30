//! `bhx` — the command line front door to the harness.
//!
//! One binary, three layers of surface:
//!
//! * look at a page: `observe`, `run` (a step script, no task state)
//! * do work and keep evidence: `task run|ls|show|resume|approve|reject|cancel|
//!   artifacts|recover`, then `stats` and `trace` to read what happened
//! * hand it to something else: `mcp` (stdio, for agents), `serve` (REST + WS),
//!   `schedule` (run a stored task on an interval), `plan` (goal → steps → run)
//!
//! The full flag list lives in `USAGE` below and is what `bhx --help` prints —
//! keep it in sync with the docs rather than restating it there.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bh_browser::{Browser, BrowserOptions};
use bh_core::{Observation, TaskSpec, Viewport};
use bh_exec::{Runner, TaskReport, TaskStatus};
use bh_task::schedule::{self, format_interval, parse_interval};
use bh_task::{Engine, NewSchedule, RunOptions, RunOutcome, ScheduleRow};
use bh_observe::{ObserveOptions, Observer};
use serde_json::json;

mod player;

const USAGE: &str = "\
bhx — AI browser harness

USAGE
  bhx observe [--url URL] [--max-elements N] [--json] [--shot FILE] [--headed]
  bhx run <script.json> [--headed] [--artifacts DIR] [--keep-open]
  bhx task run <script.json> [--headed] [--home DIR] [--cred NAME=VALUE]
  bhx task ls [--limit N] [--home DIR]
  bhx task show <id> [--json] [--home DIR]
  bhx task resume <id> [--headed] [--home DIR]
  bhx task approve <id> [--home DIR]
  bhx task reject <id> [--reason TEXT] [--home DIR]
  bhx task cancel <id> [--home DIR]
  bhx task artifacts <id> [--home DIR]
  bhx task recover [--home DIR]
  bhx stats [--json] [--home DIR]
  bhx trace <id> [--json] [--out DIR] [--home DIR]
  bhx schedule add <script.json> --every 30m [--home DIR]
  bhx schedule ls [--json] [--home DIR]
  bhx schedule rm <id> [--home DIR]
  bhx schedule enable|disable <id> [--home DIR]
  bhx schedule run [--once] [--concurrency N] [--home DIR]
  bhx plan --goal TEXT [--url URL] [--out FILE] [--run] [--cred NAME=VALUE] [--home DIR]
  bhx serve [--addr 127.0.0.1:8765] [--home DIR]
  bhx mcp [--artifacts DIR] [--home DIR]
  bhx version

ENV
  BH_CHROME   path to the Chrome/Chromium binary
  BHX_HOME    task home (default: ./.bhx)
  BH_CDP_URL  attach to a running Chrome (http://host:port or ws://…) instead of launching one
  BH_CRED_<NAME>  value for `${cred:name}` in a scheduled task (name upper-cased)
  RUST_LOG    tracing filter (default: warn)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let Some(cmd) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };

    match cmd {
        "observe" => block_on(cmd_observe(&args[1..])),
        "run" => block_on(cmd_run(&args[1..])),
        "task" => block_on(cmd_task(&args[1..])),
        "stats" => block_on(cmd_stats(&args[1..])),
        "trace" => block_on(cmd_trace(&args[1..])),
        "schedule" => block_on(cmd_schedule(&args[1..])),
        "plan" => block_on(cmd_plan(&args[1..])),
        "serve" => block_on(cmd_serve(&args[1..])),
        "mcp" => block_on(cmd_mcp(&args[1..])),
        "version" | "--version" | "-V" => {
            println!("bhx {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("bhx: unknown command {other:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn block_on<F: std::future::Future<Output = ExitCode>>(f: F) -> ExitCode {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

async fn cmd_observe(args: &[String]) -> ExitCode {
    let headless = !has(args, "--headed");
    let max_elements = flag(args, "--max-elements")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let json = has(args, "--json");
    let url = flag(args, "--url");
    let shot = flag(args, "--shot").map(PathBuf::from);

    let mut opts = BrowserOptions {
        headless,
        ..Default::default()
    };
    opts.viewport = Viewport::default();
    let browser = match Browser::launch(opts.clone()).await {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let session = match browser.new_session(&opts).await {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    if let Some(url) = &url {
        if let Err(e) = session.navigate(url).await {
            return fail(&e);
        }
    }

    let mut observer = Observer::new(ObserveOptions { max_elements });

    // `--run` makes this the debugger for the decision model: do the steps, then
    // print exactly the state the model would see afterwards.
    if let Some(script) = flag(args, "--run").map(PathBuf::from) {
        let spec = match read_spec(&script) {
            Ok(s) => s,
            Err(code) => return code,
        };
        let report = {
            let mut runner = Runner::new(&browser, &session, &mut observer);
            runner.run(&spec).await
        };
        eprintln!("{}", summary(&report, Path::new(".")));
    }

    let obs = match observer.observe(&session).await {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };

    if let Some(path) = &shot {
        match session.screenshot().await {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, &bytes) {
                    eprintln!("bhx: write {}: {e}", path.display());
                } else {
                    eprintln!("screenshot: {}", path.display());
                }
            }
            Err(e) => eprintln!("bhx: screenshot: {e}"),
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&obs).unwrap_or_default());
    } else {
        println!("{}", render(&obs));
    }
    ExitCode::SUCCESS
}

async fn cmd_run(args: &[String]) -> ExitCode {
    let Some(script) = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from) else {
        eprintln!("bhx: run needs a script path\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let headless = !has(args, "--headed");
    let keep_open = has(args, "--keep-open");

    let spec = match read_spec(&script) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let artifacts = flag(args, "--artifacts")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_artifact_dir(&script));
    if let Err(e) = std::fs::create_dir_all(&artifacts) {
        eprintln!("bhx: create artifacts dir {}: {e}", artifacts.display());
        return ExitCode::from(2);
    }

    let opts = BrowserOptions {
        headless,
        download_dir: Some(artifacts.clone()),
        accept_confirm: spec.policy.accept_confirm,
        ..Default::default()
    };
    let browser = match Browser::launch(opts.clone()).await {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let session = match browser.new_session(&opts).await {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };

    let mut observer = Observer::new(ObserveOptions {
        max_elements: spec.policy.observation.max_elements,
    });
    let report = {
        let mut runner =
            Runner::new(&browser, &session, &mut observer).with_artifacts_dir(artifacts.clone());
        runner.run(&spec).await
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    eprintln!("{}", summary(&report, &artifacts));

    if keep_open {
        eprintln!("browser kept open — press Ctrl-C to exit");
        let _ = tokio::signal::ctrl_c().await;
    }

    match report.status {
        TaskStatus::Succeeded => ExitCode::SUCCESS,
        TaskStatus::Failed => ExitCode::FAILURE,
        TaskStatus::WaitingApproval => ExitCode::from(3),
    }
}

// ------------------------------------------------------------------- tasks

fn task_home(args: &[String]) -> PathBuf {
    flag(args, "--home")
        .or_else(|| std::env::var("BHX_HOME").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".bhx")
        })
}

async fn cmd_task(args: &[String]) -> ExitCode {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("bhx: task needs a subcommand\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let home = task_home(rest);
    let pos = positional(rest);

    let mut engine = match Engine::open(&home) {
        Ok(e) => e,
        Err(e) => return fail(&e),
    };

    match sub {
        "run" | "resume" => {
            let opts = RunOptions {
                headless: !has(rest, "--headed"),
                cancel: None,
                controller_updates: None,
            };
            let outcome = if sub == "run" {
                let Some(script) = pos.first() else {
                    eprintln!("bhx: task run needs a script path\n\n{USAGE}");
                    return ExitCode::from(2);
                };
                let mut spec = match read_spec(Path::new(script)) {
                    Ok(s) => s,
                    Err(code) => return code,
                };
                match parse_credentials(rest) {
                    Ok(creds) => spec.credentials.extend(creds),
                    Err(msg) => {
                        eprintln!("bhx: {msg}");
                        return ExitCode::from(2);
                    }
                }
                match engine.run(spec, opts).await {
                    Ok(o) => o,
                    Err(e) => return fail(&e),
                }
            } else {
                let Some(id) = pos.first() else {
                    eprintln!("bhx: task resume needs a task id\n\n{USAGE}");
                    return ExitCode::from(2);
                };
                match engine.resume(id, opts).await {
                    Ok(o) => o,
                    Err(e) => return fail(&e),
                }
            };
            print_outcome(&outcome)
        }
        "ls" => {
            let limit = flag(rest, "--limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let tasks = match engine.store().tasks(limit) {
                Ok(t) => t,
                Err(e) => return fail(&e),
            };
            if tasks.is_empty() {
                println!("(no tasks in {})", home.display());
                return ExitCode::SUCCESS;
            }
            for t in &tasks {
                println!(
                    "{}  {:<16} {:<10} {}",
                    t.id,
                    t.state,
                    &t.created_at[..19.min(t.created_at.len())],
                    t.goal
                );
            }
            ExitCode::SUCCESS
        }
        "show" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: task show needs a task id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            show_task(&engine, id, has(rest, "--json"))
        }
        "approve" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: task approve needs a task id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            match engine.approve(id) {
                Ok(n) => {
                    println!("approved {n} pending approval(s) for {id}");
                    eprintln!("continue with: bhx task resume {id}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        "reject" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: task reject needs a task id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            let reason = flag(rest, "--reason").unwrap_or_else(|| "rejected by user".into());
            match engine.reject(id, &reason) {
                Ok(n) => {
                    println!("rejected {n} pending approval(s) for {id}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        "cancel" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: task cancel needs a task id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            // Cancelling a live task works by flipping the state; the running
            // engine checks it between steps.
            match engine.cancel(id) {
                Ok(()) => {
                    println!("{id} canceled");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        "artifacts" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: task artifacts needs a task id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            match engine.store().artifacts(id) {
                Ok(rows) => {
                    for a in rows {
                        println!("{}\t{}\t{}", a.path, a.bytes, a.sha256.unwrap_or_default());
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        "recover" => match engine.reap() {
            Ok(ids) if ids.is_empty() => {
                println!("nothing to recover");
                ExitCode::SUCCESS
            }
            Ok(ids) => {
                for id in &ids {
                    println!("{id}: running -> crashed");
                }
                eprintln!("inspect with `bhx task show <id>`, continue with `bhx task resume <id>`");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        other => {
            eprintln!("bhx: unknown task subcommand {other:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn show_task(engine: &Engine, id: &str, as_json: bool) -> ExitCode {
    let task = match engine.store().task(id) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    let steps = engine.store().steps(id).unwrap_or_default();
    let approvals = engine.store().approvals(id).unwrap_or_default();
    let artifacts = engine.store().artifacts(id).unwrap_or_default();
    let checkpoint = engine.store().last_checkpoint(id).ok().flatten();
    let events = engine.store().events(id, 0).unwrap_or_default();

    let v = json!({
        "id": task.id,
        "kind": task.kind,
        "goal": task.goal,
        "mode": task.mode,
        "state": task.state,
        "model_calls": task.model_calls,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "finished_at": task.finished_at,
        "result": task.result_json.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
        "error": task.error_json.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
        "steps": steps.iter().map(|s| json!({
            "step_id": s.step_id,
            "idx": s.idx,
            "goal": s.goal,
            "state": s.state,
            "attempts": s.attempts,
            "verified": s.verified,
            "error": s.error,
        })).collect::<Vec<_>>(),
        "approvals": approvals.iter().map(|a| json!({
            "id": a.id,
            "step_id": a.step_id,
            "reason": a.reason,
            "decision": a.decision,
            "payload": a.payload,
        })).collect::<Vec<_>>(),
        "artifacts": artifacts.iter().map(|a| json!({
            "kind": a.kind,
            "name": a.name,
            "path": a.path,
            "bytes": a.bytes,
            "sha256": a.sha256,
        })).collect::<Vec<_>>(),
        "checkpoint": checkpoint.as_ref().map(|c| json!({ "seq": c.seq, "step_id": c.step_id, "state": c.state })),
        "events": events.len(),
    });

    if as_json {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return ExitCode::SUCCESS;
    }

    println!("{}  {}  {}", task.id, task.state, task.goal);
    println!("  created {}  updated {}", task.created_at, task.updated_at);
    if !steps.is_empty() {
        println!("steps:");
        for s in &steps {
            println!(
                "  {:<10} {:<10} verified={:<6} attempts={:<3} {}",
                s.step_id,
                s.state,
                s.verified.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                s.attempts,
                s.goal
            );
            if let Some(e) = &s.error {
                println!("{:<14}{}", "", e);
            }
        }
    }
    if !approvals.is_empty() {
        println!("approvals:");
        for a in &approvals {
            println!(
                "  {:<10} {:<20} {}",
                a.step_id.clone().unwrap_or_else(|| "-".into()),
                a.decision.clone().unwrap_or_else(|| "PENDING".into()),
                a.reason
            );
        }
    }
    if !artifacts.is_empty() {
        println!("artifacts:");
        for a in &artifacts {
            println!(
                "  {}  {} bytes  {}",
                a.path,
                a.bytes,
                a.sha256.as_deref().unwrap_or("")
            );
        }
    }
    if let Some(c) = &checkpoint {
        println!("last checkpoint: seq={} state={}", c.seq, c.state);
    }
    println!("events: {}", events.len());
    ExitCode::SUCCESS
}

fn print_outcome(o: &RunOutcome) -> ExitCode {
    match &o.report {
        Some(report) => {
            println!("{}", serde_json::to_string_pretty(report).unwrap_or_default());
            eprintln!(
                "\n{} in {} ms  (task {})",
                o.state.to_uppercase(),
                report["elapsed_ms"],
                o.task_id
            );
            for s in report["steps"].as_array().into_iter().flatten() {
                let repaired = s["repair"]["status"]
                    .as_str()
                    .map(|st| format!("  [repaired: {st}]"))
                    .unwrap_or_default();
                eprintln!(
                    "  {:<10} {:<10} verified={:<5} {} ms  {}{}{}",
                    s["step_id"].as_str().unwrap_or("?"),
                    s["status"].as_str().unwrap_or("?"),
                    s["verified"],
                    s["elapsed_ms"],
                    "",
                    s["error"].as_str().map(|e| format!("  ({e})")).unwrap_or_default(),
                    repaired,
                );
            }
            if let Some(sem) = report.get("semantic") {
                eprintln!(
                    "  semantic: {} (confidence {:.2}, threshold {:.2}, model {})",
                    sem["verdict"].as_str().unwrap_or("?"),
                    sem["confidence"].as_f64().unwrap_or(0.0),
                    sem["threshold"].as_f64().unwrap_or(0.0),
                    sem["model"].as_str().unwrap_or("?"),
                );
            }
        }
        None => eprintln!("{}  (task {})", o.state.to_uppercase(), o.task_id),
    }
    match o.state.as_str() {
        "succeeded" => ExitCode::SUCCESS,
        "waiting_approval" => ExitCode::from(3),
        _ => ExitCode::FAILURE,
    }
}

/// Positional arguments, skipping flags and the values of value-taking flags.
async fn cmd_stats(args: &[String]) -> ExitCode {
    let home = task_home(args);
    let engine = match Engine::open(&home) {
        Ok(e) => e,
        Err(e) => return fail(&e),
    };
    let st = match engine.store().stats() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let json_out = json!({
        "home": home.display().to_string(),
        "tasks": st.tasks,
        "by_state": st.by_state,
        "finished": st.finished(),
        "success_rate": st.success_rate(),
        "duration_ms": {
            "p50": st.duration_ms(50),
            "p95": st.duration_ms(95),
            "n": st.durations_ms.len(),
        },
        "model_calls": st.model_calls,
        "model_calls_per_task": if st.tasks > 0 { Some(st.model_calls as f64 / st.tasks as f64) } else { None },
        "actions": st.actions,
        "failed_actions": st.failed_actions,
        "action_failure_rate": st.action_failure_rate(),
    });
    if has(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&json_out).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    if st.tasks == 0 {
        println!("(no tasks in {})", home.display());
        return ExitCode::SUCCESS;
    }
    let states: Vec<String> = st.by_state.iter().map(|(k, v)| format!("{k} {v}")).collect();
    println!("tasks: {} ({})", st.tasks, states.join(", "));
    match st.success_rate() {
        Some(r) => println!(
            "success: {:.1}% of {} finished",
            r * 100.0,
            st.finished()
        ),
        None => println!("success: no finished tasks yet"),
    }
    println!(
        "duration: p50 {}, p95 {} (n={})",
        fmt_ms(st.duration_ms(50)),
        fmt_ms(st.duration_ms(95)),
        st.durations_ms.len()
    );
    println!(
        "model calls: {} total, {:.2} per task",
        st.model_calls,
        if st.tasks > 0 { st.model_calls as f64 / st.tasks as f64 } else { 0.0 }
    );
    match st.action_failure_rate() {
        Some(r) => println!(
            "actions: {} recorded, {} failed ({:.1}%)",
            st.actions,
            st.failed_actions,
            r * 100.0
        ),
        None => println!("actions: none recorded"),
    }
    ExitCode::SUCCESS
}

fn fmt_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}

async fn cmd_trace(args: &[String]) -> ExitCode {
    let home = task_home(args);
    let Some(id) = positional(args).first().cloned() else {
        eprintln!("bhx: trace needs a task id\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let engine = match Engine::open(&home) {
        Ok(e) => e,
        Err(e) => return fail(&e),
    };
    let store = engine.store();
    let task = match store.task(&id) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    let steps = match store.steps(&id) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let events = match store.events(&id, 0) {
        Ok(e) => e,
        Err(e) => return fail(&e),
    };
    let artifacts = match store.artifacts(&id) {
        Ok(a) => a,
        Err(e) => return fail(&e),
    };

    let bundle = json!({
        "task": {
            "id": task.id,
            "kind": task.kind,
            "goal": task.goal,
            "mode": task.mode,
            "state": task.state,
            "model_calls": task.model_calls,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "finished_at": task.finished_at,
        },
        "steps": steps.iter().map(|s| json!({
            "step_id": s.step_id,
            "idx": s.idx,
            "goal": s.goal,
            "state": s.state,
            "attempts": s.attempts,
            "verified": s.verified,
            "started_at": s.started_at,
            "finished_at": s.finished_at,
            "error": s.error,
        })).collect::<Vec<_>>(),
        "events": events.iter().map(|e| json!({
            "seq": e.seq,
            "ts": e.ts,
            "type": e.kind,
            "data": e.data,
        })).collect::<Vec<_>>(),
        "artifacts": artifacts.iter().map(|a| json!({
            "id": a.id,
            "kind": a.kind,
            "name": a.name,
            "path": a.path,
            "bytes": a.bytes,
            "sha256": a.sha256,
        })).collect::<Vec<_>>(),
    });

    if let Some(dir) = flag(args, "--out") {
        let out = PathBuf::from(dir);
        let art_dir = out.join("artifacts");
        if let Err(e) = std::fs::create_dir_all(&art_dir) {
            return fail(&e);
        }
        for (i, a) in artifacts.iter().enumerate() {
            let name = a
                .name
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| Path::new(&a.path).file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| format!("artifact-{i}"));
            let dest = art_dir.join(format!("{i:02}-{name}"));
            if let Err(e) = std::fs::copy(&a.path, &dest) {
                eprintln!("bhx: could not copy {}: {e}", a.path);
            }
        }
        let file = out.join("trace.json");
        let bytes = serde_json::to_vec_pretty(&bundle).unwrap_or_default();
        if let Err(e) = std::fs::write(&file, bytes) {
            return fail(&e);
        }
        println!("wrote {}", file.display());
        // The player is a single self-contained page next to the JSON; it works
        // from file:// with no server, which is the point of the bundle.
        let html = out.join("trace.html");
        if let Err(e) = std::fs::write(&html, player::render(&bundle)) {
            return fail(&e);
        }
        println!("wrote {}", html.display());
        if has(args, "--json") {
            println!("{}", serde_json::to_string_pretty(&bundle).unwrap_or_default());
        }
        return ExitCode::SUCCESS;
    }

    if has(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&bundle).unwrap_or_default());
        return ExitCode::SUCCESS;
    }

    println!("{}  {}  {}", task.id, task.state, task.goal);
    println!(
        "created {}  finished {}  model calls {}",
        task.created_at,
        task.finished_at.unwrap_or_else(|| "-".into()),
        task.model_calls
    );
    println!("steps");
    for s in &steps {
        println!(
            "  {:<4} {:<16} verified={:<5} attempts={}  {}",
            s.step_id,
            s.state,
            s.verified.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            s.attempts,
            s.goal
        );
    }
    println!("events");
    for e in &events {
        println!("  {:>4}  {}  {}", e.seq, e.ts, e.kind);
        println!("        {}", e.data);
    }
    if !artifacts.is_empty() {
        println!("artifacts");
        for a in &artifacts {
            println!(
                "  {}  {} B  sha256 {}",
                a.name.clone().unwrap_or_else(|| a.path.clone()),
                a.bytes,
                a.sha256.clone().unwrap_or_else(|| "-".into())
            );
        }
    }
    ExitCode::SUCCESS
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Human delay until `at_ms`, rounded up to whole seconds.
fn due_in(at_ms: i64) -> String {
    let d = at_ms - now_ms();
    if d <= 0 {
        return "due".into();
    }
    format!("in {}", format_interval((d + 999) / 1000 * 1000))
}

fn schedule_goal(spec_json: &str) -> String {
    serde_json::from_str::<TaskSpec>(spec_json)
        .map(|s| s.goal)
        .unwrap_or_else(|_| "(unreadable spec)".into())
}

async fn cmd_schedule(args: &[String]) -> ExitCode {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("bhx: schedule needs a subcommand\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let home = task_home(rest);
    let pos = positional(rest);
    let mut engine = match Engine::open(&home) {
        Ok(e) => e,
        Err(e) => return fail(&e),
    };

    match sub {
        "add" => {
            let Some(path) = pos.first() else {
                eprintln!("bhx: schedule add needs a script path\n\n{USAGE}");
                return ExitCode::from(2);
            };
            let every = match flag(rest, "--every") {
                Some(v) => match parse_interval(&v) {
                    Ok(ms) => ms,
                    Err(msg) => {
                        eprintln!("bhx: {msg}");
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("bhx: schedule add needs --every 30m\n\n{USAGE}");
                    return ExitCode::from(2);
                }
            };
            let mut spec = match read_spec(Path::new(path)) {
                Ok(s) => s,
                Err(code) => return code,
            };
            // A schedule is a template: every run gets its own task identity,
            // and only credential *names* are stored — values are read from
            // the environment when the run starts.
            spec.id = None;
            spec.credentials.clear();
            let names = spec.credential_names();
            let spec_json = match serde_json::to_string(&spec) {
                Ok(j) => j,
                Err(e) => return fail(&e),
            };
            let id = ulid::Ulid::new().to_string();
            if let Err(e) = engine.store().add_schedule(&NewSchedule {
                id: &id,
                source: path,
                spec_json: &spec_json,
                cred_names: &names,
                interval_ms: every,
                next_run_at_ms: now_ms(),
            }) {
                return fail(&e);
            }
            println!(
                "{id}  every {}  first run now  {}",
                format_interval(every),
                spec.goal
            );
            if !names.is_empty() {
                let keys: Vec<String> = names.iter().map(|n| schedule::credential_env_key(n)).collect();
                println!("credentials at run time: {}", keys.join(", "));
            }
            ExitCode::SUCCESS
        }
        "ls" => {
            let rows = match engine.store().schedules() {
                Ok(r) => r,
                Err(e) => return fail(&e),
            };
            if has(rest, "--json") {
                let out: Vec<_> = rows
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "source": s.source,
                            "goal": schedule_goal(&s.spec_json),
                            "enabled": s.enabled,
                            "interval_ms": s.interval_ms,
                            "interval": format_interval(s.interval_ms),
                            "next_run_at_ms": s.next_run_at_ms,
                            "next_in_ms": (s.next_run_at_ms - now_ms()).max(0),
                            "last_run_at": s.last_run_at,
                            "last_task_id": s.last_task_id,
                            "last_state": s.last_state,
                            "last_error": s.last_error,
                            "cred_names": s.cred_names,
                            "spec": serde_json::from_str::<serde_json::Value>(&s.spec_json)
                                .unwrap_or(serde_json::Value::Null),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
                return ExitCode::SUCCESS;
            }
            if rows.is_empty() {
                println!("(no schedules in {})", home.display());
                return ExitCode::SUCCESS;
            }
            for s in &rows {
                let last = match (&s.last_state, &s.last_task_id) {
                    (Some(state), Some(task)) => format!("{state} ({task})"),
                    (Some(state), None) => state.clone(),
                    _ => "-".into(),
                };
                let next = if s.enabled { due_in(s.next_run_at_ms) } else { "-".into() };
                println!(
                    "{}  {:<8} every {:<5} next {:<9} last {}  {}",
                    s.id,
                    if s.enabled { "enabled" } else { "disabled" },
                    format_interval(s.interval_ms),
                    next,
                    last,
                    schedule_goal(&s.spec_json)
                );
                if let Some(err) = &s.last_error {
                    println!("    last error: {err}");
                }
            }
            ExitCode::SUCCESS
        }
        "rm" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: schedule rm needs an id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            match engine.store().remove_schedule(id) {
                Ok(true) => {
                    println!("removed {id}");
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    eprintln!("bhx: no schedule {id}");
                    ExitCode::FAILURE
                }
                Err(e) => fail(&e),
            }
        }
        "enable" | "disable" => {
            let Some(id) = pos.first() else {
                eprintln!("bhx: schedule {sub} needs an id\n\n{USAGE}");
                return ExitCode::from(2);
            };
            let enabled = sub == "enable";
            match engine.store().set_schedule_enabled(id, enabled, now_ms()) {
                Ok(true) => {
                    println!("{id} {}", if enabled { "enabled" } else { "disabled" });
                    if enabled {
                        eprintln!("next slot is one interval from now");
                    }
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    eprintln!("bhx: no schedule {id}");
                    ExitCode::FAILURE
                }
                Err(e) => fail(&e),
            }
        }
        "run" => {
            let once = has(rest, "--once");
            let concurrency = match flag(rest, "--concurrency") {
                None => 1,
                Some(v) => match v.parse::<usize>() {
                    Ok(n) if (1..=8).contains(&n) => n,
                    _ => {
                        eprintln!("bhx: --concurrency takes 1..=8 (got {v:?})");
                        return ExitCode::from(2);
                    }
                },
            };
            let rows = match engine.store().schedules() {
                Ok(r) => r,
                Err(e) => return fail(&e),
            };
            eprintln!(
                "bhx schedule run: {} schedule(s) in {}, {}{}",
                rows.len(),
                home.display(),
                if once { "one pass" } else { "polling every 250 ms" },
                if concurrency > 1 {
                    format!(", up to {concurrency} at a time")
                } else {
                    String::new()
                }
            );

            // Runs report back over a channel instead of being joined: each one
            // owns its engine (opened on the same home), because a shared
            // `&mut Engine` cannot outlive one task, let alone several.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunMsg>();
            let mut failed = false;

            if once {
                let due = match engine.store().due_schedules(now_ms()) {
                    Ok(d) => d,
                    Err(e) => return fail(&e),
                };
                if due.is_empty() {
                    println!("(no schedule due)");
                    return ExitCode::SUCCESS;
                }
                // One pass means one snapshot: a schedule that becomes due
                // while these run waits for the next pass, not this one.
                let queue = due;
                let mut next = 0;
                let mut running = 0;
                while next < queue.len() || running > 0 {
                    while running < concurrency && next < queue.len() {
                        let s = &queue[next];
                        next += 1;
                        // Claimed when it is about to start, not up front:
                        // a slot queued behind the limit is not "running".
                        match engine.store().claim_schedule(&s.id, now_ms()) {
                            Ok(true) => {}
                            Ok(false) => continue,
                            Err(e) => return fail(&e),
                        }
                        spawn_run(&tx, s, &home);
                        running += 1;
                    }
                    match rx.recv().await {
                        Some((id, spec_json, result)) => {
                            running -= 1;
                            if let Err(e) =
                                record_schedule_result(&engine, &id, &spec_json, result, &mut failed)
                            {
                                return fail(&e);
                            }
                        }
                        None => break,
                    }
                }
                return if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS };
            }

            let mut inflight: HashSet<String> = HashSet::new();
            loop {
                while let Ok((id, spec_json, result)) = rx.try_recv() {
                    inflight.remove(&id);
                    if let Err(e) =
                        record_schedule_result(&engine, &id, &spec_json, result, &mut failed)
                    {
                        return fail(&e);
                    }
                }

                let due = match engine.store().due_schedules(now_ms()) {
                    Ok(d) => d,
                    Err(e) => return fail(&e),
                };
                for s in &due {
                    let busy = inflight.contains(&s.id);
                    if !busy && inflight.len() >= concurrency {
                        break;
                    }
                    // The slot is claimed before the run: a crash mid-run must
                    // not leave a task that is retried blindly on restart.
                    match engine.store().claim_schedule(&s.id, now_ms()) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(e) => return fail(&e),
                    }
                    if busy {
                        // One schedule never runs twice at once. The slot is
                        // consumed rather than deferred, so a run longer than
                        // its interval does not turn into a catch-up burst.
                        if let Err(e) = engine.store().finish_schedule_run(
                            &s.id,
                            None,
                            "skipped_overlap",
                            Some("previous run still in flight"),
                        ) {
                            return fail(&e);
                        }
                        println!("{}  skipped_overlap  (still running)", s.id);
                        continue;
                    }
                    inflight.insert(s.id.clone());
                    spawn_run(&tx, s, &home);
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        other => {
            eprintln!("bhx: unknown schedule subcommand {other:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

type RunMsg = (String, String, std::result::Result<RunOutcome, String>);

/// Hands one claimed schedule to a task of its own. The message carries the
/// stored spec back so the report can print the goal without a second lookup.
fn spawn_run(
    tx: &tokio::sync::mpsc::UnboundedSender<RunMsg>,
    s: &ScheduleRow,
    home: &std::path::Path,
) {
    let tx = tx.clone();
    let id = s.id.clone();
    let spec_json = s.spec_json.clone();
    let cred_names = s.cred_names.clone();
    let home = home.to_path_buf();
    tokio::spawn(async move {
        let result = run_schedule_owned(home, &spec_json, &cred_names).await;
        let _ = tx.send((id, spec_json, result));
    });
}

/// One scheduled run with its own engine. `Engine` opens the store again, which
/// is what makes concurrent runs possible: no handle is shared between tasks.
async fn run_schedule_owned(
    home: PathBuf,
    spec_json: &str,
    cred_names: &[String],
) -> std::result::Result<RunOutcome, String> {
    let mut spec: TaskSpec =
        serde_json::from_str(spec_json).map_err(|e| format!("stored spec is unreadable: {e}"))?;
    // All or nothing: an unresolved `${cred:x}` would be typed into the page.
    spec.credentials = schedule::resolve_credentials(cred_names, |k| std::env::var(k).ok())?;
    let mut engine = Engine::open(&home).map_err(|e| e.to_string())?;
    engine.run(spec, RunOptions::default()).await.map_err(|e| e.to_string())
}

fn record_schedule_result(
    engine: &Engine,
    id: &str,
    spec_json: &str,
    result: std::result::Result<RunOutcome, String>,
    failed: &mut bool,
) -> std::result::Result<(), String> {
    match result {
        Ok(outcome) => {
            if !outcome.succeeded() {
                *failed = true;
            }
            engine
                .store()
                .finish_schedule_run(id, Some(&outcome.task_id), &outcome.state, None)
                .map_err(|e| e.to_string())?;
            println!(
                "{}  {}  task {}  {}",
                id,
                outcome.state,
                outcome.task_id,
                schedule_goal(spec_json)
            );
        }
        Err(msg) => {
            *failed = true;
            engine
                .store()
                .finish_schedule_run(id, None, "failed", Some(&msg))
                .map_err(|e| e.to_string())?;
            eprintln!("bhx: {id}  failed: {msg}");
        }
    }
    Ok(())
}

fn positional(args: &[String]) -> Vec<String> {
    const VALUE_FLAGS: &[&str] = &[
        "--home", "--limit", "--reason", "--artifacts", "--max-elements", "--shot", "--url",
        "--cred", "--out", "--every", "--concurrency",
    ];
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.starts_with("--") {
            if VALUE_FLAGS.contains(&a.as_str()) {
                it.next();
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

async fn cmd_plan(args: &[String]) -> ExitCode {
    let Some(goal) = flag(args, "--goal") else {
        eprintln!("bhx: plan needs --goal TEXT\n\n{USAGE}");
        return ExitCode::from(2);
    };
    if goal.trim().is_empty() {
        eprintln!("bhx: --goal is empty");
        return ExitCode::from(2);
    }
    let url = flag(args, "--url");
    let out = flag(args, "--out").map(PathBuf::from);
    let run = has(args, "--run");
    let home = task_home(args);
    let budget: u32 = flag(args, "--budget")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let max_elements = flag(args, "--max-elements")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);

    let router = bh_model::ModelRouter::from_env(budget);
    if !router.is_configured() {
        eprintln!(
            "bhx: no model configured — set BH_LLM_URL (or BH_VLM_URL) to an OpenAI-compatible endpoint"
        );
        return ExitCode::from(2);
    }

    // The plan is made against a real page state, not a guess: the same
    // observer that feeds the runner feeds the planner.
    let opts = BrowserOptions { headless: !has(args, "--headed"), ..Default::default() };
    let browser = match Browser::launch(opts.clone()).await {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let session = match browser.new_session(&opts).await {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    if let Some(url) = &url {
        if let Err(e) = session.navigate(url).await {
            return fail(&e);
        }
    }
    let mut observer = Observer::new(ObserveOptions { max_elements });
    let obs = match observer.observe(&session).await {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    let mut state = obs.to_state_text("");
    if obs.truncated {
        state.push_str(&format!("(truncated at {} elements)\n", obs.elements.len()));
    }

    let proposal = match router.propose(&bh_model::ModelRequest::plan(&goal, &state)).await {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let Some(mut steps) = proposal.steps.clone() else {
        eprintln!("bhx: model returned no steps");
        return ExitCode::FAILURE;
    };
    // The router rejects duplicate non-empty ids, so any remaining empty id is
    // the only one that needs filling; the index is stable for checkpointing.
    for (i, step) in steps.iter_mut().enumerate() {
        if step.id.trim().is_empty() {
            step.id = format!("s{}", i + 1);
        }
    }

    let credentials = match parse_credentials(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("bhx: {msg}");
            return ExitCode::from(2);
        }
    };

    let spec = TaskSpec {
        goal: goal.clone(),
        steps,
        credentials,
        ..Default::default()
    };
    if let Err(e) = validate(&spec) {
        eprintln!("bhx: planned task is invalid: {e}");
        return ExitCode::FAILURE;
    }

    // Credentials stay in memory: the spec written to disk (and printed) keeps
    // the `${cred:...}` references and none of their values.
    let persisted = TaskSpec { credentials: Default::default(), ..spec.clone() };
    let pretty = serde_json::to_string_pretty(&persisted).unwrap_or_default();
    if let Some(path) = &out {
        if let Err(e) = std::fs::write(path, format!("{pretty}\n")) {
            eprintln!("bhx: write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }

    eprintln!(
        "planned {} step(s) with model {:?} ({} call(s) of budget {})",
        spec.steps.len(),
        proposal.model,
        router.calls(),
        router.budget()
    );
    for s in &spec.steps {
        eprintln!("  {:<10} {}", s.id, s.goal);
    }
    if let Some(path) = &out {
        eprintln!("spec: {}", path.display());
    }

    // The planning browser is not the one that runs the task: the engine owns
    // its own lifetime and its own download directory.
    drop(session);
    drop(browser);

    if run {
        let mut engine = match Engine::open(&home) {
            Ok(e) => e,
            Err(e) => return fail(&e),
        };
        let outcome = match engine
            .run(spec, RunOptions { headless: !has(args, "--headed"), ..RunOptions::default() })
            .await
        {
            Ok(o) => o,
            Err(e) => return fail(&e),
        };
        return print_outcome(&outcome);
    }

    if out.is_none() {
        println!("{pretty}");
    }
    ExitCode::SUCCESS
}

async fn cmd_serve(args: &[String]) -> ExitCode {
    let addr = flag(args, "--addr").unwrap_or_else(|| "127.0.0.1:8765".into());
    let home = task_home(args);
    eprintln!("bhx serve: http://{addr}/v1 (home: {})", home.display());
    match bh_server::http::serve_http(&addr, home).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

async fn cmd_mcp(args: &[String]) -> ExitCode {    let artifacts = flag(args, "--artifacts")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".bhx")
                .join("artifacts")
                .join("mcp")
        });
    let home = task_home(args);
    // stdout belongs to the protocol; everything human goes to stderr.
    eprintln!(
        "bhx mcp: serving on stdio (artifacts: {}, home: {})",
        artifacts.display(),
        home.display()
    );
    match bh_server::serve_stdio(artifacts, home).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn read_spec(path: &Path) -> Result<TaskSpec, ExitCode> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bhx: read {}: {e}", path.display());
            return Err(ExitCode::from(2));
        }
    };
    let spec: TaskSpec = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bhx: parse {}: {e}", path.display());
            return Err(ExitCode::from(2));
        }
    };
    if let Err(e) = validate(&spec) {
        eprintln!("bhx: invalid task: {e}");
        return Err(ExitCode::from(2));
    }
    Ok(spec)
}

fn validate(spec: &TaskSpec) -> Result<(), String> {
    if spec.protocol_version != "0.1" {
        return Err(format!(
            "unsupported protocol_version {:?} (expected \"0.1\")",
            spec.protocol_version
        ));
    }
    match spec.kind {
        bh_core::TaskKind::Workflow => {
            if spec.steps.is_empty() {
                if spec.mode != "auto" {
                    return Err(
                        "workflow task has no steps (set mode=auto to let the engine plan)".into()
                    );
                }
                if spec.goal.trim().is_empty() {
                    return Err("auto planning needs a non-empty goal".into());
                }
            }
        }
        bh_core::TaskKind::Interactive => {
            let Some(c) = &spec.controller else {
                return Err("interactive task has no controller".into());
            };
            if c.start_url.is_none() {
                return Err("interactive task needs controller.start_url".into());
            }
            if c.observe.script.trim().is_empty() {
                return Err("interactive task needs controller.observe.script".into());
            }
        }
    }
    if spec.policy.max_wall_ms == 0 {
        return Err("policy.max_wall_ms must be > 0".into());
    }
    Ok(())
}

fn default_artifact_dir(script: &Path) -> PathBuf {
    let stem = script.file_stem().and_then(|s| s.to_str()).unwrap_or("task");
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".bhx")
        .join("artifacts")
        .join(stem)
}

fn render(obs: &Observation) -> String {
    let mut s = obs.to_state_text("");
    if obs.truncated {
        s.push_str(&format!(
            "(truncated at {} elements; raise --max-elements to see more)\n",
            obs.elements.len()
        ));
    }
    s
}

fn summary(r: &TaskReport, artifacts: &Path) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "\n{} in {} ms\n",
        match r.status {
            TaskStatus::Succeeded => "SUCCEEDED",
            TaskStatus::Failed => "FAILED",
            TaskStatus::WaitingApproval => "WAITING FOR APPROVAL",
        },
        r.elapsed_ms
    ));
    for step in &r.steps {
        s.push_str(&format!(
            "  {:<10} {:<8} verified={:<5} {} ms  {}{}\n",
            step.id,
            format!("{:?}", step.status).to_lowercase(),
            step.verified,
            step.elapsed_ms,
            step.goal,
            step.error
                .as_ref()
                .map(|e| format!("  ({e})"))
                .unwrap_or_default()
        ));
    }
    if !r.artifacts.is_empty() {
        s.push_str(&format!("artifacts in {}:\n", artifacts.display()));
        for a in &r.artifacts {
            s.push_str(&format!("  {a}\n"));
        }
    }
    s
}

fn fail(e: &dyn std::fmt::Display) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "bhx: {e}");
    ExitCode::FAILURE
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

/// Every occurrence of a repeatable flag, in order.
fn flags(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        } else if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            out.push(v.to_string());
        }
    }
    out
}

/// Credentials are accepted only on the command line (or, for REST, in the
/// request body). They are never read from a task file or written back to one.
fn parse_credentials(args: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for raw in flags(args, "--cred") {
        let Some((name, value)) = raw.split_once('=') else {
            return Err(format!("--cred wants NAME=VALUE, got {raw:?}"));
        };
        if name.trim().is_empty() {
            return Err("--cred has an empty name".into());
        }
        out.insert(name.trim().to_string(), value.to_string());
    }
    Ok(out)
}
