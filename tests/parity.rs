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
    // Binary fixtures: `binnul.bin` has a NUL before the pattern token, so
    // under quit detection the file reports nothing while -a prints raw
    // lines and --binary prints the binary-matches notice. `binpre.bin`
    // carries a match BEFORE the NUL to cover the suppression case.
    std::fs::write(dir.path().join("binnul.bin"), b"all\x00binary\x00here\n").unwrap();
    std::fs::write(dir.path().join("binpre.bin"), b"match before\nnul\x00after\n").unwrap();
    // Whitelist rescue (kernel-style): dotkit/.gitignore ignores all
    // dotfiles via `.*` then un-hides select ones with `!` rules. rg shows
    // whitelisted dotfiles and the contents of whitelisted dot-dirs; a
    // dotfile with no `!` rule stays ignored entirely (invisible even to
    // --hidden, since it is *ignored*, not merely hidden). Scoped to a
    // subdir so the top-level fixtures keep their plain hidden semantics.
    std::fs::create_dir_all(dir.path().join("dotkit/.wdir")).unwrap();
    std::fs::write(
        dir.path().join("dotkit/.gitignore"),
        ".*\n!.gitignore\n!.keepme\n!.wdir/\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("dotkit/.keepme"), "keeptoken whitelisted\n").unwrap();
    std::fs::write(dir.path().join("dotkit/.nope"), "nopetoken still hidden\n").unwrap();
    std::fs::write(dir.path().join("dotkit/.wdir/in.txt"), "wdirtoken under whitelisted dir\n").unwrap();
    std::fs::write(dir.path().join("dotkit/plain.txt"), "plaintoken normal file\n").unwrap();
    // Leading-whitespace fixture for --trim and a multi-match line for
    // --vimgrep's one-line-per-match output.
    std::fs::write(dir.path().join("pad.txt"), "   padded hello   \n").unwrap();
    std::fs::write(dir.path().join("mm.txt"), "aa bb aa\nplain\n").unwrap();
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
        // Binary files: default quit detection suppresses them entirely
        // (even a match before the NUL); --binary reports the notice;
        // -a prints raw bytes. -c/-l under --binary report real results.
        &["binary"],
        &["--binary", "binary"],
        &["--binary", "-l", "binary"],
        &["--binary", "-c", "binary"],
        &["-a", "binary"],
        &["-a", "-c", "binary"],
        &["-a", "-l", "binary"],
        &["match"],
        &["--binary", "match"],
        &["-a", "match"],
        // Last-wins pair: -a --binary behaves as --binary.
        &["-a", "--binary", "binary"],
        &["--binary", "-a", "binary"],
        // --files listing under --no-ignore: rg ignores -n/--no-heading/
        // --color for --files (verified manually against real rg), and
        // --sort path still applies, so the harness's fixed rg flag set
        // composes cleanly with --files --no-ignore for both tools.
        &["--no-ignore", "--files"],
        // Whitelist rescue: whitelisted dotfiles are searched and listed;
        // `nopetoken` is ignored (not merely hidden) so nothing shows it.
        &["--files"],
        &["keeptoken"],
        &["wdirtoken"],
        &["nopetoken"],
        &["--hidden", "nopetoken"],
        &["-l", "keeptoken"],
        // Output plumbing: column/byte-offset/vimgrep/trim/null,
        // path-separator, include-zero counts, depth limits, filename
        // gating, and the single-file-operand heuristic.
        &["--column", "hello"],
        &["-b", "hello"],
        &["--vimgrep", "aa"],
        &["--vimgrep", "hello"],
        &["--trim", "padded"],
        &["--null", "hello"],
        &["-0", "-l", "hello"],
        &["--null", "--files"],
        &["-c", "--include-zero", "hello"],
        &["--include-zero", "-c", "zz_no_match_zz"],
        &["-c", "--null", "hello"],
        &["--path-separator", "%", "hello"],
        &["--max-depth", "1", "hello"],
        &["--max-depth", "0", "hello"],
        &["-I", "hello"],
        &["-j", "1", "hello"],
        // Single-file operand: rg drops the path prefix by default.
        &["hello", "README.md"],
        &["-c", "hello", "README.md"],
        &["-H", "hello", "README.md"],
        // repeatable -e: union of patterns; positional still a path
        &["-e", "hello", "-e", "answer", "-l"],
        &["-e", "hello", "-l", "src"],
        &["-F", "-e", "fn m", "-e", "answer", "-l"],
        // -g gitignore-style negation & last-wins
        &["-g", "!*.log", "-l", "hello"],
        &["--iglob", "!*.log", "-l", "hello"],
        &["-g", "*.rs", "-l", "hello"],
        &["-g", "*.rs", "-g", "!*main*", "-l", "fn main"],
        // --multiline-dotall + -U: . spans newlines
        &["-U", "--multiline-dotall", "a.b\nc.d"],
        // -T type exclusion
        &["-T", "rust", "-l", "hello"],
        // --count-matches counts occurrences (single-file operand keeps
        // ordering identical between the tools)
        &["--count-matches", "Xb"],
        // --sort/--sortr on the small corpus
        &["--sort", "modified", "-l", "hello"],
        &["--sortr", "path", "-l", "hello"],
        // --color always: single-file match keeps order identical;
        // ANSI sequences compared literally
        &["--color", "always", "fn main"],
        // -u/-uu fold into --no-ignore/--hidden
        &["-u", "-l", "hello"],
        // --passthru: all lines of all searched files print
        &["--passthru", "answer"],
        // --no-unicode: \w narrows to ASCII
        &["--no-unicode", "\\w+", "-l"],
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
