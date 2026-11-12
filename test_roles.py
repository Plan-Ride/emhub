"""Roles and tab access, attacked from outside over HTTP. Run: python3 test_roles.py (throwaway database)."""
import base64
import http.client
import json
import os
import re
import socket
import sqlite3
import struct
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
tmp = tempfile.mkdtemp()
os.environ.update(HUB_DB=os.path.join(tmp, "t.db"), HUB_BOOTSTRAP_PASSWORD="bootstrap-pass-1", HUB_SMTP_HOST="127.0.0.1",
                  HUB_SMTP_PORT="1", HUB_BACKUP_DIR=os.path.join(tmp, "backups"))
sys.path.insert(0, HERE)
import hub  # noqa: E402

hub.init_db()
server = hub.ThreadingHTTPServer(("127.0.0.1", 0), hub.Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
PORT = server.server_port
hub.mail_worker = lambda: None

with hub.db() as conn:
    admin_id = conn.execute("SELECT id FROM users").fetchone()[0]
    assert conn.execute("SELECT role FROM users").fetchone()[0] == "admin", "the first account is an admin"
    for kind, title in (("bug", "A bug"), ("feature", "An idea")):
        conn.execute("INSERT INTO bugs(created_at, updated_at, consent_at, kind, title, description) VALUES (?,?,?,?,?,?)",
                     (hub.now(), hub.now(), hub.now(), kind, title, "x"))
    bug_id, idea_id = [r[0] for r in conn.execute("SELECT id FROM bugs ORDER BY id")]


def make(name, role="custom", perms=None):
    with hub.db() as conn:
        uid = conn.execute("INSERT INTO users(username, password_hash, email, created_at, role, perms) VALUES (?,?,?,?,?,?)",
                           (name, hub.hash_password("right-password-1"), name + "@x.sk", hub.now(), role, json.dumps(perms or {}))).lastrowid
        return uid, "hub_session=" + hub.create_session(conn, uid)


def req(method, path, cookie, body=None, headers=None):
    c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=10)
    h = {"Cookie": cookie, **(headers or {})}
    data = None
    if method == "POST":
        h.setdefault("Content-Type", "application/json")
        data = json.dumps({} if body is None else body)
    c.request(method, path, data, h)
    r = c.getresponse()
    out = r.read()
    c.close()
    return r.status, out


