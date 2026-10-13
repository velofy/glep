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
    std::fs::write(
        dir.path().join("skipme2.txt"),
        "hello hidden via dot-ignore\n",
    )
    .unwrap();
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
    std::fs::write(
        dir.path().join("ctx.txt"),
        "ctx1\nmatch1\nctx2\nmatch2\nctx3\n",
    )
    .unwrap();
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
    std::fs::write(
        dir.path().join(".github/workflows/ci.yml"),
        "name: hiddentoken_ci\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".hidden.txt"),
        "hiddentoken plain dotfile\n",
    )
    .unwrap();
    // Binary fixtures: `binnul.bin` has a NUL before the pattern token, so
    // under quit detection the file reports nothing while -a prints raw
    // lines and --binary prints the binary-matches notice. `binpre.bin`
    // carries a match BEFORE the NUL to cover the suppression case.
    std::fs::write(dir.path().join("binnul.bin"), b"all\x00binary\x00here\n").unwrap();
    std::fs::write(
        dir.path().join("binpre.bin"),
        b"match before\nnul\x00after\n",
    )
    .unwrap();
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
    std::fs::write(
        dir.path().join("dotkit/.wdir/in.txt"),
        "wdirtoken under whitelisted dir\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dotkit/plain.txt"),
        "plaintoken normal file\n",
    )
    .unwrap();
    // Leading-whitespace fixture for --trim and a multi-match line for
    // --vimgrep's one-line-per-match output.
    std::fs::write(dir.path().join("pad.txt"), "   padded hello   \n").unwrap();
    std::fs::write(dir.path().join("mm.txt"), "aa bb aa\nplain\n").unwrap();
    // CRLF fixture for --crlf: matches on the first and last lines, a
    // non-matching line between them, and `$` anchors that only match
    // before `\r\n` in CRLF mode.
    std::fs::write(
        dir.path().join("crlf.txt"),
        "crlf hello\r\nsecond line\r\nhello end\r\n",
    )
    .unwrap();
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
        .args([
            "-n",
            "--no-heading",
            "--color=never",
            "--sort",
            "path",
            "--no-require-git",
        ])
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

/// The core rg parity cases, run over both corpora below.
const BASE_CASES: &[&[&str]] = &[
    &["hello"],
    &["-i", "HELLO"],
    &["-F", "hello world"],
    &["foo|goodbye"],
    &["^use "],
    &["hel.o"],
    &["a"], // falls back to All: still must match rg
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
    // --vimgrep always prints the path (editors run `rg --vimgrep pat
    // file`) unless -I says otherwise, and never prints headings.
    &["--vimgrep", "hello", "README.md"],
    &["--vimgrep", "-C", "1", "hello", "README.md"],
    &["--vimgrep", "-I", "hello", "README.md"],
    &["--heading", "--vimgrep", "aa"],
    &["--heading", "--vimgrep", "hello", "README.md", "pad.txt"],
    // repeatable -e: union of patterns; positional still a path
    &["-e", "hello", "-e", "answer", "-l"],
    &["-e", "hello", "-l", "src"],
    &["-F", "-e", "fn m", "-e", "answer", "-l"],
    // -g gitignore-style negation & last-wins
    &["-g", "!*.log", "-l", "hello"],
    &["--iglob", "!*.log", "-l", "hello"],
    &["-g", "*.rs", "-l", "hello"],
    &["-g", "*.rs", "-g", "!*main*", "-l", "fn main"],
    // A `!dir` glob prunes the directory itself, wherever it sits, as
    // rg's walker does; directory-only and nested forms too.
    &["-g", "!src", "-l", "hello"],
    &["-g", "!src/", "-l", "hello"],
    &["-g", "!src/deep", "-l", "answer"],
    &["-g", "!**/deep", "-l", "answer"],
    &["--iglob", "!SRC", "-l", "hello"],
    &["--files", "-g", "!src"],
    &["-g", "*.rs", "-g", "!src/deep", "-l", "fn|answer"],
    &["-g", "!pool/d1", "-c", "hello"],
    &["-g", "!pool/d1/", "-c", "hello"],
    &["-g", "!d2", "-c", "foo"],
    // A -g or -t whitelist rescues a hidden file, not the contents of a
    // hidden directory unless a glob whitelists the directory as well.
    &["-g", "*.txt", "-l", "hiddentoken"],
    &["-t", "txt", "-l", "hiddentoken"],
    &["--files", "-t", "txt"],
    &["-g", "*.yml", "-l", "hiddentoken"],
    &["-g", ".github", "-g", "*.yml", "-l", "hiddentoken"],
    // An operand is never filtered itself: a hidden directory operand is
    // searched like any other.
    &["hiddentoken", ".github"],
    &["-c", "hiddentoken", ".github"],
    &["-l", "wdirtoken", "dotkit"],
    &["nopetoken", "dotkit"],
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

#[test]
fn parity_with_ripgrep() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    check_cases(corpus().path(), "small", BASE_CASES);
}

/// The same cases over `pool_corpus()`, where the common needles hit
/// more files than search.rs's SMALL_WORKLOAD, so those searches run
/// through the worker pool and the in-order printer thread.
#[test]
fn parity_with_ripgrep_on_the_worker_pool() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = pool_corpus();
    for args in [
        &["hello"][..],
        &["-i", "HELLO"],
        &["foo"],
        &["-c", "hello"],
        &["-l", "foo"],
    ] {
        assert_eq!(search_path(dir.path(), args), "pool", "{args:?}");
    }
    check_cases(dir.path(), "pool", BASE_CASES);
}

