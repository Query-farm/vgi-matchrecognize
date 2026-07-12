//! Catalog / schema metadata and the `match_recognize` function metadata,
//! surfaced to DuckDB and the `vgi-lint` metadata-quality linter.

use vgi::catalog::{CatSchema, CatView, CatalogModel};
use vgi::function::FunctionMetadata;

const REPO: &str = "https://github.com/Query-farm/vgi-matchrecognize";

/// A curated, browsable reference of the AFTER MATCH SKIP modes accepted by
/// `match_recognize`'s `after` argument. Backed entirely by inline `VALUES`, so
/// it scans locally with no worker round-trip, no network, and no credentials.
const AFTER_MATCH_SKIP_MODES_DEF: &str = "SELECT mode, needs_variable, resumes_at FROM (VALUES \
    ('past last row', FALSE, 'Resume the next match search at the row immediately after the last \
     row of the matched span. This is the default when after is omitted.'), \
    ('to next row', FALSE, 'Resume at the row immediately after the FIRST row of the matched \
     span, so overlapping matches can be found.'), \
    ('to first <var>', TRUE, 'Resume at the first row that was bound to the named pattern \
     variable; replace <var> with a variable named in define or pattern.'), \
    ('to last <var>', TRUE, 'Resume at the last row that was bound to the named pattern \
     variable; replace <var> with a variable named in define or pattern.') \
    ) AS t(mode, needs_variable, resumes_at)";

/// Object-level example for the `after_match_skip_modes` view. A JSON array of
/// `{description, sql}` (VGI502), fully catalog-qualified, projecting real
/// columns rather than a bare `SELECT *` (VGI514).
const AFTER_MODES_EXAMPLES: &str = r#"[
  {
    "description": "List the two parameter-free AFTER MATCH SKIP modes and where each resumes the next match search.",
    "sql": "SELECT mode, resumes_at FROM mr.main.after_match_skip_modes WHERE needs_variable = FALSE ORDER BY mode"
  }
]"#;

/// Example query that runs against inline `VALUES` (no external data), so it
/// executes cleanly in the `vgi-lint` sandbox. Single-quoted JSON args carry
/// escaped double quotes (this is a raw string, so `\"` is literal JSON).
const EXEC_EXAMPLES: &str = r#"[
  {
    "description": "V-shape: a falling run then a rising run, one summary row per match. Returns match_no=1, bottom=6.",
    "sql": "SELECT match_no, bottom FROM mr.main.match_recognize((SELECT * FROM (VALUES ('ACME',1,10),('ACME',2,8),('ACME',3,6),('ACME',4,9),('ACME',5,11)) AS t(symbol,ts,price)), partition_by := ['symbol'], order_by := ['ts'], pattern := 'START DOWN+ UP+', define := '{\"DOWN\":\"price < PREV(price)\",\"UP\":\"price > PREV(price)\"}', measures := '{\"match_no\":\"MATCH_NUMBER()\",\"bottom\":\"LAST(DOWN.price)\"}')"
  },
  {
    "description": "Brute-force then breach: three or more failed logins immediately followed by a success, ALL ROWS PER MATCH with each event tagged by classifier.",
    "sql": "SELECT classifier, n_fails FROM mr.main.match_recognize((SELECT * FROM (VALUES ('u',1,'fail'),('u',2,'fail'),('u',3,'fail'),('u',4,'success')) AS t(uid,ts,outcome)), partition_by := ['uid'], order_by := ['ts'], pattern := 'FAIL{3,} OK', define := '{\"FAIL\":\"outcome = ''fail''\",\"OK\":\"outcome = ''success''\"}', measures := '{\"n_fails\":\"FINAL COUNT(FAIL.*)\"}', rows := 'all')"
  }
]"#;

