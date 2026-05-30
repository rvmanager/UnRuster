//! Fixture for `==` / if-else-if dispatch-chain detection (enum-coverage,
//! parallel-matches --include-if-chains). Each fn below is one detection shape.

/// Unit-variant enum so `==` dispatch needs no constructed payload.
#[derive(PartialEq)]
pub enum Mode {
    A,
    B,
    C,
    D,
}

/// Two-arm chain with an explicit trailing `else` → catch-all, 2 variants.
pub fn two_arm_with_else(m: &Mode) -> u8 {
    if *m == Mode::A {
        1
    } else if *m == Mode::B {
        2
    } else {
        0
    }
}

/// Two-arm chain with NO trailing else → still a partial site (catch-all false).
pub fn two_arm_no_else(m: &Mode) -> u8 {
    if *m == Mode::A {
        1
    } else if *m == Mode::B {
        2
    }
    0
}

/// Three-arm chain → 3 variants covered.
pub fn three_arm(m: &Mode) -> u8 {
    if *m == Mode::A {
        1
    } else if *m == Mode::B {
        2
    } else if *m == Mode::C {
        3
    } else {
        0
    }
}

/// Mixed scrutinee → not a dispatch chain (lhs differs), nothing emitted.
pub fn mixed_scrutinee(m: &Mode, n: &Mode) -> u8 {
    if *m == Mode::A {
        1
    } else if *n == Mode::B {
        2
    } else {
        0
    }
}

/// Negated chain → `!=` is not `==`, nothing emitted.
pub fn negated(m: &Mode) -> u8 {
    if *m != Mode::A {
        1
    } else if *m != Mode::B {
        2
    } else {
        0
    }
}

/// Single guard (no else-if) → length 1, nothing emitted.
pub fn single_guard(m: &Mode) -> u8 {
    if *m == Mode::A {
        1
    } else {
        0
    }
}

/// Reversed operand order `Mode::A == x` → still emitted.
pub fn reversed(m: &Mode) -> u8 {
    if Mode::A == *m {
        1
    } else if Mode::B == *m {
        2
    } else {
        0
    }
}

/// Nested chain: the outer chain's first arm body contains its own `==` chain.
/// Outer emitted as one site (A,B); inner emitted as its own site (C,D).
pub fn nested(m: &Mode, inner: &Mode) -> u8 {
    if *m == Mode::A {
        if *inner == Mode::C {
            10
        } else if *inner == Mode::D {
            11
        } else {
            12
        }
    } else if *m == Mode::B {
        2
    } else {
        0
    }
}

/// Trait-routed catch-all: the `else` routes through a method on the scrutinee,
/// so a new variant is picked up automatically — a structural false positive.
pub trait Classify {
    fn rank(&self) -> u8;
}
impl Classify for Mode {
    fn rank(&self) -> u8 {
        0
    }
}
pub fn trait_routed_else(m: &Mode) -> u8 {
    if *m == Mode::A {
        1
    } else if *m == Mode::B {
        2
    } else {
        m.rank()
    }
}

// ── Vectorian-like dispatcher: 17 unit variants, only 2 covered (2/17). ──────

#[derive(PartialEq)]
pub enum DragHandle {
    Center,
    Rotation,
    Start,
    End,
    Line,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Top,
    Bottom,
    Left,
    Right,
    Skew,
    Scale,
    Anchor,
    Origin,
}

/// Mirrors `apply_static_handle_drag_to_doc`'s pre-fix shape: a 2-arm chain that
/// silently routes the other 15 variants (including Start/End) into the `else`.
pub fn apply_static_handle_drag(h: &DragHandle) -> u8 {
    if *h == DragHandle::Center {
        1
    } else if *h == DragHandle::Rotation {
        2
    } else {
        0
    }
}
