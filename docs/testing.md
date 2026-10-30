# 测试与验收 v0.1

验收标准在 [requirements.md](requirements.md)。这份文件讲**怎么跑**，
以及为什么断言必须打在服务端真相上。

## 1. 原则

1. **离线可重复。** 所有测试站点本地起，无外部资源，无公网依赖。站点改版不会造成假红。
2. **断言服务端状态，不断言「Agent 觉得自己做成了」。** 每个站点暴露 `/__state`，
   测试读服务端的订单表、表单记录、文件清单。UI 只是被操作的对象。
3. **故障可注入、可复现。** chaos 接口带 seed，同一个 seed 产生同一串故障。
4. **不靠 sleep。** 测试自己的等待也走条件轮询，否则测试本身就会随机失败。

## 2. 测试场

当前实现（Phase 4，2026-10-28）：

```
testkit/
├── server.py               Python 标准库 HTTP 服务：静态托管 + 站点 API + 服务端真相 + chaos
├── check.py                断言脚本：读 __state / 产物文件，与服务器记录比对
├── vlm.py                  视觉模型替身：纯标准库 PNG 解码 + 颜色区域扫描，OpenAI 兼容接口
├── planner.py              规划模型替身：从状态文本取 URL，返回固定 3 步工作流；`/__mode` 产出空/坏步骤/越界等计划，`/__last` 留请求原文
├── verifier.py             校验模型替身：规则判定 + `/__mode` 强制 achieved / not_achieved / 置信度
├── repairer.py             修复模型替身：返回修正步骤，`/__mode` 切换各类违规提案，`/__last` 留请求原文
├── laya-stub.py            Laya sidecar 替身：同一行协议（ready 握手 + 每行一答），按 state 里的标记作答，`LAYA_STUB_LOG` 记录 spawn/请求/判定
├── run-phase1.sh           Phase 1 套件（6 项）
├── run-phase2.sh           Phase 2 套件（A1–A5 + R1，14 项）
├── run-phase3.sh           Phase 3 套件（C1–C4 + B1–B6，10 个用例 41 项断言）
├── run-phase4.sh           Phase 4/5/6 套件（起 REST 服务与三个模型替身，跑 rest/session/trace-stats/schedule/remote/plan/auto-plan/semantic/laya/interactive/repair smoke）
├── agent-acceptance.sh     跨 Agent 验收（人工，调外部 Agent；按站点下载记录核对，不进 CI）
├── mcp-smoke.py            MCP stdio 端到端（browser_* + task_*）
├── rest-smoke.py           REST + WebSocket 端到端（创建、轮询、事件流、产物、错误、取消）
├── session-smoke.py        REST 会话级浏览器控制（导航、观察、动作、条件、JS 双世界、截图、下载、双会话、关闭）
├── plan-smoke.py           规划端到端（goal → 提案 → spec → 任务 → CSV，查凭据不落盘）
├── auto-plan-smoke.py      空步骤自动规划（观察 start_url → 计划落盘 → 执行 → 八类拒绝路径）
├── semantic-smoke.py       任务内语义校验（achieved / not_achieved / uncertain / 无适配器 / 坏阈值 / 截图校验）
├── laya-smoke.py           Laya 适配器（本地判定、审批、远程优先/回落、坏 choice、配置错误、MCP 复用）
├── interactive-smoke.py    MCP 交互作业（后台启动、状态、策略热更新、陈旧版本、停止释放输入）
├── repair-smoke.py         自动修复（成功、修复后仍失败、非 read_only 不修、九类违规提案被拒）
├── trace-stats-smoke.py    运行历史与 trace（统计口径、action 事件、trace 包、密码不入 trace）
├── schedule-smoke.py       定时执行（名字不落盘、缺值拒跑、真实导出、失败不重试、禁用/删除、常驻模式、并发上限）
├── remote-smoke.py         远端浏览器（自起 Chrome，http/ws 两种端点、只关自己的标签、坏端点 5 s 内失败）
├── arbiter-smoke.py        歧义定位仲裁（默认失败、可区分才问、阈值、假 id、错选也如实记录、模型不可用、候选过多、坏策略）
├── decide-smoke.py         控制器内的模型选策略（两种策略得分不同、不启用即零调用、阈值、假 id、预算、无适配器、坏策略被拒）
├── dino-smoke.py           chrome://dino 交互演示（像素观察 + 跳跃规则；需真实 Chrome，不进 run-phase4）
├── chooser.py              候选选择（Locate）替身：按标签/模式回答、可制造假 id 与低置信（id 不限于 c1..cN）
└── sites/
    ├── admin/              登录、订单表（async fetch）、筛选、导出 CSV、审计握手、弹窗
    ├── form/               3 页向导：客户端校验 + 服务端 422 校验 + 重提
    ├── game/               canvas 小游戏：键盘移动 + 坐标点击 + 遥测上报
    ├── dino/               chrome://dino 交互任务：canvas 像素 → 跳跃阈值（见该目录 README）
    ├── arbiter/            两个同名按钮（不可区分）与导出 CSV/PDF（可区分）：仲裁的两面
    ├── plane/              两条航线速度不同的打飞机：「先守哪条」有可测量收益（策略选择）
    └── canvas/             视觉用例：有/无语义按钮、拖拽、延迟确认、布局 b、DOM 改道
```

`python3 testkit/server.py --port 8766` 起服务，`/` 是 admin，`/form/` 是表单，
`/canvas/` 是视觉测试页。任务脚本里的端口由套件用 `sed` 替换成实际端口，套件不硬编码 8766。

> 原来设计里的 `testkit/server`（axum 二进制）与 `testkit/sites/chaos` 静态站点没有按原样做：
> 服务端真相必须由**同一个进程**记录，chaos 也必须在请求/渲染路径上，而不是另一个页面。
> 一个 200 行的 `server.py` 比两个 cargo crate 更容易看清和改动，且不引入测试侧的编译依赖。
> `canvas` 的布局变体用查询参数（`?mode=dom`、`?slow=1`、`?ghost=1`）产生，不复制页面。

