use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use syn::parse::Parser;
use syn::punctuated::Punctuated;

use crate::ast::path_to_string;

/// Boolean lattice for cfg evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum Cfg {
    Ident(String),
    KeyVal(String, String),
    All(Vec<Cfg>),
    Any(Vec<Cfg>),
    Not(Box<Cfg>),
}

/// Set of `--cfg` flags. Boolean keys live in `bools`; key=value in `kvs`.
/// A key is "known" if it has any entry in either set (so unknown keys give Unknown).
#[derive(Debug, Default, Clone)]
pub struct CfgEnv {
    pub bools: BTreeSet<String>,
    pub kvs: BTreeMap<String, BTreeSet<String>>,
    /// Keys we always treat as known even when no `--cfg` provided
    /// (so `cfg(test)` is definitively False in production, not Unknown).
    pub known_keys: BTreeSet<String>,
}

impl CfgEnv {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.known_keys.insert("test".to_string());
        s
    }

    pub fn add(&mut self, raw: &str) {
        if let Some(eq) = raw.find('=') {
            let key = raw[..eq].trim().to_string();
            let mut val = raw[eq + 1..].trim().to_string();
            if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                val = val[1..val.len() - 1].to_string();
            }
            self.kvs.entry(key.clone()).or_default().insert(val);
            self.known_keys.insert(key);
        } else {
            let key = raw.trim().to_string();
            self.bools.insert(key.clone());
            self.known_keys.insert(key);
        }
    }

    fn is_set_bool(&self, name: &str) -> bool {
        self.bools.contains(name)
    }

    fn is_known(&self, name: &str) -> bool {
        self.known_keys.contains(name) || self.bools.contains(name) || self.kvs.contains_key(name)
    }

    pub fn eval(&self, c: &Cfg) -> Tri {
        match c {
            Cfg::Ident(n) => {
                if self.is_set_bool(n) {
                    Tri::True
                } else if self.is_known(n) {
                    Tri::False
                } else {
                    Tri::Unknown
                }
            }
            Cfg::KeyVal(k, v) => {
                if let Some(set) = self.kvs.get(k) {
                    if set.contains(v) {
                        Tri::True
                    } else {
                        Tri::False
                    }
                } else if self.is_known(k) {
                    Tri::False
                } else {
                    Tri::Unknown
                }
            }
            Cfg::All(cs) => {
                let mut any_unknown = false;
                for c in cs {
                    match self.eval(c) {
                        Tri::False => return Tri::False,
                        Tri::Unknown => any_unknown = true,
                        Tri::True => {}
                    }
                }
                if any_unknown {
                    Tri::Unknown
                } else {
                    Tri::True
                }
            }
            Cfg::Any(cs) => {
                let mut any_unknown = false;
                for c in cs {
                    match self.eval(c) {
                        Tri::True => return Tri::True,
                        Tri::Unknown => any_unknown = true,
                        Tri::False => {}
                    }
                }
                if any_unknown {
                    Tri::Unknown
                } else {
                    Tri::False
                }
            }
            Cfg::Not(c) => match self.eval(c) {
                Tri::True => Tri::False,
                Tri::False => Tri::True,
                Tri::Unknown => Tri::Unknown,
            },
        }
    }

    /// Should we strip an item whose cfg-set has these expressions?
    /// Strip if any cfg attribute evaluates to definitively False.
    pub fn strip(&self, cfgs: &[Cfg]) -> bool {
        cfgs.iter().any(|c| self.eval(c) == Tri::False)
    }
}

pub fn parse_cfg_attr(attr: &syn::Attribute) -> Option<Cfg> {
    if !attr.path().is_ident("cfg") {
        return None;
    }
    let syn::Meta::List(ml) = &attr.meta else {
        return None;
    };
    let inner: syn::Meta = syn::parse2(ml.tokens.clone()).ok()?;
    meta_to_cfg(&inner)
}

fn meta_to_cfg(m: &syn::Meta) -> Option<Cfg> {
    match m {
        syn::Meta::Path(p) => Some(Cfg::Ident(path_to_string(p))),
        syn::Meta::NameValue(nv) => {
            if let syn::Expr::Lit(l) = &nv.value {
                if let syn::Lit::Str(s) = &l.lit {
                    return Some(Cfg::KeyVal(path_to_string(&nv.path), s.value()));
                }
            }
            None
        }
        syn::Meta::List(ml) => {
            let kind = path_to_string(&ml.path);
            let inner: Vec<syn::Meta> = parse_meta_list(&ml.tokens)?;
            let cfgs: Vec<Cfg> = inner.iter().filter_map(meta_to_cfg).collect();
            match kind.as_str() {
                "all" => Some(Cfg::All(cfgs)),
                "any" => Some(Cfg::Any(cfgs)),
                "not" => cfgs.into_iter().next().map(|c| Cfg::Not(Box::new(c))),
                _ => None,
            }
        }
    }
}

fn parse_meta_list(ts: &TokenStream) -> Option<Vec<syn::Meta>> {
    let parser = Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated;
    parser
        .parse2(ts.clone())
        .ok()
        .map(|p| p.into_iter().collect())
}
