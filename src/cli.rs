use crate::index::Index;
use crate::index::manifest::{FLAG_SKIP_BINARY, FLAG_SKIP_TOO_LARGE};
use crate::timing::Timings;
use crate::{plan, search, walk};
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "glep", version, about = "Indexed grep + glob for AI agents")]
pub struct Args {
    /// Pattern, or the subcommands: index, status
    pub pattern: Option<String>,
    /// Restrict results to these subtrees
    pub paths: Vec<PathBuf>,
    /// Explicit pattern (use when the pattern is literally "index" or "status")
    #[arg(short = 'e', long = "regexp")]
    pub regexp: Option<String>,
    /// List files matching a glob instead of searching content
    #[arg(long)]
    pub files: bool,
    #[arg(short = 'i', long)]
    pub ignore_case: bool,
    #[arg(short = 'F', long)]
    pub fixed_strings: bool,
    #[arg(short = 'l', long, conflicts_with = "json")]
    pub files_with_matches: bool,
    #[arg(short = 'c', long = "count", conflicts_with_all = ["files_with_matches", "json"])]
    pub count: bool,
    /// Filter candidate files by glob (repeatable)
    #[arg(short = 'g', long = "glob")]
    pub globs: Vec<String>,
    /// Filter candidate files by type from the ignore crate's defaults (repeatable)
    #[arg(short = 't', long = "type")]
    pub types: Vec<String>,
    #[arg(short = 'C', long)]
    pub context: Option<usize>,
    #[arg(short = 'A', long = "after-context")]
    pub after_context: Option<usize>,
    #[arg(short = 'B', long = "before-context")]
    pub before_context: Option<usize>,
    #[arg(long)]
    pub json: bool,
    /// Allow matches to span multiple lines (patterns may contain \n)
    #[arg(short = 'U', long)]
    pub multiline: bool,
    /// Include hidden (dot-prefixed) files and directories, rg semantics.
    /// .git is always excluded regardless of this flag.
    #[arg(long)]
    pub hidden: bool,
    /// Search ignored files too (gitignore/.ignore/global excludes all
    /// bypassed), rg semantics. Implemented as a live scan that never
    /// opens, updates, or writes the index: ignored trees (node_modules,
    /// target, ...) must never enter the index, so this trades speed for
    /// that guarantee. .git/.glep are still always excluded.
    #[arg(long)]
    pub no_ignore: bool,
    /// Search binary files as if they were text (rg -a/--text)
    #[arg(short = 'a', long, overrides_with = "binary")]
    pub text: bool,
    /// Search binary files but report matches as a notice instead of
    /// printing matched lines (rg --binary)
    #[arg(long, overrides_with = "text")]
    pub binary: bool,
    /// Skip the index for this run: live gitignore-aware walk + scan of
    /// the whole discovered tree. Useful for one-off queries on huge
    /// trees or to sanity-check index freshness.
    #[arg(long)]
    pub no_index: bool,
    /// Show the column number of the first match per line (rg --column)
    #[arg(long)]
    pub column: bool,
    /// Show the byte offset of each printed line (rg -b/--byte-offset)
    #[arg(short = 'b', long)]
    pub byte_offset: bool,
    /// One line per match in path:line:column:text form (rg --vimgrep)
    #[arg(long)]
    pub vimgrep: bool,
    /// Trim leading whitespace from matched lines (rg --trim)
    #[arg(long)]
    pub trim: bool,
    /// Terminate printed paths with NUL instead of a separator (rg -0)
    #[arg(short = '0', long = "null")]
    pub null: bool,
    /// Replace the OS path separator in output paths; exactly one byte
    /// (rg --path-separator)
    #[arg(long, value_parser = parse_path_separator)]
    pub path_separator: Option<u8>,
    /// With -c, print `path:0` lines for searched files with no matches
    /// (rg --include-zero). Needs the full walked set, so the index plan
    /// degenerates to All when combined with -c.
    #[arg(long)]
    pub include_zero: bool,
    /// Only search files at most N levels below each path operand
    /// (rg --max-depth; a file operand itself is depth 0)
    #[arg(long, visible_alias = "maxdepth")]
    pub max_depth: Option<usize>,
    /// Worker thread count, 0 for auto (rg -j/--threads)
    #[arg(short = 'j', long = "threads")]
    pub threads: Option<usize>,
    /// Always print the file path with matches; the default unless a
    /// single file operand was given (rg -H/--with-filename)
    #[arg(short = 'H', long, overrides_with = "no_filename")]
    pub with_filename: bool,
    /// Never print the file path with matches (rg -I/--no-filename)
    #[arg(short = 'I', long, overrides_with = "with_filename")]
    pub no_filename: bool,
    /// Print the rg-style stats block after results (rg --stats)
    #[arg(long)]
    pub stats: bool,
    /// Skip the freshness sweep if the last one ran within this many seconds
    #[arg(long, default_value_t = 0)]
    pub ttl: u64,
    #[arg(long, default_value_t = 1_048_576)]
    pub max_filesize: u64,
}

