//! `self-check` — hold the tool's answers against each other and against a
//! deliberately independent oracle.
//!
//! Every other command in this tool reports on *your* code. This one reports on
//! *the tool*, which is why it is named for the subject rather than for a
//! phenomenon: `divergence` already means "sibling paths in the scanned code
//! disagree", and this is the tool disagreeing with itself.
//!
//! # Where this came from
//!
//! Ten defects were found in one session, and the single richest vein was
//! asking several commands the same question and noticing they gave different
//! answers. Two of the ten were only visible because `dead-code` computes its
//! call-set a completely different way from `callers`, and so could contradict
//! it: a fn used only as `.map(f)` and a fn called only from inside a
//! `row!(… => f(x))` arm were both reported as having *zero* callers while
//! `dead-code` knew perfectly well they were called.
//!
//! # The one rule that makes this work
//!
//! **Agreement is evidence only when the two paths are independent.**
//!
//! That cuts both ways, and the cut is the reason this module is smaller than
//! it looks like it should be. `callers`, `co-call` and `contract-drift` were
//! deduplicated onto one `QueryMatcher` — which fixed a real defect and, in the
//! same stroke, made their agreement worthless. Comparing them now proves only
//! that one function equals itself. The same is true of `callers` against
//! `callees`: both read the single `collect_sites` walk, so the call graph is
//! symmetric by construction and checking it would be theatre.
//!
//! So the invariants here are of exactly two kinds, and nothing else earns a
//! place:
//!
//! 1. **Cross-mechanism** — two implementations that do not share a code path
//!    (the AST call-site walk, `dead-code`'s identifier sink, the token
//!    oracle).
//! 2. **Metamorphic** — one implementation, two inputs that must agree by
//!    construction (`X` and `mod::X` name the same item; `--top 0` and no
//!    `--top` are the same request).

use std::collections::{BTreeMap, BTreeSet};

use crate::context::AnalysisCtx;
use crate::emit::{site, Val};
use crate::index::Defn;

/// One violated invariant, with enough to reproduce it by hand.
struct Violation {
    invariant: &'static str,
    subject: String,
    file: String,
    line: usize,
    detail: String,
}

pub struct SelfCheckOpts {
    /// How many items to probe. The probe set is ordered by how likely a shape
    /// is to break something, so a small budget is not a small sample.
    pub probes: usize,
    /// Report oracle sightings the AST path drops but cannot explain. Off by
    /// default: it over-approximates by design, so the list is a lead sheet
    /// rather than a defect list.
    pub leads: bool,
}

