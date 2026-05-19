# Rust Rule Book

A practical guide to designing Rust software well: patterns, principles, language features, and tools.

---

## 1. Core Design Principles

### 1.1 Make Illegal States Unrepresentable
Encode invariants in the type system so the compiler enforces them. If a value can only be one of three things, use an `enum` — not a `String` with a runtime check.

```rust
// Bad
struct Connection { state: String, socket: Option<TcpStream> }

// Good
enum Connection {
    Disconnected,
    Connecting { since: Instant },
    Connected { socket: TcpStream },
}
```

### 1.2 Parse, Don't Validate
Convert untrusted input into a typed, validated value *once* at the boundary. Downstream code accepts the parsed type and trusts it.

```rust
struct Email(String);                  // private inner — only constructable via parse
impl Email {
    fn parse(s: &str) -> Result<Self, EmailError> { /* ... */ }
}
```
Functions taking `Email` no longer need to re-check anything.

### 1.3 Push Side Effects to the Edges
Keep the core logic pure (data in → data out). I/O, time, randomness, logging belong in thin outer layers. Pure code is trivially testable.

### 1.4 Prefer Composition Over Inheritance
Rust has no inheritance — embrace composition: small structs containing other structs, behavior bolted on via traits.

### 1.5 The Code Should Read Like the Domain
Name types after concepts in the problem domain. `UserId`, `OrderTotal`, `RetryBudget` — not `u64`, `f64`, `u32`.

---

## 2. Type System Patterns

### 2.1 Newtype Pattern
Wrap a primitive in a struct to give it identity and prevent mix-ups.

```rust
struct UserId(u64);
struct OrderId(u64);

fn ship_order(user: UserId, order: OrderId) { /* … */ }
// ship_order(order_id, user_id) — compile error. Saved.
```

Use it for: units (`Meters(f64)`), IDs, validated values, capability tokens.

### 2.2 Type State Pattern
Encode object lifecycle in the type. Methods only exist on the states where they make sense.

```rust
struct Request<S> { url: String, _state: PhantomData<S> }
struct Draft; struct Sent;

impl Request<Draft> {
    fn send(self) -> Request<Sent> { /* … */ }
}
impl Request<Sent> {
    fn response(&self) -> &Response { /* … */ }
}
```
You cannot read a response from an unsent request — compiler-enforced.

### 2.3 Builder Pattern
For constructors with many optional fields. Use `derive_builder` or hand-roll. Use a *typestate builder* if some fields are required.

### 2.4 Sealed Traits
Prevent downstream crates from implementing your trait — preserves the right to add methods later without breaking changes.

```rust
mod private { pub trait Sealed {} }
pub trait MyTrait: private::Sealed { /* … */ }
```

### 2.5 Extension Traits
Add methods to types you don't own. Convention: name them `…Ext`.

```rust
trait StrExt { fn shout(&self) -> String; }
impl StrExt for str { fn shout(&self) -> String { self.to_uppercase() } }
```

### 2.6 Marker Traits and `PhantomData`
Use empty traits (`Send`, `Sync`, your own) for compile-time tagging. Use `PhantomData<T>` to "own" a type parameter you don't store at runtime — needed for variance and drop check.

### 2.7 Enums as State Machines
Prefer an enum of variants over a struct with a `kind` field plus optional payloads. Pattern matching gives you exhaustiveness for free.

### 2.8 Prefer `&str` over `String`, `&[T]` over `Vec<T>` in Parameters
Accept the most general borrowed form. Caller decides whether to own. For return types, return the concrete owned type unless lifetimes flow naturally.

### 2.9 `AsRef`, `Into`, `From`
- `From<T>` / `Into<T>` for owned conversions. Implement `From`; `Into` comes free.
- `AsRef<T>` for cheap reference conversion (`AsRef<Path>`, `AsRef<str>`).
- `TryFrom` when conversion can fail.

