use std::collections::HashMap;

use syn::visit::{self, Visit};

use crate::ast::{
    doc_summary, extent_of, has_allow_dead_code, line_of, line_of_span, path_to_string, sig_end,
    type_short, type_to_string, vis_str, Extent, ScopeTracker,
};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Defn {
    /// "struct" | "enum" | "trait" | "fn" | "const" | "static" | "type" |
    /// "mod" | "impl" | "impl-fn" | "trait-fn"
    pub kind: &'static str,
    pub name: String,
    pub qpath: String,
    pub file: String,
    /// The declaration line — the `fn`/`struct`/`impl` itself. Unchanged from
    /// before spans existed, because every consumer keys off it.
    pub line: usize,
    /// First line of the item including its `///` docs and `#[attrs]`.
    pub doc_start: usize,
    /// Last line of the item, closing brace included. This is the number the
    /// `grep | sed -n "$n,+70p"` idiom was guessing at.
    pub end: usize,
    /// Last line of a fn's signature. Equal to `line` for non-fn items.
    pub sig_end: usize,
    /// First line of the item's doc comment, if it has one.
    pub doc: Option<String>,
    /// Nesting inside the file: 0 at top level, +1 per enclosing `mod`, and +1
    /// for a member of an `impl`/`trait` block. What `outline` indents by.
    pub depth: usize,
    pub vis: &'static str,
    pub module: String,
    /// For impl-fn: the self-type name. For trait-fn: the trait name. For "impl"
    /// header: the self-type. None for free items.
    pub owner: Option<String>,
    /// For "impl" headers: the trait being implemented, if any.
    pub trait_name: Option<String>,
    /// True for impl-fn defns whose enclosing `impl` block is a trait impl
    /// (i.e. `impl SomeTrait for T { fn ... }`). False for inherent impls and
    /// for non-fn defns. Used by `dead-code` to skip dynamically-dispatched
    /// trait methods.
    pub in_trait_impl: bool,
    /// True if the item (or its enclosing impl block) carries `#[allow(dead_code)]`.
    /// `dead-code` skips these to respect the author's explicit opt-out.
    pub allow_dead: bool,
}

pub struct NameIndex {
    pub defns: Vec<Defn>,
    by_last: HashMap<String, Vec<usize>>,
}

