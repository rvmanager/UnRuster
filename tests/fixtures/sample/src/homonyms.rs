//! A free fn whose bare name is also a method on a type this tool never
//! indexes. `contract-drift --candidates` guards against a name with more than
//! one definition *here*, which is the wrong axis: one project's private
//! `geom::boolean::collect` has exactly one definition in its tree, and was
//! ranked 7th of 286 with "475 callers across 40 modules" — every one of them
//! an `Iterator::collect`. The evidence that settles it is already in the call
//! sites: a free fn is never called as `.name()`.

/// One genuine caller. Every other `collect` below is written `.collect()`.
pub fn collect(v: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(v);
}

pub fn the_only_real_caller(v: &[u8]) -> Vec<u8> {
    let mut o = Vec::new();
    collect(v, &mut o);
    o
}

pub fn iterator_collect_one(v: Vec<u8>) -> Vec<u8> {
    v.into_iter().collect()
}

pub fn iterator_collect_two(v: Vec<u8>) -> Vec<u8> {
    v.into_iter().collect()
}

pub fn iterator_collect_three(v: Vec<u8>) -> Vec<u8> {
    v.into_iter().collect()
}

pub fn iterator_collect_four(v: Vec<u8>) -> Vec<u8> {
    v.into_iter().collect()
}
