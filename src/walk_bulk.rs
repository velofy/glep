//! macOS fast path for `walk::sweep`, built on `getattrlistbulk(2)`.
//!
//! `getattrlistbulk` returns name, object type, mtime and size for every
//! entry in a directory in one syscall, instead of one `stat()`-class call
//! per file. This collapses the sweep's per-file stat storm into one
//! syscall per directory. See the prototype report this module was built
//! from for the measured win (roughly 1.55x wall-clock on a real corpus)
//! and for why the walk has to be single-pass: a two-pass design (discover
//! directories with the `ignore` crate, then bulk-scan each one) pays for a
//! redundant readdir traversal plus open()/close() per directory and ends
//! up slower than the plain per-file walker, not faster.
//!
//! Single-pass design: a shared pool of worker threads pulls directories to
//! scan off a work queue (rayon's scope/spawn scheduler: each worker has a
//! local work-stealing deque, and `subdirs` discovered by one scan are
//! pushed back onto that same queue for any idle worker to pick up).
//! Subdirectory names come out of the very same `getattrlistbulk` call that
//! returns file attributes, so there is no separate directory-discovery
//! pass. Files accumulate in one buffer per worker thread (indexed by
//! `rayon::current_thread_index()`) and are only merged into a single
//! sorted `Vec` once, at the very end of the sweep.
//!
//! Ignore semantics mirror `walk::sweep`'s `ignore::WalkBuilder` defaults:
//! hidden (dot-prefixed) entries are now INCLUDED (each emitted `FileMeta`
//! carries a `hidden` flag, computed the same way `walk.rs` computes it:
//! true when any path component starts with '.'), and `.gitignore` files
//! are honored per directory level, stacked along the recursion, with no
//! dependency on an actual `.git` directory being present (the same
//! "require_git(false)" behavior `sweep` opts into). The sole exception,
//! independent of hidden-ness and of gitignore rules, is `.git` and
//! `.glep`: those are hard-excluded at any depth, by component name, never
//! descended into or emitted (see `is_hard_excluded` below), matching
//! `walk.rs`'s `is_hard_excluded_component`. Beyond the sweep root's own
//! and nested `.gitignore` files, `sweep` (via the `ignore` crate,
//! `parents(true)`) also honors: global excludes (`core.excludesfile` /
//! `$XDG_CONFIG_HOME/git/ignore` / `~/.config/git/ignore`),
//! `.git/info/exclude`, ancestor `.gitignore` files above the sweep root,
//! and `.ignore` files (plus their whitelist overrides). `build_ancestor_stack`
//! seeds the matcher stack with the first three of those before the walk
//! starts (see its doc comment for precedence order); `.ignore`/`.rgignore`
//! are handled by refusing to walk at all wherever one is found, see below.
//!
//! Correctness posture: any anomaly while parsing a `getattrlistbulk`
//! result buffer (an offset or length that doesn't fit inside the buffer)
//! is treated as fatal and turns into an `Err` from `sweep_bulk`, which the
//! dispatcher in `walk.rs` turns into a fallback to the portable walker.
//! Anything narrower, like one directory being unreadable (permission
//! denied, deleted mid-scan), is non-fatal: it is reported on stderr and
//! that subtree is skipped, exactly like `walk::sweep` does for individual
//! entry errors from the `ignore` crate. The same Fatal path is also used,
//! deliberately, whenever a directory contains a `.ignore` or `.rgignore`
//! file: their precedence relative to `.gitignore` (an `.ignore` whitelist
//! entry can override a `.gitignore` ignore entry, for instance) isn't
//! reimplemented here, so rather than risk an approximate match this bulk
//! path bows out and lets the portable walker, which gets that precedence
//! right by construction, handle the whole sweep instead.

#![cfg(target_os = "macos")]

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::walk::FileMeta;

// ---------------------------------------------------------------------
// getattrlistbulk FFI surface. Declared manually since the `libc` crate
// does not expose attrlist / getattrlistbulk bindings. Layout taken from
// <sys/attr.h> and getattrlist(2).
// ---------------------------------------------------------------------

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

const ATTR_BIT_MAP_COUNT: u16 = 5;

const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

// fsobj_type_t (sys/vnode.h).
const VREG: u32 = 1;
const VDIR: u32 = 2;

