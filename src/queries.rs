//! The statements to watch, read from a file.
//!
//! A query needs a name, because a report saying *"query 3 regressed"* sends
//! somebody counting semicolons. A `-- name:` comment above a statement gives
//! it one; without it the name is a short hash of the statement itself, which
//! is stable across reorderings of the file — moving a query up should not
//! orphan its baseline entry.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub name: String,
    pub sql: String,
}

/// Split a file into named statements.
///
/// Semicolons inside string literals do not end a statement, which matters the
/// moment somebody writes `WHERE note = 'a;b'`. Dollar-quoting is not handled:
/// a function body is not a query anyone gates on, and pretending to parse one
/// would be a guess.
pub fn parse(text: &str) -> Vec<Query> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut name: Option<String> = None;
    let mut quoted = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if !quoted {
            if let Some(rest) = trimmed.strip_prefix("-- name:") {
                // A name applies to the next statement, so a name arriving
                // while one is half-written belongs to the one after it.
                if current.trim().is_empty() {
                    name = Some(rest.trim().to_string());
                }
                continue;
            }
            if trimmed.starts_with("--") || trimmed.is_empty() {
                continue;
            }
        }

        let mut rest = line;
        loop {
            let Some(at) = end_of_statement(rest, &mut quoted) else {
                break;
            };
            current.push_str(&rest[..at]);
            push(&mut out, &mut current, &mut name);
            rest = &rest[at + 1..];
        }
        current.push_str(rest);
        current.push('\n');
    }
    push(&mut out, &mut current, &mut name);
    out
}

fn push(out: &mut Vec<Query>, current: &mut String, name: &mut Option<String>) {
    let sql = current.trim().to_string();
    current.clear();
    if sql.is_empty() {
        return;
    }
    let chosen = name.take().unwrap_or_else(|| short_hash(&sql));
    out.push(Query { name: chosen, sql });
}

/// The next top-level semicolon, advancing the quote state over what it passes.
fn end_of_statement(text: &str, quoted: &mut bool) -> Option<usize> {
    for (index, ch) in text.char_indices() {
        match ch {
            '\'' => *quoted = !*quoted,
            ';' if !*quoted => return Some(index),
            _ => {}
        }
    }
    None
}

/// A short, stable name for an unnamed statement.
///
/// FNV-1a, written out rather than pulled in: it is eight lines, it is stable
/// across releases and platforms, and a hash that changed between versions
/// would silently orphan every unnamed baseline entry.
pub fn short_hash(sql: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalise(sql).bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("q{hash:012x}")
}

/// Whitespace-insensitive, so reindenting a query keeps its name.
fn normalise(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_are_split_on_semicolons() {
        let queries = parse("SELECT 1;\nSELECT 2;\n");
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].sql, "SELECT 1");
        assert_eq!(queries[1].sql, "SELECT 2");
    }

    #[test]
    fn a_name_comment_names_the_statement_below_it() {
        let queries = parse("-- name: orders by user\nSELECT 1;\n");
        assert_eq!(queries[0].name, "orders by user");
    }

    #[test]
    fn an_unnamed_statement_gets_a_stable_hash() {
        let once = parse("SELECT 1;");
        let again = parse("SELECT 1;");
        assert_eq!(once[0].name, again[0].name);
        assert!(once[0].name.starts_with('q'));
    }

    /// Reindenting a query must not orphan its baseline entry.
    #[test]
    fn the_hash_ignores_whitespace() {
        assert_eq!(
            short_hash("SELECT a FROM t WHERE b = 1"),
            short_hash("SELECT a\n  FROM t\n  WHERE b = 1")
        );
    }

    #[test]
    fn two_different_queries_do_not_share_a_name() {
        assert_ne!(short_hash("SELECT a FROM t"), short_hash("SELECT b FROM t"));
    }

    /// `WHERE note = 'a;b'` is a real query and must survive being read.
    #[test]
    fn a_semicolon_inside_a_string_does_not_end_the_statement() {
        let queries = parse("SELECT * FROM t WHERE note = 'a;b';\n");
        assert_eq!(queries.len(), 1);
        assert!(queries[0].sql.contains("'a;b'"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_statements() {
        let queries = parse("-- just a note\n\n-- another\nSELECT 1;\n");
        assert_eq!(queries.len(), 1);
    }

    #[test]
    fn a_trailing_statement_without_a_semicolon_is_still_read() {
        let queries = parse("SELECT 1");
        assert_eq!(queries.len(), 1);
    }
}
