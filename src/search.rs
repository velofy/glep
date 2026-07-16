use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

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
) -> anyhow::Result<(Vec<u8>, bool)> {
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
            ));
        }
        return Ok((Vec::new(), false));
    }
    if opts.files_with_matches {
        let mut sink = FoundSink(false);
        searcher.search_path(matcher, &full, &mut sink)?;
        return Ok((Vec::new(), sink.0));
    }
    let mut buf = Vec::new();
    let matched;
    if opts.json {
        let mut printer = grep_printer::JSONBuilder::new().build(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_path(matcher, &full, &mut sink)?;
        matched = sink.has_match();
    } else {
        let mut printer = grep_printer::StandardBuilder::new()
            .heading(false)
            .build_no_color(&mut buf);
        let mut sink = printer.sink_with_path(matcher, rel);
        searcher.search_path(matcher, &full, &mut sink)?;
        matched = sink.has_match();
    }
    Ok((buf, matched))
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
    // Build the matcher once up front; shared by reference across the rayon
    // closure (grep_regex::RegexMatcher is Sync). This also validates the
    // pattern before I/O, matching prior behavior.
    let matcher = build_matcher(pattern, opts)?;
    let mut found = false;
    let separate =
        (opts.before > 0 || opts.after > 0) && !opts.files_with_matches && !opts.json && !opts.count;
    let mut printed_any = false;
    let mut base = 0usize;
    for chunk in files.chunks(128) {
        let mut results: Vec<(usize, Vec<u8>, bool)> = chunk
            .par_iter()
            .enumerate()
            .map(|(i, rel)| match search_one(&matcher, root, rel, opts) {
                Ok((buf, matched)) => (i, buf, matched),
                Err(_) => (i, Vec::new(), false), // vanished/unreadable file: no matches
            })
            .collect();
        results.sort_by_key(|(i, _, _)| *i);
        for (i, buf, matched) in results {
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
