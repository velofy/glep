use crate::index::manifest::{FLAG_HIDDEN, FLAG_SKIP_BINARY, FLAG_SKIP_TOO_LARGE, FLAG_TRANSCODE};
use crate::index::Index;
use crate::timing::Timings;
use crate::{filters, messages, plan, search, walk};
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "glep",
    version,
    about = "Indexed grep + glob for AI agents",
    // A repeated flag is last-wins, like rg (`-n -n`, `--sort path --sort
    // none`); repeatable lists (-e, -g, -t, ...) and the -u counter keep
    // accumulating.
    args_override_self = true
)]
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
    /// Read patterns from a file, one per line; OR'd with -e/positional
    /// (rg -f/--file, repeatable)
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub pattern_files: Vec<std::path::PathBuf>,
    /// List files matching a glob instead of searching content
    #[arg(long)]
    pub files: bool,
    /// List files that do NOT match the pattern (rg --files-without-match).
    /// The output modes -l, --files-without-match, -c, --count-matches
    /// and --json are last-wins, like rg's search modes.
    #[arg(long, overrides_with_all = ["files_with_matches", "json", "count", "count_matches"])]
    pub files_without_match: bool,
    #[arg(short = 'i', long, overrides_with = "smart_case")]
    pub ignore_case: bool,
    #[arg(short = 'F', long)]
    pub fixed_strings: bool,
    #[arg(short = 'l', long, overrides_with_all = ["files_without_match", "json", "count", "count_matches"])]
    pub files_with_matches: bool,
    #[arg(short = 'c', long = "count", overrides_with_all = ["files_with_matches", "files_without_match", "json", "count_matches"])]
    pub count: bool,
    /// Filter candidate files by glob (repeatable; gitignore-style `!`
    /// negation, last match wins).
    #[arg(short = 'g', long = "glob")]
    pub globs: Vec<String>,
    /// Like -g/--glob but matched case-insensitively (rg --iglob). As in
    /// rg, these rules come after every -g rule, so they win ties.
    #[arg(long = "iglob", value_name = "GLOB")]
    pub iglobs: Vec<String>,
    /// Filter candidate files by type from the ignore crate's defaults (repeatable)
    #[arg(short = 't', long = "type")]
    pub types: Vec<String>,
    /// Exclude candidate files by type (repeatable), rg -T
    #[arg(short = 'T', long = "type-not")]
    pub types_not: Vec<String>,
    /// Sort results by the given field (path, modified, accessed, created,
    /// none), rg --sort. Overrides --sortr.
    #[arg(long, value_name = "SORTBY", overrides_with = "sortr")]
    pub sort: Option<String>,
    /// Reverse of --sort, rg --sortr. Overrides --sort.
    #[arg(long, value_name = "SORTBY", overrides_with = "sort")]
    pub sortr: Option<String>,
    /// Count match occurrences per file instead of matching lines
    /// (rg --count-matches; differs from -c which counts lines)
    #[arg(long, overrides_with_all = ["files_with_matches", "files_without_match", "json", "count"])]
    pub count_matches: bool,
    /// Accepted for script compat; glep reads no config file anyway
    #[arg(long, hide = true)]
    pub no_config: bool,
    /// Regex engine: default, pcre2 (same as -P), or auto (the default
    /// engine, falling back to PCRE2 when the pattern needs it). Other
    /// values error out like the reference.
    #[arg(long, value_name = "ENGINE")]
    pub engine: Option<String>,
    #[arg(short = 'C', long)]
    pub context: Option<usize>,
    #[arg(short = 'A', long = "after-context")]
    pub after_context: Option<usize>,
    #[arg(short = 'B', long = "before-context")]
    pub before_context: Option<usize>,
    #[arg(long, overrides_with_all = ["files_with_matches", "files_without_match", "count", "count_matches"])]
    pub json: bool,
    /// Allow matches to span multiple lines (patterns may contain \n).
    /// Overrides --stop-on-nonmatch.
    #[arg(short = 'U', long, overrides_with = "stop_on_nonmatch")]
    pub multiline: bool,
    /// Only apply .gitignore/.gitexclude rules inside a real git repo
    /// (rg --require-git). Outside a repo git rules are inert; those
    /// files aren't in the manifest, so that case takes a live-scan
    /// escape hatch. .ignore/.rgignore still apply either way.
    #[arg(long)]
    pub require_git: bool,
    /// Additional ignore rules file applied to results (repeatable,
    /// rg --ignore-file). Rules resolve relative to the file's dir.
    #[arg(long, value_name = "PATH")]
    pub ignore_file: Vec<std::path::PathBuf>,
    /// Unrestricted search: -u = --no-ignore, -uu = --no-ignore + --hidden,
    /// -uuu = --no-ignore + --hidden + --binary (rg's ladder).
    #[arg(short = 'u', long = "unrestricted", action = clap::ArgAction::Count)]
    pub unrestricted: u8,
    /// Color output: never (default), always, auto (when stdout is a tty).
    /// --colors sets individual specs like the reference's --colors flag.
    #[arg(long, value_name = "WHEN")]
    pub color: Option<String>,
    /// Additional color spec, e.g. --colors 'path:fg:magenta' (repeatable)
    #[arg(long = "colors", value_name = "COLOR_SPEC")]
    pub color_specs: Vec<String>,
    /// Print every line (matches still marked), rg --passthru (alias
    /// --passthrough). A last-wins pair with -A/-B/-C, like rg's context
    /// mode: whichever comes last decides.
    #[arg(
        long,
        alias = "passthrough",
        overrides_with_all = ["context", "after_context", "before_context"]
    )]
    pub passthru: bool,
    /// Disable unicode mode in the regex (rg --no-unicode)
    #[arg(long)]
    pub no_unicode: bool,
    /// Separator between match fields (path/line/content), default `:`;
    /// escapes like \t or \x7F are decoded (rg --field-match-separator)
    #[arg(long, value_name = "SEP")]
    pub field_match_separator: Option<String>,
    /// Separator between context fields, default `-`; escapes are
    /// decoded (rg --field-context-separator)
    #[arg(long, value_name = "SEP")]
    pub field_context_separator: Option<String>,
    /// Separator printed between non-contiguous context groups and
    /// between files in context mode, default `--`; escapes are decoded
    /// and "" still prints a line break (rg --context-separator)
    #[arg(long, value_name = "SEP", overrides_with = "no_context_separator")]
    pub context_separator: Option<String>,
    /// Print no context separators at all (rg --no-context-separator)
    #[arg(long, overrides_with = "context_separator")]
    pub no_context_separator: bool,
    /// PCRE2 regex engine (lookaround, backrefs, etc.). Last-wins with
    /// --engine and --no-pcre2, like rg.
    #[arg(short = 'P', long = "pcre2")]
    pub pcre2: bool,
    /// Include hidden (dot-prefixed) files and directories, rg semantics.
    /// .git is always excluded regardless of this flag.
    #[arg(long)]
    pub hidden: bool,
    /// Follow symbolic links (-L/--follow). Live-scan escape hatch:
    /// files reached through symlinked dirs are not in the index, so the
    /// index can't narrow them: the query sweeps the followed tree and
    /// scans everything (same trade as --no-ignore).
    #[arg(short = 'L', long)]
    pub follow: bool,
    /// Stay on the root's filesystem, don't descend into other mounts
    /// (rg --one-file-system). Live-scan escape hatch: mount-point files
    /// must not enter the index, so this never touches .glep.
    #[arg(long)]
    pub one_file_system: bool,
    /// Don't respect .ignore files (rg --no-ignore-dot). Files excluded
    /// only by .ignore aren't in the index, so this is a live-scan escape
    /// hatch.
    #[arg(long)]
    pub no_ignore_dot: bool,
    /// Don't respect .gitignore files (rg --no-ignore-vcs).
    #[arg(long)]
    pub no_ignore_vcs: bool,
    /// Don't respect .git/info/exclude (rg --no-ignore-exclude).
    #[arg(long)]
    pub no_ignore_exclude: bool,
    /// Don't respect global gitignore rules (rg --no-ignore-global).
    #[arg(long)]
    pub no_ignore_global: bool,
    /// Don't respect ignore files in parent dirs of the root
    /// (rg --no-ignore-parent).
    #[arg(long)]
    pub no_ignore_parent: bool,
    /// Suppress error messages about failed parses of ignore files
    /// (rg --no-ignore-messages). Nonfatal IO warnings stay.
    #[arg(long)]
    pub no_ignore_messages: bool,
    /// Suppress error messages (rg --no-messages): unreadable files and
    /// directories, missing operands, failing --pre commands. Like rg it
    /// hides the text only; such errors still make the exit code 2.
    #[arg(long = "no-messages")]
    pub no_messages: bool,
    /// Search case sensitively, the default (rg -s/--case-sensitive).
    /// Overrides -i and -S; whichever of the three comes last wins.
    #[arg(
        short = 's',
        long = "case-sensitive",
        overrides_with_all = ["ignore_case", "smart_case"]
    )]
    pub case_sensitive: bool,
    /// With -M, show a truncation preview marker on long lines
    /// (rg --max-columns-preview)
    #[arg(long, requires = "max_columns")]
    pub max_columns_preview: bool,
    /// Stop reading a file at the first non-matching line after a
    /// matching one (rg --stop-on-nonmatch). Overrides -U.
    #[arg(long, overrides_with = "multiline")]
    pub stop_on_nonmatch: bool,
    /// Add a file-type def `name:glob` or `name:include:a,b`
    /// (rg --type-add, repeatable)
    #[arg(long, value_name = "TYPE_SPEC")]
    pub type_add: Vec<String>,
    /// Clear a file type's definitions (rg --type-clear, repeatable)
    #[arg(long, value_name = "TYPE")]
    pub type_clear: Vec<String>,
    /// Print all known file types and exit (rg --type-list)
    #[arg(long)]
    pub type_list: bool,
    /// Alias spellings accepted for script compat (--print0 = -0,
    /// --sort-files = --sort path)
    #[arg(long, hide = true)]
    pub print0: bool,
    #[arg(long = "sort-files", hide = true)]
    pub sort_files: bool,
    /// rg -p/--pretty: colors + heading + line numbers
    #[arg(short = 'p', long = "pretty")]
    pub pretty: bool,
    /// Print the PCRE2 version and exit (rg --pcre2-version)
    #[arg(long)]
    pub pcre2_version: bool,
    /// Make -g/--glob globs case-insensitive (rg --glob-case-insensitive)
    #[arg(long)]
    pub glob_case_insensitive: bool,
    /// Case-insensitive ignore-file rule matching (rg --ignore-file-case-insensitive)
    #[arg(long)]
    pub ignore_file_case_insensitive: bool,
    /// Emit OSC8 hyperlinks for file paths, in the given format
    /// (rg --hyperlink-format; e.g. kitty, default, file). Like the
    /// reference, links are only written when color is on.
    #[arg(long, value_name = "FORMAT")]
    pub hyperlink_format: Option<String>,
    /// Program whose trimmed stdout is the hostname used by {host} in
    /// hyperlink formats; falls back to the platform hostname
    /// (rg --hostname-bin)
    #[arg(long, value_name = "COMMAND")]
    pub hostname_bin: Option<String>,
    /// Verbose debug output on stderr (rg --debug)
    #[arg(long)]
    pub debug: bool,
    /// Trace-level debug output on stderr (rg --trace)
    #[arg(long)]
    pub trace: bool,
    /// Replace matched text in output lines; capture groups resolve as
    /// $1/${name} (rg -r/--replace)
    #[arg(
        short = 'r',
        long = "replace",
        value_name = "REPLACEMENT",
        conflicts_with = "json"
    )]
    pub replace: Option<String>,
    /// Search each file's preprocessor output instead of its contents
    /// (rg --pre; the file path is the command's sole argument and the
    /// file is also its stdin)
    #[arg(long, value_name = "COMMAND")]
    pub pre: Option<String>,
    /// Apply --pre only to files matching this glob (rg --pre-glob)
    #[arg(long, value_name = "GLOB", requires = "pre")]
    pub pre_glob: Option<String>,
    /// Treat \r\n as the line terminator (rg --crlf). A last-wins pair
    /// with --null-data, like rg.
    #[arg(long, overrides_with = "null_data")]
    pub crlf: bool,
    /// Use memory-mapped IO when searching files (rg --mmap). Already
    /// glep's default: files are searched straight from the page cache.
    #[arg(long)]
    pub mmap: bool,
    /// Never use memory maps: read every file through the searcher's
    /// buffer instead (rg --no-mmap)
    #[arg(long, overrides_with = "mmap")]
    pub no_mmap: bool,
    /// Accepted for script compat; glep streams output either way
    #[arg(long, hide = true)]
    pub block_buffered: bool,
    /// Generate shell completions or man page and exit (rg --generate)
    #[arg(long, value_name = "KIND", value_parser = ["man", "complete-bash", "complete-zsh", "complete-fish", "complete-powershell"])]
    pub generate: Option<String>,
    /// NUL is the line terminator (rg --null-data). A last-wins pair
    /// with --crlf, like rg.
    #[arg(long = "null-data", overrides_with = "crlf")]
    pub null_data: bool,
    /// DFA size limit for the regex engine; K, M and G suffixes accepted
    /// (rg --dfa-size-limit)
    #[arg(long, value_name = "NUM+SUFFIX?", value_parser = parse_human_usize)]
    pub dfa_size_limit: Option<usize>,
    /// Regex compiled-size limit; K, M and G suffixes accepted
    /// (rg --regex-size-limit)
    #[arg(long, value_name = "NUM+SUFFIX?", value_parser = parse_human_usize)]
    pub regex_size_limit: Option<usize>,
    /// Search inside gzip-compressed files by decompressing on the fly
    /// (rg -z/--search-zip; gzip only for now, other formats search raw)
    #[arg(short = 'z', long = "search-zip")]
    pub search_zip: bool,
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
    /// Replace the OS path separator in output paths: exactly one byte
    /// after escapes are decoded, "" keeps the default
    /// (rg --path-separator)
    #[arg(long = "path-separator", value_name = "SEP")]
    pub path_separator_raw: Option<String>,
    /// The resolved --path-separator byte (set in `run`).
    #[arg(skip)]
    pub path_separator: Option<u8>,
    /// With -c, print `path:0` lines for searched files with no matches
    /// (rg --include-zero). Needs the full walked set, so the index plan
    /// degenerates to All when combined with -c.
    #[arg(long)]
    pub include_zero: bool,
    /// Only search files at most N levels below each path operand
    /// (rg --max-depth/-d; a file operand itself is depth 0)
    #[arg(short = 'd', long, visible_alias = "maxdepth", alias = "depth")]
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
    /// (e.g. utf-16, latin1, shift_jis). `auto` (the default) transcodes
    /// files with a UTF-8/UTF-16 byte-order mark, `none` disables that.
    /// Unknown labels error out like the reference.
    #[arg(short = 'E', long = "encoding", overrides_with = "no_encoding")]
    pub encoding: Option<String>,
    /// Back to the default `-E auto` (rg --no-encoding)
    #[arg(long, overrides_with = "encoding")]
    pub no_encoding: bool,
    /// `-E none`: BOM sniffing off (set in `run`).
    #[arg(skip)]
    pub no_bom_sniffing: bool,
    /// Make `.` match newlines (dotall / (?s) regex mode). Independently
    /// settable like the reference; only visibly changes multiline (-U)
    /// searches since single-line scanning can't span lines anyway.
    #[arg(long)]
    pub multiline_dotall: bool,
    /// Flush stdout after every output record (rg --line-buffered).
    #[arg(long)]
    pub line_buffered: bool,
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
    /// Inverse of --heading for scripts that may turn it back off
    /// (rg --no-heading)
    #[arg(long = "no-heading", overrides_with = "heading", hide = true)]
    pub no_heading: bool,
    /// Suppress all output; the exit code alone reports whether a match
    /// exists. Like rg, --json still emits the closing summary event and
    /// --stats still prints its block, both counted over every file.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Skip the freshness sweep if the last one ran within this many seconds
    #[arg(long, default_value_t = 0)]
    pub ttl: u64,
    /// Index size cap per file (larger files are never indexed and are
    /// always scanned); K, M and G suffixes accepted
    #[arg(long, default_value = "1048576", value_name = "NUM+SUFFIX?", value_parser = parse_human_u64)]
    pub max_filesize: u64,
    // rg's negated and alias spellings, accepted for scripts and aliases.
    // Each pair is last-wins (clap's overrides_with); the ones that
    // interact with -u or with each other (ignore sources, --hidden, the
    // binary mode, the regex engine, unicode, sorting, buffering) are
    // resolved by command-line position in `resolve_flag_families`.
    #[arg(long, hide = true, overrides_with = "require_git")]
    pub no_require_git: bool,
    #[arg(long, hide = true)]
    pub no_hidden: bool,
    #[arg(long = "ignore", hide = true)]
    pub ignore_all: bool,
    #[arg(long, hide = true)]
    pub ignore_dot: bool,
    #[arg(long, hide = true)]
    pub ignore_vcs: bool,
    #[arg(long, hide = true)]
    pub ignore_exclude: bool,
    #[arg(long, hide = true)]
    pub ignore_global: bool,
    #[arg(long, hide = true)]
    pub ignore_parent: bool,
    /// Ignore every --ignore-file flag, even later ones (rg
    /// --no-ignore-files; --ignore-files undoes it)
    #[arg(long, hide = true)]
    pub no_ignore_files: bool,
    #[arg(long, hide = true, overrides_with = "no_ignore_files")]
    pub ignore_files: bool,
    #[arg(long, hide = true, overrides_with = "no_messages")]
    pub messages: bool,
    #[arg(long, hide = true, overrides_with = "no_ignore_messages")]
    pub ignore_messages: bool,
    #[arg(long, hide = true, overrides_with = "multiline")]
    pub no_multiline: bool,
    #[arg(long, hide = true, overrides_with = "multiline_dotall")]
    pub no_multiline_dotall: bool,
    #[arg(long, hide = true)]
    pub no_text: bool,
    #[arg(long, hide = true)]
    pub no_binary: bool,
    #[arg(long, hide = true)]
    pub no_pcre2: bool,
    #[arg(long, hide = true)]
    pub auto_hybrid_regex: bool,
    #[arg(long, hide = true)]
    pub no_auto_hybrid_regex: bool,
    #[arg(long, hide = true)]
    pub unicode: bool,
    #[arg(long, hide = true)]
    pub pcre2_unicode: bool,
    #[arg(long, hide = true)]
    pub no_pcre2_unicode: bool,
    #[arg(long, hide = true)]
    pub no_sort_files: bool,
    #[arg(long, hide = true)]
    pub no_line_buffered: bool,
    #[arg(long, hide = true)]
    pub no_block_buffered: bool,
    #[arg(long, hide = true, overrides_with = "follow")]
    pub no_follow: bool,
    #[arg(long, hide = true, overrides_with = "crlf")]
    pub no_crlf: bool,
    #[arg(long, hide = true, overrides_with = "fixed_strings")]
    pub no_fixed_strings: bool,
    #[arg(long, hide = true, overrides_with = "invert_match")]
    pub no_invert_match: bool,
    #[arg(long, hide = true, overrides_with = "json")]
    pub no_json: bool,
    #[arg(long, hide = true, overrides_with = "stats")]
    pub no_stats: bool,
    #[arg(long, hide = true, overrides_with = "trim")]
    pub no_trim: bool,
    #[arg(long, hide = true, overrides_with = "search_zip")]
    pub no_search_zip: bool,
    #[arg(long, hide = true, overrides_with = "column")]
    pub no_column: bool,
    #[arg(long, hide = true, overrides_with = "byte_offset")]
    pub no_byte_offset: bool,
    #[arg(long, hide = true, overrides_with = "include_zero")]
    pub no_include_zero: bool,
    #[arg(long, hide = true, overrides_with = "max_columns_preview")]
    pub no_max_columns_preview: bool,
    #[arg(long, hide = true, overrides_with = "one_file_system")]
    pub no_one_file_system: bool,
    #[arg(long, hide = true, overrides_with = "glob_case_insensitive")]
    pub no_glob_case_insensitive: bool,
    #[arg(long, hide = true, overrides_with = "ignore_file_case_insensitive")]
    pub no_ignore_file_case_insensitive: bool,
    #[arg(long, hide = true, overrides_with = "pre")]
    pub no_pre: bool,
    /// The resolved pattern list for a content search (set in `run`).
    #[arg(skip)]
    pub pattern_set: Vec<String>,
    /// Path operands that name files (root-relative, set in `run`): rg
    /// searches these unconditionally, with --binary's detection.
    #[arg(skip)]
    pub explicit_files: Vec<PathBuf>,
    /// rg's single-file heuristic, decided from the operands exactly as
    /// typed (set in `run` before missing operands are dropped): exactly
    /// one operand, and it is not a directory.
    #[arg(skip)]
    pub one_file_operand: bool,
    /// The type flags in command-line order (set in `run`).
    #[arg(skip)]
    pub type_changes: Vec<crate::filters::TypeChange>,
    /// --engine auto (set in `run`).
    #[arg(skip)]
    pub engine_auto: bool,
}