fn parse_path_separator(s: &str) -> Result<u8, String> {
    if s.len() == 1 {
        Ok(s.as_bytes()[0])
    } else {
        Err(format!(
            "A path separator must be exactly one byte, but the given separator is {} bytes: {}",
            s.len(),
            s
        ))
    }
}

fn build_glob(g: &str) -> anyhow::Result<globset::GlobMatcher> {
    // gitignore semantics: a slash-free pattern matches at any depth;
    // once a pattern contains a slash, * must not cross separators.
    let pat = if g.contains('/') {
        g.to_string()
    } else {
        format!("**/{g}")
    };
    Ok(globset::GlobBuilder::new(&pat)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

/// Lexically join a base dir and a relative path, folding `.`/`..`
/// components. Returns None when the result escapes above `base`'s own
/// root — i.e. the normalized path would start with `..` (an indexed
/// search can never serve such a path; it is kept verbatim so existence
/// checks still fire and filters simply match nothing).
fn norm_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::from(base);
    for c in rel.components() {
        match c {
            std::path::Component::Normal(s) => out.push(s),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
        }
    }
    Some(out)
}

/// Discover the tree root: `GLEP_INDEX_PATH` (points at the index dir
/// itself; its parent is the root) wins; otherwise the nearest ancestor
/// of `cwd` containing a `.glep/` directory; otherwise `cwd` itself (the
/// index will be built there). Returns `(root, cwd_rel)` where `cwd_rel`
/// is `cwd` relative to `root` ("" when the same) — it is both the
/// implicit search scope and the display prefix to strip.
fn discover_index_root(cwd: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let cwd_canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if let Some(v) = std::env::var_os("GLEP_INDEX_PATH").filter(|v| !v.is_empty()) {
        let idx_dir = PathBuf::from(&v);
        let idx_dir = if idx_dir.is_absolute() {
            idx_dir
        } else {
            cwd.join(idx_dir)
        };
        anyhow::ensure!(
            idx_dir.is_dir(),
            "glep: {}: not an index directory (GLEP_INDEX_PATH)",
            idx_dir.display()
        );
        let idx_canon = std::fs::canonicalize(&idx_dir).unwrap_or(idx_dir);
        let root = idx_canon
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| idx_canon.clone());
        let cwd_rel = cwd_canon.strip_prefix(&root).unwrap_or(Path::new("")).to_path_buf();
        return Ok((root, cwd_rel));
    }
    let mut dir = Some(cwd_canon.as_path());
    while let Some(d) = dir {
        if d.join(".glep").is_dir() {
            return Ok((d.to_path_buf(), cwd_canon.strip_prefix(d).unwrap_or(Path::new("")).to_path_buf()));
        }
        dir = d.parent();
    }
    Ok((cwd_canon, PathBuf::new()))
}

fn normalize_path_filters(paths: &mut [PathBuf], root: &std::path::Path, cwd_rel: &Path) {
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for p in paths.iter_mut() {
        if p.is_absolute() {
            let canonical_p = std::fs::canonicalize(&*p).unwrap_or_else(|_| p.clone());
            if let Ok(rel) = canonical_p.strip_prefix(&canonical_root) {
                *p = rel.to_path_buf();
            }
        } else {
            *p = norm_join(cwd_rel, p).unwrap_or_else(|| p.clone());
        }
    }
}

