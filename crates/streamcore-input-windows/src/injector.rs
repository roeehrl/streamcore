//! The injector: an injection thread, and the lease that says who may post to it.

use std::collections::VecDeque;
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::engine::{Backend, Engine};
use crate::event::{Feedback, FeedbackSink, InjectError, InputEvent, RevokeReason, text_units};
use crate::lease::LeaseTable;
use crate::queue::{Admission, Inbox};

/// What this computer can inject. It is read once, when the injector starts, and does not change
/// afterwards: an injector started where the devices could not be created keeps saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// A synthetic pen device exists, so strokes carry pressure, tilt and the eraser. Without
    /// one the pen moves and clicks the mouse.
    pub pen: bool,
    /// A synthetic touch device exists, so two-finger gestures can go out as touch contacts.
    pub touch: bool,
}

/// Everything the callers and the injection thread share, under one lock: an event is checked
/// against the lease and queued, and a lease is revoked, the queue emptied and a release asked
/// for, as one step each. An event from a revoked lease therefore never reaches the computer
/// after that lease's release.
#[derive(Default)]
struct State {
    leases: LeaseTable,
    inbox: Inbox,
    /// What the thread has taken and not finished, as (events, text units).
    busy: (usize, usize),
    /// Releases asked for and done, as running counts. Any number of pending requests is one
    /// release for the thread; a caller waits until `done` reaches the count it asked at.
    releases_asked: u64,
    releases_done: u64,
    /// Leases ended without their holder's doing, for the thread to tell the host about.
    notices: VecDeque<RevokeReason>,
    /// The injector was dropped: the thread finishes up and stops.
    closed: bool,
    /// The thread has stopped (or died), and with it everything it held.
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Wakes the injection thread: an event, a release request or the end.
    work: Condvar,
    /// Wakes callers waiting for a release.
    released: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent if a holder panicked: nobody calls out with the lock held.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Discards what is queued and asks the thread to release everything held. Returns the
    /// count [`wait_released`](Self::wait_released) waits for. Never waits itself.
    fn ask_release(&self, state: &mut State) -> u64 {
        state.inbox.clear();
        state.releases_asked += 1;
        self.work.notify_one();
        state.releases_asked
    }

    /// Notes that a lease was ended by someone other than its holder, for the thread to report
    /// once the release is done.
    fn notice(&self, state: &mut State, reason: RevokeReason) {
        // Only a blocked injection thread lets these pile up, and then nobody is listening:
        // the oldest gives way.
        if state.notices.len() == MAX_PENDING_NOTICES {
            state.notices.pop_front();
        }
        state.notices.push_back(reason);
        self.work.notify_one();
    }

    /// Waits until the release asked for at `asked` is done. False when `timeout` passes first,
    /// or the thread is gone without having done it.
    fn wait_released(
        &self,
        mut state: MutexGuard<'_, State>,
        asked: u64,
        timeout: Duration,
    ) -> bool {
        let started = Instant::now();
        while state.releases_done < asked && !state.stopped {
            let Some(left) = timeout.checked_sub(started.elapsed()) else {
                break;
            };
            state = self
                .released
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.releases_done >= asked
    }
}

/// Most lease-ended notices waiting for the injection thread.
const MAX_PENDING_NOTICES: usize = 16;

/// The right to inject. One lease is live per injector at a time, whichever connection or client
/// holds it. Revoking it (or dropping it) discards what it still has queued and releases every
/// key, button and contact that is held, and events posted with it afterwards fail with
/// [`InjectError::StaleLease`].
pub struct InputLease {
    shared: Arc<Shared>,
    generation: u64,
}

impl InputLease {
    /// Whether this lease may still inject.
    pub fn is_current(&self) -> bool {
        self.shared.lock().leases.is_current(self.generation)
    }

    /// Ends the lease. It is stale as soon as this returns: nothing more of it is injected, and
    /// the injection thread releases everything held as its next step. The call does not wait
    /// for that. Later calls do nothing.
    pub fn revoke(&self) {
        let mut state = self.shared.lock();
        if state.leases.revoke(self.generation) {
            self.shared.ask_release(&mut state);
        }
    }

