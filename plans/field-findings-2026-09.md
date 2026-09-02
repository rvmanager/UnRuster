# unruster — field findings, 30 Aug – 2 Sep 2026

**Status:** Tiers 1–3 implemented on 2026-09-02 (A1–A5, W1–W2, S1–S6); Tier 4
left as observations. See *Outcome* at the foot of this file for what each
item became and where it deviates from the proposal.
**Baseline:** `0.1.85` (the binary on PATH during every session below; every
note text in the logs matches the `--help` of that version).
**Evidence:** Claude Code session transcripts under `~/.claude/projects/` for
five projects, filtered to records timestamped 2026-08-30 or later. Only two
projects invoked `unruster` in that window — **svggen** (286 calls, 17
sessions) and **SchemaChef** (218 calls, 9 sessions). AutoSharpener and
Prusselise have no Rust session in the window; Vectorian's transcripts are all
older than the cutoff (their files were touched on 31 Aug, but every record
inside predates 30 Aug).

Numbers below count Bash tool calls whose command mentions `unruster`
(500) plus 4 `Write` calls that mention it in a document. 12 of the 500 exited
non-zero; 10 of those were zsh choking on `echo ===` separators the agent put
between commands, not unruster.

---

## What the tool is actually used for

| subcommand | calls | share |
|---|---|---|
| `show` | 209 (236 mentions) | 41% |
| `audit` | 181 | 36% |
| `outline` | 62 | 12% |
| `waivers` | 44 | 9% |
| `callers` | 31 | 6% |
| `field-uses` | 20 | 4% |
| `metrics`, `dead-code` | 9 each | |
| `inventory` | 8 | |
| `tests` | 4 | |
| `type-refs` | 3 | |
| everything else | ≤ 2 each | |

Two commands are 77% of all use: `show` to read and `audit` as a gate. The
bug-hunting family the global guidance sells hardest — `divergence`, `clones`,
`config-drift`, `builder-drift`, `cohort-callees`, `co-call`, `takes-mut`,
`enum-coverage`, `parallel-matches` — was invoked directly 7 times in four
days, total. Agents reach it only through `audit`, and (see A1) they read
`audit`'s exit code, not its rows. `explain` ran once, `playbook` never,
`blind-spots` never (although 137 footers pointed at it), `at` once.

**What happened right after an unruster call** (the next tool call in the
same session):

| next call | count |
|---|---|
| Bash with `grep`/`rg` | 186 |
| Bash with another `unruster` | 162 |
| Bash with `sed -n` / `cat` | 89 |
| other Bash | 37 |
| `cargo …` | 19 |
| `Read` tool | 1 |

246 of the 504 calls also contain `grep`/`rg` in the *same* command and 100
contain `sed -n`. unruster is a locator; the read still happens with text
tools. 43 calls were a *same-target fallback*: `show X | head -N` or
`outline f.rs`, immediately followed by `sed -n 'A,Bp'` on the file range
unruster had just printed, or `grep -rn X`. Each of those is a round trip the
tool had the information to save.

**What worked and should not be touched:** multi-name `show` (159 of 209
calls pass several names); `audit` as the fourth gate next to `test`,
`clippy`, `fmt` — every session's closing summary reports "`unruster audit`
exit 0"; `dead-code` confirming write-only fields (`Trace::penalty` etc.
found, fixed, re-verified in svggen); the `field-uses` strict-mode hint
("strict matched 0; `--candidates` would report 6") — the agent followed it
on the next call. `--changed-since HEAD` is the default way agents scope the
audit (91 of 181 audit calls).

---

# Tier 1 — `audit` is the gate, and it hides the gate

## A1. Lead with the gating rows; mark them; name them in the summary

**Effort:** S–M · **Risk:** low

### Evidence
svggen, 2 Sep 15:34–15:37. A 17-file change; `audit --changed-since HEAD`
reports `1 gating + 320 advisory`. The agent ran the audit **eight times** to
find the one row:

