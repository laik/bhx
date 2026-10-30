# 最初的设计目标（2026-10，未整理）

> 这是项目开工前的一页手写目标，原样保留（文件名原本是 `目标.md`，放在仓库根）。
> 它的价值在于说明这个项目是从哪三个念头开始的——**不是**当前的定义。
> 现在的有效定义是 [requirements.md](../requirements.md)（规范）与
> [roadmap.md](../roadmap.md)（做到哪了）。

1. 通过决策模型减少大模型（LLM）的替代一些简单交互决策能力。
2. 可以变成 mcp/skills 运行在 mac
3. 验证：
    3.1： 设计一些网页，可以填表单，预期输出
    3.2： 可以玩一些页面游戏，例如小恐龙，打飞机，马里奥，冒险岛游戏；
4. 添加调用轨迹跟踪

## 后来变成了什么

| 原始念头 | 落地 |
|---|---|
| 用决策模型替掉简单交互决策 | `bh-model` 的能力契约 + 调用预算；可选的本地决策模型 sidecar（`sidecars/laya/`）。实测零样本精度不足，所以默认路径仍然是零模型调用——见 [benchmarks.md](../benchmarks.md) 的 Laya 各节 |
| MCP / skills，跑在 mac | `bhx mcp`（stdio）、`bhx serve`（REST/WS）、`.claude/skills/bhx/SKILL.md`。macOS 实测；Linux 的 CI 矩阵还没实跑 |
| 填表单、有预期输出的网页 | `testkit/sites/admin`、`form`、`canvas`，服务端记录真相（`/__state`）供断言 |
| 页面游戏 | `testkit/sites/game`、`dino`、`plane`；30 Hz 交互控制器。马里奥/冒险岛没做——它们是平台，不是能力 |
| 调用轨迹跟踪 | `events.jsonl` + SQLite 事件表 + `bhx trace`（含单文件回放器）+ `bhx stats` |

第 3.2 条里的「游戏」后来被明确成**一种负载，不是产品定义**：
游戏测的是实时控制与像素路径，而项目要交付的是可重复、可验证的网页工作。
这个取舍写在 [requirements.md §2 非目标](../requirements.md)。
