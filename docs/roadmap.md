# 路线图 v0.2

原则：**先拿到能完成真实工作的能力，再谈实时与多模型。**
每个阶段都必须留下可运行的证据，而不是只留代码。

## 阶段总览

| 阶段 | 目标 | 退出条件 |
|---|---|---|
| 0 | 打通 CDP，量出延迟基线 | 能连上真实 Chromium，完成导航/观察/点击，拿到 p50/p95 数据 |
| 1 | 通用 Browser Runtime ✅ | 会话、观察、动作、取消全部可用；最小 MCP 能驱动 |
| 2 | 任务引擎 ✅ | 多步骤任务 + 检查点恢复 + 产物管理通过 A 组 3/5 |
| 3 | 混合执行（进行中） | DOM/视觉/持续交互统一调度，A 组全通，B/C 组可用 |
| 4 | 接口与模型集成 | Claude Code、Pi、Qwen 经 MCP 跑通同一任务；模型适配器齐全 |
| 5 | 实时优化 | 连续交互延迟与稳定性达标，C 组全通 |
| 6 | 持久化与可观测 | 长任务中断恢复、运行历史、性能报告 |

## Phase 0 — 可行性验证 ✅ 2026-10-28

一次性代码，`spike/`，约 350 行，自建 CDP 客户端 + 元素索引原型 + 实测。

**结果**（详见 [benchmarks.md](benchmarks.md)）：

| 问题 | 答案 |
|---|---|
| 自建薄 CDP 客户端够不够用 | 够。往返 0.1–0.5 ms，不需要 chromiumoxide 或 Playwright |
| 单动作端到端延迟 | p50 16.7 ms，p95 17.2 ms |
| 瓶颈在哪 | `mouseMoved`（16.6 ms/次）；不发它，点击只要 0.4 ms |
| 键盘延迟 | 0.3 ms，30 Hz 循环绰绰有余 |
| 截图成本 | 50 ms/张，视觉路径不可能进高频回路 |
| 大页面观察成本 | 1000 行表格 → 13422 节点、4.9 MB、103 ms，必须做预算与差分 |
| 模型能多频繁介入 | Laya 58 ms 起，远程模型秒级；实时回路里只能低频介入 |

**退出条件**：单动作 p95 < 50 ms ✅（17.2 ms）；实时回路的介入频率问题 ✅（见上）。

**带回设计的两条修正**：

1. `click` 默认不发 `mouseMoved`，`hover` 拆成独立动作（见 architecture.md 动作运行时）。
2. 观察预算 `max_elements = 120` 从「保守默认」升级为「必需」，并要求差分。

## 实现从这里开始

- 类型与 trait：以 [interfaces.md](interfaces.md) 为准，`bh-core` 先落类型，`cargo check` 通过再往下写。
- 表结构与配置：以 [data-model.md](data-model.md) 为准，第一条 migration 直接照抄。
- 验收怎么跑：以 [testing.md](testing.md) 为准，Phase 2 之前测试场就要能起。
- 想不通某个交互时，回头读 [walkthrough.md](walkthrough.md)——它是这些接口的用例。

## Phase 1 — Browser Runtime + 最小 MCP ✅ 2026-10-28

- `bh-cdp`：连接、target 发现、事件订阅、断线重连。
- `bh-browser`：context/page 生命周期、导航与等待、输入分发、下载/上传、对话框。
- `bh-observe`：AX 树 → 元素索引（稳定排序、上限、哈希）；`PageState` / `Snapshot` 两级。
- `bh-action`：Locator 解析、可执行性检查、执行、后置条件、`CancellationToken`、中止清理。
- 最小 MCP：`browser.create_session / navigate / observe / execute_actions / screenshot`。

**为什么第一版就要 MCP**：没有对外接口，后续每个阶段只能靠单测自证，
而「能不能被 Agent 用起来」恰恰是最容易翻车的地方。

**退出条件**：一个外部 Agent（或 CLI 脚本）能通过 MCP 完成「打开页面 → 找到元素 → 点击 → 确认结果」，
取消在 200ms 内生效且无残留输入。

### 落地结果

| 交付物 | 状态 |
|---|---|
| `bh-cdp`：连接、attach、事件广播 | ✅（断线重连留到 Phase 6） |
| `bh-browser`：生命周期、导航、输入、下载、弹窗、截图 | ✅ |
| `bh-observe`：AX → 元素索引、预算、可见性、密码脱敏 | ✅（两级观察留到 Phase 2） |
| `bh-action`：定位、可执行性、分发、条件、等待、取消 | ✅ |
| `bh-exec`：前置条件、动作序列、成功条件、重试、审批闸门 | ✅ |
| 最小 MCP（stdio） | ✅ 8 个工具，串行执行，支持 `notifications/cancelled` |

**实测**（`testkit/run-phase1.sh`，6/6）：

| 检查 | 结果 |
|---|---|
| 登录 → 筛选 → 导出 CSV，连续 3 次 | 3/3 成功，202–271 ms |
| 产物 | `orders-失败.csv`，表头 + 2 行，内容与筛选一致 |
| 导出瞬间注入 `alert()` | 自动 dismiss，页面继续可用，任务 733 ms |
| MCP：navigate → observe → 3 个动作 → 条件 → 截图 | 通过 |
| 取消 | 85 ms 生效，取消后会话仍可用 |

**Phase 1 期间做出的、写回设计的判断**：

1. **`DOM.getBoxModel` 的 "Could not compute box model" 是答案，不是错误。**
   隐藏元素没有布局盒；把它当异常会让整次观察失败。现在按「不可见，跳过」处理。
2. **`placeholder` 不是 AX 名字。** 有 `<label>` 时 Chrome 用 label 作 accessible name，
   placeholder 只存在于 DOM。定位器因此是分层解析：AX 优先，DOM 兜底（`css` / `test_id` / `placeholder`）。
3. **下载事件是边沿触发，不是状态。** 必须在触发动作**之前**订阅，之后再 `wait`；
   否则 100 ms 内完成的下载会被漏掉。
4. **JS 弹窗必须有人回答，否则渲染进程一直阻塞。** Phase 1 策略：除 `beforeunload` 一律 dismiss
   并记录；把弹窗升级给模型决策是 Phase 3 的事，但「先挂住」不是可接受的现状。
5. **取消要落在轮询循环里。** 等待间隔上限压到 100 ms，取消检查在每次分发前与每次轮询，
   分发后若已取消则释放全部按键。
6. **密码值不能进错误信息。** 输入回读校验只报「不匹配」，不回显期望值或实际值。

**未做、且刻意留到后面**（不要误以为已完成）：

| 项 | 阶段 | 原因 |
|---|---|---|
| `Driver` / `Observer` / `StepExecutor` trait 抽象 | Phase 3 | 只有一个实现时，先写具体类型 |
| `PageState` / `Snapshot` 两级观察、观察差分 | Phase 3 | 任务预算已落地，但两级观察需要视觉路径才有收益 |
| `within` 容器窄化、XPath | Phase 3 | 现有多候选直接报 ambiguous，不猜 |
| `drag` / `select_option` / `set_files` / 上传 / 前进后退 | Phase 3 | 测试站点尚未覆盖 |
| 连接断开后重连 | Phase 6 | 崩溃恢复与检查点已在 Phase 2 完成，重连仍待做 |
| ~~审批 UI / `waiting_approval` 流转~~ | ~~Phase 2~~ | ✅ 已在 Phase 2 落地（CLI + MCP 的 approve/reject，无 GUI） |

