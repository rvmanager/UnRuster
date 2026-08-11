# Would unruster have caught uv's real defects?

**4 of 200 genuinely detected (2.0%).** Two more partial. The rest invisible.

## Method

For each of 200 changelog-declared defects: check out **commit A** (parent of the fix — bug still present), run `unruster audit --json --scope production` over the whole workspace, and test whether any finding lands on the lines the fix changed. 200/200 runs completed, zero failures. Ground truth = the fix's pre-image hunks, after removing 136 hunks that fell inside inline `#[cfg(test)]` modules (uv colocates tests, and a fix's *added test* would otherwise count as a hit).

Every hit was then reviewed by hand — proximity is not detection.

## Results

| Verdict | n | |
|:--|--:|:--|
| **Genuine detection** | 4 | check named the actual defect mechanism |
| Partial | 2 | right region or function, wrong reason |
| Coincidental | 60 | fired near the fix, unrelated construct |
| Degenerate | 5 | only a whole-item check (god fn / dead code) |
| File flagged, defect missed | 98 | |
| Nothing in the file | 31 | |

### Why raw hit-rate is misleading

Each run emits **~780 findings** touching **204 of 514** production files (~40%). Landing near a defect is largely chance:

| Level | Observed | Chance | Lift |
|:--|--:|--:|--:|
| File flagged | 84% | 40% | 2.13× |
| Within 15 lines of the fix | 33% | 16% | 2.10× |

A naive file-level score would report **84% 'found'**. After the null model and manual review the real number is **2.0%**.

### Signal is concentrated in the low-volume checks

| Check | Findings/run | Landed near a fix | Genuine |
|:--|--:|--:|--:|
| `error-swallows` | 665 | 58 | 2 |
| `conversion-pairs` | 16 | 6 | 1 |
| `enum-coverage --all` | 29 | 5 | 0 |
| `config-drift` | 10 | 4 | 1 |
| `stringly` | 20 | 2 | 0 |
| `builder-drift` | 3 | 2 | 0 |
| `divergence --handling` | 11 | 2 | 0 |

`error-swallows` is **82% of all output** (665 of ~800) and accounts for 48 of 66 proximity hits — but only 2 genuine ones. The two consistency checks that actually named defects, `config-drift` and `conversion-pairs`, emit 10 and 16 rows respectively: small enough to read end to end.

## The four genuine detections

**#2 · PR [#20930](https://github.com/astral-sh/uv/pull/20930) · `config-drift`** — Avoid including workspace-root default dependency groups when syncing or exporting a selected workspace member unless explicitly requested

> Named the exact defect: `DiscoveryOptions` built 2 ways, `members{Existing|None}` disagreeing. The fix made that very field conditional. Type, field and both values correct, on the right line.

**#19 · PR [#20395](https://github.com/astral-sh/uv/pull/20395) · `error-swallows`** — Preserve direct-archive hashes in `uv pip freeze` output

> Flagged `Hashes::parse_fragment(fragment).ok()` at line 205 — the exact line the fix replaced. The swallowed parse failure *was* the dropped-hashes bug.

**#161 · PR [#18794](https://github.com/astral-sh/uv/pull/18794) · `conversion-pairs`** — Don't drop `blake2b` hashes

> Flagged the `HashDigests ↔ Hashes` From-pair as 'one concept in two shapes'. The bug was that one direction forgot the `blake2b` field — precisely the drift this check exists to catch.

**#190 · PR [#18176](https://github.com/astral-sh/uv/pull/18176) · `error-swallows`** — Preserve absolute/relative paths in lockfiles

> Flagged `.unwrap_or_else(|_| dist.install_path.clone())` — a silent fallback that discarded whether the path was originally absolute. That discard was the defect.

All four are **consistency defects**: something existed in two places and one copy was wrong. That is the only defect shape this tool can see.

## What it cannot see, by construction

Every check is a consistency comparator (`divergence`, `config-drift`, `builder-drift`, `conversion-pairs`, `clones`), a non-exhaustive-dispatch detector (`enum-coverage`), or a pattern-density scanner (`error-swallows`, `casts`, `stringly`). None model semantics. So these dominant uv defect classes are structurally invisible:

| Class | n | Why |
|:--|--:|:--|
| `missing-match-arm` | 54 | 1 genuine — `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against. |
| `error-handling` | 28 | 0 genuine — `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly. |
| `other` | 28 | 0 genuine — Matches no consistency, exhaustiveness, or pattern-density shape in the battery. |
| `predicate-change` | 28 | 1 genuine — No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one. |
| `panic-removal` | 18 | 1 genuine — No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic. |
| `string-parse` | 17 | 0 genuine — `stringly` flags *branching on* string literals, not incorrect string operations. |
| `missing-call` | 9 | 0 genuine — `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable. |
| `serde-schema` | 6 | 0 genuine — No serialization/compatibility check exists; wire-format drift is outside the model. |
| `path-handling` | 4 | 0 genuine — No path-semantics check; join/canonicalize mistakes look like ordinary calls. |
| `plumb-new-param` | 4 | 0 genuine — The fix threads new state through signatures: a behavior addition, not a local smell. |
| `arith-overflow` | 3 | 0 genuine — No check models arithmetic discipline; `casts` covers lossy `as` casts only, so a `+` that should be `saturating_add` is invisible. |
| `async-concurrency` | 1 | 1 genuine — No concurrency or ordering model. |

The single clearest example: PR [#20178](https://github.com/astral-sh/uv/pull/20178) changed `corrected_initial_age + resident_age` to `saturating_add`, where the three adjacent RFC 9111 terms already saturated. A one-token inconsistency between siblings — conceptually exactly `divergence`'s thesis — but `divergence` only compares *enum dispatch sites*, so no check in the battery could see it.

## unruster defects and issues found

1. **`audit --json` emits duplicate `file`/`line` keys in one row object.** A standard JSON parser keeps the last, silently dropping the primary (`lean`) location and substituting the `vs` location — so naive tooling attributes findings to the wrong file and line. Needs distinct names (`lean_file`/`vs_file`) or a nested object.
2. **`error-swallows` drowns the battery.** 82% of output, 0.3% genuine yield here. It should be off by default in `audit`, or gated far above its current 0.55 threshold — note both genuine swallow detections scored **0.35 and 0.40, below the gate**, so `audit`'s own ranking buried its only true positives.
3. **`metrics` produces degenerate findings.** Flagging a 1247-line, cyclo-183 function means any defect inside it 'matches'. Useful as a refactor signal, misleading as a defect signal.
4. **Test-support crates are scanned as production.** `crates/uv-test` is library code, so `--scope production` includes it; several findings pointed at test scaffolding.
5. **62 macro bodies unparseable** on every run, reported as a blind spot — code inside them is analyzed by no check.

## Honest takeaways

- **As a defect finder: ~2%.** Not what it's for, and the null model shows even that is only ~2× chance at the file level.
- **As a consistency finder: genuinely good.** When a defect *was* a sibling inconsistency, the low-volume checks named it precisely — the right type, the right field, both values.
- **Volume is the enemy of the signal.** The checks that found real bugs emit 10–16 rows. The one that emits 665 found almost nothing. Running `unruster audit --top 20` while skipping `error-swallows` would have surfaced 2 of the 4 genuine hits in a reviewable list.
- **Best use is the diff-scoped one:** `unruster audit --changed-since HEAD~1`, where 800 repo-wide findings collapse to the handful touching your change.

---

## All 200 defects

### 1. [#20963](https://github.com/astral-sh/uv/pull/20963) · 0.12.2 · `serde-schema` · **MISS**

> Preserve compatibility with older uv versions when recording artifact sizes in cached wheels and source distributions

*Fix:* `uv-distribution/src/archive.rs`, `uv-distribution/src/source/revision.rs` @ L0 · churn 128 · findings that run: 815

**NOT FOUND.** No finding anywhere in the changed file(s). No serialization/compatibility check exists; wire-format drift is outside the model.

### 2. [#20930](https://github.com/astral-sh/uv/pull/20930) · 0.12.2 · `async-concurrency` · **FOUND**

> Avoid including workspace-root default dependency groups when syncing or exporting a selected workspace member unless explicitly requested

*Fix:* `uv/src/commands/project/export.rs`, `uv/src/commands/project/install_target.rs` @ L99 · churn 35 · findings that run: 812

**FOUND — verified.** `config-drift`: Named the exact defect: `DiscoveryOptions` built 2 ways, `members{Existing|None}` disagreeing. The fix made that very field conditional. Type, field and both values correct, on the right line.

### 3. [#20842](https://github.com/astral-sh/uv/pull/20842) · 0.12.1 · `error-handling` · **MISS**

> Flush shell startup file updates before `uv tool update-shell` and `uv python update-shell` exit

*Fix:* `uv/src/commands/update_shell.rs` @ L101 · churn 12 · findings that run: 803

**NOT FOUND.** No finding anywhere in the changed file(s). `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 4. [#20840](https://github.com/astral-sh/uv/pull/20840) · 0.12.1 · `missing-match-arm` · **COINCIDENT**

> Make workspace-root dependency groups available to commands run from workspace members

*Fix:* `uv-resolver/src/lib.rs`, `uv-resolver/src/lock/export/mod.rs` @ L12 · churn 293 · findings that run: 810

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 810 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 5. [#20832](https://github.com/astral-sh/uv/pull/20832) · 0.12.1 · `path-handling` · **COINCIDENT**

> Resolve `--find-links` paths in requirements files relative to the containing file

*Fix:* `uv-requirements-txt/src/lib.rs` @ L367 · churn 11 · findings that run: 803

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 803 findings that run, near-misses are expected. No path-semantics check; join/canonicalize mistakes look like ordinary calls.

### 6. [#20770](https://github.com/astral-sh/uv/pull/20770) · 0.12.1 · `other` · **COINCIDENT**

> Respect configured indexes in `uv tool list --outdated`

*Fix:* `uv/src/settings.rs` @ L1259 · churn 8 · findings that run: 806

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 806 findings that run, near-misses are expected. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 7. [#20237](https://github.com/astral-sh/uv/pull/20237) · 0.12.0 · `predicate-change` · **FILE**

> Include extras activated by dependency groups when evaluating conflicts

*Fix:* `uv-resolver/src/lock/installable.rs` @ L235 · churn 44 · findings that run: 804

**NOT FOUND.** File flagged (error-swallows×3, metrics×1) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 8. [#20671](https://github.com/astral-sh/uv/pull/20671) · 0.11.33 · `missing-match-arm` · **FILE**

> Correctly split dependencies into production and optional markers

*Fix:* `uv-resolver/src/resolver/mod.rs` @ L2340 · churn 38 · findings that run: 803

**NOT FOUND.** File flagged (error-swallows×12, enum-coverage --all×2) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 9. [#20679](https://github.com/astral-sh/uv/pull/20679) · 0.11.33 · `serde-schema` · **COINCIDENT**

> Fix discrepancies in argument parsing of exclude-newer

*Fix:* `uv-cli/src/lib.rs`, `uv-cli/src/options.rs` @ L3150 · churn 359 · findings that run: 802

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 802 findings that run, near-misses are expected. No serialization/compatibility check exists; wire-format drift is outside the model.

### 10. [#20752](https://github.com/astral-sh/uv/pull/20752) · 0.11.33 · `missing-match-arm` · **FILE**

> Cleanup managed Python temporary directory on error

*Fix:* `uv-python/src/downloads.rs` @ L1354 · churn 2 · findings that run: 803

**NOT FOUND.** File flagged (error-swallows×4) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 11. [#20586](https://github.com/astral-sh/uv/pull/20586) · 0.11.32 · `missing-match-arm` · **COINCIDENT**

> Fork universal resolutions when `Requires-Python` is discovered only from distribution metadata

*Fix:* `uv-resolver/src/resolver/mod.rs` @ L626 · churn 84 · findings that run: 801

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 801 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 12. [#20582](https://github.com/astral-sh/uv/pull/20582) · 0.11.31 · `other` · **MISS**

> Suggest `--emit-build-options` for unsupported `uv pip compile --emit-options`

*Fix:* `uv-cli/src/compat.rs` @ L139 · churn 2 · findings that run: 800

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 13. [#20432](https://github.com/astral-sh/uv/pull/20432) · 0.11.31 · `error-handling` · **FILE**

> Reject source distributions and wheels with mismatched package names

*Fix:* `uv/src/commands/build_frontend.rs` @ L98 · churn 35 · findings that run: 792

**NOT FOUND.** File flagged (enum-coverage --all×2, error-swallows×1) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 14. [#16245](https://github.com/astral-sh/uv/pull/16245) · 0.11.31 · `missing-match-arm` · **FILE**

> Avoid retrying TLS certificate verification failures

*Fix:* `uv-client/src/retry.rs` @ L11 · churn 51 · findings that run: 792

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 15. [#20153](https://github.com/astral-sh/uv/pull/20153) · 0.11.31 · `predicate-change` · **FILE**

> Avoid warnings about `uv_build` settings for in-tree build backends

*Fix:* `uv-build-frontend/src/lib.rs` @ L614 · churn 11 · findings that run: 792

**NOT FOUND.** File flagged (stringly×4, error-swallows×2) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 16. [#20429](https://github.com/astral-sh/uv/pull/20429) · 0.11.30 · `error-handling` · **FILE**

> Prevent skipped tar-wheel entries from causing unrelated files to be removed during uninstall

*Fix:* `uv-extract/src/stream.rs` @ L622 · churn 13 · findings that run: 786

**NOT FOUND.** File flagged (metrics×1) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 17. [#20466](https://github.com/astral-sh/uv/pull/20466) · 0.11.30 · `error-handling` · **COINCIDENT**

> Preserve literal `extends-environment` paths in `pyvenv.cfg` on Unix

*Fix:* `uv-test/src/lib.rs`, `uv/src/commands/project/environment.rs` @ L414 · churn 18 · findings that run: 787

**NOT FOUND (coincidental).** `conversion-pairs` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 787 findings that run, near-misses are expected. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 18. [#20391](https://github.com/astral-sh/uv/pull/20391) · 0.11.29 · `error-handling` · **FILE**

> Reject duplicate active package entries in `pylock.toml`

*Fix:* `uv-resolver/src/lock/export/pylock_toml.rs` @ L1 · churn 9 · findings that run: 784

**NOT FOUND.** File flagged (error-swallows×18) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 19. [#20395](https://github.com/astral-sh/uv/pull/20395) · 0.11.29 · `missing-match-arm` · **FOUND**

> Preserve direct-archive hashes in `uv pip freeze` output

*Fix:* `uv-distribution-types/src/requirement.rs`, `uv-distribution-types/src/specified_requirement.rs` @ L95 · churn 72 · findings that run: 786

**FOUND — verified.** `error-swallows`: Flagged `Hashes::parse_fragment(fragment).ok()` at line 205 — the exact line the fix replaced. The swallowed parse failure *was* the dropped-hashes bug.

### 20. [#20228](https://github.com/astral-sh/uv/pull/20228) · 0.11.29 · `missing-match-arm` · **FILE**

> Explain conflicting root requirements instead of displaying an empty version range

*Fix:* `uv-resolver/src/pubgrub/dependencies.rs`, `uv-resolver/src/pubgrub/report.rs` @ L16 · churn 166 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×12, enum-coverage --all×4) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 21. [#20397](https://github.com/astral-sh/uv/pull/20397) · 0.11.29 · `string-parse` · **COINCIDENT**

> Prevent build-backend data paths from escaping the project or bypassing wheel exclusions

*Fix:* `uv-build-backend/src/wheel.rs` @ L20 · churn 78 · findings that run: 786

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 786 findings that run, near-misses are expected. `stringly` flags *branching on* string literals, not incorrect string operations.

### 22. [#20387](https://github.com/astral-sh/uv/pull/20387) · 0.11.29 · `string-parse` · **FILE**

> Reject PEP 517 backend paths outside the source tree, including paths that escape through symlinks

*Fix:* `uv-build-frontend/src/error.rs`, `uv-build-frontend/src/lib.rs` @ L73 · churn 20 · findings that run: 786

**NOT FOUND.** File flagged (stringly×4, error-swallows×2) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 23. [#20401](https://github.com/astral-sh/uv/pull/20401) · 0.11.29 · `error-handling` · **FILE**

> Redact credentials from failed Git fetch commands

*Fix:* `uv-git/src/git.rs` @ L10 · churn 84 · findings that run: 785

**NOT FOUND.** File flagged (error-swallows×4) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 24. [#20268](https://github.com/astral-sh/uv/pull/20268) · 0.11.29 · `panic-removal` · **FILE**

> Fix exclusive post-release range ordering to match PEP 440

*Fix:* `uv-pep440/src/version_ranges.rs`, `uv-pep440/src/version_specifier.rs` @ L234 · churn 278 · findings that run: 785

**NOT FOUND.** File flagged (error-swallows×1, clones×1) but never near the defect. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 25. [#20182](https://github.com/astral-sh/uv/pull/20182) · 0.11.29 · `arith-overflow` · **COINCIDENT**

> Canonicalize equivalent PEP 440 ranges during dependency resolution

*Fix:* `uv-pep440/src/lib.rs`, `uv-pep440/src/version.rs` @ L28 · churn 925 · findings that run: 788

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 788 findings that run, near-misses are expected. No check models arithmetic discipline; `casts` covers lossy `as` casts only, so a `+` that should be `saturating_add` is invisible.

### 26. [#20404](https://github.com/astral-sh/uv/pull/20404) · 0.11.29 · `other` · **FILE**

> Honor Python version pins when initializing scripts

*Fix:* `uv/src/commands/project/init.rs` @ L261 · churn 4 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×4, enum-coverage --all×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 27. [#20389](https://github.com/astral-sh/uv/pull/20389) · 0.11.29 · `missing-match-arm` · **FILE**

> Respect package-scoped source filtering for scripts

*Fix:* `uv-scripts/src/lib.rs`, `uv/src/commands/project/mod.rs` @ L0 · churn 42 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×6, clones×4) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 28. [#20388](https://github.com/astral-sh/uv/pull/20388) · 0.11.29 · `error-handling` · **FILE**

> Report existing environment incompatibilities when `uv pip install --strict` has nothing to install

*Fix:* `uv/src/commands/pip/install.rs`, `uv/src/commands/pip/operations.rs` @ L364 · churn 30 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×6) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 29. [#20405](https://github.com/astral-sh/uv/pull/20405) · 0.11.29 · `error-handling` · **FILE**

> Continue scanning `platlib` when `purelib` is missing

*Fix:* `uv-installer/src/site_packages.rs` @ L74 · churn 104 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×3) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 30. [#20403](https://github.com/astral-sh/uv/pull/20403) · 0.11.29 · `other` · **FILE**

> Handle versionless `.egg-info` files as legacy package metadata

*Fix:* `uv-distribution-types/src/installed.rs` @ L284 · churn 4 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 31. [#20369](https://github.com/astral-sh/uv/pull/20369) · 0.11.29 · `predicate-change` · **MISS**

> Make repeated locking idempotent for impossible cross-variable platform markers

*Fix:* `uv-pep508/src/marker/algebra.rs` @ L928 · churn 37 · findings that run: 785

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 32. [#20372](https://github.com/astral-sh/uv/pull/20372) · 0.11.29 · `panic-removal` · **PARTIAL**

> Report invalid cloud credential endpoint URLs instead of panicking

*Fix:* `uv-auth/src/middleware.rs`, `uv-auth/src/providers.rs` @ L527 · churn 98 · findings that run: 784

**PARTIAL.** `error-swallows`: Right concern, wrong construct: it flagged a `let _ =` in the function whose error handling was rewritten, not the `.ok()?` chain that actually caused the panic.

### 33. [#20373](https://github.com/astral-sh/uv/pull/20373) · 0.11.29 · `panic-removal` · **FILE**

> Report invalid `pylock.toml` artifact URLs instead of panicking

*Fix:* `uv-resolver/src/lock/export/pylock_toml.rs` @ L102 · churn 12 · findings that run: 784

**NOT FOUND.** File flagged (error-swallows×18) but never near the defect. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 34. [#20375](https://github.com/astral-sh/uv/pull/20375) · 0.11.29 · `panic-removal` · **COINCIDENT**

> Report non-UTF-8 virtual environment paths instead of panicking while generating activation scripts

*Fix:* `uv-virtualenv/src/lib.rs`, `uv-virtualenv/src/virtualenv.rs` @ L33 · churn 15 · findings that run: 785

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 785 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 35. [#20376](https://github.com/astral-sh/uv/pull/20376) · 0.11.29 · `other` · **MISS**

> Return an unsupported-operation error from unimplemented build-backend requirement hooks

*Fix:* `uv/src/commands/build_backend.rs` @ L2 · churn 8 · findings that run: 784

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 36. [#20178](https://github.com/astral-sh/uv/pull/20178) · 0.11.28 · `arith-overflow` · **COINCIDENT**

> Avoid overflow when computing HTTP cache age

*Fix:* `uv-client/src/httpcache/mod.rs` @ L911 · churn 36 · findings that run: 792

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 792 findings that run, near-misses are expected. No check models arithmetic discipline; `casts` covers lossy `as` casts only, so a `+` that should be `saturating_add` is invisible.

### 37. [#19955](https://github.com/astral-sh/uv/pull/19955) · 0.11.28 · `other` · **MISS**

> Respect `--upgrade` when `upgrade-package` is configured

*Fix:* `uv-configuration/src/package_options.rs` @ L258 · churn 11 · findings that run: 791

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 38. [#20167](https://github.com/astral-sh/uv/pull/20167) · 0.11.28 · `other` · **MISS**

> Support `uv tree` in dependency-group-only projects

*Fix:* `uv/src/commands/project/tree.rs` @ L18 · churn 8 · findings that run: 788

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 39. [#20183](https://github.com/astral-sh/uv/pull/20183) · 0.11.28 · `predicate-change` · **FILE**

> Treat cache entries as stale at exact expiration

*Fix:* `uv-client/src/httpcache/mod.rs` @ L817 · churn 18 · findings that run: 791

**NOT FOUND.** File flagged (error-swallows×4) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 40. [#20145](https://github.com/astral-sh/uv/pull/20145) · 0.11.27 · `predicate-change` · **FILE**

> Always emit `packages` table for pylock.toml

*Fix:* `uv-resolver/src/lock/export/pylock_toml.rs` @ L1024 · churn 5 · findings that run: 792

**NOT FOUND.** File flagged (error-swallows×18) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 41. [#20062](https://github.com/astral-sh/uv/pull/20062) · 0.11.27 · `error-handling` · **FILE**

> Avoid blank line for empty `uv pip tree`

*Fix:* `uv/src/commands/pip/tree.rs` @ L167 · churn 4 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 42. [#19807](https://github.com/astral-sh/uv/pull/19807) · 0.11.27 · `string-parse` · **FILE**

> Encode hashes in file paths

*Fix:* `uv-pep508/src/unnamed.rs`, `uv-pep508/src/verbatim_url.rs` @ L44 · churn 127 · findings that run: 787

**NOT FOUND.** File flagged (conversion-pairs×3, error-swallows×2) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 43. [#19855](https://github.com/astral-sh/uv/pull/19855) · 0.11.27 · `error-handling` · **FILE**

> Error on a registry uv.lock package without a version instead of panicking

*Fix:* `uv-resolver/src/lock/mod.rs` @ L4058 · churn 31 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×9, conversion-pairs×6) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 44. [#20148](https://github.com/astral-sh/uv/pull/20148) · 0.11.27 · `predicate-change` · **FILE**

> Preserve conditional extra markers in exports

*Fix:* `uv-resolver/src/lock/export/mod.rs` @ L514 · churn 16 · findings that run: 792

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 45. [#20086](https://github.com/astral-sh/uv/pull/20086) · 0.11.27 · `string-parse` · **FILE**

> Skip the ambiguous authority check for file transport VCS URLs

*Fix:* `uv-redacted/src/lib.rs` @ L115 · churn 13 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×4, conversion-pairs×3) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 46. [#19818](https://github.com/astral-sh/uv/pull/19818) · 0.11.27 · `missing-match-arm` · **FILE**

> Sync index format when `uv add --index` updates an existing index URL

*Fix:* `uv-workspace/src/pyproject_mut.rs`, `uv/src/commands/project/add.rs` @ L14 · churn 148 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×7, casts×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 47. [#20056](https://github.com/astral-sh/uv/pull/20056) · 0.11.26 · `predicate-change` · **FILE**

> Warn when the build cache is inside the source directory

*Fix:* `uv/src/commands/build_frontend.rs` @ L41 · churn 10 · findings that run: 787

**NOT FOUND.** File flagged (enum-coverage --all×2, error-swallows×1) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 48. [#20011](https://github.com/astral-sh/uv/pull/20011) · 0.11.25 · `missing-match-arm` · **FILE**

> Preserve standalone markers in workspace metadata

*Fix:* `uv-resolver/src/lock/export/metadata.rs` @ L1 · churn 217 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×3) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 49. [#19991](https://github.com/astral-sh/uv/pull/19991) · 0.11.25 · `string-parse` · **FILE**

> Reject `uv build` if the cache dir is enclosed

*Fix:* `uv/src/commands/build_frontend.rs` @ L373 · churn 29 · findings that run: 787

**NOT FOUND.** File flagged (enum-coverage --all×2, error-swallows×1) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 50. [#19934](https://github.com/astral-sh/uv/pull/19934) · 0.11.24 · `missing-match-arm` · **COINCIDENT**

> Allow disabling `exclude-newer`

*Fix:* `uv-cli/src/lib.rs`, `uv-cli/src/options.rs` @ L32 · churn 194 · findings that run: 791

**NOT FOUND (coincidental).** `error-swallows`, `conversion-pairs` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 791 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 51. [#19949](https://github.com/astral-sh/uv/pull/19949) · 0.11.24 · `other` · **MISS**

> Avoid archive id collisions

*Fix:* `uv-cache/src/archive.rs`, `uv-distribution/src/source/revision.rs` @ L22 · churn 4 · findings that run: 789

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 52. [#19928](https://github.com/astral-sh/uv/pull/19928) · 0.11.24 · `predicate-change` · **FILE**

> Reapply "Fix transparent Python upgrades in project environments"

*Fix:* `uv-virtualenv/src/virtualenv.rs` @ L531 · churn 6 · findings that run: 789

**NOT FOUND.** File flagged (error-swallows×3, metrics×1) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 53. [#19966](https://github.com/astral-sh/uv/pull/19966) · 0.11.24 · `error-handling` · **FILE**

> Clean up partial tool entrypoint installs

*Fix:* `uv/src/commands/tool/common.rs` @ L126 · churn 53 · findings that run: 789

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 54. [#19856](https://github.com/astral-sh/uv/pull/19856) · 0.11.24 · `other` · **FILE**

> Fix relocatable `activate.fish` and broaden Fish version support

*Fix:* `uv-virtualenv/src/virtualenv.rs` @ L491 · churn 2 · findings that run: 789

**NOT FOUND.** File flagged (error-swallows×3, metrics×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 55. [#19925](https://github.com/astral-sh/uv/pull/19925) · 0.11.23 · `predicate-change` · **FILE**

> Revert "Fix transparent Python upgrades in project environments" to mitigate unintended breakage in `pre-commit-uv`

*Fix:* `uv-virtualenv/src/virtualenv.rs` @ L531 · churn 6 · findings that run: 791

**NOT FOUND.** File flagged (error-swallows×3, metrics×1) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 56. [#19926](https://github.com/astral-sh/uv/pull/19926) · 0.11.23 · `string-parse` · **FILE**

> Restore old behavior where workspace members "hidden" by an intermediate `pyproject.toml` would be treated as standalone projects

*Fix:* `uv-workspace/src/workspace.rs` @ L62 · churn 25 · findings that run: 791

**NOT FOUND.** File flagged (error-swallows×6) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 57. [#19808](https://github.com/astral-sh/uv/pull/19808) · 0.11.22 · `panic-removal` · **FILE**

> Update string marker ordering semantics to match upstream clarified rules

*Fix:* `uv-pep508/src/marker/algebra.rs`, `uv-pep508/src/marker/tree.rs` @ L221 · churn 145 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×1, clones×1) but never near the defect. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 58. [#19871](https://github.com/astral-sh/uv/pull/19871) · 0.11.22 · `missing-match-arm` · **FILE**

> Reject extras that have the same normalized name

*Fix:* `uv-build-backend/src/metadata.rs`, `uv-toml/src/lib.rs` @ L25 · churn 161 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×11, stringly×6) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 59. [#19866](https://github.com/astral-sh/uv/pull/19866) · 0.11.22 · `error-handling` · **FILE**

> Reject dependency group `include-group` entries that have additional fields

*Fix:* `uv-pypi-types/src/dependency_groups.rs`, `uv-workspace/src/workspace.rs` @ L132 · churn 26 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×6) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 60. [#19814](https://github.com/astral-sh/uv/pull/19814) · 0.11.22 · `panic-removal` · **COINCIDENT**

> Reject invalid UTF-8 URL credentials

*Fix:* `uv-auth/src/cache.rs`, `uv-auth/src/credentials.rs` @ L14 · churn 289 · findings that run: 788

**NOT FOUND (coincidental).** `error-swallows`, `enum-coverage --all` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 788 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 61. [#19834](https://github.com/astral-sh/uv/pull/19834) · 0.11.22 · `error-handling` · **FILE**

> Validate that PEP 517 `backend-path`s exist when building sdists

*Fix:* `uv-build-frontend/src/error.rs`, `uv-build-frontend/src/lib.rs` @ L71 · churn 16 · findings that run: 788

**NOT FOUND.** File flagged (stringly×4, error-swallows×2) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 62. [#19869](https://github.com/astral-sh/uv/pull/19869) · 0.11.22 · `serde-schema` · **FILE**

> Validate that `pylock.toml` files do not have an unsupported a `lock-version`

*Fix:* `uv-resolver/src/lock/export/pylock_toml.rs` @ L196 · churn 15 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×18) but never near the defect. No serialization/compatibility check exists; wire-format drift is outside the model.

### 63. [#19868](https://github.com/astral-sh/uv/pull/19868) · 0.11.22 · `error-handling` · **FILE**

> Validate that the environment satisfies the `packages.requires-python` of a `pylock.toml`

*Fix:* `uv-resolver/src/lock/export/pylock_toml.rs` @ L47 · churn 13 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×18) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 64. [#19879](https://github.com/astral-sh/uv/pull/19879) · 0.11.22 · `string-parse` · **FILE**

> Allow `uv` to be recursively invoked by PEP 517 build hooks

*Fix:* `uv-build-frontend/src/lib.rs`, `uv-static/src/env_vars.rs` @ L1231 · churn 51 · findings that run: 787

**NOT FOUND.** File flagged (error-swallows×5, stringly×4) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 65. [#19815](https://github.com/astral-sh/uv/pull/19815) · 0.11.22 · `serde-schema` · **FILE**

> Allow empty `credentials.toml` files

*Fix:* `uv-auth/src/store.rs` @ L234 · churn 2 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. No serialization/compatibility check exists; wire-format drift is outside the model.

### 66. [#19890](https://github.com/astral-sh/uv/pull/19890) · 0.11.22 · `predicate-change` · **FILE**

> Fix transparent Python upgrades in project environments

*Fix:* `uv-virtualenv/src/virtualenv.rs` @ L531 · churn 6 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×3, metrics×1) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 67. [#19867](https://github.com/astral-sh/uv/pull/19867) · 0.11.22 · `panic-removal` · **COINCIDENT**

> Handle non-file editable URLs in `uv pip list`

*Fix:* `uv/src/commands/pip/list.rs` @ L20 · churn 20 · findings that run: 787

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 787 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 68. [#19910](https://github.com/astral-sh/uv/pull/19910) · 0.11.22 · `missing-match-arm` · **FILE**

> Fix incorrect output from `uv tree --invert`

*Fix:* `uv-resolver/src/lock/tree.rs` @ L21 · churn 187 · findings that run: 789

**NOT FOUND.** File flagged (enum-coverage --all×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 69. [#19837](https://github.com/astral-sh/uv/pull/19837) · 0.11.22 · `missing-call` · **COINCIDENT**

> Fix environment locking of `uv venv` in a project

*Fix:* `uv/src/commands/project/mod.rs`, `uv/src/commands/venv.rs` @ L1169 · churn 73 · findings that run: 787

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 787 findings that run, near-misses are expected. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 70. [#19905](https://github.com/astral-sh/uv/pull/19905) · 0.11.22 · `missing-match-arm` · **FILE**

> Fix handling of workspace-exclusive dependency groups in `uv tree`

*Fix:* `uv-resolver/src/lock/tree.rs` @ L0 · churn 95 · findings that run: 789

**NOT FOUND.** File flagged (enum-coverage --all×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 71. [#19659](https://github.com/astral-sh/uv/pull/19659) · 0.11.21 · `string-parse` · **FILE**

> Improve cache robustness and pruning behavior

*Fix:* `uv/src/lib.rs` @ L540 · churn 21 · findings that run: 789

**NOT FOUND.** File flagged (error-swallows×3, metrics×2) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 72. [#19769](https://github.com/astral-sh/uv/pull/19769) · 0.11.21 · `path-handling` · **COINCIDENT**

> Fix Python discovery and version request edge cases

*Fix:* `uv-build-frontend/src/lib.rs`, `uv-dispatch/src/lib.rs` @ L291 · churn 66 · findings that run: 789

**NOT FOUND (coincidental).** `error-swallows`, `config-drift` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 789 findings that run, near-misses are expected. No path-semantics check; join/canonicalize mistakes look like ordinary calls.

### 73. [#19805](https://github.com/astral-sh/uv/pull/19805) · 0.11.21 · `error-handling` · **FILE**

> Harden parsing and validation for package metadata, requirements, markers, URLs, and conflict sets

*Fix:* `uv-platform-tags/src/abi_tag.rs` @ L369 · churn 22 · findings that run: 788

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 74. [#19798](https://github.com/astral-sh/uv/pull/19798) · 0.11.21 · `predicate-change` · **MISS**

> Improve wheel entry-point error handling and virtual environment activation quoting - Propagate errors when reading wheel entry points - Quote virtual environment activation paths with shell metacharacters

*Fix:* `uv-shell/src/shlex.rs` @ L9 · churn 37 · findings that run: 788

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 75. [#19669](https://github.com/astral-sh/uv/pull/19669) · 0.11.20 · `missing-match-arm` · **COINCIDENT**

> Allow unknown preview flags with a warning again

*Fix:* `uv-cli/src/lib.rs`, `uv-preview/src/lib.rs` @ L27 · churn 99 · findings that run: 786

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 786 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 76. [#19699](https://github.com/astral-sh/uv/pull/19699) · 0.11.20 · `missing-call` · **FILE**

> Apply dependency exclusions to direct requirements

*Fix:* `uv-resolver/src/resolver/mod.rs` @ L1793 · churn 1 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×12, enum-coverage --all×2) but never near the defect. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 77. [#19682](https://github.com/astral-sh/uv/pull/19682) · 0.11.20 · `string-parse` · **FILE**

> Avoid following external symlinks during cache clean

*Fix:* `uv-cache/src/lib.rs` @ L564 · churn 10 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 78. [#19543](https://github.com/astral-sh/uv/pull/19543) · 0.11.20 · `error-handling` · **FILE**

> Avoid following symlinks during cache prune

*Fix:* `uv-cache/src/lib.rs` @ L615 · churn 87 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 79. [#19706](https://github.com/astral-sh/uv/pull/19706) · 0.11.20 · `missing-match-arm` · **PARTIAL**

> Fix Git cache keys for worktrees and packed refs

*Fix:* `uv-cache-info/src/git_info.rs` @ L1 · churn 321 · findings that run: 790

**PARTIAL.** `stringly`: Right function, wrong reason: flagged `== "gitdir"` in `git_head`/`git_refs` — the buggy functions — but as a newtype style smell, not as the worktree/packed-ref defect.

### 80. [#19695](https://github.com/astral-sh/uv/pull/19695) · 0.11.20 · `panic-removal` · **COINCIDENT**

> Make resolver error handling iterative to avoid stack overflows

*Fix:* `uv-resolver/src/error.rs`, `uv-resolver/src/pubgrub/mod.rs` @ L2 · churn 2173 · findings that run: 786

**NOT FOUND (coincidental).** `enum-coverage --all`, `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 786 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 81. [#19703](https://github.com/astral-sh/uv/pull/19703) · 0.11.20 · `predicate-change` · **MISS**

> Pass `VIRTUAL_ENV` through `cygpath` inside `fish` on Windows

*Fix:* — @ L82 · churn 0 · findings that run: 790

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 82. [#19591](https://github.com/astral-sh/uv/pull/19591) · 0.11.20 · `missing-call` · **FILE**

> Rebuild explicit local directory tool installs

*Fix:* `uv/src/commands/tool/install.rs`, `uv/src/lib.rs` @ L77 · churn 49 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×6, metrics×3) but never near the defect. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 83. [#19679](https://github.com/astral-sh/uv/pull/19679) · 0.11.20 · `predicate-change` · **FILE**

> Validate egg top-level entries as identifiers

*Fix:* `uv-install-wheel/src/uninstall.rs` @ L8 · churn 34 · findings that run: 786

**NOT FOUND.** File flagged (error-swallows×3) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 84. [#19623](https://github.com/astral-sh/uv/pull/19623) · 0.11.19 · `other` · **FILE**

> Continue tool uninstall after dangling receipts

*Fix:* `uv/src/commands/tool/uninstall.rs` @ L139 · churn 3 · findings that run: 785

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 85. [#19424](https://github.com/astral-sh/uv/pull/19424) · 0.11.19 · `missing-match-arm` · **FILE**

> Skip Unix-specific installation steps when cross-installing Windows Python distributions

*Fix:* `uv-python/src/downloads.rs`, `uv-python/src/managed.rs` @ L1350 · churn 20 · findings that run: 785

**NOT FOUND.** File flagged (error-swallows×10) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 86. [#19628](https://github.com/astral-sh/uv/pull/19628) · 0.11.18 · `path-handling` · **MISS**

> Update activation scripts with upstream fixes

*Fix:* — @ L2 · churn 0 · findings that run: 783

**NOT FOUND.** No finding anywhere in the changed file(s). No path-semantics check; join/canonicalize mistakes look like ordinary calls.

### 87. [#19538](https://github.com/astral-sh/uv/pull/19538) · 0.11.17 · `predicate-change` · **MISS**

> Improve the performance of large entries in `tool.uv.conflicts`

*Fix:* `uv-pep508/src/marker/algebra.rs` @ L561 · churn 31 · findings that run: 775

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 88. [#19567](https://github.com/astral-sh/uv/pull/19567) · 0.11.17 · `missing-match-arm` · **COINCIDENT**

> Avoid modifying the parent process' env with `--env-file` in `uv run`

*Fix:* `uv/src/commands/mod.rs`, `uv/src/commands/project/run.rs` @ L8 · churn 233 · findings that run: 774

**NOT FOUND (coincidental).** `enum-coverage --all` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 774 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 89. [#19539](https://github.com/astral-sh/uv/pull/19539) · 0.11.17 · `other` · **FILE**

> Fix script environment creation for scripts with long filenames

*Fix:* `uv/src/commands/project/mod.rs` @ L720 · churn 2 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×5, clones×2) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 90. [#19589](https://github.com/astral-sh/uv/pull/19589) · 0.11.17 · `missing-match-arm` · **FILE**

> Fix transitive Git archive dependencies in lockfiles

*Fix:* `uv-distribution/src/metadata/lowering.rs` @ L536 · churn 22 · findings that run: 776

**NOT FOUND.** File flagged (error-swallows×3) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 91. [#19590](https://github.com/astral-sh/uv/pull/19590) · 0.11.17 · `error-handling` · **MISS**

> Preserve Git repository URLs in direct URL metadata

*Fix:* `uv-pypi-types/src/parsed_url.rs` @ L557 · churn 21 · findings that run: 776

**NOT FOUND.** No finding anywhere in the changed file(s). `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 92. [#19594](https://github.com/astral-sh/uv/pull/19594) · 0.11.17 · `other` · **DEGEN**

> Support redirects in `--check-url`

*Fix:* `uv-client/src/registry_client.rs` @ L68 · churn 5 · findings that run: 776

**NOT FOUND (degenerate).** Only `dead-code` covers the site; it flags a whole item (god function / dead code), so any fix inside matches by construction.

### 93. [#19537](https://github.com/astral-sh/uv/pull/19537) · 0.11.17 · `missing-match-arm` · **COINCIDENT**

> Accept case-insensitive HTML tags in `--find-links` parsing

*Fix:* `uv-client/src/html.rs` @ L1 · churn 324 · findings that run: 775

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 775 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 94. [#19544](https://github.com/astral-sh/uv/pull/19544) · 0.11.17 · `string-parse` · **FILE**

> Reject duplicate script metadata blocks

*Fix:* `uv-scripts/src/lib.rs` @ L437 · churn 97 · findings that run: 774

**NOT FOUND.** File flagged (clones×2) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 95. [#19536](https://github.com/astral-sh/uv/pull/19536) · 0.11.17 · `other` · **MISS**

> Ban names like "python3" as script entry points (#19535

*Fix:* `uv-install-wheel/src/wheel.rs` @ L163 · churn 6 · findings that run: 774

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 96. [#19592](https://github.com/astral-sh/uv/pull/19592) · 0.11.17 · `error-handling` · **FILE**

> Validate Git LFS artifacts for Git archives

*Fix:* `uv-client/src/error.rs`, `uv-client/src/registry_client.rs` @ L16 · churn 98 · findings that run: 776

**NOT FOUND.** File flagged (error-swallows×12, clones×2) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 97. [#19033](https://github.com/astral-sh/uv/pull/19033) · 0.11.17 · `panic-removal` · **FILE**

> Use a relative path when creating symlinks in cache to improve relocatability

*Fix:* `uv-cache/src/lib.rs` @ L829 · churn 7 · findings that run: 773

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 98. [#19503](https://github.com/astral-sh/uv/pull/19503) · 0.11.16 · `string-parse` · **COINCIDENT**

> Allow environment variables that take a list to be empty

*Fix:* `uv-cli/src/lib.rs`, `uv-cli/src/options.rs` @ L3568 · churn 281 · findings that run: 767

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 767 findings that run, near-misses are expected. `stringly` flags *branching on* string literals, not incorrect string operations.

### 99. [#19504](https://github.com/astral-sh/uv/pull/19504) · 0.11.16 · `other` · **FILE**

> Ensure that incompatible wheel hints do not leak secrets

*Fix:* `uv-installer/src/plan.rs` @ L9 · churn 6 · findings that run: 760

**NOT FOUND.** File flagged (metrics×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 100. [#19495](https://github.com/astral-sh/uv/pull/19495) · 0.11.16 · `error-handling` · **FILE**

> Reject unsafe entry points in `uv-build`

*Fix:* `uv-build-backend/src/metadata.rs` @ L61 · churn 18 · findings that run: 760

**NOT FOUND.** File flagged (stringly×6, error-swallows×3) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 101. [#19471](https://github.com/astral-sh/uv/pull/19471) · 0.11.16 · `plumb-new-param` · **MISS**

> Restrict delimiters in entry point parsing

*Fix:* `uv-install-wheel/src/script.rs` @ L1 · churn 23 · findings that run: 794

**NOT FOUND.** No finding anywhere in the changed file(s). The fix threads new state through signatures: a behavior addition, not a local smell.

### 102. [#19494](https://github.com/astral-sh/uv/pull/19494) · 0.11.16 · `predicate-change` · **MISS**

> uv-netrc: fix multi-word no-space comment lines causing parse errors

*Fix:* `uv-netrc/src/netrc.rs` @ L86 · churn 28 · findings that run: 760

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 103. [#19463](https://github.com/astral-sh/uv/pull/19463) · 0.11.15 · `other` · **MISS**

> Fix a TAR parser differential, see GHSA-3cv2-h65g-fgmm

*Fix:* — @ L280 · churn 0 · findings that run: 794

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 104. [#19464](https://github.com/astral-sh/uv/pull/19464) · 0.11.15 · `string-parse` · **FILE**

> Enforce that entry points cannot escape in the scripts directory, see GHSA-4gg8-gxpx-9rph

*Fix:* `uv-fs/src/path.rs`, `uv-install-wheel/src/lib.rs` @ L265 · churn 180 · findings that run: 794

**NOT FOUND.** File flagged (conversion-pairs×3, divergence --handling×1) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 105. [#19423](https://github.com/astral-sh/uv/pull/19423) · 0.11.15 · `missing-match-arm` · **FILE**

> Apply workspace-member `[tool.uv.sources]` credentials under `uv sync --frozen`

*Fix:* `uv-workspace/src/workspace.rs`, `uv/src/commands/project/sync.rs` @ L94 · churn 30 · findings that run: 793

**NOT FOUND.** File flagged (error-swallows×11) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 106. [#19437](https://github.com/astral-sh/uv/pull/19437) · 0.11.15 · `error-handling` · **FILE**

> Skip empty directories in uv build outputs

*Fix:* `uv-build-backend/src/lib.rs`, `uv-build-backend/src/source_dist.rs` @ L13 · churn 124 · findings that run: 793

**NOT FOUND.** File flagged (error-swallows×3, stringly×2) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 107. [#12156](https://github.com/astral-sh/uv/pull/12156) · 0.11.15 · `missing-match-arm` · **COINCIDENT**

> Fix Git submodule handling when using relative paths

*Fix:* `uv-git/src/git.rs` @ L12 · churn 161 · findings that run: 787

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 787 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 108. [#19452](https://github.com/astral-sh/uv/pull/19452) · 0.11.15 · `predicate-change` · **MISS**

> Fix line number reporting in netrc parsing

*Fix:* `uv-netrc/src/lex.rs`, `uv-netrc/src/netrc.rs` @ L29 · churn 21 · findings that run: 793

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 109. [#19343](https://github.com/astral-sh/uv/pull/19343) · 0.11.14 · `missing-match-arm` · **FILE**

> Avoid applying `.env` files in parent process

*Fix:* `uv/src/commands/tool/run.rs` @ L84 · churn 132 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×4, enum-coverage --all×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 110. [#19311](https://github.com/astral-sh/uv/pull/19311) · 0.11.14 · `string-parse` · **MISS**

> Filter ANSI codes in logging output

*Fix:* `uv-logging/src/lib.rs`, `uv/src/logging.rs` @ L5 · churn 74 · findings that run: 775

**NOT FOUND.** No finding anywhere in the changed file(s). `stringly` flags *branching on* string literals, not incorrect string operations.

### 111. [#19332](https://github.com/astral-sh/uv/pull/19332) · 0.11.14 · `missing-match-arm` · **COINCIDENT**

> Fix `uv tree` showing extra-conditional deps for packages required without extras

*Fix:* `uv-resolver/src/lock/tree.rs` @ L19 · churn 83 · findings that run: 775

**NOT FOUND (coincidental).** `enum-coverage --all` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 775 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 112. [#19366](https://github.com/astral-sh/uv/pull/19366) · 0.11.14 · `missing-match-arm` · **DEGEN**

> Respect build options (e.g., `--no-build`) during lock validation

*Fix:* `uv-distribution/src/error.rs`, `uv-distribution/src/source/mod.rs` @ L39 · churn 289 · findings that run: 775

**NOT FOUND (degenerate).** Only `metrics` covers the site; it flags a whole item (god function / dead code), so any fix inside matches by construction.

### 113. [#19312](https://github.com/astral-sh/uv/pull/19312) · 0.11.13 · `error-handling` · **FILE**

> Include data files in editable builds

*Fix:* `uv-build-backend/src/wheel.rs` @ L232 · churn 85 · findings that run: 775

**NOT FOUND.** File flagged (builder-drift×3, error-swallows×2) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 114. [#19334](https://github.com/astral-sh/uv/pull/19334) · 0.11.13 · `predicate-change` · **FILE**

> Respect `--require-hashes` when installing from `pylock.toml` files

*Fix:* `uv/src/commands/pip/install.rs`, `uv/src/commands/pip/sync.rs` @ L525 · churn 15 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×4) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 115. [#19313](https://github.com/astral-sh/uv/pull/19313) · 0.11.12 · `predicate-change` · **COINCIDENT**

> Respect `--no-dev` over `UV_DEV=1`

*Fix:* `uv-cli/src/options.rs`, `uv/src/settings.rs` @ L145 · churn 177 · findings that run: 775

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 775 findings that run, near-misses are expected. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 116. [#19294](https://github.com/astral-sh/uv/pull/19294) · 0.11.12 · `other` · **FILE**

> Don't suggest non-existent `--no-frozen` flag

*Fix:* `uv/src/commands/project/lock.rs` @ L233 · churn 2 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×7, enum-coverage --all×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 117. [#19301](https://github.com/astral-sh/uv/pull/19301) · 0.11.11 · `plumb-new-param` · **MISS**

> Accept legacy ID format from pre-0.11.9 cache entries

*Fix:* `uv-distribution/src/source/revision.rs` @ L57 · churn 35 · findings that run: 775

**NOT FOUND.** No finding anywhere in the changed file(s). The fix threads new state through signatures: a behavior addition, not a local smell.

### 118. [#19286](https://github.com/astral-sh/uv/pull/19286) · 0.11.10 · `missing-match-arm` · **COINCIDENT**

> Allow pre-release Python requests with non-zero patch versions

*Fix:* `uv-python/src/discovery.rs` @ L205 · churn 156 · findings that run: 775

**NOT FOUND (coincidental).** `builder-drift` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 775 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 119. [#18700](https://github.com/astral-sh/uv/pull/18700) · 0.11.9 · `predicate-change` · **COINCIDENT**

> Discover versioned Python executables when `requires-python` pins a version

*Fix:* `uv-distribution-types/src/requires_python.rs`, `uv-python/src/discovery.rs` @ L281 · churn 149 · findings that run: 776

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 776 findings that run, near-misses are expected. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 120. [#19154](https://github.com/astral-sh/uv/pull/19154) · 0.11.9 · `string-parse` · **COINCIDENT**

> Fix URL prefix matching to require path boundaries

*Fix:* `uv-auth/src/index.rs`, `uv-auth/src/providers.rs` @ L73 · churn 171 · findings that run: 775

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 775 findings that run, near-misses are expected. `stringly` flags *branching on* string literals, not incorrect string operations.

### 121. [#19269](https://github.com/astral-sh/uv/pull/19269) · 0.11.9 · `panic-removal` · **COINCIDENT**

> Fix transitive Git path dependencies in lockfiles

*Fix:* `uv-distribution/src/metadata/lowering.rs`, `uv-distribution/src/metadata/requires_dist.rs` @ L13 · churn 118 · findings that run: 775

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 775 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 122. [#19229](https://github.com/astral-sh/uv/pull/19229) · 0.11.9 · `missing-match-arm` · **FILE**

> Handle incorrect unlock error in `LockedFile::drop` on Wine

*Fix:* `uv-fs/src/locked_file.rs` @ L11 · churn 27 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 123. [#19114](https://github.com/astral-sh/uv/pull/19114) · 0.11.9 · `missing-call` · **FILE**

> Prevent uninstalling site-packages for empty `top_level.txt` in `.egg-info`

*Fix:* `uv-install-wheel/src/uninstall.rs` @ L228 · churn 123 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 124. [#19213](https://github.com/astral-sh/uv/pull/19213) · 0.11.9 · `missing-match-arm` · **COINCIDENT**

> Use symlinks instead of junctions on Wine

*Fix:* `uv-fs/src/lib.rs`, `uv-python/src/managed.rs` @ L81 · churn 200 · findings that run: 774

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 774 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 125. [#19157](https://github.com/astral-sh/uv/pull/19157) · 0.11.9 · `missing-match-arm` · **COINCIDENT**

> Fix floating-point environment handling on ARMv7

*Fix:* `uv-platform/src/lib.rs`, `uv-platform/src/libc.rs` @ L140 · churn 73 · findings that run: 777

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 777 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 126. [#19216](https://github.com/astral-sh/uv/pull/19216) · 0.11.9 · `error-handling` · **FILE**

> Redact credentials from remote requirements URL in offline errors

*Fix:* `uv-requirements-txt/src/lib.rs` @ L277 · churn 22 · findings that run: 774

**NOT FOUND.** File flagged (error-swallows×10) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 127. [#19199](https://github.com/astral-sh/uv/pull/19199) · 0.11.9 · `panic-removal` · **COINCIDENT**

> Windows tramplolines no longer set `PYTHONHOME` and only set `__PYVENV_LAUNCHER__` for virtual environments

*Fix:* `uv-static/src/env_vars.rs`, `uv-trampoline/src/bounce.rs` @ L631 · churn 50 · findings that run: 776

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 776 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 128. [#19131](https://github.com/astral-sh/uv/pull/19131) · 0.11.8 · `other` · **MISS**

> Add `rust-toolchain.toml` to uv-build sdist

*Fix:* — @ L76 · churn 0 · findings that run: 777

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 129. [#19088](https://github.com/astral-sh/uv/pull/19088) · 0.11.8 · `error-handling` · **COINCIDENT**

> Ensure uv invocations of git do not inherit repository location environment variables

*Fix:* `uv-configuration/src/vcs.rs`, `uv-git/src/git.rs` @ L3 · churn 98 · findings that run: 776

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 776 findings that run, near-misses are expected. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 130. [#19146](https://github.com/astral-sh/uv/pull/19146) · 0.11.8 · `string-parse` · **FILE**

> Redact pre-signed upload URLs in verbose output

*Fix:* `uv-redacted/src/lib.rs` @ L9 · churn 113 · findings that run: 777

**NOT FOUND.** File flagged (error-swallows×4, conversion-pairs×3) but never near the defect. `stringly` flags *branching on* string literals, not incorrect string operations.

### 131. [#19086](https://github.com/astral-sh/uv/pull/19086) · 0.11.8 · `serde-schema` · **COINCIDENT**

> Handle transitive URL dependencies in PEP 517 build requirements (#19076

*Fix:* `uv-build-frontend/src/lib.rs`, `uv-dispatch/src/lib.rs` @ L36 · churn 81 · findings that run: 777

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 777 findings that run, near-misses are expected. No serialization/compatibility check exists; wire-format drift is outside the model.

### 132. [#19087](https://github.com/astral-sh/uv/pull/19087) · 0.11.8 · `error-handling` · **FILE**

> Support `uv lock` on a `pyproject.toml` that only contains dependency-groups

*Fix:* `uv/src/commands/project/lock.rs` @ L43 · churn 9 · findings that run: 776

**NOT FOUND.** File flagged (error-swallows×7, enum-coverage --all×1) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 133. [#19102](https://github.com/astral-sh/uv/pull/19102) · 0.11.8 · `other` · **COINCIDENT**

> Disable transparent Python upgrades in projects when a patch version is requested via `.python-version`

*Fix:* `uv/src/commands/project/mod.rs` @ L1415 · churn 9 · findings that run: 776

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 776 findings that run, near-misses are expected. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 134. [#19012](https://github.com/astral-sh/uv/pull/19012) · 0.11.8 · `other` · **FILE**

> Fix Python variant tagging in the Windows registry

*Fix:* `uv-python/src/windows_registry.rs` @ L203 · churn 13 · findings that run: 775

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 135. [#19144](https://github.com/astral-sh/uv/pull/19144) · 0.11.8 · `missing-match-arm` · **FILE**

> Ban external symlinks in `.tar.zst` wheels

*Fix:* `uv-distribution/src/distribution_database.rs`, `uv-extract/src/stream.rs` @ L893 · churn 75 · findings that run: 777

**NOT FOUND.** File flagged (metrics×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 136. [#18966](https://github.com/astral-sh/uv/pull/18966) · 0.11.7 · `other` · **FILE**

> De-quote `workspace metadata` in linehaul data

*Fix:* `uv/src/lib.rs` @ L1968 · churn 2 · findings that run: 772

**NOT FOUND.** File flagged (error-swallows×3, metrics×2) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 137. [#18891](https://github.com/astral-sh/uv/pull/18891) · 0.11.7 · `missing-match-arm` · **COINCIDENT**

> Avoid installing tool workspace member dependencies as editable

*Fix:* `uv-bench/benches/uv.rs`, `uv-build-frontend/src/lib.rs` @ L138 · churn 203 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows`, `conversion-pairs` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 772 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 138. [#18976](https://github.com/astral-sh/uv/pull/18976) · 0.11.7 · `missing-match-arm` · **COINCIDENT**

> Emit JSON report for `uv sync --check` failures

*Fix:* `uv/src/commands/diagnostics.rs`, `uv/src/commands/pip/operations.rs` @ L139 · churn 121 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 772 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 139. [#18951](https://github.com/astral-sh/uv/pull/18951) · 0.11.7 · `panic-removal` · **COINCIDENT**

> Filter and warn on invalid TLS certificates

*Fix:* `uv-client/src/base_client.rs`, `uv-client/src/tls.rs` @ L50 · churn 188 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 772 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 140. [#18960](https://github.com/astral-sh/uv/pull/18960) · 0.11.7 · `serde-schema` · **FILE**

> Fix equality comparisons for version specifiers with `~=` operators

*Fix:* `uv-pep440/src/version_specifier.rs` @ L3 · churn 98 · findings that run: 772

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. No serialization/compatibility check exists; wire-format drift is outside the model.

### 141. [#18961](https://github.com/astral-sh/uv/pull/18961) · 0.11.7 · `other` · **COINCIDENT**

> Fix stale Python upgrade preview feature check in project environment construction

*Fix:* `uv/src/commands/project/mod.rs` @ L30 · churn 9 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 772 findings that run, near-misses are expected. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 142. [#18945](https://github.com/astral-sh/uv/pull/18945) · 0.11.7 · `panic-removal` · **COINCIDENT**

> Improve Windows path normalization

*Fix:* `uv-build-backend/src/lib.rs`, `uv-build-backend/src/source_dist.rs` @ L23 · churn 239 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 772 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 143. [#18942](https://github.com/astral-sh/uv/pull/18942) · 0.11.6 · `missing-match-arm` · **COINCIDENT**

> Do not remove files outside the venv on uninstall

*Fix:* `uv-dispatch/src/lib.rs`, `uv-install-wheel/src/uninstall.rs` @ L380 · churn 224 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 772 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 144. [#18943](https://github.com/astral-sh/uv/pull/18943) · 0.11.6 · `panic-removal` · **COINCIDENT**

> Validate and heal wheel `RECORD` during installation

*Fix:* `uv-build-backend/src/wheel.rs`, `uv-distribution-types/src/lib.rs` @ L400 · churn 467 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows`, `builder-drift` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 772 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 145. [#18856](https://github.com/astral-sh/uv/pull/18856) · 0.11.6 · `missing-match-arm` · **COINCIDENT**

> Avoid `uv cache clean` errors due to Win32 path normalization

*Fix:* `uv-cache/src/removal.rs`, `uv-fs/src/path.rs` @ L65 · churn 180 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 772 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 146. [#18612](https://github.com/astral-sh/uv/pull/18612) · 0.11.5 · `missing-call` · **DEGEN**

> Normalize persisted fork markers before lock equality checks

*Fix:* `uv-resolver/src/lock/mod.rs` @ L351 · churn 65 · findings that run: 772

**NOT FOUND (degenerate).** Only `metrics` covers the site; it flags a whole item (god function / dead code), so any fix inside matches by construction.

### 147. [#18815](https://github.com/astral-sh/uv/pull/18815) · 0.11.5 · `missing-call` · **MISS**

> Clear junction properly when uninstalling Python versions on Windows

*Fix:* `uv/src/commands/python/uninstall.rs` @ L200 · churn 83 · findings that run: 772

**NOT FOUND.** No finding anywhere in the changed file(s). `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 148. [#18904](https://github.com/astral-sh/uv/pull/18904) · 0.11.5 · `panic-removal` · **COINCIDENT**

> Report error cleanly instead of panicking on TLS certificate error

*Fix:* `uv-bench/benches/uv.rs`, `uv-bin-install/src/lib.rs` @ L73 · churn 195 · findings that run: 772

**NOT FOUND (coincidental).** `error-swallows`, `divergence --handling` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 772 findings that run, near-misses are expected. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 149. [#18828](https://github.com/astral-sh/uv/pull/18828) · 0.11.4 · `predicate-change` · **COINCIDENT**

> Avoid panics in environment finding via cycle detection

*Fix:* `uv-resolver/src/resolver/mod.rs` @ L4145 · churn 77 · findings that run: 771

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 771 findings that run, near-misses are expected. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 150. [#18786](https://github.com/astral-sh/uv/pull/18786) · 0.11.4 · `missing-match-arm` · **COINCIDENT**

> Enforce direct URL hashes for `pyproject.toml` dependencies

*Fix:* `uv-requirements/src/lib.rs`, `uv-requirements/src/lookahead.rs` @ L34 · churn 126 · findings that run: 771

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 771 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 151. [#18832](https://github.com/astral-sh/uv/pull/18832) · 0.11.4 · `missing-match-arm` · **DEGEN**

> Error on `--locked` and `--frozen` when script lockfile is missing

*Fix:* `uv/src/commands/project/run.rs` @ L77 · churn 45 · findings that run: 771

**NOT FOUND (degenerate).** Only `metrics` covers the site; it flags a whole item (god function / dead code), so any fix inside matches by construction.

### 152. [#18888](https://github.com/astral-sh/uv/pull/18888) · 0.11.4 · `missing-match-arm` · **COINCIDENT**

> Fix `uv export` extra resolution for workspace member and conflicting extras

*Fix:* `uv-resolver/src/lock/export/mod.rs`, `uv-resolver/src/universal_marker.rs` @ L18 · churn 272 · findings that run: 771

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 771 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 153. [#18886](https://github.com/astral-sh/uv/pull/18886) · 0.11.4 · `missing-match-arm` · **FILE**

> Include conflicts defined in virtual workspace root

*Fix:* `uv-pypi-types/src/conflicts.rs`, `uv-workspace/src/workspace.rs` @ L7 · churn 142 · findings that run: 771

**NOT FOUND.** File flagged (error-swallows×15, enum-coverage --all×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 154. [#18899](https://github.com/astral-sh/uv/pull/18899) · 0.11.4 · `arith-overflow` · **FILE**

> Recompute relative `exclude-newer` values during `uv tree --outdated`

*Fix:* `uv-resolver/src/exclude_newer.rs`, `uv/src/commands/project/tree.rs` @ L267 · churn 71 · findings that run: 771

**NOT FOUND.** File flagged (builder-drift×3) but never near the defect. No check models arithmetic discipline; `casts` covers lossy `as` casts only, so a `+` that should be `saturating_add` is invisible.

### 155. [#18861](https://github.com/astral-sh/uv/pull/18861) · 0.11.4 · `plumb-new-param` · **FILE**

> Respect `--exclude-newer` in `uv tool list --outdated`

*Fix:* `uv-cli/src/lib.rs`, `uv/src/commands/tool/list.rs` @ L5807 · churn 42 · findings that run: 771

**NOT FOUND.** File flagged (error-swallows×149, metrics×2) but never near the defect. The fix threads new state through signatures: a behavior addition, not a local smell.

### 156. [#18850](https://github.com/astral-sh/uv/pull/18850) · 0.11.4 · `plumb-new-param` · **FILE**

> Sort by comparator to break specifier ties

*Fix:* `uv-pep440/src/version_specifier.rs` @ L70 · churn 18 · findings that run: 771

**NOT FOUND.** File flagged (error-swallows×1) but never near the defect. The fix threads new state through signatures: a behavior addition, not a local smell.

### 157. [#18901](https://github.com/astral-sh/uv/pull/18901) · 0.11.4 · `missing-match-arm` · **COINCIDENT**

> Store relative timestamps in tool receipts

*Fix:* `uv-resolver/src/exclude_newer.rs`, `uv-resolver/src/lib.rs` @ L10 · churn 214 · findings that run: 771

**NOT FOUND (coincidental).** `conversion-pairs`, `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 771 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 158. [#18852](https://github.com/astral-sh/uv/pull/18852) · 0.11.4 · `predicate-change` · **DEGEN**

> Track newly-activated extras when determining conflicts

*Fix:* `uv-resolver/src/lock/installable.rs` @ L19 · churn 30 · findings that run: 771

**NOT FOUND (degenerate).** Only `metrics` covers the site; it flags a whole item (god function / dead code), so any fix inside matches by construction.

### 159. [#18831](https://github.com/astral-sh/uv/pull/18831) · 0.11.4 · `path-handling` · **MISS**

> Patch `Cargo.lock` in `uv-build` source distributions

*Fix:* — @ L74 · churn 0 · findings that run: 774

**NOT FOUND.** No finding anywhere in the changed file(s). No path-semantics check; join/canonicalize mistakes look like ordinary calls.

### 160. [#18797](https://github.com/astral-sh/uv/pull/18797) · 0.11.3 · `missing-match-arm` · **FILE**

> Bump simple API cache

*Fix:* `uv-cache/src/lib.rs` @ L1190 · churn 2 · findings that run: 771

**NOT FOUND.** File flagged (stringly×2, dead-code×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 161. [#18794](https://github.com/astral-sh/uv/pull/18794) · 0.11.3 · `predicate-change` · **FOUND**

> Don't drop `blake2b` hashes

*Fix:* `uv-pypi-types/src/simple_json.rs` @ L652 · churn 9 · findings that run: 771

**FOUND — verified.** `conversion-pairs`: Flagged the `HashDigests ↔ Hashes` From-pair as 'one concept in two shapes'. The bug was that one direction forgot the `blake2b` field — precisely the drift this check exists to catch.

### 162. [#18780](https://github.com/astral-sh/uv/pull/18780) · 0.11.3 · `missing-match-arm` · **FILE**

> Handle broken range request implementations

*Fix:* `uv-client/src/error.rs`, `uv-client/src/registry_client.rs` @ L11 · churn 62 · findings that run: 771

**NOT FOUND.** File flagged (error-swallows×2, dead-code×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 163. [#18800](https://github.com/astral-sh/uv/pull/18800) · 0.11.3 · `other` · **MISS**

> Remove `powerpc64-unknown-linux-gnu` from release build targets

*Fix:* — @ L30 · churn 0 · findings that run: 771

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 164. [#18742](https://github.com/astral-sh/uv/pull/18742) · 0.11.3 · `error-handling` · **COINCIDENT**

> Respect dependency metadata overrides in `uv pip check`

*Fix:* `uv-installer/src/site_packages.rs`, `uv/src/commands/pip/check.rs` @ L11 · churn 82 · findings that run: 768

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 768 findings that run, near-misses are expected. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 165. [#18739](https://github.com/astral-sh/uv/pull/18739) · 0.11.3 · `panic-removal` · **FILE**

> Support debug CPython ABI tags in environment compatibility

*Fix:* `uv-bench/benches/uv.rs`, `uv-installer/src/plan.rs` @ L127 · churn 191 · findings that run: 771

**NOT FOUND.** File flagged (error-swallows×5, config-drift×3) but never near the defect. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 166. [#17890](https://github.com/astral-sh/uv/pull/17890) · 0.11.2 · `missing-match-arm` · **COINCIDENT**

> Skip redundant project configuration parsing for `uv run`

*Fix:* `uv/src/commands/project/run.rs`, `uv/src/lib.rs` @ L31 · churn 277 · findings that run: 766

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 766 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 167. [#18686](https://github.com/astral-sh/uv/pull/18686) · 0.11.1 · `other` · **MISS**

> Add missing hash verification for `riscv64gc-unknown-linux-musl`

*Fix:* — @ L32 · churn 0 · findings that run: 766

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 168. [#18688](https://github.com/astral-sh/uv/pull/18688) · 0.11.1 · `missing-match-arm` · **MISS**

> Fallback to direct download when direct URL streaming is unsupported

*Fix:* `uv-distribution/src/distribution_database.rs` @ L251 · churn 22 · findings that run: 766

**NOT FOUND.** No finding anywhere in the changed file(s). `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 169. [#18692](https://github.com/astral-sh/uv/pull/18692) · 0.11.1 · `missing-match-arm` · **FILE**

> Revert treating 'Dynamic' values as case-insensitive

*Fix:* `uv-pypi-types/src/metadata/metadata_resolver.rs` @ L82 · churn 36 · findings that run: 766

**NOT FOUND.** File flagged (error-swallows×3) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 170. [#18703](https://github.com/astral-sh/uv/pull/18703) · 0.11.1 · `other` · **FILE**

> Remove torchdata from list of packages to source from the PyTorch index

*Fix:* `uv-torch/src/backend.rs` @ L335 · churn 3 · findings that run: 766

**NOT FOUND.** File flagged (error-swallows×1, metrics×1) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 171. [#9697](https://github.com/astral-sh/uv/pull/9697) · 0.11.1 · `predicate-change` · **FILE**

> Special-case `==` Python version request ranges

*Fix:* `uv-python/src/discovery.rs` @ L3326 · churn 5 · findings that run: 766

**NOT FOUND.** File flagged (error-swallows×7, builder-drift×3) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 172. [#18457](https://github.com/astral-sh/uv/pull/18457) · 0.11.0 · `missing-match-arm` · **COINCIDENT**

> Find the dynamic linker on the file system when sniffing binaries fails

*Fix:* `uv-platform/src/libc.rs` @ L6 · churn 172 · findings that run: 763

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 763 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 173. [#18666](https://github.com/astral-sh/uv/pull/18666) · 0.11.0 · `predicate-change` · **FILE**

> Fix export of conflicting workspace members with dependencies

*Fix:* `uv-resolver/src/lock/export/mod.rs` @ L98 · churn 5 · findings that run: 766

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 174. [#18586](https://github.com/astral-sh/uv/pull/18586) · 0.11.0 · `error-handling` · **FILE**

> Respect installed settings in `uv tool list --outdated`

*Fix:* `uv/src/commands/tool/list.rs` @ L14 · churn 67 · findings that run: 764

**NOT FOUND.** File flagged (error-swallows×3) but never near the defect. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 175. [#18680](https://github.com/astral-sh/uv/pull/18680) · 0.11.0 · `missing-match-arm` · **FILE**

> Treat paths originating as PEP 508 URLs which contain expanded variables as relative

*Fix:* `uv-pep508/src/verbatim_url.rs`, `uv-requirements-txt/src/lib.rs` @ L32 · churn 104 · findings that run: 766

**NOT FOUND.** File flagged (error-swallows×11, conversion-pairs×3) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 176. [#18635](https://github.com/astral-sh/uv/pull/18635) · 0.11.0 · `predicate-change` · **COINCIDENT**

> Fix `uv export` for workspace member packages with conflicts

*Fix:* `uv-resolver/src/lock/export/mod.rs` @ L86 · churn 5 · findings that run: 764

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 764 findings that run, near-misses are expected. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 177. [#18425](https://github.com/astral-sh/uv/pull/18425) · 0.11.0 · `missing-match-arm` · **FILE**

> Continue to alternative authentication providers when the pyx store has no token

*Fix:* `uv-auth/src/middleware.rs` @ L752 · churn 69 · findings that run: 763

**NOT FOUND.** File flagged (error-swallows×3, stringly×1) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 178. [#18599](https://github.com/astral-sh/uv/pull/18599) · 0.11.0 · `other` · **FILE**

> Use redacted URLs for log messages in cached client

*Fix:* `uv-client/src/cached_client.rs` @ L285 · churn 26 · findings that run: 764

**NOT FOUND.** File flagged (error-swallows×4, conversion-pairs×3) but never near the defect. Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 179. [#18459](https://github.com/astral-sh/uv/pull/18459) · 0.10.12 · `missing-match-arm` · **COINCIDENT**

> Improve reporting of managed interpreter symlinks in `uv python list`

*Fix:* `uv/src/commands/python/list.rs` @ L144 · churn 19 · findings that run: 763

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 763 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 180. [#18557](https://github.com/astral-sh/uv/pull/18557) · 0.10.12 · `missing-call` · **FILE**

> Preserve end-of-line comments on previous entries when removing dependencies

*Fix:* `uv-workspace/src/pyproject_mut.rs` @ L1548 · churn 235 · findings that run: 763

**NOT FOUND.** File flagged (error-swallows×2) but never near the defect. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 181. [#18536](https://github.com/astral-sh/uv/pull/18536) · 0.10.12 · `predicate-change` · **MISS**

> Treat abi3 wheel Python version as a lower bound

*Fix:* `uv-distribution-types/src/prioritized_distribution.rs` @ L928 · churn 71 · findings that run: 763

**NOT FOUND.** No finding anywhere in the changed file(s). No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 182. [#18530](https://github.com/astral-sh/uv/pull/18530) · 0.10.12 · `string-parse` · **MISS**

> Detect hard-float support on aarch64 kernels running armv7 userspace

*Fix:* `uv-platform/src/cpuinfo.rs` @ L10 · churn 66 · findings that run: 763

**NOT FOUND.** No finding anywhere in the changed file(s). `stringly` flags *branching on* string literals, not incorrect string operations.

### 183. [#18513](https://github.com/astral-sh/uv/pull/18513) · 0.10.11 · `missing-call` · **FILE**

> Allow `--project` to refer to a `pyproject.toml` directly and reduce to a warning on other files

*Fix:* `uv/src/lib.rs` @ L90 · churn 23 · findings that run: 763

**NOT FOUND.** File flagged (error-swallows×3, metrics×2) but never near the defect. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 184. [#18452](https://github.com/astral-sh/uv/pull/18452) · 0.10.11 · `missing-match-arm` · **COINCIDENT**

> Disable `SYSTEM_VERSION_COMPAT` when querying interpreters on macOS

*Fix:* `uv-python/src/interpreter.rs` @ L975 · churn 64 · findings that run: 765

**NOT FOUND (coincidental).** `config-drift` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 765 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 185. [#18451](https://github.com/astral-sh/uv/pull/18451) · 0.10.11 · `error-handling` · **COINCIDENT**

> Enforce available distributions for supported environments

*Fix:* `uv-resolver/src/options.rs`, `uv-resolver/src/resolver/mod.rs` @ L17 · churn 58 · findings that run: 765

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 765 findings that run, near-misses are expected. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 186. [#18398](https://github.com/astral-sh/uv/pull/18398) · 0.10.11 · `string-parse` · **COINCIDENT**

> Fix `uv sync --active` recreating active environments when `UV_PYTHON_INSTALL_DIR` is relative

*Fix:* `uv-python/src/interpreter.rs`, `uv-python/src/managed.rs` @ L306 · churn 51 · findings that run: 765

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 765 findings that run, near-misses are expected. `stringly` flags *branching on* string literals, not incorrect string operations.

### 187. [#18373](https://github.com/astral-sh/uv/pull/18373) · 0.10.10 · `panic-removal` · **FILE**

> Avoid sharing version metadata across indexes

*Fix:* `uv-requirements/src/extras.rs`, `uv-requirements/src/lookahead.rs` @ L6 · churn 268 · findings that run: 767

**NOT FOUND.** File flagged (error-swallows×27, conversion-pairs×6) but never near the defect. No check scores `.unwrap()`/`.expect()` density. `error-swallows` tracks *discarded* Results, not ones that panic.

### 188. [#18362](https://github.com/astral-sh/uv/pull/18362) · 0.10.10 · `other` · **MISS**

> Bump zlib-rs to 0.6.2 to fix panic on decompression of large wheels on Windows

*Fix:* — @ L8301 · churn 0 · findings that run: 767

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 189. [#18445](https://github.com/astral-sh/uv/pull/18445) · 0.10.10 · `error-handling` · **COINCIDENT**

> Filter out unsupported environment wheels

*Fix:* `uv-resolver/src/lock/mod.rs`, `uv/src/commands/project/lock.rs` @ L301 · churn 228 · findings that run: 764

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 764 findings that run, near-misses are expected. `error-swallows` fires on dropped Results. These fixes changed *which* error is produced or where it propagates — the Result was handled, just wrongly.

### 190. [#18176](https://github.com/astral-sh/uv/pull/18176) · 0.10.10 · `panic-removal` · **FOUND**

> Preserve absolute/relative paths in lockfiles

*Fix:* `uv-distribution-types/src/requirement.rs`, `uv-fs/src/path.rs` @ L9 · churn 201 · findings that run: 767

**FOUND — verified.** `error-swallows`: Flagged `.unwrap_or_else(|_| dist.install_path.clone())` — a silent fallback that discarded whether the path was originally absolute. That discard was the defect.

### 191. [#18399](https://github.com/astral-sh/uv/pull/18399) · 0.10.10 · `missing-call` · **FILE**

> Recreate Python environments under `uv tool install --force`

*Fix:* `uv-cli/src/lib.rs`, `uv/src/commands/tool/install.rs` @ L5707 · churn 47 · findings that run: 767

**NOT FOUND.** File flagged (error-swallows×5, enum-coverage --all×1) but never near the defect. `builder-drift` needs a sibling chain sharing the same constructor and constant args; absent that, an omitted step is unremarkable.

### 192. [#18396](https://github.com/astral-sh/uv/pull/18396) · 0.10.10 · `missing-match-arm` · **COINCIDENT**

> Respect timestamp and other cache keys in cached environments

*Fix:* `uv/src/commands/project/environment.rs`, `uv/src/commands/project/mod.rs` @ L13 · churn 59 · findings that run: 767

**NOT FOUND (coincidental).** `conversion-pairs` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 767 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 193. [#18433](https://github.com/astral-sh/uv/pull/18433) · 0.10.10 · `missing-match-arm` · **COINCIDENT**

> Simplify selected extra markers in `uv export`

*Fix:* `uv-resolver/src/lock/export/mod.rs` @ L57 · churn 47 · findings that run: 768

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 768 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 194. [#18334](https://github.com/astral-sh/uv/pull/18334) · 0.10.10 · `other` · **MISS**

> Send pyx mint-token requests with a proper `Content-Type`

*Fix:* `uv-publish/src/trusted_publishing/pypi.rs`, `uv-publish/src/trusted_publishing/pyx.rs` @ L86 · churn 4 · findings that run: 767

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

### 195. [#18383](https://github.com/astral-sh/uv/pull/18383) · 0.10.10 · `missing-match-arm` · **COINCIDENT**

> Fix Windows operating system and version reporting

*Fix:* `uv-platform/src/host.rs` @ L14 · churn 39 · findings that run: 767

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 767 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 196. [#18291](https://github.com/astral-sh/uv/pull/18291) · 0.10.9 · `predicate-change` · **COINCIDENT**

> Continue on trampoline job assignment failures

*Fix:* `uv-trampoline/src/bounce.rs` @ L454 · churn 20 · findings that run: 763

**NOT FOUND (coincidental).** `error-swallows` fired within 15 lines of the fix, but unreviewed proximity match; treat as noise. With 763 findings that run, near-misses are expected. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 197. [#17699](https://github.com/astral-sh/uv/pull/17699) · 0.10.9 · `missing-match-arm` · **COINCIDENT**

> Handle the hard link limit gracefully instead of failing

*Fix:* `uv-fs/src/link.rs`, `uv-static/src/env_vars.rs` @ L768 · churn 47 · findings that run: 763

**NOT FOUND (coincidental).** `divergence --handling`, `error-swallows` fired within 15 lines of the fix, but reviewed and rejected — proximity only, different construct. With 763 findings that run, near-misses are expected. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 198. [#18350](https://github.com/astral-sh/uv/pull/18350) · 0.10.9 · `predicate-change` · **FILE**

> Respect build constraints for workspace members

*Fix:* `uv/src/commands/build_frontend.rs`, `uv/src/lib.rs` @ L27 · churn 26 · findings that run: 766

**NOT FOUND.** File flagged (error-swallows×142, enum-coverage --all×2) but never near the defect. No check reasons about boolean conditions — a wrong `&&`/negation is structurally identical to a right one.

### 199. [#18328](https://github.com/astral-sh/uv/pull/18328) · 0.10.9 · `missing-match-arm` · **FILE**

> Revalidate editables and other dependencies in scripts

*Fix:* `uv-resolver/src/lock/mod.rs` @ L15 · churn 88 · findings that run: 765

**NOT FOUND.** File flagged (error-swallows×8, conversion-pairs×6) but never near the defect. `enum-coverage`/`divergence` target this shape, but only fire when a more-complete sibling site exists to compare against.

### 200. [#18301](https://github.com/astral-sh/uv/pull/18301) · 0.10.9 · `other` · **MISS**

> Support Python 3.13+ on Android

*Fix:* — @ L489 · churn 0 · findings that run: 765

**NOT FOUND.** No finding anywhere in the changed file(s). Matches no consistency, exhaustiveness, or pattern-density shape in the battery.