1. `| tail -40` — got the `metrics` tail.
2. `| grep -n "^## \["` then `awk` over `[high]`/`[medium]` sections `| head -60`.
3. `awk` slice from `concepts` to `config-drift` `| head -70`.
4. `| tail -3` plus a regex for scores ≥ 0.55 across every "gating at score" section.
5. `audit --help | grep gating`.
6. Redirect to a file, `awk` the section index, slice `casts` … `metrics`.
7. `--only divergence-handling --summary` — found it: `divergence --handling`,
   `partial_cmp` handled with `unwrap_or` in `emit.rs` and `expect` in `raster.rs`.
8. `--only <nine other checks> --summary` to confirm nothing else gated.

The row that held the exit open sat at line 6 of a 353-line digest, in a
section whose header does not say it gates (`## [high] divergence --handling —
one callee, different care`), in a row indistinguishable from advisory rows.
The header note says "read it whole rather than piping it to `head`", and 52
of the 181 audit calls piped, redirected or `--summary`'d it anyway — a
353-line result is past what an agent will read whole in a tool result.

SchemaChef, 30 Aug 04:57–05:01, same shape: exit 1, agent wrote the digest to
`/tmp/audit.txt` and grepped for `gating (` to find which section's summary
counted one, then `sed`'d that section out.

### Change
- **Summary line names the gate.** `exit 1 while gating findings remain` →
  `exit 1: divergence-handling ×1 (src/cmd/emit.rs:635)`. One line, greppable,
  survives `| tail -2`, which is how 40+ calls consumed it.
- **A `## gating` section first**, before the ranked battery, listing every
  gating row verbatim with its check name as the first column. Empty on a
  clean tree (one line: `(no gating rows)`). The per-check sections keep their
  rows too; duplication of at most a handful of lines buys a digest whose first
  screen is the whole gate.
- **Per-row marker.** A `gate` column (`!` / empty) in TSV and `"gating": true`
  in JSON on every row of a `Tiered` or `Gating` section, so `grep '^!'` or
  `jq 'select(.gating)'` works without knowing thresholds.
- Section headers of `Gating`-class checks (`divergence`, `divergence
  --handling`, `enum-coverage`, `dead-code`, `conversion-pairs`) say
  `gating: every row` the way the tiered ones say `gating at score >= 0.55`.

### Acceptance
`audit` on a tree with exactly one gating row: the row is on the first
screen, the last line names its check and site, `audit --json | jq
'[.sections[].rows[] | select(.gating)] | length'` is 1.

---

## A2. Make the default digest short; put the advisory bulk behind a flag

**Effort:** S · **Risk:** low · **Depends on:** A1 (so the short form is still
complete about the gate)

### Evidence
On svggen the `--changed-since HEAD` digest was ~350 lines: `stringly` 124
lines, `metrics` 45, `panics` 44, `error-swallows` 35, `concepts` 24 — and
0 or 1 gating rows. `--findings-only`, `--top`, `--context`, `--fail-on-new`
were used **zero** times in 181 calls; `--summary` 3 times, `--only` 2.
Agents do not discover flags from a header note; they discover the exit code.
The consumption pattern was `unruster audit >/dev/null 2>&1; echo $?` plus
`| tail -2`, and in one SchemaChef session `> /tmp/audit{1..10}.txt` to grep.

### Change
- Default: `--findings-only` behaviour on, and advisory rows capped at **5 per
  section** with the existing cap note (`showing 5 of 124 — --top 0 for the
  rest`). Gating rows are never capped (already true).
- Keep the full form one flag away: `--full` (= `--top 0`, clean sections shown).
- Drop the "read it whole rather than piping it to `head`" sentence once the
  digest fits one screen; it is advice the data shows nobody takes.

### Acceptance
The svggen 2 Sep digest renders in under 80 lines with the gating row visible
without scrolling; `--full` reproduces today's output.