### 2.10 `Cow<'a, T>` for Maybe-Owned Data
Return `Cow<str>` when the result is usually a borrow but occasionally needs allocation (e.g., string normalization).

---

## 3. Ownership and Borrowing

### 3.1 Default to Borrowing
Take `&T` unless you need to consume. Take `&mut T` unless you need to consume. Take `T` only when you genuinely need ownership (storing, dropping, transforming-by-move).

### 3.2 Avoid `.clone()` as a Crutch
A `.clone()` to silence the borrow checker is a signal to rethink the design — not always wrong, but always worth a moment of thought. Reach for `Rc`/`Arc` when shared ownership is actually the model.

### 3.3 Interior Mutability Sparingly
`RefCell`, `Cell`, `Mutex`, `RwLock` punch a hole in the type system. Use when needed (shared mutable state, observer patterns) but never as a first reach.

### 3.4 Lifetimes: Elide When You Can, Name When You Must
Don't introduce explicit lifetimes for ergonomics. Introduce them when the compiler asks or when expressing a real constraint between inputs and outputs.

### 3.5 Avoid Self-Referential Structs
They don't work in safe Rust. Use indices, `Rc`, or restructure. If you truly need them, use `ouroboros` or `pin-project` — and only then.

---

## 4. Error Handling

### 4.1 `Result<T, E>` for Recoverable, `panic!` for Bugs
Library code: never panic on bad input — return `Result`. Panic is for programmer errors (broken invariants).

### 4.2 Error Type Strategy
- **Libraries:** define a specific error enum with `thiserror`. Each variant represents a distinct failure mode the caller might handle.
- **Binaries / application code:** use `anyhow::Result` with `.context(...)` for narrative chains.

```rust
#[derive(thiserror::Error, Debug)]
enum ParseError {
    #[error("unexpected token at line {line}")]
    UnexpectedToken { line: usize },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

### 4.3 Use `?` Liberally
The `?` operator is the idiomatic way to propagate. Combine with `From` impls so errors flow naturally up the stack.

### 4.4 No `unwrap()` / `expect()` in Library Code
Acceptable in tests, examples, and `main` when failure is "exit with message." Otherwise propagate.

### 4.5 Don't Stringify Errors Prematurely
Keep structured errors structured. Stringify only at the outer boundary (UI, log line).

---

## 5. API Design

### 5.1 Smallest Possible Public Surface
`pub` is a commitment. Default everything to private. Re-export deliberately from `lib.rs`.

### 5.2 Accept Generic, Return Concrete
- Parameters: `impl AsRef<Path>`, `impl IntoIterator<Item=…>` — flexible for callers.
- Returns: concrete types (`Vec<T>`, `MyStruct`) — easier to reason about. Use `impl Trait` for return when the type is internal/unspeakable.

### 5.3 Builder Over Long Argument Lists
Three optional parameters? Builder. Don't make callers pass `None, None, Some(x)`.

### 5.4 Make Types `Send + Sync` Where Reasonable
A type that can't cross threads or be shared limits its callers. Audit `Rc`, `RefCell`, raw pointers — they kill auto-traits.

### 5.5 Derive Liberally
`#[derive(Debug, Clone, PartialEq, Eq, Hash)]` on data types unless there's a reason not to. `Debug` is non-negotiable for anything that might appear in a log or assertion.

### 5.6 Prefer Iterators in Public APIs
Returning `impl Iterator<Item = T>` is more composable than `Vec<T>`. The caller decides whether to collect.

### 5.7 Document with `///` and Examples
Doc comments compile and run as tests (`cargo test --doc`). An example that compiles is documentation that can't go stale silently.

