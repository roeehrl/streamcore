//! Window capture's portable rules: the native window id a `CaptureTarget::Window` carries, when
//! the frame pool adopts a source's new size, and how the bench picks a window by title.
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

/// How long a window's new size must hold before the capture adopts it: the frame pool is
/// recreated and the layout reported once the size has not changed for this long.
///
/// A window dragged by its edge has a new size on almost every frame, and every reported layout
/// makes the consumer re-plan (a new encoder session at the new size), so reporting each step
/// would rebuild the encoder dozens of times a second. 200 ms is several frames at any stream
/// rate (12 at 60 fps, 6 at 30), longer than the gaps between steps of a drag in progress, and
/// short enough that a resize made in one step (maximize, snap, restore) is adopted before it
/// reads as a delay. It is also well under the 600 ms the design proposes for the consumer's
/// own re-plan, so the two do not add up to a second.
pub const WINDOW_RESIZE_SETTLE_NS: u64 = 200_000_000;

/// What to do with a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDecision {
    /// Hand the frame to the stream; otherwise close it unseen.
    pub deliver: bool,
    /// Give the frame pool buffers of this size now and report the new layout: after the frame
    /// when it is delivered, instead of it when it is not.
    pub resize: Option<(i32, i32)>,
    /// Start the settle timer to fire after this many nanoseconds.
    pub arm_ns: Option<u64>,
}

impl FrameDecision {
    const DELIVER: Self = Self {
        deliver: true,
        resize: None,
        arm_ns: None,
    };
    const SKIP: Self = Self {
        deliver: false,
        resize: None,
        arm_ns: None,
    };
}

/// What to do when the settle timer fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerDecision {
    /// Nothing is pending (the size went back, or a frame already settled it).
    Idle,
    /// The size changed again since the timer was started: fire again after this many ns.
    Wait(u64),
    /// The size has held: give the pool buffers of this size and report the new layout.
    Resize((i32, i32)),
}

/// Follows a capture source's size from the content size of its frames and decides when the
/// frame pool adopts a new one. Times are nanoseconds on any one monotonic clock, passed in.
///
/// - **A display** (`settle_ns` 0) changes size in one step, a mode change: the frame that
///   reveals it is delivered and the pool is resized at once.
/// - **A window** (`settle_ns` [`WINDOW_RESIZE_SETTLE_NS`]) adopts a size once it has held for
///   the settle time. Until then frames are delivered from the pool's old buffers, so the stream
///   keeps moving: a window that shrank shows whole, one that grew shows the part that fits the
///   old buffers. The settle timer makes the change happen even when no further frame arrives,
///   so a one-step resize is adopted the settle time after its first frame, never later.
/// - **While the window is minimized** every frame is dropped: nothing of the window is drawn
///   then, and whatever a frame holds is not its picture.
/// - **Before the first real size** (the item had none when the capture opened, so the pool
///   holds a placeholder) the first frame only sizes the pool, at once, and is dropped: its
///   buffer is the placeholder's.
#[derive(Debug, Clone)]
pub struct SizeTracker {
    pool: (i32, i32),
    placeholder: bool,
    settle_ns: u64,
    /// A size other than the pool's, and when a frame first showed it.
    pending: Option<((i32, i32), u64)>,
    armed: bool,
}

impl SizeTracker {
    /// A tracker for a capture item of `item` pixels (see [`pool_size`]).
    pub fn new(item: (i32, i32), settle_ns: u64) -> Self {
        Self {
            pool: pool_size(item),
            placeholder: item.0 <= 0 || item.1 <= 0,
            settle_ns,
            pending: None,
            armed: false,
        }
    }

    /// The size the frame pool has, or is to be created with.
    pub fn pool(&self) -> (i32, i32) {
        self.pool
    }

    /// A frame arrived at `now_ns` with this content size; `minimized` is whether the source is
    /// a minimized window (false for a display).
    pub fn on_frame(
        &mut self,
        content: Option<(i32, i32)>,
        minimized: bool,
        now_ns: u64,
    ) -> FrameDecision {
        if minimized {
            return FrameDecision::SKIP;
        }
        let content = content.filter(|(width, height)| *width > 0 && *height > 0);
        if self.placeholder {
            // Nothing can be shown from a placeholder buffer; a frame that says how large the
            // source is sizes the pool.
            return FrameDecision {
                resize: content,
                ..FrameDecision::SKIP
            };
        }
        let Some(size) = content.filter(|size| *size != self.pool) else {
            // The pool's size (or no size given): any change that was pending is over.
            self.pending = None;
            return FrameDecision::DELIVER;
        };
        if self.settle_ns == 0 {
            return FrameDecision {
                resize: Some(size),
                ..FrameDecision::DELIVER
            };
        }
        match self.pending {
            Some((pending, since)) if pending == size => {
                let held = now_ns.saturating_sub(since);
                if held >= self.settle_ns {
                    FrameDecision {
                        resize: Some(size),
                        ..FrameDecision::DELIVER
                    }
                } else {
                    self.arm(self.settle_ns - held)
                }
            }
            _ => {
                self.pending = Some((size, now_ns));
                self.arm(self.settle_ns)
            }
        }
    }

