use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use std::path::Path;

fn glep(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("glep").unwrap();
    c.current_dir(dir);
    c
}

/// Convert a forward-slash path literal to the platform's native separator,
/// for comparing against glep's own (platform-native) path output. Glob
/// PATTERN arguments stay forward-slash (globset semantics); only expected
/// OUTPUT strings go through this.
fn p(s: &str) -> String {
    s.replace('/', std::path::MAIN_SEPARATOR_STR)
}

fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn hello_world() {}\n").unwrap();
    std::fs::write(
        dir.path().join("notes.txt"),
        "hello there\ngeneral kenobi\n",
    )
    .unwrap();
    dir
}

#[test]
fn content_search_builds_index_and_matches() {
    let dir = corpus();
    glep(dir.path())
        .arg("hello")
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:1:hello there"))
        .stdout(predicates::str::contains(p(
            "src/lib.rs:1:pub fn hello_world() {}",
        )));
    assert!(dir.path().join(".glep/postings.bin").exists());
}

#[test]
fn no_match_exits_one() {
    let dir = corpus();
    glep(dir.path()).arg("zzz_absent").assert().code(1);
}

#[test]
fn bad_pattern_exits_two() {
    let dir = corpus();
    glep(dir.path()).arg("[").assert().code(2);
}

#[test]
fn path_filter_restricts_results() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["hello", "src"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains(p("src/lib.rs").as_str()));
    assert!(!s.contains("notes.txt"));
}

#[test]
fn files_with_matches_flag() {
    let dir = corpus();
    glep(dir.path())
        .args(["-l", "hello"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
}

#[test]
fn explicit_regexp_flag_handles_reserved_words() {
    let dir = corpus();
    std::fs::write(dir.path().join("idx.txt"), "the index file\n").unwrap();
    glep(dir.path())
        .args(["-e", "index"])
        .assert()
        .success()
        .stdout(predicates::str::contains("idx.txt:1:the index file"));
}

#[test]
fn index_subcommand_builds() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    assert!(dir.path().join(".glep/manifest.bin").exists());
}

#[test]
fn status_subcommand_reports() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    glep(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("files: 2"));
}

#[test]
fn rooted_glob_does_not_cross_directories() {
    let dir = corpus();
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(
        dir.path().join("src/deep/nested.rs"),
        "pub fn hello_deep() {}\n",
    )
    .unwrap();
    glep(dir.path())
        .args(["--files", "src/*.rs"])
        .assert()
        .success()
        .stdout(p("src/lib.rs\n"));
    let out = glep(dir.path())
        .args(["-g", "src/*.rs", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains(p("src/lib.rs").as_str()));
    assert!(!s.contains("nested"));
}

#[test]
fn bare_glob_matches_at_any_depth() {
    let dir = corpus();
    glep(dir.path())
        .args(["--files", "*.rs"])
        .assert()
        .success()
        .stdout(p("src/lib.rs\n"));
}

#[test]
fn files_mode_lists_all_sorted() {
    let dir = corpus();
    glep(dir.path())
        .arg("--files")
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
}

#[test]
fn files_mode_with_glob() {
    let dir = corpus();
    glep(dir.path())
        .args(["--files", "**/*.rs"])
        .assert()
        .success()
        .stdout(p("src/lib.rs\n"));
}

#[test]
fn files_mode_sees_brand_new_file() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    std::fs::write(dir.path().join("brand_new.md"), "x").unwrap();
    glep(dir.path())
        .args(["--files", "*.md"])
        .assert()
        .success()
        .stdout("brand_new.md\n");
}

#[test]
fn files_mode_no_match_exits_one() {
    let dir = corpus();
    glep(dir.path()).args(["--files", "*.zig"]).assert().code(1);
}

#[test]
fn count_mode_prints_path_counts() {
    let dir = corpus();
    glep(dir.path())
        .args(["-c", "hello"])
        .assert()
        .success()
        .stdout(p("notes.txt:1\nsrc/lib.rs:1\n"));
    glep(dir.path()).args(["-c", "zz_absent"]).assert().code(1);
}

#[test]
fn explicit_regexp_with_path_scopes_results() {
    let dir = corpus();
    std::fs::create_dir_all(dir.path().join("other")).unwrap();
    std::fs::write(dir.path().join("other/c.txt"), "hello elsewhere\n").unwrap();
    let out = glep(dir.path())
        .args(["-e", "hello", "src"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains(p("src/lib.rs").as_str()));
    assert!(!s.contains("notes.txt"));
    assert!(!s.contains(p("other/c.txt").as_str()));
}

#[test]
fn status_reflects_live_tree() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    std::fs::write(dir.path().join("third.txt"), "x").unwrap();
    glep(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("files: 3"));
}

