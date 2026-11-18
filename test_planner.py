"""
The compiled planner (libplanner.so) against Python's own scans, on a real timetable: the same journeys for random
searches, and how much faster. Run on the hub: python3 test_planner.py [searches] (needs HUB_DB's gtfs folder).
"""
import os
import random
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path

os.environ.setdefault("HUB_TIMETABLE", "off")
import hub  # noqa: E402
import timetable  # noqa: E402

folder = hub.GTFS_DIR
if timetable.NATIVE is None or not list(folder.glob("*.zip")):
    print("skipped: no libplanner.so or no timetable in", folder)
    sys.exit(0)
hub.load_map_edits()
timetable.load(folder, [p.stem for p in folder.glob("*.zip")])
t, native = timetable.current, timetable.NATIVE
rng = random.Random(7)
busy = [s for s in range(len(t.names)) if t.lat[s] is not None]
midnight = int(datetime.now(timetable.ZONE).replace(hour=0, minute=0, second=0, microsecond=0).timestamp())
spent = {"python": 0.0, "rust": 0.0}
total = int(sys.argv[1]) if len(sys.argv) > 1 else 150
for n in range(total):
    a, b = rng.sample(busy, 2)
    kwargs = dict(when=midnight + rng.randrange(0, 2 * 86400), direct=rng.random() < 0.15, change=rng.choice([60, 60, 120, 300]),
                  walk_start=rng.random() < 0.85)
    got = {}
    for name, lib in (("rust", native), ("python", None)):
        timetable.NATIVE = lib
        started = time.time()
        got[name] = t.plan(a, b, **kwargs)
        spent[name] += time.time() - started
    assert got["rust"] == got["python"], (t.names[a], t.names[b], kwargs)
timetable.NATIVE = native
print(f"ok: {total} searches the same; python {spent['python']:.1f} s, rust {spent['rust']:.1f} s ({spent['python'] / max(spent['rust'], 1e-9):.0f}x)")

# 1.58.2: a direct bus that leaves earlier and arrives as early as a later one is shown (line 12, Zvolen, Nám.SNP 15:50
# -> Zlatý Potok konečná 16:05 was hidden by the 1 at 15:53 -> 16:05). Weekdays, while S.A.D. runs it so.
snp, potok = t.by_id.get("zvolen nam snp"), t.by_id.get("zvolen zlaty potok konecna")
if snp is not None and potok is not None:
    day = datetime.fromtimestamp(midnight, timetable.ZONE)
    while day.weekday() > 4:
        day += timedelta(days=1)
    at = int(day.replace(hour=15, minute=30).timestamp())
    got = [(leg["line"], datetime.fromtimestamp(leg["departure"] // 1000, timetable.ZONE).strftime("%H:%M"))
           for j in t.plan_many([snp], [potok], at, 6) for leg in j["legs"] if leg["type"] == "trip"]
    assert ("12", "15:50") in got, got
    print("ok: line 12 at 15:50 shown", got)
