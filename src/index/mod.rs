pub mod manifest;
pub mod postings;

use crate::plan::Plan;
use crate::timing::Timings;
use crate::trigram;
use crate::walk::{self, FileMeta};
use fs2::FileExt;
use manifest::{
    Manifest, FLAG_DEAD, FLAG_HIDDEN, FLAG_NUL_LATE, FLAG_SKIP_BINARY, FLAG_SKIP_TOO_LARGE,
    FLAG_TRANSCODE,
};
use postings::Postings;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Index {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub manifest: Manifest,
    main: Option<Postings>,
    delta: Option<Postings>,
    pub read_only: bool,
    lock: Option<std::fs::File>,
    /// True once this process has made the manifest authoritative for the
    /// WHOLE tree: a fresh build, or a full (unscoped, not ttl-skipped)
    /// sweep whose results were written. Only then may derived state that
    /// certifies the full listing (the macOS dir-mtime listing cache) be
    /// rebuilt from the manifest.
    swept_full: bool,
    /// After a scoped sweep: each scope prefix and whether the sweep found
    /// any file under it (see `has_files_under`).
    scope_presence: Vec<(PathBuf, bool)>,
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stable-enough path hash for sharding the freshness diff. Only ever
/// used to pick a bucket; never persisted, so DefaultHasher's per-run
/// seed is fine.
fn hash_path(p: &Path) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish() as usize
}

fn new_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
}

/// grep-searcher's first buffer (`DEFAULT_BUFFER_CAPACITY`): rg checks it
/// for binary data before searching a single line, so a NUL in this
/// window means the file can never produce output under the default
/// quit detection.
pub const BINARY_SNIFF: usize = 64 * 1024;

/// Read content and record trigrams unless the file is skip-flagged.
/// Returns the flags to store for this entry.
fn index_file(
    root: &Path,
    meta: &FileMeta,
    id: u32,
    max_filesize: u64,
    map: &mut BTreeMap<u32, Vec<u32>>,
) -> u8 {
    if meta.size > max_filesize {
        return FLAG_SKIP_TOO_LARGE;
    }
    let content = match std::fs::read(root.join(&meta.path)) {
        Ok(c) => c,
        Err(_) => return FLAG_SKIP_TOO_LARGE, // unreadable: treat as live-scan-only
    };
    // A UTF-16 BOM precedes any NUL, so this check comes before the
    // binary sniff: raw UTF-16 bytes are NUL-heavy but rg transcodes the
    // file and searches it as text.
    if content.starts_with(&[0xFF, 0xFE]) || content.starts_with(&[0xFE, 0xFF]) {
        return FLAG_TRANSCODE;
    }
    let sniff = &content[..content.len().min(BINARY_SNIFF)];
    if memchr::memchr(0, sniff).is_some() {
        return FLAG_SKIP_BINARY;
    }
    let mut flags = 0;
    if content.len() > BINARY_SNIFF && memchr::memchr(0, &content[BINARY_SNIFF..]).is_some() {
        flags |= FLAG_NUL_LATE;
    }
    for tri in trigram::extract(&content) {
        map.entry(tri).or_default().push(id);
    }
    flags
}

