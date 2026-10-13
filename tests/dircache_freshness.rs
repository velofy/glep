//! End-to-end freshness tests for the dir-mtime listing cache (--files).
//!
//! These pin the soundness contract documented in src/dircache.rs against
//! real filesystem mutations: every file-set change (create, delete,
//! rename, at any depth) and every in-place .gitignore edit must be
//! reflected by the NEXT --files invocation, even when the cache path
//! answers it. Each test compares glep's cached-path listing against an
//! authoritative sweep (GLEP_NO_DIRCACHE=1 forces the full path).

// The dir-mtime listing cache only exists on macOS (see src/dircache.rs).
#![cfg(target_os = "macos")]

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn glep() -> Command {
    Command::cargo_bin("glep").unwrap()
}

fn listing(dir: &Path, args: &[&str]) -> String {
    let out = glep()
        .arg("--files")
        .args(args)
        .current_dir(dir)
        .env("GLEP_NO_DIRCACHE", "")
        .output()
        .expect("glep run");
    String::from_utf8(out.stdout).unwrap()
}

fn cached_listing(dir: &Path, args: &[&str]) -> String {
    let out = glep()
        .arg("--files")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("glep run");
    String::from_utf8(out.stdout).unwrap()
}

/// Warm the cache with one --files run (cache path), then return.
fn warm(dir: &Path) {
    let out = glep()
        .arg("--files")
        .current_dir(dir)
        .output()
        .expect("glep run");
    assert!(out.status.success());
    assert!(
        dir.join(".glep/dirtree.bin").exists(),
        "cache must exist after warm"
    );
}

fn setup(fixture: fn(&Path)) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // build index + cache
    glep()
        .arg("index")
        .current_dir(dir.path())
        .output()
        .expect("index");
    warm(dir.path());
    dir
}

fn base_fixture(p: &Path) {
    fs::create_dir_all(p.join("a/b/c")).unwrap();
    fs::write(p.join("root.txt"), "x").unwrap();
    fs::write(p.join("a/a.txt"), "x").unwrap();
    fs::write(p.join("a/b/b.txt"), "x").unwrap();
    fs::write(p.join("a/b/c/deep.txt"), "x").unwrap();
    fs::create_dir_all(p.join("z")).unwrap();
    fs::write(p.join("z/y.rs"), "fn q() {}").unwrap();
}

#[test]
fn unchanged_tree_cached_equals_authoritative() {
    let dir = setup(base_fixture);
    assert_eq!(cached_listing(dir.path(), &[]), listing(dir.path(), &[]));
}

#[test]
fn deep_new_file_appears() {
    let dir = setup(base_fixture);
    // create at depth 3 under a leaf dir: must NOT be missed by the
    // pruned-cache path (dir mtime of a/b/c changes).
    fs::write(dir.path().join("a/b/c/new.txt"), "new").unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(
        cached.contains("a/b/c/new.txt"),
        "new deep file missing from cached listing"
    );
    assert_eq!(cached, auth);
}

#[test]
fn deep_delete_disappears() {
    let dir = setup(base_fixture);
    fs::remove_file(dir.path().join("a/b/c/deep.txt")).unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(!cached.contains("deep.txt"), "deleted file still listed");
    assert_eq!(cached, auth);
}

#[test]
fn rename_inside_leaf_is_seen() {
    let dir = setup(base_fixture);
    fs::rename(
        dir.path().join("a/b/b.txt"),
        dir.path().join("a/b/renamed.txt"),
    )
    .unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(cached.contains("renamed.txt"));
    assert!(!cached.contains("b.txt"));
    assert_eq!(cached, auth);
}

#[test]
fn new_subdir_in_leaf_dir_is_seen() {
    let dir = setup(base_fixture);
    // a/b/c is a leaf with no subdirs; adding a SUBDIR inside it makes the
    // cached leaf invalid (dir mtime bumps) and the walk must notice.
    fs::create_dir_all(dir.path().join("a/b/c/subdir")).unwrap();
    fs::write(dir.path().join("a/b/c/subdir/s.txt"), "s").unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(cached.contains("a/b/c/subdir/s.txt"));
    assert_eq!(cached, auth);
}

#[test]
fn gitignore_edit_in_leaf_dir_flips_verdict() {
    let dir = setup(|p| {
        base_fixture(p);
        fs::write(p.join("z/.gitignore"), "ignored.txt\n").unwrap();
        fs::write(p.join("z/ignored.txt"), "hidden by local rule").unwrap();
    });
    let before = cached_listing(dir.path(), &[]);
    assert!(!before.contains("ignored.txt"));
    // In-place edit of the leaf's .gitignore: no dir mtime changes
    // anywhere. The stat-check must catch it.
    fs::write(dir.path().join("z/.gitignore"), "").unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(
        cached.contains("ignored.txt"),
        "in-place .gitignore edit missed by cache"
    );
    assert_eq!(cached, auth);
}

#[test]
fn gitignore_edit_in_interior_dir_flips_verdict() {
    let dir = setup(|p| {
        base_fixture(p);
        fs::write(p.join("a/.gitignore"), "a.txt\n").unwrap();
    });
    let before = cached_listing(dir.path(), &[]);
    assert!(
        !before.contains("a/a.txt"),
        "a.txt should be ignored at setup"
    );
    // interior dir .gitignore in-place edit: chain stability must catch it
    fs::write(dir.path().join("a/.gitignore"), "").unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(
        cached.contains("a/a.txt"),
        "interior .gitignore edit missed by cache"
    );
    assert_eq!(cached, auth);
}

