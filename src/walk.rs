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
pub fn sweep(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
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
}
