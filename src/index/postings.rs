use memmap2::Mmap;
use std::collections::BTreeMap;
use std::path::Path;

const MAGIC: &[u8; 8] = b"GLEPPOST";
// Version history: 2 = hidden flagging, 3 = UTF-16 transcode entries,
// 4 = whitelist-rescued dotfiles (FLAG_HIDDEN must be recomputed for
// files a `!` rule un-hides; bumping forces a one-time rebuild),
// 5 = binary sniff widened to the searcher's 64 KB first buffer,
// FLAG_TRANSCODE for UTF-16 BOM files and FLAG_NUL_LATE (every entry's
// flags must be recomputed).
const VERSION: u32 = 5;
const HEADER: usize = 24; // magic 8 + version 4 + count 4 + generation 8
const ENTRY: usize = 16; // trigram 4 + offset 8 + len 4

pub fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

pub fn read_varint(buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut v = 0u64;
    let mut shift = 0u32;
    let mut i = 0usize;
    loop {
        if i >= buf.len() || shift >= 64 {
            return None; // truncated or overlong varint: corrupt blob
        }
        let byte = buf[i];
        v |= ((byte & 0x7f) as u64) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Some((v, &buf[i..]));
        }
        shift += 7;
    }
}

/// A posting blob that is not a well-formed varint list.
#[derive(Debug)]
pub struct Malformed;

/// Warn that a posting blob failed to decode; its trigram then counts as
/// empty.
pub fn report_corrupt() {
    if !crate::messages::is_hushed() {
        eprintln!("glep: postings blob corrupt; treating trigram as empty (run: glep index)");
    }
}

/// The next id delta of a posting blob, advancing `blob` past it; None
/// at the end. Dense lists are almost all one-byte deltas, which skip
/// the general varint decoder.
#[inline]
fn next_delta(blob: &mut &[u8]) -> Option<Result<u32, Malformed>> {
    let (&b, tail) = blob.split_first()?;
    if b < 0x80 {
        *blob = tail;
        return Some(Ok(b as u32));
    }
    Some(match read_varint(blob) {
        Some((v, rest)) => {
            *blob = rest;
            Ok(v as u32)
        }
        None => Err(Malformed),
    })
}

/// Push onto `out`, in order, every id of `acc` (ascending, unique) that
/// the posting `blob` holds: one merge pass straight off the encoded
/// bytes, which decodes the blob only as far as `acc`'s last id and
/// materializes nothing. Err when the blob is not a well-formed varint
/// list (`Postings::lookup` reports that).
pub fn intersect_blob(acc: &[u32], blob: &[u8], out: &mut Vec<u32>) -> Result<(), Malformed> {
    let mut acc = acc.iter().copied();
    let Some(mut want) = acc.next() else {
        return Ok(());
    };
    let mut blob = blob;
    let mut cur = 0u32;
    while let Some(delta) = next_delta(&mut blob) {
        cur = cur.wrapping_add(delta?);
        if cur < want {
            continue;
        }
        while want < cur {
            match acc.next() {
                Some(w) => want = w,
                None => return Ok(()),
            }
        }
        if want == cur {
            out.push(cur);
            match acc.next() {
                Some(w) => want = w,
                None => return Ok(()),
            }
        }
    }
    Ok(())
}

