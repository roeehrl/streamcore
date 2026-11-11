//! Sequence parameter set parser (ITU-T H.264 7.3.2.1.1 and E.1.1), enough to inspect what a
//! hardware encoder writes (profile, level, size, the VUI colour description and the
//! `bitstream_restriction` fields that tell a decoder it need not buffer for reordering), and
//! the VUI normaliser (design doc 3.4) that rewrites the colour description from the stream's
//! colour tags and writes a zero-reorder `bitstream_restriction`, else some decoders buffer until
//! the DPB fills.

use super::annexb::{escape, unescape};
use serde::{Deserialize, Serialize};

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// Colour description from the VUI `video_signal_type`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColourDescription {
    pub video_full_range: bool,
    pub primaries: Option<u8>,
    pub transfer: Option<u8>,
    pub matrix: Option<u8>,
}

/// `bitstream_restriction` fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitstreamRestriction {
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vui {
    pub signal: Option<ColourDescription>,
    pub timing: Option<(u32, u32, bool)>,
    pub nal_hrd: bool,
    pub vcl_hrd: bool,
    pub restriction: Option<BitstreamRestriction>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub width: u32,
    pub height: u32,
    pub max_num_ref_frames: u32,
    pub pic_order_cnt_type: u32,
    pub vui: Option<Vui>,
}

impl Sps {
    /// Whether a decoder may output each frame as soon as it is decoded: either the stream says
    /// so (`max_num_reorder_frames == 0`), or the profile cannot reorder (Baseline and
    /// Constrained Baseline have no B slices, although decoders still may buffer without the
    /// restriction; the VUI normaliser writes it).
    pub fn declares_zero_reorder(&self) -> bool {
        self.vui
            .as_ref()
            .and_then(|v| v.restriction.as_ref())
            .is_some_and(|r| r.max_num_reorder_frames == 0)
    }
}

/// Big-endian bit writer for RBSP.
#[derive(Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
    bit: u32,
}

impl Writer {
    pub(crate) fn bits(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.bit.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let b = ((v >> i) & 1) as u8;
            if let Some(last) = self.bytes.last_mut() {
                *last |= b << (7 - (self.bit % 8));
            }
            self.bit += 1;
        }
    }
    pub(crate) fn flag(&mut self, f: bool) {
        self.bits(1, u32::from(f));
    }
    pub(crate) fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.bits(len - 1, 0);
        for i in (0..len).rev() {
            self.bits(1, ((x >> i) & 1) as u32);
        }
    }
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.bits(1, 1); // rbsp_stop_one_bit
        while !self.bit.is_multiple_of(8) {
            self.bits(1, 0);
        }
        self.bytes
    }
}

impl Writer {
    /// Copies bits `[from, to)` of `src`.
    fn copy(&mut self, src: &[u8], from: usize, to: usize) {
        for pos in from..to {
            let byte = src.get(pos / 8).copied().unwrap_or(0);
            self.bits(1, u32::from((byte >> (7 - (pos % 8))) & 1));
        }
    }
}

const HIGH_PROFILES: &[u8] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            // delta_scale is -128..=127 (7.4.2.1.1.1); anything else is a corrupt SPS.
            let delta = b.se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

fn skip_hrd(b: &mut Bits<'_>) -> Option<()> {
    let cpb_cnt = b.ue()? + 1;
    b.bits(4)?; // bit_rate_scale
    b.bits(4)?; // cpb_size_scale
    for _ in 0..cpb_cnt {
        b.ue()?;
        b.ue()?;
        b.flag()?;
    }
    b.bits(5)?; // initial_cpb_removal_delay_length_minus1
    b.bits(5)?; // cpb_removal_delay_length_minus1
    b.bits(5)?; // dpb_output_delay_length_minus1
    b.bits(5)?; // time_offset_length
    Some(())
}

/// Bit positions of the VUI parts the normaliser replaces, in the RBSP.
#[derive(Clone, Copy, Debug, Default)]
struct VuiLayout {
    /// First bit after `vui_parameters_present_flag`.
    start: usize,
    signal: (usize, usize),
    restriction: (usize, usize),
    /// The restriction's motion-vector fields, kept by the normaliser.
    mv: Option<(bool, u32, u32, u32, u32)>,
    /// The encoder's `video_format`, kept by the normaliser.
    video_format: Option<u32>,
}

