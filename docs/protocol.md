# 协议草案 v0.1

本文件是 Harness 的对外契约。实现时类型定义放 `bh-core`，JSON 只是线格式。

## 0. 约定

- `protocol_version: "0.1"`，破坏性变更升 minor 前的 major。
- 所有时间字段用毫秒整数（`timeout_ms`），时间戳用 RFC3339。
- 所有 id 为 ULID 字符串。
- 未知字段必须忽略（前向兼容），未知枚举值必须报错（避免误执行）。
- `STATE_FORMAT_VERSION` 单独版本化：它变了，本地决策模型的实测阈值就作废。

## 1. 任务

```json
{
  "protocol_version": "0.1",
  "id": "01J8Z...",
  "kind": "workflow",
  "goal": "登录后台，筛选今天的失败订单，导出 CSV",
  "mode": "auto",
  "start_url": "https://admin.example.com/",
  "policy": {
    "max_model_calls": 2,
    "max_wall_ms": 600000,
    "allow_irreversible": false,
    "accept_confirm": false,
    "allowed_hosts": ["admin.example.com"],
    "observation": { "max_elements": 120, "include_screenshot": false },
    "semantic_verify": { "threshold": 0.7, "on_uncertain": "wait_human", "include_screenshot": false }
  },
  "steps": [ /* Step[]，mode=auto 时可为空，由模型规划 */ ]
}
```

`kind`：

| 值 | 含义 | 是否走步骤规划 |
|---|---|---|
| `workflow` | 分步业务任务 | 是（`steps` 为空且 `mode=auto` 时） |
| `interactive` | 持续交互，带 `controller` | 否 |

**空步骤的自动规划**（2026-10-28 实现）：`workflow` 且 `steps=[]` 只有在
`mode="auto"` 时才被接受。引擎在创建任务后、执行任何步骤前：

1. 打开 `start_url`（可选）；导航同样受 `allowed_hosts` 约束；
2. 用任务实际使用的同一个 Observer 观察页面，把状态文本交给 `Capability::Plan`；
3. 校验提案（1..50 步、非空 goal/actions、timeout 范围、id 唯一），补全空 id，
   再检查任务自己的边界：`navigate` 不得越出 `allowed_hosts`，`${cred:name}`
   必须对应调用方实际提供的凭据；
4. 把计划写回 `task.json`（保留占位符、不含凭据值），然后才执行第一步。

规划调用计入该任务的 `max_model_calls`；没有适配器、预算不足、提案非法、越界导航
或引用未提供的凭据，任务直接 `failed`，没有任何步骤被派发。计划落盘意味着 resume
继续原计划，而不是再问一次模型。事件依次为 `plan.requested`、`plan.created`
（失败时为 `plan.failed` 后接 `task.finished`）。计划中的步骤与作者手写步骤受同一套
审批/副作用/`allowed_hosts` 规则约束；模型不得通过规划获得超出规格的凭据。

任务状态：

```
pending | running | waiting_input | waiting_approval | paused
| succeeded | failed | canceled | crashed | recovering
```

**`0.1` 实际会出现的**：`pending`（建任务时写入）、`running`、`waiting_approval`、
`paused`（`task approve` 之后、`task resume` 之前：已批准但还没继续跑）、`succeeded`、`failed`、`canceled`、
`crashed`（启动时回收被打断的任务，见 [data-model.md §6](data-model.md)）。

**保留值，没有代码路径会写入**：`waiting_input`、`recovering`。列在这里是为了不让实现
各自发明名字；要用它们得先改这一节。
`waiting_approval` 在早期文档里叫过 `waiting_human`，线上只有下划线这个名字。

## 2. 步骤

```json
{
  "id": "step_03",
  "goal": "导出筛选后的订单",
  "executor": "auto",
  "preconditions": [
    { "type": "element_visible", "locator": { "role": "table", "name": "订单列表" } }
  ],
  "actions": [
    { "type": "click", "target": { "locator": { "role": "button", "name": "导出 CSV" } } }
  ],
  "success": { "type": "download_completed", "filename_regex": "orders.*\\.csv", "min_bytes": 1 },
  "fail_when": [
    { "type": "element_visible", "locator": { "css": "#list-error" } }
  ],
  "fallback_visual": "the red submit button",
  "timeout_ms": 30000,
  "retry": { "max_attempts": 2, "backoff_ms": 500, "on_exhausted": "escalate" },
  "side_effect": "reversible",
  "requires_approval": false
}
```

- `executor`: `auto | dom | vision | interactive`
- `retry.on_exhausted`: `reobserve | escalate | ask_model | abort`（`ask_model` 只对
  `read_only` 步骤生效，见下文「自动修复」）
- `side_effect`: `read_only | reversible | irreversible`
- `requires_approval` 为 true 或 `side_effect=irreversible` 且策略未允许时，执行前挂起为 `waiting_approval`。
- `fail_when`: 一组「尝试已经失败」的条件。只在等待 `success` 期间求值；任一为真即结束本次尝试
  并进入重试，而不是等满 `timeout_ms`。动作派发**前**为真的条件视为上一次尝试的残留，
  必须先变为假才重新生效（否则重试会在同一个错误横幅上立刻放弃）。
  不允许包含下载类条件；`success` 是嵌套的下载条件（`all/any`）时也不能用 `fail_when`——
  这两种情况会直接报错，而不是静默忽略。
- `fallback_visual`: 可选。仅当 `executor=auto` 且某个 `click`/`hover` 的定位器
  **完全找不到**（`LocatorNotFound`）时，用这段描述从截图重新定位并重试同一个动作。
  显式 `executor=dom` 不会改道；歧义（多个候选）与不可执行（禁用/无布局盒）也不改道——
  「页面变了」和「选择器写错了」都不该用「找个长得像的」来掩盖。改道后的结果仍由
  `success` 验证，并消耗一次模型预算。

