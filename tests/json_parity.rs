//! Structural parity between glep's and ripgrep's `--json` output.
//!
//! Unlike tests/parity.rs (byte-for-byte parity on TEXT-mode output, which
//! this file does not touch or weaken), JSON-mode output cannot be
//! byte-compared: `elapsed`/`elapsed_total` are wall-clock timings that
//! differ on every run, and glep's index narrows candidate files before any
//! file is opened, so its `searches`/`searches_with_match`/`bytes_searched`
//! counters legitimately differ from rg's own brute-force walk for the same
//! query (see src/search.rs's `merge_stats` doc comment and the README).
//! So this test does a structural comparison instead: the sequence of
//! begin/match/end events, and the subset of summary stats
//! (matches/matched_lines/bytes_printed) that are expected to match rg
//! exactly because they come from the same grep-searcher/grep-printer
//! machinery rg itself uses.

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
    std::fs::write(dir.path().join("notes.txt"), "hello there\ngeneral kenobi\n").unwrap();
    dir
}

fn glep_json(dir: &Path, pattern: &str) -> (String, i32) {
    let out = Command::cargo_bin("glep")
        .unwrap()
        .current_dir(dir)
        .args(["--json", pattern])
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

fn rg_json(dir: &Path, pattern: &str) -> (String, i32) {
    // --sort path: JSON event order (not just text output) needs to be
    // deterministic for this comparison; rg's default walk order isn't
    // guaranteed otherwise. --no-require-git matches tests/parity.rs's rg
    // invocation (the corpus here is a plain tempdir, not a git repo).
    let out = std::process::Command::new("rg")
        .current_dir(dir)
        .args(["--json", "--sort", "path", "--no-require-git"])
        .arg(pattern)
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        out.status.code().unwrap_or(-1),
    )
}

/// Extract `(type, data.path.text, data.line_number)` from one JSON-lines
/// event. Works uniformly for begin/match/end: begin and end carry no
/// `line_number` (-> None), match always does. This pattern set has no
/// `-C`/`-A`/`-B` flags, so no `context` events are produced.
fn event_key(line: &str) -> (String, Option<String>, Option<i64>) {
    let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
    let kind = v["type"].as_str().expect("type field").to_string();
    let path = v["data"]["path"]["text"].as_str().map(str::to_string);
    let line_number = v["data"]["line_number"].as_i64();
    (kind, path, line_number)
}

fn lines(output: &str) -> Vec<&str> {
    output.lines().filter(|l| !l.trim().is_empty()).collect()
}

#[test]
fn json_summary_matches_rg_structurally() {
    if !have_rg() {
        eprintln!("json_parity: rg not installed, skipping");
        return;
    }
    let dir = corpus();
    // Three representative patterns, same style of corpus as
    // tests/parity.rs: a multi-file match, a single-file match, and a
    // pattern with no matches anywhere. rg still emits a summary event for
    // the no-match case (verified against real rg 15.1.0 before writing
    // this test; see ground-truth capture in the task report).
    let patterns = ["hello", "answer", "zz_totally_absent_zz"];

    for pattern in patterns {
        let (g, gc) = glep_json(dir.path(), pattern);
        let (r, rc) = rg_json(dir.path(), pattern);
        assert_eq!(gc, rc, "exit code diverged for pattern {pattern:?}");

        let g_lines = lines(&g);
        let r_lines = lines(&r);
        assert!(
            !g_lines.is_empty(),
            "glep produced no --json output at all for {pattern:?}"
        );
        assert!(
            !r_lines.is_empty(),
            "rg produced no --json output at all for {pattern:?}"
        );

        // Structural comparison of everything before the final summary
        // line: the sequence of begin/match/end events.
        let g_events: Vec<_> = g_lines[..g_lines.len() - 1]
            .iter()
            .map(|l| event_key(l))
            .collect();
        let r_events: Vec<_> = r_lines[..r_lines.len() - 1]
            .iter()
            .map(|l| event_key(l))
            .collect();
        assert_eq!(
            g_events, r_events,
            "begin/match/end sequence diverged for {pattern:?}"
        );

        // The final line of both streams must be the summary event.
        let g_summary: serde_json::Value = serde_json::from_str(g_lines.last().unwrap()).unwrap();
        let r_summary: serde_json::Value = serde_json::from_str(r_lines.last().unwrap()).unwrap();
        assert_eq!(
            g_summary["type"], "summary",
            "glep's last --json line is not a summary event for {pattern:?}"
        );
        assert_eq!(
            r_summary["type"], "summary",
            "sanity: rg's last --json line is not a summary event for {pattern:?}"
        );

        // matches/matched_lines/bytes_printed are computed by the same
        // grep-searcher/grep-printer machinery rg uses, so they must match
        // exactly for identical queries.
        assert_eq!(
            g_summary["data"]["stats"]["matches"], r_summary["data"]["stats"]["matches"],
            "stats.matches diverged for {pattern:?}"
        );
        assert_eq!(
            g_summary["data"]["stats"]["matched_lines"], r_summary["data"]["stats"]["matched_lines"],
            "stats.matched_lines diverged for {pattern:?}"
        );
        assert_eq!(
            g_summary["data"]["stats"]["bytes_printed"], r_summary["data"]["stats"]["bytes_printed"],
            "stats.bytes_printed diverged for {pattern:?}"
        );

        // Deliberately NOT compared (masked): `elapsed`/`elapsed_total` are
        // wall-clock timings from two separate process invocations, and
        // `searches`/`searches_with_match`/`bytes_searched` legitimately
        // differ because glep's index narrows candidate files before any
        // file is opened (see the module doc comment above).
    }
}
