use syn::visit::{self, Visit};

use crate::ast::{
    fn_span, fn_visits, print_grouped_counts, scope_visits, ScopeTracker, top_module_of,
    type_to_string,
};
use crate::context::{AnalysisCtx, GroupBy};
use crate::parse::display_path;
use crate::semantic::{FnSigIndex, FnTypes};
use crate::emit::{row, site};

#[derive(Debug)]
struct Hit {
    class: &'static str,
    /// Inside an `unsafe` block or an `unsafe fn`. A pointer cast there is the
    /// FFI boundary doing its job, not a data-loss defect: `p as *const Method`
    /// in an objc shim has no safer spelling. Tracked rather than dropped so
    /// `--include-unsafe-ptr` can restore the rows.
    in_unsafe: bool,
    src: String, // "_" if unknown
    dst: String,
    context: String,
    file: String,
    line: usize,
}

struct CastVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    /// Nesting depth of `unsafe` blocks / fns currently open.
    unsafe_depth: usize,
    fn_types_stack: Vec<FnTypes>,
    fn_sigs: &'a FnSigIndex,
    hits: Vec<Hit>,
}

impl<'a> CastVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    // unruster: ok(concepts/signature:fn) 2026-08-12 — this signature IS the
    // `fn_visits!(around …)` handler contract, so the six methods that share it
    // agree on purpose. The macro is what makes them identical; consolidating
    // them further would mean consolidating the three visitors, which have
    // nothing else in common.
    /// Open a fn: track `unsafe` nesting and push the local-type inference the
    /// receiver-typed cast classes read. Shared by every fn-shaped visit
    /// method — see [`fn_visits`].
    fn enter_fn(&mut self, sig: &syn::Signature, block: Option<&syn::Block>) {
        self.unsafe_depth += usize::from(sig.unsafety.is_some());
        let Some(block) = block else { return };
        self.scope.enter_fn(sig.ident.to_string(), fn_span(sig, block));
        self.fn_types_stack.push(FnTypes::build(
            sig,
            block,
            self.fn_sigs,
            self.scope.impl_stack.last().map(String::as_str),
        ));
    }

    /// Close it. `unsafe` is recomputed from the signature rather than carried
    /// across the walk: the two halves then read as one statement about the
    /// same signature, and there is no guard value to drop on an early return.
    fn leave_fn(&mut self, sig: &syn::Signature, block: Option<&syn::Block>) {
        if block.is_some() {
            self.fn_types_stack.pop();
            self.scope.leave_fn();
        }
        self.unsafe_depth -= usize::from(sig.unsafety.is_some());
    }
}

/// `--class` filter values, kebab-cased by clap (NarrowInt → `narrow-int`).
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CastClass {
    NarrowInt,
    WidenInt,
    SignedFlip,
    FloatInt,
    IntFloat,
    NarrowFloat,
    WidenFloat,
    Ptr,
    UsizeCross,
    UsizeWiden,
    Unknown,
    Other,
}

impl CastClass {
    fn as_str(self) -> &'static str {
        match self {
            CastClass::NarrowInt => "narrow-int",
            CastClass::WidenInt => "widen-int",
            CastClass::SignedFlip => "signed-flip",
            CastClass::FloatInt => "float-int",
            CastClass::IntFloat => "int-float",
            CastClass::NarrowFloat => "narrow-float",
            CastClass::WidenFloat => "widen-float",
            CastClass::Ptr => "ptr",
            CastClass::UsizeCross => "usize-cross",
            CastClass::UsizeWiden => "usize-widen",
            CastClass::Unknown => "unknown",
            CastClass::Other => "other",
        }
    }
}

fn int_width_signed(t: &str) -> Option<(u16, bool)> {
    Some(match t {
        "u8" => (8, false),
        "u16" => (16, false),
        "u32" => (32, false),
        "u64" => (64, false),
        "u128" => (128, false),
        "i8" => (8, true),
        "i16" => (16, true),
        "i32" => (32, true),
        "i64" => (64, true),
        "i128" => (128, true),
        _ => return None,
    })
}

fn is_float(t: &str) -> bool {
    t == "f32" || t == "f64"
}

fn is_usize_family(t: &str) -> bool {
    t == "usize" || t == "isize"
}

/// Integer-to-integer classification by width and signedness.
fn classify_int_pair(sw: u16, sgn_s: bool, dw: u16, sgn_d: bool) -> &'static str {
    if sw == dw && sgn_s != sgn_d {
        "signed-flip"
    } else if dw < sw {
        "narrow-int"
    } else {
        // wider, or same width + same signedness (no-op-ish): bucket as widen.
        "widen-int"
    }
}