/// rg's human-readable sizes: digits with an optional K, M or G suffix
/// (powers of 1024).
fn parse_human_u64(s: &str) -> Result<u64, String> {
    let (digits, shift) = match s.as_bytes().last() {
        Some(b'K') => (&s[..s.len() - 1], 10),
        Some(b'M') => (&s[..s.len() - 1], 20),
        Some(b'G') => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid format for size '{s}', which should be a non-empty sequence of digits followed by an optional 'K', 'M' or 'G' suffix"));
    }
    let n: u64 = digits
        .parse()
        .map_err(|e| format!("invalid integer found in size '{s}': {e}"))?;
    n.checked_mul(1 << shift)
        .ok_or_else(|| format!("size too big in '{s}'"))
}

fn parse_human_usize(s: &str) -> Result<usize, String> {
    let n = parse_human_u64(s)?;
    usize::try_from(n).map_err(|_| format!("size too big in '{s}'"))
}

/// rg's escape decoding for separator flags (`\t`, `\n`, `\r`, `\0`,
/// `\\`, `\xNN`; anything else stays literal), via the same bstr routine
/// the reference uses.
fn unescape(s: &str) -> Vec<u8> {
    use bstr::ByteVec;
    Vec::unescape_bytes(s)
}

/// rg's --path-separator: decoded, empty means the default, otherwise it
/// must be exactly one byte.
fn parse_path_separator(s: Option<&str>) -> anyhow::Result<Option<u8>> {
    let Some(s) = s else {
        return Ok(None);
    };
    let raw = unescape(s);
    match raw.len() {
        0 => Ok(None),
        1 => Ok(Some(raw[0])),
        n => anyhow::bail!(
            "A path separator must be exactly one byte, but the given separator is {n} bytes: {s}\nIn some shells on Windows '/' is automatically expanded. Use '//' instead."
        ),
    }
}

