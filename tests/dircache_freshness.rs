//! End-to-end freshness tests for the dir-mtime listing cache (--files).
//!
//! These pin the soundness contract documented in src/dircache.rs against
//! real filesystem mutations: every file-set change (create, delete,
//! rename, at any depth) and every in-place .gitignore edit must be
//! reflected by the NEXT --files invocation, even when the cache path
//! answers it. Each test compares glep's cached-path listing against an
//! authoritative sweep (GLEP_NO_DIRCACHE=1 forces the full path), or
//! against rg. The last tests do the same for content queries no trigram
//! narrows, which search the files a cached listing lists.

// The dir-mtime listing cache only exists on macOS (see src/dircache.rs).
#![cfg(target_os = "macos")]

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

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
/// subdirectory took 64 ms through the cache and takes 14 ms this way).
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

/// Set every file's and directory's mtime under `root` (and `root`'s own)
/// an hour back, so nothing in a fresh fixture looks written during the
/// sweep that builds the listing cache.
fn age_tree(root: &Path) {
    let hour_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            if e.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(hour_ago)
                    .unwrap();
            }
        }
        dirs.push(d);
    }
    // Directories last, deepest first, so setting a child's mtime cannot
    // disturb a parent that was already aged.
    for d in dirs.iter().rev() {
        fs::File::open(d).unwrap().set_modified(hour_ago).unwrap();
    }
}

/// Whether `--files` in `dir` was answered by the listing cache.
fn uses_cache(dir: &Path) -> bool {
    let out = glep()
        .arg("--files")
        .current_dir(dir)
        .env("GLEP_TIMING", "1")
        .output()
        .expect("glep run");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stderr).contains("listing_cache")
}

/// A leaf directory's own `.ignore` or `.rgignore` keeps the cache in
/// use (the leaf is served from it), and an in-place edit of that file,
/// which changes no directory mtime, is still seen by the next listing.
fn leaf_dot_ignore_edit_is_seen(name: &str) {
    let dir = tempfile::tempdir().unwrap();
    base_fixture(dir.path());
    fs::write(dir.path().join("z").join(name), "y.rs\n").unwrap();
    // The cache distrusts any mtime from about two seconds before its
    // sweep on (dircache::RACY_SLACK), so a fixture written just now would
    // make the leaf look racy and be listed live. Age the fixture so the
    // leaf is servable from the cache, as an untouched tree would be.
    age_tree(dir.path());
    glep()
        .arg("index")
        .current_dir(dir.path())
        .output()
        .expect("index");
    // Indexing just wrote under the root; wait out the racy window so the
    // cache built by the next listing trusts what it saw.
    std::thread::sleep(std::time::Duration::from_millis(2200));
    warm(dir.path());
    assert!(uses_cache(dir.path()), "{name}: cache not used");
    let before = cached_listing(dir.path(), &[]);
    assert!(!before.contains("y.rs"), "{name}: {before}");
    assert_eq!(before, listing(dir.path(), &[]));
    fs::write(dir.path().join("z").join(name), "none.txt\n").unwrap();
    let cached = cached_listing(dir.path(), &[]);
    assert!(
        cached.contains("z/y.rs"),
        "{name}: in-place edit missed by cache: {cached}"
    );
    assert_eq!(cached, listing(dir.path(), &[]));
}

#[test]
fn rgignore_edit_in_leaf_dir_flips_verdict() {
    leaf_dot_ignore_edit_is_seen(".rgignore");
}

#[test]
fn dot_ignore_edit_in_leaf_dir_flips_verdict() {
    leaf_dot_ignore_edit_is_seen(".ignore");
}

/// An empty `.gitignore` in a leaf is still re-stat'ed: filling it in
/// place must flip the verdicts it now carries.
#[test]
fn empty_gitignore_in_leaf_filled_in_place_is_seen() {
    let dir = setup(|p| {
        base_fixture(p);
        fs::write(p.join("z/.gitignore"), "").unwrap();
    });
    assert!(cached_listing(dir.path(), &[]).contains("z/y.rs"));
    fs::write(dir.path().join("z/.gitignore"), "y.rs\n").unwrap();
    let cached = cached_listing(dir.path(), &[]);
    assert!(
        !cached.contains("y.rs"),
        "in-place edit of an empty .gitignore missed by cache: {cached}"
    );
    assert_eq!(cached, listing(dir.path(), &[]));
}

