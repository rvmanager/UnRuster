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

/// Non-blank lines of `out` as Strings.
fn rows_of(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| !l.trim().is_empty())
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
fn a_qualified_callers_query_that_finds_nothing_names_the_broader_form() {
    // `Type::method` resolves through inferred receiver types, so a receiver
    // reached through a field misses. Reporting `(0 call site(s))` made that
    // indistinguishable from a method nobody calls — and the qualified form is
    // exactly what `show`/`outline` hand you to paste back in. Two sessions
    // took the zero at face value and went to `grep`.
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
        qualified.contains("Try `callers ping`"),
        "no pointer to the broader form:\n{}",
        qualified
    );
    // The broader form does find it.
    let bare = ur_stdout(&["--root", r, "callers", "ping"]);
    assert!(!rows_of(&bare).is_empty(), "{}", String::from_utf8_lossy(&bare));

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
    // `type_to_string` writes `_` for the shapes it does not spell out
    // (`impl _`, `dyn _`, `fn(_)`, `[T; _]`). That is a deliberate elision and
    // it must stay an elision — never a panic, and never silently empty.
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
        let err = String::from_utf8_lossy(&out.stderr);
        if rows == 1 && err.contains("showing") {
            assert!(
                err.contains("raise or drop --top"),
                "{args:?} truncated without saying so: {err}"
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
    let out = ur_stdout(&["--help"]);
    let s = String::from_utf8_lossy(&out);
    let idx = s
        .lines()
        .position(|l| l.starts_with("Commands:"))
        .expect("expected a Commands: section in --help");
    assert!(
        idx < 62,
        "Commands: must appear within the first 62 help lines, found at {}",
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
        .stderr(contains("note: showing 1 of"));
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
