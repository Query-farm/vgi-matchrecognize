#!/usr/bin/env python3
"""Extract (query, expected) pairs from Trino's row-pattern JUnit tests.

Flattens the Java source into a placeholder string: every string literal becomes
\x01<n>\x01 and comments are dropped. All structural work (balanced parens,
locating the assertion that follows a query) then happens on plain text, so an
assertion can never be attributed to the wrong query — the earlier bug, where a
preceding `.returnsEmptyResult()` sat in the same lexer chunk as the next
`assertions.query(`, is structurally impossible here.
"""
import json
import re
import sys

MARK = "\x01"


def flatten(src):
    """-> (flat_text, literals). Strings become \x01n\x01; comments removed."""
    lits = []
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        # Java text block: """ ... """ (used by the aggregations tests). Exact
        # incidental-indentation stripping does not matter here because callers
        # collapse whitespace anyway.
        if src.startswith('"""', i):
            end = src.find('"""', i + 3)
            if end < 0:
                end = n
            out.append(f"{MARK}{len(lits)}{MARK}")
            lits.append(src[i + 3:end])
            i = end + 3
            continue
        if c == '"':
            j = i + 1
            buf = []
            while j < n:
                if src[j] == "\\":
                    buf.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\", "'": "'"}
                               .get(src[j + 1], src[j + 1]))
                    j += 2
                    continue
                if src[j] == '"':
                    break
                buf.append(src[j])
                j += 1
            out.append(f"{MARK}{len(lits)}{MARK}")
            lits.append("".join(buf))
            i = j + 1
            continue
        if src.startswith("//", i):
            k = src.find("\n", i)
            i = n if k < 0 else k
            continue
        if src.startswith("/*", i):
            k = src.find("*/", i)
            i = n if k < 0 else k + 2
            continue
        out.append(c)
        i += 1
    return "".join(out), lits


def balanced_end(s, open_at):
    depth = 0
    for i in range(open_at, len(s)):
        if s[i] == "(":
            depth += 1
        elif s[i] == ")":
            depth -= 1
            if depth == 0:
                return i
    raise ValueError("unbalanced")


def split_top_commas(s):
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
    return parts


def evaluate(expr, lits, variables):
    """Resolve a Java string expression (placeholders, +, format(), var) to text."""
    expr = expr.strip()
    fm = re.match(r"^format\s*\(", expr)
    if fm:
        end = balanced_end(expr, fm.end() - 1)
        args = split_top_commas(expr[fm.end():end])
        if not args:
            return None
        tmpl = evaluate(args[0], lits, variables)
        if tmpl is None:
            return None
        for a in args[1:]:
            v = evaluate(a, lits, variables)
            if v is None:
                return None
            tmpl = tmpl.replace("%s", v, 1)
        return tmpl
    # concatenation of placeholders and/or a variable name
    pieces = []
    for tok in re.split(r"\+", expr):
        tok = tok.strip()
        if not tok:
            continue
        m = re.fullmatch(rf"{MARK}(\d+){MARK}", tok)
        if m:
            pieces.append(lits[int(m.group(1))])
            continue
        if re.fullmatch(r"\w+", tok) and tok in variables:
            pieces.append(variables[tok])
            continue
        return None
    return "".join(pieces) if pieces else None


def extract(path):
    flat, lits = flatten(open(path).read())
    cases = []

    methods = [(m.start(), m.group(1)) for m in re.finditer(r"public void (\w+)\(", flat)]

    def method_at(pos):
        name = "?"
        for start, nm in methods:
            if start <= pos:
                name = nm
            else:
                break
        return name

    # Walk variable definitions and queries in source order, so a `String query =`
    # template resolves to the most recent preceding definition. Several test
    # methods declare their own `query` template; precomputing them all would let
    # the last one in the file win everywhere.
    events = [(mm.start(), "var", mm) for mm in
              re.finditer(r"String\s+(\w+)\s*=\s*([^;]+);", flat)]
    events += [(mm.start(), "query", mm) for mm in
               re.finditer(r"assertions\.query\s*\(", flat)]
    events.sort(key=lambda e: e[0])

    variables = {}
    for _pos, ekind, m in events:
        if ekind == "var":
            val = evaluate(m.group(2), lits, variables)
            if val is not None:
                variables[m.group(1)] = val
            continue
        qstart = m.end() - 1
        try:
            qend = balanced_end(flat, qstart)
        except ValueError:
            continue
        query = evaluate(flat[qstart + 1:qend], lits, variables)
        if not query:
            continue
        # The assertion for THIS query is the first one after the query group and
        # before the next assertThat(.
        rest = flat[qend + 1:]
        nxt = rest.find("assertThat")
        window = rest if nxt < 0 else rest[:nxt]
        kind, expected = None, None
        am = re.search(r"\.(matches|returnsEmptyResult|failure|hasOutputTypes)\s*\(", window)
        if am:
            verb = am.group(1)
            if verb == "matches":
                gs = am.end() - 1
                ge = balanced_end(window, gs)
                expected = evaluate(window[gs + 1:ge], lits, variables)
                kind = "matches" if expected else None
            elif verb == "returnsEmptyResult":
                kind = "empty"
            else:
                kind = "failure"
        if not kind:
            continue
        cases.append({
            "method": method_at(m.start()),
            "query": re.sub(r"\s+", " ", query).strip(),
            "kind": kind,
            "expected": re.sub(r"\s+", " ", expected).strip() if expected else None,
        })
    return cases


if __name__ == "__main__":
    allc = []
    for p in sys.argv[1:]:
        cs = extract(p)
        print(f"{p}: {len(cs)} cases", file=sys.stderr)
        allc += cs
    json.dump(allc, sys.stdout, indent=1)
