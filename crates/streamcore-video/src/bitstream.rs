//! Shared bitstream utilities for adapters (3.4). Pure functions: an encoder's
//! output thread may call them (reentrancy rule 4).

/// Converts an Annex-B access unit (start codes `00 00 01` or `00 00 00 01`)
/// into 4-byte big-endian length-prefixed NAL units (`BitstreamForm::Avcc4`).
/// Returns `None` when the input has no start code or an empty NAL unit.
#[uniffi::export]
pub fn annexb_to_length_prefixed(annexb: Vec<u8>) -> Option<Vec<u8>> {
    let bytes = annexb.as_slice();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let first = starts.first()?;
    if bytes[..first.0].iter().any(|b| *b != 0) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() + 4 * starts.len());
    for (n, &(_, body)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(bytes.len(), |next| next.0);
        // A 4-byte start code's leading zero, and trailing zero bytes, belong to
        // no NAL unit.
        while end > body && bytes[end - 1] == 0 {
            end -= 1;
        }
        let nal = &bytes[body..end];
        let length = u32::try_from(nal.len()).ok().filter(|l| *l > 0)?;
        out.extend(length.to_be_bytes());
        out.extend(nal);
    }
    Some(out)
}
