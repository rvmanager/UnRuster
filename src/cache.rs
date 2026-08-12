//! On-disk cache of per-file [`crate::facts`], under `~/.unruster_cache`.
//!
//! # Why this tool grew a cache after refusing one everywhere else
//!
//! `baseline.rs` advertises that neither of its diff modes "asks the tool to
//! keep hidden state between invocations", and `suppress.rs` makes the same
//! boast about configuration. Both are load-bearing: state you cannot see is
//! state that can be wrong about your code, and every other feature here was
//! designed so that a run is a pure function of the tree plus the flags.
//!
//! The pre-write gate breaks that budget and nothing else does. A gate that
//! answers "does this already exist?" runs on the agent's *keystroke path* —
//! once per `Write`, in front of the edit — where a full parse of a large
//! workspace is not a cost anybody absorbs twice. So the cache exists for that
//! one latency budget, and it is built so that it cannot change an answer:
//!
//! * Entries are keyed by the **content hash of the file**, so a hit is the
//!   same bytes that produced it. There is no invalidation rule to get wrong,
//!   because there is nothing to invalidate — an edited file simply has no
//!   entry.
//! * [`crate::facts::SCHEME`] is part of the key, so a change to what facts
//!   contain is a miss rather than a misparse.
//! * A cache that cannot be read, written or created is not an error. Every
//!   operation degrades to "recompute", which is what the tool did before.
//! * `--no-cache` turns it off, and `unruster cache` says what is in it.
//!
//! # Layout
//!
//! ```text
//! ~/.unruster_cache/
//!   <project-slug>/          e.g. `UnRuster-8f31a0c2d4e7`
//!     root                   the canonical scan root, so a slug is explicable
//!     f/<hash>               one file's facts
//! ```
//!
//! The slug carries the directory's own name *and* a hash of its canonical
//! path: the name alone collides across checkouts of the same project (the case
//! that matters — a worktree and its parent hold different code under one
//! name), and the hash alone produces a cache directory nobody can identify
//! when they go looking.
//!
//! # Concurrency and growth
//!
//! Writes go to a temporary file and are renamed into place, so two runs racing
//! on one entry leave a complete file either way. Entries for edited-away
//! content are never referenced again; [`Cache::sweep`] bounds the directory by
//! deleting the oldest once the count runs well past what the tree needs.

use std::path::{Path, PathBuf};

use crate::fingerprint::{fnv1a, FNV_OFFSET, FNV_PRIME};

/// A second, differently-seeded pass, concatenated with the first into a
/// 128-bit key.
///
/// One 64-bit FNV over a whole source file is not a hash anyone should key a
/// correctness-relevant cache on: at a few thousand files the birthday bound is
/// comfortable, but a collision here does not degrade a score — it hands back
/// *another file's* items and bodies, and the resulting rows name code that
/// does not exist. The second pass costs one more linear scan of bytes already
/// in cache and removes the question.
fn hash128(bytes: &[u8]) -> String {
    let a = fnv1a(bytes);
    let mut h = FNV_OFFSET ^ 0x9e37_79b9_7f4a_7c15;
    for b in bytes.iter().rev() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}{:016x}", a, h)
}

/// The cache root: `$UNRUSTER_CACHE_DIR`, else `~/.unruster_cache`.
///
/// The environment override exists for the test suite and for CI, where a
/// per-user home directory is the wrong place to put build state.
pub fn cache_root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("UNRUSTER_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).or_else(|| {
        // Windows keeps it in two halves; the tool is unix-first but there is
        // no reason for this one function to be.
        let drive = std::env::var_os("HOMEDRIVE")?;
        let path = std::env::var_os("HOMEPATH")?;
        Some(PathBuf::from(drive).join(path))
    })?;
    Some(home.join(".unruster_cache"))
}

/// Directory name for one scan root: `<dir-name>-<hash of canonical path>`.
// unruster: ok(error-swallows/.unwrap_or_else) 2026-08-12 — a root that will not
// canonicalize (it does not exist yet, or is a broken link) still deserves a
// stable cache directory, and the path as written is the best name available.
// The fallback cannot collide with a real one: two spellings of one directory
// would both canonicalize, and only an uncanonicalizable path reaches here.
pub fn slug_for(root: &Path) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // A file root (`unruster show -r src/main.rs …`) shares its parent's cache;
    // the entries are per file anyway, so nothing is mixed by doing so.
    let dir = if canon.is_file() {
        canon.parent().unwrap_or(&canon).to_path_buf()
    } else {
        canon.clone()
    };
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    // Keep the readable half short and free of separators so the directory
    // listing stays one line per project.
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    format!("{}-{:016x}", safe, fnv1a(dir.to_string_lossy().as_bytes()))
}

