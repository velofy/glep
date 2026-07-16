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
//! Ignore semantics mirror `walk::sweep`'s `ignore::WalkBuilder` defaults
//! for the common cases: hidden (dot-prefixed) entries are skipped, and
//! `.gitignore` files are honored per directory level, stacked along the
//! recursion, with no dependency on an actual `.git` directory being
//! present (the same "require_git(false)" behavior `sweep` opts into).
//!
//! Correctness posture: any anomaly while parsing a `getattrlistbulk`
//! result buffer (an offset or length that doesn't fit inside the buffer)
//! is treated as fatal and turns into an `Err` from `sweep_bulk`, which the
//! dispatcher in `walk.rs` turns into a fallback to the portable walker.
//! Anything narrower, like one directory being unreadable (permission
//! denied, deleted mid-scan), is non-fatal: it is reported on stderr and
//! that subtree is skipped, exactly like `walk::sweep` does for individual
//! entry errors from the `ignore` crate.

#![cfg(target_os = "macos")]

use std::ffi::CString;
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
struct BulkEntry {
    name: String,
    objtype: u32,
    mtime_ns: u128,
    size: u64,
}

struct DirScan {
    files: Vec<BulkEntry>,
    subdirs: Vec<String>,
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

        let mut name = String::new();
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
            name = String::from_utf8_lossy(&raw[..nul_pos]).into_owned();
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

/// Combined verdict from a stack of per-directory-level gitignore matchers,
/// plus the default "skip dot-prefixed entries" rule that applies when no
/// gitignore file made an explicit decision. Mirrors the precedence the
/// `ignore` crate uses in its own walker: a decisive match (Ignore or
/// Whitelist) from a deeper directory's gitignore overrides one from a
/// shallower directory, and an explicit Whitelist always wins over the
/// hidden-file default.
fn is_ignored(stack: &[Arc<Gitignore>], abs_path: &Path, is_dir: bool, name: &str) -> bool {
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
        ignore::Match::None => name.starts_with('.'),
    }
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

    let mut stack = matchers;
    if scan.files.iter().any(|f| f.name == ".gitignore") {
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
        local_files.push(FileMeta {
            path: rel,
            mtime_ns: entry.mtime_ns,
            size: entry.size,
        });
    }
    if !local_files.is_empty() {
        buffers[slot_idx].lock().unwrap().extend(local_files);
    }

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
pub fn sweep_bulk(root: &Path) -> anyhow::Result<Vec<FileMeta>> {
    anyhow::ensure!(
        root.is_dir(),
        "{}: No such file or directory (os error 2)",
        root.display()
    );

    let num_slots = rayon::current_num_threads().max(1);
    let buffers: Vec<Mutex<Vec<FileMeta>>> =
        (0..num_slots).map(|_| Mutex::new(Vec::new())).collect();
    let fatal: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    rayon::scope(|scope| {
        scan_recursive(scope, root, root.to_path_buf(), Vec::new(), &buffers, &fatal);
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
