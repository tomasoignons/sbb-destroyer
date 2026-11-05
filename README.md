# sbb-destroyer

Find the fastest journey that **steps into all 26 Swiss cantons** using only public
transport (trains, buses, trams, boats, cable cars, funiculars, metro) and walking.
The journey doesn't have to be a loop: give a start station and time, get the list of
vehicles to take.

## Rules modelled

A canton counts when you physically step into it:

| situation | counts? |
|---|---|
| boarding / alighting at a stop in the canton | ✅ |
| riding through a stop where the vehicle **halts** for passengers (hop off and back on) | ✅ (`--min-dwell` sets the minimum scheduled dwell, default 0 s) |
| walking to a stop in the canton | ✅ |
| passing through without a halt | ❌ (pass-throughs are not in the GTFS, so they can never count) |
| changing vehicle at a station | ✅ (it's an alight + board) |

Stops abroad (DE/FR/IT/AT) are allowed in transit but count for nothing. On-demand
buses (route_type 715, *Rufbus*) and taxis (1500) are excluded by default because they
need a booking (`--exclude-route-types ''` keeps them).

## Quick start (CLI)

```bash
conda env create -f environment.yml   # once: Python env for preprocessing
./sbb setup                           # once: convert GTFS, map stops to cantons, build the solver
./sbb find "genève"                   # exact station names
./sbb route "Genève" --date saturday --start 05:00
./sbb route "Genève" -d 2026-11-14 -s 04:30 --sweep-end 08:00 --json route.json
./sbb route "Genève" --map              # compute + open an interactive map of the route
./sbb map route.json                      # map of a saved itinerary
./sbb route "Genève" -- --pair-limit 10   # raw solver flags after --
./sbb status                          # what is prepared
./sbb day 2026-11-14 --walk-radius 1500 --walk-speed 15   # rebuild a date with custom options
./sbb --help / ./sbb route --help
```

`route` and `find` build the date's timetable automatically the first time (~8 s).
Dates can be `YYYY-MM-DD`, `today`, `tomorrow` or a weekday name (the next one).
Set `SBB_PYTHON=/path/to/python` if the `sbb` conda env lives somewhere unusual.

### Shaping the route: via stations and "run for it" stations

* **Via stations** (`./sbb route Genève --via Brig --via Lugano --via Chur`, or the "Goes via"
  field in the web app): the route must pass through them **in this order**, then continues
  freely. "Pass through" = board, alight, hop off during a halt, or walk/bike there. Max 6. They
  are stored as extra bits (26–31) of the same mask as the cantons, in unary, so the dominance
  rule ("same or more") covers them for free.
* **Run-for-it stations** (`bonus_stations.tsv`, edit to add your own): spend at least N minutes
  there and extra cantons count. Either the train you're on stops there that long (no
  change needed), or you get off and leave at least N minutes later:

  | station | min. stay | counts as |
  |---|---|---|
  | Ziegelbrücke | 1 min | + Glarus |
  | Rorschach | 18 min | + St. Gallen, Thurgau |
  | Aigle | 20 min | + Valais |
  | Nyon | 20 min | + Genève |

  Getting off shows as a **Run** step in the itinerary. Staying on board shows as "run while
  the train waits" under that ride. The lower bounds treat these stations as already
  in those cantons, so pruning stays exact. Turn it off with `--no-bonus` or the web checkbox.
  Genève 05:00, beam 8,32,128: **18h01** with bike + run-for-it stations.

* **Via stations first** (`--via-first`, or the "Go to these first, directly" toggle under the
  via field): the via stations become the route's first destinations. The solver takes the
  fastest way to each, in order (cantons passed on the way still count; detour rules are off
  for these legs), then runs the free canton search from the last via station, at its arrival
  time, with everything collected so far. Without the toggle, via stations only fix the
  *order*, and the route may collect other cantons before reaching them.
* **Mandatory end station** (`--to "Zürich HB"`, or the "End station" field): the route must
  finish there after all required cantons (and via stations), and the trip time stops on arrival
  there. With a start station/time and "Already validated", this optimises any segment of the
  run, e.g. `./sbb route Luzern -s 12:44 --have GE,VD,NE,BE,SO,JU --to "Zürich HB"`. Internally a
  last mask bit (31) that can only be set at the end station once everything else is done. The
  lower bound adds "reach the farthest missing canton, then the end station" (reverse Dijkstra
  from the end). Via stations: max 5.