## Phase 2 — Task Engine ✅ 2026-10-28

- `bh-core` 类型定稿（Task/Step/Action/Locator/Condition），协议版本化。
- `bh-task`：状态机、步骤执行、重试、升级点、审批、检查点、SQLite 持久化。
- 产物：下载文件、提取数据、截图、trace；`artifact.*` 接口。
- `bh-exec`：DOM Executor（先只做这一种）。
- `bh-model`：只接一个文本模型（规划 + 修复），加一个规则兜底。
- 测试场：`testkit/sites/admin`、`form`、`chaos`。

**退出条件**：A 组通过 ≥ 3/5；每任务模型调用 ≤ 2；进程被杀后能从最后一个检查点恢复并完成。

### 落地结果

| 交付物 | 状态 |
|---|---|
| `bh-core`：Task/Step/Action/Locator/Condition/Policy | ✅ 含新增 `Action::Extract`、`Policy.accept_confirm`、`Step.fail_when` |
| `bh-task`：状态机、检查点、审批、产物索引、崩溃恢复 | ✅ SQLite（WAL）+ `events.jsonl` 镜像 |
| 产物：下载 / 提取 JSON / 截图，sha256 记录 | ✅ `artifacts/<step>-<name>.json`、`artifacts/<原文件名>` |
| 审批流转 `waiting_approval` | ✅ CLI `approve/reject` + MCP `task_approve`，退出码 3 |
| 任务 CLI | ✅ `task run/ls/show/resume/approve/reject/cancel/artifacts/recover` |
| MCP 任务层 | ✅ `task_run/status/list/resume/approve/cancel`，与 `browser_*` 并存 |
| 测试场 | ✅ `server.py`（服务端真相 + chaos），`admin` / `form` 站点，5 个任务脚本 |
| `bh-model` | ⏸ 未做：本阶段任务全部脚本化，模型规划留在 Phase 4 |
| `testkit/sites/chaos` | ⏸ 未做：chaos 改为 `server.py` 的注入端点，比静态站点更可控 |

**实测**（`testkit/run-phase2.sh`，14/14；`testkit/run-phase1.sh` 回归 6/6）：

| 检查 | 结果 |
|---|---|
| A1 提取结构化数据 | 881 ms，产物 JSON 与 `/admin/__state` 一致 |
| A2 多步表单 | 750 ms，服务端 422 校验字段全部正确 |
| A3 条件查询 + 导出 CSV | 815 ms，字节与服务器产物逐字节一致（sha256） |
| A4 跨站点 6 步 | 905 ms，提取 + 表单两侧服务端状态都对 |
| A5 弹窗 + 接口 500 | 1283 ms（加 `fail_when` 前是 9388 ms），确认弹窗接受、首次 500 后立即重试成功 |
| 每任务模型调用 | 0（退出条件 ≤ 2） |
| R1 崩溃恢复 | `kill -9` 落在 s4 → resume 返回 3 → approve → resume 只跑 s4（3719 ms），崩溃前产物仍在 |
| MCP 任务工具 | `testkit/mcp-smoke.py` 通过，取消 82 ms |

**Phase 2 期间做出的、写回设计的判断**：

1. **成功条件是「会变为真」的谓词，不是一次性采样。** 四个用例同时失败在同一个原因上：
   fetch 渲染的页面在动作后 50 ms 还是旧 DOM。改为 25→100 ms 退避轮询到 step 截止
   （`bh-exec::verify`），四个用例一次全过。这是本阶段最重要的一条。
2. **确认弹窗默认不能被点掉。** Phase 1 的策略是「一律 dismiss」，这对 `confirm("删除吗？")`
   等于替用户说了「不」，会让导出静默失败。改为 `Policy.accept_confirm`
   （默认 false，`confirm`/`prompt` 收到才接受；`alert`/`beforeunload` 永远接受）。
3. **提取也应是产物，不只是观察。** `Action::Extract { name, script }` 把脚本返回的 JSON
   写进任务产物并记 sha256；否则「读到了数据」在进程退出后就无法证明。
4. **中断的步骤绝不能自动重跑。** 崩溃点可能在提交之后、回执之前；盲目重试会重复下单。
   现在 resume 把 `running` 步骤转成审批请求（`{"next_index": idx}`），人批准后才继续。
5. **同一任务只能有一个浏览器。** 崩溃会留下持久的 Chrome 进程占着 profile，导致下次
   `Target.createTarget` 连到旧实例。现在用 pid 文件做任务锁，并在启动前按 profile 路径
   `pgrep -f` + `kill -9` 回收残骸。
6. **凭据在写盘前剥离，而不是读时脱敏。** `spec_for_disk` 把 `credentials` 置空并 `scrub`
   掉正文里出现的字面量密钥；`task.json` 是事后最容易被翻出来的文件。
7. **失败信号要比超时先到。** 页面在 100 ms 内就显示了 `#list-error`，执行器却要等满
   `step.timeout_ms`（8 s）才知道失败。新增 `Step.fail_when`：等待成功条件时轮询失败条件，
   命中即结束本次尝试。动作前已为真的失败条件视为上次残留、需先清空——否则重试会
   在同一个错误横幅上立刻放弃。A5 因此从 9.3 s 降到 1.35 s。

**未做、且刻意留到后面**：

| 项 | 阶段 | 原因 |
|---|---|---|
| ~~失败尝试的早期失败信号~~ | ~~Phase 3~~ | ✅ 已在 Phase 2 用 `Step.fail_when` 解决 |
| `bh-model` 规划 / 修复 | Phase 4 | 本阶段先证明「确定的工作能跑稳」，再让模型写步骤 |
| 视觉升级、Task Router | Phase 3 | 需要 `bh-exec::vision` 先存在 |
| trace 产物 | Phase 6 | 事件流已在，但还没有可回放的 trace 格式 |
| 长任务定时执行 | Phase 6 | 状态机与恢复已具备，缺调度 |

## Phase 3 — 混合执行 ✅ 2026-10-28

- `bh-exec::vision`：截图 → 模型 → bbox → 坐标换算（DPR/scroll/iframe/zoom）→ 动作 → 状态验证。
- `bh-exec::interactive`：策略 DSL + 30Hz 循环 + 安全边界 + 版本化动作。
- Task Router：每步选择与就地改道；DOM 失败升级到视觉。
- 测试场：`canvas`、`game`。

**退出条件**：A 组 5/5；B 组 ≥ 4/5；C 组 ≥ 3/5；取消/超时后输入全部释放。

### 落地结果