### 站点契约

每个站点必须提供：

| 接口 | 用途 |
|---|---|
| `GET /<site>/__state` | 服务端真相：订单行、表单记录、已导出文件 |
| `POST /<site>/__reset` | 恢复初始状态 |
| `POST /<site>/__chaos` | 注入故障 |
| `GET /<site>/__downloads` | 已产生的下载文件与校验和（目前 admin 有） |

`__state` 是断言的唯一依据。A 组「导出 CSV」的通过标准是：产物文件的字节与
`/admin/__state.downloads[-1].sha256` 一致，**而不是**页面上出现了某个 toast。

实现上 `check.py` 不信任页面，也不信任产物文件名：

| 子命令 | 断言 |
|---|---|
| `extract --artifact --state` | 提取出的 JSON 与 `/admin/__state.orders` 的（筛选后）行集合一致 |
| `download --artifact --state` | 产物 sha256、字节数、UTF-8 BOM、表头与最后一行的 `\n` 与服务器记录一致 |
| `form --state --expect` | `/form/__state.submissions[-1]` 的字段逐项等于期望值 |
| `rows --state --count` | 服务端订单集合没有被测试意外改动 |
| `chaos --state` | 注入的每条规则都真的 fire 过（`chaos_applied`） |
| `game --state` | 页面遥测：退出时按键为空、位置达到目标、点击落在画布内 |
| `canvas --state` | 服务端记录的点击/拖拽事件：来源（`dom`/`canvas`）、落点距离 ≤ 容差、拖拽 `ok`、`confirmed` |

## 3. chaos 接口

```json
POST /admin/__chaos
{
  "seed": 42,
  "rules": [
    { "on": "click", "selector": "#export", "popup": "confirm",
      "message": "导出将包含 37 条记录，继续？", "times": 1 },
    { "on": "request", "path": "/api/orders", "status": 500, "times": 2 },
    { "on": "layout", "variant": "B", "after_ms": 3000 },
    { "on": "move", "selector": "#submit", "pixels": [40, -18], "duration_ms": 800 },
    { "on": "delay", "path": "/api/orders", "ms": 2500 }
  ]
}
```

语义（按实现写，不是按设想写）：

- **`on: request` / `on: delay`** 由 `server.py` 在请求路径上消费：命中 `path` 且 `times > 0`
  就返回指定状态码或先睡 `ms`。这类规则不在浏览器里，页面无法绕过。
- **`on: click`** 不下发到服务端执行，而是列在 `GET /admin/__chaos` 里，由页面在绑定事件时
  读取：命中 `selector` 的点击先弹 `popup`（`alert`/`confirm`/`prompt`）再继续，然后
  `POST /admin/__chaos/fire` 把规则记为已消费。这样弹窗是页面自己的行为，和真实站点一致。
- 规则按序消费，`times` 用尽后失效。
- 每次消费都写进 `/__state.chaos_applied`（`id`、`on`、`path`/`selector`），测试失败时能直接
  看到注入了什么、命中没有。
- 未命中的规则视为**测试配置错误**，套件失败——否则「注入了但没触发」会被误读成「恢复能力强」。
  `run-phase2.sh` 的 A5 在任务失败时会单独重查 `chaos` 子命令并提示这一点。

尚未实现：`on: layout`、`on: move`。Phase 3 的布局变化改用页面变体（B5 直接加载
布局 b）验证，不依赖 chaos 在运行中改布局；这两条留到需要「任务进行中改版」的用例。
`seed` 目前只记录，不参与规则选择——规则本身已经是确定的，随机选择只会让复现更难。

## 4. 套件

原计划用 `cargo test -p testkit` 跑套件；实际实现是 shell 套件 + 独立断言脚本。
理由：验收要驱动**真实进程**（`bhx task run`、`kill -9`）并读另一个进程的 HTTP 状态，
放进 Rust 测试只会把进程管理、超时和清理都写一遍，换不来可读性。

```bash
./testkit/run-phase1.sh          # Browser Runtime + MCP（6 项）
./testkit/run-phase2.sh          # 任务引擎 + 崩溃恢复（14 项）
./testkit/run-phase3.sh          # 交互控制器 + 视觉路径（41 项）
./testkit/run-phase4.sh          # REST + 会话 + trace 统计 + 定时执行 + 远端浏览器 + 规划 + 自动规划 + 语义校验 + Laya + 交互作业 + 自动修复 + 歧义仲裁 + 模型选策略（22 + 47 + 45 + 61 + 17 + 17 + 88 + 62 + 80 + 42 + 112 + 48 + 39 项）
python3 testkit/mcp-smoke.py     # MCP stdio 端到端
python3 testkit/dino-smoke.py    # chrome://dino 交互演示（需真实 Chrome；不计入上面的项数）
```

`dino-smoke.py` 自起（或经 `--cdp-url` 接上）Chrome，用 `interactive_start` 玩 20 秒并断言
`goal_met`、实际跳跃次数与 tick 预算；它依赖 `chrome://dino`，因此不进 `run-phase4.sh`。

`run-phase3.sh` 自己起 `server.py` 和 `vlm.py`（端口 8768 / 8790），设好 `BH_VLM_URL`，
不需要外部模型服务。`check.py` 的退出码被 shell 套件计数——断言失败会让整个套件失败，
不会出现「打印了 FAIL 但总结仍是 0 failed」。

套件失败时保留 `mktemp` 目录（脚本末尾会打印路径），里面有每个用例的 JSON 输出、
stderr 和任务 home。

每个用例的形状（以 R1 为例，就是 `run-phase2.sh` 在做的事）：

```bash
bhx task run task.json --home "$H" &        # 跑起来
# 等 events.jsonl 里出现目标步骤，然后 kill -9
bhx task resume "$ID" --home "$H"           # 期望退出码 3（要人批准）
bhx task approve "$ID" --home "$H"
bhx task resume "$ID" --home "$H"           # 只应重跑被中断的那一步
check.py form --state "$BASE/form/__state"  # 服务端真相，而不是任务自述
```

