//! Whether a baseline still describes this database.
//!
//! A baseline is a claim about plans in a particular schema at a particular
//! volume. Change either and the old plans stop being a fair comparison — not
//! because the query got worse, but because the question changed. Reporting
//! that as a regression would be the fastest way to teach somebody to ignore
//! this tool.
//!
//! # The trap this file is built around
//!
//! The obvious fingerprint includes the indexes. It must not.
//!
//! Dropping an index is the single most common way a query plan gets worse, and
//! it is the first thing this exists to catch. If indexes were part of the
//! fingerprint then dropping one would change it, the baseline would be
//! declared inapplicable, and the tool would answer *"cannot compare"* at
//! precisely the moment it had something to say. The gate would be silent for
//! the exact regression it was written for.
//!
//! So the fingerprint covers **what a query is asking about** — the tables and
//! their columns — and **how much data there is**. Indexes are left out on
//! purpose, because they are the variable under test.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What has to match before two runs can be compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Table name to its columns, as `name:type` in ordinal order. Deliberately
    /// **not** including indexes — see the note above.
    pub tables: BTreeMap<String, Vec<String>>,
    /// Table name to the decade its row count falls in: 0 for empty, 3 for a
    /// thousand-ish, 6 for a million-ish.
    ///
    /// A decade rather than a count because a real database gains rows between
    /// two runs and nobody should re-baseline for that; and because a plan that
    /// changes between 1,000 and 1,100 rows was already on a knife edge, while
    /// one that changes between 1,000 and 1,000,000 changed for a real reason.
    pub volume: BTreeMap<String, u32>,
}

/// The decade a row count sits in. 0, 9 -> 0; 10, 99 -> 1; 1000 -> 3.
pub fn decade(rows: i64) -> u32 {
    if rows <= 0 {
        return 0;
    }
    (rows as f64).log10().floor() as u32
}

/// Read the fingerprint of a live database.
pub fn of(client: &mut postgres::Client, schemas: &[String]) -> Result<Fingerprint, String> {
    let columns = client
        .query(
            "SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod)
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_attribute a ON a.attrelid = c.oid
             WHERE c.relkind IN ('r', 'p')
               AND a.attnum > 0 AND NOT a.attisdropped
               AND n.nspname = ANY($1)
             ORDER BY c.relname, a.attnum",
            &[&schemas],
        )
        .map_err(|e| crate::explain::explain_error(&e))?;

    let mut tables: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &columns {
        let table: String = row.get(0);
        let column: String = row.get(1);
        let kind: String = row.get(2);
        tables
            .entry(table)
            .or_default()
            .push(format!("{column}:{kind}"));
    }

    // `reltuples` rather than `count(*)`: the planner uses the estimate, so the
    // estimate is what decides the plan. Counting for real would also mean a
    // full scan of every table in the schema just to decide whether to compare.
    let counts = client
        .query(
            "SELECT c.relname, c.reltuples::bigint
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p') AND n.nspname = ANY($1)",
            &[&schemas],
        )
        .map_err(|e| crate::explain::explain_error(&e))?;

    let mut volume = BTreeMap::new();
    for row in &counts {
        let table: String = row.get(0);
        let rows: i64 = row.get(1);
        volume.insert(table, decade(rows.max(0)));
    }

    Ok(Fingerprint { tables, volume })
}

/// Why a baseline does not apply here.
#[derive(Debug, Clone, PartialEq)]
pub enum Drift {
    TableGone(String),
    TableNew(String),
    ColumnsChanged(String),
    VolumeChanged { table: String, was: u32, now: u32 },
}

impl Drift {
    pub fn explain(&self) -> String {
        match self {
            Drift::TableGone(table) => {
                format!("\"{table}\" was in the baseline and is not in this database")
            }
            Drift::TableNew(table) => {
                format!("\"{table}\" is here and was not in the baseline")
            }
            Drift::ColumnsChanged(table) => {
                format!("the columns of \"{table}\" have changed")
            }
            Drift::VolumeChanged { table, was, now } => format!(
                "\"{table}\" held about 10^{was} rows when the baseline was taken and holds \
                 about 10^{now} now — plans legitimately differ at different volumes"
            ),
        }
    }
}

