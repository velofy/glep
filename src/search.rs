use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Condvar, Mutex};
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
                .map_err(std::io::Error::other)?;
            self.count += n.max(1);
        } else {
            self.count += 1;
        }
        Ok(true)
    }
}

/// Per-thread search state: one `Searcher` reused across every file this
/// worker touches. `grep_searcher::Searcher` holds its decode/line buffers
/// in RefCells precisely so a single instance can be reused across files;
/// rebuilding it per file (the old design) threw that reuse away and paid
/// a fresh allocation plus cold buffers per file. ripgrep itself clones
/// one SearchWorker per walker thread and reuses it for the whole run.
struct WorkerState {
    searcher: Searcher,
}

impl WorkerState {
    fn new(opts: &SearchOpts) -> WorkerState {
        // Line-number tracking is only needed by the printing sinks; rg
        // itself searches without line numbers unless output needs them.
        // -l/-c modes skip the bookkeeping entirely.
        let need_line_numbers = !opts.files_with_matches && !opts.count;
        let searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(need_line_numbers)
            .before_context(opts.before)
            .after_context(opts.after)
            .multi_line(opts.multiline)
            .build();
        WorkerState { searcher }
    }
}

/// One completed file: the file's index, its rendered output buffer,
/// whether it matched, and (json mode only) its per-file stats.
struct Done {
    idx: usize,
    buf: Vec<u8>,
    matched: bool,
    stats: Option<grep_printer::Stats>,
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

/// Print one finished result. Shared by the sequential and parallel paths
/// so byte output cannot drift between them. `printed_any` drives the
/// rg-compatible `--` separator between files in context modes.
fn emit_result(
    out: &mut (dyn std::io::Write + Send),
    buf: &[u8],
    matched: bool,
    stats: &Option<grep_printer::Stats>,
    separate: bool,
    printed_any: &mut bool,
    total_stats: &mut grep_printer::Stats,
) -> std::io::Result<()> {
    if let Some(s) = stats {
        merge_stats(total_stats, s);
    }
    if matched {
        if separate && *printed_any {
            writeln!(out, "--")?;
        }
        out.write_all(buf)?;
        *printed_any = true;
    }
    Ok(())
}

/// The bytes to search for one file: an mmap when mapping succeeded
/// (zero-copy: the searcher runs directly against page-cache pages, no
/// kernel->user buffer copy), else the path (streamed through the
/// searcher's line buffer). ripgrep's own macOS heuristic disables mmap
/// with a 2018 "should re-evaluate" note; we re-evaluated: on
/// page-cache-bound workloads (every-file searches) the copy elimination
/// is a significant win. SIGBUS on concurrent truncation is the accepted
/// risk, identical to rg's documented --mmap behavior on Linux.
enum Haystack {
    Map(memmap2::Mmap),
    Path(PathBuf),
}

impl Haystack {
    fn open(full: &Path) -> Haystack {
        match std::fs::File::open(full) {
            Ok(f) => match unsafe { memmap2::Mmap::map(&f) } {
                Ok(m) if !m.is_empty() => Haystack::Map(m),
                _ => Haystack::Path(full.to_path_buf()),
            },
            Err(_) => Haystack::Path(full.to_path_buf()),
        }
    }

