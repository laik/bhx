# 内部接口 v0.1

这份文件定义 crate 之间的缝。实现可以改内部，**但不能改这里的语义**，
否则执行器、任务引擎、模型适配器会各自长出不一致的假设。

代码用 Rust 签名表达；`Result` 一律是 `bh_core::Result<T> = std::result::Result<T, BhError>`。

## 1. bh-core：协议类型

```rust
pub struct Task {
    pub protocol_version: String,
    pub id: TaskId,
    pub kind: TaskKind,
    pub goal: String,
    pub mode: ExecMode,
    pub policy: TaskPolicy,
    pub steps: Vec<Step>,          // kind=interactive 时为空
    pub controller: Option<ControllerPolicy>,
}

pub enum TaskKind { Workflow, Interactive }
pub enum ExecMode { Auto, Dom, Vision, Interactive }
pub enum SideEffect { ReadOnly, Reversible, Irreversible }
pub enum TaskState {
    Pending, Running, WaitingInput, WaitingApproval, Paused,
    Succeeded, Failed, Canceled, Crashed, Recovering,
}

pub struct TaskPolicy {
    pub max_model_calls: u32,
    pub max_wall_ms: u64,
    pub allow_irreversible: bool,
    pub allowed_hosts: Vec<String>,
    pub observation: ObservationBudget,
    pub semantic_verify: Option<SemanticVerify>,   // Phase 4：任务级第二意见
}

pub struct SemanticVerify {
    pub goal: Option<String>,      // None/空 => 任务 goal
    pub threshold: f64,            // 默认 0.6，必须 0..=1
    pub on_uncertain: OnUncertain, // WaitHuman（默认）/ Fail / Continue
    pub include_screenshot: bool,  // 默认 false：把视口 PNG 一起发给校验模型
}

pub struct Step {
    pub id: StepId,
    pub goal: String,
    pub executor: ExecMode,
    pub preconditions: Vec<Condition>,
    pub actions: Vec<Action>,
    pub success: Option<Condition>,
    pub fail_when: Vec<Condition>,      // Phase 2：失败信号的提前退出
    pub fallback_visual: Option<String>,// Phase 3：DOM 定位失败后的改道描述
    pub timeout_ms: u64,
    pub retry: RetryPolicy,
    pub side_effect: SideEffect,
    pub requires_approval: bool,
}
```

### 目标与定位器

定位器是**有序降级链**，不是一组可选字段。解析按 `any_of` 顺序尝试，首个唯一命中即用。

```rust
pub struct Locator {
    pub any_of: Vec<LocatorKind>,       // 空则视为非法
    pub within: Option<Box<Locator>>,   // 缩窄容器的定位器
    pub frame: FrameSelector,           // Main | ByUrl(String) | Any
}

pub enum LocatorKind {
    Role { role: String, name: Option<String>, exact: bool },
    Label(String),
    TestId(String),
    Placeholder(String),
    Alt(String),
    Title(String),
    Text { text: String, tag: Option<String> },
    Css(String),
    XPath(String),
    Visual { description: String, near: Option<String> },
}

pub enum Target {
    Locator(Locator),
    Point(Point),       // 仅 Vision / Interactive 产生
    Visual { visual: String },  // Phase 3 实装：截图里用自然语言描述的目标
}
```

**已实现的差异**：设计稿把 `Visual` 放在 `LocatorKind` 里，实现放在 `Target` 上。
原因是视觉目标不参与 `any_of` 降级链——它要么整条动作走视觉，要么不走；混进
定位链会让「DOM 没找到 → 试个坐标」变成隐式行为。`fallback_visual` 才是显式升级点。

**解析结果必须携带 `observation_id`。** 元素句柄（backendNodeId）在导航后失效，
所有动作执行前都要校验 `observation_id` 是否仍是当前观察，否则先重新解析。

```rust
pub struct ResolvedTarget {
    pub observation_id: ObservationId,
    pub eid: String,
    pub frame_id: String,
    pub backend_node_id: i64,
    pub box: Rect,
    pub strategy: &'static str,     // 命中的 LocatorKind 名，进 trace
    pub confidence: f32,
}
```

### 观察

```rust
pub struct Observation {
    pub id: ObservationId,
    pub seq: u64,
    pub state_format_version: u32,
    pub url: String,
    pub title: String,
    pub viewport: Viewport,          // w, h, dpr, scroll_x, scroll_y
    pub elements: Vec<ElementInfo>,
    pub text_blocks: Vec<TextBlock>,
    pub dialogs: Vec<DialogInfo>,
    pub truncated: bool,
}

pub struct ElementInfo {
    pub eid: String,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub box: Rect,
    pub states: Vec<ElementState>,   // enabled/visible/checked/disabled...
    pub hash: u64,
}
```

## 2. 观察引擎

