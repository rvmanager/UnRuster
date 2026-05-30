# UnRuster — collaboration notes for Claude

## Lint workflow

After every compile (`cargo build`, `cargo build --release`, or `cargo check`),
**run `cargo clippy` and clean up any issues it reports** before considering the
work done.

- Treat clippy findings as part of "does it build" — a green `cargo build` with
  outstanding clippy warnings is not finished work.
- Prefer `cargo clippy --fix --allow-dirty --allow-staged` for the mechanical
  suggestions, then resolve the rest by hand.
- This applies to pre-existing warnings you encounter too: if you touched the
  area or ran a build, leave clippy clean. If a lint is a deliberate false
  positive, silence it narrowly with a scoped `#[allow(...)]` and a one-line
  comment saying why — don't blanket-allow at the crate root.
- Goal state: `cargo clippy` reports zero warnings.

## Release build workflow

When the user asks for a release build (`cargo build --release`, "build the
release binary", "make a new release binary", etc.), **bump the patch version
in `Cargo.toml` before running the build**.

- `Cargo.toml`'s `version` field is `MAJOR.MINOR.PATCH`. Increment only the
  third digit (`PATCH`), e.g. `0.1.0` → `0.1.1` → `0.1.2`.
- Bump first, then run `cargo build --release`.
- Major and minor digits are not auto-bumped — the user changes those by hand
  when they signal a feature/breaking-change milestone.
- After the build, confirm the new version to the user (`unruster --version`
  reads it from `Cargo.toml` via clap's derive macro, so it surfaces
  automatically).

Skip the bump only when the user explicitly says so (e.g. "rebuild without
bumping the version", "don't bump", or when running an unrelated dev build
with `cargo build` / `cargo build --release` purely for testing during the
same session — but when in doubt, bump).