```rust
/// Returns the area of a rectangle.
///
/// ```
/// # use mycrate::area;
/// assert_eq!(area(3, 4), 12);
/// ```
pub fn area(w: u32, h: u32) -> u32 { w * h }
```

### 5.8 Semantic Versioning
Adding a method to a public trait is a breaking change unless the trait is sealed or the method has a default impl. Read the Rust API guidelines on what counts as breaking.

---

## 6. Modules and Project Structure

### 6.1 Module = Boundary, Not Just a Folder
A module exists to hide implementation details behind a chosen interface. If everything inside is `pub`, the module isn't doing its job.

### 6.2 Re-export From the Crate Root
Internal organization should not leak into your public path. Define types deep in the tree, re-export them at the top:
```rust
// in lib.rs
pub use crate::parser::ast::Expr;
```

### 6.3 Workspaces for Multi-Crate Projects
Split when crates have independent dependency graphs, compile times benefit, or you want to expose them separately. Don't split prematurely — module boundaries first, crate boundaries later.

### 6.4 Feature Flags
Use for optional dependencies and optional functionality. Default features should be the "expected" use. Keep features additive — enabling one should never remove functionality.

### 6.5 Avoid Cyclic Module Logic
Modules form a DAG. If module `a` and `b` reference each other, extract the shared piece into `c`.

---

## 7. Concurrency

### 7.1 Pick Your Model First
- **Shared memory:** `Arc<Mutex<T>>` / `Arc<RwLock<T>>` for shared state.
- **Message passing:** channels (`std::sync::mpsc`, `crossbeam`, `tokio::sync`). Often cleaner than locks.
- **Data parallelism:** `rayon` for embarrassingly parallel work.
- **Async I/O:** `tokio` (or `async-std`/`smol`). Don't mix runtimes.

### 7.2 Don't Hold Locks Across `.await`
Use `tokio::sync::Mutex` (not `std::sync::Mutex`) only when you must hold across await — otherwise prefer the std mutex and drop before `.await`.

### 7.3 Make Shared State a Last Resort
Easier to reason about: pure functions, owned data, channels. Lock contention is invisible until it isn't.

### 7.4 `Send` and `Sync` Are Promises
If a type isn't `Send`, it can't move between threads. If it isn't `Sync`, it can't be shared by reference. Most types are auto-derived; opting out (`!Send`) is a deliberate API statement.

### 7.5 Atomics for Counters and Flags
For a single integer or bool, `AtomicU64` / `AtomicBool` beat a `Mutex<u64>` — both in performance and in clarity.

---

## 8. Async-Specific Rules

### 8.1 Async Is Viral — Plan the Boundary
Once a function is `async`, every caller is async. Pick where async begins (typically I/O entry points) and keep core logic synchronous where possible.

### 8.2 Don't Block in Async Contexts
Use `spawn_blocking` for CPU-bound or sync-blocking work inside async code. A blocked thread starves the runtime.

### 8.3 Cancellation Is Real
Dropping a future cancels it at the next await point. Don't assume `async fn` runs to completion. For critical cleanup, use a guard or `tokio::select!` with care.

### 8.4 Bound Concurrency
`futures::stream::FuturesUnordered` with `buffer_unordered(N)` instead of spawning unbounded tasks. Backpressure matters.

---

## 9. Performance

### 9.1 Measure First
`cargo bench`, `criterion`, `flamegraph`, `perf`. Intuition about Rust performance is often wrong — the compiler is smart, allocations are slow.

### 9.2 Avoid Hidden Allocations
- `format!` allocates. `write!` into an existing buffer doesn't.
- `.collect::<Vec<_>>()` allocates. Often unnecessary — keep the iterator.
- `.to_string()` and `.to_owned()` allocate.

### 9.3 Iterator Chains Often Beat Loops
LLVM optimizes iterator pipelines extremely well. They're usually as fast as a hand loop and clearer.

### 9.4 Smallvec / Tinyvec for Stack-Friendly Vectors
When you usually have ≤ N elements but occasionally more.

### 9.5 `Box<dyn Trait>` Has a Cost
Dynamic dispatch + heap allocation. Use generics (`<T: Trait>`) when monomorphization is fine; `dyn` when you need heterogeneous collections or want to limit code bloat.

### 9.6 `#[inline]` Sparingly
The compiler inlines aggressively across crates with LTO. Add `#[inline]` for tiny cross-crate helpers; profile before adding it anywhere else.

