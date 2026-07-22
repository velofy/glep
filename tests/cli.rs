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
fn status_skipped_count_ignores_hidden_flag() {
    let dir = corpus();
    std::fs::write(dir.path().join(".hidden.txt"), "h").unwrap();
    glep(dir.path()).arg("index").assert().success();
    glep(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("skipped (binary/oversized): 0"));
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
    glep(dir.path()).args(["--no-ignore", "--files"]).assert().success();

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
