# my-browser-harness

**Browser Harness：面向 AI Agent 的通用浏览器执行与交互控制基础设施。**

它不替 Agent 思考，而是替 Agent 把「观察 → 定位 → 操作 → 验证 → 恢复」这件事做稳。
游戏只是它的一种负载，不是它的定义。

```
AI Agent  ──►  Harness  ──►  Chromium (CDP)
 (想清楚)      (跑得稳)       (真实浏览器)
```

## 三个核心价值

| 价值 | 含义 | 对应能力 |
|---|---|---|
| 工作能力 | 可靠地完成多步骤网页任务，产出文件与数据 | Task Engine + DOM Executor |
| 执行能力 | 同一任务可用 DOM、视觉或混合路径完成 | Task Router + Vision Executor |
| 实时能力 | AI 不在线时，也能按已批准的策略连续动作并反馈调整 | Interactive Controller |

## 文档

| 文档 | 内容 | 读者 |
|---|---|---|
| [docs/requirements.md](docs/requirements.md) | 产品定义、能力模型、非目标、安全、验收标准 | 所有人 |
| [docs/architecture.md](docs/architecture.md) | 分层架构、执行器、任务引擎、技术选型、风险 | 架构 |
| [docs/protocol.md](docs/protocol.md) | 任务 / 动作 / 观察 / 事件 / 模型调用协议 | 实现 |
| [docs/interfaces.md](docs/interfaces.md) | crate 之间的 trait 与不变量 | 实现 |
| [docs/walkthrough.md](docs/walkthrough.md) | 一次真实任务从入口走到底 | 所有人 |
| [docs/data-model.md](docs/data-model.md) | SQLite 表、目录布局、配置、恢复 | 实现 |
| [docs/testing.md](docs/testing.md) | 测试场、chaos 注入、套件、指标 | 测试 |
| [docs/roadmap.md](docs/roadmap.md) | Phase 0–6 交付物与退出条件 | 项目管理 |
| [docs/benchmarks.md](docs/benchmarks.md) | 实测延迟（本机 M5），含 Laya 基线 | 所有人 |

## 快速开始