| 交付物 | 状态 |
|---|---|
| `ControllerPolicy` / `ControlRule` / `SafetyPolicy` 类型 | ✅ `bh-core`，`do` 字段用 serde rename |
| `bh-exec::controller`：固定频率循环、规则匹配、动作派发 | ✅ 30 Hz，采样与求值分频 |
| 安全边界 | ✅ 滑动 1 秒动作限速、deadline、异常阈值、退出无条件释放输入 |
| 引擎接入 | ✅ `kind=interactive` 复用同一浏览器与 SQLite；`task_run` 走同一入口 |
| `testkit/sites/game`：canvas 小游戏 + 服务端遥测 | ✅ 页面用 `sendBeacon` 上报，避免关闭前丢最后一次按键状态 |
| `bh-model`：能力契约、预算、提案校验、OpenAI 兼容适配器 | ✅ 不依赖 Laya；`BH_VLM_URL` 指向任意兼容端点 |
| `bh-exec::vision`：PNG 截图 → 归一化 bbox → CSS 坐标 | ✅ 截图尺寸与 `viewport × dpr` 不符时拒绝换算 |
| `Target::Visual` / `Action::Drag` | ✅ 视觉目标与真实按下-移动-释放手势 |
| Task Router（就地改道） | ✅ `Step.fallback_visual` + `executor=auto`：DOM `LocatorNotFound` 后用视觉重试同一动作 |
| `testkit/sites/canvas`：无 DOM 语义的画布控件 | ✅ 按钮、拖拽、延迟确认、布局切换、隐藏遗留按钮 |
| C 组用例 C1–C4 | ✅ `testkit/run-phase3.sh`，0 次模型调用 |
| B 组用例 B1–B6 | ✅ 同一脚本，1–2 次模型调用，服务端核对落点 |

**实测**（`testkit/run-phase3.sh`，41 项检查全过；下表为一次代表性运行）：

| 用例 | 结果 |
|---|---|
| C1 连续按键（goal_met 后释放） | 控制器 367 ms，1 次动作，退出后按键为空 |
| C2 画布坐标点击 | 102 ms，落点在画布内（80, 88） |
| C3 观察状态后切换动作（先右后下） | 632 ms，3 次动作，终点 (131, 109) |
| C4 超时安全停止 | 1521 ms 触发 deadline，x 从 40 推到 344，按键为空 |
| B1 从截图定位（页面有语义按钮） | 535 ms，1 次模型调用，服务端记录 1 次点击 |
| B2 无语义 canvas 按钮 | 624 ms，落点 (170.5, 94.5)，期望 (170, 94) |
| B3 拖拽 | 889 ms，2 次模型调用，起点误差 0.57 px、终点误差 0.57 px（要求 ≤ 4） |
| B4 状态判定成功 | 1031 ms，点击外观不变，服务端 300 ms 后确认，步骤 verified |
| B5 布局变化后重定位 | 643 ms，落点 (459.6, 277.5)，期望 (460, 278) |
| B6 DOM→视觉改道 | 582 ms，语义定位失败后 1 次模型调用完成点击 |
| 每任务模型调用 | C 组 0；B 组 1–2（预算 2） |

**B 组证据的边界**：视觉模型端是 `testkit/vlm.py`——一个按颜色找区域的确定性替身。
它验证截图传输、schema 校验、DPR/视口一致性、bbox→CSS 换算、输入分发和状态验证；
**不衡量真实 VLM 的定位质量**。接真实模型只需换 `BH_VLM_URL`，harness 侧代码不变。

**本阶段写回设计的判断**：

1. **动作必须在规则「进入」时执行，不是每个 tick。** 否则按住一个键会变成每秒 30 次
   `key_down`。规则语义从「持续为真的条件」改成「状态的边沿」，这让策略更像状态机。
2. **释放输入不能由开关控制。** `release_all_on_exit` 留在格式里，但实现里退出路径
   无条件释放——一个能被配置关掉的安全措施不是安全措施。
3. **遥测要用 `sendBeacon`，而且释放后要留一个上报周期。** 控制器退出时先用
   `key_up` 释放，等 350 ms（页面定时器的一个周期）再交还，然后浏览器才关闭；
   普通 `fetch` 会随页面一起被取消，浏览器立刻关闭则 beacon 也来不及发出，
   测试就只能看到「最后按着键」的假象（C4 实测偶发）。
4. **视觉坐标先证明截图就是视口。** 归一化 bbox 能消掉 DPR/缩放，但前提是截图确实
   等于 CSS 视口 × DPR。尺寸差超过 2% 就直接报错——点错位置比不点更糟。
5. **视口截图不需要 scroll 修正，全页截图才需要。** 当前路径固定截视口，
   代码里不引入一个永远为 0 的偏移量。
6. **DOM→视觉改道必须由步骤显式授权。** 定位不到可能是页面真变了，此时「找个长得像的
   点下去」正是点错按钮的经典路径。只有 `fallback_visual` 给出目标描述、且
   `executor=auto` 时才改道，成功后仍由 `success` 条件验证。
7. **模型调用按尝试计数，按任务共享预算。** 失败的超时也是成本；路由器在调用前扣预算，
   `tasks.model_calls` 是绝对值，重跑步骤不会重复计数。

**尚未做**（Phase 3 之外、已在 Phase 4/5 排期）：`Executor`/`Observer` trait 抽象、
视觉升级的置信度门槛与多候选比较、C5（浏览器异常时清理输入并标记失败）、
真实 VLM 的准确率基准。

## Phase 4 — 模型与接口集成 ⏳ 进行中（2026-10-28 起）

- Model Router 完整：`plan / locate / visual_locate / verify / repair` 五类能力。
- 适配器：文本 LLM、视觉模型、Laya（本地离散决策）、规则引擎。
- MCP 全量工具面；REST + WS 事件流（断线续传）。
- 让 Claude Code、Pi、Qwen 分别用 MCP 跑同一个 A 组任务。

**退出条件**：三个 Agent 用同一套工具完成同一任务，结果与耗时记录在案。

### 已落地：REST + WebSocket

| 交付物 | 状态 |
|---|---|
| `bhx serve`：REST 任务接口 | ✅ 创建/列表/状态/审批/拒绝/取消/恢复，非 auto 空步骤 422、auto 空步骤 202，重复 id 409 |
| 事件流 | ✅ `GET /v1/tasks/{id}/events?after=N` 与 `.../events/ws?after=N` |
| 断线续传 | ✅ 流是 SQLite 的尾随读，重连与首次连接同一条路径，不依赖进程内广播 |
| 产物 | ✅ 列表 + 按名字下载；路径必须 canonicalize 到任务 artifacts 目录内；中文名 URL 编码 |
| MCP 工具面 | ✅ 新增 `browser_evaluate`（默认 isolated world）、`task_artifacts`、`artifact_list` / `artifact_metadata` / `artifact_fetch`（UTF-8 原文或 base64，5 MiB 上限，路径限定任务目录）；`mcp-smoke.py` 断言隔离语义与产物读取 |
| 验收 | ✅ `run-phase4.sh` → `rest-smoke.py` 22/22；MCP 冒烟通过 |

**REST/WS 切片写回设计的判断**：

1. **202 只表示受理，不表示成功。** 任务创建、浏览器启动、执行都在后台；客户端必须
   用状态接口确认结果。否则「HTTP 200」会变成第二个成功定义。
2. **事件流不要 `hello` 首帧。** `after` 已经表达了「从哪继续」；再加一帧就多一个
   要对齐的状态，而客户端真正的需求只是不丢事件。
3. **取消要同时写状态与置标志。** 写库让恢复逻辑一致，标志让交互控制器在两个 tick
   之间就能停；只做其中一件，要么恢复路径看不见，要么取消延迟到一个采样周期。
4. **下载路径按存储行解析，不拼 URL。** 名字来自站点，可能含中文和 `..`；先查库，
   再 canonicalize 校验前缀，不把请求参数当路径。
