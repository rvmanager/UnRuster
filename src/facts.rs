//! Per-file derived facts: the shared substrate for the noun-axis checks and
//! the pre-write gate.
//!
//! # Why this exists rather than another visitor
//!
//! Every check in this tool walks the `syn` AST itself, which is right when the
//! check is the only consumer of what it collects. Three consumers now want the
//! *same* three collections — every item's declared shape, every fn's
//! signature, and every body's canonical token structure — and two of them
//! ([`crate::concepts`], [`crate::near_clones`]) ask corpus-wide questions
//! while the third ([`crate::gate`]) asks a single-candidate question against
//! the same corpus. Three visitors producing three subtly different notions of
//! "the shape of this struct" is the exact defect this tool reports about other
//! codebases.
//!
//! # Why it is separable from the AST
//!
//! Facts are plain data: no spans, no `syn` handles, no lifetimes. That is what
//! makes them cacheable ([`crate::cache`]), and the cache is what makes the
//! pre-write gate viable — a gate that re-parses the tree on every `Write` is a
//! gate nobody leaves switched on.
//!
//! # Precision
//!
//! Shapes are **syntactic**. `Vec<T>` and `alloc::vec::Vec<T>` are different
//! strings here, and a type alias is whatever it was spelled as. That is the
//! same cut every other check in this tool makes, and it is why a shape match
//! is a *candidate*: two structs whose fields print identically are worth
//! comparing, not proven identical.

use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::ast::{
    doc_summary, extent_of, line_of, scope_visits, type_to_string, vis_str, ScopeTracker,
};
use crate::parse::{display_path, ParsedFile};

/// Field separator inside a serialized [`Shape`]. A `\u{1}` cannot occur in
/// Rust source text (idents, types and one-line doc summaries are all
/// control-char-free), so no escaping is needed at this level.
const SUB: char = '\u{1}';
/// Separator between the two halves of a named field's `name:type` pair.
const SUB2: char = '\u{2}';

/// What an item declares, reduced to the part two items can be compared on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// Tuple struct — the inner types in order. A one-element `Tuple` is a
    /// newtype, which is the case the concept checks care most about.
    Tuple(Vec<String>),
    /// Named-field struct: `(field, type)` in source order.
    Fields(Vec<(String, String)>),
    /// Enum: variant names in source order.
    Variants(Vec<String>),
    /// Function: parameter types (receiver excluded, recorded in `has_self`)
    /// and return type. `ret` is `"()"` for a bare `fn f()`, so the unit case
    /// and the "no return type written" case compare equal — which is what a
    /// reader means by "same signature".
    Signature {
        params: Vec<String>,
        ret: String,
        has_self: bool,
    },
    /// Traits, mods, consts — indexed for name lookup, not compared by shape.
    Opaque,
}

impl Shape {
    /// `(tag, payload)`, the on-disk encoding.
    fn encode(&self) -> (char, String) {
        match self {
            Shape::Tuple(v) => ('T', v.join(&SUB.to_string())),
            Shape::Fields(v) => (
                'F',
                v.iter()
                    .map(|(n, t)| format!("{}{}{}", n, SUB2, t))
                    .collect::<Vec<_>>()
                    .join(&SUB.to_string()),
            ),
            Shape::Variants(v) => ('V', v.join(&SUB.to_string())),
            Shape::Signature {
                params,
                ret,
                has_self,
            } => (
                'S',
                format!(
                    "{}{}{}{}{}",
                    if *has_self { "1" } else { "0" },
                    SUB,
                    ret,
                    SUB,
                    params.join(&SUB.to_string())
                ),
            ),
            Shape::Opaque => ('O', String::new()),
        }
    }

