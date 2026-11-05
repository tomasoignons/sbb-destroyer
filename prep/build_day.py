"""Step 3: build a compact binary timetable for one travel date, read by the Rust solver.

All times are seconds since 00:00 of --date. The timetable covers trips of the
service day before (for after-midnight runs), the date itself, and (--days - 1)
following days, so journeys can run through the night / over several days.

Every mode in the SBB GTFS feed is included (trains, buses, trams, boats, cable cars,
funiculars, metro), except route types listed in --exclude-route-types
(default: on-demand buses 715 and taxis 1500, which need a booking).

Usage:
    python prep/build_day.py --date 2026-11-14 [--days 2] [--walk-radius 800]
"""
import argparse
import datetime as dt
import json
import time
from pathlib import Path

import numpy as np
import polars as pl

def bike_ban_mask(lat_a, lon_a, lat_b, lon_b, cantons_geojson, banned_names):
    """True for each straight bike leg a->b that touches one of the banned cantons
    (starts, ends or crosses it). Uses the simplified outlines from prep/canton_shapes.py."""
    import shapely

    gj = json.loads(Path(cantons_geojson).read_text(encoding="utf-8"))
    by_name = {f["properties"]["name"].lower(): f for f in gj["features"]}
    polys = []
    for n in banned_names:
        f = by_name.get(n.lower())
        if f is None:
            raise SystemExit(f"--bike-banned-cantons: unknown canton {n!r}; known: {sorted(by_name)}")
        polys.append(shapely.geometry.shape(f["geometry"]))
    area = shapely.union_all(polys)
    shapely.prepare(area)
    coords = np.stack([np.stack([lon_a, lat_a], 1), np.stack([lon_b, lat_b], 1)], 1)  # (n, 2, 2)
    return shapely.intersects(area, shapely.linestrings(coords))


WEEKDAYS = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"]


def active_services(cal: pl.DataFrame, cal_dates: pl.DataFrame, day: dt.date) -> pl.Series:
    ds = day.strftime("%Y%m%d")
    base = cal.filter(
        (pl.col(WEEKDAYS[day.weekday()]) == 1) & (pl.col("start_date") <= ds) & (pl.col("end_date") >= ds)
    ).select("service_id")
    exc = cal_dates.filter(pl.col("date") == ds)
    added = exc.filter(pl.col("exception_type") == 1).select("service_id")
    removed = exc.filter(pl.col("exception_type") == 2).select("service_id")
    return pl.concat([base, added]).unique().join(removed, on="service_id", how="anti")["service_id"]


