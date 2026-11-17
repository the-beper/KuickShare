//! The 0xFEF3 receiver advertisement (BLE service data, also served from GATT slot 0).
//!
//! Three encodings of the same receive-mode beacon reach the scanner (port of
//! crQuickShare's `advertisement_codec.cc`, BSD-3-Clause, The Chromium Authors):
//!
//! - Regular PCP (reverse-engineered from a Pixel 9 Pro capture, see docs):
//!   `48 | fc9f5e | u32 BE len | DATA | token(2) | mask | [psm(2)] | [len | rx_instant]`
//!   with `DATA = 23 | fc9f5e | endpoint_id(4) | info_len | endpoint_info | mac(6) | 00 00`.
//! - Fast/compact (`0x4a`): the same envelope with a 1-byte length, no service hash, and
//!   `DATA = 23 | endpoint_id(4) | info_len | endpoint_info` — no classic Bluetooth address
//!   and no trailing `00 00`. Upstream caps the endpoint info at 17 bytes; phones send
//!   longer (crQuickShare normalises them for upstream), so no cap is applied here. Bytes
//!   after the endpoint info can only be a UWB address (`uwb_len(1)` then that many bytes).
//! - DCT (`0x20`): `20 | DataElement TLVs` with no endpoint id at all — type 5 the Sharing
//!   service hash, type 4 the PSM, type 7 the device name. See [`ReceiverAdvertisement::dct`].

use super::socket::SERVICE_HASH;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

/// 16-bit service UUID of the receiver advertisement and the GATT service.
pub const SERVICE_UUID: u16 = 0xFEF3;
pub const DEVICE_TYPE_LAPTOP: u8 = 3;

const HEADER: u8 = 0x48;
const HEADER_FAST: u8 = 0x4a;
/// DCT form: version 1, and nothing else in the byte (upstream `dct_advertisement.cc`).
const HEADER_DCT: u8 = 0x20;
/// DCT data-element types (upstream `dct_advertisement.cc`).
const DCT_SERVICE_HASH_TYPE: u8 = 0x05;
const DCT_PSM_TYPE: u8 = 0x04;
const DCT_DEVICE_INFO_TYPE: u8 = 0x07;
/// HKDF-SHA256(ikm="NearbySharing", salt="DCT Protocol", info="Service ID Hash")[:2];
/// a test recomputes it from the crate's own `hkdf`.
const DCT_SERVICE_HASH: [u8; 2] = [0x6d, 0xfd];
/// Most distinct data elements any legal DCT advertisement carries.
const MAX_DCT_FIELDS: usize = 32;
/// DCT device information: 1 byte (truncation bit, dedup counter) plus 1..=7 name bytes.
const MAX_DCT_NAME_LEN: usize = 7;
/// Bit of the endpoint info header that is set when the name is hidden.
const INFO_HIDDEN: u8 = 0x10;
/// Service data is at most 255 bytes on the air; anything larger is not ours.
const MAX_PARSE_LEN: usize = 512;
const DATA_VERSION_PCP: u8 = 0x23;
const MASK_PSM: u8 = 0x01;
const MASK_RX_INSTANT: u8 = 0x02;

/// The legacy advertisement is a base64url encoding of 17 bytes. It announces
/// the full advertisement in GATT slot 0; unlike the 0x4a fast form it has a
/// service-id bloom filter and an advertisement hash, not endpoint metadata.
/// Layout: `flags(1) | bloom(10) | advert_hash(4) | psm(2)`; PR #8 of rquickshare
/// (`ble_discovery/adv.rs`, from google/nearby `ble_advertisement_header.cc`) builds it the same way.
const LEGACY_HEADER_BYTES: usize = 17;
/// Version of the header, bits 7..5 of byte 0: the one field known for sure (PR #8 and the
/// captured header both read `2`).
const HEADER_VERSION: u8 = 2;
/// Byte 0 below the version. Only "`1` is one slot, not extended" comes from PR #8; where the
/// extended flag sits (bit 4) and that the low nibble counts slots are *(inferred)* from the one
/// captured header, whose byte 0 is `0x55`: version 2, bit 4 set, low nibble 5.
const HEADER_EXTENDED: u8 = 0x10;
const HEADER_SLOTS_MASK: u8 = 0x0f;
const BLOOM_FILTER_LEN: usize = 10;
const BLOOM_HASHES: i32 = 5;
const DUMMY_SERVICE_ID_LEN: usize = 128;
const SERVICE_ID: &str = "NearbySharing";

/// Last four hash bytes cannot authenticate the remote record: the dummy seed is
/// intentionally unpublished. Parse the *full* GATT record before listing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyHeader {
    pub advert_hash: [u8; 4],
}

/// Returns None for unrelated services, malformed headers and normal 0x48/0x4a data.
pub fn parse_legacy_header(data: &[u8]) -> Option<LegacyHeader> {
    let text = std::str::from_utf8(data).ok()?.trim_end_matches('=');
    let raw = URL_SAFE_NO_PAD
        .decode(text)
        .or_else(|_| STANDARD_NO_PAD.decode(text))
        .ok()?;
    if raw.len() != LEGACY_HEADER_BYTES
        || raw[0] >> 5 != HEADER_VERSION
        || !bloom_contains(&raw[1..11], SERVICE_ID.as_bytes())
    {
        return None;
    }
    Some(LegacyHeader {
        advert_hash: raw[11..15].try_into().ok()?,
    })
}

/// Generate a private randomized Nearby Connections bloom header. The complete
/// receiver record remains in GATT slot 0; the header itself reveals no name.
pub fn build_legacy_header(full: &[u8]) -> Vec<u8> {
    let mut dummy = [0u8; DUMMY_SERVICE_ID_LEN];
    rand::fill(&mut dummy[..]);
    URL_SAFE_NO_PAD
        .encode(legacy_header_bytes(SERVICE_ID, &dummy, full))
        .into_bytes()
}

/// Byte 0 of the legacy header; see [`HEADER_EXTENDED`] for what is known of the layout.
fn legacy_header_flags(extended: bool, slots: u8) -> u8 {
    (HEADER_VERSION << 5) | if extended { HEADER_EXTENDED } else { 0 } | (slots & HEADER_SLOTS_MASK)
}

