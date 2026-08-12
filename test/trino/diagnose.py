#!/usr/bin/env python3
"""Re-run the ERROR cases one at a time to capture their messages."""
import json
import re
import subprocess
import sys
from collections import Counter

from run_conformance import PRELUDE, normalize_expected, normalize_query


def run_one(c):
    got = normalize_query(c["translated"])
    if c["kind"] == "empty":
        stmt = f"SELECT count(*) AS n FROM ({got}) _g;"
    else:
        want = normalize_expected(c["expected"])
        stmt = ("WITH got AS (" + got + "), want AS (" + want + ") "
                "SELECT count(*) AS n FROM ("
                "(SELECT * FROM got EXCEPT ALL SELECT * FROM want) UNION ALL "
                "(SELECT * FROM want EXCEPT ALL SELECT * FROM got));")
    p = subprocess.run(["uvx", "haybarn-cli"], input=PRELUDE + stmt,
                       capture_output=True, text=True, timeout=300)
    blob = p.stdout + p.stderr
    for line in blob.splitlines():
        if re.search(r"Error|Exception", line):
            return re.sub(r"\s+", " ", line).strip()
    return re.sub(r"\s+", " ", blob).strip()[:200]


def main():
    cases = json.load(open(sys.argv[1]))
    errs = [c for c in cases if c.get("verdict") == "ERROR"]
    print(f"diagnosing {len(errs)} error cases", file=sys.stderr)
    for i, c in enumerate(errs):
        c["detail"] = run_one(c)
        print(f"  [{i+1}/{len(errs)}] {c['method']}: {c['detail'][:150]}", file=sys.stderr)
    json.dump(cases, open(sys.argv[1], "w"), indent=1)
    tally = Counter()
    for c in errs:
        d = c["detail"]
        d = re.sub(r"\[worker.*", "", d)
        d = re.sub(r"'[^']*'", "'X'", d)
        tally[d[:150]] += 1
    print("\n=== error classes ===")
    for k, v in tally.most_common(30):
        print(f"{v:3d}x  {k}")


if __name__ == "__main__":
    main()