## 3. 动作

统一外壳：`{ "type": "...", ... }`。所有带 `target` 的动作接受 `Locator`、`Point` 或
`Visual`（`{"visual":"the red button"}`，用自然语言描述截图里的目标）。

| type | 字段 |
|---|---|
| `click` | `target`, `button?`(`left/right/middle`), `click_count?`, `modifiers?`, `timeout_ms?` |
| `hover` | `target` |
| `type` | `target`, `text`, `clear_first?`, `delay_ms?`(默认 0) |
| `press` | `key`, `modifiers?` |
| `key_down` / `key_up` | `key`（用于持续按住；必须配对，取消时统一释放） |
| `scroll` | `delta:{x,y}` 或 `to_target`, `behavior?`(`instant/smooth`) |
| `drag` | `from`, `to`, `steps?`（默认 8；两端都接受 `Locator`/`Point`/`Visual`） |
| `select_option` | `target`, `values: string[]` |
| `set_files` | `target`, `paths: string[]` |
| `navigate` | `url`, `wait_until?`(`load/domcontentloaded/networkidle`) |
| `back` / `forward` / `reload` | — |
| `wait_for` | `condition: SuccessCondition`, `timeout_ms` |
| `evaluate` | `script`, `args?`, `world?`(`isolated/main`，默认 isolated) |
| `screenshot` | `region?`(`viewport/element/full_page`), `target?` |
| `extract` | `name`, `script`（脚本返回 JSON，写入任务产物 `artifacts/<step>-<name>.json`） |

```json
{ "type": "type", "target": { "locator": { "label": "订单号" } },
  "text": "20261028", "clear_first": true }
```

约束：

- 文本输入优先用 `type` 的真实键盘事件；超长文本可走 `evaluate` 设值，但**必须回读校验**。
- `evaluate` 仅在 `world=isolated` 时默认允许；`main` 需要任务策略显式开启。
- `extract` 不是动作而是取数：结果落盘成产物，否则「提取到了」无法在任务结束后被核对。
  按 JSON Schema 校验、`multiple` 选择器等留给 Phase 3；现在脚本自己保证形状，套件比对服务端真相。

### 视觉目标（已实现）

`{"visual": "<描述>"}` 让执行器把目标交给视觉模型：截取视口 PNG，模型返回
`bbox_norm:[x,y,w,h]`（相对图片归一化），执行器换算成 CSS 坐标后派发普通鼠标事件。
只有 `click`、`hover`、`type`、`drag` 支持视觉目标；`drag` 的两端各算一次定位。
`wait_for` / 条件里的 locator 仍走 DOM——在状态判断上使用视觉相似度会把「看起来对了」
当成「成功了」。

几何约束（不满足即报错，不猜）：

- 截图必须是视口，尺寸与 `css viewport × devicePixelRatio` 相差不超过 2%；
  全页截图或缩放不一致时拒绝换算。
- bbox 必须落在 `0..1` 内、宽高为正，且换算后至少 1×1 CSS px。
- 归一化坐标让 DPR 与浏览器缩放自然抵消；视口截图让 scroll 偏移为 0，因此不做二次加偏移。

### 策略字段的实现语义（v0.1 已落地）

| 字段 | 语义 |
|---|---|
| `accept_confirm` | 默认 `false`：`confirm()` / `prompt()` 一律**拒绝**（返回 false），因为没有人类在场。置 `true` 后自动接受，任务必须明确写出来——`confirm("确定删除全部数据？")` 不能被静默点过。`alert()` 与 `beforeunload` 不携带选择，始终接受。 |
| `allowed_hosts` | 所有 `navigate` 动作在派发前检查；越界即整任务失败，不做「尝试一下」。 |
| `max_wall_ms` | 任务级墙钟预算；步骤超时取 `min(step.timeout_ms, 剩余预算)`。 |
| `semantic_verify` | 可选。全部步骤的结构化成功条件通过后，观察最终页面并让模型判断任务目标（可选附一张视口截图）；占一次模型预算。见下方。 |
| `arbitrate_ambiguous` | 可选，默认不启用。定位器命中多个元素时默认**直接失败**；启用后可让模型在候选中挑一个，但只有置信度达标才算数。见下方。 |

**成功条件的求值语义**：成功条件是「会变为真」的谓词，不是一次性采样。执行器以
25→100 ms 退避轮询到 `step.timeout_ms`；下载类条件阻塞等待下载事件本身，但步骤声明了
`fail_when` 时改为轮询下载守卫，好让页面的失败信号能提前结束尝试。
一次性采样会让每个异步页面都变成随机失败——这是 Phase 2 实测踩到的坑。

**语义校验（`policy.semantic_verify`，2026-10-28 实现）**：机械条件只证明「某件事发生了」，
不证明结果正确（例如计数行渲染出来了，但 N 不是想要的 N）。声明后：

```json
"semantic_verify": {
  "goal": "订单列表只剩失败订单，且导出文件已生成",
  "threshold": 0.7,
  "on_uncertain": "wait_human",
  "include_screenshot": false
}
```

| 字段 | 默认 | 语义 |
|---|---|---|
| `goal` | 任务 `goal` | 校验时问模型的问题；空字符串视同未提供 |
| `threshold` | `0.6` | 置信度低于阈值就是 `uncertain`，不四舍五入 |
| `on_uncertain` | `wait_human` | `wait_human` / `fail` / `continue`，见下 |
| `include_screenshot` | `false` | 观察之后截一张视口 PNG 随请求发出；图片同时存为 `artifacts/verify-final.png` |

执行时机是**所有步骤成功之后、任务完成之前**，只对 `kind=workflow` 生效（交互任务的
结束条件是控制器目标，不用模型再判一次）。它复用任务的**同一个** `ModelRouter`，
因此与视觉调用共享 `max_model_calls`，并计入 `tasks.model_calls`。

结果三态：

