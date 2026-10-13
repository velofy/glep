use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Condvar, Mutex};
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
    /// --color always: emit ANSI colors (rg's default spec set plus any
    /// --colors overrides). auto/never resolve to false here: tty
    /// detection happens in cli.
    pub color: bool,
    /// Extra UserColorSpec strings from --colors, parsed once per run.
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
    /// The context separator (rg's default `--`, escapes already decoded;
    /// empty still prints a line break). None = --no-context-separator.
    pub context_separator: Option<Vec<u8>>,
    /// -P/--pcre2: use the PCRE2 engine instead of Rust's regex.
    pub pcre2: bool,
    /// --engine auto (rg's hybrid mode): Rust's regex when the pattern
    /// builds there, PCRE2 when it does not.
    pub engine_auto: bool,
    /// Transcode files that start with a UTF-8/UTF-16 byte-order mark
    /// (rg's default); `-E none` turns it off.
    pub bom_sniffing: bool,
    /// -z: decompress .gz files via flate2 and search the decoded stream.
    pub search_zip: bool,
    /// -M preview: a truncated long line ends with a `[... N more matches]`
    /// marker instead of the bare omission.
    pub max_columns_preview: bool,
    /// rg --stop-on-nonmatch: stop reading a file at the first
    /// non-matching line that follows a match (a per-file searcher
    /// setting, exactly like rg).
    pub stop_on_nonmatch: bool,
    /// rg --no-messages: suppress per-file nonfatal warnings.
    pub no_messages: bool,
    /// OSC8 hyperlinks for emitted paths (rg --hyperlink-format), resolved
    /// once in cli (format parsed, {host} from --hostname-bin or the
    /// platform). Like rg, links are only written when color is on.
    pub hyperlink: Option<grep_printer::HyperlinkConfig>,
    /// Worker thread count (rg -j); None or 0 means one per core.
    pub threads: Option<usize>,
    /// -r: rewrite matched text in output lines (capture refs like $1
    /// resolve; byte-level, applied by the printer).
    pub replace: Option<Vec<u8>>,
    /// --pre COMMAND: search `COMMAND <path>`'s stdout instead of the file
    /// (subprocess per candidate; --pre-glob restricts which paths get it).
    pub pre: Option<String>,
    /// --pre-glob: only --pre-process files matching this glob.
    pub pre_glob: Option<globset::GlobMatcher>,
    /// --crlf: `\r\n` line terminators (matcher crlf mode plus the
    /// searcher's CRLF line terminator, like rg). --null-data overrides it.
    pub crlf: bool,
    /// --no-mmap: read every file through the searcher's buffer instead of
    /// searching a memory map (glep maps by default; --mmap is a no-op).
    pub no_mmap: bool,
    pub word: bool,
    pub line_regexp: bool,
    pub smart_case: bool,
    pub invert: bool,
    pub max_count: Option<u64>,
    pub max_columns: Option<u64>,
    pub line_number: bool,
    pub heading: bool,
    pub only_matching: bool,
    pub quiet: bool,
}

impl SearchOpts {
    /// The searcher's line terminator, which rg's printers also end every
    /// record and separator with: NUL under --null-data, CRLF under
    /// --crlf (the two are a last-wins pair, resolved in cli), else LF.
    pub fn line_term(&self) -> &'static [u8] {
        if self.null_data {
            b"\0"
        } else if self.crlf {
            b"\r\n"
        } else {
            b"\n"
        }
    }

    /// A summary record's terminator: the -0 path terminator when set,
    /// else the line terminator (rg's summary printer).
    fn push_record_end(&self, buf: &mut Vec<u8>) {
        match self.path_terminator {
            Some(t) => buf.push(t),
            None => buf.extend_from_slice(self.line_term()),
        }
    }
}

impl Default for SearchOpts {
    /// rg's defaults: line numbers on, filenames on, quit-at-NUL binary
    /// detection, unicode on, everything else off.
    fn default() -> SearchOpts {
        SearchOpts {
            case_insensitive: false,
            fixed: false,
            files_with_matches: false,
            files_without_match: false,
            before: 0,
            after: 0,
            json: false,
            count: false,
            count_matches: false,
            multiline: false,
            binary: BinaryDetection::quit(b'\x00'),
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
            color: false,
            color_specs: Vec::new(),
            passthru: false,
            unicode: true,
            null_data: false,
            dfa_size_limit: None,
            regex_size_limit: None,
            field_match_separator: None,
            field_context_separator: None,
            context_separator: Some(b"--".to_vec()),
            pcre2: false,
            engine_auto: false,
            bom_sniffing: true,
            search_zip: false,
            max_columns_preview: false,
            stop_on_nonmatch: false,
            no_messages: false,
            hyperlink: None,
            threads: None,
            replace: None,
            pre: None,
            pre_glob: None,
            crlf: false,
            no_mmap: false,
            word: false,
            line_regexp: false,
            smart_case: false,
            invert: false,
            max_count: None,
            max_columns: None,
            line_number: true,
            heading: false,
            only_matching: false,
            quiet: false,
        }
    }
}

/// Workloads of at most this many files are searched on the calling
/// thread with one reusable searcher and zero synchronization (the
/// agent-small-repo fast path); anything larger goes through the worker
/// pool and the in-order printer thread. Tests that want the pool path
/// use a corpus larger than this.
pub const SMALL_WORKLOAD: usize = 32;

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
// `JSONSink::stats()` is unconditionally available after a search. So
// there is nothing to opt into, we just harvest `sink.stats()` per file.
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
    // rg's own PCRE2 configuration (hiargs matcher_pcre2): multi_line
    // always on so ^/$ anchor per line; case/boundary modes; JIT with a
    // 10 MB stack on 64-bit targets; utf+ucp unless --no-unicode; dotall
    // only under -U; CRLF-aware anchors under --crlf.
    let mut b = grep_pcre2::RegexMatcherBuilder::new();
    b.multi_line(true)
        .fixed_strings(opts.fixed)
        .caseless(opts.case_insensitive)
        .case_smart(opts.smart_case)
        .word(opts.word)
        .whole_line(opts.line_regexp);
    if cfg!(target_pointer_width = "64") {
        b.jit_if_available(true)
            .max_jit_stack_size(Some(10 * (1 << 20)));
    }
    if opts.unicode {
        b.utf(true).ucp(true);
    }
    if opts.multiline {
        b.dotall(opts.multiline_dotall);
    }
    if opts.crlf {
        b.crlf(true);
    }
    Ok(b.build(pattern)?)
}

