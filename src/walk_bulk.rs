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
//! and `.ignore` and `.rgignore` files (plus their whitelist overrides).
//! `build_ancestor_stack` seeds the matcher stack with the first three of
//! those before the walk starts (see its doc comment for precedence
//! order), and `build_ancestor_dot_ignores` loads the `.ignore` and
//! `.rgignore` files above the root, which outrank every gitignore source
//! (see `verdict`); an in-tree `.ignore`/`.rgignore` is handled by
//! refusing to walk at all, see below.
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

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::dircache::DirNode;
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
    fn getattrlist(
        path: *const libc::c_char,
        alist: *mut AttrList,
        attrbuf: *mut libc::c_void,
        attrbufsize: libc::size_t,
        options: libc::c_ulong,
    ) -> libc::c_int;
}

fn make_attrlist() -> AttrList {
    AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_MODTIME,
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
    subdirs: Vec<Subdir>,
}

/// A discovered subdirectory: name plus the mtime observed in the PARENT's
/// listing. That mtime is the pruning signal for the dir-mtime listing
/// cache (see dircache.rs): if it matches the cached value, the child's
/// whole subtree is taken from cache without a single syscall below it.
struct Subdir {
    name: OsString,
    mtime_ns: u128,
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

        anyhow::ensure!(
            !name.is_empty(),
            "entry at offset {entry_start} has no name"
        );
        out.push(BulkEntry {
            name,
            objtype,
            mtime_ns,
            size,
        });
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
    // Per-thread reusable scan buffer: getattrlistbulk overwrites the
    // region it reports, so a zeroed fresh allocation per directory is
    // pure waste (kernel-scale walks memset ~70MB for nothing).
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> =
            std::cell::RefCell::new(vec![0u8; BULK_BUF_SIZE]);
    }
    let mut buf_storage = BUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
    if buf_storage.len() != BULK_BUF_SIZE {
        buf_storage = vec![0u8; BULK_BUF_SIZE];
    }
    let buf: &mut Vec<u8> = &mut buf_storage;
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
            parse_bulk_buffer(buf, count, &mut parsed).map_err(|e| {
                ScanError::Fatal(e.context(format!(
                    "{}: malformed getattrlistbulk entry",
                    dir.display()
                )))
            })?;
            for entry in parsed.drain(..) {
                match entry.objtype {
                    VREG => files.push(entry),
                    VDIR => subdirs.push(Subdir {
                        name: entry.name,
                        mtime_ns: entry.mtime_ns,
                    }),
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

    // Return the scan buffer to this thread's pool (it was taken out
    // above; putting it back on every path keeps the pool warm).
    BUF.with(|b| {
        *b.borrow_mut() = buf_storage;
    });

    outcome.map(|()| DirScan { files, subdirs })
}

/// What a sweep reads for each directory entry.
#[derive(Clone, Copy)]
enum ScanAttrs {
    /// Name, type, mtime and size, from getattrlistbulk (`bulk_scan_dir`):
    /// the index's freshness diff compares mtimes and sizes.
    Full,
    /// Name and type only, from readdir (`names_scan_dir`): a listing
    /// never looks at the rest.
    Names,
}

fn scan_dir(dir: &Path, attrs: ScanAttrs) -> Result<DirScan, ScanError> {
    match attrs {
        ScanAttrs::Full => bulk_scan_dir(dir),
        ScanAttrs::Names => names_scan_dir(dir),
    }
}

/// `bulk_scan_dir` for a listing, which needs names and types only:
/// readdir(3) reads both from the directory's own records, where
/// getattrlistbulk also fetches every entry's mtime and size. An entry
/// readdir cannot type (DT_UNKNOWN) is typed by fstatat(2) without
/// following a symlink; symlinks and other non-file, non-directory
/// entries are dropped, as in `bulk_scan_dir`. Every mtime and size is 0.
/// Errors are reported in `bulk_scan_dir`'s words.
fn names_scan_dir(dir: &Path) -> Result<DirScan, ScanError> {
    let soft = |err: std::io::Error| ScanError::Soft(format!("{}: {err}", dir.display()));
    let cpath = CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| ScanError::Soft(format!("{}: {e}", dir.display())))?;
    // Safety: cpath is a NUL-terminated string that outlives the call;
    // O_DIRECTORY makes the kernel reject anything but a directory.
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(soft(std::io::Error::last_os_error()));
    }
    // Safety: fd is an open directory descriptor owned here; on success
    // the stream owns it and closedir below closes both.
    let dirp = unsafe { libc::fdopendir(fd) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        // Safety: fdopendir failed, so fd is still ours to close.
        unsafe {
            libc::close(fd);
        }
        return Err(soft(err));
    }
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    let outcome = loop {
        // readdir returns NULL at the end and on an error alike; only
        // errno tells them apart.
        // Safety: __error is this thread's errno slot.
        unsafe {
            *libc::__error() = 0;
        }
        // Safety: dirp is the open stream from fdopendir above, used by
        // this thread only.
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            // Safety: as above.
            let errno = unsafe { *libc::__error() };
            break if errno == 0 {
                Ok(())
            } else {
                Err(soft(std::io::Error::from_raw_os_error(errno)))
            };
        }
        // Safety: a non-NULL readdir result points at a dirent that stays
        // valid until the next readdir or closedir on this stream; d_namlen
        // counts the name's bytes, inside d_name.
        let ent = unsafe { &*ent };
        let name = unsafe {
            std::slice::from_raw_parts(ent.d_name.as_ptr() as *const u8, ent.d_namlen as usize)
        };
        if name == b"." || name == b".." {
            continue;
        }
        let objtype = match ent.d_type {
            libc::DT_REG => VREG,
            libc::DT_DIR => VDIR,
            libc::DT_UNKNOWN => {
                // Safety: stat is plain data, and fstatat writes it whole
                // on success; d_name is NUL-terminated and names an entry
                // of the open directory dirfd(dirp).
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    libc::fstatat(
                        libc::dirfd(dirp),
                        ent.d_name.as_ptr(),
                        &mut st,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if rc != 0 {
                    // Gone since the listing.
                    continue;
                }
                match st.st_mode & libc::S_IFMT {
                    libc::S_IFREG => VREG,
                    libc::S_IFDIR => VDIR,
                    _ => continue,
                }
            }
            // Symlinks, sockets, fifos and devices: dropped, as in
            // bulk_scan_dir.
            _ => continue,
        };
        let name = OsStr::from_bytes(name).to_os_string();
        if objtype == VREG {
            files.push(BulkEntry {
                name,
                objtype,
                mtime_ns: 0,
                size: 0,
            });
        } else {
            subdirs.push(Subdir { name, mtime_ns: 0 });
        }
    };
    // Safety: dirp came from fdopendir above and is closed exactly once,
    // whichever way the loop ended; it closes fd with it.
    unsafe {
        libc::closedir(dirp);
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
fn is_ignored(
    dot: &[Arc<Gitignore>],
    stack: &[Arc<Gitignore>],
    abs_path: &Path,
    is_dir: bool,
    name: &OsStr,
) -> bool {
    if is_hard_excluded(name) {
        return true;
    }
    matches!(
        verdict(dot, stack, abs_path, is_dir),
        ignore::Match::Ignore(_)
    )
}

/// The ignore crate's precedence across sources: `.rgignore` and
/// `.ignore` rules outrank every gitignore source wherever they sit, so
/// the files above the sweep root (`dot`; an in-tree one sends the sweep
/// to the walker instead) decide first, and the gitignore stack only when
/// they have no opinion. `dot` holds the `.rgignore` files after the
/// `.ignore` files, so any `.rgignore` opinion beats any `.ignore` one;
/// within each group the deepest file decides.
fn verdict<'a>(
    dot: &'a [Arc<Gitignore>],
    stack: &'a [Arc<Gitignore>],
    abs_path: &Path,
    is_dir: bool,
) -> ignore::Match<&'a ignore::gitignore::Glob> {
    let d = stack_verdict(dot, abs_path, is_dir);
    if !d.is_none() {
        return d;
    }
    stack_verdict(stack, abs_path, is_dir)
}

