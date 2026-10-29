//! Persistent list of finished transfers (newest first), stored as JSON under the XDG data dir.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::persist::{self, PRIVATE};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    /// Identifies the entry (Unix time in milliseconds, made unique); 0 only before it is added.
    pub uid: u64,
    /// Unix time in seconds when the transfer ended.
    pub time: i64,
    /// "inbound" or "outbound".
    pub kind: String,
    /// Final state: Finished, Cancelled, Rejected or Disconnected.
    pub state: String,
    pub device: String,
    /// Files, Text, Url or WiFi (empty if unknown).
    pub payload_kind: String,
    pub files: Vec<String>,
    /// Text or link, or the network name for Wi-Fi credentials (never the password).
    pub text: String,
    pub total_bytes: u64,
    /// Accepted without asking: the sender is a paired computer or an always-accepted device.
    pub trusted: bool,
}

pub fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("kuickshare").join("history.json"))
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The history keeps at most this many entries, whatever the retention period.
const MAX_ENTRIES: usize = 1000;

pub struct History {
    path: Option<PathBuf>,
    entries: Vec<HistoryEntry>,
    /// False: finished transfers are not recorded.
    enabled: bool,
    /// Entries older than this many days are dropped (0 keeps them for ever).
    keep_days: u32,
}

impl History {
    /// A missing or unreadable file yields an empty history; the problem is logged, not fatal.
    pub fn load(path: Option<PathBuf>) -> Self {
        if let Some(path) = path.as_deref() {
            persist::remove_stale_temps(path);
        }
        let mut entries: Vec<HistoryEntry> = path.as_deref().map(read).unwrap_or_default();
        // Entries saved before `uid` existed get one derived from their time.
        for (i, entry) in entries.iter_mut().enumerate().filter(|(_, e)| e.uid == 0) {
            entry.uid = entry.time.max(0) as u64 * 1000 + i as u64;
        }
        Self {
            path,
            entries,
            enabled: true,
            keep_days: 0,
        }
    }

    pub fn set_policy(&mut self, enabled: bool, keep_days: u32) {
        self.enabled = enabled;
        self.keep_days = keep_days;
        if self.prune() {
            self.save();
        }
    }

    /// Records a finished transfer and returns its `uid` (0 if the history is turned off).
    pub fn add(&mut self, mut entry: HistoryEntry) -> u64 {
        if !self.enabled {
            return 0;
        }
        let newest = self.entries.iter().map(|e| e.uid).max().unwrap_or(0);
        entry.uid = (now() as u64 * 1000).max(newest + 1);
        let uid = entry.uid;
        self.entries.insert(0, entry);
        self.prune();
        self.save();
        uid
    }

    /// Size of the history file on disk, in bytes (0 if it was never written).
    pub fn file_size(&self) -> u64 {
        self.path
            .as_deref()
            .and_then(|p| fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    }

    pub fn remove(&mut self, uid: u64) {
        self.entries.retain(|e| e.uid != uid);
        self.save();
    }

    /// Drops the entries beyond `MAX_ENTRIES` and those older than the retention period; true if
    /// any was dropped.
    fn prune(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.truncate(MAX_ENTRIES); // newest first
        if self.keep_days > 0 {
            let limit = now() - i64::from(self.keep_days) * 86_400;
            self.entries.retain(|e| e.time >= limit);
        }
        self.entries.len() != before
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    /// The entries as JSON; old ones are dropped first, so the retention also applies to a
    /// running daemon.
    pub fn json(&mut self) -> String {
        if self.prune() {
            self.save();
        }
        serde_json::to_string(&self.entries).unwrap_or_else(|_| "[]".to_owned())
    }

    fn save(&self) {
        if let Some(path) = &self.path
            && !persist::is_blocked(path)
            && let Err(e) = write(path, &self.entries)
        {
            tracing::warn!("cannot save history: {e:#}");
        }
    }
}

fn read(path: &Path) -> Vec<HistoryEntry> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring invalid history file {}: {e}", path.display());
            // The next save would overwrite it.
            persist::set_aside_or_block(path);
            Vec::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!("cannot read history file {}: {e}", path.display());
            persist::set_aside_or_block(path);
            Vec::new()
        }
    }
}