/// Global excludes (`~/.config/git/ignore`) must be honored identically
/// whether a sweep goes through the macOS bulk fast path or the portable
/// walker (GLEP_NO_BULK_SWEEP=1 forces the latter). This used to be an
/// in-process test that mutated the process-global HOME env var under a
/// mutex; that mutex only guarded against other tests in the same file
/// that also touched HOME, not every other test in the binary that
/// transitively reads it during a sweep, so parallel test runs could race
/// on HOME. Running each variant as its own subprocess (assert_cmd spawns
/// a real child process per Command) makes HOME/XDG_CONFIG_HOME truly
/// per-process instead of process-global, so there is nothing left to
/// race on and no mutex is needed.
#[test]
fn global_excludes_honored_identically_across_sweep_paths() {
    let dir = corpus();
    std::fs::write(dir.path().join("old.bak"), "needle_bak").unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/git")).unwrap();
    std::fs::write(home.path().join(".config/git/ignore"), "*.bak\n").unwrap();

    let run = |no_bulk: bool| {
        let mut c = glep(dir.path());
        c.args(["--files"])
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"));
        if no_bulk {
            c.env("GLEP_NO_BULK_SWEEP", "1");
        }
        let out = c.assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap()
    };
    // fresh index per variant so the sweep actually runs under each path
    std::fs::remove_dir_all(dir.path().join(".glep")).ok();
    let bulk = run(false);
    std::fs::remove_dir_all(dir.path().join(".glep")).ok();
    let walker = run(true);
    assert_eq!(bulk, walker);
    assert!(
        !bulk.contains("old.bak"),
        "global excludes must hide old.bak"
    );
}

#[test]
fn dot_slash_and_absolute_path_filters_work() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["-e", "hello", "./src"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("lib.rs"));
    assert!(!s.contains("notes.txt"));

    let abs = dir.path().join("src");
    let out2 = glep(dir.path())
        .args(["-e", "hello", abs.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s2 = String::from_utf8(out2).unwrap();
    assert!(s2.contains("lib.rs"));
    assert!(!s2.contains("notes.txt"));
}

#[test]
fn status_skipped_count_ignores_hidden_flag() {
    let dir = corpus();
    std::fs::write(dir.path().join(".hidden.txt"), "h").unwrap();
    glep(dir.path()).arg("index").assert().success();
    glep(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "skipped (binary/oversized/utf16): 0",
        ));
}

/// The whole point of --no-ignore: a plain query never sees a gitignored
/// file (indexed path, rg semantics preserved), and --no-ignore does, via
/// the live-scan bypass.
#[test]
fn default_query_does_not_find_gitignored_needle() {
    let dir = corpus();
    std::fs::write(dir.path().join(".gitignore"), "ignored.secret\n").unwrap();
    std::fs::write(dir.path().join("ignored.secret"), "needle_gitignored\n").unwrap();
    glep(dir.path()).arg("needle_gitignored").assert().code(1);
}

#[test]
fn no_ignore_finds_gitignored_needle() {
    let dir = corpus();
    std::fs::write(dir.path().join(".gitignore"), "ignored.secret\n").unwrap();
    std::fs::write(dir.path().join("ignored.secret"), "needle_gitignored\n").unwrap();
    glep(dir.path())
        .args(["--no-ignore", "needle_gitignored"])
        .assert()
        .success()
        .stdout(p("ignored.secret:1:needle_gitignored\n"));
}

