use crate::emit::Out;
use crate::index::NameIndex;
use crate::parse::ParsedFile;
use crate::semantic::Semantic;

/// Length past which a suggested waiver line is likely to be reflowed.
///
/// `rustfmt`'s default `max_width` is 100 and its `wrap_comments` follows it.
/// The line the tool prints is indented two spaces on purpose (it is meant to
/// be pasted above an item), so the number that matters is the same 100.
const WRAPPABLE_WAIVER_LEN: usize = 100;

/// The shared, read-only inputs every analysis command works from: the parsed
/// production files, the name index, semantic info (use-maps, fn signatures,
/// type aliases), and the global `--summary` flag. Built once in `main` and
/// passed by reference to each `run`, replacing the `(files, idx, sem, …,
/// summary)` tuple that was threaded through every command signature.
///
/// All fields are cheap to copy out (`&T` / `bool`), so a command that needs
/// only a subset binds what it uses at the top, e.g. `let files = ctx.files;`.
pub struct AnalysisCtx<'a> {
    pub files: &'a [ParsedFile],
    pub idx: &'a NameIndex,
    pub sem: &'a Semantic,
    /// Flattened per-item and per-body facts — shapes, signatures, docs, body
    /// skeletons. Empty for the commands that do not ask for it (see
    /// `CmdTraits::needs_corpus` in `main`), because deriving it costs a pass
    /// over the whole tree and `show` has no use for one.
    pub corpus: &'a crate::corpus::Corpus,
    pub summary: bool,
    /// Render enclosing-fn labels as `name@start-end` (the `--spans` flag).
    pub spans: bool,
    /// With `--changed-since <ref>`: the files changed vs that git ref, and
    /// the line ranges within them. Site-listing commands drop rows outside
    /// the file set, so an agent can verify exactly its own edit; the ranges
    /// say whether a kept row is in the edit or merely near it. `None` = no
    /// filter.
    pub changed: Option<Changed>,
    /// Where rows, section headers, and summary lines go. Every command emits
    /// through this so `--json` needs no per-command support.
    pub out: &'a Out,
    /// Sites waived by an in-source `// unruster: ok(…)` comment. Borrowed
    /// rather than owned so `waivers` can re-run the check battery against the
    /// same set and then read back each waiver's hit count.
    pub suppressions: &'a crate::suppress::Suppressions,
    /// With `--suggest-waivers`, print the exact waiver comment under each row.
    /// `audit` turns this on for itself so its gating digest can carry the
    /// lines; `suggest_waivers_named` says whether the reader asked.
    pub suggest_waivers: bool,
    /// `--suggest-waivers` was actually on the command line.
    pub suggest_waivers_named: bool,
}

/// A check's findings, split by whether they clear that check's gating
/// threshold.
///
/// Only the ranked checks distinguish the two. Everything else reports the same
/// number twice (all-gating) or reports zero gating (all-advisory), because for
/// an unranked check "which rows matter" is not a question the tool can answer
/// — which is exactly why the ranked ones were built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Rows reported, after waivers and filters.
    pub total: usize,
    /// Of those, the ones above the check's gating threshold.
    pub gating: usize,
}

impl Counts {
    /// A check with no tiers: every row counts the same.
    pub fn flat(total: usize) -> Self {
        Counts {
            total,
            gating: total,
        }
    }
}