    fn decode(tag: char, payload: &str) -> Shape {
        // An empty payload means an empty list, not a list containing one
        // empty string — `split` cannot tell those apart, so it is asked here.
        let parts = |s: &str| -> Vec<String> {
            if s.is_empty() {
                Vec::new()
            } else {
                s.split(SUB).map(str::to_string).collect()
            }
        };
        match tag {
            'T' => Shape::Tuple(parts(payload)),
            'F' => Shape::Fields(
                parts(payload)
                    .into_iter()
                    .map(|p| match p.split_once(SUB2) {
                        Some((n, t)) => (n.to_string(), t.to_string()),
                        None => (String::new(), p),
                    })
                    .collect(),
            ),
            'V' => Shape::Variants(parts(payload)),
            'S' => {
                let mut it = payload.splitn(3, SUB);
                let has_self = it.next() == Some("1");
                let ret = it.next().unwrap_or("()").to_string();
                let params = parts(it.next().unwrap_or(""));
                Shape::Signature {
                    params,
                    ret,
                    has_self,
                }
            }
            _ => Shape::Opaque,
        }
    }

    /// The inner type of a newtype (`struct Id(u64)`), or `None`.
    pub fn newtype_inner(&self) -> Option<&str> {
        match self {
            Shape::Tuple(v) if v.len() == 1 => Some(v[0].as_str()),
            _ => None,
        }
    }
}

/// One declared item, flattened.
#[derive(Debug, Clone)]
pub struct ItemFact {
    /// "struct" | "enum" | "fn" | "impl-fn" | "trait-fn" | "trait" | "type"
    pub kind: String,
    pub name: String,
    pub qpath: String,
    pub module: String,
    pub file: String,
    pub line: usize,
    pub end: usize,
    pub vis: String,
    /// First line of the `///` comment, if any.
    pub doc: Option<String>,
    pub shape: Shape,
    /// This is a method inside `impl SomeTrait for T`.
    ///
    /// Recorded because such a method's signature is not a choice its author
    /// made — the trait dictated it — so two of them agreeing is evidence of
    /// nothing. Without this, `concepts --kind signature` on this codebase led
    /// with fifteen `visit_expr_method_call`s, which is a fact about
    /// `syn::Visit`.
    pub in_trait_impl: bool,
    /// Declared *inside a function body*, so nothing outside that body can see
    /// it or collide with it.
    ///
    /// Found by running `gate` on this codebase: it reported `struct Finding`
    /// as already declared at `arith_drift.rs:212`, and `unruster show
    /// arith_drift::Finding` answered "no such item" — because
    /// [`crate::index`] does not descend into fn bodies and this visitor does.
    /// A confident answer nobody can verify is the worst output this tool has,
    /// so a local item is kept (its body is still a real near-clone candidate)
    /// and excluded from every question about what is *declared*.
    pub local: bool,
    /// The concept this item declares itself the canonical home of:
    /// `/// unruster: concept(user.id)`.
    ///
    /// `Some("")` records the malformed form — `concept()` with nothing in it —
    /// rather than discarding it. A declaration the tool silently ignores is
    /// the worst outcome available here: the author believes a name is claimed,
    /// the uniqueness check never fires, and the marker reads as working.
    pub concept: Option<String>,
}

impl ItemFact {
    /// Is this item's *own* visibility `pub`? Used to weight a finding: two
    /// private helpers colliding is cheaper to fix than two exported types.
    pub fn is_pub(&self) -> bool {
        self.vis.starts_with("pub")
    }
}

/// One function body, canonicalized into a structure and its leaves.
///
/// The split is what makes near-clone detection cheap. Two bodies that differ
/// only at leaf positions have, by construction, an identical [`skeleton`], so
/// bucketing on the skeleton reduces an O(n²) similarity search to a linear
/// pass plus small within-bucket comparisons.
///
/// [`skeleton`]: BodyFact::skeleton
#[derive(Debug, Clone)]
pub struct BodyFact {
    pub name: String,
    pub qpath: String,
    pub file: String,
    pub line: usize,
    pub end: usize,
    /// Canonical token count — a size proxy.
    pub tokens: usize,
    /// Punctuation, delimiters and leaf *positions*, with every ident and
    /// literal replaced by a placeholder.
    pub skeleton: String,
    /// The elided idents and literals, in order. Bindings the fn introduces
    /// are already alpha-renamed to `_0`, `_1`, … so a copy-paste that renamed
    /// its locals still matches.
    pub leaves: Vec<String>,
    /// This body implements a trait method.
    ///
    /// Carried for [`crate::near_clones`]: N sibling types implementing one
    /// trait produce N bodies of one shape *because the trait said so*, which
    /// is a weaker signal than two free functions that grew alike.
    pub in_trait_impl: bool,
}