const FSOPT_NOFOLLOW: u64 = 0x0000_0001;

const BULK_BUF_SIZE: usize = 64 * 1024;

extern "C" {
    fn getattrlistbulk(
        dirfd: libc::c_int,
        alist: *mut AttrList,
        attrbuf: *mut libc::c_void,
        attrbufsize: libc::size_t,
        options: u64,
    ) -> libc::c_int;
}

fn make_attrlist() -> AttrList {
    AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_DATALENGTH,
        forkattr: 0,
    }
}

/// One parsed directory entry: name plus whatever attributes the kernel
/// actually returned for it (a directory never gets ATTR_FILE_DATALENGTH,
/// for example, since that bit is only set in the RETURNED bitmap for
/// entries where the fileattr group applies).
///
/// `name` is an `OsString` built directly from the raw bytes the kernel
/// returned (`OsStr::from_bytes`), not a `String::from_utf8_lossy`
/// conversion: APFS allows filenames that are not valid UTF-8, and lossy
/// conversion would silently corrupt those names (replacing the invalid
/// bytes with U+FFFD) before they ever reach a `FileMeta.path`. Carrying
/// the exact original bytes through `OsString` means such a name round
/// trips exactly, the same way the portable `ignore`-crate walker's
/// `DirEntry` paths already do.
struct BulkEntry {
    name: OsString,
    objtype: u32,
    mtime_ns: u128,
    size: u64,
}

struct DirScan {
    files: Vec<BulkEntry>,
    subdirs: Vec<OsString>,
}

/// Non-fatal: one directory could not be scanned (permission denied,
/// deleted mid-walk, etc). Reported on stderr, subtree skipped, sweep
/// continues, exactly like an individual `ignore::Error` from the walker.
/// Fatal: the attribute buffer did not parse the way getattrlistbulk's
/// documented layout says it should. Something is wrong enough with our
/// assumptions about the buffer that no result from this sweep should be
/// trusted; the whole sweep_bulk call fails and the dispatcher falls back
/// to the walker.
enum ScanError {
    Soft(String),
    Fatal(anyhow::Error),
}

fn read_u32(buf: &[u8], off: usize) -> anyhow::Result<u32> {
    let bytes = buf
        .get(off..off + 4)
        .ok_or_else(|| anyhow::anyhow!("buffer overrun reading u32 at offset {off}"))?;
    Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
}

fn read_i32(buf: &[u8], off: usize) -> anyhow::Result<i32> {
    let bytes = buf
        .get(off..off + 4)
        .ok_or_else(|| anyhow::anyhow!("buffer overrun reading i32 at offset {off}"))?;
    Ok(i32::from_ne_bytes(bytes.try_into().unwrap()))
}

fn read_i64(buf: &[u8], off: usize) -> anyhow::Result<i64> {
    let bytes = buf
        .get(off..off + 8)
        .ok_or_else(|| anyhow::anyhow!("buffer overrun reading i64 at offset {off}"))?;
    Ok(i64::from_ne_bytes(bytes.try_into().unwrap()))
}

