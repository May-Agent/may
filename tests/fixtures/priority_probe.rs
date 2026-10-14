//! Kernel-scheduling probe: prints `parentNice parentIO childNice childIO`.
//! The parent spawns an exec'd copy of itself; both read their own CPU nice
//! level and I/O policy straight from the kernel, so a run shows the ambient
//! priority and whether a descendant inherits it.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("priority probe supports macOS and Linux only");

#[cfg(target_os = "linux")]
use std::ffi::c_long;
use std::ffi::{OsStr, c_int};
use std::io;
use std::process::{Command, exit};

const PRIO_PROCESS: c_int = 0;
#[cfg(target_os = "macos")]
const IOPOL_TYPE_DISK: c_int = 0;
#[cfg(target_os = "macos")]
const IOPOL_SCOPE_PROCESS: c_int = 0;
#[cfg(target_os = "linux")]
const IOPRIO_WHO_PROCESS: c_long = 1;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn getpriority(which: c_int, who: c_int) -> c_int;
    fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
    fn __error() -> *mut c_int;
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getpriority(which: c_int, who: c_int) -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
    fn __errno_location() -> *mut c_int;
}

#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::c_long;

    // ioprio_get has no libc wrapper; its syscall number is per-architecture.
    #[cfg(target_arch = "x86_64")]
    pub const IOPRIO_GET: c_long = 252;
    #[cfg(target_arch = "aarch64")]
    pub const IOPRIO_GET: c_long = 31;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("priority probe needs the ioprio_get syscall number for this Linux target");
}

#[cfg(target_os = "macos")]
fn errno() -> &'static mut c_int {
    unsafe { &mut *__error() }
}

#[cfg(target_os = "linux")]
fn errno() -> &'static mut c_int {
    unsafe { &mut *__errno_location() }
}

fn nice() -> io::Result<i32> {
    // getpriority can legitimately return -1, so errno carries the failure.
    unsafe {
        *errno() = 0;
        let value = getpriority(PRIO_PROCESS, 0);
        if value == -1 && *errno() != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    }
}

#[cfg(target_os = "macos")]
fn io_priority() -> io::Result<i32> {
    let value = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS) };
    if value == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
fn io_priority() -> io::Result<i32> {
    let value = unsafe { syscall(sys::IOPRIO_GET, IOPRIO_WHO_PROCESS, 0) };
    if value == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(value as i32)
}

fn observe() -> io::Result<(i32, i32)> {
    Ok((nice()?, io_priority()?))
}

fn run() -> io::Result<()> {
    let (nice, io) = observe()?;
    let is_child = std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == OsStr::new("--child"));
    if is_child {
        println!("{nice} {io}");
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let output = Command::new(exe).arg("--child").output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "child probe exited with {}",
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let values: Vec<i32> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    let [child_nice, child_io] = values
        .try_into()
        .map_err(|_| io::Error::other("child probe did not print two integers"))?;
    println!("{nice} {io} {child_nice} {child_io}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("priority_probe: {error}");
        exit(1);
    }
}