```rust
#[async_trait]
pub trait Observer: Send + Sync {
    /// 轻量：元素索引 + URL/标题 + 对话框。动作后默认调用。
    async fn observe(&mut self, budget: &ObservationBudget) -> Result<Observation>;

    /// 完整：AX 树 + 关键 DOM 属性 + 盒模型。步骤开始、定位失败时调用。
    async fn snapshot(&mut self, opts: SnapshotOpts) -> Result<Snapshot>;

    /// 截图。region 决定成本，默认只给 viewport。
    async fn screenshot(&mut self, region: ShotRegion) -> Result<Screenshot>;

    /// 与指定观察的差分。base 过期则返回 None，由调用方重新 observe。
    fn diff(&self, base: &ObservationId) -> Option<ObservationDiff>;
}
```

不变量：

- `eid` 在**一次观察内**唯一且按文档序稳定；同一页面连续两次 `observe` 得到相同顺序。
- `elements.len() <= budget.max_elements`；超出时 `truncated = true`，且**优先保留可交互元素**。
- 观察必须包含所有 frame 的元素；`frame_id` 参与 `eid` 的去重判定。
- 密码类输入框的 `value` 一律为 `"***"`。

## 3. 动作运行时

```rust
#[async_trait]
pub trait Driver: Send + Sync {
    async fn resolve(&mut self, target: &Locator) -> Result<ResolveOutcome>;

    async fn act(&mut self, action: &Action, cancel: &CancellationToken) -> Result<ActionResult>;

    async fn check(&mut self, cond: &Condition, timeout: Duration)
        -> Result<ConditionOutcome>;
}

pub enum ResolveOutcome {
    Unique(ResolvedTarget),
    Ambiguous { candidates: Vec<ElementInfo>, strategy: &'static str },
    NotFound { tried: Vec<&'static str> },
}

pub struct ActionResult {
    pub verified: bool,              // 是否有成功条件且已满足
    pub observation: ObservationId,  // 动作后的观察
    pub elapsed_ms: u64,
}
```

不变量：

- `resolve` 返回 `Ambiguous` 时**不得**自行选第一个。选哪个是 `bh-exec` 的策略，不是驱动层的。
- `act` 必须先做可执行性检查（可见、稳定、未被遮挡、可接收指针、未 disabled），
  失败返回 `locator.*` / `action.*` 错误，不做静默降级。
- `act` 遵守 `cancel`：分发前检查一次，分发后若已取消则释放输入并返回 `Canceled`。
- 进程退出或任务取消时，`Driver` 必须释放所有按下的键与鼠标按键（由 `bh-action` 统一持有状态）。

## 4. 执行器

```rust
#[async_trait]
pub trait StepExecutor: Send + Sync {
    fn name(&self) -> ExecutorKind;

    /// 不持有 Task 状态，不做持久化；只把这一步做对或说清楚为什么做不到。
    async fn execute(&self, step: &Step, ctx: &mut ExecCtx<'_>) -> Result<StepOutcome>;
}

pub struct ExecCtx<'a> {
    pub driver: &'a mut dyn Driver,
    pub observer: &'a mut dyn Observer,
    pub models: &'a ModelRouter,
    pub cancel: CancellationToken,
    pub budget: &'a mut TaskBudget,     // 模型调用数、墙钟
    pub artifacts: &'a mut ArtifactSink,
}

pub struct StepOutcome {
    pub status: StepStatus,             // Succeeded | RetryableFail | FatalFail | NeedsModel
    pub verified: bool,
    pub evidence: FailureEvidence,      // 失败时的观察/候选/截图引用
    pub model_calls: u32,
    pub elapsed_ms: u64,
}
```

实现提示：`Driver` 与 `Observer` 通常由同一个会话对象实现（动作运行时需要读观察）。
Rust 里两个 `&mut dyn` 指向同一结构会撞借用检查，所以 `Session` 要把内部状态拆成
`driver_state` / `observe_state` 两个字段，再分别取引用构造 `ExecCtx`。
别为了绕过它把 `ExecCtx` 改成 `Rc<RefCell<…>>`——那样取消与并发语义会更难保证。

关键约束：

- 执行器**看不到 Task**，也**不写库**。它只返回结果；重试、检查点、状态流转全在 `bh-task`。
- 执行器可以调模型（Vision 必需），但必须经过 `ModelRouter`，由它统一扣预算、卡候选上限。
- `NeedsModel` 不是失败，是「这条路径走不通，请上层重新决策」的信号。

### 视觉执行器（Phase 3 实装）

`StepExecutor` 仍是设计稿；当前由 `bh_exec::Runner` 具体类型承担，视觉路径是它的一个方法。

```rust
// crates/bh-exec/src/vision.rs
pub struct Vision { /* ModelRouter */ }

impl Vision {
    pub fn from_env(max_calls: u32) -> Arc<Vision>;
    pub async fn locate(&self, session: &Session, description: &str, goal: &str)
        -> Result<VisualHit>;
}

pub struct VisualHit {
    pub bbox: Rect,        // CSS px，相对视口
    pub point: Point,      // bbox 中心，交给 Input.dispatchMouseEvent
    pub confidence: f64,
    pub model: String,
    pub image_w: u32,
    pub image_h: u32,
    pub viewport: (f64, f64, f64),  // w, h, dpr
}
```

