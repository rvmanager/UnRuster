# unruster — validated defect list & fix priorities

> **Status: all items resolved in v0.1.64.** Each section below carries a
> `RESOLVED` line saying what changed and where. Item 9 needed no code change —
> the tool already reported everything the finding asked for, and the finding
> was a correction to the evaluation's reasoning rather than to the tool.


Source: `impl_logs/unruster-eval.md` + `impl_logs/unruster-eval.log` (200 uv
changelog defects, audit run at commit A of each fix).

Every item below was re-checked against `src/` at v0.1.63 and, where possible,
reproduced against live output. Status is one of **CONFIRMED** (reproduced),
**CONFIRMED-BY-CODE** (read in source, not run), **CORRECTED** (report's claim
is wrong or incomplete), or **OUT OF SCOPE** (not a unruster defect).

---

## P0 — output correctness. Small, mechanical, blocks machine consumption.

### 1. Multi-site rows emit duplicate `file`/`line` keys · CONFIRMED

`push_row` ([src/emit.rs:830-849](src/emit.rs:830)) hardcodes `"file"` /
`"line"` for every `Val::Site` and `Val::Span`, **discarding the cell key `k`**.
Any row carrying two site cells emits the two keys twice. A last-wins parser
(Python, JS, serde into a struct) keeps the *second* site and silently
attributes the finding to the wrong file and line.

Reproduced on `unruster audit --json` over this repo:

| section | rows with duplicate keys |
|:--|--:|
| `divergence` | 8/8 |
| `divergence --handling` | 6/6 |
| `conversion-pairs` | 2/2 |
| `config-drift` | 2/2 |
| `builder-drift` | 1/1 |

Also hits `audit::print_diff` `moved` rows ([src/audit.rs:600](src/audit.rs:600)).

Worse than the report says: for `conversion-pairs` the two sites are *anonymous*
(no `lean`/`vs` label cell at all), so the ordering is the only discriminator —
even a pairs-hook consumer cannot say which site is which.

These five checks are exactly the ones that produced 3 of the 4 genuine
detections. The tool's best signal is the signal it corrupts on the way out.

**Fix:** derive the key from the cell name — first site stays bare
`file`/`line` (back-compat), subsequent ones become `<key>_file` / `<key>_line`;
or nest as `"lean": {"file":…, "line":…}`. Give `conversion-pairs` real cell
names while you are in there.

**RESOLVED** — [emit.rs](src/emit.rs): `push_row` now names the second and later
site cells after their own column (`vs_at` → `vs_file`/`vs_line`, `reverse_at` →
`reverse_file`/`reverse_line`), and the first keeps the bare names every existing
consumer is written against. Covered by
`a_row_naming_two_sites_does_not_emit_file_twice` and
`conversion_pairs_names_both_of_its_sites`; a sweep of 26 command/flag
combinations now reports 0 duplicate keys.

### 2. A check's `context` column collides with the emitter's `context` array · CONFIRMED

Separate bug, same symptom. Many checks emit a cell literally named `context`
(the enclosing fn). `push_row` also appends the `--context N` source snippets as
`"context": [...]`. Both land in one object.

This one is worse than #1: the two values have **different types** (string vs
array), so a typed consumer doesn't mis-attribute, it crashes.

Reproduced with `--context 1`: `casts` 42/42, `stringly` 122/122, `conversions`
557/557, `catch-all-arms` 21/21, `variants` 474/669, `error-swallows` all rows.
It fires in the **default** `audit` too, because `audit` auto-sets
`CONTEXT_LINES` for the `stringly` section ([src/audit.rs:458](src/audit.rs:458)) —
20/20 stringly rows in a plain `unruster audit --json`.

**Fix:** rename the emitter's snippet key to `context_lines` (or rename the
column to `in`, which `config-drift` already uses for the same concept).

**RESOLVED** — [emit.rs](src/emit.rs): the snippet array is now `context_lines`.
The column keeps its name, so `--fields context` and every TSV pipeline are
untouched. Covered by
`a_context_column_does_not_collide_with_context_snippets` across four checks and
`audit_json_defaults_have_no_duplicate_keys_anywhere`.

