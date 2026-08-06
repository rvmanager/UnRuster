//! Fixture for the waiver grammar: `// unruster: ok(<check>[/<key>]) <date> —
//! <reason>`. Exercises item scope, variant keys, key mismatch, wrapped
//! reasons, the legacy spelling, and an orphaned waiver.
//!
//! Kept separate from `fixtures/divergence` so adding cases here can't shift
//! the row counts those tests assert on.

pub enum Node {
    Group,
    Composite,
    Guide,
    Xform,
}

pub struct Arena;

impl Arena {
    /// Rich side of the divergence pair: three of four variants.
    pub fn drop_outgoing_refs(&self, n: &Node) -> u8 {
        match n {
            Node::Group => 1,
            Node::Composite => 2,
            Node::Guide => 3,
            _ => 0,
        }
    }

    // The case that motivated variant-keyed waivers: one comment, attached to
    // the lean side, retiring this omission against every sibling at once.
    // The reason wraps across three lines to prove the parser doesn't depend
    // on line boundaries.
    // unruster: ok(divergence/Node::Group) 2026-01-10 — Group is a structural
    // child edge, not a consumer reference; every consumer walk in this impl
    // excludes it deliberately.
    pub fn strip_incoming_refs(&self, n: &Node) -> u8 {
        match n {
            Node::Composite => 2,
            Node::Guide => 3,
            _ => 0,
        }
    }
}

pub mod swallow {
    /// Site-scoped and keyed: waives this `let-_` and nothing else.
    pub fn cleanup(p: &std::path::Path) {
        let _ = std::fs::remove_file(p); // unruster: ok(error-swallows/let-_) 2026-02-01 — absence is fine
    }

    /// A keyed waiver naming a kind this line does not have. It must NOT
    /// suppress — an over-broad key match would silently hide real findings.
    pub fn wrong_key(p: &std::path::Path) {
        let _ = std::fs::remove_dir(p); // unruster: ok(error-swallows/.ok) 2026-02-01 — deliberately the wrong kind
    }

    /// The pre-grammar spelling: no check, no date. Still honoured (it waives
    /// every check on its line); `waivers --upgrade` is what qualifies it.
    pub fn legacy(p: &std::path::Path) {
        let _ = std::fs::remove_dir_all(p); // unruster: ok — best effort
    }

    /// A trailing waiver whose reason continues onto the next line —
    /// `--remove` has to take the stranded prose with it.
    pub fn trailing_wrapped(p: &std::path::Path) {
        let _ = std::fs::create_dir(p); // unruster: ok(error-swallows/let-_) 2026-03-03 — the
        // directory already existing is the common case, not an error.
    }
}

pub mod orphan {
    // unruster: ok(error-swallows/let-_) 2019-05-05 — the swallow this described is gone
    pub fn nothing_here() -> u32 {
        7
    }
}
