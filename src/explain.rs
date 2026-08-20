//! Running `EXPLAIN` and turning what comes back into a tree we can reason about.
//!
//! # Why `ANALYZE` with the timing switched off
//!
//! `EXPLAIN` alone gives estimates. Estimates are the planner's opinion, and a
//! gate built on them fails when statistics drift rather than when the query
//! got worse. `EXPLAIN ANALYZE` gives what actually happened — but it normally
//! also gives wall-clock milliseconds, which differ on every run, on every
//! machine, and under every amount of CI noise.
//!
//! `TIMING OFF` removes exactly that and keeps `Actual Rows`, which is a
//! **count**. Counts are deterministic when the data is, and the data is: the
//! whole point of pairing this with a deterministic seeder. Verified against a
//! real Postgres before the design was committed to — `TIMING OFF, SUMMARY OFF`
//! yields a JSON tree with row counts and no clock anywhere in it.
//!
//! # The query is run
//!
//! `ANALYZE` executes the statement. That is a real cost and a real hazard, so
//! it happens inside a transaction that is always rolled back, and the CLI
//! refuses a host it cannot show is local unless told otherwise. A gate that
//! quietly writes to your database while measuring it would be a poor trade.

use serde::{Deserialize, Serialize};

/// One node of a plan tree, with only the fields that are allowed to matter.
///
/// Costs are read but deliberately not stored in the baseline — see `shape`.
/// They are here so a report can mention one, not so a comparison can use one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub node_type: String,
    /// The table this node reads, when it reads one.
    pub relation: Option<String>,
    /// The index this node uses, when it uses one.
    pub index: Option<String>,
    /// Rows this node actually produced. `None` when the plan was not analysed.
    pub actual_rows: Option<f64>,
    /// Rows the node looked at and threw away. A sequential scan that returns
    /// ten rows out of fifty thousand reports ten here and fifty thousand
    /// there, and reading only the first is how "it only returned ten rows"
    /// hides a full table scan.
    pub rows_filtered: Option<f64>,
    /// Times this node was executed — an inner side of a nested loop runs once
    /// per outer row, and ignoring that understates the work by that factor.
    pub loops: Option<f64>,
    /// Hash join batches. More than one means it spilled to disk.
    pub batches: Option<i64>,
    /// Present on a `Sort`, and the reason an index that supplied order is
    /// worth noticing the loss of.
    pub sort_key: Vec<String>,
    pub children: Vec<Node>,
}

impl Node {
    /// Every node in the tree, parents before children.
    pub fn walk(&self) -> Vec<&Node> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.walk());
        }
        out
    }

    /// Rows this node produced across all of its executions.
    ///
    /// `Actual Rows` is per loop. A nested loop's inner scan reporting one row
    /// over four million loops has produced four million rows, and reading the
    /// first number alone is how that goes unnoticed.
    pub fn rows_produced(&self) -> f64 {
        self.actual_rows.unwrap_or(0.0) * self.loops.unwrap_or(1.0)
    }

    /// Rows this node had to look at, across all executions.
    ///
    /// Output rows plus the ones the filter discarded. This is the number that
    /// says what the query cost; `rows_produced` is what it handed on.
    pub fn rows_read(&self) -> f64 {
        (self.actual_rows.unwrap_or(0.0) + self.rows_filtered.unwrap_or(0.0))
            * self.loops.unwrap_or(1.0)
    }

    /// Whether this node's rows are already counted by its parent.
    ///
    /// A `Bitmap Index Scan` feeds a `Bitmap Heap Scan` directly above it, and
    /// both report the rows. Counting both doubles the work attributed to a
    /// bitmap plan, which made *adding an index* look like a two-fold
    /// regression — the exact false positive this project cannot afford.
    pub fn is_counted_by_parent(&self) -> bool {
        self.node_type == "Bitmap Index Scan"
    }

    pub fn is_scan(&self) -> bool {
        self.node_type.ends_with("Scan")
    }

    /// Whether this node reaches the table through an index.
    pub fn is_indexed_scan(&self) -> bool {
        matches!(
            self.node_type.as_str(),
            "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" | "Bitmap Heap Scan"
        )
    }
}

/// What went wrong when a plan could not be obtained at all.
#[derive(Debug)]
pub enum Failed {
    /// The statement itself is bad, or references something absent. Quoted back
    /// rather than summarised, because the message is the useful part.
    Rejected(String),
    /// The database answered, but not with a plan we recognise.
    Unreadable(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Rejected(why) => write!(f, "{why}"),
            Failed::Unreadable(why) => write!(f, "the plan could not be read: {why}"),
        }
    }
}

/// Ask the database for a plan, with the statement executed and rolled back.
pub fn plan_of(client: &mut postgres::Client, sql: &str) -> Result<Node, Failed> {
    // A transaction that is never committed. `ANALYZE` runs the statement, and
    // a gate that leaves rows behind has changed the thing it was measuring.
    let mut transaction = client
        .transaction()
        .map_err(|e| Failed::Rejected(explain_error(&e)))?;

    let statement = format!(
        "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF, COSTS OFF, FORMAT JSON) {}",
        sql.trim().trim_end_matches(';')
    );
    let rows = transaction
        .query(statement.as_str(), &[])
        .map_err(|e| Failed::Rejected(explain_error(&e)));

    // Rolled back whether the query worked or not.
    let _ = transaction.rollback();

    let rows = rows?;
    let raw: serde_json::Value = rows
        .first()
        .ok_or_else(|| Failed::Unreadable("no rows returned".into()))?
        .get(0);

    let root = raw
        .get(0)
        .and_then(|entry| entry.get("Plan"))
        .ok_or_else(|| Failed::Unreadable("no Plan object in the JSON".into()))?;
    Ok(parse(root))
}

