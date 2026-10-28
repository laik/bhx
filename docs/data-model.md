# 持久化与配置 v0.1

## 1. 存储分层

| 数据 | 位置 | 生命周期 |
|---|---|---|
| 任务、步骤、事件、检查点、产物索引、审批 | SQLite | 长期 |
| 观察快照、截图 | 文件系统 | 按保留策略（默认 7 天） |
| 产物本体（下载的文件） | 文件系统 | 长期，直到用户删除 |
| 浏览器 profile | 文件系统 | 直到会话删除 |

SQLite 存**指针和状态**，不存大对象。快照与截图放文件系统，库里只存路径与哈希。

## 2. 目录布局

实现（Phase 2）：根目录默认 `<cwd>/.bhx`，可用 `--home` 或 `BHX_HOME` 覆盖。

```
<home>/
├── bhx.db
├── profiles/
│   └── <task-id>/               Chrome user-data-dir，登录态在这里
└── tasks/
    └── <task-ulid>/
        ├── task.json            任务快照（已剥离凭据）
        ├── events.jsonl         与 events 表同构，便于直接 tail/grep
        ├── tmp/                 下载暂存 + 提取写盘，成功后移入 artifacts/
        ├── obs/                 预留：观察快照
        ├── shots/               预留：截图
        └── artifacts/
            └── orders-已支付.csv
```

与最初设计的差异，都是实现时做的选择：

- **profile 按任务而不是按会话**（`profiles/<task-id>`）。任务之间互不共享 cookie，
  越权范围更小；代价是同一站点重复登录。会话级 profile 等 Phase 3 的多任务编排再引入。
- **`tmp/` 在任务目录内**，不在 home 根下。下载必须先落在任务自己的沙箱里，
  否则「这个文件属于哪个任务」要靠文件名猜。
- **`trace.jsonl` 尚未生成。** 动作级 trace 格式还没定（Phase 6 与回放一起做）；
  现在 diagnostics 走 `events.jsonl`。
- **`obs/`、`shots/` 只建目录、暂时为空。** Phase 2 没有自动截图，观察快照也不落盘。
- `tasks/<id>/` 在任务创建时建立；尚无删除任务的功能，`deleted_at` 目前恒为 NULL。

## 3. 表结构

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- ('schema_version', '1')

