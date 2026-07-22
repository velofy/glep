use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct SearchOpts {
    pub case_insensitive: bool,
    pub fixed: bool,
    pub files_with_matches: bool,
    pub before: usize,
    pub after: usize,
    pub json: bool,
    pub count: bool,
    pub multiline: bool,
}

// --- rg-compatible --json closing `summary` event -------------------------
//
// rg's `--json` stream ends with one extra line after all begin/match/
// context/end events: a `summary` event carrying `elapsed_total` (wall time
// for the whole invocation) and a `stats` object (aggregate counters).
// Captured from real ripgrep (`rg --json <pattern>`, ripgrep 15.1.0) as
// ground truth for field names and nesting:
//
//   {"data":{"elapsed_total":{"human":"0.011150s","nanos":11150209,"secs":0},
//    "stats":{"bytes_printed":643,"bytes_searched":73,
//    "elapsed":{"human":"0.001123s","nanos":1122750,"secs":0},
//    "matched_lines":3,"matches":3,"searches":3,"searches_with_match":2}},
//    "type":"summary"}
//
// Key order does not matter (JSON object equality is key-based, not
// positional); only the field *names* and nesting need to match.
//
// `grep_printer::Stats` (the per-file "end" event's own `stats` object)
// already derives a `Serialize` impl with exactly these field names, so we
// reuse that type directly for the `stats` sub-object instead of redefining
// it. `grep_printer::JSONBuilder` (grep-printer 0.2.x) has no `.stats(bool)`
// toggle to enable/disable stats collection like `StandardBuilder`/
// `SummaryBuilder` do; the JSON sink always tracks `Stats` internally, and
// `JSONSink::stats()` is unconditionally available after a search. So step
// 1's ".stats(true) on the JSON printer builder" doesn't apply here: nothing
// to opt into, we just harvest `sink.stats()` per file below.
//
// `NiceDuration` (the `{secs,nanos,human}` shape used for both `elapsed` and
// `elapsed_total`) is `pub(crate)` inside grep-printer, so it can't be
// reused from here; this local copy reproduces its exact Serialize output,
// including the "%.6f\"s\"" human format (e.g. "1.234567s").
#[derive(serde::Serialize)]
struct NiceDuration {
    secs: u64,
    nanos: u32,
    human: String,
}

impl From<Duration> for NiceDuration {
    fn from(d: Duration) -> NiceDuration {
        NiceDuration {
            secs: d.as_secs(),
            nanos: d.subsec_nanos(),
            human: format!("{:.6}s", d.as_secs_f64()),
        }
    }
}

#[derive(serde::Serialize)]
struct SummaryData {
    elapsed_total: NiceDuration,
    stats: grep_printer::Stats,
}

#[derive(serde::Serialize)]
struct SummaryEvent {
    data: SummaryData,
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Fold `other`'s counters into `total`, deliberately skipping `elapsed`.
///
/// IMPORTANT SEMANTIC NOTE: `searches` and `bytes_searched` (and the other
/// per-file counters folded here) legitimately DIFFER from rg's own summary
/// for the same query. glep's index narrows candidates before any file is
/// opened, so `files` (the slice `run` is called with) already excludes
/// files rg would have opened and searched itself; `searches` /
/// `searches_with_match` / `bytes_searched` below report glep's own honest
/// count of files it actually searched, not rg's. `matches`, `matched_lines`
/// and `bytes_printed` are computed from the same grep-searcher/grep-printer
/// machinery rg uses and are expected to match rg exactly for identical
/// queries (see tests/json_parity.rs). See also README's Interface section.
fn merge_stats(total: &mut grep_printer::Stats, other: &grep_printer::Stats) {
    total.add_searches(other.searches());
    total.add_searches_with_match(other.searches_with_match());
    total.add_bytes_searched(other.bytes_searched());
    total.add_bytes_printed(other.bytes_printed());
    total.add_matched_lines(other.matched_lines());
    total.add_matches(other.matches());
}

fn build_matcher(pattern: &str, opts: &SearchOpts) -> anyhow::Result<grep_regex::RegexMatcher> {
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(opts.case_insensitive);
    b.fixed_strings(opts.fixed);
    // rg's -U maps to: searcher.multi_line(true) so matches may span lines,
    // plus a matcher built without a line-terminator restriction so a
    // literal \n in the pattern is allowed to compile and match. We never
    // call RegexMatcherBuilder::line_terminator here (its default is
    // already None/unrestricted), so \n-containing patterns already
    // compile; the only builder change needed for -U is enabling the
    // regex "m" flag so ^/$ keep their per-line semantics once the
    // searcher stops feeding lines one at a time (verified empirically
    // against real rg: `rg -U '^foo'` still matches at line starts, not
    // just at the start of the whole file).
    if opts.multiline {
        b.multi_line(true);
    }
    Ok(b.build(pattern)?)
}

struct FoundSink(bool);

impl grep_searcher::Sink for FoundSink {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        _: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        self.0 = true;
        Ok(false) // stop at first match
    }
}

struct CountSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    multiline: bool,
    count: u64,
}

impl grep_searcher::Sink for CountSink<'_> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        m: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        if self.multiline {
            use grep_matcher::Matcher;
            let mut n = 0u64;
            self.matcher
                .find_iter(m.bytes(), |_| {
                    n += 1;
                    true
                })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            self.count += n.max(1);
        } else {
            self.count += 1;
        }
        Ok(true)
    }
}

