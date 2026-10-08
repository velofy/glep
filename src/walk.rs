use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{ParallelVisitor, ParallelVisitorBuilder, WalkState};

#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    /// Relative to the sweep root.
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
    /// True when any component of `path` starts with '.'. Sweeps now
    /// include hidden files/dirs (see `sweep`'s doc comment); this flag is
    /// how the index and query layer decide, at query time, whether a file
    /// should be visible by default. Never true for anything under a
    /// `.git` or `.glep` directory, since those are excluded outright and
    /// never reach a `FileMeta` at all.
    pub hidden: bool,
}

/// True when `name` is a path component that must never be descended into
/// or emitted, at any depth: `.git` (so agents never see git object/index
/// internals) and `.glep` (glep's own index directory). This is a hard
/// exclusion, independent of gitignore rules and of the hidden flag below;
/// it applies even under `--hidden`.
fn is_hard_excluded_component(name: &std::ffi::OsStr) -> bool {
    name == std::ffi::OsStr::new(".git") || name == std::ffi::OsStr::new(".glep")
}

/// True when `name` (a single path component) starts with '.'. Uses a
/// lossy conversion, which is safe here: the only thing being tested is
/// whether the first byte is ASCII '.', and a leading ASCII byte survives
/// lossy UTF-8 conversion unchanged regardless of what invalid bytes (if
/// any) follow it elsewhere in the name.
fn component_is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// True when any component of a sweep-relative path starts with '.', e.g.
/// both `.env` and `.github/workflows/ci.yml` (the second via its `.github`
/// ancestor, even though `ci.yml` itself is not dot-prefixed). This is the
/// name-only half of the hidden rule; the second half — whether a `!`
/// whitelist rule in an ignore file rescues a dot component, in which
/// case the path isn't hidden at all — is `WhitelistChecker` below. The
/// `ignore` crate applies its own leaf-name hidden check only when the
/// ignore matchers return no verdict for a path, so a whitelisted
/// `.clang-format` is an ordinary file; likewise a whitelisted `.github/`
/// dir is descended into and its children judged by their own names. To
/// mirror that on our flat yield-everything sweep, a path stays hidden
/// only if EVERY dot-prefixed prefix of it fails to earn a whitelist
/// verdict.
pub fn path_is_hidden(rel: &Path) -> bool {
    rel.components().any(|c| match c {
        std::path::Component::Normal(s) => component_is_hidden(s),
        _ => false,
    })
}

/// Per-directory matcher set for whitelist checks, lazily built on first
/// use so the loading cost attaches only to directories that actually
/// contain dot-prefixed entries.
#[derive(Default)]
struct DirMatchers {
    ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
    git_exclude: Option<Gitignore>,
}

/// Whitelist rescue check for hidden candidates. One instance per sweep
/// worker; also usable standalone (cli's extra-file path) since
/// construction is free and every matcher load is lazily cached.
/// Which ignore sources participate in this walk. `all()` is the
/// default; each --no-ignore-* knob drops one source (its ignore AND
/// whitelist rules both go inert — the crate has no notion of "whitelists
/// in an inactive file").
#[derive(Clone, Copy)]
pub struct WalkFlags {
    pub dot: bool,     // .ignore
    pub vcs: bool,     // .gitignore
    pub exclude: bool, // .git/info/exclude
    pub global: bool,  // global gitignore
}

impl WalkFlags {
    pub fn all() -> Self {
        WalkFlags { dot: true, vcs: true, exclude: true, global: true }
    }
}

pub struct WhitelistChecker {
    /// Keyed by the directory's path relative to the sweep root ("" is the
    /// root itself).
    dirs: HashMap<PathBuf, DirMatchers>,
    global_loaded: bool,
    global: Option<Gitignore>,
    flags: WalkFlags,
}

impl Default for WhitelistChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl WhitelistChecker {
    pub fn new() -> Self {
        Self::with_flags(WalkFlags::all())
    }

    pub fn with_flags(flags: WalkFlags) -> Self {
        WhitelistChecker {
            dirs: HashMap::new(),
            global_loaded: false,
            global: None,
            flags,
        }
    }

    fn dir_matchers(&mut self, root: &Path, dir_rel: &Path) -> &DirMatchers {
        self.dirs.entry(dir_rel.to_path_buf()).or_insert_with(|| {
            let dir_abs = root.join(dir_rel);
            let load = |name: &str| -> Option<Gitignore> {
                let file = dir_abs.join(name);
                if !file.is_file() {
                    return None;
                }
                let mut b = GitignoreBuilder::new(dir_rel);
                if b.add(file).is_some() {
                    return None; // unreadable/invalid file: contributes nothing
                }
                b.build().ok()
            };
            let flags = self.flags;
            let git_exclude = if flags.exclude && dir_abs.join(".git").exists() {
                load(".git/info/exclude")
            } else {
                None
            };
            DirMatchers {
                // Each --no-ignore-* switch gates its matcher: the file's
                // ignore AND whitelist rules are both inert under it.
                ignore: if flags.dot { load(".ignore") } else { None },
                gitignore: if flags.vcs { load(".gitignore") } else { None },
                git_exclude,
            }
        })
    }

