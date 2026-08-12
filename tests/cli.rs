//! End-to-end CLI tests against `tests/fixtures/sample/`. Each test invokes
//! the built `unruster` binary, runs one subcommand, and asserts on key
//! substrings in the output. The fixture is hand-crafted to trigger every
//! detection path at least once.

use assert_cmd::Command;
use predicates::str::contains;

const FIXTURE: &str = "fixtures/sample";

fn ur() -> Command {
    Command::cargo_bin("unruster").unwrap()
}

// ── row / column assertion helpers (catch shape regressions) ──────────────

/// Non-blank data lines of `out`.
///
/// `(note: …)` lines are dropped: the `--top` truncation note is emitted on
/// *stdout* on purpose (a caller who writes `2>/dev/null` must still learn the
/// answer was cut), but it is commentary about the rows and not one of them.
/// No TSV row can collide with the prefix — a row's first cell is a kind, a
/// visibility or a name.
fn rows_of(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("(note:"))
        .map(str::to_string)
        .collect()
}

/// Every row must split into exactly `expected` tab-separated columns.
fn assert_tsv_cols(out: &[u8], expected: usize) {
    for line in rows_of(out) {
        let cols = line.split('\t').count();
        assert_eq!(
            cols, expected,
            "expected {} tab-cols, got {}: {:?}",
            expected, cols, line
        );
    }
}

/// `--summary` suppresses stdout entirely; assert nothing on stdout.
fn assert_summary_silent_stdout(args: &[&str]) {
    let out = ur().args(args).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.trim().is_empty(),
        "expected --summary to suppress stdout, got:\n{}",
        s
    );
    assert!(out.status.success(), "expected success");
}

/// Stdout of a run that may legitimately exit 2 (unknown target). The
/// explanation for a failed lookup goes to stdout on purpose, so a test that
/// reads it must tolerate the exit code that accompanies it.
fn ur_output_allow_2(args: &[&str]) -> String {
    let out = ur().args(args).output().unwrap();
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(2),
        "command errored (exit {:?}): {:?}",
        code,
        args
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run and return raw stdout, tolerating the exit-1 "findings remain" code.
/// `audit` reports findings by failing, so asserting success on it would only
/// pass against a fixture with nothing to find.
fn ur_stdout_allow_findings(args: &[&str]) -> Vec<u8> {
    let out = ur().args(args).output().unwrap();
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "command errored (exit {:?}): {:?}",
        code,
        args
    );
    out.stdout
}

/// Run and assert success; return raw stdout bytes.
fn ur_stdout(args: &[&str]) -> Vec<u8> {
    let out = ur().args(args).output().unwrap();
    assert!(out.status.success(), "command failed: {:?}", args);
    out.stdout
}

// ─── help / version ────────────────────────────────────────────────────────

#[test]
fn shows_help() {
    ur().arg("--help")
        .assert()
        .success()
        .stdout(contains("Query a Rust codebase"));
}

#[test]
fn shows_version() {
    ur().arg("--version").assert().success();
}

// ─── inventory ─────────────────────────────────────────────────────────────

#[test]
fn inventory_default_lists_known_items() {
    ur().args(["--root", FIXTURE, "inventory"])
        .assert()
        .success()
        .stdout(contains("Document"))
        .stdout(contains("Token"))
        .stdout(contains("Render"));
}

#[test]
fn inventory_kind_struct() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "struct"])
        .assert()
        .success()
        .stdout(contains("Document"))
        .stdout(contains("Boxx"));
}

#[test]
fn inventory_kind_enum() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "enum"])
        .assert()
        .success()
        .stdout(contains("Token"));
}

#[test]
fn inventory_vis_pub() {
    ur().args(["--root", FIXTURE, "inventory", "--vis", "pub", "--kind", "impl-fn"])
        .assert()
        .success()
        .stdout(contains("Document::new"));
}

#[test]
fn inventory_name_filters_on_the_bare_name_not_the_whole_row() {
    // The gap that produced `unruster inventory | grep -iE "profile|span"` in a
    // real session: `--kind`/`--vis` narrow by category and nothing narrowed by
    // name. A grep also matches the path and the doc column, and it takes the
    // row count and the `--top` cut down with the stderr it redirects.
    let out = ur_stdout(&["--root", FIXTURE, "inventory", "--name", "Document"]);
    let rows = rows_of(&out);
    assert!(!rows.is_empty(), "expected the fixture's `Document` items");
    for r in &rows {
        let name = r.split('\t').nth(3).unwrap();
        assert_eq!(
            name.rsplit("::").next().unwrap(),
            "Document",
            "matched something that is not the bare name: {}",
            r
        );
    }
    // A path segment must not match — that is the grep behaviour being replaced.
    assert!(
        rows_of(&ur_stdout(&["--root", FIXTURE, "inventory", "--name", "main"]))
            .iter()
            .all(|r| !r.contains("\tDocument\t")),
        "`--name main` matched by file path"
    );
}