| 判定 | 任务行为 |
|---|---|
| `achieved` | 任务 `succeeded`，`result.semantic` 记录判定、置信度、阈值、模型、理由 |
| `not_achieved` | 任务 `failed`，已完成的步骤仍如实记为 `verified=true`；不自动重跑（副作用可能已发生） |
| `uncertain` | 按 `on_uncertain`：`wait_human` 请求任务级审批（`payload.next_index = steps.len()`，`payload.semantic` 带判定）；`fail` 直接失败；`continue` 明确接受未确认的结果，仍如实记录 `uncertain` |

`wait_human` 被批准后，`resume` 直接以该判定完成，不重跑步骤、不再调用模型；被拒绝则任务失败。
每次实际校验写一条 `task.verified` 事件，页面观察或模型调用失败写 `task.verify_error`。
开启 `include_screenshot` 时先写一条 `task.verify.screenshot`（name / bytes / width / height /
sha256 / path），`result.semantic.screenshot` 带同样的字段；存盘的是**发给模型的那份字节**，
测试用 sha256 证明二者一致。截图是页面当时的样子，开启这个开关等于把屏幕内容交给校验
模型；支持图片的适配器会同时看到文字状态与图片，纯文本适配器（Laya）明确拒绝而不是
当作没看到——那次尝试仍按「按尝试计」扣一次预算。
**配置了校验但模型不可用/预算不足时任务失败**，不会把「没问模型」当作通过；阈值不在
`0..1` 内时在启动浏览器前拒绝。CLI 会在结果摘要后多打印一行 `semantic:`。

**例**：A5 里导出接口第一次返回 500，页面显示 `#list-error`，步骤声明了对应的 `fail_when`。
没有它时，失败尝试要等满 8 s 的 `step.timeout_ms`；有了它，任务从 9.3 s 降到 1.35 s。

**自动修复（`retry.on_exhausted=ask_model`，2026-10-28 实现）**：常规尝试耗尽后，
只有 `side_effect=read_only` 且不需要审批的步骤才允许把失败交给模型；其余步骤即使声明
了 `ask_model` 也直接失败——失败的动作可能已经产生副作用，重新派发模型改写的版本正是
审批门要防的重复提交。流程是：观察当前页 → **一次**文本模型请求（失败步骤、错误、当前
页面）→ 必须返回**恰好一个**替换步骤 → 通过下面的安全包络后**只派发一次**，不开启新的
重试循环。

替换步骤必须满足：`id` 不变；`side_effect` 不得升高；原来的审批要求不得删除；
`success`、`preconditions`、`fail_when` 必须与原文**逐字一致**（`Js` 条件在页面主世界
执行，改写它们等于给模型一个新的代码通道）；`executor` 与 `fallback_visual` 不得更改
（执行方式和视觉改道由步骤作者决定）；不得新增 `evaluate` / `extract` / 含 JS 的
`wait_for` 动作——只能原样复用作者已经写下的同一条动作；所有 `navigate` 仍受
`allowed_hosts` 约束；不得引入 `${cred:...}` 占位符。提案本身还要过与规划相同的结构
校验（非空 actions、timeout 范围、执行器不得为 `interactive` 等）。

```json
"retry": { "max_attempts": 1, "backoff_ms": 100, "on_exhausted": "ask_model" }
```

执行成功或失败都写 `result.steps[].repair`（`model`、`confidence`、`reason`、`step`、
`status`，失败时带 `error`）和对应的 `step.finished` 事件；`attempts` 把这次替换计入
（原始尝试 + 修复 = 2）。**被拒绝的提案从不派发**，步骤以原错误加
`repair failed: …` 结束，后续步骤不运行。修复调用与视觉、语义校验共享
`max_model_calls`；没有适配器或预算不足按失败处理，不静默跳过。CLI 的步骤摘要会给
模型改写的步骤加 `[repaired: succeeded|failed]`。

**歧义定位仲裁（`policy.arbitrate_ambiguous`，2026-10-29 实现）**：`{role:"button", name:"导出"}`
命中两个按钮时，默认行为是**失败并报出全部候选**——这不是保守，是正确：任务作者写了个页面
无法区分的定位器，猜一个就是把错误的按钮按下去。可选地让模型来选：

```json
"arbitrate_ambiguous": { "min_confidence": 0.7, "max_candidates": 8 }
```

| 字段 | 默认 | 语义 |
|---|---|---|
| `min_confidence` | `0.7` | 低于此值就放弃仲裁，原歧义错误照旧。比 `semantic_verify` 的 0.6 高是有意的：这里猜错会**操作错元素**，不只是判错结论 |
| `max_candidates` | `8` | 超过就不问模型（500 个匹配是定位器写错了，不是一道选择题），且不消耗预算 |

规则（每条都有 `testkit/arbiter-smoke.py` 对应断言）：

- **不启用就不问模型**：`calls == 0`，行为与今天完全一致。
- **候选来自解析，模型不能增删**：以 `c1..cN` 提供，顺序即解析顺序；答一个没提供过的 id
  会被路由拒掉，不会派发任何东西。
- **模型看到的是同一次观察**：解析与仲裁共用一个快照，页面状态文本（`Controls:`）与候选
  来自同一时刻，不会问完页面又变了。
- **描述完全相同的候选直接放弃，且不调模型**。这一点是测出来的：两个同名 `导出` 按钮，
  本地判定模型给出了 0.67 与 0.83 的"自信"答案，且交换提供顺序后仍锁定同一个元素——
  它读的是 id，不是页面。任何置信度阈值都拦不住这种情况，所以正确做法是放弃并说明
  该怎么改（用 css / test_id / 限定范围）。
- **每次仲裁都留在事件里**：`action.executed.arbitration` 带 `strategy` / `eid` / `model` /
  `confidence` / `candidates` / `reason`。没有它，一次猜出来的点击和一次点名指定的点击
  在 trace 里长得一模一样。