/// Entries past which [`Cache::sweep`] starts deleting the oldest. Expressed as
/// a multiple of the files a run actually touched, so a small project keeps a
/// small cache and a large one is allowed a large one. Four generations of
/// every file is enough to make branch-switching cheap without unbounded growth.
const KEEP_FACTOR: usize = 4;

/// A handle on one project's cache directory. Every method is infallible from
/// the caller's side: an unusable cache behaves as a permanently empty one.
///
/// Hit and miss counts live on [`crate::corpus::Corpus`] rather than here: the
/// corpus is what a summary line reports, and a second tally kept beside this
/// one would be two implementations of one fact — the shape this tool reports
/// about other codebases.
pub struct Cache {
    dir: PathBuf,
    /// Set when the directory could not be created. Reads miss, writes no-op.
    dead: bool,
}

// unruster: ok(silent-fallbacks) 2026-08-12 — every discarded Result in this
// impl is the module's stated contract: "A cache that cannot be read, written
// or created is not an error. Every operation degrades to recompute, which is
// what the tool did before." An entry that fails to write is a future miss; an
// entry that fails to delete is swept next time; a metadata read that fails
// omits one row from a size report. Propagating any of them would let a broken
// cache fail an analysis it exists only to speed up.
impl Cache {
    /// Open (creating if needed) the cache for `root`. `None` when caching is
    /// switched off or no home directory can be found — the caller then simply
    /// computes everything, exactly as before this module existed.
    pub fn open(root: &Path) -> Option<Cache> {
        let dir = cache_root()?.join(slug_for(root));
        let facts = dir.join("f");
        let dead = std::fs::create_dir_all(&facts).is_err();
        if !dead {
            // A one-line note of what this slug stands for. Best-effort: a
            // cache that works but cannot explain itself is still a cache.
            let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let _ = std::fs::write(dir.join("root"), format!("{}\n", canon.display()));
        }
        Some(Cache { dir, dead })
    }

    fn entry(&self, key: &str) -> PathBuf {
        self.dir.join("f").join(key)
    }

    /// Cache key for one file's bytes: the facts scheme plus the content hash.
    /// Two files with identical bytes share an entry, which is why
    /// [`crate::facts::restamp`] exists.
    pub fn key(content: &[u8]) -> String {
        format!("{}-{}", crate::facts::SCHEME, hash128(content))
    }

    /// The facts for these bytes, if this exact content has been analysed
    /// before. The `file` is stamped onto the result so rows name the path the
    /// caller asked about rather than whichever path first wrote the entry.
    pub fn get(&self, key: &str, file: &str) -> Option<crate::facts::FileFacts> {
        if self.dead {
            return None;
        }
        let text = std::fs::read_to_string(self.entry(key)).ok();
        // A miss *and* an unreadable entry answer the same way on purpose: a
        // record this build cannot parse is not evidence about the code.
        let mut f = text.as_deref().and_then(crate::facts::decode)?;
        crate::facts::restamp(&mut f, file);
        Some(f)
    }