# Every route in the dashboard API, with the tab that opens it: "" = any signed-in user, None = admins only.
R = [
    ("GET", "me", ""), ("GET", "stats", ""),
    ("GET", f"bugs?kind=bug", "bugs"), ("GET", "bugs?kind=feature", "ideas"), ("GET", f"bugs/{bug_id}", "bugs"), ("GET", f"bugs/{idea_id}", "ideas"),
    ("GET", f"bugs/{bug_id}/images/1", "bugs"), ("POST", f"bugs/{bug_id}/note", "bugs"), ("POST", f"bugs/{idea_id}/note", "ideas"),
    ("POST", f"bugs/{bug_id}/status", "bugs"), ("POST", f"bugs/{bug_id}/meta", "bugs"), ("POST", f"bugs/{bug_id}/reply", "bugs"),
    ("GET", "submissions", "submissions"), ("POST", "submissions/1/approve", "submissions"),
    ("GET", "lines", "lines"), ("POST", "lines", "lines"), ("POST", "lines/1", "lines"), ("POST", "lines/1/delete", "lines"),
    ("GET", "links", "lines"), ("GET", "links/candidates?from=1&to=2", "lines"), ("POST", "links", "lines"), ("POST", "links/1/delete", "lines"),
    ("GET", "releases/2.0/draft", "releases"), ("POST", "releases/2.0/notes", "releases"), ("POST", "releases/2.0/release", "releases"),
    ("GET", "notices", "notices"), ("POST", "notices", "notices"), ("POST", "notices/1", "notices"), ("POST", "notices/1/delete", "notices"),
    ("GET", "community/trips", "community"), ("GET", "community/reports", "community"), ("GET", "community/stats", "community"),
    ("POST", "community/delete", "community"), ("POST", "community/block", "community"),
    ("GET", "timetable/status", "timetable"), ("GET", "timetable/searches", "timetable"), ("GET", "feeds/health", "timetable"), ("POST", "timetable/update", "timetable"), ("POST", "timetable/feeds", "timetable"),
    ("GET", "map/stations?bbox=1,2,3,4", "map"), ("GET", "map/lines", "map"), ("GET", "map/todo", "map"), ("GET", "map/history", "map"), ("GET", "map/bulk", "map"),
    ("GET", "map/vehicles?bbox=1,2,3,4", "map"), ("GET", "map/station?id=1", "map"), ("GET", "map/trip?id=1", "map"),
    ("POST", "map/stop", "map"), ("POST", "map/stop/reset", "map"), ("POST", "map/station", "map"), ("POST", "map/child", "map"), ("POST", "map/child/remove", "map"),
    ("POST", "map/segment", "map"), ("POST", "map/segment/reset", "map"), ("POST", "map/via", "map"), ("POST", "map/route", "map"), ("POST", "map/way", "map"), ("POST", "map/snap", "map"),
    ("POST", "map/bulk", "map"), ("POST", "map/history/1/undo", "map"), ("POST", "stops/new", "map"),
    ("GET", "fleet", "fleet"), ("GET", "site", "site"), ("GET", "site/home/sk", "site"), ("POST", "site/home/sk/preview", "site"), ("POST", "site/home/sk/save", "site"), ("POST", "site/home/sk/reset", "site"),
    ("GET", "server", "server"), ("POST", "server/backup", "server"),
    ("GET", "mailbox/folders", "mail"), ("GET", "mailbox/list?folder=INBOX", "mail"), ("GET", "mailbox/message?uid=1", "mail"),
    ("GET", "mailbox/attachment?uid=1&n=0", "mail"), ("POST", "mailbox/send", "mail"), ("POST", "mailbox/move", "mail"), ("POST", "mailbox/draft", "mail"),
    ("GET", "audit", None), ("GET", "admins", None), ("POST", "admins", None), (f"POST", f"admins/{admin_id}/remove", None), ("POST", f"admins/{admin_id}/resend", None), ("POST", f"admins/{admin_id}/access", None),
    ("GET", "settings", None), ("POST", "settings", None), ("POST", "settings/test-oidc", None), ("GET", "templates", None), ("GET", "templates/invite", None),
    ("POST", "templates/invite/save", None), ("POST", "templates/invite/reset", None), ("POST", "templates/invite/preview", None), ("POST", "mail/test", None),
    ("POST", "replies", None), ("POST", "replies/1", None), ("POST", "replies/1/delete", None),
]
SHARED = [("GET", "labels", ("bugs", "ideas")), ("GET", "replies", ("bugs", "ideas")), ("GET", "releases", ("releases", "bugs", "ideas")),
          ("GET", "timetable/stops?q=a", ("timetable", "lines", "map")), ("GET", "fleet?map=1", ("fleet", "map"))]
ALL_PATHS = [p.split("?")[0] for _, p, _ in R] + [p.split("?")[0] for _, p, _ in SHARED]

# A route added to hub.py without being listed here fails this test.
source = open(os.path.join(HERE, "hub.py"), encoding="utf-8").read()
admin = source[source.index("    def admin_api("):source.index("    def map_api(")]
mapped = source[source.index("    def map_api("):source.index("def similar_bugs(")]
named = set(re.findall(r'route == "([^"]+)"', admin)) | {"map/" + x for x in re.findall(r'route == "([^"]+)"', mapped)}
for group in re.findall(r"route in \(([^)]+)\)", mapped):
    named |= {"map/" + x for x in re.findall(r'"([^"]+)"', group)}
missing = {x for x in named - {"preview", "events", "account/email", "account/password", "account/unlink", "account/oidc-only", "map/live"} if x not in ALL_PATHS}
assert not missing, f"not covered: {missing}"
print("ok every route listed", len(R) + len(SHARED))