5. **调用方 JS 默认进 isolated world。** MCP 的 `browser_evaluate` 能读 DOM、读不到
   页面 `window.*`，要 main world 必须显式声明。任务规格里的 `Condition::Js` 和控制器
   脚本仍走 main world：它们是任务定义的一部分，和调用方临时拼的代码不是一个信任级别。

### 已落地：规划（Plan）

| 交付物 | 状态 |
|---|---|
| `Capability::Plan` | ✅ 提案字段 `steps`；校验 1..50 步、非空 goal/actions、timeout 范围、id 唯一 |
| 文本适配器 | ✅ 同一 OpenAI 兼容线格式；`BH_LLM_URL`（回落 `BH_VLM_URL`），与视觉适配器分开注册 |
| `bhx plan` | ✅ 观察真实页面 → 规划 → 校验 → 写 spec 或 `--run`；`--cred` 只驻留内存 |
| 验收 | ✅ `plan-smoke.py` 17/17：1 次规划调用、3 步执行、CSV 同哈希、密码不在 spec/task.json |

**规划切片写回设计的判断**：

1. **规划先作为 CLI 入口落地，再进引擎。** `bhx plan` 先证明了观察、提案校验、
   凭据占位符和落 spec 这条链路；现在引擎对 `mode=auto` 的空 workflow 复用同一套
   `Capability::Plan` 与 Observer。区别在预算语义：引擎内规划是任务的一部分，计入
   `max_model_calls`；`bhx plan --run` 的规划调用发生在另一个进程，不计入随后任务。
2. **规划和执行用同一个 Observer。** 否则模型看到的控件名与执行时定位器对不上，
   错误会表现为「模型乱写」，实际是输入漂移。
3. **凭据在提案里只能是占位符。** 模型看不到密码；值由 `--cred` 在运行时注入，
   写盘前再剥一层（`persisted` 副本），两道门都过才算安全。
4. **规划调用不计入任务的 `max_model_calls`。** 预算是执行期约束；把一次性规划
   也算进去，会让「计划已经写好」的任务因为历史调用而跑不动。

### 已落地：校验（Verify）

| 交付物 | 状态 |
|---|---|
| `Capability::Verify` | ✅ 固定候选 `achieved` / `not_achieved`；沿用候选校验 |
| MCP `browser_verify {goal, threshold?}` | ✅ 观察当前页 → 一次文本请求 → `achieved` / `not_achieved` / `uncertain`（低于阈值不四舍五入） |
| 验收 | ✅ `mcp-smoke.py`：登录前 `not_achieved`、登录后 `achieved`、阈值 0.95 时 `uncertain` |

**校验切片写回设计的判断**：

1. **判定是三态，不是布尔。** 低置信度必须报告 `uncertain`，否则「模型没把握」会被读成
   「页面没达成」，这比不校验更危险。阈值由调用方给，因为不同任务的误判代价不同。
2. **校验不碰任务状态。** MCP 工具只回答「现在看起来对不对」；把 `uncertain` 接到
   等待人审/重试是任务引擎的策略，和副作用等级一起设计，而不是藏在工具里。

### 已落地：任务内语义校验（2026-10-28）

| 交付物 | 状态 |
|---|---|
| `policy.semantic_verify` | ✅ `goal?` / `threshold`（默认 0.6）/ `on_uncertain`（`wait_human` 默认、`fail`、`continue`）/ `include_screenshot`（默认 false） |
| 执行时机 | ✅ 全部步骤机械成功后观察最终页 → 一次文本请求；复用任务的 `ModelRouter`，与视觉共享预算并计入 `model_calls` |
| 三态接线 | ✅ `achieved` 成功；`not_achieved` 失败但保留步骤 `verified`；`uncertain` 按策略等待任务级人审/失败/明确接受 |
| 人审恢复 | ✅ 审批载荷带 `payload.semantic` 与 `next_index`；批准后直接完成，不重跑步骤、不再调模型 |
| 失败诚实 | ✅ 配置了校验但无适配器/预算不足/观察失败 => 任务失败并写 `task.verify_error`；非法阈值在起浏览器前拒绝 |
| 截图校验 | ✅ `include_screenshot` 截视口 PNG 随请求发给视觉模型；同一份字节存为 `artifacts/verify-final.png`，事件与摘要带 sha256；纯文本模型明确拒绝且不启动 sidecar；trace 回放器直接显示这张图 |
| 验收 | ✅ `semantic-smoke.py` 62/62：achieved / not_achieved / uncertain 审批恢复 / 策略失败 / 无适配器 / 坏阈值 / 截图同哈希 / 默认不发 / 回放器显示；`laya-smoke.py` 80/80 |

**语义校验切片写回设计的判断**：

1. **默认 `wait_human`，不默认重试。** 结果不对时，动作的副作用可能已经发生；
   自动重跑是重复提交的经典路径。人审可以批准结果，也可以先去看页面再决定拒绝。
2. **`not_achieved` 不抹掉 `verified`。** 步骤的 `verified` 回答「声明的条件是否出现」，
   任务状态回答「目标是否达成」。两个问题分开记录，失败报告才有诊断价值。
3. **配置了校验就不能静默跳过。** 模型缺失或预算耗尽都让任务失败，否则一次配置丢失
   会把语义校验悄悄退回成「只跑机械条件」。
4. **先做任务级，不做步骤级重规划。** 步骤级语义门需要和 `Repair` 一起设计重试/升级
   语义；在副作用安全边界没定清楚前，把它接到任务结果上更可控。
5. **截图必须显式要，且可审计。** 默认只发文字状态；开启后存下的正是发给模型的那份
   字节，sha256 能证明模型看到的就是磁盘上的图。文本模型不能假装看过图——拒绝比静默
   降级成文字判定诚实。

### 已落地：自动修复（Repair，2026-10-28）

| 交付物 | 状态 |
|---|---|
| `Capability::Repair` | ✅ 文本适配器支持；提案必须恰好一个步骤，结构校验同 `Plan`，拒绝 `executor=interactive` |
| 执行器接线 | ✅ `retry.on_exhausted=ask_model` 在常规尝试耗尽后触发；只对 `read_only` 且无需审批的步骤生效 |
| 安全包络 | ✅ 同 id、副作用不升高、审批不删除、成功/前置/失败条件逐字保留、执行器与视觉改道不变、不新增脚本类动作、导航受 `allowed_hosts` 约束、不引入凭据占位符 |
| 只派发一次 | ✅ 不再进入重试循环；成功/失败都写 `repair` 与 `step.finished`；拒绝的提案从不执行 |
| 观测 | ✅ `result.steps[].repair`、`step.finished.data.repair`、CLI `[repaired: …]`；与视觉/语义共享模型预算 |
| 验收 | ✅ `repair-smoke.py` 112/112：正常修复 + 修复后仍失败 + 非 read_only 不修 + 九类违规提案被拒 |

**自动修复切片写回设计的判断**：

1. **只修只读步骤。** 失败时我们无法证明动作没有生效；对可逆/不可逆步骤重新派发
   模型改写的版本，就是重复提交。`read_only` 让「再试一次」的代价保持在可观测范围内。
2. **修复是替换，不是重规划。** 只换当前失败的一个步骤，不重写后续步骤；任务其余部分
   仍按原计划执行，失败上下文和成功条件都保留。
3. **安全包络在执行器，不在模型。** 模型只提案；`id`、副作用、审批、成功/前置/失败
   条件、执行器、视觉改道、脚本动作和主机边界都由 `validate_repair` 检查。模型返回
   违规提案时，任务失败而不是执行一个「看起来更合理」的版本。尤其是只读步骤不能因为
   修复而获得新的主世界 JS 执行通道。
