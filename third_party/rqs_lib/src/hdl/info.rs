use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use crate::safety::{DestDir, Offered, Partial, Refusal};
use crate::sharing_nearby::wifi_credentials_metadata::SecurityType;
use crate::utils::RemoteDeviceInfo;

use super::TextPayloadType;

#[derive(Debug)]
pub struct InternalFileInfo {
    pub payload_id: i64,
    pub file_url: PathBuf,
    pub bytes_transferred: i64,
    pub total_size: i64,
    pub file: Option<File>,
}

/// One file of an inbound offer. Its `partial` exists from the user's accept until the file
/// is complete; dropping it removes the file.
#[derive(Debug)]
pub struct InboundFile {
    pub payload_id: i64,
    /// The name to save under, before any ` (N)` suffix.
    pub name: String,
    pub size: u64,
    pub partial: Option<Partial>,
    pub finished: bool,
}

/// The files of an inbound offer, in the order the peer listed them.
#[derive(Debug, Default)]
pub struct InboundFiles {
    items: Vec<InboundFile>,
    index: HashMap<i64, usize>,
    /// The file whose descriptor is open, if any (at most one is).
    current: Option<i64>,
    /// The download folder looked at when the offer came; the files go there at the accept
    /// unless the setting changed (`None` if it did not exist yet).
    pub dest: Option<Arc<DestDir>>,
}

impl InboundFiles {
    /// Replaces the offer. `offered` has distinct payload ids (`check_offer`).
    pub fn set(&mut self, offered: Vec<Offered>) {
        self.items.clear();
        self.index.clear();
        self.current = None;
        for (i, o) in offered.into_iter().enumerate() {
            self.index.insert(o.payload_id, i);
            self.items.push(InboundFile {
                payload_id: o.payload_id,
                name: o.name,
                size: o.size,
                partial: None,
                finished: false,
            });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get_mut(&mut self, payload_id: i64) -> Option<&mut InboundFile> {
        let i = *self.index.get(&payload_id)?;
        self.items.get_mut(i)
    }

    /// Like `get_mut`, for a file about to be written: the descriptor of the file written
    /// before is closed, so a peer that alternates between files cannot hold many open.
    pub fn get_active(&mut self, payload_id: i64) -> Option<&mut InboundFile> {
        if self.current != Some(payload_id) {
            let previous = self.current.and_then(|id| self.index.get(&id).copied());
            if let Some(p) = previous
                .and_then(|i| self.items.get_mut(i))
                .and_then(|f| f.partial.as_mut())
            {
                p.release();
            }
            self.current = Some(payload_id);
        }
        self.get_mut(payload_id)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut InboundFile> {
        self.items.iter_mut()
    }

    /// Whether every file is complete (false for an empty offer).
    pub fn all_finished(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|f| f.finished)
    }

    /// `(name, size)` of every file, in order.
    pub fn plan(&self) -> Vec<(String, u64)> {
        self.items
            .iter()
            .map(|f| (f.name.clone(), f.size))
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.items
            .iter()
            .fold(0u64, |sum, f| sum.saturating_add(f.size))
    }

    /// Removes the files that are not complete.
    pub fn discard(&mut self) {
        for f in &mut self.items {
            f.partial = None;
        }
    }
}

#[derive(Clone)]
pub enum TransferPayload {
    Files(Vec<String>),
    Text(String),
    Url(String),
    Wifi {
        ssid: String,
        password: String,
        security_type: SecurityType,
    },
}

/// Hand-written so that no `Debug` output (logs, `{:?}` of a message) can show a Wi-Fi password.
impl std::fmt::Debug for TransferPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Files(files) => f.debug_tuple("Files").field(files).finish(),
            Self::Text(text) => f.debug_tuple("Text").field(text).finish(),
            Self::Url(url) => f.debug_tuple("Url").field(url).finish(),
            Self::Wifi {
                ssid,
                security_type,
                ..
            } => f
                .debug_struct("Wifi")
                .field("ssid", ssid)
                .field("password", &"<redacted>")
                .field("security_type", security_type)
                .finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TransferPayloadKind {
    Files,
    Text,
    Url,
    WiFi,
}

impl TransferPayload {
    pub fn text_type(&self) -> Option<TextPayloadType> {
        match self {
            TransferPayload::Text(_) => Some(TextPayloadType::Text),
            TransferPayload::Url(_) => Some(TextPayloadType::Url),
            TransferPayload::Wifi {
                ssid: _,
                password: _,
                security_type: _,
            } => Some(TextPayloadType::Wifi),
            TransferPayload::Files(_items) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferMetadata {
    pub id: String,
    pub source: Option<RemoteDeviceInfo>,
    pub pin_code: Option<String>,

    // This exists since the client may want to know
    // the type before receiving the complete payload
    pub payload_kind: TransferPayloadKind,
    /// Only exists for Text data
    pub payload_preview: Option<String>,
    pub payload: Option<TransferPayload>,

    pub total_bytes: u64,
    pub ack_bytes: u64,
    /// Where the files of an inbound file transfer are saved (empty otherwise). Before the
    /// accept these are the planned paths; from `ReceivingFiles` on, the real ones.
    pub saved_paths: Vec<String>,
    /// Why an inbound offer was refused (`Rejected` state), if the library refused it.
    pub refusal: Option<Refusal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_payload_debug_hides_the_password() {
        let payload = TransferPayload::Wifi {
            ssid: "home".into(),
            password: "hunter2-secret".into(),
            security_type: SecurityType::WpaPsk,
        };
        let shown = format!("{payload:?} {:#?}", Some(payload.clone()));
        assert!(!shown.contains("hunter2-secret"));
        assert!(shown.contains("home") && shown.contains("redacted"));
    }
}