fn parse_vui(b: &mut Bits<'_>, layout: &mut VuiLayout) -> Option<Vui> {
    let mut vui = Vui::default();
    layout.start = b.pos;
    if b.flag()? {
        let idc = b.bits(8)?;
        if idc == 255 {
            b.bits(16)?;
            b.bits(16)?;
        }
    }
    if b.flag()? {
        b.flag()?; // overscan_appropriate
    }
    let signal_start = b.pos;
    if b.flag()? {
        layout.video_format = Some(b.bits(3)?);
        let full = b.flag()?;
        let mut cd = ColourDescription {
            video_full_range: full,
            ..ColourDescription::default()
        };
        if b.flag()? {
            cd.primaries = Some(b.bits(8)? as u8);
            cd.transfer = Some(b.bits(8)? as u8);
            cd.matrix = Some(b.bits(8)? as u8);
        }
        vui.signal = Some(cd);
    }
    layout.signal = (signal_start, b.pos);
    if b.flag()? {
        b.ue()?;
        b.ue()?;
    }
    if b.flag()? {
        let units = b.bits(32)?;
        let scale = b.bits(32)?;
        let fixed = b.flag()?;
        vui.timing = Some((units, scale, fixed));
    }
    vui.nal_hrd = b.flag()?;
    if vui.nal_hrd {
        skip_hrd(b)?;
    }
    vui.vcl_hrd = b.flag()?;
    if vui.vcl_hrd {
        skip_hrd(b)?;
    }
    if vui.nal_hrd || vui.vcl_hrd {
        b.flag()?; // low_delay_hrd
    }
    b.flag()?; // pic_struct_present
    let restriction_start = b.pos;
    if b.flag()? {
        let mv = (b.flag()?, b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let max_num_reorder_frames = b.ue()?;
        let max_dec_frame_buffering = b.ue()?;
        layout.mv = Some(mv);
        vui.restriction = Some(BitstreamRestriction {
            max_num_reorder_frames,
            max_dec_frame_buffering,
        });
    }
    layout.restriction = (restriction_start, b.pos);
    Some(vui)
}

/// Parses an SPS NAL unit (header byte included, emulation prevention still present).
pub fn parse(nal: &[u8]) -> Option<Sps> {
    parse_with_layout(nal).map(|(sps, _)| sps)
}

/// An SPS's RBSP, the bit position of `vui_parameters_present_flag`, and the VUI layout.
type Layout = (Vec<u8>, usize, Option<VuiLayout>);

fn parse_with_layout(nal: &[u8]) -> Option<(Sps, Layout)> {
    if nal.first().map(|b| b & 0x1f) != Some(7) {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut b = Bits::new(&rbsp);
    let profile_idc = b.bits(8)? as u8;
    let constraint_flags = b.bits(8)? as u8;
    let level_idc = b.bits(8)? as u8;
    b.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = b.flag()?;
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.flag()?; // qpprime_y_zero_transform_bypass
        if b.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = b.ue()?;
    if pic_order_cnt_type == 0 {
        b.ue()?;
    } else if pic_order_cnt_type == 1 {
        b.flag()?;
        b.se()?;
        b.se()?;
        let n = b.ue()?;
        for _ in 0..n {
            b.se()?;
        }
    }
    let max_num_ref_frames = b.ue()?;
    b.flag()?; // gaps_in_frame_num_value_allowed
    let width_mbs = b.ue()?.checked_add(1)?;
    let height_map_units = b.ue()?.checked_add(1)?;
    let frame_mbs_only = b.flag()?;
    if !frame_mbs_only {
        b.flag()?;
    }
    b.flag()?; // direct_8x8_inference
    let mut crop = (0, 0, 0, 0);
    if b.flag()? {
        crop = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
    }
    let vui_flag = b.pos;
    let mut layout = VuiLayout::default();
    let vui = if b.flag()? {
        Some(parse_vui(&mut b, &mut layout)?)
    } else {
        None
    };
    let layout = vui.as_ref().map(|_| layout);

    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (sub_w, sub_h) = match chroma_array_type {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let frame_height_factor: u32 = if frame_mbs_only { 1 } else { 2 };
    let crop_unit_x: u32 = if chroma_array_type == 0 { 1 } else { sub_w };
    let crop_unit_y: u32 = if chroma_array_type == 0 {
        frame_height_factor
    } else {
        sub_h * frame_height_factor
    };
    // Checked: a crafted SPS must not overflow (it is parsed on the encoder's event thread).
    let crop_x = crop_unit_x.checked_mul(crop.0.checked_add(crop.1)?)?;
    let crop_y = crop_unit_y.checked_mul(crop.2.checked_add(crop.3)?)?;
    let width = width_mbs.checked_mul(16)?.saturating_sub(crop_x);
    let height = height_map_units
        .checked_mul(16)?
        .checked_mul(frame_height_factor)?
        .saturating_sub(crop_y);
    let sps = Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        width,
        height,
        max_num_ref_frames,
        pic_order_cnt_type,
        vui,
    };
    Some((sps, (rbsp, vui_flag, layout)))
}

/// A colour description in H.264 code points (Table E-3, E-4, E-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colour {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl Sps {
    /// Whether the VUI states `colour` exactly.
    pub fn states_colour(&self, colour: Colour) -> bool {
        self.vui
            .as_ref()
            .and_then(|v| v.signal.as_ref())
            .is_some_and(|s| {
                s.video_full_range == colour.full_range
                    && s.primaries == Some(colour.primaries)
                    && s.transfer == Some(colour.transfer)
                    && s.matrix == Some(colour.matrix)
            })
    }
}

/// Rewrites an SPS so its VUI states `colour` and declares zero reordering
/// (`max_num_reorder_frames = 0`, `max_dec_frame_buffering = max_num_ref_frames`). Everything
/// else is kept bit for bit: the fields before the VUI, aspect ratio, overscan, `video_format`
/// (5, unspecified, when the encoder wrote no `video_signal_type`), chroma location, timing, HRD
/// and `pic_struct_present_flag`. Returns the NAL unit unchanged when it already
/// says both, and `None` when it cannot be parsed.
pub fn normalize(nal: &[u8], colour: Colour) -> Option<Vec<u8>> {
    let (sps, (rbsp, vui_flag, layout)) = parse_with_layout(nal)?;
    if sps.states_colour(colour) && sps.declares_zero_reorder() {
        return Some(nal.to_vec());
    }
    let mut w = Writer::default();
    w.copy(&rbsp, 0, vui_flag);
    w.flag(true); // vui_parameters_present_flag
    match layout {
        Some(l) => w.copy(&rbsp, l.start, l.signal.0),
        None => {
            w.flag(false); // aspect_ratio_info_present_flag
            w.flag(false); // overscan_info_present_flag
        }
    }
    w.flag(true); // video_signal_type_present_flag
    // video_format: the encoder's, else 5 (unspecified).
    w.bits(3, layout.and_then(|l| l.video_format).unwrap_or(5));
    w.flag(colour.full_range);
    w.flag(true); // colour_description_present_flag
    w.bits(8, u32::from(colour.primaries));
    w.bits(8, u32::from(colour.transfer));
    w.bits(8, u32::from(colour.matrix));
    match layout {
        Some(l) => w.copy(&rbsp, l.signal.1, l.restriction.0),
        None => {
            w.flag(false); // chroma_loc_info_present_flag
            w.flag(false); // timing_info_present_flag
            w.flag(false); // nal_hrd_parameters_present_flag
            w.flag(false); // vcl_hrd_parameters_present_flag
            w.flag(false); // pic_struct_present_flag
        }
    }
    w.flag(true); // bitstream_restriction_flag
    let (over, bytes, bits, mv_h, mv_v) = layout.and_then(|l| l.mv).unwrap_or((true, 2, 1, 16, 16));
    w.flag(over);
    w.ue(bytes);
    w.ue(bits);
    w.ue(mv_h);
    w.ue(mv_v);
    w.ue(0); // max_num_reorder_frames
    w.ue(sps.max_num_ref_frames.max(1)); // max_dec_frame_buffering
    let mut out = vec![nal[0]];
    out.extend(escape(&w.finish()));
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 1920x1080 SPS (Main or High), optionally with a VUI carrying BT.709 and a restriction.
    pub(crate) fn build(profile: u8, vui: bool, reorder: Option<u32>) -> Vec<u8> {
        let mut w = Writer::default();
        w.bits(8, u32::from(profile));
        w.bits(8, 0);
        w.bits(8, 40);
        w.ue(0);
        if HIGH_PROFILES.contains(&profile) {
            w.ue(1); // 4:2:0
            w.ue(0);
            w.ue(0);
            w.flag(false);
            w.flag(false); // no scaling matrix
        }
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // refs
        w.flag(false);
        w.ue(119); // 120 MBs = 1920
        w.ue(67); // 68 MBs = 1088
        w.flag(true); // frame_mbs_only
        w.flag(true); // direct_8x8
        w.flag(true); // cropping
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4); // 8 rows -> 1080
        w.flag(vui);
        if vui {
            w.flag(false); // aspect
            w.flag(false); // overscan
            w.flag(true); // video_signal_type
            w.bits(3, 5);
            w.flag(false); // limited range
            w.flag(true);
            w.bits(8, 1);
            w.bits(8, 1);
            w.bits(8, 1);
            w.flag(false); // chroma loc
            w.flag(true); // timing
            w.bits(32, 1);
            w.bits(32, 120);
            w.flag(false);
            w.flag(false); // nal hrd
            w.flag(false); // vcl hrd
            w.flag(false); // pic_struct
            w.flag(reorder.is_some());
            if let Some(r) = reorder {
                w.flag(true);
                w.ue(2);
                w.ue(1);
                w.ue(16);
                w.ue(16);
                w.ue(r);
                w.ue(1);
            }
        }
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        nal
    }

    #[test]
    fn main_profile_without_vui() {
        let sps = parse(&build(77, false, None)).unwrap_or_default();
        assert_eq!((sps.profile_idc, sps.level_idc), (77, 40));
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.pic_order_cnt_type, 2);
        assert!(sps.vui.is_none());
        assert!(!sps.declares_zero_reorder());
    }

    #[test]
    fn high_profile_with_colour_and_restriction() {
        let sps = parse(&build(100, true, Some(0))).unwrap_or_default();
        assert_eq!((sps.width, sps.height), (1920, 1080));
        let vui = sps.vui.clone().unwrap_or_default();
        let cd = vui.signal.unwrap_or_default();
        assert_eq!(
            (cd.primaries, cd.transfer, cd.matrix),
            (Some(1), Some(1), Some(1))
        );
        assert!(!cd.video_full_range);
        assert_eq!(vui.timing, Some((1, 120, false)));
        assert!(sps.declares_zero_reorder());
    }

    #[test]
    fn vui_without_restriction_and_with_reorder() {
        assert!(
            !parse(&build(100, true, None))
                .unwrap_or_default()
                .declares_zero_reorder()
        );
        assert!(
            !parse(&build(100, true, Some(2)))
                .unwrap_or_default()
                .declares_zero_reorder()
        );
    }

    #[test]
    fn rejects_non_sps_and_truncated() {
        assert!(parse(&[0x68, 0xff]).is_none());
        let full = build(100, true, Some(0));
        assert!(parse(&full[..6]).is_none());
    }

    #[test]
    fn exp_golomb() {
        let mut w = Writer::default();
        for v in [0, 1, 2, 3, 7, 255, 65535] {
            w.ue(v);
        }
        let data = w.finish();
        let mut b = Bits::new(&data);
        for v in [0, 1, 2, 3, 7, 255, 65535] {
            assert_eq!(b.ue(), Some(v));
        }
    }

    const BT709: Colour = Colour {
        primaries: 1,
        transfer: 1,
        matrix: 1,
        full_range: false,
    };

    fn no_start_code_emulation(nal: &[u8]) -> bool {
        nal.windows(3)
            .all(|w| !(w[0] == 0 && w[1] == 0 && w[2] <= 2))
    }

    #[test]
    fn normalize_adds_a_vui() {
        let nal = build(77, false, None);
        let out = normalize(&nal, BT709).unwrap_or_default();
        let sps = parse(&out).unwrap_or_default();
        assert_eq!((sps.width, sps.height, sps.profile_idc), (1920, 1080, 77));
        assert!(sps.declares_zero_reorder());
        assert!(sps.states_colour(BT709));
        let r = sps.vui.and_then(|v| v.restriction).unwrap_or_default();
        assert_eq!(r.max_dec_frame_buffering, 1);
        assert!(no_start_code_emulation(&out[1..]));
    }

    #[test]
    fn normalize_keeps_timing_and_rewrites_colour_and_reorder() {
        let nal = build(100, true, Some(2));
        let srgb = Colour {
            primaries: 1,
            transfer: 13,
            matrix: 1,
            full_range: true,
        };
        let out = normalize(&nal, srgb).unwrap_or_default();
        let sps = parse(&out).unwrap_or_default();
        let vui = sps.vui.clone().unwrap_or_default();
        assert_eq!(vui.timing, Some((1, 120, false)));
        assert!(sps.states_colour(srgb));
        assert!(!sps.states_colour(BT709));
        assert!(sps.declares_zero_reorder());
        // Already normal: returned unchanged.
        assert_eq!(normalize(&out, srgb), Some(out.clone()));
    }

    /// A Main SPS with the given size fields and cropping, no VUI.
    fn crafted(
        width_mbs_minus1: u32,
        height_minus1: u32,
        frame_mbs_only: bool,
        crop: [u32; 4],
    ) -> Vec<u8> {
        let mut w = Writer::default();
        w.bits(8, 77);
        w.bits(8, 0);
        w.bits(8, 40);
        w.ue(0);
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // refs
        w.flag(false);
        w.ue(width_mbs_minus1);
        w.ue(height_minus1);
        w.flag(frame_mbs_only);
        if !frame_mbs_only {
            w.flag(false); // mb_adaptive_frame_field
        }
        w.flag(true); // direct_8x8
        w.flag(true); // cropping
        for c in crop {
            w.ue(c);
        }
        w.flag(false); // no VUI
        let mut nal = vec![0x67];
        nal.extend(escape(&w.finish()));
        nal
    }

    #[test]
    fn crafted_sizes_do_not_overflow() {
        const MAX: u32 = u32::MAX - 1; // the largest ue(v) the parser reads
        // width_mbs * 16 and height * 16 * 2 overflow.
        assert!(parse(&crafted(MAX, 0, true, [0; 4])).is_none());
        assert!(parse(&crafted(0, MAX, true, [0; 4])).is_none());
        assert!(parse(&crafted(0, 1 << 28, false, [0; 4])).is_none());
        // The crop offsets' sum and the crop unit times the sum overflow.
        assert!(parse(&crafted(0, 0, true, [MAX, MAX, 0, 0])).is_none());
        assert!(parse(&crafted(0, 0, true, [1 << 31, 0, 0, 0])).is_none());
        assert!(parse(&crafted(0, 0, false, [0, 0, 1 << 30, 0])).is_none());
        // Normalising them fails the same way instead of panicking.
        assert!(normalize(&crafted(MAX, 0, true, [0; 4]), BT709).is_none());
        // In range, a crop larger than the picture clamps to zero.
        let sps = parse(&crafted(0, 0, true, [100, 0, 0, 0])).unwrap_or_default();
        assert_eq!((sps.width, sps.height), (0, 16));
    }

    #[test]
    fn crafted_scaling_list_delta_is_rejected() {
        let mut w = Writer::default();
        w.bits(8, 100); // High
        w.bits(8, 0);
        w.bits(8, 40);
        w.ue(0);
        w.ue(1); // 4:2:0
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(true); // seq_scaling_matrix_present
        w.flag(true); // list 0 present
        w.ue(u32::MAX - 1); // delta_scale = se(+2^31 - 1)
        let mut nal = vec![0x67];
        nal.extend(escape(&w.finish()));
        assert!(parse(&nal).is_none());
    }

    #[test]
    fn normalize_keeps_video_format() {
        let mut nal = build(100, true, Some(2));
        // build() writes video_format 5; make it 2 (MAC) in place: the first 3 bits after the
        // aspect and overscan flags. Re-parse to find them rather than hard-coding offsets.
        let (_, (mut rbsp, _, layout)) = parse_with_layout(&nal).unwrap_or_default();
        let at = layout.map(|l| l.signal.0 + 1).unwrap_or_default();
        for (i, bit) in [0u8, 1, 0].into_iter().enumerate() {
            let pos = at + i;
            let mask = 1 << (7 - (pos % 8));
            if bit == 1 {
                rbsp[pos / 8] |= mask;
            } else {
                rbsp[pos / 8] &= !mask;
            }
        }
        nal = vec![0x67];
        nal.extend(escape(&rbsp));
        let (_, (_, _, before)) = parse_with_layout(&nal).unwrap_or_default();
        assert_eq!(before.and_then(|l| l.video_format), Some(2));
        let out = normalize(&nal, BT709).unwrap_or_default();
        let (sps, (_, _, after)) = parse_with_layout(&out).unwrap_or_default();
        assert_eq!(after.and_then(|l| l.video_format), Some(2));
        assert!(sps.declares_zero_reorder());
        // No video_signal_type in the input: 5 (unspecified) is written.
        let out = normalize(&build(77, false, None), BT709).unwrap_or_default();
        let (_, (_, _, after)) = parse_with_layout(&out).unwrap_or_default();
        assert_eq!(after.and_then(|l| l.video_format), Some(5));
    }

    #[test]
    fn normalize_rejects_garbage() {
        assert!(normalize(&[0x68, 1, 2], BT709).is_none());
        let nal = build(100, true, Some(0));
        assert!(normalize(&nal[..5], BT709).is_none());
    }
}
