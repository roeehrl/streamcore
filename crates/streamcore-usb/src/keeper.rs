//! Keeps pipes open to cabled devices (STREAMCORE-VIDEO 8.8): one per attached
//! device and service, opened when the device appears and handed to a listener,
//! which holds it idle until the device dials. When a pipe ends, another is
//! opened after a pause, for as long as the device stays attached.
//!
//! Ends are either normal (the pipe was used, idled out, closed cleanly, found
//! no room, or the device has nothing listening yet) or abnormal (the provider
//! or the handshake failed). Abnormal ends are counted per attach, and a pipe
//! that carried a session clears the count: after a few in a row with none
//! used, only a replug opens pipes to that device again. A device whose certificate
//! the listener does not trust gets no more pipes until it is replugged.

use crate::{UsbDevice, UsbError, UsbEvent, UsbPipeProvider, UsbService};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use streamcore_transport::{
    channel::{ChannelError, SecureListener, UsbPipe},
    transport::{PipeIo, PipeOutcome},
};
use tokio::task::JoinHandle;

/// Resolves to how an offered pipe ended.
pub type PendingOutcome = Pin<Box<dyn Future<Output = PipeOutcome> + Send>>;

/// Why a sink did not take a pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferError {
    /// No room now: offer another later.
    Busy,
    /// The sink has stopped for good.
    Closed,
    /// The sink refused the pipe for a reason that will not pass by itself.
    Failed,
}

/// Where a kept pipe goes: a listener that runs the pinned handshake on it.
pub trait PipeSink: Send + Sync {
    fn offer(&self, pipe: PipeIo, device: &UsbDevice) -> Result<PendingOutcome, OfferError>;
}

impl PipeSink for SecureListener {
    fn offer(&self, pipe: PipeIo, device: &UsbDevice) -> Result<PendingOutcome, OfferError> {
        let handle = self
            .accept_stream(UsbPipe {
                io: pipe,
                medium: device.medium,
                device: device.label.clone(),
            })
            .map_err(|error| match error {
                ChannelError::Closed => OfferError::Closed,
                ChannelError::Busy => OfferError::Busy,
                _ => OfferError::Failed,
            })?;
        Ok(Box::pin(handle.outcome()))
    }
}

/// A pairing window takes pipes while it is open: the same exchange as on its
/// socket. Once it has closed it takes none, so no pairing pipe outlives it.
impl PipeSink for streamcore_transport::pairing_transport::PairingService {
    fn offer(&self, pipe: PipeIo, _device: &UsbDevice) -> Result<PendingOutcome, OfferError> {
        use streamcore_transport::pairing_transport::PairingNetworkError;
        let handle = self.accept_stream(pipe).map_err(|error| match error {
            PairingNetworkError::Closed => OfferError::Closed,
            PairingNetworkError::Unavailable => OfferError::Busy,
            _ => OfferError::Failed,
        })?;
        Ok(Box::pin(handle.outcome()))
    }
}

/// When pipes are reopened and given up.
#[derive(Clone, Copy, Debug)]
pub struct KeeperPolicy {
    /// The pause before another pipe is opened after one ended.
    pub reopen_after: Duration,
    /// Abnormal ends, with no pipe used in between, before only a replug
    /// reopens.
    pub max_abnormal: u32,
    /// The pause after the listener had no room for a pipe.
    pub crowded_pause: Duration,
}

impl Default for KeeperPolicy {
    fn default() -> Self {
        Self {
            reopen_after: Duration::from_secs(2),
            max_abnormal: 3,
            crowded_pause: Duration::from_secs(10),
        }
    }
}

/// What one pipe's end means for the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    /// The pipe carried a session: the device works, so earlier abnormal ends
    /// are forgotten. Open another after the pause.
    Used,
    /// Open another after the pause.
    Reopen,
    /// The listener had no room: open another after a longer pause, so a
    /// device that keeps finding no room does not ask every two seconds.
    Crowded,
    /// Count it, then open another after the pause unless that was too many.
    Abnormal,
    /// No more pipes to this device and service until it is replugged.
    Stop,
}

