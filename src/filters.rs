//! rg's per-entry filters, applied to file sets that come from the index
//! or a sweep instead of from rg's own directory walk.
//!
//! rg's walker checks every entry below an operand, directories included,
//! and an operand itself is never checked. For each entry the first
//! decisive answer wins, in this order (the `ignore` crate's
//! `Ignore::matched` plus the walker's hidden check):
//!
//! 1. -g/--iglob overrides: a matching `!glob` skips the entry (a skipped
//!    directory prunes its whole subtree), a matching glob keeps it; when
//!    positive globs exist, a file matching none of them is skipped.
//! 2. Ignore rules (.gitignore, .ignore, excludes, global, and last the
//!    --ignore-file rules): ignored entries are skipped; a whitelist
//!    (`!rule`) marks the entry whitelisted.
//! 3. -t/-T types (files only): a negated type skips, a selected type
//!    whitelists, and with any selection an unmatched file is skipped.
//! 4. Hidden: a dot-named entry is skipped unless --hidden or something
//!    above whitelisted it.
//!
//! The index already applied step 2's ignore files from the index root
//! down, so its files never carry an ignored level; what this module adds
//! is everything else, evaluated against the cwd-relative path rg would
//! see. Files the sweep flagged hidden are passed separately: only they
//! need the per-level hidden check, which an operand inside a hidden
//! directory or a -g/-t whitelist can override.

use crate::search::display_rel;
use crate::walk::WhitelistChecker;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use ignore::Match;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One --type-add / --type-clear / -t / -T, kept in command-line order:
/// rg applies them in that order, so `-T md -t md` selects md and
/// `--type-clear txt --type-add 'txt:*.foo' -t txt` selects only *.foo.
#[derive(Debug, Clone)]
pub enum TypeChange {
    Add(String),
    Clear(String),
    Select(String),
    Negate(String),
}

/// The type matcher for `changes`, built on the default definitions.
/// `None` when no type flag was given. Selecting an unknown or cleared
/// type is an error, as in rg.
pub fn build_types(changes: &[TypeChange]) -> anyhow::Result<Option<Types>> {
    if changes.is_empty() {
        return Ok(None);
    }
    let mut tb = TypesBuilder::new();
    tb.add_defaults();
    for c in changes {
        match c {
            TypeChange::Add(spec) => {
                tb.add_def(spec)
                    .map_err(|e| anyhow::anyhow!("{spec}: {e}"))?;
            }
            TypeChange::Clear(name) => {
                tb.clear(name);
            }
            TypeChange::Select(name) => {
                tb.select(name);
            }
            TypeChange::Negate(name) => {
                tb.negate(name);
            }
        }
    }
    Ok(Some(tb.build()?))
}

/// The -g/--iglob override matcher in rg's order: every -g rule
/// (case-insensitive under --glob-case-insensitive), then every --iglob
/// rule, always case-insensitive. `None` without globs.
pub fn build_overrides(
    globs: &[String],
    iglobs: &[String],
    glob_case_insensitive: bool,
) -> anyhow::Result<Option<Override>> {
    if globs.is_empty() && iglobs.is_empty() {
        return Ok(None);
    }
    let mut ob = OverrideBuilder::new("");
    ob.case_insensitive(glob_case_insensitive)?;
    for g in globs {
        ob.add(g)?;
    }
    ob.case_insensitive(true)?;
    for g in iglobs {
        ob.add(g)?;
    }
    Ok(Some(ob.build()?))
}

/// What rg's filters need from the command line.
pub struct FilterSpec<'a> {
    pub globs: &'a [String],
    pub iglobs: &'a [String],
    pub glob_case_insensitive: bool,
    pub type_changes: &'a [TypeChange],
    pub ignore_files: &'a [PathBuf],
    pub ignore_file_case_insensitive: bool,
    pub max_depth: Option<usize>,
    pub hidden: bool,
}

pub struct Filters {
    root: PathBuf,
    cwd_rel: PathBuf,
    overrides: Option<Override>,
    /// --ignore-file matchers, in command-line order; the last one with an
    /// opinion decides, and only where no ignore file in the tree has one.
    explicit: Vec<Gitignore>,
    types: Option<Types>,
    max_depth: Option<usize>,
    show_hidden: bool,
    /// Directory levels can be skipped by these filters: an override has a
    /// `!glob`, or an --ignore-file is in play.
    prune_dirs: bool,
    /// The tree's own ignore rules, consulted only to see whether they
    /// whitelist a level (rescuing a dot-named entry, or beating an
    /// --ignore-file rule).
    tree_rules: WhitelistChecker,
    /// Directory verdicts, keyed by (root-relative dir, hidden check).
    dirs: HashMap<(PathBuf, bool), bool>,
    /// The last file's parent and verdict: sorted input makes runs of
    /// files in one directory the common case.
    last_parent: Option<(PathBuf, usize, bool, bool)>,
}

