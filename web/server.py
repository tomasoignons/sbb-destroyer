"""Web app: enter a start station, compute the all-cantons route in the background,
keep every result in SQLite and browse them on a map.

Standard library only. Computations are delegated to `./sbb route` (which builds the
day's timetable and the solver when needed), one job at a time.

Usage:
    ./sbb web [--port 8026] [--host 127.0.0.1] [--open]
"""
import argparse
import datetime as dt
import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import unicodedata
import webbrowser
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

ROOT = Path(__file__).resolve().parent.parent
STATIC = ROOT / "web" / "static"
DB_PATH = ROOT / "data" / "app.db"
DAYS = ROOT / "data" / "days"
SHAPES = ROOT / "data" / "parquet" / "cantons.geojson"
SBB = ROOT / "sbb"

# beams per quality preset, and peak RAM per concurrent search (GB, measured + margin) to size threads
QUALITY = {
    "quick":  {"beams": "8,32",          "gb": 0.6, "label": "Quick",  "hint": "~15 s per start time"},
    "better": {"beams": "8,32,128",      "gb": 1.6, "label": "Better", "hint": "~1 min per start time"},
    "best":   {"beams": "8,32,128,512",  "gb": 5.5, "label": "Best",   "hint": "~10 min per start time, lots of RAM"},
}
# official codes in KANTONSNUMMER order, with display names
CANTON_CODES = ["ZH", "BE", "LU", "UR", "SZ", "OW", "NW", "GL", "ZG", "FR", "SO", "BS", "BL", "SH", "AR", "AI",
                "SG", "GR", "AG", "TG", "TI", "VD", "VS", "NE", "GE", "JU"]
TIME_RE = re.compile(r"^([0-3]?\d|4[0-7]):([0-5]\d)$")
MAX_SWEEP_H = 8

SCHEMA = """
CREATE TABLE IF NOT EXISTS routes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created TEXT NOT NULL,
    station TEXT NOT NULL,
    date TEXT NOT NULL,
    start TEXT NOT NULL,
    sweep_end TEXT,
    quality TEXT NOT NULL,
    min_dwell INTEGER NOT NULL DEFAULT 0,
    walk INTEGER NOT NULL DEFAULT 1,
    bike INTEGER NOT NULL DEFAULT 1,
    via TEXT,                           -- JSON list of station names, in order
    bonus INTEGER NOT NULL DEFAULT 1,
    detour INTEGER NOT NULL DEFAULT 1,
    ride INTEGER NOT NULL DEFAULT 1,
    have TEXT,                          -- JSON list of cantons already validated
    via_first INTEGER NOT NULL DEFAULT 0,
    end_station TEXT,
    note TEXT,
    status TEXT NOT NULL,              -- queued | running | done | failed | cancelled
    message TEXT,
    progress_done INTEGER, progress_total INTEGER,
    started REAL, finished REAL,
    duration_s INTEGER, first_departure TEXT, arrival TEXT, cantons INTEGER, complete INTEGER,
    result TEXT,                        -- itinerary JSON from the solver
    log TEXT
);
"""
LIST_COLS = ("id, created, station, date, start, sweep_end, quality, min_dwell, walk, bike, via, via_first, end_station, bonus, detour, ride, have, note, status, message, "
             "progress_done, progress_total, started, finished, duration_s, first_departure, arrival, cantons, complete")


# --------------------------------------------------------------------------- db
def db():
    con = sqlite3.connect(DB_PATH, timeout=30)
    con.row_factory = sqlite3.Row
    return con


def init_db():
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    with db() as con:
        con.executescript(SCHEMA)
        cols = {r[1] for r in con.execute("PRAGMA table_info(routes)")}
        # databases created by older versions
        if "bike" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN bike INTEGER NOT NULL DEFAULT 0")
        if "via" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN via TEXT")
        if "bonus" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN bonus INTEGER NOT NULL DEFAULT 0")
        if "detour" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN detour INTEGER NOT NULL DEFAULT 0")
        if "ride" not in cols:  # routes computed before the option existed didn't force the ride
            con.execute("ALTER TABLE routes ADD COLUMN ride INTEGER NOT NULL DEFAULT 0")
        if "have" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN have TEXT")
        if "via_first" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN via_first INTEGER NOT NULL DEFAULT 0")
        if "end_station" not in cols:
            con.execute("ALTER TABLE routes ADD COLUMN end_station TEXT")
        # jobs interrupted by a restart go back to the queue
        con.execute("UPDATE routes SET status='queued', message='requeued after restart' WHERE status='running'")


