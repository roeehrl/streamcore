//! Just enough H.264 to tell a receiver whether a sequence gap removed a
//! reference picture (STREAMCORE-VIDEO.md 6.5: missing discardable frames do
//! not invalidate the chain): `frame_num` advances only after reference
//! pictures (H.264 7.4.3), so a gap of non-reference (discardable) pictures
//! leaves it continuous, and a gap that removed a reference picture does not.
//!
//! [`AvcParams::from_avcc`] reads `log2_max_frame_num` from the first SPS of an
//! `avcC` record; [`slice_refs`] reads `nal_ref_idc`, IDR and `frame_num` from
//! the first slice of a 4-byte length-prefixed access unit. Both return `None`
//! on anything they do not understand (another codec, a truncated record);
//! callers then treat every gap as a break, as before.
//!
//! [`VideoSignal::from_avcc`] reads the SPS's VUI `video_signal_type` (range
//! and colour description), which is where the host's encoder states a
//! generation's colour.

/// The SPS fields slice parsing needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvcParams {
    pub log2_max_frame_num: u8,
    pub separate_colour_plane: bool,
}

/// The VUI `video_signal_type` of an SPS (H.264 E.1.1). Colour code points are
/// ITU-T H.273's; each is `None` when the SPS has no colour description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSignal {
    pub full_range: bool,
    pub primaries: Option<u8>,
    pub transfer: Option<u8>,
    pub matrix: Option<u8>,
}

impl VideoSignal {
    /// From the first SPS of an `avcC` record; `None` when it has no VUI or
    /// no `video_signal_type`, or does not parse.
    pub fn from_avcc(avcc: &[u8]) -> Option<Self> {
        read_sps(first_sps(avcc)?, true)?.1
    }
}

/// One access unit's first slice: what reference structure it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceRefs {
    /// 0: a non-reference (discardable) picture.
    pub nal_ref_idc: u8,
    pub idr: bool,
    pub frame_num: u32,
}

impl AvcParams {
    /// `MaxFrameNum`.
    pub fn max_frame_num(&self) -> u32 {
        1u32 << self.log2_max_frame_num
    }

    /// From an `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15 5.3.3.1).
    pub fn from_avcc(avcc: &[u8]) -> Option<Self> {
        Self::from_sps(first_sps(avcc)?)
    }

    /// From one SPS NAL unit (header included), H.264 7.3.2.1.1.
    pub fn from_sps(nal: &[u8]) -> Option<Self> {
        Some(read_sps(nal, false)?.0)
    }
}

/// The first SPS NAL unit of an `avcC` record.
fn first_sps(avcc: &[u8]) -> Option<&[u8]> {
    if avcc.len() < 8 || avcc[0] != 1 {
        return None;
    }
    if avcc[5] & 0x1f == 0 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([avcc[6], avcc[7]]));
    avcc.get(8..8 + length)
}

/// An SPS up to `log2_max_frame_num`, and with `vui` on to the VUI's
/// `video_signal_type` (H.264 7.3.2.1.1, E.1.1).
fn read_sps(nal: &[u8], vui: bool) -> Option<(AvcParams, Option<VideoSignal>)> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(nal.get(1..)?);
    let mut r = Bits::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags, reserved bits, level_idc
    r.ue()?; // seq_parameter_set_id
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num = r.ue()?.checked_add(4)?;
    if !(4..=16).contains(&log2_max_frame_num) {
        return None;
    }
    let params = AvcParams {
        log2_max_frame_num: log2_max_frame_num as u8,
        separate_colour_plane,
    };
    if !vui {
        return Some((params, None));
    }
    match r.ue()? {
        // pic_order_cnt_type
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()?.min(255) {
                r.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if !r.bit()? {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?; // frame_crop_*_offset
        }
    }
    if !r.bit()? {
        // vui_parameters_present_flag
        return Some((params, None));
    }
    if r.bit()? && r.bits(8)? == 255 {
        // aspect_ratio_info_present_flag, aspect_ratio_idc == Extended_SAR
        r.bits(16)?;
        r.bits(16)?;
    }
    if r.bit()? {
        r.bit()?; // overscan_appropriate_flag
    }
    if !r.bit()? {
        // video_signal_type_present_flag
        return Some((params, None));
    }
    r.bits(3)?; // video_format
    let full_range = r.bit()?;
    let (primaries, transfer, matrix) = if r.bit()? {
        (
            Some(r.bits(8)? as u8),
            Some(r.bits(8)? as u8),
            Some(r.bits(8)? as u8),
        )
    } else {
        (None, None, None)
    };
    Some((
        params,
        Some(VideoSignal {
            full_range,
            primaries,
            transfer,
            matrix,
        }),
    ))
}

