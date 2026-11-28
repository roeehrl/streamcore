//! The state of the Windows desktop a stream host reports and acts on: the focused window's
//! keyboard layout, the foreground app, the windows a switcher would list, bringing one of them
//! to the foreground, whether the desktop can take input now, and the Windows version.
//!
//! Every function here is safe to call from any thread and injects nothing: none of them needs
//! the [`InputLease`](crate::InputLease). They are polls. Nothing here registers for a
//! notification, because the notifications Windows offers for these need a window and a message
//! loop, which is the host's to own.
//!
//! The functions are built on Windows only. The types and the rules they apply (which windows a
//! switcher lists, how the session's state is told, how a layout is described) are portable and
//! tested on every platform.
//!
//! **A process with no interactive desktop** (a service, or a shell reached over SSH) has no
//! foreground window and sees no application windows: [`focused_layout`], [`focused_layout_id`]
//! and [`foreground_app`] return `None`, [`running_apps`] is empty, [`activate`] returns false
//! and [`session_state`] is [`SessionState::Detached`].
//!
//! **Not here.** App icons (a host that wants them reads them from the executable at
//! [`AppWindow::exe_path`]); change notifications; whether the display is off or asleep (see
//! [`SessionState`]).

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use streamcore_input::LayoutInfoMessage;

use crate::keys;
use crate::layout::KeyLayout;

#[cfg(windows)]
use crate::win::sys;

/// A top-level window, as [`foreground_app`] and [`running_apps`] name it and [`activate`] takes
/// it.
///
/// Opaque: it can be compared, hashed and handed back, nothing else. It is valid on this computer
/// until the window closes, and means nothing in another run or on another computer. Windows may
/// reuse the value for a new window after the old one has closed, so a host should take a fresh
/// list rather than keep ids for long.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub(crate) isize);

impl fmt::Debug for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WindowId({:#x})", self.0)
    }
}

/// Which keyboard layout the focused window uses, for noticing a change: compare it with the
/// last one read. Opaque, and valid in this run only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayoutId(pub(crate) usize);

/// The app in the foreground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundApp {
    /// The process that owns the window; for a packaged (UWP) app, the app's own process rather
    /// than the frame host's when it can be told (see [`running_apps`]).
    pub pid: u32,
    /// The full path of the process's executable. `None` when Windows will not say (a protected
    /// process, or one that has just exited).
    pub exe_path: Option<PathBuf>,
    /// The window's title; empty when it has none.
    pub window_title: String,
    pub window: WindowId,
}

impl ForegroundApp {
    /// The executable's file name ("notepad.exe"), as Windows spells it.
    pub fn exe_name(&self) -> Option<String> {
        self.exe_path.as_deref().and_then(exe_name)
    }
}

/// One window a switcher would list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppWindow {
    pub window: WindowId,
    /// As [`ForegroundApp::pid`].
    pub pid: u32,
    /// As [`ForegroundApp::exe_path`].
    pub exe_path: Option<PathBuf>,
    /// The window's title, never empty.
    pub title: String,
    /// The window is minimized. [`activate`] restores it.
    pub minimized: bool,
}

impl AppWindow {
    /// The executable's file name ("notepad.exe"), as Windows spells it.
    pub fn exe_name(&self) -> Option<String> {
        self.exe_path.as_deref().and_then(exe_name)
    }
}

/// The file name of a Windows path. Splits on both separators itself, so it reads a Windows path
/// the same way on every platform.
pub fn exe_name(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    let name = text.rsplit(['\\', '/']).next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// The process that draws the frame of every packaged (UWP) app's window.
const FRAME_HOST: &str = "ApplicationFrameHost.exe";

/// Whether a window's process is the frame host of packaged apps, whose own process then has to
/// be looked up from the window inside the frame.
pub fn is_frame_host(exe_path: Option<&Path>) -> bool {
    exe_path
        .and_then(exe_name)
        .is_some_and(|name| name.eq_ignore_ascii_case(FRAME_HOST))
}

// ---------------------------------------------------------------------------------------------
// The switcher's list

/// `WS_EX_TOOLWINDOW`: a floating toolbar; never in the taskbar or the switcher.
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
/// `WS_EX_APPWINDOW`: forced onto the taskbar while visible.
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;
/// `WS_EX_NOACTIVATE`: the user cannot activate it, so it is not listed unless it asks to be.
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// What decides whether a top-level window is one a switcher lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowFacts {
    /// `IsWindowVisible`.
    pub visible: bool,
    /// DWM's `DWMWA_CLOAKED` is not zero: the window exists and counts as visible but is not
    /// shown (a window on another virtual desktop, a suspended packaged app, a shell surface).
    pub cloaked: bool,
    /// The window has a title.
    pub titled: bool,
    /// The window has an owner (`GW_OWNER`): a dialog or a palette of another window.
    pub owned: bool,
    /// The window is the shell's desktop: the window `GetShellWindow` names, or one of the
    /// shell's desktop classes (see [`is_shell_desktop_class`]).
    pub shell_desktop: bool,
    /// The extended window style (`GWL_EXSTYLE`).
    pub ex_style: u32,
}

