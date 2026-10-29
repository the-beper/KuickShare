//! The one place that decides what an unknown sender may make us do: which names it may
//! write, how many files and bytes it may offer, where the bytes land, and when a transfer
//! counts as finished.
//!
//! Files are created with dir-fd relative, exclusive operations (`openat` with
//! `O_CREAT | O_EXCL | O_NOFOLLOW`), never "check a name, then open it". A [`Partial`] that is
//! not finished removes its file when it is dropped, so every way out of a session (cancel,
//! decline, error, EOF, timeout, panic) cleans up.
//!
//! Only SIGKILL and power loss can leave a partial file behind.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, Stat, fstat, fstatvfs, open, openat, stat, statat, unlinkat,
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

    /// Reserves a new, empty file for `declared` bytes, as `name` or the first free
    /// `stem (N).ext`. Never opens an existing entry, never follows a symlink. The file is not
    /// kept open: the descriptor only exists while data is written (`Partial::write_at`), so
    /// an offer of thousands of files does not use thousands of descriptors.
    pub fn create(self: &Arc<Self>, name: &str, declared: u64) -> io::Result<Partial> {
        if unsafe_file_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain file name",
            ));
        }
        let flags = OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::NOCTTY
            | OFlags::CLOEXEC;
        for n in 0..=MAX_SUFFIX {
            let cand = candidate(name, n);
            match openat(&self.fd, cand.as_str(), flags, FILE_MODE) {
                Ok(fd) => {
                    let file = File::from(fd);
                    let ident = match fstat(&file) {
                        Ok(st) => ident(&st),
                        Err(e) => {
                            let _ = unlinkat(&self.fd, cand.as_str(), AtFlags::empty());
                            return Err(e.into());
                        }
                    };
                    return Ok(Partial {
                        dir: Arc::clone(self),
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
}

/// Why a chunk was refused. Every variant ends the session.
#[derive(Debug)]
pub enum ChunkError {
    /// A `Bytes` payload with a negative size or one above the limit.
    BadSize(i64),
    TooManyPayloads,
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

/// A file being received. Its name is reserved from the start (an empty file); the data is
/// written through a descriptor opened on the first chunk. Dropped before `finish`, it
/// removes its file (only if the entry still is the one this created).
#[derive(Debug)]
pub struct Partial {
    dir: Arc<DestDir>,
    name: String,
    file: Option<File>,
    ident: (u64, u64),
    declared: u64,
    written: u64,
    done: bool,
}

impl Partial {
    /// Where the file is (the name may carry a ` (N)` suffix).
    pub fn path(&self) -> PathBuf {
        self.dir.path.join(&self.name)
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Files of this folder open for writing right now.
    pub fn open_files(&self) -> usize {
        self.dir.open_files()
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
                &self.dir.fd,
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
            self.dir.open_files.fetch_add(1, Ordering::Relaxed);
            self.file = Some(file);
        }
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("file not open"))
    }

    /// Closes the descriptor; the data written so far stays. Reopened by the next write.
    pub fn release(&mut self) {
        if self.file.take().is_some() {
            self.dir.open_files.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Writes `data` at `offset`, which must be where the previous chunk ended, without going
    /// past the declared size.
    pub fn write_at(&mut self, offset: i64, data: &[u8]) -> Result<(), ChunkError> {
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

    /// Completes the file: all declared bytes are there and on disk. Blocking.
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
        // The name must still lead to the file that was written, or the path reported would
        // point at something else.
        let entry = statat(&self.dir.fd, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW)?;
        if !self.is_reserved(&entry, self.declared) {
            return Err(io::Error::other(
                "the file was replaced before it was completed",
            ));
        }
        self.done = true;
        Ok(self.path())
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
        if let Ok(st) = statat(&self.dir.fd, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            && self.is_reserved(&st, self.written)
        {
            let _ = unlinkat(&self.dir.fd, self.name.as_str(), AtFlags::empty());
        }
    }
}

/// Adds one `Bytes` chunk to `buffers`. Returns the payload once the last chunk is in (and
/// forgets it); `None` while more is expected. Memory follows the bytes received, never the
/// size the peer declares, and the declared size is a ceiling on them. A refused payload is
/// forgotten too.
pub fn assemble(
    buffers: &mut HashMap<i64, Vec<u8>>,
    header: &PayloadHeader,
    chunk: &PayloadChunk,
    l: &Limits,
) -> Result<Option<Vec<u8>>, ChunkError> {
    let id = header.id();
    let declared = header.total_size();
    let ceiling = match usize::try_from(declared) {
        Ok(n) if n <= l.max_bytes_payload => n,
        _ => {
            buffers.remove(&id);
            return Err(ChunkError::BadSize(declared));
        }
    };
    if !buffers.contains_key(&id) && buffers.len() >= l.max_open_payloads {
        return Err(ChunkError::TooManyPayloads);
    }
    let buffer = buffers.entry(id).or_default();
    if u64::try_from(chunk.offset()) != Ok(buffer.len() as u64) {
        let err = ChunkError::Offset {
            got: chunk.offset(),
            expected: buffer.len() as u64,
        };
        buffers.remove(&id);
        return Err(err);
    }
    let body = chunk.body();
    if buffer.len().saturating_add(body.len()) > ceiling {
        buffers.remove(&id);
        return Err(ChunkError::TooBig {
            declared: ceiling as u64,
        });
    }
    buffer.extend_from_slice(body);
    if chunk.flags() & 1 == 1 {
        let got = buffer.len();
        if got != ceiling {
            buffers.remove(&id);
            return Err(ChunkError::Short {
                got: got as u64,
                declared: ceiling as u64,
            });
        }
        return Ok(buffers.remove(&id));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn create_numbers_from_original_name() {
        let tmp = TempDir::new("number");
        let dir = DestDir::open(&tmp.0).unwrap();
        std::fs::write(tmp.0.join("a.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(tmp.0.join("gone"), tmp.0.join("a (1).txt")).unwrap();

        let first = dir.create("a.txt", 1).unwrap();
        assert_eq!(first.path(), tmp.0.join("a (2).txt"));
        let second = dir.create("a.txt", 1).unwrap();
        assert_eq!(second.path(), tmp.0.join("a (3).txt"));
        // A name that already looks numbered is numbered from itself.
        let third = dir.create("a (2).txt", 1).unwrap();
        assert_eq!(third.path(), tmp.0.join("a (2) (1).txt"));
        let bare = dir.create("noext", 1).unwrap();
        let again = dir.create("noext", 1).unwrap();
        assert_eq!(bare.path(), tmp.0.join("noext"));
        assert_eq!(again.path(), tmp.0.join("noext (1)"));
        assert!(
            !tmp.0.join("gone").exists(),
            "a dangling symlink was followed"
        );
    }

    #[test]
    fn predict_matches_create() {
        let tmp = TempDir::new("predict");
        std::fs::write(tmp.0.join("a.txt"), b"x").unwrap();
        let dir = DestDir::open(&tmp.0).unwrap();
        let wanted: Vec<String> = ["a.txt", "a.txt", "b.txt", "a.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let predicted = dir.predict(&wanted);
        // The files are kept until the end: a dropped `Partial` removes its file.
        let partials: Vec<Partial> = wanted.iter().map(|n| dir.create(n, 0).unwrap()).collect();
        let created: Vec<String> = partials
            .iter()
            .map(|p| p.path().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(predicted, created);
        assert_eq!(predicted, ["a (1).txt", "a (2).txt", "b.txt", "a (3).txt"]);
    }

    #[test]
    fn create_fits_name_max() {
        let tmp = TempDir::new("max");
        let dir = DestDir::open(&tmp.0).unwrap();
        // 250 bytes of 2-byte characters plus `.jpg`: the suffix does not fit without a cut,
        // which must fall on a character boundary.
        let stem = "é".repeat(125);
        let name = format!("{stem}.jpg");
        assert_eq!(name.len(), 254);
        assert!(!unsafe_file_name(&name));
        let first = dir.create(&name, 1).unwrap();
        let second = dir.create(&name, 1).unwrap();
        let file_name = second
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(file_name.len() <= MAX_NAME_BYTES, "{}", file_name.len());
        assert!(file_name.ends_with(" (1).jpg"), "{file_name}");
        assert_ne!(first.path(), second.path());
        // A name of exactly the maximum length is numbered too.
        let full = "x".repeat(MAX_NAME_BYTES);
        let _a = dir.create(&full, 1).unwrap();
        let b = dir.create(&full, 1).unwrap();
        assert!(b.path().file_name().unwrap().len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn create_refuses_paths() {
        let tmp = TempDir::new("paths");
        let dir = DestDir::open(&tmp.0).unwrap();
        for bad in ["../x", "a/b", "", "..", "."] {
            assert!(dir.create(bad, 1).is_err(), "{bad:?}");
        }
        assert!(names(&tmp.0).is_empty());
    }

    #[test]
    fn finish_requires_declared_size() {
        let tmp = TempDir::new("finish");
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut short = dir.create("short.txt", 10).unwrap();
        short.write_at(0, b"12345").unwrap();
        assert!(short.finish().is_err());
        assert!(!tmp.0.join("short.txt").exists(), "a short file stayed");

        let mut full = dir.create("full.txt", 5).unwrap();
        full.write_at(0, b"123").unwrap();
        full.write_at(3, b"45").unwrap();
        let path = full.finish().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"12345");
    }

    #[test]
    fn write_at_checks_offset_and_size() {
        let tmp = TempDir::new("write");
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut p = dir.create("a", 4).unwrap();
        assert!(matches!(
            p.write_at(1, b"x"),
            Err(ChunkError::Offset { .. })
        ));
        assert!(matches!(
            p.write_at(-1, b"x"),
            Err(ChunkError::Offset { .. })
        ));
        p.write_at(0, b"ab").unwrap();
        assert!(matches!(
            p.write_at(2, b"cdef"),
            Err(ChunkError::TooBig { .. })
        ));
        assert_eq!(p.written(), 2);
    }

    #[test]
    fn drop_removes_an_unfinished_file() {
        let tmp = TempDir::new("drop");
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut p = dir.create("a.txt", 4).unwrap();
        p.write_at(0, b"ab").unwrap();
        assert_eq!(names(&tmp.0), ["a.txt"]);
        drop(p);
        assert!(names(&tmp.0).is_empty());
    }

    #[test]
    fn drop_does_not_unlink_a_replaced_entry() {
        let tmp = TempDir::new("replaced");
        let dir = DestDir::open(&tmp.0).unwrap();
        let p = dir.create("a.txt", 4).unwrap();
        // Another program swaps the entry (the old inode stays alive: `p` holds it open).
        std::fs::write(tmp.0.join("other"), b"theirs").unwrap();
        std::fs::rename(tmp.0.join("other"), tmp.0.join("a.txt")).unwrap();
        drop(p);
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"theirs");
    }

    #[test]
    fn fd_count_stays_bounded_for_many_files() {
        let tmp = TempDir::new("fds");
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut all: Vec<Partial> = (0..1500)
            .map(|i| dir.create(&format!("f{i}.txt"), 4).unwrap())
            .collect();
        assert_eq!(dir.open_files(), 0, "a reserved file holds no descriptor");
        // One file at a time is written and released, as the session does.
        for p in &mut all {
            p.write_at(0, b"abcd").unwrap();
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
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut p = dir.create("a.txt", 4).unwrap();
        std::fs::write(tmp.0.join("other"), b"theirs").unwrap();
        std::fs::rename(tmp.0.join("other"), tmp.0.join("a.txt")).unwrap();
        assert!(p.write_at(0, b"EVIL").is_err());
        drop(p);
        assert_eq!(std::fs::read(tmp.0.join("a.txt")).unwrap(), b"theirs");
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
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut p = dir.create("a", 4).unwrap();
        std::fs::remove_file(tmp.0.join("a")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            tmp.0.join("a"),
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(p.write_at(0, b"x").is_err());
        drop(p);
        assert!(tmp.0.join("a").exists(), "a foreign entry was removed");
    }

    /// The trade-off of the same-device-same-size rule, written down as a test.
    #[test]
    fn a_regular_file_of_the_expected_size_on_the_same_device_is_tolerated() {
        let tmp = TempDir::new("tolerated");
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut p = dir.create("a.txt", 6).unwrap();
        p.write_at(0, b"ab").unwrap();
        p.release();
        // Hard links keep each replaced inode in use, so the file system cannot hand its
        // number to the next file and make the check pass by coincidence.
        std::fs::hard_link(tmp.0.join("a.txt"), tmp.0.join("keep1")).unwrap();
        std::fs::write(tmp.0.join("other"), b"zz").unwrap();
        std::fs::rename(tmp.0.join("other"), tmp.0.join("a.txt")).unwrap();
        // Same size as the 2 bytes written: accepted (inode numbers are not stable everywhere).
        p.write_at(2, b"cd").unwrap();
        // A different size is not.
        std::fs::hard_link(tmp.0.join("a.txt"), tmp.0.join("keep2")).unwrap();
        std::fs::write(tmp.0.join("other"), b"longer than that").unwrap();
        std::fs::rename(tmp.0.join("other"), tmp.0.join("a.txt")).unwrap();
        p.release();
        assert!(p.write_at(4, b"ef").is_err());
    }

    #[test]
    fn finish_fails_if_the_entry_is_gone() {
        let tmp = TempDir::new("gone");
        let dir = DestDir::open(&tmp.0).unwrap();
        let mut p = dir.create("a.txt", 2).unwrap();
        p.write_at(0, b"ab").unwrap();
        std::fs::rename(tmp.0.join("a.txt"), tmp.0.join("moved.txt")).unwrap();
        assert!(p.finish().is_err());
        assert!(tmp.0.join("moved.txt").exists());
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