/// Float-involving classification; `None` when neither side is a float mix.
fn classify_float_mix(s: &str, dst: &str) -> Option<&'static str> {
    if is_float(s) && int_width_signed(dst).is_some() {
        return Some("float-int");
    }
    if int_width_signed(s).is_some() && is_float(dst) {
        return Some("int-float");
    }
    if is_float(s) && is_float(dst) {
        return Some(if s == "f64" && dst == "f32" {
            "narrow-float"
        } else {
            "widen-float"
        });
    }
    None
}

/// Width of `usize`/`isize` on the targets this tool is used to audit. Every
/// mainstream Rust target in practice is 64-bit; a 16- or 32-bit embedded
/// target would make some `usize-widen` rows narrowing, which is why the class
/// name says *widen* rather than *safe* and the summary line names the
/// assumption.
const USIZE_WIDTH: u16 = 64;

/// `u32 as usize` on a 64-bit target loses nothing, and neither does
/// `usize as u64`. Bundling those with genuinely lossy `f64 as usize` made
/// this check ~83% noise on a real codebase — 29 of 35 rows were lossless
/// widening that no reader ever acted on.
fn usize_cross_is_lossless(src: &str, dst: &str) -> bool {
    let width_of = |t: &str| -> Option<u16> {
        if is_usize_family(t) {
            Some(USIZE_WIDTH)
        } else {
            int_width_signed(t).map(|(w, _)| w)
        }
    };
    let signed_of = |t: &str| -> Option<bool> {
        if t == "usize" {
            Some(false)
        } else if t == "isize" {
            Some(true)
        } else {
            int_width_signed(t).map(|(_, s)| s)
        }
    };
    let (Some(sw), Some(dw)) = (width_of(src), width_of(dst)) else {
        return false;
    };
    let (Some(ss), Some(ds)) = (signed_of(src), signed_of(dst)) else {
        return false;
    };
    match (ss, ds) {
        // Signed → signed: lossless when the destination is at least as wide.
        (true, true) => dw >= sw,
        // Signed → unsigned: a negative value wraps. Never lossless.
        (true, false) => false,
        // Unsigned → signed: needs a strictly wider destination, since the
        // sign bit costs one bit of range.
        (false, true) => dw > sw,
        // Unsigned → unsigned: lossless when at least as wide.
        (false, false) => dw >= sw,
    }
}

fn classify(src: Option<&str>, dst: &str) -> &'static str {
    if dst.starts_with("*const") || dst.starts_with("*mut") {
        return "ptr";
    }
    let src_is_usize = src.map(is_usize_family).unwrap_or(false);
    let dst_is_usize = is_usize_family(dst);
    // `isize as usize` (and the reverse) used to fall past every arm and land
    // in `other`, which no check reports. It is a sign flip on the type Rust
    // indexes with: a negative `isize` becomes an enormous index, and the
    // panic lands far from the cast. Classify it with the other sign flips.
    if src_is_usize && dst_is_usize && src != Some(dst) {
        return "signed-flip";
    }
    let usize_involved = (src_is_usize && !dst_is_usize && int_width_signed(dst).is_some())
        || (dst_is_usize && src.is_some() && !src_is_usize);
    if usize_involved {
        let s = src.expect("usize_involved implies a known source type");
        return if usize_cross_is_lossless(s, dst) {
            "usize-widen"
        } else {
            "usize-cross"
        };
    }
    let dst_int = int_width_signed(dst);
    let src_int = src.and_then(int_width_signed);
    if let (Some((sw, sgn_s)), Some((dw, sgn_d))) = (src_int, dst_int) {
        return classify_int_pair(sw, sgn_s, dw, sgn_d);
    }
    if let Some(class) = src.and_then(|s| classify_float_mix(s, dst)) {
        return class;
    }
    if src.is_none() {
        return "unknown";
    }
    "other"
}

