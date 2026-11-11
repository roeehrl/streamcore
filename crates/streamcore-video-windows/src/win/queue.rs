//! The stream's `SerialQueue` on Windows (design doc 3.5 "Per platform", 4.2): one thread per
//! stream, in a single-threaded apartment with a `DispatcherQueue`, registered with MMCSS
//! "Capture". It waits on its wake event, a high-resolution waitable timer and its message queue:
//! a wake runs `StreamPort::run`, a due timer `StreamPort::run_timer`, and messages are pumped,
//! which is where a WGC frame pool created on this thread with `Create` fires `FrameArrived`. So
//! frames arrive on the executor's own thread and drain inline.
//!
//! `wake` and `wake_after` only set an event (and push to a short-locked heap): they never block
//! and take no app lock.

use super::sys::com::{
    Apartment, Event, Mmcss, Timer, Wake, dispatcher_queue_for_current_thread, now_ns,
    pump_messages, wait,
};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use streamcore_video::{SerialQueue, StreamPort, TimerToken};

struct Shared {
    wake: Event,
    rearm: Event,
    timers: Mutex<BinaryHeap<Reverse<(u64, TimerToken)>>>,
    port: OnceLock<Arc<StreamPort>>,
    stop: AtomicBool,
}

/// The `SerialQueue` handle the stream and its capture backend hold.
pub struct ExecutorQueue {
    shared: Arc<Shared>,
}

impl SerialQueue for ExecutorQueue {
    fn wake(&self) {
        self.shared.wake.set();
    }

    fn wake_after(&self, delay_ns: u64, timer: TimerToken) {
        let at = now_ns().saturating_add(delay_ns);
        self.shared
            .timers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Reverse((at, timer)));
        self.shared.rearm.set();
    }
}

/// Owns the executor thread.
pub struct ExecutorThread {
    queue: Arc<ExecutorQueue>,
    join: Option<JoinHandle<()>>,
    /// MMCSS registration failure, if any (the thread runs anyway).
    pub mmcss_error: Option<String>,
}

impl ExecutorThread {
    /// Starts the thread and waits until its apartment and `DispatcherQueue` exist.
    pub fn spawn() -> Result<Self, String> {
        let shared = Arc::new(Shared {
            wake: Event::new().map_err(|e| format!("event: {e}"))?,
            rearm: Event::new().map_err(|e| format!("event: {e}"))?,
            timers: Mutex::new(BinaryHeap::new()),
            port: OnceLock::new(),
            stop: AtomicBool::new(false),
        });
        let (ready, started) = mpsc::channel::<Result<Option<String>, String>>();
        let inner = shared.clone();
        let join = std::thread::Builder::new()
            .name("stream-executor".into())
            .spawn(move || executor_main(&inner, &ready))
            .map_err(|e| format!("spawn executor: {e}"))?;
        let mmcss_error = started
            .recv()
            .map_err(|_| "executor thread exited".to_string())??;
        Ok(Self {
            queue: Arc::new(ExecutorQueue { shared }),
            join: Some(join),
            mmcss_error,
        })
    }

    pub fn queue(&self) -> Arc<ExecutorQueue> {
        self.queue.clone()
    }

    /// Binds the port the thread drives. Call once, before the controller is created.
    pub fn bind(&self, port: Arc<StreamPort>) {
        let _ = self.queue.shared.port.set(port);
    }

    /// Stops and joins the thread. Call after the stream has closed.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.queue.shared.stop.store(true, Ordering::Release);
        self.queue.shared.wake.set();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ExecutorThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn executor_main(shared: &Shared, ready: &mpsc::Sender<Result<Option<String>, String>>) {
    let setup = (|| {
        let apartment = Apartment::sta().map_err(|e| format!("CoInitializeEx STA: {e}"))?;
        let controller =
            dispatcher_queue_for_current_thread().map_err(|e| format!("DispatcherQueue: {e}"))?;
        let timer = Timer::new().map_err(|e| format!("timer: {e}"))?;
        Ok::<_, String>((apartment, controller, timer))
    })();
    let (_apartment, _controller, timer) = match setup {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mmcss = Mmcss::capture();
    let _ = ready.send(Ok(mmcss.as_ref().err().map(|e| format!("MMCSS: {e}"))));
    let handles = [shared.wake.raw(), shared.rearm.raw(), timer.raw()];
    while !shared.stop.load(Ordering::Acquire) {
        // Due timers, then arm the next one.
        let now = now_ns();
        let (due, next) = {
            let mut heap = shared.timers.lock().unwrap_or_else(|p| p.into_inner());
            let mut due = Vec::new();
            while heap.peek().is_some_and(|Reverse((at, _))| *at <= now) {
                if let Some(Reverse((_, token))) = heap.pop() {
                    due.push(token);
                }
            }
            (due, heap.peek().map(|Reverse((at, _))| *at))
        };
        if let Some(port) = shared.port.get() {
            for token in due {
                port.run_timer(token);
            }
        }
        match next {
            Some(at) => timer.set_after_ns(at.saturating_sub(now_ns()).max(1)),
            None => timer.cancel(),
        }
        let woke = wait(&handles, 50);
        // Messages first: a WGC FrameArrived dispatched here drains inline.
        pump_messages();
        if woke == Wake::Handle(0)
            && let Some(port) = shared.port.get()
        {
            port.run();
        }
    }
    drop(mmcss);
}
