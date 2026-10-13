pub mod manifest;
pub mod postings;

use crate::plan::{Case, Plan};
use crate::timing::Timings;
use crate::trigram;
use crate::walk::{self, FileMeta};
use fs2::FileExt;
use manifest::{
    Manifest, FLAG_DEAD, FLAG_HIDDEN, FLAG_NUL_LATE, FLAG_RACY, FLAG_SKIP_BINARY,
    FLAG_SKIP_TOO_LARGE, FLAG_TRANSCODE,
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

/// Another glep holds the index lock and there is no index to read yet:
/// it is building (or rebuilding) it right now. A query answers with a
/// live scan instead of waiting for that build (see `cli::run`); `glep
/// index` and `glep status` report it.
#[derive(Debug)]
pub struct IndexBusy;

impl std::fmt::Display for IndexBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another glep holds the index lock")
    }
}

impl std::error::Error for IndexBusy {}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A path keyed by its bytes for the freshness diff, hashed once: the
/// shard pick and the shard map's lookup share that hash, so Path's
/// component-wise Hash and Eq never run. Manifest and sweep paths both
/// come from the sweep walkers (`strip_prefix` of the root off a joined
/// path), so the same file is the same bytes on both sides. The hash is
/// never persisted.
#[derive(Clone, Copy)]
struct PathKey<'a> {
    hash: u64,
    bytes: &'a [u8],
}

impl<'a> PathKey<'a> {
    fn new(p: &'a Path) -> Self {
        use std::hash::Hasher;
        let bytes = p.as_os_str().as_encoded_bytes();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write(bytes);
        PathKey {
            hash: h.finish(),
            bytes,
        }
    }

    /// Shard index from bits the map's probing leaves alone: it takes the
    /// low bits for the slot and the top seven as the tag.
    fn shard(&self, mask: usize) -> usize {
        (self.hash >> 32) as usize & mask
    }
}

impl std::hash::Hash for PathKey<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl PartialEq for PathKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.bytes == other.bytes
    }
}

impl Eq for PathKey<'_> {}

/// Hands a `PathKey`'s precomputed hash to the map untouched.
#[derive(Default)]
struct PrehashedHasher(u64);

impl std::hash::Hasher for PrehashedHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Only `write_u64` is ever called (see `PathKey`'s Hash).
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ b as u64;
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

type PathMap<'a> = std::collections::HashMap<
    PathKey<'a>,
    &'a manifest::FileEntry,
    std::hash::BuildHasherDefault<PrehashedHasher>,
>;

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

/// The racy-mtime window around one clock reading (git's "racy git").
/// The freshness sweep takes a file whose (mtime, size) matches its entry
/// as unchanged, which holds only when every write after the entry's
/// content was read moved the mtime. The filesystem stamps writes from a
/// clock coarser than the one read here (a kernel tick behind on Linux,
/// whole seconds on HFS+, two on FAT), so a file written within
/// `RACY_SLACK` of that read may be rewritten in the same tick, at the
/// same size, with its stat unchanged. Its entry carries `FLAG_RACY`
/// until a sweep re-reads it from outside the window.
#[derive(Clone, Copy)]
struct RacyWindow {
    /// The reading minus the slack: content read after the reading from
    /// a file with an mtime at or after this is racy.
    floor_ns: u128,
    /// The reading plus the slack: no write made before the reading
    /// carries a later mtime.
    horizon_ns: u128,
}

impl RacyWindow {
    /// The window of the clock as it reads now. A sweep takes it after
    /// its last stat and before it reads any file's content.
    fn now() -> RacyWindow {
        RacyWindow::at(std::time::SystemTime::now())
    }

    fn at(t: std::time::SystemTime) -> RacyWindow {
        let ns = |t: std::time::SystemTime| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        };
        let slack = crate::dircache::RACY_SLACK;
        RacyWindow {
            floor_ns: t.checked_sub(slack).map_or(0, ns),
            horizon_ns: ns(t) + slack.as_nanos(),
        }
    }

    /// Whether content read after the reading, from a file with
    /// `mtime_ns`, is racy: a later write may land on that same mtime.
    /// An mtime past the reading is racy too.
    fn racy(&self, mtime_ns: u128) -> bool {
        mtime_ns >= self.floor_ns
    }

    /// Whether a racy entry whose stat still matches must be re-read: a
    /// write since its content was read may carry its mtime. No write so
    /// far carries one beyond the horizon (a clock that was set back, an
    /// archive's future dates), so such an entry keeps its flag, at no
    /// cost, until the clock comes within the slack of its mtime.
    fn due(&self, mtime_ns: u128) -> bool {
        mtime_ns <= self.horizon_ns
    }
}

