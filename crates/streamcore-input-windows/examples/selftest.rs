//! A self-test for a real Windows PC, run in the interactive session:
//!
//! ```text
//! cargo run -p streamcore-input-windows --example selftest -- --out selftest.txt
//! ```
//!
//! It opens its own window with a multi-line edit control, injects into it through an
//! [`Injector`](streamcore_input_windows::Injector) and reads back what arrived. Every check
//! prints `PASS name`, `FAIL name: detail` or `SKIP name: reason` (an `INFO` line is not a check);
//! the last line is
//! `SELFTEST n passed, m failed`, and the exit code is 0 only when nothing failed. `--out <path>`
//! writes the same lines to a file, for a launch with no console.
//!
//! The run moves the pointer, types into its own window and uses the clipboard. When it ends,
//! however it ends, everything it held is released, the window is gone, and the pointer and the
//! clipboard text are back as they were. If its edit control cannot get the keyboard focus it
//! says so and stops before typing anything.

// The test window, the clipboard and the read-backs are plain Win32, which is `unsafe` in
// windows-rs. The crate denies unsafe outside `src/win/sys`; this example allows it for itself.
#![allow(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    println!("selftest: this example injects real input and needs Windows; nothing was run");
}

#[cfg(windows)]
fn main() {
    std::process::exit(selftest::run());
}

#[cfg(windows)]
mod selftest {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::Write;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::mpsc::{self, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::thread::{JoinHandle, sleep};
    use std::time::{Duration, Instant};

    use streamcore_input::modifiers::{CONTROL, OPTION, SHIFT};
    use streamcore_input::{
        GesturePhase, KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec, NamedKey, PencilPhase,
        PencilSample, PointerKind, StrokeButton, TextMessage,
    };
    use streamcore_input_windows::keys::{MASKED_ALT_TAP, VK_CONTROL, VK_MENU, VK_SHIFT};
    use streamcore_input_windows::{
        ClickEvent, Feedback, InjectError, Injector, InputEvent, InputLease, PenEvent, ScrollEvent,
        TargetRect, TouchEvent, ZoomEvent,
    };
    use windows::Win32::Foundation::{
        GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{ClientToScreen, GetStockObject, HBRUSH, WHITE_BRUSH};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Controls::EM_GETSEL;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetKeyboardLayout, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS,
        KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, SetFocus, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CreateWindowExW, DefWindowProcW, DispatchMessageW, ES_AUTOVSCROLL,
        ES_MULTILINE, ES_WANTRETURN, GUITHREADINFO, GetClientRect, GetCursorPos,
        GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, IDC_ARROW, LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
        SPI_GETKEYBOARDDELAY, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SendMessageW, SetCursorPos,
        SetForegroundWindow, SetWindowTextW, SystemParametersInfoW, TranslateMessage, WINDOW_STYLE,
        WM_APP, WM_CLOSE, WM_DESTROY, WM_SETFOCUS, WNDCLASSW, WS_BORDER, WS_CHILD, WS_EX_TOPMOST,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };
    use windows::core::{HSTRING, w};

    /// Asks the window's thread to come to the foreground and focus its edit control.
    const WM_APP_ACTIVATE: u32 = WM_APP + 1;
    /// The clipboard format of UTF-16 text (`CF_UNICODETEXT`).
    const CF_UNICODETEXT: u32 = 13;
    /// The height of the plain strip under the edit control, where the pen, touch and wheel
    /// checks land so they cannot select or scroll the text.
    const STRIP_HEIGHT: i32 = 140;
    /// How long a check waits for the window to show what was injected.
    const ARRIVAL: Duration = Duration::from_secs(4);
    /// How long to watch for input that should not arrive.
    const SETTLE: Duration = Duration::from_millis(300);

    /// What the edit control holds after "line one", Return, "line two", and its length.
    const TWO_LINES: &str = "line one\r\nline two";
    const TWO_LINES_LENGTH: u32 = 18;