impl BodyFact {
    /// The exact-clone grouping key: skeleton and leaves recombined.
    pub fn canon(&self) -> String {
        let mut s = String::with_capacity(self.skeleton.len() + self.leaves.len() * 8);
        s.push_str(&self.skeleton);
        for l in &self.leaves {
            s.push(SUB);
            s.push_str(l);
        }
        s
    }
}

/// Everything derived from one source file.
#[derive(Debug, Clone, Default)]
pub struct FileFacts {
    pub items: Vec<ItemFact>,
    pub bodies: Vec<BodyFact>,
}

// ──────────────────────────────────────────────────────────────────────────
// Derivation

/// Walk one parsed file and record its items and bodies.
pub fn derive(f: &ParsedFile) -> FileFacts {
    let file = display_path(&f.path);
    let mut v = FactVisitor {
        file: &file,
        module: f.module.clone(),
        scope: ScopeTracker::new(f.module.as_str()),
        trait_impl_depth: 0,
        fn_depth: 0,
        out: FileFacts::default(),
    };
    v.visit_file(&f.ast);
    v.out
}

struct FactVisitor<'a> {
    file: &'a str,
    module: String,
    scope: ScopeTracker,
    /// Depth-tracked rather than a plain bool: an `impl Trait for T` can hold a
    /// nested inherent `impl`, and a single flag would mislabel its methods.
    trait_impl_depth: usize,
    /// How many fn bodies enclose the current position. See [`ItemFact::local`].
    fn_depth: usize,
    out: FileFacts,
}

impl FactVisitor<'_> {
    /// The module path of the *current* scope, `mod` blocks included.
    fn module_now(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            parts.push(self.module.clone());
        }
        parts.extend(self.scope.mod_stack.iter().cloned());
        parts.join("::")
    }

    /// `ext` rather than a `(decl, end)` pair: the two are always taken from
    /// one [`extent_of`] call, and passing them separately is how a caller ends
    /// up with them swapped — a defect that shows as an off-by-a-few source
    /// range rather than as a compile error.
    fn push_item(
        &mut self,
        kind: &str,
        name: String,
        vis: &str,
        attrs: &[syn::Attribute],
        ext: crate::ast::Extent,
        shape: Shape,
    ) {
        self.out.items.push(ItemFact {
            kind: kind.to_string(),
            qpath: self.scope.qualify(&name),
            name,
            module: self.module_now(),
            file: self.file.to_string(),
            line: ext.decl,
            end: ext.end,
            vis: vis.to_string(),
            doc: doc_summary(attrs),
            shape,
            in_trait_impl: self.trait_impl_depth > 0,
            local: self.fn_depth > 0,
            concept: crate::ast::doc_marker(attrs, "concept").map(|a| a.unwrap_or_default()),
        });
    }

    fn push_body(&mut self, sig: &syn::Signature, block: &syn::Block) {
        let (skeleton, leaves, tokens) = canonical_parts(sig, block);
        let name = sig.ident.to_string();
        let start = line_of(&sig.ident);
        self.out.bodies.push(BodyFact {
            qpath: self.scope.qualify(&name),
            name,
            file: self.file.to_string(),
            line: start,
            end: crate::ast::fn_span(sig, block).1,
            tokens,
            skeleton,
            leaves,
            in_trait_impl: self.trait_impl_depth > 0,
        });
    }

    /// Record one fn item. The three fn kinds differ only in the kind label and
    /// the visibility they carry; `near-clones` reported two of them as a
    /// one-leaf divergence, which is exactly that difference.
    fn record_fn(
        &mut self,
        kind: &'static str,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        vis: &str,
    ) {
        let ext = extent_of(sig, attrs, line_of(&sig.ident));
        self.push_item(kind, sig.ident.to_string(), vis, attrs, ext, Self::fn_shape(sig));
    }

    fn fn_shape(sig: &syn::Signature) -> Shape {
        let mut params = Vec::new();
        let mut has_self = false;
        for a in &sig.inputs {
            match a {
                syn::FnArg::Receiver(_) => has_self = true,
                syn::FnArg::Typed(t) => params.push(type_to_string(&t.ty)),
            }
        }
        let ret = match &sig.output {
            // A fn with no `->` returns unit; spelling it that way is what lets
            // `fn a()` and `fn b() -> ()` compare equal, which is what a reader
            // means when they say the two have the same signature.
            syn::ReturnType::Default => "()".to_string(),
            syn::ReturnType::Type(_, t) => type_to_string(t),
        };
        Shape::Signature {
            params,
            ret,
            has_self,
        }
    }
}