fn legacy_header_bytes(service_id: &str, dummy: &[u8], full: &[u8]) -> [u8; LEGACY_HEADER_BYTES] {
    let mut bloom = [0u8; BLOOM_FILTER_LEN];
    bloom_add(&mut bloom, dummy);
    bloom_add(&mut bloom, service_id.as_bytes());
    let seed = Sha256::digest(dummy);
    // Same formula as PR #8's `header_bytes`: the first 4 bytes of SHA-256(first 4 bytes of
    // SHA-256(dummy service id) | full record). The captured header cannot confirm it (its dummy
    // service id is unknown), and whether a phone checks the hash is unknown: *(inferred)* that
    // it only uses it as a cache key.
    let advert_hash = Sha256::new()
        .chain_update(&seed[..4])
        .chain_update(full)
        .finalize();
    let mut result = [0u8; LEGACY_HEADER_BYTES];
    // Slot 0 only, no extended advertisement: this advertiser has a PSM-less GATT record.
    result[0] = legacy_header_flags(false, 1);
    result[1..11].copy_from_slice(&bloom);
    result[11..15].copy_from_slice(&advert_hash[..4]);
    // The PSM bytes stay 0: the GATT weave socket is used.
    result
}

fn bloom_positions(data: &[u8]) -> impl Iterator<Item = usize> {
    let (hash, _) = murmur3_x64_128(data, 0);
    let (h1, h2) = (hash as i32, (hash >> 32) as i32);
    (1..=BLOOM_HASHES).map(move |n| {
        let combined = h1.wrapping_add(n.wrapping_mul(h2));
        let positive = if combined < 0 { !combined } else { combined };
        positive as usize % (BLOOM_FILTER_LEN * 8)
    })
}

fn bloom_add(bloom: &mut [u8; BLOOM_FILTER_LEN], data: &[u8]) {
    for pos in bloom_positions(data) {
        bloom[pos / 8] |= 1 << (pos % 8);
    }
}

fn bloom_contains(bloom: &[u8], data: &[u8]) -> bool {
    bloom_positions(data).all(|pos| bloom[pos / 8] & (1 << (pos % 8)) != 0)
}

/// MurmurHash3 x64 128-bit, with the Nearby bloom filter's 32-bit slices of h1.
fn murmur3_x64_128(data: &[u8], seed: u64) -> (u64, u64) {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^ (k >> 33)
    }
    let mix1 = |k: u64| k.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
    let mix2 = |k: u64| k.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
    let (mut h1, mut h2) = (seed, seed);
    // The words are read from fixed-size arrays, so the slice lengths cannot differ.
    fn word(block: &[u8; 16], index: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&block[index * 8..index * 8 + 8]);
        u64::from_le_bytes(bytes)
    }
    let (blocks, tail) = data.as_chunks::<16>();
    for block in blocks {
        let (k1, k2) = (word(block, 0), word(block, 1));
        h1 ^= mix1(k1);
        h1 = h1
            .rotate_left(27)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x52dc_e729);
        h2 ^= mix2(k2);
        h2 = h2
            .rotate_left(31)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x3849_5ab5);
    }
    let mut padded = [0u8; 16];
    padded[..tail.len()].copy_from_slice(tail);
    if tail.len() > 8 {
        h2 ^= mix2(word(&padded, 1));
    }
    if !tail.is_empty() {
        h1 ^= mix1(word(&padded, 0));
    }
    h1 ^= data.len() as u64;
    h2 ^= data.len() as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

