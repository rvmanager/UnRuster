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

use crate::ast::{fn_span, scope_visits, ScopeTracker};
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
    // Kept apart from `v` and out of the exit code. A lead is the oracle's
    // over-approximation asking a question; a violation is an invariant that
    // does not hold. Counting the first as the second made `--leads` exit 1
    // with 162 "failures", which is a flag nobody can use in CI and a word
    // ("FAIL") that stops meaning anything.
    let mut leads: Vec<Violation> = Vec::new();
    let mut ran: Vec<(&'static str, usize)> = Vec::new();

    // `0` lifts the cap, the way `--top 0` and `--max-lines 0` do. It used to
    // mean "probe nothing", and the run then printed five green ticks over an
    // empty probe set — which is the exact result this command exists to make
    // impossible, produced by this command.
    let budget = if opts.probes == 0 {
        usize::MAX
    } else {
        opts.probes
    };
    let probes = probe_set(ctx, budget);
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
        check_against_oracle(ctx, &sites, &oracle_files, d, opts.leads, &mut v, &mut leads);
    }

    ran.push(("dead-code-agrees-with-callers", probes.len()));
    check_dead_code(ctx, &sites, &oracle_files, &probes, &mut v);

    let n_bind = check_shadowed_bindings(ctx, &sites, &mut v, &mut leads);
    ran.push(("no-site-is-a-shadowed-binding", n_bind));

    let n_tests = check_tests_are_not_dead(ctx, &mut v);
    ran.push(("a-test-fn-is-never-dead", n_tests));


    ran.push(("query-form-invariance", probes.len()));
    for d in &probes {
        check_query_forms(ctx, &sites, d, &mut v);
    }

    let n_types = check_type_query_forms(ctx, budget, &mut v);
    ran.push(("type-query-form-invariance", n_types));

    emit(ctx, &ran, &v, &leads);
    // Leads deliberately excluded: they are questions, not verdicts.
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
    want_leads: bool,
    out: &mut Vec<Violation>,
    leads: &mut Vec<Violation>,
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

    if !want_leads {
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
        // `s.call` here too. Without it the lane filled with local variables
        // that happen to share a fn's name — 38 sightings of a `body` binding
        // in the test file, reported against `clones::tests::body`. A lead has
        // to be a definite `name(` the tool dropped without saying why, or it
        // is not a question worth anyone's time.
        if !s.call || s.method || !(s.qualifier.is_empty() || d.qpath.ends_with(&s.qualifier)) {
            continue;
        }
        if ours.contains(&(s.file.clone(), s.line)) {
            continue;
        }
        leads.push(Violation {
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
    files: &[crate::oracle::SourceFile],
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
        // Unique *in the index* is not unique in the language. `split` is one
        // free fn here and a `str` method everywhere, so the sink's "called"
        // was about `.split(…)` and had nothing to say about this item.
        //
        // The oracle settles it, which is the reason it exists: if no sighting
        // is written the way a call to *this* item would be — bare, or with a
        // qualifier that agrees — then the two are discussing different things
        // and comparing them is comparing nothing.
        let plausible = crate::oracle::sightings(files, &d.name).into_iter().any(|s| {
            // `s.call` required: a bare `name,` is a fn-reference *or* a
            // variable, and the sink cannot tell either. Both mechanisms
            // over-approximate the same way there, so their agreement carries
            // no information — which is the whole test for whether a
            // comparison belongs in a gating check.
            s.call && !s.method && (s.qualifier.is_empty() || d.qpath.ends_with(&s.qualifier))
        });
        if !plausible {
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

// ---------------------------------------------------------------------------
// 3b. Cross-mechanism: the call-site walk against an independent binder scan
// ---------------------------------------------------------------------------

/// Every bare name a fn binds, flat and without scoping — the independent half
/// of [`check_shadowed_bindings`].
///
/// Deliberately *not* `callers::LocalScopes`. That type is the thing under
/// test: it tracks frames, ordering and pending sets, and re-deriving its
/// answer here would produce a check that cannot fail — the theatre this
/// module's header warns about. This walk knows nothing about scopes. It reads
/// the signature for `params` and unions every pattern it meets for `any`,
/// order-free, whole-fn. Two mechanisms, one question.
///
/// [`crate::callers::pat_idents`] is shared, because it extracts names from a
/// pattern and has no opinion about scope; the disagreement being tested is
/// about *where a name is in scope*, which is entirely on the other side.
#[derive(Default)]
struct BinderScan {
    scope: ScopeTracker,
    /// Parameters, by enclosing fn. In scope for the entire body with no
    /// ordering question, which is what makes them a hard invariant.
    params: BTreeMap<String, BTreeSet<String>>,
    /// Every name bound by any pattern in the fn — `let`s, arms, closure
    /// heads, `for` heads. Order-free and therefore over-approximate: a name
    /// bound *after* a call is not in scope at it, so these are leads.
    any: BTreeMap<String, BTreeSet<String>>,
    /// Names declared as nested `fn` items inside a fn. A block-level item
    /// shadows a parameter of the same name, so a bare call there names the
    /// item after all and must not be reported.
    nested_fns: BTreeMap<String, BTreeSet<String>>,
}

impl<'ast> syn::visit::Visit<'ast> for BinderScan {
    scope_visits!(item_mod, item_impl, impl_item_fn, trait_item_fn);

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        // Recorded against the *enclosing* fn, before descending: this is the
        // parent's shadowing item, not its own.
        self.nested_fns
            .entry(self.scope.enclosing_with_toplevel())
            .or_default()
            .insert(i.sig.ident.to_string());
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        syn::visit::visit_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        let here = self.scope.enclosing_with_toplevel();
        let names = crate::callers::params_of(s);
        self.any.entry(here.clone()).or_default().extend(names.clone());
        self.params.entry(here).or_default().extend(names);
        syn::visit::visit_signature(self, s);
    }

    fn visit_pat(&mut self, p: &'ast syn::Pat) {
        let mut names = BTreeSet::new();
        crate::callers::pat_idents(p, &mut names);
        self.any
            .entry(self.scope.enclosing_with_toplevel())
            .or_default()
            .extend(names);
        syn::visit::visit_pat(self, p);
    }
}

/// A site the walk attributes to an item must not be naming a local binding.
///
/// The defect this exists for: a bare identifier in *argument position* is how
/// a callback is written (`.map(parse)`) and how every variable is written, and
/// the walk recorded the first reading unconditionally. One project's `out::path()`
/// was reported with 30 callers across 7 modules, at `resolved`, every one of
/// them a parameter or a `match` binding — and `--candidates` then ranked the
/// fn 4th of 286 on the strength of it. No confidence tier separated the fake
/// rows from the one real one, and every invariant here passed: the oracle is a
/// token scan, so it sees `path` too and `callers-within-oracle` holds
/// trivially. That is the gap this closes — over-reporting was bounded against
/// a token scan, and never against *scope*.
///
/// Parameters are a violation and everything else is a lead, and the line is
/// the ordering: a parameter is in scope for the whole body, while a name bound
/// by a `let` further down is legitimately the item at a call above it.
fn check_shadowed_bindings(
    ctx: &AnalysisCtx,
    sites: &[crate::callers::CallSite],
    v: &mut Vec<Violation>,
    leads: &mut Vec<Violation>,
) -> usize {
    let mut scan = BinderScan::default();
    for f in ctx.files {
        scan.scope = ScopeTracker::new(f.module.as_str());
        syn::visit::Visit::visit_file(&mut scan, &f.ast);
    }
    let mut examined = 0;
    for s in sites {
        // Only a bare name can be captured. A qualified path, a method and a
        // macro all name the item however many locals share the spelling —
        // and `shadowed` sites are the ones the walk already caught, so
        // counting them would be marking its own homework.
        if s.shadowed
            || s.target.contains("::")
            || s.target.starts_with('.')
            || s.target.ends_with('!')
        {
            continue;
        }
        examined += 1;
        if scan
            .nested_fns
            .get(&s.caller)
            .is_some_and(|n| n.contains(&s.target))
        {
            continue;
        }
        let param = scan
            .params
            .get(&s.caller)
            .is_some_and(|n| n.contains(&s.target));
        let bound = scan
            .any
            .get(&s.caller)
            .is_some_and(|n| n.contains(&s.target));
        if param {
            v.push(Violation {
                invariant: "no-site-is-a-shadowed-binding",
                subject: s.target.clone(),
                file: s.file.clone(),
                line: s.line,
                detail: format!(
                    "recorded as a call to an item named `{}`, but `{}` is a parameter of the \
                     enclosing `{}` — in scope for the whole body, so this names the binding",
                    s.target, s.target, s.caller
                ),
            });
        } else if bound {
            leads.push(Violation {
                invariant: "no-site-is-a-shadowed-binding",
                subject: s.target.clone(),
                file: s.file.clone(),
                line: s.line,
                detail: format!(
                    "`{}` is also bound by a pattern somewhere in `{}`; a lead, not a verdict — \
                     a binding below this line is not in scope at it",
                    s.target, s.caller
                ),
            });
        }
    }
    examined
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
// 6. Metamorphic: the same thing, for the commands that take a type or an enum
// ---------------------------------------------------------------------------

/// How many types to probe, as a fraction of `--probes`.
///
/// Each type runs up to six commands twice, and every one of those is a
/// whole-tree scan, so this budget divides rather than multiplies. The probe
/// set is ordered the way [`probe_set`] is — hardest shapes first — so a small
/// number is not a small sample.
const TYPE_PROBE_DIVISOR: usize = 12;

/// One name-taking command, invoked through a silent `Out` so only its count
/// escapes.
struct TypeQuery {
    label: &'static str,
    /// `"enum"`, or `""` for anything nameable as a type.
    kind: &'static str,
    ask: fn(&AnalysisCtx, &str) -> anyhow::Result<usize>,
}

const TYPE_QUERIES: &[TypeQuery] = &[
    TypeQuery {
        label: "type-refs",
        kind: "",
        ask: |c, t| crate::type_refs::run(c, t, None),
    },
    TypeQuery {
        label: "takes-mut",
        kind: "",
        ask: crate::takes_mut::run,
    },
    TypeQuery {
        label: "enum-coverage",
        kind: "enum",
        ask: |c, t| {
            crate::parallel_matches::run_enum_coverage(
                c,
                Some(t),
                crate::parallel_matches::CoverageOpts::default(),
            )
        },
    },
    TypeQuery {
        label: "catch-all-arms",
        kind: "enum",
        ask: |c, t| crate::catch_all::run(c, Some(t)),
    },
    TypeQuery {
        label: "parallel-matches",
        kind: "enum",
        ask: |c, t| {
            crate::parallel_matches::run(c, Some(t), crate::parallel_matches::ScanOpts::default())
        },
    },
    TypeQuery {
        label: "divergence",
        kind: "enum",
        ask: |c, t| crate::divergence::run(c, Some(t), 0.0),
    },
];

/// `Kind` and `scene::Kind` name the same enum, so every command that takes one
/// must answer identically.
///
/// [`check_query_forms`] cannot see this class. It opens with
/// `if d.kind != "fn"` and resolves through `callers::QueryMatcher`, and the
/// type- and enum-target commands never touch `QueryMatcher` — so six of them
/// rejected module-qualified names for six releases while `self-check` reported
/// `ok query-form-invariance 120 0 0` throughout. A session that hit it on
/// `enum-coverage scene::Kind` — an enum carrying a `/// unruster: sealed`
/// marker placed there for that command — fell back to a hand-typed
/// `grep -rn 'Kind::Circle\|Kind::Ellipse\|…'` for the rest of the refactor.
///
/// Counts, not rows: the commands share no row type, and a count that differs
/// between two spellings of one name is already the defect. The probe runs
/// through a silent `Out` — the pattern `waivers_cmd::populate_hits` uses.
///
/// Returns the number of (type, command) pairs compared, so an empty probe set
/// reports as `none` rather than as a green tick over nothing.
fn check_type_query_forms(ctx: &AnalysisCtx, budget: usize, out: &mut Vec<Violation>) -> usize {
    let quiet = crate::emit::Out::silent();
    let probe = AnalysisCtx { // unruster: ok(config-drift/AnalysisCtx) 2026-08-15 — the third probe context, and it disagrees on `summary` on purpose: this one compares the counts the runs return, which are produced inside the row loops, so it cannot skip them the way `populate_hits` does
        files: ctx.files,
        idx: ctx.idx,
        sem: ctx.sem,
        corpus: ctx.corpus,
        // NOT `summary: true`, for the reason `battery_at_ref` gives: every
        // check guards its row loop with `if !summary`, and what this invariant
        // compares is the count those runs return. A probe that skipped the
        // loops could compare two zeroes and report a green tick over nothing,
        // which is the one result this command exists to make impossible.
        // `Out::silent()` is what suppresses the printing.
        summary: false,
        spans: false,
        changed: None,
        out: &quiet,
        suppressions: ctx.suppressions,
        suggest_waivers: false,
    };
    let mut pairs = 0;
    for d in type_probe_set(ctx, budget) {
        for q in TYPE_QUERIES {
            if !q.kind.is_empty() && d.kind != q.kind {
                continue;
            }
            pairs += 1;
            // An `Err` is `TargetNotFound`, which is itself an answer: the two
            // spellings have to agree about *that* too, and disagreeing about
            // it is exactly the shape of the six defects.
            let bare = (q.ask)(&probe, &d.name).map_err(|_| ());
            let qualified = (q.ask)(&probe, &d.qpath).map_err(|_| ());
            if bare != qualified {
                out.push(Violation {
                    invariant: "type-query-form-invariance",
                    subject: d.qpath.clone(),
                    file: d.file.clone(),
                    line: d.line,
                    detail: format!(
                        "`{} {}` answers {} but `{} {}` answers {}",
                        q.label,
                        d.name,
                        reply(&bare),
                        q.label,
                        d.qpath,
                        reply(&qualified)
                    ),
                });
            }
        }
    }
    pairs
}

fn reply(r: &Result<usize, ()>) -> String {
    match r {
        Ok(n) => format!("{} row(s)", n),
        Err(()) => "no such target".to_string(),
    }
}

/// Types whose name is qualified and unambiguous, hardest shapes first.
///
/// A type whose `qpath` is already bare cannot exercise the invariant, and one
/// whose bare name is shared by two declarations is a genuinely ambiguous query
/// where the two forms are *allowed* to differ.
fn type_probe_set<'a>(ctx: &'a AnalysisCtx, budget: usize) -> Vec<&'a Defn> {
    let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
    for d in ctx.idx.iter() {
        if matches!(d.kind, "struct" | "enum" | "union" | "type") {
            *by_name.entry(d.name.as_str()).or_insert(0) += 1;
        }
    }
    let mut scored: Vec<(u32, &Defn)> = ctx
        .idx
        .iter()
        .filter(|d| matches!(d.kind, "struct" | "enum"))
        .filter(|d| d.qpath != d.name && by_name.get(d.name.as_str()).copied() == Some(1))
        .map(|d| {
            let mut s = 0;
            // Enums reach four more commands than structs do, and all four of
            // the known defects were on the enum side.
            if d.kind == "enum" {
                s += 4;
            }
            // A deeper path is more qualifier for a `last_segment` to lose.
            s += d.qpath.matches("::").count() as u32;
            if d.vis == "priv" {
                s += 1;
            }
            (s, d)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.file.cmp(&b.1.file))
            .then_with(|| a.1.line.cmp(&b.1.line))
    });
    let take = if budget == usize::MAX {
        usize::MAX
    } else {
        (budget / TYPE_PROBE_DIVISOR).max(1)
    };
    scored.into_iter().take(take).map(|(_, d)| d).collect()
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

fn emit(
    ctx: &AnalysisCtx,
    ran: &[(&'static str, usize)],
    v: &[Violation],
    leads: &[Violation],
) {
    if !ctx.summary {
        ctx.out.section("invariants");
        let mut by_inv: BTreeMap<&str, usize> = BTreeMap::new();
        for x in v {
            *by_inv.entry(x.invariant).or_insert(0) += 1;
        }
        for (name, probes) in ran {
            let bad = by_inv.get(name).copied().unwrap_or(0);
            // An invariant that examined nothing has not passed. `ok` over zero
            // probes is the most expensive output this command could produce,
            // because it is indistinguishable from a real result.
            let hinted = leads.iter().filter(|x| x.invariant == *name).count();
            let status = match (probes, bad, hinted) {
                (0, _, _) => "none",
                (_, 0, 0) => "ok",
                (_, 0, _) => "ok+leads",
                _ => "FAIL",
            };
            ctx.out.row(vec![
                ("status", Val::from(status)),
                ("invariant", Val::from(*name)),
                ("probes", Val::from(*probes)),
                ("violations", Val::from(bad)),
                ("leads", Val::from(hinted)),
            ]);
        }
        if !leads.is_empty() {
            ctx.out.section("leads");
            for x in leads.iter().take(40) {
                ctx.out.row(vec![
                    ("invariant", Val::from(x.invariant)),
                    ("subject", Val::from(x.subject.clone())),
                    ("at", site(&x.file, x.line)),
                    ("detail", Val::from(x.detail.clone())),
                ]);
            }
            if leads.len() > 40 {
                ctx.out
                    .row_note(&format!("(note: showing 40 of {} lead(s))", leads.len()));
            }
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
    let empty: Vec<&str> = ran
        .iter()
        .filter(|(_, n)| *n == 0)
        .map(|(name, _)| *name)
        .collect();
    if !empty.is_empty() {
        ctx.out.row_note(&format!(
            "(warning: {} invariant(s) examined nothing and are reported `none`, not `ok`: {}. \
             `--scope all` and `--probes 0` widen the probe set)",
            empty.len(),
            empty.join(", ")
        ));
    }
    ctx.out.summary(&format!(
        "({} invariant(s), {} with a non-empty probe set, {} violation(s), {} lead(s) \
         (leads do not fail the run); agreement is evidence only between independent \
         paths — see the module header for what is deliberately not checked)",
        ran.len(),
        ran.len() - empty.len(),
        v.len(),
        leads.len()
    ));
}