impl<'ast> Visit<'ast> for FactVisitor<'_> {
    // `item_trait` and `item_impl` are deliberately absent: this visitor
    // overrides both below — one to record the trait's fns, the other to track
    // whether the methods inside it are trait implementations — and the macro
    // would generate a second definition of the same method.
    scope_visits!(item_mod);

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let is_trait = i.trait_.is_some();
        self.scope.enter_impl(crate::ast::type_short(&i.self_ty));
        self.trait_impl_depth += usize::from(is_trait);
        visit::visit_item_impl(self, i);
        self.trait_impl_depth -= usize::from(is_trait);
        self.scope.leave_impl();
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        let shape = match &i.fields {
            syn::Fields::Named(n) => Shape::Fields(
                n.named
                    .iter()
                    .map(|f| {
                        (
                            f.ident.as_ref().map(ToString::to_string).unwrap_or_default(),
                            type_to_string(&f.ty),
                        )
                    })
                    .collect(),
            ),
            syn::Fields::Unnamed(u) => {
                Shape::Tuple(u.unnamed.iter().map(|f| type_to_string(&f.ty)).collect())
            }
            syn::Fields::Unit => Shape::Tuple(Vec::new()),
        };
        self.push_item(
            "struct",
            i.ident.to_string(),
            vis_str(&i.vis),
            &i.attrs,
            ext,
            shape,
        );
        visit::visit_item_struct(self, i);
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        let shape = Shape::Variants(i.variants.iter().map(|v| v.ident.to_string()).collect());
        self.push_item(
            "enum",
            i.ident.to_string(),
            vis_str(&i.vis),
            &i.attrs,
            ext,
            shape,
        );
        visit::visit_item_enum(self, i);
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        self.push_item(
            "trait",
            i.ident.to_string(),
            vis_str(&i.vis),
            &i.attrs,
            ext,
            Shape::Opaque,
        );
        // `scope_visits!` supplies the enter/leave pair for `item_trait`; this
        // override replaces it, so the walk is continued by hand.
        self.scope.enter_trait(i.ident.to_string());
        for it in &i.items {
            if let syn::TraitItem::Fn(f) = it {
                let ext = extent_of(f, &f.attrs, line_of(&f.sig.ident));
                self.push_item(
                    "trait-fn",
                    f.sig.ident.to_string(),
                    "pub",
                    &f.attrs,
                    ext,
                    Self::fn_shape(&f.sig),
                );
                if let Some(b) = &f.default {
                    self.push_body(&f.sig, b);
                    self.fn_depth += 1;
                    visit::visit_block(self, b);
                    self.fn_depth -= 1;
                }
            }
        }
        self.scope.leave_trait();
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        let ext = extent_of(i, &i.attrs, line_of(&i.ident));
        // An alias's "shape" is the type it names, in the same one-element form
        // a newtype uses. That is deliberate: `type UserId = u64;` and `struct
        // UserId(u64);` are two spellings of one intention, and the concept
        // checks should see them in the same cluster.
        self.push_item(
            "type",
            i.ident.to_string(),
            vis_str(&i.vis),
            &i.attrs,
            ext,
            Shape::Tuple(vec![type_to_string(&i.ty)]),
        );
    }

    // unruster: ok(near-clones/visit_item_fn/visit_impl_item_fn) 2026-08-12 —
    // what remains after `record_fn` was extracted is the item *kind* and the
    // walk that matches it, which is the information these two methods exist to
    // carry. `fn_visits!` cannot generate them: its `around` form has no way to
    // pass a per-kind label, and adding one for a single call site would be a
    // macro written to retire a finding rather than to share a decision.
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.record_fn("fn", &i.attrs, &i.sig, vis_str(&i.vis));
        self.push_body(&i.sig, &i.block);
        self.fn_depth += 1;
        visit::visit_item_fn(self, i);
        self.fn_depth -= 1;
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.record_fn("impl-fn", &i.attrs, &i.sig, vis_str(&i.vis));
        self.push_body(&i.sig, &i.block);
        self.fn_depth += 1;
        visit::visit_impl_item_fn(self, i);
        self.fn_depth -= 1;
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Body canonicalization

