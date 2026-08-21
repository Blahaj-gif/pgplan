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
const SEED_BUDGET: u64 = 600;

use std::collections::BTreeSet;
use std::path::PathBuf;

use harness::Db;
use pgplan::explain::plan_of;
use pgplan::regress::{compare, Regression, SEQ_SCAN_ROWS};
use pgplan::shape::{Access, Shape};

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

/// Schemas a set of statements writes into, so they can be created first.
///
/// A dump that says `CREATE TABLE hdb_catalog.hdb_version (...)` fails outright
/// against a bare server, and the harness tolerates statement failures by
/// design — so the schema simply came out empty and was reported as offering no
/// indexable lookup. It offered eight tables. Hasura and Zitadel were both lost
/// this way, and neither had anything wrong with it.
fn schemas_written_to(statements: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for statement in statements {
        let head = statement.trim_start();
        let upper = head.to_uppercase();
        let rest = if let Some(rest) = upper.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
            &head[head.len() - rest.len()..]
        } else if let Some(rest) = upper.strip_prefix("CREATE TABLE ") {
            &head[head.len() - rest.len()..]
        } else {
            continue;
        };
        // Scanned rather than split, because a quoted identifier may contain
        // the space or the dot that a naive split would stop on, and a schema
        // missed here is a schema that comes out empty and gets reported as
        // offering nothing.
        let mut qualifier = String::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut qualified = false;
        for c in rest.chars() {
            match c {
                '"' => quoted = !quoted,
                '.' if !quoted && !qualified => {
                    qualifier = std::mem::take(&mut current);
                    qualified = true;
                }
                c if !quoted && (c.is_whitespace() || c == '(') => break,
                c => current.push(c),
            }
        }
        if !qualified || qualifier.is_empty() || qualifier.eq_ignore_ascii_case("public") {
            continue;
        }
        out.insert(qualifier);
    }
    out
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
        // Per occurrence, not per line. A function written on one line — `AS $$
        // BEGIN ... END; $$;` — carries two, and toggling once left the parser
        // convinced it was inside a body for the rest of the file. Everything
        // after it accumulated into a single statement that started with
        // neither CREATE nor ALTER and was filtered out on that basis, so Lago
        // contributed all 139 of its tables to nothing and was reported as a
        // schema with no indexable lookup. A parse failure wearing the costume
        // of a measurement.
        if line.matches("$$").count() % 2 == 1 {
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

/// Two tables a foreign key relates, and whether anybody indexed the key.
///
/// The last field is the point. Postgres creates an index for a primary key
/// and none at all for a foreign key, so the referencing side of most
/// relationships in most schemas is unindexed — and a nested loop that lands
/// on that side reads the whole table once per outer row. The pairs without an
/// index are therefore the interesting ones, and they are preferred.
struct Pair {
    child: String,
    key: String,
    parent: String,
    referenced: String,
    indexed: bool,
}

fn related(client: &mut postgres::Client, limit: usize, needs_rows: bool) -> Vec<Pair> {
    let rows = client
        .query(
            "SELECT c.relname, a.attname, p.relname, pa.attname,
                    EXISTS (SELECT 1 FROM pg_index x
                            WHERE x.indrelid = c.oid AND x.indkey[0] = a.attnum)
             FROM pg_constraint k
             JOIN pg_class c ON c.oid = k.conrelid
             JOIN pg_class p ON p.oid = k.confrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_attribute a  ON a.attrelid = c.oid AND a.attnum = k.conkey[1]
             JOIN pg_attribute pa ON pa.attrelid = p.oid AND pa.attnum = k.confkey[1]
             WHERE k.contype = 'f'
               AND array_length(k.conkey, 1) = 1
               AND n.nspname NOT IN ('pg_catalog','information_schema')
               AND c.relkind = 'r' AND p.relkind = 'r'
               AND c.oid <> p.oid
               -- Both sides have to have been seeded. A join against an empty
               -- table plans as nothing and measures nothing. Asked without
               -- that clause before seeding, to decide what is worth seeding.
               AND (NOT $1 OR (c.reltuples > 500 AND p.reltuples > 500))
             ORDER BY 5, p.reltuples DESC, c.relname",
            &[&needs_rows],
        )
        .unwrap_or_default();

    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for row in &rows {
        let child: String = row.get(0);
        let parent: String = row.get(2);
        if !seen.insert((child.clone(), parent.clone())) {
            continue;
        }
        out.push(Pair {
            child,
            key: row.get(1),
            parent,
            referenced: row.get(3),
            indexed: row.get(4),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
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
    /// The index was dropped, nothing was reported, and the plan had moved to
    /// another index while reading no more than before. A correct silence, and
    /// it has to be counted apart from `missed` or removing a false positive
    /// reads as losing a detection.
    dropped_swapped: usize,
    /// An ORDER BY that the index used to supply, with the index gone. Counts
    /// the schemas where a `Sort` was named for it.
    sort_tried: usize,
    sort_named: usize,
    /// The same row, fetched through a predicate the index cannot serve.
    /// Nothing is dropped: this is the innocent-looking ORM change.
    wrapped_tried: usize,
    wrapped_named: usize,
    /// Schemas that contributed at least one foreign-key pair, whether or not
    /// they also offered a lookup. Reported beside `schemas` because widening
    /// the survey to joins is worth nothing if it is not visible how much it
    /// widened it.
    schemas_joined: usize,
    /// Foreign-key pairs found with rows on both sides — the denominator for
    /// the three join experiments below.
    pairs: usize,
    /// Pairs whose join would not plan at all, and experiments that errored or
    /// ran past the statement timeout.
    ///
    /// Kept apart from "did not bite" because they are not the same thing and
    /// were being counted as though they were. A planner declining to produce a
    /// shape is a fact about the planner; a query that never finished is a fact
    /// about this machine, and reading the second as the first is how a number
    /// moves between two runs of identical code with nothing to explain it.
    pairs_unplanned: usize,
    joins_could_not_run: usize,
    /// Of those, the ones whose referencing column nobody indexed. Reported
    /// because the claim that this shape is common rests on it.
    pairs_unindexed: usize,
    /// A join forced onto a nested loop. `bit` counts the times the planner
    /// actually put a sequential scan on the inner side; only those could
    /// possibly be detected, and only those are a denominator. An experiment
    /// that did not bite is not a tool that missed something.
    loop_tried: usize,
    loop_bit: usize,
    loop_named: usize,
    /// Of the bites, the ones on a pair whose referencing column somebody had
    /// indexed. Expected to be low: an indexed key gives the loop an index scan
    /// on its inner side, and then there is nothing to find. Counted so the
    /// claim that the silences are *correct* silences rests on a number.
    loop_bit_indexed: usize,
    /// The same join with work_mem at the floor. `bit` counts the times the
    /// hash join really did start spilling; the planner is entitled to choose
    /// a different join instead, and that is not a miss either.
    spill_tried: usize,
    spill_bit: usize,
    spill_named: usize,
    /// A count through an index that was then dropped: one row returned either
    /// way, and the whole table read to produce it the second time.
    ///
    /// `scanned` is the bite measure, and it is here because the first version
    /// of this experiment did not have one. Dropping an index does not oblige
    /// the planner to start reading the table — it may reach it another way, or
    /// the value may be common enough that it was reading most of it already.
    /// Only the times it genuinely fell back to a sequential scan belong in the
    /// denominator, and even then silence below the two-fold threshold is the
    /// rule working rather than failing.
    ratio_tried: usize,
    ratio_scanned: usize,
    ratio_named: usize,
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
        let shaping: Vec<String> = statements(&sql)
            .into_iter()
            .filter(|s| shapes_the_schema(s))
            .collect();
        for schema in schemas_written_to(&shaping) {
            let _ =
                client.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", quote(&schema)));
        }
        for statement in &shaping {
            let _ = client.batch_execute(statement);
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
        // And the tables a join needs, asked for before seeding for the same
        // reason. Three of the six named regressions are about joins, and a
        // schema can be full of foreign keys while offering no lookup index at
        // all — Kong, PostgREST and Vaultwarden have eighty-nine pairs between
        // them and not one qualifying lookup. Seeding only what a lookup needed
        // meant those schemas were never filled, so their joins were never
        // planned, and the survey called them schemas with nothing to offer.
        let pairs_wanted = related(&mut client, 2, false);
        if wanted.is_empty() && pairs_wanted.is_empty() {
            let line =
                format!("  {name:<24} ddl {ddl:>3}s · no indexable lookup and no related pair");
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
            let mut fill: BTreeSet<String> = wanted.iter().map(|l| l.table.clone()).collect();
            for pair in &pairs_wanted {
                // Both sides named explicitly. pgseed widens to a child's
                // ancestors on its own, but saying so costs nothing and does
                // not rely on that staying true.
                fill.insert(pair.child.clone());
                fill.insert(pair.parent.clone());
            }
            for table in &fill {
                args.push("--include".into());
                args.push(table.clone());
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

        // Experiments five and six need two tables that are actually related,
        // because three of the six named regressions are about joins and a
        // single-table lookup cannot provoke any of them. Postgres does not
        // index a foreign key, so the referencing side is usually unindexed —
        // which is the whole reason the shape below is a real incident rather
        // than a curiosity.
        let pairs = related(&mut client, 2, true);
        let (mut loop_tried, mut loop_bit, mut loop_named) = (0, 0, 0);
        let (mut pairs_unplanned, mut joins_could_not_run) = (0, 0);
        let mut loop_bit_indexed = 0;
        let (mut spill_tried, mut spill_bit, mut spill_named) = (0, 0, 0);
        // A join that goes quadratic can take a long time to *run*, and
        // EXPLAIN ANALYZE runs it. One schema stalling must not cost the rest.
        let _ = client.batch_execute("SET statement_timeout = '60s'");
        for pair in &pairs {
            // The parent on the left, so the child is the nullable side and
            // the planner has to put it on the inside of any loop it chooses.
            // Without that it is free to swap them, and on the schemas here it
            // does: it put the child on the outside and memoized a primary-key
            // lookup on the inside, which is the planner being right.
            let join = format!(
                "SELECT count(*) FROM {} p LEFT JOIN {} c ON c.{} = p.{}",
                quote(&pair.parent),
                quote(&pair.child),
                quote(&pair.key),
                quote(&pair.referenced)
            );
            let Ok(plan) = plan_of(&mut client, &join) else {
                pairs_unplanned += 1;
                continue;
            };
            let before = Shape::of(&plan);

            // Experiment five: the same join, planned as a loop. The row
            // estimate is not what is manipulated — inducing a bad estimate on
            // twenty-four unfamiliar schemas is its own project — so the join
            // methods are switched off, which arrives at the same plan by a
            // different route. What is measured is whether the shape is
            // recognised when it appears, not how often it appears.
            loop_tried += 1;
            let _ = client.batch_execute("SET enable_hashjoin = off; SET enable_mergejoin = off;");
            let looped = plan_of(&mut client, &join);
            if looped.is_err() {
                joins_could_not_run += 1;
            }
            if let Ok(plan) = looped {
                let after = Shape::of(&plan);
                // The worst inner scan in the plan, against the same row floor
                // the rule uses. Counting a bite the rule is *right* to ignore
                // would make this harness fail the build for pgplan being
                // correct on a small table — which is the exact failure this
                // whole project is built to avoid, arriving from inside its own
                // evidence. Borrowed from the crate rather than restated, so
                // the two cannot drift apart.
                let worst = after
                    .inner_loop_rows
                    .values()
                    .copied()
                    .fold(0.0_f64, f64::max);
                if worst < SEQ_SCAN_ROWS {
                    // Either the planner found something better even with two
                    // join methods gone, or the inner side is small enough that
                    // scanning it is the right answer. Nothing to detect.
                } else {
                    loop_bit += 1;
                    if pair.indexed {
                        loop_bit_indexed += 1;
                    }
                    if compare(&before, &after)
                        .iter()
                        .any(|r| matches!(r, Regression::NestedLoopOverSequentialScan { .. }))
                    {
                        loop_named += 1;
                    }
                }
            }
            let _ = client.batch_execute("RESET enable_hashjoin; RESET enable_mergejoin;");

            // Experiment six: the same join with work_mem at the floor, which
            // is what a busy machine and a grown table look like together.
            spill_tried += 1;
            let _ = client.batch_execute("SET work_mem = '64kB'");
            let cramped = plan_of(&mut client, &join);
            if cramped.is_err() {
                joins_could_not_run += 1;
            }
            if let Ok(plan) = cramped {
                let after = Shape::of(&plan);
                // Mirrors the rule, including its guard that the baseline
                // actually had a hash join to fit in memory. A bite measure
                // that outlives the rule it measures is not a bite measure.
                if before.nodes.contains("Hash Join")
                    && before.max_batches <= 1
                    && after.max_batches > 1
                {
                    spill_bit += 1;
                    if compare(&before, &after)
                        .iter()
                        .any(|r| matches!(r, Regression::HashJoinSpilled { .. }))
                    {
                        spill_named += 1;
                    }
                }
            }
            let _ = client.batch_execute("RESET work_mem");
        }
        let _ = client.batch_execute("RESET statement_timeout");

        total.pairs += pairs.len();
        total.pairs_unplanned += pairs_unplanned;
        total.joins_could_not_run += joins_could_not_run;
        total.pairs_unindexed += pairs.iter().filter(|pair| !pair.indexed).count();
        total.loop_tried += loop_tried;
        total.loop_bit += loop_bit;
        total.loop_named += loop_named;
        total.loop_bit_indexed += loop_bit_indexed;
        total.spill_tried += spill_tried;
        total.spill_bit += spill_bit;
        total.spill_named += spill_named;
        if !pairs.is_empty() {
            total.schemas_joined += 1;
        }
        // One phrase for the per-schema line, so a schema that contributed
        // only joins still says what it contributed.
        let joins = if pairs.is_empty() {
            "fk 0".to_string()
        } else {
            format!(
                "fk {} loop {loop_named}/{loop_bit} spill {spill_named}/{spill_bit}",
                pairs.len()
            )
        };

        let found = lookups(&mut client, 6, true);
        if found.is_empty() {
            let line = format!(
                "  {name:<24} ddl {ddl:>3}s · seed {seed:>3}s · no rows behind an index · {joins}"
            );
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
            let line = format!(
                "  {name:<24} ddl {ddl:>3}s · seed {seed:>3}s · nothing planned through an index · {joins}"
            );
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

        // Experiment three: wrap the indexed column so the planner cannot see
        // through it, which is what an ORM upgrade does by accident. Nothing
        // is dropped and the answer is identical, so a test that checks
        // results cannot see it at all. This is the only one of the six named
        // regressions that needs no schema change to provoke.
        let (mut wrapped_tried, mut wrapped_named) = (0, 0);
        for (look, _, before) in &baselined {
            let Some(value) = a_value(&mut client, look) else {
                continue;
            };
            // Concatenation rather than a bare `::text`: on a text column the
            // cast is identity, the planner sees straight through it, and the
            // index is still used — so reporting nothing there was correct and
            // counting it as a miss understated this. Appending an empty
            // string defeats the index for every column type in the candidate
            // set, which is what makes the denominator mean something.
            let sql = format!(
                "SELECT * FROM {} WHERE {}::text || '' = {value}::text",
                quote(&look.table),
                quote(&look.column)
            );
            if let Ok(plan) = plan_of(&mut client, &sql) {
                wrapped_tried += 1;
                if !compare(before, &Shape::of(&plan)).is_empty() {
                    wrapped_named += 1;
                }
            }
        }

        // Experiment four: an ORDER BY the index was supplying. Recorded now,
        // judged after the drop below, because the drop is what takes the
        // ordering away. Only queries that started without a Sort are kept: if
        // the planner was already sorting, a Sort appearing is not news.
        let mut ordered = Vec::new();
        for (look, _, _) in &baselined {
            let sql = format!(
                "SELECT * FROM {} ORDER BY {} LIMIT 50",
                quote(&look.table),
                quote(&look.column)
            );
            if let Ok(plan) = plan_of(&mut client, &sql) {
                let shape = Shape::of(&plan);
                if shape.indexes.contains(&look.index) && shape.sorts.is_empty() {
                    ordered.push((sql, shape));
                }
            }
        }

        // Experiment seven, baselined here and judged after the drop below: a
        // count through the index. One row comes back whichever way it is
        // planned, so the answer is unchanged and only the work moves — which
        // is exactly what the rows-per-row-returned rule is for. Separated from
        // the wrapped experiment on purpose: folded together, a finding of any
        // kind was being read as evidence for this one.
        let mut counted = std::collections::BTreeMap::new();
        for (look, _, _) in &baselined {
            let Some(value) = a_value(&mut client, look) else {
                continue;
            };
            let sql = format!(
                "SELECT count(*) FROM {} WHERE {} = {value}",
                quote(&look.table),
                quote(&look.column)
            );
            if let Ok(plan) = plan_of(&mut client, &sql) {
                let shape = Shape::of(&plan);
                if shape.indexes.contains(&look.index) {
                    counted.insert(look.index.clone(), (sql, shape));
                }
            }
        }
        let (mut ratio_tried, mut ratio_scanned, mut ratio_named) = (0, 0, 0);

        // Experiment eight: drop the index each query depends on. Split in
        // two, because "the index named in the baseline is absent" becomes
        // true the instant it is dropped and says nothing about whether this
        // tool can recognise a degraded plan. The scan finding is the one with
        // content: it needs the planner to have actually fallen back to
        // reading the table, and the table to be over the row threshold.
        let (mut named_scan, mut named_index, mut missed, mut undropped) = (0, 0, 0, 0);
        let mut dropped_swapped = 0;
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
                let after = Shape::of(&plan);
                let found = compare(before, &after);
                if found
                    .iter()
                    .any(|r| matches!(r, Regression::SequentialScanAppeared { .. }))
                {
                    named_scan += 1;
                } else if !found.is_empty() {
                    named_index += 1;
                } else if after.indexes.difference(&before.indexes).next().is_some()
                    && after.rows_scanned <= before.rows_scanned
                {
                    // The index went and the planner moved to another one
                    // without reading any more. Nothing got worse, so saying
                    // nothing is right, and it is not a miss.
                    dropped_swapped += 1;
                } else {
                    missed += 1;
                }
            }
            // The same index, gone, judged for the ratio rule alone.
            if let Some((count_sql, count_before)) = counted.get(&look.index) {
                if let Ok(plan) = plan_of(&mut client, count_sql) {
                    let count_after = Shape::of(&plan);
                    ratio_tried += 1;
                    // Did the experiment bite? Only if the count now reads the
                    // table where it used to reach it through the index.
                    if count_before.access.get(&look.table) == Some(&Access::Indexed)
                        && count_after.access.get(&look.table) == Some(&Access::Sequential)
                    {
                        ratio_scanned += 1;
                    }
                    if compare(count_before, &count_after)
                        .iter()
                        .any(|r| matches!(r, Regression::MoreRowsPerRowReturned { .. }))
                    {
                        ratio_named += 1;
                    }
                }
            }
        }

        // The ordering queries, now that their index is gone.
        let sort_tried = ordered.len();
        let mut sort_named = 0;
        for (sql, before) in &ordered {
            if let Ok(plan) = plan_of(&mut client, sql) {
                if compare(before, &Shape::of(&plan))
                    .iter()
                    .any(|r| matches!(r, Regression::SortAppeared { .. }))
                {
                    sort_named += 1;
                }
            }
        }

        total.schemas += 1;
        total.candidates += found.len();
        total.sort_tried += sort_tried;
        total.sort_named += sort_named;
        total.wrapped_tried += wrapped_tried;
        total.wrapped_named += wrapped_named;
        total.ratio_tried += ratio_tried;
        total.ratio_scanned += ratio_scanned;
        total.ratio_named += ratio_named;
        total.queries += baselined.len();
        total.named_the_scan += named_scan;
        total.named_only_the_index += named_index;
        total.missed += missed;
        total.dropped_swapped += dropped_swapped;
        total.undropped += undropped;
        total.flapped += flapped;
        total.benign += benign;
        let line = format!(
            "  {name:<24} ddl {ddl:>3}s · seed {seed:>3}s · {:>2} q · scan {named_scan:>2} · index-only {named_index:>2} · swapped {dropped_swapped:>2} · missed {missed:>2} · undroppable {undropped:>2} · flap {flapped:>2} · benign {benign:>2} · {joins} ratio {ratio_named}/{ratio_scanned}",
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
        "\n  {} schemas produced a query and {} produced a join. {} candidate lookups had rows behind them, and {} of those were planned through the index — only those are counted below.",
        total.schemas, total.schemas_joined, total.candidates, total.queries
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
    println!(
        "    moved to another index, no more read {}  (correct silence)",
        total.dropped_swapped
    );
    println!("    nothing was said .................. {}", total.missed);
    if total.undropped > 0 {
        println!(
            "    (and {} indexes could not be dropped, so those never ran)",
            total.undropped
        );
    }
    println!(
        "\n  indexed column wrapped so the planner cannot use it, of {}:",
        total.wrapped_tried
    );
    println!(
        "    named ............................. {}",
        total.wrapped_named
    );
    println!(
        "\n  ORDER BY that an index was supplying, index dropped, of {}:",
        total.sort_tried
    );
    println!(
        "    a sort was named .................. {}",
        total.sort_named
    );
    println!(
        "\n  joins, from {} foreign-key pairs with rows on both sides ({} with no index on the referencing column):",
        total.pairs, total.pairs_unindexed
    );
    println!(
        "    forced onto a nested loop ......... {} tried, {} put a sequential scan on the inner side ({} of those on an indexed key), {} named",
        total.loop_tried, total.loop_bit, total.loop_bit_indexed, total.loop_named
    );
    println!(
        "    work_mem at the floor ............. {} tried, {} actually spilled, {} named",
        total.spill_tried, total.spill_bit, total.spill_named
    );
    if total.pairs_unplanned > 0 || total.joins_could_not_run > 0 {
        println!(
            "    could not be run at all ........... {} pairs whose join would not plan, {} experiments that errored or timed out",
            total.pairs_unplanned, total.joins_could_not_run
        );
    }
    println!(
        "\n  a count through an index that was then dropped, of {} tried:",
        total.ratio_tried
    );
    println!(
        "    the count fell back to a scan ..... {}",
        total.ratio_scanned
    );
    println!(
        "    read far more for the same answer . {}",
        total.ratio_named
    );
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
    // Only the experiments that bit are asserted on. One that did not bite is
    // not evidence in either direction, and treating it as a miss is the
    // mistake this survey has already made twice in other clothes.
    //
    // What these two are worth, stated rather than implied. Both bite measures
    // necessarily share a condition with the rule they check — a spill *is*
    // batches going from one to more than one — so neither is a strong test of
    // the rule's definition. What they are strong evidence of is that the shape
    // occurs on schemas nobody here designed, that the tool names it every time
    // it occurs, and, in the loop's case, that it stays quiet on the pairs
    // where somebody had indexed the foreign key. The silences are the half
    // that is not near-circular, and they are the half that decides whether a
    // gate survives.
    assert_eq!(
        total.loop_bit,
        total.loop_named,
        "a sequential scan on the inner side of a nested loop went unreported \
         {} times",
        total.loop_bit - total.loop_named
    );
    assert_eq!(
        total.spill_bit,
        total.spill_named,
        "a hash join started spilling and was not reported {} times",
        total.spill_bit - total.spill_named
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
    // A function body opened and closed on one line. Toggling once per line
    // left the parser inside a body forever and swallowed the rest of the file.
    let one_line = "CREATE FUNCTION f() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$;\n\
                    CREATE TABLE t (id int);\n\
                    CREATE TABLE u (id int);";
    let split = statements(one_line);
    assert_eq!(
        split.iter().filter(|s| shapes_the_schema(s)).count(),
        2,
        "both tables should survive a one-line function body: {split:?}"
    );
    // And a body genuinely spanning lines still holds together.
    let spanning = "CREATE FUNCTION g() RETURNS trigger AS $$\nBEGIN\n  RETURN NEW;\nEND;\n$$;\n\
                    CREATE TABLE v (id int);";
    assert_eq!(
        statements(spanning)
            .iter()
            .filter(|s| shapes_the_schema(s))
            .count(),
        1
    );
    assert!(shapes_the_schema("CREATE TABLE t (id int);"));
    assert!(shapes_the_schema("-- a note\nCREATE INDEX i ON t (id);"));
    assert!(!shapes_the_schema("INSERT INTO t VALUES (1);"));
    assert!(!shapes_the_schema("GRANT ALL ON t TO admin;"));
}

#[test]
fn the_schemas_a_dump_writes_into_are_found() {
    let found = schemas_written_to(&[
        "CREATE TABLE hdb_catalog.hdb_version (id int);".to_string(),
        "CREATE TABLE IF NOT EXISTS zitadel.instances(id text);".to_string(),
        "CREATE TABLE public.plain (id int);".to_string(),
        "CREATE TABLE unqualified (id int);".to_string(),
        r#"CREATE TABLE "Mixed Case"."t" (id int);"#.to_string(),
    ]);
    assert!(found.contains("hdb_catalog"));
    assert!(found.contains("zitadel"));
    assert!(found.contains("Mixed Case"));
    assert!(!found.contains("public"), "public always exists");
    assert_eq!(
        found.len(),
        3,
        "unqualified names name no schema: {found:?}"
    );
}