fn build_matcher(pattern: &str, opts: &SearchOpts) -> anyhow::Result<grep_regex::RegexMatcher> {
    // rg's own Rust-regex configuration (hiargs matcher_rust), knob for
    // knob. multi_line is always on so ^/$ are line anchors (the fast
    // whole-buffer line search depends on it). -S resolves case from the
    // pattern AST (an inline (?i) beats everything); -w/-x are a
    // last-wins pair in cli.
    let mut b = RegexMatcherBuilder::new();
    b.multi_line(true)
        .unicode(opts.unicode)
        .octal(false)
        .fixed_strings(opts.fixed)
        .case_insensitive(opts.case_insensitive)
        .case_smart(opts.smart_case)
        .word(opts.word)
        .whole_line(opts.line_regexp);
    if opts.multiline {
        // -U: matches may span lines, so the matcher has no line
        // terminator (a literal \n in the pattern is fine) and `.` spans
        // lines only under --multiline-dotall. Under --crlf the anchors
        // still treat \r\n as a line end.
        b.dot_matches_new_line(opts.multiline_dotall);
        if opts.crlf {
            b.crlf(true).line_terminator(None);
        }
    } else {
        // Line-oriented: \n is the line terminator, so a pattern that
        // must match it is an error (rg: "the literal \n is not allowed
        // in a regex") and classes like \s never match it. The same
        // knob lets the searcher use its fast whole-buffer line search.
        b.line_terminator(Some(b'\n')).dot_matches_new_line(false);
        if opts.crlf {
            b.crlf(true);
        }
        if opts.null_data {
            b.line_terminator(Some(b'\0'));
        }
    }
    if let Some(s) = opts.regex_size_limit {
        b.size_limit(s);
    }
    if let Some(d) = opts.dfa_size_limit {
        b.dfa_size_limit(d);
    }
    // With binary detection on, a NUL can never be matched (detection
    // quits at it or rewrites it), so a pattern that requires one is an
    // error, as in rg.
    if opts.binary != BinaryDetection::none() && !opts.null_data {
        b.ban_byte(Some(b'\0'));
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

struct CountSink<'a, M: Matcher> {
    matcher: &'a M,
    /// Run `find_iter` on each matched line to count real occurrences:
    /// needed by --count-matches, -U (a match may span lines), -o (rg -o
    /// -c counts occurrences), -v (each non-matching line counts once via
    /// the max(1) floor), and --stats (true occurrence count).
    occurrences_mode: bool,
    /// What -c prints: occurrences in occurrence-mode, matched lines else.
    count: u64,
    /// True match occurrences, for --stats.
    occurrences: u64,
    /// When -c counts occurrences: count_matches || multiline || only_matching.
    count_occurrences: bool,
}

impl<'a, M: Matcher> CountSink<'a, M> {
    fn new(matcher: &'a M, opts: &SearchOpts) -> CountSink<'a, M> {
        let count_occurrences = opts.count_matches || opts.multiline || opts.only_matching;
        CountSink {
            matcher,
            occurrences_mode: count_occurrences || opts.stats,
            count: 0,
            occurrences: 0,
            count_occurrences,
        }
    }
}

impl<M: Matcher> grep_searcher::Sink for CountSink<'_, M> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        m: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        if self.occurrences_mode {
            let mut n = 0u64;
            self.matcher
                .find_iter(m.bytes(), |_| {
                    n += 1;
                    true
                })
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            // Under -v the sink is invoked on non-matching lines: the
            // matcher finds 0 occurrences but the line still counts once.
            let n = n.max(1);
            self.occurrences += n;
            self.count += if self.count_occurrences { n } else { 1 };
        } else {
            self.count += 1;
            self.occurrences += 1;
        }
        Ok(true)
    }
}

/// The StandardBuilder config shared by the no-color and ANSI paths.
/// Built once per worker and reused for every file (building a printer
/// from it only clones the config).
fn standard_printer_builder(opts: &SearchOpts) -> grep_printer::StandardBuilder {
    let mut b = grep_printer::StandardBuilder::new();
    b.heading(opts.heading)
        .path(opts.with_filename)
        .column(opts.column || opts.vimgrep)
        .byte_offset(opts.byte_offset)
        .trim_ascii(opts.trim)
        .per_match(opts.vimgrep)
        .per_match_one_line(opts.vimgrep)
        .only_matching(opts.only_matching)
        .max_columns(opts.max_columns)
        .max_columns_preview(opts.max_columns_preview)
        .replacement(opts.replace.clone())
        .path_terminator(opts.path_terminator)
        .separator_path(opts.path_separator)
        .stats(opts.stats);
    if let Some(h) = &opts.hyperlink {
        b.hyperlink(h.clone());
    }
    if let Some(s) = &opts.field_match_separator {
        b.separator_field_match(s.clone());
    }
    if let Some(s) = &opts.field_context_separator {
        b.separator_field_context(s.clone());
    }
    b.separator_context(opts.context_separator.clone());
    if opts.color {
        b.color_specs(color_specs(&opts.color_specs));
    }
    b
}

/// rg's default color specs plus any --colors overrides.
pub(crate) fn color_specs(user: &[String]) -> grep_printer::ColorSpecs {
    let mut all = grep_printer::default_color_specs();
    for s in user {
        if let Ok(spec) = s.parse::<grep_printer::UserColorSpec>() {
            all.push(spec);
        }
    }
    grep_printer::ColorSpecs::new(&all)
}

/// Per-thread search state: one `Searcher` (and one configured printer
/// builder) reused across every file this worker touches.
/// `grep_searcher::Searcher` holds its decode/line buffers in RefCells
/// precisely so a single instance can be reused across files; rebuilding it
/// per file threw that reuse away and paid a fresh allocation plus cold
/// buffers per file. ripgrep itself clones one SearchWorker per walker
/// thread and reuses it for the whole run.
struct WorkerState {
    searcher: Searcher,
    printer: grep_printer::StandardBuilder,
    /// Path rendering for the summary-style modes (-l/-c).
    paths: PathStyle,
}

impl WorkerState {
    fn new(opts: &SearchOpts, encoding: Option<&grep_searcher::Encoding>) -> WorkerState {
        // Line-number tracking is only needed by the printing sinks; rg
        // itself searches without line numbers unless output needs them.
        // -l/-c/-q modes skip the bookkeeping entirely.
        let need_line_numbers = opts.line_number
            && !opts.files_with_matches
            && !opts.files_without_match
            && !opts.count
            && !opts.count_matches
            && !opts.quiet;
        // --null-data makes NUL a record separator, so it can't stay a
        // binary-detection signal: the searcher treats the file as text.
        let detection = if opts.null_data {
            BinaryDetection::none()
        } else {
            opts.binary.clone()
        };
        let mut sb = SearcherBuilder::new();
        sb.binary_detection(detection)
            .line_number(need_line_numbers)
            .invert_match(opts.invert)
            .max_matches(opts.max_count)
            .before_context(opts.before)
            .after_context(opts.after)
            .multi_line(opts.multiline)
            .passthru(opts.passthru)
            .stop_on_nonmatch(opts.stop_on_nonmatch);
        if let Some(enc) = encoding {
            sb.encoding(Some(enc.clone()));
        }
        sb.bom_sniffing(opts.bom_sniffing);
        if opts.null_data {
            sb.line_terminator(grep_matcher::LineTerminator::byte(0));
        } else if opts.crlf {
            sb.line_terminator(grep_matcher::LineTerminator::crlf());
        }
        WorkerState {
            searcher: sb.build(),
            printer: standard_printer_builder(opts),
            paths: PathStyle::new(opts),
        }
    }
}

