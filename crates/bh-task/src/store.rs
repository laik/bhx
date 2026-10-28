//! SQLite store: tasks, steps, events, checkpoints, artifacts, approvals.
//!
//! The schema is `docs/data-model.md` §3, verbatim. Two rules from that doc are
//! load-bearing here: `seq` is allocated inside the insert statement, and
//! `steps.verified` is tri-state (NULL = never verified).

use std::collections::BTreeMap;
use std::path::Path;

use bh_core::{BhError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

pub const SCHEMA_VERSION: &str = "1";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
  id                TEXT PRIMARY KEY,
  idempotency_key   TEXT UNIQUE,
  kind              TEXT NOT NULL,
  goal              TEXT NOT NULL,
  mode              TEXT NOT NULL,
  policy_json       TEXT NOT NULL,
  state             TEXT NOT NULL,
  session_id        TEXT NOT NULL,
  model_calls       INTEGER NOT NULL DEFAULT 0,
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  finished_at       TEXT,
  result_json       TEXT,
  error_json        TEXT,
  deleted_at        TEXT
);

CREATE TABLE IF NOT EXISTS steps (
  task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  step_id     TEXT NOT NULL,
  idx         INTEGER NOT NULL,
  goal        TEXT NOT NULL,
  spec_json   TEXT NOT NULL,
  state       TEXT NOT NULL,
  attempts    INTEGER NOT NULL DEFAULT 0,
  verified    INTEGER,
  model_calls INTEGER NOT NULL DEFAULT 0,
  started_at  TEXT,
  finished_at TEXT,
  error_json  TEXT,
  PRIMARY KEY (task_id, step_id)
);

