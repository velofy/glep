//! Directory-mtime pruning cache for file-set queries (`--files`).
//!
//! A content query needs every file's current (mtime, size) so the
//! freshness diff can reindex what changed — that costs a full bulk sweep
//! (~100ms on a kernel-scale tree) and is inherent. A *listing* query only
//! needs the SET of files to be fresh, and that set only changes via
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
//!      delete or rename inside L — including adding or removing a
//!      subdirectory, which would make L non-leaf — bumps L's mtime.
//!      In-place content edits never change the file set.
//!   2. L's own `.gitignore` (mtime, size) is re-statted and matches the
//!      cached values (only stat'ed when L recorded one): in-place
//!      .gitignore edits flip verdicts without touching any mtime.
//!   3. Every ancestor P on the path from the sweep root down was itself
//!      scanned live this run, and each one's own `.gitignore` (mtime,
//!      size) matched its cached node's recorded values
//!      (`chain_stable`): the cached listing was filtered by exactly
//!      those matchers.
//!   4. Ignore sources ABOVE the sweep root (global excludes,
//!      `.git/info/exclude`, ancestor `.gitignore` files) are captured as
//!      a (path, mtime, size) fingerprint at cache time and re-statted on
//!      every use; any difference discards the whole cache. (`.ignore` /
//!      `.rgignore` never coexist with a cache: their presence anywhere
//!      makes the bulk walker itself fall back to the portable walker.)
//!
//! The cache lives in `.glep/dirtree.bin`, is written atomically
//! (tmp + rename), and is independent of the manifest's trigram postings:
//! listing queries never touch, update, or write the manifest or
//! `postings.bin`. When the cache is missing, stale, or the pruned walk
//! finds too much churn (more than `RESCAN_BUDGET` live scans), listing
//! falls back to the ordinary full sweep and rebuilds the cache from it,
//! so a stale or even corrupt cache can only cost speed, never
//! correctness. `GLEP_NO_DIRCACHE=1` disables the whole mechanism.

use crate::walk::FileMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SCHEMA: u32 = 1;

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
    /// for the root node (nobody observes it).
    pub mtime_ns: u128,
    /// This directory's own `.gitignore` (mtime, size), or (0, 0) when it
    /// has none. Drives the `chain_stable` prune precondition.
    pub gitignore_mtime_ns: u128,
    pub gitignore_size: u64,
    /// Any `.gitignore` at or below this node (including itself).
    pub has_ignore_subtree: bool,
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

fn stat_src(p: PathBuf) -> IgnoreSrc {
    match std::fs::metadata(&p) {
        Ok(md) => {
            let mtime_ns = md
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            IgnoreSrc {
                path: p,
                mtime_ns,
                size: md.len(),
            }
        }
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
/// of the enclosing repo, and every ancestor `.gitignore` up to the git
/// root (bounded by the same cap the bulk walker uses). Mirrors
/// `walk_bulk::build_ancestor_stack`'s discovery so the two never
/// disagree about what "above the root" means.
pub fn source_fingerprint(root: &Path) -> Vec<IgnoreSrc> {
    let mut v = Vec::new();
    if let Some(p) = ignore::gitignore::gitconfig_excludes_path() {
        v.push(stat_src(p));
    }
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let git_root = crate::walk_bulk::find_git_root(&canon);
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
            if hit_git_root
                || (git_root.is_none() && depth >= crate::walk_bulk::ANCESTOR_GITIGNORE_CAP)
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
pub fn build_from_listing(
    root: &Path,
    listing: &[FileMeta],
    fingerprint: Vec<IgnoreSrc>,
) -> DirTree {
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
        dirs.get_mut(&parent).unwrap().0.files.push(meta.clone());
    }

    // stat each dir + probe .gitignore
    let mut mtimes: BTreeMap<PathBuf, (u128, u128, u64, bool)> = BTreeMap::new();
    for rel in dirs.keys() {
        let abs = root.join(rel);
        let md = std::fs::metadata(&abs).ok();
        let mtime_ns = md
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let gi = abs.join(".gitignore");
        let (gi_m, gi_s, gi_has) = match std::fs::metadata(&gi) {
            Ok(gmd) => {
                let gm = gmd
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                (gm, gmd.len(), true)
            }
            Err(_) => (0, 0, false),
        };
        mtimes.insert(rel.clone(), (mtime_ns, gi_m, gi_s, gi_has));
    }

    // Fold children into parents (BTreeMap iterates in sorted order;
    // deepest-first assembly via reverse iteration keeps it one pass).
    let keys: Vec<PathBuf> = dirs.keys().cloned().collect();
    for key in keys.iter().rev() {
        let (mtime_ns, gi_m, gi_s, gi_has) = mtimes[key];
        let has_ignore = gi_has
            || dirs[key]
                .1
                .iter()
                .any(|c| dirs.get(c).map(|n| n.0.has_ignore_subtree).unwrap_or(false));
        // detach this node's data
        let (mut node, children) = dirs.remove(key).unwrap();
        node.mtime_ns = mtime_ns;
        node.gitignore_mtime_ns = gi_m;
        node.gitignore_size = gi_s;
        node.has_ignore_subtree = has_ignore;
        node.subdirs = children
            .into_iter()
            .filter_map(|c| dirs.get(&c).map(|n| n.0.clone()))
            .collect();
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
        .unwrap_or(DirNode {
            rel: PathBuf::new(),
            mtime_ns: 0,
            gitignore_mtime_ns: 0,
            gitignore_size: 0,
            has_ignore_subtree: false,
            files: Vec::new(),
            subdirs: Vec::new(),
        });
    DirTree {
        schema: SCHEMA,
        fingerprint,
        root: root_node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tree = build_from_listing(dir.path(), &listing, vec![]);
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

    #[test]
    fn save_load_roundtrip_and_fingerprint_check() {
        let dir = tempfile::tempdir().unwrap();
        let listing = vec![meta("a.txt")];
        let tree = build_from_listing(dir.path(), &listing, source_fingerprint(dir.path()));
        tree.save(dir.path()).unwrap();
        let back = DirTree::load(dir.path()).unwrap();
        assert_eq!(back, tree);
        assert!(back.fingerprint_matches(dir.path()));
    }

    #[test]
    fn schema_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut tree = build_from_listing(dir.path(), &[], vec![]);
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
