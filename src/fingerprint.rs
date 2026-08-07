//! Stable identity for a finding, so two runs can be compared.
//!
//! Findings are naturally keyed `file:line`, and that key is worthless across
//! an edit: inserting three lines at the top of a file "moves" every finding
//! below it, and a naive diff of two runs reports each one as a deletion plus
//! an addition. Measured on this codebase, five of six apparently-new findings
//! after a round of fixes were pure line shift. An agent cannot honestly say
//! "fixed 3, introduced 0" from that.
//!
//! The fingerprint therefore contains **no line number**. It is a hash of:
//!
//! * the check that produced the row,
//! * every identifying cell of the row (the enclosing fn, the variant, the
//!   cast class, …) — but not measurements or scores,
//! * the normalized source text of the flagged line.
//!
//! # What changes it, and why that is right
//!
//! | edit                      | fingerprint |
//! |---------------------------|-------------|
//! | insert lines above        | unchanged   |
//! | edit the flagged line     | changes — a fix, or a different finding |
//! | add a waiver              | gone        |
//! | rename or move the fn     | changes — reported as `moved`, not `fixed` |
//! | reformat the whole file   | changes — the honest limitation |
//!
//! Two identical lines inside one function collide on purpose. They are
//! interchangeable, so the differ compares *counts* per fingerprint rather than
//! trying to pair them up.
//!
//! # Stability
//!
//! FNV-1a rather than `DefaultHasher`: std makes no promise that its hasher's
//! output is stable across releases, and a baseline file that silently
//! invalidates on a Rust upgrade is worse than no baseline. [`SCHEME`] is mixed
//! in and written to baseline files, so changing what goes into the hash
//! reports "baseline from a different scheme" instead of "everything is new".

use crate::emit::Val;

/// Bump when the hash input changes. Baselines record it; a mismatch is
/// reported rather than silently producing a wall of false `new` rows.
pub const SCHEME: u32 = 1;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Strip everything from a cell that varies without the finding changing.
///
/// * `name@120-140` → `name`. `--spans` re-embeds line numbers in the fn
///   label; leaving them in would undo the whole point.
/// * `cyclo:58` → `cyclo`. `metrics` renders its measurements as cells; the
///   identity of that row is "this fn is over the threshold", not the exact
///   number, which shifts on any edit inside the fn.
/// * whitespace collapses, so reindentation is not a change.
pub fn normalize(s: &str) -> String {
    let s = s.split('@').next().unwrap_or(s);
    let s = match s.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => s,
    };
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The identifying part of a row: text cells, normalized. Measurements
/// (`Num`/`Float`/`Bool`) and the `Site` itself are excluded — the first
/// carries values that drift, the second is the line number we are trying to
/// stop depending on.
fn identity_cells(cells: &[(&'static str, Val)]) -> Vec<String> {
    cells
        .iter()
        .filter_map(|(_, v)| match v {
            Val::Str(s) => Some(normalize(s)),
            Val::List(items) => Some(
                items
                    .iter()
                    .map(|i| normalize(i))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            // `Span` joins `Site` here for the same reason: it is line numbers,
            // which is exactly what a fingerprint must not depend on.
            Val::Num(_)
            | Val::Float(_)
            | Val::Bool(_)
            | Val::Site { .. }
            | Val::Span { .. } => None,
        })
        .collect()
}

/// Twelve hex chars: enough that a collision inside one codebase is not a
/// practical concern, short enough to sit in a TSV column.
pub fn of(check: &str, cells: &[(&'static str, Val)], site_text: Option<&str>) -> String {
    let mut input = String::with_capacity(128);
    input.push_str(&SCHEME.to_string());
    input.push('\u{1}');
    input.push_str(check);
    for c in identity_cells(cells) {
        input.push('\u{1}');
        input.push_str(&c);
    }
    if let Some(t) = site_text {
        input.push('\u{1}');
        input.push_str(&normalize(t));
    }
    format!("{:012x}", fnv1a(input.as_bytes()) & 0xffff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(vals: &[(&'static str, Val)]) -> Vec<(&'static str, Val)> {
        vals.to_vec()
    }

    #[test]
    fn spans_and_measurements_are_stripped() {
        assert_eq!(normalize("app::foo::bar@120-140"), "app::foo::bar");
        assert_eq!(normalize("cyclo:58"), "cyclo");
        assert_eq!(normalize("  a   b  "), "a b");
        // A qualified name ending in a segment, not a number, survives intact.
        assert_eq!(normalize("NodeContent::Group"), "NodeContent::Group");
    }

    #[test]
    fn the_line_number_does_not_participate() {
        let a = cells(&[
            ("kind", Val::Str("let-_".into())),
            ("context", Val::Str("foo::bar".into())),
            ("at", crate::emit::site("src/x.rs", 12)),
        ]);
        let b = cells(&[
            ("kind", Val::Str("let-_".into())),
            ("context", Val::Str("foo::bar".into())),
            ("at", crate::emit::site("src/x.rs", 900)),
        ]);
        assert_eq!(
            of("error-swallows", &a, Some("let _ = f();")),
            of("error-swallows", &b, Some("let _ = f();")),
            "inserting lines above a finding must not change its identity"
        );
    }

    #[test]
    fn measurements_drifting_does_not_change_identity() {
        let mk = |cyclo: &str| {
            cells(&[
                ("kind", Val::Str("fn".into())),
                ("cyclo", Val::Str(cyclo.into())),
                ("qpath", Val::Str("app::big".into())),
                ("at", crate::emit::site("src/x.rs", 5)),
            ])
        };
        assert_eq!(
            of("metrics", &mk("cyclo:58"), Some("fn big() {")),
            of("metrics", &mk("cyclo:57"), Some("fn big() {")),
            "a fn that is still over threshold is still the same finding"
        );
    }

    #[test]
    fn editing_the_flagged_line_is_a_different_finding() {
        let c = cells(&[
            ("kind", Val::Str("let-_".into())),
            ("at", crate::emit::site("src/x.rs", 12)),
        ]);
        assert_ne!(
            of("error-swallows", &c, Some("let _ = f();")),
            of("error-swallows", &c, Some("let _ = g();")),
        );
    }

    #[test]
    fn the_check_name_separates_otherwise_identical_rows() {
        let c = cells(&[("context", Val::Str("foo::bar".into()))]);
        assert_ne!(of("casts", &c, None), of("stringly", &c, None));
    }

    #[test]
    fn fingerprints_are_stable_across_process_runs() {
        // Hand-computed against the FNV-1a definition rather than a recorded
        // value, so this fails if the algorithm is swapped for something whose
        // stability std does not promise.
        assert_eq!(fnv1a(b""), FNV_OFFSET);
        let expect = {
            let mut h = FNV_OFFSET;
            for b in b"abc" {
                h ^= u64::from(*b);
                h = h.wrapping_mul(FNV_PRIME);
            }
            h
        };
        assert_eq!(fnv1a(b"abc"), expect);
    }
}
