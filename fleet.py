"""
Live buses of the transport companies on qrbus.me, for the dashboard (the same public call their map pages make, no login).

Each company has its own address; the area in the request never reaches another company's buses, so all of it is asked
for. A record can be weeks old (SAD Zvolen's buses 126-148 stopped on 24 Sep 2026 with their passenger counts frozen):
only ones updated in the last 3 minutes count. Driver fields are dropped here and never kept or shown.
"""
import json
import re
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

# id, name, address, map centre. Kosice's m.dpmk.qrbus.me has all the fields but none of them fresh.
CITIES = [("bb", "Banská Bystrica and Zvolen", "m.mhdbb.qrbus.me", (48.736, 19.146)),
          ("hk", "Hradec Králové", "m.dpmhk.qrbus.me", (50.21, 15.83)),
          ("ke", "Košice", "qrbus.me", (48.72, 21.26))]
PERSONAL = re.compile("driver|dut", re.I)  # DriverNumber, DriverChanged, DutyNumber, ObuDutesTime...
PASSENGERS = ("NumberOfPassengers", "LastNumberOfPassengers", "AllEntryPassengers", "StopEntryPassangers", "StopExitPassengers")
MAP_FIELDS = ("city", "Vin", "Latitude", "Longitude", "LineNumberText", "TripNumber", "Delay", "Speed", "NumberOfPassengers", "Licence",
              "Destination", "source")
MAX_AGE_S = 180
CACHE_S = 15
_lock = threading.Lock()
_cache = (0, None)


def _fetch(host):
    body = json.dumps({"lon1": -18000000, "lat1": -9000000, "lon2": 18000000, "lat2": 9000000}).encode()
    request = urllib.request.Request(f"https://{host}/api/rest/rtdwebglobal/getVehicleStatus", body, {
        "Content-Type": "application/json", "Origin": f"https://{host}", "Referer": f"https://{host}/transport-detail",
        "User-Agent": "emhub (Plan&Ride)"})
    with urllib.request.urlopen(request, timeout=15) as response:
        root = json.loads(response.read(8_000_000))
    return root.get("vehicles") or root.get("vehicleStatus") or []


def _city(city):
    cid, name, host, center = city
    out = {"id": cid, "name": name, "center": center, "total": 0, "live": 0, "onTrip": 0, "counted": 0}
    try:
        rows = [x for x in _fetch(host) if isinstance(x, dict)]
    except Exception as e:  # noqa: BLE001 - one company down leaves the others
        return dict(out, error=str(e)[:200]), []
    now = time.time()
    live = []
    for x in rows:
        at, lat, lon = x.get("DeliveryDateTime"), x.get("Latitude"), x.get("Longitude")
        if isinstance(at, int) and now - at <= MAX_AGE_S and isinstance(lat, int) and isinstance(lon, int) and not x.get("GpsInvalid"):
            live.append({"city": cid, **{k: v for k, v in x.items() if not PERSONAL.search(k)}})
    return dict(out, total=len(rows), live=len(live), onTrip=sum(1 for x in live if x.get("LineId")),
                counted=sum(1 for x in live if any(x.get(k) for k in PASSENGERS))), live


def snapshot(full=True):
    """{"at", "cities": [...], "vehicles": [...]}: the buses reporting now, asked for at most every 15 s."""
    global _cache
    with _lock:  # one fetch at a time: map polls wait for it instead of each asking all three
        if time.time() - _cache[0] > CACHE_S or _cache[1] is None:
            with ThreadPoolExecutor(max_workers=len(CITIES)) as pool:
                got = list(pool.map(_city, CITIES))
            _cache = (time.time(), {"at": int(time.time()), "cities": [c for c, _ in got], "vehicles": [v for _, r in got for v in r]})
        snap = _cache[1]
    return snap if full else dict(snap, vehicles=[{k: v.get(k) for k in MAP_FIELDS} for v in snap["vehicles"]])
