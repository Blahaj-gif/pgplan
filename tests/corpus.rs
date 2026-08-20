//! Does this work on schemas nobody here designed?
//!
//! Every other test in this project runs against a fixture written by the same
//! person who wrote the rule it exercises. That is the failure this whole set
//! of projects exists around: a corpus written by its own author only contains
//! the constructs its author remembered, and it measures agreement rather than
//! accuracy. One project next door scored 17/22 on its own corpus and 1/55 on
//! real material.
//!
//! So: the twenty-four schemas pgseed is benchmarked against, seeded by pgseed
//! at volume, queried on indexes those projects chose, and then perturbed. The
//! two numbers that matter are **caught** — an index was dropped and the gate
//! said so — and **flapped** — nothing changed and the gate complained anyway.
//! The second is the one that decides whether this is usable at all.
//!
//! Fetched, not committed: those schemas belong to their projects. The test
//! skips cleanly when they are absent.
//!
//! `cargo test --test corpus -- --ignored --nocapture`

mod harness;

/// How long one schema's seeding may take before it is written off. Generous
/// against a schema of ordinary size, and a schema that needs longer is
/// reported as unmeasured rather than allowed to stop the survey.
const SEED_BUDGET: u64 = 420;

use std::collections::BTreeSet;
use std::path::PathBuf;

use harness::Db;
use pgplan::explain::plan_of;
use pgplan::regress::{compare, Regression};
use pgplan::shape::Shape;

/// Where pgseed keeps its corpus and its binary. Both are optional.
fn pgseed_dir() -> PathBuf {
    PathBuf::from(std::env::var("PGSEED_DIR").unwrap_or_else(|_| "../pgsow".into()))
}

