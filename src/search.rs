use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

pub struct SearchOpts {
    pub case_insensitive: bool,
    pub fixed: bool,
    pub files_with_matches: bool,
    pub context: usize,
    pub json: bool,
}

fn build_matcher(pattern: &str, opts: &SearchOpts) -> anyhow::Result<grep_regex::RegexMatcher> {
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(opts.case_insensitive);
    b.fixed_strings(opts.fixed);
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

fn search_one(
    matcher: &grep_regex::RegexMatcher,
    root: &Path,
    rel: &Path,
    opts: &SearchOpts,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(opts.context)
        .after_context(opts.context)
        .build();
    let full = root.join(rel);
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
    let separate = opts.context > 0 && !opts.files_with_matches && !opts.json;
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
            context: 0,
            json: false,
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
