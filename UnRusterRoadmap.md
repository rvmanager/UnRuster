# UnRuster Roadmap: From Facts to RuleBook Enforcement

Goal: make UnRuster a static validator for `RustRuleBook.md`. Each row below is a candidate rule, mapped to its rulebook section, the rustc layer it would use, and an effort estimate.

## Design model

Two collector shapes already exist in `src/analysis/`:

1. **Inline lint** (`api_leak.rs`) — HIR/MIR visitor that emits a rustc `warn` *and* appends a fact for the viewer. Best for syntactic, per-function/per-signature rules.
2. **Pure fact collector** (`call_graph.rs`, `mutation.rs`) — dumps raw events; the viewer aggregates and decides "is this bad". Best for cross-crate or threshold-based rules.

We will keep this split. New rulebook rules become either (1) or (2), depending on whether the verdict is local.

A third shape will eventually be needed:

3. **Whole-crate verdict in the driver** — when a rule needs to see every function in the crate before deciding (e.g., "unused `pub`"). These can run in `analysis::run_all` after the collectors have populated `facts`.

## Prioritized rule list

Ranked by `(rulebook-importance × signal-to-noise) / implementation-effort`.

| # | Rule | RuleBook § | Mechanism | Shape | Effort |
|---|---|---|---|---|---|
| 1 | `stringly_error` — `pub fn` returns `Result<_, String>` / `Result<_, &str>` | 4.5, 12 | HIR fn-sig visitor | inline lint | **S** (≈100 LOC) ✅ shipped |
| 2 | `unsafe_no_safety_comment` — `unsafe { … }` block with no `// SAFETY:` comment above it | 11.3 | HIR expr visitor + source-map lookup | inline lint | S |
| 3 | `missing_debug` — `pub struct` / `pub enum` without `Debug` impl | 5.5 | HIR item + `tcx.all_impls` lookup | inline lint | S |
| 4 | `public_unwrap` — `unwrap()` / `expect()` inside the body of any non-test `pub fn` | 4.4 | MIR call-terminator scan against std lang items | inline lint | M |
| 5 | `unsafe_fn_missing_safety_doc` — `pub unsafe fn` without a `# Safety` doc section | 16.4 | HIR fn + attr scan | inline lint | S ✅ shipped |
| 6 | `owned_param_only_read` — `pub fn(x: String / Vec<T> / PathBuf)` where the body never mutates or moves out of `x` | 2.8, 3.2 | MIR local-usage analysis (already sketched in `lifetime_smells.rs`) | inline lint | M |
| 7 | `bool_pair_param` — `pub fn(_, bool, bool, …)` with ≥2 unnamed bools | (anti-pattern) | HIR fn-sig | inline lint | S |
| 8 | `pub_field_on_public_struct` — `pub struct` with `pub` fields that aren't a plain data carrier | 5.1 | HIR struct visitor (skip `#[derive]`-only types) | inline lint | S |
| 9 | `stringly_enum` — field of type `String` whose only assignments are string literals from a small fixed set | 1.1, 12 | MIR-level constant propagation across the crate; viewer rule | fact collector | L |
| 10 | `parallel_collections` — `Vec<T>` + `HashMap<K, T>` (or similar) on the same struct, both keyed by the same `K` | 12 (anti-pattern) | Already partly in place (`type_shape` on `FieldDef`) | viewer rule | M |
| 11 | `result_stringification_at_boundary` — `Display`/`format!` of an error type that is then wrapped in another error | 4.5 | MIR call-graph + ty info | viewer rule | L |
| 12 | `mutex_held_across_await` — `MutexGuard` (std) alive across an `.await` | 7.2, 8.2 | MIR liveness + async-state-machine traversal | inline lint | L |
| 13 | `unused_pub` — `pub fn` / `pub struct` never reachable from a public root or external user | 5.1 | Whole-crate verdict, needs full call graph | driver-level | M |
| 14 | `default_or_new_should_default` — type has `fn new() -> Self` with no args; flag if `Default` not also implemented | (idiom) | HIR impl scan | inline lint | S |
| 15 | `panic_in_lib` — `panic!`, `unreachable!`, `todo!`, `unimplemented!` reachable from `pub fn` | 4.1 | MIR call scan | inline lint | M |
| 16 | `error_string_concat` — error variant constructed via `format!`/`String::new() +` rather than a structured field | 4.5 | MIR pattern | viewer rule | L |
| 17 | `pub_trait_unsealed` — `pub trait` with no sealed marker and no `#[doc(non_exhaustive)]` | 2.4 | HIR trait scan | inline lint (advisory) | S |
| 18 | `clone_then_read` — `.clone()` on a value used only by shared reference afterwards | 3.2 | MIR liveness | viewer rule | L |
| 19 | `option_of_unit_field` — `Option<()>` field (should be `bool` or a typestate) | 1.1 | HIR/ty | inline lint | S |
| 20 | `result_of_option_unit` — `Result<(), Error>` chained where `Option<Error>` would be clearer at the API surface | (idiom) | HIR | advisory | M |