---

## A3. `--changed-since` gates on pre-existing rows in touched files

**Effort:** M · **Risk:** medium

### Evidence
The svggen gating row above (`partial_cmp` … `unwrap_or` vs `expect`) was in
a *changed file* but an *unchanged hunk*: `git diff HEAD -- src/cmd/emit.rs
src/raster.rs | grep -c partial_cmp` → `0`. A 17-file refactor thus surfaced
older debt as the gate on the change. The agent fixed it anyway (good), but
the flag's contract — "keep only rows in files changed" (`--help`,
`src/context.rs:526`) — makes every large diff a sweep of everything those
files ever accumulated, and the agent cannot tell "I broke this" from "this
was here" without a manual `git diff | grep`.

### Change
- Compute changed *line ranges* from `git diff -U0 <ref>` (untracked files:
  whole file) and tag each kept row as `changed` or `pre-existing` (a column
  in TSV, a field in JSON). Gating stays file-granular by default so nothing
  gets quieter silently, but the row says which it is and the summary splits
  them: `1 gating (0 in changed lines, 1 pre-existing in changed files)`.
- Let `--fail-on-new` work with `--changed-since <ref>` alone (today it needs
  `--since`/`--baseline`): materialise the ref, run the same battery, gate
  only on fingerprints absent there. That is the flag the agent-loop wants and
  it was used 0 times, most likely because it needs a second flag.

### Acceptance
On the svggen change: the `partial_cmp` row is tagged `pre-existing`;
`audit --changed-since HEAD --fail-on-new` exits 0; introducing a new
`.unwrap()` on a fallible call in a changed hunk flips it to 1.

---

## A4. `--only` miscounts waivers as "suppressing nothing"

**Effort:** XS · **Risk:** none

### Evidence
`audit --changed-since HEAD --only divergence-handling --summary` printed:
`6 waiver(s) in the changed files hiding 0 finding(s), 6 of them suppressing
nothing — unruster waivers to review`. Those six waivers name checks that were
not run. Under `--only`/`--skip` the ledger cannot know what they suppress.

### Change
When a selection is active, count only waivers whose check ran; report the
rest as `N waiver(s) for checks not run this pass` or omit them.

---

## A5. `metrics` looks waivable and is not

**Effort:** XS · **Risk:** none

### Evidence
SchemaChef, 30 Aug 02:28: `waivers` reported `1 waiver(s) name a check this
tool does not have (metrics) and so waive nothing`. The agent had written
`// unruster: ok(metrics) …` above a 7-parameter fn because the audit prints
`## [medium] metrics — fns with cyclo >= 15` and `## [low] metrics — fns with
params >= 7`. A section header is the name an agent will use. The agent then
converted the waiver into a doc comment — a fine outcome, but one round trip
plus a lie in the tree for a day.

### Change
Either accept `ok(metrics)` (advisory sections can still be waived to drop the
row) or have the `metrics` headers say `advisory · not waivable` and have the
waiver parser's "check this tool does not have" note add "(`metrics` is an
advisory section; nothing to waive)".

---

# Tier 2 — waivers

## W1. `waivers --orphaned` lists what it says it will not remove

**Effort:** XS · **Risk:** none

### Evidence
SchemaChef ran `waivers --orphaned` 43 times as part of its gate. Every run
printed **52 rows** (of 72–77 waivers) followed by the note that they
"suppress only findings below audit's thresholds … listed because they are
not holding the gating loop open, not because they are wrong". True orphans
in every run: **0**. The agent's summaries had to say "the 52 waivers it lists
as earning nothing are the pre-existing below-threshold ones, not orphans" —
a sentence it should not have had to write, 43 times. The `--json` fallback at
31 Aug 22:03 was the agent trying to get a count of *true* orphans out of the
structure because the text listing did not separate them.