/// A cache written by an older glep (another schema) is never served:
/// the listing is rebuilt from a full sweep instead.
#[test]
fn older_cache_schema_is_rebuilt() {
    let dir = setup(base_fixture);
    let cache = dir.path().join(".glep/dirtree.bin");
    let mut bytes = fs::read(&cache).unwrap();
    // bincode writes the schema first, as a little-endian u32.
    bytes[..4].copy_from_slice(&1u32.to_le_bytes());
    fs::write(&cache, &bytes).unwrap();
    assert!(!uses_cache(dir.path()), "an old-schema cache was served");
    assert!(uses_cache(dir.path()), "the cache was not rebuilt");
    assert_eq!(cached_listing(dir.path(), &[]), listing(dir.path(), &[]));
}

// ---------------------------------------------------------------------
// Against rg, on a tree with more files than the small fixtures above.
// ---------------------------------------------------------------------

fn have_rg() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `rg --files --sort path ARGS` in `dir`: (stdout, exit code). The
/// fixtures are no git repositories, and glep honors their `.gitignore`
/// files as rg does with --no-require-git.
fn rg_files(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = std::process::Command::new("rg")
        .current_dir(dir)
        .args(["--files", "--sort", "path", "--no-require-git"])
        .args(args)
        .output()
        .expect("rg run");
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

/// `glep --files ARGS` in `dir`: (stdout, exit code), and whether the
/// listing cache answered it.
fn glep_files(dir: &Path, args: &[&str]) -> ((String, i32), bool) {
    let out = glep()
        .arg("--files")
        .args(args)
        .current_dir(dir)
        .env("GLEP_TIMING", "1")
        .output()
        .expect("glep run");
    let cached = String::from_utf8_lossy(&out.stderr).contains("listing_cache");
    (
        (
            String::from_utf8(out.stdout).unwrap(),
            out.status.code().unwrap_or(-1),
        ),
        cached,
    )
}

/// `glep --files ARGS` and `glep --files ARGS -g '*.h'` print rg's bytes
/// and exit like rg, both answered by the listing cache.
fn assert_lists_like_rg(dir: &Path, args: &[&str], what: &str) {
    for extra in [&[][..], &["-g", "*.h"]] {
        let args = [args, extra].concat();
        let (got, cached) = glep_files(dir, &args);
        assert!(cached, "{what} {args:?}: not answered by the listing cache");
        assert_eq!(got, rg_files(dir, &args), "{what} {args:?}");
    }
}

/// A tree of 48 listed files at several depths: leaves, and interior
/// directories holding files of their own; `.gitignore` rules at the
/// root, in an interior directory and in a leaf; hidden files; a hidden
/// directory the root `.gitignore` whitelists (`!.github/`); C sources
/// and headers; a vendor tree. No ignored file matches a glob the tests
/// pass to -g or --iglob: rg lists those over any ignore rule, while the
/// index glep lists from never holds an ignored file.
fn big_fixture(p: &Path) {
    let files: &[(&str, &str)] = &[
        (".gitignore", "*.o\nbuild/\n!.github/\n"),
        (".env", "x"),
        (".github/workflows/ci.yml", "x"),
        (".github/CODEOWNERS", "x"),
        ("README.md", "x"),
        ("Makefile", "x"),
        ("build/out.c", "x"),
        ("src/.gitignore", "gen_*.tmp\n"),
        ("src/gen_a.tmp", "x"),
        ("src/main.c", "x"),
        ("src/main.h", "x"),
        ("src/util.c", "x"),
        ("src/util.h", "x"),
        ("src/x.o", "x"),
        ("src/core/core.c", "x"),
        ("src/core/core.h", "x"),
        ("src/core/sched.c", "x"),
        ("src/core/deep/a.c", "x"),
        ("src/core/deep/a.h", "x"),
        ("src/core/deep/b.c", "x"),
        ("src/core/deep/b.h", "x"),
        ("src/core/deep2/z.h", "x"),
        ("src/net/net.c", "x"),
        ("src/net/net.h", "x"),
        ("src/net/ipv4.c", "x"),
        ("src/net/ipv6.c", "x"),
        ("include/top.h", "x"),
        ("include/linux/list.h", "x"),
        ("include/linux/types.h", "x"),
        ("include/linux/kernel.h", "x"),
        ("include/linux/module.h", "x"),
        ("include/uapi/.gitignore", "secret.txt\n"),
        ("include/uapi/ioctl.h", "x"),
        ("include/uapi/notes.txt", "x"),
        ("include/uapi/secret.txt", "x"),
        ("include/asm/io.h", "x"),
        ("include/asm/irq.h", "x"),
        ("vendor/README", "x"),
        ("vendor/lib/a/a.c", "x"),
        ("vendor/lib/a/a.h", "x"),
        ("vendor/lib/b/b.c", "x"),
        ("vendor/lib/b/b.h", "x"),
        ("deep/one/top.c", "x"),
        ("deep/one/two/mid.c", "x"),
        ("deep/one/two/three/leaf.h", "x"),
        ("docs/index.md", "x"),
        ("docs/api.md", "x"),
        ("docs/.hidden.md", "x"),
        ("tools/x.py", "x"),
        ("tools/y.py", "x"),
        ("tools/build.sh", "x"),
        ("tools/z.c", "x"),
        ("lib/l1.c", "x"),
        ("lib/l2.c", "x"),
        ("lib/l3.c", "x"),
        ("lib/l4.h", "x"),
        ("lib/l5.h", "x"),
    ];
    for (path, body) in files {
        let path = p.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
}

/// `big_fixture`, indexed.
fn big_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    big_fixture(dir.path());
    glep()
        .arg("index")
        .current_dir(dir.path())
        .output()
        .expect("index");
    dir
}

/// The listing cache file's identity: a rewrite (tmp file and rename)
/// changes it.
fn cache_stamp(dir: &Path) -> (u64, SystemTime) {
    use std::os::unix::fs::MetadataExt;
    let md = fs::metadata(dir.join(".glep/dirtree.bin")).expect("listing cache");
    (md.ino(), md.modified().unwrap())
}

/// Run the first `glep --files` in `dir`, which lists from the index's
/// sweep and then builds the listing cache, paused right before that
/// build (a debug-build hook) while `change` runs. Returns what it
/// printed.
#[cfg(debug_assertions)]
fn list_and_build_around(dir: &Path, change: impl FnOnce()) -> String {
    use std::time::{Duration, Instant};
    assert!(!dir.join(".glep/dirtree.bin").exists());
    let pause = tempfile::tempdir().unwrap();
    let stdout = pause.path().join("stdout");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_glep"))
        .arg("--files")
        .current_dir(dir)
        .env("GLEP_TEST_PAUSE_BEFORE_DIRCACHE_BUILD", pause.path())
        .stdout(fs::File::create(&stdout).unwrap())
        .spawn()
        .expect("glep run");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !pause.path().join("reached").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "glep exited before building the cache"
        );
        assert!(Instant::now() < deadline, "glep never reached the build");
        std::thread::sleep(Duration::from_millis(5));
    }
    change();
    fs::write(pause.path().join("resume"), "").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(dir.join(".glep/dirtree.bin").exists(), "no cache built");
    fs::read_to_string(stdout).unwrap()
}

