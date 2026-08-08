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
    Ok(total)
}
