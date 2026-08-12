//! `concepts` — the same idea declared more than once, on the **noun** axis.
//!
//! # The gap this fills
//!
//! Every other duplication check here works on what code *does*: `clones`
//! compares bodies, `divergence` compares dispatch, `config-drift` compares
//! literals, `builder-drift` compares call chains. Nothing compared what data
//! *is*. So a codebase could carry `UserId(u64)`, `AccountId(u64)` and
//! `OwnerId(u64)` — three spellings of one concept, three sets of conversions,
//! three chances to drift — and the battery reported nothing, because no two
//! bodies were alike and no dispatch site disagreed.
//!
//! That shape is the commonest form of concept drift in Rust specifically,
//! because the language makes minting a new type free and gives the compiler no
//! way to notice that one already means the same thing.
//!
//! # Five views, one thesis
//!
//! | kind           | what groups                                   | precision |
//! |----------------|-----------------------------------------------|-----------|
//! | `newtype`      | newtypes/aliases over one inner type, cognate names | EXACT |
//! | `struct-shape` | named structs with the same field-type multiset | EXACT   |
//! | `enum-shape`   | enums whose variant names largely coincide     | APPROXIMATE |
//! | `signature`    | cognate fns with identical parameter and return types | APPROXIMATE |
//! | `doc`          | items whose doc comments say the same sentence | EXACT |
//!
//! # Why a shared *name word* is required, and not merely a shared shape
//!
//! The first cut of `newtype` grouped by inner type alone. On a real tree that
//! reported "31 newtypes over `String`", which is a fact about Rust, not about
//! the codebase, and no reader could act on it. Requiring the members to share
//! a word of their names — `UserId`/`OrderId` share `Id`, `Meters`/`Celsius`
//! share nothing — turns the same scan into a list of concepts somebody
//! actually duplicated. The same rule carries `signature`, for the same reason:
//! `(&str) -> Result<T>` is every parser in the tree, and `parse_user` beside
//! `parse_account` is two of them.
//!
//! # Precision
//!
//! A shape match is a **candidate**, never a proof. Types are compared as they
//! are spelled ([`crate::facts`] does no resolution), and two structs with
//! identical fields can still be two concepts — `Point` and `Size` are both
//! `{x: f64, y: f64}` in shape and neither is the other. Which is what the
//! waiver is for.

use std::collections::{BTreeMap, BTreeSet};

use crate::context::{AnalysisCtx, Counts};
use crate::corpus::Corpus;
use crate::emit::{row, site};
use crate::facts::{ItemFact, Shape};

/// The score at or above which a cluster is a gating `audit` finding.
///
/// Set so the gate admits "three or more cognate declarations of one shape,
/// spread across modules, exported" — the class where the fix is a single
/// canonical definition and it deletes the other findings rather than moving
/// them. A cognate *pair* inside one module lands below it and stays advisory,
/// on the same reasoning `clones` gives for not gating on pairs: two is how a
/// deliberate boundary looks, and gating on two puts the whole list in the gate.
pub const GATING_SCORE: f64 = 0.70;

/// Clusters below this are dropped outright. Everything above it is at least
/// worth one line of a reader's attention.
pub const DEFAULT_MIN_SCORE: f64 = 0.35;

/// How alike two variant-name sets must be before two enums are called the same
/// concept. Two thirds: `{Idle,Busy,Failed}` against `{Idle,Busy,Failed,Done}`
/// clears it (0.75), `{Idle,Busy}` against `{Read,Write}` does not (0.0).
const ENUM_JACCARD: f64 = 0.6;

/// Fewest words a doc comment must have before repeating it means anything.
/// "The name." is not a duplicated concept, it is English.
const DOC_MIN_WORDS: usize = 6;

/// Words that name a *language or library* concept rather than a domain one,
/// and so cannot be the thing two functions duplicated.
///
/// This list applies to the `signature` view only, and it exists because of
/// what the first run over this codebase produced: thirteen `as_str` methods
/// clustered on the word `str`, four `run` functions on `run`, four `build`
/// on `build`. Every one is a true observation about Rust's conventions and
/// none is a duplicated concept — `as_str` on thirteen enums is thirteen
/// different mappings that happen to share a name their authors did not choose
/// freely.
///
/// It deliberately does **not** apply to `newtype`, `struct-shape` or
/// `enum-shape`, where a shared word like `id` or `key` usually *is* the
/// concept. A stoplist that fired everywhere would suppress this module's
/// headline finding.
const GENERIC_API_WORDS: &[&str] = &[
    "and", "as", "at", "build", "by", "default", "err", "fmt", "for", "from", "get", "has", "in",
    "inner", "into", "is", "item", "iter", "key", "len", "map", "new", "next", "not", "of", "ok",
    "on", "opt", "option", "or", "out", "res", "result", "run", "set", "str", "string", "the",
    "to", "try", "value", "vec", "visit", "with",
];

