//! -q's probe: a run that only asks whether anything matches searches the
//! indexed candidates whose (mtime, size) the index still records before
//! any freshness sweep, and stops at the first match. These tests hold it
//! to rg's exit codes when the index is stale in every way a candidate can
//! be (edited, deleted, binary now, ignored now, hidden now, behind a
//! symlink or an unreadable directory now), and check that the modes which
//! count every file never take it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const NEEDLE: &str = "needle_tok";

fn have_rg() -> bool {
    Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A tree to search, with a home of its own (no global gitignore unless a
/// test writes one) next to it.
struct Tree {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Tree {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, rel: &str, body: impl AsRef<[u8]>) {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// `prog args` in the tree, isolated from the user's git config:
    /// (stdout, stderr, exit code).
    fn run(&self, prog: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
        let home = self.home.path();
        let mut cmd = Command::new(prog);
        cmd.current_dir(self.root())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .env("GIT_CONFIG_SYSTEM", home.join("no-system-gitconfig"))
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GLEP_NO_PROBE")
            .env_remove("GLEP_TIMING")
            .args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }

    fn glep(&self, args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
        self.run(Path::new(env!("CARGO_BIN_EXE_glep")), args, env)
    }

    /// rg with glep's ignore semantics (.gitignore applies outside a git
    /// repository too): (stdout, exit code).
    fn rg(&self, args: &[&str]) -> (String, i32) {
        let (out, _, code) = self.rg_err(args);
        (out, code)
    }

    /// `rg`, with its stderr: (stdout, stderr, exit code).
    fn rg_err(&self, args: &[&str]) -> (String, String, i32) {
        let mut all = vec!["--no-require-git"];
        all.extend_from_slice(args);
        self.run(Path::new("rg"), &all, &[])
    }

    /// Every file and directory an hour old, so no index entry is racy and
    /// the probe trusts what the index recorded, then the index built.
    fn index(&self) {
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        age(self.root(), hour_ago);
        let (_, err, code) = self.glep(&["index"], &[]);
        assert_eq!(code, 0, "{err}");
    }

    /// glep -q with GLEP_TIMING: whether the probe settled the run, and
    /// the exit code. Stdout must be empty.
    fn probe(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, i32) {
        let mut env = env.to_vec();
        env.push(("GLEP_TIMING", "1"));
        let (out, err, code) = self.glep(args, &env);
        assert_eq!(out, "", "{args:?}: -q prints nothing");
        (err.contains("glep timing: probe settled"), code)
    }
}

fn age(dir: &Path, t: SystemTime) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let kind = e.file_type().unwrap();
        if e.file_name() == ".glep" || kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            age(&e.path(), t);
        } else {
            std::fs::File::options()
                .write(true)
                .open(e.path())
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
    }
}

/// `a_first.txt`, the first candidate in index order, holds the needle;
/// a pool of other files holds `extra` (files given as (path, body)).
fn tree(extra: &[(&str, &str)]) -> Tree {
    let t = Tree {
        dir: tempfile::tempdir().unwrap(),
        home: tempfile::tempdir().unwrap(),
    };
    t.write("a_first.txt", format!("x {NEEDLE} y\n"));
    t.write(".gitignore", "*.log\n");
    t.write("skip.log", format!("{NEEDLE} in an ignored file\n"));
    for i in 0..40 {
        t.write(
            &format!("pool/d{}/f{i:02}.txt", i % 4),
            format!("filler {i}\n"),
        );
    }
    t.write("z_last.txt", "plain\n");
    for (rel, body) in extra {
        t.write(rel, body);
    }
    t
}

/// The -q flag shapes every scenario runs under.
const SHAPES: &[&[&str]] = &[
    &["-q", NEEDLE],
    &["-q", "-F", NEEDLE],
    &["-q", "-i", "NEEDLE_TOK"],
    &["-q", "-w", NEEDLE],
    &["-q", "-c", NEEDLE],
    &["-q", "-l", NEEDLE],
    &["-q", "-e", "zz_absent_zz", "-e", NEEDLE],
    &["-q", NEEDLE, "."],
    &["-q", "-t", "txt", NEEDLE],
];

/// Shapes held to the run without the probe only: rg lets a -g glob
/// override ignore rules, which the index (probe or not) does not model.
const GLOB_SHAPES: &[&[&str]] = &[
    &["-q", "-g", "*.txt", NEEDLE],
    &["-q", "-g", "!pool", NEEDLE],
];

/// Every shape against rg, with and without the probe, twice (the first
/// run refreshes the index). Returns whether the probe settled the first
/// run of the plain shape.
fn assert_like_rg(t: &Tree, label: &str) -> bool {
    let mut settled = None;
    for args in SHAPES {
        let (r, rc) = t.rg(args);
        assert_eq!(r, "", "{label}: rg {args:?} printed");
        let (probe_settled, code) = t.probe(args, &[]);
        settled.get_or_insert(probe_settled);
        assert_eq!(code, rc, "{label}: {args:?}");
        let (_, code) = t.probe(args, &[("GLEP_NO_PROBE", "1")]);
        assert_eq!(code, rc, "{label}: {args:?} without the probe");
        let (_, code) = t.probe(args, &[]);
        assert_eq!(code, rc, "{label}: {args:?} again");
    }
    for args in GLOB_SHAPES {
        let (_, without) = t.probe(args, &[("GLEP_NO_PROBE", "1")]);
        let (_, code) = t.probe(args, &[]);
        assert_eq!(code, without, "{label}: {args:?}");
    }
    settled.unwrap()
}

#[test]
fn a_fresh_index_settles_in_the_probe() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.index();
    assert!(assert_like_rg(&t, "fresh"));
    // Plan::All patterns, -v, operands and file operands.
    for args in [
        &["-q", r"\w+"][..],
        &["-q", "-v", "zz_absent_zz"],
        &["-q", "[0-9]{2}"],
        &["-q", r"\w+", "pool/d1"],
        &["-q", NEEDLE, "pool"],
        &["-q", "filler 17", "pool"],
        &["-q", NEEDLE, "a_first.txt"],
        &["-q", NEEDLE, "z_last.txt"],
        &["-q", NEEDLE, "z_last.txt", "pool"],
        &["-q", NEEDLE, "z_last.txt", "a_first.txt"],
        &["-q", NEEDLE, "skip.log"],
        &["-q", "zz_absent_zz"],
        &["-q", "-v", r"\w*"],
    ] {
        let (_, rc) = t.rg(args);
        let (_, code) = t.probe(args, &[]);
        assert_eq!(code, rc, "{args:?}");
    }
    // A match the probe can settle is settled there, before any sweep.
    let (_, err, code) = t.glep(&["-q", NEEDLE], &[("GLEP_TIMING", "1")]);
    assert_eq!(code, 0);
    assert!(
        err.contains("probe settled") && !err.contains("sweep_walk"),
        "{err}"
    );
    let (_, err, _) = t.glep(&["-q", "filler 17", "pool"], &[("GLEP_TIMING", "1")]);
    assert!(err.contains("probe settled"), "{err}");
}

