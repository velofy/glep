use assert_cmd::Command;
use std::path::Path;

fn have_rg() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "use std::io;\nfn main() {\n    println!(\"hello world\");\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/deep/util.rs"),
        "pub fn helper() -> u32 {\n    42 // the answer\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("README.md"),
        "# demo\nhello and goodbye\nfoo bar baz\n",
    )
    .unwrap();
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(dir.path().join("skipme.log"), "hello hidden\n").unwrap();
    // .ignore is a distinct source from .gitignore (ripgrep/rg-specific,
    // not a git concept): exercises the macOS bulk sweep's walker-fallback
    // path end to end (see src/walk_bulk.rs's divergence trap) alongside
    // the .gitignore case above.
    std::fs::write(dir.path().join(".ignore"), "skipme2.txt\n").unwrap();
    std::fs::write(dir.path().join("skipme2.txt"), "hello hidden via dot-ignore\n").unwrap();
    std::fs::write(
        dir.path().join("unicode.txt"),
        "caf\u{e9} au lait\nCAF\u{c9} AU LAIT\nplain line\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("adjacent.txt"), "aXb\ncXd\naXb\ncXd\n").unwrap();
    dir
}

fn glep_out(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::cargo_bin("glep")
        .unwrap()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

fn rg_out(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = std::process::Command::new("rg")
        .current_dir(dir)
        .args(["-n", "--no-heading", "--color=never", "--sort", "path", "--no-require-git"])
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn parity_with_ripgrep() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = corpus();
    let patterns: &[&[&str]] = &[
        &["hello"],
        &["-i", "HELLO"],
        &["-F", "hello world"],
        &["foo|goodbye"],
        &["^use "],
        &["hel.o"],
        &["a"],          // falls back to All: still must match rg
        &["-l", "hello"],
        &["-C", "1", "answer"],
        &["zz_no_match_zz"],
        &["-i", "caf\u{e9}"],
        &["-C", "1", "hello"],
        &["-A", "1", "hello"],
        &["-B", "1", "answer"],
        &["-c", "hello"],
        &["-c", "-i", "HELLO"],
        &["-c", "-C", "1", "hello"],
        &["-U", "goodbye\\nfoo"],
        &["-U", "-c", "a.b\\nc.d"],
    ];
    for args in patterns {
        let (g, gc) = glep_out(dir.path(), args);
        let (r, rc) = rg_out(dir.path(), args);
        assert_eq!(g, r, "stdout diverged for {:?}", args);
        assert_eq!(gc, rc, "exit code diverged for {:?}", args);
    }
}