#[test]
fn inventory_name_takes_a_glob_and_says_when_it_matches_nothing() {
    let globbed = rows_of(&ur_stdout(&["--root", FIXTURE, "inventory", "--name", "Doc*"]));
    assert!(
        globbed.iter().any(|r| r.contains("Document")),
        "`Doc*` should reach `Document`:\n{:?}",
        globbed
    );
    // A glob matching nothing is a typo more often than a fact about the tree,
    // and an empty listing under `(0 items)` says neither.
    let out = ur()
        .args(["--root", FIXTURE, "inventory", "--name", "Documnet"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(rows_of(&out.stdout).is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--name Documnet"), "the filter is unnamed:\n{}", err);
    assert!(err.contains("nothing matches `Documnet`"), "{}", err);
}

#[test]
fn inventory_name_is_smartcase() {
    // The case the flag was reached for: `inventory | grep -i mask`. Neither
    // `*mask*` (misses `Mask`) nor `*Mask*` (misses `load_mask_for`) covers it,
    // and `*` is the only metacharacter, so there is no character class to fall
    // back on. All-lowercase means case-insensitive; any uppercase means exact.
    let names = |pat: &str| -> Vec<String> {
        rows_of(&ur_stdout(&["--root", FIXTURE, "inventory", "--name", pat]))
            .iter()
            .map(|r| r.split('\t').nth(3).unwrap().to_string())
            .collect()
    };
    let loose = names("document");
    assert!(
        loose.iter().any(|n| n.ends_with("Document")),
        "an all-lowercase pattern should have reached `Document`:\n{:?}",
        loose
    );
    // An uppercase anywhere pins it back to exact.
    assert!(
        names("Document").iter().all(|n| n.ends_with("Document")),
        "a cased pattern must stay exact"
    );
    assert!(names("DOCUMENT").is_empty(), "`DOCUMENT` must not match");
}

#[test]
fn inventory_tree() {
    ur().args(["--root", FIXTURE, "inventory", "--tree"])
        .assert()
        .success()
        .stdout(contains("crate"));
}

#[test]
fn inventory_spans_upgrades_the_at_column_to_start_end() {
    let out = ur()
        .args(["--root", FIXTURE, "--spans", "inventory", "--kind", "fn"])
        .output()
        .unwrap();
    // Same five columns as without the flag — `--spans` widens the cell, it
    // does not add one. A new column would break every caller's `awk`.
    assert_tsv_cols(&out.stdout, 5);
    for line in rows_of(&out.stdout) {
        let at = line.split('\t').next_back().unwrap();
        let (_, range) = at.rsplit_once(':').unwrap();
        let (start, end) = range
            .split_once('-')
            .unwrap_or_else(|| panic!("expected file:start-end, got {:?}", at));
        let (start, end) = (
            start.parse::<usize>().unwrap(),
            end.parse::<usize>().unwrap(),
        );
        assert!(end >= start, "end before start in {:?}", at);
    }
}

#[test]
fn without_spans_the_at_column_is_unchanged() {
    let out = ur()
        .args(["--root", FIXTURE, "inventory", "--kind", "fn"])
        .output()
        .unwrap();
    for line in rows_of(&out.stdout) {
        let at = line.split('\t').next_back().unwrap();
        assert!(!at.contains('-'), "expected a bare file:line, got {:?}", at);
    }
}

#[test]
fn spans_does_not_move_a_finding_fingerprint() {
    // A fingerprint exists so an edit above a finding doesn't make it look new.
    // If `--spans` changed it, adding the flag would report the whole codebase
    // as new against a stored baseline — a formatting flag must not be able to
    // do that.
    let fps = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--root", FIXTURE, "--fingerprints"];
        args.extend_from_slice(extra);
        args.push("dead-code");
        let out = ur().args(&args).output().unwrap();
        let mut v: Vec<String> = rows_of(&out.stdout)
            .iter()
            .map(|l| l.split('\t').next_back().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let plain = fps(&[]);
    assert!(!plain.is_empty(), "fixture should have dead-code rows");
    assert_eq!(plain, fps(&["--spans"]));
}

#[test]
fn context_still_fires_when_the_at_cell_carries_a_span() {
    // `--context` finds its file/line by scanning the row's cells for a site.
    // The `--spans` upgrade swaps that cell for a different variant, and the
    // snippet would silently stop appearing if it only knew the old one.
    ur().args([
        "--root", FIXTURE, "--spans", "--context", "1", "inventory", "--kind", "fn",
    ])
    .assert()
    .success()
    .stdout(contains(">"));
}

// ─── CLI consistency ───────────────────────────────────────────────────────

#[test]
fn waiver_hygiene_notes_stay_off_commands_waivers_cannot_affect() {
    // These printed on every invocation of everything. On a `show` whose answer
    // is 49 bytes the preamble was 558 — eleven times the output, about a
    // subsystem the command does not consult.
    for args in [
        vec!["--root", WV, "show", "Arena", "--part", "span"],
        vec!["--root", WV, "outline", "lib.rs"],
        vec!["--root", WV, "inventory"],
    ] {
        let e = String::from_utf8_lossy(&ur().args(&args).output().unwrap().stderr).to_string();
        assert!(!e.contains("waiver(s)"), "{:?} printed waiver advice:\n{}", args, e);
    }
}

#[test]
fn waiver_hygiene_notes_survive_where_a_waiver_changes_the_answer() {
    let e = String::from_utf8_lossy(
        &ur().args(["--root", WV, "dead-code"]).output().unwrap().stderr,
    )
    .to_string();
    assert!(e.contains("waiver(s)"), "expected waiver advice on a waiver-aware check:\n{}", e);
}

#[test]
fn builder_drifts_positional_is_not_called_root() {
    // It is a constructor path. Displaying it as ROOT put it one line above
    // `-r, --root <ROOT>  Root directory to scan`, so the usage line read as
    // though the positional were a directory.
    let h = String::from_utf8_lossy(&ur_stdout(&["builder-drift", "--help"])).to_string();
    assert!(h.contains("builder-drift [OPTIONS] [CTOR]"), "{}", h);
}

#[test]
fn enum_taking_commands_say_enum_in_their_usage_line() {
    // `<NAME>` meant "enum" in five commands and "fn or item" in three, so the
    // usage line alone could not tell you which.
    for c in ["variants", "catch-all-arms", "parallel-matches", "enum-coverage", "divergence"] {
        let h = String::from_utf8_lossy(&ur_stdout(&[c, "--help"])).to_string();
        let usage = h.lines().find(|l| l.starts_with("Usage:")).unwrap();
        assert!(usage.contains("ENUM"), "{} usage says {:?}", c, usage);
    }
    // …and the item-taking ones still say NAME.
    for c in ["callers", "callees", "show"] {
        let h = String::from_utf8_lossy(&ur_stdout(&[c, "--help"])).to_string();
        let usage = h.lines().find(|l| l.starts_with("Usage:")).unwrap();
        assert!(usage.contains("NAME"), "{} usage says {:?}", c, usage);
    }
}

#[test]
fn a_right_name_of_the_wrong_kind_gets_one_answer_everywhere() {
    // `Document` is a struct. Handed to an enum-only command it used to produce
    // three different messages and two different exit codes across five
    // commands — and `variants` said "not found in the scanned tree", which is
    // false: it is right there, as a struct.
    for c in ["variants", "catch-all-arms", "parallel-matches", "enum-coverage", "divergence"] {
        let out = ur().args(["--root", FIXTURE, c, "Document"]).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{} exit", c);
        // The explanation is on stdout: it is the answer to the query, and a
        // reader who redirected stderr away must still get it.
        let e = String::from_utf8_lossy(&out.stdout);
        assert!(
            e.contains("is in the scanned tree but not as an enum") && e.contains("struct"),
            "{} said:\n{}",
            c,
            e
        );
    }
}

#[test]
fn a_zero_result_that_is_a_real_answer_stays_exit_0() {
    // The counterpart: for commands where any name could plausibly match, zero
    // hits is a finding, not a query error. Tightening these to 2 would be wrong.
    for c in ["callers", "callees", "type-refs", "takes-mut"] {
        let code = ur()
            .args(["--root", FIXTURE, c, "Document"])
            .output()
            .unwrap()
            .status
            .code();
        assert_eq!(code, Some(0), "{} should treat zero hits as an answer", c);
    }
}

#[test]
fn an_unknown_target_suggests_near_names_on_every_kind_requiring_command() {
    ur().args(["--root", FIXTURE, "variants", "Tokne"])
        .assert()
        .failure()
        .code(2)
        .stdout(contains("Did you mean"))
        .stdout(contains("Token"));
}

#[test]
fn the_unknown_target_note_survives_json() {
    // It went out through `eprintln!`, so `--json` consumers never saw it.
    // Exit is 2 (unknown target), so this reads stdout directly.
    let out = ur()
        .args(["--root", FIXTURE, "--json", "variants", "Tokne"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Did you mean"), "note missing from the JSON document:\n{}", s);
}

#[test]
fn public_only_is_spelled_the_same_way_everywhere() {
    // `inventory --vis pub` vs `dead-code --pub-only` vs `outline --pub-only`:
    // one filter, two words. `--vis` now works on all three.
    for args in [
        vec!["--root", FIXTURE, "inventory", "--vis", "pub"],
        vec!["--root", FIXTURE, "outline", "src/main.rs", "--vis", "pub"],
        vec!["--root", FIXTURE, "dead-code", "--vis", "pub"],
    ] {
        let out = ur_stdout_allow_findings(&args);
        for line in rows_of(&out) {
            assert_eq!(line.split('\t').nth(1), Some("pub"), "{:?}: {}", args, line);
        }
    }
}

#[test]
fn pub_only_still_works_as_the_shorthand() {
    let long = ur_stdout(&["--root", FIXTURE, "outline", "src/main.rs", "--vis", "pub"]);
    let short = ur_stdout(&["--root", FIXTURE, "outline", "src/main.rs", "--pub-only"]);
    assert_eq!(long, short);
}

#[test]
fn vis_and_pub_only_together_are_rejected_rather_than_silently_ranked() {
    ur().args(["--root", FIXTURE, "outline", "src/main.rs", "--vis", "priv", "--pub-only"])
        .assert()
        .failure();
}

#[test]
fn every_name_taking_command_suggests_near_names() {
    // One session ran `callers region_spec` (dead end, no suggestions) and then
    // `show region_spec` (the near-name list it needed) — two calls for one
    // question, because only `show` had been taught to answer it.
    for c in ["callers", "callees", "type-refs", "takes-mut", "show"] {
        let out = ur().args(["--root", FIXTURE, c, "Documnet"]).output().unwrap();
        let e = String::from_utf8_lossy(&out.stdout);
        assert!(e.contains("Did you mean"), "{} gave no suggestions:\n{}", c, e);
        assert!(e.contains("Document"), "{} did not find the near name:\n{}", c, e);
    }
    // …and the two-name and field forms.
    let e = String::from_utf8_lossy(
        &ur().args(["--root", FIXTURE, "field-uses", "Documnet", "name"])
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(e.contains("Did you mean"), "{}", e);
}

#[test]
fn a_warn_only_command_still_scans_after_suggesting() {
    // These warn and keep going on purpose — a name the index has never seen
    // can still be reached through a macro. The suggestion must not turn that
    // into an early exit.
    let out = ur().args(["--root", FIXTURE, "callers", "Documnet"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "unknown name is still exit 2");
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("0 call site(s)"), "the scan did not run:\n{}", e);
}

#[test]
fn a_cohort_glob_is_not_run_through_the_name_suggester() {
    // `--among 'wrap_in_*'` is a pattern, not an identifier; ranking near
    // names against it would be nonsense.
    let e = String::from_utf8_lossy(
        &ur().args(["--root", FIXTURE, "callers", "mark_pending", "--among", "no_such_*"])
            .output()
            .unwrap()
            .stderr,
    )
    .to_string();
    assert!(e.contains("cohort pattern"), "{}", e);
    assert!(!e.contains("Did you mean"), "suggested names for a glob:\n{}", e);
}

#[test]
fn a_failed_lookup_answers_through_a_stderr_redirect() {
    // The failure this closes: one session ran
    //   show <name> 2>/dev/null | head -30 || <fallback>
    // four times and got total silence each time — the suggestion erased by the
    // redirect, the `||` never firing because a pipeline exits with `head`'s
    // status. Every time, the reader concluded the tool had nothing.
    for args in [
        vec!["--root", FIXTURE, "show", "Documnet"],
        vec!["--root", FIXTURE, "variants", "Tokne"],
        vec!["--root", FIXTURE, "callers", "Documnet"],
        vec!["--root", FIXTURE, "fields", "Documnet"],
    ] {
        let out = ur().args(&args).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(
            s.contains("Did you mean") || s.contains("is in the scanned tree"),
            "{:?} said nothing on stdout:\n{}",
            args,
            s
        );
    }
}

#[test]
fn the_answer_is_still_in_the_json_notes() {
    let out = ur()
        .args(["--root", FIXTURE, "--json", "show", "Documnet"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("\"notes\""), "{}", s);
    assert!(s.contains("Did you mean"), "{}", s);
}

#[test]
fn a_name_that_is_only_a_closure_is_named_as_one() {
    // A session hunted a fn that turned out to be `let push = |…|` a few lines
    // away. The index holds items, not locals — but "no such name" sends the
    // reader looking for a definition that is right there.
    let root = std::env::temp_dir().join("unruster_closure_hint");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn build() -> u8 {\n    let tally = |a: u8| a + 1;\n    let plain = 3u8;\n    tally(plain)\n}\n",
    )
    .unwrap();
    let r = root.to_str().unwrap();

    let out = ur_output_allow_2(&["--root", r, "show", "tally"]);
    assert!(out.contains("a closure"), "{}", out);
    assert!(out.contains("lib.rs:2"), "{}", out);

    let out = ur_output_allow_2(&["--root", r, "show", "plain"]);
    assert!(out.contains("a local binding"), "{}", out);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_qualified_callers_query_resolves_to_the_item_or_says_why_not() {
    // `Type::method` used to match only the sites that spell the path out, so
    // `self.inner.ping()` missed and the command reported `(0 call site(s))` —
    // indistinguishable from a method nobody calls. Two sessions took the zero
    // at face value and went to `grep`.
    //
    // It now resolves to the item when the bare name belongs to one fn, which
    // is the better answer than any note could be. The note still has a job:
    // when the name is shared, resolution is impossible and the zero is real.
    let root = std::env::temp_dir().join("unruster_qualified_callers");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub struct Inner;\n\
         impl Inner { pub fn ping(&self) -> u8 { 1 } }\n\
         pub struct Outer { pub inner: Inner }\n\
         impl Outer { pub fn go(&self) -> u8 { self.inner.ping() } }\n",
    )
    .unwrap();
    let r = root.to_str().unwrap();

    let qualified = ur_output_allow_2(&["--root", r, "callers", "Inner::ping"]);
    assert!(
        qualified.contains("Outer::go"),
        "the receiver reached through a field must resolve now:\n{}",
        qualified
    );

    // Two `ping`s: nothing can attribute a bare `.ping()` to either, so the
    // pointer to the broader form is still the only useful answer.
    std::fs::write(
        src.join("lib.rs"),
        "pub struct Inner;\n\
         impl Inner { pub fn ping(&self) -> u8 { 1 } }\n\
         pub struct Other;\n\
         impl Other { pub fn ping(&self) -> u8 { 2 } }\n\
         pub struct Outer { pub inner: Inner }\n\
         impl Outer { pub fn go(&self) -> u8 { self.inner.ping() } }\n",
    )
    .unwrap();
    let ambiguous = ur_output_allow_2(&["--root", r, "callers", "Inner::ping"]);
    assert!(
        ambiguous.contains("Try `callers ping`"),
        "a genuinely unresolvable name still needs the broader form:\n{}",
        ambiguous
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_bare_callers_query_does_not_suggest_itself() {
    let out = ur_output_allow_2(&["--root", FIXTURE, "callers", "no_such_fn_xyz"]);
    assert!(!out.contains("Try `callers"), "{}", out);
}

#[test]
fn outline_points_at_the_batch_form_where_a_reader_has_a_list() {
    let e = String::from_utf8_lossy(
        &ur().args(["--root", FIXTURE, "outline", "src/main.rs"])
            .output()
            .unwrap()
            .stderr,
    )
    .to_string();
    assert!(e.contains("show <a> <b> <c>"), "{}", e);
}

#[test]
fn explain_resolves_a_command_name_not_just_a_defect_heading() {
    // Topics are titled by defect (`COPY-PASTED FUNCTIONS`); a reader arrives
    // holding the command they just ran. `explain clones` used to match nothing.
    for c in ["clones", "stringly", "tests", "waivers", "divergence"] {
        ur().args(["explain", c])
            .assert()
            .success()
            .stdout(contains("◇"));
    }
}

#[test]
fn explain_lists_rather_than_dumping_when_a_command_spans_many_recipes() {
    // `callers` appears in seven; 4 KB of prose is the opposite of what
    // `explain` is for.
    let out = ur_stdout(&["explain", "callers"]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("◇"), "expected a heading list, got bodies:\n{}", s);
    assert!(s.lines().count() > 2, "{}", s);
}

#[test]
fn explain_does_not_confuse_a_command_with_its_longer_namesake() {
    // `unruster conversions` must not answer for `conversion-pairs`.
    let a = ur_stdout(&["explain", "conversion-pairs"]);
    let b = ur_stdout(&["explain", "conversions"]);
    assert_ne!(a, b);
}

#[test]
fn explain_still_fails_loudly_on_a_topic_that_does_not_exist() {
    ur().args(["explain", "zzznosuchtopic"])
        .assert()
        .failure()
        .code(2);
}

// ─── show ──────────────────────────────────────────────────────────────────

#[test]
fn show_prints_the_whole_item_and_stops_at_its_closing_brace() {
    let out = ur()
        .args(["--root", FIXTURE, "show", "Document::new"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("pub fn new(name: String) -> Self {"), "{}", s);
    assert!(s.contains("transform: [0.0; 4]"), "{}", s);
    // `touch` is the next item in the impl block. A `+N` line budget would
    // either cut the body short or run into it; the AST range does neither.
    assert!(!s.contains("pub fn touch"), "ran past the item:\n{}", s);
}

#[test]
fn show_reports_the_exact_range_it_printed() {
    let out = ur()
        .args(["--root", FIXTURE, "show", "Document::new"])
        .output()
        .unwrap();
    let rows = rows_of(&out.stdout);
    let at = rows[0].split('\t').next_back().unwrap();
    let (_, range) = at.rsplit_once(':').unwrap();
    let (start, end) = range.split_once('-').unwrap();
    let (start, end) = (
        start.parse::<usize>().unwrap(),
        end.parse::<usize>().unwrap(),
    );
    // Header row plus exactly the source lines the range names.
    assert_eq!(rows.len() - 1, end - start + 1, "row count vs range:\n{:?}", rows);
}

#[test]
fn show_sig_stops_before_the_body() {
    let out = ur()
        .args(["--root", FIXTURE, "show", "Document::new", "--part", "sig", "--no-doc"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("pub fn new(name: String) -> Self"), "{}", s);
    assert!(!s.contains("transform: [0.0; 4]"), "body leaked in:\n{}", s);
}

#[test]
fn show_span_prints_the_range_and_no_source() {
    let out = ur()
        .args(["--root", FIXTURE, "show", "Document::new", "--part", "span"])
        .output()
        .unwrap();
    assert_eq!(rows_of(&out.stdout).len(), 1);
    assert_tsv_cols(&out.stdout, 4);
}

#[test]
fn show_finds_an_indented_method_that_a_caret_anchor_would_miss() {
    // `^pub fn` never matches a method inside an `impl` block. This is the
    // whole reason the grep idiom needs a per-file guess at indent width.
    ur().args(["--root", FIXTURE, "show", "render_row"])
        .assert()
        .success();
    ur().args(["--root", FIXTURE, "show", "Document::touch"])
        .assert()
        .success()
        .stdout(contains("self.transform[0] += 1.0;"));
}

#[test]
fn show_lists_rather_than_concatenating_when_a_name_is_ambiguous() {
    // `render` is a trait fn and its impl. Printing both bodies under one
    // header is the unreadable output this command exists to replace.
    let out = ur()
        .args(["--root", FIXTURE, "show", "render"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_tsv_cols(&out.stdout, 4);
    assert!(rows_of(&out.stdout).len() > 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("names"));
}

#[test]
fn an_ambiguity_listing_reports_each_items_real_extent() {
    // The crossed case that was missing: `--part sig` on an *ambiguous* name.
    // The listing prints no source, so its span is a fact about the item — but
    // it was taking the `--part` range, collapsing every non-fn row to its
    // declaration line. On one real tree a 351-line `impl PathData` was
    // catalogued as `51-51`, and the reader needed a second command to find out.
    let full = ur_stdout(&["--root", FIXTURE, "show", "Document", "--part", "span"]);
    let sig = ur_stdout(&["--root", FIXTURE, "show", "Document", "--part", "sig"]);
    let spans = |o: &[u8]| -> Vec<String> {
        rows_of(o)
            .iter()
            .map(|l| l.split('\t').next_back().unwrap().to_string())
            .collect()
    };
    assert!(rows_of(&full).len() > 1, "expected an ambiguous name");
    assert_eq!(
        spans(&full),
        spans(&sig),
        "--part changed the catalogue's spans"
    );
}

#[test]
fn a_listed_impl_block_is_not_reported_as_one_line() {
    // The concrete shape of the bug: an impl block's `sig_end` is its
    // declaration line, so under `--part sig` it collapsed to `N-N`.
    let out = ur_stdout(&["--root", FIXTURE, "show", "Document", "--part", "sig"]);
    let impl_row = rows_of(&out)
        .into_iter()
        .find(|l| l.starts_with("impl\t"))
        .expect("fixture should have an `impl Document`");
    let at = impl_row.split('\t').next_back().unwrap();
    let (_, range) = at.rsplit_once(':').unwrap();
    let (s, e) = range.split_once('-').unwrap();
    let (s, e) = (s.parse::<usize>().unwrap(), e.parse::<usize>().unwrap());
    assert!(e > s, "impl block catalogued as {} line(s): {}", e - s + 1, at);
}

#[test]
fn sig_of_a_struct_is_its_fields_and_not_an_unclosed_brace() {
    // `--part sig` treated every non-fn as "a fn whose signature is its first
    // line", so a struct answered with `pub struct Document {` and stopped —
    // an unclosed brace, no fields, and a header row claiming a range that
    // ended four lines short of the item. Wrong in the direction that costs
    // most: it reads as an answer. Three consecutive `--part sig` calls on
    // structs in one real session were each followed by a `sed -n 'A,Bp'`.
    let out = ur_stdout(&["--root", FIXTURE, "show", "Document", "--kind", "struct", "--part", "sig"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("pub struct Document {"), "{}", s);
    assert!(s.contains("pub transform: [f32; 4]"), "fields missing:\n{}", s);
    assert!(s.contains("children: Vec<Document>"), "fields missing:\n{}", s);
    assert!(
        s.lines().any(|l| l.trim_end() == "}"),
        "left the brace open:\n{}",
        s
    );
}

#[test]
fn sig_of_a_data_item_reports_the_same_span_as_the_item() {
    // The header row is a promise about the bytes below it. When `sig` stopped
    // at the declaration line the promise was still made — `681-682` for an
    // item running to 710 — and an AST tool saying that is worse than a grep
    // saying nothing.
    for kind in ["struct", "enum"] {
        let name = if kind == "struct" { "Document" } else { "Token" };
        let at = |part: &str| -> String {
            let out = ur_stdout(&["--root", FIXTURE, "show", name, "--kind", kind, "--part", part]);
            rows_of(&out)[0].split('\t').next_back().unwrap().to_string()
        };
        assert_eq!(at("sig"), at("span"), "`{}` sig span != item span", name);
    }
}

#[test]
fn sig_of_an_enum_is_its_variants() {
    let out = ur_stdout(&["--root", FIXTURE, "show", "Token", "--kind", "enum", "--part", "sig"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("pub enum Token {"), "{}", s);
    assert!(
        s.lines().filter(|l| l.starts_with("    ")).count() >= 2,
        "no variants under the header:\n{}",
        s
    );
}

#[test]
fn sig_of_a_container_says_which_command_lists_its_members() {
    // An `impl` is the one kind where the whole item is the wrong answer and
    // the header line answers nothing either. The range stays honest and the
    // note carries the way out, because the observed escape was
    // `impls | grep -A30 "impl Foo"` — thirty rows that sort after `Foo`.
    let out = ur()
        .args(["--root", FIXTURE, "show", "Document", "--kind", "impl", "--part", "sig", "--all"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("impl Document {"),
        "{:?}",
        out
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("outline "), "no route to the members:\n{}", err);
    assert!(err.contains("impls --of Document"), "{}", err);
}

#[test]
fn sig_of_a_fn_still_stops_at_the_return_type() {
    // The fix must not turn `sig` into `full` for the one kind it was right
    // about — that would delete the flag's whole reason to exist.
    let out = ur_stdout(&["--root", FIXTURE, "show", "Document::new", "--part", "sig"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("pub fn new(name: String) -> Self"), "{}", s);
    assert!(!s.contains("transform: [0.0; 4]"), "body leaked in:\n{}", s);
}

#[test]
fn show_bounds_its_own_output_and_names_the_flag_that_lifts_it() {
    // The command's own pitch is that it knows where an item ends — but
    // unbounded output is what makes a caller write `| head -N` to protect
    // itself, and *that* cut lands mid-expression in silence. Across one
    // measured session 17 of 20 invocations were piped and 5 were cut, twice
    // sending the reader back to `sed -n 'A,Bp'` for the rest of a body this
    // command had already located exactly. A bound the tool owns can say so.
    let dir = scratch("show-budget");
    let body: String = (0..400).map(|i| format!("    let _x{} = {};\n", i, i)).collect();
    std::fs::write(dir.join("src/lib.rs"), format!("pub fn big() {{\n{}}}\n", body)).unwrap();
    let root = dir.to_str().unwrap();

    let out = ur_stdout(&["--root", root, "show", "big"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("more line(s) to"), "no cut announced:\n{}", s);
    assert!(s.contains("--max-lines 0"), "the lift is unnamed:\n{}", s);
    // And it is on stdout, so it survives the redirect that surrounds it.
    assert!(
        !String::from_utf8_lossy(&ur().args(["--root", root, "show", "big"]).output().unwrap().stderr)
            .contains("more line(s)"),
        "the cut belongs with the rows"
    );

    // `--max-lines 0` is the explicit "all of it".
    let full = ur_stdout(&["--root", root, "show", "big", "--max-lines", "0"]);
    let f = String::from_utf8_lossy(&full);
    assert!(f.contains("let _x399"), "the lift did not lift:\n{}", &f[f.len() - 200..]);
    assert!(!f.contains("more line(s) to"), "cut anyway");
}

#[test]
fn show_of_an_ordinary_item_is_not_cut() {
    // The bound is a backstop for god-functions, not a default that truncates
    // normal reading. Every item in this tree's own `show` module is well
    // inside it.
    let out = ur_stdout(&["--root", FIXTURE, "show", "Document::new"]);
    assert!(
        !String::from_utf8_lossy(&out).contains("more line(s) to"),
        "a short item was cut"
    );
}

#[test]
fn a_name_that_exists_only_in_the_excluded_scope_says_so_instead_of_guessing() {
    // The old order asked "is it out of scope?" only when *nothing* was close,
    // so the better the fuzzy match the more confidently wrong the answer got.
    // `show rows_of` on this tree offered `Row`, `Out::row`, `Out::row_note`
    // and `group_of` — four production near-misses — while `rows_of` sat in
    // `tests/cli.rs`, unmentioned, because the run had not scanned it. A reader
    // concludes their name is wrong when their scope is.
    let dir = scratch("excluded-scope-name");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    // A near-miss in production, so the suggester has something to offer.
    std::fs::write(dir.join("src/lib.rs"), "pub fn row_of() {}\n").unwrap();
    std::fs::write(dir.join("tests/it.rs"), "fn rows_of() {}\n#[test]\nfn t() { rows_of() }\n")
        .unwrap();
    let root = dir.to_str().unwrap();

    let out = ur_output_allow_2(&["--root", root, "show", "rows_of"]);
    assert!(
        out.contains("`--scope` excluded") && out.contains("tests/it.rs"),
        "did not name where it actually is:\n{}",
        out
    );
    assert!(
        !out.contains("Did you mean"),
        "guessed instead of answering:\n{}",
        out
    );
    // A real typo still gets the near names — the new branch must not eat them.
    let typo = ur_output_allow_2(&["--root", root, "show", "roww_ofx"]);
    assert!(typo.contains("Did you mean"), "lost the suggester:\n{}", typo);
    // And under the wider scope it just resolves.
    ur().args(["--root", root, "--scope", "all", "show", "rows_of"])
        .assert()
        .success();
}

#[test]
fn show_of_a_struct_names_the_commands_that_answer_the_next_question() {
    // Observed: a session ran `show measure::Opened`, echoed "=== field uses
    // ===", and then wrote `grep -rn "\.cost\b|cost:"` — whose top hit was a
    // doc comment. It labelled the step with the command's own name and did
    // not use it.
    let out = ur()
        .args(["--root", FIXTURE, "show", "Document", "--kind", "struct"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("fields "), "no route to the fields:\n{}", err);
    assert!(err.contains("field-uses "), "{}", err);
    // A fn has no fields to ask about.
    let fun = ur()
        .args(["--root", FIXTURE, "show", "Document::new"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&fun.stderr).contains("field-uses"),
        "nagged about fields on a fn"
    );
}

#[test]
fn show_kind_disambiguates() {
    ur().args(["--root", FIXTURE, "show", "render", "--kind", "trait-fn"])
        .assert()
        .success()
        .stdout(contains("trait-fn"));
}

#[test]
fn show_says_which_kinds_exist_when_kind_filters_everything_out() {
    ur().args(["--root", FIXTURE, "show", "Document", "--kind", "enum"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("exists but not as a `enum`"))
        .stderr(contains("struct"));
}

#[test]
fn show_suggests_near_names_instead_of_printing_nothing() {
    // The expensive failure: a half-remembered name returns an empty result,
    // which is indistinguishable from "no such concept here".
    ur().args(["--root", FIXTURE, "show", "Document::nwe"])
        .assert()
        .failure()
        .code(2)
        .stdout(contains("Did you mean"))
        .stdout(contains("new"));
}

#[test]
fn show_unknown_name_with_nothing_close_says_so_plainly() {
    ur().args(["--root", FIXTURE, "show", "zzzqqqwwwyyy"])
        .assert()
        .failure()
        .code(2)
        .stdout(contains("nothing close to it"));
}

#[test]
fn show_json_carries_the_source_as_a_list_and_an_end_line() {
    let out = ur()
        .args(["--root", FIXTURE, "--json", "show", "Document::new"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("\"end_line\""), "{}", s);
    assert!(s.contains("\"source\""), "{}", s);
    // `line` still holds the start, so a consumer written against `Site`
    // keeps working.
    assert!(s.contains("\"line\""), "{}", s);
}

/// A throwaway workspace: two crates, each defining an enum of the given name
/// with different variants, each matched exhaustively over its own.
fn collide_workspace(dir: &str, enum_name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(dir);
    std::fs::remove_dir_all(&root).ok();
    for (krate, variants) in [("core_c", vec!["Buy", "Sell"]), ("ex_c", vec!["Long", "Short", "Flat"])] {
        let src = root.join(krate).join("src");
        std::fs::create_dir_all(&src).unwrap();
        let arms: String = variants
            .iter()
            .map(|v| format!("        {}::{} => \"{}\",\n", enum_name, v, v.to_lowercase()))
            .collect();
        std::fs::write(
            src.join("lib.rs"),
            format!(
                "pub enum {e} {{ {vs} }}\n\npub fn label_{k}(s: &{e}) -> &'static str {{\n    match s {{\n{arms}    }}\n}}\n",
                e = enum_name,
                vs = variants.join(", "),
                k = krate,
                arms = arms
            ),
        )
        .unwrap();
    }
    root
}

#[test]
fn same_named_enums_in_two_crates_are_not_merged_into_one() {
    // Two crates each define `enum Kindx`. Both matches are exhaustive over
    // their own type — the compiler would reject them otherwise. Scoring
    // against the *union* reported both as partial dispatch and listed the
    // other enum's variants as "missing", in a gating check. On one real
    // workspace this fired for seven enum names and the reader's only recourse
    // was to write the seven names down somewhere.
    let root = collide_workspace("unruster_collide_pm", "Kindx");
    let out = ur_stdout(&[
        "--root",
        root.to_str().unwrap(),
        "parallel-matches",
        "Kindx",
        "--show-missing",
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("Buy,Sell"), "expected both groups:\n{}", s);
    assert!(s.contains("Flat,Long,Short"), "expected both groups:\n{}", s);
    for bogus in ["missing: Long", "missing: Buy", "missing: Flat", "missing: Sell"] {
        assert!(!s.contains(bogus), "fabricated {:?} in:\n{}", bogus, s);
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_exhaustive_match_is_never_reported_as_partial_dispatch() {
    // The gating shape: `--hide-exhaustive` is what `audit` runs, so a match
    // the compiler already guarantees must produce no row at all.
    let root = collide_workspace("unruster_collide_hide", "Kindy");
    let out = ur_stdout(&[
        "--root",
        root.to_str().unwrap(),
        "parallel-matches",
        "Kindy",
        "--hide-exhaustive",
    ]);
    assert!(
        rows_of(&out).is_empty(),
        "exhaustive matches reported as partial:\n{}",
        String::from_utf8_lossy(&out)
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn same_named_enums_do_not_produce_divergence_pairs() {
    // Two `Kind`s whose variant sets overlap but differ. Under the union both
    // exhaustive matches look partial (2 of 3), they share `A` so they clear
    // the "not a disagreement" guard, and they are same-named fns in one file
    // so kinship fires — yielding a pair that accuses `beta::handle` of
    // forgetting `B`, a variant `beta::Kind` does not have.
    let root = std::env::temp_dir().join("unruster_collide_div");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
pub mod alpha {
    pub enum Kind { A, B }
    pub fn handle(k: &Kind) -> u8 {
        match k { Kind::A => 1, Kind::B => 2 }
    }
}
pub mod beta {
    pub enum Kind { A, C }
    pub fn handle(k: &Kind) -> u8 {
        match k { Kind::A => 1, Kind::C => 3 }
    }
}
"#,
    )
    .unwrap();
    let out = ur_stdout_allow_findings(&[
        "--root",
        root.to_str().unwrap(),
        "divergence",
        "Kind",
        "--min-score",
        "0",
    ]);
    assert!(
        rows_of(&out).is_empty(),
        "false divergence pair across two same-named enums:\n{}",
        String::from_utf8_lossy(&out)
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_workspace_crates_module_path_has_no_src_segment() {
    // `core/src/lib.rs` is `core::label`, not `core::src::label` — Rust has no
    // such segment, and every qualified path in a workspace carried it.
    let root = collide_workspace("unruster_srcseg", "Kindw");
    let out = ur_stdout(&["--root", root.to_str().unwrap(), "inventory", "--kind", "fn"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("core_c::label_core_c"), "{}", s);
    assert!(!s.contains("::src::"), "src segment still present:\n{}", s);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_single_crate_scan_still_drops_its_leading_src() {
    let out = ur_stdout(&["--root", FIXTURE, "inventory", "--kind", "fn"]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("::src::"), "{}", s);
}

#[test]
fn show_of_a_qualified_name_says_how_many_siblings_it_passed_over() {
    // `clones` reports a duplicated fn by its qualified path, so the obvious
    // next command is `show <that path>` — which returns one copy and used to
    // say nothing about the rest. That reads as a complete answer.
    ur().args(["--root", FIXTURE, "show", "Document::render"])
        .assert()
        .success()
        .stderr(contains("also named `render`"))
        .stderr(contains("--all"));
}

#[test]
fn show_of_a_bare_name_does_not_nag_about_siblings() {
    // A bare-name query already listed them; repeating the count is noise.
    let out = ur().args(["--root", FIXTURE, "show", "render"]).output().unwrap();
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(!e.contains("also named"), "{}", e);
}

#[test]
fn navigation_commands_do_not_print_the_macro_blind_spot_note() {
    // An unparseable `json!` changes neither where a fn starts nor where it
    // ends, so the warning answers a question `show`/`outline` never asked.
    for args in [
        vec!["--root", FIXTURE, "show", "Document::new", "--part", "span"],
        vec!["--root", FIXTURE, "outline", "src/main.rs"],
    ] {
        let out = ur().args(&args).output().unwrap();
        let e = String::from_utf8_lossy(&out.stderr);
        assert!(!e.contains("blind spots"), "{:?} printed it:\n{}", args, e);
    }
}

#[test]
fn an_analysis_command_still_reports_blind_spots() {
    let out = ur().args(["--root", FIXTURE, "inventory"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("blind spots"),
        "the note must survive where it is load-bearing"
    );
}

#[test]
fn show_resolves_several_names_in_one_pass() {
    let out = ur_stdout(&[
        "--root", FIXTURE, "show", "--part", "span", "Document::new", "Document::touch", "render_row",
    ]);
    let rows = rows_of(&out);
    assert_eq!(rows.len(), 3, "{:?}", rows);
    assert_tsv_cols(&out, 4);
}

#[test]
fn a_batch_survives_one_unresolvable_name() {
    // One bad name must not cost the other 114 lookups the batch existed to
    // save. Exit stays 0 because the run did answer.
    let out = ur().args([
        "--root", FIXTURE, "show", "--part", "span", "Document::new", "zzzznosuchname",
    ])
    .output()
    .unwrap();
    assert!(out.status.success());
    // One TSV row for the name that resolved, plus the explanation for the one
    // that did not — which is on stdout on purpose, so `2>/dev/null` cannot
    // erase it. Non-row lines are prefixed like `--context` snippets are.
    let s = String::from_utf8_lossy(&out.stdout);
    let data: Vec<&str> = s.lines().filter(|l| l.contains('\t')).collect();
    assert_eq!(data.len(), 1, "{}", s);
    assert!(s.contains("no item named `zzzznosuchname`"), "{}", s);
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 unresolved"));
}

#[test]
fn a_batch_where_nothing_resolves_is_still_exit_2() {
    ur().args(["--root", FIXTURE, "show", "zzzznosuchname", "qqqnosuchname"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn max_lines_bounds_a_long_body_and_says_it_did() {
    // `show … | head -40` cuts mid-body in silence. A cut the tool makes can
    // name what it dropped.
    let out = ur_stdout(&["--root", FIXTURE, "show", "Document::new", "--max-lines", "2"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("more line(s) to"), "no truncation notice:\n{}", s);
    assert!(!s.contains("children: vec![]"), "did not actually bound:\n{}", s);
}

#[test]
fn max_lines_is_a_no_op_when_the_body_is_shorter() {
    let bounded = ur_stdout(&["--root", FIXTURE, "show", "Document::touch", "--max-lines", "500"]);
    let plain = ur_stdout(&["--root", FIXTURE, "show", "Document::touch"]);
    assert_eq!(bounded, plain);
    assert!(!String::from_utf8_lossy(&bounded).contains("more line(s)"));
}

#[test]
fn max_lines_reports_the_drop_count_in_json() {
    let out = ur_stdout(&[
        "--root", FIXTURE, "--json", "show", "Document::new", "--max-lines", "2",
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\"truncated\""), "{}", s);
}

#[test]
fn show_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "show", "Document::new"]);
}

// ─── outline ───────────────────────────────────────────────────────────────

#[test]
fn outline_lists_a_files_items_with_end_lines() {
    let out = ur()
        .args(["--root", FIXTURE, "outline", "src/main.rs"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_tsv_cols(&out.stdout, 5);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Document"), "{}", s);
    assert!(s.contains("Token"), "{}", s);
    // Private items too — the `^pub fn` anchor cannot see these.
    assert!(s.contains("inner"), "{}", s);
}

#[test]
fn outline_resolves_a_trailing_path_fragment() {
    ur().args(["--root", FIXTURE, "outline", "main.rs"])
        .assert()
        .success()
        .stdout(contains("Document"));
}

#[test]
fn outline_matches_whole_path_components_only() {
    // `ain.rs` is a substring of `main.rs` but not a component of it, and a
    // command that silently resolved it would be answering about a file the
    // caller did not name.
    ur().args(["--root", FIXTURE, "outline", "ain.rs"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn outline_indents_impl_members_under_their_block() {
    let out = ur()
        .args(["--root", FIXTURE, "outline", "src/main.rs"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.lines().any(|l| l.contains("\t  new")),
        "expected an indented impl member:\n{}",
        s
    );
}

#[test]
fn outline_flat_drops_the_indent() {
    let out = ur()
        .args(["--root", FIXTURE, "outline", "src/main.rs", "--flat"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("\t  "), "expected no indent:\n{}", s);
}

#[test]
fn outline_docs_column_carries_the_doc_first_line() {
    ur().args(["--root", FIXTURE, "outline", "src/main.rs", "--docs"])
        .assert()
        .success()
        .stdout(contains("Top-level variant constant"));
}

#[test]
fn outline_pub_only_narrows_to_the_files_surface() {
    let out = ur()
        .args(["--root", FIXTURE, "outline", "src/main.rs", "--pub-only"])
        .output()
        .unwrap();
    for line in rows_of(&out.stdout) {
        assert_eq!(line.split('\t').nth(1), Some("pub"), "{}", line);
    }
}

#[test]
fn outline_of_an_unscanned_file_says_why_rather_than_listing_nothing() {
    ur().args(["--root", FIXTURE, "outline", "tests/dummy.rs"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("--scope all"));
}

#[test]
fn outline_of_a_nonexistent_file_says_that_instead() {
    ur().args(["--root", FIXTURE, "outline", "src/no_such_file.rs"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("no such path exists"));
}

#[test]
fn outline_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "outline", "src/main.rs"]);
}

// ─── callers / callees ─────────────────────────────────────────────────────

#[test]
fn callers_bare_name_matches_methods_and_macros() {
    ur().args(["--root", FIXTURE, "callers", "println"])
        .assert()
        .success()
        .stdout(contains("println!"));
}

#[test]
fn callers_qualified() {
    ur().args(["--root", FIXTURE, "callers", "Document::new"])
        .assert()
        .success()
        .stdout(contains("Document::new"));
}

#[test]
fn callers_macro_only_with_bang() {
    ur().args(["--root", FIXTURE, "callers", "println!"])
        .assert()
        .success()
        .stdout(contains("println!"));
}

#[test]
fn callers_transitive() {
    ur().args([
        "--root",
        FIXTURE,
        "callers",
        "--transitive",
        "--depth",
        "3",
        "Document::new",
    ])
    .assert()
    .success();
}

#[test]
fn callers_by_file_groups() {
    ur().args(["--root", FIXTURE, "callers", "--by", "file", "Document::new"])
        .assert()
        .success();
}

#[test]
fn callees_lists_calls_inside_fn() {
    ur().args(["--root", FIXTURE, "callees", "main"])
        .assert()
        .success();
}

// ─── callers --among / cohort-callees (sibling-cohort divergence) ──────────

#[test]
fn callers_among_marks_present_and_absent() {
    // `wrap_in_group` / `wrap_in_composite` call `mark_pending`;
    // `wrap_in_transform` (the defect) does not.
    let out = ur_stdout(&[
        "--root", FIXTURE, "callers", "mark_pending", "--among", "wrap_in_*",
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("✓\twrap_in_group"), "expected ✓ for wrap_in_group:\n{}", s);
    assert!(s.contains("✓\twrap_in_composite"), "expected ✓ for wrap_in_composite:\n{}", s);
    assert!(
        s.contains("✗\twrap_in_transform"),
        "expected ✗ for wrap_in_transform (the divergence):\n{}",
        s
    );
}

#[test]
fn callers_among_unknown_cohort_exits_2() {
    ur().args(["--root", FIXTURE, "callers", "mark_pending", "--among", "no_such_cohort_*"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no fn or method matching cohort pattern"));
}

#[test]
fn callers_among_summary_mode() {
    assert_summary_silent_stdout(&[
        "--root", FIXTURE, "--summary", "callers", "mark_pending", "--among", "wrap_in_*",
    ]);
}

#[test]
fn cohort_callees_matrix_flags_divergence() {
    let out = ur_stdout(&["--root", FIXTURE, "cohort-callees", "wrap_in_*"]);
    let s = String::from_utf8_lossy(&out);
    // Header lists the cohort columns.
    assert!(s.contains("wrap_in_group"), "header should list cohort fns:\n{}", s);
    // `mark_pending` is called by 2/3 → flagged as divergence.
    let diverge_line = s
        .lines()
        .find(|l| l.contains("mark_pending"))
        .unwrap_or("");
    assert!(
        diverge_line.contains("divergence"),
        "mark_pending row should be flagged:\n{}",
        s
    );
    // `arena_insert` is unanimous → NOT flagged.
    let unanimous_line = s.lines().find(|l| l.contains("arena_insert")).unwrap_or("");
    assert!(
        !unanimous_line.contains("divergence"),
        "unanimous callee must not be flagged:\n{}",
        s
    );
}

#[test]
fn cohort_callees_unknown_cohort_exits_2() {
    ur().args(["--root", FIXTURE, "cohort-callees", "no_such_cohort_*"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("no fn or method matching cohort pattern"));
}

#[test]
fn cohort_callees_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "cohort-callees", "wrap_in_*"]);
}

// ─── co-call (paired-action invariant) ─────────────────────────────────────

#[test]
fn co_call_flags_asymmetric_caller() {
    // `wrap_in_group` / `wrap_in_composite` call both `arena_insert` and
    // `mark_pending`; `wrap_in_transform` (the defect) calls A but not B.
    let out = ur_stdout(&["--root", FIXTURE, "co-call", "arena_insert", "mark_pending"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().any(|l| l.starts_with("A-only") && l.contains("wrap_in_transform")),
        "expected wrap_in_transform flagged as A-only (calls A, not B):\n{}",
        s
    );
    // The canonical both-callers must NOT be listed as suspects.
    assert!(
        !s.contains("wrap_in_group") && !s.contains("wrap_in_composite"),
        "both-callers should not appear as rows:\n{}",
        s
    );
    // Each suspect row carries a `via file:line` pointer.
    assert!(s.contains("via "), "expected a `via` pointer:\n{}", s);
}

#[test]
fn co_call_flags_b_only_direction() {
    // Reverse the pair: now `mark_pending` is A and `arena_insert` is B, so
    // `wrap_in_transform` (calls arena_insert, not mark_pending) is B-only.
    let out = ur_stdout(&["--root", FIXTURE, "co-call", "mark_pending", "arena_insert"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().any(|l| l.starts_with("B-only") && l.contains("wrap_in_transform")),
        "expected wrap_in_transform flagged as B-only (calls B, not A):\n{}",
        s
    );
}

#[test]
fn co_call_summary_counts_both_callers() {
    // Summary goes to stderr; with --summary stdout is silent.
    ur().args(["--root", FIXTURE, "--summary", "co-call", "arena_insert", "mark_pending"])
        .assert()
        .success()
        .stdout("")
        .stderr(predicates::str::contains("call both"));
}

#[test]
fn co_call_unknown_symbol_warns_and_exits_2() {
    ur().args(["--root", FIXTURE, "co-call", "no_such_fn_xyz", "mark_pending"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("no fn, method, or macro `"));
}

// ─── field / fields ────────────────────────────────────────────────────────

#[test]
fn field_uses_strict_finds_self_writes() {
    ur().args(["--root", FIXTURE, "field-uses", "Document", "transform"])
        .assert()
        .success()
        .stdout(contains("Document::touch"));
}

#[test]
fn field_uses_candidates_includes_unknown_receivers() {
    ur().args([
        "--root",
        FIXTURE,
        "field-uses",
        "Document",
        "transform",
        "--candidates",
    ])
    .assert()
    .success();
}

#[test]
fn field_uses_writes_only_filter() {
    ur().args([
        "--root",
        FIXTURE,
        "field-uses",
        "Document",
        "transform",
        "--kind", "write",
    ])
    .assert()
    .success();
}

#[test]
fn field_uses_hint_when_strict_empty_but_candidates_match() {
    // No `impl NoSuchType { self.transform = ... }` exists, but many other
    // `self.transform` accesses do — strict matches 0, candidates would match
    // many. Exercises the hint code in field.rs.
    let out = ur()
        .args(["--root", FIXTURE, "field-uses", "NoSuchType", "transform"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hint:"),
        "expected hint about candidates, got stderr:\n{}",
        stderr
    );
}

#[test]
fn field_uses_via_receiver_filter() {
    ur().args([
        "--root",
        FIXTURE,
        "field-uses",
        "Document",
        "transform",
        "--candidates",
        "--via-receiver",
        "self",
    ])
    .assert()
    .success();
}

#[test]
fn fields_lists_struct_fields_with_counts() {
    ur().args(["--root", FIXTURE, "fields", "Document"])
        .assert()
        .success()
        .stdout(contains("transform"))
        .stdout(contains("name"));
}

#[test]
fn the_type_commands_take_a_qualified_name_like_every_other_command() {
    // `show` prints the *qualified* path in its header row, the playbook says
    // targets resolve by last `::` segment, and `impls --of`/`callers` do — but
    // these three compared the raw string against a bare `ident`, so the name a
    // reader had just copied off `show` matched nothing. `fields index::Defn`
    // answered "(0 field(s))" under a note contradicting itself ("not as a
    // struct with named fields — it is: struct"). A command that says "none"
    // for a copied name teaches the reader it does not work.
    let rows = |args: &[&str]| -> usize {
        let mut full = vec!["--root", FIXTURE];
        full.extend(args);
        rows_of(&ur_stdout_allow_findings(&full)).len()
    };
    assert_eq!(
        rows(&["fields", "Document"]),
        rows(&["fields", "main::Document"]),
        "fields disagreed with itself over a qualified name"
    );
    assert_eq!(
        rows(&["field-uses", "Document", "transform"]),
        rows(&["field-uses", "main::Document", "transform"]),
        "field-uses disagreed with itself over a qualified name"
    );
    assert_eq!(
        rows(&["variants", "Token"]),
        rows(&["variants", "main::Token"]),
        "variants disagreed with itself over a qualified name"
    );
    // Non-zero, or the equality above is satisfied by two empty answers.
    assert!(rows(&["fields", "main::Document"]) > 0);
}

#[test]
fn fields_exotic_field_types() {
    // Drives ast::type_to_string through Tuple / Array / Ptr / TraitObject /
    // BareFn / Parenthesized / QSelf / leading `::` / Never branches.
    ur().args(["--root", FIXTURE, "fields", "ExoticFields"])
        .assert()
        .success()
        .stdout(contains("tup"))
        .stdout(contains("fn_ptr"));
}

// (Was `type_refs_array_type` — actually called `impls`, redundant with
// `impls_lists_all_blocks`. Removed.)

// ─── variants ──────────────────────────────────────────────────────────────

#[test]
fn variants_lists_defs_and_sites() {
    ur().args(["--root", FIXTURE, "variants", "Token"])
        .assert()
        .success()
        .stdout(contains("Token::Eof"))
        .stdout(contains("Token::Resize"));
}

#[test]
fn variants_bare_matches_bare_paths() {
    ur().args(["--root", FIXTURE, "variants", "Token", "--bare"])
        .assert()
        .success();
}

// ─── impls ─────────────────────────────────────────────────────────────────

#[test]
fn impls_lists_all_blocks() {
    ur().args(["--root", FIXTURE, "impls"])
        .assert()
        .success()
        .stdout(contains("Document"));
}

#[test]
fn impls_filter_by_trait() {
    ur().args(["--root", FIXTURE, "impls", "--trait", "Render"])
        .assert()
        .success()
        .stdout(contains("Render"));
}

#[test]
fn impls_filter_by_self_type() {
    ur().args(["--root", FIXTURE, "impls", "--of", "Document"])
        .assert()
        .success();
}

#[test]
fn an_unfiltered_impls_listing_names_the_commands_that_narrow_it() {
    // The listing is a wall, and the escape people write is
    // `impls | grep -A30 "impl Mask"` — which returns the rows that sort after
    // `Mask`, not its members. Observed verbatim in a real session, followed by
    // `outline src/mask.rs`, which is what actually answered.
    let big = ur().args(["--root", FIXTURE, "impls"]).output().unwrap();
    let err = String::from_utf8_lossy(&big.stderr);
    assert!(err.contains("impls --of <Type>"), "no route offered:\n{}", err);
    assert!(err.contains("outline <file>"), "{}", err);
    // A caller who already narrowed does not need telling.
    let narrowed = ur()
        .args(["--root", FIXTURE, "impls", "--of", "Document"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&narrowed.stderr).contains("unfiltered"),
        "nagged a caller who had already filtered"
    );
}

#[test]
fn impls_header_carries_the_traits_generic_arguments() {
    // The header used to render the trait path without its arguments, so
    // `impl Tag<u32> for Boxx` and `impl Tag<String> for Boxx` both came out as
    // `impl Tag for Boxx` — two rows a reader could only tell apart by going and
    // opening the file, which is the one thing a header exists to prevent.
    let out = ur_stdout(&["--root", FIXTURE, "impls", "--of", "Boxx"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("impl Tag<u32> for Boxx"), "{}", s);
    assert!(s.contains("impl Tag<String> for Boxx"), "{}", s);
    assert!(
        !s.contains("impl Tag for Boxx"),
        "trait arguments dropped again:\n{}",
        s
    );
}

#[test]
fn every_impl_header_in_the_fixture_is_distinct() {
    // The property, rather than the two examples: no two impl blocks may render
    // to the same string unless they really are two inherent blocks on one type
    // (which the fixture has none of).
    let out = ur_stdout(&["--root", FIXTURE, "inventory", "--kind", "impl"]);
    // Column 3, not 2: `inventory` now carries `loc` between `vis` and `name`,
    // the same five cells `outline` emits.
    let mut headers: Vec<String> = rows_of(&out)
        .iter()
        .map(|l| l.split('\t').nth(3).unwrap().to_string())
        .collect();
    assert!(!headers.is_empty(), "fixture should have impl blocks");
    headers.sort();
    let before = headers.len();
    headers.dedup();
    assert_eq!(before, headers.len(), "duplicate impl headers: {:?}", headers);
}

#[test]
fn impls_filters_still_take_the_bare_trait_name() {
    // The rendered header gained arguments; the filters did not. `--trait
    // Tag<u32>` is not a thing anyone would type, and requiring it would make
    // the common query unspellable.
    let out = ur_stdout(&["--root", FIXTURE, "impls", "--trait", "Tag"]);
    assert_eq!(rows_of(&out).len(), 2, "{}", String::from_utf8_lossy(&out));
}

#[test]
fn nested_and_elided_generics_render_without_panicking() {
    // `type_to_string` elides only what cannot distinguish two types (a
    // computed const generic, a variant syn added since). Whatever it writes,
    // it must never panic and never render empty — an impl header is a
    // `qpath`, and an empty one names nothing.
    let out = ur_stdout(&["--root", FIXTURE, "inventory", "--kind", "impl"]);
    for line in rows_of(&out) {
        let header = line.split('\t').nth(3).unwrap();
        assert!(header.starts_with("impl "), "{}", header);
        assert!(header.len() > "impl ".len(), "empty header: {:?}", header);
    }
}

// ─── type-refs ─────────────────────────────────────────────────────────────

#[test]
fn type_refs_resolves_aliases() {
    ur().args(["--root", FIXTURE, "type-refs", "Document"])
        .assert()
        .success()
        .stdout(contains("Document"));
}

#[test]
fn type_refs_via_alias() {
    ur().args(["--root", FIXTURE, "type-refs", "Doc"])
        .assert()
        .success();
}

#[test]
fn type_refs_unknown_warns_and_exits_2() {
    ur().args(["--root", FIXTURE, "type-refs", "NotAType"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("no type `NotAType`"));
}

#[test]
fn type_refs_in_submodule_file() {
    // Exercises the `module-not-empty` path inside RefVisitor::enclosing.
    ur().args(["--root", FIXTURE, "type-refs", "G1"])
        .assert()
        .success();
}

#[test]
fn type_refs_tuple_struct_ctor() {
    // `TupleS(1, 2)` is a single-segment Expr::Call — type_refs.rs len==1 branch.
    ur().args(["--root", FIXTURE, "type-refs", "TupleS"])
        .assert()
        .success()
        .stdout(contains("TupleS"));
}

// ─── takes-mut ─────────────────────────────────────────────────────────────

#[test]
fn takes_mut_finds_mut_params() {
    ur().args(["--root", FIXTURE, "takes-mut", "Document"])
        .assert()
        .success()
        .stdout(contains("Document::touch"));
}

#[test]
fn takes_mut_with_u8_param() {
    // Finds &mut u8 params in exotic.rs — exercises module-non-empty enclosing.
    ur().args(["--root", FIXTURE, "takes-mut", "u8"])
        .assert()
        .success();
}

#[test]
fn takes_mut_unknown_type_warns_and_exits_2() {
    // Exercises the knows_name false branch (warning + exit 2 on zero hits).
    let out = ur()
        .args(["--root", FIXTURE, "takes-mut", "NoSuchType"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("no type `NoSuchType`"));
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn callees_unknown_fn_warns_and_exits_2() {
    let out = ur()
        .args(["--root", FIXTURE, "callees", "no_such_fn_xyz"])
        .output()
        .unwrap();
    // The explanation is the answer, so it goes to stdout where `2>/dev/null`
    // cannot erase it; the summary line stays on stderr with every other summary.
    assert!(String::from_utf8_lossy(&out.stdout).contains("no fn or method `"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("0 distinct callees"));
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn pass_through_method_call_form() {
    // wrap_method body is `d.render()` — Expr::MethodCall.
    ur().args(["--root", FIXTURE, "pass-through"])
        .assert()
        .success()
        .stdout(contains("wrap_method"));
}

#[test]
fn pass_through_macro_form() {
    // wrap_macro_call body is `println!("x")` — Expr::Macro.
    ur().args(["--root", FIXTURE, "pass-through"])
        .assert()
        .success()
        .stdout(contains("wrap_macro_call"));
}

// ─── metrics ───────────────────────────────────────────────────────────────

#[test]
fn metrics_sort_loc() {
    ur().args(["--root", FIXTURE, "metrics", "--sort", "loc"])
        .assert()
        .success()
        .stdout(contains("loc:"));
}

#[test]
fn metrics_sort_cyclo() {
    ur().args(["--root", FIXTURE, "metrics", "--sort", "cyclo"])
        .assert()
        .success()
        .stdout(contains("cyclo:"));
}

#[test]
fn metrics_sort_nesting() {
    ur().args(["--root", FIXTURE, "metrics", "--sort", "nesting"])
        .assert()
        .success()
        .stdout(contains("nesting:"));
}

#[test]
fn metrics_threshold_filters() {
    ur().args([
        "--root",
        FIXTURE,
        "metrics",
        "--sort",
        "cyclo",
        "--threshold",
        "3",
    ])
    .assert()
    .success();
}

// ─── dead-code ─────────────────────────────────────────────────────────────

#[test]
fn dead_code_finds_really_dead() {
    ur().args(["--root", FIXTURE, "dead-code"])
        .assert()
        .success()
        .stdout(contains("really_dead"));
}

#[test]
fn dead_code_skips_allow_dead_code_attr() {
    let out = ur()
        .args(["--root", FIXTURE, "dead-code"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("intentionally_dead"),
        "intentionally_dead should be filtered by #[allow(dead_code)], got:\n{}",
        s
    );
}

#[test]
fn dead_code_skips_macro_rules_referenced() {
    let out = ur()
        .args(["--root", FIXTURE, "dead-code"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("format_backtrace"),
        "format_backtrace is referenced inside macro_rules! body — should not be dead:\n{}",
        s
    );
}

#[test]
fn dead_code_sees_calls_inside_unparseable_macro_arms() {
    // `kv_row!("age" => age_label())`: the `=>` arm is not an expression, so
    // the chunk parse drops it while still succeeding on the other chunks —
    // no blind spot is even recorded. The call-set therefore reads raw macro
    // tokens too. Found by running unruster on itself, where `row!(…)` made
    // a live `age_str` look dead.
    let out = ur_stdout(&["--root", FIXTURE, "dead-code"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        !s.contains("age_label"),
        "age_label is called inside a `=>` macro arm — not dead:\n{}",
        s
    );
}

#[test]
fn dead_code_pub_only() {
    ur().args(["--root", FIXTURE, "dead-code", "--pub-only"])
        .assert()
        .success();
}

// ─── catch-all-arms / parallel-matches ─────────────────────────────────────

#[test]
fn catch_all_arms_finds_wildcard() {
    ur().args(["--root", FIXTURE, "catch-all-arms", "Token"])
        .assert()
        .success()
        .stdout(contains("classify"));
}

#[test]
fn parallel_matches_groups_match_sites() {
    ur().args(["--root", FIXTURE, "parallel-matches", "Token"])
        .assert()
        .success()
        .stdout(contains("group"));
}

#[test]
fn parallel_matches_partial_hides_exhaustive_group() {
    // `dispatch` covers all four Token variants (exhaustive). Default output
    // includes that group; --partial must drop it.
    let full = ur_stdout(&["--root", FIXTURE, "parallel-matches", "Token"]);
    let full = String::from_utf8_lossy(&full);
    assert!(full.contains("Eof,Number,Resize,Word"), "exhaustive group expected by default");

    let part = ur_stdout(&["--root", FIXTURE, "parallel-matches", "Token", "--hide-exhaustive"]);
    let part = String::from_utf8_lossy(&part);
    assert!(
        !part.contains("Eof,Number,Resize,Word"),
        "--partial should hide the exhaustive group, got:\n{}",
        part
    );
    // Partial groups (with `_`) survive.
    assert!(part.contains(" | _"), "partial groups should remain:\n{}", part);
}

#[test]
fn parallel_matches_rank_by_gap_and_show_missing() {
    let out = ur_stdout(&[
        "--root", FIXTURE, "parallel-matches", "Token",
        "--rank-by-gap", "--show-missing", "--hide-exhaustive",
    ]);
    let s = String::from_utf8_lossy(&out);
    // rank-by-gap prefixes the [covered/total] ratio.
    assert!(s.contains("[3/4]"), "expected [3/4] ratio prefix:\n{}", s);
    // The 3/4 group must come before the 2/4 group (higher coverage = louder).
    let i3 = s.find("[3/4]").unwrap();
    let i2 = s.find("[2/4]").unwrap();
    assert!(i3 < i2, "3/4 group should rank above 2/4:\n{}", s);
    // show-missing names uncovered variants.
    assert!(s.contains("missing: Resize"), "expected missing list:\n{}", s);
}

#[test]
fn parallel_matches_include_matches_macro() {
    // `matches_guard` uses `matches!(t, Token::Number(...))` — only surfaced
    // with --include-matches-macro.
    let without = ur_stdout(&["--root", FIXTURE, "parallel-matches", "Token"]);
    assert!(!String::from_utf8_lossy(&without).contains("matches!"));

    let with = ur_stdout(&[
        "--root", FIXTURE, "parallel-matches", "Token", "--include-matches-macro",
    ]);
    assert!(
        String::from_utf8_lossy(&with).contains("matches!"),
        "expected a (matches!) site with --include-matches-macro"
    );
}

#[test]
fn parallel_matches_summary_mode_with_flags() {
    assert_summary_silent_stdout(&[
        "--root", FIXTURE, "--summary", "parallel-matches", "Token",
        "--hide-exhaustive", "--rank-by-gap", "--show-missing", "--include-matches-macro",
    ]);
}

// ─── enum-coverage ─────────────────────────────────────────────────────────

#[test]
fn enum_coverage_ranks_partials_by_gap() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Token"]);
    let s = String::from_utf8_lossy(&out);
    // Highest-coverage partial (3/4) first, lowest (1/4) last.
    assert!(s.contains("0.75"), "expected a 0.75 gap_score row:\n{}", s);
    let i_high = s.find("0.75").unwrap();
    let i_low = s.find("0.25").unwrap();
    assert!(i_high < i_low, "rows must sort by gap_score desc:\n{}", s);
    // matches!() is always included in enum-coverage.
    assert!(s.contains("matches!"), "matches! must be included:\n{}", s);
    // Exhaustive `dispatch` site must NOT appear.
    assert!(
        !s.contains("Eof,Number,Resize,Word"),
        "exhaustive site must be hidden:\n{}",
        s
    );
}

#[test]
fn enum_coverage_lists_missing_variants() {
    ur().args(["--root", FIXTURE, "enum-coverage", "Token"])
        .assert()
        .success()
        .stdout(contains("Resize")); // the variant missing from the 3/4 site
}

#[test]
fn enum_coverage_unknown_enum_warns_and_exits_2() {
    ur().args(["--root", FIXTURE, "enum-coverage", "NotAnEnum"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("no enum `NotAnEnum`"));
}

#[test]
fn enum_coverage_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "enum-coverage", "Token"]);
}

// A partial match whose `_` arm calls a method on the scrutinee is a structural
// false positive: it's tagged, and --hide-trait-routed-catchalls drops it.
#[test]
fn enum_coverage_flags_and_hides_trait_routed_catchalls() {
    let tmp = std::env::temp_dir().join("unruster-trait-routed");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(
        tmp.join("src/main.rs"),
        "pub enum Shape { Base, Composite, Constraint, Text }\n\
         trait Paint { fn paintable_kind(&self) -> u8; }\n\
         // Real defect: partial matches! with a plain false arm.\n\
         pub fn is_path(s: &Shape) -> bool {\n\
             matches!(s, Shape::Base | Shape::Composite | Shape::Constraint)\n\
         }\n\
         // False positive: catch-all routes through a method on the scrutinee.\n\
         pub fn classify(node: &Shape) -> u8 {\n\
             match node {\n\
                 Shape::Base => 1,\n\
                 Shape::Composite => 2,\n\
                 _ => node.paintable_kind(),\n\
             }\n\
         }\n",
    )
    .unwrap();
    let root = tmp.to_str().unwrap();

    // Without the flag: both rows show; the routed one carries the tag.
    let out = ur_stdout(&["--root", root, "enum-coverage", "Shape"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("is_path"), "real defect must show:\n{}", s);
    assert!(
        s.contains("classify") && s.contains("catchall→method"),
        "trait-routed catch-all must be tagged:\n{}",
        s
    );

    // With the flag: the routed row is dropped, the real defect stays.
    let out = ur_stdout(&[
        "--root",
        root,
        "enum-coverage",
        "Shape",
        "--hide-trait-routed-catchalls",
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("is_path"), "real defect must still show:\n{}", s);
    assert!(
        !s.contains("classify"),
        "trait-routed catch-all must be hidden:\n{}",
        s
    );
}

// The enum dispatch routinely hides one pattern level down — inside the
// `Some(...)` / `Ok(...)` wrapper a lookup returns, a tuple scrutinee, or a
// `binding @ Variant` subpattern. The scanner must recurse into nested
// patterns or these sites score as "no variants" and vanish from
// enum-coverage / parallel-matches / catch-all-arms entirely (the
// `Selection::reconcile` blind spot).
#[test]
fn enum_coverage_sees_variants_nested_in_wrapper_patterns() {
    let tmp = std::env::temp_dir().join("unruster-nested-patterns");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(
        tmp.join("src/main.rs"),
        "pub enum Node { Base(u8), Composite(u8), Image, Text }\n\
         // Option-wrapped scrutinee: `match lookup(id) { Some(Node::X) => … }`.\n\
         pub fn via_option(n: Option<&Node>) -> bool {\n\
             match n {\n\
                 Some(Node::Base(_)) => true,\n\
                 Some(Node::Image) => true,\n\
                 _ => false,\n\
             }\n\
         }\n\
         // Tuple scrutinee dispatching on the enum in one position.\n\
         pub fn via_tuple(n: &Node, flag: bool) -> bool {\n\
             match (n, flag) {\n\
                 (Node::Base(_), true) => true,\n\
                 (Node::Composite(_), _) => true,\n\
                 _ => false,\n\
             }\n\
         }\n\
         // `binding @ Variant` subpattern.\n\
         pub fn via_binding(n: &Node) -> bool {\n\
             match n {\n\
                 b @ Node::Text => { let _ = b; true }\n\
                 Node::Image => true,\n\
                 _ => false,\n\
             }\n\
         }\n\
         // matches! with an Option-wrapped pattern.\n\
         pub fn via_matches(n: Option<&Node>) -> bool {\n\
             matches!(n, Some(Node::Base(_) | Node::Text))\n\
         }\n",
    )
    .unwrap();
    let root = tmp.to_str().unwrap();

    let out = ur_stdout(&["--root", root, "enum-coverage", "Node"]);
    let s = String::from_utf8_lossy(&out);
    let row = |f: &str| {
        s.lines()
            .find(|l| l.contains(f))
            .unwrap_or_else(|| panic!("expected a row for `{}`:\n{}", f, s))
            .to_string()
    };
    assert!(
        row("via_option").contains("2/4") && row("via_option").contains("Base,Image"),
        "Option-wrapped match must score its nested variants:\n{}",
        s
    );
    assert!(
        row("via_tuple").contains("Base,Composite"),
        "tuple-scrutinee match must score per-position variants:\n{}",
        s
    );
    assert!(
        row("via_binding").contains("Image,Text"),
        "`b @ Variant` subpattern must count:\n{}",
        s
    );
    assert!(
        row("via_matches").contains("Base,Text"),
        "matches! with wrapped pattern must count:\n{}",
        s
    );
}

// ─── if-chains (== / if-else-if dispatch) ───────────────────────────────────

/// The whole `if-chain` row for a given enclosing fn, or "" if absent.
fn coverage_row_for(out: &[u8], needle: &str) -> String {
    String::from_utf8_lossy(out)
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or("")
        .to_string()
}

#[test]
fn if_chain_two_arm_with_else_emitted() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let row = coverage_row_for(&out, "two_arm_with_else");
    assert!(row.contains("(if-chain)"), "expected if-chain tag:\n{}", row);
    assert!(row.contains("2/4"), "expected 2 covered variants:\n{}", row);
    assert!(row.contains("A,B"), "expected A,B covered:\n{}", row);
}

#[test]
fn if_chain_no_trailing_else_still_emitted() {
    // No catch-all `else`, but the missing variants are still missed → partial.
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let row = coverage_row_for(&out, "two_arm_no_else");
    assert!(row.contains("2/4"), "expected 2/4 site:\n{}", row);
    assert!(row.contains("(if-chain)"), "expected if-chain tag:\n{}", row);
}

#[test]
fn if_chain_three_arm_counts_all_variants() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let row = coverage_row_for(&out, "three_arm");
    assert!(row.contains("3/4"), "expected 3 covered variants:\n{}", row);
    assert!(row.contains("A,B,C"), "expected A,B,C:\n{}", row);
}

#[test]
fn if_chain_reversed_operand_order_emitted() {
    // `Mode::A == *m` (variant on the left) is detected too.
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let row = coverage_row_for(&out, "reversed");
    assert!(row.contains("2/4"), "reversed chain must emit:\n{}", row);
}

#[test]
fn if_chain_mixed_scrutinee_negated_and_single_not_emitted() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("mixed_scrutinee"), "mixed scrutinee must be skipped:\n{}", s);
    assert!(!s.contains("negated"), "`!=` chain must be skipped:\n{}", s);
    assert!(!s.contains("single_guard"), "single `if` must be skipped:\n{}", s);
}

#[test]
fn if_chain_nested_emits_outer_and_inner() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let s = String::from_utf8_lossy(&out);
    // Outer chain covers A,B; inner chain (in the first arm's body) covers C,D.
    let nested: Vec<&str> = s.lines().filter(|l| l.contains("::nested ")).collect();
    assert_eq!(nested.len(), 2, "expected outer + inner site:\n{}", s);
    assert!(nested.iter().any(|l| l.contains("A,B\t")), "outer A,B:\n{}", s);
    assert!(nested.iter().any(|l| l.contains("C,D\t")), "inner C,D:\n{}", s);
}

#[test]
fn if_chain_trait_routed_else_tagged_and_hidden() {
    // The `else { m.rank() }` arm routes through a method on the scrutinee.
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Mode"]);
    let row = coverage_row_for(&out, "trait_routed_else");
    assert!(
        row.contains("catchall→method"),
        "trait-routed else must be tagged:\n{}",
        row
    );

    let hidden = ur_stdout(&[
        "--root", FIXTURE, "enum-coverage", "Mode", "--hide-trait-routed-catchalls",
    ]);
    assert!(
        !String::from_utf8_lossy(&hidden).contains("trait_routed_else"),
        "trait-routed else must be dropped by the flag"
    );
}

#[test]
fn if_chain_vectorian_dispatcher_two_of_seventeen() {
    // Mirrors apply_static_handle_drag_to_doc's pre-fix shape: 2/17 coverage,
    // Center+Rotation covered, Start/End among the missing.
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "DragHandle"]);
    let row = coverage_row_for(&out, "apply_static_handle_drag");
    assert!(row.contains("2/17"), "expected 2/17 coverage:\n{}", row);
    assert!(row.contains("Center,Rotation"), "expected Center,Rotation:\n{}", row);
    assert!(row.contains("Start") && row.contains("End"), "Start/End missing:\n{}", row);
    assert!(row.contains("(if-chain)"), "expected if-chain tag:\n{}", row);
}

#[test]
fn parallel_matches_include_if_chains_toggle() {
    let without = ur_stdout(&["--root", FIXTURE, "parallel-matches", "Mode"]);
    assert!(
        !String::from_utf8_lossy(&without).contains("if-chain"),
        "if-chains must be off by default in parallel-matches"
    );
    let with = ur_stdout(&[
        "--root", FIXTURE, "parallel-matches", "Mode", "--include-if-chains",
    ]);
    assert!(
        String::from_utf8_lossy(&with).contains("(if-chain)"),
        "expected (if-chain) sites with --include-if-chains"
    );
}

#[test]
fn parallel_matches_include_if_chains_summary_silent() {
    assert_summary_silent_stdout(&[
        "--root", FIXTURE, "--summary", "parallel-matches", "Mode", "--include-if-chains",
    ]);
}

// ─── global flags ──────────────────────────────────────────────────────────

/// The same file must render one way, whatever `--root` was spelled as.
/// `--root .` used to yield `./src/a.rs:12` and `--root src` `src/a.rs:12` for
/// the same item, so two runs could not be diffed and a path grep had to allow
/// for both spellings.
#[test]
fn site_paths_do_not_depend_on_how_root_was_spelled() {
    let plain = ur_stdout_allow_findings(&["--root", FIXTURE, "inventory", "--top", "3"]);
    let dotted = ur_stdout_allow_findings(&["--root", &format!("./{FIXTURE}"), "inventory", "--top", "3"]);
    assert_eq!(
        String::from_utf8_lossy(&plain),
        String::from_utf8_lossy(&dotted),
        "`--root X` and `--root ./X` must render identical rows"
    );
    assert!(
        !String::from_utf8_lossy(&plain).contains("\t./"),
        "no site cell should carry a `./` prefix"
    );
}

/// `--top` is one global flag, so it has to work on every command — including
/// the enum sweeps, which emit rows while scanning and could not be capped at
/// all before the budget moved into the emitter.
#[test]
fn top_caps_every_command_and_announces_it() {
    for args in [
        vec!["error-swallows"],
        vec!["dead-code"],
        vec!["enum-coverage"],
        vec!["catch-all-arms"],
        vec!["inventory"],
        vec!["impls"],
        vec!["stringly"],
        vec!["casts"],
        vec!["metrics"],
    ] {
        let mut full = vec!["--root", FIXTURE];
        full.extend(args.iter().copied());
        full.extend(["--top", "1"]);
        let out = ur().args(&full).output().unwrap();
        let rows = rows_of(&out.stdout).len();
        assert!(rows <= 1, "{args:?} listed {rows} rows under --top 1");
        // The cut announces itself beside the rows it cut — on stdout, so a
        // `2>/dev/null` caller still learns the listing is partial.
        let said = String::from_utf8_lossy(&out.stdout);
        if rows == 1 && said.contains("showing") {
            assert!(
                said.contains("raise or drop --top"),
                "{args:?} truncated without saying so: {said}"
            );
        }
    }
}


/// `-r`/`--root` is documented under GLOBAL FLAGS and has to behave like one.
/// It was the only flag on `Cli` without `global = true`, so every
/// `unruster <cmd> -r <path>` — the order the help implies — was a hard clap
/// error.
#[test]
fn root_is_accepted_after_the_subcommand() {
    for args in [
        vec!["metrics", "--root", FIXTURE],
        vec!["metrics", "-r", FIXTURE],
        vec!["error-swallows", "-r", FIXTURE],
        vec!["stringly", "-r", FIXTURE],
    ] {
        let out = ur().args(&args).output().unwrap();
        let code = out.status.code();
        assert!(
            code == Some(0) || code == Some(1),
            "{args:?} exited {code:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `builder-drift` takes a positional constructor filter. It used to be called
/// `root`, which collided with the newly-global `--root` — and clap reports an
/// id/type mismatch by panicking at *access* time, so the subcommand built
/// fine and died on use.
#[test]
fn builder_drift_positional_does_not_collide_with_global_root() {
    ur().args(["--root", "fixtures/drift/src", "builder-drift", "Cmd::new"])
        .assert()
        .success()
        .stdout(contains("Cmd::new"));
    // And the same filter with the root supplied *after* the subcommand.
    ur().args(["builder-drift", "Cmd::new", "-r", "fixtures/drift/src"])
        .assert()
        .success()
        .stdout(contains("Cmd::new"));
}

/// The three enum views share one scan, so a consumer should not have to learn
/// three row shapes — and, more sharply, the shape must not change with the
/// *argument*. Naming an enum used to drop the `enum` column, so
/// `enum-coverage Foo` and `enum-coverage` returned rows of different widths
/// and a TSV consumer indexing by position silently read the wrong cell.
#[test]
fn enum_views_keep_one_row_shape_whether_or_not_an_enum_is_named() {
    for (cmd, enum_name) in [
        ("catch-all-arms", "Sig"),
        ("enum-coverage", "DragHandle"),
        ("parallel-matches", "Shape"),
    ] {
        let named = ur_stdout_allow_findings(&["--root", FIXTURE, cmd, enum_name]);
        let swept = ur_stdout_allow_findings(&["--root", FIXTURE, cmd]);
        let width = |b: &[u8]| -> Option<usize> {
            rows_of(b)
                .iter()
                .find(|l| !l.starts_with('#') && !l.starts_with(' '))
                .map(|l| l.split('\t').count())
        };
        let (a, b) = (width(&named), width(&swept));
        assert!(a.is_some(), "{cmd} {enum_name} produced no rows to compare");
        assert_eq!(a, b, "{cmd}: row width changes with the argument");
        // And every view leads with the enum it is talking about.
        let first = rows_of(&named)
            .into_iter()
            .find(|l| !l.starts_with('#') && !l.starts_with(' '))
            .unwrap();
        let lead = first.split('\t').next().unwrap();
        assert!(
            lead == enum_name || lead == "group",
            "{cmd}: expected the enum column to lead, got {lead:?}"
        );
    }
}

/// The blind-spot count is the tool's one statement about its own coverage, and
/// a count alone is not actionable: on a real 6k-item codebase "45 macro bodies
/// could not be parsed" left the reader unable to say which regions were dark,
/// and it talked a contributor out of an otherwise-idiomatic `macro_rules!`
/// because it would add one more unlocatable hole.
#[test]
fn blind_spots_can_be_located_not_just_counted() {
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "blind-spots"]);
    let rows = rows_of(&out);
    assert!(!rows.is_empty(), "the fixture has unparseable macro bodies");
    for line in &rows {
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 2, "macro, at: {line:?}");
        assert!(cols[0].ends_with('!'), "first column names the macro: {line:?}");
        assert!(cols[1].contains(".rs:"), "second column is a site: {line:?}");
    }
    // And the count matches what every other command reports in its note.
    let note = String::from_utf8_lossy(
        &ur().args(["--root", FIXTURE, "inventory"]).output().unwrap().stderr,
    )
    .to_string();
    let n = note
        .split("blind spots: ")
        .nth(1)
        .and_then(|t| t.split(' ').next())
        .and_then(|t| t.parse::<usize>().ok())
        .expect("every run reports a blind-spot count");
    assert_eq!(n, rows.len(), "the listing and the count must agree");
}

// ─── clones ────────────────────────────────────────────────────────────────

/// A fresh scratch tree under the system temp dir, named after the caller so
/// two tests never share one. Matches the inline pattern used elsewhere in this
/// file; extracted because the clone tests need four of them.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("unruster-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    dir
}

/// The finding the check exists for: one helper, several copies, locals
/// renamed. The fixture carries three copies of a `parse_id`-shaped body.
#[test]
fn clones_groups_copy_pasted_bodies() {
    let dir = scratch("clone-group");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a {
    pub fn parse_id(bytes: &[u8], field: &'static str) -> Result<Id, Err> {
        let trimmed = bytes.strip_prefix(b"id:").unwrap_or(bytes);
        Id::from_slice(trimmed).map_err(|_| Err::bad(field))
    }
}
pub mod b {
    pub fn parse_id(raw: &[u8], name: &'static str) -> Result<Id, Err> {
        let cut = raw.strip_prefix(b"id:").unwrap_or(raw);
        Id::from_slice(cut).map_err(|_| Err::bad(name))
    }
}
pub mod c {
    pub fn parse_id(input: &[u8], label: &'static str) -> Result<Id, Err> {
        let body = input.strip_prefix(b"id:").unwrap_or(input);
        Id::from_slice(body).map_err(|_| Err::bad(label))
    }
}
pub fn unrelated(v: &[u8]) -> usize {
    v.iter().filter(|b| **b != 0).map(|b| *b as usize).sum()
}
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "clones", "--all-stdout"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("parse_id"), "expected the group: {text}");
    assert!(
        text.contains('\t'),
        "expected a TSV row, got: {text}"
    );
    // Three copies, and the lone `unrelated` fn is not dragged in.
    let row = text.lines().find(|l| l.contains("parse_id")).unwrap();
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[2], "3", "copies column: {row}");
    assert!(!text.contains("unrelated"), "false grouping: {text}");
}

/// Renaming what a function *calls* is a different function, not a clone.
/// Without this the check degrades into a shape-similarity metric.
#[test]
fn clones_does_not_group_different_callees() {
    let dir = scratch("clone-callees");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn area(s: &Shape) -> u32 {
    let w = s.width();
    let h = s.height();
    let border = s.width() + s.height();
    w * h + border * 2 + s.width()
}
pub fn cells(t: &Table) -> u32 {
    let r = t.rows();
    let c = t.cols();
    let edge = t.rows() + t.cols();
    r * c + edge * 2 + t.rows()
}
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "clones", "--all-stdout"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("2 fn(s) scanned"),
        "both bodies must clear --min-tokens or this proves nothing: {text}"
    );
    assert!(
        text.contains("across 0 group(s)"),
        "same shape, different callees — must not group: {text}"
    );
}

/// `audit` gates on the top tier of the ranked checks. Before this, five
/// row-gating checks returning zero meant `exit 0` on a tree whose
/// `error-swallows` section held a real defect.
#[test]
fn audit_gates_on_top_tier_of_ranked_checks() {
    let dir = scratch("audit-gate-on");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
// `#[allow(dead_code)]` so the dead-code check stays silent and the exit
// code isolates the tier under test.
#[allow(dead_code)]
pub fn settle(db: &Db, id: u64) {
    // A discarded external mutation: the gating tier.
    let _ = db.query("DELETE FROM events WHERE id = $1").bind(id).execute();
}
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "audit", "--all-stdout"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a discarded DELETE must hold the loop open: {text}"
    );
    assert!(
        text.contains("exit 1 while gating findings remain"),
        "summary should say the gate is held: {text}"
    );
    // The section *header* also names the threshold, so match the summary's
    // own phrasing or this passes on every tree.
    assert!(
        text.contains("(discarded external effects"),
        "the error-swallows summary should report a gating tier: {text}"
    );
}

/// The converse: a tree whose only swallows are deliberate cause-collapsing
/// must not hold the loop open, or the gate is one nobody can clear.
#[test]
fn audit_does_not_gate_on_deliberate_sanitization() {
    let dir = scratch("audit-gate-off");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
#[allow(dead_code)]
pub fn verify(raw: &[u8]) -> Result<Id, Bad> {
    Id::from_slice(raw).map_err(|_| Bad::Malformed)
}
#[allow(dead_code)]
pub fn decode_count(text: &str) -> Result<u32, Bad> {
    text.parse::<u32>().map_err(|_| Bad::Malformed)
}
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "audit", "--all-stdout"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        out.status.code(),
        Some(0),
        "collapsing a decode cause is correct and must not gate: {text}"
    );
    assert!(
        !text.contains("(discarded external effects"),
        "no row should have reached the gating tier: {text}"
    );
}

// ─── error-swallows ────────────────────────────────────────────────────────

/// The list is long and converts at a few percent, so order is the product.
/// A discarded external mutation must outrank a deliberately-collapsed decode
/// error no matter where either sits in the file.
#[test]
fn error_swallows_ranks_discarded_effects_first() {
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "error-swallows"]);
    let scores: Vec<f64> = rows_of(&out)
        .iter()
        .filter_map(|l| l.split('\t').nth(1)?.parse().ok())
        .collect();
    assert!(scores.len() > 1, "expected several ranked rows");
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "rows are not in descending score order: {scores:?}"
    );
}

#[test]
fn error_swallows_finds_methods() {
    ur().args(["--root", FIXTURE, "error-swallows"])
        .assert()
        .success()
        .stdout(contains(".ok"))
        .stdout(contains(".unwrap_or_default"));
}

#[test]
fn error_swallows_include_unwrap_or() {
    ur().args(["--root", FIXTURE, "error-swallows", "--include-unwrap-or"])
        .assert()
        .success();
}

#[test]
fn error_swallows_finds_match_err_wild() {
    ur().args(["--root", FIXTURE, "error-swallows"])
        .assert()
        .success()
        .stdout(contains("match-err-wild"));
}

#[test]
fn error_swallows_finds_if_let_ok() {
    ur().args(["--root", FIXTURE, "error-swallows"])
        .assert()
        .success()
        .stdout(contains("if-let-ok"));
}

#[test]
fn error_swallows_finds_let_underscore() {
    ur().args(["--root", FIXTURE, "error-swallows"])
        .assert()
        .success()
        .stdout(contains("let-_"));
}

#[test]
fn error_swallows_finds_while_let_ok() {
    ur().args(["--root", FIXTURE, "error-swallows"])
        .assert()
        .success()
        .stdout(contains("while-let-ok"));
}

#[test]
fn error_swallows_finds_map_err_wildcard() {
    ur().args(["--root", FIXTURE, "error-swallows"])
        .assert()
        .success()
        .stdout(contains(".map_err"));
}

// Exercises parse_dir's read-failure / parse-failure error paths.
#[test]
fn parse_failure_surfaces_in_summary() {
    let tmp = std::env::temp_dir().join("unruster-parse-fail");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/main.rs"), "fn x() { unclosed").unwrap();
    let out = ur()
        .args(["--root", tmp.to_str().unwrap(), "inventory"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parse failed") || stderr.contains("1 parse errors"),
        "expected parse-failure warning, got:\n{}",
        stderr
    );
}

// Exercises NameIndex glob-import resolution path in semantic.rs.
#[test]
fn type_refs_via_glob_import() {
    let tmp = std::env::temp_dir().join("unruster-glob-import");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(
        tmp.join("src/main.rs"),
        "pub mod m { pub struct Thing; }\n\
         use crate::m::*;\n\
         fn use_thing() { let _: Thing; }\n\
         fn main() { use_thing(); }",
    )
    .unwrap();
    ur().args(["--root", tmp.to_str().unwrap(), "type-refs", "Thing"])
        .assert()
        .success();
}

#[test]
fn callers_by_module_groups() {
    ur().args(["--root", FIXTURE, "callers", "--by", "module", "Document::new"])
        .assert()
        .success();
}

#[test]
fn callers_dot_method_form() {
    ur().args(["--root", FIXTURE, "callers", ".touch"])
        .assert()
        .success();
}

#[test]
fn callers_double_colon_form_skips_methods() {
    ur().args(["--root", FIXTURE, "callers", "::new"])
        .assert()
        .success();
}

#[test]
fn callees_summary_mode() {
    ur().args(["--root", FIXTURE, "--summary", "callees", "main"])
        .assert()
        .success();
}

#[test]
fn variants_summary_mode() {
    ur().args(["--root", FIXTURE, "--summary", "variants", "Token"])
        .assert()
        .success();
}

#[test]
fn fields_summary_mode() {
    ur().args(["--root", FIXTURE, "--summary", "fields", "Document"])
        .assert()
        .success();
}

#[test]
fn impls_summary_mode() {
    ur().args(["--root", FIXTURE, "--summary", "impls"])
        .assert()
        .success();
}

#[test]
fn type_refs_summary_mode() {
    ur().args(["--root", FIXTURE, "--summary", "type-refs", "Document"])
        .assert()
        .success();
}

#[test]
fn metrics_invalid_sort_rejected_by_clap() {
    ur().args(["--root", FIXTURE, "metrics", "--sort", "bogus"])
        .assert()
        .failure()
        .stderr(contains("invalid value 'bogus'"));
}

#[test]
fn callers_unknown_symbol_emits_note() {
    let out = ur()
        .args(["--root", FIXTURE, "callers", "nonexistent_xyz"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not defined") || stderr.contains("0 call"));
}

#[test]
fn variants_unknown_enum_warns_and_exits_2() {
    ur().args(["--root", FIXTURE, "variants", "NotAnEnum"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("no enum `NotAnEnum`"));
}

#[test]
fn catch_all_unknown_enum_warns_and_exits_2() {
    ur().args(["--root", FIXTURE, "catch-all-arms", "NotAnEnum"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("no enum `NotAnEnum`"));
}

#[test]
fn parallel_matches_unknown_enum_warns_and_exits_2() {
    ur().args(["--root", FIXTURE, "parallel-matches", "NotAnEnum"])
        .assert()
        .failure()
        .code(2)
        .stdout(predicates::str::contains("no enum `NotAnEnum`"));
}

#[test]
fn dead_code_scope_all() {
    ur().args(["--root", FIXTURE, "--scope", "all", "dead-code"])
        .assert()
        .success();
}

// ─── pass-through ──────────────────────────────────────────────────────────

#[test]
fn pass_through_finds_wrapper() {
    ur().args(["--root", FIXTURE, "pass-through"])
        .assert()
        .success()
        .stdout(contains("wrapper"));
}

// ─── casts ─────────────────────────────────────────────────────────────────

#[test]
fn casts_finds_narrowing() {
    ur().args(["--root", FIXTURE, "casts"])
        .assert()
        .success()
        .stdout(contains("narrow-int"));
}

#[test]
fn casts_class_filter() {
    ur().args(["--root", FIXTURE, "casts", "--class", "narrow-int"])
        .assert()
        .success()
        .stdout(contains("narrow-int"));
}

#[test]
fn casts_by_fn_groups() {
    ur().args(["--root", FIXTURE, "casts", "--by", "fn"])
        .assert()
        .success();
}

#[test]
fn casts_no_widen() {
    ur().args(["--root", FIXTURE, "casts", "--hide-widen"])
        .assert()
        .success();
}

#[test]
fn casts_class_signed_flip_and_narrow_float() {
    ur().args(["--root", FIXTURE, "casts"])
        .assert()
        .success()
        .stdout(contains("signed-flip"))
        .stdout(contains("narrow-float"))
        .stdout(contains("ptr"));
}

#[test]
fn casts_by_file_groups() {
    ur().args(["--root", FIXTURE, "casts", "--by", "file"])
        .assert()
        .success();
}

#[test]
fn casts_by_module_groups() {
    ur().args(["--root", FIXTURE, "casts", "--by", "module"])
        .assert()
        .success();
}

// ─── conversions / conversion-pairs ────────────────────────────────────────

#[test]
fn conversions_finds_methods() {
    ur().args(["--root", FIXTURE, "conversions"])
        .assert()
        .success()
        .stdout(contains(".to_string"));
}

#[test]
fn conversions_by_fn_top() {
    ur().args(["--root", FIXTURE, "conversions", "--by", "fn", "--top", "5"])
        .assert()
        .success();
}

#[test]
fn conversions_by_file_top() {
    ur().args(["--root", FIXTURE, "conversions", "--by", "file", "--top", "3"])
        .assert()
        .success();
}

#[test]
fn conversions_by_module_top() {
    ur().args(["--root", FIXTURE, "conversions", "--by", "module"])
        .assert()
        .success();
}

#[test]
fn conversions_kind_filter() {
    ur().args(["--root", FIXTURE, "conversions", "--kind", ".to_string,.into"])
        .assert()
        .success();
}

#[test]
fn conversion_pairs_finds_bidirectional() {
    ur().args(["--root", FIXTURE, "conversion-pairs"])
        .assert()
        .success()
        .stdout(contains("Document"))
        .stdout(contains("LegacyDoc"));
}

// ─── stringly ──────────────────────────────────────────────────────────────

#[test]
fn stringly_default_finds_cmp_and_match() {
    ur().args(["--root", FIXTURE, "stringly"])
        .assert()
        .success()
        .stdout(contains("cmp-eq"))
        .stdout(contains("match-lit"));
}

#[test]
fn stringly_include_substring() {
    ur().args(["--root", FIXTURE, "stringly", "--include-substring"])
        .assert()
        .success();
}

#[test]
fn stringly_by_fn() {
    ur().args(["--root", FIXTURE, "stringly", "--by", "fn"])
        .assert()
        .success();
}

#[test]
fn stringly_include_map_keys() {
    ur().args(["--root", FIXTURE, "stringly", "--include-map-keys"])
        .assert()
        .success()
        .stdout(contains("map-lit-key"));
}

// (Was `stringly_substr_via_starts_with` — exact duplicate of
// `stringly_include_substring`. Removed.)

#[test]
fn stringly_by_file_groups() {
    ur().args(["--root", FIXTURE, "stringly", "--by", "file"])
        .assert()
        .success();
}

#[test]
fn stringly_by_module_groups() {
    ur().args(["--root", FIXTURE, "stringly", "--by", "module"])
        .assert()
        .success();
}

// ─── scope / cfg / summary ─────────────────────────────────────────────────

#[test]
fn scope_all_includes_tests_module() {
    ur().args(["--root", FIXTURE, "--scope", "all", "inventory", "--kind", "fn"])
        .assert()
        .success()
        .stdout(contains("it_runs"));
}

#[test]
fn scope_production_excludes_tests_module() {
    let out = ur()
        .args([
            "--root",
            FIXTURE,
            "--scope",
            "production",
            "inventory",
            "--kind",
            "fn",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("it_runs"));
}

#[test]
fn scope_tests_includes_test_module() {
    ur().args([
        "--root",
        FIXTURE,
        "--scope",
        "tests",
        "inventory",
        "--kind",
        "fn",
    ])
    .assert()
    .success();
}

#[test]
fn cfg_flag_accepted() {
    ur().args([
        "--root",
        FIXTURE,
        "--cfg",
        "feature=test",
        "inventory",
    ])
    .assert()
    .success();
}

#[test]
fn cfg_feature_gpu_keeps_gpu_only() {
    ur().args([
        "--root",
        FIXTURE,
        "--cfg",
        "feature=gpu",
        "inventory",
        "--kind",
        "fn",
    ])
    .assert()
    .success()
    .stdout(contains("gpu_only"));
}

#[test]
fn cfg_feature_gpu_strips_cpu_only() {
    let out = ur()
        .args([
            "--root",
            FIXTURE,
            "--cfg",
            "feature=gpu",
            "inventory",
            "--kind",
            "fn",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("cpu_only"), "cpu_only should be stripped, got:\n{}", s);
}

#[test]
fn cfg_default_keeps_both_unknown_features() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "fn"])
        .assert()
        .success()
        .stdout(contains("gpu_only"))
        .stdout(contains("cpu_only"));
}

#[test]
fn cfg_multi_flags_unix_macos() {
    ur().args([
        "--root",
        FIXTURE,
        "--cfg",
        "unix",
        "--cfg",
        "target_os=macos",
        "inventory",
        "--kind",
        "fn",
    ])
    .assert()
    .success()
    .stdout(contains("macos_only"));
}

#[test]
fn cfg_any_keeps_with_gpu() {
    ur().args([
        "--root",
        FIXTURE,
        "--cfg",
        "feature=gpu",
        "inventory",
        "--kind",
        "fn",
    ])
    .assert()
    .success()
    .stdout(contains("any_gfx_backend"));
}

#[test]
fn cfg_any_keeps_with_metal_too() {
    ur().args([
        "--root",
        FIXTURE,
        "--cfg",
        "feature=metal",
        "inventory",
        "--kind",
        "fn",
    ])
    .assert()
    .success()
    .stdout(contains("any_gfx_backend"));
}

#[test]
fn cfg_not_inverts() {
    let out = ur()
        .args([
            "--root",
            FIXTURE,
            "--cfg",
            "feature=no_color",
            "inventory",
            "--kind",
            "fn",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("with_color"), "with_color should be stripped under --cfg feature=no_color");
}

#[test]
fn cfg_quoted_value_parses() {
    // `--cfg feature="gpu"` (with quotes) should behave the same as bare.
    ur().args([
        "--root",
        FIXTURE,
        "--cfg",
        "feature=\"gpu\"",
        "inventory",
        "--kind",
        "fn",
    ])
    .assert()
    .success()
    .stdout(contains("gpu_only"));
}

#[test]
fn cfg_multi_flags_not_macos_strips() {
    let out = ur()
        .args([
            "--root",
            FIXTURE,
            "--cfg",
            "unix",
            "--cfg",
            "target_os=linux",
            "inventory",
            "--kind",
            "fn",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("macos_only"));
}

#[test]
fn cfg_invalid_scope_errors() {
    ur().args(["--root", FIXTURE, "--scope", "bogus", "inventory"])
        .assert()
        .failure();
}

#[test]
fn summary_suppresses_rows() {
    let out = ur()
        .args(["--root", FIXTURE, "--summary", "inventory"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Summary mode suppresses per-row stdout; nothing on stdout, summary on stderr.
    assert!(stdout.trim().is_empty(), "summary should suppress stdout, got:\n{}", stdout);
}

// ════════════════════════════════════════════════════════════════════════════
//  --summary parity tests: every subcommand must silence stdout under --summary.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn inventory_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "inventory"]);
}

#[test]
fn callers_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "callers", "Document::new"]);
}

#[test]
fn field_uses_summary_mode() {
    assert_summary_silent_stdout(&[
        "--root", FIXTURE, "--summary", "field-uses", "Document", "transform",
    ]);
}

#[test]
fn takes_mut_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "takes-mut", "Document"]);
}

#[test]
fn metrics_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "metrics"]);
}

#[test]
fn dead_code_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "dead-code"]);
}

#[test]
fn catch_all_arms_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "catch-all-arms", "Token"]);
}

#[test]
fn parallel_matches_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "parallel-matches", "Token"]);
}

#[test]
fn error_swallows_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "error-swallows"]);
}

#[test]
fn pass_through_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "pass-through"]);
}

#[test]
fn casts_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "casts"]);
}

#[test]
fn conversions_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "conversions"]);
}

#[test]
fn conversion_pairs_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "conversion-pairs"]);
}

#[test]
fn stringly_summary_mode() {
    assert_summary_silent_stdout(&["--root", FIXTURE, "--summary", "stringly"]);
}

// ════════════════════════════════════════════════════════════════════════════
//  inventory --vis and --kind: cover all values, not just the most common.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn inventory_vis_crate() {
    ur().args(["--root", FIXTURE, "inventory", "--vis", "crate"])
        .assert()
        .success();
}

#[test]
fn inventory_vis_priv() {
    ur().args(["--root", FIXTURE, "inventory", "--vis", "priv"])
        .assert()
        .success();
}

#[test]
fn inventory_vis_unknown_rejected_by_clap() {
    ur().args(["--root", FIXTURE, "inventory", "--vis", "bogus"])
        .assert()
        .failure()
        .stderr(contains("invalid value 'bogus'"));
}

#[test]
fn inventory_kind_trait() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "trait"])
        .assert()
        .success()
        .stdout(contains("Render"));
}

#[test]
fn inventory_kind_impl() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "impl"])
        .assert()
        .success();
}

#[test]
fn inventory_kind_mod() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "mod"])
        .assert()
        .success()
        .stdout(contains("inner"));
}