不变量：

- 截图失败、PNG 非法、尺寸与 `viewport × dpr` 偏差 >2%、bbox 越界或小于 1×1 CSS px
  都返回错误；绝不把「不确定的坐标」交给输入层。
- 一个 `Runner` 共享一个 `Vision`（进而一个 `ModelRouter`），预算按任务计。
- `Runner::model_calls()` 是路由器计数；引擎在每步结束后写入 `tasks.model_calls`。

### 就地改道（Task Router）

```rust
// bh-exec：仅当 step.executor == Auto && step.fallback_visual.is_some()
// && 错误是 LocatorNotFound && 动作是 click/hover 时触发。
async fn escalate(&mut self, step: &Step, action: &Action, error: &BhError)
    -> Result<Option<ActionResult>>;
```

歧义、禁用、无布局盒都不改道；改道只重试当前动作，成功后仍走步骤的 `success` 条件。

### 持续交互

```rust
// Phase 3 实现（crates/bh-exec/src/controller.rs）：不引入 trait，只有一个实现。
pub async fn run(
    session: &Session,
    policy: &ControllerPolicy,
    cancel: Option<Arc<AtomicBool>>,
    on_summary: impl FnMut(&TickSummary),
) -> Result<ControllerOutcome>;

// Phase 4：运行中换策略。只采纳严格更大的 policy_version，且在 tick 边界生效。
pub async fn run_with_updates(
    session: &Session,
    policy: ControllerPolicy,
    updates: Option<watch::Receiver<ControllerPolicy>>,
    cancel: Option<Arc<AtomicBool>>,
    on_summary: impl FnMut(&TickSummary),
) -> Result<ControllerOutcome>;

pub struct ControllerOutcome {
    pub why: StopReason,      // GoalMet | Deadline | Canceled | Anomaly | TerminalRule
    pub ticks: u64,
    pub samples: u64,
    pub actions: u64,
    pub elapsed_ms: u64,
    pub policy_version: u64,  // 停止时实际运行的版本
    pub error: Option<String>,
}
```

不变量：退出路径（含 panic、取消、超时）都必须释放输入，并留一个页面上报周期
（350 ms）让释放可见，再交还控制权；`max_actions_per_sec` 是硬上限。
热更新的不变量：版本只增不减；截止时间只能收紧；规则按 id + 内容判重，未变化的规则
不会因一次更新重复执行进入动作。`ControllerRunner` trait 等出现第二个实现
（例如视觉策略）再抽，和 `Driver` 的处理一致。

`bh_task::RunOptions` 因此多了一个可选字段：

```rust
pub struct RunOptions {
    pub headless: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    pub controller_updates: Option<watch::Receiver<ControllerPolicy>>, // 只有 interactive 用
}
```

`bh_task::check_spec(&TaskSpec)` 是引擎开工前的结构校验，单独公开：MCP 的后台
`interactive_start` 要在还有调用方等着回话时拒绝坏 spec，而不是启动一个注定失败的作业。

## 5. 模型层

Phase 3 实装了视觉定位所需的最小面；Phase 4 加了 `Plan`、`Verify` 与 `Repair`。

```rust
// crates/bh-model（已实现）
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn supports(&self, capability: Capability) -> bool;
    async fn propose(&self, req: &ModelRequest) -> Result<ModelProposal>;
}

pub enum Capability { Plan, Locate, VisualLocate, Verify, Repair }

pub struct ModelRequest {
    pub capability: Capability,
    pub instructions: String,            // Plan 时是 goal
    pub state: Option<String>,           // 页面状态或步骤目标
    pub criteria: Vec<(String, String)>, // 有序候选，顺序即优先级
    pub image: Option<ImageData>,        // 仅 VisualLocate
    pub timeout_ms: u64,
}

impl ModelRequest {
    pub fn visual(description: &str, goal: &str) -> Self;
    pub fn plan(goal: &str, state: &str) -> Self;
    pub fn locate(goal: &str, state: &str, candidates: Vec<(String, String)>) -> Self;
    pub fn verify(goal: &str, state: &str) -> Self;   // 固定两个候选
    pub fn repair(goal: &str, state: &str, failed: &Step, error: &str) -> Self;
}

pub struct ImageData {
    pub media_type: &'static str,        // "image/png"
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct ModelProposal {               // 结构性提案，不是 enum
    pub choice: Option<String>,
    pub bbox_norm: Option<[f64; 4]>,     // [x, y, w, h]，各在 0..1
    pub steps: Option<Vec<Step>>,        // 仅 Plan
    pub confidence: f64,
    pub reason: Option<String>,
    pub model: String,
}
```

设计稿的 `capabilities() -> &'static [Capability]` 换成了 `supports(cap)`：适配器通常只
支持一两种能力，逐次询问比返回静态切片少一层分配。`ModelProposal` 从带载荷的 enum
改成结构体 + `Option` 字段：校验只看「这个能力该有的字段是否存在」，不需要先解 enum。

