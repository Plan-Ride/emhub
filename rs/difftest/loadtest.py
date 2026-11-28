"""
Load test of emHUB's public API through https://par.karafa.net, as the apps call it. GET only (nothing is written);
plan searches carry pre=1 so they aren't counted in the search stats. Key from HUB_KEY env, never printed.
Each scenario: N requests at C at once; per-thread connection reused like OkHttp (or a new one each time).
After each scenario the hub's CPU seconds used (systemd CPUUsageNSec) are read over ssh.
"""
import http.client
import json
import os
import random
import ssl
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

HOST = os.environ.get("HUB_HOST_NAME", "par.karafa.net")
H = {"X-App-Key": os.environ["HUB_KEY"], "User-Agent": "emhub-loadtest", "Accept-Encoding": "gzip"}
# The hub over ssh, for its CPU and memory after each scenario (HUB_SSH="ssh -i key root@host"; empty: not read).
SSH = os.environ.get("HUB_SSH", "").split()
CTX = ssl.create_default_context()
local = threading.local()
random.seed(7)


def cpu():
    if not SSH:
        return 0.0, 0.0, 0
    out = subprocess.run(SSH + ["systemctl show emhub -p CPUUsageNSec -p MemoryCurrent -p TasksCurrent"], capture_output=True, text=True).stdout
    kv = dict(line.split("=", 1) for line in out.split())
    return int(kv["CPUUsageNSec"]) / 1e9, int(kv["MemoryCurrent"]) / 2**20, int(kv["TasksCurrent"])


def call(path, q, fresh=False):
    url = "/api/v1/" + path + ("?" + urllib.parse.urlencode(q) if q else "")
    t = time.perf_counter()
    for attempt in (0, 1):
        conn = None if fresh else getattr(local, "conn", None)
        if conn is None:
            conn = http.client.HTTPSConnection(HOST, timeout=90, context=CTX)
        try:
            conn.request("GET", url, headers=H)
            r = conn.getresponse()
            body = r.read()
            if fresh or r.getheader("Connection", "").lower() == "close":
                conn.close(); local.conn = None
            else:
                local.conn = conn
            return path, r.status, (time.perf_counter() - t) * 1000, len(body)
        except Exception as e:  # noqa: BLE001
            conn.close(); local.conn = None
            if attempt or fresh:
                return path, f"ERR {type(e).__name__}", (time.perf_counter() - t) * 1000, 0


# ---- real ids from the timetable
def setup():
    _, _, _, _ = call("health", {})
    conn = http.client.HTTPSConnection(HOST, timeout=60, context=CTX)
    def j(path, q):
        conn.request("GET", "/api/v1/" + path + "?" + urllib.parse.urlencode(q), headers={k: v for k, v in H.items() if k != "Accept-Encoding"})
        return json.loads(conn.getresponse().read())
    stops, trips = [], []
    for lat, lon in PLACES[:6]:
        d = j("timetable/departures", {"lat": lat, "lon": lon, "radius": 400, "modes": MODES})
        stops += [s["id"] for s in d["stops"]]
        trips += [x["tripId"] for x in d["departures"]]
    return sorted(set(stops)), sorted(set(trips))


MODES = "bus,city,rail"
PLACES = [(48.736, 19.146), (48.577, 19.125), (48.592, 19.157), (48.330, 19.667), (48.797, 19.646), (48.571, 19.124),
          (48.587, 18.849), (48.640, 19.030), (48.524, 19.371), (48.434, 19.001), (48.749, 19.152), (48.725, 19.120)]
TOWNS = ["town:zvolen", "town:banska-bystrica", "town:detva", "town:brezno", "town:lucenec", "town:krupina", "town:ziar-nad-hronom",
         "town:sliac", "town:hrinova", "town:banska-stiavnica"]
WORDS = ["zvolen", "banska", "lucenec", "detva", "brezno", "krupina", "sliac", "ziar", "hrinova", "moyses", "nemocnica", "zelezni",
         "tulska", "podlavice", "fonclova", "rakytovce", "kremnica", "hronec", "lieskovec", "zvolenska"]


def jitter(p, m=0.01):
    return round(p[0] + random.uniform(-m, m), 4), round(p[1] + random.uniform(-m, m), 4)