/// The cache a full listing builds stats each directory and `.gitignore`
/// after the sweep that listed it. A file created in a leaf in between
/// must not be hidden from every later listing (the leaf's new mtime
/// recorded next to its old listing), and a `.gitignore` edited in
/// between must not let listings filtered by its old rules be served,
/// whether it sits at the root, in an interior directory, or in a leaf.
#[cfg(debug_assertions)]
#[test]
fn changes_while_the_cache_is_built_are_seen() {
    if !have_rg() {
        eprintln!("dircache: rg not installed, skipping");
        return;
    }
    let cases: &[(&str, &str, &str, &str)] = &[
        // (what, file written, its new contents, a line the listing
        // printed before the change and must not print after it, or
        // "" for none)
        (
            "a file created in a leaf",
            "src/core/deep/during.h",
            "x",
            "",
        ),
        (
            "the root .gitignore edited in place",
            ".gitignore",
            "*.c\nbuild/\n!.github/\n",
            "src/core/deep/a.c\n",
        ),
        (
            "an interior .gitignore edited in place",
            "src/.gitignore",
            "*.c\n",
            "src/core/deep/a.c\n",
        ),
        (
            "a leaf .gitignore edited in place, same size",
            "include/uapi/.gitignore",
            "notes.txt\n\n",
            "include/uapi/notes.txt\n",
        ),
    ];
    for &(what, file, contents, gone) in cases {
        let dir = big_tree();
        let root = dir.path();
        let printed = list_and_build_around(root, || fs::write(root.join(file), contents).unwrap());
        assert!(!printed.contains("during.h"), "{what}: {printed}");
        // The next two listings come from the cache and see the change.
        for run in 0..2 {
            assert_lists_like_rg(root, &[], &format!("{what}, run {run}"));
            let ((listed, _), _) = glep_files(root, &[]);
            if gone.is_empty() {
                assert!(listed.contains("src/core/deep/during.h\n"), "{what}");
            } else {
                assert!(printed.contains(gone), "{what}: the fixture changed");
                assert!(!listed.contains(gone), "{what}: {listed}");
            }
        }
    }
}

