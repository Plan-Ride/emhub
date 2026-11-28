"""
On the hub: the live buses and timetable/bus, Python against Rust on the same data. The three live sources are fetched
once and saved (<out>/live: sadzv.json, fleet.json, page.txt); Python's sadzv.vehicles() merges those same bytes at one
fixed moment (<out>/now.txt). queries.txt: "gps" (the merged buses) and timetable/bus for every bus and every city trip
of the day. Rust: HUB_TEST_LIVE=<out>/live HUB_FAKE_NOW=$(cat now.txt) emhub answer snapshot.json queries.txt.
"""
import io, json, os, random, sys, time, urllib.parse, urllib.request
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

# Python's merging on exactly those answers, at NOW
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
gps = sadzv.vehicles()


def as_list(b):
    return [list(x) if isinstance(x, tuple) else x for x in b]


rows = [[k[0], k[1], as_list(b)] for k, b in sorted((k, b) for k, b in gps.items() if k[1] != "portal")]
rows += [[k[0], "portal", [as_list(b) for b in v]] for k, v in sorted((k, v) for k, v in gps.items() if k[1] == "portal")]
q, answers = [("gps", "")], [[200, rows]]


def bus(query, when=None):
    when = NOW if when is None else when
    params = dict(urllib.parse.parse_qsl(query))
    try:
        line_id, number = int(params.get("lineId") or 0), int(params.get("trip") or 0)
    except ValueError:
        return [400, {"error": "lineId and trip are numbers"}]
    return [200, {"bus": t.city_bus(line_id, number, when, gps) if line_id else None,
                  "checked": bool(line_id) and t.city_trip(line_id, number), "at": when}]


random.seed(5)
day = (timetable.datetime.fromtimestamp(NOW, timetable.ZONE).date() - t.first_day).days
asks = {(k[0], k[1]) for k in gps if k[1] != "portal"}
today = [(c[0], c[2]) for i, c in t.city.items() if day in t.services.get(t.trips[i][6], ())]
asks |= set(random.sample(today, min(3000, len(today))))
asks |= {(k[0], random.randint(1, 900)) for k in gps for _ in range(5)}
for line_id, number in sorted(asks):
    q.append(("bus", urllib.parse.urlencode({"lineId": line_id, "trip": number})))
for bad in [{"lineId": "x", "trip": "1"}, {"lineId": "0", "trip": "5"}, {}, {"lineId": "77"}, {"lineId": " 77 ", "trip": "1.5"}]:
    q.append(("bus", urllib.parse.urlencode(bad)))
answers += [bus(x) for r, x in q[1:]]
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
found = sum(1 for a in answers[1:] if a[0] == 200 and a[1]["bus"])
print(len(q), "queries;", len(rows), "gps rows;", found, "buses found;", sum(1 for a in answers[1:] if a[0] == 200 and a[1]["checked"]), "checked")

# Round 2: made-up buses where real trips are right now, so matching by place, start and destination is compared too.
base = timetable.base_epoch(timetable.datetime.fromtimestamp(NOW, timetable.ZONE).date())
fake, asks2, n = {}, [], 0
for when in (base + 43200 - 5 * 3600 + 1800, base + 43200, base + 43200 + 4 * 3600, NOW):  # 07:30, 12:00, 16:00, now
    running = [(i, c) for i, c in t.city.items() if day in t.services.get(t.trips[i][6], ()) and t._spot(t.trips[i], when - base)]
    random.shuffle(running)
    for i, c in running[:150]:
        n += 1
        trip = t.trips[i]
        delay = random.choice([0, 60, 180, -60])
        at = t._spot(trip, when - base - delay)
        if not at:
            continue
        lat, lon = at[1] + random.uniform(-.0004, .0004), at[2] + random.uniform(-.0004, .0004)
        end = timetable.fold((trip[3] or t.names[trip[7][-1]]).split(",", 1)[-1])
        kind, plate, start = n % 6, f"T{n}", base + trip[8][0] + random.choice([0, 30, -90])
        if kind == 0:    # its own number, no fleet start: destination and place decide
            fake[(c[0], c[2])] = (lat, lon, delay, end, plate, 0)
        elif kind == 1:  # a wrong number, but the fleet's start fits: placed by start and place
            b = (lat, lon, delay, ["elsewhere"], plate, start)
            fake[(c[0], 9000 + n)] = b
            fake.setdefault((c[0], "portal"), []).append(b)
        elif kind == 2:  # the city page: destination and place
            fake.setdefault((c[0], "portal"), []).append((lat, lon, delay, end, plate))
        elif kind == 3:  # the city page without a destination: place only
            fake.setdefault((c[0], "portal"), []).append((lat, lon, delay, [], plate))
        elif kind == 4:  # its own number but it left 10 min off: not trusted, not placed
            fake[(c[0], c[2])] = (lat, lon, delay, end, plate, start + 600)
        else:            # far away
            fake.setdefault((c[0], "portal"), []).append((lat + 0.03, lon, delay, end, plate))
        asks2.append((c[0], c[2], when))
rows2 = [[k[0], k[1], as_list(b)] for k, b in sorted((k, b) for k, b in fake.items() if k[1] != "portal")]
rows2 += [[k[0], "portal", [as_list(b) for b in v]] for k, v in sorted((k, v) for k, v in fake.items() if k[1] == "portal")]
json.dump(rows2, open(f"{out}/fake_gps.json", "w"))
gps = fake
q2 = [("bus", urllib.parse.urlencode({"lineId": a, "trip": b}), when) for a, b, when in asks2]
with open(f"{out}/queries2.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\t{when}\n" for r, x, when in q2)
with open(f"{out}/python2.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(bus(x, when), ensure_ascii=False) + "\n" for r, x, when in q2)
print("round 2:", len(q2), "made-up buses on running trips")
