//! The -q probe's view of an index (see `cli::probe`): the trigram
//! postings, opened without the eager check of their whole table, and the
//! manifest decoded lazily, entry by entry, only as far as the ids the
//! probe asks for. Nothing read here is trusted: every file the probe
//! searches is first looked up on disk the way the freshness sweep would
//! find it, so a stale, torn or corrupt index can only cost the probe its
//! speed, never its answer.

use super::postings::Postings;
use crate::plan::Case;
use std::path::{Path, PathBuf};

/// Bytes of a manifest entry besides its path: id (4), path length (8),
/// mtime (16), size (8) and flags (1), as bincode lays a `FileEntry` out.
const ENTRY_FIXED: usize = 37;

/// The manifest's bytes: mapped on Unix, where a replacing rename leaves
/// a mapped file be, and read whole elsewhere, where a mapping would hold
/// up another glep's rename of a new manifest over it.
#[cfg(unix)]
type ManifestBytes = memmap2::Mmap;
#[cfg(not(unix))]
type ManifestBytes = Vec<u8>;

pub struct QuickIndex {
    manifest: ManifestBytes,
    /// The entry count the manifest starts with: ids run below it.
    count: usize,
    /// Main postings, then the delta when it belongs to the same build.
    segs: Vec<Postings>,
    /// The id of the next entry to decode, and where it starts.
    next_id: usize,
    pos: usize,
    /// The manifest up to here is advised as needed (see `advise_ahead`).
    advised: usize,
}

/// A scan that reaches this far into the manifest is a long one: from
/// here on it asks for ADVISE_CHUNK bytes ahead at a time, mapped in one
/// call instead of a page fault per page.
const ADVISE_FROM: usize = 256 << 10;
const ADVISE_CHUNK: usize = 2 << 20;

/// One manifest entry, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickEntry {
    pub id: u32,
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
    pub flags: u8,
}

/// Little-endian integer of `N` bytes at `pos`, None past the end.
fn le<const N: usize>(bytes: &[u8], pos: usize) -> Option<[u8; N]> {
    bytes.get(pos..pos.checked_add(N)?)?.try_into().ok()
}

impl QuickIndex {
    /// The index of `root`, or None when it cannot be read as one.
    pub fn open(root: &Path) -> Option<QuickIndex> {
        let dir = root.join(".glep");
        #[cfg(unix)]
        // Safety: the map is read-only and glep replaces index files only
        // by an atomic rename, which leaves this mapping's file intact.
        let manifest =
            unsafe { memmap2::Mmap::map(&std::fs::File::open(dir.join("manifest.bin")).ok()?) }
                .ok()?;
        #[cfg(not(unix))]
        let manifest = std::fs::read(dir.join("manifest.bin")).ok()?;
        let count = usize::try_from(u64::from_le_bytes(le::<8>(&manifest, 0)?)).ok()?;
        let main = Postings::open_lazy(&dir.join("postings.bin")).ok()?;
        let mut segs = vec![main];
        if let Ok(delta) = Postings::open_lazy(&dir.join("delta.bin")) {
            // A delta from another build is a leftover (see
            // `Index::open_or_build_swept`).
            if delta.generation() == segs[0].generation() {
                segs.push(delta);
            }
        }
        Some(QuickIndex {
            manifest,
            count,
            segs,
            next_id: 0,
            pos: 8,
            advised: ADVISE_FROM,
        })
    }

    /// How many entries the manifest holds, dead ones included.
    pub fn len(&self) -> usize {
        self.count
    }

    /// The ids below `below` of the indexed files that hold every trigram
    /// of some group of a narrowed plan, ascending (`super::group_ids`).
    pub fn group_ids(&self, groups: &[Vec<u32>], case: Case, below: u64) -> Vec<u32> {
        let segs: Vec<&Postings> = self.segs.iter().collect();
        super::group_ids(&segs, groups, case, below)
    }

    /// The entries of `ids` (ascending, each past every id asked for
    /// before), decoded on from where the last call stopped. Ids past the
    /// manifest's end have none, and a malformed entry ends the decoding
    /// for good: the probe then knows fewer files, never wrong ones.
    #[cfg(test)]
    pub fn entries(&mut self, ids: &[u32]) -> Vec<QuickEntry> {
        self.entries_where(ids, usize::MAX, |_| true).0
    }

