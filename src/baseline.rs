//! Comparing one run against another: `gone` / `new` / `moved` / `unchanged`.
//!
//! Two ways to get the other run, neither of which asks the tool to keep hidden
//! state between invocations:
//!
//! * `--since <git-ref>` — extract that ref with `git archive`, run the battery
//!   over it, diff in memory. Git already holds every prior state of the tree,
//!   and the tool already shells out to it for `--changed-since`. Nothing is
//!   written anywhere.
//! * `--write-baseline` / `--baseline <file>` — an explicit snapshot the *user*
//!   owns, for pinning a CI gate against a release rather than a commit.
//!
//! The baseline file is tab-separated rather than JSON on purpose. It is read
//! back by this tool, so a self-describing format buys nothing, and a
//! line-oriented one is greppable, diffable in review, and needs no parser
//! dependency for a tool that currently has four.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::emit::Finding;
use crate::fingerprint;

const HEADER: &str = "# unruster-baseline";

/// What changed between two runs.
pub struct Diff {
    /// In the baseline, absent now — fixed, waived, or deleted.
    pub gone: Vec<Finding>,
    /// Present now, absent from the baseline.
    pub new: Vec<Finding>,
    /// The same finding, in a different place: `(was, now)`. A refactor, not a
    /// fix. Line shifts never reach here — the fingerprint already ignores
    /// them — so this bucket is only ever renames and moves between modules.
    pub moved: Vec<(Finding, Finding)>,
    pub unchanged: usize,
}

impl Diff {
    /// Nothing changed in either direction. The predicate a caller gating on
    /// "identical to the baseline" wants, as distinct from `--fail-on-new`,
    /// which tolerates fixes.
    #[allow(dead_code)] // exercised by the unit tests; kept as the paired API
    pub fn is_clean(&self) -> bool {
        self.new.is_empty() && self.moved.is_empty() && self.gone.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} gone, {} new, {} moved, {} unchanged",
            self.gone.len(),
            self.new.len(),
            self.moved.len(),
            self.unchanged
        )
    }
}

/// Count occurrences per fingerprint. Two textually identical findings inside
/// one function share a fingerprint by design — they are interchangeable, so
/// the honest comparison is "there were 3, now there are 2", not an attempt to
/// decide *which* one went.
fn tally(items: &[Finding]) -> BTreeMap<&str, Vec<&Finding>> {
    let mut m: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
    for f in items {
        m.entry(f.fp.as_str()).or_default().push(f);
    }
    m
}

/// A finding that changed identity only because its enclosing item moved.
///
/// Paired on the check plus the label with every `::` prefix stripped, so a
/// function relocated into a submodule (`c` → `moved::c`) still matches. The
/// label already has spans and measurements normalized away, leaving the
/// finding's own description; dropping the module path is what makes a
/// *relocation* read as one `moved` rather than as a fix plus a regression.
///
/// Two same-named items in different modules with the same finding will pair
/// up. That is the heuristic's price, and `moved` is the honest label for it:
/// this finding shape left there and appeared here.
fn move_key(f: &Finding) -> String {
    let leaf = f
        .label
        .split(" | ")
        .map(|tok| tok.rsplit("::").next().unwrap_or(tok))
        .collect::<Vec<_>>()
        .join(" | ");
    format!("{}\u{1}{}", f.check, leaf)
}

pub fn diff(base: &[Finding], cur: &[Finding]) -> Diff {
    let (bt, ct) = (tally(base), tally(cur));
    let mut gone: Vec<Finding> = Vec::new();
    let mut new: Vec<Finding> = Vec::new();
    let mut unchanged = 0usize;

    for (fp, items) in &bt {
        let now = ct.get(fp).map(Vec::len).unwrap_or(0);
        unchanged += now.min(items.len());
        for f in items.iter().skip(now) {
            gone.push((*f).clone());
        }
    }
    for (fp, items) in &ct {
        let before = bt.get(fp).map(Vec::len).unwrap_or(0);
        for f in items.iter().skip(before) {
            new.push((*f).clone());
        }
    }

    // Re-pair the two buckets: a finding that vanished here and appeared there
    // with the same description is one that moved, and reporting it as
    // "1 fixed, 1 introduced" would be two lies rather than one truth.
    let mut moved: Vec<(Finding, Finding)> = Vec::new();
    let mut by_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, f) in new.iter().enumerate() {
        by_key.entry(move_key(f)).or_default().push(i);
    }
    let mut claimed = vec![false; new.len()];
    let mut kept_gone: Vec<Finding> = Vec::new();
    for g in gone.drain(..) {
        let idx = by_key
            .get_mut(&move_key(&g))
            .and_then(|v| v.iter().position(|&i| !claimed[i]).map(|p| v[p]));
        match idx {
            Some(i) => {
                claimed[i] = true;
                moved.push((g, new[i].clone()));
            }
            None => kept_gone.push(g),
        }
    }
    let new: Vec<Finding> = new
        .into_iter()
        .zip(claimed)
        .filter(|(_, c)| !c)
        .map(|(f, _)| f)
        .collect();

    Diff {
        gone: kept_gone,
        new,
        moved,
        unchanged,
    }
}