    /// The ignore chain's verdict for `rel` (a root-relative path) as the
    /// `ignore` crate would compute it: per-source, the deepest directory
    /// level with a non-None verdict wins; then .ignore beats .gitignore
    /// beats git-exclude beats global excludes. Matchers are rooted at
    /// their own directory, and `matched` strips the dir prefix itself.
    fn verdict(&mut self, root: &Path, rel: &Path, is_dir: bool) -> ignore::Match<()> {
        let mut m_ignore = ignore::Match::None;
        let mut m_gi = ignore::Match::None;
        let mut m_excl = ignore::Match::None;
        // Enclosing dirs of `rel`, deepest first ("" = the sweep root).
        let mut dir = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        loop {
            let dm = self.dir_matchers(root, &dir);
            if m_ignore.is_none() {
                if let Some(g) = &dm.ignore {
                    m_ignore = g.matched(rel, is_dir).map(|_| ());
                }
            }
            if m_gi.is_none() {
                if let Some(g) = &dm.gitignore {
                    m_gi = g.matched(rel, is_dir).map(|_| ());
                }
            }
            if m_excl.is_none() {
                if let Some(g) = &dm.git_exclude {
                    m_excl = g.matched(rel, is_dir).map(|_| ());
                }
            }
            if dir.as_os_str().is_empty()
                || (!m_ignore.is_none() && !m_gi.is_none() && !m_excl.is_none())
            {
                break;
            }
            dir = dir.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        }
        if !self.global_loaded {
            self.global_loaded = true;
            self.global = self.flags.global.then(|| Gitignore::global().0);
        }
        let m_global = self
            .global
            .as_ref()
            .map(|g| g.matched(rel, is_dir).map(|_| ()))
            .unwrap_or(ignore::Match::None);
        m_ignore.or(m_gi).or(m_excl).or(m_global)
    }

    /// Effective hiddenness for a yielded path, rg semantics: hidden iff
    /// some dot-prefixed prefix of `rel` is not rescued by a whitelist.
    /// `.clang-format` with `!.clang-format` is visible; children of a
    /// whitelisted `.github/` are visible; `.github/.env` (a second dot
    /// component of its own) still needs its own rescue.
    pub fn is_hidden(&mut self, root: &Path, rel: &Path) -> bool {
        if !path_is_hidden(rel) {
            return false;
        }
        let mut p = rel;
        loop {
            if p.file_name().is_some_and(|n| component_is_hidden(n))
                && !self.verdict(root, p, p != rel).is_whitelist()
            {
                return true;
            }
            match p.parent() {
                Some(parent) => p = parent,
                None => return false,
            }
        }
    }
}

/// Builds one `Collector` per worker thread, each with its own local buffer.
struct CollectorBuilder<'a> {
    root: &'a Path,
    global: &'a Mutex<Vec<FileMeta>>,
    /// False on the --no-ignore sweep: with ignore rules disabled there is
    /// no whitelist to consult, and hidden-ness is purely name-based.
    whitelists: bool,
    /// Which ignore sources' `!` rules may rescue — mirrors the walker's
    /// own toggles (a disabled source's whitelists go inert too).
    flags: WalkFlags,
}

impl<'s> ParallelVisitorBuilder<'s> for CollectorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Collector {
            root: self.root,
            local: Vec::new(),
            global: self.global,
            whitelists: self.whitelists,
            whitelist: WhitelistChecker::with_flags(self.flags),
        })
    }
}

/// Per-thread visitor. Accumulates into `local` without locking, then
/// flushes into `global` exactly once when the thread's traversal ends.
struct Collector<'a> {
    root: &'a Path,
    local: Vec<FileMeta>,
    global: &'a Mutex<Vec<FileMeta>>,
    whitelists: bool,
    whitelist: WhitelistChecker,
}

impl ParallelVisitor for Collector<'_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        match entry {
            Ok(e) => {
                if e.file_type().map_or(false, |t| t.is_file()) {
                    if let Ok(md) = e.metadata() {
                        let mtime_ns = md
                            .modified()
                            .ok()
                            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        let rel = e
                            .path()
                            .strip_prefix(self.root)
                            .unwrap_or(e.path())
                            .to_path_buf();
                        let hidden = if self.whitelists {
                            self.whitelist.is_hidden(self.root, &rel)
                        } else {
                            path_is_hidden(&rel)
                        };
                        self.local.push(FileMeta {
                            path: rel,
                            mtime_ns,
                            size: md.len(),
                            hidden,
                        });
                    }
                }
            }
            Err(err) => eprintln!("glep: {err}"),
        }
        WalkState::Continue
    }
}

impl Drop for Collector<'_> {
    fn drop(&mut self) {
        if !self.local.is_empty() {
            self.global.lock().unwrap().append(&mut self.local);
        }
    }
}