> A 组没有模型参与（任务脚本化），所以 `model_calls` 恒为 0；退出条件「≤ 2」
> 现在还测不出模型质量问题，要到 Phase 4 接了真实规划才有意义。

### Phase 3：C 组与 B 组

C 组测循环，不测模型；B 组测整条视觉链路：

| 用例 | 断言 |
|---|---|
| C1 长按移动 | 退出后无粘键（页面探针 `keys` 为空）、位置过线、`why=goal_met` |
| C2 坐标点击 | 服务端收到恰好 1 次画布点击 |
| C3 观察后切换 | 先右后下，两个方向都到位，动作数 ≥ 2 |
| C4 截止时间 | `why=deadline`、退出码 1、1–3 s 内停止、输入已释放 |
| B1 有语义按钮 | 目标仍写作 `{"visual": …}`，只能走截图路径；落点来源 `dom` |
| B2 无语义画布 | 前置条件断言页面无可交互语义元素；落点距离期望 ≤ 6 px |
| B3 拖拽 | 起点 140,200、落点 500,200，两端误差 ≤ 4 px，服务端 `ok` |
| B4 状态确认 | 按钮外观不变；步骤耗时 ≥ 300 ms（等到服务端确认），且 `confirmed` |
| B5 布局变化 | 同一描述，布局 b 下重新命中（460,278 ± 6 px），不沿用旧坐标 |
| B6 DOM 改道 | 定位器命中 `display:none` 祖先下的元素 → `LocatorNotFound` → 视觉重试成功 |

「无粘键」读的是页面自己的最后一次上报，所以控制器在 `key_up` 之后会留一个
上报周期（350 ms）再返回：页面按定时器采集输入状态，浏览器进程一关，最后一条
释放报告就可能丢在 socket 里。C4 曾因此偶发失败（页面最后一次上报仍是按住），
这个等待是让断言测到真实状态，不是放宽断言。`controller.elapsed_ms` 在这一步之前
就固定了，所以截止时间的窗口不受影响。

每个 B 用例都断言 `steps[-1].verified`、任务耗时和模型调用数（≤ 2）。视觉模型是
`testkit/vlm.py`：纯标准库解码 PNG，按颜色找最大区域，返回 `bbox_norm` 和置信度。
它**只证明 harness 这一侧**（截图尺寸、归一化换算、动作、验证）是通的，
不衡量真实模型的选择质量；换成真模型只改 `BH_VLM_URL`。

### Phase 4：REST + WebSocket

`run-phase4.sh` 起 testkit server（8770）与 `bhx serve`（8771），再由
`rest-smoke.py` 用标准库把外部 Agent 的路径走一遍：

| 检查 | 断言 |
|---|---|
| 创建 | `POST /v1/tasks` 返回 202 + `task_id`，后台执行 |
| 状态 | 轮询到 `succeeded`；3 个步骤 `verified`；`model_calls = 0` |
| 事件（HTTP） | `seq` 严格递增且无重复；含 `task.created` / `task.finished` |
| 事件（WS） | `?after=0` 重放全部 12 条；收到 `task.finished` 与 `stream.closed` |
| 产物 | 列表给出 `text/csv`；字节 sha256 与列表一致，且等于站点 `/admin/__state` 的记录 |
| 失败路径 | 非 auto 空 workflow → 422；auto 空 workflow → 202，随后因无模型失败并报 `planning_unavailable`；重复 id → 409 |
| 取消 | 启动一个 3 s 握手任务，0.8 s 后 cancel，状态变 `canceled` |

WS 客户端是脚本里手写的（握手 + 帧解析，约 60 行），不引入 `websockets` 依赖。
事件流本身是从 SQLite 尾随读，所以「重连」与「首次连接」走的是同一条路径；
这比测试一个内存广播的订阅语义更能说明断线续传是真的。

同一阶段 `mcp-smoke.py` 增加断言：`browser_evaluate` 在 main world 写下的
`window.__bhxProbe` 在 isolated world 里是 `undefined`（DOM 仍可读）；
`browser_verify` 在登录前判 `not_achieved`、登录后判 `achieved`、阈值调到 0.95 时
判 `uncertain`（不把低置信度四舍五入）；
`task_artifacts` 对无产物任务返回 `(no artifacts)` 而不是报错；
`artifact_list` / `artifact_metadata` / `artifact_fetch` 对一个真实导出任务返回
CSV 名称、sha256 与原文，未知名字返回错误。
冒烟脚本自己起 `verifier.py`（端口 8793）并把 `BH_LLM_URL` 只传给 MCP 子进程。

### Phase 4：REST 会话级控制

`session-smoke.py` 用同一个 `bhx serve`（未配置模型适配器）把 HTTP-only Agent 的
页面级路径走一遍。断言落在三处：HTTP 状态与响应、站点 `/admin/__state` /
`/admin/__downloads`、以及会话下载目录里的真实文件。

| 检查 | 断言 |
|---|---|
| 生命周期 | 创建 201 + `about:blank`；列表含 id；`GET` 回显 `allowed_hosts`；关闭后 404 且从列表消失 |
| 导航 | 到 `/admin/` 返回 URL 与标题；不在白名单的主机 → 403，且 `GET` 仍停在原页 |
| 观察 | 状态文本含 `Controls:` 与「登录」；元素数 ≥ 3；附带截图是 JPEG 且带 `screenshot_mime`；`screenshot_error` 为空 |
| JS 双世界 | isolated 能读 DOM；main world 写 `window.__probe` 后 isolated 读到 `undefined`；未知 world → 422 |
| 动作 | 空批次 → 422；点不存在的元素 → 422 且日志含 `[0] failed`；输入账号密码 → 200，观察里密码是 `***` 且原文不出现 |
| 条件 | 等待 `#app` 出现 → `holds:true`；`false` 即时采样 → `holds:false`（不是错误） |
| 白名单与批次 | 批次里先点击再导航到白名单外 → 403；页面未导航，且站点下载记录仍为 0（第一个动作也没跑） |
| 下载 | 点导出后站点记录出现；文件落在 `<home>/sessions/<id>/downloads/`，sha256 与站点一致 |
| 截图端点 | `GET …/screenshot` 返回 `image/png` 且字节以 PNG 魔数开头 |
| 能力缺失 | 未配置适配器时 `verify` → 503（不是 422） |
| 双会话 | 第二个会话导航到 `/game/`，第一个仍在 `/admin/`；关掉第二个后第一个仍能判条件 |
| 未知会话 | 任意端点 → 404 |

