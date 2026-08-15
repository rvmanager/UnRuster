# unruster — implementation plan from three field sessions

**Status:** implemented in `0.1.83` — every item R1a–R17. See *Outcome* at the
foot of this file for what each one measured afterwards and what could not be
verified here.
**Baseline:** `0.1.82` plus the uncommitted `emit.rs` / `waivers_cmd.rs` fixes.
**Evidence:** three session logs in `impl_logs/`, against two codebases. The
**CLI project** supplied two — an analysis session (0.1.81, 3.3k lines) and a
refactor (0.1.82, 7k lines). The **GUI project** supplied one — an audit loop
over 4,867 items (0.1.82, 6k lines). Both are named only by those labels below;
the logs themselves are kept as-is.

Each item is self-contained: evidence, root cause with file and line, the
change, acceptance criteria, and risks. Items are independent unless a
`Depends on` line says otherwise.

---

## Why this plan exists — the one number that matters

Tallying every **gating** finding across the two audit-driven sessions against
what a competent agent actually did with it:

| check | findings | actions taken | rate |
|---|---|---|---|
| `dead-code` | 3 | 3 deletions (+4 cascaded) | **100%** |
| `error-swallows` | 2 | 1 latent bug fixed | **50%** |
| `near-clones` | 23 | 4 consolidations (~10 pairs), 13 waived | ~43% |
| `validation-drift` | 20 | 0 | **0%** |
| `concepts` | 52 | 0 | **0%** |
| `panics` | 95 | 0 | **0%** |

167 gating findings produced nothing and cost 158 hand-written waivers. Five
produced five actions. Tier 1 is about that asymmetry; everything else is
smaller.

**Do not start by demoting the noisy checks to advisory.** The `Gate` enum's
own docstring (`src/audit.rs:71`) records why that was already tried and
reversed: a twelve-crate workspace had all five gating checks return zero while
`error-swallows` sat in the advisory pile holding a permanent loss of Stripe
payment confirmations. Fix precision first (R1, R3); reclassify only what
precision cannot save (R2).

---

# Tier 1 — Gating precision

## R1a. `concepts`: weight the `signature` view by how rare the signature is

**Tier:** 1 · **Effort:** S (½ day incl. tests) · **Risk:** low

### Evidence
GUI project: 52 of 52 gating `concepts` findings were `signature` clusters, all
verified false. The agent checked the two most plausible by hand — `node_count`
vs `total_node_count` (deliberately different sets, doc comments cross-link) and
`AppState::any_wizard_active` vs `OperationState::any_wizard_active` (the first
*calls* the second) — then waived all 52. Observed rows clustered on
`() -> bool` + "active", `() -> usize` + "count", `() -> &str` + "label".

### Root cause
`signature_clusters` (`src/concepts.rs:~600-655`) groups every fn by its
signature string, then `cognate_partition` splits each group by shared name
word. The `agreement` weight passed to the describe closure is a **flat `0.4`
for every signature cluster** (`src/concepts.rs:~646`), so `() -> bool` — which
may appear on 200 fns tree-wide — scores exactly like
`(AabbHandle, Rect, egui::Pos2) -> bool`, which appears on three.

Score arithmetic for the observed 0.71 rows (`Cluster::score`,
`src/concepts.rs:274`): a 3-member cluster, all `pub`, spread across ≥3 modules:
```
count      = (3-2)/3        = 0.333 → 0.22 * 0.333 = 0.073
public     = 1.0            → 0.14
spread     = 1.0            → 0.16
deliberate = 0.0            → 0.00
agreement  = 0.4            → 0.15 * 0.4 = 0.060
raw = 0.28 + 0.073 + 0.14 + 0.16 + 0.06 = 0.713    ← gates at 0.70
```
`taxonomy()` (n ≥ `TAXONOMY_SIZE` = 6) never fires because `cognate_partition`
has already split the large family into small word-groups. **The existing
demotion is size-based and the noise is small-by-construction** — this is why
the measured `TAXONOMY_SIZE` tuning did not catch it.

### Change
In `signature_clusters`, `by_sig` already knows how many fns share each
signature before partitioning. Thread that count into the describe closure and
scale `agreement` by signature rarity:

```rust
// in the `for (sig, members) in by_sig` loop
let population = members.len();               // fns tree-wide with this signature
...
move |word, _| {
    // A signature shared by half the tree carries no information about
    // whether two fns are one concept. `() -> bool` on 200 fns says only
    // that Rust has booleans; `(AabbHandle, Rect, Pos2) -> bool` on three
    // is a real interface. Rarity is the difference and the flat 0.4 could
    // not see it.
    let rarity = (1.0 / (population as f64 - 1.0).max(1.0)).clamp(0.0, 1.0);
    (label, shape, 0.4 * rarity)
}
```
Exact curve is a tuning decision — see *Calibration* below. Anything monotonic
decreasing in `population` works; start with the reciprocal.