### 9.7 `cargo build --release` Is a Different Language
Debug builds can be 10–100× slower. Never benchmark a debug build.

---

## 10. Testing

### 10.1 Unit Tests Live Next to Code
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn it_works() { /* … */ }
}
```

### 10.2 Integration Tests in `tests/`
Each file is a separate crate that uses your public API. Good for end-to-end behavior.

### 10.3 Property Tests for Algorithmic Code
`proptest` or `quickcheck`. Especially valuable for parsers, serializers, data structures.

### 10.4 Snapshot Tests for Output-Heavy Code
`insta` — saves expected output to a file, diffs on change. Great for ASTs, generated code, formatted strings.

### 10.5 Test Doubles via Traits
Define a trait for an external dependency, implement it for the real thing and for a fake. No mocking framework required.

### 10.6 `#[should_panic]` Tests Are a Code Smell
Usually means the function should return `Result`. Reserve for genuine invariant-violation tests.

---

## 11. Unsafe Code

### 11.1 Avoid It
The vast majority of Rust code never needs `unsafe`. If you reach for it, ask first: is there a safe abstraction in `std` or a crate?

### 11.2 Encapsulate Unsafe in Safe Wrappers
Every `unsafe` block must uphold documented invariants. Wrap it in a safe API so the burden is contained, not viral.

### 11.3 Document Every `// SAFETY:`
Each `unsafe { … }` deserves a comment above it stating *why* the invariants hold. No exceptions.

### 11.4 Run `miri`
`cargo +nightly miri test` catches undefined behavior in unsafe code that compiles and "works."

---

## 12. Common Anti-Patterns

| Anti-pattern | Better |
|---|---|
| `String` field that's really an enum of 4 values | An `enum` |
| `Option<T>` for a field that's *always* present after init | A typestate or builder |
| `Vec<(String, String)>` for key-value | `HashMap` or a named struct |
| Returning `Result<T, String>` from library code | A real error enum |
| Re-implementing `Drop` to "clean up" a field | Let the field's own `Drop` run |
| `pub` everything "to be safe" | Default private, re-export deliberately |
| `Arc<Mutex<Vec<…>>>` shared across many threads | A channel and a single owner |
| `match` on a stringified enum | The enum itself |
| `if let Some(_) = … { true } else { false }` | `.is_some()` |
| Catching panics to control flow | Return `Result` |
| `#[allow(dead_code)]` scattered around | Delete the dead code |

---

## 13. The Toolchain

### 13.1 Always Run
- `cargo fmt` — formatting is not a discussion.
- `cargo clippy -- -D warnings` — its lints encode community wisdom. Treat warnings as errors in CI.
- `cargo test` — including doc tests.

### 13.2 Worth Adopting
- `cargo deny` — license, advisory, and duplicate-dependency policy.
- `cargo audit` — known vulnerabilities.
- `cargo machete` / `cargo udeps` — unused dependencies.
- `cargo hack` — feature flag matrix testing.
- `cargo expand` — see macro expansion.
- `cargo asm` / `cargo-show-asm` — see generated assembly.
- `cargo bloat` — find what's making binaries large.
- `cargo-flamegraph` — visual profiling.

### 13.3 Editor / IDE
`rust-analyzer` — non-negotiable. Inlay hints, type-on-hover, structural search.

