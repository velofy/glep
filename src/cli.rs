use crate::index::manifest::{
    FLAG_DEAD, FLAG_HIDDEN, FLAG_RACY, FLAG_SKIP_BINARY, FLAG_SKIP_TOO_LARGE, FLAG_TRANSCODE,
};
use crate::index::quick::QuickIndex;
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
    /// Don't respect .ignore and .rgignore files (rg --no-ignore-dot).
    /// Files excluded only by them aren't in the index, so this is a
    /// live-scan escape hatch.
    #[arg(long)]
    pub no_ignore_dot: bool,
    /// Don't respect VCS ignore rules: .gitignore, .git/info/exclude and
    /// the global gitignore (rg --no-ignore-vcs).
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
    /// $1/${name} (rg -r/--replace). Under --json each submatch carries
    /// its replacement, like rg 15.
    #[arg(short = 'r', long = "replace", value_name = "REPLACEMENT")]
    pub replace: Option<String>,
    /// Search each file's preprocessor output instead of its contents
    /// (rg --pre; the file path is the command's sole argument and the
    /// file is also its stdin)
    #[arg(long, value_name = "COMMAND")]
    pub pre: Option<String>,
    /// Apply --pre only to files matching these globs (rg --pre-glob,
    /// repeatable; a `!glob` exempts files). Without --pre it changes
    /// nothing, as in rg.
    #[arg(long, value_name = "GLOB")]
    pub pre_glob: Vec<String>,
    /// Treat \r\n as the line terminator (rg --crlf). A last-wins pair
    /// with --null-data, like rg.
    #[arg(long, overrides_with = "null_data")]
    pub crlf: bool,
    /// Search using memory maps when possible (rg --mmap). Accepted for
    /// compat: glep already searches each file as one slice, read whole
    /// into a reused buffer, or mapped when over 1 MiB.
    #[arg(long)]
    pub mmap: bool,
    /// Never search a file as one slice: stream every file through the
    /// searcher's buffer instead (rg --no-mmap)
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
    /// Worker thread count, 0 for auto: GLEP_THREADS if set, else the
    /// performance-core count on macOS, else one per core (rg -j/--threads)
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
    if p.is_absolute() {
        // Outside the tree: kept as is, and printed as is.
        return p.to_path_buf();
    }
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
            // Root-relative; an operand above the root keeps leading `..`
            // components, so it still names the right place.
            *p = root_relative(cwd_rel, p);
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
                    "{}: IO error for operation on {}: {}",
                    p.display(),
                    p.display(),
                    crate::messages::not_found()
                ));
            } else {
                messages::error(format!("{}: {}", p.display(), crate::messages::not_found()));
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

    /// The paths of the directory operands the index serves, when every
    /// one lies below the root: only files under them can be selected.
    /// None when one of them is the root itself (every file can be).
    fn dir_prefixes(&self) -> Option<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        for op in &self.operands {
            if matches!(op.kind, filters::OperandKind::Dir) {
                if op.path.as_os_str().is_empty() {
                    return None;
                }
                dirs.push(op.path.clone());
            }
        }
        Some(dirs)
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
                let prefix = op.path.clone();
                let all = prefix.as_os_str().is_empty();
                let filters = &mut self.filters;
                filters.begin_operand(&prefix);
                let rescue = filters.may_keep_hidden(&prefix);
                let mut live = live;
                live.any(|(f, hidden)| {
                    (all || f.starts_with(&prefix))
                        && (rescue || !hidden)
                        && filters.keep(f, hidden)
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

    /// Whether `select` keeps the indexed file `f`, flagged `hidden` by
    /// the sweep, under some directory operand: the -q probe's per-file
    /// form of it (file operands and walked directories aside).
    fn probe_keeps(&mut self, f: &Path, hidden: bool) -> bool {
        let Scope {
            operands, filters, ..
        } = self;
        operands.iter().any(|op| {
            let p = &op.path;
            if !matches!(op.kind, filters::OperandKind::Dir)
                || !(p.as_os_str().is_empty() || f.starts_with(p))
            {
                return false;
            }
            filters.begin_operand(p);
            (!hidden || filters.may_keep_hidden(p)) && filters.keep(f, hidden)
        })
    }
}

/// The --files glob (glep's pattern slot), matched against the path as
/// printed: relative to the cwd, like -g.
fn files_glob(args: &Args) -> anyhow::Result<Option<globset::GlobMatcher>> {
    match args.pattern.as_deref() {
        // `**` (built as `**/**`) matches every path: nothing to filter.
        Some("**") | None => Ok(None),
        Some(g) => Ok(Some(build_glob_icase(g, args.glob_case_insensitive)?)),
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
    if respect && !args.no_ignore_dot {
        walk::add_rgignore(&mut wb);
    }
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
    match out.write_all(&buf).and_then(|()| out.flush()) {
        // A reader that went away (`glep --files | head -1`) ends the
        // listing, not the run: rg's graceful termination.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        res => Ok(res?),
    }
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
        pre_glob: filters::build_overrides(&args.pre_glob, &[], false)
            .ok()
            .flatten(),
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
    ahead: &Ahead,
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
    let found = ahead.search(&pattern, root, &mut files, None, &opts)?;
    timings.stage("search");
    timings.finish();
    Ok(exit_code(args, had_error || nothing_searched, found))
}

/// Unindexed trees holding more than this many bytes of files answer
/// their first query from a sweep and build the index in the background
/// (see `run`); smaller ones build it inline, which takes about a second
/// at this size. GLEP_BG_BUILD_MIN_BYTES overrides it.
#[cfg(unix)]
const BG_BUILD_MIN_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(unix)]
fn background_build_min_bytes() -> u64 {
    parse_bg_build_min_bytes(std::env::var("GLEP_BG_BUILD_MIN_BYTES").ok().as_deref())
}

/// A GLEP_BG_BUILD_MIN_BYTES value: a byte count with an optional K, M
/// or G suffix, like --max-filesize; unset or unparsable is the default.
#[cfg(unix)]
fn parse_bg_build_min_bytes(v: Option<&str>) -> u64 {
    v.and_then(|v| parse_human_u64(v.trim()).ok())
        .unwrap_or(BG_BUILD_MIN_BYTES)
}

/// The first query on an unindexed tree too big to index inline, answered
/// from the full sweep that measured the tree the way `run_live` answers
/// from its walk: every selected file is searched, so binary handling is
/// the searcher's own. The sweep applies the index's ignore and hidden
/// rules (it is the sweep a build indexes), so the scope selects from it
/// exactly as from a fresh manifest, and the rest is the index path's:
/// directory operands the sweep holds no file under walk live with rg's
/// rules, then the --files glob and printer, or the search. Nothing here
/// opens, builds or writes the index.
#[cfg(unix)]
fn run_from_sweep(
    root: &Path,
    cwd_rel: &Path,
    args: &Args,
    scope: &mut Scope,
    swept: Vec<walk::FileMeta>,
    timings: &mut Timings,
    had_error: bool,
) -> anyhow::Result<i32> {
    // `Index::has_files_under` over the sweep instead of the manifest:
    // operands are root-relative and normalized, so one that is not plain
    // names below the root (`..`, absolute elsewhere) holds no swept file.
    scope.walk_unserved(args, cwd_rel, |op| {
        names_below_root(op) && prefixes_with_files(&swept, &[op.to_path_buf()])[0]
    });
    timings.stage("operands");
    // Split by the sweep's hidden flag, as the index splits by
    // FLAG_HIDDEN; under --hidden every file is plain.
    use rayon::prelude::*;
    let (mut hidden, mut plain): (Vec<PathBuf>, Vec<PathBuf>) =
        swept.into_par_iter().partition_map(|m| {
            if m.hidden && !args.hidden {
                rayon::iter::Either::Left(m.path)
            } else {
                rayon::iter::Either::Right(m.path)
            }
        });
    sort_swept_paths(&mut plain);
    sort_swept_paths(&mut hidden);
    let mut files = scope.select(plain, hidden);
    if args.files {
        // With --files the pattern slot is the glob.
        retain_glob(&mut files, files_glob(args)?.as_ref(), cwd_rel);
        apply_sort(&mut files, args, root, cwd_rel)?;
        print_paths(&files, cwd_rel, args)?;
        timings.stage("listing");
        return Ok(exit_code(args, had_error, !files.is_empty()));
    }

    let (pattern, effective_fixed) = resolve_pattern(args);
    apply_sort(&mut files, args, root, cwd_rel)?;
    timings.stage("candidates");
    // Nothing narrowed the selection, so an empty one searched nothing.
    let nothing_searched = scope.implicit && files.is_empty();
    if nothing_searched {
        report_nothing_searched();
    }
    let mut opts = search_opts(args, cwd_rel);
    opts.fixed = effective_fixed;
    let mut out = search::StdoutOut;
    let found = search::run(&pattern, root, &files, None, &opts, &mut out)?;
    timings.stage("search");
    Ok(exit_code(args, had_error || nothing_searched, found))
}

/// Start building the index of `root` in a detached child (`glep index`
/// with GLEP_BG set, see `run_background_build`) and return without
/// waiting for it. Its stdio is the null device, so a pipe or `$(...)`
/// reading this run sees EOF as soon as this process exits, and it runs in
/// a process group of its own, so a Ctrl-C at the terminal leaves it
/// alone. Nothing is spawned when the index dir cannot be written or
/// another glep holds its lock (a build is running already), and a spawn
/// that fails is silent: the next query on the tree tries again.
#[cfg(unix)]
fn spawn_background_build(root: &Path, cwd: &Path, max_filesize: u64) {
    if !Index::can_build(root) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index")
        .arg("--max-filesize")
        .arg(max_filesize.to_string())
        .current_dir(root)
        .env("GLEP_BG", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The child finds this run's root from `root`: through the index dir
    // `can_build` made there, or through GLEP_INDEX_PATH, resolved against
    // this run's cwd first because the child runs from the root.
    if let Some(v) = std::env::var_os("GLEP_INDEX_PATH").filter(|v| !v.is_empty()) {
        cmd.env("GLEP_INDEX_PATH", cwd.join(v));
    }
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    if let Ok(child) = cmd.spawn() {
        // Test hook: the builder's pid, for tests that stop it mid-build.
        if let Some(path) = std::env::var_os("GLEP_BG_PIDFILE") {
            let _ = std::fs::write(path, format!("{}\n", child.id()));
        }
    }
}

/// `glep index` as the detached build a first query spawned (GLEP_BG set):
/// at background priority, it builds the index only while none exists,
/// and exits 0 without building when another glep holds the lock (a
/// second spawner lost the race).
fn run_background_build(root: &Path, max_filesize: u64) -> anyhow::Result<i32> {
    lower_thread_priority();
    // The sweep's pool threads too (a second init keeps the first).
    let _ = rayon::ThreadPoolBuilder::new()
        .start_handler(|_| lower_thread_priority())
        .build_global();
    Index::build_if_missing(root, max_filesize)?;
    Ok(0)
}

/// Background priority for the calling thread: utility QoS on macOS,
/// below every interactive thread for CPU and I/O; nice 10 on other Unix
/// systems (a per-thread setting on Linux, so every pool thread sets it).
/// Windows never spawns the build (see `run`).
fn lower_thread_priority() {
    #[cfg(target_os = "macos")]
    // SAFETY: a plain call on the current thread; no pointers involved.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    // SAFETY: a plain call on the current thread; no pointers involved.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// True when `p` (root-relative after `normalize_path_filters`) is a path
/// of plain names below the root: not the root itself, not absolute, no
/// `.` or `..` component.
fn names_below_root(p: &Path) -> bool {
    !p.as_os_str().is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Whether some swept path lies under each prefix, in one pass over the
/// sweep: a path is under a prefix when its bytes start with the
/// prefix's and the next byte is a separator or the end (both are plain
/// names joined by single separators). Stops once every prefix has one.
fn prefixes_with_files(metas: &[walk::FileMeta], prefixes: &[PathBuf]) -> Vec<bool> {
    let mut found = vec![false; prefixes.len()];
    let mut missing = prefixes.len();
    for m in metas {
        if missing == 0 {
            break;
        }
        for (prefix, seen) in prefixes.iter().zip(found.iter_mut()) {
            if !*seen && path_is_under(&m.path, prefix) {
                *seen = true;
                missing -= 1;
            }
        }
    }
    found
}

/// Whether `path` is `prefix` or lies below it, comparing whole names.
/// On Unix both are plain names joined by single '/' separators, so a byte
/// comparison is exact and cheap. Windows accepts '/' and '\\' alike and
/// operands may mix them, so it compares path components instead.
#[cfg(unix)]
fn path_is_under(path: &Path, prefix: &Path) -> bool {
    let path = path.as_os_str().as_encoded_bytes();
    let prefix = prefix.as_os_str().as_encoded_bytes();
    path.starts_with(prefix) && path.get(prefix.len()).is_none_or(|&b| b == b'/')
}

#[cfg(not(unix))]
fn path_is_under(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

/// A scoped --files listing (directory operands below the root, see
/// `run`) from a live sweep of the operands, never the index: a listing
/// needs neither postings nor the manifest, and the scoped sweep is the
/// one `Index::update_scoped` runs (read with names only), whose result
/// is exactly the set the index path lists under the operands. Nothing
/// here opens, builds or writes the index (or the listing cache), so an
/// unindexed tree lists without a build.
///
/// Operands the sweep holds no file under are walked live with rg's
/// rules, as on the index path (`Scope::walk_unserved`), unless the sweep
/// entered the operand as a directory (every level above it one the walk
/// from the root enters) and no -g/--iglob is given: rg's walk of such an
/// operand applies the same ignore rules, and every other filter (-t,
/// --ignore-file, --max-depth, hidden) only drops files, so it could
/// yield nothing either. Only a glob override can bring back files that
/// ignore rules exclude, so with one those operands still walk live,
/// exactly as on the index path. The rest of the listing is the index
/// path's: the scope's selection, the --files glob, --sort, the printer.
fn list_scoped(
    root: &Path,
    cwd_rel: &Path,
    args: &Args,
    scope: &mut Scope,
    prefixes: &[PathBuf],
    timings: &mut Timings,
    had_error: bool,
) -> anyhow::Result<i32> {
    // The sweep's directory reads contend in the kernel the way a
    // search's opens and reads do, so without -j (which sized the pool
    // already) the pool takes the search pool's width: on an M3 Pro the
    // readdir walk of the kernel's drivers/ takes about 20 ms on the 5
    // performance cores against 25 ms on all 11.
    if args.threads.is_none() {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(search::default_threads())
            .build_global();
    }
    // Only explicit file operands: nothing to sweep.
    let swept = if prefixes.is_empty() {
        walk::ScopedListing {
            files: Vec::new(),
            entered: Vec::new(),
        }
    } else {
        walk::sweep_scoped_listing(root, prefixes)?
    };
    timings.stage("scoped_sweep");
    let found = prefixes_with_files(&swept.files, prefixes);
    let empty_is_final = scope.filters.overrides().is_none();
    scope.walk_unserved(args, cwd_rel, |op| {
        prefixes.iter().zip(&found).any(|(p, &has)| has && p == op)
            || (empty_is_final && swept.entered.iter().any(|p| p == op))
    });
    timings.stage("operands");
    // Split by the sweep's hidden flag, as the index splits by
    // FLAG_HIDDEN; under --hidden every file is plain.
    use rayon::prelude::*;
    let (mut hidden, mut plain): (Vec<PathBuf>, Vec<PathBuf>) =
        swept.files.into_par_iter().partition_map(|m| {
            if m.hidden && !args.hidden {
                rayon::iter::Either::Left(m.path)
            } else {
                rayon::iter::Either::Right(m.path)
            }
        });
    sort_swept_paths(&mut plain);
    sort_swept_paths(&mut hidden);
    let mut files = scope.select(plain, hidden);
    // With --files the pattern slot is the glob.
    retain_glob(&mut files, files_glob(args)?.as_ref(), cwd_rel);
    apply_sort(&mut files, args, root, cwd_rel)?;
    print_paths(&files, cwd_rel, args)?;
    timings.stage("listing");
    timings.finish();
    Ok(exit_code(args, had_error, !files.is_empty()))
}

/// Sort paths a sweep yielded (relative, plain names joined by single
/// separators, each path once) into `Path` order. For such paths the
/// component-wise order is byte order with the separator ranked below
/// every other byte, and no name holds a NUL. On Unix each path's bytes
/// are sorted in place with the separator swapped for NUL: a plain byte
/// comparison per pair instead of `Path::cmp`'s component parsing (1.6 ms
/// against about 0.3 ms for a 6k-file listing).
fn sort_swept_paths(paths: &mut Vec<PathBuf>) {
    use rayon::prelude::*;
    #[cfg(unix)]
    {
        let mut keys: Vec<Vec<u8>> = std::mem::take(paths)
            .into_par_iter()
            .map(into_sort_key)
            .collect();
        keys.par_sort_unstable();
        *paths = keys.into_par_iter().map(from_sort_key).collect();
    }
    #[cfg(not(unix))]
    paths.par_sort_unstable();
}

/// A swept path's bytes as its `Path`-order sort key (see
/// `sort_swept_paths`): the separator becomes NUL, in place.
#[cfg(unix)]
fn into_sort_key(p: PathBuf) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    swap_byte(p.into_os_string().into_vec(), b'/', 0)
}

/// The path an `into_sort_key` key came from, in the key's own buffer.
#[cfg(unix)]
fn from_sort_key(k: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(swap_byte(k, 0, b'/')))
}

#[cfg(unix)]
fn swap_byte(mut b: Vec<u8>, from: u8, to: u8) -> Vec<u8> {
    for c in b.iter_mut().filter(|c| **c == from) {
        *c = to;
    }
    b
}

/// How the matcher will compare letters, for the index plan. -S always
/// plans case-insensitively (a safe superset when the pattern's capitals
/// make the search case-sensitive). Both engines fold with Unicode's
/// tables unless --no-unicode, and even then a pattern can turn Unicode
/// back on inline (`(?u)`, PCRE2's `(*UTF)`), so only a pattern without
/// inline flags gets ASCII-only folding.
fn plan_case(args: &Args, pattern: &str, fixed: bool) -> plan::Case {
    if !(args.ignore_case || args.smart_case) {
        plan::Case::Sensitive
    } else if args.no_unicode && (fixed || !(pattern.contains("(?") || pattern.contains("(*"))) {
        plan::Case::Ascii
    } else {
        plan::Case::Unicode
    }
}

/// A content query's resolved patterns and index plan (`plan_query`).
struct Query {
    /// The patterns as one (`resolve_pattern`), and whether the matcher
    /// takes it as a literal.
    pattern: String,
    fixed: bool,
    /// No -E names an encoding other than UTF-8.
    utf8_only: bool,
    case: plan::Case,
    plan: plan::Plan,
}

/// Resolve the patterns and choose the index plan, from the flags alone:
/// nothing here reads the index, so the plan is known before it opens.
/// Several modes must bypass trigram narrowing (each is sound, just scans
/// more):
///  -v        : a file with zero literal occurrences still "matches" all lines
///  --passthru: non-matching lines of searched files must still print
///  -c --include-zero : every walked file emits `path:0`
///  -E non-utf8: index trigrams index raw bytes; decoded matches can't narrow
///  -P patterns may use pcre2-only syntax (parse failure means Plan::All)
///  --pre     : the searched stream is a subprocess's output, not the
///              file's bytes, so index trigrams can't narrow it.
///  --files-without-match needs no special plan: narrowed-out files
///    are listed from the universe without being searched
fn plan_query(args: &Args) -> anyhow::Result<Query> {
    let (pattern, fixed) = resolve_pattern(args);
    let utf8_only = match &args.encoding {
        Some(label) => {
            grep_searcher::Encoding::new(label).map_err(|e| anyhow::anyhow!("{label}: {e}"))?
                == grep_searcher::Encoding::new("utf-8").unwrap()
        }
        None => true,
    };
    let case = plan_case(args, &pattern, fixed);
    let plan = if args.invert_match
        || args.passthru
        || (args.count && args.include_zero)
        || !utf8_only
        || args.pre.is_some()
    {
        plan::Plan::All
    } else {
        plan::build(&pattern, fixed, case)
    };
    Ok(Query {
        pattern,
        fixed,
        utf8_only,
        case,
        plan,
    })
}

/// rg's nothing-searched warning: with no path operand, a walk that
/// yields no file at all (filters ate everything) is an error, exit 2.
fn report_nothing_searched() {
    messages::error(
        "No files were searched, which means glep probably applied a filter you didn't expect.\nRunning with --debug will show why files are being skipped.",
    );
}

/// True when the listing cache is the fast way to answer: a root-wide
/// listing whose flags change neither what the cache would list nor the
/// order it is printed in. Every flag that changes listing SEMANTICS
/// (--no-ignore and -u/-uu/-uuu, the --no-ignore-* toggles, -L,
/// --one-file-system, --require-git outside a repo, --no-index) takes a
/// live-scan escape hatch before the cache is ever consulted; the
/// remaining filters (-g, -t/-T/--type-add/--type-clear, --ignore-file,
/// --max-depth, --hidden, the glob slot) are pure functions of the path
/// and the sweep's hidden flag, applied identically to either listing
/// (inside the cached one, before any path is copied). Timestamp sorts
/// need a live walk order and live metadata, so they bypass the cache.
///
/// The cache's pruned walk always covers the whole tree, so a scoped
/// listing (a path operand below the root, or the cwd scope of a
/// subdirectory run) is cheaper through the index's scoped sweep, which
/// walks only the operands: on a kernel-scale tree `--files` in `rust/`
/// takes about 14 ms that way, while the cache walks the whole tree.
///
/// A content query no trigram narrows searches the cache's listing under
/// the same conditions (see `search_cached_listing`): the files it
/// searches, and the order it prints them in, are the listing's.
#[cfg(target_os = "macos")]
fn listing_cache_eligible(args: &Args) -> bool {
    sort_is_path_order(args) && args.paths.iter().all(|p| p.as_os_str().is_empty())
}

/// macOS-only fast listing via the dir-mtime cache (see dircache.rs):
/// load the cached tree, verify the ignore-source fingerprint, run the
/// pruned walk (zero-clone: cache-hit entries borrow the loaded tree),
/// save the walked tree when some directory changed, and return the paths
/// `keep` accepts, in path order. `keep` runs across the pool with one
/// `init()` state per rayon job, and only the paths it keeps are copied.
/// None = cache miss for any reason: caller falls back to the full
/// freshness-sweep path.
#[cfg(target_os = "macos")]
fn listing_via_cache<S, I, F>(
    root: &Path,
    index_dir: &Path,
    init: I,
    keep: F,
    timings: &mut Timings,
) -> Option<CachedListing>
where
    I: Fn() -> S + Sync + Send,
    F: Fn(&mut S, &crate::walk::FileMeta) -> bool + Sync + Send,
{
    use rayon::prelude::*;
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
    let t_walk = std::time::Instant::now();
    if let Some(walked) = outcome.tree {
        let updated = crate::dircache::DirTree {
            schema: crate::dircache::SCHEMA,
            fingerprint: tree.fingerprint.clone(),
            root: walked,
        };
        let _ = updated.save(index_dir);
        // The listing prints and the process exits right after: leave the
        // copy to the exit (see the cache branch in `run`).
        std::mem::forget(updated);
    }
    let t_save = std::time::Instant::now();
    // Parallel filter: 86k globset matches on a kernel-scale tree cost
    // ~15ms serial, ~2ms across the pool. Only the kept paths are copied,
    // straight into their sort keys.
    let mut keys: Vec<Vec<u8>> = outcome
        .files
        .par_iter()
        .map_init(&init, |state, m| {
            keep(state, m).then(|| into_sort_key(m.path.clone()))
        })
        .filter_map(std::convert::identity)
        .collect();
    let t_filter = std::time::Instant::now();
    keys.par_sort_unstable();
    let files: Vec<PathBuf> = keys.into_par_iter().map(from_sort_key).collect();
    // The entries borrow `tree`, which the caller leaves to the exit too.
    std::mem::forget(outcome.files);
    timings.stage("listing_cache_walk");
    if std::env::var_os("GLEP_TIMING").is_some() {
        let ms = |a: std::time::Instant, b: std::time::Instant| {
            b.duration_since(a).as_secs_f64() * 1000.0
        };
        eprintln!(
            "glep timing: cache load {:.1}ms, pruned walk {:.1}ms, save {:.1}ms, filter {:.1}ms, sort {:.1}ms",
            ms(t0, t1),
            ms(t1, t_walk),
            ms(t_walk, t_save),
            ms(t_save, t_filter),
            ms(t_filter, std::time::Instant::now())
        );
    }
    Some(CachedListing { files, tree })
}

/// The listing cache's file set for a run whose every operand is the root
/// (`listing_cache_eligible`), in path order, once per operand (rg lists
/// a root named twice twice). rg's filters for the root (-g/--iglob,
/// -t/-T, --ignore-file, --max-depth and the hidden rule, as
/// `Filters::select` applies them) and `glob`, the --files glob, all apply
/// inside the listing, before any path is copied: each rayon job filters
/// with its own copy of the scope's filters, whose verdict caches are per
/// copy. None on a cache miss.
#[cfg(target_os = "macos")]
fn cached_root_listing(
    root: &Path,
    cwd_rel: &Path,
    scope: &mut Scope,
    glob: Option<globset::GlobMatcher>,
    timings: &mut Timings,
) -> Option<CachedListing> {
    scope.filters.begin_operand(Path::new(""));
    let rescue = scope.filters.may_keep_hidden(Path::new(""));
    let template = &scope.filters;
    let keep = |flt: &mut filters::Filters, m: &crate::walk::FileMeta| -> bool {
        (rescue || !m.hidden)
            && glob
                .as_ref()
                .is_none_or(|g| g.is_match(&*search::display_rel(&m.path, cwd_rel)))
            && flt.keep(&m.path, m.hidden)
    };
    let mut listing =
        listing_via_cache(root, &root.join(".glep"), || template.fork(), keep, timings)?;
    if scope.operands.len() > 1 {
        let once = listing.files.clone();
        for _ in 1..scope.operands.len() {
            listing.files.extend_from_slice(&once);
        }
    }
    Some(listing)
}

/// A root-wide content query whose plan no trigram narrows (Plan::All; see
/// `run` for when it comes here), searched over the listing cache's file
/// set. The index would hand back every file it holds, after the full
/// freshness sweep that makes sure of them; the cache lists the same set
/// (its listings are the index's, as --files compares them), filtered as
/// the scope selects from the index's (`cached_root_listing`). glep's
/// --max-filesize only caps what the index stores, so no file is left out
/// for its size. Every listed file is searched, binary ones included, as
/// `run_live` searches its walk: the searcher's binary detection decides
/// what they print, and no content flag of the index is read that could
/// be stale. Nothing here opens, sweeps or writes the index, so its
/// manifest and sweep epoch stay as they were and the next narrowed query
/// sweeps as before. None on a cache miss, before anything is printed.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn search_cached_listing(
    root: &Path,
    cwd_rel: &Path,
    args: &Args,
    scope: &mut Scope,
    query: &Query,
    timings: &mut Timings,
    had_error: bool,
    ahead: &Ahead,
) -> anyhow::Result<Option<i32>> {
    let Some(CachedListing { mut files, tree }) =
        cached_root_listing(root, cwd_rel, scope, None, timings)
    else {
        return Ok(None);
    };
    apply_sort(&mut files, args, root, cwd_rel)?;
    timings.stage("candidates");
    // Nothing narrowed the selection, so an empty one searched nothing.
    let nothing_searched = scope.implicit && files.is_empty();
    if nothing_searched {
        report_nothing_searched();
    }
    let mut opts = search_opts(args, cwd_rel);
    opts.fixed = query.fixed;
    let found = ahead.search(&query.pattern, root, &mut files, None, &opts)?;
    timings.stage("search");
    timings.finish();
    // The process exits right after this returns: the file list and the
    // loaded tree are left to the exit, as in the --files cache branch.
    std::mem::forget(files);
    std::mem::forget(tree);
    Ok(Some(exit_code(args, had_error || nothing_searched, found)))
}

/// Whether -q's probe may answer this run (see `probe`): -q without
/// --json or --stats, which count every file (rg's quit_after_match), and
/// without --files-without-match, whose answer is a file without a
/// match. --ttl keeps its meaning: inside its window the index answers
/// without a sweep anyway. GLEP_NO_PROBE turns the probe off.
fn probe_eligible(args: &Args) -> bool {
    args.quiet
        && !args.json
        && !args.stats
        && !args.files_without_match
        && args.ttl == 0
        && std::env::var_os("GLEP_NO_PROBE").is_none()
}

/// The -q probe's file check (`walk::FileCheck`), set up on a thread of
/// its own when the arguments look like a run the probe answers (a `-q`
/// or `--quiet`, or a cluster of short flags holding a q, and none of the
/// flags `probe_eligible` turns away): it finds the index root as `run`
/// does and loads the ignore rules above it (the global gitignore among
/// them, whose git-config parsing compiles a regex) while the arguments
/// parse. None otherwise. A guess the parsed arguments prove wrong costs
/// that thread its work, or the probe the time to load the rules itself.
fn early_file_check() -> Option<std::thread::JoinHandle<Option<walk::FileCheck>>> {
    if std::env::var_os("GLEP_NO_PROBE").is_some() {
        return None;
    }
    let mut quiet = false;
    for a in std::env::args_os().skip(1) {
        let a = a.as_encoded_bytes();
        if [&b"--stats"[..], b"--json", b"--files-without-match"].contains(&a)
            || a.starts_with(b"--ttl")
        {
            return None;
        }
        quiet |= a == b"--quiet"
            || (a.len() > 1
                && a[0] == b'-'
                && a[1] != b'-'
                && a[1..].iter().all(u8::is_ascii_alphanumeric)
                && a.contains(&b'q'));
    }
    if !quiet {
        return None;
    }
    std::thread::Builder::new()
        .spawn(|| {
            let cwd = std::env::current_dir().ok()?;
            let (root, _) = discover_index_root(&cwd).ok()?;
            root.join(".glep/manifest.bin")
                .exists()
                .then(|| walk::FileCheck::new(root))
        })
        .ok()
}

/// How far the probe goes before it hands the run to the freshness sweep:
/// candidates looked at, files searched, and bytes searched. The probe
/// searches one file at a time; past these the sweep and the parallel
/// search take over, skipping what the probe searched (see `Ahead`).
const PROBE_EXAMINE: usize = 256;
const PROBE_FILES: usize = 64;
const PROBE_BYTES: u64 = 4 << 20;

/// The first round of a narrowed probe decodes the posting lists only
/// below this id; each later round goes eight times as far (see `probe`).
const PROBE_FIRST_IDS: u64 = 4096;

/// What a probe that found nothing leaves the run (see `Ahead`).
#[derive(Default)]
struct Probed {
    /// Searched without a match, with the (mtime, size) they had then.
    unsettled: Vec<(PathBuf, u128, u64)>,
    /// Searched with an error, reported already.
    failed: Vec<PathBuf>,
}

/// -q's probe, for a run whose answer is whether anything matches
/// (`probe_eligible`): before the freshness sweep runs, search the indexed
/// files the plan picks (every entry under Plan::All), in index order, on
/// this thread, and end the run at the first match. A match decides the
/// run only in a file the run searches, so each candidate is first looked
/// up on disk as the sweep would find it (`walk::FileCheck`: ignore rules,
/// hidden flag, symlinks, unreadable directories, all as of now) and
/// filtered by the run's operands and filters. It is searched only when
/// its (mtime, size) and hidden flag are still what the index recorded
/// and its entry is not racy: what a new, changed or racy file holds is
/// the sweep's business, which reads it again. Files the index never read
/// (oversized, UTF-16, binary) wait for the run too. Nothing here trusts
/// the index: it only picks which files to look at first, and a stale,
/// torn or corrupt one just finds nothing. File operands come first: the
/// run searches each as itself, whatever any rule says.
///
/// The first candidates are the ones that matter, so a narrowed plan's
/// posting lists are decoded in rounds, each only below an id bound
/// (PROBE_FIRST_IDS, then eight times further each round), instead of
/// whole: on a kernel-scale tree a pattern of common trigrams intersects
/// tens of thousands of ids per list. Candidates are looked up in batches
/// that start at one file and double.
///
/// None when a file settled the run (exit 0). Otherwise what was searched,
/// for the run to skip (`Ahead`), after at most PROBE_EXAMINE candidates,
/// PROBE_FILES files or PROBE_BYTES bytes. `check` is the file check, set
/// up on a thread of its own (`early_file_check`): without one, or with
/// one for another root, the probe sets one up itself.
#[allow(clippy::too_many_arguments)]
fn probe(
    root: &Path,
    scope: &mut Scope,
    query: &Query,
    mut view: QuickIndex,
    check: Option<std::thread::JoinHandle<Option<walk::FileCheck>>>,
    matcher: &search::AnyMatcher,
    opts: &search::SearchOpts,
) -> anyhow::Result<Option<Probed>> {
    let timing = std::env::var_os("GLEP_TIMING").is_some();
    let mut prober = search::Prober::new(matcher, opts)?;
    let mut probed = Probed::default();
    // Content read now from a file with an mtime past this may be
    // rewritten within the same filesystem clock tick, its (mtime, size)
    // kept: such a file is searched again by the run (see `Ahead`).
    let settled_before = std::time::SystemTime::now()
        .checked_sub(crate::dircache::RACY_SLACK)
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for op in &scope.operands {
        match op.kind {
            filters::OperandKind::File => {
                if !seen.insert(op.path.clone()) {
                    continue;
                }
                match prober.probe(root, &op.path, opts) {
                    search::Probe::Settled => {
                        if timing {
                            eprintln!("glep timing: probe settled on a file operand");
                        }
                        return Ok(None);
                    }
                    search::Probe::Failed => probed.failed.push(op.path.clone()),
                    // Read through a link, maybe: nothing for the run to skip.
                    search::Probe::Unsettled => {}
                }
            }
            filters::OperandKind::Dir => dirs.push(op.path.clone()),
            filters::OperandKind::Walked(_) => {}
        }
    }
    let under_dirs = |p: &Path| {
        dirs.iter()
            .any(|d| d.as_os_str().is_empty() || p.starts_with(d))
    };
    let mut pending = check;
    let mut check: Option<walk::FileCheck> = None;
    let count = view.len() as u64;
    let (mut examined, mut searched, mut bytes) = (0usize, 0usize, 0u64);
    // A narrowed plan's candidates below `hi`, from `lo` on; `at` is the
    // next one to look at.
    let (mut round, mut at, mut lo, mut hi) = (Vec::new(), 0usize, 0u64, PROBE_FIRST_IDS);
    let mut batch = 1usize;
    'batches: while !dirs.is_empty() && examined < PROBE_EXAMINE {
        let want = batch.min(PROBE_EXAMINE - examined);
        batch = (batch * 2).min(64);
        let entries = match &query.plan {
            plan::Plan::Groups(groups) => {
                while at >= round.len() {
                    if lo >= count {
                        break 'batches;
                    }
                    round = crate::messages::hushed(|| view.group_ids(groups, query.case, hi));
                    round.retain(|&id| u64::from(id) >= lo);
                    (at, lo, hi) = (0, hi, hi.saturating_mul(8));
                }
                let (entries, taken) = view.entries_where(&round[at..], want, under_dirs);
                examined += entries.len();
                at += taken;
                entries
            }
            plan::Plan::All => {
                let entries = view.scan(want, under_dirs);
                if entries.is_empty() {
                    break;
                }
                examined += entries.len();
                entries
            }
        };
        let wanted: Vec<_> = entries
            .into_iter()
            .filter(|e| {
                e.flags
                    & (FLAG_DEAD
                        | FLAG_RACY
                        | FLAG_SKIP_TOO_LARGE
                        | FLAG_TRANSCODE
                        | FLAG_SKIP_BINARY)
                    == 0
                    && !seen.contains(&e.path)
                    && scope.probe_keeps(&e.path, e.flags & FLAG_HIDDEN != 0)
            })
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let paths: Vec<PathBuf> = wanted.iter().map(|e| e.path.clone()).collect();
        let check = check.get_or_insert_with(|| match pending.take().map(|h| h.join()) {
            Some(Ok(Some(check))) if check.root() == root => check,
            Some(Err(panic)) => std::panic::resume_unwind(panic),
            _ => walk::FileCheck::new(root.to_path_buf()),
        });
        for (e, now) in wanted.iter().zip(check.files(&paths)) {
            let Some(now) = now else {
                continue;
            };
            if now.mtime_ns != e.mtime_ns
                || now.size != e.size
                || now.hidden != (e.flags & FLAG_HIDDEN != 0)
            {
                continue;
            }
            seen.insert(e.path.clone());
            match prober.probe(root, &e.path, opts) {
                search::Probe::Settled => {
                    if timing {
                        eprintln!(
                            "glep timing: probe settled after {examined} candidates, {} searched",
                            searched + 1
                        );
                    }
                    return Ok(None);
                }
                search::Probe::Unsettled if now.mtime_ns < settled_before => probed
                    .unsettled
                    .push((e.path.clone(), now.mtime_ns, now.size)),
                search::Probe::Unsettled => {}
                search::Probe::Failed => probed.failed.push(e.path.clone()),
            }
            searched += 1;
            bytes += now.size;
            if searched >= PROBE_FILES || bytes >= PROBE_BYTES {
                break 'batches;
            }
        }
    }
    if timing {
        eprintln!("glep timing: probe {examined} candidates, {searched} searched, no match");
    }
    Ok(Some(probed))
}

/// What the -q probe did ahead of the run's own search, if it ran: the
/// matcher it built, and the files it searched without settling the run,
/// which the run's search skips. A failed file's error is reported once.
/// A file searched without a match is skipped only while its (mtime,
/// size) are still what they were then, and only when that mtime was
/// already outside the racy window when the probe read it: a write since
/// would have changed them.
#[derive(Default)]
struct Ahead {
    matcher: Option<search::AnyMatcher>,
    probed: Probed,
}

impl Ahead {
    /// `search::run` over `files`, minus what the probe settled.
    fn search(
        &self,
        pattern: &str,
        root: &Path,
        files: &mut Vec<PathBuf>,
        universe: Option<&search::Universe>,
        opts: &search::SearchOpts,
    ) -> anyhow::Result<bool> {
        let done: std::collections::HashSet<&Path> = self
            .probed
            .failed
            .iter()
            .map(PathBuf::as_path)
            .chain(
                self.probed
                    .unsettled
                    .iter()
                    .filter(|(p, mtime_ns, size)| {
                        std::fs::symlink_metadata(root.join(p)).is_ok_and(|md| {
                            md.len() == *size && crate::dircache::mtime_of(&md) == *mtime_ns
                        })
                    })
                    .map(|(p, _, _)| p.as_path()),
            )
            .collect();
        if !done.is_empty() {
            // The probe searched each file once: a file the run lists
            // twice (under two operands) is searched once more.
            let mut done = done;
            files.retain(|f| !done.remove(f.as_path()));
        }
        let mut out = search::StdoutOut;
        match &self.matcher {
            Some(m) => search::run_with(m, root, files, universe, opts, &mut out),
            None => search::run(pattern, root, files, universe, opts, &mut out),
        }
    }
}

/// Debug builds only, for tests: when the environment variable `var`
/// names a directory, create `reached` in it, then wait (a minute at
/// most) until `resume` appears there, so a test can change the tree at
/// that exact point of a run.
#[cfg(debug_assertions)]
fn test_pause(var: &str) {
    let Some(dir) = std::env::var_os(var).map(PathBuf::from) else {
        return;
    };
    let _ = std::fs::write(dir.join("reached"), b"");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !dir.join("resume").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// A listing served by the dir-mtime cache: the kept paths in path order,
/// and the loaded tree they were listed from, which the caller leaves to
/// the process exit.
#[cfg(target_os = "macos")]
struct CachedListing {
    files: Vec<PathBuf>,
    tree: crate::dircache::DirTree,
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
    // .git/info/exclude and the global gitignore are git sources too: rg
    // reads them only while VCS ignores are on (`--no-ignore-vcs` turns
    // all three off, and `--ignore-exclude` alone does not bring exclude
    // back), so the walker flags below are the effective ones.
    let exclude = vcs && respected("no_ignore_exclude", "ignore_exclude");
    let global = vcs && respected("no_ignore_global", "ignore_global");
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

/// glep's command line on clap, parsed like rg's: a flag that takes a
/// value takes the next word verbatim, even one that starts with a dash
/// (`-e -foo`, `-e '-> Result'`, `-g -x`, `--context-separator --`).
/// Positionals keep clap's rule, so a dash-leading pattern needs `-e` or
/// `--`, as in rg.
fn command() -> clap::Command {
    use clap::CommandFactory;
    Args::command().mut_args(|a| {
        if !a.is_positional() && a.get_action().takes_values() {
            a.allow_hyphen_values(true)
        } else {
            a
        }
    })
}

pub fn run() -> anyhow::Result<i32> {
    use clap::FromArgMatches;
    let early_check = early_file_check();
    let matches = command().get_matches();
    let mut args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    messages::configure(!args.no_messages, !args.no_ignore_messages);
    if let Some(n) = args.threads {
        // Global rayon pool (sweeps, filters); the search pool reads
        // SearchOpts::threads. Without -j the rayon pool keeps its
        // one-per-core default rather than `search::default_threads`:
        // directory sweeps and listing filters still scale past the
        // performance cores (a kernel-tree --files listing measured about
        // 4% slower at 5 threads than at 11 on an M3 Pro). A second init
        // keeps the first (only ever happens under unit tests calling
        // run() twice).
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
        std::io::stdout().write_all(buf.as_bytes())?;
        return Ok(0);
    }
    if args.pcre2_version {
        let jit = if grep_pcre2::is_jit_available() {
            " (JIT is available)"
        } else {
            ""
        };
        let (maj, min) = grep_pcre2::version();
        writeln!(
            std::io::stdout(),
            "PCRE2 {}.{:02} is available{}",
            maj,
            min,
            jit
        )?;
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
    // rg builds the --pre-glob matcher whether or not --pre is given, so
    // a bad glob is an error either way.
    filters::build_overrides(&args.pre_glob, &[], false)?;

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
                if std::env::var_os("GLEP_BG").is_some() {
                    return run_background_build(&root, args.max_filesize);
                }
                let idx = Index::build(&root, args.max_filesize)?;
                eprintln!(
                    "glep: indexed {} files",
                    idx.manifest.live_entries().count()
                );
                return Ok(0);
            }
            Some("status") => {
                let mut idx = Index::open_or_build(&root, args.max_filesize)?;
                // A build just now swept the tree itself.
                if !idx.read_only && !idx.swept_full() {
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
                let report = format!(
                    "files: {live}\nskipped (binary/oversized/utf16): {skipped}\nlast sweep epoch: {}\n",
                    idx.manifest.last_sweep_epoch
                );
                std::io::stdout().write_all(report.as_bytes())?;
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
        return run_live(
            &root,
            &cwd_rel,
            &args,
            &mut scope,
            &mut timings,
            had_error,
            &Ahead::default(),
        );
    }

    // Dir-mtime cached listing (macOS only) for root-wide listings,
    // consulted BEFORE even opening the index: this path needs no
    // manifest, no postings, no freshness sweep, just the cache and the
    // pruned walk. The index opens below only on cache miss, and serves
    // every scoped listing (see `listing_cache_eligible`). The cache
    // lives in the discovered index dir, so GLEP_INDEX_PATH shares it.
    #[cfg(target_os = "macos")]
    if args.files && listing_cache_eligible(&args) {
        let glob = files_glob(&args)?;
        if let Some(listing) = cached_root_listing(&root, &cwd_rel, &mut scope, glob, &mut timings)
        {
            let CachedListing { mut files, tree } = listing;
            apply_sort(&mut files, &args, &root, &cwd_rel)?;
            print_paths(&files, &cwd_rel, &args)?;
            timings.stage("listing_cache");
            timings.finish();
            let code = exit_code(&args, had_error, !files.is_empty());
            // The run returns right here and the process exits: the
            // listing and the loaded tree (about 100k allocations on a
            // kernel-scale tree) are left to the exit instead of being
            // freed one by one. Nothing may use them, or run at all,
            // between this and the return.
            std::mem::forget(files);
            std::mem::forget(tree);
            return Ok(code);
        }
    }

    // A scoped listing (every directory operand below the root, spelled as
    // plain names: no `..`, nothing absolute outside the tree) lists
    // straight from a live sweep of its operands, without the index; see
    // `list_scoped`. A root-wide listing never gets here with operands
    // like these: its only operand is the root itself.
    if args.files {
        if let Some(prefixes) = scope.dir_prefixes() {
            if prefixes.iter().all(|p| names_below_root(p)) {
                return list_scoped(
                    &root,
                    &cwd_rel,
                    &args,
                    &mut scope,
                    &prefixes,
                    &mut timings,
                    had_error,
                );
            }
        }
    }

    // A content query resolves its patterns and chooses its index plan
    // now, before the index opens: none of that reads the index. A plan no
    // trigram narrows (Plan::All) gets nothing from the index but its file
    // set, which a full freshness sweep has to refresh first. A root-wide
    // one on macOS searches the listing cache's file set instead, which
    // the cache's pruned walk lists for a fraction of that sweep (see
    // `search_cached_listing`), unless --ttl is given: within its window
    // the index answers without a sweep, faster still. On a cache miss the
    // index answers as before. On a tree with no index yet the query
    // searches a live walk, as --no-index does, and builds nothing: the
    // first narrowed query builds the index.
    let query = if args.files {
        None
    } else {
        Some(plan_query(&args)?)
    };
    // -q asks only whether anything matches: the probe searches indexed
    // files the plan picks before any sweep, and a match there ends the
    // run (see `probe`). Otherwise the run goes on as below, skipping what
    // the probe searched already (see `Ahead`).
    let mut ahead = Ahead::default();
    if let Some(q) = &query {
        timings.stage("plan");
        if probe_eligible(&args) {
            if let Some(view) = QuickIndex::open(&root) {
                let mut opts = search_opts(&args, &cwd_rel);
                opts.fixed = q.fixed;
                let matcher = search::AnyMatcher::build(&q.pattern, &opts)?;
                let probed = probe(&root, &mut scope, q, view, early_check, &matcher, &opts)?;
                timings.stage("probe");
                match probed {
                    None => {
                        timings.finish();
                        return Ok(exit_code(&args, had_error, true));
                    }
                    Some(probed) => {
                        #[cfg(debug_assertions)]
                        test_pause("GLEP_TEST_PAUSE_AFTER_PROBE");
                        ahead = Ahead {
                            matcher: Some(matcher),
                            probed,
                        }
                    }
                }
            }
        }
        if q.plan == plan::Plan::All {
            #[cfg(target_os = "macos")]
            if listing_cache_eligible(&args) && args.ttl == 0 {
                if let Some(code) = search_cached_listing(
                    &root,
                    &cwd_rel,
                    &args,
                    &mut scope,
                    q,
                    &mut timings,
                    had_error,
                    &ahead,
                )? {
                    return Ok(code);
                }
            }
            if !root.join(".glep/manifest.bin").exists() {
                return run_live(
                    &root,
                    &cwd_rel,
                    &args,
                    &mut scope,
                    &mut timings,
                    had_error,
                    &ahead,
                );
            }
        }
    }

    // A listing rebuilds the listing cache below from the sweep that
    // builds or refreshes the index. That build stats directories after
    // the sweep, so it trusts no mtime from about when the sweep starts
    // on, and it records the ignore sources above the root as they stood
    // before the sweep read them (see `dircache::build_from_listing`).
    #[cfg(target_os = "macos")]
    let cache_seed = args.files.then(|| {
        (
            std::time::SystemTime::now(),
            crate::dircache::source_fingerprint(&root),
        )
    });

    // First query on a tree with no index yet: one sweep measures it. A
    // small tree builds its index inline from that same sweep. A bigger
    // one is answered from the sweep alone, at full-scan cost, and its
    // index is built afterwards by a detached low-priority child. The
    // manifest is the build's last write, so queries that arrive
    // meanwhile find none and take this path again, and the first query
    // after the build opens the index. GLEP_SYNC_BUILD forces the inline
    // build. Unix only: a Windows child inherits every inheritable
    // handle, the parent's stdout pipe included, so a reader would wait
    // for the whole build.
    #[cfg(unix)]
    let first_sweep = if std::env::var_os("GLEP_SYNC_BUILD").is_none()
        && !root.join(".glep/manifest.bin").exists()
    {
        let swept = walk::sweep_unsorted(&root)?;
        timings.stage("first_sweep");
        let bytes: u64 = swept.iter().map(|m| m.size).sum();
        let deferred = bytes > background_build_min_bytes();
        if std::env::var_os("GLEP_TIMING").is_some() {
            eprintln!(
                "glep timing: unindexed tree {} files, {bytes} bytes ({} build)",
                swept.len(),
                if deferred { "background" } else { "inline" }
            );
        }
        if deferred {
            let result = run_from_sweep(
                &root,
                &cwd_rel,
                &args,
                &mut scope,
                swept,
                &mut timings,
                had_error,
            );
            // The output is flushed by now: its reader never waits for the
            // spawn, let alone the build. A failed query spawns it too, as
            // an inline build ran before the search could fail.
            spawn_background_build(&root, &cwd, args.max_filesize);
            timings.stage("bg_spawn");
            timings.finish();
            return result;
        }
        Some(swept)
    } else {
        None
    };
    #[cfg(not(unix))]
    let first_sweep = None;

    let mut idx = match Index::open_or_build_swept(&root, args.max_filesize, first_sweep) {
        Ok(idx) => idx,
        // Another glep is building the index right now (parallel first
        // queries on a new tree): answer with a live scan, as --no-index
        // does, instead of failing or waiting for the build.
        Err(e) if e.is::<crate::index::IndexBusy>() => {
            return run_live(
                &root,
                &cwd_rel,
                &args,
                &mut scope,
                &mut timings,
                had_error,
                &ahead,
            );
        }
        Err(e) => return Err(e),
    };
    timings.stage("index_open");
    // Path filters scope the freshness sweep too: subtrees outside the
    // filter can't produce results, so sweeping them is wasted work. An
    // index this process just built needs no sweep at all: the build's
    // own sweep is the freshest there is (a later process starts with
    // swept_full unset), and the manifest answers every operand.
    let extra = if idx.swept_full() {
        Vec::new()
    } else if args.paths.is_empty() {
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
    // Every live file the scope can select (the --files listing, the
    // --files-without-match universe), split by the hidden flag.
    let scope_dirs = scope.dir_prefixes();
    let live_split = || {
        let (mut plain, mut hidden) = idx.live_files_split(args.hidden, scope_dirs.as_deref());
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
        if let Some((sweep_started, fingerprint)) = cache_seed {
            if idx.swept_full() && extra.is_empty() {
                #[cfg(debug_assertions)]
                test_pause("GLEP_TEST_PAUSE_BEFORE_DIRCACHE_BUILD");
                let listing = idx.live_file_metas(true);
                let tree = crate::dircache::build_from_listing(
                    &root,
                    &listing,
                    fingerprint,
                    sweep_started,
                );
                let _ = tree.save(&idx.dir);
            }
        }
        return Ok(exit_code(&args, had_error, !files.is_empty()));
    }

    // Every --files run returned above: this is the content query planned
    // before the index opened.
    let Some(Query {
        pattern,
        fixed: effective_fixed,
        utf8_only,
        case: plan_case,
        plan: query_plan,
    }) = query
    else {
        unreachable!("a content query is planned before the index opens");
    };

    // Binary-flagged files are candidates only when the mode can surface
    // them: -a/--binary, --null-data (NUL is a separator, not binary),
    // an explicit non-UTF-8 encoding (UTF-16 text is NUL-heavy until
    // transcoded), and --pre (a preprocessor turns a PDF into text). Under quit detection they can never emit anything,
    // and rg excludes them even under -c --include-zero (verified), so
    // they stay out.
    let search_binary =
        args.text || args.binary || args.null_data || !utf8_only || args.pre.is_some();
    let (mut plain, mut hidden) =
        idx.candidates_split(&query_plan, plan_case, args.hidden, search_binary);
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

    let found = ahead.search(&pattern, &root, &mut files, universe.as_ref(), &opts)?;
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
    fn root_relative_folds_dots_and_keeps_climbs() {
        assert_eq!(
            root_relative(Path::new("src"), Path::new("..")),
            PathBuf::new()
        );
        assert_eq!(
            root_relative(Path::new("src"), Path::new("./deep/../x")),
            PathBuf::from("src/x")
        );
        // Above the root: the leading `..` stays, and display_rel turns
        // it back into the same cwd-relative path.
        let up = root_relative(Path::new("a"), Path::new("../../x/y"));
        assert_eq!(up, PathBuf::from("../x/y"));
        assert_eq!(
            crate::search::display_rel(&up, Path::new("a")),
            Path::new("../../x/y")
        );
        let abs = std::env::temp_dir();
        assert_eq!(root_relative(Path::new("a"), &abs), abs);
    }

    /// Paths with names that sort before the separator byte (`-`, `.`,
    /// space, `!`), shared prefixes and, on Unix, non-UTF-8 bytes: the
    /// swept-path sort gives `Path` order.
    #[test]
    fn swept_paths_sort_in_path_order() {
        let mut paths: Vec<PathBuf> = [
            "a/y.rs",
            "a-b/x.rs",
            "a.c/z.rs",
            "a.rs",
            "a",
            "a b/c",
            "a!/d",
            "ab",
            "a/b/c/d",
            "a/b",
            "a/b.c",
            "a/b-c/e",
            "a/b/c",
            "z",
            ".hidden/x",
            ".a",
            "-",
            "a/.b",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            for raw in [&b"a/\xff/x"[..], b"a\xfe", b"\xc3\xa9/b", b"e/\x01"] {
                paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(raw)));
            }
        }
        let mut expected = paths.clone();
        expected.sort();
        for rotate in 0..paths.len() {
            let mut v = paths.clone();
            v.rotate_left(rotate);
            v.reverse();
            sort_swept_paths(&mut v);
            assert_eq!(v, expected);
        }
    }

    /// `--files '**'` filters nothing, which `files_glob` relies on to
    /// skip it: the glob it builds (`**/**`) matches every path, dotted,
    /// with a newline or non-UTF-8 bytes, at any depth, either case mode.
    #[test]
    fn match_all_glob_matches_every_path() {
        let mut paths: Vec<PathBuf> = ["a", ".a", "a/b/c", "a\nb", "-x", ".git/x", "a b/.c d"]
            .iter()
            .map(PathBuf::from)
            .collect();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(b"\xff/\xfe.c")));
        }
        for icase in [false, true] {
            let g = build_glob_icase("**", icase).unwrap();
            for p in &paths {
                assert!(g.is_match(p), "{p:?}");
            }
        }
    }

    #[test]
    fn prefixes_with_files_compares_whole_names() {
        let meta = |p: &str| walk::FileMeta {
            path: PathBuf::from(p),
            mtime_ns: 0,
            size: 0,
            hidden: false,
        };
        let metas = [meta("src/a.rs"), meta("srcx/b.rs"), meta("src2/c/d")];
        let prefixes: Vec<PathBuf> = ["src", "srcx", "sr", "src/a.rs", "src2/c", "nope", "src"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            prefixes_with_files(&metas, &prefixes),
            [true, true, false, true, true, false, true]
        );
        assert_eq!(prefixes_with_files(&[], &prefixes), [false; 7]);
    }

    /// Windows takes either separator in operands and swept paths alike.
    #[cfg(windows)]
    #[test]
    fn prefixes_with_files_takes_either_separator_on_windows() {
        let meta = |p: &str| walk::FileMeta {
            path: PathBuf::from(p),
            mtime_ns: 0,
            size: 0,
            hidden: false,
        };
        let metas = [meta("src\\a.rs"), meta("srcx\\b.rs"), meta("src2\\c\\d")];
        let prefixes: Vec<PathBuf> = ["src", "src2/c", "src2\\c", "sr", "src\\a.rs"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            prefixes_with_files(&metas, &prefixes),
            [true, true, true, false, true]
        );
    }

    #[cfg(unix)]
    #[test]
    fn bg_build_min_bytes_parses_like_max_filesize() {
        assert_eq!(parse_bg_build_min_bytes(None), 64 * 1024 * 1024);
        assert_eq!(parse_bg_build_min_bytes(Some("0")), 0);
        assert_eq!(parse_bg_build_min_bytes(Some("4096")), 4096);
        assert_eq!(parse_bg_build_min_bytes(Some("1K")), 1024);
        assert_eq!(parse_bg_build_min_bytes(Some(" 2M ")), 2 * 1024 * 1024);
        assert_eq!(parse_bg_build_min_bytes(Some("1G")), 1 << 30);
        for junk in ["", "junk", "-1", "1.5M", "64MB"] {
            assert_eq!(
                parse_bg_build_min_bytes(Some(junk)),
                BG_BUILD_MIN_BYTES,
                "{junk:?}"
            );
        }
    }

    /// `args` parsed as `run` parses them, patterns resolved.
    fn parsed(args: &[&str]) -> Args {
        use clap::FromArgMatches;
        let matches = command()
            .try_get_matches_from(std::iter::once("glep").chain(args.iter().copied()))
            .unwrap();
        let mut a = Args::from_arg_matches(&matches).unwrap();
        resolve_flag_families(&mut a, &matches).unwrap();
        a.pattern_set = pattern_list(&a).unwrap();
        a
    }

    /// The plan comes from the flags and patterns alone, so `run` knows
    /// it before the index opens: no trigram narrows a letter, a
    /// two-letter literal, `.*`, `\w+` or `[0-9]{2}`, nor any pattern
    /// under -v, --passthru, -c --include-zero, a non-UTF-8 -E or --pre,
    /// nor an alternation with such an arm. A three-letter literal
    /// narrows, also under -c, -l, --files-without-match, -i and -E
    /// utf-8.
    #[test]
    fn plan_query_decides_from_the_flags_alone() {
        for args in [
            &["q"][..],
            &["-c", "ab"],
            &["-c", ".*"],
            &["\\w+"],
            &["-c", "[0-9]{2}"],
            &["--files-without-match", ".*"],
            &["-v", "-c", "hello"],
            &["--passthru", "hello"],
            &["-c", "--include-zero", "hello"],
            &["-E", "utf-16le", "hello"],
            &["--pre", "cat", "hello"],
            &["-e", "hello", "-e", "q"],
        ] {
            let q = plan_query(&parsed(args)).unwrap();
            assert_eq!(q.plan, plan::Plan::All, "{args:?}");
        }
        for args in [
            &["hello"][..],
            &["-c", "hello"],
            &["-l", "hello"],
            &["--files-without-match", "hello"],
            &["-i", "HELLO"],
            &["-E", "utf-8", "hello"],
            &["-e", "hello", "-e", "world"],
        ] {
            let q = plan_query(&parsed(args)).unwrap();
            assert!(matches!(q.plan, plan::Plan::Groups(_)), "{args:?}");
        }
        let q = plan_query(&parsed(&["-F", "a.b"])).unwrap();
        assert!(q.fixed && q.utf8_only && q.pattern == "a.b");
        assert_eq!(q.case, plan::Case::Sensitive);
        let q = plan_query(&parsed(&["-F", "-i", "-e", "a.b", "-e", "c"])).unwrap();
        assert!(!q.fixed, "a union of literals is a regex");
        assert_eq!(q.case, plan::Case::Unicode);
        let q = plan_query(&parsed(&["-E", "utf-16le", "x"])).unwrap();
        assert!(!q.utf8_only);
    }

    #[test]
    fn names_below_root_takes_plain_names_only() {
        for p in ["src", "src/deep", ".github/workflows", "a b"] {
            assert!(names_below_root(Path::new(p)), "{p}");
        }
        for p in ["", "..", "../x", "/abs", "./src"] {
            assert!(!names_below_root(Path::new(p)), "{p}");
        }
    }
}
