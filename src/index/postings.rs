use memmap2::Mmap;
use std::collections::BTreeMap;
use std::path::Path;

const MAGIC: &[u8; 8] = b"GLEPPOST";
const VERSION: u32 = 1;
const HEADER: usize = 16; // magic 8 + version 4 + count 4
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

pub fn write(path: &Path, map: &BTreeMap<u32, Vec<u32>>) -> anyhow::Result<()> {
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
}

impl Postings {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
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
        anyhow::ensure!(mmap.len() >= HEADER + n * ENTRY, "truncated postings table");
        let p = Self { mmap, n };
        let blob_len = p.mmap.len() - HEADER - n * ENTRY;
        for i in 0..n {
            let (_, off, len) = p.entry(i);
            anyhow::ensure!(
                off.checked_add(len).map_or(false, |end| end <= blob_len),
                "postings table entry out of bounds"
            );
        }
        Ok(p)
    }

    pub fn trigram_count(&self) -> usize {
        self.n
    }

    fn entry(&self, i: usize) -> (u32, usize, usize) {
        let base = HEADER + i * ENTRY;
        let tri = u32::from_le_bytes(self.mmap[base..base + 4].try_into().unwrap());
        let off = u64::from_le_bytes(self.mmap[base + 4..base + 12].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(self.mmap[base + 12..base + 16].try_into().unwrap()) as usize;
        (tri, off, len)
    }

    pub fn lookup(&self, trigram: u32) -> Option<Vec<u32>> {
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
        let blob_start = HEADER + self.n * ENTRY;
        let mut slice = &self.mmap[blob_start + off..blob_start + off + len];
        let mut ids = Vec::new();
        let mut cur = 0u32;
        while !slice.is_empty() {
            match read_varint(slice) {
                Some((v, rest)) => {
                    cur = cur.wrapping_add(v as u32);
                    ids.push(cur);
                    slice = rest;
                }
                None => {
                    eprintln!(
                        "glep: postings blob corrupt; treating trigram as empty (run: glep index)"
                    );
                    return None;
                }
            }
        }
        Some(ids)
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
        write(&p, &map).unwrap();
        let post = Postings::open(&p).unwrap();
        assert_eq!(post.trigram_count(), 2);
        assert_eq!(post.lookup(5), Some(vec![0, 1, 300, 70000]));
        assert_eq!(post.lookup(9), Some(vec![2]));
        assert_eq!(post.lookup(6), None);
        assert_eq!(post.lookup(99), None);
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

    #[test]
    fn open_rejects_out_of_bounds_table_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oob.bin");
        let mut map = BTreeMap::new();
        map.insert(7u32, vec![1u32, 2]);
        write(&p, &map).unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        // corrupt the entry's len field (last 4 bytes of the 16-byte entry)
        let len_pos = 16 + 12;
        bytes[len_pos..len_pos + 4].copy_from_slice(&1000u32.to_le_bytes());
        std::fs::write(&p, &bytes).unwrap();
        assert!(Postings::open(&p).is_err());
    }
}