def write_u32(path: Path, arr):
    np.asarray(arr, dtype=np.uint32).tofile(path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--date", required=True, help="travel date YYYY-MM-DD")
    ap.add_argument("--days", type=int, default=2, help="number of service days from --date to include")
    ap.add_argument("--parquet", default="data/parquet")
    ap.add_argument("--out", default="data/days")
    ap.add_argument("--exclude-route-types", default="715,1500",
                    help="comma-separated GTFS route_type values to drop ('' keeps everything)")
    ap.add_argument("--walk-radius", type=float, default=800.0, help="max straight-line walk between stops [m]")
    ap.add_argument("--walk-speed", type=float, default=4.5, help="walking speed [km/h]")
    ap.add_argument("--walk-detour", type=float, default=1.3, help="street-network detour factor")
    ap.add_argument("--walk-buffer", type=int, default=60, help="fixed extra seconds per walk")
    ap.add_argument("--bike-max-km", type=float, default=10.0,
                    help="max bike ride between stations, road distance [km] (0 = no bike)")
    ap.add_argument("--bike-speed", type=float, default=20.0, help="cycling speed [km/h]")
    ap.add_argument("--bike-detour", type=float, default=1.3, help="road / straight-line distance factor")
    ap.add_argument("--bike-buffer", type=int, default=120,
                    help="fixed extra seconds per bike leg (unlock, park, reach the platform)")
    ap.add_argument("--bike-banned-cantons", default="Appenzell Ausserrhoden,Appenzell Innerrhoden",
                    help="no bike legs starting, ending or crossing these cantons (comma-separated, '' = none)")
    ap.add_argument("--bike-k-near", type=int, default=3,
                    help="per station and canton in range: link the N nearest stations")
    ap.add_argument("--bike-k-hub", type=int, default=3,
                    help="per station and canton in range: link the N busiest stations")
    ap.add_argument("--platform-change", type=int, default=60,
                    help="seconds for every transfer: between platforms of a station and to the "
                         "nearby stops SBB lists as transfers (all stations)")
    ap.add_argument("--sbb-change-times", action="store_true",
                    help="use SBB's official minimum transfer times where they exist instead of the "
                         "flat --default-change / --platform-change everywhere")
    ap.add_argument("--default-change", type=int, default=60,
                    help="min seconds to change vehicle at the same stop when no transfer rule exists")
    args = ap.parse_args()

    t0 = time.time()
    P = Path(args.parquet)
    date = dt.date.fromisoformat(args.date)
    out = Path(args.out) / args.date
    out.mkdir(parents=True, exist_ok=True)

    def log(msg):
        print(f"[{time.time() - t0:6.1f}s] {msg}", flush=True)

    # ------------------------------------------------------------------ services
    cal = pl.read_parquet(P / "calendar.parquet")
    cal_dates = pl.read_parquet(P / "calendar_dates.parquet")
    day_offsets = list(range(-1, args.days))  # service day D+k, k=-1 catches after-midnight trips
    svc_days = pl.concat([
        pl.DataFrame({"service_id": active_services(cal, cal_dates, date + dt.timedelta(days=k)),
                      "day": k}, schema={"service_id": pl.String, "day": pl.Int8})
        for k in day_offsets
    ])
    log(f"active service ids per day: {svc_days.group_by('day').len().sort('day').to_dicts()}")

    # ------------------------------------------------------------------ trips
    routes = pl.read_parquet(P / "routes.parquet")
    excl = [int(x) for x in args.exclude_route_types.split(",") if x.strip()]
    routes = routes.filter(~pl.col("route_type").is_in(excl))
    agency = pl.read_parquet(P / "agency.parquet").select("agency_id", "agency_name")
    trips = (
        pl.read_parquet(P / "trips.parquet")
        .select("route_id", "service_id", "trip_id", "trip_headsign", "trip_short_name")
        .join(routes.select("route_id", "agency_id", "route_short_name", "route_desc", "route_type"), on="route_id")
        .join(agency, on="agency_id", how="left")
    )
    trip_days = trips.join(svc_days, on="service_id")  # one row per (trip, service day)
    log(f"trip-days: {trip_days.height}")

    # frequency-based trips: template stop_times shifted by start offsets
    freq = pl.read_parquet(P / "frequencies.parquet")
    need = trip_days.select("trip_id").unique()
    st = (
        pl.scan_parquet(P / "stop_times.parquet")
        .join(need.lazy(), on="trip_id", how="semi")
        .collect()
    )
    log(f"stop_times rows for active trips: {st.height}")

    first_dep = st.group_by("trip_id").agg(pl.col("departure_time").sort_by("stop_sequence").first().alias("t0"))
    freq_rows = []
    for r in freq.join(first_dep, on="trip_id").iter_rows(named=True):
        for n, s in enumerate(range(r["start_time"], r["end_time"], r["headway_secs"])):
            freq_rows.append((r["trip_id"], f"#f{r['start_time']}_{n}", s - r["t0"]))
    variants = pl.DataFrame(freq_rows, schema={"trip_id": pl.String, "var": pl.String, "shift": pl.Int32},
                            orient="row")
    plain = need.join(freq.select("trip_id").unique(), on="trip_id", how="anti").with_columns(
        pl.lit("").alias("var"), pl.lit(0, pl.Int32).alias("shift"))
    variants = pl.concat([plain, variants])
    log(f"frequency trips expanded into {len(freq_rows)} runs")

    # trip instances = (trip, variant, service day)
    inst = (
        trip_days.join(variants, on="trip_id")
        .with_columns((pl.col("shift") + pl.col("day").cast(pl.Int32) * 86400).alias("offset"))
    )
    # drop instances entirely before 00:00 of D (cheap bound from first/last times)
    span = st.group_by("trip_id").agg(pl.col("arrival_time").max().alias("tmax"))
    inst = inst.join(span, on="trip_id").filter(pl.col("tmax") + pl.col("offset") >= 0)
    inst = inst.sort("offset", "trip_id", "var").with_row_index("tidx")
    log(f"trip instances: {inst.height}")

    # ------------------------------------------------------------------ stop times per instance
    sti = (
        st.join(inst.select("tidx", "trip_id", "offset"), on="trip_id")
        .with_columns((pl.col("arrival_time") + pl.col("offset")).alias("arr"),
                      (pl.col("departure_time") + pl.col("offset")).alias("dep"))
        .select("tidx", "stop_sequence", "stop_id", "arr", "dep", "pickup_type", "drop_off_type")
        .sort("tidx", "stop_sequence")
    )
    del st
    log(f"instance stop_times: {sti.height}")

    # ------------------------------------------------------------------ stops
    stops_all = pl.read_parquet(P / "stops.parquet").select("stop_id", "stop_name", "parent_station",
                                                             "stop_lat", "stop_lon")
    cantons = pl.read_parquet(P / "stop_cantons.parquet")
    used = sti.select("stop_id").unique()
    stops = (
        used.join(stops_all, on="stop_id", how="left")
        .join(cantons, on="stop_id", how="left")
        .with_columns(pl.col("canton").fill_null(-1), pl.col("x").fill_null(0.0), pl.col("y").fill_null(0.0),
                      pl.col("stop_name").fill_null(pl.col("stop_id")),
                      pl.col("parent_station").fill_null(""),
                      pl.col("stop_lat").fill_null(0.0), pl.col("stop_lon").fill_null(0.0))
        .sort("stop_id")
        .with_row_index("sidx")
    )
    sid = stops.select("stop_id", "sidx")
    log(f"stops used: {stops.height}")

    # ------------------------------------------------------------------ connections
    s = sti.join(sid, on="stop_id").sort("tidx", "stop_sequence")
    tidx = s["tidx"].to_numpy().astype(np.int64)
    sidx = s["sidx"].to_numpy().astype(np.int64)
    arr = s["arr"].to_numpy().astype(np.int64)
    dep = s["dep"].to_numpy().astype(np.int64)
    pick = s["pickup_type"].to_numpy()
    drop = s["drop_off_type"].to_numpy()
    del s, sti

    same = tidx[:-1] == tidx[1:]
    i = np.nonzero(same)[0]  # connection i -> i+1
    j = i + 1
    is_last = np.ones(len(tidx), dtype=bool)
    is_last[:-1] = ~same
    c_dep, c_arr = dep[i], arr[j]
    c_arr = np.maximum(c_arr, c_dep)  # guard against bad data
    keep = c_dep >= 0  # times are stored as u32 seconds since 00:00 of --date
    i, j, c_dep, c_arr = i[keep], j[keep], c_dep[keep], c_arr[keep]
    # flags: 1 = can board at from, 2 = can alight at to,
    #        4 = vehicle halts at `to` for passengers (both pickup & drop-off) and continues
    flags = ((pick[i] != 1).astype(np.uint32)
             | ((drop[j] != 1).astype(np.uint32) << 1)
             | (((pick[j] != 1) & (drop[j] != 1) & ~is_last[j]).astype(np.uint32) << 2))
    dwell = np.clip(dep[j] - arr[j], 0, 65535)
    order = np.lexsort((j, tidx[i], c_arr, c_dep))  # sort by dep, arr, trip, sequence
    conns = {
        "dep": c_dep[order], "arr": c_arr[order], "from": sidx[i][order], "to": sidx[j][order],
        "trip": tidx[i][order], "flags": flags[order], "dwell": dwell[order],
    }
    for k, v in conns.items():
        write_u32(out / f"conn_{k}.bin", v)
    log(f"connections: {len(order)}  (dep range {c_dep.min() / 3600:.1f}h .. {c_dep.max() / 3600:.1f}h)")

    # ------------------------------------------------------------------ transfers & footpaths
    tr = pl.read_parquet(P / "transfers.parquet")
    t2 = tr.filter(pl.col("transfer_type") == 2)
    change = (
        t2.filter(pl.col("from_stop_id") == pl.col("to_stop_id"))
        .group_by("from_stop_id").agg(pl.col("min_transfer_time").min())
        .rename({"from_stop_id": "stop_id", "min_transfer_time": "change"})
    )
    if args.sbb_change_times:
        stops = stops.join(change, on="stop_id", how="left").with_columns(
            pl.col("change").fill_null(args.default_change)).sort("sidx")
    else:  # same change time everywhere, big stations included
        stops = stops.with_columns(pl.lit(args.default_change, pl.Int32).alias("change")).sort("sidx")

    explicit = (
        t2.filter(pl.col("from_stop_id") != pl.col("to_stop_id"), pl.col("min_transfer_time").is_not_null())
        .join(sid.rename({"stop_id": "from_stop_id", "sidx": "a"}), on="from_stop_id")
        .join(sid.rename({"stop_id": "to_stop_id", "sidx": "b"}), on="to_stop_id")
        .group_by("a", "b").agg(pl.col("min_transfer_time").min().alias("dur"))
    )
    if not args.sbb_change_times:
        # every transfer SBB lists (same station or to a nearby differently named stop) is
        # treated as a flat --platform-change (1 min by default)
        explicit = explicit.select(pl.col("a").cast(pl.Int64), pl.col("b").cast(pl.Int64),
                                   pl.lit(args.platform_change, pl.Int32).alias("dur"))

    # geometric walking links (grid hashing, all pairs within radius)
    xs, ys = stops["x"].to_numpy(), stops["y"].to_numpy()
    valid = (xs != 0) | (ys != 0)
    R = args.walk_radius
    speed = args.walk_speed / 3.6
    cell = np.floor(np.stack([xs / R, ys / R], 1)).astype(np.int64)
    key = cell[:, 0] * 1_000_003 + cell[:, 1]
    df = pl.DataFrame({"a": np.arange(len(xs)), "key": key, "cx": cell[:, 0], "cy": cell[:, 1]}).filter(
        pl.Series(valid))
    pairs = []
    for dx in (-1, 0, 1):
        for dy in (-1, 0, 1):
            nb = df.with_columns(((pl.col("cx") + dx) * 1_000_003 + (pl.col("cy") + dy)).alias("key"))
            m = nb.select("a", "key").join(df.select(pl.col("a").alias("b"), "key"), on="key")
            m = m.filter(pl.col("a") != pl.col("b"))
            a_, b_ = m["a"].to_numpy(), m["b"].to_numpy()
            d = np.hypot(xs[a_] - xs[b_], ys[a_] - ys[b_])
            ok = d <= R
            pairs.append(pl.DataFrame({"a": a_[ok], "b": b_[ok],
                                       "dur": (d[ok] * args.walk_detour / speed + args.walk_buffer).astype(np.int32)}))
    geo = pl.concat(pairs)
    # platforms of the same (small) station: flat platform-change time instead of
    # distance + walk buffer (big stations have explicit SBB transfer rules, which win)
    names = stops["stop_name"].to_numpy()
    same = names[geo["a"].to_numpy()] == names[geo["b"].to_numpy()]
    geo = geo.with_columns(pl.when(pl.Series(same)).then(pl.lit(args.platform_change, pl.Int32))
                           .otherwise(pl.col("dur")).alias("dur"))
    links = [
        explicit.select(pl.col("a").cast(pl.Int64), pl.col("b").cast(pl.Int64), pl.col("dur").cast(pl.Int32),
                        pl.lit(1).alias("pri"), pl.lit(0, pl.UInt8).alias("kind")),
        geo.select(pl.col("a").cast(pl.Int64), pl.col("b").cast(pl.Int64), pl.col("dur").cast(pl.Int32),
                   pl.lit(0).alias("pri"), pl.lit(0, pl.UInt8).alias("kind")),
    ]

    # bike links between stations (a station = all platforms sharing a name). Linking every
    # pair within 10 km would give tens of millions of links, so from each station and for
    # each canton in range we keep the K nearest stations (fastest way across a border) and
    # the K busiest ones (good onward connections), then connect all their platforms.
    if args.bike_max_km > 0:
        Rb = args.bike_max_km * 1000 / args.bike_detour  # straight-line radius
        bspeed = args.bike_speed / 3.6
        deps = np.bincount(conns["from"].astype(np.int64), minlength=stops.height)
        plat = (stops.select("sidx", "stop_name", "x", "y", "canton", "stop_lat", "stop_lon").filter(pl.Series(valid))
                .with_columns(pl.Series("deps", deps[stops["sidx"].to_numpy()][valid])))
        stn = (plat.group_by("stop_name")
               .agg(pl.col("x").mean(), pl.col("y").mean(), pl.col("canton").mode().first(),
                    pl.col("stop_lat").mean().alias("lat"), pl.col("stop_lon").mean().alias("lon"),
                    pl.col("deps").sum(), pl.col("sidx").alias("members"))
               .sort("stop_name").with_row_index("k"))
        sx, sy = stn["x"].to_numpy(), stn["y"].to_numpy()
        scell = np.floor(np.stack([sx / Rb, sy / Rb], 1)).astype(np.int64)
        sdf = pl.DataFrame({"ka": np.arange(len(sx)), "cx": scell[:, 0], "cy": scell[:, 1]})
        sdf = sdf.with_columns((pl.col("cx") * 1_000_003 + pl.col("cy")).alias("key"))
        cand = []
        for dx in (-1, 0, 1):
            for dy in (-1, 0, 1):
                nb = sdf.select("ka", ((pl.col("cx") + dx) * 1_000_003 + (pl.col("cy") + dy)).alias("key"))
                m = nb.join(sdf.select(pl.col("ka").alias("kb"), "key"), on="key").filter(pl.col("ka") != pl.col("kb"))
                ka, kb = m["ka"].to_numpy(), m["kb"].to_numpy()
                d = np.hypot(sx[ka] - sx[kb], sy[ka] - sy[kb]).astype(np.float32)
                ok = d <= Rb
                cand.append(pl.DataFrame({"ka": ka[ok].astype(np.int32), "kb": kb[ok].astype(np.int32), "d": d[ok]}))
        cand = pl.concat(cand).join(stn.select(pl.col("k").cast(pl.Int32).alias("kb"), pl.col("canton").alias("cb"),
                                               pl.col("deps").alias("db")), on="kb")
        near = cand.sort("d").group_by("ka", "cb", maintain_order=True).head(args.bike_k_near)
        hub = cand.sort("db", descending=True).group_by("ka", "cb", maintain_order=True).head(args.bike_k_hub)
        sel = pl.concat([near, hub]).unique(subset=["ka", "kb"])
        banned = [c.strip() for c in args.bike_banned_cantons.split(",") if c.strip()]
        if banned:
            slat, slon = stn["lat"].to_numpy(), stn["lon"].to_numpy()
            ka, kb = sel["ka"].to_numpy(), sel["kb"].to_numpy()
            hit = bike_ban_mask(slat[ka], slon[ka], slat[kb], slon[kb], P / "cantons.geojson", banned)
            sel = sel.filter(pl.Series(~hit))
            log(f"bike: {int(hit.sum())} station pairs dropped (touch {', '.join(banned)})")
        members = stn.select(pl.col("k").cast(pl.Int32), "members")
        bl = (sel.join(members.rename({"k": "ka", "members": "a"}), on="ka")
                 .join(members.rename({"k": "kb", "members": "b"}), on="kb")
                 .explode("a").explode("b")
                 .select(pl.col("a").cast(pl.Int64), pl.col("b").cast(pl.Int64),
                         (pl.col("d") * args.bike_detour / bspeed + args.bike_buffer).cast(pl.Int32).alias("dur"),
                         pl.lit(0).alias("pri"), pl.lit(1, pl.UInt8).alias("kind")))
        links.append(bl)
        log(f"bike: {stn.height} stations, {cand.height} station pairs within {Rb / 1000:.1f} km "
            f"straight line, {sel.height} kept -> {bl.height} platform links")

    foot = (
        pl.concat(links)
        # explicit SBB transfer times win; otherwise the fastest way (walk or bike)
        .sort(["pri", "dur"], descending=[True, False])
        .unique(subset=["a", "b"], keep="first")
        .sort("a", "b")
    )
    fa = foot["a"].to_numpy()
    offs = np.searchsorted(fa, np.arange(stops.height + 1))
    write_u32(out / "foot_offsets.bin", offs)
    write_u32(out / "foot_to.bin", foot["b"].to_numpy())
    write_u32(out / "foot_dur.bin", foot["dur"].to_numpy())
    foot["kind"].to_numpy().astype(np.uint8).tofile(out / "foot_kind.bin")  # 0 = walk, 1 = bike
    nb = int((foot["kind"] == 1).sum())
    log(f"links: {foot.height} ({explicit.height} explicit SBB transfers, {foot.height - nb} walk, {nb} bike)")

    # ------------------------------------------------------------------ in-seat transfers (type 4)
    t4 = tr.filter(pl.col("transfer_type") == 4).select("from_stop_id", "from_trip_id", "to_trip_id", "service_id")
    t4 = t4.join(svc_days, on="service_id")  # applies on that service day
    key_inst = inst.filter(pl.col("var") == "").select("tidx", "trip_id", "day")
    t4 = (
        t4.join(key_inst.rename({"tidx": "ft", "trip_id": "from_trip_id"}), on=["from_trip_id", "day"])
        .join(key_inst.rename({"tidx": "tt", "trip_id": "to_trip_id"}), on=["to_trip_id", "day"])
        .join(sid.rename({"stop_id": "from_stop_id"}), on="from_stop_id")
        .unique(subset=["ft"], keep="first")
    )
    write_u32(out / "stay_from.bin", t4["ft"].to_numpy())
    write_u32(out / "stay_to.bin", t4["tt"].to_numpy())
    write_u32(out / "stay_stop.bin", t4["sidx"].to_numpy())
    log(f"in-seat trip continuations: {t4.height}")

    # ------------------------------------------------------------------ metadata for printing
    stops.select("sidx", "stop_id", "stop_name", "canton", "x", "y", "change", "parent_station",
                 "stop_lat", "stop_lon").write_csv(
        out / "stops.tsv", separator="\t", quote_style="never")
    inst.select(
        "tidx", "trip_id", pl.col("var"), "day",
        pl.col("route_short_name").fill_null(""), pl.col("route_desc").fill_null(""),
        pl.col("route_type"), pl.col("trip_short_name").fill_null(""),
        pl.col("trip_headsign").fill_null("").str.replace_all("\t", " "),
        pl.col("agency_name").fill_null("").str.replace_all("\t", " "),
    ).write_csv(out / "trips.tsv", separator="\t", quote_style="never")
    canton_names = (cantons.filter(pl.col("canton") >= 0).group_by("canton")
                    .agg(pl.col("canton_name").first()).sort("canton")["canton_name"].to_list())
    meta = {"format": 7, "date": args.date, "days": args.days, "n_stops": stops.height, "n_trips": inst.height,
            "n_conns": int(len(order)), "cantons": canton_names, "args": vars(args)}
    (out / "meta.json").write_text(json.dumps(meta, indent=2, ensure_ascii=False))
    log(f"written to {out}")


if __name__ == "__main__":
    main()