/// The bytes to search for one file: an mmap when mapping succeeded
/// (zero-copy: the searcher runs directly against page-cache pages, no
/// kernel->user buffer copy), else the path (streamed through the
/// searcher's line buffer). ripgrep's own macOS heuristic disables mmap
/// with a 2018 "should re-evaluate" note; we re-evaluated: on
/// page-cache-bound workloads (every-file searches) the copy elimination
/// is a significant win. SIGBUS on concurrent truncation is the accepted
/// risk, identical to rg's documented --mmap behavior on Linux.
///
/// Modes whose bytes are not the file's own bytes take a reader instead:
/// `--pre` searches a preprocessor's stdout and `-z` decodes a gzip
/// stream. `--binary` (convert detection) and `--no-mmap` read the file
/// through the line buffer, which is also what rewrites NULs and detects
/// binary data past the first buffer exactly like rg's default reader.
enum Haystack<'a> {
    Map(memmap2::Mmap),
    Path(PathBuf),
    Gz(PathBuf),
    Pre(&'a str, PathBuf),
}

impl<'a> Haystack<'a> {
    fn open(full: &Path, rel: &Path, opts: &'a SearchOpts) -> Haystack<'a> {
        if let Some(cmd) = &opts.pre {
            if opts.pre_glob.as_ref().is_none_or(|g| g.is_match(rel)) {
                return Haystack::Pre(cmd, full.to_path_buf());
            }
        }
        if opts.search_zip && rel.extension().is_some_and(|x| x == "gz") {
            return Haystack::Gz(full.to_path_buf());
        }
        if opts.no_mmap || (opts.binary.convert_byte().is_some() && !opts.null_data) {
            return Haystack::Path(full.to_path_buf());
        }
        match std::fs::File::open(full) {
            Ok(f) => match unsafe { memmap2::Mmap::map(&f) } {
                Ok(m) if !m.is_empty() => Haystack::Map(m),
                _ => Haystack::Path(full.to_path_buf()),
            },
            Err(_) => Haystack::Path(full.to_path_buf()),
        }
    }

    fn search<M: Matcher, S: grep_searcher::Sink>(
        &self,
        searcher: &mut Searcher,
        matcher: &M,
        sink: S,
    ) -> Result<(), S::Error> {
        match self {
            Haystack::Map(m) => searcher.search_slice(matcher, &m[..], sink),
            Haystack::Path(p) => searcher.search_path(matcher, p, sink),
            Haystack::Gz(p) => {
                use grep_searcher::SinkError;
                let file = std::fs::File::open(p).map_err(S::Error::error_io)?;
                let dec = flate2::read::GzDecoder::new(file);
                searcher.search_reader(matcher, dec, sink)
            }
            Haystack::Pre(cmd, p) => {
                // rg's contract: the path is the sole argument and the file
                // is also the command's stdin.
                use grep_searcher::SinkError;
                let stdin = std::fs::File::open(p).map_err(S::Error::error_io)?;
                let mut child = std::process::Command::new(cmd)
                    .arg(p)
                    .stdin(stdin)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| {
                        S::Error::error_message(format!(
                            "preprocessor command could not start: '{cmd}': {e}"
                        ))
                    })?;
                let stdout = child.stdout.take().expect("piped stdout");
                // search_reader consumes (and so closes) the pipe before
                // the wait: a sink that stops early (-l, -q, -m) makes the
                // child see EPIPE instead of blocking on a full pipe.
                let r = searcher.search_reader(matcher, stdout, sink);
                let _ = child.wait();
                r
            }
        }
    }

    fn is_stream(&self) -> bool {
        matches!(self, Haystack::Gz(_) | Haystack::Pre(..))
    }
}

/// How a bare path record is rendered: the -l/-c/--count-matches/
/// --files-without-match records and the --files listing. Without color
/// it is the path's bytes with --path-separator applied. Under --color it
/// goes through grep-printer's `PathPrinter`, the printer rg itself uses
/// for --files, which writes exactly what rg's summary printer writes for
/// a path: the hyperlink opener (when --hyperlink-format is set), the
/// path color spec, the path, a reset, and the hyperlink closer.
pub(crate) struct PathStyle {
    sep: Option<u8>,
    styled: Option<grep_printer::PathPrinterBuilder>,
}

impl PathStyle {
    pub(crate) fn new(opts: &SearchOpts) -> PathStyle {
        PathStyle::with(
            opts.path_separator,
            opts.color,
            &opts.color_specs,
            opts.hyperlink.as_ref(),
        )
    }

    pub(crate) fn with(
        sep: Option<u8>,
        color: bool,
        user_specs: &[String],
        hyperlink: Option<&grep_printer::HyperlinkConfig>,
    ) -> PathStyle {
        let styled = color.then(|| {
            let mut b = grep_printer::PathPrinterBuilder::new();
            b.color_specs(color_specs(user_specs))
                .separator(sep)
                .terminator(b'\n');
            if let Some(h) = hyperlink {
                b.hyperlink(h.clone());
            }
            b
        });
        PathStyle { sep, styled }
    }

    /// True when a path renders as its plain bytes (no color, no
    /// separator rewrite): callers can then copy the bytes directly.
    pub(crate) fn is_plain(&self) -> bool {
        self.sep.is_none() && self.styled.is_none()
    }

    /// Append `rel` to `out` with no record terminator.
    pub(crate) fn write(&self, out: &mut Vec<u8>, rel: &Path) {
        match &self.styled {
            None => write_path(out, rel, self.sep),
            Some(b) => {
                // PathPrinter always ends a path with its one-byte
                // terminator; callers append their own record terminator
                // (`:`, NUL, or the searcher's line terminator), so drop it.
                if b.build(termcolor::Ansi::new(&mut *out)).write(rel).is_ok() {
                    out.pop();
                }
            }
        }
    }
}

/// Write `rel` honoring --path-separator (components rejoined by the
/// custom byte).
fn write_path(out: &mut Vec<u8>, rel: &Path, sep: Option<u8>) {
    match sep {
        None => out.extend_from_slice(rel.as_os_str().as_encoded_bytes()),
        Some(sep) => {
            let mut first = true;
            for c in rel.components() {
                if !first {
                    out.push(sep);
                }
                first = false;
                out.extend_from_slice(c.as_os_str().as_encoded_bytes());
            }
        }
    }
}