/// Parse `count` entries out of a getattrlistbulk result buffer.
///
/// Buffer layout per entry (see getattrlist(2)):
///   u32 entry_length                (includes this field)
///   attribute_set_t returned        (5 x u32: common/vol/dir/file/fork)
///   [ATTR_CMN_NAME]        attrreference_t (i32 offset, u32 length) + inline string
///   [ATTR_CMN_OBJTYPE]     u32 fsobj_type_t
///   [ATTR_CMN_MODTIME]     struct timespec (i64 tv_sec, i64 tv_nsec)
///   [ATTR_FILE_DATALENGTH] i64 off_t   (only present if the fileattr group
///                                        was actually returned, which the
///                                        kernel omits for non-file entries
///                                        such as directories; the code
///                                        below branches on the *returned*
///                                        bitmap, never the *requested* one)
///
/// Every offset read goes through a bounds-checked helper. A buffer that
/// doesn't match this layout produces an `Err`, never a panic or an
/// out-of-bounds read.
fn parse_bulk_buffer(buf: &[u8], count: i32, out: &mut Vec<BulkEntry>) -> anyhow::Result<()> {
    let mut offset = 0usize;
    for _ in 0..count {
        anyhow::ensure!(
            offset < buf.len(),
            "entry start {offset} at or past buffer end {}",
            buf.len()
        );
        let entry_start = offset;
        let entry_len = read_u32(buf, entry_start)? as usize;
        anyhow::ensure!(entry_len >= 4, "implausible entry_length {entry_len}");
        let entry_end = entry_start
            .checked_add(entry_len)
            .ok_or_else(|| anyhow::anyhow!("entry_length overflow at offset {entry_start}"))?;
        anyhow::ensure!(
            entry_end <= buf.len(),
            "entry [{entry_start}, {entry_end}) extends past buffer end {}",
            buf.len()
        );

        let mut cursor = entry_start + 4;
        let common_returned = read_u32(buf, cursor)?;
        let fileattr_returned = read_u32(buf, cursor + 12)?;
        cursor += 20; // attribute_set_t: 5 x u32

        let mut name = OsString::new();
        let mut objtype: u32 = 0;
        let mut mtime_ns: u128 = 0;
        let mut size: u64 = 0;

        if common_returned & ATTR_CMN_NAME != 0 {
            anyhow::ensure!(cursor + 8 <= entry_end, "name attrref out of entry bounds");
            let ref_start = cursor;
            let attr_dataoffset = read_i32(buf, cursor)?;
            let attr_length = read_u32(buf, cursor + 4)? as usize;
            let data_start = ref_start
                .checked_add_signed(attr_dataoffset as isize)
                .ok_or_else(|| anyhow::anyhow!("name attr_dataoffset overflow"))?;
            let data_end = data_start
                .checked_add(attr_length)
                .ok_or_else(|| anyhow::anyhow!("name attr_length overflow"))?;
            anyhow::ensure!(
                data_end <= entry_end,
                "name bytes [{data_start}, {data_end}) escape entry bounds (entry_end {entry_end})"
            );
            let raw = buf
                .get(data_start..data_end)
                .ok_or_else(|| anyhow::anyhow!("name bytes out of buffer bounds"))?;
            let nul_pos = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            // Exact byte-for-byte round trip, valid UTF-8 or not: see the
            // `BulkEntry::name` doc comment above for why this is not a
            // `String::from_utf8_lossy` conversion.
            name = OsStr::from_bytes(&raw[..nul_pos]).to_os_string();
            cursor += 8;
        }
        if common_returned & ATTR_CMN_OBJTYPE != 0 {
            anyhow::ensure!(cursor + 4 <= entry_end, "objtype out of entry bounds");
            objtype = read_u32(buf, cursor)?;
            cursor += 4;
        }
        if common_returned & ATTR_CMN_MODTIME != 0 {
            anyhow::ensure!(cursor + 16 <= entry_end, "modtime out of entry bounds");
            let tv_sec = read_i64(buf, cursor)?;
            let tv_nsec = read_i64(buf, cursor + 8)?;
            anyhow::ensure!(
                tv_sec >= 0 && tv_nsec >= 0,
                "negative modtime timespec ({tv_sec}, {tv_nsec})"
            );
            mtime_ns = (tv_sec as u128) * 1_000_000_000 + (tv_nsec as u128);
            cursor += 16;
        }
        if fileattr_returned & ATTR_FILE_DATALENGTH != 0 {
            anyhow::ensure!(cursor + 8 <= entry_end, "datalength out of entry bounds");
            let raw = read_i64(buf, cursor)?;
            anyhow::ensure!(raw >= 0, "negative file size {raw}");
            size = raw as u64;
            cursor += 8;
        }
        let _ = cursor;

        anyhow::ensure!(!name.is_empty(), "entry at offset {entry_start} has no name");
        out.push(BulkEntry { name, objtype, mtime_ns, size });
        offset = entry_end;
    }
    Ok(())
}

