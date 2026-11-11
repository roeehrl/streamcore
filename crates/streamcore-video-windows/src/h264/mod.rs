//! H.264 helpers for non-Apple encoders (design doc 3.4): Annex-B splitting and conversion to
//! AVCC, the avcC record, an SPS/VUI inspector and the VUI normaliser that writes the colour
//! description and a zero-reorder `bitstream_restriction`.
//!
//! They live here until a second non-Apple backend needs them; then they move to a shared crate.

pub mod annexb;
pub mod sps;