fn build_glob_icase(g: &str, icase: bool) -> anyhow::Result<globset::GlobMatcher> {
    // gitignore semantics: a slash-free pattern matches at any depth;
    // once a pattern contains a slash, * must not cross separators.
    let pat = if g.contains('/') {
        g.to_string()
    } else {
        format!("**/{g}")
    };
    Ok(globset::GlobBuilder::new(&pat)
        .literal_separator(true)
        .case_insensitive(icase)
        .build()?
        .compile_matcher())
}

/// Lexically join a base dir and a relative path, folding `.`/`..`
/// components. Returns None when the result escapes above `base`'s own
/// root, i.e. the normalized path would start with `..` (an indexed
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
/// is `cwd` relative to `root` ("" when the same, or when the cwd lies
/// outside the tree): it is both the implicit search scope and the base
/// output paths are printed relative to.
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
            "{}: not an index directory (GLEP_INDEX_PATH)",
            idx_dir.display()
        );
        let idx_canon = std::fs::canonicalize(&idx_dir).unwrap_or(idx_dir);
        let root = idx_canon
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| idx_canon.clone());
        let cwd_rel = cwd_canon
            .strip_prefix(&root)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        return Ok((root, cwd_rel));
    }
    let mut dir = Some(cwd_canon.as_path());
    while let Some(d) = dir {
        if d.join(".glep").is_dir() {
            return Ok((
                d.to_path_buf(),
                cwd_canon
                    .strip_prefix(d)
                    .unwrap_or(Path::new(""))
                    .to_path_buf(),
            ));
        }
        dir = d.parent();
    }
    Ok((cwd_canon, PathBuf::new()))
}

/// A path the walk yielded relative to the cwd, as a root-relative path:
/// `.` and `..` fold lexically, and a path that climbs above the root
/// keeps its leading `..` components (display_rel turns it back into the
/// same cwd-relative path).
fn root_relative(cwd_rel: &Path, p: &Path) -> PathBuf {
    let mut parts: Vec<&std::ffi::OsStr> = cwd_rel.iter().collect();
    let mut ups = 0usize;
    for c in p.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s),
            std::path::Component::ParentDir => {
                if parts.pop().is_none() {
                    ups += 1;
                }
            }
            _ => {}
        }
    }
    let mut out = PathBuf::new();
    for _ in 0..ups {
        out.push("..");
    }
    for s in parts {
        out.push(s);
    }
    out
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

/// The patterns rg would search for: every -e value, then every line of
/// every -f file (`-f -` reads stdin; line terminators are stripped and
/// empty lines are kept, since an empty pattern matches every line, as
/// in rg), else the positional pattern. Empty only when -f files hold no
/// lines at all.
fn pattern_list(args: &Args) -> anyhow::Result<Vec<String>> {
    let mut pats: Vec<String> = args.regexp.clone();
    for f in &args.pattern_files {
        let text = if f.as_os_str() == "-" {
            std::io::read_to_string(std::io::stdin()).map_err(|e| anyhow::anyhow!("-: {e}"))?
        } else {
            std::fs::read_to_string(f).map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?
        };
        pats.extend(text.lines().map(str::to_string));
    }
    // -e/-f leave the positional slot as a path; only the bare
    // positional counts as a pattern when no -e/-f is present.
    if args.regexp.is_empty() && args.pattern_files.is_empty() {
        match &args.pattern {
            Some(p) => pats.push(p.clone()),
            None => anyhow::bail!("a pattern is required (or --files)"),
        }
    }
    Ok(pats)
}

/// Combine the resolved pattern list into one pattern string. Multiple
/// patterns OR together, each arm wrapped in a non-capturing group so
/// ^/$ stay arm-local. Under -F each arm is escaped individually and the
/// union is a regex: the matcher/planner see effective_fixed=false.
fn resolve_pattern(args: &Args) -> (String, bool) {
    let pats = &args.pattern_set;
    if pats.is_empty() {
        // Zero patterns only reach a search under -v (see the
        // matches_possible check in `run`): a pattern that never matches,
        // so every line is inverted in, as in rg.
        return (r"[^\s\S]".to_string(), false);
    }
    if pats.len() == 1 {
        return (pats[0].clone(), args.fixed_strings);
    }
    let fixed = args.fixed_strings;
    let joined = pats
        .iter()
        .map(|p| {
            if fixed {
                regex_syntax::escape(p)
            } else {
                p.clone()
            }
        })
        .map(|p| format!("(?:{p})"))
        .collect::<Vec<_>>()
        .join("|");
    (joined, false)
}

/// Report every path operand that doesn't exist on disk with rg's error
/// (an error like any other: exit 2 unless -q found a match). Returns
/// true if any path was missing.
fn report_missing_paths(paths: &[PathBuf], cwd: &Path, files_mode: bool) -> bool {
    let mut missing = false;
    for p in paths {
        if !cwd.join(p).exists() {
            missing = true;
            if files_mode {
                messages::error(format!(
                    "{}: IO error for operation on {}: No such file or directory (os error 2)",
                    p.display(),
                    p.display()
                ));
            } else {
                messages::error(format!(
                    "{}: No such file or directory (os error 2)",
                    p.display()
                ));
            }
        }
    }
    missing
}

/// --color WHEN resolution: `always`/`ansi` and `auto`+tty enable ANSI
/// output; `never` disables it (other values were rejected in `run`).
/// `auto` on a piped stream is the common path (agents capture stdout),
/// so it stays colorless there.
fn want_color(args: &Args) -> bool {
    match args.color.as_deref() {
        Some("always") | Some("ansi") => true,
        Some("auto") | None => {
            use std::io::IsTerminal;
            std::io::stdout().is_terminal()
        }
        _ => false,
    }
}

/// Reject output-styling values the reference rejects at flag-parse time
/// (exit 2): an unknown --color choice, a malformed --colors spec, or an
/// invalid --hyperlink-format.
fn validate_output_flags(args: &Args) -> anyhow::Result<()> {
    if let Some(c) = args.color.as_deref() {
        if !matches!(c, "never" | "auto" | "always" | "ansi") {
            anyhow::bail!("error parsing flag --color: choice '{c}' is unrecognized");
        }
    }
    for spec in &args.color_specs {
        if let Err(e) = spec.parse::<grep_printer::UserColorSpec>() {
            anyhow::bail!("error parsing flag --colors: {e}");
        }
    }
    if let Some(f) = args.hyperlink_format.as_deref() {
        if let Err(e) = f.parse::<grep_printer::HyperlinkFormat>() {
            anyhow::bail!("error parsing flag --hyperlink-format: {e}");
        }
    }
    Ok(())
}