/// The stack's combined verdict for `abs_path`: deepest non-None wins
/// (matchers were pushed shallowest to deepest). Returns the raw Match so
/// the caller can distinguish Whitelist (rescues hidden-ness) from a
/// plain no-match.
fn stack_verdict<'a>(
    stack: &'a [Arc<Gitignore>],
    abs_path: &Path,
    is_dir: bool,
) -> ignore::Match<&'a ignore::gitignore::Glob> {
    let mut verdict = ignore::Match::None;
    for gi in stack {
        match gi.matched(abs_path, is_dir) {
            ignore::Match::None => {}
            m => verdict = m,
        }
    }
    verdict
}

/// Effective hiddenness for a yielded file, mirroring the walker check:
/// hidden iff some dot-prefixed prefix of `rel` did not earn a whitelist
/// verdict from the ignore stack. A whitelisted `.github/` dir makes its
/// children ordinary files; `.github/.env` still needs a rescue of its
/// own. `rel` is the root-relative path. Non-dot paths short-circuit
/// through the component scan without a matcher call.
fn still_hidden(dot: &[Arc<Gitignore>], stack: &[Arc<Gitignore>], root: &Path, rel: &Path) -> bool {
    // Ordinary paths (no dot-prefixed component at all, nearly every file)
    // cost one component scan, exactly what the sweep paid before
    // whitelist rescue existed.
    if !crate::walk::path_is_hidden(rel) {
        return false;
    }
    let mut p = rel;
    loop {
        if p.file_name()
            .is_some_and(|n| n.as_encoded_bytes().first() == Some(&b'.'))
        {
            let is_dir = p != rel;
            if !verdict(dot, stack, &root.join(p), is_dir).is_whitelist() {
                return true;
            }
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => return false,
        }
    }
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
        anyhow::bail!(
            "{}: .git is neither a directory nor a file",
            dot_git.display()
        );
    };

    let exclude_path = git_common_dir.join("info/exclude");
    if !exclude_path.is_file() {
        return Ok(None);
    }
    let mut builder = GitignoreBuilder::new(gitroot);
    if let Some(err) = builder.add(&exclude_path) {
        crate::messages::ignore_warning(format!("{}: {err}", exclude_path.display()));
        return Ok(None);
    }
    match builder.build() {
        Ok(gi) => Ok(Some(gi)),
        Err(err) => {
            crate::messages::ignore_warning(format!("{}: {err}", exclude_path.display()));
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
        crate::messages::ignore_warning(format!("global gitignore: {err}"));
    }
    if !global.is_empty() {
        stack.push(Arc::new(global));
    }

    let canon_root = sweep_root
        .canonicalize()
        .unwrap_or_else(|_| sweep_root.to_path_buf());
    let git_root = crate::walk::find_git_root(&canon_root);

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
            if git_root.is_none() && depth >= crate::walk::ANCESTOR_GITIGNORE_CAP {
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
            crate::messages::ignore_warning(format!("{}: {err}", gi_path.display()));
            continue;
        }
        match builder.build() {
            Ok(gi) => stack.push(Arc::new(gi)),
            Err(err) => crate::messages::ignore_warning(format!("{}: {err}", gi_path.display())),
        }
    }

    Ok(stack)
}

/// `.ignore` and `.rgignore` files in every directory above the sweep
/// root: every `.ignore` shallowest first, then every `.rgignore`
/// shallowest first, so `stack_verdict` (the last opinion wins) ranks a
/// `.rgignore` above any `.ignore` and a deeper file above a shallower
/// one, as rg does. The portable walker applies them through
/// `parents(true)`, all the way up and ahead of every gitignore source,
/// so the bulk walker does too (see `verdict`) instead of handing such
/// sweeps to the walker. An empty file contributes nothing.
fn build_ancestor_dot_ignores(sweep_root: &Path) -> Vec<Arc<Gitignore>> {
    let canon = sweep_root
        .canonicalize()
        .unwrap_or_else(|_| sweep_root.to_path_buf());
    let mut dirs: Vec<&Path> = canon.ancestors().skip(1).collect();
    dirs.reverse();
    let mut out = Vec::new();
    for name in [".ignore", crate::walk::RGIGNORE] {
        for dir in &dirs {
            let file = dir.join(name);
            if !file.is_file() {
                continue;
            }
            let mut builder = GitignoreBuilder::new(dir);
            if let Some(err) = builder.add(&file) {
                // Unreadable lines are reported; the readable ones still
                // apply, as in the walker.
                crate::messages::ignore_warning(format!("{}: {err}", file.display()));
            }
            match builder.build() {
                Ok(gi) if !gi.is_empty() => out.push(Arc::new(gi)),
                Ok(_) => {}
                Err(err) => crate::messages::ignore_warning(format!("{}: {err}", file.display())),
            }
        }
    }
    out
}

/// True when `rel` (root-relative) lies inside one of the scope prefixes.
fn in_scope(rel: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes.iter().any(|p| rel.starts_with(p))
}

/// True when the directory `rel` must be descended for a scoped sweep:
/// it is inside a prefix, or it is an ancestor of one.
fn dir_in_scope(rel: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes
        .iter()
        .any(|p| rel.starts_with(p) || p.starts_with(rel))
}