#[test]
fn inventory_kind_const() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "const"])
        .assert()
        .success();
}

#[test]
fn inventory_kind_static() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "static"])
        .assert()
        .success();
}

#[test]
fn inventory_kind_type() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "type"])
        .assert()
        .success()
        .stdout(contains("Doc"));
}

#[test]
fn inventory_kind_trait_fn() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "trait-fn"])
        .assert()
        .success();
}

#[test]
fn inventory_kind_impl_fn() {
    ur().args(["--root", FIXTURE, "inventory", "--kind", "impl-fn"])
        .assert()
        .success();
}

#[test]
fn inventory_tree_with_vis() {
    // Cross-flag combo: tree + vis. Catches per-flag composition regressions.
    ur().args(["--root", FIXTURE, "inventory", "--tree", "--vis", "pub"])
        .assert()
        .success()
        .stdout(contains("crate"));
}

// ════════════════════════════════════════════════════════════════════════════
//  field-uses kind filters: all three should be tested, not just --kind write.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn field_uses_reads_only_filter() {
    // Only the read rows should appear; writes/inits filtered out.
    let out = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "Document", "transform", "--kind", "read",
    ]);
    for line in rows_of(&out) {
        let first_col = line.split('\t').next().unwrap_or("");
        assert_eq!(first_col, "read", "non-read row leaked through: {:?}", line);
    }
}

#[test]
fn field_uses_inits_only_filter() {
    let out = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "Document", "transform", "--kind", "init",
    ]);
    for line in rows_of(&out) {
        let first_col = line.split('\t').next().unwrap_or("");
        assert_eq!(first_col, "init", "non-init row leaked through: {:?}", line);
    }
}

#[test]
fn field_uses_unknown_type_no_results_exits_2() {
    // Querying a non-existent type: zero data rows, an explanation, exit 2.
    let out = ur()
        .args(["--root", FIXTURE, "field-uses", "NoSuchType", "no_field"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.lines().any(|l| l.contains('\t')),
        "expected no data rows for an unknown type:\n{}",
        s
    );
    assert!(s.contains("NoSuchType"), "no explanation printed:\n{}", s);
    assert_eq!(out.status.code(), Some(2));
}

// ════════════════════════════════════════════════════════════════════════════
//  metrics: --sort params and --top behavior.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn metrics_sort_params() {
    ur().args(["--root", FIXTURE, "metrics", "--sort", "params"])
        .assert()
        .success()
        .stdout(contains("params:"));
}

#[test]
fn metrics_top_truncates() {
    // --top 1 should yield at most 1 fn row + at most 1 struct row + at most 1 enum row.
    let out = ur_stdout(&["--root", FIXTURE, "metrics", "--top", "1"]);
    let fn_rows = rows_of(&out).into_iter().filter(|l| l.starts_with("fn\t")).count();
    let struct_rows = rows_of(&out).into_iter().filter(|l| l.starts_with("struct\t")).count();
    let enum_rows = rows_of(&out).into_iter().filter(|l| l.starts_with("enum\t")).count();
    assert!(fn_rows <= 1, "fn rows {} > 1", fn_rows);
    assert!(struct_rows <= 1, "struct rows {} > 1", struct_rows);
    assert!(enum_rows <= 1, "enum rows {} > 1", enum_rows);
}