impl Filters {
    /// Build the matchers once per run. An --ignore-file that cannot be
    /// read is reported (an ignore-file warning, as in rg) and skipped.
    pub fn new(spec: &FilterSpec<'_>, root: &Path, cwd_rel: &Path) -> anyhow::Result<Filters> {
        let overrides = build_overrides(spec.globs, spec.iglobs, spec.glob_case_insensitive)?;
        let mut explicit = Vec::new();
        for p in spec.ignore_files {
            // Rules match relative to the directory glep runs in, as rg's
            // --ignore-file does, wherever the file itself lives.
            let mut b = GitignoreBuilder::new(root.join(cwd_rel));
            b.case_insensitive(spec.ignore_file_case_insensitive)?;
            if let Some(e) = b.add(p) {
                crate::messages::ignore_warning(e);
            }
            explicit.push(b.build()?);
        }
        let types = build_types(spec.type_changes)?;
        let prune_dirs =
            overrides.as_ref().is_some_and(|o| o.num_ignores() > 0) || !explicit.is_empty();
        Ok(Filters {
            root: root.to_path_buf(),
            cwd_rel: cwd_rel.to_path_buf(),
            overrides,
            explicit,
            types,
            max_depth: spec.max_depth,
            show_hidden: spec.hidden,
            prune_dirs,
            tree_rules: WhitelistChecker::new(),
            dirs: HashMap::new(),
            last_parent: None,
        })
    }

    /// The override matcher, for walkers that apply it themselves.
    pub fn overrides(&self) -> Option<&Override> {
        self.overrides.as_ref()
    }

    /// The type matcher, for walkers that apply it themselves.
    pub fn types(&self) -> Option<&Types> {
        self.types.as_ref()
    }

    /// Whether rg would yield `f` (root-relative) when walking the operand
    /// whose path has `base` components. `hidden` is the sweep's flag:
    /// some dot-named component of `f` is not whitelisted by ignore rules.
    pub fn keep(&mut self, f: &Path, base: usize, hidden: bool) -> bool {
        let hidden_check = hidden && !self.show_hidden;
        let rules = self.overrides.is_some() || !self.explicit.is_empty() || self.types.is_some();
        if self.max_depth.is_none() && !rules && !hidden_check {
            return true;
        }
        let depth = f.components().count().saturating_sub(base);
        if self.max_depth.is_some_and(|d| depth > d) {
            return false;
        }
        if !rules && !hidden_check {
            return true;
        }
        if (self.prune_dirs || hidden_check)
            && depth > 1
            && !self.dirs_ok(f, base, depth, hidden_check)
        {
            return false;
        }
        self.level_ok(f, false, hidden_check)
    }

    /// Every directory strictly between the operand and `f` survives.
    fn dirs_ok(&mut self, f: &Path, base: usize, depth: usize, hidden_check: bool) -> bool {
        let parent = f.parent().unwrap_or(Path::new(""));
        if let Some((p, b, h, ok)) = &self.last_parent {
            if *b == base && *h == hidden_check && p == parent {
                return *ok;
            }
        }
        let mut ok = true;
        // Shallowest first, so a pruned directory decides early.
        let dirs: Vec<&Path> = f.ancestors().skip(1).take(depth - 1).collect();
        for d in dirs.into_iter().rev() {
            let key = (d.to_path_buf(), hidden_check);
            let verdict = match self.dirs.get(&key) {
                Some(&v) => v,
                None => {
                    let v = self.level_ok(d, true, hidden_check);
                    self.dirs.insert(key, v);
                    v
                }
            };
            if !verdict {
                ok = false;
                break;
            }
        }
        self.last_parent = Some((parent.to_path_buf(), base, hidden_check, ok));
        ok
    }

    /// rg's verdict for one level (a directory below the operand, or the
    /// file itself), with the steps described in the module docs.
    fn level_ok(&mut self, l: &Path, is_dir: bool, hidden_check: bool) -> bool {
        let shown = display_rel(l, &self.cwd_rel);
        if let Some(ov) = &self.overrides {
            match ov.matched(&*shown, is_dir) {
                Match::Ignore(_) => return false,
                Match::Whitelist(_) => return true,
                Match::None => {}
            }
        }
        // --ignore-file: the last file with an opinion decides.
        let explicit = self
            .explicit
            .iter()
            .rev()
            .map(|g| g.matched(&*shown, is_dir))
            .find(|m| !m.is_none())
            .map_or(Match::None, |m| m.map(|_| ()));
        let dot = hidden_check
            && l.file_name()
                .is_some_and(|n| n.as_encoded_bytes().first() == Some(&b'.'));
        let mut whitelisted = explicit.is_whitelist();
        if dot || explicit.is_ignore() {
            // The tree's own rules take precedence over --ignore-file, and
            // their whitelist is what rescues a dot-named level.
            if self
                .tree_rules
                .verdict(&self.root, l, is_dir)
                .is_whitelist()
            {
                whitelisted = true;
            } else if explicit.is_ignore() {
                return false;
            }
        }
        if !is_dir {
            if let Some(t) = &self.types {
                match t.matched(l, false) {
                    Match::Ignore(_) => return false,
                    Match::Whitelist(_) => whitelisted = true,
                    Match::None => {}
                }
            }
        }
        !dot || whitelisted
    }
}

/// A path operand, root-relative, in command-line order.
pub struct Operand {
    pub path: PathBuf,
    pub kind: OperandKind,
}