### 13.4 CI Essentials
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features`
- `cargo doc --no-deps` (catches broken doc links)
- Pinned toolchain via `rust-toolchain.toml`

### 13.5 Lints to Enable
At the crate root:
```rust
#![warn(
    clippy::pedantic,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub,
    unused_qualifications,
)]
```
Allow specific ones back as needed — don't allow categories wholesale.

---

## 14. Macros

### 14.1 Function Before `macro_rules!` Before Procedural
- Try a regular function first.
- If you need variadic or pattern-based input, `macro_rules!`.
- If you need to inspect/generate syntax (derives, attributes), procedural — but the build-time and complexity cost is real.

### 14.2 Hygiene Matters
Macros should not capture variables from the call site unless intentional. Test macros from multiple call sites.

### 14.3 Doc-Test Your Macros
A macro that works in one file may fail in another due to imports. Doc tests catch this.

---

## 15. Dependencies

### 15.1 A Dependency Is a Commitment
Every crate added is a supply-chain risk, a compile-time cost, and a maintenance signal. Audit before adding.

### 15.2 Prefer Smaller, Focused Crates
`thiserror` over a giant framework. Compose.

### 15.3 Pin Major, Float Minor
`tokio = "1"` (will accept 1.x). Lockfile pins exact. Commit `Cargo.lock` for binaries, not for libraries.

### 15.4 Feature Flags on Dependencies
`default-features = false` then opt in. Most ecosystem crates pull in more than you need by default.

---

## 16. Documentation

### 16.1 Crate-Level Doc Comment
Every crate needs `//!` at the top of `lib.rs`: what is this, why does it exist, a 10-line example.

### 16.2 Module-Level Comments Explain Why
Each module's `//!` explains its purpose. Items get `///` explaining what.

### 16.3 Examples Are the Real Documentation
Working code beats prose. Every nontrivial public item should have one.

### 16.4 Document Panics, Errors, Safety
Sections that doc-link checkers know about:
- `# Panics` — when can this panic?
- `# Errors` — what does the `Err` variant mean?
- `# Safety` — required for `unsafe fn`, lists caller obligations.

---

## 17. Idiomatic Snippets

### Default impl via `Default::default()`
```rust
#[derive(Default)]
struct Config { workers: usize, timeout_ms: u64 }
let cfg = Config { workers: 8, ..Default::default() };
```

### Iterator combinators over manual loops
```rust
let sum_of_squares: u32 = (1..=10).filter(|n| n % 2 == 0).map(|n| n * n).sum();
```

### `?` with `From` for ergonomic propagation
```rust
fn read_config() -> Result<Config, AppError> {
    let s = std::fs::read_to_string("config.toml")?;   // io::Error → AppError via From
    let cfg = toml::from_str(&s)?;                      // toml::Error → AppError via From
    Ok(cfg)
}
```

### Early return for the happy path
```rust
fn process(input: &Input) -> Result<Output, Error> {
    let Some(record) = input.record() else { return Ok(Output::empty()); };
    let validated = record.validate()?;
    Ok(validated.into())
}
```

### Strong-typed IDs
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UserId(u64);
```

### State machine as an enum
```rust
enum Job {
    Queued { submitted: Instant },
    Running { started: Instant, worker: WorkerId },
    Done { duration: Duration, output: Output },
    Failed { reason: Error },
}
```

---

## 18. A Short Checklist Before Shipping

- [ ] `cargo fmt` clean
- [ ] `cargo clippy -- -D warnings` clean
- [ ] All public items documented with examples
- [ ] No `unwrap`/`expect` outside tests/main
- [ ] Error types are real enums, not `String`
- [ ] No `pub` that shouldn't be `pub`
- [ ] Auto-traits (`Send`, `Sync`) where reasonable
- [ ] `Debug` derived on all data types
- [ ] CI runs tests, lints, and `cargo doc`
- [ ] `Cargo.toml` has `description`, `license`, `repository`
- [ ] Public API reviewed for breaking-change risk

---

*This rulebook is a default, not a law. Every rule has a context where breaking it is right — but breaking a rule should be a deliberate, justified act.*
