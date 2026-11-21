//! The injector: an injection thread, and the lease that says who may post to it.

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::engine::{Backend, Engine};
use crate::event::{FeedbackSink, InjectError, InputEvent};
use crate::lease::LeaseTable;

/// What this computer can inject, found when the injector starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// A synthetic pen device exists, so strokes carry pressure, tilt and the eraser. Without
    /// one the pen moves and clicks the mouse.
    pub pen: bool,
    /// A synthetic touch device exists, so two-finger gestures can go out as touch contacts.
    pub touch: bool,
}

enum Command {
    Inject(InputEvent),
    /// Let go of everything held, then signal.
    ReleaseAll(SyncSender<()>),
}

/// The lease table and the way to the injection thread, under one lock: an event is checked and
/// posted, and a lease is revoked and its release posted, as one step each. An event from a
/// revoked lease therefore never follows that lease's release.
struct State {
    leases: LeaseTable,
    /// `None` once the injector is dropped.
    commands: Option<Sender<Command>>,
}

struct Shared(Mutex<State>);

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent if a holder panicked: every update is a single assignment.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Posts a release of everything held, if `revoked` says a lease just ended, and waits until
    /// the injection thread has done it.
    fn release(&self, state: MutexGuard<'_, State>, revoked: bool) {
        let (done, wait) = mpsc::sync_channel(1);
        let posted = revoked
            && state
                .commands
                .as_ref()
                .is_some_and(|commands| commands.send(Command::ReleaseAll(done)).is_ok());
        drop(state);
        if posted {
            // An error means the thread is gone, and with it everything it held.
            let _ = wait.recv();
        }
    }
}

/// The right to inject. One lease is live per injector at a time, whichever connection or client
/// holds it. Revoking it (or dropping it) releases every key, button and contact that is held,
/// and events posted with it afterwards fail with [`InjectError::StaleLease`].
pub struct InputLease {
    shared: Arc<Shared>,
    generation: u64,
}

impl InputLease {
    /// Whether this lease may still inject.
    pub fn is_current(&self) -> bool {
        self.shared.lock().leases.is_current(self.generation)
    }

    /// Ends the lease and releases everything held. Returns once that is done. Later calls do
    /// nothing.
    pub fn revoke(&self) {
        let mut state = self.shared.lock();
        let revoked = state.leases.revoke(self.generation);
        self.shared.release(state, revoked);
    }
}

impl Drop for InputLease {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Injects input on its own thread. [`inject`](Self::inject) posts and returns; keys that have
/// to be paced never hold up the caller, a pen stroke or a release.
pub struct Injector {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
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
        let (commands, inbox) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("input-injection".into())
            .spawn(move || {
                let engine = Engine::new(make(), Box::new(feedback));
                let backend = engine.backend();
                let capabilities = Capabilities {
                    pen: backend.pen_available(),
                    touch: backend.touch_available(),
                };
                if ready.send(capabilities).is_ok() {
                    run(engine, &inbox);
                }
            })?;
        let capabilities = started
            .recv()
            .map_err(|_| io::Error::other("the injection thread stopped while starting"))?;
        let state = State {
            leases: LeaseTable::default(),
            commands: Some(commands),
        };
        Ok(Self {
            shared: Arc::new(Shared(Mutex::new(state))),
            thread: Some(thread),
            capabilities,
        })
    }

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

    /// Posts an event to the injection thread. Fails with [`InjectError::StaleLease`] when the
    /// lease has been revoked or is another injector's.
    pub fn inject(&self, event: InputEvent, lease: &InputLease) -> Result<(), InjectError> {
        let state = self.shared.lock();
        if !Arc::ptr_eq(&self.shared, &lease.shared) || !state.leases.is_current(lease.generation) {
            return Err(InjectError::StaleLease);
        }
        state
            .commands
            .as_ref()
            .and_then(|commands| commands.send(Command::Inject(event)).ok())
            .ok_or(InjectError::Stopped)
    }

    /// Ends whichever lease is live and releases everything held: every key, mouse button, touch
    /// and pen contact. Returns once that is done. The holder's lease is stale from then on.
    pub fn revoke(&self) {
        let mut state = self.shared.lock();
        state.leases.revoke_current();
        self.shared.release(state, true);
    }

    /// Releases everything held without ending the lease, for a client that went quiet in the
    /// middle of a key or a stroke. Returns once that is done.
    pub fn release_all(&self) {
        self.shared.release(self.shared.lock(), true);
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.leases.revoke_current();
            // Closing the channel ends the thread, which releases everything on its way out.
            state.commands = None;
        }
        if let Some(thread) = self.thread.take() {
            // A panic on the injection thread has already been reported there.
            let _ = thread.join();
        }
    }
}

/// The injection thread: takes commands as they come, and wakes for the engine's own deadlines
/// (paced keys, key repeats, the touch refresh) in between.
fn run<B: Backend>(mut engine: Engine<B>, inbox: &Receiver<Command>) {
    let started = Instant::now();
    let now = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    loop {
        let command = match engine.next_deadline(now()) {
            Some(deadline) => {
                match inbox.recv_timeout(Duration::from_millis(deadline.saturating_sub(now()))) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match inbox.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            },
        };
        match command {
            Some(Command::Inject(event)) => engine.handle(event, now()),
            Some(Command::ReleaseAll(done)) => {
                engine.release_all();
                // The caller may have stopped waiting; the release happened either way.
                let _ = done.send(());
            }
            None => engine.tick(now()),
        }
    }
    engine.release_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SendOp;
    use crate::engine::testing::{Call, Calls, FakeBackend};
    use crate::event::Feedback;
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
        injector.inject(key_down(0x04), &lease).expect("current");
        lease.revoke();
        // The release has happened by the time revoke returns.
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
        assert_eq!(
            injector.inject(key_down(0x05), &lease),
            Err(InjectError::StaleLease)
        );
        lease.revoke();
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
        injector.inject(key_down(0x04), &lease).expect("current");
        drop(lease);
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
        assert!(injector.lease().is_some());
    }

    #[test]
    fn the_injector_can_revoke_and_a_stale_lease_cannot_touch_its_successor() {
        let calls = Calls::default();
        let injector = injector(&calls);
        let old = injector.lease().expect("free");
        injector.inject(key_down(0x04), &old).expect("current");
        injector.revoke();
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
        assert_eq!(
            injector.inject(key_down(0x04), &old),
            Err(InjectError::StaleLease)
        );

        let new = injector.lease().expect("free after the injector revoked");
        injector.inject(key_down(0x04), &new).expect("current");
        // The former holder going away late must not release the new holder's key.
        drop(old);
        injector
            .inject(key_down(0x05), &new)
            .expect("still current");
        new.revoke();
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
        injector.inject(key_down(0x04), &lease).expect("current");
        injector.release_all();
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
        injector.inject(key_down(0x04), &lease).expect("current");
        drop(injector);
        assert_eq!(keys(&calls), [(A, false), (A, true)]);
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
