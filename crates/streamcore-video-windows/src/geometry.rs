//! Source-rectangle maths for the converter, kept apart from the D3D11 calls so it can be tested
//! anywhere.

/// A rectangle in an input's pixels: `(left, top, width, height)`.
pub type Rect4 = (u32, u32, u32, u32);

/// A crop of `(x, y, width, height)` clamped into an input of `input` pixels, with an even origin
/// and an even size of at least 2 x 2. An odd input loses its last row or column; an input
/// narrower or shorter than 2 pixels is returned whole (there is no even crop of it).
pub fn crop(x: u32, y: u32, width: u32, height: u32, input: (u32, u32)) -> Rect4 {
    let (even_w, even_h) = (input.0 & !1, input.1 & !1);
    if even_w < 2 || even_h < 2 {
        return (0, 0, input.0, input.1);
    }
    let left = (x & !1).min(even_w - 2);
    let top = (y & !1).min(even_h - 2);
    (
        left,
        top,
        (width & !1).clamp(2, even_w - left),
        (height & !1).clamp(2, even_h - top),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_and_clamped() {
        assert_eq!(crop(10, 20, 100, 50, (1920, 1080)), (10, 20, 100, 50));
        // Odd values round down to even.
        assert_eq!(crop(11, 21, 101, 51, (1920, 1080)), (10, 20, 100, 50));
        // Past the right and bottom edges: clamped to what is left.
        assert_eq!(
            crop(1900, 1070, 100, 100, (1920, 1080)),
            (1900, 1070, 20, 10)
        );
        // Origin outside the input: the last 2 x 2.
        assert_eq!(crop(5000, 5000, 10, 10, (1920, 1080)), (1918, 1078, 2, 2));
        // Empty request: at least 2 x 2.
        assert_eq!(crop(0, 0, 0, 1, (1920, 1080)), (0, 0, 2, 2));
    }

    #[test]
    fn odd_input_keeps_an_even_origin() {
        // 1919 x 1079: the even area is 1918 x 1078.
        let (left, top, w, h) = crop(5000, 5000, 64, 64, (1919, 1079));
        assert_eq!((left, top, w, h), (1916, 1076, 2, 2));
        assert_eq!((left % 2, top % 2), (0, 0));
        assert_eq!(crop(0, 0, 4000, 4000, (1919, 1079)), (0, 0, 1918, 1078));
        assert_eq!(crop(0, 0, 4, 4, (3, 3)), (0, 0, 2, 2));
    }

    #[test]
    fn tiny_input_does_not_panic() {
        assert_eq!(crop(0, 0, 2, 2, (1, 1)), (0, 0, 1, 1));
        assert_eq!(crop(4, 4, 8, 8, (0, 0)), (0, 0, 0, 0));
        assert_eq!(crop(0, 0, 8, 8, (1, 100)), (0, 0, 1, 100));
        assert_eq!(crop(0, 0, 8, 8, (100, 1)), (0, 0, 100, 1));
    }
}