pub enum OperandKind {
    /// A file: searched as itself, whatever the filters say.
    File,
    /// A directory served from the index or a sweep's file set.
    Dir,
    /// A directory the index cannot serve, already walked live with rg's
    /// rules: these are its files, root-relative and in path order.
    Walked(Vec<PathBuf>),
}

/// Merge two path-sorted lists.
fn merge_sorted(a: Vec<PathBuf>, b: Vec<PathBuf>) -> Vec<PathBuf> {
    if b.is_empty() {
        return a;
    }
    if a.is_empty() {
        return b;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut a = a.into_iter().peekable();
    let mut b = b.into_iter().peekable();
    loop {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => x <= y,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        out.push(if take_a { a.next() } else { b.next() }.unwrap());
    }
    out
}

impl Filters {
    /// rg's file list for these operands, in output order: operands in
    /// command-line order, a file operand as itself, a directory as its
    /// surviving files in path order (a file under two operands appears
    /// twice, as in rg). `plain` and `hidden` are path-sorted; `hidden`
    /// holds the files the sweep flagged hidden.
    pub fn select(
        &mut self,
        operands: &[Operand],
        mut plain: Vec<PathBuf>,
        hidden: Vec<PathBuf>,
    ) -> Vec<PathBuf> {
        if let [op] = operands {
            match &op.kind {
                OperandKind::File => return vec![op.path.clone()],
                OperandKind::Walked(files) => return files.clone(),
                OperandKind::Dir => {}
            }
            let base = op.path.components().count();
            let p = &op.path;
            plain.retain(|f| (base == 0 || f.starts_with(p)) && self.keep(f, base, false));
            let hidden: Vec<PathBuf> = hidden
                .into_iter()
                .filter(|f| (base == 0 || f.starts_with(p)) && self.keep(f, base, true))
                .collect();
            return merge_sorted(plain, hidden);
        }
        let mut out = Vec::new();
        for op in operands {
            match &op.kind {
                OperandKind::File => out.push(op.path.clone()),
                OperandKind::Walked(files) => out.extend(files.iter().cloned()),
                OperandKind::Dir => {
                    let base = op.path.components().count();
                    let p = &op.path;
                    let mut mine = Vec::new();
                    for f in &plain {
                        if (base == 0 || f.starts_with(p)) && self.keep(f, base, false) {
                            mine.push(f.clone());
                        }
                    }
                    let mut mine_hidden = Vec::new();
                    for f in &hidden {
                        if (base == 0 || f.starts_with(p)) && self.keep(f, base, true) {
                            mine_hidden.push(f.clone());
                        }
                    }
                    out.extend(merge_sorted(mine, mine_hidden));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(globs: &'a [String]) -> FilterSpec<'a> {
        FilterSpec {
            globs,
            iglobs: &[],
            glob_case_insensitive: false,
            type_changes: &[],
            ignore_files: &[],
            ignore_file_case_insensitive: false,
            max_depth: None,
            hidden: false,
        }
    }

    #[test]
    fn negated_directory_glob_prunes_the_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let globs = vec!["!vendor".to_string()];
        let mut f = Filters::new(&spec(&globs), dir.path(), Path::new("")).unwrap();
        assert!(!f.keep(Path::new("vendor/x/a.txt"), 0, false));
        assert!(f.keep(Path::new("src/vendor.txt"), 0, false));
        assert!(f.keep(Path::new("src/b.txt"), 0, false));
        // The operand itself is never checked.
        assert!(f.keep(Path::new("vendor/x/a.txt"), 1, false));
    }

    #[test]
    fn globs_match_the_cwd_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let globs = vec!["deep/**".to_string()];
        let mut f = Filters::new(&spec(&globs), dir.path(), Path::new("src")).unwrap();
        assert!(f.keep(Path::new("src/deep/util.rs"), 1, false));
        assert!(!f.keep(Path::new("src/main.rs"), 1, false));
    }

    #[test]
    fn type_changes_apply_in_command_line_order() {
        let changes = vec![
            TypeChange::Negate("md".into()),
            TypeChange::Select("md".into()),
        ];
        let t = build_types(&changes).unwrap().unwrap();
        assert!(t.matched("README.md", false).is_whitelist());
        let changes = vec![
            TypeChange::Select("md".into()),
            TypeChange::Negate("md".into()),
        ];
        let t = build_types(&changes).unwrap().unwrap();
        assert!(t.matched("README.md", false).is_ignore());
        let changes = vec![
            TypeChange::Add("txt:*.foo".into()),
            TypeChange::Clear("txt".into()),
            TypeChange::Select("txt".into()),
        ];
        assert!(build_types(&changes).is_err());
    }

    #[test]
    fn merge_keeps_path_order() {
        let a = vec![PathBuf::from("a"), PathBuf::from("c/x")];
        let b = vec![
            PathBuf::from(".h"),
            PathBuf::from("b/.y"),
            PathBuf::from("d"),
        ];
        let m: Vec<String> = merge_sorted(a, b)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(m, [".h", "a", "b/.y", "c/x", "d"]);
    }
}
