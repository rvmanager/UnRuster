//! `gate` — ask what already exists *before* the code is written.
//!
//! # The one command in this tool that runs before the edit
//!
//! Every other check here is post-hoc: the duplicate gets written, and the next
//! `audit` finds it. That is the right shape for cleaning up a tree, and it is
//! the wrong shape for the failure mode that produces most concept drift now.
//! An agent has a keyhole view of a codebase. It writes `AccountId` because it
//! never opened the module holding `UserId` — not from carelessness, but
//! because "what already means this?" is a question it had no cheap way to ask.
//!
//! This command is that question, answered in one call:
//!
//! ```text
//! unruster gate --snippet 'pub fn parse_user_id(s: &str) -> Result<UserId>'
//! unruster gate --file src/new_thing.rs
//! unruster gate UserId --kind struct
//! unruster gate --hook          # reads a PreToolUse event on stdin
//! ```
//!
//! # Why it is a join, not a new analysis
//!
//! Nothing here computes anything the tool could not already compute. A
//! proposal is reduced to [`crate::facts`] — the same shapes, signatures, docs
//! and body skeletons every other check works from — and then looked up against
//! the corpus five ways. The only new engineering is [`crate::cache`], because
//! a gate that re-parses the tree on every `Write` is a gate somebody switches
//! off within the hour.
//!
//! # Blocking, and the reason it blocks only once
//!
//! The post-hoc checks have an escape hatch: a site you have judged
//! intentional gets a `// unruster: ok(…)` waiver and stops being reported.
//! A pre-hoc gate has nowhere to put one — the code does not exist yet — so a
//! gate that simply denies an edit leaves an agent with a correct-but-deliberate
//! collision no way to say so, and the only move left is to route around the
//! tool.
//!
//! So the default is **warn-once**: the first proposal that collides is
//! stopped, with the existing declarations named; an identical proposal
//! immediately afterwards is allowed through. The reader is told exactly once,
//! which is the whole job, and the second attempt is the acknowledgment.
//! `UNRUSTER_GATE=block` makes it absolute and `UNRUSTER_GATE=off` disables it.

use std::collections::BTreeSet;
use std::path::Path;

use crate::corpus::Corpus;
use crate::emit::{row, site, Out};
use crate::facts::{FileFacts, ItemFact, Shape};

/// How strongly the corpus objects to a proposal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Verdict {
    /// Nothing in the tree looks like this.
    Clear,
    /// Something similar exists. Worth reading before writing.
    Warn,
    /// The same thing exists, by name or by body.
    Collide,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Clear => "clear",
            Verdict::Warn => "warn",
            Verdict::Collide => "collide",
        }
    }
}

/// One reason a proposal is not new.
struct Hit<'a> {
    verdict: Verdict,
    /// `name` | `near-name` | `shape` | `signature` | `doc` | `body`
    kind: &'static str,
    /// The proposed item this is about.
    proposed: String,
    existing: &'a ItemFact,
    why: String,
}

// ──────────────────────────────────────────────────────────────────────────
// Matching

