//! The other `twinned`, in an unrelated module. A query for
//! `glob_parent::nested::twinned` must not be answered with this one alone.

pub fn twinned() -> usize {
    2
}
