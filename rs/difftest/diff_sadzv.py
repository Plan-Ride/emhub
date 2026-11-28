"""On the hub: sadzv's live direct buses (sadzv.journeys), Python against Rust, between stops of city trips running
now (so there are buses to find). sadzv's answers are recorded for Rust (HUB_TEST_SADZV)."""
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
calls, real_call = {}, sadzv._call


def recording(path, body=None):
    answer = real_call(path, body)
    calls[path + json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False)] = answer
    return answer


sadzv._call = recording
random.seed(8)
d = timetable.datetime.fromtimestamp(NOW, timetable.ZONE).date()
base, day = timetable.base_epoch(d), (d - t.first_day).days
running = [t.trips[i] for i in t.city if day in t.services.get(t.trips[i][6], ()) and t.trips[i][9][0] <= NOW - base + 3600 <= t.trips[i][8][-1] + 3600]
random.shuffle(running)
name_of = lambda lat, lon: next((s["name"] for s in t.near(lat, lon, 120, 1)), None)  # noqa: E731
q, answers = [], []
for trip in running[:150]:
    st = trip[7]
    k = random.randrange(0, max(1, len(st) - 3))
    a, b = st[k], st[min(len(st) - 1, k + random.randint(2, 6))]
    if t.lat[a] is None or t.lat[b] is None:
        continue
    when = base + trip[9][k] - random.choice([60, 300, 900])
    q.append(("sadzv", urllib.parse.urlencode({"a": f"{t.lat[a]},{t.lon[a]}", "b": f"{t.lat[b]},{t.lon[b]}", "when": when, "n": 5})))
    answers.append([200, sadzv.journeys((t.lat[a], t.lon[a]), (t.lat[b], t.lon[b]), when, 5, name_of)])
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
json.dump(calls, open(f"{out}/sadzv_calls.json", "w", encoding="utf-8"), ensure_ascii=False)
print(len(q), "asked,", sum(len(a[1]) for a in answers), "live buses found,", len(calls), "sadzv calls")
