//! Integration-test fixture for unruster.
//! Crafted to exercise every subcommand's detection paths in one file.

use crate::inner::Renamed;

pub struct Document {
    pub transform: [f32; 4],
    pub name: String,
    children: Vec<Document>,
}

pub struct Boxx {
    pub transform: f32,
}

pub type Doc = Document;
pub type DocAlias = Doc;

pub enum Token {
    Eof,
    Word(String),
    Number(i64),
    Resize { w: u32, h: u32 },
}

/// Top-level variant constant — exercises SiteVisitor::enclosing top-level branch.
pub const SAMPLE_TOKEN: Token = Token::Eof;

pub trait Render {
    fn render(&self) -> String;
}

impl Render for Document {
    fn render(&self) -> String {
        self.name.clone()
    }
}

impl Document {
    pub fn new(name: String) -> Self {
        Self {
            transform: [0.0; 4],
            name,
            children: vec![],
        }
    }

    pub fn touch(&mut self) {
        self.transform[0] += 1.0;
    }

    pub fn show(&self) {
        println!("doc: {} = {:?}", self.name, self.transform);
    }

    pub fn add_child(d: &mut Document, c: Document) {
        d.children.push(c);
    }

    /// Match-on-enum inside an `impl` block — exercises catch_all + parallel_matches
    /// when impl_stack is non-empty (RefVisitor::enclosing impl_stack branch).
    pub fn classify_token(&self, t: &Token) -> u8 {
        match t {
            Token::Eof => 0,
            Token::Word(_) => 1,
            _ => 99,
        }
    }
}

impl Boxx {
    pub fn touch(&mut self) {
        self.transform = 1.0;
    }
}

pub fn build_doc() -> Document {
    Document::new("seed".into())
}

pub fn write_via_doc(d: &mut Document) {
    d.transform[0] = 9.0;
}

pub fn dispatch(t: &Token) -> u8 {
    match t {
        Token::Eof => 0,
        Token::Word(_) => 1,
        Token::Number(_) => 2,
        Token::Resize { w, h } => {
            let _ = (w, h);
            3
        }
    }
}

pub fn classify(t: &Token) -> u8 {
    match t {
        Token::Eof => 0,
        Token::Word(_) => 1,
        _ => 99,
    }
}

/// Or-pattern + wildcard — exercises catch_all Or-arm + parallel_matches Or-case branches.
pub fn classify_or(t: &Token) -> u8 {
    match t {
        Token::Eof | Token::Word(_) => 1,
        Token::Number(_) => 2,
        _ => 99,
    }
}

pub fn deep_nest(a: bool, b: bool, c: bool) -> u8 {
    if a {
        if b {
            if c {
                1
            } else {
                2
            }
        } else {
            3
        }
    } else {
        4
    }
}

pub fn stringly_action(action: &str) -> u8 {
    if action == "save" {
        return 1;
    }
    if action.eq("load") {
        return 2;
    }
    match action {
        "open" => 3,
        "close" => 4,
        _ => 0,
    }
}

pub fn cast_chain(x: i64) -> u8 {
    let a = x as u32;
    let b = a as u8;
    b
}

pub fn maybe_parse(s: &str) -> Option<i32> {
    s.parse().ok()
}

pub fn safe_get(opt: Option<i32>) -> i32 {
    opt.unwrap_or_default()
}

pub fn swallow_via_match(r: Result<i32, String>) -> i32 {
    match r {
        Ok(v) => v,
        Err(_) => 0,
    }
}

pub fn swallow_via_if_let(r: Result<i32, String>) {
    if let Ok(v) = r {
        let _ = v;
    }
}

pub fn swallow_via_while_let(it: &mut dyn Iterator<Item = Result<u8, ()>>) {
    while let Ok(_v) = it.next().unwrap_or(Ok(0)) {
        break;
    }
}

pub fn swallow_via_map_err(r: Result<i32, String>) -> Result<i32, ()> {
    r.map_err(|_| ())
}

pub fn wrapper() -> Document { Document::new("x".into()) }

pub struct LegacyDoc(i64);
impl From<Document> for LegacyDoc {
    fn from(_d: Document) -> Self {
        LegacyDoc(0)
    }
}
impl From<LegacyDoc> for Document {
    fn from(_d: LegacyDoc) -> Self {
        Document::new("from-legacy".into())
    }
}

macro_rules! log_with_trace {
    ($msg:expr) => {{
        let _t = format_backtrace();
        println!("[trace] {} {}", $msg, _t);
    }};
}

pub fn format_backtrace() -> String {
    String::from("trace-stub")
}

mod inner {
    pub use crate::Document as Renamed;
}

pub fn use_renamed(r: &mut Renamed) {
    r.transform[0] = 2.0;
}

pub fn convert_heavy(d: Document) -> String {
    let s = d.name.to_string();
    let _v: Vec<u8> = s.as_bytes().to_vec();
    let _l: LegacyDoc = d.into();
    s.to_owned()
}

#[allow(dead_code)]
pub fn intentionally_dead() {}

pub fn really_dead() {}

#[cfg(feature = "gpu")]
pub fn gpu_only() {}

#[cfg(not(feature = "gpu"))]
pub fn cpu_only() {}

#[cfg(all(unix, target_os = "macos"))]
pub fn macos_only() {}

#[cfg(any(test, feature = "tracing"))]
pub fn telemetry_helper() {}

#[cfg(any(feature = "gpu", feature = "metal"))]
pub fn any_gfx_backend() {}

#[cfg(all(feature = "gpu", target_os = "macos"))]
pub fn gpu_macos_only() {}

#[cfg(not(feature = "no_color"))]
pub fn with_color() {}

// Triggers more cast classes.
pub fn cast_variety(x: u32, y: f64) -> (i8, f32, *const u8) {
    let a = x as u8 as i8;        // signed-flip after narrow
    let b = y as f32;              // narrow-float
    let p = &x as *const u32 as *const u8;
    (a, b, p)
}

// Matches macro with guard (exercises macro_scan::strip_match_guard).
pub fn matches_guard(t: &Token) -> bool {
    matches!(t, Token::Number(n) if *n > 0)
}

// vec! with `;` form (exercises macro_scan top-level `;` split).
pub fn repeat_zero(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

// String-table lookup (exercises stringly --include-map-keys).
pub fn lookup(m: &std::collections::HashMap<String, u32>) -> Option<u32> {
    m.get("user_count").copied()
}

// String substring check.
pub fn is_api_path(p: &str) -> bool {
    p.starts_with("/api/")
}

fn main() {
    let mut d = build_doc();
    d.touch();
    d.show();
    write_via_doc(&mut d);
    let _ = dispatch(&Token::Eof);
    let _ = classify(&Token::Word("hi".into()));
    let _ = deep_nest(true, false, true);
    let _ = stringly_action("save");
    let _ = cast_chain(42);
    let _ = maybe_parse("1");
    let _ = safe_get(Some(1));
    let _ = wrapper();
    let _ = convert_heavy(d);
    let mut d2 = build_doc();
    use_renamed(&mut d2);
    log_with_trace!("starting");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn it_runs() {
        assert_eq!(safe_get(Some(7)), 7);
    }
}
