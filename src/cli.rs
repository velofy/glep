use crate::index::manifest::{FLAG_SKIP_BINARY, FLAG_SKIP_TOO_LARGE};
use crate::index::Index;
use crate::timing::Timings;
use crate::{plan, search, walk};
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug, Clone)]
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
/// Print `files` one per line, buffered: stdout is a LineWriter, so a
/// per-path println! costs one flush syscall per line (27k syscalls on a
/// kernel-scale .c listing). Build the whole block and write it once.
fn print_paths_batched(files: &[PathBuf]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut buf = Vec::with_capacity(files.len() * 48);
    for f in files {
        // Raw bytes: Path::display().to_string() goes through the fmt
        // machinery per path (~0.7µs each, ~20ms for a kernel-scale .c
        // listing); the bytes are identical for the paths we emit.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            buf.extend_from_slice(f.as_os_str().as_bytes());
        }
        #[cfg(not(unix))]
        {
            let s = f.display().to_string();
            buf.extend_from_slice(s.as_bytes());
        }
        buf.push(b'\n');
    }
    std::io::stdout().write_all(&buf)?;
    Ok(())
}

/// macOS-only fast listing via the dir-mtime cache (see dircache.rs):
/// load the cached tree, verify the ignore-source fingerprint, run the
/// pruned walk (zero-clone: cache-hit entries borrow the loaded tree),
/// filter the result with `keep`, save the rebuilt tree if anything
/// changed, and return the matching paths (unsorted; caller sorts).
/// None = cache miss for any reason: caller falls back to the full
/// freshness-sweep path. Filtering inside keeps the loaded tree's
/// ownership local to this function.
#[cfg(target_os = "macos")]
fn listing_via_cache<F>(
    root: &Path,
    index_dir: &Path,
    keep: F,
    timings: &mut Timings,
) -> Option<Vec<PathBuf>>
where
    F: Fn(&crate::walk::FileMeta) -> bool + Sync,
{
    let t0 = std::time::Instant::now();
    let tree = crate::dircache::DirTree::load(index_dir)?;
    let t1 = std::time::Instant::now();
    if !tree.fingerprint_matches(root) {
        return None;
    }
    let outcome = crate::walk_bulk::sweep_bulk_listing(root, Some(&tree.root)).ok()?;
    if outcome.aborted {
        return None;
    }
    let t_cmp = std::time::Instant::now();
    let changed = outcome.tree != tree.root;
    if std::env::var_os("GLEP_TIMING").is_some() {
        eprintln!(
            "glep timing: tree compare {:.1}ms ({changed})",
            t_cmp.elapsed().as_secs_f64() * 1000.0
        );
    }
    if changed {
        let updated = crate::dircache::DirTree {
            schema: crate::dircache::SCHEMA,
            fingerprint: tree.fingerprint.clone(),
            root: outcome.tree,
        };
        let _ = updated.save(index_dir);
    }
    // Parallel filter: 86k globset matches on a kernel-scale tree cost
    // ~15ms serial, ~2ms across the pool.
    use rayon::prelude::*;
    let matched: Vec<PathBuf> = outcome
        .files
        .par_iter()
        .filter(|m| keep(m))
        .map(|m| m.path.clone())
        .collect();
    timings.stage("listing_cache_walk");
    if std::env::var_os("GLEP_TIMING").is_some() {
        eprintln!(
            "glep timing: cache load {:.1}ms, pruned walk+filter {:.1}ms",
            t1.duration_since(t0).as_secs_f64() * 1000.0,
            t1.elapsed().as_secs_f64() * 1000.0
        );
    }
    Some(matched)
}

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
        print_paths_batched(&files)?;
        timings.finish();
        return Ok(if files.is_empty() { 1 } else { 0 });
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
    };
    let mut out = search::StdoutOut;
    let found = search::run(&pattern, root, &files, &opts, &mut out)?;
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
                eprintln!(
                    "glep: indexed {} files",
                    idx.manifest.live_entries().count()
                );
                return Ok(0);
            }
            Some("status") => {
                let mut idx = Index::open_or_build(&root, args.max_filesize)?;
                if !idx.read_only {
                    idx.update(args.max_filesize, 0)?;
                }
                let live = idx.manifest.live_entries().count();
                let skipped = idx
                    .manifest
                    .live_entries()
                    .filter(|e| e.flags & (FLAG_SKIP_BINARY | FLAG_SKIP_TOO_LARGE) != 0)
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

    // Dir-mtime cached listing (macOS only), consulted BEFORE even
    // opening the index: this path needs no manifest, no postings, no
    // freshness sweep, just the cache and the pruned walk. The index
    // opens below only on cache miss.
    #[cfg(target_os = "macos")]
    if args.files {
        // Hidden flag + positional pattern slot as a glob.
        let hidden = args.hidden;
        let mut glob_matcher = None;
        if let Some(g) = args.pattern.clone() {
            glob_matcher = Some(build_glob(&g)?);
        }
        let keep = move |m: &crate::walk::FileMeta| -> bool {
            if !hidden && m.hidden {
                return false;
            }
            match &glob_matcher {
                Some(g) => g.is_match(&m.path),
                None => true,
            }
        };
        if let Some(mut files) = listing_via_cache(&root, &root.join(".glep"), keep, &mut timings) {
            let args2 = Args {
                pattern: None,
                ..args.clone()
            };
            apply_filters(&mut files, &args2)?;
            use rayon::prelude::*;
            files.par_sort();
            print_paths_batched(&files)?;
            timings.stage("listing_cache");
            timings.finish();
            return Ok(if files.is_empty() { 1 } else { 0 });
        }
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
        files.extend(extra.clone());
        files.sort();
        files.dedup();
        // With --files the pattern slot is the glob.
        if let Some(g) = args.pattern.as_deref() {
            let glob = build_glob(g)?;
            files.retain(|f| glob.is_match(f));
        }
        let args2 = Args {
            pattern: None,
            ..args
        };
        apply_filters(&mut files, &args2)?;
        print_paths_batched(&files)?;
        timings.finish();
        // Rebuild the listing cache from the authoritative sweep (macOS
        // only; skip when the read-only fallback contributed files, they
        // are not part of a cacheable coherent tree view).
        #[cfg(target_os = "macos")]
        {
            if extra.is_empty() {
                let listing = idx.live_file_metas(true);
                let tree = crate::dircache::build_from_listing(
                    &root,
                    &listing,
                    crate::dircache::source_fingerprint(&root),
                );
                let _ = tree.save(&idx.dir);
            }
        }
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
    let mut out = search::StdoutOut;
    let found = search::run(&pattern, &root, &files, &opts, &mut out)?;
    timings.stage("search");
    timings.finish();
    Ok(if found { 0 } else { 1 })
}