    fn search<S: grep_searcher::Sink>(
        &self,
        searcher: &mut Searcher,
        matcher: &grep_regex::RegexMatcher,
        sink: &mut S,
    ) -> Result<(), S::Error> {
        match self {
            Haystack::Map(m) => searcher.search_slice(matcher, &m[..], sink),
            Haystack::Path(p) => searcher.search_path(matcher, p, sink),
        }
    }
}

fn search_one(
    state: &mut WorkerState,
    matcher: &grep_regex::RegexMatcher,
    root: &Path,
    rel: &Path,
    opts: &SearchOpts,
) -> anyhow::Result<(Vec<u8>, bool, Option<grep_printer::Stats>)> {
    let full = root.join(rel);
    let hay = Haystack::open(&full);
    if opts.count {
        let mut sink = CountSink {
            matcher,
            multiline: opts.multiline,
            count: 0,
        };
        hay.search(&mut state.searcher, matcher, &mut sink)?;
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
        hay.search(&mut state.searcher, matcher, &mut sink)?;
        if sink.0 {
            return Ok((format!("{}\n", rel.display()).into_bytes(), true, None));
        }
        return Ok((Vec::new(), false, None));
    }
    let mut buf = Vec::new();
    let matched;
    let mut stats = None;
    if opts.json {
        let mut printer = grep_printer::JSONBuilder::new().build(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        hay.search(&mut state.searcher, matcher, &mut sink)?;
        matched = sink.has_match();
        stats = Some(sink.stats().clone());
    } else {
        let mut printer = grep_printer::StandardBuilder::new()
            .heading(false)
            .build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        hay.search(&mut state.searcher, matcher, &mut sink)?;
        matched = sink.has_match();
    }
    Ok((buf, matched, stats))
}

/// Worker loop: claim indices from the shared cursor, search each file with
/// the thread-local reusable state, and send the finished result to the
/// printer thread. Errors from individual files keep rg's behavior: warn on
/// stderr, treat as unmatched, keep going (and still send the result so
/// in-order printing never stalls). The bounded channel blocks a worker
/// only when the printer is a full window behind, bounded memory, and it
/// never blocks the *other* workers.
fn worker(
    cursor: &AtomicUsize,
    tx: &SyncSender<Done>,
    state: &mut WorkerState,
    matcher: &grep_regex::RegexMatcher,
    root: &Path,
    files: &[PathBuf],
    opts: &SearchOpts,
    window: &WindowState,
    window_cap: usize,
) {
    loop {
        let idx = cursor.fetch_add(1, Ordering::Relaxed);
        if idx >= files.len() {
            return;
        }
        // Bounded-window backpressure: hold off searching until this
        // index falls within a ring-capacity of the print cursor. Waiting
        // BEFORE the search (not before the send) means blocked workers
        // hold no Done at all, keeping the invariant exactly one-sided.
        {
            let mut guard = window.lock.lock().unwrap();
            while idx >= window.next.load(Ordering::Acquire) + window_cap {
                guard = window.room.wait(guard).unwrap();
            }
        }
        let rel = &files[idx];
        let (buf, matched, stats) = match search_one(state, matcher, root, rel, opts) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("glep: {}: {}", rel.display(), e);
                (Vec::new(), false, None)
            }
        };
        if tx
            .send(Done {
                idx,
                buf,
                matched,
                stats,
            })
            .is_err()
        {
            return; // printer gone: nothing left to do
        }
    }
}

/// Printer-thread state: a bounded ring of finished-but-not-yet-printed
/// results plus the running cursor of the next index to emit. Ring safety:
/// at most `window + workers` results can exist outside the printer at
/// once (the bounded channel holds `window`; each worker holds one more
/// mid-send), so the ring is sized accordingly and a slot is only reused
/// after its occupant was printed.
struct Printer {
    next: usize,
    ring: Vec<Option<Done>>,
    ring_mask: usize,
    total: usize,
    separate: bool,
    printed_any: bool,
    found: bool,
    total_stats: grep_printer::Stats,
    /// Write coalescing buffer: a kernel-scale every-file search issues
    /// one write syscall per file (~86k syscalls, ~150ms). Batching into
    /// 256KB flushes cuts that to a few hundred with byte-identical
    /// output (ordering is the printer's own; nothing else interleaves
    /// stdout).
    out_buf: Vec<u8>,
}

/// Flush the coalescing buffer once it passes this size.
const PRINT_BATCH: usize = 256 * 1024;

