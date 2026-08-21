# pgplan

A migration drops an index nothing seems to use. The tests pass — the results
are still correct. CI is green. At 3am the query that was an index lookup is a
sequential scan over four million rows.

`pgplan` fails that build.

```
pgplan baseline --dsn "$DATABASE_URL" -q queries.sql -o plans.json   # commit this
pgplan check    --dsn "$DATABASE_URL" -q queries.sql -b plans.json   # in CI
```

```
pgplan: 1 of 2 queries got worse.

  orders by user
    SELECT id, total FROM orders WHERE user_id = 42
    — "orders" is now read sequentially — 50000 rows, where the baseline
      reached it through an index. An index it was using has gone, or a change
      stopped the planner being able to use one.
    — the index "orders_user_id_idx" is no longer used by this query.
    — this reads 5000 rows for every row it returns, against 1 in the baseline.
```

## Why this is not just "diff the EXPLAIN output"

Because that does not work, and it is worth being specific about why.

`EXPLAIN` output moves when the planner is upgraded, when statistics are
refreshed, when one more row crosses a threshold, and when the query gets
**better**. A gate built on it fails for all of those, so it gets switched off
inside a week — and a disabled gate is worse than none, because it also occupies
the slot a working one would have had.

So the rule here is narrower:

> **Never fail a build for a plan change that cannot be shown to be worse.**

Four decisions follow from it:

- **Shape is compared, cost never is.** `Total Cost` is not comparable between
  two machines. It is read, and it is thrown away.
- **`EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF)`.** Real row counts, and no
  clock anywhere in the output. Timing is the unstable half of `ANALYZE`; rows
  are a count, and counts are stable when the data is.
- **A closed set of named regressions.** Not "the plan changed" — six specific
  degradations, each provable from two plans. Anything else is reported and
  does not fail.
- **Thresholds.** A sequential scan of sixty rows is the planner being right.
  Failing a build for it would teach people to add indexes that make their
  database slower.

## Three outcomes, not two

| | exit | |
|---|---:|---|
| pass | **0** | no named regression |
| regression | **1** | a named degradation, with the query and the reason |
| could-not-compare | **2** | the baseline describes a different database |

The third is what keeps the gate trusted. If the schema changed or the table
grew by an order of magnitude, the old plans are not a fair comparison — not
because anything got worse, but because the question changed. Reporting that as
a regression is the fastest way to teach somebody to ignore this tool, so it
says what happened and asks for a re-baseline instead.

**Indexes are deliberately not part of that comparison.** They are the variable
under test: if dropping one made the baseline "inapplicable", the tool would go
quiet at exactly the moment it had something to say.

## What it catches

| | |
|---|---|
| a sequential scan appeared | a table reached through an index no longer is, and it is big enough to matter |
| an index stopped being used | named, so you know which one |
| a nested loop over a sequential scan | fast on ten rows, fatal on a million |
| more rows read per row returned | the work went up and the answer did not |
| a sort appeared | this query now orders rows it used to get in order for free |
| a hash join started spilling | it fitted in memory before |

## Why it needs realistic data

Plan regressions only appear at volume. At two hundred rows a sequential scan is
the *correct* plan, so a baseline taken against a handful of fixtures records
the wrong thing and compares against it forever.

