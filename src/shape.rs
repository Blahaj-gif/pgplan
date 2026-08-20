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
//! parallel worker counts, the shape of the tree itself — is read and thrown
//! away, because none of it can be compared across two machines without
//! producing an argument rather than an answer.

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