```bash
cargo build --release

# 看页面：输出元素索引（e1, e2…）与状态文本
./target/release/bhx observe --url https://example.com

# 跑步骤脚本：登录 → 筛选 → 导出 CSV（跑完即走，不落任务状态）
./target/release/bhx run testkit/sites/admin/task.json

# 作为任务跑：落 SQLite、检查点、产物、审批，可中断可恢复
./target/release/bhx task run testkit/sites/admin/task.json

# 持续交互：30 Hz 规则策略（模型不在回路里）
./target/release/bhx task run testkit/sites/game/c1-move.json

# 视觉定位：指向任意 OpenAI 兼容的视觉模型端点
# （testkit/vlm.py 是确定性替身，只用于验收 harness 这一侧）
python3 testkit/server.py --port 8766 &
python3 testkit/vlm.py --port 8790 &
BH_VLM_URL=http://127.0.0.1:8790/v1/chat/completions \
  ./target/release/bhx task run testkit/sites/canvas/b2-no-semantics.json

# 规划：给一个目标，模型看当前页面并给出步骤，校验后直接跑
# （testkit/planner.py 是确定性替身，只证明 harness 这一侧）
python3 testkit/planner.py --port 8791 &
BH_LLM_URL=http://127.0.0.1:8791/v1/chat/completions \
  ./target/release/bhx plan --goal "登录后台，筛选失败订单，导出 CSV" \
    --url http://127.0.0.1:8766/ --out plan.json --run \
    --cred username=admin --cred password=admin123
# plan.json 里只有 ${cred:username} 占位符；密码不落盘

# 或者让引擎自动规划：steps 为空 + mode=auto + start_url
# 引擎先用同一个 Observer 观察该页，规划一次，把计划写回 task.json 后再执行
# （下面仍用 planner.py 替身；换成真模型只改 BH_LLM_URL）
BH_LLM_URL=http://127.0.0.1:8791/v1/chat/completions \
  ./target/release/bhx task run testkit/sites/admin/auto-task.json \
    --cred username=admin --cred password=admin123
# 规划失败（无模型/坏提案/越界导航）时任务直接 failed，不派发任何步骤

# 任务末尾的语义校验（可选）：机械条件全过后再问一次模型
# 在 task.json 的 policy 里加上：
#   "semantic_verify": { "threshold": 0.7, "on_uncertain": "wait_human" }
# not_achieved 让任务失败但保留步骤 verified；uncertain 默认等人审批
# 加 "include_screenshot": true 会把视口 PNG 一并交给视觉模型；图片存为
# artifacts/verify-final.png，任务摘要里记 sha256。纯文本模型（Laya）会明确拒绝

# 本地决策模型（可选）：Laya sidecar 只答 Locate / Verify 这类选择题，
# 不起远程请求。默认排在远程模型之后兜底；量过准确率后可以把它放前面。
# BH_LAYA_DECIDE=sidecars/laya/decide.py \
# BH_LAYA_PYTHON=~/.local/share/laya-demo/.venv/bin/python \
# BH_LAYA_PRIORITY=first \
#   ./target/release/bhx task run testkit/sites/admin/task.json
# 零样本在本仓库的中文后台页上判错（见 docs/benchmarks.md）——先量再用

# 步骤失败后的自动修复（仅 read_only、且不需要审批）：在步骤里写
#   "retry": { "max_attempts": 1, "backoff_ms": 100, "on_exhausted": "ask_model" }
# 模型只拿到失败步骤、错误和当前页；改 id、执行器、成功/前置条件、升高副作用、
# 新增脚本动作或越出 allowed_hosts 的替换步骤会被拒绝，且只派发一次

./target/release/bhx task ls
./target/release/bhx task show <task-id>
./target/release/bhx task resume <task-id>      # 崩溃或审批后继续
./target/release/bhx task approve <task-id>

# 运行历史与 trace：数字从 store 复算，动作序列按步骤记录（不记载荷，密码不入事件）
./target/release/bhx stats                      # 成功率、耗时 p50/p95、模型调用、失败动作率
./target/release/bhx stats --json
./target/release/bhx trace <task-id>            # 事件时间线 + 步骤 + 产物 sha256
./target/release/bhx trace <task-id> --out ./trace-<task-id>   # 自包含包：trace.json + trace.html + artifacts/
# 打开 trace-<task-id>/trace.html：步骤卡 + 事件时间线 + 截图 + 按真实间隔回放（空格暂停、可拖动）
# 全序列截图：任务 policy 里加 "trace_screenshots": true（失败步骤无论如何都会截一张）

# 定时执行：写进 spec 的 ${cred:password} 只存名字，值在运行时从环境读
./target/release/bhx schedule add task.json --every 30m
./target/release/bhx schedule ls                # 间隔、下次运行、上次状态/任务
./target/release/bhx schedule run               # 常驻；--once 只跑一趟（CI 用）
./target/release/bhx schedule run --concurrency 4  # 最多 4 个不同调度同时跑（默认 1，上限 8）
BH_CRED_PASSWORD=... ./target/release/bhx schedule run --once

# 远端浏览器：不自己起 Chrome，而是连一个已经在跑的（http:// 或 ws:// 端点）
# 只关自己开的标签，不杀进程；端点不通 5 s 内失败，不会偷偷改回本地启动
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 --user-data-dir=/tmp/bhx-profile about:blank &
BH_CDP_URL=http://127.0.0.1:9222 ./target/release/bhx task run testkit/sites/admin/task.json

# REST + WebSocket 入口（非 MCP 的 Agent 用这个）
./target/release/bhx serve --addr 127.0.0.1:8765 &
curl -s localhost:8765/v1/health
# 事件流：GET /v1/tasks/<id>/events/ws?after=<seq>，断线续传
# （先在 8766 起 testkit server，命令见上面的视觉一节）
python3 testkit/rest-smoke.py --api http://127.0.0.1:8765 --site http://127.0.0.1:8766

# 页面级控制（HTTP 版 browser_*）：先建会话，再导航/观察/动作/截图
curl -s -XPOST localhost:8765/v1/sessions -H 'Content-Type: application/json' \
  -d '{"allowed_hosts":["127.0.0.1"]}'
# -> {"session_id":"…","url":"about:blank"}
curl -s -XPOST localhost:8765/v1/sessions/<id>/navigate -H 'Content-Type: application/json' \
  -d '{"url":"http://127.0.0.1:8766/admin/"}'
curl -s -XPOST localhost:8765/v1/sessions/<id>/observe -H 'Content-Type: application/json' \
  -d '{"goal":"登录"}'
python3 testkit/session-smoke.py --api http://127.0.0.1:8765 \
  --site http://127.0.0.1:8766 --home ./.bhx

# 作为 MCP 服务接到 Claude Code / 其它 Agent
./target/release/bhx mcp --home ./.bhx
```