批次失败的回滚语义是刻意不做的：422 表示「停在这里」，失败前的动作已经生效，
日志给出停在第几条。

### Phase 4：规划

`plan-smoke.py` 的模型端是 `testkit/planner.py`（确定性规则：从状态文本的 `URL:` 行取
站点地址，返回登录 → 筛选 → 导出 3 步）。它验证的是 harness 这一侧，不评估真实模型的
规划质量。流程：`bhx plan --url … --out … --run --cred …`，然后断言：

| 检查 | 断言 |
|---|---|
| 规划调用 | 退出码 0；stderr 报告 3 步、**恰好 1 次**模型调用（预算 2） |
| 提案校验 | `out.json` 的步骤 id 为 `login`/`filter`/`export`，目标与输入一致 |
| 凭据 | spec 与 `task.json` 都含 `${cred:username}` / `${cred:password}`，且都**不含**密码值；`credentials` 表为空 |
| 执行 | 任务 `succeeded`，3 步全部 `verified` |
| 模型预算 | 执行规划后任务的 `model_calls = 0`（规划调用不计入任务预算） |
| 产物 | 任务产物字节数/sha256 与列表一致，且等于站点 `/admin/__state` 的下载记录 |

规划请求带上的是 CLI 观察到的真实页面状态（与执行器同一个 `Observer`），所以输入形状
不会和运行时漂移；`--run` 前丢弃规划用的浏览器，执行由引擎自己起浏览器和下载目录。
`run-phase4.sh` 用独立 home（`home-plan`）跑这一组，避免和 REST 用例的任务混在一起。

### Phase 4：引擎内自动规划

`auto-plan-smoke.py` 提交一份 `steps=[]`、`mode=auto`、带 `start_url` 的 workflow，
由 `bhx task run`（可带 `--cred`）在引擎内规划。`planner.py` 的 `/__mode` 让同一组
用例走不同拒绝路径，`/__last` 用来核对发给模型的原文。

| 场景 | 断言 |
|---|---|
| 正常规划 | 退出码 0；规划恰好 1 次且请求含 `URL: <start_url>` 与 `Goal:`；任务 `succeeded`、3 步 verified、`model_calls=1` |
| 计划落盘 | `task.json` 的步骤 id/顺序与执行一致，保留 `start_url` 和 `${cred:…}`，不含密码值；steps 表同样保留占位符、不含密码；事件里也没有 |
| 事件 | `task.created` 时 steps=0，随后 `plan.requested` / `plan.created`（记录 3 步）；没有步骤在计划落盘前启动 |
| 产物 | CSV 的 sha256 同时等于任务列表与站点 `/admin/__state.downloads[-1]` |
| 坏提案 | 空计划、未知 action、空 actions、timeout=0：任务 `failed`，错误说明原因，0 个步骤行，模型各调用 1 次 |
| 越界提案 | `navigate` 到 `allowed_hosts` 外、引用未提供的凭据：计划被拒，0 个步骤行，各 1 次调用 |
| 预算/适配器 | `max_model_calls=0` → `budget` 失败且模型未被调用；无适配器 → `planning_unavailable`、0 次调用 |
| 起点越界 | `start_url` 不在 `allowed_hosts`：浏览器不导航、模型不被调用，任务失败 |
| 显式空步骤 | `mode=scripted` + `steps=[]`：CLI 退出 2、错误说明需要 `mode=auto`、没有创建任务 |

这一组证明的是「引擎在什么条件下才允许模型写计划、计划何时算被接受、失败时是否
真的什么都没派发」。计划本身的可执行性由确定性替身固定，真实模型的规划质量不在 CI
声明范围内。

### Phase 4：任务内语义校验

`semantic-smoke.py` 用 `verifier.py` 的 `/__mode` 端点强制判定（默认仍按页面规则），
对同一份 admin 工作流逐个场景跑真实任务：

| 场景 | 断言 |
|---|---|
| `achieved` | 任务 `succeeded`；`result.semantic` 为 `achieved`、模型 `llm`；3 步仍全部 verified；`model_calls=1`；恰好一条 `task.verified` |
| `not_achieved` | 任务 `failed`，但 3 步的机械校验仍为 `verified`；`result.semantic` 为 `not_achieved`；错误信息说明语义校验未确认 |
| `uncertain`（wait_human） | `task run` 退出码 3；任务级审批的 `payload.next_index=3`、`payload.semantic.verdict=uncertain`；approve + resume 后成功，`step.started` 仍只有 3 条、模型调用仍为 1、`task.verified` 只有一条 |
| `uncertain` + `on_uncertain=fail` | 任务 `failed`，错误说明是策略决定 |
| 无模型适配器 | 步骤全 verified 但任务 `failed`，错误包含 `semantic verification failed` 与 `no model adapter`；`model_calls=0` |
| 阈值 1.5 | 启动浏览器前拒绝，`tasks/` 为空 |
| 截图校验 | 请求里带 `image_url` 数据 URI；`result.semantic.screenshot` 记下 `1280x800`、字节数与 sha256；替身 `/__last` 收到的 sha256、字节数与之相同；`artifacts/verify-final.png` 的哈希也相同；`task.verify.screenshot` 在 `task.verified` 之前 |
| 不开截图 | 请求里没有 image；`result.semantic` 没有 `screenshot`；任务目录里没有 `verify-final.png`；没有截图事件 |
| 回放器 | `trace --out` 包里有 `*-verify-final.png`；真实 Chrome `--dump-dom` 后的页面含 `<img src="artifacts/…verify-final.png">`，即模型看到的那张图 |