### Calibration (do this, don't guess)
1. `unruster --root <gui-project> concepts --kind signature --json --top 0` and
   record every cluster's score under the current code.
2. Apply the change, re-run, diff.
3. Target: the 52 named in the GUI project's audit-loop log around line 4694 fall
   below 0.70. The three real ones from `TAXONOMY_SIZE`'s own docstring
   (`src/concepts.rs:209`) — 2, 2 and 4 members — must stay above it.
4. Re-run on UnRuster's own tree: `concepts` currently reports 122 declarations
   / 54 rows, 0 gating. It must stay 0 gating.

### Acceptance
- New CLI test: a fixture with one common signature on 8+ fns split across word
  groups, plus one rare signature on 2 fns. The rare cluster gates; the common
  ones do not.
- `cargo test`, `cargo clippy --all-targets`, `self-check` all clean.
- `unruster audit` still 0 gating on this repo.

### Risks
Under-scoring genuine duplicates that happen to share a common signature. The
`spread`, `public` and `positional` terms still carry those. Mitigate by
checking step 3 above rather than by softening the curve.

---

## R1b. `validation-drift`: demote large single-word cohorts

**Tier:** 1 · **Effort:** S (½ day) · **Risk:** low

### Evidence
CLI project: 10 of 13 gating findings were `edit::*parse*` — `parse_region`,
`parse_inside`, `parse_addr`, `parse_ops`, `parse_pair`, `parse_select` flagged
for not validating like siblings `parse_addrs`, `parse_op`, `parse_entry_spec`,
`parse_predicates`. The agent verified them as non-defects **twice**, across two
sessions, and refused to waive on the user's behalf — so `audit` exited 1 for
the entire session and the `until unruster audit; do <fix>; done` loop could
never close.

GUI project: the same shape as `Document::*transform*`, a 10-member family. 10
gating findings, 0 fixes, 14 waivers.

### Root cause
`run_drift_counted` (`src/validation.rs:358`) keys cohorts on
`(enclosing scope, shared name word)`. The filter at `src/validation.rs:378`
excludes words under 3 chars and `is_generic_api_word` (Rust API vocabulary:
`new`, `from`, `len`). It does **not** exclude ubiquitous *domain* verbs. In a
parser, `parse` is a domain word on 9+ functions with unrelated contracts.

`Drift::score` (`src/validation.rs:338`) makes this worse: `weight` saturates at
4 checked siblings, so a large cohort scores *higher*, not lower:
```
parse_region: checked = 4, unchecked = 6
ratio  = 4/10 = 0.4      → 0.45 * 0.4 = 0.18
weight = (4-1)/3 = 1.0   → 0.30 * 1.0 = 0.30
score  = 0.25 + 0.18 + 0.30 = 0.73    ← gates at 0.70
```
The module header (`src/validation.rs:40`) names `parse_header`/`parse_body` as
the *positive* example — which is precisely the misfiring shape.

### Change
Mirror `concepts`' taxonomy demotion, which already exists and is measured. Add
to `src/validation.rs`:

```rust
/// Cohort size at which a shared word stops being a contract and starts being
/// a naming convention.
///
/// Measured on two codebases. `edit::*parse*` (9 members) and
/// `Document::*transform*` (10) produced 20 gating findings between them and
/// zero defects: in a parser every function is named `parse_*`, and the word
/// says nothing about whether they share an input contract. The cohorts that
/// produced real findings were small — a handful of siblings in one impl.
///
/// Demoted rather than dropped, on `concepts::TAXONOMY_SIZE`'s reasoning: a
/// large cohort is still where a *new* member would be added, and a reader
/// scanning advisory rows may want it. It must not hold the gating loop open.
const CONVENTION_SIZE: usize = 6;
```
Apply in `Drift::score`, after the existing terms:
```rust
let convention = if self.checked.len() + self.unchecked >= CONVENTION_SIZE {
    0.25
} else {
    0.0
};
(0.25 + 0.45 * ratio + 0.30 * weight - convention).min(1.0).max(0.0)
```
`Drift` already carries `checked: Vec<String>` and `unchecked: usize`, so cohort
size needs no new field.