/// A directory's own `.gitignore` as a matcher rooted at the directory.
/// A rule that fails to parse, or a file that fails to read, is reported
/// as an ignore warning and the readable rules still apply, as in the
/// walker.
fn load_dir_gitignore(dir: &Path) -> Option<Arc<Gitignore>> {
    let path = dir.join(".gitignore");
    let mut builder = GitignoreBuilder::new(dir);
    if let Some(err) = builder.add(&path) {
        crate::messages::ignore_warning(format!("{}: {err}", path.display()));
    }
    match builder.build() {
        Ok(gi) => Some(Arc::new(gi)),
        Err(err) => {
            crate::messages::ignore_warning(format!("{}: {err}", path.display()));
            None
        }
    }
}

/// One directory entry looked up by name instead of listed: what a bulk
/// scan of its parent would report for it.
enum Lookup {
    /// The entry, stored under exactly the name looked up.
    Found(BulkEntry),
    /// No entry by that name.
    Absent,
    /// A lookup cannot settle it: an error other than "no such entry", or
    /// an entry stored under another spelling (a case- or
    /// normalization-insensitive volume finds `Src` for `src`, while a
    /// listing reports only the stored name).
    Unsure,
}

/// Look up `abs`, whose last component is `name`, with getattrlist(2):
/// the attributes a bulk scan of its parent returns, for the entry itself
/// (a symlink is not followed). The reply has a bulk entry's layout, so
/// `parse_bulk_buffer` reads it.
fn lookup_entry(abs: &Path, name: &OsStr) -> Lookup {
    let Ok(cpath) = CString::new(abs.as_os_str().as_bytes()) else {
        return Lookup::Unsure;
    };
    let mut attrlist = make_attrlist();
    // One reply: the fixed attributes plus a name of at most 255 UTF-8
    // characters, in a 4-byte aligned buffer.
    let mut buf = [0u32; 1024];
    // Safety: cpath is a NUL-terminated string that outlives the call;
    // attrlist is the request struct the kernel reads; buf is a live
    // allocation of size_of_val(&buf) bytes and the kernel writes at most
    // that many into it.
    let rc = unsafe {
        getattrlist(
            cpath.as_ptr(),
            &mut attrlist as *mut AttrList,
            buf.as_mut_ptr() as *mut libc::c_void,
            std::mem::size_of_val(&buf),
            FSOPT_NOFOLLOW as libc::c_ulong,
        )
    };
    if rc != 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => Lookup::Absent,
            _ => Lookup::Unsure,
        };
    }
    // Safety: buf is initialized u32s, and any byte view of them is valid.
    let bytes = unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, std::mem::size_of_val(&buf))
    };
    let mut parsed = Vec::with_capacity(1);
    if parse_bulk_buffer(bytes, 1, &mut parsed).is_err() {
        return Lookup::Unsure;
    }
    match parsed.pop() {
        Some(e) if e.name == name => Lookup::Found(e),
        _ => Lookup::Unsure,
    }
}

/// What a directory on the way down to a scope prefix contributes.
enum Level {
    /// Its own `.gitignore` matcher, if it has one.
    Rules(Option<Arc<Gitignore>>),
    /// One of its ignore files could not be settled by a lookup.
    Unsure,
}

/// Read a directory on the way from the root to a scope prefix by name:
/// the walk from the root lists it only to find its ignore files and the
/// next level down. Err when it holds an `.ignore` or `.rgignore` (a
/// regular file, as a listing counts it), which defers the sweep to the
/// walker exactly as listing it would.
fn level_rules(dir: &Path) -> anyhow::Result<Level> {
    for name in [".ignore", crate::walk::RGIGNORE] {
        match lookup_entry(&dir.join(name), OsStr::new(name)) {
            Lookup::Found(e) if e.objtype == VREG => anyhow::bail!(
                "{}: contains .ignore/.rgignore, deferring to the walker",
                dir.display()
            ),
            Lookup::Found(_) | Lookup::Absent => {}
            Lookup::Unsure => return Ok(Level::Unsure),
        }
    }
    Ok(
        match lookup_entry(&dir.join(".gitignore"), OsStr::new(".gitignore")) {
            Lookup::Found(e) if e.objtype == VREG => Level::Rules(load_dir_gitignore(dir)),
            Lookup::Found(_) | Lookup::Absent => Level::Rules(None),
            Lookup::Unsure => Level::Unsure,
        },
    )
}

/// Where a scoped sweep starts below the root.
enum ScopedStart {
    /// A prefix naming a directory, with the matcher stack the walk from
    /// the root holds when it reaches it.
    Dir {
        prefix: PathBuf,
        abs: PathBuf,
        stack: Vec<Arc<Gitignore>>,
    },
    /// A prefix naming a regular file, as the walk from the root yields it.
    File(FileMeta),
}

/// The starting points of a scoped sweep, one per prefix that lies in no
/// other prefix. The walk from the root lists each directory on the way
/// to a prefix only to load its ignore files and to find the next level,
/// so both are read by name instead (`level_rules`, `lookup_entry`): the
/// next level must be stored under exactly its name, be a directory (or,
/// last, a regular file) and not be ignored by the stack built so far,
/// which is the stack the walk from the root carries there. A level that
/// walk would not enter (absent, ignored, a symlink, `.git`) leaves the
/// prefix with no files, as it does there. Err defers the sweep to the
/// walker (an `.ignore` or `.rgignore` on the way); Ok(None) means some
/// name could not be settled by a lookup, and the caller walks from the
/// root instead.
fn scoped_starts(
    root: &Path,
    dot: &[Arc<Gitignore>],
    seed: &[Arc<Gitignore>],
    prefixes: &[PathBuf],
) -> anyhow::Result<Option<Vec<ScopedStart>>> {
    if prefixes.iter().any(|p| p.as_os_str().is_empty()) {
        return Ok(None);
    }
    // Path order puts each prefix right before the prefixes inside it,
    // duplicates included; its own walk covers those.
    let mut sorted: Vec<&Path> = prefixes.iter().map(PathBuf::as_path).collect();
    sorted.sort();
    let mut outer: Vec<&Path> = Vec::with_capacity(sorted.len());
    for p in sorted {
        if !outer.last().is_some_and(|o| p.starts_with(o)) {
            outer.push(p);
        }
    }
    // Each level's own .gitignore, loaded once however many prefixes
    // pass through it.
    let mut levels: HashMap<PathBuf, Option<Arc<Gitignore>>> = HashMap::new();
    let mut starts = Vec::with_capacity(outer.len());
    'prefix: for prefix in outer {
        let mut names: Vec<&OsStr> = Vec::new();
        for c in prefix.components() {
            match c {
                Component::Normal(n) => names.push(n),
                // Swept paths are plain names below the root: an absolute,
                // `.` or `..` prefix matches none of them.
                _ => continue 'prefix,
            }
        }
        let mut stack = seed.to_vec();
        let mut dir = root.to_path_buf();
        for (i, &name) in names.iter().enumerate() {
            let own = match levels.get(&dir) {
                Some(own) => own.clone(),
                None => match level_rules(&dir)? {
                    Level::Rules(own) => {
                        levels.insert(dir.clone(), own.clone());
                        own
                    }
                    Level::Unsure => return Ok(None),
                },
            };
            stack.extend(own);
            let abs = dir.join(name);
            let entry = match lookup_entry(&abs, name) {
                Lookup::Found(e) => e,
                Lookup::Absent => continue 'prefix,
                Lookup::Unsure => return Ok(None),
            };
            let last = i + 1 == names.len();
            match entry.objtype {
                VDIR if !is_ignored(dot, &stack, &abs, true, name) => {
                    if last {
                        starts.push(ScopedStart::Dir {
                            prefix: prefix.to_path_buf(),
                            abs,
                            stack,
                        });
                        continue 'prefix;
                    }
                    dir = abs;
                }
                VREG if last && !is_ignored(dot, &stack, &abs, false, name) => {
                    let rel = abs.strip_prefix(root).unwrap_or(&abs).to_path_buf();
                    let hidden = still_hidden(dot, &stack, root, &rel);
                    starts.push(ScopedStart::File(FileMeta {
                        path: rel,
                        mtime_ns: entry.mtime_ns,
                        size: entry.size,
                        hidden,
                    }));
                    continue 'prefix;
                }
                // Ignored, a symlink, a file on the way, or neither file
                // nor directory: the walk from the root yields nothing
                // under it.
                _ => continue 'prefix,
            }
        }
    }
    Ok(Some(starts))
}