    /// `entries` for the ids of `ids` whose path `keep` accepts, until
    /// `max` are found; also returns how many ids that took. Rejected
    /// entries cost no decoding beyond their path.
    pub fn entries_where(
        &mut self,
        ids: &[u32],
        max: usize,
        mut keep: impl FnMut(&Path) -> bool,
    ) -> (Vec<QuickEntry>, usize) {
        let mut out = Vec::new();
        for (taken, &want) in ids.iter().enumerate() {
            if out.len() >= max {
                return (out, taken);
            }
            let want = want as usize;
            if want < self.next_id {
                continue;
            }
            while self.next_id < want {
                self.advise_ahead();
                match self.skip() {
                    Some(next) => {
                        self.pos = next;
                        self.next_id += 1;
                    }
                    None => return (self.stop(out), ids.len()),
                }
            }
            let Some((start, end, next)) = self.bounds() else {
                return (self.stop(out), ids.len());
            };
            let Ok(path) = std::str::from_utf8(&self.manifest[start..end]) else {
                return (self.stop(out), ids.len());
            };
            if keep(Path::new(path)) {
                match self.decode() {
                    Some((entry, _)) => out.push(entry),
                    None => return (self.stop(out), ids.len()),
                }
            }
            self.pos = next;
            self.next_id += 1;
        }
        (out, ids.len())
    }

    /// The next `max` entries, in id order from where the last call
    /// stopped, whose path `keep` accepts; fewer at the manifest's end.
    /// Rejected entries cost no decoding beyond their path.
    pub fn scan(&mut self, max: usize, mut keep: impl FnMut(&Path) -> bool) -> Vec<QuickEntry> {
        let mut out = Vec::new();
        while out.len() < max {
            self.advise_ahead();
            let Some((start, end, next)) = self.bounds() else {
                return self.stop(out);
            };
            let Ok(path) = std::str::from_utf8(&self.manifest[start..end]) else {
                return self.stop(out);
            };
            if keep(Path::new(path)) {
                match self.decode() {
                    Some((entry, _)) => out.push(entry),
                    None => return self.stop(out),
                }
            }
            self.pos = next;
            self.next_id += 1;
        }
        out
    }

    /// Once a scan runs past `advised`, advise the next ADVISE_CHUNK bytes
    /// of the manifest as needed (MADV_WILLNEED): faulting a long scan's
    /// pages in one at a time costs several times as much.
    fn advise_ahead(&mut self) {
        if self.pos < self.advised {
            return;
        }
        let end = self
            .pos
            .saturating_add(ADVISE_CHUNK)
            .min(self.manifest.len());
        #[cfg(unix)]
        if end > self.pos {
            let _ = self
                .manifest
                .advise_range(memmap2::Advice::WillNeed, self.pos, end - self.pos);
        }
        self.advised = end.max(self.pos + 1);
    }

    /// No entry is decoded after a malformed one or the last one.
    fn stop(&mut self, out: Vec<QuickEntry>) -> Vec<QuickEntry> {
        self.next_id = usize::MAX;
        out
    }

    /// The entry at `pos` must be the one with id `next_id` (ids are
    /// positions); returns its path's bounds and where the next one
    /// starts.
    fn bounds(&self) -> Option<(usize, usize, usize)> {
        if self.next_id >= self.count {
            return None;
        }
        let m = &self.manifest[..];
        let id = u32::from_le_bytes(le::<4>(m, self.pos)?) as usize;
        if id != self.next_id {
            return None;
        }
        let len = usize::try_from(u64::from_le_bytes(le::<8>(m, self.pos + 4)?)).ok()?;
        let start = self.pos + 12;
        let next = self.pos.checked_add(ENTRY_FIXED)?.checked_add(len)?;
        (next <= m.len()).then_some((start, start + len, next))
    }

    fn skip(&self) -> Option<usize> {
        self.bounds().map(|(_, _, next)| next)
    }