/// Whether a window class is one the shell draws the desktop with: `Progman` (titled "Program
/// Manager") and `WorkerW`, the windows behind the icons and the wallpaper. They are visible,
/// titled, unowned top-level windows with no style that says they are not applications, so
/// only their class tells them apart.
pub fn is_shell_desktop_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("Progman") || class.eq_ignore_ascii_case("WorkerW")
}

/// Whether a switcher lists the window. The rule is the one Windows documents for taskbar
/// buttons, which Alt+Tab follows too, and of what is shown only what the user can see:
///
/// - the window is visible, not cloaked, and has a title;
/// - it is not the shell's desktop, whatever its styles say;
/// - `WS_EX_APPWINDOW` lists it whatever else is set;
/// - otherwise it is listed when it has no owner and is neither a tool window
///   (`WS_EX_TOOLWINDOW`) nor one the user cannot activate (`WS_EX_NOACTIVATE`).
///
/// An owned window (a modal dialog) is therefore not listed on its own: its owner is, and
/// activating the owner brings the dialog with it.
pub fn is_switcher_window(facts: &WindowFacts) -> bool {
    if !facts.visible || facts.cloaked || !facts.titled || facts.shell_desktop {
        return false;
    }
    if facts.ex_style & WS_EX_APPWINDOW != 0 {
        return true;
    }
    !facts.owned && facts.ex_style & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) == 0
}

// ---------------------------------------------------------------------------------------------
// The session

/// Whether the interactive desktop can take input now.
///
/// **What this cannot tell.** Whether the display is off or the computer is about to sleep:
/// Windows reports that only as power notifications to a window or a service, which a poll cannot
/// read. Which secure desktop has the input (a consent prompt, the sign-in screen or
/// Ctrl+Alt+Del all look the same from a user's process). And a lock that Windows has not
/// reported yet: the flag is set a moment after the lock screen appears, and in that moment the
/// answer is [`SecureDesktop`](Self::SecureDesktop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionState {
    /// The desktop this process runs on has the input: injected input reaches its windows.
    Ready,
    /// The workstation is locked.
    Locked,
    /// Another desktop has the input and the session is not reported as locked: a consent (UAC)
    /// prompt on the secure desktop, the sign-in screen, Ctrl+Alt+Del or a screen saver that
    /// asks for the password. Input injected now reaches nothing.
    SecureDesktop,
    /// This process's session has no interactive desktop in front of anyone: it runs in a
    /// service's session or on a window station of its own (a shell reached over SSH), or its
    /// remote-desktop or console session is disconnected.
    Detached,
}

/// What [`session_state`] reads, for [`classify_session`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionFacts {
    /// The process's window station is the interactive one (`WinSta0`).
    pub interactive_station: bool,
    /// The session is connected to its console or remote display; `None` when Windows will not
    /// say.
    pub connected: Option<bool>,
    /// The session is locked; `None` when Windows reports the lock state as unknown.
    pub locked: Option<bool>,
    /// The name of the desktop that has the input; `None` when this process may not open it,
    /// which is the case for the secure desktop.
    pub input_desktop: Option<String>,
    /// The name of the desktop the calling thread runs on.
    pub own_desktop: Option<String>,
}

/// The name of the desktop applications run on.
const APPLICATION_DESKTOP: &str = "Default";

/// Tells the session's state from what was read. In order: no interactive window station or a
/// disconnected session is [`Detached`](SessionState::Detached); a reported lock is
/// [`Locked`](SessionState::Locked); the input on this thread's own desktop is
/// [`Ready`](SessionState::Ready); anything else is
/// [`SecureDesktop`](SessionState::SecureDesktop).
pub fn classify_session(facts: &SessionFacts) -> SessionState {
    if !facts.interactive_station || facts.connected == Some(false) {
        return SessionState::Detached;
    }
    if facts.locked == Some(true) {
        return SessionState::Locked;
    }
    let own = facts.own_desktop.as_deref().unwrap_or(APPLICATION_DESKTOP);
    match facts.input_desktop.as_deref() {
        Some(input) if input.eq_ignore_ascii_case(own) => SessionState::Ready,
        _ => SessionState::SecureDesktop,
    }
}