    /// Ends the lease as [`revoke`](Self::revoke) does, then waits up to `timeout` for the
    /// release to be done. True when everything held has been released; false when the time ran
    /// out or the injection thread has stopped (the lease is ended either way). On a lease that
    /// has ended already it waits for the release that followed.
    pub fn revoke_and_wait(&self, timeout: Duration) -> bool {
        let mut state = self.shared.lock();
        if state.leases.revoke(self.generation) {
            self.shared.ask_release(&mut state);
        }
        let asked = state.releases_asked;
        self.shared.wait_released(state, asked, timeout)
    }
}

impl Drop for InputLease {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Injects input on its own thread. [`inject`](Self::inject) queues and returns; it never waits,
/// and keys that have to be paced never hold up the caller, a pen stroke or a release. The queue
/// is bounded (see [`MAX_PENDING_OPERATIONS`](crate::MAX_PENDING_OPERATIONS) and
/// [`MAX_PENDING_TEXT_UNITS`](crate::MAX_PENDING_TEXT_UNITS)); [`InjectError`] says what a full
/// queue does.
pub struct Injector {
    shared: Arc<Shared>,
    capabilities: Capabilities,
}

impl Injector {
    /// Starts an injector for this computer. `feedback` hears what the host may need to act on.
    #[cfg(windows)]
    pub fn new(feedback: impl FeedbackSink) -> io::Result<Self> {
        Self::with_backend(crate::win::Win32Backend::new, feedback)
    }

    /// Starts an injector over another [`Backend`], which `make` creates on the injection
    /// thread. For tests, and for hosts that inject some other way.
    pub fn with_backend<B: Backend + 'static>(
        make: impl FnOnce() -> B + Send + 'static,
        feedback: impl FeedbackSink,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let (ready, started) = mpsc::sync_channel(1);
        let for_thread = shared.clone();
        // The thread is not joined: dropping the injector must not wait for it. It ends by itself
        // once the injector is gone, releasing everything on its way out.
        std::thread::Builder::new()
            .name("input-injection".into())
            .spawn(move || {
                // Whatever ends the thread, nobody is left waiting for it.
                let _stopped = Stopped(&for_thread);
                let engine = Engine::new(make(), Box::new(feedback));
                let backend = engine.backend();
                let capabilities = Capabilities {
                    pen: backend.pen_available(),
                    touch: backend.touch_available(),
                };
                if ready.send(capabilities).is_ok() {
                    run(engine, &for_thread);
                }
            })?;
        let capabilities = started
            .recv()
            .map_err(|_| io::Error::other("the injection thread stopped while starting"))?;
        Ok(Self {
            shared,
            capabilities,
        })
    }

    /// What this computer can inject, as found when the injector started.
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Takes the lease, or `None` while another one is live.
    pub fn lease(&self) -> Option<InputLease> {
        let generation = self.shared.lock().leases.acquire()?;
        Some(InputLease {
            shared: self.shared.clone(),
            generation,
        })
    }

    /// Takes the lease whoever holds it: ends the live lease, if any, and grants a new one in
    /// one step, so no other caller can take the lease in between. The former holder's queued
    /// events are discarded, everything it held is released before anything of the new lease is
    /// injected, and it hears [`Feedback::LeaseRevoked`](crate::Feedback::LeaseRevoked) with
    /// [`RevokeReason::TakenOver`]. The call does not wait for the release.
    pub fn take_over(&self) -> InputLease {
        let mut state = self.shared.lock();
        let (generation, ended) = state.leases.take_over();
        if ended {
            self.shared.ask_release(&mut state);
            self.shared.notice(&mut state, RevokeReason::TakenOver);
        }
        InputLease {
            shared: self.shared.clone(),
            generation,
        }
    }

