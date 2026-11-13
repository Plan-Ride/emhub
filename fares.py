"""What a ride costs: the carriers' own published price lists, kept here and changed in the dashboard (Settings, Fares).

Prices are in cents, [dopravná karta or app, cash]; None where the price list has no such ticket. The apps read the
whole table (GET /api/v1/fares) and each planner leg says which tariff it goes by ("tariff": its id), so a new price
only has to be typed in the dashboard.

Sources (read 2026-11-13):
  MHD Zvolen          https://sadzv.sk/wp-content/uploads/2025/12/cennik_MHD_ZV_26.pdf       valid from 2026-01-01
  MHD Banská Bystrica https://sadzv.sk/wp-content/uploads/2026/07/cennik_MHDBB_260715.pdf    valid from 2026-07-15
  IDS BBSK            https://idsbbsk.sk/prepravny-poriadok/ (Príloha 1)                      valid 2026-11-11 to 2026-11-30
"""
import datetime
import json
import re
import unicodedata

CATEGORIES = ("full", "child", "student", "senior", "ztp", "donor", "companion")

_IDS = [(4, 60, 41, 45), (7, 60, 45, 50), (10, 60, 50, 55), (13, 90, 59, 65), (17, 90, 68, 75), (20, 90, 81, 90),
        (25, 120, 99, 110), (30, 120, 113, 125), (35, 150, 131, 145), (40, 150, 140, 155), (45, 180, 149, 165),
        (50, 180, 158, 175), (55, 210, 167, 185), (60, 210, 176, 195), (70, 210, 198, 220), (80, 240, 216, 240),
        (90, 240, 239, 265), (100, 240, 257, 285)]

DEFAULT = {
    "tariffs": [
        {"id": "mhd-zv", "name": "MHD Zvolen", "kind": "flat", "operators": ["MHD Zvolen"], "validFrom": "2026-01-01", "validTo": "",
         # Paid by card the ticket is good for 45 minutes with any number of changes; cash is one ride.
         "transferMinutes": 45, "transferCents": 0,
         "prices": {"full": [50, 60], "child": [27, 30], "student": [27, 30], "senior": [30, 40], "ztp": [15, 20],
                    "donor": [27, 30], "companion": [15, 20]}},
        {"id": "mhd-bb", "name": "MHD Banská Bystrica", "kind": "flat", "operators": ["MHD Banská Bystrica"],
         "validFrom": "2026-07-15", "validTo": "",
         # The first change within 45 minutes, paid by card: 0,10 € whatever the category.
         "transferMinutes": 45, "transferCents": 10,
         "prices": {"full": [50, 100], "child": [30, 50], "student": [30, 50], "senior": [30, 50], "ztp": [15, 50],
                    "donor": [15, None], "companion": [15, 50]}},
        {"id": "ids-bbsk", "name": "IDS BBSK", "kind": "units", "operators": ["S.A.D. Zvolen", "SAD Lučenec"],
         "validFrom": "2026-11-11", "validTo": "2026-11-30",
         # Which column of the price list a rider's category pays (the rest pay "full").
         "as": {"child": "discount", "student": "discount", "senior": "discount", "donor": "discount",
                "ztp": "special", "companion": "special"},
         "special": [54, 60],
         "bands": [{"upTo": u, "minutes": m, "full": [card, cash], "discount": [card, cash]} for u, m, card, cash in _IDS]},
    ]
}


def _pair(value):
    """[card, cash] in cents, each a whole number from 0 to 100 000 or None."""
    if not isinstance(value, list) or len(value) != 2:
        raise ValueError("A price is [card, cash] in cents")
    for cents in value:
        if cents is not None and (isinstance(cents, bool) or not isinstance(cents, int) or not 0 <= cents <= 100_000):
            raise ValueError("A price is a whole number of cents, or empty")
    return value


def _day(value):
    """A date as YYYY-MM-DD, or "" (no end known)."""
    value = str(value or "").strip()
    if value and not re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        raise ValueError("A date is written like 2026-11-30")
    return value