* **Already validated cantons** (`./sbb route Luzern -s 12:44 --have GE,VD,NE,BE,SO,JU,BL,BS`, or
  the "Already validated" picker in the web app): optimise only part of the trip. The search
  starts with those cantons in its set, so the trip time runs from your first vehicle until
  the last *missing* canton. Codes (GE) and names with or without accents (Genève, geneve)
  both work. In the viewer they're marked ✓ and the visit numbering continues after them.
* **Detour rules** (`detour_rules.tsv`): "at Lausanne or Renens VD without Genève → go get
  Genève first". In that situation you may only board a train that still stops in Genève later
  on, may only get off once Genève is reached (or at a run-for-it station that gives
  Genève, i.e. Nyon), and may not walk or cycle away (platform changes
  are fine). Turn off with `--no-detour` or the web checkbox.

**Change times:** 1 minute for every transfer, big stations included: 60 s to change at the
same stop (`--default-change`), and 60 s for every transfer SBB lists, between platforms
of a station or to a nearby differently named stop (`--platform-change`). Only walks that SBB
doesn't list as transfers (and bike legs) are timed by distance. `./sbb day <date>
--sbb-change-times` brings back SBB's official (more conservative) transfer times.

### Web app

```bash
./sbb web --open            # http://localhost:8026/
./sbb web --host 0.0.0.0    # reachable from other devices on your network
```

* **New route:** a start station with autocomplete (accent-insensitive: "zurich" finds Zürich HB),
  a date, the earliest start, optionally "try starts until" (sweeps start times in parallel and
  keeps the shortest trip), search quality (Quick `8,32` / Better `8,32,128` / Best
  `8,32,128,512`), min. dwell, walking, and a note.
* Jobs run in the background **one at a time**, with live progress and the solver log. The
  number of parallel searches per job is set from free RAM (`--threads` caps it). Deleting a
  running job stops it. Jobs interrupted by a server restart are re-queued.
* Every result is stored in SQLite (`data/app.db`). Click a saved route to view it on the map.
  Sort by newest or shortest trip. "Edit & recompute" copies a route's settings into the form.
  `#route=ID&leg=N` URLs link to a route and a connection.
* Code: `web/server.py` (standard library only: http.server, sqlite3, which runs `./sbb route`),
  `web/static/` (`app.js`, `app.css`, plus `route_view.js`/`.css`, the map viewer shared with `./sbb map`).

**Trip time** runs from boarding the first vehicle until the 26th canton is reached: getting
off (or hopping off) there, or arriving on foot if the last canton is reached by walking.
Waiting or walking before the first vehicle doesn't count. Since an earlier train plus a
wait elsewhere can arrive just as late, the solver also **refines** the start: it retries
every minute after the chosen first departure (30 min by default; up to the next swept start
with `--sweep-end`) and keeps the shortest trip. Example: Genève, 14 Nov: the 05:06 departure
→ 19h27 becomes the 05:24 departure → 19h09, with the same arrival.

### Route map

`--map` writes `data/routes/<station>_<date>_<time>.json` + `.html` and opens it. The page
is self-contained (only the Leaflet library and swisstopo map tiles come from the internet):

* the route drawn stop-by-stop, colored by mode (train, bus, tram, boat, cable car,
  funicular, metro; walks dashed), with numbered markers where each canton was collected;
* a clickable list of connections (vehicle, number, direction, operator, times, cantons
  gained). Click a connection or line to zoom and see its intermediate stops; ← / → step
  through, Esc shows the whole route, `#leg=N` in the URL links to one connection;
* a **Cantons** tab: all 26 in visit order with how/where/when;
* light/dark following the OS, with a toggle.

Built by `viz/make_map.py` (stdlib only) from `viz/route_map.html` and the simplified canton
outlines `data/parquet/cantons.geojson` (`prep/canton_shapes.py`, run by `./sbb setup`).

The sections below describe the individual steps the CLI runs.

## Setup (manual)