/// Which search path glep took ("small" or "pool"), from GLEP_TIMING.
fn search_path(dir: &Path, args: &[&str]) -> String {
    let out = Command::cargo_bin("glep")
        .unwrap()
        .current_dir(dir)
        .env("GLEP_TIMING", "1")
        .args(args)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    err.lines()
        .find_map(|l| l.strip_prefix("glep timing: search path "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or("none")
        .to_string()
}

/// Stamp every regular file under `dir` (except the index) with the same
/// modification time, so timestamp sorts are decided entirely by their
/// tie-break. Coarse filesystem clocks (Linux's jiffy-granular inode
/// timestamps, NTFS) produce such ties naturally for files written in a
/// burst, which is how the plain `--sort modified` case above diverged on
/// Linux CI.
fn stamp_all(dir: &Path, t: std::time::SystemTime) {
    for ent in walkdir(dir) {
        if ent.components().any(|c| c.as_os_str() == ".glep") {
            continue;
        }
        let f = std::fs::File::options().write(true).open(&ent).unwrap();
        f.set_modified(t).unwrap();
    }
}

fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// rg sorts timestamp modes with a STABLE sort over its single-threaded,
/// unsorted walk, so equal timestamps keep directory-entry order (hash
/// order on ext4 and APFS, case-insensitive name order on NTFS), and
/// --sortr reverses the timestamp comparison only, never the ties. glep
/// must reproduce that exactly on every platform, for listings and for
/// searches, on the small path and on the worker-pool path.
#[test]
fn sort_by_timestamp_ties_follow_rg_walk_order() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = corpus();
    // Enough extra files that searches take the worker-pool path, with
    // names whose byte order, case-folded order and readdir order differ.
    for i in 0..48 {
        let name = match i % 4 {
            0 => format!("Upper{i:02}.txt"),
            1 => format!("lower{i:02}.txt"),
            2 => format!("_under{i:02}.txt"),
            _ => format!("sub{}/n{i:02}.txt", i % 3),
        };
        let p = dir.path().join(&name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("hello tie {i}\n")).unwrap();
    }
    let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    stamp_all(dir.path(), t0);
    let cases: &[&[&str]] = &[
        &["--sort", "modified", "-l", "hello"],
        &["--sortr", "modified", "-l", "hello"],
        &["--sort", "modified", "--files"],
        &["--sortr", "modified", "--files"],
        &["--sort", "modified", "hello"],
        &["--sort", "modified", "-c", "hello"],
        &["--sortr", "path", "-l", "hello"],
        &["--sortr", "path", "--files"],
        &["--sort", "path", "--files"],
    ];
    let check = |label: &str| {
        for args in cases {
            let (g, gc) = glep_out(dir.path(), args);
            let (r, rc) = rg_out(dir.path(), args);
            assert_eq!(g, r, "{label}: stdout diverged for {:?}", args);
            assert_eq!(gc, rc, "{label}: exit code diverged for {:?}", args);
        }
    };
    check("all timestamps tied");
    // Now give a few files distinct, later timestamps: they must move to
    // the end (or the front, reversed) while the rest keep walk order.
    for (k, name) in ["lower01.txt", "README.md", "sub1/n07.txt"]
        .iter()
        .enumerate()
    {
        let f = std::fs::File::options()
            .write(true)
            .open(dir.path().join(name))
            .unwrap();
        f.set_modified(t0 + std::time::Duration::from_secs(100 - 10 * k as u64))
            .unwrap();
    }
    check("partially tied");
}

/// search.rs's SMALL_WORKLOAD: searches over more candidate files than
/// this run through the worker pool instead of the calling thread.
const SMALL_WORKLOAD: usize = 32;

/// `corpus()` plus 48 files spread over several directories, every one
/// of them carrying `hello` and `foo` (and a mix of `match`, `answer`,
/// `aa`, `use` and CRLF line endings), so searches for the common
/// needles keep more than SMALL_WORKLOAD candidates after index
/// narrowing and take the worker-pool path.
const POOL_FILES: usize = 48;
const _: () = assert!(POOL_FILES > SMALL_WORKLOAD);

fn pool_corpus() -> tempfile::TempDir {
    let dir = corpus();
    for i in 0..POOL_FILES {
        let p = dir.path().join(format!("pool/d{}/f{i:02}.txt", i % 4));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let body = match i % 4 {
            0 => format!("before {i}\nhello pool {i} foo\nafter {i}\n"),
            1 => format!("foo {i} and foo again\nhello again\nmatch {i} answer\n"),
            2 => format!("say hello\naa bb aa {i}\nfoo bar baz\nuse x;\n"),
            _ => format!("Hello Foo {i}\r\nhello crlf\r\nfoo\r\nmatch {i}\r\n"),
        };
        std::fs::write(&p, body).unwrap();
    }
    dir
}

fn check_cases(dir: &Path, label: &str, cases: &[&[&str]]) {
    for args in cases {
        let (g, gc) = glep_out(dir, args);
        let (r, rc) = rg_out(dir, args);
        assert_eq!(g, r, "{label}: stdout diverged for {:?}", args);
        assert_eq!(gc, rc, "{label}: exit code diverged for {:?}", args);
    }
}

/// Under --color always rg styles the path of every summary record (-l,
/// -c, --count-matches, --files-without-match) and of the --files listing
/// with the path color spec, and wraps it in an OSC 8 hyperlink when
/// --hyperlink-format is set. Without color neither happens. Invalid
/// --color, --colors and --hyperlink-format values are flag errors.
#[test]
fn colored_and_hyperlinked_paths_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["--color", "always", "-l", "hello"],
        &["--color", "always", "-c", "hello"],
        &["--color", "always", "--count-matches", "hello"],
        &["--color", "always", "--files-without-match", "hello"],
        &["--color", "always", "--files"],
        &["--color", "always", "-0", "-l", "hello"],
        &["--color", "always", "-0", "-c", "hello"],
        &["--color", "always", "--path-separator", "%", "-c", "hello"],
        &["--color", "always", "--path-separator", "%", "--files"],
        &["--color", "always", "-c", "--include-zero", "hello"],
        &[
            "--color",
            "always",
            "--colors",
            "path:fg:red",
            "--colors",
            "path:style:bold",
            "-l",
            "hello",
        ],
        &["--color", "always", "--colors", "path:none", "-c", "hello"],
        &["--color", "ansi", "-l", "hello"],
        &["--color", "always", "--heading", "hello"],
        &["--color", "always", "-C", "1", "hello"],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "-l",
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "file://{path}",
            "-c",
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "kitty",
            "--files-without-match",
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "--files",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "none",
            "-l",
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "vscode",
            "--vimgrep",
            "hello",
        ],
        &[
            "--color",
            "never",
            "--hyperlink-format",
            "default",
            "-l",
            "hello",
        ],
        &["--hyperlink-format", "default", "-c", "hello"],
        // Flag errors: exit 2, nothing on stdout.
        &["--color", "bogus", "hello"],
        &["--colors", "path:fg:nope", "hello"],
        &["--colors", "zz:fg:red", "hello"],
        &["--hyperlink-format", "{bogus}", "hello"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    assert_eq!(search_path(pool.path(), &["hello"]), "pool");
    check_cases(pool.path(), "pool", cases);
}

/// --hostname-bin names a PROGRAM whose trimmed stdout becomes {host};
/// a program that cannot run falls back to the platform hostname.
#[cfg(unix)]
#[test]
fn hostname_bin_runs_a_program_like_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = corpus();
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("fakehost");
    std::fs::write(&bin, "#!/bin/sh\necho '  fake.host  '\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = bin.to_str().unwrap();
    let cases: &[&[&str]] = &[
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "--hostname-bin",
            bin,
            "-l",
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "--hostname-bin",
            bin,
            "hello",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "--hostname-bin",
            bin,
            "--files",
        ],
        &[
            "--color",
            "always",
            "--hyperlink-format",
            "default",
            "--hostname-bin",
            "/nonexistent/glep-test-host",
            "-c",
            "hello",
        ],
    ];
    check_cases(dir.path(), "hostname-bin", cases);
    let (g, _) = glep_out(dir.path(), cases[0]);
    assert!(g.contains("file://fake.host/"), "{g:?}");
}

/// Every record a printer writes ends with the searcher's line
/// terminator: CRLF under --crlf, NUL under --null-data (a last-wins
/// pair). That covers -c/-l/--count-matches records and the separators
/// between files. Separator flags decode escapes; --context-separator ""
/// still prints a line break while --no-context-separator prints none;
/// --heading always separates files with a bare line break; --passthru
/// and -A/-B/-C are a last-wins pair with no file separator under
/// --passthru.
#[test]
fn terminators_and_separators_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["--crlf", "hello"],
        &["--crlf", "hello$"],
        &["--crlf", "-c", "hello"],
        &["--crlf", "-l", "hello"],
        &["--crlf", "--count-matches", "hello"],
        &["--crlf", "--files-without-match", "hello"],
        &["--crlf", "-0", "--files-without-match", "hello"],
        &["--crlf", "-c", "--include-zero", "hello"],
        &["--crlf", "-C", "1", "hello"],
        &["--crlf", "--heading", "hello"],
        &["--crlf", "--heading", "-C", "1", "foo"],
        &["--crlf", "-o", "hello"],
        &["--crlf", "--vimgrep", "hello"],
        &["--crlf", "-0", "-l", "hello"],
        &["--null-data", "hello"],
        &["--null-data", "-l", "hello"],
        &["--null-data", "-c", "hello"],
        &["--null-data", "-C", "1", "hello"],
        &["--null-data", "--heading", "hello"],
        &["--null-data", "--files-without-match", "hello"],
        &["--crlf", "--null-data", "-c", "hello"],
        &["--null-data", "--crlf", "-c", "hello"],
        &["--null-data", "--crlf", "-l", "hello"],
        &["-C", "1", "--context-separator", "", "hello"],
        &["-C", "1", "--context-separator", "==", "hello"],
        &["-C", "1", "--context-separator", "\\x3d\\t", "hello"],
        &["--heading", "--context-separator", "==", "hello"],
        &["--heading", "-C", "1", "--context-separator", "==", "foo"],
        &["--heading", "-C", "1", "--context-separator", "", "foo"],
        &["-C", "1", "--no-context-separator", "hello"],
        &[
            "-C",
            "1",
            "--no-context-separator",
            "--context-separator",
            "XX",
            "hello",
        ],
        &[
            "-C",
            "1",
            "--context-separator",
            "XX",
            "--no-context-separator",
            "hello",
        ],
        &["--heading", "--no-context-separator", "hello"],
        &["--passthru", "-C", "1", "hello"],
        &["-C", "1", "--passthru", "hello"],
        &["-A", "1", "--passthru", "-B", "1", "hello"],
        &["--passthrough", "hello"],
        &["--passthru", "--heading", "hello"],
        &["--field-match-separator", "\\t", "hello"],
        &["--field-context-separator", "\\x2b", "-C", "1", "hello"],
        &["--path-separator", "\\x25", "hello"],
        &["--path-separator", "", "-l", "hello"],
        &["--path-separator", "ab", "hello"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    assert_eq!(search_path(pool.path(), &["hello"]), "pool");
    check_cases(pool.path(), "pool", cases);
}

