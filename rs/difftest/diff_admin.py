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

# Round 2: the read-only views from the database, as an admin and as a read-only user
def view(user, route, query):
    h = handler(user)
    h.admin_api("GET", route, query)
    raw = h.wfile.getvalue()
    head, body = raw.split(b"\r\n\r\n", 1)
    return [int(head.split(b" ")[1]), json.loads(body)]


with hub.db() as conn:
    labels = [x for r in conn.execute("SELECT labels FROM bugs WHERE labels<>''") for x in r[0].split(",")]
    words = [w for r in conn.execute("SELECT title FROM bugs ORDER BY RANDOM() LIMIT 30") for w in r[0].split()[:2]]
asks = [("notices", {}), ("replies", {}), ("audit", {}), ("labels", {}), ("releases", {})]
for kind in (None, "bug", "feature", "x"):
    for status in (None, "open", "new", "resolved", "awaiting_release", "nope"):
        asks.append(("bugs", {k: v for k, v in (("kind", kind), ("status", status)) if v}))
asks += [("bugs", {"label": random.choice(labels)}) for _ in range(15)] + [("bugs", {"q": random.choice(words)}) for _ in range(20)]
asks += [("bugs", {"label": " P2P ", "kind": "feature"}), ("bugs", {"q": "%"}), ("bugs", {"q": "_"})]
q2, a2 = [], []
admin = next(u for u in made if u["role"] == "admin")
for user in (admin, next(u for u in made if u["role"] == "read-only")):
    for route, query in asks:
        q2.append((f"admin/view/{route}", urllib.parse.urlencode({"user": json.dumps(user), "query": json.dumps(query)})))
        a2.append(view(user, route, query))
with open(f"{out}/queries2.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q2)
with open(f"{out}/python2.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in a2)
print("round 2:", len(q2), "views,", sum(len(a[1]) if isinstance(a[1], list) else 1 for a in a2), "rows/answers")