    /// Queues an event for the injection thread. It never waits.
    ///
    /// An event with a number that is not finite, or a target rectangle without pixels, is
    /// refused with [`InjectError::InvalidEvent`]: nothing is queued and the lease is untouched.
    /// Points outside their target are not refused; they are clamped into it.
    ///
    /// Fails with [`InjectError::StaleLease`] when the lease has been revoked or is another
    /// injector's. When the queue is full a pointer move is dropped ([`InjectError::Dropped`]);
    /// any other event that does not fit revokes the lease and releases everything held
    /// ([`InjectError::Overflow`]), so a key-up or a button-up is never lost on its own.
    pub fn inject(&self, event: InputEvent, lease: &InputLease) -> Result<(), InjectError> {
        let mut state = self.shared.lock();
        if !Arc::ptr_eq(&self.shared, &lease.shared) || !state.leases.is_current(lease.generation) {
            return Err(InjectError::StaleLease);
        }
        if state.stopped {
            return Err(InjectError::Stopped);
        }
        if !event.is_valid() {
            return Err(InjectError::InvalidEvent);
        }
        let busy = state.busy;
        match state.inbox.admit(event, busy) {
            Admission::Queued => {
                self.shared.work.notify_one();
                Ok(())
            }
            Admission::Merged => Ok(()),
            Admission::Dropped => Err(InjectError::Dropped),
            Admission::Overflow => {
                // As for the other platform's adapter (docs/INPUT-OWNERSHIP.md): the lease is
                // invalid from here on, queued work is discarded and held input is released.
                // The caller is not made to wait for the release.
                state.leases.revoke(lease.generation);
                self.shared.ask_release(&mut state);
                self.shared.notice(&mut state, RevokeReason::Overflow);
                Err(InjectError::Overflow)
            }
        }
    }

    /// Ends whichever lease is live. The holder's lease is stale as soon as this returns: what
    /// it had queued is discarded, and the injection thread releases everything held (every key,
    /// mouse button, touch and pen contact) as its next step. The call does not wait for that.
    /// The holder hears [`Feedback::LeaseRevoked`](crate::Feedback::LeaseRevoked) with
    /// [`RevokeReason::Revoked`].
    pub fn revoke(&self) {
        drop(self.end_lease());
    }

    /// Ends the live lease as [`revoke`](Self::revoke) does, then waits up to `timeout` for the
    /// release to be done. True when everything held has been released; false when the time ran
    /// out or the injection thread has stopped (the lease is ended either way).
    pub fn revoke_and_wait(&self, timeout: Duration) -> bool {
        let (state, asked) = self.end_lease();
        self.shared.wait_released(state, asked, timeout)
    }

    fn end_lease(&self) -> (MutexGuard<'_, State>, u64) {
        let mut state = self.shared.lock();
        if state.leases.revoke_current() {
            self.shared.notice(&mut state, RevokeReason::Revoked);
        }
        // Asked for even when no lease is live: the host wants everything let go of.
        let asked = self.shared.ask_release(&mut state);
        (state, asked)
    }

    /// Discards what is queued and has the injection thread release everything held, without
    /// ending the lease: for a client that went quiet in the middle of a key or a stroke. The
    /// call does not wait for the release.
    pub fn release_all(&self) {
        let mut state = self.shared.lock();
        self.shared.ask_release(&mut state);
    }

    /// As [`release_all`](Self::release_all), then waits up to `timeout` for the release to be
    /// done. True when it was; false when the time ran out or the injection thread has stopped.
    pub fn release_all_and_wait(&self, timeout: Duration) -> bool {
        let mut state = self.shared.lock();
        let asked = self.shared.ask_release(&mut state);
        self.shared.wait_released(state, asked, timeout)
    }
}

impl Drop for Injector {
    /// Ends the live lease and tells the injection thread to stop. The thread releases
    /// everything held on its way out; the drop does not wait for it. A host about to exit calls
    /// [`revoke_and_wait`](Self::revoke_and_wait) first, or the process may end before the
    /// release.
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.leases.revoke_current();
        state.inbox.clear();
        state.notices.clear();
        state.closed = true;
        self.shared.work.notify_one();
    }
}