none_id, none = make("nobody")
ro_id, ro = make("reader", "read-only")
bugs_id, bugs_r = make("bugsreader", "custom", {"bugs": "r"})
_, ideas_rw = make("ideasclerk", "custom", {"ideas": "rw"})
_, notices_w = make("noticewriter", "custom", {"notices": "w"})
_, map_only = make("mapper", "custom", {"map": "r"})
_, all_rw = make("allrw", "custom", {t: ("r" if t == "site" else "rw") for t in hub.TABS})
_, broken = make("oddrole", "something-else", {t: "rw" for t in hub.TABS})
admin_cookie = "hub_session=" + (lambda c: hub.create_session(c, admin_id))(sqlite3.connect(os.environ["HUB_DB"], isolation_level=None))


def fingerprint():
    with hub.db() as conn:
        return {t: [tuple(r) for r in conn.execute(f"SELECT * FROM {t}")] for t in
                ("users", "settings", "invites", "notices", "site_pages", "email_templates", "saved_replies", "admin_lines", "community_reports", "bugs")}


def expect(cookie, access, who):
    """access: tab -> "r"/"w"/"rw"."""
    for method, path, tab in R:
        need = "r" if method == "GET" else "w"
        ok = tab == "" or (tab is not None and need in access.get(tab, ""))
        status, out = req(method, "/admin/api/" + path, cookie)
        assert (status != 403) == ok, (who, method, path, status, out[:120])
    for method, path, tabs in SHARED:
        ok = any("r" in access.get(t, "") for t in tabs)
        status, out = req(method, "/admin/api/" + path, cookie)
        assert (status != 403) == ok, (who, method, path, status, out[:120])


before = fingerprint()
expect(none, {}, "no tabs")
expect(ro, {t: "r" for t in hub.TABS if t != "mail"}, "read-only")  # mail is private: only a ticked box gives it
expect(broken, {}, "unknown role")  # never falls open
after = fingerprint()
assert after == before, [t for t in after if after[t] != before[t]]
print("ok no tabs, read-only, unknown role: all refused, nothing changed")

expect(bugs_r, {"bugs": "r"}, "bugs read")
expect(map_only, {"map": "r"}, "map read")
assert fingerprint() == before
print("ok one tab read")
expect(notices_w, {"notices": "w"}, "notices write only")
assert req("GET", "/admin/api/notices", notices_w)[0] == 403
print("ok write without read")
expect(ideas_rw, {"ideas": "rw"}, "ideas")
print("ok one tab read and write")
expect(all_rw, {x: ("r" if x == "site" else "rw") for x in hub.TABS}, "every tab read and write")
assert req("POST", "/admin/api/site/home/sk/save", all_rw, {"html": "x"})[0] == 403
print("ok every tab, still no admin powers")

# Getting around it: same routes spelled another way, the other kind of report, escalating.
for cookie in (none, ro, bugs_r):
    for path in (f"admins/{admin_id}/remove/", f"/admins/{admin_id}/remove", f"admins/{admin_id}/access/", "settings/", "/settings",
                 "ADMINS", f"admins/{admin_id}//remove", "templates/invite/save/", "site/home/sk/save/", "replies/1/delete/"):
        assert req("POST", "/admin/api/" + path, cookie, {"role": "admin"})[0] == 403, path
    assert req("HEAD", "/admin/api/admins", cookie)[0] == 403
    assert req("HEAD", "/admin/api/audit", cookie)[0] == 403
    assert req("POST", "/admin/api/admins", cookie, {"username": "evil", "email": "e@x.sk", "role": "admin"})[0] == 403
    assert req("POST", f"/admin/api/admins/{ro_id}/access", cookie, {"role": "admin"})[0] == 403
    assert req("POST", "/admin/api/settings", cookie, {"oidc_issuer": "https://evil.example"})[0] == 403
for spoof in ("bugs?kind=feature", "bugs?kind=bug&kind=feature", f"bugs/{idea_id}", f"bugs/{idea_id}/", "fleet", "server", "fleet?full=1"):
    assert req("GET", "/admin/api/" + spoof, bugs_r)[0] == 403, spoof