/// Display form of an index-relative path: strip the cwd scope prefix so
/// output stays relative to the directory the user ran from.
fn display_path<'a>(p: &'a Path, cwd_rel: &Path) -> &'a Path {
    if cwd_rel.as_os_str().is_empty() {
        p
    } else {
        p.strip_prefix(cwd_rel).unwrap_or(p)
    }
}

/// Drop path filters that don't exist on disk, printing the rg-style
/// error for each. Returns true if any path was missing (final exit code
/// must be 2 regardless of matches).
fn report_missing_paths(paths: &[PathBuf], cwd: &Path, files_mode: bool) -> bool {
    let mut missing = false;
    for p in paths {
        if !cwd.join(p).exists() {
            missing = true;
            if files_mode {
                eprintln!(
                    "glep: {}: IO error for operation on {}: No such file or directory (os error 2)",
                    p.display(),
                    p.display()
                );
            } else {
                eprintln!("glep: {}: No such file or directory (os error 2)", p.display());
            }
        }
    }
    missing
}

fn apply_filters(files: &mut Vec<PathBuf>, args: &Args) -> anyhow::Result<()> {
    if !args.paths.is_empty() {
        files.retain(|f| args.paths.iter().any(|p| f.starts_with(p)));
    }
    if !args.globs.is_empty() {
        let matchers = args
            .globs
            .iter()
            .map(|g| build_glob(g))
            .collect::<anyhow::Result<Vec<_>>>()?;
        files.retain(|f| matchers.iter().any(|m| m.is_match(f)));
    }
    if !args.types.is_empty() {
        let mut tb = ignore::types::TypesBuilder::new();
        tb.add_defaults();
        for t in &args.types {
            tb.select(t);
        }
        let types = tb.build()?;
        files.retain(|f| types.matched(f, false).is_whitelist());
    }
    if let Some(maxd) = args.max_depth {
        // Depth is operand-relative: a file's depth is its component count
        // minus the containing operand's. With no operands the implicit
        // root is the operand, so depth is the whole component count.
        files.retain(|f| {
            let depth = f.components().count();
            if args.paths.is_empty() {
                depth <= maxd
            } else {
                args.paths.iter().any(|p| {
                    f.starts_with(p)
                        && depth.saturating_sub(p.components().count()) <= maxd
                })
            }
        });
    }
    Ok(())
}

/// Write a path honoring --path-separator (components rejoined by the
/// custom byte) followed by the path terminator: NUL under --null,
/// newline otherwise.
fn write_terminated_path(
    out: &mut dyn std::io::Write,
    rel: &Path,
    sep: Option<u8>,
    term: u8,
) -> std::io::Result<()> {
    if let Some(sep) = sep {
        let mut first = true;
        for c in rel.components() {
            if !first {
                out.write_all(&[sep])?;
            }
            first = false;
            out.write_all(c.as_os_str().as_encoded_bytes())?;
        }
    } else {
        write!(out, "{}", rel.display())?;
    }
    out.write_all(&[term])
}

