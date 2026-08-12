//! `near-clones` — two bodies that were one body until somebody edited one.
//!
//! # Why `clones` cannot answer this
//!
//! [`crate::clones`] is EXACT by design and says so: two bodies in a group are
//! token-identical after alpha-renaming, with no similarity threshold. That cut
//! makes its findings believable, and it has one consequence nobody wants —
//! **the check goes quiet at exactly the moment the copies start to diverge.**
//! Two byte-identical copies of a helper are a maintenance smell. The same two
//! copies after one of them gains a bug fix are a *defect*, and that is the
//! version `clones` stops reporting.
//!
//! # How a near clone is found without a similarity metric
//!
//! Not by scoring how alike two bodies look. Every body is canonicalized into a
//! **skeleton** — punctuation, delimiters and leaf positions — and a list of
//! **leaves**: the idents and literals that sat in those positions, with the
//! function's own bindings already alpha-renamed ([`crate::facts`]).
//!
//! Two bodies that differ only at leaf positions therefore have a *byte-equal*
//! skeleton. So bucketing on the skeleton is not an optimisation, it is the
//! definition: everything in one bucket has identical structure, identical
//! token count and identical control flow, and the only question left is how
//! many of the leaves disagree. That question is answered by a positional
//! comparison, not an edit distance, so a row says something exact — "these two
//! differ in one leaf, and that leaf is `users` against `orders`".
//!
//! It is also what makes the search affordable: one linear pass to bucket, then
//! small within-bucket comparisons, rather than the quadratic all-pairs sweep a
//! real similarity metric would need.
//!
//! # Precision
//!
//! EXACT about *what* differs; a judgment call about whether it should. Two
//! bodies differing in one literal are sometimes two jobs that legitimately
//! share a shape (`min` and `max`, `left` and `right`) — the check ranks a
//! one-leaf difference highest precisely because that is where both the worst
//! copy-paste bugs and the most deliberate twins live. The row names the
//! difference so the reader can tell them apart in one glance instead of
//! opening two files.

use std::collections::BTreeMap;

use crate::context::{AnalysisCtx, Counts};
use crate::corpus::Corpus;
use crate::emit::{row, site};
use crate::facts::BodyFact;

/// Leaves that may differ before two bodies stop being versions of one body.
///
/// Two is the useful default rather than one. A drifted copy usually differs in
/// the fixed thing *and* in something the fix dragged along — a renamed local
/// that alpha-renaming cannot absorb because it became a different call, or a
/// changed constant beside a changed method. One catches the cleanest cases and
/// misses the common one; past three the rows stop being "the same code" in any
/// sense a reader will accept.
pub const DEFAULT_MAX_DIFF: usize = 2;

/// Same floor as `clones`, and for the same reason: two copies of a three-token
/// accessor say something about Rust, not about the codebase.
pub const DEFAULT_MIN_TOKENS: usize = 24;

/// The score at or above which a pair is a gating `audit` finding.
///
/// Tuned so the gate admits "a substantial body, duplicated under one name,
/// differing in a single leaf" — the shape where one copy has a fix and the
/// other does not, and where the row already tells you which leaf to look at.
pub const GATING_SCORE: f64 = 0.75;

/// Bodies sharing one skeleton, past which the bucket is reported rather than
/// expanded.
///
/// A skeleton shared by dozens of functions is a shape the language or a macro
/// imposes — `impl Display` bodies, generated accessors, exhaustive match arms
/// — and every pair inside it is a coincidence of form. Expanding it would emit
/// hundreds of rows and bury the real findings. The cap announces itself, per
/// this tool's rule that a silent truncation reads as "that is all there is".
const MAX_BUCKET: usize = 12;

/// One pair of bodies that differ in a few leaves.
struct Pair<'a> {
    a: &'a BodyFact,
    b: &'a BodyFact,
    /// Positions whose leaves disagree, as `(a_leaf, b_leaf)`.
    diffs: Vec<(&'a str, &'a str)>,
    tokens: usize,
    same_name: bool,
    same_dir: bool,
    /// How many bodies are in this pair's family (see [`spanning_pairs`]).
    family: usize,
    /// Waiver key and row identity.
    label: String,
}