- **放弃时说明原因**：低于阈值、候选过多、无法区分、模型不可用，四种情况的修复动作不同，
  错误里直接写清是哪种。
- 仲裁占用任务的 `max_model_calls`，与规划、视觉、修复、语义校验同一预算；预算不足即失败。

### 本地判定模型实测（2026-10-29）

同一个两候选问题（`导出 CSV` vs `导出 PDF`，目标"导出为 CSV"），`sidecars/laya/decide.py`
（torch/MPS）：

| 场景 | 结果 |
|---|---|
| 标签可区分（上述） | 选 **c1=PDF**，置信度 **0.625** —— 错的；`min_confidence=0.7` 时放弃，任务不派发 |
| 标签换成三个候选（含无关按钮） | 选 CSV 0.78、无关项排最后 0.09 —— 说明它确实读标签 |
| 两个同名按钮，交换提供顺序 | 仍锁定同一元素（0.67 / 0.83）—— 读的是 id，不是页面 |

结论与 `docs/benchmarks.md` 一致：本地判定模型能读文本描述，但在中文后台这种
**描述几乎相同**的场景不可靠。阈值不是装饰——实测的 0.625 正好落在 0.7 之下。

## 4. 定位器 Locator

```json
{
  "role": "button",
  "name": "导出 CSV",
  "exact": false,
  "within": { "role": "region", "name": "订单列表" }
}
```

支持字段（按推荐优先级）：`role`+`name` > `label` > `test_id` > `placeholder` > `alt` > `title`
> `text` > `css` > `xpath` > `visual` > `point`。

组合式：

```json
{ "any": [ {"role":"button","name":"导出"}, {"test_id":"export-btn"} ] }
```

```json
{ "visual": { "description": "表格右上角的导出按钮", "near": "订单列表" } }
```

解析结果：

```json
{
  "matched": 1,
  "element": { "eid": "e7", "observation_id": "obs_21", "box": {"x":1024,"y":88,"w":96,"h":32} },
  "strategy_used": "role+name",
  "confidence": 0.97
}
```

多命中不是错误地「取第一个」，而是返回 `matched: N` + 候选列表，由上层决定缩窄或升级。

## 5. 成功条件 SuccessCondition

```json
{ "type": "url_matches", "pattern": "**/orders*" }
{ "type": "element_visible", "locator": {...} }
{ "type": "element_text_matches", "locator": {...}, "regex": "共 \\d+ 条" }
{ "type": "element_count", "locator": {...}, "op": "gt", "value": 0 }
{ "type": "download_completed", "filename_regex": ".*\\.csv", "min_bytes": 1 }
{ "type": "network_idle", "quiet_ms": 500 }
{ "type": "js", "script": "() => document.querySelectorAll('tbody tr').length > 0" }
{ "type": "screenshot_changed", "region": {...}, "min_change_ratio": 0.01 }
{ "type": "all", "of": [ ... ] }
{ "type": "any", "of": [ ... ] }
{ "type": "not", "of": { ... } }
```

评估在观察刷新后立即进行，不轮询式反复截图。`js` 谓词在 isolated world 执行，有超时。

## 6. 观察

```json
{
  "id": "obs_21",
  "seq": 21,
  "state_format_version": 1,
  "url": "https://admin.example.com/orders?status=failed&date=2026-10-28",
  "title": "订单管理",
  "frame_id": "main",
  "viewport": { "w": 1280, "h": 800, "dpr": 2 },
  "elements": [
    { "eid":"e1", "role":"button", "name":"导出 CSV",
      "box":{"x":1024,"y":88,"w":96,"h":32},
      "states":["enabled","visible"], "hash":"a1b2c3" }
  ],
  "text_blocks": [ { "eid":"e9", "text":"共 37 条", "box":{...} } ],
  "dialogs": [],
  "truncated": false
}
```

差分（动作后）：

```json
{ "type": "observation.diff", "base":"obs_21", "current":"obs_22",
  "added":[...], "removed":["e4"], "changed":[{ "eid":"e9", "field":"text",
  "from":"共 37 条", "to":"共 12 条" }], "url_changed": false }
```

## 7. 事件

所有事件共享：

```json
{ "seq": 42, "ts": "2026-10-28T10:41:00.123Z", "task_id": "01J8Z...", "type": "step.finished" }
```

| type | 触发 |
|---|---|
| `task.started` / `task.finished` | 生命周期 |
| `step.started` / `step.finished` | 步骤 |
| `action.executed` | 单个动作完成：`step_id` / `index` / `type` / `ok` / `elapsed_ms` / `escalated` / `arbitration`（模型代选的痕迹，见第 3 节）/ `error`；不记动作载荷。在步骤返回后按步骤成组写入，序号与耗时保留真实顺序 |
| `action.failed` | 动作失败（含候选元素、页面快照引用） |
| `escalation.requested` | 需要模型重新决策 |
| `approval.requested` / `human.required` | 挂起等人 |
| `download.completed` | 产物落地 |
| `controller.tick_summary` | 每秒聚合一次，不是每 tick 一条 |
| `interactive.started` / `interactive.stopped` | 控制器开始/结束（含 why、ticks、actions、evals、tick 间隔 p50/p95/max、`rules[]` 逐条命中次数） |
| `checkpoint.saved` | 检查点落盘 |
| `approval.decided` | 人工批准/拒绝 |
| `task.resumed` | 从检查点继续 |

## 8. 模型调用契约

请求：

```json
{
  "capability": "locate",
  "observation_id": "obs_21",
  "state": "Task: 导出筛选后的订单\n...",
  "instructions": "Which control should be clicked?",
  "criteria": { "e7": "button \"导出 CSV\"", "e12": "button \"重置\"" },
  "budget": { "timeout_ms": 4000 }
}
```

响应（提案，非动作）：

