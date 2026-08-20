//! The committed file: what the plans were, and what database they were of.
//!
//! Versioned from the first release, because this file is meant to live in
//! somebody's repository for years and be read by a binary they upgrade
//! occasionally. A format that changed shape without saying so would produce a
//! confident wrong answer, which is the one outcome worth engineering against.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::fingerprint::Fingerprint;
use crate::shape::Shape;

/// Bumped only when an older file can no longer be read correctly.
///
/// 2: a shape now records which tables were scanned sequentially on the inner
/// side of a nested loop. A version-1 file does not carry that, so every plan
/// in it would compare as though no loop had ever touched anything — the
/// comparison would run and quietly answer a different question. Refusing it is
/// the only honest reading.
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub sql: String,
    pub shape: Shape,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    /// What was true of the database when these plans were taken.
    pub fingerprint: Fingerprint,
    /// Query name to what its plan looked like.
    pub queries: BTreeMap<String, Entry>,
}

impl Baseline {
    pub fn new(fingerprint: Fingerprint) -> Baseline {
        Baseline {
            version: VERSION,
            fingerprint,
            queries: BTreeMap::new(),
        }
    }

    /// Pretty-printed, because this is a committed file and a diff of it should
    /// be readable by the person reviewing the pull request that changes it.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a baseline always serialises")
    }

    pub fn from_json(text: &str) -> Result<Baseline, String> {
        let parsed: Baseline = serde_json::from_str(text)
            .map_err(|e| format!("this baseline could not be read: {e}"))?;
        if parsed.version != VERSION {
            return Err(format!(
                "this baseline is version {} and this is pgplan's version {VERSION}. \
                 Re-run `pgplan baseline` to write a current one.",
                parsed.version
            ));
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::parse;
    use crate::shape::Shape;

    fn a_baseline() -> Baseline {
        let mut baseline = Baseline::new(Fingerprint {
            tables: [("t".to_string(), vec!["id:integer".to_string()])].into(),
            volume: [("t".to_string(), 4)].into(),
        });
        baseline.queries.insert(
            "by id".into(),
            Entry {
                sql: "SELECT * FROM t WHERE id = 1".into(),
                shape: Shape::of(&parse(
                    &serde_json::from_str(
                        r#"{"Node Type": "Index Scan", "Relation Name": "t",
                             "Index Name": "t_pkey", "Actual Rows": 1.0, "Actual Loops": 1.0}"#,
                    )
                    .unwrap(),
                )),
            },
        );
        baseline
    }

    #[test]
    fn a_baseline_survives_the_round_trip() {
        let original = a_baseline();
        let read = Baseline::from_json(&original.to_json()).expect("should read");
        assert_eq!(original, read);
    }

    #[test]
    fn a_baseline_from_another_version_is_refused_rather_than_guessed_at() {
        let mut ahead = a_baseline();
        ahead.version = VERSION + 1;
        let error = Baseline::from_json(&ahead.to_json()).unwrap_err();
        assert!(
            error.contains("Re-run"),
            "the message must say what to do: {error}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_baseline_says_so() {
        let error = Baseline::from_json("{\"nonsense\": true}").unwrap_err();
        assert!(error.contains("could not be read"));
    }

    /// The file that exists in somebody's repository right now.
    ///
    /// It parses — the new field defaults — and that is exactly the danger: it
    /// would compare cleanly and mean something else. The version check is what
    /// turns a wrong answer into an instruction.
    #[test]
    fn a_baseline_written_before_the_shape_grew_is_refused_not_reinterpreted() {
        let old = r#"{
          "version": 1,
          "fingerprint": {"tables": {"t": ["id:integer"]}, "volume": {"t": 4}},
          "queries": {"by id": {"sql": "SELECT * FROM t WHERE id = 1", "shape": {
            "access": {"t": "Indexed"}, "indexes": ["t_pkey"], "nodes": ["Index Scan"],
            "rows_scanned": 1.0, "rows_returned": 1.0, "max_batches": 1,
            "sorts": [], "sequential_rows": {}}}}
        }"#;
        let error = Baseline::from_json(old).unwrap_err();
        assert!(
            error.contains("Re-run"),
            "an older file must be told what to do, not guessed at: {error}"
        );
    }

    /// The committed file is reviewed by people, so its diff has to be legible.
    #[test]
    fn the_written_form_is_indented() {
        assert!(a_baseline().to_json().contains("\n  "));
    }
}