/// Enumerate one directory via getattrlistbulk, splitting children into
/// regular files and subdirectory names in a single pass over the buffer.
/// Getting subdirectory names out of the same syscall that returns file
/// attributes is what keeps the whole walk single-pass.
fn bulk_scan_dir(dir: &Path) -> Result<DirScan, ScanError> {
    let cpath = CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| ScanError::Soft(format!("{}: {e}", dir.display())))?;

    // Safety: cpath is a valid NUL-terminated C string owned by this stack
    // frame for the duration of the call; open() only reads through the
    // pointer, and O_DIRECTORY makes the kernel reject non-directories
    // rather than us having to check that ourselves.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(ScanError::Soft(format!("{}: {err}", dir.display())));
    }

    let mut attrlist = make_attrlist();
    let mut buf = vec![0u8; BULK_BUF_SIZE];
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    let mut parsed = Vec::new();

    let outcome: Result<(), ScanError> = (|| {
        loop {
            // Safety: fd is a valid, open, O_DIRECTORY file descriptor from
            // the open() call above and is not used by any other thread;
            // `attrlist` describes the fixed-size request struct kernel
            // reads from; `buf` is a single live allocation of `buf.len()`
            // bytes and the kernel writes at most attrbufsize bytes into
            // it, never more.
            let count = unsafe {
                getattrlistbulk(
                    fd,
                    &mut attrlist as *mut AttrList,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    FSOPT_NOFOLLOW,
                )
            };
            if count == 0 {
                break;
            }
            if count < 0 {
                let err = std::io::Error::last_os_error();
                return Err(ScanError::Soft(format!("{}: {err}", dir.display())));
            }
            parsed.clear();
            parse_bulk_buffer(&buf, count, &mut parsed).map_err(|e| {
                ScanError::Fatal(e.context(format!(
                    "{}: malformed getattrlistbulk entry",
                    dir.display()
                )))
            })?;
            for entry in parsed.drain(..) {
                match entry.objtype {
                    VREG => files.push(entry),
                    VDIR => subdirs.push(entry.name),
                    _ => {} // symlinks, sockets, fifos, etc: dropped, matching sweep_walker
                }
            }
        }
        Ok(())
    })();

    // Safety: fd was returned by the open() call above, is not shared with
    // any other thread, and is closed exactly once here regardless of how
    // the loop above exited.
    unsafe {
        libc::close(fd);
    }

    outcome.map(|()| DirScan { files, subdirs })
}

/// True when `name` is a path component that must never be descended into
/// or emitted, at any depth, regardless of gitignore rules and regardless
/// of the hidden flag: `.git` and `.glep`. Mirrors
/// `walk::is_hard_excluded_component`; kept as a separate copy here since
/// `walk.rs`'s version takes an `&OsStr` built from a real filesystem path
/// rather than a bulk-scan `BulkEntry::name`, and comparing `OsStr`
/// directly (rather than through a lossy `String` conversion) is what lets
/// this stay correct for non-UTF-8 names too.
fn is_hard_excluded(name: &OsStr) -> bool {
    name_eq(name, ".git") || name_eq(name, ".glep")
}

fn name_eq(name: &OsStr, s: &str) -> bool {
    name == OsStr::new(s)
}

/// Combined verdict from a stack of per-directory-level gitignore matchers.
/// Mirrors the precedence the `ignore` crate uses in its own walker: a
/// decisive match (Ignore or Whitelist) from a deeper directory's
/// gitignore overrides one from a shallower directory. Hidden (dot-prefixed)
/// entries are no longer default-ignored here: `sweep`/`sweep_bulk` now
/// include them, each carrying a `FileMeta::hidden` flag for query-time
/// filtering instead. `.git` and `.glep` are the one hard exclusion that
/// bypasses the gitignore stack entirely, checked first.
fn is_ignored(stack: &[Arc<Gitignore>], abs_path: &Path, is_dir: bool, name: &OsStr) -> bool {
    if is_hard_excluded(name) {
        return true;
    }
    let mut verdict = ignore::Match::None;
    for gi in stack {
        match gi.matched(abs_path, is_dir) {
            ignore::Match::None => {}
            m => verdict = m,
        }
    }
    match verdict {
        ignore::Match::Ignore(_) => true,
        ignore::Match::Whitelist(_) => false,
        ignore::Match::None => false,
    }
}

/// Bound on how many levels above the sweep root to look for ancestor
/// `.gitignore` files when no `.git` directory brackets the search. The
/// real `ignore` crate (via `parents(true)`) walks all the way to the
/// filesystem root regardless of git; capping here trades a little fidelity
/// for a sweep root outside any repo (e.g. a plain checkout-less directory)
/// for not doing an unbounded stat() climb on every single sweep. A sweep
/// root inside a real repo isn't affected: the walk stops at the git root.
const ANCESTOR_GITIGNORE_CAP: usize = 10;