### 3. `error-swallows` is the only long section in `audit` with no default cap · CONFIRMED-BY-CODE

[src/audit.rs:405](src/audit.rs:405) passes `cap: None`. Every other section
that can run long carries one — divergence 40, clones 20, config-drift 10,
builder-drift 10, stringly 20, metrics 20. On uv that let `error-swallows` emit
665 of ~800 rows (82% of the battery) for 2 genuine hits.

Rows are already score-sorted ([src/error_swallows.rs:735](src/error_swallows.rs:735)),
so a cap keeps the top tier and drops the tail, and the emitter's cap note
already announces the truncation. `--top` overrides it, counts and `--since`
baselines are unaffected (the cap is applied after fingerprint recording).

**Fix:** `Some(40)`, matching `divergence`. One line.

**RESOLVED** — [audit.rs](src/audit.rs): `ERROR_SWALLOWS_TOP = 40`, applied to
`error-swallows` and the new `panics` section. Counts, waiver hits and `--since`
baselines are unaffected (the cap runs after fingerprint recording) and
`cap_note` announces the truncation. Covered by
`the_highest_volume_check_is_capped_like_every_other_long_section`.

---

## P1 — make the signal reachable. Small to medium.

### 4. `audit` cannot select or drop checks · CONFIRMED

No `--only` / `--skip`. The report's headline recommendation — "run
`audit --top 20` while skipping `error-swallows`, and 2 of the 4 genuine hits
land in a reviewable list" — is not expressible as a command. `--top` half
already works; the skip half does not. Check names already exist as strings
(`ctx.out.set_check(name)`), so the plumbing is there.

**Fix:** `--only <check>[,…]` / `--skip <check>[,…]`, validated against the
battery's name list; mention skipped checks in the summary line so a shortened
report can't read as a clean one.

**RESOLVED** — [audit.rs](src/audit.rs): `audit::Selection`, wired to
`--only`/`--skip` (repeatable or comma-separated). A skipped check does not run
at all; the closing line names every one left out and how many of the battery
ran. The selection is honoured by the `--since` baseline pass too, or a baseline
that ran a skipped check would report all of its findings as `gone`. An unknown
name is an error listing the valid ones, and selecting nothing is an error.
Covered by four tests including `an_unknown_check_name_is_an_error_not_a_silent_no_op`.

### 5. Two score-ranked checks have no `--min-score` · CONFIRMED

`divergence`, `config-drift`, `builder-drift` all expose `--min-score`.
`error-swallows` (gate 0.55) and `clones` (gate 0.75) rank their rows and
expose nothing. A user who reads the eval and wants "only the tier that gates"
has no flag for it.

**Fix:** add `--min-score` to both, defaulting to 0.0 to keep today's output.

**RESOLVED** — `error-swallows --min-score` and `clones --min-score`, both
defaulting to 0.0. The floor is applied *before* the counts, so a filtered row
stops being a finding rather than becoming a hidden one, and the summary says
how many it removed — unlike `--top`, which only bounds the listing.

### 6. Test-support *crates* are scanned as production · CONFIRMED-BY-CODE

