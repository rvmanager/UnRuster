use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::ast::{line_of_span, type_last_segment};
use crate::context::AnalysisCtx;
use crate::parse::display_path;
use crate::emit::{row, site};

#[derive(Debug, Clone)]
struct FromImpl {
    trait_name: String, // "From" or "TryFrom"
    src: String,
    dst: String,
    file: String,
    line: usize,
}

/// `A<->B`, order-independent, for the waiver key. Uses `<->` rather than the
/// display `↔` so the key is typeable without a character picker — a waiver you
/// cannot type by hand is one nobody will correct by hand.
fn pair_key(f: &FromImpl) -> String {
    let (a, b) = if f.src < f.dst {
        (&f.src, &f.dst)
    } else {
        (&f.dst, &f.src)
    };
    format!("{}<->{}", a, b)
}

struct FromVisitor<'a> {
    file: &'a str,
    out: Vec<FromImpl>,
}

impl<'ast, 'a> Visit<'ast> for FromVisitor<'a> {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let Some((_, trait_path, _)) = &i.trait_ else {
            visit::visit_item_impl(self, i);
            return;
        };
        let Some(last_seg) = trait_path.segments.last() else {
            return;
        };
        let trait_name = last_seg.ident.to_string();
        if trait_name != "From" && trait_name != "TryFrom" {
            return;
        }
        let src = match &last_seg.arguments {
            syn::PathArguments::AngleBracketed(a) => a.args.iter().find_map(|arg| match arg {
                syn::GenericArgument::Type(t) => type_last_segment(t),
                _ => None,
            }),
            _ => None,
        };
        let dst = type_last_segment(&i.self_ty);
        if let (Some(s), Some(d)) = (src, dst) {
            self.out.push(FromImpl {
                trait_name,
                src: s,
                dst: d,
                file: self.file.to_string(),
                line: line_of_span(i.impl_token.span),
            });
        }
    }
}

pub fn run(ctx: &AnalysisCtx) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut impls: Vec<FromImpl> = Vec::new();
    for f in files {
        let mut v = FromVisitor {
            file: &display_path(&f.path),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        impls.extend(v.out);
    }

    // Index by (trait, src, dst) -> FromImpl.
    let mut idx: BTreeMap<(String, String, String), FromImpl> = BTreeMap::new();
    for fi in &impls {
        idx.insert(
            (fi.trait_name.clone(), fi.src.clone(), fi.dst.clone()),
            fi.clone(),
        );
    }

    // Find bidirectional pairs. Canonicalize ordering by alphabetical name so
    // we don't double-emit `A↔B` and `B↔A`.
    let mut emitted: std::collections::BTreeSet<(String, String, String)> =
        std::collections::BTreeSet::new();
    // The waiver key is carried alongside so `retain_unsuppressed` can borrow
    // it — a `Site` holds `&str`, so a key built inside the closure would not
    // outlive the call.
    let mut pairs: Vec<(FromImpl, FromImpl, String)> = Vec::new();
    for fi in &impls {
        let key_reverse = (
            fi.trait_name.clone(),
            fi.dst.clone(),
            fi.src.clone(),
        );
        if fi.src == fi.dst {
            continue;
        }
        if let Some(rev) = idx.get(&key_reverse) {
            // Pick canonical ordering so each pair appears once.
            let (a, b) = if fi.src < fi.dst {
                (fi.clone(), rev.clone())
            } else {
                (rev.clone(), fi.clone())
            };
            let canon_key = (fi.trait_name.clone(), a.src.clone(), a.dst.clone());
            if emitted.insert(canon_key) {
                let key = pair_key(&a);
                pairs.push((a, b, key));
            }
        }
    }

    pairs.sort_by(|x, y| {
        x.0.trait_name
            .cmp(&y.0.trait_name)
            .then_with(|| x.0.src.cmp(&y.0.src))
            .then_with(|| x.0.dst.cmp(&y.0.dst))
    });

    // The only check in the audit battery that was not honouring
    // `--changed-since`, and a gating one — so a scoped run leaked whole-tree
    // rows and exited 1 over code the caller had not touched, which is exactly
    // the `until unruster --fail-on-findings audit` loop failing to be able to
    // go green. Either side counts: a pair is a relationship between two impls,
    // and editing one half is what makes it this diff's business.
    if ctx.changed.is_some() {
        pairs.retain(|(a, b, _)| ctx.in_scope(&a.file) || ctx.in_scope(&b.file));
    }

    // Keyed by the type pair, and matched against the *forward* impl's site so
    // one waiver above `impl From<A> for B` retires the pair. A gating check
    // whose commonest true verdict is "one of these types is foreign, so they
    // cannot be merged" needs a way to record that verdict.
    let waived = ctx.retain_unsuppressed("conversion-pairs", &mut pairs, |p| {
        crate::suppress::Site::keyed(p.0.file.as_str(), p.0.line, p.2.as_str())
    });

    if !summary {
        let today = crate::suppress::Date::today();
        for (forward, reverse, key) in &pairs {
            row!(
                ctx.out,
                "trait" => forward.trait_name.clone(),
                "pair" => format!("{} ↔ {}", forward.src, forward.dst),
                "at" => site(&forward.file, forward.line),
                "reverse_at" => site(&reverse.file, reverse.line),
            );
            ctx.suggest("conversion-pairs", Some(key), today);
        }
    }
    ctx.out.summary(&format!(
        "({} bidirectional pair(s){}; explain: replication)",
        pairs.len(),
        ctx.waived_note(waived)
    ));
    Ok(pairs.len())
}
