"""
On the hub: load the timetable as emHUB starts it, write the snapshot, and Python's answers to many queries
(route<TAB>query per line in queries.txt, [status, body] per line in python.jsonl). Rust then answers queries.txt
from the snapshot and diff_compare.py compares. Fixed times, so nothing live differs.
"""
import json, random, sys, urllib.parse
sys.path.insert(0, sys.argv[1])
import hub, timetable  # noqa: E402

out = sys.argv[2]
hub.load_map_edits()
timetable.load(hub.GTFS_DIR, hub.gtfs_feeds())
t = timetable.current
with open(f"{out}/snapshot.json", "w", encoding="utf-8") as f:
    json.dump(timetable.snapshot(t), f, ensure_ascii=False, separators=(",", ":"))

random.seed(11)
M = ["bus,city,rail", "bus,city", "rail", "city", None]
TIMES = [1795845600, 1795875000, 1795905000, 1795912200, 1795930000, 1795960000, 1796000000, "2026-11-29T07:30", "2026-12-24T23:50"]
q = []


def add(route, **params):
    params = {k: v for k, v in params.items() if v is not None}
    q.append((route, urllib.parse.urlencode(params)))


live = [s for s in range(len(t.names)) if t.lat[s] is not None and s not in t.deleted]
for _ in range(1500):
    name = t.names[random.choice(range(len(t.names)))]
    words = [w for w in timetable.fold(name) if w]
    cut = [w[:random.randint(1, len(w))] for w in words]
    text = " ".join(cut[:random.randint(1, len(cut))]) if cut else "x"
    if random.random() < 0.15:
        text = text.upper() + random.choice(["", " ", "č", "-AS"])
    add("stops", q=text, modes=random.choice(M), n=random.choice([None, None, "3", "50", "100"]))
for text in ["", "zzzzq", "bb as", "z z p k", "nabrezie", "zvolen vlak", "Žiar n.Hronom", "1", "nám š moysesa", "zvol zla po kon"]:
    add("stops", q=text, modes="bus,city,rail")
for _ in range(500):
    s = random.choice(live)
    add("stops", lat=round(t.lat[s] + random.uniform(-.004, .004), 5), lon=round(t.lon[s] + random.uniform(-.004, .004), 5),
        modes=random.choice(M), radius=random.choice([None, "100", "800", "5000"]), n=random.choice([None, "20"]))
towns = list(t.towns())
for _ in range(1200):
    pick = random.random()
    if pick < 0.6:
        stop = ",".join(t.station_ids[random.choice(live)] for _ in range(random.choice([1, 1, 1, 2])))
    elif pick < 0.75:
        stop = "town:" + random.choice(towns).replace(" ", "-")
    else:
        stop = None
    s = random.choice(live)
    add("departures", stop=stop, lat=None if stop else round(t.lat[s], 5), lon=None if stop else round(t.lon[s], 5),
        time=random.choice(TIMES), modes=random.choice(M), n=random.choice([None, "5", "200"]),
        ending=random.choice([None, None, "1"]), exclude=random.choice([None, None, "sad-zvolen-as", "zsr,dpb"]),
        radius=random.choice([None, "400"]))
for extra in [dict(stop="nope"), dict(lat="x", lon="1"), dict(stop=t.station_ids[live[0]], n="abc"), dict(stop=t.station_ids[live[0]], time="nope"), {}]:
    add("departures", **extra)
add("stops", lat="48.7", lon="19.1", radius="abc")
add("stops")


def answer(route, query):
    try:
        status, body = hub.timetable_query(route, dict(urllib.parse.parse_qsl(query)))
    except (ValueError, json.JSONDecodeError) as error:
        status, body = 400, {"error": str(error)}
    return [status, body]


# community/delay at one fixed moment just after the newest report (HUB_FAKE_NOW for Rust)
with hub.db() as conn:
    NOW = conn.execute("SELECT MAX(created_at) FROM community_reports").fetchone()[0] + 90
    keys = [r[0] for r in conn.execute("SELECT trip_key FROM community_reports GROUP BY trip_key ORDER BY MAX(created_at) DESC LIMIT 1200")]
    keys += [r[0] for r in conn.execute("SELECT DISTINCT trip_key FROM crowd_reports ORDER BY created_at DESC LIMIT 200")]
hub.now = lambda: NOW
open(f"{out}/now.txt", "w").write(str(NOW))
for k in keys:
    add("community/delay", trip=k)
day = timetable.datetime.fromtimestamp(NOW, timetable.ZONE).date()
for i in random.sample(range(len(t.trips)), 3000):
    if (day - t.first_day).days in t.services.get(t.trips[i][6], ()):
        for k in t.community_keys(i, day):
            add("community/delay", trip=k)
for bad in ["", " ", "x", "a|b|c", "sadzv|1|2|nope|5", "x" * 400]:
    add("community/delay", trip=bad)


def answer_community(query):
    key = hub.clean(dict(urllib.parse.parse_qsl(query)).get("trip"), 300)
    if not key:
        return [400, {"error": "trip is required"}]
    try:
        return [200, hub.community_delay(key)]
    except (ValueError, json.JSONDecodeError) as error:
        return [400, {"error": str(error)}]


answers = [answer_community(x) if r == "community/delay" else answer(r, x) for r, x in q]
# ...and each at its own moment: 30 s after one of its reports, so live delays (phones, taps, outliers) are compared
with hub.db() as conn:
    moments = conn.execute("SELECT trip_key, created_at FROM community_reports ORDER BY RANDOM() LIMIT 1500").fetchall()
for key, at in moments:
    hub.now = lambda at=at: at + 30
    q.append(("community/delay", urllib.parse.urlencode({"trip": key}) + f"\t{at + 30}"))
    answers.append(answer_community(q[-1][1].split("\t")[0]))
hub.now = lambda: NOW
trips = sorted({d["tripId"] for status, body in answers if status == 200 for d in body.get("departures", [])})
random.shuffle(trips)
for tid in trips[:2500]:
    q.append(("trip", urllib.parse.urlencode({"id": tid})))
    answers.append(answer("trip", q[-1][1]))
for bad in ["x", "a:b:c", "nope:x:20261128", t.trips[0][0] + ":" + t.trips[0][1] + ":20261399"]:
    q.append(("trip", urllib.parse.urlencode({"id": bad})))
    answers.append(answer("trip", q[-1][1]))
# path: the stops of trips as the apps send them, some moved a little, and broken ones
for status, body in answers[-2504:-4]:
    if status == 200 and body.get("tripId") and random.random() < 0.4:
        pts = [(s["lat"], s["lon"]) for s in body["stops"] if s.get("lat") is not None]
        if random.random() < 0.3:
            pts = [(round(a + random.uniform(-.0005, .0005), 6), round(b + random.uniform(-.0005, .0005), 6)) for a, b in pts]
        q.append(("path", urllib.parse.urlencode({"stops": ";".join(f"{a},{b}" for a, b in pts)})))
        answers.append(answer("path", q[-1][1]))
for bad in ["", "48.7", "48.7,19.1;x,1", "48.7,19.1;48.71", "48.7,19.1,5;48.71,19.2"]:
    q.append(("path", urllib.parse.urlencode({"stops": bad})))
    answers.append(answer("path", q[-1][1]))
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
print(len(q), "queries")
