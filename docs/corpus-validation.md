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

Eight experiments. Two must report nothing, six must report something.

| experiment | change | required verdict |
|---|---|---|
| **unchanged** | nothing at all | no finding |
| **benign** | re-`ANALYZE`; add a column; add an index on another column | no finding |
| **dropped** | drop the index the query was using | a finding |
| **wrapped** | `WHERE col::text \|\| '' = …`, which the index cannot serve | a finding |
| **unordered** | `ORDER BY` an index was supplying, index dropped | a `Sort` named |
| **counted** | `count(*)` through the index, index then dropped | `MoreRowsPerRowReturned` |
| **looped** | a join, with hash and merge joins switched off | `NestedLoopOverSequentialScan` |
| **cramped** | the same join, with `work_mem` at its 64kB floor | `HashJoinSpilled` |

The last three are new. They exist because the first five are all single-table
lookups, while three of the six named regressions are about joins or about
ratios — so a survey that only issues lookups can say nothing about them, and
saying nothing was the honest previous position.

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

## Every experiment reports what it could not do

Three of the eight cannot be relied on to happen merely because they were asked
for. Dropping an index does not oblige the planner to start reading the table.
Switching off two join methods does not oblige it to put a sequential scan on a
loop's inner side. Cramping `work_mem` does not oblige a hash join to spill —
the planner is entitled to pick a different join instead.

So each of those three reports **tried**, **bit** and **named** separately, and
only the ones that bit are a denominator. This is not fussiness. Four of the
five ways this survey was wrong before it was right were one mistake in
different clothes, and the mistake is that **an experiment that did not bite is
indistinguishable from a tool that missed something, unless you check.**

What the two join bite measures are worth, stated rather than implied: both
share a condition with the rule they check — a spill *is* batches going from one
to more than one — so neither is a strong test of the rule's definition. What
they are strong evidence of is that the shape occurs on schemas nobody here
designed, that the tool names it every time it occurs, and that it stays quiet
on the pairs where somebody had indexed the foreign key. The silences are the
half that is not near-circular, and the silences are what decide whether a gate
survives its first month.

## Joins, and why two of these are provoked with planner flags

Two tables and a foreign key between them, found by asking the schema rather
than by choosing: single-column `contype = 'f'`, both sides holding rows, and
preferring the pairs whose referencing column nobody indexed.

That preference is the point. **Postgres creates an index for a primary key and
none at all for a foreign key.** Of the 29 usable pairs here, asked of the live
catalogue after seeding rather than counted out of the DDL, **15 have no index
on the referencing column**. A nested loop that lands on that side reads the
whole table once per outer row, and nobody has to have made a mistake for that
to happen — one row estimate has to collapse.

A wider count across all of the corpus's foreign keys is deliberately not
offered. The only cheap way to get one is a regex over the DDL text, and that
cannot tell `user_id` on one table from `user_id` on another, so it produces a
number that looks measured and is not. 15 of 29 is what this survey can stand
behind.

The estimate is not what is manipulated here. Inducing a bad estimate on demand
across twenty-four unfamiliar schemas is its own research project, so the join
methods are switched off instead, which arrives at the same plan by a different
route. `work_mem` is treated the same way: the floor is set directly rather than
waiting for a table to grow into it.

**What that does and does not establish.** It establishes that pgplan recognises
the shape when the shape is in front of it, on a schema nobody here designed, at
volume. It establishes nothing about how often the shape arises, and this survey
should not be read as claiming otherwise. The `wrapped` experiment needs no such
caveat — nothing is switched off there — which is why it remains the one closest
to a real incident.

The parent goes on the left of a `LEFT JOIN` deliberately. Without that the
planner is free to swap the sides, and on these schemas it does: it puts the
child on the outside and memoizes a primary-key lookup on the inside, which is
the planner being right and the experiment measuring nothing. A `LEFT JOIN` pins
the nullable side to the inside, and it is also the more common query.

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

count(*) through an index that was then dropped, 76 tried
  the count fell back to a scan ..... 67
  read far more for the same answer . 67  (100% of the ones that fell back)

joins, from 29 foreign-key pairs with rows on both sides
  15 had no index on the referencing column
  forced onto a nested loop ......... 12 of 29 put a sequential scan on the
                                      inner side; all 12 were named
  work_mem at the floor ............. 27 of 29 actually spilled;
                                      all 27 were named

reported when nothing got worse
  nothing changed at all ............  0
  re-analyzed, column added, index added .  0