// ──────────────────────────────────────────────────────────────────────────
// Names

/// Lowercased words of an identifier, splitting `CamelCase`, `snake_case` and
/// `SCREAMING_CASE` alike. `UserId` → `["user", "id"]`.
///
/// One-character fragments are dropped: `T`, `x` and the `s` of `Ids` carry no
/// concept, and admitting them makes every name cognate with every other.
pub fn words_of(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '_' || c == '-' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        // A lower→upper transition is a word boundary (`userId`), an
        // upper→upper one is not (`HTTPServer` stays whole rather than
        // shattering into eight letters).
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_numeric();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.retain(|w| w.chars().count() > 1);
    out
}

/// Words two names share, filtered as the `signature` view filters them.
///
/// Exposed because [`crate::gate`] asks the same question one candidate at a
/// time, and two implementations of "do these two names mean the same thing"
/// would drift — which is the defect this whole module reports about other
/// codebases.
pub fn cognate_words(a: &str, b: &str, drop_generic: bool) -> Vec<String> {
    let bw: BTreeSet<String> = words_of(b).into_iter().collect();
    words_of(a)
        .into_iter()
        .filter(|w| bw.contains(w))
        .filter(|w| !(drop_generic && GENERIC_API_WORDS.contains(&w.as_str())))
        .collect()
}

/// Is this word Rust API vocabulary rather than a domain concept?
///
/// Shared with [`crate::validation`], which forms sibling cohorts the same way
/// and hit the same wall: a `run`/`run_handling` cohort is not two functions
/// that should agree about validating their inputs, it is one entry point and
/// one variant of it.
pub fn is_generic_api_word(w: &str) -> bool {
    GENERIC_API_WORDS.contains(&w)
}

/// Words shared by every name in `names`, best-first by length — the longest
/// shared word is the most specific thing the group has in common.
fn shared_words(names: &[&str]) -> Vec<String> {
    let mut it = names.iter().map(|n| {
        words_of(n)
            .into_iter()
            .collect::<BTreeSet<String>>()
    });
    let Some(mut acc) = it.next() else {
        return Vec::new();
    };
    for s in it {
        acc = acc.intersection(&s).cloned().collect();
    }
    let mut v: Vec<String> = acc.into_iter().collect();
    v.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    v
}

/// Do every one of these names end with `word` (a suffix cohort like
/// `UserId`/`OrderId`), or every one begin with it?
///
/// A cohort that agrees on *where* the shared word sits is a naming convention
/// somebody followed on purpose, which is much stronger evidence of one concept
/// than the word merely occurring somewhere in each name.
fn positional_cohort(names: &[&str], word: &str) -> bool {
    let all = |f: fn(&[String], &str) -> bool| {
        names.iter().all(|n| {
            let w = words_of(n);
            !w.is_empty() && f(&w, word)
        })
    };
    all(|w, x| w.last().map(String::as_str) == Some(x))
        || all(|w, x| w.first().map(String::as_str) == Some(x))
}

/// |A ∩ B| / |A ∪ B| over two name sets.
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Clusters

/// One reported group: N declarations the tool believes name one concept.
struct Cluster<'a> {
    kind: &'static str,
    /// Short identity, also the waiver key. Free of spaces so it is typeable
    /// inside `ok(concepts/<key>)` without quoting.
    label: String,
    /// What the members have in common, in prose, for the row's `shape` cell.
    shape: String,
    members: Vec<&'a ItemFact>,
    /// The shared name word this cluster was formed on, when it has one.
    word: Option<String>,
    /// Members agree on where the shared word sits in the name.
    positional: bool,
    /// Extra evidence particular to the kind, already normalized to 0..1 — the
    /// field-name overlap for `struct-shape`, the variant overlap for
    /// `enum-shape`. `1.0` when the kind's grouping is itself exact.
    agreement: f64,
}

