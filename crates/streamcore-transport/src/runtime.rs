//! One owned executor for async native calls. Dropping a foreign future aborts
//! its Rust task so canceled socket operations cannot survive their owner.
use std::{
    future::Future,
    sync::{Arc, OnceLock},
};

/// UniFFI Swift futures do not propagate Task cancellation. Native owners pass
/// an explicit signal so cancelled dialing/pairing drops its sockets promptly.
#[derive(uniffi::Object)]
pub struct OperationCancellation {
    signal: tokio::sync::watch::Sender<bool>,
}

#[uniffi::export]
impl OperationCancellation {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            signal: tokio::sync::watch::channel(false).0,
        })
    }

    pub fn cancel(&self) {
        self.signal.send_replace(true);
    }
}

impl OperationCancellation {
    pub(crate) async fn run<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        let mut signal = self.signal.subscribe();
        tokio::select! {
            biased;
            _ = async {
                loop {
                    if *signal.borrow_and_update() { return; }
                    if signal.changed().await.is_err() { return; }
                }
            } => None,
            result = future => Some(result),
        }
    }
}

static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, std::io::Error>> = OnceLock::new();

pub(crate) async fn run<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, ()> {
    let runtime = RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("streamcore-network")
                .build()
        })
        .as_ref()
        .map_err(|_| ())?;
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let task = runtime.spawn(future);
    let _abort = AbortOnDrop(task.abort_handle());
    task.await.map_err(|_| ())
}

pub(crate) fn cleanup(future: impl Future<Output = ()> + Send + 'static) {
    if let Some(Ok(runtime)) = RUNTIME.get() {
        runtime.spawn(future);
    }
}