    /// Store facts under `key`. Temp-file-and-rename so a reader never sees a
    /// half-written entry, and silent on failure — a cache write that fails
    /// must not fail the analysis it was accelerating.
    pub fn put(&self, key: &str, f: &crate::facts::FileFacts) {
        if self.dead {
            return;
        }
        let dst = self.entry(key);
        // The pid keeps two concurrent runs from writing one temp file; the
        // rename then makes whichever finishes last the winner, and both wrote
        // the same bytes.
        let tmp = dst.with_extension(format!("tmp{}", std::process::id()));
        // One cleanup path rather than two. The `else` arm used to be the only
        // one that removed the temp file, so a *failed rename* — a full disk, a
        // read-only directory — left it behind on every run, and `sweep` counts
        // entries rather than recognising them.
        let wrote = std::fs::write(&tmp, crate::facts::encode(f)).is_ok()
            && std::fs::rename(&tmp, &dst).is_ok();
        if !wrote {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Delete the oldest entries once the directory holds more than
    /// `KEEP_FACTOR × live` of them. Called after a full corpus pass, where
    /// `live` is how many files that pass actually needed.
    pub fn sweep(&self, live: usize) {
        if self.dead || live == 0 {
            return;
        }
        let cap = live.saturating_mul(KEEP_FACTOR);
        let Ok(rd) = std::fs::read_dir(self.dir.join("f")) else {
            return;
        };
        let mut entries: Vec<(std::time::SystemTime, PathBuf)> = rd
            .filter_map(|e| {
                let e = e.ok()?;
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, e.path()))
            })
            .collect();
        if entries.len() <= cap {
            return;
        }
        entries.sort_by_key(|(t, _)| *t);
        for (_, p) in entries.iter().take(entries.len() - cap) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Remove this project's cache directory entirely.
    pub fn clear(&self) -> std::io::Result<()> {
        std::fs::remove_dir_all(&self.dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every other project cache under the same root: `(slug, entries)`,
    /// biggest first.
    ///
    /// Reported when *this* root's cache is empty, because the alternative is a
    /// trap somebody actually fell into: a session ran every check with
    /// `-r vectorian/src` and then `unruster cache` with no `-r`, read "0
    /// cached file(s)" for a slug it had never written, and concluded the cache
    /// was dead — in the same session where `gate` reported 289 files served
    /// from it. Entries are per `--root`, and a zero that means "you asked
    /// about a different directory" has to say so.
    pub fn siblings(&self) -> Vec<(String, usize)> {
        let Some(parent) = self.dir.parent() else {
            return Vec::new();
        };
        let Ok(rd) = std::fs::read_dir(parent) else {
            return Vec::new();
        };
        let mut out: Vec<(String, usize)> = rd
            .flatten()
            .filter(|e| e.path() != self.dir && e.path().is_dir())
            .map(|e| {
                let n = std::fs::read_dir(e.path().join("f"))
                    .map(|d| d.count())
                    .unwrap_or(0);
                (e.file_name().to_string_lossy().into_owned(), n)
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// `(entries, bytes)` currently stored.
    pub fn size(&self) -> (usize, u64) {
        let Ok(rd) = std::fs::read_dir(self.dir.join("f")) else {
            return (0, 0);
        };
        let mut n = 0;
        let mut b = 0;
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                n += 1;
                b += m.len();
            }
        }
        (n, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_changes_with_the_content_and_with_the_scheme() {
        assert_ne!(Cache::key(b"fn a() {}"), Cache::key(b"fn b() {}"));
        assert_eq!(Cache::key(b"fn a() {}"), Cache::key(b"fn a() {}"));
        assert!(Cache::key(b"x").starts_with(&format!("{}-", crate::facts::SCHEME)));
    }

    /// Two checkouts of one project must not share a cache directory: the
    /// names are equal and the code is not.
    #[test]
    fn the_slug_separates_two_directories_with_one_name() {
        let a = slug_for(Path::new("/tmp/a/UnRuster"));
        let b = slug_for(Path::new("/tmp/b/UnRuster"));
        assert_ne!(a, b);
        assert!(a.starts_with("UnRuster-"), "{a} should stay identifiable");
    }

    #[test]
    fn a_slug_never_contains_a_path_separator() {
        let s = slug_for(Path::new("/tmp/some dir/with.dots"));
        assert!(!s.contains('/') && !s.contains('\\'), "{s}");
    }

    #[test]
    fn round_trips_through_a_temporary_cache_directory() {
        let tmp = std::env::temp_dir().join(format!("unruster-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY-of-intent: the whole test process is single-threaded here and
        // the var is read only by `cache_root`.
        std::env::set_var("UNRUSTER_CACHE_DIR", &tmp);
        let c = Cache::open(Path::new(".")).expect("opens");

        let ast = syn::parse_file("pub struct Id(u64);").unwrap();
        let pf = crate::parse::ParsedFile {
            path: std::path::PathBuf::from("src/x.rs"),
            ast,
            module: "x".into(),
        };
        let f = crate::facts::derive(&pf);
        let key = Cache::key(b"pub struct Id(u64);");
        assert!(c.get(&key, "src/x.rs").is_none(), "cold cache must miss");
        c.put(&key, &f);
        let got = c.get(&key, "src/other.rs").expect("warm cache hits");
        assert_eq!(got.items.len(), 1);
        // Restamped to the path the caller asked about, not the one that wrote
        // the entry.
        assert_eq!(got.items[0].file, "src/other.rs");

        c.clear().expect("clears");
        std::env::remove_var("UNRUSTER_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
