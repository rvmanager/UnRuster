# `contract-drift` — specification

> **Status: implemented** in `src/contract_drift.rs` (v0.1.69, 37 subcommands).
> §9 records where the build departed from this design and why. Everything
> above §9 is the design as written; §10 records the first field test against
> another codebase and §11 the second. Read all three before treating §1–§8 as a
> description of the code.

Compare a function's implementation against the contract its callers assume.

---

## 1. Why this is a new shape

Every existing check is **horizontal**: it compares siblings to each other
(`divergence`, `config-drift`, `builder-drift`, `clones`, `cohort-callees`,
`arith-drift`). `contract-drift` is **vertical**: it compares one function's
implementation against the aggregate expectation of everything that calls it.
That axis is currently uncovered.

It is also the first command where **unruster is not the analyst**. It is a
dossier builder with an enforced reveal order. The reasoning is done by the
reader (human or agent) in five steps:

| Step | Who | What |
|:--|:--|:--|
| 1 | reader | pick a target `x` (or take one from `--candidates`) |
| 2 | reader | read every caller of `x`, without reading `x` |
| 3 | reader | write the expectation — prose or a mini-spec — implied by those callers |
| 4 | reader | read what `x` actually does |
| 5 | reader | compare, and name the disagreements |

The tool's job is steps 2 and 4: assemble the material, in that order, and
**refuse to hand over step 4's material during step 2**. Contamination is the
failure mode and it is silent — once the body has been read, the expectation
derived afterwards is worthless, and nothing in the output would show it.

### 1.1 Why it is not `callers --context 10` + `show`

That composition gets ~70% of the way and loses the part that matters:

1. **It shows the body on request.** Nothing stops (or even discourages) an
   agent from reading the implementation first. The blindfold is the product.
2. **`--context N` clips the evidence.** The expectation lives in what the
   caller does *after* the call — `?`, `.unwrap()`, `let _ =`, a `match` on the
   result, a loop around it, an `assert!` before it. A ±10-line window cuts
   exactly that. The unit is the whole enclosing fn.
3. **No deterministic evidence layer.** Return-value disposition, argument
   shape, and call environment are all AST-visible facts. Making the reader
   re-derive them from prose wastes the one thing this tool is good at, and
   makes the derivation unauditable.
4. **No bounding.** A target with 60 callers is not readable. Needs `--top`
   with diversity-aware selection, not the first ten alphabetically.

---

## 2. Grammar

### 2.1 Invocation

```
unruster contract-drift <NAME>              # phase 1 — callers, body withheld
unruster contract-drift <NAME> --reveal     # phase 2 — doc, body, callees
unruster contract-drift --candidates        # rank targets worth the exercise
```

`<NAME>` uses the **`callers` query grammar verbatim** (reuses
`callers::matches_target`):

| Form | Matches |
|:--|:--|
| `resolve_scope` | free fns, methods, macros by last segment |
| `Doc::write` | paths ending in `Type::method` |
| `.write` | method calls only |
| `::open` | free-fn paths only |
| `render!` | macro invocations only |

Unknown target → `ctx.warn_unknown("fn, method, or macro", query)` on the way
in, and `TargetNotFound::err(...)` (exit 2) if nothing resolved — same as
`run_callers`.

`--candidates` takes no `<NAME>`; the two are mutually exclusive
(`conflicts_with`), and `--reveal` requires `<NAME>`.

### 2.2 Command-local flags

| Flag | Default | Meaning |
|:--|:--|:--|
| `--reveal` | off | Phase 2: print the target's doc comment, body, and callee list. Prints **no** caller material. |
| `--candidates` | off | Target-selection mode. No `<NAME>`. |
| `--no-bodies` | off | Phase 1 emits rows and the usage table only, no caller source. |
| `--max-lines <N>` | `80` | Per-caller source cap. `0` = uncapped. Same semantics as `show --max-lines`: it names the lines it dropped and the flag that lifts it. |
| `--min-callers <N>` | `3` | `--candidates` only: floor below which no cross-caller consensus exists. |
| `--min-confidence <tier>` | none | Reused from `callers`. Drops low-tier caller matches. |

