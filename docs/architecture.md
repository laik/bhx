# 架构设计 v0.2

## 1. 总览

```
                    Claude Code / Pi / Qwen / 脚本 / CI
                                  │
                 ┌────────────────┴─────────────────┐
                 │        API 层  REST / WS / MCP    │
                 └────────────────┬─────────────────┘
                                  │
                 ┌────────────────┴─────────────────┐
                 │            Task Engine            │
                 │  规划·状态机·检查点·恢复·产物·审批  │
                 └───────┬──────────────────┬────────┘
                         │                  │
                 ┌───────┴───────┐   ┌──────┴─────────┐
                 │  Task Router  │   │  Model Router  │
                 │  每步选执行路径 │   │  按能力选模型   │
                 └───────┬───────┘   └──────┬─────────┘
                         │                  │
      ┌──────────────────┼──────────────────┼───────────────┐
      │                  │                  │               │
 DOM Executor     Vision Executor   Interactive Ctrl   Rule Engine
 结构化操作         视觉定位与验证      策略循环执行        确定性判断
      └──────────────────┴──────────────────┴───────────────┘
                         │
                 ┌───────┴────────┐
                 │  Action Runtime │  解析·可执行性·执行·后置条件·取消
                 └───────┬────────┘
                 ┌───────┴────────┐
                 │ Browser Runtime │  上下文·页面·导航·输入·下载·上传
                 └───────┬────────┘
                         │  CDP (WebSocket)
                 ┌───────┴────────┐
                 │    Chromium     │
                 └───────┬────────┘
                         │
                 ┌───────┴────────────┐
                 │  Observation Engine │  AX·DOM·截图·事件·元素索引·差分
                 └───────┬────────────┘
                         └──► 回灌 Task Engine / Executor / 验证器
```

**相对 v0.1 提案的三处修正：**

1. **Observation Engine 不是 Browser Runtime 的下游，而是与它并列的 CDP 消费者。**
   它直接订阅 CDP 事件，同时按需主动拉取快照；把它画在运行时之下会误导实现者。
2. **Task Router 是「每步决策函数」，不是「任务开始时的一次性分类器」。**
   一个任务的第 3 步可能用 DOM、第 4 步用视觉；某步 DOM 定位失败时还要能就地改道。
3. **Interactive 任务不走规划流水线。** 它有自己的生命周期（`policy_running`），
   共用存储、事件与产物，但不经过 Step 规划，否则会把「实时」做成「每帧想一遍」。

## 2. 为什么是多执行引擎

单一执行方式必然在某类页面上失效：

- 纯 DOM：Canvas、WebGL、图片按钮、动态生成的遮罩层无解。
- 纯视觉：慢（每步一次模型）、贵、且对「读一张表格的 30 行数据」这类任务质量远不如 DOM。
- 纯坐标：页面一改版全崩，且无法验证。

所以 Harness 的价值不在「选一种」，而在**为每个步骤选路径，并允许路径之间交接状态**。

### 路径选择规则（Task Router）

按顺序判断，命中即用：

| 条件 | 路径 |
|---|---|
| 步骤已给定明确 locator（role/name/testid/label） | DOM |
| 目标在 DOM/AX 中可见、可交互、命中唯一 | DOM |
| 目标在 canvas / iframe 内不可达 / AX 节点缺失或重复不可消歧 | Vision |
| 步骤是持续动作（按键保持、连续拖拽、轨迹） | Interactive |
| 只需要判断「是否达成」 | Rule Engine（JS 谓词），不调模型 |

`mode: "auto"` 时由 Router 决定；调用方也可以显式指定 `executor` 覆盖。
**升级（escalation）是显式的**：DOM 失败 N 次后，Router 可升到 Vision，并把已获得的候选信息作为提示传下去。

## 3. 分层职责

每一层都遵守同一契约：**输入是上一层的结构化输出，输出是本层的结构化结果 + 可回放的事件**。

### 3.1 Browser Runtime

