//! Fixture for `arith-drift` and `panics`.
//!
//! The arithmetic half reproduces the shape the check was written for: a
//! function where most of the terms saturate and one does not.

/// Three saturating adds and one raw `+` — the odd one out, at 0.75.
///
/// Modelled on the RFC 9111 age calculation that a real fix changed from `+`
/// to `saturating_add`, in a function where the neighbouring terms already
/// saturated. Note the grouping is per *operator*: the `saturating_sub` is a
/// sibling of subtractions, not of these additions.
pub fn corrected_age(apparent: u64, response_delay: u64, resident: u64, initial: u64) -> u64 {
    let corrected_initial = apparent.saturating_add(response_delay);
    let corrected = corrected_initial.saturating_sub(initial);
    let with_delay = corrected.saturating_add(response_delay);
    let total = with_delay.saturating_add(initial);
    // The drift: every sibling term saturates, this one wraps.
    total + resident
}

/// An even split: two checked adds and two raw ones. Two different jobs in one
/// scope, so it scores 0.5 — reported by the bare command, below the audit's
/// floor. Note the siblings must share an *operator* to be siblings at all.
pub fn split(a: u32, b: u32, index: u32, len: u32) -> u32 {
    let sum = a.checked_add(b).unwrap_or(u32::MAX);
    let room = len.checked_add(index).unwrap_or(0);
    sum + index + room
}

/// A single checked call is one call, not a convention — nothing to be the odd
/// one out from, so this scope reports nothing whatever the raw count.
pub fn lone_checked(a: u32, b: u32, c: u32, d: u32) -> u32 {
    let x = a.saturating_add(b);
    x + c + d
}

/// String concatenation in a scope that also saturates. Has no checked sibling
/// and must not be reported as drift.
pub fn label(prefix: &str, n: u64, m: u64) -> String {
    let _total = n.saturating_add(m).saturating_mul(2);
    prefix.to_string() + "-suffix"
}

// ── panics ───────────────────────────────────────────────────────────────

/// `.unwrap()` on a parse of a caller-supplied string: the crash class that a
/// changelog's worth of "report X instead of panicking" fixes all shared.
pub fn port_of(raw: &str) -> u16 {
    raw.parse::<u16>().unwrap()
}

/// The idiomatic families, hidden unless `--include-idiomatic`.
pub fn idiomatic(state: &std::sync::Mutex<u32>) -> u32 {
    let literal: u16 = "8080".parse().unwrap();
    let guard = state.lock().unwrap();
    *guard + literal as u32
}

/// Ships as a crash on a path someone can reach.
pub fn unfinished(_kind: u8) -> u32 {
    todo!("variant routing")
}
