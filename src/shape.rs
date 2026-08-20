//! The part of a plan a baseline is allowed to remember.
//!
//! A baseline that stores the whole plan compares the whole plan, and then
//! fails when `Total Cost` moves from 8.31 to 8.34 because the planner was
//! upgraded. That gate gets disabled in a week, and a disabled gate is worse
//! than none because it also occupies the slot a working one would have had.
//!
//! So what is stored is a deliberately lossy summary: **which tables were
//! reached how, which indexes were used, how many rows were touched, and what
//! kinds of node appeared.** Everything else — costs, estimates, widths,
//! parallel worker counts — is read and thrown away, because none of it can be
//! compared across two machines without producing an argument rather than an
//! answer.
//!
//! The tree's structure is thrown away too, with exactly one exception. A
//! sequential scan on the inner side of a nested loop runs once per outer row,
//! and no summary that has forgotten which side it was on can tell that from
//! the same table being read once elsewhere in the same plan. So that one fact
//! is kept, and it is kept because a rule needs it — not because it was there.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::explain::Node;

/// How a table was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Access {
    /// Through an index. The good case, and the one worth protecting.
    Indexed,
    /// Every row, read in order. Correct and fast on a small table; the thing
    /// that takes a site down on a large one.
    Sequential,
    /// Something else — a CTE scan, a function scan, a values list.
    Other,
}

/// A plan, reduced to what can be compared honestly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    /// Table name to how it was reached. Sorted, so two runs serialise the same.
    pub access: BTreeMap<String, Access>,
    /// Every index the plan used, by name.
    pub indexes: BTreeSet<String>,
    /// Node types present anywhere in the tree.
    pub nodes: BTreeSet<String>,
    /// Rows every scan node had to look at, summed across loops. Rows read,
    /// not rows returned — see `Node::rows_read`.
    pub rows_scanned: f64,
    /// Rows the query actually returned.
    pub rows_returned: f64,
    /// The largest hash-join batch count seen. More than one means a spill.
    pub max_batches: i64,
    /// Sort keys, so losing an index that supplied order is visible.
    pub sorts: BTreeSet<String>,
    /// Tables read sequentially, with how many rows that cost. Kept separately
    /// from `access` because the threshold questions need the number.
    pub sequential_rows: BTreeMap<String, f64>,
    /// Tables read sequentially *beneath the inner side of a nested loop*,
    /// with rows summed across every iteration.
    ///
    /// A subset of `sequential_rows`, kept apart because it is the only thing
    /// that can tell an accidental quadratic from a large table being read
    /// once somewhere else in the same plan. Without it the rule had to settle
    /// for "a nested loop exists" and "some table is scanned" — two facts that
    /// are usually about different halves of the tree.
    ///
    /// Defaulted rather than required so a baseline written before this field
    /// existed still parses, and is then refused by the version check with a
    /// message that says what to do instead of a parse error that does not.
    #[serde(default)]
    pub inner_loop_rows: BTreeMap<String, f64>,
}

/// Every table scanned sequentially under a nested loop's inner side.
///
/// `Node::walk` is flat, and flat is enough for every other field here: it does
/// not matter *where* an index was used, only that it was. It matters entirely
/// here, because "the inner side" is the whole claim. So this one walks the
/// tree as a tree.
///
/// Which side a child is on comes from the planner's own `Parent Relationship`,
/// falling back to position for a plan that did not say. A `Materialize` above
/// the inner side is deliberately not special-cased: the scan beneath it reads
/// the table once and reports one loop, so what is counted falls to what the
/// plan actually read from disk rather than to what multiplying by the outer
/// row count would suggest. The finding then rests on that table being large
/// enough to be worth scanning at all, which is the honest claim.
fn inner_sequential(root: &Node) -> BTreeMap<String, f64> {
    fn descend(node: &Node, under_inner: bool, out: &mut BTreeMap<String, f64>) {
        if under_inner && node.node_type == "Seq Scan" && !node.is_counted_by_parent() {
            if let Some(table) = &node.relation {
                *out.entry(table.clone()).or_insert(0.0) += node.rows_read();
            }
        }
        let loops_here = node.node_type == "Nested Loop";
        for (position, child) in node.children.iter().enumerate() {
            let is_inner = match child.parent_relationship.as_deref() {
                Some(side) => side == "Inner",
                None => position == 1,
            };
            descend(child, under_inner || (loops_here && is_inner), out);
        }
    }
    let mut out = BTreeMap::new();
    descend(root, false, &mut out);
    out
}