任务目录：`./.bhx/tasks/<id>/{task.json,events.jsonl,artifacts/,obs/,shots/}`；
`task.json` 落盘前剥离凭据，事件同时进 SQLite 与 `events.jsonl`。

MCP 注册（项目根 `.mcp.json` 或用户级配置）：

```json
{ "mcpServers": { "bhx": { "command": "/abs/path/to/bhx", "args": ["mcp"] } } }
```

两层工具，按「Agent 是否已经知道要做什么」区分：

| 层 | 工具 | 何时用 |
|---|---|---|
| 底层（自己决策） | `browser_navigate`、`browser_observe`、`browser_execute_actions`、`browser_check_condition`、`browser_verify`、`browser_evaluate`、`browser_screenshot`、`browser_close` | 还在探索、边看边决定 |
| 任务层（交给引擎） | `task_run`、`task_status`、`task_list`、`task_artifacts`、`artifact_list`、`artifact_metadata`、`artifact_fetch`、`task_resume`、`task_approve`、`task_cancel` | 工作已明确，要落状态、可恢复、能审批 |
| 实时层（交给控制器） | `interactive_start`、`interactive_status`、`interactive_update_policy`、`interactive_stop` | 要连续交互，且中途可能改主意 |

`interactive_start` 不等结果：控制器在后台按 `tick_hz` 跑，`update_policy` 在 tick
边界换规则（版本必须更大，截止时间只能收紧），`stop` 释放输入后返回最终状态。

`browser_evaluate` 默认在 isolated world 执行（能读 DOM，读不到页面 `window.*`）；
需要页面全局时显式传 `"world": "main"`。

`browser_run_task` 仍在（引擎的步骤执行器直接挂当前会话），但需要持久化时用 `task_run`。

## 代码

| 路径 | 内容 |
|---|---|
| `crates/bh-core` | 协议类型：任务/步骤/动作/定位器/条件/观察，以及错误 |
| `crates/bh-cdp` | 薄 CDP 客户端：websocket、请求/响应、事件广播 |
| `crates/bh-browser` | 浏览器生命周期、会话、导航、输入、下载、弹窗 |
| `crates/bh-observe` | AX 树 → 元素索引（预算、可见性、密码脱敏、哈希） |
| `crates/bh-action` | 定位解析、可执行性检查、动作分发、条件与等待、取消 |
| `crates/bh-exec` | 步骤执行器（前置条件、重试、成功条件轮询、`fail_when`、产物）、视觉执行器、交互控制器 |
| `crates/bh-model` | 模型层：能力契约（Plan / Locate / VisualLocate / Verify / Repair）、调用预算、提案校验、OpenAI 兼容适配器、可选的 Laya sidecar 适配器（不依赖特定模型） |
| `crates/bh-task` | 任务引擎：SQLite 状态机、检查点、审批、产物索引、崩溃恢复、凭据剥离、空步骤自动规划 |
| `crates/bh-server` | MCP stdio 服务 + REST/WS 入口（`bhx serve`），事件流可从存储续传 |
| `crates/bhx` | CLI：`observe` / `run` / `task` / `plan` / `serve` / `mcp` |
| `testkit/` | 本地测试站点与服务器（`server.py`）、验收脚本（`run-phase1.sh`–`run-phase4.sh`、`mcp-smoke.py`、`arbiter-smoke.py`、`decide-smoke.py`、`rest-smoke.py`、`plan-smoke.py`、`auto-plan-smoke.py`、`semantic-smoke.py`、`laya-smoke.py`、`interactive-smoke.py`、`session-smoke.py`、`repair-smoke.py`、`trace-stats-smoke.py`、`schedule-smoke.py`、`remote-smoke.py`、`dino-smoke.py`、`agent-acceptance.sh`、`check.py`）、模型替身（`vlm.py`、`planner.py`、`verifier.py`、`repairer.py`、`chooser.py`、`laya-stub.py`） |
| `sidecars/laya/decide.py` | 本地决策模型 sidecar（torch/MPS，含 ready 握手） |
| `spike/` | Phase 0 一次性验证代码（自建 CDP 客户端 + 延迟基线），不进入产品 |