impl AnalysisCtx<'_> {
    /// `min_score=0.05 (audit gates at 0.40)` — the check's own threshold, plus
    /// the one `audit` uses when they differ.
    ///
    /// The dedicated commands deliberately run looser than the battery so they
    /// can show the long tail, but the gap was invisible and uneven: 1.8x on
    /// `divergence`, 2.4x on `config-drift` and 8x on `builder-drift`. A reader
    /// comparing a command's output against an audit section had no way to know
    /// the two were asking different questions, and the 8x one looked like a
    /// bug in the audit rather than a wider net here.
    ///
    /// Prints nothing when the two agree — which is the case when `audit`
    /// itself is the caller, so its own sections stay uncluttered.
    pub fn threshold_note(&self, used: f64, audit_gate: f64) -> String {
        if (used - audit_gate).abs() < f64::EPSILON {
            String::new()
        } else {
            format!(" (audit gates at {:.2})", audit_gate)
        }
    }

    /// The `at` cell for a row that names a whole *item* rather than a point in
    /// one. Plain `file:line` by default; `file:start-end` under `--spans`.
    ///
    /// The column count does not change, which is the reason this is an upgrade
    /// of the existing cell rather than a new one — every caller's `awk` and
    /// every column-shape assertion keeps working, and a JSON consumer written
    /// against `line` still reads the start.
    ///
    /// Site-listing commands do not use this: their `--spans` support comes
    /// from [`crate::ast::ScopeTracker`], which appends `@start-end` to the
    /// *enclosing fn*, since the thing worth reading around a call site is the
    /// fn that contains it.
    /// `line` stays the item's declaration line under both renderings. A flag
    /// that only says where a row *ends* must not also move where it starts.
    ///
    /// A one-line item renders as `file:7-7`, not `file:7`: under one flag every
    /// row has one shape, or a consumer has to handle both and the ones that
    /// forget break on whichever item happens to be a one-liner.
    pub fn at(&self, file: &str, line: usize, end: usize) -> crate::emit::Val {
        if self.spans {
            crate::emit::span_site(file, line, end.max(line))
        } else {
            crate::emit::site(file, line)
        }
    }

    /// With `--context N`, print the ±N source lines around `line` beneath a
    /// finding row (`>` marks the site line). No-op otherwise. Rows emitted
    /// through `out.row(…)` already carry their own context — this is only for
    /// grouped listings that render their site lines by hand.
    pub fn print_context(&self, file: &str, line: usize) {
        self.out.context_at(file, line);
    }

    /// Drop waived sites from `items`, returning how many were dropped so the
    /// summary line can say so — a silent drop would read as "clean".
    ///
    /// `check` is the waiver check name (`"casts"`, `"divergence"`, …): an
    /// `ok(casts)` waiver must not silence an error-swallow that happens to
    /// share the line. `site_of` supplies the optional check-specific key.
    pub fn retain_unsuppressed<T>(
        &self,
        check: &str,
        items: &mut Vec<T>,
        site_of: impl Fn(&T) -> crate::suppress::Site<'_>,
    ) -> usize {
        self.retain_unsuppressed_tiered(check, items, site_of, |_| true)
    }

    /// As [`Self::retain_unsuppressed`], plus the one fact the waiver ledger
    /// needs and could not have: whether this finding would have reached
    /// `audit`'s gating tier.
    ///
    /// Every check calls the retain *before* its own class filter and score
    /// gate — it has to, because a suppressed row must not be counted at all —
    /// so without this the `hits` column counted rows the gating battery had
    /// already discarded, and reported a waiver as load-bearing that was not.
    /// See [`crate::suppress::Suppressions::matches_tiered`].
    pub fn retain_unsuppressed_tiered<T>(
        &self,
        check: &str,
        items: &mut Vec<T>,
        site_of: impl Fn(&T) -> crate::suppress::Site<'_>,
        gating_of: impl Fn(&T) -> bool,
    ) -> usize {
        if self.suppressions.is_empty() {
            return 0;
        }
        let before = items.len();
        items.retain(|it| {
            !self
                .suppressions
                .matches_tiered(check, site_of(it), gating_of(it))
        });
        before - items.len()
    }

    /// As [`Self::retain_unsuppressed_tiered`], for a finding that *is* several
    /// sites: a waiver at any one of them retires it.
    ///
    /// The one-site form silently misfiles these. A `concepts` cluster, a
    /// `clones` group, a `near-clones` or `conversion-pairs` pair — the finding
    /// is the relationship between the members, and no member is more the
    /// finding than another. Keying on one of them anyway (whichever sorted
    /// first) meant a waiver written above either of two cognate `px` methods
    /// worked or did nothing depending on a sort order the reader never saw. It
    /// parsed, it read correctly, the row came back, and the ledger showed it
    /// earning zero — so the reader's next guess was that the key was wrong.
    ///
    /// Every candidate site is probed, with no short-circuit on the first
    /// match, so two members waived independently each get credit in `hits`.
    /// Stopping early would leave the second looking dead and invite someone to
    /// delete a waiver that is doing exactly what it says.
    pub fn retain_unsuppressed_multi<T>(
        &self,
        check: &str,
        items: &mut Vec<T>,
        sites_of: impl Fn(&T) -> Vec<crate::suppress::Site<'_>>,
        gating_of: impl Fn(&T) -> bool,
    ) -> usize {
        if self.suppressions.is_empty() {
            return 0;
        }
        let before = items.len();
        items.retain(|it| {
            let gating = gating_of(it);
            let mut waived = false;
            for site in sites_of(it) {
                if self.suppressions.matches_tiered(check, site, gating) {
                    waived = true;
                }
            }
            !waived
        });
        before - items.len()
    }

    /// `; N waived` for a summary line, or empty when nothing was waived. Every
    /// check that filters appends this — a suppression that hides its own
    /// volume reads as a clean codebase.
    pub fn waived_note(&self, n: usize) -> String {
        if n == 0 {
            String::new()
        } else {
            format!("; {} waived", n)
        }
    }

    /// With `--suggest-waivers`, print the exact comment that would retire the
    /// row just emitted — correct check, correct key, today's date filled in —
    /// and the `file:line` it has to be attached to. This is the only place the
    /// waiver grammar is spelled out at the point of use, so nobody has to go
    /// find it in the help.
    ///
    /// `at` is not optional and not decoration. Several checks match a waiver
    /// against one *designated* site of a multi-site finding — `concepts` keys
    /// on the cluster's lead member, `near-clones` on the first of the pair —
    /// so a waiver written above any of the other members parses fine, reads
    /// fine, and suppresses nothing. That is a silent failure: the finding
    /// comes back, the ledger shows a waiver earning zero, and the reader
    /// concludes the key was wrong. One session lost three edits to it,
    /// grepping the suggestions out of a run that had no way to say which of a
    /// cluster's two `px` methods was the one that counted. Taking the anchor
    /// by value means a check cannot add a suggestion without answering the
    /// question.
    pub fn suggest(
        &self,
        check: &str,
        key: Option<&str>,
        today: crate::suppress::Date,
        at: (&str, usize),
    ) {
        if !self.suggest_waivers {
            return;
        }
        let spec = match key {
            Some(k) => format!("{}/{}", check, k),
            None => check.to_string(),
        };
        let line = format!("  // unruster: ok({}) {} — WHY?", spec, today);
        // Anchor first, comment second, two lines. Appending the location to
        // the comment instead would have kept it to one line and broken it: the
        // whole thing is a `//` comment, so a trailing `[at foo.rs:30]` lands
        // inside the reason field and the parser reads it back as prose. The
        // pasteable line has to stay exactly, and only, what gets pasted.
        self.out
            .hint(&format!("  ↳ attach above {}:{} —", at.0, at.1));
        self.out.hint(&line);
        // Structured, alongside the prose, so `--json` output is a `jq` away
        // from `waivers --apply`. Without it the suggestion carried no
        // location: a reader building a batch had to re-derive which row each
        // comment belonged to, and that pipeline was hand-written four separate
        // times in one session.
        self.out.tag_last_row(&[
            ("waiver_check", check.to_string()),
            ("waiver_key", key.unwrap_or_default().to_string()),
            ("waiver_file", at.0.to_string()),
            ("waiver_line", at.1.to_string()),
        ]);
        // A `near-clones` key concatenates two function names, and the line it
        // produces can outrun a formatter's width. The parser now reads a date
        // off the first continuation line, so a wrap no longer loses it — but a
        // line nobody can read without scrolling is still worth one word of
        // warning, and item scope is shorter than the alternative.
        if line.len() > WRAPPABLE_WAIVER_LEN {
            self.out.note(&format!(
                "(note: that waiver line is {} chars and will likely be wrapped by a \
                 formatter. Keep the date on the line after the `ok(...)` — the parser \
                 reads it there — or place the waiver above the item instead of \
                 trailing the site, which shortens the key.)",
                line.len()
            ));
        }
    }

    /// The target did not resolve: say which of the two reasons, and return the
    /// error that exits 2.
    ///
    /// The two are not the same question and the old single message answered
    /// only one of them. `unruster variants Defn` — a struct handed to an
    /// enum-only command — reported "no enum `Defn` found in the scanned tree",
    /// which is false: `Defn` is right there, as a struct. A reader who
    /// believes it goes looking for a typo, or for a `--scope` problem, and
    /// finds neither. Naming the kinds that *do* exist answers the question
    /// they actually have, and near-name suggestions cover the real typo case.
    ///
    /// Every kind-requiring command returns this, so the exit code is the same
    /// across all of them: an unanswerable query is 2, not a clean 0. A command
    /// where any name could plausibly match (`callers`, `type-refs`) must NOT
    /// use this for a zero-hit result — there, zero is a real answer.
    ///
    /// `main` adds the line that says what the 2 means, for every path that
    /// ends in a [`TargetNotFound`] rather than only the ones through here.
    pub fn unknown_target(&self, what: &str, name: &str) -> anyhow::Error {
        self.say_unknown(what, name);
        TargetNotFound::err_owned(what, name)
    }

    /// The same explanation, without ending the run.
    ///
    /// Some commands warn and keep scanning on purpose: a name absent from the
    /// index can still be reached through a macro or an external crate, so the
    /// scan may yet find hits, and only a zero-hit result is an error. They get
    /// the near-name list too — a typo is the likeliest reason the index has
    /// never heard of the name, and finding that out at the end of the scan
    /// rather than the start is what made `callers` a dead end while `show`
    /// answered the same question.
    pub fn warn_unknown(&self, what: &str, name: &str) {
        self.say_unknown(what, name);
    }

    /// Which of the three things went wrong, said once — on stdout, because
    /// this is the answer rather than a remark about it (see [`Out::answer`]).
    fn say_unknown(&self, what: &str, name: &str) {
        let existing = self.idx.lookup(name);
        if !existing.is_empty() {
            let mut kinds: Vec<&str> = existing.iter().map(|d| d.kind).collect();
            kinds.sort_unstable();
            kinds.dedup();
            // The item is already the kind that was asked for, so "not as an
            // enum — it is: enum" is the sentence this used to print, and it
            // sends the reader looking for a spelling problem that isn't there.
            // What actually happened is that the declaration is empty: `enum
            // Never {}` has no variants to score, and `enum-coverage Never`
            // has nothing to say about it. Naming the declaration ends the
            // search where naming the kind restarted it.
            //
            // `fields` on a tuple struct does not land here — it asks for a
            // "struct with named fields" against a kind of "struct", so the
            // phrase already carries the difference.
            if kinds.contains(&what) {
                self.out.answer(&format!(
                    "note: `{}` is {} {} in the scanned tree, so the name is right — but the \
                     declaration is empty, and an empty one gives this command nothing to \
                     report. Check the source at {}:{}.",
                    name,
                    article(what),
                    what,
                    existing[0].file,
                    existing[0].line
                ));
                return;
            }
            self.out.answer(&format!(
                "note: `{}` is in the scanned tree but not as {} {} — it is: {}",
                name,
                article(what),
                what,
                kinds.join(", ")
            ));
            return;
        }
        // Not an item — but it may be a `let` binding or a closure, which the
        // index does not hold and never will: those are not items and have no
        // callers, fields or variants to report. Saying *that* ends the search;
        // saying "no such name" sends the reader off to grep for a definition
        // that is right there. One session hunted a `focus_regions` that turned
        // out to be `let push = |…|` a few lines away.
        if let Some(b) = crate::semantic::find_binding(self.files, name) {
            self.out.answer(&format!(
                "note: `{}` is not an item — it is {} at {}:{}. \
                 This tool indexes items (fn, struct, enum, impl, …); locals and \
                 closures have no callers or fields to report, so a text search \
                 is the right tool for them.",
                name, b.kind, b.file, b.line
            ));
            return;
        }
        // Before offering guesses: is it simply outside this run's `--scope`?
        //
        // The old order asked that only when there was nothing close, so the
        // *better* the fuzzy match the more confidently wrong the answer got.
        // `show rows_of` on this tree offered `Row`, `Out::row`, `Out::row_note`
        // and `group_of` — four production near-misses — while `rows_of` sat in
        // `tests/cli.rs`, unmentioned, because the run had not scanned it. A
        // reader concludes their name is wrong when their scope is. Parsing the
        // excluded files costs a handful of files on a path that was going to
        // exit 2 regardless.
        if let Some(d) = self.in_excluded_scope(name) {
            self.out.answer(&format!(
                "note: no {} `{}` in the scanned tree — but `{}` is there in the code \
                 `--scope` excluded: {} {} at {}:{}. Re-run with `--scope all`.",
                what, name, name, d.kind, d.qpath, d.file, d.line
            ));
            return;
        }
        // `mod::name` where `mod` glob-imports the module that defines `name`.
        // This is the name a reader writes from a call site: inside
        // `geom/boolean.rs`, `dist(a, b)` is spelled bare and the enclosing
        // module is `geom::boolean`, so `geom::boolean::dist` is the honest
        // guess — and it was answered with six near-misses in other modules,
        // the right item not among them. Asked before the fuzzy list, because
        // an exact resolution beats every guess.
        if let Some(d) = self.resolve_through_globs(name) {
            self.out.answer(&format!(
                "note: no {} `{}` — `{}` is defined as `{}` and reaches `{}` \
                 through a glob import (`use …::*`), so the two name one item. \
                 Use `{}`.",
                what,
                name,
                crate::ast::last_segment(name),
                d.qpath,
                crate::ast::module_of_path(name),
                d.qpath
            ));
            return;
        }
        // `Type::method` where the type is real and the method name is not.
        // Asked before the fuzzy list because the type half of the query is
        // evidence and the fuzzy list throws it away: it ranks on the last
        // segment and tie-breaks on shared *module* prefix, which a method
        // never shares with its own type. `show FrameSelectionCtx::compute`
        // (the name a stale doc comment gave; the real one was `refresh`) drew
        // `CircularArray::compute`, `Affine2D::compose` and four more — six
        // rows, none of them a member of the named type, while
        // `inventory --name 'FrameSelectionCtx::*'` had the answer in one call.
        // The reader read the list as "not here" and grepped the file instead.
        if name.contains("::") {
            if let Some((ty, decl, copies)) = self.owner_type_of(name) {
                let (members, total) =
                    self.idx.members_of(decl, crate::ast::last_segment(name), 8);
                // Naming one of several same-named types without saying so is
                // how the wrong one gets read as the only one.
                let chosen = if copies > 1 {
                    format!(
                        " ({} types in this tree are named `{}`; spell the module to pick another)",
                        copies, ty
                    )
                } else {
                    String::new()
                };
                // A variant is not an item and is not indexed, so an enum
                // qualifier's suggestions are its *methods* — an answer to a
                // question the reader may not have asked unless it says where
                // the variants are.
                let variants_note = if decl.kind == "enum" {
                    format!(
                        "  (note: `{}` is an enum — its variants are not items and are not \
                         listed above. `variants {}` names them.)",
                        ty, ty
                    )
                } else {
                    String::new()
                };
                if members.is_empty() {
                    self.out.answer(&format!(
                        "note: no {} `{}` — `{}` is {} {} at {}:{}{}, but it declares no \
                         member named `{}`, or any other.",
                        what,
                        name,
                        ty,
                        article(decl.kind),
                        decl.kind,
                        decl.file,
                        decl.line,
                        chosen,
                        crate::ast::last_segment(name)
                    ));
                    if !variants_note.is_empty() {
                        self.out.answer(&variants_note);
                        return;
                    }
                    // A type with no members and no variants has nothing more
                    // to offer, so the fuzzy list below still gets its turn.
                } else {
                    self.out.answer(&format!(
                        "note: no {} `{}` — but `{}` is {} {} at {}:{}{}, and it has {} member(s). \
                         Did you mean:",
                        what,
                        name,
                        ty,
                        article(decl.kind),
                        decl.kind,
                        decl.file,
                        decl.line,
                        chosen,
                        total
                    ));
                    for d in &members {
                        self.out
                            .answer(&format!("  {} {}\t{}:{}", d.kind, d.qpath, d.file, d.line));
                    }
                    if total > members.len() {
                        self.out.answer(&format!(
                            "  (note: {} of {} shown — `inventory --name '{}::*'` lists every member)",
                            members.len(),
                            total,
                            ty
                        ));
                    }
                    if !variants_note.is_empty() {
                        self.out.answer(&variants_note);
                    }
                    return;
                }
            }
        }
        let near = self.idx.similar_to_query(name, 6);
        if near.is_empty() {
            self.out.answer(&format!(
                "note: no {} `{}` in the scanned tree, and nothing close to it \
                 (try --scope all if it is test-only)",
                what, name
            ));
            return;
        }
        self.out
            .answer(&format!("note: no {} `{}`. Did you mean:", what, name));
        for d in &near {
            self.out
                .answer(&format!("  {} {}\t{}:{}", d.kind, d.qpath, d.file, d.line));
        }
        // The list holds one entry per distinct name, so a method defined in
        // four impls cannot crowd out the other candidates. That is right for
        // four impls and wrong in silence: with `geom::dist` and `trace::dist`
        // both in the tree, a query for `geom::boolean::dist` was answered with
        // `trace::dist` alone and the reader had no way to know a second `dist`
        // existed. Ranking now prefers the shared module prefix; this says when
        // the choice was made at all.
        let hidden: usize = near
            .iter()
            .map(|d| self.idx.lookup(&d.name).len().saturating_sub(1))
            .sum();
        if hidden > 0 {
            self.out.answer(&format!(
                "  (note: {} further item(s) share a name listed above and are not shown — \
                 one row per distinct name. `show <name> --all` prints every copy)",
                hidden
            ));
        }
    }

    /// The type a `Type::method` query names, when the tree has one — with how
    /// many types share that bare name, so a note built on it can say when it
    /// had to choose.
    ///
    /// The whole qualifier is tried before its last segment, because the last
    /// segment is not a type: this tree has `parse::Scope` and
    /// `suppress::Scope`, and matching on `Scope` alone answered
    /// `show suppress::Scope::Nope` by citing the enum in `parse.rs`. A reader
    /// who spelled the path out gets the one they spelled.
    ///
    /// Returns an `impl` header only when nothing declares the type in this
    /// tree, which is how `impl u8` and other foreign-type impls appear.
    fn owner_type_of(&self, query: &str) -> Option<(String, &crate::index::Defn, usize)> {
        let qualifier = crate::ast::module_of_path(query);
        if qualifier.is_empty() {
            return None;
        }
        let ty = crate::ast::last_segment(qualifier);
        let bare = self.idx.lookup(ty);
        let copies = bare
            .iter()
            .filter(|d| matches!(d.kind, "struct" | "enum" | "trait" | "type"))
            .count();
        let spelled = if qualifier == ty {
            Vec::new()
        } else {
            self.idx.lookup(qualifier)
        };
        let decl = pick_type_decl(&spelled).or_else(|| pick_type_decl(&bare))?;
        Some((ty.to_string(), decl, copies))
    }

    /// The item `mod::name` names when `mod` reaches `name` through a glob.
    ///
    /// Only the unambiguous case answers: if two globs in that module both
    /// supply the name, the query really is ambiguous and a confident answer
    /// would be a guess wearing the shape of a resolution.
    fn resolve_through_globs(&self, query: &str) -> Option<&crate::index::Defn> {
        if !query.contains("::") {
            return None;
        }
        let module = crate::ast::module_of_path(query);
        let name = crate::ast::last_segment(query);
        let mut found: Vec<&crate::index::Defn> = Vec::new();
        for f in self.files {
            if f.module != module {
                continue;
            }
            let Some(uses) = self.sem.uses_for(&f.path) else {
                continue;
            };
            for g in &uses.globs {
                let candidate = if g.is_empty() {
                    name.to_string()
                } else {
                    format!("{}::{}", g, name)
                };
                for d in self.idx.lookup(&candidate) {
                    if d.qpath == candidate && !found.iter().any(|x| x.qpath == d.qpath) {
                        found.push(d);
                    }
                }
            }
        }
        match found.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }

    /// Does `name` resolve in the files `--scope` left out of this run?
    ///
    /// Parses them on the spot. That is affordable *here and nowhere else*:
    /// every caller is already on its way to exit 2, and the set is whatever
    /// the scope excluded rather than the whole tree. Returns the first match
    /// in source order — one concrete location ends the search, where a list
    /// would just be a second set of guesses.
    fn in_excluded_scope(&self, name: &str) -> Option<crate::index::Defn> {
        let files = crate::parse::parse_excluded();
        if files.is_empty() {
            return None;
        }
        let idx = crate::index::NameIndex::build(&files);
        idx.lookup(name).first().map(|d| (*d).clone())
    }

    /// Is this file inside the run's `--changed-since` scope? Always true when
    /// there is no filter.
    ///
    /// Split out of [`retain_changed`](Self::retain_changed) because not
    /// everything scoping applies to arrives as a `Vec` of rows. The waiver
    /// ledger is the case that forced it: `audit`'s closing line tallies
    /// waivers that suppressed nothing, and under a scoped run every waiver in
    /// an unchanged file has zero hits by construction — the check dropped its
    /// rows there before the waiver could ever be consulted. Asking this per
    /// waiver is what keeps that tally from calling a live ledger dead.
    pub fn in_scope(&self, file: &str) -> bool {
        match &self.changed {
            None => true,
            Some(c) => c.contains_file(file),
        }
    }

    /// With `--changed-since`, keep only hits whose file is in the changed
    /// set (no-op otherwise). `file_of` extracts the hit's display path.
    pub fn retain_changed<T>(&self, items: &mut Vec<T>, file_of: impl Fn(&T) -> &str) {
        if self.changed.is_some() {
            items.retain(|it| self.in_scope(file_of(it)));
        }
    }
}

