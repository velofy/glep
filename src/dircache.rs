//! Directory-mtime pruning cache for file-set queries (`--files`, and
//! content queries no trigram narrows).
//!
//! A content query the index narrows needs every file's current (mtime,
//! size) so the freshness diff can reindex what changed, that costs a full
//! bulk sweep (~100ms on a kernel-scale tree) and is inherent. A *listing*
//! query only needs the SET of files to be fresh, and so does a content
//! query no trigram narrows (Plan::All), which searches every file of the
//! set and reads each one's bytes as it is now. That set only changes via
//! create/delete/rename, each of which bumps the mtime of the directory
//! whose direct entries changed. Crucially, directory mtimes do NOT
//! propagate upward (a file created in `a/b/c` bumps `a/b/c`'s mtime and
//! nothing else's), so only a LEAF directory's whole file set is certified
//! by its own mtime. Listing queries therefore walk the tree listing
//! interior directories, but take each LEAF directory's cached listing
//! wholesale whenever its mtime matches the value observed in the
//! parent's bulk listing. On a kernel-scale tree this turns ~5,500 bulk
//! scans into ~600 interior listings plus a handful of stats.
//!
//! Soundness of pruning a LEAF directory L (child of live-scanned P):
//!
//!   1. L's mtime (observed in P's bulk listing) equals the cached value.
//!      That certifies L's direct entry set is identical: any create,
//!      delete or rename inside L, including adding or removing a
//!      subdirectory, which would make L non-leaf, bumps L's mtime.
//!      In-place content edits never change the file set.
//!   2. L's own `.gitignore`, `.ignore` and `.rgignore` (mtime, size) are
//!      re-statted and match the cached values (only stat'ed when L
//!      recorded one): in-place edits of an ignore file flip verdicts
//!      without touching any mtime.
//!   3. Every ancestor P on the path from the sweep root down was itself
//!      scanned live this run, and each one's own `.gitignore` (mtime,
//!      size) matched its cached node's recorded values
//!      (`chain_stable`): the cached listing was filtered by exactly
//!      those matchers.
//!   4. Ignore sources ABOVE the sweep root (global excludes,
//!      `.git/info/exclude`, ancestor `.gitignore`, `.ignore` and
//!      `.rgignore` files) are captured as a (path, mtime, size)
//!      fingerprint before the sweep a cache is built from, and re-statted
//!      on every use; any difference discards the whole cache. (An in-tree
//!      `.ignore` / `.rgignore` is only ever served from a pruned leaf: in
//!      a directory the walk lists live, its presence makes the listing
//!      fall back to the portable walker.)
//!
//! Every recorded mtime must predate the listing it certifies. The pruned
//! walk observes a directory's mtime in its parent's listing before it
//! reads the directory, so what it records can only be older than what it
//! read. A cache built from a full sweep (`build_from_listing`) stats each
//! directory and `.gitignore` after the sweep instead, so a file created
//! in between would leave the new mtime next to the old listing, and every
//! later pruned walk would serve that listing. Such a build records any
//! directory or ignore-file mtime from the sweep's start onwards as
//! unknown: a zero directory mtime never prunes (the next listing rescans
//! it live), and an unknown `.gitignore` matches no observed one, so
//! nothing below it is pruned until a live scan has recorded it again.
//! That comparison takes the filesystem's mtimes to come from the clock
//! the sweep's start is read from, finely enough to fall after it (APFS
//! keeps nanoseconds); taking each directory's mtime from the sweep's own
//! listing of its parent instead would need neither.
//!
//! The cache lives in `.glep/dirtree.bin`, is written atomically
//! (tmp + rename), and is independent of the manifest's trigram postings:
//! queries it answers never touch, update, or write the manifest or
//! `postings.bin`. When the cache is missing, stale, or the pruned walk
//! finds too much churn (more than `RESCAN_BUDGET` live scans), listing
//! falls back to the ordinary full sweep and rebuilds the cache from it
//! (a content query falls back to the index path, which leaves the cache
//! to the next listing), so a stale or even corrupt cache can only cost
//! speed, never correctness. `GLEP_NO_DIRCACHE=1` disables the whole
//! mechanism.

// Built and unit-tested on every platform; only the macOS listing path
// (cli.rs, walk_bulk.rs) uses it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::walk::FileMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bumped whenever a cached listing could differ from what the current
/// walker would list (2: `.rgignore` files are honored, and leaves record
/// their own `.ignore`/`.rgignore`), so an older cache is rebuilt once.
/// Recording untrusted mtimes as zero or unknown (see `build_from_listing`)
/// kept the layout, and an older reader takes either for a change too, so
/// builds before and after it share one cache.
pub const SCHEMA: u32 = 2;