#[allow(clippy::too_many_arguments)]
fn scan_recursive<'scope>(
    scope: &rayon::Scope<'scope>,
    root: &'scope Path,
    dir: PathBuf,
    dot: &'scope [Arc<Gitignore>],
    matchers: Vec<Arc<Gitignore>>,
    buffers: &'scope [Mutex<Vec<FileMeta>>],
    fatal: &'scope Mutex<Option<anyhow::Error>>,
    filter: Option<&'scope [PathBuf]>,
    attrs: ScanAttrs,
) {
    let scan = match scan_dir(&dir, attrs) {
        Ok(s) => s,
        Err(ScanError::Soft(msg)) => {
            crate::messages::error(msg);
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
        stack.extend(load_dir_gitignore(&dir));
    }

    let slot_idx = rayon::current_thread_index().unwrap_or(0) % buffers.len().max(1);
    let mut local_files = Vec::with_capacity(scan.files.len());
    for entry in &scan.files {
        let abs = dir.join(&entry.name);
        if is_ignored(dot, &stack, &abs, false, &entry.name) {
            continue;
        }
        let rel = abs.strip_prefix(root).unwrap_or(&abs).to_path_buf();
        if filter.is_some_and(|f| !in_scope(&rel, f)) {
            continue;
        }
        // still_hidden is cheap for ordinary paths (a component scan);
        // only dot-prefixed prefixes pay a matcher lookup.
        let hidden = still_hidden(dot, &stack, root, &rel);
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
    for sub in scan.subdirs {
        let name = sub.name;
        let abs = dir.join(&name);
        if is_ignored(dot, &stack, &abs, true, &name) {
            continue;
        }
        if let Some(f) = filter {
            if !dir_in_scope(abs.strip_prefix(root).unwrap_or(&abs), f) {
                continue;
            }
        }
        let child_matchers = stack.clone(); // Vec<Arc<Gitignore>>: cheap, just Arc pointer copies
        scope.spawn(move |scope| {
            scan_recursive(
                scope,
                root,
                abs,
                dot,
                child_matchers,
                buffers,
                fatal,
                filter,
                attrs,
            );
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
    Ok(sweep_bulk_filtered(root, None, false, ScanAttrs::Full)?.0)
}

/// `sweep_bulk` restricted to `prefixes` (root-relative): exactly the
/// full sweep's subset under the prefixes, so an ignored directory named
/// as a prefix yields nothing (it never enters the index) and every
/// ancestor `.gitignore` between the root and a prefix applies. The walk
/// starts at each prefix, with the matcher stack the walk from the root
/// would carry there (`scoped_starts` reads the levels above by name, and
/// several prefixes walk in parallel); when a level cannot be read that
/// way, it runs from the root, pruned to directories on the way to or
/// inside a prefix and to files inside one. A `.ignore`/`.rgignore` on
/// the way to or inside a prefix defers to the walker like the full sweep
/// does. Unsorted, like `sweep_bulk`.
pub fn sweep_bulk_scoped(root: &Path, prefixes: &[PathBuf]) -> anyhow::Result<Vec<FileMeta>> {
    Ok(sweep_bulk_filtered(root, Some(prefixes), true, ScanAttrs::Full)?.0)
}

/// `sweep_bulk_scoped` for a listing: the same paths and hidden flags,
/// read with readdir (mtimes and sizes are 0, except for a prefix that
/// names a file, which is looked up whole), plus the prefixes whose
/// walk started at the prefix itself, as a directory every level above
/// which the walk from the root enters: the files under such a prefix
/// are exactly the ones returned, none or many.
pub fn sweep_bulk_scoped_names(
    root: &Path,
    prefixes: &[PathBuf],
) -> anyhow::Result<(Vec<FileMeta>, Vec<PathBuf>)> {
    sweep_bulk_filtered(root, Some(prefixes), true, ScanAttrs::Names)
}

/// `sweep_bulk_scoped` walked from the root only: the reference the
/// prefix-start walk is tested against.
#[cfg(test)]
pub fn sweep_bulk_scoped_from_root(
    root: &Path,
    prefixes: &[PathBuf],
) -> anyhow::Result<Vec<FileMeta>> {
    Ok(sweep_bulk_filtered(root, Some(prefixes), false, ScanAttrs::Full)?.0)
}

/// The bulk sweep, scoped by `filter` when given (see `sweep_bulk_scoped`;
/// `from_prefixes` false always walks from the root). Also returns the
/// prefixes whose walk started at the prefix itself, as a directory.
fn sweep_bulk_filtered(
    root: &Path,
    filter: Option<&[PathBuf]>,
    from_prefixes: bool,
    attrs: ScanAttrs,
) -> anyhow::Result<(Vec<FileMeta>, Vec<PathBuf>)> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let seed_matchers = build_ancestor_stack(root)?;
    let dot = build_ancestor_dot_ignores(root);
    let starts = match filter {
        Some(prefixes) if from_prefixes => scoped_starts(root, &dot, &seed_matchers, prefixes)?,
        _ => None,
    };

    let num_slots = rayon::current_num_threads().max(1);
    let buffers: Vec<Mutex<Vec<FileMeta>>> =
        (0..num_slots).map(|_| Mutex::new(Vec::new())).collect();
    let fatal: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    let mut entered = Vec::new();
    let (dot_ref, buffers_ref, fatal_ref) = (&dot[..], &buffers[..], &fatal);
    rayon::scope(|scope| match starts {
        Some(starts) => {
            for start in starts {
                match start {
                    ScopedStart::Dir { prefix, abs, stack } => {
                        entered.push(prefix);
                        scope.spawn(move |scope| {
                            scan_recursive(
                                scope,
                                root,
                                abs,
                                dot_ref,
                                stack,
                                buffers_ref,
                                fatal_ref,
                                None,
                                attrs,
                            );
                        })
                    }
                    ScopedStart::File(meta) => buffers_ref[0].lock().unwrap().push(meta),
                }
            }
        }
        None => scan_recursive(
            scope,
            root,
            root.to_path_buf(),
            dot_ref,
            seed_matchers,
            buffers_ref,
            fatal_ref,
            filter,
            attrs,
        ),
    });

    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }

    let mut all: Vec<FileMeta> = Vec::new();
    for buf in buffers {
        all.extend(buf.into_inner().unwrap());
    }
    // Deliberately unsorted: callers (`walk::sweep`) sort when they need
    // ordered output; the freshness-diff caller does not, and sorting 86k
    // paths costs ~30ms on a kernel-scale tree.
    Ok((all, entered))
}

// ---------------------------------------------------------------------
// Dir-mtime pruned listing walk (see dircache.rs for the design).
//
// Produces the same file listing `sweep_bulk` would, but consults a
// `DirNode` cache: interior directories are listed live (directory mtimes
// never propagate upward, so only a leaf's own mtime certifies its whole
// entry set, see dircache.rs), while each LEAF directory whose mtime
// matches the parent's cached observation is emitted from cache without a
// syscall. Directories are listed in parallel; on heavy churn the
// RESCAN_BUDGET abort hands off to the parallel full sweep, which also
// rebuilds the cache. A tree for the cache is assembled only when some
// directory listed live differs from its cached node.
// ---------------------------------------------------------------------

/// Shared state for the parallel pruned listing walk.
struct ListingCtx<'a> {
    /// Per-worker output buffers (same pattern as `sweep_bulk`'s buffers):
    /// lock-free accumulation, merged once at the end. Entries from pruned
    /// (cache-hit) subtrees are BORROWED from the cached tree, zero
    /// clones for the 86k-file common case; only live-scanned entries are
    /// owned.
    files: Vec<Mutex<Vec<Cow<'a, FileMeta>>>>,
    /// Live-scanned directories so far (root included).
    rescans: AtomicUsize,
    /// Set when rescans exceed `dircache::RESCAN_BUDGET`: workers unwind
    /// and the caller falls back to the parallel full sweep.
    budget_exceeded: AtomicBool,
    /// First fatal error (malformed buffers, .ignore/.rgignore presence):
    /// the listing aborts and the caller falls back to the full sweep,
    /// exactly like `sweep_bulk` does for the same conditions.
    fatal: Mutex<Option<anyhow::Error>>,
    /// Every directory the walk reached: pruned leaves borrowed from the
    /// cached tree, directories listed live as fresh nodes whose subdirs
    /// `assemble_tree` attaches (scoped tasks cannot return values
    /// directly). Assembled only when `dirty`.
    nodes: Mutex<Vec<Cow<'a, DirNode>>>,
    /// Some directory listed live differs from its cached node (or has no
    /// cached node, or could not be listed), so the walked tree differs
    /// from the cached one. While clear the cached tree is still exact,
    /// and is neither reassembled nor rewritten.
    dirty: AtomicBool,
    /// `.ignore` files above the root (see `verdict`), fixed for the walk.
    dot: Vec<Arc<Gitignore>>,
}

/// Result of a completed pruned listing walk.
pub struct ListingOutcome<'a> {
    pub files: Vec<Cow<'a, FileMeta>>,
    /// The walked tree (cached subtrees reused verbatim, rescanned dirs
    /// rebuilt fresh) when it differs from the cached root, to be saved;
    /// None when the cached tree is still exact.
    pub tree: Option<DirNode>,
    pub aborted: bool,
}

/// Pruned listing walk over `root`, seeded with the cached tree. `Err`
/// mirrors the conditions under which `sweep_bulk` itself fails (missing
/// root, unresolvable gitdir pointer, malformed attr buffers): the caller
/// treats it exactly like a bulk-sweep failure and falls back.
pub fn sweep_bulk_listing<'a>(
    root: &Path,
    cached: Option<&'a DirNode>,
) -> anyhow::Result<ListingOutcome<'a>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let stack = build_ancestor_stack(root)?;
    let num_slots = rayon::current_num_threads().max(1);
    let ctx = ListingCtx {
        files: (0..num_slots).map(|_| Mutex::new(Vec::new())).collect(),
        rescans: AtomicUsize::new(0),
        budget_exceeded: AtomicBool::new(false),
        fatal: Mutex::new(None),
        nodes: Mutex::new(Vec::new()),
        dirty: AtomicBool::new(false),
        dot: build_ancestor_dot_ignores(root),
    };
    let timing = std::env::var_os("GLEP_TIMING").is_some();
    let t_scope = std::time::Instant::now();
    rayon::scope(|scope| {
        scan_listing(
            scope,
            root,
            root.to_path_buf(),
            PathBuf::new(),
            stack,
            cached,
            // the root has no in-tree ancestors; ignore sources above it
            // are guarded by the cache fingerprint instead
            true,
            0, // observed mtime: nobody observes the root
            &ctx,
        );
    });
    let t_walked = std::time::Instant::now();
    let ListingCtx {
        files,
        rescans,
        budget_exceeded,
        fatal,
        nodes,
        dirty,
        ..
    } = ctx;
    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }
    if budget_exceeded.into_inner() {
        return Ok(ListingOutcome {
            files: Vec::new(),
            tree: None,
            aborted: true,
        });
    }
    // Merge the per-worker buffers, moving every entry (an owned one too).
    let bufs: Vec<Vec<Cow<'a, FileMeta>>> =
        files.into_iter().map(|b| b.into_inner().unwrap()).collect();
    let mut files = Vec::with_capacity(bufs.iter().map(Vec::len).sum());
    for b in bufs {
        files.extend(b);
    }
    let t_merged = std::time::Instant::now();
    let nodes = nodes.into_inner().unwrap();
    let dirty = dirty.into_inner();
    let tree = if dirty {
        Some(assemble_tree(nodes))
    } else {
        // Every directory listed live matched its cached node, and every
        // other one is a cached leaf: the walked tree is the cached one.
        debug_assert!(
            cached.is_some_and(|c| assemble_tree(nodes) == *c),
            "a clean pruned walk must reproduce the cached tree"
        );
        None
    };
    if timing {
        eprintln!(
            "glep timing: walk scope {:.1}ms, merge {:.1}ms, assemble {:.1}ms (dirty: {dirty})",
            t_walked.duration_since(t_scope).as_secs_f64() * 1000.0,
            t_merged.duration_since(t_walked).as_secs_f64() * 1000.0,
            t_merged.elapsed().as_secs_f64() * 1000.0
        );
    }
    if std::env::var_os("GLEP_DEBUG_PRUNE").is_some() {
        eprintln!(
            "glep debug: listing walk rescanned {} dirs",
            rescans.into_inner()
        );
    }
    Ok(ListingOutcome {
        files,
        tree,
        aborted: false,
    })
}

