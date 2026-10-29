//! The one way the core writes its files (identity, trusted computers, settings, history).
//!
//! `write_atomic` never leaves a half-written target: the bytes go to a uniquely named temporary
//! file next to the target (created exclusively, with the final mode from the start), are synced to
//! disk, and only then renamed over the target. On any error the previous target is untouched and
//! the temporary file is removed. The rename is atomic because both names are in the same
//! directory, hence on the same file system.
//!
//! Callers that can be reached from several threads serialize "build the bytes, then write" with
//! one lock of their own, otherwise an older snapshot could be renamed over a newer one (see
//! `Trust::save_peers`); the helper itself only guarantees that two writes never share a temporary
//! file and that readers see one complete file or the other.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write as _},
    os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Mode of every file that holds a key, names or addresses of the user's devices.
pub const PRIVATE: u32 = 0o600;

/// Files this run must not write: they could not be read and could not be moved aside, so the
/// store holds defaults and a save would destroy what the user has. Saves are skipped (the
/// settings keep working in memory).
static BLOCKED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// How many `<file>.bak.N` backups are kept next to `<file>.bak` before the move is refused.
const MAX_BACKUPS: u32 = 99;

/// Length of the random part of a temporary name (hex digits of a `u64`).
const SUFFIX_LEN: usize = 16;

/// Tries with a fresh name when the random name is taken (it practically never is).
const NAME_TRIES: usize = 8;

/// Moves `path` to `<file>.bak`, or to the first free `<file>.bak.1`, `.2`, ... when that exists:
/// an earlier backup is never overwritten. Returns where the file went.
pub fn keep_aside(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let backup = |suffix: String| {
        let mut file = name.to_owned();
        file.push(suffix);
        path.with_file_name(file)
    };
    let free =
        |p: &Path| matches!(p.symlink_metadata(), Err(e) if e.kind() == io::ErrorKind::NotFound);
    let target = std::iter::once(backup(".bak".to_owned()))
        .chain((1..=MAX_BACKUPS).map(|n| backup(format!(".bak.{n}"))))
        .find(|p| free(p))
        .ok_or_else(|| io::Error::other("too many backups"))?;
    fs::rename(path, &target)?;
    Ok(target)
}

/// For a file that could not be used (unreadable or invalid): moves it aside so the next save does
/// not destroy it. When even that fails the file is blocked for this run (`is_blocked`). Logs.
pub fn set_aside_or_block(path: &Path) {
    match keep_aside(path) {
        Ok(to) => tracing::warn!("{} kept as {}", path.display(), to.display()),
        Err(e) => {
            tracing::warn!(
                "cannot keep {} aside ({e}): it is not overwritten while this runs",
                path.display()
            );
            BLOCKED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.to_path_buf());
        }
    }
}

/// True when saves to `path` must be skipped (see `set_aside_or_block`).
pub fn is_blocked(path: &Path) -> bool {
    BLOCKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(path)
}

/// Writes `bytes` to `path` so that a reader (or a crash) sees the old file or the new one, never a
/// mix. The file gets `mode` (before the umask) when it is created, also when it replaces a file
/// that had another mode. The directory must exist.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };

    let (tmp, file) = create_temp(dir, name, mode)?;
    let written = write_and_sync(file, bytes).and_then(|()| fs::rename(&tmp, path));
    if let Err(e) = written {
        // Only the file this call created; the target is left as it was.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename is only durable once the directory entry is on disk. The data is already
    // consistent without it, so a failure here is not an error of the write.
    if let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::debug!("cannot sync directory {}: {e}", dir.display());
    }
    Ok(())
}

