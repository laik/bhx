# Phase 0 spike

一次性代码，不进入产品。存在的唯一理由是量出真实延迟，并证明自建薄 CDP 客户端够用。

```bash
cargo run --release
```

需要本机有 Chrome（路径写死在 `src/main.rs` 的 `CHROME`）。程序会：

1. 以 `--headless=new` 起一个独立 profile 的 Chrome（端口 9333）。
2. 打开一个页面 target，直连它的 WebSocket。
3. 生成一个小页面和一个 1000 行的表格页。
4. 测量 CDP 往返、AX 树、元素索引、盒模型、鼠标/键盘分发、截图、端到端点击。
5. 打印 Markdown 表格，直接贴进 `docs/benchmarks.md`。

结果与结论见 [../docs/benchmarks.md](../docs/benchmarks.md#cdp-基线phase-0-spike2026-10-28)。

## 它证明了什么

- 自建 CDP 客户端（约 350 行）够用，不需要 `chromiumoxide` 或 Playwright。
- CDP 往返 0.1–0.5 ms，不是瓶颈。
- `mouseMoved` 单次 16.6 ms，是点击成本的 97%；press+release 只要 0.4 ms。
- 大页面 AX 树 4.9 MB / 13422 节点 / 103 ms，观察必须做预算和差分。

## 它没做什么

- 没有事件订阅（CDP 事件被丢弃），正式实现需要。
- 没有 frame/iframe 处理，只测了主 frame。
- 没有错误分类、重试、取消。
- 用的是直连 page target，不是 browser 连接 + session 路由。

这些都是 Phase 1 的事，不该在 spike 里解决。
