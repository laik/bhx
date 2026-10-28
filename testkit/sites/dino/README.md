# dino — interactive controller demo

`chrome://dino` is Chrome's offline page: a canvas runner with randomly spawned
obstacles. There is no DOM to observe and, in current Chrome, the game object
(`Runner`) is module-scoped and not on `window`. The controller therefore reads
the **canvas pixels** and turns them into a small state object, once per
sample. Nothing here needs a model: this is the fast path the interactive layer
exists for.

```bash
cargo build --release
python3 testkit/dino-smoke.py             # self-launched headless Chrome, 20 s
python3 testkit/dino-smoke.py --headed    # watch it play
python3 testkit/dino-smoke.py --cdp-url http://127.0.0.1:9333 --headed
python3 testkit/dino-smoke.py --seconds 6 # short run
```

This demo is **not** part of `run-phase4.sh`: it needs a real Chrome (the page
does not exist in a plain HTTP server) and its assertion count depends on the
survival goal. The smoke asserts `goal_met`, a real number of jumps, ~30 Hz
ticks and a healthy tick budget.

## How the state is built

`controller.observe.script` (in `task.json`) returns:

| field | meaning |
|---|---|
| `ground` | the row with the most dark pixels in the bottom third — the ground line |
| `dbot` | lowest dark pixel in `x < 45` — the dinosaur, so `airborne = dbot < ground - 4` |
| `gap` | nearest obstacle's left edge minus the dinosaur's right edge (42) |
| `trigger` | jump distance: `max(45, speed * 0.22 - 8)` |
| `speed` | px/s estimated from the same obstacle between two samples |
| `stalled` | canvas hash unchanged for 400 ms → the start/game-over screen |

Obstacle detection is deliberately strict: a column counts only if it has ≥ 3
dark pixels **and** rises ≥ 8 px above the ground line. That rejects the ground
speckles, clouds and the HUD digits. Columns are grouped across gaps of up to
3 px because the cactus sprite is drawn in parts (trunk + arms).

## Rules

| rule | when | do |
|---|---|---|
| `start-or-restart` | `s.stalled` | `key_down ArrowUp` |
| `jump` | `!s.airborne && s.gap < s.trigger` | `key_down ArrowUp` |
| `release` | `!s.airborne && (s.gap == null \|\| s.gap >= s.trigger)` | `key_up ArrowUp` |

The goal is `(s) => s.elapsed_ms >= 20000`; the controller stops on its own and
reports `goal_met`.

## Three things the first attempt got wrong

1. **`press` is not a jump.** `press` is keydown+keyup in the same tick, and
   the game's keyup ends the jump — the dinosaur twitches and lands. Use
   `key_down`, and `key_up` only after landing.
2. **Group across sprite gaps.** A cactus is several dark runs; breaking at the
   first 1–3 px gap drops it behind the `nw < 6` width check.
3. **Jump on time-to-arrival, not distance.** The jump lasts ~0.48 s. A wide
   cactus cluster needs the whole cluster to pass inside that window, so the
   trigger is a time (~0.22 s ahead), scaled by the measured speed — not a
   fixed pixel gap.

## Chrome note

Chrome 136+ refuses remote debugging on the default profile (the port answers
HTTP 404). If you attach with `--cdp-url`, launch a dedicated profile on a free
port:

```bash
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9333 --user-data-dir=/tmp/bhx-profile-9333 about:blank &
```