/// Everything in the corpus that could already be `p`.
fn hits_for<'a>(c: &'a Corpus, p: &ItemFact, proposal: &FileFacts) -> Vec<Hit<'a>> {
    let mut out: Vec<Hit<'a>> = Vec::new();
    let label = format!("{} {}", p.kind, p.name);

    // 1. The name is taken. The strongest answer there is, and the one an agent
    //    most often does not have: it is looking at three files, and the name
    //    lives in a fourth.
    for e in c.named(&p.name) {
        // Re-running the gate over a file already on disk would otherwise
        // report every item in it as colliding with itself.
        if e.file == p.file && e.line == p.line {
            continue;
        }
        out.push(Hit {
            verdict: Verdict::Collide,
            kind: "name",
            proposed: label.clone(),
            existing: e,
            why: format!("`{}` is already declared here as {}", p.name, e.kind),
        });
    }

    // 2. A name close enough that one of the two is probably the other. Skipped
    //    once an exact match is in hand — a "did you mean" beside a definite
    //    answer is noise.
    if out.is_empty() {
        let names: BTreeSet<&str> = c.declarations().map(|i| i.name.as_str()).collect();
        // Three, not more. This is the softest of the six matchers and it fires
        // on almost any name; a long "did you mean" list under a definite
        // answer teaches a reader to stop reading the list.
        let near = crate::index::closest(&p.name, names, 3);
        for n in near {
            if n == p.name {
                continue;
            }
            if let Some(e) = c.named(n).next() {
                out.push(Hit {
                    verdict: Verdict::Warn,
                    kind: "near-name",
                    proposed: label.clone(),
                    existing: e,
                    why: format!("`{}` is a near-spelling of the proposed name", n),
                });
            }
        }
    }

    // 3. The same shape under another name — the concept-drift case, asked one
    //    candidate at a time instead of corpus-wide.
    match &p.shape {
        Shape::Tuple(v) if v.len() == 1 => {
            for e in c.declarations() {
                if e.shape.newtype_inner() != Some(v[0].as_str()) || e.name == p.name {
                    continue;
                }
                // Generic API words are *kept* here, as in `concepts --kind
                // newtype`: for a wrapper type the shared word usually is the
                // concept (`Id`, `Key`, `Name`).
                let shared = crate::concepts::cognate_words(&p.name, &e.name, false);
                if shared.is_empty() {
                    continue;
                }
                out.push(Hit {
                    verdict: Verdict::Warn,
                    kind: "shape",
                    proposed: label.clone(),
                    existing: e,
                    why: format!(
                        "also wraps `{}` and shares the word `{}`",
                        v[0],
                        shared.join("/")
                    ),
                });
            }
        }
        Shape::Fields(f) if f.len() >= 2 => {
            let mut want: Vec<&str> = f.iter().map(|(_, t)| t.as_str()).collect();
            want.sort_unstable();
            for e in c.of_kind("struct") {
                let Shape::Fields(g) = &e.shape else { continue };
                if g.len() != f.len() || e.name == p.name {
                    continue;
                }
                let mut have: Vec<&str> = g.iter().map(|(_, t)| t.as_str()).collect();
                have.sort_unstable();
                if have != want {
                    continue;
                }
                out.push(Hit {
                    verdict: Verdict::Warn,
                    kind: "shape",
                    proposed: label.clone(),
                    existing: e,
                    why: format!("has the same {} field types", f.len()),
                });
            }
        }
        // 4. The same interface under a cognate name. Requiring the shared word
        //    is what keeps `(&str) -> Result<T>` — every parser in the tree —
        //    from matching every proposal.
        Shape::Signature { params, ret, .. } if !(params.is_empty() && ret == "()") => {
            for e in c.declarations() {
                if e.name == p.name || e.in_trait_impl {
                    continue;
                }
                let Shape::Signature {
                    params: ep,
                    ret: er,
                    ..
                } = &e.shape
                else {
                    continue;
                };
                if ep != params || er != ret {
                    continue;
                }
                // Generic API words dropped, exactly as `concepts --kind
                // signature` drops them: `(&str) -> &str` sharing the word `of`
                // is every path helper in the tree.
                let shared = crate::concepts::cognate_words(&p.name, &e.name, true);
                if shared.is_empty() {
                    continue;
                }
                out.push(Hit {
                    verdict: Verdict::Warn,
                    kind: "signature",
                    proposed: label.clone(),
                    existing: e,
                    why: format!(
                        "same signature ({}) -> {}, shares `{}`",
                        params.join(", "),
                        ret,
                        shared.join("/")
                    ),
                });
            }
        }
        _ => {}
    }

    // 5. The same sentence. Somebody already described this concept.
    if let Some(d) = &p.doc {
        let norm = crate::concepts::normalize_doc(d);
        if norm.split(' ').filter(|w| !w.is_empty()).count() >= 6 {
            for e in c.declarations() {
                if e.name == p.name {
                    continue;
                }
                if e.doc.as_deref().map(crate::concepts::normalize_doc).as_deref() == Some(norm.as_str()) {
                    out.push(Hit {
                        verdict: Verdict::Warn,
                        kind: "doc",
                        proposed: label.clone(),
                        existing: e,
                        why: "documented with the same sentence".to_string(),
                    });
                }
            }
        }
    }

    // 6. The body is already in the tree, exactly or nearly. This is the one
    //    match that needs the proposal's own bodies rather than its items.
    for b in &proposal.bodies {
        if b.name != p.name || b.tokens < crate::near_clones::DEFAULT_MIN_TOKENS {
            continue;
        }
        for e in &c.bodies {
            if e.file == b.file && e.line == b.line {
                continue;
            }
            if e.skeleton != b.skeleton || e.leaves.len() != b.leaves.len() {
                continue;
            }
            let diffs = e
                .leaves
                .iter()
                .zip(b.leaves.iter())
                .filter(|(x, y)| x != y)
                .count();
            if diffs > crate::near_clones::DEFAULT_MAX_DIFF {
                continue;
            }
            // The existing *item*, so the row points at something a reader can
            // open by name rather than at a bare line.
            let Some(item) = c
                .items
                .iter()
                .find(|i| i.file == e.file && i.line == e.line)
            else {
                continue;
            };
            out.push(Hit {
                verdict: if diffs == 0 {
                    Verdict::Collide
                } else {
                    Verdict::Warn
                },
                kind: "body",
                proposed: label.clone(),
                existing: item,
                why: if diffs == 0 {
                    "this exact body already exists".to_string()
                } else {
                    format!("body differs in only {} leaf/leaves", diffs)
                },
            });
        }
    }

    out.sort_by(|a, b| {
        b.verdict
            .cmp(&a.verdict)
            .then_with(|| a.existing.file.cmp(&b.existing.file))
            .then_with(|| a.existing.line.cmp(&b.existing.line))
    });
    out.dedup_by(|a, b| {
        a.kind == b.kind && a.existing.file == b.existing.file && a.existing.line == b.existing.line
    });
    out
}