/// Placeholder standing in for an elided ident or literal in a skeleton.
const LEAF: char = '\u{b7}';

/// Canonical `(skeleton, leaves, token count)` for one fn body.
///
/// Bindings the function introduces are alpha-renamed to `_0`, `_1`, … in order
/// of first appearance, exactly as [`crate::clones`] does — the two must agree
/// or an "exact clone" and a "near clone with zero differences" would be
/// different things. Called names, literals and control flow are kept verbatim,
/// for the reason `clones` documents: renaming those too turns a defect report
/// into a shape-similarity metric.
pub fn canonical_parts(sig: &syn::Signature, body: &syn::Block) -> (String, Vec<String>, usize) {
    let mut r = Renamer::default();
    for arg in &sig.inputs {
        if let syn::FnArg::Typed(t) = arg {
            r.bind_pat(&t.pat);
        }
    }
    BindingCollector { r: &mut r }.visit_block(body);

    let mut sk = String::new();
    let mut leaves = Vec::new();
    let mut n = 0usize;
    render(body.to_token_stream(), &r, &mut sk, &mut leaves, &mut n);
    (sk, leaves, n)
}

#[derive(Default)]
struct Renamer {
    map: std::collections::HashMap<String, String>,
}

impl Renamer {
    fn bind(&mut self, ident: &syn::Ident) {
        let k = ident.to_string();
        if !self.map.contains_key(&k) {
            let n = self.map.len();
            self.map.insert(k, format!("_{n}"));
        }
    }

    fn bind_pat(&mut self, p: &syn::Pat) {
        struct V<'a>(&'a mut Renamer);
        impl<'ast> Visit<'ast> for V<'_> {
            fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
                self.0.bind(&p.ident);
                visit::visit_pat_ident(self, p);
            }
        }
        V(self).visit_pat(p);
    }
}

struct BindingCollector<'a> {
    r: &'a mut Renamer,
}

impl<'ast> Visit<'ast> for BindingCollector<'_> {
    fn visit_local(&mut self, l: &'ast syn::Local) {
        self.r.bind_pat(&l.pat);
        visit::visit_local(self, l);
    }
    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        for i in &c.inputs {
            self.r.bind_pat(i);
        }
        visit::visit_expr_closure(self, c);
    }
    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        self.r.bind_pat(&a.pat);
        visit::visit_arm(self, a);
    }
    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.r.bind_pat(&e.pat);
        visit::visit_expr_for_loop(self, e);
    }
    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        self.r.bind_pat(&e.pat);
        visit::visit_expr_let(self, e);
    }
}