/// The first candidate changed after the index was built: the probe
/// passes over it, and the run answers for the tree as it is now.
#[test]
fn stale_first_candidates_answer_like_rg() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let other = format!("another {NEEDLE}\n");
    type Change = fn(&Tree);
    let cases: &[(&str, Change, bool)] = &[
        ("edited away", |t| t.write("a_first.txt", "x y\n"), false),
        (
            "deleted",
            |t| std::fs::remove_file(t.root().join("a_first.txt")).unwrap(),
            false,
        ),
        (
            "binary now",
            |t| t.write("a_first.txt", format!("x\0{NEEDLE}\n")),
            false,
        ),
        (
            "binary now, the needle first",
            |t| t.write("a_first.txt", format!("{NEEDLE}\nx\0\n")),
            false,
        ),
        (
            "ignored now",
            |t| t.write(".gitignore", "*.log\na_first.txt\n"),
            false,
        ),
        (
            "ignored by a new .ignore",
            |t| t.write(".ignore", "a_first.txt\n"),
            false,
        ),
        (
            "ignored by a new .rgignore",
            |t| t.write(".rgignore", "a_*\n"),
            false,
        ),
        (
            "ignored by the global gitignore",
            |t| {
                let xdg = t.home.path().join("xdg/git");
                std::fs::create_dir_all(&xdg).unwrap();
                std::fs::write(xdg.join("ignore"), "a_first.txt\n").unwrap();
            },
            false,
        ),
        (
            "renamed",
            |t| {
                std::fs::rename(t.root().join("a_first.txt"), t.root().join("b_moved.txt")).unwrap()
            },
            true,
        ),
        (
            "renamed to an ignored name",
            |t| std::fs::rename(t.root().join("a_first.txt"), t.root().join("moved.log")).unwrap(),
            false,
        ),
    ];
    for &(label, change, still) in cases {
        for with_other in [false, true] {
            let extra: &[(&str, &str)] = if with_other {
                &[("pool/d3/f07.txt", &other)]
            } else {
                &[]
            };
            let t = tree(extra);
            t.index();
            change(&t);
            let label = format!("{label}, another match: {with_other}");
            let (_, rc) = t.rg(&["-q", NEEDLE]);
            assert_eq!(rc, if still || with_other { 0 } else { 1 }, "{label}");
            // The other match is an unchanged candidate the probe settles
            // on; the changed first one never settles it.
            assert_eq!(assert_like_rg(&t, &label), with_other, "{label}");
        }
    }
}