// ════════════════════════════════════════════════════════════════════════════
//  Unknown-input warnings for commands that take a name argument.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn fields_unknown_type_warns_and_exits_2() {
    let out = ur()
        .args(["--root", FIXTURE, "fields", "NoSuchStruct"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("no struct with named fields `NoSuchStruct`"),
        "expected unknown-struct warning, got:\n{}",
        stderr
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn impls_unknown_of_no_results_but_success() {
    let out = ur_stdout(&["--root", FIXTURE, "impls", "--of", "NoSuchType"]);
    assert!(rows_of(&out).is_empty());
}

#[test]
fn impls_unknown_trait_no_results_but_success() {
    let out = ur_stdout(&["--root", FIXTURE, "impls", "--trait", "NoSuchTrait"]);
    assert!(rows_of(&out).is_empty());
}

// ════════════════════════════════════════════════════════════════════════════
//  Output-shape assertions (catches row-count or column-shuffle regressions).
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn inventory_kind_struct_row_shape() {
    // Five tab-separated columns: kind, vis, loc, name, file:line — the same
    // five `outline` emits, so one parser reads both listings.
    let out = ur_stdout(&["--root", FIXTURE, "inventory", "--kind", "struct"]);
    assert!(!rows_of(&out).is_empty(), "expected at least one struct row");
    assert_tsv_cols(&out, 5);
}

#[test]
fn fields_row_shape() {
    // Every row: vis, name, type, r:N, w:M, i:K, file:line  → 7 cols.
    let out = ur_stdout(&["--root", FIXTURE, "fields", "Document"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 7);
}

#[test]
fn variants_def_row_shape() {
    // def rows: "def", "Enum::Variant", shape, file:line → 4 cols.
    let out = ur_stdout(&["--root", FIXTURE, "variants", "Token"]);
    let def_rows: Vec<_> = rows_of(&out).into_iter().filter(|l| l.starts_with("def\t")).collect();
    assert!(!def_rows.is_empty());
    for line in def_rows {
        assert_eq!(line.split('\t').count(), 4, "def row col-count drift: {:?}", line);
    }
}

#[test]
fn casts_row_shape() {
    // class, src, dst, context, file:line → 5 cols.
    let out = ur_stdout(&["--root", FIXTURE, "casts"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 5);
}

#[test]
fn conversions_row_shape() {
    // kind, target, context, file:line → 4 cols.
    let out = ur_stdout(&["--root", FIXTURE, "conversions"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 4);
}

#[test]
fn stringly_row_shape() {
    // class, literal, context, file:line → 4 cols.
    let out = ur_stdout(&["--root", FIXTURE, "stringly"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 4);
}

#[test]
fn casts_class_filter_excludes_others() {
    // Filter to narrow-int — output must have only "narrow-int" in class column.
    let out = ur_stdout(&["--root", FIXTURE, "casts", "--class", "narrow-int"]);
    for line in rows_of(&out) {
        let c = line.split('\t').next().unwrap_or("");
        assert_eq!(c, "narrow-int", "non-narrow-int class leaked: {:?}", line);
    }
}

#[test]
fn casts_no_widen_excludes_widen_classes() {
    let out = ur_stdout(&["--root", FIXTURE, "casts", "--hide-widen"]);
    for line in rows_of(&out) {
        let c = line.split('\t').next().unwrap_or("");
        assert!(c != "widen-int" && c != "widen-float", "widening leaked: {:?}", line);
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  Playbook chains: compose the workflows documented in --help long_about.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn playbook_extract_trait_audit() {
    // From --help: "EXTRACT A TRAIT" workflow uses takes-mut + type-refs +
    // callers + inventory. Each must produce evidence for Document.
    let mut_takers = ur_stdout(&["--root", FIXTURE, "takes-mut", "Document"]);
    assert!(!rows_of(&mut_takers).is_empty(), "no &mut Document takers");

    let refs = ur_stdout(&["--root", FIXTURE, "type-refs", "Document"]);
    assert!(!rows_of(&refs).is_empty(), "no Document type refs");

    let methods = ur_stdout(&[
        "--root", FIXTURE, "inventory", "--kind", "impl-fn",
    ]);
    let doc_methods: Vec<_> = rows_of(&methods)
        .into_iter()
        .filter(|l| l.contains("Document::"))
        .collect();
    assert!(!doc_methods.is_empty(), "no Document methods in inventory");
}

#[test]
fn playbook_match_to_polymorphism() {
    // From --help: "REPLACE ENUM-MATCH WITH POLYMORPHISM" — parallel-matches
    // should surface ≥2 match sites covering the same variant set.
    let out = ur_stdout(&["--root", FIXTURE, "parallel-matches", "Token"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("2 site(s)") || s.contains("3 site(s)") || s.contains("4 site(s)"),
        "expected at least one group with ≥2 sites, got:\n{}",
        s
    );
}

#[test]
fn playbook_pub_surface_audit() {
    // From --help: "SHRINK A PUB SURFACE" — inventory --vis pub + dead-code --pub-only.
    let inv = ur_stdout(&["--root", FIXTURE, "inventory", "--vis", "pub", "--kind", "fn"]);
    assert!(!rows_of(&inv).is_empty(), "no pub fns in inventory");

    // dead-code may legitimately find 0 (clean tree) — just assert it ran.
    ur().args(["--root", FIXTURE, "--scope", "all", "dead-code", "--pub-only"])
        .assert()
        .success();
}

// ════════════════════════════════════════════════════════════════════════════
//  `tests` subcommand itself.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn tests_lists_test_fns() {
    // Self-referential: against the unruster root, must find the fixture's
    // `#[test] fn it_runs` and direct test attrs.
    ur().args(["--root", "fixtures/sample", "tests"])
        .assert()
        .success()
        .stdout(contains("it_runs"));
}

#[test]
fn tests_with_hint_includes_args() {
    // Against unruster's own tests dir, hints should expose the args fingerprint.
    ur().args(["--root", ".", "tests", "--with-hint"])
        .assert()
        .success()
        .stdout(contains("inventory"));
}

#[test]
fn tests_by_subcommand_groups() {
    // Histogram should mention inventory (heavily tested subcommand).
    ur().args(["--root", ".", "tests", "--by-subcommand"])
        .assert()
        .success()
        .stdout(contains("inventory"));
}

#[test]
fn tests_summary_mode() {
    assert_summary_silent_stdout(&["--root", ".", "--summary", "tests"]);
}

#[test]
fn tests_subcommand_names_the_tests_the_histogram_only_counted() {
    // `--by-subcommand` says `8  impls`; this says which eight. Without it the
    // only route from the count to the tests was to grep the test file for the
    // subcommand string — the locate-by-guessing this tool exists to end.
    let out = ur_stdout(&["--root", ".", "tests", "--subcommand", "impls"]);
    let rows = rows_of(&out);
    assert!(!rows.is_empty(), "expected the impls tests");
    for r in &rows {
        assert!(r.contains("impls_"), "not an impls test: {}", r);
    }
    assert_tsv_cols(&out, 3);
}

#[test]
fn tests_subcommand_count_agrees_with_the_histogram() {
    // The drill-in and the overview must not be able to disagree — a listing
    // that quietly dropped a row would be worse than no listing.
    let hist = ur_stdout(&["--root", ".", "tests", "--by-subcommand"]);
    let counted: usize = rows_of(&hist)
        .iter()
        .find(|l| l.ends_with("\timpls"))
        .and_then(|l| l.split('\t').next())
        .and_then(|n| n.parse().ok())
        .expect("expected an `impls` row in the histogram");
    let listed = rows_of(&ur_stdout(&["--root", ".", "tests", "--subcommand", "impls"])).len();
    assert_eq!(counted, listed);
}

#[test]
fn tests_subcommand_none_lists_the_undetected_bucket() {
    // The histogram reports these as `<no detectable subcommand>`, which is not
    // a string anyone would type at a shell.
    let out = ur_stdout(&["--root", FIXTURE, "tests", "--subcommand", "none"]);
    assert!(!rows_of(&out).is_empty());
}

#[test]
fn tests_subcommand_composes_with_with_hint() {
    let out = ur_stdout(&["--root", ".", "tests", "--subcommand", "outline", "--with-hint"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 4);
}

#[test]
fn tests_subcommand_typo_suggests_the_real_one() {
    ur().args(["--root", ".", "tests", "--subcommand", "impl"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("Did you mean"))
        .stderr(contains("impls"));
}

#[test]
fn tests_subcommand_with_no_coverage_lists_what_is_covered() {
    // Distinct from a typo: the name may be a real subcommand that simply has
    // no test. An empty listing could not tell the two apart.
    ur().args(["--root", ".", "tests", "--subcommand", "zzzznotasubcommand"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("Subcommands with tests:"));
}

#[test]
fn tests_subcommand_conflicts_with_by() {
    ur().args(["--root", ".", "tests", "--by-subcommand", "--subcommand", "impls"])
        .assert()
        .failure();
}

#[test]
fn tests_range_is_a_real_site_so_context_can_find_it() {
    // The range column was a hand-built `format!("{}:{}-{}")` string, which
    // meant `--context` could not locate the row and silently printed nothing.
    ur().args(["--root", ".", "--context", "1", "tests", "--subcommand", "impls"])
        .assert()
        .success()
        .stdout(contains(">"));
}

#[test]
fn tests_json_carries_file_line_and_end_line_not_one_opaque_string() {
    let out = ur_stdout(&["--root", FIXTURE, "--json", "tests"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\"end_line\""), "{}", s);
    assert!(!s.contains("\"range\""), "range is still a string:\n{}", s);
}

#[test]
fn a_tests_fingerprint_survives_an_edit_above_it() {
    // The documented contract for every fingerprint: "findings key on a
    // line-number-free fingerprint, so an edit above one doesn't make it look
    // new". `tests` violated it — its range column was a `Val::Str`, so the
    // line numbers went straight into the hash.
    let tmp = std::env::temp_dir().join("unruster_tests_fp_stability");
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let write = |body: &str| std::fs::write(src.join("lib.rs"), body).unwrap();
    let fp = || -> String {
        let out = ur_stdout(&[
            "--root",
            tmp.to_str().unwrap(),
            "--scope",
            "all",
            "--fingerprints",
            "tests",
        ]);
        rows_of(&out)
            .first()
            .and_then(|l| l.split('\t').next_back())
            .expect("expected one test row")
            .to_string()
    };
    write("#[test]\nfn t() { let _ = 1; }\n");
    let before = fp();
    write("\n\n\n#[test]\nfn t() { let _ = 1; }\n");
    assert_eq!(before, fp(), "fingerprint moved with the line number");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn tests_row_shape_default() {
    // Default rows: attr, file:start-end, qpath  → 3 cols.
    let out = ur_stdout(&["--root", "fixtures/sample", "tests"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 3);
}

#[test]
fn tests_row_shape_with_hint() {
    // With-hint rows: attr, file:start-end, qpath, hint  → 4 cols.
    let out = ur_stdout(&["--root", "fixtures/sample", "tests", "--with-hint"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 4);
}

#[test]
fn playbook_field_bleed_audit() {
    // From --help: "PRIVATIZE A FIELD" — fields + field-uses --candidates.
    let f = ur_stdout(&["--root", FIXTURE, "fields", "Document"]);
    assert!(!rows_of(&f).is_empty(), "no Document fields");

    let cand = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "Document", "transform", "--candidates",
    ]);
    // At least one strict-confirmed and one inferred or candidate hit.
    assert!(!rows_of(&cand).is_empty(), "no candidate field uses");
}

// ════════════════════════════════════════════════════════════════════════════
//  Agent-loop surface: exit codes, --all, sealed, --spans, explain, audit,
//  --exclude, --min-confidence, --changed-since, --context, blind spots.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn fail_on_findings_exits_1_when_findings() {
    ur().args(["--root", FIXTURE, "--fail-on-findings", "error-swallows"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn fail_on_findings_exits_0_when_clean() {
    // SealedGear has no `match` wildcard arms (only a matches! site, which
    // catch-all-arms doesn't scan) → zero findings → exit 0.
    ur().args(["--root", FIXTURE, "--fail-on-findings", "catch-all-arms", "SealedGear"])
        .assert()
        .success();
}

#[test]
fn exclude_glob_drops_files() {
    // Excluding the whole fixture src leaves nothing to scan.
    let out = ur()
        .args(["--root", FIXTURE, "--exclude", "src/**", "inventory"])
        .output()
        .unwrap();
    assert!(rows_of(&out.stdout).is_empty(), "expected no rows with src/** excluded");
}

#[test]
fn enum_coverage_all_scans_every_enum() {
    ur().args(["--root", FIXTURE, "enum-coverage", "--all"])
        .assert()
        .success()
        .stdout(contains("SealedGear"))
        .stdout(contains("Token"));
}

#[test]
fn enum_coverage_all_conflicts_with_name() {
    ur().args(["--root", FIXTURE, "enum-coverage", "Token", "--all"])
        .assert()
        .failure();
}

#[test]
fn catch_all_arms_all_prefixes_enum_column() {
    let out = ur_stdout(&["--root", FIXTURE, "catch-all-arms", "--all"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().any(|l| l.starts_with("Token\t")),
        "expected enum-name column in --all rows:\n{}",
        s
    );
}

#[test]
fn parallel_matches_all_mode() {
    ur().args(["--root", FIXTURE, "parallel-matches", "--all"])
        .assert()
        .success()
        .stdout(contains("group"));
}

#[test]
fn sealed_enum_partial_site_tagged() {
    ur().args(["--root", FIXTURE, "enum-coverage", "SealedGear"])
        .assert()
        .success()
        .stdout(contains("SEALED"))
        .stdout(contains("gear_is_moving"));
}

#[test]
fn spans_flag_adds_fn_ranges() {
    let out = ur_stdout(&["--root", FIXTURE, "--spans", "error-swallows"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().any(|l| l.contains('@') && l.contains('-')),
        "expected @start-end spans in context labels:\n{}",
        s
    );
}

#[test]
fn explain_prints_one_topic() {
    ur().args(["explain", "stringly"])
        .assert()
        .success()
        .stdout(contains("STRINGLY-TYPED CODE"));
}

#[test]
fn explain_lists_topics_without_arg() {
    ur().args(["explain"])
        .assert()
        .success()
        .stdout(contains("PARTIAL-ENUMERATION"));
}

#[test]
fn explain_unknown_topic_exits_2() {
    ur().args(["explain", "nosuchtopiczzz"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn audit_runs_all_sections_and_exits_1_on_findings() {
    let out = ur()
        .args(["--root", FIXTURE, "audit"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "fixtures have findings → exit 1");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("## [high]"), "expected severity section headers:\n{}", s);
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("(audit:"), "expected audit summary:\n{}", e);
}

#[test]
fn audit_summary_mode_silent_stdout() {
    let out = ur()
        .args(["--root", FIXTURE, "--summary", "audit"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.trim().is_empty(), "expected --summary to silence stdout:\n{}", s);
}

#[test]
fn callers_rows_carry_confidence_column() {
    let out = ur_stdout(&["--root", FIXTURE, "callers", "mark_pending"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().all(|l| l.contains("\tresolved\t") || l.contains("\theuristic\t")),
        "every callers row should carry a confidence column:\n{}",
        s
    );
}

#[test]
fn field_uses_min_confidence_exact_drops_inferred() {
    // Document.transform has 1 ti (inferred) hit; exact-only must drop it.
    let all = ur_stdout(&["--root", FIXTURE, "field-uses", "Document", "transform"]);
    let exact = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "Document", "transform",
        "--min-confidence", "exact",
    ]);
    assert!(
        rows_of(&exact).len() < rows_of(&all).len(),
        "exact filter should drop the type-inferred row"
    );
}

#[test]
fn changed_since_invalid_ref_exits_2() {
    ur().args(["--root", FIXTURE, "--changed-since", "no-such-ref-zzz", "dead-code"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("git"));
}

#[test]
fn changed_since_head_runs() {
    ur().args(["--root", FIXTURE, "--changed-since", "HEAD", "dead-code"])
        .assert()
        .success();
}

/// A committed two-file tree, each file carrying one waiver that hits, with
/// only `touched.rs` dirty. The shape both `--changed-since` waiver bugs need:
/// one waiver inside the diff and one outside it, and both genuinely live.
fn scoped_waiver_repo(name: &str) -> std::path::PathBuf {
    let dir = scratch(name);
    let body = |fname: &str| {
        format!(
            "pub fn {fname}(p: &std::path::Path) {{\n    \
             let _ = std::fs::remove_file(p); // unruster: ok(error-swallows/let-_) \
             2026-01-01 — absence is fine\n}}\n"
        )
    };
    std::fs::write(dir.join("src/kept.rs"), body("kept")).unwrap();
    std::fs::write(dir.join("src/touched.rs"), body("touched")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub mod kept;\npub mod touched;\n").unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit", "-qm", "init",
    ]);
    // Dirty exactly one file, without disturbing its waiver.
    std::fs::write(
        dir.join("src/touched.rs"),
        format!("{}\npub const EDIT: u8 = 1;\n", body("touched")),
    )
    .unwrap();
    dir
}

#[test]
fn a_usage_question_says_when_the_default_scope_walked_past_the_tests() {
    // `--scope production` is right for the checks and wrong for "who uses
    // this", which is the question asked immediately before a signature
    // changes. A real session widening a struct by one field found its
    // construction sites with `grep -rn "Options {" src/ tests/`; eight of
    // ~fourteen were in `tests/`. The AST answer would have been better in
    // every way except that by default it would not have looked there — and
    // would have said so nowhere.
    let dir = scratch("scope-gap");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub struct Cfg { pub a: u8 }\n").unwrap();
    std::fs::write(
        dir.join("tests/it.rs"),
        "#[test]\nfn t() { let _ = demo::Cfg { a: 1 }; }\n",
    )
    .unwrap();
    let root = dir.to_str().unwrap();

    let err = |args: &[&str]| -> String {
        let mut full = vec!["--root", root];
        full.extend(args);
        String::from_utf8_lossy(&ur().args(&full).output().unwrap().stderr).into_owned()
    };
    let usage = err(&["type-refs", "Cfg"]);
    assert!(
        usage.contains("test file(s) were not scanned") && usage.contains("--scope all"),
        "a usage question answered production-only in silence:\n{}",
        usage
    );
    // Asking for the wider scope is not then told it is missing something.
    assert!(
        !err(&["type-refs", "Cfg", "--scope", "all"]).contains("were not scanned"),
        "nagged a caller who had already widened"
    );
    // A catalogue is not a usage question: `--scope` narrowing the catalogue is
    // the flag doing its job, and saying so on every listing is the noise that
    // got the blind-spot line gated in the first place.
    assert!(
        !err(&["inventory"]).contains("were not scanned"),
        "a catalogue does not need the warning"
    );
}

#[test]
fn changed_since_asks_the_root_s_repository_and_not_the_cwd_s() {
    // The scope was computed by running git in the process CWD, so
    // `unruster -r <another checkout> --changed-since HEAD` diffed *this* repo
    // and matched nothing under `--root`: every check dropped every row and the
    // run reported clean over a tree it had not looked at. Same shape as the
    // empty-`files` guard in `main`, one layer down and without the error.
    let dir = scoped_waiver_repo("changed-since-root");
    let rows = rows_of(&ur_stdout_allow_findings(&[
        "--root",
        dir.to_str().unwrap(),
        "--changed-since",
        "HEAD",
        "--no-suppress",
        "error-swallows",
    ]));
    assert!(
        rows.iter().any(|r| r.contains("touched.rs")),
        "the changed file's items went missing:\n{:?}",
        rows
    );
    assert!(
        !rows.iter().any(|r| r.contains("kept.rs")),
        "the unchanged file was not scoped out:\n{:?}",
        rows
    );
}

#[test]
fn conversion_pairs_honours_changed_since_like_every_other_gating_check() {
    // It was the one check in the battery that never called `retain_changed`,
    // and it gates — so `audit --changed-since HEAD` exited 1 over a pair in a
    // file the caller had not touched, and the documented
    // `until unruster --fail-on-findings audit` loop could not go green no
    // matter what the caller fixed.
    let dir = scratch("conv-pairs-scope");
    std::fs::write(
        dir.join("src/kept.rs"),
        "pub struct A;\npub struct B;\n\
         impl From<A> for B { fn from(_: A) -> B { B } }\n\
         impl From<B> for A { fn from(_: B) -> A { A } }\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/touched.rs"), "pub fn touched() {}\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub mod kept;\npub mod touched;\n").unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    std::fs::write(dir.join("src/touched.rs"), "pub fn touched() {}\npub fn more() {}\n").unwrap();

    let root = dir.to_str().unwrap();
    let unscoped = rows_of(&ur_stdout_allow_findings(&["--root", root, "conversion-pairs"]));
    assert_eq!(unscoped.len(), 1, "fixture should hold one pair:\n{:?}", unscoped);
    let scoped = rows_of(&ur_stdout_allow_findings(&[
        "--root",
        root,
        "--changed-since",
        "HEAD",
        "conversion-pairs",
    ]));
    assert!(
        scoped.is_empty(),
        "a pair in an untouched file leaked into a scoped run:\n{:?}",
        scoped
    );
}

#[test]
fn a_scoped_audit_does_not_report_out_of_scope_waivers_as_dead() {
    // Every check calls `retain_changed` *before* `retain_unsuppressed`, so
    // under `--changed-since` a waiver in an unchanged file never sees a
    // finding — its hit count is zero by construction, not by decay. Tallied
    // whole-ledger, a scoped run on unruster's own tree said "25 waiver(s) …,
    // 24 of them suppressing nothing" where the unscoped answer is 4: a line
    // that reads as a demand to delete two dozen live waivers.
    let dir = scoped_waiver_repo("scoped-waivers-audit");
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "--changed-since", "HEAD", "audit"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stderr);
    let line = s
        .lines()
        .find(|l| l.starts_with("(audit:"))
        .unwrap_or_else(|| panic!("no audit summary in:\n{}", s));
    assert!(
        !line.contains("suppressing nothing"),
        "the waiver outside the diff was called dead:\n{}",
        line
    );
    assert!(
        line.contains("1 waiver(s) in the changed files"),
        "the count should name its scope:\n{}",
        line
    );
}

#[test]
fn waivers_honours_changed_since_on_its_rows_but_not_on_orphanhood() {
    // `--changed-since` is global and its help promises it "applies to
    // site-listing commands"; `waivers` used to fall through and list the whole
    // ledger, so it disagreed with the scoped `audit` line that sent the reader
    // over. Scoping the rows is the fix — scoping the *hit counts* would not be,
    // since it would report every waiver outside the diff as orphaned, which is
    // the bug this command exists to detect.
    let dir = scoped_waiver_repo("scoped-waivers-list");
    let root = dir.to_str().unwrap();
    let out = ur()
        .args([
            "--root", root, "--changed-since", "HEAD", "waivers", "--scope", "all",
        ])
        .output()
        .unwrap();
    let (rows, err) = (rows_of(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(rows.len(), 1, "expected only the changed file's waiver:\n{:?}", rows);
    assert!(rows[0].contains("touched.rs"), "{:?}", rows);
    assert!(err.contains("held back 1 waiver(s)"), "the drop was silent:\n{}", err);
    // Judged against the whole tree: the out-of-scope waiver is live, so
    // nothing is reported orphaned.
    assert!(
        err.contains("2 waiver(s) shown") || err.contains("of 2 waiver(s) shown"),
        "the ledger total should stay whole-tree:\n{}",
        err
    );
    assert!(
        err.contains("0 earning nothing in `audit`"),
        "a live waiver outside the diff was scored as orphaned:\n{}",
        err
    );
}

#[test]
fn context_flag_prints_snippets() {
    let out = ur_stdout(&["--root", FIXTURE, "--context", "1", "casts"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().any(|l| l.trim_start().starts_with('>')),
        "expected `>`-marked snippet lines:\n{}",
        s
    );
}

#[test]
fn blind_spots_reported_on_stderr() {
    // The fixture contains a macro whose tokens don't parse as expressions.
    let out = ur()
        .args(["--root", FIXTURE, "callers", "println"])
        .output()
        .unwrap();
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("blind spots:"), "expected blind-spot count:\n{}", e);
}

#[test]
fn dead_code_include_trait_impls_reports_more() {
    let base = ur()
        .args(["--root", FIXTURE, "dead-code"])
        .output()
        .unwrap();
    let more = ur()
        .args(["--root", FIXTURE, "dead-code", "--include-trait-impls"])
        .output()
        .unwrap();
    assert!(
        rows_of(&more.stdout).len() >= rows_of(&base.stdout).len(),
        "trait-impl mode must be a superset"
    );
}

#[test]
fn callers_transitive_unlimited_depth_terminates() {
    // Regression: the BFS re-enqueued names forever on cyclic call graphs, so
    // `--transitive` WITHOUT `--depth` hung. Found by the full-option sweep.
    ur().args(["--root", FIXTURE, "callers", "--transitive", "Document::new"])
        .timeout(std::time::Duration::from_secs(20))
        .assert()
        .success();
}

#[test]
fn audit_strict_gates_advisory_findings() {
    ur().args(["--root", FIXTURE, "audit", "--strict"])
        .assert()
        .failure()
        .code(1)
        .stderr(contains("--strict: all gate"));
}

#[test]
fn enum_coverage_all_tags_sealed_rows() {
    ur().args(["--root", FIXTURE, "enum-coverage", "--all"])
        .assert()
        .stdout(contains("SEALED"));
}

#[test]
fn explain_matches_multi_word_topic() {
    ur().args(["explain", "god", "function"])
        .assert()
        .failure(); // clap: one positional — multi-word must be quoted
    ur().args(["explain", "god function"])
        .assert()
        .success()
        .stdout(contains("GOD FUNCTION TO SPLIT"));
}

// ════════════════════════════════════════════════════════════════════════════
//  Divergence, waivers, output format, and the noise-reduction defaults.
//  These assert the behaviours added after an audit of two real-codebase runs;
//  each one corresponds to a failure mode observed in that transcript.
// ════════════════════════════════════════════════════════════════════════════

/// Second fixture, kept apart from `sample` so new cases here can't shift the
/// row counts the sample-fixture tests assert on.
const DIV: &str = "fixtures/divergence";

#[test]
fn audit_prints_each_section_header_before_its_rows() {
    // The regression this guards: `section(title, gate, count)` evaluated
    // `count` — which printed the rows — before the call that printed the
    // header, so every header landed after its own section.
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "audit"]);
    let s = String::from_utf8_lossy(&out);
    let lines: Vec<&str> = s.lines().collect();
    let first_header = lines
        .iter()
        .position(|l| l.starts_with("## "))
        .expect("expected at least one section header");
    assert_eq!(
        first_header, 0,
        "the first line of audit output must be a section header, got:\n{}",
        lines[..5.min(lines.len())].join("\n")
    );
}

#[test]
fn audit_puts_each_sections_summary_in_the_section() {
    // Rows on stdout and counts on stderr forced a re-run with split
    // redirection just to read the output once.
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "audit"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("partial site(s)"),
        "expected per-check summary lines on stdout:\n{}",
        s
    );
}

#[test]
fn audit_findings_only_drops_clean_sections_and_says_how_many() {
    // Two thirds of a healthy battery is sections reporting that they found
    // nothing, which is what pushed one session's real findings past its own
    // `| head -60` and made it run the whole battery again with `| tail -40`.
    // Hiding them must never hide a finding, so the count and the closing
    // tallies have to be identical either way.
    let full = ur_stdout_allow_findings(&["--root", FIXTURE, "audit"]);
    let lean = ur_stdout_allow_findings(&["--root", FIXTURE, "audit", "--findings-only"]);
    let headers = |o: &[u8]| -> Vec<String> {
        rows_of(o).into_iter().filter(|l| l.starts_with("## ")).collect()
    };
    assert!(
        headers(&lean).len() < headers(&full).len(),
        "nothing was dropped: {} vs {}",
        headers(&lean).len(),
        headers(&full).len()
    );
    // Every surviving header is one that also appears in the full run — the
    // flag omits, it does not rewrite.
    for h in headers(&lean) {
        assert!(headers(&full).contains(&h), "invented a section: {}", h);
    }
    let err = |args: &[&str]| -> String {
        let mut full = vec!["--root", FIXTURE];
        full.extend(args);
        String::from_utf8_lossy(&ur().args(&full).output().unwrap().stderr).into_owned()
    };
    let line = err(&["audit", "--findings-only"]);
    let summary = line.lines().find(|l| l.starts_with("(audit:")).unwrap();
    assert!(summary.contains("hid"), "the omission is silent:\n{}", summary);
    // The finding counts and check count are the run's, not the listing's.
    let counts = |s: &str| s.split(';').next().unwrap().to_string();
    let plain = err(&["audit"]);
    assert_eq!(
        counts(plain.lines().find(|l| l.starts_with("(audit:")).unwrap()),
        counts(summary),
        "--findings-only changed what was counted"
    );
}

#[test]
fn a_failing_audit_says_the_exit_code_is_the_process_s() {
    // The documented loop is `until unruster audit; do …; done`, and the habit
    // around it is a pipe: one session ran `audit … | tail -40; echo "EXIT=$?"`
    // and read back `EXIT=0`, which was tail's. It happened to be clean.
    let out = ur().args(["--root", FIXTURE, "audit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "fixture should have gating findings");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("after a pipe `$?` is the pipe's"),
        "the trap is unsaid:\n{}",
        err
    );
}

#[test]
fn a_note_stays_under_the_rows_it_qualifies_when_the_streams_are_merged() {
    // Under the `2>&1` a reader actually writes, the sibling note printed a
    // line *above* the header row it was about — `show cmd::isolate 2>&1 |
    // head -160` opened with "note: 1 other item(s) … also named `isolate`",
    // reading as though the tool had answered before it was asked. Not a
    // buffering race (Rust line-buffers stdout through a pipe too): it was
    // emitted during name resolution, before anything had been printed.
    //
    // Goes through a shell and a pipe because the merge is the whole
    // condition — the harness captures the two streams separately and would
    // show any ordering as passing.
    let bin = assert_cmd::cargo::cargo_bin("unruster");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{} --root {} show Document::render 2>&1 | cat",
            bin.display(),
            FIXTURE
        ))
        .output()
        .unwrap();
    let merged = String::from_utf8_lossy(&out.stdout);
    let at = |pat: &str| -> usize {
        merged
            .lines()
            .position(|l| l.contains(pat))
            .unwrap_or_else(|| panic!("no {:?} in:\n{}", pat, merged))
    };
    assert!(
        at("also named") > at("Document::render"),
        "the note overtook the row it qualifies:\n{}",
        merged
    );
}

#[test]
fn help_shows_the_command_list_within_the_first_screen() {
    // The playbook used to occupy the first 296 lines of `--help`, so the
    // command list was invisible to anyone piping through `head`. That is the
    // failure this guards against — the exact bound is a budget, not a
    // measurement, and it is deliberately tight so that adding preamble is a
    // decision someone makes rather than one that happens.
    //
    // Raised 60 → 62 when `show` and `outline` were added: two new top-level
    // commands earn their two lines in a list of commands, and the rest of that
    // feature's documentation went to `explain reading-code` rather than here.
    //
    // Raised 62 → 63 for `contract-drift`, on the same terms: one quickstart
    // line for the command, and the rest of it in `explain contract-drift`.
    //
    // Raised 63 → 71 for `concepts`, `near-clones` and `gate`: three commands
    // earn their three lines and `--no-cache` its one, and `gate` earns three
    // more because it is a new *mode* of use rather than another check —
    // nobody discovers a PreToolUse hook from a command list. Everything else
    // about them went to `explain pre-write-gate` / `explain concept-drift`,
    // which is where the last three raises sent their features too.
    //
    // Raised 71 → 75 for `doc-drift`, `vocabulary`, `validation-drift` and
    // `asserts`: two quickstart lines for the two a reader reaches for first,
    // and two for the `concept(…)` marker, which is the one thing here nobody
    // can discover from a command list because it is written in *their*
    // source. The other two commands and every repair recipe went to
    // `explain vocabulary` / `explain doc-drift` / `explain validation-drift`.
    let out = ur_stdout(&["--help"]);
    let s = String::from_utf8_lossy(&out);
    let idx = s
        .lines()
        .position(|l| l.starts_with("Commands:"))
        .expect("expected a Commands: section in --help");
    assert!(
        idx < 75,
        "Commands: must appear within the first 75 help lines, found at {}",
        idx
    );
}

#[test]
fn playbook_subcommand_prints_the_full_text() {
    ur().args(["playbook"])
        .assert()
        .success()
        .stdout(contains("DESIGN AUDIT PLAYBOOK"))
        .stdout(contains("GOD FUNCTION TO SPLIT"));
}

#[test]
fn the_playbook_names_the_habits_that_cost_real_sessions() {
    // Each of these is in the text because a measured session paid for not
    // knowing it. They are cheap to delete in a reflow and expensive to
    // rediscover, so they are pinned.
    let s = String::from_utf8_lossy(&ur_stdout(&["playbook"])).into_owned();
    for (needle, why) in [
        // A `grep -A<N>` on a type is `+70` under another name — written three
        // times in one session that used `show` correctly eight times.
        ("-A45", "the grep -A<N> form `show` replaces"),
        // Three full batteries: two to page the report, one for the exit code
        // the pipes had thrown away.
        ("--findings-only --top 10", "the bounded audit invocation"),
        ("$?` is the *last* command's status", "the piped-exit-code trap"),
        // A prose sweep stopped at a waiver, unsure what was load-bearing.
        ("The reason is prose and nothing keys off it", "the waiver contract"),
    ] {
        assert!(s.contains(needle), "the playbook lost {}", why);
    }
}

#[test]
fn divergence_pairs_the_sibling_that_forgot_a_variant() {
    let out = ur_stdout(&["--root", DIV, "divergence"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("handle_anchor_delete_anim") && s.contains("MiddleKnot"),
        "expected the lean sibling and its missing variant:\n{}",
        s
    );
    assert!(
        s.contains("handle_anchor_delete "),
        "expected the rich sibling named on the same row:\n{}",
        s
    );
}

#[test]
fn divergence_handling_finds_the_careless_sibling() {
    let out = ur_stdout(&["--root", DIV, "divergence", "--handling"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("handle_open_file") && s.contains("take_open_file"),
        "expected both sides of the poisoned-lock divergence:\n{}",
        s
    );
    assert!(
        s.contains("lock"),
        "expected the callee column to name what diverged:\n{}",
        s
    );
}

#[test]
fn divergence_row_shape() {
    // score, kin, missing, lean, at, vs, vs_at → 7 cols (8 with the enum
    // prefix in all-enums mode, which is the default).
    let out = ur_stdout(&["--root", DIV, "divergence"]);
    assert!(!rows_of(&out).is_empty());
    assert_tsv_cols(&out, 8);
}

#[test]
fn enum_coverage_max_missing_isolates_the_forgot_one_shape() {
    let all = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Token"]);
    let one = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Token", "--max-missing", "1"]);
    assert!(
        rows_of(&one).len() < rows_of(&all).len(),
        "--max-missing 1 should drop wider-gap rows"
    );
    for line in rows_of(&one) {
        // Column 5 is the missing-variant list; exactly one entry. (Index 4,
        // not 3: every enum row now leads with the `enum` column whether or not
        // one was named, so the width no longer depends on the argument.)
        let missing = line.split('\t').nth(4).unwrap_or("");
        assert_eq!(
            missing.split(',').count(),
            1,
            "row kept by --max-missing 1 has >1 missing variant: {:?}",
            line
        );
    }
}

#[test]
fn enum_coverage_reports_what_max_missing_hid() {
    // A filter that silently shrinks the result set reads as a clean codebase.
    ur().args(["--root", FIXTURE, "enum-coverage", "Token", "--max-missing", "1"])
        .assert()
        .success()
        .stderr(contains("hidden by --max-missing"));
}

#[test]
fn enum_coverage_rank_enums_gives_one_row_per_enum() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "--rank-enums"]);
    let rows = rows_of(&out);
    assert!(!rows.is_empty());
    assert_tsv_cols(&out, 4);
    let names: Vec<&str> = rows.iter().map(|l| l.split('\t').next().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names.len(), sorted.len(), "each enum should appear once");
}

#[test]
fn enum_coverage_compact_drops_the_repeated_variant_columns() {
    let out = ur_stdout(&["--root", FIXTURE, "enum-coverage", "Token", "--compact"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.starts_with("# Token ["),
        "compact mode should state the variant set once, in a header:\n{}",
        s
    );
    for line in rows_of(&out).into_iter().filter(|l| !l.starts_with('#')) {
        // enum, gap, covered, at, context — `enum` is always present now.
        assert_eq!(line.split('\t').count(), 5, "compact row shape: {:?}", line);
    }
}

#[test]
fn casts_hide_lossless_usize_widening_by_default() {
    let out = ur_stdout(&["--root", DIV, "casts"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        !s.contains("usize-widen"),
        "u32 as usize is lossless on 64-bit and should be off by default:\n{}",
        s
    );
    assert!(
        s.contains("usize-cross\tf64\tusize"),
        "genuinely lossy f64 as usize must still be reported:\n{}",
        s
    );
}

#[test]
fn casts_can_ask_for_the_widening_rows_by_name() {
    ur().args(["--root", DIV, "casts", "--class", "usize-widen"])
        .assert()
        .success()
        .stdout(contains("usize-widen\tu32\tusize"));
}

#[test]
fn casts_never_state_a_guessed_source_type() {
    // `Bitmap::width()` returns u32, but the fixture also defines
    // `Rect::width() -> f64`. Resolving the method by bare name reported f64
    // here — a confidently wrong type that cost the whole check its credibility.
    let out = ur_stdout(&["--root", DIV, "casts"]);
    let s = String::from_utf8_lossy(&out);
    let stride_row = s
        .lines()
        .find(|l| l.contains("casting::stride"))
        .expect("expected a cast row in casting::stride");
    assert!(
        stride_row.starts_with("unknown\t_\t"),
        "an ungrounded source must render as `_`, not a guess: {:?}",
        stride_row
    );
}

#[test]
fn error_swallows_keeps_benign_families_by_default_and_audit_drops_them() {
    let default = ur_stdout(&["--root", DIV, "error-swallows"]);
    let strict = ur_stdout(&[
        "--root",
        DIV,
        "error-swallows",
        "--hide-infallible",
        "--hide-logged",
    ]);
    assert!(
        rows_of(&strict).len() < rows_of(&default).len(),
        "hiding infallible writes and logged fallbacks should shrink the set"
    );
    let s = String::from_utf8_lossy(&strict);
    assert!(
        !s.contains("swallows::render"),
        "`let _ = write!(String, …)` is infallible and should be hidden:\n{}",
        s
    );
}

#[test]
fn ok_under_a_question_mark_is_propagation_not_a_swallow() {
    // `parse().ok()?` discards the error value but propagates the failure, so
    // control never continues past it. Found by running unruster on itself,
    // where six of seven `.ok` rows were this idiom. `audit` drops them; the
    // bare command shows them but says how many are benign.
    let audit = ur_stdout_allow_findings(&["--root", DIV, "--no-suppress", "audit"]);
    let s = String::from_utf8_lossy(&audit);
    assert!(
        s.contains(".ok=1"),
        "only the bare `.ok()` should reach audit, got:\n{}",
        s.lines().find(|l| l.contains("swallow site")).unwrap_or("")
    );
    // …and the bare command must not read as "nothing changed".
    let bare = ur().args(["--root", DIV, "--no-suppress", "error-swallows"]).output().unwrap();
    let err = String::from_utf8_lossy(&bare.stderr);
    assert!(err.contains("are benign"), "summary should own up:\n{}", err);
}

#[test]
fn waiver_comment_suppresses_exactly_its_own_site() {
    let with = ur_stdout(&["--root", DIV, "error-swallows"]);
    let without = ur_stdout(&["--root", DIV, "--no-suppress", "error-swallows"]);
    assert_eq!(
        rows_of(&without).len(),
        rows_of(&with).len() + 1,
        "the single `// unruster: ok` waiver should hide exactly one row"
    );
}

#[test]
fn rewording_a_waivers_reason_does_not_stop_it_matching() {
    // A session doing a prose sweep over its own comments hit an
    // `// unruster: ok(...)` line and stopped, unsure: "there's a small risk
    // that unruster might hash the entire line". It had to run an audit to
    // find out. Nothing in the help, the grammar or the playbook said which
    // parts of the comment are load-bearing — and the reason, the one part a
    // human is meant to keep current, is the part nothing keys off.
    let dir = scratch("waiver-reword");
    let src = |reason: &str| {
        format!(
            "pub fn cleanup(p: &std::path::Path) {{\n    \
             let _ = std::fs::remove_file(p); // unruster: ok(error-swallows/let-_) \
             2026-01-01 — {}\n}}\n",
            reason
        )
    };
    let root = dir.to_str().unwrap();
    let hidden = |reason: &str| -> usize {
        std::fs::write(dir.join("src/lib.rs"), src(reason)).unwrap();
        let with = rows_of(&ur_stdout_allow_findings(&["--root", root, "error-swallows"])).len();
        let without = rows_of(&ur_stdout_allow_findings(&[
            "--root",
            root,
            "--no-suppress",
            "error-swallows",
        ]))
        .len();
        without - with
    };
    assert_eq!(hidden("absence is fine"), 1, "the waiver never worked");
    assert_eq!(
        hidden("the file may already be gone, and that is the success case"),
        1,
        "rewording the reason stopped the waiver matching"
    );
    // Including across a wrap onto a second comment line.
    assert_eq!(
        hidden("the file may already be gone\n    // and that is the success case"),
        1,
        "a wrapped reason stopped the waiver matching"
    );
}

// ── waiver grammar: scope, keys, lifecycle ────────────────────────────────
//
// `fixtures/waivers` is separate from `fixtures/divergence` so these cases
// can't shift the row counts asserted above.

const WV: &str = "fixtures/waivers/src";
const SCOPE_FIXTURE: &str = "fixtures/scope/src";
const DIVGROUP: &str = "fixtures/divgroup/src";
const TYPO_FIXTURE: &str = "fixtures/typo/src";
const DRIFT: &str = "fixtures/drift/src";
/// Pinned "today" — the system clock is the only non-deterministic input in
/// the tool, and an unpinned age would make these assertions rot.
const TODAY: &str = "2026-08-06";

/// stderr as a String, for summary-line assertions.
fn ur_stderr(args: &[&str]) -> String {
    let out = ur().args(args).output().unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Copy the waiver fixture into a scratch dir so mutating tests can't touch
/// the checked-in source. Returns the scratch root.
fn scratch_fixture(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::copy(
        "fixtures/waivers/src/lib.rs",
        dir.join("src").join("lib.rs"),
    )
    .unwrap();
    dir.join("src")
}

// ── stable finding identity ───────────────────────────────────────────────

/// A throwaway git repo with `src/lib.rs` containing `body`, committed.
fn git_fixture(name: &str, body: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), body).unwrap();
    for args in [
        vec!["init", "-q", "."],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
        vec!["add", "-A"],
        vec!["commit", "-qm", "base"],
    ] {
        let ok = std::process::Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }
    dir
}

fn baseline_line(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .lines()
        .find(|l| l.starts_with("(baseline:"))
        .unwrap_or("<no baseline line>")
        .to_string()
}

const BODY: &str = "\
pub fn a(p: &std::path::Path) { let _ = std::fs::remove_file(p); }
pub fn b(n: u64) -> u32 { n as u32 }
";

#[test]
fn inserting_lines_does_not_manufacture_findings() {
    // The whole reason fingerprints exist. Before them, a diff of two runs
    // across a five-line insertion reported every finding below it as one
    // deletion plus one addition; five of six apparent regressions in a real
    // session were this and nothing else.
    let dir = git_fixture("fp-shift", BODY);
    let src = dir.join("src");
    std::fs::write(
        src.join("lib.rs"),
        format!("//! 1\n//! 2\n//! 3\n//! 4\n//! 5\n{BODY}"),
    )
    .unwrap();
    let out = ur_stdout_allow_findings(&[
        "--root", src.to_str().unwrap(), "--all-stdout", "audit", "--since", "HEAD",
    ]);
    let line = baseline_line(&out);
    assert!(
        line.contains("0 gone, 0 new, 0 moved"),
        "a pure line shift must be invisible: {line}"
    );
}

#[test]
fn fixed_new_and_moved_land_in_the_right_buckets() {
    let dir = git_fixture("fp-buckets", BODY);
    let src = dir.join("src");
    // Waive one, add one, relocate one into a submodule.
    std::fs::write(
        src.join("lib.rs"),
        "pub mod moved;\n\
         // unruster: ok(error-swallows/let-_) 2026-08-06 — absence is fine\n\
         pub fn a(p: &std::path::Path) { let _ = std::fs::remove_file(p); }\n\
         pub fn d(x: u64) -> u32 { x as u32 }\n",
    )
    .unwrap();
    std::fs::create_dir_all(src.join("moved")).unwrap();
    std::fs::write(
        src.join("moved/mod.rs"),
        "pub fn b(n: u64) -> u32 { n as u32 }\n",
    )
    .unwrap();

    let out = ur_stdout_allow_findings(&[
        "--root", src.to_str().unwrap(), "--all-stdout", "audit", "--since", "HEAD",
    ]);
    let s = String::from_utf8_lossy(&out);
    // The waived swallow is gone; `b` relocated, so it must read as moved
    // rather than as a fix plus a regression; `d` is genuinely new.
    assert!(s.contains("gone\terror-swallows"), "expected the waiver to retire one:\n{s}");
    assert!(s.contains("moved\t"), "a relocation is not a fix + a regression:\n{s}");
    assert!(s.contains("new\t"), "a genuinely new finding must show:\n{s}");
}

#[test]
fn a_baseline_file_round_trips_and_gates_on_regressions() {
    let dir = git_fixture("fp-baseline", BODY);
    let src = dir.join("src");
    let bl = dir.join("bl.tsv");
    let (s, b) = (src.to_str().unwrap(), bl.to_str().unwrap());

    ur_stdout_allow_findings(&["--root", s, "audit", "--write-baseline", b]);
    let same = ur_stdout_allow_findings(&["--root", s, "--all-stdout", "audit", "--baseline", b]);
    assert!(
        baseline_line(&same).contains("0 gone, 0 new, 0 moved"),
        "an unchanged tree must diff clean: {}",
        baseline_line(&same)
    );
    // The gate an agent wants: "did I make it worse", not "is it perfect".
    ur().args(["--root", s, "audit", "--baseline", b, "--fail-on-new"])
        .assert()
        .success();
    std::fs::write(
        src.join("lib.rs"),
        format!("{BODY}pub fn e(v: u64) -> u32 {{ v as u32 }}\n"),
    )
    .unwrap();
    ur().args(["--root", s, "audit", "--baseline", b, "--fail-on-new"])
        .assert()
        .failure();
}

#[test]
fn fingerprints_are_emitted_in_json_and_behind_a_tsv_flag() {
    let json = ur_stdout(&["--root", WV, "--json", "casts"]);
    assert!(
        String::from_utf8_lossy(&json).contains("\"fp\""),
        "JSON always carries the fingerprint"
    );
    // TSV stays byte-compatible unless asked: a new column breaks callers.
    let plain = ur_stdout(&["--root", WV, "casts"]);
    let flagged = ur_stdout(&["--root", WV, "--fingerprints", "casts"]);
    let cols = |o: &[u8]| rows_of(o).first().map(|r| r.split('\t').count()).unwrap_or(0);
    assert_eq!(cols(&flagged), cols(&plain) + 1, "--fingerprints adds exactly one column");
}

// ── config-drift ──────────────────────────────────────────────────────────

#[test]
fn config_drift_ignores_a_naming_field_and_an_import_spelling() {
    // Seven of the nine false positives on a real codebase were these two
    // shapes: descriptors differing only in `label`, and the same constant
    // written `Margin::Percent(0)` at one site and
    // `crate::…::Margin::Percent(0)` at another.
    let out = ur_stdout(&["--root", DRIFT, "config-drift", "--min-score", "0.0"]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("Desc\t"), "a label is meant to differ:\n{s}");
    assert!(!s.contains("Pending\t"), "same value, two import spellings:\n{s}");
    // …and the label-only case is counted, never silently dropped.
    let err = ur_stderr(&["--root", DRIFT, "config-drift", "--min-score", "0.0"]);
    assert!(err.contains("naming field"), "{err}");
}

#[test]
fn an_empty_scan_is_an_error_not_a_clean_result() {
    // A typo'd --root reported "0 gating + 0 advisory; clean; exit 0", so
    // `until unruster audit; do fix; done` terminated immediately and a CI
    // gate passed vacuously. Seen in the wild.
    let out = ur().args(["--root", "no/such/dir", "audit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "an empty scan must exit 2");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nothing was analysed"), "{err}");
}

#[test]
fn builder_drift_finds_the_chain_that_forgot_a_step() {
    // Two `git` chains alike but for one call — the shape that made
    // `--since` resolve the wrong repository. `co-call` could not see it:
    // the enclosing fn calls both the constructor and the missing method.
    let out = ur_stdout(&["--root", DRIFT, "builder-drift"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("dir{1/2}"), "expected the missing call:\n{s}");
    assert!(s.contains("chains::resolve"), "row must name the lean chain:\n{s}");
}

#[test]
fn builder_drift_groups_by_the_constructors_constant_args() {
    // `Cmd::new("tar")` configures a different program; comparing its chain
    // with the `git` ones would be noise, and it is the only `tar` chain so it
    // cannot drift against anything.
    let out = ur_stdout(&["--root", DRIFT, "builder-drift", "--min-score", "0.0"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("Cmd::new(\"git\")"), "{s}");
    assert!(!s.contains("Cmd::new(\"tar\")"), "a lone chain cannot drift:\n{s}");
}

#[test]
fn config_drift_finds_two_presets_that_agree_on_nothing() {
    // The shape of the real defect: two modules building the same options
    // struct for the same operation, with every field diverged.
    let out = ur_stdout(&["--root", DRIFT, "config-drift"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("Opts"), "expected the drifted struct:\n{s}");
    assert!(
        s.contains("gating::build") && s.contains("probe::build"),
        "row must name a concrete pair to diff:\n{s}"
    );
    // A field left to `..Default::default()` on one side and spelled out on
    // the other is a difference, and the easiest kind to skim past.
    assert!(s.contains("compact{(default)|true}"), "{s}");
}

#[test]
fn config_drift_ignores_a_types_own_constructors() {
    // `Sink::new` / `Sink::silent` differ on purpose — that is the type's API.
    // Without this rule every two-constructor type tops the ranking forever.
    let out = ur_stdout(&["--root", DRIFT, "config-drift"]);
    assert!(
        !String::from_utf8_lossy(&out).contains("Sink"),
        "constructors are not drift:\n{}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
fn config_drift_lets_a_computed_site_abstain() {
    // `cli::build` sets every field from a parameter. It must not suppress the
    // comparison between the two sites that do spell out constants.
    let out = ur_stdout(&["--root", DRIFT, "config-drift"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("hide_routed{false|true}"),
        "a computed third site must not erase the field:\n{s}"
    );
}

#[test]
fn config_drift_ranks_a_narrow_disagreement_above_a_broad_one() {
    let out = ur_stdout(&["--root", DRIFT, "config-drift", "--min-score", "0.0"]);
    let scores: Vec<f64> = rows_of(&out)
        .iter()
        .filter_map(|r| r.split('\t').nth(1)?.parse().ok())
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "rows must be ranked loudest-first: {scores:?}"
    );
}

#[test]
fn the_two_gating_checks_that_had_no_waiver_support_now_have_it() {
    // `dead-code` and `conversion-pairs` gate the audit loop but ignored
    // waivers entirely, so a verified false positive in either could never be
    // retired and `audit` could never exit 0. On a real codebase that dead end
    // pushed someone into maintaining a parallel `// NOTE (unruster …)`
    // convention this tool cannot read.
    for (check, marker) in [
        ("dead-code", "named_by_attribute"),
        ("conversion-pairs", "Foreign"),
    ] {
        let without = ur_stdout(&["--root", WV, "--no-suppress", check]);
        let with = ur_stdout(&["--root", WV, check]);
        let s_without = String::from_utf8_lossy(&without);
        let s_with = String::from_utf8_lossy(&with);
        assert!(
            s_without.contains(marker),
            "{check} should flag {marker} unwaived:\n{s_without}"
        );
        assert!(
            !s_with.contains(marker),
            "{check} waiver should retire it:\n{s_with}"
        );
    }
}

#[test]
fn the_waiver_a_check_suggests_is_the_waiver_it_honours() {
    // `clones` printed `ok(clones/<label>)` and then matched on an *empty* key,
    // so the comment the tool told you to write did nothing and only a bare
    // `ok(clones)` worked. A suggestion that is silently inert is worse than no
    // suggestion: it looks like it succeeded.
    //
    // Asserted for every check that offers one — a key can only drift on the
    // check where suggestion and filter are written apart.
    let root = std::env::temp_dir().join("unruster_suggest_honoured");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // One duplicated body (clones) and one literal comparison (stringly).
    // Bodies wide enough to clear `--min-tokens`, differing only in local names.
    std::fs::write(
        src.join("lib.rs"),
        r#"
pub fn one(v: &[u8]) -> usize {
    let mut n = 0usize;
    for b in v {
        if *b > 3 && *b < 200 { n += 1; } else { n += 2; }
    }
    n * 3 + v.len()
}
pub fn two(w: &[u8]) -> usize {
    let mut m = 0usize;
    for c in w {
        if *c > 3 && *c < 200 { m += 1; } else { m += 2; }
    }
    m * 3 + w.len()
}
pub fn pick(k: &str) -> u8 { if k == "alpha" { 1 } else { 0 } }
"#,
    )
    .unwrap();
    let r = root.to_str().unwrap();

    let original = std::fs::read_to_string(src.join("lib.rs")).unwrap();

    for check in ["clones", "stringly"] {
        let suggested = ur_stdout(&["--root", r, "--suggest-waivers", check]);
        let s = String::from_utf8_lossy(&suggested);
        let mut lines = s.lines();
        let row = lines
            .by_ref()
            .find(|l| l.contains('\t'))
            .unwrap_or_else(|| panic!("{check} produced no row:\n{s}"))
            .to_string();
        let waiver = s
            .lines()
            .find(|l| l.contains("unruster: ok("))
            .unwrap_or_else(|| panic!("{check} offered no waiver:\n{s}"))
            .trim()
            .replace("WHY?", "verified false positive");

        // Paste it where the row points — above that line, which is what the
        // waiver grammar means by "above the item" for a site-scoped finding.
        let at = row
            .split('\t')
            .find_map(|c| c.rsplit_once(':').and_then(|(_, n)| n.parse::<usize>().ok()))
            .unwrap_or_else(|| panic!("no file:line in row: {row}"));
        let mut out: Vec<String> = original.lines().map(str::to_string).collect();
        out.insert(at - 1, waiver.clone());
        std::fs::write(src.join("lib.rs"), out.join("\n") + "\n").unwrap();

        let after = rows_of(&ur_stdout(&["--root", r, check])).len();
        let unwaived = rows_of(&ur_stdout(&["--root", r, "--no-suppress", check])).len();
        assert!(
            after < unwaived,
            "{check} suggested `{waiver}` and then ignored it ({after} of {unwaived} rows remain)"
        );
        std::fs::write(src.join("lib.rs"), &original).unwrap();
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn suggest_waivers_says_so_when_a_check_cannot_use_them() {
    // Silence here is what sent a real agent off to invent its own format.
    //
    // The check named here must be one that genuinely ignores waivers.
    // `stringly` used to stand in for that and does not: it filters on them and
    // honours the key `--suggest-waivers` prints, so the note was false and the
    // test was pinning the falsehood. `conversions` neither suggests nor filters.
    let out = ur().args(["--root", WV, "--suggest-waivers", "conversions"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not support waivers"),
        "expected a note, got:\n{}",
        err
    );
    // And it must not cry wolf on a check that does support them.
    let ok = ur().args(["--root", WV, "--suggest-waivers", "dead-code"]).output().unwrap();
    assert!(
        !String::from_utf8_lossy(&ok.stderr).contains("does not support waivers"),
        "dead-code supports waivers now"
    );
}

#[test]
fn one_enum_named_waiver_covers_every_missing_variant() {
    // `ok(enum-coverage/Modal)` against findings keyed `Modal::None`,
    // `Modal::NewDoc`, … Before the prefix match this needed one comment per
    // missing variant — four, on a real row.
    let without = ur_stdout(&["--root", WV, "--no-suppress", "enum-coverage", "Modal"]);
    let with = ur_stdout(&["--root", WV, "enum-coverage", "Modal"]);
    assert_eq!(rows_of(&without).len(), 1);
    assert!(rows_of(&with).is_empty(), "one waiver should clear the row");

    // …and the count lands in `below_audit`, not `suppresses`: this row misses
    // four of five variants, so `audit`'s `--max-missing 1` filters it out and
    // the waiver earns nothing there. That distinction is the whole point of
    // the two columns — a real ledger was a third full of waivers like this
    // while reporting "0 orphaned".
    let waivers = ur_stdout(&["--root", WV, "waivers", "--check", "enum-coverage", "--today", TODAY]);
    let s = String::from_utf8_lossy(&waivers);
    let row = s.lines().find(|l| l.contains("Modal")).expect("Modal row");
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[5], "0", "earns nothing in audit: {row}");
    assert_eq!(cols[6], "4", "one comment, four variants: {row}");
}

#[test]
fn orphan_detection_agrees_with_the_audit_line() {
    // These two used to contradict each other in the same run: `audit` counted
    // hits under its own (strict) config while `waivers` counted them wide
    // open, so a ledger could report "0 orphaned" next to an audit line saying
    // several suppressed nothing.
    // `--all-stdout`: the audit summary rides stderr by default.
    let audit = ur_stdout_allow_findings(&["--root", WV, "--all-stdout", "audit"]);
    let a = String::from_utf8_lossy(&audit);
    let audit_dead: usize = a
        .lines()
        .find(|l| l.contains("suppressing nothing"))
        .and_then(|l| l.split(", ").find_map(|p| p.trim().split(' ').next()?.parse().ok()))
        .unwrap_or(0);
    let orphaned = rows_of(&ur_stdout(&["--root", WV, "waivers", "--orphaned", "--today", TODAY]));
    assert_eq!(
        audit_dead,
        orphaned.len(),
        "audit and `waivers --orphaned` must count the same set:\naudit said {audit_dead}, \
         waivers listed {}",
        orphaned.len()
    );
}

#[test]
fn a_group_key_waives_every_check_in_the_group() {
    // `divergence` and `enum-coverage` ask the same question of the same site.
    // Six of thirty-three waivers on a real ledger had the reason `same.`,
    // written only because the check name differed.
    let div_off = ur_stdout(&["--root", WV, "--no-suppress", "divergence", "G"]);
    let cov_off = ur_stdout(&["--root", WV, "--no-suppress", "enum-coverage", "G"]);
    assert!(!rows_of(&div_off).is_empty(), "fixture needs a divergence pair");
    assert!(
        String::from_utf8_lossy(&cov_off).contains("narrow"),
        "fixture needs an enum-coverage row on the same fn"
    );
    // One comment, both checks.
    let div_on = String::from_utf8_lossy(&ur_stdout(&["--root", WV, "divergence", "G"])).into_owned();
    let cov_on = String::from_utf8_lossy(&ur_stdout(&["--root", WV, "enum-coverage", "G"])).into_owned();
    assert!(!div_on.contains("narrow"), "group key must cover divergence:\n{div_on}");
    assert!(!cov_on.contains("narrow"), "…and enum-coverage:\n{cov_on}");
}

#[test]
fn a_waiver_naming_an_unknown_check_is_reported() {
    // A typo'd check name waives nothing, silently — the same dead weight as an
    // orphan, but catchable the moment the comment is read.
    let out = ur().args(["--root", TYPO_FIXTURE, "casts", "--summary"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not have") && err.contains("divergance"),
        "expected an unknown-check note, got:\n{}",
        err
    );
}

#[test]
fn a_fn_named_only_by_an_attribute_string_is_not_dead() {
    // `#[serde(default = "default_true")]` is a real call the derive expands,
    // but the name lives in a string literal.
    let out = ur_stdout(&["--root", WV, "--no-suppress", "dead-code"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        !s.contains("default_true"),
        "attribute-named fn must not read as dead:\n{}",
        s
    );
}

#[test]
fn pointer_casts_inside_unsafe_are_the_ffi_boundary_not_a_defect() {
    let hidden = ur_stdout(&["--root", WV, "casts", "--class", "ptr"]);
    let shown = ur_stdout(&["--root", WV, "casts", "--class", "ptr", "--include-unsafe-ptr"]);
    assert_eq!(rows_of(&hidden).len(), 1, "only the safe cast");
    assert_eq!(rows_of(&shown).len(), 3, "--include-unsafe-ptr restores them");
}

#[test]
fn test_named_files_are_not_production_code() {
    // `looks_like_test_named` only ever widened `--scope tests`; under
    // `production` a `foo_tests.rs` was analysed as production, so swallows in
    // test helpers were reported as defects. Per-file cfg stripping cannot
    // catch it — the `#[cfg(test)] mod` gate lives in the *parent* file.
    let prod = ur_stdout(&["--root", SCOPE_FIXTURE, "error-swallows"]);
    let all = ur_stdout(&["--root", SCOPE_FIXTURE, "--scope", "all", "error-swallows"]);
    let tests = ur_stdout(&["--root", SCOPE_FIXTURE, "--scope", "tests", "error-swallows"]);
    assert_eq!(rows_of(&prod).len(), 1, "only lib.rs is production");
    assert_eq!(rows_of(&all).len(), 3);
    assert_eq!(rows_of(&tests).len(), 2, "tests.rs + foo_tests.rs");
}

#[test]
fn divergence_collapses_one_decision_into_one_row() {
    // The scan is an N×M cross-product by construction, but "this fn omits
    // Group" is one decision no matter how many siblings handle Group. On a
    // real tree three such decisions filled seventeen rows.
    let out = ur_stdout(&["--root", DIVGROUP, "divergence"]);
    let rows = rows_of(&out);
    let collapsed: usize = rows
        .iter()
        .filter_map(|r| r.split("(+").nth(1))
        .filter_map(|r| r.split(' ').next())
        .filter_map(|n| n.parse::<usize>().ok())
        .sum();
    assert!(collapsed > 0, "expected some rows to absorb siblings:\n{:?}", rows);
    assert!(
        rows.len() < rows.len() + collapsed,
        "grouping must reduce the row count"
    );
    // Every lean site appears at most once per (enum, delta).
    let mut keys: Vec<String> = rows
        .iter()
        .map(|r| {
            let c: Vec<&str> = r.split('\t').collect();
            format!("{}|{}|{}", c[0], c[3], c[5])
        })
        .collect();
    let before = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(before, keys.len(), "duplicate (enum, delta, lean) rows remain");
}

#[test]
fn item_scoped_variant_keyed_waiver_retires_a_divergence_pair() {
    // The arena `NodeContent::Group` shape: one comment on the lean side,
    // scoped to the whole fn, keyed to the one variant it means.
    // Scoped to `Node`: the fixture carries other enums for other cases.
    let without = ur_stdout(&["--root", WV, "--no-suppress", "divergence", "Node"]);
    let with = ur_stdout(&["--root", WV, "divergence", "Node"]);
    assert_eq!(rows_of(&without).len(), 1, "fixture should have one pair");
    assert!(
        rows_of(&with).is_empty(),
        "the variant-keyed waiver should retire it:\n{}",
        String::from_utf8_lossy(&with)
    );
    assert!(
        ur_stderr(&["--root", WV, "divergence", "Node"]).contains("1 waived"),
        "the summary must report what it hid — a silent drop reads as clean"
    );
}

#[test]
fn a_keyed_waiver_naming_the_wrong_key_suppresses_nothing() {
    // `ok(error-swallows/.ok)` sits on a `let _ =` line. Matching it would be
    // the over-suppression the key exists to prevent.
    let out = ur_stdout(&["--root", WV, "error-swallows"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("wrong_key"),
        "the mismatched-key row must survive:\n{}",
        s
    );
}

#[test]
fn a_waiver_for_one_check_does_not_silence_another_on_the_same_line() {
    // Two checks, one line, one waiver: only the named check is waived.
    let swallows = ur_stdout(&["--root", WV, "error-swallows"]);
    assert_eq!(
        rows_of(&swallows).len(),
        1,
        "two of three swallows are waived, the wrong-key one is not"
    );
    // The divergence waiver must not touch error-swallows, and vice versa.
    let s = String::from_utf8_lossy(&swallows);
    assert!(!s.contains("strip_incoming_refs"), "check leaked: {}", s);
}

#[test]
fn a_reason_wrapped_across_lines_is_rejoined() {
    // Reflow tolerance: a human (or rustfmt) breaking a long waiver must not
    // truncate what the listing reports.
    let out = ur_stdout(&["--root", WV, "waivers", "--today", TODAY]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("Group is a structural child edge, not a consumer reference; \
                    every consumer walk in this impl excludes it deliberately."),
        "three-line reason should come back as one string:\n{}",
        s
    );
}

#[test]
fn waivers_listing_reports_scope_key_and_suppression_count() {
    let out = ur_stdout(&["--root", WV, "waivers", "--today", TODAY]);
    assert_tsv_cols(&out, 9);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\titem\t"), "item scope must be visible:\n{}", s);
    assert!(s.contains("\tsite\t"), "site scope must be visible:\n{}", s);
    assert!(
        s.contains("Node::Group"),
        "the key belongs in its own column:\n{}",
        s
    );
    // The guardrail on item scope: a waiver that hides a lot must say so.
    let summary = ur_stderr(&["--root", WV, "waivers", "--today", TODAY]);
    assert!(summary.contains("item-scoped"), "{}", summary);
    assert!(summary.contains("orphaned"), "{}", summary);
}

#[test]
fn orphaned_finds_waivers_that_suppress_nothing() {
    let out = ur_stdout(&["--root", WV, "waivers", "--orphaned", "--today", TODAY]);
    let rows = rows_of(&out);
    assert_eq!(rows.len(), 3, "two dead + one below-audit:\n{:?}", rows);
    for r in &rows {
        let cols: Vec<&str> = r.split('\t').collect();
        assert_eq!(cols[5], "0", "orphaned rows earn nothing in audit: {}", r);
    }
    // The two sub-cases must be distinguishable, or "delete it" and "it is
    // below your thresholds" collapse into one unactionable bucket.
    let below: Vec<&String> = rows.iter().filter(|r| r.split('\t').nth(6) != Some("0")).collect();
    assert_eq!(below.len(), 1, "exactly one is below-audit rather than dead:\n{:?}", rows);
}

#[test]
fn stale_measures_against_the_pinned_date() {
    // Fixture ages at TODAY: 2650d, 208d, 186d, 186d, and one undated.
    let over_365 = rows_of(&ur_stdout(&[
        "--root", WV, "waivers", "--stale", "365", "--today", TODAY,
    ]));
    assert_eq!(over_365.len(), 2, "2650d + the undated one: {:?}", over_365);
    let over_200 = rows_of(&ur_stdout(&[
        "--root", WV, "waivers", "--stale", "200", "--today", TODAY,
    ]));
    assert_eq!(over_200.len(), 3, "208d joins them: {:?}", over_200);
}

#[test]
fn an_undated_waiver_counts_as_stale_at_every_threshold() {
    // Deliberate: a waiver with no date cannot be shown to be fresh, so
    // treating it as fresh would make dating optional in practice. `--stale`
    // and `--fail-on-stale` agree on this.
    let huge = rows_of(&ur_stdout(&[
        "--root", WV, "waivers", "--stale", "99999", "--today", TODAY,
    ]));
    assert_eq!(huge.len(), 1, "only the undated one survives: {:?}", huge);
    assert!(huge[0].starts_with("—\t"), "and it is the undated one: {:?}", huge);
}

#[test]
fn fail_on_stale_gates_the_exit_code() {
    ur().args(["--root", WV, "waivers", "--fail-on-stale", "365", "--today", TODAY])
        .assert()
        .failure();
    // A tree whose waivers are all dated and fresh passes the gate. Upgrading
    // stamps today's date on the legacy one; removing the 2019 orphan clears
    // the rest.
    let root = scratch_fixture("waivers-gate");
    let r = root.to_str().unwrap();
    ur_stdout(&["--root", r, "waivers", "--upgrade", "--write", "--today", TODAY]);
    ur_stdout(&[
        "--root", r, "waivers", "--stale", "365", "--remove", "--write", "--today", TODAY,
    ]);
    ur().args(["--root", r, "waivers", "--fail-on-stale", "365", "--today", TODAY])
        .assert()
        .success();
}

#[test]
fn suggest_waivers_prints_a_line_that_actually_works() {
    // The suggestion is the only place the grammar is spelled out at the point
    // of use, so it has to be exactly right — key included.
    let out = ur_stdout(&[
        "--root", WV, "--no-suppress", "--suggest-waivers", "divergence",
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("// unruster: ok(divergence/Node::Group)"),
        "suggestion must carry the qualified key:\n{}",
        s
    );
    // And it must be the same spelling the fixture already proves works.
    let suggested = s
        .lines()
        .find(|l| l.contains("unruster: ok("))
        .unwrap()
        .trim();
    assert!(suggested.contains(" — WHY?"), "{}", suggested);
}

#[test]
fn mutating_actions_are_dry_runs_until_write_is_given() {
    let root = scratch_fixture("waivers-dryrun");
    let file = root.join("lib.rs");
    let before = std::fs::read_to_string(&file).unwrap();
    let r = root.to_str().unwrap();

    let out = ur_stdout(&["--root", r, "waivers", "--upgrade", "--today", TODAY]);
    assert!(
        String::from_utf8_lossy(&out).contains("+"),
        "a dry run should still show the diff"
    );
    ur_stdout(&["--root", r, "waivers", "--orphaned", "--remove", "--today", TODAY]);
    assert_eq!(
        before,
        std::fs::read_to_string(&file).unwrap(),
        "neither action may touch the file without --write"
    );
}

#[test]
fn upgrade_qualifies_a_legacy_waiver_with_the_check_that_hit_it() {
    let root = scratch_fixture("waivers-upgrade");
    let file = root.join("lib.rs");
    let r = root.to_str().unwrap();

    ur_stdout(&["--root", r, "waivers", "--upgrade", "--write", "--today", TODAY]);
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("// unruster: ok(error-swallows) 2026-08-06 — best effort"),
        "check inferred, date stamped, reason preserved:\n{}",
        after
    );
    // The upgraded waiver must still do its job, and no longer be legacy.
    assert_eq!(rows_of(&ur_stdout(&["--root", r, "error-swallows"])).len(), 1);
    assert!(!ur_stderr(&["--root", r, "error-swallows"]).contains("predate"));
}

#[test]
fn remove_strips_a_multi_line_waiver_and_leaves_valid_source() {
    let root = scratch_fixture("waivers-remove");
    let file = root.join("lib.rs");
    let r = root.to_str().unwrap();

    ur_stdout(&[
        "--root", r, "waivers", "--check", "divergence", "--remove", "--write", "--today", TODAY,
    ]);
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        !after.contains("unruster: ok(divergence"),
        "all three lines of the wrapped waiver should be gone:\n{}",
        after
    );
    assert!(
        !after.contains("child edge, not a consumer reference"),
        "continuation lines must go too, not just the head:\n{}",
        after
    );
    // Still parses, and the finding it was hiding comes back.
    assert!(syn::parse_file(&after).is_ok(), "removal broke the source");
    assert_eq!(rows_of(&ur_stdout(&["--root", r, "divergence"])).len(), 1);
}

#[test]
fn removing_a_trailing_waiver_keeps_the_code_on_its_line() {
    let root = scratch_fixture("waivers-trailing");
    let file = root.join("lib.rs");
    let r = root.to_str().unwrap();

    ur_stdout(&[
        "--root", r, "waivers", "--check", "error-swallows", "--remove", "--write", "--today",
        TODAY,
    ]);
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("let _ = std::fs::remove_file(p);"),
        "the statement must survive its comment:\n{}",
        after
    );
    assert!(!after.contains("absence is fine"), "{}", after);
    // A trailing waiver can carry continuation lines too; leaving them behind
    // would strand prose that no longer refers to anything.
    assert!(
        !after.contains("directory already existing is the common case"),
        "continuation of a trailing waiver must go with it:\n{}",
        after
    );
    assert!(
        after.contains("let _ = std::fs::create_dir(p);"),
        "…but its statement stays:\n{}",
        after
    );
    assert!(syn::parse_file(&after).is_ok(), "removal broke the source");
}

#[test]
fn takes_mut_without_a_type_ranks_candidates_instead_of_erroring() {
    let out = ur_stdout(&["--root", FIXTURE, "takes-mut"]);
    let rows = rows_of(&out);
    assert!(!rows.is_empty(), "expected candidate types, got nothing");
    assert_tsv_cols(&out, 2);
    let first = rows[0].split('\t').next().unwrap().parse::<usize>();
    assert!(first.is_ok(), "first column should be a count: {:?}", rows[0]);
}

#[test]
fn catch_all_arms_without_a_name_scans_every_enum() {
    let out = ur_stdout(&["--root", FIXTURE, "catch-all-arms"]);
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.lines().any(|l| l.starts_with("Token\t")),
        "bare invocation should behave like --all:\n{}",
        s
    );
}

#[test]
fn json_output_is_parseable_and_keeps_line_numbers_numeric() {
    let out = ur_stdout(&["--root", FIXTURE, "--json", "enum-coverage", "Token"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.trim_start().starts_with('{') && s.trim_end().ends_with('}'));
    assert!(s.contains("\"command\": \"enum-coverage\""));
    assert!(
        s.contains("\"line\": "),
        "line must be a number field, not part of a file:line string:\n{}",
        s
    );
    assert!(
        !s.contains("\"line\": \""),
        "line must not be quoted:\n{}",
        s
    );
    // Balanced braces is a cheap structural check that catches a truncated or
    // double-closed document without pulling in a JSON parser.
    let opens = s.chars().filter(|c| *c == '{').count();
    let closes = s.chars().filter(|c| *c == '}').count();
    assert_eq!(opens, closes, "unbalanced braces in JSON output:\n{}", s);
}

#[test]
fn json_output_survives_a_command_with_no_findings() {
    let out = ur_stdout(&["--root", DIV, "--json", "pass-through"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\"rows\": []"), "expected an empty rows array:\n{}", s);
}

#[test]
fn json_audit_labels_every_section() {
    let out = ur()
        .args(["--root", FIXTURE, "--json", "audit"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("\"title\": \"[high] divergence"), "sections need titles:\n{}", s);
    assert!(s.contains("\"summary\": \""), "sections need summaries:\n{}", s);
}

#[test]
fn all_stdout_moves_the_summary_line_off_stderr() {
    let out = ur()
        .args(["--root", FIXTURE, "--all-stdout", "inventory"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("items)"),
        "expected the summary on stdout"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("items)"),
        "summary should not be duplicated on stderr"
    );
}

#[test]
fn row_capped_checks_announce_what_they_dropped() {
    // A capped list reads as the whole result set: "20 rows" gets treated as
    // "20 hits". The cap has to say so.
    ur().args(["--root", FIXTURE, "stringly", "--top", "1"])
        .assert()
        .success()
        .stdout(contains("note: showing 1 of"));
}

#[test]
fn the_truncation_note_survives_the_redirect_callers_actually_write() {
    // Everything on stderr is commentary a caller can discard and still hold a
    // correct answer — except this. One real session paired `2>/dev/null` with
    // `| head -N` on five of seven invocations, so the line saying the answer
    // was cut short was the first thing thrown away, and three rows of
    // thirty-seven read as the whole tree. The cut goes where the rows go.
    let out = ur()
        .args(["--root", FIXTURE, "--top", "1", "inventory"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("raise or drop --top"),
        "the cut did not survive `2>/dev/null`:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // And it stays out of the row stream a consumer parses.
    assert_eq!(rows_of(&out.stdout).len(), 1);
    assert_tsv_cols(&out.stdout, 5);
}

// ════════════════════════════════════════════════════════════════════════════
//  Divergence — regressions from the 0.1.30 field run (see impl_logs/).
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn divergence_accepts_the_all_flag_documented_in_the_quickstart() {
    // The quickstart advertised `divergence --all`; the command rejected it,
    // so the first thing an agent typed after reading help was an error.
    ur().args(["--root", FIXTURE, "divergence", "--all"])
        .assert()
        .success();
}

#[test]
fn divergence_drops_pairs_with_no_shared_variant() {
    // Two sites naming disjoint variants are a family of single-purpose fns,
    // not a disagreement. Scoring them by |lean|/|rich| gave every such pair
    // 1.00 — the top of the ranking — so the loudest rows were all noise.
    let out = ur_stdout(&["--root", FIXTURE, "divergence", "--min-score", "0.0"]);
    for line in rows_of(&out) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        let score: f64 = cols[1].parse().unwrap_or(0.0);
        assert!(
            score < 1.0,
            "a 1.00 score means |lean|/|rich|, not a real intersection: {:?}",
            line
        );
    }
}

#[test]
fn divergence_top_caps_the_ranking_not_the_scan() {
    // Capping mid-scan made `--top` a prefix by enum name: on a 170-enum tree
    // only the first six enums alphabetically were ever reachable.
    let capped = ur_stdout(&["--root", FIXTURE, "divergence", "--min-score", "0.0", "--top", "1"]);
    let full = ur_stdout(&["--root", FIXTURE, "divergence", "--min-score", "0.0"]);
    let capped_rows = rows_of(&capped);
    let full_rows = rows_of(&full);
    if !full_rows.is_empty() {
        assert_eq!(capped_rows.len(), 1, "--top 1 must yield exactly one row");
        assert_eq!(
            capped_rows[0], full_rows[0],
            "the capped row must be the globally highest-scoring one"
        );
    }
}

#[test]
fn handling_divergence_reports_one_row_per_careless_site() {
    // One careless site with N careful siblings is one decision, not N rows.
    let out = ur_stdout(&["--root", FIXTURE, "divergence", "--handling"]);
    let mut sites: Vec<String> = rows_of(&out)
        .iter()
        .filter_map(|l| l.split('\t').nth(4).map(str::to_string))
        .collect();
    let before = sites.len();
    sites.sort();
    sites.dedup();
    assert_eq!(before, sites.len(), "duplicate careless sites in output");
}

// ─── coverage: outline filters, ambiguity, conversions classes ─────────────

#[test]
fn outline_kind_filter_keeps_only_that_kind() {
    let out = ur_stdout(&["--root", FIXTURE, "outline", "src/main.rs", "--kind", "struct"]);
    let rows = rows_of(&out);
    assert!(!rows.is_empty(), "fixture main.rs defines structs");
    for r in &rows {
        assert!(r.starts_with("struct"), "non-struct row leaked through: {r}");
    }
}

#[test]
fn outline_sort_kind_groups_rows_into_a_census() {
    let out = ur_stdout(&["--root", FIXTURE, "outline", "src/main.rs", "--sort", "kind"]);
    let kinds: Vec<String> = rows_of(&out)
        .iter()
        .map(|r| r.split('\t').next().unwrap().to_string())
        .collect();
    // Grouped means each kind appears in one contiguous run.
    let mut seen: Vec<&String> = Vec::new();
    for k in &kinds {
        if seen.last() != Some(&k) {
            assert!(!seen.contains(&k), "kind `{k}` appears in two separate runs: {kinds:?}");
            seen.push(k);
        }
    }
}

#[test]
fn outline_names_every_file_an_ambiguous_suffix_matches() {
    let root = std::env::temp_dir().join("unruster_outline_ambig");
    let _ = std::fs::remove_dir_all(&root);
    for m in ["alpha", "beta"] {
        let d = root.join("src").join(m);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("util.rs"), "pub fn helper() {}\n").unwrap();
    }
    let out = ur()
        .args(["--root", root.to_str().unwrap(), "outline", "util.rs"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("matches 2 files"), "expected ambiguity note, got: {err}");
    // Both files are outlined rather than one silently winning.
    let rows = rows_of(&out.stdout);
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn conversions_class_filter_accepts_every_kind() {
    for k in [
        ".into", ".try_into", ".to_string", ".to_owned", ".to_vec", ".as_str", ".as_bytes",
        ".as_ref", ".as_mut", ".parse", ".cloned", ".copied", ".collect", "::from", "::try_from",
    ] {
        ur().args(["--root", FIXTURE, "conversions", "--class", k])
            .assert()
            .success();
    }
}

// ─── coverage: config-drift targeting, ranking, waivers ────────────────────

#[test]
fn config_drift_unknown_type_reports_and_exits_2() {
    let out = ur()
        .args(["--root", "fixtures/drift", "config-drift", "NoSuchOpts"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("NoSuchOpts"), "{err}");
}

#[test]
fn config_drift_min_score_gate_drops_everything_below_it() {
    let out = ur_stdout(&["--root", "fixtures/drift", "config-drift", "--min-score", "9999"]);
    assert!(rows_of(&out).is_empty(), "a 9999 gate must drop every row");
}

/// Two drifting types rank against each other; identical literals and
/// single-site types stay silent; a waiver retires its row.
fn drift_pair_fixture(name: &str, waiver: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let waiver_line = if waiver.is_empty() {
        String::new()
    } else {
        format!("        {waiver}\n")
    };
    std::fs::write(
        src.join("lib.rs"),
        format!(
            "pub struct A {{ pub depth: u32, pub wide: bool }}\n\
             pub struct B {{ pub max: u32, pub loud: bool }}\n\
             pub struct C {{ pub k: u32 }}\n\
             pub struct D {{ pub only: u32 }}\n\
             pub struct T(pub u32);\n\
             pub mod one {{\n\
                 pub fn a() -> crate::A {{\n\
             {waiver_line}        crate::A {{ depth: 3, wide: true }}\n\
                 }}\n\
                 pub fn b() -> crate::B {{ crate::B {{ max: 10, loud: true }} }}\n\
                 pub fn c() -> crate::C {{ crate::C {{ k: 1 }} }}\n\
                 pub fn t() -> crate::T {{ crate::T {{ 0: 1 }} }}\n\
             }}\n\
             pub mod two {{\n\
                 pub fn a() -> crate::A {{ crate::A {{ depth: 9, wide: true }} }}\n\
                 pub fn b() -> crate::B {{ crate::B {{ max: 99, loud: false }} }}\n\
                 pub fn c() -> crate::C {{ crate::C {{ k: 1 }} }}\n\
                 pub fn d() -> crate::D {{ crate::D {{ only: 7 }} }}\n\
             }}\n"
        ),
    )
    .unwrap();
    root
}

#[test]
fn config_drift_ranks_two_drifting_types_and_stays_quiet_on_agreement() {
    let root = drift_pair_fixture("unruster_drift_two", "");
    let out = ur_stdout(&["--root", root.to_str().unwrap(), "config-drift"]);
    let rows = rows_of(&out);
    let tys: Vec<&str> = rows
        .iter()
        .map(|r| r.split('\t').next().unwrap())
        .collect();
    // Both drifting types rank; the agreeing pair (C), the single-site
    // type (D), and the tuple-struct literal (T) do not.
    assert!(rows.len() >= 2, "{rows:?}");
    assert!(tys.contains(&"A") && tys.contains(&"B"), "{rows:?}");
    assert!(!tys.contains(&"C") && !tys.contains(&"D") && !tys.contains(&"T"), "{rows:?}");
}

#[test]
fn config_drift_honours_a_site_waiver() {
    let root = drift_pair_fixture(
        "unruster_drift_waived",
        "// unruster: ok(config-drift/A) 2026-01-01 — presets differ on purpose\n                 ",
    );
    let out = ur_stdout(&["--root", root.to_str().unwrap(), "config-drift"]);
    let tys: Vec<String> = rows_of(&out)
        .iter()
        .map(|r| r.split('\t').next().unwrap().to_string())
        .collect();
    assert!(!tys.contains(&"A".to_string()), "waived type still listed: {tys:?}");
    assert!(tys.contains(&"B".to_string()), "unwaived type must survive: {tys:?}");
}

// ─── coverage: waiver ledger edges ─────────────────────────────────────────

#[test]
fn waivers_with_no_ledger_says_how_to_start_one() {
    let err = ur_stderr(&["--root", FIXTURE, "waivers", "--today", TODAY]);
    assert!(err.contains("0 waiver(s)"), "{err}");
    assert!(err.contains("--suggest-waivers"), "{err}");
}

/// A scratch tree whose ledger has: a future-dated waiver, a reasonless one,
/// and one whose check differs from the other rows'.
fn waiver_edges_fixture() -> std::path::PathBuf {
    let root = std::env::temp_dir().join("unruster_waiver_edges");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "// unruster: ok(dead-code) 2030-01-01\n\
         fn unused_future() {}\n\
         // unruster: ok(dead-code) 2026-01-01 — verified: called from build.rs\n\
         fn unused_dated() {}\n\
         pub fn keep(p: &std::path::Path) {\n\
             let _ = std::fs::remove_file(p); // unruster: ok(error-swallows/let-_) 2026-01-01 — absence is fine\n\
         }\n",
    )
    .unwrap();
    root
}

#[test]
fn waiver_listing_flags_future_dates_and_missing_reasons() {
    let root = waiver_edges_fixture();
    let out = ur_stdout(&["--root", root.to_str().unwrap(), "waivers", "--today", TODAY]);
    let s = String::from_utf8_lossy(&out);
    // A date ahead of the clock renders `+Nd`, not a silent `0d`.
    assert!(
        s.lines().any(|l| l.starts_with('+')),
        "future date must announce itself:\n{s}"
    );
    assert!(s.contains("(none)"), "missing reason must render as (none):\n{s}");
}

#[test]
fn waivers_check_filter_drops_other_checks_rows() {
    let root = waiver_edges_fixture();
    let out = ur_stdout(&[
        "--root",
        root.to_str().unwrap(),
        "waivers",
        "--check",
        "dead-code",
        "--today",
        TODAY,
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("dead-code"), "{s}");
    assert!(!s.contains("error-swallows"), "other check's row leaked:\n{s}");
}

#[test]
fn a_ledger_dated_in_one_session_gets_the_herd_note() {
    let root = std::env::temp_dir().join("unruster_waiver_herd");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let body: String = (0..5)
        .map(|i| {
            format!(
                "// unruster: ok(dead-code) 2026-01-01 — case {i}\n\
                 fn unused_{i}() {{}}\n"
            )
        })
        .collect();
    std::fs::write(src.join("lib.rs"), body).unwrap();
    let err = ur_stderr(&["--root", root.to_str().unwrap(), "waivers", "--today", TODAY]);
    assert!(err.contains("dated waiver(s) carry"), "{err}");
}

#[test]
fn waivers_note_the_pair_a_group_key_would_retire() {
    let root = std::env::temp_dir().join("unruster_waiver_groupable");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub enum Node { A, B, C }\n\
         pub fn strip_rich(n: &Node) -> u8 {\n\
             match n { Node::A => 1, Node::B => 2, Node::C => 3 }\n\
         }\n\
         // unruster: ok(divergence/Node::C) 2026-01-01 — deliberate omission\n\
         // unruster: ok(enum-coverage/Node::C) 2026-01-01 — same.\n\
         pub fn strip_lean(n: &Node) -> u8 {\n\
             match n { Node::A => 1, Node::B => 2, _ => 0 }\n\
         }\n",
    )
    .unwrap();
    let err = ur_stderr(&["--root", root.to_str().unwrap(), "waivers", "--today", TODAY]);
    assert!(
        err.contains("single group key") && err.contains("ok(partial-enumeration/Node::C)"),
        "{err}"
    );
}

#[test]
fn upgrade_leaves_an_unhit_legacy_waiver_alone_and_says_why() {
    let root = std::env::temp_dir().join("unruster_upgrade_ambig");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // `helper` is called, so no check fires on its line: the legacy waiver
    // suppresses nothing and no check name can be inferred for it.
    std::fs::write(
        src.join("lib.rs"),
        "pub fn caller() -> u8 { helper() }\n\
         fn helper() -> u8 { 7 } // unruster: ok — stale note from an old sweep\n",
    )
    .unwrap();
    let before = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    let out = ur()
        .args([
            "--root",
            root.to_str().unwrap(),
            "waivers",
            "--upgrade",
            "--write",
            "--today",
            TODAY,
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    // Zero hits means no check can be named — the waiver must survive, and
    // the run must say so rather than silently skipping it.
    assert!(err.contains("not upgraded"), "{err}");
    assert!(err.contains("left alone as ambiguous"), "{err}");
    let after = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    assert_eq!(before, after, "an ambiguous waiver must not be rewritten");
}

#[test]
fn upgrade_of_a_reasonless_waiver_stamps_check_and_date_only() {
    let root = std::env::temp_dir().join("unruster_upgrade_bare");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn keep(p: &std::path::Path) {\n\
             let _ = std::fs::remove_file(p); // unruster: ok\n\
         }\n",
    )
    .unwrap();
    ur_stdout(&[
        "--root",
        root.to_str().unwrap(),
        "waivers",
        "--upgrade",
        "--write",
        "--today",
        TODAY,
    ]);
    let after = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    assert!(
        after.contains(&format!("// unruster: ok(error-swallows) {TODAY}")),
        "expected an upgraded, reasonless waiver:\n{after}"
    );
    assert!(!after.contains(" — \n"), "no dangling reason separator:\n{after}");
}

// ─── coverage: local bindings must not masquerade as item callers ──────────

/// A tree where the item `grow` is shadowed at some call sites by local
/// bindings — the false-attribution shape from a real analysis session.
fn shadow_fixture() -> std::path::PathBuf {
    let root = std::env::temp_dir().join("unruster_local_shadow");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn grow(v: &[f64], m: f64) -> Vec<f64> { v.iter().map(|x| x * m).collect() }\n\
         pub fn real_caller(h: &[f64]) -> Vec<f64> {\n\
             grow(h, 1.1)\n\
         }\n\
         pub fn closure_shadow(w: f64) -> f64 {\n\
             let grow = |lo: f64| lo + w;\n\
             grow(1.0) + grow(2.0)\n\
         }\n\
         pub fn call_before_let(h: &[f64]) -> Vec<f64> {\n\
             let out = grow(h, 2.0);\n\
             let grow = |x: f64| x;\n\
             let _ = grow(3.0);\n\
             out\n\
         }\n\
         pub fn param_shadow(grow: impl Fn(f64) -> f64) -> f64 {\n\
             grow(4.0)\n\
         }\n\
         pub fn recursive_init(h: &[f64]) -> Vec<f64> {\n\
             let grow = |v: &[f64]| grow(v, 9.0);\n\
             grow(h)\n\
         }\n",
    )
    .unwrap();
    root
}

#[test]
fn callers_demotes_calls_to_a_shadowing_local_binding() {
    let root = shadow_fixture();
    let out = ur().args(["--root", root.to_str().unwrap(), "callers", "grow"]).output().unwrap();
    assert!(out.status.success());
    let rows = rows_of(&out.stdout);
    let confidence_of = |caller: &str| -> Vec<String> {
        rows.iter()
            .filter(|r| r.starts_with(caller))
            .map(|r| r.split('\t').nth(2).unwrap().to_string())
            .collect()
    };
    // Calls through a local closure or fn param can never be the item.
    for caller in ["closure_shadow", "param_shadow"] {
        for c in confidence_of(caller) {
            assert_eq!(c, "heuristic", "{caller} must be demoted:\n{rows:?}");
        }
    }
    // A call *before* the shadowing `let` resolves to the item, and so does
    // the call inside the closure's own initializer (`let grow = |v| grow(v)`).
    for caller in ["real_caller", "call_before_let", "recursive_init"] {
        assert!(
            confidence_of(caller).contains(&"resolved".to_string()),
            "{caller} must keep a resolved site:\n{rows:?}"
        );
    }
    // The demotion announces itself rather than silently reshuffling a column.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("local"), "expected the local-binding note, got: {err}");
}

#[test]
fn min_confidence_resolved_drops_shadowed_sites() {
    let root = shadow_fixture();
    let out = ur_stdout(&[
        "--root",
        root.to_str().unwrap(),
        "callers",
        "grow",
        "--min-confidence",
        "resolved",
    ]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("closure_shadow"), "{s}");
    assert!(!s.contains("param_shadow"), "{s}");
    assert!(s.contains("real_caller"), "{s}");
}

// ── JSON shape: one key, one meaning ──────────────────────────────────────
//
// Every row in this section exists because a 200-defect evaluation of this
// tool parsed its `--json` with Python and silently got the wrong answer. Two
// separate collisions produced duplicate keys in one object; `json.loads`
// keeps the last, so findings were attributed to the wrong file and line.

const ARITH_FIXTURE: &str = "fixtures/arith";
const TEST_CRATE_FIXTURE: &str = "fixtures/testcrate";

/// The keys of one JSON row object, in order, including repeats.
///
/// Hand-scanned rather than parsed: the point of the assertion is to catch a
/// *duplicate* key, and every JSON library in the world would have already
/// dropped one by the time a test could look. Rows are emitted one per line.
fn json_row_keys(line: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
                // A string at depth 1 followed by `:` is a key.
                let mut rest = chars.clone();
                let next = rest.find(|c: &char| !c.is_whitespace());
                if depth == 1 && next == Some(':') {
                    keys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    keys
}

/// Every row object in a `--json` document, as its key list.
fn all_json_row_keys(out: &[u8]) -> Vec<Vec<String>> {
    String::from_utf8_lossy(out)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .map(json_row_keys)
        .collect()
}

fn assert_no_duplicate_keys(out: &[u8], what: &str) {
    for keys in all_json_row_keys(out) {
        let mut seen = std::collections::HashSet::new();
        for k in &keys {
            assert!(
                seen.insert(k.clone()),
                "{what}: duplicate key `{k}` in one row object — a standard parser \
                 keeps only the last: {keys:?}"
            );
        }
    }
}

#[test]
fn a_row_naming_two_sites_does_not_emit_file_twice() {
    // `divergence`, `--handling`, `conversion-pairs`, `config-drift` and
    // `builder-drift` all name two locations per row, and all five hardcoded
    // `file`/`line` for both — so the *primary* (lean) site was the one a
    // parser dropped. These are the checks the evaluation found real defects
    // with, so the corruption landed exactly on the good rows.
    let out = ur_stdout_allow_findings(&["--root", DIVGROUP, "--json", "audit"]);
    assert_no_duplicate_keys(&out, "audit --json");
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("\"vs_file\": ") && s.contains("\"vs_line\": "),
        "the second site must be namespaced by its own column:\n{s}"
    );
    assert!(
        s.contains("\"file\": ") && s.contains("\"line\": "),
        "the first site keeps the bare names every consumer is written against:\n{s}"
    );
}

#[test]
fn conversion_pairs_names_both_of_its_sites() {
    // The worst of the five: both sites were anonymous, so even a consumer
    // that kept duplicates could only tell them apart by position.
    let out = ur_stdout(&["--root", FIXTURE, "--json", "conversion-pairs"]);
    assert_no_duplicate_keys(&out, "conversion-pairs --json");
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("\"reverse_file\": "),
        "the reverse direction needs a name of its own:\n{s}"
    );
}

#[test]
fn a_context_column_does_not_collide_with_context_snippets() {
    // Nine checks emit a column *called* `context` (the enclosing item) and
    // `--context N` adds an array under the same key. Worse than the site
    // collision: the two values are different types, so a consumer reading the
    // column as a string got a list of source lines.
    for cmd in ["error-swallows", "stringly", "casts", "catch-all-arms"] {
        let out = ur_stdout(&["--root", FIXTURE, "--json", "--context", "1", cmd]);
        assert_no_duplicate_keys(&out, cmd);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("\"context_lines\": ["),
            "{cmd}: snippets need a key of their own:\n{s}"
        );
        assert!(
            s.contains("\"context\": \""),
            "{cmd}: the enclosing-item column must survive:\n{s}"
        );
    }
}

#[test]
fn audit_json_defaults_have_no_duplicate_keys_anywhere() {
    // `audit` raises `--context` for two of its sections by itself, so the
    // collision fired on a plain `audit --json` with no flags at all.
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "--json", "audit"]);
    assert_no_duplicate_keys(&out, "audit --json (defaults)");
}

#[test]
fn json_sections_name_their_check_and_its_finding_kind() {
    // The title is prose with a severity tag and an `explain:` topic in it. A
    // consumer had to regex it — which is how a `metrics` row, a whole
    // 1200-line function, gets scored as if it pointed at a line.
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "--json", "audit"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\"check\": \"metrics\""), "{s}");
    assert!(s.contains("\"check\": \"error-swallows\""), "{s}");
    // `item` says the row spans a whole function; `site` says it points at a
    // line; `pair` says the finding is the disagreement between two places.
    assert!(s.contains("\"kind\": \"item\""), "{s}");
    assert!(s.contains("\"kind\": \"site\""), "{s}");
    assert!(s.contains("\"kind\": \"pair\""), "{s}");
}

// ── audit: choosing what runs ─────────────────────────────────────────────

#[test]
fn audit_can_skip_a_check_and_says_which() {
    let out = ur_stderr(&["--root", FIXTURE, "audit", "--skip", "error-swallows,dead-code"]);
    assert!(
        out.contains("--only/--skip left out: dead-code, error-swallows"),
        "a shortened battery must name what it left out:\n{out}"
    );
    assert!(
        out.contains("check(s) of "),
        "and how many of the full battery ran:\n{out}"
    );
}

#[test]
fn audit_only_runs_exactly_what_was_named() {
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "audit", "--only", "stringly"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("## [medium] stringly"), "{s}");
    assert!(!s.contains("## [high] divergence"), "{s}");
    assert!(!s.contains("## [high] error-swallows"), "{s}");
}

#[test]
fn an_unknown_check_name_is_an_error_not_a_silent_no_op() {
    // `--skip error_swallows` that quietly skips nothing reads as a check that
    // found nothing.
    let out = ur()
        .args(["--root", FIXTURE, "audit", "--skip", "error_swallows"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown check"), "{err}");
    assert!(err.contains("Known checks:"), "{err}");
}

#[test]
fn selecting_nothing_at_all_is_an_error() {
    let out = ur()
        .args([
            "--root", FIXTURE, "audit", "--only", "stringly", "--skip", "stringly",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("selected no checks"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_highest_volume_check_is_capped_like_every_other_long_section() {
    // It was the one section that could run long with no default cap: 665 of
    // ~800 rows on a twelve-crate workspace, and the reader who gave up on it
    // gave up on the battery.
    let out = ur_stdout_allow_findings(&["--root", FIXTURE, "audit", "--only", "error-swallows"]);
    let s = String::from_utf8_lossy(&out);
    let rows = s.lines().filter(|l| l.starts_with(".") || l.starts_with("let-_")).count();
    assert!(rows <= 40, "expected the section capped at 40, got {rows} rows");
}

// ── the tier a ranked check gates on, askable ─────────────────────────────

#[test]
fn error_swallows_takes_a_min_score_like_its_ranked_siblings() {
    let all = ur_stdout(&["--root", FIXTURE, "error-swallows"]);
    let gating = ur_stdout(&["--root", FIXTURE, "error-swallows", "--min-score", "0.55"]);
    assert!(
        rows_of(&gating).len() < rows_of(&all).len(),
        "a floor must actually drop rows"
    );
    for line in rows_of(&gating) {
        let score: f64 = line.split('\t').nth(1).unwrap().parse().unwrap();
        assert!(score >= 0.55, "row below the floor survived: {line}");
    }
}

#[test]
fn a_score_floor_is_reported_not_silently_applied() {
    let err = ur_stderr(&["--root", FIXTURE, "error-swallows", "--min-score", "0.55"]);
    assert!(err.contains("below --min-score 0.55"), "{err}");
}

#[test]
fn clones_takes_a_min_score_too() {
    let out = ur().args(["--root", FIXTURE, "clones", "--min-score", "0.99"]).output().unwrap();
    assert!(out.status.success() || out.status.code() == Some(1));
    for line in rows_of(&out.stdout) {
        let score: f64 = line.split('\t').nth(1).unwrap().parse().unwrap();
        assert!(score >= 0.99, "row below the floor survived: {line}");
    }
}

// ── error-swallows: the substitution term ─────────────────────────────────

#[test]
fn a_fallback_that_substitutes_another_value_outranks_one_that_defaults() {
    // uv PR #18176 replaced `.unwrap_or_else(|_| dist.install_path.clone())`,
    // which quietly turned an absolute lockfile path into a relative one. It
    // scored 0.35 — below the gate — so the ranking buried its own true
    // positive. The fixture's `unwrap_or_else(|_| "/tmp".to_string())` is the
    // control: a constant, however many calls it is spelled with.
    let out = ur_stdout(&["--root", TEST_CRATE_FIXTURE, "--scope", "all", "error-swallows"]);
    let s = String::from_utf8_lossy(&out);
    let row = s
        .lines()
        .find(|l| l.starts_with(".unwrap_or_else"))
        .expect("fixture has one");
    let score: f64 = row.split('\t').nth(1).unwrap().parse().unwrap();
    assert!(
        score < 0.55,
        "a constant fallback is a default, not a substitution: {row}"
    );
}

// ── panics: the mirror of error-swallows ──────────────────────────────────

#[test]
fn unwrapping_a_parse_of_external_input_outranks_an_in_process_expect() {
    let out = ur_stdout(&["--root", ARITH_FIXTURE, "panics"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("port_of"), "the parse unwrap must be reported:\n{s}");
    let row = s.lines().find(|l| l.contains("port_of")).unwrap();
    let score: f64 = row.split('\t').nth(1).unwrap().parse().unwrap();
    assert!(score >= 0.55, "unwrapping a parse must gate: {row}");
    assert!(row.contains("decode"), "and be classified by what it asserted: {row}");
}

#[test]
fn a_shipped_todo_gates_on_its_own() {
    let out = ur_stdout(&["--root", ARITH_FIXTURE, "panics"]);
    let s = String::from_utf8_lossy(&out);
    let row = s.lines().find(|l| l.starts_with("todo!")).expect("fixture has one");
    let score: f64 = row.split('\t').nth(1).unwrap().parse().unwrap();
    assert!(score >= 0.55, "a shipped todo! is a crash on a reachable path: {row}");
}

#[test]
fn poisoned_lock_unwraps_are_idiomatic_and_hideable() {
    let shown = ur_stdout(&["--root", ARITH_FIXTURE, "panics"]);
    let hidden = ur_stdout(&["--root", ARITH_FIXTURE, "panics", "--hide-idiomatic"]);
    assert!(rows_of(&hidden).len() < rows_of(&shown).len());
    let err = ur_stderr(&["--root", ARITH_FIXTURE, "panics", "--hide-idiomatic"]);
    assert!(err.contains("idiomatic site(s) hidden"), "{err}");
}

#[test]
fn panics_rows_have_a_stable_column_shape() {
    let out = ur_stdout(&["--root", ARITH_FIXTURE, "panics"]);
    assert_tsv_cols(&out, 5);
}

// ── arith-drift: sibling expression divergence ────────────────────────────

#[test]
fn one_raw_operator_among_saturating_siblings_is_reported() {
    // The shape no check in the tool could see: a fix changed `+` to
    // `saturating_add` in a function whose neighbouring terms already
    // saturated. Conceptually `divergence`'s thesis, but `divergence` pairs
    // enum dispatch sites and nothing looked at expressions.
    let out = ur_stdout(&["--root", ARITH_FIXTURE, "arith-drift"]);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("corrected_age"), "{s}");
    assert!(s.starts_with("add\t0.75"), "three checked, one raw = 0.75:\n{s}");
    assert!(s.contains("saturating_add"), "the row names the sibling to compare against:\n{s}");
}

#[test]
fn a_lone_checked_call_is_not_a_convention() {
    let out = ur_stdout(&["--root", ARITH_FIXTURE, "arith-drift", "--min-score", "0.0"]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("lone_checked"), "one call is not a majority to differ from:\n{s}");
}

#[test]
fn string_concatenation_is_not_arithmetic_drift() {
    let out = ur_stdout(&["--root", ARITH_FIXTURE, "arith-drift", "--min-score", "0.0"]);
    let s = String::from_utf8_lossy(&out);
    assert!(!s.contains("\tlabel\t"), "`String + &str` has no checked sibling:\n{s}");
}

#[test]
fn an_even_split_is_below_the_audit_floor() {
    let loose = ur_stdout(&["--root", ARITH_FIXTURE, "arith-drift", "--min-score", "0.0"]);
    assert!(String::from_utf8_lossy(&loose).contains("split"));
    let tight = ur_stdout(&["--root", ARITH_FIXTURE, "arith-drift", "--min-score", "0.6"]);
    assert!(
        !String::from_utf8_lossy(&tight).contains("\tsplit\t"),
        "two different jobs in one scope must not reach the battery"
    );
}

// ── scope: test-support crates ────────────────────────────────────────────

/// Every package in `fixtures/testcrate`, as `unruster inventory` sees it.
///
/// The fixture is a four-member workspace built to pin all four verdicts:
///
/// | member | package | edge in | verdict |
/// |:--|:--|:--|:--|
/// | `prod` | `sample-prod` | none (a root) | production |
/// | `golden` | `sample-tests` | normal, from `prod` | production |
/// | `harness` | `sample-test-utils` | dev, from `prod` | test support |
/// | `fixtures` | `sample-fixtures` | normal, from `harness` | test support |
fn testcrate_modules(extra: &[&str]) -> String {
    let mut args = vec!["--root", TEST_CRATE_FIXTURE];
    args.extend_from_slice(extra);
    args.push("inventory");
    String::from_utf8_lossy(&ur_stdout(&args)).into_owned()
}

#[test]
fn a_dev_only_dependency_is_not_production_code() {
    // `crates/foo-test/src/lib.rs` is ordinary library code by every syntactic
    // measure — not under `tests/`, not named `tests.rs`, not `#[cfg(test)]`,
    // because a crate pulled in from `[dev-dependencies]` compiles normally.
    // A battery run over a real workspace reported its swallowed `env::var`s
    // as production defects.
    let prod = testcrate_modules(&[]);
    assert!(!prod.contains("Harness::new"), "{prod}");
    // …and it is still there under `--scope all`, which is the whole point of
    // classifying it rather than ignoring it.
    assert!(testcrate_modules(&["--scope", "all"]).contains("Harness::new"));
}

#[test]
fn test_support_is_transitive_through_normal_edges() {
    // `sample-fixtures` is reached only by the harness, over an ordinary
    // `[dependencies]` edge, and its name says nothing. No naming rule could
    // ever have caught it; the graph gets it for free.
    let prod = testcrate_modules(&[]);
    assert!(!prod.contains("sample_json"), "{prod}");
    assert!(testcrate_modules(&["--scope", "all"]).contains("sample_json"));
}

#[test]
fn a_normal_dependency_stays_production_however_it_is_named() {
    // `sample-tests` reads exactly like scaffolding and is listed under
    // `[dependencies]` by production code. That is hard evidence, and it beats
    // the name — this is the case the naming rule got wrong in the other
    // direction, silently dropping real production code from the scan.
    let prod = testcrate_modules(&[]);
    assert!(
        prod.contains("threshold"),
        "a normal dependency of production code must stay in scope:\n{prod}"
    );
}

#[test]
fn the_scope_note_names_the_crates_it_removed_and_why() {
    // "it was a test file" is not an explanation a reader can check by opening
    // `crates/foo-test/src/lib.rs`. Naming the crates also makes the verdict
    // falsifiable: a crate listed here that the reader knows is production is
    // a bug report, where a bare count is not.
    let err = ur_stderr(&["--root", TEST_CRATE_FIXTURE, "callers", "load"]);
    assert!(err.contains("sample-test-utils"), "{err}");
    assert!(err.contains("sample-fixtures"), "{err}");
    assert!(err.contains("[dev-dependencies]"), "{err}");
    // The one it kept must not be named as removed.
    assert!(!err.contains("sample-tests"), "{err}");
}

#[test]
fn a_lone_harness_crate_falls_back_to_its_name() {
    // Rooted straight at the harness: one manifest, nothing in the tree can
    // depend on it, so it is a root. A root is production only by *absence* of
    // a dependent, which is not evidence — so the graph declines and the name
    // rule is what removes it. Answering "production" here would silently
    // un-classify a crate the same run catches from one directory up.
    let root = format!("{TEST_CRATE_FIXTURE}/harness");
    let out = ur().args(["--root", &root, "inventory"]).output().unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!combined.contains("Harness::new"), "{combined}");
}

#[test]
fn the_note_does_not_claim_a_dependency_edge_it_never_saw() {
    // Two unrelated crates, no dependency between them, so both are roots and
    // the graph has no evidence about either. The harness is removed by its
    // *name*, and the note has to say that rather than describing a
    // `[dev-dependencies]` edge that does not exist.
    let dir = scratch("name-fallback-note");
    for (name, sub) in [("app", "app"), ("app-test-utils", "harness")] {
        std::fs::create_dir_all(dir.join(sub).join("src")).unwrap();
        std::fs::write(
            dir.join(sub).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
    }
    std::fs::write(dir.join("app/src/lib.rs"), "pub fn ship() {}\n").unwrap();
    std::fs::write(dir.join("harness/src/lib.rs"), "pub fn rig() {}\n").unwrap();

    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "callers", "ship"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("name says test support"),
        "expected the name-rule wording:\n{err}"
    );
    assert!(
        !err.contains("[dev-dependencies]"),
        "the graph saw no edge; the note must not claim one:\n{err}"
    );
}

// ── macro bodies: fewer blind spots ───────────────────────────────────────

#[test]
fn a_statement_shaped_macro_body_is_no_longer_a_blind_spot() {
    // `tokio::select! { … }` and friends fail every expression parse — a `let`
    // is not an expression, and splitting on `;` leaves half-statements — so
    // they were recorded as bodies no check could read.
    let dir = scratch("macro-block-body");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn f(v: &Vec<u8>) {\n    \
         some_macro! {\n        let n = v.len();\n        drop(n);\n    }\n}\n",
    )
    .unwrap();
    let err = ur_stderr(&["--root", dir.to_str().unwrap(), "blind-spots"]);
    assert!(
        err.contains("0 blind spot(s)") || !err.contains("blind spot(s) —"),
        "a statement-shaped body should parse now:\n{err}"
    );
}

// ── contract-drift: the implementation vs. what its callers assume ─────────

/// The fixture the blindfold tests read: one target whose body carries a
/// sentinel token that appears nowhere else, and callers that exercise the
/// disposition vocabulary.
fn contract_fixture(name: &str) -> std::path::PathBuf {
    let dir = scratch(name);
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod core {
    /// Resolve a scope. DOC_SENTINEL_ZQX.
    pub fn resolve_scope(root: &str, depth: usize) -> Result<Vec<String>, String> {
        let marker = BODY_SENTINEL_ZQX;
        if root.is_empty() {
            return Err(marker.to_string());
        }
        Ok(vec![root.to_string(); depth])
    }
    pub const BODY_SENTINEL_ZQX: &str = "x";
}

pub mod alpha {
    use crate::core::resolve_scope;
    pub fn propagates(root: &str) -> Result<usize, String> {
        let v = resolve_scope(root, 1)?;
        Ok(v.len())
    }
    pub fn asserts(root: &str) -> usize {
        resolve_scope(root, 1).unwrap().len()
    }
}

pub mod beta {
    use crate::core::resolve_scope;
    pub fn discards(root: &str) {
        let _ = resolve_scope(root, 0);
    }
    pub fn guarded(root: &str) -> usize {
        if root.is_empty() {
            return 0;
        }
        resolve_scope(root, 2).map(|v| v.len()).unwrap_or(0)
    }
    pub fn looped(roots: &[String]) {
        for r in roots {
            let _ = resolve_scope(r, 1);
        }
    }
}
"#,
    )
    .unwrap();
    dir
}

/// The load-bearing test. Everything else in this command is convenience; if
/// phase 1 can leak the body, the exercise it exists to support is worthless
/// and nothing in the output would say so.
#[test]
fn contract_drift_phase_one_never_emits_the_body() {
    let dir = contract_fixture("contract-blindfold");
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "contract-drift", "resolve_scope"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !all.contains("BODY_SENTINEL_ZQX"),
        "phase 1 leaked the implementation:\n{all}"
    );
    assert!(
        !all.contains("DOC_SENTINEL_ZQX"),
        "phase 1 leaked the doc comment, which is the *stated* contract and \
         must not contaminate the caller-derived one:\n{all}"
    );
    // The signature is contract, not implementation: withholding it would only
    // make the reader invent expectations the compiler already rules out.
    assert!(
        all.contains("Result<Vec<String>, String>"),
        "the signature must still be shown:\n{all}"
    );
}

/// A blindfold instruction on stderr is a blindfold that `2>/dev/null` removes.
#[test]
fn contract_drift_withheld_note_survives_a_stderr_redirect() {
    let dir = contract_fixture("contract-stdout-note");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "resolve_scope",
        "--no-bodies",
    ]))
    .into_owned();
    assert!(
        out.contains("withheld on purpose") && out.contains("--reveal"),
        "the instruction must be on stdout:\n{out}"
    );
}

#[test]
fn contract_drift_reveal_prints_body_and_doc_but_no_callers() {
    let dir = contract_fixture("contract-reveal");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "resolve_scope",
        "--reveal",
    ]))
    .into_owned();
    assert!(out.contains("BODY_SENTINEL_ZQX"), "no body:\n{out}");
    assert!(out.contains("DOC_SENTINEL_ZQX"), "no doc comment:\n{out}");
    assert!(
        !out.contains("## callers"),
        "phase 2 must not reprint the caller material:\n{out}"
    );
}

#[test]
fn contract_drift_classifies_return_dispositions() {
    let dir = contract_fixture("contract-ret");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "resolve_scope",
        "--no-bodies",
        "--top",
        "20",
    ]))
    .into_owned();
    for expected in ["ret:?", "ret:unwrap", "ret:discarded", "ret:chained:map"] {
        assert!(out.contains(expected), "missing {expected}:\n{out}");
    }
}

/// A precondition the caller believes it must establish, written down nowhere
/// else — not in the signature, and not anywhere a compiler checks.
#[test]
fn contract_drift_detects_a_preceding_guard() {
    let dir = contract_fixture("contract-guard");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "resolve_scope",
        "--no-bodies",
        "--top",
        "20",
    ]))
    .into_owned();
    assert!(out.contains("env:guarded"), "no guard detected:\n{out}");
    assert!(out.contains("env:loop"), "no loop detected:\n{out}");
}

#[test]
fn contract_drift_json_marks_the_body_withheld() {
    let dir = contract_fixture("contract-json");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "resolve_scope",
        "--no-bodies",
        "--json",
    ]))
    .into_owned();
    assert!(
        out.contains("\"body\": \"withheld\""),
        "a JSON consumer must be able to assert which phase this is:\n{out}"
    );
    assert!(!out.contains("BODY_SENTINEL_ZQX"), "JSON leaked the body:\n{out}");
}

/// The `--top` cut here is not the global one — these rows were chosen to be
/// unalike. A reader who thinks they are "the first N" reads the sample as the
/// population.
#[test]
fn contract_drift_top_cut_announces_itself_and_spreads_across_modules() {
    let dir = contract_fixture("contract-top");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "resolve_scope",
        "--no-bodies",
        "--top",
        "2",
    ]))
    .into_owned();
    assert!(
        out.contains("spread across modules rather than taken in file order"),
        "the cut must say what kind of cut it is:\n{out}"
    );
    assert!(
        out.contains("alpha::") && out.contains("beta::"),
        "two rows should come from two modules:\n{out}"
    );
}

/// A fn calling itself is the implementation, and phase 1 undertook not to show
/// that. Left in, the target's own argument shapes appear as caller evidence.
#[test]
fn contract_drift_excludes_recursive_call_sites() {
    let dir = scratch("contract-recursive");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn walk(n: usize) -> usize {
    if n == 0 {
        return RECUR_SENTINEL_ZQX;
    }
    walk(n - 1) + walk(n - 2)
}
pub const RECUR_SENTINEL_ZQX: usize = 1;
pub fn a() -> usize { walk(3) }
pub fn b() -> usize { walk(4) }
pub fn c() -> usize { walk(5) }
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "contract-drift", "walk", "--no-bodies"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("recursive call site(s) excluded"),
        "the exclusion must be disclosed, not silent:\n{all}"
    );
    assert!(
        all.contains("3 caller(s)"),
        "only the three real callers should count:\n{all}"
    );
}

#[test]
fn contract_drift_candidates_skips_names_it_cannot_attribute() {
    let dir = scratch("contract-candidates");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a { pub fn run() {} }
pub mod b { pub fn run() {} }
pub fn unique_target(x: usize) -> Option<usize> { Some(x) }
pub fn c1() { a::run(); b::run(); unique_target(1); }
pub fn c2() { a::run(); unique_target(2); }
pub fn c3() { b::run(); unique_target(3); }
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "contract-drift", "--candidates"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("unique_target"), "the attributable fn should rank:\n{all}");
    assert!(
        all.contains("more than one definition here"),
        "skipping same-named fns must be disclosed:\n{all}"
    );
}

/// It emits material, not findings: there is no judgment here to fail a build
/// on, and no per-site verdict to waive.
#[test]
fn contract_drift_is_not_a_gating_check() {
    let dir = contract_fixture("contract-not-gating");
    ur().args([
        "--root",
        dir.to_str().unwrap(),
        "--fail-on-findings",
        "contract-drift",
        "resolve_scope",
        "--no-bodies",
    ])
    .assert()
    .success();
    // The battery must not *run* it — checked against the sections it emits,
    // not against `--help`, whose global-flag prose legitimately names the
    // command.
    let raw = ur_stdout_allow_findings(&["--root", dir.to_str().unwrap(), "audit"]);
    let audit = String::from_utf8_lossy(&raw);
    assert!(
        !audit.contains("## contract-drift"),
        "the battery must not run an unbounded material dump:\n{audit}"
    );
}

/// The rendered type string is an IDENTITY — `index` builds an impl block's
/// `qpath` from it and `fields` stores it as `FieldDef.ty` — so two different
/// types must never render alike.
///
/// Found by `contract-drift type_to_string`: the callers imply an identity,
/// the implementation collapsed whole classes onto one spelling. `[u8; 4]` and
/// `[u8; 32]` both wrote `[u8; _]`, so two impls got the *same* qpath; every
/// `impl Trait` wrote `impl _`, every `dyn Trait` wrote `dyn _`, every fn
/// pointer wrote `fn(_)`, and `Matrix<4>`/`Matrix<32>` both wrote `Matrix<_>`.
#[test]
fn rendered_types_do_not_collide_across_distinct_types() {
    let dir = scratch("type-identity");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub trait T { fn go(&self); }
impl T for [u8; 4]  { fn go(&self) {} }
impl T for [u8; 32] { fn go(&self) {} }
pub struct Matrix<const N: usize>;
impl T for Matrix<4>  { fn go(&self) {} }
impl T for Matrix<32> { fn go(&self) {} }
pub const MAX: usize = 8;
pub struct S {
    pub small: [u8; 4],
    pub big: [u8; 32],
    pub named: [u8; MAX],
    pub shown: Box<dyn std::fmt::Display>,
    pub other: Box<dyn std::fmt::Debug + Send>,
    pub f1: fn(u8) -> u16,
    pub f2: fn(u8, u8),
    pub cb: Box<dyn Fn(u32) -> bool>,
}
"#,
    )
    .unwrap();
    let root = dir.to_str().unwrap();

    // Impl headers: four impls, four distinct qpaths.
    let impls = rows_of(&ur_stdout(&["--root", root, "impls"]));
    let headers: std::collections::BTreeSet<&str> =
        impls.iter().map(|l| l.split('\t').nth(2).unwrap()).collect();
    assert_eq!(
        headers.len(),
        4,
        "impl headers collided: {:?}",
        headers
    );

    // Field types: eight fields, eight distinct renderings.
    let fields = rows_of(&ur_stdout(&["--root", root, "fields", "S"]));
    let tys: std::collections::BTreeSet<&str> =
        fields.iter().map(|l| l.split('\t').nth(2).unwrap()).collect();
    assert_eq!(tys.len(), 8, "field types collided: {:?}", tys);

    // And the renderings are informative, not merely distinct.
    let all = tys.iter().copied().collect::<Vec<_>>().join(" | ");
    for expected in [
        "[u8; 4]",
        "[u8; 32]",
        "[u8; MAX]",
        "fn(u8) -> u16",
        "fn(u8, u8)",
    ] {
        assert!(all.contains(expected), "missing {expected} in {all}");
    }
    assert!(all.contains("Display"), "dyn bound elided: {all}");
    assert!(all.contains("Fn(u32) -> bool"), "Fn sugar elided: {all}");
}