/// Index `meta`'s content under `id` (see `index_file`), reading it after
/// `window`'s reading, and return the entry's flags: `index_file`'s, the
/// sweep's hidden flag, and `FLAG_RACY` when the content read is racy.
/// Oversized and unreadable files are never racy: no content of theirs
/// was recorded, and every query scans them.
fn entry_flags(
    root: &Path,
    meta: &FileMeta,
    id: u32,
    max_filesize: u64,
    map: &mut BTreeMap<u32, Vec<u32>>,
    window: &RacyWindow,
) -> u8 {
    let mut flags = index_file(root, meta, id, max_filesize, map);
    if meta.hidden {
        flags |= FLAG_HIDDEN;
    }
    if flags & FLAG_SKIP_TOO_LARGE == 0 && window.racy(meta.mtime_ns) {
        flags |= FLAG_RACY;
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

    /// Create the index dir with its self-ignore file: git never tracks
    /// the index, and the user's .gitignore is never touched.
    fn prepare_dir(dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let self_ignore = dir.join(".gitignore");
        if !self_ignore.exists() {
            std::fs::write(&self_ignore, "*\n")?;
        }
        Ok(())
    }

    pub fn build(root: &Path, max_filesize: u64) -> anyhow::Result<Index> {
        Self::build_swept(root, max_filesize, None)
    }

    /// `build`, indexing `swept` (an unsorted full sweep of `root` this
    /// process already ran) instead of sweeping again when given.
    fn build_swept(
        root: &Path,
        max_filesize: u64,
        swept: Option<Vec<FileMeta>>,
    ) -> anyhow::Result<Index> {
        let dir = root.join(".glep");
        Self::prepare_dir(&dir)?;
        let (lock, busy) = Self::acquire_lock(&dir);
        if busy {
            return Err(IndexBusy.into());
        }
        Self::build_locked(root, max_filesize, lock, swept)
    }

    /// The detached first build (`glep index` with GLEP_BG set, spawned by
    /// a query that answered from a sweep): build only while no index
    /// exists, under the lock. False, without building, when another glep
    /// holds the lock (a second spawner lost the race, or a query is busy
    /// with an index that appeared meanwhile) or the manifest is already
    /// there: what a later query finds then is the open path's business.
    pub fn build_if_missing(root: &Path, max_filesize: u64) -> anyhow::Result<bool> {
        let dir = root.join(".glep");
        Self::prepare_dir(&dir)?;
        let (lock, busy) = Self::acquire_lock(&dir);
        if busy || dir.join("manifest.bin").exists() {
            return Ok(false);
        }
        Self::build_locked(root, max_filesize, lock, None)?;
        Ok(true)
    }

    /// Whether a detached build could start now: the index dir, its
    /// self-ignore and its lock file can be created, and no other glep
    /// holds the lock. The probe's lock is released before this returns,
    /// so no child spawned afterwards ever shares it.
    #[cfg(unix)]
    pub fn can_build(root: &Path) -> bool {
        let dir = root.join(".glep");
        if Self::prepare_dir(&dir).is_err() {
            return false;
        }
        let (lock, busy) = Self::acquire_lock(&dir);
        drop(lock);
        !busy
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
        swept: Option<Vec<FileMeta>>,
    ) -> anyhow::Result<Index> {
        let dir = root.join(".glep");
        let generation = new_generation();
        let metas = match swept {
            // `walk::sweep`'s order: ids follow path order.
            Some(mut v) => {
                v.sort_by(|a, b| a.path.cmp(&b.path));
                v
            }
            None => walk::sweep(root)?,
        };
        // Every file is read below, after this reading.
        let window = RacyWindow::now();
        let mut man = Manifest::default();
        let mut map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (n, meta) in metas.iter().enumerate() {
            if n > 0 && n % 5000 == 0 {
                eprintln!("glep: indexed {n} files...");
            }
            let id = man.add(meta);
            man.entries[id as usize].flags =
                entry_flags(root, meta, id, max_filesize, &mut map, &window);
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
        Self::open_or_build_swept(root, max_filesize, None)
    }

    /// `open_or_build`; a build indexes `swept` (an unsorted full sweep of
    /// `root` this process already ran) when given, instead of sweeping
    /// the tree a second time. An existing index ignores it.
    pub fn open_or_build_swept(
        root: &Path,
        max_filesize: u64,
        swept: Option<Vec<FileMeta>>,
    ) -> anyhow::Result<Index> {
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
                    // A busy lock means another glep is writing the index
                    // right now (a build or a compaction): what failed to
                    // read is its work in progress, not a broken index.
                    let (lock, busy) = Self::acquire_lock(&dir);
                    if busy {
                        return Err(IndexBusy.into());
                    }
                    eprintln!("glep: index unreadable ({e}); rebuilding");
                    return Self::build_locked(root, max_filesize, lock, swept);
                }
            }
        }
        Self::build_swept(root, max_filesize, swept)
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

    /// Every spelling `case` looks `tri` up as.
    fn lookup_variants(tri: u32, case: Case) -> Vec<u32> {
        match case {
            Case::Sensitive => vec![tri],
            Case::Ascii => trigram::case_variants(tri),
            Case::Unicode => trigram::fold_variants(tri),
        }
    }

    fn segments(&self) -> impl Iterator<Item = &Postings> {
        [self.main.as_ref(), self.delta.as_ref()]
            .into_iter()
            .flatten()
    }

    fn postings_for(&self, tri: u32, case: Case) -> Vec<u32> {
        let mut ids = Vec::new();
        for t in Self::lookup_variants(tri, case) {
            for seg in self.segments() {
                if let Some(v) = seg.lookup(t) {
                    ids.extend(v);
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Total stored blob bytes of `tri`'s postings, every spelling `case`
    /// looks it up as, main and delta, without decoding any of them: 0
    /// when no indexed file holds the trigram.
    fn postings_len(&self, tri: u32, case: Case) -> usize {
        Self::lookup_variants(tri, case)
            .into_iter()
            .map(|t| self.segments().map(|seg| seg.blob_len(t)).sum::<usize>())
            .sum()
    }

    /// Narrow `acc` (ascending, unique) to the ids `tri`'s postings hold,
    /// under any spelling `case` looks it up as, main and delta. Each
    /// stored blob is merged against `acc` as it decodes, so no list is
    /// materialized and none decodes past `acc`'s last id. A blob that
    /// fails to decode before then counts as empty, as `Postings::lookup`
    /// treats it.
    fn retain_postings(&self, acc: &mut Vec<u32>, tri: u32, case: Case) {
        let mut hits = Vec::with_capacity(acc.len());
        let mut sources = 0;
        for t in Self::lookup_variants(tri, case) {
            for seg in self.segments() {
                let Some(blob) = seg.blob(t) else {
                    continue;
                };
                sources += 1;
                let before = hits.len();
                if postings::intersect_blob(acc, blob, &mut hits).is_err() {
                    hits.truncate(before);
                    postings::report_corrupt();
                }
            }
        }
        if sources > 1 {
            // Several spellings or segments: their hits interleave.
            hits.sort_unstable();
            hits.dedup();
        }
        *acc = hits;
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
        case: Case,
        include_hidden: bool,
        search_binary: bool,
    ) -> Vec<PathBuf> {
        let mut ids = self.candidate_ids(plan, case, search_binary);
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
        case: Case,
        include_hidden: bool,
        search_binary: bool,
    ) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let ids = self.candidate_ids(plan, case, search_binary);
        self.split_hidden(ids, include_hidden)
    }

    fn candidate_ids(&self, plan: &Plan, case: Case, search_binary: bool) -> Vec<u32> {
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
                    // Rarest first, by stored blob size (nothing decoded):
                    // the narrowest list starts the intersection, and a
                    // group holding an absent trigram matches no indexed
                    // file, so it is skipped before decoding anything.
                    let mut by_size: Vec<(usize, u32)> = group
                        .iter()
                        .map(|&t| (self.postings_len(t, case), t))
                        .collect();
                    by_size.sort_unstable();
                    let Some(&(smallest, rarest)) = by_size.first() else {
                        continue;
                    };
                    if smallest == 0 {
                        continue;
                    }
                    let mut acc = self.postings_for(rarest, case);
                    for &(_, t) in &by_size[1..] {
                        if acc.is_empty() {
                            break;
                        }
                        self.retain_postings(&mut acc, t, case);
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

    /// See the `swept_full` field. Before any update this holds only for
    /// an index this process just built, whose freshness sweep would
    /// repeat the build's own sweep: queries skip it.
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
    /// but they are also never consulted by the scoped query. A full
    /// sweep within the ttl already verified the scope, so the scoped
    /// sweep is skipped too; but the global sweep epoch is only bumped for
    /// unscoped (full) sweeps: a scoped sweep must not suppress the
    /// freshness sweep a later unscoped query needs (ttl correctness).
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
        // The epoch marks "the whole tree was verified as of epoch", which
        // covers every scope too: within the ttl no sweep runs, scoped or
        // not. Only a full sweep sets it (below), since a scoped sweep
        // leaves the other subtrees unverified.
        let scope: Option<&[PathBuf]> =
            scope.filter(|s| !s.is_empty() && !s.iter().any(|p| p.as_os_str().is_empty()));
        let scoped = scope.is_some();
        if ttl_secs > 0 && now_epoch().saturating_sub(self.manifest.last_sweep_epoch) <= ttl_secs {
            return Ok(Vec::new());
        }
        let swept = match scope {
            Some(s) => walk::sweep_scoped_unsorted(&self.root, s)?,
            None => walk::sweep_unsorted(&self.root)?,
        };
        // After the sweep's last stat, before any file is read below.
        let window = RacyWindow::now();
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
        // changed ids come out of phase 1 per shard. Every path is hashed
        // exactly once, as bytes (see `PathKey`). A racy entry's matching
        // stat proves nothing, so one that is due counts as changed: its
        // file is read again, as of now (see `RacyWindow`).
        let n_shards = rayon::current_num_threads().max(1).next_power_of_two();
        let shard_mask = n_shards - 1;
        // Phase 0a: key every live in-scope entry, in parallel.
        let keyed: Vec<(PathKey, &manifest::FileEntry)> = self
            .manifest
            .entries
            .par_iter()
            .filter(|e| e.flags & manifest::FLAG_DEAD == 0 && in_scope(&e.path))
            .map(|e| (PathKey::new(&e.path), e))
            .collect();
        // Phase 0b: build each shard's lookup map in parallel, each from
        // its own keys only, so no HashMap is ever shared or mutated
        // concurrently.
        // An even share plus headroom, so no shard map rehashes.
        let per_shard = keyed.len() / n_shards;
        let shard_maps: Vec<PathMap> = (0..n_shards)
            .into_par_iter()
            .map(|shard| {
                let mut map = PathMap::with_capacity_and_hasher(
                    per_shard + per_shard / 4 + 16,
                    Default::default(),
                );
                map.extend(
                    keyed
                        .iter()
                        .filter(|(k, _)| k.shard(shard_mask) == shard)
                        .copied(),
                );
                map
            })
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
        // Unchanged files whose hidden flag the sweep now computes
        // differently: an ignore file gained or lost the `!` rule that
        // rescues them, which touches neither the file nor its mtime.
        let mut hidden_flips: Vec<(u32, bool)> = Vec::new();
        {
            let chunk = (swept.len() / n_shards / 2).max(1024);
            type Diff = (Vec<FileMeta>, Vec<u32>, Vec<(u32, bool)>);
            let results: Vec<Diff> = swept
                .par_chunks(chunk)
                .map(|metas| {
                    let mut my_fresh = Vec::new();
                    let mut my_changed = Vec::new();
                    let mut my_flips = Vec::new();
                    for meta in metas {
                        let key = PathKey::new(&meta.path);
                        match shard_maps[key.shard(shard_mask)].get(&key) {
                            Some(e) => {
                                seen[e.id as usize].store(1, std::sync::atomic::Ordering::Relaxed);
                                if e.mtime_ns != meta.mtime_ns
                                    || e.size != meta.size
                                    || (e.flags & FLAG_RACY != 0 && window.due(e.mtime_ns))
                                {
                                    my_fresh.push(meta.clone());
                                    my_changed.push(e.id);
                                } else if (e.flags & FLAG_HIDDEN != 0) != meta.hidden {
                                    my_flips.push((e.id, meta.hidden));
                                }
                            }
                            None => my_fresh.push(meta.clone()), // brand new
                        }
                    }
                    (my_fresh, my_changed, my_flips)
                })
                .collect();
            for (f, c, h) in results {
                fresh.extend(f);
                changed_old_ids.extend(c);
                hidden_flips.extend(h);
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
        // The flags are what queries read, so they flip in memory even
        // when the manifest cannot be written (read-only mode).
        for &(id, hidden) in &hidden_flips {
            let flags = &mut self.manifest.entries[id as usize].flags;
            if hidden {
                *flags |= FLAG_HIDDEN;
            } else {
                *flags &= !FLAG_HIDDEN;
            }
        }

        if self.read_only {
            // Nothing is written, but this query must not see what the
            // sweep found gone: a deleted file's entry and a changed (or
            // due racy) file's old one die in memory, as the hidden flips
            // above change there. Changed files come back as `fresh`
            // paths, which the caller searches live.
            for &id in dead_ids.iter().chain(&changed_old_ids) {
                self.manifest.entries[id as usize].flags |= manifest::FLAG_DEAD;
            }
            if let Some(t) = &mut timings {
                t.stage("index_write");
            }
            return Ok(fresh.into_iter().map(|m| m.path).collect());
        }
        if fresh.is_empty() && dead_ids.is_empty() {
            // Flipped hidden flags must persist: later queries trust them.
            let mut save = !hidden_flips.is_empty();
            // Only a full sweep may advance the freshness epoch.
            if !scoped {
                self.manifest.last_sweep_epoch = now_epoch();
                self.swept_full = true;
                // The persisted epoch is consumed solely by ttl checks; skip
                // the write when ttl is unused so a no-op query stays
                // write-free. A later process may then see a slightly stale
                // epoch, which can only cause an extra sweep, never a
                // missed one.
                save |= ttl_secs > 0;
            }
            if save {
                self.manifest.save(&self.dir.join("manifest.bin"))?;
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
            self.manifest.entries[id as usize].flags =
                entry_flags(&self.root, meta, id, max_filesize, &mut map, &window);
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
            *self = Index::build_locked(&self.root.clone(), max_filesize, lock, None)?;
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

/// Set the mtime of every file under `root` (the index dir aside) an hour
/// back, as an untouched tree's would be. A fixture written just now is
/// racy (see `RacyWindow`): the first sweep after its index is built
/// reads every file of it again.
#[cfg(test)]
pub(crate) fn age_files(root: &Path) {
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let kind = e.file_type().unwrap();
            if kind.is_dir() && e.file_name() != ".glep" {
                stack.push(e.path());
            } else if kind.is_file() {
                std::fs::File::options()
                    .write(true)
                    .open(e.path())
                    .unwrap()
                    .set_modified(hour_ago)
                    .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four files written an hour ago (see `age_files`).
    fn corpus() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();
        std::fs::write(dir.path().join("b.txt"), "goodbye world").unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"\x00\x01binary").unwrap();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(100)).unwrap();
        age_files(dir.path());
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
        age_files(dir.path());
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
        let plan = crate::plan::build("needle", true, Case::Sensitive);
        let c = idx.candidates(&plan, Case::Sensitive, false, false);
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
        let plan = crate::plan::build("v2x a", true, Case::Sensitive);
        let c = idx.candidates(&plan, Case::Sensitive, false, false);
        assert_eq!(c, vec![std::path::PathBuf::from("src/a.txt")]);
        // Out-of-scope changes were never looked at: b.txt's new
        // contents are NOT a candidate (still stale, which is sound,
        // since a scoped query never returns it anyway).
        let plan2 = crate::plan::build("b v2x", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan2, Case::Sensitive, false, false)
            .is_empty());
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
        let plan = crate::plan::build("new visible", true, Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false),
            vec![std::path::PathBuf::from("src/y.txt")]
        );
        let plan2 = crate::plan::build("new ignored", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan2, Case::Sensitive, false, false)
            .is_empty());
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

    /// Adding or removing the `!` rule that rescues a dotfile flips its
    /// hidden flag on the next sweep, in place: the file keeps its entry
    /// and postings (no rebuild, no reindex), and a read-only pass flips
    /// it for its own queries too.
    #[test]
    fn hidden_flag_follows_whitelist_edits() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            let words: String = (0..30).map(|k| format!("w{i}x{k}y ")).collect();
            std::fs::write(dir.path().join(format!("f{i:03}.txt")), words).unwrap();
        }
        std::fs::write(dir.path().join(".env"), "token").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "!.env\n").unwrap();
        // Untouched files keep their entries only once their stat is
        // trusted: outside the racy window.
        age_files(dir.path());
        Index::build(dir.path(), 1_048_576).unwrap();
        let mut idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        let env = |idx: &Index| {
            let e = idx
                .manifest
                .live_entries()
                .find(|e| e.path == Path::new(".env"))
                .unwrap();
            (e.id, e.flags & FLAG_HIDDEN != 0)
        };
        let (id, hidden) = env(&idx);
        assert!(!hidden, "whitelisted at build time");
        std::fs::write(dir.path().join(".gitignore"), "*.log\n*.tmp\n").unwrap();
        idx.update(1_048_576, 0).unwrap();
        assert!(idx.has_delta(), "one edited file must not compact");
        assert_eq!(env(&idx), (id, true), "rule removed: hidden, same entry");
        let reopened = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert_eq!(env(&reopened), (id, true), "the flip was saved");
        std::fs::write(dir.path().join(".gitignore"), "!.env\n").unwrap();
        let mut ro = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        ro.read_only = true;
        ro.update(1_048_576, 0).unwrap();
        assert_eq!(env(&ro), (id, false), "read-only pass flips in memory");
        idx.update(1_048_576, 0).unwrap();
        assert_eq!(env(&idx), (id, false), "rule restored: visible again");
    }

    /// A build from a sweep the caller already ran is the build that
    /// sweeps for itself: the same entries, ids, flags and postings. A
    /// fresh build is authoritative (swept_full, so queries skip the
    /// freshness sweep it would repeat); an opened index is not.
    #[test]
    fn a_build_from_a_presweep_matches_a_plain_build() {
        let a = corpus();
        let b = corpus();
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
            std::fs::write(dir.path().join("sub/deep/z.txt"), "hello deep").unwrap();
            std::fs::write(dir.path().join("sub/.dot"), "hello dot").unwrap();
            // Racy flags would depend on how soon each build ran.
            age_files(dir.path());
        }
        let plain = Index::build(a.path(), 1_048_576).unwrap();
        let mut swept = walk::sweep_unsorted(b.path()).unwrap();
        swept.reverse();
        let pre = Index::open_or_build_swept(b.path(), 1_048_576, Some(swept)).unwrap();
        assert!(plain.swept_full() && pre.swept_full());
        let entries = |idx: &Index| -> Vec<(u32, PathBuf, u64, u8)> {
            idx.manifest
                .entries
                .iter()
                .map(|e| (e.id, e.path.clone(), e.size, e.flags))
                .collect()
        };
        assert_eq!(entries(&plain), entries(&pre));
        for pat in ["hello", "world", "deep"] {
            let plan = crate::plan::build(pat, true, Case::Sensitive);
            assert_eq!(
                plain.candidates(&plan, Case::Sensitive, true, false),
                pre.candidates(&plan, Case::Sensitive, true, false),
                "{pat}"
            );
        }
        // An existing index ignores the presweep.
        drop(pre);
        let reopened = Index::open_or_build_swept(b.path(), 1_048_576, Some(Vec::new())).unwrap();
        assert!(!reopened.swept_full());
        assert_eq!(entries(&reopened), entries(&plain));
    }

    /// The detached build fills in a missing index only: once the
    /// manifest exists, or while another glep holds the lock, it builds
    /// nothing.
    #[test]
    fn build_if_missing_builds_a_missing_index_only() {
        let dir = corpus();
        assert!(Index::build_if_missing(dir.path(), 1_048_576).unwrap());
        let mpath = dir.path().join(".glep/manifest.bin");
        let before = std::fs::read(&mpath).unwrap();
        std::fs::write(dir.path().join("c.txt"), "new file").unwrap();
        assert!(!Index::build_if_missing(dir.path(), 1_048_576).unwrap());
        assert_eq!(std::fs::read(&mpath).unwrap(), before);

        let dir = corpus();
        let held = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::remove_file(dir.path().join(".glep/manifest.bin")).unwrap();
        assert!(!Index::build_if_missing(dir.path(), 1_048_576).unwrap());
        assert!(!dir.path().join(".glep/manifest.bin").exists());
        drop(held);
        assert!(Index::build_if_missing(dir.path(), 1_048_576).unwrap());
        assert_eq!(
            Index::open_or_build(dir.path(), 1_048_576)
                .unwrap()
                .live_files(false)
                .len(),
            4
        );
    }

    /// A query spawns a background build only when one could start: the
    /// index dir (with its self-ignore) and lock file can be created, and
    /// no other glep holds the lock. The probe never keeps the lock.
    #[cfg(unix)]
    #[test]
    fn can_build_needs_a_writable_index_dir_and_a_free_lock() {
        let dir = corpus();
        assert!(Index::can_build(dir.path()));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".glep/.gitignore")).unwrap(),
            "*\n"
        );
        assert!(Index::can_build(dir.path()), "the probe kept the lock");
        let held = Index::build(dir.path(), 1_048_576).unwrap();
        assert!(!Index::can_build(dir.path()));
        drop(held);
        assert!(Index::can_build(dir.path()));

        let blocked = corpus();
        std::fs::write(blocked.path().join(".glep"), "not a directory").unwrap();
        assert!(!Index::can_build(blocked.path()));
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
        let plan = crate::plan::build("hello", true, Case::Sensitive);
        let c = idx.candidates(&plan, Case::Sensitive, false, false);
        assert_eq!(c, vec![std::path::PathBuf::from("a.txt")]);
    }

    #[test]
    fn candidates_case_insensitive_uses_variants() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let plan = crate::plan::build("HELLO", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan, Case::Sensitive, false, false)
            .is_empty());
        let c = idx.candidates(&plan, Case::Unicode, false, false);
        assert_eq!(c, vec![std::path::PathBuf::from("a.txt")]);
    }

    #[test]
    fn candidates_include_oversized_files_always() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 50).unwrap(); // big.txt skip-flagged
        let plan = crate::plan::build("hello", true, Case::Sensitive);
        let c = idx.candidates(&plan, Case::Sensitive, false, false);
        assert!(c.contains(&std::path::PathBuf::from("a.txt")));
        assert!(c.contains(&std::path::PathBuf::from("big.txt")));
        // Binary files join only when the caller can surface them
        // (-a/--binary); under quit detection they can never emit output.
        assert!(!c.contains(&std::path::PathBuf::from("bin.dat")));
        let c = idx.candidates(&plan, Case::Sensitive, false, true);
        assert!(c.contains(&std::path::PathBuf::from("bin.dat")));
    }

    #[test]
    fn candidates_all_returns_live_non_binary() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let c = idx.candidates(&crate::plan::Plan::All, Case::Sensitive, false, false);
        assert_eq!(c.len(), 3); // a.txt, b.txt, big.txt; bin.dat excluded
        let c = idx.candidates(&crate::plan::Plan::All, Case::Sensitive, false, true);
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
        let plan = crate::plan::build("freshneedle", true, Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false),
            vec![std::path::PathBuf::from("new.txt")]
        );
        let plan2 = crate::plan::build("changedneedle", true, Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan2, Case::Sensitive, false, false),
            vec![std::path::PathBuf::from("a.txt")]
        );
    }

    #[test]
    fn update_tombstones_deleted_files() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        idx.update(1_048_576, 0).unwrap();
        let plan = crate::plan::build("hello", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan, Case::Sensitive, false, false)
            .is_empty());
    }

    #[test]
    fn update_respects_ttl() {
        let dir = corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join("late.txt"), "ttlneedle").unwrap();
        idx.update(1_048_576, 3600).unwrap(); // within ttl: sweep skipped
        let plan = crate::plan::build("ttlneedle", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan, Case::Sensitive, false, false)
            .is_empty());
        idx.update(1_048_576, 0).unwrap(); // ttl 0: always sweeps
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false).len(),
            1
        );
    }

    /// A scoped query honors a fresh ttl epoch like a full one (the full
    /// sweep that set it covered the scope), yet never sets the epoch
    /// itself.
    #[test]
    fn scoped_update_respects_ttl_but_never_sets_the_epoch() {
        let dir = corpus();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        Index::build(dir.path(), 1_048_576).unwrap();
        let mut idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join("src/late.txt"), "ttlneedle").unwrap();
        let scope = [PathBuf::from("src")];
        let mut timings = Timings::new();
        idx.update_scoped(1_048_576, 3600, &scope, &mut timings)
            .unwrap(); // within ttl: sweep skipped
        let plan = crate::plan::build("ttlneedle", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan, Case::Sensitive, false, false)
            .is_empty());
        // An expired epoch: the scoped sweep runs, without renewing it.
        idx.manifest.last_sweep_epoch = 1;
        idx.update_scoped(1_048_576, 3600, &scope, &mut timings)
            .unwrap();
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false).len(),
            1
        );
        assert_eq!(idx.manifest.last_sweep_epoch, 1);
        assert!(!idx.swept_full());
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
        let plan = crate::plan::build("persistneedle", true, Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false).len(),
            1
        );
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
        let plan = crate::plan::build("uniqtoken1999", true, Case::Sensitive);
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false),
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

    /// With no index to read and the lock held by another glep (a build
    /// in progress), building or opening is `IndexBusy`, which queries
    /// answer with a live scan.
    #[test]
    fn build_under_a_held_lock_is_busy() {
        use fs2::FileExt;
        let dir = corpus();
        std::fs::create_dir_all(dir.path().join(".glep")).unwrap();
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.path().join(".glep/lock"))
            .unwrap();
        held.lock_exclusive().unwrap();
        let busy = |r: anyhow::Result<Index>| r.err().is_some_and(|e| e.is::<IndexBusy>());
        assert!(busy(Index::build(dir.path(), 1_048_576)));
        assert!(busy(Index::open_or_build(dir.path(), 1_048_576)));
        // A torn manifest under a held lock is the writer's work in
        // progress: busy too, never a rebuild.
        std::fs::write(dir.path().join(".glep/manifest.bin"), b"torn").unwrap();
        assert!(busy(Index::open_or_build(dir.path(), 1_048_576)));
        FileExt::unlock(&held).unwrap();
        assert!(Index::open_or_build(dir.path(), 1_048_576).is_ok());
    }

    /// Under another glep's lock an update writes nothing, yet the query
    /// running it must not see what the sweep found gone: a deleted file
    /// leaves the live set and the candidates, and a changed file's old
    /// entry (with its old postings) dies too, its path coming back as a
    /// fresh one for a live search.
    #[test]
    fn read_only_update_writes_nothing_and_returns_fresh_paths() {
        let dir = corpus();
        let _writer = Index::build(dir.path(), 1_048_576).unwrap(); // holds the lock
        let mut reader = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(reader.read_only);
        let id_of = |idx: &Index, p: &str| {
            idx.manifest
                .entries
                .iter()
                .find(|e| e.path == Path::new(p))
                .unwrap()
                .id
        };
        let (old_a, old_b) = (id_of(&reader, "a.txt"), id_of(&reader, "b.txt"));
        std::fs::write(dir.path().join("hot.txt"), "hotneedle").unwrap();
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello there, longer now").unwrap();
        let manifest_before = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
        let mut extra = reader.update(1_048_576, 0).unwrap();
        extra.sort();
        assert_eq!(
            extra,
            vec![PathBuf::from("a.txt"), PathBuf::from("hot.txt")]
        );
        let manifest_after = std::fs::read(dir.path().join(".glep/manifest.bin")).unwrap();
        assert_eq!(
            manifest_before, manifest_after,
            "read-only update must not write"
        );
        assert!(!dir.path().join(".glep/delta.bin").exists());
        // The deleted file and the changed file's stale entry are gone
        // from this query's view; the untouched files stay.
        assert!(!reader.is_live(old_a) && !reader.is_live(old_b));
        assert_eq!(
            reader.live_files(true),
            vec![PathBuf::from("big.txt"), PathBuf::from("bin.dat")]
        );
        let plan = crate::plan::build("world", true, Case::Sensitive);
        assert!(
            reader
                .candidates(&plan, Case::Sensitive, true, false)
                .is_empty(),
            "no live entry still holds `world`"
        );
        assert!(!reader.has_files_under(Path::new("b.txt")));
        // A second read-only pass over the same tree finds the same.
        let mut again = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(again.read_only);
        let mut extra = again.update(1_048_576, 0).unwrap();
        extra.sort();
        assert_eq!(
            extra,
            vec![PathBuf::from("a.txt"), PathBuf::from("hot.txt")]
        );
        assert_eq!(again.live_files(true), reader.live_files(true));
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

        let plan = crate::plan::build("hiddenneedle", true, Case::Sensitive);
        assert!(
            idx.candidates(&plan, Case::Sensitive, false, false)
                .is_empty(),
            "hidden file must not surface by default"
        );
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, true, false),
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

        let plan = crate::plan::build("healneedle", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan, Case::Sensitive, false, false)
            .is_empty());
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, true, false),
            vec![std::path::PathBuf::from(".newhidden.txt")]
        );
    }

    /// xorshift64*: deterministic randomness for the model tests.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        }

        fn text(&mut self, letters: &[u8], len: u64) -> Vec<u8> {
            (0..len)
                .map(|_| letters[self.below(letters.len() as u64) as usize])
                .collect()
        }
    }

    /// A group holding a trigram no file has narrows to nothing (it is
    /// skipped before any list decodes); other alternatives still count.
    #[test]
    fn candidates_skip_a_group_with_an_absent_trigram() {
        let dir = corpus();
        let idx = Index::build(dir.path(), 1_048_576).unwrap();
        let plan = crate::plan::build("hello|zqzqz", false, Case::Sensitive);
        assert!(matches!(&plan, Plan::Groups(g) if g.len() == 2));
        assert_eq!(
            idx.candidates(&plan, Case::Sensitive, false, false),
            vec![PathBuf::from("a.txt")]
        );
        // `zqz` is absent, though `wor` and `orl` sit in two files.
        let plan = crate::plan::build("worlzqz", true, Case::Sensitive);
        assert!(idx
            .candidates(&plan, Case::Sensitive, false, false)
            .is_empty());
    }

    /// Rarest-first, streaming candidate narrowing agrees with a naive
    /// model (a file is a candidate when, for some group, it holds every
    /// trigram under some spelling the case mode looks it up as), with
    /// trigrams ranging from every file to none, postings split across
    /// main and delta, in every case mode.
    #[test]
    fn candidates_match_a_naive_model() {
        // Skewed letters: `a` is in every file, `K` and `k` in few.
        const LETTERS: &[u8] = b"aaaaaaaaaabbbbbccdKk";
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let dir = tempfile::tempdir().unwrap();
        let name = |i: usize| PathBuf::from(format!("f{i:03}.txt"));
        for i in 0..400 {
            let len = 100 + rng.below(200);
            std::fs::write(dir.path().join(name(i)), rng.text(LETTERS, len)).unwrap();
        }
        age_files(dir.path());
        let mut idx = Index::build(dir.path(), 1 << 20).unwrap();
        // A changed file and a new one: their postings land in the delta,
        // so lookups merge main and delta blobs.
        for i in [7, 400] {
            let len = 400 + rng.below(100); // never a built file's size
            std::fs::write(dir.path().join(name(i)), rng.text(LETTERS, len)).unwrap();
        }
        idx.update(1 << 20, 0).unwrap();
        assert!(idx.has_delta(), "two edits must not compact");
        // Each file's trigrams, straight from its bytes.
        let files: Vec<(PathBuf, std::collections::HashSet<u32>)> = (0..=400)
            .map(|i| {
                let text = std::fs::read(dir.path().join(name(i))).unwrap();
                (name(i), text.windows(3).map(trigram::pack).collect())
            })
            .collect();
        for _ in 0..80 {
            let (la, lb) = (3 + rng.below(4), 3 + rng.below(4));
            let a = String::from_utf8(rng.text(LETTERS, la)).unwrap();
            let b = String::from_utf8(rng.text(LETTERS, lb)).unwrap();
            for (pattern, fixed) in [(a.clone(), true), (format!("{a}|{b}"), false)] {
                for case in [Case::Sensitive, Case::Ascii, Case::Unicode] {
                    let plan = crate::plan::build(&pattern, fixed, case);
                    let Plan::Groups(groups) = &plan else {
                        panic!("{pattern}: a literal plan narrows");
                    };
                    let want: Vec<PathBuf> = files
                        .iter()
                        .filter(|(_, tris)| {
                            groups.iter().any(|g| {
                                g.iter().all(|&t| {
                                    Index::lookup_variants(t, case)
                                        .iter()
                                        .any(|v| tris.contains(v))
                                })
                            })
                        })
                        .map(|(p, _)| p.clone())
                        .collect();
                    assert_eq!(
                        idx.candidates(&plan, case, false, false),
                        want,
                        "{pattern:?} {case:?}"
                    );
                }
            }
        }
    }

    /// The sharded freshness diff on a tree that fills every shard:
    /// unchanged files keep their entries, changed and new files get new
    /// ones, deleted files are tombstoned, and nothing else moves.
    #[test]
    fn update_diff_classifies_files_in_every_shard() {
        let dir = tempfile::tempdir().unwrap();
        let name = |i: usize| PathBuf::from(format!("d{}/f{i:04}.txt", i % 7));
        for d in 0..7 {
            std::fs::create_dir_all(dir.path().join(format!("d{d}"))).unwrap();
        }
        for i in 0..1200 {
            std::fs::write(dir.path().join(name(i)), "ab".repeat(20 + i % 13)).unwrap();
        }
        age_files(dir.path());
        let mut idx = Index::build(dir.path(), 1 << 20).unwrap();
        let before: std::collections::HashMap<PathBuf, u32> = idx
            .manifest
            .live_entries()
            .map(|e| (e.path.clone(), e.id))
            .collect();
        let changed: Vec<usize> = (0..1200).step_by(101).collect();
        let deleted: Vec<usize> = (50..1200).step_by(97).collect();
        for &i in &changed {
            // A size no built file has, so coarse mtimes cannot hide it.
            std::fs::write(dir.path().join(name(i)), "ab".repeat(60)).unwrap();
        }
        for &i in &deleted {
            std::fs::remove_file(dir.path().join(name(i))).unwrap();
        }
        for i in 1200..1210 {
            std::fs::write(dir.path().join(name(i)), "ab".repeat(5)).unwrap();
        }
        idx.update(1 << 20, 0).unwrap();
        assert!(idx.has_delta(), "a few edits must not compact");
        let mut want: Vec<PathBuf> = (0..1210)
            .filter(|i| !deleted.contains(i))
            .map(name)
            .collect();
        want.sort();
        assert_eq!(idx.live_files(true), want);
        assert_eq!(idx.manifest.live_entries().count(), want.len());
        for e in idx.manifest.live_entries() {
            let i: usize = e.path.file_stem().unwrap().to_str().unwrap()[1..]
                .parse()
                .unwrap();
            if changed.contains(&i) || i >= 1200 {
                assert!(e.id as usize >= before.len(), "{:?} re-added", e.path);
            } else {
                assert_eq!(before[&e.path], e.id, "{:?} kept its entry", e.path);
            }
        }
    }

    #[test]
    fn path_keys_compare_bytes_and_share_one_hash() {
        let owned = PathBuf::from("src/a.rs");
        let (a, b) = (PathKey::new(Path::new("src/a.rs")), PathKey::new(&owned));
        assert!(a == b);
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.shard(15), b.shard(15));
        assert!(PathKey::new(Path::new("src/b.rs")) != a);
        // Bytes, not components: the sweep walkers never spell one file
        // two ways, so the diff needs no component-wise equality.
        assert!(PathKey::new(Path::new("src//a.rs")) != a);
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// Overwrite `name` in `dir` with `body`, at its old size, and pin its
    /// mtime back: a rewrite inside the filesystem clock tick of the last
    /// write, which leaves the file's (mtime, size) as they were.
    fn rewrite_in_the_same_tick(dir: &Path, name: &str, body: &[u8]) {
        let path = dir.join(name);
        let before = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), body.len() as u64, "{name}: same size");
        let mtime = before.modified().unwrap();
        std::fs::write(&path, body).unwrap();
        set_mtime(&path, mtime);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    }

    /// `corpus()` plus two files written just now: `r.txt` (text) and
    /// `t.bin` (binary). Their mtimes are pinned a second ahead, so an
    /// index built within three seconds reads them inside the racy
    /// window, and every later sweep finds them due.
    fn racy_corpus() -> tempfile::TempDir {
        let dir = corpus();
        let soon = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
        for (name, body) in [("r.txt", &b"oldneedle\n"[..]), ("t.bin", b"\x00oldtext\n")] {
            std::fs::write(dir.path().join(name), body).unwrap();
            set_mtime(&dir.path().join(name), soon);
        }
        dir
    }

    fn entry<'a>(idx: &'a Index, p: &str) -> &'a manifest::FileEntry {
        idx.manifest
            .live_entries()
            .find(|e| e.path == Path::new(p))
            .unwrap()
    }

    /// The candidates for the literal `pattern`, hidden files included.
    fn holding(idx: &Index, pattern: &str) -> Vec<PathBuf> {
        let plan = crate::plan::build(pattern, true, Case::Sensitive);
        idx.candidates(&plan, Case::Sensitive, true, false)
    }

    /// A file rewritten at its size within the clock tick of the write its
    /// entry recorded keeps its (mtime, size), so its stat alone would
    /// serve the old content's postings and flags for good. An entry read
    /// within the racy window is racy, and the next sweep reads its file
    /// again: the new content is a candidate, the old is not (a text file
    /// that turned binary, or back, takes its new flags too), and so it
    /// stays once reopened. Older files are not racy.
    #[test]
    fn a_same_size_rewrite_in_the_same_tick_is_read_again() {
        let dir = racy_corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        for p in ["r.txt", "t.bin"] {
            assert_ne!(entry(&idx, p).flags & FLAG_RACY, 0, "{p} was just written");
        }
        for p in ["a.txt", "b.txt", "bin.dat", "big.txt"] {
            assert_eq!(entry(&idx, p).flags & FLAG_RACY, 0, "{p} is old");
        }
        let recorded = |idx: &Index, p: &str| (entry(idx, p).mtime_ns, entry(idx, p).size);
        let stats = [recorded(&idx, "r.txt"), recorded(&idx, "t.bin")];
        rewrite_in_the_same_tick(dir.path(), "r.txt", b"newneedle\n");
        rewrite_in_the_same_tick(dir.path(), "t.bin", b"xnewtext\n");
        let swept = walk::sweep(dir.path()).unwrap();
        for (p, stat) in ["r.txt", "t.bin"].into_iter().zip(stats) {
            let m = swept.iter().find(|m| m.path == Path::new(p)).unwrap();
            assert_eq!((m.mtime_ns, m.size), stat, "{p}: the stat cannot tell");
        }
        idx.update(1_048_576, 0).unwrap();
        let check = |idx: &Index| {
            assert_eq!(holding(idx, "newneedle"), vec![PathBuf::from("r.txt")]);
            assert!(holding(idx, "oldneedle").is_empty());
            assert_eq!(holding(idx, "newtext"), vec![PathBuf::from("t.bin")]);
            assert!(!idx.binary_files().contains(Path::new("t.bin")));
        };
        check(&idx);
        drop(idx);
        check(&Index::open_or_build(dir.path(), 1_048_576).unwrap());
    }

    /// Under another glep's lock the sweep writes nothing, yet a racy
    /// entry dies in this query's memory like a changed one, and its file
    /// comes back as a fresh path, which the caller searches live.
    #[test]
    fn a_read_only_sweep_hands_racy_files_back_to_search_live() {
        let dir = racy_corpus();
        let _writer = Index::build(dir.path(), 1_048_576).unwrap(); // holds the lock
        let mut reader = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(reader.read_only);
        rewrite_in_the_same_tick(dir.path(), "r.txt", b"newneedle\n");
        let mpath = dir.path().join(".glep/manifest.bin");
        let manifest = std::fs::read(&mpath).unwrap();
        let mut extra = reader.update(1_048_576, 0).unwrap();
        extra.sort();
        assert_eq!(extra, vec![PathBuf::from("r.txt"), PathBuf::from("t.bin")]);
        assert!(holding(&reader, "oldneedle").is_empty());
        assert!(!reader.live_files(true).contains(&PathBuf::from("r.txt")));
        assert_eq!(std::fs::read(&mpath).unwrap(), manifest);
        assert!(!dir.path().join(".glep/delta.bin").exists());
    }

    /// --ttl keeps its meaning: inside the window no sweep runs, so a
    /// racy entry stands, as any entry whose file changed since; the
    /// first sweep after the window reads the file again and renews the
    /// epoch.
    #[test]
    fn racy_entries_wait_for_the_ttl_like_any_edit() {
        let dir = racy_corpus();
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        rewrite_in_the_same_tick(dir.path(), "r.txt", b"newneedle\n");
        let id = entry(&idx, "r.txt").id;
        idx.update(1_048_576, 3600).unwrap();
        assert_eq!(entry(&idx, "r.txt").id, id, "no sweep inside the ttl");
        assert_eq!(holding(&idx, "oldneedle"), vec![PathBuf::from("r.txt")]);
        idx.manifest.last_sweep_epoch = 1;
        idx.update(1_048_576, 3600).unwrap();
        assert_eq!(holding(&idx, "newneedle"), vec![PathBuf::from("r.txt")]);
        assert!(holding(&idx, "oldneedle").is_empty());
        assert!(
            idx.manifest.last_sweep_epoch > 1,
            "a full sweep renews the epoch"
        );
    }

    /// A racy entry read again from outside the window settles: its new
    /// entry is not racy, and later sweeps trust its stat and write
    /// nothing. No write so far can carry an mtime beyond the window (a
    /// future date), so such an entry keeps its id and its flag until the
    /// clock comes within the slack of it.
    #[test]
    fn racy_entries_settle_and_future_mtimes_wait() {
        let dir = corpus();
        // Enough postings that reading one file again does not compact.
        for i in 0..200 {
            let words: String = (0..30).map(|k| format!("w{i}x{k}y ")).collect();
            std::fs::write(dir.path().join(format!("f{i:03}.txt")), words).unwrap();
        }
        age_files(dir.path());
        let in_an_hour = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        set_mtime(&dir.path().join("b.txt"), in_an_hour);
        let mut idx = Index::build(dir.path(), 1_048_576).unwrap();
        // As if a.txt, written an hour ago, had been read within its window.
        let a = entry(&idx, "a.txt").id;
        idx.manifest.entries[a as usize].flags |= FLAG_RACY;
        let b = entry(&idx, "b.txt").id;
        assert_ne!(entry(&idx, "b.txt").flags & FLAG_RACY, 0);
        idx.update(1_048_576, 0).unwrap();
        assert!(idx.has_delta(), "one file read again must not compact");
        assert_ne!(entry(&idx, "a.txt").id, a, "read again");
        assert_eq!(entry(&idx, "a.txt").flags & FLAG_RACY, 0, "settled");
        assert_eq!(entry(&idx, "b.txt").id, b, "not due yet");
        assert_ne!(entry(&idx, "b.txt").flags & FLAG_RACY, 0);
        let mpath = dir.path().join(".glep/manifest.bin");
        let manifest = std::fs::read(&mpath).unwrap();
        idx.update(1_048_576, 0).unwrap();
        assert_eq!(
            std::fs::read(&mpath).unwrap(),
            manifest,
            "nothing to read again"
        );
    }

    #[test]
    fn the_racy_window_is_the_slack_on_either_side_of_its_reading() {
        let sec = 1_000_000_000u128;
        let w = RacyWindow::at(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000));
        let (at, slack) = (1_000_000 * sec, crate::dircache::RACY_SLACK.as_nanos());
        assert!(!w.racy(0) && !w.racy(at - slack - 1));
        assert!(w.racy(at - slack) && w.racy(at));
        assert!(w.racy(at + 3600 * sec), "an mtime past the reading is racy");
        assert!(w.due(0) && w.due(at) && w.due(at + slack));
        assert!(!w.due(at + slack + 1), "no write so far carries it");
    }
}
