//! COM and Media Foundation start-up, QPC, events, the high-resolution timer, the message-aware
//! wait, the DispatcherQueue and MMCSS.

use crate::clock::{qpc_to_ns, throttling_with};
use std::marker::PhantomData;
use std::sync::OnceLock;
use windows::System::DispatcherQueueController;
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW,
    CreateWaitableTimerExW, GetCurrentProcess, GetProcessInformation, GetSystemTimes,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetEvent, SetProcessInformation,
    SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT, DispatcherQueueOptions,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, TranslateMessage,
};
use windows::core::{PCWSTR, Result, w};

/// COM initialised on this thread until dropped. Not `Send`: COM apartments are per thread.
pub struct Apartment {
    _not_send: PhantomData<*const ()>,
}

impl Apartment {
    /// Single-threaded apartment: needed for a `DispatcherQueue` on the current thread, which
    /// `Direct3D11CaptureFramePool::Create` requires.
    pub fn sta() -> Result<Self> {
        // SAFETY: plain COM initialisation on the calling thread; balanced by `Drop`.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()? };
        Ok(Self {
            _not_send: PhantomData,
        })
    }

    pub fn mta() -> Result<Self> {
        // SAFETY: as above, multithreaded apartment.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx in the constructor, on the same thread
        // (the type is not Send).
        unsafe { CoUninitialize() };
    }
}

/// Media Foundation started until dropped (process-wide, reference counted by MF).
pub struct MediaFoundation(());

impl MediaFoundation {
    pub fn start() -> Result<Self> {
        // SAFETY: MFStartup takes plain integers; balanced by MFShutdown in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(Self(()))
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: balances the successful MFStartup above.
        let _ = unsafe { MFShutdown() };
    }
}

fn qpc_frequency() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: writes one i64 through a valid pointer; cannot fail on XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        u64::try_from(f).unwrap_or(0)
    })
}

/// Host-monotonic time in nanoseconds: QPC ticks x 10^9 / QPC frequency (the clock of
/// `SystemRelativeTime`, VIDEO-THREADING.md "Clocks").
pub fn now_ns() -> u64 {
    let mut t = 0i64;
    // SAFETY: writes one i64 through a valid pointer; cannot fail on XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    qpc_to_ns(u64::try_from(t).unwrap_or(0), qpc_frequency())
}

/// An auto-reset Win32 event.
pub struct Event(HANDLE);