```

Per schema. `loop`, `spill` and `count` read *named / bit*; `--` means the
experiment never bit there, which is not a miss.

| schema | queries | scan named | index only | fk pairs | loop | spill | count |
|---|---:|---:|---:|---:|---:|---:|---:|
| camunda | 6 | 6 | 0 | 1 | -- | 1/1 | 6/6 |
| discourse | 5 | 5 | 0 | 2 | -- | 2/2 | 5/5 |
| documenso | 5 | 5 | 0 | 2 | 2/2 | 2/2 | 5/5 |
| gitlab | 4 | 3 | 1 | 2 | -- | -- | 3/3 |
| harbor | 2 | 2 | 0 | 0 | -- | -- | 2/2 |
| hexpm | 6 | 6 | 0 | 2 | 2/2 | 2/2 | 6/6 |
| hydra | 5 | 5 | 0 | 2 | 1/1 | 2/2 | 5/5 |
| kratos | 5 | 3 | 2 | 2 | 1/1 | 2/2 | 3/3 |
| langfuse | 5 | 5 | 0 | 2 | 1/1 | 2/2 | 5/5 |
| listmonk | 6 | 4 | 2 | 2 | -- | 2/2 | 4/4 |
| mattermost | 4 | 4 | 0 | 0 | -- | -- | 4/4 |
| plausible | 3 | 3 | 0 | 2 | -- | 2/2 | 3/3 |
| powerdns | 3 | 3 | 0 | 2 | -- | 2/2 | 3/3 |
| sourcegraph | 4 | 3 | 1 | 2 | 2/2 | 2/2 | 3/3 |
| sourcegraph_codeintel | 4 | 3 | 1 | 2 | -- | 2/2 | 3/3 |
| sourcegraph_insights | 5 | 4 | 1 | 2 | 2/2 | 2/2 | 4/4 |
| synapse | 4 | 3 | 1 | 2 | 1/1 | 2/2 | 3/3 |

Every degradation that actually occurred produced a finding. Nothing was
reported when nothing got worse.

The two halves of the `counted` experiment line up exactly, which is worth
noticing: 67 counts fell back to a sequential scan and 67 were reported, and the
9 that did not fall back are the same 9 where the dropped-index experiment named
only the index. 67 + 9 = 76. The first run of that experiment reported "67 of
76" and it read as nine misses. There were none.

## What this does not show

Stated because a survey that reports only its coverage reports the coverage of
the easy cases as if it were the coverage of all of them.

**Seven of twenty-four schemas produced no measurement.** They offer no
non-unique single-column index on an ordinary scalar type, so there was no
lookup of the shape an application issues: hasura, kong, lago, postgrest,
temporal, vaultwarden, zitadel.

That exclusion is wider than it looks, because seeding is driven by the lookup
search: a schema with no ordinary lookup index is never seeded, so **its joins
are never reached either**. postgrest is the sharpest case — all thirteen of its
foreign-key columns are unindexed, which is exactly the material the loop
experiment wants, and none of it is measured.

**Seventeen of ninety-three candidates were discarded before the experiment**,
because the planner did not route them through the index even with it present.
A query already being scanned sequentially cannot regress that way, and counting
it would have flattered the result.

**Two schemas offered no foreign-key pair at all** with rows on both sides:
harbor and mattermost. Mattermost declares almost no foreign keys, which is a
fact about Mattermost rather than about this tool, and it is why the join
denominator is pairs rather than schemas.

**Seventeen of the 29 pairs never produced the loop shape**, so the nested-loop
result rests on 12. One of those 12 was on a pair whose referencing column *was*
indexed — the planner chose to scan it anyway — which is a reminder that the
index is a strong predictor of the shape and not a guarantee either way.

**`SortAppeared` fires on a proxy.** It requires a new sort and *some* table
reached through *some* index in the baseline; it cannot show that the index in
use was the one supplying that order, because sort keys are not reliably
qualified by table. The behaviour is the best available and is unchanged, but
the finding's message no longer claims the causal link it cannot demonstrate.

**The data is pgseed's, not the schema owner's.** Selectivity drives planner
choice, and a column filled with deterministic synthetic values does not have
production's distribution. This measures the pair — pgseed's data and pgplan's
judgement — on real *schemas*, not on real *data*.

## Five ways this survey was wrong before it was right

Each produced a plausible number that was wrong in a different direction, which
is the argument for not publishing the first result a harness gives you. Four of
the five share one shape: **an experiment that did not bite is indistinguishable
from a tool that missed something**, unless you check.

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

5. **A ratio experiment with no bite measure.** The first run of `counted`
   reported a fraction of *attempts*, and dropping an index does not oblige the
   planner to read the table. It scored 67 of 76 and read as nine misses; with a
   bite measure the same run is 67 of 67. The same reasoning, applied *before*
   the fact this time, is why `looped` and `cramped` shipped with bite measures
   from their first run — and applied to this harness's own assertions, it is
   why a loop whose inner scan falls under the rule's row floor is not counted
   as a bite. Without that, this survey would fail a build for pgplan being
   right about a small table, which is the failure the whole project exists to
   avoid, arriving from inside its own evidence.

The harness now excludes constraint-backed indexes, verifies that each drop
actually happened, uses a wrap that bites on every candidate type, reports a
bite measure for each of the three experiments that can decline to bite, and
prints every denominator so that none of them is implied.

## What this survey found in pgplan

Worth recording separately from what it found in pgseed, because this one was
found by *writing* the harness rather than by running it. The rule could not be
exercised on a real schema until it was correct, and it was not correct.

**`NestedLoopOverSequentialScan` was inverted in both directions at once.**

- It required a `Nested Loop` to be present in the **baseline**, so a plan that
  acquired one — the ordinary way this degradation arrives — was excluded by
  construction. A hash join becoming a nested loop over a nine-million-row
  sequential scan produced no finding of that name. Its own doc comment said
  "only when it is new", which is what the code did not do.
- It tested "a `Nested Loop` exists somewhere" against "some table is scanned
  sequentially somewhere", two facts usually about different halves of the tree.
  A loop reaching both its sides through indexes, beside an unrelated table that
  had grown, produced a finding naming a table no loop had ever touched.

One cause for both, and it was structural: a `Shape` was flat, so the rule could
not state where in the tree anything was — while its own message said "on its
inner side". The shape now records which tables are scanned sequentially beneath
a nested loop's inner side, one structural fact kept because a rule needs it,
and the rule is stated in terms of it. Both directions are unit tests, and both
are provoked against a real planner in `tests/regressions.rs`: one where the
foreign key is unindexed and the finding must appear, one where somebody indexed
it and the finding must not.

The baseline file is version 2 as a result. A version-1 file still parses — the
new field defaults — and that is exactly the hazard, because it would then
compare cleanly while answering a different question. It is refused with an
instruction instead.

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