fn write_and_sync(mut file: fs::File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

/// Creates `.<name>.<16 hex digits>.tmp` in `dir`, exclusively and with `mode`.
fn create_temp(dir: &Path, name: &OsStr, mode: u32) -> io::Result<(PathBuf, fs::File)> {
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..NAME_TRIES {
        let tmp = dir.join(temp_name(name, rand::random()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn temp_name(name: &OsStr, random: u64) -> OsString {
    let mut tmp = OsString::from(".");
    tmp.push(name);
    tmp.push(format!(".{random:0width$x}.tmp", width = SUFFIX_LEN));
    tmp
}

/// Whether `candidate` is a temporary name `write_atomic` makes for `name`, or the single
/// `<name>.tmp` that older versions used.
fn is_own_temp(candidate: &OsStr, name: &OsStr) -> bool {
    let (candidate, name) = (candidate.as_bytes(), name.as_bytes());
    if candidate.strip_prefix(name) == Some(&b".tmp"[..]) {
        return true;
    }
    let Some(rest) = candidate
        .strip_prefix(b".")
        .and_then(|c| c.strip_prefix(name))
        .and_then(|c| c.strip_prefix(b"."))
        .and_then(|c| c.strip_suffix(b".tmp"))
    else {
        return false;
    };
    rest.len() == SUFFIX_LEN && rest.iter().all(u8::is_ascii_hexdigit)
}

/// Removes the temporary files a crash left next to `path`. Only regular files with exactly our
/// temporary name for this target go; `.bak` files and everything else stay. Call it once at
/// start, before the first write: a temporary file of a write in progress would go too.
pub fn remove_stale_temps(path: &Path) {
    let (Some(name), Some(dir)) = (path.file_name(), path.parent()) else {
        return;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !is_own_temp(&entry.file_name(), name) {
            continue;
        }
        // `file_type` does not follow symbolic links: a link with our name is not ours to follow.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => tracing::info!("removed leftover {}", entry.path().display()),
            Err(e) => tracing::warn!("cannot remove leftover {}: {e}", entry.path().display()),
        }
    }
}

/// Helpers for the tests of the modules that write through `write_atomic`.
#[cfg(test)]
pub(crate) mod testutil {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
    };

    /// A fresh directory under the system temp dir, removed when dropped.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "kuickshare-persist-{name}-{}-{:x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // A test may have made the directory read-only.
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Sorted names of the entries of `dir`.
    pub fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Permission bits of `path`.
    pub fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt as _,
        sync::{Arc, Barrier},
        thread,
    };

    use super::{
        testutil::{TempDir, mode_of, names},
        *,
    };

    #[test]
    fn writes_and_replaces() {
        let dir = TempDir::new("replace");
        let path = dir.0.join("data.json");
        write_atomic(&path, b"one", PRIVATE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one");
        write_atomic(&path, b"two, longer", PRIVATE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two, longer");
        write_atomic(&path, b"3", PRIVATE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"3");
        assert_eq!(names(&dir.0), ["data.json"], "no temporary file is left");
    }

    #[test]
    fn mode_is_private_on_create_and_on_replace() {
        let dir = TempDir::new("mode");
        let path = dir.0.join("data.json");
        write_atomic(&path, b"x", PRIVATE).unwrap();
        assert_eq!(mode_of(&path), 0o600);

        let old = dir.0.join("old.json");
        fs::write(&old, "old").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&old, b"new", PRIVATE).unwrap();
        assert_eq!(
            mode_of(&old),
            0o600,
            "a world-readable file becomes private"
        );
        assert_eq!(fs::read(&old).unwrap(), b"new");
    }

    #[test]
    fn a_parent_that_is_a_file_fails_cleanly() {
        let dir = TempDir::new("parent-file");
        let blocker = dir.0.join("blocker");
        fs::write(&blocker, "i am a file").unwrap();
        assert!(write_atomic(&blocker.join("data.json"), b"x", PRIVATE).is_err());
        assert_eq!(fs::read(&blocker).unwrap(), b"i am a file");
        assert_eq!(names(&dir.0), ["blocker"]);
    }

    #[test]
    fn a_failing_rename_removes_the_temp_file_and_keeps_the_target() {
        let dir = TempDir::new("rename-fails");
        // A directory with content cannot be replaced by a file: the rename fails after the
        // temporary file was written.
        let target = dir.0.join("data.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside"), "kept").unwrap();
        assert!(write_atomic(&target, b"x", PRIVATE).is_err());
        assert_eq!(fs::read(target.join("inside")).unwrap(), b"kept");
        assert_eq!(names(&dir.0), ["data.json"]);
    }

    #[test]
    fn a_read_only_directory_keeps_the_old_file() {
        let dir = TempDir::new("read-only");
        let path = dir.0.join("data.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_atomic(&path, b"new", PRIVATE);
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        if result.is_ok() {
            // Running as root: the mode bits do not apply, nothing to check.
            return;
        }
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(names(&dir.0), ["data.json"]);
    }

    #[test]
    fn a_path_without_a_file_name_is_refused() {
        let error = write_atomic(Path::new("/"), b"x", PRIVATE).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn temp_names_are_recognised_exactly() {
        let name = OsStr::new("peers.json");
        let own = temp_name(name, 0xdead_beef);
        assert!(is_own_temp(&own, name));
        assert!(is_own_temp(OsStr::new("peers.json.tmp"), name), "old name");
        for other in [
            "peers.json",
            "peers.json.bak",
            ".peers.json.bak",
            ".peers.json.tmp",
            ".peers.json.xyz.tmp",
            ".peers.json.00000000deadbeef",
            ".peers.json.00000000deadbeeg.tmp",
            ".peers.json.0000000deadbeef.tmp",
            ".settings.json.00000000deadbeef.tmp",
            "notes.tmp",
        ] {
            assert!(!is_own_temp(OsStr::new(other), name), "{other}");
        }
    }

    #[test]
    fn stale_temps_go_but_backups_and_other_files_stay() {
        let dir = TempDir::new("stale");
        let target = dir.0.join("settings.json");
        fs::write(&target, "{}").unwrap();
        // What a crash leaves: a temp of ours (new and old naming).
        let stale = dir.0.join(temp_name(OsStr::new("settings.json"), 42));
        fs::write(&stale, "half").unwrap();
        fs::write(dir.0.join("settings.json.tmp"), "old version").unwrap();
        // What must stay.
        for keep in [
            "settings.json.bak",
            "settings.json.tmp.bak",
            "other.json",
            "notes.tmp",
        ] {
            fs::write(dir.0.join(keep), "keep").unwrap();
        }
        let other_temp = dir.0.join(temp_name(OsStr::new("history.json"), 7));
        fs::write(&other_temp, "of another target").unwrap();
        // A directory with our temp name is not a file of ours.
        let dir_named = dir.0.join(temp_name(OsStr::new("settings.json"), 43));
        fs::create_dir(&dir_named).unwrap();

        remove_stale_temps(&target);

        assert!(!stale.exists());
        assert!(!dir.0.join("settings.json.tmp").exists());
        assert!(dir_named.is_dir());
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        for keep in [
            "settings.json.bak",
            "settings.json.tmp.bak",
            "other.json",
            "notes.tmp",
        ] {
            assert!(dir.0.join(keep).exists(), "{keep} must stay");
        }
        assert!(other_temp.exists());
    }

    #[test]
    fn removing_stale_temps_of_a_missing_directory_is_harmless() {
        remove_stale_temps(Path::new("/nonexistent-kuickshare-dir/data.json"));
        remove_stale_temps(Path::new("/"));
    }

    #[test]
    fn concurrent_writes_leave_one_complete_file() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 40;
        let dir = TempDir::new("concurrent");
        let path = Arc::new(dir.0.join("data.json"));
        let start = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let (path, start) = (path.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    for round in 0..ROUNDS {
                        // Every payload is valid JSON of its own, of a different length.
                        let len = 1 + (t * ROUNDS + round) % 50;
                        let payload = serde_json::to_vec(&vec![t; len]).unwrap();
                        write_atomic(&path, &payload, PRIVATE).unwrap();
                        // Whenever it is read, the file is one complete payload.
                        let read = fs::read(&*path).unwrap();
                        let numbers: Vec<usize> = serde_json::from_slice(&read).unwrap();
                        assert!(numbers.iter().all(|n| *n == numbers[0]));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(names(&dir.0), ["data.json"], "every temp file was renamed");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn keep_aside_never_overwrites_a_backup() {
        let dir = TempDir::new("aside");
        let path = dir.0.join("data.json");
        for (n, expected) in ["data.json.bak", "data.json.bak.1", "data.json.bak.2"]
            .into_iter()
            .enumerate()
        {
            fs::write(&path, format!("content {n}")).unwrap();
            assert_eq!(keep_aside(&path).unwrap(), dir.0.join(expected));
        }
        assert_eq!(
            names(&dir.0),
            ["data.json.bak", "data.json.bak.1", "data.json.bak.2"]
        );
        let first = fs::read_to_string(dir.0.join("data.json.bak")).unwrap();
        assert_eq!(first, "content 0");
        let last = fs::read_to_string(dir.0.join("data.json.bak.2")).unwrap();
        assert_eq!(last, "content 2");
    }

    #[test]
    fn a_file_that_cannot_be_kept_is_blocked() {
        let dir = TempDir::new("blocked");
        let missing = dir.0.join("absent.json");
        assert!(!is_blocked(&missing));
        set_aside_or_block(&missing);
        assert!(is_blocked(&missing));
        assert!(!is_blocked(&dir.0.join("other.json")));
    }
}