impl Shape {
    pub fn of(root: &Node) -> Shape {
        let mut shape = Shape {
            access: BTreeMap::new(),
            indexes: BTreeSet::new(),
            nodes: BTreeSet::new(),
            rows_scanned: 0.0,
            rows_returned: root.rows_produced(),
            max_batches: 1,
            sorts: BTreeSet::new(),
            sequential_rows: BTreeMap::new(),
            inner_loop_rows: inner_sequential(root),
        };

        for node in root.walk() {
            shape.nodes.insert(node.node_type.clone());
            if let Some(index) = &node.index {
                shape.indexes.insert(index.clone());
            }
            if let Some(batches) = node.batches {
                shape.max_batches = shape.max_batches.max(batches);
            }
            for key in &node.sort_key {
                shape.sorts.insert(key.clone());
            }
            if node.is_scan() && !node.is_counted_by_parent() {
                let rows = node.rows_read();
                shape.rows_scanned += rows;
                if let Some(table) = &node.relation {
                    let how = if node.is_indexed_scan() {
                        Access::Indexed
                    } else if node.node_type == "Seq Scan" {
                        *shape.sequential_rows.entry(table.clone()).or_insert(0.0) += rows;
                        Access::Sequential
                    } else {
                        Access::Other
                    };
                    // A table can be reached twice in one plan, once each way.
                    // The worse reading is the one that matters, and `Access`
                    // orders Indexed before Sequential for exactly this.
                    let entry = shape.access.entry(table.clone()).or_insert(how);
                    if how > *entry {
                        *entry = how;
                    }
                }
            }
        }
        shape
    }

    /// Rows touched for each row handed back.
    ///
    /// The single most useful number about a query, and the one that survives
    /// a planner upgrade: returning ten rows after reading ten is a lookup,
    /// and returning ten after reading four million is a table scan wearing a
    /// LIMIT. Returns `None` when nothing came back, because a ratio over zero
    /// rows is not a fact about the query.
    pub fn amplification(&self) -> Option<f64> {
        (self.rows_returned > 0.0).then(|| self.rows_scanned / self.rows_returned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::parse;

    fn shape_of(json: &str) -> Shape {
        Shape::of(&parse(&serde_json::from_str(json).unwrap()))
    }

    #[test]
    fn an_index_scan_records_the_table_and_the_index() {
        let shape = shape_of(
            r#"{"Node Type": "Index Scan", "Relation Name": "orders",
                 "Index Name": "orders_user_id_idx", "Actual Rows": 3.0, "Actual Loops": 1.0}"#,
        );
        assert_eq!(shape.access.get("orders"), Some(&Access::Indexed));
        assert!(shape.indexes.contains("orders_user_id_idx"));
        assert!(shape.sequential_rows.is_empty());
    }

    #[test]
    fn a_sequential_scan_records_what_it_cost() {
        let shape = shape_of(
            r#"{"Node Type": "Seq Scan", "Relation Name": "orders",
                 "Actual Rows": 50000.0, "Actual Loops": 1.0}"#,
        );
        assert_eq!(shape.access.get("orders"), Some(&Access::Sequential));
        assert_eq!(shape.sequential_rows.get("orders"), Some(&50_000.0));
    }