def update(rid, **fields):
    if not fields:
        return
    with db() as con:
        con.execute(f"UPDATE routes SET {', '.join(f'{k}=?' for k in fields)} WHERE id=?", (*fields.values(), rid))


# --------------------------------------------------------------------------- stations
def norm(s: str) -> str:
    s = unicodedata.normalize("NFKD", s.lower())
    return "".join(c for c in s if not unicodedata.combining(c)).replace("-", " ")


class Stations:
    """Station names (with canton) from the most recent built timetable, for autocomplete."""

    def __init__(self):
        self.lock = threading.Lock()
        self.src = None
        self.items = []  # (norm, name, canton, platforms)
        self.by_lower = {}

    def refresh(self):
        days = sorted(DAYS.glob("*/stops.tsv"), key=lambda p: p.stat().st_mtime) if DAYS.exists() else []
        src = days[-1] if days else None
        if src == self.src and self.items:
            return
        counts, canton = {}, {}
        if src:
            with open(src, encoding="utf-8") as f:
                next(f)
                for line in f:
                    r = line.rstrip("\n").split("\t")
                    name, c = r[2], int(r[3])
                    counts[name] = counts.get(name, 0) + 1
                    if c >= 0 or name not in canton:
                        canton[name] = c
        meta = json.loads((src.parent / "meta.json").read_text(encoding="utf-8")) if src else {"cantons": []}
        cn = meta["cantons"]
        items = [(norm(n), n, cn[canton[n]] if canton[n] >= 0 else "abroad", k) for n, k in counts.items()]
        with self.lock:
            self.src, self.items = src, items
            self.by_lower = {n.lower(): n for _, n, _, _ in items}

    def search(self, q: str, limit=12):
        self.refresh()
        q = norm(q.strip())
        if not q:
            return []
        out = []
        for nq, name, canton, k in self.items:
            if nq == q:
                rank = 0
            elif nq.startswith(q):
                rank = 1
            elif f" {q}" in f" {nq}" or f",{q}" in nq.replace(" ", ""):
                rank = 2
            elif q in nq:
                rank = 3
            else:
                continue
            out.append((rank, canton == "abroad", len(name), -k, name, canton))
        out.sort()
        if not out and len(q) > 3:  # typo: retry with a shorter prefix
            return self.search(q[:max(3, len(q) - 2)], limit)
        return [{"name": n, "canton": c} for *_, n, c in out[:limit]]

    def canonical(self, name: str):
        self.refresh()
        return self.by_lower.get(name.strip().lower())


STATIONS = Stations()


# --------------------------------------------------------------------------- jobs
def feed_range():
    p = ROOT / "feed_info.txt"
    if not p.exists():
        return None
    lines = p.read_text(encoding="utf-8-sig").splitlines()
    d = dict(zip([h.strip('"') for h in lines[0].split(",")], [v.strip('"') for v in lines[1].split(",")]))
    f = lambda s: dt.date(int(s[:4]), int(s[4:6]), int(s[6:8]))
    return f(d["feed_start_date"]), f(d["feed_end_date"])


def mem_available_gb():
    try:
        for line in open("/proc/meminfo"):
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) / 1e6
    except OSError:
        pass
    return 8.0