/// The needle only in files the index has not read: new, or edited in.
#[test]
fn matches_the_index_has_not_seen_are_found() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    type Change = fn(&Tree);
    let cases: &[(&str, Change)] = &[
        ("a new file", |t| {
            t.write("pool/new.txt", format!("{NEEDLE}\n"))
        }),
        ("edited in", |t| {
            t.write("z_last.txt", format!("plain {NEEDLE}\n"))
        }),
        ("a new directory", |t| {
            t.write("newdir/deep/n.txt", format!("{NEEDLE}\n"))
        }),
        ("unignored", |t| t.write(".gitignore", "\n")),
    ];
    for &(label, change) in cases {
        let t = tree(&[]);
        t.write("a_first.txt", "x y\n");
        t.index();
        assert_eq!(t.probe(&["-q", NEEDLE], &[]), (false, 1), "{label}: before");
        change(&t);
        let (_, rc) = t.rg(&["-q", NEEDLE]);
        assert_eq!(rc, 0, "{label}");
        assert!(!assert_like_rg(&t, label), "{label}: nothing to probe");
    }
}

/// A whitelisted dotfile that the whitelist no longer rescues is hidden
/// again: rg searches it only under --hidden.
#[test]
fn a_dotfile_hidden_again_is_left_to_hidden() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.write("a_first.txt", "x y\n");
    t.write(".gitignore", "*.log\n!.keep\n");
    t.write(".keep", format!("{NEEDLE}\n"));
    t.index();
    assert_eq!(t.probe(&["-q", NEEDLE], &[]), (true, 0));
    t.write(".gitignore", "*.log\n");
    for args in [&["-q", NEEDLE][..], &["-q", "--hidden", NEEDLE]] {
        let (_, rc) = t.rg(args);
        let (_, code) = t.probe(args, &[]);
        assert_eq!(code, rc, "{args:?}");
    }
}

/// A directory on the candidate's path replaced by a symlink to where it
/// went: rg does not follow it, and neither does the probe.
#[cfg(unix)]
#[test]
fn a_directory_turned_symlink_is_not_followed() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.write("a_first.txt", "x y\n");
    t.write("sub/deep/n.txt", format!("{NEEDLE}\n"));
    t.index();
    assert_eq!(t.probe(&["-q", NEEDLE], &[]), (true, 0));
    let away = t.home.path().join("away");
    std::fs::rename(t.root().join("sub"), &away).unwrap();
    std::os::unix::fs::symlink(&away, t.root().join("sub")).unwrap();
    let (_, rc) = t.rg(&["-q", NEEDLE]);
    assert_eq!(rc, 1);
    assert_like_rg(&t, "symlinked directory");
    // Named as an operand, rg follows it.
    let (_, rc) = t.rg(&["-q", NEEDLE, "sub"]);
    let (_, code) = t.probe(&["-q", NEEDLE, "sub"], &[]);
    assert_eq!((code, rc), (0, 0));
}