/// A listing rewrites the cache exactly when some directory changed: a
/// file created in a leaf or in an interior directory, a subdirectory
/// added to a leaf (which makes it interior), a directory deleted, a
/// `.gitignore` edited in place with its size kept. Every listing prints
/// rg's bytes.
#[test]
fn the_cache_is_rewritten_exactly_when_a_directory_changed() {
    if !have_rg() {
        eprintln!("dircache: rg not installed, skipping");
        return;
    }
    let dir = big_tree();
    let root = dir.path();
    // The first listing builds the cache from the index's sweep, and the
    // walk after it may still reorder what that build recorded.
    for _ in 0..2 {
        glep_files(root, &[]);
    }
    let check = |what: &str, change: Option<&dyn Fn()>| {
        if let Some(change) = change {
            change();
        }
        let before = cache_stamp(root);
        let (got, cached) = glep_files(root, &[]);
        assert!(cached, "{what}: not answered by the listing cache");
        assert_eq!(got, rg_files(root, &[]), "{what}");
        assert_eq!(
            cache_stamp(root) != before,
            change.is_some(),
            "{what}: rewritten exactly when something changed"
        );
        // Nothing changed since: the cache stays as it is.
        let before = cache_stamp(root);
        assert_lists_like_rg(root, &[], what);
        assert_eq!(cache_stamp(root), before, "{what}: rewritten again");
    };
    check("no change", None);
    check(
        "a file created in a leaf",
        Some(&|| fs::write(root.join("src/core/deep/new.c"), "x").unwrap()),
    );
    check(
        "a file created in an interior directory",
        Some(&|| fs::write(root.join("src/core/new.h"), "x").unwrap()),
    );
    check(
        "a subdirectory added to a leaf",
        Some(&|| {
            fs::create_dir(root.join("src/net/sub")).unwrap();
            fs::write(root.join("src/net/sub/s.h"), "x").unwrap();
        }),
    );
    check(
        "a directory deleted",
        Some(&|| fs::remove_dir_all(root.join("vendor/lib/b")).unwrap()),
    );
    check(
        "a leaf .gitignore edited in place, same size",
        Some(&|| fs::write(root.join("include/uapi/.gitignore"), "notes.txt\n\n").unwrap()),
    );
    check(
        "an interior .gitignore edited in place, same size",
        Some(&|| fs::write(root.join("src/.gitignore"), "util.c\n\n\n\n").unwrap()),
    );
    check(
        "the root .gitignore edited in place, same size",
        Some(&|| fs::write(root.join(".gitignore"), "*.c\nbuild/\n!.github/\n").unwrap()),
    );
    let ((listed, _), _) = glep_files(root, &[]);
    for (path, shown) in [
        ("src/core/deep/new.c", false),
        ("src/core/new.h", true),
        ("src/net/sub/s.h", true),
        ("vendor/lib/b/b.h", false),
        ("include/uapi/secret.txt", true),
        ("include/uapi/notes.txt", false),
        ("src/gen_a.tmp", true),
        ("src/util.c", false),
        ("src/util.h", true),
    ] {
        assert_eq!(listed.contains(&format!("{path}\n")), shown, "{path}");
    }
}

