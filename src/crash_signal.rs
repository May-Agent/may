//! Dump the log ring to `~/.may/bugs` when a fatal signal takes the process
//! down — the one path the panic hook never sees.
//!
//! A stack overflow does not panic: std's SIGSEGV handler prints "has
//! overflowed its stack" and the process aborts. The handler installed here
//! runs first, writes the recent log and a native backtrace to
//! `crash-<secs>-<pid>.log`, then defers to whatever the signal would have
//! done anyway, so the usual message and crash report still happen.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::log_ring::LogRingHandle;

/// The signals whose dump precedes their usual outcome.
const SIGNALS: [i32; 3] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT];

/// Backtrace depth the dump keeps.
const MAX_FRAMES: usize = 128;

/// Maximum path bytes the fixed buffers hold.
const PATH_CAP: usize = 1024;

/// What a signal handler needs, reachable without parameters. A handler
/// receives only the signal and its `siginfo`, so the state it uses has to
/// live in a static — the one place a static is unavoidable.
struct CrashState {
    ring: LogRingHandle,
    bugs_dir: CString,
    previous: [libc::sigaction; SIGNALS.len()],
}

static CRASH_STATE: OnceLock<CrashState> = OnceLock::new();

/// Set once the first dump is written so a nested or chained signal (the
/// overflow that ends in abort) never writes a second file.
static DUMPED: AtomicBool = AtomicBool::new(false);