/// Attach every reached directory to its parent (`rel.parent()`). Nodes
/// are processed deepest-first, each TAKEN OUT of the map and moved into
/// its parent, so a parent always folds in its children's final
/// assembled form; only pruned leaves, borrowed from the cache, are
/// copied. Siblings attach in rel order for deterministic trees, the
/// order `dircache::build_from_listing` uses too.
fn assemble_tree(nodes: Vec<Cow<'_, DirNode>>) -> DirNode {
    use std::cmp::Reverse;
    use std::collections::BTreeMap;
    let mut by_rel: BTreeMap<PathBuf, DirNode> = BTreeMap::new();
    for node in nodes {
        let node = node.into_owned();
        by_rel.insert(node.rel.clone(), node);
    }
    // Deepest rels first (siblings tie-broken by rel ascending): a node's
    // parent is always strictly shallower and therefore still present.
    let mut rels: Vec<PathBuf> = by_rel.keys().cloned().collect();
    rels.sort_by_cached_key(|r| (Reverse(r.components().count()), r.clone()));
    let mut root = DirNode::default_for(Path::new(""));
    for rel in rels {
        let Some(node) = by_rel.remove(&rel) else {
            continue;
        };
        match rel.parent() {
            None => root = node,
            Some(parent) => {
                if let Some(p) = by_rel.get_mut(parent) {
                    p.has_ignore_subtree |= node.has_ignore_subtree;
                    p.subdirs.push(node);
                }
            }
        }
    }
    root
}