/// A directory the walk cannot read: rg reports it and exits 2, unless a
/// match elsewhere answers -q. Its files are never searched, even though
/// a lookup by name still reaches them.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_keeps_rgs_exit_code() {
    use std::os::unix::fs::PermissionsExt;
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    for with_other in [false, true] {
        let t = tree(&[]);
        t.write("a_first.txt", "x y\n");
        t.write("sub/deep/n.txt", format!("{NEEDLE}\n"));
        if with_other {
            t.write("pool/d2/f10.txt", format!("{NEEDLE}\n"));
        }
        t.index();
        let locked = t.root().join("sub/deep");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o311)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: nothing is unreadable.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let (_, rc) = t.rg(&["-q", NEEDLE]);
        assert_eq!(rc, if with_other { 0 } else { 2 });
        let (_, code) = t.probe(&["-q", NEEDLE], &[]);
        let (_, err, again) = t.glep(&["-q", NEEDLE], &[]);
        let (_, before, _) = t.glep(&["-q", NEEDLE], &[("GLEP_NO_PROBE", "1")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!((code, again), (rc, rc), "another match: {with_other}");
        // Without a match the probe's own look at the directory reports
        // nothing: the sweep that follows reports it, as without the
        // probe. (With one, like rg's walk, -q may stop before reaching
        // the directory.)
        if !with_other {
            assert_eq!(
                err.matches("Permission denied").count(),
                before.matches("Permission denied").count(),
                "{err}"
            );
            assert!(err.contains("Permission denied"), "{err}");
        }
    }
}

/// A candidate that cannot be read: its error is reported once, whether
/// the probe or the run's search reads it first, as rg reports it once.
#[cfg(unix)]
#[test]
fn an_unreadable_candidate_is_reported_once() {
    use std::os::unix::fs::PermissionsExt;
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.index();
    let file = t.root().join("a_first.txt");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }
    for args in [
        &["-q", NEEDLE][..],
        &["-q", NEEDLE, "a_first.txt"],
        &["-q", NEEDLE, "a_first.txt", "."],
    ] {
        let (_, rerr, rc) = t.rg_err(args);
        let (out, err, code) = t.glep(args, &[]);
        assert_eq!((out.as_str(), code), ("", rc), "{args:?}");
        assert_eq!(
            err.matches("Permission denied").count(),
            rerr.matches("Permission denied").count(),
            "{args:?}: {err}"
        );
    }
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Missing operands keep rg's -q exit codes: a match wins over the error
/// (exit 0), a miss after it is exit 2, and --no-messages hides only the
/// text.
#[test]
fn missing_operands_keep_rgs_exit_codes() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.index();
    for args in [
        &["-q", NEEDLE, "no_such_path"][..],
        &["-q", NEEDLE, "no_such_path", "pool"],
        &["-q", NEEDLE, "no_such_path", "a_first.txt"],
        &["-q", "zz_absent_zz", "no_such_path"],
        &["-q", "zz_absent_zz", "no_such_path", "."],
        &["-q", NEEDLE, "no_such_path", "--no-messages"],
        &["-q", "zz_absent_zz", "no_such_path", "--no-messages"],
        &["-q", r"\w+", "no_such_path", "pool"],
        &["-q", NEEDLE, "no_such_path", "other_missing"],
    ] {
        let (_, rerr, rc) = t.rg_err(args);
        let (out, err, code) = t.glep(args, &[]);
        assert_eq!((out.as_str(), code), ("", rc), "{args:?}");
        assert_eq!(
            err.lines().filter(|l| l.contains("no_such_path")).count(),
            rerr.lines().filter(|l| l.contains("no_such_path")).count(),
            "{args:?}: {err}"
        );
    }
}