/// Install the fatal-signal log dump for `SIGSEGV`, `SIGBUS` and `SIGABRT`.
///
/// `bugs_dir` is created if missing. Each signal gets one handler with
/// `SA_SIGINFO | SA_ONSTACK` that writes `crash-<secs>-<pid>.log` under the
/// directory, then restores the action it displaced and lets the signal take
/// its usual course.
///
/// # Panics
///
/// Installing twice is a programming error.
///
/// # Errors
///
/// When the directory cannot be created or an action cannot be installed.
pub fn install(log_ring: LogRingHandle, bugs_dir: PathBuf) -> io::Result<()> {
    std::fs::create_dir_all(&bugs_dir)?;
    let bugs_dir_c = CString::new(bugs_dir.into_os_string().into_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bugs dir contains a NUL byte"))?;

    // The alternate stack so the overflow handler has room to run —
    // SA_ONSTACK is meaningless without one. Leaked for the process's
    // lifetime; the kernel copies the registration and never lets it go.
    let mut alt: libc::stack_t = unsafe { std::mem::zeroed() };
    alt.ss_sp = Box::into_raw(vec![0u8; 32 * 1024].into_boxed_slice()).cast();
    alt.ss_size = 32 * 1024;
    alt.ss_flags = 0;
    if unsafe { libc::sigaltstack(&raw const alt, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // The handler's state — OnceLock because a signal handler takes no
    // context pointer.
    let mut previous: [libc::sigaction; SIGNALS.len()] = unsafe { std::mem::zeroed() };
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = crash_handler as *const () as usize;
    action.sa_mask = {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&raw mut set) };
        set
    };
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
    for (index, &signal) in SIGNALS.iter().enumerate() {
        if unsafe { libc::sigaction(signal, &raw const action, &raw mut previous[index]) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    if CRASH_STATE
        .set(CrashState {
            ring: log_ring,
            bugs_dir: bugs_dir_c,
            previous,
        })
        .is_err()
    {
        panic!("the crash-signal dump installs once");
    }
    Ok(())
}

/// Format `value` decimal into `buf` at `used`, returning the new length.
fn put_u64(buf: &mut [u8; PATH_CAP], mut used: usize, mut value: u64) -> usize {
    let mut digits = [0u8; 20];
    let mut count = 0;
    loop {
        digits[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        count += 1;
        if value == 0 {
            break;
        }
    }
    for i in (0..count).rev() {
        if used < PATH_CAP {
            buf[used] = digits[i];
            used += 1;
        }
    }
    used
}

/// Append raw bytes into `buf` at `used`, returning the new length.
fn put_bytes(buf: &mut [u8; PATH_CAP], mut used: usize, bytes: &[u8]) -> usize {
    for &b in bytes {
        if used < PATH_CAP {
            buf[used] = b;
            used += 1;
        }
    }
    used
}

/// Append `value` hex into `buf` at `used`, returning the new length.
fn put_hex(buf: &mut [u8; PATH_CAP], mut used: usize, mut value: usize) -> usize {
    let mut digits = [0u8; 16];
    let mut count = 0;
    loop {
        let d = u8::try_from(value % 16).unwrap_or(0);
        digits[count] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        value /= 16;
        count += 1;
        if value == 0 {
            break;
        }
    }
    for i in (0..count).rev() {
        if used < PATH_CAP {
            buf[used] = digits[i];
            used += 1;
        }
    }
    used
}

/// Write `bytes` to `fd`, ignoring the result — the dump is best-effort and
/// short writes or errors change nothing the handler does next.
unsafe fn write_all(fd: i32, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n <= 0 {
            return;
        }
        bytes = &bytes[n.cast_unsigned()..];
    }
}

/// The shared handler: one best-effort allocation-free dump, then the
/// signal's displaced action.
unsafe extern "C" fn crash_handler(
    signal: i32,
    info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    let Some(state) = CRASH_STATE.get() else {
        return;
    };
    if !DUMPED.swap(true, Ordering::AcqRel) {
        unsafe { dump(state, signal, info) };
    }
    // Defer to what the signal would have done. Restoring then returning
    // replays the fault for SIGSEGV/SIGBUS, so std's overflow handler still
    // prints and the platform crash report still lands; SIGABRT is re-raised.
    let index = SIGNALS.iter().position(|&s| s == signal).unwrap_or(0);
    unsafe {
        libc::sigaction(
            signal,
            &raw const state.previous[index],
            std::ptr::null_mut(),
        )
    };
    if signal == libc::SIGABRT {
        unsafe { libc::raise(libc::SIGABRT) };
    }
}

/// Write `crash-<secs>-<pid>.log` under the bugs dir: the signal header, the
/// log ring if its lock is free, and a native backtrace.
unsafe fn dump(state: &CrashState, signal: i32, info: *mut libc::siginfo_t) {
    let mut name = [0u8; PATH_CAP];
    let mut used = put_bytes(&mut name, 0, b"crash-");
    used = put_u64(
        &mut name,
        used,
        unsafe { libc::time(std::ptr::null_mut()) }.cast_unsigned(),
    );
    used = put_bytes(&mut name, used, b"-");
    used = put_u64(
        &mut name,
        used,
        u64::from(unsafe { libc::getpid() }.cast_unsigned()),
    );
    used = put_bytes(&mut name, used, b".log");

    let mut path = [0u8; PATH_CAP];
    let dir = state.bugs_dir.as_bytes();
    let mut pused = put_bytes(&mut path, 0, dir);
    pused = put_bytes(&mut path, pused, b"/");
    pused = put_bytes(&mut path, pused, &name[..used]);
    if pused >= PATH_CAP {
        return;
    }
    path[pused] = 0;

    let fd = unsafe {
        libc::open(
            path.as_ptr().cast(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_EXCL,
            0o600,
        )
    };
    if fd < 0 {
        return;
    }

    // Header: signal name/number and the faulting address.
    let signal_name: &[u8] = match signal {
        libc::SIGSEGV => b"SIGSEGV",
        libc::SIGBUS => b"SIGBUS",
        libc::SIGABRT => b"SIGABRT",
        _ => b"SIGNAL",
    };
    let mut line = [0u8; PATH_CAP];
    let mut lused = put_bytes(&mut line, 0, b"may crash: signal ");
    lused = put_bytes(&mut line, lused, signal_name);
    lused = put_bytes(&mut line, lused, b" (");
    lused = put_u64(&mut line, lused, u64::from(signal.cast_unsigned()));
    lused = put_bytes(&mut line, lused, b")");
    if !info.is_null() {
        let addr = unsafe { (*info).si_addr() } as usize;
        lused = put_bytes(&mut line, lused, b" at 0x");
        lused = put_hex(&mut line, lused, addr);
    }
    lused = put_bytes(&mut line, lused, b"\n");
    unsafe { write_all(fd, &line[..lused]) };

    // The log ring — `try_lock` because locking inside a signal handler
    // deadlocks when the holder is the thread that was interrupted.
    if !state.ring.try_write_chunks(|chunk| unsafe {
        write_all(fd, chunk);
    }) {
        unsafe { write_all(fd, b"[log ring busy at crash]\n") };
    }
    unsafe { write_all(fd, b"\n-- backtrace --\n") };

    // A native backtrace straight into the file.
    let mut frames: [*mut libc::c_void; MAX_FRAMES] = [std::ptr::null_mut(); MAX_FRAMES];
    let count = unsafe {
        libc::backtrace(
            frames.as_mut_ptr(),
            i32::try_from(MAX_FRAMES).unwrap_or(i32::MAX),
        )
    };
    if count > 0 {
        unsafe {
            libc::backtrace_symbols_fd(frames.as_ptr(), count, fd);
        }
    }
    unsafe { libc::close(fd) };
}
