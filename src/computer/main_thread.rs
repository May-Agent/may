//! The helper's main thread.
//!
//! macOS builds a virtual display's topology only while the main thread
//! services its run loop, and `waterkit_automation::VirtualDisplay` is
//! created and dropped there. So the helper's main thread does nothing but
//! run that loop ([`host`]) while the async runtime lives on a thread of its
//! own, and anything bound to the main thread is handed to it as a block on
//! the main dispatch queue ([`run`], [`release`]).

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dispatch2::DispatchQueue;
pub use dispatch2::MainThreadBound;
use objc2::MainThreadMarker;
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};

/// Seconds until the idle timer that keeps the run loop alive would fire:
/// longer than any helper lives.
const IDLE_TIMER_SECONDS: f64 = 1.0e10;

/// Ends [`host`]'s serving when dropped, whether the work returned or
/// panicked, so [`host`] never waits on a worker that is gone.
struct EndServing(Arc<AtomicBool>);

impl Drop for EndServing {
    fn drop(&mut self) {
        let done = self.0.clone();
        // Set from a main-queue block: running it is a handled source, which
        // ends the turn `serve_until` is in, so the flag is read at once.
        DispatchQueue::main().exec_async(move || done.store(true, Ordering::Relaxed));
    }
}

/// Serves the calling thread's run loop in its default mode until `done`
/// is set.
///
/// Each turn returns after one handled source — a main-queue block counts
/// as one — and re-reads `done`. A turn is never ended to finish serving:
/// `CFRunLoopStop` sets one flag on the whole run loop that the next turn to
/// check it clears, so a stop meant for a nested turn, such as the one each
/// display reconfiguration callback sends while `VirtualDisplay::new` waits,
/// can land on this one. Here such a stop ends one turn only.
fn serve_until(done: &AtomicBool) {
    // SAFETY: the default mode constant is always valid.
    let mode = unsafe { kCFRunLoopDefaultMode };
    while !done.load(Ordering::Relaxed) {
        CFRunLoop::run_in_mode(mode, IDLE_TIMER_SECONDS, true);
    }
}

/// Runs `work` on a new thread while the main thread services its run loop.
///
/// The worker gets `stack_size` bytes of stack. The calling thread, which
/// must be the main thread, services the main run loop and dispatch queue
/// until `work` has finished, and returns its result.
///
/// # Panics
/// Panics when not called on the main thread, when the worker thread cannot
/// start, and resumes the worker's panic if it panicked.
pub fn host<R: Send + 'static>(stack_size: usize, work: impl FnOnce() -> R + Send + 'static) -> R {
    let _main = MainThreadMarker::new().expect("the run loop is hosted on the main thread");
    let done = Arc::new(AtomicBool::new(false));
    let end = EndServing(done.clone());
    let worker = std::thread::Builder::new()
        .name("computer-agent".to_owned())
        .stack_size(stack_size)
        .spawn(move || {
            let _end = end;
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
    serve_until(&done);
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

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use objc2_core_foundation::CFRunLoopTimerContext;

    use super::*;

    /// A one-shot timer on the current run loop's default mode, due now,
    /// calling `callout` with `done`.
    fn schedule(
        callout: unsafe extern "C-unwind" fn(*mut CFRunLoopTimer, *mut c_void),
        done: *const AtomicBool,
    ) {
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: done.cast_mut().cast(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: the context is copied at creation; `done` outlives the
        // serving that fires the timer.
        let timer = unsafe {
            CFRunLoopTimer::new(
                None,
                CFAbsoluteTimeGetCurrent(),
                0.0,
                0,
                0,
                Some(callout),
                &raw mut context,
            )
        }
        .expect("CoreFoundation creates a timer");
        let current = CFRunLoop::current().expect("the thread has a run loop");
        // SAFETY: the default mode constant is always valid.
        current.add_timer(Some(&timer), unsafe { kCFRunLoopDefaultMode });
    }

    /// Stops the loop as a display reconfiguration callback does, then
    /// schedules the timer that ends serving. A timer added while timers
    /// fire is due only in a later turn, so it fires only if serving
    /// outlived the stop.
    unsafe extern "C-unwind" fn stray_stop(_: *mut CFRunLoopTimer, done: *mut c_void) {
        CFRunLoop::current()
            .expect("the thread has a run loop")
            .stop();
        schedule(finish, done.cast());
    }

    unsafe extern "C-unwind" fn finish(_: *mut CFRunLoopTimer, done: *mut c_void) {
        // SAFETY: `done` is the flag `schedule` was given, still alive.
        unsafe { &*done.cast::<AtomicBool>() }.store(true, Ordering::Relaxed);
        // A timer is not a handled source; the stop ends the turn.
        CFRunLoop::current()
            .expect("the thread has a run loop")
            .stop();
    }

    #[test]
    fn a_stray_stop_ends_one_turn_not_the_serving() {
        let done = AtomicBool::new(false);
        schedule(stray_stop, &raw const done);
        serve_until(&done);
        assert!(done.load(Ordering::Relaxed));
    }
}