/// What `--changed-since <ref>` selected: the changed files, and for each
/// tracked one the line ranges `git diff -U0` reports as added or modified.
///
/// The ranges exist because a *file* filter gates on everything a touched file
/// ever accumulated. A 17-file refactor on one real tree surfaced a
/// `partial_cmp … unwrap_or` that no hunk in the diff came near, and the agent
/// had to run `git diff | grep -c partial_cmp` by hand to learn it was
/// pre-existing. The filter stays file-granular — nothing gets quieter — but
/// every kept row can now say which of the two it is.
#[derive(Clone, Debug)]
pub struct Changed {
    /// The ref the diff was taken against, so `audit --fail-on-new` can reuse
    /// it as its baseline without being told twice.
    pub git_ref: String,
    pub files: std::collections::HashSet<std::path::PathBuf>,
    /// Inclusive 1-based line ranges per canonical path. A file in `files`
    /// with no entry here is untracked — new in its entirety.
    pub hunks: std::collections::HashMap<std::path::PathBuf, Vec<(usize, usize)>>,
}

/// Which side of the diff a kept row is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// The row's lines intersect a changed hunk (or the file is untracked).
    Changed,
    /// The row is in a changed file, outside every changed hunk.
    PreExisting,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Changed => "changed",
            ChangeKind::PreExisting => "pre-existing",
        }
    }
}