`--top` is global and applies to the caller list (see §4.4). Every other global
flag applies unchanged: `--scope`, `--changed-since`, `--exclude`, `--cfg`,
`--json`, `--spans`, `--summary`, `--all-stdout`, `--fingerprints`, `--context`.

### 2.3 Why bodies are on by default

`show`'s doc comment already records the lesson: an agent needing 115 items
sized 115 calls as "slow but manageable" and went looking for another way,
because the parse — not the lookup — is the cost. Emitting rows and making the
reader issue N follow-up `show` calls reintroduces exactly that. One invocation
pays the parse once and returns everything step 2 needs.

### 2.4 Why there is no `--both`

A flag that prints phase 1 and phase 2 together defeats the only mechanism this
command has. It would be used — by any agent optimising for round-trips, and by
any human who forgets what the ordering is for — and its use would be invisible
in the output. Two invocations is one extra keystroke. The blindfold is not
advisory.

---

## 3. Output

TSV by default, sections in fixed order, following `audit`'s section model
(`Out::section` defers the `## title` header until the section emits something).

### 3.1 Phase 1 — `contract-drift resolve_scope`

```
## target
kind  vis  name                  at                    body  callers
fn    pub  scope::resolve_scope  src/scope.rs:88-141   withheld  12

pub fn resolve_scope(root: &Path, spec: &ScopeSpec) -> anyhow::Result<Vec<PathBuf>>

## callers
via        at                in                    in_at                 ret       args              env
resolved   src/main.rs:412   main::run             src/main.rs:390-455   ?         var, var          —
resolved   src/audit.rs:96   audit::run_battery    src/audit.rs:70-190   unwrap    var, const        loop
heuristic  src/show.rs:301   show::resolve         src/show.rs:290-320   match:2   var, default      cond, guarded

## usage
fact              sites  callers  detail
ret:?                 7        7  propagated
ret:unwrap            1        1  src/audit.rs:96
ret:match:2           2        2  both arms bound; no site inspects emptiness
arg2:default          3        3  `ScopeSpec::default()`
env:guarded           4        4  `if root.exists()` precedes the call
env:loop              1        1  src/audit.rs:96

## caller sources
<enclosing fn source, one block per caller row, `>` marking the call line>
```

Then, on **stdout** (`Out::answer`, not `Out::note` — see §3.4):

```
the body of `scope::resolve_scope` (src/scope.rs:88-141) was withheld on
purpose. write the expectation these 12 callers imply, then run
`unruster contract-drift resolve_scope --reveal`. do not open
src/scope.rs:88-141 first — an expectation written after reading the
implementation is not evidence of anything.
```

**The signature is shown.** Types, generics, `Result`/`Option`, lifetimes and
`&mut` are part of the contract, not part of the implementation — a reader
denied them would invent an expectation the compiler already rules out.

**The doc comment is withheld.** This buys a three-way comparison in step 5:
what callers assume, what the doc promises, what the code does. Doc-vs-callers
disagreement is a finding in its own right, and usually the cheaper fix.

### 3.2 Phase 2 — `--reveal`

```
## target
kind  vis  name                  at                   lines
fn    pub  scope::resolve_scope  src/scope.rs:88-141     51

<doc comment>
<full source>

## callees
callee                    at                 n
walkdir::WalkDir::new     src/scope.rs:94     1
Path::exists              src/scope.rs:101    2
...
```

Callees are included because they are the implementation's own contract surface
— the fastest read on "what does it actually do" is often "what does it call".
Reuses `callers::run_callees`.

Phase 2 deliberately re-prints **none** of the caller material. It is already in
the transcript above it; reprinting doubles the cost of the expensive half and
blurs the boundary the two phases exist to draw.

### 3.3 `--candidates`

```
## candidates
score  name                      at                    callers  mods  loc  ret       doc
0.86   scope::resolve_scope      src/scope.rs:88-141        12     4   51  Result    —
0.71   index::lookup_qpath       src/index.rs:210-244        8     3   35  Option    yes
```

Ranked by, in order of weight:

| Signal | Why |
|:--|:--|
| caller count ≥ `--min-callers` | below 3, there is no consensus to derive a contract *from* |
| callers spread across ≥ 2 modules | a contract crossing a module boundary is one nobody owns |
| body LOC | more room for the implementation to have drifted |
| returns `Result`/`Option` | the contract has a failure axis, which is where drift hides |
| no doc comment | nothing is written down, so the contract exists only in callers' heads |