/// Walk up from `start` (inclusive) looking for the nearest ancestor
/// directory containing a `.git` entry, directory or gitlink file. `start`
/// should already be canonicalized so `.parent()` walks real directories.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").symlink_metadata().is_ok() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Follow a `.git` gitdir-pointer file (a worktree's `gitdir: <path>`
/// marker) to the real git directory, then follow *that* directory's own
/// `commondir` file if present, since `info/exclude` lives in the common
/// dir, not necessarily the per-worktree one. Mirrors `resolve_git_commondir`
/// in the `ignore` crate itself, including what that crate does when the
/// `commondir` file can't be resolved: it does NOT assume the per-worktree
/// git dir doubles as the common dir, it gives up and the caller falls back
/// to an EMPTY exclude matcher. `Ok(None)` here reproduces that give-up
/// case, so `load_git_exclude` below loads nothing rather than guessing.
/// Only a failure to read/parse the *outer* gitdir-pointer file itself
/// comes back as `Err`, per the "never approximate, fall back to the
/// walker instead" rule for that indirection (see module docs).
fn resolve_gitdir_file(dot_git: &Path) -> anyhow::Result<Option<PathBuf>> {
    let contents = std::fs::read_to_string(dot_git)
        .map_err(|e| anyhow::anyhow!("{}: {e}", dot_git.display()))?;
    let first_line = contents.lines().next().unwrap_or("");
    let raw = first_line.strip_prefix("gitdir: ").ok_or_else(|| {
        anyhow::anyhow!("{}: unrecognized gitdir pointer format", dot_git.display())
    })?;
    let parent = dot_git.parent().unwrap_or_else(|| Path::new("."));
    let real_git_dir = parent.join(raw.trim());

    let commondir_file = real_git_dir.join("commondir");
    let commondir_contents = match std::fs::read_to_string(&commondir_file) {
        Ok(c) => c,
        // No commondir file (or unreadable): matches resolve_git_commondir
        // in the ignore crate, which gives up here rather than treating
        // real_git_dir as the common dir. An orphaned/unlinked worktree's
        // gitdir has no relation to wherever the real common dir's
        // info/exclude actually lives, so guessing it is real_git_dir would
        // be wrong, not just imprecise.
        Err(_) => return Ok(None),
    };
    let line = commondir_contents.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        // Same give-up-to-empty-matcher outcome as a missing file: an
        // empty/blank commondir is unresolvable, not a reason to abort the
        // whole bulk sweep.
        return Ok(None);
    }
    let commondir_path = Path::new(line);
    Ok(Some(if commondir_path.is_relative() {
        real_git_dir.join(commondir_path)
    } else {
        commondir_path.to_path_buf()
    }))
}

/// Load `<gitroot>/.git/info/exclude` as a `Gitignore` rooted at `gitroot`,
/// or `None` when there's no `.git` there or no `info/exclude` inside it.
/// I/O errors reading or parsing the exclude file itself are non-fatal (a
/// stderr warning, treated as absent), matching how the root/nested
/// `.gitignore` loader in `scan_recursive` handles the same class of error.
/// Only the gitdir-pointer indirection (see `resolve_gitdir_file`) is
/// treated as fatal.
fn load_git_exclude(gitroot: &Path) -> anyhow::Result<Option<Gitignore>> {
    let dot_git = gitroot.join(".git");
    let meta = match std::fs::symlink_metadata(&dot_git) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };

    let git_common_dir = if meta.is_dir() {
        dot_git
    } else if meta.is_file() {
        match resolve_gitdir_file(&dot_git)? {
            Some(dir) => dir,
            // commondir file missing/unreadable/empty: the ignore crate
            // gives up and uses an empty exclude matcher rather than
            // guessing, see resolve_gitdir_file's doc comment. Match that
            // by loading nothing instead of falling back to the
            // per-worktree git dir's own (nonexistent) info/exclude.
            None => return Ok(None),
        }
    } else {
        anyhow::bail!("{}: .git is neither a directory nor a file", dot_git.display());
    };

    let exclude_path = git_common_dir.join("info/exclude");
    if !exclude_path.is_file() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(gitroot);
    if let Some(err) = builder.add(&exclude_path) {
        eprintln!("glep: {}: {err}", exclude_path.display());
        return Ok(None);
    }
    match builder.build() {
        Ok(gi) => Ok(Some(gi)),
        Err(err) => {
            eprintln!("glep: {}: {err}", exclude_path.display());
            Ok(None)
        }
    }
}

