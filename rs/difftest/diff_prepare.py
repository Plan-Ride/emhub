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


answers = [answer(r, x) for r, x in q]
trips = sorted({d["tripId"] for status, body in answers if status == 200 for d in body.get("departures", [])})
random.shuffle(trips)
for tid in trips[:2500]:
    q.append(("trip", urllib.parse.urlencode({"id": tid})))
    answers.append(answer("trip", q[-1][1]))
for bad in ["x", "a:b:c", "nope:x:20261128", t.trips[0][0] + ":" + t.trips[0][1] + ":20261399"]:
    q.append(("trip", urllib.parse.urlencode({"id": bad})))
    answers.append(answer("trip", q[-1][1]))
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
print(len(q), "queries")