/// Whether the cached LEAF `cc`, a subdirectory of a directory listed
/// live with a stable ignore chain (`me_stable` in `scan_listing`), still
/// holds its directory's listing (see dircache.rs). Directory mtimes never
/// propagate upward, so only a leaf's own mtime, as observed in its
/// parent's listing, certifies its whole entry set; a zero is no
/// observation (the root's, one the cache could not trust, or a
/// filesystem that reports none) and never matches. In-place edits of an
/// ignore file change no directory mtime, so a leaf with its own
/// .gitignore, .ignore or .rgignore additionally stat-checks each one
/// (mtime, size; an empty .gitignore included). The root..parent matcher
/// chain is covered by `me_stable`; sources above the root by the
/// fingerprint checked before the walk.
fn leaf_unchanged(cc: &DirNode, observed_mtime_ns: u128, abs: &Path) -> bool {
    if cc.mtime_ns == 0 || cc.mtime_ns != observed_mtime_ns || !cc.subdirs.is_empty() {
        return false;
    }
    // A leaf's has_ignore_subtree is its own .gitignore.
    let gitignore_ok = !cc.has_ignore_subtree
        || crate::dircache::unchanged(
            &abs.join(".gitignore"),
            cc.gitignore_mtime_ns,
            cc.gitignore_size,
        );
    gitignore_ok
        && cc
            .dot_ignores
            .iter()
            .all(|src| crate::dircache::unchanged(&abs.join(&src.path), src.mtime_ns, src.size))
}