assert req("GET", "/admin/api/bugs?kind=nonsense", ideas_rw)[0] == 403  # the handler treats it as bugs
assert req("POST", "/admin/api/preview", bugs_r, {"bugId": bug_id})[0] == 403
assert req("POST", "/admin/api/preview", ideas_rw, {"bugId": bug_id})[0] == 403
assert req("POST", "/admin/api/preview", ideas_rw, {"bugId": idea_id})[0] == 200
assert b"oidc" not in req("GET", "/admin/api/me", bugs_r)[1].replace(b"oidcLinked", b"").replace(b"oidcName", b"").replace(b"oidcOnly", b"").replace(b"oidcEnabled", b"").replace(b"oidcLabel", b"")
for key, tab in ((b'"ideas"', "ideas"), (b'"submissions"', "submissions"), (b'"activeTrips"', "community")):
    assert key not in req("GET", "/admin/api/stats", bugs_r)[1], tab  # the counters don't tell what can't be read
assert b'"bugs"' in req("GET", "/admin/api/stats", bugs_r)[1]
assert req("POST", "/admin/api/notices", ideas_rw, {}, {"Content-Type": "text/plain"})[0] == 415
assert req("POST", "/admin/api/account/email", ro, {}, {"Origin": "https://evil.example"})[0] == 403
assert req("POST", "/admin/api/account/email", ro, {"email": "mine@x.sk"})[0] == 200  # their own account stays theirs
with hub.db() as conn:
    assert conn.execute("SELECT 1 FROM audit_log WHERE action='refused: admins'").fetchone(), "refused changes are logged"
print("ok ways around it")

# Roles can't be set to something odd, and the website can't be delegated.
for bad in ({"role": "root"}, {"role": ["admin"]}, {"role": "custom", "perms": {"settings": "rw"}}, {"role": "custom", "perms": {"bugs": "x"}},
            {"role": "custom", "perms": {"site": "rw"}}, {"role": "custom", "perms": {"site": "w"}}, {"role": "custom", "perms": ["bugs"]}):
    assert req("POST", f"/admin/api/admins/{none_id}/access", admin_cookie, bad)[0] == 400, bad
assert req("POST", "/admin/api/admins", admin_cookie, {"username": "plain", "email": "p@x.sk"})[0] == 400  # a role must be chosen
with hub.db() as conn:
    conn.execute("UPDATE users SET perms=? WHERE id=?", ('{"site": "rw"}', none_id))
assert req("POST", "/admin/api/site/home/sk/save", none, {"html": "<script>"})[0] == 403  # even if the database says so
print("ok bad roles refused")

# Changing access takes effect on the next request; nobody changes their own; admins can't vanish.
assert req("POST", f"/admin/api/admins/{bugs_id}/access", admin_cookie, {"role": "custom", "perms": {"fleet": "r"}})[0] == 200
assert req("GET", "/admin/api/bugs", bugs_r)[0] == 403 and req("GET", "/admin/api/fleet", bugs_r)[0] != 403
assert req("POST", f"/admin/api/admins/{admin_id}/access", admin_cookie, {"role": "read-only"})[0] == 400
assert req("POST", f"/admin/api/admins/{admin_id}/remove", admin_cookie)[0] == 400
second_id, second = make("second", "admin")
assert req("POST", f"/admin/api/admins/{admin_id}/access", second, {"role": "read-only"})[0] == 200
assert req("POST", f"/admin/api/admins/{second_id}/access", admin_cookie, {"role": "read-only"})[0] == 403, "a demoted admin keeps nothing"
assert req("POST", f"/admin/api/admins/{admin_id}/access", second, {"role": "admin"})[0] == 200
threads = [threading.Thread(target=req, args=("POST", f"/admin/api/admins/{a}/access", c, {"role": "read-only"}))
           for a, c in ((second_id, admin_cookie), (admin_id, second))]
[t.start() for t in threads]
[t.join() for t in threads]
with hub.db() as conn:
    assert conn.execute("SELECT COUNT(*) FROM users WHERE role='admin'").fetchone()[0] >= 1, "two admins demoting each other"
    conn.execute("UPDATE users SET role='admin' WHERE id IN (?,?)", (admin_id, second_id))