    /// The edit control, for the window procedure (a window handle as an integer).
    static EDIT: AtomicIsize = AtomicIsize::new(0);

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut c_void)
    }

    // The window.

    extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let edit = hwnd(EDIT.load(Ordering::SeqCst));
        match message {
            WM_APP_ACTIVATE => {
                // SAFETY: `window` is this thread's own live window, as the system passed it.
                // Both calls only ask; a refusal shows in the focus check that follows.
                unsafe {
                    let _ = BringWindowToTop(window);
                    let _ = SetForegroundWindow(window);
                }
                focus(edit);
                LRESULT(0)
            }
            // The keyboard focus belongs in the edit control whenever the window has it.
            WM_SETFOCUS => {
                focus(edit);
                LRESULT(0)
            }
            WM_DESTROY => {
                // SAFETY: ends this thread's message loop; no arguments to get wrong.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: the arguments are passed on exactly as the system delivered them.
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    fn focus(edit: HWND) {
        if !edit.is_invalid() {
            // SAFETY: `edit` is a child window of the calling thread; a failure changes nothing.
            let _ = unsafe { SetFocus(Some(edit)) };
        }
    }

    /// What the window's thread reports once the window exists: the window, its edit control (as
    /// integers) and the thread's ID.
    type Created = Result<(isize, isize, u32), String>;

    /// Creates the window and its edit control, then pumps messages until the window is closed.
    fn window_thread(created: &SyncSender<Created>) {
        let made = create_window();
        let failed = made.is_err();
        let _ = created.send(made);
        if failed {
            return;
        }
        let mut message = MSG::default();
        // SAFETY: `message` is a live MSG the calls fill and read, on the thread that owns the
        // window. GetMessageW returns 0 on WM_QUIT and -1 on an error; both end the loop.
        unsafe {
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn create_window() -> Created {
        let failed = |what: &str, error: windows::core::Error| format!("{what}: {error}");
        // SAFETY: a null name asks for the module of this process; the handle needs no release.
        let module = unsafe { GetModuleHandleW(None) }.map_err(|e| failed("module handle", e))?;
        let class_name = w!("StreamCoreInputSelfTest");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            // SAFETY: loading a stock system cursor; it is shared and never freed.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            // SAFETY: a stock object, owned by the system and never freed.
            hbrBackground: HBRUSH(unsafe { GetStockObject(WHITE_BRUSH) }.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        // SAFETY: `class` is fully initialized, and the strings it points to are static.
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(failed(
                "register class",
                windows::core::Error::from_thread(),
            ));
        }
        // SAFETY: the class was just registered by this thread; the strings are static. Topmost,
        // so another window cannot cover the control the checks click into.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                class_name,
                w!("Input injection self-test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                120,
                120,
                760,
                520,
                None,
                None,
                Some(module.into()),
                None,
            )
        }
        .map_err(|e| failed("create window", e))?;
        let mut client = RECT::default();
        // SAFETY: `window` is live and `client` is a RECT the call fills.
        unsafe { GetClientRect(window, &mut client) }.map_err(|e| failed("client area", e))?;
        let style = WS_CHILD
            | WS_VISIBLE
            | WS_BORDER
            | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32);
        // SAFETY: "EDIT" is a system class; the parent is this thread's live window.
        let edit = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("EDIT"),
                w!(""),
                style,
                0,
                0,
                client.right,
                (client.bottom - STRIP_HEIGHT).max(40),
                Some(window),
                None,
                Some(module.into()),
                None,
            )
        }
        .map_err(|e| failed("create edit control", e))?;
        EDIT.store(edit.0 as isize, Ordering::SeqCst);
        // SAFETY: returns the calling thread's ID.
        let thread = unsafe { GetCurrentThreadId() };
        Ok((window.0 as isize, edit.0 as isize, thread))
    }

    // Reading the computer's state.

    fn cursor() -> (i32, i32) {
        let mut point = POINT::default();
        // SAFETY: `point` is a live POINT the call fills; on failure it stays zeroed.
        let _ = unsafe { GetCursorPos(&mut point) };
        (point.x, point.y)
    }

    fn key_is_down(vk: u16) -> bool {
        // SAFETY: takes a virtual key and returns its state; no other effect.
        let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
        state < 0
    }

    /// A window's rectangle on the desktop.
    fn window_rect(window: HWND) -> TargetRect {
        let mut rect = RECT::default();
        // SAFETY: `rect` is a live RECT the call fills; on failure it stays zeroed.
        let _ = unsafe { GetWindowRect(window, &mut rect) };
        TargetRect::new(
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    }

    /// The plain strip under the edit control, on the desktop.
    fn strip_rect(window: HWND) -> TargetRect {
        let mut client = RECT::default();
        let mut origin = POINT::default();
        // SAFETY: `client` and `origin` are live structures the calls fill for a window of this
        // process; on failure they stay zeroed.
        unsafe {
            let _ = GetClientRect(window, &mut client);
            let _ = ClientToScreen(window, &mut origin);
        }
        let top = (client.bottom - STRIP_HEIGHT).max(40);
        TargetRect::new(origin.x, origin.y + top, client.right, client.bottom - top)
    }

    fn window_text(edit: HWND) -> String {
        // SAFETY: asks a window of this process for its text length, then copies at most the
        // buffer's length; its own thread answers.
        unsafe {
            let length = usize::try_from(GetWindowTextLengthW(edit)).unwrap_or(0);
            let mut buffer = vec![0u16; length + 1];
            let copied = usize::try_from(GetWindowTextW(edit, &mut buffer)).unwrap_or(0);
            String::from_utf16_lossy(&buffer[..copied.min(length)])
        }
    }

    fn set_window_text(edit: HWND, text: &str) {
        // SAFETY: the string is NUL-terminated and outlives the call, which the window's own
        // thread handles.
        let _ = unsafe { SetWindowTextW(edit, &HSTRING::from(text)) };
    }

    /// The edit control's selection as (start, end) in UTF-16 units; equal for a bare caret.
    fn selection(edit: HWND) -> (u32, u32) {
        let (mut start, mut end) = (0u32, 0u32);
        // SAFETY: EM_GETSEL writes two 32-bit values through the pointers passed as its
        // parameters; both locals outlive the call, which the control's own thread (in this
        // process) answers before SendMessageW returns.
        unsafe {
            SendMessageW(
                edit,
                EM_GETSEL,
                Some(WPARAM(&raw mut start as usize)),
                Some(LPARAM(&raw mut end as isize)),
            );
        }
        (start, end)
    }

    /// The keyboard layout of the window's thread, as the low 32 bits of its handle.
    fn layout_of(thread: u32) -> u32 {
        // SAFETY: takes a thread ID and returns that thread's layout handle.
        let layout = unsafe { GetKeyboardLayout(thread) };
        layout.0 as usize as u32
    }

    /// The system's keyboard delay before a held key repeats, in milliseconds.
    fn repeat_delay() -> Duration {
        let mut setting = 1u32;
        // SAFETY: for this action pvParam points to a 32-bit integer the call writes; `setting`
        // outlives the call and keeps its default on failure.
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETKEYBOARDDELAY,
                0,
                Some((&raw mut setting).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        Duration::from_millis((u64::from(setting.min(3)) + 1) * 250)
    }

    /// Whether the window is in the foreground with the keyboard focus in its edit control;
    /// otherwise, what is the matter.
    fn has_focus(window: HWND, edit: HWND, thread: u32) -> Result<(), String> {
        // SAFETY: takes no arguments and returns a window handle or null.
        let foreground = unsafe { GetForegroundWindow() };
        if foreground != window {
            return Err(format!(
                "the test window is not in the foreground (window {:?} is)",
                foreground.0
            ));
        }
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a live GUITHREADINFO with cbSize set, which the call fills.
        unsafe { GetGUIThreadInfo(thread, &mut info) }
            .map_err(|e| format!("the window's thread state cannot be read: {e}"))?;
        if info.hwndFocus != edit {
            return Err("the edit control does not have the keyboard focus".into());
        }
        Ok(())
    }

    /// An Alt tap with an unassigned key in the middle, straight through `SendInput`.
    fn masked_alt_tap() {
        let inputs = MASKED_ALT_TAP.map(|(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        });
        // SAFETY: `inputs` is a live array of initialized INPUT structures and cbSize is the
        // size of one of them.
        unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    }

    /// Brings the window to the foreground with the focus in its edit control.
    ///
    /// Windows refuses `SetForegroundWindow` from a process that is not the foreground one and
    /// has not just received input, which is the case when a scheduled task starts this. The
    /// workaround used is the one the documentation of `LockSetForegroundWindow` describes: the
    /// system re-enables `SetForegroundWindow` when the Alt key is pressed. So after a first
    /// plain attempt, each further one sends an Alt tap first. The tap carries an unassigned key
    /// between Alt's down and up, so whichever window receives it does not open its menu; no
    /// character is typed anywhere.
    fn activate(window: HWND, edit: HWND, thread: u32) -> Result<(), String> {
        let mut problem = String::new();
        for attempt in 0..6 {
            if attempt > 0 {
                masked_alt_tap();
            }
            // SAFETY: sends an application message to a window of this process, whose own thread
            // handles it.
            unsafe { SendMessageW(window, WM_APP_ACTIVATE, None, None) };
            sleep(Duration::from_millis(150));
            match has_focus(window, edit, thread) {
                Ok(()) => return Ok(()),
                Err(why) => problem = why,
            }
        }
        Err(problem)
    }

    // The clipboard.

    /// Runs `body` with the clipboard open, or returns `None` when another program holds it.
    fn with_clipboard<T>(body: impl FnOnce() -> T) -> Option<T> {
        for _ in 0..10 {
            // SAFETY: opens the clipboard for this thread; it is closed below before returning.
            if unsafe { OpenClipboard(None) }.is_ok() {
                let result = body();
                // SAFETY: closes what this thread opened just above.
                let _ = unsafe { CloseClipboard() };
                return Some(result);
            }
            sleep(Duration::from_millis(30));
        }
        None
    }

    /// The clipboard's text, if it holds any.
    fn clipboard_text() -> Option<String> {
        with_clipboard(|| {
            // SAFETY: the clipboard is open. The handle stays owned by the clipboard; it is
            // locked only while its NUL-terminated UTF-16 text is copied out, reading no further
            // than the size the system reports for it.
            unsafe {
                let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
                let memory = HGLOBAL(handle.0);
                let units = GlobalSize(memory) / size_of::<u16>();
                let data = GlobalLock(memory).cast::<u16>();
                if data.is_null() {
                    return None;
                }
                let text = std::slice::from_raw_parts(data, units);
                let end = text.iter().position(|unit| *unit == 0).unwrap_or(units);
                let text = String::from_utf16_lossy(&text[..end]);
                let _ = GlobalUnlock(memory);
                Some(text)
            }
        })
        .flatten()
    }

    /// Replaces the clipboard's contents with `text`, or empties it for `None`.
    fn set_clipboard_text(text: Option<&str>) {
        with_clipboard(|| {
            // SAFETY: the clipboard is open, and emptying it makes this thread its owner.
            let _ = unsafe { EmptyClipboard() };
            let Some(text) = text else {
                return;
            };
            let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
            // SAFETY: the block is allocated for exactly `units`, locked while they are copied
            // in, and then handed to the clipboard, which owns it from there. If the clipboard
            // refuses it, it is freed here.
            unsafe {
                let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, size_of_val(units.as_slice())) else {
                    return;
                };
                let data = GlobalLock(memory).cast::<u16>();
                if data.is_null() {
                    let _ = GlobalFree(Some(memory));
                    return;
                }
                std::ptr::copy_nonoverlapping(units.as_ptr(), data, units.len());
                let _ = GlobalUnlock(memory);
                if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).is_err() {
                    let _ = GlobalFree(Some(memory));
                }
            }
        });
    }

    // The report.

    struct Report {
        out: Option<File>,
        passed: u32,
        failed: u32,
    }

    impl Report {
        fn line(&mut self, line: &str) {
            println!("{line}");
            if let Some(out) = &mut self.out {
                // The console line above is the fallback when the file cannot be written.
                let _ = writeln!(out, "{line}").and_then(|()| out.flush());
            }
        }

        fn check(&mut self, name: &str, outcome: Outcome) {
            match outcome {
                Ok(detail) if detail.is_empty() => {
                    self.passed += 1;
                    self.line(&format!("PASS {name}"));
                }
                Ok(detail) => {
                    self.passed += 1;
                    self.line(&format!("PASS {name}: {detail}"));
                }
                Err(Problem::Skipped(reason)) => self.line(&format!("SKIP {name}: {reason}")),
                Err(Problem::Failed(detail)) => {
                    self.failed += 1;
                    self.line(&format!("FAIL {name}: {detail}"));
                }
            }
        }
    }

    enum Problem {
        Failed(String),
        /// The computer cannot do this (no pen or touch device); neither a pass nor a failure.
        Skipped(String),
    }

    impl From<String> for Problem {
        fn from(detail: String) -> Self {
            Self::Failed(detail)
        }
    }

    /// A passed check's detail (may be empty), or why it did not pass.
    type Outcome = Result<String, Problem>;

    fn fail<T>(detail: impl Into<String>) -> Result<T, Problem> {
        Err(Problem::Failed(detail.into()))
    }

    // Leaving nothing behind.

    /// Puts the computer back when the run ends, however it ends: the window is closed and the
    /// pointer and the clipboard text restored. The injector is declared after this and so is
    /// dropped before it, which releases everything it held.
    struct Restore {
        window: isize,
        thread: Option<JoinHandle<()>>,
        cursor: (i32, i32),
        /// The clipboard's text before the run touched it (`Some(None)`: it held no text).
        clipboard: RefCell<Option<Option<String>>>,
    }

    impl Restore {
        fn restore_clipboard(&self) {
            if let Some(before) = self.clipboard.borrow_mut().take() {
                set_clipboard_text(before.as_deref());
            }
        }
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            self.restore_clipboard();
            // SAFETY: posts a close request to a window of this process (ignored if it is
            // already gone), and moves the pointer back; neither can harm.
            unsafe {
                let _ = PostMessageW(Some(hwnd(self.window)), WM_CLOSE, WPARAM(0), LPARAM(0));
                let _ = SetCursorPos(self.cursor.0, self.cursor.1);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    // The checks.

    struct Bench<'a> {
        injector: Injector,
        lease: InputLease,
        feedback: Arc<Mutex<Vec<Feedback>>>,
        window: HWND,
        edit: HWND,
        thread: u32,
        restore: &'a Restore,
    }

    fn sample(phase: PencilPhase, kind: PointerKind, x: f32, y: f32, force: f32) -> PencilSample {
        PencilSample {
            phase,
            kind,
            button: StrokeButton::Left,
            x,
            y,
            force,
            altitude: 1.2,
            azimuth: 0.5,
            roll: 0.0,
            hover_z: 0.0,
            timestamp: 0.0,
        }
    }

    fn shortcut(key: KeySpec, modifiers: u8, phase: KeyPhase, auto_repeat: bool) -> InputEvent {
        InputEvent::KeyShortcut(KeyShortcutMessage {
            key,
            modifiers,
            phase,
            auto_repeat,
        })
    }

    fn tap(key: KeySpec, modifiers: u8) -> InputEvent {
        shortcut(key, modifiers, KeyPhase::Tap, false)
    }

    fn key(hid_usage: u16, is_down: bool, modifiers: u8) -> InputEvent {
        InputEvent::Key(KeyPacket {
            is_down,
            hid_usage,
            modifiers,
        })
    }

    fn text(text: &str) -> InputEvent {
        InputEvent::Text(TextMessage {
            text: text.into(),
            replace_before: 0,
        })
    }

    const HID_A: u16 = 0x04;
    const HID_C: u16 = 0x06;
    const HID_R: u16 = 0x15;
    const HID_V: u16 = 0x19;
    const HID_LEFT_SHIFT: u16 = 0xE1;

    impl Bench<'_> {
        fn inject(&self, event: InputEvent) -> Result<(), Problem> {
            self.injector
                .inject(event, &self.lease)
                .map_err(|e| Problem::Failed(format!("inject: {e}")))
        }

        /// Keys may only be injected while they will land in the edit control.
        fn need_focus(&self) -> Result<(), Problem> {
            has_focus(self.window, self.edit, self.thread)
                .map_err(|why| Problem::Failed(format!("{why}; no keys were injected")))
        }

        fn take_feedback(&self) -> Vec<Feedback> {
            match self.feedback.lock() {
                Ok(mut heard) => std::mem::take(&mut *heard),
                Err(_) => Vec::new(),
            }
        }

        fn clear(&self) {
            set_window_text(self.edit, "");
        }

        /// Waits until the edit control holds exactly `want`, then watches that nothing more
        /// arrives.
        fn expect_text(&self, want: &str) -> Result<(), Problem> {
            let started = Instant::now();
            while window_text(self.edit) != want {
                if started.elapsed() > ARRIVAL {
                    let got = window_text(self.edit);
                    return fail(format!("expected {want:?}, the control holds {got:?}"));
                }
                sleep(Duration::from_millis(10));
            }
            sleep(SETTLE);
            let got = window_text(self.edit);
            if got != want {
                return fail(format!("expected {want:?}, but more arrived: {got:?}"));
            }
            Ok(())
        }

        fn capabilities(&self) -> Outcome {
            let capabilities = self.injector.capabilities();
            Ok(format!(
                "pen device {}, touch device {}",
                if capabilities.pen {
                    "available"
                } else {
                    "not available"
                },
                if capabilities.touch {
                    "available"
                } else {
                    "not available"
                },
            ))
        }

        fn click(&self) -> Outcome {
            let target = window_rect(self.edit);
            self.inject(InputEvent::Click(ClickEvent {
                x: 0.5,
                y: 0.5,
                button: StrokeButton::Left,
                phase: KeyPhase::Tap,
                target,
            }))?;
            let want = (target.x + target.width / 2, target.y + target.height / 2);
            let started = Instant::now();
            loop {
                let at = cursor();
                if (at.0 - want.0).abs() <= 2 && (at.1 - want.1).abs() <= 2 {
                    break;
                }
                if started.elapsed() > ARRIVAL {
                    return fail(format!("the pointer is at {at:?}, expected {want:?}"));
                }
                sleep(Duration::from_millis(10));
            }
            sleep(Duration::from_millis(100));
            // A click must not have cost the control its focus.
            self.need_focus()?;
            Ok(format!("pointer at {want:?}"))
        }

        fn text_ascii(&self) -> Outcome {
            self.need_focus()?;
            self.clear();
            let line = "Hello, World 123! (a+b)=c";
            self.inject(text(line))?;
            self.expect_text(line)?;
            Ok(String::new())
        }

        fn text_unicode(&self) -> Outcome {
            self.need_focus()?;
            self.clear();
            // Accented Latin, Cyrillic, and an emoji that needs a surrogate pair.
            let line = "caf\u{e9} na\u{ef}ve \u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{1F600}";
            self.inject(text(line))?;
            self.expect_text(line)?;
            Ok(String::new())
        }

        /// Runs a shortcut check. When it fails, the detail also carries every feedback the
        /// injector gave during it and which modifiers are down right after it, so an
        /// unavailable key or a stuck or missing modifier shows.
        fn shortcut_check(&self, body: impl FnOnce() -> Outcome) -> Outcome {
            self.take_feedback();
            body().map_err(|problem| match problem {
                Problem::Failed(detail) => {
                    let heard = self.take_feedback();
                    Problem::Failed(format!(
                        "{detail}; feedback during the check: {heard:?}; modifiers down after it: \
                         Ctrl {}, Shift {}, Alt {}",
                        key_is_down(VK_CONTROL),
                        key_is_down(VK_SHIFT),
                        key_is_down(VK_MENU),
                    ))
                }
                skipped => skipped,
            })
        }

        /// Waits until the edit control's selection is `want` (start, end; equal for a bare
        /// caret).
        fn expect_selection(&self, want: (u32, u32)) -> Result<(), Problem> {
            let started = Instant::now();
            while selection(self.edit) != want {
                if started.elapsed() > ARRIVAL {
                    let got = selection(self.edit);
                    return fail(format!("expected the selection {want:?}, it is {got:?}"));
                }
                sleep(Duration::from_millis(10));
            }
            Ok(())
        }

        /// Types the two lines the caret checks move in, leaving the caret at the end.
        fn type_two_lines(&self) -> Result<(), Problem> {
            self.need_focus()?;
            self.clear();
            self.inject(text("line one\nline two"))?;
            self.expect_text(TWO_LINES)
        }

        fn ctrl_home(&self) -> Outcome {
            self.shortcut_check(|| {
                self.type_two_lines()?;
                self.expect_selection((TWO_LINES_LENGTH, TWO_LINES_LENGTH))?;
                self.inject(tap(KeySpec::named(NamedKey::Home), CONTROL))?;
                // A plain Home would stop at the start of the second line (10).
                self.expect_selection((0, 0))?;
                Ok(String::new())
            })
        }

        fn ctrl_end(&self) -> Outcome {
            self.shortcut_check(|| {
                self.type_two_lines()?;
                self.inject(tap(KeySpec::named(NamedKey::Home), CONTROL))?;
                self.expect_selection((0, 0))?;
                self.inject(tap(KeySpec::named(NamedKey::End), CONTROL))?;
                // A plain End would stop at the end of the first line (8).
                self.expect_selection((TWO_LINES_LENGTH, TWO_LINES_LENGTH))?;
                Ok(String::new())
            })
        }

        fn shift_select(&self) -> Outcome {
            self.shortcut_check(|| {
                self.type_two_lines()?;
                self.inject(tap(KeySpec::named(NamedKey::End), CONTROL))?;
                self.inject(tap(KeySpec::named(NamedKey::Home), CONTROL | SHIFT))?;
                self.expect_selection((0, TWO_LINES_LENGTH))?;
                self.inject(tap(KeySpec::named(NamedKey::Delete), 0))?;
                self.expect_text("")?;
                Ok(String::new())
            })
        }

        fn copy_paste(&self) -> Outcome {
            self.shortcut_check(|| {
                self.need_focus()?;
                self.clear();
                *self.restore.clipboard.borrow_mut() = Some(clipboard_text());
                // Not what the copy is expected to put there, so a stale match proves nothing.
                set_clipboard_text(None);
                let outcome = self.copy_and_paste();
                self.restore.restore_clipboard();
                outcome
            })
        }

        fn copy_and_paste(&self) -> Outcome {
            let line = "copy me";
            self.inject(text(line))?;
            self.expect_text(line)?;
            self.inject(tap(KeySpec::named(NamedKey::Home), CONTROL | SHIFT))?;
            self.expect_selection((0, 7))?;
            // A character shortcut with Ctrl, resolved on the layout.
            self.inject(tap(KeySpec::char_with_hid("c", HID_C), CONTROL))?;
            let started = Instant::now();
            let mut copied = clipboard_text();
            while copied.as_deref() != Some(line) && started.elapsed() < ARRIVAL {
                sleep(Duration::from_millis(20));
                copied = clipboard_text();
            }
            if copied.as_deref() != Some(line) {
                return fail(format!(
                    "the copy half failed: after Ctrl+C the clipboard text is {copied:?}"
                ));
            }
            self.inject(tap(KeySpec::named(NamedKey::End), CONTROL))?;
            self.inject(tap(KeySpec::char_with_hid("v", HID_V), CONTROL))?;
            self.expect_text("copy mecopy me").map_err(|problem| match problem {
                Problem::Failed(detail) => Problem::Failed(format!(
                    "the paste half failed (the clipboard held {copied:?} after Ctrl+C): {detail}"
                )),
                skipped => skipped,
            })?;
            Ok(String::new())
        }

        /// Whether this edit control selects everything on Ctrl+A. A classic multi-line edit
        /// control does not, so this informs and is not a check.
        fn ctrl_a(&self) -> String {
            if self.type_two_lines().is_err() {
                return "not tried, the text could not be typed".into();
            }
            if self
                .inject(tap(KeySpec::char_with_hid("a", HID_A), CONTROL))
                .is_err()
            {
                return "not tried, the shortcut was not accepted".into();
            }
            sleep(Duration::from_millis(400));
            if selection(self.edit) == (0, TWO_LINES_LENGTH) {
                "the control selected all".into()
            } else {
                "the control ignored Ctrl+A".into()
            }
        }

        fn raw_key(&self) -> Outcome {
            self.need_focus()?;
            self.clear();
            let layout = layout_of(self.thread);
            let us = layout == 0x0409_0409;
            // The A position with Shift flagged and no Shift key, as an on-screen keyboard sends
            // a capital.
            self.inject(key(HID_A, true, SHIFT))?;
            self.inject(key(HID_A, false, SHIFT))?;
            let described = format!("layout {layout:08X}{}", if us { " (US)" } else { "" });
            if us {
                self.expect_text("A")?;
                return Ok(described);
            }
            let started = Instant::now();
            while window_text(self.edit).is_empty() && started.elapsed() < ARRIVAL {
                sleep(Duration::from_millis(10));
            }
            sleep(SETTLE);
            let got = window_text(self.edit);
            if got.chars().count() == 1 {
                Ok(format!("{described}, the key typed {got:?}"))
            } else {
                fail(format!("{described}, expected one character, got {got:?}"))
            }
        }

        /// A held key repeats, and stops with its up. Windows repeats only real keyboards' keys,
        /// so the injector repeats a held shortcut itself; a raw key position does not repeat.
        fn held_key(&self) -> Outcome {
            self.need_focus()?;
            self.clear();
            let held = KeySpec::char_with_hid("r", HID_R);
            self.inject(shortcut(held.clone(), 0, KeyPhase::Down, true))?;
            sleep(repeat_delay() + Duration::from_millis(400));
            self.inject(shortcut(held, 0, KeyPhase::Up, false))?;
            sleep(Duration::from_millis(150));
            let after_up = window_text(self.edit);
            sleep(SETTLE);
            let later = window_text(self.edit);
            let count = after_up.chars().count();
            if count < 2 || after_up.chars().any(|c| c != 'r') {
                return fail(format!(
                    "expected the key to repeat, the control holds {after_up:?}"
                ));
            }
            if later != after_up {
                let extra = later.chars().count() - count;
                return fail(format!(
                    "{extra} more characters arrived after the key came up"
                ));
            }
            Ok(format!("{count} characters"))
        }

        fn pen_event(&self, sample: PencilSample) -> Result<(), Problem> {
            self.inject(InputEvent::Pen(PenEvent {
                sample,
                target: strip_rect(self.window),
                right_button_is_barrel: false,
            }))
        }

        /// One stroke across the strip with rising pressure.
        fn stroke(&self, kind: PointerKind, y: f32) -> Result<(), Problem> {
            self.pen_event(sample(PencilPhase::Down, kind, 0.3, y, 0.1))?;
            for step in 1..=6u8 {
                let along = f32::from(step) / 6.0;
                sleep(Duration::from_millis(15));
                let moved = sample(
                    PencilPhase::Moved,
                    kind,
                    0.3 + 0.3 * along,
                    y,
                    0.1 + 0.8 * along,
                );
                self.pen_event(moved)?;
            }
            sleep(Duration::from_millis(15));
            self.pen_event(sample(PencilPhase::Up, kind, 0.6, y, 0.0))
        }

        /// What the pen feedback since the last look says went wrong, if anything.
        fn pen_problems(&self) -> Result<(), Problem> {
            sleep(Duration::from_millis(200));
            let heard = self.take_feedback();
            let bad: Vec<_> = heard
                .iter()
                .filter(|f| matches!(f, Feedback::PenFailed | Feedback::StrokeCancelled))
                .collect();
            if bad.is_empty() {
                Ok(())
            } else {
                fail(format!("feedback: {bad:?}"))
            }
        }

        fn pen(&self) -> Outcome {
            if !self.injector.capabilities().pen {
                return Err(Problem::Skipped("no synthetic pen device".into()));
            }
            self.take_feedback();
            let strip = strip_rect(self.window);
            self.pen_event(sample(PencilPhase::Hover, PointerKind::Pen, 0.2, 0.4, 0.0))?;
            sleep(Duration::from_millis(200));
            let want = (strip.x + strip.width / 5, strip.y + strip.height * 2 / 5);
            let at = cursor();
            let followed = (at.0 - want.0).abs() <= 3 && (at.1 - want.1).abs() <= 3;
            self.stroke(PointerKind::Pen, 0.4)?;
            self.pen_event(sample(
                PencilPhase::HoverExit,
                PointerKind::Pen,
                0.6,
                0.4,
                0.0,
            ))?;
            self.pen_problems()?;
            Ok(format!(
                "on the hover the system pointer {} (at {at:?}, pen at {want:?})",
                if followed {
                    "followed"
                } else {
                    "did not follow"
                }
            ))
        }

        fn eraser(&self) -> Outcome {
            if !self.injector.capabilities().pen {
                return Err(Problem::Skipped("no synthetic pen device".into()));
            }
            self.take_feedback();
            // No hover first: the eraser has to enter range by itself.
            self.stroke(PointerKind::Eraser, 0.6)?;
            self.pen_problems()?;
            Ok(String::new())
        }

        fn touch(&self) -> Outcome {
            if !self.injector.capabilities().touch {
                return Err(Problem::Skipped("no synthetic touch device".into()));
            }
            self.take_feedback();
            let target = strip_rect(self.window);
            let fingers = |phase, spread: f32| {
                InputEvent::Touch(TouchEvent {
                    phase,
                    contacts: [(0.5 - spread, 0.5), (0.5 + spread, 0.5)],
                    target,
                })
            };
            self.inject(fingers(GesturePhase::Began, 0.05))?;
            for step in 1..=6u8 {
                sleep(Duration::from_millis(20));
                self.inject(fingers(
                    GesturePhase::Changed,
                    0.05 + 0.03 * f32::from(step),
                ))?;
            }
            sleep(Duration::from_millis(20));
            self.inject(fingers(GesturePhase::Ended, 0.23))?;
            sleep(Duration::from_millis(200));
            let heard = self.take_feedback();
            if heard.contains(&Feedback::TouchFailed) {
                return fail("Windows refused the touch contacts");
            }
            Ok(String::new())
        }

        fn wheel_and_zoom(&self) -> Outcome {
            self.take_feedback();
            let target = strip_rect(self.window);
            let zoom = |phase, delta| {
                InputEvent::Zoom(ZoomEvent {
                    phase,
                    delta,
                    anchor: Some((0.5, 0.5)),
                    // Alt, the harder case: a lone Alt would put the window into its menu.
                    modifiers: OPTION,
                    target,
                })
            };
            // The zoom goes first: it puts the pointer over the strip, where the wheel then lands.
            self.inject(zoom(GesturePhase::Began, 0.0))?;
            self.inject(zoom(GesturePhase::Changed, 0.6))?;
            self.inject(zoom(GesturePhase::Ended, 0.0))?;
            self.inject(InputEvent::Scroll(ScrollEvent {
                dx: 0.0,
                dy: 0.2,
                target,
            }))?;
            sleep(Duration::from_millis(200));
            if key_is_down(VK_MENU) || key_is_down(VK_CONTROL) {
                return fail("a zoom modifier is still down");
            }
            let heard = self.take_feedback();
            if !heard.is_empty() {
                return fail(format!("unexpected feedback: {heard:?}"));
            }
            // Had the Alt been left alone, the window would be in its menu and eat this.
            self.need_focus()?;
            self.clear();
            self.inject(text("after zoom"))?;
            self.expect_text("after zoom")?;
            Ok(String::new())
        }

        /// Holds Shift and a letter, revokes, and looks at the keys. Consumes the lease.
        fn release(&self) -> Outcome {
            self.need_focus()?;
            self.clear();
            let letter = u16::from(b'A');
            self.inject(key(HID_LEFT_SHIFT, true, 0))?;
            self.inject(key(HID_A, true, 0))?;
            let started = Instant::now();
            while !(key_is_down(VK_SHIFT) && key_is_down(letter)) {
                if started.elapsed() > ARRIVAL {
                    return fail("the held keys never showed as down");
                }
                sleep(Duration::from_millis(10));
            }
            self.lease.revoke();
            // The release is done when revoke returns; no waiting here.
            let (shift, key_down) = (key_is_down(VK_SHIFT), key_is_down(letter));
            if shift || key_down {
                return fail(format!(
                    "still down after revoke: Shift {shift}, the letter {key_down}"
                ));
            }
            match self.injector.inject(key(HID_A, true, 0), &self.lease) {
                Err(InjectError::StaleLease) => Ok(String::new()),
                other => fail(format!(
                    "the revoked lease answered {other:?}, not StaleLease"
                )),
            }
        }

        fn new_lease(&self) -> Outcome {
            match self.injector.lease() {
                Some(lease) if lease.is_current() => Ok(String::new()),
                Some(_) => fail("the new lease is not current"),
                None => fail("no lease after the revoke"),
            }
        }
    }

    fn checks(bench: &Bench<'_>, report: &mut Report) {
        report.check("capabilities", bench.capabilities());
        report.check("click", bench.click());
        report.check("text-ascii", bench.text_ascii());
        report.check("text-unicode", bench.text_unicode());
        report.check("shortcut-ctrl-home", bench.ctrl_home());
        report.check("shortcut-ctrl-end", bench.ctrl_end());
        report.check("shortcut-shift-select", bench.shift_select());
        report.check("shortcut-copy-paste", bench.copy_paste());
        let ctrl_a = bench.ctrl_a();
        report.line(&format!("INFO ctrl-a: {ctrl_a}"));
        report.check("raw-key-shift", bench.raw_key());
        report.check("held-key-repeat", bench.held_key());
        report.check("pen", bench.pen());
        report.check("eraser", bench.eraser());
        report.check("touch", bench.touch());
        report.check("wheel-and-zoom", bench.wheel_and_zoom());
        report.check("release-on-revoke", bench.release());
        report.check("new-lease", bench.new_lease());
    }

    fn out_path() -> Option<String> {
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            if argument == "--out" {
                return arguments.next();
            }
        }
        None
    }

    /// Runs the self-test and returns the process's exit code.
    pub fn run() -> i32 {
        let out = out_path().and_then(|path| match File::create(&path) {
            Ok(file) => Some(file),
            Err(error) => {
                eprintln!("selftest: cannot write {path}: {error}");
                None
            }
        });
        let mut report = Report {
            out,
            passed: 0,
            failed: 0,
        };
        // Physical pixels everywhere, as the injector uses them.
        // SAFETY: sets this process's DPI awareness before it creates any window; a failure
        // (already set by a manifest) changes nothing.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let outcome = catch_unwind(AssertUnwindSafe(|| session(&mut report)));
        if outcome.is_err() {
            report.check("selftest", fail("the run panicked; see the console"));
        }
        let summary = format!(
            "SELFTEST {} passed, {} failed",
            report.passed, report.failed
        );
        report.line(&summary);
        i32::from(report.failed != 0)
    }

    /// Opens the window, runs the checks and puts everything back.
    fn session(report: &mut Report) {
        let start_cursor = cursor();
        let (created, ready) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("selftest-window".into())
            .spawn(move || window_thread(&created));
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => return report.check("window", fail(format!("no thread: {error}"))),
        };
        let (window, edit, window_thread_id) = match ready.recv() {
            Ok(Ok(made)) => made,
            Ok(Err(why)) => return report.check("window", fail(why)),
            Err(_) => return report.check("window", fail("the window's thread stopped")),
        };
        // From here on the computer is put back however this function ends.
        let restore = Restore {
            window,
            thread: Some(thread),
            cursor: start_cursor,
            clipboard: RefCell::new(None),
        };
        let (window, edit) = (hwnd(window), hwnd(edit));
        if let Err(why) = activate(window, edit, window_thread_id) {
            let detail = format!("{why}; nothing was injected");
            return report.check("focus", fail(detail));
        }
        report.check("focus", Ok(String::new()));

        let feedback = Arc::new(Mutex::new(Vec::new()));
        let sink = feedback.clone();
        let injector = Injector::new(move |heard| {
            if let Ok(mut all) = sink.lock() {
                all.push(heard);
            }
        });
        let injector = match injector {
            Ok(injector) => injector,
            Err(error) => return report.check("injector", fail(format!("did not start: {error}"))),
        };
        let Some(lease) = injector.lease() else {
            return report.check("lease", fail("a new injector gave no lease"));
        };
        let bench = Bench {
            injector,
            lease,
            feedback,
            window,
            edit,
            thread: window_thread_id,
            restore: &restore,
        };
        checks(&bench, report);
    }
}