/// Analyst tasks the `vgi-lint simulate` agent-check pass runs against the
/// worker (VGI152/VGI920). Each `reference_sql` is a known-good query over
/// inline `VALUES`, so it executes cleanly in the sandbox with no external data.
const AGENT_TEST_TASKS: &str = r#"[
  {
    "name": "detect_v_shape",
    "prompt": "This worker has no source tables; build the input relation inline. Call the mr.main.match_recognize table function over this exact relation: (SELECT * FROM (VALUES ('ACME',1,10),('ACME',2,8),('ACME',3,6),('ACME',4,9),('ACME',5,11)) AS t(symbol,ts,price)). Partition by symbol, order by ts, and match the pattern 'START DOWN+ UP+' where variable DOWN means price < PREV(price) and UP means price > PREV(price) (START is unconstrained). Use ONE ROW PER MATCH and project two measures: match_no as MATCH_NUMBER() and bottom as LAST(DOWN.price). Select just match_no and bottom.",
    "reference_sql": "SELECT match_no, bottom FROM mr.main.match_recognize((SELECT * FROM (VALUES ('ACME',1,10),('ACME',2,8),('ACME',3,6),('ACME',4,9),('ACME',5,11)) AS t(symbol,ts,price)), partition_by := ['symbol'], order_by := ['ts'], pattern := 'START DOWN+ UP+', define := '{\"DOWN\":\"price < PREV(price)\",\"UP\":\"price > PREV(price)\"}', measures := '{\"match_no\":\"MATCH_NUMBER()\",\"bottom\":\"LAST(DOWN.price)\"}')"
  },
  {
    "name": "brute_force_then_success",
    "prompt": "This worker has no source tables; build the input relation inline. Call the mr.main.match_recognize table function over this exact relation: (SELECT * FROM (VALUES ('u',1,'fail'),('u',2,'fail'),('u',3,'fail'),('u',4,'success')) AS t(uid,ts,outcome)). Partition by uid, order by ts, and match the pattern 'FAIL{3,} OK' where variable FAIL means outcome = 'fail' and OK means outcome = 'success'. Use ALL ROWS PER MATCH (rows := 'all') and project one measure n_fails as FINAL COUNT(FAIL.*). Select just classifier and n_fails.",
    "reference_sql": "SELECT classifier, n_fails FROM mr.main.match_recognize((SELECT * FROM (VALUES ('u',1,'fail'),('u',2,'fail'),('u',3,'fail'),('u',4,'success')) AS t(uid,ts,outcome)), partition_by := ['uid'], order_by := ['ts'], pattern := 'FAIL{3,} OK', define := '{\"FAIL\":\"outcome = ''fail''\",\"OK\":\"outcome = ''success''\"}', measures := '{\"n_fails\":\"FINAL COUNT(FAIL.*)\"}', rows := 'all')"
  },
  {
    "name": "explain_pattern_structure",
    "prompt": "Call the mr.main.explain_pattern scalar function on the row pattern string 'START DOWN+ UP+' to render its compiled structure and return that single string value. Do not query any table.",
    "reference_sql": "SELECT mr.main.explain_pattern('START DOWN+ UP+') AS compiled",
    "ignore_column_names": true
  },
  {
    "name": "worker_version",
    "prompt": "Call the mr.main.mr_version scalar function to report the running worker's version string and return that single string value. Do not query any table.",
    "reference_sql": "SELECT mr.main.mr_version() AS version",
    "ignore_column_names": true
  },
  {
    "name": "match_number_one_row",
    "prompt": "This worker has no source tables; build the input relation inline. Call the mr.main.match_recognize table function over this exact relation: (SELECT * FROM (VALUES ('ACME',1,10),('ACME',2,8),('ACME',3,6),('ACME',4,9),('ACME',5,11)) AS t(symbol,ts,price)). Partition by symbol, order by ts, and match the pattern 'START DOWN+ UP+' where DOWN means price < PREV(price) and UP means price > PREV(price). Use ONE ROW PER MATCH (rows := 'one', the default). Project a single measure n as MATCH_NUMBER(). Select symbol and n.",
    "reference_sql": "SELECT symbol, n FROM mr.main.match_recognize((SELECT * FROM (VALUES ('ACME',1,10),('ACME',2,8),('ACME',3,6),('ACME',4,9),('ACME',5,11)) AS t(symbol,ts,price)), partition_by := ['symbol'], order_by := ['ts'], pattern := 'START DOWN+ UP+', define := '{\"DOWN\":\"price < PREV(price)\",\"UP\":\"price > PREV(price)\"}', measures := '{\"n\":\"MATCH_NUMBER()\"}')"
  },
  {
    "name": "explain_alternation",
    "prompt": "Call the mr.main.explain_pattern scalar function on the row pattern string 'A+? (B | C) D' (a reluctant one-or-more quantifier followed by an alternation group) to render its compiled structure and return that single string value. Do not query any table.",
    "reference_sql": "SELECT mr.main.explain_pattern('A+? (B | C) D') AS compiled",
    "ignore_column_names": true
  },
  {
    "name": "list_variable_skip_modes",
    "prompt": "The mr.main.after_match_skip_modes reference view documents every AFTER MATCH SKIP mode accepted by match_recognize's `after` argument. Using that view, list only the modes that require naming a pattern variable. Return just the mode column, ordered alphabetically.",
    "reference_sql": "SELECT mode FROM mr.main.after_match_skip_modes WHERE needs_variable ORDER BY mode"
  }
]"#;

