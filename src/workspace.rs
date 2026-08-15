//! Which crates in this tree exist only to support tests.
//!
//! The problem: `crates/foo-test/src/lib.rs` is ordinary library code by every
//! syntactic measure. It is not under a `tests/` directory, it is not named
//! `tests.rs`, and it is not behind `#[cfg(test)]` — a crate pulled in from
//! `[dev-dependencies]` is compiled exactly like any other. So `--scope
//! production` scanned it, and a battery run over a twelve-crate workspace
//! reported swallowed `env::var`s in test scaffolding as production defects,
//! twice landing them next to an unrelated fix and reading as a near miss.
//!
//! The answer is not in the source, it is in the manifests: a crate is test
//! support when the only way production code reaches it is through a
//! `[dev-dependencies]` edge. That is a graph question, so this module builds
//! the graph.
//!
//! ## The classification
//!
//! Nodes are every package found under the scan root — not just the ones a
//! `[workspace] members` list names, so several independent manifests under one
//! root (this repo's own `tests/fixtures/`) each get classified. Edges are
//! dependencies resolved *by package name*, which is what makes `uv-test =
//! { workspace = true }` resolve: the inherited entry keeps the name even
//! though the path lives in the root manifest.
//!
//! 1. **Roots** are packages nothing depends on: a binary, the crate a user
//!    installs, a standalone tool. Every workspace has at least one, because
//!    Cargo forbids cycles among normal dependencies.
//! 2. **Production** is everything reachable from a root by following *normal*
//!    and *build* dependency edges. Dev edges are not followed — that is the
//!    entire point.
//! 3. **Test support** is everything else.
//!
//! Step 3 is transitive by construction, which the naming rule this replaced
//! could not be: a `foo-test-helpers` that only `foo-test` depends on is
//! test support too, even though its own edge is a normal one and its name says
//! nothing.
//!
//! ## When the graph declines to answer
//!
//! Two guards, because the failure mode is silently dropping files from a
//! production scan:
//!
//! * **No roots.** Only reachable when dev edges form a cycle (Cargo permits
//!   that; `A` dev-depends on `B` and `B` on `A`). With no root there is
//!   nothing to be production *from*, and the honest answer is not "everything
//!   is test support".
//! * **Everything demoted.** A tree with no production code in it is a
//!   misreading, not a finding.
//!
//! In both cases the graph classifies nothing and [`Workspace::verdict`]
//! returns [`Verdict::Unknown`], which is the caller's cue to fall back to the
//! name heuristic. A file with no manifest above it answers `Unknown` too.
//!
//! So does a **root**, which is the subtle one. A root is production by
//! default, but only because nothing depends on it — an inference from absence.
//! Reporting that as `Production` would be a claim the graph cannot support,
//! and it would break the case that matters: `-r crates/foo-test` scans a tree
//! containing exactly one manifest, which is therefore a root, and answering
//! "production" there would un-classify a harness that the same run catches
//! from one directory up. Only an incoming *normal* edge is evidence, and only
//! evidence outranks the crate's name.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// What the dependency graph concluded about one crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Some production package depends on this one *normally*. Positive
    /// evidence, and strong enough to outrank the crate's name.
    Production,
    /// Not reachable from any root without crossing a dev edge — scaffolding.
    TestSupport,
    /// The graph has no evidence either way. Three ways to get here, and the
    /// caller must resolve all of them by other means rather than reading this
    /// as `Production`:
    ///
    /// * no manifest above the file;
    /// * a graph a guard rejected (see the module docs);
    /// * **a root nothing depends on.** A root is production by default, but
    ///   that is an inference from absence, not evidence. `-r crates/foo-test`
    ///   scans a tree holding one manifest that nothing can depend on, and
    ///   calling that production would silently un-classify a harness the same
    ///   run would have caught from one directory up.
    Unknown,
}

/// One package: where it lives and what it depends on.
#[derive(Debug, Default)]
struct Package {
    /// Every directory declaring this package name. A `Vec` because two
    /// unrelated manifests under one scan root may share a name, and both
    /// deserve the verdict rather than whichever was walked first.
    dirs: Vec<PathBuf>,
    /// `[dependencies]` + `[build-dependencies]`, including the `[target.…]`
    /// forms. Followed when deciding what production reaches.
    normal: HashSet<String>,
    /// `[dev-dependencies]`, including the `[target.…]` form. Never followed.
    dev: HashSet<String>,
}