### Calibration
`CONVENTION_SIZE = 6` aligns with `concepts::TAXONOMY_SIZE` and puts the two
observed families (9 and 10) under the gate: `0.73 - 0.25 = 0.48`. Verify
against both trees before settling; if a real finding in a 6-member cohort is
known, raise to 7 rather than dropping the penalty.

### Acceptance
- The CLI project's 10 `edit::*parse*` rows drop below 0.70 and `audit` exits 0
  there with **no new waivers**. This is the headline acceptance test.
- The GUI project's 10 `Document::*transform*` rows likewise.
- New CLI test: a fixture with a 7-member `parse_*` cohort (one unchecked) and a
  3-member `decode_*` cohort (one unchecked). Only the small one gates.

### Risks
A genuinely careless sibling inside a large family is demoted to advisory. That
is the intended trade — it is still reported, and the alternative is 20 findings
nobody acts on holding the gate shut.

---

## R2. Reclassify only what R1 cannot fix

**Tier:** 1 · **Effort:** S · **Risk:** medium · **Depends on:** R1a, R1b

Do **not** start here. Read `src/audit.rs:71` first — the `Gate` docstring
documents a previous demotion that hid a live money bug, which is why `Tiered`
exists.

After R1a and R1b land, re-run the full battery on both codebases:
- If `concepts` / `validation-drift` gating counts are near zero, stop. Done.
- If either is still producing findings nobody acts on, raise its
  `GATING_SCORE` (`src/concepts.rs:61`, `src/validation.rs:67`, both `0.70`)
  rather than switching `Gate::Tiered` → `Gate::Advisory`. A raised threshold
  keeps the tier mechanism; demotion throws it away.

`panics` is handled by R3, not here.

---

## R3. Narrow `panics`' `decode` class to data from outside the process

**Tier:** 1 · **Effort:** M (1–2 days) · **Risk:** medium

### Evidence
GUI project: 95 gating findings, 0 fixes, 58 item-scoped waivers. The agent read
every one and classified them into five rationales — all `T::try_from(<in-process
length / index / clamped slider value>)` or `.expect` on arena calls. It
specifically checked the two touching external data (`dxf_import` pushes then
reads `len()-1`; `load_from_bytes` length-checks before slicing) and found both
safe.

### Root cause
The `decode` effect class treats a fallible conversion as external-input
decoding regardless of where the value came from. `src/panics.rs:112` documents
the intent — "the input to a parse is by definition data the process did not
produce" — but the implementation cannot tell `u32::try_from(vec.len())` from
`u32::try_from(parsed_header_field)`.

### Change
Add a provenance test before assigning `decode`. Treat as **in-process** (score
below `GATING_SCORE`, or a new `invariant` class) when the converted expression
is:
- `.len()`, `.count()`, `.capacity()` on a local collection;
- an index or loop variable bounded by such a call;
- the result of `.clamp(..)`, `.min(..)`, `.max(..)` on the same line or the
  line above;
- a call to a fn defined in this tree whose return type is not a `Result` from
  a parse/IO family.

The first three are syntactic and cheap. The fourth needs `ctx.sem.fn_sigs`,
already available.

### Acceptance
- The GUI project's 95 drop to the handful that touch genuinely external bytes.
- The two sites the agent verified by hand (`dxf_import`, `load_from_bytes`)
  must still be *reported* — they are the true-positive shape — even if their
  score changes.
- New unit tests in `src/panics.rs` alongside `decode_outranks_mutation_for_panics`
  (`src/panics.rs:417`), one per provenance rule.

### Risks
Highest-uncertainty item on the list. Provenance analysis is where a syntactic
tool runs out of road, and an over-broad in-process rule would hide the exact
crash the check exists to find. Ship behind measurement: count the class
population before and after on both codebases, and keep `--include-idiomatic`-style
escape hatch semantics so the narrowed rows remain reachable.

---

## R4. Make `near-clones`' recommended fix visible in what `audit` watches

**Tier:** 1 · **Effort:** S · **Risk:** low

### Evidence
GUI project: merging `draw_equal_distance` and `draw_align` into one
parameterised `draw_two_point_symbol` pushed it from 7 to 8 arguments and
tripped clippy's `too_many_arguments` (audit-loop log, line 5577). The
standard `near-clones` fix — parameterise and merge — trades duplication for
parameter count, and `audit`'s `metrics` section gates on **`cyclo` only**
(`src/audit.rs:423`, `--sort cyclo --threshold 15`).

