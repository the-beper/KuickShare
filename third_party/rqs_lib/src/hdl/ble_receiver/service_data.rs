//! The 0xFEF3 receiver advertisement (BLE service data, also served from GATT slot 0).
//!
//! Layout (reverse-engineered from a Pixel 9 Pro capture, see docs):
//! `48 | fc9f5e | u32 BE len | DATA | token(2) | mask | [psm(2)] | [len | rx_instant]`
//! with `DATA = 23 | fc9f5e | endpoint_id(4) | info_len | endpoint_info | mac(6) | 00 00`.

use super::socket::SERVICE_HASH;

/// 16-bit service UUID of the receiver advertisement and the GATT service.
pub const SERVICE_UUID: u16 = 0xFEF3;
pub const DEVICE_TYPE_LAPTOP: u8 = 3;
/// Display names are cut at this many bytes so the advertisement stays well under 251 bytes.
pub const MAX_NAME_LEN: usize = 100;

const HEADER: u8 = 0x48;
const HEADER_FAST: u8 = 0x4a;
/// Bit of the endpoint info header that is set when the name is hidden.
const INFO_HIDDEN: u8 = 0x10;
/// Service data is at most 255 bytes on the air; anything larger is not ours.
const MAX_PARSE_LEN: usize = 512;
const DATA_VERSION_PCP: u8 = 0x23;
const MASK_PSM: u8 = 0x01;
const MASK_RX_INSTANT: u8 = 0x02;

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
        }
    }

    /// Nearby Share application advertisement (same layout as the mDNS `n` record).
    pub fn endpoint_info(&self) -> Vec<u8> {
        let name = truncate_name(&self.name).as_bytes();
        let mut info = Vec::with_capacity(18 + name.len());
        info.push((1 << 5) | ((self.device_type & 0x7) << 1));
        info.extend_from_slice(&self.identity);
        info.push(name.len() as u8);
        info.extend_from_slice(name);
        info
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

    /// Parses the advertisement of a receiver that is visible to everyone (name in clear).
    /// The "fast" form (`0x4a`, no name and no PSM) is rejected on purpose.
    pub fn try_parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() > MAX_PARSE_LEN {
            return Err(ParseError::TooLong);
        }
        let (&header, rest) = bytes.split_first().ok_or(ParseError::Truncated)?;
        match header {
            HEADER => {}
            HEADER_FAST => return Err(ParseError::FastForm),
            _ => return Err(ParseError::BadHeader),
        }
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

        Ok(Self {
            endpoint_id,
            device_type,
            identity,
            name,
            bt_mac,
            psm,
            token,
            rx_instant,
        })
    }
}

/// Why [`ReceiverAdvertisement::try_parse`] rejected its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Longer than any legal service data.
    TooLong,
    /// A length field points past the end of the input.
    Truncated,
    /// Not a `0x48 | fc9f5e` advertisement.
    BadHeader,
    /// The `0x4a` form that carries neither name nor PSM.
    FastForm,
    /// The receiver is not visible to everyone: its name is not in clear.
    NotEveryone,
    /// The display name is not UTF-8.
    BadName,
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
    if name.len() <= MAX_NAME_LEN {
        return name;
    }
    let mut end = MAX_NAME_LEN;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
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

    #[test]
    fn rejects_the_fast_form() {
        let fast = [0x4a, 0x05, 1, 2, 3, 4, 5, 0x62, 0xf1];
        assert_eq!(
            ReceiverAdvertisement::try_parse(&fast),
            Err(ParseError::FastForm)
        );
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

        // 2. Mutations of valid advertisements: flipped bytes, cuts and appended bytes.
        let valid = [CAPTURE.to_vec(), captured().to_bytes(), {
            let mut adv = ReceiverAdvertisement::new(*b"ABCD", 3, [7; 16], "Desk");
            adv.psm = Some(0x00a1);
            adv.to_bytes()
        }];
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
