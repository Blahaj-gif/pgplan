//! The closed set of degradations this is willing to fail a build for.
//!
//! Not "the plan changed". A plan changes for a dozen innocent reasons — a
//! planner upgrade, a statistics refresh, one more row crossing a threshold,
//! or the query genuinely getting *better*. Failing on change means failing on
//! all of those, and a gate that fails on improvement is one nobody trusts
//! twice.
//!
//! So there is a list, each entry provable from two shapes and a threshold, and
//! anything outside it is reported without failing. Same discipline as the
//! closed set of CHECK shapes next door, and for the same reason: a partial
//! rule that silently guesses at the rest is worse than a narrow rule that says
//! what it does not cover.

use crate::shape::{Access, Shape};

/// How much work a table has to be doing before a sequential scan of it counts
/// as a regression rather than the planner being sensible.
///
/// Below this, a sequential scan is usually *correct*: reading two hundred rows
/// beats descending an index to fetch them one at a time, and Postgres knows
/// that. Failing a build for it would be teaching people to add indexes that
/// make their database slower.
pub const SEQ_SCAN_ROWS: f64 = 1_000.0;

/// How much worse the rows-read-per-row-returned ratio has to get.
///
/// Two-fold rather than any drift, because this number moves a little with the
/// data and a gate on noise is not a gate.
pub const AMPLIFICATION_FACTOR: f64 = 2.0;

/// A named, provable degradation.
#[derive(Debug, Clone, PartialEq)]
pub enum Regression {
    SequentialScanAppeared { table: String, rows: f64 },
    IndexNoLongerUsed { index: String },
    NestedLoopOverSequentialScan { table: String, rows: f64 },
    MoreRowsPerRowReturned { before: f64, after: f64 },
    SortAppeared { keys: Vec<String> },
    HashJoinSpilled { batches: i64 },
}

impl Regression {
    /// One line naming what got worse and what to do about it.
    pub fn explain(&self) -> String {
        match self {
            Regression::SequentialScanAppeared { table, rows } => format!(
                "\"{table}\" is now read sequentially — {rows:.0} rows, where the baseline \
                 reached it through an index. An index it was using has gone, or a change \
                 stopped the planner being able to use one."
            ),
            Regression::IndexNoLongerUsed { index } => format!(
                "the index \"{index}\" is no longer used by this query. It may have been \
                 dropped, or a column in the predicate may now be wrapped in something the \
                 planner cannot see through."
            ),
            Regression::NestedLoopOverSequentialScan { table, rows } => format!(
                "a nested loop now scans \"{table}\" sequentially on its inner side, {rows:.0} \
                 rows across all iterations. This is the shape that is fast on ten rows and \
                 takes a site down on a million."
            ),
            Regression::MoreRowsPerRowReturned { before, after } => format!(
                "this reads {after:.0} rows for every row it returns, against {before:.0} in \
                 the baseline. The query is doing more work for the same answer."
            ),
            Regression::SortAppeared { keys } => format!(
                "a sort on ({}) appeared where the baseline had none — an index was supplying \
                 that order and no longer is.",
                keys.join(", ")
            ),
            Regression::HashJoinSpilled { batches } => format!(
                "a hash join now spills to disk in {batches} batches. It fitted in memory in \
                 the baseline, so either the input grew or work_mem shrank."
            ),
        }
    }
}