// ── contract-drift: qualified names must name the item, not a spelling ─────

/// A call site records the callee **as written**, so a qualified query used to
/// match only the sites that spell the path out. On one real run `svg::n`
/// reported 2 callers out of 164 and said nothing about it, and eight other
/// qualified targets reported a confident zero — after which the session
/// stopped using qualified names at all.
fn qualified_fixture(name: &str) -> std::path::PathBuf {
    let dir = scratch(name);
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod svg {
    pub fn n(v: f64) -> String { format!("{}", v) }
    pub struct Svg;
    impl Svg { pub fn leaf(&self, t: &str) -> usize { t.len() } }
}
pub mod a {
    use crate::svg::n;
    pub fn one() -> String { n(1.0) }
    pub fn two() -> String { n(2.0) }
    pub fn three() -> String { n(3.0) }
}
pub mod b {
    pub fn qualified() -> String { crate::svg::n(4.0) }
    pub fn m(s: &crate::svg::Svg) -> usize { s.leaf("x") }
}
"#,
    )
    .unwrap();
    dir
}

fn caller_count(root: &str, target: &str) -> usize {
    let out = ur().args(["--root", root, "contract-drift", target, "--no-bodies"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    all.lines()
        .find_map(|l| {
            let n = l.strip_prefix('(')?;
            let n = n.split(" caller(s)").next()?;
            n.parse::<usize>().ok()
        })
        .unwrap_or_else(|| panic!("no caller count for {target}:\n{all}"))
}

#[test]
fn a_qualified_target_finds_the_same_callers_as_its_bare_form() {
    let dir = qualified_fixture("cd-qualified");
    let root = dir.to_str().unwrap();
    // Free fn: three bare `n(…)` calls plus one written `crate::svg::n(…)`.
    // The qualified form used to see only the last of the four.
    assert_eq!(caller_count(root, "svg::n"), 4);
    assert_eq!(caller_count(root, "::n"), 4);
    assert_eq!(caller_count(root, "n"), 4);
    // Method: `Type::method` used to return a confident zero.
    assert_eq!(caller_count(root, "Svg::leaf"), 1);
    assert_eq!(caller_count(root, ".leaf"), 1);
}

#[test]
fn widening_a_qualified_target_says_that_it_did() {
    let dir = qualified_fixture("cd-widen-note");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "svg::n",
        "--no-bodies",
    ]))
    .into_owned();
    assert!(
        out.contains("was matched as an item, in its own call form `::n`"),
        "a widened match must be visible, on stdout, and must name the form it \
         actually searched for:\n{out}"
    );
}