/// `--no-ignore`: content mode and `--files` mode both bypass the index
/// entirely (no open, no update, no write, not even a read-only fallback
/// sweep) in favor of a live scan via `walk::sweep_unfiltered`, which
/// walks with every ignore source disabled. This is the escape hatch's
/// whole point: ignored trees must never enter the index, so the only
/// sound way to search them is to never touch the index at all for this
/// run. Slower than the indexed path (full walk + full scan every time,
/// same cost as `rg --no-ignore` itself), but that trade is deliberate.
///
/// `args.hidden` gates hidden files exactly as it does on the indexed
/// path; `sweep_unfiltered` does that gating itself (see its doc comment
/// in walk.rs), so the result is not re-filtered by hidden here. The
/// existing positional-path/glob/type filters (`apply_filters`, already
/// normalized by the caller) and exit-code conventions are unchanged.
fn run_no_ignore(
    root: &Path,
    cwd_rel: &Path,
    args: &Args,
    timings: &mut Timings,
    had_error: bool,
) -> anyhow::Result<i32> {
    let mut files: Vec<PathBuf> = walk::sweep_unfiltered(root, args.hidden)?
        .into_iter()
        .map(|m| m.path)
        .collect();
    timings.stage("sweep_unfiltered");

    if args.files {
        // With --files the pattern slot is the glob.
        if let Some(g) = args.pattern.as_deref() {
            let glob = build_glob(g)?;
            files.retain(|f| glob.is_match(f));
        }
        apply_filters(&mut files, args)?;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let term = if args.null { 0 } else { b'\n' };
        for f in &files {
            println!("{}", display_path(f, cwd_rel).display());
            write_terminated_path(&mut lock, f, args.path_separator, term)?;
        }
        timings.finish();
        return Ok(if had_error {
            2
        } else if files.is_empty() {
            1
        } else {
            0
        });
    }

    let pattern = match args.regexp.clone().or_else(|| args.pattern.clone()) {
        Some(p) => p,
        None => anyhow::bail!("a pattern is required (or --files)"),
    };
    apply_filters(&mut files, args)?;
    timings.stage("candidates");

    let before = args.before_context.or(args.context).unwrap_or(0);
    let after = args.after_context.or(args.context).unwrap_or(0);
    let opts = search::SearchOpts {
        case_insensitive: args.ignore_case,
        fixed: args.fixed_strings,
        files_with_matches: args.files_with_matches,
        before,
        after,
        json: args.json,
        count: args.count,
        multiline: args.multiline,
        binary: binary_detection(args),
        display_prefix: cwd_rel.to_path_buf(),
        stats: args.stats,
    };
    let opts = search_opts(args, root);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, root, &files, &opts, &mut lock)?;
    timings.stage("search");
    timings.finish();
    Ok(if had_error {
        2
    } else if found {
        0
    } else {
        1
    })
}

/// Binary-detection mode for the searcher: `-a` searches binary files as
/// raw text, `--binary` searches them but reports a notice instead of the
/// matched lines, and the default quits a file at the first NUL byte.
/// `-a`/`--binary` are a last-wins pair, like rg.
fn binary_detection(args: &Args) -> grep_searcher::BinaryDetection {
    use grep_searcher::BinaryDetection;
    if args.text {
        BinaryDetection::none()
    } else if args.binary {
        BinaryDetection::convert(0)
    } else {
        BinaryDetection::quit(0)
    }
}

/// Resolved path-prefixing: explicit -H/-I win (last-wins pair); else on
/// for --vimgrep, or whenever the operand isn't exactly one file.
fn with_filename(args: &Args, root: &Path) -> bool {
    if args.no_filename {
        return false;
    }
    if args.with_filename {
        return true;
    }
    let single_file =
        args.paths.len() == 1 && root.join(&args.paths[0]).is_file();
    args.vimgrep || !single_file
}

fn search_opts(args: &Args, root: &Path) -> search::SearchOpts {
    search::SearchOpts {
        case_insensitive: args.ignore_case,
        fixed: args.fixed_strings,
        files_with_matches: args.files_with_matches,
        before: args.before_context.or(args.context).unwrap_or(0),
        after: args.after_context.or(args.context).unwrap_or(0),
        json: args.json,
        count: args.count,
        multiline: args.multiline,
        column: args.column || args.vimgrep,
        byte_offset: args.byte_offset,
        vimgrep: args.vimgrep,
        trim: args.trim,
        path_terminator: args.null.then_some(0u8),
        path_separator: args.path_separator,
        include_zero: args.include_zero,
        with_filename: with_filename(args, root),
    }
}

