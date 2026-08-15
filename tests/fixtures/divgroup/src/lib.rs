//! One lean site against several richer siblings: the arena shape that turned
//! three decisions into seventeen rows before grouping.
pub enum N { Group, Composite, Guide, Xform }
pub struct Arena;

impl Arena {
    pub fn insert_refs(&self, n: &N) -> u8 {
        match n { N::Composite => 1, N::Guide => 2, N::Xform => 3, _ => 0 }
    }
    pub fn logical_refs(&self, n: &N) -> u8 {
        match n { N::Group => 1, N::Composite => 2, N::Guide => 3, _ => 0 }
    }
    pub fn reachable_refs(&self, n: &N) -> u8 {
        match n { N::Group => 1, N::Composite => 2, N::Xform => 3, _ => 0 }
    }
    pub fn referrer_refs(&self, n: &N) -> u8 {
        match n { N::Group => 1, N::Guide => 2, N::Xform => 3, _ => 0 }
    }
}