/// -q with --stats or --json counts every file, and --files-without-match
/// asks for a file without a match: none of them takes the probe, each
/// prints what it printed before, and the match counters are rg's (glep
/// counts the files it searched, the index's candidates, where rg counts
/// every file it walked).
#[test]
fn quiet_modes_that_need_every_file_skip_the_probe() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.index();
    let timeless = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| !l.contains("seconds"))
            .map(String::from)
            .collect()
    };
    for args in [
        &["-q", "--stats", NEEDLE][..],
        &["-q", "--stats", "zz_absent_zz"],
        &["--stats", "-q", "-c", NEEDLE],
        &["-q", "--stats", r"\w+"],
    ] {
        let (r, rc) = t.rg(args);
        let (g, err, code) = t.glep(args, &[("GLEP_TIMING", "1")]);
        assert!(!err.contains("glep timing: probe"), "{args:?}: {err}");
        let (before, _, before_code) = t.glep(args, &[("GLEP_NO_PROBE", "1")]);
        assert_eq!(
            (timeless(&g), code),
            (timeless(&before), before_code),
            "{args:?}"
        );
        assert_eq!(code, rc, "{args:?}");
        assert_eq!(
            timeless(&g)[..4],
            timeless(&r)[..4],
            "{args:?}: match counters"
        );
    }
    for args in [
        &["--json", "-q", NEEDLE][..],
        &["-q", "--json", "zz_absent_zz"],
    ] {
        let (r, rc) = t.rg(args);
        let (g, err, code) = t.glep(args, &[("GLEP_TIMING", "1")]);
        assert!(!err.contains("glep timing: probe"), "{args:?}: {err}");
        let stats = |s: &str| -> serde_json::Value {
            let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
            let s = &v["data"]["stats"];
            serde_json::json!([s["matches"], s["matched_lines"], s["searches_with_match"]])
        };
        assert_eq!((stats(&g), code), (stats(&r), rc), "{args:?}");
        assert_eq!(g.lines().count(), 1, "{args:?}: one summary line");
    }
    for args in [
        &["-q", "--files-without-match", NEEDLE][..],
        &["-q", "--files-without-match", "filler"],
    ] {
        let (_, rc) = t.rg(args);
        let (g, err, code) = t.glep(args, &[("GLEP_TIMING", "1")]);
        assert!(!err.contains("glep timing: probe"), "{args:?}: {err}");
        assert_eq!((g.as_str(), code), ("", rc), "{args:?}");
    }
}

/// Files every probe passes over: candidates by their trigrams (`foo` and
/// `bar` both present) with no line that matches `foo.*bar`.
fn decoys() -> Tree {
    let mut extra = Vec::new();
    for i in 0..12 {
        extra.push((format!("decoy/d{i:02}.txt"), format!("foo {i}\nbar {i}\n")));
    }
    let extra: Vec<(&str, &str)> = extra
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let t = tree(&extra);
    t.index();
    t
}

/// The files a search ran over, from GLEP_TIMING's search path line.
fn searched(err: &str) -> Option<usize> {
    let rest = err
        .lines()
        .find_map(|l| l.strip_prefix("glep timing: search path "))?;
    let n = rest.split('(').nth(1)?.split(' ').next()?;
    n.parse().ok()
}

/// A probe that finds nothing hands its files to the run, which searches
/// only the rest: the decoys it searched, still as they were, are not
/// read twice, and a match the index has not seen is still found.
#[test]
fn a_probe_without_a_match_hands_over_what_it_searched() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = decoys();
    let (_, err, code) = t.glep(&["-q", "foo.*bar"], &[("GLEP_TIMING", "1")]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("12 searched, no match"), "{err}");
    // Windows reads a listing's attributes from the directory entry, which
    // can lag the file's own: a decoy may then be searched again.
    if !cfg!(windows) {
        assert_eq!(searched(&err), Some(0), "{err}");
    }
    // Without the probe the run searches every decoy itself.
    let (_, err, code) = t.glep(
        &["-q", "foo.*bar"],
        &[("GLEP_TIMING", "1"), ("GLEP_NO_PROBE", "1")],
    );
    assert_eq!((code, searched(&err)), (1, Some(12)), "{err}");
    t.write("pool/d0/late.txt", "foo bar\n");
    let (_, rc) = t.rg(&["-q", "foo.*bar"]);
    let (_, err, code) = t.glep(&["-q", "foo.*bar"], &[("GLEP_TIMING", "1")]);
    assert_eq!((code, rc), (0, 0), "{err}");
}

