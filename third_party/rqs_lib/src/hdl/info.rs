use std::fs::File;
use std::path::PathBuf;

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
    /// Where the files of an inbound file transfer are saved (empty otherwise).
    pub saved_paths: Vec<String>,
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
