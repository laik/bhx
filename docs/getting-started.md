# 上手：第一个任务从头走到产物

> 所有命令都可以直接复制。前提：本机有 Chrome、Rust 1.89+、Python 3。
> 下面用 `BH=./target/release/bhx` 指代二进制；测试站点固定 `127.0.0.1:8766`
> （仓库里的任务 JSON 用的就是这个端口，套件脚本才会 `sed` 换端口）。

## 0. 构建 + 起测试站点

```bash
cargo build --release
BH=./target/release/bhx

python3 testkit/server.py --port 8766 &      # 只用标准库；提供站点 + 服务端真相
```

Chrome 不在默认路径时先设 `export BH_CHROME=/path/to/chrome`。
默认 headless；想看浏览器加 `--headed`。日志级别用 `RUST_LOG=info`。

## 1. 先看页面：拿到真实的元素索引

```bash
$BH observe --url http://127.0.0.1:8766/ --max-elements 8
```

```
URL: http://127.0.0.1:8766/
Title: 运营后台 · 订单
Controls:
e1  textbox "用户名"
e2  textbox "密码"
e3  button "登录"
```

**不要猜定位器**。观察输出里的 role/name 就是下一步要写的东西。
`--json` 给机器读，`--shot out.png` 顺带存一张视口截图。

## 2. 跑一个脚本：跑完即走，不落任务状态

任务 JSON 的最小形状（完整字段见 [protocol.md §1–5](protocol.md)）：

```json
{
  "protocol_version": "0.1",
  "kind": "workflow",
  "goal": "登录后台，筛选失败订单，导出 CSV",
  "policy": {
    "max_model_calls": 2, "max_wall_ms": 60000,
    "allowed_hosts": ["127.0.0.1"],
    "observation": { "max_elements": 120 }
  },
  "steps": [
    { "id": "s1", "goal": "登录运营后台",
      "actions": [
        { "type": "navigate", "url": "http://127.0.0.1:8766/" },
        { "type": "type", "target": { "role": "textbox", "name": "用户名" }, "text": "admin", "clear_first": true },
        { "type": "type", "target": { "role": "textbox", "name": "密码" }, "text": "admin123", "clear_first": true },
        { "type": "click", "target": { "role": "button", "name": "登录" } }
      ],
      "success": { "type": "element_visible", "locator": { "role": "button", "name": "导出 CSV" } },
      "timeout_ms": 20000 }
  ]
}
```

`success` 不是可选的装饰：没有它，这一步的结果就是「未验证」。

```bash
$BH run testkit/sites/admin/task.json
```

```
  s3         succeeded verified=true  85 ms  导出筛选结果为 CSV
artifacts in ./.bhx/artifacts/task:
  ./.bhx/artifacts/task/orders-失败.csv
```

零模型调用、约 0.2 秒。这就是「快执行在 Harness」的意思。

## 3. 同一份 JSON 作为任务跑：落状态、可恢复、能审计

```bash
$BH task run testkit/sites/admin/task.json --home ./.bhx \
    --cred username=admin --cred password=admin123
# SUCCEEDED in 706 ms  (task 01M65XT9X1…)   # 冷启动那次会到 ~1.4 s

$BH task ls --home ./.bhx
$BH task show <task-id> --home ./.bhx --json     # 每步状态、模型调用数、产物
$BH task artifacts <task-id> --home ./.bhx
```

落盘的东西在 `./.bhx/tasks/<id>/`：`task.json`（凭据已剥离，只剩 `${cred:…}` 占位符）、
`events.jsonl`、`artifacts/`（下载的文件与步骤截图都在这里），加上 `./.bhx/bhx.db` 里的
任务/步骤/事件/产物表。`obs/` 与 `shots/` 会建出来但当前是空的——截图改放 `artifacts/`
才有 sha256 与产物索引，理由见 [data-model.md §2](data-model.md)。

## 4. 出错了怎么办

**故障注入**（弹窗 + 接口 500，任务仍要完成）：

```bash
curl -s -XPOST http://127.0.0.1:8766/admin/__chaos -H 'Content-Type: application/json' -d '{
  "rules": [
    { "on": "click", "selector": "#export", "popup": "confirm", "message": "导出将包含 3 条记录，继续？", "times": 1 },
    { "on": "request", "path": "/admin/api/export", "status": 500, "times": 1 }
  ]
}'
$BH task run testkit/sites/admin/task-chaos.json --home ./.bhx
```

注入的规则必须真的 fire 过才算证据：查 `curl -s http://127.0.0.1:8766/admin/__state | python3 -m json.tool`
里的 `chaos_applied`。chaos 接口的完整语义见 [testing.md §3](testing.md)。

**进程被杀**：跑一个步骤更多的任务，中途 `kill -9`，然后

```bash
$BH task show <id> --home ./.bhx      # state = waiting_approval，中断点被记下来
$BH task approve <id> --home ./.bhx
$BH task resume <id> --home ./.bhx    # 只重跑被打断那一步，之前的产物还在
```