impl Fingerprint {
    /// Everything that stops these two being comparable. Empty means they are.
    ///
    /// Only tables the baseline knows about are checked for volume: a new table
    /// nobody has a plan for cannot invalidate anything.
    pub fn drift_from(&self, baseline: &Fingerprint) -> Vec<Drift> {
        let mut out = Vec::new();
        for (table, columns) in &baseline.tables {
            match self.tables.get(table) {
                None => out.push(Drift::TableGone(table.clone())),
                Some(now) if now != columns => out.push(Drift::ColumnsChanged(table.clone())),
                Some(_) => {}
            }
        }
        for table in self.tables.keys() {
            if !baseline.tables.contains_key(table) {
                out.push(Drift::TableNew(table.clone()));
            }
        }
        for (table, was) in &baseline.volume {
            if let Some(now) = self.volume.get(table) {
                if now != was {
                    out.push(Drift::VolumeChanged {
                        table: table.clone(),
                        was: *was,
                        now: *now,
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(table: &str, columns: &[&str], rows: i64) -> Fingerprint {
        Fingerprint {
            tables: [(
                table.to_string(),
                columns.iter().map(|c| c.to_string()).collect(),
            )]
            .into(),
            volume: [(table.to_string(), decade(rows))].into(),
        }
    }

    #[test]
    fn decades_bucket_by_order_of_magnitude() {
        assert_eq!(decade(0), 0);
        assert_eq!(decade(9), 0);
        assert_eq!(decade(10), 1);
        assert_eq!(decade(999), 2);
        assert_eq!(decade(1_000), 3);
        assert_eq!(decade(1_000_000), 6);
    }

    #[test]
    fn the_same_database_has_not_drifted() {
        let one = fingerprint("orders", &["id:integer"], 50_000);
        assert!(one.drift_from(&one).is_empty());
    }

    /// The reason a decade is used rather than a count.
    #[test]
    fn ordinary_growth_inside_a_decade_is_not_drift() {
        let before = fingerprint("orders", &["id:integer"], 50_000);
        let after = fingerprint("orders", &["id:integer"], 61_400);
        assert!(after.drift_from(&before).is_empty());
    }

    #[test]
    fn growing_by_an_order_of_magnitude_is_drift() {
        let before = fingerprint("orders", &["id:integer"], 5_000);
        let after = fingerprint("orders", &["id:integer"], 900_000);
        assert!(matches!(
            after.drift_from(&before).as_slice(),
            [Drift::VolumeChanged { .. }]
        ));
    }

    #[test]
    fn a_changed_column_list_is_drift() {
        let before = fingerprint("orders", &["id:integer"], 100);
        let after = fingerprint("orders", &["id:integer", "note:text"], 100);
        assert_eq!(
            after.drift_from(&before),
            vec![Drift::ColumnsChanged("orders".into())]
        );
    }

    /// The trap this whole file is arranged around. A fingerprint that noticed
    /// indexes would answer "cannot compare" at exactly the moment the tool had
    /// something worth saying.
    #[test]
    fn dropping_an_index_is_not_drift_because_indexes_are_the_thing_under_test() {
        let before = fingerprint("orders", &["id:integer", "user_id:integer"], 50_000);
        let after = fingerprint("orders", &["id:integer", "user_id:integer"], 50_000);
        assert!(
            after.drift_from(&before).is_empty(),
            "an index change must leave the baseline applicable, or the gate is \
             silent for the regression it exists to catch"
        );
    }

    #[test]
    fn a_new_table_nobody_baselined_still_counts_as_drift() {
        let before = fingerprint("orders", &["id:integer"], 100);
        let mut after = before.clone();
        after
            .tables
            .insert("audit".into(), vec!["id:integer".into()]);
        assert!(after
            .drift_from(&before)
            .contains(&Drift::TableNew("audit".into())));
    }
}
