//! The one place that decides what an unknown sender may make us do: which names it may
//! write, how many files and bytes it may offer, where the bytes land, and when a transfer
//! counts as finished.
//!
//! A session receives into a hidden folder inside the download folder ([`Staging`]); a file
//! gets its final name only when it is complete (after crQuickShare `ReceiveBatch`), by a
//! rename that never replaces an existing file. Everything is dir-fd relative and exclusive
//! (`openat` with `O_CREAT | O_EXCL | O_NOFOLLOW`, `renameat2` with `RENAME_NOREPLACE`), never
//! "check a name, then open it". A [`Partial`] that is not finished removes its staged file
//! when it is dropped, and the last one removes the hidden folder, so every way out of a
//! session (cancel, decline, error, EOF, timeout, panic) cleans up.
//!
//! Only SIGKILL and power loss can leave a staging folder behind. The next accepted file
//! transfer into that folder removes the files directly in it, and the folder once it is
//! empty, if no session holds its `flock` and nothing in it changed for `STALE_AFTER`: a
//! folder on a file system that refuses `flock`, or on one shared by two machines (the lock
//! is per machine), is kept while it may still be in use.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustix::fs::{
    AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, RenameFlags, Stat, flock, fstat,
    fstatvfs, fsync, mkdirat, open, openat, renameat, renameat_with, stat, statat, unlinkat,
};
use rustix::io::Errno;

use crate::location_nearby_connections::payload_transfer_frame::{PayloadChunk, PayloadHeader};
use crate::sharing_nearby::FileMetadata;
use crate::sharing_nearby::connection_response_frame::Status;
use crate::utils::received_file_name;

/// The longest file name most file systems take, in bytes.
pub const MAX_NAME_BYTES: usize = 255;

/// The highest ` (N)` suffix tried when a name is taken.
const MAX_SUFFIX: u32 = 9999;

/// Start of the name of a session's hidden folder in the download folder.
const STAGING_PREFIX: &str = ".kuickshare-incoming-";

/// Names tried for a staging folder before giving up.
const STAGING_ATTEMPTS: usize = 4;

/// How long a staging folder and everything in it must be untouched before a sweep may take
/// it. A live session writes into its folder at least every `Limits::idle`, so this is far
/// above that, and it covers locks that do not reach another machine.
const STALE_AFTER: Duration = Duration::from_secs(3600);

/// Whether `name` is a staging folder name: the prefix and exactly 16 lowercase hex digits, so
/// that a user's own folder that happens to share the prefix is never swept.
fn is_staging_name(name: &[u8]) -> bool {
    name.strip_prefix(STAGING_PREFIX.as_bytes())
        .is_some_and(|rest| {
            rest.len() == 16 && rest.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Moves the modification time of `path` (a symlink itself, not its target) past
/// `STALE_AFTER`, as a dead session's leftovers would be.
#[cfg(test)]
pub(crate) fn backdate(path: &Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let then = rustix::fs::Timespec {
        tv_sec: i64::try_from(now).unwrap() - 2 * i64::try_from(STALE_AFTER.as_secs()).unwrap(),
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &rustix::fs::Timestamps {
            last_access: then,
            last_modification: then,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// Whether the stat's last change is older than `STALE_AFTER`. A clock before the epoch or a
/// time in the future counts as recent.
fn untouched_for_long(st: &Stat) -> bool {
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let changed = u64::try_from(st.st_mtime).unwrap_or(u64::MAX);
    now.as_secs().saturating_sub(changed) >= STALE_AFTER.as_secs()
}

/// Bounds on what a peer can make a session do.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Files in one offer; a larger offer is refused, never cut short.
    pub max_files: usize,
    /// Size of one in-memory (`Bytes`) payload: sharing frames, text, URLs, Wi-Fi.
    pub max_bytes_payload: usize,
    /// `Bytes` payloads being assembled at the same time.
    pub max_open_payloads: usize,
    /// From the start of a session to the offer. Slow links (BLE) need time here.
    pub handshake: Duration,
    /// Without a byte from the peer. Not applied while the user decides (the core's timer
    /// applies) nor while a Wi-Fi upgrade runs (it has its own deadlines).
    pub idle: Duration,
    /// How long the user may take to answer an offer. The core's own timer (300 s) comes
    /// first; this ceiling is for a core that missed the event (a lagging bus).
    pub consent: Duration,
    /// Inbound sessions at the same time on the LAN listener.
    pub max_sessions: usize,
    /// Inbound sessions at the same time from one address.
    pub max_per_addr: usize,
    /// Free space that must remain after the offer is saved.
    pub space_reserve: u64,
    /// When the service stops: time for the Cancel frames, the FIN and the peer's close. It
    /// must stay below `manager::STOP_WIND_DOWN`.
    pub stop_close: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_bytes_payload: 5 * 1024 * 1024,
            max_open_payloads: 4,
            handshake: Duration::from_secs(90),
            idle: Duration::from_secs(60),
            consent: Duration::from_secs(330),
            max_sessions: 32,
            max_per_addr: 4,
            space_reserve: 32 * 1024 * 1024,
            stop_close: Duration::from_millis(300),
        }
    }
}

/// Why an offer is refused. Each maps onto an existing protocol status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A name that cannot be saved as is (control or invisible characters, too long).
    BadName,
    TooManyFiles,
    /// A negative size, or sizes whose sum overflows.
    BadSize,
    /// Two files with one payload id.
    DuplicateId,
    NotEnoughSpace {
        need: u64,
        free: u64,
    },
    /// The download folder cannot be used (missing, not writable, or replaced since the offer).
    Destination,
}

impl Refusal {
    /// The status sent to the peer.
    pub fn status(&self) -> Status {
        match self {
            Self::NotEnoughSpace { .. } | Self::Destination => Status::NotEnoughSpace,
            _ => Status::UnsupportedAttachmentType,
        }
    }

    /// A stable key for the app (`reason` in the D-Bus API).
    pub fn key(&self) -> &'static str {
        match self {
            Self::BadName => "bad_name",
            Self::TooManyFiles => "too_many_files",
            Self::BadSize => "bad_size",
            Self::DuplicateId => "duplicate_id",
            Self::NotEnoughSpace { .. } => "not_enough_space",
            Self::Destination => "destination",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEnoughSpace { need, free } => {
                write!(f, "not enough space (need {need} bytes, {free} free)")
            }
            other => f.write_str(other.key()),
        }
    }
}

/// True if `name` could write outside the download folder or is not a plain file name.
pub fn unsafe_file_name(name: &str) -> bool {
    name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > MAX_NAME_BYTES
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control() || is_invisible(c))
}

/// Zero-width and text-direction characters: with them `photo<U+202E>gpj.exe` shows as `photoexe.jpg`.
/// U+200C and U+200D (joiners) are allowed: Persian and emoji sequences need them.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// The soft hyphen shows nothing of itself, and the Arabic letter mark works like the direction overrides. Both are allowed in a file name, and not in text.
fn invisible_in_text(c: char) -> bool {
    is_invisible(c) || matches!(c, '\u{00AD}' | '\u{061C}')
}

/// `text` without what a reader should not be shown: every control character except line breaks (tab, newline and carriage return stay), the direction overrides and zero-width characters a file name is refused for, and the soft hyphen and the Arabic letter mark, which hide the same way.
///
/// The text of a payload, its preview and the network name are an unknown sender's choice, and a consent, a notification and the history show them: with an `U+202E` in the middle, `no\u{202E}CPU` shows as `noUPC`, a line of digits reads backwards, and the invisible ones can carry content nobody sees. What this returns is what gets shown and copied; the payload itself is not display.
pub fn clean_text(text: &str) -> String {
    let drop =
        |c: char| (c.is_control() && !matches!(c, '\t' | '\n' | '\r')) || invisible_in_text(c);
    if !text.chars().any(drop) {
        return text.to_owned();
    }
    text.chars().filter(|c| !drop(*c)).collect()
}

