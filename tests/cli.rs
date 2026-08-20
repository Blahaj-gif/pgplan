//! The binary, end to end, including the exit codes CI reads.
//!
//! A gate is its exit code. Everything else is a courtesy to whoever reads the
//! log afterwards, and none of it matters if `check` returns 0 when a plan got
//! worse or 1 when the schema simply moved on.
//!
//! `cargo test --test cli -- --test-threads=1`

mod harness;

use std::process::Command;

use harness::{Db, ORDERS};

fn pgplan() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pgplan"))
}

struct Ran {
    code: i32,
    err: String,
}

fn run(args: &[&str]) -> Ran {
    let out = pgplan().args(args).output().expect("pgplan should run");
    Ran {
        code: out.status.code().unwrap_or(-1),
        err: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn write(dir: &std::path::Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    path.to_string_lossy().to_string()
}

const BY_USER: &str = "-- name: orders by user\nSELECT id, total FROM orders WHERE user_id = 42;\n";

#[test]
fn baseline_then_check_passes_when_nothing_changed() {
    let db = Db::start();
    db.apply(ORDERS);
    let dir = std::env::temp_dir().join("pgplan_pass");
    std::fs::create_dir_all(&dir).unwrap();
    let queries = write(&dir, "q.sql", BY_USER);
    let plans = dir.join("plans.json").to_string_lossy().to_string();

    let wrote = run(&["baseline", "--dsn", db.url(), "-q", &queries, "-o", &plans]);
    assert_eq!(wrote.code, 0, "baseline failed: {}", wrote.err);
    assert!(std::path::Path::new(&plans).exists());

    let checked = run(&["check", "--dsn", db.url(), "-q", &queries, "-b", &plans]);
    assert_eq!(checked.code, 0, "check should pass: {}", checked.err);
    assert!(checked.err.contains("no regression"), "{}", checked.err);
}

#[test]
fn dropping_the_index_exits_one_and_names_it() {
    let db = Db::start();
    db.apply(ORDERS);
    let dir = std::env::temp_dir().join("pgplan_regress");
    std::fs::create_dir_all(&dir).unwrap();
    let queries = write(&dir, "q.sql", BY_USER);
    let plans = dir.join("plans.json").to_string_lossy().to_string();

    run(&["baseline", "--dsn", db.url(), "-q", &queries, "-o", &plans]);
    db.apply("DROP INDEX orders_user_id_idx;");

    let checked = run(&["check", "--dsn", db.url(), "-q", &queries, "-b", &plans]);
    assert_eq!(
        checked.code, 1,
        "a dropped index must fail the build:\n{}",
        checked.err
    );
    assert!(
        checked.err.contains("orders_user_id_idx"),
        "the index should be named:\n{}",
        checked.err
    );
    assert!(
        checked.err.contains("orders by user"),
        "the query should be named:\n{}",
        checked.err
    );
}

/// The exit code that keeps the gate alive. A schema that moved on is not a
/// regression, and saying it is teaches people to ignore this.
#[test]
fn a_changed_schema_exits_two_and_says_to_re_baseline() {
    let db = Db::start();
    db.apply(ORDERS);
    let dir = std::env::temp_dir().join("pgplan_drift");
    std::fs::create_dir_all(&dir).unwrap();
    let queries = write(&dir, "q.sql", BY_USER);
    let plans = dir.join("plans.json").to_string_lossy().to_string();

    run(&["baseline", "--dsn", db.url(), "-q", &queries, "-o", &plans]);
    db.apply("ALTER TABLE orders ADD COLUMN note text;");

    let checked = run(&["check", "--dsn", db.url(), "-q", &queries, "-b", &plans]);
    assert_eq!(
        checked.code, 2,
        "schema drift is could-not-compare, not a regression:\n{}",
        checked.err
    );
    assert!(checked.err.contains("Re-run"), "{}", checked.err);
    assert!(
        checked.err.contains("not a regression"),
        "the difference has to be said out loud:\n{}",
        checked.err
    );
}

#[test]
fn a_remote_looking_host_is_refused_without_the_flag() {
    let ran = run(&[
        "check",
        "--dsn",
        "postgres://user:hunter2@db.internal:5432/app",
        "-q",
        "nothing.sql",
    ]);
    assert_eq!(ran.code, 2);
    assert!(ran.err.contains("--remote"), "{}", ran.err);
    assert!(
        !ran.err.contains("hunter2"),
        "the password must not be printed back:\n{}",
        ran.err
    );
}

#[test]
fn a_missing_baseline_says_which_file() {
    let db = Db::start();
    db.apply(ORDERS);
    let dir = std::env::temp_dir().join("pgplan_missing");
    std::fs::create_dir_all(&dir).unwrap();
    let queries = write(&dir, "q.sql", BY_USER);

    let ran = run(&[
        "check",
        "--dsn",
        db.url(),
        "-q",
        &queries,
        "-b",
        "no-such-file.json",
    ]);
    assert_eq!(ran.code, 2);
    assert!(ran.err.contains("no-such-file.json"), "{}", ran.err);
}

/// A statement that cannot be planned is a broken build, and neither a pass nor
/// a plan that got worse.
#[test]
fn a_query_file_that_does_not_run_exits_two() {
    let db = Db::start();
    db.apply(ORDERS);
    let dir = std::env::temp_dir().join("pgplan_broken");
    std::fs::create_dir_all(&dir).unwrap();
    let queries = write(&dir, "q.sql", "SELECT * FROM no_such_table;\n");
    let plans = dir.join("plans.json").to_string_lossy().to_string();

    let ran = run(&["baseline", "--dsn", db.url(), "-q", &queries, "-o", &plans]);
    assert_eq!(ran.code, 2, "{}", ran.err);
    assert!(ran.err.contains("no_such_table"), "{}", ran.err);
}

/// Adding a query is an ordinary thing to do, and must not fail the build for
/// the queries that were already there.
#[test]
fn a_query_missing_from_the_baseline_is_reported_not_failed() {
    let db = Db::start();
    db.apply(ORDERS);
    let dir = std::env::temp_dir().join("pgplan_new_query");
    std::fs::create_dir_all(&dir).unwrap();
    let queries = write(&dir, "q.sql", BY_USER);
    let plans = dir.join("plans.json").to_string_lossy().to_string();

    run(&["baseline", "--dsn", db.url(), "-q", &queries, "-o", &plans]);
    let more = write(
        &dir,
        "q.sql",
        &format!("{BY_USER}-- name: totals\nSELECT sum(total) FROM orders;\n"),
    );

    let checked = run(&["check", "--dsn", db.url(), "-q", &more, "-b", &plans]);
    assert_eq!(checked.code, 0, "{}", checked.err);
    assert!(checked.err.contains("totals"), "{}", checked.err);
    assert!(
        checked.err.contains("not in the baseline"),
        "{}",
        checked.err
    );
}