/// Parallel gitignore-aware sweep. Hidden (dot-prefixed) files and
/// directories are INCLUDED in the result, each with `FileMeta::hidden` set;
/// whether they are actually shown to the user is a query-time decision
/// (`Index::candidates`/`live_files`, gated by `--hidden`), not a sweep-time
/// one. The sole exception is `.git` and `.glep`, which are hard-excluded
/// at any depth by component name and never descended into or emitted,
/// regardless of the hidden flag or of gitignore rules: agents never want
/// git internals or glep's own index files as search results. Gitignore
/// files (`.gitignore`, `.git/info/exclude`, global excludes) still apply
/// exactly as before; a hidden file can also be gitignored, same rules
/// either way. Returns files sorted by relative path.
///
/// On macOS this dispatches to `walk_bulk::sweep_bulk`, a getattrlistbulk
/// based fast path that collapses the per-file stat() storm into one
/// syscall per directory (see walk_bulk.rs for the design). Setting the
/// env var GLEP_NO_BULK_SWEEP forces this portable walker instead. Any
/// `Err` from sweep_bulk also falls back to this walker, with a warning on
/// stderr, so a bug in the macOS-only fast path can never surface as a
/// hard failure, only as a missed speedup for that one sweep.
/// True when any ancestor of `root` has a `.ignore`, `.rgignore`,
/// `.gitignore`, or `.git/info/exclude` file that could apply to the
/// sweep. The portable walker loads these through `parents(true)`; the
/// macOS bulk sweep has no parent-chain machinery, so their presence
/// forces the portable path (correctness over speed — ancestor files are
/// a handful of stat calls to check, and most roots have none).
fn ancestors_have_ignore_files(root: &Path) -> bool {
    let mut dir = root.parent();
    while let Some(d) = dir {
        if d.join(".ignore").is_file()
            || d.join(".rgignore").is_file()
            || d.join(".gitignore").is_file()
            || (d.join(".git").exists() && d.join(".git/info/exclude").is_file())
        {
            return true;
        }
        dir = d.parent();
    }
    false
}

pub fn sweep(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
    #[cfg(target_os = "macos")]
    {
        if std::env::var_os("GLEP_NO_BULK_SWEEP").is_none()
            && !ancestors_have_ignore_files(root)
        {
            match crate::walk_bulk::sweep_bulk(root) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    eprintln!("glep: bulk sweep failed ({e}), falling back to walker sweep");
                }
            }
        }
    }
    sweep_walker(root)
}

