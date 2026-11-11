//! Frame-counter barcode (design doc 9.6): a row of black and white cells that encodes a 16-bit
//! counter, drawn by the synthetic source and decoded from the luma plane of the converted frame.
//! Every source frame can then be classed as presented, repeated or skipped, and its draw time
//! matched to the frame's H0..H5 stamps.
//!
//! Layout, left to right: start `1 0`, 16 data bits (MSB first), two check bits (parity of the
//! even and of the odd data bits), end `1`. Cells are wide enough to survive scaling, NV12 chroma
//! subsampling and noise; decoding averages the middle half of each cell.

/// Data bits per code.
pub const DATA_BITS: usize = 16;
/// Total cells in a code.
pub const CELLS: usize = 2 + DATA_BITS + 2 + 1;

/// Cell values (true = white) for `counter`.
pub fn cells(counter: u16) -> [bool; CELLS] {
    let mut out = [false; CELLS];
    out[0] = true;
    out[1] = false;
    let mut even = false;
    let mut odd = false;
    for i in 0..DATA_BITS {
        let bit = (counter >> (DATA_BITS - 1 - i)) & 1 == 1;
        out[2 + i] = bit;
        if i % 2 == 0 {
            even ^= bit;
        } else {
            odd ^= bit;
        }
    }
    out[2 + DATA_BITS] = even;
    out[3 + DATA_BITS] = odd;
    out[CELLS - 1] = true;
    out
}

fn cell_mean(row: &[u8], x0: f64, cell_w: f64, i: usize) -> Option<f64> {
    let start = x0 + (i as f64 + 0.25) * cell_w;
    let end = x0 + (i as f64 + 0.75) * cell_w;
    let a = start.round().max(0.0) as usize;
    let b = (end.round() as usize).max(a + 1).min(row.len());
    if a >= b {
        return None;
    }
    let sum: u32 = row[a..b].iter().map(|v| u32::from(*v)).sum();
    Some(f64::from(sum) / (b - a) as f64)
}

/// Decodes a code drawn across `[x0, x0 + width)` of a luma row. `None` if the start, end or
/// check cells do not match (no code, torn frame, or too small to read).
pub fn decode(row: &[u8], x0: f64, width: f64) -> Option<u16> {
    let cell_w = width / CELLS as f64;
    if cell_w < 2.0 {
        return None;
    }
    let means: Vec<f64> = (0..CELLS)
        .map(|i| cell_mean(row, x0, cell_w, i))
        .collect::<Option<_>>()?;
    let white = means[0];
    let black = means[1];
    if white - black < 40.0 {
        return None;
    }
    let threshold = (white + black) / 2.0;
    let bits: Vec<bool> = means.iter().map(|m| *m > threshold).collect();
    let mut counter = 0u16;
    for i in 0..DATA_BITS {
        counter = (counter << 1) | u16::from(bits[2 + i]);
    }
    if bits[..] == cells(counter)[..] {
        Some(counter)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders a code into a luma row as studio-range NV12 would carry it (16 black, 235 white),
    /// scaled by `scale`, with a deterministic noise pattern and a soft edge from scaling.
    fn render(counter: u16, cell_px: f64, offset: f64, len: usize, noise: i32) -> Vec<u8> {
        let c = cells(counter);
        let mut row = vec![128u8; len];
        let mut seed = 12345u32;
        for (x, v) in row.iter_mut().enumerate() {
            let pos = (x as f64 - offset) / cell_px;
            let base = if pos < 0.0 || pos >= CELLS as f64 {
                128.0
            } else {
                let i = pos as usize;
                let frac = pos - i as f64;
                let here = if c[i] { 235.0 } else { 16.0 };
                // Blur the first 10% of a cell towards the previous cell (bilinear scaling).
                match i.checked_sub(1).map(|p| c[p]) {
                    Some(prev) if frac < 0.1 => {
                        let there = if prev { 235.0 } else { 16.0 };
                        there + (here - there) * (frac / 0.1)
                    }
                    _ => here,
                }
            };
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let n = ((seed >> 16) as i32 % (2 * noise + 1)) - noise;
            *v = (base as i32 + n).clamp(0, 255) as u8;
        }
        row
    }

    #[test]
    fn round_trip_at_native_and_scaled() {
        for counter in [0u16, 1, 2, 0x5555, 0xaaaa, 12345, u16::MAX] {
            for (cell_px, offset) in [(12.0, 0.0), (24.0, 3.0), (7.5, 10.0), (4.0, 1.0)] {
                let width = cell_px * CELLS as f64;
                let row = render(counter, cell_px, offset, (offset + width) as usize + 8, 12);
                assert_eq!(
                    decode(&row, offset, width),
                    Some(counter),
                    "{counter} at {cell_px}px"
                );
            }
        }
    }

    #[test]
    fn rejects_flat_and_corrupt() {
        assert_eq!(decode(&[128u8; 400], 0.0, 399.0), None);
        let mut row = render(777, 10.0, 0.0, 220, 0);
        // Flip one data cell: the check bits catch it.
        for v in &mut row[(5 * 10 + 2)..(5 * 10 + 8)] {
            *v = if *v > 128 { 16 } else { 235 };
        }
        assert_eq!(decode(&row, 0.0, 210.0), None);
    }

    #[test]
    fn too_small_to_read() {
        let row = render(9, 1.5, 0.0, 40, 0);
        assert_eq!(decode(&row, 0.0, 1.5 * CELLS as f64), None);
    }

    #[test]
    fn check_bits_distinguish_neighbours() {
        let a = cells(0b1000_0000_0000_0000);
        let b = cells(0b0100_0000_0000_0000);
        assert_ne!(a[2 + DATA_BITS], b[2 + DATA_BITS]);
        assert_ne!(a[3 + DATA_BITS], b[3 + DATA_BITS]);
    }
}