// ---------------------------------------------------------------------------------------------
// The keyboard layout

/// The arrangement of the letters, told from the top letter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayoutFamily {
    Qwerty,
    Azerty,
    Qwertz,
    Dvorak,
    Colemak,
}

impl LayoutFamily {
    /// The name a layout description carries: "QWERTY", "AZERTY", "QWERTZ", "Dvorak", "Colemak".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qwerty => "QWERTY",
            Self::Azerty => "AZERTY",
            Self::Qwertz => "QWERTZ",
            Self::Dvorak => "Dvorak",
            Self::Colemak => "Colemak",
        }
    }
}

/// The six keys that spell QWERTY on a US keyboard, as HID usages.
const FAMILY_KEYS: [u16; 6] = [0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C];

/// The family of a layout from its labels (as [`layout_info`] builds them): what the six keys
/// that spell QWERTY on a US keyboard type unshifted. `None` for any other arrangement,
/// including layouts in other scripts.
pub fn layout_family(labels: &HashMap<String, Vec<String>>) -> Option<LayoutFamily> {
    let mut row = String::new();
    for hid in FAMILY_KEYS {
        row.push_str(&labels.get(&hid.to_string())?.first()?.to_lowercase());
    }
    match row.as_str() {
        "qwerty" => Some(LayoutFamily::Qwerty),
        "azerty" => Some(LayoutFamily::Azerty),
        "qwertz" => Some(LayoutFamily::Qwertz),
        "',.pyf" => Some(LayoutFamily::Dvorak),
        "qwfpgj" => Some(LayoutFamily::Colemak),
        _ => None,
    }
}

/// The keys a layout description labels, as HID usages: the letters, the digits, the punctuation
/// keys of the main block, and the two keys only ISO keyboards have (0x32 beside Return, 0x64
/// beside the left Shift).
pub fn labelled_keys() -> impl Iterator<Item = u16> {
    (0x04..=0x27).chain(0x2D..=0x38).chain([0x64])
}

/// What a key shows: the character it types, or for a dead key the accent it starts.
fn label(layout: &dyn KeyLayout, vk: u16, scan: u16, shift: bool) -> Option<String> {
    layout
        .translate(vk, scan, shift, false)
        .or_else(|| layout.dead_key(vk, scan, shift))
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
}

/// Describes a layout for a client that draws its keys: each of [`labelled_keys`] that the
/// layout has, by HID usage (as a decimal string), with what it types unshifted and with Shift.
/// A key with no character for one of the two has an empty string there; a key with neither is
/// left out. Keys are read by physical position, so the labels are right for layouts that move
/// letters. A dead key is labelled with its accent.
///
/// `id` is the layout's identifier (its KLID on Windows) and `locale` its language tag. The name
/// is the layout's own ([`KeyLayout::name`]).
pub fn layout_info(layout: &dyn KeyLayout, id: &str, locale: Option<String>) -> LayoutInfoMessage {
    let mut labels = HashMap::new();
    for hid in labelled_keys() {
        let scan = keys::hid_to_scan(hid);
        let vk = layout.vk_from_scan(scan);
        if scan == 0 || vk == 0 {
            continue;
        }
        let plain = label(layout, vk, scan, false);
        let shifted = label(layout, vk, scan, true);
        if plain.is_none() && shifted.is_none() {
            continue;
        }
        labels.insert(
            hid.to_string(),
            vec![plain.unwrap_or_default(), shifted.unwrap_or_default()],
        );
    }
    LayoutInfoMessage {
        id: id.to_owned(),
        name: layout.name(),
        short: layout_family(&labels).map(|family| family.as_str().to_owned()),
        locale,
        labels,
    }
}

// ---------------------------------------------------------------------------------------------
// The Windows version

/// A Windows version as text: "10.0.26200".
pub fn format_version(major: u32, minor: u32, build: u32) -> String {
    format!("{major}.{minor}.{build}")
}

// ---------------------------------------------------------------------------------------------
// The functions (Windows only)