```json
{ "choice": "e7", "confidence": 0.82, "reason": "matches goal", "model": "laya" }
```

视觉定位响应：

```json
{ "capability": "visual_locate", "screenshot_id": "shot_31",
  "target": { "bbox_norm": [0.82, 0.90, 0.10, 0.04], "description": "蓝色提交按钮" },
  "confidence": 0.71 }
```

Harness 侧必做校验：

1. schema 合法；
2. `choice` 在候选集合内（不得凭空编 id）；
3. 坐标在视口内；
4. 不违反副作用策略；
5. 未超预算。

任一步失败即丢弃提案，按 `on_exhausted` 处理。

**已实现（Phase 3，`bh-model`）**：

- 适配器是 OpenAI 兼容的 `POST {BH_VLM_URL}`，`messages[1].content` 同时携带文本与
  `image_url` 的 `data:image/png;base64,...`；`content` 解析为 JSON 对象。
- 视觉提案字段为 `{"bbox_norm":[x,y,w,h],"confidence":0..1,"reason":"..."}`；
  文本定位为 `{"choice":"<候选 id>","confidence":...}`。
- 路由器在调用**前**扣任务级预算（失败/超时也计数）；无可用适配器时不扣预算、直接报错。
- 校验：`confidence` 有限且在 `0..1`；`bbox_norm` 有限、原点非负、宽高为正、
  右/下边界不超过 1（允许 1e-6 的浮点误差）；`choice` 必须等于某个候选 id。
- 配置：`BH_VLM_URL`、`BH_VLM_MODEL`、`BH_VLM_KEY`、`BH_VLM_TIMEOUT_MS`（默认 8000）。
  不配置时视觉目标会得到明确的「no model adapter configured」，而不是静默降级。
- 模型层也接受 `locate`（从候选 id 里选一个）。走这条路径的是**歧义定位仲裁**
  （`bh-exec::arbiter::ModelArbiter`，按 `policy.arbitrate_ambiguous` 才启用，见 §4 与
  [roadmap.md](roadmap.md)）：候选可区分且过阈值时才问模型。默认路径仍是本地解析，
  不配模型也不会改变定位行为；这是执行器的事，不是模型契约的事。

**规划（Phase 4，2026-10-28）**。同一 OpenAI 兼容线格式，只是没有图片、回复更长：

```json
{
  "capability": "plan",
  "state": "URL: http://127.0.0.1:8766/\nTitle: 运营后台\nControls:\ne1 textbox \"用户名\"\n...",
  "instructions": "登录后台，筛选失败订单，导出 CSV",
  "criteria": [],
  "budget": { "timeout_ms": 30000 }
}
```

```json
{
  "steps": [
    { "id": "login", "goal": "登录运营后台",
      "actions": [
        { "type": "navigate", "url": "http://127.0.0.1:8766/" },
        { "type": "type", "target": { "role": "textbox", "name": "用户名" },
          "text": "${cred:username}", "clear_first": true }
      ],
      "success": { "type": "element_visible",
                   "locator": { "role": "button", "name": "导出 CSV" } },
      "timeout_ms": 20000 }
  ],
  "confidence": 0.8,
  "reason": "testkit admin workflow"
}
```

步骤体就是普通的 `Step`（见第 4 节），因此规划结果可以直接落成 `TaskSpec`。
Harness 侧校验：`steps` 存在且为 1..50 步；每步 `goal` 非空、`actions` 非空、
`timeout_ms` 在 1..600000；非空 `id` 唯一；`confidence` 仍在 0..1。任何一条不满足
就丢弃整个提案，不做局部修补。

**凭据**：规划模型永远看不到密码，提案里只能出现 `${cred:NAME}` 占位符；
运行时的值由 `--cred NAME=VALUE` 提供，只在内存里，写盘的 spec 和引擎的 `task.json`
都只保留占位符（见 [interfaces.md](interfaces.md) 不变量 15）。

**入口与配置**：`bhx plan --goal TEXT [--url URL] [--out FILE] [--run] [--cred NAME=VALUE]`。
文本适配器读 `BH_LLM_URL`（未设置时回落到 `BH_VLM_URL`），模型名 `BH_LLM_MODEL`
（回落到 `BH_VLM_MODEL`），键与超时同理（`BH_LLM_KEY`、`BH_LLM_TIMEOUT_MS`，默认 30000）。
`--run` 把校验后的 spec 直接交给任务引擎；规划那次调用不计入任务的
`max_model_calls`，任务自身的 `model_calls` 仍是 0（规划已固化成步骤）。

**校验（Phase 4，2026-10-28）**。同一文本线格式，候选固定为两个：

```json
{
  "capability": "verify",
  "state": "URL: http://127.0.0.1:8766/\nTitle: 运营后台 · 订单\nControls:\ne1 textbox \"搜索订单号\"\ne2 button \"导出 CSV\"\n...",
  "instructions": "已登录后台并看到订单列表",
  "criteria": {
    "achieved": "the page state shows the goal completed",
    "not_achieved": "the page state does not show the goal, or shows an error"
  },
  "budget": { "timeout_ms": 15000 }
}
```

```json
{ "choice": "achieved", "confidence": 0.9,
  "reason": "the export control is visible after login", "model": "llm" }
```

Harness 侧：`choice` 必须是两个候选之一、`confidence` 0..1；调用方给阈值
（默认 0.6），低于阈值时报告 `uncertain`，**不四舍五入到任何一边**。
MCP 工具 `browser_verify {goal, threshold?}` 返回三行式文本，第一行就是判定：

```
achieved
confidence: 0.90 (threshold 0.60)
model: llm
reason: ...
```

它只是第二意见：不点击、不改任务状态；机械条件仍用 `browser_check_condition`。
任务内的同一条判定以 `policy.semantic_verify` 接线（见第 3 节），由任务引擎决定
`uncertain` 是等待人审还是失败；MCP 工具本身仍不替调用方做这个决定。