/// rg's hyperlink setup: the --hyperlink-format template (or alias) with
/// `{host}` resolved from --hostname-bin's output, else the platform
/// hostname, and `{wslprefix}` under WSL. None when color is off (links
/// are a color-mode feature in the reference: its plain writer never
/// emits them), or when the format is empty (`none`). Called at most once
/// per run, so --hostname-bin's program runs at most once.
fn hyperlink_config(args: &Args) -> Option<grep_printer::HyperlinkConfig> {
    let fmt: grep_printer::HyperlinkFormat = args.hyperlink_format.as_deref()?.parse().ok()?;
    if fmt.is_empty() || !want_color(args) {
        return None;
    }
    let mut env = grep_printer::HyperlinkEnvironment::new();
    env.host(hyperlink_host(args.hostname_bin.as_deref()))
        .wsl_prefix(wsl_prefix());
    Some(fmt.into_config(env))
}

/// The `{host}` value: --hostname-bin's trimmed stdout when the program
/// runs, succeeds and prints something; otherwise the platform hostname
/// (gethostname), like the reference.
fn hyperlink_host(bin: Option<&str>) -> Option<String> {
    let platform = || hostname::get().ok().and_then(|h| h.into_string().ok());
    let Some(bin) = bin else {
        return platform();
    };
    let out = std::process::Command::new(bin)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => match String::from_utf8(o.stdout) {
            Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => platform(),
        },
        _ => platform(),
    }
}

/// `{wslprefix}`: `wsl$/<distro>` when running under WSL, as in rg.
fn wsl_prefix() -> Option<String> {
    if !cfg!(unix) {
        return None;
    }
    let distro = std::env::var_os("WSL_DISTRO_NAME")?;
    Some(format!("wsl$/{}", distro.to_str()?))
}

/// Which files a run covers, in rg's terms: the operands in command-line
/// order (the cwd scope when none was given) and rg's per-entry filters
/// (-g/--iglob, -t/-T, --ignore-file, hidden, --max-depth; see
/// filters.rs), all built once per run.
struct Scope {
    operands: Vec<filters::Operand>,
    filters: filters::Filters,
    /// No path operand was given: rg's implicit scope, the only one its
    /// nothing-searched warning applies to.
    implicit: bool,
}

impl Scope {
    fn new(args: &Args, root: &Path, cwd_rel: &Path, implicit: bool) -> anyhow::Result<Scope> {
        let filters = filters::Filters::new(
            &filters::FilterSpec {
                globs: &args.globs,
                iglobs: &args.iglobs,
                glob_case_insensitive: args.glob_case_insensitive,
                type_changes: &args.type_changes,
                ignore_files: &args.ignore_file,
                ignore_file_case_insensitive: args.ignore_file_case_insensitive,
                max_depth: args.max_depth,
                hidden: args.hidden,
            },
            root,
            cwd_rel,
        )?;
        // An explicit FILE operand is always searched, as in rg: ignore
        // rules, the hidden filter, binary flags, -g/-t/--ignore-file and
        // --max-depth never drop it, even when the index never held it.
        let operands = if args.paths.is_empty() {
            vec![filters::Operand {
                path: PathBuf::new(),
                kind: filters::OperandKind::Dir,
            }]
        } else {
            args.paths
                .iter()
                .map(|p| filters::Operand {
                    path: p.clone(),
                    kind: if args.explicit_files.contains(p) {
                        filters::OperandKind::File
                    } else {
                        filters::OperandKind::Dir
                    },
                })
                .collect()
        };
        Ok(Scope {
            operands,
            filters,
            implicit,
        })
    }

    /// rg's walker configuration for this run: the ignore sources the
    /// flags leave on, --hidden, -L, --one-file-system, --max-depth, and
    /// the run's globs, types and --ignore-file rules.
    fn live_config<'a>(filters: &'a filters::Filters, args: &'a Args) -> walk::LiveWalk<'a> {
        walk::LiveWalk {
            flags: walk::WalkFlags {
                dot: !args.no_ignore_dot,
                vcs: !args.no_ignore_vcs,
                exclude: !args.no_ignore_exclude,
                global: !args.no_ignore_global,
            },
            parents: !args.no_ignore_parent,
            require_git: args.require_git,
            show_hidden: args.hidden,
            follow: args.follow,
            one_file_system: args.one_file_system,
            max_depth: args.max_depth,
            overrides: filters.overrides(),
            types: filters.types(),
            ignore_files: &args.ignore_file,
            ignore_file_case_insensitive: args.ignore_file_case_insensitive,
        }
    }

    /// rg's walk of one directory operand, as root-relative paths.
    fn walk_operand(cfg: &walk::LiveWalk<'_>, op: &Path, cwd_rel: &Path) -> Vec<PathBuf> {
        // Walk the operand as rg is given it, relative to the cwd, so
        // globs and --ignore-file rules see rg's paths.
        let start = search::display_rel(op, cwd_rel);
        walk::live_walk(&start, cfg)
            .into_iter()
            .map(|p| root_relative(cwd_rel, &p))
            .collect()
    }

    /// The live escape hatches' file list: every operand walked by rg's
    /// rules (a file operand as itself), in command-line order.
    fn walk_all(&mut self, args: &Args, cwd_rel: &Path) -> Vec<PathBuf> {
        let cfg = Scope::live_config(&self.filters, args);
        let mut out = Vec::new();
        for op in &self.operands {
            match op.kind {
                filters::OperandKind::File => out.push(op.path.clone()),
                _ => out.extend(Scope::walk_operand(&cfg, &op.path, cwd_rel)),
            }
        }
        out
    }

    /// Walk live every directory operand the index cannot serve (`served`
    /// says it holds no file under it): an ignored directory, a symlink to
    /// a directory, a spelling that differs from the one on disk. rg walks
    /// those like any other operand, and the operand itself is never
    /// filtered. The root operand always comes from the index.
    fn walk_unserved(&mut self, args: &Args, cwd_rel: &Path, served: impl Fn(&Path) -> bool) {
        let cfg = Scope::live_config(&self.filters, args);
        for op in self.operands.iter_mut() {
            if matches!(op.kind, filters::OperandKind::Dir)
                && !op.path.as_os_str().is_empty()
                && !served(&op.path)
            {
                op.kind =
                    filters::OperandKind::Walked(Scope::walk_operand(&cfg, &op.path, cwd_rel));
            }
        }
    }

    /// Whether the implicit scope's walk would search at least one file
    /// (rg's nothing-searched check), stopping at the first file that
    /// survives the filters instead of building the list.
    fn searches_anything<'a>(&mut self, live: impl Iterator<Item = (&'a Path, bool)>) -> bool {
        let Some(op) = self.operands.first() else {
            return false;
        };
        match &op.kind {
            filters::OperandKind::File => true,
            filters::OperandKind::Walked(files) => !files.is_empty(),
            filters::OperandKind::Dir => {
                let base = op.path.components().count();
                let prefix = op.path.clone();
                let filters = &mut self.filters;
                let mut live = live;
                live.any(|(f, hidden)| {
                    (base == 0 || f.starts_with(&prefix)) && filters.keep(f, base, hidden)
                })
            }
        }
    }

    /// rg's file list from a path-sorted file set split by the sweep's
    /// hidden flag: operands in command-line order (path order within
    /// each, a file under two operands twice), each file filtered the way
    /// rg's walk of its operand would.
    fn select(&mut self, plain: Vec<PathBuf>, hidden: Vec<PathBuf>) -> Vec<PathBuf> {
        self.filters.select(&self.operands, plain, hidden)
    }
}

/// The --files glob (glep's pattern slot), matched against the path as
/// printed: relative to the cwd, like -g.
fn files_glob(args: &Args) -> anyhow::Result<Option<globset::GlobMatcher>> {
    match args.pattern.as_deref() {
        Some(g) => Ok(Some(build_glob_icase(g, args.glob_case_insensitive)?)),
        None => Ok(None),
    }
}

/// Keep the paths the --files glob matches, in order. Parallel: 86k
/// globset matches on a kernel-scale tree cost ~15ms serially, ~2ms
/// across the pool.
fn retain_glob(files: &mut Vec<PathBuf>, glob: Option<&globset::GlobMatcher>, cwd_rel: &Path) {
    if let Some(g) = glob {
        use rayon::prelude::*;
        *files = std::mem::take(files)
            .into_par_iter()
            .filter(|f| g.is_match(&*search::display_rel(f, cwd_rel)))
            .collect();
    }
}

/// Whether a path-sorted list holds a path under `prefix`: in component
/// order, everything under a directory sorts right after it.
fn sorted_has_under(list: &[PathBuf], prefix: &Path) -> bool {
    let i = list.partition_point(|x| x.as_path() < prefix);
    list.get(i).is_some_and(|x| x.starts_with(prefix))
}

/// Split read-only sweep extras by rg's hidden rule (whitelist rescue
/// included): (plain, hidden), both path-sorted. Under --hidden every
/// file is plain.
fn split_extra(root: &Path, extra: &[PathBuf], show_hidden: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
    if show_hidden {
        return (extra.to_vec(), Vec::new());
    }
    let mut wl = walk::WhitelistChecker::new();
    let (hidden, plain): (Vec<PathBuf>, Vec<PathBuf>) =
        extra.iter().cloned().partition(|p| wl.is_hidden(root, p));
    (plain, hidden)
}