`OpenAiCompat` 是内置适配器，按端点分成两种 `Kind`：`Vision` 支持
`Locate`/`VisualLocate`，`Text` 支持 `Plan`/`Verify`/`Repair`。`from_env` 因此返回 **适配器列表**：
`BH_VLM_URL` 有值时加一个视觉适配器，`BH_LLM_URL`（缺省回落到 `BH_VLM_URL`）有值时
加一个文本适配器。同一个 URL 同时出现两次是允许的——很多 OpenAI 兼容服务两者都接。

`LayaSidecar`（2026-10-28）是第二个适配器，`BH_LAYA_DECIDE` 指向
`sidecars/laya/decide.py` 时启用。它只支持 `Locate` / `Verify`——这两件事本来就是
「从给定选项里选一个」，而 `Plan` / `Repair` 要生成、`VisualLocate` 要视觉，都直接拒绝，
不把选择硬解释成别的东西。生命周期由适配器负责：懒启动、等 `{"ready":true}` 握手、
常驻复用、超时或断管时 kill 并让下一次请求重启；同一配置在进程内共享一个 sidecar，
所以每次 `browser_verify` 新建 router 不会重付模型加载。默认**远程优先、本地兜底**，
`BH_LAYA_PRIORITY=first` 才反过来——依据是实测的零样本准确率（[benchmarks.md](benchmarks.md)）。

**`ModelRouter` 是唯一入口**，当前实现：

1. 按 `Capability` 过滤适配器；**一个都没有时不扣预算**，直接报
   `no model adapter configured for …`（不能为「没配模型」收钱）。
2. **先扣预算再调用**：`fetch_update` 原子自增，超限返回
   `model call budget exhausted (n of N used)`，调用方拿到的是失败而不是延后。
   计的是尝试次数，不是成功次数——超时同样花了钱。
3. 依次尝试候选适配器；**任一适配器返回非法提案就换下一个**，全部失败才报错，
   并把每个适配器的错误拼进消息。
4. 校验提案（`validate`）：
   - 通用：置信度有限且 0..1。
   - 有 `criteria` 时：必须选中其中之一（用于 `Locate`）。
   - `VisualLocate`：必须有 `bbox_norm`，坐标有限、原点非负、尺寸为正，
     `x+w`/`y+h` 不超过 1（容差 1e-6，吸收浮点抖动）。
   - `Plan`（2026-10-28 实现）：必须有 `steps`；1..50 步；每步 `goal` 非空、
     `actions` 非空、`timeout_ms` 在 1..600000；非空 `id` 必须唯一。
   - `Verify`（2026-10-28 实现）：候选固定为 `achieved` / `not_achieved`，
     沿用「必须选中候选之一」的通用校验；置信度阈值由调用方判断，router 不替它做决定。
   - `Repair`（2026-10-28 实现）：必须有 `steps`，且**只能有一个**；沿用 `Plan` 的
     步骤结构校验，另外拒绝 `executor=interactive`（交互任务不属于工作流修复）。
     router 只做结构校验；副作用、`id`、审批、成功条件和主机边界由执行器再查一遍。
   - **不修，只丢**。
5. 提案没写 `model` 时用适配器的 `name()` 回填，避免「匿名提案」进入任务记录。
6. 计数留在 router 内（`calls()`），任务引擎每步结束后写入 `tasks.model_calls`。

`Plan` 的**入口是 CLI**（`bhx plan --goal … --url … [--run]`），不是任务引擎自动触发：
CLI 先观察真实页面，把 `to_state_text()` 作为 `state` 发出去，校验后把步骤写成
`TaskSpec`。凭据以 `${cred:NAME}` 占位符出现在提案里，`--cred NAME=VALUE` 只留在内存；
写盘的 spec（以及引擎的 `task.json`）既没有值也没有 credentials 表。

`Verify` 有两个入口：MCP 工具 `browser_verify` 是即时「第二意见」——调用方给出 goal
与阈值，harness 观察当前页、发一次文本请求，返回 `achieved` / `not_achieved` /
`uncertain`（低于阈值），不自行改任务状态。任务内的入口是
`policy.semantic_verify`：全部步骤的机械条件通过后，任务引擎观察最终页面、调用
`bh_model::verify`，再按 `on_uncertain` 决定成功、失败或请求任务级人审。
`uncertain` 在两种入口里都不会被四舍五入；引擎的 `wait_human` 审批载荷带
`payload.semantic` 与 `payload.next_index`，批准后直接完成，不重跑步骤、不再调模型。
配置了校验但模型不可用/预算不足时任务失败，绝不静默跳过。

`include_screenshot` 打开后，任务引擎在观察之后用 `Vision::screenshot_image()` 截一张
视口 PNG（`Page.captureScreenshot`），把 `ImageData` 传给 `bh_model::verify`；OpenAI
兼容适配器将其作为 `image_url` 数据 URI 附在文字状态之后，Laya 适配器在 `body()` 里
直接报 `laya has no vision`。同一份字节写进任务目录 `artifacts/verify-final.png` 并索引为
`screenshot` 产物，先写 `task.verify.screenshot` 事件、再写 `task.verified`；任务摘要
`result.semantic.screenshot` 记录 name / bytes / width / height / sha256。没有开启时请求
里没有 image，磁盘上也没有这张图。