| 项 | 内容 |
|---|---|
| 负责 | 进程生命周期、browser context、page/target、导航、输入事件、下载、上传、对话框、权限 |
| 不负责 | 元素语义、任务状态、模型调用 |
| 关键对象 | `Session`（一个 context + 一组 page）、`PageHandle` |

要点：

- **Session = 一个 browser context**，任务默认各自一个，互不污染 cookie/localStorage。
- 网络与渲染进程异常要转为事件，而不是让调用方看到 CDP 报错原文。
- 崩溃检测：CDP 连接断开 → 会话标记 `crashed` → 任务按策略进入 `failed` 或 `recovering`。
- 导航后必须等 `Page.lifecycleEvent` 的 `load`/`networkIdle`，**禁止固定 sleep**。

**并发与调度**

- 一个 Chromium 进程托管 N 个 browser context；**每个任务一个 context**，登录态互不可见。
- 默认并发任务数 4（可配），超出排队；`interactive` 任务占用独立槽位，避免被长任务饿死。
- **context 内动作严格串行**：CDP 输入是页面级全局状态，并发点击会产生无法归因的结果。
- profile 目录不能被两个 Chrome 进程同时使用——同一 `session_id` 已有活动会话时，
  新的 `task.start` 要么复用该会话（同一 context），要么显式报 `browser.session_busy`。
- 排队中的任务可以立即取消；取消不消耗模型预算。
- 残留 page/context 由后台回收，按 `created_at` 与最后一次活动时间。

### 3.2 Observation Engine

观察是 Harness 的「眼睛」，也是最容易被做成性能黑洞的地方。

**三种观察产物：**

| 产物 | 内容 | 成本 | 何时用 |
|---|---|---|---|
| `PageState` | URL、标题、可见文本摘要、交互元素索引 | 低 | 默认每次动作后 |
| `Snapshot` | 完整 AX 树 + 关键 DOM 属性 + 元素盒模型 | 中 | 步骤开始、定位失败 |
| `Screenshot` | 视口/元素/整页图像（JPEG/WebP，DPR 感知） | 中高 | 视觉路径、人工排查 |

**元素索引（Element Index）是核心数据结构。** 每次观察生成一版带稳定短 id 的元素表：

```
e1   button "导出 CSV"          enabled
e2   textbox "订单号"            value=""
e3   combobox "状态"             value="全部"
e4   link "下一页"               visible
```

规则：

- id 形如 `e1..eN`，**只在一次观察内有效**，跨观察引用必须带 `observation_id`。
- 排序稳定（文档序 + 可见性 + 可交互性），同页面重复观察得到相同顺序。
- 只收录：可交互元素、有文本的块、带 aria 角色的容器；默认上限 120 个，可配。
- 每个元素带内容哈希，用于差分。

> 这个表格本身就是给本地决策模型（Laya）的输入格式。**格式必须冻结并版本化**
> （`STATE_FORMAT_VERSION`），否则阈值与准确率全部失效——这是实测得到的教训，不是理论洁癖。

**差分（diff）**：动作后只回传「新增 / 消失 / 文本变化 / 状态变化」的元素，长流程下这是把
每步 token 从几千压到几十的唯一手段。

### 3.3 Action Runtime

所有动作都走同一条管线：

```
resolve → actionability → dispatch → post-condition → observe-diff
```

- **resolve**：按 locator 策略链解出唯一点击目标；多命中即报歧义（附候选列表），不赌。
- **actionability**：可见、稳定（两次盒模型一致）、未被遮挡、可接收指针事件、未 disabled。
  这是 Playwright 最有价值的部分，我们自己实现，因为 Rust 侧没有官方绑定。
- **dispatch**：走 CDP `Input.*`，不用 JS `element.click()`（后者绕过真实事件序列，且很多站点不认）。
  **点击 = press + release，不发 `mouseMoved`。** 实测一次 `mouseMoved` 要 16.6 ms
  （接近一帧），而 press+release 只要 0.4 ms；默认先移动会让每次点击白丢一帧。
  悬停拆成独立的 `hover` 动作；只有可执行性检查发现元素是 hover-gated 时才补发移动。