### Change
`--orphaned` lists only waivers suppressing nothing at all. `--include-below-
audit` (already the `--remove` opt-in) adds the below-threshold set to the
listing. The summary keeps both counts. This mirrors `--remove`'s contract so
the listing and the action agree.

### Acceptance
SchemaChef tree: `waivers --orphaned` prints 0 rows and
`(0 orphaned; 52 below audit's thresholds — --include-below-audit lists
them)`.

---

## W2. Put the waiver line on the gating row

**Effort:** S · **Risk:** low · **Depends on:** A1

### Evidence
`--suggest-waivers` was used 3 times; waivers were hand-written via
`python3 - <<'PY' … s.replace(...)` 5 times (SchemaChef: `page::Border`
vocabulary, `pdf.rs` casts, `doc.rs` error-swallows, plus two cleanups).
Hand-written waivers are how `ok(metrics)` happened. `audit` — the command
that shows the agent the row it wants to waive — has no `--suggest-waivers`
of its own; the agent would have to re-run the individual check.

### Change
`audit --suggest-waivers` (and on by default in the `## gating` section from
A1): each gating row carries the exact `// unruster: ok(<check>/<key>) <date> —`
prefix the check would print, so the agent copies rather than composes.

---

# Tier 3 — `show` / `outline` ergonomics (53% of all calls)

## S1. A type and its own `impl` should not be an ambiguity

**Effort:** S · **Risk:** low

### Evidence
12 `show <Name>` calls answered `names 2 items — showing the list` where the
two items were the type and its own inherent impl (`Aabb`, `FootprintPad`,
`Claim`, `Kind`, `SolverOpts` + `impl Default for SolverOpts`) or a fn and an
impl-fn of the same name (`solve`, `diff`, `writable`, `isolate`). Every one
cost a second call — `--kind enum`, the qualified name — or a `sed -n` on the
range the list had just printed (`show Aabb` → `sed -n '70,132p'
src/layout.rs`; `show Claim` → `sed -n '1477,1605p'`). Reproduced here:
`unruster show AliasGraph` lists `struct semantic::AliasGraph` and
`impl AliasGraph` and prints neither. The `--kind` help even names this as
"the usual `Foo`-is-a-struct-and-an-impl case" — the usual case should be the
default, not a flag.

### Change
When a bare name resolves to exactly one *type* (struct/enum/trait/type)
plus impl blocks **of that type**, print the type and follow it with a one-line
pointer per impl (`impl AliasGraph  src/semantic.rs:176-237 (10 fns; show
AliasGraph --kind impl)`). Trait impls (`impl Default for X`) never compete
with `X` for its bare name. Genuine collisions (two fns in two modules) keep
today's list.

### Acceptance
`show AliasGraph` prints the struct; `show AliasGraph --kind impl` prints the
impl; `show diff` on svggen still lists `cmd::diff` and `raster::diff`.

---

## S2. The "bounds itself" note fires when nothing was cut

**Effort:** XS · **Risk:** none