`Repair` 的入口在执行器：步骤声明 `retry.on_exhausted=ask_model` 且常规尝试耗尽后，
只有 `side_effect=read_only`、无需审批的步骤才会把失败步骤、错误和当前页发给文本
模型，拿回恰好一个替换步骤。执行器在派发前强制安全包络——`id` 不变、副作用不升高、
审批要求不删除、`success` / `preconditions` / `fail_when` 逐字保留、`executor` 与
`fallback_visual` 不变、不新增脚本类动作（`evaluate` / `extract` / 含 JS 的
`wait_for` 只能原样复用作者写下的动作）、导航不越出 `allowed_hosts`、不引入
`${cred:…}`——并且**只执行一次**，不再重试。被拒绝的提案从不派发；成功与失败都写进
`StepReport.repair` / `step.finished`。可逆或不可逆步骤即使声明 `ask_model` 也不修复。

候选预过滤与耗时进 trace 尚未实装：`criteria` 的顺序语义已由调用方保证，
`LayaSidecar` 超过 `BH_LAYA_MAX_CRITERIA`（默认 40）会直接报错而不是截断，
超过 10 个打警告但不改变结果。适配器优先级链已落地（远程优先、`BH_LAYA_PRIORITY=first`
反过来）；同一轮 `propose` 里换适配器只扣一次预算。

适配器**只提案，不执行**。任何适配器都不能直接拿到 `Driver`。

## 6. 任务引擎

```rust
#[async_trait]
pub trait TaskEngine: Send + Sync {
    async fn start(&self, spec: TaskSpec) -> Result<TaskId>;
    async fn status(&self, id: &TaskId) -> Result<TaskStatus>;
    async fn cancel(&self, id: &TaskId) -> Result<()>;
    async fn approve(&self, id: &TaskId, step: Option<&StepId>, d: Approval) -> Result<()>;
    async fn resume(&self, id: &TaskId) -> Result<()>;
    fn events(&self) -> broadcast::Receiver<Event>;
}

pub struct TaskSpec {
    pub goal: String,
    pub kind: TaskKind,
    pub mode: ExecMode,
    pub steps: Option<Vec<Step>>,
    /// Opened before planning when `steps` is empty; not a second navigation
    /// mechanism for scripted tasks.
    pub start_url: Option<String>,
    pub policy: TaskPolicy,
    pub session: SessionRef,
}
```

**规划契约**（2026-10-28 起两条入口都已接通）：

- **引擎自动路径**：`workflow` + `steps=[]` + `mode=auto` 被接受。任务创建后，
  引擎打开 `TaskSpec.start_url`（可选，受 `allowed_hosts` 约束），用任务自己的
  `Observer` 观察，调用一次 `Capability::Plan`，校验并补全 id，检查任务边界
  （planned `navigate` 不越界、`${cred:…}` 必须是调用方提供的名字），把计划写回
  `task.json` 后才执行。计划调用计入该任务的 `max_model_calls`；没有适配器、预算
  不足或提案非法都会让任务失败，而不是运行半个计划。非 `mode=auto` 的空 workflow
  仍在校验时被拒绝（CLI 退出 2，REST 422）。
- **显式路径**：`bhx plan --goal … --url … [--run]`：在独立的规划浏览器里观察、
  规划、校验、落 spec（或直接交给引擎执行）；这次规划调用不计入随后任务
  （新进程、新 router）的预算。

两条路径都没有可用规划模型时都必须报 `planning_unavailable`——**不允许**用规则引擎
假装能规划任意目标（它只会产出看起来像计划的错误步骤）。计划与手写步骤受同一套
审批、副作用和 `allowed_hosts` 约束；规划失败不派发任何步骤。

`TaskStatus` 包含：`state`、当前步、`model_calls`、`elapsed_ms`、`artifacts`、`last_error`。

## 7. 服务层：Page 与 REST 会话

```rust
pub struct Page { browser: Browser, session: Session, observer: Observer }

impl Page {
    pub async fn open(artifacts: PathBuf, headless: bool) -> Result<Self>;
    pub fn parts_mut(&mut self) -> (&Browser, &Session, &mut Observer);
    pub async fn navigate(&self, url: &str) -> Result<(String, String)>;
    pub async fn observe(&mut self, goal: &str, max_elements: Option<usize>, screenshot: bool)
        -> Result<Observed>;          // Observed 带 text/truncated/elements/screenshot
    pub async fn act(&mut self, actions: &[Action], observe_after: bool, cancel: Arc<AtomicBool>)
        -> Result<Acted>;             // Acted 带 log/failed，失败不吞已执行的前缀
    pub async fn check(&mut self, cond: &Condition, timeout_ms: u64, cancel: Arc<AtomicBool>)
        -> Result<bool>;              // 超时是 false
    pub async fn verify(&mut self, goal: &str, threshold: f64) -> Result<VerifyReport>;
    pub async fn evaluate(&self, script: &str, world: &str) -> Result<Value>;
    pub async fn screenshot(&self) -> Result<Vec<u8>>;      // JPEG（MCP）
    pub async fn screenshot_png(&self) -> Result<Vec<u8>>;  // PNG（REST）
}
```

