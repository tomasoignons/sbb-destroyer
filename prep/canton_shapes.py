"""Simplified canton outlines (WGS84 GeoJSON) for the route map.

Output: data/parquet/cantons.geojson, properties: canton (0..25), name, label [lat, lon]

Usage:
    python prep/canton_shapes.py --gdb swissBOUNDARIES3D_1_5_LV95_LN02.gdb
"""
import argparse
import json
import os
import sys

os.environ.setdefault("PROJ_DATA", os.path.join(sys.prefix, "share", "proj"))

import geopandas as gpd  # noqa: E402


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gdb", default="swissBOUNDARIES3D_1_5_LV95_LN02.gdb")
    ap.add_argument("--out", default="data/parquet/cantons.geojson")
    ap.add_argument("--tolerance", type=float, default=120.0, help="simplification tolerance [m]")
    args = ap.parse_args()

    k = gpd.read_file(args.gdb, layer="TLM_KANTONSGEBIET")[["KANTONSNUMMER", "NAME", "geometry"]]
    k["geometry"] = k.geometry.force_2d()
    k = k.set_crs(2056, allow_override=True).dissolve(by="KANTONSNUMMER", as_index=False)
    k["geometry"] = k.geometry.simplify(args.tolerance, preserve_topology=True)
    label = k.geometry.representative_point().to_crs(4326)
    k = k.to_crs(4326)
    feats = []
    for (_, r), lp in zip(k.iterrows(), label):
        feats.append({
            "type": "Feature",
            "properties": {"canton": int(r["KANTONSNUMMER"]) - 1, "name": r["NAME"],
                           "label": [round(lp.y, 5), round(lp.x, 5)]},
            "geometry": json.loads(gpd.GeoSeries([r.geometry]).set_precision(1e-5).to_json())["features"][0]["geometry"],
        })
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump({"type": "FeatureCollection", "features": feats}, f, ensure_ascii=False, separators=(",", ":"))
    print(f"wrote {args.out} ({os.path.getsize(args.out) / 1e3:.0f} kB)")


if __name__ == "__main__":
    main()