assert req("POST", f"/admin/api/admins/{second_id}/remove", admin_cookie)[0] == 200
assert req("GET", "/admin/api/me", second)[0] == 401
print("ok changes apply at once, admins stay")

# The invited account gets the role it was given, and accepting can't change it.
status, out = req("POST", "/admin/api/admins", admin_cookie, {"username": "newone", "email": "n@x.sk", "role": "custom", "perms": {"bugs": "r"}})
assert status == 201, out
with hub.db() as conn:
    row = conn.execute("SELECT role, perms FROM users WHERE username='newone'").fetchone()
assert (row["role"], json.loads(row["perms"])) == ("custom", {"bugs": "r"})


# ---- the map's live channel
def socket_open(cookie):
    s = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    origin = next(iter(hub.ORIGINS))
    s.sendall((f"GET /admin/api/map/live?tab=t{key[:6]} HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\nOrigin: {origin}\r\n"
               f"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\r\n").encode())
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = s.recv(4096)
        if not chunk:
            break
        head += chunk
    return s, head


def focus(s, station):
    data, mask = json.dumps({"type": "focus", "station": station}).encode(), b"abcd"
    s.sendall(bytes([0x81, 0x80 | len(data)]) + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(data)))


assert b" 403 " in socket_open(ideas_rw)[1].split(b"\r\n")[0], "no map access, no socket"
viewer, head = socket_open(map_only)
assert b" 101 " in head
focus(viewer, "S1")
time.sleep(.3)
assert not any(p_["station"] for p_ in hub.map_room.editors()), "someone only looking is not shown as on a stop"
_, editor_cookie = make("editor", "custom", {"map": "rw"})
with hub.db() as conn:
    editor_id = conn.execute("SELECT id FROM users WHERE username='editor'").fetchone()[0]
editor, head = socket_open(editor_cookie)
focus(editor, "S2")
time.sleep(.3)
assert [p_["name"] for p_ in hub.map_room.editors() if p_["station"] == "S2"] == ["editor"]
assert req("POST", f"/admin/api/admins/{editor_id}/access", admin_cookie, {"role": "custom", "perms": {"map": "r"}})[0] == 200
editor.settimeout(3)
got = b""
try:
    while True:
        chunk = editor.recv(4096)
        if not chunk:
            break
        got += chunk
except socket.timeout:
    raise AssertionError("the socket stayed open after access changed")
time.sleep(.2)
assert not any(p_["station"] == "S2" for p_ in hub.map_room.editors())
print("ok map socket")


# ---- Authentik groups
TEXT = """
emhub-admins = admin
emhub-readers = read-only
emhub support = bugs:rw ideas:r map:r
"""
rules = hub.role_rules(TEXT)
assert [r[0] for r in rules] == ["emhub-admins", "emhub-readers", "emhub support"]
assert hub.group_access(rules, ["emhub-admins", "emhub-readers"]) == ("admin", "{}")
assert hub.group_access(rules, ["emhub-readers"]) == ("read-only", "{}")
assert hub.group_access(rules, ["other"]) == ("custom", "{}"), "no matching group, no access"
role, perms = hub.group_access(rules, ["emhub support", "emhub-readers"])
assert role == "custom" and json.loads(perms)["bugs"] == "rw" and json.loads(perms)["fleet"] == "r" and json.loads(perms)["ideas"] == "r"
for bad in ("no equals", "= admin", "a = ", "a = root", "a = settings:rw", "a = site:rw", "a = bugs:x"):
    try:
        hub.role_rules(bad)
    except ValueError:
        continue
    raise AssertionError(bad)
assert req("POST", "/admin/api/settings", admin_cookie, {"oidc_role_map": "a = site:rw"})[0] == 400
assert req("POST", "/admin/api/settings", admin_cookie, {"oidc_role_map": TEXT})[0] == 200
assert hub.setting("oidc_role_map").strip() == TEXT.strip()