impl Cluster<'_> {
    fn modules(&self) -> usize {
        self.members
            .iter()
            .map(|m| m.module.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Rank: how many, how public, how far apart, and how deliberate the naming
    /// looks.
    ///
    /// The four terms answer four different questions a reader has, in the
    /// order they ask them. *How many* — three copies is a pattern, two is a
    /// pair. *How public* — a duplicated exported type costs every downstream
    /// caller, a duplicated private one costs this module. *How far apart* —
    /// declarations in one module are visible to each other and drift slowly;
    /// declarations in four modules cannot see each other at all, which is the
    /// mechanism this check exists to catch. *How deliberate* — a suffix cohort
    /// is a convention somebody was following, so a collision inside one is
    /// much more likely to be an oversight than a coincidence.
    fn score(&self) -> f64 {
        let n = self.members.len() as f64;
        // Zero at exactly two, saturating at five: past five the answer is
        // already "yes, consolidate", and letting it run would sort one large
        // cluster above three obvious ones.
        let count = ((n - 2.0) / 3.0).clamp(0.0, 1.0);
        let public = self.members.iter().filter(|m| m.is_pub()).count() as f64 / n;
        let spread = ((self.modules() as f64 - 1.0) / 2.0).clamp(0.0, 1.0);
        let deliberate = if self.positional { 1.0 } else { 0.0 };
        (0.28 + 0.22 * count + 0.14 * public + 0.16 * spread + 0.10 * deliberate
            + 0.15 * self.agreement)
            .min(1.0)
    }

    fn first(&self) -> &ItemFact {
        self.members[0]
    }
}

/// Sort members by location so a cluster's row is stable across runs.
fn ordered(mut v: Vec<&ItemFact>) -> Vec<&ItemFact> {
    v.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
    v
}

// ──────────────────────────────────────────────────────────────────────────
// 1. newtypes and aliases over one inner type

/// `struct Id(u64)`, `type Id = u64` — one concept wrapped once, several times.
///
/// Aliases are included with newtypes on purpose: `type UserId = u64;` and
/// `struct UserId(u64);` are two spellings of one intention, and a codebase
/// that has drifted usually has both.
fn newtype_clusters<'a>(c: &'a Corpus) -> Vec<Cluster<'a>> {
    let mut by_inner: BTreeMap<&str, Vec<&ItemFact>> = BTreeMap::new();
    for i in c.declarations() {
        if i.kind != "struct" && i.kind != "type" {
            continue;
        }
        if let Some(inner) = i.shape.newtype_inner() {
            by_inner.entry(inner).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (inner, members) in by_inner {
        if members.len() < 2 {
            continue;
        }
        out.extend(cognate_partition(
            members,
            "newtype",
            false,
            |word, _| {
                (
                    format!("newtype:{}:{}", word, sanitize(inner)),
                    format!("({})", inner),
                    1.0,
                )
            },
        ));
    }
    out
}

/// Split a same-shape group into the sub-groups that share a name word.
///
/// This is the step that turns "everything of this shape" into "everything
/// somebody meant as one thing". A member may share different words with
/// different neighbours, so several sub-groups can come out of one shape; a
/// sub-group wholly contained in another is dropped, since the wider one
/// already says everything it does.
fn cognate_partition<'a, F>(
    members: Vec<&'a ItemFact>,
    kind: &'static str,
    stoplist: bool,
    describe: F,
) -> Vec<Cluster<'a>>
where
    F: Fn(&str, &[&'a ItemFact]) -> (String, String, f64),
{
    let mut by_word: BTreeMap<String, Vec<&ItemFact>> = BTreeMap::new();
    for m in &members {
        for w in words_of(&m.name) {
            if stoplist && GENERIC_API_WORDS.contains(&w.as_str()) {
                continue;
            }
            by_word.entry(w).or_default().push(m);
        }
    }
    let mut groups: Vec<(String, Vec<&ItemFact>)> = by_word
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(w, v)| (w, ordered(v)))
        .collect();
    // Widest first, so a narrower subset can be recognised and dropped.
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    let mut kept: Vec<(String, Vec<&ItemFact>)> = Vec::new();
    for (word, g) in groups {
        let keys: BTreeSet<(&str, usize)> = g.iter().map(|m| (m.file.as_str(), m.line)).collect();
        let subsumed = kept.iter().any(|(_, other)| {
            let ok: BTreeSet<(&str, usize)> =
                other.iter().map(|m| (m.file.as_str(), m.line)).collect();
            keys.is_subset(&ok)
        });
        if !subsumed {
            kept.push((word, g));
        }
    }
    kept.into_iter()
        .map(|(word, g)| {
            let names: Vec<&str> = g.iter().map(|m| m.name.as_str()).collect();
            let (label, shape, agreement) = describe(&word, &g);
            Cluster {
                kind,
                label,
                shape,
                positional: positional_cohort(&names, &word),
                word: Some(word),
                agreement,
                members: g,
            }
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────
// 2. named structs with one field-type multiset

/// Two structs whose fields have the same types, in any order.
///
/// Order is discarded because a reordered copy is still a copy — and because
/// keeping it would let a field moved during a refactor hide the duplication
/// this check exists to find.
fn struct_shape_clusters<'a>(c: &'a Corpus) -> Vec<Cluster<'a>> {
    let mut by_shape: BTreeMap<String, Vec<&ItemFact>> = BTreeMap::new();
    for i in c.of_kind("struct") {
        let Shape::Fields(f) = &i.shape else { continue };
        if f.len() < 2 {
            continue;
        }
        let mut types: Vec<&str> = f.iter().map(|(_, t)| t.as_str()).collect();
        types.sort_unstable();
        by_shape.entry(types.join(", ")).or_default().push(i);
    }
    let mut out = Vec::new();
    for (types, members) in by_shape {
        if members.len() < 2 {
            continue;
        }
        let members = ordered(members);
        let overlap = field_name_overlap(&members);
        // A two-field `{String, String}` coincidence is not evidence; four or
        // more fields agreeing on type is, whatever the fields are called.
        let fields = field_count(members[0]);
        if overlap < 0.5 && fields < 4 {
            continue;
        }
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        let word = shared_words(&names).into_iter().next();
        let label = format!(
            "struct-shape:{}",
            sanitize(&names.iter().take(3).copied().collect::<Vec<_>>().join("/"))
        );
        out.push(Cluster {
            kind: "struct-shape",
            label,
            shape: format!("{{{}}}", types),
            positional: word
                .as_deref()
                .map(|w| positional_cohort(&names, w))
                .unwrap_or(false),
            word,
            agreement: overlap,
            members,
        });
    }
    out
}

fn field_count(i: &ItemFact) -> usize {
    match &i.shape {
        Shape::Fields(f) => f.len(),
        _ => 0,
    }
}

/// Average pairwise Jaccard over the members' field-*name* sets. Types already
/// match by construction; this asks whether the authors also agreed on what to
/// call them, which is what separates two copies of one record from two
/// unrelated types that happen to be two `f64`s.
fn field_name_overlap(members: &[&ItemFact]) -> f64 {
    let sets: Vec<BTreeSet<String>> = members
        .iter()
        .map(|m| match &m.shape {
            Shape::Fields(f) => f.iter().map(|(n, _)| n.to_lowercase()).collect(),
            _ => BTreeSet::new(),
        })
        .collect();
    let mut total = 0.0;
    let mut pairs = 0.0;
    for i in 0..sets.len() {
        for j in i + 1..sets.len() {
            total += jaccard(&sets[i], &sets[j]);
            pairs += 1.0;
        }
    }
    if pairs == 0.0 {
        0.0
    } else {
        total / pairs
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 3. enums with coinciding variant sets

/// `Status{Idle,Busy,Failed}` beside `State{Idle,Busy,Failed,Done}`.
///
/// Reported pairwise rather than as one cluster: enum similarity is a
/// *relation*, not an equivalence — A can be close to B and B to C without A
/// being close to C — so merging them into a group would assert something the
/// measurement does not support.
fn enum_shape_clusters<'a>(c: &'a Corpus) -> Vec<Cluster<'a>> {
    let enums: Vec<(&ItemFact, BTreeSet<String>)> = c
        .of_kind("enum")
        .filter_map(|i| match &i.shape {
            Shape::Variants(v) if v.len() >= 3 => {
                Some((i, v.iter().map(|s| s.to_lowercase()).collect()))
            }
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for i in 0..enums.len() {
        for j in i + 1..enums.len() {
            let (a, sa) = &enums[i];
            let (b, sb) = &enums[j];
            let sim = jaccard(sa, sb);
            if sim < ENUM_JACCARD {
                continue;
            }
            let shared: Vec<&str> = sa.intersection(sb).map(String::as_str).collect();
            let members = ordered(vec![*a, *b]);
            let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
            let word = shared_words(&names).into_iter().next();
            out.push(Cluster {
                kind: "enum-shape",
                label: format!("enum-shape:{}", sanitize(&format!("{}/{}", a.name, b.name))),
                shape: format!(
                    "{} of {} variant(s) shared: {}",
                    shared.len(),
                    sa.union(sb).count(),
                    shared.join(",")
                ),
                positional: false,
                word,
                agreement: sim,
                members,
            });
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────
// 4. cognate fns with one signature

/// The same operation, written twice, over the same types.
///
/// This is the shape `clones` cannot see. `clones` groups on the body, and two
/// implementations of one decision stop having the same body the moment one of
/// them is fixed — so the check goes quiet exactly when the drift starts. This
/// one groups on the *interface* and the *name*, both of which survive the
/// drift.
///
/// Bodies that `clones` already groups are excluded, so the two checks report a
/// finding once between them rather than each claiming it.
fn signature_clusters<'a>(c: &'a Corpus) -> Vec<Cluster<'a>> {
    // Bodies with a twin somewhere: `clones` owns these.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for b in &c.bodies {
        *seen.entry(b.canon()).or_insert(0) += 1;
    }
    let exact_cloned: BTreeSet<(&str, usize)> = c
        .bodies
        .iter()
        .filter(|b| seen.get(&b.canon()).copied().unwrap_or(0) > 1)
        .map(|b| (b.file.as_str(), b.line))
        .collect();

    let mut by_sig: BTreeMap<String, Vec<&ItemFact>> = BTreeMap::new();
    for i in c.declarations() {
        if i.kind != "fn" && i.kind != "impl-fn" {
            continue;
        }
        // A trait implementation's signature is the trait's decision, not its
        // author's, so two of them agreeing says nothing about this codebase.
        if i.in_trait_impl {
            continue;
        }
        let Shape::Signature { params, ret, .. } = &i.shape else {
            continue;
        };
        // A no-argument fn returning unit is a shape shared by every `main`,
        // every `drop` and every test; it discriminates nothing.
        if params.is_empty() && ret == "()" {
            continue;
        }
        if exact_cloned.contains(&(i.file.as_str(), i.line)) {
            continue;
        }
        by_sig
            .entry(format!("({}) -> {}", params.join(", "), ret))
            .or_default()
            .push(i);
    }
    let mut out = Vec::new();
    for (sig, members) in by_sig {
        if members.len() < 2 {
            continue;
        }
        let sig = sig.clone();
        out.extend(cognate_partition(
            members,
            "signature",
            true,
            move |word, _| {
                (
                    format!("signature:{}", sanitize(word)),
                    sig.clone(),
                    // The weakest of the five groupings, and weighted like it:
                    // two functions sharing an interface and a name word is
                    // suggestive, where two structs sharing every field type is
                    // nearly conclusive.
                    0.4,
                )
            },
        ));
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────
// 5. items documented with one sentence

/// Two items whose doc comments say the same thing.
///
/// The cheapest signal in this module and one of the strongest: a reader who
/// wrote the same sentence twice was describing the same concept twice. The
/// comparison is on normalized text, so punctuation and casing do not matter,
/// and it is exact rather than fuzzy — a near-miss between two English
/// sentences is a similarity metric, and this check is meant to be believed.
fn doc_clusters<'a>(c: &'a Corpus) -> Vec<Cluster<'a>> {
    let mut by_doc: BTreeMap<String, Vec<&ItemFact>> = BTreeMap::new();
    for i in c.declarations() {
        let Some(d) = &i.doc else { continue };
        let norm = normalize_doc(d);
        if norm.split(' ').filter(|w| !w.is_empty()).count() < DOC_MIN_WORDS {
            continue;
        }
        by_doc.entry(norm).or_default().push(i);
    }
    let mut out = Vec::new();
    for (doc, members) in by_doc {
        if members.len() < 2 {
            continue;
        }
        // A trait method and its implementations legitimately repeat the
        // trait's doc; that is the doc comment doing its job, not a duplicated
        // concept.
        if members.iter().all(|m| m.name == members[0].name)
            && members
                .iter()
                .all(|m| m.kind == "impl-fn" || m.kind == "trait-fn")
        {
            continue;
        }
        let members = ordered(members);
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        let word = shared_words(&names).into_iter().next();
        let short: String = doc.chars().take(48).collect();
        out.push(Cluster {
            kind: "doc",
            label: format!("doc:{}", sanitize(&short)),
            shape: format!("\"{}\"", short),
            positional: false,
            word,
            agreement: 1.0,
            members,
        });
    }
    out
}

/// Lowercase, punctuation to spaces, whitespace collapsed. Two sentences that
/// differ only in how they were typeset are one sentence.
///
/// Shared with [`crate::gate`], which asks the same question one candidate at a
/// time. The two had a copy each and `clones` reported them.
pub fn normalize_doc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() {
            o.extend(c.to_lowercase());
        } else if !o.ends_with(' ') {
            o.push(' ');
        }
    }
    o.trim().to_string()
}

/// Make a fragment safe to sit inside `ok(concepts/<key>)`: no whitespace and
/// no parenthesis, so the waiver the tool suggests is one a reader can type and
/// the parser can close.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            c if c.is_whitespace() => '_',
            '(' | ')' => '_',
            c => c,
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────
// Run

/// Which views to run. `None` means all five.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Newtype,
    StructShape,
    EnumShape,
    Signature,
    Doc,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Newtype => "newtype",
            Kind::StructShape => "struct-shape",
            Kind::EnumShape => "enum-shape",
            Kind::Signature => "signature",
            Kind::Doc => "doc",
        }
    }
}

