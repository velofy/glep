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
        .stdout("notes.txt\nsrc/lib.rs\n");
    // -F with multiple -e: each arm is a literal, metachars don't parse.
    glep(dir.path())
        .args(["-F", "-e", "() {}", "-e", "kenobi", "-l"])
        .assert()
        .success()
        .stdout("notes.txt\nsrc/lib.rs\n");
    // -e makes positionals paths
    glep(dir.path())
        .args(["-e", "hello", "-l", "src"])
        .assert()
        .success()
        .stdout("src/lib.rs\n");
    assert!(s.contains("lat.txt:1:café test"));
    // 'café' (decoded side) does not appear in a.txt's "caf ascii"
    assert!(!s.contains("a.txt"));
    // unknown label errors
    glep(dir.path())
        .args(["-E", "bogus-enc", "x"])
        .assert()
        .failure()
        .code(2);
#[cfg(unix)]
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
    let out = glep(dir.path()).args(["-L", "-l", "linktok"]).assert().success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(s.contains("linkdir/inner.txt"), "{s}");
    assert!(s.contains("linkfile.txt"), "{s}");
fn engine_rejects_unknown() {
    let dir = corpus();
    glep(dir.path())
        .args(["--engine", "default", "hello"])
        .assert()
        .success();
    glep(dir.path())
        .args(["--engine", "pcre2", "hello"])
        .assert()
        .failure()
        .code(2);
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
fn files_with_matches_conflicts_with_json() {
    let dir = corpus();
    glep(dir.path()).args(["-l", "--json", "hello"]).assert().code(2);
}

/// Binary files are listed by --files and searched under -a/--binary, but
/// the default quit detection suppresses them entirely.
#[test]
fn binary_files_suppressed_by_default() {
    let dir = corpus();
    std::fs::write(dir.path().join("blob.bin"), b"aa\x00hello-bin\x00zz\n").unwrap();
    // Default: no output, exit 1 even though "hello" precedes... no wait,
    // "hello-bin" follows the NUL here; either way binary data suppresses.
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
/// A missing path filter is an error (exit 2), not a silent no-match;
/// valid paths still produce their matches.
#[test]
fn missing_path_errors_exit_two() {
    let dir = corpus();
    glep(dir.path())
        .args(["hello", "no_such_dir"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no_such_dir"))
        .stderr(predicates::str::contains("No such file or directory"));
    // Mixed: the valid path still yields its matches, code stays 2.
    glep(dir.path())
        .args(["hello", "src", "no_such_dir"])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("src/lib.rs"))
        .stderr(predicates::str::contains("no_such_dir"));
    // --files uses the operation-error wording. (glep's first positional
    // under --files is the glob; a path filter follows it.)
    glep(dir.path())
        .args(["--files", "*rs", "no_such_dir"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("IO error for operation on"));
}

/// Running from a subdirectory discovers the ancestor `.glep` index,
/// scopes the search to the subtree, and prints cwd-relative paths.
#[test]
fn subdir_discovers_ancestor_index() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    let sub = dir.path().join("src");
    glep(&sub)
        .arg("hello")
        .assert()
        .success()
        .stdout(predicates::str::contains("lib.rs:1:pub fn hello_world()"))
        // root-level notes.txt is outside the cwd subtree
        .stdout(predicates::str::contains("notes.txt").not());
    // No second index was created inside src/.
    assert!(!sub.join(".glep").exists());
    // And --files shows the scoped listing.
    let out = glep(&sub).args(["--files"]).assert().success().get_output().stdout.clone();
    let listing = String::from_utf8(out).unwrap();
    assert!(listing.contains("lib.rs"));
    assert!(!listing.contains("notes.txt"));
}

/// GLEP_INDEX_PATH points at an index directory explicitly; results are
/// printed index-root-relative when the cwd is outside the tree.
#[test]
fn index_path_env_override() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    let elsewhere = tempfile::tempdir().unwrap();
    glep(elsewhere.path())
        .arg("hello")
        .env("GLEP_INDEX_PATH", dir.path().join(".glep"))
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:1:hello there"));
}

/// With an ancestor index and an explicit path arg pointing back up, the
/// whole tree is reachable again.
#[test]
fn subdir_dotdot_reaches_parent_tree() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    glep(&dir.path().join("src"))
        .args(["hello", ".."])
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:1:hello there"));
/// A single file operand drops the path prefix (rg convention); -H
/// restores it, -I forces it off, and the nothing-searched heuristic
/// fires when a filter empties the implicit-scope pool.
#[test]
fn filename_and_depth_semantics() {
    let dir = corpus();
    glep(dir.path())
        .args(["hello", "notes.txt"])
        .assert()
        .success()
        .stdout(predicates::str::contains("1:hello there"))
        .stdout(predicates::str::contains("notes.txt").not());
    glep(dir.path())
        .args(["-H", "hello", "notes.txt"])
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:1:hello there"));
    glep(dir.path())
        .args(["-I", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("1:hello there"))
        .stdout(predicates::str::contains("notes.txt").not());
    // --max-depth 0 searches nothing under the implicit scope → warn + 2.
    glep(dir.path())
        .args(["--max-depth", "0", "hello"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("No files were searched"));
    // …but a narrow *plan* (no candidates, pool non-empty) is exit 1.
    glep(dir.path()).args(["zz_absent_zz"]).assert().code(1);
}

#[test]
fn include_zero_counts_every_searched_file() {
    let dir = corpus();
    // "general" only matches notes.txt; src/lib.rs gets a :0 line.
    glep(dir.path())
        .args(["-c", "--include-zero", "general"])
        .assert()
        .success()
        .stdout(predicates::str::contains("notes.txt:1"))
        .stdout(predicates::str::contains("src/lib.rs:0"));
}