pub struct Workspace {
    /// Manifest directory → the package declared there.
    by_dir: HashMap<PathBuf, String>,
    /// Package names the graph demoted.
    test_support: HashSet<String>,
    /// Packages nothing in the tree depends on. Production by default, but by
    /// absence of evidence rather than presence, so they answer `Unknown`.
    roots: HashSet<String>,
    /// False when a guard fired: the graph has no opinion about anything.
    usable: bool,
}

impl Workspace {
    /// An empty graph that answers [`Verdict::Unknown`] for everything.
    pub fn unknown() -> Self {
        Workspace {
            by_dir: HashMap::new(),
            test_support: HashSet::new(),
            roots: HashSet::new(),
            usable: false,
        }
    }

    /// Read every `Cargo.toml` under `root` and classify.
    ///
    /// Walker errors and unparseable manifests are skipped rather than fatal:
    /// this decides a *scope*, and a tree with one broken manifest in it must
    /// still be scannable. The cost of a skip is a crate the graph cannot
    /// classify, which degrades to the name heuristic.
    pub fn discover(root: &Path, excludes: &[String]) -> Self {
        let mut packages: HashMap<String, Package> = HashMap::new();
        let Ok(walk) = crate::parse::build_walker(root, excludes) else {
            return Self::unknown();
        };
        for entry in walk.flatten() {
            if entry.file_name() != "Cargo.toml" {
                continue;
            }
            let path = entry.path();
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let Some(m) = Manifest::parse(&text) else {
                continue;
            };
            let Some(name) = m.name else {
                // A workspace-only root manifest declares no package. Its
                // `[workspace.dependencies]` are declarations, not edges.
                continue;
            };
            let e = packages.entry(name).or_default();
            if let Some(dir) = path.parent() {
                e.dirs.push(dir.to_path_buf());
            }
            e.normal.extend(m.normal);
            e.dev.extend(m.dev);
        }
        Self::classify(packages)
    }

    fn classify(packages: HashMap<String, Package>) -> Self {
        let mut by_dir = HashMap::new();
        for (name, p) in &packages {
            for d in &p.dirs {
                by_dir.insert(d.clone(), name.clone());
            }
        }
        // Only edges that land on a package we actually found count. A
        // dependency on `serde` says nothing about the workspace's shape.
        let has_incoming: HashSet<&str> = packages
            .values()
            .flat_map(|p| p.normal.iter().chain(p.dev.iter()))
            .map(String::as_str)
            .filter(|d| packages.contains_key(*d))
            .collect();

        let roots: Vec<&str> = packages
            .keys()
            .map(String::as_str)
            .filter(|n| !has_incoming.contains(n))
            .collect();
        if roots.is_empty() {
            return Self::unknown();
        }

        let mut production: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = roots.iter().map(|s| s.to_string()).collect();
        while let Some(n) = queue.pop_front() {
            if !production.insert(n.clone()) {
                continue;
            }
            let Some(p) = packages.get(&n) else { continue };
            for d in &p.normal {
                if packages.contains_key(d) && !production.contains(d) {
                    queue.push_back(d.clone());
                }
            }
        }

        let test_support: HashSet<String> = packages
            .keys()
            .filter(|n| !production.contains(*n))
            .cloned()
            .collect();
        if test_support.len() == packages.len() {
            return Self::unknown();
        }
        Workspace {
            by_dir,
            test_support,
            roots: roots.iter().map(|s| s.to_string()).collect(),
            usable: true,
        }
    }

    /// The graph's verdict for the package containing `file`.
    ///
    /// The *nearest* manifest above the file wins, so a member inside another
    /// member's directory is read as itself.
    pub fn verdict(&self, file: &Path) -> Verdict {
        if !self.usable {
            return Verdict::Unknown;
        }
        let mut dir = file.parent();
        while let Some(d) = dir {
            if let Some(name) = self.by_dir.get(d) {
                return if self.test_support.contains(name) {
                    Verdict::TestSupport
                } else if self.roots.contains(name) {
                    // Production by default, but nothing depends on it, so the
                    // graph has seen no evidence. Let the caller's name rule
                    // have the question.
                    Verdict::Unknown
                } else {
                    Verdict::Production
                };
            }
            dir = d.parent();
        }
        Verdict::Unknown
    }

    /// The demoted package names, sorted — for the note that has to say which
    /// crates a production scan left out.
    pub fn test_support_crates(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.test_support.iter().map(String::as_str).collect();
        v.sort_unstable();
        v
    }
}