### Evidence
36 results carried `note: N line(s) of source follow. This command bounds
itself; piping it to head cuts mid-item and says nothing.` It is printed for
every item over some length — including under `--max-lines 0`, where nothing
is bounded (`show main --max-lines 0` on this repo: "246 line(s) of source
follow. This command bounds itself"). Agents responded to it in the least
useful way possible: `--max-lines 0` was passed **26 times, always 0, and in
most cases followed by `| head -80`** — they read the note as "the tool is
cutting me", lifted the cap, and then cut it themselves. 42 of 209 `show`
calls piped to `head` regardless. What agents actually wanted from a long fn
was a *shorter* preview, not a longer one.

### Change
- Print the note only when lines were actually dropped, and say what was
  dropped: `(cut at 240 of 512 lines — --max-lines 0 for all, --max-lines 60
  for less)`. Under `--max-lines 0` say nothing.
- When the item is longer than the cap and `--max-lines` was not given, add
  the item's *structure* instead of the tail: top-level comments and control
  flow lines (`// ---- step 2`, `for …`, `if …`, `match …`) with line numbers —
  what the svggen agent hand-built with `show cmd::outline --max-lines 0 >
  /tmp/outline.rs; grep -n "^    // \|^    let \|^    if "` on 2 Sep.

---

## S3. `outline --sig`: the file's items with their signatures

**Effort:** S · **Risk:** none

### Evidence
62 `outline` calls; the typical follow-up was `sed -n 'A,Bp'` over a range or
several ranges of the same file (`outline src/spec.rs` → `sed -n
'860,1000p'`; `outline src/kicad/board/mod.rs | head -80` → `sed -n
'119,368p'`). The question being answered was "what is in this file and what
do these take/return", which is one `--part sig` per item — but `show --part
sig` needs names, and the names are what `outline` was run to learn.

### Change
`outline <file> --sig` prints each item's signature line(s) under its row
(docs elided). Bounded by the same per-item cap as `show --part sig`.

---

## S4. Default `callers` / `field-uses` / `type-refs` / `variants` to `--scope all`

**Effort:** XS · **Risk:** low

### Evidence
34 results carried the scope footer "`--scope all` includes tests, which is
usually what you want before changing a signature or a type's shape"; 26
calls passed `--scope` explicitly, nearly all `all`, and several were re-runs
of the same query one call earlier without it (`callers rules_class` →
`callers rules_class --scope all`). The footer is right about what the reader
wants; make it the default for the *usage* queries and keep production-only
for the *finding* checks and `audit`, whose thresholds were tuned on it.

---

## S5. Trim the repeated trailers

**Effort:** XS · **Risk:** none

### Evidence
Per result, verbatim, every time: `(blind spots: 9 macro body(ies) in the
scanned tree could not be parsed as expressions — code inside them was not
analyzed by any check; unruster blind-spots lists them)` — 137 times, never
acted on; `(scope: 7 test file(s) were not scanned …)` — 34 times; `(1
item(s) from 1 name(s))` after every single-name `show`. About 4% of `show`
output bytes, but the real cost is a footer that the reader has learned to
skip and therefore also skips when it matters.

### Change
One short line each: `(blind spots: 9 macro bodies — unruster blind-spots)`,
`(scope: production; --scope all adds 7 test files)`. Drop `(1 item(s) from 1
name(s))` when both numbers are 1.

---

## S6. `field-uses` strict=0 with candidates: print them, do not hint

**Effort:** XS · **Risk:** none

### Evidence
SchemaChef 1 Sep 20:05: `field-uses NetClassRules track_width --scope all` →
`0 reads … hint: strict matched 0; --candidates would report 6 hit(s)`. The
agent re-ran with `--candidates` and got the 6 rows. The hint works; the
rerun is the waste.

### Change
When strict matches 0 and candidates match > 0, print the candidate rows in
the same pass, marked `?` in the resolution column, under the existing hint
text.

---

# Tier 4 — smaller observations, no change proposed yet

- **Root auto-recovery works.** 4 calls from a subdirectory got `(note: no .rs
  files under .; scanned /Users/alois/projects/svggen instead …)` and correct
  results. 84 calls were prefixed with `cd <root>`; agents have learned to be
  safe about it.
- **Separators.** Agents chain two or three unruster commands per Bash call and
  put `echo ===` between them; in zsh a bare `===` is an equals-expansion and
  fails, which produced 10 of the 12 non-zero exits in the sample. Not
  unruster's defect. Multi-name `show` already removes the need for `show`;
  nothing does for mixed commands.
- **`--json` is barely used** (5 calls, one to `waivers` while hunting for the
  orphan count). Agents parse TSV with `awk`/`grep`. Not a problem in itself;
  it means the TSV columns are the API and A1's marker column has to be in TSV.
- **Audit runtime** was not measurable from the logs (no `time`); one 120 s
  tool timeout on SchemaChef included `cargo test` in the same command. 2.3 s
  on this repo.
- **`variants` and `enum-coverage` are under-used** where they fit: `show
  Command | grep -E "^\s+[A-Z]…"` was run to list an enum's variants. The
  `variants` pointer note only appears on long enums; consider printing it on
  every enum `show`.

---

## Suggested order

1. **A1 + A2** together (one release): the gate becomes readable in one
   screen and the exit code stops being the only thing agents read. This is
   the change with the most calls behind it (181) and the clearest waste
   (8 reruns to find one row).
2. **W1, A4, A5, S2, S5** — five XS fixes, each removing a note or listing
   that was wrong or ignored in every run.
3. **S1** — the ambiguity that cost a second call 12 times in four days.
4. **A3, W2, S3, S4, S6** — round-trip savers, in that order.

---

## Outcome (2026-09-02)

Every Tier 1–3 item landed; 9 new end-to-end tests cover them and the 15
existing tests whose assertions described the old behaviour were updated.
Deviations from the text above:

- **A1.** The `## gating` digest is printed *first* by holding the battery's
  stdout in the emitter until the last section has run (`Out::start_buffering`),
  so no second pass is needed. Digest rows lead with the check name; rows in
  their sections lead with a `!` column (empty for advisory rows); JSON rows
  carry `"gating": true|false`. The digest carries each row's waiver line up to
  25 rows and says so past that. The summary reads
  `exit 1: dead-code ×13 (src/x.rs:19), …`.
- **A2.** Default = clean sections dropped, advisory rows capped at 5 per
  section; `--full` restores the old report; `--findings-only` is accepted and
  hidden. `--top` still overrides.
- **A3.** `--changed-since` now also parses `git diff -U0` hunks. Gating rows
  in the digest end with `changed` / `pre-existing`, JSON rows carry `"change"`,
  and the summary splits the gate count. `audit --fail-on-new` with only
  `--changed-since <ref>` uses that ref as its baseline. The file-granular
  filter itself is unchanged.
- **A4.** Waivers whose check did not run under `--only`/`--skip` are counted
  as `N waiver(s) for checks not run this pass`, not as suppressing nothing.
- **A5.** `metrics` is now a waivable check (`ok(metrics)` /
  `ok(metrics/<fn>)`); the two metrics section headers say so.
- **W1.** `waivers --orphaned` lists only waivers that suppress nothing at all;
  `--include-below-audit` (no longer tied to `--remove`) widens the listing and
  the removal alike. `--orphaned --remove` still names the held-back count.
- **W2.** `audit` generates the waiver lines unconditionally and prints them
  under the gating digest; `--suggest-waivers` additionally prints them beside
  every row as before.
- **S1.** A bare name resolving to one type plus impl blocks of that type
  prints the type and follows it with a note naming each impl block, its
  span and fn count, plus `show <T> --kind impl` / `outline <file>`.
- **S2.** The size note is printed only when the print will actually be cut,
  says how many lines print, and the cut is followed by a numbered sketch of
  the dropped tail's top-level comments and control flow (30 lines max). The
  enum route to `variants` is kept, reworded.
- **S3.** `outline --sig` prints signatures beneath rows (TSV) or as a `sig`
  list on the row (JSON), capped at 12 lines per item.
- **S4.** `callers`, `callees`, `co-call`, `field-uses`, `type-refs`,
  `takes-mut`, `module-uses`, `variants` default to `--scope all` when no
  scope is named, with a one-line note; the production-gap note is kept for
  the other usage queries and for an explicit `--scope production`.
- **S5.** Blind-spot and scope footers are one short line each; the
  `(1 item(s) from 1 name(s))` trailer is dropped for a single resolved name.
- **S6.** `field-uses` prints the candidate rows inline when strict mode
  matches nothing, under a note and with a summary count, instead of a hint.