def make(kind, stops, trips):
    """One request of a kind, with random real-looking parameters (what different phones ask)."""
    lat, lon = jitter(random.choice(PLACES))
    tid = random.choice(trips)
    parts = tid.split(":")[1].split("-") if ":" in tid else ["0", "0"]
    return {
        "health": ("health", {}),
        "stops_q": ("timetable/stops", {"q": random.choice(WORDS), "modes": MODES}),
        "stops_near": ("timetable/stops", {"lat": lat, "lon": lon, "modes": MODES}),
        "departures": ("timetable/departures", {"stop": random.choice(stops), "modes": MODES}),
        "departures_near": ("timetable/departures", {"lat": lat, "lon": lon, "modes": MODES}),
        "plan": ("timetable/plan", dict(zip(("from", "to"), random.sample(TOWNS, 2)), modes=MODES, pre=1)),
        "trip": ("timetable/trip", {"id": tid}),
        "bus": ("timetable/bus", {"lineId": parts[1] if len(parts) > 2 else 0, "trip": parts[-1] if parts[-1].isdigit() else 0}),
        "path": ("timetable/path", {"stops": ";".join(f"{a},{b}" for a, b in (jitter(PLACES[0], .003), jitter(PLACES[0], .006), jitter(PLACES[0], .009)))}),
        "community_delay": ("community/delay", {"trip": tid}),
        "notices": ("notices", {}),
        "fares": ("fares", {}),
        "p2p_pack": ("p2p/pack", {"lat": lat, "lon": lon}),
        "weather": ("timetable/weather", {"lat": round(lat, 3), "lon": round(lon, 3)}),
    }[kind]


# how often the app asks each, roughly (tracker polls, boards, searches)
MIX = {"departures": 20, "departures_near": 10, "stops_q": 10, "stops_near": 10, "trip": 10, "bus": 12, "community_delay": 10,
       "plan": 6, "path": 3, "notices": 2, "fares": 1, "p2p_pack": 2, "weather": 2, "health": 2}


def stats(rows, wall):
    ms = sorted(r[2] for r in rows)
    ok = sum(1 for r in rows if r[1] == 200)
    q = lambda p: ms[min(len(ms) - 1, int(p * len(ms)))]  # noqa: E731
    codes = {}
    for r in rows:
        codes[str(r[1])] = codes.get(str(r[1]), 0) + 1
    return {"n": len(rows), "ok": ok, "codes": codes, "rps": round(len(rows) / wall, 1), "p50": round(q(.5)), "p90": round(q(.9)),
            "p99": round(q(.99)), "max": round(ms[-1]), "mean": round(statistics.mean(ms))}


def run(name, reqs, c, fresh=False):
    local.__dict__.clear()
    c0 = cpu()
    t = time.perf_counter()
    with ThreadPoolExecutor(c) as pool:
        rows = list(pool.map(lambda r: call(*r, fresh=fresh), reqs))
    wall = time.perf_counter() - t
    c1 = cpu()
    s = dict(stats(rows, wall), scenario=name, c=c, wall=round(wall, 1), cpu_s=round(c1[0] - c0[0], 1), mem_mb=round(c1[1]), tasks=c1[2])
    per = {}
    for r in rows:
        per.setdefault(r[0], []).append(r)
    s["per"] = {k: stats(v, wall) for k, v in per.items()} if len(per) > 1 else {}
    print(f"{name:<28} c={c:<3} n={s['n']:<4} ok={s['ok']:<4} {s['rps']:>6} r/s  p50 {s['p50']:>6}  p90 {s['p90']:>6}  p99 {s['p99']:>6}"
          f"  max {s['max']:>6} ms  cpu {s['cpu_s']:>5}s  mem {s['mem_mb']} MB  {'' if s['ok'] == s['n'] else s['codes']}", flush=True)
    return s


def main():
    stops, trips = setup()
    print(f"{len(stops)} stops, {len(trips)} trips; hub {cpu()}", flush=True)
    out = []
    # 1. each call alone, 8 times one after another (reused connection)
    for kind in MIX:
        out.append(run(f"alone {kind}", [make(kind, stops, trips) for _ in range(8)], 1))
    # 2. the same request from many phones at once
    same_dep = make("departures", stops, trips)
    same_plan = make("plan", stops, trips)
    for c in (1, 10, 25, 50):
        out.append(run("same departures", [same_dep] * max(20, 4 * c), c))
    for c in (1, 5, 10, 20):
        out.append(run("same plan", [same_plan] * max(10, 3 * c), c))
    # 3. different plans at once (the heaviest call)
    for c in (5, 10, 20):
        out.append(run("different plans", [make("plan", stops, trips) for _ in range(3 * c)], c))
    # 4. the app's mix of different requests
    kinds = [k for k, w in MIX.items() for _ in range(w)]
    for c in (1, 10, 25, 50, 100):
        out.append(run("mixed", [make(random.choice(kinds), stops, trips) for _ in range(max(60, 4 * c))], c))
    # 5. new connection for every request (phones waking up / no keep-alive)
    out.append(run("mixed, new connection each", [make(random.choice(kinds), stops, trips) for _ in range(100)], 10, fresh=True))
    json.dump(out, open(sys.argv[1] if len(sys.argv) > 1 else "results.json", "w"), indent=1)


main()