/// The first slice NAL unit of a length-prefixed (4-byte) access unit, or of
/// its first bytes (the slice header is within the first few dozen bytes).
pub fn slice_refs(sample: &[u8], params: &AvcParams) -> Option<SliceRefs> {
    let mut at = 0usize;
    while at + 5 <= sample.len() {
        let length = u32::from_be_bytes(sample[at..at + 4].try_into().ok()?) as usize;
        let header = sample[at + 4];
        let kind = header & 0x1f;
        if kind == 1 || kind == 5 {
            let end = (at + 4).saturating_add(length).min(sample.len());
            let rbsp = unescape(sample.get(at + 5..end)?);
            let mut r = Bits::new(&rbsp);
            r.ue()?; // first_mb_in_slice
            r.ue()?; // slice_type
            r.ue()?; // pic_parameter_set_id
            if params.separate_colour_plane {
                r.bits(2)?;
            }
            let frame_num = r.bits(params.log2_max_frame_num)?;
            return Some(SliceRefs {
                nal_ref_idc: (header >> 5) & 3,
                idr: kind == 5,
                frame_num,
            });
        }
        at = at.checked_add(4)?.checked_add(length)?;
    }
    None
}

fn skip_scaling_list(r: &mut Bits<'_>, size: usize) -> Option<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Removes emulation-prevention bytes (`00 00 03` → `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(bit == 1)
    }

    fn bits(&mut self, n: u8) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | u32::from(self.bit()?);
        }
        Some(value)
    }

    /// Exp-Golomb, up to 31 leading zeros.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u8;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(rest)).try_into().ok()
    }

    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Main-profile SPS with log2_max_frame_num_minus4 = 0 and a High-profile
    /// one with = 2, written bit by bit.
    fn sps(profile: u8, log2_minus4: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.bits(u32::from(profile), 8);
        w.bits(0, 16);
        w.ue(0);
        if profile == 100 {
            w.ue(1); // 4:2:0
            w.ue(0);
            w.ue(0);
            w.bits(0, 1);
            w.bits(0, 1);
        }
        w.ue(log2_minus4);
        w.ue(0); // pic_order_cnt_type
        w.bits(1, 1); // stop bit (enough for this parser)
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        nal
    }

    fn avcc(sps: &[u8]) -> Vec<u8> {
        let mut out = vec![1, sps[1], 0, 0x1f, 0xff, 0xe1];
        out.extend((sps.len() as u16).to_be_bytes());
        out.extend(sps);
        out.extend([1, 0, 2, 0x68, 0xce]);
        out
    }

    fn slice(nal_ref_idc: u8, idr: bool, frame_num: u32, log2: u8) -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(0);
        w.ue(if idr { 7 } else { 5 });
        w.ue(0);
        w.bits(frame_num, log2);
        w.bits(1, 1);
        let mut nal = vec![(nal_ref_idc << 5) | if idr { 5 } else { 1 }];
        nal.extend(w.finish());
        let mut out = (nal.len() as u32).to_be_bytes().to_vec();
        out.extend(nal);
        out
    }

    #[derive(Default)]
    struct Writer {
        bits: Vec<bool>,
    }

    impl Writer {
        fn bits(&mut self, value: u32, n: u8) {
            for i in (0..n).rev() {
                self.bits.push((value >> i) & 1 == 1);
            }
        }
        fn ue(&mut self, value: u32) {
            let v = value + 1;
            let len = 32 - v.leading_zeros();
            self.bits(0, (len - 1) as u8);
            self.bits(v, len as u8);
        }
        fn finish(self) -> Vec<u8> {
            self.bits
                .chunks(8)
                .map(|c| {
                    c.iter()
                        .enumerate()
                        .fold(0u8, |b, (i, &x)| b | (u8::from(x) << (7 - i)))
                })
                .collect()
        }
    }

    #[test]
    fn reads_log2_max_frame_num_from_main_and_high_profile_records() {
        let main = AvcParams::from_avcc(&avcc(&sps(77, 0))).unwrap();
        assert_eq!(main.log2_max_frame_num, 4);
        let high = AvcParams::from_avcc(&avcc(&sps(100, 2))).unwrap();
        assert_eq!((high.log2_max_frame_num, high.max_frame_num()), (6, 64));
        assert_eq!(AvcParams::from_avcc(&[1, 2, 3]), None);
        assert_eq!(AvcParams::from_avcc(b"hvcC-like-record"), None);
    }

    #[test]
    fn reads_the_first_slice_of_an_access_unit() {
        let params = AvcParams::from_avcc(&avcc(&sps(100, 2))).unwrap();
        let mut unit = vec![0, 0, 0, 2, 0x06, 0x80]; // an SEI first
        unit.extend(slice(0, false, 37, 6));
        let refs = slice_refs(&unit, &params).unwrap();
        assert_eq!((refs.nal_ref_idc, refs.idr, refs.frame_num), (0, false, 37));
        let idr = slice_refs(&slice(3, true, 0, 6), &params).unwrap();
        assert!(idr.idr && idr.nal_ref_idc == 3);
        assert_eq!(slice_refs(&[0, 0, 0, 9, 0x06], &params), None);
    }

    /// A Main-profile SPS through its VUI's video_signal_type.
    fn sps_with_vui(full_range: bool, colour: Option<(u8, u8, u8)>) -> Vec<u8> {
        let mut w = Writer::default();
        w.bits(77, 8);
        w.bits(0, 16);
        w.ue(0);
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(1); // pic_order_cnt_type 1
        w.bits(0, 1);
        w.ue(0); // se 0
        w.ue(1); // se 1
        w.ue(2); // two offsets
        w.ue(2); // se -1
        w.ue(0);
        w.ue(1); // max_num_ref_frames
        w.bits(0, 1);
        w.ue(119); // 1920 / 16 - 1
        w.ue(67); // 1088 / 16 - 1
        w.bits(1, 1); // frame_mbs_only
        w.bits(1, 1); // direct_8x8
        w.bits(1, 1); // cropping
        for offset in [0, 0, 0, 4] {
            w.ue(offset);
        }
        w.bits(1, 1); // vui
        w.bits(1, 1); // aspect ratio
        w.bits(255, 8);
        w.bits(1, 16);
        w.bits(1, 16);
        w.bits(0, 1); // overscan
        w.bits(1, 1); // video_signal_type
        w.bits(5, 3);
        w.bits(u32::from(full_range), 1);
        match colour {
            Some((p, t, m)) => {
                w.bits(1, 1);
                w.bits(u32::from(p), 8);
                w.bits(u32::from(t), 8);
                w.bits(u32::from(m), 8);
            }
            None => w.bits(0, 1),
        }
        w.bits(0, 1); // chroma_loc_info_present_flag
        w.bits(1, 1);
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        nal
    }

    #[test]
    fn reads_the_video_signal_from_the_vui() {
        let signal = VideoSignal::from_avcc(&avcc(&sps_with_vui(true, Some((9, 16, 9))))).unwrap();
        assert_eq!(
            signal,
            VideoSignal {
                full_range: true,
                primaries: Some(9),
                transfer: Some(16),
                matrix: Some(9),
            }
        );
        let signal = VideoSignal::from_avcc(&avcc(&sps_with_vui(false, None))).unwrap();
        assert_eq!((signal.full_range, signal.primaries), (false, None));
        // The slice fields still read the same SPS.
        let params = AvcParams::from_avcc(&avcc(&sps_with_vui(false, None))).unwrap();
        assert_eq!(params.log2_max_frame_num, 4);
        // No VUI in the short test SPS (it ends early): unknown.
        assert_eq!(VideoSignal::from_avcc(&avcc(&sps(77, 0))), None);
        assert_eq!(VideoSignal::from_avcc(b"hvcC-like-record"), None);
    }

    #[test]
    fn unescapes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }
}
