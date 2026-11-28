//! Window capture's portable rules: the native window id a `CaptureTarget::Window` carries, what
//! a frame's content size means for the frame pool, and how the bench picks a window by title.
//! Kept apart from the Win32 and WinRT calls so they are tested on every platform.

/// The window handle behind a `CaptureTarget::Window { window_id }`.
///
/// A native window id is the window's handle value, sign-extended to 64 bits. It names a window
/// on this computer, in this run of the desktop session, until that window closes; it means
/// nothing anywhere else, and Windows may give the value to a new window later. `None` for 0 (no
/// window) and for a value that is no handle on this platform (it does not fit a pointer).
pub fn handle_from_native_id(id: u64) -> Option<isize> {
    let handle = isize::try_from(id as i64).ok()?;
    (handle != 0).then_some(handle)
}

/// The native window id of a window handle: the inverse of [`handle_from_native_id`].
pub fn native_id_from_handle(handle: isize) -> u64 {
    handle as i64 as u64
}

/// What a captured frame's content size asks of the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    /// The source has the frame pool's size: deliver the frame.
    Deliver,
    /// The source changed size: deliver this frame (it still comes in a buffer of the old
    /// size), then give the pool buffers of the new size and report the new layout.
    DeliverAndResize { width: i32, height: i32 },
    /// The size changed while the window is minimized: what arrives then is not the window's
    /// picture, so the frame is dropped and the pool keeps its size.
    Skip,
}

/// Decides [`FrameAction`] from a frame's content size, the size the frame pool was last given
/// and whether the source is a minimized window (always false for a display). A missing or
/// empty content size changes nothing: the frame is delivered at its buffer's size.
pub fn frame_action(content: Option<(i32, i32)>, pool: (i32, i32), minimized: bool) -> FrameAction {
    match content {
        Some((width, height)) if width > 0 && height > 0 && (width, height) != pool => {
            if minimized {
                FrameAction::Skip
            } else {
                FrameAction::DeliverAndResize { width, height }
            }
        }
        _ => FrameAction::Deliver,
    }
}

/// The size of the image inside a frame's buffer: the content size, clipped to the buffer (the
/// frame that reveals a larger source still comes in a buffer of the old size); the whole buffer
/// when the content size is missing or empty.
pub fn image_size(content: Option<(i32, i32)>, buffer: (u32, u32)) -> (u32, u32) {
    match content {
        Some((width, height)) if width > 0 && height > 0 => {
            ((width as u32).min(buffer.0), (height as u32).min(buffer.1))
        }
        _ => buffer,
    }
}

/// The frame pool's first size for a capture item of `item` pixels: at least 1 x 1, because an
/// item may report no size yet (a window that starts minimized) and a pool cannot be empty. The
/// first frame's content size then sets the real one.
pub fn pool_size(item: (i32, i32)) -> (i32, i32) {
    (item.0.max(1), item.1.max(1))
}

/// A rectangle's size from its edges `[left, top, right, bottom]`; 0 for an inverted edge pair.
pub fn rect_size(edges: [i32; 4]) -> (u32, u32) {
    let span = |from: i32, to: i32| u32::try_from(i64::from(to) - i64::from(from)).unwrap_or(0);
    (span(edges[0], edges[2]), span(edges[1], edges[3]))
}

