<!--
If this changes the wire protocol, the SQLite schema, or the non-goals list,
open an issue first (see CONTRIBUTING.md).
-->

## What

<!-- 1–3 sentences: what changes, and which problem it solves. -->

## How it is verified

- [ ] `cargo test --workspace`
- [ ] affected acceptance suite(s): `./testkit/run-phaseN.sh`
- [ ] new test fails **before** the fix (paste the failure below)

```
# paste the failing output here, or "no new test: docs/typo only"
```

## Measurements (only for performance / model-path changes)

| | before | after |
|---|---|---|
| | | |

Machine / command:

## Docs

- [ ] `docs/protocol.md` / `docs/interfaces.md` updated in this PR (required if behaviour changed)
- [ ] `docs/benchmarks.md` gets a **new dated section**, existing numbers untouched
- [ ] `docs/roadmap.md` status markers updated