这一组刻意不检查模型判断质量：替身只证明引擎在三种回答下的状态机、审批载荷、
恢复路径和「配置了就必须真的问」是通的。

> `verifier.py` 的 `/__mode` 只影响替身返回值，不进入 harness 协议；生产代码没有
> 「按测试开关改判定」的分支。

### Phase 4：Laya 适配器

`laya-smoke.py` 通过 `BH_LAYA_DECIDE` 接上 `testkit/laya-stub.py`。替身说同一行协议
（ready 握手 + 每行一答），但不推理：它按 state 里的标记作答，并把 spawn、state、
criteria、判定写进 `LAYA_STUB_LOG`。这样检查组能同时验证「判定怎么被用」和
「什么跨过了 sidecar 边界」。

| 场景 | 断言 |
|---|---|
| 本地判定 | 只配 Laya：任务 `succeeded`；`semantic.model=laya`、confidence 0.90、threshold 0.60；`model_calls=1`；`task.verified` 记 laya |
| 边界内容 | 日志里的 state 含 `Task: <goal>` 与 `导出 CSV`；criteria 恰好是 `achieved` / `not_achieved`；整份日志不含密码值；恰好 1 个进程、1 次判定 |
| `not_achieved` | 机械步骤全 verified，任务因判定 `failed`；`model_calls=1` |
| `uncertain` + 审批 | 0.40 < 0.60 → `waiting_approval`；审批载荷 `semantic.model=laya`；approve + resume 后成功，判定仍只有 1 次 |
| 远程优先（默认） | 同时配 `BH_LLM_URL`：远程作答，sidecar **没有启动**（spawn=0） |
| 远程不可达 | `BH_LLM_URL` 指向死端口：Laya 兜底作答，仍只计 1 次调用 |
| 本地优先 | `BH_LAYA_PRIORITY=first`：本地判定生效，任务按 `not_achieved` 失败 |
| 坏 choice | 本地优先且替身返回 `e99`：router 拒绝，远程作答；sidecar 不重启（spawn 仍 1） |
| 无兜底 | sidecar 启动即退出、也没配远程：任务失败，错误含 `before the ready handshake`，尝试计 1 次 |
| 配置错误 | `BH_LAYA_DECIDE` 指向不存在的文件：适配器不注册，错误 `no model adapter`，0 次调用 |
| 截图被拒 | 任务要求截图但只配 Laya：任务 `failed`，错误含 `laya has no vision`；sidecar 未启动（spawn=0）、没有任何判定；这次尝试仍计 1 次模型调用；不落盘截图、不伪造判定 |
| MCP 复用 | 同一 `bhx mcp` 进程两次 `browser_verify`：1 个 sidecar 进程、2 次判定；父进程关 stdin 后 sidecar 退出 |

进程生命周期（错误请求不杀进程、超时杀进程且下一次重启、握手超时、提前退出）在
`bh-model` 的单元测试里用同一个替身直接跑，不需要浏览器。真实 Laya 的延迟与
零样本准确率见 [benchmarks.md](benchmarks.md)；它不在 CI 里。

### Phase 4：MCP 交互作业

`interactive-smoke.py` 用 game 页做载荷：控制器按住右方向键，页面自己的遥测记录
位置与按住的键。断言读 `/game/__state`，不读控制器的自述。

| 场景 | 断言 |
|---|---|
| 后台启动 | `interactive_start` 立刻返回 `task_id` 与 `policy_version: 1`；作业在后台跑 |
| 状态 | 运行中 `interactive_status` 第一行是 `job: running`，给出当前策略版本 |
| 目标达成 | 目标 `s.x >= 170`：页面 x ≥ 170、`keys=[]`、停止原因 `goal_met` |
| 热更新 | 运行中把目标换成 `s.x >= 150`（版本 2）：更新被接收，停止事件里的 `policy_version` 是 2，页面在新目标处停下 |
| 陈旧版本 | 同版本更新被拒（`not newer`）；作业结束后更新被拒（`finished`） |
| 停止 | `interactive_stop` 返回 `canceled`；页面按键已释放，位置确实移动过 |
| 谓词批量 | 停止事件里 `evals <= ticks + 1`：目标与所有规则共用一个 JS 往返，不随规则条数增长 |
| tick 指标 | `tick_ms_p50 <= p95 <= max` 且都为正：循环把抖动报告出来，而不是只报平均值 |
| 逐规则计数 | 停止事件 `rules[]` 只列出真正触发过的规则（顺序即首次触发顺序），`entries` 与 `actions` 相加等于总动作数：聚合的 `actions` 分不清「20 次跳跃」和「19 次跳跃 + 一次撞死重开」，这个能 |
| 30 s soak | 目标恒假、持续按住键 30 s：`ticks >= 700`（实测 909）、`evals == ticks`、p50 33 ms / p95 34 ms、结束后 `keys=[]`、服务进程平均 CPU < 100%（实测 3.6%） |
| 坏请求 | workflow 规格、空 observe 脚本、越界 `start_url`、不存在的 task_id 都在启动前/调用时拒绝 |

「页面看到按键被按住」是服务器真相断言：它证明 `key_down` 真的到达了页面，
之后的 `keys=[]` 才有意义。热更新检查组先等页面确认移动，再发更新——否则
浏览器还没启动，测到的只是启动失败。

### Phase 4：歧义定位仲裁

`arbiter-smoke.py` 用两个页面分别证明「什么时候不该问模型」和「问的时候拿到了什么」：
`/arbiter/` 上两个按钮**同名**（页面上没有任何东西能区分它们），`/arbiter/export/` 上
是 `导出 CSV` 与 `导出 PDF`（标签能回答问题）。服务端 `/arbiter/__click` 记录到底哪个
按钮被按下——「选对了」是对站点记录的断言，不是对模型意图的断言。

