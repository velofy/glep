use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    /// Relative to the sweep root.
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
}

/// Parallel gitignore-aware sweep. Skips hidden entries (default), so
/// .git/ and .glep/ never appear. Returns files sorted by relative path.
pub fn sweep(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
    let collected: Mutex<Vec<FileMeta>> = Mutex::new(Vec::new());
    let walker = ignore::WalkBuilder::new(root).require_git(false).build_parallel();
    walker.run(|| {
        Box::new(|entry| {
            if let Ok(e) = entry {
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
                            .strip_prefix(root)
                            .unwrap_or(e.path())
                            .to_path_buf();
                        collected.lock().unwrap().push(FileMeta {
                            path: rel,
                            mtime_ns,
                            size: md.len(),
                        });
                    }
                }
            }
            ignore::WalkState::Continue
        })
    });
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
}
