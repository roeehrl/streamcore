//! The synthetic source: a topmost window on its own thread that draws a frame-counter barcode
//! (and, in flip mode, a full-window colour change) at known times, so capture -> encoded latency
//! can be measured without a client. It shares nothing with the pipeline except the log of
//! (counter, draw time) it returns when stopped.

use super::com::now_ns;
use crate::barcode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, CreateSolidBrush, DeleteObject, FillRect, GdiFlush, GetDC, GetStockObject, HBRUSH,
    HDC, HGDIOBJ, ReleaseDC, ValidateRect, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WM_ERASEBKGND, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{Result, w};

/// Where the barcode strip is drawn, in window client coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Strip {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug)]
pub enum Pace {
    /// A new counter every composition (`DwmFlush`).
    EveryComposition,
    /// A new counter and background colour every `ms`.
    Every(u32),
}

/// (counter, draw time in ns) for every frame the source drew.
pub type DrawLog = Vec<(u32, u64)>;

pub struct Synthetic {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<std::result::Result<DrawLog, String>>>,
}

impl Synthetic {
    /// Opens the window over `rect` (left, top, width, height in desktop pixels) and starts
    /// drawing. Returns once the window shows its first frame.
    pub fn start(rect: (i32, i32, i32, i32), strip: Strip, pace: Pace) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("synthetic-source".into())
            .spawn(move || run(rect, strip, pace, &stop2).map_err(|e| e.to_string()))
            .ok();
        std::thread::sleep(Duration::from_millis(300));
        Self { stop, thread }
    }

    /// Stops drawing, closes the window and returns the (counter, draw ns) log.
    pub fn stop(mut self) -> std::result::Result<Vec<(u32, u64)>, String> {
        self.stop.store(true, Ordering::SeqCst);
        match self.thread.take() {
            Some(t) => t
                .join()
                .map_err(|_| "synthetic thread panicked".to_string())?,
            None => Err("synthetic thread did not start".into()),
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Drawing happens from the source loop; just validate.
        // SAFETY: `hwnd` is the window this procedure was called for.
        WM_PAINT => unsafe {
            let _ = ValidateRect(Some(hwnd), None);
            LRESULT(0)
        },
        WM_ERASEBKGND => LRESULT(1),
        // SAFETY: forwards the arguments this procedure received unchanged.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn fill(dc: HDC, r: RECT, brush: HBRUSH) {
    // SAFETY: valid DC from GetDC and a live brush; the RECT lives across the call.
    unsafe { FillRect(dc, &r, brush) };
}

fn draw_code(dc: HDC, strip: Strip, counter: u16, white: HBRUSH, black: HBRUSH) {
    let cells = barcode::cells(counter);
    let cell_w = strip.width / barcode::CELLS as i32;
    for (i, on) in cells.iter().enumerate() {
        let left = strip.x + i as i32 * cell_w;
        let r = RECT {
            left,
            top: strip.y,
            right: left + cell_w,
            bottom: strip.y + strip.height,
        };
        fill(dc, r, if *on { white } else { black });
    }
}

fn run(
    rect: (i32, i32, i32, i32),
    strip: Strip,
    pace: Pace,
    stop: &AtomicBool,
) -> Result<Vec<(u32, u64)>> {
    // SAFETY: per-thread DPI awareness so the window uses physical pixels.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: the module handle of this executable.
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("StreamCoreP5Synthetic"),
        ..WNDCLASSW::default()
    };
    // SAFETY: `class` is fully initialised and its strings are static. Re-registering in the same
    // process fails harmlessly (the class exists).
    unsafe { RegisterClassW(&class) };
    // SAFETY: registered class, static strings, no parent or create parameter.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("StreamCoreP5Synthetic"),
            w!("StreamCore P5 synthetic source"),
            WS_POPUP | WS_VISIBLE,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    // SAFETY: the window just created on this thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    // SAFETY: the window's DC, released below on this thread.
    let dc = unsafe { GetDC(Some(hwnd)) };
    // SAFETY: stock objects need no release.
    let (white, black) = unsafe {
        (
            HBRUSH(GetStockObject(WHITE_BRUSH).0),
            HBRUSH(GetStockObject(BLACK_BRUSH).0),
        )
    };
    // SAFETY: plain COLORREF values; deleted below.
    let backgrounds = unsafe {
        [
            CreateSolidBrush(COLORREF(0x0040_2010)),
            CreateSolidBrush(COLORREF(0x0010_80c0)),
        ]
    };
    let full = RECT {
        left: 0,
        top: 0,
        right: rect.2,
        bottom: rect.3,
    };
    fill(dc, full, backgrounds[0]);

    let mut log = Vec::new();
    let mut counter: u32 = 0;
    let mut msg = MSG::default();
    let start = now_ns();
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: message loop for this thread's window with a valid MSG buffer.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        match pace {
            Pace::EveryComposition => {
                // SAFETY: no arguments; waits for the next DWM composition.
                let _ = unsafe { DwmFlush() };
            }
            Pace::Every(ms) => {
                let next = start + u64::from(counter + 1) * u64::from(ms) * 1_000_000;
                let now = now_ns();
                if next > now {
                    std::thread::sleep(Duration::from_nanos((next - now).min(50_000_000)));
                    if now_ns() < next {
                        continue;
                    }
                }
            }
        }
        counter += 1;
        if let Pace::Every(_) = pace {
            fill(dc, full, backgrounds[(counter % 2) as usize]);
        }
        draw_code(dc, strip, counter as u16, white, black);
        // SAFETY: flushes this thread's GDI batch so the stamp follows the drawing.
        let _ = unsafe { GdiFlush() };
        log.push((counter, now_ns()));
    }
    // SAFETY: releases the DC and brushes created above and destroys this thread's window.
    unsafe {
        ReleaseDC(Some(hwnd), dc);
        for b in backgrounds {
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
        let _ = DestroyWindow(hwnd);
    }
    Ok(log)
}