/// The run skips a file the probe searched only while its (mtime, size)
/// are still what the probe read: one rewritten between the probe and the
/// run's search is searched again. A debug-build hook pauses the run right
/// after the probe.
#[cfg(debug_assertions)]
#[test]
fn a_file_changed_after_the_probe_is_searched_again() {
    use std::time::Instant;
    for change in [false, true] {
        let t = decoys();
        let pause = tempfile::tempdir().unwrap();
        let stderr = pause.path().join("stderr");
        let mut child = Command::new(env!("CARGO_BIN_EXE_glep"))
            .args(["-q", "foo.*bar"])
            .current_dir(t.root())
            .env("HOME", t.home.path())
            .env("XDG_CONFIG_HOME", t.home.path().join("xdg"))
            .env("GLEP_TIMING", "1")
            .env("GLEP_TEST_PAUSE_AFTER_PROBE", pause.path())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !pause.path().join("reached").exists() {
            assert!(child.try_wait().unwrap().is_none(), "glep exited early");
            assert!(Instant::now() < deadline, "glep never finished its probe");
            std::thread::sleep(Duration::from_millis(5));
        }
        if change {
            t.write("decoy/d05.txt", "foo 5\nbar 5\nfoo bar\n");
        }
        std::fs::write(pause.path().join("resume"), "").unwrap();
        let code = child.wait().unwrap().code();
        let err = std::fs::read_to_string(&stderr).unwrap();
        assert!(err.contains("12 searched, no match"), "{err}");
        assert_eq!(code, Some(if change { 0 } else { 1 }), "{err}");
        // Windows may search a decoy again (see the test above).
        if !cfg!(windows) {
            assert_eq!(searched(&err), Some(usize::from(change)), "{err}");
        }
    }
}

/// A racy entry (its content read within the racy window of its mtime)
/// rewritten in the same tick at the same size keeps its (mtime, size):
/// the probe leaves it to the run, whose sweep reads it again.
#[test]
fn a_same_tick_rewrite_of_a_racy_entry_is_seen() {
    let t = tree(&[]);
    t.write("a_first.txt", "x y\n");
    age(t.root(), SystemTime::now() - Duration::from_secs(3600));
    let r = t.root().join("r.txt");
    std::fs::write(&r, "oldneedle\n").unwrap();
    let pinned = SystemTime::now() + Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(&r)
        .unwrap()
        .set_modified(pinned)
        .unwrap();
    let (_, err, code) = t.glep(&["index"], &[]);
    assert_eq!(code, 0, "{err}");
    std::fs::write(&r, "newneedle\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&r)
        .unwrap()
        .set_modified(pinned)
        .unwrap();
    assert_eq!(t.probe(&["-q", "oldneedle"], &[]), (false, 1));
    assert_eq!(t.probe(&["-q", "newneedle"], &[]), (false, 0));
}

/// On a volume that ignores case, a file renamed to another case is found
/// under its new name, and an ignore rule for the new name applies.
#[test]
fn a_case_rename_is_judged_by_the_new_name() {
    if !have_rg() {
        eprintln!("quiet_probe: rg not installed, skipping");
        return;
    }
    let t = tree(&[]);
    t.write("a_first.txt", "x y\n");
    t.write("Needle.txt", format!("{NEEDLE}\n"));
    t.write(".gitignore", "*.log\nneedle.txt\n");
    t.index();
    if !t.root().join("needle.txt").exists() {
        // A case-sensitive volume: no other spelling names the file.
        return;
    }
    assert_eq!(t.probe(&["-q", NEEDLE], &[]), (true, 0));
    let tmp: PathBuf = t.root().join("tmp_rename");
    std::fs::rename(t.root().join("Needle.txt"), &tmp).unwrap();
    std::fs::rename(&tmp, t.root().join("needle.txt")).unwrap();
    let (_, rc) = t.rg(&["-q", NEEDLE]);
    assert_eq!(rc, 1);
    assert_like_rg(&t, "case rename");
}