/// Scoped sweep: same semantics as `sweep` (ignore files applied,
/// `.git`/`.glep` hard-excluded, hidden entries flagged) but only
/// descends into the given index-relative subtrees. Ancestor ignore files
/// still apply because `WalkBuilder::parents` loads them for each
/// subtree root. File (non-dir) prefixes are stat'ed and yielded
/// directly. Missing subtrees produce an `Err` entry on stderr like any
/// other path problem. Used by `Index::update_scoped` for queries
/// already restricted by path filters, where sweeping the rest of the
/// tree would be wasted work (issue #21).
///
/// Portable-walker-only by design, like `sweep_unfiltered`: a scoped
/// subtree is small, so the `getattrlistbulk` fast path is not worth the
/// extra ignore-stack machinery for ancestor rules here.
pub fn sweep_scoped(root: &Path, prefixes: &[PathBuf]) -> anyhow::Result<Vec<FileMeta>> {
    // A missing or root-anchored scope is just the full sweep.
    if prefixes.is_empty() || prefixes.iter().any(|p| p.as_os_str().is_empty()) {
        return sweep(root);
    }
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let mut wb = ignore::WalkBuilder::new(root.join(&prefixes[0]));
    for p in &prefixes[1..] {
        wb.add(root.join(p));
    }
    let walker = wb
        .require_git(false)
        .hidden(false)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        whitelists: true,
        flags: WalkFlags::all(),
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// Portable, non-macOS-specific sweep: `ignore::WalkParallel` plus a
/// `stat()`-class metadata() call per file. This is the sole implementation
/// on non-macOS platforms, and the fallback / correctness reference on
/// macOS (see `sweep` above and the differential tests below).
fn sweep_walker(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .hidden(false)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        whitelists: true,
        flags: WalkFlags::all(),
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// Live, index-bypassing sweep for `-L/--follow`: identical filtering
/// semantics to `sweep_walker` (gitignore/.ignore/global excludes apply,
/// `.git`/`.glep` hard-excluded) but follows symlinks during traversal.
/// Ignore rules are NOT bypassed here — `-L` only changes traversal.
///
/// Why this is a live-scan escape hatch and not an index mode: files
/// reached THROUGH a symlinked directory are not in the manifest at all
/// (the index sweep doesn't descend links), so index narrowing couldn't
/// find them; scanning the followed tree is the only sound option.
/// `include_hidden` gates hidden entries the same as `sweep_unfiltered`.
///
/// Portable walker only (like `sweep_unfiltered`): the macOS bulk path
/// has no loop detection and no link handling, and `-L` is a deliberate
/// full-scan path anyway.
pub fn sweep_follow(root: &Path, include_hidden: bool) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .hidden(!include_hidden)
        .follow_links(true)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        whitelists: true,
        flags: WalkFlags::all(),
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// `--require-git` on a non-repo tree: gitignore rules are inert so
/// files that the index sweep skips (gitignored) can still appear in
/// results — a live scan is the only sound option (the index doesn't
/// have them). `.ignore`/`.rgignore`/global excludes still apply; only
/// the git-derived sources are switched off. `include_hidden` gates
/// hidden entries the same as `sweep_unfiltered`.
pub fn sweep_no_git(root: &Path, include_hidden: bool) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!include_hidden)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        // gitignore-derived whitelists are inert here (git sources off),
        // but .ignore `!` rules still apply.
        whitelists: true,
        flags: WalkFlags { dot: true, vcs: false, exclude: false, global: false },
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// Live, index-bypassing sweep for `--no-ignore`: same shape as `sweep`
/// (root.is_dir() ensure with identical error text, Err-entry warnings on
/// stderr, output sorted by relative path, `.git`/`.glep` hard-excluded at
/// any depth) but built on a `WalkBuilder` with every ignore source
/// disabled, so gitignore'd and .ignore'd trees (node_modules, target,
/// etc.) are swept anyway. Nothing here reads or writes `.glep/`; this
/// function has no knowledge of the index at all, which is the point:
/// `--no-ignore` must never let an ignored tree enter the index (see the
/// module-level rationale this function's caller documents in cli.rs).
///
/// `include_hidden` gates hidden (dot-prefixed) entries at the walker
/// itself via `.hidden(!include_hidden)`, mirroring rg's own default:
/// with `include_hidden = false`, dot-prefixed files/dirs never reach the
/// result at all, so `FileMeta::hidden` is always false for what comes
/// back and callers must NOT re-filter by it (that would just be a no-op
/// pass over an already-hidden-free list). With `include_hidden = true`,
/// hidden entries are walked and `FileMeta::hidden` is set on them exactly
/// as `path_is_hidden` would compute it, same as `sweep`.
///
/// This is deliberately walker-only, not the macOS `walk_bulk::sweep_bulk`
/// fast path: an unfiltered scan is a deliberately slow escape hatch (full
/// rg-speed cost, every time, by design), not the hot path that fast path
/// exists to speed up.
pub fn sweep_unfiltered(root: &Path, include_hidden: bool, follow: bool) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .hidden(!include_hidden)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .follow_links(follow)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        whitelists: false,
        flags: WalkFlags::all(),
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

/// Portable sweep that stays on the root's filesystem (`--one-file-system`,
/// rg `-x`... wait, that's `--one-file-system` not `-x`): all ignore
/// sources on, hidden gated at the walker, and `same_file_system(true)`
/// descends only into directories whose `st_dev` equals the root's.
/// Deliberately walker-based (like `sweep_unfiltered`): the macOS bulk
/// sweep has no mount filtering, and mount-point subtrees must not enter
/// the index either — callers route this through the live-scan path.
/// Live sweep honoring individual ignore-source toggles
/// (`--no-ignore-dot`/`--no-ignore-vcs`/`--no-ignore-exclude`/
/// `--no-ignore-global`/`--no-ignore-parent`): like `--require-git`
/// outside a repo, a file excluded by a disabled source simply isn't in
/// the index, so the only sound path is a live scan — never the index.
/// Whitelist `!` rules in a DISABLED source go inert with it.
pub fn sweep_selective(
    root: &Path,
    include_hidden: bool,
    follow: bool,
    flags: WalkFlags,
    parents: bool,
) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .hidden(!include_hidden)
        .ignore(flags.dot)
        .git_ignore(flags.vcs)
        .git_exclude(flags.exclude)
        .git_global(flags.global)
        .parents(parents)
        .follow_links(follow)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        whitelists: true,
        flags,
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

pub fn sweep_one_fs(root: &Path, include_hidden: bool) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .hidden(!include_hidden)
        .same_file_system(true)
        .filter_entry(|entry| !is_hard_excluded_component(entry.file_name()))
        .build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
        whitelists: true,
        flags: WalkFlags::all(),
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

#[cfg(test)]
mod scoped_tests {
    use super::*;
    #[test]
    fn sweep_scoped_visits_only_scope() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("src");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("a.txt"), "x").unwrap();
        std::fs::write(dir.path().join("top.txt"), "y").unwrap();
        let metas = sweep_scoped(dir.path(), &[PathBuf::from("src")]).unwrap();
        assert_eq!(
            metas.iter().map(|m| m.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("src/a.txt")]
        );
    }

    #[test]
    fn sweep_scoped_empty_or_root_prefix_is_full_sweep() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let metas = sweep_scoped(dir.path(), &[PathBuf::from("")]).unwrap();
        assert_eq!(metas.len(), 1);
    }
}