/// The database's own message, which `postgres::Error` hides behind "db error".
pub fn explain_error(error: &postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => db.message().to_string(),
        None => {
            let mut cause: &dyn std::error::Error = error;
            while let Some(next) = cause.source() {
                cause = next;
            }
            cause.to_string()
        }
    }
}

/// One JSON node into one `Node`, recursively.
pub fn parse(value: &serde_json::Value) -> Node {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let number = |key: &str| value.get(key).and_then(serde_json::Value::as_f64);

    Node {
        node_type: text("Node Type").unwrap_or_else(|| "Unknown".into()),
        relation: text("Relation Name"),
        index: text("Index Name"),
        actual_rows: number("Actual Rows"),
        rows_filtered: number("Rows Removed by Filter"),
        loops: number("Actual Loops"),
        batches: value
            .get("Hash Batches")
            .and_then(serde_json::Value::as_i64),
        sort_key: value
            .get("Sort Key")
            .and_then(serde_json::Value::as_array)
            .map(|keys| {
                keys.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        children: value
            .get("Plans")
            .and_then(serde_json::Value::as_array)
            .map(|kids| kids.iter().map(parse).collect())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_SCAN: &str = r#"{
        "Node Type": "Index Scan", "Relation Name": "t", "Index Name": "t_pkey",
        "Actual Rows": 1.0, "Actual Loops": 1.0
    }"#;

    #[test]
    fn an_index_scan_is_read_whole() {
        let node = parse(&serde_json::from_str(INDEX_SCAN).unwrap());
        assert_eq!(node.node_type, "Index Scan");
        assert_eq!(node.relation.as_deref(), Some("t"));
        assert_eq!(node.index.as_deref(), Some("t_pkey"));
        assert!(node.is_indexed_scan());
    }

    /// The one that is easy to get wrong, and expensive when it is.
    #[test]
    fn rows_produced_multiplies_by_the_loop_count() {
        let inner = parse(
            &serde_json::from_str(
                r#"{"Node Type": "Seq Scan", "Relation Name": "u",
                     "Actual Rows": 1.0, "Actual Loops": 40000.0}"#,
            )
            .unwrap(),
        );
        // One row per loop looks harmless and is forty thousand rows of work.
        assert_eq!(inner.actual_rows, Some(1.0));
        assert_eq!(inner.rows_produced(), 40_000.0);
    }

    #[test]
    fn a_tree_is_walked_parents_first() {
        let tree = parse(
            &serde_json::from_str(
                r#"{"Node Type": "Nested Loop", "Plans": [
                     {"Node Type": "Seq Scan", "Relation Name": "a"},
                     {"Node Type": "Index Scan", "Relation Name": "b"}]}"#,
            )
            .unwrap(),
        );
        let kinds: Vec<&str> = tree.walk().iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(kinds, ["Nested Loop", "Seq Scan", "Index Scan"]);
    }

    #[test]
    fn a_sort_keeps_its_key_and_a_hash_keeps_its_batches() {
        let sort = parse(
            &serde_json::from_str(r#"{"Node Type": "Sort", "Sort Key": ["id", "name"]}"#).unwrap(),
        );
        assert_eq!(sort.sort_key, ["id", "name"]);
        let hash = parse(
            &serde_json::from_str(r#"{"Node Type": "Hash Join", "Hash Batches": 8}"#).unwrap(),
        );
        assert_eq!(hash.batches, Some(8));
    }

    /// Found by a real planner, not by reading the docs: a filtered sequential
    /// scan reports the rows it *kept*, and the ones it threw away sit in a
    /// separate field. Reading only the first made a full table scan returning
    /// ten rows look like a ten-row lookup.
    #[test]
    fn a_filtered_scan_counts_the_rows_it_discarded() {
        let node = parse(
            &serde_json::from_str(
                r#"{"Node Type": "Seq Scan", "Relation Name": "orders",
                     "Actual Rows": 10.0, "Rows Removed by Filter": 49990.0,
                     "Actual Loops": 1.0}"#,
            )
            .unwrap(),
        );
        assert_eq!(node.rows_produced(), 10.0);
        assert_eq!(node.rows_read(), 50_000.0);
    }

    /// Also found by a real planner. A bitmap plan reports its rows twice, and
    /// counting both made *adding an index* register as a two-fold regression.
    #[test]
    fn a_bitmap_index_scan_is_not_counted_twice() {
        let inner = parse(
            &serde_json::from_str(r#"{"Node Type": "Bitmap Index Scan", "Index Name": "i"}"#)
                .unwrap(),
        );
        let outer = parse(
            &serde_json::from_str(r#"{"Node Type": "Bitmap Heap Scan", "Relation Name": "t"}"#)
                .unwrap(),
        );
        assert!(inner.is_counted_by_parent());
        assert!(!outer.is_counted_by_parent());
    }

    /// A plan taken without ANALYZE has no counts, and everything downstream
    /// has to cope rather than assume.
    #[test]
    fn a_plan_without_actuals_reports_none_rather_than_zero() {
        let node = parse(&serde_json::from_str(r#"{"Node Type": "Seq Scan"}"#).unwrap());
        assert_eq!(node.actual_rows, None);
        assert_eq!(node.rows_produced(), 0.0);
    }
}
