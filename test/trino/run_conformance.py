#!/usr/bin/env python3
"""Run translated Trino row-pattern cases against the worker and diff results."""
import json
import re
import subprocess
import sys

WORKER = "/Users/rusty/Development/vgi-matchrecognize/target/release/vgi-matchrecognize-worker"

PRELUDE = f"""LOAD vgi;
ATTACH 'mr' AS mr (TYPE vgi, LOCATION '{WORKER}');
"""


def _split_top(s):
    parts, depth, cur, in_str = [], 0, [], False
    for c in s:
        if c == "'":
            in_str = not in_str
        if not in_str:
            if c in "([":
                depth += 1
            elif c in ")]":
                depth -= 1
            elif c == "," and depth == 0:
                parts.append("".join(cur))
                cur = []
                continue
        cur.append(c)
    parts.append("".join(cur))
    return parts


def paren_values(sql):
    """DuckDB requires each VALUES row parenthesized; Trino allows bare scalars
    (`VALUES 1, 2, 3` / `VALUES CAST(null AS integer)`)."""
    out, i = [], 0
    low = sql.lower()
    while True:
        j = low.find("values", i)
        if j < 0:
            out.append(sql[i:])
            break
        # end of the VALUES list: the enclosing ')' at depth 0, or end of string
        k, depth, in_str = j + 6, 0, False
        while k < len(sql):
            c = sql[k]
            if c == "'":
                in_str = not in_str
            elif not in_str:
                if c in "([":
                    depth += 1
                elif c in ")]":
                    if depth == 0:
                        break
                    depth -= 1
            k += 1
        body = sql[j + 6:k]
        items = _split_top(body)
        fixed = []
        for it in items:
            t = it.strip()
            if t and not t.startswith("("):
                fixed.append(" (" + t + ")")
            else:
                fixed.append(it)
        out.append(sql[i:j + 6] + ",".join(fixed))
        i = k
    return "".join(out)


def normalize_expected(v):
    """Trino literal syntax -> DuckDB."""
    v = re.sub(r"\bVARCHAR\s+'", "'", v, flags=re.I)
    v = re.sub(r"\bDECIMAL\s+'([^']*)'", r"\1", v, flags=re.I)
    v = re.sub(r"\bBIGINT\s+'([^']*)'", r"\1", v, flags=re.I)
    v = re.sub(r"\bARRAY\[", "[", v, flags=re.I)
    # array(varchar) -> VARCHAR[]  (also array(array(integer)))
    for _ in range(3):
        v = re.sub(r"\barray\(([a-z0-9_]+)\)", r"\1[]", v, flags=re.I)
    return paren_values(v)


def normalize_query(q):
    return paren_values(q)


def build_script(cases):
    out = [PRELUDE]
    for i, c in enumerate(cases):
        out.append(f".print @@CASE {i}")
        got = normalize_query(c["translated"])
        if c["kind"] == "empty":
            out.append(
                f"SELECT CASE WHEN count(*) = 0 THEN 'PASS' ELSE 'FAIL rows=' || count(*) END "
                f"AS v FROM ({got}) _g;"
            )
        else:
            want = normalize_expected(c["expected"])
            out.append(
                "WITH got AS (" + got + "), want AS (" + want + ") "
                "SELECT CASE WHEN (SELECT count(*) FROM ("
                "(SELECT * FROM got EXCEPT ALL SELECT * FROM want) UNION ALL "
                "(SELECT * FROM want EXCEPT ALL SELECT * FROM got))) = 0 "
                "THEN 'PASS' ELSE 'FAIL diff' END AS v;"
            )
    return "\n".join(out) + "\n"


def run(script):
    p = subprocess.run(["uvx", "haybarn-cli"], input=script, capture_output=True, text=True, timeout=3600)
    return p.stdout + p.stderr


def parse_output(text, n):
    """Attribute output lines to cases via the @@CASE markers."""
    verdicts = {}
    cur = None
    buf = []
    for line in text.splitlines():
        m = re.match(r"@@CASE (\d+)\s*$", line.strip())
        if m:
            if cur is not None:
                verdicts[cur] = "\n".join(buf)
            cur = int(m.group(1))
            buf = []
            continue
        if cur is not None:
            buf.append(line)
    if cur is not None:
        verdicts[cur] = "\n".join(buf)

    res = {}
    for i in range(n):
        blob = verdicts.get(i, "")
        if re.search(r"\bPASS\b", blob):
            res[i] = ("PASS", "")
        elif "FAIL diff" in blob or re.search(r"FAIL rows=", blob):
            m = re.search(r"FAIL rows=\d+", blob)
            res[i] = ("FAIL", m.group(0) if m else "result mismatch")
        else:
            err = " ".join(
                l.strip() for l in blob.splitlines()
                if re.search(r"Error|error:|Exception", l)
            )
            res[i] = ("ERROR", (err or blob.strip().replace("\n", " "))[:300])
    return res


def main():
    cases = json.load(open(sys.argv[1]))
    # `failure` cases assert Trino's own error text, which is not a portable
    # conformance signal; they are counted as skipped rather than run.
    runnable = [c for c in cases if c.get("translated") and c["kind"] != "failure"]
    script = build_script(runnable)
    open("conformance.sql", "w").write(script)
    text = run(script)
    open("conformance.out", "w").write(text)
    res = parse_output(text, len(runnable))

    for i, c in enumerate(runnable):
        c["verdict"], c["detail"] = res[i]
    json.dump(cases, open(sys.argv[2], "w"), indent=1)

    from collections import Counter
    tally = Counter(c["verdict"] for c in runnable)
    print(f"runnable: {len(runnable)}   {dict(tally)}")
    print()
    by_method = {}
    for c in runnable:
        by_method.setdefault(c["method"], Counter())[c["verdict"]] += 1
    for m, t in sorted(by_method.items()):
        flag = "" if set(t) == {"PASS"} else "   <-- "
        print(f"  {m:42s} {dict(t)}{flag}")
    print()
    errs = Counter()
    for c in runnable:
        if c["verdict"] == "ERROR":
            key = re.sub(r"'[^']*'", "'…'", c["detail"])[:130]
            errs[key] += 1
    for k, v in errs.most_common(15):
        print(f"  {v:3d}x {k}")


if __name__ == "__main__":
    main()