`Page` 是 MCP `browser_*` 与 REST `/v1/sessions/{id}/*` 的**唯一**实现：两条前门若各写一份，
isolated world 默认、动作日志格式、截图编码迟早会分叉。`parts_mut` 给 `Runner` 用
（它同时要 browser、session 和可变 observer）。

REST 会话的不变量：

1. **会话只在进程内。** 交互式的页面没有检查点可恢复；进程退出即关闭浏览器，
   剩下的半截会话没有可续的东西。
2. **每个会话一把锁、一个浏览器、一个下载目录。** 同一会话的请求串行；不同会话互不影响。
   下载目录固定在 `<home>/sessions/<id>/downloads`，除非创建时显式给 `download_dir`。
3. **`allowed_hosts` 在动作之前检查。** 既管 `navigate` 端点，也管 `actions` 批次里的
   `navigate`；拒绝时页面和批次都不动。
4. **批次失败是 422 + 索引日志，不是回滚。** 失败前的动作已经生效；把「停在这里」说成
   「什么都没发生」会让调用方在错误的前提上重试。
5. **媒体类型随字节走。** `observe` 附带 JPEG 并标 `screenshot_mime`；`GET …/screenshot`
   返回 PNG 并带 `Content-Type: image/png`。
6. **能力缺失不是坏请求。** `verify` 没有适配器返回 503；参数问题才返回 422。

## 8. 存储

```rust
#[async_trait]
pub trait Store: Send + Sync {
    async fn put_task(&self, t: &Task) -> Result<()>;
    async fn update_task_state(&self, id: &TaskId, s: TaskState) -> Result<()>;
    async fn put_step(&self, task: &TaskId, step: &StepRecord) -> Result<()>;
    async fn save_checkpoint(&self, cp: &Checkpoint) -> Result<u64>;
    async fn latest_checkpoint(&self, id: &TaskId) -> Result<Option<Checkpoint>>;
    async fn append_event(&self, e: &Event) -> Result<u64>;
    async fn events_after(&self, id: &TaskId, seq: u64) -> Result<Vec<Event>>;
    async fn add_artifact(&self, a: &Artifact) -> Result<()>;
    async fn list_artifacts(&self, id: &TaskId) -> Result<Vec<Artifact>>;
    async fn request_approval(&self, r: ApprovalRequest) -> Result<ApprovalId>;
    async fn decide_approval(&self, id: &ApprovalId, d: Approval) -> Result<()>;
}
```

表结构见 [data-model.md](data-model.md)。

## 9. 不变量汇总

实现时拿这张表逐条核对，这些是最容易在各层之间悄悄漂移的假设：

| # | 不变量 | 违反后果 |
|---|---|---|
| 1 | `eid` 只在一次观察内有效，跨观察必须带 `observation_id` | 点到错的元素 |
| 2 | 动作分发前必须校验观察未过期 | 页面已跳转仍在旧坐标点击 |
| 3 | 多命中不得自动取第一个 | 静默点错，且事后无法解释 |
| 4 | 执行器看不到 Task、不写库 | 状态机被绕过，检查点错乱 |
| 5 | 模型只提案，且必须过校验 | 越权动作、坐标越界 |
| 6 | 模型调用统一走 `ModelRouter` 计数 | 预算失控、金额失控 |
| 7 | 候选 >10 先截断再发送 | 准确率塌陷 |
| 8 | 取消路径必须释放输入 | 粘键、鼠标卡在按下状态 |
| 9 | 不可逆步骤失败后不得盲目重试 | 重复提交 |
| 10 | 所有等待都是条件轮询，不是固定 sleep | 慢，且随机失败 |
| 11 | 未知枚举值报错而非忽略 | 新协议在旧实现上被静默曲解 |
| 12 | 视觉坐标必须来自与当前视口同形的截图（尺寸差 ≤2%） | 点击系统性偏移 |
| 13 | 模型预算按尝试计；没有适配器时不扣预算 | 预算假性耗尽 / 为必然失败付费 |
| 14 | 规划提案必须含 1..50 个可执行步骤，非法即丢弃 | 空计划伪装成成功 |
| 15 | 凭据只以 `${cred:…}` 占位符进入规划结果；写盘 spec 永不含值 | 密码进任务目录 |
| 16 | `semantic_verify` 只有 `achieved` 能让任务成功；配置了却无法校验必须失败 | 未确认的结果被包装成成功 |
| 17 | 自动修复只碰 `read_only` 且无需审批的步骤；替换步骤不得改 id / 执行器 / 成功条件 / 前置或失败条件，不得升高副作用、删除审批、新增脚本动作或越出 `allowed_hosts` | 重复提交、越过审批、把失败改成无法验证的「成功」，或把只读步骤变成任意 JS 通道 |
| 18 | `action.executed` 只记动作类型与结果，不记载荷（输入文本在 materialize 后可能是凭据值） | 密码进入事件流与 trace 包 |
| 19 | 调度表只存凭据名字；值在运行开始时从环境读，缺任何一个整次拒跑 | 密码长期留在 home 里，或占位符被当字面量敲进页面 |
| 20 | 调度槽位在运行前消费（`next_run_at_ms = now + interval`） | 运行中崩溃后自动盲重跑已提交的不可逆动作 |
| 21 | attach 模式只关自己创建的 target，从不结束远端浏览器进程 | 关掉别人的浏览器，或杀掉还有其它标签的会话 |
| 22 | 同一调度绝不同时跑两次；并发上限默认 1，显式 `--concurrency` 才能提高 | 同一任务被两个浏览器同时执行，重复提交不可逆动作 |
| 23 | 语义校验的截图只在 `include_screenshot` 显式开启时截取、发送并落盘；落盘字节与发送字节一致 | 页面内容（可能含个人信息）被悄悄交给模型或留在任务目录 |