impl Changed {
    pub fn contains_file(&self, file: &str) -> bool {
        std::fs::canonicalize(file)
            .map(|p| self.files.contains(&p))
            .unwrap_or(false)
    }

    /// Classify the line range `start..=end` of `file`. `None` when the file
    /// is not in the changed set at all — a row the filter should already have
    /// dropped.
    pub fn classify(&self, file: &str, start: usize, end: usize) -> Option<ChangeKind> {
        let p = std::fs::canonicalize(file).ok()?;
        if !self.files.contains(&p) {
            return None;
        }
        let Some(ranges) = self.hunks.get(&p) else {
            return Some(ChangeKind::Changed);
        };
        let hit = ranges.iter().any(|&(a, b)| a <= end && start <= b);
        Some(if hit {
            ChangeKind::Changed
        } else {
            ChangeKind::PreExisting
        })
    }
}

/// Parse `git diff -U0` output into `path -> [(start, end)]` of the new-side
/// line ranges. A pure-deletion hunk (`+N,0`) is recorded as `(N, N)`: the
/// lines around the deletion are where the change is.
fn parse_hunks(diff: &str) -> std::collections::HashMap<String, Vec<(usize, usize)>> {
    let mut out: std::collections::HashMap<String, Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.strip_prefix("b/").unwrap_or(path);
            current = (path != "/dev/null").then(|| path.to_string());
            continue;
        }
        if !line.starts_with("@@") {
            continue;
        }
        let Some(cur) = &current else { continue };
        // `@@ -a,b +c,d @@` — take the `+c,d` half.
        let Some(plus) = line.split_whitespace().find(|w| w.starts_with('+')) else {
            continue;
        };
        let mut it = plus[1..].split(',');
        let Some(start) = it.next().and_then(|n| n.parse::<usize>().ok()) else {
            continue;
        };
        let len = it.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1);
        let end = if len == 0 { start } else { start + len - 1 };
        out.entry(cur.clone()).or_default().push((start.max(1), end.max(1)));
    }
    out
}