**候选上限**：`criteria` 由调用方预过滤到 ≤10 项再发。实测超过 30 项时顶层概率降到 0.16–0.24，
而延迟并不会更高（平台期），所以「多发几个候选碰运气」只会降低准确率。详见 [benchmarks.md](benchmarks.md)。

**顺序敏感**：`criteria` 的插入顺序就是优先级，超过上限时从尾部截断。
各语言实现都必须用有序映射，不能依赖普通哈希表的遍历顺序。

**本地决策 sidecar（Phase 4，2026-10-28，`bh-model`）**。Laya 这类编码器不是聊天模型：
它只回答「从这些选项里选一个」，不做生成。所以它只接 `locate` 与 `verify`，`plan` /
`repair` / `visual_locate` 直接拒绝，而不是把选择硬解释成步骤或坐标。

协议是一行 JSON 请求、一行 JSON 回复，先握手再收请求：

```json
// 进程启动后第一行
{ "ready": true, "backend": "torch/mps" }
// 请求（state 就是观察文本，含 `Task: <goal>`；criteria 的顺序即优先级）
{ "state": "Task: 已登录并看到订单列表\nURL: ...\nControls:\n...",
  "instructions": "Does the page state show the task's goal achieved? ...",
  "criteria": { "achieved": "...", "not_achieved": "..." } }
// 回复
{ "choice": "achieved", "probs": {"achieved": 0.67, "not_achieved": 0.33},
  "conf": 0.67, "top": [["achieved", 0.67]], "ms": 60,
  "backend": "torch/mps", "n_criteria": 2 }
```

适配器负责生命周期：懒启动、等握手（`BH_LAYA_STARTUP_TIMEOUT_MS`，默认 20 s）、
常驻复用、超时或管道断裂时 kill 并由下一次请求重启。同一配置的适配器在进程内共享
一个 sidecar，所以「每次 `browser_verify` 新建 router」不会重付 5 s 模型加载。

- 配置：`BH_LAYA_DECIDE`（`decide.py` 路径，不设即不启用）、`BH_LAYA_PYTHON`
  （默认 `python3`）、`BH_LAYA_MAX_CRITERIA`（默认 40，超过直接报错要求预筛）。
- 顺序：默认远程适配器优先，sidecar 兜底；`BH_LAYA_PRIORITY=first` 反过来。
  依据见 [benchmarks.md](benchmarks.md)：零样本 Laya 在本仓库的中文后台页上
  两次关键判定都错，不能默认让它压过更强的模型。
- `conf` 缺失即错误，不补 0.0——0.0 会在阈值语义里被读成 `uncertain`，把
  「模型没回答」伪装成「模型不确定」。

## 9. 持续交互策略 Controller Policy

```json
{
  "kind": "interactive",
  "goal": "走到右边黄色区域",
  "controller": {
    "policy_version": 3,
    "tick_hz": 30,
    "start_url": "http://127.0.0.1:8766/game/",
    "observe": {
      "source": "js",
      "script": "() => window.__gameState()",
      "max_hz": 10
    },
    "decide": {
      "question": "先挡哪一条航线？",
      "options": [
        { "id": "left",  "description": "守住左侧航线（x=120）" },
        { "id": "right", "description": "守住右侧航线（x=360）" }
      ],
      "every_ms": 1000,
      "min_confidence": 0.5
    },
    "goal": { "script": "(s) => s.x >= 120", "deadline_ms": 15000 },
    "rules": [
      { "id": "r1", "when": "s.x < 120", "do": [{ "type":"key_down", "key":"ArrowRight" }], "priority": 10 },
      { "id": "r2", "when": "s.x >= 120", "do": [{ "type":"key_up", "key":"ArrowRight" }], "priority": 20, "terminal": true }
    ],
    "safety": {
      "max_actions_per_sec": 60,
      "max_duration_ms": 30000,
      "release_all_on_exit": true
    }
  }
}
```

`controller` 在 `kind=interactive` 时必填，`steps` 为空。已实现语义（Phase 3 C 组）：

- **`start_url`** 是必填项：控制器要有一个可驱动页面。导航前同样受 `allowed_hosts` 约束。
- **采样与求值分频。** 页面状态按 `observe.max_hz` 采样；规则每个 tick 用**上一次采样**求值。
  30 Hz 的循环不会变成每秒 30 次 JS 往返。
- **`when` 的写法**是作用于 `s` 的 JS 谓词（`s.x < 120`），也接受 `(s) => ...`；
  `observe.script` 接受 `() => (...)` 或裸表达式。两者都在页面里求值。
- **动作在规则「进入」时执行一次**，不是每个 tick 重复：按住一个键是 1 次 `key_down`，
  不是每秒 30 次。规则不再匹配后再次进入，会重新执行。
- **优先级高的先匹配，只执行第一条命中的规则。** `terminal: true` 在该规则动作后停止循环。
- **表达式出错**：跳过该条（或该 tick 的采样），连续 5 次出错判定 `anomaly` 并失败——
  静默忽略会让一个坏策略看起来像「游戏太难」。
- **速率限制是滑动 1 秒窗口**，超出时**等待**而不是丢动作：丢掉一个 `key_up` 就是粘键。
- **tick 落后不补发。** 落后时直接进入下一次 tick，不为了追进度突发一串输入。
- **退出路径一律释放输入**（目标达成、超时、取消、异常、错误），不依赖
  `release_all_on_exit` 的布尔值——它保留在协议里是为了向前兼容，但把它设成 false
  不会让控制器留下按住的键。释放后留 350 ms 才返回：页面通常按定时器记录输入状态，
  否则最后一条「已释放」会随浏览器进程一起消失。
- **结束原因决定任务状态**：`goal_met` → `succeeded`；`terminal_rule` + 无 goal 脚本 →
  `succeeded`；`deadline` / `anomaly` → `failed`；`canceled` → `canceled`。
