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

fn normalize_path_filters(paths: &mut [PathBuf], root: &std::path::Path) {
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for p in paths.iter_mut() {
        if p.is_absolute() {
            let canonical_p = std::fs::canonicalize(&*p).unwrap_or_else(|_| p.clone());
            if let Ok(rel) = canonical_p.strip_prefix(&canonical_root) {
                *p = rel.to_path_buf();
            }
        } else if let Ok(stripped) = p.strip_prefix(".") {
            *p = stripped.to_path_buf();
        }
    }
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
fn run_no_ignore(root: &Path, args: &Args, timings: &mut Timings) -> anyhow::Result<i32> {
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
            write_terminated_path(&mut lock, f, args.path_separator, term)?;
        }
        timings.finish();
        return Ok(if files.is_empty() { 1 } else { 0 });
    }

    let pattern = match args.regexp.clone().or_else(|| args.pattern.clone()) {
        Some(p) => p,
        None => anyhow::bail!("a pattern is required (or --files)"),
    };
    apply_filters(&mut files, args)?;
    timings.stage("candidates");

    let opts = search_opts(args, root);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, root, &files, &opts, &mut lock)?;
    timings.stage("search");
    timings.finish();
    Ok(if found { 0 } else { 1 })
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
    let root = std::env::current_dir()?;
    normalize_path_filters(&mut args.paths, &root);

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
        return run_no_ignore(&root, &args, &mut timings);
    }

    let mut idx = Index::open_or_build(&root, args.max_filesize)?;
    timings.stage("index_open");
    let mut extra = idx.update_timed(args.max_filesize, args.ttl, &mut timings)?;
    // `extra` is the read-only-mode live-scan fallback: files discovered by
    // this sweep that couldn't be written into the index because another
    // process holds the lock. They carry no FLAG_HIDDEN of their own (no
    // manifest entry yet), so apply the same rg-matching default here too:
    // hidden unless --hidden was passed.
    if !args.hidden {
        extra.retain(|p| !walk::path_is_hidden(p));
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
            write_terminated_path(&mut lock, f, args.path_separator, term)?;
        }
        timings.finish();
        return Ok(if files.is_empty() { 1 } else { 0 });
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
    Ok(if nothing_searched {
        2
    } else if found {
        0
    } else {
        1
    })
}