/// Add `extra` to a path-sorted set, keeping it sorted and duplicate-free.
fn merge_extra(set: &mut Vec<PathBuf>, extra: Vec<PathBuf>) {
    if !extra.is_empty() {
        set.extend(extra);
        set.sort();
        set.dedup();
    }
}

/// The effective --sort/--sortr request: (field, descending). clap's
/// overrides_with keeps at most one of the two flags set (last wins).
fn sort_request(args: &Args) -> Option<(&str, bool)> {
    match (&args.sort, &args.sortr) {
        (Some(s), _) => Some((s.as_str(), false)),
        (None, Some(s)) => Some((s.as_str(), true)),
        (None, None) => None,
    }
}

fn validate_sort(args: &Args) -> anyhow::Result<()> {
    if let Some((mode, _)) = sort_request(args) {
        match mode {
            "path" | "none" | "modified" | "accessed" | "created" => {}
            other => anyhow::bail!(
                "{other}: unsupported sort field (path|modified|accessed|created|none)"
            ),
        }
    }
    Ok(())
}

/// Whether the current sort request keeps glep's natural order (path
/// ascending, which `none` is free to use too) or a plain reversal of it.
/// Only these can be served by the listing cache.
#[cfg(target_os = "macos")]
fn sort_is_path_order(args: &Args) -> bool {
    matches!(
        sort_request(args),
        None | Some(("path", _)) | Some(("none", _))
    )
}

/// Walk positions in rg's own traversal order: rg sorts by timestamp with
/// a stable sort over a single-threaded, UNSORTED walk (sorting forces
/// one thread), so files with equal timestamps keep directory-entry order
/// (readdir order: hash order on ext4/APFS, case-insensitive name order
/// on NTFS). The same `ignore` walk over the same operands reproduces
/// that order exactly; filtering never reorders survivors, so walking with
/// glep's own ignore settings is enough to rank every candidate.
fn walk_positions(
    root: &Path,
    cwd_rel: &Path,
    args: &Args,
) -> std::collections::HashMap<PathBuf, usize> {
    let mut operands: Vec<PathBuf> = args.paths.iter().map(|p| root.join(p)).collect();
    if operands.is_empty() {
        operands.push(root.join(cwd_rel));
    }
    let mut wb = ignore::WalkBuilder::new(&operands[0]);
    for op in &operands[1..] {
        wb.add(op);
    }
    let respect = !args.no_ignore;
    wb.require_git(false)
        .hidden(false)
        .ignore(respect && !args.no_ignore_dot)
        .git_ignore(respect && !args.no_ignore_vcs)
        .git_exclude(respect && !args.no_ignore_exclude)
        .git_global(respect && !args.no_ignore_global)
        .parents(respect && !args.no_ignore_parent)
        .follow_links(args.follow)
        .same_file_system(args.one_file_system)
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != ".glep");
    let mut pos = std::collections::HashMap::new();
    for ent in wb.build().flatten() {
        if ent.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let rel = ent
            .path()
            .strip_prefix(root)
            .unwrap_or(ent.path())
            .to_path_buf();
        let next = pos.len();
        pos.entry(rel).or_insert(next);
    }
    pos
}

/// Sort the file list per --sort/--sortr, reproducing rg's ordering
/// exactly. `files` arrives in path order. `path` ascending (and `none`,
/// whose order rg leaves unspecified) keep it: Rust's component-wise Path
/// order is rg's per-directory file-name order. `path` descending
/// reverses it. Timestamp fields read live metadata (like rg) and break
/// ties by rg's walk order (see `walk_positions`); a file whose timestamp
/// cannot be read sorts last ascending and first descending, as in rg.
fn apply_sort(
    files: &mut Vec<PathBuf>,
    args: &Args,
    root: &Path,
    cwd_rel: &Path,
) -> anyhow::Result<()> {
    let Some((mode, reverse)) = sort_request(args) else {
        return Ok(());
    };
    match mode {
        "none" => {}
        "path" => {
            if reverse {
                // rg collects every result and sorts it descending, across
                // operands (stable, so duplicates stay adjacent).
                files.sort_by(|a, b| b.cmp(a));
            }
        }
        "modified" | "accessed" | "created" => {
            let positions = walk_positions(root, cwd_rel, args);
            let mut keyed: Vec<(Option<std::time::SystemTime>, usize, PathBuf)> = files
                .drain(..)
                .map(|f| {
                    let t = std::fs::metadata(root.join(&f)).ok().and_then(|m| {
                        match mode {
                            "modified" => m.modified(),
                            "accessed" => m.accessed(),
                            _ => m.created(),
                        }
                        .ok()
                    });
                    let p = positions.get(&f).copied().unwrap_or(usize::MAX);
                    (t, p, f)
                })
                .collect();
            keyed.sort_by(|a, b| {
                use std::cmp::Ordering;
                let o = match (a.0, b.0) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                };
                let o = if reverse { o.reverse() } else { o };
                o.then(a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2))
            });
            files.extend(keyed.into_iter().map(|(_, _, f)| f));
        }
        other => {
            anyhow::bail!("{other}: unsupported sort field (path|modified|accessed|created|none)")
        }
    }
    Ok(())
}

/// Print `files` one record per path into ONE buffered write: stdout is a
/// LineWriter, so a per-path println! costs one flush syscall per line
/// (27k syscalls on a kernel-scale .c listing). Paths print relative to
/// the cwd, with --path-separator, the -0 terminator and, under --color,
/// rg's path color and hyperlinks applied (rg's --files printer).
fn print_paths(files: &[PathBuf], cwd_rel: &Path, args: &Args) -> anyhow::Result<()> {
    let term = if args.null { 0 } else { b'\n' };
    let style = search::PathStyle::with(
        args.path_separator,
        want_color(args),
        &args.color_specs,
        hyperlink_config(args).as_ref(),
    );
    let plain = style.is_plain();
    let mut buf = Vec::with_capacity(files.len() * 48);
    for f in files {
        let d = search::display_rel(f, cwd_rel);
        if plain {
            // Raw bytes: Path::display().to_string() goes through the fmt
            // machinery per path (~0.7us each, ~20ms for a kernel-scale .c
            // listing); the bytes are identical for the paths we emit.
            buf.extend_from_slice(d.as_os_str().as_encoded_bytes());
        } else {
            style.write(&mut buf, &d);
        }
        buf.push(term);
    }
    let mut out = search::StdoutOut;
    out.write_all(&buf)?;
    out.flush()?;
    Ok(())
}

/// -a/--binary/--null-data: the searcher's binary-detection mode.
/// last-wins ordering between -a and --binary matters (rg semantics).
fn binary_detection(args: &Args) -> grep_searcher::BinaryDetection {
    if args.text {
        grep_searcher::BinaryDetection::none()
    } else if args.binary {
        grep_searcher::BinaryDetection::convert(b'\x00')
    } else {
        grep_searcher::BinaryDetection::quit(b'\x00')
    }
}

/// rg's with-filename rule: -H/-I decide when given (last wins); else
/// --vimgrep always prints the path, and otherwise a single operand that
/// is not a directory prints bare matches. The operand count is the one
/// typed, so `glep pat file missing` still prints `file:` prefixes.
fn with_filename(args: &Args) -> bool {
    if args.with_filename {
        return true;
    }
    if args.no_filename {
        return false;
    }
    args.vimgrep || !args.one_file_operand
}

/// Full SearchOpts builder: every flag mode in one place. `cwd_rel`
/// feeds the display-prefix strip.
fn search_opts(args: &Args, cwd_rel: &Path) -> search::SearchOpts {
    search::SearchOpts {
        case_insensitive: args.ignore_case,
        engine_auto: args.engine_auto,
        bom_sniffing: !args.no_bom_sniffing,
        fixed: args.fixed_strings,
        files_with_matches: args.files_with_matches,
        files_without_match: args.files_without_match,
        before: args.before_context.or(args.context).unwrap_or(0),
        after: args.after_context.or(args.context).unwrap_or(0),
        json: args.json,
        count: args.count,
        count_matches: args.count_matches,
        multiline: args.multiline,
        binary: binary_detection(args),
        display_prefix: cwd_rel.to_path_buf(),
        column: args.column || args.vimgrep,
        byte_offset: args.byte_offset,
        vimgrep: args.vimgrep,
        trim: args.trim,
        path_terminator: args.null.then_some(0u8),
        path_separator: args.path_separator,
        include_zero: args.include_zero,
        with_filename: with_filename(args),
        stats: args.stats,
        multiline_dotall: args.multiline_dotall,
        encoding: args.encoding.clone(),
        line_buffered: args.line_buffered,
        color: want_color(args),
        color_specs: args.color_specs.clone(),
        passthru: args.passthru,
        unicode: !args.no_unicode,
        null_data: args.null_data,
        dfa_size_limit: args.dfa_size_limit,
        regex_size_limit: args.regex_size_limit,
        field_match_separator: args.field_match_separator.as_deref().map(unescape),
        field_context_separator: args.field_context_separator.as_deref().map(unescape),
        context_separator: if args.no_context_separator {
            None
        } else {
            Some(
                args.context_separator
                    .as_deref()
                    .map_or_else(|| b"--".to_vec(), unescape),
            )
        },
        pcre2: args.pcre2,
        search_zip: args.search_zip,
        word: args.word_regexp,
        line_regexp: args.line_regexp,
        smart_case: args.smart_case,
        invert: args.invert_match,
        max_count: args.max_count,
        // -M0 means no limit in rg.
        max_columns: args.max_columns.filter(|&n| n > 0),
        line_number: !args.no_line_number,
        // rg never prints headings in --vimgrep's one-line records.
        heading: args.heading && !args.vimgrep,
        only_matching: args.only_matching,
        quiet: args.quiet,
        max_columns_preview: args.max_columns_preview,
        stop_on_nonmatch: args.stop_on_nonmatch,

        hyperlink: hyperlink_config(args),
        threads: args.threads,
        replace: args.replace.as_deref().map(|s| s.as_bytes().to_vec()),
        pre: args.pre.clone(),
        // validated in run(), so a parse failure cannot reach here
        pre_glob: args
            .pre_glob
            .as_deref()
            .and_then(|g| build_glob_icase(g, false).ok()),
        crlf: args.crlf,
        no_mmap: args.no_mmap,
        explicit_files: args.explicit_files.iter().cloned().collect(),
    }
}