/// Only the DDL that shapes a schema. A production dump also carries grants,
/// settings and seed rows, and replaying those is not what this is measuring.
fn shapes_the_schema(statement: &str) -> bool {
    let head = statement
        .lines()
        .skip_while(|line| {
            let t = line.trim();
            t.is_empty() || t.starts_with("--")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_uppercase();
    [
        "CREATE TABLE",
        "ALTER TABLE",
        "CREATE TYPE",
        "CREATE DOMAIN",
        "CREATE SEQUENCE",
        "CREATE INDEX",
        "CREATE UNIQUE INDEX",
        "CREATE SCHEMA",
        "CREATE EXTENSION",
    ]
    .iter()
    .any(|allowed| head.starts_with(allowed))
}

fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_body = false;
    for line in sql.lines() {
        let trimmed = line.trim();
        if !quoted && !in_body && (trimmed.starts_with("--") || trimmed.is_empty()) {
            continue;
        }
        if line.contains("$$") {
            in_body = !in_body;
        }
        if in_body {
            current.push_str(line);
            current.push('\n');
            continue;
        }
        let mut rest = line;
        loop {
            let mut at = None;
            for (index, ch) in rest.char_indices() {
                match ch {
                    '\'' => quoted = !quoted,
                    ';' if !quoted => {
                        at = Some(index);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(at) = at else { break };
            current.push_str(&rest[..=at]);
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.clear();
            rest = &rest[at + 1..];
        }
        current.push_str(rest);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// A single-column index on a table with rows in it — the shape of every
/// lookup an application actually issues, and the one a dropped index ruins.
struct Lookup {
    table: String,
    column: String,
    index: String,
}

fn lookups(client: &mut postgres::Client, limit: usize, needs_rows: bool) -> Vec<Lookup> {
    let rows = client
        .query(
            "SELECT c.relname, a.attname, i.relname
             FROM pg_index x
             JOIN pg_class c ON c.oid = x.indrelid
             JOIN pg_class i ON i.oid = x.indexrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = x.indkey[0]
             WHERE n.nspname NOT IN ('pg_catalog','information_schema')
               AND c.relkind = 'r'
               AND x.indnatts = 1
               AND NOT x.indisprimary
               -- A unique index backs a constraint; Postgres refuses to drop
               -- it on its own, and it is not what a careless migration
               -- removes anyway. Left in, the DROP silently failed and the
               -- unchanged plan was scored as pgplan having missed something.
               AND NOT x.indisunique
               AND a.atttypid IN ('int4'::regtype, 'int8'::regtype, 'text'::regtype,
                                  'varchar'::regtype, 'uuid'::regtype)
               AND (NOT $1 OR c.reltuples > 500)
             ORDER BY c.reltuples DESC, c.relname, i.relname",
            &[&needs_rows],
        )
        .unwrap_or_default();

    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for row in &rows {
        let table: String = row.get(0);
        // One query per table: several lookups on one table measure the same
        // thing, and the point is breadth across schemas.
        if !seen.insert(table.clone()) {
            continue;
        }
        out.push(Lookup {
            table,
            column: row.get(1),
            index: row.get(2),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Run a child to completion, or kill it at the deadline.
///
/// Returns whether it finished on its own. pgseed satisfies foreign keys before
/// it fills a table, so `--include` bounds what is *asked for* and not what has
/// to be built to get there: on GitLab, six tables pulled in enough of the rest
/// of the schema to occupy a backend for eight minutes and 1.9 GB. One schema
/// behaving like that must not stop the other twenty-three from being measured.
fn finished_within(mut child: std::process::Child, seconds: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// A value that exists, so the query returns something and the plan is real.
fn a_value(client: &mut postgres::Client, look: &Lookup) -> Option<String> {
    let sql = format!(
        "SELECT quote_literal({}) FROM {} WHERE {} IS NOT NULL LIMIT 1",
        quote(&look.column),
        quote(&look.table),
        quote(&look.column)
    );
    client.query_opt(sql.as_str(), &[]).ok().flatten()?.get(0)
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Default, Debug)]
struct Tally {
    schemas: usize,
    /// Every (table, index, column) the schema offered that had rows behind
    /// it. Reported so the denominator below is visible rather than implied.
    candidates: usize,
    /// Of those, the ones the planner actually reached through the index. Only
    /// these are evidence: a query already scanned sequentially cannot regress
    /// that way, and counting it would flatter the result.
    queries: usize,
    /// The index the query used was dropped and a *sequential scan* was named.
    /// This is the number that is not circular: it required the plan to
    /// actually degrade, and the row threshold to actually be met.
    named_the_scan: usize,
    /// The index was dropped and only its disappearance was named. True by
    /// construction — the index name is in the baseline and gone afterwards —
    /// so it is counted apart rather than folded into a headline.
    named_only_the_index: usize,
    missed: usize,
    /// The index could not be dropped, so the experiment never happened. Kept
    /// out of every ratio rather than counted as a miss.
    undropped: usize,
    /// Findings reported when nothing changed. A floor, not evidence.
    flapped: usize,
    /// Findings reported when something changed but nothing got worse. The
    /// number that decides whether anyone leaves this switched on.
    benign: usize,
}

#[test]
#[ignore]
fn against_schemas_nobody_here_designed() {
    let corpus = pgseed_dir().join("tests/corpus");
    let seeder = pgseed_dir().join("target/release/pgseed.exe");
    if !corpus.exists() {
        eprintln!("corpus not fetched at {corpus:?}; skipping");
        return;
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(&corpus)
        .expect("corpus directory")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();

    let mut total = Tally::default();
    let mut per_schema = Vec::new();
    let mut unmeasured: Vec<String> = Vec::new();

    for file in &files {
        let name = file.file_stem().unwrap().to_string_lossy().to_string();
        let Ok(sql) = std::fs::read_to_string(file) else {
            continue;
        };

        print!("  {name:<24} ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let began = std::time::Instant::now();
        let db = Db::start();
        let mut client = db.client();
        // Statement failures are tolerated: a production dump references
        // extensions and roles a bare server does not have, and the point is
        // to get a large real schema into a database, not to replay it.
        for statement in statements(&sql).into_iter().filter(|s| shapes_the_schema(s)) {
            let _ = client.batch_execute(&statement);
        }
        let _ = client.batch_execute("SET search_path TO public;");
        let ddl = began.elapsed().as_secs();

        // Which tables are worth querying, decided *before* seeding so only
        // those get filled. Seeding every table of GitLab's 1,057 to two
        // thousand rows was the first version of this and it ran for half an
        // hour without finishing — the cost is in the schema's width, and the
        // measurement only needs a handful of tables deep enough that a
        // sequential scan of one is expensive.
        let wanted = lookups(&mut client, 6, false);
        if wanted.is_empty() {
            let line = format!("  {name:<24} ddl {ddl:>3}s · no indexable lookup");
            println!("{line}");
            per_schema.push(line);
            continue;
        }

        // pgseed is the whole reason a baseline means anything, so this
        // measures the pair rather than pgplan alone.
        let mut seeded = true;
        if seeder.exists() {
            let mut args: Vec<String> = vec![
                "--dsn".into(),
                db.url().into(),
                "--apply".into(),
                "--rows".into(),
                "3000".into(),
                "--allow-nonempty".into(),
                "--probe".into(),
            ];
            for look in &wanted {
                args.push("--include".into());
                args.push(look.table.clone());
            }
            seeded = match std::process::Command::new(&seeder)
                .args(&args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => finished_within(child, SEED_BUDGET),
                Err(_) => false,
            };
        }
        if !seeded {
            // Could-not-measure, said out loud. This project's whole argument
            // is that a check which could not run is never a check that
            // passed, and that applies to its own evidence first.
            let line = format!(
                "  {name:<24} ddl {ddl:>3}s · seeding exceeded {SEED_BUDGET}s — not measured"
            );
            println!("{line}");
            per_schema.push(line);
            unmeasured.push(name.clone());
            continue;
        }
        let _ = client.batch_execute("ANALYZE;");
        let seed = began.elapsed().as_secs() - ddl;

        let found = lookups(&mut client, 6, true);
        if found.is_empty() {
            let line = format!("  {name:<24} ddl {ddl:>3}s · seed {seed:>3}s · no rows behind an index");
            println!("{line}");
            per_schema.push(line);
            continue;
        }

        // Baseline every query, then two experiments against it.
        let mut baselined = Vec::new();
        for look in &found {
            let Some(value) = a_value(&mut client, look) else {
                continue;
            };
            let sql = format!(
                "SELECT * FROM {} WHERE {} = {value}",
                quote(&look.table),
                quote(&look.column)
            );
            if let Ok(plan) = plan_of(&mut client, &sql) {
                let shape = Shape::of(&plan);
                // Only queries that actually used the index are evidence: one
                // the planner already scans sequentially cannot regress that
                // way, and counting it would flatter the result.
                if shape.indexes.contains(&look.index) {
                    baselined.push((look, sql, shape));
                }
            }
        }
        if baselined.is_empty() {
            let line = format!("  {name:<24} ddl {ddl:>3}s · seed {seed:>3}s · nothing planned through an index");
            println!("{line}");
            per_schema.push(line);
            continue;
        }

        // Experiment one: change nothing at all. The weakest flap test there
        // is — same session, same statistics — and it is here as a floor.
        // Passing it demonstrates determinism, not usability.
        let mut flapped = 0;
        for (_, sql, before) in &baselined {
            if let Ok(plan) = plan_of(&mut client, sql) {
                if !compare(before, &Shape::of(&plan)).is_empty() {
                    flapped += 1;
                }
            }
        }

        // Experiment two: changes that are *not* worse, each of which lands in
        // ordinary releases. This is the experiment that decides whether the
        // gate survives contact with a real repository — one that fails an
        // innocent migration is deleted from CI within a week, and everything
        // it would have caught afterwards is never caught.
        //
        //   - re-ANALYZE, which moves statistics under a fixed schema
        //   - add a column, the most common migration there is
        //   - add an index on another column: somebody else's optimisation
        //     arriving in the same release
        let mut benign = 0;
        let _ = client.batch_execute("ANALYZE;");
        for (_, sql, before) in &baselined {
            if let Ok(plan) = plan_of(&mut client, sql) {
                if !compare(before, &Shape::of(&plan)).is_empty() {
                    benign += 1;
                }
            }
        }
        for (index, (look, _, _)) in baselined.iter().enumerate() {
            let _ = client.batch_execute(&format!(
                "ALTER TABLE {} ADD COLUMN pgplan_benign_{index} integer",
                quote(&look.table)
            ));
            let _ = client.batch_execute(&format!(
                "CREATE INDEX pgplan_benign_idx_{index} ON {} (pgplan_benign_{index})",
                quote(&look.table)
            ));
        }
        let _ = client.batch_execute("ANALYZE;");
        for (_, sql, before) in &baselined {
            if let Ok(plan) = plan_of(&mut client, sql) {
                if !compare(before, &Shape::of(&plan)).is_empty() {
                    benign += 1;
                }
            }
        }

        // Experiment three: drop the index each query depends on. Split in
        // two, because "the index named in the baseline is absent" becomes
        // true the instant it is dropped and says nothing about whether this
        // tool can recognise a degraded plan. The scan finding is the one with
        // content: it needs the planner to have actually fallen back to
        // reading the table, and the table to be over the row threshold.
        let (mut named_scan, mut named_index, mut missed, mut undropped) = (0, 0, 0, 0);
        for (look, sql, before) in &baselined {
            // Checked, not assumed. A DROP that failed leaves the plan exactly
            // as it was, and scoring that as "pgplan said nothing" would be
            // counting a check that never ran as a check that ran.
            if client
                .batch_execute(&format!("DROP INDEX {}", quote(&look.index)))
                .is_err()
            {
                undropped += 1;
                continue;
            }
            let _ = client.batch_execute("ANALYZE;");
            if let Ok(plan) = plan_of(&mut client, sql) {
                let found = compare(before, &Shape::of(&plan));
                if found
                    .iter()
                    .any(|r| matches!(r, Regression::SequentialScanAppeared { .. }))
                {
                    named_scan += 1;
                } else if !found.is_empty() {
                    named_index += 1;
                } else {
                    missed += 1;
                }
            }
        }

        total.schemas += 1;
        total.candidates += found.len();
        total.queries += baselined.len();
        total.named_the_scan += named_scan;
        total.named_only_the_index += named_index;
        total.missed += missed;
        total.undropped += undropped;
        total.flapped += flapped;
        total.benign += benign;
        let line = format!(
            "  {name:<24} ddl {ddl:>3}s · seed {seed:>3}s · {:>2} q · scan {named_scan:>2} · index-only {named_index:>2} · missed {missed:>2} · undroppable {undropped:>2} · flap {flapped:>2} · benign {benign:>2}",
            baselined.len()
        );
        println!("{line}");
        per_schema.push(line);
    }

    println!("\npgplan against pgseed's corpus\n");
    println!("  {} schema files read\n", files.len());
    for line in &per_schema {
        println!("{line}");
    }
    let dropped = total.named_the_scan + total.named_only_the_index + total.missed;
    println!(
        "\n  {} schemas produced a query. {} candidate lookups had rows behind them, and {} of those were planned through the index — only those are counted below.",
        total.schemas, total.candidates, total.queries
    );
    println!("\n  index dropped, of {dropped}:");
    println!(
        "    a sequential scan was named ....... {} ({:.0}%)",
        total.named_the_scan,
        100.0 * total.named_the_scan as f64 / dropped.max(1) as f64
    );
    println!(
        "    only the missing index was named .. {}",
        total.named_only_the_index
    );
    println!("    nothing was said .................. {}", total.missed);
    if total.undropped > 0 {
        println!(
            "    (and {} indexes could not be dropped, so those never ran)",
            total.undropped
        );
    }
    println!("\n  reported when nothing got worse:");
    println!("    nothing changed at all ........... {}", total.flapped);
    println!(
        "    re-analyzed, column added, index added ... {}",
        total.benign
    );
    if !unmeasured.is_empty() {
        // Named rather than dropped: a survey that silently skips what it
        // could not handle reports the coverage of the easy cases as if it
        // were the coverage of all of them.
        println!(
            "\n  not measured, seeding over budget ... {} ({})",
            unmeasured.len(),
            unmeasured.join(", ")
        );
    }

    // The gate on the gate. A false positive is the failure mode that gets a
    // tool switched off, so it is the one asserted rather than reported.
    assert_eq!(
        total.flapped, 0,
        "the same query against unchanged data reported a regression {} times",
        total.flapped
    );
    assert_eq!(
        total.benign, 0,
        "a change that made nothing worse reported a regression {} times, and          that is the failure which gets a gate switched off",
        total.benign
    );
    assert!(
        total.named_the_scan > 0,
        "not one dropped index produced a sequential-scan finding, so the only          thing measured was that a dropped index has a name"
    );
    assert!(
        total.schemas >= 8,
        "only {} schemas produced a usable query, which is too few to conclude \
         anything from",
        total.schemas
    );
}

#[test]
fn the_statement_splitter_handles_what_a_dump_contains() {
    assert_eq!(statements("SELECT 1; SELECT 2;").len(), 2);
    assert_eq!(statements("SELECT ';';").len(), 1);
    assert!(shapes_the_schema("CREATE TABLE t (id int);"));
    assert!(shapes_the_schema("-- a note\nCREATE INDEX i ON t (id);"));
    assert!(!shapes_the_schema("INSERT INTO t VALUES (1);"));
    assert!(!shapes_the_schema("GRANT ALL ON t TO admin;"));
}
