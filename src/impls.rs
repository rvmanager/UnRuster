use crate::context::AnalysisCtx;

pub fn run(
    ctx: &AnalysisCtx,
    of_type: Option<&str>,
    of_trait: Option<&str>,
) -> anyhow::Result<()> {
    let index = ctx.idx;
    let summary = ctx.summary;
    let mut hits: Vec<_> = index
        .iter()
        .filter(|d| d.kind == "impl")
        .filter(|d| match of_type {
            Some(t) => {
                let last = t.rsplit("::").next().unwrap_or(t);
                d.name == last
            }
            None => true,
        })
        .filter(|d| match of_trait {
            Some(tr) => {
                let last = tr.rsplit("::").next().unwrap_or(tr);
                d.trait_name.as_deref() == Some(last)
            }
            None => true,
        })
        .collect();

    hits.sort_by(|a, b| {
        a.trait_name
            .clone()
            .unwrap_or_default()
            .cmp(&b.trait_name.clone().unwrap_or_default())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        for d in &hits {
            let trait_disp = d.trait_name.as_deref().unwrap_or("—");
            println!("{}\t{}\t{}\t{}:{}", trait_disp, d.name, d.qpath, d.file, d.line);
        }
    }
    eprintln!("({} impl block(s))", hits.len());
    Ok(())
}