/// The fixture the qualified form genuinely cannot resolve: two fns share a
/// bare name, so widening would mix them. Then the answer must be a disclosed
/// subset, never a quiet one.
fn ambiguous_fixture(name: &str) -> std::path::PathBuf {
    let dir = scratch(name);
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a { pub fn check(x: u32) -> bool { x > 0 } }
pub mod b { pub fn check(x: u32) -> bool { x < 9 } }
pub mod u {
    pub fn q1() -> bool { crate::a::check(1) }
    pub fn q2() -> bool { crate::a::check(2) }
}
pub mod v {
    use crate::a::check;
    pub fn w1() -> bool { check(3) }
    pub fn w2() -> bool { check(4) }
    pub fn w3() -> bool { check(5) }
}
"#,
    )
    .unwrap();
    dir
}

#[test]
fn an_ambiguous_qualified_target_discloses_the_sites_it_could_not_attribute() {
    let dir = ambiguous_fixture("cd-ambiguous");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "a::check",
        "--no-bodies",
    ]))
    .into_owned();
    // Two sites spell `a::check` out; three more call a bare `check`.
    assert!(out.contains("3 further site(s)"), "{out}");
    assert!(
        out.contains("SUBSET"),
        "a sampled caller set is the one failure this command cannot survive \
         quietly — it must say so, and on stdout:\n{out}"
    );
}