/// The `[package] name` of one manifest's text, if it declares a package.
///
/// Exposed because the name-based fallback in `parse` needs exactly this and
/// must not answer it a second, differing way.
pub fn package_name_of(manifest: &str) -> Option<String> {
    Manifest::parse(manifest)?.name
}

/// The three things this module needs out of a manifest.
#[derive(Debug, Default)]
struct Manifest {
    name: Option<String>,
    normal: HashSet<String>,
    dev: HashSet<String>,
}

impl Manifest {
    /// Parse with a real TOML parser rather than a line scanner.
    ///
    /// The line-scanning version of this got the motivating case right and
    /// would have got `[dependencies.foo]` sub-tables, `foo.workspace = true`
    /// dotted keys, multi-line inline tables, and a `#` inside a literal string
    /// wrong — each of them silently, and each of them in the direction of
    /// dropping files from a production scan. `toml_edit` is the parser Cargo
    /// itself uses, and it is pulled in without `serde` or `display`.
    fn parse(text: &str) -> Option<Self> {
        let doc = toml_edit::Document::parse(text).ok()?;
        let mut m = Manifest {
            name: doc
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string),
            ..Default::default()
        };
        collect_deps(doc.as_table(), &mut m);
        // `[target.'cfg(unix)'.dependencies]` and its dev/build siblings. One
        // level down, keyed by the cfg expression.
        if let Some(targets) = doc.get("target").and_then(|t| t.as_table_like()) {
            for (_, cfg) in targets.iter() {
                if let Some(t) = cfg.as_table_like() {
                    collect_deps_from(t, &mut m);
                }
            }
        }
        Some(m)
    }
}

fn collect_deps(t: &toml_edit::Table, m: &mut Manifest) {
    collect_deps_from(t, m);
}

