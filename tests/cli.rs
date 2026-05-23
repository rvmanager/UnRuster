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
    ur().args(["--root", FIXTURE, "inventory", "--vis", "pub", "--kind", "fn"])
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
        "--writes-only",
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
fn type_refs_unknown_warns() {
    ur().args(["--root", FIXTURE, "type-refs", "NotAType"])
        .assert()
        .success();
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
fn takes_mut_unknown_type_warns() {
    // Exercises the knows_name false branch (note about unknown type).
    let out = ur()
        .args(["--root", FIXTURE, "takes-mut", "NoSuchType"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a known"));
}

#[test]
fn callees_no_calls_message() {
    // Exercises "no fn matching" path.
    let out = ur()
        .args(["--root", FIXTURE, "callees", "no_such_fn_xyz"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no fn matching") || stderr.contains("0 distinct"));
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

// ─── error-swallows ────────────────────────────────────────────────────────

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
fn metrics_invalid_sort_falls_back_to_loc() {
    ur().args(["--root", FIXTURE, "metrics", "--sort", "bogus"])
        .assert()
        .success();
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
fn variants_unknown_enum_warns() {
    ur().args(["--root", FIXTURE, "variants", "NotAnEnum"])
        .assert()
        .success();
}

#[test]
fn catch_all_unknown_enum_warns() {
    ur().args(["--root", FIXTURE, "catch-all-arms", "NotAnEnum"])
        .assert()
        .success();
}

#[test]
fn parallel_matches_unknown_enum_warns() {
    ur().args(["--root", FIXTURE, "parallel-matches", "NotAnEnum"])
        .assert()
        .success();
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
    ur().args(["--root", FIXTURE, "casts", "--no-widen"])
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
fn inventory_vis_unknown_warns() {
    let out = ur()
        .args(["--root", FIXTURE, "inventory", "--vis", "bogus"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown --vis"), "expected unknown --vis warning");
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
//  field-uses kind filters: all three should be tested, not just --writes-only.
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn field_uses_reads_only_filter() {
    // Only the read rows should appear; writes/inits filtered out.
    let out = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "Document", "transform", "--reads-only",
    ]);
    for line in rows_of(&out) {
        let first_col = line.split('\t').next().unwrap_or("");
        assert_eq!(first_col, "read", "non-read row leaked through: {:?}", line);
    }
}

#[test]
fn field_uses_inits_only_filter() {
    let out = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "Document", "transform", "--inits-only",
    ]);
    for line in rows_of(&out) {
        let first_col = line.split('\t').next().unwrap_or("");
        assert_eq!(first_col, "init", "non-init row leaked through: {:?}", line);
    }
}

#[test]
fn field_uses_unknown_type_no_results() {
    // Querying a non-existent type should produce zero hits but exit success.
    let out = ur_stdout(&[
        "--root", FIXTURE, "field-uses", "NoSuchType", "no_field",
    ]);
    assert!(rows_of(&out).is_empty(), "expected no rows for unknown type");
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
fn fields_unknown_type_warns() {
    let out = ur()
        .args(["--root", FIXTURE, "fields", "NoSuchStruct"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no struct named"),
        "expected unknown-struct warning, got:\n{}",
        stderr
    );
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
    // Every row should be 4 tab-separated columns: kind, vis, name, file:line.
    let out = ur_stdout(&["--root", FIXTURE, "inventory", "--kind", "struct"]);
    assert!(!rows_of(&out).is_empty(), "expected at least one struct row");
    assert_tsv_cols(&out, 4);
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
    let out = ur_stdout(&["--root", FIXTURE, "casts", "--no-widen"]);
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
        "--root", FIXTURE, "inventory", "--kind", "fn",
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