/// The `gitignore_size` of a node whose ignore state is unknown: its
/// directory or its `.gitignore` changed during or after the sweep the
/// cache was built from. No observed `.gitignore` has this size and a
/// missing one is (0, 0), so the chain-stability check never matches it
/// and nothing below the node is pruned until a live scan records it.
pub const UNKNOWN_SIZE: u64 = u64::MAX;

/// Give up on the pruned walk and fall back to the full sweep once this
/// many directories have had to be re-scanned live. The pruned path's
/// whole point is that an unchanged tree only lists interior directories
/// (~1.1k on a kernel-scale tree; leaves come from cache); a tree with
/// heavy churn is better served by the parallel full sweep. Break-even is
/// ~2000 sequential listings vs a ~120ms parallel sweep, so 4096 keeps
/// even pathological shapes correct while comfortably covering real
/// monorepo-scale interior counts.
pub const RESCAN_BUDGET: usize = 4096;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IgnoreSrc {
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DirNode {
    /// Path relative to the sweep root ("" for the root itself).
    pub rel: PathBuf,
    /// This directory's mtime as observed in its PARENT's listing. Zero
    /// when unknown: for the root node (nobody observes it), and for a
    /// directory whose mtime may postdate its cached listing (see
    /// `build_from_listing`). A zero never matches, so such a leaf is
    /// always rescanned live.
    pub mtime_ns: u128,
    /// This directory's own `.gitignore` (mtime, size): (0, 0) when it has
    /// none, (0, `UNKNOWN_SIZE`) when unknown. Drives the `chain_stable`
    /// prune precondition.
    pub gitignore_mtime_ns: u128,
    pub gitignore_size: u64,
    /// Any `.gitignore` at or below this node (including itself).
    pub has_ignore_subtree: bool,
    /// This directory's own `.ignore` and `.rgignore` (file name, mtime,
    /// size), recorded when the cache is built from a full sweep (a zero
    /// mtime when unknown). Only a pruned leaf can hold any (a directory
    /// listed live with one falls back to the walker), and the prune
    /// re-stats each one.
    pub dot_ignores: Vec<IgnoreSrc>,
    /// Immediate files that survived ignore filtering at cache time,
    /// hidden included (each carries its own `hidden` flag).
    pub files: Vec<FileMeta>,
    /// Filtered subdirectories, recursive.
    pub subdirs: Vec<DirNode>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DirTree {
    pub schema: u32,
    /// Fingerprint of every ignore source above the sweep root, as
    /// resolved at cache time (see module docs, precondition 4).
    pub fingerprint: Vec<IgnoreSrc>,
    pub root: DirNode,
}

/// Whether the file at `p` still has the recorded (mtime, size); false
/// when it cannot be stat'ed, and for a zero (unknown) recorded mtime.
pub fn unchanged(p: &Path, mtime_ns: u128, size: u64) -> bool {
    if mtime_ns == 0 {
        return false;
    }
    match std::fs::metadata(p) {
        Ok(md) => mtime_of(&md) == mtime_ns && md.len() == size,
        Err(_) => false,
    }
}

/// `md`'s mtime in nanoseconds since the epoch, 0 when unavailable.
fn mtime_of(md: &std::fs::Metadata) -> u128 {
    md.modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn stat_src(p: PathBuf) -> IgnoreSrc {
    match std::fs::metadata(&p) {
        Ok(md) => IgnoreSrc {
            path: p,
            mtime_ns: mtime_of(&md),
            size: md.len(),
        },
        Err(_) => IgnoreSrc {
            path: p,
            mtime_ns: 0,
            size: 0,
        },
    }
}

/// Fingerprint every ignore source above (and including git plumbing for)
/// `root`: the global excludes file (resolved exactly like the `ignore`
/// crate resolves it, via `gitconfig_excludes_path`), `.git/info/exclude`
/// of the enclosing repo, every ancestor `.gitignore` up to the git root
/// (bounded by the same cap the bulk walker uses), and every ancestor
/// `.ignore` and `.rgignore`. Mirrors `walk_bulk::build_ancestor_stack` and
/// `build_ancestor_dot_ignores` so they never disagree about what "above
/// the root" means.
pub fn source_fingerprint(root: &Path) -> Vec<IgnoreSrc> {
    let mut v = Vec::new();
    if let Some(p) = ignore::gitignore::gitconfig_excludes_path() {
        v.push(stat_src(p));
    }
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let git_root = crate::walk::find_git_root(&canon);
    if let Some(gr) = &git_root {
        v.push(stat_src(gr.join(".git/info/exclude")));
    }
    // Ancestor .gitignore files from the root's parent up to the git root
    // (or the walker's cap when there is no git root).
    let mut ancestors: Vec<PathBuf> = Vec::new();
    if git_root.as_deref() != Some(canon.as_path()) {
        let mut cur = canon.parent();
        let mut depth = 0usize;
        while let Some(dir) = cur {
            ancestors.push(dir.to_path_buf());
            let hit_git_root = git_root.as_deref() == Some(dir);
            depth += 1;
            if hit_git_root || (git_root.is_none() && depth >= crate::walk::ANCESTOR_GITIGNORE_CAP)
            {
                break;
            }
            cur = dir.parent();
        }
    }
    for dir in ancestors {
        let gi = dir.join(".gitignore");
        if gi.is_file() {
            v.push(stat_src(gi));
        }
    }
    // Every `.ignore` and `.rgignore` above the root, all the way up, like
    // the bulk walker's `build_ancestor_dot_ignores`: creating, editing or
    // deleting one changes the fingerprint and discards the cache.
    for dir in canon.ancestors().skip(1) {
        for name in [".ignore", crate::walk::RGIGNORE] {
            let f = dir.join(name);
            if f.is_file() {
                v.push(stat_src(f));
            }
        }
    }
    v
}

pub fn cache_path(index_dir: &Path) -> PathBuf {
    index_dir.join("dirtree.bin")
}

impl DirNode {
    /// An empty node for `rel` (the aborted-walk placeholder).
    pub fn default_for(rel: &Path) -> DirNode {
        DirNode {
            rel: rel.to_path_buf(),
            mtime_ns: 0,
            gitignore_mtime_ns: 0,
            gitignore_size: 0,
            has_ignore_subtree: false,
            dot_ignores: Vec::new(),
            files: Vec::new(),
            subdirs: Vec::new(),
        }
    }
}

impl DirTree {
    pub fn load(index_dir: &Path) -> Option<DirTree> {
        if std::env::var_os("GLEP_NO_DIRCACHE").is_some() {
            return None;
        }
        let bytes = std::fs::read(cache_path(index_dir)).ok()?;
        let tree: DirTree = bincode::deserialize(&bytes).ok()?;
        if tree.schema != SCHEMA {
            return None;
        }
        Some(tree)
    }

    pub fn save(&self, index_dir: &Path) -> anyhow::Result<()> {
        let tmp = cache_path(index_dir).with_extension("tmp");
        std::fs::write(&tmp, bincode::serialize(self)?)?;
        std::fs::rename(tmp, cache_path(index_dir))?;
        Ok(())
    }

    /// Fingerprint validity: same sources, same (mtime, size) each.
    pub fn fingerprint_matches(&self, root: &Path) -> bool {
        self.fingerprint == source_fingerprint(root)
    }
}

/// Build a DirTree from a flat, already-filtered listing (the full-sweep
/// fallback path). Directories are grouped by parent path; each unique
/// directory is stat'ed once for its mtime, probed once for a
/// `.gitignore` (existence + stat), and `has_ignore_subtree` is folded
/// bottom-up. This costs ~2 stats per directory but only runs when the
/// cache is being built from scratch or after heavy churn.
///
/// `sweep_started` is when the sweep that produced `listing` began, and
/// `fingerprint` should be taken before it. The stats here come after
/// that sweep, so an mtime from `sweep_started` onwards may belong to a
/// change the listing missed: such a directory is recorded with a zero
/// mtime and an unknown `.gitignore`, such a `.gitignore` as unknown,
/// such an `.ignore`/`.rgignore` with a zero mtime, and such a
/// fingerprint source as unknown, which no later stat matches (see the
/// module docs).
pub fn build_from_listing(
    root: &Path,
    listing: &[FileMeta],
    mut fingerprint: Vec<IgnoreSrc>,
    sweep_started: std::time::SystemTime,
) -> DirTree {
    let started_ns = sweep_started
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // An mtime from the sweep's start on may postdate what the sweep
    // read. Zero is no mtime at all (a missing source, or none reported).
    let racy = |mtime_ns: u128| mtime_ns != 0 && mtime_ns >= started_ns;
    for src in &mut fingerprint {
        if racy(src.mtime_ns) {
            src.mtime_ns = 0;
            src.size = UNKNOWN_SIZE;
        }
    }
    // parent path -> (node being built, children rel paths)
    let mut dirs: BTreeMap<PathBuf, (DirNode, Vec<PathBuf>)> = BTreeMap::new();
    let ensure_dir =
        |rel: &Path, dirs: &mut BTreeMap<PathBuf, (DirNode, Vec<PathBuf>)>| -> PathBuf {
            // ensure an entry for rel AND all its ancestors (root = "")
            let mut chain = Vec::new();
            let mut cur = rel.to_path_buf();
            loop {
                chain.push(cur.clone());
                match cur.parent() {
                    Some(p) if p != cur => cur = p.to_path_buf(),
                    _ => break,
                }
            }
            for c in chain {
                dirs.entry(c.clone())
                    .or_insert_with(|| (DirNode::default_for(&c), Vec::new()));
            }
            rel.to_path_buf()
        };

    for meta in listing {
        let parent = match meta.path.parent() {
            Some(p) => p.to_path_buf(),
            None => PathBuf::new(),
        };
        ensure_dir(&parent, &mut dirs);
        let node = &mut dirs.get_mut(&parent).unwrap().0;
        // The directory's own `.ignore`/`.rgignore`, with the sweep's
        // (mtime, size): a pruned leaf re-stats them.
        if let Some(name) = meta
            .path
            .file_name()
            .filter(|n| *n == ".ignore" || *n == crate::walk::RGIGNORE)
        {
            node.dot_ignores.push(IgnoreSrc {
                path: PathBuf::from(name),
                mtime_ns: if racy(meta.mtime_ns) {
                    0
                } else {
                    meta.mtime_ns
                },
                size: meta.size,
            });
        }
        node.files.push(meta.clone());
    }

    // stat each dir + probe .gitignore
    let mut mtimes: BTreeMap<PathBuf, (u128, u128, u64, bool)> = BTreeMap::new();
    for rel in dirs.keys() {
        let abs = root.join(rel);
        let mtime_ns = std::fs::metadata(&abs).map_or(0, |md| mtime_of(&md));
        // Changed during or after the sweep (or gone since): both its
        // listing and its own ignore files may be newer than what the
        // sweep read.
        let unknown = mtime_ns == 0 || racy(mtime_ns);
        let (gi_m, gi_s, gi_has) = if unknown {
            (0, UNKNOWN_SIZE, true)
        } else {
            match std::fs::metadata(abs.join(".gitignore")) {
                Ok(gmd) if racy(mtime_of(&gmd)) => (0, UNKNOWN_SIZE, true),
                Ok(gmd) => (mtime_of(&gmd), gmd.len(), true),
                Err(_) => (0, 0, false),
            }
        };
        // Nobody observes the root's own mtime (no parent lists it).
        let mtime_ns = if unknown || rel.as_os_str().is_empty() {
            0
        } else {
            mtime_ns
        };
        mtimes.insert(rel.clone(), (mtime_ns, gi_m, gi_s, gi_has));
    }

    // Fold children into parents (BTreeMap iterates in sorted order;
    // deepest-first assembly via reverse iteration keeps it one pass).
    // Each directory's children register in descending order, reversed
    // here into the ascending order the pruned walk assembles, and each
    // assembled child moves into its parent, which is assembled next.
    let keys: Vec<PathBuf> = dirs.keys().cloned().collect();
    for key in keys.iter().rev() {
        let (mtime_ns, gi_m, gi_s, gi_has) = mtimes[key];
        // detach this node's data
        let (mut node, children) = dirs.remove(key).unwrap();
        node.mtime_ns = mtime_ns;
        node.gitignore_mtime_ns = gi_m;
        node.gitignore_size = gi_s;
        node.subdirs = children
            .into_iter()
            .rev()
            .filter_map(|c| dirs.remove(&c).map(|n| n.0))
            .collect();
        node.has_ignore_subtree = gi_has || node.subdirs.iter().any(|s| s.has_ignore_subtree);
        // re-insert without pending-children bookkeeping
        dirs.insert(key.clone(), (node, Vec::new()));
        // register with parent
        if key != Path::new("") {
            let parent = match key.parent() {
                Some(p) => p.to_path_buf(),
                None => PathBuf::new(),
            };
            if let Some(p) = dirs.get_mut(&parent) {
                p.1.push(key.clone());
            }
        }
    }

    let root_node = dirs
        .remove(&PathBuf::new())
        .map(|(n, _)| n)
        .unwrap_or_else(|| DirNode::default_for(Path::new("")));
    DirTree {
        schema: SCHEMA,
        fingerprint,
        root: root_node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// A sweep that started after the fixture was written: the build
    /// trusts every mtime in it.
    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(60)
    }

    fn meta(p: &str) -> FileMeta {
        FileMeta {
            path: PathBuf::from(p),
            mtime_ns: 1,
            size: 1,
            hidden: p.starts_with('.'),
        }
    }

    #[test]
    fn build_from_listing_groups_by_parent_and_folds_ignore() {
        let dir = tempfile::tempdir().unwrap();
        // tree: root/.gitignore, root/a.txt, root/sub/b.rs, root/sub/deep/c.rs
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::write(dir.path().join("sub/b.rs"), "fn x() {}").unwrap();
        std::fs::write(dir.path().join("sub/deep/c.rs"), "fn y() {}").unwrap();
        let listing = vec![
            meta(".gitignore"),
            meta("a.txt"),
            meta("sub/b.rs"),
            meta("sub/deep/c.rs"),
        ];
        let tree = build_from_listing(dir.path(), &listing, vec![], later());
        assert!(tree.root.gitignore_size > 0, "root .gitignore recorded");
        assert!(tree.root.has_ignore_subtree);
        assert_eq!(tree.root.files.len(), 2, "root immediate files");
        assert_eq!(tree.root.subdirs.len(), 1);
        let sub = &tree.root.subdirs[0];
        assert_eq!(sub.rel, PathBuf::from("sub"));
        assert!(!sub.gitignore_size > 0, "sub has no own .gitignore");
        assert!(!sub.has_ignore_subtree, "no .gitignore under sub/");
        assert_eq!(sub.files.len(), 1);
        assert_eq!(sub.subdirs.len(), 1);
        assert_eq!(sub.subdirs[0].files.len(), 1);
    }

    #[cfg(unix)]
    fn ns(t: SystemTime) -> u128 {
        t.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    /// Set `p`'s mtime. Unix only: a read-only handle (all a directory
    /// opens as) can set it there, not on Windows.
    #[cfg(unix)]
    fn set_mtime(p: &Path, t: SystemTime) {
        std::fs::File::open(p).unwrap().set_modified(t).unwrap();
    }

    /// A build stats directories after the sweep that listed them, so it
    /// distrusts every mtime from that sweep's start onwards: such a
    /// directory gets a zero mtime and an unknown `.gitignore`, such a
    /// `.gitignore` is unknown, such a `.ignore` gets a zero mtime, and
    /// such a fingerprint source is unknown. Older ones are kept, a missing
    /// source included. The root's own mtime is never recorded, and
    /// children come in path order.
    #[cfg(unix)]
    #[test]
    fn build_distrusts_mtimes_from_its_own_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for d in ["old", "oldgi", "dot"] {
            std::fs::create_dir(p.join(d)).unwrap();
        }
        for f in [
            "old/a.txt",
            "old/.gitignore",
            "oldgi/c.txt",
            "oldgi/.gitignore",
            "dot/d.txt",
            "dot/.ignore",
        ] {
            std::fs::write(p.join(f), "x\n").unwrap();
        }
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        for f in ["old/.gitignore", "old", "oldgi", "dot"] {
            set_mtime(&p.join(f), hour_ago);
        }
        // The sweep starts. Then a directory is created, and two ignore
        // files are rewritten in place (their directories keep their
        // mtimes); the root is set back, as if `fresh` had been there.
        let started = SystemTime::now();
        std::fs::create_dir(p.join("fresh")).unwrap();
        std::fs::write(p.join("fresh/b.txt"), "x\n").unwrap();
        std::fs::write(p.join("oldgi/.gitignore"), "y\n").unwrap();
        std::fs::write(p.join("dot/.ignore"), "y\n").unwrap();
        set_mtime(p, hour_ago);
        let fresh_ignore = ns(std::fs::metadata(p.join("dot/.ignore"))
            .unwrap()
            .modified()
            .unwrap());
        let mut listing: Vec<FileMeta> = ["old/a.txt", "fresh/b.txt", "oldgi/c.txt", "dot/d.txt"]
            .into_iter()
            .map(meta)
            .collect();
        listing.push(FileMeta {
            path: PathBuf::from("dot/.ignore"),
            mtime_ns: fresh_ignore,
            size: 2,
            hidden: false,
        });
        let src = |name: &str, mtime_ns: u128, size: u64| IgnoreSrc {
            path: PathBuf::from(name),
            mtime_ns,
            size,
        };
        let fingerprint = vec![
            src("/above/old", ns(hour_ago), 3),
            src("/above/fresh", ns(started), 3),
            src("/above/missing", 0, 0),
        ];
        let tree = build_from_listing(p, &listing, fingerprint, started);
        assert_eq!(
            tree.fingerprint,
            vec![
                src("/above/old", ns(hour_ago), 3),
                src("/above/fresh", 0, UNKNOWN_SIZE),
                src("/above/missing", 0, 0),
            ]
        );
        let root = &tree.root;
        assert_eq!(root.mtime_ns, 0, "the root's mtime is never recorded");
        assert_eq!((root.gitignore_mtime_ns, root.gitignore_size), (0, 0));
        let rels: Vec<&Path> = root.subdirs.iter().map(|n| n.rel.as_path()).collect();
        assert_eq!(rels, ["dot", "fresh", "old", "oldgi"].map(Path::new));
        let node = |name: &str| {
            root.subdirs
                .iter()
                .find(|n| n.rel == Path::new(name))
                .unwrap()
        };
        let old = node("old");
        assert_eq!(old.mtime_ns, ns(hour_ago));
        assert_eq!(
            (old.gitignore_mtime_ns, old.gitignore_size),
            (ns(hour_ago), 2)
        );
        let fresh = node("fresh");
        assert_eq!(fresh.mtime_ns, 0, "a directory changed during the sweep");
        assert_eq!(
            (fresh.gitignore_mtime_ns, fresh.gitignore_size),
            (0, UNKNOWN_SIZE)
        );
        assert!(fresh.has_ignore_subtree);
        let oldgi = node("oldgi");
        assert_eq!(oldgi.mtime_ns, ns(hour_ago));
        assert_eq!(
            (oldgi.gitignore_mtime_ns, oldgi.gitignore_size),
            (0, UNKNOWN_SIZE),
            "a .gitignore edited during the sweep"
        );
        let dot = node("dot");
        assert_eq!(dot.mtime_ns, ns(hour_ago));
        assert_eq!(dot.dot_ignores, vec![src(".ignore", 0, 2)]);
        // Nothing an observation can report matches an unknown record.
        assert!(!unchanged(&p.join("oldgi/.gitignore"), 0, UNKNOWN_SIZE));
        assert!(!unchanged(&p.join("dot/.ignore"), 0, 2));
    }

    /// A zero recorded mtime is no observation: even a file whose mtime
    /// really is the epoch does not count as unchanged against it.
    #[cfg(unix)]
    #[test]
    fn zero_recorded_mtime_never_counts_as_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(".gitignore");
        std::fs::write(&f, "x\n").unwrap();
        set_mtime(&f, std::time::UNIX_EPOCH);
        assert_eq!(
            std::fs::metadata(&f).unwrap().modified().unwrap(),
            std::time::UNIX_EPOCH
        );
        assert!(!unchanged(&f, 0, 2));
        let t = SystemTime::now() - Duration::from_secs(60);
        set_mtime(&f, t);
        assert!(unchanged(&f, ns(t), 2));
        assert!(!unchanged(&f, ns(t), 3));
    }

    #[test]
    fn save_load_roundtrip_and_fingerprint_check() {
        let dir = tempfile::tempdir().unwrap();
        let listing = vec![meta("a.txt")];
        let tree = build_from_listing(
            dir.path(),
            &listing,
            source_fingerprint(dir.path()),
            later(),
        );
        tree.save(dir.path()).unwrap();
        let back = DirTree::load(dir.path()).unwrap();
        assert_eq!(back, tree);
        assert!(back.fingerprint_matches(dir.path()));
    }

    #[test]
    fn schema_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut tree = build_from_listing(dir.path(), &[], vec![], later());
        tree.schema = 999;
        tree.save(dir.path()).unwrap();
        assert!(DirTree::load(dir.path()).is_none());
    }

    #[test]
    fn corrupt_cache_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_path(dir.path()), b"not a dirtree").unwrap();
        assert!(DirTree::load(dir.path()).is_none());
    }
}
