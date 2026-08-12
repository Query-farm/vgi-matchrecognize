#!/usr/bin/env python3
"""Show a case: original pattern bits, our result, Trino's expected result."""
import json
import re
import subprocess
import sys

from run_conformance import PRELUDE, normalize_expected, normalize_query


def q(sql):
    p = subprocess.run(["uvx", "haybarn-cli"], input=PRELUDE + sql,
                       capture_output=True, text=True, timeout=300)
    return (p.stdout + p.stderr).strip()


def main():
    cases = json.load(open("r.json"))
    method = sys.argv[1]
    want_verdict = sys.argv[2] if len(sys.argv) > 2 else None
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 3
    shown = 0
    for i, c in enumerate(cases):
        if c["method"] != method or not c.get("translated"):
            continue
        if want_verdict and c.get("verdict") != want_verdict:
            continue
        shown += 1
        if shown > limit:
            break
        print("=" * 78)
        pat = re.search(r"PATTERN\s*\(", c["query"])
        print(f"[{i}] {c['method']}  verdict={c.get('verdict')}")
        m = re.search(r"(PATTERN.*?)(?:SUBSET|DEFINE|\)\s*AS\s|\Z)", c["query"], re.S)
        print("TRINO:", (m.group(1)[:220] if m else c["query"][:220]))
        m2 = re.search(r"(DEFINE.*?)(?:\)\s*AS\s|\Z)", c["query"], re.S)
        if m2:
            print("      ", m2.group(1)[:220])
        print("--- ours ---")
        print(q(normalize_query(c["translated"]) + ";"))
        print("--- trino expects ---")
        if c["kind"] == "empty":
            print("(empty result)")
        else:
            print(q("SELECT * FROM (" + normalize_expected(c["expected"]) + ") _w;"))


if __name__ == "__main__":
    main()
