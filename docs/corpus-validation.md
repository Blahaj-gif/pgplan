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

The dropped result is split, because dropping an index does not always make a
plan worse:

- **a sequential scan was named** — the planner actually fell back to reading
  the table, and the table was over the row threshold. This required the plan to
  genuinely degrade.
- **moved to another index, reading no more** — the index went and the planner
  reached the same data through a different one without doing more work. Nothing
  can be shown to have got worse, so nothing is reported, and these are held out
  of the denominator rather than counted as failures to detect.

An earlier version of this page had a third row here, "only the missing index
was named", and called it weak evidence because it is true by construction the
moment an index is dropped. That was too kind to it. Those nine were not weak
evidence of a detection; they were false positives, and the tool was failing
builds for them. All nine are the row above.

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

index dropped, 76 indexes dropped
  the plan moved to another index and
    read no more — nothing to report ..  9
  of the 67 that did degrade:
    a sequential scan was named ...... 67  (100%)
    nothing was said .................  0

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
  could not be run at all ...........  0

reported when nothing got worse
  nothing changed at all ............  0
  re-analyzed, column added, index added .  0
```

Per schema. `swapped` is a drop the planner absorbed by moving to another index,
which is not a miss. `loop`, `spill` and `count` read *named / bit*; `--` means
the experiment never bit there, which is also not a miss.

| schema | queries | scan named | swapped | missed | fk pairs | loop | spill | count |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| camunda | 6 | 6 | 0 | 0 | 1 | -- | 1/1 | 6/6 |
| discourse | 5 | 5 | 0 | 0 | 2 | -- | 2/2 | 5/5 |
| documenso | 5 | 5 | 0 | 0 | 2 | 2/2 | 2/2 | 5/5 |
| gitlab | 4 | 3 | 1 | 0 | 2 | -- | -- | 3/3 |
| harbor | 2 | 2 | 0 | 0 | 0 | -- | -- | 2/2 |
| hexpm | 6 | 6 | 0 | 0 | 2 | 2/2 | 2/2 | 6/6 |
| hydra | 5 | 5 | 0 | 0 | 2 | 1/1 | 2/2 | 5/5 |
| kratos | 5 | 3 | 2 | 0 | 2 | 1/1 | 2/2 | 3/3 |
| langfuse | 5 | 5 | 0 | 0 | 2 | 1/1 | 2/2 | 5/5 |
| listmonk | 6 | 4 | 2 | 0 | 2 | -- | 2/2 | 4/4 |
| mattermost | 4 | 4 | 0 | 0 | 0 | -- | -- | 4/4 |
| plausible | 3 | 3 | 0 | 0 | 2 | -- | 2/2 | 3/3 |
| powerdns | 3 | 3 | 0 | 0 | 2 | -- | 2/2 | 3/3 |
| sourcegraph | 4 | 3 | 1 | 0 | 2 | 2/2 | 2/2 | 3/3 |
| sourcegraph_codeintel | 4 | 3 | 1 | 0 | 2 | -- | 2/2 | 3/3 |
| sourcegraph_insights | 5 | 4 | 1 | 0 | 2 | 2/2 | 2/2 | 4/4 |
| synapse | 4 | 3 | 1 | 0 | 2 | 1/1 | 2/2 | 3/3 |

Every degradation that actually occurred produced a finding. Nothing was
reported when nothing got worse.

The same nine appear in every experiment that drops an index, and they are the
same nine each time: 76 indexes dropped, 67 plans that degraded, 9 the planner
absorbed by reaching the data another way. 67 counts fell back to a sequential
scan and 67 were reported; the 9 that did not fall back are those nine. 67 + 9 =
76, three times over.

Both of the errors that number has been through are worth keeping in view,
because they point in opposite directions. The `counted` experiment first
reported "67 of 76" and it read as nine misses — too harsh. The dropped-index
experiment reported the same nine as findings — too lenient, and failing builds
for them. One denominator, wrong twice, in both directions, before anybody
looked at what the nine actually were.

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

**The spill experiment's denominator is not stable between runs.** Across three
runs of identical code against identical seeded data it bit 27, then 29, then 27
times out of 29. It is not errors or timeouts — the survey counts those
separately and reported zero. The likely cause is that `ANALYZE` samples rather
than reads every row, so column statistics differ a little between runs and the
planner's hash-batch decision moves with them; that is stated as the likely
cause rather than a demonstrated one. What did not move: pgplan named **every**
bite in all three runs, and every other figure on this page was identical in all
three. The denominator wobbles; the detection rate does not.

**Two schemas offered no foreign-key pair at all** with rows on both sides:
harbor and mattermost. Mattermost declares almost no foreign keys, which is a
fact about Mattermost rather than about this tool, and it is why the join
denominator is pairs rather than schemas.

**Seventeen of the 29 pairs never produced the loop shape**, so the nested-loop
result rests on 12. One of those 12 was on a pair whose referencing column *was*
indexed — the planner chose to scan it anyway — which is a reminder that the
index is a strong predictor of the shape and not a guarantee either way.

**`IndexNoLongerUsed` is now suppressed when the plan swapped indexes.** The
criterion is the tool's own: the plan reaches its data through an index it did
not have before, and reads no more rows than it did. That is not a proof the new
plan is *faster* — it is the absence of any demonstrable degradation, which is
the standard this whole project holds itself to and the reason the finding is
withheld. Nine of the corpus's dropped indexes fall in that bucket.

**`SortAppeared` fires on a proxy.** It requires a new sort and *some* table
reached through *some* index in the baseline; it cannot show that the index in
use was the one supplying that order, because sort keys are not reliably
qualified by table. The behaviour is the best available and is unchanged, but
the finding's message no longer claims the causal link it cannot demonstrate.

**The data is pgseed's, not the schema owner's.** Selectivity drives planner
choice, and a column filled with deterministic synthetic values does not have
production's distribution. This measures the pair — pgseed's data and pgplan's
judgement — on real *schemas*, not on real *data*.

## Six ways this survey was wrong before it was right

Each produced a plausible number that was wrong in a different direction, which
is the argument for not publishing the first result a harness gives you. Five of
the six share one shape: **an experiment that did not bite is indistinguishable
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

6. **"Could not run" was filed as "did not bite".** The join experiments read
   `if let Ok(plan) = plan_of(...)`, so a statement timeout and a planner that
   simply chose a different shape landed in the same bucket — in the code
   written to fix number 5, by the person who had just written number 5. It
   surfaced only because a figure moved between two runs with nothing to
   explain it, and the harness could not say which of the two had happened.
   Errors and timeouts are now counted and reported on their own line. They
   turned out to be zero, which means the wobble is the planner's and is
   described above; but that was worth knowing rather than assuming.

The harness now excludes constraint-backed indexes, verifies that each drop
actually happened, uses a wrap that bites on every candidate type, reports a
bite measure for each of the three experiments that can decline to bite,
separates "could not be run" from "did not bite", and prints every denominator
so that none of them is implied.

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

**Two more of the same species, found by probing rather than by reading.** The
review that caught the nested-loop rule had examined all six and pronounced four
of them sound. Those four were only *read*. Probed with constructed plans
afterwards, all four turned out to claim something they did not check, and two
were worth fixing immediately:

- **`MoreRowsPerRowReturned` said "for the same answer" without looking at the
  answer.** The ratio is rows read over rows returned, so it doubles just as
  readily when the denominator falls — and the denominator falls whenever the
  data shifts under a predicate, with the plan untouched and not one extra row
  read. The comparability fingerprint does not save you: it buckets table volume
  by order of magnitude, and a table can hold ten thousand rows while what
  matches a `WHERE` moves from a hundred to ten. It now also requires the total
  actually read to have doubled.
- **`HashJoinSpilled` said the baseline "fitted in memory" without checking
  there was a hash join in it.** `max_batches` starts at one and is only raised
  by a node reporting batches, so "one batch" and "no hash join anywhere" are
  the same number. A plan that *gained* a spilling hash join where the baseline
  had a nested loop was reported as one that had fitted in memory. It now
  requires a hash join in the baseline.

Neither cost a single true positive: every figure in the result above is
identical before and after. That is what a fix to an over-claim should look
like — it removes findings that were never earned and leaves the rest alone.

Fixing the second also corrected a fixture that had been quietly wrong. The unit
test wrote `Hash Batches` on the `Hash Join` node; a real planner puts it on the
`Hash` node below. The test passed anyway, which is a fixture agreeing with its
author inside the file arguing against exactly that.

**A third, and the plan for it that turned out to be wrong.**
`IndexNoLongerUsed` could not tell an index being *swapped for a better one*
from an index being lost. It is a set difference over index names, so a release
that adds a better index and lets the planner move to it loses the old name and
was reported as a regression — measured, a query going from 900 rows through a
broad index to 3 through a precise one. That is this gate arguing for the slower
plan, against the one promise it cannot break.

The intended fix was to make the shape know which table each index served, at
the cost of a third baseline format in a day. That was unnecessary, and noticing
so was worth more than the fix: the question is not *which table did this index
serve* but *did the plan come out worse*, and the shape already answers that. It
now withholds the finding when the plan gained an index it did not have and
reads no more rows than before — both halves required, since a plan can pick up
an index and still be reading far more, and a plan that simply stopped using one
also reads no more.

On the corpus this removed nine findings and cost none: every other figure on
this page is unchanged, including the wrapped experiment's 76 of 76. It also
corrected the headline in the other direction, because those nine had been
sitting in the denominator of the dropped-index result and understating it. 67
of 76 was never the number. 67 of 67 is.

**`SequentialScanAppeared` remains unfixed and is recorded.** It sums rows across
loops, so a forty-row table scanned forty times reads 1,600 and crosses a
threshold whose stated purpose is "the table is big enough for this to matter".
Both that rule and the nested-loop rule fire on such a plan, and only one of
them has the right story: the remedy `SequentialScanAppeared` implies is an
index, and the actual problem is the loop.

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
