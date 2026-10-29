use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use hkdf::Hkdf;
use if_addrs::get_if_addrs;
use num_bigint::{BigUint, ToBigInt};
use p256::elliptic_curve::Generate;
use p256::{PublicKey, SecretKey};
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;
use rand::{Rng, RngExt};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::CUSTOM_DOWNLOAD;

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum DeviceType {
    Unknown = 0,
    Phone = 1,
    Tablet = 2,
    Laptop = 3,
}

#[allow(dead_code)]
impl DeviceType {
    pub fn from_raw_value(value: u8) -> Self {
        match value {
            0 => DeviceType::Unknown,
            1 => DeviceType::Phone,
            2 => DeviceType::Tablet,
            3 => DeviceType::Laptop,
            _ => DeviceType::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteDeviceInfo {
    pub name: String,
    pub device_type: DeviceType,
}

impl RemoteDeviceInfo {
    pub fn serialize(&self) -> Vec<u8> {
        // 1 byte: Version(3 bits)|Visibility(1 bit)|Device Type(3 bits)|Reserved(1 bit)
        let mut endpoint_info: Vec<u8> = vec![((self.device_type.clone() as u8) << 1) & 0b111];

        // 16 bytes: unknown random bytes
        endpoint_info.extend((0..16).map(|_| rand::rng().random_range(0..=255)));

        // Device name in UTF-8 prefixed with 1-byte length
        let mut name_chars = self.name.as_bytes().to_vec();
        if name_chars.len() > 255 {
            name_chars.truncate(255);
        }
        endpoint_info.push(name_chars.len() as u8);
        endpoint_info.extend(name_chars);

        endpoint_info
    }
}

pub fn gen_mdns_name(endpoint_id: [u8; 4]) -> String {
    let mut name_b = Vec::new();

    let pcp: [u8; 1] = [0x23];
    name_b.extend_from_slice(&pcp);

    name_b.extend_from_slice(&endpoint_id);

    let service_id: [u8; 3] = [0xFC, 0x9F, 0x5E];
    name_b.extend_from_slice(&service_id);

    let unknown_bytes: [u8; 2] = [0x00, 0x00];
    name_b.extend_from_slice(&unknown_bytes);

    URL_SAFE_NO_PAD.encode(&name_b)
}

pub fn gen_mdns_endpoint_info(device_type: u8, device_name: &str) -> String {
    let mut record = Vec::new();

    // 1 byte: Version(3 bits)|Visibility(1 bit)|Device Type(3 bits)|Reserved(1 bits)
    // Device types: unknown=0, phone=1, tablet=2, laptop=3
    record.push(device_type << 1);

    let unknown_bytes = rand::rng().random::<[u8; 16]>();
    record.extend_from_slice(&unknown_bytes);

    let device_name = device_name.as_bytes();
    let length = device_name.len() as u8;
    record.push(length);
    record.extend_from_slice(device_name);

    URL_SAFE_NO_PAD.encode(&record)
}

pub fn parse_mdns_endpoint_info(encoded_str: &str) -> Result<(DeviceType, String), anyhow::Error> {
    let decoded_bytes = URL_SAFE_NO_PAD.decode(encoded_str)?;
    if decoded_bytes.len() < 19 {
        return Err(anyhow!("Invalid data length"));
    }

    let device_type = (decoded_bytes[0] >> 1) & 0x7;
    let name_length = decoded_bytes[17] as usize;
    if 18 + name_length > decoded_bytes.len() {
        return Err(anyhow!("Invalid name length"));
    }

    let device_name_bytes = &decoded_bytes[18..18 + name_length];
    let device_name = String::from_utf8(device_name_bytes.to_vec())?;

    Ok((DeviceType::from_raw_value(device_type), device_name))
}

pub async fn stream_read_exact<R: AsyncRead + Unpin>(
    socket: &mut R,
    buf: &mut [u8],
) -> Result<(), anyhow::Error> {
    match socket.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Cancel-safe version of reading a 4-byte length prefix: bytes already received stay in
/// `buf` (`filled` counts them), so dropping the future between reads loses nothing.
/// `filled` is reset once the prefix is complete.
pub async fn read_length_prefix<R: AsyncRead + Unpin>(
    socket: &mut R,
    buf: &mut [u8; 4],
    filled: &mut usize,
) -> std::io::Result<[u8; 4]> {
    while *filled < buf.len() {
        let n = socket.read(&mut buf[*filled..]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        *filled += n;
    }
    *filled = 0;
    Ok(*buf)
}

/// Cancel-safe read of a whole `[length][frame]`: the progress (`len_filled`, `body`,
/// `body_filled`) lives in the caller, so dropping the future at any point loses nothing and
/// the next call resumes the same frame. The state is reset once the frame is complete.
pub async fn read_frame<R: AsyncRead + Unpin>(
    socket: &mut R,
    len_buf: &mut [u8; 4],
    len_filled: &mut usize,
    body: &mut Vec<u8>,
    body_filled: &mut usize,
    max_len: usize,
) -> std::io::Result<Vec<u8>> {
    while *len_filled < len_buf.len() {
        let n = socket.read(&mut len_buf[*len_filled..]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        *len_filled += n;
    }
    let len = u32::from_be_bytes(*len_buf) as usize;
    if len > max_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message length too big",
        ));
    }
    // A no-op when resuming: the buffer already has its final size.
    body.resize(len, 0);
    while *body_filled < len {
        let n = socket.read(&mut body[*body_filled..]).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        *body_filled += n;
    }
    *len_filled = 0;
    *body_filled = 0;
    Ok(std::mem::take(body))
}

pub fn gen_ecdsa_keypair() -> (SecretKey, PublicKey) {
    let secret_key = SecretKey::generate_from_rng(&mut UnwrapErr(SysRng));
    let public_key = secret_key.public_key();

    (secret_key, public_key)
}

pub fn encode_point(unsigned: Bytes) -> Result<Vec<u8>, anyhow::Error> {
    let big_int = BigUint::from_bytes_be(&unsigned)
        .to_bigint()
        .ok_or_else(|| anyhow!("Failed to convert to bigint"))?;

    Ok(big_int.to_signed_bytes_be())
}

/// Turns a signed big-endian coordinate, as the peer sends it, into exactly 32 bytes: a sign byte
/// is dropped and a coordinate whose leading zero bytes were trimmed is padded back.
pub fn decode_point(signed: &[u8]) -> Vec<u8> {
    if signed.len() >= 32 {
        signed[signed.len() - 32..].to_vec()
    } else {
        let mut out = vec![0u8; 32 - signed.len()];
        out.extend_from_slice(signed);
        out
    }
}

/// Extensions chosen when a MIME type maps to several: `mime_guess` lists them alphabetically,
/// so the first one is often an odd one (`text/plain` starts with `asm`).
const PREFERRED_EXTENSIONS: &[&str] = &[
    "txt", "jpg", "png", "gif", "webp", "pdf", "mp4", "mp3", "zip", "json", "html", "csv", "apk",
];

/// The file name to save a received file under. Only the last component of the peer's name is
/// kept, so it cannot leave the download directory (`None` if nothing usable remains). A name
/// with no extension, or a generic one (`.tmp`, `.bin`), gets the extension `mime_type` implies
/// when that is unambiguous.
pub fn received_file_name(name: &str, mime_type: &str) -> Option<String> {
    let path = Path::new(name);
    let base = path.file_name()?.to_str()?;
    let ext = path.extension().and_then(|e| e.to_str());
    let generic = ext.is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "tmp" | "bin"));
    if (ext.is_some() && !generic) || base.starts_with('.') && ext.is_none() {
        return Some(base.to_owned());
    }
    let new_ext = mime_guess::get_mime_extensions_str(mime_type)
        .filter(|_| mime_type != "application/octet-stream")
        .and_then(|exts| {
            exts.iter()
                .find(|e| PREFERRED_EXTENSIONS.contains(e))
                .or(if exts.len() == 1 { exts.first() } else { None })
        });
    Some(match new_ext {
        Some(e) => {
            // The name must stay within what a file system takes.
            let room = crate::safety::MAX_NAME_BYTES.saturating_sub(e.len() + 1);
            let stem = crate::safety::clip(path.file_stem()?.to_str()?, room);
            format!("{stem}.{e}")
        }
        None => base.to_owned(),
    })
}

pub fn hkdf_extract_expand(
    salt: &[u8],
    input: &[u8],
    info: &[u8],
    output_len: usize,
) -> Result<Vec<u8>, anyhow::Error> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), input);
    let mut okm = vec![0u8; output_len];
    hkdf.expand(info, &mut okm)
        .map_err(|e| anyhow!("HKDF expand failed: {}", e))?;
    Ok(okm)
}