pub struct Opts {
    pub kind: Option<Kind>,
    pub min_score: f64,
}

pub fn run(ctx: &AnalysisCtx, corpus: &Corpus, opts: &Opts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, corpus, opts)?.total)
}

/// Every cluster the requested views form, unfiltered and unranked.
fn collect<'a>(corpus: &'a Corpus, kind: Option<Kind>) -> Vec<Cluster<'a>> {
    let want = |k: &'static str| kind.is_none_or(|w| w.as_str() == k);
    let mut out: Vec<Cluster> = Vec::new();
    if want("newtype") {
        out.extend(newtype_clusters(corpus));
    }
    if want("struct-shape") {
        out.extend(struct_shape_clusters(corpus));
    }
    if want("enum-shape") {
        out.extend(enum_shape_clusters(corpus));
    }
    if want("signature") {
        out.extend(signature_clusters(corpus));
    }
    if want("doc") {
        out.extend(doc_clusters(corpus));
    }
    out
}

/// The member lists of every cluster scoring at or above [`DEFAULT_MIN_SCORE`].
///
/// Exposed for [`crate::vocabulary`], which runs the same clustering to ask a
/// different question — "does this group of look-alikes have a declared home?"
/// Two implementations of "what counts as one concept here" would drift apart,
/// and the two commands would then disagree about the same three types.
pub fn clusters(corpus: &Corpus, kind: Option<Kind>) -> Vec<Vec<&ItemFact>> {
    collect(corpus, kind)
        .into_iter()
        .filter(|c| c.score() >= DEFAULT_MIN_SCORE)
        .map(|c| c.members)
        .collect()
}