完整的自动化版本是 `testkit/run-phase2.sh` 的 R1（第 177 行起）。

**为什么失败**：

```bash
$BH trace <task-id> --home ./.bhx --out ./trace-<task-id>
open ./trace-<task-id>/trace.html      # 步骤卡 + 事件时间线 + 按真实间隔回放
$BH stats --home ./.bhx                # 成功率、耗时分位数、失败动作率、模型调用
```

trace 包里的截图就是当时屏幕上的一切 —— 分享出去之前想清楚。

## 5. 接给 Agent：MCP

```bash
$BH mcp --home ./.bhx
```

```json
{ "mcpServers": { "bhx": { "command": "/abs/path/to/bhx", "args": ["mcp"] } } }
```

工具分两层：还在探索就用 `browser_*`（观察、点击、判条件、求值、截图）；
工作已经明确就交给 `task_*`（落状态、可恢复、能审批）；
要连续交互就用 `interactive_*`（后台跑，不等结果）。清单与语义见
[protocol.md §10](protocol.md)。端到端例子：`python3 testkit/mcp-smoke.py`。

## 6. 脚本 / CI：REST + WebSocket

```bash
$BH serve --addr 127.0.0.1:8765 &
curl -s localhost:8765/v1/health
python3 testkit/rest-smoke.py --api http://127.0.0.1:8765 --site http://127.0.0.1:8766
```

事件流可断线续传：`GET /v1/tasks/<id>/events/ws?after=<seq>`。
页面级控制（HTTP 版的 `browser_*`）见 `python3 testkit/session-smoke.py`。

## 7. 要模型的时候才给它

四个入口都是「配置了就用，没配就明确失败」，绝不会假装规划过：

```bash
# 显式规划：给目标 → 模型看当前页 → 校验后直接跑（testkit/planner.py 是确定性替身）
python3 testkit/planner.py --port 8791 &
BH_LLM_URL=http://127.0.0.1:8791/v1/chat/completions \
  $BH plan --goal "登录后台，筛选失败订单，导出 CSV" --url http://127.0.0.1:8766/ \
       --out plan.json --run --cred username=admin --cred password=admin123
# plan.json 里只有 ${cred:username}；密码不落盘

# 引擎内自动规划：steps=[] + mode=auto + start_url
BH_LLM_URL=http://127.0.0.1:8791/v1/chat/completions \
  $BH task run testkit/sites/admin/auto-task.json --cred username=admin --cred password=admin123

# 视觉定位（OpenAI 兼容端点）
BH_VLM_URL=http://127.0.0.1:8790/v1/chat/completions $BH task run testkit/sites/canvas/b2-no-semantics.json

# 语义校验 / 自动修复：写在 task 的 policy 与 step 的 retry 里，见 protocol.md §8
```

换成真模型只改 `BH_LLM_URL` / `BH_VLM_URL`。
本地决策模型（可选）：`BH_LAYA_DECIDE=sidecars/laya/decide.py`，
零样本在中文后台页上判错过，**先量再用**（[benchmarks.md](benchmarks.md)）。

## 8. 定时与远端浏览器

```bash
$BH schedule add testkit/sites/admin/task.json --every 30m
BH_CRED_PASSWORD=admin123 $BH schedule run --once --home ./.bhx   # 凭据只从环境读
$BH schedule ls

"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 --user-data-dir=/tmp/bhx-profile about:blank &
BH_CDP_URL=http://127.0.0.1:9222 $BH task run testkit/sites/admin/task.json
# attach 模式只关自己开的标签，不杀进程；端点不通就失败，不会偷偷回到本地启动
```

## 9. 然后读代码

按依赖顺序读最省力：`bh-core`（协议即类型）→ `bh-cdp`（206 行）→
`bh-observe` → `bh-action` → `bh-exec` → `bh-task` → `bh-server` → `bhx`（CLI）。
每个 crate 的对外契约与不变量在 [interfaces.md](interfaces.md)。
`cargo test --workspace` 不需要浏览器，可以直接跑。

## 10. 常见坑

| 现象 | 原因 |
|---|---|
| `spawn /Applications/Google Chrome.app/…: No such file` | 设 `BH_CHROME`；Linux 上指向 chromium |
| `connection refused` 或步骤卡在 s1 | 测试站点没起，或端口不是 8766（套件用 `sed` 换，手跑就得起在 8766） |
| `no model adapter configured` | 该步骤需要模型（视觉/规划/校验/修复），没配 `BH_*_URL`。**这是故意的**：不退回猜坐标 |
| 步骤 `waiting_approval` | 不可逆动作或中断恢复需要人批准：`task approve` / `task reject` |
| 观察里没有密码值 | 密码字段一律脱敏（强行显示也是 `***`），要断言请查站点 `/__state` |
| 想清理所有状态 | 删 `.bhx/`（SQLite + 任务目录都在里面） |