```bash
conda env create -f environment.yml        # or: conda create -n sbb -c conda-forge python=3.12 polars geopandas pyogrio pyproj shapely numpy pyarrow
PY=~/Apps/miniconda3/envs/sbb/bin/python   # or `conda activate sbb` and use `python`

# once per GTFS download
$PY -I prep/convert_gtfs.py --gtfs . --out data/parquet          # ~20 s, CSV -> parquet
$PY -I prep/stop_cantons.py --gdb swissBOUNDARIES3D_1_5_LV95_LN02.gdb   # stop -> canton

# once per travel date (~8 s)
$PY -I prep/build_day.py --date 2026-11-14

# solver
cd solver && cargo build --release && cd ..
```

## Usage

```bash
S=solver/target/release/canton-solver
D=data/days/2026-11-14

$S --data $D --find "genève"                           # look up exact station names
$S --data $D --from "Genève" --start 05:00              # one start time
$S --data $D --from "Genève" --start 04:30 --sweep-end 08:00 --sweep-step 15   # many start times in parallel
$S --data $D --from "Genève" --start 05:00 --beams 8,32,128,512 --json route.json  # wider search (more RAM/time)
```

Output (truncated):

```
START  Genève  at 05:00  [Genève]  (1/26)
  1. 05:06 → 05:59  RE33       Genève → Lausanne   (dir. St-Maurice, n° 18403)
        + Vaud                     hop off/on at Coppet (05:16)  (2/26)
  2. 06:04 → 08:26  IC51       Lausanne → Basel SBB   (dir. Basel SBB, n° 1609)
        + Neuchâtel                hop off/on at Neuchâtel (06:51)  (3/26)
        + Bern                     hop off/on at Biel/Bienne (07:12)  (4/26)
  ...
DONE: 26 cantons, leave 05:06 arrive 00:33+1  →  19h27
```

`+1` means the next day. `--json` writes every leg with stop IDs, LV95 coordinates, trip
IDs and where each canton was collected.

Main options:

| option | meaning |
|---|---|
| `--beams 8,32` | search width of successive passes; each pass prunes with the best arrival found so far |
| `--evict bound` | which labels survive at a full stop: `bound` (most cantons, then lower bound), `popcount` (most cantons, then earliest), `spread` (λ·missing + lower bound) |
| `--pair-limit 8` | use the stronger pairwise lower bound when ≤ N cantons are missing |
| `--min-dwell 0` | min scheduled dwell [s] for a hop-off/on to count |
| `--no-walk` | no walking between nearby stops |
| `--skip "Ticino,Valais"` | don't require some cantons (experiments) |
| `--upper-bound 00:30` | prune with a known arrival time (HH:MM, may exceed 24:00) |
| `--max-hours 36` | search horizon |

Preprocessing options (`prep/build_day.py`): `--days 2` (service days covered),
`--walk-radius 800` m, `--walk-speed 4.5` km/h, `--walk-detour 1.3`, `--walk-buffer 60` s,
`--default-change 120` s, `--exclude-route-types 715,1500`.

**Bike:** on by default, ≤ 10 km per leg (road distance ≈ 1.3 × straight line) at 20 km/h,
+120 s per leg. From every station, for each canton within range, links go to the 3 nearest
and the 3 busiest stations of that canton (`--bike-max-km`, `--bike-speed`, `--bike-detour`,
`--bike-buffer`, `--bike-k-near`, `--bike-k-hub`; `./sbb day <date> --bike-max-km 0` disables).
`./sbb route … --no-bike` / the web form's checkbox turn it off per search. **No cycling in either
Appenzell:** bike legs that start, end or cross AR or AI are dropped (checked against the canton
outlines; walking there is still allowed). Change with `./sbb day <date> --bike-banned-cantons
"…"` (`''` = no ban). The trip clock starts
at the first vehicle **or** bike leg. Genève 05:00 on 14 Nov: 19h27 without a bike → 18h52 with one.

## How it works

### Data pipeline (`prep/`)

1. **convert_gtfs.py** – streams the 3.7 GB `stop_times.txt` into typed parquet.
2. **stop_cantons.py** – point-in-polygon of every stop against `TLM_KANTONSGEBIET`
   (swissBOUNDARIES3D, LV95). Stops ≤ 50 m outside a border are snapped in.