/// Stats for paths searched in count/-l modes: occurrences, matched
/// lines, one search, bytes from the file size (a decoded stream has no
/// stat()able size, so it reports 0 bytes searched).
fn file_stats(full: Option<&Path>, matched_lines: u64, matches: u64) -> grep_printer::Stats {
    let mut s = grep_printer::Stats::new();
    s.add_searches(1);
    s.add_searches_with_match(u64::from(matched_lines > 0));
    s.add_matched_lines(matched_lines);
    s.add_matches(matches);
    if let Some(full) = full {
        if let Ok(m) = std::fs::metadata(full) {
            s.add_bytes_searched(m.len());
        }
    }
    s
}

/// One finished file: its rendered output, whether it matched, and its
/// stats when the mode tracks them.
struct FileResult {
    buf: Vec<u8>,
    matched: bool,
    stats: Option<grep_printer::Stats>,
}

impl FileResult {
    fn empty() -> FileResult {
        FileResult {
            buf: Vec::new(),
            matched: false,
            stats: None,
        }
    }
}

/// Display form of an index-relative path: relative to the directory the
/// user ran from (`cwd_rel` inside the index tree). Paths under the cwd
/// just lose the prefix; paths reached through a `..` operand climb with
/// `..` components, so every printed path opens from the cwd.
pub(crate) fn display_rel<'a>(p: &'a Path, cwd_rel: &Path) -> std::borrow::Cow<'a, Path> {
    if cwd_rel.as_os_str().is_empty() {
        return std::borrow::Cow::Borrowed(p);
    }
    if let Ok(rest) = p.strip_prefix(cwd_rel) {
        return std::borrow::Cow::Borrowed(rest);
    }
    let mut cwd_it = cwd_rel.components().peekable();
    let mut p_it = p.components().peekable();
    while let (Some(a), Some(b)) = (cwd_it.peek(), p_it.peek()) {
        if a != b {
            break;
        }
        cwd_it.next();
        p_it.next();
    }
    let mut out = PathBuf::new();
    for _ in cwd_it {
        out.push("..");
    }
    for c in p_it {
        out.push(c.as_os_str());
    }
    std::borrow::Cow::Owned(out)
}

fn display_path<'a>(rel: &'a Path, opts: &SearchOpts) -> std::borrow::Cow<'a, Path> {
    // Paths print relative to the user's cwd, not the index root.
    display_rel(rel, &opts.display_prefix)
}

fn search_one<M: Matcher>(
    state: &mut WorkerState,
    matcher: &M,
    root: &Path,
    rel: &Path,
    opts: &SearchOpts,
) -> anyhow::Result<FileResult> {
    let full = root.join(rel);
    let display = display_path(rel, opts);
    let display: &Path = &display;
    let hay = Haystack::open(&full, rel, opts);
    let stat_path = if hay.is_stream() {
        None
    } else {
        Some(full.as_path())
    };
    let searcher = &mut state.searcher;
    if opts.quiet {
        // -q: nothing is written; the exit code carries the answer. Under
        // --json rg still emits the closing summary, so each file runs
        // through the JSON sink into a scratch buffer to harvest stats.
        if opts.json {
            let mut scratch = Vec::new();
            let mut printer = grep_printer::JSONBuilder::new().build(&mut scratch);
            let mut sink = printer.sink_with_path(matcher, display);
            hay.search(searcher, matcher, &mut sink)?;
            // -q printed nothing: bytes_printed stays 0, like rg's summary.
            let s = sink.stats();
            let mut stats = grep_printer::Stats::new();
            stats.add_searches(s.searches());
            stats.add_searches_with_match(s.searches_with_match());
            stats.add_bytes_searched(s.bytes_searched());
            stats.add_matched_lines(s.matched_lines());
            stats.add_matches(s.matches());
            return Ok(FileResult {
                buf: Vec::new(),
                matched: sink.has_match(),
                stats: Some(stats),
            });
        }
        let mut sink = FoundSink(false);
        hay.search(searcher, matcher, &mut sink)?;
        return Ok(FileResult {
            buf: Vec::new(),
            matched: sink.0,
            stats: None,
        });
    }
    if opts.count || opts.count_matches {
        let mut sink = CountSink::new(matcher, opts);
        hay.search(searcher, matcher, &mut sink)?;
        let stats = opts
            .stats
            .then(|| file_stats(stat_path, sink.count, sink.occurrences));
        if sink.count > 0 || opts.include_zero {
            // `path` + field terminator (`:` normally, NUL under --null) +
            // count; bare `N` when the path is suppressed. The record ends
            // with the line terminator (CRLF under --crlf, NUL under
            // --null-data), exactly like rg's summary printer.
            let mut line = Vec::with_capacity(display.as_os_str().len() + 12);
            if opts.with_filename {
                state.paths.write(&mut line, display);
                line.push(opts.path_terminator.unwrap_or(b':'));
            }
            line.extend_from_slice(sink.count.to_string().as_bytes());
            line.extend_from_slice(opts.line_term());
            return Ok(FileResult {
                buf: line,
                matched: sink.count > 0,
                stats,
            });
        }
        return Ok(FileResult {
            buf: Vec::new(),
            matched: false,
            stats,
        });
    }
    if opts.files_with_matches || opts.files_without_match {
        let (matched, stats) = if opts.stats {
            // --stats needs real counts: run the count sink instead of
            // the early-exit one.
            let mut sink = CountSink {
                matcher,
                occurrences_mode: true,
                count: 0,
                occurrences: 0,
                count_occurrences: false,
            };
            hay.search(searcher, matcher, &mut sink)?;
            (
                sink.count > 0,
                Some(file_stats(stat_path, sink.count, sink.occurrences)),
            )
        } else {
            let mut sink = FoundSink(false);
            hay.search(searcher, matcher, &mut sink)?;
            (sink.0, None)
        };
        let mut buf = Vec::new();
        if matched && opts.files_with_matches {
            state.paths.write(&mut buf, display);
            opts.push_record_end(&mut buf);
        }
        return Ok(FileResult {
            buf,
            matched,
            stats,
        });
    }
    let mut buf = Vec::new();
    let matched;
    let stats;
    if opts.json {
        let mut printer = grep_printer::JSONBuilder::new().build(&mut buf);
        let mut sink = printer.sink_with_path(matcher, display);
        hay.search(searcher, matcher, &mut sink)?;
        matched = sink.has_match();
        stats = Some(sink.stats().clone());
    } else if opts.color {
        let mut printer = state.printer.build(termcolor::Ansi::new(&mut buf));
        let mut sink = printer.sink_with_path(matcher, display);
        hay.search(searcher, matcher, &mut sink)?;
        matched = sink.has_match();
        stats = sink.stats().cloned();
    } else {
        let mut printer = state.printer.build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, display);
        hay.search(searcher, matcher, &mut sink)?;
        matched = sink.has_match();
        stats = sink.stats().cloned();
    }
    Ok(FileResult {
        buf,
        matched,
        stats,
    })
}