#[cfg(test)]
mod tests {

#[test]
fn sweep_selective_parent_off() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    std::fs::write(root.join("b.txt"), "x").unwrap();
    // parent ignore file ignoring a.txt — should NOT apply with parents off
    std::fs::write(dir.path().join(".ignore"), "a.txt\n").unwrap();
    let flags = crate::walk::WalkFlags::all();
    let on: Vec<_> = crate::walk::sweep_selective(&root, false, false, flags, true)
        .unwrap().into_iter().map(|m| m.path).collect();
    let off: Vec<_> = crate::walk::sweep_selective(&root, false, false, flags, false)
        .unwrap().into_iter().map(|m| m.path).collect();
    eprintln!("on={on:?} off={off:?}");
    assert_eq!(off, vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
}

#[test]
fn sweep_selective_parent_off_inside_git_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    std::fs::write(root.join("x.log"), "x").unwrap();
    std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(dir.path().join(".ignore"), "a.txt\n").unwrap();
    let flags = crate::walk::WalkFlags::all();
    let off: Vec<_> = crate::walk::sweep_selective(&root, false, false, flags, false)
        .unwrap().into_iter().map(|m| m.path).collect();
    eprintln!("off={off:?}");
    // a.txt rescued (parent .ignore off), x.log still gitignored
    assert_eq!(off, vec![PathBuf::from("a.txt")]);
}

    use super::*;

    /// Convert a forward-slash path literal to the platform's native
    /// separator, for comparing against paths that came off a real sweep
    /// (which carry the OS's native separator).
    fn p(s: &str) -> String {
        s.replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    #[test]
    fn sweep_finds_files_sorted_and_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("ignored.log"), "nope").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".glep")).unwrap();
        std::fs::write(dir.path().join(".glep/manifest.bin"), "x").unwrap();

        let metas = sweep(dir.path()).unwrap();
        let paths: Vec<String> = metas
            .iter()
            .map(|m| m.path.to_string_lossy().into_owned())
            .collect();
        // .gitignore itself is now swept (hidden files are included at the
        // sweep level; query-time filtering is what hides them by default,
        // see Index::candidates/live_files). ignored.log stays excluded via
        // its own *.log rule, and .glep/ stays hard-excluded regardless.
        assert_eq!(paths, vec![p(".gitignore"), p("b.txt"), p("src/a.rs")]);
        let by_path = |name: &str| metas.iter().find(|m| m.path == PathBuf::from(name)).unwrap();
        assert!(by_path(".gitignore").hidden);
        assert!(!by_path("b.txt").hidden);
        assert!(!by_path("src/a.rs").hidden);
        assert!(by_path("b.txt").size == 5);
        assert!(by_path("b.txt").mtime_ns > 0);
    }

    #[test]
    fn sweep_missing_root_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");
        assert!(sweep(&missing).is_err());
    }

    #[test]
    fn sweep_unfiltered_missing_root_is_error_with_same_text() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");
        let err = sweep_unfiltered(&missing, false, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("{}: No such file or directory (os error 2)", missing.display())
        );
    }

    #[test]
    fn sweep_unfiltered_bypasses_gitignore_and_dot_ignore_but_hard_excludes_git_and_glep() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep me").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("skipped.log"), "would be gitignored").unwrap();
        std::fs::write(dir.path().join(".ignore"), "vendored.txt\n").unwrap();
        std::fs::write(dir.path().join("vendored.txt"), "would be .ignore'd").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".glep")).unwrap();
        std::fs::write(dir.path().join(".glep/manifest.bin"), "x").unwrap();

        let metas = sweep_unfiltered(dir.path(), false, false).unwrap();
        let paths: Vec<String> =
            metas.iter().map(|m| m.path.to_string_lossy().into_owned()).collect();
        // Both the gitignore'd and the .ignore'd file must be present: this
        // is the whole point of --no-ignore. .gitignore/.ignore themselves
        // are hidden (dot-prefixed) and include_hidden is false here, so
        // they're excluded too, same as keep.txt's non-dot siblings would
        // be included. .git and .glep never show up, at any depth.
        assert_eq!(paths, vec!["keep.txt", "skipped.log", "vendored.txt"]);
        assert!(!paths.iter().any(|p| p.contains(".git")));
        assert!(!paths.iter().any(|p| p.contains(".glep")));
    }

    #[test]
    fn sweep_unfiltered_hidden_gating_matches_include_hidden_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plain.txt"), "plain").unwrap();
        std::fs::write(dir.path().join(".dotfile"), "dotfile").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".glep")).unwrap();
        std::fs::write(dir.path().join(".glep/manifest.bin"), "x").unwrap();

        let without_hidden = sweep_unfiltered(dir.path(), false, false).unwrap();
        let paths: Vec<String> =
            without_hidden.iter().map(|m| m.path.to_string_lossy().into_owned()).collect();
        assert_eq!(paths, vec!["plain.txt"]);
        // With include_hidden = false, the walker itself dropped the hidden
        // entry, so nothing left over is flagged hidden either: callers
        // must not re-filter by FileMeta::hidden on top of this.
        assert!(without_hidden.iter().all(|m| !m.hidden));

        let with_hidden = sweep_unfiltered(dir.path(), true, false).unwrap();
        let mut paths2: Vec<String> =
            with_hidden.iter().map(|m| m.path.to_string_lossy().into_owned()).collect();
        paths2.sort();
        assert_eq!(paths2, vec![".dotfile", "plain.txt"]);
        assert!(!paths2.iter().any(|p| p.contains(".git")));
        assert!(!paths2.iter().any(|p| p.contains(".glep")));
        let by_path =
            |name: &str| with_hidden.iter().find(|m| m.path == PathBuf::from(name)).unwrap();
        assert!(by_path(".dotfile").hidden);
        assert!(!by_path("plain.txt").hidden);
    }

    /// Differential parity between the macOS getattrlistbulk fast path
    /// (`walk_bulk::sweep_bulk`) and the portable walker
    /// (`sweep_walker`, this file's original implementation, kept as the
    /// fallback and correctness reference). Both must agree exactly on a
    /// fixture that exercises nested directories, a root-level .gitignore,
    /// a NESTED .gitignore that only applies to its own subtree, a hidden
    /// file, a hidden directory containing a file, a `.github`-style nested
    /// hidden directory (whose leaf file is not itself dot-prefixed, only
    /// an ancestor is), and plain files. Hidden entries are now swept
    /// (included) rather than skipped; the fixture and the `expect` list
    /// below reflect that, and every emitted `FileMeta` (path, size,
    /// mtime_ns, and hidden) is compared exactly between the two sweeps.
    #[cfg(target_os = "macos")]
    mod bulk_parity {
        use super::*;

        fn build_fixture() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
            std::fs::create_dir_all(dir.path().join(".hidden_dir")).unwrap();
            std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();

            std::fs::write(dir.path().join("root.txt"), "root file").unwrap();
            std::fs::write(dir.path().join("skip.log"), "gitignored at root").unwrap();
            std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
            std::fs::write(dir.path().join(".hidden_file"), "dotfile, now swept").unwrap();
            std::fs::write(dir.path().join(".hidden_dir/inside.txt"), "hidden dir contents").unwrap();
            std::fs::write(dir.path().join(".github/workflows/x.yml"), "nested under a hidden dir").unwrap();

            std::fs::write(dir.path().join("sub/normal.txt"), "normal sub file").unwrap();
            std::fs::write(dir.path().join("sub/local.txt"), "ignored only under sub/").unwrap();
            std::fs::write(dir.path().join("sub/.gitignore"), "local.txt\n").unwrap();

            std::fs::write(dir.path().join("sub/deeper/nested.txt"), "deep file").unwrap();
            std::fs::write(dir.path().join("sub/deeper/also.log"), "still root-ignored").unwrap();
            dir
        }

        #[test]
        fn sweep_bulk_matches_sweep_walker_exactly() {
            let dir = build_fixture();

            let mut walker = sweep_walker(dir.path()).unwrap();
            let mut bulk = crate::walk_bulk::sweep_bulk(dir.path()).unwrap();
            walker.sort_by(|a, b| a.path.cmp(&b.path));
            bulk.sort_by(|a, b| a.path.cmp(&b.path));

            let walker_paths: Vec<_> = walker.iter().map(|m| m.path.clone()).collect();
            let bulk_paths: Vec<_> = bulk.iter().map(|m| m.path.clone()).collect();
            assert_eq!(walker_paths, bulk_paths, "sweep_bulk and sweep_walker disagree on file set");

            // Sanity: gitignore scoping actually took effect (root pattern
            // applies everywhere, nested pattern applies only under sub/),
            // AND hidden files/dirs are now swept: .gitignore itself,
            // .hidden_file, .hidden_dir/inside.txt, sub/.gitignore, and the
            // .github-style nested file all show up.
            let expect: Vec<PathBuf> = [
                ".github/workflows/x.yml",
                ".gitignore",
                ".hidden_dir/inside.txt",
                ".hidden_file",
                "root.txt",
                "sub/.gitignore",
                "sub/deeper/nested.txt",
                "sub/normal.txt",
            ]
            .iter()
            .map(PathBuf::from)
            .collect();
            assert_eq!(walker_paths, expect);

            assert_eq!(walker.len(), bulk.len());
            for (w, b) in walker.iter().zip(bulk.iter()) {
                assert_eq!(w.path, b.path);
                assert_eq!(w.size, b.size, "size mismatch for {:?}", w.path);
                assert_eq!(
                    w.mtime_ns, b.mtime_ns,
                    "mtime_ns resolution mismatch for {:?}: walker={} bulk={}",
                    w.path, w.mtime_ns, b.mtime_ns
                );
                assert_eq!(w.hidden, b.hidden, "hidden flag mismatch for {:?}", w.path);
                // Independently recompute what "hidden" should be (any path
                // component starting with '.') rather than trusting either
                // sweep's own logic, so a naive basename-only check (which
                // would wrongly say ci.yml-under-.github is not hidden)
                // gets caught.
                let should_be_hidden =
                    w.path.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
                assert_eq!(w.hidden, should_be_hidden, "hidden flag wrong for {:?}", w.path);
            }
            // Full-struct equality (path, mtime_ns, size, hidden all at
            // once), strictly in addition to the per-field checks above.
            assert_eq!(walker, bulk, "full FileMeta vectors must match exactly, hidden flags included");
        }

        /// The resolution check above (exact mtime_ns equality) is the
        /// unit-level guarantee; this is the end-to-end one. `Index::update`
        /// decides "did this file change" purely by comparing stored
        /// mtime_ns/size against a fresh sweep's mtime_ns/size. If the two
        /// sweep implementations disagreed on mtime_ns resolution (e.g. one
        /// truncated to whole seconds), every file would look changed the
        /// first time a query switched sweep paths, forcing a full reindex.
        /// Build with one path, update with the other, both directions:
        /// zero files should ever look reindexed.
        #[test]
        fn mtime_resolution_survives_switching_sweep_paths_zero_reindex() {
            use crate::index::Index;

            // Case 1: build via the walker path, update via the bulk path.
            let dir = build_fixture();
            std::env::set_var("GLEP_NO_BULK_SWEEP", "1");
            let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
            std::env::remove_var("GLEP_NO_BULK_SWEEP");
            idx.update(1_048_576, 0).unwrap();
            assert!(
                !dir.path().join(".glep/delta.bin").exists(),
                "walker-built index saw files as changed after switching to the bulk sweep"
            );

            // Case 2: build via the bulk path, update via the walker path.
            let dir2 = build_fixture();
            let mut idx2 = Index::build(dir2.path(), 1_048_576).unwrap();
            std::env::set_var("GLEP_NO_BULK_SWEEP", "1");
            idx2.update(1_048_576, 0).unwrap();
            std::env::remove_var("GLEP_NO_BULK_SWEEP");
            assert!(
                !dir2.path().join(".glep/delta.bin").exists(),
                "bulk-built index saw files as changed after switching to the walker sweep"
            );
        }
    }

    /// The five sweep_bulk vs. sweep_walker divergences an opus review
    /// found, and the fix in walk_bulk.rs (see its module docs and
    /// `build_ancestor_stack`) closes: gitignore sources above the sweep
    /// root that the bulk fast path never used to look at, plus `.ignore`/
    /// `.rgignore` files, whose precedence relative to `.gitignore` isn't
    /// reimplemented and instead trips the walker fallback (item B in the
    /// fix design). Each test below builds a fixture that reproduces
    /// exactly one divergence and asserts the *public* dispatcher (`sweep`,
    /// what `Index::build`/`update` actually call) matches `sweep_walker`
    /// exactly, the same correctness bar `bulk_parity` above holds the
    /// common-case fixture to. Scenarios 2 and 3 also assert `sweep_bulk`
    /// itself (not just the dispatcher after a fallback) gets the right
    /// answer, proving the new ancestor-matcher seeding actually runs on
    /// the fast path; scenarios 1 and 5 assert the opposite, that
    /// `sweep_bulk` refuses (`Err`) rather than approximate. Scenario 4
    /// (the global excludes file) now lives in
    /// tests/cli.rs::global_excludes_honored_identically_across_sweep_paths
    /// instead of here: it needs to mutate the process-global HOME env
    /// var, and doing that in-process raced against every other test in
    /// the binary that transitively reads HOME during a sweep, not just
    /// tests in this module; a subprocess per variant, which assert_cmd
    /// gives for free, makes HOME truly per-process instead. This module
    /// also carries a commondir-resolution test, a defect a later
    /// re-review found in the fix for these five: `resolve_gitdir_file`'s
    /// handling of a worktree whose `commondir` file is missing.
    #[cfg(target_os = "macos")]
    mod divergence_scenarios {
        use super::*;

        fn paths_of(metas: &[FileMeta]) -> Vec<String> {
            let mut v: Vec<String> =
                metas.iter().map(|m| m.path.to_string_lossy().into_owned()).collect();
            v.sort();
            v
        }

        /// Asserts the public dispatcher and the walker reference agree
        /// exactly on the file set for `root`, and returns that file set.
        fn assert_parity(root: &Path) -> Vec<String> {
            let dispatched = sweep(root).unwrap();
            let walked = sweep_walker(root).unwrap();
            let dispatched_paths = paths_of(&dispatched);
            let walked_paths = paths_of(&walked);
            assert_eq!(
                dispatched_paths, walked_paths,
                "sweep() and sweep_walker() disagree on file set for {}",
                root.display()
            );
            dispatched_paths
        }

        /// Scenario 1: an `.ignore` file at the sweep root
        /// (`vendored.txt` pattern). The bulk fast path doesn't know
        /// `.ignore` precedence, so it must trip the divergence trap
        /// (Fatal, item B) and defer the whole sweep to the walker, which
        /// excludes `vendored.txt` correctly via `.ignore`. `.ignore`
        /// itself is a hidden file with no gitignore rule excluding it, so
        /// it is now swept too (hidden files are included at sweep time;
        /// only .git/.glep are hard-excluded).
        #[test]
        fn dot_ignore_file_triggers_walker_fallback() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("keep.txt"), "keep me").unwrap();
            std::fs::write(dir.path().join("vendored.txt"), "vendored content").unwrap();
            std::fs::write(dir.path().join(".ignore"), "vendored.txt\n").unwrap();

            assert!(
                crate::walk_bulk::sweep_bulk(dir.path()).is_err(),
                "bulk sweep should refuse a directory with .ignore, not approximate it"
            );

            let files = assert_parity(dir.path());
            assert_eq!(files, vec![".ignore".to_string(), "keep.txt".to_string()]);
        }

        /// Scenario 2: a `.gitignore` above the sweep root (`*.log`), no
        /// `.git` anywhere. `build_ancestor_stack`'s ancestor-.gitignore
        /// search (item A) should pick this up on the bulk fast path
        /// itself, no fallback needed.
        #[test]
        fn ancestor_gitignore_above_root_is_honored() {
            let outer = tempfile::tempdir().unwrap();
            std::fs::write(outer.path().join(".gitignore"), "*.log\n").unwrap();
            let root = outer.path().join("sweep_root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("keep.txt"), "keep me").unwrap();
            std::fs::write(root.join("debug.log"), "noisy").unwrap();

            let bulk = crate::walk_bulk::sweep_bulk(&root)
                .expect("bulk sweep should honor an ancestor .gitignore directly, not fall back");
            assert_eq!(
                paths_of(&bulk),
                vec!["keep.txt".to_string()],
                "bulk sweep should honor the ancestor .gitignore's *.log rule"
            );

            let files = assert_parity(&root);
            assert_eq!(files, vec!["keep.txt".to_string()]);
        }

        /// Scenario 3: `.git/info/exclude` above the sweep root
        /// (`secret.txt`). `build_ancestor_stack` walks up to find the git
        /// root and loads its info/exclude (item A); also handled on the
        /// bulk fast path directly, no fallback needed.
        #[test]
        fn git_info_exclude_above_root_is_honored() {
            let outer = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(outer.path().join(".git/info")).unwrap();
            std::fs::write(outer.path().join(".git/info/exclude"), "secret.txt\n").unwrap();
            let root = outer.path().join("sweep_root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("keep.txt"), "keep me").unwrap();
            std::fs::write(root.join("secret.txt"), "shh").unwrap();

            let bulk = crate::walk_bulk::sweep_bulk(&root)
                .expect("bulk sweep should resolve .git/info/exclude directly, not fall back");
            assert_eq!(
                paths_of(&bulk),
                vec!["keep.txt".to_string()],
                "bulk sweep should honor .git/info/exclude"
            );

            let files = assert_parity(&root);
            assert_eq!(files, vec!["keep.txt".to_string()]);
        }

        /// Commondir defect: a `.git` gitdir-pointer file above the sweep
        /// root whose target directory exists but has NO `commondir` file,
        /// i.e. an orphaned/unlinked worktree. The `ignore` crate's own
        /// `resolve_git_commondir` (ignore-0.4.28/src/dir.rs) refuses to
        /// guess that the per-worktree dir doubles as the common dir in
        /// this case: it gives up and the caller falls back to an EMPTY
        /// exclude matcher (see `resolve_gitdir_file`'s doc comment in
        /// walk_bulk.rs). Parity means the bulk fast path must do the same:
        /// load NOTHING from `fake_gitdir/info/exclude`, not guess that it
        /// applies to the sweep root. `secret.txt` must therefore show up
        /// in BOTH sweeps.
        #[test]
        fn orphan_worktree_commondir_missing_matches_ignore_crate_empty_matcher() {
            let outer = tempfile::tempdir().unwrap();
            let gitdir_holder = tempfile::tempdir().unwrap();
            let fake_gitdir = gitdir_holder.path().join("fake_gitdir");
            std::fs::create_dir_all(fake_gitdir.join("info")).unwrap();
            std::fs::write(fake_gitdir.join("info/exclude"), "secret.txt\n").unwrap();
            // Deliberately no `fake_gitdir/commondir` file: this is the
            // orphaned-worktree case the fix targets.

            std::fs::write(
                outer.path().join(".git"),
                format!("gitdir: {}\n", fake_gitdir.display()),
            )
            .unwrap();

            let root = outer.path().join("sweep_root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("keep.txt"), "keep me").unwrap();
            std::fs::write(root.join("secret.txt"), "shh").unwrap();

            let bulk = crate::walk_bulk::sweep_bulk(&root).expect(
                "bulk sweep should resolve the gitdir pointer directly, not fall back, \
                 even though its commondir is missing",
            );
            let bulk_paths = paths_of(&bulk);
            assert!(
                bulk_paths.contains(&"secret.txt".to_string()),
                "bulk sweep must NOT guess fake_gitdir/info/exclude applies: the ignore \
                 crate gives up and uses an empty matcher when commondir is missing"
            );

            let files = assert_parity(&root);
            assert!(
                files.contains(&"secret.txt".to_string()),
                "sweep() and sweep_walker() must both include secret.txt: the ignore \
                 crate's own walker also uses an empty matcher here"
            );
            assert_eq!(
                files,
                vec!["keep.txt".to_string(), "secret.txt".to_string()],
                "both keep.txt and secret.txt should be swept, nothing else"
            );
        }

        /// Scenario 5: an `.ignore` whitelist (`!important.log`) overriding
        /// a `.gitignore` blanket `*.log`. The bulk fast path doesn't
        /// reimplement that cross-file precedence, so `.ignore`'s mere
        /// presence (item B) must trip the fallback, same mechanism as
        /// scenario 1, but here the walker's correct answer *includes* a
        /// file the old bulk path used to wrongly drop. `.gitignore` and
        /// `.ignore` themselves are hidden files with no rule excluding
        /// them, so both are now swept too.
        #[test]
        fn dot_ignore_whitelist_overrides_gitignore() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
            std::fs::write(dir.path().join(".ignore"), "!important.log\n").unwrap();
            std::fs::write(dir.path().join("important.log"), "keep this one").unwrap();
            std::fs::write(dir.path().join("other.log"), "still noisy").unwrap();
            std::fs::write(dir.path().join("keep.txt"), "keep me").unwrap();

            assert!(
                crate::walk_bulk::sweep_bulk(dir.path()).is_err(),
                "bulk sweep should refuse a directory with .ignore, not approximate it"
            );

            let files = assert_parity(dir.path());
            assert_eq!(
                files,
                vec![
                    ".gitignore".to_string(),
                    ".ignore".to_string(),
                    "important.log".to_string(),
                    "keep.txt".to_string(),
                ]
            );
        }
    }
}

#[cfg(test)]
mod wl_debug {
    #[test]
    fn dbg_whitelist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "x\n").unwrap();
        let mut wl = super::WhitelistChecker::new();
        eprintln!("gitignore hidden={}", wl.is_hidden(dir.path(), std::path::Path::new(".gitignore")));
        eprintln!("b hidden={}", wl.is_hidden(dir.path(), std::path::Path::new("b.txt")));
        eprintln!("verdict={:?}", wl.verdict(dir.path(), std::path::Path::new(".gitignore"), false));
    }
}
