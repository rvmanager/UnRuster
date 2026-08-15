//! Drives semantic.rs's TypeInferVisitor / FnSigIndex / AliasGraph paths
//! and stringly + error_swallows + conversion patterns we missed.

// ── Alias chain (forward + back) ────────────────────────────────────────
pub struct Atom;
pub type A1 = Atom;
pub type A2 = A1;
pub type A3 = A2;

// ── FnSigIndex: two fns same name different return types → ambiguous ────
pub fn make_a() -> Atom { Atom }
mod variant {
    pub fn make_a() -> u32 { 0 }
}

// ── Local type inference: param + typed let + constructor + fn-call lookup
pub fn infer_paths(x: Atom, n: u8) -> u32 {
    let typed: Atom = make_a();          // typed let
    let from_ctor = Atom;                // unit struct ctor (Expr::Path)
    let from_call = make_a();            // fn return-type inference
    let from_struct = Atom { /* none */ };
    let r = &typed;                       // Reference, infer through
    let p = (r);                          // Paren, infer through
    let _q = p;
    let casted = n as u32;                // Cast — typed via target
    let optional: Option<u8> = Some(n);
    let v = optional?;                    // Try operator
    let _ = (x, from_ctor, from_call, from_struct, casted, v);
    0
}

// ── New variants of `.from()` / `Type::from_str()` etc. ─────────────────
pub fn ctor_kinds() {
    let _: String = String::from_str("x").unwrap();
    let _: Vec<u8> = Vec::with_capacity(4);
    let _: String = String::empty();
}

pub trait Build {
    fn empty() -> Self where Self: Sized;
}

impl Build for String {
    fn empty() -> Self { String::new() }
}

// ── Or-pattern in match arms (catch_all + parallel_matches Or branches) ─
pub enum Sig { Up, Down, Left, Right }

pub fn or_patterns(s: Sig) -> u8 {
    match s {
        Sig::Up | Sig::Down => 1,
        Sig::Left | Sig::Right => 2,
    }
}

pub fn or_with_wildcard(s: Sig) -> u8 {
    match s {
        Sig::Up => 0,
        Sig::Down | _ => 1,    // Or that contains wildcard
    }
}

// ── Struct + Reference patterns in match (catch_all.pattern_targets_variant)
pub enum Shape {
    Circle { r: u32 },
    Square(u32),
    Point,
}

pub fn shape_match(s: &Shape) -> u32 {
    match s {
        &Shape::Circle { r } => r,
        Shape::Square(x) => *x,
        Shape::Point => 0,
    }
}

// ── String literal in if-let pattern (stringly if-let-ok variant) ───────
pub fn if_let_str(mode: &str) -> u8 {
    if let "edit" | "view" = mode { 1 } else { 0 }
}

// ── matches! with guard (macro_scan strip_match_guard path) ─────────────
pub fn match_guard(x: i32) -> bool {
    matches!(x, n if n > 0)
}

// ── second bidirectional From pair (conversion_pairs sort comparator) ──
impl From<i32> for Atom { fn from(_: i32) -> Atom { Atom } }
impl From<Atom> for i32 { fn from(_: Atom) -> i32 { 0 } }

// ── exercise infer_expr_type Call branches: Type::new / ::with_capacity / etc.
impl Atom { pub fn new() -> Atom { Atom } }

pub fn ctor_inference() {
    let _a = Atom::new();                   // Type::new pattern
    let _v = Vec::with_capacity(8);         // Type::with_capacity
    let _h: std::collections::HashMap<u8, u8> = std::collections::HashMap::default();
    let _s = String::from("x");
    let _multi_path = std::process::abort;   // multi-segment Expr::Path (returns None)
    let _ = _multi_path;
    let _typed_collect: Vec<u32> = [1u8, 2, 3].iter().map(|&x| x as u32).collect::<Vec<u32>>();
}

/// Token match inside a non-empty-module file — exercises catch_all module
/// + parallel_matches module + RefVisitor::enclosing module branch.
pub fn token_in_module(t: &super::Token) -> u8 {
    match t {
        super::Token::Eof => 0,
        super::Token::Word(_) => 1,
        _ => 99,
    }
}

/// Single-segment variant via `use Token::*;` — exercises SiteVisitor `--bare` branch.
pub fn bare_variant_use() {
    use super::Token::Eof;
    let _ = Eof;
}

// ── stringly variants that drive remaining branches in stringly.rs ─────
pub fn stringly_kitchen(action: &str, role: &str) -> u8 {
    if "save" == action { return 1; }                              // LHS literal form
    let _msg_with_long_literal = action == "very_long_string_literal_more_than_32_chars_to_exercise_truncate_lit";
    let _ = action;
    assert_eq!(role, "admin");                                      // assert_eq! special case
    assert_ne!(role, "guest");                                      // assert_ne! special case
    debug_assert_eq!(role, "user");                                 // debug_assert_eq! special case
    0
}

// Stringly inside an impl block — exercises StringlyVisitor::enclosing
// impl_stack non-empty branch.
pub struct Dispatcher;
impl Dispatcher {
    pub fn handle(&self, action: &str) -> u8 {
        match action {
            "ping" => 1,
            _ => 0,
        }
    }
}