impl Printer {
    fn advance_window(&self, window: &WindowState) {
        window.next.store(self.next, Ordering::Release);
        let _guard = window.lock.lock().unwrap();
        window.room.notify_all();
    }

    fn new(separate: bool, window: usize, workers: usize, total: usize) -> Printer {
        let cap = (window + workers + 1).next_power_of_two();
        Printer {
            next: 0,
            ring: (0..cap).map(|_| None).collect(),
            ring_mask: cap - 1,
            total,
            separate,
            printed_any: false,
            found: false,
            total_stats: grep_printer::Stats::new(),
            out_buf: Vec::with_capacity(PRINT_BATCH + 64 * 1024),
        }
    }

    fn write_bytes(
        &mut self,
        bytes: &[u8],
        out: &mut (dyn std::io::Write + Send),
    ) -> std::io::Result<()> {
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
        self.ring[slot] = Some(d);
        self.drain(out)
    }

    /// Emit every result that is now consecutively available, in index
    /// order. Called after each stash and once more at channel close.
    fn drain(&mut self, out: &mut (dyn std::io::Write + Send)) -> anyhow::Result<()> {
        while self.next < self.total {
            let d = match self.ring[self.next & self.ring_mask].take() {
                Some(d) => d,
                None => break,
            };
            self.emit(d, out)?;
        }
        Ok(())
    }

    fn emit(&mut self, d: Done, out: &mut (dyn std::io::Write + Send)) -> anyhow::Result<()> {
        let Done {
            idx,
            buf,
            matched,
            stats,
        } = d;
        debug_assert_eq!(idx, self.next, "printer must emit in order");
        self.next += 1;
        if matched {
            self.found = true;
        }
        if let Some(s) = &stats {
            merge_stats(&mut self.total_stats, s);
        }
        if matched {
            if self.separate && self.printed_any {
                self.write_bytes(b"--\n", out)?;
            }
            self.write_bytes(&buf, out)?;
            self.printed_any = true;
        }
        Ok(())
    }
}

fn finish_json(
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
/// `run` hands the writer to worker threads. `Stdout` itself is `Sync`;
/// locking per write call is fine here because the pipeline already emits
/// one buffered write per file.
pub struct StdoutOut;

impl std::io::Write for StdoutOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().lock().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().lock().flush()
    }
}