impl<'ast, 'a> Visit<'ast> for CastVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, trait_item_fn_typed);
    fn_visits!(around enter_fn, leave_fn; item_fn, impl_item_fn);

    fn visit_expr_cast(&mut self, e: &'ast syn::ExprCast) {
        let dst = type_to_string(&e.ty);
        // Grounded inference only: a cast row *states* the source type, and a
        // wrong statement costs more than a missing one. A reader who sees
        // `f64 → usize` on code that is actually `u32 → usize` stops trusting
        // the whole check; `_ → usize` just prompts a look.
        let src = self
            .fn_types_stack
            .last()
            .and_then(|ft| ft.type_of_grounded(&e.expr, self.fn_sigs));
        let class = classify(src.as_deref(), &dst);
        self.hits.push(Hit {
            class,
            in_unsafe: self.unsafe_depth > 0,
            src: src.unwrap_or_else(|| "_".into()),
            dst,
            context: self.enclosing(),
            file: self.file.to_string(),
            line: e.as_token.span.start().line,
        });
        visit::visit_expr_cast(self, e);
    }

    fn visit_expr_unsafe(&mut self, e: &'ast syn::ExprUnsafe) {
        self.unsafe_depth += 1;
        visit::visit_expr_unsafe(self, e);
        self.unsafe_depth -= 1;
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    class_filter: &[CastClass],
    by: Option<GroupBy>,
    hide_widen: bool,
    include_unsafe_ptr: bool,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let fn_sigs = &ctx.sem.fn_sigs;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = CastVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            unsafe_depth: 0,
            fn_types_stack: Vec::new(),
            fn_sigs,
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    ctx.retain_changed(&mut all, |h| &h.file);
    // Keyed by cast class, so `ok(casts/ptr)` on an FFI shim doesn't also
    // waive a narrowing cast that lands inside the same span.
    // The tier is a *class* filter here rather than a score, and it is applied
    // below — after this retain, because a suppressed row must not be counted
    // at all. So the ledger is told which side of it each hit falls on: a
    // waiver over a `widen-int` cast is not holding the audit loop open, and
    // reporting `hits=1` for it said it was.
    let reported = |h: &Hit| {
        (class_filter.is_empty() || class_filter.iter().any(|c| c.as_str() == h.class))
            && !(hide_widen && matches!(h.class, "widen-int" | "widen-float" | "usize-widen"))
            && !(!include_unsafe_ptr && h.class == "ptr" && h.in_unsafe)
            && !(!class_filter.contains(&CastClass::UsizeWiden) && h.class == "usize-widen")
    };
    let waived = ctx.retain_unsuppressed_tiered(
        "casts",
        &mut all,
        |h| crate::suppress::Site::keyed(h.file.as_str(), h.line, h.class),
        reported,
    );
    if !class_filter.is_empty() {
        let wanted: Vec<&str> = class_filter.iter().map(|c| c.as_str()).collect();
        all.retain(|h| wanted.contains(&h.class));
    }
    if hide_widen {
        all.retain(|h| !matches!(h.class, "widen-int" | "widen-float" | "usize-widen"));
    }
    // `usize-widen` is lossless by construction, so it is off unless asked for
    // by name. Without an explicit --class the default view is defect classes.
    // A `ptr` cast inside `unsafe` is the FFI boundary, not a data-loss defect.
    // Five of five on a real audit were objc / UTI shims, reported every run and
    // acted on never.
    let mut unsafe_ptr_hidden = 0usize;
    if !include_unsafe_ptr {
        let before = all.len();
        all.retain(|h| !(h.class == "ptr" && h.in_unsafe));
        unsafe_ptr_hidden = before - all.len();
    }
    let asked_for_widen = class_filter.contains(&CastClass::UsizeWiden);
    let mut widen_hidden = 0usize;
    if !asked_for_widen {
        let before = all.len();
        all.retain(|h| h.class != "usize-widen");
        widen_hidden = before - all.len();
    }

    all.sort_by(|a, b| {
        a.class
            .cmp(b.class)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        match by {
            Some(GroupBy::Fn) => print_grouped_counts(ctx.out, &all, |h| h.context.clone()),
            Some(GroupBy::File) => print_grouped_counts(ctx.out, &all, |h| h.file.clone()),
            Some(GroupBy::Module) => {
                print_grouped_counts(ctx.out, &all, |h| top_module_of(&h.context).to_string())
            }
            None => {
                let today = crate::suppress::Date::today();
                for h in &all {
                    row!(
                        ctx.out,
                        "class" => h.class,
                        "src" => h.src.clone(),
                        "dst" => h.dst.clone(),
                        "in_fn" => h.context.clone(),
                        "at" => site(&h.file, h.line),
                    );
                    ctx.suggest("casts", Some(h.class), today);
                }
            }
        }
    }

    use std::collections::BTreeMap;
    let mut by_class: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_class.entry(h.class).or_insert(0) += 1;
    }
    let break_str: Vec<String> = by_class.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    ctx.out.summary(&format!(
        "({} cast(s); {}; hide_widen={}{}{}{}; explain: casts)",
        all.len(),
        break_str.join(", "),
        hide_widen,
        ctx.waived_note(waived),
        if unsafe_ptr_hidden > 0 {
            format!(
                "; {} ptr cast(s) inside `unsafe` hidden (FFI boundary — \
                 `--include-unsafe-ptr` to restore)",
                unsafe_ptr_hidden
            )
        } else {
            String::new()
        },
        if widen_hidden > 0 {
            format!(
                "; {} lossless usize-widen row(s) hidden (assumes {}-bit usize; \
                 `--class usize-widen` to see them)",
                widen_hidden, USIZE_WIDTH
            )
        } else {
            String::new()
        }
    ));
    Ok(all.len())
}
