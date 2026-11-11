//! The bounded wait for Rust adapters whose platform call has no timeout (3.5).

use std::sync::mpsc;
use std::time::Duration;

/// Run `call` on a helper thread and wait for it at most `bound`. Returns
/// whether it finished in time. A call that overruns keeps running on its
/// thread; outputs it produces later carry the old encoder generation and are
/// discarded by the executor. Use it for `Encoder::invalidate`, the one allowed
/// wait on an executor.
pub fn run_bounded(bound: Duration, call: impl FnOnce() + Send + 'static) -> bool {
    let (done, finished) = mpsc::sync_channel::<()>(1);
    let spawned = std::thread::Builder::new()
        .name("streamcore-video-bounded".into())
        .spawn(move || {
            call();
            let _ = done.send(());
        });
    if spawned.is_err() {
        return false;
    }
    finished.recv_timeout(bound).is_ok()
}