/// The keyboard layout of the thread that owns the focused window, described for a client: its
/// identifier (the KLID, "0000040C"), its name as Windows shows it, its language tag ("fr-FR"),
/// its family and its key labels (see [`layout_info`]).
///
/// It is the layout the [`Injector`](crate::Injector) resolves shortcuts and text on. Windows
/// keeps a layout per thread: when the focused window's thread will not say (some console
/// windows), the calling thread's layout stands in, as it does for the injector.
///
/// `None` when no window is in the foreground. It reads the registry and asks the layout about
/// every labelled key, so poll [`focused_layout_id`] and call this when that changes.
#[cfg(windows)]
pub fn focused_layout() -> Option<LayoutInfoMessage> {
    let layout = sys::layout::focused_window_layout()?;
    Some(layout_info(&layout, &layout.klid(), layout.language_tag()))
}

/// Which layout [`focused_layout`] would describe now. Cheap (three calls, no registry), for a
/// host that polls to notice the user switching layouts or moving to a window with another one.
/// `None` when no window is in the foreground.
#[cfg(windows)]
pub fn focused_layout_id() -> Option<LayoutId> {
    sys::layout::focused_window_layout().map(|layout| LayoutId(layout.handle()))
}

/// The process behind a window, looking inside the frame of a packaged app.
#[cfg(windows)]
fn window_process(window: WindowId) -> Option<(u32, Option<PathBuf>)> {
    let pid = sys::desktop::window_process(window)?;
    let exe_path = sys::desktop::process_image(pid);
    if is_frame_host(exe_path.as_deref())
        && let Some(app) = sys::desktop::hosted_app_process(window, pid)
    {
        return Some((app, sys::desktop::process_image(app)));
    }
    Some((pid, exe_path))
}

/// The app whose window is in the foreground: its process, executable, window title and window.
///
/// `None` when no window is in the foreground, which happens for a moment whenever the foreground
/// changes, and for as long as the workstation is locked or a secure desktop has the input.
///
/// The title is read without sending the window a message (`InternalGetWindowText`), so a window
/// that has stopped responding cannot stall the call.
#[cfg(windows)]
pub fn foreground_app() -> Option<ForegroundApp> {
    let window = sys::desktop::foreground_window()?;
    let (pid, exe_path) = window_process(window)?;
    Some(ForegroundApp {
        pid,
        exe_path,
        window_title: sys::desktop::window_title(window),
        window,
    })
}

/// The application windows a switcher would list, front to back (Windows' own Z order): the
/// visible top-level windows that are not cloaked, have a title and pass the taskbar rule (see
/// [`is_switcher_window`]).
///
/// What that leaves out: the shell's own desktop window ("Program Manager"), windows on other
/// virtual desktops and suspended packaged apps (both are
/// cloaked), tool windows, owned dialogs (their owner is listed), windows with no title, and
/// every window of another session or desktop. The caller's own windows are listed like any
/// other; filter by [`AppWindow::pid`] to leave them out.
///
/// A packaged (UWP) app's window belongs to a shared frame host process. For those the app's own
/// process is looked up from the window inside the frame, so `pid` and `exe_path` name the app.
/// A packaged app that is minimized keeps its own window elsewhere, and then the frame host is
/// what is reported.
///
/// Icons are not read here.
#[cfg(windows)]
pub fn running_apps() -> Vec<AppWindow> {
    let shell = sys::desktop::shell_window();
    sys::desktop::top_level_windows()
        .into_iter()
        .filter_map(|window| {
            let title = sys::desktop::window_title(window);
            let facts = WindowFacts {
                visible: sys::desktop::is_visible(window),
                cloaked: sys::desktop::is_cloaked(window),
                titled: !title.is_empty(),
                owned: sys::desktop::has_owner(window),
                shell_desktop: shell == Some(window)
                    || is_shell_desktop_class(&sys::desktop::class_name(window)),
                ex_style: sys::desktop::ex_style(window),
            };
            if !is_switcher_window(&facts) {
                return None;
            }
            let (pid, exe_path) = window_process(window)?;
            Some(AppWindow {
                window,
                pid,
                exe_path,
                title,
                minimized: sys::desktop::is_minimized(window),
            })
        })
        .collect()
}

