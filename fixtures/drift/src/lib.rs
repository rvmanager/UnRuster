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
