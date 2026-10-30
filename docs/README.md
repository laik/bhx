# 文档索引

> 深入文档目前是中文。想读英文请先看 [README.md](../README.md)；
> 把 `docs/` 翻译成英文是很好的第一个 issue（见 [CONTRIBUTING.md](../CONTRIBUTING.md)）。

## 按目的找

| 我想… | 读这个 |
|---|---|
| 跑起来、看懂第一个任务 | [getting-started.md](getting-started.md) |
| 明白它到底是什么、不做什么 | [requirements.md](requirements.md) |
| 用它的接口 | 任务规格：[protocol.md §1–6](protocol.md)；MCP：[protocol.md §10](protocol.md)；REST/WS：[protocol.md §10](protocol.md)；CLI：`bhx --help` |
| 改它的实现、加一个能力 | [architecture.md](architecture.md) → [interfaces.md](interfaces.md) → [data-model.md](data-model.md) |
| 追一次真实调用 | [walkthrough.md](walkthrough.md) |
| 测它、加测试 | [testing.md](testing.md) |
| 看它有多快 / 数字从哪来 | [benchmarks.md](benchmarks.md) |
| 看它做到哪了、下一步 | [roadmap.md](roadmap.md)、[../CHANGELOG.md](../CHANGELOG.md) |
| 提交什么改动算安全 | [interfaces.md §9 不变量汇总](interfaces.md) + [testing.md §8 回归纪律](testing.md) |

## 按顺序读（第一次接触本项目）

1. [requirements.md](requirements.md) — 定义、能力模型、非目标。**特别是 §2 非目标与 §8 失败的定义**：
   它们解释了后面所有看起来绕远的设计。
2. [architecture.md](architecture.md) — 为什么是三个执行器、为什么不用 Playwright。
3. [walkthrough.md](walkthrough.md) — 一次真实任务从入口走到产物，把上面的图对上代码。
4. [protocol.md](protocol.md) — 对外契约（任务 JSON、事件、模型调用）。**这是规范性文档**：
   与代码不一致时，以协议为准，代码要改，或在文档里说明为什么例外。
5. [interfaces.md](interfaces.md) / [data-model.md](data-model.md) — 内部契约与存储，改实现前必读。
6. [testing.md](testing.md) — 测试场与断言口径（断言来自服务端真相，不是页面自述）。
7. [roadmap.md](roadmap.md) / [benchmarks.md](benchmarks.md) — 做到哪了、代价是什么。

## 每份文档的性质

| 文档 | 性质 | 过期了吗 |
|---|---|---|
| requirements.md | 规范 | v0.2，2026-10-28 |
| architecture.md | 设计说明（含决策理由） | v0.2；§5 的目录树已与仓库同步（标「建议」的段落是当初的提案，落地结果紧跟其后） |
| protocol.md | **规范**（线上契约 v0.1） | 已实现部分与代码同步；「尚未实现」的小节是提案 |
| interfaces.md | 规范（内部契约） | 与代码同步 |
| data-model.md | 描述实现 | 与代码同步（§7 的配置文件尚未实现，当前用环境变量） |
| walkthrough.md | 教学 + 落地对照 | §9 是真实执行对照 |
| testing.md | 操作手册 | 与 `testkit/` 同步 |
| benchmarks.md | **实测记录**（带日期与机器） | 每个数字标了日期与环境，不随代码更新，只追加 |
| roadmap.md | 计划 + 落地结果 | 历史部分不删，只标 ✅/⏳/⏸ |
| history/original-goals.md | 开工前的一页手写目标 + 后来的对应关系 | 归档，不是定义 |

> benchmarks.md 与 roadmap.md 里的日期是真实测量/完成日期。
> 数字过期了不要改写它，另起一节加新 measurement——保留旧数字才能看出回归。

## 什么不在文档里

- 完整 CLI 用法：`bhx --help`（真相在 `crates/bhx/src/main.rs` 的 usage 里）
- 每个 Rust 函数的签名：`cargo doc --open`
- 未落地且不打算做的事：见 [requirements.md §2 非目标](requirements.md) 与
  [roadmap.md「明确推迟」](roadmap.md)