3. **build_day.py** – for one date: resolves `calendar` + `calendar_dates`, expands
   `frequencies.txt`, builds trip *instances* for the previous, current and next service
   days (times = seconds since 00:00 of the date), then writes:
   * connections (one per consecutive stop pair of a trip), sorted by departure, with flags
     *can board*, *can alight* and *halts for passengers*, plus the dwell time;
   * footpaths: SBB's own transfer times (`transfers.txt`, type 2) plus straight-line walks
     between stops within the walk radius;
   * per-stop minimum change time;
   * in-seat continuations (`transfers.txt`, type 4: the train changes number and you stay seated).

### Solver (`solver/`, Rust)

A **Connection Scan Algorithm** where every label carries a 26-bit mask of the cantons
collected so far:

```
for each connection c (u → v, trip t) in departure order:
    board:  labels at u ready by dep(c)  →  trip t's label set   (mask |= canton(u))
    alight: trip t's labels              →  labels at v, ready at arr(c) + change time
            (mask |= canton(v)); then walk to nearby stops (mask |= canton(w))
    stay:   if t halts at v for passengers, trip labels get mask |= canton(v)
    stop when dep(c) ≥ best arrival with all 26 cantons
```

* **Dominance:** at a stop, label A beats B if it is ready no later *and* has a superset of
  B's cantons. Only non-dominated labels are kept.
* **Beam:** the full Pareto set is exponential, so each stop/trip keeps at most *k*
  labels. Larger *k* gives better answers; `--beams 100000` is effectively exact on small
  instances. Default `8,32` takes ~15 s; `512` takes ~7 min and ~5 GB (Genève 05:00 →
  19h27 with 32, 19h12 with 512).
* **Lower bounds** (`heuristic.rs`): reverse Dijkstra per canton on the static network (fastest
  ride per stop pair, waits ignored) gives `dist[stop][canton]`. A label can't finish
  before `ready + max over missing cantons c of dist(stop, c)`; when ≤ 8 cantons are
  missing, the bound also tries both orders of each pair. Labels that can't beat the current
  best are pruned. This pruning is exact, so it never loses the optimum.
* **Sweeps:** different start times are independent and run in parallel on all cores (rayon).

On this data: 4.15 M connections, 56.7 k stops and 334 k trip instances for a
Saturday ± 1 day. Loading takes ~1 s.

### Ideas for further optimisation

* **Stronger lower bound** → much more pruning, e.g. a minimum-spanning-tree or
  TSP-relaxation bound over the missing cantons, or time-dependent distances (profile CSA).
* **Smarter beam:** keep diversity, e.g. reserve slots per "region signature" of the mask, or
  learn per-canton difficulty weights (Genève, Ticino, Valais, Jura, Graubünden are the corners).
* **Iterated local search** on the itinerary: fix a canton order, re-optimise sub-paths
  with plain earliest-arrival CSA, swap and re-insert cantons.
* **Walk chains:** footpaths are one-hop (ride → walk → ride); a walk followed by another
  walk is not allowed.
* **Performance:** stop label sets are small `Vec`s with linear dominance checks; SIMD over
  masks, or storing masks and ready times as separate arrays, would speed this up. The scan
  itself is sequential; the GPU is not used. It would only help for massively parallel
  independent queries (e.g. computing all-pairs time-dependent bounds), not for this
  label-setting scan.
* **Real-world robustness:** require a minimum transfer margin, avoid hop-offs with
  0-minute dwell (`--min-dwell 60`), and take the DST change days into account.

## Layout

```
prep/convert_gtfs.py    GTFS CSV -> parquet
prep/stop_cantons.py    stop -> canton
prep/build_day.py       one date -> binary timetable in data/days/<date>/
solver/src/data.rs      loading the binary timetable
solver/src/heuristic.rs lower bounds (per-canton reverse Dijkstra)
solver/src/solver.rs    canton-mask CSA with beam + pruning
solver/src/output.rs    itinerary text / JSON
solver/src/main.rs      CLI, start-time sweep, iterative beams
prep/canton_shapes.py   simplified canton outlines for the map
viz/make_map.py         itinerary JSON -> interactive HTML map
viz/route_map.html      map page template
sbb                     command-line front end
web/server.py           web app backend (stdlib HTTP + SQLite + job worker)
web/static/             web app frontend; route_view.* is shared with the standalone map
```
