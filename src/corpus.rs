//! The tree's [`crate::facts`], gathered — with or without a parse.
//!
//! Two ways in, for the two shapes of question this tool now answers:
//!
//! * [`Corpus::from_files`] — the **corpus** question. The caller is already
//!   holding a parsed tree because a check is about to walk it, so facts are
//!   derived from those ASTs. The cache is *written* here, which is what makes
//!   an ordinary `unruster audit` leave the gate warm.
//! * [`Corpus::load`] — the **candidate** question. Nothing is parsed unless it
//!   has to be: each file is read, hashed, and looked up, so an unedited tree
//!   costs one read and one hash per file instead of a `syn` parse. This is the
//!   path [`crate::gate`] runs on, once per proposed write.
//!
//! Both produce the same type, so a check cannot behave differently depending
//! on how its inputs arrived.

use std::path::Path;

use crate::cache::Cache;
use crate::facts::{BodyFact, FileFacts, ItemFact};

#[derive(Default)]
pub struct Corpus {
    pub items: Vec<ItemFact>,
    pub bodies: Vec<BodyFact>,
    /// Files whose facts came from the cache, and files that had to be derived.
    pub hits: usize,
    pub misses: usize,
}

impl Corpus {
    fn absorb(&mut self, f: FileFacts) {
        self.items.extend(f.items);
        self.bodies.extend(f.bodies);
    }

    /// Derive facts from an already-parsed tree, writing each file's result to
    /// the cache for the benefit of later gate runs.
    ///
    /// The cache is never *read* here: the ASTs are in hand, deriving from them
    /// is cheaper than reading a file back and parsing its records, and a read
    /// would introduce a way for a check to disagree with the tree it was
    /// handed.
    pub fn from_files(files: &[crate::parse::ParsedFile], cache: Option<&Cache>) -> Corpus {
        let mut c = Corpus::default();
        for pf in files {
            let f = crate::facts::derive(pf);
            if let Some(cache) = cache {
                // Hashing the source costs one read of a file the walker has
                // already read once. Worth it only because the alternative —
                // keying on mtime — makes a restored or touched file look
                // edited and a `git checkout` of identical content look new.
                // unruster: ok(error-swallows/if-let-ok) 2026-08-12 — a file the
                // walker parsed a moment ago but cannot be re-read is mid-edit
                // or gone. Its facts are still correct and still returned; only
                // the cache write is skipped, which is a future miss.
                if let Ok(bytes) = std::fs::read(&pf.path) {
                    cache.put(&Cache::key(&bytes), &f);
                }
            }
            c.absorb(f);
        }
        c.misses = files.len();
        c
    }

    /// Gather facts for every `.rs` file under `root`, parsing only the files
    /// whose exact contents the cache has not seen.
    ///
    /// A file that cannot be read or parsed is skipped rather than fatal: the
    /// question being asked ("what already exists?") is answerable from the
    /// rest of the tree, and refusing to answer at all because one file is
    /// mid-edit is the wrong trade for a check that runs in front of an edit.
    pub fn load(
        root: &Path,
        excludes: &[String],
        cache: Option<&Cache>,
    ) -> anyhow::Result<Corpus> {
        let paths = crate::parse::walk_rs_files(root, excludes)?;
        let mut c = Corpus::default();
        for p in &paths {
            let display = crate::parse::display_path(p);
            let Ok(bytes) = std::fs::read(p) else { continue };
            let key = Cache::key(&bytes);
            if let Some(f) = cache.and_then(|c| c.get(&key, &display)) {
                c.hits += 1;
                c.absorb(f);
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let Ok(ast) = syn::parse_file(text) else {
                continue;
            };
            let pf = crate::parse::ParsedFile {
                path: p.clone(),
                ast,
                module: crate::parse::module_of(root, p),
            };
            let f = crate::facts::derive(&pf);
            if let Some(cache) = cache {
                cache.put(&key, &f);
            }
            c.misses += 1;
            c.absorb(f);
        }
        if let Some(cache) = cache {
            cache.sweep(paths.len());
        }
        Ok(c)
    }

    /// Drop everything declared in `file`.
    ///
    /// The gate compares a proposal against *the rest of the tree*. Without
    /// this, gating a file that already exists reports every item in it as
    /// colliding with itself — and worse, an `Edit` whose replacement text
    /// shifts line numbers slips past the identity check in
    /// [`crate::gate`] and produces a confident, entirely wrong "this name is
    /// already taken".
    pub fn excluding(&mut self, file: &str) {
        self.items.retain(|i| i.file != file);
        self.bodies.retain(|b| b.file != file);
    }

    /// Every item that *declares a name others could collide with* — the whole
    /// corpus minus the items declared inside a fn body.
    ///
    /// This is the accessor every "does X already exist?" question must use.
    /// A `struct Finding` inside a fn body is invisible outside it, and
    /// [`crate::index`] does not index one at all — so reporting a collision
    /// with it produces an answer the reader cannot verify with `show`, which
    /// this tool treats as worse than no answer.
    pub fn declarations(&self) -> impl Iterator<Item = &ItemFact> {
        self.items.iter().filter(|i| !i.local)
    }

    /// Declarations of one kind, e.g. `"struct"`.
    ///
    /// The query's lifetime is deliberately separate from the corpus's: tying
    /// them together makes every borrow of a *result* also a borrow of the
    /// string that asked for it, which is how a caller ends up unable to keep
    /// the answer past the question.
    pub fn of_kind<'a, 'k>(&'a self, kind: &'k str) -> impl Iterator<Item = &'a ItemFact> + 'k
    where
        'a: 'k,
    {
        self.declarations().filter(move |i| i.kind == kind)
    }

    /// Every declaration of `name`, by bare name.
    pub fn named<'a, 'k>(&'a self, name: &'k str) -> impl Iterator<Item = &'a ItemFact> + 'k
    where
        'a: 'k,
    {
        self.declarations().filter(move |i| i.name == name)
    }

    /// `(hits, misses)` rendered for a summary line, or empty when nothing was
    /// cached — a cache that did nothing should not advertise itself.
    pub fn cache_note(&self) -> String {
        if self.hits == 0 {
            String::new()
        } else {
            format!("; {} file(s) from cache, {} parsed", self.hits, self.misses)
        }
    }
}