/// rg's parser gives a flag that takes a value the next word verbatim,
/// even one that starts with a dash. The agent hooks pass every Grep
/// pattern as `-e '<pattern>'`, so a pattern like `-> u32` or `--` must
/// search instead of failing as an unknown flag; separators, globs and
/// replacements that start with a dash work the same way, and a bad
/// value is still a flag error (exit 2).
#[test]
fn dash_leading_flag_values_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["-e", "-> u32"],
        &["--regexp", "-> u32", "-l"],
        &["-F", "-e", "-> u32"],
        &["-e", "hello", "-e", "-> u32", "-c"],
        &["-e", "--", "-c"],
        &["-e", "-1", "-c"],
        &["-r", "-X-", "hello"],
        &["-o", "-r", "--", "hel+o"],
        &["-C", "1", "--context-separator", "--", "hello"],
        &["-C", "1", "--context-separator", "-x-", "hello"],
        &["-C", "1", "--field-context-separator", "-x", "hello"],
        &["--field-match-separator", "-", "hello"],
        &["--path-separator", "-", "-l", "hello"],
        &["-g", "-*", "-l", "hello"],
        &["-m", "-1", "hello"],
        &["-C", "-1", "hello"],
        &["-e"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    check_cases(pool.path(), "pool", cases);
}

/// The matcher is configured knob for knob like rg's: \n is the line
/// terminator outside -U (a pattern that must match it is an error),
/// -U --crlf keeps CRLF-aware anchors without a line terminator, a
/// pattern that must match NUL is an error unless -a, -s/-i/-S are a
/// last-wins trio, --engine auto falls back to PCRE2 per pattern, and
/// -m 0 or an empty pattern set (without -v) exits 1 without searching.
/// Pattern files keep empty lines (an empty pattern matches every line).
#[test]
fn matcher_config_matches_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let pats = tempfile::tempdir().unwrap();
    let with_empty = pats.path().join("with_empty.txt");
    std::fs::write(&with_empty, "foo\n\n").unwrap();
    let two = pats.path().join("two.txt");
    std::fs::write(&two, "hello\r\nanswer\n").unwrap();
    let none = pats.path().join("none.txt");
    std::fs::write(&none, "").unwrap();
    let (with_empty, two, none) = (
        with_empty.to_str().unwrap(),
        two.to_str().unwrap(),
        none.to_str().unwrap(),
    );
    let cases: &[&[&str]] = &[
        &["-c", "-f", with_empty],
        &["-c", "-f", two],
        &["-c", "-f", none],
        &["-v", "-c", "-f", none],
        &["--json", "-f", none],
        &["--files-without-match", "-f", none],
        &["-c", "-e", ""],
        &["-c", "-e", "hello", "-f", none],
        // A literal line terminator is an error outside -U.
        &["-c", "hello\nfoo"],
        &["-c", "hello\\nfoo"],
        &["-c", "-F", "hello\nfoo"],
        &["-U", "-c", "goodbye\\nfoo"],
        &["-U", "-c", "-F", "goodbye\nfoo"],
        // -U -c counts lines when the pattern can never span lines, and
        // matches when it can.
        &["-U", "-c", "o"],
        &["-U", "-c", "l+"],
        &["-U", "-c", "o\\s+"],
        &["-U", "--count-matches", "l+"],
        &["-U", "-c", "-v", "l"],
        &["-c", "\\s+$"],
        &["-c", "^\\s"],
        &["-c", "(?-m)^use"],
        &["-c", "x\\z"],
        // CRLF: anchors see \r\n as a line end, with and without -U.
        &["--crlf", "-c", "o$"],
        &["-U", "--crlf", "hello\\r\\nsecond"],
        &["-U", "--crlf", "end$"],
        &["-U", "--crlf", "-c", "line$"],
        // NUL can only be matched without binary detection.
        &["-c", "a\\x00"],
        &["-a", "-c", "a\\x00"],
        // -s/-i/-S: last wins.
        &["-s", "-i", "HELLO"],
        &["-i", "-s", "HELLO"],
        &["-S", "-s", "hello"],
        &["-s", "-S", "hello"],
        &["-s", "HELLO"],
        // Size limits take K/M/G suffixes; a too-small limit is an error.
        &["--regex-size-limit", "1M", "hello"],
        &["--dfa-size-limit", "1K", "hello"],
        &["--regex-size-limit", "1m", "hello"],
        &["--regex-size-limit", "10", "a{100}"],
        // -m 0: nothing can match, so nothing is searched or printed.
        &["-m", "0", "--files-without-match", "hello"],
        &["-m", "0", "-c", "--include-zero", "hello"],
        &["-m", "0", "--json", "hello"],
        &["-m", "0", "--stats", "hello"],
        &["-m", "0", "-v", "hello"],
        &["-m", "0", "hello", "no_such_operand"],
        // --engine auto keeps the default engine when it can; -P and
        // --engine are a last-wins pair.
        &["--engine", "auto", "-c", "hello"],
        &["-P", "--engine", "default", "-c", "hel(?=lo)"],
        // -E: auto/none keywords, a real label, an unknown label.
        &["-E", "none", "hello"],
        &["-E", "utf-16le", "-c", "hello"],
        &["-E", "bogus", "hello"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    assert_eq!(search_path(pool.path(), &["hello"]), "pool");
    check_cases(pool.path(), "pool", cases);
}

/// Under Unicode case folding (rg's -i and -S), ASCII `k` and `s` also
/// match KELVIN SIGN (U+212A) and LONG S (U+017F). The index narrows a
/// case-insensitive search with the ASCII case variants of each trigram,
/// which never cover those two, so kelvin.txt and longs.txt are found
/// only when trigrams holding a k or an s do not narrow. Other ASCII
/// trigrams still narrow, next to non-ASCII text too (cafe.txt).
#[test]
fn unicode_case_folding_matches_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("kelvin.txt"), "\u{212A}free here\n").unwrap();
    std::fs::write(d.join("longs.txt"), "mi\u{17F}match long s\n").unwrap();
    std::fs::write(d.join("plain.txt"), "plain kfree and mismatch\n").unwrap();
    std::fs::write(d.join("cafe.txt"), "caf\u{e9} au lait\nCAF\u{c9} AU LAIT\n").unwrap();
    std::fs::write(d.join("other.txt"), "nothing to see\n").unwrap();
    let cases: &[&[&str]] = &[
        &["-i", "kfree"],
        &["-S", "kfree"],
        &["-i", "-F", "KFREE"],
        &["-i", "-w", "kfree"],
        &["-i", "-l", "-e", "kfree", "-e", "zz_absent_zz"],
        &["-i", "mismatch"],
        &["-S", "-c", "mismatch"],
        &["-S", "-l", "KFREE"],
        &["-i", "-c", "caf\u{e9}"],
        &["-i", "-F", "-c", "CAF\u{c9}"],
        &["--no-unicode", "-i", "-l", "kfree"],
        &["--no-unicode", "-i", "-l", "(?u)kfree"],
        &["-l", "(?i)kfree"],
    ];
    check_cases(d, "fold", cases);
    if rg_has_pcre2() {
        let pcre2: &[&[&str]] = &[&["-P", "-i", "kfree"], &["-P", "-i", "-l", "mismatch"]];
        check_cases(d, "fold pcre2", pcre2);
    }
}