pub fn to_four_digit_string(bytes: &Vec<u8>) -> String {
    let k_hash_modulo = 9973;
    let k_hash_base_multiplier = 31;

    let mut hash = 0;
    let mut multiplier = 1;
    for &byte in bytes {
        let byte = byte as i8 as i32;
        hash = (hash + byte * multiplier) % k_hash_modulo;
        multiplier = (multiplier * k_hash_base_multiplier) % k_hash_modulo;
    }

    format!("{:04}", hash.abs())
}

pub fn gen_random(size: usize) -> Vec<u8> {
    let mut data = vec![0; size];
    rand::rng().fill_bytes(&mut data);

    data
}

pub fn get_download_dir() -> PathBuf {
    let cdown = CUSTOM_DOWNLOAD.read();
    match cdown {
        Ok(mg) => {
            if mg.is_some() {
                return mg.as_ref().unwrap().to_path_buf();
            }
        }
        Err(_) => {
            // TODO
        }
    }

    if let Some(dd) = dirs::download_dir() {
        return dd;
    }
    if let Some(home) = dirs::home_dir() {
        return home;
    }

    Path::new("/").to_path_buf()
}

/// The machine hostname, or "Unknown device" when it cannot be read.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown device".to_owned())
}

pub fn is_not_self_ip(ip_address: &Ipv4Addr) -> bool {
    if let Ok(if_addrs) = get_if_addrs() {
        for if_addr in if_addrs {
            if if_addr.ip() == *ip_address {
                return false;
            }
        }
    }

    true
}