def due(table, today, days=7):
    """Tariffs whose price list ends within [days] (or has ended): [(name, validTo)], for the dashboard's reminder."""
    limit = (today + datetime.timedelta(days=days)).isoformat()
    return [(t["name"], t["validTo"]) for t in table["tariffs"] if t.get("validTo") and t["validTo"] <= limit]


def _text(value, limit):
    value = " ".join(str(value or "").split())
    if not value or len(value) > limit:
        raise ValueError("A name is missing or too long")
    return value


def clean(data):
    """The table as it is saved, or ValueError saying what is wrong with it."""
    tariffs = data.get("tariffs") if isinstance(data, dict) else None
    if not isinstance(tariffs, list) or not 1 <= len(tariffs) <= 40:
        raise ValueError("Fares need a list of tariffs")
    out, seen = [], set()
    for t in tariffs:
        if not isinstance(t, dict):
            raise ValueError("A tariff is an object")
        tid, kind = _text(t.get("id"), 30), t.get("kind")
        if tid in seen or kind not in ("flat", "units"):
            raise ValueError("Each tariff needs its own id and a kind (flat or units)")
        seen.add(tid)
        operators = [_text(o, 80) for o in t.get("operators") or []][:30]
        row = {"id": tid, "name": _text(t.get("name"), 60), "kind": kind, "operators": operators,
               "validFrom": _day(t.get("validFrom")), "validTo": _day(t.get("validTo"))}
        if kind == "flat":
            row["transferMinutes"] = max(0, min(600, int(t.get("transferMinutes") or 0)))
            row["transferCents"] = max(0, min(100_000, int(t.get("transferCents") or 0)))
            prices = t.get("prices") or {}
            if not isinstance(prices, dict) or "full" not in prices or set(prices) - set(CATEGORIES):
                raise ValueError("A flat tariff needs prices by category, with at least full")
            row["prices"] = {c: _pair(prices[c]) for c in CATEGORIES if c in prices}
        else:
            how = t.get("as") or {}
            if not isinstance(how, dict) or set(how) - set(CATEGORIES) or set(how.values()) - {"full", "discount", "special"}:
                raise ValueError("Categories of a units tariff pay full, discount or special")
            bands = t.get("bands")
            if not isinstance(bands, list) or not 1 <= len(bands) <= 60:
                raise ValueError("A units tariff needs its bands")
            row.update({"as": how, "special": _pair(t.get("special")), "bands": [
                {"upTo": int(b["upTo"]), "minutes": int(b.get("minutes") or 0), "full": _pair(b.get("full")),
                 "discount": _pair(b.get("discount"))} for b in bands]})
            if [b["upTo"] for b in row["bands"]] != sorted({b["upTo"] for b in row["bands"]}) or row["bands"][0]["upTo"] < 1:
                raise ValueError("Bands go up by tariff units, each once")
        out.append(row)
    return {"tariffs": out}


def load(saved):
    """The saved table (a JSON text from the settings) or, with none or a broken one, the price lists above."""
    try:
        return clean(json.loads(saved)) if saved else DEFAULT
    except (ValueError, KeyError, TypeError):
        return DEFAULT


# ------------------------------------------------------------------ IDS BBSK zones
# ids_bbsk_zones.json, made from the organiser's own lists (idsbbsk.sk/prepravny-poriadok, Príloha 6 and 7):
#   towns  folded town name -> zone        stops  "town|rest of the name" -> zone, for towns that lie in several zones
#   units  "a-b" (a <= b) -> tariff units between two zones (1 inside one zone)
_towns = {}


def _fold(text):
    plain = unicodedata.normalize("NFD", text).encode("ascii", "ignore").decode().lower()
    return re.findall(r"[a-z0-9]+", plain)


