//! Fixture for `divergence`, `// unruster: ok` waivers, and cast classing.
//! Kept separate from `fixtures/sample` so adding cases here can't shift the
//! row counts that the sample-fixture tests assert on.

use std::sync::Mutex;

static OPEN: Mutex<u32> = Mutex::new(0);

/// Two siblings that disagree about a poisoned lock — the shape that produced
/// a real defect: the reader recovered with a warning, the writer dropped the
/// value on the floor.
pub mod openfile {
    use super::OPEN;

    pub fn take_open_file() -> u32 {
        *OPEN.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn handle_open_file(v: u32) {
        if let Ok(mut g) = OPEN.lock() {
            *g = v;
        }
    }
}

pub enum Anchor {
    Corner,
    Knot,
    MiddleKnot,
}

/// Edit-mode delete handles two of three anchor kinds…
pub fn handle_anchor_delete(a: Anchor) -> bool {
    match a {
        Anchor::Knot | Anchor::MiddleKnot => true,
        _ => false,
    }
}

/// …and its animate-mode twin forgot `MiddleKnot`. One-variant gap between
/// siblings: the canonical divergence row.
pub fn handle_anchor_delete_anim(a: Anchor) -> bool {
    matches!(a, Anchor::Knot)
}

pub mod swallows {
    use std::fmt::Write;

    /// Infallible: `write!` into a `String` cannot fail.
    pub fn render(items: &[u32]) -> String {
        let mut s = String::new();
        for i in items {
            let _ = write!(s, "{}", i);
        }
        s
    }

    pub fn cleanup(p: &std::path::Path) {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_dir(p); // unruster: ok — best-effort, absence is fine
    }

    /// A fallback that logs is a policy, not a silent drop.
    pub fn parse_or_warn(s: &str) -> u32 {
        s.parse().unwrap_or_else(|_| {
            eprintln!("bad number {s}, defaulting to 0");
            0
        })
    }
}

pub mod casting {
    /// A local `width()` returning f64. A name-only method-return guess would
    /// type every `.width()` in the tree as f64 — including the u32 one below.
    pub struct Rect {
        pub w: f64,
    }
    impl Rect {
        pub fn width(&self) -> f64 {
            self.w
        }
    }

    pub struct Bitmap;
    impl Bitmap {
        pub fn width(&self) -> u32 {
            0
        }
    }

    pub const BYTES_PER_PIXEL: u32 = 4;

    /// Source type must come out `_`, never `f64`.
    pub fn stride(b: &Bitmap) -> usize {
        let w = b.width();
        let unpadded = w * BYTES_PER_PIXEL;
        unpadded as usize
    }

    pub fn widening(n: u32) -> usize {
        n as usize
    }
    pub fn lossy(f: f64) -> usize {
        f as usize
    }
    pub fn narrowing(n: u64) -> u32 {
        n as u32
    }
}