/// Run `search_one`, turning a per-file error into rg's warn-and-continue
/// behavior: report on stderr (unless --no-messages), treat as unmatched.
fn search_one_or_warn<M: Matcher>(
    state: &mut WorkerState,
    matcher: &M,
    root: &Path,
    rel: &Path,
    opts: &SearchOpts,
) -> FileResult {
    match search_one(state, matcher, root, rel, opts) {
        Ok(r) => r,
        Err(e) => {
            if !opts.no_messages {
                eprintln!("glep: {}: {}", rel.display(), e);
            }
            FileResult::empty()
        }
    }
}

/// One completed file: the file's index plus its result.
struct Done {
    idx: usize,
    res: FileResult,
}

/// Bounded-window coordination between workers and the printer. The
/// printer's ring is finite (ring capacity entries); without backpressure,
/// workers racing ahead of in-order printing can overflow it (the channel
/// capacity alone does NOT bound the lead: the printer continuously drains
/// the channel into the ring). Workers therefore wait before SEARCHING an
/// index that is a full ring-capacity ahead of the print cursor, and the
/// printer notifies after every advance. The worker holding `next` itself
/// never waits, so progress is guaranteed.
struct WindowState {
    next: AtomicUsize,
    room: Condvar,
    lock: Mutex<()>,
}

/// Flush the coalescing buffer once it passes this size.
const PRINT_BATCH: usize = 256 * 1024;

/// In-order output state, shared by the on-thread small path and the
/// printer thread so byte output cannot drift between them. Results are
/// emitted strictly in file-index order; `file_separator` is rg's
/// separator between files' output.
struct Printer {
    next: usize,
    ring: Vec<Option<FileResult>>,
    ring_mask: usize,
    total: usize,
    /// rg's file separator, line terminator included: a bare line break
    /// under --heading, the context separator plus a line break in
    /// context mode (-A/-B/-C, not --passthru), nothing otherwise or under
    /// --no-context-separator. Only the standard output mode has one.
    file_separator: Option<Vec<u8>>,
    line_buffered: bool,
    printed_any: bool,
    found: bool,
    total_stats: grep_printer::Stats,
    /// --files-without-match: which file indices matched (the complement
    /// over the universe prints after the search).
    matched_idx: Option<Vec<bool>>,
    /// Write coalescing buffer: a kernel-scale every-file search issues
    /// one write syscall per file (~86k syscalls, ~150ms). Batching into
    /// 256KB flushes cuts that to a few hundred with byte-identical
    /// output (ordering is the printer's own; nothing else interleaves
    /// stdout).
    out_buf: Vec<u8>,
}

impl Printer {
    fn new(opts: &SearchOpts, ring_cap: usize, total: usize) -> Printer {
        let cap = ring_cap.max(1).next_power_of_two();
        let standard = !opts.files_with_matches
            && !opts.files_without_match
            && !opts.json
            && !opts.count
            && !opts.count_matches
            && !opts.quiet;
        let sep: Option<&[u8]> = if !standard {
            None
        } else if opts.heading {
            Some(b"")
        } else if (opts.before > 0 || opts.after > 0) && !opts.passthru {
            opts.context_separator.as_deref()
        } else {
            None
        };
        let file_separator = sep.map(|s| [s, opts.line_term()].concat());
        Printer {
            next: 0,
            ring: (0..cap).map(|_| None).collect(),
            ring_mask: cap - 1,
            total,
            file_separator,
            line_buffered: opts.line_buffered,
            printed_any: false,
            found: false,
            total_stats: grep_printer::Stats::new(),
            matched_idx: opts.files_without_match.then(|| vec![false; total]),
            out_buf: Vec::with_capacity(if total > SMALL_WORKLOAD {
                PRINT_BATCH + 64 * 1024
            } else {
                0
            }),
        }
    }

    fn advance_window(&self, window: &WindowState) {
        window.next.store(self.next, Ordering::Release);
        let _guard = window.lock.lock().unwrap();
        window.room.notify_all();
    }

    fn write_bytes(
        &mut self,
        bytes: &[u8],
        out: &mut (dyn std::io::Write + Send),
    ) -> std::io::Result<()> {
        if self.line_buffered {
            out.write_all(bytes)?;
            return out.flush();
        }
        self.out_buf.extend_from_slice(bytes);
        if self.out_buf.len() >= PRINT_BATCH {
            out.write_all(&self.out_buf)?;
            self.out_buf.clear();
        }
        Ok(())
    }

    fn flush_batch(&mut self, out: &mut (dyn std::io::Write + Send)) -> std::io::Result<()> {
        if !self.out_buf.is_empty() {
            out.write_all(&self.out_buf)?;
            self.out_buf.clear();
        }
        Ok(())
    }

    fn stash_then_drain(
        &mut self,
        d: Done,
        out: &mut (dyn std::io::Write + Send),
    ) -> anyhow::Result<()> {
        let slot = d.idx & self.ring_mask;
        if self.ring[slot].is_some() {
            // release-mode tripwire for the ring-capacity invariant
            eprintln!(
                "GLEP BUG: ring slot {slot} (idx {}) overwritten; next={} mask={}",
                d.idx, self.next, self.ring_mask
            );
            std::process::abort();
        }
        self.ring[slot] = Some(d.res);
        self.drain(out)
    }

    /// Emit every result that is now consecutively available, in index
    /// order. Called after each stash and once more at channel close.
    fn drain(&mut self, out: &mut (dyn std::io::Write + Send)) -> anyhow::Result<()> {
        while self.next < self.total {
            let res = match self.ring[self.next & self.ring_mask].take() {
                Some(r) => r,
                None => break,
            };
            self.emit(res, out)?;
        }
        Ok(())
    }

    fn emit(
        &mut self,
        res: FileResult,
        out: &mut (dyn std::io::Write + Send),
    ) -> anyhow::Result<()> {
        let idx = self.next;
        self.next += 1;
        let FileResult {
            buf,
            matched,
            stats,
        } = res;
        if let Some(s) = &stats {
            merge_stats(&mut self.total_stats, s);
        }
        if let Some(m) = &mut self.matched_idx {
            m[idx] = matched;
        }
        if matched {
            self.found = true;
        }
        // Write any produced output, not just matches: --passthru emits
        // every line including non-matching ones, and --include-zero -c
        // emits `path:0` lines for searched files.
        if buf.is_empty() {
            return Ok(());
        }
        if self.printed_any {
            if let Some(sep) = &self.file_separator {
                if self.line_buffered {
                    out.write_all(sep)?;
                } else {
                    // Coalesced with the file's own bytes just below.
                    self.out_buf.extend_from_slice(sep);
                }
            }
        }
        self.write_bytes(&buf, out)?;
        self.printed_any = true;
        Ok(())
    }
}