## 10. 错误类型

```rust
pub enum BhError {
    Browser(BrowserError),     // 连接、崩溃、导航
    Locator(LocatorError),     // NotFound / Ambiguous / NotActionable / Occluded
    Action(ActionError),       // DispatchFailed / Timeout / Canceled
    Task(TaskError),           // BudgetExceeded / PlanningUnavailable / ApprovalDenied
    Model(ModelError),         // Timeout / InvalidProposal / Unavailable
    Policy(PolicyError),       // HostNotAllowed / IrreversibleBlocked
    Internal(String),
}
```

序列化到线格式时映射为 `protocol.md` 第 11 节的 `code` 字符串（`locator_ambiguous` 等）。
`retryable` 由变体决定，不由调用方猜。

## 11. 运行历史与 trace

```text
bhx stats [--json] [--home DIR]
bhx trace <id> [--json] [--out DIR] [--home DIR]
```

`Store::stats()` 只读聚合，不新增表、不额外埋点：

```rust
pub struct Stats {
    pub tasks: i64,
    pub by_state: BTreeMap<String, i64>,
    pub model_calls: i64,
    pub durations_ms: Vec<u64>,   // 已排序，只有 finished_at 非空的任务
    pub actions: i64,             // type = 'action.executed' 的事件数
    pub failed_actions: i64,      // 其中 json_extract(data_json,'$.ok') = 0
}
```

派生口径写在 `Stats` 上，而不是散在 SQL 或 CLI 里：`finished()` 只加
`succeeded + failed`（取消是调用方的决定）；`success_rate()` 在没有完成运行时返回
`None` 而不是 `0.0`；`duration_ms(p)` 用最近秩百分位；`action_failure_rate()` 在没有
动作事件时返回 `None`（旧 home 不被伪造成 0%）。

`trace` 的数据源只有 store：任务行、步骤行、按 `seq` 排序的事件、产物行。
`--out DIR` 写 `trace.json`、`trace.html` 与 `artifacts/<序号>-<名字>`，不写数据库、
不写裸 SQL；产物复制失败只警告，不中断（trace 仍是完整的，缺的是一个文件）。

`trace.html` 由 `crates/bhx/src/player.rs` 生成：一个自包含页面，把同一个 bundle
序列化进 `<script type="application/json">`（所有 `<` 转成 `\u003c`，站点文本无法闭合
脚本标签）。它只读这份数据——不重跑任务、不访问网络；产物链接使用与复制步骤相同的
`<序号>-<名字>`。播放器不显示动作载荷，因为 bundle 里本来就没有（见不变量 18）。

**步骤截图**：`Policy.trace_screenshots`（默认 `false`）打开后，每个步骤结束时写一张
JPEG（`Session::screenshot`，quality 70），作为 `kind = "screenshot"` 的普通产物入库，
并发一条 `step.screenshot` 事件：

```json
{"step_id": "s2", "name": "step-02-s2.jpg", "path": ".../artifacts/step-02-s2.jpg",
 "bytes": 23326, "sha256": "…"}
```

失败步骤（`report.status == Failed`）无条件截图，与开关无关；截图或写盘失败只记 `warn!`，
不改变步骤状态。截图在 `execute` 里 `await`（只借 `&session`），随后同步入库——
`&Engine` 不能跨 `await`，否则任务 future 不再是 `Send`，`serve` / MCP 编不过。
播放器把步骤卡与时间线里的 `step.screenshot` 渲染成缩略图，详情区给整图；
`task.verify.screenshot`（语义校验发给模型的那张 PNG）在时间线与详情区同样显示，
所以回放时看到的就是模型当时看到的那一张。

**分享边界**：截图记录的是页面当时的样子，可能包含列表里的手机号、地址、登录后的
用户名——脱敏规则覆盖的是输入值与凭据，覆盖不了页面自己展示的内容。默认只截失败步骤
就是为了让这类文件的产生量保持在「出了事才有一张」；把 trace 包发出去之前要把它当
截图分享来对待。语义校验的 `include_screenshot` 是另一条独立开关：它存下的
`verify-final.png` 不受 `trace_screenshots` 影响，而且这份字节已经发给了校验模型。

