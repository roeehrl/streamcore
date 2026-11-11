//! Annex-B access units: NAL splitting and types, moving parameter sets out of an access unit,
//! the avcC configuration record and emulation prevention. The AVCC conversion itself is
//! `streamcore_video::bitstream::annexb_to_length_prefixed`.

/// NAL unit types the encoder adapter looks at.
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Splits an Annex-B byte stream into NAL units (start codes removed). Accepts 3- and 4-byte
/// start codes; trailing zero bytes before a start code belong to the start code, not the NAL.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, (_, payload)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |(sc, _)| *sc);
        while end > *payload && data[end - 1] == 0 {
            end -= 1;
        }
        if end > *payload {
            out.push(&data[*payload..end]);
        }
    }
    out
}

/// `nal_unit_type` of a NAL unit.
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// Summary of one access unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessUnitInfo {
    pub nal_types: Vec<u8>,
    pub idr: bool,
    pub sps: bool,
    pub pps: bool,
}

pub fn inspect(au: &[u8]) -> AccessUnitInfo {
    let nal_types: Vec<u8> = split(au).iter().filter_map(|n| nal_type(n)).collect();
    AccessUnitInfo {
        idr: nal_types.contains(&NAL_IDR),
        sps: nal_types.contains(&NAL_SPS),
        pps: nal_types.contains(&NAL_PPS),
        nal_types,
    }
}

/// First NAL unit of the given type.
pub fn find(au: &[u8], ty: u8) -> Option<&[u8]> {
    split(au).into_iter().find(|n| nal_type(n) == Some(ty))
}

/// Removes emulation-prevention bytes (`00 00 03` -> `00 00`) to get the RBSP.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Adds emulation-prevention bytes to an RBSP (`00 00 0x` with x <= 3 -> `00 00 03 0x`).
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 1);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// One encoder output split for the wire: the access unit (Annex-B, 4-byte start codes) without
/// parameter sets or delimiters, and the SPS and PPS it carried (they travel in the avcC record).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitAccessUnit {
    pub annexb: Vec<u8>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub idr: bool,
}

/// Splits an Annex-B access unit into its picture data and its parameter sets.
pub fn split_access_unit(au: &[u8]) -> SplitAccessUnit {
    let mut out = SplitAccessUnit {
        annexb: Vec::with_capacity(au.len()),
        ..SplitAccessUnit::default()
    };
    for nal in split(au) {
        match nal_type(nal) {
            Some(NAL_SPS) => out.sps = Some(nal.to_vec()),
            Some(NAL_PPS) => out.pps = Some(nal.to_vec()),
            Some(NAL_AUD) => {}
            ty => {
                out.idr |= ty == Some(NAL_IDR);
                out.annexb.extend_from_slice(&[0, 0, 0, 1]);
                out.annexb.extend_from_slice(nal);
            }
        }
    }
    out
}

/// The `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 5.3.3.1) for one SPS and one PPS, with
/// 4-byte NAL lengths. High-profile records carry the 4:2:0, 8-bit extension. `None` if a unit
/// is too short or too long.
pub fn avcc_config(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() || sps.len() > 0xffff || pps.len() > 0xffff {
        return None;
    }
    let mut out = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        out.extend_from_slice(&[0xfc | 1, 0xf8, 0xf8, 0]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mixed_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        let nals = split(&data);
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]
        );
        let info = inspect(&data);
        assert_eq!(info.nal_types, vec![NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(info.idr && info.sps && info.pps);
        assert_eq!(find(&data, NAL_PPS), Some(&[0x68, 3][..]));
    }

    #[test]
    fn no_start_code() {
        assert!(split(&[1, 2, 3]).is_empty());
        assert!(split(&[]).is_empty());
        assert!(!inspect(&[0, 0, 1, 0x41, 0]).idr);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(
            unescape(&[0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3]),
            vec![0x67, 0, 0, 1, 0, 0, 0, 0]
        );
        // A 3 after a single zero is data.
        assert_eq!(unescape(&[0, 3, 0]), vec![0, 3, 0]);
    }

    #[test]
    fn escape_round_trips() {
        let rbsp = [0x42, 0, 0, 1, 0, 0, 0, 0, 0, 3, 0, 0, 2, 7];
        let escaped = escape(&rbsp);
        assert_eq!(
            escaped,
            vec![0x42, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 3, 0, 0, 3, 2, 7]
        );
        assert_eq!(unescape(&escaped), rbsp.to_vec());
        assert_eq!(escape(&[0, 0, 4]), vec![0, 0, 4]);
    }

    #[test]
    fn access_unit_split_moves_parameter_sets_out() {
        let au = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 77, 0, 40, 0, 0, 1, 0x68, 0xce, 0, 0, 1,
            0x65, 1, 2,
        ];
        let s = split_access_unit(&au);
        assert!(s.idr);
        assert_eq!(s.sps.as_deref(), Some(&[0x67, 77, 0, 40][..]));
        assert_eq!(s.pps.as_deref(), Some(&[0x68, 0xce][..]));
        assert_eq!(s.annexb, vec![0, 0, 0, 1, 0x65, 1, 2]);
        let delta = split_access_unit(&[0, 0, 1, 0x41, 9]);
        assert!(!delta.idr && delta.sps.is_none());
        assert_eq!(delta.annexb, vec![0, 0, 0, 1, 0x41, 9]);
    }

    #[test]
    fn avcc_record() {
        let sps = [0x67, 77, 0x40, 31, 0xaa];
        let pps = [0x68, 0xce, 0x3c];
        let c = avcc_config(&sps, &pps).unwrap_or_default();
        assert_eq!(&c[..6], &[1, 77, 0x40, 31, 0xff, 0xe1]);
        assert_eq!(&c[6..8], &[0, 5]);
        assert_eq!(&c[8..13], &sps);
        assert_eq!(&c[13..16], &[1, 0, 3]);
        assert_eq!(&c[16..], &pps);
        let high = avcc_config(&[0x67, 100, 0, 40], &pps).unwrap_or_default();
        assert_eq!(&high[high.len() - 4..], &[0xfd, 0xf8, 0xf8, 0]);
        assert!(avcc_config(&[0x67, 77], &pps).is_none());
    }
}