/// Asks Windows to bring a window to the foreground. Returns whether Windows agreed.
///
/// **What it does.** If the window still exists and is visible: `ShowWindowAsync(SW_RESTORE)`
/// when it is minimized (posted, so a window that has stopped responding cannot stall the call),
/// then `SetForegroundWindow`. Nothing else. It attaches no thread's input to another's, presses
/// no key, moves no pointer and changes no system setting, so it needs no
/// [`InputLease`](crate::InputLease) and cannot leave anything held.
///
/// **Its limit.** Windows lets a process set the foreground window only in some cases: it is
/// the foreground process itself, the foreground process started it, it received the last input
/// event, no window is in the foreground, or the foreground process allowed it
/// (`AllowSetForegroundWindow`). A host running in the background is usually none of these. Then
/// Windows flashes the window's taskbar button instead and this returns false, having changed
/// nothing else. Windows also makes the same exception after an Alt key press, which is why
/// hosts send [`MASKED_ALT_TAP`](crate::keys::MASKED_ALT_TAP) first; that is injected input and
/// belongs to whoever holds the lease, so this function does not do it.
///
/// A true return means Windows accepted the request; the change of foreground follows at once
/// but is the target's to complete. Read [`foreground_app`] to confirm it. False also for a
/// window that has closed, is hidden, or belongs to another desktop.
#[cfg(windows)]
pub fn activate(window: WindowId) -> bool {
    if !sys::desktop::is_window(window) || !sys::desktop::is_visible(window) {
        return false;
    }
    if sys::desktop::is_minimized(window) {
        sys::desktop::restore(window);
    }
    sys::desktop::set_foreground(window)
}

/// Whether the interactive desktop can take input now. A poll, by documented calls only:
///
/// - the process's window station by name (`GetProcessWindowStation`,
///   `GetUserObjectInformationW`): anything but `WinSta0` has no interactive desktop;
/// - the session's connection and lock state (`WTSQuerySessionInformationW` with
///   `WTSSessionInfoEx`);
/// - the desktop that has the input, by name (`OpenInputDesktop`), against the calling thread's
///   own (`GetThreadDesktop`). A user's process may not open the secure desktop, and that
///   refusal is how it is told.
///
/// See [`SessionState`] for what each answer means and what cannot be told, and
/// [`classify_session`] for the order the readings are taken in.
#[cfg(windows)]
pub fn session_state() -> SessionState {
    let session = sys::desktop::session_info();
    classify_session(&SessionFacts {
        interactive_station: sys::desktop::station_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("WinSta0")),
        connected: session.map(|info| info.connected),
        locked: session.and_then(|info| info.locked),
        input_desktop: sys::desktop::input_desktop_name(),
        own_desktop: sys::desktop::thread_desktop_name(),
    })
}