gid, gcookie = make("groupie", "admin")
with hub.db() as conn:
    conn.execute("UPDATE users SET oidc_sub='g1', oidc_name='groupie' WHERE id=?", (gid,))
    assert hub.sync_roles(conn, gid, {"sub": "g1"}) is False, "no groups claim: sign-in refused"
    assert hub.sync_roles(conn, gid, {"sub": "g1", "groups": ["emhub-readers"]})
    assert conn.execute("SELECT role FROM users WHERE id=?", (gid,)).fetchone()[0] == "read-only"
assert req("POST", "/admin/api/notices", gcookie)[0] == 403, "a demotion from Authentik reaches open sessions"
with hub.db() as conn:
    assert hub.sync_roles(conn, gid, {"sub": "g1", "groups": []})
    assert tuple(conn.execute("SELECT role, perms FROM users WHERE id=?", (gid,)).fetchone()) == ("custom", "{}")
assert req("GET", "/admin/api/bugs", gcookie)[0] == 403
assert req("POST", "/admin/api/account/unlink", gcookie)[0] == 400, "unlinking would escape the groups"
status = http.client.HTTPConnection("127.0.0.1", PORT)
status.request("POST", "/auth/login", "username=groupie&password=right-password-1", {"Content-Type": "application/x-www-form-urlencoded"})
res = status.getresponse()
assert res.status == 303 and "oidc_only" in res.getheader("Location"), res.getheader("Location")
with hub.db() as conn:  # the only admin left keeps it, whatever the groups say
    others = [r[0] for r in conn.execute("SELECT id FROM users WHERE role='admin' AND id<>?", (admin_id,))]
    conn.execute(f"UPDATE users SET role='custom' WHERE id IN ({','.join('?' * len(others))})", others)
    conn.execute("UPDATE users SET oidc_sub='a1' WHERE id=?", (admin_id,))
    hub.sync_roles(conn, admin_id, {"sub": "a1", "groups": []})
    assert conn.execute("SELECT role FROM users WHERE id=?", (admin_id,)).fetchone()[0] == "admin"
print("ok authentik groups")


# ---- an existing database: everyone stays what they were
old = os.path.join(tmp, "old.db")
conn = sqlite3.connect(old)
conn.executescript("""CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT UNIQUE NOT NULL, password_hash TEXT NOT NULL,
    email TEXT NOT NULL DEFAULT '', oidc_sub TEXT, oidc_name TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL);
    INSERT INTO users(username, password_hash, created_at) VALUES ('a', 'x', 1), ('b', 'y', 2);""")
conn.close()
real, hub.DB_PATH = hub.DB_PATH, old
hub.init_db()
hub.init_db()
with hub.db() as conn:
    assert [r[0] for r in conn.execute("SELECT role FROM users")] == ["admin", "admin"]
    conn.execute("INSERT INTO users(username, password_hash, created_at) VALUES ('c', 'z', 3)")
    assert conn.execute("SELECT role, perms FROM users WHERE username='c'").fetchone()[0] == "custom", "new rows start with nothing"
hub.DB_PATH = real
print("ok migration")

# ---- the mailbox: what an IMAP server answers, parsed; folder names can't break out of the quotes
import email.message  # noqa: E402


