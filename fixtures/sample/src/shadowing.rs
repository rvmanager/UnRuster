//! A bare name in *argument position* is how a callback is written
//! (`.map(parse)`) and how every ordinary variable is written, and the call
//! walk used to record the first reading unconditionally. `svggen`'s
//! `out::path()` was reported with 30 callers across 7 modules at `resolved`
//! confidence — every row a parameter, a `let` or a `match` binding — and
//! `contract-drift --candidates` then ranked the fn 4th of 286 on the strength
//! of the fake set.
//!
//! Every binding form below shadows `logfile`, and `keep_it` / `helpers::spell`
//! are the genuine references that must survive at full confidence.

pub mod helpers {
    /// The item every local below is mistaken for.
    pub fn logfile() -> Option<String> {
        None
    }

    /// Takes an argument, so a shadowing parameter must be caught here too —
    /// the defect was never about arity.
    pub fn spell(s: &str) -> usize {
        s.len()
    }
}

fn consume(s: &str) -> usize {
    s.len()
}

// ── the shadowed shapes: none of these calls the item ────────────────────

pub fn by_parameter(logfile: &str) -> usize {
    consume(logfile)
}

pub fn by_let() -> usize {
    let logfile = "x";
    consume(logfile)
}

pub fn by_match_arm(m: Option<&str>) -> usize {
    match m {
        Some(logfile) => consume(logfile),
        None => 0,
    }
}

pub fn by_for_pattern(v: Vec<&str>) -> usize {
    let mut n = 0;
    for logfile in v {
        n += consume(logfile);
    }
    n
}

pub fn by_if_let(m: Option<&str>) -> usize {
    if let Some(logfile) = m {
        consume(logfile)
    } else {
        0
    }
}

pub fn by_closure_head(v: Vec<&str>) -> usize {
    v.into_iter().map(|logfile| consume(logfile)).sum()
}

/// A parameter named `spell` shadows the one-argument item of that name.
pub fn by_parameter_of_a_fn_with_args(spell: &str) -> usize {
    consume(spell)
}

// ── the genuine references, which must stay `resolved` ───────────────────

pub fn calls_the_item() -> bool {
    helpers::logfile().is_some()
}

/// A real fn-reference: `keep_it` is bound nowhere, so this is the shape the
/// fn-ref feature exists for and must keep finding.
pub fn hands_over_a_fn(v: Vec<Option<String>>) -> usize {
    v.into_iter().filter(keep_it).count()
}

fn keep_it(x: &Option<String>) -> bool {
    x.is_some()
}

/// A qualified fn-reference. No local can capture a path with a `::` in it,
/// however many bindings share the last segment.
pub fn hands_over_a_qualified_fn(v: Vec<&str>) -> usize {
    v.into_iter().map(helpers::spell).sum()
}

/// The iterated expression sits *outside* the binding the loop introduces, so
/// this call is the item even though the loop variable shares its name.
pub fn the_head_of_a_for_loop_is_not_shadowed() -> usize {
    let mut n = 0;
    for logfile in [helpers::logfile()] {
        n += logfile.map_or(0, |s| s.len());
    }
    n
}

/// A block-level `fn` item shadows a parameter of the same name, so a bare
/// call here names the item after all — `self-check` must not report it.
pub fn a_nested_item_wins_over_a_parameter(shim: u8) -> u8 {
    fn shim() -> u8 {
        7
    }
    shim() + 0 * u8::from(shim > 0)
}