/// Files changed vs `git_ref`: `git diff --name-only <ref>` (tracked changes,
/// staged or not) plus untracked files, with the changed line ranges of the
/// tracked ones. Paths are resolved against the repo top-level, so this works
/// from any CWD. Git is the only state consulted — there is no tracking file.
///
/// Asked of `root`'s repository, not the process's. Reading the CWD's repo
/// instead is the same failure the empty-`files` guard in `main` exists to
/// catch, one layer down and quieter: `unruster -r ../other/src
/// --changed-since HEAD audit` diffed *this* checkout, none of whose paths are
/// under `../other`, so every check dropped every row and the run reported
/// "0 gating + 0 advisory; clean; exit 0" over a tree it had not looked at.
// unruster: ok(error-swallows/if-let-ok) 2026-08-06 — a path that will not
// canonicalize is not in the working tree, which is precisely the reason to
// leave it out of the changed set.
pub fn changed_set(git_ref: &str, root: &std::path::Path) -> anyhow::Result<Changed> {
    use std::process::Command;
    // `--root` may name a file; git wants a directory.
    let at = match root.is_dir() {
        true => root,
        false => root.parent().unwrap_or(std::path::Path::new(".")),
    };
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git").arg("-C").arg(at).args(args).output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let top = std::path::Path::new(top.trim());
    let mut set = std::collections::HashSet::new();
    let listings = [
        git(&["diff", "--name-only", git_ref])?,
        git(&["ls-files", "--others", "--exclude-standard"])?,
    ];
    for listing in &listings {
        for line in listing.lines() {
            if line.is_empty() {
                continue;
            }
            if let Ok(p) = std::fs::canonicalize(top.join(line)) {
                set.insert(p);
            }
        }
    }
    let mut hunks = std::collections::HashMap::new();
    for (path, ranges) in parse_hunks(&git(&["diff", "-U0", git_ref])?) {
        if let Ok(p) = std::fs::canonicalize(top.join(&path)) {
            hunks.insert(p, ranges);
        }
    }
    Ok(Changed {
        git_ref: git_ref.to_string(),
        files: set,
        hunks,
    })
}

