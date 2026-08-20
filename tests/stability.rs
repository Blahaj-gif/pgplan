//! The property that decides whether anyone leaves this switched on.
//!
//! A gate that fails intermittently gets disabled, and a disabled gate is worse
//! than no gate because it also occupies the slot a working one would have had.
//! So the flapping question gets its own file and its own tests, ahead of any
//! question about how much this detects.
//!
//! `cargo test --test stability -- --test-threads=1`

mod harness;

use harness::{Db, ORDERS};
use pgplan::explain::plan_of;
use pgplan::regress::compare;
use pgplan::shape::Shape;

fn shape(client: &mut postgres::Client, sql: &str) -> Shape {
    Shape::of(&plan_of(client, sql).expect("should plan"))
}

const BY_USER: &str = "SELECT id, total FROM orders WHERE user_id = 42";

#[test]
fn the_same_query_against_the_same_data_gives_the_same_shape() {
    let db = Db::start();
    db.apply(ORDERS);
    let mut client = db.client();

    let first = shape(&mut client, BY_USER);
    let second = shape(&mut client, BY_USER);
    let third = shape(&mut client, BY_USER);

    assert_eq!(first, second, "two runs disagreed");
    assert_eq!(second, third, "three runs disagreed");
}

#[test]
fn a_shape_survives_being_written_and_read_back() {
    let db = Db::start();
    db.apply(ORDERS);
    let taken = shape(&mut db.client(), BY_USER);

    // A baseline is a file. If the round trip is lossy, every check after the
    // first commit compares against something slightly different from what was
    // measured, and the difference will be blamed on the query.
    let written = serde_json::to_string(&taken).expect("serialise");
    let read: Shape = serde_json::from_str(&written).expect("deserialise");
    assert_eq!(taken, read);
    assert!(compare(&taken, &read).is_empty());
}

/// Statistics move. Plans should not, for a query this simple, and if they do
/// the gate has to survive it rather than blame the query.
#[test]
fn re_analysing_the_table_does_not_move_the_verdict() {
    let db = Db::start();
    db.apply(ORDERS);
    let before = shape(&mut db.client(), BY_USER);

    db.apply("ANALYZE orders;");
    let after = shape(&mut db.client(), BY_USER);

    assert!(
        compare(&before, &after).is_empty(),
        "a fresh ANALYZE of unchanged data reported a regression: {:?}",
        compare(&before, &after)
    );
}

/// More rows of the same kind is the ordinary state of a growing application.
/// The plan may legitimately change; what must not happen is a failure with no
/// name attached to it.
#[test]
fn growing_the_table_reports_something_nameable_or_nothing() {
    let db = Db::start();
    db.apply(ORDERS);
    let before = shape(&mut db.client(), BY_USER);

    db.apply(
        "INSERT INTO orders SELECT g, g % 5000, (g % 997)::numeric / 100
         FROM generate_series(50001, 120000) g;
         ANALYZE orders;",
    );
    let after = shape(&mut db.client(), BY_USER);

    for finding in compare(&before, &after) {
        let text = finding.explain();
        assert!(
            text.len() > 40 && !text.contains("{"),
            "every finding must read as a sentence: {text:?}"
        );
    }
}

/// The one a careless implementation gets wrong, and the reason a threshold
/// exists at all: on a small table a sequential scan is the *right* plan, and
/// failing a build for it teaches people to add indexes that make their
/// database slower.
#[test]
fn a_small_table_scanned_sequentially_never_fails_the_build() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE currencies (code text PRIMARY KEY, name text NOT NULL);
         INSERT INTO currencies
         SELECT 'C' || g, 'Currency ' || g FROM generate_series(1, 60) g;
         ANALYZE currencies;",
    );
    let mut client = db.client();

    let by_key = shape(&mut client, "SELECT name FROM currencies WHERE code = 'C7'");
    let scanned = shape(
        &mut client,
        "SELECT name FROM currencies WHERE name = 'Currency 7'",
    );

    assert!(
        compare(&by_key, &scanned).is_empty(),
        "sixty rows is the planner being right: {:?}",
        compare(&by_key, &scanned)
    );
}