/// Precedence-ordered matcher stack seeded from every gitignore source that
/// applies above the sweep root, lowest precedence first: global excludes,
/// then `.git/info/exclude`, then ancestor `.gitignore` files from the git
/// root (or a bounded cap when there is none, see `ANCESTOR_GITIGNORE_CAP`)
/// down to the sweep root's parent. `is_ignored` keeps the last decisive
/// verdict as it walks a matcher stack, so this order makes a more specific
/// ancestor's rule beat a less specific one, matching the precedence
/// `ignore` itself uses. The sweep root's own and nested `.gitignore` files
/// are layered on top of whatever this returns, by `scan_recursive` as it
/// descends.
///
/// Reading any of these sources is best-effort and non-fatal EXCEPT
/// resolving a worktree's `.git` gitdir-pointer file, which surfaces as
/// `Err` straight out of this function (and therefore out of `sweep_bulk`,
/// forcing the walker fallback) if it can't be resolved outright.
fn build_ancestor_stack(sweep_root: &Path) -> anyhow::Result<Vec<Arc<Gitignore>>> {
    let mut stack = Vec::new();

    // Global excludes: resolves core.excludesfile / $XDG_CONFIG_HOME/git/ignore
    // / ~/.config/git/ignore exactly like `ignore::WalkBuilder` does by
    // default (relative to the process's current_dir, since `sweep_walker`
    // never calls `.current_dir()` either, so this matches it exactly).
    let (global, err) = Gitignore::global();
    if let Some(err) = err {
        eprintln!("glep: global gitignore: {err}");
    }
    if !global.is_empty() {
        stack.push(Arc::new(global));
    }

    let canon_root = sweep_root.canonicalize().unwrap_or_else(|_| sweep_root.to_path_buf());
    let git_root = find_git_root(&canon_root);

    if let Some(ref gitroot) = git_root {
        if let Some(gi) = load_git_exclude(gitroot)? {
            stack.push(Arc::new(gi));
        }
    }

    // Ancestor .gitignore files, collected child-to-root then reversed so
    // they get pushed outermost first (lowest precedence among themselves).
    let mut ancestors: Vec<PathBuf> = Vec::new();
    if git_root.as_deref() != Some(canon_root.as_path()) {
        let mut cur = canon_root.parent();
        let mut depth = 0usize;
        while let Some(dir) = cur {
            ancestors.push(dir.to_path_buf());
            let hit_git_root = git_root.as_deref() == Some(dir);
            depth += 1;
            if hit_git_root {
                break;
            }
            if git_root.is_none() && depth >= ANCESTOR_GITIGNORE_CAP {
                break;
            }
            cur = dir.parent();
        }
    }
    ancestors.reverse();

    for dir in ancestors {
        let gi_path = dir.join(".gitignore");
        if !gi_path.is_file() {
            continue;
        }
        let mut builder = GitignoreBuilder::new(&dir);
        if let Some(err) = builder.add(&gi_path) {
            eprintln!("glep: {}: {err}", gi_path.display());
            continue;
        }
        match builder.build() {
            Ok(gi) => stack.push(Arc::new(gi)),
            Err(err) => eprintln!("glep: {}: {err}", gi_path.display()),
        }
    }

    Ok(stack)
}