## 一句话验收

> 给定一个真实的后台工作任务（登录 → 筛选 → 导出 CSV），它能连续三次独立完成，
> 全程模型调用 ≤ 2 次，中途注入弹窗与接口超时仍能恢复。

## 状态

设计定稿（v0.2）。**Phase 0 ✅**（2026-10-28）：自建 CDP 客户端验证通过，单动作 p95 17.2 ms。
**Phase 1 ✅**（2026-10-28）：Browser Runtime + 最小 MCP。证据：

| 项 | 结果 |
|---|---|
| 真实工作流（登录 → 筛选 → 导出 CSV） | 连续 3 次成功，202 / 271 / 218 ms，0 次模型调用 |
| 注入 `alert` 后继续 | 自动处理后页面仍可交互，任务 733 ms 完成 |
| MCP 端到端（导航 → 观察 → 点击 → 确认） | 通过（`testkit/mcp-smoke.py`） |
| 取消生效时间 | 85 ms（要求 < 200 ms），取消后无残留按下状态 |
| 密码字段 | AX 值被抑制，强行显示时也输出 `***` |

验收脚本：`./testkit/run-phase1.sh`（6/6 通过）。

**Phase 2 ✅**（2026-10-28）：任务引擎。证据（`./testkit/run-phase2.sh`，14/14 通过）：

| 项 | 结果 |
|---|---|
| A 组 5/5 | 提取结构化数据、多步表单、条件导出、跨站点 6 步、弹窗 + 接口 500 恢复 |
| 失败尝试早退 | `fail_when` 命中页面错误信号，A5 从 9.4 s 降到 1.3 s |
| 每任务模型调用 | 0（要求 ≤ 2） |
| 产物比对 | 导出 CSV 与 `/admin/__state` 记录逐字节一致（sha256 相同） |
| 断言来源 | 服务端 `/__state`、`/__downloads`，不是页面自述 |
| 崩溃恢复 | `kill -9` 落在第 4 步 → `waiting_approval` → 审批 → 只重跑第 4 步并完成 |
| 恢复后的产物 | 崩溃前步骤的 CSV 仍在任务产物里 |

默认没有模型驱动：Phase 2 的任务仍是脚本化的（`task.json` 的步骤），模型规划在 Phase 4。
Agent 可以现在就用 MCP 的 `browser_*` 自己决策，或用 `task_*` 把确定的工作交给引擎。

**Phase 3 ✅**（2026-10-28）：混合执行。交互控制器以 30 Hz 运行规则策略，模型不在回路里；
视觉步骤走「截图 → 模型提案 → 归一化 bbox → CSS 坐标 → 动作 → 状态验证」，DOM 定位失败时
可按步骤的 `fallback_visual` 就地改道。证据：`./testkit/run-phase3.sh`（C1–C4 + B1–B6，41/41 通过）：

| 项 | 结果 |
|---|---|
| C 组 4/4 | 按住/释放按键、画布坐标点击、状态变化后换动作、超时释放全部输入 |
| B 组 6/6 | 截图定位（有/无语义）、拖拽落点误差 0.6 px（要求 ≤ 4 px）、状态判定、布局变化后重定位、DOM→视觉改道 |
| 每任务模型调用 | C 组 0；B 组 1–2（预算 2） |
| 坐标换算 | PNG 尺寸与 `viewport × dpr` 不符时拒绝点击，不猜 |
| 视觉模型 | `bh-model` 适配 OpenAI 兼容接口；B 组用确定性像素扫描替身，验证的是 harness 这一侧 |

视觉精度取决于接入的模型；本仓库的替身不衡量模型质量，只证明契约、几何和验证链路是通的。