CREATE TABLE IF NOT EXISTS events (
  task_id   TEXT NOT NULL,
  seq       INTEGER NOT NULL,
  ts        TEXT NOT NULL,
  type      TEXT NOT NULL,
  data_json TEXT NOT NULL,
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE IF NOT EXISTS checkpoints (
  task_id    TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  ts         TEXT NOT NULL,
  step_id    TEXT,
  state_json TEXT NOT NULL,
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE IF NOT EXISTS artifacts (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL,
  kind       TEXT NOT NULL,
  name       TEXT,
  path       TEXT NOT NULL,
  bytes      INTEGER,
  sha256     TEXT,
  meta_json  TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS approvals (
  id           TEXT PRIMARY KEY,
  task_id      TEXT NOT NULL,
  step_id      TEXT,
  reason       TEXT NOT NULL,
  payload_json TEXT,
  created_at   TEXT NOT NULL,
  decided_at   TEXT,
  decision     TEXT
);

CREATE TABLE IF NOT EXISTS schedules (
  id              TEXT PRIMARY KEY,
  source          TEXT NOT NULL,
  spec_json       TEXT NOT NULL,
  cred_names      TEXT NOT NULL,
  interval_ms     INTEGER NOT NULL,
  enabled         INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  last_run_at     TEXT,
  last_task_id    TEXT,
  last_state      TEXT,
  last_error      TEXT,
  next_run_at_ms  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS events_by_task_seq ON events(task_id, seq);
CREATE INDEX IF NOT EXISTS artifacts_by_task   ON artifacts(task_id);
CREATE INDEX IF NOT EXISTS tasks_by_state      ON tasks(state) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS schedules_by_due    ON schedules(next_run_at_ms) WHERE enabled = 1;
"#;

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct NewTask<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub goal: &'a str,
    pub mode: &'a str,
    pub policy_json: &'a str,
    pub session_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: String,
    pub kind: String,
    pub goal: String,
    pub mode: String,
    pub state: String,
    pub model_calls: i64,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub result_json: Option<String>,
    pub error_json: Option<String>,
}

/// Aggregates from `Store::stats`. The derived numbers live here, not in the
/// SQL, so the definitions are in one place and can be tested without a
/// database.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub tasks: i64,
    pub by_state: BTreeMap<String, i64>,
    pub model_calls: i64,
    /// Sorted; empty when nothing has finished yet.
    pub durations_ms: Vec<u64>,
    pub actions: i64,
    pub failed_actions: i64,
}

impl Stats {
    /// Canceled runs are the caller's decision, not a failure; they are
    /// excluded from the success rate but still counted in `tasks`.
    pub fn finished(&self) -> i64 {
        self.by_state.get("succeeded").copied().unwrap_or(0)
            + self.by_state.get("failed").copied().unwrap_or(0)
    }

    pub fn success_rate(&self) -> Option<f64> {
        let finished = self.finished();
        if finished == 0 {
            return None;
        }
        Some(self.by_state.get("succeeded").copied().unwrap_or(0) as f64 / finished as f64)
    }

    /// Nearest-rank percentile; `0` when nothing finished.
    pub fn duration_ms(&self, p: usize) -> u64 {
        if self.durations_ms.is_empty() {
            return 0;
        }
        let rank = (self.durations_ms.len() * p).div_ceil(100).clamp(1, self.durations_ms.len()) - 1;
        self.durations_ms[rank]
    }

    pub fn action_failure_rate(&self) -> Option<f64> {
        if self.actions == 0 {
            return None;
        }
        Some(self.failed_actions as f64 / self.actions as f64)
    }
}

#[derive(Debug, Clone)]
pub struct StepRow {
    pub step_id: String,
    pub idx: i64,
    pub goal: String,
    pub state: String,
    pub attempts: i64,
    pub verified: Option<bool>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct CheckpointRow {
    pub seq: i64,
    pub step_id: Option<String>,
    pub state: Value,
}

#[derive(Debug, Clone)]
pub struct ArtifactRow {
    pub id: String,
    pub kind: String,
    pub name: Option<String>,
    pub path: String,
    pub bytes: i64,
    pub sha256: Option<String>,
}

/// A stored schedule. `spec_json` is the task itself, snapshot at add time, so
/// editing the source file later does not silently change what runs. It holds
/// no credential values — only `cred_names`, which the runner resolves from
/// the environment.
#[derive(Debug, Clone)]
pub struct ScheduleRow {
    pub id: String,
    pub source: String,
    pub spec_json: String,
    pub cred_names: Vec<String>,
    pub interval_ms: i64,
    pub enabled: bool,
    pub created_at: String,
    pub last_run_at: Option<String>,
    pub last_task_id: Option<String>,
    pub last_state: Option<String>,
    pub last_error: Option<String>,
    pub next_run_at_ms: i64,
}

pub struct NewSchedule<'a> {
    pub id: &'a str,
    pub source: &'a str,
    pub spec_json: &'a str,
    pub cred_names: &'a [String],
    pub interval_ms: i64,
    pub next_run_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ApprovalRow {
    pub id: String,
    pub task_id: String,
    pub step_id: Option<String>,
    pub reason: String,
    pub payload: Value,
    pub decided_at: Option<String>,
    pub decision: Option<String>,
}

fn db_err(e: rusqlite::Error) -> BhError {
    BhError::Other(format!("sqlite: {e}"))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| BhError::Other(format!("create {}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(db_err)?;
        // More than one engine can hold this database at once: the MCP server
        // keeps one for task tools while each background interactive job opens
        // its own. A writer that arrives during another write waits instead of
        // failing with SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_millis(5_000)).map_err(db_err)?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;

        let known: Option<String> = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        match known.as_deref() {
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) => {
                return Err(BhError::Other(format!(
                    "database schema {v} is newer or older than this build ({SCHEMA_VERSION})"
                )));
            }
            None => {
                conn.execute(
                    "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION],
                )
                .map_err(db_err)?;
            }
        }
        Ok(Self { conn })
    }

    // ---------------------------------------------------------------- tasks

    pub fn create_task(&self, t: &NewTask<'_>) -> Result<()> {
        let ts = now();
        self.conn
            .execute(
                "INSERT INTO tasks (id, kind, goal, mode, policy_json, state, session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?7)",
                params![t.id, t.kind, t.goal, t.mode, t.policy_json, t.session_id, ts],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_task_state(&self, id: &str, state: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, state, now()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Absolute count, not a delta: the router owns the counter for the whole
    /// task, so re-running a step cannot double-count earlier calls.
    pub fn set_model_calls(&self, id: &str, calls: u32) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tasks SET model_calls = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, i64::from(calls), now()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn finish_task(
        &self,
        id: &str,
        state: &str,
        result: Option<&Value>,
        error: Option<&Value>,
    ) -> Result<()> {
        let ts = now();
        self.conn
            .execute(
                "UPDATE tasks SET state = ?2, updated_at = ?3, finished_at = ?3,
                    result_json = ?4, error_json = ?5
                 WHERE id = ?1",
                params![
                    id,
                    state,
                    ts,
                    result.map(|v| v.to_string()),
                    error.map(|v| v.to_string())
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<TaskRow> {
        self.conn
            .query_row(
                "SELECT id, kind, goal, mode, state, model_calls, created_at, updated_at,
                        finished_at, result_json, error_json
                 FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| BhError::Other(format!("task {id} not found")))
    }

    pub fn tasks(&self, limit: usize) -> Result<Vec<TaskRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, goal, mode, state, model_calls, created_at, updated_at,
                        finished_at, result_json, error_json
                 FROM tasks WHERE deleted_at IS NULL ORDER BY created_at DESC LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_task)
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Aggregate view over everything recorded in this home. Counts are over
    /// live tasks only; durations only over tasks that finished. Action counts
    /// only cover events written after `action.executed` existed, so a home
    /// with older tasks reports a smaller denominator rather than a guess.
    pub fn stats(&self) -> Result<Stats> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM tasks WHERE deleted_at IS NULL GROUP BY state")
            .map_err(db_err)?;
        let by_state: BTreeMap<String, i64> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(db_err)?
            .collect::<std::result::Result<_, _>>()
            .map_err(db_err)?;
        let tasks: i64 = by_state.values().sum();

        let mut stmt = self
            .conn
            .prepare(
                "SELECT CAST((julianday(finished_at) - julianday(created_at)) * 86400000 AS INTEGER)
                 FROM tasks
                 WHERE deleted_at IS NULL AND finished_at IS NOT NULL
                   AND created_at IS NOT NULL AND state != 'waiting_approval'",
            )
            .map_err(db_err)?;
        let mut durations: Vec<u64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(db_err)?
            .filter_map(|v| v.ok())
            .filter(|v| *v >= 0)
            .map(|v| v as u64)
            .collect();
        durations.sort_unstable();

        let model_calls: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(model_calls), 0) FROM tasks WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;

        let (actions, failed_actions): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN json_extract(data_json, '$.ok') = 0 THEN 1 ELSE 0 END), 0)
                 FROM events WHERE type = 'action.executed'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_err)?;

        Ok(Stats {
            tasks,
            by_state,
            model_calls,
            durations_ms: durations,
            actions,
            failed_actions,
        })
    }

    /// Tasks left in `running` belong to a process that is no longer here.
    pub fn reap_running(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM tasks WHERE state = 'running' AND deleted_at IS NULL")
            .map_err(db_err)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        for id in &ids {
            self.set_task_state(id, "crashed")?;
        }
        Ok(ids)
    }

    // ---------------------------------------------------------------- steps

    pub fn upsert_step(
        &self,
        task_id: &str,
        step_id: &str,
        idx: usize,
        goal: &str,
        spec_json: &str,
        state: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO steps (task_id, step_id, idx, goal, spec_json, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(task_id, step_id) DO UPDATE SET
                    idx = excluded.idx, goal = excluded.goal,
                    spec_json = excluded.spec_json, state = excluded.state",
                params![task_id, step_id, idx as i64, goal, spec_json, state],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn start_step(&self, task_id: &str, step_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE steps SET state = 'running', started_at = ?3,
                        attempts = attempts + 1
                 WHERE task_id = ?1 AND step_id = ?2",
                params![task_id, step_id, now()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn finish_step(
        &self,
        task_id: &str,
        step_id: &str,
        state: &str,
        verified: Option<bool>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE steps SET state = ?3, verified = ?4, error_json = ?5, finished_at = ?6
                 WHERE task_id = ?1 AND step_id = ?2",
                params![
                    task_id,
                    step_id,
                    state,
                    verified.map(|v| v as i64),
                    error.map(|e| serde_json::json!({ "message": e }).to_string()),
                    now()
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn steps(&self, task_id: &str) -> Result<Vec<StepRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT step_id, idx, goal, state, attempts, verified, error_json,
                        started_at, finished_at
                 FROM steps WHERE task_id = ?1 ORDER BY idx ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                let err: Option<String> = r.get(6)?;
                let err = err.and_then(|e| {
                    serde_json::from_str::<Value>(&e)
                        .ok()
                        .and_then(|v| v["message"].as_str().map(str::to_string))
                });
                Ok(StepRow {
                    step_id: r.get(0)?,
                    idx: r.get(1)?,
                    goal: r.get(2)?,
                    state: r.get(3)?,
                    attempts: r.get(4)?,
                    verified: r.get::<_, Option<i64>>(5)?.map(|v| v != 0),
                    error: err,
                    started_at: r.get(7)?,
                    finished_at: r.get(8)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Steps still marked running when the process died. Per the recovery
    /// rules these are never retried automatically: a side effect may have
    /// happened, and only a human can say whether it did.
    pub fn interrupted_steps(&self, task_id: &str) -> Result<Vec<StepRow>> {
        Ok(self
            .steps(task_id)?
            .into_iter()
            .filter(|s| s.state == "running")
            .collect())
    }

    // ---------------------------------------------------------------- events

    pub fn append_event(&self, task_id: &str, kind: &str, data: &Value) -> Result<i64> {
        let ts = now();
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO events (task_id, seq, ts, type, data_json)
             SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4 FROM events WHERE task_id = ?1",
            params![task_id, ts, kind, data.to_string()],
        )
        .map_err(db_err)?;
        let seq: i64 = tx
            .query_row(
                "SELECT seq FROM events WHERE task_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(seq)
    }

    pub fn events(&self, task_id: &str, since: i64) -> Result<Vec<EventRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, ts, type, data_json FROM events
                 WHERE task_id = ?1 AND seq > ?2 ORDER BY seq ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![task_id, since], |r| {
                let raw: String = r.get(3)?;
                Ok(EventRow {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    kind: r.get(2)?,
                    data: serde_json::from_str(&raw).unwrap_or(Value::Null),
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    // ------------------------------------------------------------ checkpoints

    pub fn save_checkpoint(&self, task_id: &str, step_id: Option<&str>, state: &Value) -> Result<i64> {
        let ts = now();
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO checkpoints (task_id, seq, ts, step_id, state_json)
             SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4 FROM checkpoints WHERE task_id = ?1",
            params![task_id, ts, step_id, state.to_string()],
        )
        .map_err(db_err)?;
        let seq: i64 = tx
            .query_row(
                "SELECT seq FROM checkpoints WHERE task_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(seq)
    }

    pub fn last_checkpoint(&self, task_id: &str) -> Result<Option<CheckpointRow>> {
        self.conn
            .query_row(
                "SELECT seq, step_id, state_json FROM checkpoints
                 WHERE task_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![task_id],
                |r| {
                    let raw: String = r.get(2)?;
                    Ok(CheckpointRow {
                        seq: r.get(0)?,
                        step_id: r.get(1)?,
                        state: serde_json::from_str(&raw).unwrap_or(Value::Null),
                    })
                },
            )
            .optional()
            .map_err(db_err)
    }

    // ------------------------------------------------------------- artifacts

    pub fn add_artifact(
        &self,
        task_id: &str,
        kind: &str,
        name: Option<&str>,
        path: &str,
        bytes: i64,
        sha256: Option<&str>,
    ) -> Result<String> {
        let id = ulid::Ulid::new().to_string();
        self.conn
            .execute(
                "INSERT INTO artifacts (id, task_id, kind, name, path, bytes, sha256, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, task_id, kind, name, path, bytes, sha256, now()],
            )
            .map_err(db_err)?;
        Ok(id)
    }

    pub fn artifacts(&self, task_id: &str) -> Result<Vec<ArtifactRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, name, path, bytes, sha256 FROM artifacts
                 WHERE task_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(ArtifactRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    path: r.get(3)?,
                    bytes: r.get(4)?,
                    sha256: r.get(5)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    // ------------------------------------------------------------- approvals

    pub fn request_approval(
        &self,
        task_id: &str,
        step_id: Option<&str>,
        reason: &str,
        payload: &Value,
    ) -> Result<String> {
        let id = ulid::Ulid::new().to_string();
        self.conn
            .execute(
                "INSERT INTO approvals (id, task_id, step_id, reason, payload_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, task_id, step_id, reason, payload.to_string(), now()],
            )
            .map_err(db_err)?;
        Ok(id)
    }

    pub fn decide_approval(&self, task_id: &str, decision: &str) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "UPDATE approvals SET decision = ?2, decided_at = ?3
                 WHERE task_id = ?1 AND decision IS NULL",
                params![task_id, decision, now()],
            )
            .map_err(db_err)?;
        Ok(n)
    }

    pub fn approvals(&self, task_id: &str) -> Result<Vec<ApprovalRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, step_id, reason, payload_json, decided_at, decision
                 FROM approvals WHERE task_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                let raw: Option<String> = r.get(4)?;
                Ok(ApprovalRow {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    step_id: r.get(2)?,
                    reason: r.get(3)?,
                    payload: raw
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(Value::Null),
                    decided_at: r.get(5)?,
                    decision: r.get(6)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Steps a human has already approved, keyed by step id.
    pub fn approved_step_ids(&self, task_id: &str) -> Result<Vec<String>> {
        Ok(self
            .approvals(task_id)?
            .into_iter()
            .filter(|a| a.decision.as_deref() == Some("approved"))
            .filter_map(|a| a.step_id)
            .collect())
    }

    // ------------------------------------------------------------ schedules

    pub fn add_schedule(&self, s: &NewSchedule<'_>) -> Result<()> {
        let names = serde_json::to_string(s.cred_names)
            .map_err(|e| BhError::Other(format!("schedule cred names: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO schedules (id, source, spec_json, cred_names, interval_ms, enabled,
                                        created_at, next_run_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
                params![s.id, s.source, s.spec_json, names, s.interval_ms, now(), s.next_run_at_ms],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>> {
        self.query_schedules(
            "SELECT id, source, spec_json, cred_names, interval_ms, enabled, created_at,
                    last_run_at, last_task_id, last_state, last_error, next_run_at_ms
             FROM schedules ORDER BY created_at ASC",
            [],
        )
    }

    /// Enabled schedules whose next slot is at or before `at_ms`.
    pub fn due_schedules(&self, at_ms: i64) -> Result<Vec<ScheduleRow>> {
        self.query_schedules(
            "SELECT id, source, spec_json, cred_names, interval_ms, enabled, created_at,
                    last_run_at, last_task_id, last_state, last_error, next_run_at_ms
             FROM schedules WHERE enabled = 1 AND next_run_at_ms <= ?1
             ORDER BY next_run_at_ms ASC",
            params![at_ms],
        )
    }

    pub fn schedule(&self, id: &str) -> Result<Option<ScheduleRow>> {
        Ok(self
            .query_schedules(
                "SELECT id, source, spec_json, cred_names, interval_ms, enabled, created_at,
                        last_run_at, last_task_id, last_state, last_error, next_run_at_ms
                 FROM schedules WHERE id = ?1",
                params![id],
            )?
            .into_iter()
            .next())
    }

    /// Claims a due slot before running it: advances `next_run_at_ms` to
    /// `at_ms + interval` and marks the schedule running.
    ///
    /// The slot is consumed *before* the run, not after. If the process dies
    /// mid-run the schedule simply waits for its next slot — it does not
    /// blindly retry a task that may already have submitted an irreversible
    /// action. Advancing from `at_ms` rather than from the old slot also means
    /// a process that slept through three intervals runs once, not three times.
    pub fn claim_schedule(&self, id: &str, at_ms: i64) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE schedules
                 SET last_run_at = ?2, last_state = 'running', last_error = NULL,
                     next_run_at_ms = ?3 + interval_ms
                 WHERE id = ?1 AND enabled = 1 AND next_run_at_ms <= ?3",
                params![id, now(), at_ms],
            )
            .map_err(db_err)?;
        Ok(changed > 0)
    }

    /// Records how a claimed run ended. The next slot was already set by
    /// `claim_schedule`, so a failure does not schedule a retry either.
    ///
    /// `task_id` is written as given: a run that never created a task clears
    /// the link, so `ls` cannot show an old task next to a fresh failure.
    pub fn finish_schedule_run(
        &self,
        id: &str,
        task_id: Option<&str>,
        state: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE schedules
                 SET last_task_id = ?2, last_state = ?3, last_error = ?4
                 WHERE id = ?1",
                params![id, task_id, state, error],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_schedule_enabled(&self, id: &str, enabled: bool, at_ms: i64) -> Result<bool> {
        // Re-enabling starts a fresh interval; otherwise a disabled schedule
        // would be overdue the moment it came back and fire immediately.
        let changed = self
            .conn
            .execute(
                "UPDATE schedules SET enabled = ?2,
                        next_run_at_ms = CASE WHEN ?2 = 1 THEN ?3 + interval_ms ELSE next_run_at_ms END
                 WHERE id = ?1",
                params![id, enabled as i64, at_ms],
            )
            .map_err(db_err)?;
        Ok(changed > 0)
    }

    pub fn remove_schedule(&self, id: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])
            .map_err(db_err)?;
        Ok(changed > 0)
    }

    fn query_schedules<P: rusqlite::Params>(&self, sql: &str, p: P) -> Result<Vec<ScheduleRow>> {
        let mut stmt = self.conn.prepare(sql).map_err(db_err)?;
        let rows = stmt
            .query_map(p, |r| {
                let names: String = r.get(3)?;
                Ok(ScheduleRow {
                    id: r.get(0)?,
                    source: r.get(1)?,
                    spec_json: r.get(2)?,
                    cred_names: serde_json::from_str(&names).unwrap_or_default(),
                    interval_ms: r.get(4)?,
                    enabled: r.get::<_, i64>(5)? != 0,
                    created_at: r.get(6)?,
                    last_run_at: r.get(7)?,
                    last_task_id: r.get(8)?,
                    last_state: r.get(9)?,
                    last_error: r.get(10)?,
                    next_run_at_ms: r.get(11)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }
}

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        goal: r.get(2)?,
        mode: r.get(3)?,
        state: r.get(4)?,
        model_calls: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        finished_at: r.get(8)?,
        result_json: r.get(9)?,
        error_json: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("bhx-store-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("bhx.db")).unwrap(), dir)
    }

    #[test]
    fn event_seq_is_monotonic_and_per_task() {
        let (s, dir) = tmp_store();
        for id in ["a", "b"] {
            s.create_task(&NewTask {
                id,
                kind: "workflow",
                goal: "g",
                mode: "auto",
                policy_json: "{}",
                session_id: id,
            })
            .unwrap();
        }
        assert_eq!(s.append_event("a", "x", &serde_json::json!({})).unwrap(), 1);
        assert_eq!(s.append_event("a", "x", &serde_json::json!({})).unwrap(), 2);
        assert_eq!(s.append_event("b", "x", &serde_json::json!({})).unwrap(), 1);
        assert_eq!(s.events("a", 0).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_roundtrip() {
        let (s, dir) = tmp_store();
        s.create_task(&NewTask {
            id: "t1",
            kind: "workflow",
            goal: "g",
            mode: "auto",
            policy_json: "{}",
            session_id: "t1",
        })
        .unwrap();
        s.save_checkpoint("t1", Some("s1"), &serde_json::json!({"next_index": 1}))
            .unwrap();
        s.save_checkpoint("t1", Some("s2"), &serde_json::json!({"next_index": 2}))
            .unwrap();
        let c = s.last_checkpoint("t1").unwrap().unwrap();
        assert_eq!(c.seq, 2);
        assert_eq!(c.state["next_index"], 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stats_leave_canceled_runs_out_of_the_success_rate() {
        let st = Stats {
            tasks: 4,
            by_state: [("succeeded".to_string(), 2), ("failed".to_string(), 1), ("canceled".to_string(), 1)]
                .into_iter()
                .collect(),
            model_calls: 3,
            durations_ms: vec![100, 200, 900],
            actions: 10,
            failed_actions: 2,
        };
        assert_eq!(st.finished(), 3);
        assert_eq!(st.success_rate(), Some(2.0 / 3.0));
        assert_eq!(st.duration_ms(50), 200);
        assert_eq!(st.duration_ms(95), 900);
        assert_eq!(st.action_failure_rate(), Some(0.2));

        let empty = Stats {
            tasks: 0,
            by_state: Default::default(),
            model_calls: 0,
            durations_ms: Vec::new(),
            actions: 0,
            failed_actions: 0,
        };
        assert_eq!(empty.success_rate(), None, "no finished runs is not 0%");
        assert_eq!(empty.action_failure_rate(), None);
        assert_eq!(empty.duration_ms(50), 0);
    }

    #[test]
    fn stats_read_actions_and_durations_from_the_store() {
        let (s, dir) = tmp_store();
        s.create_task(&NewTask {
            id: "t3",
            kind: "workflow",
            goal: "g",
            mode: "auto",
            policy_json: "{}",
            session_id: "t3",
        })
        .unwrap();
        s.append_event("t3", "action.executed", &serde_json::json!({"ok": true})).unwrap();
        s.append_event("t3", "action.executed", &serde_json::json!({"ok": false})).unwrap();
        let st = s.stats().unwrap();
        assert_eq!(st.tasks, 1);
        assert_eq!(st.actions, 2);
        assert_eq!(st.failed_actions, 1);
        assert_eq!(st.action_failure_rate(), Some(0.5));
        assert!(st.durations_ms.is_empty(), "a task still running has no duration");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn running_tasks_are_reaped() {
        let (s, dir) = tmp_store();
        s.create_task(&NewTask {
            id: "t2",
            kind: "workflow",
            goal: "g",
            mode: "auto",
            policy_json: "{}",
            session_id: "t2",
        })
        .unwrap();
        s.set_task_state("t2", "running").unwrap();
        assert_eq!(s.reap_running().unwrap(), vec!["t2".to_string()]);
        assert_eq!(s.task("t2").unwrap().state, "crashed");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn add_test_schedule(s: &Store, id: &str, interval_ms: i64, next: i64) {
        s.add_schedule(&NewSchedule {
            id,
            source: "task.json",
            spec_json: r#"{"goal":"g"}"#,
            cred_names: &["password".to_string()],
            interval_ms,
            next_run_at_ms: next,
        })
        .unwrap();
    }

    #[test]
    fn schedules_are_due_once_and_never_catch_up_in_a_burst() {
        let (s, dir) = tmp_store();
        add_test_schedule(&s, "sch1", 60_000, 1_000);

        assert!(s.due_schedules(999).unwrap().is_empty(), "not due before its slot");
        let due = s.due_schedules(1_000).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].cred_names, vec!["password".to_string()], "only names are stored");
        assert!(due[0].spec_json.contains("\"goal\""), "the spec snapshot is stored");

        // Three intervals pass while the scheduler is asleep.
        assert!(s.claim_schedule("sch1", 181_000).unwrap());
        assert!(!s.claim_schedule("sch1", 181_000).unwrap(), "a slot can only be claimed once");
        assert!(s.due_schedules(181_000).unwrap().is_empty(), "no catch-up run");
        assert!(s.due_schedules(240_999).unwrap().is_empty());
        assert_eq!(s.due_schedules(241_000).unwrap().len(), 1, "next slot is now + interval");

        s.finish_schedule_run("sch1", Some("t9"), "succeeded", None).unwrap();
        let row = s.schedule("sch1").unwrap().unwrap();
        assert_eq!(row.last_task_id.as_deref(), Some("t9"));
        assert_eq!(row.last_state.as_deref(), Some("succeeded"));
        assert!(row.last_run_at.is_some());

        // A run that dies before creating a task must not keep pointing at the
        // previous run's task.
        s.finish_schedule_run("sch1", None, "failed", Some("missing credential")).unwrap();
        let row = s.schedule("sch1").unwrap().unwrap();
        assert_eq!(row.last_task_id, None);
        assert_eq!(row.last_error.as_deref(), Some("missing credential"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_claimed_slot_is_not_retried_after_a_crash() {
        let (s, dir) = tmp_store();
        add_test_schedule(&s, "sch3", 60_000, 0);
        assert!(s.claim_schedule("sch3", 0).unwrap());
        // The process dies here: last_state stays "running" and no finish call
        // arrives. Nothing runs again until the next slot.
        drop(s);
        let s = Store::open(&dir.join("bhx.db")).unwrap();
        assert!(s.due_schedules(59_999).unwrap().is_empty());
        let row = s.schedule("sch3").unwrap().unwrap();
        assert_eq!(row.last_state.as_deref(), Some("running"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_schedules_skip_and_reenable_from_now() {
        let (s, dir) = tmp_store();
        add_test_schedule(&s, "sch2", 10_000, 0);
        assert_eq!(s.due_schedules(0).unwrap().len(), 1);

        assert!(s.set_schedule_enabled("sch2", false, 0).unwrap());
        assert!(s.due_schedules(50_000).unwrap().is_empty(), "disabled schedules do not run");

        assert!(s.set_schedule_enabled("sch2", true, 50_000).unwrap());
        assert!(s.due_schedules(50_000).unwrap().is_empty(), "re-enabling waits one interval");
        assert_eq!(s.due_schedules(60_000).unwrap().len(), 1);

        assert!(!s.set_schedule_enabled("missing", true, 0).unwrap());
        assert!(s.remove_schedule("sch2").unwrap());
        assert!(!s.remove_schedule("sch2").unwrap());
        assert!(s.schedules().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
