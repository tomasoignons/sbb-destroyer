"""Render a solver itinerary (--json output) as a self-contained interactive HTML map.

Standard library only. Uses the same viewer code as the web app (web/static/route_view.*).

Usage:
    python viz/make_map.py route.json [-o route.html] [--cantons data/parquet/cantons.geojson]
"""
import argparse
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
STATIC = HERE.parent / "web" / "static"


def to_js(obj) -> str:
    # safe to embed inside <script>: no "</script>" or HTML comment openers
    return (json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
            .replace("</", "<\\/").replace("<!--", "<\\!--"))


def render(route: dict, cantons: dict) -> str:
    legs = route.get("legs", [])
    if legs and "path" not in legs[0]:
        raise SystemExit("this itinerary has no stop coordinates — rebuild the day "
                         "(./sbb day <date>) and re-run the route with the current solver")
    start = legs[0]["from"]["name"] if legs else "?"
    title = f"{start} · 26 cantons · {route.get('date', '')}"
    tpl = (HERE / "route_map.html").read_text(encoding="utf-8")
    return (tpl.replace("/*TITLE*/", title.replace("<", "&lt;"))
               .replace("/*CSS*/", (STATIC / "route_view.css").read_text(encoding="utf-8"))
               .replace("/*JS*/", (STATIC / "route_view.js").read_text(encoding="utf-8").replace("</", "<\\/"))
               .replace("/*ROUTE*/null", to_js(route))
               .replace("/*CANTONS*/null", to_js(cantons)))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("route", help="itinerary JSON written by the solver (--json)")
    ap.add_argument("-o", "--out", help="output HTML (default: same name as the JSON)")
    ap.add_argument("--cantons", default=str(HERE.parent / "data" / "parquet" / "cantons.geojson"))
    a = ap.parse_args()
    route = json.loads(Path(a.route).read_text(encoding="utf-8"))
    cantons = json.loads(Path(a.cantons).read_text(encoding="utf-8"))
    out = Path(a.out) if a.out else Path(a.route).with_suffix(".html")
    out.write_text(render(route, cantons), encoding="utf-8")
    print(out)


if __name__ == "__main__":
    main()
