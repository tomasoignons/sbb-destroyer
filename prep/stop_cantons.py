"""Step 2: assign every GTFS stop to a canton (point-in-polygon on swissBOUNDARIES3D).

Output: data/parquet/stop_cantons.parquet with columns
    stop_id, canton (0..25 = KANTONSNUMMER-1, -1 = outside Switzerland), canton_name, x, y (LV95)

Usage:
    python prep/stop_cantons.py --gdb swissBOUNDARIES3D_1_5_LV95_LN02.gdb
"""
import argparse
import os
import sys

# pyproj needs to find its database when the conda env is not "activated".
os.environ.setdefault("PROJ_DATA", os.path.join(sys.prefix, "share", "proj"))

import geopandas as gpd  # noqa: E402
import polars as pl  # noqa: E402
from pyproj import Transformer  # noqa: E402

# Stops whose coordinates fall just outside Switzerland (border stations, lake piers,
# rounding) but which are clearly Swiss are snapped to the nearest canton within this radius.
SNAP_M = 50.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gdb", default="swissBOUNDARIES3D_1_5_LV95_LN02.gdb")
    ap.add_argument("--parquet", default="data/parquet")
    args = ap.parse_args()

    k = gpd.read_file(args.gdb, layer="TLM_KANTONSGEBIET")[["KANTONSNUMMER", "NAME", "geometry"]]
    k["geometry"] = k.geometry.force_2d()
    k = k.dissolve(by="KANTONSNUMMER", as_index=False)  # one (multi)polygon per canton
    k["canton"] = k["KANTONSNUMMER"].astype(int) - 1
    k = k.set_crs(2056, allow_override=True)
    print(f"{len(k)} cantons loaded")

    stops = pl.read_parquet(f"{args.parquet}/stops.parquet").select("stop_id", "stop_lat", "stop_lon")
    stops = stops.filter(pl.col("stop_lat").is_not_null() & pl.col("stop_lon").is_not_null())
    tr = Transformer.from_crs(4326, 2056, always_xy=True)
    x, y = tr.transform(stops["stop_lon"].to_numpy(), stops["stop_lat"].to_numpy())
    pts = gpd.GeoDataFrame({"stop_id": stops["stop_id"].to_list(), "x": x, "y": y},
                           geometry=gpd.points_from_xy(x, y), crs=2056)

    j = gpd.sjoin(pts, k[["canton", "NAME", "geometry"]], how="left", predicate="within")
    j = j[~j.index.duplicated(keep="first")]
    inside = j["canton"].notna()
    print(f"{inside.sum()} stops inside a canton, {(~inside).sum()} outside")

    # Snap near-border stops.
    out_pts = pts[~inside.values]
    near = gpd.sjoin_nearest(out_pts, k[["canton", "NAME", "geometry"]], how="inner",
                             max_distance=SNAP_M, distance_col="dist")
    near = near[~near.index.duplicated(keep="first")]
    j.loc[near.index, "canton"] = near["canton"]
    j.loc[near.index, "NAME"] = near["NAME"]
    print(f"{len(near)} near-border stops snapped (<= {SNAP_M:.0f} m)")

    res = pl.DataFrame({
        "stop_id": j["stop_id"].tolist(),
        "canton": j["canton"].fillna(-1).astype(int).tolist(),
        "canton_name": j["NAME"].fillna("").tolist(),
        "x": j["x"].tolist(),
        "y": j["y"].tolist(),
    }).with_columns(pl.col("canton").cast(pl.Int8))
    res.write_parquet(f"{args.parquet}/stop_cantons.parquet")
    print(res.group_by("canton_name").len().sort("len", descending=True))


if __name__ == "__main__":
    main()
