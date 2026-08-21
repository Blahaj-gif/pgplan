# Contributing

The most useful contribution is **a new regression to the closed set** — one
more way a plan can be shown to have got worse.

It is also the easiest place to do harm, so this document is mostly about the
bar rather than the mechanics.

## The rule

> **Never fail a build for a plan change that cannot be shown to be worse.**

A gate that fails on innocent change gets switched off, and a switched-off gate
is worse than no gate because it also occupies the slot a working one would have
had. So every entry in `regress.rs` has to survive three questions:

1. **Is it provable from two shapes?** Not "looks slower" — a specific,
   mechanical difference. If you cannot decide it from the two `Shape` values
   without asking a human, it does not belong in the set.
2. **Can it fire when nothing got worse?** A sequential scan of sixty rows is
   the planner being *right*. If your rule has no threshold, find out what its
   threshold is before proposing it.
3. **What happens when the query got better?** Run that case. `regress.rs` has
   `a_plan_that_got_better_is_not_a_regression` for exactly this and yours needs
   the equivalent.

Anything that fails those is still welcome — as something **reported and not
failed**. That is a real category and it is where a rule should start.

## Adding one

1. **A variant on `Regression`** in [src/regress.rs](src/regress.rs), carrying
   what the message needs to name — the table, the index, the numbers. A
   finding that says "something regressed" sends somebody hunting.
2. **`explain()`**, one or two sentences: what got worse, and the likely cause.
   The reader is looking at a red build and wants to know where to go next.
3. **The test in `compare`**, with its threshold as a named constant beside
   `SEQ_SCAN_ROWS` and a comment saying how the number was chosen.
4. **Whatever `Shape` needs to hold**, in [src/shape.rs](src/shape.rs). Keep it
   lossy: costs, row *estimates* and widths are deliberately not stored, because
   none of them is comparable between two machines.

   **But the shape has to be able to prove what your message says.** That is the
   harder half of this step and it is where the worst bug in this file's history
   came from. `NestedLoopOverSequentialScan` said "on its inner side" in a
   message while testing, from a flat summary, that a `Nested Loop` existed
   somewhere and that some table was scanned somewhere — two facts usually about
   different halves of the tree. It missed the case it was named for and fired
   on a table no loop had touched.

   Tree structure is therefore *mostly* not stored rather than never stored.
   `inner_loop_rows` is the one exception, it is one flag deep, and it earned
   its place by being the thing a rule could not do without. If your rule needs
   another, the bar is the same: name the claim, show the summary cannot carry
   it, and add the smallest fact that can. Do not soften the message instead —
   or if you do, soften it honestly, the way `SortAppeared` now states that the
   sort is new and offers the lost index as the usual cause rather than
   asserting a link it cannot demonstrate.

## What it ships with

**A test that provokes it against a real planner**, in `tests/regressions.rs` —
not hand-written JSON, and asserting your variant *by name* rather than that
something was found. Two of the six once asserted only that the finding list was
non-empty, which passes when a different rule fires and says nothing about
yours.

**And its silence.** Every rule needs a case where the same provocation is
applied to a schema that does not deserve the finding — a forced nested loop
over a foreign key somebody *did* index, a sequential scan of a small table.
The catches are what the rule is for; the silences are what decide whether
anybody leaves it switched on.

Three bugs got through review here and were caught only by a live server:

- a filtered `Seq Scan` reports the rows it **kept**, with the discarded ones in
  `Rows Removed by Filter`, so a full table scan returning ten rows looked like
  a ten-row lookup;
- a bitmap plan reports its rows **twice**, once on the index scan and once on
  the heap scan above it, which made *adding an index* register as a two-fold
  regression.

- and the planner will not simply do as it is told. A fixture built to force a
  loop over an unindexed table instead had the child put on the *outside* with a
  memoized primary-key lookup on the inside, which is the planner being right
  and the test measuring nothing. A `LEFT JOIN` pins the nullable side inward.

None was reachable from a fixture: the first two are facts about what Postgres
emits rather than what the docs say it emits, and the third is a fact about what
it chooses.

**And a stability case.** If your rule can be affected by `ANALYZE`, by ordinary
row growth, or by running twice, `tests/stability.rs` is where that gets proved
one way or the other.

## Running it

```
cargo test                       parsers, shapes, the closed set — no database
cargo test -- --test-threads=1   everything, against a real Postgres
```

Integration tests download and start their own Postgres through
`postgresql_embedded`. No Docker, no install, and no version drift between CI
and a laptop. The first run downloads a server and takes a few minutes; the
timeout in `tests/harness.rs` is set for that and says so.

`cargo fmt --check` and `cargo clippy --all-targets` both have to be clean — CI
runs clippy with `-D warnings` against a pinned compiler, so a lint that arrives
with a new release gets fixed in a commit somebody reviewed rather than in
whatever pull request happens to be open.

## What this is not

Not a plan visualiser, not a cost model, not a query rewriter, and not a MySQL
tool. Each is a different product, and the correctness argument here rests
entirely on comparing shapes at fixed data volume against one database whose
planner output has been read closely.