4. **失败也要留痕。** 修复后仍失败、以及提案被拒绝，都写清原因；否则「修过但没用」
   会退化成一条普通失败，无法区分模型问题和页面问题。

### 已落地：空步骤自动规划（2026-10-28）

| 交付物 | 状态 |
|---|---|
| 引擎入口 | ✅ `workflow` + `steps=[]` + `mode=auto` 被接受；非 auto 仍拒绝 |
| 规划输入 | ✅ `TaskSpec.start_url`（可选，受 `allowed_hosts` 约束）→ 任务自己的 Observer → 页面状态文本 |
| 任务边界 | ✅ 提案先过模型侧 schema（1..50 步、非空 goal/actions、timeout、id 唯一），再过引擎侧检查（planned `navigate` 不越界、`${cred:…}` 必须是调用方提供的名字） |
| 落盘与恢复 | ✅ 计划先写回 `task.json`（占位符保留、凭据剥离）再执行；resume 继续原计划，不再调模型 |
| 预算 | ✅ 规划调用计入任务的 `max_model_calls`；无适配器/预算不足/坏提案都让任务失败，零步骤派发 |
| 凭据 | ✅ 规划时只看到 `${cred:…}`；`task.json`、steps 表、事件里都没有密码值（顺带修掉 steps 表曾存解析后动作文本的问题） |
| 观测 | ✅ `plan.requested` / `plan.created` / `plan.failed`；失败任务 error 带 `phase=planning` |
| 验收 | ✅ `auto-plan-smoke.py` 88/88：观察正确页面、计划落盘、CSV 同哈希、八类拒绝路径；REST 22/22 |

**空步骤自动规划写回设计的判断**：

1. **只有 `mode=auto` 才规划。** 空 steps 不是「随便调模型的许可」；显式模式说明
   调用方没给步骤，应当退回客户端错误，而不是让引擎猜。
2. **计划先落盘，再执行第一步。** 否则进程死在规划与执行之间，resume 会再问一次
   模型并可能得到另一份计划，检查点就失去意义。
3. **任务边界由引擎检查，不由提案自证。** 模型会写出越界导航和拼错的凭据名；
   在派发前拒绝整份计划，比执行一半再失败更容易诊断。
4. **规划计入任务预算。** 引擎内规划不是免费的前奏；`max_model_calls=0` 的任务
   不能靠自动规划绕过预算。`bhx plan` 是独立进程，预算语义不同，文档分别写清。
5. **规划失败不降级成规则计划。** 没有模型就诚实失败（`planning_unavailable`），
   不用关键词规则拼一份看起来像计划的步骤。

### 已落地：Laya 适配器（2026-10-28）

| 交付物 | 状态 |
|---|---|
| 能力面 | ✅ `LayaSidecar` 只接 `Locate` / `Verify`；`Plan` / `Repair` / `VisualLocate` 明确拒绝 |
| 生命周期 | ✅ 懒启动、`{"ready":true}` 握手、常驻复用、超时/断管 kill 并重启、同配置进程内共享一个 sidecar、父进程退出后随 stdin EOF 退出 |
| 契约 | ✅ 回复必须是候选之一且带置信度；缺 `conf` 报错而不是补 0；超过 `BH_LAYA_MAX_CRITERIA`（默认 40）拒绝并要求预筛 |
| 顺序 | ✅ 默认远程优先、本地兜底；`BH_LAYA_PRIORITY=first` 反过来；同一轮 `propose` 换适配器只扣一次预算 |
| 无视觉 | ✅ 请求带图时明确拒绝（`laya has no vision`），不启动 sidecar、不产生判定；尝试仍计 1 次 |
| 验收 | ✅ `laya-smoke.py` 80/80：本地判定、not_achieved、uncertain 审批、远程优先、远程不可达回落、本地优先、坏 choice 被拒后回落、配置错误、截图被拒、MCP 两次调用一个进程 |

**Laya 切片写回设计的判断**：

1. **只接选择类能力。** Laya 不生成；把 Plan 或 Repair 塞给它，只会得到一份看起来
   像步骤的东西。能力边界写在适配器里，不靠模型自觉。
2. **默认不把它放在远程模型前面。** 实测零样本在中文后台页上把「未登录」判成已完成、
   把「已登录」判成未完成（[benchmarks.md](benchmarks.md)）。所以默认远程优先，
   本地是「没有远程模型时仍能做离散判定」的兜底；要本地优先，显式配置。
3. **进程归适配器管，不归调用方。** `browser_verify` 每次新建 router，而模型加载要
   5 s；共享一个常驻 sidecar，才让「每次调用都判一次」可用。
4. **超时就杀。** 一次挂死的推理不能拖死之后所有判定；杀掉重启，比复用一条状态未知
   的管道便宜。
5. **缺置信度不能当 0。** 0 在阈值语义里就是 `uncertain`，会把「模型没回答」伪装成
   「模型不确定」。

### 已落地：MCP 交互工具（2026-10-28）

| 交付物 | 状态 |
|---|---|
| 后台启动 | ✅ `interactive_start` 校验 spec、分配 id、立刻返回；控制器在自己的 engine 连接与浏览器里跑，不占 `task_run` 的锁 |
| 跟随 | ✅ `interactive_status`：运行/结束、当前策略版本、最近事件（tick 汇总与停止原因） |
| 热更新 | ✅ `interactive_update_policy`：版本必须严格更大；控制器在 tick 边界采纳；规则按 id + 内容判重；截止时间只能收紧 |
| 停止 | ✅ `interactive_stop`：置取消标志，等最多 2.5 s，返回最终状态；释放输入并留一个上报周期 |
| 并发 | ✅ 同 home 多连接：WAL + 5 s `busy_timeout`；每个交互任务独立 profile |
| 验收 | ✅ `interactive-smoke.py` 40/40：目标达成、页面上报「按键被按住」、热更新改变目标、陈旧版本被拒、停止后无粘键、四类坏请求、谓词每 tick 一次往返、tick 间隔有序、30 s soak（909 tick、无粘键、进程 CPU 3.6%） |

**交互工具写回设计的判断**：

1. **「启动」与「跑完」必须分开。** 交互任务按分钟计；把它塞进一次阻塞的
   `task_run`，调用方在它结束前发不出下一条指令，实时就名存实亡。
2. **策略更新只认更大的版本，且在 tick 边界生效。** 乱序到达的旧策略不能把新策略
   覆盖掉；正在派发的动作属于进入该 tick 时的版本，不做「半个 tick」的混合。
3. **更新不能延长截止时间。** 否则 `max_duration_ms` 变成建议值，一个循环可以靠
   不断续命永远不结束。
4. **规则不因更新重复执行。** 按 id + 内容判重：策略里换个无关字段不该让一个
   点击规则再点一次。
5. **后台作业要有自己的浏览器与连接。** 与 `task_run` 共享一把引擎锁意味着长时间
   工作流会冻住控制器；独立 engine + WAL 才是实时与持久化的正确分工。

### 已落地：REST 会话级接口（2026-10-28）

