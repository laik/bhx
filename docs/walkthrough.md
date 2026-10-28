# 端到端走查：一次真实工作任务

设计是否闭合，看一个任务能不能从入口一路走到底。这里用 A 组验收的基准任务走一遍，
标出每一层的输入输出、模型在哪几处被调用、失败时谁负责什么。

> 耗时数字是示意量级（来自 [benchmarks.md](benchmarks.md) 的实测），不是本次运行的记录。

**任务**：登录后台，筛选今天的失败订单，导出 CSV。

## 0. 调用方

```json
// MCP: task.start
{
  "goal": "登录后台，筛选今天的失败订单，导出 CSV",
  "kind": "workflow",
  "mode": "auto",
  "policy": {
    "max_model_calls": 2,
    "max_wall_ms": 120000,
    "allow_irreversible": false,
    "allowed_hosts": ["admin.local"]
  },
  "session": {
    "base_url": "http://admin.local:8080",
    "credentials": { "username": "qa", "password": "••••" }
  }
}
```

返回 `{ "task_id": "01J8Z..." }`，后续通过事件流观察。

## 1. 规划（模型调用 #1）

`steps` 为空 → Task Engine 调 `Capability::Plan`。

```
t=0      task.created
t=40ms   step 计划返回（1 次模型调用，约 1.8s 实际）
```

计划产出：

```json
[
  { "id":"s1", "goal":"登录", "executor":"dom", "side_effect":"read_only",
    "actions":[
      {"type":"type","target":{"locator":{"any_of":[{"label":"用户名"}]}},"text":"qa"},
      {"type":"type","target":{"locator":{"any_of":[{"label":"密码"}]}},"text":"••••"},
      {"type":"click","target":{"locator":{"any_of":[{"role":"button","name":"登录"}]}}}
    ],
    "success":{"type":"element_visible","locator":{"any_of":[{"role":"navigation"}]}},
    "timeout_ms":15000 },

  { "id":"s2", "goal":"筛选今天的失败订单", "executor":"dom",
    "actions":[
      {"type":"navigate","url":"http://admin.local:8080/orders"},
      {"type":"type","target":{"locator":{"any_of":[{"label":"日期"}]}},"text":"2026-10-28"},
      {"type":"select_option","target":{"locator":{"any_of":[{"label":"状态"}]}},
       "values":["failed"]},
      {"type":"click","target":{"locator":{"any_of":[{"role":"button","name":"查询"}]}}}
    ],
    "success":{"type":"element_text_matches",
               "locator":{"any_of":[{"role":"row"}]},"regex":"共 \\d+ 条"},
    "timeout_ms":20000 },

  { "id":"s3", "goal":"导出筛选后的订单", "executor":"dom",
    "actions":[{"type":"click","target":{"locator":{"any_of":[{"role":"button","name":"导出 CSV"}]}}}],
    "success":{"type":"download_completed","filename_regex":".*\\.csv","min_bytes":1},
    "timeout_ms":30000,
    "side_effect":"reversible" }
]
```

规划期间 `task.state = running`，`model_calls = 1`。

## 2. 执行步骤 s1：登录

```
t=1.9s  step.started(s1)
```

DOM Executor 对每个 action 走 `resolve → actionability → dispatch → post-condition`：

| 动作 | 解析命中策略 | 可执行性 | 耗时 |
|---|---|---|---|
| type 用户名 | `label` | 可见、enabled | 18 ms |
| type 密码 | `label` | 可见、enabled | 16 ms |
| click 登录 | `role+name` | 可见、未被遮挡 | 11 ms |

成功后 `s1.success` 的 `element_visible(navigation)` 在统一的 `check` 里求值：
观察刷新（`observe`，约 35 ms）→ 谓词成立 → `step.finished(succeeded)`。

此处**没有模型参与**。如果「登录」按钮匹配到 2 个候选，`resolve` 返回 `Ambiguous`，
DOM Executor 依次尝试：`within` 缩窄到登录表单 → 仍歧义 → 报 `locator_ambiguous` 并携带候选，
由 Task Engine 决定重试还是升级。