class FakeImap:
    def __init__(self):
        m = email.message.EmailMessage()
        m["From"], m["To"], m["Subject"], m["Date"] = "Jakub <j@x.sk>", "par@karafa.net", "Dáta o vlakoch", "Sun, 08 Nov 2026 19:00:00 +0100"
        m["Message-ID"] = "<a1@x.sk>"
        m.set_content("Ahoj")
        m.add_alternative("<p>Ahoj <b>svet</b></p>", subtype="html")
        m.add_attachment(b"PDFDATA", maintype="application", subtype="pdf", filename="cennik.pdf")
        self.raw, self.selected, self.stored = m.as_bytes(), None, []

    def list(self):
        return "OK", [b'(\\HasNoChildren) "/" "Trash"', b'(\\HasNoChildren) "/" "INBOX"', b'(\\Noselect) "/" "Folders"', b'(\\HasNoChildren) "/" "Folders/Vlaky"']

    def select(self, name, readonly=True):
        self.selected = (name, readonly)
        return "OK", [b"1"]

    def uid(self, cmd, *args):
        if cmd == "SEARCH":
            return "OK", [b"3 7"]
        if cmd == "STORE":
            self.stored.append(args)
            return "OK", []
        if args[1] == "(UID INTERNALDATE)":  # uid 3 arrived after uid 7
            return "OK", [b'1 (UID 7 INTERNALDATE "08-Nov-2026 19:00:00 +0100")', b'2 (UID 3 INTERNALDATE "08-Nov-2026 19:43:00 +0100")']
        if "BODY.PEEK[]" in args[1]:
            return "OK", [(b"1 (UID 7 BODY[] {9}", self.raw), b")"]
        head = b"From: Jakub <j@x.sk>\r\nSubject: =?utf-8?q?D=C3=A1ta?=\r\nDate: Sun, 08 Nov 2026 19:00:00 +0100\r\n\r\n"
        return "OK", [(b"1 (UID 7 FLAGS (\\Seen \\Answered) BODY[HEADER.FIELDS (FROM TO SUBJECT DATE)] {9}", head), b")",
                      (b"2 (UID 3 FLAGS () BODY[HEADER.FIELDS (FROM TO SUBJECT DATE)] {9}", head), b")"]


fake = FakeImap()
assert hub.mail_folders(fake) == ["INBOX", "Trash", "Folders/Vlaky"], hub.mail_folders(fake)
listed = hub.mail_list(fake, "INBOX")
assert [(m["uid"], m["seen"], m["answered"]) for m in listed["messages"]] == [(3, False, False), (7, True, True)], listed
assert listed["messages"][0]["subject"] == "Dáta" and listed["total"] == 2
view = hub.mail_view(hub.mail_fetch(fake, "INBOX", 7, mark_seen=False))
assert "<b>svet</b>" in view["html"] and view["text"] == "Ahoj svet" and view["attachments"] == [{"n": 0, "name": "cennik.pdf", "size": 7}], view
assert fake.selected == ('"INBOX"', True) and not fake.stored, "a reader only looks"
hub.mail_fetch(fake, "Folders/Vlaky", 7, mark_seen=True)
assert fake.selected == ('"Folders/Vlaky"', False) and fake.stored == [("7", "+FLAGS", "(\\Seen)")]
for bad in ('INBOX" DELETE "x', "a\\b", "a\r\nb", "x" * 201):
    try:
        hub.imap_folder(bad)
        raise AssertionError(bad)
    except ValueError:
        pass
for bad in ("a@x.sk, nope", ", ".join(f"p{i}@x.sk" for i in range(21))):
    try:
        hub.mail_addresses(bad)
        raise AssertionError(bad)
    except ValueError:
        pass
assert hub.mail_addresses("Jakub <j@x.sk>, b@y.sk") == ["j@x.sk", "b@y.sk"]
assert hub.imap_name("Folders/Plaveck&AP0-") == "Folders/Plavecký" and hub.imap_name("A&-B") == "A&B" and hub.imap_name("&Jjo-") == "☺"
assert req("GET", "/admin/api/mailbox/folders", admin_cookie)[0] == 503, "not set up yet: says so"
print("ok mailbox")
# ---- 1.48.1: mail about new reports goes to those who may act on them, never to read-only accounts
make("mail-ro", "read-only"); make("mail-bugs", "custom", {"bugs": "rw"}); make("mail-look", "custom", {"bugs": "r", "ideas": "r"})
with hub.db() as conn:
    hub.save_settings(conn, {"admin_email": ""})
to = hub.staff_mail("bugs").split(",")
assert "mail-bugs@x.sk" in to and "mail-ro@x.sk" not in to and "mail-look@x.sk" not in to, to
assert "mail-bugs@x.sk" not in hub.staff_mail("ideas") and hub.staff_mail("ideas"), "admins still hear about ideas"
print("ok staff mail")
server.shutdown()
print("all good")
