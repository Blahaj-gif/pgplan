//! `pgplan` — fail a build when a query plan gets worse.
//!
//! Two subcommands and three exit codes. The third code is the one that keeps
//! the gate alive: a schema that has moved on makes the baseline inapplicable,
//! and answering "regression" there would train somebody to ignore this.

use std::collections::BTreeMap;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use pgplan::baseline::{Baseline, Entry};
use pgplan::explain::{explain_error, plan_of};
use pgplan::fingerprint;
use pgplan::queries::{self, Query};
use pgplan::regress::compare;
use pgplan::shape::Shape;

#[derive(Parser)]
#[command(
    name = "pgplan",
    version,
    about = "Fails a build when a query plan gets worse — a named, provable \
             degradation, not a diff of EXPLAIN output"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record what the plans are now, to commit alongside the code.
    Baseline {
        #[command(flatten)]
        common: Common,
        /// Where to write the baseline.
        #[arg(long, short, default_value = "plans.json")]
        out: String,
    },
    /// Compare the plans now against a committed baseline.
    Check {
        #[command(flatten)]
        common: Common,
        /// The committed baseline to compare against.
        #[arg(long, short, default_value = "plans.json")]
        baseline: String,
    },
}

#[derive(clap::Args)]
struct Common {
    /// Where to read the plans from.
    #[arg(long, env = "DATABASE_URL")]
    dsn: String,
    /// A file of SQL statements, one per `;`, optionally named with `-- name:`.
    #[arg(long, short)]
    queries: String,
    /// Schemas to fingerprint. Repeatable; defaults to `public`.
    #[arg(long, default_value = "public")]
    schema: Vec<String>,
    /// Allow a database that is not on this machine.
    ///
    /// `EXPLAIN (ANALYZE)` *runs* each statement — inside a transaction that is
    /// rolled back, but it runs. Pointing that at a production database by
    /// accident is a mistake worth one flag.
    #[arg(long)]
    remote: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let (common, mode) = match &args.command {
        Command::Baseline { common, out } => (common, Mode::Write(out.clone())),
        Command::Check { common, baseline } => (common, Mode::Check(baseline.clone())),
    };

    match run(common, mode) {
        Ok(code) => code,
        Err(problem) => {
            eprintln!("pgplan: {problem}");
            // Could not be done at all, which is neither a pass nor a regression.
            ExitCode::from(2)
        }
    }
}

enum Mode {
    Write(String),
    Check(String),
}

fn run(common: &Common, mode: Mode) -> Result<ExitCode, String> {
    if !common.remote && !is_local(&common.dsn) {
        return Err(format!(
            "this database does not look like it is on this machine, and `EXPLAIN \
             (ANALYZE)` runs every statement it plans. Pass --remote if that is \
             what you meant.\n       {}",
            redact(&common.dsn)
        ));
    }

    let text = std::fs::read_to_string(&common.queries)
        .map_err(|e| format!("could not read {}: {e}", common.queries))?;
    let statements = queries::parse(&text);
    if statements.is_empty() {
        return Err(format!("{} holds no statements", common.queries));
    }

    let mut client = postgres::Client::connect(&common.dsn, postgres::NoTls)
        .map_err(|e| format!("cannot connect: {}", explain_error(&e)))?;
    let now = fingerprint::of(&mut client, &common.schema)?;

    match mode {
        Mode::Write(path) => write_baseline(&mut client, &statements, now, &path),
        Mode::Check(path) => check(&mut client, &statements, now, &path),
    }
}

