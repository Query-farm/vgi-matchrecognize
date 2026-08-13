#!/usr/bin/env python3
"""Translate Trino MATCH_RECOGNIZE queries to mr.main.match_recognize(...) calls.

Trino:                                  ours:
  FROM src MATCH_RECOGNIZE (              FROM mr.main.match_recognize(
    PARTITION BY a  ORDER BY b              (SELECT * FROM src),
    MEASURES e AS n                         partition_by := ['a'], order_by := ['b'],
    ALL ROWS PER MATCH                      measures := '{"n":"e"}', rows := 'all',
    AFTER MATCH SKIP PAST LAST ROW          after := 'past last row',
    PATTERN (A B+) DEFINE B AS ...          pattern := 'A B+', define := '{"B":"..."}'
  ) AS m                                  ) AS m
"""
import json
import re

KEYWORDS = [
    "PARTITION BY", "ORDER BY", "MEASURES", "ONE ROW PER MATCH",
    "ALL ROWS PER MATCH", "AFTER MATCH SKIP", "PATTERN", "SUBSET", "DEFINE",
]


class Unsupported(Exception):
    pass


def find_balanced(s, open_at):
    """Index just past the ')' matching the '(' at open_at."""
    depth = 0
    in_str = False
    for i in range(open_at, len(s)):
        c = s[i]
        if c == "'":
            in_str = not in_str
        if in_str:
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return i + 1
    raise Unsupported("unbalanced parentheses")


def top_level_find(s, needle, start=0):
    """Find `needle` (case-insensitive) at paren depth 0, outside string literals."""
    depth = 0
    in_str = False
    n = len(needle)
    up = s.upper()
    needle = needle.upper()
    i = start
    while i < len(s):
        c = s[i]
        if c == "'":
            in_str = not in_str
            i += 1
            continue
        if not in_str:
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
            elif depth == 0 and up.startswith(needle, i):
                before = s[i - 1] if i else " "
                after = s[i + n] if i + n < len(s) else " "
                if not (before.isalnum() or before == "_") and not (after.isalnum() or after == "_"):
                    return i
        i += 1
    return -1


def split_top_commas(s):
    parts, depth, cur, in_str = [], 0, [], False
    for c in s:
        if c == "'":
            in_str = not in_str
        if not in_str:
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
            elif c == "," and depth == 0:
                parts.append("".join(cur).strip())
                cur = []
                continue
        cur.append(c)
    if "".join(cur).strip():
        parts.append("".join(cur).strip())
    return parts


def jstr(s):
    """Embed a SQL expression inside a single-quoted SQL literal holding JSON."""
    return s


def sql_lit(s):
    return "'" + s.replace("'", "''") + "'"


def parse_mr(body):
    """Split a MATCH_RECOGNIZE body into its clauses."""
    hits = []
    for kw in KEYWORDS:
        idx = top_level_find(body, kw)
        while idx >= 0:
            hits.append((idx, kw))
            idx = top_level_find(body, kw, idx + 1)
    # ORDER BY inside MEASURES/DEFINE expressions would confuse us; keep the first
    # occurrence of each keyword only, in positional order.
    seen = set()
    ordered = []
    for idx, kw in sorted(hits):
        if kw in seen:
            continue
        seen.add(kw)
        ordered.append((idx, kw))
    out = {}
    for i, (idx, kw) in enumerate(ordered):
        start = idx + len(kw)
        end = ordered[i + 1][0] if i + 1 < len(ordered) else len(body)
        out[kw] = body[start:end].strip()
    return out


def col_list(s):
    """`a, b DESC` -> ['a', 'b DESC'] with Trino sort suffixes preserved."""
    items = []
    for it in split_top_commas(s):
        it = " ".join(it.split())
        items.append(it)
    return items


def parse_measures(s):
    out = {}
    for item in split_top_commas(s):
        # The measure name follows the LAST top-level ' AS '.
        pos = -1
        idx = top_level_find(item, "AS")
        while idx >= 0:
            pos = idx
            idx = top_level_find(item, "AS", idx + 1)
        if pos < 0:
            raise Unsupported(f"measure without AS: {item}")
        expr = item[:pos].strip()
        name = item[pos + 2:].strip().strip('"')
        out[name] = " ".join(expr.split())
    return out


def parse_define(s):
    out = {}
    for item in split_top_commas(s):
        idx = top_level_find(item, "AS")
        if idx < 0:
            raise Unsupported(f"define without AS: {item}")
        name = item[:idx].strip().strip('"')
        out[name] = " ".join(item[idx + 2:].split())
    return out


def parse_subset(s):
    """`U = (A, B), V = (C)` -> {"U": ["A", "B"], "V": ["C"]}"""
    out = {}
    for item in split_top_commas_paren_aware(s):
        if "=" not in item:
            raise Unsupported(f"malformed SUBSET item: {item}")
        name, members = item.split("=", 1)
        name = name.strip().strip('"')
        members = members.strip()
        if not (members.startswith("(") and members.endswith(")")):
            raise Unsupported(f"malformed SUBSET members: {members}")
        out[name] = [m.strip().strip('"') for m in members[1:-1].split(",") if m.strip()]
    return out


def split_top_commas_paren_aware(s):
    """Split `U = (A, B), V = (C)` at the commas *between* items."""
    parts, depth, cur = [], 0, []
    for c in s:
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
            continue
        cur.append(c)
    if "".join(cur).strip():
        parts.append("".join(cur))
    return [p.strip() for p in parts]


