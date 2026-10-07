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
    #[arg(short = 'i', long, overrides_with = "smart_case")]
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
    /// Match only whole words
    #[arg(short = 'w', long = "word-regexp", overrides_with = "line_regexp")]
    pub word_regexp: bool,
    /// Match only whole lines
    #[arg(short = 'x', long = "line-regexp", overrides_with = "word_regexp")]
    pub line_regexp: bool,
    /// Case-insensitive only when the pattern has no uppercase chars
    #[arg(short = 'S', long = "smart-case", overrides_with = "ignore_case")]
    pub smart_case: bool,
    /// Match lines that do NOT match the pattern
    #[arg(short = 'v', long = "invert-match")]
    pub invert_match: bool,
    /// Stop after NUM matching lines per file
    #[arg(short = 'm', long = "max-count", value_name = "NUM")]
    pub max_count: Option<u64>,
    /// Replace lines longer than NUM bytes with an omission note
    #[arg(short = 'M', long = "max-columns", value_name = "NUM")]
    pub max_columns: Option<u64>,
    /// Print only the matched part of each matching line
    #[arg(short = 'o', long = "only-matching")]
    pub only_matching: bool,
    /// Show line numbers (already the default; accepted for rg parity)
    #[arg(short = 'n', long = "line-number", overrides_with = "no_line_number")]
    pub line_number: bool,
    /// Suppress line numbers
    #[arg(short = 'N', long = "no-line-number", overrides_with = "line_number")]
    pub no_line_number: bool,
    /// Print each file's path on its own line above its matches
    #[arg(long)]
    pub heading: bool,
    /// Suppress all output; the exit code alone reports whether a match
    /// exists (--json still emits the closing summary event, like rg)
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Skip the freshness sweep if the last one ran within this many seconds
    #[arg(long, default_value_t = 0)]
    pub ttl: u64,
    #[arg(long, default_value_t = 1_048_576)]
    pub max_filesize: u64,
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
    Ok(())
}

fn search_opts(args: &Args) -> search::SearchOpts {
    let before = args.before_context.or(args.context).unwrap_or(0);
    let after = args.after_context.or(args.context).unwrap_or(0);
    search::SearchOpts {
        case_insensitive: args.ignore_case,
        fixed: args.fixed_strings,
        files_with_matches: args.files_with_matches,
        before,
        after,
        json: args.json,
        count: args.count,
        multiline: args.multiline,
        word: args.word_regexp,
        line_regexp: args.line_regexp,
        smart_case: args.smart_case,
        invert: args.invert_match,
        max_count: args.max_count,
        // -M0 means no limit in rg.
        max_columns: args.max_columns.filter(|&n| n > 0),
        line_number: !args.no_line_number,
        heading: args.heading,
        only_matching: args.only_matching,
        quiet: args.quiet,
    }
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
        for f in &files {
            println!("{}", f.display());
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

    let opts = search_opts(&args);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, root, &files, &opts, &mut lock)?;
    timings.stage("search");
    timings.finish();
    Ok(if found { 0 } else { 1 })
}

pub fn run() -> anyhow::Result<i32> {
    let mut args = Args::parse();

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
        for f in &files {
            println!("{}", f.display());
        }
        timings.finish();
        return Ok(if files.is_empty() { 1 } else { 0 });
    }

    let pattern = match args.regexp.clone().or_else(|| args.pattern.clone()) {
        Some(p) => p,
        None => anyhow::bail!("a pattern is required (or --files)"),
    };
    // -S resolves case per-pattern at match time (an inline (?i) wins), so
    // for narrowing it is treated as case-insensitive unconditionally: the
    // case-variant union is a superset of whatever the matcher resolves to.
    let plan_ic = args.ignore_case || args.smart_case;
    // Under -v even a file with zero occurrences of the literals has all of
    // its lines "match", so trigram narrowing is unsound; scan everything.
    let query_plan = if args.invert_match {
        plan::Plan::All
    } else {
        plan::build(&pattern, args.fixed_strings, plan_ic)
    };
    timings.stage("plan");
    let mut files = idx.candidates(&query_plan, plan_ic, args.hidden);
    files.extend(extra);
    files.sort();
    files.dedup();
    apply_filters(&mut files, &args)?;
    timings.stage("candidates");

    let opts = search_opts(&args);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, &root, &files, &opts, &mut lock)?;
    timings.stage("search");
    timings.finish();
    Ok(if found { 0 } else { 1 })
}