// unruster: ok(concepts/signature:counted) 2026-08-12 — the check entry-point
// convention: every ranked check takes the context and its own `Opts` and
// returns `Counts`. The bodies are not clones (`clones` reports zero between
// them) — each collects different rows, ranks them differently and writes its
// own summary. The shared thing is the calling convention, which is the point.
pub fn run_counted(ctx: &AnalysisCtx, corpus: &Corpus, opts: &Opts) -> anyhow::Result<Counts> {
    let mut clusters: Vec<Cluster> = collect(corpus, opts.kind);

    // `--changed-since` keeps a cluster when *any* member is in the changed
    // set: the finding is the duplication, and it can be acted on from either
    // end. Same rule `clones` uses, for the same reason.
    if ctx.changed.is_some() {
        clusters.retain(|c| c.members.iter().any(|m| ctx.in_scope(&m.file)));
    }

    let waived = ctx.retain_unsuppressed("concepts", &mut clusters, |c| {
        crate::suppress::Site::keyed(c.first().file.as_str(), c.first().line, &c.label)
    });

    let below = {
        let n = clusters.len();
        clusters.retain(|c| c.score() >= opts.min_score);
        n - clusters.len()
    };

    clusters.sort_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
            .then_with(|| b.members.len().cmp(&a.members.len()))
            .then_with(|| a.first().file.cmp(&b.first().file))
            .then_with(|| a.first().line.cmp(&b.first().line))
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for c in &clusters {
            let first = c.first();
            row!(
                ctx.out,
                "kind" => c.kind,
                "score" => format!("{:.2}", c.score()),
                "n" => c.members.len().to_string(),
                "concept" => c.word.clone().unwrap_or_else(|| first.name.clone()),
                "shape" => c.shape.clone(),
                "at" => site(&first.file, first.line),
                "item" => first.qpath.clone(),
                "others" => c.members[1..]
                    .iter()
                    .map(|m| format!("{} {}:{}", m.qpath, m.file, m.line))
                    .collect::<Vec<_>>()
                    .join("  "),
            );
            ctx.suggest("concepts", Some(&c.label), today);
        }
    }

    let gating = clusters.iter().filter(|c| c.score() >= GATING_SCORE).count();
    let decls: usize = clusters.iter().map(|c| c.members.len()).sum();
    ctx.out.summary(&format!(
        "({} declaration(s) across {} cluster(s){}{}; {} item(s) scanned{}{}; \
         explain: concept-drift)",
        decls,
        clusters.len(),
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
        corpus.items.len(),
        corpus.cache_note(),
        ctx.waived_note(waived)
    ));
    Ok(Counts {
        total: clusters.len(),
        gating,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::FileFacts;

    fn corpus_of(srcs: &[(&str, &str)]) -> Corpus {
        let files: Vec<crate::parse::ParsedFile> = srcs
            .iter()
            .map(|(path, src)| crate::parse::ParsedFile {
                path: std::path::PathBuf::from(path),
                ast: syn::parse_file(src).expect("parse"),
                module: crate::parse::module_of(std::path::Path::new("."), std::path::Path::new(path)),
            })
            .collect();
        let mut c = Corpus::default();
        for f in &files {
            let FileFacts { items, bodies } = crate::facts::derive(f);
            c.items.extend(items);
            c.bodies.extend(bodies);
        }
        c
    }

    #[test]
    fn camel_and_snake_split_the_same_way() {
        assert_eq!(words_of("UserId"), ["user", "id"]);
        assert_eq!(words_of("parse_user_id"), ["parse", "user", "id"]);
        assert_eq!(words_of("HTTPServer"), ["httpserver"]);
        // One-character fragments carry no concept.
        assert_eq!(words_of("T"), Vec::<String>::new());
    }

    /// The finding this whole module exists for.
    #[test]
    fn three_id_newtypes_over_one_primitive_cluster() {
        let c = corpus_of(&[
            ("src/user.rs", "pub struct UserId(u64);"),
            ("src/order.rs", "pub struct OrderId(u64);"),
            ("src/owner.rs", "pub struct OwnerId(u64);"),
        ]);
        let cl = newtype_clusters(&c);
        assert_eq!(cl.len(), 1, "one cluster, on the shared word");
        assert_eq!(cl[0].members.len(), 3);
        assert_eq!(cl[0].word.as_deref(), Some("id"));
        assert!(cl[0].positional, "all three are suffix-cohort names");
        assert!(
            cl[0].score() >= GATING_SCORE,
            "score {:.2} should gate",
            cl[0].score()
        );
    }

    /// The false positive the shared-word rule exists to prevent: unrelated
    /// concepts that happen to wrap the same primitive.
    #[test]
    fn unrelated_newtypes_over_one_primitive_do_not_cluster() {
        let c = corpus_of(&[(
            "src/units.rs",
            "pub struct Meters(f64); pub struct Celsius(f64); pub struct Volts(f64);",
        )]);
        assert!(newtype_clusters(&c).is_empty());
    }

    #[test]
    fn an_alias_and_a_newtype_of_one_concept_land_together() {
        let c = corpus_of(&[
            ("src/a.rs", "pub type UserId = u64;"),
            ("src/b.rs", "pub struct OrderId(u64);"),
        ]);
        let cl = newtype_clusters(&c);
        assert_eq!(cl.len(), 1);
        assert_eq!(cl[0].members.len(), 2);
    }

    #[test]
    fn two_records_with_the_same_fields_cluster() {
        let c = corpus_of(&[
            (
                "src/a.rs",
                "pub struct UserRec { pub id: u64, pub name: String, pub email: String }",
            ),
            (
                "src/b.rs",
                "pub struct Account { pub id: u64, pub name: String, pub email: String }",
            ),
        ]);
        let cl = struct_shape_clusters(&c);
        assert_eq!(cl.len(), 1);
        assert!((cl[0].agreement - 1.0).abs() < 1e-9, "field names all agree");
    }

    /// Two two-field structs of the same primitive types are a coincidence
    /// unless their fields are also called the same things.
    #[test]
    fn a_two_field_coincidence_is_not_a_finding() {
        let c = corpus_of(&[(
            "src/geom.rs",
            "pub struct Point { pub x: f64, pub y: f64 } \
             pub struct Extent { pub w: f64, pub h: f64 }",
        )]);
        assert!(struct_shape_clusters(&c).is_empty());
    }

    #[test]
    fn enums_with_mostly_the_same_variants_pair_up() {
        let c = corpus_of(&[
            ("src/a.rs", "pub enum Status { Idle, Busy, Failed }"),
            ("src/b.rs", "pub enum State { Idle, Busy, Failed, Done }"),
            ("src/c.rs", "pub enum Dir { North, South, East }"),
        ]);
        let cl = enum_shape_clusters(&c);
        assert_eq!(cl.len(), 1, "Dir shares nothing with either");
        assert!(cl[0].agreement >= ENUM_JACCARD);
    }

    /// The gap `clones` leaves: same interface, cognate names, bodies that have
    /// already drifted apart.
    #[test]
    fn cognate_fns_with_one_signature_cluster_even_when_bodies_differ() {
        let c = corpus_of(&[
            (
                "src/a.rs",
                "pub fn parse_user(s: &str) -> Result<u64, E> { s.parse().map_err(E::from) }",
            ),
            (
                "src/b.rs",
                "pub fn parse_owner(s: &str) -> Result<u64, E> { Ok(s.trim().parse()?) }",
            ),
        ]);
        let cl = signature_clusters(&c);
        assert_eq!(cl.len(), 1);
        assert_eq!(cl[0].word.as_deref(), Some("parse"));
    }

    /// And the hand-off: when the bodies *are* identical, `clones` owns the
    /// finding and this check stays quiet rather than reporting it twice.
    #[test]
    fn exact_body_clones_are_left_to_the_clones_check() {
        let body = "{ s.parse().map_err(E::from) }";
        let c = corpus_of(&[
            ("src/a.rs", &format!("pub fn parse_user(s: &str) -> Result<u64, E> {body}")),
            ("src/b.rs", &format!("pub fn parse_owner(s: &str) -> Result<u64, E> {body}")),
        ]);
        assert!(signature_clusters(&c).is_empty());
    }

    #[test]
    fn one_sentence_written_twice_is_a_finding() {
        let c = corpus_of(&[
            (
                "src/a.rs",
                "/// The canonical identifier for a user in this system.\npub struct UserKey(u64);",
            ),
            (
                "src/b.rs",
                "/// The canonical identifier for a user in this system.\npub struct Principal(u64);",
            ),
        ]);
        assert_eq!(doc_clusters(&c).len(), 1);
    }

    #[test]
    fn a_short_doc_repeated_is_english_not_a_finding() {
        let c = corpus_of(&[
            ("src/a.rs", "/// The name.\npub struct A(String);"),
            ("src/b.rs", "/// The name.\npub struct B(String);"),
        ]);
        assert!(doc_clusters(&c).is_empty());
    }

    #[test]
    fn a_pair_in_one_module_reports_but_does_not_gate() {
        let c = corpus_of(&[(
            "src/ids.rs",
            "struct UserId(u64); struct OrderId(u64);",
        )]);
        let cl = newtype_clusters(&c);
        assert_eq!(cl.len(), 1);
        assert!(cl[0].score() >= DEFAULT_MIN_SCORE, "still worth reading");
        assert!(
            cl[0].score() < GATING_SCORE,
            "score {:.2} should not gate",
            cl[0].score()
        );
    }

    #[test]
    fn waiver_keys_are_spellable_inside_a_waiver_comment() {
        let c = corpus_of(&[
            ("src/a.rs", "pub struct UserId(u64);"),
            ("src/b.rs", "pub struct OrderId(u64);"),
        ]);
        for cl in newtype_clusters(&c) {
            assert!(
                !cl.label.contains(char::is_whitespace) && !cl.label.contains(')'),
                "{} is not typeable inside ok(concepts/…)",
                cl.label
            );
        }
    }
}