CREATE TABLE tasks (
  id                TEXT PRIMARY KEY,          -- ULID
  idempotency_key   TEXT UNIQUE,                -- 调用方重试保护
  kind              TEXT NOT NULL,              -- workflow | interactive
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

CREATE TABLE steps (
  task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  step_id     TEXT NOT NULL,
  idx         INTEGER NOT NULL,
  goal        TEXT NOT NULL,
  spec_json   TEXT NOT NULL,
  state       TEXT NOT NULL,       -- pending|running|succeeded|failed|skipped
  attempts    INTEGER NOT NULL DEFAULT 0,
  verified    INTEGER,             -- NULL = 未验证
  model_calls INTEGER NOT NULL DEFAULT 0,
  started_at  TEXT,
  finished_at TEXT,
  error_json  TEXT,
  PRIMARY KEY (task_id, step_id)
);

CREATE TABLE events (
  task_id  TEXT NOT NULL,
  seq      INTEGER NOT NULL,       -- 每任务单调递增，断线续传用
  ts       TEXT NOT NULL,
  type     TEXT NOT NULL,
  data_json TEXT NOT NULL,
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE checkpoints (
  task_id TEXT NOT NULL,
  seq     INTEGER NOT NULL,
  ts      TEXT NOT NULL,
  step_id TEXT,
  state_json TEXT NOT NULL,        -- 足够恢复的最小状态，不含页面内容
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE artifacts (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL,
  kind       TEXT NOT NULL,        -- download|extract|screenshot|trace
  name       TEXT,
  path       TEXT NOT NULL,
  bytes      INTEGER,
  sha256     TEXT,
  meta_json  TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE approvals (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL,
  step_id    TEXT,
  reason     TEXT NOT NULL,
  payload_json TEXT,               -- 动作、截图引用、模型建议
  created_at TEXT NOT NULL,
  decided_at TEXT,
  decision   TEXT                  -- approved|rejected
);

CREATE TABLE schedules (
  id              TEXT PRIMARY KEY,
  source          TEXT NOT NULL,    -- 添加时的脚本路径，仅用于展示
  spec_json       TEXT NOT NULL,    -- 添加时的任务快照；id 已清空、credentials 为空
  cred_names      TEXT NOT NULL,    -- JSON 数组：只存名字，值在运行时从环境读
  interval_ms     INTEGER NOT NULL,
  enabled         INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  last_run_at     TEXT,
  last_task_id    TEXT,
  last_state      TEXT,             -- running|succeeded|failed
  last_error      TEXT,
  next_run_at_ms  INTEGER NOT NULL  -- epoch ms；先占槽再运行
);

CREATE INDEX events_by_task_seq ON events(task_id, seq);
CREATE INDEX artifacts_by_task   ON artifacts(task_id);
CREATE INDEX tasks_by_state      ON tasks(state) WHERE deleted_at IS NULL;
CREATE INDEX schedules_by_due    ON schedules(next_run_at_ms) WHERE enabled = 1;
```

> **实现状态**：以上 DDL 已逐字落在 `crates/bh-task/src/store.rs`，`schema_meta` 写入
> `schema_version = 1`，启动时校验版本不符即报错。列已建但尚未写入的：
> `idempotency_key`（没有调用方重试入口，MCP `task_run` 尚未支持该字段）、
> `steps.model_calls`（按任务计一个共享的 `ModelRouter`，每步结束后把**累计值**
> 写进 `tasks.model_calls`；按步拆分留给 Phase 4 的规划/校验/修复）、
> `deleted_at`（无删除功能）。`tasks.model_calls` 是绝对计数，不是增量——
> 重复写同一个值是无害的，崩溃恢复后也不会翻倍。`schedules` 是 Phase 6 追加的表
> （`CREATE TABLE IF NOT EXISTS`，不改 `schema_version`），见 `crates/bh-task/src/store.rs`。

### 几个刻意的选择

- **`events` 不设自增主键。** 每任务独立 `seq`，断线续传语义简单，删除任务也干净。
- **`seq` 分配在事务内完成**（`INSERT ... SELECT COALESCE(MAX(seq),0)+1`），崩溃不会产生空洞之外的重复。
- **`verified` 是 `NULL` 而非 `0`。** 三态：未验证 / 验证失败 / 验证通过。把「没验证」和「验证失败」混成布尔是验收指标失真的根源。
- **`idempotency_key` 唯一索引**挡住调用方网络重试导致的重复任务。
- **`state_json` 只存恢复所需**，不存页面快照——后者不可重放，存了也会误导恢复逻辑。
  当前实现存 `{"next_index", "url", "completed"}`：`next_index` 决定从哪一步继续，
  `url` 只用于恢复时把浏览器带回原页面（失败则重新导航），`completed` 是已完成步数。

## 4. 凭据与脱敏

- 凭据只存在内存中的 `Session`（`TaskSpec.credentials`），**任何表、任何文件都不写**。
- 定时执行存的是**名字**：`schedules.cred_names` 记录任务引用了哪些 `${cred:…}`，
  值在每次运行开始时从 `BH_CRED_<NAME>` 读取（大写、非字母数字转 `_`）。缺任何一个
  就整次拒绝启动——部分替换会把 `${cred:password}` 当字面量敲进页面。
- `task.json` 落盘前：`credentials` 置为空对象，并把每个凭据的**字面值**在整份 JSON 里
  替换为 `[REDACTED]`（`spec_for_disk` + `scrub`）。只删字段不够——`${cred:pass}`
  解析后的明文会留在动作文本里。
- **执行器用解析后的副本，持久化用原始占位符副本**：`steps.spec_json`、`task.json`
  和事件里的动作文本都保留 `${cred:…}`，明文只存在于进程内存（自动规划的步骤也先
  落盘占位符、再解析执行）。
- 密码框的值在观察阶段就已是 `"***"`（`bh-observe`），不会进入事件或报告。
- 动作原文目前**不进事件**：`step.started` / `step.finished` 只记 `step_id`、`goal`、
  状态、耗时、错误。因此暂时不需要事件脱敏器；一旦要记动作参数，必须补上写入侧脱敏。
- 截图默认对 `input[type=password]` 区域打码、`redact_selectors`：**未实现**，
  因为 Phase 2 不自动截图。

脱敏是写入侧的职责，不是读取侧的。读侧过滤意味着原始凭据已经躺在磁盘上了。

## 5. 保留策略

| 数据 | 默认 | 说明 |
|---|---|---|
| 事件 | 30 天 | 超过后按任务级删除 |
| 观察快照 | 每任务保留最近 20 个；失败任务额外保留失败现场 | 用于事后排查 |
| 截图 | 仅在失败、审批、显式请求时保留 | 默认动作后不截图 |
| 产物 | 永久 | 用户显式删除 |
| 成功任务的 trace | 7 天 | 与指标聚合分开：聚合数据长期保留 |

**实现状态**：保留策略尚未实现。`events`、`artifacts` 与任务目录都只增不减——
在这些数据有实际体量之前，加清理逻辑只会制造「为什么我的失败现场没了」。
需要手工清理时直接删 `<home>/tasks/<id>` 与对应行即可（无软删除）。

## 6. 崩溃恢复

恢复是显式动作（`bhx task resume <id>` / MCP `task_resume`），不会在启动时偷偷重跑：

1. 任务有未决审批 → 直接返回 `waiting_approval`（退出码 3），不做任何执行。
2. 读最新 checkpoint 的 `next_index`；`url` 用于恢复后的首次导航。
3. 存在 `running` 的步骤 → 视为「动作已发出但未验证」，登记一条审批
   （payload `{"next_index": idx}`），转 `waiting_approval`，**不自动重试**。
4. 批准后从被中断的**那一步**继续，已完成步骤不重放。
5. 浏览器与会话都是重建的：按 `profiles/<task-id>/` 打开持久 profile，所以 localStorage
   里的登录态与筛选条件还在；页面 URL 来自检查点。

实现细节（都是踩过的坑）：

- **每个任务一个 pid 锁**（`tasks/<id>/lock` 存 pid）：同一任务不能并发跑两个引擎。
- **启动前回收残留浏览器**：按 profile 路径 `pgrep -f` 后 `kill -9`。崩溃会留下占着 profile
  的 Chrome，不清理的话 `Target.createTarget` 会连到旧实例，恢复执行的是另一个页面的状态。
- **检查点只在步骤成功后写**，所以「已完成的步骤」与检查点不会互相矛盾。
- **interactive 任务没有检查点，也不能 resume**：把输入流从头重放不是恢复。

恢复**不重放已完成的动作**，只从检查点继续。这是「宁可停下来问，也不要重复提交」的落地。

## 7. 配置文件

> **实现状态**：`config.toml` 尚未实现。当前配置只有 CLI flag / 环境变量
> （`--home`、`BHX_HOME`、`--headed`、`--artifacts`、`BH_CHROME`、`RUST_LOG`）
> 与任务内的 `policy`。浏览器默认 headless，`--headed` 才显示窗口。
> 下面的 TOML 仍是目标形态，Phase 3 引入视觉与模型适配器时再落地——现在写出来只是为了
> 固定字段名，避免各处自己发明键名。

```toml
[server]
bind = "127.0.0.1:7331"
mcp_transports = ["stdio"]          # 也可加 "http"

[browser]
executable = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
headless = false
profile_root = "~/.local/share/bhx/profiles"
default_viewport = { width = 1280, height = 800 }

[observation]
max_elements = 120
include_screenshot_by_default = false
screenshot_format = "jpeg"
screenshot_quality = 70

[policy]
allow_irreversible = false
max_model_calls = 2
max_wall_ms = 600000
allowed_hosts = []                  # 空 = 不限制；建议按站点收紧

[downloads]
dir = "~/.local/share/bhx/tasks"    # 实际落在 <task>/artifacts/
max_bytes = 104857600

# ---- 模型：按能力配置优先级链，前面的失败/不支持时降级 ----

[[models]]
capability = "plan"
adapter = "anthropic"
model = "claude-sonnet-5-5"
timeout_ms = 30000

[[models]]
capability = "locate"
adapter = "laya"
backend = "torch"
device = "mps"                      # cpu 会慢 6-9 倍
max_criteria = 10

[[models]]
capability = "locate"               # 降级链第二项
adapter = "anthropic"
model = "claude-haiku-5-5"
timeout_ms = 5000

[[models]]
capability = "visual_locate"
adapter = "anthropic"
model = "claude-sonnet-5-5"
timeout_ms = 15000

[[models]]
capability = "verify"
adapter = "laya"
backend = "torch"
device = "mps"

[laya]
sidecar = "sidecars/laya/decide.py"
python = "~/.local/share/laya-demo/.venv/bin/python"
startup_timeout_ms = 15000          # 冷启动 5.5s + 首次推理 430ms
```

### 配置的原则

- **能力先于模型。** 配置的是「谁负责 plan」，不是「用哪个模型」；换模型不动调用方。
- **降级链显式。** 本地模型不可用时降级到远程模型，顺序写在配置里，不藏在代码里。
- **预算可被任务覆盖，但只能收紧不能放宽。** 任务可以要求「更便宜」，不能要求「更贵」。
  这条要在 `bh-task` 里检查，不能只写文档。

## 8. 从 CLI 到 MCP 的注册

```bash
bhx mcp                         # stdio MCP server，给 Claude Code 用
bhx run task.json               # 一次性跑步骤脚本，不落任务状态
bhx task run task.json          # 落库、检查点、产物、审批
bhx task ls / show <id> / artifacts <id> / resume <id> / approve <id> / cancel <id>
bhx task recover                # 列出被崩溃打断的任务
```

`bhx serve`（REST + WS）尚未实现，Phase 4 与 MCP 全量工具面一起做。

Claude Code 侧：

```json
{
  "mcpServers": {
    "bhx": { "command": "bhx", "args": ["mcp"] }
  }
}
```
