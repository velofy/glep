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
    // -w/-x fixtures: "foo" as substring, as a word, and as a whole line.
    std::fs::write(
        dir.path().join("words.txt"),
        "aaafooaaa\nfoo bar\nxfooy\nfoo\n",
    )
    .unwrap();
    // -wF/-xF fixtures: the literal "foo.bar" whole-word and whole-line.
    std::fs::write(dir.path().join("dots.txt"), "foo.bar\nfooXbar\nafoo.bar\n").unwrap();
    // -S fixture: all three case variants on separate lines so sensitive
    // and insensitive resolutions print different line sets.
    std::fs::write(dir.path().join("case.txt"), "foo\nFOO\nFoo\n").unwrap();
    // -m + context fixture: the second match arrives after the limit as
    // after-context (rg prints match1, ctx2, match2 for -m1 -A2).
    std::fs::write(dir.path().join("ctx.txt"), "ctx1\nmatch1\nctx2\nmatch2\nctx3\n").unwrap();
    // -M fixture: one line far over a small column limit plus a short
    // matching line. Also gives --heading -C1 an intra-file `--` break.
    std::fs::write(
        dir.path().join("long.txt"),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nfoo here\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("gap.txt"), "foo\nx1\nx2\nx3\nx4\nfoo\n").unwrap();
    // Hidden-files fixture for --hidden: a .github-style nested hidden
    // directory (ci.yml itself is not dot-prefixed, only its .github
    // ancestor is) plus a plain top-level dotfile. Both carry a token that
    // appears nowhere else in the corpus, so a --hidden search for it is
    // unambiguous, and neither is matched by the .gitignore/.ignore rules
    // above, so their visibility is governed purely by the hidden default.
    std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    std::fs::write(dir.path().join(".github/workflows/ci.yml"), "name: hiddentoken_ci\n").unwrap();
    std::fs::write(dir.path().join(".hidden.txt"), "hiddentoken plain dotfile\n").unwrap();
    // UTF-16LE with BOM: rg auto-transcodes and searches it, so glep must
    // too (its NUL-heavy raw bytes must not mark it binary). bin.dat is
    // NUL-heavy with no BOM: binary in both tools, invisible by default.
    // "utf16token" appears nowhere else, giving it a unique needle.
    let mut u16le = vec![0xFF, 0xFE];
    for b in "hello utf16token\n".bytes() {
        u16le.push(b);
        u16le.push(0);
    }
    std::fs::write(dir.path().join("utf16le.txt"), &u16le).unwrap();
    std::fs::write(dir.path().join("bin.dat"), b"hello\x00\x01\x02bin\n").unwrap();
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
        // utf16token lives only in utf16le.txt (UTF-16LE + BOM): rg
        // transcodes and finds it; bin.dat contains "hello" but is binary
        // and stays invisible, which the "hello" cases above also pin.
        &["utf16token"],
        &["-c", "utf16token"],
        &["-l", "utf16token"],
        &["-U", "goodbye\\nfoo"],
        &["-U", "-c", "a.b\\nc.d"],
        // Hidden files invisible by default in both tools.
        &["hiddentoken"],
        // --hidden reveals them. The corpus has no .git dir, so plain
        // `rg --hidden` (the rg_out helper's flags, no special-casing)
        // matches glep's semantics exactly here; the divergence noted in
        // README (.git always excluded from glep, not from rg --hidden) is
        // a deliberate one this corpus does not exercise.
        &["--hidden", "hiddentoken"],
        // --no-ignore: skipme.log (gitignored) and skipme2.txt (.ignore'd)
        // both carry "hello", invisible by default (see the plain "hello"
        // case above, which the corpus fixture comment confirms excludes
        // them) and visible once ignore sources are bypassed. Still hidden
        // by default: no dotfile in the corpus contains "hello", so this
        // case alone wouldn't catch a hidden-gating regression, but the
        // dedicated tests/cli.rs cases do.
        &["--no-ignore", "hello"],
        &["--no-ignore", "-l", "hello"],
        // --files listing under --no-ignore: rg ignores -n/--no-heading/
        // --color for --files (verified manually against real rg), and
        // --sort path still applies, so the harness's fixed rg flag set
        // composes cleanly with --files --no-ignore for both tools.
        &["--no-ignore", "--files"],
        // -w/-x: word and whole-line matching, plain and with -F. -xF
        // exercises literal escaping inside the whole-line wrapper.
        &["-w", "foo"],
        &["-w", "-F", "foo.bar"],
        &["-x", "foo"],
        &["-x", "-F", "foo.bar"],
        // -w/-x are a last-wins pair in rg: `-x -w` == `-w` alone, and
        // `-w -x` == `-x` alone.
        &["-x", "-w", "foo"],
        &["-w", "-x", "foo"],
        // -S: lowercase-only pattern resolves insensitive (case.txt's FOO
        // and Foo lines must appear); a pattern with uppercase resolves
        // sensitive; an inline (?i) beats the pattern's uppercase bytes and
        // forces insensitive. The (?i) case is also a narrowing regression
        // test: planning -S case-sensitively would drop files (README.md,
        // case.txt) whose only "FOO" is lowercase.
        &["-S", "foo"],
        &["-S", "Foo"],
        &["-S", "(?i)FOO"],
        &["-S", "-F", "FOO"],
        &["-i", "-S", "hello"],
        // -v: inversion disables trigram narrowing (a file with zero
        // occurrences still matches every line); util.rs, unicode.txt and
        // friends carry no "foo"/"hello" literal, so this catches the
        // unsound-narrowing regression.
        &["-v", "foo"],
        &["-v", "hello"],
        &["-v", "-c", "foo"],
        &["-v", "-l", "foo"],
        &["-x", "-v", "foo"],
        &["-U", "-v", "hello"],
        // -m: per-file cap on matched lines; Some(0) prints nothing and
        // exits 1; interacts with context (overflow matches arrive as
        // after-context) and -l/-c.
        &["-m", "1", "foo"],
        &["-m", "0", "foo"],
        &["-m", "1", "-l", "foo"],
        &["-c", "-m", "1", "foo"],
        &["-m", "1", "-A", "2", "match"],
        // -o: print each match on its own line; under -c it counts matches
        // not matched lines; under -v it prints whole non-matching lines.
        &["-o", "foo"],
        &["-o", "-c", "foo"],
        &["-o", "-v", "hello"],
        &["-o", "-m", "1", "foo"],
        // -n is a no-op alias for the default; -N drops the number but
        // keeps the path prefix.
        &["-n", "hello"],
        &["-N", "hello"],
        // --heading: path on its own line, blank line between file groups;
        // with context, intra-file gaps still get `--` (gap.txt).
        &["--heading", "hello"],
        &["--heading", "-C", "1", "foo"],
        // -M: over-long lines are replaced by an omission note; 0 = no cap.
        &["-M", "5", "a+"],
        &["-M", "0", "a+"],
        // -q: no output at all; only the exit code answers.
        &["-q", "hello"],
        &["-q", "zz_no_match_zz"],
    ];
    for args in patterns {
        let (g, gc) = glep_out(dir.path(), args);
        let (r, rc) = rg_out(dir.path(), args);
        assert_eq!(g, r, "stdout diverged for {:?}", args);
        assert_eq!(gc, rc, "exit code diverged for {:?}", args);
    }
}