/// rg's filters apply inside the cached listing, before any path is
/// copied: each of them lists rg's bytes through the cache, over a tree
/// whose root `.gitignore` whitelists the hidden `.github/` directory.
#[test]
fn filters_over_the_cache_match_rg() {
    if !have_rg() {
        eprintln!("dircache: rg not installed, skipping");
        return;
    }
    let dir = big_tree();
    let root = dir.path();
    for _ in 0..2 {
        glep_files(root, &[]);
    }
    let ((listed, _), _) = glep_files(root, &[]);
    assert!(listed.contains(".github/workflows/ci.yml\n"), "{listed}");
    assert!(!listed.contains(".env"), "{listed}");
    for args in [
        &[][..],
        &["-g", "!vendor"],
        &["-g", "deep/**"],
        &["--iglob", "*.C"],
        &["-t", "c"],
        &["-T", "c"],
        &["--type-add", "hdr:*.h", "-t", "hdr"],
        &["--max-depth", "2"],
        &["--max-depth", "1"],
        &["--hidden"],
        &["--hidden", "-g", "!.github"],
        &["-g", ".env"],
        &["-g", ".github/**"],
        &["-g", "!.github/"],
        &["-g", "!src", "-g", "!include"],
    ] {
        assert_lists_like_rg(root, args, "filter");
    }
}

/// The walk that first sees a change saves the whole tree whatever the
/// query filters: a filtered listing leaves the cache right for the next
/// plain one.
#[test]
fn a_filtered_listing_saves_the_whole_tree() {
    if !have_rg() {
        eprintln!("dircache: rg not installed, skipping");
        return;
    }
    let dir = big_tree();
    let root = dir.path();
    for _ in 0..2 {
        glep_files(root, &[]);
    }
    fs::write(root.join("lib/new.c"), "x").unwrap();
    assert_lists_like_rg(root, &["-g", "!lib", "-t", "py"], "a filtered listing");
    let ((listed, _), cached) = glep_files(root, &[]);
    assert!(cached);
    assert!(listed.contains("lib/new.c\n"), "{listed}");
    assert_eq!((listed, 0), rg_files(root, &[]));
}

// ---------------------------------------------------------------------
// Content queries no trigram narrows (Plan::All) search the listing
// cache's file set, without the index.
// ---------------------------------------------------------------------

/// Queries whose plans no trigram narrows: a single letter, a two-letter
/// literal, -v, --files-without-match and -c --include-zero over every
/// file, --passthru, and a class repeat.
const PLAN_ALL: &[&[&str]] = &[
    &["q"],
    &["-c", "ab"],
    &["-v", "-c", "a"],
    &["--files-without-match", ".*"],
    &["-c", "--include-zero", "x"],
    &["--passthru", "a"],
    &["-c", "[0-9]{2}"],
];

/// What a run printed and how it exited: (stdout, stderr, exit code).
type Printed = (String, String, i32);

