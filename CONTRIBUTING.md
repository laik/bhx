# Contributing

Thanks for reading this far. `bhx` is a small, opinionated engine — most of the
opinions are written down, so start with them.

- **Docs:** [docs/README.md](docs/README.md) (index and reading order)
- **Security issues:** see [SECURITY.md](SECURITY.md) — please don't open a public issue first
- **What to work on:** the [open gaps](README.md#status) and the [roadmap](docs/roadmap.md)

## Development setup

```bash
cargo build                 # debug build used by the suites
cargo test --workspace      # unit tests, no browser needed
./testkit/run-phase1.sh     # acceptance suites; each starts its own test server
./testkit/run-phase2.sh
./testkit/run-phase3.sh
./testkit/run-phase4.sh
cargo doc --open            # public API docs (the crates have doc comments)
```

Requirements: Rust 1.89+, Chrome/Chromium, Python 3 (stdlib only). Chrome outside
the default location: `BH_CHROME=/path/to/chrome`. The suites bind local ports
(8766/8770+ by default, overridable with `PORT=` / `APORT=`) and never touch the
public internet.

Everything is verified against the local sites in `testkit/sites/`, where the
server records the truth (`GET /<site>/__state`). Assertions come from that
record, never from "the page says success". If you change behaviour, the suites
have to disagree with you before the change is real.

## Invariants worth keeping

These are the reasons the code is shaped the way it is
([requirements.md §4](docs/requirements.md), [interfaces.md §9](docs/interfaces.md)):

1. **The model is not on the critical path.** A/B/C acceptance suites run with 0
   model calls; a step may use a model only at declared upgrade points, inside a
   per-task budget. If a test starts passing "because the model figured it out",
   the test is broken.
2. **Every action is verifiable.** No success condition ⇒ the step is reported as
   unverified. Don't add actions that silently skip verification.
3. **No silent fallback.** Missing model adapter, missing credential, bbox that
   doesn't match the viewport, an out-of-budget navigation: fail loudly with an
   error code. Falling back to guessing hides the bug and breaks recovery.
4. **Cancellation is a hard guarantee.** Cancel takes effect < 200 ms and leaves
   no keys or mouse buttons held. Any new input path must handle the cancel case.
5. **Side effects are graded.** `read_only` / `reversible` / `irreversible`;
   irreversible actions need approval. Don't widen what runs unattended.
6. **Secrets never reach disk or the model context.** Credentials stay as
   `${cred:name}` in specs, task rows and events; password field values are
   redacted in observations. Screenshots are the exception and are opt-in — say
   so in the PR if a change makes a screen capture happen somewhere new.
7. **Deterministic by default.** No humanising jitter, no random retries without
   a seed recorded in the events.

## Pull requests

- Small, one idea each. A PR that changes the wire protocol must say so in the
  title and update [docs/protocol.md](docs/protocol.md) in the same commit —
  that file is normative for the implementation, not a description of it.
- **Bug fixes ship with a test that failed before the fix.** Paste the failing
  output in the PR. A test that has never failed is not evidence
  ([testing.md §8](docs/testing.md)).
- Performance changes paste before/after numbers into
  [docs/benchmarks.md](docs/benchmarks.md) as a **new dated section** with
  machine and command — old measurements are never rewritten, that's how
  regressions become visible.
- New dependencies need a reason. The runtime deliberately has no CDP wrapper and
  no browser driver: `chromiumoxide`/Playwright were evaluated and rejected
  ([architecture.md §4](docs/architecture.md)). The test side deliberately has no
  Python dependencies.
- Format with `cargo fmt --all` on files you touch; keep `cargo clippy --workspace
  --all-targets` from gaining new warnings. The tree still carries legacy
  formatting and warnings — cleaning them up is welcome, but please as its own
  commit, not mixed into a behavioural change.
- Language: code comments and identifiers are English; `docs/` is currently
  Chinese. Translating a doc is a welcome PR — keep the technical content and the
  tables identical, and don't "improve" the numbers while moving them.

## Good first issues

- [ ] `cargo fmt --all` + clippy cleanup, one commit, no behaviour change
- [ ] Run the acceptance suites on Linux in CI (chromium headless) and fix what breaks
- [ ] English translation of `docs/requirements.md` (shortest) and `docs/getting-started.md`
- [ ] `GET /v1/events` global event stream ([protocol.md §10](docs/protocol.md) — listed as not implemented)
- [ ] `chaos` rules `on: layout` and `on: move` ([testing.md §3](docs/testing.md))
- [ ] `config.toml` ([data-model.md §7](docs/data-model.md) defines the field names; env vars only today)
- [ ] Packaging: crates.io release / Homebrew formula / prebuilt binaries in CI
- [ ] Protocol version negotiation between client and `bhx serve`
- [ ] Recording/replay for model traffic ([testing.md §6](docs/testing.md) explains why it isn't done yet)

Open an issue before starting anything that changes the protocol, the storage
schema, or the "non-goals" list — those need agreement first.

## Style

- Rust 2024 edition, no `unsafe` in the workspace. Keep it that way unless there
  is no alternative, and explain it.
- Errors are `BhError` with a documented code (`locator.*`, `action.*`,
  `task.*`, `model.*`, …) — never a stringly-typed panic.
- Crate boundaries are one-way (`bh-model` depends only on `bh-core`; executors
  never reach into the task engine). See the dependency graph in
  [architecture.md §5](docs/architecture.md).