    /// Deliver, and start the timer unless it is already running (it then re-arms itself for
    /// what is left, see [`on_timer`](Self::on_timer)).
    fn arm(&mut self, after_ns: u64) -> FrameDecision {
        let arm_ns = (!self.armed).then_some(after_ns);
        self.armed = true;
        FrameDecision {
            arm_ns,
            ..FrameDecision::DELIVER
        }
    }

    /// The settle timer fired at `now_ns`.
    pub fn on_timer(&mut self, now_ns: u64) -> TimerDecision {
        self.armed = false;
        match self.pending {
            None => TimerDecision::Idle,
            Some((size, since)) => {
                let held = now_ns.saturating_sub(since);
                if held >= self.settle_ns {
                    TimerDecision::Resize(size)
                } else {
                    self.armed = true;
                    TimerDecision::Wait(self.settle_ns - held)
                }
            }
        }
    }

    /// The frame pool now has buffers of `size`. Until this is called a decided resize stays
    /// pending, so one that failed is decided again by the next frame of that size.
    pub fn resized(&mut self, size: (i32, i32)) {
        self.pool = size;
        self.placeholder = false;
        if self.pending.is_some_and(|(pending, _)| pending == size) {
            self.pending = None;
        }
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
/// item may report no size yet (a window that starts minimized) and a pool cannot be empty. Such
/// a pool is a placeholder: the first frame's content size sets the real one, and nothing is
/// delivered from it ([`SizeTracker`]).
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

    const SETTLE: u64 = WINDOW_RESIZE_SETTLE_NS;
    const MS: u64 = 1_000_000;

    fn deliver() -> FrameDecision {
        FrameDecision {
            deliver: true,
            resize: None,
            arm_ns: None,
        }
    }

    fn skip() -> FrameDecision {
        FrameDecision {
            deliver: false,
            resize: None,
            arm_ns: None,
        }
    }

    #[test]
    fn same_size_delivers() {
        for settle in [0, SETTLE] {
            let mut t = SizeTracker::new((800, 600), settle);
            assert_eq!(t.pool(), (800, 600));
            assert_eq!(t.on_frame(Some((800, 600)), false, 0), deliver());
            // A missing or empty size changes nothing.
            for content in [None, Some((0, 0)), Some((0, 600)), Some((800, -1))] {
                assert_eq!(t.on_frame(content, false, MS), deliver());
            }
            assert_eq!(t.on_timer(2 * MS), TimerDecision::Idle);
        }
    }

    #[test]
    fn a_display_resizes_at_once() {
        let mut t = SizeTracker::new((1920, 1080), 0);
        let d = t.on_frame(Some((2560, 1440)), false, 5 * MS);
        assert_eq!(
            d,
            FrameDecision {
                deliver: true,
                resize: Some((2560, 1440)),
                arm_ns: None
            }
        );
        // A resize that failed is decided again by the next frame.
        assert_eq!(t.on_frame(Some((2560, 1440)), false, 6 * MS), d);
        t.resized((2560, 1440));
        assert_eq!(t.pool(), (2560, 1440));
        assert_eq!(t.on_frame(Some((2560, 1440)), false, 7 * MS), deliver());
    }

    #[test]
    fn a_one_step_window_resize_waits_the_settle_time_and_no_longer() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        // The first frame at the new size is delivered and starts the timer.
        assert_eq!(
            t.on_frame(Some((1920, 1040)), false, 10 * MS),
            FrameDecision {
                deliver: true,
                resize: None,
                arm_ns: Some(SETTLE)
            }
        );
        // The window repaints at its new size: delivered, the timer already runs.
        assert_eq!(t.on_frame(Some((1920, 1040)), false, 30 * MS), deliver());
        assert_eq!(t.pool(), (800, 600));
        // No further frame is needed: the timer adopts the size, the settle time after the
        // first frame showed it.
        assert_eq!(
            t.on_timer(10 * MS + SETTLE),
            TimerDecision::Resize((1920, 1040))
        );
        t.resized((1920, 1040));
        assert_eq!(t.on_frame(Some((1920, 1040)), false, 300 * MS), deliver());
        assert_eq!(t.on_timer(900 * MS), TimerDecision::Idle);
    }

