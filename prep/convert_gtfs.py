"""Step 1: convert the raw GTFS CSV files into typed parquet files (run once).

stop_times.txt is ~3.7 GB; polars streams it so this works with modest RAM.

Usage:
    python prep/convert_gtfs.py --gtfs . --out data/parquet
"""
import argparse
import time
from pathlib import Path

import polars as pl


def hms_to_sec(col: str) -> pl.Expr:
    """'25:13:00' -> 90780 (GTFS times can exceed 24h)."""
    parts = pl.col(col).str.split(":")
    return (
        parts.list.get(0).cast(pl.Int32) * 3600
        + parts.list.get(1).cast(pl.Int32) * 60
        + parts.list.get(2).cast(pl.Int32)
    ).alias(col)


def read(path: Path, **kw) -> pl.LazyFrame:
    # Every column as string first; we cast explicitly afterwards.
    return pl.scan_csv(path, infer_schema=False, encoding="utf8-lossy", **kw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gtfs", default=".", help="directory containing the GTFS .txt files")
    ap.add_argument("--out", default="data/parquet")
    args = ap.parse_args()
    gtfs, out = Path(args.gtfs), Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    t0 = time.time()
    small = ["agency", "routes", "trips", "stops", "calendar", "calendar_dates", "transfers", "frequencies"]
    for name in small:
        lf = read(gtfs / f"{name}.txt")
        if name == "frequencies":
            lf = lf.with_columns(hms_to_sec("start_time"), hms_to_sec("end_time"),
                                 pl.col("headway_secs").cast(pl.Int32))
        if name == "calendar":
            days = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"]
            lf = lf.with_columns([pl.col(d).cast(pl.Int8) for d in days])
        if name == "calendar_dates":
            lf = lf.with_columns(pl.col("exception_type").cast(pl.Int8))
        if name == "transfers":
            lf = lf.with_columns(pl.col("transfer_type").cast(pl.Int8),
                                 pl.col("min_transfer_time").cast(pl.Int32, strict=False))
        if name == "routes":
            lf = lf.with_columns(pl.col("route_type").cast(pl.Int32))
        if name == "stops":
            lf = lf.with_columns(pl.col("stop_lat").cast(pl.Float64), pl.col("stop_lon").cast(pl.Float64))
        lf.sink_parquet(out / f"{name}.parquet")
        print(f"{name:15s} done ({time.time() - t0:.0f}s)", flush=True)

    (
        read(gtfs / "stop_times.txt")
        .with_columns(
            hms_to_sec("arrival_time"),
            hms_to_sec("departure_time"),
            pl.col("stop_sequence").cast(pl.Int32),
            pl.col("pickup_type").cast(pl.Int8, strict=False).fill_null(0),
            pl.col("drop_off_type").cast(pl.Int8, strict=False).fill_null(0),
        )
        .sink_parquet(out / "stop_times.parquet", compression="zstd")
    )
    print(f"stop_times      done ({time.time() - t0:.0f}s)")


if __name__ == "__main__":
    main()