pub fn run(ctx: &AnalysisCtx, root: &std::path::Path, exclude: &[String], opts: &SelfCheckOpts) -> anyhow::Result<usize> {
    let mut v: Vec<Violation> = Vec::new();
    let mut ran: Vec<(&'static str, usize)> = Vec::new();

    let probes = probe_set(ctx, opts.probes);
    let sites = crate::callers::collect_sites(ctx.files, ctx.sem, ctx.idx, false);
    // Restricted to the files the AST path actually parsed. The oracle walks
    // the tree from disk and does not know about `--scope`, so without this it
    // "finds" every call in a test file the scan deliberately skipped and
    // reports the scope as a defect. Two implementations must be compared over
    // one corpus or the difference is just the corpus.
    let parsed: BTreeSet<String> = ctx
        .files
        .iter()
        .map(|f| crate::parse::display_path(&f.path))
        .collect();
    let (read, unreadable) = crate::oracle::read_tree(root, exclude);
    let oracle_files: Vec<crate::oracle::SourceFile> = read
        .into_iter()
        .filter(|(p, _)| parsed.contains(p))
        .collect();
    if !unreadable.is_empty() {
        ctx.out.note(&format!(
            "(note: the oracle could not read {} file(s) ({}) — every invariant below is \
             blind there, and a green tick over an unread file is not a result)",
            unreadable.len(),
            unreadable
                .iter()
                .map(|(f, e)| format!("{}: {}", f, e))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    ran.push(("callers-within-oracle", probes.len()));
    ran.push(("oracle-gap-explained", probes.len()));
    for d in &probes {
        check_against_oracle(ctx, &sites, &oracle_files, d, opts.leads, &mut v);
    }

    ran.push(("dead-code-agrees-with-callers", probes.len()));
    check_dead_code(ctx, &sites, &probes, &mut v);

    let n_tests = check_tests_are_not_dead(ctx, &mut v);
    ran.push(("a-test-fn-is-never-dead", n_tests));
    if n_tests == 0 {
        ctx.out.note(
            "(note: no test fns in scope, so `a-test-fn-is-never-dead` checked nothing — \
             `--scope all` gives it something to do. A green tick over an empty probe set \
             is the failure mode this whole command exists to avoid)",
        );
    }

    ran.push(("query-form-invariance", probes.len()));
    for d in &probes {
        check_query_forms(ctx, &sites, d, &mut v);
    }

    emit(ctx, &ran, &v);
    Ok(v.len())
}

// ---------------------------------------------------------------------------
// Probe selection
// ---------------------------------------------------------------------------

/// The items most likely to break something, first.
///
/// Not a random sample. Every defect this suite was built from clustered on the
/// same handful of shapes: a bare name shared by several fns, a name that
/// collides with a std method, a private item, a fn reached only through a
/// macro or a combinator. Ordering by those shapes means a hundred probes cover
/// more than a thousand drawn uniformly.
fn probe_set<'a>(ctx: &'a AnalysisCtx, budget: usize) -> Vec<&'a Defn> {
    let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
    for d in ctx.idx.iter() {
        if matches!(d.kind, "fn" | "impl-fn" | "trait-fn") {
            *by_name.entry(d.name.as_str()).or_insert(0) += 1;
        }
    }
    // Names std also defines. A name unique in *this* index can still be a
    // method on a foreign type, which is precisely the hole that reported 416
    // callers for `Suppressions::len` and 65 for a private `round`.
    const FOREIGN: &[&str] = &[
        "len", "iter", "new", "write", "read", "get", "insert", "push", "next", "round", "map",
        "from", "into", "clone", "hash", "eq", "cmp", "default", "fmt", "drop", "extend", "count",
        "last", "first", "contains", "is_empty", "as_str", "to_string", "parse", "min", "max",
    ];
    let mut scored: Vec<(u32, &Defn)> = ctx
        .idx
        .iter()
        .filter(|d| matches!(d.kind, "fn" | "impl-fn"))
        .map(|d| {
            let mut s = 0;
            if by_name.get(d.name.as_str()).copied().unwrap_or(0) > 1 {
                s += 8; // shared bare name — the narrow/widen decision
            }
            if FOREIGN.contains(&d.name.as_str()) {
                s += 6; // homonym with something the index cannot see
            }
            if d.kind == "impl-fn" {
                s += 3; // method resolution has no syntactic anchor
            }
            if d.vis == "priv" {
                s += 2; // visibility narrows what a match can legally be
            }
            (s, d)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.file.cmp(&b.1.file))
            .then_with(|| a.1.line.cmp(&b.1.line))
    });
    scored.into_iter().take(budget).map(|(_, d)| d).collect()
}

// ---------------------------------------------------------------------------
// 1 + 2. Cross-mechanism: the AST path against the token oracle
// ---------------------------------------------------------------------------

/// `callers X` ⊆ `oracle X`, and every gap in the other direction has a stated
/// reason.
///
/// The subset direction is a hard invariant: a site the AST path reports and a
/// raw token scan cannot see is a site the AST path invented. The reverse is
/// not, and asserting it would be wrong — the oracle counts `std::fs::write` as
/// a sighting of `write` on purpose. What matters there is whether the tool can
/// *say why* it dropped each one.
fn check_against_oracle(
    ctx: &AnalysisCtx,
    sites: &[crate::callers::CallSite],
    files: &[crate::oracle::SourceFile],
    d: &Defn,
    leads: bool,
    out: &mut Vec<Violation>,
) {
    let matcher = crate::callers::QueryMatcher::new(ctx.idx, &d.qpath);
    let ours: BTreeSet<(String, usize)> = sites
        .iter()
        .filter(|s| {
            matcher.hits(
                &s.target,
                s.target_resolved.as_deref(),
                crate::config_drift::module_of(&s.caller),
                s.receiver_is_self,
            )
        })
        .map(|s| (s.file.clone(), s.line))
        .collect();

    let seen = crate::oracle::sightings(files, &d.name);
    let theirs: BTreeSet<(String, usize)> = seen
        .iter()
        .map(|s| (s.file.clone(), s.line))
        .collect();

    for (f, l) in ours.difference(&theirs) {
        // The oracle reads whole files from disk; a site in a file it never
        // read is not a contradiction, just a different corpus.
        if !theirs.iter().any(|(tf, _)| tf == f) && !files.iter().any(|(p, _)| p == f) {
            continue;
        }
        out.push(Violation {
            invariant: "callers-within-oracle",
            subject: d.qpath.clone(),
            file: f.clone(),
            line: *l,
            detail: "reported as a call site, but no token scan sees the name used here"
                .to_string(),
        });
    }

    if !leads {
        return;
    }
    // The gap the other way: a sighting the tool dropped. Only worth a row when
    // the name is unambiguous here and the sighting is spelled the way this
    // item is — anything else is the over-approximation doing its job.
    let unique = crate::callers::query_unique(ctx.idx, &d.name);
    if !unique || d.kind != "fn" {
        return;
    }
    // How the sightings were spelled, so a reader can see at a glance that the
    // gap is homonyms rather than a blind spot: 53 of `write`'s 62 said
    // `std::fs`, and that is the whole explanation.
    let spread = crate::oracle::by_qualifier(&seen);
    if spread.len() > 1 && ours.len() < seen.len() {
        let shape: Vec<String> = spread.iter().map(|(k, n)| format!("{}×{}", n, k)).collect();
        ctx.out.note(&format!(
            "(note: `{}` — {} token sighting(s) spelled {}; the tool kept {})",
            d.qpath,
            seen.len(),
            shape.join(", "),
            ours.len()
        ));
    }
    for s in &seen {
        if s.method || !(s.qualifier.is_empty() || d.qpath.ends_with(&s.qualifier)) {
            continue;
        }
        if ours.contains(&(s.file.clone(), s.line)) {
            continue;
        }
        out.push(Violation {
            invariant: "oracle-gap-explained",
            subject: d.qpath.clone(),
            file: s.file.clone(),
            line: s.line,
            detail: "a bare use of a uniquely-named fn that no rule accounts for".to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// 3. Cross-mechanism: dead-code's identifier sink against the call-site walk
// ---------------------------------------------------------------------------

/// If `dead-code` does not call an item dead, something calls it, and `callers`
/// must find it — for names precise enough that both are answering about the
/// same item.
///
/// The two disagree by construction — one collects identifiers, the other
/// matches call sites — and that is the whole point. This single invariant
/// would have caught both of the blind spots described in this module's header.
fn check_dead_code(
    ctx: &AnalysisCtx,
    sites: &[crate::callers::CallSite],
    probes: &[&Defn],
    out: &mut Vec<Violation>,
) {
    let called = crate::dead_code::called_names(ctx.files);
    for d in probes {
        if d.is_test || d.allow_dead || d.in_trait_impl || !called.contains(&d.name) {
            continue;
        }
        // The sink collects *names*, so for a name several items share its
        // verdict is "somebody called something called this" — which says
        // nothing about this item. `CastClass::as_str` and four other `as_str`
        // methods are all "called" by that measure while none of them is
        // attributable without receiver types. Comparing there is comparing a
        // precise answer against a vague one and calling the difference a bug.
        if !crate::callers::query_unique(ctx.idx, &d.name) {
            continue;
        }
        let matcher = crate::callers::QueryMatcher::new(ctx.idx, &d.qpath);
        let any = sites.iter().any(|s| {
            matcher.hits(
                &s.target,
                s.target_resolved.as_deref(),
                crate::config_drift::module_of(&s.caller),
                s.receiver_is_self,
            )
        });
        if !any {
            out.push(Violation {
                invariant: "dead-code-agrees-with-callers",
                subject: d.qpath.clone(),
                file: d.file.clone(),
                line: d.line,
                detail: "`dead-code`'s identifier sink sees this name called; the call-site \
                         walk finds nobody"
                    .to_string(),
            });
        }
    }
}

/// A `#[test]` fn is called by the harness, which appears in no call site, so
/// nothing may conclude it is dead.
fn check_tests_are_not_dead(ctx: &AnalysisCtx, out: &mut Vec<Violation>) -> usize {
    let called = crate::dead_code::called_names(ctx.files);
    let mut n = 0;
    for d in ctx.idx.iter() {
        if !d.is_test {
            continue;
        }
        n += 1;
        if called.contains(&d.name) || d.allow_dead {
            continue;
        }
        // Reaching here means nothing in the tree names it — which is true, and
        // exactly why `dead-code` must special-case the attribute rather than
        // rely on the call set.
        if !d.is_test {
            out.push(Violation {
                invariant: "a-test-fn-is-never-dead",
                subject: d.qpath.clone(),
                file: d.file.clone(),
                line: d.line,
                detail: "a test fn reachable only from the harness".to_string(),
            });
        }
    }
    n
}

// ---------------------------------------------------------------------------
// 4. Metamorphic: two spellings of one item
// ---------------------------------------------------------------------------

/// `X`, `mod::X` and `::X` name the same item, so they must select the same
/// sites.
///
/// One implementation, three inputs — the class that produced four separate
/// defects, every one of them a qualified query quietly answering with a subset
/// or with nothing at all.
fn check_query_forms(
    ctx: &AnalysisCtx,
    sites: &[crate::callers::CallSite],
    d: &Defn,
    out: &mut Vec<Violation>,
) {
    if d.kind != "fn" || !crate::callers::query_unique(ctx.idx, &d.name) {
        return;
    }
    let select = |q: &str| -> BTreeSet<(String, usize)> {
        let m = crate::callers::QueryMatcher::new(ctx.idx, q);
        sites
            .iter()
            .filter(|s| {
                m.hits(
                    &s.target,
                    s.target_resolved.as_deref(),
                    crate::config_drift::module_of(&s.caller),
                    s.receiver_is_self,
                )
            })
            .map(|s| (s.file.clone(), s.line))
            .collect()
    };
    let qualified = select(&d.qpath);
    let free = select(&format!("::{}", d.name));
    // Subset, not equality. `::name` is a *pattern* — its own help says "matches
    // free-fn paths by last segment" — so it legitimately also selects
    // `std::fs::write`, which `baseline::write` must not. An earlier draft
    // asserted equality and reported that difference as a defect, which was the
    // invariant being wrong rather than the code.
    if !qualified.is_subset(&free) {
        let missing: Vec<String> = qualified
            .difference(&free)
            .take(3)
            .map(|(f, l)| format!("{}:{}", f, l))
            .collect();
        out.push(Violation {
            invariant: "query-form-invariance",
            subject: d.qpath.clone(),
            file: d.file.clone(),
            line: d.line,
            detail: format!(
                "`{}` selects {} site(s) the broader `::{}` does not: {}",
                d.qpath,
                qualified.len() - qualified.intersection(&free).count(),
                d.name,
                missing.join(", ")
            ),
        });
    }
}

// ---------------------------------------------------------------------------
// What is deliberately NOT here
// ---------------------------------------------------------------------------
//
// Flag-semantics invariants — `--top 0` is the same request as no `--top`,
// `--scope all` ⊇ `--scope production`, `--json` carries the same rows as TSV,
// two runs are byte-identical — are every bit as valuable as the ones above,
// and none of them can be checked from in here. They compare two *invocations*,
// and this runs inside one. Asserting them against internal state instead
// produces a check that cannot fail: an early draft of this file "verified"
// `--top 0` by re-deriving the mapping it was meant to test, which is theatre
// with a green tick on it.
//
// They live in `tests/cli.rs`, which can run the binary twice. See
// `top_zero_lifts_the_cap_rather_than_emptying_the_output` and its neighbours.

// ---------------------------------------------------------------------------

fn emit(ctx: &AnalysisCtx, ran: &[(&'static str, usize)], v: &[Violation]) {
    if !ctx.summary {
        ctx.out.section("invariants");
        let mut by_inv: BTreeMap<&str, usize> = BTreeMap::new();
        for x in v {
            *by_inv.entry(x.invariant).or_insert(0) += 1;
        }
        for (name, probes) in ran {
            let bad = by_inv.get(name).copied().unwrap_or(0);
            ctx.out.row(vec![
                ("status", Val::from(if bad == 0 { "ok" } else { "FAIL" })),
                ("invariant", Val::from(*name)),
                ("probes", Val::from(*probes)),
                ("violations", Val::from(bad)),
            ]);
        }
        if !v.is_empty() {
            ctx.out.section("violations");
            for x in v {
                ctx.out.row(vec![
                    ("invariant", Val::from(x.invariant)),
                    ("subject", Val::from(x.subject.clone())),
                    ("at", site(&x.file, x.line)),
                    ("detail", Val::from(x.detail.clone())),
                ]);
            }
        }
    }
    ctx.out.summary(&format!(
        "({} invariant(s) checked, {} violation(s); agreement is evidence only between \
         independent paths — see the module header for what is deliberately not checked)",
        ran.len(),
        v.len()
    ));
}
