use assert_cmd::Command;
use std::path::Path;

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