    /// One table, reached both ways in one plan. The bad reading has to win, or
    /// a partial regression hides behind the half that still uses an index.
    #[test]
    fn the_worse_of_two_readings_of_one_table_is_the_one_kept() {
        let shape = shape_of(
            r#"{"Node Type": "Append", "Plans": [
                 {"Node Type": "Index Scan", "Relation Name": "t", "Index Name": "i",
                  "Actual Rows": 1.0, "Actual Loops": 1.0},
                 {"Node Type": "Seq Scan", "Relation Name": "t",
                  "Actual Rows": 900.0, "Actual Loops": 1.0}]}"#,
        );
        assert_eq!(shape.access.get("t"), Some(&Access::Sequential));
    }

    /// The shape the whole `NestedLoopOverSequentialScan` rule rests on: an
    /// inner sequential scan reporting one row per loop and three thousand
    /// loops has read three thousand rows, not one.
    #[test]
    fn a_sequential_scan_on_the_inner_side_is_recorded_as_such() {
        let shape = shape_of(
            r#"{"Node Type": "Nested Loop", "Actual Rows": 3000.0, "Actual Loops": 1.0,
                 "Plans": [
                   {"Node Type": "Seq Scan", "Relation Name": "parent",
                    "Parent Relationship": "Outer",
                    "Actual Rows": 3000.0, "Actual Loops": 1.0},
                   {"Node Type": "Seq Scan", "Relation Name": "child",
                    "Parent Relationship": "Inner",
                    "Actual Rows": 1.0, "Rows Removed by Filter": 2999.0,
                    "Actual Loops": 3000.0}]}"#,
        );
        assert_eq!(shape.inner_loop_rows.get("child"), Some(&9_000_000.0));
        assert_eq!(
            shape.inner_loop_rows.get("parent"),
            None,
            "the outer side is read once and is not the quadratic"
        );
    }

    /// The false positive that made the old rule unusable: a nested loop in one
    /// branch and a large sequential scan in another are two unrelated facts.
    #[test]
    fn a_sequential_scan_elsewhere_in_the_plan_is_not_on_the_inner_side() {
        let shape = shape_of(
            r#"{"Node Type": "Append", "Actual Rows": 2.0, "Actual Loops": 1.0, "Plans": [
                 {"Node Type": "Nested Loop", "Actual Rows": 1.0, "Actual Loops": 1.0,
                  "Parent Relationship": "Member", "Plans": [
                    {"Node Type": "Index Scan", "Relation Name": "a", "Index Name": "a_pkey",
                     "Parent Relationship": "Outer", "Actual Rows": 1.0, "Actual Loops": 1.0},
                    {"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pkey",
                     "Parent Relationship": "Inner", "Actual Rows": 1.0, "Actual Loops": 1.0}]},
                 {"Node Type": "Seq Scan", "Relation Name": "log",
                  "Parent Relationship": "Member",
                  "Actual Rows": 40000.0, "Actual Loops": 1.0}]}"#,
        );
        assert!(
            shape.inner_loop_rows.is_empty(),
            "nothing here is a sequential scan on an inner side: {:?}",
            shape.inner_loop_rows
        );
        assert_eq!(shape.sequential_rows.get("log"), Some(&40_000.0));
    }

    /// A plan from a source that did not label its children still has to be
    /// read, and for a two-child join the second one is the inner side.
    #[test]
    fn without_a_label_the_second_child_is_the_inner_side() {
        let shape = shape_of(
            r#"{"Node Type": "Nested Loop", "Actual Rows": 1.0, "Actual Loops": 1.0,
                 "Plans": [
                   {"Node Type": "Seq Scan", "Relation Name": "a",
                    "Actual Rows": 5.0, "Actual Loops": 1.0},
                   {"Node Type": "Seq Scan", "Relation Name": "b",
                    "Actual Rows": 200.0, "Actual Loops": 5.0}]}"#,
        );
        assert_eq!(shape.inner_loop_rows.get("b"), Some(&1_000.0));
        assert_eq!(shape.inner_loop_rows.get("a"), None);
    }

    #[test]
    fn amplification_is_rows_read_over_rows_returned() {
        let shape = shape_of(
            r#"{"Node Type": "Aggregate", "Actual Rows": 1.0, "Actual Loops": 1.0,
                 "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t",
                            "Actual Rows": 4000.0, "Actual Loops": 1.0}]}"#,
        );
        assert_eq!(shape.amplification(), Some(4000.0));
    }

    #[test]
    fn a_query_that_returned_nothing_has_no_ratio_rather_than_a_huge_one() {
        let shape = shape_of(
            r#"{"Node Type": "Seq Scan", "Relation Name": "t",
                 "Actual Rows": 0.0, "Actual Loops": 1.0}"#,
        );
        assert_eq!(shape.amplification(), None);
    }

    /// The property the whole file exists for: the same plan twice is the same
    /// shape, and a cost that moved does not change it.
    #[test]
    fn cost_is_not_part_of_the_shape() {
        let cheap = shape_of(
            r#"{"Node Type": "Seq Scan", "Relation Name": "t", "Total Cost": 8.31,
                 "Actual Rows": 10.0, "Actual Loops": 1.0}"#,
        );
        let dear = shape_of(
            r#"{"Node Type": "Seq Scan", "Relation Name": "t", "Total Cost": 9917.44,
                 "Actual Rows": 10.0, "Actual Loops": 1.0}"#,
        );
        assert_eq!(cheap, dear);
    }
}