fn write_summary(
    out: &mut (dyn std::io::Write + Send),
    total_stats: &mut grep_printer::Stats,
    elapsed: Duration,
) -> anyhow::Result<()> {
    // Per design: both `elapsed_total` and `stats.elapsed` are filled
    // from this single measured wall-clock duration for the whole run,
    // rather than trying to reproduce rg's internal split between
    // "time summing individual per-file searches" (its `stats.elapsed`)
    // and "total process wall time" (its `elapsed_total`). Both fields
    // are masked out of tests/json_parity.rs's comparison, so this
    // simplification is safe; `merge_stats` above deliberately never
    // touches `elapsed`, so this is the only place it's set.
    total_stats.add_elapsed(elapsed);
    let event = SummaryEvent {
        kind: "summary",
        data: SummaryData {
            elapsed_total: NiceDuration::from(elapsed),
            stats: total_stats.clone(),
        },
    };
    writeln!(out, "{}", serde_json::to_string(&event)?)?;
    Ok(())
}

/// Thread-safe stdout adapter: `StdoutLock` guards are not `Send`, but
/// `run` hands the writer to the printer thread. `Stdout` itself is
/// `Sync`; locking per write call is fine here because the pipeline
/// already coalesces output into large batched writes.
pub struct StdoutOut;

impl std::io::Write for StdoutOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().lock().write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        std::io::stdout().lock().write_all(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().lock().flush()
    }
}

/// Search `files` (relative paths, pre-sorted) under `root`. Prints results
/// in input order for determinism. Returns true if anything matched (for
/// --files-without-match: if any file was listed).
///
/// `universe` is --files-without-match's domain: the full live, filtered
/// file set. Files that were narrowed out by the index can't contain the
/// pattern, so they are "without match" by construction and print
/// without being searched; the index still narrows what is searched.
pub fn run(
    pattern: &str,
    root: &Path,
    files: &[PathBuf],
    universe: Option<&[PathBuf]>,
    opts: &SearchOpts,
    out: &mut (dyn std::io::Write + Send),
) -> anyhow::Result<bool> {
    // Two monomorphized paths, one per engine. Sinks and the searcher are
    // generic over Matcher; only the builder differs. Building the matcher
    // up front also validates the pattern before any I/O.
    if opts.pcre2 {
        let matcher = build_pcre2_matcher(pattern, opts)?;
        return run_impl(&matcher, root, files, universe, opts, out);
    }
    let matcher = match build_matcher(pattern, opts) {
        Ok(m) => m,
        // --engine auto: rg's hybrid mode falls back to PCRE2 whenever the
        // default engine rejects the pattern, and reports the default
        // engine's error when PCRE2 rejects it too.
        Err(e) if opts.engine_auto => match build_pcre2_matcher(pattern, opts) {
            Ok(m) => return run_impl(&m, root, files, universe, opts, out),
            Err(_) => return Err(e),
        },
        Err(e) => return Err(e),
    };
    run_impl(&matcher, root, files, universe, opts, out)
}

/// Parallel design (v2): workers pull file indices off a shared atomic
/// cursor, no per-chunk barriers like the old `chunks(128)` loop, so one
/// slow file never stalls the whole fleet. A dedicated printer thread
/// receives results over a bounded channel and emits them strictly in
/// index order (a classic in-order completion queue), so stdout stays
/// byte-identical to `rg --sort path`-style deterministic output while the
/// search itself runs at rg's unordered parallel throughput. No worker
/// ever holds a lock while writing to stdout. Each worker owns one
/// reusable `Searcher` (buffers included), mirroring ripgrep's per-thread
/// SearchWorker reuse.
fn run_impl<M: Matcher + Sync>(
    matcher: &M,
    root: &Path,
    files: &[PathBuf],
    universe: Option<&[PathBuf]>,
    opts: &SearchOpts,
    out: &mut (dyn std::io::Write + Send),
) -> anyhow::Result<bool> {
    // Times the whole run, used for the --json summary event's
    // `elapsed_total` (and, per design, `stats.elapsed` too: see
    // write_summary) and the --stats block.
    let start = Instant::now();
    // Resolve the -E label up front so a bad label errors before any I/O
    // (the reference exits 2 on an unknown encoding).
    let encoding = match &opts.encoding {
        Some(label) => {
            Some(grep_searcher::Encoding::new(label).map_err(|e| anyhow::anyhow!("{label}: {e}"))?)
        }
        None => None,
    };

    let mut printer = if files.len() <= SMALL_WORKLOAD {
        // Small workloads: stay on this thread, one reusable searcher,
        // zero synchronization. This is the agent-small-repo fast path.
        let mut state = WorkerState::new(opts, encoding.as_ref());
        let mut printer = Printer::new(opts, files.len(), files.len());
        for rel in files {
            let res = search_one_or_warn(&mut state, matcher, root, rel, opts);
            let stop = opts.quiet && res.matched;
            printer.emit(res, out)?;
            if stop {
                // -q: the first match settles the exit code.
                break;
            }
        }
        printer.flush_batch(out)?;
        printer
    } else {
        run_pool(matcher, root, files, opts, encoding.as_ref(), out)?
    };

    finish(&mut printer, files, universe, opts, out, start)
}

/// The worker pool + in-order printer thread for workloads larger than
/// `SMALL_WORKLOAD`. Returns the printer's final state (found flag,
/// merged stats, matched indices).
fn run_pool<M: Matcher + Sync>(
    matcher: &M,
    root: &Path,
    files: &[PathBuf],
    opts: &SearchOpts,
    encoding: Option<&grep_searcher::Encoding>,
    out: &mut (dyn std::io::Write + Send),
) -> anyhow::Result<Printer> {
    let parallelism = match opts.threads {
        Some(n) if n > 0 => n,
        _ => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    };
    let workers = parallelism.min(files.len()).max(1);
    // The channel capacity is the memory bound: at most this many file
    // output buffers (plus one per worker in flight) exist at once.
    let window = (workers * 16).max(64);
    let (tx, rx) = sync_channel::<Done>(window);
    // Ring capacity (mirrors Printer::new's computation) is the lead
    // bound the workers wait against.
    let ring_cap = (window + workers + 1).next_power_of_two();
    let window_state = WindowState {
        next: AtomicUsize::new(0),
        room: Condvar::new(),
        lock: Mutex::new(()),
    };
    // -q: set by the first match; workers stop claiming new files.
    let stop = AtomicBool::new(false);

    // Declared outside the scope so worker borrows outlive it.
    let cursor = AtomicUsize::new(0);
    let mut printer_result = None;
    std::thread::scope(|scope| -> anyhow::Result<()> {
        // Printer thread: the only writer to `out`; workers are free to
        // keep searching while it prints.
        let printer_out = &mut *out;
        let mut printer = Printer::new(opts, ring_cap, files.len());
        let printer_window = &window_state;
        let printer_handle = scope.spawn(move || -> anyhow::Result<Printer> {
            run_printer(rx, &mut printer, printer_out, printer_window)?;
            Ok(printer)
        });
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let mut state = WorkerState::new(opts, encoding);
            let tx = tx.clone();
            let cursor = &cursor;
            let window = &window_state;
            let stop = &stop;
            handles.push(scope.spawn(move || {
                worker(
                    cursor, &tx, &mut state, matcher, root, files, opts, window, ring_cap, stop,
                );
            }));
        }
        for h in handles {
            h.join().expect("search worker panicked");
        }
        // All workers are done (their sender clones dropped); drop OUR
        // sender so the printer's recv loop terminates after draining,
        // then collect the printer's final state.
        drop(tx);
        printer_result = Some(printer_handle.join().expect("printer thread panicked")?);
        Ok(())
    })?;
    Ok(printer_result.expect("printer state collected"))
}