| 交付物 | 状态 |
|---|---|
| `bh_server::page::Page` | ✅ 浏览器 + 会话 + 观察器的唯一实现；MCP `browser_*` 与 REST 共用，避免两条前门各写一份 |
| 会话生命周期 | ✅ `POST/GET/DELETE /v1/sessions`，每个会话独立浏览器与下载目录；关闭即杀浏览器 |
| 页面操作 | ✅ `navigate` / `observe` / `actions` / `condition` / `verify` / `evaluate` / `screenshot` |
| 安全边界 | ✅ `allowed_hosts` 同时约束 `navigate` 端点与批次内 `navigate`，在第一个动作前拒绝（403）；`evaluate` 默认 isolated；下载落在会话目录 |
| 媒体类型 | ✅ `observe` 附带 JPEG 并标 `screenshot_mime`；截图端点返回 PNG 并带 `Content-Type` |
| 失败语义 | ✅ 批次在某步失败 → 422 + 索引日志（不回滚已执行前缀）；`holds:false` 不是错误；无模型适配器 → 503 |
| 验收 | ✅ `session-smoke.py` 47/47：导航、观察、动作、条件、JS 双世界、截图、下载（站点哈希与文件一致）、双会话独立、关闭与四类错误 |

**REST 会话写回设计的判断**：

1. **HTTP 无状态，所以页面要有名字。** 不引入服务端 cookie 或隐式「当前页面」；每个请求
   带 `session_id`，两个会话就能各自导航而互不踩踏。
2. **两条前门必须共用实现。** 一开始把 MCP 的 `browser_*` 逻辑抽成 `Page`，REST 直接调用；
   否则 isolated-world 默认、日志格式与截图编码会在两份代码里慢慢分叉。
3. **批次失败不假装没发生。** 422 携带执行到哪一步的日志；失败前的动作已经生效，
   调用方需要据此决定是否补救，而不是收到一个「全部失败」的假象。
4. **能力缺失与坏请求分开。** `verify` 没有适配器是 503（服务端缺东西），坏 world、
   空 actions 是 422（请求要改）；用同一个状态码会把运维问题伪装成调用错误。
5. **下载目录按会话隔离。** 页面名来自站点，可能含中文或 `..`；固定写到
   `<home>/sessions/<id>/downloads`，不落到服务器 cwd。

### 已落地：跨 Agent 验收（2026-10-28，2/3）

| 交付物 | 状态 |
|---|---|
| 同一任务、同一工具 | ✅ Claude Code 与 Pi 各自通过 `bhx mcp` 跑完 A 组（登录 → 筛选失败订单 → 导出 CSV），都只调 `task_run` + `task_status` |
| 结果可核对 | ✅ 两边导出的 `orders-失败.csv` 与站点 `/admin/__downloads` 记录同哈希（`2dd0b48d…`），不是「各自看起来成功」 |
| 耗时 | ✅ Claude Code 10 s，Pi + 本地 Qwen3.8 22 s（CLI 直跑约 0.3 s；差值主要是 Agent 的推理与工具循环） |
| 第三个 Agent | ⏸ 环境阻塞，未完成：Codex 账号不接受可用模型，OpenCode 在 `init` 挂起，Qoder 请求报错 |

数字与原始错误记在 [benchmarks.md](benchmarks.md) 的「Phase 4 跨 Agent 验收」一节。
**尚未完成**：第三个 Agent 的通过记录（换机器或修好 Codex/OpenCode 环境后补跑即可，
harness 侧不需要改动）。

## Phase 5 — 实时优化（2 周）

- tick 频率与抖动优化；采样率与动作率解耦。
- 高频动作合并（同一帧内的移动/按键聚合）。
- 状态谓词求值的批量化，减少 CDP 往返。
- 延迟预算表落地，并给出「模型可介入频率」的实测结论。

**退出条件**：C 组 5/5；持续 30 秒交互无粘键、无失控；控制器 CPU 占用可接受。

### 已落地：谓词批量化与 tick 指标（2026-10-28）

| 交付物 | 状态 |
|---|---|
| 一次往返判全部谓词 | ✅ 目标 + 每条规则的 `when` 合成一个脚本；每个谓词自带 try/catch，一条表达式抛错不会拖垮整个 tick；无谓词的规则是占位 `null`，不会被当成真值 |
| 错误语义不变 | ✅ 每条谓词的异常仍按「连续 5 次异常即 anomaly」计数；传输失败只计一次，不按规则条数放大 |
| tick 抖动可观测 | ✅ `interactive.stopped` 带 `evals`、`tick_ms_p50/p95/max`；配速仍按锚点、落后不补发 |
| 延迟预算 | ✅ 30 Hz soak：909 tick / 31 s，p50 33 ms、p95 34 ms、max 42 ms，CPU 3.6%；60 Hz × 5 规则：188 tick / 3 s，`evals == ticks`（改前每 tick 6 次往返） |
| 逐规则命中 | ✅ `interactive.stopped.rules[]` 列出真正触发过的规则（`id` / `entries` / `actions`），相加等于总动作数；聚合计数分不清「20 次跳跃」和「19 次跳跃 + 一次撞死重开」 |
| 验收 | ✅ `interactive-smoke.py` 42/42：原有 29 项 + 谓词批量、tick 有序、30 s soak（无粘键、CPU < 1 核）+ 逐规则计数两条 |
| 模型介入上界 | ✅ 循环内无模型调用；策略更新在下一个 tick 采纳（16–33 ms），只保留最新版本 |

数字见 [benchmarks.md](benchmarks.md)「Phase 5 延迟预算」。**尚未做**：同一帧内的
移动/按键动作合并——当前规则在「进入」时派发一次，已经避免了每 tick 重复按下；
真正的同帧合并要等有高频鼠标轨迹的用例，避免为没有的负载加复杂度。

### 已落地：歧义定位的模型仲裁（2026-10-29）

按「Harness 理解状态、模型只在必要时做局部决策」的方向做的第一个增量：把
`Capability::Locate` 接到它唯一自然的位置——解析器已经找到多个候选、但按不变量
不能自己挑一个的那一步。

| 交付物 | 状态 |
|---|---|
| 默认行为不变 | ✅ 不写 `policy.arbitrate_ambiguous` 时一律不问模型（`calls == 0`），仍是原来的 `locator_ambiguous` |
| 候选由解析给出 | ✅ 模型只看到 `c1..cN` 与真实标签，不能增删候选；假 id 被路由拒绝，不派发任何东西 |
| 同一次观察 | ✅ 匹配与仲裁共用一个快照（修掉了一个自己引入的二次观察 bug：状态文本与候选必须同一时刻） |
| 阈值与上限 | ✅ `min_confidence`（默认 0.7）、`max_candidates`（默认 8）；超限**不花调用**；坏策略在开工前拒绝 |
| 不可区分即放弃 | ✅ 候选描述完全相同时不问模型——实测同名两按钮下 Laya 给 0.67/0.83 的自信答案且锁定同一元素，阈值拦不住 |
| 可审计 | ✅ `action.executed.arbitration`（strategy / eid / model / confidence / candidates / reason）；放弃时错误里写明是哪一种原因、该怎么修 |
| 纯文本模型也能选 | ✅ `Kind::Text` 适配器现在支持 `Locate`（读标签是文本任务，不需要图）；`VisualLocate` 仍只给视觉模型 |
| 验收 | ✅ `arbiter-smoke.py` 48/48 + `bh-exec` 单元 24 项；两套页面（同名 vs 标签可区分），点击结果以站点 `/arbiter/__click` 记录为准 |
| 实测 | ⚠️ 真实 Laya 在可区分场景**选错**（PDF，0.625），被 0.7 阈值拦住；三候选时读标签读对（CSV 0.78、无关项 0.093）。见 [benchmarks.md](benchmarks.md) |

