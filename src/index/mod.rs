pub mod manifest;
pub mod postings;

use crate::trigram;
use crate::walk::{self, FileMeta};
use fs2::FileExt;
use manifest::{Manifest, FLAG_SKIP_BINARY, FLAG_SKIP_TOO_LARGE};
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
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
}

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
    let sniff = &content[..content.len().min(8192)];
    if sniff.contains(&0) {
        return FLAG_SKIP_BINARY;
    }
    for tri in trigram::extract(&content) {
        map.entry(tri).or_default().push(id);
    }
    0
}

impl Index {
    fn acquire_lock(dir: &Path) -> (Option<std::fs::File>, bool) {
        let lock_path = dir.join("lock");
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
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

        let generation = new_generation();
        let metas = walk::sweep(root)?;
        let mut man = Manifest::default();
        let mut map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (n, meta) in metas.iter().enumerate() {
            if n > 0 && n % 5000 == 0 {
                eprintln!("glep: indexed {n} files...");
            }
            let id = man.add(meta);
            man.entries[id as usize].flags = index_file(root, meta, id, max_filesize, &mut map);
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
                    });
                }
                Err(e) => {
                    eprintln!("glep: index unreadable ({e}); rebuilding");
                }
            }
        }
        Self::build(root, max_filesize)
    }

    pub fn live_files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self
            .manifest
            .live_entries()
            .map(|e| e.path.clone())
            .collect();
        v.sort();
        v
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
        let files = idx.live_files();
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn open_or_build_reopens_existing() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(!idx.read_only);
        assert_eq!(idx.live_files().len(), 4);
    }

    #[test]
    fn corrupt_index_rebuilds() {
        let dir = corpus();
        Index::build(dir.path(), 1_048_576).unwrap();
        std::fs::write(dir.path().join(".glep/postings.bin"), b"garbage").unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert_eq!(idx.live_files().len(), 4);
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
        assert_eq!(idx.live_files().len(), 4);
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
        assert_eq!(idx.live_files().len(), 4);
        drop(idx);

        // Matching generation: delta must attach.
        let man = manifest::Manifest::load(&dir.path().join(".glep/manifest.bin")).unwrap();
        postings::write(&dir.path().join(".glep/delta.bin"), &map, man.generation).unwrap();
        let idx = Index::open_or_build(dir.path(), 1_048_576).unwrap();
        assert!(idx.has_delta());
    }
}