/// Metadata for the `match_recognize` table-buffering function.
pub fn match_recognize_metadata() -> FunctionMetadata {
    let mut tags = crate::meta::object_tags(
        "Row Pattern Matching (MATCH_RECOGNIZE)",
        "Run SQL:2016 MATCH_RECOGNIZE row pattern matching over a buffered relation — the \
         table-in / table-out function DuckDB otherwise lacks. The input relation is supplied as a \
         subquery; it is partitioned by `partition_by`, each partition is sorted by `order_by`, \
         and a regular-expression-over-rows `pattern` is matched against it. `define` is a JSON \
         object of boolean predicates (one per pattern variable; PREV/NEXT/FIRST/LAST, running \
         aggregates, arithmetic/comparison/logical operators), `measures` is a JSON object/array \
         of output expressions whose types are inferred at bind time (with an explicit `type` \
         override available via the array form), `rows` selects ONE ROW PER MATCH (default) or ALL \
         ROWS PER MATCH, and `after` selects the AFTER MATCH SKIP mode. The matcher backtracks \
         with a per-partition step budget so it never hangs. Use it for funnel analysis, \
         sessionization, sequence/anomaly detection, and time-series pattern search — and to keep \
         Oracle/Trino/Snowflake MATCH_RECOGNIZE queries working on DuckDB.",
        "MATCH_RECOGNIZE row pattern matching over a buffered subquery relation. Partition with \
         `partition_by`, order with `order_by`, match a `pattern` (variables + concat / `|` / \
         quantifiers / grouping / anchors), constrain variables with a JSON `define`, project a \
         JSON `measures` (types inferred at bind), pick `rows` ('one'/'all') and `after` (AFTER \
         MATCH SKIP). Returns the partition columns plus the measures (ONE ROW) or partition + \
         order + match_number + classifier + measures (ALL ROWS).",
        "match_recognize, row pattern matching, MATCH_RECOGNIZE, SQL:2016, funnel, sessionization, \
         sequence detection, anomaly detection, time series, pattern, define, measures, classifier, \
         match_number, partition by, order by, after match skip, Oracle, Trino, Snowflake",
    );
    tags.push((
        "vgi.result_dynamic_columns_md".into(),
        // The result schema is fixed at bind time but varies by the `rows`
        // argument (and by the caller's own partition/order/measure names), so
        // it is declared as a dynamic schema with one variant table per `rows`
        // mode. The example column names below are those produced by this
        // function's own worked examples.
        "The output columns are **fixed at bind time** and depend on the `rows` argument (and on \
         the caller's own `partition_by` / `order_by` / `measures` names). Measure types are \
         inferred at bind time: `MATCH_NUMBER()` and `COUNT(...)` give BIGINT, `CLASSIFIER()` \
         gives VARCHAR, `FIRST`/`LAST`/`PREV`/`NEXT`/`MIN`/`MAX` and aggregate-of-column give \
         that column's type, `SUM` widens (int to HUGEINT, float to DOUBLE), `AVG` gives DOUBLE, \
         arithmetic gives the widened numeric type, comparison and logical operators give \
         BOOLEAN, and `||` gives VARCHAR; the array measures form can override any inferred \
         type.\n\n\
         ## ONE ROW PER MATCH (`rows := 'one'`, the default)\n\n\
         The `partition_by` columns (carried through with their original names and types) \
         followed by one column per measure, named by the measures key (or its `as`) and \
         nullable. The names below are those of the one-row worked example.\n\n\
         | Name | Type | Description |\n\
         | --- | --- | --- |\n\
         | symbol | VARCHAR | A `partition_by` column, passed through with its original name and type. |\n\
         | match_no | BIGINT | A measure column; here `MATCH_NUMBER()`, the 1-based match ordinal. |\n\
         | bottom | INTEGER | A measure column; here `LAST(DOWN.price)`, so it inherits the price column's type. |\n\n\
         ## ALL ROWS PER MATCH (`rows := 'all'`)\n\n\
         The `partition_by` columns, then the `order_by` columns, then `match_number` and \
         `classifier` (added automatically unless a measure of the same name shadows them), then \
         one column per measure. The names below are those of the all-rows worked example.\n\n\
         | Name | Type | Description |\n\
         | --- | --- | --- |\n\
         | uid | VARCHAR | A `partition_by` column, passed through unchanged. |\n\
         | ts | INTEGER | An `order_by` column, passed through unchanged. |\n\
         | match_number | BIGINT | The 1-based ordinal of the match this row belongs to. |\n\
         | classifier | VARCHAR | The pattern variable this row was bound to. |\n\
         | n_fails | BIGINT | A measure column; here `FINAL COUNT(FAIL.*)`. |"
            .into(),
    ));
    tags.push(("vgi.example_queries".into(), EXEC_EXAMPLES.into()));
    tags.push(("vgi.category".into(), "Row Pattern Matching".into()));
    FunctionMetadata {
        description: "SQL:2016 MATCH_RECOGNIZE row pattern matching over a buffered relation"
            .into(),
        tags,
        ..Default::default()
    }
}