fn after_outcome(outcome: PipeOutcome) -> Next {
    match outcome {
        PipeOutcome::Accepted => Next::Used,
        PipeOutcome::IdleTimeout | PipeOutcome::ClosedIdle => Next::Reopen,
        PipeOutcome::Refused => Next::Crowded,
        PipeOutcome::HandshakeFailed => Next::Abnormal,
        PipeOutcome::Untrusted => Next::Stop,
    }
}

fn after_open_error(error: &UsbError) -> Next {
    match error {
        // Nothing listens on the device yet (its app is not running), or the
        // device just left: not a fault, and the next try may find it.
        UsbError::Refused(_) => Next::Reopen,
        UsbError::ServiceMissing | UsbError::Protocol | UsbError::Timeout | UsbError::Io(_) => {
            Next::Abnormal
        }
    }
}

/// Keeps one device's pipe to one service until the task is aborted (the
/// device detached) or it gives up.
async fn keep_one(
    provider: Arc<dyn UsbPipeProvider>,
    sink: Arc<dyn PipeSink>,
    device: UsbDevice,
    service: UsbService,
    policy: KeeperPolicy,
) {
    let mut abnormal = 0;
    loop {
        let next = match provider.open(&device, service).await {
            Err(error) => after_open_error(&error),
            Ok(pipe) => match sink.offer(pipe, &device) {
                Ok(outcome) => after_outcome(outcome.await),
                Err(OfferError::Busy) => Next::Crowded,
                Err(OfferError::Closed) => Next::Stop,
                Err(OfferError::Failed) => Next::Abnormal,
            },
        };
        match next {
            Next::Stop => return,
            Next::Used => abnormal = 0,
            Next::Abnormal => {
                abnormal += 1;
                if abnormal >= policy.max_abnormal {
                    return;
                }
            }
            Next::Reopen | Next::Crowded => {}
        }
        let pause = if next == Next::Crowded {
            policy.crowded_pause
        } else {
            policy.reopen_after
        };
        tokio::time::sleep(pause).await;
    }
}