pub fn run() -> anyhow::Result<i32> {
    let mut args = Args::parse();
    if let Some(n) = args.threads {
        // Global rayon pool; .ok() because a second init simply keeps the
        // first (only ever happens under unit tests calling run() twice).
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }

    // With -e/--regexp the positional pattern slot is free; a bare
    // positional there is a path (e.g. `glep -e foo src`).
    if args.regexp.is_some() && !args.files {
        if let Some(p) = args.pattern.take() {
            args.paths.insert(0, PathBuf::from(p));
        }
    }
    let cwd = std::env::current_dir()?;
    let (root, cwd_rel) = discover_index_root(&cwd)?;
    // Missing path filters are errors (exit 2), checked against the cwd
    // before paths are relativized into the index tree.
    let had_error = report_missing_paths(&args.paths, &cwd, args.files);
    normalize_path_filters(&mut args.paths, &root, &cwd_rel);
    // No explicit paths: the implicit scope is the cwd subtree (rg's
    // default `.`). At the discovered root itself this is "" — everything.
    if args.paths.is_empty() && !cwd_rel.as_os_str().is_empty() {
        args.paths.push(cwd_rel.clone());
    }

    // Subcommand-style words in the pattern slot.
    if args.regexp.is_none() && !args.files && !args.no_ignore {
        match args.pattern.as_deref() {
            Some("index") => {
                let idx = Index::build(&root, args.max_filesize)?;
                eprintln!("glep: indexed {} files", idx.manifest.live_entries().count());
                return Ok(0);
            }
            Some("status") => {
                let mut idx = Index::open_or_build(&root, args.max_filesize)?;
                if !idx.read_only { idx.update(args.max_filesize, 0)?; }
                let live = idx.manifest.live_entries().count();
                let skipped = idx
                    .manifest
                    .live_entries()
                    .filter(|e| {
                        e.flags & (FLAG_SKIP_BINARY | FLAG_SKIP_TOO_LARGE) != 0
                    })
                    .count();
                println!("files: {live}");
                println!("skipped (binary/oversized): {skipped}");
                println!("last sweep epoch: {}", idx.manifest.last_sweep_epoch);
                return Ok(0);
            }
            _ => {}
        }
    }

    let mut timings = Timings::new();

    if args.no_ignore {
        return run_no_ignore(&root, &cwd_rel, &args, &mut timings, had_error);
    }

    if args.no_index {
        // Live gitignore-aware scan of the discovered tree: same file set
        // as the indexed path would yield, minus all trigram narrowing.
        let mut files: Vec<PathBuf> = walk::sweep(&root)?
            .into_iter()
            .filter(|m| args.hidden || !m.hidden)
            .map(|m| m.path)
            .collect();
        timings.stage("sweep");
        if args.files {
            if let Some(g) = args.pattern.as_deref() {
                let glob = build_glob(g)?;
                files.retain(|f| glob.is_match(f));
            }
            apply_filters(&mut files, &args)?;
            for f in &files {
                println!("{}", display_path(f, &cwd_rel).display());
            }
            timings.finish();
            return Ok(if had_error {
                2
            } else if files.is_empty() {
                1
            } else {
                0
            });
        }
        let pattern = match args.regexp.clone().or_else(|| args.pattern.clone()) {
            Some(p) => p,
            None => anyhow::bail!("a pattern is required (or --files)"),
        };
        apply_filters(&mut files, &args)?;
        let opts = search::SearchOpts {
            case_insensitive: args.ignore_case,
            fixed: args.fixed_strings,
            files_with_matches: args.files_with_matches,
            before: args.before_context.or(args.context).unwrap_or(0),
            after: args.after_context.or(args.context).unwrap_or(0),
            json: args.json,
            count: args.count,
            multiline: args.multiline,
            display_prefix: cwd_rel.to_path_buf(),
        };
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let found = search::run(&pattern, &root, &files, &opts, &mut lock)?;
        timings.finish();
        return Ok(if had_error {
            2
        } else if found {
            0
        } else {
            1
        });
    }

    let mut idx = Index::open_or_build(&root, args.max_filesize)?;
    timings.stage("index_open");
    // Path filters scope the freshness sweep too: subtrees outside the
    // filter can't produce results, so sweeping them is wasted work.
    let mut extra = if args.paths.is_empty() {
        idx.update_timed(args.max_filesize, args.ttl, &mut timings)?
    } else {
        idx.update_scoped(args.max_filesize, args.ttl, &args.paths, &mut timings)?
    };
    // `extra` is the read-only-mode live-scan fallback: files discovered by
    // this sweep that couldn't be written into the index because another
    // process holds the lock. They carry no FLAG_HIDDEN of their own (no
    // manifest entry yet), so apply the same rg-matching default here too:
    // hidden unless --hidden was passed, with the same whitelist rescue
    // the indexed path uses.
    if !args.hidden {
        let mut wl = walk::WhitelistChecker::new();
        extra.retain(|p| !wl.is_hidden(&root, p));
    }

    if args.files {
        let mut files = idx.live_files(args.hidden);
        files.extend(extra);
        files.sort();
        files.dedup();
        // With --files the pattern slot is the glob.
        if let Some(g) = args.pattern.as_deref() {
            let glob = build_glob(g)?;
            files.retain(|f| glob.is_match(f));
        }
        let args2 = Args { pattern: None, ..args };
        apply_filters(&mut files, &args2)?;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let term = if args.null { 0 } else { b'\n' };
        for f in &files {
            println!("{}", display_path(f, &cwd_rel).display());
            write_terminated_path(&mut lock, f, args.path_separator, term)?;
        }
        timings.finish();
        return Ok(if had_error {
            2
        } else if files.is_empty() {
            1
        } else {
            0
        });
    }

    let pattern = match args.regexp.clone().or_else(|| args.pattern.clone()) {
        Some(p) => p,
        None => anyhow::bail!("a pattern is required (or --files)"),
    };
    // -c --include-zero must emit `:0` for every walked file, so trigram
    // narrowing is unsound: degenerate to the full candidate set. Binary
    // files join too — under quit detection they simply count as 0.
    let include_binary = args.count && args.include_zero;
    let query_plan = if include_binary {
        crate::plan::Plan::All
    } else {
        plan::build(&pattern, args.fixed_strings, args.ignore_case)
    };
    timings.stage("plan");
    // Binary-flagged files are candidates only under -a/--binary: the
    // default quit detection can never emit them, so including them would
    // be pure IO cost (matches rg's observed behavior either way).
    let search_binary = args.text || args.binary;
    let mut files = idx.candidates(&query_plan, args.ignore_case, args.hidden, search_binary);
    files.extend(extra);
    let mut files = idx.candidates(
        &query_plan,
        args.ignore_case,
        args.hidden,
        include_binary,
    );
    files.extend(extra.iter().cloned());
    files.sort();
    files.dedup();
    apply_filters(&mut files, &args)?;
    timings.stage("candidates");

    let before = args.before_context.or(args.context).unwrap_or(0);
    let after = args.after_context.or(args.context).unwrap_or(0);
    let opts = search::SearchOpts {
        case_insensitive: args.ignore_case,
        fixed: args.fixed_strings,
        files_with_matches: args.files_with_matches,
        before,
        after,
        json: args.json,
        count: args.count,
        multiline: args.multiline,
        binary: binary_detection(&args),
        display_prefix: cwd_rel.to_path_buf(),
        stats: args.stats,
    };
    // rg's nothing-searched heuristic: with the implicit path scope, an
    // empty walked pool (ignore rules or filters ate everything) warns on
    // stderr and exits 2. An empty set produced by trigram narrowing is a
    // plain no-match (exit 1) — the pool was still searched.
    let mut nothing_searched = false;
    if files.is_empty() && args.paths.is_empty() {
        let mut pool = idx.live_files(args.hidden);
        pool.extend(extra.iter().cloned());
        apply_filters(&mut pool, &args)?;
        if pool.is_empty() {
            nothing_searched = true;
            eprintln!(
                "glep: No files were searched, which means glep probably applied a filter you didn't expect.\nRunning with --debug will show why files are being skipped."
            );
        }
    }

    let opts = search_opts(&args, &root);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, &root, &files, &opts, &mut lock)?;
    timings.stage("search");
    timings.finish();
    Ok(if had_error {
    Ok(if nothing_searched {
        2
    } else if found {
        0
    } else {
        1
    })
}