| 场景 | 断言 |
|---|---|
| 不启用 | 没有 `arbitrate_ambiguous` 时仍是原来的歧义错误，`calls == 0`，什么都没点 |
| 不可区分 | 两个同名按钮：放弃，且**不消耗模型调用**（`calls == 0`）；错误里写清「role 与 name 相同，页面无法区分」并给出改法 |
| 可区分 | 模型选第二个（CSV）→ 任务成功，站点记录 `["csv"]`；一次调用；候选以 `c1/c2` 带真实标签给出，并带 `Controls:` 页面状态与任务目标 |
| 只读标签 | 一个只会读「目标里的词 vs 候选标签」的替身也能选对 CSV —— 证明请求携带的信息足够做决定 |
| 审计 | 点击事件带 `arbitration`：`strategy` / `eid` / `model` / `confidence` / `candidates` / `reason` |
| 低于阈值 | 0.2 的自信答案被拒，原歧义错误照旧，错误里点名是多少置信度不够 |
| 假 id | 回答一个没提供过的 id：不派发任何东西 |
| 选错也如实 | 模型自信地选了两个里的**另一个**（PDF）：确实按它选的按下，事件里能看出这次是猜的 |
| 模型不可用 | 替身 500：仍按歧义失败，错误说明是模型那一段失败，不会偷偷猜 |
| 预算 | `max_model_calls=0` 时不仲裁，`calls == 0` |
| 候选过多 | `max_candidates=1` < 匹配数：拒绝且不花调用，错误说「定位器需要收窄」 |
| 坏策略 | `min_confidence=1.7` 或 `max_candidates=0` 在开工前就被拒，并指出是哪个字段 |

真实 Laya 在同一个可区分场景上选错了（PDF，0.625），`min_confidence=0.7` 把它拦住；
换三个候选时它读标签读对了（CSV 0.78，无关项 0.09）。数字见
[benchmarks.md](benchmarks.md)，这条也是阈值默认取 0.7 的原因。

### Phase 6：控制器里的模型选策略

`decide-smoke.py` 用 `testkit/sites/plane/`：两个编队沿两条速度不同的航线下来，
"先守哪条"因此有可测量的后果。分数读服务端遥测 `/plane/__state`（页面每 250 ms 上报），
不是控制器的自述。模型由 `chooser.py` 扮演，所以「它选哪边」是受控变量而不是采样结果。

| 场景 | 断言 |
|---|---|
| 策略确实起作用 | 模型选慢航线 → 得分 3、两编队全灭；选快航线 → 得分 0、两编队飞走。**同一套规则**，只有 `options` 顺序和模型答案变了 |
| 只有被选中的策略在跑 | 停止事件里 `rules[]` 只有 `go-left/hold-left`（或反之），另一条一次没进 |
| 真的动了 | 遥测里 x 走到对应航线（左 ≤150 / 右 ≥330） |
| 不启用 | 没有 `decide` 时 `decisions == 0`、没有策略规则触发，且替身调用计数不变 |
| 没有适配器 | 配了 `decide` 但没有任何模型：任务照常完成、遥测照常上报，`decisions == 0` —— 不会因为没配模型就失败，也不会假装做了决定 |
| 低于阈值 | `min_confidence` 高于答案置信度 → 不采纳，不改变行为 |
| 假策略 id | 答案不在 `options` 里 → 忽略 |
| 预算 | 接受的决策数不超过 `max_model_calls` |
| 坏策略 | 只有一个选项、空 question、id 含空格/引号/非 ASCII（会进 `s.d` 谓词）→ 开工前拒绝并点名字段 |

真实 Laya（torch/MPS）在同一场景跑过三次，**三次都是 0 分**：第一次选对了，却在火力窗口打开前
改到另一条航线。单独喂它那一局的中盘状态（`nearest: "left"`）它答对，但只把选项顺序对调，置信度就从
0.690 掉到 0.511——刚好越过 0.5 的阈值。结论是**策略切换需要迟滞**，不是把阈值调高。
数字见 [benchmarks.md](benchmarks.md)。

### Phase 4：自动修复

`repair-smoke.py` 把 admin 工作流的 s2 换成一个必然找不到的定位器，并声明
`retry.on_exhausted=ask_model`。`repairer.py` 返回基于收到的失败步骤改写的「筛选失败
订单」步骤（保留 id、前置、成功条件等，只换动作）。

| 场景 | 断言 |
|---|---|
| 正常修复 | 任务 `succeeded`；`model_calls=1`；s2 的 `repair.status=succeeded`、模型 `llm`、置信度 0.82、替换步骤保留 `id=s2` / 前置 / 成功条件 / 执行器 / 副作用；`step.started` 仍为 3；CSV 与站点下载记录同哈希；请求原文含失败步骤、错误与当前页；CLI 摘要带 `[repaired: succeeded]` |
| 修复后仍失败 | 任务 `failed`；`repair.status=failed` 且带 `error`；步骤错误说明「repaired step failed」；只调用一次模型 |
| 非 `read_only` | 任务 `failed`；`model_calls=0`；模型从未被询问；无 `repair` 记录 |
| 九类违规提案 | 任务 `failed`；步骤错误包含对应违规原因；提案**未派发**（无 `repair` 记录、后续步骤不运行）；每次只计一次模型调用 |

违规场景覆盖 `raise_side_effect`、`change_id`、`change_executor`、`change_success`、
`change_preconditions`、`inject_js`、`inject_extract`、`external_nav`、`drop_success`；
替身另有 `fail_action` 专测「替换步骤本身也失败」。`inject_js` / `inject_extract`
守住的是「只读步骤不能变成模型可写的主世界 JS 通道」；`/__last` 让套件能核对发给
模型的原文，而不是只看最终状态。

### Phase 4：跨 Agent 验收（人工，不进 CI）

