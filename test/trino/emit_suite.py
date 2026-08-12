#!/usr/bin/env python3
"""Emit a self-contained SQLLogic file from the passing Trino conformance cases.

Each case compares our result against Trino's expected VALUES with a symmetric
EXCEPT ALL, so the assertion is exact without having to reformat every literal
into SQLLogic's text conventions.
"""
import json
import re
import sys

from run_conformance import normalize_expected, normalize_query

HEADER = """# name: test/sql/trino_conformance.test
# description: SQL:2016 row-pattern conformance cases ported from Trino's TestRowPatternMatching / TestAggregationsInRowPatternMatching.
# group: [sql]
#
# GENERATED FILE - do not edit by hand. Regenerate with test/trino/port.sh
# (see test/trino/README.md). Each case runs the translated query and compares it
# against Trino's expected result with a symmetric EXCEPT ALL, so 'PASS' means the
# two results are identical as multisets.

require-env VGI_MATCHRECOGNIZE_WORKER

statement ok
ATTACH 'mr' AS mr (TYPE vgi, LOCATION '${VGI_MATCHRECOGNIZE_WORKER}');
"""


def main():
    cases = json.load(open(sys.argv[1]))
    passing = [c for c in cases if c.get("verdict") == "PASS"]
    out = [HEADER]
    by_method = {}
    for c in passing:
        by_method.setdefault(c["method"], []).append(c)

    n = 0
    for method in sorted(by_method):
        out.append(f"\n# ============ {method} ============")
        for c in by_method[method]:
            n += 1
            got = normalize_query(c["translated"])
            # SQLLogic statements must be single-line-ish; collapse whitespace.
            got = " ".join(got.split())
            pat = re.search(r"pattern := '([^']*(?:''[^']*)*)'", got)
            label = f"PATTERN ({pat.group(1)})" if pat else ""
            out.append(f"\n# [{n}] {label}")
            if c["kind"] == "empty":
                out.append("query I")
                out.append(
                    f"SELECT CASE WHEN count(*) = 0 THEN 'PASS' ELSE 'FAIL' END FROM ({got}) _g;"
                )
            else:
                want = " ".join(normalize_expected(c["expected"]).split())
                out.append("query I")
                out.append(
                    f"WITH got AS ({got}), want AS ({want}) "
                    "SELECT CASE WHEN (SELECT count(*) FROM "
                    "((SELECT * FROM got EXCEPT ALL SELECT * FROM want) UNION ALL "
                    "(SELECT * FROM want EXCEPT ALL SELECT * FROM got))) = 0 "
                    "THEN 'PASS' ELSE 'FAIL' END;"
                )
            out.append("----")
            out.append("PASS")
    open(sys.argv[2], "w").write("\n".join(out) + "\n")
    print(f"wrote {n} cases across {len(by_method)} Trino test methods")


if __name__ == "__main__":
    main()