fn scan_recursive<'scope>(
    scope: &rayon::Scope<'scope>,
    root: &'scope Path,
    dir: PathBuf,
    matchers: Vec<Arc<Gitignore>>,
    buffers: &'scope [Mutex<Vec<FileMeta>>],
    fatal: &'scope Mutex<Option<anyhow::Error>>,
) {
    let scan = match bulk_scan_dir(&dir) {
        Ok(s) => s,
        Err(ScanError::Soft(msg)) => {
            eprintln!("glep: {msg}");
            return;
        }
        Err(ScanError::Fatal(e)) => {
            let mut slot = fatal.lock().unwrap();
            if slot.is_none() {
                *slot = Some(e);
            }
            return;
        }
    };

    // Single pass over this directory's files to find both the local
    // .gitignore (existing behavior) and any .ignore/.rgignore (the
    // divergence trap from the module docs: their precedence relative to
    // .gitignore isn't reimplemented here, so their presence anywhere sends
    // the whole sweep back to the walker instead of risking a wrong match).
    let mut has_local_gitignore = false;
    let mut has_divergent_ignore_file = false;
    for f in &scan.files {
        if name_eq(&f.name, ".gitignore") {
            has_local_gitignore = true;
        } else if name_eq(&f.name, ".ignore") || name_eq(&f.name, ".rgignore") {
            has_divergent_ignore_file = true;
        }
    }

    if has_divergent_ignore_file {
        let mut slot = fatal.lock().unwrap();
        if slot.is_none() {
            *slot = Some(anyhow::anyhow!(
                "{}: contains .ignore/.rgignore, deferring to the walker",
                dir.display()
            ));
        }
        return;
    }

    let mut stack = matchers;
    if has_local_gitignore {
        let mut builder = GitignoreBuilder::new(&dir);
        if let Some(err) = builder.add(dir.join(".gitignore")) {
            eprintln!("glep: {}: {err}", dir.join(".gitignore").display());
        }
        match builder.build() {
            Ok(gi) => stack.push(Arc::new(gi)),
            Err(err) => eprintln!("glep: {}: {err}", dir.join(".gitignore").display()),
        }
    }

    let slot_idx = rayon::current_thread_index().unwrap_or(0) % buffers.len().max(1);
    let mut local_files = Vec::with_capacity(scan.files.len());
    for entry in &scan.files {
        let abs = dir.join(&entry.name);
        if is_ignored(&stack, &abs, false, &entry.name) {
            continue;
        }
        let rel = abs.strip_prefix(root).unwrap_or(&abs).to_path_buf();
        let hidden = crate::walk::path_is_hidden(&rel);
        local_files.push(FileMeta {
            path: rel,
            mtime_ns: entry.mtime_ns,
            size: entry.size,
            hidden,
        });
    }
    if !local_files.is_empty() {
        buffers[slot_idx].lock().unwrap().extend(local_files);
    }

    // .git and .glep are never descended into, hidden or not, gitignored
    // or not: is_ignored's hard-exclusion check (ahead of the gitignore
    // stack) covers that here.
    for name in scan.subdirs {
        let abs = dir.join(&name);
        if is_ignored(&stack, &abs, true, &name) {
            continue;
        }
        let child_matchers = stack.clone(); // Vec<Arc<Gitignore>>: cheap, just Arc pointer copies
        scope.spawn(move |scope| {
            scan_recursive(scope, root, abs, child_matchers, buffers, fatal);
        });
    }
}

/// getattrlistbulk-based sweep. Same contract as `walk::sweep`: relative
/// paths, sorted by path, identical missing-root error text, stderr
/// warnings on per-directory errors that don't abort the whole sweep.
///
/// Single-pass: worker threads (rayon's scheduler) pull directories off a
/// shared work queue, each directory is enumerated exactly once, discovered
/// subdirectories are pushed back onto the queue, and files accumulate in
/// one buffer per worker thread that gets merged into the final sorted
/// `Vec` only once, at the end.
///
/// Before the scan starts, `build_ancestor_stack` seeds the matcher stack
/// with every gitignore source above the sweep root (global excludes,
/// `.git/info/exclude`, ancestor `.gitignore` files); an `Err` from that
/// seeding step (currently only an unresolvable worktree gitdir pointer)
/// aborts before any directory is touched and propagates out of
/// `sweep_bulk` like any other fatal error, sending the whole sweep to the
/// walker fallback.
pub fn sweep_bulk(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );

    let seed_matchers = build_ancestor_stack(root)?;

    let num_slots = rayon::current_num_threads().max(1);
    let buffers: Vec<Mutex<Vec<FileMeta>>> =
        (0..num_slots).map(|_| Mutex::new(Vec::new())).collect();
    let fatal: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    rayon::scope(|scope| {
        scan_recursive(scope, root, root.to_path_buf(), seed_matchers, &buffers, &fatal);
    });

    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }

    let mut all: Vec<FileMeta> = Vec::new();
    for buf in buffers {
        all.extend(buf.into_inner().unwrap());
    }
    all.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(all)
}