/// Everything that got worse between two shapes of the same query.
///
/// Order matters only for reading: the scan-level findings come first because
/// they are usually the cause of the ratio findings below them.
pub fn compare(before: &Shape, after: &Shape) -> Vec<Regression> {
    let mut found = Vec::new();

    // A table that was reached through an index and now is not. The row
    // threshold is what keeps this from firing on small tables where a
    // sequential scan is the right answer.
    for (table, how) in &after.access {
        let was = before.access.get(table);
        let rows = after.sequential_rows.get(table).copied().unwrap_or(0.0);
        if *how == Access::Sequential && was == Some(&Access::Indexed) && rows >= SEQ_SCAN_ROWS {
            found.push(Regression::SequentialScanAppeared {
                table: table.clone(),
                rows,
            });
        }
    }

    // An index the baseline used and this plan does not. Reported per index so
    // the message can name it, which is the difference between a finding and a
    // puzzle.
    for index in before.indexes.difference(&after.indexes) {
        found.push(Regression::IndexNoLongerUsed {
            index: index.clone(),
        });
    }

    // The accidental quadratic. Only when it is new: a nested loop that was
    // always there and is still there is the plan working as designed.
    if after.nodes.contains("Nested Loop") {
        for (table, rows) in &after.sequential_rows {
            let was_sequential = before
                .sequential_rows
                .get(table)
                .is_some_and(|had| *had >= *rows);
            if *rows >= SEQ_SCAN_ROWS && !was_sequential && before.nodes.contains("Nested Loop") {
                found.push(Regression::NestedLoopOverSequentialScan {
                    table: table.clone(),
                    rows: *rows,
                });
            }
        }
    }

    // Work per unit of answer. Skipped entirely when either side returned
    // nothing, because a ratio over zero rows says nothing about the query.
    if let (Some(before_ratio), Some(after_ratio)) = (before.amplification(), after.amplification())
    {
        if before_ratio > 0.0 && after_ratio >= before_ratio * AMPLIFICATION_FACTOR {
            found.push(Regression::MoreRowsPerRowReturned {
                before: before_ratio,
                after: after_ratio,
            });
        }
    }

    // A sort that was not there before, where the baseline reached its data
    // through an index. Without that second condition this fires whenever
    // somebody adds an ORDER BY, which is a change and not a regression.
    let new_sorts: Vec<String> = after.sorts.difference(&before.sorts).cloned().collect();
    if !new_sorts.is_empty()
        && !before.nodes.contains("Sort")
        && before.access.values().any(|how| *how == Access::Indexed)
    {
        found.push(Regression::SortAppeared { keys: new_sorts });
    }

    if after.max_batches > 1 && before.max_batches <= 1 {
        found.push(Regression::HashJoinSpilled {
            batches: after.max_batches,
        });
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::parse;
    use crate::shape::Shape;

    fn shape(json: &str) -> Shape {
        Shape::of(&parse(&serde_json::from_str(json).unwrap()))
    }

    fn indexed(table: &str, index: &str, rows: f64) -> Shape {
        shape(&format!(
            r#"{{"Node Type": "Index Scan", "Relation Name": "{table}",
                  "Index Name": "{index}", "Actual Rows": {rows}, "Actual Loops": 1.0}}"#
        ))
    }

    fn sequential(table: &str, rows: f64) -> Shape {
        shape(&format!(
            r#"{{"Node Type": "Seq Scan", "Relation Name": "{table}",
                  "Actual Rows": {rows}, "Actual Loops": 1.0}}"#
        ))
    }

    #[test]
    fn an_index_scan_becoming_a_large_sequential_scan_is_a_regression() {
        let found = compare(
            &indexed("orders", "orders_pkey", 1.0),
            &sequential("orders", 50_000.0),
        );
        assert!(found.iter().any(
            |r| matches!(r, Regression::SequentialScanAppeared { table, .. } if table == "orders")
        ));
        assert!(found.iter().any(
            |r| matches!(r, Regression::IndexNoLongerUsed { index } if index == "orders_pkey")
        ));
    }

    /// The rule that stops this being a tool that makes databases slower.
    #[test]
    fn a_sequential_scan_of_a_small_table_is_not_a_regression() {
        let found = compare(
            &indexed("lookup", "lookup_pkey", 1.0),
            &sequential("lookup", 40.0),
        );
        assert!(
            !found
                .iter()
                .any(|r| matches!(r, Regression::SequentialScanAppeared { .. })),
            "40 rows is the planner being right, not a regression: {found:?}"
        );
    }

    /// The property that decides whether anyone leaves this switched on.
    #[test]
    fn a_plan_that_got_better_is_not_a_regression() {
        let found = compare(
            &sequential("orders", 50_000.0),
            &indexed("orders", "orders_pkey", 1.0),
        );
        assert!(
            found.is_empty(),
            "improving must never fail a build: {found:?}"
        );
    }

    #[test]
    fn an_unchanged_plan_finds_nothing() {
        let plan = indexed("orders", "orders_pkey", 3.0);
        assert!(compare(&plan, &plan).is_empty());
    }

    #[test]
    fn reading_twice_as_much_for_the_same_answer_is_a_regression() {
        let before = shape(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0,
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 100.0, "Actual Loops": 1.0}]}"#,
        );
        let after = shape(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0,
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 400.0, "Actual Loops": 1.0}]}"#,
        );
        assert!(compare(&before, &after)
            .iter()
            .any(|r| matches!(r, Regression::MoreRowsPerRowReturned { .. })));
    }

    #[test]
    fn a_ratio_that_barely_moved_is_not_a_regression() {
        let before = shape(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0,
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 100.0, "Actual Loops": 1.0}]}"#,
        );
        let after = shape(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0,
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 150.0, "Actual Loops": 1.0}]}"#,
        );
        assert!(compare(&before, &after).is_empty());
    }

    #[test]
    fn a_hash_join_that_starts_spilling_is_a_regression() {
        let before = shape(r#"{"Node Type": "Hash Join", "Hash Batches": 1}"#);
        let after = shape(r#"{"Node Type": "Hash Join", "Hash Batches": 16}"#);
        assert_eq!(
            compare(&before, &after),
            vec![Regression::HashJoinSpilled { batches: 16 }]
        );
    }

    /// Adding an ORDER BY is a change to the query, not a degradation of it,
    /// and the baseline having no index scan is how the two are told apart.
    #[test]
    fn a_sort_on_a_plan_that_never_used_an_index_is_not_a_regression() {
        let before = sequential("t", 500.0);
        let after = shape(
            r#"{"Node Type": "Sort", "Sort Key": ["id"],
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 500.0, "Actual Loops": 1.0}]}"#,
        );
        assert!(!compare(&before, &after)
            .iter()
            .any(|r| matches!(r, Regression::SortAppeared { .. })));
    }

    #[test]
    fn losing_an_index_that_supplied_order_is_a_regression() {
        let before = indexed("t", "t_id_idx", 10.0);
        let after = shape(
            r#"{"Node Type": "Sort", "Sort Key": ["id"], "Actual Rows": 10.0, "Actual Loops": 1.0,
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 10.0, "Actual Loops": 1.0}]}"#,
        );
        assert!(compare(&before, &after)
            .iter()
            .any(|r| matches!(r, Regression::SortAppeared { .. })));
    }
}