It also has to be the **same** data each run, or the baseline is noise. That is
why this pairs with [pgseed](https://github.com/Blahaj-gif/pgseed), which
produces byte-identical rows from the same seed:

```yaml
- run: pgseed --dsn "$DATABASE_URL" --apply --truncate --rows 10000
- run: pgplan check --dsn "$DATABASE_URL" -q queries.sql -b plans.json
```

Any deterministic fixture set works. The requirement is determinism, not pgseed.

## The queries

A file of SQL. `-- name:` gives a statement a name for the report; without one
it gets a stable hash of the statement, so reindenting or reordering the file
does not orphan a baseline entry.

```sql
-- name: orders by user
SELECT id, total FROM orders WHERE user_id = $1;

-- name: revenue
SELECT sum(total) FROM orders;
```

## Options

| | |
|---|---|
| `--dsn` / `$DATABASE_URL` | where to read plans from |
| `-q, --queries` | the SQL file |
| `-o, --out` / `-b, --baseline` | the baseline file; default `plans.json` |
| `--schema` | schemas to fingerprint, repeatable; default `public` |
| `--remote` | allow a database that is not on this machine |

**`EXPLAIN (ANALYZE)` runs every statement it plans.** Inside a transaction that
is always rolled back — there is a test that plans an `INSERT` and asserts the
table is still empty — but it runs. So a host that is not this machine needs
`--remote`, and the check is on the *host*, because reading a database name for
the word "prod" stops nobody who called theirs `main`.

## Testing

```
cargo test                          parsers, shapes and the closed set; no database
cargo test -- --test-threads=1      everything, against a real Postgres
```

Integration tests download and start their own Postgres via
`postgresql_embedded` — no Docker, no install. That matters here more than
usual: a plan is the database's opinion, so testing against a model of a planner
would be testing a model of the thing under test.

Four things were found that way, none reachable from hand-written JSON. A
filtered `Seq Scan` reports the rows it **kept**, with the discarded ones in a
separate field — so a full table scan returning ten rows looked like a ten-row
lookup. A bitmap plan reports its rows twice, which made *adding an index*
register as a two-fold regression. `Hash Batches` sits on the `Hash` node and
not on the `Hash Join` above it, and a fixture that put it on the join passed
anyway. And the planner will not simply do as it is told: a fixture built to
force a loop over an unindexed table had the child put on the *outside* with a
memoized key lookup on the inside, which is the planner being right and the test
measuring nothing.

The same rule caught a bug report that was not one. A probe showed the row
threshold failing a build for a forty-row lookup table, because rows are summed
across executions and repetition carried it over. Nine measured arrangements
later: the planner materialises a small inner side rather than re-scanning it,
so the count stays at forty. The fix that had been planned was not written. A
finding a planner has not seen is a hypothesis.

`tests/stability.rs` exists because flapping is what kills this category. It
asserts that the same data twice gives the same answer, that a fresh `ANALYZE`
moves nothing, and that adding an index never fails a build.

## Measured on schemas nobody here designed

Every test above runs against a fixture written by the same person who wrote the
rule it exercises, which measures agreement rather than accuracy. So the same
checks were run against twenty-four production schemas — GitLab, Discourse,
Mattermost, Synapse and others — seeded by [pgseed][pgseed] at volume and
queried on indexes those projects chose for themselves. All six named
regressions are exercised there, three of them only since the joins were added.

Of 81 queries the planner routed through an index, then degraded four ways.
Dropping an index does not always make a plan worse — nine times the planner
reached the same data through another index without reading more, and those are
held out rather than counted as failures to detect:

| | | |
|---|---:|---:|
| the index dropped, the plan absorbed it | 9 | *nothing to report* |
| of the 72 that degraded, a sequential scan named | **72** | (100%) |
| of the 72 that degraded, nothing said | **0** | |
| the column wrapped so the index cannot serve it | **81** | (100%) |
| an `ORDER BY` the index supplied, a sort named | **73 of 80** | (91%) |
| a `count(*)` whose index went, of the 72 that fell back to a scan | **72** | (100%) |

And across 41 foreign-key pairs with rows on both sides, joined:

| | | |
|---|---:|---:|
| forced onto a nested loop, of the 22 that put a scan on the inner side | **22** | (100%) |
| `work_mem` at its floor, of the 39 that actually spilled | **39** | (100%) |

The spill row's denominator is the one thing here that moves: across six runs it
bit 27, 29, 27, 27, 29 and 39, the last after the survey was widened, and the
run-to-run movement is always the same two GitLab pairs. `ANALYZE` samples, so
the planner's batch decision moves with the statistics. Every bite was named in
every run.

And with nothing made worse — re-`ANALYZE`d, a column added, an unrelated index
added, each an ordinary migration:

| | |
|---|---:|
| findings reported | **0** |

The last table is the one that decides whether anyone leaves this switched on.
The wrapped-column row is the one closest to a real incident: no schema changes,
the answer is identical, and only the plan is worse — so a test that checks
results cannot see it at all.

**Every denominator above is what the experiment actually did, not what it was
asked to do.** Dropping an index does not oblige the planner to start reading
the table; cramping `work_mem` does not oblige a hash join to spill. Counting
those as misses is a mistake this survey made three times before it stopped
making it, so each experiment reports *tried*, *bit* and *named* separately and
only the bites are a denominator.

Eighteen of the twenty-four schemas produce a lookup query and twenty-one produce
a join; three produce neither, for three different reasons, and all three are
named. Three of the seven that once produced nothing turned out to be schemas the
harness had failed to load and then measured as empty — the seventh of seven ways
this survey has been wrong, and the reason it now reports what it could not do
beside what it did. The method, the full per-schema table, what the numbers do
*not* show, all seven wrong answers, and the two bugs it found in this tool's own
nested-loop rule are in
[docs/corpus-validation.md](docs/corpus-validation.md).

[pgseed]: https://github.com/Blahaj-gif/pgseed

## Contributing

The most useful contribution is one more named regression. The bar and the
mechanics are in [CONTRIBUTING.md](CONTRIBUTING.md) — the short version is that
a rule has to be provable from two plans, has to have a threshold, and has to
be proved not to fire when the query got *better*.

## Licence

MIT.
