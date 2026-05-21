# UnRuster — collaboration notes for Claude

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