## 12. 定时执行

```text
bhx schedule add <script.json> --every 30m [--home DIR]
bhx schedule ls [--json] [--home DIR]
bhx schedule rm|enable|disable <id> [--home DIR]
bhx schedule run [--once] [--concurrency N] [--home DIR]
```

`Store` 上新增的接口（`crates/bh-task/src/store.rs`）：

```rust
pub struct ScheduleRow {
    pub id, source, spec_json: String,
    pub cred_names: Vec<String>,   // 只存名字
    pub interval_ms: i64, pub enabled: bool, pub created_at: String,
    pub last_run_at: Option<String>, pub last_task_id: Option<String>,
    pub last_state: Option<String>, pub last_error: Option<String>,
    pub next_run_at_ms: i64,
}

Store::add_schedule(&NewSchedule) -> Result<()>
Store::schedules() -> Result<Vec<ScheduleRow>>
Store::due_schedules(at_ms) -> Result<Vec<ScheduleRow>>       // enabled 且 next_run_at_ms <= at_ms
Store::claim_schedule(id, at_ms) -> Result<bool>              // 条件更新：槽位只被消费一次
Store::finish_schedule_run(id, task_id, state, error) -> Result<()>
Store::set_schedule_enabled(id, enabled, at_ms) -> Result<bool>
Store::remove_schedule(id) -> Result<bool>
```

间隔与凭据解析在 `bh-task::schedule`：`parse_interval` 接受 `s/m/h/d` 与组合
（`1h30m`），下限 `MIN_INTERVAL_MS = 1000`；`credential_env_key` 把名字映射成
`BH_CRED_<NAME>`（大写、非字母数字转 `_`）；`resolve_credentials` 是全有或全无，
缺失时返回要设置的变量名、不回显值。

`schedule run` 的顺序是 **claim → 解析凭据 → `Engine::run` → finish**。解析凭据在
`Engine::run` 之前，所以缺值的那次运行不会创建任务、不会启动浏览器、不会碰站点。
`--once` 只做一趟，有失败的运行则退出非零；常驻模式每 250 ms 查一次到期项。

并发由 `--concurrency N`（默认 1，范围 `1..=8`）控制，指的是**不同调度**同时运行的
上限。每个运行在自己的 tokio 任务里打开自己的 `Engine`（同一个 home，靠 WAL 与
`busy_timeout` 共享 store），所以没有 `&mut Engine` 被两个任务同时借用的问题。
槽位在即将开始跑时才 claim，排队等上限的调度不会显示成 `running`。同一调度的下一次
槽位在它仍运行时到期，会被消费并记为 `skipped_overlap`（附 `last_error` 说明），
而不是排队补跑——一次长运行不应变成一串追赶。`--once` 只在开始时取一次到期快照，
按并发上限分批跑完这批准就退出；运行期间新到期的留给下一趟。

尚未提供 REST/MCP 入口：调度是本地运维动作，Agent 通过 `task_run` 直接跑任务即可。

## 13. 远端浏览器（attach）

```rust
pub struct BrowserOptions {
    ...
    /// 连一个已在运行的 Chrome，而不是自己起一个。
    /// 接受 `http://host:port`、`ws(s)://…/devtools/browser/…`、裸 `host:port`。
    pub cdp_url: Option<String>,
}
```

`Browser::launch` 的第一件事：`opts.cdp_url` 或环境变量 `BH_CDP_URL` 有值就转 `Browser::attach`，
不会再 spawn 本地 Chrome。`resolve_cdp_endpoint` 的解析规则：

| 输入 | 行为 |
|---|---|
| `ws://…` / `wss://…` | 原样连接 |
| `http(s)://…` 或裸 `host:port` | 取 `{base}/json/version` 的 `webSocketDebuggerUrl`（ureq，`spawn_blocking`，5 s 上限） |
| 解析失败 | `no CDP endpoint at "…" (tried …/json/version); start Chrome with --remote-debugging-port or pass a ws:// URL` |

attach 出来的 `Browser` 与本地启动的只有一个差别：`child: None`、`port: 0`。因此：

- `new_session` 仍然 `Target.createTarget` + `Target.attachToTarget(flatten)`，其余流程完全一致；
- 每建的 target 记进 `Browser::targets`；`shutdown` 只对它们发 `Target.closeTarget`（最多等 2 s），
  **从不杀进程**——远端浏览器可能是别人的、可能还有别的标签；
- 端点不可达时任务在 `Engine::execute` 的启动阶段就以 `failed` 收尾，并写
  `task.finished { phase: "launch" }`；不存在「启动失败但任务停在 pending」的状态。

`bhx serve`、`bhx observe`、`bhx task run`、`bhx schedule run` 共用同一套启动路径，
所以设了 `BH_CDP_URL` 之后它们全都改用远端浏览器。
