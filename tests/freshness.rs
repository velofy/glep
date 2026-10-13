use assert_cmd::Command;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn glep(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("glep").unwrap();
    c.current_dir(dir);
    c
}

#[test]
fn edit_create_delete_visible_immediately() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    glep(dir.path()).arg("alpha").assert().success();

    // create
    std::fs::write(dir.path().join("b.txt"), "brand_new_needle\n").unwrap();
    glep(dir.path())
        .arg("brand_new_needle")
        .assert()
        .success()
        .stdout("b.txt:1:brand_new_needle\n");

    // edit
    std::fs::write(dir.path().join("a.txt"), "edited_needle now\n").unwrap();
    glep(dir.path())
        .arg("edited_needle")
        .assert()
        .success()
        .stdout("a.txt:1:edited_needle now\n");
    glep(dir.path()).arg("alpha").assert().code(1);

    // delete
    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    glep(dir.path()).arg("brand_new_needle").assert().code(1);
}

/// While another glep holds the index lock, a query sweeps without
/// writing the index, yet answers for the tree as it is: files deleted
/// since the last written sweep are neither listed nor searched (no
/// "No such file" error), and an edited file is searched as it is now.
/// Holding the lock here stands in for a concurrent writer.
#[test]
fn queries_under_a_held_lock_see_deletions_and_edits() {
    use fs2::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let w = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
    w("a.txt", "needle zebra\n");
    w("b.txt", "needle\n");
    w("c.txt", "zebra only\n");
    w("d.txt", "plain\n");
    glep(dir.path()).arg("needle").assert().success();
    assert!(dir.path().join(".glep/manifest.bin").exists());

    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.path().join(".glep/lock"))
        .unwrap();
    held.lock_exclusive().unwrap();
    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    std::fs::remove_file(dir.path().join("d.txt")).unwrap();
    w("c.txt", "zebra and needle now\n");
    let manifest = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
    let cases: &[(&[&str], &str, i32)] = &[
        (&["--files"], "a.txt\nc.txt\n", 0),
        (&["-l", "needle"], "a.txt\nc.txt\n", 0),
        (&["-l", "zebra"], "a.txt\nc.txt\n", 0),
        (&["-c", "--include-zero", "needle"], "a.txt:1\nc.txt:1\n", 0),
        (&["--files-without-match", "needle"], "", 1),
        (&["--files-without-match", "plain"], "a.txt\nc.txt\n", 0),
        (
            &["-n", "needle"],
            "a.txt:1:needle zebra\nc.txt:1:zebra and needle now\n",
            0,
        ),
    ];
    for &(args, expected, code) in cases {
        let out = glep(dir.path()).args(args).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.is_empty(), "{args:?}: {err}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
    assert_eq!(
        std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap(),
        manifest,
        "a query under a held lock writes nothing"
    );
    FileExt::unlock(&held).unwrap();
    for &(args, expected, code) in cases {
        let out = glep(dir.path()).args(args).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{args:?}");
        assert_eq!(out.status.code(), Some(code), "{args:?}");
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// Overwrite `path` with `body`, at its old size, and pin its mtime back:
/// a rewrite inside the filesystem clock tick of the last write, which
/// leaves the file's (mtime, size) as they were.
fn rewrite_in_the_same_tick(path: &Path, body: &str) {
    let before = std::fs::metadata(path).unwrap();
    assert_eq!(before.len(), body.len() as u64, "a same-size rewrite");
    let mtime = before.modified().unwrap();
    std::fs::write(path, body).unwrap();
    set_mtime(path, mtime);
    assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
}

/// `others` files holding `common`, written an hour ago, and `r.txt`
/// holding `oldneedle`, written just now, then indexed by a first query.
/// The mtime of `r.txt` is pinned two seconds (the racy slack) ahead, so
/// that build reads it inside the racy window even when it starts up to
/// four seconds later, and every later sweep finds it due.
fn racy_tree(others: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for i in 0..others {
        let f = dir.path().join(format!("f{i:02}.txt"));
        std::fs::write(&f, "common\n").unwrap();
        set_mtime(&f, hour_ago);
    }
    let r = dir.path().join("r.txt");
    std::fs::write(&r, "oldneedle\n").unwrap();
    set_mtime(&r, SystemTime::now() + Duration::from_secs(2));
    // The first query builds the index, inline.
    glep(dir.path())
        .env("GLEP_SYNC_BUILD", "1")
        .arg("oldneedle")
        .assert()
        .success()
        .stdout("r.txt:1:oldneedle\n");
    assert!(dir.path().join(".glep/manifest.bin").exists());
    dir
}

/// `glep args` in `dir` with GLEP_TIMING: (stdout, stderr, exit code).
fn run_timed(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = glep(dir)
        .env("GLEP_TIMING", "1")
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// A file rewritten at its size within the clock tick of its last write
/// keeps its (mtime, size), so the freshness sweep's stat cannot see the
/// change. Its index entry was read within the racy window, so the next
/// query reads the file again: the new match appears and the old one is
/// gone, whether the search runs on the calling thread (a handful of
/// candidates) or on the worker pool.
#[test]
fn same_size_rewrites_in_the_same_tick_are_seen() {
    for (others, path) in [(3, "small"), (40, "pool")] {
        let dir = racy_tree(others);
        let d = dir.path();
        rewrite_in_the_same_tick(&d.join("r.txt"), "newneedle\n");
        let common: String = (0..others)
            .map(|i| format!("f{i:02}.txt:1:common\n"))
            .collect();
        let (out, err, code) = run_timed(d, &["-e", "common", "-e", "newneedle"]);
        assert_eq!(out, format!("{common}r.txt:1:newneedle\n"), "{path}");
        assert_eq!(code, 0, "{path}");
        assert!(err.contains(&format!("search path {path}")), "{err}");
        let (out, err, code) = run_timed(d, &["-e", "common", "-e", "oldneedle"]);
        assert_eq!(out, common, "{path}");
        assert_eq!(code, 0, "{path}");
        assert!(err.contains(&format!("search path {path}")), "{err}");
        let (out, _, code) = run_timed(d, &["-l", "oldneedle"]);
        assert_eq!((out.as_str(), code), ("", 1), "{path}");
        let (out, _, code) = run_timed(d, &["--files-without-match", "newneedle"]);
        let without: String = (0..others).map(|i| format!("f{i:02}.txt\n")).collect();
        assert_eq!((out, code), (without, 0), "{path}");
    }
}

/// Under another glep's lock a query writes nothing, yet it reads a racy
/// file again, live, and answers for it as it is now. Holding the lock
/// here stands in for a concurrent writer.
#[test]
fn same_tick_rewrites_are_seen_under_a_held_lock() {
    use fs2::FileExt;
    let dir = racy_tree(3);
    let d = dir.path();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(d.join(".glep/lock"))
        .unwrap();
    held.lock_exclusive().unwrap();
    rewrite_in_the_same_tick(&d.join("r.txt"), "newneedle\n");
    let manifest = std::fs::read(d.join(".glep/manifest.bin")).unwrap();
    for _ in 0..2 {
        glep(d)
            .arg("newneedle")
            .assert()
            .success()
            .stdout("r.txt:1:newneedle\n");
        glep(d).arg("oldneedle").assert().code(1).stdout("");
    }
    assert_eq!(
        std::fs::read(d.join(".glep/manifest.bin")).unwrap(),
        manifest,
        "a query under a held lock writes nothing"
    );
    FileExt::unlock(&held).unwrap();
    glep(d)
        .arg("newneedle")
        .assert()
        .success()
        .stdout("r.txt:1:newneedle\n");
    glep(d).arg("oldneedle").assert().code(1).stdout("");
}

/// --ttl keeps its meaning with racy entries: inside the window a query
/// answers from the index without a sweep, as after any edit, and the
/// first query that sweeps reads the racy file again, after which the
/// window answers with what that sweep read.
#[test]
fn ttl_still_skips_the_sweep_with_racy_entries() {
    let dir = racy_tree(3);
    let d = dir.path();
    rewrite_in_the_same_tick(&d.join("r.txt"), "newneedle\n");
    let swept = |args: &[&str]| -> (bool, String, i32) {
        let (out, err, code) = run_timed(d, args);
        (err.contains("sweep_walk"), out, code)
    };
    let found = || "r.txt:1:newneedle\n".to_string();
    assert_eq!(
        swept(&["--ttl", "600", "newneedle"]),
        (false, String::new(), 1)
    );
    assert_eq!(swept(&["newneedle"]), (true, found(), 0));
    assert_eq!(swept(&["--ttl", "600", "newneedle"]), (false, found(), 0));
    assert_eq!(
        swept(&["--ttl", "600", "oldneedle"]),
        (false, String::new(), 1)
    );
}