/// One file of an accepted-for-review offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub payload_id: i64,
    /// The name to save under (before any ` (N)` suffix); always safe.
    pub name: String,
    pub size: u64,
}

/// Checks an offer against `l`. The sum of the sizes cannot overflow `u64` when this returns.
pub fn check_offer(files: &[FileMetadata], l: &Limits) -> Result<Vec<Offered>, Refusal> {
    if files.len() > l.max_files {
        return Err(Refusal::TooManyFiles);
    }
    let mut seen = HashSet::with_capacity(files.len());
    let mut out = Vec::with_capacity(files.len());
    let mut total: u64 = 0;
    for file in files {
        if !seen.insert(file.payload_id()) {
            return Err(Refusal::DuplicateId);
        }
        let size = u64::try_from(file.size()).map_err(|_| Refusal::BadSize)?;
        total = total.checked_add(size).ok_or(Refusal::BadSize)?;
        let name =
            received_file_name(file.name(), file.mime_type()).unwrap_or_else(|| "file".to_owned());
        if unsafe_file_name(&name) {
            return Err(Refusal::BadName);
        }
        out.push(Offered {
            payload_id: file.payload_id(),
            name,
            size,
        });
    }
    Ok(out)
}

/// `name`, or `stem (N).ext` for `n >= 1`, cut to fit `MAX_NAME_BYTES`. Always numbered from
/// the original name, so a name never nests (`a (1) (1).jpg`).
fn candidate(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let suffix = format!(" ({n})");
    let room = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let ext = clip(&ext, room);
    let stem = clip(stem, room - ext.len());
    format!("{stem}{suffix}{ext}")
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
pub(crate) fn clip(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `(device, inode)` of a stat result.
#[allow(clippy::unnecessary_cast)] // the field types differ between platforms
fn ident(st: &Stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

const FILE_MODE: Mode = Mode::from_bits_truncate(0o666);

/// Creates a file that must not exist yet, without following a symlink or blocking on a FIFO.
const CREATE_NEW: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY)
    .union(OFlags::CLOEXEC);

/// The folder files are saved in, held open by descriptor: every later operation is relative
/// to it, so renaming or swapping the folder, or a path component of it, changes nothing.
#[derive(Debug)]
pub struct DestDir {
    fd: std::os::fd::OwnedFd,
    path: PathBuf,
    /// Files of this folder currently open for writing.
    open_files: AtomicUsize,
}

impl DestDir {
    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC);

    /// Opens `path`, which must exist (the offer only looks, it creates nothing).
    pub fn open_existing(path: &Path) -> io::Result<Arc<Self>> {
        let fd = open(path, Self::DIR_FLAGS, Mode::empty())?;
        Ok(Arc::new(Self {
            fd,
            path: path.to_owned(),
            open_files: AtomicUsize::new(0),
        }))
    }

    /// Opens `path`, creating it first if it does not exist yet (the user accepted).
    pub fn open(path: &Path) -> io::Result<Arc<Self>> {
        match Self::open_existing(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(path)?;
                Self::open_existing(path)
            }
            other => other,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `path` still leads to the folder this holds (not swapped or removed since).
    pub fn is_still_at_path(&self) -> bool {
        match (stat(&self.path), fstat(&self.fd)) {
            (Ok(at_path), Ok(held)) => ident(&at_path) == ident(&held),
            _ => false,
        }
    }

    /// Files of this folder open for writing right now.
    pub fn open_files(&self) -> usize {
        self.open_files.load(Ordering::Relaxed)
    }

    /// Bytes an unprivileged writer can still store here. `u64::MAX` if the file system does
    /// not report its size.
    pub fn free_bytes(&self) -> io::Result<u64> {
        let st = fstatvfs(&self.fd)?;
        if st.f_blocks == 0 {
            return Ok(u64::MAX);
        }
        Ok(st.f_bavail.saturating_mul(st.f_frsize))
    }

    /// Free bytes at the nearest existing folder of `path` (the folder itself may not exist yet).
    pub fn free_bytes_near(path: &Path) -> io::Result<u64> {
        for dir in path.ancestors() {
            if let Ok(d) = Self::open_existing(dir) {
                return d.free_bytes();
            }
        }
        Err(io::ErrorKind::NotFound.into())
    }

    /// Whether `name` is taken, a dangling symlink included. An unreadable entry counts as taken.
    fn exists(&self, name: &str) -> bool {
        match statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => true,
            Err(e) => e != Errno::NOENT,
        }
    }

    /// The names `create` would pick for `names` (in order) right now. Cheap: it keeps, per
    /// original name, where the last search ended, so many files with one name cost one
    /// `stat` each, not a scan of everything taken before. Only for the consent preview: the
    /// names that count are the ones `create` returns.
    pub fn predict(&self, names: &[String]) -> Vec<String> {
        let mut taken: HashSet<String> = HashSet::with_capacity(names.len());
        let mut resume: HashMap<&str, u32> = HashMap::new();
        names
            .iter()
            .map(|name| {
                let start = resume.get(name.as_str()).copied().unwrap_or(0);
                for n in start..=MAX_SUFFIX {
                    let cand = candidate(name, n);
                    if !taken.contains(&cand) && !self.exists(&cand) {
                        taken.insert(cand.clone());
                        resume.insert(name.as_str(), n + 1);
                        return cand;
                    }
                }
                name.clone()
            })
            .collect()
    }

    /// Opens this session's hidden folder, where the files stay until they are complete, after
    /// removing the ones dead sessions left behind. Created at the accept, never at the offer.
    /// Blocking.
    pub fn staging(self: &Arc<Self>) -> io::Result<Arc<Staging>> {
        self.sweep_stale();
        for _ in 0..STAGING_ATTEMPTS {
            let mut random = [0u8; 8];
            rand::Rng::fill_bytes(&mut rand::rng(), &mut random);
            let name = format!("{STAGING_PREFIX}{}", hex::encode(random));
            if let Some(fd) = self.try_staging(&name)? {
                return Ok(Arc::new(Staging {
                    fd,
                    dest: Arc::clone(self),
                    name,
                    next: Mutex::new(HashMap::new()),
                    staged_next: Mutex::new(HashMap::new()),
                    sync_warned: AtomicBool::new(false),
                }));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free staging folder",
        ))
    }

    /// Makes, opens and locks the folder `name`. `None` if the name cannot be used (taken,
    /// replaced or removed under us, locked by a session that swept it first): try another.
    fn try_staging(&self, name: &str) -> io::Result<Option<OwnedFd>> {
        match mkdirat(&self.fd, name, Mode::RWXU) {
            Ok(()) => {}
            Err(Errno::EXIST) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let fd = match openat(
            &self.fd,
            name,
            Self::DIR_FLAGS | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::LOOP | Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Ok(None),
            // Network and FUSE file systems may refuse the lock in several ways: the folder
            // stays unlocked, and the sweep's age check keeps it while it is in use.
            Err(e) => {
                debug!("The staging folder is not locked ({e})");
            }
        }
        // Still the folder this made, and not one swapped in or swept between the calls.
        match (
            statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW),
            fstat(&fd),
        ) {
            (Ok(at_name), Ok(held)) if ident(&at_name) == ident(&held) => Ok(Some(fd)),
            _ => Ok(None),
        }
    }

    /// Removes the staging folders that no session holds (SIGKILL and power loss leave them).
    /// A folder is stale when its `flock` can be taken and neither it nor anything in it
    /// changed for `STALE_AFTER`; anything that is not a folder, and anything inside a folder
    /// that cannot be unlinked (a subfolder), stays.
    pub(crate) fn sweep_stale(&self) {
        let Ok(dir) = Dir::read_from(&self.fd) else {
            return;
        };
        let names: Vec<CString> = dir
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_owned())
            .filter(|name| is_staging_name(name.as_bytes()))
            .collect();
        for name in names {
            let Ok(fd) = openat(
                &self.fd,
                name.as_c_str(),
                Self::DIR_FLAGS | OFlags::NOFOLLOW,
                Mode::empty(),
            ) else {
                continue;
            };
            if matches!(
                flock(&fd, FlockOperation::NonBlockingLockExclusive),
                Err(Errno::WOULDBLOCK)
            ) {
                continue;
            }
            if !fstat(&fd).is_ok_and(|st| untouched_for_long(&st)) {
                continue;
            }
            let Ok(inside) = Dir::read_from(&fd) else {
                continue;
            };
            let entries: Vec<CString> = inside
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_owned())
                .filter(|entry| !matches!(entry.as_bytes(), b"." | b".."))
                .collect();
            let all_old = entries.iter().all(|entry| {
                statat(&fd, entry.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
                    .is_ok_and(|st| untouched_for_long(&st))
            });
            if !all_old {
                continue;
            }
            // No flags: a symlink is removed, never followed, and a folder is left.
            for entry in entries {
                let _ = unlinkat(&fd, entry.as_c_str(), AtFlags::empty());
            }
            let _ = unlinkat(&self.fd, name.as_c_str(), AtFlags::REMOVEDIR);
        }
    }
}