impl Pair<'_> {
    /// Rank: how close the two are, how much code that covers, and how strongly
    /// they claim to be one thing.
    ///
    /// Closeness leads, which is the inversion this check exists to make. Every
    /// other duplication rank in this tool asks "how much is duplicated"; here
    /// the interesting quantity is how *little* is not, because one differing
    /// leaf between two forty-token bodies is either a fix that landed once or
    /// a bug that was pasted twice, and both are worth a reader's next minute.
    fn score(&self, max_diff: usize) -> f64 {
        let n = self.diffs.len() as f64;
        let span = max_diff.max(1) as f64;
        // 1 differing leaf → 1.0, falling linearly to the cap.
        let closeness = (1.0 - (n - 1.0) / span).clamp(0.0, 1.0);
        let bulk = (self.tokens as f64 / 40.0).min(1.0);
        let named = if self.same_name { 0.15 } else { 0.0 };
        let local = if self.same_dir { 0.05 } else { 0.0 };
        (0.30 + 0.25 * closeness + 0.20 * bulk + named + local).min(1.0)
    }

    /// `users→orders, 3→5` — the drift itself, capped so a row stays a row.
    fn delta(&self) -> String {
        let shown: Vec<String> = self
            .diffs
            .iter()
            .take(3)
            .map(|(a, b)| format!("{}→{}", a, b))
            .collect();
        if self.diffs.len() > 3 {
            format!("{}, +{} more", shown.join(", "), self.diffs.len() - 3)
        } else {
            shown.join(", ")
        }
    }
}

fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// The positions at which two leaf lists disagree, or `None` if they are not
/// comparable.
///
/// Equal lengths are guaranteed by construction — one skeleton placeholder per
/// leaf — so an inequality here would mean the canonicalizer and the skeleton
/// had drifted apart. Returning `None` rather than asserting keeps a corrupt
/// cache entry from aborting a run, and the bucket simply reports nothing.
fn leaf_diffs<'a>(a: &'a [String], b: &'a [String]) -> Option<Vec<(&'a str, &'a str)>> {
    if a.len() != b.len() {
        return None;
    }
    Some(
        a.iter()
            .zip(b.iter())
            .filter(|(x, y)| x != y)
            .map(|(x, y)| (x.as_str(), y.as_str()))
            .collect(),
    )
}

/// Reduce a family of mutually-near bodies to the pairs that connect it.
///
/// Six sibling visitors that differ in one literal are one finding, and the
/// all-pairs reading of them is fifteen rows. Worse than verbose: the fifteen
/// crowd every other check out of the top of an `audit`, and the reader cannot
/// tell from any one row that the other fourteen describe the same family.
///
/// So the qualifying pairs are treated as edges of a graph and a **minimum
/// spanning forest** is taken over them, cheapest edge first (Kruskal). Every
/// emitted row is still a real, checkable claim about two named functions —
/// nothing is invented or merged — and a family of *n* bodies produces *n − 1*
/// rows that chain through it, led by its closest pair. `family` on each row
/// says how large the family it belongs to is, so one row is enough to know
/// there are others.
fn spanning_pairs(mut edges: Vec<(usize, usize, usize)>, n: usize) -> Vec<(usize, usize)> {
    // Cheapest first, then by index so the choice among equal-cost edges is
    // deterministic across runs.
    edges.sort_by_key(|&(d, i, j)| (d, i, j));
    let mut parent: Vec<usize> = (0..n).collect();
    let mut kept = Vec::new();
    for (_, i, j) in edges {
        let (ri, rj) = (uf_find(&mut parent, i), uf_find(&mut parent, j));
        if ri == rj {
            continue;
        }
        parent[ri] = rj;
        kept.push((i, j));
    }
    kept
}