/// Worker loop: claim indices from the shared cursor, search each file with
/// the thread-local reusable state, and send the finished result to the
/// printer thread. Errors from individual files keep rg's behavior: warn on
/// stderr, treat as unmatched, keep going (and still send the result so
/// in-order printing never stalls). Every claimed index is always sent,
/// even after a -q stop, so the printer's cursor can reach any index a
/// waiting worker holds.
#[allow(clippy::too_many_arguments)]
fn worker<M: Matcher + Sync>(
    cursor: &AtomicUsize,
    tx: &SyncSender<Done>,
    state: &mut WorkerState,
    matcher: &M,
    root: &Path,
    files: &[PathBuf],
    opts: &SearchOpts,
    window: &WindowState,
    window_cap: usize,
    stop: &AtomicBool,
) {
    loop {
        let idx = cursor.fetch_add(1, Ordering::Relaxed);
        if idx >= files.len() {
            return;
        }
        // Bounded-window backpressure: hold off searching until this
        // index falls within a ring-capacity of the print cursor. Waiting
        // BEFORE the search (not before the send) means blocked workers
        // hold no result at all, keeping the invariant exactly one-sided.
        {
            let mut guard = window.lock.lock().unwrap();
            while idx >= window.next.load(Ordering::Acquire) + window_cap {
                guard = window.room.wait(guard).unwrap();
            }
        }
        let res = if stop.load(Ordering::Relaxed) {
            FileResult::empty()
        } else {
            search_one_or_warn(state, matcher, root, &files[idx], opts)
        };
        if opts.quiet && res.matched {
            stop.store(true, Ordering::Relaxed);
            // No new claims: everything below the cursor is already
            // claimed and will be sent by its owner.
            cursor.fetch_max(files.len(), Ordering::Relaxed);
        }
        if tx.send(Done { idx, res }).is_err() {
            return; // printer gone: nothing left to do
        }
    }
}

/// Printer loop: receive results until the channel closes (all senders
/// dropped), then drain anything still buffered so no tail results are
/// lost, and emit everything strictly in index order.
fn run_printer(
    rx: Receiver<Done>,
    printer: &mut Printer,
    out: &mut (dyn std::io::Write + Send),
    window: &WindowState,
) -> anyhow::Result<()> {
    while let Ok(d) = rx.recv() {
        printer.stash_then_drain(d, out)?;
        printer.advance_window(window);
    }
    printer.drain(out)?;
    printer.advance_window(window);
    printer.flush_batch(out)?;
    Ok(())
}