pub fn write(path: &Path, map: &BTreeMap<u32, Vec<u32>>, generation: u64) -> anyhow::Result<()> {
    let mut blob = Vec::new();
    let mut table = Vec::with_capacity(map.len());
    for (&tri, ids) in map {
        let start = blob.len() as u64;
        let mut prev = 0u32;
        for &id in ids {
            debug_assert!(id >= prev, "postings ids must be sorted ascending");
            write_varint(&mut blob, (id - prev) as u64);
            prev = id;
        }
        table.push((tri, start, (blob.len() as u64 - start) as u32));
    }
    let mut out = Vec::with_capacity(HEADER + table.len() * ENTRY + blob.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    for (tri, off, len) in &table {
        out.extend_from_slice(&tri.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&blob);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub struct Postings {
    mmap: Mmap,
    n: usize,
    generation: u64,
}

impl Postings {
    /// Open and check the whole file: every table entry must lie inside
    /// the blob region, so a corrupt file fails here (and the index is
    /// rebuilt) instead of losing candidates one lookup at a time.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let p = Self::open_lazy(path)?;
        let blob_len = p.mmap.len() - HEADER - p.n * ENTRY;
        for i in 0..p.n {
            let (_, off, len) = p.entry(i);
            anyhow::ensure!(
                off.checked_add(len).is_some_and(|end| end <= blob_len),
                "postings table entry out of bounds"
            );
        }
        Ok(p)
    }

    /// `open` without the check of every table entry, which reads the
    /// whole table (6 MB on a kernel-scale index): for the -q probe, which
    /// looks a few trigrams up and settles for less from a corrupt file.
    /// `blob` then treats an entry out of bounds as absent.
    pub fn open_lazy(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        // Safety: the map is read-only and glep replaces index files only via
        // atomic rename; a concurrent external truncation would SIGBUS, which
        // we accept for an internal index file.
        let mmap = unsafe { Mmap::map(&file)? };
        anyhow::ensure!(
            mmap.len() >= HEADER && &mmap[..8] == MAGIC,
            "not a glep postings file"
        );
        let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        anyhow::ensure!(version == VERSION, "postings version mismatch");
        let n = u32::from_le_bytes(mmap[12..16].try_into().unwrap()) as usize;
        let generation = u64::from_le_bytes(mmap[16..24].try_into().unwrap());
        anyhow::ensure!(mmap.len() >= HEADER + n * ENTRY, "truncated postings table");
        Ok(Self {
            mmap,
            n,
            generation,
        })
    }

    /// Index introspection helper; exercised by unit tests and kept for
    /// status/debug tooling.
    #[allow(dead_code)]
    pub fn trigram_count(&self) -> usize {
        self.n
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn entry(&self, i: usize) -> (u32, usize, usize) {
        let base = HEADER + i * ENTRY;
        let tri = u32::from_le_bytes(self.mmap[base..base + 4].try_into().unwrap());
        let off = u64::from_le_bytes(self.mmap[base + 4..base + 12].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(self.mmap[base + 12..base + 16].try_into().unwrap()) as usize;
        (tri, off, len)
    }

    /// Where a trigram's blob sits in the blob region, as (offset,
    /// length), or None when the table has no entry for it.
    fn find(&self, trigram: u32) -> Option<(usize, usize)> {
        let (mut lo, mut hi) = (0usize, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.entry(mid).0 < trigram {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= self.n {
            return None;
        }
        let (tri, off, len) = self.entry(lo);
        if tri != trigram {
            return None;
        }
        Some((off, len))
    }

    /// Stored byte length of a trigram's posting blob (0 when absent),
    /// read from the table without decoding anything. Every id takes at
    /// least one varint byte, so it orders trigrams by rarity.
    pub fn blob_len(&self, trigram: u32) -> usize {
        self.find(trigram).map_or(0, |(_, len)| len)
    }

    /// A trigram's posting blob as stored (delta varints, see `write`),
    /// or None when the table has no entry for it.
    pub fn blob(&self, trigram: u32) -> Option<&[u8]> {
        let (off, len) = self.find(trigram)?;
        let start = (HEADER + self.n * ENTRY).checked_add(off)?;
        self.mmap.get(start..start.checked_add(len)?)
    }

    pub fn lookup(&self, trigram: u32) -> Option<Vec<u32>> {
        self.lookup_below(trigram, u64::MAX)
    }

    /// `lookup`, decoding only the ids below `below`.
    pub fn lookup_below(&self, trigram: u32, below: u64) -> Option<Vec<u32>> {
        let mut slice = self.blob(trigram)?;
        // At most one id per byte.
        let mut ids = Vec::with_capacity(if below == u64::MAX { slice.len() } else { 0 });
        let mut cur = 0u32;
        while let Some(delta) = next_delta(&mut slice) {
            match delta {
                Ok(d) => {
                    cur = cur.wrapping_add(d);
                    if u64::from(cur) >= below {
                        break;
                    }
                    ids.push(cur);
                }
                Err(Malformed) => {
                    report_corrupt();
                    return None;
                }
            }
        }
        Some(ids)
    }

    pub fn iter_all(&self) -> Vec<(u32, Vec<u32>)> {
        (0..self.n)
            .map(|i| {
                let tri = self.entry(i).0;
                (tri, self.lookup(tri).unwrap_or_default())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn write_open_lookup_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("postings.bin");
        let mut map = BTreeMap::new();
        map.insert(5u32, vec![0u32, 1, 300, 70000]);
        map.insert(9u32, vec![2u32]);
        write(&p, &map, 42).unwrap();
        let post = Postings::open(&p).unwrap();
        assert_eq!(post.trigram_count(), 2);
        assert_eq!(post.lookup(5), Some(vec![0, 1, 300, 70000]));
        assert_eq!(post.lookup(9), Some(vec![2]));
        assert_eq!(post.lookup(6), None);
        assert_eq!(post.lookup(99), None);
        assert_eq!(post.generation(), 42);
    }

    #[test]
    fn open_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.bin");
        std::fs::write(&p, b"not a postings file at all").unwrap();
        assert!(Postings::open(&p).is_err());
    }

    #[test]
    fn varint_roundtrip() {
        let mut buf = Vec::new();
        for v in [0u64, 1, 127, 128, 300, 1 << 20, u32::MAX as u64] {
            buf.clear();
            write_varint(&mut buf, v);
            let (got, rest) = read_varint(&buf).unwrap();
            assert_eq!(got, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn truncated_varint_is_detected() {
        assert!(read_varint(&[0x80]).is_none());
        assert!(read_varint(&[]).is_none());
        let overlong = [0xffu8; 11];
        assert!(read_varint(&overlong).is_none());
    }

    /// xorshift64*: a deterministic generator for the randomized tests.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        }

        /// Up to `n` distinct ids below `max`, ascending.
        fn ids(&mut self, n: usize, max: u64) -> Vec<u32> {
            let mut v: Vec<u32> = (0..n).map(|_| self.below(max) as u32).collect();
            v.sort_unstable();
            v.dedup();
            v
        }
    }

    /// A posting blob as `write` stores it.
    fn encode(ids: &[u32]) -> Vec<u8> {
        let mut blob = Vec::new();
        let mut prev = 0;
        for &id in ids {
            write_varint(&mut blob, (id - prev) as u64);
            prev = id;
        }
        blob
    }

    fn naive(acc: &[u32], list: &[u32]) -> Vec<u32> {
        let set: std::collections::HashSet<u32> = list.iter().copied().collect();
        acc.iter().copied().filter(|id| set.contains(id)).collect()
    }

    #[test]
    fn intersect_blob_matches_a_naive_intersection() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // (acc ids, list ids, id space): similar sizes, dense and sparse
        // (multi-byte deltas, ids up to u32::MAX), and very skewed both
        // ways.
        let shapes: [(usize, usize, u64); 9] = [
            (50, 50, 200),
            (2000, 2000, 5000),
            (300, 300, 1 << 32),
            (3, 50_000, 100_000),
            (50_000, 3, 100_000),
            (1, 80_000, 100_000),
            (80_000, 1, 100_000),
            (1, 1, 2),
            (500, 20, 1 << 20),
        ];
        for (na, nl, max) in shapes {
            for _ in 0..8 {
                let acc = rng.ids(na, max);
                let list = rng.ids(nl, max);
                // Hits append after whatever `out` already holds.
                let mut out = vec![u32::MAX];
                intersect_blob(&acc, &encode(&list), &mut out).unwrap();
                assert_eq!(out[0], u32::MAX);
                assert_eq!(out[1..], naive(&acc, &list), "{na} vs {nl} below {max}");
            }
        }
    }

    #[test]
    fn intersect_blob_empty_disjoint_identical() {
        let mut out = Vec::new();
        // Nothing wanted: the blob is never read, not even a corrupt one.
        intersect_blob(&[], &encode(&[1, 2, 3]), &mut out).unwrap();
        intersect_blob(&[], &[0x80], &mut out).unwrap();
        // An empty blob holds nothing.
        intersect_blob(&[1, 2, 3], &[], &mut out).unwrap();
        intersect_blob(&[1, 3, 5], &encode(&[0, 2, 4, 6]), &mut out).unwrap();
        assert!(out.is_empty());
        // Id 0 (a zero first delta) and identical lists.
        intersect_blob(&[0, 2, 4], &encode(&[0, 2, 4]), &mut out).unwrap();
        assert_eq!(out, vec![0, 2, 4]);
    }

    #[test]
    fn intersect_blob_stops_past_the_last_wanted_id() {
        let mut blob = encode(&[1, 5, 9]);
        blob.push(0x80); // a truncated varint after id 9
        let mut out = Vec::new();
        // Decoding ends at id 5: the corrupt tail is never reached.
        intersect_blob(&[5], &blob, &mut out).unwrap();
        assert_eq!(out, vec![5]);
        // Wanting an id past it reaches the tail, which is malformed.
        assert!(intersect_blob(&[5, 10], &blob, &mut out).is_err());
    }

    #[test]
    fn blob_len_reads_the_table_without_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("postings.bin");
        let mut map = BTreeMap::new();
        map.insert(5u32, vec![0u32, 1, 300, 70000]);
        map.insert(9u32, vec![2u32]);
        write(&p, &map, 1).unwrap();
        let post = Postings::open(&p).unwrap();
        let five = encode(&[0, 1, 300, 70000]);
        assert_eq!(post.blob(5), Some(&five[..]));
        assert_eq!(post.blob_len(5), five.len());
        assert_eq!(post.blob_len(9), 1);
        // Absent trigrams, between and past the table's entries.
        for t in [0, 6, 99] {
            assert_eq!(post.blob_len(t), 0);
            assert_eq!(post.blob(t), None);
        }
    }

    #[test]
    fn open_rejects_out_of_bounds_table_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oob.bin");
        let mut map = BTreeMap::new();
        map.insert(7u32, vec![1u32, 2]);
        write(&p, &map, 1).unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        // corrupt the entry's len field (last 4 bytes of the 16-byte entry)
        let len_pos = 24 + 12;
        bytes[len_pos..len_pos + 4].copy_from_slice(&1000u32.to_le_bytes());
        std::fs::write(&p, &bytes).unwrap();
        assert!(Postings::open(&p).is_err());
    }
}