/// How strongly a row's match is grounded. Ordered weakest-first so
/// `--min-confidence <tier>` filters with a simple `>=`:
/// - `heuristic` — last-segment name match only; same-named items elsewhere
///   would also match.
/// - `inferred`  — matched through local type inference or an alias chain.
/// - `resolved`  — matched through a `use`-map resolution, a qualified path,
///   or a name with exactly one definition in the tree.
/// - `exact`     — structurally certain (e.g. `self.field` inside `impl Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Confidence {
    Heuristic,
    Inferred,
    Resolved,
    Exact,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Heuristic => "heuristic",
            Confidence::Inferred => "inferred",
            Confidence::Resolved => "resolved",
            Confidence::Exact => "exact",
        }
    }
}

/// Grouping dimension for commands that support `--by`. Parsed by clap
/// (value_enum), so an invalid value is rejected uniformly at the CLI boundary
/// instead of each command improvising its own fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum GroupBy {
    Fn,
    File,
    Module,
}

/// Typed error for "the queried target doesn't exist in the scanned tree".
/// `main` maps it to exit code 2 so scripts can distinguish "no findings"
/// (exit 0, empty output) from "the queried name isn't there". The warning
/// text is printed by [`warn_unknown_target`] before the scan runs; this error
/// itself is not printed again.
#[derive(Debug)]
pub struct TargetNotFound {
    pub what: String,
    pub name: String,
}

