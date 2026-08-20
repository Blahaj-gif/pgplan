# Does this work on schemas nobody here designed?

Every other test in this project runs against a fixture written by the same
person who wrote the rule it exercises. That is the failure this whole family of
projects exists around: a corpus written by its own author contains only the
constructs its author remembered, so it measures agreement rather than accuracy.
The project next door scored 17/22 on its own corpus and 1/55 on real material.

So the twenty-four production schemas pgseed is benchmarked against are seeded
by pgseed at volume, queried on indexes those projects chose for themselves, and
then perturbed.

Reproduce with `cargo test --test corpus -- --ignored --nocapture`. It takes
about eighteen minutes and needs pgseed's corpus checked out next door; those
schemas belong to their projects and are not redistributed here, so the test
skips cleanly when they are absent.

## What is measured

Five experiments per query. Two must report nothing, three must report
something.

| experiment | change | required verdict |
|---|---|---|
| **unchanged** | nothing at all | no finding |
| **benign** | re-`ANALYZE`; add a column; add an index on another column | no finding |
| **dropped** | drop the index the query was using | a finding |
| **wrapped** | `WHERE col::text \|\| '' = …`, which the index cannot serve | a finding |
| **unordered** | `ORDER BY` an index was supplying, index dropped | a `Sort` named |

The benign experiment is the one that matters most. A gate which fails an
innocent migration is deleted from CI within a week, and everything it would
have caught afterwards is never caught. Adding a column is the most common
migration there is; an unrelated index arriving in the same release is somebody
else's optimisation. Neither made anything worse, so neither may be reported.

The **wrapped** experiment changes no schema at all. The answer is identical and
only the plan is worse, so a test that checks results cannot see it — which is
what makes it the most representative of a real incident.

The dropped result is split in two, because the halves are not equally
informative:

- **a sequential scan was named** — the planner actually fell back to reading
  the table, and the table was over the row threshold. This required the plan to
  genuinely degrade.
- **only the missing index was named** — true by construction the instant the
  index is dropped, since its name is in the baseline and absent afterwards.
  Counted separately rather than folded into a headline, because on its own it
  demonstrates only that a dropped index has a name.

## Result

Measured 2026-08-20, PostgreSQL via `postgresql_embedded`, 3,000 rows per table.

```
24 schema files read
17 produced a query
 0 could not be measured
93 candidate lookups had rows behind them
76 of those were planned through the index — only those are counted

index dropped, of 76
  a sequential scan was named ....... 67  (88%)
  only the missing index was named ..  9
  nothing was said ..................  0

indexed column wrapped, of 76
  named ............................. 76  (100%)

ORDER BY an index was supplying, index dropped, of 75
  a sort was named .................. 68  (91%)

reported when nothing got worse
  nothing changed at all ............  0
  re-analyzed, column added, index added .  0
```

Per schema, for the dropped-index experiment:

| schema | queries | scan named | index only |
|---|---:|---:|---:|
| camunda | 6 | 6 | 0 |
| discourse | 5 | 5 | 0 |
| documenso | 5 | 5 | 0 |
| gitlab | 4 | 3 | 1 |
| harbor | 2 | 2 | 0 |
| hexpm | 6 | 6 | 0 |
| hydra | 5 | 5 | 0 |
| kratos | 5 | 3 | 2 |
| langfuse | 5 | 5 | 0 |
| listmonk | 6 | 4 | 2 |
| mattermost | 4 | 4 | 0 |
| plausible | 3 | 3 | 0 |
| powerdns | 3 | 3 | 0 |
| sourcegraph | 4 | 3 | 1 |
| sourcegraph_codeintel | 4 | 3 | 1 |
| sourcegraph_insights | 5 | 4 | 1 |
| synapse | 4 | 3 | 1 |

Every degradation produced a finding. Nothing was reported when nothing got
worse.

## What this does not show

Stated because a survey that reports only its coverage reports the coverage of
the easy cases as if it were the coverage of all of them.

**Seven of twenty-four schemas produced no measurement.** They offer no
non-unique single-column index on an ordinary scalar type, so there was no
lookup of the shape an application issues: hasura, kong, lago, postgrest,
temporal, vaultwarden, zitadel.

**Seventeen of ninety-three candidates were discarded before the experiment**,
because the planner did not route them through the index even with it present.
A query already being scanned sequentially cannot regress that way, and counting
it would have flattered the result.

**Three of six named regressions are exercised here.**
`NestedLoopOverSequentialScan` and `HashJoinSpilled` need a join, and
`MoreRowsPerRowReturned` is reachable but was not separated from the wrapped
experiment. All three are provoked in `tests/regressions.rs` against a fixture,
and this survey says nothing about how they behave on a schema nobody here
designed.

**The data is pgseed's, not the schema owner's.** Selectivity drives planner
choice, and a column filled with deterministic synthetic values does not have
production's distribution. This measures the pair — pgseed's data and pgplan's
judgement — on real *schemas*, not on real *data*.

## Four ways this survey was wrong before it was right

Each produced a plausible number that was wrong in a different direction, which
is the argument for not publishing the first result a harness gives you. Three
of the four share one shape: **an experiment that did not bite is
indistinguishable from a tool that missed something**, unless you check.

1. **It never finished.** The first version seeded 2,000 rows into all 1,057
   GitLab tables. No number at all.

2. **The number was circular.** The second reported "caught 24 of 24" by
   counting any finding, including `IndexNoLongerUsed` — which is true the
   moment the index is dropped. It also tested flapping by running an identical
   query twice in one session, which demonstrates determinism and nothing else.
   Flattering.

3. **A drop that failed was scored as a miss.** The third reported that pgplan
   said nothing for 20 of 62 dropped indexes. It had not. `DROP INDEX` fails on
   an index backing a unique constraint, the harness ignored the error, and the
   unchanged plan was recorded as a missed detection. Unfairly harsh.

4. **A cast that changed nothing was scored as a miss.** The fourth wrapped
   predicates as `col::text`, which on a `text` column is identity — the planner
   sees through it, keeps the index, and reporting nothing is correct. That
   scored 43 of 76. With `col::text || ''`, which defeats the index for every
   type in the candidate set, the same run scores 76 of 76. Unfairly harsh
   again, and the same mistake as the one above wearing different clothes.

The harness now excludes constraint-backed indexes, verifies that each drop
actually happened, uses a wrap that bites on every candidate type, and prints
the candidate count so the denominator is visible rather than implied.

## What this survey found in pgseed

Worth recording, because it is the reason to run a tool against another team's
tool rather than only against a fixture. Two bugs, neither reachable from inside
pgseed's own suite:

- **`--probe` ignored `--include`** and offered every table in the database.
  `pgseed --include orders --probe` filled the lot. That is what made GitLab and
  Sourcegraph unmeasurable here — not the seven-minute budget.
- **`--include` on a child table aborted the run**, because classification does
  not know about the selection: the child was reported fillable and then died on
  a not-null foreign key. This predated the first bug's fix and had been hidden
  by it, since the parent was getting rows as a side effect of filling
  everything. Fixing the probe took this survey from 15 usable schemas to 5,
  which is how it surfaced.

With both fixed, the two largest schemas in the corpus became measurable and
nothing is now reported as could-not-measure.