### 已落地：控制器里的模型选策略（2026-10-29）

目标里「用决策模型替代一部分简单交互决策」的正脸：模型不参与 tick，只每隔
`every_ms` 选一次**作者已经写了规则的策略**，结果以 `s.d` 注入状态，规则照常以
30 Hz 执行。这样一次 150–500 ms 的判定不会饿死循环，而模型的输出被限制在
「从这几条里选一个」——它不能发明策略，规则才是行为的全部来源。

| 交付物 | 状态 |
|---|---|
| 协议字段 | ✅ `controller.decide = {question, options[], every_ms, min_confidence}`；`options ≥ 2`、id 限 `[A-Za-z0-9_-]+`（要进谓词）、`every_ms` 下限 250 ms |
| 问在循环外 | ✅ `decision_task` 与循环并行、同时最多一个未完成请求；循环 `try_recv` 取最新答案，慢一次只是反应迟钝 |
| 决策进状态 | ✅ `s.d` / `s.d_confidence`，规则用普通 `when` 分支；没有第二套"模型的看法" |
| 版本一致才采纳 | ✅ 策略热更新换掉选项列表后，旧答案作废（`policy_version` 不符即丢弃） |
| 没模型也能跑 | ✅ 没有适配器时记一次警告、`decisions: 0`、任务照常完成——既不失败，也不假装做了决定 |
| 可计量 | ✅ `interactive.stopped.decisions`；与 `actions` 分开计（不同的成本）；占用 `max_model_calls` |
| 验收 | ✅ `decide-smoke.py` 29 项 + `bh-core` 单元 4 项（校验、id 约束、间隔下限、默认为空） |
| 实测有收益 | ✅ 新站点 `testkit/sites/plane/`（两条航线，一条来得及一条来不及）：同一套规则，模型选慢航线 **得 3 分**、选快航线 **得 0 分**（分数读服务端遥测） |
| 真实 Laya | ⚠️ 同一场景三次都得 **0 分**：第一次选对了，却在火力窗口打开前改到另一条航线。单独喂它那一局的中盘状态（`nearest: "left"`）它答对（0.690），只把选项顺序对调置信度就掉到 0.511——刚好越过阈值。见 [benchmarks.md](benchmarks.md) |

踩到的坑记在 `testkit/sites/plane/index.html` 顶部：游戏世界**按帧积分**在测试负载下
会因为后台标签丢帧而整体慢于时钟，测出来的是机器不是策略；改成「位置是时间的函数」后稳定。

**由此测出的下一步（比换模型更值钱）**：策略切换需要**迟滞**。三次失败形状相同——
第一次选对、中途反悔、错过窗口。置信度阈值挡不住：反悔的答案也有 0.51 这种"刚刚够格"的置信度。
可行的做法（尚未做）：换策略比保持策略要求更高置信度，或要求连续两轮一致才切。
这是控制回路的问题，不是模型的问题——换任何模型都要面对。

**尚未做**：候选生成/排序本身（现在候选就是解析器的原始命中，不做相关性筛选，
`to_state_text(goal)` 里的 goal 也只当标题用）——这是这条线的下一步，也是评审
P0 里真正缺的那一件；以及跨 A/B/C/D 的统一对照指标。

## Phase 6 — 持久化与可观测（持续）

- 长任务恢复、定时执行、运行历史。
- 每次运行的 trace 可回放（截图序列 + 动作序列 + 事件序列）。
- 性能报告：成功率、耗时分布、模型调用数、无效动作率。
- ✅ 远端浏览器（attach）、多任务并发调度（`schedule run --concurrency`）。

### 已落地：运行历史与 trace（2026-10-28）

| 交付物 | 状态 |
|---|---|
| 动作级事件 | ✅ 每个派发的动作写一条 `action.executed`（步骤、序号、类型、耗时、是否视觉回落、错误）；只记类型不记载荷，密码/输入文本不进事件 |
| `bhx stats` | ✅ 任务数/终态分布、成功率、耗时 p50/p95、模型调用、动作数与失败动作率；`--json` 供脚本读，空 home 明确说「没有任务」 |
| `bhx trace <id>` | ✅ 事件时间线 + 步骤 + 产物（含 sha256）；`--json` 给机器，`--out DIR` 落成自包含包（`trace.json` + `trace.html` + `artifacts/`） |
| `trace.html` 回放器 | ✅ 单文件、无外部请求；步骤卡（状态/尝试/耗时/逐动作/截图）、事件时间线（类型过滤、动作耗时条、截图缩略图）、按真实间隔回放（0.5×–4×、空格暂停、可拖动进度）、点事件看原始 `data` 与整图、产物直链；默认停在第一个失败动作上 |
| 统计口径 | ✅ 取消不算失败（成功率分母是 succeeded+failed）；耗时只算已结束任务；失败动作率只覆盖有 `action.executed` 的任务，旧 home 不会被伪造成 0% |
| 截图序列 | ✅ 每步结束时截一张 JPEG（`step.screenshot` 事件 + `kind=screenshot` 产物）；失败步骤无条件截，全序列由 `policy.trace_screenshots` 打开；截图失败只警告，不改任务结果 |
| 验收 | ✅ `trace-stats-smoke.py` 45/45，并入 `run-phase4.sh` 第九组 |

**写回设计的判断**：

1. **动作事件在步骤返回后写，不在派发时写。** 引擎的日志句柄 `&Engine` 不是 `Send`，
   把回调塞进 Runner 会让整个任务 future 失去 `Send`。代价是事件按步骤成组到达，
   `index` 与 `elapsed_ms` 保住了顺序与时长；`trace` 因此仍然能按真实顺序回放。
2. **trace 只记动作类型，不记动作载荷。** 输入文本在 materialize 之后可能是凭据值；
   回放需要的是「第几步、什么类型、成功与否、多久」，不是密码。
3. **统计从 store 复算，不额外埋点。** 同一个数字用 `stats`、`trace`、SQL 三种读法
   必须一致；埋点会引入第二套真相。
4. **空数据不是 0%。** 没有完成的运行、没有动作事件时返回 `null`，而不是 0，
   否则「还没跑过」和「全失败」在报表里长得一样。

**写回设计的判断（回放器）**：

1. **回放是「看记录」，不是「再跑一遍」。** 播放器只读 `trace.json`；重放会重新产生副作用，
   而 trace 的意义是事后能解释发生了什么。
2. **单文件、零外部依赖。** `file://` 直接能开、能发给别人；没有 CDN、没有构建步骤，
   smoke 用 `--dump-dom` 验证它在真实浏览器里渲染出了时间线。
3. **数据块按 JSON 转义 `<`。** 页面标题、错误信息都来自被驱动的站点，可能包含
   `</script>`；嵌入时统一转成 `\u003c`，既仍是合法 JSON，也无法闭合脚本标签。
4. **默认定位到失败。** 打开 trace 的人通常想知道「哪一步坏了」；没有失败时停在最后一个事件。

**写回设计的判断（截图）**：

