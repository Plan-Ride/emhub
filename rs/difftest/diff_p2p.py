"""
On the hub: p2p/pack, p2p/boards, p2p/key, notices and fares, Python against Rust, byte for byte: the same live bus
sources (saved to <out>/live for HUB_TEST_LIVE), one fixed moment (HUB_FAKE_NOW) and a stand-in signature on both
sides (HUB_TEST_FIXED_SIG; ECDSA signatures differ every time by design).
"""
import base64, io, json, os, random, sys, time, urllib.parse, urllib.request
sys.path.insert(0, sys.argv[1])
import hub, timetable, sadzv, fleet  # noqa: E402

out = sys.argv[2]
os.makedirs(f"{out}/live", exist_ok=True)
hub.load_map_edits()
timetable.load(hub.GTFS_DIR, hub.gtfs_feeds())
t = timetable.current
with open(f"{out}/snapshot.json", "w", encoding="utf-8") as f:
    json.dump(timetable.snapshot(t), f, ensure_ascii=False, separators=(",", ":"))


def get(url, body=None, headers=None):
    req = urllib.request.Request(url, body, dict({"User-Agent": "emhub (Plan&Ride)"}, **(headers or {})))
    with urllib.request.urlopen(req, timeout=20) as r:
        return r.read()


raw_sadzv = get(sadzv.BASE + "getAllRtdVehicles", b"{}", {"X-Requested-With": "XMLHttpRequest", "Content-Type": "application/json; charset=utf-8"})
host = "m.mhdbb.qrbus.me"
raw_fleet = get(f"https://{host}/api/rest/rtdwebglobal/getVehicleStatus",
                json.dumps({"lon1": -18000000, "lat1": -9000000, "lon2": 18000000, "lat2": 9000000}).encode(),
                {"Content-Type": "application/json", "Origin": f"https://{host}", "Referer": f"https://{host}/transport-detail"})
raw_page = get(fleet.PORTAL, headers={"X-Requested-With": "XMLHttpRequest"}) if getattr(fleet, "PORTAL", "") else None
for name, data in (("sadzv.json", raw_sadzv), ("fleet.json", raw_fleet), ("page.txt", raw_page)):
    if data is not None:
        open(f"{out}/live/{name}", "wb").write(data)
NOW = int(time.time())
open(f"{out}/now.txt", "w").write(str(NOW))
time.time = lambda: float(NOW)
hub.now = lambda: NOW
sadzv._call = lambda path, body=None: json.loads(raw_sadzv)


def _fetch(host):
    root = json.loads(raw_fleet)
    return root.get("vehicles") or root.get("vehicleStatus") or []


class _Page(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


fleet._fetch = _fetch
fleet.urllib.request.urlopen = lambda req, timeout=None: _Page(raw_page or b"")
sadzv._cache.clear()
fleet._cities.clear()
hub._p2p_cache.clear()
hub.p2p_sign = lambda d, message: b"\x30\x06\x02\x01\x01\x02\x01\x01"


def answer(route, query):
    q = dict(urllib.parse.parse_qsl(query))
    try:
        if route == "p2p/pack":
            try:
                lat, lon = float(q.get("lat") or ""), float(q.get("lon") or "")
            except ValueError:
                lat = lon = 0.0
            if not (47 < lat < 50 and 16.5 < lon < 23):
                return [400, {"error": "lat and lon in Slovakia"}]
            return [200, hub.p2p_pack(round(lat, 3), round(lon, 3))]
        if route == "p2p/boards":
            try:
                lat, lon = float(q.get("lat") or ""), float(q.get("lon") or "")
                codes = [int(x, 16) & 0xFFFFFF for x in (q.get("codes") or "").split(",") if x][:3]
            except ValueError:
                lat = lon = 0.0
                codes = []
            if not (47 < lat < 50 and 16.5 < lon < 23) or not codes:
                return [400, {"error": "codes, and lat and lon in Slovakia"}]
            return [200, hub.p2p_asked(codes, round(lat, 3), round(lon, 3))]
        if route == "p2p/key":
            return [200, {"key": base64.b64encode(hub.p2p_public_der(hub.p2p_key())).decode()}]
        if route == "notices":
            with hub.db() as conn:
                return [200, {"notices": hub.active_notices(conn)}]
        if route == "fares":
            return [200, hub.fare_table()]
    except (ValueError, json.JSONDecodeError) as error:
        return [400, {"error": str(error)}]


random.seed(12)
places = [s for s in range(len(t.names)) if t.lat[s] is not None and s not in t.deleted and 47 < t.lat[s] < 50]
bb_zv = [s for s in places if 48.5 < t.lat[s] < 48.8 and 19.0 < t.lon[s] < 19.3]
q = [("p2p/key", ""), ("notices", ""), ("fares", "")]
for s in random.sample(bb_zv, 150) + random.sample(places, 100):
    q.append(("p2p/pack", urllib.parse.urlencode({"lat": round(t.lat[s] + random.uniform(-.003, .003), 5), "lon": round(t.lon[s] + random.uniform(-.003, .003), 5)})))
for _ in range(120):
    asked = [hub.p2p_hash(t.station_ids[s]) for s in random.sample(bb_zv, random.randint(1, 4))]
    s = random.choice(bb_zv)
    q.append(("p2p/boards", urllib.parse.urlencode({"lat": t.lat[s], "lon": t.lon[s], "codes": ",".join(f"{c:x}" for c in asked)})))
for bad in [("p2p/pack", "lat=x&lon=19"), ("p2p/pack", "lat=52&lon=19"), ("p2p/pack", ""), ("p2p/boards", "lat=48.7&lon=19.1"),
            ("p2p/boards", "lat=48.7&lon=19.1&codes=zz"), ("p2p/boards", "lat=48.7&lon=19.1&codes=0x1f,-2,abc_d"), ("p2p/boards", "codes=1f")]:
    q.append(bad)
answers = [answer(r, x) for r, x in q]
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
packs = [a[1] for (r, _), a in zip(q, answers) if r == "p2p/pack" and a[0] == 200]
print(len(q), "asked;", len(packs), "packs,", sum(p["buses"] for p in packs), "bus rows,", sum(1 for p in packs if "board" in p), "with a board;",
      sum(len(a[1]["boards"]) for (r, _), a in zip(q, answers) if r == "p2p/boards" and a[0] == 200), "asked boards")