/// The hidden folder (`.kuickshare-incoming-<random>`) of one receiving session, inside the
/// download folder so a rename out of it never crosses a file system. It is locked while the
/// session lives and removed with the last [`Partial`] that holds it.
#[derive(Debug)]
pub struct Staging {
    fd: OwnedFd,
    dest: Arc<DestDir>,
    name: String,
    /// Per wanted name, the number the next publish starts from.
    next: Mutex<HashMap<String, u32>>,
    /// The same for the names in the staging folder, so that thousands of files offered
    /// under one name do not retry every taken number again.
    staged_next: Mutex<HashMap<String, u32>>,
    /// A failed sync of the download folder is reported once per session, not per file.
    sync_warned: AtomicBool,
}

impl Staging {
    /// Reserves a new, empty file for `declared` bytes, staged as `name` or the first free
    /// `stem (N).ext`. Never opens an existing entry, never follows a symlink. The file is not
    /// kept open: the descriptor only exists while data is written (`Partial::write_at`), so
    /// an offer of thousands of files does not use thousands of descriptors. The real name is
    /// kept, so a name the file system refuses fails here, at the accept.
    pub fn create(self: &Arc<Self>, name: &str, declared: u64) -> io::Result<Partial> {
        if unsafe_file_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain file name",
            ));
        }
        let mut staged_next = self
            .staged_next
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let start = staged_next.get(name).copied().unwrap_or(0);
        for n in start..=MAX_SUFFIX {
            let cand = candidate(name, n);
            match openat(&self.fd, cand.as_str(), CREATE_NEW, FILE_MODE) {
                Ok(fd) => {
                    staged_next.insert(name.to_owned(), n + 1);
                    let file = File::from(fd);
                    let ident = match fstat(&file) {
                        Ok(st) => ident(&st),
                        Err(e) => {
                            let _ = unlinkat(&self.fd, cand.as_str(), AtFlags::empty());
                            return Err(e.into());
                        }
                    };
                    return Ok(Partial {
                        staging: Arc::clone(self),
                        wanted: name.to_owned(),
                        planned: None,
                        name: cand,
                        file: None,
                        ident,
                        declared,
                        written: 0,
                        done: false,
                    });
                }
                Err(e) if e == Errno::EXIST => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free file name",
        ))
    }

    /// Moves the staged file `staged` into the download folder as `planned`, else as `wanted`
    /// or the first free `stem (N).ext`, never replacing a file, and returns the name it got.
    /// Blocking.
    fn publish(&self, staged: &str, wanted: &str, planned: Option<&str>) -> io::Result<String> {
        // The name reported at the accept first, so that files of one name that complete out
        // of order still get the paths they were announced with.
        if let Some(planned) = planned {
            match self.move_unless_taken(staged, planned) {
                Ok(()) => {
                    self.sync_dest();
                    return Ok(planned.to_owned());
                }
                Err(Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let start = next.get(wanted).copied().unwrap_or(0);
        for n in start..=MAX_SUFFIX {
            let cand = candidate(wanted, n);
            match self.move_unless_taken(staged, &cand) {
                Ok(()) => {
                    next.insert(wanted.to_owned(), n + 1);
                    self.sync_dest();
                    return Ok(cand);
                }
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free file name",
        ))
    }

    /// The download folder is synced so that a power loss after the peer's ack does not leave
    /// the file in the staging folder, where a later sweep would delete it.
    fn sync_dest(&self) {
        if let Err(e) = fsync(&self.dest.fd)
            && !self.sync_warned.swap(true, Ordering::Relaxed)
        {
            warn!("Syncing the download folder failed: {e}");
        }
    }

    /// `renameat2` with `RENAME_NOREPLACE`: `EXIST` if `cand` is taken, a dangling symlink
    /// included.
    fn move_unless_taken(&self, staged: &str, cand: &str) -> Result<(), Errno> {
        match renameat_with(
            &self.fd,
            staged,
            &self.dest.fd,
            cand,
            RenameFlags::NOREPLACE,
        ) {
            Err(Errno::INVAL | Errno::NOSYS) => self.move_unless_taken_without_flag(staged, cand),
            other => other,
        }
    }

    /// For a kernel or file system without `RENAME_NOREPLACE`: reserves `cand` with an
    /// exclusive create, then renames over that empty file. A file put under `cand` between
    /// the two calls is replaced; the folder is the user's own.
    fn move_unless_taken_without_flag(&self, staged: &str, cand: &str) -> Result<(), Errno> {
        drop(openat(&self.dest.fd, cand, CREATE_NEW, FILE_MODE)?);
        renameat(&self.fd, staged, &self.dest.fd, cand).inspect_err(|_| {
            let _ = unlinkat(&self.dest.fd, cand, AtFlags::empty());
        })
    }

    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.dest.path.join(&self.name)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Not empty if a file was swapped in or left behind: the next sweep takes it.
        let _ = unlinkat(&self.dest.fd, self.name.as_str(), AtFlags::REMOVEDIR);
    }
}

/// Why a chunk was refused. Every variant ends the session.
#[derive(Debug)]
pub enum ChunkError {
    /// A `Bytes` payload with a negative size or one above the limit.
    BadSize(i64),
    TooManyPayloads,
    /// A chunk whose header declares another total than the first chunk of the payload did,
    /// or than the offer did for a file.
    SizeChanged {
        got: i64,
        declared: u64,
    },
    /// The offset is not where the previous chunk ended.
    Offset {
        got: i64,
        expected: u64,
    },
    /// More bytes than the declared size.
    TooBig {
        declared: u64,
    },
    /// The last chunk came before the declared size was reached.
    Short {
        got: u64,
        declared: u64,
    },
    Io(io::Error),
}

impl fmt::Display for ChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSize(n) => write!(f, "payload size {n} is not acceptable"),
            Self::TooManyPayloads => f.write_str("too many payloads at the same time"),
            Self::SizeChanged { got, declared } => {
                write!(
                    f,
                    "payload size changed to {got} from the declared {declared}"
                )
            }
            Self::Offset { got, expected } => {
                write!(f, "unexpected chunk offset {got}, expected {expected}")
            }
            Self::TooBig { declared } => {
                write!(f, "more data than the declared {declared} bytes")
            }
            Self::Short { got, declared } => {
                write!(
                    f,
                    "the payload ended after {got} of the declared {declared} bytes"
                )
            }
            Self::Io(e) => write!(f, "writing the file: {e}"),
        }
    }
}

