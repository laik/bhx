# plane — where a slow decision has a payoff

`decide-smoke.py` uses this page. A game is the only test site where "which option
should I take" is genuinely open: on a form or a table, the correct control follows
from the goal. Here it does not, and the choice changes the score.

```
cannon: fires every 350 ms from y=276, reaches 70 px above the line
left lane   80 px/s → first plane enters the firing band at 2.95 s, last escapes at 5.22 s
right lane  25 px/s → first plane would enter the band at 9.44 s; the clock ends at 9.0 s
```

So "cover which lane first" is worth exactly 3 points or 0 — discrete, and readable
from the server (`POST /plane/api/report` every 250 ms, read back at `/plane/__state`)
rather than from the harness's own report.

The two things the test separates:

| model answers | rules that fire | site score |
|---|---|---|
| `left` | `go-left`, `hold-left` | 3 |
| `right` | `go-right`, `hold-right` | 0 (two squadrons escape) |

Same rules, same clock, same seed. Only `controller.decide.options` order and the
stub's answer differ — which is what makes it evidence that the model's decision,
not the harness, moved the outcome.

## Three numbers that have to agree

Getting a reproducible payoff took three constraints, each found by a test that
failed in a way that blamed the wrong thing:

| constraint | why |
|---|---|
| `RANGE == HIT_X` (24 px) | The rules release the key when "aligned". If alignment is defined wider than the shot's hit window, an aligned player still scores nothing — that reads as "the strategy is wrong" but it is a wrong definition. |
| `SHIP_SPEED / observe.max_hz < RANGE` | The controller samples at 20 Hz. At 300 px/s the ship moves 30 px between samples, overshoots the lane, and presses on to the wall. 120 px/s keeps it inside 12 px per sample. |
| the cannon has a band (`BAND = 70`) | An unlimited-range cannon can hit planes still off the top of the screen, which quietly makes the far lane shootable too and erases the decision. |

## The physics lesson (why every position is a function of `t`)

Two earlier versions integrated positions per frame (`p.y += speed * dt`) and tested
shot/plane overlap at each frame. Under test load a background tab drops frames: the
world ran slower than wall-clock, and a fast plane could sit between two samples so a
shot passed straight through it. The same policy scored 3 in one run and 0 in another —
the test was measuring the machine.

Every position here derives from elapsed time (`planeY(side, i, t)`, shot height from
its fire time), and hits are **solved**: both move linearly, so
`t = (FIRE_Y - y0 + v_s·t0) / (v_s + v_p)` is the exact crossing time, applied in
time order against escapes. A dropped frame changes what is drawn, never what happened.
If you add a site for timing-sensitive verification, copy that pattern rather than a
per-frame update loop.

## Files

- `index.html` — the game; exposes `window.__planeState()` for `controller.observe`
- `decide-left.json` — the task spec (`kind=interactive`, 30 Hz rules, one `decide`
  block). `{PORT}` is substituted by the suite.

The rules use `s.d` (the strategy the model chose) exactly like any other state
field. If no model is configured, `s.d` is simply absent, no strategy rule matches,
and the game still runs — which is the behaviour `decide-smoke.py` asserts.