// ---------------------------------------------------------------------------
// Baseline file
// ---------------------------------------------------------------------------

pub fn write(path: &Path, findings: &[Finding]) -> Result<()> {
    let mut s = format!("{} v{}\n", HEADER, fingerprint::SCHEME);
    for f in findings {
        // Tabs separate; the label is last so an embedded tab cannot shift a
        // field. Labels are tool-generated, but "cannot" beats "does not".
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            f.fp,
            f.check,
            f.file,
            f.line,
            f.label.replace('\t', " ")
        ));
    }
    std::fs::write(path, s).with_context(|| format!("writing baseline {}", path.display()))
}

pub fn read(path: &Path) -> Result<Vec<Finding>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading baseline {}", path.display()))?;
    let mut lines = src.lines();
    let header = lines.next().unwrap_or_default();
    let Some(v) = header.strip_prefix(&format!("{} v", HEADER)) else {
        bail!(
            "{} is not an unruster baseline (expected a `{} vN` header)",
            path.display(),
            HEADER
        );
    };
    // A scheme change alters every fingerprint. Saying so beats reporting the
    // entire codebase as new findings.
    if v.trim() != fingerprint::SCHEME.to_string() {
        bail!(
            "baseline {} uses fingerprint scheme v{}, this build emits v{} — \
             regenerate it with `--write-baseline`",
            path.display(),
            v.trim(),
            fingerprint::SCHEME
        );
    }
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let c: Vec<&str> = line.splitn(5, '\t').collect();
        if c.len() < 5 {
            continue;
        }
        out.push(Finding {
            fp: c[0].to_string(),
            check: c[1].to_string(),
            file: c[2].to_string(),
            line: c[3].parse().unwrap_or(0),
            label: c[4].to_string(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

/// A `git archive` extraction of `git_ref`, and the path inside it matching
/// `root`. The caller scans that path and drops the guard to clean up.
pub struct Snapshot {
    dir: PathBuf,
    pub scan_root: PathBuf,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Materialize `root` as it existed at `git_ref`.
///
/// Only the scan root is extracted, not the whole tree — on a large repo the
/// difference is seconds. Works against a dirty working tree, because
/// `git archive` reads the object store rather than the checkout.
pub fn snapshot(git_ref: &str, root: &Path) -> Result<Snapshot> {
    use std::process::Command;
    let abs = std::fs::canonicalize(root)
        .with_context(|| format!("resolving scan root {}", root.display()))?;
    // Every git call runs from the scan root, not the process cwd. `unruster
    // -r ../other-repo/src audit --since HEAD` must compare against
    // *that* repo's history, not whichever one the shell happens to be in.
    let git_dir = if abs.is_dir() {
        abs.clone()
    } else {
        abs.parent().unwrap_or(&abs).to_path_buf()
    };
    let top = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&git_dir)
        .output()
        .context("running git rev-parse")?;
    if !top.status.success() {
        bail!("not inside a git repository, so `--since` has nothing to compare against");
    }
    let top = std::fs::canonicalize(String::from_utf8_lossy(&top.stdout).trim())
        .context("resolving the repository root")?;
    let rel = abs.strip_prefix(&top).unwrap_or(&abs).to_path_buf();
    let rel_str = if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.to_string_lossy().into_owned()
    };

    let dir = std::env::temp_dir().join(format!(
        "unruster-baseline-{}-{}",
        std::process::id(),
        git_ref.replace(['/', '~', '^', ':'], "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let archive = Command::new("git")
        .args(["archive", "--format=tar", git_ref, "--", &rel_str])
        .current_dir(&top)
        .output()
        .context("running git archive")?;
    if !archive.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!(
            "git archive {} failed: {}",
            git_ref,
            String::from_utf8_lossy(&archive.stderr).trim()
        );
    }

    let mut tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(&dir)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("running tar")?;
    {
        use std::io::Write;
        let stdin = tar.stdin.as_mut().context("tar stdin")?;
        stdin.write_all(&archive.stdout)?;
    }
    if !tar.wait()?.success() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("extracting the {} snapshot failed", git_ref);
    }

    let scan_root = dir.join(&rel);
    if !scan_root.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!(
            "{} did not exist at {} — nothing to compare against",
            rel_str,
            git_ref
        );
    }
    Ok(Snapshot { dir, scan_root })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(fp: &str, check: &str, label: &str, file: &str, line: usize) -> Finding {
        Finding {
            fp: fp.into(),
            check: check.into(),
            file: file.into(),
            line,
            label: label.into(),
        }
    }

    #[test]
    fn identical_runs_are_all_unchanged() {
        let a = vec![f("aa", "casts", "narrow | x", "a.rs", 1)];
        let d = diff(&a, &a);
        assert_eq!(d.unchanged, 1);
        assert!(d.is_clean());
    }

    #[test]
    fn a_fix_is_gone_and_nothing_else() {
        let base = vec![
            f("aa", "casts", "narrow | x", "a.rs", 1),
            f("bb", "casts", "narrow | y", "a.rs", 9),
        ];
        let cur = vec![f("aa", "casts", "narrow | x", "a.rs", 1)];
        let d = diff(&base, &cur);
        assert_eq!(d.gone.len(), 1);
        assert!(d.new.is_empty() && d.moved.is_empty());
        assert_eq!(d.unchanged, 1);
    }

    #[test]
    fn relocating_into_a_submodule_reads_as_moved() {
        // The label gains a module prefix; the finding did not change.
        let base = vec![f("aa", "dead-code", "fn | pub | c", "a.rs", 3)];
        let cur = vec![f("zz", "dead-code", "fn | pub | moved::c", "a/moved.rs", 1)];
        let d = diff(&base, &cur);
        assert_eq!(d.moved.len(), 1, "expected a move, got {}", d.summary());
        assert!(d.gone.is_empty() && d.new.is_empty());
    }

    #[test]
    fn a_rename_reports_as_moved_not_as_a_fix_plus_a_regression() {
        // Same finding, different enclosing fn: the fingerprints differ, but
        // reporting "1 fixed, 1 introduced" would be two lies rather than one
        // truth.
        let base = vec![f("aa", "casts", "narrow | old::fn", "a.rs", 3)];
        let cur = vec![f("zz", "casts", "narrow | old::fn", "b.rs", 40)];
        let d = diff(&base, &cur);
        assert_eq!(d.moved.len(), 1);
        assert!(d.gone.is_empty() && d.new.is_empty());
        assert_eq!(d.moved[0].0.file, "a.rs");
        assert_eq!(d.moved[0].1.file, "b.rs");
    }

    #[test]
    fn duplicates_are_compared_by_count() {
        // Three identical `let _ = f();` in one fn share a fingerprint. Losing
        // one is one `gone`, not a re-pairing puzzle.
        let base = vec![
            f("aa", "es", "let-_ | m", "a.rs", 1),
            f("aa", "es", "let-_ | m", "a.rs", 2),
            f("aa", "es", "let-_ | m", "a.rs", 3),
        ];
        let cur = vec![
            f("aa", "es", "let-_ | m", "a.rs", 1),
            f("aa", "es", "let-_ | m", "a.rs", 2),
        ];
        let d = diff(&base, &cur);
        assert_eq!(d.gone.len(), 1);
        assert_eq!(d.unchanged, 2);
        assert!(d.new.is_empty());
    }

    #[test]
    fn a_genuinely_new_finding_is_not_absorbed_as_a_move() {
        let base = vec![f("aa", "casts", "narrow | x", "a.rs", 1)];
        let cur = vec![
            f("aa", "casts", "narrow | x", "a.rs", 1),
            f("bb", "casts", "signed-flip | y", "a.rs", 7),
        ];
        let d = diff(&base, &cur);
        assert_eq!(d.new.len(), 1);
        assert!(d.moved.is_empty() && d.gone.is_empty());
    }

    #[test]
    fn baseline_round_trips_through_the_file() {
        // `CARGO_TARGET_TMPDIR` is an integration-test variable; unit tests in a
        // binary crate do not get it.
        let dir = std::env::temp_dir().join("unruster-baseline-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bl.tsv");
        let items = vec![
            f("aa", "casts", "narrow | x", "a.rs", 1),
            f("bb", "error-swallows", "let-_ | m::n", "b.rs", 22),
        ];
        write(&p, &items).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].check, "error-swallows");
        assert_eq!(back[1].line, 22);
        assert_eq!(back[1].label, "let-_ | m::n");
        assert!(diff(&items, &back).is_clean());
    }

    #[test]
    fn a_scheme_mismatch_is_reported_rather_than_read_as_total_churn() {
        // `CARGO_TARGET_TMPDIR` is an integration-test variable; unit tests in a
        // binary crate do not get it.
        let dir = std::env::temp_dir().join("unruster-baseline-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bl-old.tsv");
        std::fs::write(&p, "# unruster-baseline v99\naa\tcasts\ta.rs\t1\tx\n").unwrap();
        let err = read(&p).unwrap_err().to_string();
        assert!(err.contains("scheme v99"), "{err}");
    }

    #[test]
    fn a_foreign_file_is_rejected() {
        // `CARGO_TARGET_TMPDIR` is an integration-test variable; unit tests in a
        // binary crate do not get it.
        let dir = std::env::temp_dir().join("unruster-baseline-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("not-a-baseline.tsv");
        std::fs::write(&p, "hello\n").unwrap();
        assert!(read(&p).unwrap_err().to_string().contains("not an unruster baseline"));
    }
}
