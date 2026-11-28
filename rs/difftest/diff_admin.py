"""
On the hub: the dashboard's "me" and its access check (allowed/route_tabs), Python against Rust, for the users in the
database and made-up ones with other roles. No one signs in: a request handler is made in memory with its user set.
<out> holds user rows (with password hashes): the caller deletes it afterwards.
"""
import io, json, random, sys, urllib.parse
from email.message import Message
sys.path.insert(0, sys.argv[1])
import hub  # noqa: E402

out = sys.argv[2]
with hub.db() as conn:
    users = [dict(r) for r in conn.execute("SELECT * FROM users")]
    bug_ids = [r[0] for r in conn.execute("SELECT id FROM bugs ORDER BY RANDOM() LIMIT 20")]
base = dict(users[0])
made = []
for role, perms in [("read-only", {}), ("custom", {"bugs": "r"}), ("custom", {"bugs": "rw", "map": "r"}), ("custom", {"mail": "r", "site": "rw"}),
                    ("custom", {"timetable": "w", "lines": "rw", "fleet": "r"}), ("custom", {}), ("admin", {}), ("nobody", {})]:
    made.append(dict(base, role=role, perms=json.dumps(perms), oidc_sub=None, oidc_only=0, username=f"test-{role}"))
everyone = users + made


def handler(user, body=None):
    h = object.__new__(hub.Handler)
    h.headers = Message()
    h.headers["Content-Type"] = "application/json"
    h.wfile, h._raw, h.command, h.request_version = io.BytesIO(), json.dumps(body or {}).encode(), "GET", "HTTP/1.1"
    h.requestline, h.client_address, h.close_connection = "GET / HTTP/1.1", ("127.0.0.1", 0), True
    h.current_user = lambda: user
    h.json_body = lambda limit=None: body or {}
    h.log_message = lambda *a: None
    return h


def me(user):
    h = handler(user)
    h.admin_api("GET", "me", {})
    raw = h.wfile.getvalue()
    return json.loads(raw.split(b"\r\n\r\n", 1)[1])


routes = ["me", "stats", "events", "bugs", "bugs/x", "labels", "replies", "releases", "releases/3", "timetable/stops", "timetable/status",
          "timetable/update", "timetable/feeds", "timetable/searches", "timetable", "feeds/health", "fleet", "mailbox", "mailbox/1",
          "map/stations", "map/snap", "stops/new", "lines", "lines/4", "links", "links/candidates", "submissions", "submissions/2",
          "notices", "community/stats", "community/trips", "site", "site/home", "server", "server/backup", "demo", "demo/apk", "users",
          "users/3", "settings", "audit", "templates", "account/password", "account", "nope", "", "/bugs/", "fares", "preview"]
routes += [f"bugs/{i}" for i in bug_ids] + [f"bugs/{i}/reply" for i in bug_ids[:5]]
queries = [{}, {"kind": "feature"}, {"map": "1"}, {"kind": "bug"}]
q, answers = [], []
for user in everyone:
    q.append(("admin/me", urllib.parse.urlencode({"user": json.dumps(user)})))
    answers.append([200, me(user)])
random.seed(6)
for user in everyone:
    for route in routes:
        for method in ("GET", "POST"):
            query = random.choice(queries)
            body = {"bugId": random.choice(bug_ids + [0])} if route == "preview" else None
            q.append(("admin/allowed", urllib.parse.urlencode({"user": json.dumps(user), "method": method, "route": route,
                                                               "query": json.dumps(query), "body": json.dumps(body)})))
            answers.append([200, handler(user, body).allowed(user, method, route, query)])
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in answers)
print(len(users), "users +", len(made), "made up;", len(q), "asked;", sum(1 for a in answers[len(everyone):] if a[1]), "allowed")