Effort key: S ≈ 1–2 hours, M ≈ half a day, L ≈ multi-day.

## Phased rollout

**Phase 1 — extend the api_leak template (rules 1, 5, 7, 8, 14, 17, 19).**
All are signature-only HIR visitors. Together they double the coverage of the public-API rulebook section with minimal new infrastructure. Each is its own module under `src/analysis/`, registered in `analysis::run_all`. Each appends a new fact variant in `unruster-facts/src/lib.rs`.

Deliverable: every public API surface in the analyzed crate gets a verdict from a half-dozen rules on every `cargo check`.

**Phase 2 — body-level lints (rules 2, 3, 4, 6, 15).**
Adds the source-map / MIR-walking dimension. Hardest piece is body-local analysis for #6 (`owned_param_only_read`) — the sketch in `lifetime_smells.rs` becomes real code.

Deliverable: rulebook §4 (error handling) and §11 (unsafe) are statically enforced.

**Phase 3 — whole-program rules (10, 13, 18).**
These live in `unruster-viewer` since the facts are already collected per-crate and merged. Surfaces them in the viewer's existing report.

Deliverable: rulebook §5.1 (smallest public surface) and §12 (anti-patterns) checks fire across crate boundaries.

**Phase 4 — semantic / dataflow rules (9, 11, 12, 16, 18, 20).**
These need real dataflow. Either ride MIR borrowck data or build small dedicated analyses. Worth a dedicated effort and a stable schema first.

## Output channels

Today UnRuster has two output channels: rustc warnings (inline, ephemeral) and facts JSON (durable, viewer-readable). Two more are worth adding:

- **`unruster.toml`** — per-rule on/off + severity, plus per-rule `allow` paths. Today every check fires unconditionally. Needed once we ship ≥5 rules.
- **SARIF or LSP-diagnostics export** — so editors and CI can render the warnings without a Rust toolchain present. Drop-in to the existing `Reporter`.

## What this roadmap deliberately omits

- **Clippy duplication.** Rules already well-served by clippy (e.g., `redundant_clone`, `needless_collect`, `option_as_ref_deref`) belong in clippy, not UnRuster. UnRuster's edge is *design*-level rules that span function/struct boundaries.
- **Style rules.** Formatting, naming case, brace style — all `rustfmt`/clippy territory.
- **Compiler-builtin lints.** `missing_docs`, `unreachable_pub`, etc. are rustc lints already; the rulebook recommends turning them on, not reimplementing them.

## First implementation

Starting with rule #1, `stringly_error`. It is the closest mechanical clone of `api_leak.rs`: same HIR visitor surface, same fact emission, same warning style. Implementing it establishes the "one rule = one module" template that Phase 1 builds on.

Files touched:
- `unruster-facts/src/lib.rs` — new `StringlyErrorFact` variant on `CrateFacts`.
- `src/analysis/stringly_error.rs` — new collector module.
- `src/analysis/mod.rs` — register it.
- `tests/fixtures/stringly/` — fixture crate covering positive and negative cases.