- **post-condition**：有则等条件满足，无则做一次轻量观察并记录「未验证」。
- **取消**：贯穿全链路 `CancellationToken`；中止时统一释放按下的键、鼠标按键、进行中的拖拽。

动作集合见 [protocol.md](protocol.md#3-动作)。

### 3.4 三种执行器

#### DOM Executor

面向真实工作。输入是结构化步骤，输出是执行结果与验证结论。**执行期间零模型调用。**

```
步骤 → 解析 locator → 可执行性检查 → 输入 → 验证 → 下一步
                ↑                                    │
                └────────── 失败时重新观察 ←──────────┘
```

失败处理顺序：重新观察 → 换 locator 策略 → 缩小容器范围 → 升级到 Vision → 请求模型重新规划。

#### Vision Executor

只负责「视觉模型擅长的部分」：目标识别与界面理解。**坐标换算、动作调度、状态管理仍在 Harness。**

```
截图 → 裁剪/缩放 → 模型输出 (bbox 或 描述) → 归一化坐标
     → 换算到视口坐标（DPR / scroll / iframe offset / zoom）
     → 点击 → 重新观察 → 用状态条件验证（不是用截图相似度）
```

要点：

- 模型**不必**输出像素坐标；输出归一化 bbox 或语义描述 + 候选区域均可，由 Harness 换算。
- 截图按需取、可缓存、可裁剪 ROI；不要每帧整页截图。
- 验证优先用 DOM/AX 状态变化，截图只作兜底，避免「看起来像成功」。

#### Interactive Controller

面向连续交互（游戏是其中一类）。核心是：**策略在 Harness 内以固定频率执行，模型不在回路里。**

```
Policy (规则集 + 观察源 + 终止条件)
        │
   ┌────┴────┐  tick 30Hz
   │  采样状态 │◄── 页面 JS 谓词 / 轻量 DOM 读取
   │  匹配规则 │
   │  执行动作 │──► Input.dispatchKeyEvent / mouseEvent
   └────┬────┘
        │
   终止：达成目标 / 超时 / 取消 / 异常 / 策略过期
```

策略的三种形态，按能力递增：

1. **声明式**：`while state.y < 100: hold KeyW`（由 DSL 表达，最安全）。
2. **脚本式**：给定 JS 谓词与动作表，由 Rust 侧状态机循环执行。
3. **模型低频修正**：每 N 秒或状态跳变时，让模型/本地决策器更新规则集（限频、限次）。

硬性要求：`max_actions_per_sec`、`max_duration_ms`、`release_all_on_exit`、
策略版本号（旧版策略的迟到动作必须丢弃）。

### 3.5 Task Engine

任务模型：

```
Task
 ├── id / kind (workflow | interactive)
 ├── goal
 ├── mode (auto | dom | vision | interactive)
 ├── policy          执行预算：模型调用上限、总时长、是否允许不可逆动作
 ├── steps[]         每步：动作、前置条件、成功条件、超时、重试、副作用级别
 ├── state           pending|running|waiting_input|waiting_approval|paused|succeeded|failed|canceled|crashed
 ├── checkpoints[]   每步结束落盘，可恢复
 ├── artifacts[]     下载文件、提取数据、截图、trace
 └── result          结论 + 指标 + 失败上下文
```

**升级点（escalation）是唯一允许调模型的地方：**

- 步骤在重试预算内失败；
- 出现未预期的对话框 / 权限请求 / 登录墙；
- 观察结果与计划前提矛盾（页面结构变了）；
- 需要判断「这个结果是否满足用户目标」。

其余一切走规则与已有计划。这条约束直接决定「每任务模型调用 ≤ 2 次」是否可能。

**状态机**（workflow）：

```
pending ──► running ──┬──► succeeded
                      ├──► failed
                      ├──► canceled
                      ├──► waiting_approval ──► running | canceled
                      ├──► waiting_human ──► running
                      └──► crashed ──► recovering ──► running | failed
```

**幂等与副作用保护**：不可逆步骤执行前先查该步骤的完成记录；
若上次执行结果未知（进程崩溃在动作与验证之间），默认**不重试**，转为 `waiting_approval`。

### 3.6 Model Layer

模型是**建议生成器**，不是执行者。

```
Executor 需要什么都由能力声明定义：
  plan      目标 → 步骤序列
  locate    候选元素 → 选一个
  visual_locate 截图 → 目标框
  verify    状态 + 目标 → 是否达成
  repair    失败上下文 → 修正后的步骤
```

- 每个适配器实现同一套 `capability → proposal` 契约，输出**提案**而非动作。
- `Policy Validation` 是必经环节：schema 校验、权限校验、副作用校验、预算校验。
- **模型选择与执行器选择正交**：同一个文本模型可以给 DOM 步骤做规划，也可以给视觉步骤做目标理解。
- **本地决策模型（Laya）的位置**：`locate` / `verify` / `route` 三类离散选择。
  它是 stdin/stdout JSON sidecar（`sidecars/laya/decide.py`），默认 torch + MPS，
  ONNX/CPU 作回退。本机实测：冷启动 5.5s，3 候选 58ms、10 候选 117ms、30 候选以上约 250ms，
  常驻 1.69GB（见 [benchmarks.md](benchmarks.md)）。
  两件事不变：**候选必须先由规则压到 ≤10**，否则准确率塌陷；**不进 30Hz 控制回路**，
  实时路径靠策略循环。概率阈值必须按部署实测，状态文本格式一变结果就变（`STATE_FORMAT_VERSION`）。

### 3.7 API 层

三种接口，一套语义：

| 接口 | 用途 |
|---|---|
| REST | 创建会话、启动任务、查询状态、取产物 |
| WebSocket | 事件流（步骤进度、观察差分、审批请求） |
| MCP | 给 Agent 的工具面，任务级优先，低级原语保留 |

原则：**MCP 暴露的是「任务」和「观察」，不是「每帧鼠标坐标」。**
低级动作接口存在，但 Agent 不应被引导去用它做长流程。

## 4. 技术选型

| 层 | 选择 | 说明 |
|---|---|---|
| 语言 | Rust 2024 edition（本机 1.93） | 异步、取消、零成本抽象，适合长驻进程 |
| 浏览器控制 | **CDP over WebSocket，自建薄客户端** | 见下方修正 |
| 异步运行时 | tokio | |
| 存储 | SQLite（任务、检查点、产物索引）+ 文件系统（产物本体） | 单机足够，易备份 |
| HTTP / WS | axum | |
| MCP | 官方 Rust SDK（`rmcp`）为候选；若版本不满足，手写 JSON-RPC over stdio（协议本身很小） | 实现时验证 |
| 序列化 | serde + serde_json | 协议即类型 |
| 可观测 | tracing + JSONL trace + 可选 OpenTelemetry | 每次运行为一个 trace 目录 |

### 关于 Playwright 的修正

**Playwright 不能从 Rust 链接**（官方绑定只有 JS/TS、Python、Java、.NET）。
更关键的是：本项目需要的能力——直接订阅 CDP 事件、隔离 world 求值、精确分发输入、
接管已登录的浏览器——本来就要求裸 CDP，套一层 Playwright 只会增加延迟与不可控因素。

Playwright 真正值得借鉴的是它的**可执行性检查与自动等待**，这部分在 Action Runtime 里自研，
成本很低。若将来确实需要 Playwright 生态（比如某些录制能力），以独立 sidecar 形式接入，
不进关键路径。

### 关于 CDP 客户端

候选：`chromiumoxide`（成熟度尚可，但类型覆盖滞后、维护节奏不稳）。
建议：自建薄客户端（约 1–2k 行），只覆盖本项目用到的 Domain，特性检测与控制权都在自己手里。
若时间紧，先用 `chromiumoxide` 起步，接口层与它隔离，便于替换。

## 5. 代码结构（建议）

```
crates/
  bh-core        // 协议类型：Task/Step/Action/Locator/Observation/Event/Artifact
  bh-cdp         // CDP 传输、target/session 管理、事件分发
  bh-browser     // Browser Runtime：context/page/导航/输入/下载/上传
  bh-observe     // Observation Engine：AX/DOM/截图/元素索引/差分/序列化
  bh-action      // Action Runtime：解析/可执行性/分发/后置条件/取消
  bh-exec        // DOM / Vision / Interactive 执行器 + Task Router
  bh-task        // Task Engine：状态机/检查点/产物/审批/恢复
  bh-model       // Model Router + 适配器（文本 LLM / VLM / Laya / 规则）
  bh-server      // REST + WS + MCP
  bhx            // 二进制：CLI + serve
testkit/
  sites/         // admin / form / canvas / game / chaos 本地站点
  suites/        // A/B/C 验收套件
docs/
```

依赖方向严格单向，执行器不得反向依赖 Task Engine：

```
        bh-core                 协议类型，无内部依赖
           ▲
      ┌────┴────┬─────────┐
   bh-cdp   bh-model   （规则引擎在 core 内）
      ▲        │
 bh-browser    │
      ▲        │
 ┌────┴───┬────┴────┐
bh-observe  bh-action
      ▲        ▲
      └───┬────┘
       bh-exec              DOM / Vision / Interactive
          ▲
       bh-task              + SQLite 存储、检查点、产物
          ▲
       bh-server            REST / WS / MCP
          ▲
         bhx
```

注意：`bh-model` 只依赖 `bh-core`，被 `bh-exec` 使用——**视觉执行器必须调用模型**，
把模型层放在任务层之上会让 Vision Executor 无法实现。这是 v0.1 图里的一个真实错误。

完整 trait 签名与不变量见 [interfaces.md](interfaces.md)。

## 6. 关键权衡

| 权衡 | 选择 | 理由 |
|---|---|---|
| 语义定位 vs 坐标 | 语义优先，坐标兜底 | 坐标不可验证、不可重定位 |
| 每步调模型 vs 预编译计划 | 预编译计划 + 显式升级点 | 模型是延迟与成本的主要来源 |
| 实时回路内模型 vs 策略外置 | 策略外置 | 模型 0.5–4s，实时需要 30ms 级 |
| 大而全工作流引擎 vs 最小状态机 | 最小状态机 + 检查点 | 先证明能完成真实任务，再谈分布式 |
| 一个浏览器 vs 每任务一个 context | 每任务一个 context | 隔离登录态与故障影响 |
| 拟人化 vs 确定性 | 默认确定性 | 可复现才能调试 |

## 7. 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| 本地决策模型（Laya）在真实页面上准确率不足 | 视觉/定位路径不可靠 | 只用于离散选择且可回退到规则或文本模型；阈值实测；保留纯规则路径 |
| 站点反自动化 | 任务失败 | 独立 profile、降低频率、不做规避检测的军备竞赛；必要场景用远端浏览器 |
| 长时间任务中途崩溃 | 结果丢失 | 每步落盘检查点，崩溃后从最后已验证步骤恢复 |
| 视觉模型坐标误差 | 点偏 | 归一化框 + 元素命中校验（点击后检查目标元素状态） |
| 观察数据过大 | token 爆炸、延迟高 | 元素上限、差分、按需截图 |

## 8. 命名

本机 `~/.local/bin/browser-harness` 是 browser-use 的 CLI，与本项目无关。
为免混淆，本项目：

- 仓库名保留 `my-browser-harness`（沿用既有路径）；
- 二进制名 **`bhx`**；
- 文档与协议中用「Harness」指本项目，用「browser-use/browser-harness」指另一个项目。