// SAFETY: an event handle may be signalled and waited on from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; `set` takes `&self` and the kernel object is thread-safe.
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> Result<Self> {
        // SAFETY: creates an unnamed auto-reset event with default security; closed in Drop.
        let h = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
        Ok(Self(h))
    }
    pub fn set(&self) {
        // SAFETY: the handle is valid until Drop.
        let _ = unsafe { SetEvent(self.0) };
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: closes the handle created in `new`, once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A high-resolution waitable timer (the executor's `wake_after`).
pub struct Timer(HANDLE);

impl Timer {
    pub fn new() -> Result<Self> {
        // SAFETY: creates an unnamed high-resolution timer; closed in Drop.
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )?
        };
        Ok(Self(h))
    }

    /// Arms the timer to fire once after `delay_ns` (replacing any earlier arming).
    pub fn set_after_ns(&self, delay_ns: u64) {
        let due = -i64::try_from((delay_ns / 100).max(1)).unwrap_or(i64::MAX);
        // SAFETY: the handle is valid; `due` lives across the call; no completion routine.
        let _ = unsafe { SetWaitableTimer(self.0, &due, 0, None, None, false) };
    }

    pub fn cancel(&self) {
        // SAFETY: the handle is valid until Drop.
        let _ = unsafe { CancelWaitableTimer(self.0) };
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: closes the handle created in `new`, once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Why `wait` returned.
#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    Handle(usize),
    Input,
    Timeout,
}

/// Waits for any of `handles` or for window messages on this thread (the DispatcherQueue
/// delivers `FrameArrived` through the message queue).
pub fn wait(handles: &[HANDLE], timeout_ms: u32) -> Wake {
    // SAFETY: `handles` are live handles owned by the caller for the duration of the call.
    let r: WAIT_EVENT = unsafe {
        MsgWaitForMultipleObjectsEx(Some(handles), timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
    };
    let index = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    if index < handles.len() {
        Wake::Handle(index)
    } else if index == handles.len() {
        Wake::Input
    } else if r == WAIT_TIMEOUT {
        Wake::Timeout
    } else {
        Wake::Input
    }
}

/// Dispatches every queued message on this thread. This is where `FrameArrived` runs when the
/// frame pool was created with `Create`.
pub fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message loop on the calling thread with a valid MSG buffer.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// A DispatcherQueue on the calling thread (which must be in an STA).
pub fn dispatcher_queue_for_current_thread() -> Result<DispatcherQueueController> {
    let options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    };
    // SAFETY: plain struct argument; returns an owned WinRT object.
    unsafe { CreateDispatcherQueueController(options) }
}

/// MMCSS registration of the calling thread until dropped.
pub struct Mmcss(HANDLE);

impl Mmcss {
    /// Registers the thread with the MMCSS "Capture" task (design doc 3.5).
    pub fn capture() -> Result<Self> {
        let mut index = 0u32;
        // SAFETY: valid task name and out pointer; reverted in Drop on the same thread.
        let h = unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &mut index)? };
        Ok(Self(h))
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: reverts the registration made in `capture`.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

fn ft(f: FILETIME) -> u64 {
    (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
}

/// System-wide (idle, kernel, user) times in 100 ns units.
pub fn system_times() -> Option<(u64, u64, u64)> {
    let (mut idle, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: three valid out pointers.
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
    Some((ft(idle), ft(kernel), ft(user)))
}

/// The streams holding the 1 ms timer period, what the first hold applied, and the
/// power-throttling state it found (restored, for its timer bit only, by the last release).
struct Holders {
    count: u32,
    applied: TimerPeriodStatus,
    saved: Option<(u32, u32)>,
}

static TIMER_HOLDERS: std::sync::Mutex<Holders> = std::sync::Mutex::new(Holders {
    count: 0,
    applied: TimerPeriodStatus {
        period_1ms: false,
        throttling_opt_out: false,
    },
    saved: None,
});

/// This process's power-throttling state, `(ControlMask, StateMask)`.
fn throttling_state() -> Option<(u32, u32)> {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    // SAFETY: the pseudo-handle of this process and a valid, correctly sized, versioned state
    // that the call fills.
    unsafe {
        GetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw mut state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .ok()?;
    Some((state.ControlMask, state.StateMask))
}

fn set_throttling_state((control, state): (u32, u32)) -> bool {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: control,
        StateMask: state,
    };
    // SAFETY: the pseudo-handle of this process and a valid, correctly sized state.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .is_ok()
}

/// A hold on a 1 ms system timer period for this process, taken while a stream captures and
/// released when it stops (never held while idle). Reference-counted across streams: the
/// first hold calls `timeBeginPeriod(1)` and opts the process out of timer-resolution
/// throttling, the last release undoes both. Quick Sync's encoder MFT waits on this timer: at the
/// default 15.6 ms tick an encode takes about 15 ms, at 1 ms about 6 ms.
///
/// Only the `IGNORE_TIMER_RESOLUTION` bits of the process's power-throttling state are changed
/// (read with `GetProcessInformation`): EcoQoS or any other policy the host set stays as it is,
/// and the last release puts the timer bits back as the first hold found them.
pub struct TimerPeriod(());

/// What the holds currently apply (nothing while none is held).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerPeriodStatus {
    pub period_1ms: bool,
    pub throttling_opt_out: bool,
}

const TIMER_BIT: u32 = PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;

impl TimerPeriod {
    pub fn acquire() -> Self {
        let mut holders = TIMER_HOLDERS.lock().unwrap_or_else(|p| p.into_inner());
        if holders.count == 0 {
            // Unreadable (an OS without the query): treat it as the default, system-managed state.
            let saved = throttling_state().unwrap_or((0, 0));
            let throttling_opt_out = set_throttling_state(throttling_with(saved, TIMER_BIT, None));
            // SAFETY: no pointers; paired with timeEndPeriod on the last release.
            let period_1ms = unsafe { windows::Win32::Media::timeBeginPeriod(1) } == 0;
            holders.applied = TimerPeriodStatus {
                period_1ms,
                throttling_opt_out,
            };
            holders.saved = throttling_opt_out.then_some(saved);
        }
        holders.count += 1;
        Self(())
    }

    /// What is applied now: the first hold's result while any hold exists, else nothing.
    pub fn status() -> TimerPeriodStatus {
        let holders = TIMER_HOLDERS.lock().unwrap_or_else(|p| p.into_inner());
        if holders.count == 0 {
            TimerPeriodStatus::default()
        } else {
            holders.applied
        }
    }
}

impl Drop for TimerPeriod {
    fn drop(&mut self) {
        let mut holders = TIMER_HOLDERS.lock().unwrap_or_else(|p| p.into_inner());
        holders.count = holders.count.saturating_sub(1);
        if holders.count == 0 {
            if holders.applied.period_1ms {
                // SAFETY: ends the period begun by the first hold.
                unsafe { windows::Win32::Media::timeEndPeriod(1) };
            }
            if let Some(saved) = holders.saved.take() {
                let current = throttling_state().unwrap_or(throttling_with(saved, TIMER_BIT, None));
                set_throttling_state(throttling_with(current, TIMER_BIT, Some(saved)));
            }
            holders.applied = TimerPeriodStatus::default();
        }
    }
}