def zone_of(data, name):
    """The tariff zone of a stop by its name ("Zvolen, Lipovec" -> 650), or None when the lists don't have its town."""
    town, _, rest = name.partition(",")
    words = _fold(town)
    key = " ".join(words)
    zone = data["stops"].get(key + "|" + " ".join(_fold(rest))) if rest else None
    if zone is None:
        zone = data["towns"].get(key)
    if zone is None and words:
        if key not in _towns:
            # The lists shorten names ("Č. Balog", "Kokava n.Rimavicou"): the same words, each cut to its start.
            found = {z for t, z in data["towns"].items() if len(t.split()) == len(words)
                     and all(w.startswith(short) for w, short in zip(words, t.split()))}
            _towns[key] = found.pop() if len(found) == 1 else None
        zone = _towns[key]
    return zone


def units(data, a, b):
    """Tariff units between two stops by their names, or None when either has no zone."""
    za, zb = zone_of(data, a), zone_of(data, b)
    if za is None or zb is None:
        return None
    return data["units"].get(f"{min(za, zb)}-{max(za, zb)}")


def tariff_of(table, operator):
    """The id of the tariff a line of this operator goes by, or None (no price known)."""
    name = (operator or "").casefold()
    for t in table["tariffs"]:
        if name and any(o.casefold() in name for o in t["operators"]):
            return t["id"]
    return None


def price(table, tariff_id, category="full", cash=False, units=None):
    """Cents for one ride, or None where the table can't say (no such ticket, or a units tariff without units)."""
    t = next((x for x in table["tariffs"] if x["id"] == tariff_id), None)
    if t is None:
        return None
    if t["kind"] == "flat":
        return (t["prices"].get(category) or t["prices"]["full"])[int(cash)]
    column = t["as"].get(category, "full")
    if column == "special":
        return t["special"][int(cash)]
    band = next((b for b in t["bands"] if units is not None and units <= b["upTo"]), None)
    return band[column][int(cash)] if band else None


if __name__ == "__main__":
    table = clean(json.loads(json.dumps(DEFAULT)))
    assert table == DEFAULT == load("") == load("{broken")
    assert price(table, "mhd-zv", "student") == 27 and price(table, "mhd-zv", "student", cash=True) == 30
    assert price(table, "mhd-bb", "student") == 30 and price(table, "mhd-bb") == 50 and price(table, "mhd-bb", cash=True) == 100
    assert price(table, "mhd-bb", "donor", cash=True) is None
    assert price(table, "ids-bbsk", units=4) == 41 and price(table, "ids-bbsk", "student", units=5) == 45
    assert price(table, "ids-bbsk", "ztp", units=90) == 54 and price(table, "ids-bbsk") is None
    assert price(table, "ids-bbsk", units=100, cash=True) == 285 and price(table, "ids-bbsk", units=101) is None
    assert tariff_of(table, "MHD Zvolen") == "mhd-zv" and tariff_of(table, "S.A.D. Zvolen a.s.") == "ids-bbsk"
    assert tariff_of(table, "SAD Žilina a.s.") is None and tariff_of(table, "") is None
    for bad in ({}, {"tariffs": []}, {"tariffs": [dict(DEFAULT["tariffs"][0], prices={"full": [1.5, 2]})]},
                {"tariffs": [DEFAULT["tariffs"][0]] * 2}):
        try:
            clean(bad)
            raise AssertionError(bad)
        except ValueError:
            pass
    assert due(table, datetime.date(2026, 11, 13)) == [] and due(table, datetime.date(2026, 11, 24)) == [("IDS BBSK", "2026-11-30")]
    assert due(table, datetime.date(2027, 1, 1)) == [("IDS BBSK", "2026-11-30")]
    import os
    zones = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ids_bbsk_zones.json"), encoding="utf-8"))
    assert zone_of(zones, "Zvolen, Lipovec") == 650 and zone_of(zones, "Banská Bystrica, AS") == 600
    assert zone_of(zones, "Čierny Balog, Krám, rázc.") == 401 and zone_of(zones, "Brno, ÚAN Zvonařka") is None
    assert units(zones, "Zvolen, AS", "Banská Bystrica, AS") == units(zones, "Banská Bystrica, AS", "Zvolen, AS") == 25
    assert units(zones, "Zvolen, AS", "Zvolen, Lipovec") == 1 and units(zones, "Zvolen, AS", "Brno") is None
    assert price(table, "ids-bbsk", units=25) == 99
    print("fares ok")
