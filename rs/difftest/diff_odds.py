"""On the hub: transfer odds (hub.transfer_odds), Python against Rust, for pairs of trips with riders' history."""
import json, random, sys, time, urllib.parse
sys.path.insert(0, sys.argv[1])
import hub  # noqa: E402

out = sys.argv[2]
NOW = int(time.time())
open(f"{out}/now.txt", "w").write(str(NOW))
hub.now = lambda: NOW
random.seed(4)

# Round 2: transfer odds themselves, for pairs of trips with riders' history (few plans have one in the evening)
with hub.db() as conn:
    keys = [r[0] for r in conn.execute("""SELECT trip_key FROM community_reports WHERE created_at >= ? GROUP BY trip_key
                                          ORDER BY COUNT(*) DESC LIMIT 400""", (NOW - 120 * 86400,))]
    q2, a2 = [], []
    for _ in range(1500):
        ka, kb = random.choice(keys), random.choice(keys)
        slack = random.choice([-120, 0, 60, 180, 300, 900])
        q2.append(("odds", urllib.parse.urlencode({"a": ka, "b": kb, "slack": slack})))
        a2.append([200, hub.transfer_odds(conn, ka, kb, slack)])
with open(f"{out}/queries.txt", "w", encoding="utf-8") as f:
    f.writelines(f"{r}\t{x}\n" for r, x in q2)
with open(f"{out}/python.jsonl", "w", encoding="utf-8") as f:
    f.writelines(json.dumps(a, ensure_ascii=False) + "\n" for a in a2)
print(len(q2), "transfer odds,", sum(1 for _, o in a2 if o), "known,", sum(1 for _, o in a2 if o and 0 < o["worked"] < o["runs"]), "partly working")