Score is a 0..1 ratio in the style of `arith_drift::score`, printed to 2dp.
`--candidates` is a listing, not a verdict: it says which targets are *worth*
the exercise, never that they are wrong.

### 3.4 Channel discipline

The withheld-body note goes through `Out::answer` (stdout), not `Out::note`
(stderr). `Out::answer`'s doc comment records why: agents suppress stderr
routinely, and a session that ran `show <name> 2>/dev/null | head -30` got total
silence four times and each time concluded the tool had nothing. A blindfold
instruction that lands on a suppressed channel is a blindfold that is not
applied. Same reasoning, same channel.

### 3.5 JSON

`--json` emits one document. Phase 1 **must** carry the withholding as data, not
only as prose:

```json
{
  "target": {
    "name": "scope::resolve_scope",
    "file": "src/scope.rs", "line": 88, "end": 141,
    "sig": "pub fn resolve_scope(root: &Path, spec: &ScopeSpec) -> anyhow::Result<Vec<PathBuf>>",
    "body": null,
    "doc": null,
    "withheld": true
  },
  "callers": [ ... ],
  "usage": [ ... ],
  "notes": [ "the body of ... was withheld on purpose ..." ]
}
```

`"withheld": true` with `"body": null` lets a JSON consumer assert it is in
phase 1 rather than infer it from a missing key. Under `--reveal`,
`"withheld": false` and both fields are populated.

---

## 4. The deterministic evidence layer

This is what makes the command more than `cat`. All three vocabularies are
closed sets, AST-derived, and emitted per call site.

### 4.1 `ret` — return-value disposition

Exactly one per site, first match wins:

| Value | Shape |
|:--|:--|
| `?` | `f(..)?` — propagated |
| `unwrap` / `expect` | asserted (contract violation = crash) |
| `ok` / `unwrap_or` / `unwrap_or_default` / `unwrap_or_else` / `map_err_` | swallowed — overlaps `error-swallows`' vocabulary deliberately |
| `match:N` | scrutinee of a `match` with N arms |
| `if-let` / `let-else` / `while-let` | conditional binding |
| `returned` | tail expression of the caller |
| `arg:<callee>` | passed straight into another call |
| `field:<Type>.<name>` | stored into a struct literal or assigned to a field |
| `bound` | `let x = f(..)` and nothing above applies |
| `discarded` | `let _ = f(..)`, or statement position with the value dropped |

`discarded` and `swallowed` are the high-signal rows: a caller that throws the
result away is asserting the call is infallible, which the implementation may
disagree with.

### 4.2 `args` — argument shape, per position

| Value | Shape |
|:--|:--|
| `literal` | a literal, spelled out in `detail` |
| `const` | path resolving to a `const`/`static` |
| `default` | `Default::default()`, `None`, `vec![]`, `""`, `0` |
| `field` | `self.x`, `cfg.root` |
| `call:<callee>` | the result of another call |
| `var` | anything else |

A parameter that is `literal` or `default` at every site is a parameter the
implementation may treat as more variable than it is — and a candidate for
deletion regardless of what step 5 concludes.

### 4.3 `env` — call environment

| Value | Shape |
|:--|:--|
| `loop` | inside a `for` / `while` / `loop` — cost and idempotence become contract terms |
| `cond` | inside an `if` branch or `match` arm |
| `guarded` | a preceding `if` / `assert!` / `debug_assert!` in the same block mentions one of the argument bindings. **Highest-value single fact in the table** — it is a precondition the caller believes it must establish, written down nowhere else |
| `repeated` | the same target is called more than once in the enclosing fn |
| `paired:<name>` | nearest sibling call in the same block, in both directions — the `co-call` signal, as evidence rather than as a finding |

### 4.4 Caller selection when `--top` bites

Default `--top` for this command: **10**. Selection is diversity-first, not
positional — take one caller per distinct module before taking a second from
any module, then order within by `via` tier (`exact` > `resolved` > `inferred`
> `heuristic`).