/// Flatten a token stream into a skeleton plus the leaves it elided.
///
/// Delimiters are written into the skeleton so `f(a)(b)` and `f(a, b)` can
/// never collide, and so two bodies in one skeleton bucket really do have the
/// same structure rather than merely the same token count.
fn render(
    ts: proc_macro2::TokenStream,
    r: &Renamer,
    sk: &mut String,
    leaves: &mut Vec<String>,
    n: &mut usize,
) {
    use proc_macro2::TokenTree;
    for tt in ts {
        *n += 1;
        match tt {
            TokenTree::Ident(i) => {
                let s = i.to_string();
                leaves.push(r.map.get(&s).cloned().unwrap_or(s));
                sk.push(LEAF);
            }
            TokenTree::Literal(l) => {
                leaves.push(l.to_string());
                sk.push(LEAF);
            }
            TokenTree::Punct(p) => sk.push(p.as_char()),
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    proc_macro2::Delimiter::Parenthesis => ('(', ')'),
                    proc_macro2::Delimiter::Brace => ('{', '}'),
                    proc_macro2::Delimiter::Bracket => ('[', ']'),
                    proc_macro2::Delimiter::None => ('\u{2039}', '\u{203a}'),
                };
                sk.push(open);
                render(g.stream(), r, sk, leaves, n);
                sk.push(close);
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Serialization
//
// Line-oriented and tab-separated, for the reason `baseline.rs` gives about its
// own format: this file is written and read by one program, so a self-
// describing format buys nothing, and a line-oriented one stays greppable and
// diffable when something goes wrong.

/// Bump when the record layout changes. The cache stores this alongside the
/// content hash, so an old entry is a miss rather than a misparse.
///
/// v2: items carry `in_trait_impl`.
/// v3: …and `local`.
/// v4: …and `concept`.
/// v5: bodies carry `in_trait_impl`.
pub const SCHEME: u32 = 5;

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => o.push('\\'),
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some(other) => o.push(other),
            None => o.push('\\'),
        }
    }
    o
}

pub fn encode(f: &FileFacts) -> String {
    let mut s = String::new();
    for i in &f.items {
        let (tag, payload) = i.shape.encode();
        s.push_str(&format!(
            "I\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            esc(&i.kind),
            esc(&i.name),
            esc(&i.qpath),
            esc(&i.module),
            esc(&i.file),
            i.line,
            i.end,
            esc(&i.vis),
            u8::from(i.in_trait_impl),
            u8::from(i.local),
            // A declared-but-empty concept and no concept at all are different
            // states, so the marker's presence is its own column rather than an
            // empty string that would collapse the two.
            match &i.concept {
                Some(c) => format!("1{}", esc(c)),
                None => "0".to_string(),
            },
            esc(i.doc.as_deref().unwrap_or("")),
            tag,
            esc(&payload),
        ));
    }
    for b in &f.bodies {
        s.push_str(&format!(
            "B\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            esc(&b.name),
            esc(&b.qpath),
            esc(&b.file),
            b.line,
            b.end,
            b.tokens,
            u8::from(b.in_trait_impl),
            esc(&b.skeleton),
            esc(&b.leaves.join(&SUB.to_string()))
        ));
    }
    s
}

