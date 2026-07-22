use crate::index::Index;
use crate::timing::Timings;
use crate::{plan, search, walk};
use clap::Parser;
use std::path::PathBuf;

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
    #[arg(short = 'l', long)]
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
    if args.regexp.is_none() && !args.files {
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
                    .filter(|e| e.flags != 0)
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
    let query_plan = plan::build(&pattern, args.fixed_strings, args.ignore_case);
    timings.stage("plan");
    let mut files = idx.candidates(&query_plan, args.ignore_case, args.hidden);
    files.extend(extra);
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
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, &root, &files, &opts, &mut lock)?;
    timings.stage("search");
    timings.finish();
    Ok(if found { 0 } else { 1 })
}