/// Search `files` (relative paths, pre-sorted) under `root`. Prints results
/// in input order for determinism. Returns true if anything matched.
///
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
pub fn run(
    pattern: &str,
    root: &Path,
    files: &[PathBuf],
    opts: &SearchOpts,
    out: &mut (dyn std::io::Write + Send),
) -> anyhow::Result<bool> {
    // Times the whole run, used for the --json summary event's
    // `elapsed_total` (and, per design, `stats.elapsed` too: see below).
    let start = Instant::now();
    // Build the matcher once up front; shared by reference across the
    // workers (grep_regex::RegexMatcher is Sync). This also validates the
    // pattern before I/O, matching prior behavior.
    let matcher = build_matcher(pattern, opts)?;
    let separate = (opts.before > 0 || opts.after > 0)
        && !opts.files_with_matches
        && !opts.json
        && !opts.count;

    if files.len() <= 32 {
        // Small workloads: stay on this thread, one reusable searcher,
        // zero synchronization. This is the agent-small-repo fast path.
        let mut state = WorkerState::new(opts);
        let mut found = false;
        let mut printed_any = false;
        let mut total_stats = grep_printer::Stats::new();
        for rel in files {
            let (buf, matched, stats) = match search_one(&mut state, &matcher, root, rel, opts) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("glep: {}: {}", rel.display(), e);
                    (Vec::new(), false, None)
                }
            };
            emit_result(
                out,
                &buf,
                matched,
                &stats,
                separate,
                &mut printed_any,
                &mut total_stats,
            )?;
            if matched {
                found = true;
            }
        }
        if opts.json {
            finish_json(out, &mut total_stats, start.elapsed())?;
        }
        return Ok(found);
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    // The channel capacity is the memory bound: at most this many file
    // output buffers (plus one per worker in flight) exist at once.
    let window = (workers * 16).max(64);
    let (tx, rx) = sync_channel::<Done>(window);
    // Ring capacity (mirrors Printer::new's computation) is the lead
    // bound the workers wait against.
    let ring_cap = (window + workers + 1).next_power_of_two();
    let window_state = std::sync::Arc::new(WindowState {
        next: AtomicUsize::new(0),
        room: Condvar::new(),
        lock: Mutex::new(()),
    });

    // Declared outside the scope so worker borrows outlive it.
    let cursor = AtomicUsize::new(0);
    let mut printer_result = None;
    std::thread::scope(|scope| -> anyhow::Result<()> {
        // Printer thread: the only writer to `out`; workers are free to
        // keep searching while it prints.
        let printer_out = &mut *out;
        let mut printer = Printer::new(separate, window, workers, files.len());
        let printer_window = window_state.clone();
        let printer_handle = scope.spawn(move || -> anyhow::Result<Printer> {
            run_printer(rx, &mut printer, printer_out, &printer_window)?;
            Ok(printer)
        });
        let cursor_ref = &cursor;
        let matcher_ref = &matcher;
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let mut state = WorkerState::new(opts);
            let tx = tx.clone();
            let window_ref = window_state.clone();
            handles.push(scope.spawn(move || {
                worker(
                    cursor_ref,
                    &tx,
                    &mut state,
                    matcher_ref,
                    root,
                    files,
                    opts,
                    &window_ref,
                    ring_cap,
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
    let mut printer = printer_result.expect("printer state collected");

    if opts.json {
        finish_json(out, &mut printer.total_stats, start.elapsed())?;
    }
    Ok(printer.found)
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

    /// The parallel path (files.len() > 32) must produce byte-identical
    /// output to the sequential path: build 100 files with a known match
    /// pattern and compare against the expected ordered bytes.
    #[test]
    fn parallel_output_matches_expected_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        let mut expect = String::new();
        for i in 0..100 {
            let name = format!("f{:03}.txt", i);
            let content = if i % 3 == 0 {
                format!("line one\nneedle here {}\nline three\n", i)
            } else {
                format!("file {} has no matches\n", i)
            };
            std::fs::write(dir.path().join(&name), content).unwrap();
            let expect_name = name.clone();
            files.push(PathBuf::from(name));
            if i % 3 == 0 {
                expect.push_str(&format!("{}:2:needle here {}\n", expect_name, i));
            }
        }
        let mut out = Vec::new();
        assert!(run("needle", dir.path(), &files, &opts(), &mut out).unwrap());
        assert_eq!(String::from_utf8(out).unwrap(), expect);
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
            let expect_name = name.clone();
            files.push(PathBuf::from(name));
            if i % 7 == 0 {
                expect.push_str(&expect_name);
                expect.push('\n');
            }
        }
        let mut o = opts();
        o.files_with_matches = true;
        let mut out = Vec::new();
        assert!(run("needle", dir.path(), &files, &o, &mut out).unwrap());
        assert_eq!(String::from_utf8(out).unwrap(), expect);
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
            let content: String = std::iter::repeat("needle line\n")
                .take(n)
                .collect::<Vec<_>>()
                .join("");
            std::fs::write(dir.path().join(&name), content).unwrap();
            let expect_name = name.clone();
            files.push(PathBuf::from(name));
            if n > 0 {
                expect.push_str(&format!("{}:{}\n", expect_name, n));
            }
        }
        let mut o = opts();
        o.count = true;
        let mut out = Vec::new();
        assert!(run("needle", dir.path(), &files, &o, &mut out).unwrap());
        assert_eq!(String::from_utf8(out).unwrap(), expect);
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
        assert!(run("needle", dir.path(), &files, &o, &mut out).unwrap());

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