class Worker(threading.Thread):
    """Runs queued jobs one after another."""

    def __init__(self, max_threads):
        super().__init__(daemon=True)
        self.max_threads = max_threads
        self.wake = threading.Event()
        self.proc = None
        self.current = None
        self.cancelled = set()

    def kick(self):
        self.wake.set()

    def cancel(self, rid):
        self.cancelled.add(rid)
        if self.current == rid and self.proc:
            try:  # kill the whole process group (sbb wrapper + solver / build_day child)
                os.killpg(self.proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

    def run(self):
        while True:
            with db() as con:
                row = con.execute("SELECT * FROM routes WHERE status='queued' ORDER BY id LIMIT 1").fetchone()
            if not row:
                self.wake.wait(5)
                self.wake.clear()
                continue
            try:
                self.execute(dict(row))
            except Exception:
                update(row["id"], status="failed", message="internal error", log=traceback.format_exc(), finished=time.time())
            finally:
                self.proc = None
                self.current = None

    def threads_for(self, quality):
        auto = max(1, int((mem_available_gb() - 1.0) / QUALITY[quality]["gb"]))
        return max(1, min(auto, self.max_threads))

    def execute(self, job):
        rid = job["id"]
        self.current = rid
        threads = self.threads_for(job["quality"])
        tmp = Path(tempfile.mkdtemp(prefix="sbb-route-")) / "route.json"
        cmd = [sys.executable, str(SBB), "route", job["station"], "-d", job["date"], "-s", job["start"],
               "-b", QUALITY[job["quality"]]["beams"], "--min-dwell", str(job["min_dwell"]), "-j", str(tmp)]
        if job["sweep_end"]:
            cmd += ["--sweep-end", job["sweep_end"]]
        if not job["walk"]:
            cmd.append("--no-walk")
        elif not job["bike"]:
            cmd.append("--no-bike")
        for v in json.loads(job["via"] or "[]"):
            cmd += ["--via", v]
        if job["via_first"] and job["via"] and json.loads(job["via"]):
            cmd.append("--via-first")
        if job["end_station"]:
            cmd += ["--to", job["end_station"]]
        if not job["bonus"]:
            cmd.append("--no-bonus")
        if not job["detour"]:
            cmd.append("--no-detour")
        if not job["ride"]:
            cmd += ["--ride-together", ""]
        have = json.loads(job["have"] or "[]")
        if have:
            cmd += ["--have", ",".join(have)]
        cmd += ["--", "--threads", str(threads)]
        update(rid, status="running", started=time.time(), message=f"starting ({threads} threads)",
               progress_done=0, progress_total=None, log="")
        log = []
        self.proc = subprocess.Popen(cmd, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     text=True, bufsize=1, env={**os.environ, "NO_COLOR": "1"},
                                     start_new_session=True)
        last_flush = 0
        for line in self.proc.stdout:
            line = line.rstrip("\n")
            log.append(line)
            fields = self.parse(line)
            if fields or time.time() - last_flush > 2:
                fields["log"] = "\n".join(log[-400:])
                update(rid, **fields)
                last_flush = time.time()
        code = self.proc.wait()
        text = "\n".join(log[-400:])
        if rid in self.cancelled:
            update(rid, status="cancelled", message="cancelled", log=text, finished=time.time())
            return
        if code != 0 or not tmp.exists():
            err = next((l for l in reversed(log) if l.startswith("error") or "no station" in l), f"exit code {code}")
            update(rid, status="failed", message=err.removeprefix("error: "), log=text, finished=time.time())
            return
        res = json.loads(tmp.read_text(encoding="utf-8"))
        tmp.unlink(missing_ok=True)
        update(rid, status="done", finished=time.time(), log=text, result=json.dumps(res, ensure_ascii=False),
               duration_s=res["duration_s"], first_departure=res["first_departure"], arrival=res["arrival"],
               cantons=res["cantons_visited"], complete=int(res["complete"]),
               message="all 26 cantons" if res["complete"] else f"only {res['cantons_visited']} cantons")

    @staticmethod
    def parse(line):
        if m := re.match(r"progress (\d+)/(\d+)", line):
            return {"progress_done": int(m[1]), "progress_total": int(m[2])}
        if "older version" in line:
            return {"message": "rebuilding the day's timetable (new format)"}
        if "build_day.py" in line:
            return {"message": "preparing the day's timetable"}
        if "cargo build" in line:
            return {"message": "building the solver"}
        if line.startswith("phase search"):
            return {"message": "searching " + line.split(":", 1)[1].strip()}
        if line.startswith("phase refine"):
            return {"message": "refining: " + line.split(":", 1)[1].strip()}
        return {}


# --------------------------------------------------------------------------- http
CTYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
          ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml", ".png": "image/png", ".ico": "image/x-icon"}


class Api(BaseHTTPRequestHandler):
    server_version = "sbb-web/1"
    worker: Worker = None

    def log_message(self, fmt, *args):  # quieter console
        if not self.path.startswith("/api/routes") or self.command != "GET":
            sys.stderr.write(f"{self.command} {self.path} -> {args[1] if len(args) > 1 else ''}\n")

    # -- helpers
    def send_json(self, obj, status=200):
        body = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def error(self, status, msg, **extra):
        self.send_json({"error": msg, **extra}, status)

    def send_file(self, path: Path, ctype=None):
        if not path.is_file():
            return self.error(404, "not found")
        data = path.read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", ctype or CTYPES.get(path.suffix, "application/octet-stream"))
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(data)

    def body_json(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n > 100_000:
            raise ValueError("request too large")
        return json.loads(self.rfile.read(n) or b"{}")

    # -- routing
    def do_GET(self):
        u = urlparse(self.path)
        p = u.path
        if p in ("/", "/index.html"):
            return self.send_file(STATIC / "index.html")
        if p.startswith("/static/"):
            f = (STATIC / p.removeprefix("/static/")).resolve()
            if STATIC.resolve() not in f.parents:
                return self.error(404, "not found")
            return self.send_file(f)
        if p == "/api/meta":
            rng = feed_range()
            today = dt.date.today()
            sat = today + dt.timedelta(days=(5 - today.weekday()) % 7 or 7)
            if rng and not (rng[0] <= sat <= rng[1]):
                sat = rng[0]
            return self.send_json({
                "feed": [rng[0].isoformat(), rng[1].isoformat()] if rng else None,
                "default_date": sat.isoformat(),
                "qualities": [{"key": k, **{kk: v[kk] for kk in ("label", "hint")}} for k, v in QUALITY.items()],
                "max_sweep_hours": MAX_SWEEP_H,
                "canton_codes": CANTON_CODES,
                "cpus": os.cpu_count(),
                "has_timetable": bool(STATIONS.items),
            })
        if p == "/api/bonus":
            rows = []
            for name, finish in (("bonus_stations.tsv", False), ("finish_stations.tsv", True)):
                f = ROOT / name
                if f.exists():
                    for line in f.read_text(encoding="utf-8").splitlines():
                        parts = [x.strip() for x in line.split("\t")]
                        if line.strip() and not line.startswith("#") and len(parts) >= 3:
                            rows.append({"station": parts[0], "minutes": parts[1], "cantons": parts[2], "finish": finish})
            return self.send_json(rows)
        if p == "/api/detours":
            rows = []
            f = ROOT / "detour_rules.tsv"
            if f.exists():
                for line in f.read_text(encoding="utf-8").splitlines():
                    parts = [x.strip() for x in line.split("\t")]
                    if line.strip() and not line.startswith("#") and len(parts) >= 2:
                        rows.append({"station": parts[0], "cantons": parts[1]})
            return self.send_json(rows)
        if p == "/api/stations":
            q = parse_qs(u.query).get("q", [""])[0]
            return self.send_json(STATIONS.search(q))
        if p == "/api/cantons":
            if not SHAPES.exists():
                return self.error(503, "canton shapes missing — run ./sbb setup")
            return self.send_file(SHAPES, "application/geo+json")
        if p == "/api/routes":
            with db() as con:
                rows = con.execute(f"SELECT {LIST_COLS} FROM routes ORDER BY id DESC").fetchall()
            return self.send_json([dict(r) for r in rows])
        if m := re.fullmatch(r"/api/routes/(\d+)", p):
            with db() as con:
                r = con.execute("SELECT * FROM routes WHERE id=?", (int(m[1]),)).fetchone()
            if not r:
                return self.error(404, "no such route")
            d = dict(r)
            d["result"] = json.loads(d["result"]) if d["result"] else None
            return self.send_json(d)
        self.error(404, "not found")

    def do_POST(self):
        p = urlparse(self.path).path
        if p == "/api/routes":
            try:
                b = self.body_json()
            except (ValueError, json.JSONDecodeError) as e:
                return self.error(400, f"bad request: {e}")
            return self.create(b)
        if m := re.fullmatch(r"/api/routes/(\d+)/note", p):
            b = self.body_json()
            update(int(m[1]), note=str(b.get("note", ""))[:200])
            return self.send_json({"ok": True})
        self.error(404, "not found")

    def do_DELETE(self):
        if m := re.fullmatch(r"/api/routes/(\d+)", urlparse(self.path).path):
            rid = int(m[1])
            self.worker.cancel(rid)
            with db() as con:
                con.execute("DELETE FROM routes WHERE id=?", (rid,))
            return self.send_json({"ok": True})
        self.error(404, "not found")

    def create(self, b):
        station = STATIONS.canonical(str(b.get("station", "")))
        if not station:
            return self.error(400, "unknown station — pick one from the suggestions",
                              suggestions=STATIONS.search(str(b.get("station", "")), 5))
        try:
            date = dt.date.fromisoformat(str(b.get("date", "")))
        except ValueError:
            return self.error(400, "invalid date")
        rng = feed_range()
        if rng and not (rng[0] <= date <= rng[1]):
            return self.error(400, f"date outside the timetable ({rng[0]} .. {rng[1]})")
        start = str(b.get("start", "05:00"))
        sweep_end = str(b.get("sweep_end") or "") or None
        for t in [start] + ([sweep_end] if sweep_end else []):
            if not TIME_RE.match(t):
                return self.error(400, f"invalid time {t!r} (HH:MM)")
        mins = lambda t: int(t.split(":")[0]) * 60 + int(t.split(":")[1])
        if sweep_end and not (0 <= mins(sweep_end) - mins(start) <= MAX_SWEEP_H * 60):
            return self.error(400, f"'try until' must be after the start and at most {MAX_SWEEP_H} h later")
        quality = b.get("quality", "quick")
        if quality not in QUALITY:
            return self.error(400, "invalid quality")
        try:
            min_dwell = max(0, min(600, int(b.get("min_dwell", 0))))
        except (TypeError, ValueError):
            return self.error(400, "invalid min dwell")
        walk = 1 if b.get("walk", True) else 0
        bike = 1 if (walk and b.get("bike", True)) else 0
        bonus = 1 if b.get("bonus", True) else 0
        detour = 1 if b.get("detour", True) else 0
        ride = 1 if b.get("ride", True) else 0
        via_first = 1 if b.get("via_first", False) else 0
        end_station = None
        if str(b.get("end_station") or "").strip():
            end_station = STATIONS.canonical(str(b["end_station"]))
            if not end_station:
                return self.error(400, f"unknown end station {b['end_station']!r}",
                                  suggestions=STATIONS.search(str(b["end_station"]), 5))
        have = b.get("have") or []
        if not isinstance(have, list) or any(c not in CANTON_CODES for c in have):
            return self.error(400, "invalid 'already validated' cantons")
        if len(set(have)) >= 26:
            return self.error(400, "all 26 cantons are already validated — nothing to compute")
        via_in = b.get("via") or []
        if not isinstance(via_in, list) or len(via_in) > 5:
            return self.error(400, "at most 5 via stations")
        via = []
        for v in via_in:
            cv = STATIONS.canonical(str(v))
            if not cv:
                return self.error(400, f"unknown via station {v!r}", suggestions=STATIONS.search(str(v), 5))
            via.append(cv)
        with db() as con:
            cur = con.execute(
                "INSERT INTO routes (created, station, date, start, sweep_end, quality, min_dwell, walk, bike, via,"
                " via_first, end_station, bonus, detour, ride, have, note, status, message)"
                " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, 'queued', 'waiting in queue')",
                (dt.datetime.now().isoformat(timespec="seconds"), station, date.isoformat(), start, sweep_end,
                 quality, min_dwell, walk, bike, json.dumps(via, ensure_ascii=False), via_first, end_station, bonus,
                 detour, ride, json.dumps(sorted(set(have), key=CANTON_CODES.index)), str(b.get("note", ""))[:200]))
            rid = cur.lastrowid
        self.worker.kick()
        self.send_json({"id": rid}, 201)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="127.0.0.1", help="interface to listen on (0.0.0.0 = whole network)")
    ap.add_argument("--port", type=int, default=8026)
    ap.add_argument("--threads", type=int, default=os.cpu_count() or 4,
                    help="max parallel searches per job (also limited by free RAM)")
    ap.add_argument("--open", action="store_true", help="open the browser")
    a = ap.parse_args()

    init_db()
    STATIONS.refresh()
    if not STATIONS.items:
        # station search needs one built timetable; build the default date in the background
        today = dt.date.today()
        sat = today + dt.timedelta(days=(5 - today.weekday()) % 7 or 7)
        print(f"no timetable built yet — preparing {sat} for station search…", file=sys.stderr)
        threading.Thread(target=lambda: subprocess.run([sys.executable, str(SBB), "day", sat.isoformat()], cwd=ROOT),
                         daemon=True).start()
    worker = Worker(a.threads)
    worker.start()
    Api.worker = worker
    srv = ThreadingHTTPServer((a.host, a.port), Api)
    url = f"http://{'localhost' if a.host in ('127.0.0.1', '0.0.0.0') else a.host}:{a.port}/"
    print(f"sbb web app on {url}  (Ctrl+C to stop)", file=sys.stderr)
    if a.open:
        threading.Timer(0.5, lambda: webbrowser.open(url)).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        worker.cancel(worker.current)


if __name__ == "__main__":
    main()