fn search_one(
    matcher: &grep_regex::RegexMatcher,
    root: &Path,
    rel: &Path,
    opts: &SearchOpts,
) -> anyhow::Result<(Vec<u8>, bool, Option<grep_printer::Stats>)> {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(opts.before)
        .after_context(opts.after)
        .multi_line(opts.multiline)
        .build();
    let full = root.join(rel);
    if opts.count {
        let mut sink = CountSink {
            matcher,
            multiline: opts.multiline,
            count: 0,
        };
        searcher.search_path(matcher, &full, &mut sink)?;
        if sink.count > 0 {
            return Ok((
                format!("{}:{}\n", rel.display(), sink.count).into_bytes(),
                true,
                None,
            ));
        }
        return Ok((Vec::new(), false, None));
    }
    if opts.files_with_matches {
        let mut sink = FoundSink(false);
        searcher.search_path(matcher, &full, &mut sink)?;
        return Ok((Vec::new(), sink.0, None));
    }
    let mut buf = Vec::new();
    let matched;
    let mut stats = None;
    if opts.json {
        let mut printer = grep_printer::JSONBuilder::new().build(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_path(matcher, &full, &mut sink)?;
        matched = sink.has_match();
        stats = Some(sink.stats().clone());
    } else {
        let mut printer = grep_printer::StandardBuilder::new()
            .heading(false)
            .build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_path(matcher, &full, &mut sink)?;
        matched = sink.has_match();
    }
    Ok((buf, matched, stats))
}

/// Search `files` (relative paths, pre-sorted) under `root`. Prints results
/// in input order for determinism. Returns true if anything matched.
pub fn run(
    pattern: &str,
    root: &Path,
    files: &[PathBuf],
    opts: &SearchOpts,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<bool> {
    // Times the whole run, used for the --json summary event's
    // `elapsed_total` (and, per design, `stats.elapsed` too: see below).
    let start = Instant::now();
    // Build the matcher once up front; shared by reference across the rayon
    // closure (grep_regex::RegexMatcher is Sync). This also validates the
    // pattern before I/O, matching prior behavior.
    let matcher = build_matcher(pattern, opts)?;
    let mut found = false;
    let separate =
        (opts.before > 0 || opts.after > 0) && !opts.files_with_matches && !opts.json && !opts.count;
    let mut printed_any = false;
    let mut base = 0usize;
    let mut total_stats = grep_printer::Stats::new();
    for chunk in files.chunks(128) {
        let mut results: Vec<(usize, Vec<u8>, bool, Option<grep_printer::Stats>)> = chunk
            .par_iter()
            .enumerate()
            .map(|(i, rel)| match search_one(&matcher, root, rel, opts) {
                Ok((buf, matched, stats)) => (i, buf, matched, stats),
                Err(e) => {
                    eprintln!("glep: {}: {}", rel.display(), e);
                    (i, Vec::new(), false, None)
                }
            })
            .collect();
        results.sort_by_key(|(i, _, _, _)| *i);
        for (i, buf, matched, stats) in results {
            if let Some(s) = &stats {
                merge_stats(&mut total_stats, s);
            }
            if matched {
                found = true;
                let global_i = base + i;
                if opts.files_with_matches {
                    writeln!(out, "{}", files[global_i].display())?;
                } else {
                    if separate && printed_any {
                        writeln!(out, "--")?;
                    }
                    out.write_all(&buf)?;
                    printed_any = true;
                }
            }
        }
        base += chunk.len();
    }
    if opts.json {
        // Per design: both `elapsed_total` and `stats.elapsed` are filled
        // from this single measured wall-clock duration for the whole run,
        // rather than trying to reproduce rg's internal split between
        // "time summing individual per-file searches" (its `stats.elapsed`)
        // and "total process wall time" (its `elapsed_total`). Both fields
        // are masked out of tests/json_parity.rs's comparison, so this
        // simplification is safe; `merge_stats` above deliberately never
        // touches `elapsed`, so this is the only place it's set.
        let elapsed = start.elapsed();
        total_stats.add_elapsed(elapsed);
        let event = SummaryEvent {
            kind: "summary",
            data: SummaryData {
                elapsed_total: NiceDuration::from(elapsed),
                stats: total_stats,
            },
        };
        writeln!(out, "{}", serde_json::to_string(&event)?)?;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn corpus() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one hello\ntwo\nthree hello\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "nothing here\n").unwrap();
        dir
    }

    fn opts() -> SearchOpts {
        SearchOpts {
            case_insensitive: false,
            fixed: false,
            files_with_matches: false,
            before: 0,
            after: 0,
            json: false,
            count: false,
            multiline: false,
        }
    }

    #[test]
    fn default_output_is_path_line_text() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        let mut out = Vec::new();
        let found = run("hello", dir.path(), &files, &opts(), &mut out).unwrap();
        assert!(found);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "a.txt:1:one hello\na.txt:3:three hello\n"
        );
    }

    #[test]
    fn files_with_matches_prints_paths_once() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        let mut o = opts();
        o.files_with_matches = true;
        let mut out = Vec::new();
        let found = run("hello", dir.path(), &files, &o, &mut out).unwrap();
        assert!(found);
        assert_eq!(String::from_utf8(out).unwrap(), "a.txt\n");
    }

    #[test]
    fn no_match_returns_false() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt")];
        let mut out = Vec::new();
        let found = run("absent_zz", dir.path(), &files, &opts(), &mut out).unwrap();
        assert!(!found);
        assert!(out.is_empty());
    }

    #[test]
    fn case_insensitive_matches() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt")];
        let mut o = opts();
        o.case_insensitive = true;
        let mut out = Vec::new();
        assert!(run("HELLO", dir.path(), &files, &o, &mut out).unwrap());
    }
}
