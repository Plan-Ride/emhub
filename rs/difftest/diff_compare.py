"""Python's and Rust's answers to the same queries, line by line: how many are the same, and the first differences."""
import collections, json, sys

folder = sys.argv[1]
queries = open(f"{folder}/queries.txt", encoding="utf-8").read().splitlines()
py = [json.loads(x) for x in open(f"{folder}/python.jsonl", encoding="utf-8")]
rs = [json.loads(x) for x in open(f"{folder}/rust.jsonl", encoding="utf-8")]
assert len(py) == len(rs) == len(queries), (len(py), len(rs), len(queries))


def first_diff(a, b, path=""):
    if type(a) is not type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float))):
        return f"{path}: {json.dumps(a, ensure_ascii=False)[:150]} != {json.dumps(b, ensure_ascii=False)[:150]}"
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                return f"{path}.{k}: only in {'python' if k in a else 'rust'}"
            d = first_diff(a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: {len(a)} items != {len(b)}"
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_diff(x, y, f"{path}[{i}]")
            if d:
                return d
        return None
    return None if a == b else f"{path}: {json.dumps(a, ensure_ascii=False)[:150]} != {json.dumps(b, ensure_ascii=False)[:150]}"


per, shown = collections.Counter(), collections.Counter()
for q, a, b in zip(queries, py, rs):
    route = q.split("\t")[0]
    d = first_diff(a, b)
    per[(route, d is None)] += 1
    if d and shown[route] < 6:
        shown[route] += 1
        print(f"DIFF {q[:140]}\n     {d}")
for route in sorted({r for r, _ in per}):
    print(f"{route:<12} same {per[(route, True)]:>5}  different {per[(route, False)]:>5}")
