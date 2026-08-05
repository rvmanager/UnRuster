//! `explain <topic>` — print one playbook section instead of the whole
//! `--help` text. Sections are parsed from the same `playbook.txt` that backs
//! `long_about`, so there is exactly one source of repair recipes. Designed
//! for agent loops: a finding row's summary points at a topic, and `explain`
//! fetches just that recipe (token economy).

const PLAYBOOK: &str = include_str!("playbook.txt");

/// A `◇ HEADING` playbook section: (first heading line, full section text).
fn sections() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for line in PLAYBOOK.lines() {
        let is_boundary = line.starts_with("────") || line.starts_with("═");
        if let Some(rest) = line.strip_prefix("◇ ") {
            if let Some((h, body)) = current.take() {
                out.push((h, body.join("\n")));
            }
            current = Some((rest.trim().to_string(), vec![line.to_string()]));
        } else if is_boundary {
            if let Some((h, body)) = current.take() {
                out.push((h, body.join("\n")));
            }
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line.to_string());
        }
    }
    if let Some((h, body)) = current.take() {
        out.push((h, body.join("\n")));
    }
    out
}

/// Case-insensitive match: every word of the query (split on space and `-`)
/// must appear in the heading.
fn heading_matches(heading: &str, query: &str) -> bool {
    let h = heading.to_lowercase();
    query
        .to_lowercase()
        .split([' ', '-'])
        .filter(|w| !w.is_empty())
        .all(|w| h.contains(w))
}

pub fn run(out: &crate::emit::Out, topic: Option<&str>) -> anyhow::Result<usize> {
    let secs = sections();
    match topic {
        None => {
            out.line("playbook topics (unruster explain <topic>):");
            for (h, _) in &secs {
                out.line(&format!("  {}", h));
            }
            out.summary(&format!("({} topic(s))", secs.len()));
            Ok(0)
        }
        Some(q) => {
            let hits: Vec<_> = secs
                .iter()
                .filter(|(h, _)| heading_matches(h, q))
                .collect();
            if hits.is_empty() {
                out.note(&format!("no playbook topic matching `{}`; topics are:", q));
                for (h, _) in &secs {
                    out.note(&format!("  {}", h));
                }
                return Err(crate::context::TargetNotFound::err("playbook topic", q));
            }
            for (i, (_, body)) in hits.iter().enumerate() {
                if i > 0 {
                    out.line("");
                }
                out.line(body.trim_end());
            }
            out.summary(&format!("({} matching topic(s))", hits.len()));
            Ok(0)
        }
    }
}

/// `playbook` — the whole design-audit text, which used to be printed by
/// `--help` ahead of the command list. Moving it behind its own subcommand is
/// what let `Commands:` move to the top of the help output.
pub fn run_playbook(out: &crate::emit::Out) {
    for line in PLAYBOOK.lines() {
        out.line(line);
    }
}