/// Minimum bytes required to parse a slot-0 advertisement, including PSM and
/// RxInstant fields when present. Rejects bogus lengths instead of allocating.
pub fn regular_length(data: &[u8]) -> Option<usize> {
    if data.len() < 8 || data[0] != HEADER || &data[1..4] != SERVICE_HASH.as_slice() {
        return None;
    }
    let len = u32::from_be_bytes(data[4..8].try_into().ok()?) as usize;
    let tail = 8usize.checked_add(len)?;
    let mask_at = tail.checked_add(2)?;
    let mut total = mask_at.checked_add(1)?;
    if total > MAX_PARSE_LEN {
        return None;
    }
    if let Some(&mask) = data.get(mask_at) {
        if mask & MASK_PSM != 0 {
            total += 2;
        }
        if mask & MASK_RX_INSTANT != 0 {
            let rx_len = data.get(total).copied().unwrap_or(0);
            total += 1 + usize::from(rx_len);
        }
    }
    (total <= MAX_PARSE_LEN).then_some(total)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverAdvertisement {
    /// Must equal the endpoint id used in the mDNS instance name.
    pub endpoint_id: [u8; 4],
    pub device_type: u8,
    /// Random per run; the same 16 bytes as in the mDNS endpoint info.
    pub identity: [u8; 16],
    pub name: String,
    /// All zeroes by default: a wrong MAC makes the phone try Bluetooth Classic first.
    pub bt_mac: [u8; 6],
    /// L2CAP PSM to advertise. `None` clears the mask bit (UNVERIFIED on a phone);
    /// the proven capture advertised `Some(0x0082)`.
    pub psm: Option<u16>,
    /// Random device token (Nearby derives it from a random number); carries no identity.
    pub token: [u8; 2],
    /// The "RxInstantConnectionAdv" field (at most 255 bytes). `None` leaves it out: the
    /// bytes seen in a Pixel capture are opaque and possibly tied to that device's account,
    /// so they are not reused. A Pixel 7 listed the receiver without them (one hardware run).
    pub rx_instant: Option<Vec<u8>>,
    /// Parsed from the DCT form: the phone sent no endpoint id, no identity and no classic
    /// Bluetooth address, so `endpoint_id`, `identity` and `bt_mac` carry zeros and the
    /// scanner stands in with a per-address id (see `ble_scan::scan_result`). The name and
    /// the PSM are present, so the phone is listed and reachable like any other.
    pub dct: bool,
}

impl ReceiverAdvertisement {
    pub fn new(endpoint_id: [u8; 4], device_type: u8, identity: [u8; 16], name: &str) -> Self {
        Self {
            endpoint_id,
            device_type,
            identity,
            name: truncate_name(name).to_owned(),
            bt_mac: [0; 6],
            psm: None,
            token: rand::random(),
            rx_instant: None,
            dct: false,
        }
    }

    /// Nearby Share application advertisement (same layout as the mDNS `n` record).
    pub fn endpoint_info(&self) -> Vec<u8> {
        crate::utils::gen_endpoint_info(self.device_type, &self.identity, &self.name)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let info = self.endpoint_info();
        let mut data = vec![DATA_VERSION_PCP];
        data.extend_from_slice(&SERVICE_HASH);
        data.extend_from_slice(&self.endpoint_id);
        data.push(info.len() as u8);
        data.extend_from_slice(&info);
        data.extend_from_slice(&self.bt_mac);
        data.extend_from_slice(&[0, 0]);

        let mut out = vec![HEADER];
        out.extend_from_slice(&SERVICE_HASH);
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(&self.token);
        let rx_instant = self.rx_instant.as_deref().filter(|d| d.len() <= 255);
        out.push(
            if self.psm.is_some() { MASK_PSM } else { 0 }
                | if rx_instant.is_some() {
                    MASK_RX_INSTANT
                } else {
                    0
                },
        );
        if let Some(psm) = self.psm {
            out.extend_from_slice(&psm.to_be_bytes());
        }
        if let Some(data) = rx_instant {
            out.push(data.len() as u8);
            out.extend_from_slice(data);
        }
        out
    }

    /// Parses an advertisement; see [`Self::try_parse`] for the reason of a failure.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        Self::try_parse(bytes).ok()
    }

    /// Parses the advertisement of a receiver that is visible to everyone (name in clear),
    /// in any of the three encodings of the module docs.
    pub fn try_parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() > MAX_PARSE_LEN {
            return Err(ParseError::TooLong);
        }
        let (&header, rest) = bytes.split_first().ok_or(ParseError::Truncated)?;
        match header {
            HEADER => Self::parse_regular(rest),
            HEADER_FAST => Self::parse_fast(rest),
            HEADER_DCT => Self::parse_dct(rest),
            _ => Err(ParseError::BadHeader),
        }
    }

    /// The regular form: see the module docs.
    fn parse_regular(rest: &[u8]) -> Result<Self, ParseError> {
        if slice(rest, 0, 3)? != SERVICE_HASH {
            return Err(ParseError::BadHeader);
        }
        let len = usize::try_from(u32::from_be_bytes(array(rest, 3)?))
            .map_err(|_| ParseError::TooLong)?;
        let data_end = 7usize.checked_add(len).ok_or(ParseError::Truncated)?;
        let data = rest.get(7..data_end).ok_or(ParseError::Truncated)?;
        let tail = rest.get(data_end..).ok_or(ParseError::Truncated)?;

        if *data.first().ok_or(ParseError::Truncated)? != DATA_VERSION_PCP
            || slice(data, 1, 3)? != SERVICE_HASH
        {
            return Err(ParseError::BadHeader);
        }
        let endpoint_id = array(data, 4)?;
        let info_len = usize::from(*data.get(8).ok_or(ParseError::Truncated)?);
        let info = slice(data, 9, info_len)?;
        let bt_mac = array(data, 9 + info_len)?;
        let (device_type, identity, name) = parse_endpoint_info(info)?;
        let (token, psm, rx_instant) = parse_tail(tail)?;

        Ok(Self {
            endpoint_id,
            device_type,
            identity,
            name,
            bt_mac,
            psm,
            token,
            rx_instant,
            dct: false,
        })
    }

    /// The fast/compact form: see the module docs.
    fn parse_fast(rest: &[u8]) -> Result<Self, ParseError> {
        let len = usize::from(*rest.first().ok_or(ParseError::Truncated)?);
        let data_end = 1usize.checked_add(len).ok_or(ParseError::Truncated)?;
        let data = rest.get(1..data_end).ok_or(ParseError::Truncated)?;
        let tail = rest.get(data_end..).ok_or(ParseError::Truncated)?;

        if *data.first().ok_or(ParseError::Truncated)? != DATA_VERSION_PCP {
            return Err(ParseError::BadHeader);
        }
        let endpoint_id = array(data, 1)?;
        let info_len = usize::from(*data.get(5).ok_or(ParseError::Truncated)?);
        let info = slice(data, 6, info_len)?;
        // Bytes after the endpoint info can only be a UWB address: a length byte and that
        // many bytes, exactly (crQuickShare's "connection_extensions" bound). A short one is
        // junk; a long one means the info length was misread.
        let after = data
            .len()
            .checked_sub(6 + info_len)
            .ok_or(ParseError::Truncated)?;
        if after != 0 {
            let uwb_len = usize::from(*data.get(6 + info_len).ok_or(ParseError::Truncated)?);
            if after != 1 + uwb_len {
                return Err(ParseError::BadHeader);
            }
        }
        let (device_type, identity, name) = parse_endpoint_info(info)?;
        let (token, psm, rx_instant) = parse_tail(tail)?;

        Ok(Self {
            endpoint_id,
            device_type,
            identity,
            name,
            bt_mac: [0; 6],
            psm,
            token,
            rx_instant,
            dct: false,
        })
    }

    /// The DCT form: see the module docs. Upstream reads the elements in one fixed order;
    /// crQuickShare accepts any order and skips unknown types, and so does this.
    fn parse_dct(rest: &[u8]) -> Result<Self, ParseError> {
        let mut seen = [false; 256];
        let mut fields = 0usize;
        let mut psm = None;
        let mut name = None;
        let mut at = 0usize;
        while at < rest.len() {
            let header = rest[at];
            at += 1;
            // Every element header is exactly 8 or 16 bits and every value a whole number of
            // bytes, so the element stream never straddles a byte boundary (upstream
            // `data_element.cc` reads the same bits through a bit reader).
            let (type_, len) = if header & 0x80 == 0 {
                // One-byte header `0b0_LLL_TTTT`: 3-bit length, 4-bit type.
                (header & 0x0f, (header >> 4) & 0x07)
            } else {
                // Two-byte header: 7-bit length, then the full type byte.
                let type_ = *rest.get(at).ok_or(ParseError::Truncated)?;
                at += 1;
                (type_, header & 0x7f)
            };
            if type_ == 0 {
                return Err(ParseError::BadHeader);
            }
            let value = slice(rest, at, usize::from(len))?;
            at += usize::from(len);
            if seen[usize::from(type_)] {
                return Err(ParseError::DuplicateField);
            }
            seen[usize::from(type_)] = true;
            fields += 1;
            if fields > MAX_DCT_FIELDS {
                // More distinct fields than any legal DCT advertisement: junk that must not
                // keep the loop growing.
                return Err(ParseError::TooLong);
            }
            match type_ {
                DCT_SERVICE_HASH_TYPE => {
                    if value != DCT_SERVICE_HASH {
                        return Err(ParseError::BadServiceHash);
                    }
                }
                DCT_PSM_TYPE => {
                    if len != 2 {
                        return Err(ParseError::BadPsm);
                    }
                    let bytes: [u8; 2] = array(value, 0)?;
                    let value = u16::from_be_bytes(bytes);
                    if value == 0 {
                        return Err(ParseError::BadPsm);
                    }
                    psm = Some(value);
                }
                DCT_DEVICE_INFO_TYPE => {
                    if usize::from(len) < 2 || usize::from(len) > 1 + MAX_DCT_NAME_LEN {
                        return Err(ParseError::BadHeader);
                    }
                    // Byte 0: bit 7 the name is truncated upstream, bits 0-6 a dedup counter
                    // for two phones of the same name. Neither is used here: the raw name is
                    // kept as sent, and the id comes from the address (see `dct`).
                    let text = std::str::from_utf8(&value[1..]).map_err(|_| ParseError::BadName)?;
                    if text.chars().any(|c| matches!(c, '\0'..='\x1f' | '\x7f')) {
                        return Err(ParseError::BadName);
                    }
                    name = Some(text.to_owned());
                }
                // Unknown types carry their own length: skipped and counted, like upstream.
                _ => {}
            }
        }
        if !(seen[usize::from(DCT_SERVICE_HASH_TYPE)]
            && seen[usize::from(DCT_PSM_TYPE)]
            && seen[usize::from(DCT_DEVICE_INFO_TYPE)])
        {
            return Err(ParseError::MissingField);
        }
        // The checks above guarantee both: a seen PSM element always set it, same for the name.
        let psm = psm.ok_or(ParseError::MissingField)?;
        let name = name.ok_or(ParseError::MissingField)?;
        Ok(Self {
            endpoint_id: [0; 4],
            device_type: 0,
            identity: [0; 16],
            name,
            bt_mac: [0; 6],
            psm: Some(psm),
            token: [0; 2],
            rx_instant: None,
            dct: true,
        })
    }
}

