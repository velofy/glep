use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use ignore::{ParallelVisitor, ParallelVisitorBuilder, WalkState};

#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    /// Relative to the sweep root.
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
}

/// Builds one `Collector` per worker thread, each with its own local buffer.
struct CollectorBuilder<'a> {
    root: &'a Path,
    global: &'a Mutex<Vec<FileMeta>>,
}

impl<'s> ParallelVisitorBuilder<'s> for CollectorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Collector {
            root: self.root,
            local: Vec::new(),
            global: self.global,
        })
    }
}

/// Per-thread visitor. Accumulates into `local` without locking, then
/// flushes into `global` exactly once when the thread's traversal ends.
struct Collector<'a> {
    root: &'a Path,
    local: Vec<FileMeta>,
    global: &'a Mutex<Vec<FileMeta>>,
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
                        self.local.push(FileMeta {
                            path: rel,
                            mtime_ns,
                            size: md.len(),
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

/// Parallel gitignore-aware sweep. Skips hidden entries (default), so
/// .git/ and .glep/ never appear. Returns files sorted by relative path.
///
/// On macOS this dispatches to `walk_bulk::sweep_bulk`, a getattrlistbulk
/// based fast path that collapses the per-file stat() storm into one
/// syscall per directory (see walk_bulk.rs for the design). Setting the
/// env var GLEP_NO_BULK_SWEEP forces this portable walker instead. Any
/// `Err` from sweep_bulk also falls back to this walker, with a warning on
/// stderr, so a bug in the macOS-only fast path can never surface as a
/// hard failure, only as a missed speedup for that one sweep.
pub fn sweep(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
    #[cfg(target_os = "macos")]
    {
        if std::env::var_os("GLEP_NO_BULK_SWEEP").is_none() {
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
    let walker = ignore::WalkBuilder::new(root).require_git(false).build_parallel();
    let mut builder = CollectorBuilder {
        root,
        global: &collected,
    };
    walker.visit(&mut builder);
    let mut v = collected.into_inner().unwrap();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(paths, vec!["b.txt", "src/a.rs"]);
        assert!(metas[0].size == 5);
        assert!(metas[0].mtime_ns > 0);
    }

    #[test]
    fn sweep_missing_root_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");
        assert!(sweep(&missing).is_err());
    }

    /// Differential parity between the macOS getattrlistbulk fast path
    /// (`walk_bulk::sweep_bulk`) and the portable walker
    /// (`sweep_walker`, this file's original implementation, kept as the
    /// fallback and correctness reference). Both must agree exactly on a
    /// fixture that exercises nested directories, a root-level .gitignore,
    /// a NESTED .gitignore that only applies to its own subtree, a hidden
    /// file, a hidden directory, and plain files.
    #[cfg(target_os = "macos")]
    mod bulk_parity {
        use super::*;

        fn build_fixture() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
            std::fs::create_dir_all(dir.path().join(".hidden_dir")).unwrap();

            std::fs::write(dir.path().join("root.txt"), "root file").unwrap();
            std::fs::write(dir.path().join("skip.log"), "gitignored at root").unwrap();
            std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
            std::fs::write(dir.path().join(".hidden_file"), "dotfile, always skipped").unwrap();
            std::fs::write(dir.path().join(".hidden_dir/inside.txt"), "never visited").unwrap();

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
            // applies everywhere, nested pattern applies only under sub/,
            // hidden file and hidden dir never appear).
            let expect: Vec<PathBuf> = [
                "root.txt",
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
            }
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
        /// excludes `vendored.txt` correctly via `.ignore`.
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
            assert_eq!(files, vec!["keep.txt".to_string()]);
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
        /// file the old bulk path used to wrongly drop.
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
                vec!["important.log".to_string(), "keep.txt".to_string()]
            );
        }
    }
}