`out_of_scope` ([src/parse.rs:90](src/parse.rs:90)) knows only `tests/` and
`benches/` directories and `tests.rs` / `*_test(s).rs` filenames. A crate like
`crates/uv-test` is ordinary library code by every one of those tests, so
`--scope production` scans it. In the eval it produced `error-swallows` and
`divergence --handling` rows over test scaffolding, and two coincidental
"near the fix" hits (#148, #197) were entirely uv-test rows.

**Fix, cheap:** treat a workspace member whose package name is `test`,
`*-test`, `*-tests`, `test-*` or `*-test-utils` as test scope, and say so in
the summary. **Fix, precise:** walk `Cargo.toml`s and mark members reachable
only through `[dev-dependencies]`. Today's workaround is
`--exclude 'crates/uv-test/**'`.

**RESOLVED** (cheap form) — [parse.rs](src/parse.rs): `in_test_support_crate`
reads the `[package] name` of the nearest ancestor manifest and classifies
`test`, `tests`, `test-*`, `*-test`, `*-tests`, `*-testing`, `*-test-utils`,
`*-test-support` as test scope, cached per directory. The scope note names the
rule separately from the other two, because "it was a test file" is not an
explanation a reader can check by opening `crates/foo-test/src/lib.rs`. The
dev-dependency graph form was not built: it is strictly more precise but needs a
real TOML parser, and the naming rule covers the case that motivated this.
Fixture: `fixtures/testcrate` (a two-member workspace).

### 7. JSON has no machine-readable check name or finding kind · CONFIRMED

The section title is prose (`"[medium] metrics — fns with cyclo >= 15 (explain:
god-function)"`); a consumer has to regex it. This is what makes the report's
point 3 (`metrics` produces degenerate findings) unavoidable: a 1247-line,
cyclo-183 fn is a legitimate refactor signal, but a consumer doing line-proximity
scoring cannot tell it apart from a site finding, so every defect inside that fn
"matches".

**Fix:** per-section `"check": "metrics"` and `"kind": "site" | "item" | "pair"`
in the JSON. Costs nothing at the terminal, removes a whole class of downstream
misreading. (`--spans` already gives item extents; `kind` is the missing half.)

**RESOLVED** — [emit.rs](src/emit.rs): every JSON section carries `check` and
`kind`, stamped when the section opens so an *empty* section still says which
check found nothing. `kind_of_check` is the single table: `pair` for the six
comparison checks, `item` for the ones whose row spans a whole item (`metrics`,
`dead-code`, `pass-through`, …), `site` for everything else.

---

## P2 — calibration. Needs data, not just an edit.

### 8. The `error-swallows` gate is unreachable for domain code · CONFIRMED

Both genuine detections scored **below** the 0.55 gate, and the arithmetic
([src/error_swallows.rs:47](src/error_swallows.rs:47)) says exactly why:

- #19 `Hashes::parse_fragment(fragment).ok()` → `.ok` 0.20 + `Unknown` 0.20 = **0.40**
- #190 `.unwrap_or_else(|_| dist.install_path.clone())` → 0.15 + `Unknown` 0.20 = **0.35**

Both landed in `Effect::Unknown`. That is the general case: `Effect` is
classified from stdlib-ish verb names, so *every project-specific call chain*
scores Unknown (0.20), and 0.20 + the best kind term (`let-_`, 0.30) = 0.50 —
below the gate by construction. The gate currently selects for **recognized
stdlib verbs**, not for risk, and `audit`'s own ranking buried its only two true
positives.

Do not just lower the gate (that promotes 665 rows). Add a term for the shape
both hits share — a fallback that **substitutes a value from another source**
(`unwrap_or_else(|_| other.field.clone())`, `.ok()` feeding a stored field)
rather than a default or an early return. That is data loss, and it is
structurally visible.

**RESOLVED, partly** — [error_swallows.rs](src/error_swallows.rs): a third score
term, `SUBSTITUTION_WEIGHT = 0.20`, for a fallback that hands downstream code a
*different value* rather than a default. That lifts #190
(`.unwrap_or_else(|_| dist.install_path.clone())`, 0.35) to exactly the 0.55
gate, while a defaulting fallback on the same call stays below it.

Two exemptions keep the term narrow, both found by running the new build over
this repo: a fallback built entirely from source literals is a constant however
many calls it takes to spell (`"/tmp".to_string()`), and a fallback built *out of
the error* is the "inspects" tier of the `divergence --handling` care scale, not
a substitution — `.unwrap_or_else(|e| e.into_inner())`, the poisoned-lock
recovery idiom, is that shape and the first cut promoted every one of them.

**Not resolved:** #19 (`Hashes::parse_fragment(fragment).ok()`, 0.40) is still
below the gate. It is a `.ok()`, so there is no fallback expression to classify,
and every heuristic that would reach it also promotes the whole `.ok()` family.
It is now reachable in a 40-row capped, `--min-score`-filterable section rather
than buried at position 400 of 665, which is the honest improvement.

### 9. Re-run the 54 `missing-match-arm` defects before believing the verdict · CORRECTED

The report's explanation for its largest class (54 of 200, 27%) —
"`enum-coverage`/`divergence` … only fire when a more-complete sibling site
exists to compare against" — is **true of `divergence` and false of
`enum-coverage`**. `run_enum_coverage` ([src/parallel_matches.rs:823](src/parallel_matches.rs:823))
resolves the enum's real variant list from the AST via `variant_names_of`; no
sibling is required.

What actually suppressed it is `audit`'s config ([src/audit.rs:95](src/audit.rs:95)):
`min_variants: 3`, `max_missing: Some(1)`, `hide_trait_routed: true`. A site
missing two variants, or on a 2-variant enum, or with a trait-routed catch-all,
is dropped before it reaches the section.

So the ceiling on the largest defect class is a **tuning** question, not a
structural one. Highest-value follow-up experiment: re-run those 54 with
`BatteryConfig::permissive()` coverage opts and see how many the dedicated
command would have named. Do this before committing to any new check.

**NO CODE CHANGE NEEDED** — verified against
[parallel_matches.rs](src/parallel_matches.rs): the sweep already announces every
one of its filters in the section summary — `N trait-routed catch-all(s)
hidden`, `N site(s) hidden by --max-missing 1 (drop the flag to see them)`, `N
enum(s) with <3 variants skipped (… --min-variants 0 to include)`. Nothing about
the ceiling was hidden; the evaluation's *explanation* of the misses was wrong,
and that correction is this document. The re-run experiment stands as the
recommended next measurement, and `audit --only enum-coverage` now makes it a
one-liner.

---

## P3 — new checks, ranked by corpus share × tractability.

### 10. A `panics` check — `.unwrap()` / `.expect()` scoring — **BUILT**

18 of 200 defects (`panic-removal`) and no check looks at panics at all.
Most tractable gap in the tool: same site-collection machinery as
`error-swallows` with the opposite polarity, and `divergence --handling`
already models `.expect` as the careful end of its care scale
([src/divergence.rs:577](src/divergence.rs:577)) — the site data exists, nothing
scores it.

### 11. Sibling *expression* divergence — **BUILT** (arithmetic axis)

The report's cleanest example: PR #20178 changed `corrected_initial_age +
resident_age` to `saturating_add`, where the three adjacent RFC 9111 terms
already saturated. A one-token inconsistency between siblings — conceptually
`divergence`'s exact thesis — but `divergence` only pairs enum dispatch sites
and `--handling` only pairs callee error-handling, so nothing sees it.

Generalizes well beyond arithmetic: `get` vs `get_mut`, `+` vs `checked_add`,
`unwrap` vs `?` among sibling expressions in one fn or impl. Bigger build than
#10; higher ceiling.

### 12. Macro-body `Block` fallback — **DONE**

62 unparseable macro bodies per uv run, correctly reported as a blind spot
([src/macro_scan.rs:166](src/macro_scan.rs:166)). Real, but small (62 sites
across 514 files) and the eval never located a defect inside one. Cheap partial
win: try `syn::Block` / `Vec<syn::Stmt>` before recording the blind spot —
statement-shaped bodies (`tokio::select!`, `bitflags!`) parse then.

---

## Explicitly not on the list

- **`predicate-change` (28), `error-handling` (28), `other` (28)** — 84 of 200
  defects, 0 genuine. These need semantics, not structure: a wrong `&&` is
  structurally identical to a right one. This is a scope boundary to state in
  the README, not a backlog item. Chasing them is how a consistency tool turns
  into a bad linter.
- **The "0 swallow-removals" result** in the log (~L1747) — **OUT OF SCOPE**.
  The harness ran from the scratchpad instead of the repo; the log records the
  author catching and re-running it. Not a unruster defect.
- **`--changed-since` as the recommended mode** — the report's conclusion here
  is right and already implemented. Worth promoting in the docs, no code change.

---

## Suggested order of work

1. Items 1–3 (P0). One session; all mechanical; unblocks anyone consuming JSON.
2. Item 9 (the enum-coverage re-run). Data before features — it decides whether
   P3 is worth starting.
3. Items 4–7 (P1). Each independent.
4. Item 8, then 10, then 11.

Item 1 needs a JSON-shape decision (suffixed keys vs nested object) — suffixed
keys keep every existing single-site consumer working, so that is the
recommendation unless a breaking bump is already planned.