### Change
Either:
1. Add a second `metrics` pass to the battery on `--sort params` with a
   threshold of 7 (clippy's own default), advisory; or
2. Add a sentence to `explain replication` naming the trade.

(1) is more useful and costs ~15 lines in `src/audit.rs` next to the existing
metrics section. Note `metrics::run` now takes a `GroupBy` argument.

### Acceptance
A fixture where merging two near-clones would exceed 7 params shows the params
row after the merge. `audit` exit code unchanged (advisory).

---

# Tier 2 — Correctness and data safety

## R5. Extend `self_check`'s `query-form-invariance` past the fn family

**Tier:** 2 · **Effort:** M · **Risk:** low

### Evidence
Six commands (`type-refs`, `takes-mut`, `enum-coverage`, `catch-all-arms`,
`parallel-matches`, `divergence`) rejected module-qualified names for six
releases. `self-check` reported `ok query-form-invariance 120 0 0` throughout.
The CLI project's session hit it on `enum-coverage scene::Kind` — an enum carrying a
`/// unruster: sealed` marker placed there for that command — and fell back to
hand-typed `grep -rn 'Kind::Circle\|Kind::Ellipse\|…'` for the rest of the
refactor.

### Root cause
`check_query_forms` (`src/self_check.rs:261`) opens with
`if !unique || d.kind != "fn" { return; }` and resolves through
`callers::QueryMatcher`. The type- and enum-target commands never touch
`QueryMatcher`, so the invariant cannot observe them. Its own docstring calls
this "the class that produced four separate defects".

### Change
Add a sibling invariant that does not go through `QueryMatcher`. For each
indexed `struct` / `enum` with a unique name, assert that running the
name-taking commands with the bare name and with `d.qpath` selects the same row
set. This means either invoking the command functions with a silent `Out` (the
pattern `waivers_cmd::populate_hits` already uses, `src/waivers_cmd.rs:60`) and
comparing counts, or comparing the row fingerprints the silent sink records.

The `populate_hits` probe-context pattern is the model: build an `AnalysisCtx`
with `out: &Out::silent()`, run, read counts.

### Acceptance
- Reverting any one of the six `last_segment` calls added this session makes
  `self-check` exit 1.
- Probe count for the new invariant is > 0 on this repo (a `none` result is a
  silent pass — see the existing `a-test-fn-is-never-dead` warning).

### Risks
Running N commands × M types is O(N·M) whole-tree scans. Bound the probe set the
way the existing invariants do (`--probes`, default 120).

---

## R6. Make `hits()` mean what the waiver ledger says it means

**Tier:** 2 · **Effort:** M · **Risk:** medium

### Evidence
Found while building the R-N1 fixture (already fixed). A `casts/widen-int`
waiver and an `error-swallows` row scoring 0.50 both report `hits=1` even though
`audit` filters them out of the gating tier.

### Root cause
`Waiver::hits` (`src/suppress.rs:252`) is documented as "findings suppressed
that the audit battery **would have gated on**". `populate_hits`
(`src/waivers_cmd.rs:60`) runs the battery twice — `BatteryConfig::gating()` and
`::permissive()` — but hit counting happens in `retain_unsuppressed`, which runs
*before* each check applies its own class filter and score gate. So a row the
gating config produces but the check then filters still counts as a hit.

### Change
Count hits after the check's own filtering, not before. Options:
- Move `retain_unsuppressed` after the score/class filter in each check (large,
  touches every check, risks changing waiver semantics); or
- Have `retain_unsuppressed` take the score and the gate threshold and record
  into `hits` vs `below_audit` accordingly (smaller, but needs the score at that
  point, which not every check has yet computed).

Investigate both before choosing. Prefer the second.

### Acceptance
- A waiver on a `widen-int` cast reports `hits=0, below_audit=1` (currently
  `1, 1`).
- The two waivers the fixed `--orphaned --remove` guard distinguishes still
  behave as they do now.
- The GUI project's ledger summary numbers change only in the direction of accuracy.

### Risks
This narrows what counts as "earning its place", so waivers that currently look
live may become orphans. That is the point, but it will move numbers on real
ledgers. Land it with the R-N1 guard already in place (it is), so nothing is
auto-deleted as a result.

---

## R7. Report a blind-spot delta under `--changed-since`

**Tier:** 2 · **Effort:** S · **Risk:** low

### Evidence
GUI project: 45 → 49 blind spots during the session — the agent's own dedup
edits introduced four macro bodies no check can read, unwarned. At
audit-loop log line 3197 the same agent *declined* a macro-based
refactor because macros are known blind spots, so the disclosure already shapes
design decisions; it just does not fire when you create one.

### Change
`macro_scan::survey` already runs once over every parsed file
(`src/main.rs:~2565`). Under `--changed-since`, additionally count blind spots
in the changed files at the git ref and report the delta:
```
(blind spots: 49 in the tree, 4 of them new in the changed files — code inside
 them was not analyzed by any check; `unruster blind-spots` lists them)
```
The `--since` machinery already reads a git ref into a temp tree
(`audit --since` / `battery_at_ref`); reuse it.

### Acceptance
A fixture where the working tree adds a `macro_rules!` body reports `1 new`.
Unchanged trees report no delta clause.

---

# Tier 3 — Setup traps (a wrong answer that reads as a clean one)

Both items in this tier are the same doctrine already written down at
`src/main.rs:2545`:

> A scan that saw nothing is a setup error … and reporting it as a clean run is
> the worst possible answer: `until unruster audit; do fix; done` terminates
> immediately and a CI gate passes vacuously.

That doctrine was applied to `--root` and never extended.

## R8. Walk up to the nearest `Cargo.toml`

**Tier:** 3 · **Effort:** S · **Risk:** low

### Evidence
Reproduces today: from `impl_logs/` inside this repo, every command dies with
`error: no .rs files found under .`. The CLI project's analysis session hit it twice
after a `cd` into a scratch directory persisted, and it killed a **batched**
invocation both times — `show 'scene::LineSpec' 'tune::TUNABLE'` lost both
targets (analysis log, line 1533).

### Change
In `src/main.rs`, before the exit-2 branch at ~2545: if `--root` was not given
explicitly and no `.rs` files were found, walk parent directories looking for
`Cargo.toml`. On finding one, re-run discovery from there and print:
```
(note: no .rs files under `.`; scanned /path/to/crate instead — the nearest
 Cargo.toml above the working directory. Pass --root to choose another.)
```
If the walk also finds nothing, keep the current exit-2 error verbatim.

Only when `--root` is absent. An explicit `--root` that finds nothing stays an
error — the user named a place and it was wrong.

### Acceptance
- `cd impl_logs && unruster inventory` scans the crate and says so.
- `unruster --root /nonexistent inventory` still exits 2 with the current
  message.
- New CLI test for both.

### Risks
A user in `~/` with a `Cargo.toml` three levels up gets a surprise scan. Bound
the walk (say 4 levels) and always print the chosen root.

---

## R9. Distinguish "nothing changed" from "clean" in `--changed-since`

**Tier:** 3 · **Effort:** S · **Risk:** low

### Evidence
Reproduced: on a committed tree with 2 real gating findings,
`audit --changed-since HEAD --findings-only` reports
`0 gating + 0 advisory … clean: no gating findings, exit 0`. The CLI project's agent
wrote *"It's odd that the second run reports zero gating and advisory findings…
I should rerun the unscoped audit to see if this is caching or nondeterminism"*
and spent two commands establishing that nothing had changed
(refactor log, line 5549).

### Change
`AnalysisCtx` already holds the changed-file set (`ctx.changed`). When it is
`Some` and empty, prepend to the summary — for every command, not just `audit`:
```
(note: 0 files changed vs HEAD, so nothing was scanned — this is an empty
 scope, not a clean result.)
```
Consider exit 2 for `audit` specifically, matching the R8 doctrine. That is a
behaviour change for anyone running `audit --changed-since` in CI on a
no-op commit, so it needs a decision; the note alone is the safe subset.

### Acceptance
- Committed tree with known findings: `--changed-since HEAD` prints the note.
- Tree with real changes: no note, findings reported as now.
- New CLI test covering both.

---

## R10. Long waiver keys must not lose their date

**Tier:** 3 · **Effort:** S · **Risk:** low

### Evidence
The GUI project reported "4 undated" waivers that all carried dates. Cause: the
generated key was long enough that the date wrapped:
```rust
// unruster: ok(near-clones/cmd_toggle_visibility/cmd_toggle_construction)
// 2026-08-15 — the reason…
```
Reproduced locally: the waiver **still suppresses correctly** (check and key
parse) but the date is lost, so `--fail-on-stale` fires on it forever and
`--stale N` always includes it. `--suggest-waivers` generates these keys —
`near-clones/<fn_a>/<fn_b>` concatenates two function names — so the tool emits
a line it cannot fully parse if anyone wraps it.

### Change
Pick one:
- **(a)** Accept the date on the first continuation line. In the waiver parser
  (`src/suppress.rs`), when the head line has no date, look at the next comment
  line for a leading `YYYY-MM-DD`. Backward compatible, fixes existing files.
- **(b)** Have `--suggest-waivers` warn when the generated line exceeds ~100
  chars and suggest an item-scoped placement instead.

(a) is the real fix; (b) is a useful addition. Do (a) first.

### Acceptance
- The wrapped fixture in this plan's sibling test reports `0 undated`.
- Existing single-line waivers unchanged.
- `waivers --upgrade` still rewrites correctly.

---

## R11. Add `waivers --undated`

**Tier:** 3 · **Effort:** XS · **Risk:** none

### Evidence
The summary reports a count with no way to list them. The GUI project's agent
hand-rolled the tool's own grammar as a regex:
```
grep -rn "unruster: ok(" src/ | grep -vE "ok\([^)]*\) [0-9]{4}-[0-9]{2}-[0-9]{2}"
```
`--stale 9999` works as an obscure workaround (dated waivers cannot be that old).

### Change
Add `undated: bool` to `WaiverOpts` (`src/waivers_cmd.rs:48`) and one filter
clause beside the existing `orphaned` / `legacy_only` / `stale` clauses at
`src/waivers_cmd.rs:~119`. Wire through `main.rs`.

### Acceptance
`waivers --undated` lists exactly the waivers the summary counts. New CLI test.

---

# Tier 4 — Missing capability

## R12. `callers --with-imports`

**Tier:** 4 · **Effort:** M · **Risk:** low

### Evidence
GUI project: `callers` correctly found 14 call sites for three
`*_or_identity_logged` helpers (audit-loop log, line 2526). The agent
then fell back to
`grep -rn "<three names>" src/ | grep -v "^src/<defining file>"`
(`:2813`) — for the `use` lines, which a rename or removal must also touch and
which `callers` never reports.

### Change
`src/module_uses.rs` already implements `use`-tree leaf extraction
(`use_leaves`) and per-scope use-map stacking. Reuse it: add `--with-imports` to
`callers`, emitting rows with `via=use` for import sites of the target name.

Cheaper alternative if that is too invasive: add a note to `callers`' summary
when the target is imported anywhere — "N file(s) also import this name;
`module-uses <mod>` lists them" — which at least stops the grep.

### Acceptance
`callers --with-imports <fn>` on a fixture with one `use` and two call sites
returns three rows. Default output unchanged (column-shape regression risk).

---

## R13. A bulk waiver application path

**Tier:** 4 · **Effort:** L (2–3 days) · **Risk:** medium

### Evidence
The single largest time sink in the GUI project's session. 95 `panics` sites →
JSON dump → grouping script → hand-built five-class rationale taxonomy
(`SLOT`, `CHANNEL`, `SLIDER`, `EXPORT`, `BOUNDED`) → patch script. That pipeline
was written **four separate times** across the session (`:2838`, `:2976`,
`:3646`, `:4211`, `:4352`, `:4485`, `:4593`, `:5285`, `:5414`).
`--suggest-waivers` does not help because its output carries no location.

### Change
`waivers --apply <file>` reading TSV or JSON rows of
`{file, line, check, key, reason}` and inserting correctly-formed, correctly-
scoped waiver comments — dry-run by default, `--write` to apply, reusing the
existing `mutate` machinery (`src/waivers_cmd.rs:391`), which already handles
bottom-up line-stable edits, trailing vs item scope, and preview rendering.

Pair with: make `--suggest-waivers --json` emit the location alongside the
suggested line, so `<check> --json | jq | waivers --apply -` is a one-liner.

### Acceptance
- 20 waivers applied from a file in one command, correctly scoped, `cargo fmt`
  clean, and the subsequent `waivers` ledger shows 20 dated entries.
- Dry-run previews exactly what `--write` would do.

### Risks
This writes to user source. It must inherit `mutate`'s discipline: preview by
default, refuse anything ambiguous, never guess a scope. Do not ship a version
that infers placement — take it from the input rows.

---

## R14. `dead-code --transitive`

**Tier:** 4 · **Effort:** M · **Risk:** low

### Evidence
CLI project: deleting three dead `pub fn`s exposed four more private orphans
(`refuse`, `namespace_shared_names`, `split_head`, `struct Head`) over four
build-delete-rebuild rounds driven by a Python loop over `cargo build` warnings
(refactor log, line 5671).

Worth noting **why `dead-code` earns its place**: verified locally that rustc's
own `dead_code` lint reports **zero** for a dead `pub fn` in a lib crate — it
cannot, since `pub` is API surface. unruster sees the whole tree. The two tools
are complementary: unruster for the `pub` layer, rustc for the private cascade.
`--transitive` closes the loop on unruster's side.

### Change
`dead_code::run` already builds a call-set from the full tree
(`src/dead_code.rs:202`). Iterate: remove the candidate set from the call graph,
recompute, repeat to a fixed point. Report second-round items with a marker
(`via=transitive`) and a note naming what would have to go first.

### Acceptance
A fixture where `pub fn a` calls private `b` calls private `c`, and nothing
calls `a`, reports all three with `a` direct and `b`/`c` transitive.

---

## R15. Make `audit` readable without a pipe

**Tier:** 4 · **Effort:** M · **Risk:** low

### Evidence
Every session's first `audit` was piped to `head` and cut. The refactor's cost
three recovery commands (refactor log, lines 188-228: `| head -200`, then `cat` the
tool-results file, then `sed -n '199,500p'`). The help says of `audit`: *"never
pipe it"*. The default `--top` also truncated three sections on this repo and
every section on the GUI project ("showing 40 of 222", "20 of 278", "15 of 46"),
forcing per-check re-runs with `--top 200`.

This is the third independent occurrence of the head-truncation pattern across
the logs, after `show` (fixed this session with a pre-body size note) and
`DEFAULT_MAX_LINES`' own docstring evidence.

### Change
Make `audit --findings-only` produce a bounded, *complete* digest by default:
every gating row, a capped sample of advisory rows, and an explicit per-section
"N more — `unruster <check> --top 0`" pointer. The goal is that the default
invocation never needs a follow-up and never needs a pipe.

Consider also emitting the total line count on the first line, the same
mechanism that fixed `show`.

### Acceptance
`audit --findings-only` on GUI-project-scale input fits a readable budget, and no
gating row is ever hidden by the cap.

---

# Tier 5 — Adoption

## R16. Route the navigation commands at the point of need

**Tier:** 5 · **Effort:** S per route · **Risk:** none

### Evidence
Zero pull-through on all five 0.1.82 additions across 5,997 lines of the GUI
project —
`at`, `module-uses`, `metrics --by`, `tests --mentions`, `show <Variant>` — none
used once. Meanwhile: 14 `sed -n 'N,Mp'` range reads, 7 `grep -n` against
`src/`, and one grep that was `module-uses`' exact idiom down to the `grep -v`
excluding the defining file.

They are all in `--help`, which the agent read at line 20. Nothing points at
them when the question arises.

### Change
Follow the pattern already in `show` (`note_field_route` for structs,
`note_size_and_route` for enums): name the better command in the output of the
worse one.
- `callers` on a target whose module is being removed → mention `module-uses`.
- `outline` → mention `at <file>:<line>` for the reverse lookup.
- `dead-code` → mention `tests --mentions <name>` before deleting.
- `metrics` default → mention `--by file|module`.

Each is one `ctx.out.note` behind a narrow condition. Do not add them
unconditionally; a note on every invocation is noise of its own.

### Acceptance
Each note fires only in its intended condition, asserted by a CLI test.

---

## R17. Rename or document the JSON `context` key

**Tier:** 5 · **Effort:** XS · **Risk:** low (breaking)

### Evidence
The GUI project's first grouping script died on `KeyError: 'item'`
(audit-loop log, line 5188) — the enclosing fn is under `context`,
which also names `--context` snippet lines (`context_lines`). One wasted round
trip.

### Change
Either rename the cell key from `context` to `in_fn` / `enclosing` across the
checks that emit it, or document the shape in `explain` and in `--help`'s JSON
line. Renaming is a breaking change for JSON consumers — check
`tests/cli.rs` for column-shape assertions first.

---

# Suggested order

1. **R1a + R1b** — the two precision fixes. Together they address 72 of the 167
   dead findings and would take the CLI project's gate from permanently red to
   green with
   zero new waivers. Start here.
2. **R8 + R9 + R11** — three small setup/UX fixes, all cheap, all cost real
   round-trips today.
3. **R5** — so the qualified-name class cannot regress a third time.
4. **R3** — the largest remaining noise source (95 findings), but the riskiest
   change on the list; do it after the cheap wins and with measurement.
5. **R13** — if audit loops on mature codebases matter; it was the dominant cost
   on the GUI project.
6. **R2** — only after re-measuring, and only if R1/R3 left something behind.
7. The rest as capacity allows.

# What this plan deliberately does not recommend

- **Removing any check.** Even the 0% ones are correct in what they report; they
  are mis-*tiered*, not wrong.
- **Blanket demotion to advisory.** See `src/audit.rs:71`.
- **Loosening `near-clones`.** ~43% action rate is good, and its four GUI-project
  consolidations were worth doing on their own merits.
- **Touching the navigation commands' behaviour.** They carried 37% of the
  analysis session with no complaint in any log.

---

# Outcome — implemented in 0.1.83

Every item landed. What each one is, in one line, and what it measured after.

| item | where | measured after |
|---|---|---|
| R1a | `concepts::signature_rarity` | agreement weight now `0.4 × 1/(population−1)`; top signature cluster on this tree 0.73 → 0.69, **0 gating** |
| R1b | `validation::CONVENTION_SIZE` | 6+ member cohorts lose 0.25; **0 gating** on this tree, a 7-member `parse_*` cohort demoted to 0.69 while a 5-member `decode_*` one stays at 0.91 |
| R2 | — | re-measured, **no reclassification needed**: `concepts` 0 gating, `validation-drift` 0 gating, `panics` 2 (both genuine). No `GATING_SCORE` was raised and no `Gate::Tiered` was demoted |
| R3 | `panics::Provenance` | five provenance rules (length/count, arithmetic over one, bounded value, local binding, loop variable, local non-fallible call); demoted rows keep their place and say `decode(in-process)` |
| R4 | `audit` `metrics-params` | second `metrics` pass on `--sort params --threshold 7`, advisory; `explain replication` now names the trade |
| R5 | `self_check::check_type_query_forms` | new invariant over `type-refs`, `takes-mut`, `enum-coverage`, `catch-all-arms`, `parallel-matches`, `divergence`; 60 (type, command) pairs, 0 violations. Reverting one `last_segment` call makes `self-check` exit 1 |
| R6 | `Suppressions::matches_tiered` | `hits` counts only what the gating tier would have kept; a `widen-int` waiver reports `hits=0, below_audit=1`. This tree's ledger moved 4 → 16 "suppressing nothing", which is the narrowing working |
| R7 | `macro_scan::count_in` | `--changed-since` reports blind spots new in the diff |
| R8 | `main::nearest_crate_root` | `cd impl_logs && unruster inventory` scans the crate and says so; an explicit `--root` that finds nothing still exits 2 |
| R9 | `main` | an empty changed set says "empty scope, not a clean result". **Exit code deliberately unchanged** — see below |
| R10 | `suppress::scan_source` | a date on the first continuation line is the date; `--suggest-waivers` warns past 100 chars |
| R11 | `waivers --undated` | lists exactly what the summary counts |
| R12 | `callers --with-imports` | `via=use` rows for import sites, behind a flag; the default four-column shape is untouched, and a note fires when import sites exist and were not asked for |
| R13 | `waivers --apply <file>` | TSV of `file, line, check, key, scope, reason`; dry-run by default, refuses every row it cannot place, `--suggest-waivers --json` now emits `waiver_check` / `waiver_key` beside `file`/`line` |
| R14 | `dead-code --transitive` | iterates the call set to a fixed point, `via=transitive after <item>` |
| R15 | `Out::row_budget_floor` | no cap can hide a gating row, each section names the command that lists its tail, and the report says how it is bounded before its first section |
| R16 | four `ctx.out.note` routes | `callers`→`module-uses`, `outline`→`at`, `dead-code`→`tests --mentions`, `metrics`→`--by`, each behind a narrow condition |
| R17 | `in_fn` | the JSON cell for the enclosing fn is `in_fn`. **Breaking for JSON consumers** that read `context` |

## Decisions taken inside the plan's latitude

- **R9 stops at the note.** The plan left the exit code open. `audit
  --changed-since` on a no-op commit is a legitimate CI shape and failing it
  would break pipelines that are doing nothing wrong; the disclosure is the part
  that costs nobody anything.
- **R13 reads TSV, not JSON.** The tool has no JSON reader and adding a
  dependency to parse six fields would be a large cost for a format `jq`
  already emits. The one-liner in the plan still works, with `@tsv` in place of
  the object.
- **R17 renamed rather than documented.** Both were offered. Renaming is
  breaking for JSON consumers reading `context`; nothing in this repo asserted
  on it, and the collision with `--context`'s snippet lines is the defect.
- **R12 and R14 add their new column only behind their flag.** Appending `via`
  unconditionally would move every existing reader's `awk`.

## What could not be verified here

The calibration steps for R1a, R1b and R3 ask for a diff against the two
codebases the evidence came from. Neither is present in this checkout, so every
number above is measured on UnRuster's own tree and on purpose-built fixtures
that reproduce the observed shapes. The predictions the plan makes about the CLI
project's ten `edit::*parse*` rows and the GUI project's 95 `panics` sites are
**untested against those trees** — run `unruster audit` on each before treating
the acceptance criteria as met.