/// rg's exit status, shared by every path: 0 on a hit (even after
/// errors, as long as -q asked only whether something matches), 2 when
/// anything errored (a missing operand, an unreadable file or directory,
/// a failing --pre command, nothing searched at all), else 1.
fn exit_code(args: &Args, had_error: bool, hit: bool) -> i32 {
    let errored = had_error || messages::errored();
    if hit && (args.quiet || !errored) {
        0
    } else if errored {
        2
    } else {
        1
    }
}

/// Every live-scan escape hatch: --no-ignore and the -u ladder, the
/// --no-ignore-* toggles, -L, --one-file-system, --require-git outside a
/// repo, and --no-index. The index never holds what these flags add
/// (ignored trees, files behind links, other filesystems) and must never
/// learn about them, so this never opens, updates or writes it: each
/// operand is walked live with rg's own walker configuration, rooted at
/// the operand like rg (so --no-ignore-parent drops the ignore files
/// above it). The walker applies every filter, so its result is the file
/// list; the cost is rg's, every time.
fn run_live(
    root: &Path,
    cwd_rel: &Path,
    args: &Args,
    scope: &mut Scope,
    timings: &mut Timings,
    had_error: bool,
) -> anyhow::Result<i32> {
    let mut files = scope.walk_all(args, cwd_rel);
    timings.stage("live_walk");
    if args.files {
        // With --files the pattern slot is the glob.
        retain_glob(&mut files, files_glob(args)?.as_ref(), cwd_rel);
        apply_sort(&mut files, args, root, cwd_rel)?;
        print_paths(&files, cwd_rel, args)?;
        timings.finish();
        return Ok(exit_code(args, had_error, !files.is_empty()));
    }

    let (pattern, effective_fixed) = resolve_pattern(args);
    apply_sort(&mut files, args, root, cwd_rel)?;
    timings.stage("candidates");
    let nothing_searched = scope.implicit && files.is_empty();
    if nothing_searched {
        report_nothing_searched();
    }
    let mut opts = search_opts(args, cwd_rel);
    opts.fixed = effective_fixed;
    let mut out = search::StdoutOut;
    let found = search::run(&pattern, root, &files, None, &opts, &mut out)?;
    timings.stage("search");
    timings.finish();
    Ok(exit_code(args, had_error || nothing_searched, found))
}

/// rg's nothing-searched warning: with no path operand, a walk that
/// yields no file at all (filters ate everything) is an error, exit 2.
fn report_nothing_searched() {
    messages::error(
        "No files were searched, which means glep probably applied a filter you didn't expect.\nRunning with --debug will show why files are being skipped.",
    );
}

/// True when no flag changes what the listing cache would list or the
/// order it is printed in. Every flag that changes listing SEMANTICS
/// (--no-ignore and -u/-uu/-uuu, the --no-ignore-* toggles, -L,
/// --one-file-system, --require-git outside a repo, --no-index) takes a
/// live-scan escape hatch before the cache is ever consulted; the
/// remaining filters (-g, -t/-T/--type-add/--type-clear, --ignore-file,
/// --max-depth, path operands, --hidden, the glob slot) are pure
/// functions of the path applied identically after either listing.
/// Timestamp sorts need a live walk order and live metadata, so they
/// bypass the cache.
#[cfg(target_os = "macos")]
fn listing_cache_eligible(args: &Args) -> bool {
    sort_is_path_order(args)
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
fn listing_via_cache(
    root: &Path,
    index_dir: &Path,
    timings: &mut Timings,
) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
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
    // The sweep's hidden flag splits the result, like the index's
    // live_files_split.
    use rayon::prelude::*;
    let (hidden, plain): (Vec<PathBuf>, Vec<PathBuf>) = outcome
        .files
        .par_iter()
        .map(|m| (m.hidden, m.path.clone()))
        .partition_map(|(h, p)| {
            if h {
                rayon::iter::Either::Left(p)
            } else {
                rayon::iter::Either::Right(p)
            }
        });
    timings.stage("listing_cache_walk");
    if std::env::var_os("GLEP_TIMING").is_some() {
        eprintln!(
            "glep timing: cache load {:.1}ms, pruned walk+filter {:.1}ms",
            t1.duration_since(t0).as_secs_f64() * 1000.0,
            t1.elapsed().as_secs_f64() * 1000.0
        );
    }
    Some((plain, hidden))
}

/// Every --type-add, --type-clear, -t and -T in command-line order (rg
/// applies type changes in that order).
fn type_changes(matches: &clap::ArgMatches) -> Vec<filters::TypeChange> {
    let mut changes: Vec<(usize, filters::TypeChange)> = Vec::new();
    let mut collect = |id: &str, make: fn(String) -> filters::TypeChange| {
        if let (Some(values), Some(at)) = (matches.get_many::<String>(id), matches.indices_of(id)) {
            changes.extend(at.zip(values).map(|(i, v)| (i, make(v.clone()))));
        }
    };
    collect("type_add", filters::TypeChange::Add);
    collect("type_clear", filters::TypeChange::Clear);
    collect("types", filters::TypeChange::Select);
    collect("types_not", filters::TypeChange::Negate);
    changes.sort_by_key(|(i, _)| *i);
    changes.into_iter().map(|(_, c)| c).collect()
}

/// Where a flag last appeared on the command line (None when it was not
/// given there: defaults carry indices too, so they are filtered out).
/// Flags that interact the way rg's do, each acting where it stands, are
/// resolved by comparing these positions.
fn last_index(matches: &clap::ArgMatches, id: &str) -> Option<usize> {
    if matches.value_source(id) != Some(clap::parser::ValueSource::CommandLine) {
        return None;
    }
    matches.indices_of(id).and_then(|i| i.max())
}

/// rg resolves these flags by position: each acts where it stands, so the
/// last writer of a setting wins even across different flags (`-uu
/// --no-hidden`, `--no-ignore --ignore-vcs`, `-a -uuu`, `-P --engine
/// default`, `--sort path --no-sort-files`). The -u counter carries only
/// its last position, so all of its levels act there (the usual `-uu` or
/// `-uuu` cluster).
fn resolve_flag_families(args: &mut Args, matches: &clap::ArgMatches) -> anyhow::Result<()> {
    /// The value of the last writer among `(position, value)` pairs, or
    /// `default` when none of them was given.
    fn last<T: Copy>(default: T, writers: &[(Option<usize>, T)]) -> T {
        writers
            .iter()
            .filter_map(|&(at, v)| at.map(|at| (at, v)))
            .max_by_key(|&(at, _)| at)
            .map_or(default, |(_, v)| v)
    }
    let at = |id: &str| last_index(matches, id);
    let u = at("unrestricted");
    let level = |n: u8| u.filter(|_| args.unrestricted >= n);

    // Ignore sources: --no-ignore and -u turn all five off, --ignore
    // turns them back on, each --no-ignore-X / --ignore-X flips one.
    let all_off = at("no_ignore").max(level(1));
    let all_on = at("ignore_all");
    let respected = |no: &str, yes: &str| {
        last(
            true,
            &[
                (all_off, false),
                (all_on, true),
                (at(no), false),
                (at(yes), true),
            ],
        )
    };
    let dot = respected("no_ignore_dot", "ignore_dot");
    let vcs = respected("no_ignore_vcs", "ignore_vcs");
    let exclude = respected("no_ignore_exclude", "ignore_exclude");
    let global = respected("no_ignore_global", "ignore_global");
    let parent = respected("no_ignore_parent", "ignore_parent");
    args.no_ignore_dot = !dot;
    args.no_ignore_vcs = !vcs;
    args.no_ignore_exclude = !exclude;
    args.no_ignore_global = !global;
    args.no_ignore_parent = !parent;
    args.no_ignore = !(dot || vcs || exclude || global || parent);

    // -uu adds --hidden.
    args.hidden = last(
        false,
        &[
            (at("hidden"), true),
            (at("no_hidden"), false),
            (level(2), true),
        ],
    );

    // The binary mode: -a/--text, --binary (and -uuu), or rg's default
    // (--no-text and --no-binary both go back to it).
    #[derive(Clone, Copy, PartialEq)]
    enum Bin {
        Quit,
        Text,
        Convert,
    }
    let bin = last(
        Bin::Quit,
        &[
            (at("text"), Bin::Text),
            (at("no_text"), Bin::Quit),
            (at("binary"), Bin::Convert),
            (at("no_binary"), Bin::Quit),
            (level(3), Bin::Convert),
        ],
    );
    args.text = bin == Bin::Text;
    args.binary = bin == Bin::Convert;

    // The regex engine: -P, --no-pcre2, --engine, and the deprecated
    // --auto-hybrid-regex pair. --engine's value is checked wherever it
    // stands, like rg's flag parser does.
    #[derive(Clone, Copy, PartialEq)]
    enum Engine {
        Default,
        Pcre2,
        Auto,
    }
    let engine = match args.engine.as_deref() {
        None => Engine::Default,
        Some("default") => Engine::Default,
        Some("pcre2") => Engine::Pcre2,
        // rg's hybrid mode: decided per pattern when the matcher is built
        // (search::run falls back to PCRE2).
        Some("auto") => Engine::Auto,
        Some(other) => anyhow::bail!("unrecognized regex engine '{other}'"),
    };
    let engine = last(
        Engine::Default,
        &[
            (at("pcre2"), Engine::Pcre2),
            (at("no_pcre2"), Engine::Default),
            (at("engine"), engine),
            (at("auto_hybrid_regex"), Engine::Auto),
            (at("no_auto_hybrid_regex"), Engine::Default),
        ],
    );
    args.pcre2 = engine == Engine::Pcre2;
    args.engine_auto = engine == Engine::Auto;

    // Unicode, with the deprecated --pcre2-unicode spellings.
    args.no_unicode = !last(
        true,
        &[
            (at("no_unicode"), false),
            (at("unicode"), true),
            (at("pcre2_unicode"), true),
            (at("no_pcre2_unicode"), false),
        ],
    );

    // Sorting: --sort, --sortr, and the deprecated --sort-files pair
    // (--no-sort-files turns sorting off again).
    #[derive(Clone, Copy)]
    enum Sort {
        Unset,
        Sort,
        Sortr,
        Path,
        Off,
    }
    match last(
        Sort::Unset,
        &[
            (at("sort"), Sort::Sort),
            (at("sortr"), Sort::Sortr),
            (at("sort_files"), Sort::Path),
            (at("no_sort_files"), Sort::Off),
        ],
    ) {
        Sort::Unset => {}
        Sort::Sort => args.sortr = None,
        Sort::Sortr => args.sort = None,
        Sort::Path => {
            args.sort = Some("path".to_string());
            args.sortr = None;
        }
        Sort::Off => {
            args.sort = None;
            args.sortr = None;
        }
    }

    // Output buffering: only line buffering changes what glep does.
    args.line_buffered = last(
        false,
        &[
            (at("line_buffered"), true),
            (at("no_line_buffered"), false),
            (at("block_buffered"), false),
            (at("no_block_buffered"), false),
        ],
    );

    // --no-ignore-files drops every --ignore-file, even later ones.
    if args.no_ignore_files {
        args.ignore_file.clear();
    }
    Ok(())
}