/// The index of the first title that contains `needle`, ignoring case; `None` when none does or
/// `needle` is empty. The bench lists windows front to back, so the first match is the one
/// nearest the front.
pub fn find_title<'a>(titles: impl IntoIterator<Item = &'a str>, needle: &str) -> Option<usize> {
    let needle = needle.to_lowercase();
    if needle.is_empty() {
        return None;
    }
    titles
        .into_iter()
        .position(|title| title.to_lowercase().contains(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_ids_round_trip() {
        for handle in [1isize, 0x1f0a2, 0x7fff_fff1, -2, isize::MAX, isize::MIN] {
            assert_eq!(
                handle_from_native_id(native_id_from_handle(handle)),
                Some(handle),
                "{handle:#x}"
            );
        }
        assert_eq!(native_id_from_handle(0x1f0a2), 0x1f0a2);
        // A handle with its top bit set is sign-extended, as Windows does between 32 and 64 bits.
        assert_eq!(native_id_from_handle(-2), u64::MAX - 1);
    }

    #[test]
    fn no_window_and_foreign_ids_are_refused() {
        assert_eq!(handle_from_native_id(0), None);
        // Where a handle is 32 bits wide, an id that needs more names nothing.
        let wide = 0x0000_0001_0000_0000u64;
        if isize::BITS == 32 {
            assert_eq!(handle_from_native_id(wide), None);
        } else {
            assert_eq!(handle_from_native_id(wide), Some(0x1_0000_0000));
        }
    }

    #[test]
    fn same_size_delivers() {
        assert_eq!(
            frame_action(Some((800, 600)), (800, 600), false),
            FrameAction::Deliver
        );
        // Minimized with nothing changed: still the window's last picture.
        assert_eq!(
            frame_action(Some((800, 600)), (800, 600), true),
            FrameAction::Deliver
        );
    }

    #[test]
    fn a_new_size_resizes_the_pool() {
        for content in [(801, 600), (800, 599), (1920, 1080), (1, 1)] {
            assert_eq!(
                frame_action(Some(content), (800, 600), false),
                FrameAction::DeliverAndResize {
                    width: content.0,
                    height: content.1
                }
            );
        }
        // The pool's placeholder size (an item with no size yet) is replaced by the first frame.
        assert_eq!(
            frame_action(Some((640, 480)), pool_size((0, 0)), false),
            FrameAction::DeliverAndResize {
                width: 640,
                height: 480
            }
        );
    }

    #[test]
    fn a_new_size_while_minimized_is_skipped() {
        assert_eq!(
            frame_action(Some((160, 28)), (800, 600), true),
            FrameAction::Skip
        );
    }

    #[test]
    fn a_missing_or_empty_size_changes_nothing() {
        for content in [None, Some((0, 0)), Some((0, 600)), Some((800, -1))] {
            for minimized in [false, true] {
                assert_eq!(
                    frame_action(content, (800, 600), minimized),
                    FrameAction::Deliver
                );
            }
        }
    }

    #[test]
    fn image_is_clipped_to_its_buffer() {
        assert_eq!(image_size(Some((800, 600)), (800, 600)), (800, 600));
        // Shrunk: the image is the top-left part of the old buffer.
        assert_eq!(image_size(Some((640, 480)), (800, 600)), (640, 480));
        // Grown: only what fits the old buffer is there.
        assert_eq!(image_size(Some((1024, 500)), (800, 600)), (800, 500));
        assert_eq!(image_size(None, (800, 600)), (800, 600));
        assert_eq!(image_size(Some((0, 10)), (800, 600)), (800, 600));
        assert_eq!(image_size(Some((-5, -5)), (800, 600)), (800, 600));
    }

    #[test]
    fn pool_is_never_empty() {
        assert_eq!(pool_size((1283, 777)), (1283, 777));
        assert_eq!(pool_size((0, 0)), (1, 1));
        assert_eq!(pool_size((-3, 40)), (1, 40));
    }

    #[test]
    fn rect_sizes() {
        assert_eq!(rect_size([10, 20, 110, 70]), (100, 50));
        assert_eq!(rect_size([-1920, 0, 0, 1080]), (1920, 1080));
        assert_eq!(rect_size([5, 5, 5, 5]), (0, 0));
        assert_eq!(rect_size([10, 10, 0, 0]), (0, 0));
        assert_eq!(rect_size([i32::MIN, 0, i32::MAX, 1]), (u32::MAX, 1));
    }

    #[test]
    fn titles_match_by_substring_without_case() {
        let titles = ["Untitled - Notepad", "Calculator", "notes.txt - Notepad"];
        assert_eq!(find_title(titles, "calc"), Some(1));
        assert_eq!(find_title(titles, "NOTEPAD"), Some(0));
        assert_eq!(find_title(titles, "notes"), Some(2));
        assert_eq!(find_title(titles, "paint"), None);
        assert_eq!(find_title(titles, ""), None);
        assert_eq!(find_title([], "x"), None);
    }
}