/// Everything printed after the per-file results: the
/// --files-without-match complement, the --json summary event, and the
/// --stats block.
fn finish(
    printer: &mut Printer,
    files: &[PathBuf],
    universe: Option<&[PathBuf]>,
    opts: &SearchOpts,
    out: &mut (dyn std::io::Write + Send),
    start: Instant,
) -> anyhow::Result<bool> {
    let mut found = printer.found;
    if opts.files_without_match {
        found = false;
        let matched: std::collections::HashSet<&Path> = printer
            .matched_idx
            .as_ref()
            .map(|m| {
                files
                    .iter()
                    .zip(m.iter())
                    .filter(|(_, &hit)| hit)
                    .map(|(p, _)| p.as_path())
                    .collect()
            })
            .unwrap_or_default();
        let paths = PathStyle::new(opts);
        let mut buf = Vec::new();
        for p in universe.unwrap_or(files) {
            if !matched.contains(p.as_path()) {
                found = true;
                paths.write(&mut buf, &display_path(p, opts));
                opts.push_record_end(&mut buf);
            }
        }
        out.write_all(&buf)?;
    }
    if opts.json {
        write_summary(out, &mut printer.total_stats, start.elapsed())?;
    } else if opts.stats && !opts.quiet {
        // rg's stats block: a blank line then fixed-order counters.
        // `bytes_printed` is whatever the printer tracked (standard mode);
        // count/-l modes report 0 there like rg's non-printer paths.
        let elapsed = start.elapsed();
        let s = &printer.total_stats;
        writeln!(
            out,
            "\n{} matches\n{} matched lines\n{} files contained matches\n{} files searched\n{} bytes printed\n{} bytes searched\n{:.6} seconds spent searching\n{:.6} seconds total",
            s.matches(),
            s.matched_lines(),
            s.searches_with_match(),
            s.searches(),
            s.bytes_printed(),
            s.bytes_searched(),
            elapsed.as_secs_f64(),
            elapsed.as_secs_f64()
        )?;
    }
    out.flush()?;
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
        SearchOpts::default()
    }

    fn run_str(pattern: &str, root: &Path, files: &[PathBuf], o: &SearchOpts) -> (bool, String) {
        let mut out = Vec::new();
        let found = run(pattern, root, files, None, o, &mut out).unwrap();
        (found, String::from_utf8(out).unwrap())
    }

    #[test]
    fn default_output_is_path_line_text() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        let (found, out) = run_str("hello", dir.path(), &files, &opts());
        assert!(found);
        assert_eq!(out, "a.txt:1:one hello\na.txt:3:three hello\n");
    }

    #[test]
    fn files_with_matches_prints_paths_once() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        let mut o = opts();
        o.files_with_matches = true;
        let (found, out) = run_str("hello", dir.path(), &files, &o);
        assert!(found);
        assert_eq!(out, "a.txt\n");
    }

    #[test]
    fn no_match_returns_false() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt")];
        let (found, out) = run_str("absent_zz", dir.path(), &files, &opts());
        assert!(!found);
        assert!(out.is_empty());
    }

    #[test]
    fn case_insensitive_matches() {
        let dir = corpus();
        let files = vec![PathBuf::from("a.txt")];
        let mut o = opts();
        o.case_insensitive = true;
        let (found, _) = run_str("HELLO", dir.path(), &files, &o);
        assert!(found);
    }

    /// A corpus strictly larger than SMALL_WORKLOAD so `run` takes the
    /// worker-pool path. Every third file matches `needle`.
    fn big_corpus(n: usize) -> (tempfile::TempDir, Vec<PathBuf>) {
        assert!(n > SMALL_WORKLOAD);
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for i in 0..n {
            let name = format!("f{:03}.txt", i);
            let content = if i % 3 == 0 {
                format!("line one\nneedle here {}\nline three\n", i)
            } else {
                format!("file {} has no matches\n", i)
            };
            std::fs::write(dir.path().join(&name), content).unwrap();
            files.push(PathBuf::from(name));
        }
        (dir, files)
    }

    /// The parallel path (files.len() > SMALL_WORKLOAD) must produce
    /// byte-identical output to the sequential path: build 100 files with
    /// a known match pattern and compare against the expected ordered bytes.
    #[test]
    fn parallel_output_matches_expected_order() {
        let (dir, files) = big_corpus(100);
        let mut expect = String::new();
        for (i, f) in files.iter().enumerate() {
            if i % 3 == 0 {
                expect.push_str(&format!("{}:2:needle here {}\n", f.display(), i));
            }
        }
        let (found, out) = run_str("needle", dir.path(), &files, &opts());
        assert!(found);
        assert_eq!(out, expect);
    }

    /// The same 100-file search through a 2-worker pool (-j 2) and through
    /// one worker: identical bytes regardless of the worker count.
    #[test]
    fn parallel_output_independent_of_thread_count() {
        let (dir, files) = big_corpus(100);
        let mut o1 = opts();
        o1.threads = Some(1);
        let mut o2 = opts();
        o2.threads = Some(2);
        let (_, a) = run_str("needle", dir.path(), &files, &o1);
        let (_, b) = run_str("needle", dir.path(), &files, &o2);
        let (_, c) = run_str("needle", dir.path(), &files, &opts());
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// -l through the parallel path must print one path per matched file,
    /// in order.
    #[test]
    fn parallel_files_with_matches_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        let mut expect = String::new();
        for i in 0..80 {
            let name = format!("h{:03}.txt", i);
            let content = if i % 7 == 0 { "has needle\n" } else { "nope\n" };
            std::fs::write(dir.path().join(&name), content).unwrap();
            files.push(PathBuf::from(&name));
            if i % 7 == 0 {
                expect.push_str(&name);
                expect.push('\n');
            }
        }
        let mut o = opts();
        o.files_with_matches = true;
        let (found, out) = run_str("needle", dir.path(), &files, &o);
        assert!(found);
        assert_eq!(out, expect);
    }

    /// -c through the parallel path: per-file counts, ordered.
    #[test]
    fn parallel_count_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        let mut expect = String::new();
        for i in 0..64 {
            let name = format!("k{:03}.txt", i);
            let n = i % 4; // 0..=3 matches
            let content: String = std::iter::repeat_n("needle line\n", n).collect();
            std::fs::write(dir.path().join(&name), content).unwrap();
            files.push(PathBuf::from(&name));
            if n > 0 {
                expect.push_str(&format!("{}:{}\n", name, n));
            }
        }
        let mut o = opts();
        o.count = true;
        let (found, out) = run_str("needle", dir.path(), &files, &o);
        assert!(found);
        assert_eq!(out, expect);
    }

    /// --files-without-match through the parallel path: the complement,
    /// in input order, and exit status true when anything was listed.
    #[test]
    fn parallel_files_without_match_ordered() {
        let (dir, files) = big_corpus(70);
        let mut o = opts();
        o.files_without_match = true;
        let (found, out) = run_str("needle", dir.path(), &files, &o);
        assert!(found);
        let expect: String = files
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 3 != 0)
            .map(|(_, f)| format!("{}\n", f.display()))
            .collect();
        assert_eq!(out, expect);
    }

    /// -q through the parallel path: nothing printed, a match found, and
    /// the run stops early without deadlocking the in-order printer.
    #[test]
    fn parallel_quiet_prints_nothing_and_finds() {
        let (dir, files) = big_corpus(200);
        let mut o = opts();
        o.quiet = true;
        let (found, out) = run_str("needle", dir.path(), &files, &o);
        assert!(found);
        assert!(out.is_empty());
        let (found, out) = run_str("zz_absent_zz", dir.path(), &files, &o);
        assert!(!found);
        assert!(out.is_empty());
    }

    /// --json -q through the parallel path: exactly one line, the summary.
    #[test]
    fn parallel_quiet_json_emits_only_summary() {
        let (dir, files) = big_corpus(90);
        let mut o = opts();
        o.quiet = true;
        o.json = true;
        let (found, out) = run_str("needle", dir.path(), &files, &o);
        assert!(found);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"type\":\"summary\""));
    }

    /// --heading through the parallel path: blank line between groups.
    #[test]
    fn parallel_heading_groups() {
        let (dir, files) = big_corpus(40);
        let mut o = opts();
        o.heading = true;
        let (_, out) = run_str("needle", dir.path(), &files, &o);
        let mut expect = String::new();
        for (i, f) in files.iter().enumerate().filter(|(i, _)| i % 3 == 0) {
            if !expect.is_empty() {
                expect.push('\n');
            }
            expect.push_str(&format!("{}\n2:needle here {}\n", f.display(), i));
        }
        assert_eq!(out, expect);
    }

    /// --stats through the parallel path: counters summed over every file.
    #[test]
    fn parallel_stats_totals() {
        let (dir, files) = big_corpus(99);
        let mut o = opts();
        o.stats = true;
        let (_, out) = run_str("needle", dir.path(), &files, &o);
        assert!(
            out.contains(
                "\n33 matches\n33 matched lines\n33 files contained matches\n99 files searched\n"
            ),
            "{out}"
        );
    }

    /// Context mode through the parallel path must reproduce the exact
    /// `rg --sort path -n -C1` byte stream (including `-path-line-`
    /// context lines and `--` separators between files).
    #[test]
    fn parallel_context_matches_rg_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for i in 0..60 {
            let name = format!("g{:02}.txt", i);
            let content = if i % 5 == 0 {
                format!("before\nneedle {}\nafter\n", i)
            } else {
                format!("no match {}\nnothing\n", i)
            };
            std::fs::write(dir.path().join(&name), content).unwrap();
            files.push(PathBuf::from(name));
        }
        let mut o = opts();
        o.before = 1;
        o.after = 1;
        let mut out = Vec::new();
        assert!(run("needle", dir.path(), &files, None, &o, &mut out).unwrap());

        // Ground truth: real rg with sorted output in the same tree.
        let rg = std::process::Command::new("rg")
            .arg("-n")
            .arg("--sort")
            .arg("path")
            .arg("-C")
            .arg("1")
            .arg("needle")
            .current_dir(dir.path())
            .output()
            .expect("rg must be installed for parity tests");
        assert!(
            rg.status.success(),
            "rg failed: {}",
            String::from_utf8_lossy(&rg.stderr)
        );
        assert_eq!(
            out, rg.stdout,
            "glep parallel -C1 output must match rg --sort path -C1"
        );
    }
}
