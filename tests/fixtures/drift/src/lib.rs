//! Fixture for `config-drift`, modelled on the real defect: two functions in
//! two modules building the same options struct to configure the same
//! operation, with the configurations quietly diverging.

#[derive(Default, Clone, Copy)]
pub struct Opts {
    pub hide_routed: bool,
    pub min_variants: usize,
    pub max_missing: Option<usize>,
    pub compact: bool,
    pub rank: bool,
}

/// Two presets of the same struct that agree on *nothing*. The motivating
/// defect had exactly this shape, and an `agreement`-multiplied score dropped
/// it to 0.0.
pub mod gating {
    use super::Opts;
    pub fn build() -> Opts {
        Opts {
            hide_routed: true,
            min_variants: 3,
            max_missing: Some(1),
            ..Default::default()
        }
    }
}

pub mod probe {
    use super::Opts;
    pub fn build() -> Opts {
        Opts {
            hide_routed: false,
            min_variants: 0,
            max_missing: None,
            compact: true,
            rank: false,
        }
    }
}

/// A site that sets every field from a parameter abstains rather than
/// suppressing the comparison for everyone else.
pub mod cli {
    use super::Opts;
    pub fn build(a: bool, n: usize) -> Opts {
        Opts {
            hide_routed: a,
            min_variants: n,
            max_missing: None,
            compact: a,
            rank: a,
        }
    }
}

/// A type's own constructors are its API, not drift.
pub struct Sink {
    pub quiet: bool,
    pub width: usize,
}
impl Sink {
    pub fn new() -> Self {
        Sink { quiet: false, width: 80 }
    }
    pub fn silent() -> Self {
        Sink { quiet: true, width: 80 }
    }
}
impl Default for Sink {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixture for `builder-drift`: sibling chains on one constructor, one missing
/// a step. Modelled on the real defect — two `git` invocations, one of which
/// forgot to say which directory to run in.
pub mod chains {
    pub struct Cmd(pub String);
    impl Cmd {
        pub fn new(p: &str) -> Self { Cmd(p.into()) }
        pub fn args(self, _a: &[&str]) -> Self { self }
        pub fn dir(self, _d: &str) -> Self { self }
        pub fn run(self) -> String { self.0 }
    }

    pub fn resolve() -> String {
        // Forgot `.dir()`, so this runs wherever the caller happens to be.
        Cmd::new("git").args(&["rev-parse"]).run()
    }

    pub fn archive(d: &str) -> String {
        Cmd::new("git").args(&["archive"]).dir(d).run()
    }

    /// A different program entirely — must not be compared with the two above.
    pub fn extract(d: &str) -> String {
        Cmd::new("tar").args(&["-x"]).dir(d).run()
    }
}

/// Precision cases measured on a real codebase, where seven of nine top rows
/// were one of these two shapes.
pub mod precision {
    pub mod inner { #[derive(Clone, Copy)] pub enum Margin { Percent(u32) } }

    /// wgpu-style descriptors differ in `label` by design — that is what the
    /// field is for. Five of ten top rows on a real audit were exactly this.
    pub struct Desc { pub label: Option<&'static str>, pub layers: u32 }
    pub fn pipeline_a() -> Desc { Desc { label: Some("glass"), layers: 1 } }
    pub fn pipeline_b() -> Desc { Desc { label: Some("glass-hl"), layers: 1 } }

    /// The *same value*, written two ways. Reported as a 0.56 drift until
    /// paths were compared by item rather than by import spelling.
    pub struct Pending { pub margin: inner::Margin, pub scale: u32 }
    pub fn near() -> Pending { Pending { margin: inner::Margin::Percent(0), scale: 1 } }
    pub fn far() -> Pending {
        Pending { margin: crate::precision::inner::Margin::Percent(0), scale: 1 }
    }
}