1. **失败步骤无条件截图，全序列要显式开。** 失败现场是最难复现、最值钱的证据；
   每步都截则是每步一次 CDP 往返 + 20–35 KB 磁盘，默认关掉，需要时在 policy 里写
   `"trace_screenshots": true`。
2. **截图失败不是任务失败。** 浏览器崩溃或截图超时只留一条 warn，步骤状态与验证结果
   照旧；证据缺一张，不该把已经完成的操作改判成失败。
3. **截图走产物表，不另开一套存储。** 它和下载文件一样有 name / bytes / sha256，
   `task artifacts`、`trace`、`--out` 的复制与直链全部复用现成路径。

**尚未做**：动作级截图（当前粒度是每步一张，不是每个动作一张）；视觉校验的图片
预处理（当前直接发原始 PNG，没有缩放/裁剪/标注）；调度相关项见下一节。

### 已落地：定时执行（2026-10-28）

| 交付物 | 状态 |
|---|---|
| `bhx schedule add <script> --every 30m` | ✅ 任务快照存进 `schedules` 表；间隔支持 `s/m/h/d` 与组合（`1h30m`），下限 1 s；首次运行立即，之后按间隔 |
| `bhx schedule ls [--json]` / `rm` / `enable` / `disable` | ✅ 列出间隔、下次运行、上次状态/任务/错误；`--json` 给脚本；启用后从当前时刻起算一个间隔 |
| `bhx schedule run [--once] [--concurrency N]` | ✅ 常驻轮询（250 ms）取到期项执行，或单趟执行后退出；默认一次一个，`--concurrency`（1..=8）让不同调度并行；`--once` 有失败运行时退出非零 |
| 凭据 | ✅ 只存名字：`add` 扫描 `${cred:name}` 得到 `cred_names`，值在运行开始时从 `BH_CRED_<NAME>` 读；缺任何一个整次拒绝启动，错误只报变量名 |
| 槽位语义 | ✅ 先占槽再运行（`next_run_at_ms = now + interval`），运行中崩溃不会留下可被重启盲重跑的任务；睡过三个间隔只跑一次，无补偿爆发 |
| 失败处理 | ✅ 记录 `last_state` / `last_error`，调度保持启用——瞬态失败不该永久停掉调度；重试等下一个正常槽位 |
| 并发 | ✅ 每个运行自带 `Engine`，上限 `--concurrency`；同一调度的重叠槽位消费为 `skipped_overlap`，不排队；默认 1 保证兼容旧行为 |
| 验收 | ✅ `schedule-smoke.py` 61/61，并入 `run-phase4.sh` 第十组 |

**写回设计的判断**：

1. **首次运行立即，不等一个间隔。** 「加进去」就是希望它发生；等 30 分钟才第一次跑
   会让人以为没生效。间隔只约束后续运行。
2. **槽位在运行前消费。** 与 Phase 2 的崩溃恢复相反：可恢复的是步骤级检查点，
   调度槽位不可恢复。已派发的任务可能已经提交了不可逆动作，重启后自动重跑是盲重试。
3. **凭据只存名字，值从环境读。** 调度器天生无人值守重复执行，把值存进 SQLite
   等于把密码长期放在 home 里；名字 + 环境变量让同一个 home 在 CI 与笔记本上都能跑。
4. **缺值整次失败，而不是部分替换。** 只替换部分占位符会把 `${cred:password}` 当
   字面量敲进页面，在站点留下一次错误密码的失败登录——比不跑更糟。
5. **默认串行，并发要显式要。** 一个调度进程同时起多个浏览器上下文会把 CPU 与站点
   配额问题混在一起，所以默认仍是 1；`--concurrency N` 把选择权交给运维，上限 8，
   每个运行独立 `Engine`、独立浏览器。同一调度的重叠槽位记为 `skipped_overlap`：
   长运行不该变成一串追赶，也不该让同一任务同时跑两遍。

**已知边界**：带审批闸门的步骤会让这次调度运行停在 `waiting_approval`（记为
`last_state`），调度器不会替你批准；下一槽位跑的是**新任务**，旧任务仍可用
`bhx task approve` + `bhx task resume` 继续。不可逆动作因此不会因为「到点了」被自动放行。

**尚未做**：调度进程的守护配置（launchd/systemd）、`schedule` 的 REST/MCP 入口、
跨进程的全局并发配额（当前上限只约束单个 `schedule run` 进程）。

### 已落地：远端浏览器（attach，2026-10-28）

| 交付物 | 状态 |
|---|---|
| `BH_CDP_URL` / `BrowserOptions.cdp_url` | ✅ `http(s)://host:port`（取 `/json/version` 的 ws）、`ws(s)://…` 直连、裸 `host:port` 三种写法；命中即 attach，不再拉起本地 Chrome |
| 只关自己开的标签 | ✅ 每个 `Target.createTarget` 记进 `Browser::targets`，退出时逐个 `Target.closeTarget`（最多等 2 s）；远端浏览器进程与别人的标签一概不动 |
| 失败路径 | ✅ 端点不通 5 s 内失败，错误点名 `--remote-debugging-port` 或 `ws://`；任务以 `failed` 收尾（`task.finished` 带 `phase: "launch"`），不留 `pending` |
| 验收 | ✅ `remote-smoke.py` 17/17，并入 `run-phase4.sh` 第十一组 |

**写回设计的判断**：

1. **attach 只接管会话，不接管生命周期。** 远端浏览器可能是开发者在看的、CI 共享的、
   或挂着别的 Agent 的；`child = None` 让「shutdown 杀进程」这段代码根本走不到。
2. **只关自己建的 target，而不是「关掉所有 page」。** 计数全部 CDP target 会被扩展的
   `background_page` / `service_worker` 骗到——它们自己会闪；断言和清理都按 target id 走。
3. **端点错了就报错，不回落本地启动。** 静默换一个浏览器会让「在远端跑」的请求实际跑在
   本机，远端什么都没发生，却看起来成功了。
4. **启动失败要落成失败任务。** 早期实现把 launch 错误直接向上抛，任务停在 `pending`；
   现在 `Engine::execute` 在启动阶段就写 `failed` + `task.finished(phase=launch)`。
5. **同一个浏览器 = 同一个会话。** 页面把登录态放在 profile 的 `localStorage` 里，
   第二次 attach 会直接是已登录状态。这不是 bug，是复用浏览器的真实语义；测试因此
   给第二次运行换了 origin（见 [testing.md](testing.md#phase-6远端浏览器attach)）。

**已知边界**：没有端口转发、代理认证（`ws://` 带 token 的地址可以原样传入）与断线重连
（见「明确推迟」）。`Browser.setDownloadBehavior` 的 `downloadPath` 由**远端**解释，
跨机器 attach 时文件会落在远端主机上——本仓库只在同一台机器上验证过，跨机下载的校验方式
（共享目录或只依赖 `downloadCompleted` 事件）尚未做。

## 明确推迟

以下先不做，避免分心：

- 分布式工作流引擎、跨机器调度。
- 图形化编排 UI。
- 视觉模型的微调。
- 反检测对抗。
- 多用户权限体系（先做本地单用户 + 任务级白名单）。

## 每阶段的固定动作

1. 更新 `docs/benchmarks.md`（真实数字）。
2. 跑一遍对应验收子集，记录成功率与指标。
3. 失败用例写成回归，进 `testkit/suites/`。
4. 若某阶段退出条件连续两次不达标，**缩小范围**而不是堆代码。