/// A bare zero reads as "nobody calls this" when it means "nothing spells the
/// path out". One session took that zero at face value and went to `grep`.
#[test]
fn a_qualified_target_with_no_literal_call_sites_says_why_it_is_zero() {
    let dir = ambiguous_fixture("cd-zero-why");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root",
        dir.to_str().unwrap(),
        "contract-drift",
        "b::check",
        "--no-bodies",
    ]))
    .into_owned();
    assert!(
        out.contains("no call site spells out") && out.contains("5 site(s)"),
        "the zero must explain itself:\n{out}"
    );
}

/// Step 1's output has to be valid input to step 2. The `name` column is a
/// qpath; feeding the top candidate straight back in is the first thing anyone
/// does, and it used to return zero.
#[test]
fn candidate_names_round_trip_back_into_the_command() {
    let dir = qualified_fixture("cd-roundtrip");
    let root = dir.to_str().unwrap();
    let listing = ur_stdout(&["--root", root, "contract-drift", "--candidates", "--min-callers", "1"]);
    let names: Vec<String> = rows_of(&listing)
        .iter()
        .filter(|l| l.starts_with('0') || l.starts_with('1'))
        .filter_map(|l| l.split('\t').nth(2).map(str::to_string))
        .collect();
    assert!(!names.is_empty(), "no candidates: {}", String::from_utf8_lossy(&listing));
    for n in &names {
        assert!(
            caller_count(root, n) > 0,
            "`--candidates` offered `{n}`, which the command then cannot find callers for"
        );
    }
}

/// `callers` warned only at *zero*, so a qualified query that matched 2 of 164
/// sites looked like a complete answer. It now warns on any shortfall — but
/// only when the bare name belongs to one fn, or the note would fire on every
/// `new`, `len` and `push` in the tree and be read as wallpaper.
#[test]
fn callers_reports_a_partial_qualified_match_only_when_the_name_is_unambiguous() {
    let dir = qualified_fixture("callers-partial");
    // `svg::n` now resolves to the item, so all four sites are found and there
    // is no shortfall left to warn about — parity with `contract-drift` is the
    // point, and a warning here would mean the resolution had failed.
    let out = ur().args(["--root", dir.to_str().unwrap(), "callers", "svg::n"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stderr);
    assert!(
        s.contains("4 call site(s)"),
        "the qualified form must find every call to the item:\n{s}"
    );

    // `new` is defined on several types in the sample fixture, so the sites the
    // qualified form skipped belong to the others — the narrow answer is right.
    let noisy = ur().args(["--root", FIXTURE, "callers", "Document::new"])
        .output()
        .unwrap();
    let n = String::from_utf8_lossy(&noisy.stdout);
    assert!(
        !n.contains("call site records the callee as written"),
        "a shared name must not produce a shortfall warning:\n{n}"
    );
}

/// Widening a qualified target must keep its **call form**. Matching the bare
/// name threw it away: `trace::round` is one private free fn and no other
/// `round` is indexed — but `f64::round` is not indexed either, so a bare scan
/// claimed 65 callers across 13 modules, nearly all `.round()` on a float, and
/// marked every one `resolved`.
#[test]
fn widening_does_not_collect_same_named_methods() {
    let dir = scratch("cd-round");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod trace {
    fn round(v: f64) -> String { format!("{:.1}", v) }
    pub fn emit(a: f64, b: f64) -> String { format!("{} {}", round(a), round(b)) }
}
pub mod colour {
    pub fn straight(x: f64) -> f64 { x.round() }
    pub fn mean(v: &[f64]) -> f64 { (v[0] + v[1]).round() }
}
pub mod raster {
    pub fn overlay(x: f64) -> f64 { (x * 2.0).round() }
}
"#,
    )
    .unwrap();
    let root = dir.to_str().unwrap();
    // Two real call sites, both in `trace::emit`. The three `.round()` calls
    // are a method on `f64` and must not appear.
    assert_eq!(caller_count(root, "trace::round"), 2);
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root", root, "contract-drift", "trace::round", "--no-bodies",
    ]))
    .into_owned();
    assert!(
        !out.contains("colour::") && !out.contains("raster::"),
        "a `.round()` method call was collected as a caller:\n{out}"
    );
    // And the note must not promise more than the index can know.
    assert!(
        out.contains("would not be visible here"),
        "the widening note must not claim every same-named site is this item:\n{out}"
    );
}

/// A private fn cannot be called from another module, so a widened match that
/// lands outside its subtree is a homonym — dropped, and said out loud.
#[test]
fn widening_drops_sites_a_private_target_cannot_reach() {
    let dir = scratch("cd-vis");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod inner {
    fn helper(x: u32) -> u32 { x + 1 }
    pub fn a() -> u32 { helper(1) }
    pub fn b() -> u32 { helper(2) }
    pub mod deeper { pub fn c() -> u32 { super::helper(3) } }
}
pub mod other {
    pub fn helper(x: u32) -> u32 { x * 2 }
    pub fn d() -> u32 { helper(9) }
}
"#,
    )
    .unwrap();
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root", dir.to_str().unwrap(), "contract-drift", "inner::helper", "--no-bodies",
    ]))
    .into_owned();
    assert!(
        !out.contains("other::d"),
        "a call in a module the private target cannot reach was kept:\n{out}"
    );
}

/// Withholding the body here does not make it unreadable elsewhere. A session
/// ran phase 1 and `unruster show` as two halves of one shell command, labelled
/// the second "=== REVEAL ===", and never had a moment in which an expectation
/// could exist. The instruction has to name the bypass.
#[test]
fn the_withheld_note_names_the_ways_around_it() {
    let dir = contract_fixture("cd-bypass");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root", dir.to_str().unwrap(), "contract-drift", "resolve_scope", "--no-bodies",
    ]))
    .into_owned();
    for bypass in ["show", "sed", "cat"] {
        assert!(out.contains(bypass), "the note must name `{bypass}`:\n{out}");
    }
}

/// `row!(out, "at" => at(d, range))` parsed to `out` alone and dropped the arm,
/// so a fn called only from inside a `=>` arm was invisible to every usage
/// command — and `row!` is how every command in this tool emits. `dead-code`
/// documented the hole rather than closing it, because over-collecting is safe
/// there; `callers` had no such escape.
#[test]
fn a_call_inside_a_fat_arrow_macro_arm_is_a_call() {
    let raw = ur_stdout(&["--root", FIXTURE, "callers", "age_label"]);
    let out = String::from_utf8_lossy(&raw);
    assert!(
        out.contains("render_row"),
        "the `kv_row!(\"age\" => age_label())` site must be found:\n{out}"
    );
}

/// A bare call inside the module that defines the fn is unambiguous by Rust's
/// own scoping. Four modules here define a `score`, so `arith_drift::score`
/// reported zero callers while its own module called it four times — twenty-five
/// of this tree's fns answered zero for that reason alone.
#[test]
fn a_bare_call_in_the_defining_module_resolves_to_that_module() {
    let dir = scratch("cd-local-scope");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a {
    fn score(x: u32) -> u32 { x + 1 }
    pub fn run() -> u32 { score(1) + score(2) }
}
pub mod b {
    fn score(x: u32) -> u32 { x * 2 }
    pub fn run() -> u32 { score(3) }
}
"#,
    )
    .unwrap();
    let root = dir.to_str().unwrap();
    // Two bare calls in `a`, and `b`'s single call must not be attributed here.
    assert_eq!(caller_count(root, "a::score"), 2);
    assert_eq!(caller_count(root, "b::score"), 1);
}

/// `arg_shape` has two `.map(arg_shape)` uses and no call expression anywhere,
/// so every usage command reported a confident zero.
#[test]
fn a_fn_passed_as_a_value_is_a_use() {
    let dir = scratch("cd-fn-ref");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn widen(x: &u32) -> u64 { *x as u64 }
pub fn go(v: &[u32]) -> Vec<u64> { v.iter().map(widen).collect() }
pub fn wrapped(v: &[u32]) -> Vec<Option<u64>> {
    v.iter().map(widen).map(Some).collect()
}
"#,
    )
    .unwrap();
    let root = dir.to_str().unwrap();
    assert_eq!(caller_count(root, "widen"), 2, "both `.map(widen)` uses");
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root", root, "contract-drift", "widen", "--no-bodies",
    ]))
    .into_owned();
    assert!(
        out.contains("fn-ref:.map"),
        "the consumer names what it expects of the fn:\n{out}"
    );
    // `.map(Some)` is a constructor, not a use of anything this tree defines.
    // (The "no such fn" note is itself stdout, so assert on the count.)
    let some = ur().args(["--root", root, "callers", "Some"]).output().unwrap();
    // The count is the summary line, which lives on stderr.
    let s = String::from_utf8_lossy(&some.stderr);
    assert!(
        s.contains("0 call site(s)"),
        "constructors must not be recorded as fn references:\n{s}"
    );
}

/// `co-call` never got the item resolution `callers` and `contract-drift` have.
/// A paired-action check that silently sees no pairs is worse than one that
/// errors: `co-call emit::push_str emit::push_val` answered 0/0/0 while the
/// bare pair scored 1 both and 11 A-only.
#[test]
fn co_call_resolves_qualified_names_to_their_items() {
    let dir = scratch("cc-qualified");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod g {
    pub fn lock() {}
    pub fn unlock() {}
}
pub mod u {
    use crate::g::{lock, unlock};
    pub fn both() { lock(); unlock(); }
    pub fn only_a() { lock(); }
}
"#,
    )
    .unwrap();
    let out = ur().args([
        "--root", dir.to_str().unwrap(), "co-call", "g::lock", "g::unlock",
    ])
    .output()
    .unwrap();
    let s = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(s.contains("1 call both"), "the qualified pair must resolve:\n{s}");
    assert!(s.contains("1 call A-not-B"), "the asymmetry must be found:\n{s}");
}

/// `--top 0` capped at zero here and meant "all of it" in `contract-drift`, so
/// one flag emptied one command's output and filled another's. `--max-lines 0`
/// has always meant "all"; nothing ever wanted the literal reading, because
/// `--summary` is how you ask for no rows.
#[test]
fn top_zero_lifts_the_cap_rather_than_emptying_the_output() {
    let uncapped = rows_of(&ur_stdout(&["--root", FIXTURE, "inventory"])).len();
    let zero = rows_of(&ur_stdout(&["--root", FIXTURE, "inventory", "--top", "0"])).len();
    assert!(uncapped > 5, "fixture should have plenty of items");
    assert_eq!(zero, uncapped, "`--top 0` must lift the cap, not apply it");
}

/// The harness calls a `#[test]` fn, and the harness is in no call site. Under
/// `--scope all` — the scope every command's own note recommends — `dead-code`
/// answered with 600 rows of which every single one was a test fn.
#[test]
fn dead_code_does_not_call_the_test_suite_dead() {
    let dir = scratch("dc-tests");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn used() -> u32 { 1 }
pub fn go() -> u32 { used() }
fn genuinely_dead() -> u32 { 9 }
#[cfg(test)]
mod tests {
    #[test]
    fn a_test() { assert_eq!(super::go(), 1); }
    #[test]
    fn another_test() { assert_eq!(super::go(), 1); }
}
"#,
    )
    .unwrap();
    let out = String::from_utf8_lossy(&ur_stdout(&[
        "--root", dir.to_str().unwrap(), "dead-code", "--scope", "all",
    ]))
    .into_owned();
    assert!(out.contains("genuinely_dead"), "real dead code must survive:\n{out}");
    assert!(!out.contains("a_test"), "a #[test] fn is called by the harness:\n{out}");
    assert!(!out.contains("another_test"), "{out}");
}

/// The command whose premise is "everything that calls this" was the one usage
/// command not registered for the scope-gap warning, and had grown a private
/// note of its own that could never fire — `--scope` drops those files before
/// the scan, so nothing downstream can count what was never read.
#[test]
fn contract_drift_reports_the_scope_gap() {
    let dir = scratch("cd-scope-gap");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn target(x: u32) -> u32 { x }\npub fn a() -> u32 { target(1) }\npub fn b() -> u32 { target(2) }\n").unwrap();
    std::fs::write(dir.join("tests/it.rs"), "#[test]\nfn t() { assert_eq!(demo::target(3), 3); }\n").unwrap();
    let out = ur().args([
        "--root", dir.to_str().unwrap(), "contract-drift", "target", "--no-bodies",
    ])
    .output()
    .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("test file(s) were not scanned"),
        "the scope gap must be reported here of all places:\n{err}"
    );
}

// ── self-consistency: the tool's own registries ───────────────────────────

/// `WAIVER_AWARE_NAMES` only renders a message; `traits_of` decides behaviour.
/// This ties them together through what a user can observe, so the list cannot
/// drift from the match the way `USAGE_COMMANDS` drifted from reality — that
/// drift is why `contract-drift` never reported a scope gap.
#[test]
fn the_waiver_aware_list_matches_what_the_commands_actually_do() {
    let named = [
        "audit", "builder-drift", "casts", "clones", "config-drift",
        "conversion-pairs", "dead-code", "divergence", "enum-coverage",
        "error-swallows", "stringly", "waivers",
    ];
    for cmd in named {
        let out = ur()
            .args(["--root", FIXTURE, "--suggest-waivers", cmd])
            .output()
            .unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !all.contains("does not support waivers"),
            "`{cmd}` is listed as waiver-aware but disclaims it:\n{all}"
        );
    }
    // And the other direction: a command off the list must say so rather than
    // silently offering nothing.
    let out = ur()
        .args(["--root", FIXTURE, "--suggest-waivers", "inventory"])
        .output()
        .unwrap();
    let all = String::from_utf8_lossy(&out.stderr);
    assert!(
        all.contains("does not support waivers"),
        "a non-waiver command must disclaim, not go quiet:\n{all}"
    );
}

// ── ground truth: fixtures whose answer is known by construction ──────────
//
// The invariant suite and the token oracle both compare the tool to itself or
// to an approximation. Neither can say what the *right* number is. These can:
// each fixture is written so the caller count is a fact about the text, and the
// assertion is that exact number.
//
// This is what stops the "fixed one half of a symmetry" pattern that produced
// two of the ten defects — a corpus finds a shape only if it happens to contain
// it, and a generated fixture contains it because it was asked to.

/// One target, one fixture, one number that is true by construction.
struct GroundTruth {
    what: &'static str,
    query: &'static str,
    src: &'static str,
    /// Call sites that genuinely belong to the queried item.
    expect: usize,
}

const GROUND_TRUTH: &[GroundTruth] = &[
    GroundTruth {
        what: "a free fn reached every way there is",
        query: "m::target",
        // bare, use-imported bare, fully qualified, fn-reference, macro arm.
        src: r#"
macro_rules! kv { ($($k:literal => $v:expr),+) => { vec![$(format!("{}{}", $k, $v)),+] }; }
pub mod m {
    pub fn target(x: u32) -> u32 { x }
    pub fn near() -> u32 { target(1) }
}
pub mod n {
    use crate::m::target;
    pub fn bare() -> u32 { target(2) }
    pub fn qualified() -> u32 { crate::m::target(3) }
    pub fn as_value(v: &[u32]) -> Vec<u32> { v.iter().copied().map(target).collect() }
    pub fn in_macro() -> Vec<String> { kv!("a" => target(4)) }
}
"#,
        expect: 5,
    },
    GroundTruth {
        what: "a name four modules share resolves per module",
        query: "a::score",
        src: r#"
pub mod a {
    pub fn score(x: u32) -> u32 { x + 1 }
    pub fn run() -> u32 { score(1) + score(2) }
}
pub mod b {
    pub fn score(x: u32) -> u32 { x * 2 }
    pub fn run() -> u32 { score(3) + score(4) + score(5) }
}
pub mod c { pub fn run() -> u32 { crate::a::score(6) } }
"#,
        expect: 3,
    },
    GroundTruth {
        what: "a fn whose name std also defines",
        query: "s::write",
        src: r#"
pub mod s {
    pub fn write(x: u32) -> u32 { x }
    pub fn near() -> u32 { write(1) }
}
pub mod t {
    pub fn spill(p: &std::path::Path) { std::fs::write(p, b"x").unwrap(); }
    pub fn more(p: &std::path::Path) { std::fs::write(p, b"y").unwrap(); }
    pub fn ours() -> u32 { crate::s::write(2) }
}
"#,
        expect: 2,
    },
    GroundTruth {
        what: "a method reached by self, by Self, and by path",
        query: "V::push",
        src: r#"
pub struct V { pub items: Vec<u32> }
impl V {
    pub fn push(&mut self, x: u32) { self.items.push(x); }
    pub fn two(&mut self) { self.push(1); self.push(2); }
    pub fn indirect(v: &mut V) { V::push(v, 3); }
}
pub fn outside(v: &mut V) { v.items.push(9); }
"#,
        // `self.push(1)`, `self.push(2)`, `V::push(v, 3)`. The three
        // `self.items.push(…)` / `v.items.push(…)` are `Vec::push`.
        expect: 3,
    },
    GroundTruth {
        what: "a private fn cannot be called from another module",
        query: "p::helper",
        src: r#"
pub mod p {
    fn helper(x: u32) -> u32 { x + 1 }
    pub fn a() -> u32 { helper(1) }
    pub mod deeper { pub fn c() -> u32 { super::helper(3) } }
}
pub mod q {
    pub fn helper(x: u32) -> u32 { x * 2 }
    pub fn d() -> u32 { helper(9) }
}
"#,
        expect: 2,
    },
    GroundTruth {
        what: "recursion is the implementation, not a caller",
        query: "r::walk",
        src: r#"
pub mod r {
    pub fn walk(n: u32) -> u32 { if n == 0 { 0 } else { walk(n - 1) + walk(n - 2) } }
}
pub mod u {
    pub fn a() -> u32 { crate::r::walk(3) }
    pub fn b() -> u32 { crate::r::walk(4) }
}
"#,
        // `callers` counts the two recursive sites; `contract-drift` excludes
        // them, and says so. Both numbers are in the assertions below.
        expect: 4,
    },
];

fn ground_truth_root(idx: usize, g: &GroundTruth) -> std::path::PathBuf {
    let dir = scratch(&format!("ground-{idx}"));
    std::fs::write(dir.join("src/lib.rs"), g.src).unwrap();
    dir
}

#[test]
fn callers_matches_ground_truth_on_every_call_shape() {
    for (i, g) in GROUND_TRUTH.iter().enumerate() {
        let dir = ground_truth_root(i, g);
        // At `resolved`, which is the tier the tool claims to be sure about.
        // A method widened by name alone is explicitly a lead list — every
        // `.push()` in the tree arrives looking like a caller — so ground truth
        // is the set the tool asserts, not the set it offers for review.
        let out = ur()
            .args([
                "--root", dir.to_str().unwrap(), "callers", g.query,
                "--scope", "all", "--min-confidence", "resolved",
            ])
            .output()
            .unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let got: usize = all
            .lines()
            .find_map(|l| l.strip_prefix('(')?.split(" call site").next()?.parse().ok())
            .unwrap_or_else(|| panic!("no count for {}:\n{all}", g.query));
        assert_eq!(
            got, g.expect,
            "{} — `{}` should have {} caller(s), got {}:\n{all}",
            g.what, g.query, g.expect, got
        );
    }
}

/// `contract-drift` must agree with `callers` up to the one exclusion it
/// documents: a fn calling itself is the implementation, not evidence about it.
#[test]
fn contract_drift_matches_ground_truth_less_its_stated_exclusion() {
    for (i, g) in GROUND_TRUTH.iter().enumerate() {
        let dir = ground_truth_root(i, g);
        let root = dir.to_str().unwrap();
        let out = ur()
            .args([
                "--root", root, "contract-drift", g.query, "--no-bodies",
                "--scope", "all", "--min-confidence", "resolved",
            ])
            .output()
            .unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let got: usize = all
            .lines()
            .find_map(|l| l.strip_prefix('(')?.split(" caller(s)").next()?.parse().ok())
            .unwrap_or(0);
        let recursive: usize = all
            .split("recursive call site(s) excluded")
            .next()
            .and_then(|s| s.rsplit("(note: ").next().map(str::to_string))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        assert_eq!(
            got + recursive,
            g.expect,
            "{} — contract-drift saw {} + {} recursive, ground truth is {}:\n{all}",
            g.what, got, recursive, g.expect
        );
    }
}

/// The suite must be able to fail. A fixture with a deliberately wrong
/// expectation has to be caught, or the green ticks above mean nothing.
#[test]
fn the_ground_truth_harness_would_notice_a_wrong_answer() {
    let g = &GROUND_TRUTH[0];
    let dir = ground_truth_root(99, g);
    let out = ur()
        .args([
            "--root", dir.to_str().unwrap(), "callers", g.query,
            "--scope", "all", "--min-confidence", "resolved",
        ])
        .output()
        .unwrap();
    let all = String::from_utf8_lossy(&out.stderr);
    let got: usize = all
        .lines()
        .find_map(|l| l.strip_prefix('(')?.split(" call site").next()?.parse().ok())
        .unwrap();
    assert_ne!(got, g.expect + 1, "the harness must read a real count, not a constant");
    assert_eq!(got, g.expect);
}

// ── self-check: the checker has to be checkable ───────────────────────────

/// `--probes 0` capped the probe set at zero and the run printed five green
/// ticks over nothing — the exact result this command exists to make
/// impossible, produced by the command. `0` lifts the cap, as it does for
/// `--top` and `--max-lines`.
#[test]
fn self_check_probes_zero_widens_rather_than_empties() {
    let capped = ur_stdout(&["--root", FIXTURE, "self-check", "--probes", "2"]);
    let all = ur_stdout(&["--root", FIXTURE, "self-check", "--probes", "0"]);
    let count = |o: &[u8]| -> usize {
        rows_of(o)
            .iter()
            .filter_map(|l| l.split('\t').nth(2)?.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
    };
    assert!(
        count(&all) > count(&capped),
        "`--probes 0` must widen: capped={} all={}",
        count(&capped),
        count(&all)
    );
}

/// An invariant that examined nothing has not passed. `ok` over an empty probe
/// set is the most expensive output here, because it is indistinguishable from
/// a real result.
#[test]
fn an_invariant_with_no_probes_reports_none_not_ok() {
    let dir = scratch("sc-empty");
    // No fns at all, so every probe-driven invariant has nothing to look at.
    std::fs::write(dir.join("src/lib.rs"), "pub struct Empty;\n").unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "self-check"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("none"), "an empty probe set must not read `ok`:\n{all}");
    assert!(
        all.contains("examined nothing"),
        "and it must say so out loud:\n{all}"
    );
}

/// A local `let far = …` in one file made a never-called `far()` in another
/// look called: `dead-code`'s sink records every path expression, and the
/// oracle counts `far,` as a use. Both over-approximate the same way, so their
/// agreement is not evidence — the gating comparison needs a definite `name(`.
#[test]
fn a_variable_sharing_a_fns_name_is_not_evidence_it_was_called() {
    let dir = scratch("sc-variable");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a { pub fn far() -> u32 { 1 } }
pub mod b {
    pub fn go(near: u32) -> u32 {
        let far = 2;
        assert!(near > far, "{far}");
        far
    }
}
"#,
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "self-check", "--probes", "0"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !all.contains("dead-code-agrees-with-callers\ta::far"),
        "a variable must not be read as a call:\n{all}"
    );
    assert!(all.contains("0 violation(s)"), "{all}");
}

/// A lead is the oracle's over-approximation asking a question; a violation is
/// an invariant that does not hold. Counting the first as the second made
/// `--leads` exit 1 with 162 "failures" — a flag nobody can put in CI, and a
/// word ("FAIL") that stops meaning anything once it covers both.
#[test]
fn leads_are_reported_without_failing_the_run() {
    let dir = scratch("sc-leads");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod a { pub fn solo() -> u32 { 1 } }\npub fn go() -> u32 { a::solo() }\n",
    )
    .unwrap();
    let out = ur()
        .args(["--root", dir.to_str().unwrap(), "self-check", "--probes", "0", "--leads"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`--leads` must not gate: exit {:?}",
        out.status.code()
    );
    let all = String::from_utf8_lossy(&out.stderr);
    assert!(all.contains("leads do not fail the run"), "{all}");
}

// ── shadowed bindings are not call sites ─────────────────────────────────

