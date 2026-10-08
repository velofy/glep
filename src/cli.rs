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
    /// Explicit pattern (repeatable: multiple patterns are OR'd; use when
    /// the pattern is literally "index" or "status"). When any -e is
    /// present, all positionals are paths.
    #[arg(short = 'e', long = "regexp")]
    pub regexp: Vec<String>,
    pub regexp: Option<String>,
    /// Read patterns from a file, one per line; OR'd with -e/positional
    /// (rg -f/--file, repeatable)
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub pattern_files: Vec<std::path::PathBuf>,
    /// List files matching a glob instead of searching content
    #[arg(long)]
    pub files: bool,
    /// List files that do NOT match the pattern (rg --files-without-match)
    #[arg(long, conflicts_with_all = ["files_with_matches", "json", "count"])]
    pub files_without_match: bool,
    #[arg(short = 'i', long)]
    pub ignore_case: bool,
    #[arg(short = 'F', long)]
    pub fixed_strings: bool,
    #[arg(short = 'l', long, conflicts_with = "json")]
    pub files_with_matches: bool,
    #[arg(short = 'c', long = "count", conflicts_with_all = ["files_with_matches", "json"])]
    pub count: bool,
    /// Filter candidate files by glob (repeatable; gitignore-style `!`
    /// negation, last match wins). --iglob is an alias for ag compat.
    #[arg(short = 'g', long = "glob", visible_alias = "iglob")]
    pub globs: Vec<String>,
    /// Filter candidate files by type from the ignore crate's defaults (repeatable)
    #[arg(short = 't', long = "type")]
    pub types: Vec<String>,
    /// Exclude candidate files by type (repeatable), rg -T
    #[arg(short = 'T', long = "type-not")]
    pub types_not: Vec<String>,
    /// Sort results by the given field (path, modified, accessed, created,
    /// none), rg --sort
    #[arg(long, value_name = "SORTBY")]
    pub sort: Option<String>,
    /// Reverse of --sort, rg --sortr
    #[arg(long, value_name = "SORTBY")]
    pub sortr: Option<String>,
    /// Count match occurrences per file instead of matching lines
    /// (rg --count-matches; differs from -c which counts lines)
    #[arg(long, conflicts_with_all = ["files_with_matches", "json", "count"])]
    pub count_matches: bool,
    /// Accepted for script compat; glep reads no config file anyway
    #[arg(long, hide = true)]
    pub no_config: bool,
    /// Regex engine; only "default"/"auto" is supported — pcre2 errors out
    /// like the reference does for unknown engines.
    #[arg(long, value_name = "ENGINE")]
    pub engine: Option<String>,
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
    /// Only apply .gitignore/.gitexclude rules inside a real git repo
    /// (rg --require-git). Outside a repo git rules are inert — those
    /// files aren't in the manifest, so that case takes a live-scan
    /// escape hatch. .ignore/.rgignore still apply either way.
    #[arg(long)]
    pub require_git: bool,
    /// Additional ignore rules file applied to results (repeatable,
    /// rg --ignore-file). Rules resolve relative to the file's dir.
    #[arg(long, value_name = "PATH")]
    pub ignore_file: Vec<std::path::PathBuf>,
    /// Unrestricted search: -u = --no-ignore, -uu = --no-ignore + --hidden.
    /// (-uuu also implies -a/--text; reserved until binary text mode lands.)
    #[arg(short = 'u', action = clap::ArgAction::Count)]
    pub unrestricted: u8,
    /// Color output: never (default), always, auto (when stdout is a tty).
    /// --colors sets individual specs like the reference's --colors flag.
    #[arg(long, value_name = "WHEN")]
    pub color: Option<String>,
    /// Additional color spec, e.g. --colors 'path:fg:magenta' (repeatable)
    #[arg(long = "colors", value_name = "COLOR_SPEC")]
    pub color_specs: Vec<String>,
    /// Print every line (matches still marked), rg --passthru
    #[arg(long)]
    pub passthru: bool,
    /// Disable unicode mode in the regex (rg --no-unicode)
    #[arg(long)]
    pub no_unicode: bool,
    /// Separator between match fields (path/line/content), single byte —
    /// rg --field-match-separator
    #[arg(long, value_name = "SEP")]
    pub field_match_separator: Option<String>,
    /// Separator between context fields, single byte —
    /// rg --field-context-separator
    #[arg(long, value_name = "SEP")]
    pub field_context_separator: Option<String>,
    /// Separator printed between match groups/files — rg --context-separator.
    /// "" disables the separator entirely.
    #[arg(long, value_name = "SEP")]
    pub context_separator: Option<String>,
    /// PCRE2 regex engine (lookaround, backrefs, etc.)
    #[arg(short = 'P', long = "pcre2")]
    pub pcre2: bool,
    /// Include hidden (dot-prefixed) files and directories, rg semantics.
    /// .git is always excluded regardless of this flag.
    #[arg(long)]
    pub hidden: bool,
    /// Follow symbolic links (-L/--follow). Live-scan escape hatch:
    /// files reached through symlinked dirs are not in the index, so the
    /// index can't narrow them — the query sweeps the followed tree and
    /// scans everything (same trade as --no-ignore).
    #[arg(short = 'L', long)]
    pub follow: bool,
    /// Stay on the root's filesystem — don't descend into other mounts
    /// (rg --one-file-system). Live-scan escape hatch: mount-point files
    /// must not enter the index, so this never touches .glep.
    #[arg(long)]
    pub one_file_system: bool,
    /// NUL is the line terminator (rg --null-data)
    #[arg(long = "null-data")]
    pub null_data: bool,
    /// DFA size limit for the regex engine (rg --dfa-size-limit)
    #[arg(long, value_name = "BYTES")]
    pub dfa_size_limit: Option<usize>,
    /// Regex compiled-size limit (rg --regex-size-limit)
    #[arg(long, value_name = "BYTES")]
    pub regex_size_limit: Option<usize>,
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
    /// Decode files with the given encoding label before searching
    /// (e.g. utf-16, latin1, shift_jis). Unknown labels error out like
    /// the reference.
    #[arg(short = 'E', long = "encoding")]
    pub encoding: Option<String>,
    /// Make `.` match newlines (dotall / (?s) regex mode). Independently
    /// settable like the reference; only visibly changes multiline (-U)
    /// searches since single-line scanning can't span lines anyway.
    #[arg(long)]
    pub multiline_dotall: bool,
    /// Flush stdout after every output record (rg --line-buffered).
    #[arg(long)]
    pub line_buffered: bool,
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
/// Combine `-e` patterns and the positional into one pattern string.
/// Multiple patterns OR together (each wrapped in a non-capturing group
/// so `^`/`$`/anchors keep their per-arm semantics). Under -F each arm is
/// escaped individually and the result is a plain regex union — the
/// matcher/planner must then treat it as regex, not literal, so callers
/// use `effective_fixed` from this same helper.
fn combine_patterns(args: &Args) -> anyhow::Result<(String, bool)> {
    let pats: &[String] = if !args.regexp.is_empty() {
        &args.regexp
    } else {
        match &args.pattern {
            Some(p) => std::slice::from_ref(p),
            None => &[],
        }
    };
/// Combine -e/--file/positional into one pattern string. Multiple
/// sources OR together, each arm wrapped in a non-capturing group so
/// ^/$ stay arm-local. -f files contribute one arm per non-empty line
/// (rg -f semantics; line-trailing `\n` stripped).
fn resolve_pattern(args: &Args) -> anyhow::Result<(String, bool)> {
    let mut pats: Vec<String> = args.regexp.clone().into_iter().collect();
    for f in &args.pattern_files {
        let text = std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?;
        for line in text.lines() {
            if !line.is_empty() {
                pats.push(line.to_string());
            }
        }
    }
    // -e/-f leave the positional slot as a path; only the bare
    // positional counts as a pattern when no -e/-f is present.
    if pats.is_empty() {
        if let Some(p) = &args.pattern {
            pats.push(p.clone());
        }
    }
    if pats.is_empty() {
        anyhow::bail!("a pattern is required (or --files)");
    }
    if pats.len() == 1 {
        return Ok((pats[0].clone(), args.fixed_strings));
    }
    // Multiple patterns OR together; each arm wrapped in a
    // non-capturing group so ^/$ stay arm-local. Under -F each arm is
    // escaped individually and the matcher/planner treat the union as a
    // regex (effective_fixed = false).
    // -F across multiple arms: each arm escapes to a literal and the
    // union is a regex — the matcher/planner see effective_fixed=false.
    let fixed = args.fixed_strings;
    let joined = pats
        .iter()
        .map(|p| if fixed { regex_syntax::escape(p) } else { p.clone() })
        .map(|p| format!("(?:{p})"))
        .collect::<Vec<_>>()
        .join("|");
    Ok((joined, false))
}

fn normalize_path_filters(paths: &mut [PathBuf], root: &std::path::Path) {
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
/// --color WHEN resolution: `always` and `auto`+tty enable ANSI output;
/// `never` and anything else disable it. `auto` on a piped stream is the
/// common path (agents capture stdout), so it stays colorless there.
fn want_color(args: &Args) -> bool {
    match args.color.as_deref() {
        Some("always") => true,
        Some("auto") | None => {
            use std::io::IsTerminal;
            std::io::stdout().is_terminal()
        }
        _ => false,
    }
}

fn apply_filters(files: &mut Vec<PathBuf>, args: &Args) -> anyhow::Result<()> {
    if !args.paths.is_empty() {
        files.retain(|f| args.paths.iter().any(|p| f.starts_with(p)));
    }
    if !args.globs.is_empty() {
        // Gitignore-style overrides (rg's -g semantics): positives
        // whitelist, `!` negates, last matching rule wins; with only
        // negations, unmatched files pass.
        let mut ob = ignore::overrides::OverrideBuilder::new("");
        for g in &args.globs {
            ob.add(g)?;
        }
        let overrides = ob.build()?;
        files.retain(|f| !overrides.matched(f, false).is_ignore());
    }
    if !args.types.is_empty() || !args.types_not.is_empty() {
    if !args.ignore_file.is_empty() {
        // Build one Gitignore per extra rules file, anchored at the
        // file's parent dir so its patterns resolve like a .gitignore
        // sitting there (rg semantics).
        let mut gbs = Vec::new();
        for p in &args.ignore_file {
            let parent = p.parent().map(|d| d.to_path_buf()).unwrap_or_default();
            let mut b = ignore::gitignore::GitignoreBuilder::new(&parent);
            if let Some(e) = b.add(p) {
                anyhow::bail!("{}: {e}", p.display());
            }
            // Canonicalize for the strip below (macOS /var -> /private/var
            // etc.); matched_path_or_any_parents panics on paths outside
            // the gitignore root, so only call it when the strip works.
            let parent = std::fs::canonicalize(&parent).unwrap_or(parent);
            gbs.push((parent, b.build()?));
        }
        files.retain(|f| {
            let abs = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
            !gbs.iter().any(|(root, g)| {
                abs.strip_prefix(root).is_ok_and(|rel| {
                    g.matched_path_or_any_parents(rel, false).is_ignore()
                })
            })
        });
    }
    if !args.types.is_empty() {
        let mut tb = ignore::types::TypesBuilder::new();
        tb.add_defaults();
        for t in &args.types {
            tb.select(t);
        }
        for t in &args.types_not {
            tb.negate(t);
        }
        let types = tb.build()?;
        files.retain(|f| {
            let m = types.matched(f, false);
            // -t present -> require whitelist; -T only -> keep unless
            // explicitly ignored (same rule as globs' overrides).
            !m.is_ignore() && (args.types.is_empty() || m.is_whitelist())
        });
    }
    Ok(())
}

/// Sort the file list per --sort/--sortr. `modified` uses manifest mtimes
/// via `mtime_of`; `accessed`/`created` stat each candidate (sets are
/// usually small); `path` is the default order; `none` keeps sweep order.
fn apply_sort(
    files: &mut Vec<PathBuf>,
    args: &Args,
    idx: &Index,
    root: &Path,
) -> anyhow::Result<()> {
    let mode = args.sort.as_deref().or(args.sortr.as_deref());
    let Some(mode) = mode else { return Ok(()) };
    match mode {
        "path" | "none" => {}
        "modified" => {
            let mtimes = idx.mtime_map();
            files.sort_by_key(|f| (mtimes.get(f.as_path()).copied().unwrap_or(0), f.clone()))
        }
        "accessed" | "created" => {
            let key = |f: &PathBuf| -> u128 {
                std::fs::metadata(root.join(f))
                    .ok()
                    .and_then(|m| {
                        let t = if mode == "accessed" { m.accessed() } else { m.created() };
                        t.ok()
                    })
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            };
            files.sort_by_key(|f| (key(f), f.clone()));
        }
        other => anyhow::bail!("{other}: unsupported sort field (path|modified|accessed|created|none)"),
    }
    if args.sortr.is_some() {
        files.reverse();
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
/// `-L`/`--follow`: live-scan escape hatch — files reached through
/// symlinked directories are never in the manifest, so index narrowing
/// cannot find them; sweep the followed tree (ignore rules still
/// applied — `-L` only changes traversal) and search everything. Same
/// correctness-over-speed tradeoff as `--no-ignore`.
fn run_follow(root: &Path, args: &Args, timings: &mut Timings) -> anyhow::Result<i32> {
    let files: Vec<PathBuf> = walk::sweep_follow(root, args.hidden)?
        .into_iter()
        .map(|m| m.path)
        .collect();
    timings.stage("sweep_follow");
    run_live_files(root, args, timings, files)
}

fn run_no_ignore(root: &Path, args: &Args, timings: &mut Timings) -> anyhow::Result<i32> {
    let files: Vec<PathBuf> = walk::sweep_unfiltered(root, args.hidden, args.follow)?
fn run_no_ignore(root: &Path, args: &Args, timings: &mut Timings) -> anyhow::Result<i32> {
    let files: Vec<PathBuf> = walk::sweep_unfiltered(root, args.hidden)?
        .into_iter()
        .map(|m| m.path)
        .collect();
    timings.stage("sweep_unfiltered");
    run_live_files(root, args, timings, files)
}

/// Shared tail of the live-scan escape hatches (`--no-ignore`, `-L`):
/// --files listing and content search over an already-computed file set.
/// `--require-git` outside a repo: git-derived ignore rules are inert,
/// so files the manifest lacks (gitignored) can match — live scan over
/// `sweep_no_git` (git sources off, `.ignore`/`.rgignore` still on).
fn run_require_git(root: &Path, args: &Args, timings: &mut Timings) -> anyhow::Result<i32> {
    let files: Vec<PathBuf> = walk::sweep_no_git(root, args.hidden)?
        .into_iter()
        .map(|m| m.path)
        .collect();
    timings.stage("sweep_no_git");
    run_live_files(root, args, timings, files)
}

/// Shared tail of the live-scan paths (`--no-ignore`, `--require-git`
/// outside a repo): --files listing and content search over an
/// already-computed file set.
fn run_live_files(
    root: &Path,
    args: &Args,
    timings: &mut Timings,
    mut files: Vec<PathBuf>,
) -> anyhow::Result<i32> {
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

    let (pattern, effective_fixed) = combine_patterns(&args)?;
    let (pattern, effective_fixed) = resolve_pattern(&args)?;
    apply_filters(&mut files, args)?;
    timings.stage("candidates");

    let before = args.before_context.or(args.context).unwrap_or(0);
    let after = args.after_context.or(args.context).unwrap_or(0);
    let opts = search::SearchOpts {
        case_insensitive: args.ignore_case,
        fixed: effective_fixed,
        files_with_matches: args.files_with_matches,
        files_without_match: args.files_without_match,
        before,
        after,
        json: args.json,
        count: args.count,
        count_matches: args.count_matches,
        multiline: args.multiline,
        binary: binary_detection(args),
        display_prefix: cwd_rel.to_path_buf(),
        stats: args.stats,
        multiline_dotall: args.multiline_dotall,
        encoding: args.encoding.clone(),
        line_buffered: args.line_buffered,
        color: want_color(&args),
        color_specs: args.color_specs.clone(),
        passthru: args.passthru,
        unicode: !args.no_unicode,
        null_data: args.null_data,
        dfa_size_limit: args.dfa_size_limit,
        regex_size_limit: args.regex_size_limit,
    };
    let opts = search_opts(args, root);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, root, &files, None, &opts, &mut lock)?;
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

/// `--one-file-system`: live-scan escape hatch in the same shape as
/// `--no-ignore` — mount-point subtrees must not enter the index, so the
/// flag routes through `walk::sweep_one_fs` (ignores ON, same-st_dev
/// descent) instead of ever consulting the index.
fn run_one_fs(root: &Path, args: &Args, timings: &mut Timings) -> anyhow::Result<i32> {
    let mut files: Vec<PathBuf> = walk::sweep_one_fs(root, args.hidden)?
        .into_iter()
        .map(|m| m.path)
        .collect();
    timings.stage("sweep_one_fs");

    if args.files {
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
        null_data: args.null_data,
        dfa_size_limit: args.dfa_size_limit,
        regex_size_limit: args.regex_size_limit,
        field_match_separator: args
            .field_match_separator
            .as_ref()
            .map(|v| v.clone().into_bytes()),
        field_context_separator: args
            .field_context_separator
            .as_ref()
            .map(|v| v.clone().into_bytes()),
        context_separator: args.context_separator.as_ref().map(|v| v.clone().into_bytes()),
        pcre2: args.pcre2,
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let found = search::run(&pattern, root, &files, &opts, &mut lock)?;
    timings.finish();
    Ok(if found { 0 } else { 1 })
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
    if !args.regexp.is_empty() && !args.files {
    if (args.regexp.is_some() || !args.pattern_files.is_empty()) && !args.files {
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
    // -u/-uu/-uuu fold into their flag equivalents (rg semantics).
    if args.unrestricted > 0 {
        args.no_ignore = true;
        if args.unrestricted >= 2 {
            args.hidden = true;
        }
    }
    let root = std::env::current_dir()?;
    normalize_path_filters(&mut args.paths, &root);

    // Subcommand-style words in the pattern slot.
    if args.regexp.is_empty() && !args.files && !args.no_ignore {
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
    if args.require_git && !root.join(".git").exists() {
        return run_require_git(&root, &args, &mut timings);
    }
    if args.one_file_system {
        return run_one_fs(&root, &args, &mut timings);
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
    if args.follow {
        return run_follow(&root, &args, &mut timings);
    }

    if let Some(e) = &args.engine {
        match e.as_str() {
            "default" | "auto" => {}
            other => anyhow::bail!("unrecognized regex engine '{other}'"),
        }
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
    // --passthru prints every line of every *searched* file, so index
    // narrowing would silently drop files whose lines must still appear:
    // it forces a full scan.
    let query_plan = if args.passthru {
        crate::plan::Plan::All
    } else {
        plan::build(&pattern, args.fixed_strings, args.ignore_case)
    };
    let (pattern, effective_fixed) = combine_patterns(&args)?;
    let query_plan = plan::build(&pattern, effective_fixed, args.ignore_case);
    // The index stores RAW file bytes; a non-UTF-8 -E decodes before
    // matching, so index trigrams can't narrow a decoded match — fall
    // back to scanning everything (sound, just slower). ASCII-only
    // patterns on single-byte encodings could narrow, but keep it simple
    // and always-correct.
    let utf8_only = match &args.encoding {
        Some(label) => grep_searcher::Encoding::new(label)
            .map_err(|e| anyhow::anyhow!("{label}: {e}"))?
            == grep_searcher::Encoding::new("utf-8").unwrap(),
        None => true,
    };
    let query_plan = if utf8_only {
        plan::build(&pattern, args.fixed_strings, args.ignore_case)
    } else {
        crate::plan::Plan::All
    };
    timings.stage("plan");
    // Binary-flagged files are candidates only under -a/--binary: the
    // default quit detection can never emit them, so including them would
    // be pure IO cost (matches rg's observed behavior either way).
    let search_binary = args.text || args.binary;
    let mut files = idx.candidates(&query_plan, args.ignore_case, args.hidden, search_binary);
    let (pattern, effective_fixed) = resolve_pattern(&args)?;
    let query_plan = plan::build(&pattern, effective_fixed, args.ignore_case);
    timings.stage("plan");
    // --files-without-match needs the full live set as its universe:
    // narrowed-out files can't match, so they emit without searching.
    let universe;
    if args.files_without_match {
        let mut u = idx.live_files(args.hidden);
        u.extend(extra.iter().cloned());
        apply_filters(&mut u, &args)?;
        u.sort();
        u.dedup();
        universe = Some(u);
    } else {
        universe = None;
    }
    let mut files = idx.candidates(&query_plan, args.ignore_case, args.hidden);
    let mut files = idx.candidates(&query_plan, args.ignore_case, args.hidden, args.null_data);
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
    apply_sort(&mut files, &args, &idx, &root)?;
    timings.stage("candidates");

    let before = args.before_context.or(args.context).unwrap_or(0);
    let after = args.after_context.or(args.context).unwrap_or(0);
    let opts = search::SearchOpts {
        case_insensitive: args.ignore_case,
        fixed: effective_fixed,
        files_with_matches: args.files_with_matches,
        files_without_match: args.files_without_match,
        before,
        after,
        json: args.json,
        count: args.count,
        count_matches: args.count_matches,
        multiline: args.multiline,
        binary: binary_detection(&args),
        display_prefix: cwd_rel.to_path_buf(),
        stats: args.stats,
        multiline_dotall: args.multiline_dotall,
        encoding: args.encoding.clone(),
        line_buffered: args.line_buffered,
        color: want_color(&args),
        color_specs: args.color_specs.clone(),
        passthru: args.passthru,
        unicode: !args.no_unicode,
        null_data: args.null_data,
        dfa_size_limit: args.dfa_size_limit,
        regex_size_limit: args.regex_size_limit,
        field_match_separator: args
            .field_match_separator
            .as_ref()
            .map(|v| v.clone().into_bytes()),
        field_context_separator: args
            .field_context_separator
            .as_ref()
            .map(|v| v.clone().into_bytes()),
        context_separator: args.context_separator.as_ref().map(|v| v.clone().into_bytes()),
        pcre2: args.pcre2,
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
    let found = search::run(&pattern, &root, &files, universe.as_deref(), &opts, &mut lock)?;
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