#[allow(dead_code)]
impl NameIndex {
    pub fn build(files: &[ParsedFile]) -> Self {
        let mut defns: Vec<Defn> = Vec::new();
        for f in files {
            let mut v = IndexVisitor {
                file: &display_path(&f.path),
                scope: ScopeTracker::new(f.module.as_str()),
                out: &mut defns,
            };
            v.visit_file(&f.ast);
        }
        let mut by_last: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, d) in defns.iter().enumerate() {
            by_last.entry(d.name.clone()).or_default().push(i);
        }
        NameIndex { defns, by_last }
    }

    /// Lookup by bare name or qualified suffix (`Type` or `module::Type`).
    /// For bare names, returns all defns whose last segment matches.
    /// For qualified, returns defns whose qpath ends with the query.
    pub fn lookup(&self, query: &str) -> Vec<&Defn> {
        let last = crate::ast::last_segment(query);
        let Some(ids) = self.by_last.get(last) else {
            return Vec::new();
        };
        if query.contains("::") {
            let suffix = format!("::{}", query);
            ids.iter()
                .filter_map(|&i| {
                    let d = &self.defns[i];
                    if d.qpath == query || d.qpath.ends_with(&suffix) {
                        Some(d)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            ids.iter().map(|&i| &self.defns[i]).collect()
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Defn> {
        self.defns.iter()
    }

    /// True if any definition with the given last-segment name exists.
    pub fn knows_name(&self, last: &str) -> bool {
        self.by_last.contains_key(last)
    }

    /// Definitions whose name is *close to* `query` — the answer to a lookup
    /// that found nothing.
    ///
    /// This exists because of what a miss costs. An agent that greps for a name
    /// it half-remembers (`contour_of` for `contour_points`) gets an empty
    /// result, and an empty result is indistinguishable from "this codebase has
    /// no such concept" — so the next move is another search, and another. One
    /// `did you mean` line ends that loop on the first call.
    ///
    /// Ranked best-first by [`similarity`] on the last path segment. Returns at
    /// most one representative per distinct name, so a method defined in four
    /// impls does not crowd out the other candidates.
    pub fn similar(&self, query: &str, limit: usize) -> Vec<&Defn> {
        let want = crate::ast::last_segment(query).to_lowercase();
        // A one- or two-letter query is close to everything; suggesting from it
        // would be noise wearing the shape of an answer.
        if want.chars().count() < 3 {
            return Vec::new();
        }
        let mut scored: Vec<(f64, &str, &Defn)> = Vec::new();
        for (name, ids) in &self.by_last {
            let sim = similarity(&name.to_lowercase(), &want);
            if sim < SIMILAR_ENOUGH {
                continue;
            }
            let Some(&first) = ids.first() else { continue };
            scored.push((sim, name.as_str(), &self.defns[first]));
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.truncate(limit);
        scored.into_iter().map(|(_, _, d)| d).collect()
    }

    /// Sorted, deduplicated names of every enum defined in the tree.
    /// Used by the `--all` modes of the enum-target commands.
    pub fn enum_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .defns
            .iter()
            .filter(|d| d.kind == "enum")
            .map(|d| d.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// One item about to be recorded. A struct rather than eight positional
/// arguments: `push(kind, name, vis, doc_start, decl, end, sig_end, doc, …)`
/// is a call nobody can read, and two of those numbers being swapped is a
/// defect that shows up as an off-by-a-few source range rather than as a
/// compile error.
struct Spot {
    kind: &'static str,
    name: String,
    vis: &'static str,
    ext: Extent,
    /// Defaults to `ext.decl`; only fns set it.
    sig_end: usize,
    doc: Option<String>,
    allow_dead: bool,
}

impl Spot {
    /// A non-fn item: signature end is the declaration line.
    fn item(kind: &'static str, name: String, vis: &'static str, ext: Extent) -> Spot {
        Spot {
            kind,
            name,
            vis,
            ext,
            sig_end: ext.decl,
            doc: None,
            allow_dead: false,
        }
    }

    fn doc(mut self, attrs: &[syn::Attribute]) -> Spot {
        self.doc = doc_summary(attrs);
        self
    }

    fn sig(mut self, sig: &syn::Signature) -> Spot {
        self.sig_end = sig_end(sig, self.ext.decl);
        self
    }

    fn allow_dead(mut self, on: bool) -> Spot {
        self.allow_dead = on;
        self
    }
}

/// How close two names must be before one is offered as a correction. Tuned
/// against the misses that actually happen: `contour_of` → `contour_points`
/// (0.74) and `heading_match` → `heading_matches` (0.92) must clear it, while
/// `contour_of` → `of` (0.20) and any one-letter name must not. A suggestion
/// list that includes `C` teaches the reader to stop reading suggestion lists.
const SIMILAR_ENOUGH: f64 = 0.6;

/// How alike two identifiers are, in `0.0..=1.0`.
///
/// Plain edit distance is the wrong measure for identifiers and fails on the
/// exact case this exists for: `contour_of` and `contour_points` are six edits
/// apart — further than a two-edit budget allows — yet no reader would call
/// them unrelated. Two corrections fix that, both keyed to how identifiers are
/// actually mistyped or half-remembered:
///
/// - a **prefix boost** (Winkler's), because a name recalled from its opening
///   is the common failure — `draft…` for `draft_regions`;
/// - a **containment floor**, because a remembered fragment is the other one —
///   `parse` for `window_parse`, where the prefix boost gives nothing.
///
/// Containment requires three characters. Without that floor every single-letter
/// constant in the tree is "contained in" every query and scores perfectly.
fn similarity(a: &str, b: &str) -> f64 {
    let (la, lb) = (a.chars().count(), b.chars().count());
    let (min, max) = (la.min(lb), la.max(lb));
    if max == 0 {
        return 0.0;
    }
    let mut sim = 1.0 - edit_distance(a, b) as f64 / max as f64;
    let prefix = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count().min(4);
    sim += 0.1 * prefix as f64 * (1.0 - sim);
    if min >= 3 && (a.contains(b) || b.contains(a)) {
        sim = sim.max(0.5 + 0.5 * min as f64 / max as f64);
    }
    sim
}

/// Optimal string alignment distance — Levenshtein plus a transposition edit.
///
/// The transposition is not a refinement, it is the difference between working
/// and not. Swapping two adjacent characters is the single most common typo,
/// and plain Levenshtein charges *two* edits for it: `nwe` against `new` scores
/// the same as a name sharing only its first letter, so the one suggestion
/// worth making falls below any useful threshold. This exact case is what the
/// first cut of `similar` failed on.
///
/// Operates on `char`s rather than bytes, so a non-ASCII identifier is measured
/// in characters and not in UTF-8 lengths.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let n = b.len();
    // Three rows: a transposition reaches back two rows, which the usual
    // two-row Levenshtein does not keep.
    let mut prev2 = vec![0usize; n + 1];
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur = vec![0usize; n + 1];
    for i in 0..a.len() {
        cur[0] = i + 1;
        for j in 0..n {
            let cost = usize::from(a[i] != b[j]);
            let mut v = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            if i > 0 && j > 0 && a[i] == b[j - 1] && a[i - 1] == b[j] {
                v = v.min(prev2[j - 1] + 1);
            }
            cur[j + 1] = v;
        }
        // prev2 ← prev (row i), prev ← cur (row i+1), cur ← scratch.
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

struct IndexVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    out: &'a mut Vec<Defn>,
}

impl<'a> IndexVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        self.scope.qualify(name)
    }

    // `push()` is only invoked for top-level items (struct/enum/trait/fn/etc.),
    // never during impl/trait iteration — those construct Defns directly. So
    // the "current owner" at push time is always None.
    fn current_owner(&self) -> Option<String> {
        None
    }

    fn current_module(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.scope.module.is_empty() {
            parts.push(self.scope.module.clone());
        }
        parts.extend(self.scope.mod_stack.iter().cloned());
        parts.join("::")
    }

    /// How deeply nested the *current* scope is. Read before `enter_*`, so an
    /// item is measured by what encloses it rather than by what it opens.
    fn depth(&self) -> usize {
        self.scope.mod_stack.len() + self.scope.impl_stack.len() + self.scope.trait_stack.len()
    }

    /// A `Defn` in the current scope, for the members that `impl`/`trait`
    /// iteration builds by hand (they set `owner`/`in_trait_impl` afterwards).
    fn defn(&self, s: Spot) -> Defn {
        Defn {
            kind: s.kind,
            qpath: self.qualify(&s.name),
            name: s.name,
            file: self.file.to_string(),
            line: s.ext.decl,
            doc_start: s.ext.doc_start,
            end: s.ext.end,
            sig_end: s.sig_end,
            doc: s.doc,
            depth: self.depth(),
            vis: s.vis,
            module: self.current_module(),
            owner: self.current_owner(),
            trait_name: None,
            in_trait_impl: false,
            allow_dead: s.allow_dead,
        }
    }

    fn push(&mut self, s: Spot) {
        let d = self.defn(s);
        self.out.push(d);
    }
}

impl<'ast, 'a> Visit<'ast> for IndexVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let name = i.ident.to_string();
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("mod", name.clone(), vis_str(&i.vis), ext).doc(&i.attrs));
        self.scope.enter_mod(name);
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("struct", i.ident.to_string(), vis_str(&i.vis), ext).doc(&i.attrs));
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("enum", i.ident.to_string(), vis_str(&i.vis), ext).doc(&i.attrs));
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.to_string();
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("trait", name.clone(), vis_str(&i.vis), ext).doc(&i.attrs));
        self.scope.enter_trait(name);
        for item in &i.items {
            if let syn::TraitItem::Fn(f) = item {
                let ext = extent_of(f, &f.attrs, line_of(&f.sig.ident));
                let spot = Spot::item("trait-fn", f.sig.ident.to_string(), "pub", ext)
                    .doc(&f.attrs)
                    .sig(&f.sig)
                    .allow_dead(has_allow_dead_code(&f.attrs));
                let mut d = self.defn(spot);
                d.owner = self.scope.trait_stack.last().cloned();
                self.out.push(d);
            }
        }
        self.scope.leave_trait();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        let ext = extent_of(i, &i.attrs, line_of(&i.sig.ident));
        self.push(
            Spot::item("fn", i.sig.ident.to_string(), vis_str(&i.vis), ext)
                .doc(&i.attrs)
                .sig(&i.sig)
                .allow_dead(has_allow_dead_code(&i.attrs)),
        );
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("const", i.ident.to_string(), vis_str(&i.vis), ext).doc(&i.attrs));
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("static", i.ident.to_string(), vis_str(&i.vis), ext).doc(&i.attrs));
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push(Spot::item("type", i.ident.to_string(), vis_str(&i.vis), ext).doc(&i.attrs));
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let self_ty = type_short(&i.self_ty);
        let trait_name = i.trait_.as_ref().and_then(|(_, p, _)| {
            p.segments.last().map(|s| s.ident.to_string())
        });
        let header = match &i.trait_ {
            Some((bang, trait_path, _)) => {
                let prefix = if bang.is_some() { "!" } else { "" };
                format!(
                    "impl {}{} for {}",
                    prefix,
                    path_to_string(trait_path),
                    type_to_string(&i.self_ty)
                )
            }
            None => format!("impl {}", type_to_string(&i.self_ty)),
        };
        let is_trait_impl = trait_name.is_some();
        let impl_block_allow = has_allow_dead_code(&i.attrs);
        let ext = extent_of(i, &i.attrs, line_of_span(i.impl_token.span));
        // The impl header's `qpath` is the rendered `impl Trait for Type`
        // line, not a path — so it is built here rather than by `qualify`.
        let mut d = self.defn(
            Spot::item("impl", self_ty.clone(), "—", ext)
                .doc(&i.attrs)
                .allow_dead(impl_block_allow),
        );
        d.qpath = header;
        d.owner = Some(self_ty.clone());
        d.trait_name = trait_name;
        self.out.push(d);
        self.scope.enter_impl(self_ty);
        for item in &i.items {
            if let syn::ImplItem::Fn(f) = item {
                let ext = extent_of(f, &f.attrs, line_of(&f.sig.ident));
                let spot = Spot::item("impl-fn", f.sig.ident.to_string(), vis_str(&f.vis), ext)
                    .doc(&f.attrs)
                    .sig(&f.sig)
                    .allow_dead(impl_block_allow || has_allow_dead_code(&f.attrs));
                let mut d = self.defn(spot);
                d.owner = self.scope.impl_stack.last().cloned();
                d.in_trait_impl = is_trait_impl;
                self.out.push(d);
            }
        }
        self.scope.leave_impl();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: &str, b: &str) -> bool {
        similarity(a, b) >= SIMILAR_ENOUGH
    }

    #[test]
    fn edit_distance_counts_characters_not_utf8_bytes() {
        // `é` is two bytes; a byte-wise DP would call this distance 2.
        assert_eq!(edit_distance("café", "cafe"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    #[test]
    fn a_swapped_pair_is_one_edit_not_two() {
        // Plain Levenshtein charges 2 here, which drops `new` below the
        // threshold for a query of `nwe` — the commonest typo there is.
        assert_eq!(edit_distance("new", "nwe"), 1);
        assert_eq!(edit_distance("parse", "prase"), 1);
        assert!(near("new", "nwe"));
    }

    #[test]
    fn the_miss_that_motivated_this_resolves() {
        // From an agent transcript: it grepped for `contour_of`, the fn was
        // `contour_points`, and the empty result cost a whole turn. Six edits
        // apart — no plain-distance budget small enough to be useful would
        // admit it, which is why `similarity` has a prefix boost.
        assert!(near("contour_points", "contour_of"));
        assert!(near("draft_regions", "draft"));
        assert!(near("heading_matches", "heading_match"));
    }

    #[test]
    fn a_remembered_fragment_finds_the_whole_name() {
        // No shared prefix, so the boost gives nothing — this is the
        // containment floor's case.
        assert!(near("window_parse", "parse"));
    }

    #[test]
    fn single_letter_names_are_not_close_to_everything() {
        // Every one-letter const is "contained in" every query. Without the
        // three-character floor these score perfectly and crowd out the real
        // suggestion — which is what the first cut of this actually did.
        assert!(!near("c", "contour_of"));
        assert!(!near("n", "heading_match"));
        assert!(!near("of", "contour_of"));
    }

    #[test]
    fn unrelated_names_stay_unrelated() {
        assert!(!near("foo", "bar"));
        assert!(!near("write_baseline", "contour_of"));
    }

    #[test]
    fn a_query_too_short_to_discriminate_suggests_nothing() {
        let idx = NameIndex {
            defns: Vec::new(),
            by_last: HashMap::new(),
        };
        assert!(idx.similar("ab", 8).is_empty());
    }
}
