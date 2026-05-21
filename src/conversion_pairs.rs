use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::ast::{line_of_span, type_last_segment};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug, Clone)]
struct FromImpl {
    trait_name: String, // "From" or "TryFrom"
    src: String,
    dst: String,
    file: String,
    line: usize,
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

pub fn run(files: &[ParsedFile], summary: bool) -> anyhow::Result<()> {
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
    let mut pairs: Vec<(FromImpl, FromImpl)> = Vec::new();
    for fi in &impls {
        let key_forward = (
            fi.trait_name.clone(),
            fi.src.clone(),
            fi.dst.clone(),
        );
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
                pairs.push((a, b));
            }
        }
        let _ = key_forward; // suppress unused warning
    }

    pairs.sort_by(|x, y| {
        x.0.trait_name
            .cmp(&y.0.trait_name)
            .then_with(|| x.0.src.cmp(&y.0.src))
            .then_with(|| x.0.dst.cmp(&y.0.dst))
    });

    if !summary {
        for (forward, reverse) in &pairs {
            println!(
                "{}\t{} ↔ {}\t{}:{}\t{}:{}",
                forward.trait_name,
                forward.src,
                forward.dst,
                forward.file,
                forward.line,
                reverse.file,
                reverse.line,
            );
        }
    }
    eprintln!(
        "({} bidirectional pair(s); design-smell hint: two types with mutual From impls are usually the same logical concept wearing two shapes — collapse to one type, or make one a view (`AsRef`) of the other.)",
        pairs.len()
    );
    Ok(())
}