模型与订阅由本机决定，耗时不进 CI 阈值；`testkit/agent-acceptance.sh` 只把「同一任务、
同一工具、结果按站点记录核对」这件事变成可复跑的命令：

```bash
./testkit/agent-acceptance.sh claude   # claude -p + --strict-mcp-config
./testkit/agent-acceptance.sh pi       # pi -p --provider local --model qwen3.8-flash-next
AGENT_CMD='aider ...' ./testkit/agent-acceptance.sh cmd   # 提示从 stdin 送入
```

脚本起 testkit 站点，写一份只含一个 `bhx mcp` 的 `--mcp-config`，给 Agent 的提示固定为
「`task_run` 一次（参数是 `sites/admin/task.json`）→ `task_status` 一次」，然后**不读
Agent 的回答**，而是拉 `/admin/__state` 核对下载记录和 sha256；站点没记到导出就算失败。

| 场景 | 断言 |
|---|---|
| Claude Code | 退出码 0；站点记录 `orders-失败.csv`，sha256 前缀 `2dd0b48d`；实测 10 s |
| Pi + 本地 Qwen3.8 | 同上；实测 16–22 s |
| 其它 Agent | 结果与耗时记进 [benchmarks.md](benchmarks.md)，不改 harness |

### Phase 6：运行历史与 trace

`trace-stats-smoke.py` 先跑一次成功的 admin 任务（打开 `trace_screenshots`）、一次注定失败的任务
（不开——用来验证失败步骤照样会截），然后只读 store：
`bhx stats` 的数字必须能由 `tasks` / `events` 复算出来，`bhx trace` 必须能还原动作序列
并把产物带出来。断言不读 CLI 的摘要文案，读 `--json`。

| 场景 | 断言 |
|---|---|
| 空 home | `stats` 说没有任务，而不是打印一排 `0` 假装有统计 |
| 成功运行 | `tasks=1`、`succeeded=1`、`success_rate=1.0`、`duration p50>0`、`actions=7`、`failed_actions=0` |
| 动作事件 | `trace` 里恰有 7 条 `action.executed`，且位于对应步骤的 `step.started` 与 `step.finished` 之间；每条带 `step_id` / `index` / `type` / `ok` / `elapsed_ms` |
| 不入 trace 的载荷 | trace JSON 里搜不到输入过的密码（动作只记类型，不记文本） |
| trace 包 | `--out` 写出 `trace.json`；导出的 CSV 被复制进 `artifacts/`，sha256 与站点下载记录一致 |
| 回放器文件 | `trace.html` 随包写出（> 4 KB），源码里没有 `src="http` / `href="http` |
| 回放器真的能跑 | 用真实 Chrome `--dump-dom` 打开 `file://…/trace.html`：DOM 里有脚本生成的 `class="ev"` 行、步骤 `s3` 与产物名；页面里没有密码 |
| 产物直链 | DOM 里的 `href="artifacts/…"` 指向包里真实存在的文件（与 `00-` 前缀的复制命名一致） |
| 截图序列 | 成功任务开了 `policy.trace_screenshots`：3 张 `kind=screenshot` 产物（每张 > 1 KB），3 条 `step.screenshot` 按 s1/s2/s3，且都排在对应 `step.finished` 之前 |
| 失败必截 | 没开开关的失败任务仍留 1 张截图，名字指向失败的那一步（`step-02-s2`） |
| 回放器里的截图 | DOM 里有 6 个 `<img src="artifacts/…">`（3 张步骤卡 + 3 条时间线），全部指向包内文件 |
| 失败运行 | `task run` 退出非零；`stats` 为 `tasks=2 / failed=1 / success_rate=0.5`；`failed_actions=1` |
| 失败动作 | 失败 trace 里恰有一条 `ok=false` 的 `action.executed`，带 `error`；任务终态 `failed` |

### Phase 6：定时执行（schedule）

`schedule-smoke.py` 把 admin 任务的密码改成 `${cred:password}`，`schedule add --every 1s`
之后先不带值跑一次、再带 `BH_CRED_PASSWORD` 跑一次，然后验证禁用/启用/删除与常驻模式，
最后用两个到期的调度验证并发上限。
断言只看站点真相（`/admin/__downloads`）、`schedule ls --json` 和任务 home 里的字节。

| 场景 | 断言 |
|---|---|
| 添加 | 退出 0；提示要设置的 `BH_CRED_PASSWORD`；`ls --json` 有 1 条、enabled、`interval_ms=1000` |
| 存的是什么 | `cred_names=["password"]`；列出的 JSON 里搜不到密码值；任务 home 所有文件里也没有 |
| 缺值运行 | 退出非零且 stderr 含 `BH_CRED_PASSWORD`；站点下载数不变；`last_state=failed`、`last_error` 记录原因、没有 `last_task_id` |
| 槽位已消费 | 紧接着第二次 `run --once` 输出 `no schedule due`、退出 0；下载数仍不变 |
| 到点运行 | 等 `next_in_ms==0` → 带值运行 → 退出 0、输出 `<id> succeeded task <task-id>`；站点恰好多 1 个 `orders-*.csv`（> 20 B） |
| 成功后 | `last_state=succeeded`、`last_task_id` 指向真实任务；`stats` 里 `tasks=1 / succeeded=1`；home 里仍无密码明文 |
| 禁用/启用 | `disable` 后睡过一个间隔也不跑；`enable` 后立刻 `run --once` 也不跑（从当前起算一个间隔） |
| 删除 | `rm` 退出 0、列表清空；重复 `rm` 退出非零 |
| 常驻模式 | 无 `--once` 时启动即跑到期项、跑完继续轮询；1h 间隔在 0.6 s 内不重跑 |
| 并发参数 | `--concurrency 0` / `9` 退出非零并说明范围 `1..=8` |
| 并发 2 | 轮询 `schedule ls` 观察到两个调度同时 `running`；两个调度各产出一个 CSV，`last_state` 都是 `succeeded` |
| 默认串行 | 同样两个调度不带 `--concurrency`，同一时刻最多只有一个 `running`；两个都跑完 |
| 重叠槽位 | 1 s 间隔 + 约 2 s 运行：常驻进程打印 `skipped_overlap` 且不退出；禁用后本次运行仍完成并记为 `succeeded` |
| 坏间隔 | `5 minutes` / `500ms` / `30` 被拒绝，错误说明单位或 1 s 下限 |

