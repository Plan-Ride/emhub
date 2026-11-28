# Checking emHUB 2 against Python

- `diff_prepare.py <emhub folder> <out folder>` (on the hub, as emhub): loads the timetable as emHUB starts it, writes
  `snapshot.json`, `queries.txt` (route TAB query string) and Python's answers `python.jsonl`. Times are fixed.
- `emhub answer <out>/snapshot.json <out>/queries.txt > <out>/rust.jsonl`: Rust's answers to the same queries.
- `diff_compare.py <out>`: how many answers are the same per route, and the first differences.
- `loadtest.py results.json` (anywhere, `HUB_KEY` = an app key): many requests at once through the public address;
  `HUB_SSH` adds the hub's CPU and memory after each scenario.

Before a route moves to Rust (`HUB_RS_ROUTES`), add its queries to `diff_prepare.py` and get the answers identical.