- **模型只被允许选策略，不被允许进 tick**（`controller.decide`，2026-10-29）。见下方单独一节。
- **interactive 任务不可 resume。** 它没有步骤检查点；把输入流从头重放不是恢复，
  是第二次执行。`task resume` 会直接报错。

### 模型选策略（`controller.decide`）

规则能表达"怎么做"，表达不了"先看哪个"。当页面自己不知道该怎么办时（两个编队同时逼近、
一个表单有三条可选路径），需要一个模型来选——但**绝不能每 tick 都问**：实测一次本地判定
150–500 ms，而 30 Hz 的 tick 只有 33 ms。所以这里的分工是固定的：

```
模型（每 every_ms 一次，最多一个未完成请求） ──> s.d ──> 规则（每 tick）──> 输入
```

| 字段 | 默认 | 语义 |
|---|---|---|
| `question` | — | 问模型的问题；必填 |
| `options` | — | **作者写好的策略**，≥2 项，`{id, description}`。`id` 限 `[A-Za-z0-9_-]+`，因为它会出现在 `s.d` 里被规则比较 |
| `every_ms` | `1000` | 询问间隔。低于 250 ms 会被抬到 250：任何实测模型都做不到更频繁，写了也只是永远用上一次的答案 |
| `min_confidence` | `0.5` | 低于此值**保持上一次策略不变**，不猜测 |

契约：

- **选项由作者枚举**。模型只在已经写了规则的策略里选，答不出规则之外的行为——这是
  "模型介入"与"模型失控"的分界。
- **决策写进状态**（`s.d`、`s.d_confidence`），规则用普通 `when` 分支，不新增谓词语言，
  也不存在"模型的看法"与"页面的事实"两套状态打架。
- **问在循环外**：单独的后台任务采样并询问，循环只取最新已到达的答案。慢一次是反应迟钝，
  停下来才是问题；一个请求未完成时不会再发第二个。
- **采纳的策略版本必须与当前运行的一致**：策略热更新换掉了选项列表，之前那个答案作废。
- **没有适配器时不失败**：记录一次 `decision model call failed`，循环照跑，`decisions: 0`，
  没有规则命中策略分支——任务不会因为我没配模型就停下，也不会假装做了什么决定。
- 每次接受的决策都计入 `max_model_calls` 与 `interactive.stopped.decisions`。

**实测（2026-10-29）**：`testkit/sites/plane/` 两条航线速度不同，"先守哪条"有可测量的收益。
同一套规则，只把模型的答案换一边：

| 模型选 | 结果（读服务端遥测，不读自述） |
|---|---|
| 慢航线（left） | 得分 **3**，两编队全灭 |
| 快航线（right） | 得分 **0**，两编队飞走 |

真实 Laya（torch/MPS）在同一场景被问 2 次、中途改过策略、得分 3。静态探针里它偏好排在前面的
选项（换序后答案不变），但**在真实游戏状态前它会用事实**：把两个选项对调后它换到了另一条航线。
`testkit/decide-smoke.py` 29 项钉住上面每条契约。

**策略热更新（Phase 4）。** 交互任务启动后，`interactive_update_policy` 可以把新策略
送进正在运行的循环：

- 只有 `policy_version` **严格更大**的策略会被采纳；相同或更小的版本是迟到更新，拒绝。
- 采纳发生在 **tick 边界**：当前 tick 的动作仍属于进入该 tick 时的策略，下一 tick 起
  新策略生效。频道的语义是「最新覆盖」——连续两次更新之间没被采纳的中间版本不会执行。
- `tick_hz`、`observe.max_hz`、速率上限、规则集都随新策略重新计算；`start_url` 不重导航。
- **截止时间只能收紧，不能延长**：新策略的 `goal.deadline_ms` / `safety.max_duration_ms`
  只会把结束时间提前。允许热更新延长 deadline 等于让 `max_duration_ms` 变成建议值。
- 规则按「id + 规则内容」判重：更新后仍匹配、内容没变的规则不会重复执行进入动作，
  内容变了或换了规则则按新一次进入处理。
- 停止原因与结束原因不变；`interactive.stopped` 与每秒的 `controller.tick_summary`
  都带上实际运行到的 `policy_version`，调用方可确认最后一次更新是否被采纳。
- 目标与所有规则谓词在**一次** `Runtime.evaluate` 里求值：`interactive.stopped.evals`
  与 tick 数同量级，不随规则条数增长。某个谓词抛错只影响它自己，仍按「连续异常」
  计数；传输失败只计一次。

## 10. 对外接口

### MCP 工具（任务级优先）

```
browser.create_session   browser.close_session
browser.navigate         browser.observe
browser.execute_actions  browser.screenshot     browser.evaluate
task.start               task.get_status        task.get_result
task.cancel              task.resume            task.approve
artifact.list            artifact.get_metadata  artifact.fetch
interactive.start        interactive.update_policy  interactive.stop
```

`browser.execute_actions` 是低级原语，用于单步调试；长流程用 `task.start`。

**实装名用下划线**（24 个，与 `tools/list` 一致）：`browser_create_session`、`browser_navigate`、`browser_observe`、`browser_execute_actions`、
`browser_check_condition`、`browser_verify`、`browser_evaluate`、`browser_screenshot`、
`browser_close`、`browser_run_task`、`task_run`、`task_status`、`task_list`、
`task_artifacts`、`artifact_list`、`artifact_metadata`、`artifact_fetch`、`task_resume`、
`task_approve`、`task_cancel`、`interactive_start`、`interactive_status`、
`interactive_update_policy`、`interactive_stop`。