pub fn run() -> anyhow::Result<i32> {
    use clap::{CommandFactory, FromArgMatches};
    let matches = Args::command().get_matches();
    let mut args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    messages::configure(!args.no_messages, !args.no_ignore_messages);
    if let Some(n) = args.threads {
        // Global rayon pool (sweeps, filters); the search pool reads
        // SearchOpts::threads. A second init keeps the first (only ever
        // happens under unit tests calling run() twice).
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }

    // With -e/-f the positional pattern slot is free; a bare positional
    // there is a path (e.g. `glep -e foo src`, `glep -f pats src`).
    if (!args.regexp.is_empty() || !args.pattern_files.is_empty()) && !args.files {
        if let Some(p) = args.pattern.take() {
            args.paths.insert(0, PathBuf::from(p));
        }
    }
    resolve_flag_families(&mut args, &matches)?;
    args.type_changes = type_changes(&matches);

    // Commands that print and exit before any tree work.
    // --generate emits shell completions or a man page.
    if let Some(kind) = &args.generate {
        use clap::CommandFactory;
        let mut cmd = Args::command();
        let mut out = Vec::new();
        match kind.as_str() {
            "man" => {
                let man = clap_mangen::Man::new(cmd);
                man.render(&mut out)?;
            }
            sh => {
                let shell = match sh {
                    "complete-bash" => clap_complete::Shell::Bash,
                    "complete-zsh" => clap_complete::Shell::Zsh,
                    "complete-fish" => clap_complete::Shell::Fish,
                    "complete-powershell" => clap_complete::Shell::PowerShell,
                    _ => anyhow::bail!("unsupported --generate kind: {sh}"),
                };
                clap_complete::generate(shell, &mut cmd, "glep", &mut out);
            }
        }
        std::io::stdout().write_all(&out)?;
        return Ok(0);
    }
    if args.type_list {
        // rg's --type-list reflects --type-add and --type-clear, applied
        // in command-line order.
        let mut tb = ignore::types::TypesBuilder::new();
        tb.add_defaults();
        for change in &args.type_changes {
            match change {
                filters::TypeChange::Add(spec) => {
                    tb.add_def(spec)
                        .map_err(|e| anyhow::anyhow!("{spec}: {e}"))?;
                }
                filters::TypeChange::Clear(t) => {
                    tb.clear(t);
                }
                _ => {}
            }
        }
        let mut defs = tb.definitions();
        defs.sort_by(|a, b| a.name().cmp(b.name()));
        let mut buf = String::new();
        for d in defs {
            buf.push_str(&format!("{}: {}\n", d.name(), d.globs().join(", ")));
        }
        print!("{buf}");
        return Ok(0);
    }
    if args.pcre2_version {
        let jit = if grep_pcre2::is_jit_available() {
            " (JIT is available)"
        } else {
            ""
        };
        let (maj, min) = grep_pcre2::version();
        println!("PCRE2 {}.{:02} is available{}", maj, min, jit);
        return Ok(0);
    }

    // Flag folding and validation: everything rg rejects while parsing
    // flags exits 2 here, before any tree or index work.
    args.path_separator = parse_path_separator(args.path_separator_raw.as_deref())?;
    // Compat spellings fold into their canonical flags.
    if args.print0 {
        args.null = true;
    }
    if args.pretty {
        // -p is rg's alias for --color always --heading --line-number, so
        // each of those settings only holds unless a flag given AFTER -p
        // changes it again (`-p --color never` keeps the heading).
        let p = last_index(&matches, "pretty");
        if last_index(&matches, "color") < p {
            args.color = Some("always".to_string());
        }
        if last_index(&matches, "no_heading") < p {
            args.heading = true;
            args.no_heading = false;
        }
        if last_index(&matches, "no_line_number") < p {
            args.line_number = true;
            args.no_line_number = false;
        }
    }
    if args.no_heading {
        args.heading = false;
    }
    // --null-data implies -a (NUL is data) and overrides --binary.
    if args.null_data {
        args.text = true;
        args.binary = false;
    }
    // -E: `auto` is the default (BOM sniffing), `none` turns sniffing
    // off; any other label must name a real encoding.
    if args.no_encoding {
        args.encoding = None;
    }
    match args.encoding.as_deref() {
        Some("auto") => args.encoding = None,
        Some("none") => {
            args.encoding = None;
            args.no_bom_sniffing = true;
        }
        Some(label) => {
            grep_searcher::Encoding::new(label).map_err(|e| anyhow::anyhow!("{label}: {e}"))?;
        }
        None => {}
    }
    validate_sort(&args)?;
    validate_output_flags(&args)?;
    if let Some(g) = &args.pre_glob {
        build_glob_icase(g, false)?;
    }

    // Content searches resolve their patterns now (reading -f files),
    // then apply rg's matches_possible rule: with no pattern at all
    // (and no -v) or -m 0 nothing can ever match, so rg exits 1 without
    // walking, printing or even checking path operands.
    let subcommand = args.regexp.is_empty()
        && args.pattern_files.is_empty()
        && !args.files
        && !args.no_ignore
        && matches!(args.pattern.as_deref(), Some("index") | Some("status"));
    if !args.files && !subcommand {
        args.pattern_set = pattern_list(&args)?;
        if args.max_count == Some(0) || (args.pattern_set.is_empty() && !args.invert_match) {
            return Ok(1);
        }
    }

    let cwd = std::env::current_dir()?;
    let (root, cwd_rel) = discover_index_root(&cwd)?;
    args.one_file_operand = args.paths.len() == 1 && !cwd.join(&args.paths[0]).is_dir();
    // Missing path filters are errors (exit 2), checked against the cwd
    // before paths are relativized into the index tree.
    let had_error = report_missing_paths(&args.paths, &cwd, args.files);
    // Missing operands were already reported: drop them so the sweeps
    // don't re-error on the same paths (search continues with the valid
    // ones, matching the reference's report-and-continue contract).
    if had_error {
        args.paths.retain(|p| cwd.join(p).exists());
        if args.paths.is_empty() && !subcommand {
            // Every operand was missing: rg searches nothing at all (not
            // the cwd), still prints the --json summary or --stats block,
            // and exits 2.
            if !args.files {
                let (pattern, fixed) = resolve_pattern(&args);
                let mut opts = search_opts(&args, &cwd_rel);
                opts.fixed = fixed;
                search::run(&pattern, &root, &[], None, &opts, &mut search::StdoutOut)?;
            }
            return Ok(2);
        }
    }
    normalize_path_filters(&mut args.paths, &root, &cwd_rel);
    args.explicit_files = args
        .paths
        .iter()
        .filter(|p| root.join(p).is_file())
        .cloned()
        .collect();
    // No explicit paths: the implicit scope is the cwd subtree (rg's
    // default `.`). At the discovered root itself this is "": everything.
    // Only an implicit scope can trip rg's nothing-searched warning.
    let implicit = args.paths.is_empty();
    if args.paths.is_empty() && !cwd_rel.as_os_str().is_empty() {
        args.paths.push(cwd_rel.clone());
    }
    if args.debug || args.trace {
        eprintln!(
            "glep: DEBUG cwd={} root={} threads={} pattern={:?}",
            cwd.display(),
            root.display(),
            args.threads
                .map(|t| t.to_string())
                .unwrap_or_else(|| "auto".into()),
            args.pattern
        );
    }

    // Subcommand-style words in the pattern slot.
    if subcommand {
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
                    .filter(|e| {
                        e.flags & (FLAG_SKIP_BINARY | FLAG_SKIP_TOO_LARGE | FLAG_TRANSCODE) != 0
                    })
                    .count();
                println!("files: {live}");
                println!("skipped (binary/oversized/utf16): {skipped}");
                println!("last sweep epoch: {}", idx.manifest.last_sweep_epoch);
                return Ok(0);
            }
            _ => {}
        }
    }

    let mut timings = Timings::new();
    let mut scope = Scope::new(&args, &root, &cwd_rel, implicit)?;

    // Live-scan escape hatches, which never read or write the index:
    // --no-ignore and the -u ladder, the --no-ignore-* toggles (files
    // excluded only by a disabled source aren't in the manifest),
    // --require-git outside a repo (git rules go inert), and
    // --one-file-system, -L and --no-index (no narrowing at all).
    if args.no_ignore
        || args.no_ignore_dot
        || args.no_ignore_vcs
        || args.no_ignore_exclude
        || args.no_ignore_global
        || args.no_ignore_parent
        || (args.require_git && walk::find_git_root(&root).is_none())
        || args.one_file_system
        || args.follow
        || args.no_index
    {
        return run_live(&root, &cwd_rel, &args, &mut scope, &mut timings, had_error);
    }

    // Dir-mtime cached listing (macOS only), consulted BEFORE even
    // opening the index: this path needs no manifest, no postings, no
    // freshness sweep, just the cache and the pruned walk. The index
    // opens below only on cache miss. The cache lives in the discovered
    // index dir, so subdirectory runs and GLEP_INDEX_PATH share it; the
    // cwd scope and every other filter apply after the listing.
    #[cfg(target_os = "macos")]
    if args.files && listing_cache_eligible(&args) {
        if let Some((mut plain, mut hidden)) =
            listing_via_cache(&root, &root.join(".glep"), &mut timings)
        {
            // Path order first: the scope groups by operand from it.
            use rayon::prelude::*;
            if args.hidden {
                plain.append(&mut hidden);
            }
            plain.par_sort();
            hidden.sort();
            scope.walk_unserved(&args, &cwd_rel, |op| {
                sorted_has_under(&plain, op) || sorted_has_under(&hidden, op)
            });
            let mut files = scope.select(plain, hidden);
            retain_glob(&mut files, files_glob(&args)?.as_ref(), &cwd_rel);
            apply_sort(&mut files, &args, &root, &cwd_rel)?;
            print_paths(&files, &cwd_rel, &args)?;
            timings.stage("listing_cache");
            timings.finish();
            return Ok(exit_code(&args, had_error, !files.is_empty()));
        }
    }

    let mut idx = Index::open_or_build(&root, args.max_filesize)?;
    timings.stage("index_open");
    // Path filters scope the freshness sweep too: subtrees outside the
    // filter can't produce results, so sweeping them is wasted work.
    let extra = if args.paths.is_empty() {
        idx.update_timed(args.max_filesize, args.ttl, &mut timings)?
    } else {
        idx.update_scoped(args.max_filesize, args.ttl, &args.paths, &mut timings)?
    };
    // `extra` is the read-only-mode live-scan fallback: files discovered by
    // this sweep that couldn't be written into the index because another
    // process holds the lock. They carry no FLAG_HIDDEN of their own (no
    // manifest entry yet), so the same rule is computed here, whitelist
    // rescue included.
    let (extra_plain, extra_hidden) = split_extra(&root, &extra, args.hidden);
    scope.walk_unserved(&args, &cwd_rel, |op| idx.has_files_under(op));
    timings.stage("operands");
    // Every live file (the --files listing, the --files-without-match
    // universe), split by the hidden flag.
    let live_split = || {
        let (mut plain, mut hidden) = idx.live_files_split(args.hidden);
        merge_extra(&mut plain, extra_plain.clone());
        merge_extra(&mut hidden, extra_hidden.clone());
        (plain, hidden)
    };

    if args.files {
        let (plain, hidden) = live_split();
        let mut files = scope.select(plain, hidden);
        // With --files the pattern slot is the glob.
        retain_glob(&mut files, files_glob(&args)?.as_ref(), &cwd_rel);
        apply_sort(&mut files, &args, &root, &cwd_rel)?;
        print_paths(&files, &cwd_rel, &args)?;
        timings.finish();
        // Rebuild the listing cache from the authoritative sweep (macOS
        // only). Only a full, written sweep certifies the whole manifest:
        // a --ttl skip, a scoped sweep, or a read-only (lock contended)
        // pass can leave entries stale, and stale entries must never be
        // re-certified by current directory mtimes.
        #[cfg(target_os = "macos")]
        {
            if idx.swept_full() && extra.is_empty() {
                let listing = idx.live_file_metas(true);
                let tree = crate::dircache::build_from_listing(
                    &root,
                    &listing,
                    crate::dircache::source_fingerprint(&root),
                );
                let _ = tree.save(&idx.dir);
            }
        }
        return Ok(exit_code(&args, had_error, !files.is_empty()));
    }

    let (pattern, effective_fixed) = resolve_pattern(&args);

    // Plan selection: several modes must bypass trigram narrowing
    // (each is sound, just scans more):
    //  -v        : a file with zero literal occurrences still "matches" all lines
    //  --passthru: non-matching lines of searched files must still print
    //  -c --include-zero : every walked file emits `path:0`
    //  -E non-utf8: index trigrams index raw bytes; decoded matches can't narrow
    //  -P patterns may use pcre2-only syntax (parse failure means Plan::All)
    //  --pre     : the searched stream is a subprocess's output, not the
    //              file's bytes, so index trigrams can't narrow it.
    //  --files-without-match needs no special plan: narrowed-out files
    //    are listed from the universe without being searched
    let utf8_only = match &args.encoding {
        Some(label) => {
            grep_searcher::Encoding::new(label).map_err(|e| anyhow::anyhow!("{label}: {e}"))?
                == grep_searcher::Encoding::new("utf-8").unwrap()
        }
        None => true,
    };
    let plan_ic = args.ignore_case || args.smart_case;
    let query_plan = if args.invert_match
        || args.passthru
        || (args.count && args.include_zero)
        || !utf8_only
        || args.pre.is_some()
    {
        crate::plan::Plan::All
    } else {
        plan::build(&pattern, effective_fixed, plan_ic)
    };
    timings.stage("plan");

    // Binary-flagged files are candidates only when the mode can surface
    // them: -a/--binary, --null-data (NUL is a separator, not binary),
    // an explicit non-UTF-8 encoding (UTF-16 text is NUL-heavy until
    // transcoded), and --pre (a preprocessor turns a PDF into text). Under quit detection they can never emit anything,
    // and rg excludes them even under -c --include-zero (verified), so
    // they stay out.
    let search_binary =
        args.text || args.binary || args.null_data || !utf8_only || args.pre.is_some();
    let (mut plain, mut hidden) =
        idx.candidates_split(&query_plan, plan_ic, args.hidden, search_binary);
    if args.search_zip {
        let (zip_plain, zip_hidden) = idx.zip_candidates_split(args.hidden);
        merge_extra(&mut plain, zip_plain);
        merge_extra(&mut hidden, zip_hidden);
    }
    merge_extra(&mut plain, extra_plain.clone());
    merge_extra(&mut hidden, extra_hidden.clone());
    let mut files = scope.select(plain, hidden);
    apply_sort(&mut files, &args, &root, &cwd_rel)?;
    timings.stage("candidates");

    // --files-without-match needs the full live set as its universe:
    // narrowed-out files can't match, so they emit without searching.
    // Under quit detection rg never lists a file it detects as binary,
    // so the index's binary flags ride along for the unsearched ones.
    let universe = if args.files_without_match {
        let (plain, hidden) = live_split();
        let mut u = scope.select(plain, hidden);
        apply_sort(&mut u, &args, &root, &cwd_rel)?;
        Some(search::Universe {
            files: u,
            binary: if search_binary {
                Default::default()
            } else {
                let mut b = idx.binary_files();
                // Explicit file operands are always searched (and with
                // --binary's detection), so their own outcome decides.
                for e in &args.explicit_files {
                    b.remove(e);
                }
                b
            },
        })
    } else {
        None
    };

    let mut opts = search_opts(&args, &cwd_rel);
    opts.fixed = effective_fixed;

    // rg's nothing-searched heuristic: with the implicit path scope (also
    // from a subdirectory), an empty walked pool (ignore rules or filters
    // ate everything) warns on stderr and exits 2. An empty set produced
    // by trigram narrowing is a plain no-match (exit 1): the pool was
    // still searched. The check stops at the first surviving file.
    let mut nothing_searched = false;
    if files.is_empty() && scope.implicit {
        let live = idx
            .manifest
            .live_entries()
            .map(|e| (e.path.as_path(), e.flags & FLAG_HIDDEN != 0))
            .chain(extra_plain.iter().map(|p| (p.as_path(), false)))
            .chain(extra_hidden.iter().map(|p| (p.as_path(), true)));
        if !scope.searches_anything(live) {
            nothing_searched = true;
            report_nothing_searched();
        }
    }

    let mut out = search::StdoutOut;
    let found = search::run(&pattern, &root, &files, universe.as_ref(), &opts, &mut out)?;
    timings.stage("search");
    timings.finish();
    Ok(exit_code(&args, had_error || nothing_searched, found))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_rel_strips_or_climbs() {
        use crate::search::display_rel;
        let p = Path::new("src/deep/x.rs");
        assert_eq!(display_rel(p, Path::new("")), Path::new("src/deep/x.rs"));
        assert_eq!(display_rel(p, Path::new("src")), Path::new("deep/x.rs"));
        assert_eq!(
            display_rel(Path::new("notes.txt"), Path::new("src")),
            Path::new("../notes.txt")
        );
        assert_eq!(
            display_rel(Path::new("a/c/x"), Path::new("a/b/d")),
            Path::new("../../c/x")
        );
    }

    #[test]
    fn norm_join_folds_dots() {
        assert_eq!(
            norm_join(Path::new("src"), Path::new("..")),
            Some(PathBuf::new())
        );
        assert_eq!(
            norm_join(Path::new("src"), Path::new("./deep/../x")),
            Some(PathBuf::from("src/x"))
        );
        assert_eq!(norm_join(Path::new(""), Path::new("..")), None);
    }
}