/// Parse what [`encode`] wrote. Returns `None` on any malformed record — a
/// cache entry that cannot be read in full is discarded rather than partly
/// trusted, since a half-read corpus would report absences that are artifacts.
pub fn decode(text: &str) -> Option<FileFacts> {
    let mut out = FileFacts::default();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let c: Vec<&str> = line.split('\t').collect();
        match c.first() {
            Some(&"I") if c.len() == 15 => {
                let doc = unesc(c[12]);
                out.items.push(ItemFact {
                    kind: unesc(c[1]),
                    name: unesc(c[2]),
                    qpath: unesc(c[3]),
                    module: unesc(c[4]),
                    file: unesc(c[5]),
                    line: c[6].parse().ok()?,
                    end: c[7].parse().ok()?,
                    vis: unesc(c[8]),
                    in_trait_impl: c[9] == "1",
                    local: c[10] == "1",
                    concept: c[11]
                        .strip_prefix('1')
                        .map(unesc),
                    doc: (!doc.is_empty()).then_some(doc),
                    shape: Shape::decode(c[13].chars().next()?, &unesc(c[14])),
                });
            }
            Some(&"B") if c.len() == 10 => {
                let leaves = unesc(c[9]);
                out.bodies.push(BodyFact {
                    name: unesc(c[1]),
                    qpath: unesc(c[2]),
                    file: unesc(c[3]),
                    line: c[4].parse().ok()?,
                    end: c[5].parse().ok()?,
                    tokens: c[6].parse().ok()?,
                    in_trait_impl: c[7] == "1",
                    skeleton: unesc(c[8]),
                    leaves: if leaves.is_empty() {
                        Vec::new()
                    } else {
                        leaves.split(SUB).map(str::to_string).collect()
                    },
                });
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Rewrite the `file` field of every record. Cache entries are keyed by content
/// hash, so the same bytes under two paths share one entry — and the stored
/// path is whichever one wrote it first. A row naming a file the reader is not
/// looking at is worse than a cache miss, so the caller stamps the path it
/// asked for.
pub fn restamp(f: &mut FileFacts, file: &str) {
    for i in &mut f.items {
        i.file = file.to_string();
    }
    for b in &mut f.bodies {
        b.file = file.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_of(src: &str) -> FileFacts {
        let ast = syn::parse_file(src).expect("parse");
        let pf = ParsedFile {
            path: std::path::PathBuf::from("src/t.rs"),
            ast,
            module: "t".into(),
        };
        derive(&pf)
    }

    #[test]
    fn a_newtype_records_its_inner_type() {
        let f = facts_of("pub struct UserId(u64);");
        assert_eq!(f.items[0].shape.newtype_inner(), Some("u64"));
        assert!(f.items[0].is_pub());
    }

    #[test]
    fn a_type_alias_shares_the_newtype_shape() {
        // Two spellings of one intention; the concept checks must see both.
        let a = facts_of("pub type UserId = u64;");
        let b = facts_of("pub struct UserId(u64);");
        assert_eq!(a.items[0].shape, b.items[0].shape);
    }

    #[test]
    fn a_bare_fn_returns_unit_rather_than_nothing() {
        let a = facts_of("fn f(x: u8) {}");
        let b = facts_of("fn g(y: u8) -> () {}");
        let (Shape::Signature { ret: ra, .. }, Shape::Signature { ret: rb, .. }) =
            (&a.items[0].shape, &b.items[0].shape)
        else {
            panic!("not signatures");
        };
        assert_eq!(ra, rb);
    }

    /// The property the near-clone bucketing depends on: two bodies differing
    /// only in a leaf must land on one skeleton.
    #[test]
    fn one_differing_leaf_keeps_the_skeleton_identical() {
        let a = facts_of(r#"fn f(d: &D) { d.exec("DELETE FROM users"); }"#);
        let b = facts_of(r#"fn g(d: &D) { d.exec("DELETE FROM orders"); }"#);
        assert_eq!(a.bodies[0].skeleton, b.bodies[0].skeleton);
        assert_ne!(a.bodies[0].leaves, b.bodies[0].leaves);
        assert_ne!(a.bodies[0].canon(), b.bodies[0].canon());
    }

    /// And the converse: different structure must not share a bucket, or the
    /// leaf-difference count would be comparing unrelated positions.
    #[test]
    fn different_structure_lands_in_different_buckets() {
        let a = facts_of("fn f(x: T) { g(h(x)); }");
        let b = facts_of("fn f(x: T) { g(h, x); }");
        assert_ne!(a.bodies[0].skeleton, b.bodies[0].skeleton);
    }

    #[test]
    fn alpha_renaming_matches_the_clones_check() {
        let a = facts_of(
            r#"fn parse_uuid(bytes: &[u8], field: &'static str) -> Result<Uuid, Status> {
                   Uuid::from_slice(bytes).map_err(|_| Status::invalid_argument(field))
               }"#,
        );
        let b = facts_of(
            r#"fn parse_uuid(raw: &[u8], name: &'static str) -> Result<Uuid, Status> {
                   Uuid::from_slice(raw).map_err(|_| Status::invalid_argument(name))
               }"#,
        );
        assert_eq!(a.bodies[0].canon(), b.bodies[0].canon());
    }

    fn canon_of(src: &str) -> String {
        let f: syn::ItemFn = syn::parse_str(src).expect("parse");
        let (sk, leaves, _) = canonical_parts(&f.sig, &f.block);
        BodyFact {
            name: String::new(),
            qpath: String::new(),
            file: String::new(),
            line: 0,
            end: 0,
            tokens: 0,
            skeleton: sk,
            leaves,
            in_trait_impl: false,
        }
        .canon()
    }

    /// Formatting is not identity. Two copies rustfmt broke differently are
    /// still two copies. (Moved here with the canonicalizer, from `clones`.)
    #[test]
    fn whitespace_and_line_breaks_do_not_matter() {
        let a = canon_of("fn f(x: u32) -> u32 { let y = x + 1; y * 2 }");
        let b = canon_of("fn f(x: u32) -> u32 {\n    let y = x + 1;\n\n    y * 2\n}");
        assert_eq!(a, b);
    }

    /// Called names are API, not local naming. Renaming them too would turn
    /// the clone check into a shape-similarity metric and group everything.
    #[test]
    fn different_callees_are_not_clones() {
        let a = canon_of("fn f(x: T) -> u32 { x.width() + x.height() }");
        let b = canon_of("fn g(y: T) -> u32 { y.rows() + y.cols() }");
        assert_ne!(a, b);
    }

    /// Literals carry meaning. Two functions that differ only in the table they
    /// write to are not the same function.
    #[test]
    fn different_literals_are_not_clones() {
        let a = canon_of(r#"fn f(d: &D) { d.exec("DELETE FROM users"); }"#);
        let b = canon_of(r#"fn g(d: &D) { d.exec("DELETE FROM orders"); }"#);
        assert_ne!(a, b);
    }

    /// Structure has to survive flattening: same tokens, different nesting.
    #[test]
    fn delimiters_are_part_of_the_key() {
        let a = canon_of("fn f(x: T) { g(h(x)); }");
        let b = canon_of("fn f(x: T) { g(h, x); }");
        assert_ne!(a, b);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let f = facts_of(
            r#"
            /// Docs with a	tab and a \ backslash.
            pub struct Cfg { pub name: String, pub retries: u32 }
            pub enum State { Idle, Busy }
            pub fn run(c: &Cfg) -> Result<(), Error> { let x = c.retries; go(x) }
            "#,
        );
        let round = decode(&encode(&f)).expect("decodes");
        assert_eq!(round.items.len(), f.items.len());
        assert_eq!(round.bodies.len(), f.bodies.len());
        assert_eq!(round.items[0].doc, f.items[0].doc);
        assert_eq!(round.items[0].shape, f.items[0].shape);
        assert_eq!(round.items[1].shape, f.items[1].shape);
        assert_eq!(round.bodies[0].leaves, f.bodies[0].leaves);
        assert_eq!(round.bodies[0].canon(), f.bodies[0].canon());
    }

    #[test]
    fn a_corrupt_record_decodes_to_nothing_rather_than_a_partial_corpus() {
        assert!(decode("I\tstruct\tonly-three-fields").is_none());
    }

    /// Found by running `gate` on this codebase. A `struct` declared inside a
    /// fn body was reported as a name collision, and `unruster show` could not
    /// resolve it — because `index` does not descend into fn bodies and this
    /// visitor does. The body still counts as a near-clone candidate; only the
    /// *declaration* is withdrawn.
    #[test]
    fn an_item_declared_inside_a_fn_body_is_marked_local() {
        let f = facts_of(
            "pub struct Outer(u8);\n\
             pub fn run() { #[derive(Debug)] struct Finding { a: u8 } let _ = Finding { a: 1 }; }",
        );
        let outer = f.items.iter().find(|i| i.name == "Outer").expect("outer");
        assert!(!outer.local);
        let inner = f.items.iter().find(|i| i.name == "Finding").expect("inner");
        assert!(inner.local, "a fn-local struct must not read as a declaration");
    }

    /// The other half: a fn-local `impl` block's methods must still be
    /// collected as bodies, or near-clone coverage silently loses every
    /// `struct V; impl Visit for V` helper in this codebase.
    #[test]
    fn a_fn_local_impl_still_contributes_its_bodies() {
        let f = facts_of(
            "pub fn run(p: &P) { struct V; impl V { fn go(&self, n: usize) -> usize { n + 1 } } \
             V.go(p.len()); }",
        );
        assert!(
            f.bodies.iter().any(|b| b.name == "go"),
            "bodies: {:?}",
            f.bodies.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn methods_are_recorded_with_their_impl_qualified_path() {
        let f = facts_of("struct W; impl W { pub fn parse(s: &str) -> W { W } }");
        let m = f
            .items
            .iter()
            .find(|i| i.kind == "impl-fn")
            .expect("method recorded");
        assert_eq!(m.qpath, "t::W::parse");
    }
}