## 3. 执行步骤 s2：筛选

导航 → 等 `load` → 填日期 → 选状态 → 点查询。

`success` 是正则匹配「共 N 条」。这里有一个设计细节：条件成立 ≠ 结果正确。
`element_text_matches` 只证明页面渲染出了计数行，不证明 N 是今天的数量。
所以步骤的 `success` 应写成可判定的形式（例如 `element_count` 匹配预期），
否则任务会「成功」在错误结果上。这类语义验证要额外花一次模型调用（`Verify`），
而本任务预算只有 2 次，已被规划和对话框用掉——因此 s2 **按「结构验证通过、
语义未验证」记录**，`verified` 如实上报，不包装成成功。

预算充裕时（例如 `max_model_calls = 3`），这里会调一次 `Verify` 来判断计数是否合理。
两者都是合法设计，区别只在于是否愿意为「结果正确性」付费。

> 实现（2026-10-28）把语义校验接在**任务末尾**：`policy.semantic_verify` 在所有步骤
> 机械通过后观察最终页面并判一次；`uncertain` 默认转成任务级人审。上面的步骤内校验
> 是同一能力的另一种接法；`Repair` 落地后仍未采用步骤级语义门——修复只处理失败的
> `read_only` 步骤，是动作级恢复，不改变「成功」的定义。

## 4. 执行步骤 s3：导出（升级点）

点击「导出 CSV」后，页面弹出一个计划里不存在的确认框。

```
t=8.4s  action.executed(s3, click, ok=true, elapsed_ms=14)
t=8.45s observation: dialogs = [{ kind:"confirm", message:"导出将包含 37 条记录，继续？" }]
```

Task Engine 的升级规则命中（出现未预期对话框）→ `escalation.requested`。

**模型调用 #2**（`Capability::Verify` 或 `Locate`，二选一由 Router 决定）：

```json
{
  "capability": "verify",
  "observation_id": "obs_64",
  "state": "Task: 导出筛选后的订单\nGoal step: export filtered orders\nDialog: 导出将包含 37 条记录，继续？",
  "instructions": "Should the dialog be confirmed to continue the task?",
  "criteria": { "confirm": "确认/继续 对话框以完成导出", "cancel": "取消对话框" }
}
```

响应 `{"choice":"confirm","confidence":0.81}` → 通过提案校验 → 生成一个临时动作
`click role=button name=确认` 并执行。**这个动作不写回计划**，只进 trace（否则下次执行会多一步）。

如果置信度低于阈值或候选歧义，任务进入 `waiting_approval`，事件里带上截图与对话框文本。
这正是「不可逆动作默认拦截」的落点：导出本身可逆，但如果对话框写的是「删除 37 条记录」，
`side_effect` 判定为 `irreversible`，即使模型说 confirm 也不执行。

## 5. 产物与收尾

```
t=9.1s  download.completed { filename:"orders_2026-10-28.csv", bytes: 4821 }
t=9.1s  artifact.created { id, path:".../artifacts/orders_2026-10-28.csv", sha256 }
t=9.15s task.succeeded
```

最终结果：

```json
{
  "task_id": "01J8Z...",
  "state": "succeeded",
  "steps": [
    { "id":"s1", "status":"succeeded", "verified":true,  "model_calls":0, "elapsed_ms":1180 },
    { "id":"s2", "status":"succeeded", "verified":true,  "model_calls":0, "elapsed_ms":2140 },
    { "id":"s3", "status":"succeeded", "verified":true,  "model_calls":1, "elapsed_ms":3900 }
  ],
  "metrics": {
    "model_calls": 2,
    "actions": 9,
    "invalid_actions": 0,
    "wall_ms": 9150,
    "downloads": 1
  },
  "artifacts": ["art_01J8Z..."]
}
```

## 6. 异常路径（同一任务的变体）