**Phase 4 ⏳ 进行中**：REST + WebSocket 入口、显式规划与引擎内自动规划已可用
（`bhx serve`、`bhx plan`、空步骤 `task run`）。同一任务可以从 HTTP 创建、轮询、
用 WebSocket 跟事件（`?after=` 断线续传），并下载产物核对 sha256。`bhx plan` 用真实
页面状态做输入，校验后的步骤可写成 spec 或直接运行；`steps=[]` + `mode=auto` +
`start_url` 的任务由引擎先观察该页、调用一次规划、把计划写回 `task.json` 后再执行，
规划调用计入任务预算，失败时零步骤派发。凭据只在内存，写盘、steps 表与事件里都只有
`${cred:…}` 占位符。MCP 增加了 `browser_verify`（第二意见：`achieved` /
`not_achieved` / `uncertain`）、`artifact_list` / `artifact_metadata` / `artifact_fetch`
（产物读取限定任务目录，UTF-8 原文或 base64，5 MiB 上限）。任务可在末尾声明
`policy.semantic_verify`：机械条件全过后再判一次目标，`not_achieved` 让任务失败但保留
步骤 `verified`，`uncertain` 默认转任务级人审（批准后不重跑步骤、不再调模型）。
`include_screenshot: true` 会把当时的视口 PNG 一起发给校验模型，并把同一份字节存成
`artifacts/verify-final.png`——截图就是屏幕上的一切，这个开关默认关闭。
失败步骤可以在 `retry.on_exhausted=ask_model` 下交给模型修复，但只有 `read_only`
且无需审批的步骤允许；替换必须保留 id、成功/前置/失败条件、执行器与副作用等级，
不能新增脚本动作或越出 `allowed_hosts`，并且只派发一次。
本地模型：`BH_LAYA_DECIDE` 指向 `sidecars/laya/decide.py` 时，Laya 作为
`Locate` / `Verify` 的适配器接入——一次 ready 握手的常驻进程、进程内共享、
超时即杀并重启。默认排在远程模型之后；零样本在中文后台页上的两次关键判定都是错的
（[benchmarks.md](docs/benchmarks.md)），所以「本地优先」要显式开。