/// Union-find root of `x`, with path compression.
fn uf_find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Size of each index's component, after [`spanning_pairs`] has joined them.
fn family_sizes(pairs: &[(usize, usize)], n: usize) -> Vec<usize> {
    let mut parent: Vec<usize> = (0..n).collect();
    for &(i, j) in pairs {
        let (ri, rj) = (uf_find(&mut parent, i), uf_find(&mut parent, j));
        if ri != rj {
            parent[ri] = rj;
        }
    }
    let mut count = vec![0usize; n];
    for i in 0..n {
        let r = uf_find(&mut parent, i);
        count[r] += 1;
    }
    (0..n)
        .map(|i| {
            let r = uf_find(&mut parent, i);
            count[r]
        })
        .collect()
}

pub struct Opts {
    pub min_tokens: usize,
    pub max_diff: usize,
    pub min_score: f64,
}

pub fn run(ctx: &AnalysisCtx, corpus: &Corpus, opts: &Opts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, corpus, opts)?.total)
}

pub fn run_counted(ctx: &AnalysisCtx, corpus: &Corpus, opts: &Opts) -> anyhow::Result<Counts> {
    let mut buckets: BTreeMap<&str, Vec<&BodyFact>> = BTreeMap::new();
    for b in &corpus.bodies {
        if b.tokens >= opts.min_tokens {
            buckets.entry(b.skeleton.as_str()).or_default().push(b);
        }
    }
    let scanned: usize = buckets.values().map(Vec::len).sum();

    let mut pairs: Vec<Pair> = Vec::new();
    let mut skipped_buckets = 0usize;
    let mut skipped_bodies = 0usize;
    for members in buckets.values() {
        if members.len() < 2 {
            continue;
        }
        if members.len() > MAX_BUCKET {
            skipped_buckets += 1;
            skipped_bodies += members.len();
            continue;
        }
        let mut edges: Vec<(usize, usize, usize)> = Vec::new();
        for i in 0..members.len() {
            for j in i + 1..members.len() {
                let Some(diffs) = leaf_diffs(&members[i].leaves, &members[j].leaves) else {
                    continue;
                };
                // Zero differences is an exact clone; `clones` owns those, and
                // reporting them here would double-count every one of them.
                if diffs.is_empty() || diffs.len() > opts.max_diff {
                    continue;
                }
                edges.push((diffs.len(), i, j));
            }
        }
        let spanning = spanning_pairs(edges, members.len());
        let sizes = family_sizes(&spanning, members.len());
        for &(i, j) in &spanning {
            let (a, b) = if (members[i].file.as_str(), members[i].line)
                <= (members[j].file.as_str(), members[j].line)
            {
                (members[i], members[j])
            } else {
                (members[j], members[i])
            };
            let same_name = a.name == b.name;
            let label = if same_name {
                a.name.clone()
            } else {
                format!("{}/{}", a.name, b.name)
            };
            pairs.push(Pair {
                diffs: leaf_diffs(&a.leaves, &b.leaves).unwrap_or_default(),
                tokens: a.tokens,
                same_name,
                same_dir: dir_of(&a.file) == dir_of(&b.file),
                family: sizes[i],
                label,
                a,
                b,
            });
        }
    }

    // `--changed-since` keeps a pair when either end moved: the finding is the
    // divergence between them, and it is actionable from either side.
    if ctx.changed.is_some() {
        pairs.retain(|p| ctx.in_scope(&p.a.file) || ctx.in_scope(&p.b.file));
    }

    let waived = ctx.retain_unsuppressed("near-clones", &mut pairs, |p| {
        crate::suppress::Site::keyed(p.a.file.as_str(), p.a.line, &p.label)
    });

    let below = {
        let n = pairs.len();
        pairs.retain(|p| p.score(opts.max_diff) >= opts.min_score);
        n - pairs.len()
    };

    pairs.sort_by(|x, y| {
        y.score(opts.max_diff)
            .total_cmp(&x.score(opts.max_diff))
            .then_with(|| x.diffs.len().cmp(&y.diffs.len()))
            .then_with(|| x.a.file.cmp(&y.a.file))
            .then_with(|| x.a.line.cmp(&y.a.line))
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for p in &pairs {
            row!(
                ctx.out,
                "what" => p.label.clone(),
                "score" => format!("{:.2}", p.score(opts.max_diff)),
                "diffs" => p.diffs.len().to_string(),
                "family" => p.family.to_string(),
                "tokens" => p.tokens.to_string(),
                "delta" => p.delta(),
                "at" => site(&p.a.file, p.a.line),
                "fn" => p.a.qpath.clone(),
                "vs_at" => site(&p.b.file, p.b.line),
                "vs" => p.b.qpath.clone(),
            );
            ctx.suggest("near-clones", Some(&p.label), today);
        }
    }

    if skipped_buckets > 0 {
        ctx.out.row_note(&format!(
            "(note: {} shared-shape group(s) covering {} bod(ies) were larger than {} members \
             and were not expanded — a shape that many functions share is imposed by a macro \
             or by the language, not copy-pasted)",
            skipped_buckets, skipped_bodies, MAX_BUCKET
        ));
    }

    let gating = pairs
        .iter()
        .filter(|p| p.score(opts.max_diff) >= GATING_SCORE)
        .count();
    ctx.out.summary(&format!(
        "({} near-duplicate pair(s){}{}; {} bod(ies) scanned; max_diff={} min_tokens={}{}{}; \
         explain: replication)",
        pairs.len(),
        if gating > 0 {
            format!(
                ", {} at score >= {:.2} (the tier `audit` gates on)",
                gating, GATING_SCORE
            )
        } else {
            String::new()
        },
        if below > 0 {
            format!("; {} below --min-score {:.2}", below, opts.min_score)
        } else {
            String::new()
        },
        scanned,
        opts.max_diff,
        opts.min_tokens,
        corpus.cache_note(),
        ctx.waived_note(waived)
    ));
    Ok(Counts {
        total: pairs.len(),
        gating,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(srcs: &[(&str, &str)]) -> Corpus {
        let mut c = Corpus::default();
        for (path, src) in srcs {
            let pf = crate::parse::ParsedFile {
                path: std::path::PathBuf::from(path),
                ast: syn::parse_file(src).expect("parse"),
                module: "m".into(),
            };
            let f = crate::facts::derive(&pf);
            c.items.extend(f.items);
            c.bodies.extend(f.bodies);
        }
        c
    }

    /// A copy that gained a fix. The literal is the whole finding, and the row
    /// must be able to name it.
    #[test]
    fn one_differing_literal_is_a_near_clone_and_the_delta_names_it() {
        let c = bodies(&[
            (
                "src/a.rs",
                r#"fn purge(d: &D, n: usize) -> Result<()> {
                       let rows = d.query("DELETE FROM users WHERE age > ?", n)?;
                       log::info!("purged {} rows", rows);
                       Ok(())
                   }"#,
            ),
            (
                "src/b.rs",
                r#"fn purge(d: &D, n: usize) -> Result<()> {
                       let rows = d.query("DELETE FROM orders WHERE age > ?", n)?;
                       log::info!("purged {} rows", rows);
                       Ok(())
                   }"#,
            ),
        ]);
        assert_eq!(c.bodies.len(), 2);
        let d = leaf_diffs(&c.bodies[0].leaves, &c.bodies[1].leaves).expect("comparable");
        assert_eq!(d.len(), 1);
        assert!(d[0].0.contains("users") && d[0].1.contains("orders"));
    }

    /// The hand-off to `clones`: identical bodies are not near clones.
    #[test]
    fn an_exact_copy_is_left_to_the_clones_check() {
        let src = r#"fn f(d: &D) -> Result<()> {
                         let rows = d.query("DELETE FROM users", 1)?;
                         log::info!("purged {} rows", rows);
                         Ok(())
                     }"#;
        let c = bodies(&[("src/a.rs", src), ("src/b.rs", src)]);
        let d = leaf_diffs(&c.bodies[0].leaves, &c.bodies[1].leaves).expect("comparable");
        assert!(d.is_empty());
    }

    /// Different structure must never enter one bucket, or the positional
    /// comparison would be lining up unrelated leaves.
    #[test]
    fn restructured_code_is_not_a_near_clone() {
        let c = bodies(&[
            ("src/a.rs", "fn f(x: T) -> u32 { let y = x.a(); y + x.b() + x.c() }"),
            ("src/b.rs", "fn g(x: T) -> u32 { x.a() + x.b() + x.c() + 1 }"),
        ]);
        assert_ne!(c.bodies[0].skeleton, c.bodies[1].skeleton);
    }

    fn pair<'a>(a: &'a BodyFact, b: &'a BodyFact, diffs: usize, tokens: usize, named: bool) -> Pair<'a> {
        Pair {
            a,
            b,
            diffs: (0..diffs).map(|_| ("x", "y")).collect(),
            tokens,
            same_name: named,
            same_dir: true,
            family: 2,
            label: "l".into(),
        }
    }

    fn body(name: &str, file: &str) -> BodyFact {
        BodyFact {
            name: name.into(),
            qpath: format!("m::{name}"),
            file: file.into(),
            line: 1,
            end: 9,
            tokens: 40,
            skeleton: "·".into(),
            leaves: vec!["a".into()],
        }
    }

    /// Closeness leads the rank: the pair that differs least is the pair most
    /// likely to be one fix that landed once.
    #[test]
    fn fewer_differences_rank_higher() {
        let (a, b) = (body("f", "src/a.rs"), body("f", "src/b.rs"));
        let one = pair(&a, &b, 1, 40, true);
        let two = pair(&a, &b, 2, 40, true);
        assert!(one.score(2) > two.score(2));
        assert!(
            one.score(2) >= GATING_SCORE,
            "a big single-leaf divergence under one name should gate ({:.2})",
            one.score(2)
        );
    }

    #[test]
    fn a_small_differently_named_pair_does_not_gate() {
        let (a, b) = (body("encode", "src/a.rs"), body("write", "src/b.rs"));
        let p = pair(&a, &b, 2, 24, false);
        assert!(p.score(2) < GATING_SCORE, "score {:.2}", p.score(2));
    }

    /// Six sibling visitors differing in one literal are one family, not
    /// fifteen findings. The spanning forest is what keeps a clique from
    /// crowding every other check out of an `audit`.
    #[test]
    fn a_family_of_six_yields_five_rows_not_fifteen() {
        let mut edges = Vec::new();
        for i in 0..6 {
            for j in i + 1..6 {
                edges.push((1usize, i, j));
            }
        }
        let kept = spanning_pairs(edges, 6);
        assert_eq!(kept.len(), 5);
        assert!(family_sizes(&kept, 6).iter().all(|&n| n == 6));
    }

    /// Two independent families in one bucket must stay independent.
    #[test]
    fn disjoint_families_are_not_joined() {
        let kept = spanning_pairs(vec![(1, 0, 1), (1, 2, 3)], 4);
        assert_eq!(kept.len(), 2);
        assert_eq!(family_sizes(&kept, 4), vec![2, 2, 2, 2]);
    }

    /// Kruskal takes the cheapest edges, so the rows a reader sees first are
    /// the closest pairs in the family rather than an arbitrary chain.
    #[test]
    fn the_closest_pair_is_the_one_kept() {
        // 0–1 differ in 2, 0–2 and 1–2 differ in 1. A spanning tree of three
        // nodes has two edges; both cheap ones must win.
        let kept = spanning_pairs(vec![(2, 0, 1), (1, 0, 2), (1, 1, 2)], 3);
        assert_eq!(kept.len(), 2);
        assert!(!kept.contains(&(0, 1)), "the expensive edge should be dropped");
    }

    #[test]
    fn the_delta_cell_stays_one_row_long() {
        let (a, b) = (body("f", "src/a.rs"), body("f", "src/b.rs"));
        let p = pair(&a, &b, 7, 40, true);
        let d = p.delta();
        assert!(d.contains("+4 more"), "{d}");
        assert!(!d.contains('\n'));
    }
}