fn printed(out: std::process::Output) -> Printed {
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

/// `glep ARGS` in `dir` with GLEP_TIMING (stdin, as for every run here,
/// reads nothing): what it printed, its timing lines dropped, and the
/// timing lines alone.
fn glep_timed(dir: &Path, args: &[&str]) -> (Printed, String) {
    let out = glep()
        .args(args)
        .current_dir(dir)
        .env("GLEP_TIMING", "1")
        .output()
        .expect("glep run");
    let (stdout, stderr, code) = printed(out);
    let (timing, rest): (Vec<&str>, Vec<&str>) =
        stderr.lines().partition(|l| l.starts_with("glep timing: "));
    let lines = |v: Vec<&str>| -> String { v.iter().map(|l| format!("{l}\n")).collect() };
    ((stdout, lines(rest), code), lines(timing))
}

/// `glep_timed`, and whether the listing cache served the files searched
/// while the index stayed shut.
fn glep_search(dir: &Path, args: &[&str]) -> (Printed, bool) {
    let (got, timing) = glep_timed(dir, args);
    let cached = timing.contains("glep timing: listing_cache_walk")
        && !timing.contains("glep timing: index_open");
    (got, cached)
}

/// `rg ARGS` in `dir` with glep's default output settings (line numbers,
/// no headings, no color) and rg's path sort, stdin from the null device.
fn rg_search(dir: &Path, args: &[&str]) -> Printed {
    let out = std::process::Command::new("rg")
        .current_dir(dir)
        .args([
            "-n",
            "--no-heading",
            "--color=never",
            "--sort",
            "path",
            "--no-require-git",
        ])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("rg run");
    printed(out)
}

/// Every PLAN_ALL query, with `extra` after it, prints rg's stdout and
/// stderr and exits like rg, its files served by the listing cache.
/// Returns what `q` printed.
fn assert_plan_all_like_rg(dir: &Path, extra: &[&str], what: &str) -> String {
    let mut q = String::new();
    for args in PLAN_ALL {
        let args = [args, extra].concat();
        let (got, cached) = glep_search(dir, &args);
        assert!(cached, "{what} {args:?}: not served by the listing cache");
        assert_eq!(got, rg_search(dir, &args), "{what} {args:?}");
        if args[0] == "q" {
            q = got.0;
        }
    }
    q
}

/// Every regular file below `dir`, in path order.
fn files_below(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                dirs.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

/// `big_fixture` with lines the PLAN_ALL queries tell apart in every
/// file but the `.gitignore` files (letters, `ab`, two-digit numbers,
/// empty lines and empty files), the same line in each of the hidden
/// files and in both files a leaf `.gitignore` decides between, a binary
/// file (a NUL in its first bytes), a file over the index's size cap and
/// a UTF-16 one: 51 listed files, enough for the worker pool.
fn search_fixture(p: &Path) {
    big_fixture(p);
    let bodies = [
        "q ab 42\nplain line\n",
        "",
        "x marks the spot 7\nab\nzz 10\n",
        "a\nb\nc\nd\n",
        "quiet 99 x\n\nend of file\n",
        "nothing to see\n123 456\nxq\n",
    ];
    for (i, file) in files_below(p).iter().enumerate() {
        if file.file_name().is_some_and(|n| n != ".gitignore") {
            fs::write(file, bodies[i % bodies.len()]).unwrap();
        }
    }
    for hidden in [".env", ".github/workflows/ci.yml", "docs/.hidden.md"] {
        fs::write(p.join(hidden), "q ab 12 x a hidden\n").unwrap();
    }
    // Both sides of the leaf `.gitignore` edit the tests make.
    for uapi in ["include/uapi/notes.txt", "include/uapi/secret.txt"] {
        fs::write(p.join(uapi), "q ab 12 x a uapi\n").unwrap();
    }
    fs::write(p.join("lib/blob.bin"), b"q ab 12 x a\x00binary\nq\n").unwrap();
    // Files the index never reads for trigrams, so a narrowed query
    // always searches them: one over the 1 MiB --max-filesize cap, and a
    // UTF-16 one that rg transcodes.
    let mut big = Vec::new();
    for _ in 0..1100 {
        big.extend_from_slice(b"q ab 12 x a ");
        big.extend_from_slice(&[b'z'; 987]);
        big.push(b'\n');
    }
    assert!(big.len() > 1 << 20);
    fs::write(p.join("docs/big.txt"), big).unwrap();
    let mut utf16 = vec![0xFF, 0xFE];
    for b in "q ab 12 x a utf16\nzz\n".bytes() {
        utf16.extend_from_slice(&[b, 0]);
    }
    fs::write(p.join("docs/utf16.txt"), utf16).unwrap();
}

/// `search_fixture`, indexed, with the listing cache built (the first
/// listing builds it from the index's sweep, and the walk after it may
/// still reorder what that build recorded).
fn search_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    search_fixture(dir.path());
    glep()
        .arg("index")
        .current_dir(dir.path())
        .output()
        .expect("index");
    for _ in 0..2 {
        glep_files(dir.path(), &[]);
    }
    assert!(dir.path().join(".glep/dirtree.bin").exists());
    // blob.bin, big.txt and utf16.txt carry the index's skip flags.
    let status = glep()
        .arg("status")
        .current_dir(dir.path())
        .output()
        .expect("status");
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(
        status.contains("skipped (binary/oversized/utf16): 3\n"),
        "{status}"
    );
    dir
}

/// Write `body` over `file` in place, then give its directory back the
/// mtime it had, as `touch -r` from a copy of it would: the file set the
/// cache recorded for the directory still holds, so a listing serves the
/// directory from the cache.
fn rewrite_keeping_dir_mtime(file: &Path, body: &[u8]) {
    let dir = file.parent().unwrap();
    let mtime = fs::metadata(dir).unwrap().modified().unwrap();
    fs::write(file, body).unwrap();
    fs::File::open(dir).unwrap().set_modified(mtime).unwrap();
    assert_eq!(fs::metadata(dir).unwrap().modified().unwrap(), mtime);
}

/// Each PLAN_ALL query prints rg's bytes over files the listing cache
/// serves, after each change to the tree: a file added, deleted and
/// renamed in a leaf, a text file turned binary and back in place with
/// its directory's mtime kept (no content flag of the index decides
/// anything), a file with a NUL past the searcher's first buffer, a leaf
/// `.gitignore` edited in place with its size kept, and a hidden file
/// added, with and without --hidden. The root `.gitignore` whitelists the
/// hidden `.github/` directory throughout, and rg's filters, a reversed
/// path sort and -j1 apply as they do to the index's files.
#[test]
fn plan_all_queries_search_the_cached_listing_like_rg() {
    if !have_rg() {
        eprintln!("dircache: rg not installed, skipping");
        return;
    }
    let dir = search_tree();
    let root = dir.path();
    let q = assert_plan_all_like_rg(root, &[], "unchanged");
    assert!(q.contains(".github/workflows/ci.yml:1:"), "{q}");
    assert!(!q.contains(".env") && !q.contains("blob.bin"), "{q}");
    let (_, timing) = glep_timed(root, &["q"]);
    assert!(timing.contains("search path pool"), "{timing}");
    for extra in [
        &["-g", "*.h"][..],
        &["-g", "!vendor"],
        &["-t", "c"],
        &["--max-depth", "2"],
        &["--sortr", "path"],
        &["--sort", "path"],
        &["-j1"],
    ] {
        assert_plan_all_like_rg(root, extra, "filtered");
    }
    // glep's --max-filesize only caps what the index stores (README): no
    // file is left out of a search for its size, through the cache either.
    let (got, cached) = glep_search(root, &["--max-filesize", "4", "-c", "ab"]);
    assert!(cached, "--max-filesize: not served by the listing cache");
    assert_eq!(got, rg_search(root, &["-c", "ab"]));

    let deep = root.join("src/core/deep");
    fs::write(deep.join("added.c"), "q ab 12 x a added\n").unwrap();
    let q = assert_plan_all_like_rg(root, &[], "a file added in a leaf");
    assert!(q.contains("src/core/deep/added.c:1:"), "{q}");
    fs::remove_file(deep.join("a.h")).unwrap();
    assert_plan_all_like_rg(root, &[], "a file deleted in a leaf");
    fs::rename(deep.join("added.c"), deep.join("renamed.c")).unwrap();
    let q = assert_plan_all_like_rg(root, &[], "a file renamed in a leaf");
    assert!(
        q.contains("src/core/deep/renamed.c:1:") && !q.contains("added.c"),
        "{q}"
    );

    let net = root.join("src/net/net.c");
    let text = fs::read(&net).unwrap();
    fs::write(&net, "q ab 12 x a net\n").unwrap();
    let q = assert_plan_all_like_rg(root, &[], "a text file");
    assert!(q.contains("src/net/net.c:1:"), "{q}");
    // The walk lists src/net from the cache (no directory changed), and
    // the search reads what the file holds now.
    let clean = || glep_timed(root, &["q"]).1.contains("(dirty: false)");
    rewrite_keeping_dir_mtime(&net, b"q ab\x00 12 x a now binary\n");
    assert!(clean(), "src/net was listed live");
    let q = assert_plan_all_like_rg(root, &[], "the text file turned binary");
    assert!(!q.contains("src/net/net.c"), "{q}");
    rewrite_keeping_dir_mtime(&net, &text);
    assert!(clean(), "src/net was listed live");
    assert_plan_all_like_rg(root, &[], "the binary file turned back");

    let mut late = Vec::new();
    for _ in 0..700 {
        late.extend_from_slice(b"q ab 12 x a ");
        late.extend_from_slice(&[b'y'; 88]);
        late.push(b'\n');
    }
    late.extend_from_slice(b"nul\x00here\nq ab 12 x a late\n");
    assert!(late.len() > 64 * 1024);
    fs::write(root.join("include/asm/late.h"), &late).unwrap();
    let q = assert_plan_all_like_rg(root, &[], "a NUL past the first buffer");
    assert!(q.contains("include/asm/late.h:1:"), "{q}");

    let uapi = root.join("include/uapi");
    assert!(
        q.contains("include/uapi/notes.txt:1:") && !q.contains("secret.txt"),
        "{q}"
    );
    fs::write(uapi.join(".gitignore"), "notes.txt\n\n").unwrap();
    let q = assert_plan_all_like_rg(root, &[], "a leaf .gitignore edited in place");
    assert!(
        q.contains("include/uapi/secret.txt:1:") && !q.contains("notes.txt"),
        "{q}"
    );

    fs::write(root.join("src/core/.secret"), "q ab 12 x a secret\n").unwrap();
    let q = assert_plan_all_like_rg(root, &[], "a hidden file added");
    assert!(!q.contains(".secret") && !q.contains(".env"), "{q}");
    let q = assert_plan_all_like_rg(root, &["--hidden"], "a hidden file added");
    assert!(
        q.contains("src/core/.secret:1:") && q.contains(".env:1:"),
        "{q}"
    );
}

/// A Plan::All query never opens the index, so it certifies nothing: the
/// manifest stays byte for byte what it was, and the next narrowed query
/// still sweeps and finds a file created just before. Within a --ttl the
/// index answers a Plan::All query without a sweep, as before.
#[test]
fn plan_all_queries_leave_the_index_to_the_next_narrowed_query() {
    if !have_rg() {
        eprintln!("dircache: rg not installed, skipping");
        return;
    }
    let dir = search_tree();
    let root = dir.path();
    let manifest = fs::read(root.join(".glep/manifest.bin")).unwrap();
    fs::write(root.join("src/core/deep/fresh.c"), "the needle q ab 12\n").unwrap();
    for args in PLAN_ALL {
        let (got, cached) = glep_search(root, args);
        assert!(cached, "{args:?}: not served by the listing cache");
        assert_eq!(got, rg_search(root, args), "{args:?}");
        assert_eq!(
            fs::read(root.join(".glep/manifest.bin")).unwrap(),
            manifest,
            "{args:?} wrote the manifest"
        );
    }
    let (got, timing) = glep_timed(root, &["-l", "needle"]);
    assert!(timing.contains("sweep_walk"), "{timing}");
    assert_eq!(got.0, "src/core/deep/fresh.c\n");
    assert_eq!(got, rg_search(root, &["-l", "needle"]));
    // --ttl is glep's own flag: rg answers the same query without it.
    let (got, timing) = glep_timed(root, &["--ttl", "600", "-c", "ab"]);
    assert!(
        timing.contains("index_open")
            && !timing.contains("sweep_walk")
            && !timing.contains("listing_cache"),
        "{timing}"
    );
    assert_eq!(got, rg_search(root, &["-c", "ab"]));
}
