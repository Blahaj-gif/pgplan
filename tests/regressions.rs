//! Every named regression, provoked against a real planner and caught.
//!
//! Each test does the thing a careless migration does — drops an index, wraps a
//! column in a function, shrinks `work_mem` — and asserts that the named
//! finding comes out. A closed set whose members were only ever tested against
//! hand-written JSON would be a set of opinions about what Postgres does.
//!
//! `cargo test --test regressions -- --test-threads=1`

mod harness;

use harness::{Db, ORDERS};
use pgplan::explain::plan_of;
use pgplan::regress::{compare, Regression};
use pgplan::shape::Shape;

fn shape(client: &mut postgres::Client, sql: &str) -> Shape {
    Shape::of(&plan_of(client, sql).expect("the query should plan"))
}

const BY_USER: &str = "SELECT id, total FROM orders WHERE user_id = 42";

#[test]
fn dropping_the_index_a_query_needs_is_caught() {
    let db = Db::start();
    db.apply(ORDERS);
    let mut client = db.client();

    let before = shape(&mut client, BY_USER);
    assert_eq!(
        before.access.get("orders"),
        Some(&pgplan::shape::Access::Indexed),
        "the fixture must start out using the index, or this proves nothing"
    );

    // The careless migration.
    db.apply("DROP INDEX orders_user_id_idx;");
    let after = shape(&mut db.client(), BY_USER);

    let found = compare(&before, &after);
    assert!(
        found.iter().any(|r| matches!(
            r, Regression::SequentialScanAppeared { table, .. } if table == "orders")),
        "expected a sequential scan finding, got {found:?}"
    );
    assert!(
        found.iter().any(|r| matches!(
            r, Regression::IndexNoLongerUsed { index } if index == "orders_user_id_idx")),
        "and the index should be named: {found:?}"
    );
}

#[test]
fn wrapping_an_indexed_column_so_the_planner_cannot_use_it_is_caught() {
    let db = Db::start();
    db.apply(ORDERS);
    let mut client = db.client();

    // Nothing was dropped. The query changed shape in a way that hides the
    // column from the index — the failure mode of an innocent-looking ORM
    // change, and invisible in a test that only checks results.
    let before = shape(&mut client, BY_USER);
    let after = shape(
        &mut client,
        "SELECT id, total FROM orders WHERE user_id::text = '42'",
    );

    let found = compare(&before, &after);
    assert!(
        !found.is_empty(),
        "a predicate the index cannot serve should be caught, got nothing"
    );
}

#[test]
fn a_hash_join_pushed_into_spilling_is_caught() {
    let db = Db::start();
    db.apply(ORDERS);
    db.apply(
        "CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL);
         INSERT INTO users SELECT g, 'user ' || g FROM generate_series(1, 5000) g;
         ANALYZE users;",
    );
    let join = "SELECT u.name, count(*) FROM users u \
                JOIN orders o ON o.user_id = u.id GROUP BY u.name";

    let mut roomy = db.client();
    roomy.batch_execute("SET work_mem = '64MB';").unwrap();
    let before = shape(&mut roomy, join);

    let mut cramped = db.client();
    cramped.batch_execute("SET work_mem = '64kB';").unwrap();
    let after = shape(&mut cramped, join);

    let found = compare(&before, &after);
    if before.max_batches <= 1 && after.max_batches > 1 {
        assert!(
            found
                .iter()
                .any(|r| matches!(r, Regression::HashJoinSpilled { .. })),
            "batches went {} -> {} and nothing was reported: {found:?}",
            before.max_batches,
            after.max_batches
        );
    } else {
        // The planner is entitled to choose a different join entirely at 64kB.
        // Asserting it must spill would be asserting a planner decision rather
        // than testing this tool, so the honest move is to say so and stop.
        eprintln!(
            "planner did not spill ({} -> {} batches); nothing to assert here",
            before.max_batches, after.max_batches
        );
    }
}

#[test]
fn a_query_reading_far_more_for_the_same_answer_is_caught() {
    let db = Db::start();
    db.apply(ORDERS);
    let mut client = db.client();

    // One row, found through the primary key.
    let before = shape(&mut client, "SELECT total FROM orders WHERE id = 500");
    // One row, found by reading the table. Same answer, fifty thousand times
    // the work, and no index is involved on either side of the ratio.
    let after = shape(
        &mut client,
        "SELECT total FROM orders WHERE total::text = (SELECT total::text FROM orders WHERE id = 500) LIMIT 1",
    );

    let found = compare(&before, &after);
    assert!(
        !found.is_empty(),
        "reading the whole table for one row should be caught: \
         before {:?} after {:?}",
        before.amplification(),
        after.amplification()
    );
}

#[test]
fn an_unchanged_schema_and_query_report_nothing() {
    let db = Db::start();
    db.apply(ORDERS);
    let mut client = db.client();
    let before = shape(&mut client, BY_USER);
    let after = shape(&mut client, BY_USER);
    assert!(
        compare(&before, &after).is_empty(),
        "nothing changed, so nothing may be reported"
    );
}

#[test]
fn adding_an_index_is_never_a_regression() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE t (id int, v int);
         INSERT INTO t SELECT g, g % 100 FROM generate_series(1, 50000) g;
         ANALYZE t;",
    );
    let mut client = db.client();
    let query = "SELECT * FROM t WHERE v = 7";

    let before = shape(&mut client, query);
    db.apply("CREATE INDEX t_v_idx ON t (v); ANALYZE t;");
    let after = shape(&mut db.client(), query);

    assert!(
        compare(&before, &after).is_empty(),
        "making a query faster must never fail a build"
    );
}

#[test]
fn a_statement_that_does_not_run_says_what_the_database_said() {
    let db = Db::start();
    let mut client = db.client();
    let failed = plan_of(&mut client, "SELECT * FROM no_such_table").unwrap_err();
    let message = failed.to_string();
    assert!(
        message.contains("no_such_table"),
        "the database's own message is the useful part, got: {message}"
    );
}

/// `ANALYZE` executes the statement. If that were not rolled back, this tool
/// would be changing the database it is measuring.
#[test]
fn planning_a_write_leaves_nothing_behind() {
    let db = Db::start();
    db.apply("CREATE TABLE audit (id int);");
    let mut client = db.client();

    plan_of(&mut client, "INSERT INTO audit VALUES (1)").expect("should plan");

    let count: i64 = client
        .query_one("SELECT count(*) FROM audit", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "EXPLAIN ANALYZE ran the insert and kept it");
}