    fn decode(&self) -> Option<(QuickEntry, usize)> {
        let (start, end, next) = self.bounds()?;
        let m = &self.manifest[..];
        // serde writes a path as its UTF-8 text.
        let path = PathBuf::from(std::str::from_utf8(&m[start..end]).ok()?);
        let entry = QuickEntry {
            id: self.next_id as u32,
            path,
            mtime_ns: u128::from_le_bytes(le::<16>(m, end)?),
            size: u64::from_le_bytes(le::<8>(m, end + 16)?),
            flags: m[end + 24],
        };
        Some((entry, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("a.txt", "alpha needle\n"),
            ("b/c.txt", "beta\n"),
            ("b/d.txt", "gamma needle\n"),
            ("e.txt", "delta\n"),
        ] {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    /// Lazily decoded entries are the manifest's own, wherever a call
    /// starts and whichever ids it skips.
    #[test]
    fn entries_match_the_loaded_manifest() {
        let dir = tree();
        let idx = Index::build(dir.path(), 1 << 20).unwrap();
        let want: Vec<QuickEntry> = idx
            .manifest
            .entries
            .iter()
            .map(|e| QuickEntry {
                id: e.id,
                path: e.path.clone(),
                mtime_ns: e.mtime_ns,
                size: e.size,
                flags: e.flags,
            })
            .collect();
        assert_eq!(want.len(), 4);
        let mut q = QuickIndex::open(dir.path()).unwrap();
        assert_eq!(q.len(), 4);
        assert_eq!(q.entries(&[0]), want[..1]);
        assert_eq!(q.entries(&[2, 3]), want[2..]);
        // Past the end, and behind the decoding position: nothing.
        assert!(q.entries(&[1, 9]).is_empty());
        let mut q = QuickIndex::open(dir.path()).unwrap();
        assert_eq!(
            q.entries(&[1, 3, 7]),
            vec![want[1].clone(), want[3].clone()]
        );
    }

    /// The postings narrow like the index's own candidates.
    #[test]
    fn group_ids_match_the_index() {
        let dir = tree();
        let idx = Index::build(dir.path(), 1 << 20).unwrap();
        let q = QuickIndex::open(dir.path()).unwrap();
        let plan = crate::plan::build("needle", true, Case::Sensitive);
        let crate::plan::Plan::Groups(groups) = &plan else {
            panic!("a literal narrows");
        };
        let ids = q.group_ids(groups, Case::Sensitive, u64::MAX);
        assert_eq!(
            q.group_ids(groups, Case::Sensitive, u64::from(ids[1])),
            ids[..1]
        );
        let paths: Vec<PathBuf> = ids
            .iter()
            .map(|&id| idx.manifest.entries[id as usize].path.clone())
            .collect();
        assert_eq!(paths, idx.candidates(&plan, Case::Sensitive, false, false));
        assert_eq!(paths, [PathBuf::from("a.txt"), PathBuf::from("b/d.txt")]);
    }

    /// Entries by id decode only what the filter keeps, up to a count,
    /// and say how many ids that took.
    #[test]
    fn entries_where_stops_at_the_count() {
        let dir = tree();
        Index::build(dir.path(), 1 << 20).unwrap();
        let mut q = QuickIndex::open(dir.path()).unwrap();
        let (got, taken) = q.entries_where(&[0, 1, 2, 3], 1, |p| p.starts_with("b"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, PathBuf::from("b/c.txt"));
        assert_eq!(taken, 2);
        let (got, taken) = q.entries_where(&[2, 3], 9, |p| p.starts_with("b"));
        assert_eq!((got.len(), taken), (1, 2));
        assert_eq!(got[0].path, PathBuf::from("b/d.txt"));
    }

    /// A scan decodes the entries its filter keeps, in id order, and
    /// carries on from where it stopped.
    #[test]
    fn scan_keeps_what_the_filter_accepts() {
        let dir = tree();
        Index::build(dir.path(), 1 << 20).unwrap();
        let mut q = QuickIndex::open(dir.path()).unwrap();
        let under_b = |p: &Path| p.starts_with("b");
        let paths =
            |v: Vec<QuickEntry>| -> Vec<PathBuf> { v.into_iter().map(|e| e.path).collect() };
        assert_eq!(paths(q.scan(1, under_b)), [PathBuf::from("b/c.txt")]);
        assert_eq!(paths(q.scan(5, under_b)), [PathBuf::from("b/d.txt")]);
        assert!(q.scan(5, |_| true).is_empty());
        let mut q = QuickIndex::open(dir.path()).unwrap();
        assert_eq!(q.scan(9, |_| true).len(), 4);
    }

    /// A truncated manifest decodes the entries before the cut and none
    /// after it; a missing one opens no view.
    #[test]
    fn a_damaged_manifest_decodes_what_precedes_the_damage() {
        let dir = tree();
        Index::build(dir.path(), 1 << 20).unwrap();
        let manifest = dir.path().join(".glep/manifest.bin");
        let bytes = std::fs::read(&manifest).unwrap();
        let first = QuickIndex::open(dir.path()).unwrap().entries(&[0]);
        let cut = 8 + ENTRY_FIXED + first[0].path.as_os_str().len() + 5;
        std::fs::write(&manifest, &bytes[..cut]).unwrap();
        let mut q = QuickIndex::open(dir.path()).unwrap();
        assert_eq!(q.entries(&[0, 1, 2]), first);
        assert!(q.entries(&[3]).is_empty());
        std::fs::remove_file(&manifest).unwrap();
        assert!(QuickIndex::open(dir.path()).is_none());
    }
}
