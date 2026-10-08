use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct SearchOpts {
    pub case_insensitive: bool,
    pub fixed: bool,
    pub files_with_matches: bool,
    /// --files-without-match: emit names of files with NO match.
    pub files_without_match: bool,
    pub before: usize,
    pub after: usize,
    pub json: bool,
    pub count: bool,
    /// rg --count-matches: count match occurrences, not lines.
    pub count_matches: bool,
    pub multiline: bool,
    /// quit: stop at the first NUL byte (default, rg). convert: NULs become
    /// line terminators and a match yields the printer's binary notice
    /// (rg --binary). none: binary files are plain text (rg -a).
    pub binary: BinaryDetection,
    /// Index-relative prefix to strip from paths for display. Results
    /// always print relative to the cwd the user ran in, which may be a
    /// subdirectory of the discovered index root; "" means no stripping.
    pub display_prefix: PathBuf,
    /// First-match column per line (rg --column); implied by --vimgrep.
    pub column: bool,
    /// Byte offset of each printed line (rg -b).
    pub byte_offset: bool,
    /// One output line per match (rg --vimgrep = column + per_match +
    /// per_match_one_line + forced filename).
    pub vimgrep: bool,
    /// Strip leading whitespace on matched lines (rg --trim).
    pub trim: bool,
    /// Replaces the `:` between path and line fields (rg -0/--null).
    pub path_terminator: Option<u8>,
    /// Replaces `/` inside printed paths (rg --path-separator).
    pub path_separator: Option<u8>,
    /// With count, emit `path:0` for searched-but-unmatched files
    /// (rg --include-zero).
    pub include_zero: bool,
    /// Print the path field; rg suppresses it for a single file operand.
    pub with_filename: bool,
    /// Print the rg-style stats block after results (rg --stats).
    pub stats: bool,
    /// rg --multiline-dotall: `.` matches `\n` at the matcher level.
    pub multiline_dotall: bool,
    /// -E/--encoding label, resolved once in `run`.
    pub encoding: Option<String>,
    /// rg --line-buffered: flush after every record.
    pub line_buffered: bool,
    /// --color always: emit ANSI colors (rg's default spec set +
    /// any --colors overrides). auto/never resolve to false here —
    /// tty detection happens in cli.
    pub color: bool,
    /// Extra UserColorSpec strings from --colors, parsed in run().
    pub color_specs: Vec<String>,
    /// --passthru: emit all lines, not just matches/context.
    pub passthru: bool,
    /// --no-unicode: matcher-level unicode off (\w, ., classes).
    pub unicode: bool,
    /// --null-data: NUL is the line terminator (searcher + matcher).
    pub null_data: bool,
    /// --dfa-size-limit (None = default)
    pub dfa_size_limit: Option<usize>,
    /// --regex-size-limit
    pub regex_size_limit: Option<usize>,
    /// --field-match-separator (bytes between fields on match lines)
    pub field_match_separator: Option<Vec<u8>>,
    /// --field-context-separator (bytes between fields on context lines)
    pub field_context_separator: Option<Vec<u8>>,
    /// --context-separator ("" = none)
    pub context_separator: Option<Vec<u8>>,
    /// -P/--pcre2: use the PCRE2 engine instead of Rust's regex.
    pub pcre2: bool,
    /// -z: decompress .gz files via flate2 and search the decoded stream.
    pub search_zip: bool,
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

fn build_pcre2_matcher(
    pattern: &str,
    opts: &SearchOpts,
) -> anyhow::Result<grep_pcre2::RegexMatcher> {
    let mut b = grep_pcre2::RegexMatcherBuilder::new();
    b.fixed_strings(opts.fixed)
        .caseless(opts.case_insensitive)
        .jit_if_available(true);
    if opts.multiline {
        b.multi_line(true);
    }
    Ok(b.build(pattern)?)
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
    if opts.multiline_dotall {
        b.dot_matches_new_line(true);
    if !opts.unicode {
        b.unicode(false);
    if opts.null_data {
        // NUL is the record separator: the matcher must know so `.`
        // doesn't stop at NUL and ^/$ anchor on NUL boundaries.
        b.line_terminator(Some(0));
    }
    if let Some(d) = opts.dfa_size_limit {
        b.dfa_size_limit(d);
    }
    if let Some(s) = opts.regex_size_limit {
        b.size_limit(s);
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

struct CountSink<'a, M: grep_matcher::Matcher> {
    matcher: &'a M,
    multiline: bool,
    /// Count occurrences per line instead of lines (-U needs it always;
    /// --count-matches needs it for output)
    occurrences: bool,
    count: u64,
    /// Match occurrences (a line can hold several) — needed by --stats.
    occurrences: u64,
}

impl<M: grep_matcher::Matcher> grep_searcher::Sink for CountSink<'_, M> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        m: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        // Occurrences are always counted (--stats wants them);
        // under -U the reported count is occurrences (a match may span
        // lines), under plain mode it's matched lines.
        use grep_matcher::Matcher;
        let mut n = 0u64;
        self.matcher
            .find_iter(m.bytes(), |_| {
                n += 1;
                true
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let n = n.max(1);
        self.occurrences += n;
        self.count += if self.multiline { n } else { 1 };
        if self.multiline || self.occurrences {
            use grep_matcher::Matcher;
        if self.multiline {
            let mut n = 0u64;
            self.matcher
                .find_iter(m.bytes(), |_| {
                    n += 1;
                    true
                })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            self.count += n.max(1);
        } else {
            self.count += 1;
        }
        Ok(true)
    }
}

fn search_one<M: grep_matcher::Matcher>(
    matcher: &M,
/// The compressed-body half of `search_one`: identical sink wiring,
/// but the source is a decoded reader rather than a path.
fn search_one_decoded<R: std::io::Read>(
    matcher: &grep_regex::RegexMatcher,
    mut reader: R,
    rel: &Path,
    opts: &SearchOpts,
    searcher: &mut grep_searcher::Searcher,
) -> anyhow::Result<(Vec<u8>, bool, Option<grep_printer::Stats>)> {
    if opts.count {
        let mut sink = CountSink {
            matcher,
            multiline: opts.multiline,
            count: 0,
        };
        searcher.search_reader(matcher, &mut reader, &mut sink)?;
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
        searcher.search_reader(matcher, &mut reader, &mut sink)?;
        return Ok((Vec::new(), sink.0, None));
    }
    let mut buf = Vec::new();
    let matched;
    let mut stats = None;
    if opts.json {
        let mut printer = grep_printer::JSONBuilder::new().build(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_reader(matcher, &mut reader, &mut sink)?;
        matched = sink.has_match();
        stats = Some(sink.stats().clone());
    } else {
        let mut printer = grep_printer::StandardBuilder::new()
            .heading(false)
            .build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_reader(matcher, &mut reader, &mut sink)?;
        matched = sink.has_match();
    }
    Ok((buf, matched, stats))
}

fn search_one(
    matcher: &grep_regex::RegexMatcher,
    root: &Path,
    rel: &Path,
    opts: &SearchOpts,
    encoding: Option<grep_searcher::Encoding>,
) -> anyhow::Result<(Vec<u8>, bool, Option<grep_printer::Stats>)> {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(opts.binary.clone())
    let mut builder = SearcherBuilder::new();
    builder
        .binary_detection(BinaryDetection::quit(0))
    let mut sb = SearcherBuilder::new();
    // --null-data makes NUL a record separator, so it can't stay a
    // binary-detection signal — the searcher treats the file as text.
    sb.binary_detection(if opts.null_data {
        BinaryDetection::none()
    } else {
        BinaryDetection::quit(0)
    })
        .line_number(true)
        .before_context(opts.before)
        .after_context(opts.after)
        .multi_line(opts.multiline);
    if let Some(enc) = &encoding {
        builder.encoding(Some(enc.clone()));
    }
    let mut searcher = builder.build();
        .multi_line(opts.multiline)
        .passthru(opts.passthru)
        .build();
    if opts.null_data {
        sb.line_terminator(grep_matcher::LineTerminator::byte(0));
    }
    let mut searcher = sb.build();
    let full = root.join(rel);
    // Paths print relative to the user's cwd, not the index root: strip
    // the scope prefix (no-op when the search ran at the root itself).
    let display: &Path = if opts.display_prefix.as_os_str().is_empty() {
        rel
    } else {
        rel.strip_prefix(&opts.display_prefix).unwrap_or(rel)
    };
    // -z: compressed files search their decoded stream, never their raw
    // bytes. Everything else (and unsupported formats) search normally.
    let compressed = opts.search_zip
        && rel.extension().map(|x| x == "gz").unwrap_or(false);
    if compressed {
        let file = std::fs::File::open(&full)?;
        let mut dec = flate2::read::GzDecoder::new(file);
        // A corrupt/mislabeled .gz yields an io error mid-decode; report
        // it per-file and move on (the reference does the same: a bordered
        // gzip warning to stderr, other files still searched).
        return match search_one_decoded(matcher, &mut dec, rel, opts, &mut searcher) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!("glep: {}: {e}", rel.display());
                Ok((Vec::new(), false, None))
            }
        };
    }
    if opts.count {
    if opts.count || opts.count_matches {
        let mut sink = CountSink {
            matcher,
            multiline: opts.multiline,
            occurrences: opts.count_matches,
            count: 0,
            occurrences: 0,
        };
        searcher.search_path(matcher, &full, &mut sink)?;
        if sink.count > 0 || opts.stats {
            let stats = opts.stats.then(|| {
                file_stats(&full, sink.count, sink.occurrences)
            });
            return Ok((
                format!("{}:{}\n", display.display(), sink.count).into_bytes(),
                true,
                None,
                if sink.count > 0 {
                    format!("{}:{}\n", rel.display(), sink.count).into_bytes()
                } else {
                    Vec::new()
                },
                sink.count > 0,
                stats,
            ));
        if sink.count > 0 || opts.include_zero {
            // `path` + path terminator (`:` normally, NUL under --null) +
            // count; bare `N` when the path is suppressed.
            let mut line = Vec::new();
            if opts.with_filename {
                write_path(&mut line, rel, opts.path_separator)?;
                line.push(opts.path_terminator.unwrap_or(b':'));
            }
            line.extend_from_slice(sink.count.to_string().as_bytes());
            line.push(b'\n');
            return Ok((line, sink.count > 0, None));
        }
        return Ok((Vec::new(), false, None));
    }
    if opts.files_with_matches {
        if opts.stats {
            // --stats needs real counts: run the count sink instead of
            // the early-exit one.
            let mut sink = CountSink {
                matcher,
                multiline: opts.multiline,
                count: 0,
                occurrences: 0,
            };
            searcher.search_path(matcher, &full, &mut sink)?;
            return Ok((
                Vec::new(),
                sink.count > 0,
                Some(file_stats(&full, sink.count, sink.occurrences)),
            ));
        }
        let mut sink = FoundSink(false);
        searcher.search_path(matcher, &full, &mut sink)?;
        return Ok((Vec::new(), sink.0, None));
    }
    let mut buf = Vec::new();
    let matched;
    let stats;
    if opts.json {
        let mut printer = grep_printer::JSONBuilder::new().build(&mut buf);
        let mut sink = printer.sink_with_path(matcher, display);
        searcher.search_path(matcher, &full, &mut sink)?;
        matched = sink.has_match();
        stats = Some(sink.stats().clone());
    } else {
        let mut printer = grep_printer::StandardBuilder::new()
            .heading(false)
            .path(opts.with_filename)
            .column(opts.column || opts.vimgrep)
            .byte_offset(opts.byte_offset)
            .trim_ascii(opts.trim)
            .per_match(opts.vimgrep)
            .per_match_one_line(true)
            .path_terminator(opts.path_terminator)
            .separator_path(opts.path_separator)
            .stats(opts.stats)
            .build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, display);
        let mut printer_b = grep_printer::StandardBuilder::new();
        printer_b.heading(false);
        let mut printer = printer_b.build_no_color(&mut buf);
        let mut b = grep_printer::StandardBuilder::new();
        b.heading(false);
        if let Some(s) = &opts.field_match_separator {
            b.separator_field_match(s.clone());
        }
        if let Some(s) = &opts.field_context_separator {
            b.separator_field_context(s.clone());
        }
        if let Some(s) = &opts.context_separator {
            b.separator_context(if s.is_empty() { None } else { Some(s.clone()) });
        }
        let mut printer = b.build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_path(matcher, &full, &mut sink)?;
        matched = sink.has_match();
        stats = sink.stats().cloned();
        let mut b = grep_printer::StandardBuilder::new();
        b.heading(false);
        if opts.color {
            let mut all = grep_printer::default_color_specs();
            for s in &opts.color_specs {
                if let Ok(spec) = s.parse::<grep_printer::UserColorSpec>() {
                    all.push(spec);
                }
            }
            b.color_specs(grep_printer::ColorSpecs::new(&all));
            let mut printer = b.build(termcolor::Ansi::new(&mut buf));
            let mut sink = printer.sink_with_path(matcher, rel);
            searcher.search_path(matcher, &full, &mut sink)?;
            matched = sink.has_match();
        } else {
            let mut printer = b.build_no_color(&mut buf);
            let mut sink = printer.sink_with_path(matcher, rel);
            searcher.search_path(matcher, &full, &mut sink)?;
            matched = sink.has_match();
        }
    }
    Ok((buf, matched, stats))
}

/// Write `rel` honoring --path-separator (components rejoined by the
/// custom byte).
fn write_path<W: std::io::Write + ?Sized>(out: &mut W, rel: &Path, sep: Option<u8>) -> std::io::Result<()> {
    match sep {
        None => out.write_all(rel.as_os_str().as_encoded_bytes()),
        Some(sep) => {
            let mut first = true;
            for c in rel.components() {
                if !first {
                    out.write_all(&[sep])?;
                }
                first = false;
                out.write_all(c.as_os_str().as_encoded_bytes())?;
            }
            Ok(())
        }
    }
/// Stats for paths searched without the printer (count/fwm modes):
/// occurrences, matched lines, one search, bytes from the file size.
fn file_stats(full: &Path, matched_lines: u64, matches: u64) -> grep_printer::Stats {
    let mut s = grep_printer::Stats::new();
    s.add_searches(1);
    s.add_searches_with_match(u64::from(matched_lines > 0));
    s.add_matched_lines(matched_lines);
    s.add_matches(matches);
    if let Ok(m) = std::fs::metadata(full) {
        s.add_bytes_searched(m.len() as u64);
    }
    s
}

/// Search `files` (relative paths, pre-sorted) under `root`. Prints results
/// in input order for determinism. Returns true if anything matched.
pub fn run(
    pattern: &str,
    root: &Path,
    files: &[PathBuf],
    // For --files-without-match: the full live+filtered set. Files that
    // were narrowed out by the index can't contain the pattern, so
    // they're "without match" by construction and emit directly — the
    // index still narrows what's actually searched.
    universe: Option<&[PathBuf]>,
    opts: &SearchOpts,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<bool> {
    // Two monomorphized paths — one per engine. Sinks and the searcher are
    // generic over Matcher; only the builder differs.
    if opts.pcre2 {
        let matcher = build_pcre2_matcher(pattern, opts)?;
        return run_impl(&matcher, root, files, opts, out);
    }
    let matcher = build_matcher(pattern, opts)?;
    run_impl(&matcher, root, files, opts, out)
}

fn run_impl<M: grep_matcher::Matcher + Sync>(
    matcher: &M,
    root: &Path,
    files: &[PathBuf],
    opts: &SearchOpts,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<bool> {
    // Times the whole run, used for the --json summary event's
    // `elapsed_total` (and, per design, `stats.elapsed` too: see below).
    let start = Instant::now();
    let mut matched_names = std::collections::BTreeSet::new();
    // Build the matcher once up front; shared by reference across the rayon
    // closure (grep_regex::RegexMatcher is Sync). This also validates the
    // pattern before I/O, matching prior behavior.
    let matcher = build_matcher(pattern, opts)?;
    // Resolve the -E label up front so a bad label errors before any I/O
    // (the reference exits 2 on an unknown encoding).
    let encoding = match &opts.encoding {
        Some(label) => Some(
            grep_searcher::Encoding::new(label)
                .map_err(|e| anyhow::anyhow!("{label}: {e}"))?,
        ),
        None => None,
    };
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
            .map(|(i, rel)| match search_one(&matcher, root, rel, opts, encoding.clone()) {
            .map(|(i, rel)| match search_one(matcher, root, rel, opts) {
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
            // Write any produced output, not just matches: --passthru
            // emits every line including non-matching ones, so a
            // no-match file can still produce bytes.
            if matched || !buf.is_empty() {
                if matched {
                    found = true;
                }
            if opts.files_without_match {
                // Collect matched names; the complement of `universe`
                // emits after the loop.
                if matched {
                    matched_names.insert(files[base + i].clone());
                }
                continue;
            }
            if matched {
                found = true;
                let global_i = base + i;
                if opts.files_with_matches {
                    let display = files[global_i]
                        .strip_prefix(&opts.display_prefix)
                        .unwrap_or(&files[global_i]);
                    writeln!(out, "{}", display.display())?;
                    if matched {
                        writeln!(out, "{}", files[global_i].display())?;
                    }
                } else {
                    if separate && printed_any {
                        match &opts.context_separator {
                            // "" disables the separator entirely
                            Some(v) if v.is_empty() => {}
                            Some(v) => {
                                out.write_all(v)?;
                                out.write_all(b"\n")?;
                            }
                            None => writeln!(out, "--")?,
                        }
                    }
                    out.write_all(&buf)?;
                    if opts.line_buffered {
                        out.flush()?;
                    }
                    printed_any = true;
                }
            }
            if opts.files_with_matches {
                if matched {
                    let global_i = base + i;
                    write_path(out, &files[global_i], opts.path_separator)?;
                    out.write_all(&[opts.path_terminator.unwrap_or(b'\n')])?;
                }
                continue;
            }
            // Non-empty buffers print even when the file didn't match:
            // --include-zero -c emits `path:0` lines for searched files.
            if buf.is_empty() {
                continue;
            }
            if separate && printed_any {
                writeln!(out, "--")?;
            }
            out.write_all(&buf)?;
            printed_any = true;
        }
        base += chunk.len();
    }
    if opts.files_without_match {
        let uni = universe.unwrap_or(files);
        for p in uni {
            if !matched_names.contains(p) {
                found = true;
                writeln!(out, "{}", p.display())?;
            }
        }
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
                stats: total_stats.clone(),
            },
        };
        writeln!(out, "{}", serde_json::to_string(&event)?)?;
    }
    if opts.stats && !opts.json {
        // rg's stats block: a blank line then fixed-order counters.
        // `bytes_printed` is whatever the printer tracked (standard mode);
        // count/-l modes report 0 there like rg's non-printer paths.
        let elapsed = start.elapsed();
        writeln!(
            out,
            "\n{} matches\n{} matched lines\n{} files contained matches\n{} files searched\n{} bytes printed\n{} bytes searched\n{:.6} seconds spent searching\n{:.6} seconds total",
            total_stats.matches(),
            total_stats.matched_lines(),
            total_stats.searches_with_match(),
            total_stats.searches(),
            total_stats.bytes_printed(),
            total_stats.bytes_searched(),
            elapsed.as_secs_f64(),
            elapsed.as_secs_f64()
        )?;
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
            files_without_match: false,
            before: 0,
            after: 0,
            json: false,
            count: false,
            multiline: false,
            binary: BinaryDetection::quit(0),
            display_prefix: PathBuf::new(),
            column: false,
            byte_offset: false,
            vimgrep: false,
            trim: false,
            path_terminator: None,
            path_separator: None,
            include_zero: false,
            with_filename: true,
            stats: false,
            multiline_dotall: false,
            encoding: None,
            line_buffered: false,
            count_matches: false,
            color: false,
            color_specs: Vec::new(),
            passthru: false,
            unicode: true,
            null_data: false,
            dfa_size_limit: None,
            regex_size_limit: None,
            field_match_separator: None,
            field_context_separator: None,
            context_separator: None,
            pcre2: false,
            search_zip: false,
        }
    }

    #[test]
    fn default_output_is_path_line_text() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        let mut out = Vec::new();
        let found = run("hello", dir.path(), &files, None, &opts(), &mut out).unwrap();
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
        let found = run("hello", dir.path(), &files, None, &o, &mut out).unwrap();
        assert!(found);
        assert_eq!(String::from_utf8(out).unwrap(), "a.txt\n");
    }

    #[test]
    fn no_match_returns_false() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt")];
        let mut out = Vec::new();
        let found = run("absent_zz", dir.path(), &files, None, &opts(), &mut out).unwrap();
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
        assert!(run("HELLO", dir.path(), &files, None, &o, &mut out).unwrap());
    }
}