/// Scan one directory live; subdirectories spawn onto the rayon scope.
/// Skipped or aborted scans simply contribute no node.
///
/// `parent_chain_stable`: every ancestor directory from the sweep root down
/// was scanned live this run AND its own `.gitignore` (mtime, size) matched
/// its cached node's recorded values. Only when the whole chain is stable
/// may a child subtree be pruned, since the child's cached listing was
/// filtered by exactly those matchers (dircache.rs precondition 3).
#[allow(clippy::too_many_arguments)]
fn scan_listing<'a, 'b>(
    scope: &rayon::Scope<'b>,
    root: &'b Path,
    dir: PathBuf,
    rel: PathBuf,
    stack: Vec<Arc<Gitignore>>,
    cached: Option<&'a DirNode>,
    parent_chain_stable: bool,
    observed_mtime_ns: u128,
    // 'a belongs to the cached tree (and the Cow entries in the output
    // buffers); 'b is the walk region (scope, root, ctx borrows). Spawned
    // tasks capture &'a DirNode references and must satisfy the scope's
    // 'b bound, hence 'a: 'b.
    ctx: &'b ListingCtx<'a>,
) where
    'a: 'b,
{
    if ctx.budget_exceeded.load(AtomicOrdering::Relaxed) {
        return;
    }
    if ctx.rescans.fetch_add(1, AtomicOrdering::Relaxed) + 1 > crate::dircache::RESCAN_BUDGET {
        ctx.budget_exceeded.store(true, AtomicOrdering::Relaxed);
        return;
    }

    let scan = match bulk_scan_dir(&dir) {
        Ok(s) => s,
        Err(ScanError::Soft(msg)) => {
            crate::messages::error(msg);
            // Its subtree drops out of the walked tree, which then no
            // longer matches the cached one.
            ctx.dirty.store(true, AtomicOrdering::Relaxed);
            return;
        }
        Err(ScanError::Fatal(e)) => {
            let mut slot = ctx.fatal.lock().unwrap();
            if slot.is_none() {
                *slot = Some(e);
            }
            return;
        }
    };

    // Same divergence trap as scan_recursive: .ignore/.rgignore precedence
    // is not reimplemented; their presence aborts the listing (caller
    // falls back to the portable walker, which handles them by
    // construction). .gitignore mtime/size feed chain stability.
    let mut has_local_gitignore = false;
    let mut gitignore_mtime_ns: u128 = 0;
    let mut gitignore_size: u64 = 0;
    let mut has_divergent_ignore_file = false;
    for f in &scan.files {
        if name_eq(&f.name, ".gitignore") {
            has_local_gitignore = true;
            gitignore_mtime_ns = f.mtime_ns;
            gitignore_size = f.size;
        } else if name_eq(&f.name, ".ignore") || name_eq(&f.name, ".rgignore") {
            has_divergent_ignore_file = true;
        }
    }
    if has_divergent_ignore_file {
        let mut slot = ctx.fatal.lock().unwrap();
        if slot.is_none() {
            *slot = Some(anyhow::anyhow!(
                "{}: contains .ignore/.rgignore, deferring to the walker",
                dir.display()
            ));
        }
        return;
    }

    let mut stack = stack;
    if has_local_gitignore {
        stack.extend(load_dir_gitignore(&dir));
    }

    // This dir's own .gitignore must match the cached observation for its
    // children to be prunable (the cache-time filter for a child listing
    // used this matcher's contents).
    let me_stable = parent_chain_stable
        && cached
            .map(|c| {
                c.gitignore_mtime_ns == gitignore_mtime_ns && c.gitignore_size == gitignore_size
            })
            .unwrap_or(false);

    let slot_idx = rayon::current_thread_index().unwrap_or(0) % ctx.files.len().max(1);
    let mut node = DirNode {
        rel,
        mtime_ns: observed_mtime_ns,
        gitignore_mtime_ns,
        gitignore_size,
        has_ignore_subtree: has_local_gitignore,
        // A directory listed live has neither file (it would have aborted
        // the listing above).
        dot_ignores: Vec::new(),
        files: Vec::with_capacity(scan.files.len()),
        subdirs: Vec::new(),
    };
    // This worker's own buffer: no other thread locks it.
    let mut out = ctx.files[slot_idx].lock().unwrap();
    for entry in &scan.files {
        let abs = dir.join(&entry.name);
        if is_ignored(&ctx.dot, &stack, &abs, false, &entry.name) {
            continue;
        }
        let crel = abs.strip_prefix(root).unwrap_or(&abs).to_path_buf();
        // Same whitelist-aware rule as the sweep (and so the
        // manifest's FLAG_HIDDEN): a `!` rule can un-hide a dotfile.
        let hidden = still_hidden(&ctx.dot, &stack, root, &crel);
        let fm = FileMeta {
            path: crel,
            mtime_ns: entry.mtime_ns,
            size: entry.size,
            hidden,
        };
        node.files.push(fm.clone());
        out.push(Cow::Owned(fm));
    }

    // Whether this directory's fresh node differs from its cached one,
    // which makes the walked tree differ from the cached tree: its own
    // fields here, its set of subdirectories below. Each subdirectory
    // listed live compares its own node in turn, and a pruned one is the
    // cached node itself. `has_ignore_subtree` folds in the children's,
    // which then equal the cached children's. Cached subdirs out of the
    // order assembly attaches them in (a cache an older glep built) count
    // as a change too, so a clean walk's tree is exactly the cached one;
    // siblings share their parent's prefix, so their paths' bytes order
    // them as `Path` does.
    let mut changed = match cached {
        None => true,
        Some(c) => {
            c.mtime_ns != node.mtime_ns
                || c.gitignore_mtime_ns != node.gitignore_mtime_ns
                || c.gitignore_size != node.gitignore_size
                || !c.dot_ignores.is_empty()
                || c.has_ignore_subtree
                    != (has_local_gitignore || c.subdirs.iter().any(|s| s.has_ignore_subtree))
                || c.files != node.files
                || c.subdirs.windows(2).any(|w| {
                    w[0].rel.as_os_str().as_encoded_bytes()
                        >= w[1].rel.as_os_str().as_encoded_bytes()
                })
        }
    };
    let mut reached: Vec<Cow<'a, DirNode>> = Vec::new();
    let mut subdirs = 0usize;
    for sub in scan.subdirs {
        let name = sub.name;
        let abs = dir.join(&name);
        if is_ignored(&ctx.dot, &stack, &abs, true, &name) {
            continue;
        }
        subdirs += 1;
        let child_rel = node.rel.join(&name);
        let cached_child = cached.and_then(|c| c.subdirs.iter().find(|n| n.rel == child_rel));
        match cached_child {
            Some(cc) if me_stable && leaf_unchanged(cc, sub.mtime_ns, &abs) => {
                emit_cached(cc, &mut out);
                reached.push(Cow::Borrowed(cc));
            }
            _ => {
                changed |= cached_child.is_none();
                let stack2 = stack.clone();
                scope.spawn(move |scope| {
                    if ctx.budget_exceeded.load(AtomicOrdering::Relaxed) {
                        return;
                    }
                    scan_listing(
                        scope,
                        root,
                        abs,
                        child_rel,
                        stack2,
                        cached_child,
                        me_stable,
                        sub.mtime_ns,
                        ctx,
                    );
                });
            }
        }
    }
    drop(out);
    changed |= cached.is_some_and(|c| c.subdirs.len() != subdirs);
    if changed {
        ctx.dirty.store(true, AtomicOrdering::Relaxed);
    }
    reached.push(Cow::Owned(node));
    ctx.nodes.lock().unwrap().append(&mut reached);
}

/// Emit a pruned subtree's cached listing recursively, borrowing every
/// FileMeta from the cached tree (zero clones).
fn emit_cached<'a>(n: &'a DirNode, out: &mut Vec<Cow<'a, FileMeta>>) {
    out.extend(n.files.iter().map(Cow::Borrowed));
    for s in &n.subdirs {
        emit_cached(s, out);
    }
}

// ---------------------------------------------------------------------
// --no-ignore bulk sweep: same getattrlistbulk traversal with EVERY
// ignore source deliberately absent (no matcher stack, no .gitignore
// handling, no divergence abort). The only filtering left is the hard
// .git/.glep exclusion and, when include_hidden is false, dot-prefixed
// entries, mirroring `walk::sweep_unfiltered`'s walker configuration
// (.ignore(false) .git_ignore(false) .git_global(false) .git_exclude(false)
// .parents(false) .hidden(!include_hidden)). The old path paid the
// `ignore` crate's per-file DirEntry metadata on top of the search; this
// collapses discovery to one bulk stat per directory.
// ---------------------------------------------------------------------

struct UnfilteredCtx {
    include_hidden: bool,
    files: Vec<Mutex<Vec<FileMeta>>>,
    fatal: Mutex<Option<anyhow::Error>>,
}

pub fn sweep_bulk_unfiltered(root: &Path, include_hidden: bool) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );
    let num_slots = rayon::current_num_threads().max(1);
    let ctx = UnfilteredCtx {
        include_hidden,
        files: (0..num_slots).map(|_| Mutex::new(Vec::new())).collect(),
        fatal: Mutex::new(None),
    };
    rayon::scope(|scope| {
        scan_unfiltered(scope, root, root.to_path_buf(), &ctx);
    });
    if let Some(e) = ctx.fatal.lock().unwrap().take() {
        return Err(e);
    }
    let mut all: Vec<FileMeta> = Vec::new();
    for buf in &ctx.files {
        all.extend(buf.lock().unwrap().iter().cloned());
    }
    Ok(all)
}