The cut announces itself. `Out::cap_note` already does this and the rule is
codified in `--top`'s own help text: *a silent truncation reads as "that is all
there is"*. That failure is worse here than anywhere else in the tool, because a
truncated caller set produces a confidently-wrong contract rather than a short
one.

### 4.5 Confidence

Reuse `Confidence` and `callers::site_confidence` unchanged; the `via` column is
the first column of every caller row. A `heuristic` row — a shadowed local
binding, or a bare name with several definitions — is a caller that **may not be
calling this function at all**, and one such row poisons the derived
expectation. When any caller row is below `resolved`, emit the count through
`Out::note` and name `--min-confidence resolved` as the fix, mirroring
`run_callers`' existing shadowed-binding note.

### 4.6 Test callers

Default `--scope production` hides test callers, and tests are *the best
available* contract evidence: a test states expected behaviour explicitly rather
than implying it. When the target has test callers that scope hid, say so and
recommend `--scope all` — the top-level help already promises that usage
commands report when they hid tests.

---

## 5. What this command is not

- **Not a check.** It emits material, not findings. Nothing is scored as a
  defect, so there is nothing to gate on.
- **Not in `audit`.** `audit::CHECKS` is a battery of deterministic checks that
  exits 1 while gating findings remain. `contract-drift` produces no findings
  and, with bodies on, unbounded output. Adding it would make `audit`
  un-gateable and unpipeable. `implies_fail_on_findings` → **false**.