#[test]
fn no_ignore_files_lists_the_ignored_file() {
    let dir = corpus();
    std::fs::write(dir.path().join(".gitignore"), "ignored.secret\n").unwrap();
    std::fs::write(dir.path().join("ignored.secret"), "x\n").unwrap();
    glep(dir.path())
        .args(["--no-ignore", "--files"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ignored.secret"));
    // The default, indexed --files listing must NOT show it.
    glep(dir.path())
        .arg("--files")
        .assert()
        .success()
        .stdout(predicates::str::contains("ignored.secret").not());
}

/// .git/.glep are hard-excluded at sweep time regardless of ignore rules
/// or the hidden flag (see walk.rs's is_hard_excluded_component); this
/// must hold even under the live-scan --no-ignore path combined with
/// --hidden, the most permissive combination glep supports.
#[test]
fn no_ignore_hidden_never_shows_dot_glep() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success(); // creates .glep/
    let out = glep(dir.path())
        .args(["--no-ignore", "--hidden", "--files"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(
        !s.contains(".glep"),
        ".glep must never appear even under --no-ignore --hidden: {s}"
    );
    assert!(
        !s.lines().any(|l| l.split('/').any(|c| c == ".git")),
        ".git must never appear even under --no-ignore --hidden: {s}"
    );
}

/// Mirrors index/mod.rs's `read_only_update_writes_nothing_and_returns_
/// fresh_paths` in spirit: a --no-ignore run must never open, update, or
/// write the index at all, so the on-disk manifest and postings must be
/// bit-identical before and after, and no delta.bin may appear.
#[test]
fn no_ignore_never_touches_the_index() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    let manifest_before = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
    let postings_before = std::fs::read(dir.path().join(".glep/postings.bin")).unwrap();

    std::fs::write(dir.path().join(".gitignore"), "ignored.secret\n").unwrap();
    std::fs::write(dir.path().join("ignored.secret"), "needle_no_ignore\n").unwrap();

    glep(dir.path())
        .args(["--no-ignore", "needle_no_ignore"])
        .assert()
        .success();
    glep(dir.path())
        .args(["--no-ignore", "--files"])
        .assert()
        .success();

    let manifest_after = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
    let postings_after = std::fs::read(dir.path().join(".glep/postings.bin")).unwrap();
    assert_eq!(
        manifest_before, manifest_after,
        "--no-ignore run must not touch the manifest bytes"
    );
    assert_eq!(
        postings_before, postings_after,
        "--no-ignore run must not touch the postings bytes"
    );
    assert!(
        !dir.path().join(".glep/delta.bin").exists(),
        "--no-ignore run must not create a delta"
    );
}

#[test]
fn no_ignore_treats_subcommand_words_as_patterns() {
    let dir = corpus();
    std::fs::write(dir.path().join("idx2.txt"), "the index word\n").unwrap();
    glep(dir.path())
        .args(["--no-ignore", "index"])
        .assert()
        .success()
        .stdout(predicates::str::contains("idx2.txt"));
    assert!(
        !dir.path().join(".glep").exists(),
        "--no-ignore must not create an index"
    );
}

#[test]
fn no_ignore_composes_with_glob_filters() {
    let dir = corpus();
    std::fs::create_dir_all(dir.path().join("vendor")).unwrap();
    std::fs::write(dir.path().join("vendor/dep.js"), "hello vendored\n").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "vendor/\n").unwrap();
    let out = glep(dir.path())
        .args(["--no-ignore", "-g", "*.js", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("dep.js"));
    assert!(!s.contains("notes.txt"));
}

#[test]
fn json_mode_emits_rg_summary_event() {
    let dir = corpus();

    // Match: exits 0, last stdout line parses as JSON with type "summary"
    // and stats.matches >= 1.
    let out = glep(dir.path())
        .args(["--json", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let last = stdout.lines().last().expect("--json produced no output");
    let summary: serde_json::Value = serde_json::from_str(last).expect("last line is valid JSON");
    assert_eq!(summary["type"], "summary");
    assert!(
        summary["data"]["stats"]["matches"].as_u64().unwrap_or(0) >= 1,
        "expected stats.matches >= 1, got {summary}"
    );

    // No match: rg still emits a summary event even when nothing matched
    // (verified against real rg 15.1.0 before writing this test), and glep
    // still exits 1, same as its existing no-match convention.
    let out = glep(dir.path())
        .args(["--json", "zzz_absent_zz"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let last = stdout
        .lines()
        .last()
        .expect("--json produced no output on no-match");
    let summary: serde_json::Value = serde_json::from_str(last).expect("last line is valid JSON");
    assert_eq!(summary["type"], "summary");
}

#[test]
fn stats_block_appended() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["--stats", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("2 matches\n"));
    assert!(s.contains("2 matched lines\n"));
    assert!(s.contains("2 files contained matches\n"));
    assert!(s.contains("files searched\n"));
    assert!(s.contains("seconds total\n"));
    // and under -c the counters are exact (occurrences vs lines)
    let out = glep(dir.path())
        .args(["-c", "--stats", "o"]) // 'o' occurs twice on line 1 of notes.txt
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("notes.txt:2")); // hello + Kenobi
}

#[test]
fn encoding_flag_transcodes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lat.txt"), b"caf\xe9 test\n").unwrap();
    std::fs::write(dir.path().join("a.txt"), "caf ascii\n").unwrap();
    let out = glep(dir.path())
        .args(["-E", "latin1", "café"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("lat.txt:1:café test"));
    // 'café' (decoded side) does not appear in a.txt's "caf ascii"
    assert!(!s.contains("a.txt"));
    // unknown label errors
    glep(dir.path())
        .args(["-E", "bogus-enc", "x"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn multiple_e_patterns_union() {
    let dir = corpus();
    // notes.txt has "hello"+"Kenobi"; lib.rs has "hello" only.
    glep(dir.path())
        .args(["-e", "there", "-e", "kenobi", "-l"])
        .assert()
        .success()
        .stdout("notes.txt\n");
    glep(dir.path())
        .args(["-e", "there", "-e", "hello_world", "-l"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
    // -F with multiple -e: each arm is a literal, metachars don't parse.
    glep(dir.path())
        .args(["-F", "-e", "() {}", "-e", "kenobi", "-l"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
    // -e makes positionals paths
    glep(dir.path())
        .args(["-e", "hello", "-l", "src"])
        .assert()
        .success()
        .stdout(p("src/lib.rs\n"));
}

#[cfg(unix)]
#[test]
fn follow_reaches_symlinked_dirs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real_target.txt"), "linktok real\n").unwrap();
    std::fs::create_dir_all(dir.path().join("realdir")).unwrap();
    std::fs::write(dir.path().join("realdir/inner.txt"), "linktok inner\n").unwrap();
    std::os::unix::fs::symlink("realdir", dir.path().join("linkdir")).unwrap();
    std::os::unix::fs::symlink("real_target.txt", dir.path().join("linkfile.txt")).unwrap();

    // default: links are not descended
    let out = glep(dir.path()).args(["-l", "linktok"]).assert().success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("realdir/inner.txt"));
    assert!(s.contains("real_target.txt"));
    assert!(!s.contains("linkdir"));

    // -L: linkdir path produces results through the link
    let out = glep(dir.path())
        .args(["-L", "-l", "linktok"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("linkdir/inner.txt"), "{s}");
    assert!(s.contains("linkfile.txt"), "{s}");
}

#[test]
fn engine_rejects_unknown() {
    let dir = corpus();
    glep(dir.path())
        .args(["--engine", "default", "hello"])
        .assert()
        .success();
    glep(dir.path())
        .args(["--engine", "bogus", "hello"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn ignore_file_filters_results() {
    let dir = corpus();
    let extra = dir.path().join("extra.ignore");
    std::fs::write(&extra, "src/\n").unwrap();
    let out = glep(dir.path())
        .args(["--ignore-file", extra.to_str().unwrap(), "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("notes.txt"));
    assert!(!s.contains("lib.rs"));
}

#[test]
fn pattern_file_unions_with_positional() {
    let dir = corpus();
    std::fs::write(dir.path().join("pats.txt"), "hello\nworld\n").unwrap();
    let out = glep(dir.path())
        .args(["-f", "pats.txt", "-l"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("notes.txt"), "{s}");
    assert!(s.contains(p("src/lib.rs").as_str()), "{s}");
    // -f + positional-as-path: 'src' is a path since -f is present
    let out = glep(dir.path())
        .args(["-f", "pats.txt", "-l", "src"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(s, p("src/lib.rs\n"));
}

#[test]
fn files_without_match_lists_non_matching() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["--files-without-match", "hello_world"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("notes.txt"), "{s}");
    assert!(!s.contains("lib.rs"), "{s}");
}

#[test]
fn require_git_outside_repo_live_scans_gitignored() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(dir.path().join("x.log"), "hello log\n").unwrap();
    std::fs::write(dir.path().join("x.txt"), "hello txt\n").unwrap();
    // no .git: gitignore is inert under --require-git -> x.log matches
    let out = glep(dir.path())
        .args(["--require-git", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("x.log"), "{s}");
    // without the flag, .gitignore applies and x.log is skipped
    let out = glep(dir.path()).args(["-l", "hello"]).assert().success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!s.contains("x.log"), "{s}");
}

#[test]
fn unrestricted_count_maps_to_flags() {
    let dir = corpus();
    std::fs::write(dir.path().join(".gitignore"), "x.log\n").unwrap();
    std::fs::write(dir.path().join("x.log"), "hello log\n").unwrap();
    std::fs::write(dir.path().join(".hid.txt"), "hello hid\n").unwrap();
    // -u: no-ignore (log found, hidden not)
    let out = glep(dir.path())
        .args(["-u", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("x.log"), "{s}");
    assert!(!s.contains(".hid.txt"), "{s}");
    // -uu: + hidden
    let out = glep(dir.path())
        .args(["-uu", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains(".hid.txt"), "{s}");
}

#[test]
fn null_data_searches_binaryish_files() {
    let dir = tempfile::tempdir().unwrap();
    // NUL-separated records: 'needle' on NUL-records 1 and 3
    std::fs::write(
        dir.path().join("data.bin"),
        "aa\x00needle\x00bb\x00needle x\x00",
    )
    .unwrap();
    let out = glep(dir.path())
        .args(["--null-data", "needle"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("data.bin:2:needle"), "{s}");
    assert!(s.contains("data.bin:4:needle x"), "{s}");
}

#[test]
fn custom_separators() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["--field-match-separator", "|", "-A1", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("notes.txt|1|hello"), "{s}");
    // --context-separator replaces the between-file '--'
    let out = glep(dir.path())
        .args(["--context-separator", "==", "-A1", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("\n==\n"), "{s}");
}

#[test]
fn pcre2_lookaround_and_backref() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("p.txt"), "fooxbar\nfooy\nlook(ahead)\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "aa bb aa\ncc dd\n").unwrap();
    // lookahead: foo followed by x
    let out = glep(dir.path())
        .args(["-P", "foo(?=x)", "-l"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("p.txt"), "{s}");
    assert!(!s.contains("b.txt"), "{s}");
    // backreference: repeated word
    let out = glep(dir.path())
        .args(["-P", "(\\w+) \\w+ \\1", "-l"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("b.txt"), "{s}");
    // invalid under -P surfaces an error (not a silent miss)
    glep(dir.path()).args(["-P", "("]).assert().failure();
}

#[test]
fn search_zip_decompresses_gz() {
    let dir = tempfile::tempdir().unwrap();
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(b"zipneedle packed\n").unwrap();
    std::fs::write(dir.path().join("pack.gz"), enc.finish().unwrap()).unwrap();
    std::fs::write(dir.path().join("plain.txt"), "zipneedle plain\n").unwrap();
    let out = glep(dir.path())
        .args(["-z", "-l", "zipneedle"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("pack.gz"), "{s}");
    assert!(s.contains("plain.txt"), "{s}");
    // without -z the compressed file's raw bytes don't match
    let out = glep(dir.path())
        .args(["-l", "zipneedle"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!s.contains("pack.gz"), "{s}");
}

#[test]
fn no_ignore_parent_toggles_parent_rules() {
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("a.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join(".ignore"), "a.txt\n").unwrap();
    // parent .ignore hides a.txt by default
    glep(&proj).args(["-l", "hello"]).assert().failure();
    // --no-ignore-parent disables it -> live scan finds the file
    let out = glep(&proj)
        .args(["--no-ignore-parent", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("a.txt"), "{s}");
}

/// -uuu is rg's --no-ignore --hidden --binary: binary files are searched
/// and reported with a notice, never printed as raw text.
#[test]
fn uuu_searches_binary_files_like_binary() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.bin"), b"aa\x00hello\x00zz\n").unwrap();
    let out = glep(dir.path())
        .args(["-uuu", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("b.bin"), "{s}");
    let out = glep(dir.path()).args(["-uuu", "hello"]).assert().success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("binary file matches"), "{s}");
    assert!(!s.contains('\0'), "{s:?}");
}

#[test]
fn max_columns_preview_marks_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(200) + "hello" + &"y".repeat(200);
    std::fs::write(dir.path().join("long.txt"), format!("{long}\n")).unwrap();
    let out = glep(dir.path())
        .args(["-M", "50", "--max-columns-preview", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("[... omitted end of long line]"), "{s}");
    // without the preview flag the marker is absent
    let out = glep(dir.path())
        .args(["-M", "50", "hello"])
        .assert()
        .success();
    let s2 = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!s2.contains("omitted end"), "{s2}");
}

#[test]
fn type_list_and_add() {
    let dir = corpus();
    // --type-list prints the type table and exits (no index touched)
    let out = glep(dir.path()).arg("--type-list").assert().success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("rust:"), "{s}");
    // --type-add registers a new glob for -t
    std::fs::write(dir.path().join("m.weird"), "hello weird\n").unwrap();
    let out = glep(dir.path())
        .args(["--type-add", "weird:*.weird", "-t", "weird", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("m.weird"), "{s}");
}

#[test]
fn pretty_and_compat_aliases() {
    let dir = corpus();
    // -p forces color+heading+line-numbers
    let out = glep(dir.path()).args(["-p", "hello"]).assert().success();
    let s = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(s.contains("\x1b["), "{s:?}");
    // --passthrough == --passthru, --print0 == -0, --sort-files == --sort path
    glep(dir.path())
        .args(["--passthrough", "hello"])
        .assert()
        .success();
    let out = glep(dir.path())
        .args(["--print0", "-l", "hello"])
        .assert()
        .success();
    assert!(out.get_output().stdout.contains(&0));
    glep(dir.path())
        .args(["--sort-files", "-l", "hello"])
        .assert()
        .success();
}

#[test]
fn pcre2_version_flag() {
    let out = glep(&std::env::current_dir().unwrap())
        .arg("--pcre2-version")
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.starts_with("PCRE2 "), "{s}");
    assert!(s.contains("available"), "{s}");
}

#[test]
fn glob_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.MD"), "hello\n").unwrap();
    // -g '*.md' without --glob-case-insensitive misses .MD
    glep(dir.path())
        .args(["-g", "*.md", "-l", "hello"])
        .assert()
        .failure();
    let out = glep(dir.path())
        .args(["-g", "*.md", "--glob-case-insensitive", "-l", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("README.MD"), "{s}");
}

#[test]
fn replace_rewrites_matched_text() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["-r", "[$0]", "hello"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("[hello]"), "{s}");
}

// `cat`/`echo` as preprocessors: Unix-only commands.
#[cfg(unix)]
#[test]
fn pre_searches_command_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "nothing\n").unwrap();
    // preprocessor output replaces the file's contents as the search stream
    let out = glep(dir.path())
        .args(["--pre", "cat", "nothing"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("a.txt"), "{s}");
    let out = glep(dir.path())
        .args(["--pre", "echo", "txt"]) // `echo a.txt` -> "a.txt" ~ "txt"
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("a.txt"), "{s}");
}

#[test]
fn generate_completions_and_man() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["--generate", "complete-bash"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("_glep"), "{s}");
    let out = glep(dir.path())
        .args(["--generate", "man"])
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains(".TH glep"), "{s}");
}

/// The output modes are last-wins, like rg's search modes: `-l --json`
/// is a JSON search and `--json -l` lists paths. (-r still conflicts
/// with --json: rg 15's JSON carries replacements, grep-printer 0.2's
/// JSON printer cannot.)
#[test]
fn output_modes_are_last_wins() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["-l", "--json", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("\"type\":\"summary\""), "{s}");
    glep(dir.path())
        .args(["--json", "-l", "hello"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
    glep(dir.path())
        .args(["-r", "x", "--json", "hello"])
        .assert()
        .code(2);
}

/// Binary files are listed by --files and searched under -a/--binary, but
/// the default quit detection suppresses them entirely.
#[test]
fn binary_files_suppressed_by_default() {
    let dir = corpus();
    std::fs::write(dir.path().join("blob.bin"), b"aa\x00hello-bin\x00zz\n").unwrap();
    // Default: no output, exit 1: binary data suppresses the file.
    glep(dir.path()).args(["hello-bin"]).assert().code(1);
    // -a searches it as text and prints the raw line.
    glep(dir.path())
        .args(["-a", "hello-bin"])
        .assert()
        .success()
        .stdout(predicates::str::contains("blob.bin:1:aa"));
    // --binary prints the notice rather than the matched line.
    glep(dir.path())
        .args(["--binary", "hello-bin"])
        .assert()
        .success()
        .stdout(predicates::str::contains("binary file matches"));
    // --files already lists it (indexing and searching are distinct).
    glep(dir.path())
        .args(["--files"])
        .assert()
        .success()
        .stdout(predicates::str::contains("blob.bin"));
}

#[test]
fn word_regexp_matches_whole_words_only() {
    let dir = corpus();
    std::fs::write(dir.path().join("w.txt"), "aaafooaaa\nfoo bar\nxfooy\n").unwrap();
    glep(dir.path())
        .args(["-w", "foo"])
        .assert()
        .success()
        .stdout("w.txt:2:foo bar\n");
}

#[test]
fn line_regexp_matches_whole_lines_only() {
    let dir = corpus();
    std::fs::write(dir.path().join("x.txt"), "foo\nxfoo\nfoo bar\n").unwrap();
    glep(dir.path())
        .args(["-x", "foo"])
        .assert()
        .success()
        .stdout("x.txt:1:foo\n");
}

#[test]
fn smart_case_resolves_from_pattern() {
    let dir = corpus();
    std::fs::write(dir.path().join("c.txt"), "token\nTOKEN\nToken\n").unwrap();
    glep(dir.path())
        .args(["-S", "token"])
        .assert()
        .success()
        .stdout("c.txt:1:token\nc.txt:2:TOKEN\nc.txt:3:Token\n");
    glep(dir.path())
        .args(["-S", "Token"])
        .assert()
        .success()
        .stdout("c.txt:3:Token\n");
}

/// -v must print files that contain ZERO occurrences of the pattern:
/// trigram narrowing would otherwise exclude them entirely (the index only
/// knows which files contain a literal, and under inversion a file with no
/// occurrences matches every line it has). This test guards that Plan::All
/// fallback.
#[test]
fn invert_match_scans_files_without_the_literal() {
    let dir = corpus();
    glep(dir.path())
        .args(["-v", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:2:general kenobi"));
    // Inversion composes with -c (count of non-matching lines).
    glep(dir.path())
        .args(["-v", "-c", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:1"));
}

#[test]
fn max_count_stops_per_file() {
    let dir = corpus();
    std::fs::write(dir.path().join("m.txt"), "m1\nm2\nm3\n").unwrap();
    glep(dir.path())
        .args(["-m", "1", "m"])
        .assert()
        .success()
        .stdout("m.txt:1:m1\n");
    // -m0: no output at all, exit 1 (rg semantics).
    glep(dir.path()).args(["-m", "0", "m"]).assert().code(1);
}

#[test]
fn only_matching_prints_match_parts_and_counts_matches() {
    let dir = corpus();
    std::fs::write(dir.path().join("o.txt"), "foofoo\nfoo bar\n").unwrap();
    glep(dir.path())
        .args(["-o", "foo"])
        .assert()
        .success()
        .stdout("o.txt:1:foo\no.txt:1:foo\no.txt:2:foo\n");
    // With -o, -c counts matches rather than matched lines.
    glep(dir.path())
        .args(["-o", "-c", "foo"])
        .assert()
        .success()
        .stdout("o.txt:3\n");
}

#[test]
fn no_line_number_keeps_path_prefix() {
    let dir = corpus();
    glep(dir.path())
        .args(["-N", "hello"])
        .assert()
        .success()
        .stdout(p(
            "notes.txt:hello there\nsrc/lib.rs:pub fn hello_world() {}\n",
        ));
}

#[test]
fn heading_groups_matches_under_path_lines() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["--heading", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s,
        p("notes.txt\n1:hello there\n\nsrc/lib.rs\n1:pub fn hello_world() {}\n")
    );
}

#[test]
fn quiet_suppresses_output_but_keeps_exit_code() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["-q", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(out.is_empty(), "-q must print nothing");
    glep(dir.path())
        .args(["-q", "zz_absent_zz"])
        .assert()
        .code(1);
    // rg still emits the --json summary event under -q.
    let out = glep(dir.path())
        .args(["--json", "-q", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let summary: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(summary["type"], "summary");
}

/// `-f -` reads the pattern list from stdin (rg semantics); the
/// positional slot is then a path.
#[test]
fn pattern_file_dash_reads_stdin() {
    let dir = corpus();
    glep(dir.path())
        .args(["-f", "-", "-l"])
        .write_stdin("kenobi\nhello_world\n")
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
    glep(dir.path())
        .args(["-f", "-", "-l", "src"])
        .write_stdin("hello\n")
        .assert()
        .success()
        .stdout(p("src/lib.rs\n"));
}

/// -s is rg's --case-sensitive (not --no-messages, which is long-only).
#[test]
fn short_s_is_case_sensitive() {
    let dir = corpus();
    glep(dir.path())
        .args(["-i", "-s", "HELLO"])
        .assert()
        .code(1);
    glep(dir.path())
        .args(["-s", "-i", "HELLO", "-l"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
    glep(dir.path())
        .args(["--no-messages", "hello", "-l"])
        .assert()
        .success();
}

/// glep never reads stdin (rg searches a piped stdin when given no path,
/// which is what made an earlier probe with an open pipe look hung): a
/// search with an open, silent stdin pipe must still finish.
#[test]
fn open_stdin_pipe_never_blocks_a_search() {
    let dir = corpus();
    let bin = assert_cmd::cargo::cargo_bin("glep");
    for args in [&["hello"][..], &["--crlf", "-c", "hello"], &["--files"]] {
        let mut child = std::process::Command::new(&bin)
            .current_dir(dir.path())
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("glep {args:?} blocked with an open stdin pipe");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(status.success(), "{args:?}: {status}");
    }
}

/// GLEP_INDEX_PATH points at an index directory: its parent is the tree
/// searched, whatever the cwd, and no index is built anywhere else. A
/// path that is not a directory is an error (exit 2), and a relative one
/// resolves against the cwd.
#[test]
fn index_path_env_override() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    let idx = dir.path().join(".glep");
    let elsewhere = tempfile::tempdir().unwrap();
    glep(elsewhere.path())
        .env("GLEP_INDEX_PATH", &idx)
        .args(["-l", "hello"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
    assert!(!elsewhere.path().join(".glep").exists());
    let out = glep(elsewhere.path())
        .env("GLEP_INDEX_PATH", elsewhere.path().join("no_such_index"))
        .args(["-l", "hello"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not an index directory"), "{err}");
    assert!(!elsewhere.path().join("no_such_index").exists());
    glep(dir.path())
        .env("GLEP_INDEX_PATH", ".glep")
        .args(["-l", "hello"])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
}

/// A run from a subdirectory finds the ancestor's index and searches the
/// cwd subtree with cwd-relative paths; it never builds a second .glep
/// below the root.
#[test]
fn subdirectory_run_uses_the_ancestor_index() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    let sub = dir.path().join("src");
    glep(&sub)
        .args(["-l", "hello"])
        .assert()
        .success()
        .stdout("lib.rs\n");
    glep(&sub)
        .arg("--files")
        .assert()
        .success()
        .stdout("lib.rs\n");
    glep(&sub)
        .args(["-l", "hello", "../notes.txt"])
        .assert()
        .success()
        .stdout(p("../notes.txt\n"));
    assert!(!sub.join(".glep").exists());
}

/// A reader that goes away early (`glep ... | head -1`) ends the run
/// quietly, as in rg: no "Broken pipe" message, and the exit status the
/// results so far decide (0 after a match, 1 when nothing matched even
/// though --passthru or -c --include-zero printed lines). The big file
/// sorts first, so pool workers race ahead of the printer and wait on its
/// window while it is blocked on the full pipe: when the pipe closes they
/// must be released, not left waiting forever.
#[test]
fn closed_pipe_ends_the_run_quietly() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let mut big = String::new();
    for i in 0..120_000 {
        big.push_str(&format!("needle line {i} in the big file\n"));
    }
    std::fs::write(dir.path().join("a_big.txt"), big).unwrap();
    // Long names, so every listing and summary mode prints more than a
    // pipe buffer holds.
    let deep = dir
        .path()
        .join("z")
        .join("d".repeat(60))
        .join("e".repeat(40));
    std::fs::create_dir_all(&deep).unwrap();
    for i in 0..600 {
        let name = format!("f{i:03}_{}.txt", "x".repeat(40));
        std::fs::write(deep.join(name), format!("needle small {i}\n")).unwrap();
    }
    glep(dir.path()).arg("index").assert().success();
    let bin = assert_cmd::cargo::cargo_bin("glep");
    let cases: &[(&[&str], i32)] = &[
        (&["needle"], 0),
        (&["--files"], 0),
        (&["-l", "needle"], 0),
        (&["-c", "needle"], 0),
        (&["--json", "needle"], 0),
        (&["--line-buffered", "needle"], 0),
        (&["-j", "1", "needle"], 0),
        (&["--passthru", "zz_absent_zz"], 1),
        (&["-c", "--include-zero", "zz_absent_zz"], 1),
    ];
    for &(args, code) in cases {
        let mut child = std::process::Command::new(&bin)
            .current_dir(dir.path())
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Read the first line, then close the pipe.
        let mut stdout = child.stdout.take().unwrap();
        let mut byte = [0u8; 1];
        while stdout.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {}
        drop(stdout);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("glep {args:?} hung after its reader closed the pipe");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        assert!(err.is_empty(), "{args:?}: {err}");
        assert_eq!(status.code(), Some(code), "{args:?}");
    }
}

/// Parallel first queries on a tree with no index yet: one glep builds
/// the index under its lock, and every other one answers with a live
/// scan (what --no-index searches) instead of failing with "another glep
/// holds the index lock". Holding the lock here stands in for the
/// building glep, so the race is not left to timing.
#[test]
fn queries_during_another_build_answer_with_a_live_scan() {
    use fs2::FileExt;
    let dir = corpus();
    std::fs::create_dir_all(dir.path().join(".glep")).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.path().join(".glep/lock"))
        .unwrap();
    held.lock_exclusive().unwrap();
    let cases: &[(&[&str], &str)] = &[
        (&["-n", "hello"], "notes.txt:1:hello there\nsrc/lib.rs:1:pub fn hello_world() {}\n"),
        (&["-l", "hello"], "notes.txt\nsrc/lib.rs\n"),
        (&["-c", "kenobi"], "notes.txt:1\n"),
        (&["--files"], "notes.txt\nsrc/lib.rs\n"),
        (&["--files", "*.rs"], "src/lib.rs\n"),
    ];
    for &(args, expected) in cases {
        let out = glep(dir.path()).args(args).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{args:?}: {err}");
        assert!(err.is_empty(), "{args:?}: {err}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), p(expected), "{args:?}");
    }
    assert!(!dir.path().join(".glep/manifest.bin").exists());
    // The explicit maintenance commands still report the busy lock.
    glep(dir.path())
        .arg("index")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("another glep holds the index lock"));
    FileExt::unlock(&held).unwrap();
    for &(args, expected) in cases {
        glep(dir.path()).args(args).assert().success().stdout(p(expected));
    }
    assert!(dir.path().join(".glep/manifest.bin").exists());
}