impl Index {
    fn acquire_lock(dir: &Path) -> (Option<std::fs::File>, bool) {
        let lock_path = dir.join("lock");
        // Lock file: create-or-open, never truncated (contents unused;
        // the flock on the descriptor is the lock).
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => match f.try_lock_exclusive() {
                Ok(()) => (Some(f), false),
                Err(_) => (None, true),
            },
            Err(_) => (None, true),
        }
    }

    pub fn build(root: &Path, max_filesize: u64) -> anyhow::Result<Index> {
        let dir = root.join(".glep");
        std::fs::create_dir_all(&dir)?;
        // Self-ignoring directory: git never tracks the index, and we never
        // have to touch the user's .gitignore.
        let self_ignore = dir.join(".gitignore");
        if !self_ignore.exists() {
            std::fs::write(&self_ignore, "*\n")?;
        }
        let (lock, read_only) = Self::acquire_lock(&dir);
        anyhow::ensure!(!read_only, "another glep holds the index lock");
        Self::build_locked(root, max_filesize, lock)
    }

    /// The full build, under a lock this process already holds. Compaction
    /// rebuilds through here without ever letting go of its lock: dropping
    /// and re-taking it raced with any child process forked in between (the
    /// child shares the lock's open file until it execs), which then
    /// failed the rebuild with "another glep holds the index lock".
    fn build_locked(
        root: &Path,
        max_filesize: u64,
        lock: Option<std::fs::File>,
    ) -> anyhow::Result<Index> {
        let dir = root.join(".glep");
        let generation = new_generation();
        let metas = walk::sweep(root)?;
        let mut man = Manifest::default();
        let mut map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (n, meta) in metas.iter().enumerate() {
            if n > 0 && n % 5000 == 0 {
                eprintln!("glep: indexed {n} files...");
            }
            let id = man.add(meta);
            let mut flags = index_file(root, meta, id, max_filesize, &mut map);
            if meta.hidden {
                flags |= FLAG_HIDDEN;
            }
            man.entries[id as usize].flags = flags;
        }
        man.last_sweep_epoch = now_epoch();
        man.generation = generation;
        postings::write(&dir.join("postings.bin"), &map, generation)?;
        let _ = std::fs::remove_file(dir.join("delta.bin"));
        man.save(&dir.join("manifest.bin"))?;
        let main = Postings::open(&dir.join("postings.bin"))?;
        Ok(Index {
            root: root.to_path_buf(),
            dir,
            manifest: man,
            main: Some(main),
            delta: None,
            read_only: false,
            lock,
            swept_full: true,
            scope_presence: Vec::new(),
        })
    }

    pub fn open_or_build(root: &Path, max_filesize: u64) -> anyhow::Result<Index> {
        let dir = root.join(".glep");
        let try_open = || -> anyhow::Result<(Manifest, Postings, Option<Postings>)> {
            let man = Manifest::load(&dir.join("manifest.bin"))?;
            let main = Postings::open(&dir.join("postings.bin"))?;
            anyhow::ensure!(
                main.generation() == man.generation,
                "index generation mismatch (torn write)"
            );
            let delta = match Postings::open(&dir.join("delta.bin")) {
                // A delta from another generation is a leftover; drop it.
                Ok(d) if d.generation() == man.generation => Some(d),
                _ => None,
            };
            Ok((man, main, delta))
        };
        if dir.join("manifest.bin").exists() {
            let opened = try_open().or_else(|_| try_open());
            match opened {
                Ok((man, main, delta)) => {
                    let (lock, read_only) = Self::acquire_lock(&dir);
                    return Ok(Index {
                        root: root.to_path_buf(),
                        dir,
                        manifest: man,
                        main: Some(main),
                        delta,
                        read_only,
                        lock,
                        swept_full: false,
                        scope_presence: Vec::new(),
                    });
                }
                Err(e) => {
                    eprintln!("glep: index unreadable ({e}); rebuilding");
                }
            }
        }
        Self::build(root, max_filesize)
    }

    /// `include_hidden = false` (the default, matching rg) drops entries
    /// flagged `FLAG_HIDDEN`: files/dirs with a dot-prefixed path
    /// component. `.git`/`.glep` never reach the manifest at all (hard
    /// sweep-time exclusion, see walk.rs), so there is no flag for those to
    /// check here.
    /// Full FileMeta view of live entries (mtimes/sizes as of the last
    /// sweep), for cache building. `hidden` is reconstructed from
    /// FLAG_HIDDEN.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn live_file_metas(&self, include_hidden: bool) -> Vec<crate::walk::FileMeta> {
        self.manifest
            .live_entries()
            .filter(|e| include_hidden || e.flags & FLAG_HIDDEN == 0)
            .map(|e| crate::walk::FileMeta {
                path: e.path.clone(),
                mtime_ns: e.mtime_ns,
                size: e.size,
                hidden: e.flags & FLAG_HIDDEN != 0,
            })
            .collect()
    }

    #[cfg(test)]
    pub fn live_files(&self, include_hidden: bool) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self
            .manifest
            .live_entries()
            .filter(|e| include_hidden || e.flags & FLAG_HIDDEN == 0)
            .map(|e| e.path.clone())
            .collect();
        v.sort();
        v
    }

    /// Whether the tree holds any indexable file under `prefix` (hidden
    /// files included). A directory operand without one is something the
    /// index cannot serve: an ignored or symlinked directory, or a
    /// spelling that differs from the one on disk. Answered from the last
    /// scoped sweep when it covered `prefix`, else from the manifest.
    pub fn has_files_under(&self, prefix: &Path) -> bool {
        if let Some((_, has)) = self.scope_presence.iter().find(|(p, _)| p == prefix) {
            return *has;
        }
        self.manifest
            .live_entries()
            .any(|e| e.path.starts_with(prefix))
    }

    /// Every live file, split by `FLAG_HIDDEN` into (plain, hidden), both
    /// path-sorted: rg may still show a hidden-flagged file (below a
    /// hidden operand, or whitelisted by -g/-t), so the query layer
    /// decides per file. With `include_hidden` every file is plain.
    /// `under` keeps only the files below one of those directories (None
    /// keeps every file), so a scoped listing never clones and sorts the
    /// whole tree's paths.
    pub fn live_files_split(
        &self,
        include_hidden: bool,
        under: Option<&[PathBuf]>,
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let ids: Vec<u32> = self
            .manifest
            .live_entries()
            .filter(|e| under.is_none_or(|dirs| dirs.iter().any(|d| e.path.starts_with(d))))
            .map(|e| e.id)
            .collect();
        self.split_hidden(ids, include_hidden)
    }

    /// Sorted, deduplicated paths for `ids`, split into (plain, hidden)
    /// unless `include_hidden`.
    fn split_hidden(
        &self,
        mut ids: Vec<u32>,
        include_hidden: bool,
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        ids.retain(|&id| self.is_live(id));
        let mut plain = Vec::with_capacity(ids.len());
        let mut hidden = Vec::new();
        for id in ids {
            let e = &self.manifest.entries[id as usize];
            if !include_hidden && e.flags & FLAG_HIDDEN != 0 {
                hidden.push(e.path.clone());
            } else {
                plain.push(e.path.clone());
            }
        }
        for v in [&mut plain, &mut hidden] {
            v.sort();
            v.dedup();
        }
        (plain, hidden)
    }

    fn postings_for(&self, tri: u32, ci: bool) -> Vec<u32> {
        let tris = if ci {
            trigram::case_variants(tri)
        } else {
            vec![tri]
        };
        let mut ids = Vec::new();
        for t in tris {
            for seg in [self.main.as_ref(), self.delta.as_ref()]
                .into_iter()
                .flatten()
            {
                if let Some(v) = seg.lookup(t) {
                    ids.extend(v);
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn is_live(&self, id: u32) -> bool {
        self.manifest
            .entries
            .get(id as usize)
            .is_some_and(|e| e.flags & FLAG_DEAD == 0)
    }

    /// `include_hidden = false` (the default, matching rg) drops candidates
    /// flagged `FLAG_HIDDEN` after narrowing by plan, so both the `All` and
    /// `Groups` arms (trigram postings carry no hidden bit of their own)
    /// are covered by one filter rather than two.
    ///
    /// Oversized files carry no trigram postings, so they can never be
    /// narrowed by a plan; they are unconditional live-scan candidates in
    /// both arms. `FLAG_SKIP_BINARY` files join them only when
    /// `search_binary` is set (-a/--binary, --null-data, a non-UTF-8
    /// -E): under the default quit detection a binary file can never emit
    /// output, so excluding it is a pure win (rg walks the file and quits
    /// at the first NUL instead; observably identical).
    #[cfg(test)]
    pub fn candidates(
        &self,
        plan: &Plan,
        case_insensitive: bool,
        include_hidden: bool,
        search_binary: bool,
    ) -> Vec<PathBuf> {
        let mut ids = self.candidate_ids(plan, case_insensitive, search_binary);
        ids.retain(|&id| self.is_live(id));
        if !include_hidden {
            ids.retain(|&id| self.manifest.entries[id as usize].flags & FLAG_HIDDEN == 0);
        }
        let mut paths: Vec<PathBuf> = ids
            .iter()
            .map(|&id| self.manifest.entries[id as usize].path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// `candidates`, split by `FLAG_HIDDEN` like `live_files_split`.
    pub fn candidates_split(
        &self,
        plan: &Plan,
        case_insensitive: bool,
        include_hidden: bool,
        search_binary: bool,
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let ids = self.candidate_ids(plan, case_insensitive, search_binary);
        self.split_hidden(ids, include_hidden)
    }

    fn candidate_ids(&self, plan: &Plan, case_insensitive: bool, search_binary: bool) -> Vec<u32> {
        match plan {
            Plan::All => self
                .manifest
                .live_entries()
                .filter(|e| search_binary || e.flags & FLAG_SKIP_BINARY == 0)
                .map(|e| e.id)
                .collect(),
            Plan::Groups(groups) => {
                let mut union: Vec<u32> = Vec::new();
                for group in groups {
                    let mut iter = group.iter();
                    let mut acc = match iter.next() {
                        Some(&t) => self.postings_for(t, case_insensitive),
                        None => continue,
                    };
                    for &t in iter {
                        if acc.is_empty() {
                            break;
                        }
                        let next = self.postings_for(t, case_insensitive);
                        acc.retain(|id| next.binary_search(id).is_ok());
                    }
                    union.extend(acc);
                }
                // Skip-flagged text files (oversized, UTF-16 BOM) were
                // never indexed; always scan them. Binary files too, when
                // the caller can surface them.
                union.extend(
                    self.manifest
                        .live_entries()
                        .filter(|e| {
                            e.flags & (FLAG_SKIP_TOO_LARGE | FLAG_TRANSCODE) != 0
                                || (search_binary && e.flags & FLAG_SKIP_BINARY != 0)
                        })
                        .map(|e| e.id),
                );
                union.sort_unstable();
                union.dedup();
                union
            }
        }
    }

    /// Live files rg would detect as binary under quit detection without
    /// finding a match first when it reads them whole: a NUL in the first
    /// buffer (`FLAG_SKIP_BINARY`) or later (`FLAG_NUL_LATE`). Hidden
    /// files are included; callers intersect with their own file sets.
    pub fn binary_files(&self) -> std::collections::HashSet<PathBuf> {
        self.manifest
            .live_entries()
            .filter(|e| e.flags & (FLAG_SKIP_BINARY | FLAG_NUL_LATE) != 0)
            .map(|e| e.path.clone())
            .collect()
    }

    /// Live entries whose names carry a compressed extension (`-z`).
    /// Compressed files are always candidate-side (their indexed
    /// trigrams are compressed bytes, useless for narrowing decoded
    /// content), so all of them join the scan set when -z is active.
    /// Currently only gzip is supported; other extensions get scanned
    /// raw (binary-quit finds nothing, same as -z off).
    /// Split by `FLAG_HIDDEN` like `live_files_split`.
    pub fn zip_candidates_split(&self, include_hidden: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let ids: Vec<u32> = self
            .manifest
            .live_entries()
            .filter(|e| e.path.extension().is_some_and(|x| x == "gz"))
            .map(|e| e.id)
            .collect();
        self.split_hidden(ids, include_hidden)
    }

    /// See the `swept_full` field.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn swept_full(&self) -> bool {
        self.swept_full
    }

    pub fn update(&mut self, max_filesize: u64, ttl_secs: u64) -> anyhow::Result<Vec<PathBuf>> {
        self.update_impl(max_filesize, ttl_secs, None, None)
    }

    /// Same as `update`, but records sweep_walk/sweep_diff/index_write
    /// stages on the given Timings. Kept as a sibling method (rather than
    /// growing `update`'s signature) so existing callers and tests are
    /// untouched.
    pub fn update_timed(
        &mut self,
        max_filesize: u64,
        ttl_secs: u64,
        timings: &mut Timings,
    ) -> anyhow::Result<Vec<PathBuf>> {
        self.update_impl(max_filesize, ttl_secs, None, Some(timings))
    }

    /// Scoped update: like `update_timed` but sweeps only `scope`
    /// subtrees (index-relative path filters). Manifest entries outside
    /// the scope keep their existing data untouched: they can be stale,
    /// but they are also never consulted by the scoped query. The global
    /// sweep epoch is only bumped for unscoped (full) sweeps: a scoped
    /// sweep must not suppress the freshness sweep a later unscoped
    /// query needs (ttl correctness).
    pub fn update_scoped(
        &mut self,
        max_filesize: u64,
        ttl_secs: u64,
        scope: &[PathBuf],
        timings: &mut Timings,
    ) -> anyhow::Result<Vec<PathBuf>> {
        self.update_impl(max_filesize, ttl_secs, Some(scope), Some(timings))
    }

    fn update_impl(
        &mut self,
        max_filesize: u64,
        ttl_secs: u64,
        scope: Option<&[PathBuf]>,
        mut timings: Option<&mut Timings>,
    ) -> anyhow::Result<Vec<PathBuf>> {
        // The ttl only gates FULL sweeps: it marks "the whole tree was
        // verified as of epoch". A scoped sweep leaves other subtrees
        // unverified, so it neither honors nor sets the epoch.
        let scope: Option<&[PathBuf]> =
            scope.filter(|s| !s.is_empty() && !s.iter().any(|p| p.as_os_str().is_empty()));
        let scoped = scope.is_some();
        if !scoped
            && ttl_secs > 0
            && now_epoch().saturating_sub(self.manifest.last_sweep_epoch) <= ttl_secs
        {
            return Ok(Vec::new());
        }
        let swept = match scope {
            Some(s) => walk::sweep_scoped_unsorted(&self.root, s)?,
            None => walk::sweep_unsorted(&self.root)?,
        };
        if let Some(s) = scope {
            self.scope_presence = s
                .iter()
                .map(|p| (p.clone(), swept.iter().any(|m| m.path.starts_with(p))))
                .collect();
        }
        if let Some(t) = &mut timings {
            t.stage("sweep_walk");
        }
        // For a scoped sweep only manifest entries inside the scope are
        // candidates for tombstoning or refresh; everything outside is
        // untouched (it simply wasn't looked at).
        let in_scope =
            |p: &Path| -> bool { scope.is_none_or(|s| s.iter().any(|pre| p.starts_with(pre))) };
        // Parallel sharded diff (replaces the old two-HashMap serial pass,
        // ~28ms on a kernel-scale tree). Three phases:
        //   0. build live-id maps sharded by path hash, in parallel;
        //   1. classify every swept file against its shard, in parallel,
        //      marking seen manifest entries atomically;
        //   2. dead = live entries never marked seen.
        // Unchanged files touch nothing but an atomic store; fresh and
        // changed ids come out of phase 1 per shard.
        let n_shards = rayon::current_num_threads().max(1).next_power_of_two();
        let shard_mask = n_shards - 1;
        // Phase 0a: bucket live entries by path hash (serial, cheap push
        // per entry) so phase 0b can give each shard to exactly one
        // thread, no concurrent HashMap mutation anywhere.
        let mut buckets: Vec<Vec<&manifest::FileEntry>> =
            (0..n_shards).map(|_| Vec::new()).collect();
        for e in self.manifest.live_entries().filter(|e| in_scope(&e.path)) {
            buckets[hash_path(&e.path) & shard_mask].push(e);
        }
        // Phase 0b: build each shard's lookup map in parallel.
        let shard_maps: Vec<std::collections::HashMap<&Path, &manifest::FileEntry>> = buckets
            .into_par_iter()
            .map(|bucket| bucket.into_iter().map(|e| (e.path.as_path(), e)).collect())
            .collect();
        // seen flags for dead detection: 1 = present on disk unchanged or
        // changed-in-place (either way, not deleted)
        let seen: Vec<std::sync::atomic::AtomicU8> = self
            .manifest
            .entries
            .iter()
            .map(|_| std::sync::atomic::AtomicU8::new(0))
            .collect();
        use rayon::prelude::*;
        let mut fresh: Vec<FileMeta> = Vec::new(); // new or changed (order irrelevant)
        let mut changed_old_ids: Vec<u32> = Vec::new();
        {
            let chunk = (swept.len() / n_shards / 2).max(1024);
            let results: Vec<(Vec<FileMeta>, Vec<u32>)> = swept
                .par_chunks(chunk)
                .map(|metas| {
                    let mut my_fresh = Vec::new();
                    let mut my_changed = Vec::new();
                    for meta in metas {
                        let h = hash_path(&meta.path) & shard_mask;
                        match shard_maps[h].get(meta.path.as_path()) {
                            Some(e) => {
                                seen[e.id as usize].store(1, std::sync::atomic::Ordering::Relaxed);
                                if e.mtime_ns != meta.mtime_ns || e.size != meta.size {
                                    my_fresh.push(meta.clone());
                                    my_changed.push(e.id);
                                }
                            }
                            None => my_fresh.push(meta.clone()), // brand new
                        }
                    }
                    (my_fresh, my_changed)
                })
                .collect();
            for (f, c) in results {
                fresh.extend(f);
                changed_old_ids.extend(c);
            }
        }
        let dead_ids: Vec<u32> = self
            .manifest
            .entries
            .iter()
            .filter(|e| {
                e.flags & manifest::FLAG_DEAD == 0
                    && seen[e.id as usize].load(std::sync::atomic::Ordering::Relaxed) == 0
                    && in_scope(&e.path)
            })
            .map(|e| e.id)
            .collect();
        if let Some(t) = &mut timings {
            t.stage("sweep_diff");
        }

        if self.read_only {
            if let Some(t) = &mut timings {
                t.stage("index_write");
            }
            return Ok(fresh.into_iter().map(|m| m.path).collect());
        }
        if fresh.is_empty() && dead_ids.is_empty() {
            // Only a full sweep may advance the freshness epoch.
            if !scoped {
                self.manifest.last_sweep_epoch = now_epoch();
                self.swept_full = true;
                // The persisted epoch is consumed solely by ttl checks; skip
                // the write when ttl is unused so a no-op query stays
                // write-free. A later process may then see a slightly stale
                // epoch, which can only cause an extra sweep, never a
                // missed one.
                if ttl_secs > 0 {
                    self.manifest.save(&self.dir.join("manifest.bin"))?;
                }
            }
            if let Some(t) = &mut timings {
                t.stage("index_write");
            }
            return Ok(Vec::new());
        }

        for id in dead_ids.into_iter().chain(changed_old_ids) {
            self.manifest.entries[id as usize].flags |= manifest::FLAG_DEAD;
        }
        let mut map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        if let Some(delta) = &self.delta {
            for (tri, ids) in delta.iter_all() {
                map.insert(tri, ids);
            }
        }
        for meta in &fresh {
            let id = self.manifest.add(meta);
            let mut flags = index_file(&self.root, meta, id, max_filesize, &mut map);
            if meta.hidden {
                flags |= FLAG_HIDDEN;
            }
            self.manifest.entries[id as usize].flags = flags;
        }
        for ids in map.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        postings::write(&self.dir.join("delta.bin"), &map, self.manifest.generation)?;
        if !scoped {
            self.manifest.last_sweep_epoch = now_epoch();
            self.swept_full = true;
        }
        self.manifest.save(&self.dir.join("manifest.bin"))?;
        self.delta = Some(Postings::open(&self.dir.join("delta.bin"))?);

        // Compaction: delta grew past a tenth of main. Full rebuild is the
        // simple, correct v1 compaction strategy. Scoped updates skip the
        // trigger: their delta contribution is bounded by the scope, and a
        // rebuild would sweep the whole tree, defeating the point. The
        // next full sweep still compacts.
        let main_size = std::fs::metadata(self.dir.join("postings.bin"))
            .map(|m| m.len())
            .unwrap_or(0);
        let delta_size = std::fs::metadata(self.dir.join("delta.bin"))
            .map(|m| m.len())
            .unwrap_or(0);
        if !scoped && main_size > 0 && delta_size > main_size / 10 {
            // Rebuild under the lock this update already holds.
            let lock = self.lock.take();
            *self = Index::build_locked(&self.root.clone(), max_filesize, lock)?;
        }
        if let Some(t) = &mut timings {
            t.stage("index_write");
        }
        Ok(Vec::new())
    }

    #[cfg(test)]
    fn has_delta(&self) -> bool {
        self.delta.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();
        std::fs::write(dir.path().join("b.txt"), "goodbye world").unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"\x00\x01binary").unwrap();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(100)).unwrap();
        dir
    }

    #[test]
    fn build_indexes_text_flags_binary_and_oversized() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 50).unwrap(); // 50-byte cap: big.txt skipped
        assert!(dir.path().join(".glep/manifest.bin").exists());
        assert!(dir.path().join(".glep/postings.bin").exists());
        // index dir self-ignores so git never tracks it
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".glep/.gitignore")).unwrap(),
            "*\n"
        );
        let by_path = |p: &str| {
            idx.manifest
                .entries
                .iter()
                .find(|e| e.path.to_string_lossy() == p)
                .unwrap()
                .clone()
        };
        assert_eq!(by_path("a.txt").flags, 0);
        assert_eq!(by_path("bin.dat").flags, manifest::FLAG_SKIP_BINARY);
        assert_eq!(by_path("big.txt").flags, manifest::FLAG_SKIP_TOO_LARGE);
        let files = idx.live_files(false);
        assert_eq!(files.len(), 4);
    }

    /// The binary sniff covers the searcher's whole first buffer: a NUL
    /// anywhere in the first 64 KB marks the file binary, a later NUL
    /// only sets FLAG_NUL_LATE (the file stays indexed: rg prints its
    /// matches before the NUL). UTF-16 BOM files are never indexed and
    /// always scanned (FLAG_TRANSCODE): rg transcodes them as text.
    #[test]
    fn binary_flags_follow_the_searchers_first_buffer() {
        let dir = corpus();
        let mut u16le = vec![0xFF, 0xFE];
        for b in "needle utf16\n".bytes() {
            u16le.extend_from_slice(&[b, 0]);
        }
        std::fs::write(dir.path().join("u16.txt"), &u16le).unwrap();
        let mut u16be = vec![0xFE, 0xFF];
        for b in "needle utf16\n".bytes() {
            u16be.extend_from_slice(&[0, b]);
        }
        std::fs::write(dir.path().join("u16be.txt"), &u16be).unwrap();
        let mut mid = b"needle mid\n".to_vec();
        mid.extend(std::iter::repeat_n(b'x', 20_000));
        mid.extend_from_slice(b"\n\x00\n");
        std::fs::write(dir.path().join("mid.txt"), &mid).unwrap();
        let mut late = b"needle late\n".to_vec();
        late.extend(std::iter::repeat_n(b'y', 70_000));
        late.extend_from_slice(b"\n\x00\n");
        std::fs::write(dir.path().join("late.txt"), &late).unwrap();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let flags = |p: &str| {
            idx.manifest
                .entries
                .iter()
                .find(|e| e.path.to_string_lossy() == p)
                .unwrap()
                .flags
        };
        assert_eq!(flags("u16.txt"), manifest::FLAG_TRANSCODE);
        assert_eq!(flags("u16be.txt"), manifest::FLAG_TRANSCODE);
        assert_eq!(flags("mid.txt"), manifest::FLAG_SKIP_BINARY);
        assert_eq!(flags("late.txt"), manifest::FLAG_NUL_LATE);
        let plan = crate::plan::build("needle", true, crate::plan::Case::Sensitive);
        let c = idx.candidates(&plan, false, false, false);
        for p in ["u16.txt", "u16be.txt", "late.txt"] {
            assert!(c.contains(&PathBuf::from(p)), "{p} must be a candidate");
        }
        assert!(!c.contains(&PathBuf::from("mid.txt")));
        let bin = idx.binary_files();
        for p in ["bin.dat", "mid.txt", "late.txt"] {
            assert!(bin.contains(Path::new(p)), "{p} is binary to rg");
        }
        assert!(!bin.contains(Path::new("u16.txt")));
        assert!(!bin.contains(Path::new("a.txt")));
    }

    #[test]
    fn open_or_build_reopens_existing() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(!idx.read_only);
        assert_eq!(idx.live_files(false).len(), 4);
    }

    #[test]
    fn corrupt_index_rebuilds() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join(".glep/postings.bin"), b"garbage").unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert_eq!(idx.live_files(false).len(), 4);
    }

    #[test]
    fn generation_mismatch_triggers_rebuild() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        // Tear the pair: stamp the manifest with a different generation.
        let mpath = dir.path().join(".glep/manifest.bin");
        let mut man = manifest::Manifest::load(&mpath).unwrap();
        man.generation ^= 0xdead_beef;
        man.save(&mpath).unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert_eq!(idx.live_files(false).len(), 4);
        let man2 = manifest::Manifest::load(&mpath).unwrap();
        let post = postings::Postings::open(&dir.path().join(".glep/postings.bin")).unwrap();
        assert_eq!(man2.generation, post.generation());
    }

    #[test]
    fn stale_delta_is_ignored_matching_delta_attaches() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        let mut map = std::collections::BTreeMap::new();
        map.insert(0x0061_6263u32, vec![0u32]);

        // Stale generation: delta must be dropped.
        postings::write(&dir.path().join(".glep/delta.bin"), &map, 12345).unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(!idx.has_delta());
        assert_eq!(idx.live_files(false).len(), 4);
        drop(idx);

        // Matching generation: delta must attach.
        let man = manifest::Manifest::load(&dir.path().join(".glep/manifest.bin")).unwrap();
        postings::write(&dir.path().join(".glep/delta.bin"), &map, man.generation).unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(idx.has_delta());
    }

    #[test]
    fn update_scoped_only_sweeps_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let other_dir = dir.path().join("other");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), "scopedneedle a").unwrap();
        std::fs::write(other_dir.join("b.txt"), "scopedneedle b").unwrap();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();

        // Change a file inside scope and outside scope.
        std::fs::write(src_dir.join("a.txt"), "scopedneedle v2x a").unwrap();
        std::fs::write(other_dir.join("b.txt"), "scopedneedle b v2x").unwrap();
        std::fs::write(other_dir.join("c.txt"), "scopedneedle c new").unwrap();

        let mut timings = Timings::new();
        let scope = vec![std::path::PathBuf::from("src")];
        idx.update_scoped(1_048_576, 0, &scope, &mut timings)
            .unwrap();
        // The in-scope change is now indexed (fresh files join the delta).
        let plan = crate::plan::build("v2x a", true, crate::plan::Case::Sensitive);
        let c = idx.candidates(&plan, false, false, false);
        assert_eq!(c, vec![std::path::PathBuf::from("src/a.txt")]);
        // Out-of-scope changes were never looked at: b.txt's new
        // contents are NOT a candidate (still stale, which is sound,
        // since a scoped query never returns it anyway).
        let plan2 = crate::plan::build("b v2x", true, crate::plan::Case::Sensitive);
        assert!(idx.candidates(&plan2, false, false, false).is_empty());
        // A scoped sweep never certifies the whole tree.
        drop(idx);
        let mut reopened = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        reopened
            .update_scoped(1_048_576, 0, &scope, &mut timings)
            .unwrap();
        assert!(!reopened.swept_full());
    }

    #[test]
    fn update_scoped_tombstones_only_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("src/gone.txt"), "tombneedle").unwrap();
        std::fs::write(dir.path().join("other/gone.txt"), "tombneedle").unwrap();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::remove_file(dir.path().join("src/gone.txt")).unwrap();
        std::fs::remove_file(dir.path().join("other/gone.txt")).unwrap();
        let mut timings = Timings::new();
        idx.update_scoped(
            1_048_576,
            0,
            &[std::path::PathBuf::from("src")],
            &mut timings,
        )
        .unwrap();
        let live = idx.live_files(false);
        assert!(!live.contains(&std::path::PathBuf::from("src/gone.txt")));
        // outside the scope: untouched (stale until the next full sweep)
        assert!(live.contains(&std::path::PathBuf::from("other/gone.txt")));
        idx.update(1_048_576, 0).unwrap();
        assert!(!idx
            .live_files(false)
            .contains(&std::path::PathBuf::from("other/gone.txt")));
    }

    #[test]
    fn update_scoped_respects_parent_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.ign\nignored_dir/\n").unwrap();
        std::fs::write(src_dir.join("x.ign"), "no").unwrap();
        std::fs::write(src_dir.join("x.txt"), "yes").unwrap();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        let mut timings = Timings::new();
        // A new ignored file inside scope must NOT appear (parent
        // .gitignore still applies to the scoped subtree).
        std::fs::write(src_dir.join("y.ign"), "new ignored").unwrap();
        std::fs::write(src_dir.join("y.txt"), "new visible").unwrap();
        idx.update_scoped(
            1_048_576,
            0,
            &[std::path::PathBuf::from("src")],
            &mut timings,
        )
        .unwrap();
        // y.txt is indexed; the ignored y.ign was never swept.
        let plan = crate::plan::build("new visible", true, crate::plan::Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, false, false, false),
            vec![std::path::PathBuf::from("src/y.txt")]
        );
        let plan2 = crate::plan::build("new ignored", true, crate::plan::Case::Sensitive);
        assert!(idx.candidates(&plan2, false, false, false).is_empty());
        // An ignored directory named as the scope stays out of the index
        // entirely: the scoped sweep is the full sweep restricted to the
        // scope, never a walk rooted inside an ignored tree.
        std::fs::create_dir_all(dir.path().join("ignored_dir")).unwrap();
        std::fs::write(dir.path().join("ignored_dir/z.txt"), "inside ignored").unwrap();
        idx.update_scoped(
            1_048_576,
            0,
            &[std::path::PathBuf::from("ignored_dir")],
            &mut timings,
        )
        .unwrap();
        assert!(!idx
            .live_files(true)
            .iter()
            .any(|p| p.starts_with("ignored_dir")));
    }

    #[test]
    fn swept_full_tracks_authoritative_sweeps() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        let mut idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(!idx.swept_full(), "an opened index is not yet verified");
        idx.update(1_048_576, 3600).unwrap(); // within ttl: skipped
        assert!(!idx.swept_full(), "a ttl-skipped sweep certifies nothing");
        idx.update(1_048_576, 0).unwrap();
        assert!(idx.swept_full());
    }

    #[test]
    fn candidates_narrow_by_trigram() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let plan = crate::plan::build("hello", true, crate::plan::Case::Sensitive);
        let c = idx.candidates(&plan, false, false, false);
        assert_eq!(c, vec![std::path::PathBuf::from("a.txt")]);
    }

    #[test]
    fn candidates_case_insensitive_uses_variants() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let plan = crate::plan::build("HELLO", true, crate::plan::Case::Sensitive);
        assert!(idx.candidates(&plan, false, false, false).is_empty());
        let c = idx.candidates(&plan, true, false, false);
        assert_eq!(c, vec![std::path::PathBuf::from("a.txt")]);
    }

    #[test]
    fn candidates_include_oversized_files_always() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 50).unwrap(); // big.txt skip-flagged
        let plan = crate::plan::build("hello", true, crate::plan::Case::Sensitive);
        let c = idx.candidates(&plan, false, false, false);
        assert!(c.contains(&std::path::PathBuf::from("a.txt")));
        assert!(c.contains(&std::path::PathBuf::from("big.txt")));
        // Binary files join only when the caller can surface them
        // (-a/--binary); under quit detection they can never emit output.
        assert!(!c.contains(&std::path::PathBuf::from("bin.dat")));
        let c = idx.candidates(&plan, false, false, true);
        assert!(c.contains(&std::path::PathBuf::from("bin.dat")));
    }

    #[test]
    fn candidates_all_returns_live_non_binary() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let c = idx.candidates(&crate::plan::Plan::All, false, false, false);
        assert_eq!(c.len(), 3); // a.txt, b.txt, big.txt; bin.dat excluded
        let c = idx.candidates(&crate::plan::Plan::All, false, false, true);
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn update_sees_new_and_changed_files() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join("new.txt"), "freshneedle here").unwrap();
        // ensure mtime moves even on coarse filesystems
        std::fs::write(dir.path().join("a.txt"), "hello changedneedle").unwrap();
        idx.update(1_048_576, 0).unwrap();
        let plan = crate::plan::build("freshneedle", true, crate::plan::Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, false, false, false),
            vec![std::path::PathBuf::from("new.txt")]
        );
        let plan2 = crate::plan::build("changedneedle", true, crate::plan::Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan2, false, false, false),
            vec![std::path::PathBuf::from("a.txt")]
        );
    }

    #[test]
    fn update_tombstones_deleted_files() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        idx.update(1_048_576, 0).unwrap();
        let plan = crate::plan::build("hello", true, crate::plan::Case::Sensitive);
        assert!(idx.candidates(&plan, false, false, false).is_empty());
    }

    #[test]
    fn update_respects_ttl() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join("late.txt"), "ttlneedle").unwrap();
        idx.update(1_048_576, 3600).unwrap(); // within ttl: sweep skipped
        let plan = crate::plan::build("ttlneedle", true, crate::plan::Case::Sensitive);
        assert!(idx.candidates(&plan, false, false, false).is_empty());
        idx.update(1_048_576, 0).unwrap(); // ttl 0: always sweeps
        assert_eq!(idx.candidates(&plan, false, false, false).len(), 1);
    }

    #[test]
    fn update_persists_across_reopen() {
        let dir = corpus();
        {
            let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
            std::fs::write(dir.path().join("persist.txt"), "persistneedle").unwrap();
            idx.update(1_048_576, 0).unwrap();
        }
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        let plan = crate::plan::build("persistneedle", true, crate::plan::Case::Sensitive);
        assert_eq!(idx.candidates(&plan, false, false, false).len(), 1);
    }

    #[test]
    fn compaction_folds_delta_into_main() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        let gen_before = idx.manifest.generation;
        // A change set with far more trigrams than the tiny main index
        // pushes delta.bin past main/10 and must trigger compaction.
        let big: String = (0..2000).map(|i| format!("uniqtoken{i} ")).collect();
        std::fs::write(dir.path().join("bulk.txt"), &big).unwrap();
        idx.update(1_048_576, 0).unwrap();
        assert!(!idx.has_delta(), "delta should be folded into main");
        assert!(!dir.path().join(".glep/delta.bin").exists());
        assert_ne!(
            idx.manifest.generation, gen_before,
            "compaction rebuilds with a fresh generation"
        );
        let plan = crate::plan::build("uniqtoken1999", true, crate::plan::Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, false, false, false),
            vec![std::path::PathBuf::from("bulk.txt")]
        );
    }

    #[test]
    fn noop_update_without_ttl_writes_nothing() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        let mpath = dir.path().join(".glep/manifest.bin");
        let before = std::fs::metadata(&mpath).unwrap().modified().unwrap();
        idx.update(1_048_576, 0).unwrap();
        let after = std::fs::metadata(&mpath).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "no-op update with ttl 0 must not rewrite the manifest"
        );
    }

    #[test]
    fn read_only_update_writes_nothing_and_returns_fresh_paths() {
        let dir = corpus();
        let _writer = Index::build(dir.path(), 1_048_576).unwrap(); // holds the lock
        let mut reader = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(reader.read_only);
        std::fs::write(dir.path().join("hot.txt"), "hotneedle").unwrap();
        let manifest_before = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
        let extra = reader.update(1_048_576, 0).unwrap();
        assert_eq!(extra, vec![std::path::PathBuf::from("hot.txt")]);
        let manifest_after = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
        assert_eq!(
            manifest_before, manifest_after,
            "read-only update must not write"
        );
        assert!(!dir.path().join(".glep/delta.bin").exists());
    }

    #[test]
    fn hidden_files_flagged_and_filtered_by_default() {
        let dir = corpus();
        std::fs::write(dir.path().join(".secret.txt"), "hiddenneedle here").unwrap();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();

        let by_path = |p: &str| {
            idx.manifest
                .entries
                .iter()
                .find(|e| e.path.to_string_lossy() == p)
                .unwrap()
                .clone()
        };
        assert_eq!(
            by_path(".secret.txt").flags & manifest::FLAG_HIDDEN,
            manifest::FLAG_HIDDEN
        );
        assert_eq!(by_path("a.txt").flags & manifest::FLAG_HIDDEN, 0);

        let plan = crate::plan::build("hiddenneedle", true, crate::plan::Case::Sensitive);
        assert!(
            idx.candidates(&plan, false, false, false).is_empty(),
            "hidden file must not surface by default"
        );
        assert_eq!(
            idx.candidates(&plan, false, true, false),
            vec![std::path::PathBuf::from(".secret.txt")]
        );

        assert!(!idx
            .live_files(false)
            .contains(&std::path::PathBuf::from(".secret.txt")));
        assert!(idx
            .live_files(true)
            .contains(&std::path::PathBuf::from(".secret.txt")));
    }

    #[test]
    fn update_self_heals_hidden_files_into_index() {
        let dir = corpus();
        // Built before the hidden file exists: exercises the same
        // add-new-file path a pre-FLAG_HIDDEN index takes on its first
        // sweep after upgrade, since a hidden file it had never seen
        // before looks identical to any other brand new file.
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join(".newhidden.txt"), "healneedle appears").unwrap();
        idx.update(1_048_576, 0).unwrap();

        let plan = crate::plan::build("healneedle", true, crate::plan::Case::Sensitive);
        assert!(idx.candidates(&plan, false, false, false).is_empty());
        assert_eq!(
            idx.candidates(&plan, false, true, false),
            vec![std::path::PathBuf::from(".newhidden.txt")]
        );
    }
}