/// The Nearby Share application advertisement inside any form: the endpoint info the
/// mDNS `n` record carries too (`device_type | hidden bit`, identity, name).
fn parse_endpoint_info(info: &[u8]) -> Result<(u8, [u8; 16], String), ParseError> {
    let info_header = *info.first().ok_or(ParseError::Truncated)?;
    if info_header & INFO_HIDDEN != 0 {
        return Err(ParseError::NotEveryone);
    }
    let device_type = (info_header >> 1) & 0x7;
    let identity = array(info, 1)?;
    let name_len = usize::from(*info.get(17).ok_or(ParseError::Truncated)?);
    let name = std::str::from_utf8(slice(info, 18, name_len)?)
        .map_err(|_| ParseError::BadName)?
        .to_owned();
    Ok((device_type, identity, name))
}

/// The token, the PSM and the `rx_instant` bytes of an advertisement's tail.
type Tail = ([u8; 2], Option<u16>, Option<Vec<u8>>);

/// The bytes after the declared payload of a regular or fast form: the token, then the
/// extension mask and what it asks for.
fn parse_tail(tail: &[u8]) -> Result<Tail, ParseError> {
    let token = array(tail, 0)?;
    // Samsung's Quick Share ends the advertisement after the token: no mask, no PSM.
    let mask = tail.get(2).copied().unwrap_or(0);
    let mut at = 3;
    let psm = if mask & MASK_PSM != 0 {
        at += 2;
        Some(u16::from_be_bytes(array(tail, at - 2)?))
    } else {
        None
    };
    let rx_instant = if mask & MASK_RX_INSTANT != 0 {
        let len = usize::from(*tail.get(at).ok_or(ParseError::Truncated)?);
        Some(slice(tail, at + 1, len)?.to_vec())
    } else {
        None
    };
    Ok((token, psm, rx_instant))
}

/// Why [`ReceiverAdvertisement::try_parse`] rejected its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Longer than any legal service data.
    TooLong,
    /// A length field points past the end of the input.
    Truncated,
    /// Not a `0x48`/`0x4a`/`0x20` advertisement, or a DCT element of an unknown shape.
    BadHeader,
    /// The receiver is not visible to everyone: its name is not in clear.
    NotEveryone,
    /// The display name is not UTF-8 (or, in the DCT form, carries control characters).
    BadName,
    /// A DCT data element repeats a type.
    DuplicateField,
    /// A DCT service hash element does not carry the Sharing hash.
    BadServiceHash,
    /// A DCT PSM element is not two bytes or is zero.
    BadPsm,
    /// A DCT advertisement without the required service hash, PSM or device name.
    MissingField,
}

/// `len` bytes of `b` from `from`, or `Truncated`.
fn slice(b: &[u8], from: usize, len: usize) -> Result<&[u8], ParseError> {
    let end = from.checked_add(len).ok_or(ParseError::Truncated)?;
    b.get(from..end).ok_or(ParseError::Truncated)
}

fn array<const N: usize>(b: &[u8], from: usize) -> Result<[u8; N], ParseError> {
    slice(b, from, N)?
        .try_into()
        .map_err(|_| ParseError::Truncated)
}

