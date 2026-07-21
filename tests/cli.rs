use assert_cmd::Command;
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
    std::fs::write(dir.path().join("notes.txt"), "hello there\ngeneral kenobi\n").unwrap();
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
    assert!(!bulk.contains("old.bak"), "global excludes must hide old.bak");
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
fn dot_path_filter_searches_whole_index() {
    let dir = corpus();
    glep(dir.path())
        .args(["-l", "hello", "."])
        .assert()
        .success()
        .stdout(p("notes.txt\nsrc/lib.rs\n"));
}

#[test]
fn dotted_relative_path_filters_resolve_against_root() {
    let dir = corpus();
    let out = glep(dir.path())
        .args(["-e", "hello", "src/../src"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("lib.rs"));
    assert!(!s.contains("notes.txt"));
}

#[test]
fn warns_when_path_filter_outside_index_root() {
    let dir = corpus();
    let outside = dir.path().join("outside-tree");
    glep(dir.path())
        .args(["-e", "hello", outside.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("outside indexed tree"));
}