### Phase 6：远端浏览器（attach）

`remote-smoke.py` 自己拉起一个 headless Chrome（`--remote-debugging-port` + 临时 profile），
完全不走 `bhx` 的启动路径，然后让 `bhx task run` 通过 `BH_CDP_URL` 去驱动它。它同时起两个
`server.py`（`--port` 与 `--port+1`）：页面把登录态存在 profile 的 `localStorage` 里，
第二次运行若复用同一个 origin 会直接是已登录状态，碰不到用户名输入框——换 origin 才能让
两次运行都真正走完登录。

| 场景 | 断言 |
|---|---|
| `http://host:port` | 退出 0、`state=succeeded`、站点恰好多 1 个导出 |
| 浏览器生命周期 | `bhx` 退出后 Chrome 仍在跑（`child=None`，从不 kill 别人的进程） |
| 标签清理 | 页面 target 数回到运行前；扩展的 background / service_worker target 不计入（它们自己会闪） |
| 任务 home | 用的是 `--home` 指定的目录，不是临时目录 |
| `ws://…` 端点 | 直接连 `/json/version` 给的 `webSocketDebuggerUrl`，也能跑完、也多 1 个导出、也不留标签 |
| 坏端点 | 退出非零；stderr 含 `CDP` 与 `--remote-debugging-port`；5.1 s 内放弃（不挂死） |
| 坏端点不留残局 | `stats --json` 里没有 `pending`；记录下来的任务是 `failed`；站点下载数不变 |


## 5. 指标采集

指标从 `events.jsonl` + SQLite `tasks` / `steps` 读出，不额外埋点。下表**粗体是当前已采集**的，
其余随对应阶段补：

| 指标 | 来源 |
|---|---|
| **任务成功率** | `tasks.state` + `task.finished` 事件 |
| **模型调用数** | `tasks.model_calls`：`ModelRouter` 的尝试计数，任务每步结束后写入。A/C 组恒 0，B 组 1–2 |
| **失败动作率** | `action.executed.ok == false` ÷ `action.executed` 总数，来自 `bhx stats` |
| **步骤耗时 / 任务耗时** | `steps.elapsed_ms`、`tasks`（CLI 输出的 `elapsed_ms`） |
| 观察延迟 | `observe` 的耗时（trace 内） |
| **取消延迟** | `task.cancel` 请求 → `task.finished(canceled)` 的时间差（Phase 1 实测 85 ms / MCP 82 ms） |
| 残留输入 | 取消后对页面调用 `document.activeElement` / 键盘状态探针；由 chaos 站点提供 `__input_state` |
| **恢复率** | 注入故障或 kill -9 的用例中最终 `state == succeeded` 的比例 |

当前 `run-phase2.sh` 直接打印逐项 PASS/FAIL、失败原因和每个用例的 `elapsed_ms`，
不写 `target/report.json`——单个套件的结果还不够稳定，做成报告文件只会让数字看起来比实际更权威。
等 A/B/C 三组齐全、需要跨版本比较时再落报告格式。

## 6. 模型在测试里怎么办

- **A/C 组断言模型调用 ≤ 1**（实际恒 0，C 组规格里根本不配模型）。
  超出即失败——那说明计划或控制器靠模型兜底掩盖了问题。
- **B 组必须走视觉路径**，预算 2：点击 1 次、拖拽 2 次（起点 + 落点）、改道 1 次。
  实际次数从 `task show --json` 的 `model_calls` 读，没有适配器时 `ModelRouter`
  不扣预算并直接报错，所以「配置丢了」不会被算成「模型没用上」。
- **视觉模型走确定性替身。** `testkit/vlm.py` 不解码语义，只做像素颜色扫描，
  因此 B 组只验证 harness 的几何与验证链路。真模型的准确率不在 CI 的声明范围内。
- **规划 / 校验模型也走确定性替身。** `testkit/planner.py` 不读目标、不推理，只按状态里的
  URL 返回固定工作流；`testkit/verifier.py` 只看状态里有没有「导出 CSV」；
  `testkit/laya-stub.py` 按 state 里的标记选边。规划组因此只能证明请求形状、步骤校验、
  凭据占位符、执行接线是对的；校验组只能证明两选一、阈值与 `uncertain` 语义是通的；
  Laya 组只能证明能力边界、适配器顺序、进程生命周期与容错。「模型能否为任意目标写好计划」
  或「本地模型在这个站点上判得准不准」不在 CI 的声明范围内，留非 CI 抽查。
  录制回放（首次记录请求/响应、之后重放）还没做：当前没有真实模型参与 CI，
  回放只会多一层需要维护的样本。

> Phase 3/4 现状：`bh-model` 已实现 `Plan` / `Locate` / `VisualLocate` / `Verify` / `Repair`。
> 没有任何适配器时视觉步骤、规划与校验都会明确失败（`no model adapter configured`），
> 不会退回猜坐标、用规则假装规划，或把「没问模型」算成通过。

## 7. CI

```yaml
- cargo test --workspace                    # 单元测试，无浏览器
- ./testkit/run-phase1.sh                   # 自带 testkit server
- ./testkit/run-phase2.sh
- ./testkit/run-phase3.sh                   # 自带 testkit server + vlm.py 替身
- ./testkit/run-phase4.sh                   # 自带 testkit server + bhx serve
- 失败时保留并上传 mktemp 目录里的任务 home（events.jsonl、obs/、shots/）
```

失败时必须保留任务目录（`events.jsonl`、`obs/`、`shots/`），否则无法定位。

## 8. 回归纪律

每次修 bug 都补一个用例，并且**先确认它在修复前失败**。没有失败过的测试不算证据。