// ──────────────────────────────────────────────────────────────────────────
// Turning a proposal into facts

/// Parse a proposal into the same facts an on-disk file would produce.
///
/// Three spellings are accepted because three things send proposals here: a
/// whole file (`Write`), a fragment (`Edit`'s replacement text), and a bare
/// signature typed by a human. A fragment that is not a sequence of items is
/// not an error — there is simply nothing declared to check, and saying so is
/// better than refusing the edit over a parse this command never needed.
fn facts_of_snippet(text: &str, as_path: &str) -> Option<FileFacts> {
    let ast = syn::parse_file(text)
        .ok()
        // `pub fn f(x: A) -> B` with no body: the natural thing a person types,
        // and not a valid file. Give it one.
        .or_else(|| syn::parse_file(&format!("{} {{ unimplemented!() }}", text.trim())).ok())?;
    let path = std::path::PathBuf::from(as_path);
    let module = crate::parse::module_of(Path::new("."), &path);
    Some(crate::facts::derive(&crate::parse::ParsedFile { path, ast, module }))
}

/// A bare `unruster gate UserId --kind struct` — a name with no declaration.
fn facts_of_name(name: &str, kind: &str) -> FileFacts {
    FileFacts {
        items: vec![ItemFact {
            kind: kind.to_string(),
            name: name.to_string(),
            qpath: name.to_string(),
            module: String::new(),
            file: "<proposed>".to_string(),
            line: 0,
            end: 0,
            vis: "pub".to_string(),
            doc: None,
            shape: Shape::Opaque,
            in_trait_impl: false,
            local: false,
            concept: None,
        }],
        bodies: Vec::new(),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Run

pub struct Opts {
    pub snippet: Option<String>,
    pub file: Option<std::path::PathBuf>,
    pub name: Option<String>,
    pub kind: String,
    /// Report `warn`-tier hits too. On by default; `--collisions-only` narrows
    /// it to the answers nobody argues with.
    pub warnings: bool,
}

/// Gather the proposal's facts from whichever input was given.
pub fn proposal_of(opts: &Opts) -> anyhow::Result<Option<FileFacts>> {
    if let Some(p) = &opts.file {
        let text = std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {}", p.display(), e))?;
        return Ok(facts_of_snippet(&text, &crate::parse::display_path(p)));
    }
    if let Some(s) = &opts.snippet {
        return Ok(facts_of_snippet(s, "<proposed>.rs"));
    }
    if let Some(n) = &opts.name {
        return Ok(Some(facts_of_name(n, &opts.kind)));
    }
    anyhow::bail!("nothing to check: pass a name, --snippet, --file, or --hook")
}

/// Run the gate and emit rows. Returns `(findings, worst verdict)`.
pub fn run(
    out: &Out,
    corpus: &Corpus,
    proposal: &FileFacts,
    opts: &Opts,
    summary: bool,
) -> (usize, Verdict) {
    let mut all: Vec<Hit> = Vec::new();
    for p in &proposal.items {
        all.extend(hits_for(corpus, p, proposal));
    }
    if !opts.warnings {
        all.retain(|h| h.verdict == Verdict::Collide);
    }
    let worst = all
        .iter()
        .map(|h| h.verdict)
        .max()
        .unwrap_or(Verdict::Clear);

    if !summary {
        for h in &all {
            row!(
                out,
                "verdict" => h.verdict.as_str(),
                "kind" => h.kind,
                "proposed" => h.proposed.clone(),
                "why" => h.why.clone(),
                "at" => site(&h.existing.file, h.existing.line),
                "existing" => h.existing.qpath.clone(),
            );
        }
    }
    out.summary(&format!(
        "({} proposed item(s); {} existing declaration(s) may already be one of them; \
         verdict={}{}; {} item(s) in corpus{})",
        proposal.items.len(),
        all.len(),
        worst.as_str(),
        if opts.warnings { "" } else { " (collisions only)" },
        corpus.items.len(),
        corpus.cache_note()
    ));
    (all.len(), worst)
}

/// One line per hit, for the hook's feedback text. Deliberately not the TSV
/// rows: this is read by a model mid-edit, so it has to be a sentence naming a
/// path it can open.
pub fn brief(corpus: &Corpus, proposal: &FileFacts) -> Vec<String> {
    let mut lines = Vec::new();
    for p in &proposal.items {
        for h in hits_for(corpus, p, proposal) {
            lines.push(format!(
                "  {} [{}] {} — {} ({}:{})",
                h.proposed, h.kind, h.existing.qpath, h.why, h.existing.file, h.existing.line
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_of(srcs: &[(&str, &str)]) -> Corpus {
        let mut c = Corpus::default();
        for (path, src) in srcs {
            let pf = crate::parse::ParsedFile {
                path: std::path::PathBuf::from(path),
                ast: syn::parse_file(src).expect("parse"),
                module: crate::parse::module_of(Path::new("."), Path::new(path)),
            };
            let f = crate::facts::derive(&pf);
            c.items.extend(f.items);
            c.bodies.extend(f.bodies);
        }
        c
    }

    fn check(c: &Corpus, snippet: &str) -> (Verdict, Vec<String>) {
        let p = facts_of_snippet(snippet, "<proposed>.rs").expect("parses");
        let mut worst = Verdict::Clear;
        let mut kinds = Vec::new();
        for item in &p.items {
            for h in hits_for(c, item, &p) {
                worst = worst.max(h.verdict);
                kinds.push(h.kind.to_string());
            }
        }
        (worst, kinds)
    }

    #[test]
    fn a_name_already_in_the_tree_collides() {
        let c = corpus_of(&[("src/ids.rs", "pub struct UserId(u64);")]);
        let (v, kinds) = check(&c, "pub struct UserId(u32);");
        assert_eq!(v, Verdict::Collide);
        assert!(kinds.contains(&"name".to_string()));
    }

    /// The failure this command exists for: the agent invents a second name for
    /// a concept whose first name lives in a file it never opened.
    #[test]
    fn a_second_name_for_an_existing_concept_warns() {
        let c = corpus_of(&[("src/ids.rs", "pub struct UserId(u64);")]);
        let (v, kinds) = check(&c, "pub struct AccountId(u64);");
        assert_eq!(v, Verdict::Warn);
        assert!(kinds.contains(&"shape".to_string()), "{kinds:?}");
    }

    #[test]
    fn an_unrelated_newtype_over_the_same_primitive_is_clear() {
        let c = corpus_of(&[("src/ids.rs", "pub struct UserId(u64);")]);
        let (v, _) = check(&c, "pub struct ByteOffset(u64);");
        assert_eq!(v, Verdict::Clear);
    }

    #[test]
    fn a_cognate_fn_with_the_same_signature_warns() {
        let c = corpus_of(&[(
            "src/a.rs",
            "pub fn parse_user(s: &str) -> Result<u64, E> { s.parse().map_err(E::from) }",
        )]);
        let (v, kinds) = check(&c, "pub fn parse_owner(s: &str) -> Result<u64, E> { todo!() }");
        assert_eq!(v, Verdict::Warn);
        assert!(kinds.contains(&"signature".to_string()), "{kinds:?}");
    }

    /// A bodyless signature is what a person types when they ask "does this
    /// exist yet?", and it is not a valid Rust file.
    #[test]
    fn a_bare_signature_with_no_body_still_parses() {
        let c = corpus_of(&[(
            "src/a.rs",
            "pub fn parse_user(s: &str) -> Result<u64, E> { s.parse().map_err(E::from) }",
        )]);
        let (v, _) = check(&c, "pub fn parse_owner(s: &str) -> Result<u64, E>");
        assert_eq!(v, Verdict::Warn);
    }

    #[test]
    fn a_body_already_in_the_tree_collides() {
        let body = "{ let n = s.trim().len(); if n > 10 { return Err(E::Long); } Ok(Id(n as u64)) }";
        let c = corpus_of(&[("src/a.rs", &format!("pub fn make(s: &str) -> Result<Id, E> {body}"))]);
        let (v, kinds) = check(&c, &format!("pub fn make(s: &str) -> Result<Id, E> {body}"));
        assert_eq!(v, Verdict::Collide);
        assert!(kinds.contains(&"body".to_string()), "{kinds:?}");
    }

    #[test]
    fn a_genuinely_new_item_is_clear() {
        let c = corpus_of(&[("src/a.rs", "pub struct UserId(u64);")]);
        let (v, kinds) = check(&c, "pub struct RetryPolicy { pub attempts: u8, pub backoff: Duration }");
        assert_eq!(v, Verdict::Clear, "{kinds:?}");
    }

    /// Re-gating a file that is already on disk must not report every item in
    /// it as colliding with itself.
    #[test]
    fn an_item_does_not_collide_with_its_own_declaration() {
        let src = "pub struct UserId(u64);";
        let c = corpus_of(&[("src/ids.rs", src)]);
        let p = facts_of_snippet(src, "src/ids.rs").expect("parses");
        let hits = hits_for(&c, &p.items[0], &p);
        assert!(hits.is_empty(), "{:?}", hits.iter().map(|h| h.kind).collect::<Vec<_>>());
    }

    #[test]
    fn a_fragment_that_is_not_a_declaration_yields_nothing_rather_than_an_error() {
        assert!(facts_of_snippet("let x = 1;", "<proposed>.rs").is_none());
    }
}