/// Pull `[dependencies]`, `[build-dependencies]` and `[dev-dependencies]` out
/// of one table, wherever in the document that table sits.
fn collect_deps_from(t: &dyn toml_edit::TableLike, m: &mut Manifest) {
    for (key, dev) in [
        ("dependencies", false),
        ("build-dependencies", false),
        ("dev-dependencies", true),
    ] {
        let Some(table) = t.get(key).and_then(|d| d.as_table_like()) else {
            continue;
        };
        for (name, spec) in table.iter() {
            // `foo = { package = "real-name" }` — the key is the rename the
            // dependent uses, and the edge belongs to the package it renames.
            let resolved = spec
                .as_table_like()
                .and_then(|s| s.get("package"))
                .and_then(|p| p.as_str())
                .unwrap_or(name)
                .to_string();
            if dev {
                m.dev.insert(resolved);
            } else {
                m.normal.insert(resolved);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, normal: &[&str], dev: &[&str]) -> (String, Package) {
        (
            name.to_string(),
            Package {
                dirs: vec![PathBuf::from(name)],
                normal: normal.iter().map(|s| s.to_string()).collect(),
                dev: dev.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    fn classify(pkgs: Vec<(String, Package)>) -> Workspace {
        Workspace::classify(pkgs.into_iter().collect())
    }

    /// The motivating shape: a binary, its libraries, and a harness that only
    /// the libraries' dev-dependencies reach.
    #[test]
    fn a_dev_only_dependency_is_test_support() {
        let ws = classify(vec![
            pkg("app", &["core"], &[]),
            pkg("core", &[], &["harness"]),
            pkg("harness", &[], &[]),
        ]);
        assert_eq!(ws.test_support_crates(), vec!["harness"]);
    }

    /// What the name heuristic could not do. `helpers` is a *normal*
    /// dependency and its name says nothing, but the only crate that reaches it
    /// is itself scaffolding.
    #[test]
    fn test_support_is_transitive() {
        let ws = classify(vec![
            pkg("app", &["core"], &["harness"]),
            pkg("core", &[], &[]),
            pkg("harness", &["helpers"], &[]),
            pkg("helpers", &[], &[]),
        ]);
        assert_eq!(ws.test_support_crates(), vec!["harness", "helpers"]);
    }

    /// A crate that production code also depends on normally is production,
    /// whatever else dev-depends on it — and whatever it is called.
    #[test]
    fn a_normal_edge_from_production_wins() {
        let ws = classify(vec![
            pkg("app", &["fixture-test-utils"], &["harness"]),
            pkg("fixture-test-utils", &[], &[]),
            pkg("harness", &[], &[]),
        ]);
        assert_eq!(ws.test_support_crates(), vec!["harness"]);
    }

    /// Dev-dependency cycles are legal Cargo and leave the graph rootless.
    /// Answering "all of it is test support" there would silently empty a
    /// production scan.
    #[test]
    fn a_rootless_graph_declines_to_answer() {
        let ws = classify(vec![pkg("a", &[], &["b"]), pkg("b", &[], &["a"])]);
        assert_eq!(ws.verdict(Path::new("a/src/lib.rs")), Verdict::Unknown);
        assert!(ws.test_support_crates().is_empty());
    }

    /// A root is production by *absence* of a dependent, which is not evidence.
    /// This is the `-r crates/uv-test` case: pointed straight at a harness,
    /// with no manifest that dev-depends on it anywhere in the tree. Answering
    /// `Production` here would silently un-classify it.
    #[test]
    fn a_root_nothing_depends_on_is_not_evidence() {
        let ws = classify(vec![pkg("solo", &[], &[])]);
        assert_eq!(ws.verdict(Path::new("solo/src/lib.rs")), Verdict::Unknown);
    }

    /// An incoming *normal* edge is evidence, and it is what outranks a name.
    #[test]
    fn a_normally_depended_on_crate_is_positively_production() {
        let ws = classify(vec![pkg("app", &["core-tests"], &[]), pkg("core-tests", &[], &[])]);
        assert_eq!(
            ws.verdict(Path::new("core-tests/src/lib.rs")),
            Verdict::Production
        );
        assert_eq!(ws.verdict(Path::new("app/src/main.rs")), Verdict::Unknown);
    }

    #[test]
    fn a_file_with_no_manifest_above_it_is_unknown() {
        let ws = classify(vec![pkg("app", &[], &["harness"]), pkg("harness", &[], &[])]);
        assert_eq!(ws.verdict(Path::new("elsewhere/src/lib.rs")), Verdict::Unknown);
    }

    // ── manifest reading ──────────────────────────────────────────────────

    fn parsed(text: &str) -> Manifest {
        Manifest::parse(text).expect("parse")
    }

    #[test]
    fn dependencies_are_read_in_every_spelling_cargo_allows() {
        let m = parsed(
            r#"
            [package]
            name = "app"

            [dependencies]
            inline = { version = "1", features = ["x"] }
            plain = "1.0"
            dotted.workspace = true

            [dependencies.subtable]
            version = "2"

            [build-dependencies]
            builder = "1"

            [dev-dependencies]
            harness = { workspace = true }

            [target.'cfg(unix)'.dependencies]
            unixdep = "1"

            [target.'cfg(windows)'.dev-dependencies]
            windev = "1"
            "#,
        );
        assert_eq!(m.name.as_deref(), Some("app"));
        for d in ["inline", "plain", "dotted", "subtable", "builder", "unixdep"] {
            assert!(m.normal.contains(d), "missing normal dep {d}: {m:?}");
        }
        for d in ["harness", "windev"] {
            assert!(m.dev.contains(d), "missing dev dep {d}: {m:?}");
        }
        // A build-dependency is production: it runs, and its failures are the
        // build's failures.
        assert!(!m.dev.contains("builder"));
    }

    /// The edge belongs to the package being renamed, not to the name the
    /// dependent happens to use for it.
    #[test]
    fn a_renamed_dependency_resolves_to_its_real_package() {
        let m = parsed(
            r#"
            [package]
            name = "app"
            [dev-dependencies]
            h = { package = "real-harness", version = "1" }
            "#,
        );
        assert!(m.dev.contains("real-harness"), "{m:?}");
        assert!(!m.dev.contains("h"), "{m:?}");
    }

    /// A workspace-only root manifest declares no package, so it is not a node
    /// and its `[workspace.dependencies]` are declarations rather than edges.
    #[test]
    fn a_workspace_root_manifest_is_not_a_package() {
        let m = parsed(
            r#"
            [workspace]
            members = ["a", "b"]
            [workspace.dependencies]
            harness = { path = "b" }
            "#,
        );
        assert!(m.name.is_none());
        assert!(m.normal.is_empty() && m.dev.is_empty(), "{m:?}");
    }

    /// The shapes a line scanner got wrong, asserted so nobody is tempted back.
    #[test]
    fn comments_and_literal_strings_do_not_confuse_the_reader() {
        let m = parsed(
            r#"
            [package]
            name = "app" # [dependencies] is not a section here
            description = 'a # b [dev-dependencies]'

            [dependencies]
            real = "1"
            "#,
        );
        assert_eq!(m.name.as_deref(), Some("app"));
        assert_eq!(m.normal.len(), 1, "{m:?}");
        assert!(m.normal.contains("real"));
    }
}