impl std::error::Error for ChunkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ChunkError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A file being received. Its staged name is reserved from the start (an empty file in the
/// hidden folder); the data is written through a descriptor opened on the first chunk, and
/// `finish` publishes the file under its final name. Dropped before `finish`, it removes its
/// staged file (only if the entry still is the one this created).
#[derive(Debug)]
pub struct Partial {
    staging: Arc<Staging>,
    /// The name the peer offered, which the file is published as (or numbered from).
    wanted: String,
    /// The final name announced at the accept, tried first when the file is published.
    planned: Option<String>,
    /// The name in the staging folder.
    name: String,
    file: Option<File>,
    ident: (u64, u64),
    declared: u64,
    written: u64,
    done: bool,
}

impl Partial {
    #[cfg(test)]
    fn staged_path(&self) -> PathBuf {
        self.staging.path().join(&self.name)
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Sets the final name announced to the app at the accept.
    pub fn plan(&mut self, name: String) {
        self.planned = Some(name);
    }

    /// Files of the download folder open for writing right now.
    pub fn open_files(&self) -> usize {
        self.staging.dest.open_files()
    }

    /// Whether `st` (an entry or an open file) is the file this reserved, given that it
    /// should hold `size` bytes. The same `(dev, ino)` is. Some file systems (vfat and exFAT
    /// after the inode was evicted, CIFS without server inode numbers) hand out another
    /// inode number for the same file, which would fail every transfer there; so a regular
    /// file on the same device with exactly the size written so far (0 before the first
    /// chunk) is taken too. The trade-off: a process of the same user that swaps in a
    /// regular file of that size passes, and such a process can write the folder anyway.
    fn is_reserved(&self, st: &Stat, size: u64) -> bool {
        // Only a regular file can be ours: an inode number freed by a removal can come back
        // at once for a FIFO or a device node created under the same name.
        if !FileType::from_raw_mode(st.st_mode).is_file() {
            return false;
        }
        if ident(st) == self.ident {
            return true;
        }
        let same = ident(st).0 == self.ident.0 && u64::try_from(st.st_size) == Ok(size);
        if same {
            debug!("A received file changed its inode number; kept because device and size match");
        }
        same
    }

    /// Opens the reserved file for writing, if it is not open; fails if the entry is no
    /// longer the file this reserved.
    fn handle(&mut self) -> io::Result<&File> {
        if self.file.is_none() {
            let fd = openat(
                &self.staging.fd,
                self.name.as_str(),
                // NONBLOCK: a FIFO swapped in under the name must not block a runtime thread.
                OFlags::WRONLY
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::NOCTTY
                    | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let file = File::from(fd);
            let st = fstat(&file)?;
            if !FileType::from_raw_mode(st.st_mode).is_file()
                || !self.is_reserved(&st, self.written)
            {
                return Err(io::Error::other(
                    "the file was replaced while it was received",
                ));
            }
            self.staging.dest.open_files.fetch_add(1, Ordering::Relaxed);
            self.file = Some(file);
        }
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("file not open"))
    }

    /// Closes the descriptor; the data written so far stays. Reopened by the next write.
    pub fn release(&mut self) {
        if self.file.take().is_some() {
            self.staging.dest.open_files.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Writes `data` at `offset`, which must be where the previous chunk ended, without going
    /// past the declared size. `total` is the size in the chunk's header: a chunk that carries
    /// data must declare the offer's size. The empty last chunk is not held to it, because what
    /// phones put in that header was never observed *(inferred)*, and it writes nothing.
    pub fn write_at(&mut self, total: i64, offset: i64, data: &[u8]) -> Result<(), ChunkError> {
        if !data.is_empty() && u64::try_from(total) != Ok(self.declared) {
            return Err(ChunkError::SizeChanged {
                got: total,
                declared: self.declared,
            });
        }
        if u64::try_from(offset) != Ok(self.written) {
            return Err(ChunkError::Offset {
                got: offset,
                expected: self.written,
            });
        }
        let len = data.len() as u64;
        if self
            .written
            .checked_add(len)
            .is_none_or(|end| end > self.declared)
        {
            return Err(ChunkError::TooBig {
                declared: self.declared,
            });
        }
        if !data.is_empty() {
            let at = self.written;
            self.handle()?.write_all_at(data, at)?;
            self.written += len;
        }
        Ok(())
    }

    /// Completes the file: all declared bytes are there and on disk, and it is published
    /// under its final name. Returns the path it got (the name may carry a ` (N)` suffix).
    /// Blocking.
    pub fn finish(mut self) -> io::Result<PathBuf> {
        if self.written != self.declared {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "the peer sent {} of the declared {} bytes",
                    self.written, self.declared
                ),
            ));
        }
        // fsync reports delayed write errors; a close error after a good fsync is not
        // actionable, and std cannot report one.
        if self.written > 0 {
            self.handle()?.sync_all()?;
        }
        self.release();
        // The name must still lead to the file that was written, or what is published would
        // be something else.
        let entry = statat(
            &self.staging.fd,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        if !self.is_reserved(&entry, self.declared) {
            return Err(io::Error::other(
                "the file was replaced before it was completed",
            ));
        }
        let published = self
            .staging
            .publish(&self.name, &self.wanted, self.planned.as_deref())?;
        self.done = true;
        Ok(self.staging.dest.path.join(published))
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        self.release();
        if self.done {
            return;
        }
        // Another program may have replaced the entry: remove only the file this wrote. The
        // check and the unlink are two calls, so an entry swapped between them is still
        // removed; the directory is the user's own and Linux has no unlink-if-inode.
        if let Ok(st) = statat(
            &self.staging.fd,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        ) && self.is_reserved(&st, self.written)
        {
            let _ = unlinkat(&self.staging.fd, self.name.as_str(), AtFlags::empty());
        }
    }
}

/// A `Bytes` payload being assembled. The size comes from its first chunk and stays: a later
/// chunk cannot raise the ceiling.
#[derive(Debug)]
pub struct Assembly {
    declared: usize,
    data: Vec<u8>,
}

impl Assembly {
    #[cfg(test)]
    pub(crate) fn buffered(&self) -> usize {
        self.data.len()
    }
}

/// Adds one `Bytes` chunk to `buffers`. Returns the payload once the last chunk is in (and
/// forgets it); `None` while more is expected. Memory follows the bytes received, never the
/// size the peer declares, and the size declared by the first chunk is a ceiling on them. A
/// refused payload is forgotten too.
pub fn assemble(
    buffers: &mut HashMap<i64, Assembly>,
    header: &PayloadHeader,
    chunk: &PayloadChunk,
    l: &Limits,
) -> Result<Option<Vec<u8>>, ChunkError> {
    let id = header.id();
    let total = header.total_size();
    let declared = match buffers.get(&id).map(|open| open.declared) {
        Some(first) if u64::try_from(total) != Ok(first as u64) => {
            buffers.remove(&id);
            return Err(ChunkError::SizeChanged {
                got: total,
                declared: first as u64,
            });
        }
        Some(first) => first,
        None => match usize::try_from(total) {
            Ok(n) if n <= l.max_bytes_payload => {
                if buffers.len() >= l.max_open_payloads {
                    return Err(ChunkError::TooManyPayloads);
                }
                n
            }
            _ => return Err(ChunkError::BadSize(total)),
        },
    };
    let open = buffers.entry(id).or_insert_with(|| Assembly {
        declared,
        data: Vec::new(),
    });
    if u64::try_from(chunk.offset()) != Ok(open.data.len() as u64) {
        let err = ChunkError::Offset {
            got: chunk.offset(),
            expected: open.data.len() as u64,
        };
        buffers.remove(&id);
        return Err(err);
    }
    let body = chunk.body();
    if open.data.len().saturating_add(body.len()) > declared {
        buffers.remove(&id);
        return Err(ChunkError::TooBig {
            declared: declared as u64,
        });
    }
    open.data.extend_from_slice(body);
    if chunk.flags() & 1 == 1 {
        let got = open.data.len();
        if got != declared {
            buffers.remove(&id);
            return Err(ChunkError::Short {
                got: got as u64,
                declared: declared as u64,
            });
        }
        return Ok(buffers.remove(&id).map(|done| done.data));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("rqs-safety-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }

    #[test]
    fn plain_names_are_fine() {
        for name in [
            "photo.jpg",
            "Video 6.mp4",
            "relazione finale (2).pdf",
            ".bashrc",
            "a.b.c",
        ] {
            assert!(!unsafe_file_name(name), "{name}");
        }
    }

    #[test]
    fn clean_text_passes_ordinary_text_unchanged() {
        for text in [
            "",
            "hello",
            "https://example.org/a?b=c",
            "one\ttwo\nthree\r\nfour",
            "come prima: una sì e una no",
            "می\u{200C}خواهم",
            "family \u{1F468}\u{200D}\u{1F469}.png",
            "€ 5 or 5 €",
        ] {
            assert_eq!(clean_text(text), text);
        }
    }

    #[test]
    fn clean_text_drops_what_a_display_lies_with() {
        // A direction override shows `no\u{202E}CPU` as `noUPC`: gone from what is shown.
        assert_eq!(clean_text("no\u{202E}CPU"), "noCPU");
        assert_eq!(clean_text("order\u{200E}\u{200F}66"), "order66");
        assert_eq!(clean_text("a\u{2066}b\u{2069}c"), "abc");
        assert_eq!(clean_text("a\u{200B}b"), "ab");
        assert_eq!(clean_text("a\u{2060}b\u{FEFF}"), "ab");
        // The soft hyphen and the Arabic letter mark.
        assert_eq!(clean_text("e\u{00AD}-mail"), "e-mail");
        assert_eq!(clean_text("x\u{061C}y"), "xy");
        // Every control character goes but the line breaks: BEL, NUL and DEL included.
        assert_eq!(clean_text("a\u{0007}b\n"), "ab\n");
        assert_eq!(clean_text("a\u{0000}b\u{007F}"), "ab");
        assert_eq!(clean_text("a\u{009F}b"), "ab");
        assert_eq!(clean_text("\u{001B}[31m"), "[31m");
    }

    #[test]
    fn unsafe_names() {
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        for name in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "/etc/passwd",
            "..\\x",
            "a\0b",
            "line\nbreak",
            "photo\u{202E}gpj.exe",
            "zero\u{200B}width",
            long.as_str(),
        ] {
            assert!(unsafe_file_name(name), "{name:?}");
        }
    }

    /// A staging folder in the temp dir's download folder.
    fn staging_in(tmp: &TempDir) -> Arc<Staging> {
        DestDir::open(&tmp.0).unwrap().staging().unwrap()
    }

    /// Receives `data` as one complete file and returns the path it was published under.
    fn receive(staging: &Arc<Staging>, name: &str, data: &[u8]) -> PathBuf {
        let total = data.len() as i64;
        let mut p = staging.create(name, data.len() as u64).unwrap();
        p.write_at(total, 0, data).unwrap();
        p.finish().unwrap()
    }

    #[test]
    fn publish_never_replaces() {
        let tmp = TempDir::new("publish");
        let staging = staging_in(&tmp);
        std::fs::write(tmp.0.join("a.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(tmp.0.join("gone"), tmp.0.join("a (1).txt")).unwrap();

        assert_eq!(receive(&staging, "a.txt", b"1"), tmp.0.join("a (2).txt"));
        assert_eq!(receive(&staging, "a.txt", b"2"), tmp.0.join("a (3).txt"));
        // A name that already looks numbered is numbered from itself.
        assert_eq!(
            receive(&staging, "a (2).txt", b"3"),
            tmp.0.join("a (2) (1).txt")
        );
        assert_eq!(receive(&staging, "noext", b"4"), tmp.0.join("noext"));
        assert_eq!(receive(&staging, "noext", b"5"), tmp.0.join("noext (1)"));
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"x");
        assert_eq!(std::fs::read(tmp.0.join("a (2).txt")).unwrap(), b"1");
        assert_eq!(std::fs::read(tmp.0.join("a (3).txt")).unwrap(), b"2");
        assert!(
            std::fs::symlink_metadata(tmp.0.join("a (1).txt"))
                .unwrap()
                .is_symlink(),
            "a dangling symlink was replaced"
        );
        assert!(
            !tmp.0.join("gone").exists(),
            "a dangling symlink was followed"
        );
    }

    /// Files of one transfer with one name are numbered in the order they complete.
    #[test]
    fn publish_numbers_duplicates_within_a_transfer() {
        let tmp = TempDir::new("duplicates");
        let staging = staging_in(&tmp);
        let mut all: Vec<Partial> = (0..3)
            .map(|_| staging.create("a.txt", 1).unwrap())
            .collect();
        for (i, p) in all.iter_mut().enumerate() {
            p.write_at(1, 0, &[b'0' + i as u8]).unwrap();
        }
        let third = all.pop().unwrap().finish().unwrap();
        let first = all.remove(0).finish().unwrap();
        let second = all.remove(0).finish().unwrap();
        assert_eq!(third, tmp.0.join("a.txt"));
        assert_eq!(first, tmp.0.join("a (1).txt"));
        assert_eq!(second, tmp.0.join("a (2).txt"));
        assert_eq!(std::fs::read(third).unwrap(), b"2");
        assert_eq!(std::fs::read(first).unwrap(), b"0");
        assert_eq!(std::fs::read(second).unwrap(), b"1");
    }

    /// The path for a kernel or file system without `RENAME_NOREPLACE`.
    #[test]
    fn fallback_publish_never_replaces() {
        let tmp = TempDir::new("fallback");
        let staging = staging_in(&tmp);
        let mut p = staging.create("x", 1).unwrap();
        p.write_at(1, 0, b"n").unwrap();
        std::fs::write(tmp.0.join("taken"), b"keep").unwrap();
        std::os::unix::fs::symlink(tmp.0.join("gone"), tmp.0.join("dangling")).unwrap();

        for name in ["taken", "dangling"] {
            assert_eq!(
                staging.move_unless_taken_without_flag("x", name),
                Err(Errno::EXIST),
                "{name}"
            );
        }
        assert_eq!(std::fs::read(tmp.0.join("taken")).unwrap(), b"keep");
        assert!(
            !tmp.0.join("gone").exists(),
            "a dangling symlink was followed"
        );
        assert!(p.staged_path().exists(), "the staged file moved anyway");

        assert_eq!(staging.move_unless_taken_without_flag("x", "free"), Ok(()));
        assert_eq!(std::fs::read(tmp.0.join("free")).unwrap(), b"n");
        assert!(!p.staged_path().exists());
    }

    #[test]
    fn partial_is_staged_until_finished() {
        let tmp = TempDir::new("staged");
        let dir = DestDir::open(&tmp.0).unwrap();
        let staging = dir.staging().unwrap();
        let hidden = staging.path();
        let hidden_name = hidden.file_name().unwrap().to_string_lossy().into_owned();
        assert!(hidden_name.starts_with(STAGING_PREFIX), "{hidden_name}");
        assert_eq!(hidden_name.len(), STAGING_PREFIX.len() + 16);
        assert_eq!(
            std::fs::metadata(&hidden).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let mut p = staging.create("a.txt", 4).unwrap();
        drop(staging);
        p.write_at(4, 0, b"ab").unwrap();
        assert!(p.staged_path().exists());
        p.write_at(4, 2, b"cd").unwrap();
        assert_eq!(
            names(&tmp.0),
            [hidden_name],
            "a file has its final name early"
        );

        let path = p.finish().unwrap();
        assert_eq!(path, tmp.0.join("a.txt"));
        assert_eq!(std::fs::read(path).unwrap(), b"abcd");
        assert_eq!(names(&tmp.0), ["a.txt"], "the hidden folder is left");
    }

    #[test]
    fn predict_matches_publish() {
        let tmp = TempDir::new("predict");
        std::fs::write(tmp.0.join("a.txt"), b"x").unwrap();
        let dir = DestDir::open(&tmp.0).unwrap();
        let wanted: Vec<String> = ["a.txt", "a.txt", "b.txt", "a.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let predicted = dir.predict(&wanted);
        let staging = dir.staging().unwrap();
        let published: Vec<String> = wanted
            .iter()
            .map(|n| {
                let path = staging.create(n, 0).unwrap().finish().unwrap();
                path.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        assert_eq!(predicted, published);
        assert_eq!(predicted, ["a (1).txt", "a (2).txt", "b.txt", "a (3).txt"]);
    }

    #[test]
    fn names_fit_name_max() {
        let tmp = TempDir::new("max");
        let staging = staging_in(&tmp);
        // 250 bytes of 2-byte characters plus `.jpg`: the suffix does not fit without a cut,
        // which must fall on a character boundary.
        let stem = "é".repeat(125);
        let name = format!("{stem}.jpg");
        assert_eq!(name.len(), 254);
        assert!(!unsafe_file_name(&name));
        let first = receive(&staging, &name, b"x");
        let second = receive(&staging, &name, b"x");
        let file_name = second.file_name().unwrap().to_string_lossy().into_owned();
        assert!(file_name.len() <= MAX_NAME_BYTES, "{}", file_name.len());
        assert!(file_name.ends_with(" (1).jpg"), "{file_name}");
        assert_ne!(first, second);
        // A name of exactly the maximum length is numbered too, staged and published.
        let full = "x".repeat(MAX_NAME_BYTES);
        let mut a = staging.create(&full, 1).unwrap();
        let mut b = staging.create(&full, 1).unwrap();
        a.write_at(1, 0, b"x").unwrap();
        b.write_at(1, 0, b"x").unwrap();
        let (a, b) = (a.finish().unwrap(), b.finish().unwrap());
        assert!(a.file_name().unwrap().len() <= MAX_NAME_BYTES);
        assert!(b.file_name().unwrap().len() <= MAX_NAME_BYTES);
        assert_ne!(a, b);
    }

    #[test]
    fn create_refuses_paths() {
        let tmp = TempDir::new("paths");
        let staging = staging_in(&tmp);
        for bad in ["../x", "a/b", "", "..", "."] {
            assert!(staging.create(bad, 1).is_err(), "{bad:?}");
        }
        assert!(names(&staging.path()).is_empty());
        assert_eq!(names(&tmp.0).len(), 1, "only the staging folder");
    }

    #[test]
    fn finish_requires_declared_size() {
        let tmp = TempDir::new("finish");
        let staging = staging_in(&tmp);
        let mut short = staging.create("short.txt", 10).unwrap();
        short.write_at(10, 0, b"12345").unwrap();
        assert!(short.finish().is_err());
        assert!(!tmp.0.join("short.txt").exists(), "a short file stayed");
        assert!(names(&staging.path()).is_empty(), "a short file stayed");

        let mut full = staging.create("full.txt", 5).unwrap();
        full.write_at(5, 0, b"123").unwrap();
        full.write_at(5, 3, b"45").unwrap();
        let path = full.finish().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"12345");
    }

    #[test]
    fn write_at_checks_offset_and_size() {
        let tmp = TempDir::new("write");
        let staging = staging_in(&tmp);
        let mut p = staging.create("a", 4).unwrap();
        assert!(matches!(
            p.write_at(4, 1, b"x"),
            Err(ChunkError::Offset { .. })
        ));
        assert!(matches!(
            p.write_at(4, -1, b"x"),
            Err(ChunkError::Offset { .. })
        ));
        p.write_at(4, 0, b"ab").unwrap();
        assert!(matches!(
            p.write_at(4, 2, b"cdef"),
            Err(ChunkError::TooBig { .. })
        ));
        assert_eq!(p.written(), 2);
    }

    #[test]
    fn write_at_refuses_a_total_other_than_the_offer() {
        let tmp = TempDir::new("total");
        let staging = staging_in(&tmp);
        let mut p = staging.create("a", 4).unwrap();
        for total in [5, 3, 0, -1, i64::MAX] {
            assert!(
                matches!(
                    p.write_at(total, 0, b"ab"),
                    Err(ChunkError::SizeChanged { declared: 4, .. })
                ),
                "{total}"
            );
        }
        assert_eq!(p.written(), 0);
        p.write_at(4, 0, b"ab").unwrap();
        assert!(matches!(
            p.write_at(8, 2, b"cd"),
            Err(ChunkError::SizeChanged {
                got: 8,
                declared: 4
            })
        ));
        assert_eq!(p.written(), 2);
        // The empty last chunk writes nothing and is not held to the offer's size.
        p.write_at(4, 2, b"cd").unwrap();
        p.write_at(-1, 4, b"").unwrap();
        assert_eq!(p.written(), 4);
    }

    #[test]
    fn drop_removes_an_unfinished_file() {
        let tmp = TempDir::new("drop");
        let staging = staging_in(&tmp);
        let hidden = staging.path();
        let mut p = staging.create("a.txt", 4).unwrap();
        p.write_at(4, 0, b"ab").unwrap();
        assert_eq!(names(&hidden), ["a.txt"]);
        drop(staging);
        drop(p);
        assert!(!hidden.exists(), "the staging folder is left");
        assert!(names(&tmp.0).is_empty());
    }

    #[test]
    fn drop_does_not_unlink_a_replaced_entry() {
        let tmp = TempDir::new("replaced");
        let staging = staging_in(&tmp);
        let hidden = staging.path();
        let p = staging.create("a.txt", 4).unwrap();
        // Another program swaps the entry (the old inode stays alive: `p` holds it open).
        std::fs::write(hidden.join("other"), b"theirs").unwrap();
        std::fs::rename(hidden.join("other"), hidden.join("a.txt")).unwrap();
        drop(p);
        assert_eq!(std::fs::read(hidden.join("a.txt")).unwrap(), b"theirs");
    }

    #[test]
    fn fd_count_stays_bounded_for_many_files() {
        let tmp = TempDir::new("fds");
        let dir = DestDir::open(&tmp.0).unwrap();
        let staging = dir.staging().unwrap();
        let mut all: Vec<Partial> = (0..1500)
            .map(|i| staging.create(&format!("f{i}.txt"), 4).unwrap())
            .collect();
        assert_eq!(dir.open_files(), 0, "a reserved file holds no descriptor");
        // One file at a time is written and released, as the session does.
        for p in &mut all {
            p.write_at(4, 0, b"abcd").unwrap();
            assert!(dir.open_files() <= 1);
            p.release();
        }
        assert_eq!(dir.open_files(), 0);
        let path = all.remove(0).finish().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcd");
        assert_eq!(dir.open_files(), 0);
    }

    #[test]
    fn a_replaced_reserved_file_is_not_written() {
        let tmp = TempDir::new("swapped");
        let staging = staging_in(&tmp);
        let hidden = staging.path();
        let mut p = staging.create("a.txt", 4).unwrap();
        std::fs::write(hidden.join("other"), b"theirs").unwrap();
        std::fs::rename(hidden.join("other"), hidden.join("a.txt")).unwrap();
        assert!(p.write_at(4, 0, b"EVIL").is_err());
        drop(p);
        assert_eq!(std::fs::read(hidden.join("a.txt")).unwrap(), b"theirs");
    }

    /// A folder left by a dead session (no lock held) is removed with what it holds; a
    /// symlink inside is removed, not followed.
    #[test]
    fn sweep_removes_an_unlocked_staging_dir() {
        let tmp = TempDir::new("sweep");
        let outside = TempDir::new("sweep-outside");
        let target = outside.0.join("target.txt");
        std::fs::write(&target, b"keep").unwrap();
        let stale = tmp.0.join(format!("{STAGING_PREFIX}0123456789abcdef"));
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("half.bin"), b"half").unwrap();
        std::os::unix::fs::symlink(&target, stale.join("link")).unwrap();
        std::fs::write(tmp.0.join("mine.txt"), b"mine").unwrap();
        backdate(&stale.join("half.bin"));
        backdate(&stale.join("link"));
        backdate(&stale);

        DestDir::open(&tmp.0).unwrap().sweep_stale();
        assert_eq!(names(&tmp.0), ["mine.txt"]);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    /// No lock proves nothing where locks do not reach (another machine, a file system
    /// without `flock`): a folder with anything changed recently is kept.
    #[test]
    fn sweep_keeps_an_unlocked_staging_dir_in_use() {
        let tmp = TempDir::new("sweep-recent");
        let stale = tmp.0.join(format!("{STAGING_PREFIX}0123456789abcdef"));
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("old.bin"), b"old").unwrap();
        std::fs::write(stale.join("growing.bin"), b"new").unwrap();
        backdate(&stale.join("old.bin"));
        backdate(&stale);
        DestDir::open(&tmp.0).unwrap().sweep_stale();
        assert!(stale.join("old.bin").exists(), "a folder in use was swept");

        // The folder itself changed recently (a file was just created in it).
        backdate(&stale.join("growing.bin"));
        std::fs::write(stale.join("just-made.bin"), b"").unwrap();
        backdate(&stale.join("just-made.bin"));
        DestDir::open(&tmp.0).unwrap().sweep_stale();
        assert!(stale.join("old.bin").exists(), "a folder in use was swept");
    }

    /// A user's own folder that only starts like a staging folder is never touched.
    #[test]
    fn sweep_needs_the_whole_staging_name() {
        let tmp = TempDir::new("sweep-name");
        for name in [
            "old",
            "0123456789ABCDEF",
            "0123456789abcde",
            "0123456789abcdef0",
        ] {
            let dir = tmp.0.join(format!("{STAGING_PREFIX}{name}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("mine.txt"), b"mine").unwrap();
            backdate(&dir.join("mine.txt"));
            backdate(&dir);
        }
        DestDir::open(&tmp.0).unwrap().sweep_stale();
        assert_eq!(
            names(&tmp.0).len(),
            4,
            "a folder that is not a staging folder was swept"
        );
    }

    /// Two files offered under one name and completed in the other order still get the
    /// paths announced at the accept.
    #[test]
    fn files_of_one_name_keep_their_planned_names_out_of_order() {
        let tmp = TempDir::new("planned");
        let dest = DestDir::open(&tmp.0).unwrap();
        let planned = dest.predict(&["a.txt".to_owned(), "a.txt".to_owned()]);
        assert_eq!(planned, ["a.txt", "a (1).txt"]);
        let staging = dest.staging().unwrap();
        let mut first = staging.create("a.txt", 1).unwrap();
        let mut second = staging.create("a.txt", 1).unwrap();
        first.plan(planned[0].clone());
        second.plan(planned[1].clone());
        second.write_at(1, 0, b"2").unwrap();
        first.write_at(1, 0, b"1").unwrap();
        assert_eq!(second.finish().unwrap(), tmp.0.join("a (1).txt"));
        assert_eq!(first.finish().unwrap(), tmp.0.join("a.txt"));
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"1");
    }

    #[test]
    fn sweep_keeps_a_locked_staging_dir() {
        let tmp = TempDir::new("sweep-locked");
        let dir = DestDir::open(&tmp.0).unwrap();
        let live = dir.staging().unwrap();
        let mut p = live.create("a.txt", 4).unwrap();
        p.write_at(4, 0, b"ab").unwrap();
        // Another session in the same folder sweeps before it makes its own.
        let other = dir.staging().unwrap();
        assert_ne!(live.path(), other.path());
        assert!(p.staged_path().exists(), "a live session was swept");
        p.write_at(4, 2, b"cd").unwrap();
        assert_eq!(std::fs::read(p.finish().unwrap()).unwrap(), b"abcd");
    }

    #[test]
    fn sweep_skips_files_and_symlinks_with_the_prefix() {
        let tmp = TempDir::new("sweep-skip");
        let outside = TempDir::new("sweep-skip-outside");
        std::fs::write(outside.0.join("inside.txt"), b"keep").unwrap();
        let file = tmp.0.join(format!("{STAGING_PREFIX}00000000000000f1"));
        let link = tmp.0.join(format!("{STAGING_PREFIX}00000000000000f2"));
        std::fs::write(&file, b"a file").unwrap();
        std::os::unix::fs::symlink(&outside.0, &link).unwrap();
        backdate(&file);
        backdate(&link);

        DestDir::open(&tmp.0).unwrap().sweep_stale();
        assert_eq!(std::fs::read(&file).unwrap(), b"a file");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(names(&outside.0), ["inside.txt"], "a symlink was followed");
    }

    #[test]
    fn open_existing_does_not_create_and_open_does() {
        let tmp = TempDir::new("missing");
        let missing = tmp.0.join("a/b");
        assert!(DestDir::open_existing(&missing).is_err());
        assert!(!missing.exists());
        assert!(DestDir::free_bytes_near(&missing).unwrap() > 0);
        let dir = DestDir::open(&missing).unwrap();
        assert!(missing.is_dir());
        assert!(dir.is_still_at_path());
        std::fs::remove_dir(&missing).unwrap();
        assert!(!dir.is_still_at_path());
    }

    #[test]
    fn joiners_are_allowed_in_names() {
        assert!(!unsafe_file_name("می\u{200C}خواهم.txt"));
        assert!(!unsafe_file_name("family \u{1F468}\u{200D}\u{1F469}.png"));
        assert!(unsafe_file_name("a\u{200B}b"));
    }

    /// A FIFO swapped in under a reserved name must not block (the open is non-blocking and
    /// the file is not a regular one).
    #[test]
    fn a_fifo_under_a_reserved_name_is_refused_without_blocking() {
        let tmp = TempDir::new("fifo");
        let staging = staging_in(&tmp);
        let hidden = staging.path();
        let mut p = staging.create("a", 4).unwrap();
        std::fs::remove_file(hidden.join("a")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            hidden.join("a"),
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(p.write_at(4, 0, b"x").is_err());
        drop(p);
        assert!(hidden.join("a").exists(), "a foreign entry was removed");
    }

    /// The trade-off of the same-device-same-size rule, written down as a test.
    #[test]
    fn a_regular_file_of_the_expected_size_on_the_same_device_is_tolerated() {
        let tmp = TempDir::new("tolerated");
        let staging = staging_in(&tmp);
        let hidden = staging.path();
        let mut p = staging.create("a.txt", 6).unwrap();
        p.write_at(6, 0, b"ab").unwrap();
        p.release();
        // Hard links keep each replaced inode in use, so the file system cannot hand its
        // number to the next file and make the check pass by coincidence.
        std::fs::hard_link(hidden.join("a.txt"), hidden.join("keep1")).unwrap();
        std::fs::write(hidden.join("other"), b"zz").unwrap();
        std::fs::rename(hidden.join("other"), hidden.join("a.txt")).unwrap();
        // Same size as the 2 bytes written: accepted (inode numbers are not stable everywhere).
        p.write_at(6, 2, b"cd").unwrap();
        // A different size is not.
        std::fs::hard_link(hidden.join("a.txt"), hidden.join("keep2")).unwrap();
        std::fs::write(hidden.join("other"), b"longer than that").unwrap();
        std::fs::rename(hidden.join("other"), hidden.join("a.txt")).unwrap();
        p.release();
        assert!(p.write_at(6, 4, b"ef").is_err());
    }

    #[test]
    fn finish_fails_if_the_entry_is_gone() {
        let tmp = TempDir::new("gone");
        let staging = staging_in(&tmp);
        let hidden = staging.path();
        let mut p = staging.create("a.txt", 2).unwrap();
        p.write_at(2, 0, b"ab").unwrap();
        std::fs::rename(hidden.join("a.txt"), hidden.join("moved.txt")).unwrap();
        assert!(p.finish().is_err());
        assert!(hidden.join("moved.txt").exists());
        assert!(
            !tmp.0.join("a.txt").exists(),
            "a missing file was published"
        );
    }

    #[test]
    fn free_bytes_is_reported() {
        let tmp = TempDir::new("free");
        let dir = DestDir::open(&tmp.0).unwrap();
        assert!(dir.free_bytes().unwrap() > 0);
    }

    fn header(id: i64, total: i64) -> PayloadHeader {
        PayloadHeader {
            id: Some(id),
            total_size: Some(total),
            ..Default::default()
        }
    }

    fn chunk(offset: i64, flags: i32, body: &[u8]) -> PayloadChunk {
        PayloadChunk {
            offset: Some(offset),
            flags: Some(flags),
            body: Some(body.to_vec()),
            index: None,
        }
    }

    #[test]
    fn assemble_returns_the_payload_once_and_forgets_it() {
        let l = Limits::default();
        let mut buffers = HashMap::new();
        assert!(
            assemble(&mut buffers, &header(1, 5), &chunk(0, 0, b"he"), &l)
                .unwrap()
                .is_none()
        );
        let done = assemble(&mut buffers, &header(1, 5), &chunk(2, 1, b"llo"), &l).unwrap();
        assert_eq!(done.as_deref(), Some(&b"hello"[..]));
        assert!(buffers.is_empty());
    }

    #[test]
    fn assemble_refuses_bad_sizes_and_offsets() {
        let l = Limits::default();
        let mut buffers = HashMap::new();
        assert!(matches!(
            assemble(&mut buffers, &header(1, -1), &chunk(0, 0, b"x"), &l),
            Err(ChunkError::BadSize(-1))
        ));
        let too_big = l.max_bytes_payload as i64 + 1;
        assert!(matches!(
            assemble(&mut buffers, &header(1, too_big), &chunk(0, 0, b"x"), &l),
            Err(ChunkError::BadSize(_))
        ));
        assemble(&mut buffers, &header(2, 10), &chunk(0, 0, &[1; 8]), &l).unwrap();
        assert!(matches!(
            assemble(&mut buffers, &header(2, 10), &chunk(8, 0, &[2; 8]), &l),
            Err(ChunkError::TooBig { .. })
        ));
        assert!(buffers.is_empty(), "a refused payload was kept");
        assemble(&mut buffers, &header(3, 10), &chunk(0, 0, b"ab"), &l).unwrap();
        assert!(matches!(
            assemble(&mut buffers, &header(3, 10), &chunk(5, 0, b"cd"), &l),
            Err(ChunkError::Offset { .. })
        ));
        assert!(buffers.is_empty());
    }

    /// The first chunk fixes the ceiling: a later chunk cannot raise it (or lower it).
    #[test]
    fn assemble_refuses_a_changed_total_size() {
        let l = Limits::default();
        let mut buffers = HashMap::new();
        assemble(&mut buffers, &header(1, 10), &chunk(0, 0, b"ab"), &l).unwrap();
        assert!(matches!(
            assemble(&mut buffers, &header(1, 20), &chunk(2, 0, b"cd"), &l),
            Err(ChunkError::SizeChanged {
                got: 20,
                declared: 10
            })
        ));
        assert!(buffers.is_empty(), "a refused payload was kept");
        assemble(&mut buffers, &header(2, 10), &chunk(0, 0, b"ab"), &l).unwrap();
        assert!(matches!(
            assemble(&mut buffers, &header(2, 2), &chunk(2, 1, b""), &l),
            Err(ChunkError::SizeChanged { .. })
        ));
        assert!(buffers.is_empty());
    }

    #[test]
    fn assemble_refuses_a_last_chunk_before_the_declared_size() {
        let l = Limits::default();
        let mut buffers = HashMap::new();
        assert!(matches!(
            assemble(&mut buffers, &header(1, 10), &chunk(0, 1, b"abc"), &l),
            Err(ChunkError::Short {
                got: 3,
                declared: 10
            })
        ));
        assert!(buffers.is_empty());
    }

    #[test]
    fn assemble_limits_open_payloads() {
        let l = Limits {
            max_open_payloads: 2,
            ..Limits::default()
        };
        let mut buffers = HashMap::new();
        for id in 1..=2 {
            assemble(&mut buffers, &header(id, 10), &chunk(0, 0, b"a"), &l).unwrap();
        }
        assert!(matches!(
            assemble(&mut buffers, &header(3, 10), &chunk(0, 0, b"a"), &l),
            Err(ChunkError::TooManyPayloads)
        ));
        // A payload that is already open still takes chunks.
        assemble(&mut buffers, &header(1, 10), &chunk(1, 0, b"b"), &l).unwrap();
    }

    fn meta(name: &str, id: i64, size: i64) -> FileMetadata {
        FileMetadata {
            name: Some(name.into()),
            payload_id: Some(id),
            size: Some(size),
            mime_type: Some("text/plain".into()),
            ..Default::default()
        }
    }

    #[test]
    fn check_offer_refuses_what_it_must() {
        let l = Limits {
            max_files: 3,
            ..Limits::default()
        };
        let ok = check_offer(&[meta("a.txt", 1, 5), meta("../b.txt", 2, 0)], &l).unwrap();
        assert_eq!(ok[1].name, "b.txt", "only the last component is kept");
        assert_eq!(ok[0].size, 5);

        let many: Vec<FileMetadata> = (0..4).map(|i| meta("a.txt", i, 1)).collect();
        assert_eq!(check_offer(&many, &l), Err(Refusal::TooManyFiles));
        assert_eq!(
            check_offer(&[meta("a.txt", 1, -1)], &l),
            Err(Refusal::BadSize)
        );
        assert_eq!(
            check_offer(
                &[
                    meta("a.txt", 1, i64::MAX),
                    meta("b.txt", 2, i64::MAX),
                    meta("c.txt", 3, 2)
                ],
                &l
            ),
            Err(Refusal::BadSize),
            "the sum overflows u64"
        );
        assert_eq!(
            check_offer(&[meta("a.txt", 1, 1), meta("b.txt", 1, 1)], &l),
            Err(Refusal::DuplicateId)
        );
        assert_eq!(
            check_offer(&[meta("pho\u{202E}to.txt", 1, 1)], &l),
            Err(Refusal::BadName)
        );
        // Nothing usable left of the name: the fallback is a plain name.
        assert_eq!(
            check_offer(&[meta("..", 1, 1)], &l).unwrap()[0].name,
            "file"
        );
    }

    #[test]
    fn refusals_use_existing_statuses() {
        assert_eq!(
            Refusal::NotEnoughSpace { need: 2, free: 1 }.status(),
            Status::NotEnoughSpace
        );
        for r in [
            Refusal::BadName,
            Refusal::TooManyFiles,
            Refusal::BadSize,
            Refusal::DuplicateId,
        ] {
            assert_eq!(r.status(), Status::UnsupportedAttachmentType);
        }
    }
}