fn write_baseline(
    client: &mut postgres::Client,
    statements: &[Query],
    now: fingerprint::Fingerprint,
    path: &str,
) -> Result<ExitCode, String> {
    let mut baseline = Baseline::new(now);
    let mut refused = Vec::new();

    for query in statements {
        match plan_of(client, &query.sql) {
            Ok(plan) => {
                baseline.queries.insert(
                    query.name.clone(),
                    Entry {
                        sql: query.sql.clone(),
                        shape: Shape::of(&plan),
                    },
                );
            }
            // A statement that does not run is not recorded as a plan of
            // nothing. A baseline holding an entry that was never measured
            // would compare against a fiction.
            Err(why) => refused.push(format!("{}: {why}", query.name)),
        }
    }

    if baseline.queries.is_empty() {
        return Err(format!(
            "not one statement could be planned:\n       {}",
            refused.join("\n       ")
        ));
    }

    std::fs::write(path, baseline.to_json()).map_err(|e| format!("could not write {path}: {e}"))?;

    eprintln!(
        "pgplan: wrote {} — {} queries, {} tables",
        path,
        baseline.queries.len(),
        baseline.fingerprint.tables.len()
    );
    for line in &refused {
        eprintln!("  not planned: {line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn check(
    client: &mut postgres::Client,
    statements: &[Query],
    now: fingerprint::Fingerprint,
    path: &str,
) -> Result<ExitCode, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
    let baseline = Baseline::from_json(&text)?;

    // Comparability first. Everything after this assumes the two runs are
    // asking the same question of the same database, and if they are not then
    // every finding below would be about the schema rather than the query.
    let drift = now.drift_from(&baseline.fingerprint);
    if !drift.is_empty() {
        eprintln!("pgplan: this baseline does not describe this database.\n");
        for reason in drift.iter().take(8) {
            eprintln!("  {}", reason.explain());
        }
        if drift.len() > 8 {
            eprintln!("  ... and {} more", drift.len() - 8);
        }
        eprintln!(
            "\n  Nothing was compared. Re-run `pgplan baseline` and commit the result.\n  \
             This is not a regression: plans legitimately differ when the schema or the \
             volume does."
        );
        return Ok(ExitCode::from(2));
    }

    let mut regressions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut checked, mut unknown, mut unplannable) = (0usize, Vec::new(), Vec::new());

    for query in statements {
        let Some(recorded) = baseline.queries.get(&query.name) else {
            unknown.push(query.name.clone());
            continue;
        };
        match plan_of(client, &query.sql) {
            Ok(plan) => {
                checked += 1;
                let found = compare(&recorded.shape, &Shape::of(&plan));
                if !found.is_empty() {
                    regressions.insert(
                        query.name.clone(),
                        found.iter().map(|r| r.explain()).collect(),
                    );
                }
            }
            Err(why) => unplannable.push(format!("{}: {why}", query.name)),
        }
    }

    // A statement that will not run at all is a broken build, not a plan that
    // got worse — and definitely not a pass.
    if !unplannable.is_empty() && checked == 0 {
        return Err(format!(
            "no statement could be planned:\n       {}",
            unplannable.join("\n       ")
        ));
    }

    if regressions.is_empty() {
        eprintln!("pgplan: {checked} queries, no regression.");
        report_asides(&unknown, &unplannable);
        return Ok(ExitCode::SUCCESS);
    }

    eprintln!(
        "pgplan: {} of {checked} queries got worse.\n",
        regressions.len()
    );
    for (name, found) in &regressions {
        eprintln!("  {name}");
        if let Some(entry) = baseline.queries.get(name) {
            eprintln!("    {}", one_line(&entry.sql));
        }
        for line in found {
            eprintln!("    — {line}");
        }
        eprintln!();
    }
    report_asides(&unknown, &unplannable);
    Ok(ExitCode::from(1))
}

fn report_asides(unknown: &[String], unplannable: &[String]) {
    if !unknown.is_empty() {
        eprintln!(
            "  {} not in the baseline, so not compared: {}",
            unknown.len(),
            unknown.join(", ")
        );
    }
    for line in unplannable {
        eprintln!("  could not be planned: {line}");
    }
}

fn one_line(sql: &str) -> String {
    let joined = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 96 {
        format!("{}...", joined.chars().take(93).collect::<String>())
    } else {
        joined
    }
}

/// Whether the host is this machine.
///
/// Asked rather than guessed at: reading a database name for the word "prod"
/// stops nobody who called theirs `main` and annoys everybody whose local copy
/// is a production dump. The host is a fact; the name is a habit.
fn is_local(dsn: &str) -> bool {
    let host = dsn
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?']).next())
        .map(|authority| authority.rsplit('@').next().unwrap_or(authority))
        .map(|hostport| {
            hostport
                .rsplit_once(':')
                .map_or(hostport, |(host, _)| host)
                .trim_matches(['[', ']'])
                .to_string()
        });
    match host.as_deref() {
        None | Some("") => true, // a socket, or `host=` in key/value form
        Some(name) => matches!(name, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"),
    }
}

/// A connection string with the password removed, for printing.
fn redact(dsn: &str) -> String {
    match (dsn.find("://"), dsn.find('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}://***@{}", &dsn[..scheme], &dsn[at + 1..])
        }
        _ => dsn.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_host_is_recognised_in_the_forms_people_write() {
        assert!(is_local("postgres://localhost/app"));
        assert!(is_local("postgres://user:pw@127.0.0.1:5432/app"));
        assert!(is_local("postgresql://[::1]:5432/app"));
        assert!(is_local("postgres:///app")); // unix socket
    }

    #[test]
    fn anything_else_needs_the_flag() {
        assert!(!is_local("postgres://db.internal:5432/app"));
        assert!(!is_local("postgres://user:pw@10.0.0.7/app"));
        assert!(!is_local("postgres://prod.example.com/app"));
    }

    #[test]
    fn a_password_is_not_printed_back() {
        assert_eq!(
            redact("postgres://me:hunter2@db.internal/app"),
            "postgres://***@db.internal/app"
        );
        assert!(!redact("postgres://me:hunter2@db.internal/app").contains("hunter2"));
    }

    #[test]
    fn a_long_statement_is_shortened_for_the_report() {
        let long = "SELECT ".to_string() + &"column_name, ".repeat(40);
        assert!(one_line(&long).chars().count() <= 96);
        assert_eq!(one_line("SELECT  1\n  FROM t"), "SELECT 1 FROM t");
    }
}
