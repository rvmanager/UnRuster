# unruster — field findings, Augrym audit session, 13 Sep 2026

**Status:** **D1–D4 fixed on 2026-09-13** (working tree; not yet in the PATH
binary). See *Outcome* at the foot of this file. Everything else stands as
written.
**Baseline:** `0.1.92` — the binary on PATH (`/usr/local/bin/unruster`, installed
12 Sep 08:11) is the current `HEAD` (`41a70a7`). Every finding below is against
shipping code, not a stale build.

**Evidence:** one session — `69c72fab` "Grade level settings UI", project
`/Users/alois/projects/Augrym`, read with `lsc`. Scoped to prompt #67
(*"I commited. No run unruster and fix any other audit findings"*, t+9:34:15)
through the end of the transcript. In that window: **39 unruster invocations
across 30 Bash calls, 0 failed**, ~9 min of the session's 11 min total inside
the tool.

| subcommand | calls | | subcommand | calls |
|---|---|---|---|---|
| `audit` | 13 | | `concepts` | 1 |
| `show` | 4 | | `casts` | 1 |
| `dead-code` | 3 | | `config-drift` | 1 |
| `metrics` | 3 | | `builder-drift` | 1 |
| `callers`, `tests`, `error-swallows`, `arith-drift`, `outline` | 2 each | | `near-clones`, `stringly` | 1 each |

`waivers`, `explain`, `playbook`, `blind-spots`, `at`, `--changed-since`,
`--only`, `--gating-only` and `--list-checks`: **zero uses**, despite every
footer pointing at `explain:` and `blind-spots`.

## What the tool actually bought

Three rounds. Round 1 (the gate): **13 gating findings → 5 real fixes, 8
waivers.** 38% precision.

| check | rows | real | waived |
|---|---|---|---|
| `dead-code` | 2 | 2 | 0 |
| `error-swallows` | 5 | 2 | 3 |
| `near-clones` | 1 | 1 | 0 |
| `concepts` | 5 | **0** | **5** |

The best find was real and expensive: `Foreman/src/housekeeping.rs:90` discarded
`pg_advisory_unlock`'s result, so a failed release strands the lock on a pooled
connection and every later sweep returns immediately — housekeeping stops for
good, silently. `error-swallows` scored it 0.90 and led with it. That one row
justifies the battery.

