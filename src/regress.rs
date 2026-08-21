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
                 the baseline, and the total it reads has at least doubled as well. So this \
                 is more work being done, rather than a smaller result set flattering the \
                 ratio."
            ),
            Regression::SortAppeared { keys } => format!(
                "a sort on ({}) appeared where the baseline had none, so this query is now \
                 ordering rows it used to get in order for free. The usual cause is an \
                 index that supplied that order being dropped, or hidden from the planner.",
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

    // The accidental quadratic, and the only rule here that needs the plan's
    // structure rather than a summary of it. `inner_loop_rows` holds a table
    // only when the plan really did scan it once per outer row, so the finding
    // can say "on its inner side" and have that be a fact rather than an
    // inference drawn from two unrelated ones.
    //
    // Two thresholds, because either alone is wrong. `SEQ_SCAN_ROWS` is the
    // same floor the sequential-scan rule uses — below it the planner is being
    // sensible. The second asks that it got *materially* worse rather than
    // merely moved: a loop already reading this table this hard is the plan
    // working as designed, and the build has been passing or failing on it
    // since the baseline was taken either way.
    for (table, rows) in &after.inner_loop_rows {
        let had = before.inner_loop_rows.get(table).copied().unwrap_or(0.0);
        if *rows >= SEQ_SCAN_ROWS && *rows >= had * AMPLIFICATION_FACTOR {
            found.push(Regression::NestedLoopOverSequentialScan {
                table: table.clone(),
                rows: *rows,
            });
        }
    }

    // Work per unit of answer. Skipped entirely when either side returned
    // nothing, because a ratio over zero rows says nothing about the query.
    //
    // The ratio alone is not enough, and this was wrong before it was right.
    // It is rows read over rows returned, so it doubles just as readily when
    // the *denominator* falls — and the denominator falls whenever the data
    // shifts under a predicate, with the plan untouched and not one extra row
    // read. The comparability fingerprint does not catch that, because it
    // buckets table volume by order of magnitude and a table can hold ten
    // thousand rows while what matches a `WHERE` moves from a hundred to ten.
    //
    // So the numerator has to have moved too. Requiring the total actually
    // read to have doubled makes the finding say only what both numbers show:
    // more work, not a smaller result set.
    if let (Some(before_ratio), Some(after_ratio)) = (before.amplification(), after.amplification())
    {
        let reads_more = after.rows_scanned >= before.rows_scanned * AMPLIFICATION_FACTOR;
        if before_ratio > 0.0 && after_ratio >= before_ratio * AMPLIFICATION_FACTOR && reads_more {
            found.push(Regression::MoreRowsPerRowReturned {
                before: before_ratio,
                after: after_ratio,
            });
        }
    }

    // A sort that was not there before, where the baseline reached its data
    // through an index. Without that second condition this fires whenever
    // somebody adds an ORDER BY, which is a change and not a regression.
    //
    // That condition is a proxy, and knowingly a loose one: it establishes that
    // *an* index was in use, not that the index in use was the one supplying
    // this order. Sort keys are not reliably qualified by table, so tying the
    // two together is not something this can currently show. The message
    // therefore states what is proven — the sort is new — and offers the cause
    // as the usual one rather than asserting it.
    let new_sorts: Vec<String> = after.sorts.difference(&before.sorts).cloned().collect();
    if !new_sorts.is_empty()
        && !before.nodes.contains("Sort")
        && before.access.values().any(|how| *how == Access::Indexed)
    {
        found.push(Regression::SortAppeared { keys: new_sorts });
    }

    // A hash join that has started spilling. `max_batches` begins at one and is
    // only raised by a node that reports batches, so "one batch" and "no hash
    // join anywhere" are the same number — and without the guard below, a plan
    // that *gained* a spilling hash join where the baseline had a nested loop
    // was reported as one that "fitted in memory in the baseline". It had not
    // fitted in memory; it had not existed. Verified against a real planner:
    // `Hash Batches` sits on the `Hash` node rather than on the `Hash Join`
    // above it, and both are present whenever either is.
    if before.nodes.contains("Hash Join") && before.max_batches <= 1 && after.max_batches > 1 {
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

    /// The case the rule is named for, and the one it used to miss entirely.
    ///
    /// The old rule required a `Nested Loop` in the *baseline*, so a plan that
    /// acquired one — the ordinary way this degradation arrives — was excluded
    /// by construction. Nine million rows for a one-row answer, and silence.
    #[test]
    fn a_join_that_becomes_a_loop_over_a_sequential_scan_is_a_regression() {
        let before = shape(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0, "Plans": [
                 {"Node Type": "Hash Join", "Actual Rows": 3000.0, "Actual Loops": 1.0,
                  "Hash Batches": 1, "Plans": [
                   {"Node Type": "Seq Scan", "Relation Name": "parent",
                    "Parent Relationship": "Outer",
                    "Actual Rows": 3000.0, "Actual Loops": 1.0},
                   {"Node Type": "Hash", "Parent Relationship": "Inner",
                    "Actual Rows": 3000.0, "Actual Loops": 1.0, "Plans": [
                     {"Node Type": "Seq Scan", "Relation Name": "child",
                      "Parent Relationship": "Outer",
                      "Actual Rows": 3000.0, "Actual Loops": 1.0}]}]}]}"#,
        );
        let after = shape(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0, "Plans": [
                 {"Node Type": "Nested Loop", "Actual Rows": 3000.0, "Actual Loops": 1.0,
                  "Plans": [
                   {"Node Type": "Seq Scan", "Relation Name": "parent",
                    "Parent Relationship": "Outer",
                    "Actual Rows": 3000.0, "Actual Loops": 1.0},
                   {"Node Type": "Seq Scan", "Relation Name": "child",
                    "Parent Relationship": "Inner",
                    "Actual Rows": 1.0, "Rows Removed by Filter": 2999.0,
                    "Actual Loops": 3000.0}]}]}"#,
        );
        let found = compare(&before, &after);
        assert!(
            found.iter().any(|r| matches!(
                r, Regression::NestedLoopOverSequentialScan { table, rows }
                if table == "child" && *rows == 9_000_000.0)),
            "the shape this rule is named for was not named: {found:?}"
        );
    }

    /// And the other direction, which is the one that gets a gate switched off.
    ///
    /// A nested loop reaching both its sides through indexes and, in an
    /// entirely different branch, a small table that grew. Two true facts about
    /// one plan with no relationship between them; the old rule joined them and
    /// reported a table no loop had ever touched.
    #[test]
    fn a_loop_and_an_unrelated_scan_are_two_facts_not_a_finding() {
        let branch = |log_rows: f64| {
            shape(&format!(
                r#"{{"Node Type": "Append", "Actual Rows": 2.0, "Actual Loops": 1.0, "Plans": [
                     {{"Node Type": "Nested Loop", "Parent Relationship": "Member",
                       "Actual Rows": 1.0, "Actual Loops": 1.0, "Plans": [
                        {{"Node Type": "Index Scan", "Relation Name": "a", "Index Name": "a_pkey",
                          "Parent Relationship": "Outer",
                          "Actual Rows": 1.0, "Actual Loops": 1.0}},
                        {{"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pkey",
                          "Parent Relationship": "Inner",
                          "Actual Rows": 1.0, "Actual Loops": 1.0}}]}},
                     {{"Node Type": "Seq Scan", "Relation Name": "log",
                       "Parent Relationship": "Member",
                       "Actual Rows": {log_rows}, "Actual Loops": 1.0}}]}}"#
            ))
        };
        let found = compare(&branch(10.0), &branch(40_000.0));
        assert!(
            !found
                .iter()
                .any(|r| matches!(r, Regression::NestedLoopOverSequentialScan { .. })),
            "nothing here is a scan on an inner side: {found:?}"
        );
    }

    /// A loop that was always expensive and is still exactly as expensive has
    /// not degraded. The baseline was taken with it in place; failing now would
    /// be failing for something nobody changed.
    #[test]
    fn a_loop_that_was_already_this_expensive_is_not_a_new_regression() {
        let plan = shape(
            r#"{"Node Type": "Nested Loop", "Actual Rows": 1.0, "Actual Loops": 1.0, "Plans": [
                 {"Node Type": "Seq Scan", "Relation Name": "a", "Parent Relationship": "Outer",
                  "Actual Rows": 100.0, "Actual Loops": 1.0},
                 {"Node Type": "Seq Scan", "Relation Name": "b", "Parent Relationship": "Inner",
                  "Actual Rows": 500.0, "Actual Loops": 100.0}]}"#,
        );
        assert!(
            compare(&plan, &plan).is_empty(),
            "fifty thousand inner rows in the baseline and fifty thousand now"
        );
    }

    /// Shaped the way a real planner shapes it, which is not where anyone
    /// writing this from memory puts it: `Hash Batches` is reported on the
    /// `Hash` node, not on the `Hash Join` above it. The previous fixture put
    /// it on the join, which is a fixture agreeing with its author.
    fn hash_join(batches: i64) -> Shape {
        shape(&format!(
            r#"{{"Node Type": "Hash Join", "Actual Rows": 10.0, "Actual Loops": 1.0,
                  "Plans": [
                   {{"Node Type": "Seq Scan", "Relation Name": "a",
                     "Parent Relationship": "Outer",
                     "Actual Rows": 10.0, "Actual Loops": 1.0}},
                   {{"Node Type": "Hash", "Parent Relationship": "Inner",
                     "Hash Batches": {batches},
                     "Actual Rows": 10.0, "Actual Loops": 1.0, "Plans": [
                      {{"Node Type": "Seq Scan", "Relation Name": "b",
                        "Parent Relationship": "Outer",
                        "Actual Rows": 10.0, "Actual Loops": 1.0}}]}}]}}"#
        ))
    }

    #[test]
    fn a_hash_join_that_starts_spilling_is_a_regression() {
        assert_eq!(
            compare(&hash_join(1), &hash_join(16)),
            vec![Regression::HashJoinSpilled { batches: 16 }]
        );
    }

    /// A plan that *gained* a hash join did not have one that fitted in memory.
    ///
    /// `max_batches` cannot tell "one batch" from "no hash join", so without a
    /// guard this reported a spill against a baseline that never hashed
    /// anything — naming a cause that had not happened, which is the same
    /// over-claim the nested-loop rule was carrying.
    #[test]
    fn a_plan_that_gained_a_hash_join_is_not_a_spill() {
        let looped = shape(
            r#"{"Node Type": "Nested Loop", "Actual Rows": 10.0, "Actual Loops": 1.0,
                 "Plans": [
                  {"Node Type": "Index Scan", "Relation Name": "a", "Index Name": "a_pkey",
                   "Parent Relationship": "Outer", "Actual Rows": 10.0, "Actual Loops": 1.0},
                  {"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pkey",
                   "Parent Relationship": "Inner", "Actual Rows": 1.0, "Actual Loops": 10.0}]}"#,
        );
        let found = compare(&looped, &hash_join(8));
        assert!(
            !found
                .iter()
                .any(|r| matches!(r, Regression::HashJoinSpilled { .. })),
            "there was no hash join in the baseline to fit in memory: {found:?}"
        );
    }

    /// The ratio doubled and the query did not read one extra row.
    ///
    /// Identical plan, identical rows scanned; only what matched the predicate
    /// moved. That is the data changing, not the query getting worse, and the
    /// fingerprint will not stop it — it buckets table volume by order of
    /// magnitude, and this table did not change size.
    #[test]
    fn a_ratio_that_moved_because_fewer_rows_matched_is_not_a_regression() {
        let plan = |returned: f64| {
            shape(&format!(
                r#"{{"Node Type": "Seq Scan", "Relation Name": "t",
                      "Actual Rows": {returned}, "Rows Removed by Filter": {},
                      "Actual Loops": 1.0}}"#,
                10_000.0 - returned
            ))
        };
        let found = compare(&plan(100.0), &plan(10.0));
        assert!(
            found.is_empty(),
            "ten thousand rows read on both sides is not more work: {found:?}"
        );
    }

    /// And the case it must still catch: the answer held and the work went up.
    #[test]
    fn a_ratio_that_moved_because_the_work_went_up_is_still_a_regression() {
        let before = shape(
            r#"{"Node Type": "Seq Scan", "Relation Name": "t",
                 "Actual Rows": 100.0, "Rows Removed by Filter": 900.0,
                 "Actual Loops": 1.0}"#,
        );
        let after = shape(
            r#"{"Node Type": "Seq Scan", "Relation Name": "t",
                 "Actual Rows": 100.0, "Rows Removed by Filter": 99900.0,
                 "Actual Loops": 1.0}"#,
        );
        assert!(
            compare(&before, &after)
                .iter()
                .any(|r| matches!(r, Regression::MoreRowsPerRowReturned { .. })),
            "same hundred rows back, a hundred times the reading"
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
