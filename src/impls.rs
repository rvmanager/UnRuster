use crate::context::AnalysisCtx;
use crate::emit::row;

pub fn run(
    ctx: &AnalysisCtx,
    of_type: Option<&str>,
    of_trait: Option<&str>,
) -> anyhow::Result<usize> {
    let index = ctx.idx;
    let summary = ctx.summary;
    let mut hits: Vec<_> = index
        .iter()
        .filter(|d| d.kind == "impl")
        .filter(|d| match of_type {
            Some(t) => {
                let last = crate::ast::last_segment(t);
                d.name == last
            }
            None => true,
        })
        .filter(|d| match of_trait {
            Some(tr) => {
                let last = crate::ast::last_segment(tr);
                d.trait_name.as_deref() == Some(last)
            }
            None => true,
        })
        .collect();

    hits.sort_by(|a, b| {
        a.trait_name
            .as_deref()
            .unwrap_or("")
            .cmp(b.trait_name.as_deref().unwrap_or(""))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    // The summary counts the whole result set; `--top` only bounds the list.
    let total = hits.len();
    if !summary {
        for d in &hits {
            let trait_disp = d.trait_name.as_deref().unwrap_or("—");
            row!(
                ctx.out,
                "trait" => trait_disp,
                "name" => d.name.clone(),
                "qpath" => d.qpath.clone(),
                "at" => ctx.at(&d.file, d.line, d.end),
            );
        }
    }
    ctx.out.summary(&format!("({} impl block(s))", total));
    // An unfiltered listing is a wall, and the escape people write is
    // `impls | grep -A30 "impl Mask"` — which returns the thirty rows that
    // happen to *sort* after `Mask` rather than any of its members, because a
    // two-column TSV cannot express "the contents of this block" and grep
    // cannot know that. Both commands that answer the question are named here,
    // and only when the listing is big enough for the question to arise.
    // Ten because that is roughly where a two-column listing stops being
    // something you take in at a glance and starts being something you pipe.
    if of_type.is_none() && of_trait.is_none() && total > 10 {
        ctx.out.note(
            "note: unfiltered. `impls --of <Type>` lists one type's blocks and \
             `impls --trait <Trait>` one trait's implementors; for the *members* of a \
             block it is `outline <file>` — a grep over these rows returns whatever sorts \
             next to the name, not what the block contains.",
        );
    }
    Ok(total)
}