/// Catalog + schema metadata.
pub fn catalog_metadata(name: &str) -> CatalogModel {
    CatalogModel {
        name: name.to_string(),
        comment: Some(
            "SQL:2016 MATCH_RECOGNIZE row pattern matching for DuckDB — funnel analysis, \
             sessionization, and sequence/anomaly detection over event and time-series data."
                .to_string(),
        ),
        tags: vec![
            (
                "vgi.title".to_string(),
                "MATCH_RECOGNIZE — Row Pattern Matching".to_string(),
            ),
            (
                "vgi.keywords".to_string(),
                crate::meta::keywords_json(
                    "match_recognize, row pattern matching, SQL:2016, funnel analysis, \
                     sessionization, sequence detection, anomaly detection, time series, pattern, \
                     define, measures, classifier, match_number, after match skip, CEP, Oracle, \
                     Trino, Snowflake, BigQuery, Flink",
                ),
            ),
            (
                "vgi.doc_llm".to_string(),
                "Brings SQL:2016 MATCH_RECOGNIZE (row pattern matching) to DuckDB, which has no \
                 native support for it. The single table-in/table-out function \
                 `mr.match_recognize((<relation>), partition_by:=, order_by:=, pattern:=, \
                 define:=, measures:=, rows:=, after:=)` buffers the input relation, partitions \
                 and sorts it, and runs a backtracking regular-expression-over-rows matcher, \
                 emitting either one summary row per match or every matched row (tagged with its \
                 match_number and the pattern variable it matched). Patterns support \
                 concatenation, alternation, greedy/reluctant quantifiers, grouping, and \
                 partition-edge anchors; DEFINE/MEASURES expressions support column refs, \
                 PREV/NEXT/FIRST/LAST navigation, running aggregates, CLASSIFIER()/MATCH_NUMBER(), \
                 RUNNING/FINAL, and arithmetic/comparison/logical/BETWEEN/IN/`||` operators. \
                 Measure output types are inferred at bind time with an explicit type-override \
                 escape hatch. Also provides `mr_version()` and `explain_pattern()`. Pure local \
                 compute: no network, no secrets, nothing on disk."
                    .to_string(),
            ),
            (
                "vgi.doc_md".to_string(),
                "# mr — MATCH_RECOGNIZE for DuckDB\n\nSQL:2016 **row pattern matching** as a \
                 table-in/table-out function. DuckDB has no native `MATCH_RECOGNIZE`; this worker \
                 supplies it so funnel analysis, sessionization, and sequence/anomaly detection \
                 read as the standard SQL the rest of the industry (Oracle, Trino, Snowflake, \
                 BigQuery, Flink) already uses, instead of fragile `LAG`/`LEAD` + self-join \
                 hacks.\n\nCall `mr.main.match_recognize((<subquery>), partition_by := [...], \
                 order_by := [...], pattern := '...', define := '{...}', measures := '{...}', rows \
                 := 'one'|'all', after := '...')`. The pattern is a regex over **pattern \
                 variables**; `define` constrains each variable with a boolean predicate; \
                 `measures` projects the output (types inferred at bind, override via the array \
                 form). Helpers: `mr_version()` and `explain_pattern(p)`."
                    .to_string(),
            ),
            (
                "vgi.doc_links".to_string(),
                "[{\"title\":\"ISO/IEC 9075-2 (SQL:2016) row pattern recognition\",\"url\":\
                 \"https://www.iso.org/standard/63556.html\"},{\"title\":\"Trino MATCH_RECOGNIZE\",\
                 \"url\":\"https://trino.io/docs/current/sql/match-recognize.html\"},{\"title\":\
                 \"Oracle MATCH_RECOGNIZE\",\"url\":\
                 \"https://docs.oracle.com/en/database/oracle/oracle-database/19/dwhsg/sql-pattern-matching.html\"}]"
                    .to_string(),
            ),
            ("vgi.author".to_string(), "Query.Farm".to_string()),
            (
                "vgi.copyright".to_string(),
                "Copyright 2026 Query Farm LLC - https://query.farm".to_string(),
            ),
            ("vgi.license".to_string(), "MIT".to_string()),
            (
                "vgi.support_contact".to_string(),
                format!("{REPO}/issues"),
            ),
            (
                "vgi.support_policy_url".to_string(),
                format!("{REPO}/blob/main/README.md"),
            ),
            (
                "vgi.agent_test_tasks".to_string(),
                AGENT_TEST_TASKS.to_string(),
            ),
        ],
        source_url: Some(REPO.to_string()),
        schemas: vec![CatSchema {
            name: "main".to_string(),
            comment: Some(
                "Row pattern matching functions: match_recognize, mr_version, explain_pattern."
                    .to_string(),
            ),
            tags: vec![
                ("vgi.title".to_string(), "MATCH_RECOGNIZE — main".to_string()),
                (
                    "vgi.keywords".to_string(),
                    crate::meta::keywords_json(
                        "match_recognize, mr_version, explain_pattern, row pattern matching, \
                         funnel, sessionization, sequence detection, pattern, define, measures",
                    ),
                ),
                // VGI123 bare-key classifiers for faceting.
                ("domain".to_string(), "data-analytics".to_string()),
                ("category".to_string(), "pattern-matching".to_string()),
                ("topic".to_string(), "row-pattern-recognition".to_string()),
                // VGI413 category registry; each object declares a `vgi.category`
                // naming one of these.
                (
                    "vgi.categories".to_string(),
                    "[{\"name\":\"Row Pattern Matching\",\"description\":\"The core SQL:2016 \
                     MATCH_RECOGNIZE table function that runs a regular-expression-over-rows \
                     matcher for funnel analysis, sessionization, and sequence/anomaly \
                     detection.\"},{\"name\":\"Diagnostics\",\"description\":\"Helper scalars for \
                     inspecting the worker and pattern compilation — the worker version and a \
                     pattern pretty-printer — that touch no data.\"}]"
                        .to_string(),
                ),
                (
                    "vgi.doc_llm".to_string(),
                    "The single schema for the `mr` worker (the catalog name matches the ATTACH \
                     name, so qualify calls as `mr.main.<fn>(...)`). It holds `match_recognize` \
                     (the table-in/table-out row pattern matcher), `mr_version()` (the worker \
                     version), and `explain_pattern(p)` (pretty-print a compiled pattern, no \
                     data)."
                        .to_string(),
                ),
                (
                    "vgi.doc_md".to_string(),
                    "## `mr.main` — row pattern matching\n\nThe single schema of the `mr` worker, \
                     bringing SQL:2016 `MATCH_RECOGNIZE` to DuckDB. Qualify every call as \
                     `mr.main.<fn>(...)`, matching the catalog's `ATTACH` name.\n\n**Key \
                     concepts.** You describe a sequence as a regular expression over *pattern \
                     variables*; a `define` predicate constrains each variable, and `measures` \
                     projects the matched runs into output columns whose types are inferred at \
                     bind time.\n\n**When to use it.** Reach for this schema whenever a question \
                     is really \"find rows that occur in this order\" — funnel and conversion \
                     analysis, sessionization, and sequence or anomaly detection over event and \
                     time-series data — instead of hand-rolling fragile `LAG`/`LEAD` plus \
                     self-join workarounds."
                        .to_string(),
                ),
                (
                    "vgi.example_queries".to_string(),
                    "SELECT mr.main.mr_version();\n\
                     SELECT mr.main.explain_pattern('START DOWN+ UP+');\n\
                     SELECT * FROM mr.main.match_recognize((SELECT * FROM (VALUES \
                     ('ACME',1,10),('ACME',2,8),('ACME',3,6),('ACME',4,9),('ACME',5,11)) AS \
                     t(symbol,ts,price)), partition_by := ['symbol'], order_by := ['ts'], pattern \
                     := 'START DOWN+ UP+', define := '{\"DOWN\":\"price < PREV(price)\",\"UP\":\
                     \"price > PREV(price)\"}', measures := '{\"n\":\"MATCH_NUMBER()\"}');"
                        .to_string(),
                ),
            ],
            views: vec![CatView {
                name: "after_match_skip_modes".to_string(),
                definition: AFTER_MATCH_SKIP_MODES_DEF.to_string(),
                comment: Some(
                    "Browsable reference of the AFTER MATCH SKIP modes accepted by \
                     match_recognize's `after` argument: the mode string, whether it names a \
                     pattern variable, and where the next match search resumes. Backed by inline \
                     VALUES — it scans locally with no worker round-trip, network, or credentials."
                        .to_string(),
                ),
                tags: vec![
                    (
                        "vgi.title".to_string(),
                        "Row-pattern SKIP mode reference".to_string(),
                    ),
                    // VGI123 classifying bare-keys for faceting.
                    ("domain".to_string(), "data-analytics".to_string()),
                    ("topic".to_string(), "row-pattern-recognition".to_string()),
                    (
                        "vgi.doc_llm".to_string(),
                        "A curated, credential-free reference view of the four AFTER MATCH SKIP \
                         modes that match_recognize's `after` argument accepts. Each row gives the \
                         mode text ('past last row', 'to next row', 'to first <var>', 'to last \
                         <var>'), a needs_variable flag marking the two variable-relative forms \
                         whose <var> is replaced by a real pattern variable, and a resumes_at \
                         sentence explaining where the search for the next match continues. Query \
                         it to discover the valid `after` values without trial and error."
                            .to_string(),
                    ),
                    (
                        "vgi.doc_md".to_string(),
                        "## `after_match_skip_modes`\n\nA browsable reference for the AFTER MATCH \
                         SKIP modes accepted by `match_recognize`'s `after` argument. Columns: \
                         `mode` (the text to pass, or a `<var>` template), `needs_variable` (TRUE \
                         for the two variable-relative forms), and `resumes_at` (a plain-English \
                         description of where the next match search continues). The two \
                         `needs_variable = FALSE` rows are complete literals; for the other two, \
                         replace `<var>` with a pattern variable named in `define` or `pattern`."
                            .to_string(),
                    ),
                    (
                        "vgi.keywords".to_string(),
                        crate::meta::keywords_json(
                            "after match skip, skip mode, past last row, to next row, to first, \
                             to last, resume, match_recognize, reference",
                        ),
                    ),
                    ("vgi.category".to_string(), "Diagnostics".to_string()),
                    ("vgi.example_queries".to_string(), AFTER_MODES_EXAMPLES.to_string()),
                ],
                column_comments: vec![
                    (
                        "mode".to_string(),
                        "The AFTER MATCH SKIP mode text to pass as `after`; the `<var>` forms are \
                         templates where `<var>` is replaced by a pattern variable."
                            .to_string(),
                    ),
                    (
                        "needs_variable".to_string(),
                        "TRUE for the two variable-relative modes ('to first <var>' / 'to last \
                         <var>'), FALSE for the two complete-literal modes."
                            .to_string(),
                    ),
                    (
                        "resumes_at".to_string(),
                        "Plain-English description of where the search for the next match resumes \
                         under this mode."
                            .to_string(),
                    ),
                ],
            }],
            macros: Vec::new(),
            tables: Vec::new(),
        }],
        ..Default::default()
    }
}