/// rg reports whether it was built with PCRE2: `rg --pcre2-version` exits
/// 0 only then (`cargo install ripgrep` leaves it out unless
/// `--features pcre2`).
fn rg_has_pcre2() -> bool {
    std::process::Command::new("rg")
        .arg("--pcre2-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The PCRE2 side of the matcher configuration: CRLF-aware anchors, NUL
/// matching, --engine auto falling back to PCRE2 per pattern, and -P
/// winning over an earlier --engine. Compared only against an rg that
/// has PCRE2: without it rg rejects every one of these patterns.
#[test]
fn pcre2_matcher_config_matches_rg() {
    if !have_rg() || !rg_has_pcre2() {
        eprintln!("parity: rg without PCRE2, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["-P", "--crlf", "-c", "o$"],
        &["--null-data", "--crlf", "-P", "-c", "o$"],
        &["-P", "-c", "\\x00"],
        &["--engine", "auto", "-c", "hel(?=lo)"],
        &["--engine", "auto", "-c", "a\\nb"],
        &["--engine", "default", "-P", "-c", "hel(?=lo)"],
        &["-P", "-c", "(?<=hel)lo"],
        &["-P", "-o", "(\\w)\\1"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    check_cases(pool.path(), "pool", cases);
}

/// Binary files the way rg's default reader sees them. `nul20k.txt` has
/// its first NUL past 8 KB but inside the searcher's first 64 KB buffer,
/// so it is binary before any line is searched; `nullate.txt` has a
/// match, then a NUL past 64 KB, then another match, so rg prints the
/// first match and its "stopped searching binary file" warning. UTF-16
/// files with a byte-order mark are transcoded and searched.
fn binary_corpus(pool: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("plain.txt"), "hello plain\nnothing\n").unwrap();
    std::fs::write(d.join("binnul.bin"), b"all\x00binary hello\n").unwrap();
    let mut nul20k = b"hello early\n".to_vec();
    nul20k.extend(std::iter::repeat_n(b'x', 20_000));
    nul20k.extend_from_slice(b"\nnul\x00here\nhello late\n");
    std::fs::write(d.join("nul20k.txt"), nul20k).unwrap();
    let mut nullate = b"hello early\n".to_vec();
    for _ in 0..800 {
        nullate.extend(std::iter::repeat_n(b'y', 99));
        nullate.push(b'\n');
    }
    nullate.extend_from_slice(b"nul\x00here\nhello late\n");
    std::fs::write(d.join("nullate.txt"), nullate).unwrap();
    // Same late NUL but no match anywhere: only --files-without-match
    // and -c --include-zero can show it, and rg shows it in neither.
    let mut quietlate = b"quiet early\n".to_vec();
    quietlate.extend(std::iter::repeat_n(b'z', 70_000));
    quietlate.extend_from_slice(b"\nnul\x00here\n");
    std::fs::write(d.join("quietlate.txt"), quietlate).unwrap();
    let mut u16le = vec![0xFF, 0xFE];
    for b in "hello utf16token\nsecond\n".bytes() {
        u16le.extend_from_slice(&[b, 0]);
    }
    std::fs::write(d.join("utf16le.txt"), u16le).unwrap();
    let mut u16be = vec![0xFE, 0xFF];
    for b in "hello utf16be\n".bytes() {
        u16be.extend_from_slice(&[0, b]);
    }
    std::fs::write(d.join("utf16be.txt"), u16be).unwrap();
    std::fs::write(d.join("bom8.txt"), b"\xEF\xBB\xBFhello bom8\n").unwrap();
    if pool {
        for i in 0..40 {
            let body = if i % 2 == 0 {
                format!("hello pool {i}\n")
            } else {
                format!("quiet pool {i}\nhello late {i}\n")
            };
            std::fs::write(d.join(format!("p{i:02}.txt")), body).unwrap();
        }
    }
    dir
}

/// Default quit detection, the summary modes' binary squash (no record
/// at all, yet --files-without-match counts the file for its exit
/// status), -a, --binary, -E none and BOM transcoding, all byte for byte
/// against rg, on the small path and the worker pool.
#[test]
fn binary_files_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["hello"],
        &["-c", "hello"],
        &["-c", "--include-zero", "hello"],
        &["-c", "--include-zero", "zz_absent_zz"],
        &["--count-matches", "hello"],
        &["-l", "hello"],
        &["--files-without-match", "hello"],
        &["--files-without-match", "zz_absent_zz"],
        &["--files-without-match", "-c", "zz_absent_zz"],
        &["-c", "--files-without-match", "zz_absent_zz"],
        &["-l", "--files-without-match", "hello"],
        &["--files-without-match", "-l", "hello"],
        &["-q", "late"],
        &["late"],
        &["-o", "hello"],
        &["-m", "1", "hello"],
        &["-A", "1", "hello"],
        &["-v", "-c", "hello"],
        &["-U", "hello"],
        &["--no-mmap", "hello"],
        &["-a", "hello"],
        &["-a", "-c", "hello"],
        &["-a", "--files-without-match", "zz_absent_zz"],
        &["--binary", "hello"],
        &["--binary", "-c", "hello"],
        &["--binary", "--files-without-match", "zz_absent_zz"],
        &["utf16token"],
        &["-c", "utf16token"],
        &["-l", "utf16"],
        &["-E", "none", "hello"],
        &["-E", "none", "-c", "hello"],
        &["-E", "auto", "-c", "hello"],
        &["--no-encoding", "-c", "hello"],
    ];
    check_cases(binary_corpus(false).path(), "small", cases);
    let pool = binary_corpus(true);
    assert_eq!(search_path(pool.path(), &["hello"]), "pool");
    check_cases(pool.path(), "pool", cases);
}

/// rg's search modes (-l, --files-without-match, -c, --count-matches,
/// --json) are last-wins: the later flag decides the output.
#[test]
fn search_modes_are_last_wins_like_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["-l", "-c", "hello"],
        &["-c", "-l", "hello"],
        &["--files-without-match", "-c", "hello"],
        &["-c", "--files-without-match", "hello"],
        &["-l", "--files-without-match", "hello"],
        &["--files-without-match", "-l", "hello"],
        &["--count-matches", "-c", "o"],
        &["-c", "--count-matches", "o"],
        &["--json", "-l", "hello"],
        &["--json", "-c", "hello"],
        &["--json", "--count-matches", "hello"],
        &["--json", "--files-without-match", "hello"],
        &["-l", "--count-matches", "hello"],
        &["--count-matches", "-l", "hello"],
        &["-q", "--debug", "hello"],
        // -q --files-without-match: nothing printed; the answer is whether
        // some searched file has no match.
        &["-q", "--files-without-match", "hello"],
        &["-q", "--files-without-match", "zz_absent_zz"],
        &[
            "--files-without-match",
            "-q",
            "hello",
            "README.md",
            "src/main.rs",
        ],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    assert_eq!(search_path(pool.path(), &["hello"]), "pool");
    check_cases(pool.path(), "pool", cases);
}

/// Every remaining flag of the merged stack that reaches the search hot
/// path or the file set, against rg on both corpora.
#[test]
fn remaining_stack_flags_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let extra = tempfile::tempdir().unwrap();
    let ignore_file = extra.path().join("extra.ignore");
    std::fs::write(&ignore_file, "src/\n*.md\n").unwrap();
    let ignore_file = ignore_file.to_str().unwrap();
    let keep_ignore = extra.path().join("keep.ignore");
    std::fs::write(&keep_ignore, ".keepme\nplain.txt\n").unwrap();
    let keep_ignore = keep_ignore.to_str().unwrap();
    let ignore_md = extra.path().join("md.ignore");
    std::fs::write(&ignore_md, "*.md\n").unwrap();
    let ignore_md = ignore_md.to_str().unwrap();
    let keep_md = extra.path().join("keep_md.ignore");
    std::fs::write(&keep_md, "!README.md\n").unwrap();
    let keep_md = keep_md.to_str().unwrap();
    let cases: &[&[&str]] = &[
        &["--line-buffered", "hello"],
        &["-j", "2", "hello"],
        &["-j", "1", "-c", "foo"],
        &["--no-config", "hello"],
        &["-uu", "-l", "hello"],
        &["-uuu", "-l", "binary"],
        &["-uuu", "-c", "binary"],
        // -uuu is --no-ignore --hidden --binary: binary files report a
        // notice in content mode, and a later -a still wins.
        &["-uuu", "binary"],
        &["-uuu", "match"],
        &["-uuu", "-a", "binary"],
        &["-a", "-uuu", "binary"],
        &["-M", "10", "--max-columns-preview", "a+"],
        &["-M", "10", "--max-columns-preview", "-o", "a+"],
        &["--stop-on-nonmatch", "foo"],
        &["--stop-on-nonmatch", "-c", "a"],
        &["-U", "--stop-on-nonmatch", "foo"],
        &["--stop-on-nonmatch", "-U", "foo"],
        &["--no-messages", "hello"],
        &["--no-messages", "hello", "no_such_path"],
        &["hello", "no_such_path"],
        // The single-file heuristic counts the operands as typed: one
        // file plus a missing one still prints `path:` prefixes.
        &["-n", "hello", "README.md", "no_such_path"],
        &["-c", "hello", "README.md", "no_such_path"],
        &["--vimgrep", "hello", "no_such_path", "README.md"],
        // -q answers only "did anything match": a match wins over an
        // error (exit 0), a miss after an error is still exit 2.
        &["-q", "hello", "README.md", "no_such_path"],
        &["-q", "zz_no_match_zz", "README.md", "no_such_path"],
        &["-q", "-c", "hello", "no_such_path", "src"],
        &["-c", "hello", "no_such_path", "src"],
        &["--files-without-match", "hello", "no_such_path"],
        &["-c", "--include-zero", "hello", "no_such_path"],
        &["-r", "X", "hello"],
        &["-r", "$0!", "hello"],
        &["-o", "-r", "[$1]", "h(el)lo"],
        &["-r", "", "hello"],
        &["-r", "${1}x", "(hel)lo"],
        &["-c", "-r", "X", "hello"],
        &["--mmap", "hello"],
        &["--no-mmap", "-c", "hello"],
        &["--mmap", "--no-mmap", "hello"],
        &["-p", "hello"],
        &["-p", "-c", "hello"],
        // -p is an alias for --color always --heading --line-number, so
        // a later flag overrides each part and an earlier one does not.
        &["-p", "--color", "never", "hello"],
        &["--color", "never", "-p", "hello"],
        &["-p", "--no-heading", "hello"],
        &["--no-heading", "-p", "hello"],
        &["-p", "-N", "hello"],
        &["-N", "-p", "hello"],
        &["--iglob", "*.MD", "-l", "hello"],
        &["-g", "*.MD", "-l", "hello"],
        &["--glob-case-insensitive", "-g", "*.MD", "-l", "hello"],
        &["-g", "*.md", "--iglob", "!*.MD", "-l", "hello"],
        &["--iglob", "!*.MD", "-g", "*.md", "-l", "hello"],
        &["-t", "rust", "hello"],
        &["-T", "rust", "-c", "hello"],
        &["--type-add", "web:*.{html,md}", "-t", "web", "hello"],
        &["--type-clear", "rust", "-t", "rust", "hello"],
        // Type changes apply in command-line order, as in rg.
        &["-T", "md", "-t", "md", "hello"],
        &["-t", "md", "-T", "md", "-l", "hello"],
        &[
            "--type-clear",
            "md",
            "--type-add",
            "md:*.txt",
            "-t",
            "md",
            "-l",
            "hello",
        ],
        &[
            "--type-add",
            "md:*.txt",
            "--type-clear",
            "md",
            "-t",
            "md",
            "hello",
        ],
        &["-t", "md", "--type-add", "md:*.rs", "-l", "hello"],
        &["--ignore-file", ignore_file, "-l", "hello"],
        // A missing --ignore-file is a warning: the search goes on.
        &["--ignore-file", "no_such.ignore", "-l", "hello"],
        // The tree's own whitelist (`!.keepme` in dotkit/.gitignore) beats
        // an --ignore-file rule, and among --ignore-file rules the last
        // file with an opinion decides.
        &["--ignore-file", keep_ignore, "-l", "keeptoken"],
        &[
            "--ignore-file",
            ignore_md,
            "--ignore-file",
            keep_md,
            "-l",
            "hello",
        ],
        &[
            "--ignore-file",
            keep_md,
            "--ignore-file",
            ignore_md,
            "-l",
            "hello",
        ],
        &["--require-git", "-l", "hello"],
        &["--no-ignore-vcs", "-l", "hello"],
        &["--no-ignore-dot", "-l", "hello"],
        &["--no-ignore-parent", "-l", "hello"],
        &["--no-ignore-exclude", "-l", "hello"],
        &["--no-ignore-global", "-l", "hello"],
        &["--one-file-system", "-l", "hello"],
        &["-E", "latin1", "-c", "caf"],
        &["hello", "src", "README.md"],
        &["hello", "src/deep", "src"],
        &["hello", "README.md", "src"],
        &["-l", "hello", "src", "README.md", "crlf.txt"],
        &["--sortr", "path", "hello", "src", "README.md"],
        &["-d", "0", "hello", "src", "README.md"],
        &["-d", "1", "-l", "hello", "src", "src/deep"],
        &["-c", "hello", "src", "crlf.txt"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    assert_eq!(search_path(pool.path(), &["hello"]), "pool");
    check_cases(pool.path(), "pool", cases);
    // A file operand inside a directory operand. rg prints a file operand
    // exactly as typed and glep prints the cwd-relative path, which agree
    // when the typed path uses the native separator (on Windows a typed
    // `src/main.rs` prints as `src\main.rs`, see README).
    let main_rs = format!("src{}main.rs", std::path::MAIN_SEPARATOR);
    let native: &[&[&str]] = &[
        &["hello", &main_rs, "src"],
        &["-c", "hello", "src", &main_rs],
    ];
    check_cases(corpus().path(), "small", native);
    check_cases(pool.path(), "pool", native);
}

/// Like rg, a repeated flag is last-wins instead of an error (wrappers and
/// aliases that always add -n, --color or --sort), and rg's negated and
/// alias spellings are accepted, each acting where it stands: `-uu
/// --no-hidden`, `--no-ignore --ignore-vcs`, `-P --no-pcre2`, ...
#[test]
fn repeated_and_negated_flags_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let cases: &[&[&str]] = &[
        &["-n", "-n", "hello"],
        &["-i", "-i", "HELLO"],
        &["-w", "-w", "foo"],
        &["-l", "-l", "hello"],
        &["-c", "-c", "hello"],
        &["-A1", "-A2", "hello"],
        &["-m1", "-m2", "foo"],
        &["-M", "5", "-M", "10", "a+"],
        &["--threads", "2", "-j1", "hello"],
        &["--sort", "path", "--sortr", "path", "-l", "hello"],
        &["--sortr", "path", "--sort", "path", "-l", "hello"],
        &["--color", "never", "--color", "never", "hello"],
        &["--json", "--json", "-l", "hello"],
        &["--hidden", "--hidden", "-l", "hiddentoken"],
        &["-E", "utf-8", "-E", "utf-8", "hello"],
        &["-r", "a", "-r", "b", "hello"],
        &[
            "--context-separator",
            "a",
            "--context-separator",
            "b",
            "-C1",
            "hello",
        ],
        &["--null-data", "--null-data", "-c", "hello"],
        &["-v", "-v", "-c", "hello"],
        &["-q", "-q", "hello"],
        &["-g", "*.rs", "-g", "*.md", "-l", "hello"],
        &["-e", "hello", "-e", "answer", "-l"],
        &["--unrestricted", "-l", "hello"],
        &["--unrestricted", "--unrestricted", "-l", "hiddentoken"],
        &["-uu", "--no-hidden", "-l", "hiddentoken"],
        &["--no-hidden", "-uu", "-l", "hiddentoken"],
        &["--no-ignore", "--ignore", "-l", "hello"],
        &["--ignore", "--no-ignore", "-l", "hello"],
        &["--no-ignore", "--ignore-vcs", "-l", "hello"],
        &["--no-ignore", "--ignore-dot", "-l", "hello"],
        &["--no-ignore-vcs", "--ignore", "-l", "hello"],
        &["-u", "--ignore", "-l", "hello"],
        &["-a", "--no-text", "-c", "binary"],
        &["--binary", "--no-binary", "binary"],
        &["-uuu", "--no-binary", "-l", "binary"],
        &["-P", "--no-pcre2", "-c", "hel+o"],
        &["--no-pcre2", "-c", "hello"],
        &["--no-unicode", "--unicode", "-c", "\\w+"],
        &["--sort-files", "-l", "hello"],
        &["--no-sort-files", "--sort-files", "-l", "hello"],
        &["--line-buffered", "--no-line-buffered", "hello"],
        &["--no-block-buffered", "hello"],
        &["--no-require-git", "-l", "hello"],
        &["--require-git", "--no-require-git", "-l", "hello"],
        &["--no-require-git", "--require-git", "-l", "hello"],
        &["-U", "--no-multiline", "-c", "hello"],
        &["--multiline-dotall", "--no-multiline-dotall", "-c", "hello"],
        &["--no-follow", "-l", "hello"],
        &["--crlf", "--no-crlf", "-c", "hello$"],
        &["-F", "--no-fixed-strings", "-c", "hel.o"],
        &["-v", "--no-invert-match", "-c", "hello"],
        &["--json", "--no-json", "-c", "hello"],
        &["--stats", "--no-stats", "-c", "hello"],
        &["--trim", "--no-trim", "padded"],
        &["-z", "--no-search-zip", "-l", "hello"],
        &["--column", "--no-column", "hello"],
        &["-b", "--no-byte-offset", "hello"],
        &["-c", "--include-zero", "--no-include-zero", "hello"],
        &[
            "-M",
            "5",
            "--max-columns-preview",
            "--no-max-columns-preview",
            "a+",
        ],
        &["--one-file-system", "--no-one-file-system", "-l", "hello"],
        &[
            "--glob-case-insensitive",
            "--no-glob-case-insensitive",
            "-g",
            "*.MD",
            "-l",
            "hello",
        ],
        &["--no-ignore-file-case-insensitive", "-l", "hello"],
        &["--no-pre", "-c", "hello"],
        &["--messages", "--ignore-messages", "-l", "hello"],
        &["--no-messages", "--messages", "hello", "no_such_path"],
        &["--no-ignore-files", "-l", "hello"],
        &["--ignore-files", "-l", "hello"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    check_cases(pool.path(), "pool", cases);
}

#[cfg(unix)]
fn have_cmd(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// -L, -z and --pre need symlinks, a gzip binary (rg decompresses -z
/// input through it) and `cat`: Unix only.
#[cfg(unix)]
#[test]
fn unix_only_flags_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = corpus();
    let d = dir.path();
    std::fs::create_dir_all(d.join("real")).unwrap();
    std::fs::write(d.join("real/inner.txt"), "linktok hello inner\n").unwrap();
    std::os::unix::fs::symlink("real", d.join("linkdir")).unwrap();
    std::os::unix::fs::symlink("README.md", d.join("linkfile.md")).unwrap();
    let mut cases: Vec<&[&str]> = vec![
        &["linktok"],
        &["-L", "linktok"],
        &["-L", "-l", "hello"],
        &["-L", "-c", "hello"],
        &["-L", "--files"],
        &["--pre", "cat", "-c", "hello"],
        &["--pre", "cat", "--pre-glob", "*.md", "hello"],
        &["--pre-glob", "*.rs", "--pre", "cat", "-l", "hello"],
        // The command gets the cwd-relative path as its argument, like
        // rg: `echo` prints it back as the searched text.
        &["--pre", "echo", "-e", "md", "-e", "rs"],
    ];
    if have_cmd("gzip") {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"zipneedle hello packed\nsecond\n").unwrap();
        std::fs::write(d.join("pack.txt.gz"), enc.finish().unwrap()).unwrap();
        cases.extend_from_slice(&[
            &["-z", "zipneedle"],
            &["-z", "-l", "hello"],
            &["-z", "-c", "hello"],
            &["zipneedle"],
        ]);
    }
    check_cases(d, "unix", &cases);
}

/// Per-file errors are reported and make the exit code 2, like rg: an
/// unreadable file, a dangling link under -L, a failing --pre command, a
/// corrupt -z stream. -q still exits 0 when something matched.
#[cfg(unix)]
#[test]
fn per_file_errors_exit_two_like_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.txt"), "hello a\n").unwrap();
    std::fs::write(d.join("p.txt"), "hello p\nnothing\n").unwrap();
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/s.txt"), "hello s\n").unwrap();
    std::fs::write(d.join("u.txt"), "hello u\n").unwrap();
    std::fs::set_permissions(d.join("u.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
    std::os::unix::fs::symlink("nowhere", d.join("dang")).unwrap();
    // As root nothing is unreadable: those cases would prove nothing, but
    // the dangling link, --pre and -z ones still hold.
    let unreadable = std::fs::read(d.join("u.txt")).is_err();
    let mut cases: Vec<&[&str]> = Vec::new();
    if unreadable {
        cases.extend_from_slice(&[
            &["hello"],
            &["-c", "hello"],
            &["-l", "hello"],
            &["-q", "hello"],
            &["-q", "zz_absent_zz"],
            &["--no-messages", "hello"],
            &["--files"],
        ]);
    }
    cases.extend_from_slice(&[
        &["-L", "-l", "hello"],
        &["-L", "--files"],
        &["--pre", "false", "hello", "p.txt"],
        &["--pre", "false", "-l", "hello"],
        &["--pre", "cat", "-c", "hello"],
        &["--pre", "cat", "-q", "hello"],
    ]);
    if have_cmd("gzip") {
        std::fs::write(d.join("bad.gz"), b"not gzip at all\n").unwrap();
        cases.extend_from_slice(&[&["-z", "hello", "bad.gz"], &["-z", "-c", "hello"]]);
    }
    check_cases(d, "errors", &cases);
    std::fs::set_permissions(d.join("u.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Running from a subdirectory of the indexed tree: rg's cwd-relative
/// paths, `..` operands included.
#[test]
fn subdirectory_runs_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = corpus();
    // Index at the root first, so the subdirectory run discovers it.
    let (_, code) = glep_out(dir.path(), &["-c", "hello"]);
    assert_eq!(code, 0);
    let sub = dir.path().join("src");
    let cases: &[&[&str]] = &[
        &["hello"],
        &["-l", "hello"],
        &["-c", "--include-zero", "hello"],
        &["fn"],
        &["hello", "../README.md"],
        &["-l", "hello", "deep", "main.rs"],
        &["--files"],
        &["--files-without-match", "hello"],
        // -g globs match the path relative to the cwd, as rg sees it.
        &["-l", "-g", "deep/**", "answer|hello"],
        &["-l", "-g", "/deep/*.rs", "answer|hello"],
        &["-l", "-g", "!deep/**", "answer|hello"],
        &["-l", "-g", "!/main.rs", "fn|answer"],
        &["-l", "-g", "!deep", "answer|hello"],
        &["--files", "-g", "deep/*"],
        // Under a `..` operand rg matches globs against `../...` paths.
        &["-c", "hello", "..", "-g", "!../src"],
        &["-c", "hello", "..", "-g", "!src"],
        &["-c", "hello", "..", "-g", "../*.md"],
        // The cwd scope is still rg's implicit path: when filters leave
        // nothing to search, rg warns and exits 2.
        &["-g", "*.zz", "hello"],
        &["--max-depth", "0", "hello"],
        &["-t", "json", "hello"],
        &["-l", "-g", "src/**", "hello"],
    ];
    check_cases(&sub, "subdir", cases);
    // Operands outside the index root, relative or absolute, are walked
    // live as rg walks them.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("o.txt"), "hello outside\n").unwrap();
    let name = outside.path().file_name().unwrap().to_str().unwrap();
    let sep = std::path::MAIN_SEPARATOR;
    let up = format!("..{sep}..{sep}{name}");
    let abs = outside.path().to_str().unwrap();
    let away: &[&[&str]] = &[
        &["-l", "hello", &up],
        &["-c", "hello", &up, "main.rs"],
        &["-l", "hello", abs],
        // --path-separator rewrites every separator byte, the root's
        // included, as rg's printers do.
        &["--path-separator", "%", "-l", "hello", abs],
        &["--path-separator", "/", "-c", "hello", abs],
        &["--path-separator", "%", "--count-matches", "hello", abs],
        &[
            "--path-separator",
            "%",
            "--files-without-match",
            "zz_absent_zz",
            abs,
        ],
        &["--path-separator", "%", "-c", "hello", &up],
        &["--path-separator", "%", "hello", abs],
    ];
    check_cases(&sub, "subdir outside", away);
    // The --files listing too (glep's pattern slot is a glob, rg's
    // positionals are paths).
    for sep in ["%", "/"] {
        let glep = glep_out(&sub, &["--path-separator", sep, "--files", "**", abs]);
        let rg = rg_out(&sub, &["--path-separator", sep, "--files", abs]);
        assert_eq!(glep, rg, "--path-separator {sep} --files {abs}");
    }
}

/// Directory operands the index holds no file under are walked live with
/// rg's rules, because rg walks them like any other operand and never
/// filters an operand itself: an ignored directory (as an operand or as
/// the cwd), a symlink to a directory, a hidden cwd, and a spelling that
/// differs from the one on disk on a case-insensitive filesystem. The
/// live escape hatches walk from each operand too, so --no-ignore-parent
/// drops the ignore files above an operand.
#[test]
fn operands_the_index_cannot_serve_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // A repository root: .ignore and .gitignore rules apply to `sub` from
    // above, unless --no-ignore-parent.
    std::fs::create_dir_all(d.join(".git")).unwrap();
    std::fs::write(d.join(".gitignore"), "build/\n*.log\nt.txt\n").unwrap();
    std::fs::write(d.join(".ignore"), "s.txt\n").unwrap();
    for (p, body) in [
        ("README.md", "hello readme\n"),
        ("build/out.txt", "hello build\n"),
        ("build/deep/more.txt", "hello build deep\n"),
        ("build/x.log", "hello log\n"),
        (".hdir/inner.rs", "hello hdir\n"),
        ("src/main.rs", "hello main\n"),
        ("sub/in/s.txt", "hello s\n"),
        ("sub/in/t.txt", "hello t\n"),
        ("sub/in/u.txt", "hello u\n"),
    ] {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let (_, code) = glep_out(d, &["-c", "hello"]);
    assert_eq!(code, 0);
    let mut cases: Vec<&[&str]> = vec![
        &["-n", "hello", "build"],
        &["-c", "hello", "build", "src"],
        &["-l", "hello", "src", "build"],
        &["-l", "-g", "!deep", "hello", "build"],
        &["--max-depth", "1", "-l", "hello", "build"],
        &["-l", "hello", ".hdir"],
        &["-l", "hello", "sub"],
        &["--no-ignore-parent", "hello", "sub"],
        &["--no-ignore-parent", "-l", "hello"],
        &["--no-ignore", "-l", "hello", ".hdir"],
        &["--no-ignore-vcs", "-l", "hello", "sub"],
        &["--files-without-match", "zz_absent", "build"],
    ];
    #[cfg(unix)]
    {
        // Forward-slash operands print as typed in rg, so these stay off
        // Windows, where glep prints native separators.
        std::os::unix::fs::symlink("../sub/in", d.join("src/link_dir")).unwrap();
        cases.extend_from_slice(&[
            &["-n", "hello", "src/link_dir"],
            &["-c", "hello", "src/link_dir", "src"],
            &["--no-ignore-parent", "-l", "hello", "sub/in"],
        ]);
    }
    // A case-insensitive filesystem (macOS and Windows defaults) finds
    // `SRC`; rg prints the operand as typed.
    if d.join("SRC").exists() {
        cases.extend_from_slice(&[&["-n", "hello", "SRC"], &["-l", "hello", "Src", "build"]]);
    }
    check_cases(d, "unserved", &cases);
    // The cwd itself inside an ignored or a hidden directory.
    let in_build: &[&[&str]] = &[&["hello"], &["-l", "hello"], &["-c", "hello", "deep"]];
    check_cases(&d.join("build"), "cwd build", in_build);
    let in_hdir: &[&[&str]] = &[&["hello"], &["-c", "hello"]];
    check_cases(&d.join(".hdir"), "cwd .hdir", in_hdir);
}

/// Warm the listing cache (macOS: `.glep/dirtree.bin`) so a later
/// `--files` could be answered from it.
fn warm_listing_cache(dir: &Path) {
    for _ in 0..2 {
        let (_, code) = glep_out(dir, &["--files"]);
        assert_eq!(code, 0);
    }
    #[cfg(target_os = "macos")]
    assert!(dir.join(".glep/dirtree.bin").exists(), "cache must be warm");
}

/// Timestamp sorts need live metadata and rg's walk order, so they must
/// bypass a warm listing cache.
#[test]
fn listing_cache_bypass_timestamp_sorts() {
    if !have_rg() {
        return;
    }
    let dir = corpus();
    warm_listing_cache(dir.path());
    let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    stamp_all(dir.path(), t0);
    for (k, name) in ["words.txt", "src/main.rs", "README.md"].iter().enumerate() {
        let f = std::fs::File::options()
            .write(true)
            .open(dir.path().join(name))
            .unwrap();
        f.set_modified(t0 + std::time::Duration::from_secs(10 * (k as u64 + 1)))
            .unwrap();
    }
    let cases: &[&[&str]] = &[
        &["--sort", "modified", "--files"],
        &["--sortr", "modified", "--files"],
        &["--sortr", "path", "--files"],
    ];
    check_cases(dir.path(), "cache", cases);
}

/// --no-ignore and the -u ladder list ignored (and hidden) files: a warm
/// cache, which holds only the indexed set, must not answer them.
#[test]
fn listing_cache_bypass_no_ignore_family() {
    if !have_rg() {
        return;
    }
    let dir = corpus();
    warm_listing_cache(dir.path());
    let cases: &[&[&str]] = &[
        &["--no-ignore", "--files"],
        &["-u", "--files"],
        &["-u", "-l", "hello"],
    ];
    check_cases(dir.path(), "cache", cases);
    // -uu/-uuu add hidden files; rg would also list the index's own
    // `.glep/` files there, which glep never lists (like `.git`).
    for flag in ["-uu", "-uuu"] {
        let (g, code) = glep_out(dir.path(), &[flag, "--files"]);
        assert_eq!(code, 0);
        assert!(g.contains(".hidden.txt") && g.contains("skipme.log"), "{g}");
        assert!(!g.contains(".glep"), "{g}");
    }
}

/// The per-source toggles each re-admit files one ignore source hides.
#[test]
fn listing_cache_bypass_ignore_toggles() {
    if !have_rg() {
        return;
    }
    let dir = corpus();
    warm_listing_cache(dir.path());
    let cases: &[&[&str]] = &[
        &["--no-ignore-vcs", "--files"],
        &["--no-ignore-dot", "--files"],
        &["--no-ignore-exclude", "--files"],
        &["--no-ignore-global", "--files"],
        &["--no-ignore-parent", "--files"],
    ];
    check_cases(dir.path(), "cache", cases);
}

/// -L lists files behind symlinked directories, which never enter the
/// index or the cache.
#[cfg(unix)]
#[test]
fn listing_cache_bypass_follow() {
    if !have_rg() {
        return;
    }
    let dir = corpus();
    std::fs::create_dir_all(dir.path().join("real")).unwrap();
    std::fs::write(dir.path().join("real/inner.txt"), "x\n").unwrap();
    std::os::unix::fs::symlink("real", dir.path().join("linkdir")).unwrap();
    warm_listing_cache(dir.path());
    check_cases(dir.path(), "cache", &[&["-L", "--files"], &["--files"]]);
}

/// --one-file-system and --require-git (outside a repo: git rules go
/// inert) take live scans of their own.
#[test]
fn listing_cache_bypass_one_fs_and_require_git() {
    if !have_rg() {
        return;
    }
    let dir = corpus();
    warm_listing_cache(dir.path());
    let cases: &[&[&str]] = &[
        &["--one-file-system", "--files"],
        &["--require-git", "--files"],
    ];
    check_cases(dir.path(), "cache", cases);
}

/// --no-index lists through a live walk: the same set as the cached
/// listing, never served from the cache or the index.
#[test]
fn listing_cache_bypass_no_index() {
    let dir = corpus();
    warm_listing_cache(dir.path());
    let (cached, _) = glep_out(dir.path(), &["--files"]);
    let (live, code) = glep_out(dir.path(), &["--no-index", "--files"]);
    assert_eq!(code, 0);
    assert_eq!(live, cached);
    std::fs::write(dir.path().join("brand_new.txt"), "x\n").unwrap();
    let (live, _) = glep_out(dir.path(), &["--no-index", "--files"]);
    assert!(live.contains("brand_new.txt"), "{live}");
}

/// An `.ignore` above the tree applies as a parent rule, as in rg, ahead
/// of every .gitignore (its `!rule` beats an in-tree `*.log`). The bulk
/// sweep and the listing cache honor it themselves (the cache's
/// fingerprint covers it), so a common setup like a `~/.ignore` keeps
/// both fast paths: once rebuilt, the cache serves the listing without
/// being rewritten on every run.
#[test]
fn listing_cache_with_ancestor_dot_ignore() {
    if !have_rg() {
        return;
    }
    let outer = tempfile::tempdir().unwrap();
    let inner = outer.path().join("tree");
    std::fs::create_dir_all(inner.join("sub")).unwrap();
    std::fs::write(inner.join("keep.txt"), "hello keep\n").unwrap();
    std::fs::write(inner.join("sub/drop.txt"), "hello drop\n").unwrap();
    std::fs::write(inner.join("sub/keep2.txt"), "hello keep2\n").unwrap();
    std::fs::write(inner.join("sub/app.log"), "hello app log\n").unwrap();
    std::fs::write(inner.join("sub/keep.log"), "hello keep log\n").unwrap();
    std::fs::write(inner.join(".gitignore"), "*.log\n").unwrap();
    warm_listing_cache(&inner);
    std::fs::write(outer.path().join(".ignore"), "drop.txt\n!keep.log\n").unwrap();
    let cases: &[&[&str]] = &[
        &["--files"],
        &["-l", "hello"],
        &["-c", "hello", "sub"],
        &["--no-ignore-parent", "--files"],
        &["--no-ignore-parent", "-l", "hello"],
    ];
    check_cases(&inner, "cache", cases);
    // Editing the ancestor file changes the fingerprint: the cache is
    // rebuilt, never served stale.
    std::fs::write(outer.path().join(".ignore"), "keep2.txt\n").unwrap();
    check_cases(&inner, "cache edited", cases);
    #[cfg(target_os = "macos")]
    {
        let cache = inner.join(".glep/dirtree.bin");
        let (_, code) = glep_out(&inner, &["--files"]);
        assert_eq!(code, 0);
        let before = std::fs::metadata(&cache).unwrap().modified().unwrap();
        let out = Command::cargo_bin("glep")
            .unwrap()
            .current_dir(&inner)
            .env("GLEP_TIMING", "1")
            .arg("--files")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("listing_cache"), "cache not used: {err}");
        assert_eq!(
            std::fs::metadata(&cache).unwrap().modified().unwrap(),
            before,
            "an unchanged tree must not rewrite the listing cache"
        );
    }
}

/// Scoped listings (path operands, a subdirectory cwd) come from the
/// index's scoped sweep even when the listing cache is warm, and list
/// what rg lists, filters and hidden operands included. The
/// --files-without-match universe is scoped the same way. glep's --files
/// takes a glob where rg takes paths, hence the paired argument lists.
#[test]
fn scoped_listings_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let dir = corpus();
    let d = dir.path();
    warm_listing_cache(d);
    let deep = format!("src{}deep", std::path::MAIN_SEPARATOR);
    let pairs: &[(&[&str], &[&str])] = &[
        (&["--files", "**", "src"], &["--files", "src"]),
        (
            &["--files", "**", "src", "dotkit"],
            &["--files", "src", "dotkit"],
        ),
        (&["--files", "**", &deep, "src"], &["--files", &deep, "src"]),
        (
            &["--hidden", "--files", "**", "dotkit", ".github"],
            &["--hidden", "--files", "dotkit", ".github"],
        ),
        (
            &["-g", "*.rs", "--files", "**", "src"],
            &["-g", "*.rs", "--files", "src"],
        ),
    ];
    for (glep_args, rg_args) in pairs {
        assert_eq!(glep_out(d, glep_args), rg_out(d, rg_args), "{glep_args:?}");
    }
    let cases: &[&[&str]] = &[
        &["--files-without-match", "hello", "src", "dotkit"],
        &[
            "--files-without-match",
            "-g",
            "*.txt",
            "hello",
            "dotkit",
            "src",
        ],
    ];
    check_cases(d, "scoped", cases);
    let sub = d.join("src");
    assert_eq!(glep_out(&sub, &["--files"]), rg_out(&sub, &["--files"]));
    assert_eq!(
        glep_out(&sub, &["--files", "**", "deep"]),
        rg_out(&sub, &["--files", "deep"])
    );
    let in_sub: &[&[&str]] = &[
        &["--files-without-match", "hello"],
        &["--files-without-match", "zz_absent_zz", "deep"],
    ];
    check_cases(&sub, "scoped subdir", in_sub);
}

/// Filters applied after a listing (globs, types, depth, hidden, extra
/// ignore files) give the same answer whether or not the cache served it.
#[test]
fn listing_cache_filters_match_rg() {
    if !have_rg() {
        return;
    }
    let dir = corpus();
    warm_listing_cache(dir.path());
    let extra = tempfile::tempdir().unwrap();
    let ignore_file = extra.path().join("x.ignore");
    std::fs::write(&ignore_file, "*.md\n").unwrap();
    let ignore_file = ignore_file.to_str().unwrap();
    let cases: &[&[&str]] = &[
        &["-g", "*.rs", "--files"],
        &["-g", "!*.txt", "--files"],
        &["--iglob", "*.MD", "--files"],
        &["-t", "rust", "--files"],
        &["-T", "rust", "--files"],
        &["--max-depth", "1", "--files"],
        &["--hidden", "--files"],
        &["--ignore-file", ignore_file, "--files"],
        &["--color", "always", "--files"],
        &["-0", "--files"],
    ];
    check_cases(dir.path(), "cache", cases);
}

/// rg searches an explicit FILE operand unconditionally: ignore rules,
/// the hidden filter, -g/-t/--ignore-file and --max-depth never drop it,
/// and binary data in it is reported ("binary file matches") instead of
/// hiding the file.
#[test]
fn explicit_file_operands_match_rg() {
    if !have_rg() {
        eprintln!("parity: rg not installed, skipping");
        return;
    }
    let extra = tempfile::tempdir().unwrap();
    let ignore_md = extra.path().join("md.ignore");
    std::fs::write(&ignore_md, "*.md\n").unwrap();
    let ignore_md = ignore_md.to_str().unwrap();
    let cases: &[&[&str]] = &[
        &["hello", "skipme.log"],
        &["hello", "skipme2.txt"],
        &["hiddentoken", ".hidden.txt"],
        &["nopetoken", "dotkit/.nope"],
        &["match", "binpre.bin"],
        &["-c", "match", "binpre.bin"],
        &["-l", "binary", "binnul.bin", "binpre.bin"],
        &["-a", "binary", "binnul.bin"],
        &["--binary", "binary", "binnul.bin"],
        &[
            "--files-without-match",
            "zz_absent_zz",
            "skipme.log",
            "binpre.bin",
            "README.md",
        ],
        &["-c", "--include-zero", "zz_absent_zz", "skipme.log"],
        &["-g", "!*.log", "hello", "skipme.log"],
        &["-t", "rust", "hello", "README.md"],
        &["--ignore-file", ignore_md, "hello", "README.md"],
        &["-d", "0", "hello", "src/main.rs"],
        &["hello", "skipme.log", "src"],
        &["hello", "src", "skipme.log"],
        &["-c", "hello", "skipme.log", ".hidden.txt", "README.md"],
        &["--no-ignore", "hello", "skipme.log", "src"],
        &["--hidden", "-l", "hello", ".hidden.txt", "skipme.log"],
    ];
    check_cases(corpus().path(), "small", cases);
    let pool = pool_corpus();
    check_cases(pool.path(), "pool", cases);
}
