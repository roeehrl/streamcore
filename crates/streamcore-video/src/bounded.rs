//! Bounded waits for calls that may block (3.5): `Encoder::invalidate` in
//! adapters whose platform call has no timeout, and the executor's own wait for
//! `EncoderFactory::create`.

use std::sync::mpsc;
use std::time::Duration;

/// Run `call` on a helper thread and wait for it at most `bound`. Returns
/// whether it finished in time. A call that overruns keeps running on its
/// thread; outputs it produces later carry the old encoder generation and are
/// discarded by the executor. Use it for `Encoder::invalidate`, the one allowed
/// wait on an executor. On Windows, the closure initialises COM (MTA) itself.
pub fn run_bounded(bound: Duration, call: impl FnOnce() + Send + 'static) -> bool {
    run_bounded_value(bound, call, |()| {}).is_some()
}

/// Run `call` on a helper thread and wait for its value at most `bound`. When it
/// overruns, `None` is returned and `abandon` later receives the late value on
/// the helper thread (to invalidate a session nobody will use).
pub fn run_bounded_value<T: Send + 'static>(
    bound: Duration,
    call: impl FnOnce() -> T + Send + 'static,
    abandon: impl FnOnce(T) + Send + 'static,
) -> Option<T> {
    let (done, finished) = mpsc::sync_channel::<T>(1);
    let spawned = std::thread::Builder::new()
        .name("streamcore-video-bounded".into())
        .spawn(move || {
            let value = call();
            if let Err(mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value)) =
                done.try_send(value)
            {
                abandon(value);
            }
        });
    if spawned.is_err() {
        return None;
    }
    // Dropping the receiver on timeout makes a late send fail, so the helper
    // thread hands the value to `abandon`.
    finished.recv_timeout(bound).ok()
}
