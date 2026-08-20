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
about forty minutes and needs pgseed's corpus checked out next door; those
schemas belong to their projects and are not redistributed here, so the test
skips cleanly when they are absent.

## What is measured

Three experiments per query, in this order.

| experiment | change | required verdict |
|---|---|---|
| **unchanged** | nothing at all | no finding |
| **benign** | re-`ANALYZE`; add a column; add an index on another column | no finding |
| **destructive** | drop the index the query was using | a finding |

The benign experiment is the one that matters. A gate which fails an innocent
migration is deleted from CI within a week, and everything it would have caught
afterwards is never caught. Adding a column is the most common migration there
is; an unrelated index arriving in the same release is somebody else's
optimisation. Neither made anything worse, so neither may be reported.

The destructive result is split in two, because they are not equally
informative:

- **a sequential scan was named** — the planner actually fell back to reading
  the table, and the table was over the row threshold. This required the plan to
  genuinely degrade.
- **only the missing index was named** — true by construction the instant the
  index is dropped, since its name is in the baseline and absent afterwards.
  Counted separately rather than folded into a headline, because on its own it
  demonstrates that a dropped index has a name.

## Result

Measured 2026-08-20, PostgreSQL via `postgresql_embedded`, 3,000 rows per table.

```
24 schema files read
15 produced a query
82 candidate lookups had rows behind them
68 of those were planned through the index — only those are counted

index dropped, of 68
  a sequential scan was named ....... 61  (90%)
  only the missing index was named ..  7
  nothing was said ..................  0

reported when nothing got worse
  nothing changed at all ............  0
  re-analyzed, column added, index added .  0

not measured, seeding over budget ....  2  (gitlab, sourcegraph)
```

Per schema:

| schema | queries | scan named | index only |
|---|---:|---:|---:|
| camunda | 6 | 6 | 0 |
| discourse | 5 | 5 | 0 |
| documenso | 5 | 5 | 0 |
| harbor | 2 | 2 | 0 |
| hexpm | 6 | 6 | 0 |
| hydra | 5 | 5 | 0 |
| kratos | 5 | 3 | 2 |
| langfuse | 5 | 5 | 0 |
| listmonk | 6 | 4 | 2 |
| mattermost | 4 | 4 | 0 |
| plausible | 3 | 3 | 0 |
| powerdns | 3 | 3 | 0 |
| sourcegraph_codeintel | 4 | 3 | 1 |
| sourcegraph_insights | 5 | 4 | 1 |
| synapse | 4 | 3 | 1 |

Every dropped index produced a finding. Nothing was reported when nothing got
worse.

## What this does not show

Stated because a survey that reports only its coverage reports the coverage of
the easy cases as if it were the coverage of all of them.

**Nine of twenty-four schemas produced no measurement.** Seven offered no
non-unique single-column index on an ordinary scalar type, so there was no
lookup of the shape an application issues: hasura, kong, lago, postgrest,
temporal, vaultwarden, zitadel. Two — gitlab and sourcegraph — could not be
seeded inside a seven-minute budget, because `--include` bounds what pgseed is
*asked* to fill and not the foreign-key closure it must build to get there. Both
are reported as could-not-measure rather than dropped from the denominator.

**Fourteen of eighty-two candidates were discarded before the experiment**,
because the planner did not route them through the index even with it present.
A query already being scanned sequentially cannot regress that way, and counting
it would have flattered the result.

**One of six named regressions is exercised here.** The corpus drops indexes.
`NestedLoopOverSequentialScan`, `MoreRowsPerRowReturned`, `SortAppeared` and
`HashJoinSpilled` are provoked in `tests/regressions.rs` against a fixture, and
this survey says nothing about how they behave on a schema nobody here designed.

**The data is pgseed's, not the schema owner's.** Selectivity drives planner
choice, and a column filled with deterministic synthetic values does not have
production's distribution. This measures the pair — pgseed's data and pgplan's
judgement — on real *schemas*, not on real *data*.

## Three ways this survey was wrong before it was right

Each of these produced a plausible number that was wrong in a different
direction, which is the argument for not publishing the first result a harness
gives you.

1. **It never finished.** The first version seeded 2,000 rows into all 1,057
   GitLab tables. No number at all.

2. **The number was circular.** The second reported "caught 24 of 24" by
   counting any finding, including `IndexNoLongerUsed` — which is true the
   moment the index is dropped. It also tested flapping by running an identical
   query twice in one session, which demonstrates determinism and nothing else.
   Flattering.

3. **A check that never ran was scored as a check that failed.** The third
   reported that pgplan said nothing for 20 of 62 dropped indexes. It had not.
   `DROP INDEX` fails on an index backing a unique constraint, the harness
   ignored the error, and the unchanged plan was recorded as a missed detection.
   camunda went from 5/6 to 6/6 once the drop was verified rather than assumed.
   Unfairly harsh.

The harness now excludes constraint-backed indexes from the candidate set,
verifies that each drop actually happened, and prints the candidate count so the
denominator is visible rather than implied.
