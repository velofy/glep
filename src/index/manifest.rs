use crate::walk::FileMeta;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FLAG_SKIP_BINARY: u8 = 1;
pub const FLAG_SKIP_TOO_LARGE: u8 = 2;
pub const FLAG_DEAD: u8 = 4;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    pub id: u32,
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
    pub flags: u8,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Manifest {
    pub entries: Vec<FileEntry>,
    pub last_sweep_epoch: u64,
}

impl Manifest {
    /// Ids are positional: entry with id N lives at entries[N]. Never reused.
    pub fn add(&mut self, meta: &FileMeta) -> u32 {
        let id = self.entries.len() as u32;
        self.entries.push(FileEntry {
            id,
            path: meta.path.clone(),
            mtime_ns: meta.mtime_ns,
            size: meta.size,
            flags: 0,
        });
        id
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(bincode::deserialize(&std::fs::read(path)?)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bincode::serialize(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn live_entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| e.flags & FLAG_DEAD == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::FileMeta;
    use std::path::PathBuf;

    fn meta(p: &str) -> FileMeta {
        FileMeta { path: PathBuf::from(p), mtime_ns: 42, size: 7 }
    }

    #[test]
    fn add_assigns_sequential_ids() {
        let mut m = Manifest::default();
        assert_eq!(m.add(&meta("a")), 0);
        assert_eq!(m.add(&meta("b")), 1);
        assert_eq!(m.entries[1].path, PathBuf::from("b"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.bin");
        let mut m = Manifest::default();
        m.add(&meta("x/y.rs"));
        m.entries[0].flags = FLAG_SKIP_BINARY;
        m.last_sweep_epoch = 123;
        m.save(&p).unwrap();
        let loaded = Manifest::load(&p).unwrap();
        assert_eq!(loaded.entries[0].path, PathBuf::from("x/y.rs"));
        assert_eq!(loaded.entries[0].flags, FLAG_SKIP_BINARY);
        assert_eq!(loaded.last_sweep_epoch, 123);
    }

    #[test]
    fn live_entries_filters_dead() {
        let mut m = Manifest::default();
        m.add(&meta("a"));
        m.add(&meta("b"));
        m.entries[0].flags |= FLAG_DEAD;
        let live: Vec<u32> = m.live_entries().map(|e| e.id).collect();
        assert_eq!(live, vec![1]);
    }
}