`interactive_*` 与 `task_run` 的区别是**不等结果**：`interactive_start` 立刻返回
`task_id`，控制器在后台跑；`interactive_status` 返回运行状态、当前策略版本和最近事件；
`interactive_update_policy` 把新策略送进循环（版本必须更大，见上）；
`interactive_stop` 置取消标志、等最多 2.5 s 拿到最终状态。每个交互任务有自己的
engine 连接和浏览器，不和 `task_run` 抢同一把锁；同一 home 的 SQLite 用 WAL +
5 s busy timeout 协调。
`artifact_*` 是任务级的：按 `task_id` 查存储里的产物行，路径必须 canonicalize 到该任务的
artifacts 目录内；`artifact_fetch` 对 UTF-8 内容（CSV/JSON/TXT）返回原文，其余返回
base64，并受 `max_bytes`（默认 5 MiB）限制，避免一个大文件灌满上下文。

`browser_evaluate` 默认在 **isolated world** 执行：能看到 DOM，看不到页面自定义的
`window.*`。需要页面全局时显式传 `"world": "main"`。任务里的 `Condition::Js` 与
控制器脚本仍在 main world 执行——它们是任务规格的一部分，不是调用方临时拼的代码。

### REST（已实现，`bhx serve`）

```
GET    /v1/health
POST   /v1/tasks                       # 202 + {task_id}；任务在后台跑
GET    /v1/tasks?limit=N
GET    /v1/tasks/{id}                  # task + steps + approvals + artifacts
POST   /v1/tasks/{id}/resume           # 202
POST   /v1/tasks/{id}/approve
POST   /v1/tasks/{id}/reject
POST   /v1/tasks/{id}/cancel
GET    /v1/tasks/{id}/events?after=N
GET    /v1/tasks/{id}/artifacts
GET    /v1/tasks/{id}/artifacts/{name|id}

POST   /v1/sessions                    # 201 + {session_id, url, allowed_hosts}
GET    /v1/sessions
GET    /v1/sessions/{id}               # url + title + allowed_hosts
DELETE /v1/sessions/{id}
POST   /v1/sessions/{id}/navigate      # {url} -> {url, title}
POST   /v1/sessions/{id}/observe       # {goal?, max_elements?, screenshot?}
POST   /v1/sessions/{id}/actions       # {actions:[…], observe_after?}
POST   /v1/sessions/{id}/condition     # {condition, timeout_ms?} -> {holds}
POST   /v1/sessions/{id}/verify        # {goal, threshold?}
POST   /v1/sessions/{id}/evaluate      # {script, world?} -> {result}
GET    /v1/sessions/{id}/screenshot    # image/png
```

`POST /v1/tasks` 的 body 就是 `protocol.md` 第 1 节的任务 JSON。带 `id` 时可作幂等键：
已存在返回 409，不会覆盖。`workflow` 空步骤只在 `mode=auto` 时接受；非 auto 返回 422，
不会「接受后永远 running」。auto 任务仍可能在后台因无规划模型或坏提案而失败，客户端
必须用状态接口确认结果（`planning_unavailable` 写在任务 error 里）。任务的创建、
浏览器启动、执行都在后台；202 只表示已受理，状态以 `GET /v1/tasks/{id}` 为准。

产物下载只服务 `tasks/{id}/artifacts` 下的文件：存储路径 canonicalize 后必须仍在
该目录内。产物名可能含中文，URL 字段已百分号编码。

**会话级接口**是 MCP `browser_*` 的 HTTP 镜像，供说不了 stdio 的 Agent 用。HTTP 无状态，
所以页面要有名字：`POST /v1/sessions` 返回 `session_id`，后续每个请求都带上它。每个会话
有自己的浏览器与下载目录（`<home>/sessions/<id>/downloads`），两个会话可以各自导航，
关掉一个不影响另一个。`allowed_hosts` 在创建时给，空表示不限制；`navigate` 与
`actions` 里出现的 `navigate` 都在**第一个动作之前**检查，越界返回 403，页面不动、批次
一个动作也不跑。`actions` 在某一步失败时返回 422 与带索引的日志（`[0] failed: …`）——
这是「批次停在这里」，不是「什么都没发生」，失败前的动作已经生效。`condition` 的
`timeout_ms=0` 表示采样一次；超时是 `{"holds":false}`，不是错误。`evaluate` 默认
isolated world，`"world":"main"` 显式开，与 MCP 一致。`observe` 附带的截图是 JPEG
（按模型上下文体积取舍），`GET …/screenshot` 返回 PNG（给要做像素测量的调用方）；
两者都带媒体类型。`verify` 没有配置模型适配器时返回 503，坏参数返回 422。
单个会话的请求串行执行；取消不作用于单个 HTTP 请求，长工作流请用 `POST /v1/tasks`。

### WebSocket（已实现）

```
GET /v1/tasks/{id}/events/ws?after={seq}
```

每条消息是一行 JSON 事件：`{"seq":12,"ts":"...","type":"step.succeeded","data":{...}}`，
与 `events.jsonl` 和 `GET /events` 同构。`after` 就是断线续传：重连时带上最后收到的
`seq`，中间不会漏也不会重。

流是**存储的尾随读**，不是进程内广播：服务重启后客户端依然能续上。任务进入终态且
`task.finished` 已送达后，服务端发 `{"type":"stream.closed"}` 再关连接。没有设计稿里的
`hello` 首帧——`after` 参数已经表达了同一件事，多一帧就多一个要对齐的状态。

尚未实现：全局事件流 `GET /v1/events`。

## 11. 错误

```json
{
  "error": {
    "code": "locator_ambiguous",
    "message": "12 candidates matched role=button name=导出",
    "retryable": false,
    "context": { "observation_id": "obs_21", "candidates": ["e7","e12"] }
  }
}
```

错误码分层：

- `browser.*`：连接、崩溃、导航失败
- `locator.*`：未命中、歧义、不可见、被遮挡
- `action.*`：分发失败、超时
- `task.*`：预算超限、审批拒绝、取消
- `model.*`：超时、非法输出
- `policy.*`：策略越界