impl std::fmt::Display for TargetNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no {} `{}` found in the scanned tree", self.what, self.name)
    }
}

impl std::error::Error for TargetNotFound {}

impl TargetNotFound {
    pub fn err(what: &'static str, name: &str) -> anyhow::Error {
        Self::err_owned(what, name)
    }

    /// Same, for a `what` computed at run time.
    pub fn err_owned(what: &str, name: &str) -> anyhow::Error {
        anyhow::Error::new(TargetNotFound {
            what: what.to_string(),
            name: name.to_string(),
        })
    }
}

/// The declaration among `found` that a `Type::…` qualifier names: a real type
/// declaration for preference, an `impl` header only when the tree declares the
/// type nowhere (`impl u8`, and every other impl on a foreign type).
fn pick_type_decl<'a>(found: &[&'a crate::index::Defn]) -> Option<&'a crate::index::Defn> {
    found
        .iter()
        .find(|d| matches!(d.kind, "struct" | "enum" | "trait" | "type"))
        .or_else(|| found.iter().find(|d| d.kind == "impl"))
        .copied()
}

/// `a` or `an` for a target kind. The kinds are a fixed, tiny vocabulary
/// (`enum`, `impl`, `struct with named fields`, …), so the vowel test is exact
/// here rather than the usual approximation.
pub(crate) fn article(what: &str) -> &'static str {
    match what.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u' | 'A' | 'E' | 'I' | 'O' | 'U') => "an",
        _ => "a",
    }
}

/// Uniform up-front warning for a target the index doesn't know. The scan
/// still runs (macros and external names aren't indexed, so hits are possible);
/// commands that then find zero hits return [`TargetNotFound::err`] so main
/// exits with code 2.
///
/// Prefer [`AnalysisCtx::unknown_target`], which can tell "no such name" from
/// "that name is something else" and routes through `out` so `--json` keeps it.
/// This plain form remains for the few callers whose target is not an indexed
/// item at all (a cohort glob, a constructor path).
pub fn warn_unknown_target(what: &str, name: &str) {
    eprintln!(
        "warning: no {} `{}` found in the scanned tree; \
         a zero-hit result likely means the name doesn't exist here \
         (try --scope all if it's test-only)",
        what, name
    );
}