    #[test]
    fn a_frame_after_the_settle_time_resizes_without_the_timer() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        t.on_frame(Some((900, 700)), false, 0);
        assert_eq!(
            t.on_frame(Some((900, 700)), false, SETTLE),
            FrameDecision {
                deliver: true,
                resize: Some((900, 700)),
                arm_ns: None
            }
        );
        t.resized((900, 700));
        // The timer, firing late, finds nothing to do.
        assert_eq!(t.on_timer(SETTLE + MS), TimerDecision::Idle);
    }

    #[test]
    fn a_drag_is_one_resize_at_its_end() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        let mut resizes = 0;
        let mut armed = Vec::new();
        // A new size every 16 ms for a second.
        for step in 0..60u64 {
            let d = t.on_frame(Some((801 + step as i32, 600)), false, step * 16 * MS);
            assert!(d.deliver);
            resizes += usize::from(d.resize.is_some());
            armed.extend(d.arm_ns);
        }
        assert_eq!(resizes, 0);
        // One timer for the whole drag, started by its first frame.
        assert_eq!(armed, [SETTLE]);
        // It fires mid-drag and waits for what is left of the newest size's settle time.
        let last = 59 * 16 * MS;
        assert_eq!(t.on_timer(last + 50 * MS), TimerDecision::Wait(150 * MS));
        assert_eq!(t.on_timer(last + SETTLE), TimerDecision::Resize((860, 600)));
        t.resized((860, 600));
        assert_eq!(t.pool(), (860, 600));
        assert_eq!(t.on_timer(last + 2 * SETTLE), TimerDecision::Idle);
    }

    #[test]
    fn a_pause_shorter_than_the_settle_time_does_not_resize() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        t.on_frame(Some((900, 600)), false, 0);
        // Repaints of the same size during the pause do not restart the clock.
        assert_eq!(t.on_frame(Some((900, 600)), false, 100 * MS), deliver());
        // The drag goes on before the size settled: the clock restarts for the new size.
        assert_eq!(t.on_frame(Some((950, 600)), false, 150 * MS), deliver());
        assert_eq!(t.on_timer(SETTLE), TimerDecision::Wait(150 * MS));
        assert_eq!(
            t.on_timer(150 * MS + SETTLE),
            TimerDecision::Resize((950, 600))
        );
    }

    #[test]
    fn a_size_that_goes_back_is_no_resize() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        t.on_frame(Some((900, 600)), false, 0);
        assert_eq!(t.on_frame(Some((800, 600)), false, 50 * MS), deliver());
        assert_eq!(t.on_timer(SETTLE), TimerDecision::Idle);
        // The timer is free again for the next change.
        assert_eq!(
            t.on_frame(Some((900, 600)), false, 300 * MS).arm_ns,
            Some(SETTLE)
        );
    }

    #[test]
    fn a_failed_resize_is_decided_again_by_the_next_frame() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        t.on_frame(Some((900, 600)), false, 0);
        assert_eq!(t.on_timer(SETTLE), TimerDecision::Resize((900, 600)));
        // `resized` was not called: the pool could not be recreated.
        assert_eq!(t.pool(), (800, 600));
        assert_eq!(
            t.on_frame(Some((900, 600)), false, SETTLE + 16 * MS).resize,
            Some((900, 600))
        );
    }

    #[test]
    fn frames_while_minimized_are_dropped() {
        let mut t = SizeTracker::new((800, 600), SETTLE);
        // Whatever their size: the same, another, or none.
        for content in [Some((800, 600)), Some((160, 28)), None] {
            assert_eq!(t.on_frame(content, true, 0), skip());
        }
        // Nothing was learned from them: no timer, no resize.
        assert_eq!(t.on_timer(SETTLE), TimerDecision::Idle);
        assert_eq!(t.pool(), (800, 600));
        // Restored: frames flow again.
        assert_eq!(t.on_frame(Some((800, 600)), false, SETTLE), deliver());
    }

    #[test]
    fn a_placeholder_pool_is_sized_before_anything_is_delivered() {
        // The item had no size when the capture opened (the window was minimized).
        let mut t = SizeTracker::new((0, 0), SETTLE);
        assert_eq!(t.pool(), (1, 1));
        // Still minimized, or a frame that gives no size: dropped, nothing changes.
        assert_eq!(t.on_frame(Some((1280, 720)), true, 0), skip());
        assert_eq!(t.on_frame(None, false, MS), skip());
        // The first frame with a size sizes the pool at once, without the settle time, and is
        // itself dropped: its buffer is the placeholder's.
        let first = FrameDecision {
            deliver: false,
            resize: Some((1280, 720)),
            arm_ns: None,
        };
        assert_eq!(t.on_frame(Some((1280, 720)), false, 2 * MS), first);
        // Until the pool has its buffers the next frame asks again.
        assert_eq!(t.on_frame(Some((1280, 720)), false, 3 * MS), first);
        t.resized((1280, 720));
        assert_eq!(t.on_frame(Some((1280, 720)), false, 4 * MS), deliver());
        // From here on a new size settles like any other.
        assert_eq!(
            t.on_frame(Some((1300, 720)), false, 5 * MS).arm_ns,
            Some(SETTLE)
        );
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
