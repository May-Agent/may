//! The helper's main thread.
//!
//! macOS builds a virtual display's topology only while the main thread
//! services its run loop, and `waterkit_automation::VirtualDisplay` is
//! created and dropped there. So the helper's main thread does nothing but
//! run that loop ([`host`]) while the async runtime lives on a thread of its
//! own, and anything bound to the main thread is handed to it as a block on
//! the main dispatch queue ([`run`], [`release`]).

use std::ptr;

use dispatch2::DispatchQueue;
pub use dispatch2::MainThreadBound;
use objc2::MainThreadMarker;
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer, kCFRunLoopCommonModes,
};

/// Seconds until the idle timer that keeps the run loop alive would fire:
/// longer than any helper lives.
const IDLE_TIMER_SECONDS: f64 = 1.0e10;

/// Stops the main run loop when dropped, whether the work returned or
/// panicked, so [`host`] never waits on a worker that is gone.
struct StopLoop;

impl Drop for StopLoop {
    fn drop(&mut self) {
        DispatchQueue::main().exec_async(|| {
            CFRunLoop::main()
                .expect("the process has a main run loop")
                .stop();
        });
    }
}

/// Runs `work` on a new thread while the main thread services its run loop.
///
/// The worker gets `stack_size` bytes of stack. The calling thread, which
/// must be the main thread, services the main run loop and dispatch queue,
/// and returns `work`'s result once it has finished.
///
/// # Panics
/// Panics when not called on the main thread, when the worker thread cannot
/// start, and resumes the worker's panic if it panicked.
pub fn host<R: Send + 'static>(stack_size: usize, work: impl FnOnce() -> R + Send + 'static) -> R {
    let _main = MainThreadMarker::new().expect("the run loop is hosted on the main thread");
    let worker = std::thread::Builder::new()
        .name("computer-agent".to_owned())
        .stack_size(stack_size)
        .spawn(move || {
            let _stop = StopLoop;
            work()
        })
        .expect("the worker thread starts");
    // A run loop with no source returns at once; this timer, which fires
    // after the helper is long gone, is the source that keeps it serving.
    // SAFETY: no callback and a null context.
    let idle = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + IDLE_TIMER_SECONDS,
            0.0,
            0,
            0,
            None,
            ptr::null_mut(),
        )
    }
    .expect("CoreFoundation creates a timer");
    let main = CFRunLoop::main().expect("the process has a main run loop");
    // SAFETY: the common modes constant is always valid.
    main.add_timer(Some(&idle), unsafe { kCFRunLoopCommonModes });
    CFRunLoop::run();
    idle.invalidate();
    worker
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Runs `job` on the main thread and returns what it produced.
///
/// The caller awaits; nothing blocks. A job that returns a value the main
/// thread owns wraps it in a [`MainThreadBound`].
///
/// # Panics
/// Panics when the main thread no longer serves its queue.
pub async fn run<R: Send + 'static>(job: impl FnOnce(MainThreadMarker) -> R + Send + 'static) -> R {
    let (sender, receiver) = async_channel::bounded(1);
    DispatchQueue::main().exec_async(move || {
        let main =
            MainThreadMarker::new().expect("blocks on the main queue run on the main thread");
        // The receiver is gone when the caller stopped waiting; the result
        // then drops here, on the main thread.
        let _ = sender.try_send(job(main));
    });
    receiver
        .recv()
        .await
        .expect("the main thread serves its queue")
}

/// Drops `value` on the main thread without making the caller wait for it.
pub fn release<T: 'static>(value: MainThreadBound<T>) {
    DispatchQueue::main().exec_async(move || drop(value));
}