fn truncate_name(name: &str) -> &str {
    crate::utils::endpoint_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 132-byte service data captured from a Pixel 9 Pro, with endpoint id and name replaced
    /// by "PKT1" / "Packet Linux Test 01" (quickshare-direct examples/rx_advert.rs).
    const CAPTURE: [u8; 132] = [
        0x48, 0xfc, 0x9f, 0x5e, 0x00, 0x00, 0x00, 0x37, 0x23, 0xfc, 0x9f, 0x5e, 0x50, 0x4b, 0x54,
        0x31, 0x26, 0x22, 0x4a, 0x22, 0x71, 0x16, 0x9c, 0x15, 0x99, 0xa2, 0x44, 0xaf, 0x44, 0xb0,
        0x17, 0x9c, 0x0f, 0x23, 0x14, 0x50, 0x61, 0x63, 0x6b, 0x65, 0x74, 0x20, 0x4c, 0x69, 0x6e,
        0x75, 0x78, 0x20, 0x54, 0x65, 0x73, 0x74, 0x20, 0x30, 0x31, 0xfc, 0x41, 0x16, 0xb6, 0x17,
        0x20, 0x00, 0x00, 0x62, 0xf1, 0x03, 0x00, 0x82, 0x3f, 0xa0, 0x17, 0xfd, 0xf1, 0x70, 0x59,
        0x6e, 0x1e, 0xd3, 0x4d, 0xe0, 0x92, 0x56, 0x4d, 0x66, 0xd4, 0x29, 0x0f, 0x0f, 0x8f, 0x15,
        0x05, 0x34, 0x7b, 0x13, 0x23, 0x01, 0xea, 0x7f, 0x92, 0xa8, 0xd8, 0xd4, 0x61, 0x84, 0x15,
        0x05, 0x3f, 0x00, 0x00, 0x84, 0x15, 0x06, 0x2d, 0x00, 0x00, 0x84, 0x15, 0x04, 0x7f, 0x1f,
        0x00, 0x84, 0x15, 0x07, 0x2d, 0x1f, 0x00, 0x83, 0x15, 0x01, 0x15, 0x7c,
    ];

    fn captured() -> ReceiverAdvertisement {
        ReceiverAdvertisement {
            endpoint_id: *b"PKT1",
            device_type: 1,
            identity: [
                0x4a, 0x22, 0x71, 0x16, 0x9c, 0x15, 0x99, 0xa2, 0x44, 0xaf, 0x44, 0xb0, 0x17, 0x9c,
                0x0f, 0x23,
            ],
            name: "Packet Linux Test 01".into(),
            bt_mac: [0xfc, 0x41, 0x16, 0xb6, 0x17, 0x20],
            psm: Some(0x0082),
            token: [0x62, 0xf1],
            rx_instant: Some(CAPTURE[CAPTURE.len() - 63..].to_vec()),
            dct: false,
        }
    }

    #[test]
    fn builds_the_captured_bytes() {
        assert_eq!(captured().to_bytes(), CAPTURE);
    }

    #[test]
    fn parses_the_captured_bytes() {
        assert_eq!(ReceiverAdvertisement::parse(&CAPTURE), Some(captured()));
    }

    #[test]
    fn laptop_without_psm_layout() {
        let mut adv = ReceiverAdvertisement::new(*b"ABCD", DEVICE_TYPE_LAPTOP, [7; 16], "Desk");
        adv.bt_mac = [0; 6];
        adv.token = [0x62, 0xf1];
        let b = adv.to_bytes();
        assert_eq!(&b[..8], [0x48, 0xfc, 0x9f, 0x5e, 0, 0, 0, 0x27]);
        assert_eq!(&b[8..16], [0x23, 0xfc, 0x9f, 0x5e, b'A', b'B', b'C', b'D']);
        // info length 1 + 16 + 1 + 4, header byte 0x26 (visible laptop)
        assert_eq!(&b[16..18], [22, 0x26]);
        assert_eq!(&b[18..34], [7; 16]);
        assert_eq!(&b[34..39], [4, b'D', b'e', b's', b'k']);
        assert_eq!(&b[39..47], [0; 8]);
        assert_eq!(&b[47..49], [0x62, 0xf1]);
        // No PSM and none of the captured opaque field: the mask is empty and nothing follows.
        assert_eq!(&b[49..], [0x00]);
        assert_eq!(ReceiverAdvertisement::parse(&b), Some(adv));
    }

    /// Captured from a Galaxy A55 (Samsung Quick Share): the endpoint info carries 3 extra bytes
    /// after the name and the advertisement ends with the token, with no mask byte at all.
    #[test]
    fn samsung_advertisement_without_mask_byte() {
        const SAMSUNG: [u8; 61] = [
            0x48, 0xfc, 0x9f, 0x5e, 0, 0, 0, 0x33, 0x23, 0xfc, 0x9f, 0x5e, b'Y', b'Q', b'I', b'Z',
            0x22, 0x22, 0xa0, 0x67, 0x41, 0x08, 0x51, 0x41, 0x8d, 0xc0, 0xbf, 0xbf, 0xd8, 0xd9,
            0x5f, 0xaf, 0xd0, 0x3e, 0x0d, b'G', b'a', b'l', b'a', b'x', b'y', b' ', b'A', b'5',
            b'5', b' ', b'5', b'G', 0x02, 0x01, 0x01, 0xc0, 0x7a, 0xd6, 0x1d, 0x7f, 0xe6, 0, 0,
            0x07, 0x59,
        ];
        let adv = ReceiverAdvertisement::parse(&SAMSUNG).expect("parsed");
        assert_eq!(adv.endpoint_id, *b"YQIZ");
        assert_eq!(adv.name, "Galaxy A55 5G");
        assert_eq!(adv.psm, None);
        assert_eq!(adv.rx_instant, None);
    }

    #[test]
    fn psm_is_big_endian_after_the_mask() {
        let mut adv = ReceiverAdvertisement::new(*b"ABCD", 3, [0; 16], "x");
        adv.psm = Some(0x00a1);
        let b = adv.to_bytes();
        let off = 8 + b[7] as usize;
        assert_eq!(&b[off + 2..off + 5], [0x01, 0x00, 0xa1]);
        assert_eq!(ReceiverAdvertisement::parse(&b).unwrap().psm, Some(0xa1));
    }

    #[test]
    fn the_token_is_random_per_advertisement() {
        let tokens: std::collections::HashSet<_> = (0..32)
            .map(|_| ReceiverAdvertisement::new(*b"ABCD", 3, [0; 16], "x").token)
            .collect();
        assert!(tokens.len() > 1);
    }

    #[test]
    fn long_names_are_cut_on_a_char_boundary() {
        let adv = ReceiverAdvertisement::new(*b"ABCD", 3, [0; 16], &"è".repeat(80));
        assert_eq!(adv.name.len(), 100);
        let parsed = ReceiverAdvertisement::parse(&adv.to_bytes()).unwrap();
        assert_eq!(parsed.name, adv.name);
        assert!(adv.to_bytes().len() < 251);
    }

    /// The mDNS `n` record and the BLE endpoint info must be the same bytes: the phone merges
    /// the two mediums of an endpoint id only then.
    #[test]
    fn endpoint_info_matches_the_mdns_record() {
        let identity = [0x5au8; 16];
        let adv = ReceiverAdvertisement::new(*b"ABCD", DEVICE_TYPE_LAPTOP, identity, "Desk");
        let n = crate::utils::gen_mdns_endpoint_info(DEVICE_TYPE_LAPTOP, &identity, "Desk");
        assert_eq!(
            adv.endpoint_info(),
            URL_SAFE_NO_PAD.decode(n).unwrap(),
            "mDNS and BLE advertise one endpoint info"
        );
    }

    #[test]
    fn legacy_header_matches_nearby_reference_vectors() {
        assert_eq!(murmur3_x64_128(b"", 0), (0, 0));
        assert_eq!(
            murmur3_x64_128(b"The quick brown fox jumps over the lazy dog", 0),
            (0xe34bbc7bbc071b6c, 0x7a433ca9c49a9347)
        );
        let captured = parse_legacy_header(b"VSAACFACACADAQFQDlSFAAA").unwrap();
        assert_eq!(captured.advert_hash, [0x50, 0x0e, 0x54, 0x85]);
        assert!(parse_legacy_header(b"VSAACFACACADAQFQDlSFAAA!").is_none());
        assert!(parse_legacy_header(&[0x4a, 0x17]).is_none());
        assert!(parse_legacy_header(b"a random Bluetooth name").is_none());
    }

    #[test]
    fn legacy_header_byte_0_has_the_known_version_and_the_inferred_rest() {
        let captured = URL_SAFE_NO_PAD.decode("VSAACFACACADAQFQDlSFAAA").unwrap();
        assert_eq!(captured[0], 0x55);
        assert_eq!(captured[0] >> 5, HEADER_VERSION);
        // Known from PR #8: version 2, not extended, one slot.
        assert_eq!(legacy_header_flags(false, 1), 0x41);
        // (inferred) layout: it reproduces the captured byte with the extended bit and 5 slots.
        assert_eq!(legacy_header_flags(true, 5), captured[0]);
        let full = ReceiverAdvertisement::new(*b"ABCD", 3, [7; 16], "laptop").to_bytes();
        let ours = legacy_header_bytes(SERVICE_ID, &[7u8; DUMMY_SERVICE_ID_LEN], &full);
        assert_eq!(ours[0], 0x41);
        assert_eq!(ours[0] >> 5, captured[0] >> 5);
        assert_eq!(&ours[15..], [0, 0], "PSM 0: the GATT weave socket");
    }

    #[test]
    fn the_advert_hash_changes_with_the_record_and_the_dummy_service_id() {
        let a = ReceiverAdvertisement::new(*b"ABCD", 3, [7; 16], "laptop").to_bytes();
        let b = ReceiverAdvertisement::new(*b"ABCD", 3, [7; 16], "laptop2").to_bytes();
        let hash = |dummy: u8, full: &[u8]| {
            parse_legacy_header(
                URL_SAFE_NO_PAD
                    .encode(legacy_header_bytes(
                        SERVICE_ID,
                        &[dummy; DUMMY_SERVICE_ID_LEN],
                        full,
                    ))
                    .as_bytes(),
            )
            .unwrap()
            .advert_hash
        };
        assert_eq!(hash(7, &a), hash(7, &a));
        assert_ne!(hash(7, &a), hash(7, &b));
        assert_ne!(hash(7, &a), hash(8, &a));
    }

    #[test]
    fn generated_legacy_header_is_bloom_filtered_and_points_to_gatt() {
        let full = ReceiverAdvertisement::new(*b"ABCD", 3, [7; 16], "laptop").to_bytes();
        let dummy = [7u8; DUMMY_SERVICE_ID_LEN];
        let raw = legacy_header_bytes(SERVICE_ID, &dummy, &full);
        assert_eq!(raw.len(), 17);
        assert!(bloom_contains(&raw[1..11], SERVICE_ID.as_bytes()));
        assert!(!bloom_contains(&raw[1..11], b"SomeOtherService"));
        let encoded = build_legacy_header(&full);
        assert_eq!(encoded.len(), 23);
        assert!(parse_legacy_header(&encoded).is_some());
        assert!(ReceiverAdvertisement::parse(&encoded).is_none());
        assert!(regular_length(&full).is_some_and(|n| n == full.len()));
        assert_eq!(regular_length(&[0x49]), None);
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert_eq!(ReceiverAdvertisement::parse(&[]), None);
        assert_eq!(
            ReceiverAdvertisement::parse(&[0x49, 0xfc, 0x9f, 0x5e]),
            None
        );
        for n in 0..CAPTURE.len() - 1 {
            // Cuts inside the fixed part must never panic; most return None.
            let _ = ReceiverAdvertisement::parse(&CAPTURE[..n]);
        }
        assert_eq!(ReceiverAdvertisement::parse(&CAPTURE[..62]), None);
    }

    /// A valid advertisement with the given endpoint info header byte.
    fn with_info_header(hdr: u8) -> Vec<u8> {
        let mut b = ReceiverAdvertisement::new(*b"ABCD", 1, [7; 16], "Pixel").to_bytes();
        b[17] = hdr;
        b
    }

    /// A hand-built fast-form advertisement: no service hash, 1-byte length, no classic
    /// Bluetooth address, and an endpoint info far past upstream's 17-byte fast cap.
    fn fast_bytes() -> Vec<u8> {
        fast_bytes_with(0)
    }

    /// `extra_uwb`: bytes of a UWB address appended inside the declared payload.
    fn fast_bytes_with(extra_uwb: usize) -> Vec<u8> {
        let info = crate::utils::gen_endpoint_info(1, &[7; 16], "Packet Linux Test 01");
        let mut data = vec![DATA_VERSION_PCP];
        data.extend_from_slice(b"PKT1");
        data.push(info.len() as u8);
        data.extend_from_slice(&info);
        if extra_uwb > 0 {
            data.push(extra_uwb as u8);
            data.extend(vec![0xa5; extra_uwb]);
        }
        let mut out = vec![HEADER_FAST, data.len() as u8];
        out.extend_from_slice(&data);
        out.extend_from_slice(&[0x62, 0xf1]);
        out.push(MASK_PSM);
        out.extend_from_slice(&0x0082u16.to_be_bytes());
        out
    }

    #[test]
    fn parses_the_fast_form() {
        let adv = ReceiverAdvertisement::parse(&fast_bytes()).expect("fast form parsed");
        assert_eq!(adv.endpoint_id, *b"PKT1");
        assert_eq!(adv.name, "Packet Linux Test 01");
        assert_eq!(adv.device_type, 1);
        assert_eq!(adv.psm, Some(0x0082));
        assert_eq!(adv.token, [0x62, 0xf1]);
        assert_eq!(adv.bt_mac, [0; 6], "the fast form carries no address");
        assert_eq!(adv.identity, [7; 16]);
        assert!(!adv.dct);
        assert!(adv.rx_instant.is_none());
    }

    #[test]
    fn the_fast_form_takes_a_uwb_address_after_the_endpoint_info() {
        assert!(ReceiverAdvertisement::parse(&fast_bytes_with(6)).is_some());
        // A length byte that does not match the bytes that follow it is junk. The uwb length
        // byte sits right before the 6 address bytes at the end of the declared data.
        let mut bad = fast_bytes_with(6);
        let uwb_len_at = 2 + usize::from(bad[1]) - 1 - 6;
        bad[uwb_len_at] = 5;
        assert_eq!(
            ReceiverAdvertisement::try_parse(&bad),
            Err(ParseError::BadHeader)
        );
    }

    #[test]
    fn every_truncation_of_the_fast_form_fails_cleanly() {
        let fast = fast_bytes();
        for n in 0..fast.len() {
            let _ = ReceiverAdvertisement::try_parse(&fast[..n]);
        }
        assert_eq!(
            ReceiverAdvertisement::try_parse(&fast[..8]),
            Err(ParseError::Truncated)
        );
    }

    /// A hand-built DCT advertisement: version byte, then the three required data elements.
    /// `order`: the upstream order (hash, PSM, name); a reordered variant is tested below.
    fn dct_bytes(name: &[u8], order: u8, dedup_byte: u8) -> Vec<u8> {
        let psm = |out: &mut Vec<u8>| {
            out.push((2 << 4) | DCT_PSM_TYPE);
            out.extend_from_slice(&0x0082u16.to_be_bytes());
        };
        let hash = |out: &mut Vec<u8>| {
            out.push((2 << 4) | DCT_SERVICE_HASH_TYPE);
            out.extend_from_slice(&DCT_SERVICE_HASH);
        };
        let device = |out: &mut Vec<u8>| {
            out.push(((1 + name.len() as u8) << 4) | DCT_DEVICE_INFO_TYPE);
            out.push(dedup_byte);
            out.extend_from_slice(name);
        };
        let mut out = vec![HEADER_DCT];
        match order {
            0 => {
                hash(&mut out);
                psm(&mut out);
                device(&mut out);
            }
            _ => {
                device(&mut out);
                psm(&mut out);
                hash(&mut out);
            }
        }
        out
    }

    #[test]
    fn parses_the_dct_form() {
        let adv = ReceiverAdvertisement::parse(&dct_bytes(b"Pixel", 0, 0)).expect("DCT parsed");
        assert_eq!(adv.name, "Pixel");
        assert_eq!(adv.psm, Some(0x0082));
        assert!(adv.dct);
        assert_eq!(
            adv.endpoint_id, [0; 4],
            "the DCT form carries no endpoint id"
        );
        assert_eq!(adv.device_type, 0);
        assert_eq!(adv.identity, [0; 16]);
        assert_eq!(adv.bt_mac, [0; 6]);
        assert_eq!(adv.token, [0; 2]);
        assert!(adv.rx_instant.is_none());
    }

    #[test]
    fn the_dct_form_takes_the_elements_in_any_order_and_skips_unknown_ones() {
        // Device information first, then PSM, then the hash: not the upstream order.
        let reordered = dct_bytes(b"Pixel", 1, 0x42);
        let adv = ReceiverAdvertisement::parse(&reordered).expect("reordered elements parsed");
        assert_eq!(adv.name, "Pixel");
        // An unknown element (type 9, one byte: header 0b0001_1001) is skipped, not a reason
        // to refuse the phone.
        let mut with_unknown = dct_bytes(b"Pixel", 0, 0);
        with_unknown.insert(1, 0x19);
        with_unknown.insert(2, b'x');
        let adv = ReceiverAdvertisement::parse(&with_unknown).expect("unknown element skipped");
        assert_eq!(adv.name, "Pixel");
        // A one-byte element header whose type is 0 (0b0000_0000) is malformed.
        let mut zero_type = dct_bytes(b"Pixel", 0, 0);
        zero_type.insert(1, 0x00);
        assert_eq!(
            ReceiverAdvertisement::try_parse(&zero_type),
            Err(ParseError::BadHeader)
        );
    }

    #[test]
    fn the_dct_form_caps_the_distinct_fields() {
        let mut full = dct_bytes(b"Pixel", 0, 0);
        // 3 required + 29 unknown = 32 distinct fields, the most a legal DCT advertisement
        // carries. Each unknown element: two-byte header (length 1), type, value.
        for n in 0..29u8 {
            full.extend_from_slice(&[0x80 | 1, 8 + n, b'x']);
        }
        assert!(ReceiverAdvertisement::parse(&full).is_some());
        // One more field is junk, however small it is.
        full.extend_from_slice(&[0x80 | 1, 200, b'x']);
        assert_eq!(
            ReceiverAdvertisement::try_parse(&full),
            Err(ParseError::TooLong)
        );
    }

    #[test]
    fn the_dct_form_rejects_the_documented_cases() {
        // Bad version byte: not a DCT advertisement at all.
        assert_eq!(
            ReceiverAdvertisement::try_parse(&[0x21, 0x25, 0x6d, 0xfd]),
            Err(ParseError::BadHeader)
        );
        // A repeated type: the PSM element again, well-formed.
        let mut dup = dct_bytes(b"Pixel", 0, 0);
        dup.extend_from_slice(&[(2 << 4) | DCT_PSM_TYPE, 0x00, 0x82]);
        assert_eq!(
            ReceiverAdvertisement::try_parse(&dup),
            Err(ParseError::DuplicateField)
        );
        // A missing required field: the service hash element cut out whole.
        let mut missing = dct_bytes(b"Pixel", 0, 0);
        missing.drain(1..4);
        assert_eq!(
            ReceiverAdvertisement::try_parse(&missing),
            Err(ParseError::MissingField)
        );
        // A wrong service hash: the hash value sits at bytes 2..4.
        let mut hash = dct_bytes(b"Pixel", 0, 0);
        hash[2] ^= 1;
        assert_eq!(
            ReceiverAdvertisement::try_parse(&hash),
            Err(ParseError::BadServiceHash)
        );
        // A zero PSM: the PSM value sits at bytes 5..7.
        let mut zero = dct_bytes(b"Pixel", 0, 0);
        zero[5] = 0;
        zero[6] = 0;
        assert_eq!(
            ReceiverAdvertisement::try_parse(&zero),
            Err(ParseError::BadPsm)
        );
        // A PSM that is not two bytes.
        let mut short = vec![HEADER_DCT];
        short.push((1 << 4) | DCT_PSM_TYPE);
        short.push(0x82);
        short.extend_from_slice(&[(2 << 4) | DCT_SERVICE_HASH_TYPE, 0x6d, 0xfd]);
        short.extend_from_slice(&[(6 << 4) | DCT_DEVICE_INFO_TYPE, 0]);
        short.extend_from_slice(b"Pixel");
        assert_eq!(
            ReceiverAdvertisement::try_parse(&short),
            Err(ParseError::BadPsm)
        );
        // A name with a control character, and a name that is not UTF-8.
        assert_eq!(
            ReceiverAdvertisement::try_parse(&dct_bytes(b"Pi\x01xel", 0, 0)),
            Err(ParseError::BadName)
        );
        assert!(ReceiverAdvertisement::parse(&dct_bytes(&[0xff, 0xfe], 0, 0)).is_none());
        // A name element without a name byte: the device element header sits at byte 7.
        let mut no_name = dct_bytes(b"Pixel", 0, 0);
        no_name[7] = (1 << 4) | DCT_DEVICE_INFO_TYPE;
        no_name.truncate(9);
        assert_eq!(
            ReceiverAdvertisement::try_parse(&no_name),
            Err(ParseError::BadHeader)
        );
        // Eight name bytes, one over the cap: a length 9 element needs the two-byte header.
        let mut long = vec![HEADER_DCT];
        long.extend_from_slice(&[(2 << 4) | DCT_SERVICE_HASH_TYPE, 0x6d, 0xfd]);
        long.extend_from_slice(&[(2 << 4) | DCT_PSM_TYPE, 0x00, 0x82]);
        long.extend_from_slice(&[0x80 | 9, DCT_DEVICE_INFO_TYPE, 0]);
        long.extend_from_slice(&[b'x'; 8]);
        assert_eq!(
            ReceiverAdvertisement::try_parse(&long),
            Err(ParseError::BadHeader)
        );
        // An element value that runs past the end.
        assert_eq!(
            ReceiverAdvertisement::try_parse(&[HEADER_DCT, 0x25, 0x6d]),
            Err(ParseError::Truncated)
        );
    }

    /// The DCT service hash, recomputed from the HKDF the constant is documented with.
    #[test]
    fn dct_service_hash_matches_its_hkdf_definition() {
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(Some(b"DCT Protocol"), b"NearbySharing");
        let mut okm = [0u8; 2];
        hk.expand(b"Service ID Hash", &mut okm)
            .expect("2 bytes always fit");
        assert_eq!(okm, DCT_SERVICE_HASH);
    }

    #[test]
    fn rejects_receivers_that_hide_the_name() {
        assert_eq!(
            ReceiverAdvertisement::try_parse(&with_info_header(0x22)).map(|a| a.name),
            Ok("Pixel".into())
        );
        assert_eq!(
            ReceiverAdvertisement::try_parse(&with_info_header(0x32)),
            Err(ParseError::NotEveryone)
        );
    }

    #[test]
    fn every_truncation_of_the_capture_fails_cleanly() {
        // Cut right after the token it is the Samsung form (no mask byte), which is valid.
        let after_token = 8 + usize::from(CAPTURE[7]) + 2;
        for n in 0..CAPTURE.len() {
            let parsed = ReceiverAdvertisement::try_parse(&CAPTURE[..n]);
            assert_eq!(parsed.is_ok(), n == after_token, "{n}");
        }
        assert!(ReceiverAdvertisement::try_parse(&CAPTURE).is_ok());
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 0x1234_5678_u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        for round in 0..2000 {
            let mut b: Vec<u8> = (0..round % 140).map(|_| next()).collect();
            // Make some of them pass the header so that the inner lengths get exercised.
            if round % 2 == 0 && b.len() > 8 {
                b[..4].copy_from_slice(&[0x48, 0xfc, 0x9f, 0x5e]);
                b[4..7].copy_from_slice(&[0, 0, 0]);
            }
            let _ = ReceiverAdvertisement::try_parse(&b);
        }
        let huge = [0x48, 0xfc, 0x9f, 0x5e, 0xff, 0xff, 0xff, 0xff, 0x23];
        assert_eq!(
            ReceiverAdvertisement::try_parse(&huge),
            Err(ParseError::Truncated)
        );
        assert_eq!(
            ReceiverAdvertisement::try_parse(&[0; 600]),
            Err(ParseError::TooLong)
        );
        assert_eq!(
            ReceiverAdvertisement::try_parse(&[0x00, 1, 2]),
            Err(ParseError::BadHeader)
        );
    }

    /// Randomized, deterministic: whatever arrives over the air, `try_parse` returns instead
    /// of panicking, and what `to_bytes` builds still parses back to the same value.
    #[test]
    fn random_input_never_panics_and_valid_input_still_parses() {
        use rand::{Rng, RngExt, SeedableRng, rngs::StdRng};

        let mut rng = StdRng::seed_from_u64(0x5eed_0fef_3a11);

        // 1. Random byte strings of length 0..=64, a third of them behind a valid header
        //    (so the inner lengths are reached).
        for round in 0..100_000 {
            let len = rng.random_range(0..=64usize);
            let mut b = vec![0u8; len];
            rng.fill_bytes(&mut b);
            if round % 3 == 0 && len >= 8 {
                b[..4].copy_from_slice(&[HEADER, 0xfc, 0x9f, 0x5e]);
                // Small data length, so the data part is often inside the input.
                b[4..7].copy_from_slice(&[0, 0, 0]);
                b[7] = rng.random_range(0..=64u8);
            }
            let _ = ReceiverAdvertisement::try_parse(&b);
        }

        // 1b. The same behind the fast and the DCT header.
        for round in 0..100_000 {
            let len = rng.random_range(0..=64usize);
            let mut b = vec![0u8; len];
            rng.fill_bytes(&mut b);
            if !b.is_empty() {
                b[0] = if round % 2 == 0 {
                    HEADER_FAST
                } else {
                    HEADER_DCT
                };
            }
            if b.len() > 1 && round % 3 == 0 {
                // A small length, so the inner structure is often reached.
                b[1] = rng.random_range(0..=32u8);
            }
            let _ = ReceiverAdvertisement::try_parse(&b);
        }

        // 2. Mutations of valid advertisements: flipped bytes, cuts and appended bytes.
        let valid = [
            CAPTURE.to_vec(),
            captured().to_bytes(),
            {
                let mut adv = ReceiverAdvertisement::new(*b"ABCD", 3, [7; 16], "Desk");
                adv.psm = Some(0x00a1);
                adv.to_bytes()
            },
            fast_bytes(),
            fast_bytes_with(6),
            dct_bytes(b"Pixel", 0, 0),
            dct_bytes(b"Pixel", 1, 0x42),
        ];
        for round in 0..50_000 {
            let mut b = valid[round % valid.len()].clone();
            for _ in 0..rng.random_range(1..=3usize) {
                match rng.random_range(0..3u8) {
                    0 if !b.is_empty() => {
                        let at = rng.random_range(0..b.len());
                        b[at] = rng.random();
                    }
                    1 => b.truncate(rng.random_range(0..=b.len())),
                    _ => b.push(rng.random()),
                }
            }
            let _ = ReceiverAdvertisement::try_parse(&b);
        }

        // 3. Valid advertisements of random shape parse back to what was built.
        for _ in 0..5_000 {
            let name: String = (0..rng.random_range(0..=30usize))
                .map(|_| char::from(rng.random_range(b'a'..=b'z')))
                .collect();
            let mut adv = ReceiverAdvertisement::new(
                rng.random(),
                rng.random_range(0..=7u8),
                rng.random(),
                &name,
            );
            adv.bt_mac = rng.random();
            adv.psm = rng.random::<bool>().then(|| rng.random());
            adv.rx_instant = rng.random::<bool>().then(|| {
                let mut d = vec![0u8; rng.random_range(0..=255usize)];
                rng.fill_bytes(&mut d);
                d
            });
            let parsed = ReceiverAdvertisement::try_parse(&adv.to_bytes());
            assert_eq!(parsed, Ok(adv));
        }
    }
}