/// The Windows version as `major.minor.build` ("10.0.26200"; Windows 11 is version 10.0 with a
/// build of 22000 or more).
///
/// Read with `RtlGetVersion`, which reports the running system's version whatever the
/// executable's manifest declares. (`GetVersionExW` and the version helper functions answer
/// 6.2 to a program whose manifest does not name Windows 8.1 or 10.) "unknown" if the call
/// fails, which it is not documented to do.
#[cfg(windows)]
pub fn os_version() -> String {
    match sys::desktop::os_version() {
        Some((major, minor, build)) => format_version(major, minor, build),
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::testing::FakeLayout;

    /// A layout whose unshifted letters sit where `rows` puts them: the three letter rows of a
    /// keyboard, left to right, as characters at the US positions `US_ROWS` names.
    fn letters(name: &'static str, rows: [&str; 3]) -> FakeLayout {
        const US_ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl;", "zxcvbnm,./"];
        let mut keys = Vec::new();
        for (row, us_row) in rows.iter().zip(US_ROWS) {
            for (typed, us) in row.chars().zip(us_row.chars()) {
                let hid = match us {
                    'a'..='z' => 0x04 + (us as u16 - 'a' as u16),
                    ';' => 0x33,
                    ',' => 0x36,
                    '.' => 0x37,
                    _ => 0x38,
                };
                // A virtual key of its own per position: letters where the layout has letters.
                let vk = if typed.is_ascii_alphabetic() {
                    typed.to_ascii_uppercase() as u16
                } else {
                    0xB0 + hid
                };
                keys.push((typed, vk, keys::hid_to_scan(hid), 0));
                if typed.is_alphabetic() {
                    keys.push((typed.to_ascii_uppercase(), vk, keys::hid_to_scan(hid), 1));
                }
            }
        }
        FakeLayout::new(name, &keys)
    }

    fn family_of(rows: [&str; 3]) -> Option<LayoutFamily> {
        let info = layout_info(&letters("test", rows), "00000409", None);
        layout_family(&info.labels)
    }

    #[test]
    fn families_from_the_top_row() {
        assert_eq!(
            family_of(["qwertyuiop", "asdfghjkl;", "zxcvbnm,./"]),
            Some(LayoutFamily::Qwerty)
        );
        assert_eq!(
            family_of(["azertyuiop", "qsdfghjklm", "wxcvbn,;:!"]),
            Some(LayoutFamily::Azerty)
        );
        assert_eq!(
            family_of(["qwertzuiop", "asdfghjklö", "yxcvbnm,.-"]),
            Some(LayoutFamily::Qwertz)
        );
        assert_eq!(
            family_of(["',.pyfgcrl", "aoeuidhtns", ";qjkxbmwvz"]),
            Some(LayoutFamily::Dvorak)
        );
        assert_eq!(
            family_of(["qwfpgjluy;", "arstdhneio", "zxcvbkm,./"]),
            Some(LayoutFamily::Colemak)
        );
        // Another script, and a layout with a key missing from the row.
        assert_eq!(family_of(["йцукенгшщз", "фывапролдж", "ячсмитьбю."]), None);
        assert_eq!(family_of(["qwert", "asdfghjkl;", "zxcvbnm,./"]), None);
    }

    #[test]
    fn family_names_are_the_wire_names() {
        let names: Vec<_> = [
            LayoutFamily::Qwerty,
            LayoutFamily::Azerty,
            LayoutFamily::Qwertz,
            LayoutFamily::Dvorak,
            LayoutFamily::Colemak,
        ]
        .map(LayoutFamily::as_str)
        .into();
        assert_eq!(names, ["QWERTY", "AZERTY", "QWERTZ", "Dvorak", "Colemak"]);
    }

    #[test]
    fn layout_info_labels_keys_by_position() {
        let info = layout_info(
            &letters("French", ["azertyuiop", "qsdfghjklm", "wxcvbn,;:!"]),
            "0000040C",
            Some("fr-FR".into()),
        );
        assert_eq!(info.id, "0000040C");
        assert_eq!(info.name, "French");
        assert_eq!(info.short.as_deref(), Some("AZERTY"));
        assert_eq!(info.locale.as_deref(), Some("fr-FR"));
        // HID 0x14 is the key that types Q on a US keyboard: A here, with its capital.
        assert_eq!(info.labels["20"], ["a", "A"]);
        // HID 0x33 (US ;) types m; HID 0x36 (US ,) types ; and has no shifted character here.
        assert_eq!(info.labels["51"], ["m", "M"]);
        assert_eq!(info.labels["54"], [";", ""]);
        // Keys the layout does not have are left out: no digits in this fake.
        assert!(!info.labels.contains_key("30"));
        assert_eq!(info.labels.len(), 30);
    }

    #[test]
    fn layout_info_of_the_us_fake() {
        let info = layout_info(&FakeLayout::us(), "00000409", None);
        assert_eq!(info.short.as_deref(), Some("QWERTY"));
        assert_eq!(info.labels["30"], ["1", "!"]);
        assert_eq!(info.labels["46"], ["=", "+"]);
        assert_eq!(info.locale, None);
    }

    /// A layout with one dead key and one key that types a control character.
    struct Odd;

    impl KeyLayout for Odd {
        fn vk_key_scan(&self, _unit: u16) -> Option<(u16, u8)> {
            None
        }
        fn scan_from_vk(&self, _vk: u16) -> u16 {
            0
        }
        fn vk_from_scan(&self, scan: u16) -> u16 {
            match scan {
                0x1A => 0xDD,
                0x1B => 0xBA,
                _ => 0,
            }
        }
        fn translate(&self, vk: u16, _scan: u16, shift: bool, _caps: bool) -> Option<String> {
            match (vk, shift) {
                (0xBA, false) => Some("\u{1b}".into()),
                (0xBA, true) => Some("$".into()),
                _ => None,
            }
        }
        fn dead_key(&self, vk: u16, _scan: u16, shift: bool) -> Option<String> {
            (vk == 0xDD).then(|| if shift { "¨" } else { "^" }.to_owned())
        }
        fn name(&self) -> String {
            "Odd".into()
        }
    }

    #[test]
    fn dead_keys_show_their_accent_and_control_characters_nothing() {
        let info = layout_info(&Odd, "0000040C", None);
        assert_eq!(info.labels["47"], ["^", "¨"]);
        assert_eq!(info.labels["48"], ["", "$"]);
        assert_eq!(info.labels.len(), 2);
        assert_eq!(info.short, None);
    }

    #[test]
    fn labelled_keys_are_the_character_keys() {
        let keys: Vec<u16> = labelled_keys().collect();
        assert_eq!(keys.len(), 49);
        assert!(keys.contains(&0x04) && keys.contains(&0x27) && keys.contains(&0x32));
        assert!(keys.contains(&0x64));
        // Not Return, Escape, Backspace, Tab, Space or Caps Lock.
        assert!((0x28..=0x2C).all(|hid| !keys.contains(&hid)) && !keys.contains(&0x39));
        assert!(keys.iter().all(|hid| keys::hid_to_scan(*hid) != 0));
    }

    fn window(ex_style: u32) -> WindowFacts {
        WindowFacts {
            visible: true,
            cloaked: false,
            titled: true,
            owned: false,
            shell_desktop: false,
            ex_style,
        }
    }

    #[test]
    fn switcher_leaves_out_the_shell_desktop() {
        assert!(is_shell_desktop_class("Progman"));
        assert!(is_shell_desktop_class("WorkerW"));
        assert!(is_shell_desktop_class("progman"));
        assert!(!is_shell_desktop_class("Notepad"));
        assert!(!is_shell_desktop_class("Shell_TrayWnd"));
        assert!(!is_shell_desktop_class(""));
        // "Program Manager": visible, titled, unowned, no style that would leave it out.
        let desktop = WindowFacts {
            shell_desktop: true,
            ..window(0)
        };
        assert!(!is_switcher_window(&desktop));
        assert!(!is_switcher_window(&WindowFacts {
            ex_style: WS_EX_APPWINDOW,
            ..desktop
        }));
    }

    #[test]
    fn switcher_lists_plain_application_windows() {
        assert!(is_switcher_window(&window(0)));
        // Other style bits do not matter (WS_EX_WINDOWEDGE, WS_EX_ACCEPTFILES).
        assert!(is_switcher_window(&window(0x0000_0110)));
    }

    #[test]
    fn switcher_leaves_out_what_the_user_cannot_see() {
        let hidden = WindowFacts {
            visible: false,
            ..window(0)
        };
        let cloaked = WindowFacts {
            cloaked: true,
            ..window(0)
        };
        let untitled = WindowFacts {
            titled: false,
            ..window(0)
        };
        for facts in [hidden, cloaked, untitled] {
            assert!(!is_switcher_window(&facts), "{facts:?}");
            // Not even when the window asks for a taskbar button.
            let forced = WindowFacts {
                ex_style: WS_EX_APPWINDOW,
                ..facts
            };
            assert!(!is_switcher_window(&forced), "{forced:?}");
        }
    }

    #[test]
    fn switcher_follows_the_taskbar_rule() {
        let owned = WindowFacts {
            owned: true,
            ..window(0)
        };
        assert!(!is_switcher_window(&owned));
        assert!(!is_switcher_window(&window(WS_EX_TOOLWINDOW)));
        assert!(!is_switcher_window(&window(WS_EX_NOACTIVATE)));
        // WS_EX_APPWINDOW overrides each of the three.
        assert!(is_switcher_window(&WindowFacts {
            ex_style: WS_EX_APPWINDOW,
            ..owned
        }));
        assert!(is_switcher_window(&window(
            WS_EX_APPWINDOW | WS_EX_TOOLWINDOW
        )));
        assert!(is_switcher_window(&window(
            WS_EX_APPWINDOW | WS_EX_NOACTIVATE
        )));
    }

    fn session(input: Option<&str>) -> SessionFacts {
        SessionFacts {
            interactive_station: true,
            connected: Some(true),
            locked: Some(false),
            input_desktop: input.map(str::to_owned),
            own_desktop: Some("Default".into()),
        }
    }

    #[test]
    fn session_is_ready_when_the_input_is_on_our_desktop() {
        assert_eq!(
            classify_session(&session(Some("Default"))),
            SessionState::Ready
        );
        assert_eq!(
            classify_session(&session(Some("default"))),
            SessionState::Ready
        );
        // Windows will not say whether it is connected or locked: the desktop decides.
        let unknown = SessionFacts {
            connected: None,
            locked: None,
            ..session(Some("Default"))
        };
        assert_eq!(classify_session(&unknown), SessionState::Ready);
        // Our own desktop's name cannot be read: applications run on "Default".
        let unnamed = SessionFacts {
            own_desktop: None,
            ..session(Some("Default"))
        };
        assert_eq!(classify_session(&unnamed), SessionState::Ready);
    }

    #[test]
    fn session_on_another_desktop_is_the_secure_desktop() {
        // A user's process may not open the secure desktop.
        assert_eq!(
            classify_session(&session(None)),
            SessionState::SecureDesktop
        );
        assert_eq!(
            classify_session(&session(Some("Winlogon"))),
            SessionState::SecureDesktop
        );
        assert_eq!(
            classify_session(&session(Some("Screen-saver"))),
            SessionState::SecureDesktop
        );
        // A process that itself runs on the desktop that has the input can inject there.
        let on_winlogon = SessionFacts {
            own_desktop: Some("Winlogon".into()),
            ..session(Some("Winlogon"))
        };
        assert_eq!(classify_session(&on_winlogon), SessionState::Ready);
    }

    #[test]
    fn session_lock_wins_over_the_desktop() {
        for input in [None, Some("Winlogon"), Some("Default")] {
            let locked = SessionFacts {
                locked: Some(true),
                ..session(input)
            };
            assert_eq!(classify_session(&locked), SessionState::Locked);
        }
    }

    #[test]
    fn session_without_a_desktop_is_detached() {
        let service = SessionFacts {
            interactive_station: false,
            ..session(Some("Default"))
        };
        assert_eq!(classify_session(&service), SessionState::Detached);
        // A disconnected remote session is also locked; detached is the more useful answer.
        let disconnected = SessionFacts {
            connected: Some(false),
            locked: Some(true),
            ..session(None)
        };
        assert_eq!(classify_session(&disconnected), SessionState::Detached);
        assert_eq!(
            classify_session(&SessionFacts::default()),
            SessionState::Detached
        );
    }

    #[test]
    fn exe_names() {
        let path = Path::new(r"C:\Program Files\Paint\paint.exe");
        assert_eq!(exe_name(path).as_deref(), Some("paint.exe"));
        assert_eq!(
            exe_name(Path::new("C:/tools/a.exe")).as_deref(),
            Some("a.exe")
        );
        assert_eq!(exe_name(Path::new("a.exe")).as_deref(), Some("a.exe"));
        assert_eq!(exe_name(Path::new(r"C:\tools\")), None);
        assert!(is_frame_host(Some(Path::new(
            r"C:\Windows\System32\applicationframehost.exe"
        ))));
        assert!(!is_frame_host(Some(path)));
        assert!(!is_frame_host(None));
        let app = AppWindow {
            window: WindowId(0x1f0a2),
            pid: 4,
            exe_path: Some(path.to_owned()),
            title: "Untitled".into(),
            minimized: false,
        };
        assert_eq!(app.exe_name().as_deref(), Some("paint.exe"));
        assert_eq!(format!("{:?}", app.window), "WindowId(0x1f0a2)");
    }

    #[test]
    fn versions() {
        assert_eq!(format_version(10, 0, 26200), "10.0.26200");
    }

    /// The functions on a real Windows computer. A test run has no say over the desktop it runs
    /// on (often none at all: a service's or an SSH session), so these check what holds
    /// everywhere and print what they read; `examples/desktop.rs` shows the real thing.
    #[cfg(windows)]
    mod on_windows {
        use super::super::*;

        #[test]
        fn os_version_is_three_numbers() {
            let version = os_version();
            println!("os_version {version}");
            let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
            assert_eq!(parts.len(), 3, "{version}");
            assert!(parts[0] >= 10 && parts[2] >= 10240, "{version}");
        }

        #[test]
        fn session_state_agrees_with_the_foreground() {
            let state = session_state();
            let foreground = foreground_app();
            println!("session_state {state:?}, foreground {foreground:?}");
            if state == SessionState::Detached {
                assert_eq!(foreground, None);
                assert!(running_apps().is_empty());
                assert_eq!(focused_layout_id(), None);
                assert_eq!(focused_layout(), None);
            }
        }

        #[test]
        fn focused_layout_matches_its_id() {
            let (id, layout) = (focused_layout_id(), focused_layout());
            println!("focused_layout_id {id:?}");
            match layout {
                Some(info) => {
                    println!(
                        "focused_layout {} {:?} {:?} {:?}, {} labels",
                        info.id,
                        info.name,
                        info.short,
                        info.locale,
                        info.labels.len()
                    );
                    assert_eq!(info.id.len(), 8, "{}", info.id);
                    assert!(info.id.chars().all(|c| c.is_ascii_hexdigit()));
                    assert!(!info.name.is_empty());
                    assert!(!info.labels.is_empty());
                }
                // No foreground window; the id can only have appeared in between.
                None => println!("focused_layout None"),
            }
        }

        #[test]
        fn running_apps_are_listable_windows() {
            let apps = running_apps();
            println!("running_apps {}", apps.len());
            for app in &apps {
                println!("  {app:?}");
                assert!(!app.title.is_empty());
                assert_ne!(app.pid, 0);
            }
        }

        #[test]
        fn activate_refuses_a_window_that_does_not_exist() {
            assert!(!activate(WindowId(0)));
            // Window handles are even; an odd value names none.
            assert!(!activate(WindowId(0x7fff_fff1)));
        }
    }
}