/// A bare name in **argument position** is how a callback is written
/// (`.map(parse)`) and how every ordinary variable is written. The walk applied
/// its shadow check to direct calls and to nothing else, so `fn_ref` arguments
/// were recorded with `shadowed: false` hard-coded: `svggen`'s `out::path()`
/// came back with 30 callers across 7 modules at `resolved`, every row a
/// parameter, a `let` or a `match` binding, and `--candidates` ranked the fn
/// 4th of 286 on the strength of it. No confidence tier separated the fakes
/// from the one real site.
#[test]
fn a_local_binding_in_argument_position_is_not_a_call_site() {
    let out = ur()
        .args(["--root", FIXTURE, "callers", "helpers::logfile"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let notes = String::from_utf8_lossy(&out.stderr);

    // Every binding form, and each must be demoted rather than dropped: the
    // reader still learns the name occurs there.
    for shape in [
        "by_parameter",
        "by_let",
        "by_match_arm",
        "by_for_pattern",
        "by_if_let",
        "by_closure_head",
    ] {
        let row = s
            .lines()
            .find(|l| l.contains(shape))
            .unwrap_or_else(|| panic!("no row for {shape} in:\n{s}"));
        assert!(
            row.contains("heuristic"),
            "`{shape}` binds the name locally, so its row must not be `resolved`:\n{row}"
        );
    }
    assert!(
        notes.contains("name a *local* binding of `logfile`") || s.contains("name a *local* binding of `logfile`"),
        "the demotion must say why it happened:\n{notes}{s}"
    );
}

/// The other direction, which matters just as much: the fn-reference feature
/// exists because `.map(f)` is a real use, and narrowing the shadow check must
/// not cost it. A qualified path can never be captured by a local, however many
/// bindings share its last segment.
#[test]
fn a_genuine_fn_reference_survives_the_shadow_check() {
    for (target, caller) in [
        ("keep_it", "hands_over_a_fn"),
        ("helpers::spell", "hands_over_a_qualified_fn"),
        ("helpers::logfile", "calls_the_item"),
        // The iterated expression sits outside the binding the loop opens.
        ("helpers::logfile", "the_head_of_a_for_loop_is_not_shadowed"),
    ] {
        let out = ur()
            .args(["--root", FIXTURE, "callers", target, "--min-confidence", "resolved"])
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(
            s.contains(caller),
            "`{caller}` really does reference `{target}` and must survive at resolved:\n{s}"
        );
    }
}

/// `--min-confidence resolved` is the filter the docs point at, so it must be
/// the one that separates the fakes from the real site.
#[test]
fn min_confidence_resolved_drops_the_shadowed_rows() {
    let out = ur()
        .args([
            "--root", FIXTURE, "callers", "helpers::logfile",
            "--min-confidence", "resolved",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    for shape in ["by_parameter", "by_let", "by_match_arm", "by_for_pattern", "by_if_let"] {
        assert!(!s.contains(shape), "`{shape}` must be filtered out:\n{s}");
    }
    assert!(s.contains("calls_the_item"), "the real caller must remain:\n{s}");
}

/// `contract-drift` is where a fake caller set does the most damage — the whole
/// exercise is deriving a contract from the callers — so it must apply the same
/// predicate and say so in words a reader cannot miss.
#[test]
fn contract_drift_applies_the_same_shadow_check_and_names_it() {
    let out = ur()
        .args(["--root", FIXTURE, "contract-drift", "helpers::logfile", "--no-bodies"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("name a *local* binding of `logfile`"),
        "contract-drift must name the reason its rows were demoted:\n{all}"
    );
    let rows: Vec<&str> = all.lines().filter(|l| l.contains("by_parameter\t")).collect();
    assert!(
        rows.iter().all(|r| r.starts_with("heuristic")),
        "a shadowed caller row must not be `resolved`:\n{rows:?}"
    );
}

/// The invariant must be able to *fail*, which means computing the binder set a
/// different way from the walk it audits — a re-derivation would pass by
/// construction. Here it only needs to hold on a tree that exercises every
/// shape; `no-site-is-a-shadowed-binding` reporting `ok` over a non-empty probe
/// set is the assertion.
#[test]
fn self_check_audits_the_walk_against_an_independent_binder_scan() {
    let out = ur()
        .args(["--root", FIXTURE, "self-check", "--probes", "0"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let row = s
        .lines()
        .find(|l| l.contains("no-site-is-a-shadowed-binding"))
        .unwrap_or_else(|| panic!("the invariant must appear in the report:\n{s}"));
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[0], "ok", "the invariant must hold on the fixture:\n{row}");
    assert_ne!(
        cols[2], "0",
        "an invariant that examined nothing has not passed:\n{row}"
    );
}

// ── `--candidates` guards the axis that matters ──────────────────────────

/// The ranker skipped names with more than one definition *in this tree*,
/// which is the wrong axis: `svggen`'s private `geom::boolean::collect` has
/// exactly one definition there and was ranked 7th of 286 on "475 callers
/// across 40 modules" — every one of them an `Iterator::collect`. The evidence
/// is in how the sites are written, and a free fn is never called `.name()`.
#[test]
fn candidates_do_not_credit_a_free_fn_with_method_calls() {
    let out = ur()
        .args(["--root", FIXTURE, "contract-drift", "--candidates", "--min-callers", "3"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("homonyms::collect"),
        "a free `collect` evidenced only by `.collect()` must not be ranked:\n{s}"
    );
    let all = format!("{}{}", s, String::from_utf8_lossy(&out.stderr));
    assert!(
        all.contains("only on `.name()` sites"),
        "the drop must announce itself — a silent skip reads as 'no such candidate':\n{all}"
    );
}

/// The other fabricated target from the same session: `out::path()` scored 0.73
/// and ranked 4th of 286 on 30 "callers" that were all locals named `path`.
#[test]
fn candidates_do_not_credit_a_fn_with_its_own_shadowing_locals() {
    let out = ur()
        .args(["--root", FIXTURE, "contract-drift", "--candidates", "--min-callers", "3"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("helpers::logfile"),
        "a fn whose caller set is entirely local bindings must not be ranked:\n{s}"
    );
    let all = format!("{}{}", s, String::from_utf8_lossy(&out.stderr));
    assert!(
        all.contains("the name is a local binding"),
        "the drop must announce itself:\n{all}"
    );
}

/// Both guards are exclusions, so the cheapest way for them to be wrong is to
/// exclude everything. A fn with real free-call sites must still rank.
#[test]
fn candidates_still_rank_a_genuine_target() {
    let out = ur()
        .args(["--root", FIXTURE, "contract-drift", "--candidates", "--min-callers", "3"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("shadowing::consume"),
        "`consume` has seven ordinary call sites and must survive both guards:\n{s}"
    );
}

// ── a qualified name reaches an item its module glob-imports ─────────────

/// The name a reader writes from a call site. Inside `geom/boolean.rs`, `dist`
/// is spelled bare because `use super::*` brought it in, so the honest guess is
/// `geom::boolean::dist` — and the answer was "no item named", followed by six
/// near-misses in other modules with the right one not among them. Two defects
/// at once: globs were stored as the literal string `"super"`, and the
/// suggestion list keeps one row per name and picked the wrong copy.
#[test]
fn a_qualified_name_resolves_through_a_glob_import() {
    let s = ur_output_allow_2(&["--root", FIXTURE, "show", "glob_parent::globbed::reaches_the_parent"]);
    assert!(
        s.contains("glob_parent::reaches_the_parent"),
        "the resolution must name the item, not guess near it:\n{s}"
    );
}

/// One row per distinct name is right — four impls of `new` must not fill the
/// list — but choosing the row by index order threw away the only evidence
/// about which copy was meant, and said nothing about the ones it dropped.
#[test]
fn a_suggestion_prefers_the_candidate_under_the_querys_own_module() {
    let s = ur_output_allow_2(&["--root", FIXTURE, "show", "glob_parent::nested::twinned"]);
    let lines: Vec<&str> = s.lines().filter(|l| l.contains("twinned")).collect();
    let first = lines
        .iter()
        .find(|l| l.trim_start().starts_with("fn "))
        .unwrap_or_else(|| panic!("expected a suggestion row in:\n{s}"));
    assert!(
        first.contains("glob_parent::twinned"),
        "the copy sharing the query's `glob_parent` prefix must lead:\n{s}"
    );
    assert!(
        s.contains("share a name listed above and are not shown"),
        "dropping the other copy must be said out loud:\n{s}"
    );
}

// ── output-shape fixes ───────────────────────────────────────────────────

/// The sixth TSV cell of the `## target` row carried the caller count when the
/// body was withheld and the body's *line* count under `--reveal`, so a
/// consumer reading position 6 got a different quantity depending on a flag it
/// never saw. `--json` named them apart all along; the default format did not.
#[test]
fn the_target_row_keeps_one_column_set_in_both_modes() {
    let withheld = String::from_utf8(ur_stdout(&["--root", FIXTURE, "contract-drift", "shadowing::consume", "--no-bodies"])).unwrap();
    let revealed = String::from_utf8(ur_stdout(&[
        "--root", FIXTURE, "contract-drift", "shadowing::consume", "--no-bodies", "--reveal",
    ]))
    .unwrap();
    let row_of = |s: &str| -> Vec<String> {
        s.lines()
            .find(|l| l.contains("shadowing::consume") && l.starts_with("fn\t"))
            .unwrap_or_else(|| panic!("no target row in:\n{s}"))
            .split('\t')
            .map(str::to_string)
            .collect()
    };
    let (w, r) = (row_of(&withheld), row_of(&revealed));
    assert_eq!(w.len(), r.len(), "the two modes must agree on the shape:\n{w:?}\n{r:?}");
    // Whichever quantity this mode cannot answer is `—`, never a number in the
    // other one's slot and never a `0` claiming there are none.
    assert_eq!(w[6], "—", "withheld gathers no body line count:\n{w:?}");
    assert_eq!(r[5], "—", "--reveal gathers no callers:\n{r:?}");
    assert_ne!(w[5], "—", "withheld does report callers:\n{w:?}");
    assert_ne!(r[6], "—", "--reveal does report body lines:\n{r:?}");
}

/// The withheld span starts at the signature and the revealed one at the doc
/// comment, which is correct — the doc is the stated contract and is withheld
/// with the body, so a `sed` over the printed range cannot spend the exercise.
/// Correct and, until now, unsaid: the same item reported two locations with no
/// hint that the difference was deliberate.
#[test]
fn the_withheld_span_says_why_it_differs_from_show() {
    let out = ur()
        .args(["--root", FIXTURE, "contract-drift", "shadowing::consume", "--no-bodies"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("starts at its **signature**"),
        "the span difference must be stated, not left to be discovered:\n{all}"
    );
}

/// A caller with two call sites had its body printed twice, byte for byte,
/// differing only in which line carried the `>` — and under a `--max-lines` cut
/// above both, not even in that. It also spent two `--top` slots on one caller.
#[test]
fn a_caller_body_is_printed_once_however_many_sites_it_has() {
    let s = String::from_utf8(ur_stdout(&["--root", FIXTURE, "contract-drift", "shadowing::consume"])).unwrap();
    let bodies = s.matches("pub fn by_closure_head").count();
    assert!(bodies <= 1, "expected one copy of each caller body, got {bodies}:\n{s}");
    // `the_only_real_caller` is not a caller of `consume`; `by_*` all are, and
    // each must appear exactly once in the header rows.
    let headers = s.lines().filter(|l| l.starts_with("shadowing::by_parameter\t")).count();
    assert_eq!(headers, 1, "one header row per caller:\n{s}");
}

/// The truncation hint read `--max-lines 480` whether three lines were left or
/// three hundred: it was `max(shown × 2, 480)`, and the floor always won. The
/// suggested value must be the one that shows the rest.
#[test]
fn the_truncation_hint_names_the_value_that_shows_the_rest() {
    let s = String::from_utf8(ur_stdout(&["--root", FIXTURE, "show", "control_flow::loopy", "--max-lines", "3"])).unwrap();
    let hint = s
        .lines()
        .find(|l| l.contains("more line(s) to"))
        .unwrap_or_else(|| panic!("expected a truncation hint in:\n{s}"));
    // "… N more line(s) to END — `--max-lines K` for the rest"
    let end: usize = hint
        .split(" to ")
        .nth(1)
        .and_then(|t| t.split_whitespace().next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("could not read the end line from: {hint}"));
    let dropped: usize = hint
        .split_whitespace()
        .nth(1)
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("could not read the dropped count from: {hint}"));
    let suggested: usize = hint
        .split("--max-lines ")
        .nth(1)
        .and_then(|t| t.split('`').next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("could not read the suggestion from: {hint}"));
    assert!(
        suggested >= dropped + 3 && suggested <= end,
        "the hint must name a value that shows the rest and no more \
         (dropped {dropped}, ends at {end}, suggested {suggested}): {hint}"
    );
}

/// The explanation was printed before anything had been looked for, so
/// `callers Paint::Raw` said "no fn, method, or macro ... and nothing close to
/// it" and then listed the construction site it had just found. A note that
/// contradicts the rows under it teaches the reader to skip the notes.
#[test]
fn an_unknown_target_note_does_not_contradict_the_rows_below_it() {
    // A tuple variant is not an indexed fn, so `query_known` is false — and
    // the scan finds its construction site anyway. That is the exact shape
    // (`Paint::Raw`) that produced "nothing close to it" above a listed hit.
    let out = ur()
        .args(["--root", FIXTURE, "callers", "Token::Word"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let summary = String::from_utf8_lossy(&out.stderr);
    assert!(
        s.contains("Token::Word") && !summary.contains("(0 call site(s)"),
        "this target must have a hit for the test to mean anything:\n{s}{summary}"
    );
    assert!(
        !s.contains("nothing close to it") && !s.contains("Did you mean"),
        "sites were found, so the not-found explanation must not appear:\n{s}"
    );

    // The note must still fire where it is true.
    let miss = ur_output_allow_2(&["--root", FIXTURE, "callers", "no_such_name_at_all"]);
    assert!(
        miss.contains("no fn, method, or macro"),
        "a genuine miss must still be explained:\n{miss}"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// concepts / near-clones / gate / cache
//
// Every one of these sets `UNRUSTER_CACHE_DIR`. The cache is keyed by content
// hash and so cannot change an answer, but a test suite that writes into the
// developer's real `~/.unruster_cache` is rude, and one that shares a cache
// directory across parallel tests is a flake waiting to be blamed on the
// feature rather than on the test.

/// A scratch tree plus a cache directory of its own.
fn scratch_cached(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = scratch(name);
    let cache = std::env::temp_dir().join(format!("unruster-cache-{name}"));
    let _ = std::fs::remove_dir_all(&cache);
    (dir, cache)
}

fn run_in(dir: &std::path::Path, cache: &std::path::Path, args: &[&str]) -> String {
    let out = ur()
        .env("UNRUSTER_CACHE_DIR", cache)
        .args(["--root", dir.to_str().unwrap(), "--all-stdout"])
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The finding the whole noun axis exists for: one concept, three names.
#[test]
fn concepts_clusters_cognate_newtypes_over_one_primitive() {
    let (dir, cache) = scratch_cached("concepts-newtype");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod user { pub struct UserId(u64); }
pub mod order { pub struct OrderId(u64); }
pub mod owner { pub struct OwnerId(u64); }
"#,
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["concepts", "--kind", "newtype"]);
    let row = text
        .lines()
        .find(|l| l.starts_with("newtype\t"))
        .unwrap_or_else(|| panic!("expected a newtype cluster:\n{text}"));
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[2], "3", "n column: {row}");
    assert_eq!(cols[3], "id", "the shared concept word: {row}");
    assert!(row.contains("(u64)"), "the shape cell: {row}");
    assert!(row.contains("UserId") && row.contains("OwnerId"), "{row}");
}

/// The false positive the shared-word rule exists to prevent. Grouping by
/// inner type alone reports every wrapper in the tree.
#[test]
fn concepts_does_not_cluster_unrelated_newtypes() {
    let (dir, cache) = scratch_cached("concepts-unrelated");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub struct Meters(f64);\npub struct Celsius(f64);\npub struct Volts(f64);\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["concepts", "--kind", "newtype"]);
    assert!(
        !text.lines().any(|l| l.starts_with("newtype\t")),
        "three unrelated wrappers are a fact about Rust, not a finding:\n{text}"
    );
}

/// A method inside a trait impl did not choose its own signature, so two of
/// them agreeing is evidence of nothing. This was the largest false-positive
/// class the first cut produced.
#[test]
fn concepts_ignores_signatures_a_trait_dictated() {
    let (dir, cache) = scratch_cached("concepts-traitimpl");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub trait Render { fn render_frame(&self, w: &mut W) -> Res; }
pub struct A; pub struct B;
impl Render for A { fn render_frame(&self, w: &mut W) -> Res { w.a() } }
impl Render for B { fn render_frame(&self, w: &mut W) -> Res { w.b() } }
"#,
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["concepts", "--kind", "signature"]);
    assert!(
        !text.contains("render_frame"),
        "a trait's signature is not a duplicated decision:\n{text}"
    );
}

#[test]
fn concepts_row_shape_is_stable() {
    let (dir, cache) = scratch_cached("concepts-shape");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod a { pub struct UserId(u64); }\npub mod b { pub struct OrderId(u64); }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["concepts"]);
    for line in text.lines().filter(|l| l.contains('\t')) {
        assert_eq!(line.split('\t').count(), 8, "concepts row shape: {line}");
    }
}

/// The gap `clones` leaves. Two copies, one of which got a fix — `clones`
/// reports nothing, because they are no longer identical.
#[test]
fn near_clones_reports_the_pair_clones_goes_quiet_on() {
    let (dir, cache) = scratch_cached("near-clone-fix");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a {
    pub fn purge(d: &D, n: usize) -> Result<usize, E> {
        let rows = d.query("DELETE FROM users WHERE age > ?", n)?;
        let logged = d.audit("purge", rows)?;
        Ok(rows + logged)
    }
}
pub mod b {
    pub fn purge(d: &D, n: usize) -> Result<usize, E> {
        let rows = d.query("DELETE FROM orders WHERE age > ?", n)?;
        let logged = d.audit("purge", rows)?;
        Ok(rows + logged)
    }
}
"#,
    )
    .unwrap();
    let clones = run_in(&dir, &cache, &["clones"]);
    assert!(
        !clones.contains("purge"),
        "clones is EXACT and must stay quiet here — that is the point:\n{clones}"
    );
    let text = run_in(&dir, &cache, &["near-clones"]);
    let row = text
        .lines()
        .find(|l| l.starts_with("purge\t"))
        .unwrap_or_else(|| panic!("expected the near-clone pair:\n{text}"));
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[2], "1", "diffs column: {row}");
    // The delta names the drift itself, so the row is checkable without
    // opening either file.
    assert!(row.contains("users") && row.contains("orders"), "delta: {row}");
}

/// Six siblings that differ in one literal are one family. All-pairs would be
/// fifteen rows and would crowd every other check out of an `audit`.
#[test]
fn near_clones_collapses_a_family_into_a_chain() {
    let (dir, cache) = scratch_cached("near-clone-family");
    let mut src = String::new();
    for k in ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"] {
        src.push_str(&format!(
            r#"
pub fn emit_{k}(o: &mut O, v: &V) -> usize {{
    let head = o.begin("{k}");
    let body = o.write(v.payload(), head);
    o.end(head, body);
    head + body
}}
"#
        ));
    }
    std::fs::write(dir.join("src/lib.rs"), src).unwrap();
    let text = run_in(&dir, &cache, &["near-clones"]);
    let rows: Vec<&str> = text.lines().filter(|l| l.contains("emit_")).collect();
    assert_eq!(
        rows.len(),
        5,
        "a family of six is five chained rows, not fifteen pairs:\n{text}"
    );
    for r in &rows {
        let cols: Vec<&str> = r.split('\t').collect();
        assert_eq!(cols[3], "6", "family column: {r}");
    }
}

#[test]
fn near_clones_row_shape_is_stable() {
    let (dir, cache) = scratch_cached("near-clone-shape");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn a(d: &D) -> R { let x = d.q("one", 1); let y = d.p(x); d.fin(x, y) }
pub fn b(d: &D) -> R { let x = d.q("two", 1); let y = d.p(x); d.fin(x, y) }
"#,
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["near-clones", "--min-tokens", "8"]);
    for line in text.lines().filter(|l| l.contains('\t')) {
        assert_eq!(line.split('\t').count(), 10, "near-clones row shape: {line}");
    }
}

/// The strongest answer the gate has, and the one an agent most often lacks:
/// the name lives in a file it never opened.
#[test]
fn gate_reports_a_taken_name_as_a_collision() {
    let (dir, cache) = scratch_cached("gate-name");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    let text = run_in(&dir, &cache, &["gate", "UserId", "--kind", "struct"]);
    assert!(text.starts_with("collide\tname\t"), "{text}");
    assert!(text.contains("src/lib.rs"), "must say where: {text}");
}

/// The failure the gate exists for: a second name for a concept that is
/// already declared elsewhere.
#[test]
fn gate_finds_a_second_name_for_an_existing_concept() {
    let (dir, cache) = scratch_cached("gate-concept");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    let text = run_in(
        &dir,
        &cache,
        &["gate", "--snippet", "pub struct AccountId(u64);"],
    );
    assert!(text.contains("\tshape\t"), "expected a shape match:\n{text}");
    assert!(text.contains("UserId"), "{text}");
}

/// A genuinely new item must come back clean, or the gate is a speed bump.
#[test]
fn gate_passes_a_genuinely_new_item() {
    let (dir, cache) = scratch_cached("gate-clear");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    let out = ur()
        .env("UNRUSTER_CACHE_DIR", &cache)
        .args(["--root", dir.to_str().unwrap()])
        .args([
            "gate",
            "--snippet",
            "pub struct RetryPolicy { pub attempts: u8, pub backoff: u64 }",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "expected no rows: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(out.status.success(), "a clean gate exits 0");
}

/// A bodyless signature is what a person types when asking "does this exist
/// yet?", and it is not a valid Rust file.
#[test]
fn gate_accepts_a_bare_signature() {
    let (dir, cache) = scratch_cached("gate-sig");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn parse_user(s: &str) -> Result<u64, E> { s.parse().map_err(E::from) }\n",
    )
    .unwrap();
    let text = run_in(
        &dir,
        &cache,
        &["gate", "--snippet", "pub fn parse_owner(s: &str) -> Result<u64, E>"],
    );
    assert!(text.contains("\tsignature\t"), "{text}");
}

/// The pre-hoc escape hatch. There is nowhere to write a waiver for code that
/// does not exist, so the retry is the acknowledgment.
#[test]
fn gate_hook_blocks_once_then_allows_the_retry() {
    let (dir, cache) = scratch_cached("gate-hook");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    let event = format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":"{}/src/new.rs",
           "content":"pub struct UserId(u32);"}}}}"#,
        dir.display()
    );
    let call = || {
        ur()
            .env("UNRUSTER_CACHE_DIR", &cache)
            .args(["--root", dir.to_str().unwrap(), "gate", "--hook"])
            .write_stdin(event.clone())
            .output()
            .unwrap()
    };
    let first = call();
    assert_eq!(first.status.code(), Some(2), "the first collision blocks");
    let err = String::from_utf8_lossy(&first.stderr);
    assert!(err.contains("UserId"), "the model must be told what: {err}");

    let second = call();
    assert!(second.status.success(), "an identical retry goes through");
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("systemMessage"),
        "…and still says why"
    );
}

/// A `warn`-tier match must never stop an edit: a gate with no waiver to offer
/// that argues with judgment calls is one an agent routes around.
#[test]
fn gate_hook_does_not_block_on_a_soft_match() {
    let (dir, cache) = scratch_cached("gate-hook-soft");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    let event = format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":"{}/src/new.rs",
           "content":"pub struct AccountId(u64);"}}}}"#,
        dir.display()
    );
    let out = ur()
        .env("UNRUSTER_CACHE_DIR", &cache)
        .args(["--root", dir.to_str().unwrap(), "gate", "--hook"])
        .write_stdin(event)
        .output()
        .unwrap();
    assert!(out.status.success(), "a shape match warns, it does not block");
    assert!(String::from_utf8_lossy(&out.stdout).contains("AccountId"));
}

/// An event this hook has no business gating must pass through untouched.
#[test]
fn gate_hook_ignores_events_it_is_not_for() {
    let (dir, cache) = scratch_cached("gate-hook-other");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    for event in [
        r#"{"tool_name":"Bash","tool_input":{"file_path":"x.rs","content":"pub struct UserId(u8);"}}"#,
        r#"{"tool_name":"Write","tool_input":{"file_path":"notes.md","content":"pub struct UserId(u8);"}}"#,
        "not json at all",
        "",
    ] {
        let out = ur()
            .env("UNRUSTER_CACHE_DIR", &cache)
            .args(["--root", dir.to_str().unwrap(), "gate", "--hook"])
            .write_stdin(event)
            .output()
            .unwrap();
        assert!(out.status.success(), "must not block on: {event}");
    }
}

/// The cache is keyed by content hash, so it cannot change an answer. That
/// claim is worth a test rather than a paragraph.
#[test]
fn the_cache_changes_no_answer() {
    let (dir, cache) = scratch_cached("cache-agrees");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod a { pub struct UserId(u64); }\npub mod b { pub struct OrderId(u64); }\n",
    )
    .unwrap();
    // Rows only. The *summary* is allowed to differ, and must: it says how many
    // files came from the cache, which is the one honest place for a run to
    // admit where its facts came from.
    let rows = |s: String| -> Vec<String> {
        s.lines()
            .filter(|l| l.contains('\t'))
            .map(str::to_string)
            .collect()
    };
    let cold = rows(run_in(&dir, &cache, &["gate", "--snippet", "pub struct OwnerId(u64);"]));
    let warm = rows(run_in(&dir, &cache, &["gate", "--snippet", "pub struct OwnerId(u64);"]));
    assert!(!cold.is_empty(), "the fixture must produce rows");
    assert_eq!(cold, warm, "a warm cache must answer identically");

    let uncached = ur()
        .env("UNRUSTER_CACHE_DIR", &cache)
        .args(["--root", dir.to_str().unwrap(), "--all-stdout", "--no-cache"])
        .args(["gate", "--snippet", "pub struct OwnerId(u64);"])
        .output()
        .unwrap();
    assert_eq!(
        rows(String::from_utf8_lossy(&uncached.stdout).to_string()),
        cold,
        "--no-cache must answer identically too"
    );
}

/// An edit invalidates nothing — it simply has no entry. Worth asserting,
/// because "keyed by content" is the reason there is no invalidation rule to
/// get wrong.
#[test]
fn an_edited_file_is_not_answered_from_the_cache() {
    let (dir, cache) = scratch_cached("cache-edit");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    let before = run_in(&dir, &cache, &["gate", "UserId", "--kind", "struct"]);
    assert!(before.contains("collide"), "{before}");

    std::fs::write(dir.join("src/lib.rs"), "pub struct Renamed(u64);\n").unwrap();
    let after = run_in(&dir, &cache, &["gate", "UserId", "--kind", "struct"]);
    assert!(
        !after.contains("collide"),
        "the cache must not answer for bytes that are gone:\n{after}"
    );
}

#[test]
fn cache_reports_its_contents_and_clears_them() {
    let (dir, cache) = scratch_cached("cache-cmd");
    std::fs::write(dir.join("src/lib.rs"), "pub struct UserId(u64);\n").unwrap();
    run_in(&dir, &cache, &["gate", "UserId", "--kind", "struct"]);
    let listed = run_in(&dir, &cache, &["cache"]);
    assert!(listed.contains('\t'), "expected a TSV row: {listed}");
    assert!(
        listed.split('\t').nth(1).unwrap_or("0").trim() != "0",
        "expected a non-empty cache: {listed}"
    );
    let cleared = run_in(&dir, &cache, &["cache", "--clear"]);
    assert!(cleared.contains("cleared"), "{cleared}");
}

/// Both new checks have to be in the battery, or `audit` is quietly narrower
/// than its own summary line claims.
#[test]
fn audit_runs_the_noun_axis_checks() {
    let (dir, cache) = scratch_cached("audit-noun");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"
pub mod a { pub struct UserId(u64); }
pub mod b { pub struct OrderId(u64); }
pub mod c { pub struct OwnerId(u64); }
"#,
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["audit"]);
    assert!(text.contains("## [high] concepts"), "{text}");
    assert!(text.contains("## [high] near-clones"), "{text}");
    assert!(text.contains("UserId"), "the cluster must be reported: {text}");
}

// ───────────────────────────────────────────────────────────────────────────
// vocabulary / doc-drift / asserts / validation-drift

/// The registry `--only` and `--skip` validate against must list every section
/// the battery actually emits.
///
/// This caught a real defect: `near-clones` and `concepts` ran in the default
/// battery while `audit --only near-clones` answered "unknown check", because
/// `CHECKS` is hand-maintained and the two newest sections were never added.
/// Derived from `--json`, so it cannot drift again — a new section that forgets
/// the registry fails here rather than at a user's prompt.
#[test]
fn every_audit_section_is_a_name_only_and_skip_accept() {
    let (dir, cache) = scratch_cached("audit-registry");
    std::fs::write(dir.join("src/lib.rs"), "pub struct A(u8);\n").unwrap();
    let json = run_in(&dir, &cache, &["audit", "--json"]);
    let emitted: Vec<String> = json
        .lines()
        .filter_map(|l| l.trim().strip_prefix("\"check\": "))
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == ',').to_string())
        .collect();
    assert!(emitted.len() > 10, "expected a full battery, got {emitted:?}");
    for check in &emitted {
        let out = ur()
            .env("UNRUSTER_CACHE_DIR", &cache)
            .args(["--root", dir.to_str().unwrap(), "audit", "--only", check])
            .output()
            .unwrap();
        assert_ne!(
            out.status.code(),
            Some(2),
            "`audit --only {check}` is rejected, but `{check}` is a section the battery emits: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The falsifier the whole `concept(…)` design turns on: a second claimant is
/// a compile-clean, test-clean, review-clean way to split a concept in half.
#[test]
fn vocabulary_reports_two_claimants_of_one_concept() {
    let (dir, cache) = scratch_cached("vocab-dup");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod a {\n/// unruster: concept(user.id)\npub struct UserId(u64);\n}\n\
         pub mod b {\n/// unruster: concept(user.id)\npub struct Principal(u64);\n}\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["vocabulary"]);
    let dups: Vec<&str> = text.lines().filter(|l| l.starts_with("duplicate\t")).collect();
    assert_eq!(dups.len(), 2, "both claimants are named:\n{text}");
    assert!(text.contains("user.id"), "{text}");
}

/// The other half: `concepts` found the cluster, and the marker turns "these
/// resemble each other" into "this one is the home and that one drifted".
#[test]
fn vocabulary_reports_a_look_alike_of_a_declared_home() {
    let (dir, cache) = scratch_cached("vocab-undeclared");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod a {\n/// unruster: concept(user.id)\npub struct UserId(u64);\n}\n\
         pub mod b {\npub struct OwnerId(u64);\n}\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["vocabulary"]);
    assert!(text.contains("undeclared\tuser.id"), "{text}");
    assert!(text.contains("OwnerId"), "{text}");
}

/// A marker the tool quietly skips is one the author believes is working.
#[test]
fn vocabulary_reports_a_nameless_marker() {
    let (dir, cache) = scratch_cached("vocab-malformed");
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// unruster: concept()\npub struct Mystery(u64);\n",
    )
    .unwrap();
    assert!(run_in(&dir, &cache, &["vocabulary"]).contains("malformed"));
}

/// A codebase that has not adopted the vocabulary must report nothing, or the
/// feature is a wall of findings on first run and nobody keeps it on.
#[test]
fn vocabulary_is_silent_on_a_codebase_with_no_markers() {
    let (dir, cache) = scratch_cached("vocab-none");
    // Three cognate declarations across modules — the shape `concepts` gates
    // on, and therefore the shape `--coverage` reports. A *pair* in one module
    // is deliberately below the floor; see the next test.
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod a { pub struct UserId(u64); }\n\
         pub mod b { pub struct OrderId(u64); }\n\
         pub mod c { pub struct OwnerId(u64); }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["vocabulary"]);
    assert!(
        !text.lines().any(|l| l.contains('\t')),
        "expected no rows without --coverage:\n{text}"
    );
    // …and `--coverage` is how you ask the opposite question.
    assert!(run_in(&dir, &cache, &["vocabulary", "--coverage"]).contains("unclaimed"));
}

/// `--coverage` reports only what `concepts` would gate on.
///
/// At the reporting floor it listed 270 clusters on a real codebase — "mostly
/// `label()` methods and action newtypes where one concept, many declarations
/// is just Rust". Advice that long is advice nobody reads.
#[test]
fn vocabulary_coverage_reports_only_the_gating_tier() {
    let (dir, cache) = scratch_cached("vocab-floor");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub struct UserId(u64);\npub struct OrderId(u64);\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["vocabulary", "--coverage"]);
    assert!(
        !text.contains("unclaimed"),
        "a cognate pair in one module is below the gate and must not be advised on:\n{text}"
    );
}

/// The sentence that survives the refactor which removed the panic.
#[test]
fn doc_drift_reports_an_unbacked_panics_section() {
    let (dir, cache) = scratch_cached("doc-panics");
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// Adds one.\n///\n/// # Panics\n///\n/// On overflow.\n\
         pub fn inc(x: u32) -> u32 { x.wrapping_add(1) }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["doc-drift"]);
    assert!(text.starts_with("panics-doc-unbacked\t"), "{text}");
}

#[test]
fn doc_drift_reports_an_errors_section_on_an_infallible_fn() {
    let (dir, cache) = scratch_cached("doc-errors");
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// Adds one.\n///\n/// # Errors\n///\n/// Never.\npub fn inc(x: u32) -> u32 { x + 1 }\n",
    )
    .unwrap();
    assert!(run_in(&dir, &cache, &["doc-drift"]).contains("errors-doc-unbacked"));
}

/// What a rename leaves behind.
#[test]
fn doc_drift_reports_a_doc_naming_a_vanished_parameter() {
    let (dir, cache) = scratch_cached("doc-stale");
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// Splits `text` on `sep`, keeping at most `limit` pieces.\n\
         pub fn split(text: &str, sep: char) -> usize { 0 }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["doc-drift", "--names"]);
    assert!(text.contains("stale-name"), "{text}");
    assert!(text.contains("`limit`"), "{text}");
    // Off unless asked for: the class produced 205 rows on this repo before it
    // was tightened, essentially all of them wrong, so it does not run by
    // default and `audit` does not run it at all.
    assert!(!run_in(&dir, &cache, &["doc-drift"]).contains("stale-name"));
}

#[test]
fn doc_drift_row_shape_is_stable() {
    let (dir, cache) = scratch_cached("doc-shape");
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// Adds one.\n///\n/// # Errors\n///\n/// Never.\npub fn inc(x: u32) -> u32 { x + 1 }\n",
    )
    .unwrap();
    for line in run_in(&dir, &cache, &["doc-drift"]).lines().filter(|l| l.contains('\t')) {
        assert_eq!(line.split('\t').count(), 5, "doc-drift row shape: {line}");
    }
}

/// Most Rust validates with a guard, not an assert, and nothing here could see
/// one before.
#[test]
fn asserts_counts_guards_as_well_as_assertion_macros() {
    let (dir, cache) = scratch_cached("asserts-inv");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn a(n: usize) { assert!(n > 0); }\n\
         pub fn b(n: usize) -> Result<(), E> { if n == 0 { return Err(E); } Ok(()) }\n\
         pub fn c(s: &str) -> Result<u8, E> { Ok(s.parse()?) }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["asserts"]);
    assert!(text.contains("assert\t"), "{text}");
    assert!(text.contains("guard-return-err\t"), "{text}");
    // Propagation is not validation.
    assert!(!text.contains("::c"), "`?` must not count:\n{text}");
}

/// `arith-drift`'s shape, pointed at validation: one sibling that checks
/// nothing among three that do.
#[test]
fn validation_drift_finds_the_sibling_that_checks_nothing() {
    let (dir, cache) = scratch_cached("valid-drift");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub struct P;\n\
         impl P {\n\
           pub fn parse_head(s: &str) -> Result<u8, E> { if s.is_empty() { return Err(E); } Ok(1) }\n\
           pub fn parse_body(s: &str) -> Result<u8, E> { if s.is_empty() { return Err(E); } Ok(2) }\n\
           pub fn parse_tail(s: &str) -> Result<u8, E> { if s.is_empty() { return Err(E); } Ok(3) }\n\
           pub fn parse_trailer(s: &str) -> Result<u8, E> { Ok(4) }\n\
         }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["validation-drift"]);
    let row = text
        .lines()
        .find(|l| l.contains("parse_trailer"))
        .unwrap_or_else(|| panic!("expected the unchecked sibling:\n{text}"));
    assert!(row.contains("parse_head"), "the checked siblings: {row}");
    assert!(
        !text.contains("parse_head\t") || text.lines().filter(|l| l.contains('\t')).count() == 1,
        "only the unchecked one is a finding:\n{text}"
    );
}

/// A scope where nobody validates is a design, not a divergence.
#[test]
fn validation_drift_is_silent_when_no_sibling_validates() {
    let (dir, cache) = scratch_cached("valid-none");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub struct P;\n\
         impl P {\n\
           pub fn parse_head(s: &str) -> u8 { 1 }\n\
           pub fn parse_body(s: &str) -> u8 { 2 }\n\
         }\n",
    )
    .unwrap();
    let text = run_in(&dir, &cache, &["validation-drift"]);
    assert!(
        !text.lines().any(|l| l.contains('\t')),
        "expected no rows:\n{text}"
    );
}

/// Every check that honours a waiver must be a check the tool *says* honours
/// waivers.
///
/// The doc on `WAIVER_AWARE_NAMES` claimed a `waiver_names_match_traits` test
/// kept it in step with `traits_of`. That test was never written, and the two
/// drifted: `panics`, `arith-drift` and `pass-through` all consult the ledger
/// and print suggestions, while `traits_of` marked them unsupported — so
/// `unruster panics --suggest-waivers` printed "does not support waivers, so
/// --suggest-waivers has nothing to offer here" and then printed the
/// suggestion. Found by a run over a real codebase, whose notes had by then
/// concluded that two checks "don't support waivers" and told readers to write
/// a parallel `// NOTE (unruster … false positive)` comment instead.
///
/// Driven through the binary rather than over the two lists, so it fails on the
/// behaviour a reader sees rather than on a copy of it.
#[test]
fn no_check_denies_the_waiver_support_it_has() {
    let (dir, cache) = scratch_cached("waiver-support");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn f(s: &str) -> u8 { s.parse().unwrap() }\n\
         pub fn g(a: u8, b: u8) -> u8 { a.saturating_add(b) + a + b }\n\
         pub fn h(x: u8) -> u8 { f(\"1\") + x }\n",
    )
    .unwrap();
    for check in ["panics", "arith-drift", "pass-through", "dead-code", "conversion-pairs"] {
        let out = ur()
            .env("UNRUSTER_CACHE_DIR", &cache)
            .args(["--root", dir.to_str().unwrap(), "--all-stdout", "--suggest-waivers", check])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(
            !text.contains("does not support waivers"),
            "`{check}` denies waiver support, but it is in WAIVABLE_CHECKS:\n{text}"
        );
    }
}

/// …and the note, when it does fire, must list every check that qualifies —
/// a reader who takes an incomplete list at face value concludes the missing
/// ones cannot be waived.
#[test]
fn the_unsupported_note_lists_every_waivable_check() {
    let (dir, cache) = scratch_cached("waiver-list");
    std::fs::write(dir.join("src/lib.rs"), "pub struct A(u8);\n").unwrap();
    let out = ur()
        .env("UNRUSTER_CACHE_DIR", &cache)
        .args(["--root", dir.to_str().unwrap(), "--all-stdout", "--suggest-waivers", "inventory"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("does not support waivers"), "expected the note: {text}");
    // `divergence-handling` is an axis of `divergence`, not a command of its
    // own; every other waivable check name is also a command name.
    for check in ["panics", "arith-drift", "pass-through", "dead-code", "concepts", "doc-drift"] {
        assert!(text.contains(check), "note omits `{check}`:\n{text}");
    }
}