#[test]
fn deleted_gitignore_in_leaf_is_seen() {
    let dir = setup(|p| {
        base_fixture(p);
        fs::write(p.join("z/.gitignore"), "y.rs\n").unwrap();
    });
    let before = cached_listing(dir.path(), &[]);
    assert!(!before.contains("y.rs"));
    fs::remove_file(dir.path().join("z/.gitignore")).unwrap();
    // deleting the gitignore bumps z's dir mtime: prune must fail, rescan
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(cached.contains("y.rs"));
    assert_eq!(cached, auth);
}

#[test]
fn new_gitignore_in_leaf_is_seen() {
    let dir = setup(base_fixture);
    fs::write(dir.path().join("z/.gitignore"), "y.rs\n").unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert!(!cached.contains("y.rs"));
    assert_eq!(cached, auth);
}

#[test]
fn churn_beyond_budget_falls_back_correctly() {
    let dir = setup(|p| {
        base_fixture(p);
        for i in 0..15 {
            fs::create_dir_all(p.join(format!("gen{i}"))).unwrap();
            fs::write(p.join(format!("gen{i}/f.txt")), "x").unwrap();
        }
    });
    // heavy churn: 15 new dirs; small trees stay under RESCAN_BUDGET so
    // this exercises the rescan path itself, not the budget abort, but
    // with many changes the answer must still be exact.
    for i in 0..15 {
        fs::write(dir.path().join(format!("gen{i}/g.txt")), "x").unwrap();
        fs::remove_file(dir.path().join(format!("gen{i}/f.txt"))).unwrap();
    }
    fs::remove_dir_all(dir.path().join("a/b/c")).unwrap();
    let auth = listing(dir.path(), &[]);
    let cached = cached_listing(dir.path(), &[]);
    assert_eq!(cached, auth);
    assert!(!cached.contains("deep.txt"));
    assert!(cached.contains("gen0/g.txt"));
}

#[test]
fn glob_filter_over_cache_matches_authoritative() {
    let dir = setup(base_fixture);
    assert_eq!(
        cached_listing(dir.path(), &["**/*.rs"]),
        listing(dir.path(), &["**/*.rs"])
    );
    assert_eq!(
        cached_listing(dir.path(), &["**/*.txt"]),
        listing(dir.path(), &["**/*.txt"])
    );
}

#[test]
fn hidden_flag_over_cache_matches_authoritative() {
    let dir = setup(|p| {
        base_fixture(p);
        fs::write(p.join(".env"), "SECRET=1").unwrap();
    });
    warm(dir.path());
    assert_eq!(
        cached_listing(dir.path(), &["--hidden"]),
        listing(dir.path(), &["--hidden"])
    );
    assert_eq!(cached_listing(dir.path(), &[]), listing(dir.path(), &[]));
    assert!(!cached_listing(dir.path(), &[]).contains(".env"));
    assert!(cached_listing(dir.path(), &["--hidden"]).contains(".env"));
}

/// Operands the index cannot serve (here an ignored directory) are walked
/// live: the listing is the same whether the cache or the authoritative
/// path answers, and the --files glob applies to the walked files too.
#[test]
fn unserved_operands_list_the_same_through_the_cache() {
    let dir = setup(|p| {
        base_fixture(p);
        fs::write(p.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(p.join("build/deep")).unwrap();
        fs::write(p.join("build/out.txt"), "x").unwrap();
        fs::write(p.join("build/deep/x.rs"), "x").unwrap();
    });
    for args in [
        &["*.txt", "build"][..],
        &["**", "build", "z"],
        &["*.rs", "build", "a"],
    ] {
        let cached = cached_listing(dir.path(), args);
        assert_eq!(cached, listing(dir.path(), args), "{args:?}");
        assert!(cached.contains("build/"), "{args:?}: {cached}");
    }
}

/// The cache's pruned walk always covers the whole tree, so only a
/// root-wide listing takes it. A listing scoped to a path operand or to a
/// subdirectory cwd goes through the index's scoped sweep, which walks
/// just that subtree (on a kernel-scale tree `--files` in a small
/// subdirectory took 75 ms through the cache and takes 14 ms this way).
/// Either way the answer is the authoritative one.
#[test]
fn only_root_wide_listings_use_the_cache() {
    let dir = setup(base_fixture);
    let uses_cache = |cwd: &Path, args: &[&str]| -> bool {
        let out = glep()
            .arg("--files")
            .args(args)
            .current_dir(cwd)
            .env("GLEP_TIMING", "1")
            .output()
            .expect("glep run");
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).contains("listing_cache")
    };
    let root = dir.path();
    let sub = dir.path().join("a");
    assert!(uses_cache(root, &[]));
    assert!(uses_cache(root, &["**/*.txt"]));
    assert!(uses_cache(root, &["**", "."]));
    assert!(uses_cache(&sub, &["**", ".."]));
    assert!(!uses_cache(root, &["**", "a"]));
    assert!(!uses_cache(root, &["**/*.txt", "a/b", "z"]));
    assert!(!uses_cache(&sub, &[]));
    assert!(!uses_cache(&sub, &["**/*.txt", "b"]));
    for (cwd, args) in [
        (sub.as_path(), &[][..]),
        (sub.as_path(), &["**/*.txt", "b"]),
        (sub.as_path(), &["**", ".."]),
        (root, &["**", "a"]),
        (root, &["**/*.txt", "a/b", "z"]),
    ] {
        assert_eq!(cached_listing(cwd, args), listing(cwd, args), "{args:?}");
    }
}