def normalize_after(s):
    t = " ".join(s.split()).upper()
    if t == "PAST LAST ROW":
        return "past last row"
    if t == "TO NEXT ROW":
        return "to next row"
    m = re.match(r"TO (FIRST|LAST) (\w+)$", t)
    if m:
        return f"to {m.group(1).lower()} {m.group(2)}"
    m = re.match(r"TO (\w+)$", t)
    if m:
        # Trino allows `SKIP TO <label>`, meaning SKIP TO LAST <label>.
        return f"to last {m.group(1)}"
    raise Unsupported(f"after match skip: {s}")


def translate(query):
    q = " ".join(query.split())
    mr_at = top_level_find(q, "MATCH_RECOGNIZE")
    if mr_at < 0:
        raise Unsupported("no MATCH_RECOGNIZE")
    if top_level_find(q, "MATCH_RECOGNIZE", mr_at + 1) >= 0:
        raise Unsupported("multiple MATCH_RECOGNIZE")
    open_at = q.index("(", mr_at)
    close_at = find_balanced(q, open_at)
    head = q[:mr_at].rstrip()
    body = q[open_at + 1:close_at - 1]
    tail = q[close_at:].strip()

    # head := SELECT <sel> FROM <source>
    fm = top_level_find(head, "FROM")
    if fm < 0:
        raise Unsupported("no FROM")
    select_list = head[len("SELECT"):fm].strip() if head.upper().startswith("SELECT") else None
    if select_list is None:
        raise Unsupported("head is not a SELECT")
    source = head[fm + 4:].strip()

    cl = parse_mr(body)
    if "PATTERN" not in cl:
        raise Unsupported("no PATTERN")

    pat_raw = cl["PATTERN"].strip()
    if not pat_raw.startswith("("):
        raise Unsupported("PATTERN not parenthesized")
    pat_end = find_balanced(pat_raw, 0)
    pattern = pat_raw[1:pat_end - 1].strip()
    leftover = pat_raw[pat_end:].strip()
    if leftover:
        raise Unsupported(f"trailing tokens after PATTERN: {leftover}")
    if "{-" in pattern:
        raise Unsupported("exclusion syntax {- -} not implemented")

    rows_all = "ALL ROWS PER MATCH" in cl
    empty_mode = None
    if rows_all:
        mode = " ".join(cl["ALL ROWS PER MATCH"].split()).upper()
        # strip the AFTER MATCH SKIP part if it got glued on (it is a later keyword,
        # so parse_mr already separated it)
        if mode.startswith("SHOW EMPTY MATCHES"):
            empty_mode = "show"
        elif mode.startswith("OMIT EMPTY MATCHES"):
            empty_mode = "omit"
        elif mode.startswith("WITH UNMATCHED ROWS"):
            raise Unsupported("WITH UNMATCHED ROWS not implemented")
        elif mode:
            raise Unsupported(f"unrecognized ALL ROWS modifier: {mode}")

    order_by = col_list(cl["ORDER BY"]) if "ORDER BY" in cl else []
    partition_by = col_list(cl["PARTITION BY"]) if "PARTITION BY" in cl else []
    measures = parse_measures(cl["MEASURES"]) if "MEASURES" in cl else {}
    subsets = parse_subset(cl["SUBSET"]) if "SUBSET" in cl else {}
    define = parse_define(cl["DEFINE"]) if "DEFINE" in cl else {}
    after = normalize_after(cl["AFTER MATCH SKIP"]) if "AFTER MATCH SKIP" in cl else "past last row"

    if not order_by:
        raise Unsupported("no ORDER BY (our order_by is required)")

    args = [f"(SELECT * FROM {source})"]
    if partition_by:
        args.append("partition_by := [" + ", ".join(sql_lit(c) for c in partition_by) + "]")
    args.append("order_by := [" + ", ".join(sql_lit(c) for c in order_by) + "]")
    args.append("pattern := " + sql_lit(pattern))
    args.append("define := " + sql_lit(json.dumps(define)))
    if subsets:
        args.append("subset := " + sql_lit(json.dumps(subsets)))
    if measures:
        args.append("measures := " + sql_lit(json.dumps(measures)))
    args.append("rows := " + ("'all'" if rows_all else "'one'"))
    if empty_mode == "omit":
        args.append("empty_matches := 'omit'")
    args.append("after := " + sql_lit(after))

    call = "mr.main.match_recognize(\n    " + ",\n    ".join(args) + "\n  )"
    out = f"SELECT {select_list} FROM {call}"
    if tail:
        # `AS m` / `AS m(...)` alias, plus any trailing ORDER BY
        out += " " + tail
    return out, {"empty_mode": empty_mode, "rows_all": rows_all}


if __name__ == "__main__":
    import sys
    cases = json.load(open(sys.argv[1]))
    ok = 0
    skipped = {}
    out = []
    for c in cases:
        try:
            sql, meta = translate(c["query"])
            c["translated"] = sql
            c["meta"] = meta
            out.append(c)
            ok += 1
        except Unsupported as e:
            key = str(e).split(":")[0]
            skipped[key] = skipped.get(key, 0) + 1
            c["skip_reason"] = str(e)
            out.append(c)
    print(f"translated {ok}/{len(cases)}", file=sys.stderr)
    for k, v in sorted(skipped.items(), key=lambda kv: -kv[1]):
        print(f"  skipped {v:3d}  {k}", file=sys.stderr)
    json.dump(out, open(sys.argv[2], "w"), indent=1)
