"""
On the hub: timetable/plan, Python against Rust. Every sadzv call Python makes is recorded (<out>/sadzv_calls.json) so
Rust answers from the same data (HUB_TEST_SADZV); both answer at one fixed moment (<out>/now.txt, HUB_FAKE_NOW).
"""
import json, random, sys, time, urllib.parse
sys.path.insert(0, sys.argv[1])
import hub, timetable, sadzv  # noqa: E402

out = sys.argv[2]
hub.load_map_edits()
timetable.load(hub.GTFS_DIR, hub.gtfs_feeds())
t = timetable.current
with open(f"{out}/snapshot.json", "w", encoding="utf-8") as f:
    json.dump(timetable.snapshot(t), f, ensure_ascii=False, separators=(",", ":"))

NOW = int(time.time())
open(f"{out}/now.txt", "w").write(str(NOW))
time.time = lambda: float(NOW)
hub.now = lambda: NOW
calls, real_call = {}, sadzv._call


def recording(path, body=None):
    answer = real_call(path, body)
    calls[path + json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False)] = answer
    return answer


sadzv._call = recording
random.seed(9)
towns = sorted(t.towns())
bb = [s for s in range(len(t.names)) if t.lat[s] is not None and 48.70 < t.lat[s] < 48.77 and 19.09 < t.lon[s] < 19.20
      and s not in t.deleted and "city" in t.station_groups()[s]]
everywhere = [s for s in range(len(t.names)) if t.lat[s] is not None and s not in t.deleted and t.weight[s] > 20]
TIMES = [1795845600, 1795875000, 1795905000, 1795930000, 1796000000, "2026-12-01T06:40", "2026-12-05T13:00"]
q = []


def add(**params):
    q.append(("plan", urllib.parse.urlencode({k: v for k, v in params.items() if v is not None})))


def variant():
    return dict(modes=random.choice(["bus,city,rail", "bus,city", "city", "rail", None]), n=random.choice([None, None, "1", "3", "10"]),
                arrive=random.choice([None] * 6 + ["1"]), direct=random.choice([None] * 6 + ["1"]),
                change=random.choice([None, None, "0", "5", "12", "45"]), walkstart=random.choice([None] * 5 + ["0"]),
                walkend=random.choice([None] * 5 + ["0"]), pre=random.choice([None, "1"]))


for _ in range(220):
    a, b = random.sample(towns, 2)
    add(**{"from": "town:" + a.replace(" ", "-"), "to": "town:" + b.replace(" ", "-"), "time": random.choice(TIMES)}, **variant())
for _ in range(220):  # Banska Bystrica now: sadzv's live direct buses join the timetable's
    a, b = random.sample(bb, 2)
    add(**{"from": t.station_ids[a], "to": t.station_ids[b], "time": NOW + random.randint(-600, 3 * 3600)}, **variant())
for _ in range(200):
    a, b = random.sample(everywhere, 2)
    add(**{"from": t.station_ids[a], "to": t.station_ids[b], "time": random.choice(TIMES)}, **variant())
for bad in [{"from": "nope", "to": "town:zvolen"}, {"from": "town:zvolen", "to": "town:zvolen"}, {"from": "town:zvolen", "to": "town:detva", "time": "x"},
            {"from": "town:zvolen", "to": "town:detva", "n": "abc", "time": "1795905000"}, {"from": "Zvolen, AS", "to": "Detva, AS", "time": "1795905000"}]:
    add(**bad)


def answer(query):
    try:
        status, body = hub.timetable_query("plan", dict(urllib.parse.parse_qsl(query)))
    except (ValueError, json.JSONDecodeError) as error:
        status, body = 400, {"error": str(error)}
    return [status, body]


started = time.perf_counter()
answers = [answer(x) for _, x in q]
python_s = time.perf_counter() - started
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
json.dump(calls, open(f"{out}/sadzv_calls.json", "w", encoding="utf-8"), ensure_ascii=False)
ok = [b for s, b in answers if s == 200]
legs = [leg for b in ok for j in b["connections"] for leg in j["legs"]]
print(f"{len(q)} plans in {python_s:.0f} s (Python); {sum(len(b['connections']) for b in ok)} connections, "
      f"{sum(1 for l in legs if l.get('tripId', '').startswith('sadzv:'))} live sadzv legs, {sum(1 for l in legs if l.get('transfer'))} with transfer odds, "
      f"{sum(1 for l in legs if 'units' in l)} with IDS units, {sum(1 for l in legs if l.get('continues'))} going on as another line, {len(calls)} sadzv calls")