交互作业走 `interactive_*`：控制器在自己的 engine 连接与浏览器里后台运行，
`interactive_start` 立刻返回；`interactive_update_policy` 只采纳严格更大的
`policy_version`，在 tick 边界生效，且新策略只能收紧截止时间；`interactive_stop`
释放全部输入、留一个页面上报周期（350 ms）再关闭浏览器。
REST 的页面级接口是 MCP `browser_*` 的镜像：`POST /v1/sessions` 建会话（可给
`allowed_hosts`），再按 `session_id` 导航、观察、执行动作、判条件、求值、截图、关闭；
每个会话独立浏览器与下载目录，批次失败返回 422 + 已执行到哪一步的日志。
证据：`./testkit/run-phase4.sh` 十三组全绿——`rest-smoke.py` 22/22（创建 → 状态 →
12 条事件 → CSV 产物 → 非 auto 空步骤 422 / auto 空步骤 202 后诚实失败 → 重复 id
409 → 取消），`session-smoke.py` 47/47（建会话 → 导航 → 观察 → 输密码（观察里是 `***`）
→ 条件 → JS 双世界 → 白名单拒绝批次 → 下载落会话目录且哈希一致 → 双会话独立 → 关闭），
`trace-stats-smoke.py` 45/45（成功/失败两次运行 → 统计口径 → 7 条动作事件 → 每步截图 →
trace 包与站点同哈希 → 密码不入 trace），`schedule-smoke.py` 61/61（`${cred:password}` 名字入库
→ 缺值拒跑且站点零下载 → 带值跑出真实 CSV → 失败后不盲重试 → 禁用/启用/删除 → 常驻模式
→ 默认串行、`--concurrency 2` 真并行 → 同一调度重叠槽位记为 `skipped_overlap`），
`remote-smoke.py` 17/17（自起 Chrome → `BH_CDP_URL=http://…` 跑完 → 只关自己开的标签、
浏览器仍活着 → `ws://…` 端点同样跑完 → 坏端点 5 s 内失败且不留 pending 任务），
`plan-smoke.py` 17/17（1 次规划调用 → 3 步校验 → 执行 → CSV 与站点
记录同哈希 → 密码不在 spec/task.json 里），`auto-plan-smoke.py` 88/88（观察
start_url → 计划落盘 → 执行 → 八类拒绝路径 → 磁盘/DB/事件无密码），
`semantic-smoke.py` 62/62（achieved / not_achieved / uncertain 审批恢复 / 策略失败 /
无适配器 / 坏阈值 → 截图与模型收到的字节同哈希 → 不开就不发、不落盘 → 回放器里能看到），
`laya-smoke.py` 80/80（本地判定 → 审批 → 远程优先/回落 → 坏 choice 被拒 → 配置错误 →
截图被诚实拒绝 → MCP 两次调用一个进程），`interactive-smoke.py` 42/42
（后台启动 → 状态 → 页面确认按键被按住 → 热更新目标 → 陈旧版本被拒 → 停止无粘键 →
谓词每 tick 一次往返 → 30 s soak 909 tick、p95 34 ms、CPU 3.6%、无粘键），
`repair-smoke.py` 112/112（正常修复 → CSV 同哈希 / 修复后仍失败 / 非 read_only 不修 /
九类违规提案被拒），`arbiter-smoke.py` 48/48（不启用时行为不变 → 两个同名按钮**不问模型直接放弃**
→ 标签可区分时才仲裁、站点记录证明按的是被选中的那个 → 低于阈值/假 id/模型不可用/候选过多都退回原歧义错误
且说明原因 → 每次仲裁都写进 `action.executed.arbitration` → 坏策略开工前拒绝），
`decide-smoke.py` 29/29（模型每 1 s 选一次策略、规则照常 30 Hz 执行；**同一套规则**下
模型选不同策略 → 站点记 **3 分 vs 0 分**；不启用即零调用；没配模型也能跑完且 `decisions: 0`；
阈值/假 id/预算/坏策略各按契约处理）；`mcp-smoke.py` 覆盖 Verify 三态与产物读取。
Phase 5 的第一个增量也落地了：目标与所有规则谓词合成一次 `Runtime.evaluate`，
`interactive.stopped` 报告 `evals` 与 tick 间隔分位数；延迟预算见
[benchmarks.md](docs/benchmarks.md#phase-5-延迟预算2026-10-28)。
Phase 6 的运行历史也开了头：每个动作写一条 `action.executed`（不记载荷），
`bhx stats` 从 store 复算成功率/耗时分布/失败动作率，`bhx trace` 输出事件时间线
与可带走的 trace 包；包里的 `trace.html` 是单文件回放器（步骤、动作耗时、事件详情、产物直链），`file://` 直接能开。
每步的截图默认只截失败步骤；在 policy 里写 `"trace_screenshots": true` 会截全序列，
回放器里直接显示——注意截图就是页面当时的样子，分享 trace 包等于分享屏幕上的一切。定时执行也落地了：`bhx schedule add/ls/rm/run` 把任务快照存进
SQLite，槽位先消费再运行（失败不会变成重启后的盲重试），凭据只存名字、
值在运行时从 `BH_CRED_<NAME>` 读；缺值直接拒跑，不会把占位符敲进页面。
默认一次只跑一个调度，`--concurrency N`（1..=8）让不同调度并行；每个运行自带
`Engine`，同一调度的重叠槽位记成 `skipped_overlap`，不会排队补跑。
远端浏览器也接上了：`BH_CDP_URL` 指向一个已在运行的 Chrome（`http://host:port` 或
`ws://…`），`bhx` 就不再本地启动，只在自己退出时关掉它建的标签，失败时不回落到本地。
跨 Agent 验收已跑通两方：Claude Code（10 s）与 Pi + 本地 Qwen3.8（16–22 s）各自通过
`bhx mcp` 用 `task_run` / `task_status` 完成同一任务，导出的 CSV 与站点记录同哈希
（`2dd0b48d…`）。第三个 Agent 受本机环境阻塞（Codex 账号无可用模型、OpenCode 启动挂起），
未完成；复跑用 `./testkit/agent-acceptance.sh claude|pi`，数字见 [benchmarks.md](docs/benchmarks.md)。

## 命名

仓库沿用 `my-browser-harness`。本机已存在 browser-use 的 `browser-harness` CLI
（`~/.local/bin/browser-harness`），两者不是同一个东西；本项目二进制定为 **`bhx`**，
避免 PATH 冲突与文档混淆。详见 [docs/architecture.md](docs/architecture.md#8-命名)。