pub fn create_text_preview(text: &str, max_chars: usize) -> String {
    // Flatten newlines so the preview renders cleanly on a single line
    let single_line = text.lines().next().unwrap_or("").trim();

    let char_count = single_line.chars().count();
    if char_count <= max_chars {
        single_line.to_string()
    } else {
        let truncated: String = single_line.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    /// The extension taken from the MIME type must not push the name over 255 bytes.
    #[test]
    fn mime_extension_keeps_the_name_within_the_limit() {
        let long = "é".repeat(127); // 254 bytes, no extension
        let name = super::received_file_name(&long, "image/jpeg").unwrap();
        assert!(
            name.len() <= crate::safety::MAX_NAME_BYTES,
            "{}",
            name.len()
        );
        assert!(name.ends_with(".jpg"), "{name}");
        assert!(!crate::safety::unsafe_file_name(&name));
    }

    use super::*;

    #[test]
    fn the_hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }

    /// A frame split across reads, with the read future dropped in between (as a `select!`
    /// does), resumes at the same byte: nothing is lost or read twice.
    #[tokio::test]
    async fn read_frame_resumes_a_split_frame() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        type State = ([u8; 4], usize, Vec<u8>, usize);
        async fn read(
            socket: &mut tokio::io::DuplexStream,
            st: &mut State,
        ) -> Option<std::io::Result<Vec<u8>>> {
            let (len_buf, len_filled, body, body_filled) = st;
            tokio::time::timeout(
                Duration::from_millis(50),
                read_frame(socket, len_buf, len_filled, body, body_filled, 16),
            )
            .await
            .ok()
        }

        let (mut peer, mut ours) = tokio::io::duplex(64);
        let mut st: State = ([0; 4], 0, Vec::new(), 0);

        // Half of the prefix, then the rest of it and half of the body, then the end of the
        // body and the first byte of the next frame.
        peer.write_all(&[0, 0]).await.unwrap();
        assert!(read(&mut ours, &mut st).await.is_none());
        peer.write_all(&[0, 4, 1, 2]).await.unwrap();
        assert!(read(&mut ours, &mut st).await.is_none());
        peer.write_all(&[3, 4, 0]).await.unwrap();
        assert_eq!(
            read(&mut ours, &mut st).await.unwrap().unwrap(),
            vec![1, 2, 3, 4]
        );
        peer.write_all(&[0, 0, 1, 7]).await.unwrap();
        assert_eq!(read(&mut ours, &mut st).await.unwrap().unwrap(), vec![7]);

        // Oversized frames are refused.
        peer.write_all(&[0, 0, 0, 17]).await.unwrap();
        let err = read(&mut ours, &mut st).await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_point_restores_32_bytes() {
        // Every shape a peer can send for a coordinate: trimmed, exact, with a sign byte.
        let mut full = [7u8; 32];
        assert_eq!(decode_point(&full), full);
        let mut signed = vec![0u8];
        signed.extend_from_slice(&full);
        assert_eq!(decode_point(&signed), full);
        full[0] = 0;
        assert_eq!(decode_point(&full[1..]), full);
        full[1] = 0;
        assert_eq!(decode_point(&full[2..]), full);
    }

    #[test]
    fn received_file_name_fixes_extension_and_path() {
        let n = received_file_name;
        assert_eq!(
            n("cache1.tmp", "application/pdf").as_deref(),
            Some("cache1.pdf")
        );
        assert_eq!(n("cache1.TMP", "text/plain").as_deref(), Some("cache1.txt"));
        assert_eq!(n("cache1", "image/jpeg").as_deref(), Some("cache1.jpg"));
        assert_eq!(n("v", "video/mp4").as_deref(), Some("v.mp4"));
        assert_eq!(
            n("photo.jpg", "application/pdf").as_deref(),
            Some("photo.jpg")
        );
        assert_eq!(n("photo.jpeg", "image/jpeg").as_deref(), Some("photo.jpeg"));
        assert_eq!(
            n("a.tmp", "application/octet-stream").as_deref(),
            Some("a.tmp")
        );
        assert_eq!(n("a.tmp", "").as_deref(), Some("a.tmp"));
        assert_eq!(n("a", "").as_deref(), Some("a"));
        assert_eq!(n(".bashrc", "text/plain").as_deref(), Some(".bashrc"));
        assert_eq!(n("../../etc/x.txt", "text/plain").as_deref(), Some("x.txt"));
        assert_eq!(n("/abs/y.txt", "text/plain").as_deref(), Some("y.txt"));
        assert_eq!(n("dir/", "text/plain").as_deref(), Some("dir.txt"));
        assert_eq!(n("..", "text/plain"), None);
        assert_eq!(n("", "text/plain"), None);
    }

    #[test]
    fn encoded_points_round_trip_for_leading_zero_bytes() {
        for lead in [0u8, 1, 0x80, 0xff] {
            let mut coord = [0x55u8; 32];
            coord[0] = lead;
            let encoded = encode_point(Bytes::copy_from_slice(&coord)).unwrap();
            assert_eq!(decode_point(&encoded), coord, "leading byte {lead:#x}");
        }
    }

    #[test]
    fn test_gen_and_parse_mdns_info() {
        let device_name = "a_device_name";
        let device_type = DeviceType::Laptop;

        dbg!(&device_type);
        dbg!(device_type.clone() as u8);

        let info = gen_mdns_endpoint_info(device_type.clone() as u8, device_name);
        let parse_info = parse_mdns_endpoint_info(&info).unwrap();

        assert_eq!(parse_info.1, device_name);
        assert_eq!(parse_info.0, device_type);
    }
}

/// Keep-alives come every 5 s, so this allows about 30 s of silence before the handshake gives up.
pub(crate) const MAX_HANDSHAKE_KEEP_ALIVES: u8 = 6;

/// A keep-alive frame, which a peer may send at any time, also before the handshake is done.
pub(crate) fn is_keep_alive(frame: &crate::location_nearby_connections::OfflineFrame) -> bool {
    use crate::location_nearby_connections::v1_frame::FrameType;
    frame
        .v1
        .as_ref()
        .is_some_and(|v1| v1.r#type() == FrameType::KeepAlive)
}

#[cfg(test)]
mod keep_alive_tests {
    use super::is_keep_alive;
    use crate::location_nearby_connections::{OfflineFrame, V1Frame, v1_frame::FrameType};

    fn frame(kind: FrameType) -> OfflineFrame {
        OfflineFrame {
            v1: Some(V1Frame {
                r#type: Some(kind.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_keep_alive_frames_are_recognised() {
        assert!(is_keep_alive(&frame(FrameType::KeepAlive)));
        assert!(!is_keep_alive(&frame(FrameType::ConnectionResponse)));
        assert!(!is_keep_alive(&OfflineFrame::default()));
    }
}