Round 2 (the advisory pass, at the user's prompt): **169 advisory findings → 3
real bugs.** All three were *outside* the gate:

- `user_service::ascend` — `match table { "plot_machines" => …, "plot_blocks" => …, _ => "DELETE FROM ore_balances …" }`. A fourth table added to the list compiles and deletes from the wrong table. Found via `stringly` (advisory/medium).
- `zone_service::looks` — the same swallow just fixed in its sibling `display_name`, at score **0.50**, one notch under the 0.55 gate.
- `specs::coordinates::plane` — `(reach as i32 + 7).clamp(8, 24)`: narrowed before clamping, so a truncated value lands inside the clamp. Found via `casts` narrow-int (advisory/medium).

So on this codebase the gate's precision was 38% and it held back every bug the
second pass found. That is the frame for the findings below.

---

# Tier 1 — defects

## D1. `dead-code --transitive`'s orphan closure stops at any call cycle

**Effort:** M · **Risk:** low · **Reproduced**

### Evidence
Session call #196: `dead-code --transitive` on the tree with
`Toolbelt/aug-problems/src/generators.rs` restored reports exactly two rows,
both `direct`, and a footer with no "N of them only once the others are gone"
clause — i.e. deleting `dispatch` orphans nothing:

```
fn  pub  …::generators::dispatch     …/generators.rs:70   direct
fn  pub  …::stencil::engine::all_specs …/engine.rs:225     direct
(2 candidate dead fn(s); …)
```

Call #197 deletes the file. `dead-code` immediately reports a *new* row —
`Toolbelt::stencil::rng::Rng::chance` — that `--transitive` had just said would
not appear. Gating count stayed at 13 across a deletion that was supposed to
clear it.

The agent explained this as "the orphans deletion would expose live in the same
file", which is **not** the cause — same-file private orphans are reported
correctly. A call cycle is:

```rust
// src/generators.rs — two private helpers that reference each other
fn gen_add(r: &mut Rng, d: u8) -> u32 { if d > 0 { gen_sub(r, d-1) } else if r.chance(3) { 1 } else { 2 } }
fn gen_sub(r: &mut Rng, d: u8) -> u32 { if d > 0 { gen_add(r, d-1) } else { 4 } }
pub fn dispatch(r: &mut Rng, w: u8) -> u32 { if w == 0 { gen_add(r, 2) } else { gen_sub(r, 2) } }
```

```
$ unruster dead-code --transitive
fn  pub  generators::dispatch  src/generators.rs:7  direct
(1 candidate dead fn(s); …)

$ rm src/generators.rs && unruster dead-code
impl-fn  pub  rng::Rng::chance  src/rng.rs:4
```

Break the cycle (`gen_add`/`gen_sub` no longer call each other) and the same
tree correctly reports 5 rows, 4 of them transitive, `Rng::chance` and
`Rng::next_u64` included. The closure is "remove item, recount items with zero
remaining callers, repeat" — a two-node cycle keeps itself alive at every
iteration, so the whole subgraph under it, and everything it alone calls, is
invisible. Augrym's ~40 private generator fns are exactly that shape.

### Change
Compute the closure over strongly-connected components, not single items: after
condensing cycles, a component whose only in-edges come from already-removed
items is itself an orphan. Emit its members with `transitive after <entry>` as
today.

### Acceptance
The cycle repro above reports 5 candidates (`dispatch` direct; `gen_add`,
`gen_sub`, `Rng::chance`, `Rng::next_u64` transitive) and the footer says
"4 of them only once the others are gone". `dead-code` after the deletion adds
no row that `--transitive` did not predict.

---

## D2. `dead-code` withholds the import/re-export note that `callers` already prints

**Effort:** S · **Risk:** low · **Reproduced**

### Evidence
Calls #218–#220. The agent deleted `stencil::engine::all_specs` on `dead-code`'s
say-so. The build broke: `Toolbelt/stencil/src/lib.rs:100` re-exported it
(`pub use engine::{Failure, all_specs, …}`). Recovery cost a `grep`, a `sed -i`
and a 2 min 43 s `cargo test --workspace` re-run.

`callers` already knows this and says so; `dead-code` — the command that exists
to tell you what to delete — does not:

```
$ unruster callers all_specs
(0 call site(s) across 0 caller(s))
(note: 1 file(s) also import `all_specs` by name. A rename or a removal has to
 touch those `use` lines too, and they are not call sites —
 `callers all_specs --with-imports` lists them)

$ unruster dead-code
fn  pub  engine::all_specs  src/engine.rs:2
(note: `tests --mentions all_specs` lists the tests that name it — …)
```

The note `dead-code` *does* carry points at `tests --mentions`, which found
nothing here (`0 of 196 test fn(s)`), so the one note it printed was the one
that did not matter.

### Change
Add an `imports` column, or a per-row note, to `dead-code`: `also imported in
N file(s) — `callers <name> --with-imports``. The import index is already built.
A re-export is the single highest-frequency reason a "dead" `pub fn` cannot just
be deleted.

### Acceptance
`dead-code` on the two-file repro (`src/engine.rs` + a `pub use` in `lib.rs`)
names the re-export on or beside the row.

---

## D3. A waiver split from its date by a blank line still suppresses, and nothing in the digest says so

**Effort:** S · **Risk:** low · **Reproduced**

### Evidence
Calls #222–#223 wrote seven waivers with a regex whose `^(\s*)` ate the
preceding newline, landing a blank line between the key and the date:

```rust
// unruster: ok(concepts/signature:simple)

// 2026-09-13 — one fn per topic returning Spec is the engine's whole shape …

pub fn simple_average() -> Spec {
```

`is_continuation` (src/suppress.rs:852) returns `false` for a blank line, so the
head parses with `date: None, reason: ""` — but `is_codeless` skips blanks when
choosing the covered item, so the waiver still lands on `simple_average` and
still suppresses. `audit` went from 13 gating to **0** on waivers that carry no
date and no reason.

The digest was byte-identical before and after the agent redid them properly
(calls #223 vs #238):

```
… 15 waiver(s) hiding 10 finding(s), 5 suppressing only below audit's thresholds)
```

`audit` *does* emit `note: N // unruster: ok waiver(s) carry no reason …` — but
as a separate line **after** the digest. The agent's command was
`unruster audit 2>&1 | grep -E "^\(audit:"`, which is how the digest is meant to
be consumed, and it dropped the note. The agent found the problem by eye in
`git diff`, not from the tool.

The global rule is "waivers are never hand-written — use `--suggest-waivers`",
and this was `--suggest-waivers` output re-inserted by a script. Seven waivers
suppressing a gate with nothing a reviewer can evaluate is the exact failure
`suppress.rs:687` already documents from a previous ledger.

### Change
- Fold waiver health into the digest line itself, where the counts already live:
  `… 15 waiver(s) hiding 10 finding(s), **7 undated, 7 unexplained**, 5 suppressing only below audit's thresholds`.
- Name the command: the digest reports waiver counts and never mentions
  `unruster waivers`, which prints `1 undated` and the empty reason column
  immediately. Zero uses in this session.
- Consider making an unexplained waiver non-suppressing under `--strict`, or at
  minimum exit 1 on one: a waiver nobody can evaluate is the tool's own stated
  worst case, and today it turns a red tree green in silence.

### Acceptance
On the two-waiver repro (one blank-line-split, one adjacent),
`unruster audit | grep '^(audit:'` names the undated/unexplained count.

---

## D4. `error-swallows` ranks `.unwrap_or_default` below `.ok`, against its own axis

**Effort:** S · **Risk:** medium (re-tunes a gate) · **Verified in source**

### Evidence
Two methods, same `impl ZoneServiceImpl`, same failure, same `io` effect:

| site | kind | score | gated? | outcome |
|---|---|---|---|---|
| `display_name` zone_service.rs:39 | `.ok` | **0.55** | yes | fixed in round 1 |
| `looks` zone_service.rs:89 | `.unwrap_or_default` | **0.50** | no | found in round 2, by hand |

`Hit::score` (src/error_swallows.rs:50) weights `.ok` at 0.20 and
`.unwrap_or_default` at 0.15, so the only thing separating them is one notch of
kind. The doc comment for that axis says it measures *"how completely the failure
vanished"* — and by that measure the ordering is backwards: `.ok()` hands the
caller an `Option` it can still branch on; `.unwrap_or_default()` hands back a
value indistinguishable from success. The row comments agree with the code, not
with the axis: "*the failure becomes a `None` the caller may or may not check*"
(0.20) is strictly less complete a vanishing than "*a substituted value:
execution continues as if it had succeeded*" (0.15).

The cost was the classic divergence: the agent's own round-1 fix made
`display_name` warn and left its sibling `looks` silent, so one outage would
report the names and say nothing about the appearances. It only surfaced because
the user asked for a second pass.

### Change
Reorder the kind weights to match the stated axis — `.unwrap_or_default` /
`.unwrap_or_else` / `.unwrap_or` at or above `.ok` / `.err` / `if-let-ok` /
`while-let-ok`. The `substitution` term already handles the "substituted a
*different* value" case and is orthogonal to this.

### Acceptance
A fixture with `.ok()` and `.unwrap_or_default()` on the same `io` receiver in
sibling fns scores them equal or with the latter higher, and both land on the
same side of the 0.55 gate.

---

# Tier 2 — `audit` and the standalone commands answer different questions

`rerun_cmd` (src/audit.rs:159) exists, is exact, and is interpolated from the
constants each section passes — the design is right. It just does not reach the
reader in the cases that matter.

## S1. The rerun command is printed only when a section was capped

**Effort:** S · **Risk:** none

### Evidence
`Out::cap_note` (src/emit.rs:684) returns `None` when `dropped == 0`. Every
section with fewer rows than the cap therefore prints no rerun command, and the
reader types the bare subcommand. In this session that happened twice, both
times silently changing the question:

- `arith-drift`: audit section reports `min_score=0.60`, 2 rows, uncapped → no hint → call #250 ran bare `unruster arith-drift` → footer says `min_score=0.50`, no disclosure of the gap.
- `casts`: audit section reports 4 rows (`narrow-int=1, signed-flip=3`), uncapped → no hint → call #251 ran bare `unruster casts` → 25 rows of `int-float` and `other`, mostly `Level`/`Topic`/`Quarry` → `i32` enum discriminant casts that audit does not consider at all.

### Change
Print the section's rerun command unconditionally in `audit --full` and
whenever the section's arguments differ from the standalone defaults — i.e.
whenever `rerun_cmd(check) != check`. It is one line per section and it is the
difference between "here are the other rows" and "here is the same question".

---

## S2. `threshold_note` covers 3 of the ~11 sections whose defaults differ

**Effort:** S · **Risk:** none · **Verified in source**

### Evidence
`AnalysisCtx::threshold_note` (src/context.rs:92) prints
`min_score=0.05 (audit gates at 0.40)` and its doc names the exact problem it
solves. `unruster callers threshold_note` returns four call sites in three
modules: `builder_drift`, `config_drift`, `divergence`. Meanwhile `rerun_cmd`
lists eleven sections that run with non-default arguments, including
`arith-drift`, `validation-drift`, `error-swallows`, `panics`, `enum-coverage`,
`casts`, `metrics`, `metrics-params`.

Side by side in this session:

```
config-drift  →  (… min_score=0.05 (audit gates at 0.12); …)     ✓
arith-drift   →  (… min_score=0.50; explain: divergence)          ✗  audit uses 0.60
```

### Change
Call `threshold_note` from every check whose standalone default differs from
its audit constant, and extend it to non-score filters (`casts --class`,
`error-swallows --hide-*`, `metrics --threshold`). `self_check` can assert that
each `rerun_cmd` arm has a matching disclosure.

---

## S3. `casts` has no `--class` pointer at all

**Effort:** S · **Risk:** none · **Reproduced**

### Evidence
On unruster's own tree the two views do not overlap by a single row:

```
$ unruster casts | tail -2
(46 cast(s); other=28, unknown=16, usize-cross=2; hide_widen=false; 2 waived; explain: casts)

$ unruster audit --only casts | grep '^(audit:'
(audit: 0 gating + 0 advisory finding(s) …)
```

46 rows versus zero findings, and nothing in the standalone footer says audit
gates on `narrow-int` and `signed-flip` only, or that `--class` exists. The
section header (`data-loss classes only`) carries the fact; the command the
reader runs next does not.

### Change
Footer: `(46 cast(s); … ; audit reports the data-loss classes only —
`casts --class narrow-int,signed-flip,float-int,narrow-float,ptr`)`. `rerun_cmd`
already builds that exact string from `CAST_CLASSES` — `audit --list-checks`
prints it today. (Corrected: the section runs five classes; `narrow-int` and
`signed-flip` were merely the two with rows on Augrym.)

---

## S4. `audit` prints source context under `stringly`; the command it names does not

**Effort:** S · **Risk:** none · **Reproduced**

### Evidence
`audit` sets `set_context_lines(Some(CONTEXT_LINES))` for `stringly` and
`conversion-pairs` (src/audit.rs:1058) — the standalone commands default to
none. So audit's rows come with ±2 source lines and the standalone's do not,
and the cap note under the contexted section points at the uncontexted command.
Call #262 grepped `stringly --top 0` with `^\s*(cmp-eq|match-lit)` to strip
context lines that were never going to be there — the agent had learned the
shape from the audit output.

Two further costs, visible on unruster's own tree:

- Context lines are **not** tab-prefixed while rows are, so `awk -F'\t'` over an audit digest silently ingests them as data.
- Two literals on one source line print the same block twice: `cmp-eq "test"` and `cmp-eq "bench"` at `src/ast.rs:625` emit 10 context lines for one line of code. Augrym had the same shape at `plain.rs:75`/`:76`.

### Change
Either give the standalone `stringly` / `conversion-pairs` the same default, or
drop it from audit. Whichever way, collapse repeated context: one block per
distinct `file:line`, with the row list above it.

---

# Tier 3 — signal quality

## Q1. `concepts/signature` gated 5 rows on this codebase and all 5 were false positives

**Effort:** M · **Risk:** medium · **Diagnosed in source**

### Evidence
All five gating `concepts` rows cluster on `() -> Spec` — the return type of
**139 spec functions**, one per topic, which is the stencil engine's entire
shape. The clusters are name-word groups: *unit* (`unit_circle`, `unit_fraction`,
`unit_conversion`, `unit_rate`), *add*, *simple*, *function*. They scored 0.72 –
0.83 against a 0.70 gate. Five hand-written reasons, five waivers, zero bugs.

`signature_rarity` (src/concepts.rs:682) exists precisely for this and is
computed correctly — population 139 gives ≈0.007. But its own sibling doc
(`is_accessor_shape`, src/concepts.rs:705) already measured why it cannot help:
*"its entire authority is 0.06 of a score whose floor is 0.28 — while spread …
adds 0.16"*. The accessor case was solved by a **hard skip**, and that skip is
deliberately restricted to `SCALAR_RETURNS`, on the reasoning that *"a nullary
method returning a domain type is an interface somebody designed, and stays
clustered"*. `() -> Spec` is a nullary method returning a domain type worn by
139 functions, so it is exempt from the one mechanism that works.

`cognate_partition` is the amplifier: it splits the 139-member signature family
into 4- and 5-member word groups *before* scoring, so the `TAXONOMY_SIZE`
demotion (6+) never fires either. The check is structurally unable to see a
large family, by two independent routes.

### Change
Hard-skip (or demote past the gate) any `signature` cluster whose signature's
tree-wide population exceeds a threshold — 20 is far above "an interface
somebody designed twice" and far below 139 — regardless of return type. This is
what `signature_rarity`'s doc says the check needs ("rarity has to be measured
before the split") and what `is_accessor_shape` proves is the effective shape of
the fix.

### Acceptance
A fixture with 30 `pub fn x() -> DomainType` split across four name words
produces no gating `concepts` row; a fixture with three functions sharing a
distinctive 3-parameter signature still does.

---

## Q2. `stringly` cannot see the catch-all that makes a literal list a lie

**Effort:** M · **Risk:** low

### Evidence
The best bug of round 2:

```rust
for table in ["plot_machines", "plot_blocks", "ore_balances"] {
    let sql = match table {
        "plot_machines" => "DELETE FROM plot_machines WHERE owner_id = $1",
        "plot_blocks"   => "DELETE FROM plot_blocks WHERE owner_id = $1",
        _               => "DELETE FROM ore_balances WHERE user_id = $1",
    };
```

`stringly --top 0` printed two rows — `match-lit "plot_machines"` at :292 and
`match-lit "plot_blocks"` at :293 — and nothing for :294. **The finding is the
row that is absent.** A fourth table added to the array compiles, passes, and
deletes from `ore_balances`, on the path that wipes a player's plot. The agent
caught it by reading the source around the rows.

`enum-coverage`, `catch-all-arms` and `parallel-matches` all cover this failure
mode, and all three are enum-only. A string `match` is exactly where the
compiler is *not* helping, which is the check's own sales pitch ("candidate for
an enum or newtype so the compiler catches typos and missing cases").

### Change
Group `match-lit` hits by their enclosing `syn::ExprMatch` and emit one row per
match, with the arm count and a `catch-all` flag: `match-lit 3-arm +wild
user_service.rs:291`. Rank a literal match with a `_` arm above one without —
the wildcard is what converts "add a case, get a compile error" into "add a
case, get silent data loss".

---

## Q3. `divergence --handling` made zero comparisons across 29 callees

**Effort:** M · **Risk:** low · **Needs investigation**

### Evidence
`audit --full` (call #249):

```
## [high] divergence --handling — one callee, different care; gating: every row
(0 careless site(s) across 29 callee(s); 0 sibling comparison(s); min_care_gap=2; …)
```

The check is billed as *"highest-yield check in the tool; start here"*. It found
29 multi-site callees and compared **none** of them, on a workspace where the
`display_name` / `looks` pair (same `impl`, same failure, `.ok` versus
`.unwrap_or_default` — a care gap of exactly 2) was sitting there and had to be
found by hand from an advisory row four checks away.

The sibling key requires an identical callee, and two sqlx queries in one impl
call `fetch_optional` and `fetch_all`. That may be correct by design, but "29
callees, 0 comparisons" means the section contributed nothing and said so in a
way that reads as *clean*.

### Change
Worth an hour of measurement before any change: instrument why the sibling
predicate rejects all 29, and report it (`0 sibling comparison(s) — no two call
sites of one callee shared a scope or a name word`). If the answer is that
scope-siblings with *different* callees of the same family (`sqlx::query*`) are
the real population, widen the key to the callee's path prefix behind a flag.

---

# Tier 4 — friction

## F1. Three full audit runs to read three sections

Calls #199, #200, #201 — 35.9 s + 19.0 s + 26.9 s, each re-running the whole
21-check battery, to read `dead-code`, then `error-swallows`+`panics`, then
`near-clones`+`concepts`:

```
unruster audit 2>&1 | grep -E "^## \[" | head -30
unruster audit 2>&1 | sed -n '/## \[high\] dead-code/,/## \[high\] panics/p' | head -40
unruster audit 2>&1 | sed -n '/## \[high\] panics/,/## \[medium\] config-drift/p' | head -50
```

`--only` does exactly this in one run and was never used. It appears in
`--help` and nowhere in the output: the header note names `--full` and
`--top 0`, the digest names `--full`, the cap notes name the rerun command —
none names `--only` or `--gating-only`, and the `## [high] <check>` headers do
not say that `<check>` is the `--only` token. One line in the header note would
have saved two full batteries.

## F2. `--changed-since` is the documented workflow and was never used

The global CLAUDE.md says *"`unruster audit --changed-since HEAD~1` to scope it
to recent work — do this after a non-trivial change"*. The agent **quoted that
rule back to the user** at reply #66 and then ran bare `audit` thirteen times.
`audit`'s own output never suggests it. A one-line note when the tree has
uncommitted or recently-committed changes (`--changed-since HEAD` scopes this to
what you just touched`) would land it where it is read.

## F3. `metrics --sort cyclo --threshold N` still lists every struct and enum

Call #277, grepping the post-split `play.rs`, got four struct rows mixed into
three fn rows. On unruster's own tree `--sort cyclo --threshold 30` prints 7 fns
followed by 202 structs and 35 enums, none of which have a cyclomatic complexity
and none of which the threshold applies to. They are correctly excluded from the
finding count; they just make the rows ungreppable. Suppress struct/enum rows
when `--sort` names a fn-only metric, or move them behind `--shapes`.

## F4. `show <name>` ambiguity costs a round trip

Call #202: `unruster show sweep` → *"names 2 items — showing the list. Re-run
with the qualified name"*. The list it printed was enough here (the agent read
the `file:start-end` and used `sed`), but the message asks for a re-run the
agent did not do. Either print both items when there are two (the budget is
`loc`, not item count), or drop the "re-run" wording when the list already
answers it.

## F5. Not unruster, logged for completeness

Call #209 — `grep -rn "Topic::all()" --include=*.rs .` died with zsh's
`no matches found: --include=*.rs`; call #240 — a `git stash` passing a file
list as one pathspec. Both cost a retry. The zsh quoting hazard is already in
the user's global CLAUDE.md for `=`-leading words; `*`-containing flag values
are the same class.

---

# Priority

1. **D3** (unexplained waivers turn a gate green silently) — this one is a correctness hole in the gate itself, and it fired in this session.
2. **D1** (`--transitive` and cycles) — the flag's whole purpose, reproducibly wrong.
3. **Q1** (`concepts/signature` 0-for-5) — the single largest source of waiver noise; a 5-row 100%-FP gating check trains readers to waive without reading.
4. **D2**, **S1**–**S3** — all small, all pure output, all cost real minutes here.
5. **Q2** (catch-all over string matches) — new capability, and it is where the best advisory find of the session came from.
6. **D4**, **Q3**, **F1**–**F4**.


---

# Outcome — 2026-09-13

**D1, D2, D3, D4 implemented.** `cargo clippy --all-targets` clean, 861 tests
green (248 unit + 613 CLI, 6 new), `unruster self-check` 0 violations,
`unruster audit` 0 gating.

## D1 — `dead-code --transitive` (`src/dead_code.rs`, `src/main.rs`)

The round loop and `TRANSITIVE_ROUNDS` are gone, replaced by mark-and-sweep
reachability. Roots are every body that survives a deletion — `main`, test fns,
trait impls, ABI exports, anything `reportable` declines — plus `CallSink`'s
`outside` set (names used at module scope). Everything reachable from them is
live; every remaining candidate is dead. Cycles have no special case because
they need none: a cycle nobody outside it reaches is not reachable.

Deviation from the proposal: no SCC condensation. Reachability from the roots
is the same answer with less machinery — the condensation was a way of
describing the problem, not the smallest fix for it.

The `via` attribution is preserved and slightly better: for a cycle member it
prefers a *direct* dead caller (`transitive after gen::dispatch`) over the other
half of the cycle, which is equally conditional.

`--transitive` on the D1 repro went from 1 row to 5, and a deletion now adds no
row the run did not predict. Two tests:
`dead_code_transitive_sees_through_a_cycle_of_private_helpers` (including the
delete-and-compare invariant) and
`dead_code_transitive_keeps_a_cycle_that_a_root_reaches`, which is the
false-positive side — a cycle a test reaches stays live.

Also corrected: the footer and `--transitive`'s help said "the **private**
orphans each deletion would expose". It reports `pub` ones too, and always did
(`Rng::chance` is one).

## D2 — `dead-code` names the rows a `use` line still points at (`src/callers.rs`, `src/dead_code.rs`)

`import_sites`' visitor became `use_leaves` — every `use` leaf in the tree, once
— and two callers now read it: `import_sites` filters it to one name,
`imported_names` (new, `pub(crate)`) folds it to `name → files`. `ImportSite`
gained a `name` field, which is what made one walk serve both.

`dead-code` calls it when it has hits and prints:

```
(note: 2 of 3 row(s) also imported by name elsewhere — `all_specs` in 1 file(s),
 `also_used_in_a_mod` in 2 file(s). A `pub use` re-export is a reference and not
 a call site, so it neither keeps an item off this list nor survives its
 deletion: remove the `use` line in the same edit.
 `callers <name> --with-imports` lists them.)
```

The fraction counts rows, the list counts distinct names (two items can wear
one), and a file that both defines and imports an item is excluded — that is
the `use self::…` shape, not a reference an edit has to reach. Capped at five
names with `and N more`. Test:
`dead_code_names_the_rows_a_use_line_still_points_at`, which also asserts the
note stays silent when nothing imports anything.

## D3 — waiver health on the digest (`src/audit.rs`)

The waiver clause now carries `N undated` and `N with no reason`, counted over
the same scoped `ledger` as the other numbers on that line, and the
`— \`unruster waivers\` to review` pointer fires on them as well as on dead
waivers:

```
… 2 waiver(s) hiding 2 finding(s), 1 undated, 1 with no reason — `unruster waivers` to review
```

A healthy ledger is unchanged — unruster's own 28 waivers add nothing to the
line, which is the point: an alarm on every run is an alarm nobody reads.

Deviation from the proposal: the blank-line waiver still suppresses, and the
parser still stops at a blank line. Both are deliberate. Absorbing a comment
across a blank line would let a waiver silently adopt unrelated prose below it,
and refusing to suppress on an undated waiver would light up gates on every tree
carrying legacy (pre-`ok(<check>)`) comments. What was wrong was that the digest
could not tell the two ledgers apart; now it can. Test:
`the_audit_line_says_when_a_waiver_cannot_be_reviewed`, which asserts both the
alarm and the silence.

**Still open from D3:** whether an unexplained waiver should hold the exit code
open under `--strict`. Left alone — it changes what a gate means, and that is a
call to make deliberately rather than as part of a reporting fix.


## D4 — the kind scale, ordered by the axis it claims (`src/error_swallows.rs`)

`.unwrap_or_default` / `.unwrap_or_else` / `.unwrap_or` moved from 0.15 to
**0.25**, above `.ok`/`.err`/`if-let-ok`/`while-let-ok` at 0.20 and below
`let-_`/`match-err-wild` at 0.30. The arms are now written in descending order
so the scale reads down the page. `.ok` was *not* lowered: that would have
un-gated `display_name`, a confirmed real bug.

One new gating class results: **a silent substituted fallback on an `Io`
effect**, 0.50 → 0.60. That is exactly the reported miss (`ZoneServiceImpl::
looks`). Every other combination keeps its verdict — `Unknown` 0.35 → 0.45 and
`Decode` 0.20 → 0.30 are both still below the gate, and `Mutation` already
gated.

`SUBSTITUTION_WEIGHT` stayed at 0.20. Its doc was tuned to a boundary
(*"a substituting fallback on an unrecognised call gates, and a defaulting one
still does not"*) and that boundary still holds at the new weight — 0.45 + 0.20
gates, 0.45 alone does not. Only the arithmetic in the doc moved.

### The half that was not in the proposal: a new benign family

Raising the fallback kinds without this would have promoted
`Mutex::lock().unwrap_or_else(|e| e.into_inner())` — **7 of the 10 `io`
fallbacks on unruster itself** — into a gating tier with nothing wrong in it.

So `.unwrap_or_else` whose closure reads its error is now the benign family
`error-used`: the error was *consulted*, not dropped, which is the "inspects"
tier of the care scale `divergence --handling` ranks on.
`fallback_substitutes` already exempted this exact shape for this exact reason
and named the poisoned-lock idiom while doing it; this is the other half of
that judgment. `|_|` still reads as a swallow, and the dedicated command still
shows the family — only `audit` drops it.

Measured on unruster's own tree: the 7 poison-recovery rows fell 0.50 → 0.25
(benign, effect zeroed) and three rows rose 0.50 → 0.60. All three were
verified and waived with reasons, which is the prescribed repair for that tier:

| site | verdict |
|---|---|
| `contract_drift::arg_shape` | false positive — `call_of` returns `Option`, so there is no error. `receiver_is_option` bottoms out at method names it knows and cannot see through a local fn's return type. |
| `emit::Out::source_line` | real but a rendering — a file parsed moments ago, re-read for `--context` lines. Same shape as the standing waiver in `suppress::scan`. |
| `outline::run` | same, for `--sig` lines. |

### Also fixed in passing

The benign-family filter was two identical `match` arms — one in the waiver
retain, one in the row retain — which is the shape where one side gets a new
family and the other does not. Extracted to `benign_kept`.

One existing unit test had to change: `unknown_chains_do_not_gate` asserted
`.unwrap_or_default` on **`Effect::Io`** does not gate. `Io` is not an unknown
chain and never was — the assertion was in the wrong test and was itself the
defect, stated as a guarantee. Replaced with the `Unknown` case the test's name
claims, plus
`a_substituted_value_vanishes_more_completely_than_an_option`, which asserts
the full ordering across all four effects. Two CLI tests cover the end-to-end
shape and the new benign family.

`explain silent-fallbacks` / `playbook.txt` updated: three idiomatic families,
not two, and the `kind` axis now spells out where `.unwrap_or_default` and
`.ok` sit relative to each other.

---

# Recommendation on Q1, S1, Q2 — 2026-09-13

Arithmetic below is from `Cluster::score` (`src/concepts.rs:324`),
`Out::cap_note` (`src/emit.rs`), `audit::rerun_cmd` (`src/audit.rs:159`) and
`stringly::visit_expr_match` (`src/stringly.rs:106`), read against the Augrym
session's numbers.

## Q1 — do it, as a hard demotion on signature population

### Why it scores 0.83

The `unit` cluster: 4 members, all `pub`, 4 modules, positional.

```
0.28  floor
0.15  0.22 × count      (4 members → 0.67)
0.14  0.14 × public     (4/4 pub          → 1.0, maxed)
0.16  0.16 × spread     (4 modules        → 1.0, maxed)
0.10  0.10 × deliberate (positional cohort → 1.0, maxed)
0.00  0.15 × agreement  (signature_rarity(139) ≈ 0.007 → ≈0.0004)
────
0.83  against a 0.70 gate
```

Three of the six terms — 0.40 of the score — are **maximised by construction**
for a well-organised table. Every spec is `pub` because the engine calls it
from elsewhere. Every spec is in its own topic module, which the check reads as
"these authors could not see each other". Every spec is named `<word>_<word>`,
which the check reads as "a convention, so a collision inside it is an
oversight". Add the 0.28 floor and you are at 0.68 before any evidence of
duplication has been weighed.

### Why all four guards missed

| guard | why it did not fire |
|---|---|
| `TAXONOMY_SIZE` (≥6 members) | `cognate_partition` splits *first*: 139 members became word-groups of 3–5. Its own doc says so — *"the clusters that reach the score are small by construction no matter how common the signature is"*. |
| `dispatch_family()` | needs `modules() == 1` **and** every member private. A spec-per-module table is the exact opposite of both. |
| `is_accessor_shape` | restricted to `SCALAR_RETURNS`. `Spec` is a domain type, deliberately exempt. |
| `signature_rarity` | measures the pre-split population correctly, then enters as `0.15 × (0.4 × rarity)` ≈ 0.0004. Its sibling doc already measured this: *"its entire authority is 0.06 of a score whose floor is 0.28"*. |

### The change

Do what `is_accessor_shape` did — a hard demotion, not another weight. Carry
the tree-wide signature population (already computed for `signature_rarity`)
onto `Cluster`, and extend `taxonomy()`:

```rust
fn taxonomy(&self) -> bool {
    self.members.len() >= TAXONOMY_SIZE
        || self.dispatch_family()
        || self.signature_population >= SIGNATURE_FAMILY
}
```

`SIGNATURE_FAMILY = 12`. The evidence brackets it cleanly: the false positives
this check has measured are signatures *"worn by dozens of functions"*
(`() -> bool`/*active*, `() -> usize`/*count*, `() -> &str`/*label*, and now
`() -> Spec` × 139); the true positive it must keep is
`(AabbHandle, Rect, egui::Pos2) -> bool` **on three functions**. 12 is also
`TAXONOMY_SIZE × 2`, which keeps one idea in the file rather than two.

Demotion, not a drop: 0.83 − 0.25 = 0.58, under the gate, still listed — which
is what `taxonomy()` already means and why *"a taxonomy is still worth one line
of a reader's attention"*.

**Effort S–M · risk low** (a demotion can only remove gating rows, never add
one) · measure against the fixture corpus and the four noise rows
`TAXONOMY_SIZE`'s doc records (0.87, 0.77, 0.77, 0.89).

**Priority: highest of the three.** A gating check that went 0-for-5 on a real
codebase and cost five hand-written waivers is worse than a noisy advisory one:
it teaches the reader to waive without reading, which is the habit the waiver
rule exists to prevent.

## S1 — do it, and note the trigger is inverted

`rerun_cmd` runs 12 of 21 checks with non-default arguments. `cap_note()`
returns `None` when `dropped == 0`, so the hint is triggered by **volume** —
and a short section is precisely the one whose standalone command asks a
*different question*. The trigger is backwards: it fires when there is more of
the same, and stays silent when the answer would change.

Measured on unruster's own tree: 8 sections shown, 6 hints. On Augrym: `casts`
(4 rows), `arith-drift` (2), `config-drift` (3), `builder-drift` (1),
`near-clones` (4), `panics` (2) all printed nothing, and two of them were then
re-run bare and misread.

1. Print it whenever `rerun_cmd(check) != check`, capped or not — and phrase it
   as what ran, not as "for the rest": `(this section ran arith-drift
   --min-score 0.6; the bare command defaults to 0.50)`. ~8–12 lines on a full
   audit.
2. Do **not** print it for the 9 checks that run bare. `other => other` already
   makes the bare name exact there, and a no-op hint on every section is the
   mistake the codebase already names — *"appending it to every run is what
   taught readers to walk to the ledger and find nothing there"*.
3. Separately, the standalone side of the same gap: `threshold_note` is called
   by 3 checks (`divergence`, `config-drift`, `builder-drift`) out of the 12
   that diverge. `config-drift` says *"min_score=0.05 (audit gates at 0.12)"*;
   `arith-drift` says `min_score=0.50` and nothing else. `self_check` can assert
   that every `rerun_cmd` arm has a matching disclosure.

**Effort S · risk none** (pure output). Do it first — it is hours, and it
directly caused two wrong readings in one session.

## Q2 — do it, but as its own piece, and mind the row shape

`visit_expr_match` records one row per string literal *in a pattern*. A `_ =>`
arm has no literal, so it emits nothing: on `user_service::ascend` the tell was
the **absent third row**, and the reader found the bug by reading the source
around the two that were there.

`catch_all.rs` and `enum-coverage` already do this analysis — all of it keyed
on an enum type, and a `match` on `&str` has none. That is the gap.

Group `match-lit` hits by their enclosing `syn::ExprMatch` and carry arm count,
literal count and whether the last arm is a wildcard. Then:

- **Rank a literal match with a `_` arm above one without.** The wildcard is
  what turns "add a case, get a compile error" into "add a case, get silent
  fallthrough" — which *is* the argument for the newtype the check already
  recommends.
- **Flag the iterate-a-list-and-match-it-back shape** — `for x in ["a","b","c"]
  { match x { "a" => …, _ => … } }`. Syntactically local, and it is the exact
  Augrym bug: a fourth entry compiles and deletes from the wrong table.

Side benefit: it collapses the row count (Augrym 21 → far fewer; unruster 265 →
~80), which answers the session's other `stringly` complaint, and it removes
the duplicate-context problem in S4 — two literals on one line become one row.

**Effort M · risk medium**, and the risk is the row shape: this check's output
is `awk`-ed, and the codebase is explicit about not moving columns (*"appending
it unconditionally would move every existing reader's awk"*). Mitigate by
keeping per-literal rows as they are and adding the match-level row as a new
kind (`match-wild`) rather than changing `match-lit`.

**Priority: after Q1.** It is the highest-yield *new* finding in the session,
but it is the only one of the three that is new capability rather than
finishing something already half-built — and it wants its own measurement pass
against the fixture corpus.

## Order

**S1 → Q1 → Q2.** S1 and Q1 are the same kind of defect — the check already
knows the thing and either does not say it or does not weigh it — so they can
share one pass and one measurement run. Q2 is a separate piece of work.
