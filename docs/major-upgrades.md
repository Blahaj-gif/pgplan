# Does a plan survive a major Postgres upgrade?

Every vendor runbook for a major upgrade describes the same manual ritual:
capture the queries that matter, replay them on the new cluster, compare the
plans, and — the hard part — tell a real regression from a plan that merely
changed. That last clause is what this project is for, so the question is
whether it already answers it.

It appears nobody ships a tool that does. Aurora's Query Plan Management is
proprietary, AWS-only, and *pins* plans rather than checking them. PostgreSQL 18
can carry planner statistics across an upgrade, which removes a common cause of
trouble without verifying the outcome. Everything else in the search results is
a blog post describing the ritual.

`cargo test --test corpus plans_across_a_major_upgrade -- --ignored --nocapture`

## What is measured

Each corpus schema is loaded and seeded on **three** servers: 16.4, 16.4 again,
and 17.0. The first derives the questions; the other two answer that exact SQL,
verbatim. Three kinds of question:

| kind | query |
|---|---|
| indexed lookup | `SELECT * FROM t WHERE col = <a value that exists>` |
| join | `SELECT count(*) FROM parent p LEFT JOIN child c ON c.fk = p.pk` |
| cramped join | the same join with `work_mem` at its 64kB floor |

No planner flags are switched off. The question here is what the planner chooses
by itself on each version, not whether a forced shape can be detected — which is
the survey's job, next door.

## The three things that make a zero mean something

A run that reports no differences is the answer this experiment was hoping for,
which is exactly when it should be distrusted. Three separate guards exist
because the first two attempts produced that zero and neither was evidence.

**The control arm.** Two fresh clusters of the *same* version need not agree:
`ANALYZE` samples rather than reading every row, and the survey next door has
watched a verdict move between runs for that reason. So the same-version pair is
measured too, and it is the noise floor the cross-version pair is read against.

**The versions are checked, not requested.** The first attempt asked for 17.0
and never confirmed it got it, so it could have compared 16.4 against itself
twice and reported zero differences. Every server is now asked `SHOW
server_version` and the run fails unless both majors appear.

**A positive control.** The second attempt tried to establish sensitivity by
argument: cramped joins were chosen because the survey's spill count moves
between runs. That reasoning was wrong — the survey's movement is in the
*default-to-cramped transition*, and this compares cramped against cramped, like
with like, which is far more stable. A witness chosen by argument witnessed
nothing.

So instead the pipeline is handed a difference on purpose. After the deriving
server's shapes are recorded, an index one of the queries was using is dropped,
the same query is replanned, and the same `compare()` has to say something about
it. The server is discarded immediately afterwards. If that fails on too many
schemas, the run fails and the zeros above are not reported as a finding.

Three schemas contribute joins and have no lookup index to drop, so about
seventeen can host the control at all.

## Result

Measured 2026-08-22, `postgresql_embedded`, 3,000 rows per table.

```
server versions actually measured: 16.4, 17.0
20 schemas, 171 baseline plans
  87 reach their data through an index
  78 contain a join node
positive control: 15 schemas were handed a dropped index and this
                  pipeline reported it

same version, two fresh clusters (the noise floor)
  indexed lookup                93 compared,  93 identical,  0 changed,  0 regressions
  join, default settings        39 compared,  39 identical,  0 changed,  0 regressions
  join, work_mem at the floor   39 compared,  39 identical,  0 changed,  0 regressions

16.4 -> 17.0
  indexed lookup                93 compared,  93 identical,  0 changed,  0 regressions
  join, default settings        39 compared,  39 identical,  0 changed,  0 regressions
  join, work_mem at the floor   39 compared,  39 identical,  0 changed,  0 regressions
```

Not "no regressions" — **no differences at all**. Every shape byte-identical,
across both arms, with a pipeline shown to detect a planted difference.

Nothing was dropped for fingerprint drift, so the databases being compared
really were the same database. Four schemas produced no measurement: gitlab
exceeded the seeding budget this test allows, hasura and temporal offer neither
a lookup nor a usable pair, and zitadel is seeded with no rows.

## What this does not show

**One version step.** 16.4 to 17.0. Nothing here is evidence about 15 to 16, or
17 to 18, and the planner changes in any particular release are the whole
question.

**Simple query shapes.** Single-column lookups and a two-table `LEFT JOIN
count(*)`. No grouped aggregates, window functions, partitioned tables, CTEs or
five-table joins — and those are where a planner change is most likely to show.
The result should be read as *these shapes are stable*, not *plans are stable*.

**The shape, not the plan.** Costs, widths and parallel worker counts are
discarded by design, so two plans differing only there are identical here. That
is correct for what this tool gates on and is a narrower claim than it sounds.

**pgseed's data, not the schema owner's.** Selectivity drives planner choice and
deterministic synthetic values do not have production's distribution.

## What it does support

For query shapes of this kind, a baseline taken before a major upgrade still
applies afterwards: it does not flap, and a finding after an upgrade would be a
finding about the query rather than about the version. That is the property the
manual ritual is trying to establish by hand, and it is the one thing here that
needed measuring rather than asserting.
