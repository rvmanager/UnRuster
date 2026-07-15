//! Exercises rarely-encountered AST shapes that drive uncovered branches in
//! ast.rs (vis_str, type_to_string, type_last_segment, is_mut_ref,
//! has_test_attr, item_attrs, type_short) and index.rs (item visitors for
//! const/static/type/trait/extern).

// ── pub(restricted) visibility variants ─────────────────────────────────
pub(crate) struct CrateOnly;
pub(super) struct SuperOnly;
pub(self) struct SelfVis;
pub(in crate) struct InPath;

// ── unusual top-level items (item_attrs branches) ───────────────────────
pub const C: u32 = 1;
pub static S: u32 = 2;
pub type AliasFnPtr = fn(u8) -> u32;
pub type AliasTuple = (u8, u16, u32);
pub type AliasSlice = [u8];
pub type AliasArray = [u8; 8];
pub type AliasDyn = dyn std::fmt::Display;
pub type AliasFnTrait = Box<dyn Fn(u8) -> u32>;
pub type AliasNever = !;
pub type AliasQSelf = <Vec<u8> as IntoIterator>::Item;
pub type AliasRefMut = *mut u8;
pub type AliasConst = *const u8;

pub union U { _a: u8, _b: u16 }

extern crate core as _core;

extern "C" {
    pub fn extern_decl();
}

pub trait NoMethods {}

// ── generics + lifetimes + const generics (path_to_string_with_args) ────
pub struct WithLt<'a, const N: usize> { _r: &'a [u8; N] }

pub fn lifetimed<'a>(x: &'a u8) -> &'a u8 { x }

pub fn returns_impl_trait() -> impl Iterator<Item = u8> { std::iter::empty() }

// ── Paren/Group through reference (is_mut_ref, type_last_segment edges) ─
pub fn takes_paren_ref(_x: &(u8)) {}
pub fn takes_paren_mut(_x: &mut (u8)) {}

// ── ImplItem const / type / fn (impl-level non-fn variants) ─────────────
pub struct Holder;
impl Holder {
    pub const INNER: u32 = 0;
    pub fn helper(&self) -> u32 { Self::INNER }
}

pub trait HasItems {
    const TC: u32;
    type Item;
    fn need(&self) -> Self::Item;
}

// ── #[test] direct attribute (not just cfg(test)) ───────────────────────
#[test]
fn direct_test_attr() {
    assert_eq!(1, 1);
}

#[bench]
fn direct_bench_attr() {}

// ── Allow-dead at item AND impl-block level ─────────────────────────────
#[allow(unused, dead_code)]
pub fn item_allow_dead() {}

#[allow(dead_code)]
impl SelfVis {
    pub fn h1() {}
    pub fn h2() {}
}

// ── Single-segment `use` (UseTree::Name + Rename, empty prefix) ─────────
use core;
use core as core_alias;

// ── Glob and nested use group ───────────────────────────────────────────
mod inner_glob {
    pub struct G1;
    pub struct G2;
}
use inner_glob::*;
use std::collections::{HashMap, BTreeMap as BMap};

pub fn use_glob_items(_g1: G1, _g2: G2) {}

// ── Const generic in fn signature ───────────────────────────────────────
pub fn const_param<const N: usize>(_: [u8; N]) {}

// ── Trait object in arg (TraitObject path through type_short Reference) ──
pub fn takes_dyn(_r: &dyn std::fmt::Display) {}

// ── BareFn in arg ───────────────────────────────────────────────────────
pub fn takes_fn_ptr(_f: fn(u8) -> u32) {}

// ── Tuple struct + Unit struct ──────────────────────────────────────────
pub struct TupleS(pub u8, pub u16);
pub struct UnitS;

// ── Struct field types exercising every type_to_string branch ──────────
pub struct ExoticFields {
    pub tup: (u8, u16, u32),                            // Tuple
    pub arr: [u8; 4],                                   // Array
    pub slc: Box<[u8]>,                                 // Slice via Box
    pub ptr_c: *const u8,                               // Ptr const
    pub ptr_m: *mut u32,                                // Ptr mut
    pub d_obj: Box<dyn std::fmt::Display>,              // TraitObject
    pub fn_ptr: fn(u8) -> u32,                          // BareFn
    pub closure: Box<dyn Fn(u8) -> u32>,                // Parenthesized path args
    pub qself: <Vec<u8> as IntoIterator>::Item,         // Type::Path with QSelf
    pub leading: ::std::collections::HashMap<u8, u32>,  // leading `::`
    pub generic_lt: Vec<&'static str>,                  // Lifetime arg
    pub never_returner: fn() -> !,                      // Never type
    pub paren_t: (u8),                                  // Paren type (single elem tuple syntax)
}

// ── Visit qualified `lookup()` via glob (exercises index::NameIndex::lookup) ──
pub fn use_glob_call() {
    let _g1 = G1;
    let _g2 = G2;
    let _t = TupleS(1, 2);     // tuple-struct ctor — exercises type_refs.rs len==1 branch
    let _b = BMap::new();       // renamed alias — exercises resolve_target_via_uses
    let _abs = ::std::process::id();  // leading `::` path — exercises path_to_string lead-colon
}

// ── pub(in <multi-segment-path>) — exercises vis_str "pub(in ...)" branch ──
mod restricted_mod {
    pub(in crate::restricted_mod) struct InMultiSeg;
}

// ── `&mut (T)` for is_mut_ref Paren branch ─────────────────────────────
pub fn paren_mut_param(_x: &mut (u8)) {}

// ── trait with default method (visit_item_trait body + has_test_attr false-path) ──
pub trait HasDefault {
    fn default_method(&self) -> u32 { 0 }
}

// ── impl Trait for &T — exercises `type_short` Reference branch ────────
impl HasDefault for &u8 {
    fn default_method(&self) -> u32 { **self as u32 }
}

// ── impl Trait for [u8; N] — fallback `type_short` → type_to_string ────
impl HasDefault for [u8; 4] {
    fn default_method(&self) -> u32 { self.len() as u32 }
}

// ── pass-through wrappers in all describe_call shapes ──────────────────
pub fn wrap_method(d: super::Document) -> String { d.render() }
pub fn wrap_macro_call() -> String { format!("x") }

// ── sealed-enum contract marker (unruster: sealed) ──────────────────────
/// Gear states for the sealed-marker tests.
/// unruster: sealed
pub enum SealedGear {
    Park,
    Drive,
    Reverse,
}

/// A partial predicate on a sealed enum — must be tagged SEALED.
pub fn gear_is_moving(g: &SealedGear) -> bool {
    matches!(g, SealedGear::Drive | SealedGear::Reverse)
}

/// Keeps the variants constructed so `variants` sees ctor sites too.
pub fn all_gears() -> Vec<SealedGear> {
    vec![SealedGear::Park, SealedGear::Drive, SealedGear::Reverse]
}