/// Marks the injection thread as stopped when it ends, however it ends, and wakes whoever waits
/// for a release: what the thread held went with it.
struct Stopped<'a>(&'a Shared);

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        self.0.lock().stopped = true;
        self.0.released.notify_all();
    }
}

/// The injection thread: releases when asked, takes events as they come, and wakes for the
/// engine's own deadlines (paced keys, key repeats, the touch refresh) in between. The lock is
/// never held while the engine works.
fn run<B: Backend>(mut engine: Engine<B>, shared: &Shared) {
    let started = Instant::now();
    let now = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut state = shared.lock();
    loop {
        state.busy = engine.backlog();
        if state.releases_done < state.releases_asked {
            let asked = state.releases_asked;
            drop(state);
            engine.release_all();
            state = shared.lock();
            state.releases_done = asked;
            shared.released.notify_all();
        } else if let Some(reason) = state.notices.pop_front() {
            // After the release above, so the host hears of a lease's end once nothing of it is
            // held any more.
            drop(state);
            engine.report(Feedback::LeaseRevoked(reason));
            state = shared.lock();
        } else if let Some(event) = state.inbox.take() {
            // In the thread's hands now: it counts as busy until the engine has it.
            state.busy.0 += 1;
            if let InputEvent::Text(message) = &event {
                state.busy.1 += text_units(message);
            }
            drop(state);
            engine.handle(event, now());
            state = shared.lock();
        } else if state.closed {
            break;
        } else {
            state = match engine.next_deadline(now()) {
                Some(deadline) => {
                    let wait = Duration::from_millis(deadline.saturating_sub(now()));
                    let (state, _) = shared
                        .work
                        .wait_timeout(state, wait)
                        .unwrap_or_else(PoisonError::into_inner);
                    state
                }
                None => shared
                    .work
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
            drop(state);
            engine.tick(now());
            state = shared.lock();
        }
    }
    let asked = state.releases_asked;
    drop(state);
    engine.release_all();
    // Whoever still waits for a release has it now.
    shared.lock().releases_done = asked;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SendOp;
    use crate::engine::testing::{Call, Calls, FakeBackend};
    use streamcore_input::{KeyPacket, KeyPhase, KeyShortcutMessage, KeySpec, TextMessage};

    fn injector(calls: &Calls) -> Injector {
        let calls = calls.clone();
        Injector::with_backend(move || FakeBackend::new(calls), |_| {}).expect("starts")
    }

    fn key_down(hid_usage: u16) -> InputEvent {
        InputEvent::Key(KeyPacket {
            is_down: true,
            hid_usage,
            modifiers: 0,
        })
    }

    /// The keys sent so far, as (virtual key, is key-up).
    fn keys(calls: &Calls) -> Vec<(u16, bool)> {
        let calls = calls.lock().expect("calls");
        calls
            .iter()
            .flat_map(|call| match call {
                Call::Send(ops) => ops.clone(),
                _ => Vec::new(),
            })
            .filter_map(|op| match op {
                SendOp::Key { vk, up, .. } => Some((vk, up)),
                _ => None,
            })
            .collect()
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Holds a key down and waits until the injection thread has pressed it.
    fn hold(injector: &Injector, lease: &InputLease, calls: &Calls, hid_usage: u16) {
        let before = keys(calls).len();
        injector
            .inject(key_down(hid_usage), lease)
            .expect("current");
        wait_for("the key to go down", || keys(calls).len() > before);
    }

    /// Long enough for the injection thread to do anything it is asked.
    const WAIT: Duration = Duration::from_secs(10);
    const A: u16 = 0x41;
    const B: u16 = 0x42;

    #[test]
    fn the_injector_and_its_lease_can_be_shared_between_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<Injector>();
        shared::<InputLease>();
    }

    #[test]
    fn the_lease_is_exclusive() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("nothing holds the lease");
        assert!(lease.is_current());
        assert!(injector.lease().is_none(), "one lease at a time");
        assert_eq!(injector.inject(key_down(0x04), &lease), Ok(()));
        lease.revoke();
        assert!(!lease.is_current());
        assert!(injector.lease().is_some(), "free again after a revoke");
    }

    #[test]
    fn a_revoked_lease_fails_cleanly_and_has_released_everything() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        hold(&injector, &lease, &calls, 0x04);
        assert!(lease.revoke_and_wait(WAIT));
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
        assert_eq!(
            injector.inject(key_down(0x05), &lease),
            Err(InjectError::StaleLease)
        );
        assert!(
            lease.revoke_and_wait(WAIT),
            "the release that followed is done"
        );
        assert_eq!(
            keys(&calls).len(),
            2,
            "nothing was injected or released twice"
        );
    }

    #[test]
    fn dropping_the_lease_releases_too() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        hold(&injector, &lease, &calls, 0x04);
        drop(lease);
        wait_for("the release", || keys(&calls) == [(A, false), (A, true)]);
        assert!(injector.lease().is_some());
    }

    #[test]
    fn the_injector_can_revoke_and_a_stale_lease_cannot_touch_its_successor() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let old = injector.lease().expect("free");
        hold(&injector, &old, &calls, 0x04);
        assert!(injector.revoke_and_wait(WAIT));
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
        assert_eq!(
            injector.inject(key_down(0x04), &old),
            Err(InjectError::StaleLease)
        );

        let new = injector.lease().expect("free after the injector revoked");
        hold(&injector, &new, &calls, 0x04);
        // The former holder going away late must not release the new holder's key.
        drop(old);
        hold(&injector, &new, &calls, 0x05);
        assert!(new.revoke_and_wait(WAIT));
        assert_eq!(
            keys(&calls)[2..],
            [(A, false), (B, false), (A, true), (B, true)]
        );
    }

    #[test]
    fn release_all_keeps_the_lease() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        hold(&injector, &lease, &calls, 0x04);
        assert!(injector.release_all_and_wait(WAIT));
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
        assert!(lease.is_current());
        assert_eq!(injector.inject(key_down(0x05), &lease), Ok(()));
    }

    #[test]
    fn a_lease_is_good_for_its_own_injector_only() {
        let (first, second) = (Calls::default(), Calls::default());
        let (one, two) = (injector(&first), injector(&second));
        let lease = one.lease().expect("free");
        assert_eq!(
            two.inject(key_down(0x04), &lease),
            Err(InjectError::StaleLease)
        );
        let _held = two.lease().expect("the other injector's lease is its own");
        assert_eq!(
            two.inject(key_down(0x04), &lease),
            Err(InjectError::StaleLease)
        );
    }

    #[test]
    fn dropping_the_injector_releases_everything_and_outdates_the_lease() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        hold(&injector, &lease, &calls, 0x04);
        drop(injector);
        wait_for("the release", || keys(&calls) == [(A, false), (A, true)]);
        assert!(!lease.is_current());
        lease.revoke();
    }

    #[test]
    fn paced_text_is_typed_by_the_thread_without_holding_up_the_caller() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        let text = TextMessage {
            text: "ab".into(),
            replace_before: 0,
        };
        injector
            .inject(InputEvent::Text(text), &lease)
            .expect("current");
        wait_for("both letters", || keys(&calls).len() == 4);
        assert_eq!(keys(&calls), [(A, false), (A, true), (B, false), (B, true)]);
    }

    fn text(units: usize) -> InputEvent {
        InputEvent::Text(TextMessage {
            text: "a".repeat(units),
            replace_before: 0,
        })
    }

    fn hover() -> InputEvent {
        use streamcore_input::{PencilPhase, PencilSample, PointerKind, StrokeButton};
        InputEvent::Pen(crate::event::PenEvent {
            sample: PencilSample {
                phase: PencilPhase::Hover,
                kind: PointerKind::Pen,
                button: StrokeButton::Left,
                x: 0.5,
                y: 0.5,
                force: 0.0,
                altitude: 0.0,
                azimuth: 0.0,
                roll: 0.0,
                hover_z: 0.0,
                timestamp: 0.0,
            },
            target: crate::geometry::TargetRect::new(0, 0, 100, 100),
            right_button_is_barrel: false,
        })
    }

    #[test]
    fn what_is_still_queued_at_a_revoke_is_never_injected() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        // The text takes seconds to type; the key waits behind it.
        injector.inject(text(200), &lease).expect("current");
        injector.inject(key_down(0x05), &lease).expect("current");
        wait_for("typing to start", || !keys(&calls).is_empty());
        assert!(lease.revoke_and_wait(WAIT));
        let typed = keys(&calls);
        assert!(typed.len() < 400, "the text was cut off");
        assert_eq!(typed.last(), Some(&(A, true)), "the key in mid-tap came up");
        assert!(!typed.contains(&(B, false)), "the queued key was discarded");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(keys(&calls), typed, "nothing follows the release");
    }

    #[test]
    fn a_full_queue_drops_moves_and_overflows_on_a_key() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        // One text being typed, and key events waiting behind it up to the limit.
        injector.inject(text(400), &lease).expect("current");
        for _ in 1..crate::MAX_PENDING_OPERATIONS {
            injector.inject(key_down(0x05), &lease).expect("room");
        }
        // A hover is dropped and costs nothing else.
        assert_eq!(injector.inject(hover(), &lease), Err(InjectError::Dropped));
        assert!(lease.is_current());
        // A key-up may not be dropped: the lease goes, and with it everything held and queued.
        let key_up = InputEvent::Key(KeyPacket {
            is_down: false,
            hid_usage: 0x05,
            modifiers: 0,
        });
        assert_eq!(
            injector.inject(key_up.clone(), &lease),
            Err(InjectError::Overflow)
        );
        assert!(!lease.is_current());
        assert_eq!(
            injector.inject(key_up, &lease),
            Err(InjectError::StaleLease)
        );
        // The overflow did not wait for the release; a release that does wait comes after it.
        assert!(injector.release_all_and_wait(WAIT));
        let typed = keys(&calls);
        assert!(typed.is_empty() || typed.last() == Some(&(A, true)));
        assert!(
            !typed.contains(&(B, false)),
            "the waiting keys were discarded"
        );
        // The next holder starts clean.
        let next = injector.lease().expect("free after the overflow");
        hold(&injector, &next, &calls, 0x05);
        assert_eq!(keys(&calls).len(), typed.len() + 1);
    }

    #[test]
    fn too_much_text_overflows_instead_of_being_cut_short() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        let too_long = text(crate::MAX_PENDING_TEXT_UNITS + 1);
        assert_eq!(
            injector.inject(too_long, &lease),
            Err(InjectError::Overflow)
        );
        assert!(!lease.is_current());
        assert!(injector.release_all_and_wait(WAIT));
        assert!(keys(&calls).is_empty(), "none of it was typed");
    }

    #[test]
    fn a_dead_injection_thread_fails_cleanly() {
        struct Dies;
        impl FeedbackSink for Dies {
            fn feedback(&self, _: Feedback) {
                panic!("the sink failed (expected in this test)");
            }
        }
        let calls = Calls::default();
        let injector =
            Injector::with_backend(move || FakeBackend::new(calls), Dies).expect("starts");
        let lease = injector.lease().expect("free");
        let shortcut = KeyShortcutMessage {
            key: KeySpec::char("é"),
            modifiers: 0,
            phase: KeyPhase::Tap,
            auto_repeat: false,
        };
        injector
            .inject(InputEvent::KeyShortcut(shortcut), &lease)
            .expect("queued");
        wait_for("the thread to die", || {
            injector.inject(key_down(0x04), &lease) == Err(InjectError::Stopped)
        });
        // Nobody waits for a thread that is gone.
        assert!(!lease.revoke_and_wait(WAIT));
        assert!(!injector.revoke_and_wait(WAIT));
        assert!(!injector.release_all_and_wait(WAIT));
    }

    fn listening_injector(calls: &Calls) -> (Injector, mpsc::Receiver<Feedback>) {
        let calls = calls.clone();
        let (heard, feedback) = mpsc::channel();
        let injector = Injector::with_backend(
            move || FakeBackend::new(calls),
            move |feedback| {
                let _ = heard.send(feedback);
            },
        )
        .expect("starts");
        (injector, feedback)
    }

    #[test]
    fn an_event_with_a_number_that_is_not_finite_is_refused_and_costs_nothing() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let lease = injector.lease().expect("free");
        let mut bad_hover = hover();
        if let InputEvent::Pen(pen) = &mut bad_hover {
            pen.sample.x = f32::NAN;
        }
        let mut no_target = hover();
        if let InputEvent::Pen(pen) = &mut no_target {
            pen.target = crate::geometry::TargetRect::new(0, 0, 0, 10);
        }
        let bad_scroll = InputEvent::Scroll(crate::event::ScrollEvent {
            dx: f64::INFINITY,
            dy: 0.0,
            target: crate::geometry::TargetRect::new(0, 0, 10, 10),
        });
        for event in [bad_hover, no_target, bad_scroll] {
            assert_eq!(
                injector.inject(event, &lease),
                Err(InjectError::InvalidEvent)
            );
        }
        // The lease is untouched, and nothing reached the computer.
        assert!(lease.is_current());
        assert!(injector.release_all_and_wait(WAIT));
        assert!(calls.lock().expect("calls").is_empty());
        assert_eq!(injector.inject(hover(), &lease), Ok(()));
        // A stale lease is said first: a refused holder learns nothing about its event.
        lease.revoke();
        let mut bad = hover();
        if let InputEvent::Pen(pen) = &mut bad {
            pen.sample.force = f32::NAN;
        }
        assert_eq!(injector.inject(bad, &lease), Err(InjectError::StaleLease));
    }

    #[test]
    fn revoking_and_dropping_never_wait_for_a_blocked_injection_thread() {
        let calls = Calls::default();
        let (open, gate) = mpsc::channel::<()>();
        let for_backend = calls.clone();
        let injector = Injector::with_backend(
            move || {
                let mut backend = FakeBackend::new(for_backend);
                backend.gate = Some(gate);
                backend
            },
            |_| {},
        )
        .expect("starts");
        let lease = injector.lease().expect("free");
        // The key goes down, and the thread stays inside that call.
        hold(&injector, &lease, &calls, 0x04);
        let started = Instant::now();
        lease.revoke();
        assert!(!lease.is_current(), "the lease is stale at once");
        injector.revoke();
        injector.release_all();
        let next = injector.take_over();
        assert_eq!(injector.inject(key_down(0x05), &next), Ok(()));
        // The waiting forms give up at their timeout and say so.
        let short = Duration::from_millis(30);
        assert!(!next.revoke_and_wait(short));
        assert!(!injector.revoke_and_wait(short));
        assert!(!injector.release_all_and_wait(short));
        drop(next);
        drop(lease);
        drop(injector);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "something waited for the thread"
        );
        assert_eq!(
            keys(&calls),
            [(A, false)],
            "the thread is still inside its call"
        );
        // Let go of the thread: it releases the key on its way out, though nobody waits.
        drop(open);
        wait_for("the release", || keys(&calls) == [(A, false), (A, true)]);
    }

    #[test]
    fn a_take_over_leaves_no_gap_for_another_caller() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut held = injector.take_over();
        std::thread::scope(|scope| {
            // A competitor grabs the lease the moment it is free.
            let competitor = scope.spawn(|| {
                let mut wins = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(lease) = injector.lease() {
                        wins += 1;
                        lease.revoke();
                    }
                }
                wins
            });
            for _ in 0..20_000 {
                let next = injector.take_over();
                assert!(next.is_current(), "somebody else got in between");
                assert!(!held.is_current());
                held = next;
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(competitor.join().expect("competitor"), 0);
        });
        // The lease taken over works like any other.
        hold(&injector, &held, &calls, 0x04);
        assert!(held.revoke_and_wait(WAIT));
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
    }

    #[test]
    fn a_take_over_releases_the_former_holders_keys_before_the_new_ones() {
        let calls = Calls::default();
        let (injector, feedback) = listening_injector(&calls);
        let old = injector.lease().expect("free");
        hold(&injector, &old, &calls, 0x04);
        let new = injector.take_over();
        assert_eq!(
            injector.inject(key_down(0x04), &old),
            Err(InjectError::StaleLease)
        );
        hold(&injector, &new, &calls, 0x05);
        assert_eq!(keys(&calls), [(A, false), (A, true), (B, false)]);
        assert_eq!(
            feedback.recv_timeout(WAIT),
            Ok(Feedback::LeaseRevoked(RevokeReason::TakenOver))
        );
        // The former holder letting go late changes nothing for the new one.
        drop(old);
        assert!(new.is_current());
    }

    #[test]
    fn the_holder_hears_when_its_lease_is_ended_for_it() {
        let calls = Calls::default();
        let (injector, feedback) = listening_injector(&calls);
        let heard = || feedback.recv_timeout(WAIT);
        let quiet = || feedback.recv_timeout(Duration::from_millis(100)).is_err();

        // By the host.
        let lease = injector.lease().expect("free");
        assert!(injector.revoke_and_wait(WAIT));
        assert_eq!(heard(), Ok(Feedback::LeaseRevoked(RevokeReason::Revoked)));
        // Once per lease: revoking again, with no lease live, says nothing.
        assert!(injector.revoke_and_wait(WAIT));
        drop(lease);
        assert!(quiet());

        // By an overflow.
        let lease = injector.lease().expect("free");
        let too_long = text(crate::MAX_PENDING_TEXT_UNITS + 1);
        assert_eq!(
            injector.inject(too_long, &lease),
            Err(InjectError::Overflow)
        );
        assert_eq!(heard(), Ok(Feedback::LeaseRevoked(RevokeReason::Overflow)));
        assert_eq!(
            injector.inject(text(1), &lease),
            Err(InjectError::StaleLease)
        );
        assert!(quiet());

        // By a take-over; taking over a lease that is not live any more says nothing.
        let lease = injector.take_over();
        assert!(quiet());
        let next = injector.take_over();
        assert_eq!(heard(), Ok(Feedback::LeaseRevoked(RevokeReason::TakenOver)));
        drop(lease);

        // Not for the holder's own doing.
        assert!(next.revoke_and_wait(WAIT));
        let own = injector.lease().expect("free");
        drop(own);
        assert!(injector.release_all_and_wait(WAIT));
        assert!(quiet());
    }

    #[test]
    fn feedback_and_capabilities_reach_the_host() {
        let calls = Calls::default();
        let (heard, feedback) = mpsc::channel();
        let injector = Injector::with_backend(
            move || {
                let mut backend = FakeBackend::new(calls);
                backend.touch = false;
                backend
            },
            move |feedback| {
                let _ = heard.send(feedback);
            },
        )
        .expect("starts");
        assert_eq!(
            injector.capabilities(),
            Capabilities {
                pen: true,
                touch: false
            }
        );
        let lease = injector.lease().expect("free");
        let shortcut = KeyShortcutMessage {
            key: KeySpec::char("é"),
            modifiers: 0,
            phase: KeyPhase::Tap,
            auto_repeat: false,
        };
        injector
            .inject(InputEvent::KeyShortcut(shortcut), &lease)
            .expect("current");
        let heard = feedback
            .recv_timeout(Duration::from_secs(10))
            .expect("feedback");
        assert!(matches!(heard, Feedback::KeyUnavailable(_)));
    }
}