- **Not waivable.** No entry in `suppress::WAIVABLE_CHECKS`. That list is a
  registry of frozen, user-facing identifiers ("renaming is a breaking change to
  files this tool does not control"), and a name in it that never calls
  `retain_unsuppressed` is a name users can write into source comments that
  waives nothing. There is no per-site judgment here to waive.
- **Not `--fail-on-findings`-eligible.** Exit 0 on success, 2 on an unresolvable
  target.

---

## 6. Implementation touchpoints

New file `src/contract_drift.rs`, plus:

| File | Change |
|:--|:--|
| `src/main.rs` | `Cmd::ContractDrift(ContractDriftArgs)` + doc comment; `name_of_cmd` → `"contract-drift"`; arm in `implies_fail_on_findings` (`false`); `ContractDriftArgs` struct; dispatch arm |
| `src/emit.rs` | `kind_of_check` arm → `"site"` (caller rows point at lines to open; the target header is an `item` row in the `show` style) |
| `src/playbook.txt` | new `◇ CONTRACT-DRIFT (implementation vs. what callers assume)` section — `explain contract-drift` falls out of it with no code change |
| `src/main.rs` help preamble | one line in `AGENT QUICKSTART` |
| `tests/` | see §7 |

Reused wholesale — this command should add analysis, not machinery:

| Reused | From |
|:--|:--|
| `collect_sites`, `matches_target`, `site_confidence`, `query_known`, `query_unique` | `callers.rs` |
| `run_callees` | `callers.rs` (phase 2) |
| `range_of`, `print_source`, `header`, `extent_of` | `show.rs` |
| `ScopeTracker` | `ast.rs` — gives `in` / `in_at` and `--spans` for free |
| `retain_changed`, `warn_unknown`, `TargetNotFound` | `context.rs` |
| `row!`, `Out::section`, `Out::answer`, `Out::cap_note`, `Out::summary` | `emit.rs` |

Genuinely new: the three evidence vocabularies (§4.1–4.3), diversity-first
caller selection (§4.4), the candidate score (§3.3), and the withholding.

### 6.1 Summary lines

```
(12 caller(s) across 4 module(s); body withheld — `--reveal` for the implementation; explain: contract-drift)
(51 line(s); 9 distinct callee(s); explain: contract-drift)
(17 candidate(s); min_callers=3; explain: contract-drift)
```

---

## 7. Tests

| Test | Asserts |
|:--|:--|
| `phase_one_never_emits_body` | over a fixture where the target body contains a unique sentinel token, phase-1 stdout **and** stderr contain it zero times |
| `phase_one_json_marks_withheld` | `body: null`, `doc: null`, `withheld: true` |
| `phase_one_emits_signature` | the signature *is* present — the blindfold must not overshoot |
| `reveal_emits_body_and_callees` | and emits no `## callers` section |
| `withheld_note_on_stdout` | the note survives `2>/dev/null` |
| `ret_dispositions` | fixture with one caller per §4.1 value; each classified exactly |
| `arg_shapes` / `env_facts` | same, per §4.2 / §4.3 |
| `guarded_detects_preceding_if` | the precondition case specifically |
| `top_cut_announces_itself` | 30 callers, `--top 5` → cap note names the 25 |
| `top_selection_is_diverse` | 30 callers across 6 modules, `--top 6` → 6 distinct modules |
| `heuristic_callers_noted` | shadowed-binding caller produces the `--min-confidence` note |
| `hidden_test_callers_noted` | default scope with test callers → the note fires |
| `candidates_respects_min_callers` | nothing below the floor is listed |
| `unknown_target_exits_2` | `TargetNotFound` path |
| `not_in_audit_battery` | `audit::CHECKS` does not contain `"contract-drift"` |
| `not_waivable` | `WAIVABLE_CHECKS` does not contain `"contract-drift"` |

`phase_one_never_emits_body` is the load-bearing one. Everything else in this
spec is convenience; that test is the product.

---

## 8. Open questions

1. **Targets other than fns.** The same trick works on a struct field (what do
   writers assume about it?) and on a trait method (what do impls assume the
   caller does?). v1 is fns, methods and macros — the `callers` grammar covers
   them for free. Extending to fields means a second query grammar.
2. **`--candidates` and `--changed-since`.** `contract-drift --candidates
   --changed-since HEAD~1` = "which functions did I just touch that enough
   callers depend on to be worth blindfolding?" This is probably the highest-
   yield entry point in the whole command and should be the example in the
   playbook section.
3. **Should phase 1 hide `#[derive]`/attributes too?** Attributes are part of
   the signature contract (`#[must_use]`, `#[deprecated]` especially) — lean
   yes, show them. Confirm against a real target before freezing.
4. **Multi-target batching.** `show a b c` parses once where three calls parse
   three times, and the same argument applies here. Deferred: the output of one
   target is already long, and concatenating three is the failure mode
   `emit_hits` exists to prevent.

---

## 9. As built — deviations from the design above

Six changes, five of them forced by running the command on this repo.

### 9.1 Recursive call sites are excluded (new — not in the design)

Not anticipated, and a hole in the blindfold. Run against
`semantic::infer_expr_type`, six of thirteen "callers" were the target calling
itself, and their `args` column spelled out the expression kinds its own
`match` dispatches on — the implementation leaking into phase 1 through the
caller table. A fn calling itself is the implementation, not evidence about it.

Excluded, and the exclusion is announced: a caller count that silently
disagrees with `unruster callers <name>` is worse than disclosing that the fn
recurses.

### 9.2 `--top` counts callers, and clears the row budget

`--top` is a **whole-run** row budget, re-set per section only by `audit`.
`contract-drift --top 2` therefore spent the allowance on the target header and
emitted an empty `usage` table. Phase 1 now clears the budget and enforces its
own diversity-first cut, so `--top` means what a reader of this command means
by it. `--candidates`, a plain ranked listing, keeps the global behaviour.

The cut note goes through `row_note` (stdout), not `note` (stderr) — same
reason the global `--top` note does, and more so here: these rows were chosen
to be unalike, and a reader who takes them for "the first N" reads a sample as
the population.

### 9.3 `--candidates` needed two attribution guards

Both found on the first run against this repo:

- **Same-named fns.** Call sites match by last segment, and this tree has 30
  fns named `run`. Each was credited with all 55 `run(…)` sites, so every
  command's entry point ranked 0.88 on evidence belonging to the other 29.
  Now skipped, with the count disclosed (267 fns here).
- **Method names.** `Suppressions::len` was credited with all 321 `.len()`
  calls in the crate. A `via` column now tags every method row `heuristic`,
  and the score halves the caller-derived weight for those rows so an
  unattributable count cannot outrank an attributable one.

### 9.4 Three dispositions the design's vocabulary missed

All three showed up as `bare`, which says nothing:

| Added | Shape |
|:--|:--|
| `arm-value` | `Expr::Paren(p) => f(&p.expr)` — the commonest dispatch shape in Rust |
| `closure:<method>` | `xs.and_then(\|e\| f(e))` — *which* combinator consumes it is the expectation |
| `chained:<method>` | `f(x).map(…)` — a method applied to the result that is not one of the named swallow/assert forms |

Plus `cond-test`, `compared`, `assign`, and `closure-tail` as fallbacks. After
these, `bare` no longer appears on either target tested.

### 9.5 `body: "withheld"` rather than `withheld: true`

The design asked for a boolean beside a null `body`. A single cell that reads
`withheld` / `shown` works in TSV and JSON alike, and a second column carrying
the same fact in a different type is one more thing to keep consistent. A JSON
consumer asserts `row.body === "withheld"`.

### 9.6 `paired:<name>` deferred

The `co-call`-style "nearest sibling call in the same block" env fact is not in
v1. The other four (`loop`, `cond`, `guarded`, `repeated`) are, and `guarded`
carries the value the design predicted it would.

### 9.7 Confirmed by the first real run

`contract-drift type_to_string --reveal` on this repo. The callers imply a
total function whose `String` is a **type identity** — `index` builds an impl
block's `qpath` from it, `fields` stores it as `FieldDef.ty`, `casts` compares
source against target. The implementation is deliberately lossy in five
variants: `[u8; 4]` and `[u8; 32]` both render `[u8; _]`, every `impl Trait` is
`impl _`, every `dyn Trait` is `dyn _`, every fn pointer is `fn(_)`, and the
catch-all renders `_`, which is also the rendering of an explicit inferred
type. No doc comment says so.

Reproduced: two impls on `[u8; 4]` and `[u8; 32]` get the *same* `qpath` from
`unruster impls`, and two struct fields of those types are indistinguishable in
`unruster fields`. Whether that matters is a judgment call — which is the
posture the command was built for.

---

## 10. Field test — `impl_logs/math2svg01.log` (v0.1.67 → v0.1.68)

First run against a codebase that is not this one. The exercise itself worked:
the blindfold held, an expectation was written before the reveal, and it
produced the finding it was designed for — `Cx::num_prop` returns `None` for
both a *missing* and an *invalid* property, so the callers doing
`.unwrap_or(default)` (visible as `ret:unwrap_or` in the usage table) silently
apply a default over a typo. No other check in this tool would have found that.

The target-naming grammar did not work at all. Ten of ten qualified queries
gave a wrong answer, and the session responded by abandoning qualified names
for the rest of its life.

### 10.1 Qualified queries named a spelling, not an item

A call site records the callee **as written** — `n(…)`, `.leaf(…)` — so
`matches_target`'s `ends_with("svg::n")` matched only the sites that spell the
path out. Eight queries returned a confident zero; two returned a silently
truncated set:

| Query | Reported | Actual |
|:--|--:|--:|
| `render::text`, `Theme::resolve`, `render::dash`, `render::arrowhead`, `Cx::{num,req_num,uint,req_uint}_prop` | 0 | 19, 17, 6, 7, 13, 18, 4, 4 |
| `svg::n` | 2 | 164 |
| `svg::esc` | 1 | 4 |

**The zeros were loud; `svg::n` was the hazard.** It returned a plausible
two-caller dossier drawn from 1.2% of the callers, with nothing in the output
saying so. For `callers` a short list is a short list; here the premise is
"everything that calls it", so a sampled caller set yields a confident *wrong
contract*.

**Fix.** A qualified query that resolves to one indexed fn whose bare last
segment no other fn shares is matched on the bare segment — `resolved`
confidence under the rule `site_confidence` already encodes. When the name is
shared, widening would mix items, so the narrow match stands and the shortfall
is reported. Zero now explains which kind of zero it is.

### 10.2 The tool had already learned this, and the new command did not inherit it

`callers::note_narrower_than_bare` exists for this exact failure; its doc
comment records an earlier session that took a confident zero and went to
`grep`. `contract-drift` never called it. A sibling divergence, in the tool
built to find sibling divergences.

That note also fired only at *zero*, so it would still have missed 2-of-164.
It now warns on any shortfall — but only when the bare name belongs to one fn.
The first version warned unconditionally and immediately produced
`Document::new matched 3, but 6 call something named new`, which is noise: the
other three are other types' `new`, and the narrow answer was correct. The
gate is the same uniqueness test that makes widening safe.

### 10.3 `--candidates` recommended the one form that cannot work

The skipped-rows note said ``contract-drift <Type::method> names one
directly``. Those rows are skipped *because* the bare name is shared — the
same condition that stops a qualified query resolving. Reworded.

### 10.4 Step 1's output was not valid input to step 2

`--candidates` prints `name` as a qpath, which is right for `show` and was
useless as a caller query. Fixed by §10.1: every listed candidate has a unique
bare name, which is exactly the widening condition. There is now a test
asserting the round-trip, because that property is what broke.

### 10.5 Still open

- **Batching defeats the design.** The session ran phase 1 across four targets,
  then `--reveal` across six in one command. It wrote one combined expectation
  and got away with it, but the exercise assumes one target at a time. Either
  support batches explicitly or say not to.
- **`--top 0`** means "all" here and "show nothing" everywhere else.
- **The candidate score still over-rewards ubiquitous small helpers.**
  `svg::n` — a five-line float formatter — scored 0.61 on 164 callers, the same
  shape as `ast::line_of` scoring 0.69 on this repo. Two data points now. The
  fix is a body-size floor and less weight on breadth, but tuning on two
  codebases is still tuning on two codebases.

---

## 11. Second field test — `impl_logs/svggen_contract_drift.log` (v0.1.68)

A larger codebase, "focus on functions recently changed, find 5 defects". The
command carried the session: `--candidates --changed-since HEAD~3` picked the
targets, six exercises ran, and five defects were reported and accepted (three
were then fixed). §10's naming fixes held — no query was retried, and
`trace::round` resolved as typed where v0.1.67 would have returned zero.

Two defects, one of them introduced by §10's own fix.

### 11.1 Widening threw away the call form (regression from §10.1)

`trace::round` is a private free fn. No other `round` is indexed — but
`f64::round` is not indexed *either*, so the bare-name scan collected 65
callers across 13 modules, nearly all of them `.round()` on a float, and marked
every one `resolved`. The note asserted "every site calling it is this one".

This is the `Suppressions::len`-credited-with-321-`.len()`-calls problem from
§9.3, which `--candidates` was already guarded against with a `heuristic` tier.
The reasoning was not carried into widening.

It was caught immediately — a private fn cannot have callers in thirteen
modules — which makes it the cheapest kind of wrong answer to produce and the
most expensive kind to have produced. Had the target been `pub`, the 65-caller
set would have looked entirely reasonable and the derived contract would have
been about `f64`.

**Fix.** Widening preserves the call form: `::name` for a free fn (free-fn
paths only), `.name` for a method (method calls only). Neither collects the
other's homonyms. Plus a visibility guard — a `priv`/`pub(self)` target cannot
be called outside its own module subtree, so widened sites landing elsewhere
are dropped and counted. And the note no longer promises what the index cannot
know: a name unique *in this tree* can still be a method on a type defined
outside it.

### 11.2 The blindfold is advisory outside the command

One exercise in six used `--reveal`. The other five read the body with
`unruster show`, and one did it in a single shell command:

```
unruster contract-drift trace::round --no-bodies …; echo "=== REVEAL ==="; unruster show trace::round
```

Labelled "=== REVEAL ===" — so this was not evasion. The session believed it
was performing phase 2, and there was no moment in which an expectation could
exist.

§2.4 argued against a `--both` flag because "the blindfold is not advisory".
That is true only inside the command. `show`, `sed`, `cat` and an editor all
reach the same bytes, and phase 1 never said so.

**Fix (partial, and it is the honest kind of partial).** The withheld-body note
now names the bypasses and says that using them in the same breath as phase 1
leaves no moment for an expectation. That is guidance, not enforcement — the
tool cannot observe what else a reader runs. §2.4's claim is amended: the
withholding is enforced *within the command*, and the ordering across commands
is a discipline the caller keeps. Which is why the instruction text in the
project's collaboration notes asks for the expectation as **visible output**:
that is the only artifact anything downstream can check.

### 11.3 Minor

- The session piped `contract-drift` through `grep -n`, which destroys the
  section structure the usage table lives in. `audit`'s help says "never pipe
  it"; this command has the same property and says nothing.
- `--candidates --changed-since HEAD~3` was the entry point both times it was
  offered. That prediction from §8 holds.