fn write(path: &Path, entries: &[HistoryEntry]) -> Result<()> {
    let dir = path.parent().context("history path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // File names and device names: private. `History` is behind one lock in `Core`, so saves are
    // serialized by the owner of the entries.
    persist::write_atomic(path, &serde_json::to_vec(entries)?, PRIVATE)
        .with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt as _,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::persist::testutil::{TempDir, mode_of, names};

    fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kuickshare-hist-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("history.json")
    }

    fn entry(device: &str) -> HistoryEntry {
        HistoryEntry {
            device: device.into(),
            ..Default::default()
        }
    }

    #[test]
    fn newest_first_and_survives_a_restart() {
        let path = temp_path("restart");
        let mut history = History::load(Some(path.clone()));
        history.add(entry("old"));
        history.add(entry("new"));

        let reloaded = History::load(Some(path));
        let devices: Vec<_> = reloaded.entries.iter().map(|e| e.device.as_str()).collect();
        assert_eq!(devices, ["new", "old"]);
    }

    #[test]
    fn keeps_only_the_newest_max_entries() {
        let mut history = History::load(None);
        for i in 0..=MAX_ENTRIES {
            history.add(entry(&i.to_string()));
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.entries[0].device, MAX_ENTRIES.to_string());
        assert_eq!(history.entries[MAX_ENTRIES - 1].device, "1");
    }

    #[test]
    fn keeps_every_entry() {
        let mut history = History::load(None);
        for i in 0..500 {
            history.add(entry(&i.to_string()));
        }
        assert_eq!(history.entries.len(), 500);
        assert_eq!(history.entries[0].device, "499");
    }

    #[test]
    fn reports_the_size_of_the_file() {
        let path = temp_path("size");
        let mut history = History::load(Some(path));
        assert_eq!(history.file_size(), 0);
        history.add(entry("a"));
        let one = history.file_size();
        assert!(one > 0);
        history.add(entry("b"));
        assert!(history.file_size() > one);
        history.clear();
        assert!(history.file_size() <= 2); // "[]"
    }

    #[test]
    fn entries_get_unique_ids_and_can_be_removed() {
        let mut history = History::load(None);
        let a = history.add(entry("a"));
        let b = history.add(entry("b"));
        assert!(a > 0 && b > a);
        history.remove(a);
        let devices: Vec<_> = history.entries.iter().map(|e| e.device.as_str()).collect();
        assert_eq!(devices, ["b"]);
    }

    #[test]
    fn turned_off_history_records_nothing() {
        let mut history = History::load(None);
        history.set_policy(false, 0);
        assert_eq!(history.add(entry("a")), 0);
        assert_eq!(history.json(), "[]");
    }

    #[test]
    fn old_entries_are_dropped() {
        let mut history = History::load(None);
        history.add(HistoryEntry {
            time: now() - 10 * 86_400,
            ..entry("old")
        });
        history.add(HistoryEntry {
            time: now(),
            ..entry("new")
        });
        history.set_policy(true, 7);
        let devices: Vec<_> = history.entries.iter().map(|e| e.device.as_str()).collect();
        assert_eq!(devices, ["new"]);
    }

    #[test]
    fn clear_empties_the_file_and_bad_files_are_ignored() {
        let path = temp_path("clear");
        let mut history = History::load(Some(path.clone()));
        history.add(entry("a"));
        history.clear();
        assert_eq!(History::load(Some(path.clone())).json(), "[]");

        fs::write(&path, "not json").unwrap();
        assert_eq!(History::load(Some(path)).json(), "[]");
    }

    #[test]
    fn the_file_is_private_and_replaces_a_world_readable_one() {
        let dir = TempDir::new("hist-mode");
        let path = dir.0.join("history.json");
        fs::write(&path, "[]").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let mut history = History::load(Some(path.clone()));
        assert_eq!(mode_of(&path), 0o644, "loading does not rewrite the file");
        history.add(entry("a"));
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn an_invalid_file_is_kept_aside_and_a_stale_temp_is_removed() {
        let dir = TempDir::new("hist-bad");
        let path = dir.0.join("history.json");
        fs::write(&path, "not json").unwrap();
        let stale = dir.0.join(".history.json.0123456789abcdef.tmp");
        fs::write(&stale, "half").unwrap();
        fs::write(dir.0.join("unrelated.tmp"), "x").unwrap();

        let mut history = History::load(Some(path.clone()));
        assert_eq!(history.json(), "[]");
        assert_eq!(
            fs::read_to_string(dir.0.join("history.json.bak")).unwrap(),
            "not json"
        );
        assert!(!stale.exists());
        assert!(dir.0.join("unrelated.tmp").exists());
    }

    #[test]
    fn concurrent_adds_through_the_lock_lose_nothing() {
        let dir = TempDir::new("hist-threads");
        let path = dir.0.join("history.json");
        let history = Arc::new(Mutex::new(History::load(Some(path.clone()))));
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let history = history.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        history.lock().unwrap().add(entry(&format!("{t}-{i}")));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let reloaded = History::load(Some(path));
        assert_eq!(reloaded.entries.len(), 80);
        assert_eq!(history.lock().unwrap().entries, reloaded.entries);
        let mut uids: Vec<_> = reloaded.entries.iter().map(|e| e.uid).collect();
        uids.dedup();
        assert_eq!(uids.len(), 80);
        assert_eq!(names(&dir.0), ["history.json"]);
    }

    #[test]
    fn an_unreadable_history_is_never_saved_over() {
        let dir = TempDir::new("hist-unreadable");
        let path = dir.0.join("history.json");
        fs::write(&path, "[]").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_ok() {
            return; // root
        }
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o500)).unwrap();
        let mut history = History::load(Some(path.clone()));
        history.add(entry("a"));
        assert_eq!(history.entries.len(), 1, "it keeps working in memory");
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        history.add(entry("b"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[]");
        assert_eq!(names(&dir.0), ["history.json"]);
    }
}