| 场景 | 谁发现 | 处理 | 终态 |
|---|---|---|---|
| 点「导出」时元素位移（动画中） | actionability 检查 | 等待盒模型稳定，重试 1 次 | 成功，`invalid_actions +1` |
| 查询接口 500 | `success` 超时 | 重新观察 → 未恢复 → 升级模型 | 取决于模型修复 |
| 登录后出现 cookie 弹窗 | 观察发现 `dialogs` 非空 | 规则：`accept`（低风险，不需要模型） | 成功 |
| 进程在「点击导出」与「下载完成」之间被杀 | 重启时检查点停在 s3 `in_flight` | **不重试**，转 `waiting_approval`，附上次动作与截图 | 等人确认 |
| 目标是 `irreversible` 且策略不允许 | `bh-task` 前置检查 | 直接 `waiting_approval`，不执行 | 等批准 |
| 用户 `task.cancel` | `CancellationToken` | 释放按键/鼠标 → 关闭下载监听 → `canceled` | < 200 ms |

## 7. 持续交互任务（对照）

同一个 Harness，不同生命周期：

```json
{ "kind":"interactive", "goal":"把小方块移动到目标区",
  "controller": { "tick_hz":30, "policy_version":1, "rules":[ ... ] } }
```

```
task.created → controller.started → 每秒一条 controller.tick_summary
            → goal 谓词成立 → controller.stopped(GoalMet) → task.succeeded
```

差异点：

- 不规划步骤，不产生 `Step`。
- 模型只在 `interactive.start` 前（生成策略）与状态跳变时（`update_policy`）介入。
- 事件是**聚合的**（每秒一次），不是每次 tick 一条，否则事件流本身成为瓶颈。

## 8. 这次走查暴露出、并已写回设计的三件事

1. **对话框动作不能写回计划。** 计划是可重放的，临时决策不是。已加进 §4 与协议的事件语义。
2. **`success` 条件可能验证了错误的东西。** 「页面渲染出计数」不等于「筛选正确」。
   协议里补了「未验证则如实上报」，不把 `verified=false` 包装成成功。
3. **崩溃点落在动作与验证之间时，重试是危险的。** 已定为默认转 `waiting_approval`，
   而不是「没验证到就当失败重来」。

## 9. 落地对照（Phase 4 之后）

这篇走查写于实现之前。Phase 2/3/4 落地后，逐条对照如下——差距是有意的，不是遗漏：

| 走查里的说法 | 实际实现 |
|---|---|
| 契约「接口 500 → 重新观察 → 升级模型」 | 步骤级重试（`retry.max_attempts`）+ `retry.on_exhausted=ask_model`：只对 `read_only` 步骤把失败步骤、错误和当前页交给模型，替换步骤过安全包络后只派发一次 |
| 「元素位移 → 等待盒模型稳定」 | 动作前 `scroll_into_view` + 重读盒模型；没有「等待稳定」的循环 |
| 「检查点停在 s3 `in_flight`」 | 检查点只在成功后写；崩溃时靠 `steps.state = running` 判断「已发出未验证」 |
| 恢复后「转 waiting_approval，附上次动作与截图」 | 转 `waiting_approval`，payload 是 `{"next_index": idx}`；Phase 2 不截图，所以没有截图可附 |
| 第 1 节「任务开始时由引擎调模型规划」 | `mode=auto` 的空 steps 任务由引擎在任务内规划：打开 `start_url` → 观察 → 一次 Plan 调用 → 校验边界 → 写回 `task.json` → 执行；规划失败不派发步骤。`bhx plan` 仍是独立的显式入口 |
| `metrics.invalid_actions`、`model_calls` | `tasks.model_calls` 已采集（模型尝试次数；A/C 组 0，B 组 1–2，规划后的任务 0）；`invalid_actions` 仍缺动作级 trace |
| 取消 < 200 ms | 实测 85 ms（Phase 1），任务层同样保留轮询检查 |

一个**走查里没写、实现后才暴露**的路径：确认弹窗。原策略是「一律 dismiss」，
结果 `confirm("导出 3 条记录？")` 被替用户回答「取消」，导出静默不发生。现在
`policy.accept_confirm` 默认 false、任务显式开启，`alert`/`beforeunload` 仍自动接受。