/// Runs until the provider stops: opens and keeps a pipe per attached device
/// for the session service, and for the pairing service when `pairing` is
/// given. Dropping the future (or aborting its task) closes every idle pipe's
/// keeper; pipes already handed to a listener are the listener's.
pub async fn keep_pipes(
    provider: Arc<dyn UsbPipeProvider>,
    session: Arc<dyn PipeSink>,
    pairing: Option<Arc<dyn PipeSink>>,
    policy: KeeperPolicy,
) {
    struct Abort(Vec<JoinHandle<()>>);
    impl Drop for Abort {
        fn drop(&mut self) {
            for task in &self.0 {
                task.abort();
            }
        }
    }
    let mut devices: HashMap<UsbDevice, Abort> = HashMap::new();
    while let Some(event) = provider.next_event().await {
        match event {
            UsbEvent::Attached(device) => {
                let mut tasks = vec![tokio::spawn(keep_one(
                    provider.clone(),
                    session.clone(),
                    device.clone(),
                    UsbService::Session,
                    policy,
                ))];
                if let Some(pairing) = &pairing {
                    tasks.push(tokio::spawn(keep_one(
                        provider.clone(),
                        pairing.clone(),
                        device.clone(),
                        UsbService::Pairing,
                        policy,
                    )));
                }
                // A repeated attach replaces (and aborts) the earlier keepers.
                devices.insert(device, Abort(tasks));
            }
            UsbEvent::Detached(device) => {
                devices.remove(&device);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// A provider whose events and `open` results a test scripts.
    struct Scripted {
        events: tokio::sync::Mutex<mpsc::Receiver<UsbEvent>>,
        opens: Mutex<Vec<(UsbDevice, UsbService, tokio::time::Instant)>>,
        open_error: Mutex<Option<fn() -> UsbError>>,
    }

    #[async_trait::async_trait]
    impl UsbPipeProvider for Scripted {
        async fn next_event(&self) -> Option<UsbEvent> {
            self.events.lock().await.recv().await
        }
        async fn open(&self, device: &UsbDevice, service: UsbService) -> Result<PipeIo, UsbError> {
            self.opens
                .lock()
                .unwrap()
                .push((device.clone(), service, tokio::time::Instant::now()));
            if let Some(error) = *self.open_error.lock().unwrap() {
                return Err(error());
            }
            let (ours, _theirs) = tokio::io::duplex(64);
            Ok(Box::new(ours))
        }
    }

    /// A sink that ends every offered pipe with the next scripted outcome
    /// (the last one repeats).
    struct Ends(Mutex<Vec<PipeOutcome>>);

    impl PipeSink for Ends {
        fn offer(&self, _pipe: PipeIo, _device: &UsbDevice) -> Result<PendingOutcome, OfferError> {
            let mut outcomes = self.0.lock().unwrap();
            let outcome = if outcomes.len() > 1 {
                outcomes.remove(0)
            } else {
                outcomes[0]
            };
            Ok(Box::pin(async move { outcome }))
        }
    }

    fn device(label: &str) -> UsbDevice {
        UsbDevice {
            medium: streamcore_transport::transport::Medium::UsbAdb,
            label: label.into(),
        }
    }

    struct Rig {
        provider: Arc<Scripted>,
        events: mpsc::Sender<UsbEvent>,
        keeper: JoinHandle<()>,
    }

    fn rig(outcomes: Vec<PipeOutcome>, pairing: bool) -> Rig {
        let (events, receiver) = mpsc::channel(8);
        let provider = Arc::new(Scripted {
            events: tokio::sync::Mutex::new(receiver),
            opens: Mutex::new(Vec::new()),
            open_error: Mutex::new(None),
        });
        let sink: Arc<dyn PipeSink> = Arc::new(Ends(Mutex::new(outcomes)));
        let keeper = tokio::spawn(keep_pipes(
            provider.clone(),
            sink.clone(),
            pairing.then(|| sink.clone()),
            KeeperPolicy::default(),
        ));
        Rig {
            provider,
            events,
            keeper,
        }
    }

    impl Rig {
        fn opens(&self) -> usize {
            self.provider.opens.lock().unwrap().len()
        }
        async fn attach(&self, label: &str) {
            self.events
                .send(UsbEvent::Attached(device(label)))
                .await
                .unwrap();
        }
        async fn detach(&self, label: &str) {
            self.events
                .send(UsbEvent::Detached(device(label)))
                .await
                .unwrap();
        }
    }

    /// Lets spawned tasks run, then moves the paused clock.
    async fn pass(duration: Duration) {
        tokio::time::sleep(duration).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pipe_is_opened_on_attach_and_reopened_two_seconds_after_each_normal_end() {
        let rig = rig(vec![PipeOutcome::IdleTimeout], true);
        pass(Duration::from_millis(10)).await;
        assert_eq!(rig.opens(), 0, "nothing attached yet");
        rig.attach("a").await;
        pass(Duration::from_millis(10)).await;
        // One pipe per service.
        assert_eq!(rig.opens(), 2);
        pass(Duration::from_millis(1_900)).await;
        assert_eq!(rig.opens(), 2, "not before the pause is over");
        pass(Duration::from_millis(200)).await;
        assert_eq!(rig.opens(), 4);
        // Normal ends never run out.
        pass(Duration::from_secs(20)).await;
        assert!(rig.opens() >= 20);
        // Unplugged: no more pipes.
        rig.detach("a").await;
        pass(Duration::from_millis(10)).await;
        let opened = rig.opens();
        pass(Duration::from_secs(10)).await;
        assert_eq!(rig.opens(), opened);
    }

    #[tokio::test(start_paused = true)]
    async fn three_abnormal_ends_stop_until_the_device_is_replugged() {
        let rig = rig(vec![PipeOutcome::HandshakeFailed], false);
        rig.attach("a").await;
        pass(Duration::from_secs(30)).await;
        assert_eq!(rig.opens(), 3);
        // Another device is not affected.
        rig.attach("b").await;
        pass(Duration::from_secs(30)).await;
        assert_eq!(rig.opens(), 6);
        // A replug starts the count again.
        rig.detach("a").await;
        rig.attach("a").await;
        pass(Duration::from_secs(30)).await;
        assert_eq!(rig.opens(), 9);
    }

    #[tokio::test(start_paused = true)]
    async fn a_used_pipe_clears_the_count_of_abnormal_ends() {
        use PipeOutcome::{Accepted, HandshakeFailed};
        // Two failures, a session, two failures, a session: never three in a
        // row, so the device keeps getting pipes. Then failures only.
        let rig = rig(
            vec![
                HandshakeFailed,
                HandshakeFailed,
                Accepted,
                HandshakeFailed,
                HandshakeFailed,
                Accepted,
                HandshakeFailed,
            ],
            false,
        );
        rig.attach("a").await;
        pass(Duration::from_secs(60)).await;
        // The six scripted ends, then three failures in a row.
        assert_eq!(rig.opens(), 9);
    }

    #[tokio::test(start_paused = true)]
    async fn a_listener_with_no_room_is_asked_again_only_after_a_longer_pause() {
        let rig = rig(vec![PipeOutcome::Refused], false);
        rig.attach("a").await;
        pass(Duration::from_millis(10)).await;
        assert_eq!(rig.opens(), 1);
        pass(Duration::from_secs(9)).await;
        assert_eq!(rig.opens(), 1, "not every two seconds");
        pass(Duration::from_secs(2)).await;
        assert_eq!(rig.opens(), 2);
        // It is not a fault: it never runs out.
        pass(Duration::from_secs(100)).await;
        assert!(rig.opens() >= 10);
    }

    #[tokio::test(start_paused = true)]
    async fn an_untrusted_device_gets_one_pipe_per_plug() {
        let rig = rig(vec![PipeOutcome::Untrusted], false);
        rig.attach("a").await;
        pass(Duration::from_secs(30)).await;
        assert_eq!(rig.opens(), 1);
        rig.detach("a").await;
        rig.attach("a").await;
        pass(Duration::from_secs(30)).await;
        assert_eq!(rig.opens(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_with_nothing_listening_is_tried_again_without_end() {
        let rig = rig(vec![PipeOutcome::Accepted], false);
        *rig.provider.open_error.lock().unwrap() = Some(|| UsbError::Refused("closed".into()));
        rig.attach("a").await;
        pass(Duration::from_secs(60)).await;
        assert!(rig.opens() >= 25, "{} tries", rig.opens());
        // Its app starts: the next try gets a pipe, which is used.
        *rig.provider.open_error.lock().unwrap() = None;
        pass(Duration::from_secs(5)).await;
        // A provider fault is counted instead.
        *rig.provider.open_error.lock().unwrap() = Some(|| UsbError::Protocol);
        let before = rig.opens();
        pass(Duration::from_secs(60)).await;
        assert_eq!(rig.opens(), before + 3);
    }

    #[tokio::test(start_paused = true)]
    async fn the_keeper_ends_with_the_provider_and_takes_its_pipes_with_it() {
        let rig = rig(vec![PipeOutcome::IdleTimeout], false);
        rig.attach("a").await;
        pass(Duration::from_secs(5)).await;
        let Rig {
            provider,
            events,
            keeper,
        } = rig;
        drop(events);
        tokio::time::timeout(Duration::from_secs(1), keeper)
            .await
            .expect("the keeper did not end")
            .unwrap();
        let opened = provider.opens.lock().unwrap().len();
        pass(Duration::from_secs(10)).await;
        assert_eq!(provider.opens.lock().unwrap().len(), opened);
    }
}