fn scan_unfiltered<'a>(
    scope: &rayon::Scope<'a>,
    root: &'a Path,
    dir: PathBuf,
    ctx: &'a UnfilteredCtx,
) {
    let scan = match bulk_scan_dir(&dir) {
        Ok(s) => s,
        Err(ScanError::Soft(msg)) => {
            crate::messages::error(msg);
            return;
        }
        Err(ScanError::Fatal(e)) => {
            let mut slot = ctx.fatal.lock().unwrap();
            if slot.is_none() {
                *slot = Some(e);
            }
            return;
        }
    };
    let slot_idx = rayon::current_thread_index().unwrap_or(0) % ctx.files.len().max(1);
    {
        let mut out = ctx.files[slot_idx].lock().unwrap();
        for entry in &scan.files {
            // Hidden gating only; ignore rules are entirely absent in this
            // mode, and .git/.glep are hard-excluded below either way.
            if !ctx.include_hidden && entry.name.as_encoded_bytes().first() == Some(&b'.') {
                continue;
            }
            if is_hard_excluded(&entry.name) {
                continue;
            }
            let abs = dir.join(&entry.name);
            let rel = abs.strip_prefix(root).unwrap_or(&abs).to_path_buf();
            let hidden = crate::walk::path_is_hidden(&rel);
            out.push(FileMeta {
                path: rel,
                mtime_ns: entry.mtime_ns,
                size: entry.size,
                hidden,
            });
        }
    }
    for sub in scan.subdirs {
        if !ctx.include_hidden && sub.name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        if is_hard_excluded(&sub.name) {
            continue;
        }
        let abs = dir.join(&sub.name);
        scope.spawn(|scope| {
            scan_unfiltered(scope, root, abs, ctx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    /// Leaves (`a/b/c`, `a/d`, `e`), interior directories with files of
    /// their own (the root, `a`, `a/b`), and a root `.gitignore`.
    fn fixture(p: &Path) {
        for (f, body) in [
            (".gitignore", "*.log\n"),
            ("top.txt", "x"),
            ("a/a.txt", "x"),
            ("a/x.log", "x"),
            ("a/b/b.txt", "x"),
            ("a/b/c/c.txt", "x"),
            ("a/b/c/c2.txt", "x"),
            ("a/d/d.txt", "x"),
            ("e/e.txt", "x"),
        ] {
            let f = p.join(f);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, body).unwrap();
        }
    }

    fn sorted_paths<'a>(files: impl IntoIterator<Item = &'a FileMeta>) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = files.into_iter().map(|m| m.path.clone()).collect();
        v.sort();
        v
    }

    /// What a full sweep lists: the reference for every pruned walk.
    fn swept(root: &Path) -> Vec<PathBuf> {
        sorted_paths(&sweep_bulk(root).unwrap())
    }

    /// The tree the --files path saves after a full sweep (one that
    /// started well after the fixture was written), then walked once:
    /// what the cache holds once listings have run.
    fn settled(root: &Path) -> DirNode {
        let mut listing = sweep_bulk(root).unwrap();
        listing.sort_by(|a, b| a.path.cmp(&b.path));
        let later = SystemTime::now() + Duration::from_secs(60);
        let built = crate::dircache::build_from_listing(root, &listing, Vec::new(), later).root;
        let walked = sweep_bulk_listing(root, Some(&built)).unwrap();
        assert_eq!(sorted_paths(walked.files.iter().map(|c| &**c)), swept(root));
        walked.tree.unwrap_or(built)
    }

    /// Walk with `cached`: the listing, which must equal a full sweep's,
    /// and the tree to save, if any.
    fn walk(root: &Path, cached: &DirNode) -> Option<DirNode> {
        let out = sweep_bulk_listing(root, Some(cached)).unwrap();
        assert!(!out.aborted);
        assert_eq!(sorted_paths(out.files.iter().map(|c| &**c)), swept(root));
        out.tree
    }

    #[test]
    fn a_clean_walk_assembles_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let cached = settled(dir.path());
        // (In debug builds the walk also asserts that assembling its
        // nodes would give back exactly `cached`.)
        assert!(walk(dir.path(), &cached).is_none());
        assert!(walk(dir.path(), &cached).is_none());
    }

    /// Every change to a directory's listing or ignore rules makes the
    /// walk dirty, and the tree it assembles is clean for the next walk.
    #[test]
    fn a_change_makes_the_walk_assemble_the_current_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture(root);
        let mut cached = settled(root);
        let mut step = |what: &str, change: &dyn Fn()| {
            change();
            let tree = walk(root, &cached).unwrap_or_else(|| panic!("{what}: clean"));
            assert!(walk(root, &tree).is_none(), "{what}: dirty again");
            cached = tree;
        };
        step("a file created in a leaf", &|| {
            fs::write(root.join("a/b/c/new.txt"), "x").unwrap()
        });
        step("a file deleted from an interior directory", &|| {
            fs::remove_file(root.join("a/a.txt")).unwrap()
        });
        step("a subdirectory added to a leaf", &|| {
            fs::create_dir(root.join("e/f")).unwrap();
            fs::write(root.join("e/f/f.txt"), "x").unwrap();
        });
        step("an empty subdirectory added to a leaf", &|| {
            fs::create_dir(root.join("a/d/empty")).unwrap()
        });
        step("a directory deleted", &|| {
            fs::remove_dir_all(root.join("a/b/c")).unwrap()
        });
        step("the root .gitignore edited in place, same size", &|| {
            fs::write(root.join(".gitignore"), "*.txt\n").unwrap()
        });
        step("a .gitignore created", &|| {
            fs::write(root.join("a/d/.gitignore"), "d.txt\n").unwrap()
        });
    }

    /// A cached mtime of zero is no observation: the leaf is listed live
    /// even when the parent's listing reports zero too.
    #[test]
    fn a_zero_mtime_never_prunes() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let mut cached = settled(dir.path());
        let e = cached
            .subdirs
            .iter_mut()
            .find(|n| n.rel == Path::new("e"))
            .unwrap();
        let abs = dir.path().join("e");
        assert!(leaf_unchanged(e, e.mtime_ns, &abs));
        let observed = e.mtime_ns;
        e.mtime_ns = 0;
        assert!(!leaf_unchanged(e, 0, &abs));
        assert!(!leaf_unchanged(e, observed, &abs));
        // A stale listing under an unknown mtime is never served.
        e.files.clear();
        let tree = walk(dir.path(), &cached).expect("the rescanned leaf is new");
        assert!(walk(dir.path(), &tree).is_none());
    }

    #[test]
    fn assemble_tree_attaches_children_in_path_order() {
        let node = |rel: &str, gitignore: bool| DirNode {
            has_ignore_subtree: gitignore,
            ..DirNode::default_for(Path::new(rel))
        };
        let a_b = node("a/b", true);
        let nodes = vec![
            Cow::Owned(node("b", false)),
            Cow::Borrowed(&a_b),
            Cow::Owned(node("", false)),
            Cow::Owned(node("a/a-b", false)),
            Cow::Owned(node("a", false)),
        ];
        let root = assemble_tree(nodes);
        let rels =
            |n: &DirNode| -> Vec<PathBuf> { n.subdirs.iter().map(|s| s.rel.clone()).collect() };
        assert_eq!(rels(&root), [PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(
            rels(&root.subdirs[0]),
            [PathBuf::from("a/a-b"), PathBuf::from("a/b")]
        );
        assert!(root.has_ignore_subtree && root.subdirs[0].has_ignore_subtree);
        assert!(!root.subdirs[1].has_ignore_subtree);
    }
}
